#![recursion_limit = "256"]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

const PENDING_BENCHMARK_STATUS: &str =
    "Status: pending first checked-in secret-backed Tier 1 Rust beta run.";
const PUBLISHED_BENCHMARK_STATUS: &str = "Status: published from benchmark runner artifacts.";
const EXECUTION_DIAGNOSTIC_FIELDS: [&str; 4] = [
    "generation_failures",
    "safety_failures",
    "candidate_verification_failures",
    "final_bundle_failures",
];
const CANDIDATE_ATTEMPT_FIELDS: [&str; 4] = [
    "apply_edit_attempts",
    "grounded_next_edit_attempts",
    "critique_retry_attempts",
    "exploratory_next_edit_attempts",
];
const CANDIDATE_ACCEPTED_FIELDS: [&str; 4] = [
    "apply_edit_accepted_steps",
    "grounded_next_edit_accepted_steps",
    "critique_retry_accepted_steps",
    "exploratory_next_edit_accepted_steps",
];

fn manifest() -> Value {
    manifest_at("evals/v0/manifest.json")
}

fn tier0_manifest() -> Value {
    manifest_at("evals/v0/tier0-manifest.json")
}

fn tier1_manifest() -> Value {
    manifest_at("evals/v0/tier1-manifest.json")
}

fn tier2_manifest() -> Value {
    manifest_at("evals/v0/tier2-manifest.json")
}

fn typescript_manifest() -> Value {
    manifest_at("evals/v1_typescript/manifest.json")
}

fn manifest_at(path: &str) -> Value {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let raw = fs::read_to_string(repo_root.join(path)).expect("eval manifest should exist");
    serde_json::from_str(&raw).expect("eval manifest should be valid json")
}

fn assert_public_benchmark_report_is_scrubbed(report: &Value) {
    assert!(
        report.get("run_root").is_none(),
        "public benchmark report should not expose run_root"
    );
    assert!(
        report.get("binary_path").is_none(),
        "public benchmark report should not expose binary_path"
    );
    assert!(
        report.get("api_key_env").is_none(),
        "public benchmark report should not expose api_key_env"
    );
    assert!(
        report["repair_outcome_distribution"]
            .as_object()
            .is_some_and(|value| !value.is_empty()),
        "public benchmark report should keep a non-empty repair outcome distribution"
    );
    let execution_diagnostics = report["execution_diagnostics"]
        .as_object()
        .expect("public benchmark report should expose execution_diagnostics");
    for field in EXECUTION_DIAGNOSTIC_FIELDS {
        assert!(
            execution_diagnostics
                .get(field)
                .and_then(Value::as_u64)
                .is_some(),
            "public benchmark report should expose numeric execution diagnostics for {field}"
        );
    }
    for breakdown_key in [
        "tier_breakdown",
        "verifier_class_breakdown",
        "candidate_lineage_breakdown",
    ] {
        assert!(
            report
                .get(breakdown_key)
                .and_then(Value::as_object)
                .is_some_and(|value| !value.is_empty()),
            "public benchmark report should expose a non-empty {breakdown_key}"
        );
    }
    let candidate_attempt_breakdown = report["candidate_attempt_breakdown"]
        .as_object()
        .expect("public benchmark report should expose candidate_attempt_breakdown");
    assert!(
        !candidate_attempt_breakdown.is_empty(),
        "public benchmark report should expose at least one candidate_attempt_breakdown entry"
    );
    for (label, entry) in candidate_attempt_breakdown {
        assert!(
            entry.get("attempts").and_then(Value::as_u64).is_some(),
            "public benchmark report should expose numeric attempts for {label}"
        );
        assert!(
            entry
                .get("accepted_steps")
                .and_then(Value::as_u64)
                .is_some(),
            "public benchmark report should expose numeric accepted_steps for {label}"
        );
    }

    if let Some(manifest_path) = report["selection"]["manifest_path"].as_str() {
        assert!(
            !Path::new(manifest_path).is_absolute(),
            "public benchmark report should not expose an absolute manifest path"
        );
    }

    for result in report["results"]
        .as_array()
        .expect("public benchmark report should keep results as an array")
    {
        assert!(
            result.get("benchmark_run_path").is_none(),
            "public benchmark result should not expose benchmark_run_path"
        );
        assert!(
            result.get("candidate_workspace").is_none(),
            "public benchmark result should not expose candidate_workspace"
        );
        assert!(
            result["accepted_patch_bytes"].as_u64().is_some(),
            "public benchmark result should expose normalized accepted_patch_bytes"
        );
        assert!(
            result["tier"].as_str().is_some(),
            "public benchmark result should expose normalized tier"
        );
        assert!(
            result["verifier_class"].as_str().is_some(),
            "public benchmark result should expose verifier_class"
        );
        assert!(
            result["candidate_lineage"].as_str().is_some(),
            "public benchmark result should expose candidate_lineage"
        );
        for field in EXECUTION_DIAGNOSTIC_FIELDS {
            assert!(
                result.get(field).and_then(Value::as_u64).is_some(),
                "public benchmark result should expose numeric execution diagnostics for {field}"
            );
        }
        for field in CANDIDATE_ATTEMPT_FIELDS {
            assert!(
                result.get(field).and_then(Value::as_u64).is_some(),
                "public benchmark result should expose numeric candidate attempt counts for {field}"
            );
        }
        for field in CANDIDATE_ACCEPTED_FIELDS {
            assert!(
                result.get(field).and_then(Value::as_u64).is_some(),
                "public benchmark result should expose numeric candidate accepted counts for {field}"
            );
        }
    }
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
fn eval_tier0_manifest_matches_diagnostic_shape() {
    let full_manifest = manifest();
    let full_case_ids: BTreeSet<_> = full_manifest["cases"]
        .as_array()
        .expect("v0 cases must be an array")
        .iter()
        .map(|case| case["id"].as_str().expect("case id must be a string"))
        .collect();

    let manifest = tier0_manifest();
    assert_eq!(manifest["schema_version"], "mercury-evals-v0");
    assert_eq!(manifest["suite_id"], "rust-v0.3-tier0");
    assert_eq!(manifest["language"], "rust");
    assert_eq!(manifest["version"], 3);
    assert_eq!(
        manifest["description"],
        "Tier 0 Rust diagnostic lane focused on trivial single-file assertion, logic, and clippy repairs."
    );

    let supported_modes = manifest["supported_modes"]
        .as_array()
        .expect("supported_modes must be an array");
    assert_eq!(supported_modes.len(), 1);
    assert_eq!(supported_modes[0], "baseline");

    let cases = manifest["cases"]
        .as_array()
        .expect("cases must be an array");
    assert_eq!(
        cases.len(),
        20,
        "tier0 corpus should contain 20 logical case ids"
    );

    let mut ids = BTreeSet::new();
    let mut stage_counts = BTreeMap::new();
    let mut failure_class_counts = BTreeMap::new();
    for case in cases {
        let id = case["id"].as_str().expect("case id must be a string");
        assert!(ids.insert(id.to_string()), "tier0 case ids must be unique");
        assert!(
            full_case_ids.contains(id),
            "tier0 case `{id}` should exist in the seeded v0 corpus"
        );

        let stage = case["failure_stage"]
            .as_str()
            .expect("failure_stage must be a string");
        *stage_counts.entry(stage.to_string()).or_insert(0usize) += 1;

        let failure_class = case["failure_class"]
            .as_str()
            .expect("failure_class must be a string");
        assert!(
            matches!(
                failure_class,
                "test_assertion"
                    | "logic_off_by_one"
                    | "clippy_needless_return"
                    | "clippy_identity_op"
            ),
            "tier0 should keep only trivial failure class `{failure_class}`"
        );
        *failure_class_counts
            .entry(failure_class.to_string())
            .or_insert(0usize) += 1;
    }

    assert_eq!(stage_counts.get("test"), Some(&10usize));
    assert_eq!(stage_counts.get("lint"), Some(&10usize));
    assert!(
        !stage_counts.contains_key("parse"),
        "tier0 should exclude parse-stage cases"
    );
    assert!(
        !stage_counts.contains_key("compile"),
        "tier0 should exclude compile-stage cases"
    );

    for failure_class in [
        "test_assertion",
        "logic_off_by_one",
        "clippy_needless_return",
        "clippy_identity_op",
    ] {
        assert_eq!(
            failure_class_counts.get(failure_class),
            Some(&5usize),
            "tier0 should keep five cases for `{failure_class}`"
        );
    }
}

#[test]
fn eval_tier1_manifest_matches_beta_shape() {
    let full_manifest = manifest();
    let full_case_ids: BTreeSet<_> = full_manifest["cases"]
        .as_array()
        .expect("v0 cases must be an array")
        .iter()
        .map(|case| case["id"].as_str().expect("case id must be a string"))
        .collect();

    let manifest = tier1_manifest();
    assert_eq!(manifest["schema_version"], "mercury-evals-v0");
    assert_eq!(manifest["suite_id"], "rust-v0.3-tier1");
    assert_eq!(manifest["language"], "rust");
    assert_eq!(manifest["version"], 3);
    assert_eq!(
        manifest["description"],
        "Tier 1 Rust repair beta lane focused on solvable single-file compile, assertion, logic, and clippy failures."
    );

    let supported_modes = manifest["supported_modes"]
        .as_array()
        .expect("supported_modes must be an array");
    assert_eq!(supported_modes.len(), 1);
    assert_eq!(supported_modes[0], "baseline");

    let cases = manifest["cases"]
        .as_array()
        .expect("cases must be an array");
    assert_eq!(
        cases.len(),
        35,
        "tier1 corpus should contain 35 logical case ids"
    );

    let mut ids = BTreeSet::new();
    let mut stage_counts = BTreeMap::new();
    let mut failure_class_counts = BTreeMap::new();
    for case in cases {
        let id = case["id"].as_str().expect("case id must be a string");
        assert!(ids.insert(id.to_string()), "tier1 case ids must be unique");
        assert!(
            full_case_ids.contains(id),
            "tier1 case `{id}` should exist in the seeded v0 corpus"
        );

        let stage = case["failure_stage"]
            .as_str()
            .expect("failure_stage must be a string");
        *stage_counts.entry(stage.to_string()).or_insert(0usize) += 1;

        let failure_class = case["failure_class"]
            .as_str()
            .expect("failure_class must be a string");
        assert!(
            !matches!(failure_class, "parser" | "trait_bound" | "panic_unwrap"),
            "tier1 should exclude unsolved failure class `{failure_class}`"
        );
        *failure_class_counts
            .entry(failure_class.to_string())
            .or_insert(0usize) += 1;
    }

    assert_eq!(stage_counts.get("compile"), Some(&15usize));
    assert_eq!(stage_counts.get("test"), Some(&10usize));
    assert_eq!(stage_counts.get("lint"), Some(&10usize));
    assert!(
        !stage_counts.contains_key("parse"),
        "tier1 should exclude parse-stage cases"
    );

    for failure_class in [
        "type_mismatch",
        "missing_symbol",
        "unknown_field",
        "test_assertion",
        "logic_off_by_one",
        "clippy_needless_return",
        "clippy_identity_op",
    ] {
        assert_eq!(
            failure_class_counts.get(failure_class),
            Some(&5usize),
            "tier1 should keep five cases for `{failure_class}`"
        );
    }
}

#[test]
fn eval_tier2_manifest_matches_diagnostic_shape() {
    let full_manifest = manifest();
    let full_case_ids: BTreeSet<_> = full_manifest["cases"]
        .as_array()
        .expect("v0 cases must be an array")
        .iter()
        .map(|case| case["id"].as_str().expect("case id must be a string"))
        .collect();

    let manifest = tier2_manifest();
    assert_eq!(manifest["schema_version"], "mercury-evals-v0");
    assert_eq!(manifest["suite_id"], "rust-v0.3-tier2");
    assert_eq!(manifest["language"], "rust");
    assert_eq!(manifest["version"], 3);
    assert_eq!(
        manifest["description"],
        "Tier 2 Rust diagnostic lane covering parser, trait-bound, and panic-unwrap repairs that remain harder or unsupported in the current beta."
    );

    let supported_modes = manifest["supported_modes"]
        .as_array()
        .expect("supported_modes must be an array");
    assert_eq!(supported_modes.len(), 1);
    assert_eq!(supported_modes[0], "baseline");

    let cases = manifest["cases"]
        .as_array()
        .expect("cases must be an array");
    assert_eq!(
        cases.len(),
        15,
        "tier2 corpus should contain 15 logical case ids"
    );

    let mut ids = BTreeSet::new();
    let mut stage_counts = BTreeMap::new();
    let mut failure_class_counts = BTreeMap::new();
    for case in cases {
        let id = case["id"].as_str().expect("case id must be a string");
        assert!(ids.insert(id.to_string()), "tier2 case ids must be unique");
        assert!(
            full_case_ids.contains(id),
            "tier2 case `{id}` should exist in the seeded v0 corpus"
        );

        let stage = case["failure_stage"]
            .as_str()
            .expect("failure_stage must be a string");
        *stage_counts.entry(stage.to_string()).or_insert(0usize) += 1;

        let failure_class = case["failure_class"]
            .as_str()
            .expect("failure_class must be a string");
        assert!(
            matches!(failure_class, "parser" | "trait_bound" | "panic_unwrap"),
            "tier2 should keep only harder failure class `{failure_class}`"
        );
        *failure_class_counts
            .entry(failure_class.to_string())
            .or_insert(0usize) += 1;
    }

    assert_eq!(stage_counts.get("parse"), Some(&5usize));
    assert_eq!(stage_counts.get("compile"), Some(&5usize));
    assert_eq!(stage_counts.get("test"), Some(&5usize));

    for failure_class in ["parser", "trait_bound", "panic_unwrap"] {
        assert_eq!(
            failure_class_counts.get(failure_class),
            Some(&5usize),
            "tier2 should keep five cases for `{failure_class}`"
        );
    }
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
        .arg("--difficulty")
        .arg("easy")
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
    assert!(listed.iter().all(|case| case["difficulty"] == "easy"));
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
        "import hashlib",
        "def stable_pr_branch(base_ref: str, failure_command: str) -> str:",
        "pr_branch = stable_pr_branch(base_ref=base_ref, failure_command=failure_command)",
        "repo_root / \"target\" / \"release\" / \"mercury-cli\"",
        "inputs.dry_run != true",
        "Upload evidence bundle",
        "Validate evidence bundle contract",
        "summary.md",
        "decision.json",
        "environment.json",
        "pr-body.md",
        "summary-index.json",
        "failure_reason_rollup",
        "candidate_lineage",
        "winning_candidates",
        "decision_payload[\"mercury_run\"] = mercury_run",
        "## Mercury run summary",
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
        "evals/repair_benchmark/publish.py",
        "evals/v0/tier1-manifest.json",
        "--mode quality",
        "--mode agent-sweep",
        "--agent-count 1",
        "--agent-count 2",
        "--agent-count 4",
        "--agent-count 8",
        "report.json",
        "summary.md",
        "docs/benchmarks",
        "rust-v0-quality.report.json",
        "rust-v0-agent-sweep.report.json",
        "mercury-repair-benchmark-v1",
        "execution_diagnostics",
        "Publish Tier 1 Rust repair benchmark artifacts",
        "Render public benchmark surface",
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
    let quality_report_path = repo_root.join("docs/benchmarks/rust-v0-quality.report.json");
    let agent_sweep_report_path = repo_root.join("docs/benchmarks/rust-v0-agent-sweep.report.json");
    let cargo_toml =
        fs::read_to_string(repo_root.join("Cargo.toml")).expect("Cargo.toml should exist");
    let changelog =
        fs::read_to_string(repo_root.join("CHANGELOG.md")).expect("CHANGELOG should exist");
    let readme = fs::read_to_string(repo_root.join("README.md")).expect("README should exist");
    let architecture = fs::read_to_string(repo_root.join("docs/ARCHITECTURE.md"))
        .expect("architecture doc should exist");
    let quality =
        fs::read_to_string(repo_root.join("docs/QUALITY.md")).expect("quality doc should exist");
    let benchmark_readme = fs::read_to_string(repo_root.join("docs/benchmarks/README.md"))
        .expect("benchmark README should exist");
    let v0_readme =
        fs::read_to_string(repo_root.join("evals/v0/README.md")).expect("v0 README should exist");
    let operator_quickstart = fs::read_to_string(repo_root.join("docs/operator-quickstart.md"))
        .expect("operator quickstart should exist");
    let starter_index = fs::read_to_string(repo_root.join("starter-repos/README.md"))
        .expect("starter repo index should exist");
    let local_starter =
        fs::read_to_string(repo_root.join("starter-repos/local-rust-watch-repair/README.md"))
            .expect("local starter repo README should exist");
    let ci_starter =
        fs::read_to_string(repo_root.join("starter-repos/ci-draft-pr-repair/README.md"))
            .expect("ci starter repo README should exist");
    let ci_starter_workflow = fs::read_to_string(
        repo_root.join("starter-repos/ci-draft-pr-repair/.github/workflows/mercury-repair.yml"),
    )
    .expect("ci starter workflow should exist");
    let benchmark_report =
        fs::read_to_string(repo_root.join("docs/benchmarks/rust-v0-repair-benchmark.md"))
            .expect("benchmark report scaffold should exist");
    let benchmark_publisher =
        fs::read_to_string(repo_root.join("evals/repair_benchmark/README.md"))
            .expect("benchmark harness README should exist");
    let typescript_readme = fs::read_to_string(repo_root.join("evals/v1_typescript/README.md"))
        .expect("typescript README should exist");
    let benchmark_is_pending = benchmark_report.contains(PENDING_BENCHMARK_STATUS);
    let benchmark_is_published = benchmark_report.contains(PUBLISHED_BENCHMARK_STATUS);

    assert!(
        cargo_toml.contains("version = \"1.0.0-beta.1\""),
        "Cargo.toml should keep branch-head on the beta version"
    );
    assert!(
        changelog.contains("## [Unreleased]"),
        "CHANGELOG should keep unreleased work under the unreleased heading"
    );
    assert!(
        !changelog.contains("## [1.0.0]"),
        "CHANGELOG should not advertise a stable 1.0.0 release before the tag exists"
    );
    assert!(
        benchmark_is_pending ^ benchmark_is_published,
        "checked-in benchmark report should declare exactly one publication status"
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
    assert!(
        readme.contains("docs/operator-quickstart.md"),
        "README should link to the operator quickstart"
    );
    assert!(
        readme.contains("starter-repos/README.md"),
        "README should link to the starter repo index"
    );
    for expected in [
        "watch --repair",
        "artifact bundle",
        "summary-index.json",
        "failure reason rollup",
        "candidate lineage",
        "winning candidates",
        "verified_patch_ready",
        "repair_not_verified",
        "status --live",
        "starter-repos/",
    ] {
        assert!(
            operator_quickstart.contains(expected),
            "operator quickstart should explain `{expected}`"
        );
    }
    for expected in ["local-rust-watch-repair", "ci-draft-pr-repair", "Rust-only"] {
        assert!(
            starter_index.contains(expected),
            "starter repo index should describe `{expected}`"
        );
    }
    for expected in [
        "cargo test",
        "watch \"cargo test\" --repair",
        "summary-index.json",
    ] {
        assert!(
            local_starter.contains(expected),
            "local starter README should explain `{expected}`"
        );
    }
    for expected in [
        "workflow_dispatch",
        "repair is verified",
        "dry_run",
        "summary.md",
    ] {
        assert!(
            ci_starter.contains(expected),
            "ci starter README should explain `{expected}`"
        );
    }
    assert!(
        ci_starter_workflow
            .contains("uses: denster32/mercury-cli/.github/workflows/repair.yml@v1.0.0-beta.1"),
        "ci starter workflow should pin the reusable Mercury beta workflow"
    );

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

    for (label, text) in [
        ("README", readme.as_str()),
        ("QUALITY", quality.as_str()),
        ("docs/benchmarks/README.md", benchmark_readme.as_str()),
        (
            "evals/repair_benchmark/README.md",
            benchmark_publisher.as_str(),
        ),
    ] {
        assert!(
            text.contains("evals/repair_benchmark/publish.py"),
            "{label} should point to the benchmark publisher"
        );
    }

    if benchmark_is_published {
        for (label, path, expected_mode) in [
            (
                "docs/benchmarks/rust-v0-quality.report.json",
                quality_report_path.as_path(),
                "quality",
            ),
            (
                "docs/benchmarks/rust-v0-agent-sweep.report.json",
                agent_sweep_report_path.as_path(),
                "agent-sweep",
            ),
        ] {
            assert!(
                path.exists(),
                "{label} should exist when the benchmark is published"
            );
            let payload: Value = serde_json::from_str(
                &fs::read_to_string(path).expect("published benchmark json should be readable"),
            )
            .expect("published benchmark json should parse");
            assert_eq!(
                payload["schema_version"], "mercury-repair-benchmark-v1",
                "{label} should declare the benchmark schema"
            );
            assert_eq!(
                payload["mode"], expected_mode,
                "{label} should keep the published mode aligned"
            );
            assert_public_benchmark_report_is_scrubbed(&payload);
            assert!(
                payload["repair_outcome_distribution"].is_object(),
                "{label} should expose repair_outcome_distribution as an object"
            );
        }

        for (label, text) in [
            ("docs/benchmarks/README.md", benchmark_readme.as_str()),
            ("README", readme.as_str()),
            ("QUALITY", quality.as_str()),
            ("evals/v0/README.md", v0_readme.as_str()),
        ] {
            assert!(
                !text.contains("secret-backed Tier 1 Rust beta"),
                "{label} should stop describing the benchmark as unpublished once reports are checked in"
            );
            assert!(
                text.contains("rust-v0-quality.report.json"),
                "{label} should point to the checked-in quality aggregate"
            );
            assert!(
                text.contains("rust-v0-agent-sweep.report.json"),
                "{label} should point to the checked-in agent-sweep aggregate"
            );
            assert!(
                text.contains("evals/v0/tier1-manifest.json"),
                "{label} should describe the Tier 1 Rust beta manifest"
            );
            assert!(
                text.contains("evals/v0/tier0-manifest.json"),
                "{label} should describe the Tier 0 Rust diagnostic manifest"
            );
            assert!(
                text.contains("evals/v0/tier2-manifest.json"),
                "{label} should describe the Tier 2 Rust diagnostic manifest"
            );
        }
        assert!(
            readme.contains("Rust direct cargo verifier repair beta"),
            "README should describe the product as a Rust direct cargo verifier repair beta"
        );
        assert!(
            benchmark_readme.contains("repair outcome distribution"),
            "docs/benchmarks/README.md should describe the published repair outcome distribution"
        );
        assert!(
            benchmark_readme.contains("tier breakdowns"),
            "docs/benchmarks/README.md should describe the published tier breakdowns"
        );
        assert!(
            benchmark_readme.contains("verifier-class breakdowns"),
            "docs/benchmarks/README.md should describe the published verifier-class breakdowns"
        );
        assert!(
            benchmark_readme.contains("candidate lineage breakdowns"),
            "docs/benchmarks/README.md should describe the published candidate lineage breakdowns"
        );
        assert!(
            benchmark_readme.contains("cargo test")
                && benchmark_readme.contains("cargo check")
                && benchmark_readme.contains("cargo clippy"),
            "docs/benchmarks/README.md should call out the published Rust verifier classes"
        );
        assert!(
            benchmark_readme.contains("execution diagnostics"),
            "docs/benchmarks/README.md should describe the published execution diagnostics"
        );
        assert!(
            readme.contains("tier breakdowns")
                && readme.contains("candidate lineage")
                && readme.contains("cargo test")
                && readme.contains("cargo check")
                && readme.contains("cargo clippy"),
            "README should describe the published diagnostic benchmark slices"
        );
        assert!(
            quality.contains("tier breakdowns")
                && quality.contains("candidate lineage breakdowns")
                && quality.contains("cargo test")
                && quality.contains("cargo check")
                && quality.contains("cargo clippy"),
            "QUALITY should describe the published diagnostic benchmark slices"
        );
        assert!(
            benchmark_report.contains("## Repair Outcome Distribution"),
            "published benchmark markdown should include the repair outcome distribution section"
        );
        assert!(
            benchmark_report.contains("## Execution Diagnostics"),
            "published benchmark markdown should include the execution diagnostics section"
        );
    } else {
        for (label, text) in [
            ("docs/benchmarks/README.md", benchmark_readme.as_str()),
            ("README", readme.as_str()),
            ("QUALITY", quality.as_str()),
        ] {
            assert!(
                text.contains("secret-backed Tier 1 Rust beta"),
                "{label} should keep the checked-in benchmark truth aligned"
            );
        }
        assert!(
            !quality_report_path.exists(),
            "pending benchmark docs should not check in a stale quality report"
        );
        assert!(
            !agent_sweep_report_path.exists(),
            "pending benchmark docs should not check in a stale agent-sweep report"
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
fn release_workflow_requires_manual_version_to_match_manifest_version() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workflow = fs::read_to_string(repo_root.join(".github/workflows/release.yml"))
        .expect("release workflow should exist");

    let dispatch_block = workflow_input_block(&workflow, "workflow_dispatch", "version");
    assert!(
        dispatch_block.contains("must match Cargo.toml exactly"),
        "manual release input should document the manifest-version requirement"
    );

    for expected in [
        "MANIFEST_VERSION",
        "Cargo.toml",
        "version input is required for manual releases",
        "does not match Cargo.toml package version",
        "if [[ \"$VERSION\" == *-* ]]; then",
        "echo \"version=$VERSION\" >> \"$GITHUB_OUTPUT\"",
        "echo \"prerelease=$PRERELEASE\" >> \"$GITHUB_OUTPUT\"",
        "prerelease: ${{ steps.version.outputs.prerelease == 'true' }}",
        "evals/v0/tier0-manifest.json",
        "evals/v0/tier1-manifest.json",
        "evals/v0/tier2-manifest.json",
    ] {
        assert!(
            workflow.contains(expected),
            "release workflow should contain `{expected}`"
        );
    }
}

#[test]
fn repair_benchmark_checked_in_report_matches_generator_output() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let checked_in =
        fs::read_to_string(repo_root.join("docs/benchmarks/rust-v0-repair-benchmark.md"))
            .expect("checked-in benchmark report should exist");
    let temp = tempfile::tempdir().expect("tempdir should be created");
    let output_dir = temp.path().join("published");

    let mut command = Command::new("python3");
    command.arg("evals/repair_benchmark/publish.py");
    if checked_in.contains(PUBLISHED_BENCHMARK_STATUS) {
        command
            .arg("--quality-report")
            .arg(repo_root.join("docs/benchmarks/rust-v0-quality.report.json"))
            .arg("--agent-sweep-report")
            .arg(repo_root.join("docs/benchmarks/rust-v0-agent-sweep.report.json"));
    } else {
        command.arg("--pending");
    }
    let output = command
        .arg("--output-dir")
        .arg(output_dir.as_os_str())
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("python3 should execute benchmark publisher");
    assert!(
        output.status.success(),
        "checked-in benchmark publisher invocation should succeed"
    );

    let generated = fs::read_to_string(output_dir.join("rust-v0-repair-benchmark.md"))
        .expect("generated benchmark report should exist");

    assert_eq!(
        generated, checked_in,
        "checked-in benchmark report should stay in sync with the publisher"
    );

    if checked_in.contains(PUBLISHED_BENCHMARK_STATUS) {
        for file_name in [
            "rust-v0-quality.report.json",
            "rust-v0-agent-sweep.report.json",
        ] {
            let generated = fs::read_to_string(output_dir.join(file_name))
                .expect("generated published benchmark json should exist");
            let checked_in = fs::read_to_string(repo_root.join("docs/benchmarks").join(file_name))
                .expect("checked-in published benchmark json should exist");
            assert_eq!(
                generated, checked_in,
                "checked-in {file_name} should stay in sync with the publisher inputs"
            );
        }
    } else {
        assert!(
            !output_dir.join("rust-v0-quality.report.json").exists(),
            "pending benchmark publish should not emit a quality report"
        );
        assert!(
            !output_dir.join("rust-v0-agent-sweep.report.json").exists(),
            "pending benchmark publish should not emit an agent-sweep report"
        );
    }
}

#[test]
fn repair_benchmark_pending_publish_clears_stale_public_reports() {
    let temp = tempfile::tempdir().expect("tempdir should be created");
    let output_dir = temp.path().join("published");
    fs::create_dir_all(&output_dir).expect("output directory should be creatable");
    fs::write(
        output_dir.join("rust-v0-quality.report.json"),
        "{\"stale\":true}\n",
    )
    .expect("stale quality report should be writable");
    fs::write(
        output_dir.join("rust-v0-agent-sweep.report.json"),
        "{\"stale\":true}\n",
    )
    .expect("stale agent-sweep report should be writable");

    let output = Command::new("python3")
        .arg("evals/repair_benchmark/publish.py")
        .arg("--pending")
        .arg("--output-dir")
        .arg(output_dir.as_os_str())
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("python3 should execute benchmark publisher");
    assert!(
        output.status.success(),
        "pending benchmark publisher should succeed"
    );

    assert!(
        !output_dir.join("rust-v0-quality.report.json").exists(),
        "pending benchmark publish should remove stale quality report output"
    );
    assert!(
        !output_dir.join("rust-v0-agent-sweep.report.json").exists(),
        "pending benchmark publish should remove stale agent-sweep report output"
    );
    assert!(
        output_dir.join("rust-v0-repair-benchmark.md").exists(),
        "pending benchmark publish should still render the pending markdown"
    );
}

#[test]
fn repair_benchmark_publish_script_renders_public_surface_from_reports() {
    let temp = tempfile::tempdir().expect("tempdir should be created");
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let output_dir = temp.path().join("published");
    let quality_path = temp.path().join("quality.report.json");
    let agent_sweep_path = temp.path().join("agent-sweep.report.json");

    let quality_report = json!({
        "schema_version": "mercury-repair-benchmark-v1",
        "description": "Public Mercury repair benchmark aggregate report",
        "suite_id": "rust-v0.3-tier1",
        "language": "rust",
        "mode": "quality",
        "generated_at": "2026-03-11T00:00:00Z",
        "run_id": "quality-demo",
        "run_root": "/tmp/quality",
        "binary_path": "/tmp/mercury-cli",
        "agent_counts": [4],
        "max_cost_usd": 0.5,
        "timeout_seconds": 300,
        "api_key_env": "INCEPTION_API_KEY",
        "manifest": {
            "schema_version": "mercury-evals-v0",
            "version": 3,
            "artifact_schema_version": "mercury-eval-report-v0",
            "supported_modes": ["baseline"]
        },
        "selection": {
            "manifest_path": repo_root.join("evals/v0/tier1-manifest.json"),
            "selected_count": 10,
            "selected_case_ids": ["rust_type_mismatch"],
            "selected_unique_fixture_paths": 2,
            "requested_limit": serde_json::Value::Null,
            "requested_stages": ["compile"],
            "requested_difficulties": []
        },
        "started_at": "2026-03-11T00:00:00Z",
        "finished_at": "2026-03-11T00:05:00Z",
        "duration_ms": 300000,
        "metrics": {
            "attempted_cases": 10,
            "verified_repairs": 7,
            "accepted_patches": 8,
            "false_greens": 1,
            "verified_repair_rate": 0.7,
            "accepted_patch_rate": 0.8,
            "false_green_rate": 0.1,
            "median_time_to_first_candidate_ms": 1200,
            "median_time_to_verified_repair_ms": 4200,
            "median_cost_per_attempted_case_usd": 0.12,
            "mean_cost_per_attempted_case_usd": 0.15,
            "median_cost_per_verified_repair_usd": 0.18,
            "mean_cost_per_verified_repair_usd": 0.21
        },
        "failure_attribution": {},
        "execution_diagnostics": {
            "generation_failures": 0,
            "safety_failures": 0,
            "candidate_verification_failures": 1,
            "final_bundle_failures": 0
        },
        "speedup_curve": [{
            "agent_count": 4,
            "attempted_cases": 10,
            "verified_repairs": 7,
            "median_duration_ms": 5100,
            "median_time_to_verified_repair_ms": 4200,
            "speedup_vs_baseline": 1.0
        }],
        "cost_curve": [{
            "agent_count": 4,
            "attempted_cases": 10,
            "median_total_cost_usd": 0.12,
            "mean_total_cost_usd": 0.15
        }],
        "results": [{
            "case_id": "rust_type_mismatch",
            "agent_count": 4,
            "outcome": "verified_repair",
            "verified_repair": true,
            "difficulty": "easy",
            "tier": "Tier 1",
            "verifier_class": "cargo_test",
            "accepted_patch_bytes": 128,
            "generation_failures": 0,
            "safety_failures": 0,
            "candidate_verification_failures": 1,
            "final_bundle_failures": 0,
            "apply_edit_attempts": 2,
            "grounded_next_edit_attempts": 0,
            "critique_retry_attempts": 0,
            "exploratory_next_edit_attempts": 0,
            "apply_edit_accepted_steps": 1,
            "grounded_next_edit_accepted_steps": 0,
            "critique_retry_accepted_steps": 0,
            "exploratory_next_edit_accepted_steps": 0,
            "benchmark_run_path": "/tmp/quality/cases/rust_type_mismatch/agents-4/benchmark-run.json",
            "candidate_workspace": "/tmp/quality/workspaces/rust_type_mismatch-agents-4"
        }]
    });
    let agent_sweep_report = json!({
        "schema_version": "mercury-repair-benchmark-v1",
        "description": "Public Mercury repair benchmark aggregate report",
        "suite_id": "rust-v0.3-tier1",
        "language": "rust",
        "mode": "agent-sweep",
        "generated_at": "2026-03-11T01:00:00Z",
        "run_id": "sweep-demo",
        "run_root": "/tmp/sweep",
        "binary_path": "/tmp/mercury-cli",
        "agent_counts": [1, 2, 4, 8],
        "max_cost_usd": 0.5,
        "timeout_seconds": 300,
        "api_key_env": "INCEPTION_API_KEY",
        "manifest": {
            "schema_version": "mercury-evals-v0",
            "version": 3,
            "artifact_schema_version": "mercury-eval-report-v0",
            "supported_modes": ["baseline"]
        },
        "selection": {
            "manifest_path": repo_root.join("evals/v0/tier1-manifest.json"),
            "selected_count": 4,
            "selected_case_ids": ["rust_type_mismatch"],
            "selected_unique_fixture_paths": 2,
            "requested_limit": 4,
            "requested_stages": ["compile", "test"],
            "requested_difficulties": []
        },
        "started_at": "2026-03-11T01:00:00Z",
        "finished_at": "2026-03-11T01:05:00Z",
        "duration_ms": 300000,
        "metrics": {
            "attempted_cases": 16,
            "verified_repairs": 10,
            "accepted_patches": 11,
            "false_greens": 2,
            "verified_repair_rate": 0.625,
            "accepted_patch_rate": 0.6875,
            "false_green_rate": 0.125,
            "median_time_to_first_candidate_ms": 1100,
            "median_time_to_verified_repair_ms": 3900,
            "median_cost_per_attempted_case_usd": 0.11,
            "mean_cost_per_attempted_case_usd": 0.13,
            "median_cost_per_verified_repair_usd": 0.16,
            "mean_cost_per_verified_repair_usd": 0.19
        },
        "failure_attribution": {
            "candidate_failed_verifier": 1
        },
        "execution_diagnostics": {
            "generation_failures": 1,
            "safety_failures": 0,
            "candidate_verification_failures": 3,
            "final_bundle_failures": 1
        },
        "speedup_curve": [
            {
                "agent_count": 1,
                "attempted_cases": 4,
                "verified_repairs": 2,
                "median_duration_ms": 8000,
                "median_time_to_verified_repair_ms": 6000,
                "speedup_vs_baseline": 1.0
            },
            {
                "agent_count": 2,
                "attempted_cases": 4,
                "verified_repairs": 3,
                "median_duration_ms": 6500,
                "median_time_to_verified_repair_ms": 5000,
                "speedup_vs_baseline": 1.231
            },
            {
                "agent_count": 4,
                "attempted_cases": 4,
                "verified_repairs": 3,
                "median_duration_ms": 5200,
                "median_time_to_verified_repair_ms": 4100,
                "speedup_vs_baseline": 1.538
            },
            {
                "agent_count": 8,
                "attempted_cases": 4,
                "verified_repairs": 2,
                "median_duration_ms": 5000,
                "median_time_to_verified_repair_ms": 4300,
                "speedup_vs_baseline": 1.6
            }
        ],
        "cost_curve": [
            {
                "agent_count": 1,
                "attempted_cases": 4,
                "median_total_cost_usd": 0.09,
                "mean_total_cost_usd": 0.1
            },
            {
                "agent_count": 2,
                "attempted_cases": 4,
                "median_total_cost_usd": 0.1,
                "mean_total_cost_usd": 0.11
            },
            {
                "agent_count": 4,
                "attempted_cases": 4,
                "median_total_cost_usd": 0.12,
                "mean_total_cost_usd": 0.13
            },
            {
                "agent_count": 8,
                "attempted_cases": 4,
                "median_total_cost_usd": 0.16,
                "mean_total_cost_usd": 0.17
            }
        ],
        "results": [{
            "case_id": "rust_type_mismatch",
            "agent_count": 8,
            "outcome": "accepted_patch_unverified",
            "verified_repair": false,
            "failure_attribution": "candidate_failed_verifier",
            "difficulty": "medium",
            "tier": 2,
            "verifier_class": "cargo_check",
            "accepted_patch_bytes": 64,
            "generation_failures": 1,
            "safety_failures": 0,
            "candidate_verification_failures": 3,
            "final_bundle_failures": 1,
            "apply_edit_attempts": 0,
            "grounded_next_edit_attempts": 2,
            "critique_retry_attempts": 1,
            "exploratory_next_edit_attempts": 0,
            "apply_edit_accepted_steps": 0,
            "grounded_next_edit_accepted_steps": 1,
            "critique_retry_accepted_steps": 0,
            "exploratory_next_edit_accepted_steps": 0,
            "benchmark_run_path": "/tmp/sweep/cases/rust_type_mismatch/agents-8/benchmark-run.json",
            "candidate_workspace": "/tmp/sweep/workspaces/rust_type_mismatch-agents-8"
        }]
    });

    fs::write(
        &quality_path,
        serde_json::to_vec_pretty(&quality_report).expect("quality report should serialize"),
    )
    .expect("quality report should be writable");
    fs::write(
        &agent_sweep_path,
        serde_json::to_vec_pretty(&agent_sweep_report)
            .expect("agent sweep report should serialize"),
    )
    .expect("agent sweep report should be writable");

    let output = Command::new("python3")
        .arg("evals/repair_benchmark/publish.py")
        .arg("--quality-report")
        .arg(quality_path.as_os_str())
        .arg("--agent-sweep-report")
        .arg(agent_sweep_path.as_os_str())
        .arg("--output-dir")
        .arg(output_dir.as_os_str())
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("python3 should execute benchmark publisher");
    assert!(
        output.status.success(),
        "benchmark publisher should succeed"
    );

    let markdown = fs::read_to_string(output_dir.join("rust-v0-repair-benchmark.md"))
        .expect("published benchmark markdown should exist");
    assert!(
        markdown.contains("Status: published from benchmark runner artifacts."),
        "published benchmark markdown should declare published status"
    );
    assert!(
        markdown.contains("quality-demo"),
        "published benchmark markdown should include quality run metadata"
    );
    assert!(
        markdown.contains("evals/v0/tier1-manifest.json"),
        "published benchmark markdown should include the Tier 1 manifest path"
    );
    assert!(
        markdown.contains("## Execution Diagnostics"),
        "published benchmark markdown should include the execution diagnostics section"
    );
    assert!(
        markdown.contains("| candidate_verification_failures | 1 | 3 |"),
        "published benchmark markdown should render the execution diagnostics table"
    );
    assert!(
        markdown.contains("## Tier Breakdown"),
        "published benchmark markdown should include the tier breakdown section"
    );
    assert!(
        markdown.contains("## Verifier-Class Breakdown"),
        "published benchmark markdown should include the verifier-class breakdown section"
    );
    assert!(
        markdown.contains("## Candidate Lineage Breakdown"),
        "published benchmark markdown should include the candidate-lineage breakdown section"
    );
    assert!(
        markdown.contains("## Candidate Lineage Attempts"),
        "published benchmark markdown should include the candidate-lineage attempts section"
    );
    assert!(
        markdown.contains("| tier1 | 1 | 1 | 0 | 0 | 1.000 | 0.000 | 0.000 | verified_repair=1 |"),
        "published benchmark markdown should render normalized tier breakdown rows"
    );
    assert!(
        markdown
            .contains("| cargo_test | 1 | 1 | 0 | 0 | 1.000 | 0.000 | 0.000 | verified_repair=1 |"),
        "published benchmark markdown should render verifier-class breakdown rows"
    );
    assert!(
        markdown.contains("| apply_edit | 2 | 1 | 0 | 0 |"),
        "published benchmark markdown should render candidate-lineage attempt totals"
    );
    assert!(
        markdown.contains("| 8 | 4 | 2 | 5000 | 4300 | 1.600 |"),
        "published benchmark markdown should render the speedup curve table"
    );

    let copied_quality = fs::read_to_string(output_dir.join("rust-v0-quality.report.json"))
        .expect("copied quality report should exist");
    let copied_agent_sweep = fs::read_to_string(output_dir.join("rust-v0-agent-sweep.report.json"))
        .expect("copied agent sweep report should exist");
    let copied_quality: Value =
        serde_json::from_str(&copied_quality).expect("copied quality report should parse");
    let copied_agent_sweep: Value =
        serde_json::from_str(&copied_agent_sweep).expect("copied agent sweep report should parse");
    assert_eq!(copied_quality["run_id"], "quality-demo");
    assert_eq!(copied_agent_sweep["run_id"], "sweep-demo");
    assert_eq!(
        copied_quality["selection"]["manifest_path"],
        "evals/v0/tier1-manifest.json"
    );
    assert_eq!(
        copied_agent_sweep["selection"]["manifest_path"],
        "evals/v0/tier1-manifest.json"
    );
    assert_eq!(
        copied_quality["execution_diagnostics"]["candidate_verification_failures"],
        1
    );
    assert_eq!(
        copied_agent_sweep["execution_diagnostics"]["generation_failures"],
        1
    );
    assert_eq!(
        copied_quality["tier_breakdown"]["tier1"]["attempted_cases"],
        1
    );
    assert_eq!(
        copied_agent_sweep["tier_breakdown"]["tier2"]["attempted_cases"],
        1
    );
    assert_eq!(
        copied_quality["verifier_class_breakdown"]["cargo_test"]["attempted_cases"],
        1
    );
    assert_eq!(
        copied_agent_sweep["verifier_class_breakdown"]["cargo_check"]["attempted_cases"],
        1
    );
    assert_eq!(
        copied_quality["candidate_lineage_breakdown"]["apply_edit"]["attempted_cases"],
        1
    );
    assert_eq!(
        copied_agent_sweep["candidate_lineage_breakdown"]["grounded_next_edit"]["attempted_cases"],
        1
    );
    assert_eq!(
        copied_quality["candidate_attempt_breakdown"]["apply_edit"]["attempts"],
        2
    );
    assert_eq!(
        copied_quality["candidate_attempt_breakdown"]["apply_edit"]["accepted_steps"],
        1
    );
    assert_eq!(
        copied_agent_sweep["candidate_attempt_breakdown"]["grounded_next_edit"]["attempts"],
        2
    );
    assert_eq!(
        copied_agent_sweep["candidate_attempt_breakdown"]["grounded_next_edit"]["accepted_steps"],
        1
    );
    assert_eq!(
        copied_agent_sweep["failure_attribution"]["candidate_failed_verifier"],
        1
    );
    assert_public_benchmark_report_is_scrubbed(&copied_quality);
    assert_public_benchmark_report_is_scrubbed(&copied_agent_sweep);
}

#[test]
fn repair_benchmark_publish_script_backfills_legacy_metadata_from_manifest_and_benchmark_runs() {
    let temp = tempfile::tempdir().expect("tempdir should be created");
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let output_dir = temp.path().join("published");
    let quality_run_dir = temp.path().join("quality-run");
    let agent_run_dir = temp.path().join("agent-run");
    let quality_path = quality_run_dir.join("report.json");
    let agent_sweep_path = agent_run_dir.join("report.json");
    let quality_case_dir = quality_run_dir
        .join("cases")
        .join("rust_type_mismatch")
        .join("agents-4");
    let agent_case_dir = agent_run_dir
        .join("cases")
        .join("rust_type_mismatch")
        .join("agents-8");

    fs::create_dir_all(&quality_case_dir).expect("quality case dir should be creatable");
    fs::create_dir_all(&agent_case_dir).expect("agent case dir should be creatable");

    let quality_benchmark_run = json!({
        "schema_version": "mercury-repair-benchmark-run-v1",
        "accepted_patch_bytes": 72,
        "generation_failures": 0,
        "safety_failures": 1,
        "candidate_verification_failures": 2,
        "final_bundle_failures": 0,
        "critique_retry_attempts": 3,
        "critique_retry_accepted_steps": 1,
        "time_to_first_candidate_ms": 2100,
        "time_to_verified_repair_ms": 6100,
        "total_cost_usd": 0.14,
        "failure_attribution": "candidate_failed_verifier",
        "outcome": "accepted_patch_unverified"
    });
    let agent_benchmark_run = json!({
        "schema_version": "mercury-repair-benchmark-run-v1",
        "accepted_patch_bytes": 0,
        "generation_failures": 1,
        "safety_failures": 0,
        "candidate_verification_failures": 0,
        "final_bundle_failures": 0,
        "failure_attribution": "patch_generation_failed",
        "outcome": "no_patch"
    });

    fs::write(
        quality_case_dir.join("benchmark-run.json"),
        serde_json::to_vec_pretty(&quality_benchmark_run)
            .expect("quality benchmark run should serialize"),
    )
    .expect("quality benchmark run should be writable");
    fs::write(
        agent_case_dir.join("benchmark-run.json"),
        serde_json::to_vec_pretty(&agent_benchmark_run)
            .expect("agent benchmark run should serialize"),
    )
    .expect("agent benchmark run should be writable");

    let quality_report = json!({
        "schema_version": "mercury-repair-benchmark-v1",
        "description": "Legacy aggregate report missing tier and verifier metadata",
        "suite_id": "rust-v0.3-tier1",
        "language": "rust",
        "mode": "quality",
        "generated_at": "2026-03-13T00:00:00Z",
        "run_id": "quality-legacy",
        "run_root": quality_run_dir,
        "binary_path": "/tmp/mercury-cli",
        "agent_counts": [4],
        "max_cost_usd": 0.5,
        "timeout_seconds": 300,
        "api_key_env": "MERCURY_API_KEY",
        "manifest": {
            "schema_version": "mercury-evals-v0",
            "version": 3,
            "artifact_schema_version": "mercury-eval-report-v0",
            "supported_modes": ["baseline"]
        },
        "selection": {
            "manifest_path": repo_root.join("evals/v0/tier1-manifest.json"),
            "selected_count": 1,
            "selected_case_ids": ["rust_type_mismatch"],
            "selected_unique_fixture_paths": 1,
            "requested_limit": serde_json::Value::Null,
            "requested_stages": ["compile"],
            "requested_difficulties": []
        },
        "started_at": "2026-03-13T00:00:00Z",
        "finished_at": "2026-03-13T00:05:00Z",
        "duration_ms": 300000,
        "metrics": {
            "attempted_cases": 1,
            "verified_repairs": 0,
            "accepted_patches": 1,
            "false_greens": 0,
            "verified_repair_rate": 0.0,
            "accepted_patch_rate": 1.0,
            "false_green_rate": 0.0,
            "median_time_to_first_candidate_ms": 2100,
            "median_time_to_verified_repair_ms": serde_json::Value::Null,
            "median_cost_per_attempted_case_usd": 0.14,
            "mean_cost_per_attempted_case_usd": 0.14,
            "median_cost_per_verified_repair_usd": serde_json::Value::Null,
            "mean_cost_per_verified_repair_usd": serde_json::Value::Null
        },
        "failure_attribution": {},
        "execution_diagnostics": {
            "generation_failures": 0,
            "safety_failures": 0,
            "candidate_verification_failures": 0,
            "final_bundle_failures": 0
        },
        "speedup_curve": [{
            "agent_count": 4,
            "attempted_cases": 1,
            "verified_repairs": 0,
            "median_duration_ms": 6100,
            "median_time_to_verified_repair_ms": serde_json::Value::Null,
            "speedup_vs_baseline": 1.0
        }],
        "cost_curve": [{
            "agent_count": 4,
            "attempted_cases": 1,
            "median_total_cost_usd": 0.14,
            "mean_total_cost_usd": 0.14
        }],
        "results": [{
            "case_id": "rust_type_mismatch",
            "agent_count": 4,
            "outcome": "accepted_patch_unverified",
            "verified_repair": false,
            "difficulty": "medium",
            "tier": serde_json::Value::Null,
            "verifier_class": serde_json::Value::Null
        }]
    });
    let agent_sweep_report = json!({
        "schema_version": "mercury-repair-benchmark-v1",
        "description": "Legacy agent-sweep report missing lineage counters",
        "suite_id": "rust-v0.3-tier1",
        "language": "rust",
        "mode": "agent-sweep",
        "generated_at": "2026-03-13T01:00:00Z",
        "run_id": "agent-legacy",
        "run_root": agent_run_dir,
        "binary_path": "/tmp/mercury-cli",
        "agent_counts": [8],
        "max_cost_usd": 0.5,
        "timeout_seconds": 300,
        "api_key_env": "MERCURY_API_KEY",
        "manifest": {
            "schema_version": "mercury-evals-v0",
            "version": 3,
            "artifact_schema_version": "mercury-eval-report-v0",
            "supported_modes": ["baseline"]
        },
        "selection": {
            "manifest_path": repo_root.join("evals/v0/tier1-manifest.json"),
            "selected_count": 1,
            "selected_case_ids": ["rust_type_mismatch"],
            "selected_unique_fixture_paths": 1,
            "requested_limit": 1,
            "requested_stages": ["compile"],
            "requested_difficulties": []
        },
        "started_at": "2026-03-13T01:00:00Z",
        "finished_at": "2026-03-13T01:05:00Z",
        "duration_ms": 300000,
        "metrics": {
            "attempted_cases": 1,
            "verified_repairs": 0,
            "accepted_patches": 0,
            "false_greens": 0,
            "verified_repair_rate": 0.0,
            "accepted_patch_rate": 0.0,
            "false_green_rate": 0.0,
            "median_time_to_first_candidate_ms": serde_json::Value::Null,
            "median_time_to_verified_repair_ms": serde_json::Value::Null,
            "median_cost_per_attempted_case_usd": 0.03,
            "mean_cost_per_attempted_case_usd": 0.03,
            "median_cost_per_verified_repair_usd": serde_json::Value::Null,
            "mean_cost_per_verified_repair_usd": serde_json::Value::Null
        },
        "failure_attribution": {},
        "execution_diagnostics": {
            "generation_failures": 0,
            "safety_failures": 0,
            "candidate_verification_failures": 0,
            "final_bundle_failures": 0
        },
        "speedup_curve": [{
            "agent_count": 8,
            "attempted_cases": 1,
            "verified_repairs": 0,
            "median_duration_ms": 1900,
            "median_time_to_verified_repair_ms": serde_json::Value::Null,
            "speedup_vs_baseline": 1.0
        }],
        "cost_curve": [{
            "agent_count": 8,
            "attempted_cases": 1,
            "median_total_cost_usd": 0.03,
            "mean_total_cost_usd": 0.03
        }],
        "results": [{
            "case_id": "rust_type_mismatch",
            "agent_count": 8,
            "outcome": "no_patch",
            "verified_repair": false,
            "difficulty": "medium",
            "tier": serde_json::Value::Null,
            "verifier_class": serde_json::Value::Null
        }]
    });

    fs::write(
        &quality_path,
        serde_json::to_vec_pretty(&quality_report).expect("quality report should serialize"),
    )
    .expect("quality report should be writable");
    fs::write(
        &agent_sweep_path,
        serde_json::to_vec_pretty(&agent_sweep_report)
            .expect("agent sweep report should serialize"),
    )
    .expect("agent sweep report should be writable");

    let output = Command::new("python3")
        .arg("evals/repair_benchmark/publish.py")
        .arg("--quality-report")
        .arg(quality_path.as_os_str())
        .arg("--agent-sweep-report")
        .arg(agent_sweep_path.as_os_str())
        .arg("--output-dir")
        .arg(output_dir.as_os_str())
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("python3 should execute benchmark publisher");
    assert!(
        output.status.success(),
        "legacy benchmark publisher should succeed: stdout=`{}` stderr=`{}`",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let copied_quality: Value = serde_json::from_str(
        &fs::read_to_string(output_dir.join("rust-v0-quality.report.json"))
            .expect("copied quality report should exist"),
    )
    .expect("copied quality report should parse");
    let copied_agent_sweep: Value = serde_json::from_str(
        &fs::read_to_string(output_dir.join("rust-v0-agent-sweep.report.json"))
            .expect("copied agent sweep report should exist"),
    )
    .expect("copied agent sweep report should parse");

    assert_eq!(
        copied_quality["selection"]["manifest_path"],
        "evals/v0/tier1-manifest.json"
    );
    assert_eq!(copied_quality["results"][0]["tier"], "tier1");
    assert_eq!(copied_quality["results"][0]["verifier_class"], "cargo_check");
    assert_eq!(copied_quality["results"][0]["candidate_lineage"], "critique_retry");
    assert_eq!(copied_quality["results"][0]["accepted_patch_bytes"], 72);
    assert_eq!(copied_quality["results"][0]["safety_failures"], 1);
    assert_eq!(copied_quality["results"][0]["candidate_verification_failures"], 2);
    assert_eq!(
        copied_quality["candidate_lineage_breakdown"]["critique_retry"]["attempted_cases"],
        1
    );
    assert_eq!(
        copied_quality["candidate_attempt_breakdown"]["critique_retry"]["attempts"],
        3
    );
    assert_eq!(
        copied_quality["candidate_attempt_breakdown"]["critique_retry"]["accepted_steps"],
        1
    );

    assert_eq!(copied_agent_sweep["results"][0]["tier"], "tier1");
    assert_eq!(copied_agent_sweep["results"][0]["verifier_class"], "cargo_check");
    assert_eq!(copied_agent_sweep["results"][0]["candidate_lineage"], "unknown");
    assert_eq!(
        copied_agent_sweep["candidate_lineage_breakdown"]["unknown"]["attempted_cases"],
        1
    );
    assert_eq!(
        copied_agent_sweep["candidate_attempt_breakdown"]["apply_edit"]["attempts"],
        0
    );
    assert_eq!(
        copied_agent_sweep["failure_attribution"]["patch_generation_failed"],
        1
    );

    assert_public_benchmark_report_is_scrubbed(&copied_quality);
    assert_public_benchmark_report_is_scrubbed(&copied_agent_sweep);
}

#[test]
fn repair_benchmark_publish_script_backfills_canonical_manifest_tier_and_lineage_metadata() {
    let temp = tempfile::tempdir().expect("tempdir should be created");
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let output_dir = temp.path().join("published");
    let quality_run_dir = temp.path().join("quality-run");
    let agent_run_dir = temp.path().join("agent-run");
    let quality_path = quality_run_dir.join("report.json");
    let agent_sweep_path = agent_run_dir.join("report.json");
    let quality_case_dir = quality_run_dir
        .join("cases")
        .join("rust_runtime_assertion_failure")
        .join("agents-2");
    let agent_case_dir = agent_run_dir
        .join("cases")
        .join("rust_runtime_assertion_failure")
        .join("agents-4");

    fs::create_dir_all(&quality_case_dir).expect("quality case dir should be creatable");
    fs::create_dir_all(&agent_case_dir).expect("agent case dir should be creatable");

    let quality_benchmark_run = json!({
        "schema_version": "mercury-repair-benchmark-run-v1",
        "accepted_patch_bytes": 48,
        "generation_failures": 0,
        "safety_failures": 0,
        "candidate_verification_failures": 1,
        "final_bundle_failures": 0,
        "apply_edit_attempts": 2,
        "apply_edit_accepted_steps": 1,
        "time_to_first_candidate_ms": 1200,
        "time_to_verified_repair_ms": 3400,
        "total_cost_usd": 0.09,
        "failure_attribution": "candidate_failed_verifier",
        "outcome": "accepted_patch_unverified"
    });
    let agent_benchmark_run = json!({
        "schema_version": "mercury-repair-benchmark-run-v1",
        "accepted_patch_bytes": 0,
        "generation_failures": 1,
        "safety_failures": 0,
        "candidate_verification_failures": 0,
        "final_bundle_failures": 0,
        "failure_attribution": "patch_generation_failed",
        "outcome": "no_patch"
    });

    fs::write(
        quality_case_dir.join("benchmark-run.json"),
        serde_json::to_vec_pretty(&quality_benchmark_run)
            .expect("quality benchmark run should serialize"),
    )
    .expect("quality benchmark run should be writable");
    fs::write(
        agent_case_dir.join("benchmark-run.json"),
        serde_json::to_vec_pretty(&agent_benchmark_run)
            .expect("agent benchmark run should serialize"),
    )
    .expect("agent benchmark run should be writable");

    let quality_report = json!({
        "schema_version": "mercury-repair-benchmark-v1",
        "description": "Legacy canonical report missing tier and lineage metadata",
        "suite_id": "rust-v0.3-seeded",
        "language": "rust",
        "mode": "quality",
        "generated_at": "2026-03-13T02:00:00Z",
        "run_id": "quality-canonical-legacy",
        "run_root": quality_run_dir,
        "binary_path": "/tmp/mercury-cli",
        "agent_counts": [2],
        "max_cost_usd": 0.5,
        "timeout_seconds": 300,
        "api_key_env": "MERCURY_API_KEY",
        "manifest": {
            "schema_version": "mercury-evals-v0",
            "version": 3,
            "artifact_schema_version": "mercury-eval-report-v0",
            "supported_modes": ["baseline"]
        },
        "selection": {
            "manifest_path": repo_root.join("evals/v0/manifest.json"),
            "selected_count": 1,
            "selected_case_ids": ["rust_runtime_assertion_failure"],
            "selected_unique_fixture_paths": 1,
            "requested_limit": serde_json::Value::Null,
            "requested_stages": ["test"],
            "requested_difficulties": []
        },
        "started_at": "2026-03-13T02:00:00Z",
        "finished_at": "2026-03-13T02:05:00Z",
        "duration_ms": 300000,
        "metrics": {
            "attempted_cases": 1,
            "verified_repairs": 0,
            "accepted_patches": 1,
            "false_greens": 0,
            "verified_repair_rate": 0.0,
            "accepted_patch_rate": 1.0,
            "false_green_rate": 0.0,
            "median_time_to_first_candidate_ms": 1200,
            "median_time_to_verified_repair_ms": serde_json::Value::Null,
            "median_cost_per_attempted_case_usd": 0.09,
            "mean_cost_per_attempted_case_usd": 0.09,
            "median_cost_per_verified_repair_usd": serde_json::Value::Null,
            "mean_cost_per_verified_repair_usd": serde_json::Value::Null
        },
        "failure_attribution": {},
        "execution_diagnostics": {
            "generation_failures": 0,
            "safety_failures": 0,
            "candidate_verification_failures": 0,
            "final_bundle_failures": 0
        },
        "speedup_curve": [{
            "agent_count": 2,
            "attempted_cases": 1,
            "verified_repairs": 0,
            "median_duration_ms": 3400,
            "median_time_to_verified_repair_ms": serde_json::Value::Null,
            "speedup_vs_baseline": 1.0
        }],
        "cost_curve": [{
            "agent_count": 2,
            "attempted_cases": 1,
            "median_total_cost_usd": 0.09,
            "mean_total_cost_usd": 0.09
        }],
        "results": [{
            "case_id": "rust_runtime_assertion_failure",
            "agent_count": 2,
            "outcome": "accepted_patch_unverified",
            "verified_repair": false,
            "difficulty": "easy",
            "tier": serde_json::Value::Null,
            "verifier_class": serde_json::Value::Null
        }]
    });
    let agent_sweep_report = json!({
        "schema_version": "mercury-repair-benchmark-v1",
        "description": "Legacy canonical agent-sweep report missing tier metadata",
        "suite_id": "rust-v0.3-seeded",
        "language": "rust",
        "mode": "agent-sweep",
        "generated_at": "2026-03-13T02:10:00Z",
        "run_id": "agent-canonical-legacy",
        "run_root": agent_run_dir,
        "binary_path": "/tmp/mercury-cli",
        "agent_counts": [4],
        "max_cost_usd": 0.5,
        "timeout_seconds": 300,
        "api_key_env": "MERCURY_API_KEY",
        "manifest": {
            "schema_version": "mercury-evals-v0",
            "version": 3,
            "artifact_schema_version": "mercury-eval-report-v0",
            "supported_modes": ["baseline"]
        },
        "selection": {
            "manifest_path": repo_root.join("evals/v0/manifest.json"),
            "selected_count": 1,
            "selected_case_ids": ["rust_runtime_assertion_failure"],
            "selected_unique_fixture_paths": 1,
            "requested_limit": 1,
            "requested_stages": ["test"],
            "requested_difficulties": []
        },
        "started_at": "2026-03-13T02:10:00Z",
        "finished_at": "2026-03-13T02:15:00Z",
        "duration_ms": 300000,
        "metrics": {
            "attempted_cases": 1,
            "verified_repairs": 0,
            "accepted_patches": 0,
            "false_greens": 0,
            "verified_repair_rate": 0.0,
            "accepted_patch_rate": 0.0,
            "false_green_rate": 0.0,
            "median_time_to_first_candidate_ms": serde_json::Value::Null,
            "median_time_to_verified_repair_ms": serde_json::Value::Null,
            "median_cost_per_attempted_case_usd": 0.02,
            "mean_cost_per_attempted_case_usd": 0.02,
            "median_cost_per_verified_repair_usd": serde_json::Value::Null,
            "mean_cost_per_verified_repair_usd": serde_json::Value::Null
        },
        "failure_attribution": {},
        "execution_diagnostics": {
            "generation_failures": 0,
            "safety_failures": 0,
            "candidate_verification_failures": 0,
            "final_bundle_failures": 0
        },
        "speedup_curve": [{
            "agent_count": 4,
            "attempted_cases": 1,
            "verified_repairs": 0,
            "median_duration_ms": 1800,
            "median_time_to_verified_repair_ms": serde_json::Value::Null,
            "speedup_vs_baseline": 1.0
        }],
        "cost_curve": [{
            "agent_count": 4,
            "attempted_cases": 1,
            "median_total_cost_usd": 0.02,
            "mean_total_cost_usd": 0.02
        }],
        "results": [{
            "case_id": "rust_runtime_assertion_failure",
            "agent_count": 4,
            "outcome": "no_patch",
            "verified_repair": false,
            "difficulty": "easy",
            "tier": serde_json::Value::Null,
            "verifier_class": serde_json::Value::Null
        }]
    });

    fs::write(
        &quality_path,
        serde_json::to_vec_pretty(&quality_report).expect("quality report should serialize"),
    )
    .expect("quality report should be writable");
    fs::write(
        &agent_sweep_path,
        serde_json::to_vec_pretty(&agent_sweep_report)
            .expect("agent sweep report should serialize"),
    )
    .expect("agent sweep report should be writable");

    let output = Command::new("python3")
        .arg("evals/repair_benchmark/publish.py")
        .arg("--quality-report")
        .arg(quality_path.as_os_str())
        .arg("--agent-sweep-report")
        .arg(agent_sweep_path.as_os_str())
        .arg("--output-dir")
        .arg(output_dir.as_os_str())
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("python3 should execute benchmark publisher");
    assert!(
        output.status.success(),
        "canonical legacy benchmark publisher should succeed: stdout=`{}` stderr=`{}`",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let copied_quality: Value = serde_json::from_str(
        &fs::read_to_string(output_dir.join("rust-v0-quality.report.json"))
            .expect("copied quality report should exist"),
    )
    .expect("copied quality report should parse");
    let copied_agent_sweep: Value = serde_json::from_str(
        &fs::read_to_string(output_dir.join("rust-v0-agent-sweep.report.json"))
            .expect("copied agent sweep report should exist"),
    )
    .expect("copied agent sweep report should parse");

    assert_eq!(
        copied_quality["selection"]["manifest_path"],
        "evals/v0/manifest.json"
    );
    assert_eq!(copied_quality["results"][0]["tier"], "tier0");
    assert_eq!(copied_quality["results"][0]["verifier_class"], "cargo_test");
    assert_eq!(copied_quality["results"][0]["candidate_lineage"], "apply_edit");
    assert_eq!(copied_quality["results"][0]["accepted_patch_bytes"], 48);
    assert_eq!(
        copied_quality["candidate_lineage_breakdown"]["apply_edit"]["attempted_cases"],
        1
    );
    assert_eq!(
        copied_quality["candidate_attempt_breakdown"]["apply_edit"]["attempts"],
        2
    );
    assert_eq!(
        copied_quality["candidate_attempt_breakdown"]["apply_edit"]["accepted_steps"],
        1
    );
    assert_eq!(
        copied_quality["tier_breakdown"]["tier0"]["attempted_cases"],
        1
    );
    assert_eq!(
        copied_quality["verifier_class_breakdown"]["cargo_test"]["attempted_cases"],
        1
    );

    assert_eq!(
        copied_agent_sweep["selection"]["manifest_path"],
        "evals/v0/manifest.json"
    );
    assert_eq!(copied_agent_sweep["results"][0]["tier"], "tier0");
    assert_eq!(copied_agent_sweep["results"][0]["verifier_class"], "cargo_test");
    assert_eq!(copied_agent_sweep["results"][0]["candidate_lineage"], "unknown");
    assert_eq!(
        copied_agent_sweep["candidate_lineage_breakdown"]["unknown"]["attempted_cases"],
        1
    );
    assert_eq!(
        copied_agent_sweep["tier_breakdown"]["tier0"]["attempted_cases"],
        1
    );
    assert_eq!(
        copied_agent_sweep["verifier_class_breakdown"]["cargo_test"]["attempted_cases"],
        1
    );

    assert_public_benchmark_report_is_scrubbed(&copied_quality);
    assert_public_benchmark_report_is_scrubbed(&copied_agent_sweep);
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
fn run_synthetic_false_green_benchmark(keep_workspaces: bool) -> (tempfile::TempDir, PathBuf) {
    let run_id = if keep_workspaces {
        "false-green-keep"
    } else {
        "false-green"
    };
    run_synthetic_false_green_benchmark_with_run_id(keep_workspaces, run_id)
}

#[cfg(unix)]
fn run_synthetic_false_green_benchmark_with_run_id(
    keep_workspaces: bool,
    run_id: &str,
) -> (tempfile::TempDir, PathBuf) {
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
    sandbox_root = cwd / ".mercury" / "worktrees" / "fake-sandbox"
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
        "generation_failures": 0,
        "safety_failures": 0,
        "candidate_verification_failures": 0,
        "final_bundle_failures": 0,
        "apply_edit_attempts": 1,
        "grounded_next_edit_attempts": 0,
        "critique_retry_attempts": 0,
        "exploratory_next_edit_attempts": 0,
        "apply_edit_accepted_steps": 1,
        "grounded_next_edit_accepted_steps": 0,
        "critique_retry_accepted_steps": 0,
        "exploratory_next_edit_accepted_steps": 0,
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
    let mut command = Command::new("python3");
    command
        .arg("evals/repair_benchmark/run.py")
        .arg("--suite")
        .arg(suite_root.join("manifest.json"))
        .arg("--binary")
        .arg(&fake_binary)
        .arg("--case")
        .arg("synthetic_false_green")
        .arg("--run-id")
        .arg(run_id)
        .arg("--output-dir")
        .arg(&output_dir)
        .arg("--clean-output");
    if keep_workspaces {
        command.arg("--keep-workspaces");
    }
    let output = command
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

    (temp, output_dir.join(format!("run-{run_id}")))
}

#[cfg(unix)]
#[test]
fn repair_benchmark_runner_downgrades_false_green_after_independent_rerun() {
    let (_temp, run_dir) = run_synthetic_false_green_benchmark(false);
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
    assert_eq!(
        result["failure_attribution"],
        "mercury_verified_but_independent_rerun_failed"
    );
    assert_eq!(result["final_bundle_verified"], true);
    assert_eq!(result["accepted_patch"], true);
    assert_eq!(result["independent_rerun_success"], false);
    assert_eq!(result["workspace_preserved"], false);
    assert!(result["candidate_workspace"].is_null());
    assert_eq!(result["tier"], "unknown");
    assert_eq!(result["verifier_class"], "unknown");
    assert_eq!(result["candidate_lineage"], "apply_edit");
    for field in EXECUTION_DIAGNOSTIC_FIELDS {
        assert_eq!(
            result[field], 0,
            "synthetic false-green result should keep {field}"
        );
    }
    for field in CANDIDATE_ATTEMPT_FIELDS {
        assert!(
            result[field].as_u64().is_some(),
            "synthetic false-green result should expose numeric {field}"
        );
    }
    for field in CANDIDATE_ACCEPTED_FIELDS {
        assert!(
            result[field].as_u64().is_some(),
            "synthetic false-green result should expose numeric {field}"
        );
    }

    let benchmark_run: Value = serde_json::from_str(
        &fs::read_to_string(
            run_dir
                .join("cases")
                .join("synthetic_false_green")
                .join("agents-4")
                .join("benchmark-run.json"),
        )
        .expect("benchmark-run.json should exist"),
    )
    .expect("benchmark-run.json should be valid json");
    assert!(
        benchmark_run.get("sandbox_run_root").is_none(),
        "default run should redact sandbox_run_root from copied benchmark-run.json"
    );
    assert!(
        !run_dir
            .join("workspaces")
            .join("synthetic_false_green-agents-4")
            .exists(),
        "default run should delete copied workspaces after rerun"
    );

    let report: Value = serde_json::from_str(
        &fs::read_to_string(run_dir.join("report.json")).expect("report.json should exist"),
    )
    .expect("report.json should be valid json");
    assert_eq!(report["schema_version"], "mercury-repair-benchmark-v1");
    assert_eq!(report["metrics"]["false_greens"], 1);
    assert_eq!(report["metrics"]["verified_repairs"], 0);
    assert_eq!(
        report["failure_attribution"]["mercury_verified_but_independent_rerun_failed"],
        1
    );
    assert_eq!(report["keep_workspaces"], false);
    assert_eq!(report["tier_breakdown"]["unknown"]["attempted_cases"], 1);
    assert_eq!(
        report["verifier_class_breakdown"]["unknown"]["attempted_cases"],
        1
    );
    assert_eq!(
        report["candidate_lineage_breakdown"]["apply_edit"]["attempted_cases"],
        1
    );
    assert_eq!(
        report["candidate_attempt_breakdown"]["apply_edit"]["attempts"],
        1
    );
    assert_eq!(
        report["candidate_attempt_breakdown"]["apply_edit"]["accepted_steps"],
        1
    );
}

#[cfg(unix)]
#[test]
fn repair_benchmark_runner_preserves_workspaces_when_requested() {
    let (_temp, run_dir) = run_synthetic_false_green_benchmark(true);
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
    assert_eq!(result["workspace_preserved"], true);

    let candidate_workspace = PathBuf::from(
        result["candidate_workspace"]
            .as_str()
            .expect("candidate workspace should be recorded when kept"),
    );
    assert!(
        candidate_workspace.exists(),
        "kept run should retain the candidate workspace"
    );
    assert!(
        run_dir
            .join("workspaces")
            .join("synthetic_false_green-agents-4")
            .exists(),
        "kept run should retain copied workspaces"
    );

    let benchmark_run: Value = serde_json::from_str(
        &fs::read_to_string(
            run_dir
                .join("cases")
                .join("synthetic_false_green")
                .join("agents-4")
                .join("benchmark-run.json"),
        )
        .expect("benchmark-run.json should exist"),
    )
    .expect("benchmark-run.json should be valid json");
    let sandbox_run_root = PathBuf::from(
        benchmark_run["sandbox_run_root"]
            .as_str()
            .expect("kept run should preserve sandbox_run_root"),
    );
    assert!(
        sandbox_run_root.exists(),
        "kept run should preserve the sandbox root on disk"
    );

    let report: Value = serde_json::from_str(
        &fs::read_to_string(run_dir.join("report.json")).expect("report.json should exist"),
    )
    .expect("report.json should be valid json");
    assert_eq!(report["keep_workspaces"], true);
}

#[cfg(unix)]
#[test]
fn repair_benchmark_runner_clears_stale_case_output_on_rerun() {
    let (temp, run_dir) =
        run_synthetic_false_green_benchmark_with_run_id(false, "false-green-rerun");
    let result_root = run_dir
        .join("cases")
        .join("synthetic_false_green")
        .join("agents-4");
    let stale_marker = result_root.join("stale.txt");
    fs::write(&stale_marker, "stale\n").expect("stale marker should be writable");
    assert!(
        stale_marker.exists(),
        "stale marker should exist before rerun"
    );

    let output = Command::new("python3")
        .arg("evals/repair_benchmark/run.py")
        .arg("--suite")
        .arg(temp.path().join("suite/manifest.json"))
        .arg("--binary")
        .arg(temp.path().join("fake-mercury"))
        .arg("--case")
        .arg("synthetic_false_green")
        .arg("--run-id")
        .arg("false-green-rerun")
        .arg("--output-dir")
        .arg(temp.path().join("reports"))
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("INCEPTION_API_KEY", "test-key")
        .output()
        .expect("python3 should execute repair benchmark rerun");

    assert!(
        output.status.success(),
        "synthetic benchmark rerun should succeed: stdout=`{}` stderr=`{}`",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        !stale_marker.exists(),
        "rerun with the same run id should clear stale per-case output before writing new results"
    );
}

#[cfg(unix)]
#[test]
fn repair_benchmark_runner_preserves_timeout_logs_when_subprocess_returns_bytes() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("tempdir should be created");
    let suite_root = temp.path().join("synthetic-suite");
    let case_root = suite_root.join("cases").join("synthetic_timeout_bytes");
    fs::create_dir_all(&case_root).expect("synthetic case directory should be created");

    fs::write(case_root.join("verifier.py"), "import sys\nsys.exit(1)\n")
        .expect("verifier should be written");

    let manifest = json!({
        "schema_version": "mercury-evals-v0",
        "suite_id": "synthetic-timeout-bytes",
        "language": "rust",
        "version": 3,
        "artifact_schema_version": "mercury-eval-report-v0",
        "supported_modes": ["baseline"],
        "cases": [{
            "id": "synthetic_timeout_bytes",
            "title": "Synthetic timeout bytes",
            "path": "cases/synthetic_timeout_bytes",
            "verifier_command": ["python3", "verifier.py"],
            "failure_stage": "test",
            "failure_class": "timeout_bytes",
            "tags": ["language:rust", "stage:test", "kind:synthetic"],
            "timeout_seconds": 1,
            "demo_track": "none",
            "provenance": {
                "origin": "synthetic",
                "suite": "synthetic-timeout-bytes",
                "variant": "timeout",
                "generator": "tests/eval_manifest.rs"
            }
        }]
    });
    fs::write(
        suite_root.join("manifest.json"),
        serde_json::to_string_pretty(&manifest).expect("manifest should serialize"),
    )
    .expect("synthetic manifest should be written");

    let fake_binary = temp.path().join("fake-mercury-timeout");
    fs::write(
        &fake_binary,
        r#"#!/usr/bin/env python3
import sys
import time

args = sys.argv[1:]

if args and args[0] == "init":
    sys.exit(0)

if args and args[0] == "fix":
    sys.stdout.buffer.write(b"timed out bytes\n")
    sys.stdout.flush()
    time.sleep(2)
    sys.exit(1)

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
        .arg("synthetic_timeout_bytes")
        .arg("--run-id")
        .arg("timeout-bytes")
        .arg("--timeout-seconds")
        .arg("1")
        .arg("--output-dir")
        .arg(&output_dir)
        .arg("--clean-output")
        .env("INCEPTION_API_KEY", "test-key")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("python3 should execute repair benchmark runner");

    assert!(
        output.status.success(),
        "synthetic timeout benchmark run should succeed: stdout=`{}` stderr=`{}`",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let result_root = output_dir
        .join("run-timeout-bytes")
        .join("cases")
        .join("synthetic_timeout_bytes")
        .join("agents-4");
    let fix_log = fs::read_to_string(result_root.join("fix.stdout.log"))
        .expect("fix stdout log should exist");
    assert!(
        fix_log.contains("timed out bytes"),
        "timed-out fix stdout should still be captured in the log"
    );

    let fix_meta: Value = serde_json::from_str(
        &fs::read_to_string(result_root.join("fix.json")).expect("fix metadata should exist"),
    )
    .expect("fix metadata should parse");
    assert_eq!(
        fix_meta["timed_out"],
        Value::Bool(true),
        "timed-out fix metadata should be preserved"
    );

    let result: Value = serde_json::from_str(
        &fs::read_to_string(result_root.join("result.json")).expect("result metadata should exist"),
    )
    .expect("result metadata should parse");
    assert_eq!(
        result["outcome"],
        Value::String("missing_benchmark_run".to_string()),
        "timed-out runs without benchmark artifacts should still produce a result"
    );
    assert_eq!(
        result["failure_attribution"],
        Value::String("missing_benchmark_run".to_string())
    );
}

#[cfg(unix)]
#[test]
fn repair_benchmark_runner_resumes_existing_results_and_keeps_partial_checkpoints() {
    let (temp, run_dir) =
        run_synthetic_false_green_benchmark_with_run_id(false, "false-green-resume");
    let result_root = run_dir
        .join("cases")
        .join("synthetic_false_green")
        .join("agents-4");
    let resume_sentinel = result_root.join("resume-sentinel.txt");
    fs::write(&resume_sentinel, "keep\n").expect("resume sentinel should be writable");

    let output = Command::new("python3")
        .arg("evals/repair_benchmark/run.py")
        .arg("--suite")
        .arg(temp.path().join("suite/manifest.json"))
        .arg("--binary")
        .arg(temp.path().join("fake-mercury"))
        .arg("--case")
        .arg("synthetic_false_green")
        .arg("--run-id")
        .arg("false-green-resume")
        .arg("--output-dir")
        .arg(temp.path().join("reports"))
        .arg("--resume")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("INCEPTION_API_KEY", "test-key")
        .output()
        .expect("python3 should execute repair benchmark resume");

    assert!(
        output.status.success(),
        "synthetic benchmark resume should succeed: stdout=`{}` stderr=`{}`",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        resume_sentinel.exists(),
        "resume should reuse the existing case result directory instead of clearing it"
    );
    assert!(
        run_dir.join("report.partial.json").exists(),
        "resume should maintain a partial aggregate checkpoint"
    );
    assert!(
        run_dir.join("summary.partial.md").exists(),
        "resume should maintain a partial summary checkpoint"
    );

    let report: Value = serde_json::from_str(
        &fs::read_to_string(run_dir.join("report.json")).expect("report.json should exist"),
    )
    .expect("report.json should be valid json");
    assert_eq!(report["metrics"]["attempted_cases"], 1);
    assert_eq!(report["results"][0]["outcome"], "false_green");
}

#[cfg(unix)]
#[test]
fn repair_benchmark_runner_resume_reruns_legacy_results_without_lineage_metadata() {
    let (temp, run_dir) =
        run_synthetic_false_green_benchmark_with_run_id(false, "false-green-legacy-resume");
    let result_root = run_dir
        .join("cases")
        .join("synthetic_false_green")
        .join("agents-4");
    let result_path = result_root.join("result.json");
    let benchmark_run_path = result_root.join("benchmark-run.json");
    let stale_marker = result_root.join("legacy-stale.txt");
    fs::write(&stale_marker, "stale\n").expect("legacy marker should be writable");

    let mut legacy_result: Value = serde_json::from_str(
        &fs::read_to_string(&result_path).expect("result.json should exist before resume"),
    )
    .expect("result.json should be valid json before resume");
    let legacy_result_object = legacy_result
        .as_object_mut()
        .expect("legacy result should be a json object");
    legacy_result_object.remove("tier");
    legacy_result_object.remove("verifier_class");
    legacy_result_object.remove("candidate_lineage");
    for field in CANDIDATE_ATTEMPT_FIELDS {
        legacy_result_object.remove(field);
    }
    for field in CANDIDATE_ACCEPTED_FIELDS {
        legacy_result_object.remove(field);
    }
    fs::write(
        &result_path,
        serde_json::to_string_pretty(&legacy_result).expect("legacy result should serialize"),
    )
    .expect("legacy result should be rewritten");

    let mut legacy_benchmark_run: Value = serde_json::from_str(
        &fs::read_to_string(&benchmark_run_path).expect("benchmark-run.json should exist"),
    )
    .expect("benchmark-run.json should be valid json");
    let legacy_benchmark_run_object = legacy_benchmark_run
        .as_object_mut()
        .expect("legacy benchmark run should be a json object");
    for field in CANDIDATE_ATTEMPT_FIELDS {
        legacy_benchmark_run_object.remove(field);
    }
    for field in CANDIDATE_ACCEPTED_FIELDS {
        legacy_benchmark_run_object.remove(field);
    }
    fs::write(
        &benchmark_run_path,
        serde_json::to_string_pretty(&legacy_benchmark_run)
            .expect("legacy benchmark run should serialize"),
    )
    .expect("legacy benchmark run should be rewritten");

    let output = Command::new("python3")
        .arg("evals/repair_benchmark/run.py")
        .arg("--suite")
        .arg(temp.path().join("suite/manifest.json"))
        .arg("--binary")
        .arg(temp.path().join("fake-mercury"))
        .arg("--case")
        .arg("synthetic_false_green")
        .arg("--run-id")
        .arg("false-green-legacy-resume")
        .arg("--output-dir")
        .arg(temp.path().join("reports"))
        .arg("--resume")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("INCEPTION_API_KEY", "test-key")
        .output()
        .expect("python3 should execute repair benchmark resume");

    assert!(
        output.status.success(),
        "synthetic benchmark legacy resume should succeed: stdout=`{}` stderr=`{}`",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        !stale_marker.exists(),
        "resume should rerun stale legacy case results instead of reusing the existing directory"
    );

    let result: Value = serde_json::from_str(
        &fs::read_to_string(&result_path).expect("result.json should exist after resume"),
    )
    .expect("result.json should be valid json after resume");
    assert_eq!(result["candidate_lineage"], "apply_edit");
    for field in CANDIDATE_ATTEMPT_FIELDS {
        assert!(
            result[field].as_u64().is_some(),
            "legacy resume rerun should restore numeric {field}"
        );
    }
    for field in CANDIDATE_ACCEPTED_FIELDS {
        assert!(
            result[field].as_u64().is_some(),
            "legacy resume rerun should restore numeric {field}"
        );
    }

    let report: Value = serde_json::from_str(
        &fs::read_to_string(run_dir.join("report.json")).expect("report.json should exist"),
    )
    .expect("report.json should be valid json");
    assert_eq!(
        report["candidate_lineage_breakdown"]["apply_edit"]["attempted_cases"],
        1
    );
}

#[cfg(unix)]
#[test]
fn repair_benchmark_runner_records_runner_errors_instead_of_aborting() {
    let temp = tempfile::tempdir().expect("tempdir should be created");
    let suite_root = temp.path().join("suite");
    fs::create_dir_all(&suite_root).expect("synthetic suite root should be created");

    let manifest = json!({
        "schema_version": "mercury-evals-v0",
        "suite_id": "synthetic-runner-error",
        "language": "rust",
        "version": 1,
        "artifact_schema_version": "mercury-eval-report-v0",
        "supported_modes": ["baseline"],
        "cases": [{
            "id": "missing_fixture_case",
            "title": "Missing fixture case",
            "path": "cases/missing_fixture_case",
            "verifier_command": ["python3", "verifier.py"],
            "failure_stage": "test",
            "failure_class": "synthetic",
            "tags": ["language:rust", "stage:test", "kind:synthetic"],
            "timeout_seconds": 30,
            "demo_track": "none",
            "provenance": {
                "origin": "synthetic",
                "suite": "synthetic-runner-error",
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

    let fake_binary = temp.path().join("fake-mercury-runner-error");
    fs::write(
        &fake_binary,
        "#!/usr/bin/env python3\nimport sys\nsys.exit(0)\n",
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
        .arg("missing_fixture_case")
        .arg("--run-id")
        .arg("runner-error")
        .arg("--output-dir")
        .arg(&output_dir)
        .arg("--clean-output")
        .env("INCEPTION_API_KEY", "test-key")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("python3 should execute repair benchmark runner");

    assert!(
        output.status.success(),
        "runner errors should be captured without aborting the benchmark: stdout=`{}` stderr=`{}`",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let run_dir = output_dir.join("run-runner-error");
    let result: Value = serde_json::from_str(
        &fs::read_to_string(
            run_dir
                .join("cases")
                .join("missing_fixture_case")
                .join("agents-4")
                .join("result.json"),
        )
        .expect("result.json should exist"),
    )
    .expect("result.json should be valid json");
    assert_eq!(result["outcome"], "runner_error");
    assert_eq!(result["accepted_patch"], false);
    assert_eq!(result["verified_repair"], false);
    assert!(
        result["runner_error_message"]
            .as_str()
            .expect("runner error message should be present")
            .contains("missing_fixture_case"),
        "runner error should record the failing fixture path"
    );
    assert!(
        result["runner_error_traceback"]
            .as_str()
            .expect("runner error traceback should be present")
            .contains("copytree"),
        "runner error traceback should preserve the original failure site"
    );
    assert!(run_dir.join("report.partial.json").exists());
    assert!(run_dir.join("summary.partial.md").exists());

    let report: Value = serde_json::from_str(
        &fs::read_to_string(run_dir.join("report.json")).expect("report.json should exist"),
    )
    .expect("report.json should be valid json");
    assert_eq!(report["metrics"]["attempted_cases"], 1);
    assert_eq!(report["results"][0]["outcome"], "runner_error");
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
