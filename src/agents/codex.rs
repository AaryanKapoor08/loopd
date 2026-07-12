//! Codex adapter — maps the **verified** `codex exec --json` schema (§9) to
//! `LoopEvent`s, proving cross-vendor unification: a second vendor drops in as
//! one new adapter file with **no core change** (Phase 8). Modeled structurally
//! on [`super::claude`] — same line-buffering loop, same `pricing::Usage`/
//! `cost_of_usage`, same lenient non-JSON → `Output`, same `finish()` close.
//!
//! Captured live (codex-cli 0.137.0, 2026-06-20 — re-verify on upgrade). The
//! line taxonomy:
//! - `thread.started` (`thread_id`)        → `RunStart`; `thread_id` = session id.
//! - `turn.started`                        → iteration++ (an agent turn begins).
//! - `item.started`  (`item.type`)         → `ToolUse` for `command_execution`.
//! - `item.completed`(`item.type`)         → `command_execution`→`ToolResult`
//!   (status from `exit_code`), `agent_message`→`Assistant`, `reasoning`→`Thinking`.
//! - `turn.completed`(`usage`)             → `TokenUsage` (cost **computed** —
//!   Codex reports tokens only, never dollars → [`Adapter::reports_cost`] is `false`).
//!
//! Two facts the capture pinned that shape the mapping:
//! - **No `model` field appears anywhere in the stream.** Without one,
//!   `pricing.rs` can't resolve a price, so we stamp [`DEFAULT_CODEX_MODEL`] at
//!   `thread.started` (overridable if a future CLI starts emitting one).
//! - **`usage.input_tokens` is the *total* prompt count**; `cached_input_tokens`
//!   is the cached *subset* of it (OpenAI convention), unlike Claude where the
//!   buckets are disjoint. So `total_input = input_tokens` — we split out the
//!   cached portion only for the cheaper cache-read price, never add it on top.

use std::collections::{HashMap, VecDeque};

use serde::Deserialize;

use crate::core::events::{context_window_for, EventKind, LoopEvent, Source, ToolStatus};
use crate::core::pricing::{cost_of_usage, Usage as PriceUsage};

use super::{Adapter, RunOpts, RunState, StreamParser};

/// The model id Codex runs by default. The `exec --json` stream carries **no**
/// model field (verified live), so the parser stamps this so `pricing.rs` can
/// resolve a price (`gpt-5-codex` matches the `gpt-5` row). Update on Codex's
/// default-model changes; a per-run override belongs in config (`AgentConfig`).
const DEFAULT_CODEX_MODEL: &str = "gpt-5-codex";

/// The Codex adapter. Stateless factory; all per-run state lives in
/// [`CodexParser`].
pub struct CodexAdapter;

impl CodexAdapter {
    pub fn new() -> Self {
        Self
    }
}

impl Default for CodexAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl Adapter for CodexAdapter {
    fn id(&self) -> &str {
        "codex"
    }

    fn program(&self) -> &str {
        "codex"
    }

    fn build_args(&self, task: &str, _opts: &RunOpts) -> Vec<String> {
        // `--skip-git-repo-check` lets loopd spawn Codex regardless of the run
        // cwd (build_args has no cwd to test); harmless inside a repo.
        vec![
            "exec".to_string(),
            "--json".to_string(),
            "--skip-git-repo-check".to_string(),
            task.to_string(),
        ]
    }

    fn resume_args(&self, session_id: &str, task: &str, _opts: &RunOpts) -> Vec<String> {
        // `codex exec resume <thread_id>` continues the prior thread (pause =
        // stop + native resume; ARCHITECTURE §4).
        vec![
            "exec".to_string(),
            "resume".to_string(),
            session_id.to_string(),
            "--json".to_string(),
            "--skip-git-repo-check".to_string(),
            task.to_string(),
        ]
    }

    fn new_parser(&self, run_id: &str) -> Box<dyn StreamParser> {
        Box::new(CodexParser::new(run_id))
    }

    fn reports_cost(&self) -> bool {
        // Codex is token-only — there is no cost field on any line. The capability
        // flag turns cost into a one-line `pricing.rs` lookup, not a parser fork.
        false
    }

    fn env(&self) -> Vec<(String, String)> {
        // Codex (codex_core) is Rust/tracing-based; quiet its internal logs so
        // they don't interleave with the JSON stream. Harmless if ignored.
        vec![("RUST_LOG".to_string(), "error".to_string())]
    }
}

// --- the stateful parser ------------------------------------------------------

/// What we remember about an emitted `ToolUse` so the later result can be paired
/// to it (display name + a hash of the command for repeated-action detection).
#[derive(Clone)]
struct PendingTool {
    name: String,
    input_hash: Option<u64>,
}

/// One parser per run. Buffers partial PTY chunks, parses complete JSON lines,
/// pairs `command_execution` start→complete by item id, and rolls up [`RunState`].
pub struct CodexParser {
    run_id: String,
    /// Unparsed tail (a partial line awaiting its newline).
    buffer: String,
    state: RunState,
    /// Set once the `thread_id` is seen.
    session_pushed: bool,
    /// item `id` → pending tool, for `command_execution` start→complete pairing.
    tool_map: HashMap<String, PendingTool>,
    /// Insertion order of still-pending tool ids — back = most recent. Powers the
    /// missing-id fallback (crystal `getMostRecentPendingToolCall`): when a
    /// completing item carries no `id`, attribute it to the most-recent pending.
    pending_order: VecDeque<String>,
    /// Running computed-cost estimate (Codex never self-reports a dollar figure).
    computed_cost: f64,
    /// Whether a `RunStart` has been emitted yet.
    started: bool,
}

impl CodexParser {
    fn new(run_id: &str) -> Self {
        Self {
            run_id: run_id.to_string(),
            buffer: String::new(),
            state: RunState::default(),
            session_pushed: false,
            tool_map: HashMap::new(),
            pending_order: VecDeque::new(),
            computed_cost: 0.0,
            started: false,
        }
    }

    fn event(&self, kind: EventKind) -> LoopEvent {
        LoopEvent::new(self.run_id.clone(), Source::Supervisor, kind)
    }

    /// Take the pending tool a completing item pairs to. Prefer an exact `id`
    /// match; otherwise (id absent, or unknown) fall back to the **most-recent
    /// still-pending** call (crystal `getMostRecentPendingToolCall`) so a Codex
    /// result that omits its id never loses its `tool`/status pairing.
    fn take_pending(&mut self, id: Option<&str>) -> Option<PendingTool> {
        if let Some(id) = id {
            if let Some(pt) = self.tool_map.remove(id) {
                self.pending_order.retain(|p| p != id);
                return Some(pt);
            }
        }
        let recent = self.pending_order.pop_back()?;
        self.tool_map.remove(&recent)
    }

    /// Parse one complete, non-empty line into events.
    fn parse_line(&mut self, line: &str) -> Vec<LoopEvent> {
        let value: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => {
                // Lenient: Codex interleaves plain-text stderr/service lines.
                // Surface them, never crash (mirrors the CC parser).
                let mut ev = self.event(EventKind::Output);
                ev.text = Some(line.to_string());
                return vec![ev];
            }
        };
        // Older codex builds wrapped events as `{"msg":{"type":…}}`. Our live
        // 0.137.0 capture used the flat form only; handle the wrapper defensively
        // by unwrapping it so an upgrade/downgrade never silently drops events.
        let value = match value.get("msg").filter(|m| m.get("type").is_some()) {
            Some(msg) => msg.clone(),
            None => value,
        };
        let kind = value.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match kind {
            "thread.started" => self.on_thread_started(value),
            "turn.started" => {
                self.state.iteration = self.state.iteration.saturating_add(1);
                Vec::new()
            }
            "item.started" => self.on_item_started(value),
            "item.completed" => self.on_item_completed(value),
            "turn.completed" => self.on_turn_completed(value),
            // Fatal signals: a stream/auth error must surface, not be buried.
            "error" | "stream_error" | "turn.failed" | "thread.error" => self.on_error(&value),
            // Streaming/separator noise we deliberately drop (omit-partials stance,
            // same as the CC parser): `*_delta`, `agent_reasoning_section_break`,
            // bare `token_count`, and any other unknown `type` → never crash.
            _ => Vec::new(),
        }
    }

    /// Surface a Codex error line. A `401`/`Unauthorized` means the user must log
    /// in — name that explicitly (the reserved `ExecutorError::AuthRequired`
    /// taxonomy; the parser can't return it, so it emits a clear `Error` event the
    /// supervisor/CLI shows). A fatal error also closes the run.
    fn on_error(&mut self, value: &serde_json::Value) -> Vec<LoopEvent> {
        // The message can live under `message`, `error`, or `error.message`.
        let msg = value
            .get("message")
            .and_then(|m| m.as_str())
            .or_else(|| value.get("error").and_then(|e| e.as_str()))
            .or_else(|| value.pointer("/error/message").and_then(|m| m.as_str()))
            .unwrap_or("codex stream error")
            .to_string();
        let is_auth = msg.contains("401") || msg.to_ascii_lowercase().contains("unauthorized");
        let text = if is_auth {
            format!("codex needs you to log in first (run `codex login`): {msg}")
        } else {
            msg
        };
        let mut ev = self.event(EventKind::Error);
        ev.text = Some(text);
        self.state.ended = true;
        self.state.exit_ok = Some(false);
        vec![ev, self.event(EventKind::RunEnd)]
    }

    fn on_thread_started(&mut self, value: serde_json::Value) -> Vec<LoopEvent> {
        let line: ThreadStarted = match serde_json::from_value(value) {
            Ok(l) => l,
            Err(_) => return Vec::new(),
        };
        if !self.session_pushed {
            if let Some(tid) = line.thread_id {
                self.state.session_id = Some(tid);
                self.session_pushed = true;
            }
        }
        if self.started {
            return Vec::new();
        }
        // No model on the wire → stamp the known Codex default so pricing resolves.
        let model = DEFAULT_CODEX_MODEL.to_string();
        self.state.context_window = Some(context_window_for(&model));
        self.state.model = Some(model);
        self.started = true;
        vec![self.event(EventKind::RunStart)]
    }

    fn on_item_started(&mut self, value: serde_json::Value) -> Vec<LoopEvent> {
        let line: ItemLine = match serde_json::from_value(value) {
            Ok(l) => l,
            Err(_) => return Vec::new(),
        };
        let item = line.item;
        // Only a starting command execution is a `ToolUse`; agent_message/reasoning
        // arrive whole on `item.completed`.
        if item.item_type.as_deref() != Some("command_execution") {
            return Vec::new();
        }
        let display = "shell".to_string();
        let hash = item.command.as_ref().map(|c| hash_str(c));
        if let Some(id) = &item.id {
            self.tool_map.insert(
                id.clone(),
                PendingTool {
                    name: display.clone(),
                    input_hash: hash,
                },
            );
            self.pending_order.push_back(id.clone());
        }
        let mut ev = self.event(EventKind::ToolUse);
        ev.tool = Some(display);
        ev.tool_input_hash = hash;
        ev.iteration = Some(self.state.iteration);
        vec![ev]
    }

    fn on_item_completed(&mut self, value: serde_json::Value) -> Vec<LoopEvent> {
        let line: ItemLine = match serde_json::from_value(value) {
            Ok(l) => l,
            Err(_) => return Vec::new(),
        };
        let item = line.item;
        let iter = Some(self.state.iteration);
        match item.item_type.as_deref() {
            Some("command_execution") => {
                // Pair by id, else fall back to the most-recent pending call so a
                // result that omits its id still reports status (step 4).
                let pending = self.take_pending(item.id.as_deref());
                let mut ev = self.event(EventKind::ToolResult);
                ev.tool = pending
                    .as_ref()
                    .map(|p| p.name.clone())
                    .or(Some("shell".into()));
                ev.tool_input_hash = pending.and_then(|p| p.input_hash);
                // exit_code 0 (or absent) is success; any non-zero is an error.
                ev.tool_status = Some(match item.exit_code {
                    Some(0) | None => ToolStatus::Ok,
                    Some(_) => ToolStatus::Error,
                });
                ev.iteration = iter;
                vec![ev]
            }
            Some("agent_message") => match item.text {
                Some(text) if !text.trim().is_empty() => {
                    let mut ev = self.event(EventKind::Assistant);
                    ev.text = Some(text);
                    ev.iteration = iter;
                    vec![ev]
                }
                _ => Vec::new(),
            },
            Some("reasoning") => match item.text {
                Some(text) if !text.trim().is_empty() => {
                    let mut ev = self.event(EventKind::Thinking);
                    ev.text = Some(text);
                    ev.iteration = iter;
                    vec![ev]
                }
                _ => Vec::new(),
            },
            // file_change / mcp_tool_call / other item types — not mapped yet.
            _ => Vec::new(),
        }
    }

    fn on_turn_completed(&mut self, value: serde_json::Value) -> Vec<LoopEvent> {
        let line: TurnCompleted = match serde_json::from_value(value) {
            Ok(l) => l,
            Err(_) => return Vec::new(),
        };
        let usage = line.usage;
        // `input_tokens` is the TOTAL prompt; `cached_input_tokens` is its cached
        // subset (OpenAI convention). Split so the cached portion gets the cheaper
        // cache-read rate; `total_input()` then re-sums to exactly `input_tokens`
        // (no double count). Reasoning tokens are billed as output.
        let fresh = usage.input_tokens.saturating_sub(usage.cached_input_tokens);
        let out = usage
            .output_tokens
            .saturating_add(usage.reasoning_output_tokens);
        let pu = PriceUsage {
            input_tokens: fresh,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: usage.cached_input_tokens,
            output_tokens: out,
        };
        let turn_in = pu.total_input();
        self.state.tokens_in = self.state.tokens_in.saturating_add(turn_in);
        self.state.tokens_out = self.state.tokens_out.saturating_add(out);
        self.state.context_tokens = turn_in;
        if let Some(model) = &self.state.model {
            if let Some(c) = cost_of_usage(model, &pu) {
                self.computed_cost += c;
                self.state.cost_usd = Some(self.computed_cost);
            }
        }
        let mut ev = self.event(EventKind::TokenUsage);
        ev.tokens_in = Some(turn_in);
        ev.tokens_out = Some(out);
        ev.cost_usd = self.state.cost_usd;
        ev.iteration = Some(self.state.iteration);
        vec![ev]
    }
}

impl StreamParser for CodexParser {
    fn push(&mut self, chunk: &str) -> Vec<LoopEvent> {
        self.buffer.push_str(chunk);
        let mut events = Vec::new();
        while let Some(idx) = self.buffer.find('\n') {
            let raw: String = self.buffer.drain(..=idx).collect();
            let line = strip_ansi(&raw);
            let line = line.trim();
            if !line.is_empty() {
                events.extend(self.parse_line(line));
            }
        }
        events
    }

    fn finish(&mut self) -> Vec<LoopEvent> {
        let mut events = Vec::new();
        if !self.buffer.is_empty() {
            let raw = std::mem::take(&mut self.buffer);
            let line = strip_ansi(&raw);
            let line = line.trim().to_string();
            if !line.is_empty() {
                events.extend(self.parse_line(&line));
            }
        }
        // Codex emits no terminal `thread.completed`; the stream just ends at EOF.
        // Close the run ourselves so it never hangs in `Running` — the supervisor
        // sets the real exit code.
        if !self.state.ended {
            self.state.ended = true;
            events.push(self.event(EventKind::RunEnd));
        }
        events
    }

    fn session_id(&self) -> Option<&str> {
        self.state.session_id.as_deref()
    }

    fn run_state(&self) -> RunState {
        self.state.clone()
    }
}

// --- helpers ------------------------------------------------------------------

/// Stable hash of a string (a command), for repeated-action detection. Matches
/// the CC parser's `hash_input` shape so the Phase-6 detector treats both alike.
fn hash_str(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut hasher);
    hasher.finish()
}

/// Strip ANSI escape sequences a PTY may inject around the JSON. Lossy UTF-8 is
/// fine — we only need the text to `serde_json::from_str`.
fn strip_ansi(s: &str) -> String {
    String::from_utf8_lossy(&strip_ansi_escapes::strip(s.as_bytes())).into_owned()
}

// --- the lean subset of Codex's `exec --json` schema we deserialize -----------

#[derive(Deserialize)]
struct ThreadStarted {
    #[serde(default)]
    thread_id: Option<String>,
}

/// An `item.started` / `item.completed` envelope. The same struct deserializes
/// both; `exit_code`/`text`/`status` are only present on the relevant item types.
#[derive(Deserialize)]
struct ItemLine {
    item: Item,
}

#[derive(Deserialize)]
struct Item {
    #[serde(default)]
    id: Option<String>,
    #[serde(default, rename = "type")]
    item_type: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    exit_code: Option<i64>,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Deserialize)]
struct TurnCompleted {
    #[serde(default)]
    usage: CodexUsage,
}

#[derive(Deserialize, Default)]
struct CodexUsage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    cached_input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
    #[serde(default)]
    reasoning_output_tokens: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_all(p: &mut CodexParser, lines: &[&str]) -> Vec<LoopEvent> {
        let mut out = Vec::new();
        for l in lines {
            out.extend(p.push(&format!("{l}\n")));
        }
        out
    }

    // Pinned from a live `codex exec --json` capture (codex-cli 0.137.0,
    // 2026-06-20). Re-verify against `codex` on upgrade (§9 "re-verify on upgrade").
    const THREAD_STARTED: &str =
        r#"{"type":"thread.started","thread_id":"019ee76e-eb46-7102-b597-0e7cbe93f0a4"}"#;
    const TURN_STARTED: &str = r#"{"type":"turn.started"}"#;
    const ITEM_STARTED_CMD: &str = r#"{"type":"item.started","item":{"id":"item_0","type":"command_execution","command":"pwsh -Command 'echo hi'","aggregated_output":"","exit_code":null,"status":"in_progress"}}"#;
    const ITEM_COMPLETED_CMD: &str = r#"{"type":"item.completed","item":{"id":"item_0","type":"command_execution","command":"pwsh -Command 'echo hi'","aggregated_output":"hi\r\n","exit_code":0,"status":"completed"}}"#;
    const ITEM_COMPLETED_MSG: &str = r#"{"type":"item.completed","item":{"id":"item_1","type":"agent_message","text":"`echo hi` output:\n\n```text\nhi\n```"}}"#;
    const TURN_COMPLETED: &str = r#"{"type":"turn.completed","usage":{"input_tokens":75389,"cached_input_tokens":39168,"output_tokens":109,"reasoning_output_tokens":57}}"#;

    #[test]
    fn full_run_maps_to_events_and_rollup() {
        let mut p = CodexParser::new("run_cx_1");
        let mut events = parse_all(
            &mut p,
            &[
                THREAD_STARTED,
                TURN_STARTED,
                ITEM_STARTED_CMD,
                ITEM_COMPLETED_CMD,
                ITEM_COMPLETED_MSG,
                TURN_COMPLETED,
            ],
        );
        events.extend(p.finish());

        let kinds: Vec<EventKind> = events.iter().map(|e| e.kind).collect();
        assert!(kinds.contains(&EventKind::RunStart));
        assert!(kinds.contains(&EventKind::ToolUse));
        assert!(kinds.contains(&EventKind::ToolResult));
        assert!(kinds.contains(&EventKind::Assistant));
        assert!(kinds.contains(&EventKind::TokenUsage));
        assert!(kinds.contains(&EventKind::RunEnd)); // synthesized by finish()

        // session id = thread_id, exposed for pause/resume + Mode-B correlation.
        assert_eq!(p.session_id(), Some("019ee76e-eb46-7102-b597-0e7cbe93f0a4"));

        // command_execution start→complete paired by item id; exit 0 → Ok.
        let tr = events
            .iter()
            .find(|e| e.kind == EventKind::ToolResult)
            .unwrap();
        assert_eq!(tr.tool.as_deref(), Some("shell"));
        assert_eq!(tr.tool_status, Some(ToolStatus::Ok));

        let st = p.run_state();
        assert_eq!(st.model.as_deref(), Some("gpt-5-codex"));
        assert_eq!(st.iteration, 1); // one turn.started
                                     // total_input = input_tokens (cached is a subset, not additive) = 75389.
        assert_eq!(st.tokens_in, 75_389);
        // output + reasoning = 109 + 57 = 166 (reasoning billed as output).
        assert_eq!(st.tokens_out, 166);
        // The gate: Codex cost is COMPUTED from tokens, never blank.
        assert!(
            st.cost_usd.map(|c| c > 0.0).unwrap_or(false),
            "cost must compute"
        );
    }

    #[test]
    fn failed_command_yields_tool_status_error() {
        let mut p = CodexParser::new("run_cx_2");
        let failed = r#"{"type":"item.completed","item":{"id":"item_0","type":"command_execution","command":"false","aggregated_output":"","exit_code":1,"status":"completed"}}"#;
        let events = parse_all(
            &mut p,
            &[THREAD_STARTED, TURN_STARTED, ITEM_STARTED_CMD, failed],
        );
        let tr = events
            .iter()
            .find(|e| e.kind == EventKind::ToolResult)
            .unwrap();
        assert_eq!(tr.tool_status, Some(ToolStatus::Error));
    }

    #[test]
    fn result_without_id_pairs_to_most_recent_pending() {
        // Codex results sometimes omit the call id. The completing command (no
        // `id`) must still pair to the prior `item.started` and report Error on a
        // non-zero exit_code (step 4 — getMostRecentPendingToolCall fallback).
        let mut p = CodexParser::new("run_cx_6");
        let completed_no_id = r#"{"type":"item.completed","item":{"type":"command_execution","command":"false","aggregated_output":"boom","exit_code":2,"status":"completed"}}"#;
        let events = parse_all(
            &mut p,
            &[
                THREAD_STARTED,
                TURN_STARTED,
                ITEM_STARTED_CMD,
                completed_no_id,
            ],
        );
        let tr = events
            .iter()
            .find(|e| e.kind == EventKind::ToolResult)
            .expect("a result must be emitted even without an id");
        assert_eq!(tr.tool.as_deref(), Some("shell"));
        assert_eq!(tr.tool_status, Some(ToolStatus::Error));
    }

    #[test]
    fn auth_error_is_surfaced_not_buried() {
        let mut p = CodexParser::new("run_cx_7");
        let err = r#"{"type":"stream_error","message":"request failed: 401 Unauthorized"}"#;
        let events = parse_all(&mut p, &[THREAD_STARTED, err]);
        let e = events
            .iter()
            .find(|e| e.kind == EventKind::Error)
            .expect("a 401 must surface as an Error event");
        assert!(e.text.as_deref().unwrap().contains("log in"));
        // A fatal error closes the run.
        assert!(p.run_state().ended && p.run_state().exit_ok == Some(false));
    }

    #[test]
    fn non_json_line_becomes_output_not_panic() {
        let mut p = CodexParser::new("run_cx_3");
        let events = p.push("codex_core::codex some noisy log line\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, EventKind::Output);
    }

    #[test]
    fn cost_is_computed_not_reported() {
        // Codex never reports a dollar figure; reports_cost() is false and the
        // parser fills cost_usd from pricing.rs.
        assert!(!CodexAdapter::new().reports_cost());
        let mut p = CodexParser::new("run_cx_4");
        parse_all(&mut p, &[THREAD_STARTED, TURN_STARTED, TURN_COMPLETED]);
        assert!(p.run_state().cost_usd.is_some());
    }

    #[test]
    fn chunk_split_across_pushes_parses_once_complete() {
        let mut p = CodexParser::new("run_cx_5");
        let mid = THREAD_STARTED.len() / 2;
        assert!(p.push(&THREAD_STARTED[..mid]).is_empty());
        let events = p.push(&format!("{}\n", &THREAD_STARTED[mid..]));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, EventKind::RunStart);
    }
}
