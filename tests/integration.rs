//! Integration tests for Mercury CLI.

// ---------------------------------------------------------------------------
// Thermal merge property tests
// ---------------------------------------------------------------------------

/// Log-Sum-Exp merge (duplicated here for testing without importing private module).
fn thermal_merge(scores: &[f64], temperature: f64) -> f64 {
    if scores.is_empty() {
        return 0.0;
    }
    let max_score = scores.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let sum_exp: f64 = scores
        .iter()
        .map(|s| ((s - max_score) / temperature).exp())
        .sum();
    max_score + temperature * sum_exp.ln()
}

/// Exponential decay.
fn apply_decay(score: f64, elapsed_seconds: f64, half_life: f64) -> f64 {
    score * (0.5_f64).powf(elapsed_seconds / half_life)
}

#[test]
fn test_merge_commutativity() {
    let t = 0.5;
    let a = thermal_merge(&[0.3, 0.7, 0.5], t);
    let b = thermal_merge(&[0.7, 0.5, 0.3], t);
    let c = thermal_merge(&[0.5, 0.3, 0.7], t);
    assert!((a - b).abs() < 1e-10, "merge should be commutative");
    assert!((a - c).abs() < 1e-10, "merge should be commutative");
}

#[test]
fn test_merge_monotonicity() {
    let t = 0.5;
    let two = thermal_merge(&[0.3, 0.7], t);
    let three = thermal_merge(&[0.3, 0.7, 0.5], t);
    assert!(
        three >= two - 1e-10,
        "adding a score should never decrease the result"
    );
}

#[test]
fn test_merge_approximates_max_at_low_temperature() {
    let scores = vec![0.2, 0.5, 0.9, 0.3];
    let merged = thermal_merge(&scores, 0.01);
    let max = 0.9;
    assert!(
        (merged - max).abs() < 0.1,
        "LSE should approximate max at low temperature: got {merged}, expected ~{max}"
    );
}

#[test]
fn test_merge_empty_scores() {
    assert!((thermal_merge(&[], 1.0) - 0.0).abs() < f64::EPSILON);
}

#[test]
fn test_merge_single_score() {
    let result = thermal_merge(&[0.42], 1.0);
    assert!(
        (result - 0.42).abs() < 1e-10,
        "single score should return itself"
    );
}

// ---------------------------------------------------------------------------
// Decay property tests
// ---------------------------------------------------------------------------

#[test]
fn test_decay_monotonic_decrease() {
    let score = 0.8;
    let half_life = 300.0;
    let d1 = apply_decay(score, 100.0, half_life);
    let d2 = apply_decay(score, 200.0, half_life);
    let d3 = apply_decay(score, 300.0, half_life);
    assert!(d1 > d2, "decay should decrease over time");
    assert!(d2 > d3, "decay should decrease over time");
}

#[test]
fn test_decay_half_life() {
    let score = 1.0;
    let half_life = 300.0;
    let decayed = apply_decay(score, 300.0, half_life);
    assert!(
        (decayed - 0.5).abs() < 1e-10,
        "after one half-life, score should be halved: got {decayed}"
    );
}

#[test]
fn test_decay_approaches_zero() {
    let score = 1.0;
    let half_life = 300.0;
    let decayed = apply_decay(score, 1_000_000.0, half_life);
    assert!(
        decayed < 0.001,
        "after a very long time, score should approach zero: got {decayed}"
    );
}

#[test]
fn test_decay_zero_elapsed() {
    let score = 0.75;
    let decayed = apply_decay(score, 0.0, 300.0);
    assert!(
        (decayed - score).abs() < f64::EPSILON,
        "zero elapsed time should not change score"
    );
}

// ---------------------------------------------------------------------------
// Database tests
// ---------------------------------------------------------------------------

#[test]
fn test_db_init_creates_tables() {
    let db = mercury_cli::db::ThermalDb::in_memory().expect("should create db");
    let scores = db.get_all_scores().expect("should query");
    assert!(scores.is_empty());
    let aggs = db.get_all_aggregates().expect("should query");
    assert!(aggs.is_empty());
}

#[test]
fn test_thermal_crud_roundtrip() {
    let db = mercury_cli::db::ThermalDb::in_memory().unwrap();

    // Insert scores
    db.upsert_thermal_score("src/main.rs", 1, 100, 0.8, "complexity", "plan", "agent-1")
        .unwrap();
    db.upsert_thermal_score("src/main.rs", 1, 100, 0.6, "risk", "plan", "agent-1")
        .unwrap();
    db.upsert_thermal_score("src/lib.rs", 1, 50, 0.3, "complexity", "plan", "agent-2")
        .unwrap();

    // Query
    let scores = db.get_scores_for_file("src/main.rs").unwrap();
    assert_eq!(scores.len(), 2);

    let all = db.get_all_scores().unwrap();
    assert_eq!(all.len(), 3);
}

#[test]
fn test_planning_persists_all_four_metric_types() {
    use std::collections::BTreeSet;

    let db = mercury_cli::db::ThermalDb::in_memory().unwrap();
    let file_path = "src/main.rs";

    db.upsert_thermal_score(file_path, 1, 1000, 0.81, "complexity", "plan", "planner")
        .unwrap();
    db.upsert_thermal_score(file_path, 1, 1000, 0.64, "dependency", "plan", "planner")
        .unwrap();
    db.upsert_thermal_score(file_path, 1, 1000, 0.72, "risk", "plan", "planner")
        .unwrap();
    db.upsert_thermal_score(file_path, 1, 1000, 0.49, "churn", "plan", "planner")
        .unwrap();

    let scheduler = mercury_cli::engine::Scheduler::new(Default::default());
    scheduler.run_merge_cycle(&db, 1.0).unwrap();

    let score_types: BTreeSet<String> = db
        .get_scores_for_file(file_path)
        .unwrap()
        .into_iter()
        .map(|score| score.score_type)
        .collect();

    assert_eq!(
        score_types,
        BTreeSet::from([
            "churn".to_string(),
            "complexity".to_string(),
            "dependency".to_string(),
            "risk".to_string(),
        ])
    );

    let aggregate = db.get_aggregate(file_path).unwrap();
    assert!(
        aggregate.is_some(),
        "merge cycle should aggregate four-factor scores"
    );
}

#[test]
fn test_cool_zone_locking() {
    let db = mercury_cli::db::ThermalDb::in_memory().unwrap();

    // Lock a region
    db.insert_cool_lock("src/lib.rs", 1, 100, "hash123", "agent-1")
        .unwrap();

    // Check overlap
    assert!(db.is_locked("src/lib.rs", 10, 20).unwrap());
    assert!(db.is_locked("src/lib.rs", 1, 100).unwrap());
    assert!(!db.is_locked("src/other.rs", 1, 10).unwrap());

    // File-level check
    assert!(db.is_file_locked("src/lib.rs").unwrap());
    assert!(!db.is_file_locked("src/other.rs").unwrap());

    // Remove lock
    db.remove_cool_lock("src/lib.rs", 1, 100).unwrap();
    assert!(!db.is_locked("src/lib.rs", 10, 20).unwrap());
}

#[test]
fn test_swarm_state_lifecycle() {
    let db = mercury_cli::db::ThermalDb::in_memory().unwrap();

    let id = db.init_swarm().unwrap();
    db.add_cost(id, 1000, 0.05).unwrap();
    db.add_cost(id, 500, 0.02).unwrap();

    let state = db.get_swarm_state().unwrap().unwrap();
    assert_eq!(state.total_tokens_used, 1500);
    assert!((state.total_cost_usd - 0.07).abs() < 1e-10);
}

#[test]
fn test_config_roundtrip() {
    let config_str = r#"
[api]
mercury2_endpoint = "https://api.inceptionlabs.ai/v1/chat/completions"
mercury_edit_endpoint = "https://api.inceptionlabs.ai/v1/edit"
api_key_env = "INCEPTION_API_KEY"

[scheduler]
max_concurrency = 20
max_cost_per_command = 0.50
max_agents_per_command = 100
retry_limit = 3
backoff_base_ms = 500

[thermal]
decay_half_life_seconds = 300
aggregation_method = "log_sum_exp"
rescan_on_git_pull = true
hot_threshold = 0.7
cool_threshold = 0.3
lock_cool_zones = true

[annealing]
enable_global_momentum = true
initial_temperature = 1.0
cooling_rate = 0.02
min_modification_threshold = 0.1

[verification]
parse_before_write = true
test_after_write = true
lint_after_write = true
mercury2_critique_on_failure = true
test_command = "cargo test"
lint_command = "cargo clippy"

[constitutional]
style_guide = ""
architecture_rules = ""
naming_conventions = ""
"#;
    let config: toml::Value = toml::from_str(config_str).unwrap();
    assert!(config.get("api").is_some());
    assert!(config.get("scheduler").is_some());
    assert!(config.get("thermal").is_some());
    assert!(config.get("annealing").is_some());
    assert!(config.get("verification").is_some());
    assert!(config.get("constitutional").is_some());
}

// ---------------------------------------------------------------------------
// Budget enforcement tests
// ---------------------------------------------------------------------------

#[test]
fn test_budget_enforcement() {
    use mercury_cli::engine::{Scheduler, SchedulerConfig};

    let scheduler = Scheduler::new(SchedulerConfig {
        max_cost_per_command: 0.10,
        ..Default::default()
    });

    // Should succeed
    assert!(scheduler.record_cost(0.04).is_ok());
    assert!(scheduler.record_cost(0.04).is_ok());
    assert!(scheduler.has_budget());

    // Should fail — exceeds budget
    let result = scheduler.record_cost(0.05);
    assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// Fix orchestration verification gate coverage
// ---------------------------------------------------------------------------

#[tokio::test]
#[allow(clippy::manual_async_fn)]
async fn test_fix_executes_verification_gate() {
    use mercury_cli::api::{ApiError, ApiUsage, CompletePayload, EditPayload, NextEditPayload};
    use mercury_cli::engine::{
        execute_plan_steps, ExecutionPlan, Patcher, PlanStep, Scheduler, SchedulerConfig, Verifier,
        VerifyConfig,
    };
    use mercury_cli::{api::Mercury2Api, api::MercuryEditApi};
    use std::future::Future;
    use std::path::Path;

    struct TestEditApi;

    impl MercuryEditApi for TestEditApi {
        fn apply(
            &self,
            payload: &EditPayload,
        ) -> impl Future<Output = Result<(String, ApiUsage), ApiError>> + Send {
            let out = if payload.update_snippet.contains("PASS") {
                payload.update_snippet.clone()
            } else {
                payload.original_code.clone() + payload.update_snippet.as_str()
            };
            async move { Ok((out, ApiUsage::default())) }
        }

        fn complete(
            &self,
            _payload: &CompletePayload,
        ) -> impl Future<Output = Result<(String, ApiUsage), ApiError>> + Send {
            async { Ok((String::new(), ApiUsage::default())) }
        }

        fn next_edit(
            &self,
            _payload: &NextEditPayload,
        ) -> impl Future<Output = Result<(String, ApiUsage), ApiError>> + Send {
            async { Ok((String::new(), ApiUsage::default())) }
        }

        fn next_edit_with_path(
            &self,
            _current_file_path: &str,
            payload: &NextEditPayload,
        ) -> impl Future<Output = Result<(String, ApiUsage), ApiError>> + Send {
            self.next_edit(payload)
        }
    }

    struct NoopMercury2;

    impl Mercury2Api for NoopMercury2 {
        fn chat(
            &self,
            _system: &str,
            _user: &str,
            _max_tokens: u32,
        ) -> impl Future<Output = Result<(String, ApiUsage), ApiError>> + Send {
            async { Err(ApiError::MaxRetries(0)) }
        }

        fn chat_json(
            &self,
            _system: &str,
            _user: &str,
            _max_tokens: u32,
        ) -> impl Future<Output = Result<(mercury_cli::api::ThermalAssessment, ApiUsage), ApiError>> + Send
        {
            async { Err(ApiError::MaxRetries(0)) }
        }

        fn chat_json_schema_value(
            &self,
            _system: &str,
            _user: &str,
            _max_tokens: u32,
            _schema_name: &str,
            _schema: serde_json::Value,
        ) -> impl Future<Output = Result<(serde_json::Value, ApiUsage), ApiError>> + Send {
            async { Err(ApiError::MaxRetries(0)) }
        }
    }

    let temp = tempfile::tempdir().unwrap();
    let project_root = temp.path();
    let target_file = project_root.join("sample.rs");
    std::fs::write(
        &target_file,
        "fn hello() {}
",
    )
    .unwrap();

    let plan = ExecutionPlan {
        steps: vec![
            PlanStep {
                file_path: "sample.rs".to_string(),
                instruction: "fn bad( {".to_string(),
                priority: 1.0,
                estimated_tokens: 64,
            },
            PlanStep {
                file_path: "sample.rs".to_string(),
                instruction: "
fn hello() {} // PASS"
                    .to_string(),
                priority: 0.5,
                estimated_tokens: 64,
            },
        ],
        constitutional_prompt: String::new(),
        estimated_cost: 0.0,
        estimated_tokens: None,
    };

    let db = mercury_cli::db::ThermalDb::in_memory().unwrap();
    let scheduler = Scheduler::new(SchedulerConfig {
        max_cost_per_command: 1.0,
        ..Default::default()
    });

    let patcher = Patcher::new(TestEditApi);
    let verifier = Verifier::new(
        VerifyConfig {
            parse_before_write: true,
            test_after_write: false,
            lint_after_write: false,
            mercury2_critique_on_failure: false,
            test_command: "true".into(),
            lint_command: "true".into(),
        },
        None::<NoopMercury2>,
    );

    let summary = execute_plan_steps(
        &plan,
        &patcher,
        &verifier,
        &scheduler,
        &db,
        Path::new(project_root),
    )
    .await
    .unwrap();

    assert_eq!(summary.accepted, 1);
    assert!(summary.rejected > 0);
    assert!(summary.verification_failures > 0);

    let logs = db.get_agent_logs().unwrap();
    assert_eq!(logs.len(), summary.accepted + summary.rejected);
    assert!(logs.iter().any(|log| log.status == "failed"));
    assert_eq!(
        logs.iter().filter(|log| log.status == "success").count(),
        summary.accepted
    );
}
