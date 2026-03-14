use std::collections::BTreeMap;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, LazyLock, Mutex};

use mercury_cli::api::{
    ApiError, ApiUsage, Mercury2Api, ToolCall, ToolCallFunction, ToolDefinition,
};
use mercury_cli::engine::VerifyConfig;
use mercury_cli::verification::{
    build_allowlisted_verifier_command, gather_grounded_repair_context,
    parse_allowlisted_verifier_parts, RepoToolExecutor, RUN_TESTS_TOOL,
};
use serde_json::{json, Value};
use tempfile::tempdir;
use tokio::sync::{Mutex as AsyncMutex, MutexGuard as AsyncMutexGuard};

type ToolResponses = Arc<Mutex<Vec<(String, Vec<ToolCall>)>>>;

static ENV_LOCK: LazyLock<AsyncMutex<()>> = LazyLock::new(|| AsyncMutex::new(()));

fn lock_env() -> AsyncMutexGuard<'static, ()> {
    ENV_LOCK.blocking_lock()
}

async fn lock_env_async() -> AsyncMutexGuard<'static, ()> {
    ENV_LOCK.lock().await
}

#[derive(Clone)]
struct FakeMercuryApi {
    responses: ToolResponses,
}

impl FakeMercuryApi {
    fn new(responses: Vec<(String, Vec<ToolCall>)>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(responses.into_iter().rev().collect())),
        }
    }
}

impl Mercury2Api for FakeMercuryApi {
    async fn chat(
        &self,
        _system: &str,
        _user: &str,
        _max_tokens: u32,
    ) -> Result<(String, ApiUsage), ApiError> {
        Ok((String::new(), ApiUsage::default()))
    }

    async fn chat_json(
        &self,
        _system: &str,
        _user: &str,
        _max_tokens: u32,
    ) -> Result<(mercury_cli::api::ThermalAssessment, ApiUsage), ApiError> {
        unreachable!()
    }

    async fn chat_json_schema_value(
        &self,
        _system: &str,
        _user: &str,
        _max_tokens: u32,
        _schema_name: &str,
        _schema: Value,
    ) -> Result<(Value, ApiUsage), ApiError> {
        unreachable!()
    }

    async fn chat_with_tools(
        &self,
        _system: &str,
        _user: &str,
        _max_tokens: u32,
        _tools: Vec<ToolDefinition>,
        _tool_choice: Option<mercury_cli::api::ToolChoice>,
    ) -> Result<(String, Vec<ToolCall>, ApiUsage), ApiError> {
        let mut responses = self.responses.lock().expect("responses mutex poisoned");
        let (text, tool_calls) = responses.pop().unwrap_or_default();
        Ok((
            text,
            tool_calls,
            ApiUsage {
                tokens_used: 7,
                cost_usd: 0.0007,
            },
        ))
    }
}

#[test]
fn builder_injects_ci_safe_noninteractive_env_defaults() {
    let _guard = lock_env();
    let saved = save_env(&["CI", "GITHUB_ACTIONS", "MERCURY_NONINTERACTIVE"]);
    set_env("CI", Some("1"));
    set_env("GITHUB_ACTIONS", None);
    set_env("MERCURY_NONINTERACTIVE", None);

    let temp = tempdir().expect("tempdir should be created");
    let parts =
        parse_allowlisted_verifier_parts("env RUSTFLAGS=-Dwarnings cargo test --quiet").unwrap();
    let command =
        build_allowlisted_verifier_command(&parts, temp.path()).expect("verifier should build");

    restore_env(saved);

    let envs = command_envs(&command);
    assert_eq!(envs.get("RUSTFLAGS"), Some(&Some("-Dwarnings".to_string())));
    assert_eq!(envs.get("CI"), Some(&Some("1".to_string())));
    assert_eq!(
        envs.get("MERCURY_NONINTERACTIVE"),
        Some(&Some("1".to_string()))
    );
    assert_eq!(envs.get("NO_COLOR"), Some(&Some("1".to_string())));
    assert_eq!(
        envs.get("CARGO_TERM_COLOR"),
        Some(&Some("never".to_string()))
    );
    assert_eq!(
        envs.get("GIT_TERMINAL_PROMPT"),
        Some(&Some("0".to_string()))
    );
}

#[test]
fn builder_respects_explicit_verifier_env_overrides() {
    let _guard = lock_env();
    let saved = save_env(&["CI", "GITHUB_ACTIONS", "MERCURY_NONINTERACTIVE"]);
    set_env("CI", Some("1"));
    set_env("GITHUB_ACTIONS", None);
    set_env("MERCURY_NONINTERACTIVE", None);

    let temp = tempdir().expect("tempdir should be created");
    let parts = parse_allowlisted_verifier_parts(
        "env CI=0 MERCURY_NONINTERACTIVE=0 NO_COLOR=0 cargo test --quiet",
    )
    .unwrap();
    let command =
        build_allowlisted_verifier_command(&parts, temp.path()).expect("verifier should build");

    restore_env(saved);

    let envs = command_envs(&command);
    assert_eq!(envs.get("CI"), Some(&Some("0".to_string())));
    assert_eq!(
        envs.get("MERCURY_NONINTERACTIVE"),
        Some(&Some("0".to_string()))
    );
    assert_eq!(envs.get("NO_COLOR"), Some(&Some("0".to_string())));
    assert_eq!(
        envs.get("CARGO_TERM_COLOR"),
        Some(&Some("never".to_string()))
    );
}

#[test]
fn run_tests_emits_redacted_policy_and_audit_metadata() {
    let _guard = lock_env();
    let saved = save_env(&[
        "CI",
        "GITHUB_ACTIONS",
        "MERCURY_NONINTERACTIVE",
        "INCEPTION_API_KEY",
    ]);
    set_env("CI", Some("1"));
    set_env("GITHUB_ACTIONS", None);
    set_env("MERCURY_NONINTERACTIVE", None);
    set_env("INCEPTION_API_KEY", Some("supersecret-observability-token"));

    let temp = tempdir().expect("tempdir should be created");
    let project_root = temp.path().join("project");
    let workspace_root = temp.path().join("workspace");
    write_secret_failing_rust_project(&project_root);
    copy_dir_recursive(&project_root, &workspace_root);
    let executor = RepoToolExecutor::new(&project_root, &workspace_root);

    let result = executor.execute_named(
        None,
        RUN_TESTS_TOOL,
        json!({"command": "env INCEPTION_API_KEY=supersecret-observability-token cargo test --quiet"}),
    );

    restore_env(saved);

    assert!(
        result.success,
        "run_tests should execute even when tests fail"
    );
    assert_eq!(result.output["success"], Value::Bool(false));
    assert_eq!(
        result.output["policy"]["allowlist_enforced"],
        Value::Bool(true)
    );
    assert_eq!(
        result.output["policy"]["secret_redaction_enabled"],
        Value::Bool(true)
    );
    assert_eq!(result.output["policy"]["noninteractive"], Value::Bool(true));
    assert_eq!(
        result.output["policy"]["isolation_mode"],
        Value::String("repo_copy".to_string())
    );
    assert_eq!(
        result.output["audit"]["event"],
        Value::String("verifier_command_completed".to_string())
    );
    assert_eq!(
        result.output["audit"]["decision"],
        Value::String("failed".to_string())
    );
    assert_eq!(
        result.output["command_parts"][1],
        Value::String("INCEPTION_API_KEY=[REDACTED]".to_string())
    );

    let serialized = serde_json::to_string(&result.output).expect("output should serialize");
    assert!(!serialized.contains("supersecret-observability-token"));
    assert!(serialized.contains("INCEPTION_API_KEY=[REDACTED]"));
    assert!(serialized.contains("panic secret=[REDACTED]"));
}

#[test]
fn run_tests_rejection_emits_structured_audit_metadata() {
    let _guard = lock_env();
    let saved = save_env(&[
        "CI",
        "GITHUB_ACTIONS",
        "MERCURY_NONINTERACTIVE",
        "INCEPTION_API_KEY",
    ]);
    set_env("CI", Some("1"));
    set_env("GITHUB_ACTIONS", None);
    set_env("MERCURY_NONINTERACTIVE", None);
    set_env("INCEPTION_API_KEY", Some("topsecret"));

    let temp = tempdir().expect("tempdir should be created");
    let executor = RepoToolExecutor::new(temp.path(), temp.path().join("workspace"));
    let result = executor.execute_named(
        None,
        RUN_TESTS_TOOL,
        json!({"command": "env INCEPTION_API_KEY=topsecret cargo test && cargo clippy"}),
    );

    restore_env(saved);

    assert!(!result.success);
    assert!(result.output["error"]
        .as_str()
        .expect("error should be a string")
        .contains("shell composition"));
    assert_eq!(
        result.output["policy"]["allowlist_enforced"],
        Value::Bool(true)
    );
    assert_eq!(
        result.output["audit"]["event"],
        Value::String("verifier_command_rejected".to_string())
    );
    assert_eq!(
        result.output["audit"]["rejection_reason"],
        Value::String("shell_composition".to_string())
    );
    let serialized = serde_json::to_string(&result.output).expect("output should serialize");
    assert!(!serialized.contains("topsecret"));
    assert!(serialized.contains("INCEPTION_API_KEY=[REDACTED]"));
}

#[tokio::test]
async fn grounded_context_redacts_verifier_commands_and_tool_arguments() {
    let _guard = lock_env_async().await;
    let saved = save_env(&[
        "CI",
        "GITHUB_ACTIONS",
        "MERCURY_NONINTERACTIVE",
        "INCEPTION_API_KEY",
    ]);
    set_env("CI", Some("1"));
    set_env("GITHUB_ACTIONS", None);
    set_env("MERCURY_NONINTERACTIVE", None);
    set_env("INCEPTION_API_KEY", Some("grounding-secret"));

    let temp = tempdir().expect("tempdir should be created");
    let project_root = temp.path().join("project");
    write_passing_rust_project(&project_root);

    let api = FakeMercuryApi::new(vec![
        (
            "Use run_tests".to_string(),
            vec![ToolCall {
                id: "call-1".to_string(),
                kind: "function".to_string(),
                function: ToolCallFunction {
                    name: RUN_TESTS_TOOL.to_string(),
                    arguments: json!({"command": "env INCEPTION_API_KEY=grounding-secret cargo test --quiet"}).to_string(),
                },
            }],
        ),
        ("Grounding complete".to_string(), Vec::new()),
    ]);

    let context = gather_grounded_repair_context(
        &api,
        &project_root,
        &VerifyConfig {
            parse_before_write: true,
            test_after_write: true,
            lint_after_write: false,
            mercury2_critique_on_failure: true,
            test_command: "env INCEPTION_API_KEY=grounding-secret cargo test --quiet".to_string(),
            lint_command: String::new(),
        },
        "Repair failing tests",
        None,
    )
    .await
    .expect("grounded repair context should succeed");

    restore_env(saved);

    assert_eq!(
        context.verifier_commands,
        vec!["env INCEPTION_API_KEY=[REDACTED] cargo test --quiet".to_string()]
    );
    assert_eq!(
        context.rounds[0].tool_calls[0].arguments["command"],
        Value::String("env INCEPTION_API_KEY=[REDACTED] cargo test --quiet".to_string())
    );
    let serialized = serde_json::to_string(&context).expect("context should serialize");
    assert!(!serialized.contains("grounding-secret"));
    assert!(serialized.contains("INCEPTION_API_KEY=[REDACTED]"));
}

fn write_passing_rust_project(project_root: &Path) {
    fs::create_dir_all(project_root.join("src")).expect("src directory should exist");
    fs::write(
        project_root.join("Cargo.toml"),
        r#"[package]
name = "runtime-security-pass"
version = "1.0.0-beta.1"
edition = "2021"

[workspace]
members = []
"#,
    )
    .expect("Cargo.toml should be written");
    fs::write(
        project_root.join("src/main.rs"),
        "fn main() { println!(\"ok\"); }\n",
    )
    .expect("main.rs should be written");
}

fn write_secret_failing_rust_project(project_root: &Path) {
    write_passing_rust_project(project_root);
    fs::create_dir_all(project_root.join("tests")).expect("tests directory should exist");
    fs::write(
        project_root.join("tests/redaction.rs"),
        r#"#[test]
fn leaks_secret() {
    let secret = std::env::var("INCEPTION_API_KEY").expect("secret set");
    println!("stdout secret={secret}");
    eprintln!("stderr secret={secret}");
    panic!("panic secret={secret}");
}
"#,
    )
    .expect("redaction test should be written");
}

fn copy_dir_recursive(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).expect("destination directory should exist");
    for entry in fs::read_dir(source).expect("source directory should be readable") {
        let entry = entry.expect("directory entry should load");
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let metadata = entry.metadata().expect("metadata should load");
        if metadata.is_dir() {
            copy_dir_recursive(&source_path, &destination_path);
        } else if metadata.is_file() {
            fs::copy(&source_path, &destination_path).expect("file should copy");
        }
    }
}

fn command_envs(command: &Command) -> BTreeMap<String, Option<String>> {
    command
        .get_envs()
        .map(|(key, value)| (os_to_string(key), value.map(os_to_string)))
        .collect()
}

fn os_to_string(value: &OsStr) -> String {
    value.to_string_lossy().into_owned()
}

fn save_env(keys: &[&str]) -> Vec<(String, Option<String>)> {
    keys.iter()
        .map(|key| ((*key).to_string(), env::var(key).ok()))
        .collect()
}

fn restore_env(saved: Vec<(String, Option<String>)>) {
    for (key, value) in saved {
        set_env(&key, value.as_deref());
    }
}

fn set_env(key: &str, value: Option<&str>) {
    match value {
        Some(value) => {
            // SAFETY: integration tests serialize process env mutation with ENV_LOCK.
            unsafe { env::set_var(key, value) }
        }
        None => {
            // SAFETY: integration tests serialize process env mutation with ENV_LOCK.
            unsafe { env::remove_var(key) }
        }
    }
}
