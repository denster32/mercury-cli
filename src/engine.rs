//! Engine — the four-layer intelligence pipeline.
//!
//! - **Planner**: goal + repo map → Mercury 2 → execution plan + thermal scores
//! - **Patcher**: file slice + instruction → Mercury Edit → patched code
//! - **Verifier**: tree-sitter parse → test → lint → optional Mercury 2 critique
//! - **Scheduler**: tokio concurrency pool, budget tracking, thermal merge cycles

use std::collections::{hash_map::DefaultHasher, BTreeMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::process::Command;
use std::sync::{
    atomic::{AtomicI64, Ordering},
    Arc,
};
use std::time::Instant;
use std::{collections::HashMap, path::PathBuf};

use chrono::Utc;
use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::api::{
    planner_response_json_schema_v1, ApiError, ApiUsage, Mercury2Api, MercuryEditApi,
    ThermalAssessment, PLANNER_RESPONSE_SCHEMA_NAME, THERMAL_ANALYSIS_PROMPT,
};
use crate::db::{DbError, ThermalDb};
use crate::failure_parser::{
    classify_verifier_command, parse_command_parts, parse_verifier_failure, ParsedFailureReport,
    VerifierCommandKind,
};
use crate::repo::{prepare_repair_workspace, RepoError};
use crate::swarm::DensityController;
use crate::thermal::{self, ThermalError};
use crate::verification::build_allowlisted_verifier_command;

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
    /// Total estimated tokens consumed while generating this plan.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_tokens: Option<i64>,
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
        let mut total_usage = ApiUsage::default();

        let system_prompt = format!(
            "{}\n\n{}",
            THERMAL_ANALYSIS_PROMPT, self.constitutional_prompt
        );

        let user_msg = format!("Goal: {}\n\nRepository Map:\n{}", goal, repo_map);

        let (response, usage) = self
            .api
            .chat_json_schema_value(
                &system_prompt,
                &user_msg,
                4096,
                PLANNER_RESPONSE_SCHEMA_NAME,
                planner_response_json_schema_v1(),
            )
            .await?;
        total_usage.tokens_used += usage.tokens_used;
        total_usage.cost_usd += usage.cost_usd;

        let parsed: PlannerResponse =
            serde_json::from_value(response).map_err(ApiError::JsonParse)?;
        if parsed.schema_version != PLANNER_RESPONSE_SCHEMA_NAME {
            return Err(ApiError::SchemaViolation(format!(
                "expected schema_version {}, got {}",
                PLANNER_RESPONSE_SCHEMA_NAME, parsed.schema_version
            ))
            .into());
        }

        let plan = ExecutionPlan {
            steps: parsed.steps,
            constitutional_prompt: self.constitutional_prompt.clone(),
            estimated_cost: total_usage.cost_usd,
            estimated_tokens: Some(total_usage.tokens_used),
        };

        Ok((plan, parsed.assessments))
    }
}

#[derive(Serialize, Deserialize)]
struct PlannerResponse {
    schema_version: String,
    #[serde(default)]
    steps: Vec<PlanStep>,
    #[serde(default)]
    assessments: Vec<ThermalAssessment>,
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
        self.next_edit_with_path("", file_content, edit_history)
            .await
    }

    /// Predict the next edit based on file content, history, and file path.
    pub async fn next_edit_with_path(
        &self,
        current_file_path: &str,
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
        let (result, usage) = self
            .api
            .next_edit_with_path(current_file_path, &payload)
            .await?;
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
        self.verify_internal(file_path, patched_content, project_root, true)
            .await
    }

    async fn verify_internal(
        &self,
        file_path: &Path,
        patched_content: &str,
        project_root: &Path,
        allow_critique: bool,
    ) -> Result<VerifyResult, EngineError> {
        let mut result = VerifyResult {
            parse_ok: true,
            test_ok: true,
            lint_ok: true,
            critique: None,
            command_results: Vec::new(),
            errors: Vec::new(),
        };

        // Step 1: tree-sitter parse check
        if self.config.parse_before_write {
            match self.check_parse(file_path, patched_content) {
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

        if let Some(parent) = file_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Write the file before running tests/lint
        std::fs::write(file_path, patched_content)?;

        // Step 2: run tests
        if self.config.test_after_write {
            match self.run_command(&self.config.test_command, project_root) {
                Ok(output) => {
                    let failed = !output.success;
                    let failure_summary = output.failure_summary();
                    result
                        .command_results
                        .push(VerifierCommandResult::from(&output));
                    if failed {
                        result.test_ok = false;
                        result
                            .errors
                            .push(format!("tests failed: {failure_summary}"));
                    }
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
                Ok(output) => {
                    let failed = !output.success;
                    let failure_summary = output.failure_summary();
                    result
                        .command_results
                        .push(VerifierCommandResult::from(&output));
                    if failed {
                        result.lint_ok = false;
                        result
                            .errors
                            .push(format!("lint failed: {failure_summary}"));
                    }
                }
                Err(e) => {
                    result.lint_ok = false;
                    result.errors.push(format!("lint command failed: {e}"));
                }
            }
        }

        // Step 4: Mercury 2 critique on failure
        if allow_critique && !result.is_ok() && self.config.mercury2_critique_on_failure {
            if let Some(ref api) = self.critique_api {
                let critique = self
                    .get_critique(api, patched_content, &result.errors)
                    .await;
                result.critique = critique.ok();
            }
        }

        Ok(result)
    }

    /// Run verification against an isolated workspace that already contains the
    /// candidate files to evaluate.
    pub async fn verify_workspace(
        &self,
        accepted_states: &HashMap<PathBuf, String>,
        workspace_root: &Path,
    ) -> Result<VerifyResult, EngineError> {
        let mut result = VerifyResult {
            parse_ok: true,
            test_ok: true,
            lint_ok: true,
            critique: None,
            command_results: Vec::new(),
            errors: Vec::new(),
        };

        if self.config.parse_before_write {
            for (relative_path, content) in accepted_states {
                match self.check_parse(relative_path, content) {
                    Ok(true) => {}
                    Ok(false) => {
                        result.parse_ok = false;
                        result.errors.push(format!(
                            "tree-sitter parse produced errors for {}",
                            relative_path.display()
                        ));
                    }
                    Err(err) => {
                        result.parse_ok = false;
                        result.errors.push(format!(
                            "parse check failed for {}: {err}",
                            relative_path.display()
                        ));
                    }
                }
            }
        }

        if self.config.test_after_write {
            match self.run_command(&self.config.test_command, workspace_root) {
                Ok(output) => {
                    let failed = !output.success;
                    let failure_summary = output.failure_summary();
                    result
                        .command_results
                        .push(VerifierCommandResult::from(&output));
                    if failed {
                        result.test_ok = false;
                        result
                            .errors
                            .push(format!("tests failed in workspace: {failure_summary}"));
                    }
                }
                Err(err) => {
                    result.test_ok = false;
                    result
                        .errors
                        .push(format!("test command failed in workspace: {err}"));
                }
            }
        }

        if self.config.lint_after_write {
            match self.run_command(&self.config.lint_command, workspace_root) {
                Ok(output) => {
                    let failed = !output.success;
                    let failure_summary = output.failure_summary();
                    result
                        .command_results
                        .push(VerifierCommandResult::from(&output));
                    if failed {
                        result.lint_ok = false;
                        result
                            .errors
                            .push(format!("lint failed in workspace: {failure_summary}"));
                    }
                }
                Err(err) => {
                    result.lint_ok = false;
                    result
                        .errors
                        .push(format!("lint command failed in workspace: {err}"));
                }
            }
        }

        if !result.is_ok() && self.config.mercury2_critique_on_failure {
            if let Some(ref api) = self.critique_api {
                let changed_code = accepted_states
                    .iter()
                    .map(|(relative_path, content)| {
                        format!("// {}\n{}", relative_path.display(), content)
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n");
                let critique = self.get_critique(api, &changed_code, &result.errors).await;
                result.critique = critique.ok();
            }
        }

        Ok(result)
    }

    fn check_parse(&self, file_path: &Path, source: &str) -> Result<bool, EngineError> {
        if file_path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
            return Ok(true);
        }

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
        let trimmed = cmd.trim();
        if trimmed.is_empty() {
            return Err(EngineError::VerificationFailed {
                reason: "empty command".to_string(),
            });
        }
        if trimmed.contains('\0') {
            return Err(EngineError::VerificationFailed {
                reason: "command contains NUL byte".to_string(),
            });
        }

        let command_parts = parse_command_parts(trimmed);
        let mut command = build_allowlisted_verifier_command(&command_parts, working_dir)
            .map_err(|reason| EngineError::VerificationFailed { reason })?;
        let output = command.output()?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let parsed_failure = if !output.status.success() {
            let kind = classify_verifier_command(&command_parts);
            if matches!(kind, VerifierCommandKind::Unknown) {
                None
            } else {
                Some(parse_verifier_failure(
                    &kind,
                    &command_parts,
                    &stdout,
                    &stderr,
                ))
            }
        } else {
            None
        };

        Ok(CommandOutput {
            command: trimmed.to_string(),
            success: output.status.success(),
            exit_code: output.status.code(),
            stdout,
            stderr,
            parsed_failure,
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
#[derive(Debug, Clone, Serialize)]
pub struct VerifierCommandResult {
    pub command: String,
    pub success: bool,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub parsed_failure: Option<ParsedFailureReport>,
}

impl From<&CommandOutput> for VerifierCommandResult {
    fn from(output: &CommandOutput) -> Self {
        Self {
            command: output.command.clone(),
            success: output.success,
            exit_code: output.exit_code,
            stdout: output.stdout.clone(),
            stderr: output.stderr.clone(),
            parsed_failure: output.parsed_failure.clone(),
        }
    }
}

/// Result of a verification run.
#[derive(Debug, Clone, Serialize)]
pub struct VerifyResult {
    pub parse_ok: bool,
    pub test_ok: bool,
    pub lint_ok: bool,
    pub critique: Option<String>,
    pub command_results: Vec<VerifierCommandResult>,
    pub errors: Vec<String>,
}

/// Aggregate execution results for `fix` step orchestration.
#[derive(Debug, Clone, Default, Serialize)]
pub struct StepExecutionSummary {
    pub accepted: usize,
    pub rejected: usize,
    pub verification_failures: usize,
    pub retry_attempts: usize,
    pub time_to_first_candidate_ms: Option<u64>,
    pub time_to_verified_repair_ms: Option<u64>,
    pub final_bundle_verified: bool,
    pub applied: bool,
    pub run_root: Option<PathBuf>,
    pub final_verification: Option<VerifyResult>,
}

impl StepExecutionSummary {
    pub fn total(&self) -> usize {
        self.accepted + self.rejected
    }

    fn merge(&mut self, other: StepExecutionSummary) {
        self.accepted += other.accepted;
        self.rejected += other.rejected;
        self.verification_failures += other.verification_failures;
        self.retry_attempts += other.retry_attempts;
        self.time_to_first_candidate_ms = merge_min_duration(
            self.time_to_first_candidate_ms,
            other.time_to_first_candidate_ms,
        );
        self.time_to_verified_repair_ms = merge_min_duration(
            self.time_to_verified_repair_ms,
            other.time_to_verified_repair_ms,
        );
        self.final_bundle_verified |= other.final_bundle_verified;
        self.applied |= other.applied;
        if self.run_root.is_none() {
            self.run_root = other.run_root;
        }
        if self.final_verification.is_none() {
            self.final_verification = other.final_verification;
        }
    }
}

fn merge_min_duration(current: Option<u64>, other: Option<u64>) -> Option<u64> {
    match (current, other) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(left), None) => Some(left),
        (None, Some(right)) => Some(right),
        (None, None) => None,
    }
}

struct ExecutionTelemetry {
    swarm_id: i64,
    agents_spawned: AtomicI64,
    total_tokens: AtomicI64,
    iterations: AtomicI64,
}

impl ExecutionTelemetry {
    fn new(swarm_id: i64, agents_spawned: i64, total_tokens: i64, iterations: i64) -> Self {
        Self {
            swarm_id,
            agents_spawned: AtomicI64::new(agents_spawned),
            total_tokens: AtomicI64::new(total_tokens),
            iterations: AtomicI64::new(iterations),
        }
    }

    fn record_spawn(&self) -> i64 {
        self.agents_spawned.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn record_usage(&self, tokens: i64) {
        self.total_tokens.fetch_add(tokens, Ordering::Relaxed);
    }

    fn next_iteration(&self) -> i64 {
        self.iterations.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn sync(
        &self,
        db: &ThermalDb,
        scheduler: &Scheduler,
        temperature: f64,
        active_agents: i64,
    ) -> Result<(), EngineError> {
        db.update_swarm_state(
            self.swarm_id,
            self.agents_spawned.load(Ordering::Relaxed),
            active_agents,
            self.total_tokens.load(Ordering::Relaxed),
            scheduler.current_cost(),
            temperature,
            self.iterations.load(Ordering::Relaxed),
        )?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct ChangeFootprint {
    touched_lines: usize,
    byte_delta: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum CandidateSource {
    ApplyEdit,
    CritiqueRetry,
    ExploratoryNextEdit,
}

impl CandidateSource {
    fn as_str(self) -> &'static str {
        match self {
            CandidateSource::ApplyEdit => "apply_edit",
            CandidateSource::CritiqueRetry => "critique_retry",
            CandidateSource::ExploratoryNextEdit => "exploratory_next_edit",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CandidateFailureStage {
    Generation,
    Safety,
    Verification,
}

impl CandidateFailureStage {
    fn as_str(self) -> &'static str {
        match self {
            CandidateFailureStage::Generation => "generation",
            CandidateFailureStage::Safety => "safety",
            CandidateFailureStage::Verification => "verification",
        }
    }
}

#[derive(Debug)]
struct CandidateOutcome {
    agent_id: String,
    log_id: i64,
    sandbox_root: PathBuf,
    source: CandidateSource,
    candidate: Option<String>,
    retry_attempts: usize,
    total_tokens: i64,
    total_cost: f64,
    verification_errors: Vec<String>,
    state_hash: Option<String>,
    change_footprint: ChangeFootprint,
    failure_stage: Option<CandidateFailureStage>,
    reason: Option<String>,
}

/// Execute a plan's steps by patching and verifying each candidate edit.
pub async fn execute_plan_steps<E: MercuryEditApi, A: Mercury2Api>(
    plan: &ExecutionPlan,
    patcher: &Patcher<E>,
    verifier: &Verifier<A>,
    scheduler: &Scheduler,
    db: &ThermalDb,
    project_root: &Path,
) -> Result<StepExecutionSummary, EngineError> {
    let run_root = create_run_root(project_root)?;
    let started = Instant::now();
    let swarm_state = db.get_swarm_state()?;
    let swarm_id = swarm_state
        .as_ref()
        .map(|state| state.id)
        .unwrap_or(db.init_swarm()?);
    let telemetry = Arc::new(ExecutionTelemetry::new(
        swarm_id,
        swarm_state
            .as_ref()
            .map(|state| state.total_agents_spawned)
            .unwrap_or_default(),
        swarm_state
            .as_ref()
            .map(|state| state.total_tokens_used)
            .unwrap_or_default(),
        swarm_state
            .as_ref()
            .map(|state| state.iteration_count)
            .unwrap_or_default(),
    ));
    telemetry.sync(db, scheduler, 0.0, scheduler.active_count() as i64)?;
    let mut grouped_steps: BTreeMap<String, Vec<IndexedPlanStep>> = BTreeMap::new();

    for (index, step) in plan.steps.iter().cloned().enumerate() {
        grouped_steps
            .entry(step.file_path.clone())
            .or_default()
            .push(IndexedPlanStep { index, step });
    }

    let mut file_states = BTreeMap::<String, FileExecutionState>::new();
    for (file_key, steps) in grouped_steps {
        if db.is_file_locked(&file_key)? {
            continue;
        }

        ensure_file_aggregate(
            db,
            &file_key,
            steps
                .first()
                .map(|indexed| indexed.step.priority)
                .unwrap_or_default(),
        )?;
        let relative_path = PathBuf::from(&file_key);
        let latest_state = read_workspace_file(project_root, &relative_path)?;
        file_states.insert(
            file_key.clone(),
            FileExecutionState::new(file_key, relative_path, steps, latest_state),
        );
    }

    let mut summary = StepExecutionSummary {
        run_root: Some(run_root.clone()),
        ..StepExecutionSummary::default()
    };

    let mut accepted_snapshot = HashMap::<PathBuf, String>::new();
    let mut accepted_candidates = BTreeMap::<PathBuf, AcceptedCandidate>::new();
    for phase in [
        thermal::ExecutionPhase::Scaffolding,
        thermal::ExecutionPhase::Resolution,
        thermal::ExecutionPhase::Annealing,
    ] {
        loop {
            let batch =
                build_dispatch_batch(&file_states, &accepted_snapshot, scheduler, db, phase)?;
            if batch.is_empty() {
                break;
            }

            telemetry.sync(
                db,
                scheduler,
                phase_temperature(phase),
                scheduler.active_count() as i64,
            )?;
            let max_concurrency = scheduler.config().max_concurrency.max(1);
            let partials = stream::iter(batch.into_iter().map(|work_item| {
                let run_root = run_root.clone();
                let telemetry = Arc::clone(&telemetry);
                async move {
                    execute_dispatch_work_item(
                        work_item,
                        &run_root,
                        DispatchContext {
                            patcher,
                            verifier,
                            scheduler,
                            db,
                            project_root,
                            telemetry: &telemetry,
                            started_at: started,
                        },
                    )
                    .await
                }
            }))
            .buffer_unordered(max_concurrency)
            .collect::<Vec<_>>()
            .await;

            for partial in partials {
                let partial = partial?;
                summary.merge(partial.summary);
                let file_state = file_states
                    .get_mut(&partial.file_key)
                    .expect("dispatch result must map to a known file state");
                let _ = file_state.steps.pop_front();
                if let Some(winner) = partial.winner {
                    file_state.latest_state = winner.content.clone();
                    file_state.seen_hashes.insert(winner.state_hash.clone());
                    accepted_snapshot.insert(winner.relative_path.clone(), winner.content.clone());
                    accepted_candidates.insert(winner.relative_path.clone(), winner);
                }
            }
        }
    }

    if accepted_snapshot.is_empty() {
        telemetry.sync(db, scheduler, 0.0, scheduler.active_count() as i64)?;
        return Ok(summary);
    }

    let final_workspace = run_root.join("final-bundle");
    prepare_workspace(project_root, &final_workspace, &accepted_snapshot)?;
    let final_verify = verifier
        .verify_workspace(&accepted_snapshot, &final_workspace)
        .await?;
    summary.final_bundle_verified = final_verify.is_ok();
    summary.final_verification = Some(final_verify.clone());

    let original_workspace = run_root.join("original-bundle");
    prepare_workspace(project_root, &original_workspace, &HashMap::new())?;
    write_bundle_diff(&run_root, &original_workspace, &final_workspace)?;

    if final_verify.is_ok() {
        summary.time_to_verified_repair_ms = Some(started.elapsed().as_millis() as u64);
        apply_changes_atomically(project_root, &accepted_snapshot)?;
        for accepted in accepted_candidates.values() {
            write_cool_lock(
                db,
                &accepted.file_key,
                &accepted.content,
                &accepted.agent_id,
            )?;
            db.lock_aggregate(&accepted.file_key)?;
        }
        summary.applied = true;
    } else {
        summary.verification_failures += 1;
    }

    telemetry.sync(db, scheduler, 0.0, scheduler.active_count() as i64)?;

    Ok(summary)
}

impl VerifyResult {
    /// Returns true if all checks passed.
    pub fn is_ok(&self) -> bool {
        self.parse_ok && self.test_ok && self.lint_ok
    }
}

#[derive(Debug, Clone)]
struct IndexedPlanStep {
    index: usize,
    step: PlanStep,
}

#[derive(Debug)]
struct FileExecutionState {
    file_key: String,
    relative_path: PathBuf,
    steps: VecDeque<IndexedPlanStep>,
    latest_state: String,
    seen_hashes: HashSet<String>,
}

impl FileExecutionState {
    fn new(
        file_key: String,
        relative_path: PathBuf,
        steps: Vec<IndexedPlanStep>,
        latest_state: String,
    ) -> Self {
        Self {
            file_key,
            relative_path,
            steps: steps.into(),
            latest_state: latest_state.clone(),
            seen_hashes: HashSet::from([content_hash(&latest_state)]),
        }
    }

    fn next_phase(&self) -> Option<thermal::ExecutionPhase> {
        let total_steps = self.steps.len().max(1);
        self.steps.front().map(|indexed_step| {
            thermal::phase_from_progress((indexed_step.index + 1) as i64, total_steps as i64)
        })
    }

    fn priority(&self) -> f64 {
        self.steps
            .front()
            .map(|indexed_step| indexed_step.step.priority)
            .unwrap_or_default()
    }
}

#[derive(Debug)]
struct DispatchWorkItem {
    file_key: String,
    relative_path: PathBuf,
    indexed_step: IndexedPlanStep,
    phase: thermal::ExecutionPhase,
    fanout: usize,
    latest_state: String,
    seen_hashes: HashSet<String>,
    accepted_snapshot: HashMap<PathBuf, String>,
}

#[derive(Debug, Clone)]
struct AcceptedCandidate {
    file_key: String,
    relative_path: PathBuf,
    content: String,
    agent_id: String,
    state_hash: String,
}

#[derive(Debug)]
struct StepDispatchResult {
    file_key: String,
    summary: StepExecutionSummary,
    winner: Option<AcceptedCandidate>,
}

struct DispatchContext<'a, E: MercuryEditApi, A: Mercury2Api> {
    patcher: &'a Patcher<E>,
    verifier: &'a Verifier<A>,
    scheduler: &'a Scheduler,
    db: &'a ThermalDb,
    project_root: &'a Path,
    telemetry: &'a ExecutionTelemetry,
    started_at: Instant,
}

fn build_dispatch_batch(
    file_states: &BTreeMap<String, FileExecutionState>,
    accepted_snapshot: &HashMap<PathBuf, String>,
    scheduler: &Scheduler,
    db: &ThermalDb,
    phase: thermal::ExecutionPhase,
) -> Result<Vec<DispatchWorkItem>, EngineError> {
    let eligible_files = file_states
        .iter()
        .filter_map(|(file_key, state)| {
            (state.next_phase() == Some(phase)).then_some(file_key.as_str())
        })
        .collect::<HashSet<_>>();
    if eligible_files.is_empty() {
        return Ok(Vec::new());
    }

    let max_concurrency = scheduler.config().max_concurrency.max(1);
    let density_controller = DensityController::new(max_concurrency as i32);
    let dispatch_targets = thermal::dispatch_targets(
        &db.get_all_aggregates()?,
        &db.get_all_locks()?,
        &db.get_active_agents()?,
        phase,
        max_concurrency as i32,
    );

    let mut batch = dispatch_targets
        .into_iter()
        .filter(|target| {
            eligible_files.contains(target.file_path.as_str())
                && matches!(target.readiness, thermal::DispatchReadiness::LaunchNow)
        })
        .filter_map(|target| {
            let state = file_states.get(target.file_path.as_str())?;
            let indexed_step = state.steps.front()?.clone();
            let fanout = candidate_fanout(
                phase,
                max_concurrency,
                state.priority(),
                target.active_agents,
                &density_controller,
            )
            .min(target.launchable_agents.max(1));
            (fanout > 0).then(|| DispatchWorkItem {
                file_key: state.file_key.clone(),
                relative_path: state.relative_path.clone(),
                indexed_step,
                phase,
                fanout,
                latest_state: state.latest_state.clone(),
                seen_hashes: state.seen_hashes.clone(),
                accepted_snapshot: accepted_snapshot.clone(),
            })
        })
        .collect::<Vec<_>>();

    if batch.is_empty() {
        let mut fallback = file_states
            .values()
            .filter(|state| state.next_phase() == Some(phase))
            .collect::<Vec<_>>();
        fallback.sort_by(|left, right| {
            right
                .priority()
                .total_cmp(&left.priority())
                .then_with(|| left.file_key.cmp(&right.file_key))
        });
        if let Some(state) = fallback.into_iter().next() {
            if let Some(indexed_step) = state.steps.front().cloned() {
                let fanout = candidate_fanout(
                    phase,
                    max_concurrency,
                    state.priority(),
                    0,
                    &density_controller,
                )
                .max(1);
                batch.push(DispatchWorkItem {
                    file_key: state.file_key.clone(),
                    relative_path: state.relative_path.clone(),
                    indexed_step,
                    phase,
                    fanout,
                    latest_state: state.latest_state.clone(),
                    seen_hashes: state.seen_hashes.clone(),
                    accepted_snapshot: accepted_snapshot.clone(),
                });
            }
        }
    }

    Ok(batch)
}

async fn execute_dispatch_work_item<E: MercuryEditApi, A: Mercury2Api>(
    work_item: DispatchWorkItem,
    run_root: &Path,
    context: DispatchContext<'_, E, A>,
) -> Result<StepDispatchResult, EngineError> {
    let mut summary = StepExecutionSummary::default();
    let step_root = run_root.join("candidates").join(format!(
        "step-{:04}-{}",
        work_item.indexed_step.index + 1,
        sanitize_path_component(&work_item.indexed_step.step.file_path)
    ));
    let phase_temperature = phase_temperature(work_item.phase);
    let outcomes = stream::iter((0..work_item.fanout).map(|candidate_index| {
        execute_candidate_variant(
            candidate_index,
            &work_item.indexed_step,
            work_item.phase,
            &work_item.latest_state,
            &work_item.accepted_snapshot,
            context.patcher,
            context.verifier,
            context.scheduler,
            context.db,
            context.project_root,
            &step_root,
            context.telemetry,
        )
    }))
    .buffer_unordered(work_item.fanout)
    .collect::<Vec<_>>()
    .await;

    let mut verified_candidates = Vec::new();
    for outcome in outcomes {
        let outcome = outcome?;
        summary.retry_attempts += outcome.retry_attempts;
        if outcome.candidate.is_some() {
            verified_candidates.push(outcome);
        } else {
            summary.rejected += 1;
            if matches!(
                outcome.failure_stage,
                Some(CandidateFailureStage::Verification)
            ) {
                summary.verification_failures += 1;
            }
            let metadata = serde_json::json!({
                "outcome":"rejected",
                "reason": outcome.reason,
                "sandbox_root": outcome.sandbox_root.display().to_string(),
                "candidate_source": outcome.source.as_str(),
                "failure_stage": outcome.failure_stage.map(CandidateFailureStage::as_str),
                "retry_attempts": outcome.retry_attempts,
                "phase": work_item.phase.to_string(),
            });
            context.db.update_agent_status(
                outcome.log_id,
                "failed",
                outcome.total_tokens,
                outcome.total_cost,
                Some(&metadata.to_string()),
            )?;
        }
    }

    let mut ranked_candidates = Vec::new();
    for mut outcome in verified_candidates {
        if outcome
            .state_hash
            .as_ref()
            .is_some_and(|hash| work_item.seen_hashes.contains(hash))
        {
            summary.rejected += 1;
            outcome.reason = Some(
                "oscillation suppressed: candidate matches a previously accepted file state"
                    .to_string(),
            );
            let metadata = serde_json::json!({
                "outcome":"rejected",
                "reason": outcome.reason,
                "sandbox_root": outcome.sandbox_root.display().to_string(),
                "candidate_source": outcome.source.as_str(),
                "retry_attempts": outcome.retry_attempts,
                "phase": work_item.phase.to_string(),
            });
            context.db.update_agent_status(
                outcome.log_id,
                "failed",
                outcome.total_tokens,
                outcome.total_cost,
                Some(&metadata.to_string()),
            )?;
        } else {
            ranked_candidates.push(outcome);
        }
    }

    rank_candidate_outcomes(&mut ranked_candidates);
    let (ranked_candidates, duplicate_candidates) =
        split_duplicate_state_candidates(ranked_candidates);
    for duplicate in duplicate_candidates {
        summary.rejected += 1;
        let metadata = serde_json::json!({
            "outcome":"rejected",
            "reason":"duplicate verified candidate output",
            "sandbox_root": duplicate.sandbox_root.display().to_string(),
            "candidate_source": duplicate.source.as_str(),
            "retry_attempts": duplicate.retry_attempts,
            "phase": work_item.phase.to_string(),
            "fanout": work_item.fanout,
            "temperature": phase_temperature,
            "touched_lines": duplicate.change_footprint.touched_lines,
            "byte_delta": duplicate.change_footprint.byte_delta,
        });
        context.db.update_agent_status(
            duplicate.log_id,
            "failed",
            duplicate.total_tokens,
            duplicate.total_cost,
            Some(&metadata.to_string()),
        )?;
    }

    let mut ranked_iter = ranked_candidates.into_iter();
    let winner = if let Some(winner) = ranked_iter.next() {
        let accepted = winner
            .candidate
            .as_ref()
            .cloned()
            .expect("verified candidate must carry content");
        summary.accepted += 1;
        if summary.time_to_first_candidate_ms.is_none() {
            summary.time_to_first_candidate_ms =
                Some(context.started_at.elapsed().as_millis() as u64);
        }

        let winner_metadata = serde_json::json!({
            "outcome":"accepted",
            "sandbox_root": winner.sandbox_root.display().to_string(),
            "candidate_source": winner.source.as_str(),
            "retry_attempts": winner.retry_attempts,
            "phase": work_item.phase.to_string(),
            "fanout": work_item.fanout,
            "temperature": phase_temperature,
            "touched_lines": winner.change_footprint.touched_lines,
            "byte_delta": winner.change_footprint.byte_delta,
        });
        context.db.update_agent_status(
            winner.log_id,
            "success",
            winner.total_tokens,
            winner.total_cost,
            Some(&winner_metadata.to_string()),
        )?;

        for loser in ranked_iter {
            summary.rejected += 1;
            let metadata = serde_json::json!({
                "outcome":"rejected",
                "reason":"lower-ranked competing candidate",
                "sandbox_root": loser.sandbox_root.display().to_string(),
                "candidate_source": loser.source.as_str(),
                "retry_attempts": loser.retry_attempts,
                "phase": work_item.phase.to_string(),
                "fanout": work_item.fanout,
                "temperature": phase_temperature,
                "touched_lines": loser.change_footprint.touched_lines,
                "byte_delta": loser.change_footprint.byte_delta,
            });
            context.db.update_agent_status(
                loser.log_id,
                "failed",
                loser.total_tokens,
                loser.total_cost,
                Some(&metadata.to_string()),
            )?;
        }

        Some(AcceptedCandidate {
            file_key: work_item.file_key.clone(),
            relative_path: work_item.relative_path.clone(),
            content: accepted,
            agent_id: winner.agent_id,
            state_hash: winner
                .state_hash
                .expect("accepted winner must carry a state hash"),
        })
    } else {
        context.telemetry.sync(
            context.db,
            context.scheduler,
            phase_temperature,
            context.scheduler.active_count() as i64,
        )?;
        None
    };

    Ok(StepDispatchResult {
        file_key: work_item.file_key,
        summary,
        winner,
    })
}

fn read_workspace_file(project_root: &Path, relative_path: &Path) -> Result<String, EngineError> {
    let full_path = project_root.join(relative_path);
    match std::fs::read_to_string(&full_path) {
        Ok(content) => Ok(content),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(err) => Err(EngineError::Io(err)),
    }
}
#[allow(clippy::too_many_arguments)]
async fn execute_candidate_variant<E: MercuryEditApi, A: Mercury2Api>(
    candidate_index: usize,
    indexed_step: &IndexedPlanStep,
    phase: thermal::ExecutionPhase,
    latest_state: &str,
    accepted_snapshot: &HashMap<PathBuf, String>,
    patcher: &Patcher<E>,
    verifier: &Verifier<A>,
    scheduler: &Scheduler,
    db: &ThermalDb,
    project_root: &Path,
    step_root: &Path,
    telemetry: &ExecutionTelemetry,
) -> Result<CandidateOutcome, EngineError> {
    let agent_id = format!(
        "fix-step-{:04}-candidate-{:02}",
        indexed_step.index + 1,
        candidate_index + 1
    );
    let log_id = db.log_agent_spawn(
        &agent_id,
        &format!("fix:{}", phase),
        &indexed_step.step.file_path,
    )?;
    db.update_agent_status(log_id, "running", 0, 0.0, None)?;
    telemetry.record_spawn();
    telemetry.next_iteration();

    let permit = SchedulerPermit::new(scheduler.acquire().await?, scheduler);
    db.increment_density(&indexed_step.step.file_path)?;
    telemetry.sync(
        db,
        scheduler,
        phase_temperature(phase),
        scheduler.active_count() as i64,
    )?;

    let result = async {
        let mut total_tokens = 0i64;
        let mut total_cost = 0.0;
        let sandbox_root = step_root.join(format!("candidate-{:02}", candidate_index + 1));
        let seed_history = build_seed_history(indexed_step, phase, candidate_index);

        let primary = if candidate_index == 0 {
            patcher
                .patch(latest_state, &indexed_step.step.instruction)
                .await
        } else {
            patcher
                .next_edit_with_path(&indexed_step.step.file_path, latest_state, &seed_history)
                .await
        };

        let (candidate, initial_usage) = match primary {
            Ok(result) => result,
            Err(err) => {
                return Ok(CandidateOutcome {
                    agent_id,
                    log_id,
                    sandbox_root: sandbox_root.join("attempt-1"),
                    source: candidate_source(candidate_index, false),
                    candidate: None,
                    retry_attempts: 0,
                    total_tokens,
                    total_cost,
                    verification_errors: Vec::new(),
                    state_hash: None,
                    change_footprint: ChangeFootprint {
                        touched_lines: 0,
                        byte_delta: 0,
                    },
                    failure_stage: Some(CandidateFailureStage::Generation),
                    reason: Some(format!("patch generation failed: {err}")),
                });
            }
        };

        total_tokens += initial_usage.tokens_used;
        total_cost += initial_usage.cost_usd;
        telemetry.record_usage(initial_usage.tokens_used);
        scheduler.record_cost(initial_usage.cost_usd)?;
        telemetry.sync(
            db,
            scheduler,
            phase_temperature(phase),
            scheduler.active_count() as i64,
        )?;

        if let Some(reason) = unsafe_candidate_reason(latest_state, &candidate) {
            return Ok(CandidateOutcome {
                agent_id,
                log_id,
                sandbox_root: sandbox_root.join("attempt-1"),
                source: candidate_source(candidate_index, false),
                candidate: None,
                retry_attempts: 0,
                total_tokens,
                total_cost,
                verification_errors: Vec::new(),
                state_hash: None,
                change_footprint: change_footprint(latest_state, &candidate),
                failure_stage: Some(CandidateFailureStage::Safety),
                reason: Some(format!(
                    "{} candidate rejected: {reason}",
                    candidate_source(candidate_index, false).as_str()
                )),
            });
        }

        let attempt_one_root = sandbox_root.join("attempt-1");
        prepare_workspace(project_root, &attempt_one_root, accepted_snapshot)?;
        let verify = verifier
            .verify(
                &attempt_one_root.join(&indexed_step.step.file_path),
                &candidate,
                &attempt_one_root,
            )
            .await?;

        if verify.is_ok() {
            return Ok(CandidateOutcome {
                agent_id,
                log_id,
                sandbox_root: attempt_one_root,
                source: candidate_source(candidate_index, false),
                candidate: Some(candidate.clone()),
                retry_attempts: 0,
                total_tokens,
                total_cost,
                verification_errors: verify.errors,
                state_hash: Some(content_hash(&candidate)),
                change_footprint: change_footprint(latest_state, &candidate),
                failure_stage: None,
                reason: None,
            });
        }

        if candidate_index == 0 {
            if let Some(critique) = verify.critique.as_deref() {
                let retry_history = format!(
                    "{}\n\n{}",
                    seed_history,
                    build_retry_history(&indexed_step.step.instruction, &verify, critique)
                );
                db.update_agent_status(
                    log_id,
                    "retrying",
                    total_tokens,
                    total_cost,
                    Some(
                        &serde_json::json!({
                            "phase": phase.to_string(),
                            "sandbox_root": attempt_one_root.display().to_string(),
                        })
                        .to_string(),
                    ),
                )?;
                match patcher
                    .next_edit_with_path(&indexed_step.step.file_path, &candidate, &retry_history)
                    .await
                {
                    Ok((retry_candidate, retry_usage)) => {
                        total_tokens += retry_usage.tokens_used;
                        total_cost += retry_usage.cost_usd;
                        telemetry.record_usage(retry_usage.tokens_used);
                        scheduler.record_cost(retry_usage.cost_usd)?;
                        telemetry.sync(
                            db,
                            scheduler,
                            phase_temperature(phase),
                            scheduler.active_count() as i64,
                        )?;

                        if let Some(reason) = unsafe_candidate_reason(&candidate, &retry_candidate)
                        {
                            return Ok(CandidateOutcome {
                                agent_id,
                                log_id,
                                sandbox_root: sandbox_root.join("attempt-2"),
                                source: candidate_source(candidate_index, true),
                                candidate: None,
                                retry_attempts: 1,
                                total_tokens,
                                total_cost,
                                verification_errors: Vec::new(),
                                state_hash: None,
                                change_footprint: change_footprint(latest_state, &retry_candidate),
                                failure_stage: Some(CandidateFailureStage::Safety),
                                reason: Some(format!(
                                    "{} candidate rejected: {reason}",
                                    candidate_source(candidate_index, true).as_str()
                                )),
                            });
                        }

                        let attempt_two_root = sandbox_root.join("attempt-2");
                        prepare_workspace(project_root, &attempt_two_root, accepted_snapshot)?;
                        let retry_verify = verifier
                            .verify_internal(
                                &attempt_two_root.join(&indexed_step.step.file_path),
                                &retry_candidate,
                                &attempt_two_root,
                                false,
                            )
                            .await?;

                        if retry_verify.is_ok() {
                            return Ok(CandidateOutcome {
                                agent_id,
                                log_id,
                                sandbox_root: attempt_two_root,
                                source: candidate_source(candidate_index, true),
                                candidate: Some(retry_candidate.clone()),
                                retry_attempts: 1,
                                total_tokens,
                                total_cost,
                                verification_errors: retry_verify.errors,
                                state_hash: Some(content_hash(&retry_candidate)),
                                change_footprint: change_footprint(latest_state, &retry_candidate),
                                failure_stage: None,
                                reason: None,
                            });
                        }

                        return Ok(CandidateOutcome {
                            agent_id,
                            log_id,
                            sandbox_root: attempt_two_root,
                            source: candidate_source(candidate_index, true),
                            candidate: None,
                            retry_attempts: 1,
                            total_tokens,
                            total_cost,
                            verification_errors: retry_verify.errors.clone(),
                            state_hash: None,
                            change_footprint: ChangeFootprint {
                                touched_lines: 0,
                                byte_delta: 0,
                            },
                            failure_stage: Some(CandidateFailureStage::Verification),
                            reason: Some(join_verification_errors(&retry_verify.errors)),
                        });
                    }
                    Err(err) => {
                        return Ok(CandidateOutcome {
                            agent_id,
                            log_id,
                            sandbox_root: attempt_one_root,
                            source: candidate_source(candidate_index, true),
                            candidate: None,
                            retry_attempts: 1,
                            total_tokens,
                            total_cost,
                            verification_errors: verify.errors.clone(),
                            state_hash: None,
                            change_footprint: ChangeFootprint {
                                touched_lines: 0,
                                byte_delta: 0,
                            },
                            failure_stage: Some(CandidateFailureStage::Generation),
                            reason: Some(format!("retry generation failed: {err}")),
                        });
                    }
                }
            }
        }

        Ok(CandidateOutcome {
            agent_id,
            log_id,
            sandbox_root: attempt_one_root,
            source: candidate_source(candidate_index, false),
            candidate: None,
            retry_attempts: 0,
            total_tokens,
            total_cost,
            verification_errors: verify.errors.clone(),
            state_hash: None,
            change_footprint: ChangeFootprint {
                touched_lines: 0,
                byte_delta: 0,
            },
            failure_stage: Some(CandidateFailureStage::Verification),
            reason: Some(join_verification_errors(&verify.errors)),
        })
    }
    .await;

    db.decrement_density(&indexed_step.step.file_path)?;
    drop(permit);
    telemetry.sync(
        db,
        scheduler,
        phase_temperature(phase),
        scheduler.active_count() as i64,
    )?;
    result
}

fn create_run_root(project_root: &Path) -> Result<PathBuf, EngineError> {
    let run_id = format!("run-{}", Utc::now().format("%Y%m%dT%H%M%S%.3fZ"));
    let run_root = project_root.join(".mercury").join("worktrees").join(run_id);
    std::fs::create_dir_all(&run_root)?;
    Ok(run_root)
}

fn prepare_workspace(
    project_root: &Path,
    workspace_root: &Path,
    accepted_states: &HashMap<PathBuf, String>,
) -> Result<(), EngineError> {
    prepare_repair_workspace(project_root, workspace_root, accepted_states)?;
    Ok(())
}

fn write_bundle_diff(
    run_root: &Path,
    original_workspace: &Path,
    final_workspace: &Path,
) -> Result<(), EngineError> {
    let output = Command::new("git")
        .args([
            "--no-pager",
            "diff",
            "--no-index",
            "--no-color",
            original_workspace.to_string_lossy().as_ref(),
            final_workspace.to_string_lossy().as_ref(),
        ])
        .output()?;

    let status = output.status.code().unwrap_or_default();
    if status != 0 && status != 1 {
        return Err(EngineError::VerificationFailed {
            reason: format!(
                "failed to generate bundle diff: {}",
                String::from_utf8_lossy(&output.stderr)
            ),
        });
    }

    std::fs::write(run_root.join("accepted.patch"), output.stdout)?;
    Ok(())
}

fn apply_changes_atomically(
    project_root: &Path,
    accepted_states: &HashMap<PathBuf, String>,
) -> Result<(), EngineError> {
    let mut originals = HashMap::<PathBuf, Option<Vec<u8>>>::new();
    let mut staged_writes = Vec::<(PathBuf, PathBuf)>::new();

    for (relative_path, content) in accepted_states {
        let target_path = project_root.join(relative_path);
        let original = match std::fs::read(&target_path) {
            Ok(bytes) => Some(bytes),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => return Err(EngineError::Io(err)),
        };
        originals.insert(target_path.clone(), original);

        if let Some(parent) = target_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let temp_path = target_path.with_extension(format!(
            "mercury-{}.tmp",
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        std::fs::write(&temp_path, content)?;
        staged_writes.push((target_path, temp_path));
    }

    let mut applied_targets = Vec::<PathBuf>::new();
    for (target_path, temp_path) in &staged_writes {
        if let Err(err) = std::fs::rename(temp_path, target_path) {
            rollback_atomic_targets(&applied_targets, &originals)?;
            return Err(EngineError::Io(err));
        }
        applied_targets.push(target_path.clone());
    }

    Ok(())
}

fn rollback_atomic_targets(
    applied_targets: &[PathBuf],
    originals: &HashMap<PathBuf, Option<Vec<u8>>>,
) -> Result<(), EngineError> {
    for target_path in applied_targets {
        match originals.get(target_path).cloned().flatten() {
            Some(original) => {
                let rollback_path = target_path.with_extension("mercury-rollback.tmp");
                std::fs::write(&rollback_path, original)?;
                std::fs::rename(&rollback_path, target_path)?;
            }
            None => {
                if target_path.exists() {
                    std::fs::remove_file(target_path)?;
                }
            }
        }
    }
    Ok(())
}

fn sanitize_path_component(file_path: &str) -> String {
    file_path
        .chars()
        .map(|ch| match ch {
            '/' | '\\' | ':' | ' ' => '_',
            other => other,
        })
        .collect()
}

struct CommandOutput {
    command: String,
    success: bool,
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    parsed_failure: Option<ParsedFailureReport>,
}

impl CommandOutput {
    fn failure_summary(&self) -> String {
        let stderr = self.stderr.trim();
        if !stderr.is_empty() {
            return stderr.to_string();
        }

        let stdout = self.stdout.trim();
        if !stdout.is_empty() {
            return stdout.to_string();
        }

        match self.exit_code {
            Some(code) => format!("command exited with status {code}"),
            None => "command exited without a status code".to_string(),
        }
    }
}

fn build_retry_history(instruction: &str, verify: &VerifyResult, critique: &str) -> String {
    let mut sections = vec![format!("original_instruction:\n{instruction}")];

    if !verify.errors.is_empty() {
        sections.push(format!(
            "verification_errors:\n{}",
            verify.errors.join("\n")
        ));
    }

    let parsed_failures = verify
        .command_results
        .iter()
        .filter_map(|result| result.parsed_failure.as_ref())
        .collect::<Vec<_>>();
    if !parsed_failures.is_empty() {
        let structured =
            serde_json::to_string_pretty(&parsed_failures).unwrap_or_else(|_| "[]".to_string());
        sections.push(format!("structured_failures:\n{structured}"));
    }

    sections.push(format!("critique:\n{critique}"));
    sections.join("\n\n")
}

fn ensure_file_aggregate(db: &ThermalDb, file_key: &str, priority: f64) -> Result<(), EngineError> {
    let score = priority.clamp(0.0, 1.0);
    if let Some(existing) = db.get_aggregate(file_key)? {
        db.upsert_aggregate(
            file_key,
            existing.composite_score.max(score),
            existing.max_score.max(score),
            existing.agent_density,
        )?;
    } else {
        db.upsert_aggregate(file_key, score, score, 0)?;
    }
    Ok(())
}

fn content_hash(source: &str) -> String {
    let mut hasher = DefaultHasher::new();
    source.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn change_footprint(before: &str, after: &str) -> ChangeFootprint {
    let before_lines = before.lines().collect::<Vec<_>>();
    let after_lines = after.lines().collect::<Vec<_>>();
    let shared_prefix = before_lines
        .iter()
        .zip(after_lines.iter())
        .take_while(|(left, right)| left == right)
        .count();
    let shared_suffix = before_lines[shared_prefix..]
        .iter()
        .rev()
        .zip(after_lines[shared_prefix..].iter().rev())
        .take_while(|(left, right)| left == right)
        .count();
    let removed = before_lines
        .len()
        .saturating_sub(shared_prefix + shared_suffix);
    let added = after_lines
        .len()
        .saturating_sub(shared_prefix + shared_suffix);

    ChangeFootprint {
        touched_lines: removed + added,
        byte_delta: after.len().abs_diff(before.len()),
    }
}

fn unsafe_candidate_reason(before: &str, after: &str) -> Option<&'static str> {
    if !before.trim().is_empty() && after.trim().is_empty() {
        return Some("blank rewrite would erase a non-empty file");
    }

    None
}

fn candidate_source(candidate_index: usize, retry: bool) -> CandidateSource {
    if retry {
        CandidateSource::CritiqueRetry
    } else if candidate_index == 0 {
        CandidateSource::ApplyEdit
    } else {
        CandidateSource::ExploratoryNextEdit
    }
}

fn rank_candidate_outcomes(candidates: &mut [CandidateOutcome]) {
    candidates.sort_by(|left, right| {
        left.source
            .cmp(&right.source)
            .then_with(|| left.retry_attempts.cmp(&right.retry_attempts))
            .then_with(|| left.change_footprint.cmp(&right.change_footprint))
            .then_with(|| {
                left.verification_errors
                    .len()
                    .cmp(&right.verification_errors.len())
            })
            .then_with(|| {
                left.total_cost
                    .partial_cmp(&right.total_cost)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| left.agent_id.cmp(&right.agent_id))
    });
}

fn split_duplicate_state_candidates(
    candidates: Vec<CandidateOutcome>,
) -> (Vec<CandidateOutcome>, Vec<CandidateOutcome>) {
    let mut unique_candidates = Vec::with_capacity(candidates.len());
    let mut duplicate_candidates = Vec::new();
    let mut seen_hashes = HashSet::new();

    for candidate in candidates {
        let Some(state_hash) = candidate.state_hash.as_ref() else {
            unique_candidates.push(candidate);
            continue;
        };
        if seen_hashes.insert(state_hash.clone()) {
            unique_candidates.push(candidate);
        } else {
            duplicate_candidates.push(candidate);
        }
    }

    (unique_candidates, duplicate_candidates)
}

fn phase_temperature(phase: thermal::ExecutionPhase) -> f64 {
    match phase {
        thermal::ExecutionPhase::Scaffolding => 0.20,
        thermal::ExecutionPhase::Resolution => 0.65,
        thermal::ExecutionPhase::Annealing => 0.35,
    }
}

fn candidate_fanout(
    phase: thermal::ExecutionPhase,
    max_concurrency: usize,
    priority: f64,
    current_density: i32,
    density_controller: &DensityController,
) -> usize {
    let bounded_priority = priority.clamp(0.0, 1.0);
    let max_density = density_controller
        .max_density_for_score(bounded_priority)
        .max(1) as usize;
    let current_density = current_density.max(0) as usize;
    if current_density >= max_density {
        return 0;
    }

    let remaining_capacity = max_density.saturating_sub(current_density);
    let phase_cap = match phase {
        thermal::ExecutionPhase::Scaffolding => 1,
        thermal::ExecutionPhase::Resolution => max_density,
        thermal::ExecutionPhase::Annealing => max_density.div_ceil(2),
    };

    remaining_capacity
        .min(phase_cap.max(1))
        .min(max_concurrency.max(1))
}

fn write_cool_lock(
    db: &ThermalDb,
    file_key: &str,
    content: &str,
    agent_id: &str,
) -> Result<(), EngineError> {
    for lock in db
        .get_all_locks()?
        .into_iter()
        .filter(|lock| lock.file_path == file_key)
    {
        db.remove_cool_lock(&lock.file_path, lock.line_start, lock.line_end)?;
    }

    let line_end = content.lines().count().max(1) as u32;
    db.insert_cool_lock(file_key, 1, line_end, &content_hash(content), agent_id)?;
    Ok(())
}

fn build_seed_history(
    indexed_step: &IndexedPlanStep,
    phase: thermal::ExecutionPhase,
    candidate_index: usize,
) -> String {
    let instruction = indexed_step
        .step
        .instruction
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| format!("+ {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let instruction = if instruction.is_empty() {
        "+ (no explicit instruction provided)".to_string()
    } else {
        instruction
    };

    format!(
        concat!(
            "--- a/{path}\n",
            "+++ b/{path}\n",
            "@@ -0,0 +1,5 @@\n",
            "+ phase: {phase}\n",
            "+ candidate: {candidate}\n",
            "+ priority: {priority:.3}\n",
            "+ file: {path}\n",
            "{instruction}\n"
        ),
        path = indexed_step.step.file_path,
        phase = phase,
        candidate = candidate_index + 1,
        priority = indexed_step.step.priority,
        instruction = instruction,
    )
}

fn join_verification_errors(errors: &[String]) -> String {
    if errors.is_empty() {
        return "verification failed without structured errors".to_string();
    }

    errors.join("\n")
}

struct SchedulerPermit<'a> {
    _permit: OwnedSemaphorePermit,
    scheduler: &'a Scheduler,
}

impl<'a> SchedulerPermit<'a> {
    fn new(permit: OwnedSemaphorePermit, scheduler: &'a Scheduler) -> Self {
        Self {
            _permit: permit,
            scheduler,
        }
    }
}

impl Drop for SchedulerPermit<'_> {
    fn drop(&mut self) {
        self.scheduler.release();
    }
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
    use crate::api::{ApiUsage, PLANNER_RESPONSE_SCHEMA_NAME};
    use crate::failure_parser::{CargoCommandKind, FailureStage};
    use serde_json::Value;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    struct MockPlannerApi {
        response: Value,
        usage: ApiUsage,
    }

    fn candidate_outcome(
        agent_id: &str,
        source: CandidateSource,
        retry_attempts: usize,
        total_cost: f64,
        state_hash: Option<&str>,
        change_footprint: ChangeFootprint,
    ) -> CandidateOutcome {
        CandidateOutcome {
            agent_id: agent_id.to_string(),
            log_id: 1,
            sandbox_root: PathBuf::from("/tmp/candidate"),
            source,
            candidate: Some("candidate".to_string()),
            retry_attempts,
            total_tokens: 0,
            total_cost,
            verification_errors: Vec::new(),
            state_hash: state_hash.map(ToString::to_string),
            change_footprint,
            failure_stage: None,
            reason: None,
        }
    }

    impl crate::api::Mercury2Api for MockPlannerApi {
        async fn chat(
            &self,
            _system: &str,
            _user: &str,
            _max_tokens: u32,
        ) -> Result<(String, ApiUsage), ApiError> {
            unreachable!("planner tests only use strict schema chat")
        }

        async fn chat_json(
            &self,
            _system: &str,
            _user: &str,
            _max_tokens: u32,
        ) -> Result<(ThermalAssessment, ApiUsage), ApiError> {
            unreachable!("planner tests only use chat")
        }

        async fn chat_json_schema_value(
            &self,
            _system: &str,
            _user: &str,
            _max_tokens: u32,
            _schema_name: &str,
            _schema: Value,
        ) -> Result<(Value, ApiUsage), ApiError> {
            Ok((self.response.clone(), self.usage))
        }
    }

    #[tokio::test]
    async fn test_planner_sets_estimated_cost_from_usage() {
        let api = MockPlannerApi {
            response: serde_json::json!({
                "schema_version": PLANNER_RESPONSE_SCHEMA_NAME,
                "steps": [{
                    "file_path": "src/main.rs",
                    "instruction": "Do thing",
                    "priority": 0.9,
                    "estimated_tokens": 150
                }],
                "assessments": []
            }),
            usage: ApiUsage {
                tokens_used: 321,
                cost_usd: 0.0123,
            },
        };
        let planner = Planner::new(api, "constitution".to_string());

        let (plan, _assessments) = planner.plan("goal", "repo").await.unwrap();

        assert!(plan.estimated_cost > 0.0);
        assert_eq!(plan.estimated_cost, 0.0123);
        assert_eq!(plan.estimated_tokens, Some(321));
    }

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
            command_results: vec![],
            errors: vec![],
        };
        assert!(ok.is_ok());

        let fail = VerifyResult {
            parse_ok: true,
            test_ok: false,
            lint_ok: true,
            critique: None,
            command_results: vec![],
            errors: vec!["test failed".to_string()],
        };
        assert!(!fail.is_ok());
    }

    #[test]
    fn test_run_command_rejects_shell_composition() {
        let verifier = Verifier::new(
            VerifyConfig {
                parse_before_write: false,
                test_after_write: false,
                lint_after_write: false,
                mercury2_critique_on_failure: false,
                test_command: "true".to_string(),
                lint_command: "true".to_string(),
            },
            None::<MockPlannerApi>,
        );
        let temp = tempfile::tempdir().unwrap();

        let err = match verifier.run_command("printf '%s' 'hello shell' | cat", temp.path()) {
            Ok(output) => panic!(
                "expected shell-composition rejection, got success={} command={}",
                output.success, output.command
            ),
            Err(err) => err,
        };

        assert!(
            err.to_string().contains("command not allowlisted"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn test_verify_captures_structured_cargo_failures() {
        let temp = tempfile::tempdir().unwrap();
        let bin_dir = temp.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();

        let cargo_script = bin_dir.join("cargo");
        fs::write(
            &cargo_script,
            concat!(
                "#!/bin/sh\n",
                "printf 'running 1 test\\n'\n",
                "printf 'test tests::broken ... FAILED\\n'\n",
                "printf ' --> src/lib.rs:7:9\\n' >&2\n",
                "printf 'assertion `left == right` failed\\n' >&2\n",
                "exit 1\n"
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&cargo_script).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&cargo_script, permissions).unwrap();

        let existing_path = std::env::var("PATH").unwrap_or_default();
        let combined_path = format!("{}:{existing_path}", bin_dir.display());
        let test_command = format!("PATH={combined_path} cargo test");
        let verifier = Verifier::new(
            VerifyConfig {
                parse_before_write: true,
                test_after_write: true,
                lint_after_write: false,
                mercury2_critique_on_failure: false,
                test_command,
                lint_command: "true".to_string(),
            },
            None::<MockPlannerApi>,
        );

        let verify = verifier
            .verify(
                &temp.path().join("src/lib.rs"),
                "pub fn ok() {}\n",
                temp.path(),
            )
            .await
            .unwrap();

        assert!(!verify.test_ok);
        assert_eq!(verify.command_results.len(), 1);

        let command = &verify.command_results[0];
        assert!(!command.success);
        let parsed = command.parsed_failure.as_ref().unwrap();
        assert_eq!(parsed.command, CargoCommandKind::Test);
        assert_eq!(parsed.stage, FailureStage::Test);
        assert_eq!(parsed.failures.len(), 1);
        assert_eq!(parsed.failures[0].error_class, "test.assertion");
        assert_eq!(
            parsed.failures[0].target.file_path.as_deref(),
            Some("src/lib.rs")
        );
        assert_eq!(parsed.failures[0].target.line, Some(7));
        assert_eq!(parsed.failures[0].target.column, Some(9));
        assert_eq!(
            parsed.failures[0].target.symbol.as_deref(),
            Some("tests::broken")
        );
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

    #[test]
    fn test_change_footprint_prefers_localized_edit_over_full_rewrite() {
        let baseline = "fn main() {\n    alpha();\n    beta();\n    gamma();\n}\n";
        let localized = "fn main() {\n    alpha();\n    beta_fixed();\n    gamma();\n}\n";
        let rewrite = "use std::process::ExitCode;\n\nfn main() -> ExitCode {\n    beta_fixed();\n    ExitCode::SUCCESS\n}\n";

        assert!(change_footprint(baseline, localized) < change_footprint(baseline, rewrite));
    }

    #[test]
    fn test_rank_candidate_outcomes_prefers_lower_churn_before_agent_id() {
        let mut candidates = vec![
            candidate_outcome(
                "agent-z",
                CandidateSource::ApplyEdit,
                0,
                0.01,
                Some("hash-z"),
                ChangeFootprint {
                    touched_lines: 8,
                    byte_delta: 12,
                },
            ),
            candidate_outcome(
                "agent-a",
                CandidateSource::ApplyEdit,
                0,
                0.01,
                Some("hash-a"),
                ChangeFootprint {
                    touched_lines: 2,
                    byte_delta: 3,
                },
            ),
        ];

        rank_candidate_outcomes(&mut candidates);

        assert_eq!(candidates[0].agent_id, "agent-a");
        assert_eq!(candidates[1].agent_id, "agent-z");
    }

    #[test]
    fn test_rank_candidate_outcomes_prefers_apply_edit_lineage_over_exploration() {
        let mut candidates = vec![
            candidate_outcome(
                "agent-explore",
                CandidateSource::ExploratoryNextEdit,
                0,
                0.001,
                Some("explore-hash"),
                ChangeFootprint {
                    touched_lines: 1,
                    byte_delta: 1,
                },
            ),
            candidate_outcome(
                "agent-apply",
                CandidateSource::ApplyEdit,
                0,
                0.01,
                Some("apply-hash"),
                ChangeFootprint {
                    touched_lines: 3,
                    byte_delta: 4,
                },
            ),
        ];

        rank_candidate_outcomes(&mut candidates);

        assert_eq!(candidates[0].agent_id, "agent-apply");
        assert_eq!(candidates[1].agent_id, "agent-explore");
    }

    #[test]
    fn test_unsafe_candidate_reason_rejects_blank_rewrite() {
        assert_eq!(
            unsafe_candidate_reason("fn keep() {}\n", ""),
            Some("blank rewrite would erase a non-empty file")
        );
        assert_eq!(unsafe_candidate_reason("", ""), None);
    }

    #[test]
    fn test_split_duplicate_state_candidates_keeps_best_ranked_output_once() {
        let mut candidates = vec![
            candidate_outcome(
                "agent-duplicate-expensive",
                CandidateSource::ApplyEdit,
                0,
                0.05,
                Some("same-hash"),
                ChangeFootprint {
                    touched_lines: 3,
                    byte_delta: 4,
                },
            ),
            candidate_outcome(
                "agent-unique",
                CandidateSource::ApplyEdit,
                0,
                0.03,
                Some("unique-hash"),
                ChangeFootprint {
                    touched_lines: 2,
                    byte_delta: 2,
                },
            ),
            candidate_outcome(
                "agent-duplicate-cheap",
                CandidateSource::ApplyEdit,
                0,
                0.01,
                Some("same-hash"),
                ChangeFootprint {
                    touched_lines: 3,
                    byte_delta: 4,
                },
            ),
        ];

        rank_candidate_outcomes(&mut candidates);
        let (unique, duplicates) = split_duplicate_state_candidates(candidates);

        assert_eq!(unique.len(), 2);
        assert_eq!(duplicates.len(), 1);
        assert_eq!(duplicates[0].agent_id, "agent-duplicate-expensive");
        assert!(unique
            .iter()
            .any(|candidate| candidate.agent_id == "agent-duplicate-cheap"));
        assert!(unique
            .iter()
            .any(|candidate| candidate.agent_id == "agent-unique"));
    }
}
