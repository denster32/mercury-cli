//! Mercury CLI — the first diffusion-native CLI for autonomous code synthesis.
//!
//! Uses Inception Labs' Mercury 2 with thermal heat maps as a stigmergic
//! coordination primitive for multi-agent code editing.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, UNIX_EPOCH};

use anyhow::{Context, Result};
use chrono::Utc;
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
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
    },

    /// Watch a shell command and auto-repair failures.
    Watch {
        /// The shell command to watch.
        command: String,
        /// Auto-repair failures.
        #[arg(long)]
        repair: bool,
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

fn cmd_status(db: &ThermalDb, heatmap: bool, agents: bool, budget: bool) -> Result<()> {
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
) -> Result<()> {
    let _ = cmd_fix_with_verify_config(
        config,
        db,
        project_root,
        description,
        max_agents,
        max_cost,
        build_verify_config(config),
        None,
    )
    .await?;
    Ok(())
}

async fn cmd_fix_with_verify_config(
    config: &MercuryConfig,
    db: &ThermalDb,
    project_root: &Path,
    description: &str,
    max_agents: usize,
    max_cost: f64,
    verify_config: VerifyConfig,
    parsed_failure: Option<&failure_parser::ParsedFailureReport>,
) -> Result<FixCommandOutcome> {
    let started_at = Utc::now();
    let started = Instant::now();
    let artifact_root = create_run_artifact_root(project_root)?;

    println!("Mercury Fix: {description}");
    println!("  Max agents: {max_agents}");
    println!("  Max cost: ${max_cost:.2}");
    println!("  Artifact bundle: {}", artifact_root.display());
    println!();

    // Step 1: Init swarm session
    let _swarm_id = db.init_swarm()?;

    // Step 2: Index
    println!("[1/7] Indexing repository...");
    let repo_languages = config.repo_languages();
    let repo_map =
        repo::build_repo_map_with_languages(&project_root.to_string_lossy(), &repo_languages)?;
    let repo_map_str = repo::format_repo_map(&repo_map);

    // Step 3: Plan
    println!("[2/7] Planning with Mercury 2...");
    let api_key = api::resolve_api_key(&config.api.api_key_env)?;

    println!("  Grounding repair context...");
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
    println!(
        "  Plan estimate: ${:.4}{}",
        plan.estimated_cost,
        plan.estimated_tokens
            .map(|tokens| format!(" | tokens: {tokens}"))
            .unwrap_or_default()
    );

    store_assessments(db, &plan.steps, &assessments, "fix")?;
    print_assessment_summary(&plan.steps, &assessments);

    // Run initial merge
    let mut sched_config = config.to_scheduler_config();
    sched_config.max_cost_per_command = max_cost;
    sched_config.max_concurrency = max_agents;
    let scheduler = Scheduler::new(sched_config);
    scheduler.run_merge_cycle(db, config.annealing.initial_temperature)?;

    // Step 4: Scaffold (cool zones)
    println!("[3/7] Scaffolding cool zones...");
    let cool_zones = db.zones_below(config.thermal.cool_threshold)?;
    println!("  {} cool zone files identified", cool_zones.len());

    // Step 5: Resolve (hot zones)
    println!("[4/7] Resolving hot zones...");
    let hot_zones = db.zones_above(config.thermal.hot_threshold)?;
    println!("  {} hot zone files identified", hot_zones.len());

    // Step 6: Execute planned edits with verification gating
    println!("[5/7] Patching and verifying plan steps...");
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
        verify_config,
        if config.verification.mercury2_critique_on_failure {
            Some(verifier_client)
        } else {
            None
        },
    );
    let execution_summary: StepExecutionSummary =
        engine::execute_plan_steps(&plan, &patcher, &verifier, &scheduler, db, project_root)
            .await?;

    // Step 7: Anneal + report
    println!("[6/7] Annealing...");
    scheduler.run_decay_cycle(db, config.thermal.decay_half_life_seconds)?;
    scheduler.run_merge_cycle(db, config.annealing.initial_temperature)?;

    println!("[7/7] Complete!");

    let aggregates = db.get_all_aggregates()?;
    if !aggregates.is_empty() {
        let active = db.get_active_agents()?;
        println!("\nFinal Thermal Map:");
        println!("{}", render_heatmap_to_string(&aggregates, &active));
    }

    println!("\nExecution summary:");
    println!("  Accepted steps: {}", execution_summary.accepted);
    println!("  Rejected steps: {}", execution_summary.rejected);
    println!(
        "  Verification failures: {}",
        execution_summary.verification_failures
    );

    println!("\nCost: ${:.4}", scheduler.current_cost());
    println!("Budget remaining: ${:.4}", scheduler.budget_remaining());

    let finished_at = Utc::now();
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
    write_json_artifact(
        &artifact_root.join("metadata.json"),
        &FixRunMetadata {
            description: description.to_string(),
            max_agents,
            max_cost,
            started_at: started_at.to_rfc3339(),
            finished_at: finished_at.to_rfc3339(),
            duration_ms: started.elapsed().as_millis() as u64,
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
            sandbox_run_root: execution_summary
                .run_root
                .as_ref()
                .map(|path| path.display().to_string()),
            total_cost_usd,
            budget_remaining_usd,
        },
    )?;
    if let Some(run_root) = execution_summary.run_root.as_ref() {
        copy_if_exists(
            &run_root.join("accepted.patch"),
            &artifact_root.join("diff.patch"),
        )?;
    }
    println!("Artifacts written to {}", artifact_root.display());

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
) -> Result<()> {
    println!("Watching: {command}");
    if repair {
        println!("Auto-repair enabled for supported Rust verifier commands.");
    } else {
        println!("Auto-repair disabled. Failures will be reported only.");
    }
    println!("Press Ctrl-C to stop.\n");

    let mut snapshot = capture_repo_watch_snapshot(project_root)?;
    let mut cycle = 0usize;

    loop {
        cycle += 1;
        if cycle > 1 {
            println!("Waiting for repository changes...");
            if !wait_for_repo_change(project_root, &mut snapshot).await? {
                return Ok(());
            }
            println!("Change detected. Re-running watch cycle.\n");
        }

        let outcome = execute_watch_cycle(config, db, project_root, command, repair).await?;
        print_watch_cycle_result(&outcome);
        snapshot = capture_repo_watch_snapshot(project_root)?;
        println!();
    }
}

async fn execute_watch_cycle(
    config: &MercuryConfig,
    db: &ThermalDb,
    project_root: &Path,
    command: &str,
    repair: bool,
) -> Result<WatchCycleOutcome> {
    let started_at = Utc::now();
    let started = Instant::now();
    let artifact_root = create_run_artifact_root(project_root)?;

    println!("Running watch command...");
    let initial_run = run_watch_command(command, project_root)?;
    replay_watch_command_output("initial", &initial_run);

    let mut decision = "passed_without_repair".to_string();
    let mut repair_record = None;
    let mut confirmation_run = None;

    if !initial_run.success {
        if !repair {
            decision = "failed_without_repair".to_string();
        } else if let Some(target) = classify_rust_repair_command(command) {
            let verify_config = build_watch_verify_config(config, &target);
            let description = build_watch_repair_description(command, &initial_run);
            println!(
                "Watch repair targeting `{}` with Mercury fix...",
                target.verifier_command
            );
            match cmd_fix_with_verify_config(
                config,
                db,
                project_root,
                &description,
                config.scheduler.max_concurrency,
                config.scheduler.max_cost_per_command,
                verify_config,
                initial_run.parsed_failure.as_ref(),
            )
            .await
            {
                Ok(outcome) => {
                    repair_record = Some(WatchRepairRecord {
                        supported: true,
                        verifier_command: Some(target.verifier_command.clone()),
                        fix_artifact_root: Some(outcome.artifact_root.display().to_string()),
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
                        &outcome.artifact_root.join("plan.json"),
                        &artifact_root.join("repair").join("plan.json"),
                    )?;
                    copy_if_exists(
                        &outcome.artifact_root.join("grounded-context.json"),
                        &artifact_root.join("repair").join("grounded-context.json"),
                    )?;

                    println!(
                        "Repair attempt cost: ${:.4} | budget remaining: ${:.4}",
                        outcome.total_cost_usd, outcome.budget_remaining_usd
                    );
                    println!("Re-running watch command after repair...");
                    let rerun = run_watch_command(command, project_root)?;
                    replay_watch_command_output("confirmation", &rerun);
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
                        verifier_command: Some(target.verifier_command.clone()),
                        fix_artifact_root: None,
                        accepted_steps: 0,
                        rejected_steps: 0,
                        verification_failures: 0,
                        final_bundle_verified: false,
                        applied: false,
                        error: Some(err.to_string()),
                    });
                    decision = "repair_flow_failed".to_string();
                }
            }
        } else {
            repair_record = Some(WatchRepairRecord {
                supported: false,
                verifier_command: None,
                fix_artifact_root: None,
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
        command: command.to_string(),
        repair_requested: repair,
        decision,
        initial_run,
        confirmation_run,
        repair: repair_record,
        started_at: started_at.to_rfc3339(),
        finished_at: finished_at.to_rfc3339(),
        duration_ms: started.elapsed().as_millis() as u64,
    };

    write_json_artifact(&artifact_root.join("watch.json"), &record)?;
    write_watch_output_artifacts(&artifact_root, &record)?;

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
            agents,
            budget,
        } => {
            let (_config, db, _root) = load_project()?;
            cmd_status(&db, heatmap, agents, budget)?;
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
        } => {
            let (config, db, root) = load_project()?;
            cmd_fix(&config, &db, &root, &description, max_agents, max_cost).await?;
        }

        Commands::Watch { command, repair } => {
            let (config, db, root) = load_project()?;
            cmd_watch(&config, &db, &root, &command, repair).await?;
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

fn run_watch_command(command: &str, working_dir: &Path) -> Result<WatchCommandResult> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let output = run_watch_command_with_shell(&shell, command, working_dir)
        .or_else(|_| run_watch_command_with_shell("/bin/sh", command, working_dir))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let parsed_failure = if output.status.success() {
        None
    } else {
        parse_supported_watch_failure(command, &stdout, &stderr)
    };

    Ok(WatchCommandResult {
        command: command.to_string(),
        success: output.status.success(),
        exit_code: output.status.code(),
        stdout,
        stderr,
        parsed_failure,
    })
}

fn parse_supported_watch_failure(
    command: &str,
    stdout: &str,
    stderr: &str,
) -> Option<mercury_cli::failure_parser::ParsedFailureReport> {
    if failure_parser::contains_shell_composition(command) {
        return None;
    }
    let parts = failure_parser::parse_command_parts(command);
    let command_kind = failure_parser::classify_cargo_command(&parts);
    if matches!(command_kind, CargoCommandKind::Unknown) {
        return None;
    }
    Some(failure_parser::parse_cargo_failure(&parts, stdout, stderr))
}

fn run_watch_command_with_shell(
    shell: &str,
    command: &str,
    working_dir: &Path,
) -> Result<std::process::Output> {
    Command::new(shell)
        .arg("-lc")
        .arg(command)
        .current_dir(working_dir)
        .output()
        .with_context(|| format!("failed to execute watch command with shell `{shell}`"))
}

fn replay_watch_command_output(label: &str, result: &WatchCommandResult) {
    let status = result
        .exit_code
        .map(|code| code.to_string())
        .unwrap_or_else(|| "signal".to_string());
    println!(
        "[watch:{label}] exit={status} success={}",
        if result.success { "true" } else { "false" }
    );
    if !result.stdout.is_empty() {
        print!("{}", result.stdout);
    }
    if !result.stderr.is_empty() {
        eprint!("{}", result.stderr);
    }
}

fn print_watch_cycle_result(outcome: &WatchCycleOutcome) {
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
}
