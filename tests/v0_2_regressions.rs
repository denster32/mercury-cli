use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use mercury_cli::api::{ApiError, ApiUsage, CompletePayload, EditPayload, NextEditPayload};
use mercury_cli::engine::{
    execute_plan_steps, ExecutionPlan, Patcher, PlanStep, Scheduler, SchedulerConfig, Verifier,
    VerifyConfig,
};
use mercury_cli::{api::Mercury2Api, api::MercuryEditApi};

struct ScriptedEditApi;

impl MercuryEditApi for ScriptedEditApi {
    async fn apply(&self, payload: &EditPayload) -> Result<(String, ApiUsage), ApiError> {
        let candidate = if let Some(rest) = payload.update_snippet.strip_prefix("REPLACE:") {
            rest.to_string()
        } else if let Some(rest) = payload.update_snippet.strip_prefix("APPEND:") {
            format!("{}{}", payload.original_code, rest)
        } else {
            payload.update_snippet.clone()
        };

        Ok((
            candidate,
            ApiUsage {
                tokens_used: 64,
                cost_usd: 0.001,
            },
        ))
    }

    async fn complete(&self, _payload: &CompletePayload) -> Result<(String, ApiUsage), ApiError> {
        Ok((String::new(), ApiUsage::default()))
    }

    async fn next_edit(&self, _payload: &NextEditPayload) -> Result<(String, ApiUsage), ApiError> {
        Ok((String::new(), ApiUsage::default()))
    }

    async fn next_edit_with_path(
        &self,
        _current_file_path: &str,
        payload: &NextEditPayload,
    ) -> Result<(String, ApiUsage), ApiError> {
        self.next_edit(payload).await
    }
}

struct RetryingEditApi {
    retry_candidate: String,
    retry_calls: Arc<AtomicUsize>,
    retry_histories: Arc<Mutex<Vec<String>>>,
}

impl MercuryEditApi for RetryingEditApi {
    async fn apply(&self, payload: &EditPayload) -> Result<(String, ApiUsage), ApiError> {
        ScriptedEditApi.apply(payload).await
    }

    async fn complete(&self, payload: &CompletePayload) -> Result<(String, ApiUsage), ApiError> {
        ScriptedEditApi.complete(payload).await
    }

    async fn next_edit(&self, payload: &NextEditPayload) -> Result<(String, ApiUsage), ApiError> {
        self.next_edit_with_path("", payload).await
    }

    async fn next_edit_with_path(
        &self,
        _current_file_path: &str,
        payload: &NextEditPayload,
    ) -> Result<(String, ApiUsage), ApiError> {
        self.retry_calls.fetch_add(1, Ordering::Relaxed);
        self.retry_histories
            .lock()
            .unwrap()
            .push(payload.edit_history.clone());
        Ok((
            self.retry_candidate.clone(),
            ApiUsage {
                tokens_used: 32,
                cost_usd: 0.0005,
            },
        ))
    }
}

struct NoopMercury2;

impl Mercury2Api for NoopMercury2 {
    async fn chat(
        &self,
        _system: &str,
        _user: &str,
        _max_tokens: u32,
    ) -> Result<(String, ApiUsage), ApiError> {
        Err(ApiError::MaxRetries(0))
    }

    async fn chat_json(
        &self,
        _system: &str,
        _user: &str,
        _max_tokens: u32,
    ) -> Result<(mercury_cli::api::ThermalAssessment, ApiUsage), ApiError> {
        Err(ApiError::MaxRetries(0))
    }

    async fn chat_json_schema_value(
        &self,
        _system: &str,
        _user: &str,
        _max_tokens: u32,
        _schema_name: &str,
        _schema: serde_json::Value,
    ) -> Result<(serde_json::Value, ApiUsage), ApiError> {
        Err(ApiError::MaxRetries(0))
    }
}

#[derive(Clone)]
struct CritiqueMercury2 {
    critique: String,
    calls: Arc<AtomicUsize>,
}

impl Mercury2Api for CritiqueMercury2 {
    async fn chat(
        &self,
        _system: &str,
        _user: &str,
        _max_tokens: u32,
    ) -> Result<(String, ApiUsage), ApiError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok((
            self.critique.clone(),
            ApiUsage {
                tokens_used: 48,
                cost_usd: 0.0007,
            },
        ))
    }

    async fn chat_json(
        &self,
        _system: &str,
        _user: &str,
        _max_tokens: u32,
    ) -> Result<(mercury_cli::api::ThermalAssessment, ApiUsage), ApiError> {
        Err(ApiError::MaxRetries(0))
    }

    async fn chat_json_schema_value(
        &self,
        _system: &str,
        _user: &str,
        _max_tokens: u32,
        _schema_name: &str,
        _schema: serde_json::Value,
    ) -> Result<(serde_json::Value, ApiUsage), ApiError> {
        Err(ApiError::MaxRetries(0))
    }
}

fn make_scheduler() -> Scheduler {
    Scheduler::new(SchedulerConfig {
        max_concurrency: 1,
        max_cost_per_command: 1.0,
        ..Default::default()
    })
}

fn make_verifier() -> Verifier<NoopMercury2> {
    Verifier::new(
        VerifyConfig {
            parse_before_write: true,
            test_after_write: false,
            lint_after_write: false,
            mercury2_critique_on_failure: false,
            test_command: "true".into(),
            lint_command: "true".into(),
        },
        None::<NoopMercury2>,
    )
}

fn write_sample_file(project_root: &Path, contents: &str) -> std::path::PathBuf {
    let target_file = project_root.join("sample.rs");
    fs::write(&target_file, contents).expect("sample file should be written");
    target_file
}

fn make_retry_verifier(api: CritiqueMercury2) -> Verifier<CritiqueMercury2> {
    Verifier::new(
        VerifyConfig {
            parse_before_write: true,
            test_after_write: false,
            lint_after_write: false,
            mercury2_critique_on_failure: true,
            test_command: "true".into(),
            lint_command: "true".into(),
        },
        Some(api),
    )
}

#[tokio::test]
async fn rejected_candidate_restores_original_file_when_no_candidate_is_accepted() {
    let temp = tempfile::tempdir().unwrap();
    let project_root = temp.path();
    let target_file = write_sample_file(project_root, "fn hello() {}\n");

    let plan = ExecutionPlan {
        steps: vec![PlanStep {
            file_path: "sample.rs".to_string(),
            instruction: "REPLACE:fn broken( {\n".to_string(),
            priority: 1.0,
            estimated_tokens: 64,
        }],
        constitutional_prompt: String::new(),
        estimated_cost: 0.0,
        estimated_tokens: None,
    };

    let db = mercury_cli::db::ThermalDb::in_memory().unwrap();
    let patcher = Patcher::new(ScriptedEditApi);
    let verifier = make_verifier();
    let scheduler = make_scheduler();
    let summary = execute_plan_steps(&plan, &patcher, &verifier, &scheduler, &db, project_root)
        .await
        .unwrap();

    assert_eq!(summary.accepted, 0);
    assert!(summary.rejected >= 1);
    assert_eq!(summary.verification_failures, 1);
    assert_eq!(fs::read_to_string(target_file).unwrap(), "fn hello() {}\n");

    let logs = db.get_agent_logs().unwrap();
    assert_eq!(logs.len(), summary.rejected);
    assert!(logs.iter().all(|log| log.status == "failed"));
}

#[tokio::test]
async fn rejected_candidate_content_does_not_leak_into_final_file_after_later_acceptance() {
    let temp = tempfile::tempdir().unwrap();
    let project_root = temp.path();
    let target_file = write_sample_file(project_root, "fn hello() {}\n");

    let plan = ExecutionPlan {
        steps: vec![
            PlanStep {
                file_path: "sample.rs".to_string(),
                instruction: "REPLACE:fn broken( {\n".to_string(),
                priority: 1.0,
                estimated_tokens: 64,
            },
            PlanStep {
                file_path: "sample.rs".to_string(),
                instruction: "REPLACE:fn repaired() {}\n".to_string(),
                priority: 0.5,
                estimated_tokens: 64,
            },
        ],
        constitutional_prompt: String::new(),
        estimated_cost: 0.0,
        estimated_tokens: None,
    };

    let db = mercury_cli::db::ThermalDb::in_memory().unwrap();
    let patcher = Patcher::new(ScriptedEditApi);
    let verifier = make_verifier();
    let scheduler = make_scheduler();
    let summary = execute_plan_steps(&plan, &patcher, &verifier, &scheduler, &db, project_root)
        .await
        .unwrap();

    let final_content = fs::read_to_string(target_file).unwrap();
    assert_eq!(summary.accepted, 1);
    assert!(summary.rejected >= 1);
    assert_eq!(summary.verification_failures, 1);
    assert_eq!(final_content, "fn repaired() {}\n");
    assert!(!final_content.contains("broken"));
}

#[tokio::test]
async fn same_file_steps_patch_from_latest_accepted_state() {
    let temp = tempfile::tempdir().unwrap();
    let project_root = temp.path();
    let target_file = write_sample_file(project_root, "pub const A: i32 = 1;\n");

    let plan = ExecutionPlan {
        steps: vec![
            PlanStep {
                file_path: "sample.rs".to_string(),
                instruction: "REPLACE:pub const A: i32 = 2;\npub const B: i32 = 3;\n".to_string(),
                priority: 1.0,
                estimated_tokens: 64,
            },
            PlanStep {
                file_path: "sample.rs".to_string(),
                instruction: "APPEND:pub fn total() -> i32 { A + B }\n".to_string(),
                priority: 0.5,
                estimated_tokens: 64,
            },
        ],
        constitutional_prompt: String::new(),
        estimated_cost: 0.0,
        estimated_tokens: None,
    };

    let db = mercury_cli::db::ThermalDb::in_memory().unwrap();
    let patcher = Patcher::new(ScriptedEditApi);
    let verifier = make_verifier();
    let scheduler = make_scheduler();
    let summary = execute_plan_steps(&plan, &patcher, &verifier, &scheduler, &db, project_root)
        .await
        .unwrap();

    assert_eq!(summary.accepted, 2);
    let final_content = fs::read_to_string(target_file).unwrap();
    assert!(final_content.contains("pub const B: i32 = 3;"));
    assert!(final_content.contains("pub fn total() -> i32 { A + B }"));
}

#[tokio::test]
async fn rejected_runs_never_dirty_the_user_worktree() {
    let temp = tempfile::tempdir().unwrap();
    let project_root = temp.path();
    let target_file = write_sample_file(project_root, "pub fn stable() -> i32 { 1 }\n");
    let original = fs::read_to_string(&target_file).unwrap();

    let plan = ExecutionPlan {
        steps: vec![PlanStep {
            file_path: "sample.rs".to_string(),
            instruction: "REPLACE:pub fn broken( {\n".to_string(),
            priority: 1.0,
            estimated_tokens: 64,
        }],
        constitutional_prompt: String::new(),
        estimated_cost: 0.0,
        estimated_tokens: None,
    };

    let db = mercury_cli::db::ThermalDb::in_memory().unwrap();
    let patcher = Patcher::new(ScriptedEditApi);
    let verifier = make_verifier();
    let scheduler = make_scheduler();
    let summary = execute_plan_steps(&plan, &patcher, &verifier, &scheduler, &db, project_root)
        .await
        .unwrap();

    assert_eq!(summary.accepted, 0);
    assert!(summary.rejected >= 1);
    assert!(!summary.applied);
    assert!(!summary.final_bundle_verified);
    assert_eq!(fs::read_to_string(&target_file).unwrap(), original);
    assert!(project_root.join(".mercury").join("worktrees").exists());
    let run_root = summary
        .run_root
        .as_ref()
        .expect("run root should be recorded");
    assert!(run_root.starts_with(project_root.join(".mercury").join("worktrees")));
}

#[tokio::test]
async fn critique_retry_accepts_second_candidate() {
    let temp = tempfile::tempdir().unwrap();
    let project_root = temp.path();
    let target_file = write_sample_file(project_root, "fn hello() {}\n");

    let plan = ExecutionPlan {
        steps: vec![PlanStep {
            file_path: "sample.rs".to_string(),
            instruction: "REPLACE:fn broken( {\n".to_string(),
            priority: 1.0,
            estimated_tokens: 64,
        }],
        constitutional_prompt: String::new(),
        estimated_cost: 0.0,
        estimated_tokens: None,
    };

    let retry_calls = Arc::new(AtomicUsize::new(0));
    let critique_calls = Arc::new(AtomicUsize::new(0));
    let retry_histories = Arc::new(Mutex::new(Vec::new()));

    let patcher = Patcher::new(RetryingEditApi {
        retry_candidate: "fn repaired() {}\n".to_string(),
        retry_calls: retry_calls.clone(),
        retry_histories: retry_histories.clone(),
    });
    let verifier = make_retry_verifier(CritiqueMercury2 {
        critique: "Fix the parse error around the function signature.".to_string(),
        calls: critique_calls.clone(),
    });
    let scheduler = make_scheduler();
    let db = mercury_cli::db::ThermalDb::in_memory().unwrap();

    let summary = execute_plan_steps(&plan, &patcher, &verifier, &scheduler, &db, project_root)
        .await
        .unwrap();

    assert_eq!(summary.accepted, 1);
    assert_eq!(summary.rejected, 0);
    assert_eq!(summary.verification_failures, 0);
    assert_eq!(summary.retry_attempts, 1);
    assert!(summary.final_bundle_verified);
    assert!(summary.applied);
    assert_eq!(
        fs::read_to_string(&target_file).unwrap(),
        "fn repaired() {}\n"
    );
    assert_eq!(retry_calls.load(Ordering::Relaxed), 1);
    assert_eq!(critique_calls.load(Ordering::Relaxed), 1);

    let histories = retry_histories.lock().unwrap();
    assert_eq!(histories.len(), 1);
    assert!(histories[0].contains("original_instruction:"));
    assert!(histories[0].contains("verification_errors:"));
    assert!(histories[0].contains("critique:"));
}

#[tokio::test]
async fn critique_retry_is_bounded_to_one_second_pass() {
    let temp = tempfile::tempdir().unwrap();
    let project_root = temp.path();
    let target_file = write_sample_file(project_root, "fn hello() {}\n");

    let plan = ExecutionPlan {
        steps: vec![PlanStep {
            file_path: "sample.rs".to_string(),
            instruction: "REPLACE:fn broken( {\n".to_string(),
            priority: 1.0,
            estimated_tokens: 64,
        }],
        constitutional_prompt: String::new(),
        estimated_cost: 0.0,
        estimated_tokens: None,
    };

    let retry_calls = Arc::new(AtomicUsize::new(0));
    let critique_calls = Arc::new(AtomicUsize::new(0));

    let patcher = Patcher::new(RetryingEditApi {
        retry_candidate: "fn still_broken( {\n".to_string(),
        retry_calls: retry_calls.clone(),
        retry_histories: Arc::new(Mutex::new(Vec::new())),
    });
    let verifier = make_retry_verifier(CritiqueMercury2 {
        critique: "Only retry once and focus on the syntax error.".to_string(),
        calls: critique_calls.clone(),
    });
    let scheduler = make_scheduler();
    let db = mercury_cli::db::ThermalDb::in_memory().unwrap();

    let summary = execute_plan_steps(&plan, &patcher, &verifier, &scheduler, &db, project_root)
        .await
        .unwrap();

    assert_eq!(summary.accepted, 0);
    assert!(summary.rejected >= 1);
    assert_eq!(summary.verification_failures, 1);
    assert_eq!(summary.retry_attempts, 1);
    assert!(!summary.final_bundle_verified);
    assert!(!summary.applied);
    assert_eq!(fs::read_to_string(&target_file).unwrap(), "fn hello() {}\n");
    assert_eq!(retry_calls.load(Ordering::Relaxed), 1);
    assert_eq!(critique_calls.load(Ordering::Relaxed), 1);
}
