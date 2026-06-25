//! Claude Code adapter — maps the **verified** `stream-json` schema (§9) to
//! `LoopEvent`s. Invoked headless: `claude -p "<task>" --output-format
//! stream-json --verbose` (verbose is required for the full stream).
//!
//! The parser is deliberately **lean**: because we never pass
//! `--include-partial-messages`, every meaningful unit arrives as a whole
//! `assistant`/`user`/`result` line, so we skip the entire `stream_event`
//! streaming state machine that a partials-based parser needs (§6 "simplification
//! win"). We parse complete JSON lines and roll up [`RunState`].
//!
//! Robustness rules (from the vibe-kanban study + §9 "re-verify on upgrade"):
//! - lines are buffered until newline, then ANSI-stripped before parsing;
//! - a line that isn't JSON becomes an `Output` event, never a panic;
//! - unknown `type`s / content blocks are skipped, not errors;
//! - `user` lines flagged `isReplay` are dropped (resume replays history — they'd
//!   double-count tokens/tools, ARCHITECTURE §4);
//! - tokens/cost/iterations fold into the run total **only** when
//!   `parent_tool_use_id` is `None` (subagent exclusion);
//! - `mcp__server__tool` is displayed as `mcp:server:tool`.






use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

use crate::core::events::{context_window_for, EventKind, LoopEvent, Source, ToolStatus};
use crate::core::pricing::{cost_of_usage, Usage as PriceUsage};

use super::{Adapter, RunOpts, RunState, StreamParser};

/// The Claude Code adapter. Stateless factory; all per-run state lives in
/// [`ClaudeParser`].
pub struct ClaudeAdapter;

impl ClaudeAdapter {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ClaudeAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl Adapter for ClaudeAdapter {
    fn id(&self) -> &str {
        "claude"
    }

    fn program(&self) -> &str {
        "claude"
    }

    fn build_args(&self, task: &str, opts: &RunOpts) -> Vec<String> {
        let mut args = vec![
            "-p".to_string(),
            task.to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--verbose".to_string(),
        ];
        if let Some(model) = &opts.model {
            args.push("--model".to_string());
            args.push(model.clone());
        }
        args
    }

    fn resume_args(&self, session_id: &str, task: &str, opts: &RunOpts) -> Vec<String> {
        // `--resume <id>` replays prior history (parser drops the `isReplay`
        // lines) then continues with the new `-p` prompt.
        let mut args = vec!["--resume".to_string(), session_id.to_string()];
        args.extend(self.build_args(task, opts));
        args
    }

    fn new_parser(&self, run_id: &str) -> Box<dyn StreamParser> {
        Box::new(ClaudeParser::new(run_id))
    }

    fn reports_cost(&self) -> bool {
        // CC reports `total_cost_usd` on the `result` line; trust it over pricing.
        true
    }

    fn env(&self) -> Vec<(String, String)> {
        // Mute npx/npm chatter so it doesn't interleave with the JSON stream.
        vec![("NPM_CONFIG_LOGLEVEL".to_string(), "error".to_string())]
    }

    /// Mode-B routing: a Claude Code transcript is a `*.jsonl` under a `projects`
    /// directory (`~/.claude/projects/<munged-cwd>/<session_id>.jsonl`). The same
    /// [`ClaudeParser`] consumes it — one parser, two feeds (ARCHITECTURE §6).
    fn match_transcript(&self, path: &Path) -> bool {
        path.extension().map(|e| e == "jsonl").unwrap_or(false)
            && path.components().any(|c| c.as_os_str() == "projects")
    }

    /// Mode-B routing: pull the agent session id out of a CC hook payload. Every
    /// CC hook event carries `session_id` (verified live, §8 Q7), which is our
    /// `agent_session_id` correlation key.
    fn match_hook(&self, payload: &serde_json::Value) -> Option<String> {
        payload
            .get("session_id")
            .and_then(|v| v.as_str())
            .map(str::to_string)
    }
}

// --- the stateful parser ------------------------------------------------------

/// What we remember about an emitted `tool_use` so the later `tool_result` can be
/// paired to it (name for display, input hash for repeated-action detection).
#[derive(Clone)]
struct PendingTool {
    name: String,
    input_hash: Option<u64>,
}

/// One parser per run. Buffers partial PTY chunks, parses complete JSON lines,
/// pairs tools, and rolls up [`RunState`].
pub struct ClaudeParser {
    run_id: String,
    /// Unparsed tail (a partial line awaiting its newline).
    buffer: String,
    state: RunState,
    /// Set once the first line carrying a `session_id` is seen.
    session_pushed: bool,
    /// agent `tool_use_id` → pending tool, for call→result pairing.
    tool_map: HashMap<String, PendingTool>,
    /// Running computed-cost estimate (used live, and as the cost for agents that
    /// don't self-report; CC overwrites it with `total_cost_usd` at `RunEnd`).
    computed_cost: f64,
    /// Whether a `RunStart` has been emitted yet.
    started: bool,
}

impl ClaudeParser {
    fn new(run_id: &str) -> Self {
        Self {
            run_id: run_id.to_string(),
            buffer: String::new(),
            state: RunState::default(),
            session_pushed: false,
            tool_map: HashMap::new(),
            computed_cost: 0.0,
            started: false,
        }
    }

    fn event(&self, kind: EventKind) -> LoopEvent {
        LoopEvent::new(self.run_id.clone(), Source::Supervisor, kind)
    }

    /// Record a session id from any line that carries one (first wins).
    fn capture_session(&mut self, sid: &Option<String>) {
        if !self.session_pushed {
            if let Some(sid) = sid {
                self.state.session_id = Some(sid.clone());
                self.session_pushed = true;
            }
        }
    }

    /// Parse one complete, ANSI-stripped, non-empty line into events.
    fn parse_line(&mut self, line: &str) -> Vec<LoopEvent> {
        let value: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => {
                // Lenient: agents (and routers) interleave plain-text service
                // lines. Surface, never crash.
                let mut ev = self.event(EventKind::Output);
                ev.text = Some(line.to_string());
                return vec![ev];
            }
        };
        let kind = value.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match kind {
            "system" => self.on_system(value),
            "assistant" => self.on_assistant(value),
            "user" => self.on_user(value),
            "result" => self.on_result(value),
            // stream_event / rate_limit_event / hook_* / control_* — not needed.
            _ => Vec::new(),
        }
    }

    fn on_system(&mut self, value: serde_json::Value) -> Vec<LoopEvent> {
        let line: SystemLine = match serde_json::from_value(value) {
            Ok(l) => l,
            Err(_) => return Vec::new(),
        };
        self.capture_session(&line.session_id);
        // Only the `init` subtype starts the run; hook_started/hook_response etc.
        // carry no run-level signal we need.
        if line.subtype.as_deref() == Some("init") && !self.started {
            if let Some(model) = line.model {
                self.state.context_window = Some(context_window_for(&model));
                self.state.model = Some(model);
            }
            self.started = true;
            return vec![self.event(EventKind::RunStart)];
        }
        Vec::new()
    }

    fn on_assistant(&mut self, value: serde_json::Value) -> Vec<LoopEvent> {
        let line: AssistantLine = match serde_json::from_value(value) {
            Ok(l) => l,
            Err(_) => return Vec::new(),
        };
        self.capture_session(&line.session_id);
        // Main-run turn = not a sub-agent. Mode A marks sub-agents with
        // `parent_tool_use_id`; the Mode-B transcript marks them with the
        // envelope `isSidechain` flag (verified, §8 Q8). Either excludes the turn
        // from the run's token/iteration rollup.
        let is_main = line.parent_tool_use_id.is_none() && !line.is_sidechain;
        if is_main {
            self.state.iteration = self.state.iteration.saturating_add(1);
        }
        if self.state.model.is_none() {
            self.state.model = line.message.model.clone();
        }
        // Transcripts carry no `system/init` line, so the window is never seeded
        // there. Derive a best-effort window from the model id once it's known
        // (Mode A already has it from `init`, so this is a no-op there).
        if self.state.context_window.is_none() {
            if let Some(model) = &self.state.model {
                self.state.context_window = Some(context_window_for(model));
            }
        }

        let mut events = Vec::new();
        let iter = Some(self.state.iteration);
        for block in line.message.content.blocks() {
            match block {
                Block::Text { text } if !text.trim().is_empty() => {
                    let mut ev = self.event(EventKind::Assistant);
                    ev.text = Some(text.clone());
                    ev.iteration = iter;
                    ev.parent_tool_use_id = line.parent_tool_use_id.clone();
                    events.push(ev);
                }
                Block::Thinking { thinking } if !thinking.trim().is_empty() => {
                    let mut ev = self.event(EventKind::Thinking);
                    ev.text = Some(thinking.clone());
                    ev.iteration = iter;
                    ev.parent_tool_use_id = line.parent_tool_use_id.clone();
                    events.push(ev);
                }
                Block::ToolUse { id, name, input } => {
                    let display = display_tool(name);
                    let hash = hash_input(input);
                    self.tool_map.insert(
                        id.clone(),
                        PendingTool {
                            name: display.clone(),
                            input_hash: Some(hash),
                        },
                    );
                    let mut ev = self.event(EventKind::ToolUse);
                    ev.tool = Some(display);
                    ev.tool_input_hash = Some(hash);
                    ev.iteration = iter;
                    ev.parent_tool_use_id = line.parent_tool_use_id.clone();
                    events.push(ev);
                }
                _ => {}
            }
        }

        // Token/cost accounting: main run only (subagent turns double-count).
        if is_main {
            if let Some(usage) = line.message.usage.as_ref() {
                let pu = usage.to_price_usage();
                let turn_in = pu.total_input();
                self.state.tokens_in = self.state.tokens_in.saturating_add(turn_in);
                self.state.tokens_out = self.state.tokens_out.saturating_add(pu.output_tokens);
                // Current context occupancy ≈ this turn's full input.
                self.state.context_tokens = turn_in;
                if let Some(model) = &self.state.model {
                    if let Some(c) = cost_of_usage(model, &pu) {
                        self.computed_cost += c;
                        self.state.cost_usd = Some(self.computed_cost);
                    }
                }
                let mut ev = self.event(EventKind::TokenUsage);
                ev.tokens_in = Some(turn_in);
                ev.tokens_out = Some(pu.output_tokens);
                ev.cost_usd = self.state.cost_usd;
                ev.iteration = iter;
                events.push(ev);
            }
        }
        events
    }

    fn on_user(&mut self, value: serde_json::Value) -> Vec<LoopEvent> {
        let line: UserLine = match serde_json::from_value(value) {
            Ok(l) => l,
            Err(_) => return Vec::new(),
        };
        self.capture_session(&line.session_id);
        // `--resume` replays prior user lines; never re-process them.
        if line.is_replay || line.is_synthetic {
            return Vec::new();
        }
        let mut events = Vec::new();
        let iter = Some(self.state.iteration);
        for block in line.message.content.blocks() {
            if let Block::ToolResult {
                tool_use_id,
                is_error,
                ..
            } = block
            {
                let pending = self.tool_map.remove(tool_use_id);
                let mut ev = self.event(EventKind::ToolResult);
                ev.tool = pending.as_ref().map(|p| p.name.clone());
                ev.tool_input_hash = pending.and_then(|p| p.input_hash);
                ev.tool_status = Some(if is_error.unwrap_or(false) {
                    ToolStatus::Error
                } else {
                    ToolStatus::Ok
                });
                ev.iteration = iter;
                ev.parent_tool_use_id = line.parent_tool_use_id.clone();
                events.push(ev);
            }
        }
        events
    }

    fn on_result(&mut self, value: serde_json::Value) -> Vec<LoopEvent> {
        let line: ResultLine = match serde_json::from_value(value) {
            Ok(l) => l,
            Err(_) => return Vec::new(),
        };
        self.capture_session(&line.session_id);

        // CC's authoritative figures arrive here.
        if let Some(cost) = line.total_cost_usd {
            self.state.cost_usd = Some(cost);
        }
        if let Some(turns) = line.num_turns {
            // Trust CC's own turn count when present.
            self.state.iteration = turns;
        }
        // Exact context window for the model we ran.
        if let Some(map) = &line.model_usage {
            let exact = self
                .state
                .model
                .as_ref()
                .and_then(|m| map.get(m))
                .or_else(|| map.values().next())
                .and_then(|mu| mu.context_window);
            if let Some(cw) = exact {
                self.state.context_window = Some(cw);
            }
        }
        let is_error = line.is_error.unwrap_or(false) || line.subtype.as_deref() == Some("error");
        self.state.ended = true;
        self.state.exit_ok = Some(!is_error);

        let mut ev = self.event(if is_error {
            EventKind::Error
        } else {
            EventKind::RunEnd
        });
        ev.cost_usd = self.state.cost_usd;
        ev.tokens_in = Some(self.state.tokens_in);
        ev.tokens_out = Some(self.state.tokens_out);
        ev.iteration = Some(self.state.iteration);
        if is_error {
            ev.text = line.error.or_else(|| line.result.map(|r| r.to_string()));
        }
        // An `error` result still ends the run — emit RunEnd after the Error.
        if is_error {
            let end = self.event(EventKind::RunEnd);
            return vec![ev, end];
        }
        vec![ev]
    }
}

impl StreamParser for ClaudeParser {
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
        // Flush any tail line that arrived without a trailing newline.
        if !self.buffer.is_empty() {
            let raw = std::mem::take(&mut self.buffer);
            let line = strip_ansi(&raw);
            let line = line.trim().to_string();
            if !line.is_empty() {
                events.extend(self.parse_line(&line));
            }
        }
        // If the stream stopped without a `result`, close the run ourselves so it
        // never hangs in `Running`. The supervisor sets the real exit code.
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

/// `mcp__server__tool` → `mcp:server:tool`; other names pass through unchanged.
fn display_tool(name: &str) -> String {
    match name.strip_prefix("mcp__") {
        Some(rest) => format!("mcp:{}", rest.replace("__", ":")),
        None => name.to_string(),
    }
}

/// Stable hash of a tool's input args, for repeated-action detection. Default
/// `serde_json` orders object keys deterministically, so identical inputs hash
/// identically across turns.
fn hash_input(input: &serde_json::Value) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    input.to_string().hash(&mut hasher);
    hasher.finish()
}

/// Strip ANSI escape sequences a PTY may inject around the JSON. Lossy UTF-8 is
/// fine — we only need the text to `serde_json::from_str`.
fn strip_ansi(s: &str) -> String {
    String::from_utf8_lossy(&strip_ansi_escapes::strip(s.as_bytes())).into_owned()
}

// --- the lean subset of CC's stream-json schema we deserialize ----------------

// The `sessionId` aliases below let the SAME structs deserialize both feeds:
// Mode-A stream-json uses snake_case `session_id`; the Mode-B transcript uses
// camelCase `sessionId` (verified, §8 Q8). One parser, two feeds (§6).
#[derive(Deserialize)]
struct SystemLine {
    #[serde(default)]
    subtype: Option<String>,
    #[serde(default, alias = "sessionId")]
    session_id: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

#[derive(Deserialize)]
struct AssistantLine {
    message: Message,
    #[serde(default, alias = "sessionId")]
    session_id: Option<String>,
    #[serde(default)]
    parent_tool_use_id: Option<String>,
    /// Transcript envelope flag marking a sub-agent (sidechain) turn.
    #[serde(default, alias = "isSidechain")]
    is_sidechain: bool,
}

#[derive(Deserialize)]
struct UserLine {
    message: Message,
    #[serde(default, alias = "sessionId")]
    session_id: Option<String>,
    #[serde(default)]
    parent_tool_use_id: Option<String>,
    #[serde(default, alias = "isReplay")]
    is_replay: bool,
    #[serde(default, alias = "isSynthetic")]
    is_synthetic: bool,
}

#[derive(Deserialize)]
struct Message {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    content: Content,
    #[serde(default)]
    usage: Option<Usage>,
}

/// Content is either an array of typed blocks or a bare string.
#[derive(Deserialize)]
#[serde(untagged)]
enum Content {
    Blocks(Vec<Block>),
    Text(String),
}

impl Default for Content {
    fn default() -> Self {
        Content::Blocks(Vec::new())
    }
}

impl Content {
    fn blocks(&self) -> &[Block] {
        match self {
            Content::Blocks(b) => b,
            Content::Text(_) => &[],
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Block {
    Text {
        #[serde(default)]
        text: String,
    },
    Thinking {
        #[serde(default)]
        thinking: String,
    },
    ToolUse {
        id: String,
        name: String,
        #[serde(default)]
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        #[serde(default)]
        content: serde_json::Value,
        #[serde(default)]
        is_error: Option<bool>,
    },
    /// image / redacted_thinking / any future block — ignored, not an error.
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize, Default)]
struct Usage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
}

impl Usage {
    fn to_price_usage(&self) -> PriceUsage {
        PriceUsage {
            input_tokens: self.input_tokens.unwrap_or(0) as u32,
            cache_creation_input_tokens: self.cache_creation_input_tokens.unwrap_or(0) as u32,
            cache_read_input_tokens: self.cache_read_input_tokens.unwrap_or(0) as u32,
            output_tokens: self.output_tokens.unwrap_or(0) as u32,
        }
    }
}

#[derive(Deserialize)]
struct ResultLine {
    #[serde(default)]
    subtype: Option<String>,
    #[serde(default)]
    is_error: Option<bool>,
    #[serde(default)]
    total_cost_usd: Option<f64>,
    #[serde(default)]
    num_turns: Option<u32>,
    #[serde(default, alias = "sessionId")]
    session_id: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    result: Option<serde_json::Value>,
    #[serde(default, alias = "modelUsage")]
    model_usage: Option<HashMap<String, ModelUsage>>,
}

#[derive(Deserialize, Default)]
struct ModelUsage {
    #[serde(default, alias = "contextWindow")]
    context_window: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::events::DEFAULT_CONTEXT_WINDOW;

    fn parse_all(p: &mut ClaudeParser, lines: &[&str]) -> Vec<LoopEvent> {
        let mut out = Vec::new();
        for l in lines {
            out.extend(p.push(&format!("{l}\n")));
        }
        out
    }

    // The verified §9 schema, pinned. Re-verify these against `claude` on upgrade.
    const INIT: &str = r#"{"type":"system","subtype":"init","session_id":"sess_abc","model":"claude-opus-4-8","cwd":"/x","tools":[]}"#;
    const ASSISTANT_TEXT: &str = r#"{"type":"assistant","message":{"role":"assistant","model":"claude-opus-4-8","content":[{"type":"text","text":"Hello"}],"usage":{"input_tokens":100,"output_tokens":10,"cache_read_input_tokens":2000}},"session_id":"sess_abc","parent_tool_use_id":null}"#;
    const ASSISTANT_TOOL: &str = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"tu_1","name":"Bash","input":{"command":"echo hi"}}],"usage":{"input_tokens":50,"output_tokens":5}},"session_id":"sess_abc"}"#;
    const USER_RESULT: &str = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu_1","content":"hi","is_error":false}]},"session_id":"sess_abc"}"#;
    const RESULT: &str = r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":1234,"num_turns":2,"total_cost_usd":0.0123,"session_id":"sess_abc","usage":{"input_tokens":150,"output_tokens":15},"modelUsage":{"claude-opus-4-8":{"contextWindow":1000000}}}"#;

    #[test]
    fn full_run_maps_to_events_and_rollup() {
        let mut p = ClaudeParser::new("run_1");
        let events = parse_all(
            &mut p,
            &[INIT, ASSISTANT_TEXT, ASSISTANT_TOOL, USER_RESULT, RESULT],
        );

        let kinds: Vec<EventKind> = events.iter().map(|e| e.kind).collect();
        assert!(kinds.contains(&EventKind::RunStart));
        assert!(kinds.contains(&EventKind::Assistant));
        assert!(kinds.contains(&EventKind::ToolUse));
        assert!(kinds.contains(&EventKind::ToolResult));
        assert!(kinds.contains(&EventKind::RunEnd));

        // session id learned and exposed for pause/resume.
        assert_eq!(p.session_id(), Some("sess_abc"));

        // tool_use → tool_result paired by id, status Ok.
        let tr = events
            .iter()
            .find(|e| e.kind == EventKind::ToolResult)
            .unwrap();
        assert_eq!(tr.tool.as_deref(), Some("Bash"));
        assert_eq!(tr.tool_status, Some(ToolStatus::Ok));

        let st = p.run_state();
        assert_eq!(st.model.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(st.cost_usd, Some(0.0123)); // authoritative total_cost_usd
        assert_eq!(st.context_window, Some(1_000_000)); // exact, from modelUsage
        assert_eq!(st.iteration, 2); // CC num_turns
        assert!(st.ended && st.exit_ok == Some(true));
        // total_input for the text turn = 100 + 2000 cache read = 2100; tool turn 50.
        assert_eq!(st.tokens_in, 2150);
        assert_eq!(st.tokens_out, 15);
    }

    #[test]
    fn non_json_line_becomes_output_not_panic() {
        let mut p = ClaudeParser::new("run_2");
        let events = p.push("npm warn something noisy\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, EventKind::Output);
        assert_eq!(events[0].text.as_deref(), Some("npm warn something noisy"));
    }

    #[test]
    fn replayed_user_lines_are_dropped() {
        let mut p = ClaudeParser::new("run_3");
        let replay = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"old","content":"x","is_error":false}]},"isReplay":true,"session_id":"s"}"#;
        let events = p.push(&format!("{replay}\n"));
        assert!(events.is_empty(), "replayed lines must not produce events");
    }

    #[test]
    fn chunk_split_across_pushes_parses_once_complete() {
        let mut p = ClaudeParser::new("run_4");
        // Feed the init line in two halves; no event until the newline arrives.
        let mid = INIT.len() / 2;
        assert!(p.push(&INIT[..mid]).is_empty());
        let events = p.push(&format!("{}\n", &INIT[mid..]));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, EventKind::RunStart);
    }

    #[test]
    fn mcp_tool_names_are_displayed_namespaced() {
        assert_eq!(display_tool("mcp__github__create_issue"), "mcp:github:create_issue");
        assert_eq!(display_tool("Bash"), "Bash");
    }

    #[test]
    fn subagent_turns_do_not_count_toward_totals() {
        let mut p = ClaudeParser::new("run_5");
        p.push(&format!("{INIT}\n"));
        let sub = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"sub work"}],"usage":{"input_tokens":999,"output_tokens":999}},"session_id":"sess_abc","parent_tool_use_id":"tu_parent"}"#;
        p.push(&format!("{sub}\n"));
        let st = p.run_state();
        assert_eq!(st.tokens_in, 0, "subagent tokens must not fold into the run total");
        assert_eq!(st.iteration, 0, "subagent turn must not bump iteration");
    }

    // --- Mode B: the SAME parser consumes transcript JSONL lines (§6). These are
    // pinned from a live `~/.claude/projects/**/*.jsonl` capture (CC 2.x,
    // 2026-06-20): camelCase `sessionId`, an `isSidechain` envelope, `message.usage`,
    // and a `tool_use` block with a `caller` field. No `system/init` or `result`
    // line ever appears in a transcript. Re-verify on CC upgrade.
    const TRANSCRIPT_ASSISTANT: &str = r#"{"parentUuid":"p0","isSidechain":false,"message":{"model":"claude-opus-4-8","id":"msg_1","type":"message","role":"assistant","content":[{"type":"tool_use","id":"toolu_aaa","name":"Read","input":{"file_path":"/x"},"caller":"agent"}],"usage":{"input_tokens":8260,"cache_creation_input_tokens":7377,"cache_read_input_tokens":20806,"output_tokens":250}},"requestId":"req_1","type":"assistant","uuid":"u-assist-1","timestamp":"2026-06-20T22:00:00Z","sessionId":"sess_xyz","cwd":"/x","version":"2.1.183","gitBranch":"main"}"#;
    const TRANSCRIPT_USER_RESULT: &str = r#"{"parentUuid":"u-assist-1","isSidechain":false,"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_aaa","type":"tool_result","content":"file contents"}]},"uuid":"u-user-1","timestamp":"2026-06-20T22:00:01Z","sessionId":"sess_xyz","cwd":"/x","gitBranch":"main"}"#;
    const TRANSCRIPT_SIDECHAIN: &str = r#"{"parentUuid":"p9","isSidechain":true,"message":{"model":"claude-opus-4-8","role":"assistant","content":[{"type":"text","text":"sub work"}],"usage":{"input_tokens":5000,"output_tokens":500}},"type":"assistant","uuid":"u-sc-1","timestamp":"2026-06-20T22:00:02Z","sessionId":"sess_xyz"}"#;

    #[test]
    fn transcript_lines_feed_the_same_parser() {
        let mut p = ClaudeParser::new("obs_1");
        let events = parse_all(&mut p, &[TRANSCRIPT_ASSISTANT, TRANSCRIPT_USER_RESULT]);

        // session id is learned from camelCase `sessionId` (Mode-B feed).
        assert_eq!(p.session_id(), Some("sess_xyz"));

        // The tool_use → tool_result pair is recognized across the two lines.
        let tu = events.iter().find(|e| e.kind == EventKind::ToolUse).unwrap();
        assert_eq!(tu.tool.as_deref(), Some("Read"));
        let tr = events.iter().find(|e| e.kind == EventKind::ToolResult).unwrap();
        assert_eq!(tr.tool.as_deref(), Some("Read"));

        let st = p.run_state();
        assert_eq!(st.model.as_deref(), Some("claude-opus-4-8"));
        // Window is seeded (non-None) from the model id. Transcripts carry no
        // `result.modelUsage` line, so it stays the conservative default rather
        // than the model's true window — a documented Mode-B limitation (the
        // ctx% can read high for a 1M-window model observed read-only).
        assert_eq!(st.context_window, Some(DEFAULT_CONTEXT_WINDOW));
        assert_eq!(st.iteration, 1); // one main assistant turn
        // total_input = 8260 + 7377 cache-create + 20806 cache-read = 36443.
        assert_eq!(st.tokens_in, 36_443);
        assert_eq!(st.tokens_out, 250);
        // No `result` line → cost is computed from usage (transcript has no cost).
        assert!(st.cost_usd.is_some());
    }

    #[test]
    fn transcript_sidechain_turns_are_excluded_from_rollup() {
        let mut p = ClaudeParser::new("obs_2");
        parse_all(&mut p, &[TRANSCRIPT_ASSISTANT, TRANSCRIPT_SIDECHAIN]);
        let st = p.run_state();
        // Only the main turn counts; the isSidechain turn adds nothing.
        assert_eq!(st.iteration, 1, "isSidechain turn must not bump iteration");
        assert_eq!(st.tokens_in, 36_443, "isSidechain tokens must not fold in");
    }

    #[test]
    fn finish_synthesizes_run_end_when_stream_cut_off() {
        let mut p = ClaudeParser::new("run_6");
        p.push(&format!("{INIT}\n"));
        p.push(&format!("{ASSISTANT_TEXT}\n"));
        // No `result` line — process died. finish() must close the run.
        let tail = p.finish();
        assert!(tail.iter().any(|e| e.kind == EventKind::RunEnd));
        assert!(p.run_state().ended);
    }
}
