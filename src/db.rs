//! SQLite thermal database — schema, CRUD operations, and shared types.
//!
//! This module owns the thermal schema (5 tables) and all public structs
//! that other modules import. It is the shared thermal field that every
//! agent, engine, and visualization component reads and writes.

use chrono::Utc;
use rusqlite::{params, Connection, Result as SqlResult};
use std::path::Path;
use thiserror::Error;

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

/// Errors originating from the thermal database layer.
#[derive(Error, Debug)]
pub enum DbError {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("thermal score {score} out of range [0.0, 1.0] for {file}:{line}")]
    ScoreOutOfRange { score: f64, file: String, line: u32 },

    #[error("cool zone lock conflict: {file} is locked by agent {locked_by}")]
    LockConflict { file: String, locked_by: String },
}

// ---------------------------------------------------------------------------
// Shared types
// ---------------------------------------------------------------------------

/// A single thermal score entry for a code region.
#[derive(Debug, Clone)]
pub struct ThermalScore {
    pub id: i64,
    pub file_path: String,
    pub line_start: u32,
    pub line_end: u32,
    pub score: f64,
    pub score_type: String,
    pub source_command: String,
    pub source_agent_id: String,
    pub created_at: String,
    pub decay_factor: f64,
}

/// Aggregated project-level thermal view per file.
#[derive(Debug, Clone)]
pub struct ThermalAggregate {
    pub file_path: String,
    pub composite_score: f64,
    pub max_score: f64,
    pub agent_density: i32,
    pub last_updated: String,
    pub is_locked: bool,
}

/// A single agent execution log entry.
#[derive(Debug, Clone)]
pub struct AgentLogEntry {
    pub id: i64,
    pub agent_id: String,
    pub command: String,
    pub file_path: String,
    pub status: String,
    pub micro_heatmap: Option<String>,
    pub started_at: String,
    pub completed_at: Option<String>,
    pub tokens_used: i64,
    pub cost_usd: f64,
}

/// Global swarm state snapshot.
#[derive(Debug, Clone)]
pub struct SwarmState {
    pub id: i64,
    pub total_agents_spawned: i64,
    pub active_agents: i64,
    pub total_tokens_used: i64,
    pub total_cost_usd: f64,
    pub global_temperature: f64,
    pub iteration_count: i64,
    pub started_at: String,
}

/// A cool-zone lock record.
#[derive(Debug, Clone)]
pub struct CoolLock {
    pub file_path: String,
    pub line_start: u32,
    pub line_end: u32,
    pub locked_hash: String,
    pub locked_at: String,
    pub locked_by_agent: String,
}

// ---------------------------------------------------------------------------
// Database handle
// ---------------------------------------------------------------------------

/// Handle wrapping a SQLite connection with thermal-schema operations.
pub struct ThermalDb {
    conn: Connection,
}

impl ThermalDb {
    /// Open (or create) the thermal database at the given path.
    pub fn open(path: &Path) -> Result<Self, DbError> {
        let conn = Connection::open(path)?;
        let db = Self { conn };
        db.initialize_schema()?;
        Ok(db)
    }

    /// Create an in-memory database (useful for tests).
    pub fn in_memory() -> Result<Self, DbError> {
        let conn = Connection::open_in_memory()?;
        let db = Self { conn };
        db.initialize_schema()?;
        Ok(db)
    }

    // -----------------------------------------------------------------------
    // Schema
    // -----------------------------------------------------------------------

    fn initialize_schema(&self) -> Result<(), DbError> {
        self.conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS thermal_map (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                file_path TEXT NOT NULL,
                line_start INTEGER NOT NULL,
                line_end INTEGER NOT NULL,
                score REAL NOT NULL CHECK(score >= 0.0 AND score <= 1.0),
                score_type TEXT NOT NULL CHECK(score_type IN ('complexity', 'dependency', 'risk', 'churn', 'test_coverage')),
                source_command TEXT NOT NULL,
                source_agent_id TEXT NOT NULL,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                decay_factor REAL DEFAULT 1.0,
                UNIQUE(file_path, line_start, line_end, score_type)
            );

            CREATE TABLE IF NOT EXISTS thermal_aggregate (
                file_path TEXT PRIMARY KEY,
                composite_score REAL NOT NULL,
                max_score REAL NOT NULL,
                agent_density INTEGER DEFAULT 0,
                last_updated TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                is_locked BOOLEAN DEFAULT FALSE
            );

            CREATE TABLE IF NOT EXISTS agent_log (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                agent_id TEXT NOT NULL,
                command TEXT NOT NULL,
                file_path TEXT NOT NULL,
                status TEXT CHECK(status IN ('spawned', 'running', 'success', 'failed', 'retrying')),
                micro_heatmap JSON,
                started_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                completed_at TIMESTAMP,
                tokens_used INTEGER DEFAULT 0,
                cost_usd REAL DEFAULT 0.0
            );

            CREATE TABLE IF NOT EXISTS swarm_state (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                total_agents_spawned INTEGER DEFAULT 0,
                active_agents INTEGER DEFAULT 0,
                total_tokens_used INTEGER DEFAULT 0,
                total_cost_usd REAL DEFAULT 0.0,
                global_temperature REAL DEFAULT 1.0,
                iteration_count INTEGER DEFAULT 0,
                started_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            );

            CREATE TABLE IF NOT EXISTS cool_locks (
                file_path TEXT NOT NULL,
                line_start INTEGER NOT NULL,
                line_end INTEGER NOT NULL,
                locked_hash TEXT NOT NULL,
                locked_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                locked_by_agent TEXT NOT NULL,
                PRIMARY KEY(file_path, line_start, line_end)
            );

            CREATE INDEX IF NOT EXISTS idx_thermal_score ON thermal_map(score DESC);
            CREATE INDEX IF NOT EXISTS idx_thermal_file ON thermal_map(file_path);
            CREATE INDEX IF NOT EXISTS idx_aggregate_score ON thermal_aggregate(composite_score DESC);
            ",
        )?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // thermal_map CRUD
    // -----------------------------------------------------------------------

    /// Insert or replace a thermal score entry.
    #[allow(clippy::too_many_arguments)]
    pub fn upsert_thermal_score(
        &self,
        file_path: &str,
        line_start: u32,
        line_end: u32,
        score: f64,
        score_type: &str,
        source_command: &str,
        source_agent_id: &str,
    ) -> Result<(), DbError> {
        if !(0.0..=1.0).contains(&score) {
            return Err(DbError::ScoreOutOfRange {
                score,
                file: file_path.to_string(),
                line: line_start,
            });
        }
        self.conn.execute(
            "INSERT OR REPLACE INTO thermal_map
             (file_path, line_start, line_end, score, score_type, source_command, source_agent_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                file_path,
                line_start,
                line_end,
                score,
                score_type,
                source_command,
                source_agent_id
            ],
        )?;
        Ok(())
    }

    /// Get all thermal scores for a file.
    pub fn get_scores_for_file(&self, file_path: &str) -> Result<Vec<ThermalScore>, DbError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, file_path, line_start, line_end, score, score_type,
                    source_command, source_agent_id, created_at, decay_factor
             FROM thermal_map WHERE file_path = ?1 ORDER BY line_start",
        )?;
        let rows = stmt.query_map(params![file_path], |row| {
            Ok(ThermalScore {
                id: row.get(0)?,
                file_path: row.get(1)?,
                line_start: row.get(2)?,
                line_end: row.get(3)?,
                score: row.get(4)?,
                score_type: row.get(5)?,
                source_command: row.get(6)?,
                source_agent_id: row.get(7)?,
                created_at: row.get(8)?,
                decay_factor: row.get(9)?,
            })
        })?;
        rows.collect::<SqlResult<Vec<_>>>().map_err(DbError::from)
    }

    /// Get all thermal scores across all files.
    pub fn get_all_scores(&self) -> Result<Vec<ThermalScore>, DbError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, file_path, line_start, line_end, score, score_type,
                    source_command, source_agent_id, created_at, decay_factor
             FROM thermal_map ORDER BY score DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(ThermalScore {
                id: row.get(0)?,
                file_path: row.get(1)?,
                line_start: row.get(2)?,
                line_end: row.get(3)?,
                score: row.get(4)?,
                score_type: row.get(5)?,
                source_command: row.get(6)?,
                source_agent_id: row.get(7)?,
                created_at: row.get(8)?,
                decay_factor: row.get(9)?,
            })
        })?;
        rows.collect::<SqlResult<Vec<_>>>().map_err(DbError::from)
    }

    /// Update the decay factor for a thermal score.
    pub fn update_decay_factor(&self, id: i64, new_decay: f64) -> Result<(), DbError> {
        self.conn.execute(
            "UPDATE thermal_map SET decay_factor = ?1 WHERE id = ?2",
            params![new_decay, id],
        )?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // thermal_aggregate CRUD
    // -----------------------------------------------------------------------

    /// Insert or replace an aggregate score for a file.
    pub fn upsert_aggregate(
        &self,
        file_path: &str,
        composite_score: f64,
        max_score: f64,
        agent_density: i32,
    ) -> Result<(), DbError> {
        self.conn.execute(
            "INSERT OR REPLACE INTO thermal_aggregate
             (file_path, composite_score, max_score, agent_density, last_updated)
             VALUES (?1, ?2, ?3, ?4, CURRENT_TIMESTAMP)",
            params![file_path, composite_score, max_score, agent_density],
        )?;
        Ok(())
    }

    /// Get all file aggregates ordered by composite score descending.
    pub fn get_all_aggregates(&self) -> Result<Vec<ThermalAggregate>, DbError> {
        let mut stmt = self.conn.prepare(
            "SELECT file_path, composite_score, max_score, agent_density, last_updated, is_locked
             FROM thermal_aggregate ORDER BY composite_score DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(ThermalAggregate {
                file_path: row.get(0)?,
                composite_score: row.get(1)?,
                max_score: row.get(2)?,
                agent_density: row.get(3)?,
                last_updated: row.get(4)?,
                is_locked: row.get(5)?,
            })
        })?;
        rows.collect::<SqlResult<Vec<_>>>().map_err(DbError::from)
    }

    /// Get aggregate for a single file.
    pub fn get_aggregate(&self, file_path: &str) -> Result<Option<ThermalAggregate>, DbError> {
        let mut stmt = self.conn.prepare(
            "SELECT file_path, composite_score, max_score, agent_density, last_updated, is_locked
             FROM thermal_aggregate WHERE file_path = ?1",
        )?;
        let mut rows = stmt.query_map(params![file_path], |row| {
            Ok(ThermalAggregate {
                file_path: row.get(0)?,
                composite_score: row.get(1)?,
                max_score: row.get(2)?,
                agent_density: row.get(3)?,
                last_updated: row.get(4)?,
                is_locked: row.get(5)?,
            })
        })?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    /// Lock a file in the aggregate table.
    pub fn lock_aggregate(&self, file_path: &str) -> Result<(), DbError> {
        self.conn.execute(
            "UPDATE thermal_aggregate SET is_locked = TRUE WHERE file_path = ?1",
            params![file_path],
        )?;
        Ok(())
    }

    /// Increment the agent density counter for a file.
    pub fn increment_density(&self, file_path: &str) -> Result<(), DbError> {
        self.conn.execute(
            "UPDATE thermal_aggregate SET agent_density = agent_density + 1 WHERE file_path = ?1",
            params![file_path],
        )?;
        Ok(())
    }

    /// Decrement the agent density counter for a file.
    pub fn decrement_density(&self, file_path: &str) -> Result<(), DbError> {
        self.conn.execute(
            "UPDATE thermal_aggregate SET agent_density = MAX(0, agent_density - 1) WHERE file_path = ?1",
            params![file_path],
        )?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // agent_log CRUD
    // -----------------------------------------------------------------------

    /// Log a new agent spawn.
    pub fn log_agent_spawn(
        &self,
        agent_id: &str,
        command: &str,
        file_path: &str,
    ) -> Result<i64, DbError> {
        self.conn.execute(
            "INSERT INTO agent_log (agent_id, command, file_path, status)
             VALUES (?1, ?2, ?3, 'spawned')",
            params![agent_id, command, file_path],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Update an agent's status.
    pub fn update_agent_status(
        &self,
        log_id: i64,
        status: &str,
        tokens_used: i64,
        cost_usd: f64,
        micro_heatmap: Option<&str>,
    ) -> Result<(), DbError> {
        let completed = if status == "success" || status == "failed" {
            Some(Utc::now().to_rfc3339())
        } else {
            None
        };
        self.conn.execute(
            "UPDATE agent_log SET status = ?1, tokens_used = ?2, cost_usd = ?3,
             micro_heatmap = ?4, completed_at = ?5 WHERE id = ?6",
            params![
                status,
                tokens_used,
                cost_usd,
                micro_heatmap,
                completed,
                log_id
            ],
        )?;
        Ok(())
    }

    /// Get all agent log entries.
    pub fn get_agent_logs(&self) -> Result<Vec<AgentLogEntry>, DbError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, agent_id, command, file_path, status, micro_heatmap,
                    started_at, completed_at, tokens_used, cost_usd
             FROM agent_log ORDER BY started_at DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(AgentLogEntry {
                id: row.get(0)?,
                agent_id: row.get(1)?,
                command: row.get(2)?,
                file_path: row.get(3)?,
                status: row.get(4)?,
                micro_heatmap: row.get(5)?,
                started_at: row.get(6)?,
                completed_at: row.get(7)?,
                tokens_used: row.get(8)?,
                cost_usd: row.get(9)?,
            })
        })?;
        rows.collect::<SqlResult<Vec<_>>>().map_err(DbError::from)
    }

    /// Get active agents (spawned or running).
    pub fn get_active_agents(&self) -> Result<Vec<AgentLogEntry>, DbError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, agent_id, command, file_path, status, micro_heatmap,
                    started_at, completed_at, tokens_used, cost_usd
             FROM agent_log WHERE status IN ('spawned', 'running') ORDER BY started_at",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(AgentLogEntry {
                id: row.get(0)?,
                agent_id: row.get(1)?,
                command: row.get(2)?,
                file_path: row.get(3)?,
                status: row.get(4)?,
                micro_heatmap: row.get(5)?,
                started_at: row.get(6)?,
                completed_at: row.get(7)?,
                tokens_used: row.get(8)?,
                cost_usd: row.get(9)?,
            })
        })?;
        rows.collect::<SqlResult<Vec<_>>>().map_err(DbError::from)
    }

    /// Count agents currently targeting a specific file.
    pub fn agent_density_at(&self, file_path: &str) -> Result<i32, DbError> {
        let count: i32 = self.conn.query_row(
            "SELECT COUNT(*) FROM agent_log
             WHERE file_path = ?1 AND status IN ('spawned', 'running')",
            params![file_path],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    // -----------------------------------------------------------------------
    // swarm_state CRUD
    // -----------------------------------------------------------------------

    /// Initialize a new swarm session.
    pub fn init_swarm(&self) -> Result<i64, DbError> {
        self.conn.execute(
            "INSERT INTO swarm_state (total_agents_spawned, active_agents, total_tokens_used, total_cost_usd)
             VALUES (0, 0, 0, 0.0)",
            [],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Get the current (latest) swarm state.
    pub fn get_swarm_state(&self) -> Result<Option<SwarmState>, DbError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, total_agents_spawned, active_agents, total_tokens_used,
                    total_cost_usd, global_temperature, iteration_count, started_at
             FROM swarm_state ORDER BY id DESC LIMIT 1",
        )?;
        let mut rows = stmt.query_map([], |row| {
            Ok(SwarmState {
                id: row.get(0)?,
                total_agents_spawned: row.get(1)?,
                active_agents: row.get(2)?,
                total_tokens_used: row.get(3)?,
                total_cost_usd: row.get(4)?,
                global_temperature: row.get(5)?,
                iteration_count: row.get(6)?,
                started_at: row.get(7)?,
            })
        })?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    /// Update swarm state counters.
    #[allow(clippy::too_many_arguments)]
    pub fn update_swarm_state(
        &self,
        swarm_id: i64,
        agents_spawned: i64,
        active: i64,
        tokens: i64,
        cost: f64,
        temperature: f64,
        iteration: i64,
    ) -> Result<(), DbError> {
        self.conn.execute(
            "UPDATE swarm_state SET total_agents_spawned = ?1, active_agents = ?2,
             total_tokens_used = ?3, total_cost_usd = ?4, global_temperature = ?5,
             iteration_count = ?6 WHERE id = ?7",
            params![
                agents_spawned,
                active,
                tokens,
                cost,
                temperature,
                iteration,
                swarm_id
            ],
        )?;
        Ok(())
    }

    /// Add cost and tokens to the current swarm session.
    pub fn add_cost(&self, swarm_id: i64, tokens: i64, cost: f64) -> Result<(), DbError> {
        self.conn.execute(
            "UPDATE swarm_state SET total_tokens_used = total_tokens_used + ?1,
             total_cost_usd = total_cost_usd + ?2 WHERE id = ?3",
            params![tokens, cost, swarm_id],
        )?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // cool_locks CRUD
    // -----------------------------------------------------------------------

    /// Lock a code region as a verified cool zone.
    pub fn insert_cool_lock(
        &self,
        file_path: &str,
        line_start: u32,
        line_end: u32,
        locked_hash: &str,
        locked_by_agent: &str,
    ) -> Result<(), DbError> {
        self.conn.execute(
            "INSERT OR REPLACE INTO cool_locks
             (file_path, line_start, line_end, locked_hash, locked_by_agent)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                file_path,
                line_start,
                line_end,
                locked_hash,
                locked_by_agent
            ],
        )?;
        Ok(())
    }

    /// Check if a file region is locked.
    pub fn is_locked(
        &self,
        file_path: &str,
        line_start: u32,
        line_end: u32,
    ) -> Result<bool, DbError> {
        let count: i32 = self.conn.query_row(
            "SELECT COUNT(*) FROM cool_locks
             WHERE file_path = ?1
               AND line_start <= ?3
               AND line_end >= ?2",
            params![file_path, line_start, line_end],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Check if an entire file is locked.
    pub fn is_file_locked(&self, file_path: &str) -> Result<bool, DbError> {
        let count: i32 = self.conn.query_row(
            "SELECT COUNT(*) FROM cool_locks WHERE file_path = ?1",
            params![file_path],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Get all cool locks.
    pub fn get_all_locks(&self) -> Result<Vec<CoolLock>, DbError> {
        let mut stmt = self.conn.prepare(
            "SELECT file_path, line_start, line_end, locked_hash, locked_at, locked_by_agent
             FROM cool_locks ORDER BY file_path, line_start",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(CoolLock {
                file_path: row.get(0)?,
                line_start: row.get(1)?,
                line_end: row.get(2)?,
                locked_hash: row.get(3)?,
                locked_at: row.get(4)?,
                locked_by_agent: row.get(5)?,
            })
        })?;
        rows.collect::<SqlResult<Vec<_>>>().map_err(DbError::from)
    }

    /// Remove a cool lock (e.g., if code is modified externally).
    pub fn remove_cool_lock(
        &self,
        file_path: &str,
        line_start: u32,
        line_end: u32,
    ) -> Result<(), DbError> {
        self.conn.execute(
            "DELETE FROM cool_locks WHERE file_path = ?1 AND line_start = ?2 AND line_end = ?3",
            params![file_path, line_start, line_end],
        )?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Utility queries
    // -----------------------------------------------------------------------

    /// Get files with composite score above a threshold.
    pub fn zones_above(&self, threshold: f64) -> Result<Vec<ThermalAggregate>, DbError> {
        let mut stmt = self.conn.prepare(
            "SELECT file_path, composite_score, max_score, agent_density, last_updated, is_locked
             FROM thermal_aggregate WHERE composite_score >= ?1 ORDER BY composite_score DESC",
        )?;
        let rows = stmt.query_map(params![threshold], |row| {
            Ok(ThermalAggregate {
                file_path: row.get(0)?,
                composite_score: row.get(1)?,
                max_score: row.get(2)?,
                agent_density: row.get(3)?,
                last_updated: row.get(4)?,
                is_locked: row.get(5)?,
            })
        })?;
        rows.collect::<SqlResult<Vec<_>>>().map_err(DbError::from)
    }

    /// Get files with composite score below a threshold.
    pub fn zones_below(&self, threshold: f64) -> Result<Vec<ThermalAggregate>, DbError> {
        let mut stmt = self.conn.prepare(
            "SELECT file_path, composite_score, max_score, agent_density, last_updated, is_locked
             FROM thermal_aggregate WHERE composite_score < ?1 ORDER BY composite_score ASC",
        )?;
        let rows = stmt.query_map(params![threshold], |row| {
            Ok(ThermalAggregate {
                file_path: row.get(0)?,
                composite_score: row.get(1)?,
                max_score: row.get(2)?,
                agent_density: row.get(3)?,
                last_updated: row.get(4)?,
                is_locked: row.get(5)?,
            })
        })?;
        rows.collect::<SqlResult<Vec<_>>>().map_err(DbError::from)
    }

    /// Get total cost across all swarm sessions.
    pub fn total_cost(&self) -> Result<f64, DbError> {
        let cost: f64 = self.conn.query_row(
            "SELECT COALESCE(SUM(total_cost_usd), 0.0) FROM swarm_state",
            [],
            |row| row.get(0),
        )?;
        Ok(cost)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_schema_creation() {
        let db = ThermalDb::in_memory().expect("should create in-memory db");
        let scores = db.get_all_scores().expect("should query empty table");
        assert!(scores.is_empty());
    }

    #[test]
    fn test_thermal_score_crud() {
        let db = ThermalDb::in_memory().unwrap();
        db.upsert_thermal_score("src/main.rs", 1, 50, 0.75, "complexity", "plan", "agent-1")
            .unwrap();
        let scores = db.get_scores_for_file("src/main.rs").unwrap();
        assert_eq!(scores.len(), 1);
        assert!((scores[0].score - 0.75).abs() < f64::EPSILON);
    }

    #[test]
    fn test_score_out_of_range() {
        let db = ThermalDb::in_memory().unwrap();
        let result = db.upsert_thermal_score("x.rs", 1, 10, 1.5, "complexity", "plan", "a1");
        assert!(result.is_err());
    }

    #[test]
    fn test_aggregate_crud() {
        let db = ThermalDb::in_memory().unwrap();
        db.upsert_aggregate("src/lib.rs", 0.65, 0.9, 2).unwrap();
        let agg = db.get_aggregate("src/lib.rs").unwrap().unwrap();
        assert!((agg.composite_score - 0.65).abs() < f64::EPSILON);
        assert_eq!(agg.agent_density, 2);
    }

    #[test]
    fn test_cool_lock_crud() {
        let db = ThermalDb::in_memory().unwrap();
        db.insert_cool_lock("src/lib.rs", 1, 100, "abc123", "agent-1")
            .unwrap();
        assert!(db.is_locked("src/lib.rs", 10, 20).unwrap());
        assert!(!db.is_locked("src/other.rs", 1, 10).unwrap());
        db.remove_cool_lock("src/lib.rs", 1, 100).unwrap();
        assert!(!db.is_locked("src/lib.rs", 10, 20).unwrap());
    }

    #[test]
    fn test_swarm_state() {
        let db = ThermalDb::in_memory().unwrap();
        let id = db.init_swarm().unwrap();
        db.add_cost(id, 1000, 0.05).unwrap();
        let state = db.get_swarm_state().unwrap().unwrap();
        assert_eq!(state.total_tokens_used, 1000);
        assert!((state.total_cost_usd - 0.05).abs() < f64::EPSILON);
    }

    #[test]
    fn test_agent_log() {
        let db = ThermalDb::in_memory().unwrap();
        let log_id = db.log_agent_spawn("agent-1", "fix", "src/main.rs").unwrap();
        db.update_agent_status(log_id, "success", 500, 0.02, None)
            .unwrap();
        let logs = db.get_agent_logs().unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].status, "success");
    }

    #[test]
    fn test_zones_above_below() {
        let db = ThermalDb::in_memory().unwrap();
        db.upsert_aggregate("hot.rs", 0.85, 0.95, 3).unwrap();
        db.upsert_aggregate("cold.rs", 0.15, 0.2, 0).unwrap();
        db.upsert_aggregate("mid.rs", 0.5, 0.6, 1).unwrap();

        let hot = db.zones_above(0.7).unwrap();
        assert_eq!(hot.len(), 1);
        assert_eq!(hot[0].file_path, "hot.rs");

        let cold = db.zones_below(0.3).unwrap();
        assert_eq!(cold.len(), 1);
        assert_eq!(cold[0].file_path, "cold.rs");
    }
}
