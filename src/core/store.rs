//! Store — SQLite (WAL) persistence for runs and events.
//!
//! One file at `~/.loopd/loopd.db`, in WAL mode. **All writes go through the
//! daemon** (single writer); WAL lets the TUI (`loop dash`) read concurrently
//! while the daemon writes. The CLI never opens the db directly — it talks to
//! the daemon's HTTP API, which owns the one `Store`.
//!
//! Two tables: `runs` (one row per loop, the aggregate [`Run`]) and `events`
//! (the append-only [`LoopEvent`] stream). Enums are stored as their serde
//! string form (e.g. `running`, `tool_use`) and `flags` as a JSON array, so the
//! on-disk representation matches the wire format. Ported from the prior TS
//! `store.ts` and extended to the firmed-up `ARCHITECTURE §3` model.

use anyhow::{Context, Result};
use rusqlite::{params, Connection, Row};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::config;
use crate::core::events::{LoopEvent, Run};

/// SQLite-backed store. Holds the single owned [`Connection`]; the daemon keeps
/// exactly one of these.
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open the default store at `~/.loopd/loopd.db`, creating `~/.loopd` and the
    /// schema as needed.
    pub fn open_default() -> Result<Self> {
        let dir = config::ensure_loopd_dir()?;
        Self::open(dir.join("loopd.db"))
    }

    /// Open (or create) a store at an explicit path. Used by `open_default` and
    /// by tests with a temp path.
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let path = path.as_ref();
        let conn = Connection::open(path)
            .with_context(|| format!("opening sqlite db {}", path.display()))?;
        // WAL = concurrent reads while the daemon writes; busy_timeout avoids
        // spurious "database is locked" under brief contention.
        conn.pragma_update(None, "journal_mode", "WAL")
            .context("setting journal_mode=WAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)
            .context("setting busy_timeout")?;
        let store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    /// Create the tables if they don't exist.
    fn migrate(&self) -> Result<()> {
        self.conn
            .execute_batch(
                "
            CREATE TABLE IF NOT EXISTS runs (
                run_id           TEXT PRIMARY KEY,
                label            TEXT NOT NULL,
                agent            TEXT NOT NULL,
                cwd              TEXT NOT NULL,
                status           TEXT NOT NULL,
                prompt           TEXT NOT NULL,
                pid              INTEGER,
                agent_session_id TEXT,
                model            TEXT,
                iteration        INTEGER NOT NULL DEFAULT 0,
                cost_usd         REAL NOT NULL DEFAULT 0,
                tokens_in        INTEGER NOT NULL DEFAULT 0,
                tokens_out       INTEGER NOT NULL DEFAULT 0,
                exit_code        INTEGER,
                run_reason       TEXT NOT NULL,
                parent_run_id    TEXT,
                branch           TEXT,
                worktree_path    TEXT,
                started_at       INTEGER NOT NULL,
                ended_at         INTEGER,
                last_event_at    INTEGER NOT NULL,
                updated_at       INTEGER NOT NULL,
                flags            TEXT NOT NULL DEFAULT '[]',
                kill_requested   INTEGER NOT NULL DEFAULT 0,
                owned            INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS events (
                id                 INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id             TEXT NOT NULL,
                source             TEXT NOT NULL,
                kind               TEXT NOT NULL,
                tool               TEXT,
                tool_input_hash    INTEGER,
                tool_status        TEXT,
                iteration          INTEGER,
                tokens_in          INTEGER,
                tokens_out         INTEGER,
                cost_usd           REAL,
                text               TEXT,
                parent_tool_use_id TEXT,
                ts                 INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_events_run ON events(run_id, id);
            ",
            )
            .context("creating schema")?;
        Ok(())
    }

    /// Append an event.
    pub fn insert_event(&self, e: &LoopEvent) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO events
                 (run_id, source, kind, tool, tool_input_hash, tool_status,
                  iteration, tokens_in, tokens_out, cost_usd, text,
                  parent_tool_use_id, ts)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    e.run_id,
                    enum_to_text(&e.source)?,
                    enum_to_text(&e.kind)?,
                    e.tool,
                    e.tool_input_hash.map(|h| h as i64),
                    e.tool_status.map(|s| enum_to_text(&s)).transpose()?,
                    e.iteration.map(|v| v as i64),
                    e.tokens_in.map(|v| v as i64),
                    e.tokens_out.map(|v| v as i64),
                    e.cost_usd,
                    e.text,
                    e.parent_tool_use_id,
                    e.ts,
                ],
            )
            .context("inserting event")?;
        Ok(())
    }

    /// Insert a run, or update every column if it already exists.
    pub fn upsert_run(&self, r: &Run) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO runs
                 (run_id, label, agent, cwd, status, prompt, pid, agent_session_id,
                  model, iteration, cost_usd, tokens_in, tokens_out, exit_code,
                  run_reason, parent_run_id, branch, worktree_path, started_at,
                  ended_at, last_event_at, updated_at, flags, kill_requested, owned)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                         ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25)
                 ON CONFLICT(run_id) DO UPDATE SET
                   label=?2, agent=?3, cwd=?4, status=?5, prompt=?6, pid=?7,
                   agent_session_id=?8, model=?9, iteration=?10, cost_usd=?11,
                   tokens_in=?12, tokens_out=?13, exit_code=?14, run_reason=?15,
                   parent_run_id=?16, branch=?17, worktree_path=?18, started_at=?19,
                   ended_at=?20, last_event_at=?21, updated_at=?22, flags=?23,
                   kill_requested=?24, owned=?25",
                params![
                    r.run_id,
                    r.label,
                    r.agent,
                    r.cwd,
                    enum_to_text(&r.status)?,
                    r.prompt,
                    r.pid.map(|v| v as i64),
                    r.agent_session_id,
                    r.model,
                    r.iteration as i64,
                    r.cost_usd,
                    r.tokens_in as i64,
                    r.tokens_out as i64,
                    r.exit_code,
                    enum_to_text(&r.run_reason)?,
                    r.parent_run_id,
                    r.branch,
                    r.worktree_path,
                    r.started_at,
                    r.ended_at,
                    r.last_event_at,
                    r.updated_at,
                    serde_json::to_string(&r.flags).context("serializing flags")?,
                    r.kill_requested as i64,
                    r.owned as i64,
                ],
            )
            .context("upserting run")?;
        Ok(())
    }

    /// Fetch a single run by id.
    pub fn get_run(&self, run_id: &str) -> Result<Option<Run>> {
        let res = self.conn.query_row(
            "SELECT * FROM runs WHERE run_id = ?1",
            params![run_id],
            row_to_run,
        );
        match res {
            Ok(run) => Ok(Some(run)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e).context("querying run"),
        }
    }

    /// List all runs, newest-started first.
    pub fn list_runs(&self) -> Result<Vec<Run>> {
        let mut stmt = self
            .conn
            .prepare("SELECT * FROM runs ORDER BY started_at DESC")
            .context("preparing list_runs")?;
        let rows = stmt
            .query_map([], row_to_run)
            .context("querying runs")?;
        let mut runs = Vec::new();
        for row in rows {
            runs.push(row.context("reading run row")?);
        }
        Ok(runs)
    }

    /// The most recent events for a run, newest first, capped at `limit`.
    pub fn events_for_run(&self, run_id: &str, limit: u32) -> Result<Vec<LoopEvent>> {
        let mut stmt = self
            .conn
            .prepare("SELECT * FROM events WHERE run_id = ?1 ORDER BY id DESC LIMIT ?2")
            .context("preparing events_for_run")?;
        let rows = stmt
            .query_map(params![run_id, limit], row_to_event)
            .context("querying events")?;
        let mut events = Vec::new();
        for row in rows {
            events.push(row.context("reading event row")?);
        }
        Ok(events)
    }

    /// Flag a run for kill. The supervisor acts on this on its next tick.
    pub fn request_kill(&self, run_id: &str) -> Result<()> {
        self.conn
            .execute(
                "UPDATE runs SET kill_requested = 1 WHERE run_id = ?1",
                params![run_id],
            )
            .context("requesting kill")?;
        Ok(())
    }
}

/// Serialize a unit enum to its serde string form (e.g. `RunStatus::Running`
/// -> `"running"`), without the surrounding JSON quotes.
fn enum_to_text<T: Serialize>(value: &T) -> Result<String> {
    match serde_json::to_value(value).context("serializing enum")? {
        serde_json::Value::String(s) => Ok(s),
        other => anyhow::bail!("expected a string enum, got {other}"),
    }
}

/// Parse a unit enum back from its serde string form.
fn text_to_enum<T: DeserializeOwned>(s: &str) -> rusqlite::Result<T> {
    serde_json::from_value(serde_json::Value::String(s.to_string())).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })
}

/// Map a `runs` row to a [`Run`]. Returns a `rusqlite::Result` so it composes
/// inside `query_row`/`query_map` closures; column errors propagate cleanly.
fn row_to_run(row: &Row) -> rusqlite::Result<Run> {
    let flags_json: String = row.get("flags")?;
    let flags: Vec<String> = serde_json::from_str(&flags_json).unwrap_or_default();
    Ok(Run {
        run_id: row.get("run_id")?,
        label: row.get("label")?,
        agent: row.get("agent")?,
        cwd: row.get("cwd")?,
        status: text_to_enum(&row.get::<_, String>("status")?)?,
        prompt: row.get("prompt")?,
        pid: row.get::<_, Option<i64>>("pid")?.map(|v| v as u32),
        agent_session_id: row.get("agent_session_id")?,
        model: row.get("model")?,
        iteration: row.get::<_, i64>("iteration")? as u32,
        cost_usd: row.get("cost_usd")?,
        tokens_in: row.get::<_, i64>("tokens_in")? as u32,
        tokens_out: row.get::<_, i64>("tokens_out")? as u32,
        exit_code: row.get("exit_code")?,
        run_reason: text_to_enum(&row.get::<_, String>("run_reason")?)?,
        parent_run_id: row.get("parent_run_id")?,
        branch: row.get("branch")?,
        worktree_path: row.get("worktree_path")?,
        started_at: row.get("started_at")?,
        ended_at: row.get("ended_at")?,
        last_event_at: row.get("last_event_at")?,
        updated_at: row.get("updated_at")?,
        flags,
        kill_requested: row.get::<_, i64>("kill_requested")? != 0,
        owned: row.get::<_, i64>("owned")? != 0,
    })
}

/// Map an `events` row to a [`LoopEvent`].
fn row_to_event(row: &Row) -> rusqlite::Result<LoopEvent> {
    let tool_status = match row.get::<_, Option<String>>("tool_status")? {
        Some(s) => Some(text_to_enum(&s)?),
        None => None,
    };
    Ok(LoopEvent {
        run_id: row.get("run_id")?,
        source: text_to_enum(&row.get::<_, String>("source")?)?,
        kind: text_to_enum(&row.get::<_, String>("kind")?)?,
        tool: row.get("tool")?,
        tool_input_hash: row.get::<_, Option<i64>>("tool_input_hash")?.map(|v| v as u64),
        tool_status,
        iteration: row.get::<_, Option<i64>>("iteration")?.map(|v| v as u32),
        tokens_in: row.get::<_, Option<i64>>("tokens_in")?.map(|v| v as u32),
        tokens_out: row.get::<_, Option<i64>>("tokens_out")?.map(|v| v as u32),
        cost_usd: row.get("cost_usd")?,
        text: row.get("text")?,
        parent_tool_use_id: row.get("parent_tool_use_id")?,
        ts: row.get("ts")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::events::{new_run_id, EventKind, RunStatus, Source, ToolStatus};

    /// Insert a run + 3 events into a temp db, read them back, assert. The
    /// isolated path keeps the assertions deterministic.
    #[test]
    fn insert_and_read_back() {
        let dir = std::env::temp_dir().join(format!("loopd_store_{}", new_run_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(dir.join("test.db")).expect("open store");

        let run_id = new_run_id();
        let mut run = Run::new(&run_id);
        run.agent = "claude".to_string();
        run.model = Some("claude-opus-4-8".to_string());
        run.flags = vec!["repeated-action".to_string()];
        run.owned = true;
        store.upsert_run(&run).expect("upsert run");

        for i in 0..3u32 {
            let mut ev = LoopEvent::new(&run_id, Source::Supervisor, EventKind::ToolUse);
            ev.iteration = Some(i);
            ev.tool = Some("bash".to_string());
            ev.tool_input_hash = Some(1000 + i as u64);
            ev.tool_status = Some(ToolStatus::Ok);
            store.insert_event(&ev).expect("insert event");
        }

        let got = store.get_run(&run_id).expect("get run").expect("run exists");
        assert_eq!(got.agent, "claude");
        assert_eq!(got.model.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(got.status, RunStatus::Running);
        assert_eq!(got.flags, vec!["repeated-action".to_string()]);
        assert!(got.owned);

        let events = store.events_for_run(&run_id, 100).expect("events");
        assert_eq!(events.len(), 3);
        assert!(events.iter().all(|e| e.tool.as_deref() == Some("bash")));
        assert!(events.iter().all(|e| e.tool_status == Some(ToolStatus::Ok)));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// upsert_run must update an existing row, not duplicate it.
    #[test]
    fn upsert_updates_in_place() {
        let dir = std::env::temp_dir().join(format!("loopd_store_{}", new_run_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(dir.join("test.db")).expect("open store");

        let run_id = new_run_id();
        let run = Run::new(&run_id);
        store.upsert_run(&run).expect("first upsert");

        let mut updated = store.get_run(&run_id).unwrap().unwrap();
        updated.status = RunStatus::Done;
        updated.cost_usd = 1.23;
        store.upsert_run(&updated).expect("second upsert");

        let all = store.list_runs().expect("list");
        let mine: Vec<_> = all.iter().filter(|r| r.run_id == run_id).collect();
        assert_eq!(mine.len(), 1, "upsert must not duplicate");
        assert_eq!(mine[0].status, RunStatus::Done);
        assert!((mine[0].cost_usd - 1.23).abs() < 1e-9);

        std::fs::remove_dir_all(&dir).ok();
    }
}
