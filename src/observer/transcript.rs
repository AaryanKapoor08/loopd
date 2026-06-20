//! Transcript tailer (Mode B) — Phase 7 Part C.
//!
//! Watches `~/.claude/projects/**/*.jsonl` and feeds appended lines to the same
//! [`crate::agents::claude`] `StreamParser` as Mode A — the canonical source of
//! token/iteration rollup for observed runs. Implemented in the next part.
