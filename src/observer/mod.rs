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
//! Planned contents (Phase 7):
//! - `webhook`    — normalize a Claude Code hook payload → `LoopEvent`.
//! - `transcript` — `notify`-watch `~/.claude/projects/**/*.jsonl`, feed the
//!   same `StreamParser` as Mode A.
