use std::collections::HashMap;

use mercury_cli::db::{AgentLogEntry, ThermalAggregate};
use mercury_cli::swarm::{
    build_conflict_alerts, build_dispatch_plan, summarize_agent_dispatch, summarize_dispatch_plan,
    Agent, AgentStatus, ArbitrationAction, CandidateAssignment, CandidateOutcome,
    CandidateOutcomeEvent, CandidateWorkItem, ConflictSeverity, DispatchBackpressure, Monitor,
    OscillationTracker,
};
use mercury_cli::thermal::{dispatch_targets, DispatchReadiness, ExecutionPhase};

fn active_hot_agent(id: &str, file: &str) -> Agent {
    let mut agent = Agent::new_hot(file, "constitutional");
    agent.id = id.to_string();
    agent.status = AgentStatus::Running;
    agent
}

fn aggregate(file_path: &str, score: f64) -> ThermalAggregate {
    ThermalAggregate {
        file_path: file_path.to_string(),
        composite_score: score,
        max_score: score,
        agent_density: 0,
        last_updated: "2026-03-09T00:00:00Z".to_string(),
        is_locked: false,
    }
}

fn active_agent_log(file_path: &str, agent_id: &str) -> AgentLogEntry {
    AgentLogEntry {
        id: 1,
        agent_id: agent_id.to_string(),
        command: "cargo test".to_string(),
        file_path: file_path.to_string(),
        status: "running".to_string(),
        micro_heatmap: None,
        started_at: "2026-03-09T00:00:00Z".to_string(),
        completed_at: None,
        tokens_used: 0,
        cost_usd: 0.0,
    }
}

#[test]
fn dispatch_plan_balances_by_file_and_respects_capacity() {
    let agents = vec![
        active_hot_agent("a1", "src/lib.rs"),
        active_hot_agent("a2", "src/lib.rs"),
    ];
    let candidates = vec![
        CandidateWorkItem::new("c1", "src/lib.rs", 1.0),
        CandidateWorkItem::new("c2", "src/lib.rs", 2.0),
        CandidateWorkItem::new("c3", "src/lib.rs", 3.0),
        CandidateWorkItem::new("c4", "src/lib.rs", 4.0),
        CandidateWorkItem::new("c5", "src/lib.rs", 5.0),
    ];

    let plan = build_dispatch_plan(&candidates, &agents, 2);

    assert_eq!(plan.assignments.len(), 4);
    assert_eq!(plan.unassigned_candidates, vec!["c1".to_string()]);
    assert_eq!(plan.saturated_agents.len(), 2);

    let a1_load = plan
        .assignments
        .iter()
        .filter(|assignment| assignment.agent_id == "a1")
        .count();
    let a2_load = plan
        .assignments
        .iter()
        .filter(|assignment| assignment.agent_id == "a2")
        .count();
    assert_eq!(a1_load, 2);
    assert_eq!(a2_load, 2);
}

#[test]
fn dispatch_plan_ignores_monitor_agents() {
    let mut monitor = Agent::new_monitor(0, "constitutional");
    monitor.id = "m1".to_string();
    monitor.status = AgentStatus::Running;

    let candidates = vec![CandidateWorkItem::new("c1", "src/main.rs", 1.0)];
    let plan = build_dispatch_plan(&candidates, &[monitor], 1);

    assert!(plan.assignments.is_empty());
    assert_eq!(plan.unassigned_candidates, vec!["c1".to_string()]);
}

#[test]
fn dispatch_summaries_report_capacity_and_backpressure() {
    let agents = vec![
        active_hot_agent("a1", "src/lib.rs"),
        active_hot_agent("a2", "src/lib.rs"),
        active_hot_agent("a3", "src/util.rs"),
    ];
    let candidates = vec![
        CandidateWorkItem::new("lib-1", "src/lib.rs", 5.0),
        CandidateWorkItem::new("lib-2", "src/lib.rs", 4.0),
        CandidateWorkItem::new("lib-3", "src/lib.rs", 3.0),
        CandidateWorkItem::new("lib-4", "src/lib.rs", 2.0),
        CandidateWorkItem::new("lib-5", "src/lib.rs", 1.0),
        CandidateWorkItem::new("main-1", "src/main.rs", 0.5),
    ];

    let plan = build_dispatch_plan(&candidates, &agents, 2);
    let agent_summaries = summarize_agent_dispatch(&plan, &agents, 2);
    let file_summaries = summarize_dispatch_plan(&plan, &candidates, &agents, 2);

    let lib_agent = agent_summaries
        .iter()
        .find(|summary| summary.agent_id == "a1")
        .expect("lib agent summary");
    assert_eq!(lib_agent.queue_depth, 2);
    assert_eq!(lib_agent.remaining_capacity, 0);
    assert!(lib_agent.saturated);

    let util_agent = agent_summaries
        .iter()
        .find(|summary| summary.agent_id == "a3")
        .expect("util agent summary");
    assert_eq!(util_agent.queue_depth, 0);
    assert_eq!(util_agent.remaining_capacity, 2);
    assert!(!util_agent.saturated);

    let lib_file = file_summaries
        .iter()
        .find(|summary| summary.target_file == "src/lib.rs")
        .expect("lib file summary");
    assert_eq!(lib_file.backpressure, DispatchBackpressure::Saturated);
    assert_eq!(lib_file.unassigned_candidate_ids, vec!["lib-5".to_string()]);
    assert_eq!(
        lib_file.saturated_agent_ids,
        vec!["a1".to_string(), "a2".to_string()]
    );

    let blocked_file = file_summaries
        .iter()
        .find(|summary| summary.target_file == "src/main.rs")
        .expect("blocked file summary");
    assert_eq!(
        blocked_file.backpressure,
        DispatchBackpressure::BlockedNoAgent
    );
    assert_eq!(
        blocked_file.unassigned_candidate_ids,
        vec!["main-1".to_string()]
    );

    let available_file = file_summaries
        .iter()
        .find(|summary| summary.target_file == "src/util.rs")
        .expect("available file summary");
    assert_eq!(available_file.backpressure, DispatchBackpressure::Available);
    assert_eq!(available_file.remaining_capacity, 2);
}

#[test]
fn conflict_alerts_include_arbitration_metadata() {
    let assignments = vec![
        CandidateAssignment {
            candidate_id: "cand-low".to_string(),
            agent_id: "a1".to_string(),
            target_file: "src/repo.rs".to_string(),
            queue_position: 0,
        },
        CandidateAssignment {
            candidate_id: "cand-high".to_string(),
            agent_id: "a2".to_string(),
            target_file: "src/repo.rs".to_string(),
            queue_position: 0,
        },
    ];

    let mut scores = HashMap::new();
    scores.insert("cand-low".to_string(), 0.2);
    scores.insert("cand-high".to_string(), 0.9);

    let alerts = build_conflict_alerts(&assignments, &scores);
    assert_eq!(alerts.len(), 1);

    let alert = &alerts[0];
    assert_eq!(alert.file_path, "src/repo.rs");
    assert_eq!(alert.severity, ConflictSeverity::Medium);
    assert_eq!(alert.arbitration.action, ArbitrationAction::PreferCandidate);
    assert_eq!(
        alert.arbitration.winning_candidate_id.as_deref(),
        Some("cand-high")
    );
    assert_eq!(
        alert.arbitration.losing_candidate_ids,
        vec!["cand-low".to_string()]
    );
}

#[test]
fn conflict_alerts_require_human_review_when_scores_are_missing() {
    let assignments = vec![
        CandidateAssignment {
            candidate_id: "cand-a".to_string(),
            agent_id: "a1".to_string(),
            target_file: "src/repo.rs".to_string(),
            queue_position: 0,
        },
        CandidateAssignment {
            candidate_id: "cand-b".to_string(),
            agent_id: "a2".to_string(),
            target_file: "src/repo.rs".to_string(),
            queue_position: 0,
        },
    ];

    let mut scores = HashMap::new();
    scores.insert("cand-a".to_string(), 0.4);

    let alerts = build_conflict_alerts(&assignments, &scores);
    let alert = &alerts[0];
    assert_eq!(alert.severity, ConflictSeverity::High);
    assert_eq!(
        alert.arbitration.action,
        ArbitrationAction::RequireHumanReview
    );
    assert!(alert
        .arbitration
        .reason
        .contains("missing arbitration scores"));
}

#[test]
fn conflict_alerts_require_human_review_when_scores_tie() {
    let assignments = vec![
        CandidateAssignment {
            candidate_id: "cand-a".to_string(),
            agent_id: "a1".to_string(),
            target_file: "src/repo.rs".to_string(),
            queue_position: 0,
        },
        CandidateAssignment {
            candidate_id: "cand-b".to_string(),
            agent_id: "a2".to_string(),
            target_file: "src/repo.rs".to_string(),
            queue_position: 0,
        },
    ];

    let mut scores = HashMap::new();
    scores.insert("cand-a".to_string(), 0.5);
    scores.insert("cand-b".to_string(), 0.5);

    let alerts = build_conflict_alerts(&assignments, &scores);
    let alert = &alerts[0];
    assert_eq!(alert.severity, ConflictSeverity::High);
    assert_eq!(
        alert.arbitration.action,
        ArbitrationAction::RequireHumanReview
    );
    assert!(alert.arbitration.reason.contains("are tied"));
}

#[test]
fn conflict_alerts_treat_duplicate_candidate_as_low_severity_dedup() {
    let assignments = vec![
        CandidateAssignment {
            candidate_id: "cand-same".to_string(),
            agent_id: "a1".to_string(),
            target_file: "src/repo.rs".to_string(),
            queue_position: 0,
        },
        CandidateAssignment {
            candidate_id: "cand-same".to_string(),
            agent_id: "a2".to_string(),
            target_file: "src/repo.rs".to_string(),
            queue_position: 0,
        },
    ];

    let mut scores = HashMap::new();
    scores.insert("cand-same".to_string(), 0.9);

    let alerts = build_conflict_alerts(&assignments, &scores);
    let alert = &alerts[0];
    assert_eq!(alert.severity, ConflictSeverity::Low);
    assert_eq!(alert.candidate_ids, vec!["cand-same".to_string()]);
    assert_eq!(alert.arbitration.action, ArbitrationAction::PreferCandidate);
    assert_eq!(
        alert.arbitration.winning_candidate_id.as_deref(),
        Some("cand-same")
    );
    assert!(alert
        .arbitration
        .reason
        .contains("duplicated across 2 agents"));
}

#[test]
fn oscillation_tracker_flags_flip_flops() {
    let mut tracker = OscillationTracker::new(6, 3);

    assert!(tracker
        .record(CandidateOutcomeEvent {
            file_path: "src/swarm.rs".to_string(),
            candidate_id: "c1".to_string(),
            outcome: CandidateOutcome::Accepted,
        })
        .is_none());
    assert!(tracker
        .record(CandidateOutcomeEvent {
            file_path: "src/swarm.rs".to_string(),
            candidate_id: "c2".to_string(),
            outcome: CandidateOutcome::Rejected,
        })
        .is_none());
    assert!(tracker
        .record(CandidateOutcomeEvent {
            file_path: "src/swarm.rs".to_string(),
            candidate_id: "c1".to_string(),
            outcome: CandidateOutcome::Accepted,
        })
        .is_none());

    let alert = tracker.record(CandidateOutcomeEvent {
        file_path: "src/swarm.rs".to_string(),
        candidate_id: "c2".to_string(),
        outcome: CandidateOutcome::Rejected,
    });

    assert!(alert.is_some());
    let alert = alert.expect("oscillation alert expected");
    assert_eq!(alert.file_path, "src/swarm.rs");
    assert!(alert.flip_count >= 3);
    assert_eq!(
        alert.candidate_ids,
        vec!["c1".to_string(), "c2".to_string()]
    );
}

#[test]
fn oscillation_tracker_suppresses_repeat_alerts_until_stable_tail() {
    let mut tracker = OscillationTracker::with_stabilization_window(8, 2, 3);

    for (candidate_id, outcome) in [
        ("c1", CandidateOutcome::Accepted),
        ("c2", CandidateOutcome::Rejected),
    ] {
        assert!(tracker
            .record(CandidateOutcomeEvent {
                file_path: "src/swarm.rs".to_string(),
                candidate_id: candidate_id.to_string(),
                outcome,
            })
            .is_none());
    }

    let first_alert = tracker.record(CandidateOutcomeEvent {
        file_path: "src/swarm.rs".to_string(),
        candidate_id: "c1".to_string(),
        outcome: CandidateOutcome::Accepted,
    });
    assert!(first_alert.is_some());
    assert!(tracker.is_suppressed("src/swarm.rs"));

    assert!(tracker
        .record(CandidateOutcomeEvent {
            file_path: "src/swarm.rs".to_string(),
            candidate_id: "c3".to_string(),
            outcome: CandidateOutcome::Accepted,
        })
        .is_none());
    assert!(tracker.is_suppressed("src/swarm.rs"));

    for candidate_id in ["c3", "c4"] {
        assert!(tracker
            .record(CandidateOutcomeEvent {
                file_path: "src/swarm.rs".to_string(),
                candidate_id: candidate_id.to_string(),
                outcome: CandidateOutcome::Accepted,
            })
            .is_none());
    }

    assert!(!tracker.is_suppressed("src/swarm.rs"));
    assert!(tracker
        .record(CandidateOutcomeEvent {
            file_path: "src/swarm.rs".to_string(),
            candidate_id: "c5".to_string(),
            outcome: CandidateOutcome::Rejected,
        })
        .is_none());
    let second_alert = tracker.record(CandidateOutcomeEvent {
        file_path: "src/swarm.rs".to_string(),
        candidate_id: "c6".to_string(),
        outcome: CandidateOutcome::Accepted,
    });
    assert!(second_alert.is_some());
}

#[test]
fn monitor_suppression_clears_after_monotonic_recovery() {
    let mut monitor = Monitor::new(3);

    for score in [0.5, 0.7, 0.4, 0.8] {
        assert!(monitor.record_score("src/lib.rs", score).is_none());
    }

    let alert = monitor.record_score("src/lib.rs", 0.3);
    assert!(alert.is_some());
    assert!(monitor.is_suppressed("src/lib.rs"));

    assert!(monitor.record_score("src/lib.rs", 0.31).is_none());
    assert!(monitor.is_suppressed("src/lib.rs"));

    assert!(monitor.record_score("src/lib.rs", 0.32).is_none());
    assert!(!monitor.is_suppressed("src/lib.rs"));
}

#[test]
fn thermal_dispatch_targets_surface_phase_rank_and_launchability() {
    let aggregates = vec![
        aggregate("shared-hot.rs", 0.90),
        aggregate("idle-warm.rs", 0.72),
        aggregate("idle-cool.rs", 0.40),
    ];
    let active_agents = vec![
        active_agent_log("shared-hot.rs", "a1"),
        active_agent_log("shared-hot.rs", "a2"),
    ];

    let dispatch = dispatch_targets(
        &aggregates,
        &[],
        &active_agents,
        ExecutionPhase::Annealing,
        4,
    );

    assert_eq!(dispatch[0].file_path, "idle-warm.rs");
    assert_eq!(dispatch[0].priority_rank, 0);
    assert_eq!(dispatch[0].launchable_agents, 2);
    assert_eq!(dispatch[0].readiness, DispatchReadiness::LaunchNow);

    let hot = dispatch
        .iter()
        .find(|target| target.file_path == "shared-hot.rs")
        .expect("shared hot target");
    assert_eq!(hot.phase, ExecutionPhase::Annealing);
    assert_eq!(hot.active_agents, 2);
    assert_eq!(hot.desired_agent_count, 2);
    assert_eq!(hot.additional_agents_needed, 0);
    assert_eq!(hot.launchable_agents, 0);
    assert_eq!(hot.readiness, DispatchReadiness::HoldAtDesiredDensity);
}
