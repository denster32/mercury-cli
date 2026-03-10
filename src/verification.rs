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
    classify_cargo_command, contains_shell_composition, parse_cargo_failure, parse_command_parts,
    repo_native_tool_surface, CargoCommandKind, ParsedFailureReport, RepoNativeTool,
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
    let verifier_commands = verifier_commands(verify_config);
    let workspace_root = create_grounding_workspace(project_root)?;
    let _cleanup = CleanupPath(workspace_root.clone());
    copy_project_tree(project_root, &workspace_root, project_root)?;

    let executor = RepoToolExecutor::new(project_root, &workspace_root);
    let system_prompt = grounding_system_prompt();
    let tools = mercury_repair_tools();
    let parsed_failure_owned = parsed_failure.cloned();
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
                assistant_text,
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
                let result = executor.execute_tool_call(&call);
                GroundingToolCall {
                    id: call.id,
                    name,
                    arguments,
                    result,
                }
            })
            .collect::<Vec<_>>();
        total_tool_calls += executed_calls.len();
        rounds.push(GroundingRound {
            assistant_text,
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
                    output: json!({"error": format!("invalid tool arguments: {err}")}),
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
                output,
            },
            Err(error) => RepoToolResult {
                tool_call_id,
                name: name.to_string(),
                success: false,
                output: json!({"error": error}),
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
        if contains_shell_composition(command) {
            return Err(format!("shell composition is not allowed: {command}"));
        }

        let parts = parse_command_parts(command);
        let command_kind = classify_cargo_command(&parts);
        if matches!(command_kind, CargoCommandKind::Unknown) {
            return Err(format!("command not allowlisted: {command}"));
        }

        let mut process = build_allowlisted_cargo_command(&parts, &self.workspace_root)?;
        let output = process.output().map_err(|err| err.to_string())?;
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let parsed_failure = if output.status.success() {
            None
        } else {
            Some(parse_cargo_failure(&parts, &stdout, &stderr))
        };

        Ok(json!({
            "command": command,
            "kind": command_kind,
            "success": output.status.success(),
            "exit_code": output.status.code(),
            "stdout": stdout,
            "stderr": stderr,
            "parsed_failure": parsed_failure,
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

fn build_allowlisted_cargo_command(
    parts: &[String],
    workspace_root: &Path,
) -> Result<Command, String> {
    if parts.is_empty() {
        return Err("command is empty".to_string());
    }

    let command_kind = classify_cargo_command(parts);
    if matches!(command_kind, CargoCommandKind::Unknown) {
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
            if part == "-u" {
                idx += 1;
                let key = parts
                    .get(idx)
                    .ok_or_else(|| "env -u requires a variable name".to_string())?;
                env_remove.push(key.clone());
                idx += 1;
                continue;
            }
            if is_env_assignment(part) {
                env_set.push(parse_env_assignment(part)?);
                idx += 1;
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

    if parts.get(idx).map(String::as_str) != Some("cargo") {
        return Err(format!("command not allowlisted: {}", parts.join(" ")));
    }
    idx += 1;

    let mut command = Command::new("cargo");
    command.current_dir(workspace_root);
    if env_clear {
        command.env_clear();
    }
    for key in env_remove {
        command.env_remove(key);
    }
    for (key, value) in env_set {
        command.env(key, value);
    }
    command.args(&parts[idx..]);
    Ok(command)
}

fn parse_env_assignment(part: &str) -> Result<(String, String), String> {
    let (name, value) = part
        .split_once('=')
        .ok_or_else(|| format!("invalid env assignment: {part}"))?;
    Ok((name.to_string(), value.to_string()))
}

fn is_env_assignment(part: &str) -> bool {
    let Some((name, _)) = part.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && name
            .chars()
            .all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
        && name
            .chars()
            .next()
            .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
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
    use std::sync::{Arc, Mutex};

    use tempfile::tempdir;

    use crate::api::{ApiUsage, Mercury2Api, ToolCall, ToolCallFunction};

    #[derive(Clone)]
    struct FakeMercuryApi {
        responses: Arc<Mutex<Vec<(String, Vec<ToolCall>)>>>,
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
