//! Thermal engine -- Log-Sum-Exp merge, exponential pheromone decay,
//! gradient navigation across execution phases, and terminal heatmap
//! visualization.
//!
//! This module sits on top of [`crate::db`] and provides the mathematical
//! primitives that drive Mercury's swarm-intelligence scheduler.  Every
//! public function is pure (no interior I/O) except the ratatui rendering
//! helpers, which write to a terminal backend.

use std::collections::HashSet;
use std::io;

use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Bar, BarChart, BarGroup, Block, Borders, Paragraph};
use ratatui::Terminal;

use crate::db::{AgentLogEntry, CoolLock, DbError, ThermalAggregate};

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors originating from the thermal engine layer.
#[derive(thiserror::Error, Debug)]
pub enum ThermalError {
    /// A database operation failed.
    #[error("database error: {0}")]
    Database(#[from] DbError),

    /// Caller passed an empty score slice to [`thermal_merge`].
    #[error("empty score set for merge")]
    EmptyScores,

    /// Temperature parameter must be strictly positive.
    #[error("invalid temperature {0}: must be positive")]
    InvalidTemperature(f64),

    /// An I/O error from the terminal backend.
    #[error("terminal I/O error: {0}")]
    Io(#[from] io::Error),
}

// ---------------------------------------------------------------------------
// Core algorithms
// ---------------------------------------------------------------------------

/// Compute the Log-Sum-Exp (soft-maximum) of `scores` at the given
/// `temperature`.
///
/// This is the numerically stable formulation:
///
/// ```text
/// LSE(s; T) = max(s) + T * ln( sum_i exp((s_i - max(s)) / T) )
/// ```
///
/// # Errors
///
/// Returns [`ThermalError::EmptyScores`] when `scores` is empty and
/// [`ThermalError::InvalidTemperature`] when `temperature <= 0`.
pub fn thermal_merge(scores: &[f64], temperature: f64) -> Result<f64, ThermalError> {
    if scores.is_empty() {
        return Err(ThermalError::EmptyScores);
    }
    if temperature <= 0.0 || temperature.is_nan() {
        return Err(ThermalError::InvalidTemperature(temperature));
    }

    let max_score = scores.iter().cloned().fold(f64::NEG_INFINITY, f64::max);

    let sum_exp: f64 = scores
        .iter()
        .map(|s| ((s - max_score) / temperature).exp())
        .sum();

    Ok(max_score + temperature * sum_exp.ln())
}

/// Apply exponential pheromone decay to a thermal `score`.
///
/// The decay follows the half-life formula:
///
/// ```text
/// decayed = score * 0.5^(elapsed / half_life)
/// ```
///
/// Both `elapsed_seconds` and `half_life` must be non-negative (half-life
/// must be strictly positive).  No error is returned for convenience --
/// out-of-range inputs are clamped to sensible defaults.
pub fn apply_decay(score: f64, elapsed_seconds: f64, half_life: f64) -> f64 {
    if half_life <= 0.0 || elapsed_seconds < 0.0 {
        return score;
    }
    score * (0.5_f64).powf(elapsed_seconds / half_life)
}

/// Batch-apply decay to every score in a slice, returning a new `Vec`.
///
/// This is a convenience wrapper around [`apply_decay`].
pub fn apply_decay_batch(scores: &[f64], elapsed_seconds: f64, half_life: f64) -> Vec<f64> {
    scores
        .iter()
        .map(|&s| apply_decay(s, elapsed_seconds, half_life))
        .collect()
}

// ---------------------------------------------------------------------------
// Gradient navigation
// ---------------------------------------------------------------------------

/// The three execution phases of Mercury's annealing-based scheduler.
///
/// Agents traverse these phases in order during a single iteration:
///
/// 1. **Scaffolding** -- stabilize cool (low-score) regions first so the
///    foundation is solid before tackling hot spots.
/// 2. **Resolution** -- focus effort on the hottest regions where the most
///    complexity or risk remains.
/// 3. **Annealing** -- converge globally, sweeping any remaining mid-range
///    files toward completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExecutionPhase {
    /// Cool-first: lock down stable regions.
    Scaffolding,
    /// Hot zones: resolve high-complexity files.
    Resolution,
    /// Global convergence: sweep remaining mid-range targets.
    Annealing,
}

impl std::fmt::Display for ExecutionPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Scaffolding => write!(f, "Scaffolding"),
            Self::Resolution => write!(f, "Resolution"),
            Self::Annealing => write!(f, "Annealing"),
        }
    }
}

/// Select the next file target for an agent given the current thermal field.
///
/// The selection strategy depends on the [`ExecutionPhase`]:
///
/// | Phase        | Strategy                                                |
/// |--------------|---------------------------------------------------------|
/// | Scaffolding  | Pick the **coolest** unlocked file with no active agents|
/// | Resolution   | Pick the **hottest** unlocked file                      |
/// | Annealing    | Pick the file with the **lowest agent density**         |
///
/// Files that are locked (via `cool_locks`) or already at maximum agent
/// density are excluded from consideration.
///
/// Returns `None` when no eligible file exists.
pub fn next_target(
    aggregates: &[ThermalAggregate],
    locks: &[CoolLock],
    active_agents: &[AgentLogEntry],
    phase: ExecutionPhase,
    max_density: i32,
) -> Option<String> {
    // Build sets for O(1) lookup.
    let locked_files: HashSet<&str> = locks.iter().map(|l| l.file_path.as_str()).collect();

    let agent_file_counts: std::collections::HashMap<&str, i32> = {
        let mut map = std::collections::HashMap::new();
        for agent in active_agents {
            *map.entry(agent.file_path.as_str()).or_insert(0) += 1;
        }
        map
    };

    // Filter to eligible candidates.
    let candidates: Vec<&ThermalAggregate> = aggregates
        .iter()
        .filter(|a| {
            !locked_files.contains(a.file_path.as_str())
                && !a.is_locked
                && *agent_file_counts.get(a.file_path.as_str()).unwrap_or(&0) < max_density
        })
        .collect();

    if candidates.is_empty() {
        return None;
    }

    let best = match phase {
        ExecutionPhase::Scaffolding => {
            // Coolest file first, prefer files with zero active agents.
            candidates.iter().min_by(|a, b| {
                let a_agents = agent_file_counts.get(a.file_path.as_str()).unwrap_or(&0);
                let b_agents = agent_file_counts.get(b.file_path.as_str()).unwrap_or(&0);
                a_agents.cmp(b_agents).then(
                    a.composite_score
                        .partial_cmp(&b.composite_score)
                        .unwrap_or(std::cmp::Ordering::Equal),
                )
            })
        }
        ExecutionPhase::Resolution => {
            // Hottest file first.
            candidates.iter().max_by(|a, b| {
                a.composite_score
                    .partial_cmp(&b.composite_score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
        }
        ExecutionPhase::Annealing => {
            // Lowest agent density first, break ties by highest score.
            candidates.iter().min_by(|a, b| {
                let a_agents = agent_file_counts.get(a.file_path.as_str()).unwrap_or(&0);
                let b_agents = agent_file_counts.get(b.file_path.as_str()).unwrap_or(&0);
                a_agents.cmp(b_agents).then(
                    b.composite_score
                        .partial_cmp(&a.composite_score)
                        .unwrap_or(std::cmp::Ordering::Equal),
                )
            })
        }
    };

    best.map(|a| a.file_path.clone())
}

/// Determine the current [`ExecutionPhase`] from the global swarm state.
///
/// The heuristic is:
/// - iteration < 30%  of expected total  -->  Scaffolding
/// - iteration < 70%  of expected total  -->  Resolution
/// - otherwise                            -->  Annealing
///
/// `expected_iterations` must be > 0; if it is 0 the function defaults to
/// [`ExecutionPhase::Resolution`].
pub fn phase_from_progress(current_iteration: i64, expected_iterations: i64) -> ExecutionPhase {
    if expected_iterations <= 0 {
        return ExecutionPhase::Resolution;
    }
    let ratio = current_iteration as f64 / expected_iterations as f64;
    if ratio < 0.3 {
        ExecutionPhase::Scaffolding
    } else if ratio < 0.7 {
        ExecutionPhase::Resolution
    } else {
        ExecutionPhase::Annealing
    }
}

// ---------------------------------------------------------------------------
// Terminal heatmap visualization (ratatui 0.29 + crossterm 0.28)
// ---------------------------------------------------------------------------

/// Map a thermal score in `[0.0, 1.0]` to a blue-to-red color gradient.
///
/// The mapping is:
/// - `[0.0, 0.25)` -- blue
/// - `[0.25, 0.50)` -- cyan
/// - `[0.50, 0.75)` -- yellow
/// - `[0.75, 1.00]` -- red
fn score_to_color(score: f64) -> Color {
    if score < 0.25 {
        Color::Blue
    } else if score < 0.50 {
        Color::Cyan
    } else if score < 0.75 {
        Color::Yellow
    } else {
        Color::Red
    }
}

/// Generate a 10-character ASCII bar representing the score magnitude.
fn score_bar(score: f64) -> String {
    let filled = (score.clamp(0.0, 1.0) * 10.0).round() as usize;
    let empty = 10 - filled;
    format!("[{}{}]", "#".repeat(filled), " ".repeat(empty))
}

/// Render a full-screen heatmap in the alternate terminal screen.
///
/// This function enters the alternate screen, draws a single frame, and
/// then immediately returns (leaving the alternate screen).  It is
/// intended for one-shot "snapshot" rendering -- callers that want a live
/// TUI should build their own event loop around the primitives in this
/// module.
///
/// # Errors
///
/// Returns [`ThermalError::Io`] on terminal failures.
pub fn render_heatmap(aggregates: &[ThermalAggregate]) -> Result<(), ThermalError> {
    let mut stdout = io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    terminal::enable_raw_mode()?;

    let backend = CrosstermBackend::new(stdout);
    let mut term = Terminal::new(backend)?;

    let draw_result = term.draw(|frame| {
        let size = frame.area();

        // Split into header + body.
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(0)])
            .split(size);

        // Header.
        let header = Paragraph::new(Line::from(vec![
            Span::styled(
                " Mercury Thermal Heatmap ",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!("  ({} files)", aggregates.len())),
        ]))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Thermal Field"),
        );
        frame.render_widget(header, chunks[0]);

        // Build bar chart data.
        let bars: Vec<Bar> = aggregates
            .iter()
            .map(|agg| {
                let label = truncate_path(&agg.file_path, 24);
                let value = (agg.composite_score * 100.0).round() as u64;
                let color = score_to_color(agg.composite_score);
                Bar::default()
                    .label(Line::from(label))
                    .value(value)
                    .style(Style::default().fg(color))
                    .value_style(
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    )
            })
            .collect();

        let bar_chart = BarChart::default()
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Composite Scores (0-100)"),
            )
            .data(BarGroup::default().bars(&bars))
            .bar_width(3)
            .bar_gap(1)
            .direction(Direction::Horizontal);

        frame.render_widget(bar_chart, chunks[1]);
    });

    // Consume the draw result before borrowing term again for cleanup.
    let draw_ok = draw_result.is_ok();

    // Always clean up the terminal, even if drawing failed.
    terminal::disable_raw_mode()?;
    term.backend_mut().execute(LeaveAlternateScreen)?;

    if !draw_ok {
        return Err(ThermalError::Io(io::Error::other(
            "failed to draw heatmap frame",
        )));
    }
    Ok(())
}

/// Render the heatmap as a plain ASCII string for non-TUI contexts
/// (CI logs, piped output, tests).
///
/// Each line contains: file path (padded), score bar, numeric score,
/// lock status, and active agent count.
///
/// ```text
/// src/engine.rs            [########  ] 0.82  UNLOCKED  agents: 2
/// src/db.rs                [###       ] 0.31  LOCKED    agents: 0
/// ```
pub fn render_heatmap_to_string(
    aggregates: &[ThermalAggregate],
    active_agents: &[AgentLogEntry],
) -> String {
    if aggregates.is_empty() {
        return String::from("(no thermal data)");
    }

    // Pre-compute agent counts per file.
    let agent_counts: std::collections::HashMap<&str, usize> = {
        let mut map = std::collections::HashMap::new();
        for agent in active_agents {
            *map.entry(agent.file_path.as_str()).or_insert(0) += 1;
        }
        map
    };

    // Determine column width for paths.
    let max_path_len = aggregates
        .iter()
        .map(|a| a.file_path.len())
        .max()
        .unwrap_or(0)
        .clamp(12, 48);

    let mut lines = Vec::with_capacity(aggregates.len() + 2);
    lines.push(format!(
        "{:<width$}  {:>12}  {:>5}  {:>8}  {:>9}",
        "FILE",
        "BAR",
        "SCORE",
        "LOCK",
        "AGENTS",
        width = max_path_len
    ));
    lines.push("-".repeat(max_path_len + 2 + 12 + 2 + 5 + 2 + 8 + 2 + 9));

    for agg in aggregates {
        let path_display = if agg.file_path.len() > max_path_len {
            truncate_path(&agg.file_path, max_path_len)
        } else {
            agg.file_path.clone()
        };
        let bar = score_bar(agg.composite_score);
        let lock_status = if agg.is_locked { "LOCKED" } else { "UNLOCKED" };
        let count = agent_counts.get(agg.file_path.as_str()).unwrap_or(&0);
        lines.push(format!(
            "{:<width$}  {:>12}  {:>5.2}  {:>8}  agents: {}",
            path_display,
            bar,
            agg.composite_score,
            lock_status,
            count,
            width = max_path_len
        ));
    }

    lines.join("\n")
}

/// Truncate a path to at most `max_len` characters, inserting an ellipsis
/// prefix when truncated.
fn truncate_path(path: &str, max_len: usize) -> String {
    if path.len() <= max_len {
        path.to_string()
    } else {
        let keep = max_len.saturating_sub(3);
        format!("...{}", &path[path.len() - keep..])
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- thermal_merge -------------------------------------------------------

    #[test]
    fn merge_commutativity() {
        let a = thermal_merge(&[0.3, 0.7, 0.5], 1.0).unwrap();
        let b = thermal_merge(&[0.7, 0.5, 0.3], 1.0).unwrap();
        let c = thermal_merge(&[0.5, 0.3, 0.7], 1.0).unwrap();
        assert!(
            (a - b).abs() < 1e-12 && (b - c).abs() < 1e-12,
            "LSE must be commutative: {a}, {b}, {c}"
        );
    }

    #[test]
    fn merge_monotonicity() {
        // Adding a higher score should not decrease the result.
        let base = thermal_merge(&[0.3, 0.5], 1.0).unwrap();
        let with_higher = thermal_merge(&[0.3, 0.5, 0.9], 1.0).unwrap();
        assert!(
            with_higher >= base,
            "LSE must be monotone: base={base}, with_higher={with_higher}"
        );
    }

    #[test]
    fn merge_approximates_max_at_low_temperature() {
        let scores = [0.2, 0.5, 0.9, 0.1];
        let result = thermal_merge(&scores, 0.001).unwrap();
        assert!(
            (result - 0.9).abs() < 0.01,
            "At very low T, LSE should approximate max: got {result}"
        );
    }

    #[test]
    fn merge_empty_returns_error() {
        let result = thermal_merge(&[], 1.0);
        assert!(matches!(result, Err(ThermalError::EmptyScores)));
    }

    #[test]
    fn merge_invalid_temperature() {
        assert!(matches!(
            thermal_merge(&[1.0], 0.0),
            Err(ThermalError::InvalidTemperature(_))
        ));
        assert!(matches!(
            thermal_merge(&[1.0], -1.0),
            Err(ThermalError::InvalidTemperature(_))
        ));
        assert!(matches!(
            thermal_merge(&[1.0], f64::NAN),
            Err(ThermalError::InvalidTemperature(_))
        ));
    }

    #[test]
    fn merge_single_score_equals_score() {
        // LSE of a single element at any temperature equals that element.
        // LSE = max + T * ln(exp(0)) = max + T * 0 = max
        let result = thermal_merge(&[0.42], 5.0).unwrap();
        assert!(
            (result - 0.42).abs() < 1e-12,
            "Single-element LSE should equal the element: {result}"
        );
    }

    // -- apply_decay ---------------------------------------------------------

    #[test]
    fn decay_monotonic_decrease() {
        let score = 1.0;
        let half_life = 60.0;
        let mut prev = score;
        for t in 1..=10 {
            let decayed = apply_decay(score, t as f64 * 10.0, half_life);
            assert!(
                decayed <= prev,
                "Decay must be monotonically decreasing: prev={prev}, current={decayed}"
            );
            prev = decayed;
        }
    }

    #[test]
    fn decay_approaches_zero() {
        let decayed = apply_decay(1.0, 10_000.0, 1.0);
        assert!(
            decayed < 1e-100,
            "After many half-lives the score should be near zero: {decayed}"
        );
    }

    #[test]
    fn decay_at_one_half_life() {
        let decayed = apply_decay(1.0, 60.0, 60.0);
        assert!(
            (decayed - 0.5).abs() < 1e-12,
            "After exactly one half-life the score should be 0.5: {decayed}"
        );
    }

    #[test]
    fn decay_zero_elapsed_unchanged() {
        let decayed = apply_decay(0.75, 0.0, 60.0);
        assert!(
            (decayed - 0.75).abs() < 1e-12,
            "Zero elapsed time should not change the score: {decayed}"
        );
    }

    #[test]
    fn decay_batch_matches_individual() {
        let scores = [0.1, 0.5, 0.9];
        let batch = apply_decay_batch(&scores, 30.0, 60.0);
        for (i, &s) in scores.iter().enumerate() {
            let individual = apply_decay(s, 30.0, 60.0);
            assert!(
                (batch[i] - individual).abs() < 1e-12,
                "Batch and individual decay must agree"
            );
        }
    }

    // -- gradient navigation -------------------------------------------------

    fn make_aggregate(path: &str, score: f64, locked: bool) -> ThermalAggregate {
        ThermalAggregate {
            file_path: path.to_string(),
            composite_score: score,
            max_score: score,
            agent_density: 0,
            last_updated: String::new(),
            is_locked: locked,
        }
    }

    fn make_agent(path: &str) -> AgentLogEntry {
        AgentLogEntry {
            id: 0,
            agent_id: String::from("a1"),
            command: String::new(),
            file_path: path.to_string(),
            status: String::from("running"),
            micro_heatmap: None,
            started_at: String::new(),
            completed_at: None,
            tokens_used: 0,
            cost_usd: 0.0,
        }
    }

    #[test]
    fn scaffolding_picks_coolest_unlocked() {
        let aggs = vec![
            make_aggregate("hot.rs", 0.9, false),
            make_aggregate("cold.rs", 0.1, false),
            make_aggregate("locked.rs", 0.05, true),
        ];
        let target = next_target(&aggs, &[], &[], ExecutionPhase::Scaffolding, 3);
        assert_eq!(target.as_deref(), Some("cold.rs"));
    }

    #[test]
    fn resolution_picks_hottest_unlocked() {
        let aggs = vec![
            make_aggregate("hot.rs", 0.9, false),
            make_aggregate("cold.rs", 0.1, false),
            make_aggregate("hotter_locked.rs", 0.95, true),
        ];
        let target = next_target(&aggs, &[], &[], ExecutionPhase::Resolution, 3);
        assert_eq!(target.as_deref(), Some("hot.rs"));
    }

    #[test]
    fn annealing_picks_lowest_density() {
        let aggs = vec![
            make_aggregate("busy.rs", 0.5, false),
            make_aggregate("idle.rs", 0.5, false),
        ];
        let agents = vec![make_agent("busy.rs"), make_agent("busy.rs")];
        let target = next_target(&aggs, &[], &agents, ExecutionPhase::Annealing, 5);
        assert_eq!(target.as_deref(), Some("idle.rs"));
    }

    #[test]
    fn next_target_respects_max_density() {
        let aggs = vec![make_aggregate("only.rs", 0.5, false)];
        let agents = vec![make_agent("only.rs"), make_agent("only.rs")];
        // max_density = 2 means file already at capacity.
        let target = next_target(&aggs, &[], &agents, ExecutionPhase::Resolution, 2);
        assert!(target.is_none());
    }

    #[test]
    fn next_target_excludes_cool_locked_files() {
        let aggs = vec![make_aggregate("src/lib.rs", 0.8, false)];
        let locks = vec![CoolLock {
            file_path: String::from("src/lib.rs"),
            line_start: 1,
            line_end: 100,
            locked_hash: String::from("abc"),
            locked_at: String::new(),
            locked_by_agent: String::from("a1"),
        }];
        let target = next_target(&aggs, &locks, &[], ExecutionPhase::Resolution, 5);
        assert!(target.is_none());
    }

    #[test]
    fn next_target_returns_none_when_empty() {
        let target = next_target(&[], &[], &[], ExecutionPhase::Resolution, 5);
        assert!(target.is_none());
    }

    // -- phase_from_progress -------------------------------------------------

    #[test]
    fn phase_boundaries() {
        assert_eq!(phase_from_progress(0, 100), ExecutionPhase::Scaffolding);
        assert_eq!(phase_from_progress(29, 100), ExecutionPhase::Scaffolding);
        assert_eq!(phase_from_progress(30, 100), ExecutionPhase::Resolution);
        assert_eq!(phase_from_progress(69, 100), ExecutionPhase::Resolution);
        assert_eq!(phase_from_progress(70, 100), ExecutionPhase::Annealing);
        assert_eq!(phase_from_progress(100, 100), ExecutionPhase::Annealing);
    }

    #[test]
    fn phase_zero_expected_defaults_to_resolution() {
        assert_eq!(phase_from_progress(5, 0), ExecutionPhase::Resolution);
    }

    // -- visualization helpers -----------------------------------------------

    #[test]
    fn score_bar_boundaries() {
        assert_eq!(score_bar(0.0), "[          ]");
        assert_eq!(score_bar(1.0), "[##########]");
        assert_eq!(score_bar(0.5), "[#####     ]");
    }

    #[test]
    fn score_bar_clamps() {
        assert_eq!(score_bar(-0.5), "[          ]");
        assert_eq!(score_bar(1.5), "[##########]");
    }

    #[test]
    fn score_to_color_ranges() {
        assert_eq!(score_to_color(0.0), Color::Blue);
        assert_eq!(score_to_color(0.24), Color::Blue);
        assert_eq!(score_to_color(0.25), Color::Cyan);
        assert_eq!(score_to_color(0.50), Color::Yellow);
        assert_eq!(score_to_color(0.75), Color::Red);
        assert_eq!(score_to_color(1.0), Color::Red);
    }

    #[test]
    fn truncate_path_short_unchanged() {
        assert_eq!(truncate_path("src/lib.rs", 20), "src/lib.rs");
    }

    #[test]
    fn truncate_path_long_gets_ellipsis() {
        let long = "a/very/long/deeply/nested/path/to/some/file.rs";
        let truncated = truncate_path(long, 20);
        assert!(truncated.starts_with("..."));
        assert_eq!(truncated.len(), 20);
    }

    #[test]
    fn ascii_heatmap_empty() {
        let output = render_heatmap_to_string(&[], &[]);
        assert_eq!(output, "(no thermal data)");
    }

    #[test]
    fn ascii_heatmap_renders_all_files() {
        let aggs = vec![
            make_aggregate("src/hot.rs", 0.85, false),
            make_aggregate("src/cold.rs", 0.15, true),
        ];
        let agents = vec![make_agent("src/hot.rs")];
        let output = render_heatmap_to_string(&aggs, &agents);

        assert!(output.contains("src/hot.rs"), "should contain file path");
        assert!(output.contains("src/cold.rs"), "should contain file path");
        assert!(output.contains("LOCKED"), "should show lock status");
        assert!(output.contains("UNLOCKED"), "should show unlock status");
        assert!(output.contains("agents: 1"), "should show agent count");
        assert!(output.contains("agents: 0"), "should show zero agents");
    }
}
