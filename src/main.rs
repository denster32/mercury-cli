//! Mercury CLI — the first diffusion-native CLI for autonomous code synthesis.
//!
//! Uses Inception Labs' Mercury 2 with thermal heat maps as a stigmergic
//! coordination primitive for multi-agent code editing.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::env;
use std::fs::OpenOptions;
use std::io::IsTerminal;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use toml_edit::{value as toml_value, DocumentMut, Item, Table};

use mercury_cli::api::{self, Mercury2Client, MercuryEditClient};
use mercury_cli::db::{self, ThermalDb};
use mercury_cli::engine::{
    self, Scheduler, SchedulerConfig, StepExecutionSummary, Verifier, VerifyConfig,
};
use mercury_cli::failure_parser::{self, CargoCommandKind};
use mercury_cli::repo::{self, RepoRelativePath};
use mercury_cli::swarm;
use mercury_cli::thermal::{self, render_heatmap_to_string};
use mercury_cli::verification;

// ---------------------------------------------------------------------------
// Top-level error
// ---------------------------------------------------------------------------

/// Top-level error wrapping all module errors.
#[derive(Error, Debug)]
pub enum MercuryError {
    #[error("database error: {0}")]
    Db(#[from] db::DbError),

    #[error("thermal error: {0}")]
    Thermal(#[from] thermal::ThermalError),

    #[error("API error: {0}")]
    Api(#[from] api::ApiError),

    #[error("repo error: {0}")]
    Repo(#[from] repo::RepoError),

    #[error("engine error: {0}")]
    Engine(#[from] engine::EngineError),

    #[error("swarm error: {0}")]
    Swarm(#[from] swarm::SwarmError),

    #[error("config error: {0}")]
    Config(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Mercury CLI configuration deserialized from .mercury/config.toml.
#[derive(Debug, Deserialize)]
pub struct MercuryConfig {
    pub api: ApiConfig,
    pub scheduler: SchedulerConfigToml,
    pub thermal: ThermalConfig,
    pub annealing: AnnealingConfig,
    pub verification: VerificationConfig,
    pub constitutional: ConstitutionalConfig,
    #[serde(default)]
    pub repo: RepoConfig,
}

#[derive(Debug, Deserialize)]
pub struct ApiConfig {
    pub mercury2_endpoint: String,
    pub mercury_edit_endpoint: String,
    pub api_key_env: String,
}

#[derive(Debug, Deserialize)]
pub struct SchedulerConfigToml {
    pub max_concurrency: usize,
    pub max_cost_per_command: f64,
    pub max_agents_per_command: usize,
    pub retry_limit: u32,
    pub backoff_base_ms: u64,
}

#[derive(Debug, Deserialize)]
pub struct ThermalConfig {
    pub decay_half_life_seconds: f64,
    pub aggregation_method: String,
    pub rescan_on_git_pull: bool,
    pub hot_threshold: f64,
    pub cool_threshold: f64,
    pub lock_cool_zones: bool,
}

#[derive(Debug, Deserialize)]
pub struct AnnealingConfig {
    pub enable_global_momentum: bool,
    pub initial_temperature: f64,
    pub cooling_rate: f64,
    pub min_modification_threshold: f64,
}

#[derive(Debug, Deserialize)]
pub struct VerificationConfig {
    pub parse_before_write: bool,
    pub test_after_write: bool,
    pub lint_after_write: bool,
    pub mercury2_critique_on_failure: bool,
    pub test_command: String,
    pub lint_command: String,
}

#[derive(Debug, Deserialize)]
pub struct ConstitutionalConfig {
    pub style_guide: String,
    pub architecture_rules: String,
    pub naming_conventions: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct RepoConfig {
    #[serde(default)]
    pub languages: RepoLanguagesConfig,
}

#[derive(Debug, Deserialize)]
pub struct RepoLanguagesConfig {
    #[serde(default = "default_true")]
    pub rust: bool,
    #[serde(default)]
    pub python: bool,
    #[serde(default)]
    pub typescript: bool,
    #[serde(default)]
    pub go: bool,
    #[serde(default)]
    pub java: bool,
}

fn default_true() -> bool {
    true
}

impl Default for RepoLanguagesConfig {
    fn default() -> Self {
        Self {
            rust: true,
            python: false,
            typescript: false,
            go: false,
            java: false,
        }
    }
}

impl MercuryConfig {
    /// Load configuration from a TOML file.
    fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config: {}", path.display()))?;
        let config: MercuryConfig =
            toml::from_str(&content).with_context(|| "failed to parse config.toml")?;
        Ok(config)
    }

    /// Build a constitutional prompt from the config.
    fn constitutional_prompt(&self) -> String {
        let mut parts = Vec::new();
        if !self.constitutional.style_guide.is_empty() {
            parts.push(format!("Style guide: {}", self.constitutional.style_guide));
        }
        if !self.constitutional.architecture_rules.is_empty() {
            parts.push(format!(
                "Architecture rules: {}",
                self.constitutional.architecture_rules
            ));
        }
        if !self.constitutional.naming_conventions.is_empty() {
            parts.push(format!(
                "Naming conventions: {}",
                self.constitutional.naming_conventions
            ));
        }
        parts.join("\n")
    }

    fn repo_languages(&self) -> repo::RepoLanguages {
        repo::RepoLanguages {
            rust: self.repo.languages.rust,
            python: self.repo.languages.python,
            typescript: self.repo.languages.typescript,
            go: self.repo.languages.go,
            java: self.repo.languages.java,
        }
    }

    /// Convert to engine SchedulerConfig.
    fn to_scheduler_config(&self) -> SchedulerConfig {
        SchedulerConfig {
            max_concurrency: self.scheduler.max_concurrency,
            max_cost_per_command: self.scheduler.max_cost_per_command,
            retry_limit: self.scheduler.retry_limit,
            backoff_base_ms: self.scheduler.backoff_base_ms,
            decay_half_life_seconds: self.thermal.decay_half_life_seconds,
            hot_threshold: self.thermal.hot_threshold,
            cool_threshold: self.thermal.cool_threshold,
        }
    }
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

/// Mercury CLI — diffusion-native autonomous code synthesis.
#[derive(Parser)]
#[command(name = "mercury", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize Mercury in the current repository.
    Init,

    /// Generate a thermal heat map and execution plan.
    Plan {
        /// The goal or description of what to accomplish.
        goal: String,
        /// Output the plan as JSON.
        #[arg(long)]
        output: Option<PathBuf>,
    },

    /// Show thermal state, active agents, and budget consumption.
    Status {
        /// Render the thermal heatmap.
        #[arg(long)]
        heatmap: bool,
        /// Stream live status in a TTY dashboard or JSONL event feed until Ctrl-C.
        #[arg(long)]
        live: bool,
        /// Live refresh interval in milliseconds.
        #[arg(long, default_value = "1500")]
        interval_ms: u64,
        /// Show active agents.
        #[arg(long)]
        agents: bool,
        /// Show budget consumption.
        #[arg(long)]
        budget: bool,
    },

    /// Ask Mercury 2 a question about the codebase.
    Ask {
        /// The query to ask.
        query: String,
        /// Reasoning effort: instant, low, medium, high.
        #[arg(long)]
        reasoning_effort: Option<String>,
    },

    /// Edit commands (apply, complete, next).
    Edit {
        #[command(subcommand)]
        action: EditAction,
    },

    /// The killer command: plan, index, patch, verify, commit.
    Fix {
        /// Issue or description of what to fix.
        description: String,
        /// Maximum number of agents to spawn.
        #[arg(long, default_value = "20")]
        max_agents: usize,
        /// Maximum cost in USD.
        #[arg(long, default_value = "0.50")]
        max_cost: f64,
        /// Emit compact CI-safe runtime events instead of interactive progress output.
        #[arg(long)]
        noninteractive: bool,
    },

    /// Watch an allowlisted verifier command and auto-repair failures.
    Watch {
        /// The direct allowlisted verifier command to watch.
        command: String,
        /// Auto-repair failures.
        #[arg(long)]
        repair: bool,
        /// Run a single CI-safe watch cycle with compact runtime events.
        #[arg(long)]
        noninteractive: bool,
    },

    /// Get or set configuration values.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(Subcommand)]
enum EditAction {
    /// Apply a concrete replacement snippet or patch to a file.
    Apply {
        /// The file to edit.
        file: String,
        /// Concrete replacement code or patch content for Mercury Edit apply.
        #[arg(long = "update-snippet")]
        update_snippet: String,
        /// Dry run — show diff without writing.
        #[arg(long)]
        dry_run: bool,
        /// Bypass verification checks and force-write the patch.
        #[arg(long)]
        force: bool,
    },
    /// Autocomplete at a cursor position.
    Complete {
        /// File and optional line (file.rs:42).
        file: String,
    },
    /// Predict the next edit based on file content and cursor context.
    Next {
        /// File and optional line (file.rs:42).
        file: String,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Get a configuration value.
    Get {
        /// The config key (e.g., "scheduler.max_concurrency").
        key: String,
    },
    /// Set a configuration value.
    Set {
        /// The config key.
        key: String,
        /// The new value.
        value: String,
    },
    /// Validate the current configuration.
    Validate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConfigValueKind {
    String,
    Bool,
    Integer,
    Float,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NextEditCliContext {
    current_file_path: String,
    code_to_edit: String,
    cursor: String,
    recent_snippets: String,
}

const SUPPORTED_CONFIG_KEYS: &[(&str, ConfigValueKind)] = &[
    ("api.mercury2_endpoint", ConfigValueKind::String),
    ("api.mercury_edit_endpoint", ConfigValueKind::String),
    ("api.api_key_env", ConfigValueKind::String),
    ("scheduler.max_concurrency", ConfigValueKind::Integer),
    ("scheduler.max_cost_per_command", ConfigValueKind::Float),
    ("scheduler.max_agents_per_command", ConfigValueKind::Integer),
    ("scheduler.retry_limit", ConfigValueKind::Integer),
    ("scheduler.backoff_base_ms", ConfigValueKind::Integer),
    ("thermal.decay_half_life_seconds", ConfigValueKind::Float),
    ("thermal.aggregation_method", ConfigValueKind::String),
    ("thermal.rescan_on_git_pull", ConfigValueKind::Bool),
    ("thermal.hot_threshold", ConfigValueKind::Float),
    ("thermal.cool_threshold", ConfigValueKind::Float),
    ("thermal.lock_cool_zones", ConfigValueKind::Bool),
    ("annealing.enable_global_momentum", ConfigValueKind::Bool),
    ("annealing.initial_temperature", ConfigValueKind::Float),
    ("annealing.cooling_rate", ConfigValueKind::Float),
    (
        "annealing.min_modification_threshold",
        ConfigValueKind::Float,
    ),
    ("verification.parse_before_write", ConfigValueKind::Bool),
    ("verification.test_after_write", ConfigValueKind::Bool),
    ("verification.lint_after_write", ConfigValueKind::Bool),
    (
        "verification.mercury2_critique_on_failure",
        ConfigValueKind::Bool,
    ),
    ("verification.test_command", ConfigValueKind::String),
    ("verification.lint_command", ConfigValueKind::String),
    ("repo.languages.rust", ConfigValueKind::Bool),
    ("repo.languages.python", ConfigValueKind::Bool),
    ("repo.languages.typescript", ConfigValueKind::Bool),
    ("repo.languages.go", ConfigValueKind::Bool),
    ("repo.languages.java", ConfigValueKind::Bool),
    ("constitutional.style_guide", ConfigValueKind::String),
    ("constitutional.architecture_rules", ConfigValueKind::String),
    ("constitutional.naming_conventions", ConfigValueKind::String),
];

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Find the .mercury directory by walking up from the current directory.
fn find_mercury_dir() -> Result<PathBuf> {
    let mut dir = std::env::current_dir()?;
    loop {
        let mercury_dir = dir.join(".mercury");
        if mercury_dir.is_dir() {
            return Ok(mercury_dir);
        }
        if !dir.pop() {
            anyhow::bail!(
                "not a Mercury project (no .mercury/ directory found). Run `mercury init` first."
            );
        }
    }
}

/// Load the config and open the database.
fn load_project() -> Result<(MercuryConfig, ThermalDb, PathBuf)> {
    let mercury_dir = find_mercury_dir()?;
    let config_path = mercury_dir.join("config.toml");
    let db_path = mercury_dir.join("thermal.db");
    let config = MercuryConfig::load(&config_path)?;
    let db = ThermalDb::open(&db_path).context("failed to open thermal database")?;
    let project_root = mercury_dir
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    Ok((config, db, project_root))
}

const VERIFIER_ALLOWLIST_OVERRIDE_ENV: &str = "MERCURY_ALLOW_UNSAFE_VERIFIER_COMMANDS";
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
const SANDBOX_POLICY: &str = "repo_isolated_worktree_copy";
const EXECUTION_SANDBOX_ENFORCED: bool = false;

fn running_in_ci() -> bool {
    env::var("CI")
        .map(|value| value.eq_ignore_ascii_case("true") || value == "1")
        .unwrap_or(false)
        || env::var("GITHUB_ACTIONS")
            .map(|value| value.eq_ignore_ascii_case("true") || value == "1")
            .unwrap_or(false)
}

fn is_noninteractive_mode(explicit: bool) -> bool {
    explicit || running_in_ci() || !std::io::stdout().is_terminal()
}

fn allowlist_override_active() -> bool {
    env::var(VERIFIER_ALLOWLIST_OVERRIDE_ENV)
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

fn verifier_command_allowlisted(command: &str) -> bool {
    verification::verifier_command_allowlisted(command)
}

fn enforce_verifier_allowlist(verify_config: &VerifyConfig) -> Result<()> {
    let override_enabled = allowlist_override_active();
    if override_enabled {
        if running_in_ci() {
            anyhow::bail!(
                "verifier allowlist override via {} is disabled in CI",
                VERIFIER_ALLOWLIST_OVERRIDE_ENV
            );
        }
        eprintln!(
            "WARNING: verifier command allowlist bypass enabled via {}",
            VERIFIER_ALLOWLIST_OVERRIDE_ENV
        );
        return Ok(());
    }

    let mut violations = Vec::new();
    if verify_config.test_after_write && !verifier_command_allowlisted(&verify_config.test_command)
    {
        violations.push(format!(
            "test_command=`{}`",
            verify_config.test_command.trim()
        ));
    }
    if verify_config.lint_after_write && !verifier_command_allowlisted(&verify_config.lint_command)
    {
        violations.push(format!(
            "lint_command=`{}`",
            verify_config.lint_command.trim()
        ));
    }

    if violations.is_empty() {
        return Ok(());
    }

    anyhow::bail!(
        "verifier allowlist violation: {}. Supported verifier commands are direct allowlisted Rust/TypeScript verifier invocations (including env-prefix variants) without shell composition. Set {}=1 to bypass.",
        violations.join(", "),
        VERIFIER_ALLOWLIST_OVERRIDE_ENV
    )
}

fn redact_secrets(text: &str) -> String {
    let mut redacted = text.to_string();

    for key in SENSITIVE_ENV_VARS {
        if let Ok(value) = env::var(key) {
            let trimmed = value.trim();
            if trimmed.len() >= 4 && redacted.contains(trimmed) {
                redacted = redacted.replace(trimmed, &format!("[REDACTED:{}]", key));
            }
        }
    }

    let mut sanitized_lines = Vec::new();
    for line in redacted.lines() {
        let lowercase = line.to_ascii_lowercase();
        if SENSITIVE_MARKERS
            .iter()
            .any(|marker| lowercase.contains(marker))
        {
            if let Some((prefix, _)) = line.split_once('=') {
                sanitized_lines.push(format!("{prefix}=[REDACTED]"));
                continue;
            }
            if let Some((prefix, _)) = line.split_once(':') {
                sanitized_lines.push(format!("{prefix}: [REDACTED]"));
                continue;
            }
        }
        sanitized_lines.push(line.to_string());
    }

    let mut joined = sanitized_lines.join("\n");
    if text.ends_with('\n') && !joined.ends_with('\n') {
        joined.push('\n');
    }
    joined
}

fn redact_json_value(value: Value) -> Value {
    match value {
        Value::String(text) => Value::String(redact_secrets(&text)),
        Value::Array(items) => Value::Array(items.into_iter().map(redact_json_value).collect()),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, value)| (key, redact_json_value(value)))
                .collect(),
        ),
        other => other,
    }
}

fn event_payload(event: &str, details: Value) -> Value {
    serde_json::json!({
        "timestamp": Utc::now().to_rfc3339(),
        "event": event,
        "details": redact_json_value(details),
    })
}

fn emit_runtime_event(noninteractive: bool, event: &str, details: Value) {
    if !noninteractive {
        return;
    }

    let payload = event_payload(event, details);
    match serde_json::to_string(&payload) {
        Ok(line) => println!("{line}"),
        Err(err) => eprintln!("failed to serialize runtime event {event}: {err}"),
    }
}

fn write_audit_event(artifact_root: &Path, event: &str, details: serde_json::Value) -> Result<()> {
    let path = artifact_root.join("audit.log");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let payload = event_payload(event, details);
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    serde_json::to_writer(&mut file, &payload)?;
    file.write_all(b"\n")?;
    Ok(())
}

const LIVE_EVENT_BACKLOG_LIMIT: usize = 8;
const LIVE_EVENT_BUFFER_LIMIT: usize = 14;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LiveRenderMode {
    Terminal,
    JsonStream,
}

impl LiveRenderMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Terminal => "tty",
            Self::JsonStream => "jsonl",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
struct CandidateRuntimeMetadata {
    phase: Option<String>,
    source_lineage: Option<String>,
    retry_count: Option<u64>,
    outcome: Option<String>,
    reason: Option<String>,
    failure_stage: Option<String>,
    sandbox_root: Option<String>,
    fanout: Option<u64>,
    temperature: Option<f64>,
    touched_lines: Option<u64>,
    byte_delta: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
struct CandidateLiveState {
    log_id: i64,
    agent_id: String,
    file_path: String,
    command: String,
    status: String,
    started_at: String,
    completed_at: Option<String>,
    tokens_used: i64,
    cost_usd: f64,
    metadata: CandidateRuntimeMetadata,
}

#[derive(Debug, Clone, PartialEq)]
struct RuntimeOverview {
    active_agents: i64,
    total_agents_spawned: i64,
    total_tokens_used: i64,
    total_cost_usd: f64,
    global_temperature: f64,
    iteration_count: i64,
    hottest_file: Option<String>,
    hottest_score: Option<f64>,
    max_agent_density: i32,
    locked_files: usize,
}

#[derive(Debug, Clone)]
struct LiveStatusSnapshot {
    logs_by_id: BTreeMap<i64, CandidateLiveState>,
    ordered_log_ids: Vec<i64>,
    phase_counts: BTreeMap<String, usize>,
    active_phase_counts: BTreeMap<String, usize>,
    runtime: RuntimeOverview,
}

#[derive(Debug, Clone)]
struct LiveStatusEvent {
    event: &'static str,
    details: Value,
}

fn phase_from_command(command: &str) -> Option<String> {
    command
        .split_once(':')
        .map(|(_, phase)| phase.trim().to_string())
        .filter(|phase| !phase.is_empty())
}

fn parse_candidate_runtime_metadata(entry: &db::AgentLogEntry) -> CandidateRuntimeMetadata {
    let mut metadata = CandidateRuntimeMetadata {
        phase: phase_from_command(&entry.command),
        ..CandidateRuntimeMetadata::default()
    };

    let Some(raw) = entry.micro_heatmap.as_deref() else {
        return metadata;
    };
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(raw) else {
        return metadata;
    };

    let string_field = |key: &str| {
        map.get(key)
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .filter(|value| !value.is_empty())
    };
    let u64_field = |key: &str| map.get(key).and_then(Value::as_u64);
    let i64_field = |key: &str| map.get(key).and_then(Value::as_i64);
    let f64_field = |key: &str| map.get(key).and_then(Value::as_f64);

    metadata.phase = string_field("phase").or(metadata.phase);
    metadata.source_lineage = string_field("candidate_source");
    metadata.retry_count = u64_field("retry_attempts");
    metadata.outcome = string_field("outcome");
    metadata.reason = string_field("reason");
    metadata.failure_stage = string_field("failure_stage");
    metadata.sandbox_root = string_field("sandbox_root");
    metadata.fanout = u64_field("fanout");
    metadata.temperature = f64_field("temperature");
    metadata.touched_lines = u64_field("touched_lines");
    metadata.byte_delta = i64_field("byte_delta");
    metadata
}

fn build_candidate_live_state(entry: &db::AgentLogEntry) -> CandidateLiveState {
    CandidateLiveState {
        log_id: entry.id,
        agent_id: entry.agent_id.clone(),
        file_path: entry.file_path.clone(),
        command: entry.command.clone(),
        status: entry.status.clone(),
        started_at: entry.started_at.clone(),
        completed_at: entry.completed_at.clone(),
        tokens_used: entry.tokens_used,
        cost_usd: entry.cost_usd,
        metadata: parse_candidate_runtime_metadata(entry),
    }
}

fn build_runtime_overview(
    aggregates: &[db::ThermalAggregate],
    active_agents: &[db::AgentLogEntry],
    logs: &[db::AgentLogEntry],
    state: Option<&db::SwarmState>,
) -> RuntimeOverview {
    let hottest = aggregates.iter().max_by(|left, right| {
        left.composite_score
            .partial_cmp(&right.composite_score)
            .unwrap_or(Ordering::Equal)
    });
    let total_tokens_from_logs: i64 = logs.iter().map(|entry| entry.tokens_used).sum();
    let total_cost_from_logs: f64 = logs.iter().map(|entry| entry.cost_usd).sum();

    RuntimeOverview {
        active_agents: active_agents.len() as i64,
        total_agents_spawned: state
            .map(|value| value.total_agents_spawned)
            .unwrap_or(logs.len() as i64),
        total_tokens_used: state
            .map(|value| value.total_tokens_used)
            .unwrap_or(total_tokens_from_logs),
        total_cost_usd: state
            .map(|value| value.total_cost_usd)
            .unwrap_or(total_cost_from_logs),
        global_temperature: state.map(|value| value.global_temperature).unwrap_or(0.0),
        iteration_count: state.map(|value| value.iteration_count).unwrap_or(0),
        hottest_file: hottest.map(|aggregate| aggregate.file_path.clone()),
        hottest_score: hottest.map(|aggregate| aggregate.composite_score),
        max_agent_density: aggregates
            .iter()
            .map(|aggregate| aggregate.agent_density)
            .max()
            .unwrap_or_default(),
        locked_files: aggregates
            .iter()
            .filter(|aggregate| aggregate.is_locked)
            .count(),
    }
}

fn build_live_status_snapshot(
    aggregates: &[db::ThermalAggregate],
    active_agents: &[db::AgentLogEntry],
    logs: &[db::AgentLogEntry],
    state: Option<&db::SwarmState>,
) -> LiveStatusSnapshot {
    let mut logs_by_id = BTreeMap::new();
    let mut ordered = logs
        .iter()
        .map(|entry| (entry.id, entry.started_at.clone()))
        .collect::<Vec<_>>();
    ordered.sort_by(|left, right| left.1.cmp(&right.1).then(left.0.cmp(&right.0)));

    let mut phase_counts = BTreeMap::new();
    let mut active_phase_counts = BTreeMap::new();
    for entry in logs {
        let state = build_candidate_live_state(entry);
        if let Some(phase) = state.metadata.phase.as_ref() {
            *phase_counts.entry(phase.clone()).or_default() += 1;
            if matches!(state.status.as_str(), "spawned" | "running" | "retrying") {
                *active_phase_counts.entry(phase.clone()).or_default() += 1;
            }
        }
        logs_by_id.insert(state.log_id, state);
    }

    LiveStatusSnapshot {
        logs_by_id,
        ordered_log_ids: ordered.into_iter().map(|(id, _)| id).collect(),
        phase_counts,
        active_phase_counts,
        runtime: build_runtime_overview(aggregates, active_agents, logs, state),
    }
}

fn candidate_verification_result(state: &CandidateLiveState) -> &'static str {
    if state.status == "success" {
        return "passed";
    }
    if matches!(candidate_decision(state), "lost" | "suppressed")
        && state.metadata.failure_stage.as_deref() != Some("verification")
    {
        return "passed_not_selected";
    }
    if state.metadata.failure_stage.as_deref() == Some("verification") {
        return "failed";
    }
    match state.status.as_str() {
        "retrying" => "retrying",
        "spawned" | "running" => "pending",
        _ => "not_reached",
    }
}

fn candidate_decision(state: &CandidateLiveState) -> &'static str {
    if state.status == "success" {
        return "won";
    }
    if state
        .metadata
        .reason
        .as_deref()
        .is_some_and(|reason| reason.starts_with("oscillation suppressed:"))
    {
        return "suppressed";
    }
    if state
        .metadata
        .reason
        .as_deref()
        .is_some_and(|reason| reason.starts_with("duplicate verified candidate output"))
    {
        return "suppressed";
    }
    if state.metadata.failure_stage.as_deref() == Some("verification")
        || state.metadata.reason.as_deref().is_some_and(|reason| {
            reason.starts_with("lost selection to higher-ranked verified candidate")
        })
    {
        return "lost";
    }
    match state.status.as_str() {
        "retrying" | "spawned" | "running" => "in_flight",
        _ => "rejected",
    }
}

fn candidate_explanation(state: &CandidateLiveState) -> Option<String> {
    if let Some(reason) = state.metadata.reason.clone() {
        return Some(reason);
    }
    match state.status.as_str() {
        "success" => Some("verified candidate selected for application".to_string()),
        "retrying" => Some("candidate queued for critique retry".to_string()),
        "spawned" => Some("candidate launched".to_string()),
        "running" => Some("candidate generation or verification in progress".to_string()),
        "failed" => match state.metadata.failure_stage.as_deref() {
            Some("generation") => Some("candidate generation failed".to_string()),
            Some("safety") => Some("candidate blocked by safety checks".to_string()),
            Some("verification") => Some("candidate failed verification".to_string()),
            _ => None,
        },
        _ => None,
    }
}

fn candidate_event_details(
    state: &CandidateLiveState,
    transition: &str,
    backlog: bool,
) -> serde_json::Value {
    serde_json::json!({
        "transition": transition,
        "backlog": backlog,
        "log_id": state.log_id,
        "agent_id": state.agent_id,
        "file_path": state.file_path,
        "status": state.status,
        "phase": state.metadata.phase,
        "source_lineage": state.metadata.source_lineage,
        "retry_count": state.metadata.retry_count,
        "outcome": state.metadata.outcome,
        "decision": candidate_decision(state),
        "explanation": candidate_explanation(state),
        "verification_result": candidate_verification_result(state),
        "reason": state.metadata.reason,
        "failure_stage": state.metadata.failure_stage,
        "sandbox_root": state.metadata.sandbox_root,
        "fanout": state.metadata.fanout,
        "temperature": state.metadata.temperature,
        "touched_lines": state.metadata.touched_lines,
        "byte_delta": state.metadata.byte_delta,
        "tokens_used": state.tokens_used,
        "cost_usd": state.cost_usd,
        "started_at": state.started_at,
        "completed_at": state.completed_at,
    })
}

fn phase_started_event(
    phase: &str,
    observed_candidates: usize,
    active_candidates: usize,
    backlog: bool,
) -> LiveStatusEvent {
    LiveStatusEvent {
        event: "phase_started",
        details: serde_json::json!({
            "phase": phase,
            "observed_candidates": observed_candidates,
            "active_candidates": active_candidates,
            "backlog": backlog,
        }),
    }
}

fn runtime_state_event(runtime: &RuntimeOverview, backlog: bool) -> LiveStatusEvent {
    LiveStatusEvent {
        event: "runtime_state",
        details: serde_json::json!({
            "active_agents": runtime.active_agents,
            "total_agents_spawned": runtime.total_agents_spawned,
            "total_tokens_used": runtime.total_tokens_used,
            "total_cost_usd": runtime.total_cost_usd,
            "global_temperature": runtime.global_temperature,
            "iteration_count": runtime.iteration_count,
            "hottest_file": runtime.hottest_file,
            "hottest_score": runtime.hottest_score,
            "max_agent_density": runtime.max_agent_density,
            "locked_files": runtime.locked_files,
            "backlog": backlog,
        }),
    }
}

fn initial_live_status_events(
    snapshot: &LiveStatusSnapshot,
    render_mode: LiveRenderMode,
    interval_ms: u64,
) -> Vec<LiveStatusEvent> {
    let mut events = vec![LiveStatusEvent {
        event: "live_attached",
        details: serde_json::json!({
            "mode": render_mode.as_str(),
            "interval_ms": interval_ms,
        }),
    }];

    for (phase, observed) in &snapshot.phase_counts {
        events.push(phase_started_event(
            phase,
            *observed,
            snapshot
                .active_phase_counts
                .get(phase)
                .copied()
                .unwrap_or_default(),
            true,
        ));
    }

    let backlog_ids = snapshot
        .ordered_log_ids
        .iter()
        .rev()
        .take(LIVE_EVENT_BACKLOG_LIMIT)
        .copied()
        .collect::<Vec<_>>();
    for log_id in backlog_ids.into_iter().rev() {
        if let Some(state) = snapshot.logs_by_id.get(&log_id) {
            events.push(LiveStatusEvent {
                event: "candidate_event",
                details: candidate_event_details(state, "observed", true),
            });
        }
    }

    events.push(runtime_state_event(&snapshot.runtime, true));
    events
}

fn diff_live_status_events(
    previous: &LiveStatusSnapshot,
    current: &LiveStatusSnapshot,
) -> Vec<LiveStatusEvent> {
    let mut events = Vec::new();

    for (phase, observed) in &current.phase_counts {
        if !previous.phase_counts.contains_key(phase) {
            events.push(phase_started_event(
                phase,
                *observed,
                current
                    .active_phase_counts
                    .get(phase)
                    .copied()
                    .unwrap_or_default(),
                false,
            ));
        }
    }

    for log_id in &current.ordered_log_ids {
        let Some(state) = current.logs_by_id.get(log_id) else {
            continue;
        };
        match previous.logs_by_id.get(log_id) {
            None => events.push(LiveStatusEvent {
                event: "candidate_event",
                details: candidate_event_details(state, "launched", false),
            }),
            Some(previous_state) if previous_state.status != state.status => {
                events.push(LiveStatusEvent {
                    event: "candidate_event",
                    details: candidate_event_details(state, "status_changed", false),
                })
            }
            Some(previous_state) if previous_state != state => events.push(LiveStatusEvent {
                event: "candidate_event",
                details: candidate_event_details(state, "metadata_updated", false),
            }),
            Some(_) => {}
        }
    }

    if previous.runtime != current.runtime {
        events.push(runtime_state_event(&current.runtime, false));
    }

    events
}

fn push_live_event_line(buffer: &mut VecDeque<String>, line: String) {
    buffer.push_back(line);
    while buffer.len() > LIVE_EVENT_BUFFER_LIMIT {
        buffer.pop_front();
    }
}

fn value_string<'a>(details: &'a Value, key: &str) -> Option<&'a str> {
    details.get(key).and_then(Value::as_str)
}

fn value_u64(details: &Value, key: &str) -> Option<u64> {
    details.get(key).and_then(Value::as_u64)
}

fn value_i64(details: &Value, key: &str) -> Option<i64> {
    details.get(key).and_then(Value::as_i64)
}

fn value_f64(details: &Value, key: &str) -> Option<f64> {
    details.get(key).and_then(Value::as_f64)
}

fn format_live_event_line(event: &LiveStatusEvent) -> String {
    let now = Utc::now().format("%H:%M:%S");
    let details = &event.details;
    match event.event {
        "live_attached" => format!(
            "[{now}] live attached | mode={} | refresh={}ms",
            value_string(details, "mode").unwrap_or("unknown"),
            value_u64(details, "interval_ms").unwrap_or_default()
        ),
        "phase_started" => format!(
            "[{now}] phase {} | observed={} | active={}",
            value_string(details, "phase").unwrap_or("unknown"),
            value_u64(details, "observed_candidates").unwrap_or_default(),
            value_u64(details, "active_candidates").unwrap_or_default()
        ),
        "runtime_state" => {
            let hot = match (
                value_string(details, "hottest_file"),
                value_f64(details, "hottest_score"),
            ) {
                (Some(file), Some(score)) => format!(" | hottest={file}@{score:.2}"),
                (Some(file), None) => format!(" | hottest={file}"),
                _ => String::new(),
            };
            format!(
                "[{now}] runtime | active={} | total={} | tokens={} | cost=${:.4} | temp={:.2} | iter={} | density={} | locks={}{}",
                value_i64(details, "active_agents").unwrap_or_default(),
                value_i64(details, "total_agents_spawned").unwrap_or_default(),
                value_i64(details, "total_tokens_used").unwrap_or_default(),
                value_f64(details, "total_cost_usd").unwrap_or_default(),
                value_f64(details, "global_temperature").unwrap_or_default(),
                value_i64(details, "iteration_count").unwrap_or_default(),
                value_i64(details, "max_agent_density").unwrap_or_default(),
                value_u64(details, "locked_files").unwrap_or_default(),
                hot
            )
        }
        "candidate_event" => {
            let mut parts = vec![format!(
                "[{now}] {} | {} | {}",
                value_string(details, "transition").unwrap_or("observed"),
                value_string(details, "phase").unwrap_or("unknown"),
                value_string(details, "agent_id").unwrap_or("unknown-agent")
            )];
            parts.push(format!(
                "{} | {}",
                value_string(details, "status").unwrap_or("unknown"),
                value_string(details, "file_path").unwrap_or("unknown-file")
            ));
            if let Some(source) = value_string(details, "source_lineage") {
                parts.push(format!("source={source}"));
            }
            if let Some(retry_count) = value_u64(details, "retry_count") {
                if retry_count > 0 {
                    parts.push(format!("retry={retry_count}"));
                }
            }
            if let Some(outcome) = value_string(details, "outcome") {
                parts.push(format!("outcome={outcome}"));
            }
            if let Some(decision) = value_string(details, "decision") {
                parts.push(format!("decision={decision}"));
            }
            if let Some(result) = value_string(details, "verification_result") {
                parts.push(format!("verify={result}"));
            }
            if let Some(explanation) = value_string(details, "explanation") {
                parts.push(format!("why={explanation}"));
            }
            if let Some(touched_lines) = value_u64(details, "touched_lines") {
                parts.push(format!("lines={touched_lines}"));
            }
            if let Some(byte_delta) = value_i64(details, "byte_delta") {
                parts.push(format!("bytes={byte_delta:+}"));
            }
            parts.push(format!(
                "tokens={}",
                value_i64(details, "tokens_used").unwrap_or_default()
            ));
            parts.push(format!(
                "cost=${:.4}",
                value_f64(details, "cost_usd").unwrap_or_default()
            ));
            parts.join(" | ")
        }
        other => format!("[{now}] {other}"),
    }
}

fn emit_live_status_events(
    events: &[LiveStatusEvent],
    render_mode: LiveRenderMode,
    buffer: &mut VecDeque<String>,
) {
    for event in events {
        match render_mode {
            LiveRenderMode::Terminal => push_live_event_line(buffer, format_live_event_line(event)),
            LiveRenderMode::JsonStream => {
                match serde_json::to_string(&event_payload(event.event, event.details.clone())) {
                    Ok(line) => println!("{line}"),
                    Err(err) => eprintln!(
                        "failed to serialize live status event {}: {err}",
                        event.event
                    ),
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Command handlers
// ---------------------------------------------------------------------------

fn cmd_init() -> Result<()> {
    let mercury_dir = PathBuf::from(".mercury");
    if mercury_dir.exists() {
        println!("Mercury already initialized in this directory.");
        return Ok(());
    }

    std::fs::create_dir_all(&mercury_dir)?;

    // Write default config
    std::fs::write(mercury_dir.join("config.toml"), DEFAULT_CONFIG)?;

    // Create the SQLite database
    let _db = ThermalDb::open(&mercury_dir.join("thermal.db"))?;

    println!("Initialized Mercury in .mercury/");
    println!("  config: .mercury/config.toml");
    println!("  database: .mercury/thermal.db");
    println!("\nSet your API key: export INCEPTION_API_KEY=<your-key>");
    Ok(())
}

fn cmd_status(
    db: &ThermalDb,
    heatmap: bool,
    live: bool,
    interval_ms: u64,
    agents: bool,
    budget: bool,
) -> Result<()> {
    if live {
        return cmd_status_live(db, heatmap, agents, budget, interval_ms.max(250));
    }

    if heatmap || (!agents && !budget) {
        let aggregates = db.get_all_aggregates()?;
        if aggregates.is_empty() {
            println!("No thermal data yet. Run `mercury plan` to generate a heat map.");
        } else {
            let active = db.get_active_agents()?;
            let output = render_heatmap_to_string(&aggregates, &active);
            println!("{output}");
        }
    }

    if agents {
        let active = db.get_active_agents()?;
        if active.is_empty() {
            println!("No active agents.");
        } else {
            println!("\nActive Agents ({}):", active.len());
            for a in &active {
                println!(
                    "  {} | {} | {} | {}",
                    a.agent_id, a.file_path, a.status, a.started_at
                );
            }
        }
    }

    if budget {
        if let Some(state) = db.get_swarm_state()? {
            println!("\nBudget:");
            println!("  Total cost: ${:.4}", state.total_cost_usd);
            println!("  Total tokens: {}", state.total_tokens_used);
            println!("  Agents spawned: {}", state.total_agents_spawned);
            println!("  Temperature: {:.2}", state.global_temperature);
            println!("  Iteration: {}", state.iteration_count);
        } else {
            println!("No swarm session active.");
        }
    }

    Ok(())
}

fn cmd_status_live(
    db: &ThermalDb,
    heatmap: bool,
    agents: bool,
    budget: bool,
    interval_ms: u64,
) -> Result<()> {
    let render_mode = if std::io::stdout().is_terminal() {
        LiveRenderMode::Terminal
    } else {
        LiveRenderMode::JsonStream
    };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to initialize runtime for live status")?;

    runtime.block_on(async {
        let mut previous_snapshot: Option<LiveStatusSnapshot> = None;
        let mut event_buffer = VecDeque::new();

        loop {
            let aggregates = db.get_all_aggregates()?;
            let active = db.get_active_agents()?;
            let agent_logs = db.get_agent_logs()?;
            let state = db.get_swarm_state()?;
            let snapshot =
                build_live_status_snapshot(&aggregates, &active, &agent_logs, state.as_ref());
            let events = match previous_snapshot.as_ref() {
                Some(previous) => diff_live_status_events(previous, &snapshot),
                None => initial_live_status_events(&snapshot, render_mode, interval_ms),
            };
            emit_live_status_events(&events, render_mode, &mut event_buffer);
            previous_snapshot = Some(snapshot.clone());

            if render_mode == LiveRenderMode::Terminal {
                print!("\x1B[2J\x1B[H");
                println!(
                    "Mercury live status  |  {}  |  refresh={}ms  |  mode={}  |  ctrl-c to stop\n",
                    Utc::now().to_rfc3339(),
                    interval_ms,
                    render_mode.as_str()
                );

                if heatmap || (!agents && !budget) {
                    println!("{}", render_heatmap_to_string(&aggregates, &active));
                }

                if agents {
                    if active.is_empty() {
                        println!("\nActive Agents: 0");
                    } else {
                        println!("\nActive Agents ({}):", active.len());
                        for agent in active.iter().take(12) {
                            println!(
                                "  {} | {} | {} | {}",
                                agent.agent_id, agent.file_path, agent.status, agent.started_at
                            );
                        }
                        if active.len() > 12 {
                            println!("  ... {} more", active.len() - 12);
                        }
                    }
                }

                if budget {
                    println!("\nBudget:");
                    println!("  Total cost: ${:.4}", snapshot.runtime.total_cost_usd);
                    println!("  Total tokens: {}", snapshot.runtime.total_tokens_used);
                    println!(
                        "  Agents spawned: {}",
                        snapshot.runtime.total_agents_spawned
                    );
                    println!("  Active agents: {}", snapshot.runtime.active_agents);
                    println!("  Temperature: {:.2}", snapshot.runtime.global_temperature);
                    println!("  Iteration: {}", snapshot.runtime.iteration_count);
                    if state.is_none() {
                        println!("  Source: derived from persisted agent logs");
                    }
                }

                println!("\nLive Events (latest {}):", LIVE_EVENT_BUFFER_LIMIT);
                if event_buffer.is_empty() {
                    println!("  No candidate/runtime events observed yet.");
                } else {
                    for line in &event_buffer {
                        println!("  {line}");
                    }
                }
            }

            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    match render_mode {
                        LiveRenderMode::Terminal => println!("\nStopping live status."),
                        LiveRenderMode::JsonStream => {
                            let stop_event = LiveStatusEvent {
                                event: "live_stopped",
                                details: serde_json::json!({
                                    "mode": render_mode.as_str(),
                                    "reason": "ctrl_c",
                                    "interval_ms": interval_ms,
                                }),
                            };
                            emit_live_status_events(
                                &[stop_event],
                                render_mode,
                                &mut event_buffer,
                            );
                        }
                    }
                    break Ok(());
                }
                _ = tokio::time::sleep(Duration::from_millis(interval_ms)) => {}
            }
        }
    })
}

async fn cmd_ask(
    config: &MercuryConfig,
    _db: &ThermalDb,
    query: &str,
    effort: Option<api::ReasoningEffort>,
) -> Result<()> {
    let api_key = api::resolve_api_key(&config.api.api_key_env)?;

    let mut client = Mercury2Client::new(api_key)
        .with_base_url(config.api.mercury2_endpoint.clone())
        .with_retries(
            config.scheduler.retry_limit,
            config.scheduler.backoff_base_ms,
        );
    if let Some(e) = effort {
        client = client.with_reasoning_effort(e);
    }

    let repo_languages = config.repo_languages();
    let repo_context = match repo::build_repo_map_with_languages(".", &repo_languages) {
        Ok(map) => repo::format_repo_map(&map),
        Err(_) => "No repo map available.".to_string(),
    };

    let system = format!(
        "You are Mercury CLI's assistant. Answer questions about the codebase.\n\n{}",
        config.constitutional_prompt()
    );
    let user_msg = format!("Question: {query}\n\nRepository context:\n{repo_context}");

    let (response, usage) =
        mercury_cli::api::Mercury2Api::chat(&client, &system, &user_msg, 4096).await?;

    println!("{response}");
    println!("\n---");
    println!(
        "Tokens: {} | Cost: ${:.4}",
        usage.tokens_used, usage.cost_usd
    );

    Ok(())
}

async fn cmd_plan(
    config: &MercuryConfig,
    db: &ThermalDb,
    goal: &str,
    output: Option<&Path>,
) -> Result<()> {
    let api_key = api::resolve_api_key(&config.api.api_key_env)?;

    // Build repo map
    println!("Indexing repository...");
    let repo_languages = config.repo_languages();
    let repo_map = repo::build_repo_map_with_languages(".", &repo_languages)?;
    let repo_map_str = repo::format_repo_map(&repo_map);

    let client = Mercury2Client::new(api_key.clone())
        .with_base_url(config.api.mercury2_endpoint.clone())
        .with_retries(
            config.scheduler.retry_limit,
            config.scheduler.backoff_base_ms,
        );

    let planner = engine::Planner::new(client, config.constitutional_prompt());

    println!("Planning with Mercury 2...");
    let (plan, assessments) = planner.plan(goal, &repo_map_str, Path::new(".")).await?;

    store_assessments(db, &plan.steps, &assessments, "plan")?;

    // Run merge cycle
    let scheduler = Scheduler::new(config.to_scheduler_config());
    scheduler.run_merge_cycle(db, config.annealing.initial_temperature)?;

    // Display plan
    println!("\nExecution Plan ({} steps):", plan.steps.len());
    println!(
        "Estimated planning cost: ${:.4}{}",
        plan.estimated_cost,
        plan.estimated_tokens
            .map(|tokens| format!(" | tokens: {tokens}"))
            .unwrap_or_default()
    );
    for (i, step) in plan.steps.iter().enumerate() {
        println!(
            "  {}. {} (priority: {:.2})",
            i + 1,
            step.file_path,
            step.priority
        );
        println!("     {}", step.instruction);
    }
    print_assessment_summary(&plan.steps, &assessments);

    // Display heatmap
    let aggregates = db.get_all_aggregates()?;
    if !aggregates.is_empty() {
        let active = db.get_active_agents()?;
        println!("\n{}", render_heatmap_to_string(&aggregates, &active));
    }

    // Write JSON output if requested
    if let Some(output_path) = output {
        let json = serde_json::to_string_pretty(&plan)?;
        std::fs::write(output_path, json)?;
        println!("\nPlan written to {}", output_path.display());
    }

    Ok(())
}

#[derive(Debug, Clone)]
struct FixCommandOutcome {
    artifact_root: PathBuf,
    execution_summary: StepExecutionSummary,
    total_cost_usd: f64,
    budget_remaining_usd: f64,
}

#[derive(Debug, Clone, Serialize)]
struct WatchCommandResult {
    command: String,
    success: bool,
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    parsed_failure: Option<mercury_cli::failure_parser::ParsedFailureReport>,
}

#[derive(Debug, Clone, Serialize)]
struct WatchRepairRecord {
    supported: bool,
    verifier_command: Option<String>,
    fix_artifact_root: Option<String>,
    sandbox_run_root: Option<String>,
    accepted_steps: usize,
    rejected_steps: usize,
    verification_failures: usize,
    final_bundle_verified: bool,
    applied: bool,
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct WatchRunRecord {
    command: String,
    repair_requested: bool,
    decision: String,
    security: SecurityRuntimeContext,
    initial_run: WatchCommandResult,
    confirmation_run: Option<WatchCommandResult>,
    repair: Option<WatchRepairRecord>,
    started_at: String,
    finished_at: String,
    duration_ms: u64,
}

#[derive(Debug, Clone)]
struct WatchCycleOutcome {
    artifact_root: PathBuf,
    record: WatchRunRecord,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RustRepairMode {
    TestLike,
    Lint,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RustRepairTarget {
    verifier_command: String,
    mode: RustRepairMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RepoWatchSnapshot(BTreeMap<PathBuf, RepoFileFingerprint>);

#[derive(Debug, Clone, PartialEq, Eq)]
struct RepoFileFingerprint {
    modified_unix_nanos: u128,
    len: u64,
}

async fn cmd_fix(
    config: &MercuryConfig,
    db: &ThermalDb,
    project_root: &Path,
    description: &str,
    max_agents: usize,
    max_cost: f64,
    noninteractive: bool,
) -> Result<()> {
    let _ = cmd_fix_with_verify_config(
        config,
        db,
        project_root,
        FixCommandRequest {
            description,
            max_agents,
            max_cost,
            verify_config: build_verify_config(config),
            parsed_failure: None,
            noninteractive,
        },
    )
    .await?;
    Ok(())
}

struct FixCommandRequest<'a> {
    description: &'a str,
    max_agents: usize,
    max_cost: f64,
    verify_config: VerifyConfig,
    parsed_failure: Option<&'a failure_parser::ParsedFailureReport>,
    noninteractive: bool,
}

async fn cmd_fix_with_verify_config(
    config: &MercuryConfig,
    db: &ThermalDb,
    project_root: &Path,
    request: FixCommandRequest<'_>,
) -> Result<FixCommandOutcome> {
    let FixCommandRequest {
        description,
        max_agents,
        max_cost,
        verify_config,
        parsed_failure,
        noninteractive,
    } = request;
    let started_at = Utc::now();
    let started = Instant::now();
    let artifact_root = create_run_artifact_root(project_root)?;
    let description_redacted = redact_secrets(description);
    let start_security = security_runtime_context(noninteractive, None);
    let benchmark_verifier = FixBenchmarkVerifier {
        parse_before_write: verify_config.parse_before_write,
        test_after_write: verify_config.test_after_write,
        lint_after_write: verify_config.lint_after_write,
        test_command: redact_secrets(&verify_config.test_command),
        lint_command: redact_secrets(&verify_config.lint_command),
    };

    if let Err(err) = enforce_verifier_allowlist(&verify_config) {
        let error = redact_secrets(&err.to_string());
        let finished_at = Utc::now();
        let duration_ms = started.elapsed().as_millis() as u64;
        let _ = write_fix_failure_artifacts(
            &artifact_root,
            &description_redacted,
            max_agents,
            max_cost,
            started_at,
            finished_at,
            duration_ms,
            "rejected_allowlist",
            &benchmark_verifier,
            &start_security,
        );
        let _ = write_audit_event(
            &artifact_root,
            "fix_run_rejected_allowlist",
            serde_json::json!({
                "description": description_redacted,
                "error": error,
                "security": start_security.clone(),
            }),
        );
        emit_runtime_event(
            noninteractive,
            "fix_run_rejected_allowlist",
            serde_json::json!({
                "description": description_redacted,
                "artifact_root": artifact_root.display().to_string(),
                "error": error,
                "security": start_security.clone(),
            }),
        );
        return Err(err);
    }
    write_audit_event(
        &artifact_root,
        "fix_run_started",
        serde_json::json!({
            "description": description_redacted,
            "max_agents": max_agents,
            "max_cost": max_cost,
            "artifact_root": artifact_root.display().to_string(),
            "security": start_security.clone(),
        }),
    )?;
    emit_runtime_event(
        noninteractive,
        "fix_run_started",
        serde_json::json!({
            "description": description_redacted,
            "max_agents": max_agents,
            "max_cost": max_cost,
            "artifact_root": artifact_root.display().to_string(),
            "security": start_security.clone(),
        }),
    );

    if !noninteractive {
        println!("Mercury Fix: {}", description_redacted);
        println!("  Max agents: {max_agents}");
        println!("  Max cost: ${max_cost:.2}");
        println!("  Artifact bundle: {}", artifact_root.display());
        println!();
    }

    let run: Result<FixCommandOutcome> = async {
        // Step 1: Init swarm session
        let _swarm_id = db.init_swarm()?;

        // Step 2: Index
        emit_runtime_event(
            noninteractive,
            "fix_stage",
            serde_json::json!({
                "stage": "index_repository",
                "artifact_root": artifact_root.display().to_string(),
            }),
        );
        if !noninteractive {
            println!("[1/7] Indexing repository...");
        }
        let repo_languages = config.repo_languages();
        let repo_map =
            repo::build_repo_map_with_languages(&project_root.to_string_lossy(), &repo_languages)?;
        let repo_map_str = repo::format_repo_map(&repo_map);

        // Step 3: Plan
        emit_runtime_event(
            noninteractive,
            "fix_stage",
            serde_json::json!({
                "stage": "plan_repair",
                "artifact_root": artifact_root.display().to_string(),
            }),
        );
        if !noninteractive {
            println!("[2/7] Planning with Mercury 2...");
            println!("  Grounding repair context...");
        }
        let api_key = api::resolve_api_key(&config.api.api_key_env)?;
        let grounding_client = Mercury2Client::new(api_key.clone())
            .with_base_url(config.api.mercury2_endpoint.clone())
            .with_retries(
                config.scheduler.retry_limit,
                config.scheduler.backoff_base_ms,
            );
        let grounded_context = verification::gather_grounded_repair_context(
            &grounding_client,
            project_root,
            &verify_config,
            description,
            parsed_failure,
        )
        .await?;
        write_json_artifact(
            &artifact_root.join("grounded-context.json"),
            &grounded_context,
        )?;

        let planner_description = format!("{description}\n\n{}", grounded_context.planner_brief());

        let planner_client = Mercury2Client::new(api_key.clone())
            .with_base_url(config.api.mercury2_endpoint.clone())
            .with_retries(
                config.scheduler.retry_limit,
                config.scheduler.backoff_base_ms,
            );

        let planner = engine::Planner::new(planner_client, config.constitutional_prompt());
        let (plan, assessments) = planner
            .plan(&planner_description, &repo_map_str, project_root)
            .await?;
        if !noninteractive {
            println!(
                "  Plan estimate: ${:.4}{}",
                plan.estimated_cost,
                plan.estimated_tokens
                    .map(|tokens| format!(" | tokens: {tokens}"))
                    .unwrap_or_default()
            );
        }

        store_assessments(db, &plan.steps, &assessments, "fix")?;
        if !noninteractive {
            print_assessment_summary(&plan.steps, &assessments);
        }
        write_audit_event(
            &artifact_root,
            "fix_plan_ready",
            serde_json::json!({
                "plan_steps": plan.steps.len(),
                "estimated_cost_usd": plan.estimated_cost,
                "estimated_tokens": plan.estimated_tokens,
                "security": start_security.clone(),
            }),
        )?;
        emit_runtime_event(
            noninteractive,
            "fix_plan_ready",
            serde_json::json!({
                "plan_steps": plan.steps.len(),
                "estimated_cost_usd": plan.estimated_cost,
                "estimated_tokens": plan.estimated_tokens,
                "artifact_root": artifact_root.display().to_string(),
            }),
        );

        // Run initial merge
        let mut sched_config = config.to_scheduler_config();
        sched_config.max_cost_per_command = max_cost;
        sched_config.max_concurrency = max_agents;
        let scheduler = Scheduler::new(sched_config);
        scheduler.run_merge_cycle(db, config.annealing.initial_temperature)?;

        // Step 4: Scaffold (cool zones)
        emit_runtime_event(
            noninteractive,
            "fix_stage",
            serde_json::json!({
                "stage": "scaffold_cool_zones",
                "artifact_root": artifact_root.display().to_string(),
            }),
        );
        if !noninteractive {
            println!("[3/7] Scaffolding cool zones...");
        }
        let cool_zones = db.zones_below(config.thermal.cool_threshold)?;
        if !noninteractive {
            println!("  {} cool zone files identified", cool_zones.len());
        }

        // Step 5: Resolve (hot zones)
        emit_runtime_event(
            noninteractive,
            "fix_stage",
            serde_json::json!({
                "stage": "resolve_hot_zones",
                "artifact_root": artifact_root.display().to_string(),
            }),
        );
        if !noninteractive {
            println!("[4/7] Resolving hot zones...");
        }
        let hot_zones = db.zones_above(config.thermal.hot_threshold)?;
        if !noninteractive {
            println!("  {} hot zone files identified", hot_zones.len());
        }

        // Step 6: Execute planned edits with verification gating
        emit_runtime_event(
            noninteractive,
            "fix_stage",
            serde_json::json!({
                "stage": "patch_and_verify",
                "artifact_root": artifact_root.display().to_string(),
            }),
        );
        if !noninteractive {
            println!("[5/7] Patching and verifying plan steps...");
        }
        let edit_client =
            MercuryEditClient::new(api_key.clone(), config.api.mercury_edit_endpoint.clone())
                .with_retries(
                    config.scheduler.retry_limit,
                    config.scheduler.backoff_base_ms,
                );
        let patcher = engine::Patcher::new(edit_client);

        let verifier_client = Mercury2Client::new(api_key.clone())
            .with_base_url(config.api.mercury2_endpoint.clone())
            .with_retries(
                config.scheduler.retry_limit,
                config.scheduler.backoff_base_ms,
            );
        let verifier = Verifier::new(
            verify_config.clone(),
            if config.verification.mercury2_critique_on_failure {
                Some(verifier_client)
            } else {
                None
            },
        );
        let execution_summary: StepExecutionSummary = engine::execute_plan_steps(
            &plan,
            &patcher,
            &verifier,
            &scheduler,
            db,
            project_root,
            grounded_context.parsed_failure.as_ref(),
        )
        .await?;
        let final_security =
            security_runtime_context(noninteractive, execution_summary.run_root.as_deref());
        write_audit_event(
            &artifact_root,
            "fix_execution_complete",
            serde_json::json!({
                "accepted_steps": execution_summary.accepted,
                "rejected_steps": execution_summary.rejected,
                "verification_failures": execution_summary.verification_failures,
                "applied": execution_summary.applied,
                "final_bundle_verified": execution_summary.final_bundle_verified,
                "security": final_security.clone(),
            }),
        )?;
        emit_runtime_event(
            noninteractive,
            "fix_execution_complete",
            serde_json::json!({
                "accepted_steps": execution_summary.accepted,
                "rejected_steps": execution_summary.rejected,
                "verification_failures": execution_summary.verification_failures,
                "applied": execution_summary.applied,
                "final_bundle_verified": execution_summary.final_bundle_verified,
                "artifact_root": artifact_root.display().to_string(),
                "security": final_security.clone(),
            }),
        );

        // Step 7: Anneal + report
        emit_runtime_event(
            noninteractive,
            "fix_stage",
            serde_json::json!({
                "stage": "anneal_and_report",
                "artifact_root": artifact_root.display().to_string(),
            }),
        );
        if !noninteractive {
            println!("[6/7] Annealing...");
        }
        scheduler.run_decay_cycle(db, config.thermal.decay_half_life_seconds)?;
        scheduler.run_merge_cycle(db, config.annealing.initial_temperature)?;

        if !noninteractive {
            println!("[7/7] Complete!");
        }

        let aggregates = db.get_all_aggregates()?;
        if !noninteractive && !aggregates.is_empty() {
            let active = db.get_active_agents()?;
            println!("\nFinal Thermal Map:");
            println!("{}", render_heatmap_to_string(&aggregates, &active));
        }

        if !noninteractive {
            println!("\nExecution summary:");
            println!("  Accepted steps: {}", execution_summary.accepted);
            println!("  Rejected steps: {}", execution_summary.rejected);
            println!(
                "  Verification failures: {}",
                execution_summary.verification_failures
            );

            println!("\nCost: ${:.4}", scheduler.current_cost());
            println!("Budget remaining: ${:.4}", scheduler.budget_remaining());
        }

        let finished_at = Utc::now();
        let duration_ms = started.elapsed().as_millis() as u64;
        write_json_artifact(&artifact_root.join("plan.json"), &plan)?;
        write_json_artifact(&artifact_root.join("assessments.json"), &assessments)?;
        write_json_artifact(
            &artifact_root.join("execution-summary.json"),
            &execution_summary,
        )?;
        if let Some(final_verification) = execution_summary.final_verification.as_ref() {
            write_json_artifact(
                &artifact_root.join("final-verification.json"),
                final_verification,
            )?;
        }
        let agent_logs = db.get_agent_logs()?;
        write_json_artifact(&artifact_root.join("agent-logs.json"), &agent_logs)?;
        write_json_artifact(&artifact_root.join("thermal-aggregates.json"), &aggregates)?;
        let swarm_state = db.get_swarm_state()?;
        if let Some(state) = swarm_state.as_ref() {
            write_json_artifact(&artifact_root.join("swarm-state.json"), &state)?;
        }
        let grounding_tool_calls = grounded_context
            .rounds
            .iter()
            .map(|round| round.tool_calls.len())
            .sum::<usize>();
        let total_cost_usd =
            scheduler.current_cost() + grounded_context.total_usage.cost_usd + plan.estimated_cost;
        let budget_remaining_usd = (max_cost - total_cost_usd).max(0.0);
        let sandbox_run_root = execution_summary
            .run_root
            .as_ref()
            .map(|path| path.display().to_string());
        let metadata = FixRunMetadata {
            description: description_redacted.clone(),
            max_agents,
            max_cost,
            started_at: started_at.to_rfc3339(),
            finished_at: finished_at.to_rfc3339(),
            duration_ms,
            planner_schema_version: mercury_cli::api::PLANNER_RESPONSE_SCHEMA_NAME.to_string(),
            grounding_schema_version: verification::GROUNDED_REPAIR_CONTEXT_SCHEMA_NAME.to_string(),
            grounding_rounds: grounded_context.rounds.len(),
            grounding_tool_calls,
            grounding_collected: !grounded_context.summary.trim().is_empty()
                || !grounded_context.rounds.is_empty(),
            grounding_cost_usd: grounded_context.total_usage.cost_usd,
            planner_estimated_cost_usd: plan.estimated_cost,
            final_bundle_verified: execution_summary.final_bundle_verified,
            applied: execution_summary.applied,
            sandbox_run_root: sandbox_run_root.clone(),
            total_cost_usd,
            budget_remaining_usd,
            security: final_security.clone(),
        };
        write_json_artifact(&artifact_root.join("metadata.json"), &metadata)?;
        let diff_patch_path = artifact_root.join("diff.patch");
        if let Some(run_root) = execution_summary.run_root.as_ref() {
            copy_if_exists(&run_root.join("accepted.patch"), &diff_patch_path)?;
        }
        let accepted_patch_bytes = patch_size_bytes(&diff_patch_path)?;
        let accepted_patch = accepted_patch_bytes.is_some_and(|len| len > 0);
        let benchmark_run = FixBenchmarkRun {
            schema_version: FIX_BENCHMARK_RUN_SCHEMA_NAME.to_string(),
            description: description_redacted.clone(),
            started_at: started_at.to_rfc3339(),
            finished_at: finished_at.to_rfc3339(),
            duration_ms,
            accepted_steps: execution_summary.accepted,
            rejected_steps: execution_summary.rejected,
            verification_failures: execution_summary.verification_failures,
            generation_failures: execution_summary.generation_failures,
            safety_failures: execution_summary.safety_failures,
            candidate_verification_failures: execution_summary.candidate_verification_failures,
            final_bundle_failures: execution_summary.final_bundle_failures,
            apply_edit_attempts: execution_summary.apply_edit_attempts,
            grounded_next_edit_attempts: execution_summary.grounded_next_edit_attempts,
            critique_retry_attempts: execution_summary.critique_retry_attempts,
            exploratory_next_edit_attempts: execution_summary.exploratory_next_edit_attempts,
            apply_edit_accepted_steps: execution_summary.apply_edit_accepted_steps,
            grounded_next_edit_accepted_steps: execution_summary.grounded_next_edit_accepted_steps,
            critique_retry_accepted_steps: execution_summary.critique_retry_accepted_steps,
            exploratory_next_edit_accepted_steps: execution_summary
                .exploratory_next_edit_accepted_steps,
            retry_attempts: execution_summary.retry_attempts,
            time_to_first_candidate_ms: execution_summary.time_to_first_candidate_ms,
            time_to_verified_repair_ms: execution_summary.time_to_verified_repair_ms,
            final_bundle_verified: execution_summary.final_bundle_verified,
            applied: execution_summary.applied,
            accepted_patch,
            accepted_patch_bytes,
            outcome: classify_fix_benchmark_outcome(
                execution_summary.final_bundle_verified,
                accepted_patch,
            )
            .to_string(),
            failure_attribution: classify_fix_failure_attribution(
                &execution_summary,
                accepted_patch,
            )
            .map(str::to_string),
            // False-green requires an independent rerun outside `fix`; the
            // benchmark harness derives it later instead of guessing here.
            false_green: None,
            sandbox_run_root: sandbox_run_root.clone(),
            total_cost_usd,
            budget_remaining_usd,
            verifier: benchmark_verifier.clone(),
            security: final_security.clone(),
        };
        write_json_artifact(&artifact_root.join("benchmark-run.json"), &benchmark_run)?;

        let mut summary_artifacts = BTreeMap::new();
        summary_artifacts.insert(
            "grounded_context".to_string(),
            "grounded-context.json".to_string(),
        );
        summary_artifacts.insert("plan".to_string(), "plan.json".to_string());
        summary_artifacts.insert("assessments".to_string(), "assessments.json".to_string());
        summary_artifacts.insert(
            "execution_summary".to_string(),
            "execution-summary.json".to_string(),
        );
        summary_artifacts.insert("agent_logs".to_string(), "agent-logs.json".to_string());
        summary_artifacts.insert(
            "thermal_aggregates".to_string(),
            "thermal-aggregates.json".to_string(),
        );
        summary_artifacts.insert("metadata".to_string(), "metadata.json".to_string());
        summary_artifacts.insert(
            "benchmark_run".to_string(),
            "benchmark-run.json".to_string(),
        );
        summary_artifacts.insert("audit_log".to_string(), "audit.log".to_string());
        if execution_summary.final_verification.is_some() {
            summary_artifacts.insert(
                "final_verification".to_string(),
                "final-verification.json".to_string(),
            );
        }
        if swarm_state.is_some() {
            summary_artifacts.insert("swarm_state".to_string(), "swarm-state.json".to_string());
        }
        if accepted_patch_bytes.is_some() {
            summary_artifacts.insert("diff_patch".to_string(), "diff.patch".to_string());
        }
        write_fix_summary_index(
            &artifact_root,
            &metadata,
            &benchmark_run,
            &agent_logs,
            summary_artifacts,
        )?;
        write_audit_event(
            &artifact_root,
            "fix_run_completed",
            serde_json::json!({
                "duration_ms": duration_ms,
                "total_cost_usd": total_cost_usd,
                "budget_remaining_usd": budget_remaining_usd,
                "artifact_root": artifact_root.display().to_string(),
                "security": final_security.clone(),
            }),
        )?;
        emit_runtime_event(
            noninteractive,
            "fix_run_completed",
            serde_json::json!({
                "duration_ms": duration_ms,
                "total_cost_usd": total_cost_usd,
                "budget_remaining_usd": budget_remaining_usd,
                "artifact_root": artifact_root.display().to_string(),
                "security": final_security,
            }),
        );
        if !noninteractive {
            println!("Artifacts written to {}", artifact_root.display());
        }

        Ok(FixCommandOutcome {
            artifact_root: artifact_root.clone(),
            execution_summary,
            total_cost_usd,
            budget_remaining_usd,
        })
    }
    .await;

    match run {
        Ok(outcome) => Ok(outcome),
        Err(err) => {
            let finished_at = Utc::now();
            let duration_ms = started.elapsed().as_millis() as u64;
            let error = redact_secrets(&err.to_string());
            let _ = write_fix_failure_artifacts(
                &artifact_root,
                &description_redacted,
                max_agents,
                max_cost,
                started_at,
                finished_at,
                duration_ms,
                "fix_failed",
                &benchmark_verifier,
                &start_security,
            );
            let _ = write_audit_event(
                &artifact_root,
                "fix_run_failed",
                serde_json::json!({
                    "description": description_redacted,
                    "error": error,
                    "artifact_root": artifact_root.display().to_string(),
                    "security": start_security.clone(),
                }),
            );
            emit_runtime_event(
                noninteractive,
                "fix_run_failed",
                serde_json::json!({
                    "description": description_redacted,
                    "error": error,
                    "artifact_root": artifact_root.display().to_string(),
                    "security": start_security.clone(),
                }),
            );
            Err(err)
        }
    }
}

async fn cmd_watch(
    config: &MercuryConfig,
    db: &ThermalDb,
    project_root: &Path,
    command: &str,
    repair: bool,
    noninteractive: bool,
) -> Result<()> {
    let watch_parts = verification::parse_allowlisted_verifier_parts(command)
        .map_err(|err| anyhow::anyhow!("watch command rejected: {err}"))?;
    let normalized_watch_command = watch_parts.join(" ");
    let watch_command = redact_secrets(&normalized_watch_command);
    let continuous_mode = !noninteractive;
    emit_runtime_event(
        noninteractive,
        "watch_started",
        serde_json::json!({
            "command": watch_command,
            "continuous": continuous_mode,
            "repair_requested": repair,
            "repair_supported": classify_rust_repair_command(&normalized_watch_command).is_some(),
            "security": security_runtime_context(noninteractive, None),
        }),
    );
    if !noninteractive {
        println!("Watching: {watch_command}");
        println!("Execution mode: direct allowlisted verifier command (no shell).");
        if repair {
            println!("Auto-repair enabled for supported Rust verifier commands.");
        } else {
            println!("Auto-repair disabled. Failures will be reported only.");
        }
        println!("Press Ctrl-C to stop.\n");
    }

    let mut snapshot = capture_repo_watch_snapshot(project_root)?;
    let mut cycle = 0usize;

    loop {
        cycle += 1;
        if cycle > 1 {
            emit_runtime_event(
                noninteractive,
                "watch_waiting_for_changes",
                serde_json::json!({
                    "command": watch_command,
                    "cycle": cycle,
                }),
            );
            if !noninteractive {
                println!("Waiting for repository changes...");
            }
            if !wait_for_repo_change(project_root, &mut snapshot).await? {
                return Ok(());
            }
            emit_runtime_event(
                noninteractive,
                "watch_change_detected",
                serde_json::json!({
                    "command": watch_command,
                    "cycle": cycle,
                }),
            );
            if !noninteractive {
                println!("Change detected. Re-running watch cycle.\n");
            }
        }

        let outcome = execute_watch_cycle(
            config,
            db,
            project_root,
            &watch_parts,
            repair,
            noninteractive,
        )
        .await?;
        print_watch_cycle_result(&outcome, noninteractive);

        if !continuous_mode {
            let success = watch_decision_succeeded(&outcome.record.decision);
            emit_runtime_event(
                noninteractive,
                "watch_completed",
                serde_json::json!({
                    "command": watch_command,
                    "continuous": false,
                    "cycles": cycle,
                    "decision": outcome.record.decision.clone(),
                    "artifact_root": outcome.artifact_root.display().to_string(),
                    "success": success,
                }),
            );
            if success {
                return Ok(());
            }
            anyhow::bail!(
                "watch cycle failed: decision={} artifact={}",
                outcome.record.decision,
                outcome.artifact_root.display()
            );
        }

        snapshot = capture_repo_watch_snapshot(project_root)?;
        if !noninteractive {
            println!();
        }
    }
}

async fn execute_watch_cycle(
    config: &MercuryConfig,
    db: &ThermalDb,
    project_root: &Path,
    command_parts: &[String],
    repair: bool,
    noninteractive: bool,
) -> Result<WatchCycleOutcome> {
    let started_at = Utc::now();
    let started = Instant::now();
    let artifact_root = create_run_artifact_root(project_root)?;
    let command_display = command_parts.join(" ");
    let command_display_redacted = redact_secrets(&command_display);
    let watch_security = security_runtime_context(noninteractive, None);

    emit_runtime_event(
        noninteractive,
        "watch_cycle_started",
        serde_json::json!({
            "command": command_display_redacted,
            "artifact_root": artifact_root.display().to_string(),
            "repair_requested": repair,
            "security": watch_security.clone(),
        }),
    );
    if !noninteractive {
        println!("Running watch command...");
    }
    let initial_run = run_watch_command(command_parts, project_root)?;
    replay_watch_command_output("initial", &initial_run, noninteractive);
    write_audit_event(
        &artifact_root,
        "watch_cycle_initial_run",
        serde_json::json!({
            "command": initial_run.command,
            "success": initial_run.success,
            "exit_code": initial_run.exit_code,
            "repair_requested": repair,
            "parsed_failure": initial_run.parsed_failure,
            "security": watch_security.clone(),
        }),
    )?;
    emit_runtime_event(
        noninteractive,
        "watch_cycle_initial_run",
        serde_json::json!({
            "command": initial_run.command,
            "success": initial_run.success,
            "exit_code": initial_run.exit_code,
            "repair_requested": repair,
            "artifact_root": artifact_root.display().to_string(),
            "parsed_failure": initial_run.parsed_failure,
        }),
    );

    let mut decision = "passed_without_repair".to_string();
    let mut repair_record = None;
    let mut confirmation_run = None;

    if !initial_run.success {
        if !repair {
            decision = "failed_without_repair".to_string();
        } else if let Some(target) = classify_rust_repair_command(&command_display) {
            let verify_config = build_watch_verify_config(config, &target);
            let description = build_watch_repair_description(&command_display, &initial_run);
            emit_runtime_event(
                noninteractive,
                "watch_cycle_repair_started",
                serde_json::json!({
                    "command": command_display_redacted,
                    "target_verifier_command": redact_secrets(&target.verifier_command),
                    "artifact_root": artifact_root.display().to_string(),
                }),
            );
            if !noninteractive {
                println!(
                    "Watch repair targeting `{}` with Mercury fix...",
                    target.verifier_command
                );
            }
            match cmd_fix_with_verify_config(
                config,
                db,
                project_root,
                FixCommandRequest {
                    description: &description,
                    max_agents: config.scheduler.max_concurrency,
                    max_cost: config.scheduler.max_cost_per_command,
                    verify_config,
                    parsed_failure: initial_run.parsed_failure.as_ref(),
                    noninteractive,
                },
            )
            .await
            {
                Ok(outcome) => {
                    repair_record = Some(WatchRepairRecord {
                        supported: true,
                        verifier_command: Some(redact_secrets(&target.verifier_command)),
                        fix_artifact_root: Some(outcome.artifact_root.display().to_string()),
                        sandbox_run_root: outcome
                            .execution_summary
                            .run_root
                            .as_ref()
                            .map(|path| path.display().to_string()),
                        accepted_steps: outcome.execution_summary.accepted,
                        rejected_steps: outcome.execution_summary.rejected,
                        verification_failures: outcome.execution_summary.verification_failures,
                        final_bundle_verified: outcome.execution_summary.final_bundle_verified,
                        applied: outcome.execution_summary.applied,
                        error: None,
                    });
                    copy_if_exists(
                        &outcome.artifact_root.join("diff.patch"),
                        &artifact_root.join("repair").join("diff.patch"),
                    )?;
                    copy_if_exists(
                        &outcome.artifact_root.join("execution-summary.json"),
                        &artifact_root.join("repair").join("execution-summary.json"),
                    )?;
                    copy_if_exists(
                        &outcome.artifact_root.join("final-verification.json"),
                        &artifact_root.join("repair").join("final-verification.json"),
                    )?;
                    copy_if_exists(
                        &outcome.artifact_root.join("metadata.json"),
                        &artifact_root.join("repair").join("metadata.json"),
                    )?;
                    copy_if_exists(
                        &outcome.artifact_root.join("benchmark-run.json"),
                        &artifact_root.join("repair").join("benchmark-run.json"),
                    )?;
                    copy_if_exists(
                        &outcome.artifact_root.join("summary-index.json"),
                        &artifact_root.join("repair").join("summary-index.json"),
                    )?;
                    copy_if_exists(
                        &outcome.artifact_root.join("plan.json"),
                        &artifact_root.join("repair").join("plan.json"),
                    )?;
                    copy_if_exists(
                        &outcome.artifact_root.join("grounded-context.json"),
                        &artifact_root.join("repair").join("grounded-context.json"),
                    )?;

                    let repair_security = security_runtime_context(
                        noninteractive,
                        outcome.execution_summary.run_root.as_deref(),
                    );
                    write_audit_event(
                        &artifact_root,
                        "watch_cycle_repair_completed",
                        serde_json::json!({
                            "target_verifier_command": redact_secrets(&target.verifier_command),
                            "fix_artifact_root": outcome.artifact_root.display().to_string(),
                            "sandbox_run_root": outcome
                                .execution_summary
                                .run_root
                                .as_ref()
                                .map(|path| path.display().to_string()),
                            "accepted_steps": outcome.execution_summary.accepted,
                            "rejected_steps": outcome.execution_summary.rejected,
                            "verification_failures": outcome.execution_summary.verification_failures,
                            "applied": outcome.execution_summary.applied,
                            "final_bundle_verified": outcome.execution_summary.final_bundle_verified,
                            "security": repair_security.clone(),
                        }),
                    )?;
                    emit_runtime_event(
                        noninteractive,
                        "watch_cycle_repair_completed",
                        serde_json::json!({
                            "target_verifier_command": redact_secrets(&target.verifier_command),
                            "fix_artifact_root": outcome.artifact_root.display().to_string(),
                            "sandbox_run_root": outcome
                                .execution_summary
                                .run_root
                                .as_ref()
                                .map(|path| path.display().to_string()),
                            "accepted_steps": outcome.execution_summary.accepted,
                            "rejected_steps": outcome.execution_summary.rejected,
                            "verification_failures": outcome.execution_summary.verification_failures,
                            "applied": outcome.execution_summary.applied,
                            "final_bundle_verified": outcome.execution_summary.final_bundle_verified,
                            "artifact_root": artifact_root.display().to_string(),
                            "security": repair_security,
                        }),
                    );

                    if !noninteractive {
                        println!(
                            "Repair attempt cost: ${:.4} | budget remaining: ${:.4}",
                            outcome.total_cost_usd, outcome.budget_remaining_usd
                        );
                        println!("Re-running watch command after repair...");
                    }
                    write_audit_event(
                        &artifact_root,
                        "watch_cycle_confirmation_run",
                        serde_json::json!({
                            "command": command_display_redacted,
                            "artifact_root": artifact_root.display().to_string(),
                        }),
                    )?;
                    emit_runtime_event(
                        noninteractive,
                        "watch_cycle_confirmation_run",
                        serde_json::json!({
                            "command": command_display_redacted,
                            "artifact_root": artifact_root.display().to_string(),
                        }),
                    );
                    let rerun = run_watch_command(command_parts, project_root)?;
                    replay_watch_command_output("confirmation", &rerun, noninteractive);
                    let rerun_success = rerun.success;
                    confirmation_run = Some(rerun);
                    decision = if rerun_success {
                        "repaired_and_verified".to_string()
                    } else if outcome.execution_summary.applied {
                        "repair_applied_but_command_still_failing".to_string()
                    } else {
                        "repair_not_applied".to_string()
                    };
                }
                Err(err) => {
                    repair_record = Some(WatchRepairRecord {
                        supported: true,
                        verifier_command: Some(redact_secrets(&target.verifier_command)),
                        fix_artifact_root: None,
                        sandbox_run_root: None,
                        accepted_steps: 0,
                        rejected_steps: 0,
                        verification_failures: 0,
                        final_bundle_verified: false,
                        applied: false,
                        error: Some(redact_secrets(&err.to_string())),
                    });
                    decision = "repair_flow_failed".to_string();
                }
            }
        } else {
            repair_record = Some(WatchRepairRecord {
                supported: false,
                verifier_command: None,
                fix_artifact_root: None,
                sandbox_run_root: None,
                accepted_steps: 0,
                rejected_steps: 0,
                verification_failures: 0,
                final_bundle_verified: false,
                applied: false,
                error: Some(
                    "auto-repair currently supports direct Rust verifier commands: cargo test, cargo check, cargo clippy"
                        .to_string(),
                ),
            });
            decision = "repair_not_supported".to_string();
        }
    }

    let finished_at = Utc::now();
    let record = WatchRunRecord {
        command: command_display_redacted,
        repair_requested: repair,
        decision,
        security: watch_security.clone(),
        initial_run,
        confirmation_run,
        repair: repair_record,
        started_at: started_at.to_rfc3339(),
        finished_at: finished_at.to_rfc3339(),
        duration_ms: started.elapsed().as_millis() as u64,
    };

    write_watch_output_artifacts(&artifact_root, &record)?;
    write_json_artifact(&artifact_root.join("watch.json"), &record)?;
    write_watch_summary_index(&artifact_root, &record)?;
    write_audit_event(
        &artifact_root,
        "watch_cycle_completed",
        serde_json::json!({
            "decision": record.decision.clone(),
            "duration_ms": record.duration_ms,
            "repair_requested": record.repair_requested,
            "security": watch_security,
        }),
    )?;
    emit_runtime_event(
        noninteractive,
        "watch_cycle_completed",
        serde_json::json!({
            "decision": record.decision.clone(),
            "artifact_root": artifact_root.display().to_string(),
            "duration_ms": record.duration_ms,
            "repair_requested": record.repair_requested,
            "security": record.security.clone(),
        }),
    );

    Ok(WatchCycleOutcome {
        artifact_root,
        record,
    })
}

fn store_assessments(
    db: &ThermalDb,
    steps: &[engine::PlanStep],
    assessments: &[api::ThermalAssessment],
    command: &str,
) -> Result<()> {
    for (i, assessment) in assessments.iter().enumerate() {
        let file_path = steps
            .get(i)
            .map(|s| s.file_path.as_str())
            .unwrap_or("unknown");
        db.upsert_thermal_score(
            file_path,
            1,
            1000,
            assessment.complexity_score,
            "complexity",
            command,
            "planner",
        )?;
        db.upsert_thermal_score(
            file_path,
            1,
            1000,
            assessment.dependency_score,
            "dependency",
            command,
            "planner",
        )?;
        db.upsert_thermal_score(
            file_path,
            1,
            1000,
            assessment.risk_score,
            "risk",
            command,
            "planner",
        )?;
        db.upsert_thermal_score(
            file_path,
            1,
            1000,
            assessment.churn_score,
            "churn",
            command,
            "planner",
        )?;
    }
    Ok(())
}

fn print_assessment_summary(steps: &[engine::PlanStep], assessments: &[api::ThermalAssessment]) {
    if assessments.is_empty() {
        return;
    }

    println!("\nAssessment contributions (complexity/dependency/risk/churn):");
    for (i, assessment) in assessments.iter().enumerate() {
        let file_path = steps
            .get(i)
            .map(|s| s.file_path.as_str())
            .unwrap_or("unknown");
        let overall = (assessment.complexity_score
            + assessment.dependency_score
            + assessment.risk_score
            + assessment.churn_score)
            / 4.0;
        println!(
            "  - {} => c:{:.2} d:{:.2} r:{:.2} h:{:.2} avg:{:.2}",
            file_path,
            assessment.complexity_score,
            assessment.dependency_score,
            assessment.risk_score,
            assessment.churn_score,
            overall
        );
    }
}

#[derive(Clone, Debug, Serialize)]
struct SecurityRuntimeContext {
    ci: bool,
    noninteractive: bool,
    verifier_allowlist_override_requested: bool,
    verifier_allowlist_override_applied: bool,
    verifier_allowlist_enforced: bool,
    secret_redaction_enabled: bool,
    sandbox_policy: String,
    execution_sandbox_enforced: bool,
    sandbox_run_root: Option<String>,
}

fn security_runtime_context(
    noninteractive: bool,
    sandbox_run_root: Option<&Path>,
) -> SecurityRuntimeContext {
    let override_requested = allowlist_override_active();
    let ci = running_in_ci();
    SecurityRuntimeContext {
        ci,
        noninteractive,
        verifier_allowlist_override_requested: override_requested,
        verifier_allowlist_override_applied: override_requested && !ci,
        verifier_allowlist_enforced: !override_requested || ci,
        secret_redaction_enabled: true,
        sandbox_policy: SANDBOX_POLICY.to_string(),
        execution_sandbox_enforced: EXECUTION_SANDBOX_ENFORCED,
        sandbox_run_root: sandbox_run_root.map(|path| path.display().to_string()),
    }
}

const FIX_BENCHMARK_RUN_SCHEMA_NAME: &str = "mercury-repair-benchmark-case-v1";

#[derive(Debug, Serialize)]
struct FixRunMetadata {
    description: String,
    max_agents: usize,
    max_cost: f64,
    started_at: String,
    finished_at: String,
    duration_ms: u64,
    planner_schema_version: String,
    grounding_schema_version: String,
    grounding_rounds: usize,
    grounding_tool_calls: usize,
    grounding_collected: bool,
    grounding_cost_usd: f64,
    planner_estimated_cost_usd: f64,
    final_bundle_verified: bool,
    applied: bool,
    sandbox_run_root: Option<String>,
    total_cost_usd: f64,
    budget_remaining_usd: f64,
    security: SecurityRuntimeContext,
}

#[derive(Debug, Clone, Serialize)]
struct FixBenchmarkVerifier {
    parse_before_write: bool,
    test_after_write: bool,
    lint_after_write: bool,
    test_command: String,
    lint_command: String,
}

#[derive(Debug, Serialize)]
struct FixBenchmarkRun {
    schema_version: String,
    description: String,
    started_at: String,
    finished_at: String,
    duration_ms: u64,
    accepted_steps: usize,
    rejected_steps: usize,
    verification_failures: usize,
    generation_failures: usize,
    safety_failures: usize,
    candidate_verification_failures: usize,
    final_bundle_failures: usize,
    apply_edit_attempts: usize,
    grounded_next_edit_attempts: usize,
    critique_retry_attempts: usize,
    exploratory_next_edit_attempts: usize,
    apply_edit_accepted_steps: usize,
    grounded_next_edit_accepted_steps: usize,
    critique_retry_accepted_steps: usize,
    exploratory_next_edit_accepted_steps: usize,
    retry_attempts: usize,
    time_to_first_candidate_ms: Option<u64>,
    time_to_verified_repair_ms: Option<u64>,
    final_bundle_verified: bool,
    applied: bool,
    accepted_patch: bool,
    accepted_patch_bytes: Option<u64>,
    outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    failure_attribution: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    false_green: Option<bool>,
    sandbox_run_root: Option<String>,
    total_cost_usd: f64,
    budget_remaining_usd: f64,
    verifier: FixBenchmarkVerifier,
    security: SecurityRuntimeContext,
}

#[derive(Debug, Serialize)]
struct ArtifactCountSummary {
    accepted_steps: usize,
    rejected_steps: usize,
    verification_failures: usize,
    generation_failures: usize,
    safety_failures: usize,
    candidate_verification_failures: usize,
    final_bundle_failures: usize,
    retry_attempts: usize,
}

#[derive(Debug, Serialize)]
struct ArtifactCandidateLineageEntry {
    attempts: usize,
    accepted_steps: usize,
}

#[derive(Debug, Serialize)]
struct ArtifactCandidateLineageSummary {
    apply_edit: ArtifactCandidateLineageEntry,
    grounded_next_edit: ArtifactCandidateLineageEntry,
    critique_retry: ArtifactCandidateLineageEntry,
    exploratory_next_edit: ArtifactCandidateLineageEntry,
}

#[derive(Debug, Serialize)]
struct ArtifactWinningCandidateSummary {
    agent_id: String,
    file_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    phase: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_lineage: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    retry_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    touched_lines: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    byte_delta: Option<i64>,
}

#[derive(Debug, Serialize)]
struct FixArtifactSummaryIndex {
    run_kind: &'static str,
    artifact_root: String,
    headline: String,
    outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    failure_reason_rollup: Option<String>,
    description: String,
    duration_ms: u64,
    applied: bool,
    final_bundle_verified: bool,
    accepted_patch: bool,
    accepted_patch_bytes: Option<u64>,
    sandbox_run_root: Option<String>,
    counts: ArtifactCountSummary,
    candidate_lineage: ArtifactCandidateLineageSummary,
    time_to_first_candidate_ms: Option<u64>,
    time_to_verified_repair_ms: Option<u64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    winning_candidates: Vec<ArtifactWinningCandidateSummary>,
    artifacts: BTreeMap<String, String>,
}

#[derive(Debug, Serialize)]
struct WatchCommandPhaseSummary {
    success: bool,
    exit_code: Option<i32>,
    parsed_failure: bool,
}

#[derive(Debug, Serialize)]
struct WatchRepairSummaryIndex {
    headline: String,
    supported: bool,
    verifier_command: Option<String>,
    fix_artifact_root: Option<String>,
    sandbox_run_root: Option<String>,
    accepted_steps: usize,
    rejected_steps: usize,
    verification_failures: usize,
    final_bundle_verified: bool,
    applied: bool,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct WatchArtifactSummaryIndex {
    run_kind: &'static str,
    artifact_root: String,
    headline: String,
    decision: String,
    command: String,
    repair_requested: bool,
    duration_ms: u64,
    initial_run: WatchCommandPhaseSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    confirmation_run: Option<WatchCommandPhaseSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repair: Option<WatchRepairSummaryIndex>,
    artifacts: BTreeMap<String, String>,
}

fn artifact_count_summary(benchmark: &FixBenchmarkRun) -> ArtifactCountSummary {
    ArtifactCountSummary {
        accepted_steps: benchmark.accepted_steps,
        rejected_steps: benchmark.rejected_steps,
        verification_failures: benchmark.verification_failures,
        generation_failures: benchmark.generation_failures,
        safety_failures: benchmark.safety_failures,
        candidate_verification_failures: benchmark.candidate_verification_failures,
        final_bundle_failures: benchmark.final_bundle_failures,
        retry_attempts: benchmark.retry_attempts,
    }
}

fn artifact_candidate_lineage_summary(
    benchmark: &FixBenchmarkRun,
) -> ArtifactCandidateLineageSummary {
    ArtifactCandidateLineageSummary {
        apply_edit: ArtifactCandidateLineageEntry {
            attempts: benchmark.apply_edit_attempts,
            accepted_steps: benchmark.apply_edit_accepted_steps,
        },
        grounded_next_edit: ArtifactCandidateLineageEntry {
            attempts: benchmark.grounded_next_edit_attempts,
            accepted_steps: benchmark.grounded_next_edit_accepted_steps,
        },
        critique_retry: ArtifactCandidateLineageEntry {
            attempts: benchmark.critique_retry_attempts,
            accepted_steps: benchmark.critique_retry_accepted_steps,
        },
        exploratory_next_edit: ArtifactCandidateLineageEntry {
            attempts: benchmark.exploratory_next_edit_attempts,
            accepted_steps: benchmark.exploratory_next_edit_accepted_steps,
        },
    }
}

fn artifact_winning_candidates(
    agent_logs: &[db::AgentLogEntry],
) -> Vec<ArtifactWinningCandidateSummary> {
    agent_logs
        .iter()
        .filter_map(|entry| {
            let metadata = parse_candidate_runtime_metadata(entry);
            (entry.status == "success" && metadata.outcome.as_deref() == Some("accepted"))
                .then_some(ArtifactWinningCandidateSummary {
                    agent_id: entry.agent_id.clone(),
                    file_path: entry.file_path.clone(),
                    phase: metadata.phase,
                    source_lineage: metadata.source_lineage,
                    reason: metadata.reason,
                    retry_count: metadata.retry_count,
                    touched_lines: metadata.touched_lines,
                    byte_delta: metadata.byte_delta,
                })
        })
        .collect()
}

fn fix_summary_headline(
    outcome: &str,
    final_bundle_verified: bool,
    accepted_patch: bool,
) -> &'static str {
    match outcome {
        "verified_repair" => "verified repair",
        "accepted_patch_unverified" => "accepted patch but final bundle failed",
        "no_patch" => "no accepted patch",
        "fix_failed" => "fix run failed before a repair was accepted",
        "rejected_allowlist" => "verifier command rejected by allowlist",
        _ if final_bundle_verified => "verified repair",
        _ if accepted_patch => "accepted patch but final bundle failed",
        _ => "no accepted patch",
    }
}

fn fix_failure_reason_rollup(failure: &str) -> String {
    match failure {
        "final_bundle_verification_failed" => "final bundle verification failed".to_string(),
        "candidate_failed_verifier" => "candidate verification failed".to_string(),
        "candidate_failed_safety" => "candidate rejected by safety policy".to_string(),
        "patch_generation_failed" => "patch generation failed".to_string(),
        "no_patch_emitted" => "no patch was emitted".to_string(),
        "fix_failed" => "fix run failed before a verified repair was produced".to_string(),
        "rejected_allowlist" => "verifier command rejected by allowlist".to_string(),
        _ => failure.to_string(),
    }
}

fn write_fix_summary_index(
    artifact_root: &Path,
    metadata: &FixRunMetadata,
    benchmark: &FixBenchmarkRun,
    agent_logs: &[db::AgentLogEntry],
    artifacts: BTreeMap<String, String>,
) -> Result<()> {
    let summary = FixArtifactSummaryIndex {
        run_kind: "fix",
        artifact_root: artifact_root.display().to_string(),
        headline: fix_summary_headline(
            &benchmark.outcome,
            benchmark.final_bundle_verified,
            benchmark.accepted_patch,
        )
        .to_string(),
        outcome: benchmark.outcome.clone(),
        failure_reason_rollup: benchmark
            .failure_attribution
            .as_deref()
            .map(fix_failure_reason_rollup),
        description: metadata.description.clone(),
        duration_ms: benchmark.duration_ms,
        applied: benchmark.applied,
        final_bundle_verified: benchmark.final_bundle_verified,
        accepted_patch: benchmark.accepted_patch,
        accepted_patch_bytes: benchmark.accepted_patch_bytes,
        sandbox_run_root: benchmark.sandbox_run_root.clone(),
        counts: artifact_count_summary(benchmark),
        candidate_lineage: artifact_candidate_lineage_summary(benchmark),
        time_to_first_candidate_ms: benchmark.time_to_first_candidate_ms,
        time_to_verified_repair_ms: benchmark.time_to_verified_repair_ms,
        winning_candidates: artifact_winning_candidates(agent_logs),
        artifacts,
    };
    write_json_artifact(&artifact_root.join("summary-index.json"), &summary)
}

fn watch_summary_headline(decision: &str) -> &'static str {
    match decision {
        "passed_without_repair" => "watch command passed without repair",
        "failed_without_repair" => "watch command failed without repair",
        "repaired_and_verified" => "verified repair",
        "repair_applied_but_command_still_failing" => {
            "repair applied but watched command still failing"
        }
        "repair_not_applied" => "repair produced no applied patch",
        "repair_flow_failed" => "repair flow failed",
        "repair_not_supported" => "repair not supported for watched command",
        _ => "watch cycle completed",
    }
}

fn watch_phase_summary(result: &WatchCommandResult) -> WatchCommandPhaseSummary {
    WatchCommandPhaseSummary {
        success: result.success,
        exit_code: result.exit_code,
        parsed_failure: result.parsed_failure.is_some(),
    }
}

fn write_watch_summary_index(artifact_root: &Path, record: &WatchRunRecord) -> Result<()> {
    let mut artifacts = BTreeMap::new();
    artifacts.insert("watch_record".to_string(), "watch.json".to_string());
    artifacts.insert(
        "initial_stdout".to_string(),
        "initial.stdout.txt".to_string(),
    );
    artifacts.insert(
        "initial_stderr".to_string(),
        "initial.stderr.txt".to_string(),
    );
    artifacts.insert("audit_log".to_string(), "audit.log".to_string());
    if record.initial_run.parsed_failure.is_some() {
        artifacts.insert(
            "initial_failure".to_string(),
            "initial.failure.json".to_string(),
        );
    }
    if record.confirmation_run.is_some() {
        artifacts.insert(
            "confirmation_stdout".to_string(),
            "confirmation.stdout.txt".to_string(),
        );
        artifacts.insert(
            "confirmation_stderr".to_string(),
            "confirmation.stderr.txt".to_string(),
        );
    }
    if record
        .confirmation_run
        .as_ref()
        .and_then(|result| result.parsed_failure.as_ref())
        .is_some()
    {
        artifacts.insert(
            "confirmation_failure".to_string(),
            "confirmation.failure.json".to_string(),
        );
    }
    if let Some(repair) = record.repair.as_ref() {
        if repair.fix_artifact_root.is_some() {
            artifacts.insert("repair_bundle".to_string(), "repair".to_string());
            artifacts.insert(
                "repair_summary".to_string(),
                "repair/summary-index.json".to_string(),
            );
            artifacts.insert(
                "repair_execution_summary".to_string(),
                "repair/execution-summary.json".to_string(),
            );
            artifacts.insert(
                "repair_metadata".to_string(),
                "repair/metadata.json".to_string(),
            );
            artifacts.insert(
                "repair_benchmark_run".to_string(),
                "repair/benchmark-run.json".to_string(),
            );
            artifacts.insert("repair_plan".to_string(), "repair/plan.json".to_string());
        }
    }

    let summary = WatchArtifactSummaryIndex {
        run_kind: "watch",
        artifact_root: artifact_root.display().to_string(),
        headline: watch_summary_headline(&record.decision).to_string(),
        decision: record.decision.clone(),
        command: record.command.clone(),
        repair_requested: record.repair_requested,
        duration_ms: record.duration_ms,
        initial_run: watch_phase_summary(&record.initial_run),
        confirmation_run: record.confirmation_run.as_ref().map(watch_phase_summary),
        repair: record
            .repair
            .as_ref()
            .map(|repair| WatchRepairSummaryIndex {
                headline: watch_summary_headline(&record.decision).to_string(),
                supported: repair.supported,
                verifier_command: repair.verifier_command.clone(),
                fix_artifact_root: repair.fix_artifact_root.clone(),
                sandbox_run_root: repair.sandbox_run_root.clone(),
                accepted_steps: repair.accepted_steps,
                rejected_steps: repair.rejected_steps,
                verification_failures: repair.verification_failures,
                final_bundle_verified: repair.final_bundle_verified,
                applied: repair.applied,
                error: repair.error.clone(),
            }),
        artifacts,
    };
    write_json_artifact(&artifact_root.join("summary-index.json"), &summary)
}

#[allow(clippy::too_many_arguments)]
fn write_fix_failure_artifacts(
    artifact_root: &Path,
    description: &str,
    max_agents: usize,
    max_cost: f64,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    duration_ms: u64,
    outcome: &str,
    verifier: &FixBenchmarkVerifier,
    security: &SecurityRuntimeContext,
) -> Result<()> {
    let metadata = FixRunMetadata {
        description: description.to_string(),
        max_agents,
        max_cost,
        started_at: started_at.to_rfc3339(),
        finished_at: finished_at.to_rfc3339(),
        duration_ms,
        planner_schema_version: mercury_cli::api::PLANNER_RESPONSE_SCHEMA_NAME.to_string(),
        grounding_schema_version: verification::GROUNDED_REPAIR_CONTEXT_SCHEMA_NAME.to_string(),
        grounding_rounds: 0,
        grounding_tool_calls: 0,
        grounding_collected: false,
        grounding_cost_usd: 0.0,
        planner_estimated_cost_usd: 0.0,
        final_bundle_verified: false,
        applied: false,
        sandbox_run_root: None,
        total_cost_usd: 0.0,
        budget_remaining_usd: max_cost,
        security: security.clone(),
    };
    write_json_artifact(&artifact_root.join("metadata.json"), &metadata)?;

    let benchmark = FixBenchmarkRun {
        schema_version: FIX_BENCHMARK_RUN_SCHEMA_NAME.to_string(),
        description: description.to_string(),
        started_at: started_at.to_rfc3339(),
        finished_at: finished_at.to_rfc3339(),
        duration_ms,
        accepted_steps: 0,
        rejected_steps: 0,
        verification_failures: 0,
        generation_failures: 0,
        safety_failures: 0,
        candidate_verification_failures: 0,
        final_bundle_failures: 0,
        apply_edit_attempts: 0,
        grounded_next_edit_attempts: 0,
        critique_retry_attempts: 0,
        exploratory_next_edit_attempts: 0,
        apply_edit_accepted_steps: 0,
        grounded_next_edit_accepted_steps: 0,
        critique_retry_accepted_steps: 0,
        exploratory_next_edit_accepted_steps: 0,
        retry_attempts: 0,
        time_to_first_candidate_ms: None,
        time_to_verified_repair_ms: None,
        final_bundle_verified: false,
        applied: false,
        accepted_patch: false,
        accepted_patch_bytes: None,
        outcome: outcome.to_string(),
        failure_attribution: Some(outcome.to_string()),
        false_green: None,
        sandbox_run_root: None,
        total_cost_usd: 0.0,
        budget_remaining_usd: max_cost,
        verifier: FixBenchmarkVerifier {
            parse_before_write: verifier.parse_before_write,
            test_after_write: verifier.test_after_write,
            lint_after_write: verifier.lint_after_write,
            test_command: verifier.test_command.clone(),
            lint_command: verifier.lint_command.clone(),
        },
        security: security.clone(),
    };
    write_json_artifact(&artifact_root.join("benchmark-run.json"), &benchmark)?;

    let mut artifacts = BTreeMap::new();
    artifacts.insert("metadata".to_string(), "metadata.json".to_string());
    artifacts.insert(
        "benchmark_run".to_string(),
        "benchmark-run.json".to_string(),
    );
    artifacts.insert("audit_log".to_string(), "audit.log".to_string());
    write_fix_summary_index(artifact_root, &metadata, &benchmark, &[], artifacts)?;
    Ok(())
}

fn create_run_artifact_root(project_root: &Path) -> Result<PathBuf> {
    let run_id = format!("run-{}", Utc::now().format("%Y%m%dT%H%M%S%.3fZ"));
    let root = project_root.join(".mercury").join("runs").join(run_id);
    std::fs::create_dir_all(&root)?;
    Ok(root)
}

fn create_cli_worktree_root(project_root: &Path) -> Result<PathBuf> {
    let run_id = format!("edit-{}", Utc::now().format("%Y%m%dT%H%M%S%.3fZ"));
    Ok(project_root.join(".mercury").join("worktrees").join(run_id))
}

fn parse_cli_repo_relative_path(input: &str, project_root: &Path) -> Result<RepoRelativePath> {
    let relative_path = RepoRelativePath::new(input)
        .map_err(|err| anyhow!("invalid file path `{input}`: {err}"))?;
    relative_path
        .ensure_within_root(project_root)
        .map_err(|err| {
            anyhow!("file path `{input}` must stay within the repository root: {err}")
        })?;
    Ok(relative_path)
}

fn atomic_write_string(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let temp_path = path.with_extension(format!(
        "mercury-{}.tmp",
        Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    std::fs::write(&temp_path, content)?;
    std::fs::rename(&temp_path, path)?;
    Ok(())
}

fn supported_config_value_kind(key: &str) -> Option<ConfigValueKind> {
    SUPPORTED_CONFIG_KEYS
        .iter()
        .find_map(|(candidate, kind)| (*candidate == key).then_some(*kind))
}

fn supported_config_keys_help() -> String {
    SUPPORTED_CONFIG_KEYS
        .iter()
        .map(|(key, _)| *key)
        .collect::<Vec<_>>()
        .join(", ")
}

fn parse_config_item(raw: &str, kind: ConfigValueKind) -> Result<Item> {
    let item =
        match kind {
            ConfigValueKind::String => toml_value(raw),
            ConfigValueKind::Bool => toml_value(raw.parse::<bool>().with_context(|| {
                format!("expected a boolean value for config key, got '{raw}'")
            })?),
            ConfigValueKind::Integer => toml_value(raw.parse::<i64>().with_context(|| {
                format!("expected an integer value for config key, got '{raw}'")
            })?),
            ConfigValueKind::Float => {
                toml_value(raw.parse::<f64>().with_context(|| {
                    format!("expected a float value for config key, got '{raw}'")
                })?)
            }
        };
    Ok(item)
}

fn set_toml_key(document: &mut DocumentMut, key: &str, item: Item) -> Result<()> {
    let parts = key.split('.').collect::<Vec<_>>();
    let (leaf, parents) = parts
        .split_last()
        .ok_or_else(|| anyhow!("config key cannot be empty"))?;

    let mut current = document.as_table_mut();
    for part in parents {
        if !current.contains_key(part) {
            current.insert(part, Item::Table(Table::new()));
        }

        let next = current
            .get_mut(part)
            .and_then(Item::as_table_mut)
            .ok_or_else(|| {
                anyhow!("config path '{part}' is not a table and cannot contain '{key}'")
            })?;
        current = next;
    }

    current.insert(leaf, item);
    Ok(())
}

fn update_config_key(config_path: &Path, key: &str, raw_value: &str) -> Result<()> {
    let kind = supported_config_value_kind(key).ok_or_else(|| {
        anyhow!(
            "unsupported config key '{key}'. Supported keys: {}",
            supported_config_keys_help()
        )
    })?;

    let content = std::fs::read_to_string(config_path)
        .with_context(|| format!("failed to read {}", config_path.display()))?;
    let mut document = content
        .parse::<DocumentMut>()
        .with_context(|| format!("failed to parse {}", config_path.display()))?;
    let item = parse_config_item(raw_value, kind)?;
    set_toml_key(&mut document, key, item)?;

    let rendered = document.to_string();
    toml::from_str::<MercuryConfig>(&rendered)
        .with_context(|| format!("updated config would be invalid after setting {key}"))?;
    atomic_write_string(config_path, &rendered)?;
    Ok(())
}

fn render_numbered_context_window(
    file_label: &str,
    lines: &[&str],
    start_line: usize,
    end_line: usize,
    highlight_line: usize,
) -> String {
    let body = (start_line..=end_line)
        .map(|line_number| {
            let marker = if line_number == highlight_line {
                '>'
            } else {
                ' '
            };
            let line = lines
                .get(line_number.saturating_sub(1))
                .copied()
                .unwrap_or("");
            format!("{marker}{line_number:>4} | {line}")
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("{file_label}:{start_line}-{end_line}\n{body}")
}

fn join_line_window(lines: &[&str], start_line: usize, end_line: usize) -> String {
    lines[(start_line - 1)..end_line].join("\n")
}

fn build_next_edit_cli_context(
    relative_path: &RepoRelativePath,
    content: &str,
    requested_line: u32,
) -> Result<NextEditCliContext> {
    let relative = relative_path.as_str().to_string();
    let lines = content.lines().collect::<Vec<_>>();

    if lines.is_empty() {
        return Ok(NextEditCliContext {
            current_file_path: relative.clone(),
            code_to_edit: String::new(),
            cursor: "1:1".to_string(),
            recent_snippets: format!("{relative}:1-1\n>   1 | "),
        });
    }

    let requested_line = requested_line.max(1) as usize;
    let clamped_line = requested_line.min(lines.len());
    let focus_start = clamped_line.saturating_sub(3).max(1);
    let focus_end = (clamped_line + 3).min(lines.len());
    let recent_start = clamped_line.saturating_sub(8).max(1);
    let recent_end = (clamped_line + 8).min(lines.len());

    Ok(NextEditCliContext {
        current_file_path: relative.clone(),
        code_to_edit: join_line_window(&lines, focus_start, focus_end),
        cursor: format!("{}:1", clamped_line - focus_start + 1),
        recent_snippets: render_numbered_context_window(
            &relative,
            &lines,
            recent_start,
            recent_end,
            clamped_line,
        ),
    })
}

fn write_json_artifact<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(value)?)?;
    Ok(())
}

fn copy_if_exists(source: &Path, destination: &Path) -> Result<()> {
    if !source.exists() {
        return Ok(());
    }
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(source, destination)?;
    Ok(())
}

fn patch_size_bytes(path: &Path) -> Result<Option<u64>> {
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(path.metadata()?.len()))
}

fn classify_fix_benchmark_outcome(
    final_bundle_verified: bool,
    accepted_patch: bool,
) -> &'static str {
    if final_bundle_verified {
        "verified_repair"
    } else if accepted_patch {
        "accepted_patch_unverified"
    } else {
        "no_patch"
    }
}

fn classify_fix_failure_attribution(
    summary: &StepExecutionSummary,
    accepted_patch: bool,
) -> Option<&'static str> {
    if summary.final_bundle_verified {
        return None;
    }
    if summary.final_bundle_failures > 0 || accepted_patch {
        return Some("final_bundle_verification_failed");
    }
    if summary.candidate_verification_failures > 0 {
        return Some("candidate_failed_verifier");
    }
    if summary.safety_failures > 0 {
        return Some("candidate_failed_safety");
    }
    if summary.generation_failures > 0 {
        return Some("patch_generation_failed");
    }
    Some("no_patch_emitted")
}

// ---------------------------------------------------------------------------
// Default config (fallback)
// ---------------------------------------------------------------------------

const DEFAULT_CONFIG: &str = r#"[api]
mercury2_endpoint = "https://api.inceptionlabs.ai/v1/chat/completions"
mercury_edit_endpoint = "https://api.inceptionlabs.ai/v1"
api_key_env = "INCEPTION_API_KEY"

[scheduler]
max_concurrency = 20
max_cost_per_command = 0.50
max_agents_per_command = 100
retry_limit = 3
backoff_base_ms = 500

[thermal]
decay_half_life_seconds = 300
aggregation_method = "log_sum_exp"
rescan_on_git_pull = true
hot_threshold = 0.7
cool_threshold = 0.3
lock_cool_zones = true

[annealing]
enable_global_momentum = true
initial_temperature = 1.0
cooling_rate = 0.02
min_modification_threshold = 0.1

[verification]
parse_before_write = true
test_after_write = true
lint_after_write = true
mercury2_critique_on_failure = true
test_command = "cargo test"
lint_command = "cargo clippy"

[repo.languages]
rust = true
python = false
typescript = false
go = false
java = false

[constitutional]
style_guide = ""
architecture_rules = ""
naming_conventions = ""
"#;

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Init => cmd_init()?,

        Commands::Status {
            heatmap,
            live,
            interval_ms,
            agents,
            budget,
        } => {
            let (_config, db, _root) = load_project()?;
            cmd_status(&db, heatmap, live, interval_ms, agents, budget)?;
        }

        Commands::Plan { goal, output } => {
            let (config, db, _root) = load_project()?;
            cmd_plan(&config, &db, &goal, output.as_deref()).await?;
        }

        Commands::Ask {
            query,
            reasoning_effort,
        } => {
            let (config, db, _root) = load_project()?;
            let effort = reasoning_effort.as_deref().and_then(parse_reasoning_effort);
            cmd_ask(&config, &db, &query, effort).await?;
        }

        Commands::Edit { action } => {
            let (config, _db, project_root) = load_project()?;
            let api_key = api::resolve_api_key(&config.api.api_key_env)?;

            let edit_client =
                MercuryEditClient::new(api_key.clone(), config.api.mercury_edit_endpoint.clone())
                    .with_retries(
                        config.scheduler.retry_limit,
                        config.scheduler.backoff_base_ms,
                    );
            let patcher = engine::Patcher::new(edit_client);

            match action {
                EditAction::Apply {
                    file,
                    update_snippet,
                    dry_run,
                    force,
                } => {
                    let relative_path = parse_cli_repo_relative_path(&file, &project_root)?;
                    let file_path = relative_path.resolve_under(&project_root)?;
                    let content = std::fs::read_to_string(&file_path)?;
                    let (patched, usage) = patcher.patch(&content, &update_snippet).await?;
                    if dry_run {
                        println!("--- Dry run (not written) ---");
                        println!("{patched}");
                    } else if force {
                        println!("WARNING: verification bypassed due to --force");
                        atomic_write_string(&file_path, &patched)?;
                        println!("Patched {relative_path}");
                    } else {
                        let verify_client = Mercury2Client::new(api_key.clone())
                            .with_base_url(config.api.mercury2_endpoint.clone())
                            .with_retries(
                                config.scheduler.retry_limit,
                                config.scheduler.backoff_base_ms,
                            );
                        let verifier = Verifier::new(
                            build_verify_config(&config),
                            if config.verification.mercury2_critique_on_failure {
                                Some(verify_client)
                            } else {
                                None
                            },
                        );
                        verify_and_accept_patch(&verifier, &relative_path, &patched, &project_root)
                            .await?;
                        println!("Patched {relative_path}");
                    }
                    println!(
                        "Tokens: {} | Cost: ${:.4}",
                        usage.tokens_used, usage.cost_usd
                    );
                }
                EditAction::Complete { file } => {
                    let (relative_path, line) = parse_file_line(&file, &project_root)?;
                    let path = relative_path.resolve_under(&project_root)?;
                    let content = std::fs::read_to_string(&path)?;
                    // Split at the cursor line for FIM prompt/suffix
                    let lines: Vec<&str> = content.lines().collect();
                    let split_at = (line as usize).min(lines.len());
                    let prompt = lines[..split_at].join("\n");
                    let suffix = if split_at < lines.len() {
                        lines[split_at..].join("\n")
                    } else {
                        String::new()
                    };
                    let (result, usage) = patcher.complete(&prompt, &suffix).await?;
                    println!("{result}");
                    println!(
                        "Tokens: {} | Cost: ${:.4}",
                        usage.tokens_used, usage.cost_usd
                    );
                }
                EditAction::Next { file } => {
                    let (relative_path, line) = parse_file_line(&file, &project_root)?;
                    let path = relative_path.resolve_under(&project_root)?;
                    let content = std::fs::read_to_string(&path)?;
                    let context = build_next_edit_cli_context(&relative_path, &content, line)?;
                    let (result, usage) = patcher
                        .next_edit_with_context(
                            &context.current_file_path,
                            &content,
                            &context.code_to_edit,
                            &context.cursor,
                            &context.recent_snippets,
                            "",
                        )
                        .await?;
                    println!("{result}");
                    println!(
                        "Tokens: {} | Cost: ${:.4}",
                        usage.tokens_used, usage.cost_usd
                    );
                }
            }
        }

        Commands::Fix {
            description,
            max_agents,
            max_cost,
            noninteractive,
        } => {
            let (config, db, root) = load_project()?;
            let noninteractive = is_noninteractive_mode(noninteractive);
            cmd_fix(
                &config,
                &db,
                &root,
                &description,
                max_agents,
                max_cost,
                noninteractive,
            )
            .await?;
        }

        Commands::Watch {
            command,
            repair,
            noninteractive,
        } => {
            let (config, db, root) = load_project()?;
            let noninteractive = is_noninteractive_mode(noninteractive);
            cmd_watch(&config, &db, &root, &command, repair, noninteractive).await?;
        }

        Commands::Config { action } => match action {
            ConfigAction::Get { key } => {
                let mercury_dir = find_mercury_dir()?;
                let content = std::fs::read_to_string(mercury_dir.join("config.toml"))?;
                println!("Config key '{key}':");
                // Simple key lookup in TOML
                let table: toml::Value = toml::from_str(&content)?;
                let parts: Vec<&str> = key.split('.').collect();
                let mut current = &table;
                for part in &parts {
                    match current.get(part) {
                        Some(v) => current = v,
                        None => {
                            println!("  Key not found: {key}");
                            return Ok(());
                        }
                    }
                }
                println!("  {current}");
            }
            ConfigAction::Set { key, value } => {
                let mercury_dir = find_mercury_dir()?;
                let config_path = mercury_dir.join("config.toml");
                update_config_key(&config_path, &key, &value)?;
                println!("Updated {}: {key} = {value}", config_path.display());
            }
            ConfigAction::Validate => {
                let mercury_dir = find_mercury_dir()?;
                match MercuryConfig::load(&mercury_dir.join("config.toml")) {
                    Ok(_) => println!("Configuration is valid."),
                    Err(e) => println!("Configuration error: {e}"),
                }
            }
        },
    }

    Ok(())
}

/// Parse a reasoning effort string into the enum.
fn parse_reasoning_effort(s: &str) -> Option<api::ReasoningEffort> {
    match s.to_lowercase().as_str() {
        "instant" => Some(api::ReasoningEffort::Instant),
        "low" => Some(api::ReasoningEffort::Low),
        "medium" | "med" => Some(api::ReasoningEffort::Medium),
        "high" => Some(api::ReasoningEffort::High),
        _ => None,
    }
}

/// Parse "file.rs:42" into (RepoRelativePath, line_number).
fn parse_file_line(input: &str, project_root: &Path) -> Result<(RepoRelativePath, u32)> {
    if let Some((file, line)) = input.rsplit_once(':') {
        if let Ok(n) = line.parse::<u32>() {
            return Ok((parse_cli_repo_relative_path(file, project_root)?, n));
        }
    }
    Ok((parse_cli_repo_relative_path(input, project_root)?, 1))
}

fn build_verify_config(config: &MercuryConfig) -> VerifyConfig {
    VerifyConfig {
        parse_before_write: config.verification.parse_before_write,
        test_after_write: config.verification.test_after_write,
        lint_after_write: config.verification.lint_after_write,
        mercury2_critique_on_failure: config.verification.mercury2_critique_on_failure,
        test_command: config.verification.test_command.clone(),
        lint_command: config.verification.lint_command.clone(),
    }
}

fn build_watch_verify_config(config: &MercuryConfig, target: &RustRepairTarget) -> VerifyConfig {
    let mut verify_config = build_verify_config(config);
    match target.mode {
        RustRepairMode::TestLike => {
            verify_config.test_after_write = true;
            verify_config.test_command = target.verifier_command.clone();
            verify_config.lint_after_write = false;
        }
        RustRepairMode::Lint => {
            verify_config.test_after_write = false;
            verify_config.lint_after_write = true;
            verify_config.lint_command = target.verifier_command.clone();
        }
    }
    verify_config
}

fn classify_rust_repair_command(command: &str) -> Option<RustRepairTarget> {
    let trimmed = command.trim();
    if trimmed.is_empty() || failure_parser::contains_shell_composition(trimmed) {
        return None;
    }

    let command_parts = failure_parser::parse_command_parts(trimmed);
    let command_kind = failure_parser::classify_cargo_command(&command_parts);
    let mode = match command_kind {
        CargoCommandKind::Test | CargoCommandKind::Check => RustRepairMode::TestLike,
        CargoCommandKind::Clippy => RustRepairMode::Lint,
        CargoCommandKind::Unknown => return None,
    };

    Some(RustRepairTarget {
        verifier_command: trimmed.to_string(),
        mode,
    })
}

fn build_watch_repair_description(command: &str, result: &WatchCommandResult) -> String {
    let command = redact_secrets(command);
    let mut description = format!(
        "Repair the repository so the Rust verifier command `{command}` succeeds.\nExit code: {}.\nFocus only on the changes needed to make that command pass.\n",
        result
            .exit_code
            .map(|code| code.to_string())
            .unwrap_or_else(|| "unknown".to_string())
    );

    let stderr = truncate_watch_output(&result.stderr, 6000);
    if !stderr.is_empty() {
        description.push_str("\nCommand stderr:\n");
        description.push_str(&stderr);
        description.push('\n');
    }

    let stdout = truncate_watch_output(&result.stdout, 4000);
    if !stdout.is_empty() {
        description.push_str("\nCommand stdout:\n");
        description.push_str(&stdout);
        description.push('\n');
    }

    if let Some(parsed_failure) = result.parsed_failure.as_ref() {
        let parsed =
            serde_json::to_string_pretty(parsed_failure).unwrap_or_else(|_| "{}".to_string());
        description.push_str("\nStructured parsed failure:\n");
        description.push_str(&parsed);
        description.push('\n');
    }

    let tool_surface = failure_parser::repo_native_tool_surface()
        .into_iter()
        .map(|tool| format!("- {}: {}", tool.name, tool.description))
        .collect::<Vec<_>>()
        .join("\n");
    if !tool_surface.is_empty() {
        description
            .push_str("\nRepo-native tool surface available to grounding and repair loop:\n");
        description.push_str(&tool_surface);
        description.push('\n');
    }

    description
}

fn truncate_watch_output(output: &str, max_chars: usize) -> String {
    let trimmed = output.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }

    let truncated: String = trimmed.chars().take(max_chars).collect();
    format!("{truncated}\n...[truncated]")
}

fn run_watch_command(command_parts: &[String], working_dir: &Path) -> Result<WatchCommandResult> {
    let mut process = verification::build_allowlisted_verifier_command(command_parts, working_dir)
        .map_err(|err| anyhow::anyhow!("watch command rejected: {err}"))?;
    let output = process
        .output()
        .context("failed to execute watch command")?;

    let stdout_raw = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr_raw = String::from_utf8_lossy(&output.stderr).to_string();
    let parsed_failure = if output.status.success() {
        None
    } else {
        parse_supported_watch_failure(command_parts, &stdout_raw, &stderr_raw)
    };
    let stdout = redact_secrets(&stdout_raw);
    let stderr = redact_secrets(&stderr_raw);

    Ok(WatchCommandResult {
        command: redact_secrets(&command_parts.join(" ")),
        success: output.status.success(),
        exit_code: output.status.code(),
        stdout,
        stderr,
        parsed_failure,
    })
}

fn parse_supported_watch_failure(
    command_parts: &[String],
    stdout: &str,
    stderr: &str,
) -> Option<mercury_cli::failure_parser::ParsedFailureReport> {
    let command_kind = failure_parser::classify_verifier_command(command_parts);
    if matches!(command_kind, failure_parser::VerifierCommandKind::Unknown) {
        return None;
    }
    Some(failure_parser::parse_verifier_failure(
        &command_kind,
        command_parts,
        stdout,
        stderr,
    ))
}

fn replay_watch_command_output(label: &str, result: &WatchCommandResult, noninteractive: bool) {
    let status = result
        .exit_code
        .map(|code| code.to_string())
        .unwrap_or_else(|| "signal".to_string());
    if noninteractive {
        emit_runtime_event(
            true,
            "watch_command_output",
            serde_json::json!({
                "label": label,
                "exit_code": result.exit_code,
                "exit_status": status,
                "success": result.success,
                "stdout_bytes": result.stdout.len(),
                "stderr_bytes": result.stderr.len(),
                "stdout_preview": truncate_watch_output(&result.stdout, 1000),
                "stderr_preview": truncate_watch_output(&result.stderr, 1000),
                "parsed_failure": result.parsed_failure,
            }),
        );
        return;
    }

    println!(
        "[watch:{label}] mode=interactive exit={status} success={}",
        if result.success { "true" } else { "false" }
    );
    if !result.stdout.is_empty() {
        print!("{}", result.stdout);
    }
    if !result.stderr.is_empty() {
        eprint!("{}", result.stderr);
    }
}

fn print_watch_cycle_result(outcome: &WatchCycleOutcome, noninteractive: bool) {
    if noninteractive {
        emit_runtime_event(
            true,
            "watch_cycle_result",
            serde_json::json!({
                "decision": outcome.record.decision.clone(),
                "artifact_root": outcome.artifact_root.display().to_string(),
                "repair": outcome.record.repair.clone(),
            }),
        );
        return;
    }
    println!("Decision: {}", outcome.record.decision);
    println!("Artifact bundle: {}", outcome.artifact_root.display());
    if let Some(repair) = outcome.record.repair.as_ref() {
        if let Some(verifier_command) = repair.verifier_command.as_ref() {
            println!("Targeted verifier: {verifier_command}");
        }
        if let Some(path) = repair.fix_artifact_root.as_ref() {
            println!("Repair run artifacts: {path}");
        }
        if let Some(error) = repair.error.as_ref() {
            println!("Repair note: {error}");
        }
    }
}

fn watch_decision_succeeded(decision: &str) -> bool {
    matches!(decision, "passed_without_repair" | "repaired_and_verified")
}

fn write_watch_output_artifacts(artifact_root: &Path, record: &WatchRunRecord) -> Result<()> {
    atomic_write_string(
        &artifact_root.join("initial.stdout.txt"),
        &record.initial_run.stdout,
    )?;
    atomic_write_string(
        &artifact_root.join("initial.stderr.txt"),
        &record.initial_run.stderr,
    )?;
    if let Some(parsed_failure) = record.initial_run.parsed_failure.as_ref() {
        write_json_artifact(&artifact_root.join("initial.failure.json"), parsed_failure)?;
    }
    if let Some(confirmation) = record.confirmation_run.as_ref() {
        atomic_write_string(
            &artifact_root.join("confirmation.stdout.txt"),
            &confirmation.stdout,
        )?;
        atomic_write_string(
            &artifact_root.join("confirmation.stderr.txt"),
            &confirmation.stderr,
        )?;
        if let Some(parsed_failure) = confirmation.parsed_failure.as_ref() {
            write_json_artifact(
                &artifact_root.join("confirmation.failure.json"),
                parsed_failure,
            )?;
        }
    }
    Ok(())
}

fn capture_repo_watch_snapshot(project_root: &Path) -> Result<RepoWatchSnapshot> {
    let mut files = BTreeMap::new();
    repo::walk_repo_tree(
        project_root,
        |relative| should_skip_watch_path(relative.as_path()),
        &mut |relative, _, metadata| {
            if metadata.is_file() {
                let modified_unix_nanos = metadata
                    .modified()
                    .ok()
                    .and_then(|mtime| mtime.duration_since(UNIX_EPOCH).ok())
                    .map(|duration| duration.as_nanos())
                    .unwrap_or_default();
                files.insert(
                    relative.as_path().to_path_buf(),
                    RepoFileFingerprint {
                        modified_unix_nanos,
                        len: metadata.len(),
                    },
                );
            }
            Ok(())
        },
    )?;
    Ok(RepoWatchSnapshot(files))
}

fn should_skip_watch_path(relative: &Path) -> bool {
    let relative = relative.to_string_lossy();
    relative == ".git"
        || relative.starts_with(".git/")
        || relative == "target"
        || relative.starts_with("target/")
        || relative == ".mercury"
        || relative.starts_with(".mercury/")
}

async fn wait_for_repo_change(
    project_root: &Path,
    baseline: &mut RepoWatchSnapshot,
) -> Result<bool> {
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                println!("Stopping watch.");
                return Ok(false);
            }
            _ = tokio::time::sleep(Duration::from_millis(750)) => {
                let current = capture_repo_watch_snapshot(project_root)?;
                if &current != baseline {
                    *baseline = current;
                    return Ok(true);
                }
            }
        }
    }
}

async fn verify_and_accept_patch(
    verifier: &Verifier<Mercury2Client>,
    relative_path: &RepoRelativePath,
    patched_content: &str,
    project_root: &Path,
) -> Result<()> {
    let sandbox_root = create_cli_worktree_root(project_root)?;
    let accepted_states = HashMap::new();
    repo::prepare_repair_workspace(project_root, &sandbox_root, &accepted_states)?;
    let sandbox_file = relative_path.resolve_under(&sandbox_root)?;

    if let Some(parent) = sandbox_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&sandbox_file, patched_content)?;

    let verification = verifier
        .verify(&sandbox_file, patched_content, &sandbox_root)
        .await?;
    if verification.is_ok() {
        let project_file = relative_path.resolve_under(project_root)?;
        atomic_write_string(&project_file, patched_content)?;
        let _ = repo::cleanup_repair_workspace(project_root, &sandbox_root);
        return Ok(());
    }
    let _ = repo::cleanup_repair_workspace(project_root, &sandbox_root);

    let structured = serde_json::json!({
        "parse": verification.parse_ok,
        "test": verification.test_ok,
        "lint": verification.lint_ok,
        "critique": verification.critique,
        "command_results": verification.command_results,
        "errors": verification.errors,
    });
    anyhow::bail!(
        "patch rejected by verification for {}\n{}",
        relative_path,
        serde_json::to_string_pretty(&structured)?
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::tempdir;

    fn sample_config() -> MercuryConfig {
        MercuryConfig {
            api: ApiConfig {
                mercury2_endpoint: "https://example.invalid/chat".to_string(),
                mercury_edit_endpoint: "https://example.invalid/edit".to_string(),
                api_key_env: "INCEPTION_API_KEY".to_string(),
            },
            scheduler: SchedulerConfigToml {
                max_concurrency: 2,
                max_cost_per_command: 3.0,
                max_agents_per_command: 2,
                retry_limit: 2,
                backoff_base_ms: 100,
            },
            thermal: ThermalConfig {
                decay_half_life_seconds: 60.0,
                aggregation_method: "max".to_string(),
                rescan_on_git_pull: false,
                hot_threshold: 0.7,
                cool_threshold: 0.3,
                lock_cool_zones: true,
            },
            annealing: AnnealingConfig {
                enable_global_momentum: true,
                initial_temperature: 1.0,
                cooling_rate: 0.9,
                min_modification_threshold: 0.1,
            },
            verification: VerificationConfig {
                parse_before_write: true,
                test_after_write: true,
                lint_after_write: true,
                mercury2_critique_on_failure: false,
                test_command: "cargo test".to_string(),
                lint_command: "cargo clippy".to_string(),
            },
            constitutional: ConstitutionalConfig {
                style_guide: String::new(),
                architecture_rules: String::new(),
                naming_conventions: String::new(),
            },
            repo: RepoConfig::default(),
        }
    }

    fn verification_config_that_fails() -> VerifyConfig {
        VerifyConfig {
            parse_before_write: false,
            test_after_write: true,
            lint_after_write: false,
            mercury2_critique_on_failure: false,
            test_command: "false".to_string(),
            lint_command: "true".to_string(),
        }
    }

    fn verification_config_that_passes() -> VerifyConfig {
        VerifyConfig {
            parse_before_write: false,
            test_after_write: false,
            lint_after_write: false,
            mercury2_critique_on_failure: false,
            test_command: "true".to_string(),
            lint_command: "true".to_string(),
        }
    }

    fn sample_agent_log_entry(
        id: i64,
        status: &str,
        command: &str,
        micro_heatmap: Option<Value>,
    ) -> db::AgentLogEntry {
        db::AgentLogEntry {
            id,
            agent_id: format!("agent-{id:02}"),
            command: command.to_string(),
            file_path: format!("src/file_{id}.rs"),
            status: status.to_string(),
            micro_heatmap: micro_heatmap.map(|value| value.to_string()),
            started_at: format!("2026-03-11T12:00:{id:02}Z"),
            completed_at: matches!(status, "success" | "failed")
                .then(|| format!("2026-03-11T12:01:{id:02}Z")),
            tokens_used: id * 100,
            cost_usd: id as f64 * 0.015,
        }
    }

    fn sample_aggregate(
        file_path: &str,
        composite_score: f64,
        agent_density: i32,
    ) -> db::ThermalAggregate {
        db::ThermalAggregate {
            file_path: file_path.to_string(),
            composite_score,
            max_score: composite_score,
            agent_density,
            last_updated: "2026-03-11T12:00:00Z".to_string(),
            is_locked: false,
        }
    }

    fn sample_swarm_state() -> db::SwarmState {
        db::SwarmState {
            id: 1,
            total_agents_spawned: 5,
            active_agents: 2,
            total_tokens_used: 4096,
            total_cost_usd: 1.25,
            global_temperature: 0.72,
            iteration_count: 3,
            started_at: "2026-03-11T11:59:00Z".to_string(),
        }
    }

    fn init_git_repo(path: &Path) {
        run_git(path, &["init", "-q"]);
        run_git(path, &["config", "user.email", "mercury@example.com"]);
        run_git(path, &["config", "user.name", "Mercury Tests"]);
    }

    fn run_git(path: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .env("GIT_AUTHOR_NAME", "Mercury Tests")
            .env("GIT_AUTHOR_EMAIL", "mercury@example.com")
            .env("GIT_COMMITTER_NAME", "Mercury Tests")
            .env("GIT_COMMITTER_EMAIL", "mercury@example.com")
            .output()
            .expect("git command should run");

        assert!(
            output.status.success(),
            "git command failed: git -C {} {} stderr={}",
            path.display(),
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn parse_cli_repo_relative_path_rejects_invalid_inputs() {
        let temp = tempdir().unwrap();

        for (input, expected_reason) in [
            (
                "../../etc/passwd",
                "parent-directory traversal is not allowed",
            ),
            ("/tmp/file", "absolute paths are not allowed"),
            (r"C:\temp\file.rs", "absolute Windows paths are not allowed"),
        ] {
            let err = parse_cli_repo_relative_path(input, temp.path()).expect_err(input);
            assert!(err.to_string().contains(expected_reason));
        }
    }

    #[test]
    fn parse_file_line_uses_repo_relative_paths() {
        let temp = tempdir().unwrap();

        let (relative, line) = parse_file_line("src/lib.rs:42", temp.path()).unwrap();
        assert_eq!(relative, RepoRelativePath::new("src/lib.rs").unwrap());
        assert_eq!(line, 42);

        let (default_relative, default_line) = parse_file_line("src/main.rs", temp.path()).unwrap();
        assert_eq!(
            default_relative,
            RepoRelativePath::new("src/main.rs").unwrap()
        );
        assert_eq!(default_line, 1);
    }

    #[tokio::test]
    async fn verify_failure_does_not_mutate_user_worktree() {
        let temp = tempdir().unwrap();
        let file = temp.path().join("sample.rs");
        let original = "fn main() { println!(\"hello\"); }\n";
        let patched = "fn main() { println!(\"goodbye\"); }\n";
        std::fs::write(&file, original).unwrap();

        let verifier = Verifier::<Mercury2Client>::new(verification_config_that_fails(), None);
        let relative = RepoRelativePath::new("sample.rs").unwrap();
        let result = verify_and_accept_patch(&verifier, &relative, patched, temp.path()).await;

        assert!(result.is_err());
        let after = std::fs::read_to_string(&file).unwrap();
        assert_eq!(after, original);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn verify_and_accept_patch_rejects_symlink_escape_paths() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().unwrap();
        let outside = temp.path().join("outside");
        let project_root = temp.path().join("project");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::create_dir_all(&project_root).unwrap();
        init_git_repo(&project_root);
        std::fs::write(
            project_root.join("Cargo.toml"),
            "[package]\nname = \"cli-edit\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(project_root.join("lib.rs"), "pub fn ok() {}\n").unwrap();
        run_git(&project_root, &["add", "."]);
        run_git(&project_root, &["commit", "-m", "init"]);
        symlink(&outside, project_root.join("escape")).unwrap();

        let verifier = Verifier::<Mercury2Client>::new(verification_config_that_passes(), None);
        let relative = RepoRelativePath::new("escape/file.rs").unwrap();
        let result =
            verify_and_accept_patch(&verifier, &relative, "fn main() {}\n", &project_root).await;

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("symlink escapes the repository root"));
        assert!(!outside.join("file.rs").exists());
    }

    #[test]
    #[cfg(unix)]
    fn capture_repo_watch_snapshot_rejects_symlink_escape_paths() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().unwrap();
        let project_root = temp.path().join("project");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&project_root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        symlink(&outside, project_root.join("escape")).unwrap();

        let err =
            capture_repo_watch_snapshot(&project_root).expect_err("symlink escape should fail");
        assert!(err
            .to_string()
            .contains("symlink escapes the repository root"));
    }

    #[test]
    fn parse_candidate_runtime_metadata_reads_candidate_fields() {
        let entry = sample_agent_log_entry(
            1,
            "running",
            "repair:verify",
            Some(serde_json::json!({
                "phase": "verify",
                "candidate_source": "critique_retry",
                "retry_attempts": 2,
                "outcome": "verification_failed",
                "reason": "test failure",
                "failure_stage": "verification",
                "sandbox_root": ".mercury/worktrees/candidate-1",
                "fanout": 4,
                "temperature": 0.61,
                "touched_lines": 17,
                "byte_delta": -48
            })),
        );

        let metadata = parse_candidate_runtime_metadata(&entry);
        assert_eq!(metadata.phase.as_deref(), Some("verify"));
        assert_eq!(metadata.source_lineage.as_deref(), Some("critique_retry"));
        assert_eq!(metadata.retry_count, Some(2));
        assert_eq!(metadata.outcome.as_deref(), Some("verification_failed"));
        assert_eq!(metadata.reason.as_deref(), Some("test failure"));
        assert_eq!(metadata.failure_stage.as_deref(), Some("verification"));
        assert_eq!(
            metadata.sandbox_root.as_deref(),
            Some(".mercury/worktrees/candidate-1")
        );
        assert_eq!(metadata.fanout, Some(4));
        assert_eq!(metadata.temperature, Some(0.61));
        assert_eq!(metadata.touched_lines, Some(17));
        assert_eq!(metadata.byte_delta, Some(-48));
    }

    #[test]
    fn initial_live_status_events_include_backlog_candidates_and_runtime() {
        let plan = sample_agent_log_entry(
            1,
            "running",
            "repair:plan",
            Some(serde_json::json!({
                "phase": "plan",
                "candidate_source": "apply_edit"
            })),
        );
        let verify = sample_agent_log_entry(
            2,
            "success",
            "repair:verify",
            Some(serde_json::json!({
                "phase": "verify",
                "candidate_source": "critique_retry",
                "outcome": "accepted"
            })),
        );
        let aggregates = vec![sample_aggregate("src/file_2.rs", 0.91, 3)];
        let active = vec![plan.clone()];
        let logs = vec![plan, verify];
        let swarm = sample_swarm_state();

        let snapshot = build_live_status_snapshot(&aggregates, &active, &logs, Some(&swarm));
        let events = initial_live_status_events(&snapshot, LiveRenderMode::JsonStream, 750);

        assert_eq!(
            events.first().map(|event| event.event),
            Some("live_attached")
        );
        assert!(events
            .iter()
            .any(|event| { event.event == "phase_started" && event.details["phase"] == "plan" }));
        assert!(events
            .iter()
            .any(|event| { event.event == "phase_started" && event.details["phase"] == "verify" }));

        let candidate_events = events
            .iter()
            .filter(|event| event.event == "candidate_event")
            .collect::<Vec<_>>();
        assert_eq!(candidate_events.len(), 2);
        assert!(candidate_events
            .iter()
            .all(|event| event.details["backlog"] == Value::Bool(true)));
        assert!(candidate_events.iter().any(|event| {
            event.details["agent_id"] == "agent-02"
                && event.details["verification_result"] == "passed"
                && event.details["decision"] == "won"
                && event.details["explanation"] == "verified candidate selected for application"
        }));

        let runtime_event = events
            .iter()
            .find(|event| event.event == "runtime_state")
            .expect("runtime state event should exist");
        assert_eq!(runtime_event.details["backlog"], Value::Bool(true));
        assert_eq!(runtime_event.details["total_agents_spawned"], 5);
        assert_eq!(runtime_event.details["locked_files"], 0);
        assert_eq!(runtime_event.details["hottest_file"], "src/file_2.rs");
    }

    #[test]
    fn diff_live_status_events_report_candidate_transitions() {
        let running = sample_agent_log_entry(
            1,
            "running",
            "repair:apply",
            Some(serde_json::json!({
                "phase": "apply",
                "candidate_source": "apply_edit"
            })),
        );
        let previous = build_live_status_snapshot(
            &[sample_aggregate("src/file_1.rs", 0.45, 1)],
            std::slice::from_ref(&running),
            std::slice::from_ref(&running),
            Some(&sample_swarm_state()),
        );

        let success = sample_agent_log_entry(
            1,
            "success",
            "repair:apply",
            Some(serde_json::json!({
                "phase": "apply",
                "candidate_source": "apply_edit",
                "outcome": "accepted"
            })),
        );
        let launched = sample_agent_log_entry(
            2,
            "spawned",
            "repair:verify",
            Some(serde_json::json!({
                "phase": "verify",
                "candidate_source": "exploratory_next_edit"
            })),
        );
        let current = build_live_status_snapshot(
            &[
                sample_aggregate("src/file_1.rs", 0.60, 2),
                sample_aggregate("src/file_2.rs", 0.88, 3),
            ],
            std::slice::from_ref(&launched),
            &[success, launched.clone()],
            Some(&db::SwarmState {
                total_agents_spawned: 6,
                active_agents: 1,
                total_tokens_used: 8192,
                total_cost_usd: 2.5,
                global_temperature: 0.66,
                iteration_count: 4,
                ..sample_swarm_state()
            }),
        );

        let events = diff_live_status_events(&previous, &current);

        assert!(events
            .iter()
            .any(|event| { event.event == "phase_started" && event.details["phase"] == "verify" }));
        assert!(events.iter().any(|event| {
            event.event == "candidate_event"
                && event.details["agent_id"] == "agent-01"
                && event.details["transition"] == "status_changed"
                && event.details["verification_result"] == "passed"
                && event.details["decision"] == "won"
                && event.details["explanation"] == "verified candidate selected for application"
        }));
        assert!(events.iter().any(|event| {
            event.event == "candidate_event"
                && event.details["agent_id"] == "agent-02"
                && event.details["transition"] == "launched"
                && event.details["source_lineage"] == "exploratory_next_edit"
                && event.details["decision"] == "in_flight"
        }));
        assert!(events.iter().any(|event| {
            event.event == "runtime_state"
                && event.details["backlog"] == Value::Bool(false)
                && event.details["total_tokens_used"] == 8192
        }));
    }

    #[test]
    fn candidate_event_details_surface_suppression_and_verification_loss_explanations() {
        let suppressed = sample_agent_log_entry(
            1,
            "failed",
            "repair:verify",
            Some(serde_json::json!({
                "phase": "verify",
                "candidate_source": "grounded_next_edit",
                "outcome": "rejected",
                "reason": "duplicate verified candidate output; identical verified state already kept from a higher-ranked candidate"
            })),
        );
        let verification_failure = sample_agent_log_entry(
            2,
            "failed",
            "repair:verify",
            Some(serde_json::json!({
                "phase": "verify",
                "candidate_source": "critique_retry",
                "outcome": "rejected",
                "failure_stage": "verification",
                "reason": "cargo test failed"
            })),
        );

        let suppressed_details = candidate_event_details(
            &build_candidate_live_state(&suppressed),
            "status_changed",
            false,
        );
        assert_eq!(suppressed_details["decision"], "suppressed");
        assert_eq!(
            suppressed_details["verification_result"],
            "passed_not_selected"
        );
        assert_eq!(
            suppressed_details["explanation"],
            "duplicate verified candidate output; identical verified state already kept from a higher-ranked candidate"
        );

        let loss_details = candidate_event_details(
            &build_candidate_live_state(&verification_failure),
            "status_changed",
            false,
        );
        assert_eq!(loss_details["decision"], "lost");
        assert_eq!(loss_details["verification_result"], "failed");
        assert_eq!(loss_details["explanation"], "cargo test failed");
    }

    #[test]
    fn format_live_event_line_includes_decision_and_explanation() {
        let event = LiveStatusEvent {
            event: "candidate_event",
            details: serde_json::json!({
                "transition": "status_changed",
                "phase": "verify",
                "agent_id": "agent-07",
                "status": "success",
                "file_path": "src/lib.rs",
                "source_lineage": "apply_edit",
                "outcome": "accepted",
                "decision": "won",
                "verification_result": "passed",
                "explanation": "highest-ranked verified candidate over 2 competing verified candidates; suppressed 1 duplicate verified output",
                "tokens_used": 700,
                "cost_usd": 0.21
            }),
        };

        let line = format_live_event_line(&event);
        assert!(line.contains("decision=won"));
        assert!(line.contains("why=highest-ranked verified candidate over 2 competing verified candidates; suppressed 1 duplicate verified output"));
        assert!(line.contains("verify=passed"));
    }

    #[test]
    fn classify_rust_repair_command_supports_env_prefixes_and_toolchains() {
        let target =
            classify_rust_repair_command("RUST_BACKTRACE=1 cargo +nightly test -p mercury-cli")
                .unwrap();
        assert_eq!(
            target.verifier_command,
            "RUST_BACKTRACE=1 cargo +nightly test -p mercury-cli"
        );
        assert_eq!(target.mode, RustRepairMode::TestLike);
    }

    #[test]
    fn classify_rust_repair_command_supports_env_wrapper_prefixes() {
        let target = classify_rust_repair_command("env RUST_BACKTRACE=1 cargo test --quiet")
            .expect("env wrapper command should be supported");
        assert_eq!(target.mode, RustRepairMode::TestLike);

        let clippy_target =
            classify_rust_repair_command("env -i RUSTFLAGS=-Dwarnings cargo clippy")
                .expect("env wrapper clippy command should be supported");
        assert_eq!(clippy_target.mode, RustRepairMode::Lint);
    }

    #[test]
    fn classify_rust_repair_command_rejects_shell_composition() {
        assert!(classify_rust_repair_command("cargo test && cargo clippy").is_none());
        assert!(classify_rust_repair_command("cargo test | tee out.txt").is_none());
    }

    #[test]
    fn classify_rust_repair_command_supports_direct_cargo_vectors() {
        let parts = [
            "env".to_string(),
            "RUST_BACKTRACE=1".to_string(),
            "cargo".to_string(),
            "check".to_string(),
            "--workspace".to_string(),
        ];
        let target =
            classify_rust_repair_command(&parts.join(" ")).expect("command should classify");
        assert_eq!(target.mode, RustRepairMode::TestLike);
        assert_eq!(
            target.verifier_command,
            "env RUST_BACKTRACE=1 cargo check --workspace"
        );
    }

    #[test]
    fn build_watch_verify_config_targets_only_the_requested_rust_verifier() {
        let config = sample_config();
        let clippy_target = classify_rust_repair_command("cargo clippy --workspace").unwrap();
        let clippy_verify = build_watch_verify_config(&config, &clippy_target);
        assert!(!clippy_verify.test_after_write);
        assert!(clippy_verify.lint_after_write);
        assert_eq!(clippy_verify.lint_command, "cargo clippy --workspace");

        let test_target = classify_rust_repair_command("cargo test -p mercury-cli").unwrap();
        let test_verify = build_watch_verify_config(&config, &test_target);
        assert!(test_verify.test_after_write);
        assert!(!test_verify.lint_after_write);
        assert_eq!(test_verify.test_command, "cargo test -p mercury-cli");
    }

    #[test]
    fn capture_repo_watch_snapshot_ignores_mercury_internal_state() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("src");
        let mercury_runs = temp.path().join(".mercury").join("runs");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&mercury_runs).unwrap();
        std::fs::write(source.join("main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(mercury_runs.join("watch.json"), "{}\n").unwrap();

        let before = capture_repo_watch_snapshot(temp.path()).unwrap();
        std::fs::write(mercury_runs.join("watch.json"), "{\"decision\":\"noop\"}\n").unwrap();
        let after = capture_repo_watch_snapshot(temp.path()).unwrap();

        assert_eq!(before, after);
    }

    #[test]
    fn verifier_allowlist_accepts_direct_rust_cargo_commands() {
        assert!(verifier_command_allowlisted("cargo test"));
        assert!(verifier_command_allowlisted(
            "RUST_BACKTRACE=1 env RUSTFLAGS=-Dwarnings cargo +nightly clippy --workspace"
        ));
        assert!(verifier_command_allowlisted(
            "env -i cargo check -p mercury-cli"
        ));
    }

    #[test]
    fn verifier_allowlist_rejects_shell_composition_and_non_cargo_commands() {
        assert!(!verifier_command_allowlisted("cargo test && cargo clippy"));
        assert!(!verifier_command_allowlisted("cargo test | tee out.txt"));
        assert!(!verifier_command_allowlisted("pytest -q"));
        assert!(!verifier_command_allowlisted(""));
    }

    #[test]
    fn redact_secrets_scrubs_marker_lines() {
        let input = "Authorization: Bearer abcdef\napi_key=12345\nnormal=value\n";
        let output = redact_secrets(input);
        assert!(output.contains("Authorization: [REDACTED]"));
        assert!(output.contains("api_key=[REDACTED]"));
        assert!(output.contains("normal=value"));
    }

    #[test]
    fn event_payload_redacts_nested_values() {
        let payload = event_payload(
            "test_event",
            serde_json::json!({
                "token_line": "Authorization: Bearer secret-token",
                "nested": {
                    "api_key": "api_key=12345",
                },
                "items": [
                    "normal=value",
                    "password: hunter2",
                ],
            }),
        );

        let details = &payload["details"];
        assert_eq!(payload["event"], Value::String("test_event".to_string()));
        assert_eq!(
            details["token_line"],
            Value::String("Authorization: [REDACTED]".to_string())
        );
        assert_eq!(
            details["nested"]["api_key"],
            Value::String("api_key=[REDACTED]".to_string())
        );
        assert_eq!(
            details["items"][1],
            Value::String("password: [REDACTED]".to_string())
        );
        assert_eq!(
            details["items"][0],
            Value::String("normal=value".to_string())
        );
    }
}
