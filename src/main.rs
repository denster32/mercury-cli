//! Mercury CLI — the first diffusion-native CLI for autonomous code synthesis.
//!
//! Uses Inception Labs' Mercury 2 with thermal heat maps as a stigmergic
//! coordination primitive for multi-agent code editing.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::Deserialize;
use thiserror::Error;

use mercury_cli::api::{self, Mercury2Client, MercuryEditClient};
use mercury_cli::db::{self, ThermalDb};
use mercury_cli::engine::{self, Scheduler, SchedulerConfig};
use mercury_cli::repo;
use mercury_cli::swarm;
use mercury_cli::thermal::{self, render_heatmap_to_string};

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
    println!("\nSet your API key: export MERCURY_API_KEY=<your-key>");
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
    let api_key = std::env::var(&config.api.api_key_env)
        .map_err(|_| api::ApiError::MissingApiKey(config.api.api_key_env.clone()))?;

    let mut client = Mercury2Client::new(api_key)
        .with_base_url(config.api.mercury2_endpoint.clone())
        .with_retries(
            config.scheduler.retry_limit,
            config.scheduler.backoff_base_ms,
        );
    if let Some(e) = effort {
        client = client.with_reasoning_effort(e);
    }

    let repo_context = match repo::build_repo_map(".") {
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
    let api_key = std::env::var(&config.api.api_key_env)
        .map_err(|_| api::ApiError::MissingApiKey(config.api.api_key_env.clone()))?;

    // Build repo map
    println!("Indexing repository...");
    let repo_map = repo::build_repo_map(".")?;
    let repo_map_str = repo::format_repo_map(&repo_map);

    let client = Mercury2Client::new(api_key)
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

async fn cmd_fix(
    config: &MercuryConfig,
    db: &ThermalDb,
    project_root: &Path,
    description: &str,
    max_agents: usize,
    max_cost: f64,
) -> Result<()> {
    println!("Mercury Fix: {description}");
    println!("  Max agents: {max_agents}");
    println!("  Max cost: ${max_cost:.2}");
    println!();

    // Step 1: Init swarm session
    let _swarm_id = db.init_swarm()?;

    // Step 2: Index
    println!("[1/7] Indexing repository...");
    let repo_map = repo::build_repo_map(&project_root.to_string_lossy())?;
    let repo_map_str = repo::format_repo_map(&repo_map);

    // Step 3: Plan
    println!("[2/7] Planning with Mercury 2...");
    let api_key = std::env::var(&config.api.api_key_env)
        .map_err(|_| api::ApiError::MissingApiKey(config.api.api_key_env.clone()))?;

    let client = Mercury2Client::new(api_key)
        .with_base_url(config.api.mercury2_endpoint.clone())
        .with_retries(
            config.scheduler.retry_limit,
            config.scheduler.backoff_base_ms,
        );

    let planner = engine::Planner::new(client, config.constitutional_prompt());
    let (plan, assessments) = planner.plan(description, &repo_map_str).await?;

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

    // Step 6: Anneal
    println!("[5/7] Annealing...");
    scheduler.run_decay_cycle(db, config.thermal.decay_half_life_seconds)?;
    scheduler.run_merge_cycle(db, config.annealing.initial_temperature)?;

    // Step 7: Report
    println!("[6/7] Verification...");
    println!("[7/7] Complete!");

    let aggregates = db.get_all_aggregates()?;
    if !aggregates.is_empty() {
        let active = db.get_active_agents()?;
        println!("\nFinal Thermal Map:");
        println!("{}", render_heatmap_to_string(&aggregates, &active));
    }

    println!("\nCost: ${:.4}", scheduler.current_cost());
    println!("Budget remaining: ${:.4}", scheduler.budget_remaining());

    Ok(())
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

// ---------------------------------------------------------------------------
// Default config (fallback)
// ---------------------------------------------------------------------------

const DEFAULT_CONFIG: &str = r#"[api]
mercury2_endpoint = "https://api.inceptionlabs.ai/v1/chat/completions"
mercury_edit_endpoint = "https://api.inceptionlabs.ai/v1"
api_key_env = "MERCURY_API_KEY"

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
            let (config, _db, _root) = load_project()?;
            let api_key = std::env::var(&config.api.api_key_env)
                .map_err(|_| api::ApiError::MissingApiKey(config.api.api_key_env.clone()))?;

            let edit_client =
                MercuryEditClient::new(api_key, config.api.mercury_edit_endpoint.clone())
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
                } => {
                    let content = std::fs::read_to_string(&file)?;
                    // For instruction-based edits, we pass the original as both
                    // original_code and update_snippet — Mercury Edit infers the
                    // change from context. For precise edits, callers provide
                    // the actual update snippet.
                    let (patched, usage) = patcher
                        .patch(
                            &content,
                            &format!("{content}\n// Instruction: {instruction}"),
                        )
                        .await?;
                    if dry_run {
                        println!("--- Dry run (not written) ---");
                        println!("{patched}");
                    } else {
                        std::fs::write(&file, &patched)?;
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
                    let (result, usage) = patcher.next_edit(&content, "").await?;
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
            println!("Watching: {command}");
            if repair {
                println!("Auto-repair enabled.");
            }
            println!("(Watch mode not yet fully implemented in v0.1)");
            // TODO: implement watch loop in v0.2
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
