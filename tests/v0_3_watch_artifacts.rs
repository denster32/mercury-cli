use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use assert_cmd::prelude::*;
use serde_json::{json, Value};

struct ChildGuard {
    child: Child,
}

impl ChildGuard {
    fn spawn(command: &mut Command) -> Self {
        let child = command.spawn().expect("watch process should spawn");
        Self { child }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Clone, Debug)]
struct RecordedRequest {
    path: String,
    body: String,
}

struct StubServer {
    addr: SocketAddr,
    base_url: String,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl StubServer {
    fn start(fixed_source: String) -> Self {
        Self::start_with_planner_failure(fixed_source, None)
    }

    fn start_with_planner_error(message: &str) -> Self {
        Self::start_with_planner_failure(
            String::new(),
            Some((
                "HTTP/1.1 500 Internal Server Error".to_string(),
                json!({
                    "error": {
                        "message": message,
                        "type": "server_error"
                    }
                })
                .to_string(),
            )),
        )
    }

    fn start_with_planner_failure(
        fixed_source: String,
        planner_failure: Option<(String, String)>,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("stub listener should bind");
        listener
            .set_nonblocking(true)
            .expect("stub listener should become nonblocking");
        let addr = listener
            .local_addr()
            .expect("stub listener should expose local addr");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_requests = Arc::clone(&requests);
        let thread_shutdown = Arc::clone(&shutdown);

        let thread = thread::spawn(move || {
            while !thread_shutdown.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _peer)) => {
                        let _ = handle_stub_connection(
                            &mut stream,
                            &fixed_source,
                            planner_failure.as_ref(),
                            Arc::clone(&thread_requests),
                        );
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            addr,
            base_url: format!("http://{}", addr),
            requests,
            shutdown,
            thread: Some(thread),
        }
    }

    fn mercury2_endpoint(&self) -> String {
        format!("{}/v1/chat/completions", self.base_url)
    }

    fn mercury_edit_endpoint(&self) -> String {
        format!("{}/v1", self.base_url)
    }

    fn recorded_requests(&self) -> Vec<RecordedRequest> {
        self.requests
            .lock()
            .expect("request log should not be poisoned")
            .clone()
    }
}

impl Drop for StubServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(self.addr);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[test]
fn watch_rejects_non_allowlisted_command_before_cycle() {
    let temp = tempfile::tempdir().expect("tempdir should be created");
    init_repo(temp.path());

    let bin = assert_cmd::cargo::cargo_bin!("mercury-cli");
    Command::new(bin)
        .current_dir(temp.path())
        .arg("watch")
        .arg("false")
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "watch command rejected: command not allowlisted",
        ));

    assert!(
        !temp.path().join(".mercury").join("runs").exists(),
        "allowlist rejection should happen before creating watch artifacts"
    );
}

#[test]
fn watch_without_repair_records_passed_decision_for_allowlisted_success_command() {
    let temp = tempfile::tempdir().expect("tempdir should be created");
    init_repo(temp.path());
    write_passing_rust_library(temp.path());

    let bin = assert_cmd::cargo::cargo_bin!("mercury-cli");
    let child = ChildGuard::spawn(
        Command::new(bin)
            .current_dir(temp.path())
            .arg("watch")
            .arg("cargo test --quiet")
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
    );

    let (artifact_root, record) = wait_for_watch_record(temp.path());

    assert_eq!(record["command"], "cargo test --quiet");
    assert_eq!(record["repair_requested"], Value::Bool(false));
    assert_eq!(record["decision"], "passed_without_repair");
    assert_eq!(record["initial_run"]["command"], "cargo test --quiet");
    assert_eq!(record["initial_run"]["success"], Value::Bool(true));
    assert_eq!(record["initial_run"]["exit_code"], 0);
    assert!(record["initial_run"]["parsed_failure"].is_null());
    assert!(record["repair"].is_null());
    assert!(record["confirmation_run"].is_null());

    assert!(artifact_root.join("watch.json").exists());
    assert!(artifact_root.join("initial.stdout.txt").exists());
    assert!(artifact_root.join("initial.stderr.txt").exists());
    assert!(
        !artifact_root.join("initial.failure.json").exists(),
        "successful watch command should not emit parsed failure artifact"
    );
    assert!(
        !artifact_root.join("repair").exists(),
        "no nested fix artifacts should exist for a successful command"
    );
    assert!(
        !artifact_root.join("confirmation.stdout.txt").exists(),
        "successful command should not trigger a confirmation rerun"
    );
    assert!(
        !artifact_root.join("confirmation.stderr.txt").exists(),
        "successful command should not trigger a confirmation rerun"
    );
    assert!(
        !artifact_root.join("confirmation.failure.json").exists(),
        "successful command should not create confirmation parsed failure artifact"
    );

    drop(child);
}

#[test]
fn watch_repair_rejects_non_allowlisted_command_before_cycle() {
    let temp = tempfile::tempdir().expect("tempdir should be created");
    init_repo(temp.path());

    let bin = assert_cmd::cargo::cargo_bin!("mercury-cli");
    Command::new(bin)
        .current_dir(temp.path())
        .arg("watch")
        .arg("false")
        .arg("--repair")
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "watch command rejected: command not allowlisted",
        ));

    assert!(
        !temp.path().join(".mercury").join("runs").exists(),
        "allowlist rejection should happen before creating watch artifacts"
    );
}

#[test]
fn watch_without_repair_supported_env_wrapped_command_writes_initial_failure_json() {
    let temp = tempfile::tempdir().expect("tempdir should be created");
    init_repo(temp.path());
    write_failing_rust_library(temp.path());

    let bin = assert_cmd::cargo::cargo_bin!("mercury-cli");
    let child = ChildGuard::spawn(
        Command::new(bin)
            .current_dir(temp.path())
            .arg("watch")
            .arg("env RUST_BACKTRACE=1 cargo test --quiet")
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
    );

    let (artifact_root, record) = wait_for_watch_record(temp.path());
    assert_eq!(record["decision"], "failed_without_repair");
    assert_eq!(record["initial_run"]["success"], Value::Bool(false));
    assert!(record["repair"].is_null());
    assert!(record["confirmation_run"].is_null());

    assert!(artifact_root.join("initial.failure.json").exists());
    assert!(
        !artifact_root.join("confirmation.failure.json").exists(),
        "no confirmation run exists without --repair"
    );
    let initial_failure = read_json(artifact_root.join("initial.failure.json"));
    assert_eq!(initial_failure["command"], "Test");
    assert_eq!(initial_failure["stage"], "Test");
    assert!(
        initial_failure["failures"]
            .as_array()
            .is_some_and(|failures| !failures.is_empty()),
        "parsed failure artifact should include at least one parsed failure"
    );

    drop(child);
}

#[test]
fn watch_with_supported_rust_repair_records_nested_and_fix_artifact_bundles() {
    let temp = tempfile::tempdir().expect("tempdir should be created");
    init_repo(temp.path());
    let fixed_source = write_failing_rust_library(temp.path());
    let stub = StubServer::start(fixed_source.clone());
    rewrite_config_for_stub(temp.path(), &stub);

    let bin = assert_cmd::cargo::cargo_bin!("mercury-cli");
    let child = ChildGuard::spawn(
        Command::new(bin)
            .current_dir(temp.path())
            .env("INCEPTION_API_KEY", "test-inception-key")
            .arg("watch")
            .arg("RUST_BACKTRACE=1 cargo test --quiet")
            .arg("--repair")
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
    );

    let (artifact_root, record) = wait_for_watch_record(temp.path());
    let repair = record["repair"]
        .as_object()
        .expect("repair record should be present");

    assert_eq!(record["command"], "RUST_BACKTRACE=1 cargo test --quiet");
    assert_eq!(record["repair_requested"], Value::Bool(true));
    assert_ne!(
        record["decision"], "repair_not_supported",
        "direct allowlisted Rust verifier command should remain supported for watch repair"
    );
    assert_eq!(record["initial_run"]["success"], Value::Bool(false));
    assert!(
        record["initial_run"]["parsed_failure"].is_object(),
        "supported cargo command failure should include structured parsed failure"
    );
    assert!(
        record["confirmation_run"].is_object(),
        "supported watch repair should rerun the verifier and record a confirmation run"
    );
    assert_eq!(repair["supported"], Value::Bool(true));
    assert_eq!(
        repair["verifier_command"],
        Value::String("RUST_BACKTRACE=1 cargo test --quiet".to_string())
    );
    assert!(repair["accepted_steps"].as_u64().is_some());
    assert!(repair["final_bundle_verified"].is_boolean());
    assert!(repair["applied"].is_boolean());
    assert!(repair["error"].is_null());

    assert!(artifact_root.join("watch.json").exists());
    assert!(artifact_root.join("initial.stdout.txt").exists());
    assert!(artifact_root.join("initial.stderr.txt").exists());
    assert!(artifact_root.join("initial.failure.json").exists());
    assert!(artifact_root.join("confirmation.stdout.txt").exists());
    assert!(artifact_root.join("confirmation.stderr.txt").exists());
    let confirmation_success = record["confirmation_run"]["success"]
        .as_bool()
        .expect("confirmation success should be a boolean");
    if confirmation_success {
        assert!(
            !artifact_root.join("confirmation.failure.json").exists(),
            "successful confirmation run should not emit failure artifact"
        );
    } else {
        assert!(
            artifact_root.join("confirmation.failure.json").exists(),
            "failing confirmation run should emit structured failure when available"
        );
    }
    if let Some(fix_artifact_root) = repair["fix_artifact_root"].as_str().map(PathBuf::from) {
        wait_for_mirrored_repair_artifacts(&artifact_root, &fix_artifact_root);
        assert!(fix_artifact_root.exists());
        assert!(artifact_root
            .join("repair")
            .join("execution-summary.json")
            .exists());
        assert_eq!(
            artifact_root
                .join("repair")
                .join("final-verification.json")
                .exists(),
            fix_artifact_root.join("final-verification.json").exists(),
            "watch artifacts should mirror final-verification presence from fix artifacts"
        );
        assert!(artifact_root.join("repair").join("metadata.json").exists());
        assert!(artifact_root
            .join("repair")
            .join("benchmark-run.json")
            .exists());
        assert!(artifact_root.join("repair").join("plan.json").exists());
        assert_eq!(
            artifact_root
                .join("repair")
                .join("grounded-context.json")
                .exists(),
            fix_artifact_root.join("grounded-context.json").exists(),
            "watch artifacts should mirror grounded-context presence from fix artifacts"
        );
        assert_eq!(
            artifact_root.join("repair").join("diff.patch").exists(),
            fix_artifact_root.join("diff.patch").exists(),
            "watch artifacts should mirror diff.patch presence from fix artifacts"
        );
        assert!(fix_artifact_root.join("plan.json").exists());
        assert!(fix_artifact_root.join("assessments.json").exists());
        assert!(fix_artifact_root.join("execution-summary.json").exists());
        assert!(fix_artifact_root.join("metadata.json").exists());
        assert!(fix_artifact_root.join("benchmark-run.json").exists());

        if repair["applied"] == Value::Bool(true) && confirmation_success {
            assert_eq!(
                fs::read_to_string(temp.path().join("src/lib.rs"))
                    .expect("fixed source should be readable"),
                fixed_source
            );
        }

        let fix_metadata: Value = serde_json::from_slice(
            &fs::read(fix_artifact_root.join("metadata.json"))
                .expect("fix metadata should be readable"),
        )
        .expect("fix metadata should parse");
        assert!(fix_metadata["final_bundle_verified"].is_boolean());
        assert!(fix_metadata["applied"].is_boolean());

        let fix_benchmark = read_json(fix_artifact_root.join("benchmark-run.json"));
        let mirrored_benchmark = read_json(artifact_root.join("repair").join("benchmark-run.json"));
        assert_eq!(
            fix_benchmark["schema_version"],
            Value::String("mercury-repair-benchmark-case-v1".to_string())
        );
        assert_eq!(mirrored_benchmark, fix_benchmark);
        assert_eq!(
            fix_benchmark["accepted_steps"], repair["accepted_steps"],
            "benchmark accepted_steps should match the watch repair record"
        );
        assert_eq!(
            fix_benchmark["final_bundle_verified"], repair["final_bundle_verified"],
            "benchmark final_bundle_verified should match the watch repair record"
        );
        assert_eq!(
            fix_benchmark["final_bundle_verified"], fix_metadata["final_bundle_verified"],
            "benchmark final_bundle_verified should match fix metadata"
        );
        assert_eq!(
            fix_benchmark["applied"], repair["applied"],
            "benchmark applied should match the watch repair record"
        );
        assert!(fix_benchmark["accepted_patch"].is_boolean());
        assert!(
            fix_benchmark.get("false_green").is_none(),
            "raw benchmark-run.json should not claim false_green before the independent rerun"
        );
        assert!(
            fix_benchmark["verifier"]["test_command"]
                .as_str()
                .is_some_and(|command| command.contains("cargo test")),
            "benchmark verifier test command should record the cargo test invocation"
        );
    }

    let requests = stub.recorded_requests();
    let accepted_steps = repair["accepted_steps"]
        .as_u64()
        .expect("accepted_steps should be a u64");
    assert!(
        requests
            .iter()
            .any(|request| request.path == "/v1/chat/completions"),
        "watch repair should hit Mercury 2 planning"
    );
    if accepted_steps > 0 {
        assert!(
            requests
                .iter()
                .any(|request| request.path == "/v1/apply/completions"),
            "watch repair with accepted steps should hit Mercury Edit apply"
        );
    }
    assert!(
        requests.iter().any(|request| {
            request.path == "/v1/chat/completions"
                && request.body.contains("RUST_BACKTRACE=1 cargo test --quiet")
        }),
        "planner request should include the env-prefixed verifier command"
    );

    drop(child);
}

#[test]
fn watch_repair_persistent_failure_emits_confirmation_failure_json() {
    let temp = tempfile::tempdir().expect("tempdir should be created");
    init_repo(temp.path());
    write_failing_rust_library(temp.path());
    let unchanged_source = fs::read_to_string(temp.path().join("src/lib.rs"))
        .expect("broken source should be readable");
    let stub = StubServer::start(unchanged_source);
    rewrite_config_for_stub(temp.path(), &stub);

    let bin = assert_cmd::cargo::cargo_bin!("mercury-cli");
    let child = ChildGuard::spawn(
        Command::new(bin)
            .current_dir(temp.path())
            .env("INCEPTION_API_KEY", "test-inception-key")
            .arg("watch")
            .arg("env RUST_BACKTRACE=1 cargo test --quiet")
            .arg("--repair")
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
    );

    let (artifact_root, record) = wait_for_watch_record(temp.path());
    assert!(
        matches!(
            record["decision"].as_str(),
            Some("repair_not_applied") | Some("repair_applied_but_command_still_failing")
        ),
        "persistent failure should never report repaired_and_verified"
    );
    assert_eq!(record["initial_run"]["success"], Value::Bool(false));
    assert_eq!(record["confirmation_run"]["success"], Value::Bool(false));
    assert!(artifact_root.join("initial.failure.json").exists());
    assert!(artifact_root.join("confirmation.failure.json").exists());
    let confirmation_failure = read_json(artifact_root.join("confirmation.failure.json"));
    assert_eq!(confirmation_failure["command"], "Test");
    assert_eq!(confirmation_failure["stage"], "Test");
    assert!(
        confirmation_failure["failures"]
            .as_array()
            .is_some_and(|failures| !failures.is_empty()),
        "confirmation parsed failure should include at least one parsed failure"
    );

    drop(child);
}

#[test]
fn config_set_round_trips_supported_scalar_keys_and_validates() {
    let temp = tempfile::tempdir().expect("tempdir should be created");
    init_repo(temp.path());

    let bin = mercury_bin();
    Command::new(&bin)
        .current_dir(temp.path())
        .args(["config", "set", "scheduler.max_concurrency", "7"])
        .assert()
        .success();

    Command::new(&bin)
        .current_dir(temp.path())
        .args(["config", "set", "verification.test_after_write", "false"])
        .assert()
        .success();

    let max_concurrency =
        run_cli_capture_stdout(temp.path(), ["config", "get", "scheduler.max_concurrency"]);
    assert!(
        max_concurrency.contains('7'),
        "config get should report the updated max_concurrency value"
    );

    let test_after_write = run_cli_capture_stdout(
        temp.path(),
        ["config", "get", "verification.test_after_write"],
    );
    assert!(
        test_after_write.contains("false"),
        "config get should report the updated test_after_write flag"
    );

    let config = fs::read_to_string(config_path(temp.path())).expect("config should be readable");
    assert!(
        config.contains("max_concurrency = 7"),
        "config file should persist the updated max_concurrency"
    );
    assert!(
        config.contains("test_after_write = false"),
        "config file should persist the updated test_after_write flag"
    );

    Command::new(&bin)
        .current_dir(temp.path())
        .args(["config", "validate"])
        .assert()
        .success();
}

#[test]
fn config_set_rejects_unknown_key_without_mutating_config() {
    let temp = tempfile::tempdir().expect("tempdir should be created");
    init_repo(temp.path());

    let config_before =
        fs::read_to_string(config_path(temp.path())).expect("config should be readable");
    let bin = mercury_bin();
    Command::new(&bin)
        .current_dir(temp.path())
        .args(["config", "set", "scheduler.not_a_real_key", "7"])
        .assert()
        .failure();

    let config_after =
        fs::read_to_string(config_path(temp.path())).expect("config should still be readable");
    assert_eq!(
        config_after, config_before,
        "invalid keys should not partially rewrite config.toml"
    );
}

#[test]
fn config_set_rejects_type_mismatch_without_mutating_config() {
    let temp = tempfile::tempdir().expect("tempdir should be created");
    init_repo(temp.path());

    let config_before =
        fs::read_to_string(config_path(temp.path())).expect("config should be readable");
    let bin = mercury_bin();
    Command::new(&bin)
        .current_dir(temp.path())
        .args(["config", "set", "scheduler.max_concurrency", "false"])
        .assert()
        .failure();

    let config_after =
        fs::read_to_string(config_path(temp.path())).expect("config should still be readable");
    assert_eq!(
        config_after, config_before,
        "type mismatches should not partially rewrite config.toml"
    );
}

#[test]
fn edit_apply_request_wraps_concrete_replacement_content_in_update_snippet() {
    let temp = tempfile::tempdir().expect("tempdir should be created");
    init_repo(temp.path());

    let original = r#"pub fn add(left: i32, right: i32) -> i32 {
    left - right
}
"#;
    let replacement = r#"pub fn add(left: i32, right: i32) -> i32 {
    left + right
}
"#;
    fs::create_dir_all(temp.path().join("src")).expect("src directory should be created");
    fs::write(temp.path().join("src/lib.rs"), original).expect("fixture source should be written");

    let stub = StubServer::start(replacement.to_string());
    rewrite_config_for_stub(temp.path(), &stub);

    let bin = mercury_bin();
    Command::new(&bin)
        .current_dir(temp.path())
        .env("INCEPTION_API_KEY", "test-inception-key")
        .arg("edit")
        .arg("apply")
        .arg("src/lib.rs")
        .arg("--update-snippet")
        .arg(replacement)
        .arg("--dry-run")
        .assert()
        .success();

    let requests = stub.recorded_requests();
    let request = request_for_path(&requests, "/v1/apply/completions");
    let prompt = request_prompt(request);
    assert_eq!(
        extract_tag(&prompt, "original_code")
            .expect("apply request should include original_code")
            .trim(),
        original.trim()
    );
    assert_eq!(
        extract_tag(&prompt, "update_snippet")
            .expect("apply request should include update_snippet")
            .trim(),
        replacement.trim(),
        "apply requests should forward concrete replacement content as update_snippet"
    );
    assert_ne!(
        extract_tag(&prompt, "original_code")
            .expect("apply request should include original_code")
            .trim(),
        extract_tag(&prompt, "update_snippet")
            .expect("apply request should include update_snippet")
            .trim(),
        "the update_snippet contract should not collapse to the original file content"
    );
}

#[test]
fn edit_next_request_populates_code_cursor_and_recent_snippets_context() {
    let temp = tempfile::tempdir().expect("tempdir should be created");
    init_repo(temp.path());

    let source = r#"pub fn add(left: i32, right: i32) -> i32 {
    let total = left - right;
    total
}

#[cfg(test)]
mod tests {
    use super::add;

    #[test]
    fn adds_numbers() {
        assert_eq!(add(2, 2), 4);
    }
}
"#;
    fs::create_dir_all(temp.path().join("src")).expect("src directory should be created");
    fs::write(temp.path().join("src/lib.rs"), source).expect("fixture source should be written");

    let stub = StubServer::start("Replace subtraction with addition.".to_string());
    rewrite_config_for_stub(temp.path(), &stub);

    let bin = mercury_bin();
    Command::new(&bin)
        .current_dir(temp.path())
        .env("INCEPTION_API_KEY", "test-inception-key")
        .arg("edit")
        .arg("next")
        .arg("src/lib.rs:2")
        .assert()
        .success();

    let requests = stub.recorded_requests();
    let request = request_for_path(&requests, "/v1/edit/completions");
    let prompt = request_prompt(request);
    assert!(
        prompt.contains("current_file_path: src/lib.rs"),
        "next-edit requests should preserve the current file path"
    );

    let recent_snippets = extract_tag(&prompt, "recently_viewed_code_snippets")
        .expect("next-edit request should include recently_viewed_code_snippets");
    assert!(
        !recent_snippets.trim().is_empty(),
        "recent_snippets should be materially populated"
    );
    assert!(
        recent_snippets.contains("src/lib.rs")
            || recent_snippets.contains("let total = left - right;")
            || recent_snippets.contains("pub fn add"),
        "recent_snippets should carry concrete file context"
    );

    let code_to_edit = extract_tag(&prompt, "code_to_edit")
        .expect("next-edit request should include code_to_edit");
    let code_without_cursor = strip_tag(&code_to_edit, "cursor");
    assert!(
        !code_without_cursor.trim().is_empty(),
        "code_to_edit should include concrete code around the cursor"
    );
    assert!(
        code_without_cursor.contains("let total = left - right;")
            || code_without_cursor.contains("pub fn add"),
        "code_to_edit should capture current-file source context"
    );

    let cursor = extract_tag(&prompt, "cursor").expect("next-edit request should include cursor");
    assert!(
        !cursor.trim().is_empty(),
        "cursor should be materially populated"
    );
    assert!(
        !prompt.contains("<|recently_viewed_code_snippets|>\n\n<|/recently_viewed_code_snippets|>"),
        "next-edit requests should not emit an empty recent_snippets wrapper"
    );
    assert!(
        !prompt.contains("<|cursor|>\n\n<|/cursor|>"),
        "next-edit requests should not emit an empty cursor wrapper"
    );
}

#[test]
fn fix_failure_writes_benchmark_and_metadata_artifacts() {
    let temp = tempfile::tempdir().expect("tempdir should be created");
    init_repo(temp.path());
    write_failing_rust_library(temp.path());

    let stub = StubServer::start_with_planner_error("planner unavailable");
    rewrite_config_for_stub(temp.path(), &stub);

    let bin = mercury_bin();
    Command::new(&bin)
        .current_dir(temp.path())
        .env("INCEPTION_API_KEY", "test-inception-key")
        .args([
            "fix",
            "repair the failing Rust test",
            "--max-agents",
            "2",
            "--max-cost",
            "0.25",
            "--noninteractive",
        ])
        .assert()
        .failure();

    let artifact_root = wait_for_latest_run_dir(temp.path());
    assert!(
        artifact_root.join("metadata.json").exists(),
        "failed fix runs should still write metadata.json"
    );
    assert!(
        artifact_root.join("benchmark-run.json").exists(),
        "failed fix runs should still write benchmark-run.json"
    );

    let metadata = read_json(artifact_root.join("metadata.json"));
    let benchmark = read_json(artifact_root.join("benchmark-run.json"));

    assert_eq!(
        benchmark["schema_version"],
        Value::String("mercury-repair-benchmark-case-v1".to_string())
    );
    assert_eq!(
        benchmark["outcome"],
        Value::String("fix_failed".to_string())
    );
    assert_eq!(benchmark["accepted_patch"], Value::Bool(false));
    assert_eq!(benchmark["final_bundle_verified"], Value::Bool(false));
    assert_eq!(benchmark["applied"], Value::Bool(false));
    assert!(
        benchmark.get("false_green").is_none(),
        "failed fix benchmark artifact should not claim false_green"
    );

    assert_eq!(metadata["final_bundle_verified"], Value::Bool(false));
    assert_eq!(metadata["applied"], Value::Bool(false));
    assert_eq!(metadata["planner_estimated_cost_usd"], Value::from(0.0));
    assert_eq!(metadata["grounding_collected"], Value::Bool(false));

    let requests = stub.recorded_requests();
    assert!(
        requests
            .iter()
            .any(|request| request.path == "/v1/chat/completions"),
        "failing fix run should still reach the planner endpoint before writing failure artifacts"
    );
}

fn mercury_bin() -> PathBuf {
    assert_cmd::cargo::cargo_bin!("mercury-cli").to_path_buf()
}

fn init_repo(root: &Path) {
    let bin = mercury_bin();
    Command::new(bin)
        .current_dir(root)
        .arg("init")
        .assert()
        .success();
}

fn write_failing_rust_library(root: &Path) -> String {
    fs::write(
        root.join("Cargo.toml"),
        r#"[package]
name = "watch-artifacts-fixture"
version = "0.1.0"
edition = "2021"

[lib]
path = "src/lib.rs"
"#,
    )
    .expect("cargo manifest should be written");
    fs::create_dir_all(root.join("src")).expect("src directory should be created");
    fs::write(
        root.join("src/lib.rs"),
        r#"pub fn add(left: i32, right: i32) -> i32 {
    left - right
}

#[cfg(test)]
mod tests {
    use super::add;

    #[test]
    fn adds_numbers() {
        assert_eq!(add(2, 2), 4);
    }
}
"#,
    )
    .expect("broken source should be written");

    r#"pub fn add(left: i32, right: i32) -> i32 {
    left + right
}

#[cfg(test)]
mod tests {
    use super::add;

    #[test]
    fn adds_numbers() {
        assert_eq!(add(2, 2), 4);
    }
}
"#
    .to_string()
}

fn write_passing_rust_library(root: &Path) {
    fs::write(
        root.join("Cargo.toml"),
        r#"[package]
name = "watch-artifacts-passing-fixture"
version = "0.1.0"
edition = "2021"

[lib]
path = "src/lib.rs"
"#,
    )
    .expect("cargo manifest should be written");
    fs::create_dir_all(root.join("src")).expect("src directory should be created");
    fs::write(
        root.join("src/lib.rs"),
        r#"pub fn add(left: i32, right: i32) -> i32 {
    left + right
}

#[cfg(test)]
mod tests {
    use super::add;

    #[test]
    fn adds_numbers() {
        assert_eq!(add(2, 2), 4);
    }
}
"#,
    )
    .expect("passing source should be written");
}

fn config_path(project_root: &Path) -> PathBuf {
    project_root.join(".mercury").join("config.toml")
}

fn rewrite_config_for_stub(project_root: &Path, stub: &StubServer) {
    let config_path = config_path(project_root);
    let config = fs::read_to_string(&config_path).expect("config should be readable");
    let config = config.replace(
        "mercury2_endpoint = \"https://api.inceptionlabs.ai/v1/chat/completions\"",
        &format!("mercury2_endpoint = \"{}\"", stub.mercury2_endpoint()),
    );
    let config = config.replace(
        "mercury_edit_endpoint = \"https://api.inceptionlabs.ai/v1\"",
        &format!(
            "mercury_edit_endpoint = \"{}\"",
            stub.mercury_edit_endpoint()
        ),
    );
    let config = config.replace(
        "mercury2_critique_on_failure = true",
        "mercury2_critique_on_failure = false",
    );
    fs::write(config_path, config).expect("config should be rewritten");
}

fn run_cli_capture_stdout<const N: usize>(project_root: &Path, args: [&str; N]) -> String {
    let output = Command::new(mercury_bin())
        .current_dir(project_root)
        .args(args)
        .output()
        .expect("command should run");
    assert!(
        output.status.success(),
        "command should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn wait_for_watch_record(project_root: &Path) -> (PathBuf, Value) {
    let deadline = Instant::now() + Duration::from_secs(30);
    let runs_root = project_root.join(".mercury").join("runs");

    loop {
        if let Some(result) = read_latest_watch_record(&runs_root) {
            return result;
        }

        assert!(
            Instant::now() < deadline,
            "timed out waiting for watch artifact bundle under {}",
            runs_root.display()
        );

        thread::sleep(Duration::from_millis(100));
    }
}

fn wait_for_latest_run_dir(project_root: &Path) -> PathBuf {
    let deadline = Instant::now() + Duration::from_secs(30);
    let runs_root = project_root.join(".mercury").join("runs");

    loop {
        if let Some(run_dir) = read_latest_run_dir(&runs_root) {
            return run_dir;
        }

        assert!(
            Instant::now() < deadline,
            "timed out waiting for run artifact bundle under {}",
            runs_root.display()
        );

        thread::sleep(Duration::from_millis(100));
    }
}

fn wait_for_mirrored_repair_artifacts(artifact_root: &Path, fix_artifact_root: &Path) {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mirrored_repair_root = artifact_root.join("repair");

    loop {
        let fix_exists = fix_artifact_root.exists();
        let mirrored_exists = mirrored_repair_root.exists();
        let mandatory_mirrored = mirrored_repair_root.join("execution-summary.json").exists()
            && mirrored_repair_root.join("metadata.json").exists()
            && mirrored_repair_root.join("benchmark-run.json").exists()
            && mirrored_repair_root.join("plan.json").exists();
        let optional_mirrors_match = mirrored_repair_root
            .join("final-verification.json")
            .exists()
            == fix_artifact_root.join("final-verification.json").exists()
            && mirrored_repair_root.join("grounded-context.json").exists()
                == fix_artifact_root.join("grounded-context.json").exists()
            && mirrored_repair_root.join("diff.patch").exists()
                == fix_artifact_root.join("diff.patch").exists();

        if fix_exists && mirrored_exists && mandatory_mirrored && optional_mirrors_match {
            return;
        }

        assert!(
            Instant::now() < deadline,
            "timed out waiting for mirrored repair artifacts under {}",
            artifact_root.display()
        );
        thread::sleep(Duration::from_millis(50));
    }
}

fn read_latest_watch_record(runs_root: &Path) -> Option<(PathBuf, Value)> {
    for artifact_root in read_run_dirs_descending(runs_root)? {
        let watch_path = artifact_root.join("watch.json");
        if !watch_path.exists() {
            continue;
        }

        let record = serde_json::from_slice(&fs::read(&watch_path).ok()?).ok()?;
        return Some((artifact_root, record));
    }

    None
}

fn read_latest_run_dir(runs_root: &Path) -> Option<PathBuf> {
    read_run_dirs_descending(runs_root)?.into_iter().next()
}

fn read_run_dirs_descending(runs_root: &Path) -> Option<Vec<PathBuf>> {
    let mut run_dirs = fs::read_dir(runs_root)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    run_dirs.sort();
    run_dirs.reverse();
    Some(run_dirs)
}

fn request_for_path<'a>(requests: &'a [RecordedRequest], path: &str) -> &'a RecordedRequest {
    requests
        .iter()
        .find(|request| request.path == path)
        .unwrap_or_else(|| panic!("expected request for path {path}, saw: {requests:?}"))
}

fn request_prompt(request: &RecordedRequest) -> String {
    let body: Value =
        serde_json::from_str(&request.body).expect("recorded request body should parse as JSON");
    body["messages"][0]["content"]
        .as_str()
        .expect("chat request should include a text prompt")
        .to_string()
}

fn extract_tag(content: &str, tag: &str) -> Option<String> {
    let start = format!("<|{tag}|>\n");
    let end = format!("\n<|/{tag}|>");
    let from = content.find(&start)? + start.len();
    let rest = &content[from..];
    let to = rest.find(&end)?;
    Some(rest[..to].to_string())
}

fn strip_tag(content: &str, tag: &str) -> String {
    let start = format!("<|{tag}|>\n");
    let end = format!("\n<|/{tag}|>");
    if let Some(inner) = extract_tag(content, tag) {
        content.replacen(&(start + &inner + &end), "", 1)
    } else {
        content.to_string()
    }
}

fn handle_stub_connection(
    stream: &mut TcpStream,
    fixed_source: &str,
    planner_failure: Option<&(String, String)>,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let Some((path, body)) = read_http_request(stream)? else {
        return Ok(());
    };

    requests
        .lock()
        .expect("request log should not be poisoned")
        .push(RecordedRequest {
            path: path.clone(),
            body,
        });

    let (status_line, response_body) = match path.as_str() {
        "/v1/chat/completions" => {
            if let Some((status_line, response_body)) = planner_failure {
                (status_line.clone(), response_body.clone())
            } else {
                (
                    "HTTP/1.1 200 OK".to_string(),
                    json!({
                        "choices": [{
                            "message": {
                                "content": serde_json::to_string(&json!({
                                    "schema_version": "planner-response-v1",
                                    "steps": [{
                                        "file_path": "src/lib.rs",
                                        "instruction": "Replace the broken subtraction with addition so the Rust test passes.",
                                        "priority": 1.0,
                                        "estimated_tokens": 128
                                    }],
                                    "assessments": [{
                                        "complexity_score": 0.2,
                                        "dependency_score": 0.1,
                                        "risk_score": 0.2,
                                        "churn_score": 0.1,
                                        "suggested_action": "refactor",
                                        "reasoning": "Single-file Rust test repair."
                                    }]
                                }))
                                .expect("planner payload should serialize")
                            }
                        }],
                        "usage": {
                            "prompt_tokens": 10,
                            "completion_tokens": 20,
                            "total_tokens": 30,
                            "cached_input_tokens": 0
                        }
                    })
                    .to_string(),
                )
            }
        }
        "/v1/apply/completions" => (
            "HTTP/1.1 200 OK".to_string(),
            json!({
                "choices": [{
                    "message": {
                        "content": fixed_source
                    }
                }],
                "usage": {
                    "prompt_tokens": 12,
                    "completion_tokens": 24,
                    "total_tokens": 36,
                    "cached_input_tokens": 0
                }
            })
            .to_string(),
        ),
        "/v1/edit/completions" => (
            "HTTP/1.1 200 OK".to_string(),
            json!({
                "choices": [{
                    "message": {
                        "content": fixed_source
                    }
                }],
                "usage": {
                    "prompt_tokens": 8,
                    "completion_tokens": 16,
                    "total_tokens": 24,
                    "cached_input_tokens": 0
                }
            })
            .to_string(),
        ),
        _ => (
            "HTTP/1.1 404 Not Found".to_string(),
            json!({
                "choices": [{
                    "message": {
                        "content": ""
                    }
                }],
                "usage": {
                    "prompt_tokens": 0,
                    "completion_tokens": 0,
                    "total_tokens": 0,
                    "cached_input_tokens": 0
                }
            })
            .to_string(),
        ),
    };

    write_http_response(stream, &status_line, "application/json", &response_body)
}

fn read_http_request(stream: &mut TcpStream) -> std::io::Result<Option<(String, String)>> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        match stream.read(&mut chunk) {
            Ok(0) if buffer.is_empty() => return Ok(None),
            Ok(0) => break None,
            Ok(read) => {
                buffer.extend_from_slice(&chunk[..read]);
                if let Some(index) = find_bytes(&buffer, b"\r\n\r\n") {
                    break Some(index + 4);
                }
            }
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Ok(None)
            }
            Err(err) => return Err(err),
        }
    };

    let Some(header_end) = header_end else {
        return Ok(None);
    };
    let headers = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let path = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/")
        .to_string();
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.eq_ignore_ascii_case("content-length") {
                value.trim().parse::<usize>().ok()
            } else {
                None
            }
        })
        .unwrap_or(0);

    while buffer.len() < header_end + content_length {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                break
            }
            Err(err) => return Err(err),
        }
    }

    let body = if buffer.len() >= header_end {
        let available = buffer.len().saturating_sub(header_end);
        let body_len = content_length.min(available);
        String::from_utf8_lossy(&buffer[header_end..header_end + body_len]).to_string()
    } else {
        String::new()
    };

    Ok(Some((path, body)))
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn write_http_response(
    stream: &mut TcpStream,
    status_line: &str,
    content_type: &str,
    body: &str,
) -> std::io::Result<()> {
    write!(
        stream,
        "{status_line}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()
}

fn read_json(path: PathBuf) -> Value {
    serde_json::from_slice(&fs::read(path).expect("json artifact should be readable"))
        .expect("json artifact should parse")
}
