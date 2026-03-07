//! Swarm — agent lifecycle, spawner, monitor, and density controls.
//!
//! This module manages the swarm of agents that execute the thermal
//! gradient-driven code synthesis workflow.

use std::collections::HashMap;

use thiserror::Error;
use uuid::Uuid;

use crate::db::{DbError, ThermalDb};
use crate::engine::EngineError;

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

/// Errors originating from the swarm layer.
#[derive(Error, Debug)]
pub enum SwarmError {
    #[error("database error: {0}")]
    Db(#[from] DbError),

    #[error("engine error: {0}")]
    Engine(#[from] EngineError),

    #[error("agent {0} not found")]
    AgentNotFound(String),

    #[error("max density reached for {file}: {current}/{max}")]
    DensityExceeded {
        file: String,
        current: i32,
        max: i32,
    },

    #[error("swarm budget exhausted")]
    BudgetExhausted,

    #[error("oscillation detected on {file}: {count} flip-flops")]
    OscillationDetected { file: String, count: u32 },
}

// ---------------------------------------------------------------------------
// Agent types
// ---------------------------------------------------------------------------

/// The lifecycle status of a swarm agent.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentStatus {
    Spawned,
    Running,
    Success,
    Failed(String),
    Retrying(u32),
}

impl AgentStatus {
    /// Convert to the string representation used in the database.
    pub fn as_db_str(&self) -> &str {
        match self {
            AgentStatus::Spawned => "spawned",
            AgentStatus::Running => "running",
            AgentStatus::Success => "success",
            AgentStatus::Failed(_) => "failed",
            AgentStatus::Retrying(_) => "retrying",
        }
    }
}

/// The role an agent plays in the swarm.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentRole {
    /// Works on cool-zone files (fast, cheap, establishes scaffolding).
    CoolZone,
    /// Works on hot-zone files (more iterations, higher cost).
    HotZone,
    /// Monitors other agents for conflicts and oscillation.
    Monitor,
}

/// A single agent in the swarm.
#[derive(Debug, Clone)]
pub struct Agent {
    pub id: String,
    pub role: AgentRole,
    pub target_file: String,
    pub status: AgentStatus,
    pub constitutional_prompt: String,
    pub tokens_used: i64,
    pub cost_usd: f64,
    pub retry_count: u32,
    pub db_log_id: Option<i64>,
}

impl Agent {
    /// Create a new cool-zone agent.
    pub fn new_cool(target_file: &str, constitutional_prompt: &str) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            role: AgentRole::CoolZone,
            target_file: target_file.to_string(),
            status: AgentStatus::Spawned,
            constitutional_prompt: constitutional_prompt.to_string(),
            tokens_used: 0,
            cost_usd: 0.0,
            retry_count: 0,
            db_log_id: None,
        }
    }

    /// Create a new hot-zone agent.
    pub fn new_hot(target_file: &str, constitutional_prompt: &str) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            role: AgentRole::HotZone,
            target_file: target_file.to_string(),
            status: AgentStatus::Spawned,
            constitutional_prompt: constitutional_prompt.to_string(),
            tokens_used: 0,
            cost_usd: 0.0,
            retry_count: 0,
            db_log_id: None,
        }
    }

    /// Create a new monitor agent.
    pub fn new_monitor(index: usize, constitutional_prompt: &str) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            role: AgentRole::Monitor,
            target_file: format!("monitor-{index}"),
            status: AgentStatus::Spawned,
            constitutional_prompt: constitutional_prompt.to_string(),
            tokens_used: 0,
            cost_usd: 0.0,
            retry_count: 0,
            db_log_id: None,
        }
    }

    /// Check if the agent is still active.
    pub fn is_active(&self) -> bool {
        matches!(
            self.status,
            AgentStatus::Spawned | AgentStatus::Running | AgentStatus::Retrying(_)
        )
    }
}

// ---------------------------------------------------------------------------
// Spawner
// ---------------------------------------------------------------------------

/// Configuration for the swarm spawner.
#[derive(Debug, Clone)]
pub struct SpawnerConfig {
    pub max_agents_per_command: usize,
    pub max_density_per_file: i32,
    pub monitor_ratio: usize,
}

impl Default for SpawnerConfig {
    fn default() -> Self {
        Self {
            max_agents_per_command: 100,
            max_density_per_file: 5,
            monitor_ratio: 10,
        }
    }
}

/// Spawner: allocates agents based on thermal gradient.
pub struct Spawner {
    config: SpawnerConfig,
}

impl Spawner {
    /// Create a new Spawner.
    pub fn new(config: SpawnerConfig) -> Self {
        Self { config }
    }

    /// Spawn a set of agents for the given execution phase.
    ///
    /// Cool zones get agents first (cheap, establish scaffolding).
    /// Hot zones get agents with higher density.
    /// Monitor agents watch for conflicts.
    pub fn spawn_agents(
        &self,
        db: &ThermalDb,
        available_slots: usize,
        constitutional_prompt: &str,
        hot_threshold: f64,
        cool_threshold: f64,
    ) -> Result<Vec<Agent>, SwarmError> {
        let mut agents = Vec::new();

        let cool_zones = db.zones_below(cool_threshold)?;
        let hot_zones = db.zones_above(hot_threshold)?;

        // Phase 1: cool-zone agents (up to half the slots)
        let cool_slots = available_slots / 2;
        for zone in cool_zones.iter().take(cool_slots) {
            if zone.is_locked {
                continue;
            }
            if zone.agent_density >= self.config.max_density_per_file {
                continue;
            }
            agents.push(Agent::new_cool(&zone.file_path, constitutional_prompt));
        }

        // Phase 2: hot-zone agents (remaining slots)
        let remaining = available_slots.saturating_sub(agents.len());
        for zone in hot_zones.iter().take(remaining) {
            if zone.agent_density >= self.config.max_density_per_file {
                continue;
            }
            agents.push(Agent::new_hot(&zone.file_path, constitutional_prompt));
        }

        // Phase 3: monitor agents (1 per monitor_ratio execution agents)
        let execution_count = agents.len();
        if self.config.monitor_ratio > 0 {
            let monitor_count = execution_count / self.config.monitor_ratio;
            for i in 0..monitor_count {
                agents.push(Agent::new_monitor(i, constitutional_prompt));
            }
        }

        // Cap total agents
        agents.truncate(self.config.max_agents_per_command);

        Ok(agents)
    }

    /// Register spawned agents in the database.
    pub fn register_agents(
        &self,
        db: &ThermalDb,
        agents: &mut [Agent],
        command: &str,
    ) -> Result<(), SwarmError> {
        for agent in agents.iter_mut() {
            let log_id = db.log_agent_spawn(&agent.id, command, &agent.target_file)?;
            agent.db_log_id = Some(log_id);
            if agent.role != AgentRole::Monitor {
                db.increment_density(&agent.target_file)?;
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Monitor
// ---------------------------------------------------------------------------

/// Monitor: detects conflicts, oscillation, and semantic drift.
pub struct Monitor {
    /// Track score history per file to detect oscillation.
    score_history: HashMap<String, Vec<f64>>,
    /// Oscillation threshold: if score flips direction this many times, flag it.
    oscillation_threshold: u32,
}

impl Monitor {
    /// Create a new Monitor.
    pub fn new(oscillation_threshold: u32) -> Self {
        Self {
            score_history: HashMap::new(),
            oscillation_threshold,
        }
    }

    /// Record a new score for a file and check for oscillation.
    pub fn record_score(&mut self, file_path: &str, score: f64) -> Option<SwarmError> {
        let history = self.score_history.entry(file_path.to_string()).or_default();
        history.push(score);

        if history.len() < 3 {
            return None;
        }

        // Count direction changes
        let mut flips = 0u32;
        for window in history.windows(3) {
            let d1 = window[1] - window[0];
            let d2 = window[2] - window[1];
            if d1 * d2 < 0.0 {
                flips += 1;
            }
        }

        if flips >= self.oscillation_threshold {
            return Some(SwarmError::OscillationDetected {
                file: file_path.to_string(),
                count: flips,
            });
        }

        None
    }

    /// Check if two agents are conflicting (targeting the same file).
    pub fn detect_conflicts(agents: &[Agent]) -> Vec<(String, Vec<String>)> {
        let mut file_agents: HashMap<String, Vec<String>> = HashMap::new();
        for agent in agents {
            if agent.is_active() && agent.role != AgentRole::Monitor {
                file_agents
                    .entry(agent.target_file.clone())
                    .or_default()
                    .push(agent.id.clone());
            }
        }

        file_agents
            .into_iter()
            .filter(|(_, ids)| ids.len() > 1)
            .collect()
    }
}

// ---------------------------------------------------------------------------
// DensityController
// ---------------------------------------------------------------------------

/// DensityController: manages agents-per-file limits.
pub struct DensityController {
    max_density: i32,
}

impl DensityController {
    /// Create a new DensityController.
    pub fn new(max_density: i32) -> Self {
        Self { max_density }
    }

    /// Calculate the maximum allowed density for a given thermal score.
    /// Hotter files allow more concurrent agents.
    pub fn max_density_for_score(&self, score: f64) -> i32 {
        // Linear scaling: score 0.0 → 1 agent, score 1.0 → max_density
        let density = 1.0 + (self.max_density as f64 - 1.0) * score;
        (density as i32).max(1).min(self.max_density)
    }

    /// Check if a file can accept another agent.
    pub fn can_add_agent(&self, current_density: i32, score: f64) -> bool {
        current_density < self.max_density_for_score(score)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_agent_lifecycle() {
        let agent = Agent::new_cool("src/main.rs", "be careful");
        assert_eq!(agent.status, AgentStatus::Spawned);
        assert!(agent.is_active());
        assert_eq!(agent.role, AgentRole::CoolZone);
    }

    #[test]
    fn test_agent_status_db_str() {
        assert_eq!(AgentStatus::Spawned.as_db_str(), "spawned");
        assert_eq!(AgentStatus::Running.as_db_str(), "running");
        assert_eq!(AgentStatus::Success.as_db_str(), "success");
        assert_eq!(AgentStatus::Failed("err".to_string()).as_db_str(), "failed");
        assert_eq!(AgentStatus::Retrying(2).as_db_str(), "retrying");
    }

    #[test]
    fn test_density_controller() {
        let dc = DensityController::new(5);
        assert_eq!(dc.max_density_for_score(0.0), 1);
        assert_eq!(dc.max_density_for_score(1.0), 5);
        assert!(dc.can_add_agent(0, 0.5));
        assert!(!dc.can_add_agent(5, 0.5));
    }

    #[test]
    fn test_monitor_no_oscillation_monotonic() {
        // Monotonically increasing — should never detect oscillation
        let mut monitor = Monitor::new(2);
        assert!(monitor.record_score("a.rs", 0.1).is_none());
        assert!(monitor.record_score("a.rs", 0.3).is_none());
        assert!(monitor.record_score("a.rs", 0.5).is_none());
        assert!(monitor.record_score("a.rs", 0.7).is_none());
        assert!(monitor.record_score("a.rs", 0.9).is_none());
    }

    #[test]
    fn test_monitor_oscillation_detected() {
        // Alternating up/down — should detect oscillation
        let mut monitor = Monitor::new(2);
        assert!(monitor.record_score("a.rs", 0.5).is_none()); // 1 point
        assert!(monitor.record_score("a.rs", 0.7).is_none()); // 2 points
        assert!(monitor.record_score("a.rs", 0.4).is_none()); // 3 points, 1 flip (window: 0.5,0.7,0.4)
                                                              // 4 points: windows (0.5,0.7,0.4)=flip, (0.7,0.4,0.8)=flip → 2 flips >= threshold=2
        let result = monitor.record_score("a.rs", 0.8);
        assert!(result.is_some());
    }

    #[test]
    fn test_conflict_detection() {
        let agents = vec![
            Agent::new_hot("src/a.rs", ""),
            Agent::new_hot("src/a.rs", ""),
            Agent::new_cool("src/b.rs", ""),
        ];
        let conflicts = Monitor::detect_conflicts(&agents);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].0, "src/a.rs");
        assert_eq!(conflicts[0].1.len(), 2);
    }

    #[test]
    fn test_spawner_basic() {
        let db = ThermalDb::in_memory().unwrap();
        db.upsert_aggregate("cool.rs", 0.1, 0.2, 0).unwrap();
        db.upsert_aggregate("hot.rs", 0.85, 0.95, 0).unwrap();

        let spawner = Spawner::new(SpawnerConfig::default());
        let agents = spawner
            .spawn_agents(&db, 10, "constitutional", 0.7, 0.3)
            .unwrap();

        assert!(!agents.is_empty());
        // Should have at least one cool and one hot agent
        let has_cool = agents.iter().any(|a| a.role == AgentRole::CoolZone);
        let has_hot = agents.iter().any(|a| a.role == AgentRole::HotZone);
        assert!(has_cool);
        assert!(has_hot);
    }
}
