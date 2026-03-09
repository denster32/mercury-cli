use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

fn manifest() -> Value {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let raw = fs::read_to_string(repo_root.join("evals/v0/manifest.json"))
        .expect("eval manifest should exist");
    serde_json::from_str(&raw).expect("eval manifest should be valid json")
}

#[test]
fn eval_manifest_matches_v0_shape() {
    let manifest = manifest();

    assert_eq!(manifest["schema_version"], "mercury-evals-v0");
    assert_eq!(manifest["suite_id"], "rust-v0.2-seeded");
    assert_eq!(manifest["language"], "rust");
    assert_eq!(manifest["version"], 1);
    assert_eq!(
        manifest["artifact_schema_version"],
        "mercury-eval-report-v0"
    );

    let supported_modes = manifest["supported_modes"]
        .as_array()
        .expect("supported_modes must be an array");
    assert!(
        supported_modes.iter().any(|mode| mode == "baseline"),
        "baseline mode must be declared"
    );

    let cases = manifest["cases"]
        .as_array()
        .expect("cases must be an array");
    assert_eq!(
        cases.len(),
        10,
        "v0 corpus should contain 10 seeded failures"
    );

    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let harness_root = repo_root.join("evals/v0");
    let mut ids = BTreeSet::new();

    for case in cases {
        let id = case["id"].as_str().expect("case id must be a string");
        assert!(ids.insert(id.to_string()), "case ids must be unique");

        assert!(
            case["title"].is_string(),
            "title should be present for {id}"
        );
        assert!(
            case["failure_class"].is_string(),
            "failure_class should be present for {id}"
        );

        let failure_stage = case["failure_stage"]
            .as_str()
            .expect("failure_stage must be a string");
        assert!(
            matches!(failure_stage, "parse" | "compile" | "test" | "lint"),
            "unexpected failure_stage for {id}: {failure_stage}"
        );

        let path = case["path"].as_str().expect("case path must be a string");
        assert!(
            !Path::new(path).is_absolute(),
            "case path must be repo-relative for {id}"
        );
        let case_dir = harness_root.join(path);
        assert!(
            case_dir.join("Cargo.toml").exists(),
            "missing Cargo.toml for {id}"
        );
        assert!(
            case_dir.join("src/lib.rs").exists(),
            "missing src/lib.rs for {id}"
        );

        let verifier_command = case["verifier_command"]
            .as_array()
            .expect("verifier_command should be argv-shaped");
        assert!(
            !verifier_command.is_empty() && verifier_command.iter().all(|arg| arg.is_string()),
            "verifier_command should contain only strings for {id}"
        );

        let expected_exit_codes = case["expected_exit_codes"]
            .as_array()
            .expect("expected_exit_codes should be an array");
        assert!(
            !expected_exit_codes.is_empty()
                && expected_exit_codes
                    .iter()
                    .all(|code| code.as_i64().is_some_and(|value| value >= 0)),
            "expected_exit_codes should contain non-negative integers for {id}"
        );

        let expected_patterns = case["expected_patterns"]
            .as_array()
            .expect("expected_patterns should be an array");
        assert!(
            !expected_patterns.is_empty()
                && expected_patterns
                    .iter()
                    .all(|pattern| pattern.as_str().is_some_and(|value| !value.is_empty())),
            "expected_patterns should contain non-empty strings for {id}"
        );

        let source_files = case["source_files"]
            .as_array()
            .expect("source_files should be an array");
        assert!(
            !source_files.is_empty(),
            "source_files should not be empty for {id}"
        );
        for source_file in source_files {
            let source_file = source_file
                .as_str()
                .expect("source file paths must be strings");
            assert!(
                case_dir.join(source_file).exists(),
                "missing source file {source_file} for {id}"
            );
        }
    }
}

#[test]
fn eval_runner_lists_all_seeded_cases() {
    let output = Command::new("python3")
        .arg("evals/v0/run.py")
        .arg("--list")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("python3 should execute eval runner");

    assert!(output.status.success(), "runner list mode should succeed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("rust_parse_unclosed_delimiter"));
    assert!(stdout.contains("rust_runtime_assertion_failure"));
    assert!(stdout.contains("rust_clippy_identity_op"));
}
