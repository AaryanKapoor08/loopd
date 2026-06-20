//! Agents — the adapters that turn each vendor's stream into `LoopEvent`s.
//!
//! This is where cross-vendor unification actually happens. An `Adapter` is a
//! factory (`build_args`/`resume_args`/`new_parser`); each run gets its own
//! **stateful** `StreamParser` that buffers partial chunks, counts iterations,
//! extracts the `agent_session_id` once, and pairs `tool_use`→`tool_result`.
//! A single `parse(line)` is not enough — parsing carries state across lines.
//!
//! Planned contents:
//! - `mod`    — the `Adapter` + `StreamParser` traits (Phase 3).
//! - `claude` — Claude Code adapter; maps the verified stream-json schema (Phase 3).
//! - `codex`  — Codex adapter; proves cross-vendor in one cockpit (Phase 8).
