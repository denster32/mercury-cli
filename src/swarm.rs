//! Swarm — agent lifecycle, spawner, monitor, and density controls.
//!
//! This module manages the swarm of agents that execute the thermal
//! gradient-driven code synthesis workflow.

use std::collections::{HashMap, HashSet};

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
// Dispatch support
// ---------------------------------------------------------------------------

/// Work item that can be assigned to an execution agent.
#[derive(Debug, Clone, PartialEq)]
pub struct CandidateWorkItem {
    pub candidate_id: String,
    pub target_file: String,
    /// Higher scores are dispatched first.
    pub priority_score: f64,
}

impl CandidateWorkItem {
    pub fn new(
        candidate_id: impl Into<String>,
        target_file: impl Into<String>,
        priority_score: f64,
    ) -> Self {
        Self {
            candidate_id: candidate_id.into(),
            target_file: target_file.into(),
            priority_score,
        }
    }
}

/// Assignment of a candidate to a specific agent.
#[derive(Debug, Clone, PartialEq)]
pub struct CandidateAssignment {
    pub candidate_id: String,
    pub agent_id: String,
    pub target_file: String,
    /// 0-based ordering of the candidate in dispatch order for this agent.
    pub queue_position: usize,
}

/// Swarm-wide assignment plan from a global dispatch cycle.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DispatchPlan {
    pub assignments: Vec<CandidateAssignment>,
    pub unassigned_candidates: Vec<String>,
    pub saturated_agents: Vec<String>,
}

/// Coarse backpressure state for a file-level dispatch queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchBackpressure {
    Available,
    Limited,
    Saturated,
    BlockedNoAgent,
}

/// Executor-facing summary of one agent's queue after a dispatch cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDispatchSummary {
    pub agent_id: String,
    pub target_file: String,
    pub assigned_candidate_ids: Vec<String>,
    pub queue_depth: usize,
    pub remaining_capacity: usize,
    pub saturated: bool,
}

/// Executor-facing summary of one file's dispatch state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDispatchSummary {
    pub target_file: String,
    pub assigned_candidate_ids: Vec<String>,
    pub unassigned_candidate_ids: Vec<String>,
    pub eligible_agent_ids: Vec<String>,
    pub saturated_agent_ids: Vec<String>,
    pub max_queue_depth: usize,
    pub remaining_capacity: usize,
    pub backpressure: DispatchBackpressure,
}

/// Build deterministic candidate-to-agent assignments for a dispatch cycle.
///
/// Rules:
/// - monitor agents are ignored
/// - only active agents are considered assignable
/// - candidates are sorted by priority desc, then candidate_id asc
/// - candidates are assigned only to agents targeting the same file
/// - round-robin balances candidates across agents per file
pub fn build_dispatch_plan(
    candidates: &[CandidateWorkItem],
    agents: &[Agent],
    max_assignments_per_agent: usize,
) -> DispatchPlan {
    if max_assignments_per_agent == 0 {
        return DispatchPlan {
            assignments: Vec::new(),
            unassigned_candidates: candidates
                .iter()
                .map(|candidate| candidate.candidate_id.clone())
                .collect(),
            saturated_agents: Vec::new(),
        };
    }

    let mut assignable_agents_by_file: HashMap<&str, Vec<&Agent>> = HashMap::new();
    for agent in agents
        .iter()
        .filter(|agent| agent.is_active() && agent.role != AgentRole::Monitor)
    {
        assignable_agents_by_file
            .entry(agent.target_file.as_str())
            .or_default()
            .push(agent);
    }

    let mut sorted_candidates = candidates.to_vec();
    sorted_candidates.sort_by(|left, right| {
        right
            .priority_score
            .partial_cmp(&left.priority_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.candidate_id.cmp(&right.candidate_id))
    });

    let mut agent_load: HashMap<&str, usize> = HashMap::new();
    let mut per_file_rr: HashMap<String, usize> = HashMap::new();
    let mut assignments = Vec::new();
    let mut unassigned_candidates = Vec::new();

    for candidate in sorted_candidates {
        let Some(file_agents) = assignable_agents_by_file.get(candidate.target_file.as_str())
        else {
            unassigned_candidates.push(candidate.candidate_id);
            continue;
        };

        if file_agents.is_empty() {
            unassigned_candidates.push(candidate.candidate_id);
            continue;
        }

        let start_offset = per_file_rr
            .get(&candidate.target_file)
            .copied()
            .unwrap_or(0);
        let mut chosen_agent = None;

        for offset in 0..file_agents.len() {
            let idx = (start_offset + offset) % file_agents.len();
            let agent = file_agents[idx];
            let load = agent_load.get(agent.id.as_str()).copied().unwrap_or(0);
            if load < max_assignments_per_agent {
                chosen_agent = Some((idx, agent, load));
                break;
            }
        }

        match chosen_agent {
            Some((idx, agent, load)) => {
                assignments.push(CandidateAssignment {
                    candidate_id: candidate.candidate_id,
                    agent_id: agent.id.clone(),
                    target_file: candidate.target_file.clone(),
                    queue_position: load,
                });
                agent_load.insert(agent.id.as_str(), load + 1);
                per_file_rr.insert(candidate.target_file, (idx + 1) % file_agents.len());
            }
            None => unassigned_candidates.push(candidate.candidate_id),
        }
    }

    let mut saturated_agents: Vec<String> = agent_load
        .iter()
        .filter(|(_, load)| **load >= max_assignments_per_agent)
        .map(|(agent_id, _)| (*agent_id).to_string())
        .collect();
    saturated_agents.sort();
    saturated_agents.dedup();

    DispatchPlan {
        assignments,
        unassigned_candidates,
        saturated_agents,
    }
}

/// Summarize per-agent queue state from a dispatch plan.
pub fn summarize_agent_dispatch(
    plan: &DispatchPlan,
    agents: &[Agent],
    max_assignments_per_agent: usize,
) -> Vec<AgentDispatchSummary> {
    let mut assigned_by_agent: HashMap<&str, Vec<&CandidateAssignment>> = HashMap::new();
    for assignment in &plan.assignments {
        assigned_by_agent
            .entry(assignment.agent_id.as_str())
            .or_default()
            .push(assignment);
    }

    let mut summaries: Vec<AgentDispatchSummary> = agents
        .iter()
        .filter(|agent| agent.is_active() && agent.role != AgentRole::Monitor)
        .map(|agent| {
            let mut assignments = assigned_by_agent
                .remove(agent.id.as_str())
                .unwrap_or_default();
            assignments.sort_by_key(|assignment| assignment.queue_position);

            let assigned_candidate_ids: Vec<String> = assignments
                .into_iter()
                .map(|assignment| assignment.candidate_id.clone())
                .collect();
            let queue_depth = assigned_candidate_ids.len();
            let remaining_capacity = max_assignments_per_agent.saturating_sub(queue_depth);

            AgentDispatchSummary {
                agent_id: agent.id.clone(),
                target_file: agent.target_file.clone(),
                assigned_candidate_ids,
                queue_depth,
                remaining_capacity,
                saturated: remaining_capacity == 0,
            }
        })
        .collect();

    summaries.sort_by(|left, right| {
        left.target_file
            .cmp(&right.target_file)
            .then_with(|| left.agent_id.cmp(&right.agent_id))
    });
    summaries
}

/// Summarize per-file dispatch state from a plan, including backpressure.
pub fn summarize_dispatch_plan(
    plan: &DispatchPlan,
    candidates: &[CandidateWorkItem],
    agents: &[Agent],
    max_assignments_per_agent: usize,
) -> Vec<FileDispatchSummary> {
    let agent_summaries = summarize_agent_dispatch(plan, agents, max_assignments_per_agent);
    let mut candidate_files: HashMap<&str, &str> = HashMap::new();
    for candidate in candidates {
        candidate_files.insert(
            candidate.candidate_id.as_str(),
            candidate.target_file.as_str(),
        );
    }

    let mut file_paths: HashSet<String> = HashSet::new();
    for candidate in candidates {
        file_paths.insert(candidate.target_file.clone());
    }
    for agent in agents
        .iter()
        .filter(|agent| agent.is_active() && agent.role != AgentRole::Monitor)
    {
        file_paths.insert(agent.target_file.clone());
    }

    let mut assigned_by_file: HashMap<&str, Vec<String>> = HashMap::new();
    for assignment in &plan.assignments {
        assigned_by_file
            .entry(assignment.target_file.as_str())
            .or_default()
            .push(assignment.candidate_id.clone());
    }
    for assigned in assigned_by_file.values_mut() {
        assigned.sort();
    }

    let mut unassigned_by_file: HashMap<&str, Vec<String>> = HashMap::new();
    for candidate_id in &plan.unassigned_candidates {
        if let Some(file_path) = candidate_files.get(candidate_id.as_str()) {
            unassigned_by_file
                .entry(*file_path)
                .or_default()
                .push(candidate_id.clone());
        }
    }
    for unassigned in unassigned_by_file.values_mut() {
        unassigned.sort();
    }

    let mut summaries = Vec::new();
    for file_path in file_paths {
        let mut eligible_agent_ids = Vec::new();
        let mut saturated_agent_ids = Vec::new();
        let mut max_queue_depth = 0usize;
        let mut remaining_capacity = 0usize;

        for agent_summary in agent_summaries
            .iter()
            .filter(|summary| summary.target_file == file_path)
        {
            eligible_agent_ids.push(agent_summary.agent_id.clone());
            max_queue_depth = max_queue_depth.max(agent_summary.queue_depth);
            remaining_capacity += agent_summary.remaining_capacity;
            if agent_summary.saturated {
                saturated_agent_ids.push(agent_summary.agent_id.clone());
            }
        }

        eligible_agent_ids.sort();
        saturated_agent_ids.sort();

        let assigned_candidate_ids = assigned_by_file
            .remove(file_path.as_str())
            .unwrap_or_default();
        let unassigned_candidate_ids = unassigned_by_file
            .remove(file_path.as_str())
            .unwrap_or_default();
        let backpressure = if eligible_agent_ids.is_empty() {
            DispatchBackpressure::BlockedNoAgent
        } else if remaining_capacity == 0 {
            DispatchBackpressure::Saturated
        } else if !unassigned_candidate_ids.is_empty() {
            DispatchBackpressure::Limited
        } else {
            DispatchBackpressure::Available
        };

        summaries.push(FileDispatchSummary {
            target_file: file_path,
            assigned_candidate_ids,
            unassigned_candidate_ids,
            eligible_agent_ids,
            saturated_agent_ids,
            max_queue_depth,
            remaining_capacity,
            backpressure,
        });
    }

    summaries.sort_by(|left, right| left.target_file.cmp(&right.target_file));
    summaries
}

// ---------------------------------------------------------------------------
// Conflict alerts and arbitration metadata
// ---------------------------------------------------------------------------

/// Severity level for concurrent candidate conflicts on the same file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictSeverity {
    Low,
    Medium,
    High,
}

/// Suggested arbitration action for a conflict set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArbitrationAction {
    PreferCandidate,
    RequireHumanReview,
}

/// Arbitration metadata emitted with conflict alerts.
#[derive(Debug, Clone, PartialEq)]
pub struct ArbitrationMetadata {
    pub action: ArbitrationAction,
    pub winning_candidate_id: Option<String>,
    pub losing_candidate_ids: Vec<String>,
    pub reason: String,
}

/// Rich conflict alert that the engine can surface in observability/audit outputs.
#[derive(Debug, Clone, PartialEq)]
pub struct ConflictAlert {
    pub file_path: String,
    pub agent_ids: Vec<String>,
    pub candidate_ids: Vec<String>,
    pub severity: ConflictSeverity,
    pub arbitration: ArbitrationMetadata,
}

const ARBITRATION_TIE_EPSILON: f64 = 1e-9;

/// Build conflict alerts from candidate assignments and optional ranking metadata.
///
/// `candidate_scores` are interpreted as "higher is better".
pub fn build_conflict_alerts(
    assignments: &[CandidateAssignment],
    candidate_scores: &HashMap<String, f64>,
) -> Vec<ConflictAlert> {
    let mut by_file: HashMap<&str, Vec<&CandidateAssignment>> = HashMap::new();
    for assignment in assignments {
        by_file
            .entry(assignment.target_file.as_str())
            .or_default()
            .push(assignment);
    }

    let mut alerts = Vec::new();
    for (file_path, file_assignments) in by_file {
        let unique_candidates: HashSet<&str> = file_assignments
            .iter()
            .map(|assignment| assignment.candidate_id.as_str())
            .collect();
        if unique_candidates.is_empty() {
            continue;
        }

        let mut candidate_ids: Vec<String> =
            unique_candidates.into_iter().map(str::to_string).collect();
        candidate_ids.sort();

        let mut agent_ids: Vec<String> = file_assignments
            .iter()
            .map(|assignment| assignment.agent_id.clone())
            .collect();
        agent_ids.sort();
        agent_ids.dedup();

        let (severity, arbitration) =
            classify_conflict_alert(&candidate_ids, &agent_ids, candidate_scores);

        alerts.push(ConflictAlert {
            file_path: file_path.to_string(),
            agent_ids,
            candidate_ids,
            severity,
            arbitration,
        });
    }

    alerts.sort_by(|left, right| left.file_path.cmp(&right.file_path));
    alerts
}

fn classify_conflict_alert(
    candidate_ids: &[String],
    agent_ids: &[String],
    candidate_scores: &HashMap<String, f64>,
) -> (ConflictSeverity, ArbitrationMetadata) {
    if candidate_ids.len() == 1 {
        let winner_id = candidate_ids[0].clone();
        let severity = if agent_ids.len() >= 3 {
            ConflictSeverity::Medium
        } else {
            ConflictSeverity::Low
        };
        return (
            severity,
            ArbitrationMetadata {
                action: ArbitrationAction::PreferCandidate,
                winning_candidate_id: Some(winner_id.clone()),
                losing_candidate_ids: Vec::new(),
                reason: format!(
                    "candidate {winner_id} is duplicated across {} agents for the same file",
                    agent_ids.len()
                ),
            },
        );
    }

    let mut scored_candidates = Vec::with_capacity(candidate_ids.len());
    let mut missing_scores = Vec::new();
    for candidate_id in candidate_ids {
        match candidate_scores.get(candidate_id) {
            Some(score) => scored_candidates.push((candidate_id.clone(), *score)),
            None => missing_scores.push(candidate_id.clone()),
        }
    }

    if !missing_scores.is_empty() {
        return (
            ConflictSeverity::High,
            ArbitrationMetadata {
                action: ArbitrationAction::RequireHumanReview,
                winning_candidate_id: None,
                losing_candidate_ids: Vec::new(),
                reason: format!(
                    "missing arbitration scores for candidates: {}",
                    missing_scores.join(", ")
                ),
            },
        );
    }

    scored_candidates.sort_by(|left, right| {
        right
            .1
            .partial_cmp(&left.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.0.cmp(&right.0))
    });

    let best_score = scored_candidates[0].1;
    let tied_winners: Vec<String> = scored_candidates
        .iter()
        .filter(|(_, score)| (best_score - *score).abs() <= ARBITRATION_TIE_EPSILON)
        .map(|(candidate_id, _)| candidate_id.clone())
        .collect();
    if tied_winners.len() > 1 {
        return (
            ConflictSeverity::High,
            ArbitrationMetadata {
                action: ArbitrationAction::RequireHumanReview,
                winning_candidate_id: None,
                losing_candidate_ids: Vec::new(),
                reason: format!(
                    "candidates {} are tied at arbitration score {:.3}",
                    tied_winners.join(", "),
                    best_score
                ),
            },
        );
    }

    let winner_id = scored_candidates[0].0.clone();
    let second_best_score = scored_candidates
        .get(1)
        .map(|(_, score)| *score)
        .unwrap_or(f64::NEG_INFINITY);
    let score_gap = if second_best_score.is_finite() {
        best_score - second_best_score
    } else {
        f64::INFINITY
    };
    let losing_candidate_ids = candidate_ids
        .iter()
        .filter(|candidate_id| candidate_id.as_str() != winner_id.as_str())
        .cloned()
        .collect();
    let severity = if candidate_ids.len() >= 3 || agent_ids.len() >= 3 || score_gap < 0.05 {
        ConflictSeverity::High
    } else {
        ConflictSeverity::Medium
    };

    (
        severity,
        ArbitrationMetadata {
            action: ArbitrationAction::PreferCandidate,
            winning_candidate_id: Some(winner_id.clone()),
            losing_candidate_ids,
            reason: format!(
                "candidate {winner_id} leads arbitration by {:.3} score over the next candidate",
                score_gap.max(0.0)
            ),
        },
    )
}

// ---------------------------------------------------------------------------
// Oscillation tracking
// ---------------------------------------------------------------------------

/// Outcome of a candidate adjudication event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateOutcome {
    Accepted,
    Rejected,
}

/// Event provided to oscillation tracking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateOutcomeEvent {
    pub file_path: String,
    pub candidate_id: String,
    pub outcome: CandidateOutcome,
}

/// Alert emitted when candidate outcomes are oscillating for the same file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OscillationAlert {
    pub file_path: String,
    pub flip_count: u32,
    pub candidate_ids: Vec<String>,
}

/// Tracker for accept/reject oscillation on candidate outcomes.
pub struct OscillationTracker {
    window_size: usize,
    flip_threshold: u32,
    stabilization_window: usize,
    history: HashMap<String, Vec<CandidateOutcomeEvent>>,
    suppressed_files: HashSet<String>,
}

impl OscillationTracker {
    pub fn new(window_size: usize, flip_threshold: u32) -> Self {
        Self::with_stabilization_window(window_size, flip_threshold, flip_threshold.max(2) as usize)
    }

    pub fn with_stabilization_window(
        window_size: usize,
        flip_threshold: u32,
        stabilization_window: usize,
    ) -> Self {
        Self {
            window_size: window_size.max(3),
            flip_threshold,
            stabilization_window: stabilization_window.max(2),
            history: HashMap::new(),
            suppressed_files: HashSet::new(),
        }
    }

    /// Record an outcome event and return an alert when oscillation is detected.
    pub fn record(&mut self, event: CandidateOutcomeEvent) -> Option<OscillationAlert> {
        let file_path = event.file_path.clone();
        let history = self.history.entry(file_path.clone()).or_default();
        history.push(event);
        if history.len() > self.window_size {
            let overflow = history.len() - self.window_size;
            history.drain(0..overflow);
        }

        if self.suppressed_files.contains(file_path.as_str()) {
            let stable_len = trailing_same_outcome_len(history);
            if stable_len >= self.stabilization_window {
                let keep_from = history.len().saturating_sub(stable_len);
                history.drain(0..keep_from);
                self.suppressed_files.remove(file_path.as_str());
            } else {
                return None;
            }
        }

        if history.len() < 3 {
            return None;
        }

        let flip_count = history.windows(2).fold(0u32, |acc, window| {
            if window[0].outcome != window[1].outcome {
                acc + 1
            } else {
                acc
            }
        });

        if flip_count < self.flip_threshold {
            return None;
        }

        self.suppressed_files.insert(file_path.clone());
        let mut candidate_ids: Vec<String> = history
            .iter()
            .map(|event| event.candidate_id.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        candidate_ids.sort();

        Some(OscillationAlert {
            file_path,
            flip_count,
            candidate_ids,
        })
    }

    pub fn history_for(&self, file_path: &str) -> &[CandidateOutcomeEvent] {
        self.history
            .get(file_path)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub fn is_suppressed(&self, file_path: &str) -> bool {
        self.suppressed_files.contains(file_path)
    }
}

fn trailing_same_outcome_len(history: &[CandidateOutcomeEvent]) -> usize {
    let Some(last) = history.last() else {
        return 0;
    };

    history
        .iter()
        .rev()
        .take_while(|event| event.outcome == last.outcome)
        .count()
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
    /// Trailing monotonic readings required to clear suppression.
    stabilization_window: usize,
    /// Per-file suppression state once oscillation has already been reported.
    suppressed_files: HashSet<String>,
    /// Bound score history so long-running sessions do not grow unbounded.
    history_window: usize,
}

impl Monitor {
    /// Create a new Monitor.
    pub fn new(oscillation_threshold: u32) -> Self {
        Self {
            score_history: HashMap::new(),
            oscillation_threshold,
            stabilization_window: oscillation_threshold.max(2) as usize,
            suppressed_files: HashSet::new(),
            history_window: ((oscillation_threshold.max(2) as usize) * 3).max(6),
        }
    }

    /// Record a new score for a file and check for oscillation.
    pub fn record_score(&mut self, file_path: &str, score: f64) -> Option<SwarmError> {
        let history = self.score_history.entry(file_path.to_string()).or_default();
        history.push(score);
        if history.len() > self.history_window {
            let overflow = history.len() - self.history_window;
            history.drain(0..overflow);
        }

        if self.suppressed_files.contains(file_path) {
            let stable_len = trailing_monotonic_run_len(history);
            if stable_len >= self.stabilization_window {
                let keep_from = history.len().saturating_sub(stable_len);
                history.drain(0..keep_from);
                self.suppressed_files.remove(file_path);
            } else {
                return None;
            }
        }

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
            self.suppressed_files.insert(file_path.to_string());
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

    pub fn is_suppressed(&self, file_path: &str) -> bool {
        self.suppressed_files.contains(file_path)
    }
}

fn trailing_monotonic_run_len(history: &[f64]) -> usize {
    if history.is_empty() {
        return 0;
    }
    if history.len() == 1 {
        return 1;
    }

    let mut direction = 0i8;
    let mut run_len = 1usize;
    for window in history.windows(2).rev() {
        let delta = window[1] - window[0];
        let next_direction = if delta > 0.0 {
            1
        } else if delta < 0.0 {
            -1
        } else {
            0
        };

        if next_direction == 0 {
            run_len += 1;
            continue;
        }
        if direction == 0 {
            direction = next_direction;
            run_len += 1;
            continue;
        }
        if next_direction == direction {
            run_len += 1;
        } else {
            break;
        }
    }

    run_len
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
