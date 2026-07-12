//! Transcript tailer (Mode B) — the **canonical** observer feed.
//!
//! User-started `claude` sessions write a JSONL transcript at
//! `~/.claude/projects/<munged-cwd>/<session_id>.jsonl`. This tailer watches that
//! tree (`notify`, with a poll fallback), reads only **appended** bytes, and
//! feeds each new line to the SAME [`crate::agents::claude`] `StreamParser` that
//! Mode A uses (§6 — one parser, two feeds). The transcript is canonical for
//! token/iteration/cost rollup (ARCHITECTURE §4); the hook feed (`webhook.rs`)
//! only supplies low-latency liveness, so nothing is ever double-counted.
//!
//! **Dedup (ARCHITECTURE §4).** Each transcript line carries a unique envelope
//! `uuid`; we skip any `(session_id, uuid)` already processed, so a `notify`
//! storm or a re-read can never count a turn twice. Per-file byte offsets mean we
//! only ever parse fresh appends, and seeding offsets to EOF at startup keeps the
//! tailer from importing months of historical sessions — it observes only what
//! happens from now on.
//!
//! **Safety.** Strictly read-only: loopd opens these files for reading, never
//! writes them, and never spawns the agents that produce them.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use notify::{RecursiveMode, Watcher};
use serde::Deserialize;

use crate::agents::{adapter_for, RunState, StreamParser};
use crate::core::events::{now_ms, LoopEvent, Source};
use crate::core::store::Store;

use super::observe_run;

/// How often the tailer scans for growth when no filesystem event wakes it.
const POLL: Duration = Duration::from_millis(750);

/// `~/.claude/projects` — where Claude Code writes session transcripts.
fn projects_dir() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".claude").join("projects"))
}

/// Spawn the transcript tailer on its own thread (mirrors the governance tick:
/// blocking file I/O, polls a stop flag for clean shutdown). Returns `None` if
/// the projects dir is absent — Mode B then runs on hooks alone (degraded: no
/// token rollup), which the daemon logs.
pub fn spawn(
    store: Arc<Mutex<Store>>,
    stop: Arc<AtomicBool>,
) -> Option<std::thread::JoinHandle<()>> {
    let projects = projects_dir()?;
    if !projects.is_dir() {
        tracing::info!(
            "transcript tailer idle — {} does not exist yet",
            projects.display()
        );
    }
    std::thread::Builder::new()
        .name("transcript".into())
        .spawn(move || run_tailer(store, projects, stop))
        .ok()
}

/// The tailer loop: set up a recursive watcher, then wake on either a filesystem
/// event or the poll timeout and reconcile file growth.
fn run_tailer(store: Arc<Mutex<Store>>, projects: PathBuf, stop: Arc<AtomicBool>) {
    let mut tailer = TranscriptTailer::new(store, projects.clone());
    tailer.seed_offsets(); // ignore pre-existing history; observe from now on.

    let (tx, rx) = std::sync::mpsc::channel();
    // Hold the watcher for the loop's lifetime. If it fails to start, fall back to
    // pure polling (the loop still runs on the timeout).
    let _watcher = match notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    }) {
        Ok(mut w) => match w.watch(&projects, RecursiveMode::Recursive) {
            Ok(()) => Some(w),
            Err(e) => {
                tracing::warn!("transcript watch failed ({e}); polling only");
                None
            }
        },
        Err(e) => {
            tracing::warn!("transcript watcher unavailable ({e}); polling only");
            None
        }
    };

    while !stop.load(Ordering::Relaxed) {
        // Drain any pending events (we don't need their detail — a scan reconciles
        // all growth), then scan. recv_timeout doubles as the poll fallback.
        match rx.recv_timeout(POLL) {
            Ok(_) => while rx.try_recv().is_ok() {},
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {}
        }
        tailer.scan();
    }
}

/// Per-observed-session state: the run it maps to, its stateful parser (the
/// canonical rollup lives here), and the set of line uuids already processed.
struct SessionState {
    run_id: String,
    parser: Box<dyn StreamParser>,
    seen_uuids: HashSet<String>,
}

/// The tailer's working state. Not shared across threads — it lives entirely on
/// the tailer thread; it talks to the rest of the daemon only through the store.
struct TranscriptTailer {
    store: Arc<Mutex<Store>>,
    projects: PathBuf,
    /// Bytes already consumed per transcript file (only complete lines advance it).
    offsets: HashMap<PathBuf, u64>,
    /// Per session id (= the transcript filename stem).
    sessions: HashMap<String, SessionState>,
}

impl TranscriptTailer {
    fn new(store: Arc<Mutex<Store>>, projects: PathBuf) -> Self {
        Self {
            store,
            projects,
            offsets: HashMap::new(),
            sessions: HashMap::new(),
        }
    }

    /// Record current sizes for every existing transcript so their historical
    /// content is never imported — we only ever process bytes appended afterward.
    fn seed_offsets(&mut self) {
        for path in self.transcript_files() {
            if let Ok(meta) = std::fs::metadata(&path) {
                self.offsets.insert(path, meta.len());
            }
        }
    }

    /// All `*.jsonl` transcripts under the projects tree (`projects/<dir>/*.jsonl`,
    /// plus any directly under `projects`).
    fn transcript_files(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        collect_jsonl(&self.projects, &mut out, 2);
        out
    }

    /// One reconcile pass: process appended bytes in every transcript that grew.
    fn scan(&mut self) {
        for path in self.transcript_files() {
            self.process_file(&path);
        }
    }

    /// Read and parse complete lines appended to `path` since the last offset.
    fn process_file(&mut self, path: &Path) {
        let Ok(meta) = std::fs::metadata(path) else {
            return;
        };
        let size = meta.len();
        let start = *self.offsets.get(path).unwrap_or(&0);
        if size < start {
            // Truncated/rotated (rare for CC) — restart from the top next pass.
            self.offsets.insert(path.to_path_buf(), 0);
            return;
        }
        if size == start {
            return; // no growth
        }

        let mut bytes = Vec::new();
        {
            let Ok(mut f) = std::fs::File::open(path) else {
                return;
            };
            if f.seek(SeekFrom::Start(start)).is_err() {
                return;
            }
            if f.take(size - start).read_to_end(&mut bytes).is_err() {
                return;
            }
        }
        // Only consume up to the last newline — the final line may still be
        // mid-write; leave it for the next pass.
        let Some(last_nl) = bytes.iter().rposition(|&b| b == b'\n') else {
            return; // no complete line yet
        };
        let consume = last_nl + 1;
        self.offsets
            .insert(path.to_path_buf(), start + consume as u64);

        // The session id is the filename stem (`<session_id>.jsonl`, verified §8 Q7).
        let Some(session_id) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_string)
        else {
            return;
        };
        let text = String::from_utf8_lossy(&bytes[..consume]);
        for line in text.lines() {
            let line = line.trim();
            if !line.is_empty() {
                self.process_line(&session_id, line);
            }
        }
    }

    /// Feed one transcript line to the session's parser and persist the result.
    fn process_line(&mut self, session_id: &str, line: &str) {
        let env: Envelope = serde_json::from_str(line).unwrap_or_default();

        // Find-or-create the observed run + its parser on first sight.
        if !self.sessions.contains_key(session_id) {
            let Some((run_id, _)) = observe_run(
                &self.store,
                session_id,
                env.cwd.as_deref(),
                env.git_branch.as_deref(),
            ) else {
                return;
            };
            // adapter_for("claude") is always Some in v1; routing seam for later.
            let Some(adapter) = adapter_for("claude") else {
                return;
            };
            // The parser stamps every event with the run id (not the session id).
            let parser = adapter.new_parser(&run_id);
            self.sessions.insert(
                session_id.to_string(),
                SessionState {
                    run_id,
                    parser,
                    seen_uuids: HashSet::new(),
                },
            );
        }
        let session = self.sessions.get_mut(session_id).expect("just inserted");

        // Dedup: skip a line we've already processed (notify storm / re-read).
        if let Some(uuid) = &env.uuid {
            if !session.seen_uuids.insert(uuid.clone()) {
                return;
            }
        }

        let mut events = session.parser.push(&format!("{line}\n"));
        // Re-stamp the provenance: the parser hardcodes Mode-A `Supervisor`.
        for ev in &mut events {
            ev.source = Source::Transcript;
        }
        let state = session.parser.run_state();
        let run_id = session.run_id.clone();

        persist_transcript(
            &self.store,
            &run_id,
            &events,
            &state,
            env.git_branch.as_deref(),
        );
    }
}

/// Insert the transcript events and roll the canonical metrics onto the run.
/// Re-reads under the store lock and only touches the fields the transcript owns
/// (model / session id / iterations / tokens / cost / context / branch), so it
/// never clobbers liveness the hook feed wrote.
fn persist_transcript(
    store: &Arc<Mutex<Store>>,
    run_id: &str,
    events: &[LoopEvent],
    state: &RunState,
    branch: Option<&str>,
) {
    let Ok(store) = store.lock() else {
        return;
    };
    for ev in events {
        let _ = store.insert_event(ev);
    }
    if let Ok(Some(mut run)) = store.get_run(run_id) {
        if let Some(model) = &state.model {
            run.model = Some(model.clone());
        }
        if let Some(sid) = &state.session_id {
            run.agent_session_id = Some(sid.clone());
        }
        run.iteration = state.iteration;
        run.tokens_in = state.tokens_in;
        run.tokens_out = state.tokens_out;
        run.context_tokens = state.context_tokens;
        if let Some(cost) = state.cost_usd {
            run.cost_usd = cost;
        }
        if let Some(cw) = state.context_window {
            run.context_window = cw;
        }
        if let Some(b) = branch {
            if !b.is_empty() {
                run.branch = Some(b.to_string());
            }
        }
        let now = now_ms();
        if !events.is_empty() {
            run.last_event_at = now;
            // A closed observed run that appends again is alive again (a session
            // resumed after SessionEnd / the idle-timeout close).
            super::revive_if_observed(&mut run);
        }
        run.updated_at = now;
        let _ = store.upsert_run(&run);
    }
}

/// The slim envelope we pre-parse off each line for routing/dedup/attribution
/// (the full message is parsed by the shared `StreamParser`).
#[derive(Debug, Default, Deserialize)]
struct Envelope {
    #[serde(default)]
    uuid: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default, alias = "gitBranch")]
    git_branch: Option<String>,
}

/// Recursively collect `*.jsonl` files up to `depth` directory levels deep.
fn collect_jsonl(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if depth > 0 {
                collect_jsonl(&path, out, depth - 1);
            }
        } else if path.extension().map(|e| e == "jsonl").unwrap_or(false) {
            out.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::events::{new_run_id, EventKind, RunReason, RunStatus};

    fn test_store() -> Arc<Mutex<Store>> {
        let dir = std::env::temp_dir().join(format!("loopd_tr_{}", new_run_id()));
        std::fs::create_dir_all(&dir).unwrap();
        Arc::new(Mutex::new(Store::open(dir.join("t.db")).unwrap()))
    }

    // Two real-shaped transcript lines (camelCase envelope, message.usage), an
    // assistant tool_use turn paired with its user tool_result.
    const A1: &str = r#"{"parentUuid":null,"isSidechain":false,"type":"assistant","message":{"model":"claude-opus-4-8","role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Read","input":{"file_path":"/x"}}],"usage":{"input_tokens":1000,"cache_read_input_tokens":500,"output_tokens":40}},"uuid":"u1","sessionId":"sess_t","cwd":"C:\\dev\\loop","gitBranch":"main"}"#;
    const U1: &str = r#"{"parentUuid":"u1","isSidechain":false,"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_1","type":"tool_result","content":"data"}]},"uuid":"u2","sessionId":"sess_t","cwd":"C:\\dev\\loop","gitBranch":"main"}"#;

    /// Write `lines` into a `<projects>/<proj>/<sid>.jsonl` and return (tailer, path).
    fn write_transcript(
        store: &Arc<Mutex<Store>>,
        sid: &str,
        lines: &[&str],
    ) -> (TranscriptTailer, PathBuf) {
        let projects = std::env::temp_dir().join(format!("loopd_proj_{}", new_run_id()));
        let proj = projects.join("C--dev-loop");
        std::fs::create_dir_all(&proj).unwrap();
        let path = proj.join(format!("{sid}.jsonl"));
        let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(&path, body).unwrap();
        (TranscriptTailer::new(store.clone(), projects), path)
    }

    #[test]
    fn appended_lines_create_an_observed_run_and_roll_up_tokens() {
        let store = test_store();
        let (mut tailer, path) = write_transcript(&store, "sess_t", &[A1, U1]);
        tailer.process_file(&path);

        let run = store
            .lock()
            .unwrap()
            .get_run_by_session_id("sess_t")
            .unwrap()
            .unwrap();
        assert!(!run.owned, "transcript-observed runs are read-only");
        assert_eq!(run.run_reason, RunReason::Observed);
        assert_eq!(run.status, RunStatus::Running);
        assert_eq!(run.model.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(run.branch.as_deref(), Some("main"));
        assert_eq!(run.iteration, 1);
        // total_input = 1000 fresh + 500 cache-read = 1500.
        assert_eq!(run.tokens_in, 1500);
        assert_eq!(run.tokens_out, 40);
        assert!(
            run.cost_usd > 0.0,
            "cost computed from usage (no result line)"
        );

        // Events carry the Transcript provenance, and the tool pair landed.
        let events = store
            .lock()
            .unwrap()
            .events_for_run(&run.run_id, 50)
            .unwrap();
        assert!(events.iter().all(|e| e.source == Source::Transcript));
        assert!(events.iter().any(|e| e.kind == EventKind::ToolUse));
        assert!(events.iter().any(|e| e.kind == EventKind::ToolResult));
    }

    #[test]
    fn reprocessing_the_same_lines_does_not_double_count() {
        let store = test_store();
        let (mut tailer, path) = write_transcript(&store, "sess_t", &[A1, U1]);
        tailer.process_file(&path);
        let before = store
            .lock()
            .unwrap()
            .get_run_by_session_id("sess_t")
            .unwrap()
            .unwrap();

        // Force a re-read of the whole file (offset reset), as a notify storm could.
        tailer.offsets.insert(path.clone(), 0);
        tailer.process_file(&path);
        let after = store
            .lock()
            .unwrap()
            .get_run_by_session_id("sess_t")
            .unwrap()
            .unwrap();

        // Dedup by (session_id, uuid) holds: tokens/iterations unchanged.
        assert_eq!(after.tokens_in, before.tokens_in);
        assert_eq!(after.iteration, before.iteration);
        let events = store
            .lock()
            .unwrap()
            .events_for_run(&after.run_id, 100)
            .unwrap();
        let tool_uses = events
            .iter()
            .filter(|e| e.kind == EventKind::ToolUse)
            .count();
        assert_eq!(tool_uses, 1, "the tool must not be re-emitted on re-read");
    }

    #[test]
    fn seed_offsets_ignores_pre_existing_history() {
        let store = test_store();
        let (mut tailer, _path) = write_transcript(&store, "sess_old", &[A1, U1]);
        // Seeding offsets to EOF, then scanning, must import nothing.
        tailer.seed_offsets();
        tailer.scan();
        assert!(
            store
                .lock()
                .unwrap()
                .get_run_by_session_id("sess_old")
                .unwrap()
                .is_none(),
            "historical transcript content must not be imported"
        );
    }
}
