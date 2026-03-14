use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use assert_cmd::prelude::*;
use mercury_cli::verification::{
    build_allowlisted_verifier_command, parse_allowlisted_verifier_parts,
    verifier_command_allowlisted,
};

#[test]
fn allowlisted_verifier_builder_preserves_workspace_env_and_args() {
    let temp = tempfile::tempdir().expect("tempdir should be created");

    let rust_parts = parse_allowlisted_verifier_parts(
        "env RUSTFLAGS=-Dwarnings cargo +nightly test -p demo -- --nocapture",
    )
    .expect("rust verifier command should parse");
    assert!(verifier_command_allowlisted(
        "env RUSTFLAGS=-Dwarnings cargo +nightly test -p demo -- --nocapture"
    ));
    let rust_command = build_allowlisted_verifier_command(&rust_parts, temp.path())
        .expect("rust verifier command should build");
    let rust_args = rust_command
        .get_args()
        .map(os_to_string)
        .collect::<Vec<_>>();
    let rust_envs = command_envs(&rust_command);
    assert_eq!(rust_command.get_program(), OsStr::new("cargo"));
    assert_eq!(
        rust_command.get_current_dir(),
        Some(temp.path()),
        "workspace root should become the verifier current_dir"
    );
    assert_eq!(
        rust_args,
        vec![
            "+nightly".to_string(),
            "test".to_string(),
            "-p".to_string(),
            "demo".to_string(),
            "--".to_string(),
            "--nocapture".to_string(),
        ]
    );
    assert_eq!(
        rust_envs.get("RUSTFLAGS"),
        Some(&Some("-Dwarnings".to_string()))
    );

    let ts_parts =
        parse_allowlisted_verifier_parts("env NODE_ENV=test npm run test -- --runInBand")
            .expect("typescript verifier command should parse");
    assert!(verifier_command_allowlisted(
        "env NODE_ENV=test npm run test -- --runInBand"
    ));
    let ts_command = build_allowlisted_verifier_command(&ts_parts, temp.path())
        .expect("typescript verifier command should build");
    let ts_args = ts_command.get_args().map(os_to_string).collect::<Vec<_>>();
    let ts_envs = command_envs(&ts_command);
    assert_eq!(ts_command.get_program(), OsStr::new("npm"));
    assert_eq!(
        ts_command.get_current_dir(),
        Some(temp.path()),
        "workspace root should become the verifier current_dir"
    );
    assert_eq!(
        ts_args,
        vec![
            "run".to_string(),
            "test".to_string(),
            "--".to_string(),
            "--runInBand".to_string(),
        ]
    );
    assert_eq!(ts_envs.get("NODE_ENV"), Some(&Some("test".to_string())));
}

#[test]
fn fix_rejects_non_allowlisted_verifier_before_api_lookup_and_records_audit() {
    let temp = tempfile::tempdir().expect("tempdir should be created");
    init_repo(temp.path());
    write_minimal_rust_project(temp.path());
    rewrite_verifier_config(temp.path(), "pytest -q");

    let output = Command::new(cargo_bin())
        .current_dir(temp.path())
        .arg("fix")
        .arg("repair the failing tests")
        .env_remove("INCEPTION_API_KEY")
        .env_remove("MERCURY_API_KEY")
        .output()
        .expect("fix command should run");

    assert!(
        !output.status.success(),
        "fix should fail when verifier allowlist blocks the configured command"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("verifier allowlist violation"),
        "stderr should explain the verifier allowlist violation: {stderr}"
    );
    assert!(
        !stderr.contains("missing API key"),
        "allowlist rejection should happen before API key lookup: {stderr}"
    );

    let run_root = latest_run_root(temp.path());
    let audit_log =
        fs::read_to_string(run_root.join("audit.log")).expect("audit log should be written");
    assert!(
        audit_log.contains("\"event\":\"fix_run_rejected_allowlist\""),
        "audit log should record the allowlist rejection: {audit_log}"
    );
    assert!(
        !run_root.join("plan.json").exists(),
        "planner artifacts should not exist when the run is rejected before API lookup"
    );
}

#[test]
fn fix_accepts_allowlisted_env_prefixed_verifier_then_fails_on_missing_api_key() {
    let temp = tempfile::tempdir().expect("tempdir should be created");
    init_repo(temp.path());
    write_minimal_rust_project(temp.path());
    rewrite_verifier_config(temp.path(), "env RUST_BACKTRACE=1 cargo test --quiet");

    let output = Command::new(cargo_bin())
        .current_dir(temp.path())
        .arg("fix")
        .arg("repair the failing tests")
        .env_remove("INCEPTION_API_KEY")
        .env_remove("MERCURY_API_KEY")
        .output()
        .expect("fix command should run");

    assert!(
        !output.status.success(),
        "fix should fail without an API key"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("missing API key"),
        "stderr should surface missing API key once allowlist passes: {stderr}"
    );
    assert!(
        !stderr.contains("verifier allowlist violation"),
        "allowlisted command should not be rejected: {stderr}"
    );

    let run_root = latest_run_root(temp.path());
    let audit_log =
        fs::read_to_string(run_root.join("audit.log")).expect("audit log should be written");
    assert!(
        audit_log.contains("\"event\":\"fix_run_started\""),
        "audit log should record the started run when allowlist passes: {audit_log}"
    );
    assert!(
        !audit_log.contains("\"event\":\"fix_run_rejected_allowlist\""),
        "allowlisted verifier command should not emit a rejection event: {audit_log}"
    );
}

#[test]
fn fix_allowlist_override_reaches_missing_api_key_without_rejection() {
    let temp = tempfile::tempdir().expect("tempdir should be created");
    init_repo(temp.path());
    write_minimal_rust_project(temp.path());
    rewrite_verifier_config(temp.path(), "pytest -q");

    let output = Command::new(cargo_bin())
        .current_dir(temp.path())
        .arg("fix")
        .arg("repair the failing tests")
        .env("MERCURY_ALLOW_UNSAFE_VERIFIER_COMMANDS", "1")
        .env_remove("INCEPTION_API_KEY")
        .env_remove("MERCURY_API_KEY")
        .output()
        .expect("fix command should run");

    assert!(
        !output.status.success(),
        "fix should still fail without an API key"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("missing API key"),
        "override should bypass allowlist and reach API resolution: {stderr}"
    );
    assert!(
        !stderr.contains("verifier allowlist violation"),
        "override should suppress the allowlist rejection path: {stderr}"
    );

    let run_root = latest_run_root(temp.path());
    let audit_log =
        fs::read_to_string(run_root.join("audit.log")).expect("audit log should be written");
    assert!(
        audit_log.contains("\"event\":\"fix_run_started\""),
        "override path should still record the started run: {audit_log}"
    );
    assert!(
        !audit_log.contains("\"event\":\"fix_run_rejected_allowlist\""),
        "override path should not record an allowlist rejection: {audit_log}"
    );
}

#[test]
fn watch_rejects_shell_composition_before_creating_run_artifacts() {
    let temp = tempfile::tempdir().expect("tempdir should be created");
    init_repo(temp.path());

    let output = Command::new(cargo_bin())
        .current_dir(temp.path())
        .arg("watch")
        .arg("cargo test && cargo clippy")
        .arg("--repair")
        .output()
        .expect("watch command should run");

    assert!(
        !output.status.success(),
        "watch should reject shell-composed verifier commands"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("watch command rejected"),
        "stderr should explain the watch allowlist rejection: {stderr}"
    );
    assert!(
        !temp.path().join(".mercury").join("runs").exists(),
        "watch should reject before creating any run artifact bundle"
    );
}

fn cargo_bin() -> PathBuf {
    assert_cmd::cargo::cargo_bin!("mercury-cli").to_path_buf()
}

fn init_repo(root: &Path) {
    Command::new(cargo_bin())
        .current_dir(root)
        .arg("init")
        .assert()
        .success();
}

fn write_minimal_rust_project(root: &Path) {
    fs::create_dir_all(root.join("src")).expect("src directory should be created");
    fs::write(
        root.join("Cargo.toml"),
        r#"[package]
name = "allowlist-fixture"
version = "1.0.0-beta.1"
edition = "2021"

[workspace]
members = []
"#,
    )
    .expect("Cargo.toml should be written");
    fs::write(root.join("src/lib.rs"), "pub fn answer() -> i32 { 42 }\n")
        .expect("src/lib.rs should be written");
}

fn rewrite_verifier_config(root: &Path, test_command: &str) {
    let config_path = root.join(".mercury").join("config.toml");
    let config = fs::read_to_string(&config_path).expect("config should be readable");
    let rewritten = replace_config_line(
        &config,
        "test_command = ",
        format!("test_command = \"{test_command}\""),
    );
    fs::write(config_path, rewritten).expect("config should be updated");
}

fn replace_config_line(config: &str, prefix: &str, replacement: String) -> String {
    let mut replaced = false;
    let lines = config
        .lines()
        .map(|line| {
            if line.trim_start().starts_with(prefix) {
                replaced = true;
                replacement.clone()
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>();
    assert!(replaced, "config line with prefix `{prefix}` should exist");

    let mut rendered = lines.join("\n");
    if config.ends_with('\n') {
        rendered.push('\n');
    }
    rendered
}

fn latest_run_root(root: &Path) -> PathBuf {
    let runs_root = root.join(".mercury").join("runs");
    let mut entries = fs::read_dir(&runs_root)
        .expect("run artifact root should exist")
        .map(|entry| entry.expect("artifact entry should be readable").path())
        .collect::<Vec<_>>();
    entries.sort();
    entries.pop().expect("one run artifact should exist")
}

fn command_envs(command: &Command) -> BTreeMap<String, Option<String>> {
    command
        .get_envs()
        .map(|(key, value)| {
            (
                os_to_string(key),
                value.map(|inner| inner.to_string_lossy().into_owned()),
            )
        })
        .collect()
}

fn os_to_string(value: &OsStr) -> String {
    value.to_string_lossy().into_owned()
}
