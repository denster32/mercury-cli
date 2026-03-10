use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CargoCommandKind {
    Test,
    Clippy,
    Check,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum VerifierCommandKind {
    CargoTest,
    CargoClippy,
    CargoCheck,
    TypeScriptCheck,
    TypeScriptTest,
    TypeScriptLint,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FailureStage {
    Parse,
    Compile,
    Test,
    Lint,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct FailureTarget {
    pub file_path: Option<String>,
    pub line: Option<u32>,
    pub column: Option<u32>,
    pub symbol: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedFailure {
    pub error_class: String,
    pub message: String,
    pub target: FailureTarget,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedFailureReport {
    pub command: CargoCommandKind,
    pub stage: FailureStage,
    pub failures: Vec<ParsedFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoNativeTool {
    pub name: &'static str,
    pub description: &'static str,
}

pub fn parse_cargo_failure(
    verifier_command: &[String],
    stdout: &str,
    stderr: &str,
) -> ParsedFailureReport {
    let command = classify_cargo_command(verifier_command);
    match command {
        CargoCommandKind::Test => parse_test_failure(stdout, stderr),
        CargoCommandKind::Clippy => parse_compile_or_lint_failure(stderr, CargoCommandKind::Clippy),
        CargoCommandKind::Check => parse_compile_or_lint_failure(stderr, CargoCommandKind::Check),
        CargoCommandKind::Unknown => parse_unknown_failure(stdout, stderr),
    }
}

pub fn classify_verifier_command(verifier_command: &[String]) -> VerifierCommandKind {
    match classify_cargo_command(verifier_command) {
        CargoCommandKind::Test => return VerifierCommandKind::CargoTest,
        CargoCommandKind::Clippy => return VerifierCommandKind::CargoClippy,
        CargoCommandKind::Check => return VerifierCommandKind::CargoCheck,
        CargoCommandKind::Unknown => {}
    }

    let idx = command_start_index(verifier_command);
    let Some(program) = verifier_command.get(idx).map(String::as_str) else {
        return VerifierCommandKind::Unknown;
    };

    match program {
        "tsc" => VerifierCommandKind::TypeScriptCheck,
        "npx" => classify_npx_command(verifier_command, idx),
        "npm" | "pnpm" | "yarn" => classify_package_manager_command(verifier_command, idx),
        _ => VerifierCommandKind::Unknown,
    }
}

pub fn parse_verifier_failure(
    verifier_kind: &VerifierCommandKind,
    verifier_command: &[String],
    stdout: &str,
    stderr: &str,
) -> ParsedFailureReport {
    match verifier_kind {
        VerifierCommandKind::CargoTest
        | VerifierCommandKind::CargoClippy
        | VerifierCommandKind::CargoCheck => parse_cargo_failure(verifier_command, stdout, stderr),
        VerifierCommandKind::TypeScriptCheck => parse_typescript_compile_failure(stdout, stderr),
        VerifierCommandKind::TypeScriptTest => parse_typescript_test_failure(stdout, stderr),
        VerifierCommandKind::TypeScriptLint => parse_typescript_lint_failure(stdout, stderr),
        VerifierCommandKind::Unknown => parse_unknown_failure(stdout, stderr),
    }
}

pub fn repo_native_tool_surface() -> Vec<RepoNativeTool> {
    vec![
        RepoNativeTool {
            name: "read_file",
            description: "Read a repository file for targeted diagnosis.",
        },
        RepoNativeTool {
            name: "search_symbol",
            description: "Search for symbol definitions/usages in repository code.",
        },
        RepoNativeTool {
            name: "run_tests",
            description: "Execute verifier commands and collect structured output.",
        },
        RepoNativeTool {
            name: "apply_patch_temp",
            description: "Apply candidate patch in isolated sandbox/worktree.",
        },
        RepoNativeTool {
            name: "git_diff",
            description: "Emit working diff for candidate review and artifacts.",
        },
        RepoNativeTool {
            name: "rollback_candidate",
            description: "Discard candidate changes and restore clean candidate state.",
        },
    ]
}

pub fn contains_shell_composition(command: &str) -> bool {
    let mut chars = command.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;

    while let Some(ch) = chars.next() {
        if escaped {
            escaped = false;
            continue;
        }

        match ch {
            '\\' if !in_single => escaped = true,
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '&' if !in_single && !in_double => {
                if chars.peek() == Some(&'&') {
                    return true;
                }
            }
            '|' if !in_single && !in_double => return true,
            ';' | '<' | '>' if !in_single && !in_double => return true,
            _ => {}
        }
    }

    false
}

pub fn parse_command_parts(command: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut chars = command.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;

    while let Some(ch) = chars.next() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }

        match ch {
            '\\' if !in_single => escaped = true,
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '&' if !in_single && !in_double => {
                if chars.peek() == Some(&'&') {
                    if !current.is_empty() {
                        parts.push(std::mem::take(&mut current));
                    }
                    break;
                }
                current.push(ch);
            }
            '|' if !in_single && !in_double => {
                if !current.is_empty() {
                    parts.push(std::mem::take(&mut current));
                }
                break;
            }
            ';' | '<' | '>' if !in_single && !in_double => {
                if !current.is_empty() {
                    parts.push(std::mem::take(&mut current));
                }
                break;
            }
            ch if ch.is_whitespace() && !in_single && !in_double => {
                if !current.is_empty() {
                    parts.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }

    if !current.is_empty() {
        parts.push(current);
    }

    parts
}

pub fn classify_cargo_command(verifier_command: &[String]) -> CargoCommandKind {
    let mut idx = command_start_index(verifier_command);

    if verifier_command.get(idx).map(String::as_str) != Some("cargo") {
        return CargoCommandKind::Unknown;
    }
    idx += 1;

    if verifier_command
        .get(idx)
        .is_some_and(|part| part.starts_with('+'))
    {
        idx += 1;
    }

    while let Some(part) = verifier_command.get(idx) {
        match part.as_str() {
            "test" => return CargoCommandKind::Test,
            "clippy" => return CargoCommandKind::Clippy,
            "check" => return CargoCommandKind::Check,
            part if part.starts_with('-') => idx += 1,
            _ => return CargoCommandKind::Unknown,
        }
    }

    CargoCommandKind::Unknown
}

pub fn command_start_index(verifier_command: &[String]) -> usize {
    let mut idx = 0usize;

    while idx < verifier_command.len() && is_env_assignment(&verifier_command[idx]) {
        idx += 1;
    }

    if verifier_command.get(idx).map(String::as_str) == Some("env") {
        idx += 1;
        while let Some(part) = verifier_command.get(idx).map(String::as_str) {
            if part == "--" {
                idx += 1;
                break;
            }
            if is_env_assignment(part) {
                idx += 1;
                continue;
            }
            if let Some(consumes_next) = env_option_arity(part) {
                idx += 1;
                if consumes_next && !part.contains('=') && idx < verifier_command.len() {
                    idx += 1;
                }
                continue;
            }
            break;
        }
    }

    idx
}

pub fn is_env_assignment(part: &str) -> bool {
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

pub fn env_option_arity(part: &str) -> Option<bool> {
    match part {
        "-i" | "--ignore-environment" => Some(false),
        "-u" | "--unset" | "-C" | "--chdir" | "-S" | "--split-string" => Some(true),
        option
            if option.starts_with("--unset=")
                || option.starts_with("--chdir=")
                || option.starts_with("--split-string=")
                || option.starts_with("--default-signal=")
                || option.starts_with("--ignore-signal=")
                || option.starts_with("--block-signal=") =>
        {
            Some(false)
        }
        option
            if option == "--default-signal"
                || option == "--ignore-signal"
                || option == "--block-signal"
                || option == "--list-signal-handling" =>
        {
            Some(option != "--list-signal-handling")
        }
        _ => None,
    }
}

fn classify_npx_command(parts: &[String], idx: usize) -> VerifierCommandKind {
    let Some(next_idx) = first_positional_after_options(parts, idx + 1) else {
        return VerifierCommandKind::Unknown;
    };
    classify_node_tool_name(parts[next_idx].as_str())
}

fn classify_package_manager_command(parts: &[String], idx: usize) -> VerifierCommandKind {
    let Some(subcommand) = parts.get(idx + 1).map(String::as_str) else {
        return VerifierCommandKind::Unknown;
    };

    match subcommand {
        "test" | "t" => VerifierCommandKind::TypeScriptTest,
        "lint" => VerifierCommandKind::TypeScriptLint,
        "tsc" => VerifierCommandKind::TypeScriptCheck,
        "run" => {
            let Some(script) = parts.get(idx + 2).map(String::as_str) else {
                return VerifierCommandKind::Unknown;
            };
            classify_script_name(script)
        }
        // Yarn can omit the explicit `run`.
        "jest" | "vitest" => VerifierCommandKind::TypeScriptTest,
        "eslint" => VerifierCommandKind::TypeScriptLint,
        "typecheck" => VerifierCommandKind::TypeScriptCheck,
        "exec" | "dlx" => {
            let Some(tool_idx) = first_positional_after_options(parts, idx + 2) else {
                return VerifierCommandKind::Unknown;
            };
            classify_node_tool_name(parts[tool_idx].as_str())
        }
        _ => VerifierCommandKind::Unknown,
    }
}

fn first_positional_after_options(parts: &[String], mut idx: usize) -> Option<usize> {
    while idx < parts.len() {
        let part = parts[idx].as_str();
        if part == "--" {
            idx += 1;
            break;
        }
        if let Some(consumes_next) = node_option_arity(part) {
            idx += 1;
            if consumes_next && !part.contains('=') && idx < parts.len() {
                idx += 1;
            }
            continue;
        }
        if part.starts_with('-') {
            idx += 1;
            continue;
        }
        return Some(idx);
    }
    if idx < parts.len() {
        Some(idx)
    } else {
        None
    }
}

fn node_option_arity(part: &str) -> Option<bool> {
    match part {
        "-p" | "--package" | "-c" | "--call" | "--shell" => Some(true),
        option
            if option.starts_with("--package=")
                || option.starts_with("--call=")
                || option.starts_with("--shell=") =>
        {
            Some(false)
        }
        _ => None,
    }
}

fn classify_script_name(script: &str) -> VerifierCommandKind {
    match script {
        "test" | "unit" | "integration" | "jest" | "vitest" => VerifierCommandKind::TypeScriptTest,
        "lint" | "eslint" => VerifierCommandKind::TypeScriptLint,
        "check" | "typecheck" | "tsc" => VerifierCommandKind::TypeScriptCheck,
        _ => VerifierCommandKind::Unknown,
    }
}

fn classify_node_tool_name(tool: &str) -> VerifierCommandKind {
    match tool {
        "tsc" => VerifierCommandKind::TypeScriptCheck,
        "vitest" | "jest" => VerifierCommandKind::TypeScriptTest,
        "eslint" => VerifierCommandKind::TypeScriptLint,
        _ => VerifierCommandKind::Unknown,
    }
}

fn parse_test_failure(stdout: &str, stderr: &str) -> ParsedFailureReport {
    let combined = format!("{}\n{}", stdout, stderr);
    let mut failures = Vec::new();
    let mut current_symbol: Option<String> = None;
    let mut current_location = FailureTarget::default();

    for line in combined.lines() {
        let trimmed = line.trim();

        if let Some(name) = parse_failed_test_name(trimmed) {
            current_symbol = Some(name);
        }

        if let Some((file, line_no, col_no)) = parse_rust_location(trimmed) {
            current_location.file_path = Some(file);
            current_location.line = Some(line_no);
            current_location.column = Some(col_no);
        }

        if trimmed.contains("assertion `left == right` failed") {
            failures.push(ParsedFailure {
                error_class: "test.assertion".to_string(),
                message: trimmed.to_string(),
                target: FailureTarget {
                    symbol: current_symbol.clone(),
                    ..current_location.clone()
                },
            });
        } else if trimmed
            .to_ascii_lowercase()
            .contains("called `option::unwrap()` on a `none` value")
        {
            failures.push(ParsedFailure {
                error_class: "test.panic_unwrap".to_string(),
                message: trimmed.to_string(),
                target: FailureTarget {
                    symbol: current_symbol.clone(),
                    ..current_location.clone()
                },
            });
        }
    }

    if failures.is_empty() {
        let message = first_nonempty_line(stderr)
            .or_else(|| first_nonempty_line(stdout))
            .unwrap_or_else(|| "test failure".to_string());
        failures.push(ParsedFailure {
            error_class: "test.unknown".to_string(),
            message,
            target: FailureTarget {
                symbol: current_symbol,
                ..current_location
            },
        });
    }

    ParsedFailureReport {
        command: CargoCommandKind::Test,
        stage: FailureStage::Test,
        failures,
    }
}

fn parse_compile_or_lint_failure(stderr: &str, command: CargoCommandKind) -> ParsedFailureReport {
    let mut failures = Vec::new();
    let mut pending: Option<ParsedFailure> = None;
    let is_clippy = matches!(command, CargoCommandKind::Clippy);

    for line in stderr.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let starts_issue = trimmed.starts_with("error")
            || (is_clippy && trimmed.starts_with("warning:"))
            || (is_clippy && trimmed.starts_with("note:"));
        if starts_issue {
            if let Some(existing) = pending.take() {
                failures.push(existing);
            }
            pending = Some(ParsedFailure {
                error_class: classify_error_class(&command, trimmed),
                message: trimmed.to_string(),
                target: FailureTarget {
                    symbol: extract_symbol(trimmed),
                    ..FailureTarget::default()
                },
            });
            continue;
        }

        if let Some((file, line_no, col_no)) = parse_rust_location(trimmed) {
            if let Some(item) = pending.as_mut() {
                item.target.file_path = Some(file);
                item.target.line = Some(line_no);
                item.target.column = Some(col_no);
            }
            continue;
        }

        if let Some(item) = pending.as_mut() {
            if item.target.symbol.is_none() {
                item.target.symbol = extract_symbol(trimmed);
            }
            if should_append_context(trimmed) {
                item.message.push(' ');
                item.message.push_str(trimmed);
            }
        }
    }

    if let Some(existing) = pending.take() {
        failures.push(existing);
    }

    if failures.is_empty() {
        failures.push(ParsedFailure {
            error_class: match command {
                CargoCommandKind::Clippy => "lint.unknown".to_string(),
                CargoCommandKind::Check => "compile.unknown".to_string(),
                _ => "unknown".to_string(),
            },
            message: first_nonempty_line(stderr).unwrap_or_else(|| "unknown failure".to_string()),
            target: FailureTarget::default(),
        });
    }

    let stage = infer_stage_from_failures(&command, &failures);
    ParsedFailureReport {
        command,
        stage,
        failures,
    }
}

fn parse_unknown_failure(stdout: &str, stderr: &str) -> ParsedFailureReport {
    let message = first_nonempty_line(stderr)
        .or_else(|| first_nonempty_line(stdout))
        .unwrap_or_else(|| "unknown cargo failure".to_string());
    ParsedFailureReport {
        command: CargoCommandKind::Unknown,
        stage: FailureStage::Unknown,
        failures: vec![ParsedFailure {
            error_class: "unknown".to_string(),
            message,
            target: FailureTarget::default(),
        }],
    }
}

fn parse_typescript_compile_failure(stdout: &str, stderr: &str) -> ParsedFailureReport {
    let mut failures = Vec::new();
    let combined = format!("{stdout}\n{stderr}");

    for line in combined
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        if let Some((location, error_code, message)) = parse_typescript_error_line(line) {
            let mut target = FailureTarget::default();
            target.file_path = Some(location.file_path);
            target.line = Some(location.line);
            target.column = Some(location.column);
            if let Some(symbol) = extract_quoted_symbol(message) {
                target.symbol = Some(symbol);
            }

            failures.push(ParsedFailure {
                error_class: classify_typescript_compile_error(error_code, message),
                message: format!("TS{error_code}: {message}"),
                target,
            });
        }
    }

    if failures.is_empty() {
        failures.push(ParsedFailure {
            error_class: "compile.unknown".to_string(),
            message: first_nonempty_line(&combined)
                .unwrap_or_else(|| "typescript compile failure".to_string()),
            target: FailureTarget::default(),
        });
    }

    ParsedFailureReport {
        command: CargoCommandKind::Unknown,
        stage: FailureStage::Compile,
        failures,
    }
}

fn parse_typescript_lint_failure(stdout: &str, stderr: &str) -> ParsedFailureReport {
    let combined = format!("{stdout}\n{stderr}");
    let mut failures = Vec::new();

    for line in combined
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        if !line.contains("error") && !line.contains("warning") {
            continue;
        }
        let mut failure = ParsedFailure {
            error_class: "lint.unknown".to_string(),
            message: line.to_string(),
            target: FailureTarget::default(),
        };
        if let Some((file, row, col)) = parse_simple_file_location(line) {
            failure.target.file_path = Some(file);
            failure.target.line = Some(row);
            failure.target.column = Some(col);
        }
        failures.push(failure);
    }

    if failures.is_empty() {
        failures.push(ParsedFailure {
            error_class: "lint.unknown".to_string(),
            message: first_nonempty_line(&combined)
                .unwrap_or_else(|| "typescript lint failure".to_string()),
            target: FailureTarget::default(),
        });
    }

    ParsedFailureReport {
        command: CargoCommandKind::Unknown,
        stage: FailureStage::Lint,
        failures,
    }
}

fn parse_typescript_test_failure(stdout: &str, stderr: &str) -> ParsedFailureReport {
    let combined = format!("{stdout}\n{stderr}");
    let mut failures = Vec::new();
    let mut current_target = FailureTarget::default();
    let mut current_symbol: Option<String> = None;

    for raw in combined.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(file) = parse_failed_test_file(line) {
            current_target.file_path = Some(file);
        }
        if let Some(symbol) = parse_failed_js_test_name(line) {
            current_symbol = Some(symbol);
        }
        if let Some((file, row, col)) = parse_simple_file_location(line) {
            current_target.file_path = Some(file);
            current_target.line = Some(row);
            current_target.column = Some(col);
        }
        if let Some(message) = parse_test_failure_message(line) {
            let error_class = classify_typescript_test_error(&message);
            failures.push(ParsedFailure {
                error_class,
                message,
                target: FailureTarget {
                    symbol: current_symbol.clone(),
                    ..current_target.clone()
                },
            });
        }
    }

    if failures.is_empty() {
        failures.push(ParsedFailure {
            error_class: "test.unknown".to_string(),
            message: first_nonempty_line(&combined)
                .unwrap_or_else(|| "typescript test failure".to_string()),
            target: FailureTarget {
                symbol: current_symbol,
                ..current_target
            },
        });
    }

    ParsedFailureReport {
        command: CargoCommandKind::Unknown,
        stage: FailureStage::Test,
        failures,
    }
}

#[derive(Debug)]
struct TypeScriptLocation {
    file_path: String,
    line: u32,
    column: u32,
}

fn parse_typescript_error_line(line: &str) -> Option<(TypeScriptLocation, &str, &str)> {
    let (prefix, rest) = line.split_once(": error TS")?;
    let (code, message) = rest.split_once(':')?;
    let location = parse_typescript_location(prefix.trim())?;
    Some((location, code.trim(), message.trim()))
}

fn parse_typescript_location(prefix: &str) -> Option<TypeScriptLocation> {
    let start = prefix.rfind('(')?;
    let end = prefix.rfind(')')?;
    if end <= start {
        return None;
    }
    let file_path = prefix[..start].trim();
    let coords = &prefix[start + 1..end];
    let mut parts = coords.split(',');
    let line = parts.next()?.trim().parse::<u32>().ok()?;
    let column = parts.next()?.trim().parse::<u32>().ok()?;
    if file_path.is_empty() {
        return None;
    }
    Some(TypeScriptLocation {
        file_path: file_path.to_string(),
        line,
        column,
    })
}

fn parse_simple_file_location(line: &str) -> Option<(String, u32, u32)> {
    let cleaned = line.trim().trim_start_matches("at ").trim();
    let cleaned = cleaned.trim_matches(|ch| matches!(ch, '(' | ')' | ',' | '\'' | '"'));
    let mut parts = cleaned.rsplitn(3, ':');
    let column = parts
        .next()?
        .split_whitespace()
        .next()?
        .parse::<u32>()
        .ok()?;
    let row = parts.next()?.parse::<u32>().ok()?;
    let file = parts.next()?.trim();
    if file.is_empty()
        || !(file.ends_with(".ts")
            || file.ends_with(".tsx")
            || file.ends_with(".mts")
            || file.ends_with(".cts")
            || file.ends_with(".js")
            || file.ends_with(".jsx"))
    {
        return None;
    }
    Some((file.to_string(), row, column))
}

fn extract_quoted_symbol(message: &str) -> Option<String> {
    let start = message.find('\'')?;
    let tail = &message[start + 1..];
    let end = tail.find('\'')?;
    let symbol = tail[..end].trim();
    if symbol.is_empty() {
        return None;
    }
    Some(symbol.to_string())
}

fn classify_typescript_compile_error(error_code: &str, message: &str) -> String {
    match error_code {
        "2304" => "compile.missing_symbol".to_string(),
        "2339" => "compile.missing_property".to_string(),
        "2322" => "compile.type_mismatch".to_string(),
        "2345" => "compile.invalid_argument_type".to_string(),
        _ => {
            let lower = message.to_ascii_lowercase();
            if lower.contains("not assignable") {
                "compile.type_mismatch".to_string()
            } else {
                "compile.unknown".to_string()
            }
        }
    }
}

fn parse_failed_test_file(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if let Some(rest) = trimmed.strip_prefix("FAIL ") {
        return rest.split_whitespace().next().map(ToString::to_string);
    }
    None
}

fn parse_failed_js_test_name(line: &str) -> Option<String> {
    if let Some(rest) = line.trim().strip_prefix("FAIL ") {
        let mut sections = rest.split('>');
        let _ = sections.next();
        let symbol = sections
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("::");
        if !symbol.is_empty() {
            return Some(symbol);
        }
    }
    None
}

fn parse_test_failure_message(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if let Some(rest) = trimmed.strip_prefix("Error:") {
        let message = rest.trim();
        if !message.is_empty() {
            return Some(message.to_string());
        }
    }
    if let Some(rest) = trimmed.strip_prefix("AssertionError:") {
        let message = rest.trim();
        if !message.is_empty() {
            return Some(message.to_string());
        }
    }
    if trimmed.starts_with("Expected:") || trimmed.starts_with("Received:") {
        return Some(trimmed.to_string());
    }
    None
}

fn classify_typescript_test_error(message: &str) -> String {
    let lower = message.to_ascii_lowercase();
    if lower.contains("expected:") || lower.contains("received:") || lower.contains("toequal") {
        return "test.assertion".to_string();
    }
    if lower.contains("timed out") || lower.contains("timeout") {
        return "test.timeout".to_string();
    }
    if lower.contains("cannot read properties of undefined") || lower.contains("is not a function")
    {
        return "test.runtime_type_error".to_string();
    }
    "test.unknown".to_string()
}

fn classify_error_class(command: &CargoCommandKind, message: &str) -> String {
    let lower = message.to_ascii_lowercase();
    if lower.contains("unclosed delimiter") {
        return "parser.unclosed_delimiter".to_string();
    }
    if lower.contains("mismatched types") {
        return "compile.type_mismatch".to_string();
    }
    if lower.contains("cannot find function") {
        return "compile.missing_function".to_string();
    }
    if lower.contains("has no field named") {
        return "compile.unknown_struct_field".to_string();
    }
    if lower.contains("doesn't implement")
        || lower.contains("trait bound")
        || lower.contains("the trait")
    {
        return "compile.missing_trait_bound".to_string();
    }
    if lower.contains("unneeded `return` statement") || lower.contains("needless_return") {
        return "lint.clippy_needless_return".to_string();
    }
    if lower.contains("this operation has no effect") || lower.contains("identity_op") {
        return "lint.clippy_identity_op".to_string();
    }

    match command {
        CargoCommandKind::Clippy => "lint.unknown".to_string(),
        CargoCommandKind::Check => "compile.unknown".to_string(),
        _ => "unknown".to_string(),
    }
}

fn infer_stage_from_failures(
    command: &CargoCommandKind,
    failures: &[ParsedFailure],
) -> FailureStage {
    if matches!(command, CargoCommandKind::Clippy) {
        return FailureStage::Lint;
    }
    if matches!(command, CargoCommandKind::Check) {
        if failures.iter().any(|f| {
            f.error_class.starts_with("parser.") || f.message.contains("unclosed delimiter")
        }) {
            return FailureStage::Parse;
        }
        return FailureStage::Compile;
    }
    FailureStage::Unknown
}

fn first_nonempty_line(input: &str) -> Option<String> {
    input
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(ToString::to_string)
}

fn parse_failed_test_name(line: &str) -> Option<String> {
    if line.starts_with("test ") && line.contains(" ... FAILED") {
        let mut split = line.split_whitespace();
        split.next();
        return split.next().map(ToString::to_string);
    }
    if line.starts_with("---- ") && line.ends_with(" stdout ----") {
        let inner = line
            .trim_start_matches("---- ")
            .trim_end_matches(" stdout ----");
        if !inner.is_empty() {
            return Some(inner.to_string());
        }
    }
    None
}

fn parse_rust_location(line: &str) -> Option<(String, u32, u32)> {
    let mut location = line.trim().trim_start_matches("-->").trim();
    if let Some((_, tail)) = location.rsplit_once(", ") {
        location = tail.trim();
    } else if location.contains(' ') {
        location = location.split_whitespace().last()?;
    }
    let location = location.trim_matches(|c| matches!(c, '\'' | '"' | ',' | ')' | '('));

    let mut parts = location.rsplitn(3, ':');
    let col = parts.next()?.parse::<u32>().ok()?;
    let row = parts.next()?.parse::<u32>().ok()?;
    let path = parts.next()?.trim();
    if path.is_empty() {
        return None;
    }
    Some((path.to_string(), row, col))
}

fn extract_symbol(text: &str) -> Option<String> {
    let mut collecting = false;
    let mut symbol = String::new();
    for ch in text.chars() {
        if ch == '`' {
            if collecting {
                return if symbol.is_empty() {
                    None
                } else {
                    Some(symbol)
                };
            }
            collecting = true;
            continue;
        }
        if collecting {
            symbol.push(ch);
        }
    }
    None
}

fn should_append_context(line: &str) -> bool {
    line.starts_with("help:")
        || line.starts_with("note:")
        || line.starts_with("= note:")
        || line.starts_with("= help:")
}
