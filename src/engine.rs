//! Engine — the four-layer intelligence pipeline.
//!
//! - **Planner**: goal + repo map → Mercury 2 → execution plan + thermal scores
//! - **Patcher**: file slice + instruction → Mercury Edit → patched code
//! - **Verifier**: tree-sitter parse → test → lint → optional Mercury 2 critique
//! - **Scheduler**: tokio concurrency pool, budget tracking, thermal merge cycles

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::{collections::HashMap, path::PathBuf};

use chrono::Utc;
use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};

use crate::api::{
    planner_response_json_schema_v1, ApiError, ApiUsage, Mercury2Api, MercuryEditApi,
    ThermalAssessment, PLANNER_RESPONSE_SCHEMA_NAME, THERMAL_ANALYSIS_PROMPT,
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

        if let Some(parent) = file_path.parent() {
            std::fs::create_dir_all(parent)?;
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
            errors: Vec::new(),
        };

        if self.config.parse_before_write {
            for (relative_path, content) in accepted_states {
                match self.check_parse(content) {
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
                Ok(output) if output.success => {}
                Ok(output) => {
                    result.test_ok = false;
                    result
                        .errors
                        .push(format!("tests failed in workspace: {}", output.stderr));
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
                Ok(output) if output.success => {}
                Ok(output) => {
                    result.lint_ok = false;
                    result
                        .errors
                        .push(format!("lint failed in workspace: {}", output.stderr));
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
#[derive(Debug, Clone, Serialize)]
pub struct VerifyResult {
    pub parse_ok: bool,
    pub test_ok: bool,
    pub lint_ok: bool,
    pub critique: Option<String>,
    pub errors: Vec<String>,
}

/// Aggregate execution results for `fix` step orchestration.
#[derive(Debug, Clone, Default, Serialize)]
pub struct StepExecutionSummary {
    pub accepted: usize,
    pub rejected: usize,
    pub verification_failures: usize,
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
    let accepted_states = Arc::new(Mutex::new(HashMap::<PathBuf, String>::new()));
    let mut grouped_steps: BTreeMap<PathBuf, Vec<IndexedPlanStep>> = BTreeMap::new();

    for (index, step) in plan.steps.iter().cloned().enumerate() {
        grouped_steps
            .entry(PathBuf::from(&step.file_path))
            .or_default()
            .push(IndexedPlanStep { index, step });
    }

    let max_concurrency = scheduler.config().max_concurrency.max(1);
    let partials = stream::iter(grouped_steps.into_iter().map(|(relative_path, steps)| {
        let accepted_states = Arc::clone(&accepted_states);
        let run_root = run_root.clone();
        async move {
            execute_file_group(
                relative_path,
                steps,
                patcher,
                verifier,
                scheduler,
                db,
                project_root,
                &run_root,
                accepted_states,
            )
            .await
        }
    }))
    .buffer_unordered(max_concurrency)
    .collect::<Vec<_>>()
    .await;

    let mut summary = StepExecutionSummary {
        run_root: Some(run_root.clone()),
        ..StepExecutionSummary::default()
    };

    for partial in partials {
        summary.merge(partial?);
    }

    let accepted_snapshot = accepted_states.lock().await.clone();
    if accepted_snapshot.is_empty() {
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
        apply_changes_atomically(project_root, &accepted_snapshot)?;
        summary.applied = true;
    } else {
        summary.verification_failures += 1;
    }

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

async fn execute_file_group<E: MercuryEditApi, A: Mercury2Api>(
    relative_path: PathBuf,
    steps: Vec<IndexedPlanStep>,
    patcher: &Patcher<E>,
    verifier: &Verifier<A>,
    scheduler: &Scheduler,
    db: &ThermalDb,
    project_root: &Path,
    run_root: &Path,
    accepted_states: Arc<Mutex<HashMap<PathBuf, String>>>,
) -> Result<StepExecutionSummary, EngineError> {
    let _permit = SchedulerPermit::new(scheduler.acquire().await?, scheduler);
    let mut summary = StepExecutionSummary::default();
    let mut latest_state =
        read_latest_state(project_root, &accepted_states, &relative_path).await?;

    for indexed_step in steps {
        let agent_id = format!("fix-step-{}", indexed_step.index + 1);
        let log_id = db.log_agent_spawn(&agent_id, "fix", &indexed_step.step.file_path)?;
        db.update_agent_status(log_id, "running", 0, 0.0, None)?;

        let (candidate, usage) = match patcher
            .patch(&latest_state, &indexed_step.step.instruction)
            .await
        {
            Ok(value) => value,
            Err(err) => {
                summary.rejected += 1;
                let reason = format!("patch generation failed: {err}");
                let metadata = serde_json::json!({"outcome":"rejected", "reason": reason});
                db.update_agent_status(log_id, "failed", 0, 0.0, Some(&metadata.to_string()))?;
                continue;
            }
        };

        if let Err(err) = scheduler.record_cost(usage.cost_usd) {
            summary.rejected += 1;
            let reason = format!("budget rejected step: {err}");
            let metadata = serde_json::json!({"outcome":"rejected", "reason": reason});
            db.update_agent_status(
                log_id,
                "failed",
                usage.tokens_used,
                usage.cost_usd,
                Some(&metadata.to_string()),
            )?;
            continue;
        }

        let candidate_root = run_root.join("candidates").join(format!(
            "step-{:04}-{}",
            indexed_step.index + 1,
            sanitize_path_component(&indexed_step.step.file_path)
        ));
        let accepted_snapshot = accepted_states.lock().await.clone();
        prepare_workspace(project_root, &candidate_root, &accepted_snapshot)?;

        let verify = verifier
            .verify(
                &candidate_root.join(&indexed_step.step.file_path),
                &candidate,
                &candidate_root,
            )
            .await?;

        if verify.is_ok() {
            latest_state = candidate.clone();
            accepted_states
                .lock()
                .await
                .insert(relative_path.clone(), candidate);
            summary.accepted += 1;
            let metadata = serde_json::json!({
                "outcome":"accepted",
                "sandbox_root": candidate_root.display().to_string(),
            });
            db.update_agent_status(
                log_id,
                "success",
                usage.tokens_used,
                usage.cost_usd,
                Some(&metadata.to_string()),
            )?;
        } else {
            summary.rejected += 1;
            summary.verification_failures += 1;
            let metadata = serde_json::json!({
                "outcome":"rejected",
                "reason": verify.errors.join("; "),
                "sandbox_root": candidate_root.display().to_string(),
            });
            db.update_agent_status(
                log_id,
                "failed",
                usage.tokens_used,
                usage.cost_usd,
                Some(&metadata.to_string()),
            )?;
        }
    }

    Ok(summary)
}

async fn read_latest_state(
    project_root: &Path,
    accepted_states: &Mutex<HashMap<PathBuf, String>>,
    relative_path: &Path,
) -> Result<String, EngineError> {
    if let Some(existing) = accepted_states.lock().await.get(relative_path).cloned() {
        return Ok(existing);
    }

    let full_path = project_root.join(relative_path);
    match std::fs::read_to_string(&full_path) {
        Ok(content) => Ok(content),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(err) => Err(EngineError::Io(err)),
    }
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
    if workspace_root.exists() {
        std::fs::remove_dir_all(workspace_root)?;
    }
    std::fs::create_dir_all(workspace_root)?;
    copy_project_tree(project_root, workspace_root, project_root)?;

    for (relative_path, content) in accepted_states {
        let destination = workspace_root.join(relative_path);
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(destination, content)?;
    }

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

fn copy_project_tree(source: &Path, destination: &Path, root: &Path) -> Result<(), EngineError> {
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let relative = source_path
            .strip_prefix(root)
            .unwrap_or(source_path.as_path());

        if should_skip_copy(relative) {
            continue;
        }

        let destination_path = destination.join(relative);
        let metadata = entry.metadata()?;

        if metadata.is_dir() {
            std::fs::create_dir_all(&destination_path)?;
            copy_project_tree(&source_path, destination, root)?;
        } else if metadata.is_file() {
            if let Some(parent) = destination_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&source_path, &destination_path)?;
        }
    }

    Ok(())
}

fn should_skip_copy(relative: &Path) -> bool {
    let relative = relative.to_string_lossy();
    relative == ".git"
        || relative.starts_with(".git/")
        || relative == "target"
        || relative.starts_with("target/")
        || relative == ".mercury/worktrees"
        || relative.starts_with(".mercury/worktrees/")
        || relative == ".mercury/runs"
        || relative.starts_with(".mercury/runs/")
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
    success: bool,
    #[allow(dead_code)]
    stdout: String,
    stderr: String,
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
    use serde_json::Value;

    struct MockPlannerApi {
        response: Value,
        usage: ApiUsage,
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
