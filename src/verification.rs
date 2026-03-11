use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{json, Value};
use thiserror::Error;

use crate::api::{
    ApiError, ApiUsage, Mercury2Api, ToolCall, ToolDefinition, ToolFunctionDefinition,
};
use crate::engine::VerifyConfig;
use crate::failure_parser::{
    classify_verifier_command, command_start_index, contains_shell_composition, env_option_arity,
    is_env_assignment, parse_command_parts, parse_verifier_failure, repo_native_tool_surface,
    ParsedFailureReport, RepoNativeTool, VerifierCommandKind,
};

pub const GROUNDED_REPAIR_CONTEXT_SCHEMA_NAME: &str = "grounded-repair-context-v1";
pub const READ_FILE_TOOL: &str = "read_file";
pub const SEARCH_SYMBOL_TOOL: &str = "search_symbol";
pub const RUN_TESTS_TOOL: &str = "run_tests";
pub const APPLY_PATCH_TEMP_TOOL: &str = "apply_patch_temp";
pub const GIT_DIFF_TOOL: &str = "git_diff";
pub const ROLLBACK_CANDIDATE_TOOL: &str = "rollback_candidate";

const MAX_GROUNDING_ROUNDS: usize = 2;
const MAX_GROUNDING_TOOL_CALLS: usize = 6;
const MAX_SUMMARY_CHARS: usize = 2400;
const SENSITIVE_ENV_VARS: &[&str] = &[
    "INCEPTION_API_KEY",
    "MERCURY_API_KEY",
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "GITHUB_TOKEN",
    "GH_TOKEN",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
];
const SENSITIVE_MARKERS: &[&str] = &[
    "token",
    "api_key",
    "apikey",
    "authorization",
    "password",
    "secret",
];
const NONINTERACTIVE_VERIFIER_ENV: &[(&str, &str)] = &[
    ("MERCURY_NONINTERACTIVE", "1"),
    ("NO_COLOR", "1"),
    ("CLICOLOR", "0"),
    ("CLICOLOR_FORCE", "0"),
    ("FORCE_COLOR", "0"),
    ("CARGO_TERM_COLOR", "never"),
    ("GIT_TERMINAL_PROMPT", "0"),
    ("TERM", "dumb"),
];
const CI_VERIFIER_ENV: &[(&str, &str)] = &[("CI", "1")];

fn redact_sensitive_text(text: &str) -> String {
    let mut redacted = text.to_string();

    for key in SENSITIVE_ENV_VARS {
        if let Ok(value) = env::var(key) {
            let trimmed = value.trim();
            if trimmed.len() >= 4 && redacted.contains(trimmed) {
                redacted = redacted.replace(trimmed, &format!("[REDACTED:{key}]"));
            }
        }
    }

    let mut sanitized_lines = Vec::new();
    for line in redacted.lines() {
        let mut sanitized = String::with_capacity(line.len());
        let mut idx = 0usize;
        while idx < line.len() {
            let ch = line[idx..].chars().next().unwrap();
            if ch.is_whitespace() {
                sanitized.push(ch);
                idx += ch.len_utf8();
                continue;
            }

            let start = idx;
            while idx < line.len() {
                let current = line[idx..].chars().next().unwrap();
                if current.is_whitespace() {
                    break;
                }
                idx += current.len_utf8();
            }
            let token = &line[start..idx];
            sanitized.push_str(&redact_sensitive_token(token));
        }

        if !line.contains('=') {
            if let Some(key) = sensitive_colon_prefix(&sanitized) {
                sanitized_lines.push(format!("{key}: [REDACTED]"));
                continue;
            }
        }
        sanitized_lines.push(sanitized);
    }

    let mut joined = sanitized_lines.join("\n");
    if text.ends_with('\n') && !joined.ends_with('\n') {
        joined.push('\n');
    }
    joined
}

fn is_sensitive_key(key: &str) -> bool {
    let normalized = key.trim().to_ascii_lowercase();
    let exact_marker = SENSITIVE_MARKERS
        .iter()
        .any(|marker| normalized == *marker || normalized == format!("{}_value", marker));
    let suffixed_marker = [
        "_token",
        "_api_key",
        "_apikey",
        "_password",
        "_secret",
        "-token",
        "-api-key",
        "-apikey",
        "-password",
        "-secret",
    ]
    .iter()
    .any(|suffix| normalized.ends_with(suffix));

    SENSITIVE_ENV_VARS
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(key))
        || exact_marker
        || suffixed_marker
}

fn redacted_secret_value() -> Value {
    Value::String("[REDACTED]".to_string())
}

fn redact_sensitive_token(token: &str) -> String {
    if let Some((key, _)) = token.split_once('=') {
        if is_sensitive_key(key) {
            return format!("{key}=[REDACTED]");
        }
    }
    if let Some((key, _)) = token.split_once(':') {
        if is_sensitive_key(key) {
            return format!("{key}:[REDACTED]");
        }
    }
    token.to_string()
}

fn sensitive_colon_prefix(line: &str) -> Option<&str> {
    let (prefix, _) = line.split_once(':')?;
    let key = prefix.trim();
    if is_sensitive_key(key) {
        Some(key)
    } else {
        None
    }
}

fn redact_sensitive_value(value: Value) -> Value {
    match value {
        Value::String(text) => Value::String(redact_sensitive_text(&text)),
        Value::Array(items) => {
            Value::Array(items.into_iter().map(redact_sensitive_value).collect())
        }
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, value)| {
                    let value = if is_sensitive_key(&key) {
                        redacted_secret_value()
                    } else {
                        redact_sensitive_value(value)
                    };
                    (key, value)
                })
                .collect(),
        ),
        other => other,
    }
}

fn redact_parsed_failure_report(report: ParsedFailureReport) -> ParsedFailureReport {
    let redacted = redact_sensitive_value(json!(report.clone()));
    serde_json::from_value(redacted).unwrap_or(report)
}

fn env_flag_enabled(name: &str) -> bool {
    matches!(
        env::var(name),
        Ok(value)
            if matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
    )
}

#[derive(Debug, Clone, Copy)]
struct VerifierRuntimeMode {
    ci: bool,
    noninteractive: bool,
}

fn verifier_runtime_mode() -> VerifierRuntimeMode {
    let ci = env_flag_enabled("CI") || env_flag_enabled("GITHUB_ACTIONS");
    let noninteractive = ci || env_flag_enabled("MERCURY_NONINTERACTIVE");
    VerifierRuntimeMode { ci, noninteractive }
}

#[derive(Debug, Clone)]
struct VerifierInvocation {
    command_kind: VerifierCommandKind,
    env_clear: bool,
    env_remove: Vec<String>,
    env_set: Vec<(String, String)>,
    program: String,
    args: Vec<String>,
}

fn parse_allowlisted_verifier_invocation(parts: &[String]) -> Result<VerifierInvocation, String> {
    if parts.is_empty() {
        return Err("command is empty".to_string());
    }

    let command_kind = classify_verifier_command(parts);
    if matches!(command_kind, VerifierCommandKind::Unknown) {
        return Err(format!("command not allowlisted: {}", parts.join(" ")));
    }

    let mut idx = 0usize;
    let mut env_clear = false;
    let mut env_remove = Vec::new();
    let mut env_set = Vec::new();

    while idx < parts.len() && is_env_assignment(&parts[idx]) {
        env_set.push(parse_env_assignment(&parts[idx])?);
        idx += 1;
    }

    if parts.get(idx).map(String::as_str) == Some("env") {
        idx += 1;
        while idx < parts.len() {
            let part = parts[idx].as_str();
            if part == "--" {
                idx += 1;
                break;
            }
            if part == "-i" {
                env_clear = true;
                idx += 1;
                continue;
            }
            if is_env_assignment(part) {
                env_set.push(parse_env_assignment(part)?);
                idx += 1;
                continue;
            }
            if let Some(consumes_next) = env_option_arity(part) {
                idx += 1;
                if part.starts_with("--unset=") {
                    if let Some((_, key)) = part.split_once('=') {
                        env_remove.push(key.to_string());
                    }
                    continue;
                }
                if consumes_next && !part.contains('=') {
                    let Some(value) = parts.get(idx) else {
                        return Err(format!("env wrapper option requires a value: {part}"));
                    };
                    if part == "-u" || part == "--unset" {
                        env_remove.push(value.clone());
                    }
                    idx += 1;
                }
                continue;
            }
            if part.starts_with('-') {
                return Err(format!("unsupported env wrapper option: {part}"));
            }
            break;
        }
    }

    while idx < parts.len() && is_env_assignment(&parts[idx]) {
        env_set.push(parse_env_assignment(&parts[idx])?);
        idx += 1;
    }

    let command_idx = command_start_index(parts);
    if command_idx < idx {
        return Err(format!("command not allowlisted: {}", parts.join(" ")));
    }
    let program = parts
        .get(command_idx)
        .ok_or_else(|| format!("command not allowlisted: {}", parts.join(" ")))?
        .clone();

    Ok(VerifierInvocation {
        command_kind,
        env_clear,
        env_remove,
        env_set,
        program,
        args: parts[command_idx + 1..].to_vec(),
    })
}

fn injected_verifier_env(invocation: &VerifierInvocation) -> Vec<(String, String)> {
    let runtime_mode = verifier_runtime_mode();
    let explicit_keys = invocation
        .env_set
        .iter()
        .map(|(key, _)| key.as_str())
        .collect::<BTreeSet<_>>();
    let removed_keys = invocation
        .env_remove
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let mut injected = Vec::new();

    let mut maybe_push = |key: &str, value: &str| {
        if !explicit_keys.contains(key) && !removed_keys.contains(key) {
            injected.push((key.to_string(), value.to_string()));
        }
    };

    if runtime_mode.ci {
        for (key, value) in CI_VERIFIER_ENV {
            maybe_push(key, value);
        }
    }
    if runtime_mode.noninteractive {
        for (key, value) in NONINTERACTIVE_VERIFIER_ENV {
            maybe_push(key, value);
        }
    }

    injected
}

fn verifier_isolation_mode(project_root: &Path, workspace_root: &Path) -> &'static str {
    match (normalize_path(project_root), normalize_path(workspace_root)) {
        (Ok(project), Ok(workspace)) if project == workspace => "project_root",
        _ => "repo_copy",
    }
}

fn verifier_policy_json(
    invocation: Option<&VerifierInvocation>,
    command_kind: &VerifierCommandKind,
    project_root: &Path,
    workspace_root: &Path,
    injected_env: &[String],
) -> Value {
    let runtime_mode = verifier_runtime_mode();
    let env_clear = invocation
        .map(|invocation| invocation.env_clear)
        .unwrap_or(false);
    let env_removed = invocation
        .map(|invocation| invocation.env_remove.clone())
        .unwrap_or_default();
    let env_overrides = invocation
        .map(|invocation| {
            invocation
                .env_set
                .iter()
                .map(|(key, value)| {
                    let redacted = if is_sensitive_key(key) {
                        "[REDACTED]".to_string()
                    } else {
                        redact_sensitive_text(value)
                    };
                    (key.clone(), redacted)
                })
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();
    let program = invocation
        .map(|invocation| redact_sensitive_text(&invocation.program))
        .unwrap_or_default();

    json!({
        "allowlist_enforced": true,
        "secret_redaction_enabled": true,
        "ci": runtime_mode.ci,
        "noninteractive": runtime_mode.noninteractive,
        "isolation_mode": verifier_isolation_mode(project_root, workspace_root),
        "workspace_isolated": verifier_isolation_mode(project_root, workspace_root) == "repo_copy",
        "command_kind": command_kind,
        "program": program,
        "env_clear": env_clear,
        "env_removed": env_removed,
        "env_overrides": env_overrides,
        "injected_env": injected_env,
    })
}

struct VerifierAuditEntry<'a> {
    event: &'a str,
    tool: &'a str,
    command: &'a str,
    command_kind: &'a VerifierCommandKind,
    project_root: &'a Path,
    workspace_root: &'a Path,
    decision: &'a str,
    exit_code: Option<i32>,
    rejection_reason: Option<&'a str>,
}

fn verifier_audit_json(entry: VerifierAuditEntry<'_>) -> Value {
    let runtime_mode = verifier_runtime_mode();
    json!({
        "event": entry.event,
        "tool": entry.tool,
        "decision": entry.decision,
        "command": redact_sensitive_text(entry.command),
        "command_kind": entry.command_kind,
        "exit_code": entry.exit_code,
        "rejection_reason": entry.rejection_reason,
        "ci": runtime_mode.ci,
        "noninteractive": runtime_mode.noninteractive,
        "isolation_mode": verifier_isolation_mode(entry.project_root, entry.workspace_root),
        "workspace_isolated": verifier_isolation_mode(entry.project_root, entry.workspace_root)
            == "repo_copy",
    })
}

fn verifier_rejection_reason(error: &str) -> &'static str {
    if error.contains("shell composition is not allowed") {
        "shell_composition"
    } else if error.contains("command not allowlisted") {
        "command_not_allowlisted"
    } else if error.contains("unsupported env wrapper option") {
        "unsupported_env_option"
    } else if error.contains("env wrapper option requires a value") {
        "env_option_missing_value"
    } else if error.contains("missing command") || error.contains("command is empty") {
        "missing_command"
    } else {
        "execution_error"
    }
}

fn build_tool_error_output(
    tool: &str,
    args: &Value,
    error: &str,
    project_root: &Path,
    workspace_root: &Path,
) -> Value {
    let redacted_error = redact_sensitive_text(error);
    if tool != RUN_TESTS_TOOL {
        return json!({"error": redacted_error});
    }

    let command = args
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    let command_parts = if command.is_empty() {
        Vec::new()
    } else {
        parse_command_parts(&command)
    };
    let command_kind = if command_parts.is_empty() {
        VerifierCommandKind::Unknown
    } else {
        classify_verifier_command(&command_parts)
    };
    let invocation = if command_parts.is_empty() {
        None
    } else {
        parse_allowlisted_verifier_invocation(&command_parts).ok()
    };
    let injected_env = invocation
        .as_ref()
        .map(injected_verifier_env)
        .unwrap_or_default()
        .into_iter()
        .map(|(key, _)| key)
        .collect::<Vec<_>>();

    json!({
        "error": redacted_error,
        "command": redact_sensitive_text(&command),
        "command_parts": command_parts
            .iter()
            .map(|part| redact_sensitive_text(part))
            .collect::<Vec<_>>(),
        "kind": command_kind,
        "policy": verifier_policy_json(
            invocation.as_ref(),
            &command_kind,
            project_root,
            workspace_root,
            &injected_env,
        ),
        "audit": verifier_audit_json(VerifierAuditEntry {
            event: "verifier_command_rejected",
            tool,
            command: &command,
            command_kind: &command_kind,
            project_root,
            workspace_root,
            decision: "rejected",
            exit_code: None,
            rejection_reason: Some(verifier_rejection_reason(error)),
        }),
    })
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RepoToolResult {
    pub tool_call_id: Option<String>,
    pub name: String,
    pub success: bool,
    pub output: Value,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct GroundedRepairContext {
    pub schema_version: String,
    pub summary: String,
    pub verifier_commands: Vec<String>,
    pub parsed_failure: Option<ParsedFailureReport>,
    pub tool_surface: Vec<RepoNativeTool>,
    pub rounds: Vec<GroundingRound>,
    pub total_usage: ApiUsage,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct GroundingRound {
    pub assistant_text: String,
    pub tool_calls: Vec<GroundingToolCall>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GroundingToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
    pub result: RepoToolResult,
}

impl GroundedRepairContext {
    pub fn planner_brief(&self) -> String {
        let mut parts = vec![format!(
            "Grounded repair context (schema: {}).",
            self.schema_version
        )];

        if !self.summary.trim().is_empty() {
            parts.push(format!("Grounded summary:\n{}", self.summary.trim()));
        }

        if !self.verifier_commands.is_empty() {
            parts.push(format!(
                "Verifier commands:\n{}",
                self.verifier_commands
                    .iter()
                    .map(|command| format!("- {command}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }

        if let Some(parsed_failure) = self.parsed_failure.as_ref() {
            if let Ok(serialized) = serde_json::to_string_pretty(parsed_failure) {
                parts.push(format!("Parsed failure report:\n{serialized}"));
            }
        }

        let observations = self
            .rounds
            .iter()
            .flat_map(|round| round.tool_calls.iter())
            .take(6)
            .map(|call| {
                format!(
                    "- {} => {}",
                    call.name,
                    summarize_value(&call.result.output, 240)
                )
            })
            .collect::<Vec<_>>();
        if !observations.is_empty() {
            parts.push(format!(
                "Grounded tool observations:\n{}",
                observations.join("\n")
            ));
        }

        parts.join("\n\n")
    }
}

#[derive(Debug, Error)]
pub enum VerificationError {
    #[error(transparent)]
    Api(#[from] ApiError),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("grounding failed: {0}")]
    Grounding(String),
}

pub fn mercury_repair_tools() -> Vec<ToolDefinition> {
    repo_native_tool_surface()
        .into_iter()
        .map(|tool| ToolDefinition {
            kind: "function".to_string(),
            function: ToolFunctionDefinition {
                name: tool.name.to_string(),
                description: Some(tool.description.to_string()),
                parameters: tool_parameters(tool.name),
            },
        })
        .collect()
}

pub async fn gather_grounded_repair_context<A: Mercury2Api>(
    api: &A,
    project_root: &Path,
    verify_config: &VerifyConfig,
    description: &str,
    parsed_failure: Option<&ParsedFailureReport>,
) -> Result<GroundedRepairContext, VerificationError> {
    let verifier_commands = verifier_commands(verify_config)
        .into_iter()
        .map(|command| redact_sensitive_text(&command))
        .collect::<Vec<_>>();
    let workspace_root = create_grounding_workspace(project_root)?;
    let _cleanup = CleanupPath(workspace_root.clone());
    copy_project_tree(project_root, &workspace_root, project_root)?;

    let executor = RepoToolExecutor::new(project_root, &workspace_root);
    let system_prompt = grounding_system_prompt();
    let tools = mercury_repair_tools();
    let parsed_failure_owned = parsed_failure.cloned().map(redact_parsed_failure_report);
    let mut rounds = Vec::new();
    let mut total_usage = ApiUsage::default();
    let mut total_tool_calls = 0usize;

    for _ in 0..MAX_GROUNDING_ROUNDS {
        let user_prompt = grounding_user_prompt(
            description,
            &verifier_commands,
            parsed_failure_owned.as_ref(),
            &rounds,
            total_tool_calls,
        );

        let (assistant_text, tool_calls, usage) = api
            .chat_with_tools(&system_prompt, &user_prompt, 1536, tools.clone(), None)
            .await?;
        total_usage.tokens_used += usage.tokens_used;
        total_usage.cost_usd += usage.cost_usd;

        if tool_calls.is_empty() {
            rounds.push(GroundingRound {
                assistant_text: redact_sensitive_text(&assistant_text),
                tool_calls: Vec::new(),
            });
            break;
        }

        let remaining_budget = MAX_GROUNDING_TOOL_CALLS.saturating_sub(total_tool_calls);
        if remaining_budget == 0 {
            break;
        }

        let executed_calls = tool_calls
            .into_iter()
            .take(remaining_budget)
            .map(|call| {
                let name = call.function.name.clone();
                let arguments = call
                    .function
                    .parse_arguments()
                    .unwrap_or_else(|_| json!({}));
                let result = executor.execute_named(
                    Some(call.id.clone()),
                    &call.function.name,
                    arguments.clone(),
                );
                GroundingToolCall {
                    id: call.id,
                    name,
                    arguments: redact_sensitive_value(arguments),
                    result,
                }
            })
            .collect::<Vec<_>>();
        total_tool_calls += executed_calls.len();
        rounds.push(GroundingRound {
            assistant_text: redact_sensitive_text(&assistant_text),
            tool_calls: executed_calls,
        });

        if total_tool_calls >= MAX_GROUNDING_TOOL_CALLS {
            break;
        }
    }

    let summary = build_grounding_summary(
        description,
        &verifier_commands,
        parsed_failure_owned.as_ref(),
        &rounds,
    );

    Ok(GroundedRepairContext {
        schema_version: GROUNDED_REPAIR_CONTEXT_SCHEMA_NAME.to_string(),
        summary,
        verifier_commands,
        parsed_failure: parsed_failure_owned,
        tool_surface: repo_native_tool_surface(),
        rounds,
        total_usage,
    })
}

#[derive(Debug, Clone)]
pub struct RepoToolExecutor {
    project_root: PathBuf,
    workspace_root: PathBuf,
}

impl RepoToolExecutor {
    pub fn new(project_root: impl Into<PathBuf>, workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            project_root: project_root.into(),
            workspace_root: workspace_root.into(),
        }
    }

    pub fn execute_tool_call(&self, call: &ToolCall) -> RepoToolResult {
        let args = match call.function.parse_arguments() {
            Ok(value) => value,
            Err(err) => {
                return RepoToolResult {
                    tool_call_id: Some(call.id.clone()),
                    name: call.function.name.clone(),
                    success: false,
                    output: redact_sensitive_value(json!({
                        "error": format!("invalid tool arguments: {err}")
                    })),
                }
            }
        };

        self.execute_named(Some(call.id.clone()), &call.function.name, args)
    }

    pub fn execute_named(
        &self,
        tool_call_id: Option<String>,
        name: &str,
        args: Value,
    ) -> RepoToolResult {
        let outcome = match name {
            READ_FILE_TOOL => self.read_file(&args),
            SEARCH_SYMBOL_TOOL => self.search_symbol(&args),
            RUN_TESTS_TOOL => self.run_tests(&args),
            APPLY_PATCH_TEMP_TOOL => self.apply_patch_temp(&args),
            GIT_DIFF_TOOL => self.git_diff(&args),
            ROLLBACK_CANDIDATE_TOOL => self.rollback_candidate(),
            _ => Err(format!("unknown tool: {name}")),
        };

        match outcome {
            Ok(output) => RepoToolResult {
                tool_call_id,
                name: name.to_string(),
                success: true,
                output: redact_sensitive_value(output),
            },
            Err(error) => RepoToolResult {
                tool_call_id,
                name: name.to_string(),
                success: false,
                output: redact_sensitive_value(build_tool_error_output(
                    name,
                    &args,
                    &error,
                    &self.project_root,
                    &self.workspace_root,
                )),
            },
        }
    }

    fn read_file(&self, args: &Value) -> Result<Value, String> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| "missing path".to_string())?;
        let resolved = self.resolve_workspace_path(path)?;
        let content = fs::read_to_string(&resolved).map_err(|err| err.to_string())?;
        Ok(json!({
            "path": normalized_relative(&self.workspace_root, &resolved),
            "content": content,
        }))
    }

    fn search_symbol(&self, args: &Value) -> Result<Value, String> {
        let query = args
            .get("query")
            .and_then(Value::as_str)
            .ok_or_else(|| "missing query".to_string())?;

        let output = Command::new("rg")
            .args([
                "-n",
                "--fixed-strings",
                query,
                self.workspace_root.to_string_lossy().as_ref(),
            ])
            .output()
            .map_err(|err| err.to_string())?;

        if !output.status.success() && output.status.code() != Some(1) {
            return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
        }

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let matches = stdout
            .lines()
            .take(50)
            .filter_map(|line| parse_ripgrep_match(line, &self.workspace_root))
            .collect::<Vec<_>>();

        Ok(json!({"query": query, "matches": matches}))
    }

    fn run_tests(&self, args: &Value) -> Result<Value, String> {
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| "missing command".to_string())?
            .trim();

        if command.is_empty() {
            return Err("missing command".to_string());
        }

        let parts = parse_allowlisted_verifier_parts(command)?;
        let invocation = parse_allowlisted_verifier_invocation(&parts)?;
        let command_kind = invocation.command_kind.clone();
        let injected_env = injected_verifier_env(&invocation)
            .into_iter()
            .map(|(key, _)| key)
            .collect::<Vec<_>>();

        let mut process = build_allowlisted_verifier_command(&parts, &self.workspace_root)?;
        let output = process.output().map_err(|err| err.to_string())?;
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let parsed_failure = if output.status.success() {
            None
        } else {
            Some(parse_verifier_failure(
                &command_kind,
                &parts,
                &stdout,
                &stderr,
            ))
        };
        let decision = if output.status.success() {
            "passed"
        } else {
            "failed"
        };

        Ok(json!({
            "command": redact_sensitive_text(command),
            "command_parts": parts
                .iter()
                .map(|part| redact_sensitive_text(part))
                .collect::<Vec<_>>(),
            "kind": command_kind,
            "success": output.status.success(),
            "exit_code": output.status.code(),
            "stdout": redact_sensitive_text(&stdout),
            "stderr": redact_sensitive_text(&stderr),
            "stdout_bytes": output.stdout.len(),
            "stderr_bytes": output.stderr.len(),
            "parsed_failure": redact_sensitive_value(json!(parsed_failure)),
            "policy": verifier_policy_json(
                Some(&invocation),
                &invocation.command_kind,
                &self.project_root,
                &self.workspace_root,
                &injected_env,
            ),
            "audit": verifier_audit_json(VerifierAuditEntry {
                event: "verifier_command_completed",
                tool: RUN_TESTS_TOOL,
                command,
                command_kind: &invocation.command_kind,
                project_root: &self.project_root,
                workspace_root: &self.workspace_root,
                decision,
                exit_code: output.status.code(),
                rejection_reason: None,
            }),
        }))
    }

    fn apply_patch_temp(&self, args: &Value) -> Result<Value, String> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| "missing path".to_string())?;
        let content = args
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| "missing content".to_string())?;
        let resolved = self.resolve_workspace_path(path)?;
        if let Some(parent) = resolved.parent() {
            fs::create_dir_all(parent).map_err(|err| err.to_string())?;
        }
        fs::write(&resolved, content).map_err(|err| err.to_string())?;
        Ok(json!({
            "path": normalized_relative(&self.workspace_root, &resolved),
            "bytes_written": content.len(),
        }))
    }

    fn git_diff(&self, args: &Value) -> Result<Value, String> {
        let path = args.get("path").and_then(Value::as_str);
        let (left, right) = match path {
            Some(relative) if !relative.is_empty() => (
                self.resolve_project_path(relative)?,
                self.resolve_workspace_path(relative)?,
            ),
            _ => (self.project_root.clone(), self.workspace_root.clone()),
        };

        let output = Command::new("git")
            .args([
                "--no-pager",
                "diff",
                "--no-index",
                "--no-color",
                left.to_string_lossy().as_ref(),
                right.to_string_lossy().as_ref(),
            ])
            .output()
            .map_err(|err| err.to_string())?;

        let status = output.status.code().unwrap_or_default();
        if status != 0 && status != 1 {
            return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
        }

        Ok(json!({
            "diff": String::from_utf8_lossy(&output.stdout).to_string(),
            "path": path,
        }))
    }

    fn rollback_candidate(&self) -> Result<Value, String> {
        if self.workspace_root.exists() {
            fs::remove_dir_all(&self.workspace_root).map_err(|err| err.to_string())?;
        }
        fs::create_dir_all(&self.workspace_root).map_err(|err| err.to_string())?;
        copy_project_tree(&self.project_root, &self.workspace_root, &self.project_root)
            .map_err(|err| err.to_string())?;
        Ok(json!({"workspace_root": self.workspace_root.display().to_string()}))
    }

    fn resolve_workspace_path(&self, relative: &str) -> Result<PathBuf, String> {
        resolve_relative_path(&self.workspace_root, relative)
    }

    fn resolve_project_path(&self, relative: &str) -> Result<PathBuf, String> {
        resolve_relative_path(&self.project_root, relative)
    }
}

struct CleanupPath(PathBuf);

impl Drop for CleanupPath {
    fn drop(&mut self) {
        if self.0.exists() {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}

fn tool_parameters(name: &str) -> Value {
    match name {
        READ_FILE_TOOL => json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "path": {"type": "string", "minLength": 1}
            },
            "required": ["path"]
        }),
        SEARCH_SYMBOL_TOOL => json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "query": {"type": "string", "minLength": 1}
            },
            "required": ["query"]
        }),
        RUN_TESTS_TOOL => json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "command": {"type": "string", "minLength": 1}
            },
            "required": ["command"]
        }),
        APPLY_PATCH_TEMP_TOOL => json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "path": {"type": "string", "minLength": 1},
                "content": {"type": "string"}
            },
            "required": ["path", "content"]
        }),
        GIT_DIFF_TOOL => json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "path": {"type": "string"}
            }
        }),
        ROLLBACK_CANDIDATE_TOOL => json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {}
        }),
        _ => json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {}
        }),
    }
}

fn grounding_system_prompt() -> String {
    format!(
        concat!(
            "You are grounding a code repair plan before edits begin. ",
            "Inspect only what is needed to localize the failure and return concise findings. ",
            "Use the repo-native tools instead of asking for more pasted context. ",
            "You may make at most ",
            "{}",
            " tool calls total across the session."
        ),
        MAX_GROUNDING_TOOL_CALLS
    )
}

fn grounding_user_prompt(
    description: &str,
    verifier_commands: &[String],
    parsed_failure: Option<&ParsedFailureReport>,
    rounds: &[GroundingRound],
    total_tool_calls: usize,
) -> String {
    let mut prompt = format!(
        "Repair objective:\n{}\n\nVerifier commands:\n{}\n\nTool call budget already used: {} of {}.\n",
        description.trim(),
        verifier_commands
            .iter()
            .map(|command| format!("- {command}"))
            .collect::<Vec<_>>()
            .join("\n"),
        total_tool_calls,
        MAX_GROUNDING_TOOL_CALLS,
    );

    if let Some(parsed_failure) = parsed_failure {
        let serialized =
            serde_json::to_string_pretty(parsed_failure).unwrap_or_else(|_| "{}".to_string());
        prompt.push_str("\nParsed failure report:\n");
        prompt.push_str(&serialized);
        prompt.push('\n');
    }

    if !rounds.is_empty() {
        prompt.push_str("\nPreviously observed grounding evidence:\n");
        for (index, round) in rounds.iter().enumerate() {
            if !round.assistant_text.trim().is_empty() {
                prompt.push_str(&format!(
                    "Round {} assistant:\n{}\n",
                    index + 1,
                    truncate_text(round.assistant_text.trim(), 600)
                ));
            }
            for call in &round.tool_calls {
                prompt.push_str(&format!(
                    "- Tool {} {} => {}\n",
                    call.name,
                    summarize_value(&call.arguments, 120),
                    summarize_value(&call.result.output, 240)
                ));
            }
        }
    }

    prompt.push_str(
        "\nInspect the minimal code needed, then either call tools or reply with a concise grounded summary that names the likely failing region and next verification focus.",
    );
    prompt
}

fn build_grounding_summary(
    description: &str,
    verifier_commands: &[String],
    parsed_failure: Option<&ParsedFailureReport>,
    rounds: &[GroundingRound],
) -> String {
    let mut lines = vec![format!("Objective: {}", description.trim())];
    if !verifier_commands.is_empty() {
        lines.push(format!("Verifiers: {}", verifier_commands.join(" | ")));
    }
    if let Some(parsed_failure) = parsed_failure {
        lines.push(format!(
            "Failure class summary: {}",
            parsed_failure
                .failures
                .iter()
                .take(3)
                .map(|failure| {
                    let target = failure
                        .target
                        .file_path
                        .as_deref()
                        .or(failure.target.symbol.as_deref())
                        .unwrap_or("unknown target");
                    format!("{} @ {}", failure.error_class, target)
                })
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    for round in rounds {
        if !round.assistant_text.trim().is_empty() {
            lines.push(format!(
                "Assistant grounding: {}",
                truncate_text(round.assistant_text.trim(), 480)
            ));
        }
        for call in round.tool_calls.iter().take(3) {
            lines.push(format!(
                "Tool {}: {}",
                call.name,
                summarize_value(&call.result.output, 280)
            ));
        }
    }

    truncate_text(&lines.join("\n"), MAX_SUMMARY_CHARS)
}

fn verifier_commands(config: &VerifyConfig) -> Vec<String> {
    let mut commands = Vec::new();
    if config.test_after_write && !config.test_command.trim().is_empty() {
        commands.push(config.test_command.trim().to_string());
    }
    if config.lint_after_write && !config.lint_command.trim().is_empty() {
        commands.push(config.lint_command.trim().to_string());
    }
    commands
}

fn create_grounding_workspace(project_root: &Path) -> Result<PathBuf, VerificationError> {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|err| VerificationError::Grounding(err.to_string()))?
        .as_millis();
    let root = project_root
        .join(".mercury")
        .join("worktrees")
        .join(format!("grounding-{suffix}-{}", std::process::id()));
    fs::create_dir_all(&root)?;
    Ok(root)
}

pub fn parse_allowlisted_verifier_parts(command: &str) -> Result<Vec<String>, String> {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return Err("command is empty".to_string());
    }
    if contains_shell_composition(trimmed) {
        return Err(format!("shell composition is not allowed: {trimmed}"));
    }

    let parts = parse_command_parts(trimmed);
    if parts.is_empty() {
        return Err("command is empty".to_string());
    }
    if matches!(
        classify_verifier_command(&parts),
        VerifierCommandKind::Unknown
    ) {
        return Err(format!("command not allowlisted: {trimmed}"));
    }
    Ok(parts)
}

pub fn verifier_command_allowlisted(command: &str) -> bool {
    parse_allowlisted_verifier_parts(command).is_ok()
}

pub fn build_allowlisted_verifier_command(
    parts: &[String],
    workspace_root: &Path,
) -> Result<Command, String> {
    let invocation = parse_allowlisted_verifier_invocation(parts)?;
    let mut command = Command::new(&invocation.program);
    command.current_dir(workspace_root);
    if invocation.env_clear {
        command.env_clear();
    }
    for key in &invocation.env_remove {
        command.env_remove(key);
    }
    for (key, value) in &invocation.env_set {
        command.env(key, value);
    }
    for (key, value) in injected_verifier_env(&invocation) {
        command.env(key, value);
    }
    command.args(&invocation.args);
    Ok(command)
}

fn parse_env_assignment(part: &str) -> Result<(String, String), String> {
    let (name, value) = part
        .split_once('=')
        .ok_or_else(|| format!("invalid env assignment: {part}"))?;
    Ok((name.to_string(), value.to_string()))
}

fn resolve_relative_path(root: &Path, relative: &str) -> Result<PathBuf, String> {
    let candidate = root.join(relative);
    let normalized = normalize_path(&candidate)?;
    let normalized_root = normalize_path(root)?;
    if !normalized.starts_with(&normalized_root) {
        return Err(format!("path escapes workspace: {relative}"));
    }
    Ok(normalized)
}

fn normalize_path(path: &Path) -> Result<PathBuf, String> {
    let mut normalized = if path.is_absolute() {
        PathBuf::new()
    } else {
        std::env::current_dir().map_err(|err| err.to_string())?
    };

    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !normalized.pop() {
                    return Err(format!("failed to normalize path: {}", path.display()));
                }
            }
            other => normalized.push(other.as_os_str()),
        }
    }

    Ok(normalized)
}

fn normalized_relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string()
}

fn parse_ripgrep_match(line: &str, workspace_root: &Path) -> Option<Value> {
    let mut parts = line.splitn(4, ':');
    let file = parts.next()?;
    let line_no = parts.next()?.parse::<usize>().ok()?;
    let column_or_text = parts.next()?;
    let (column, text): (Option<usize>, String) = match column_or_text.parse::<usize>() {
        Ok(column) => (Some(column), parts.next().unwrap_or_default().to_string()),
        Err(_) => (
            None,
            [column_or_text, parts.next().unwrap_or_default()].join(":"),
        ),
    };

    let file_path = PathBuf::from(file);
    Some(json!({
        "path": normalized_relative(workspace_root, &file_path),
        "line": line_no,
        "column": column,
        "text": text,
    }))
}

fn copy_project_tree(source: &Path, destination: &Path, root: &Path) -> std::io::Result<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let relative = source_path
            .strip_prefix(root)
            .unwrap_or(source_path.as_path());

        if should_skip_copy(relative) {
            continue;
        }

        let destination_path = destination.join(relative);
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            fs::create_dir_all(&destination_path)?;
            copy_project_tree(&source_path, destination, root)?;
        } else if metadata.is_file() {
            if let Some(parent) = destination_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&source_path, &destination_path)?;
        }
    }
    Ok(())
}

fn should_skip_copy(relative: &Path) -> bool {
    let relative = relative.to_string_lossy();
    relative == ".git"
        || relative.starts_with(".git/")
        || relative == "target"
        || relative.starts_with("target/")
        || relative == ".mercury/worktrees"
        || relative.starts_with(".mercury/worktrees/")
        || relative == ".mercury/runs"
        || relative.starts_with(".mercury/runs/")
}

fn truncate_text(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }

    let mut truncated = text.chars().take(max_chars).collect::<String>();
    truncated.push_str("...");
    truncated
}

fn summarize_value(value: &Value, max_chars: usize) -> String {
    let rendered = match value {
        Value::String(text) => text.clone(),
        _ => serde_json::to_string(value).unwrap_or_else(|_| value.to_string()),
    };
    truncate_text(&rendered.replace('\n', " "), max_chars)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, LazyLock, Mutex};

    use tempfile::tempdir;

    use crate::api::{ApiUsage, Mercury2Api, ToolCall, ToolCallFunction};

    type ToolResponses = Arc<Mutex<Vec<(String, Vec<ToolCall>)>>>;

    #[derive(Clone)]
    struct FakeMercuryApi {
        responses: ToolResponses,
    }

    static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

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
        ) -> Result<(crate::api::ThermalAssessment, ApiUsage), ApiError> {
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
            _tool_choice: Option<crate::api::ToolChoice>,
        ) -> Result<(String, Vec<ToolCall>, ApiUsage), ApiError> {
            let mut responses = self.responses.lock().unwrap();
            let (text, tool_calls) = responses.pop().unwrap_or_default();
            Ok((
                text,
                tool_calls,
                ApiUsage {
                    tokens_used: 12,
                    cost_usd: 0.0012,
                },
            ))
        }
    }

    #[test]
    fn tool_definitions_cover_repo_native_surface() {
        let tools = mercury_repair_tools();
        let names = tools
            .iter()
            .map(|tool| tool.function.name.as_str())
            .collect::<Vec<_>>();
        assert!(names.contains(&READ_FILE_TOOL));
        assert!(names.contains(&SEARCH_SYMBOL_TOOL));
        assert!(names.contains(&RUN_TESTS_TOOL));
        assert!(names.contains(&APPLY_PATCH_TEMP_TOOL));
        assert!(names.contains(&GIT_DIFF_TOOL));
        assert!(names.contains(&ROLLBACK_CANDIDATE_TOOL));
    }

    #[test]
    fn repo_tool_executor_reads_writes_diffs_and_rolls_back() {
        let temp = tempdir().unwrap();
        let project_root = temp.path().join("project");
        let workspace_root = temp.path().join("workspace");
        fs::create_dir_all(project_root.join("src")).unwrap();
        fs::write(
            project_root.join("src/lib.rs"),
            "pub fn stable() -> i32 { 1 }\n",
        )
        .unwrap();
        fs::create_dir_all(&workspace_root).unwrap();
        copy_project_tree(&project_root, &workspace_root, &project_root).unwrap();

        let executor = RepoToolExecutor::new(&project_root, &workspace_root);
        let write = executor.execute_named(
            None,
            APPLY_PATCH_TEMP_TOOL,
            json!({"path": "src/lib.rs", "content": "pub fn stable() -> i32 { 2 }\n"}),
        );
        assert!(write.success);

        let read = executor.execute_named(None, READ_FILE_TOOL, json!({"path": "src/lib.rs"}));
        assert!(read.success);
        assert!(read.output["content"]
            .as_str()
            .unwrap()
            .contains("i32 { 2 }"));

        let diff = executor.execute_named(None, GIT_DIFF_TOOL, json!({"path": "src/lib.rs"}));
        assert!(diff.success);
        assert!(diff.output["diff"]
            .as_str()
            .unwrap()
            .contains("-pub fn stable() -> i32 { 1 }"));
        assert!(diff.output["diff"]
            .as_str()
            .unwrap()
            .contains("+pub fn stable() -> i32 { 2 }"));

        let rollback = executor.execute_named(None, ROLLBACK_CANDIDATE_TOOL, json!({}));
        assert!(rollback.success);
        let reset = fs::read_to_string(workspace_root.join("src/lib.rs")).unwrap();
        assert!(reset.contains("i32 { 1 }"));
    }

    #[test]
    fn repo_tool_executor_rejects_shell_composition() {
        let temp = tempdir().unwrap();
        let executor = RepoToolExecutor::new(temp.path(), temp.path().join("workspace"));
        let result = executor.execute_named(
            None,
            RUN_TESTS_TOOL,
            json!({"command": "cargo test && cargo clippy"}),
        );
        assert!(!result.success);
        assert!(result.output["error"]
            .as_str()
            .unwrap()
            .contains("shell composition"));
    }

    #[test]
    fn repo_tool_executor_accepts_env_prefixed_cargo_command() {
        let temp = tempdir().unwrap();
        let project_root = temp.path().join("project");
        let workspace_root = temp.path().join("workspace");
        fs::create_dir_all(project_root.join("src")).unwrap();
        fs::write(
            project_root.join("Cargo.toml"),
            r#"[package]
name = "tool-runner"
version = "0.1.0"
edition = "2021"

[workspace]
members = []
"#,
        )
        .unwrap();
        fs::write(
            project_root.join("src/main.rs"),
            "fn main() { println!(\"ok\"); }\n",
        )
        .unwrap();
        fs::create_dir_all(&workspace_root).unwrap();
        copy_project_tree(&project_root, &workspace_root, &project_root).unwrap();

        let executor = RepoToolExecutor::new(&project_root, &workspace_root);
        let result = executor.execute_named(
            None,
            RUN_TESTS_TOOL,
            json!({"command": "env RUST_BACKTRACE=1 cargo test --quiet"}),
        );
        assert!(result.success);
        assert_eq!(result.output["success"], Value::Bool(true));
    }

    #[test]
    fn repo_tool_executor_redacts_sensitive_test_output() {
        let _guard = ENV_LOCK.lock().unwrap();
        let temp = tempdir().unwrap();
        let project_root = temp.path().join("project");
        let workspace_root = temp.path().join("workspace");
        fs::create_dir_all(project_root.join("src")).unwrap();
        fs::create_dir_all(project_root.join("tests")).unwrap();
        fs::write(
            project_root.join("Cargo.toml"),
            r#"[package]
name = "tool-runner-redaction"
version = "0.1.0"
edition = "2021"

[workspace]
members = []
"#,
        )
        .unwrap();
        fs::write(
            project_root.join("src/main.rs"),
            "fn main() { println!(\"ok\"); }\n",
        )
        .unwrap();
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
        .unwrap();
        fs::create_dir_all(&workspace_root).unwrap();
        copy_project_tree(&project_root, &workspace_root, &project_root).unwrap();

        let prior = env::var("INCEPTION_API_KEY").ok();
        // SAFETY: the test holds ENV_LOCK for the full mutation window, serializing
        // process-global environment changes while the child process is spawned.
        unsafe { env::set_var("INCEPTION_API_KEY", "supersecret-observability-token") };

        let executor = RepoToolExecutor::new(&project_root, &workspace_root);
        let result = executor.execute_named(
            None,
            RUN_TESTS_TOOL,
            json!({"command": "env INCEPTION_API_KEY=supersecret-observability-token cargo test --quiet"}),
        );

        match prior {
            Some(value) => {
                // SAFETY: guarded by ENV_LOCK above.
                unsafe { env::set_var("INCEPTION_API_KEY", value) }
            }
            None => {
                // SAFETY: guarded by ENV_LOCK above.
                unsafe { env::remove_var("INCEPTION_API_KEY") }
            }
        }

        assert!(result.success);
        assert_eq!(result.output["success"], Value::Bool(false));
        let output_json = serde_json::to_string(&result.output).unwrap();
        assert!(!output_json.contains("supersecret-observability-token"));
        assert!(output_json.contains("INCEPTION_API_KEY=[REDACTED]"));
        assert!(output_json.contains("panic secret=[REDACTED]"));
    }

    #[test]
    fn build_allowlisted_verifier_command_accepts_typescript_commands() {
        let temp = tempdir().unwrap();
        let workspace = temp.path();

        let npm_test = parse_command_parts("env NODE_ENV=test npm run test -- --runInBand");
        assert!(build_allowlisted_verifier_command(&npm_test, workspace).is_ok());

        let npx_lint = parse_command_parts("env -i npx --yes eslint src");
        assert!(build_allowlisted_verifier_command(&npx_lint, workspace).is_ok());

        let pnpm_check = parse_command_parts("pnpm exec tsc --noEmit");
        assert!(build_allowlisted_verifier_command(&pnpm_check, workspace).is_ok());
    }

    #[test]
    fn build_allowlisted_verifier_command_rejects_unsupported_inputs() {
        let temp = tempdir().unwrap();
        let workspace = temp.path();

        let unsupported_env = parse_command_parts("env -x npm test");
        assert!(build_allowlisted_verifier_command(&unsupported_env, workspace).is_err());

        let unsupported_script = parse_command_parts("npm run build");
        assert!(build_allowlisted_verifier_command(&unsupported_script, workspace).is_err());
    }

    #[tokio::test]
    async fn gather_grounded_context_executes_requested_tool_calls() {
        let temp = tempdir().unwrap();
        let project_root = temp.path().join("project");
        fs::create_dir_all(project_root.join("src")).unwrap();
        fs::write(
            project_root.join("src/lib.rs"),
            "pub fn stable() -> i32 { 1 }\n",
        )
        .unwrap();

        let api = FakeMercuryApi::new(vec![
            (
                "Focus on src/lib.rs".to_string(),
                vec![ToolCall {
                    id: "call-1".to_string(),
                    kind: "function".to_string(),
                    function: ToolCallFunction {
                        name: READ_FILE_TOOL.to_string(),
                        arguments: json!({"path": "src/lib.rs"}).to_string(),
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
                test_command: "cargo test --quiet".to_string(),
                lint_command: String::new(),
            },
            "Repair failing tests",
            None,
        )
        .await
        .unwrap();

        assert_eq!(context.schema_version, GROUNDED_REPAIR_CONTEXT_SCHEMA_NAME);
        assert_eq!(context.rounds.len(), 2);
        assert_eq!(context.rounds[0].tool_calls.len(), 1);
        assert_eq!(context.rounds[0].tool_calls[0].name, READ_FILE_TOOL);
        assert!(context.rounds[0].tool_calls[0].result.success);
        assert!(context.summary.contains("Repair failing tests"));
    }
}
