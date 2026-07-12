//! Observer — Mode B (read-only).
//!
//! Surfaces agent sessions the user started themselves (e.g. `claude` in their
//! own terminal). loopd does not own these processes, so it can only watch:
//! runs ingested here are marked `owned = false` and governance actions degrade
//! to `notify`.
//!
//! Two feeds, deduplicated into one `LoopEvent` stream:
//! - hooks supply low-latency liveness via `POST /ingest`;
//! - the transcript JSONL is canonical for tokens/iterations.
//!
//! Contents:
//! - [`webhook`]    — normalize a Claude Code hook payload → `LoopEvent`
//!   (`POST /ingest`), and the verdict return channel.
//! - [`transcript`] — `notify`-watch `~/.claude/projects/**/*.jsonl`, feeding the
//!   same `StreamParser` as Mode A (the canonical token/iteration source).
//! - [`sdk`]        — Surface 2: register/report/poll a programmatic loop
//!   (`POST /sdk/track`, `POST /sdk/report`, `GET /sdk/runs/:id`). Not read-only
//!   like Mode B — it shares the same verdict channel, and the SDK *enforces* it
//!   (`pause`/`kill` make `check()` throw), since loopd owns no process to stop.
//!
//! Both feeds reconcile through the **store**: each finds-or-creates the one
//! observed run for a session via [`observe_run`], so a hook and the transcript
//! never produce two runs for one session. The store mutex (the daemon is the
//! only writer) serializes their writes.

pub mod sdk;
pub mod transcript;
pub mod webhook;

use std::sync::{Arc, Mutex};

use crate::core::events::{now_ms, Run, RunReason, RunStatus, Verdict};
use crate::core::store::Store;

/// Find-or-create the single observed run for a CC `session_id`. Returns the run
/// id and whether this call **created** it (so the caller can emit a one-time
/// `RunStart`). Holds the store lock across the read+insert so two first-sight
/// callers (a hook and the transcript tailer) can't race into two runs.
///
/// Created runs are `owned = false`, `run_reason = Observed`, `status = Running`
/// — loopd never started them, so the worst governance action stays `notify`
/// (the on-trip clamp in `core::detector`).
pub fn observe_run(
    store: &Arc<Mutex<Store>>,
    session_id: &str,
    cwd: Option<&str>,
    branch: Option<&str>,
) -> Option<(String, bool)> {
    let store = store.lock().ok()?;
    if let Ok(Some(run)) = store.get_run_by_session_id(session_id) {
        return Some((run.run_id, false));
    }
    let run_id = crate::core::events::new_run_id();
    let mut run = Run::new(&run_id);
    run.agent = "claude".to_string();
    run.owned = false;
    run.run_reason = RunReason::Observed;
    run.status = RunStatus::Running;
    run.agent_session_id = Some(session_id.to_string());
    run.label = observed_label(cwd, session_id);
    if let Some(c) = cwd {
        run.cwd = c.to_string();
    }
    run.branch = branch.map(str::to_string);
    let now = now_ms();
    run.started_at = now;
    run.last_event_at = now;
    run.updated_at = now;
    store.upsert_run(&run).ok()?;
    Some((run_id, true))
}

/// Bump a run's liveness timestamp (it's active right now) without touching the
/// canonical metrics the transcript owns. Used by the low-latency hook feed.
/// Fresh activity also **revives** an observed run that was previously closed
/// (SessionEnd / idle-timeout): the same session id resuming means the session
/// is alive again, so `Done` flips back to `Running`.
pub fn touch_run(store: &Arc<Mutex<Store>>, run_id: &str) {
    if let Ok(store) = store.lock() {
        if let Ok(Some(mut run)) = store.get_run(run_id) {
            let now = now_ms();
            revive_if_observed(&mut run);
            run.last_event_at = now;
            run.updated_at = now;
            let _ = store.upsert_run(&run);
        }
    }
}

/// Flip a closed **observed** run back to `Running` on fresh activity. Only the
/// benign `Done` close reopens — `Failed`/`Killed`/owned/SDK runs are left alone
/// (those ends carry meaning beyond "the session went quiet").
pub fn revive_if_observed(run: &mut Run) {
    if !run.owned && run.run_reason == RunReason::Observed && run.status == RunStatus::Done {
        run.status = RunStatus::Running;
        run.ended_at = None;
    }
}

/// The daemon's current verdict on a run (the `POST /ingest` return channel).
/// Observed runs only ever yield `Ok`/`Warn`; `Pause`/`Kill` are reserved for
/// owned SDK runs (Phase 9) — the shape is modeled now (ARCHITECTURE §4).
pub fn verdict_for(run: &Run) -> Verdict {
    if run.status == RunStatus::Killed || run.kill_requested {
        Verdict::Kill
    } else if run.status == RunStatus::Paused {
        Verdict::Pause
    } else if !run.flags.is_empty() {
        Verdict::Warn
    } else {
        Verdict::Ok
    }
}

/// A short human label for an observed run: the working-directory basename, else
/// a `session_id` prefix. Tagged `(observed)` so the cockpit reads clearly.
fn observed_label(cwd: Option<&str>, session_id: &str) -> String {
    let base = cwd
        .map(|c| c.trim_end_matches(['/', '\\']))
        .and_then(|c| c.rsplit(['/', '\\']).next())
        .filter(|s| !s.is_empty());
    match base {
        Some(name) => format!("{name} (observed)"),
        None => format!(
            "observed-{}",
            session_id.chars().take(8).collect::<String>()
        ),
    }
}
