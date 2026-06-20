//! Agents — the adapters that turn each vendor's stream into `LoopEvent`s.
//!
//! This is where cross-vendor unification happens. An [`Adapter`] is a **factory**
//! (`build_args`/`resume_args`/`new_parser`); each run gets its own **stateful**
//! [`StreamParser`] that buffers partial chunks, counts iterations, extracts the
//! `agent_session_id` once, and pairs `tool_use`→`tool_result`. A single
//! `parse(line)` is not enough — parsing carries state across lines (§6).
//!
//! Contents:
//! - this module — the `Adapter` + `StreamParser` traits + the small registry.
//! - [`claude`] — Claude Code adapter; maps the verified stream-json schema (§9).
//! - `codex` — Codex adapter; proves cross-vendor in one cockpit (Phase 8).
//!
//! **Design notes / deviations from `ARCHITECTURE.md §6` (deliberate):**
//! - The traits are **sync**, not `async`. Every `Adapter` method is a pure
//!   factory call and every `StreamParser` method is synchronous string work;
//!   the only async/blocking part is the PTY I/O, which is the *supervisor's*
//!   concern (it runs reads on a blocking task). So we don't pull in `async-trait`.
//! - `new_parser` takes the `run_id` — the parser stamps it onto every
//!   `LoopEvent` it emits (there is no other channel for it).
//! - Added [`StreamParser::run_state`]: the parser is the single place that knows
//!   the stream's rollup (model, session id, token totals, cost, context, exit),
//!   and the supervisor needs those to maintain the `Run` row. `LoopEvent` has no
//!   `model`/context fields, so this accessor is how that metadata reaches `Run`.
//! - Added [`Adapter::env`]: env vars to inject on spawn (e.g.
//!   `NPM_CONFIG_LOGLEVEL=error`). `availability()` (binary preflight) is a
//!   Phase-4 concern and intentionally not here yet.

pub mod claude;

use std::path::{Path, PathBuf};

use crate::core::events::LoopEvent;

/// Options that shape how an agent is invoked. Minimal for Phase 3; the Phase-4
/// CLI extends this (allowed tools, permission mode, caps, …).
#[derive(Debug, Clone, Default)]
pub struct RunOpts {
    /// Override the model the agent runs (maps to the agent's `--model` flag).
    pub model: Option<String>,
}

/// The stream's rolled-up run state, as the parser has learned it so far. The
/// supervisor merges this into the `Run` row after each `push`/`finish`. Fields
/// are `Option`/cumulative so "not learned yet" is distinct from a real zero.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunState {
    /// The agent's own session id (CC `session_id` / Codex `thread_id`) — enables
    /// native resume (= pause) and Mode-B correlation. Learned mid-stream.
    pub session_id: Option<String>,
    /// The model the agent reported using.
    pub model: Option<String>,
    /// Iterations (agent turns) seen so far.
    pub iteration: u32,
    /// Cumulative effective input tokens (fresh + cache; see `pricing::Usage`).
    pub tokens_in: u32,
    /// Cumulative output tokens.
    pub tokens_out: u32,
    /// Resolved cost in USD — agent-reported (CC `total_cost_usd`) or computed.
    /// `None` until a cost is known.
    pub cost_usd: Option<f64>,
    /// Tokens occupying the context window right now (running estimate).
    pub context_tokens: u32,
    /// The model's context window. `None` until learned; the authoritative value
    /// arrives at `RunEnd` (CC `result.modelUsage[model].contextWindow`).
    pub context_window: Option<u32>,
    /// Whether the stream has reported the run ending.
    pub ended: bool,
    /// On end: `Some(true)` clean, `Some(false)` errored. `None` until ended.
    pub exit_ok: Option<bool>,
}

/// A vendor adapter: a **factory** for per-run parsers plus the knowledge of how
/// to invoke (and resume) that vendor's CLI. Adding an agent = one `impl Adapter`
/// + its [`StreamParser`]. v1: `claude`; Codex lands in Phase 8.
pub trait Adapter: Send + Sync {
    /// Stable id used in config / `--agent` (`"claude"`, `"codex"`).
    fn id(&self) -> &str;

    /// The executable to spawn (`"claude"`, `"codex"`). The supervisor resolves
    /// it on `PATH` (and the Windows `.cmd` shim) and prepends it to the args.
    fn program(&self) -> &str;

    /// Args for a fresh headless run of `task` (after the program name).
    fn build_args(&self, task: &str, opts: &RunOpts) -> Vec<String>;

    /// Args to resume the session `session_id` and continue with `task`
    /// (pause→resume; ARCHITECTURE §4). Headless resume still needs a prompt.
    fn resume_args(&self, session_id: &str, task: &str, opts: &RunOpts) -> Vec<String>;

    /// Build a fresh stateful parser for one run; events it emits are stamped
    /// with `run_id`.
    fn new_parser(&self, run_id: &str) -> Box<dyn StreamParser>;

    /// Does this agent self-report cost (so we trust it over `pricing.rs`)?
    /// CC reports `total_cost_usd` → `true`; Codex is token-only → `false`.
    /// Reserved capability (events.rs TODO); the parser already applies it.
    fn reports_cost(&self) -> bool {
        false
    }

    /// Env vars to inject when spawning (e.g. `NPM_CONFIG_LOGLEVEL=error`).
    fn env(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    /// Probe whether this agent's executable is installed. The default resolves
    /// [`Adapter::program`] on `PATH` (honoring Windows `PATHEXT`, so `claude`
    /// matches `claude.cmd`); adapters may override to add a version string.
    /// Feeds the `loop run` preflight and `loop init` diagnostics (Phase 4).
    fn availability(&self) -> Availability {
        Availability {
            program: self.program().to_string(),
            path: find_on_path(self.program()),
        }
    }

    /// Resolve the agent binary on `PATH` or fail with [`ExecutorError::ExecutableNotFound`].
    /// `loop run` calls this before asking the daemon to spawn, so a missing
    /// binary surfaces as a clear message instead of a buried spawn error.
    fn preflight(&self) -> std::result::Result<PathBuf, ExecutorError> {
        self.availability().path.ok_or_else(|| ExecutorError::ExecutableNotFound {
            program: self.program().to_string(),
        })
    }

    /// Mode-B (Phase 7): does this transcript file belong to this agent?
    fn match_transcript(&self, _path: &Path) -> bool {
        false
    }

    /// Mode-B (Phase 7): pull the `agent_session_id` out of a hook payload.
    fn match_hook(&self, _payload: &serde_json::Value) -> Option<String> {
        None
    }
}

/// Whether an agent's executable is installed, and where. Returned by
/// [`Adapter::availability`].
#[derive(Debug, Clone)]
pub struct Availability {
    /// The program name we looked for (`claude`, `codex`).
    pub program: String,
    /// Where it resolved on `PATH`, or `None` if not found.
    pub path: Option<PathBuf>,
}

impl Availability {
    /// Is the binary installed?
    pub fn available(&self) -> bool {
        self.path.is_some()
    }
}

/// A small, explicit error taxonomy for invoking agents — modeled on
/// vibe-kanban's `ExecutorError`. v1 carries the two cases the CLI preflight and
/// `loop init` need; Phase 8 (Codex) reuses `AuthRequired` for `401`s.
#[derive(Debug)]
pub enum ExecutorError {
    /// The agent's binary was not found on `PATH`.
    ExecutableNotFound { program: String },
    /// The agent needs an interactive login first (e.g. `claude` / `codex auth`).
    AuthRequired { agent: String },
}

impl std::fmt::Display for ExecutorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExecutorError::ExecutableNotFound { program } => write!(
                f,
                "agent executable `{program}` was not found on your PATH — install it (or check PATH) and try again"
            ),
            ExecutorError::AuthRequired { agent } => {
                write!(f, "agent `{agent}` needs you to log in first")
            }
        }
    }
}

impl std::error::Error for ExecutorError {}

/// Resolve `program` to a concrete file on `PATH`. On Windows this also tries the
/// `PATHEXT` extensions (so `claude` matches the `claude.cmd` npm shim, which is
/// how Claude Code installs — there is no native `claude.exe`). Returns the first
/// match, or `None`. A read-only filesystem probe — loopd never executes here.
pub fn find_on_path(program: &str) -> Option<PathBuf> {
    // An explicit path (absolute or containing a separator) is used as-is.
    let p = Path::new(program);
    if p.components().count() > 1 || p.is_absolute() {
        return p.is_file().then(|| p.to_path_buf());
    }

    let path_var = std::env::var_os("PATH")?;
    #[cfg(windows)]
    let exts: Vec<String> = std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
        .split(';')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();

    for dir in std::env::split_paths(&path_var) {
        let direct = dir.join(program);
        if direct.is_file() {
            return Some(direct);
        }
        #[cfg(windows)]
        for ext in &exts {
            let candidate = dir.join(format!("{program}{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// The stateful half of an adapter: one per run. Fed raw output **chunks** (not
/// lines) from the PTY; buffers partial lines internally, parses complete ones,
/// and rolls up [`RunState`]. The same parser consumes Mode-A PTY output and
/// Mode-B transcript lines (one parser, two feeds — Phase 7).
pub trait StreamParser: Send {
    /// Feed a raw output chunk; returns any `LoopEvent`s completed by it.
    fn push(&mut self, chunk: &str) -> Vec<LoopEvent>;

    /// Flush the buffered tail and synthesize a terminal event if the stream
    /// ended without one. Call exactly once, after the process exits.
    fn finish(&mut self) -> Vec<LoopEvent>;

    /// The agent's session id, once discovered mid-stream.
    fn session_id(&self) -> Option<&str>;

    /// The current rolled-up run state (model, totals, cost, context, exit).
    fn run_state(&self) -> RunState;
}

/// Resolve an adapter by id. The full config-driven registry is Phase 8; for now
/// only `claude` is wired.
pub fn adapter_for(id: &str) -> Option<Box<dyn Adapter>> {
    match id {
        "claude" => Some(Box::new(claude::ClaudeAdapter::new())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_on_path_resolves_a_known_system_binary() {
        // Every platform ships the shell loopd already shells out to.
        let program = if cfg!(windows) { "cmd" } else { "sh" };
        assert!(
            find_on_path(program).is_some(),
            "`{program}` must resolve on PATH"
        );
    }

    #[test]
    fn missing_binary_does_not_resolve() {
        assert!(find_on_path("loopd-definitely-not-a-real-binary-xyz").is_none());
    }

    #[test]
    fn preflight_errors_clearly_for_a_missing_agent() {
        struct GhostAdapter;
        impl Adapter for GhostAdapter {
            fn id(&self) -> &str {
                "ghost"
            }
            fn program(&self) -> &str {
                "loopd-definitely-not-a-real-binary-xyz"
            }
            fn build_args(&self, _t: &str, _o: &RunOpts) -> Vec<String> {
                Vec::new()
            }
            fn resume_args(&self, _s: &str, _t: &str, _o: &RunOpts) -> Vec<String> {
                Vec::new()
            }
            fn new_parser(&self, run_id: &str) -> Box<dyn StreamParser> {
                claude::ClaudeAdapter::new().new_parser(run_id)
            }
        }
        let err = GhostAdapter.preflight().expect_err("missing binary must error");
        assert!(matches!(err, ExecutorError::ExecutableNotFound { .. }));
        // The message names the program and is actionable (no stack trace).
        assert!(err.to_string().contains("not found on your PATH"));
    }
}
