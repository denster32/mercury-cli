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
api_key_env = "MERCURY_API_KEY"

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
