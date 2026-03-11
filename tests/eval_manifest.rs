use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::{json, Value};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

fn manifest() -> Value {
    manifest_at("evals/v0/manifest.json")
}

fn typescript_manifest() -> Value {
    manifest_at("evals/v1_typescript/manifest.json")
}

fn manifest_at(path: &str) -> Value {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let raw = fs::read_to_string(repo_root.join(path)).expect("eval manifest should exist");
    serde_json::from_str(&raw).expect("eval manifest should be valid json")
}

fn expected_variant_or_seed(id: &str) -> String {
    let Some((_, suffix)) = id.rsplit_once("_v") else {
        return "seed".to_string();
    };

    if !suffix.is_empty() && suffix.chars().all(|ch| ch.is_ascii_digit()) {
        format!("v{suffix}")
    } else {
        "seed".to_string()
    }
}

#[test]
fn manifest_variant_detection_requires_explicit_numeric_suffix() {
    assert_eq!(expected_variant_or_seed("ts_type_mismatch"), "seed");
    assert_eq!(expected_variant_or_seed("ts_type_mismatch_v2"), "v2");
    assert_eq!(expected_variant_or_seed("ts_type_mismatch_v02"), "v02");
    assert_eq!(expected_variant_or_seed("ts_type_mismatch_v2_beta"), "seed");
    assert_eq!(expected_variant_or_seed("ts_runtime_vibes"), "seed");
    assert_eq!(expected_variant_or_seed("ts_runtime_v"), "seed");
    assert_eq!(expected_variant_or_seed("rust_parse_vibes"), "seed");
    assert_eq!(expected_variant_or_seed("rust_parse_error_v4"), "v4");
}

#[test]
fn eval_manifest_matches_v0_shape() {
    let manifest = manifest();

    assert_eq!(manifest["schema_version"], "mercury-evals-v0");
    assert_eq!(manifest["suite_id"], "rust-v0.3-seeded");
    assert_eq!(manifest["language"], "rust");
    assert_eq!(manifest["version"], 3);
    assert_eq!(
        manifest["artifact_schema_version"],
        "mercury-eval-report-v0"
    );
    assert!(
        manifest["description"]
            .as_str()
            .is_some_and(|value| !value.is_empty()),
        "manifest description should be present"
    );
    assert!(
        manifest["default_timeout_seconds"]
            .as_u64()
            .is_some_and(|value| value > 0),
        "default_timeout_seconds should be a positive integer"
    );

    let supported_modes = manifest["supported_modes"]
        .as_array()
        .expect("supported_modes must be an array");
    assert_eq!(
        supported_modes.len(),
        1,
        "v0 harness should only advertise baseline mode"
    );
    assert_eq!(supported_modes[0], "baseline");

    let cases = manifest["cases"]
        .as_array()
        .expect("cases must be an array");
    assert_eq!(
        cases.len(),
        50,
        "v0 corpus should contain 50 logical case ids"
    );

    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let harness_root = repo_root.join("evals/v0");
    let mut ids = BTreeSet::new();
    let mut path_counts = BTreeMap::new();
    let mut stage_counts = BTreeMap::new();

    for case in cases {
        let id = case["id"].as_str().expect("case id must be a string");
        assert!(ids.insert(id.to_string()), "case ids must be unique");

        assert!(
            case["title"].is_string(),
            "title should be present for {id}"
        );
        let _failure_class = case["failure_class"]
            .as_str()
            .expect("failure_class must be a string");
        assert!(
            matches!(
                case["difficulty"].as_str(),
                Some("easy") | Some("medium") | Some("hard")
            ),
            "difficulty should be easy/medium/hard for {id}"
        );

        let failure_stage = case["failure_stage"]
            .as_str()
            .expect("failure_stage must be a string");
        assert!(
            matches!(failure_stage, "parse" | "compile" | "test" | "lint"),
            "unexpected failure_stage for {id}: {failure_stage}"
        );
        *stage_counts
            .entry(failure_stage.to_string())
            .or_insert(0usize) += 1;

        let path = case["path"].as_str().expect("case path must be a string");
        assert!(
            !Path::new(path).is_absolute(),
            "case path must be repo-relative for {id}"
        );
        *path_counts.entry(path.to_string()).or_insert(0usize) += 1;
        let case_dir = harness_root.join(path);
        assert!(
            case_dir.join("Cargo.toml").exists(),
            "missing Cargo.toml for {id}"
        );

        let provenance = case["provenance"]
            .as_object()
            .expect("provenance should be an object");
        for key in ["origin", "suite", "variant", "generator"] {
            assert!(
                provenance
                    .get(key)
                    .and_then(Value::as_str)
                    .is_some_and(|value| !value.is_empty()),
                "provenance.{key} should be present for {id}"
            );
        }
        let expected_variant = expected_variant_or_seed(id);
        assert_eq!(
            provenance["variant"].as_str(),
            Some(expected_variant.as_str()),
            "variant metadata should match the case id for {id}"
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

        let tags = case["tags"].as_array().expect("tags should be an array");
        assert!(
            !tags.is_empty()
                && tags
                    .iter()
                    .all(|tag| tag.as_str().is_some_and(|value| !value.is_empty())),
            "tags should contain at least one non-empty string for {id}"
        );
        let tag_set: BTreeSet<_> = tags
            .iter()
            .filter_map(Value::as_str)
            .map(ToString::to_string)
            .collect();
        assert!(
            tag_set.contains(&format!("variant:{expected_variant}")),
            "variant tag should be present for {id}"
        );
        if expected_variant != "seed" {
            assert!(
                tag_set.contains("kind:variant"),
                "variant cases should be tagged as variants for {id}"
            );
            assert!(
                !tag_set.contains("kind:seed"),
                "variant cases should not be tagged as seeds for {id}"
            );
        } else {
            assert!(
                tag_set.contains("kind:seed"),
                "seed cases should be tagged as seeds for {id}"
            );
            assert!(
                !tag_set.contains("kind:variant"),
                "seed cases should not be tagged as variants for {id}"
            );
        }

        assert!(
            case["timeout_seconds"]
                .as_u64()
                .is_some_and(|value| value > 0),
            "timeout_seconds should be a positive integer for {id}"
        );
        assert!(
            case["demo_track"].is_null()
                || matches!(
                    case["demo_track"].as_str(),
                    Some("docs") | Some("extended") | Some("none")
                ),
            "demo_track should be null or a supported string for {id}"
        );
    }

    assert_eq!(
        path_counts.len(),
        10,
        "v0 corpus should reuse 10 canonical fixture paths"
    );
    assert!(
        path_counts.values().all(|count| *count == 5),
        "each canonical fixture path should back exactly 5 logical case ids"
    );
    assert_eq!(stage_counts.get("parse"), Some(&5usize));
    assert_eq!(stage_counts.get("compile"), Some(&20usize));
    assert_eq!(stage_counts.get("test"), Some(&15usize));
    assert_eq!(stage_counts.get("lint"), Some(&10usize));
}

#[test]
fn eval_runner_lists_filtered_cases_as_json() {
    let output = Command::new("python3")
        .arg("evals/v0/run.py")
        .arg("--list-json")
        .arg("--stage")
        .arg("lint")
        .arg("--limit")
        .arg("2")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("python3 should execute eval runner");

    assert!(output.status.success(), "runner list mode should succeed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let listed: Value = serde_json::from_str(&stdout).expect("list-json should emit json");
    let listed = listed
        .as_array()
        .expect("list-json output should be an array");
    assert!(
        !listed.is_empty(),
        "list-json should return at least one case"
    );
    assert!(listed.len() <= 2, "limit should cap returned cases");
    assert!(listed.iter().all(|case| case["failure_stage"] == "lint"));
}

#[test]
fn repair_benchmark_runner_lists_filtered_cases_as_json() {
    let output = Command::new("python3")
        .arg("evals/repair_benchmark/run.py")
        .arg("--list-json")
        .arg("--stage")
        .arg("lint")
        .arg("--limit")
        .arg("2")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("python3 should execute repair benchmark runner");

    assert!(
        output.status.success(),
        "repair benchmark list mode should succeed"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let listed: Value = serde_json::from_str(&stdout).expect("list-json should emit json");
    let listed = listed
        .as_array()
        .expect("list-json output should be an array");
    assert!(
        !listed.is_empty(),
        "repair benchmark list-json should return at least one case"
    );
    assert!(listed.len() <= 2, "limit should cap returned cases");
    assert!(listed.iter().all(|case| case["failure_stage"] == "lint"));
}

#[test]
fn repair_workflow_contract_exposes_expected_inputs_and_artifacts() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workflow = fs::read_to_string(repo_root.join(".github/workflows/repair.yml"))
        .expect("repair workflow should exist");

    for expected in [
        "name: Mercury CI Auto-Repair Draft PR",
        "workflow_dispatch:",
        "workflow_call:",
    ] {
        assert!(
            workflow.contains(expected),
            "repair workflow should contain `{expected}`",
        );
    }

    for (input, expected_required, expected_type, expected_default) in [
        ("failure_command", "required: true", "type: string", None),
        ("repair_goal", "required: false", "type: string", None),
        ("source_ref", "required: false", "type: string", None),
        ("base_ref", "required: false", "type: string", None),
        ("setup_command", "required: false", "type: string", None),
        ("lint_command", "required: false", "type: string", None),
        (
            "max_agents",
            "required: false",
            "type: number",
            Some("default: 20"),
        ),
        (
            "max_cost",
            "required: false",
            "type: number",
            Some("default: 0.5"),
        ),
        (
            "artifact_retention_days",
            "required: false",
            "type: number",
            Some("default: 14"),
        ),
        (
            "dry_run",
            "required: false",
            "type: boolean",
            Some("default: false"),
        ),
    ] {
        let dispatch_block = workflow_input_block(&workflow, "workflow_dispatch", input);
        assert!(
            dispatch_block.contains(expected_required),
            "dispatch input `{input}` should include `{expected_required}`"
        );
        assert!(
            dispatch_block.contains(expected_type),
            "dispatch input `{input}` should include `{expected_type}`"
        );
        if let Some(default) = expected_default {
            assert!(
                dispatch_block.contains(default),
                "dispatch input `{input}` should include `{default}`"
            );
        }

        let call_block = workflow_input_block(&workflow, "workflow_call", input);
        assert!(
            call_block.contains(expected_required),
            "workflow_call input `{input}` should include `{expected_required}`"
        );
        assert!(
            call_block.contains(expected_type),
            "workflow_call input `{input}` should include `{expected_type}`"
        );
        if let Some(default) = expected_default {
            assert!(
                call_block.contains(default),
                "workflow_call input `{input}` should include `{default}`"
            );
        }
    }

    for secret in ["INCEPTION_API_KEY", "MERCURY_API_KEY", "inception_api_key"] {
        let block = workflow_secret_block(&workflow, secret);
        assert!(
            block.contains("required: false"),
            "workflow_call secret `{secret}` should be optional"
        );
    }

    for expected in [
        "repair_verified = all(",
        "baseline_reproduced,",
        "final_bundle_verified,",
        "applied,",
        "post_verify.returncode == 0,",
        "bool(diff_text.strip()),",
        "repo_root / \"target\" / \"release\" / \"mercury-cli\"",
        "inputs.dry_run != true",
        "Upload evidence bundle",
        "Validate evidence bundle contract",
        "summary.md",
        "decision.json",
        "environment.json",
        "pr-body.md",
        "repair.diff",
        "repair.diffstat.txt",
        "baseline.stdout.log",
        "baseline.stderr.log",
    ] {
        assert!(
            workflow.contains(expected),
            "repair workflow should contain `{expected}`",
        );
    }
}

#[test]
fn repair_benchmark_workflow_contract_exposes_expected_inputs_and_artifacts() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workflow = fs::read_to_string(repo_root.join(".github/workflows/repair-benchmark.yml"))
        .expect("repair benchmark workflow should exist");

    assert!(
        workflow.contains("name: Mercury Repair Benchmark"),
        "repair benchmark workflow should declare its name"
    );

    for (input, expected_required, expected_type, expected_default) in [
        (
            "quality_agent_count",
            "required: false",
            "type: number",
            Some("default: 4"),
        ),
        (
            "quality_case_limit",
            "required: false",
            "type: string",
            None,
        ),
        (
            "representative_count",
            "required: false",
            "type: number",
            Some("default: 10"),
        ),
        (
            "timeout_seconds",
            "required: false",
            "type: number",
            Some("default: 300"),
        ),
        (
            "max_cost",
            "required: false",
            "type: number",
            Some("default: 0.5"),
        ),
        (
            "artifact_retention_days",
            "required: false",
            "type: number",
            Some("default: 14"),
        ),
    ] {
        let dispatch_block = workflow_input_block(&workflow, "workflow_dispatch", input);
        assert!(
            dispatch_block.contains(expected_required),
            "dispatch input `{input}` should include `{expected_required}`"
        );
        assert!(
            dispatch_block.contains(expected_type),
            "dispatch input `{input}` should include `{expected_type}`"
        );
        if let Some(default) = expected_default {
            assert!(
                dispatch_block.contains(default),
                "dispatch input `{input}` should include `{default}`"
            );
        }
    }

    for expected in [
        "evals/repair_benchmark/run.py",
        "--mode quality",
        "--mode agent-sweep",
        "--agent-count 1",
        "--agent-count 2",
        "--agent-count 4",
        "--agent-count 8",
        "report.json",
        "summary.md",
        "docs/benchmarks",
        "mercury-repair-benchmark-v1",
        "Upload benchmark artifacts",
        "Validate benchmark artifact contract",
    ] {
        assert!(
            workflow.contains(expected),
            "repair benchmark workflow should contain `{expected}`",
        );
    }
}

#[test]
fn eval_runner_writes_expected_run_bundle_for_single_case() {
    let temp = tempfile::tempdir().expect("tempdir should be created");
    let output_dir = temp.path().join("reports");

    let output = Command::new("python3")
        .arg("evals/v0/run.py")
        .arg("--case")
        .arg("rust_type_mismatch")
        .arg("--run-id")
        .arg("test-run")
        .arg("--output-dir")
        .arg(output_dir.as_os_str())
        .arg("--clean-output")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("python3 should execute eval runner");
    assert!(
        output.status.success(),
        "single-case baseline run should succeed"
    );

    let run_dir = output_dir.join("run-test-run");
    assert!(run_dir.join("manifest.snapshot.json").exists());
    assert!(run_dir.join("environment.json").exists());
    assert!(run_dir.join("selection.json").exists());
    assert!(run_dir.join("report.json").exists());
    assert!(run_dir.join("summary.md").exists());
    assert!(run_dir
        .join("cases")
        .join("rust_type_mismatch")
        .join("result.json")
        .exists());

    let report_raw =
        fs::read_to_string(run_dir.join("report.json")).expect("report.json should be readable");
    let report: Value =
        serde_json::from_str(&report_raw).expect("report.json should be valid json");
    assert_eq!(report["schema_version"], "mercury-eval-report-v0");
    assert_eq!(report["suite_id"], "rust-v0.3-seeded");
    assert_eq!(report["mode"], "baseline");
    assert_eq!(report["manifest"]["schema_version"], "mercury-evals-v0");
    assert_eq!(report["manifest"]["version"], 3);
    assert_eq!(report["manifest"]["supported_modes"][0], "baseline");
    assert_eq!(report["corpus"]["manifest_case_count"], 50);
    assert_eq!(report["corpus"]["unique_fixture_paths"], 10);
    assert_eq!(
        report["corpus"]["fixture_path_reuse"]["cases/rust_type_mismatch"],
        5
    );
    assert_eq!(report["totals"]["cases"], 1);
    assert_eq!(report["totals"]["baseline_ok"], 1);
    assert_eq!(report["totals"]["baseline_failed"], 0);
    assert_eq!(
        report["selection"]["selected_case_ids"][0],
        "rust_type_mismatch"
    );
}

#[test]
fn release_truth_and_benchmark_docs_remain_consistent() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let cargo_toml =
        fs::read_to_string(repo_root.join("Cargo.toml")).expect("Cargo.toml should exist");
    let readme = fs::read_to_string(repo_root.join("README.md")).expect("README should exist");
    let architecture = fs::read_to_string(repo_root.join("docs/ARCHITECTURE.md"))
        .expect("architecture doc should exist");
    let quality =
        fs::read_to_string(repo_root.join("docs/QUALITY.md")).expect("quality doc should exist");
    let benchmark_readme = fs::read_to_string(repo_root.join("docs/benchmarks/README.md"))
        .expect("benchmark README should exist");
    let benchmark_report =
        fs::read_to_string(repo_root.join("docs/benchmarks/rust-v0-repair-benchmark.md"))
            .expect("benchmark report scaffold should exist");
    let typescript_readme = fs::read_to_string(repo_root.join("evals/v1_typescript/README.md"))
        .expect("typescript README should exist");

    assert!(
        cargo_toml.contains("version = \"1.0.0-beta.1\""),
        "Cargo.toml should keep branch-head on the beta version"
    );

    for (label, text) in [
        ("README", readme.as_str()),
        ("ARCHITECTURE", architecture.as_str()),
        ("QUALITY", quality.as_str()),
    ] {
        assert!(
            text.contains("1.0.0-beta.1"),
            "{label} should describe the beta branch contract"
        );
        assert!(
            text.contains("docs/benchmarks/"),
            "{label} should point readers to checked-in benchmark reports"
        );
    }

    for (label, text) in [
        ("docs/benchmarks/README.md", benchmark_readme.as_str()),
        (
            "docs/benchmarks/rust-v0-repair-benchmark.md",
            benchmark_report.as_str(),
        ),
    ] {
        assert!(
            text.contains("mercury-repair-benchmark-v1"),
            "{label} should mention the aggregate benchmark schema"
        );
    }

    assert!(
        typescript_readme.contains("scoped support lane"),
        "TypeScript README should remain scoped-support only"
    );
    assert!(
        typescript_readme.contains("without claiming parity"),
        "TypeScript README should not claim parity with Rust"
    );
}

#[test]
fn eval_manifest_verifier_commands_match_supported_rust_contract() {
    let manifest = manifest();
    let cases = manifest["cases"]
        .as_array()
        .expect("cases must be an array");

    for case in cases {
        let id = case["id"].as_str().expect("case id must be a string");
        let stage = case["failure_stage"]
            .as_str()
            .expect("failure_stage must be a string");
        let verifier_command = case["verifier_command"]
            .as_array()
            .expect("verifier_command should be an array");
        let command_text = verifier_command
            .iter()
            .map(|part| {
                part.as_str()
                    .expect("verifier command args must be strings")
            })
            .collect::<Vec<_>>()
            .join(" ");

        match stage {
            "test" => assert!(
                command_text.starts_with("cargo test "),
                "test case `{id}` should use cargo test verifier command"
            ),
            "lint" => assert!(
                command_text.starts_with("cargo clippy "),
                "lint case `{id}` should use cargo clippy verifier command"
            ),
            "parse" | "compile" => assert!(
                command_text.starts_with("cargo check "),
                "parse/compile case `{id}` should use cargo check verifier command"
            ),
            other => panic!("unexpected failure stage `{other}` for `{id}`"),
        }
    }
}

#[test]
fn typescript_eval_manifest_matches_v1_shape() {
    let manifest = typescript_manifest();

    assert_eq!(manifest["schema_version"], "mercury-evals-v0");
    assert_eq!(manifest["suite_id"], "typescript-v1.0-seeded");
    assert_eq!(manifest["language"], "typescript");
    assert_eq!(manifest["version"], 1);
    assert_eq!(
        manifest["artifact_schema_version"],
        "mercury-eval-report-v0"
    );
    assert!(
        manifest["description"]
            .as_str()
            .is_some_and(|value| !value.is_empty()),
        "manifest description should be present"
    );
    assert!(
        manifest["default_timeout_seconds"]
            .as_u64()
            .is_some_and(|value| value > 0),
        "default_timeout_seconds should be a positive integer"
    );

    let supported_modes = manifest["supported_modes"]
        .as_array()
        .expect("supported_modes must be an array");
    assert_eq!(
        supported_modes,
        &vec![Value::String("baseline".to_string())],
        "typescript harness should only advertise baseline mode"
    );

    let cases = manifest["cases"]
        .as_array()
        .expect("cases must be an array");
    assert_eq!(cases.len(), 50, "typescript corpus should contain 50 cases");

    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let harness_root = repo_root.join("evals/v1_typescript");
    let mut ids = BTreeSet::new();
    let mut path_counts = BTreeMap::new();
    let mut stage_counts = BTreeMap::new();

    for case in cases {
        let id = case["id"].as_str().expect("case id must be a string");
        assert!(ids.insert(id.to_string()), "case ids must be unique");

        assert!(
            case["title"].is_string(),
            "title should be present for {id}"
        );
        let failure_class = case["failure_class"]
            .as_str()
            .expect("failure_class must be a string");
        assert!(
            matches!(
                case["difficulty"].as_str(),
                Some("easy") | Some("medium") | Some("hard")
            ),
            "difficulty should be easy/medium/hard for {id}"
        );

        let path = case["path"].as_str().expect("case path must be a string");
        assert!(
            !Path::new(path).is_absolute(),
            "case path must be repo-relative for {id}"
        );
        *path_counts.entry(path.to_string()).or_insert(0usize) += 1;
        let case_dir = harness_root.join(path);
        assert!(
            case_dir.join("package.json").exists(),
            "missing package.json for {id}"
        );

        let stage = case["failure_stage"]
            .as_str()
            .expect("failure_stage must be a string");
        assert!(
            matches!(stage, "parse" | "compile" | "test" | "lint"),
            "unexpected failure stage `{stage}` for `{id}`"
        );
        *stage_counts.entry(stage.to_string()).or_insert(0usize) += 1;

        let provenance = case["provenance"]
            .as_object()
            .expect("provenance should be an object");
        for key in ["origin", "suite", "variant", "generator"] {
            assert!(
                provenance
                    .get(key)
                    .and_then(Value::as_str)
                    .is_some_and(|value| !value.is_empty()),
                "provenance.{key} should be present for {id}"
            );
        }
        assert_eq!(
            provenance["suite"].as_str(),
            Some("typescript-v1.0-seeded"),
            "provenance.suite should match the typescript suite for {id}"
        );
        let expected_variant = expected_variant_or_seed(id);
        assert_eq!(
            provenance["variant"].as_str(),
            Some(expected_variant.as_str()),
            "variant metadata should match the case id for {id}"
        );

        let verifier_command = case["verifier_command"]
            .as_array()
            .expect("verifier_command should be argv-shaped");
        assert!(
            verifier_command
                == &vec![
                    Value::String("node".into()),
                    Value::String("scripts/check.js".into())
                ]
                || verifier_command
                    == &vec![
                        Value::String("node".into()),
                        Value::String("scripts/test.js".into())
                    ]
                || verifier_command
                    == &vec![
                        Value::String("node".into()),
                        Value::String("scripts/lint.js".into())
                    ],
            "typescript verifier command should be node script runner for {id}"
        );
        match stage {
            "parse" | "compile" => assert_eq!(
                verifier_command,
                &vec![
                    Value::String("node".into()),
                    Value::String("scripts/check.js".into())
                ],
                "parse/compile cases should use check.js for {id}"
            ),
            "test" => assert_eq!(
                verifier_command,
                &vec![
                    Value::String("node".into()),
                    Value::String("scripts/test.js".into())
                ],
                "test cases should use test.js for {id}"
            ),
            "lint" => assert_eq!(
                verifier_command,
                &vec![
                    Value::String("node".into()),
                    Value::String("scripts/lint.js".into())
                ],
                "lint cases should use lint.js for {id}"
            ),
            _ => unreachable!("already asserted valid stage"),
        }

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

        let tags = case["tags"].as_array().expect("tags should be an array");
        assert!(
            !tags.is_empty()
                && tags
                    .iter()
                    .all(|tag| tag.as_str().is_some_and(|value| !value.is_empty())),
            "tags should contain at least one non-empty string for {id}"
        );
        let tag_set: BTreeSet<_> = tags
            .iter()
            .filter_map(Value::as_str)
            .map(ToString::to_string)
            .collect();
        assert!(
            tag_set.contains("language:typescript"),
            "language tag should be present for {id}"
        );
        assert!(
            tag_set.contains(&format!("stage:{stage}")),
            "stage tag should match failure stage for {id}"
        );
        assert!(
            tag_set.contains(&format!("failure:{failure_class}")),
            "failure tag should match failure class for {id}"
        );
        assert!(
            tag_set.contains(&format!("variant:{expected_variant}")),
            "variant tag should be present for {id}"
        );
        if expected_variant != "seed" {
            assert!(
                tag_set.contains("kind:variant"),
                "variant cases should be tagged as variants for {id}"
            );
            assert!(
                !tag_set.contains("kind:seed"),
                "variant cases should not be tagged as seeds for {id}"
            );
        } else {
            assert!(
                tag_set.contains("kind:seed"),
                "seed cases should be tagged as seeds for {id}"
            );
            assert!(
                !tag_set.contains("kind:variant"),
                "seed cases should not be tagged as variants for {id}"
            );
        }

        assert!(
            case["timeout_seconds"]
                .as_u64()
                .is_some_and(|value| value > 0),
            "timeout_seconds should be a positive integer for {id}"
        );
        assert!(
            case["demo_track"].is_null()
                || matches!(
                    case["demo_track"].as_str(),
                    Some("docs") | Some("extended") | Some("none")
                ),
            "demo_track should be null or a supported string for {id}"
        );
    }

    assert_eq!(
        path_counts.len(),
        10,
        "typescript corpus should have 10 fixture paths"
    );
    assert!(
        path_counts.values().all(|count| *count == 5),
        "each typescript fixture path should back exactly 5 logical case ids"
    );
    assert_eq!(stage_counts.get("parse"), Some(&5usize));
    assert_eq!(stage_counts.get("compile"), Some(&20usize));
    assert_eq!(stage_counts.get("test"), Some(&15usize));
    assert_eq!(stage_counts.get("lint"), Some(&10usize));
}

#[test]
fn typescript_eval_runner_lists_filtered_cases_as_json() {
    let output = Command::new("python3")
        .arg("evals/v1_typescript/run.py")
        .arg("--list-json")
        .arg("--stage")
        .arg("test")
        .arg("--limit")
        .arg("2")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("python3 should execute typescript eval runner");

    assert!(
        output.status.success(),
        "typescript runner list mode should succeed"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let listed: Value = serde_json::from_str(&stdout).expect("list-json should emit json");
    let listed = listed
        .as_array()
        .expect("list-json output should be an array");
    assert!(
        !listed.is_empty(),
        "list-json should return at least one typescript case"
    );
    assert!(listed.len() <= 2, "limit should cap returned cases");
    assert!(listed.iter().all(|case| case["failure_stage"] == "test"));
}

fn workflow_input_block(workflow: &str, scope: &str, input: &str) -> String {
    let scope_anchor = format!("{scope}:\n    inputs:\n");
    let scope_start = workflow
        .find(&scope_anchor)
        .unwrap_or_else(|| panic!("workflow should contain scope anchor `{scope_anchor}`"));
    let inputs_start = scope_start + scope_anchor.len();
    let input_anchor = format!("      {input}:");
    let local_start = workflow[inputs_start..]
        .find(&input_anchor)
        .unwrap_or_else(|| panic!("workflow should contain input `{input}` under `{scope}`"));
    let start = inputs_start + local_start;
    let body_start = start + input_anchor.len();
    let mut cursor = body_start;
    let mut end = workflow.len();
    for line in workflow[body_start..].split_inclusive('\n') {
        if line.starts_with("      ")
            && !line.starts_with("        ")
            && line.trim_end().ends_with(':')
        {
            end = cursor;
            break;
        }
        cursor += line.len();
    }
    workflow[start..end].to_string()
}

fn workflow_secret_block(workflow: &str, secret: &str) -> String {
    let anchor = "workflow_call:\n    inputs:".to_string();
    let start = workflow
        .find(&anchor)
        .unwrap_or_else(|| panic!("workflow should contain anchor `{anchor}`"));
    let tail = &workflow[start..];
    let secret_anchor = format!("\n      {secret}:");
    let secret_start = tail
        .find(&secret_anchor)
        .unwrap_or_else(|| panic!("workflow should contain secret `{secret}`"));
    let absolute_start = start + secret_start;
    let secret_header = format!("\n      {secret}:");
    let body_start = absolute_start + secret_header.len();
    let mut cursor = body_start;
    let mut end = workflow.len();
    for line in workflow[body_start..].split_inclusive('\n') {
        if line.starts_with("      ")
            && !line.starts_with("        ")
            && line.trim_end().ends_with(':')
        {
            end = cursor;
            break;
        }
        cursor += line.len();
    }
    workflow[absolute_start..end].to_string()
}

fn command_available(command: &str) -> bool {
    Command::new(command)
        .arg("--version")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .is_ok()
}

#[cfg(unix)]
#[test]
fn repair_benchmark_runner_downgrades_false_green_after_independent_rerun() {
    let temp = tempfile::tempdir().expect("tempdir should be created");
    let suite_root = temp.path().join("suite");
    let case_root = suite_root.join("cases/synthetic_false_green");
    fs::create_dir_all(&case_root).expect("synthetic case directory should exist");

    fs::write(case_root.join("state.txt"), "red\n").expect("state file should be written");
    fs::write(
        case_root.join("verifier.py"),
        r#"from pathlib import Path
import sys

state = Path("state.txt").read_text(encoding="utf-8").strip()
sys.exit(0 if state == "green" else 1)
"#,
    )
    .expect("verifier should be written");

    let manifest = json!({
        "schema_version": "mercury-evals-v0",
        "suite_id": "synthetic-repair-benchmark",
        "language": "rust",
        "version": 1,
        "artifact_schema_version": "mercury-eval-report-v0",
        "description": "Synthetic manifest for false-green downgrade coverage",
        "default_timeout_seconds": 30,
        "supported_modes": ["baseline"],
        "cases": [{
            "id": "synthetic_false_green",
            "title": "Synthetic false green",
            "failure_stage": "test",
            "failure_class": "assertion",
            "difficulty": "easy",
            "path": "cases/synthetic_false_green",
            "verifier_command": ["python3", "verifier.py"],
            "expected_exit_codes": [1],
            "expected_patterns": ["synthetic"],
            "source_files": ["state.txt", "verifier.py"],
            "tags": ["language:rust", "stage:test", "kind:seed"],
            "timeout_seconds": 30,
            "demo_track": "none",
            "provenance": {
                "origin": "synthetic",
                "suite": "synthetic-repair-benchmark",
                "variant": "seed",
                "generator": "tests/eval_manifest.rs"
            }
        }]
    });
    fs::write(
        suite_root.join("manifest.json"),
        serde_json::to_string_pretty(&manifest).expect("manifest should serialize"),
    )
    .expect("synthetic manifest should be written");

    let fake_binary = temp.path().join("fake-mercury");
    fs::write(
        &fake_binary,
        r#"#!/usr/bin/env python3
import json
import shutil
import sys
from pathlib import Path

cwd = Path.cwd()
args = sys.argv[1:]

if args and args[0] == "init":
    sys.exit(0)

if args and args[0] == "fix":
    run_root = cwd / ".mercury" / "runs" / "run-test"
    run_root.mkdir(parents=True, exist_ok=True)
    sandbox_root = cwd.parent / "fake-sandbox"
    candidate = sandbox_root / "final-bundle"
    if sandbox_root.exists():
        shutil.rmtree(sandbox_root)
    shutil.copytree(cwd, candidate, ignore=shutil.ignore_patterns(".mercury"))
    payload = {
        "schema_version": "mercury-repair-benchmark-case-v1",
        "description": "synthetic false green",
        "started_at": "2026-03-11T00:00:00Z",
        "finished_at": "2026-03-11T00:00:01Z",
        "duration_ms": 1000,
        "accepted_steps": 1,
        "rejected_steps": 0,
        "verification_failures": 0,
        "retry_attempts": 0,
        "time_to_first_candidate_ms": 10,
        "time_to_verified_repair_ms": 20,
        "final_bundle_verified": True,
        "applied": True,
        "accepted_patch": True,
        "accepted_patch_bytes": 16,
        "outcome": "verified_repair",
        "false_green": False,
        "sandbox_run_root": str(sandbox_root),
        "total_cost_usd": 0.12,
        "budget_remaining_usd": 0.38,
        "verifier": {
            "parse_before_write": True,
            "test_after_write": True,
            "lint_after_write": False,
            "test_command": "python3 verifier.py",
            "lint_command": ""
        },
        "security": {
            "api_key_env_names": ["INCEPTION_API_KEY"],
            "unsafe_verifier_override": False
        }
    }
    (run_root / "benchmark-run.json").write_text(json.dumps(payload, indent=2), encoding="utf-8")
    sys.exit(0)

sys.exit(1)
"#,
    )
    .expect("fake mercury binary should be written");

    let mut permissions = fs::metadata(&fake_binary)
        .expect("fake mercury binary should exist")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&fake_binary, permissions)
        .expect("fake mercury binary should be executable");

    let output_dir = temp.path().join("reports");
    let output = Command::new("python3")
        .arg("evals/repair_benchmark/run.py")
        .arg("--suite")
        .arg(suite_root.join("manifest.json"))
        .arg("--binary")
        .arg(&fake_binary)
        .arg("--case")
        .arg("synthetic_false_green")
        .arg("--run-id")
        .arg("false-green")
        .arg("--output-dir")
        .arg(&output_dir)
        .arg("--clean-output")
        .env("INCEPTION_API_KEY", "test-key")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("python3 should execute repair benchmark runner");

    assert!(
        output.status.success(),
        "synthetic false-green benchmark run should succeed: stdout=`{}` stderr=`{}`",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let run_dir = output_dir.join("run-false-green");
    let result: Value = serde_json::from_str(
        &fs::read_to_string(
            run_dir
                .join("cases")
                .join("synthetic_false_green")
                .join("agents-4")
                .join("result.json"),
        )
        .expect("result.json should exist"),
    )
    .expect("result.json should be valid json");
    assert_eq!(result["schema_version"], "mercury-repair-benchmark-case-v1");
    assert_eq!(result["false_green"], true);
    assert_eq!(result["verified_repair"], false);
    assert_eq!(result["outcome"], "false_green");
    assert_eq!(result["final_bundle_verified"], true);
    assert_eq!(result["accepted_patch"], true);
    assert_eq!(result["independent_rerun_success"], false);

    let report: Value = serde_json::from_str(
        &fs::read_to_string(run_dir.join("report.json")).expect("report.json should exist"),
    )
    .expect("report.json should be valid json");
    assert_eq!(report["schema_version"], "mercury-repair-benchmark-v1");
    assert_eq!(report["metrics"]["false_greens"], 1);
    assert_eq!(report["metrics"]["verified_repairs"], 0);
}

#[cfg(unix)]
#[test]
fn typescript_eval_runner_writes_expected_run_bundle_for_single_case() {
    if !command_available("node") {
        eprintln!("skipping typescript eval single-case test because node is unavailable");
        return;
    }

    let temp = tempfile::tempdir().expect("tempdir should be created");
    let output_dir = temp.path().join("reports");

    let output = Command::new("python3")
        .arg("evals/v1_typescript/run.py")
        .arg("--case")
        .arg("ts_type_mismatch")
        .arg("--run-id")
        .arg("test-run")
        .arg("--output-dir")
        .arg(output_dir.as_os_str())
        .arg("--clean-output")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("python3 should execute typescript eval runner");
    assert!(
        output.status.success(),
        "single-case baseline run should succeed"
    );

    let run_dir = output_dir.join("run-test-run");
    assert!(run_dir.join("manifest.snapshot.json").exists());
    assert!(run_dir.join("environment.json").exists());
    assert!(run_dir.join("selection.json").exists());
    assert!(run_dir.join("report.json").exists());
    assert!(run_dir.join("summary.md").exists());
    assert!(run_dir
        .join("cases")
        .join("ts_type_mismatch")
        .join("result.json")
        .exists());

    let report_raw =
        fs::read_to_string(run_dir.join("report.json")).expect("report.json should be readable");
    let report: Value =
        serde_json::from_str(&report_raw).expect("report.json should be valid json");
    let selection_raw = fs::read_to_string(run_dir.join("selection.json"))
        .expect("selection.json should be readable");
    let selection: Value =
        serde_json::from_str(&selection_raw).expect("selection.json should be valid json");
    assert_eq!(report["schema_version"], "mercury-eval-report-v0");
    assert_eq!(report["suite_id"], "typescript-v1.0-seeded");
    assert_eq!(report["mode"], "baseline");
    assert_eq!(report["manifest"]["schema_version"], "mercury-evals-v0");
    assert_eq!(report["manifest"]["version"], 1);
    assert_eq!(report["corpus"]["manifest_case_count"], 50);
    assert_eq!(report["corpus"]["unique_fixture_paths"], 10);
    assert_eq!(
        report["corpus"]["fixture_path_reuse"]["cases/ts_type_mismatch"],
        5
    );
    assert_eq!(report["totals"]["cases"], 1);
    assert_eq!(report["totals"]["baseline_ok"], 1);
    assert_eq!(selection["requested_case_ids"][0], "ts_type_mismatch");
    assert_eq!(selection["selected_case_ids"][0], "ts_type_mismatch");
    assert_eq!(selection["selected_count"], 1);
    assert_eq!(selection["selected_unique_fixture_paths"], 1);
    assert_eq!(
        selection["selected_fixture_path_reuse"]["cases/ts_type_mismatch"],
        1
    );
    assert_eq!(report["selection"], selection);
    assert_eq!(
        report["selection"]["selected_case_ids"][0],
        "ts_type_mismatch"
    );
}
