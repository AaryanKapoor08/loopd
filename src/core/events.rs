//! The one normalized event model.
//!
//! Every surface loopd ingests — the Mode-A supervisor (PTY stream), the Mode-B
//! observer (CC hooks + transcript tailing), and the Surface-2 SDK — converges
//! here as exactly one type: [`LoopEvent`]. A [`Run`] is the aggregate view of a
//! single agent loop, rolled up from its events plus lifecycle metadata.
//!
//! These types are defined **once in Rust** and exported to the TS SDK via
//! `ts-rs` (see the `export_bindings` test below), so the wire format can never
//! drift between the daemon and its clients. The fields here are the firmed-up
//! set from `ARCHITECTURE.md §3`, validated against the live CC/Codex stream
//! schemas and vibe-kanban's `execution_processes`/`executor_sessions` model.
//!
//! Wire conventions (applied via serde, which `ts-rs` mirrors):
//! - structs serialize with `camelCase` field names (idiomatic JSON/TS),
//! - enums serialize as `snake_case` string variants.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::config::OnTrip;

/// Default context-window size, used until the adapter discovers the real one.
/// Conservative: current Claude models are mostly 1M (Haiku 4.5 is 200k), but
/// the authoritative figure arrives in `result.modelUsage[model]` at `RunEnd`,
/// so this only matters mid-run. See `context_window_for`.
pub const DEFAULT_CONTEXT_WINDOW: u32 = 200_000;
/// Context-window size for models running the 1M-token beta (their id carries
/// a `[1m]` marker, e.g. `claude-opus-4-8[1m]`).
pub const LARGE_CONTEXT_WINDOW: u32 = 1_000_000;

/// Best-effort context window for a model id, before the exact figure arrives in
/// `result.modelUsage` at `RunEnd`. `1_000_000` when the id carries the `[1m]`
/// marker, else [`DEFAULT_CONTEXT_WINDOW`].
pub fn context_window_for(model: &str) -> u32 {
    if model.contains("[1m]") {
        LARGE_CONTEXT_WINDOW
    } else {
        DEFAULT_CONTEXT_WINDOW
    }
}

// TODO(phase-3): cost precedence is currently resolved by `pricing.rs` as a
// fallback. Phase 3's `Adapter` should expose a static `reports_cost()`
// capability (CC=true via `total_cost_usd`, Codex=false) so the supervisor picks
// agent-reported vs computed cost by lookup, not a branch in the parser
// (vibe-kanban `BaseAgentCapability`). Reserved here; no behavior change yet.

/// Which ingestion surface produced an event. Lets the detector and dedup logic
/// reason about provenance (e.g. transcript is canonical for tokens; hooks are
/// low-latency liveness — see `ARCHITECTURE.md §4`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// Mode A — loopd spawned and owns the agent process (PTY stream).
    Supervisor,
    /// Mode B — a Claude Code hook POSTed to `/ingest`.
    Hook,
    /// Mode B — a line tailed from a transcript JSONL file.
    Transcript,
    /// Surface 2 — a programmatic loop reporting via `@loopd/sdk`.
    Sdk,
}

/// The kind of thing an event records. `ToolResult` is split out from `ToolUse`
/// so the detector can pair a call with its outcome (for error-streak and
/// repeated-action detection).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// The loop started (agent spawned / first transcript line / SDK `track`).
    RunStart,
    /// The loop ended (clean exit, error, or kill).
    RunEnd,
    /// An assistant turn (model output).
    Assistant,
    /// A user turn (prompt or tool result envelope).
    User,
    /// The agent invoked a tool.
    ToolUse,
    /// A tool returned (paired with a prior `ToolUse` via `tool`).
    ToolResult,
    /// Extended-thinking content.
    Thinking,
    /// Raw output text not otherwise classified.
    Output,
    /// An error surfaced by the agent or adapter.
    Error,
    /// A token-usage report (e.g. Codex `turn.completed`).
    TokenUsage,
    /// The agent stopped a turn (stop_reason).
    Stop,
}

/// The outcome of a tool call, assigned when a `ToolResult` is paired with its
/// `ToolUse`. Drives error-streak detection. Mirrors vibe-kanban's `ToolStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    /// The tool completed successfully.
    Ok,
    /// The tool returned an error (`is_error: true`).
    Error,
    /// The user/permission system denied the tool call.
    Denied,
    /// The tool call timed out.
    TimedOut,
}

/// The lifecycle state of a [`Run`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Actively running.
    Running,
    /// Finished cleanly.
    Done,
    /// Exited with an error / non-zero code.
    Failed,
    /// Stopped by loopd (the worst action loopd ever takes).
    Killed,
    /// Flagged stuck by the detector (runaway / no-progress).
    Stuck,
    /// Checkpointed and stopped; resumable via the agent's native `--resume`.
    Paused,
}

/// Why a run exists. `Retry` (v1) creates a *new* run with the same `prompt`,
/// `run_reason = Retry`, and `parent_run_id` pointing at the original.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum RunReason {
    /// A user-initiated `loop run`.
    UserRun,
    /// A retry of a prior run.
    Retry,
    /// An observed (Mode-B) run loopd did not start.
    Observed,
    /// A programmatic loop reporting via the SDK.
    Sdk,
}

/// The daemon's verdict on a run, returned by `POST /ingest`. This is the
/// enforcement return channel (ARCHITECTURE §4): a Mode-B hook reads it for
/// liveness, and the Phase-9 SDK's `.check()` throws/halts under `pause`/`kill`.
/// Mode B can only ever yield `ok`/`warn` (observed runs are read-only and their
/// on-trip action clamps to `notify`); `pause`/`kill` are reserved for owned SDK
/// runs. Modeled now so the wire shape is stable before the SDK consumes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Healthy — keep going.
    Ok,
    /// Flagged; surface a warning but keep going.
    Warn,
    /// The caller should checkpoint and stop (owned runs).
    Pause,
    /// The caller should stop for good (owned runs).
    Kill,
}

/// One normalized event. Every surface emits this shape; the store, detector,
/// and dashboard only ever see `LoopEvent`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct LoopEvent {
    /// The run this event belongs to.
    pub run_id: String,
    /// Which surface produced it.
    pub source: Source,
    /// What kind of event it is.
    pub kind: EventKind,
    /// Tool name, for `ToolUse`/`ToolResult`.
    pub tool: Option<String>,
    /// Stable hash of the tool's input args — feeds repeated-action / oscillation
    /// detection. The detector compares hashes; the SDK only displays it.
    pub tool_input_hash: Option<u64>,
    /// Outcome of a tool call (on `ToolResult`) — feeds error-streak detection.
    pub tool_status: Option<ToolStatus>,
    /// The agent turn number this event occurred in (see `ARCHITECTURE.md §8`).
    pub iteration: Option<u32>,
    /// Input tokens reported for this event, when available.
    pub tokens_in: Option<u32>,
    /// Output tokens reported for this event, when available.
    pub tokens_out: Option<u32>,
    /// Cost in USD — agent-reported when available, else computed via `pricing`.
    pub cost_usd: Option<f64>,
    /// Free-text payload (assistant text, thinking, error message, …).
    pub text: Option<String>,
    /// Sub-agent / sidechain attribution (CC `parent_tool_use_id` /
    /// transcript `isSidechain`) so sub-agents don't masquerade as top-level.
    pub parent_tool_use_id: Option<String>,
    /// Event timestamp (Unix epoch milliseconds).
    pub ts: i64,
}

impl LoopEvent {
    /// Build a minimal event for `run_id` of the given `kind`, stamped now. All
    /// optional fields start empty; callers set the ones their event carries.
    pub fn new(run_id: impl Into<String>, source: Source, kind: EventKind) -> Self {
        Self {
            run_id: run_id.into(),
            source,
            kind,
            tool: None,
            tool_input_hash: None,
            tool_status: None,
            iteration: None,
            tokens_in: None,
            tokens_out: None,
            cost_usd: None,
            text: None,
            parent_tool_use_id: None,
            ts: now_ms(),
        }
    }
}

/// The aggregate view of a single agent loop, rolled up from its events plus
/// lifecycle metadata. One row per loop in the store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct Run {
    /// Unique id (see [`new_run_id`]).
    pub run_id: String,
    /// Human-readable label (defaults to the run id).
    pub label: String,
    /// Which adapter drives this run (`claude`, `codex`, …).
    pub agent: String,
    /// Working directory the agent runs in.
    pub cwd: String,
    /// Lifecycle state.
    pub status: RunStatus,
    /// The task text — needed to retry/resume the loop.
    pub prompt: String,
    /// OS process id, when loopd owns the process (Mode A).
    pub pid: Option<u32>,
    /// CC `session_id` / Codex `thread_id` — enables native resume (= pause) and
    /// correlates Mode-B hook and transcript events to one run.
    pub agent_session_id: Option<String>,
    /// The model the agent reported using.
    pub model: Option<String>,
    /// Current iteration (agent turn) count.
    pub iteration: u32,
    /// Resolved cumulative cost in USD (agent-reported or computed).
    pub cost_usd: f64,
    /// Cumulative input tokens — the **summed** total (fresh + cache-creation +
    /// cache-read), not just fresh input. See `core::pricing::Usage::total_input`.
    pub tokens_in: u32,
    /// Cumulative output tokens.
    pub tokens_out: u32,
    /// Tokens currently occupying the model's context window. An estimate while
    /// the run is live; corrected to the exact figure from `result.modelUsage`
    /// at `RunEnd`. Powers the Phase-5 "context %" column and Phase-6
    /// context-exhaustion flag. Populated from Phase 3 onward.
    pub context_tokens: u32,
    /// The model's context window size. Default `200_000`; `1_000_000` when the
    /// model id carries the `[1m]` marker. A placeholder until `RunEnd`, when the
    /// authoritative window arrives in `result.modelUsage[model]` — don't assert
    /// on it mid-run (ARCHITECTURE.md §9; vibe-kanban `claude.rs`).
    pub context_window: u32,
    /// Process exit code, when known.
    pub exit_code: Option<i32>,
    /// Why this run exists.
    pub run_reason: RunReason,
    /// Sub-agent / retry lineage — the run this one derives from.
    pub parent_run_id: Option<String>,
    /// Per-run cap override: max agent iterations. `None` falls back to
    /// `config.defaults.caps.maxIterations` at detection time.
    pub max_iterations: Option<u32>,
    /// Per-run cap override: max cumulative cost in USD. `None` → config default.
    pub max_cost_usd: Option<f64>,
    /// Per-run cap override: max wall-clock minutes. `None` → config default.
    pub max_duration_min: Option<u32>,
    /// Per-run override for what happens when a cap/detector trips. `None` →
    /// `config.defaults.onTrip`.
    pub on_trip: Option<OnTrip>,
    /// Git branch (populated read-only from `gitBranch`).
    pub branch: Option<String>,
    /// Worktree path — reserved for v2 (worktree isolation is out of v1).
    pub worktree_path: Option<String>,
    /// When the run started (Unix epoch ms).
    pub started_at: i64,
    /// When the run ended (Unix epoch ms), if it has.
    pub ended_at: Option<i64>,
    /// Timestamp of the most recent event for this run.
    pub last_event_at: i64,
    /// Timestamp of the most recent write to this row.
    pub updated_at: i64,
    /// Detector flags currently raised on this run.
    pub flags: Vec<String>,
    /// Whether a kill has been requested (acted on by the supervisor).
    pub kill_requested: bool,
    /// Whether loopd owns the process. `false` = observed (read-only): on-trip
    /// actions clamp to `notify` since there's no process to act on.
    pub owned: bool,
}

impl Run {
    /// A fresh run with `run_id`, sensible defaults, and timestamps stamped now.
    /// Mirrors the old TS `defaultRun`; callers override the fields they know.
    pub fn new(run_id: impl Into<String>) -> Self {
        let run_id = run_id.into();
        let now = now_ms();
        Self {
            label: run_id.clone(),
            run_id,
            agent: "unknown".to_string(),
            cwd: String::new(),
            status: RunStatus::Running,
            prompt: String::new(),
            pid: None,
            agent_session_id: None,
            model: None,
            iteration: 0,
            cost_usd: 0.0,
            tokens_in: 0,
            tokens_out: 0,
            context_tokens: 0,
            // Conservative default until the adapter learns the real window.
            context_window: DEFAULT_CONTEXT_WINDOW,
            exit_code: None,
            run_reason: RunReason::UserRun,
            parent_run_id: None,
            max_iterations: None,
            max_cost_usd: None,
            max_duration_min: None,
            on_trip: None,
            branch: None,
            worktree_path: None,
            started_at: now,
            ended_at: None,
            last_event_at: now,
            updated_at: now,
            flags: Vec::new(),
            kill_requested: false,
            owned: false,
        }
    }
}

/// Current Unix time in milliseconds. Saturates to 0 before the epoch (which
/// never happens in practice) so callers never have to handle an error.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Generate a unique run id like `run_<nanos>_<counter>`. Uniqueness comes from
/// the nanosecond clock plus a process-global atomic counter, so two ids minted
/// in the same nanosecond still differ — no `rand` dependency needed.
pub fn new_run_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("run_{nanos:x}_{n:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_ids_are_unique() {
        let a = new_run_id();
        let b = new_run_id();
        assert_ne!(a, b);
        assert!(a.starts_with("run_"));
    }

    /// Export the wire types to the TS SDK. This is the `ts-rs` codegen step
    /// (`cargo test export_bindings`): it writes `LoopEvent`, `Run`, and all
    /// their enum dependencies into `sdk/src/types/` so `@loopd/sdk` builds
    /// against types generated from this Rust model — never hand-written ones.
    /// The SDK's npm `prebuild` runs this before `tsc` (export ordering, §8 Q9).
    #[test]
    fn export_bindings() {
        let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("sdk/src/types");
        std::fs::create_dir_all(&out).expect("create sdk/src/types");
        LoopEvent::export_all_to(&out).expect("export LoopEvent + deps");
        Run::export_all_to(&out).expect("export Run + deps");
        // The Mode-B / SDK ingest return channel (ARCHITECTURE §4).
        crate::observer::webhook::IngestResponse::export_all_to(&out)
            .expect("export IngestResponse + Verdict");
    }

    #[test]
    fn loop_event_round_trips_through_json() {
        let mut ev = LoopEvent::new("run_x", Source::Supervisor, EventKind::ToolUse);
        ev.tool = Some("bash".to_string());
        ev.tool_input_hash = Some(42);
        ev.tool_status = Some(ToolStatus::Ok);
        let json = serde_json::to_string(&ev).expect("serialize");
        // camelCase on the wire, snake_case enum variants.
        assert!(json.contains("\"toolInputHash\":42"));
        assert!(json.contains("\"kind\":\"tool_use\""));
        let back: LoopEvent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(ev, back);
    }
}
