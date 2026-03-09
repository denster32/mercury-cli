//! Engine — the four-layer intelligence pipeline.
//!
//! - **Planner**: goal + repo map → Mercury 2 → execution plan + thermal scores
//! - **Patcher**: file slice + instruction → Mercury Edit → patched code
//! - **Verifier**: tree-sitter parse → test → lint → optional Mercury 2 critique
//! - **Scheduler**: tokio concurrency pool, budget tracking, thermal merge cycles

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::Semaphore;

use crate::api::{
    ApiError, ApiUsage, Mercury2Api, MercuryEditApi, ThermalAssessment, THERMAL_ANALYSIS_PROMPT,
};
use crate::db::{DbError, ThermalDb};
use crate::repo::RepoError;
use crate::thermal::{self, ThermalError};

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

/// Errors originating from the engine layer.
#[derive(Error, Debug)]
pub enum EngineError {
    #[error("API error: {0}")]
    Api(#[from] ApiError),

    #[error("database error: {0}")]
    Db(#[from] DbError),

    #[error("thermal error: {0}")]
    Thermal(#[from] ThermalError),

    #[error("repo error: {0}")]
    Repo(#[from] RepoError),

    #[error("verification failed: {reason}")]
    VerificationFailed { reason: String },

    #[error("budget exceeded: ${spent:.4} of ${limit:.4}")]
    BudgetExceeded { spent: f64, limit: f64 },

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("parse error after patch: {0}")]
    ParseError(String),

    #[error("test failure: {0}")]
    TestFailure(String),

    #[error("lint failure: {0}")]
    LintFailure(String),
}

// ---------------------------------------------------------------------------
// Planner (Layer 1)
// ---------------------------------------------------------------------------

/// An execution plan produced by the Planner from Mercury 2 analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionPlan {
    /// Ordered list of steps to execute.
    pub steps: Vec<PlanStep>,
    /// Constitutional prompt to inject into every agent.
    pub constitutional_prompt: String,
    /// Total estimated cost.
    pub estimated_cost: f64,
}

/// A single step in an execution plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanStep {
    pub file_path: String,
    pub instruction: String,
    pub priority: f64,
    pub estimated_tokens: u32,
}

/// Planner: takes a goal and repo map, calls Mercury 2, returns an execution plan.
pub struct Planner<A: Mercury2Api> {
    api: A,
    constitutional_prompt: String,
}

impl<A: Mercury2Api> Planner<A> {
    /// Create a new Planner with the given API client.
    pub fn new(api: A, constitutional_prompt: String) -> Self {
        Self {
            api,
            constitutional_prompt,
        }
    }

    /// Generate an execution plan from a goal and repo map.
    pub async fn plan(
        &self,
        goal: &str,
        repo_map: &str,
    ) -> Result<(ExecutionPlan, Vec<ThermalAssessment>), EngineError> {
        let system_prompt = format!(
            "{}\n\n{}\n\nAdditionally, generate an execution plan as JSON with this schema:\n\
             {{\"steps\": [{{\"file_path\": \"...\", \"instruction\": \"...\", \"priority\": 0.0-1.0, \"estimated_tokens\": N}}],\n\
             \"assessments\": [{{\"complexity_score\": 0-1, \"dependency_score\": 0-1, \"risk_score\": 0-1, \"churn_score\": 0-1, \"suggested_action\": \"...\", \"reasoning\": \"...\"}}]}}",
            THERMAL_ANALYSIS_PROMPT, self.constitutional_prompt
        );

        let user_msg = format!("Goal: {}\n\nRepository Map:\n{}", goal, repo_map);

        let (response, _usage) = self.api.chat(&system_prompt, &user_msg, 4096).await?;

        // Mercury 2 may wrap JSON in markdown fences — extract the JSON body.
        let json_str = extract_json(&response);

        let parsed: PlannerResponse =
            serde_json::from_str(json_str).map_err(ApiError::JsonParse)?;

        let plan = ExecutionPlan {
            steps: parsed.steps,
            constitutional_prompt: self.constitutional_prompt.clone(),
            estimated_cost: 0.0,
        };

        Ok((plan, parsed.assessments))
    }
}

#[derive(Deserialize)]
struct PlannerResponse {
    #[serde(default)]
    steps: Vec<PlanStep>,
    #[serde(default)]
    assessments: Vec<ThermalAssessment>,
}

/// Extract JSON from a response that may be wrapped in markdown code fences.
fn extract_json(text: &str) -> &str {
    let trimmed = text.trim();
    if let Some(start) = trimmed.find("```json") {
        let after = &trimmed[start + 7..];
        if let Some(end) = after.find("```") {
            return after[..end].trim();
        }
    }
    if let Some(start) = trimmed.find("```") {
        let after = &trimmed[start + 3..];
        // skip optional language tag on same line
        let after = after.trim_start_matches(|c: char| c != '\n');
        if let Some(end) = after.find("```") {
            return after[..end].trim();
        }
    }
    trimmed
}

// ---------------------------------------------------------------------------
// Patcher (Layer 2)
// ---------------------------------------------------------------------------

/// Patcher: takes a file slice and instruction, calls Mercury Edit, returns patched code.
pub struct Patcher<E: MercuryEditApi> {
    api: E,
}

impl<E: MercuryEditApi> Patcher<E> {
    /// Create a new Patcher with the given edit API client.
    pub fn new(api: E) -> Self {
        Self { api }
    }

    /// Apply an edit to a file's content using Mercury Edit Apply.
    ///
    /// `original_code` is the code before the edit, `update_snippet` is the
    /// modified version to apply.
    pub async fn patch(
        &self,
        original_code: &str,
        update_snippet: &str,
    ) -> Result<(String, ApiUsage), EngineError> {
        use crate::api::EditPayload;
        let payload = EditPayload {
            original_code: original_code.to_string(),
            update_snippet: update_snippet.to_string(),
            max_tokens: 8192,
        };
        let (result, usage) = self.api.apply(&payload).await?;
        Ok((result, usage))
    }

    /// FIM autocomplete: provide code before and after the cursor.
    pub async fn complete(
        &self,
        prompt: &str,
        suffix: &str,
    ) -> Result<(String, ApiUsage), EngineError> {
        use crate::api::CompletePayload;
        let payload = CompletePayload {
            prompt: prompt.to_string(),
            suffix: suffix.to_string(),
            max_tokens: 256,
        };
        let (result, usage) = self.api.complete(&payload).await?;
        Ok((result, usage))
    }

    /// Predict the next edit based on file content and history.
    pub async fn next_edit(
        &self,
        file_content: &str,
        edit_history: &str,
    ) -> Result<(String, ApiUsage), EngineError> {
        use crate::api::NextEditPayload;
        let payload = NextEditPayload {
            file_content: file_content.to_string(),
            code_to_edit: String::new(),
            cursor: String::new(),
            recent_snippets: String::new(),
            edit_history: edit_history.to_string(),
        };
        let (result, usage) = self.api.next_edit(&payload).await?;
        Ok((result, usage))
    }
}

// ---------------------------------------------------------------------------
// Verifier (Layer 3)
// ---------------------------------------------------------------------------

/// Verification configuration.
#[derive(Debug, Clone)]
pub struct VerifyConfig {
    pub parse_before_write: bool,
    pub test_after_write: bool,
    pub lint_after_write: bool,
    pub mercury2_critique_on_failure: bool,
    pub test_command: String,
    pub lint_command: String,
}

impl Default for VerifyConfig {
    fn default() -> Self {
        Self {
            parse_before_write: true,
            test_after_write: true,
            lint_after_write: true,
            mercury2_critique_on_failure: true,
            test_command: "cargo test".to_string(),
            lint_command: "cargo clippy".to_string(),
        }
    }
}

/// Verifier: runs local parse/test/lint checks, optionally calls Mercury 2 for critique.
pub struct Verifier<A: Mercury2Api> {
    config: VerifyConfig,
    critique_api: Option<A>,
}

impl<A: Mercury2Api> Verifier<A> {
    /// Create a new Verifier.
    pub fn new(config: VerifyConfig, critique_api: Option<A>) -> Self {
        Self {
            config,
            critique_api,
        }
    }

    /// Run the full verification pipeline on a patched file.
    pub async fn verify(
        &self,
        file_path: &Path,
        patched_content: &str,
        project_root: &Path,
    ) -> Result<VerifyResult, EngineError> {
        let mut result = VerifyResult {
            parse_ok: true,
            test_ok: true,
            lint_ok: true,
            critique: None,
            errors: Vec::new(),
        };

        // Step 1: tree-sitter parse check
        if self.config.parse_before_write {
            match self.check_parse(patched_content) {
                Ok(true) => {}
                Ok(false) => {
                    result.parse_ok = false;
                    result
                        .errors
                        .push("tree-sitter parse produced errors".to_string());
                }
                Err(e) => {
                    result.parse_ok = false;
                    result.errors.push(format!("parse check failed: {e}"));
                }
            }
        }

        // Write the file before running tests/lint
        std::fs::write(file_path, patched_content)?;

        // Step 2: run tests
        if self.config.test_after_write {
            match self.run_command(&self.config.test_command, project_root) {
                Ok(output) if output.success => {}
                Ok(output) => {
                    result.test_ok = false;
                    result
                        .errors
                        .push(format!("tests failed: {}", output.stderr));
                }
                Err(e) => {
                    result.test_ok = false;
                    result.errors.push(format!("test command failed: {e}"));
                }
            }
        }

        // Step 3: run linter
        if self.config.lint_after_write {
            match self.run_command(&self.config.lint_command, project_root) {
                Ok(output) if output.success => {}
                Ok(output) => {
                    result.lint_ok = false;
                    result
                        .errors
                        .push(format!("lint failed: {}", output.stderr));
                }
                Err(e) => {
                    result.lint_ok = false;
                    result.errors.push(format!("lint command failed: {e}"));
                }
            }
        }

        // Step 4: Mercury 2 critique on failure
        if !result.is_ok() && self.config.mercury2_critique_on_failure {
            if let Some(ref api) = self.critique_api {
                let critique = self
                    .get_critique(api, patched_content, &result.errors)
                    .await;
                result.critique = critique.ok();
            }
        }

        Ok(result)
    }

    fn check_parse(&self, source: &str) -> Result<bool, EngineError> {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .map_err(|e| EngineError::ParseError(e.to_string()))?;
        match parser.parse(source, None) {
            Some(tree) => Ok(!tree.root_node().has_error()),
            None => Ok(false),
        }
    }

    fn run_command(&self, cmd: &str, working_dir: &Path) -> Result<CommandOutput, EngineError> {
        let parts: Vec<&str> = cmd.split_whitespace().collect();
        if parts.is_empty() {
            return Err(EngineError::VerificationFailed {
                reason: "empty command".to_string(),
            });
        }
        let output = Command::new(parts[0])
            .args(&parts[1..])
            .current_dir(working_dir)
            .output()?;

        Ok(CommandOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        })
    }

    async fn get_critique(
        &self,
        api: &A,
        code: &str,
        errors: &[String],
    ) -> Result<String, EngineError> {
        let system = "You are a code review agent. Analyze the following code that failed verification and suggest fixes. Respond with a brief critique.";
        let user_msg = format!(
            "Code:\n```\n{}\n```\n\nErrors:\n{}",
            code,
            errors.join("\n")
        );
        let (response, _) = api.chat(system, &user_msg, 2048).await?;
        Ok(response)
    }
}

/// Result of a verification run.
#[derive(Debug, Clone)]
pub struct VerifyResult {
    pub parse_ok: bool,
    pub test_ok: bool,
    pub lint_ok: bool,
    pub critique: Option<String>,
    pub errors: Vec<String>,
}

impl VerifyResult {
    /// Returns true if all checks passed.
    pub fn is_ok(&self) -> bool {
        self.parse_ok && self.test_ok && self.lint_ok
    }
}

struct CommandOutput {
    success: bool,
    #[allow(dead_code)]
    stdout: String,
    stderr: String,
}

// ---------------------------------------------------------------------------
// Scheduler (Layer 4)
// ---------------------------------------------------------------------------

/// Scheduler configuration.
#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    pub max_concurrency: usize,
    pub max_cost_per_command: f64,
    pub retry_limit: u32,
    pub backoff_base_ms: u64,
    pub decay_half_life_seconds: f64,
    pub hot_threshold: f64,
    pub cool_threshold: f64,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            max_concurrency: 20,
            max_cost_per_command: 0.50,
            retry_limit: 3,
            backoff_base_ms: 500,
            decay_half_life_seconds: 300.0,
            hot_threshold: 0.7,
            cool_threshold: 0.3,
        }
    }
}

/// Scheduler: manages concurrency, budget tracking, thermal merge cycles, and decay.
pub struct Scheduler {
    config: SchedulerConfig,
    semaphore: Arc<Semaphore>,
    total_cost: std::sync::atomic::AtomicU64,
    active_count: std::sync::atomic::AtomicUsize,
}

impl Scheduler {
    /// Create a new Scheduler with the given configuration.
    pub fn new(config: SchedulerConfig) -> Self {
        let semaphore = Arc::new(Semaphore::new(config.max_concurrency));
        Self {
            config,
            semaphore,
            total_cost: std::sync::atomic::AtomicU64::new(0),
            active_count: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Acquire a concurrency slot. Returns a permit that must be held during execution.
    pub async fn acquire(&self) -> Result<tokio::sync::OwnedSemaphorePermit, EngineError> {
        let permit = self.semaphore.clone().acquire_owned().await.map_err(|_| {
            EngineError::VerificationFailed {
                reason: "scheduler semaphore closed".to_string(),
            }
        })?;
        self.active_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(permit)
    }

    /// Release tracking when a permit is dropped (call manually for bookkeeping).
    pub fn release(&self) {
        self.active_count
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Record cost from an API call. Returns error if budget exceeded.
    pub fn record_cost(&self, cost: f64) -> Result<(), EngineError> {
        let cost_bits = (cost * 1_000_000.0) as u64;
        let prev = self
            .total_cost
            .fetch_add(cost_bits, std::sync::atomic::Ordering::Relaxed);
        let new_total = (prev + cost_bits) as f64 / 1_000_000.0;
        if new_total > self.config.max_cost_per_command {
            return Err(EngineError::BudgetExceeded {
                spent: new_total,
                limit: self.config.max_cost_per_command,
            });
        }
        Ok(())
    }

    /// Get the current total cost.
    pub fn current_cost(&self) -> f64 {
        self.total_cost.load(std::sync::atomic::Ordering::Relaxed) as f64 / 1_000_000.0
    }

    /// Get the number of currently active agents.
    pub fn active_count(&self) -> usize {
        self.active_count.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Get the budget remaining.
    pub fn budget_remaining(&self) -> f64 {
        self.config.max_cost_per_command - self.current_cost()
    }

    /// Check if budget allows another operation.
    pub fn has_budget(&self) -> bool {
        self.budget_remaining() > 0.0
    }

    /// Get the scheduler configuration.
    pub fn config(&self) -> &SchedulerConfig {
        &self.config
    }

    /// Run a thermal decay cycle on the database.
    pub fn run_decay_cycle(&self, db: &ThermalDb, elapsed_seconds: f64) -> Result<(), EngineError> {
        let scores = db.get_all_scores()?;
        for score in &scores {
            let decayed = thermal::apply_decay(
                score.decay_factor,
                elapsed_seconds,
                self.config.decay_half_life_seconds,
            );
            db.update_decay_factor(score.id, decayed)?;
        }
        Ok(())
    }

    /// Run a thermal merge cycle: re-aggregate all file scores.
    pub fn run_merge_cycle(&self, db: &ThermalDb, temperature: f64) -> Result<(), EngineError> {
        let scores = db.get_all_scores()?;

        // Group scores by file_path
        let mut file_scores: std::collections::HashMap<String, Vec<f64>> =
            std::collections::HashMap::new();
        for s in &scores {
            file_scores
                .entry(s.file_path.clone())
                .or_default()
                .push(s.score * s.decay_factor);
        }

        for (file_path, scores_vec) in &file_scores {
            let composite = thermal::thermal_merge(scores_vec, temperature)?;
            let max = scores_vec.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
            let density = db.agent_density_at(file_path)?;
            db.upsert_aggregate(
                file_path,
                composite.clamp(0.0, 1.0),
                max.clamp(0.0, 1.0),
                density,
            )?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scheduler_budget_tracking() {
        let scheduler = Scheduler::new(SchedulerConfig {
            max_cost_per_command: 0.10,
            ..Default::default()
        });
        assert!(scheduler.record_cost(0.05).is_ok());
        assert!((scheduler.current_cost() - 0.05).abs() < 0.001);
        assert!(scheduler.has_budget());
        // Exceeding budget
        let result = scheduler.record_cost(0.06);
        assert!(result.is_err());
    }

    #[test]
    fn test_verify_result() {
        let ok = VerifyResult {
            parse_ok: true,
            test_ok: true,
            lint_ok: true,
            critique: None,
            errors: vec![],
        };
        assert!(ok.is_ok());

        let fail = VerifyResult {
            parse_ok: true,
            test_ok: false,
            lint_ok: true,
            critique: None,
            errors: vec!["test failed".to_string()],
        };
        assert!(!fail.is_ok());
    }

    #[test]
    fn test_scheduler_active_count() {
        let scheduler = Scheduler::new(SchedulerConfig::default());
        assert_eq!(scheduler.active_count(), 0);
    }

    #[test]
    fn test_merge_cycle_with_db() {
        let db = ThermalDb::in_memory().unwrap();
        db.upsert_thermal_score("a.rs", 1, 50, 0.8, "complexity", "plan", "a1")
            .unwrap();
        db.upsert_thermal_score("a.rs", 1, 50, 0.5, "dependency", "plan", "a1")
            .unwrap();
        db.upsert_thermal_score("a.rs", 1, 50, 0.6, "risk", "plan", "a1")
            .unwrap();
        db.upsert_thermal_score("a.rs", 1, 50, 0.4, "churn", "plan", "a1")
            .unwrap();
        db.upsert_thermal_score("b.rs", 1, 20, 0.3, "complexity", "plan", "a1")
            .unwrap();

        let scheduler = Scheduler::new(SchedulerConfig::default());
        scheduler.run_merge_cycle(&db, 1.0).unwrap();

        let aggs = db.get_all_aggregates().unwrap();
        assert_eq!(aggs.len(), 2);
    }
}
