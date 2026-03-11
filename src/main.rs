//! Mercury CLI — the first diffusion-native CLI for autonomous code synthesis.
//!
//! Uses Inception Labs' Mercury 2 with thermal heat maps as a stigmergic
//! coordination primitive for multi-agent code editing.

use std::collections::BTreeMap;
use std::env;
use std::fs::OpenOptions;
use std::io::IsTerminal;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, UNIX_EPOCH};

use anyhow::{Context, Result};
use chrono::Utc;
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use mercury_cli::api::{self, Mercury2Client, MercuryEditClient};
use mercury_cli::db::{self, ThermalDb};
use mercury_cli::engine::{
    self, Scheduler, SchedulerConfig, StepExecutionSummary, Verifier, VerifyConfig,
};
use mercury_cli::failure_parser::{self, CargoCommandKind};
use mercury_cli::repo;
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
        /// Stream status updates continuously in a TTY until Ctrl-C.
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
    /// Apply an instruction-based edit to a file.
    Apply {
        /// The file to edit.
        file: PathBuf,
        /// The edit instruction.
        #[arg(long)]
        instruction: String,
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
    /// Predict the next edit based on history.
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
    if !std::io::stdout().is_terminal() {
        anyhow::bail!("`status --live` requires a TTY terminal");
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to initialize runtime for live status")?;

    runtime.block_on(async {
        loop {
            let aggregates = db.get_all_aggregates()?;
            let active = db.get_active_agents()?;
            let state = db.get_swarm_state()?;

            print!("\x1B[2J\x1B[H");
            println!(
                "Mercury live status  |  {}  |  refresh={}ms  |  ctrl-c to stop\n",
                Utc::now().to_rfc3339(),
                interval_ms
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
                if let Some(state) = state {
                    println!("\nBudget:");
                    println!("  Total cost: ${:.4}", state.total_cost_usd);
                    println!("  Total tokens: {}", state.total_tokens_used);
                    println!("  Agents spawned: {}", state.total_agents_spawned);
                    println!("  Temperature: {:.2}", state.global_temperature);
                    println!("  Iteration: {}", state.iteration_count);
                } else {
                    println!("\nBudget:\n  No swarm session active.");
                }
            }

            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    println!("\nStopping live status.");
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
    let (plan, assessments) = planner.plan(goal, &repo_map_str).await?;

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

    if let Err(err) = enforce_verifier_allowlist(&verify_config) {
        let error = redact_secrets(&err.to_string());
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
    let (plan, assessments) = planner.plan(&planner_description, &repo_map_str).await?;
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
    let benchmark_verifier = FixBenchmarkVerifier {
        parse_before_write: verify_config.parse_before_write,
        test_after_write: verify_config.test_after_write,
        lint_after_write: verify_config.lint_after_write,
        test_command: redact_secrets(&verify_config.test_command),
        lint_command: redact_secrets(&verify_config.lint_command),
    };
    let execution_summary: StepExecutionSummary =
        engine::execute_plan_steps(&plan, &patcher, &verifier, &scheduler, db, project_root)
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
    write_json_artifact(
        &artifact_root.join("agent-logs.json"),
        &db.get_agent_logs()?,
    )?;
    write_json_artifact(&artifact_root.join("thermal-aggregates.json"), &aggregates)?;
    if let Some(state) = db.get_swarm_state()? {
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
    write_json_artifact(
        &artifact_root.join("metadata.json"),
        &FixRunMetadata {
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
        },
    )?;
    let diff_patch_path = artifact_root.join("diff.patch");
    if let Some(run_root) = execution_summary.run_root.as_ref() {
        copy_if_exists(&run_root.join("accepted.patch"), &diff_patch_path)?;
    }
    let accepted_patch_bytes = patch_size_bytes(&diff_patch_path)?;
    let accepted_patch = accepted_patch_bytes.is_some_and(|len| len > 0);
    write_json_artifact(
        &artifact_root.join("benchmark-run.json"),
        &FixBenchmarkRun {
            schema_version: FIX_BENCHMARK_RUN_SCHEMA_NAME.to_string(),
            description: description_redacted.clone(),
            started_at: started_at.to_rfc3339(),
            finished_at: finished_at.to_rfc3339(),
            duration_ms,
            accepted_steps: execution_summary.accepted,
            rejected_steps: execution_summary.rejected,
            verification_failures: execution_summary.verification_failures,
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
            false_green: false,
            sandbox_run_root: sandbox_run_root.clone(),
            total_cost_usd,
            budget_remaining_usd,
            verifier: benchmark_verifier,
            security: final_security.clone(),
        },
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
        artifact_root,
        execution_summary,
        total_cost_usd,
        budget_remaining_usd,
    })
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

    write_json_artifact(&artifact_root.join("watch.json"), &record)?;
    write_watch_output_artifacts(&artifact_root, &record)?;
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

#[derive(Debug, Serialize)]
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
    retry_attempts: usize,
    time_to_first_candidate_ms: Option<u64>,
    time_to_verified_repair_ms: Option<u64>,
    final_bundle_verified: bool,
    applied: bool,
    accepted_patch: bool,
    accepted_patch_bytes: Option<u64>,
    outcome: String,
    false_green: bool,
    sandbox_run_root: Option<String>,
    total_cost_usd: f64,
    budget_remaining_usd: f64,
    verifier: FixBenchmarkVerifier,
    security: SecurityRuntimeContext,
}

fn create_run_artifact_root(project_root: &Path) -> Result<PathBuf> {
    let run_id = format!("run-{}", Utc::now().format("%Y%m%dT%H%M%S%.3fZ"));
    let root = project_root.join(".mercury").join("runs").join(run_id);
    std::fs::create_dir_all(&root)?;
    Ok(root)
}

fn create_cli_worktree_root(project_root: &Path) -> Result<PathBuf> {
    let run_id = format!("edit-{}", Utc::now().format("%Y%m%dT%H%M%S%.3fZ"));
    let root = project_root.join(".mercury").join("worktrees").join(run_id);
    std::fs::create_dir_all(&root)?;
    Ok(root)
}

fn copy_repo_tree(source: &Path, destination: &Path, root: &Path) -> Result<()> {
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let relative = source_path
            .strip_prefix(root)
            .unwrap_or(source_path.as_path());

        if should_skip_repo_copy(relative) {
            continue;
        }

        let destination_path = destination.join(relative);
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            std::fs::create_dir_all(&destination_path)?;
            copy_repo_tree(&source_path, destination, root)?;
        } else if metadata.is_file() {
            if let Some(parent) = destination_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&source_path, &destination_path)?;
        }
    }

    Ok(())
}

fn should_skip_repo_copy(relative: &Path) -> bool {
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

fn file_relative_to_project(file: &Path, project_root: &Path) -> Result<PathBuf> {
    let full_path = if file.is_absolute() {
        file.to_path_buf()
    } else {
        project_root.join(file)
    };

    full_path
        .strip_prefix(project_root)
        .map(PathBuf::from)
        .with_context(|| {
            format!(
                "file {} must live under project root {}",
                file.display(),
                project_root.display()
            )
        })
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
                    instruction,
                    dry_run,
                    force,
                } => {
                    let content = std::fs::read_to_string(&file)?;
                    // For instruction-based edits, we pass the original as both
                    // original_code and update_snippet — Mercury Edit infers the
                    // change from context. For precise edits, callers provide
                    // the actual update snippet.
                    let (patched, usage) = patcher.patch(&content, &instruction).await?;
                    if dry_run {
                        println!("--- Dry run (not written) ---");
                        println!("{patched}");
                    } else if force {
                        println!("WARNING: verification bypassed due to --force");
                        std::fs::write(&file, &patched)?;
                        println!("Patched {}", file.display());
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
                        verify_and_accept_patch(&verifier, &file, &patched, &project_root).await?;
                        println!("Patched {}", file.display());
                    }
                    println!(
                        "Tokens: {} | Cost: ${:.4}",
                        usage.tokens_used, usage.cost_usd
                    );
                }
                EditAction::Complete { file } => {
                    let (path, line) = parse_file_line(&file);
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
                    let (path, _) = parse_file_line(&file);
                    let content = std::fs::read_to_string(&path)?;
                    let (result, usage) = patcher
                        .next_edit_with_path(path.to_string_lossy().as_ref(), &content, "")
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
                println!("Setting {key} = {value}");
                println!("(Config set not yet implemented — edit .mercury/config.toml directly)");
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

/// Parse "file.rs:42" into (PathBuf, line_number).
fn parse_file_line(input: &str) -> (PathBuf, u32) {
    if let Some((file, line)) = input.rsplit_once(':') {
        if let Ok(n) = line.parse::<u32>() {
            return (PathBuf::from(file), n);
        }
    }
    (PathBuf::from(input), 1)
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
    collect_repo_watch_snapshot(project_root, project_root, &mut files)?;
    Ok(RepoWatchSnapshot(files))
}

fn collect_repo_watch_snapshot(
    root: &Path,
    current: &Path,
    files: &mut BTreeMap<PathBuf, RepoFileFingerprint>,
) -> Result<()> {
    for entry in std::fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();
        let relative = path.strip_prefix(root).unwrap_or(path.as_path());

        if should_skip_watch_path(relative) {
            continue;
        }

        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            collect_repo_watch_snapshot(root, &path, files)?;
        } else if metadata.is_file() {
            let modified_unix_nanos = metadata
                .modified()
                .ok()
                .and_then(|mtime| mtime.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_nanos())
                .unwrap_or_default();
            files.insert(
                relative.to_path_buf(),
                RepoFileFingerprint {
                    modified_unix_nanos,
                    len: metadata.len(),
                },
            );
        }
    }

    Ok(())
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
    file: &Path,
    patched_content: &str,
    project_root: &Path,
) -> Result<()> {
    let sandbox_root = create_cli_worktree_root(project_root)?;
    copy_repo_tree(project_root, &sandbox_root, project_root)?;
    let relative_path = file_relative_to_project(file, project_root)?;
    let sandbox_file = sandbox_root.join(&relative_path);

    if let Some(parent) = sandbox_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&sandbox_file, patched_content)?;

    let verification = verifier
        .verify(&sandbox_file, patched_content, &sandbox_root)
        .await?;
    if verification.is_ok() {
        atomic_write_string(&project_root.join(&relative_path), patched_content)?;
        let _ = std::fs::remove_dir_all(&sandbox_root);
        return Ok(());
    }
    let _ = std::fs::remove_dir_all(&sandbox_root);

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
        file.display(),
        serde_json::to_string_pretty(&structured)?
    );
}

#[cfg(test)]
mod tests {
    use super::*;
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

    #[tokio::test]
    async fn verify_failure_does_not_mutate_user_worktree() {
        let temp = tempdir().unwrap();
        let file = temp.path().join("sample.rs");
        let original = "fn main() { println!(\"hello\"); }\n";
        let patched = "fn main() { println!(\"goodbye\"); }\n";
        std::fs::write(&file, original).unwrap();

        let verifier = Verifier::<Mercury2Client>::new(verification_config_that_fails(), None);
        let result = verify_and_accept_patch(&verifier, &file, patched, temp.path()).await;

        assert!(result.is_err());
        let after = std::fs::read_to_string(&file).unwrap();
        assert_eq!(after, original);
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
