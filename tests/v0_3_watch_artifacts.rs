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
fn watch_without_repair_writes_failure_artifact_bundle() {
    let temp = tempfile::tempdir().expect("tempdir should be created");
    init_repo(temp.path());

    let bin = assert_cmd::cargo::cargo_bin!("mercury-cli");
    let child = ChildGuard::spawn(
        Command::new(&bin)
            .current_dir(temp.path())
            .arg("watch")
            .arg("false")
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
    );

    let (artifact_root, record) = wait_for_watch_record(temp.path());

    assert_eq!(record["command"], "false");
    assert_eq!(record["repair_requested"], Value::Bool(false));
    assert_eq!(record["decision"], "failed_without_repair");
    assert_eq!(record["initial_run"]["command"], "false");
    assert_eq!(record["initial_run"]["success"], Value::Bool(false));
    assert!(record["initial_run"]["exit_code"].as_i64().is_some());
    assert!(
        record["started_at"]
            .as_str()
            .is_some_and(|value| !value.is_empty()),
        "watch record should include started_at"
    );
    assert!(
        record["finished_at"]
            .as_str()
            .is_some_and(|value| !value.is_empty()),
        "watch record should include finished_at"
    );
    assert!(
        record["duration_ms"]
            .as_u64()
            .is_some_and(|value| value > 0),
        "watch record should include positive duration"
    );
    assert!(record["repair"].is_null());
    assert!(record["confirmation_run"].is_null());
    assert!(record["initial_run"]["parsed_failure"].is_null());

    assert!(artifact_root.join("watch.json").exists());
    assert!(artifact_root.join("initial.stdout.txt").exists());
    assert!(artifact_root.join("initial.stderr.txt").exists());
    assert!(
        !artifact_root.join("initial.failure.json").exists(),
        "unsupported non-cargo command should not emit parsed failure artifact"
    );
    assert!(
        !artifact_root.join("confirmation.stdout.txt").exists(),
        "no confirmation output should exist without a repair rerun"
    );
    assert!(
        !artifact_root.join("confirmation.stderr.txt").exists(),
        "no confirmation output should exist without a repair rerun"
    );
    assert!(
        !artifact_root.join("confirmation.failure.json").exists(),
        "no confirmation parsed failure should exist without a repair rerun"
    );

    drop(child);
}

#[test]
fn watch_without_repair_records_passed_decision_for_successful_command() {
    let temp = tempfile::tempdir().expect("tempdir should be created");
    init_repo(temp.path());

    let bin = assert_cmd::cargo::cargo_bin!("mercury-cli");
    let child = ChildGuard::spawn(
        Command::new(&bin)
            .current_dir(temp.path())
            .arg("watch")
            .arg("true")
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
    );

    let (artifact_root, record) = wait_for_watch_record(temp.path());

    assert_eq!(record["command"], "true");
    assert_eq!(record["repair_requested"], Value::Bool(false));
    assert_eq!(record["decision"], "passed_without_repair");
    assert_eq!(record["initial_run"]["command"], "true");
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
fn watch_with_unsupported_repair_command_records_reason_without_fix_artifacts() {
    let temp = tempfile::tempdir().expect("tempdir should be created");
    init_repo(temp.path());

    let bin = assert_cmd::cargo::cargo_bin!("mercury-cli");
    let child = ChildGuard::spawn(
        Command::new(&bin)
            .current_dir(temp.path())
            .arg("watch")
            .arg("false")
            .arg("--repair")
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
    );

    let (artifact_root, record) = wait_for_watch_record(temp.path());
    let repair = record["repair"]
        .as_object()
        .expect("repair record should be present");

    assert_eq!(record["command"], "false");
    assert_eq!(record["repair_requested"], Value::Bool(true));
    assert_eq!(record["decision"], "repair_not_supported");
    assert_eq!(record["initial_run"]["success"], Value::Bool(false));
    assert!(record["initial_run"]["parsed_failure"].is_null());
    assert_eq!(repair["supported"], Value::Bool(false));
    assert!(repair["verifier_command"].is_null());
    assert!(repair["fix_artifact_root"].is_null());
    assert_eq!(repair["accepted_steps"], 0);
    assert_eq!(repair["rejected_steps"], 0);
    assert_eq!(repair["verification_failures"], 0);
    assert_eq!(repair["final_bundle_verified"], Value::Bool(false));
    assert_eq!(repair["applied"], Value::Bool(false));
    assert!(
        repair["error"]
            .as_str()
            .is_some_and(|error| error.contains("cargo test, cargo check, cargo clippy")),
        "unsupported repair path should explain the supported verifier commands"
    );
    assert!(
        record["duration_ms"]
            .as_u64()
            .is_some_and(|value| value > 0),
        "watch run should capture runtime duration"
    );

    assert!(artifact_root.join("watch.json").exists());
    assert!(artifact_root.join("initial.stdout.txt").exists());
    assert!(artifact_root.join("initial.stderr.txt").exists());
    assert!(
        !artifact_root.join("initial.failure.json").exists(),
        "unsupported repair command should not emit parsed failure artifact"
    );
    assert!(
        !artifact_root.join("repair").exists(),
        "unsupported repair path should not create nested fix artifacts"
    );
    assert!(
        !artifact_root.join("confirmation.stdout.txt").exists(),
        "unsupported repair path should not rerun the verifier"
    );
    assert!(
        !artifact_root.join("confirmation.stderr.txt").exists(),
        "unsupported repair path should not rerun the verifier"
    );
    assert!(
        !artifact_root.join("confirmation.failure.json").exists(),
        "unsupported repair path should not emit confirmation failure artifact"
    );

    drop(child);
}

#[test]
fn watch_without_repair_supported_env_wrapped_command_writes_initial_failure_json() {
    let temp = tempfile::tempdir().expect("tempdir should be created");
    init_repo(temp.path());
    write_failing_rust_library(temp.path());

    let bin = assert_cmd::cargo::cargo_bin!("mercury-cli");
    let child = ChildGuard::spawn(
        Command::new(&bin)
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
        Command::new(&bin)
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
    assert_eq!(record["decision"], "repaired_and_verified");
    assert_eq!(record["initial_run"]["success"], Value::Bool(false));
    assert!(
        record["initial_run"]["parsed_failure"].is_object(),
        "supported cargo command failure should include structured parsed failure"
    );
    assert_eq!(
        record["confirmation_run"]["success"],
        Value::Bool(true),
        "watch repair should rerun the verifier successfully"
    );
    assert!(
        record["confirmation_run"]["parsed_failure"].is_null(),
        "successful confirmation run should not include parsed failure"
    );
    assert_eq!(repair["supported"], Value::Bool(true));
    assert_eq!(
        repair["verifier_command"],
        Value::String("RUST_BACKTRACE=1 cargo test --quiet".to_string())
    );
    assert!(
        repair["accepted_steps"]
            .as_u64()
            .is_some_and(|value| value >= 1),
        "repair should accept at least one plan step"
    );
    assert_eq!(repair["final_bundle_verified"], Value::Bool(true));
    assert_eq!(repair["applied"], Value::Bool(true));
    assert!(repair["error"].is_null());

    let fix_artifact_root = PathBuf::from(
        repair["fix_artifact_root"]
            .as_str()
            .expect("repair should record fix artifact root"),
    );
    assert!(fix_artifact_root.exists());

    assert!(artifact_root.join("watch.json").exists());
    assert!(artifact_root.join("initial.stdout.txt").exists());
    assert!(artifact_root.join("initial.stderr.txt").exists());
    assert!(artifact_root.join("initial.failure.json").exists());
    assert!(artifact_root.join("confirmation.stdout.txt").exists());
    assert!(artifact_root.join("confirmation.stderr.txt").exists());
    assert!(
        !artifact_root.join("confirmation.failure.json").exists(),
        "successful confirmation run should not emit failure artifact"
    );
    assert!(artifact_root.join("repair").join("diff.patch").exists());
    assert!(artifact_root
        .join("repair")
        .join("execution-summary.json")
        .exists());
    assert!(artifact_root
        .join("repair")
        .join("final-verification.json")
        .exists());
    assert!(artifact_root.join("repair").join("metadata.json").exists());
    assert!(artifact_root.join("repair").join("plan.json").exists());

    assert!(fix_artifact_root.join("plan.json").exists());
    assert_eq!(
        artifact_root
            .join("repair")
            .join("grounded-context.json")
            .exists(),
        fix_artifact_root.join("grounded-context.json").exists(),
        "watch artifacts should mirror grounded-context presence from fix artifacts"
    );
    assert!(fix_artifact_root.join("assessments.json").exists());
    assert!(fix_artifact_root.join("execution-summary.json").exists());
    assert!(fix_artifact_root.join("final-verification.json").exists());
    assert!(fix_artifact_root.join("agent-logs.json").exists());
    assert!(fix_artifact_root.join("thermal-aggregates.json").exists());
    assert!(fix_artifact_root.join("metadata.json").exists());
    assert!(fix_artifact_root.join("diff.patch").exists());

    assert_eq!(
        fs::read_to_string(temp.path().join("src/lib.rs"))
            .expect("fixed source should be readable"),
        fixed_source
    );

    let fix_metadata: Value = serde_json::from_slice(
        &fs::read(fix_artifact_root.join("metadata.json"))
            .expect("fix metadata should be readable"),
    )
    .expect("fix metadata should parse");
    assert_eq!(fix_metadata["final_bundle_verified"], Value::Bool(true));
    assert_eq!(fix_metadata["applied"], Value::Bool(true));

    let requests = stub.recorded_requests();
    assert!(
        requests
            .iter()
            .any(|request| request.path == "/v1/chat/completions"),
        "watch repair should hit Mercury 2 planning"
    );
    assert!(
        requests
            .iter()
            .any(|request| request.path == "/v1/apply/completions"),
        "watch repair should hit Mercury Edit apply"
    );
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
        Command::new(&bin)
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

fn init_repo(root: &Path) {
    let bin = assert_cmd::cargo::cargo_bin!("mercury-cli");
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

fn rewrite_config_for_stub(project_root: &Path, stub: &StubServer) {
    let config_path = project_root.join(".mercury").join("config.toml");
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

fn read_latest_watch_record(runs_root: &Path) -> Option<(PathBuf, Value)> {
    let mut run_dirs = fs::read_dir(runs_root)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    run_dirs.sort();
    for artifact_root in run_dirs.into_iter().rev() {
        let watch_path = artifact_root.join("watch.json");
        if !watch_path.exists() {
            continue;
        }

        let record = serde_json::from_slice(&fs::read(&watch_path).ok()?).ok()?;
        return Some((artifact_root, record));
    }

    None
}

fn handle_stub_connection(
    stream: &mut TcpStream,
    fixed_source: &str,
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

    let response_body = match path.as_str() {
        "/v1/chat/completions" => json!({
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
        "/v1/apply/completions" => json!({
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
        _ => json!({
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
    };

    let status_line = if matches!(
        path.as_str(),
        "/v1/chat/completions" | "/v1/apply/completions"
    ) {
        "HTTP/1.1 200 OK"
    } else {
        "HTTP/1.1 404 Not Found"
    };

    write_http_response(stream, status_line, "application/json", &response_body)
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
