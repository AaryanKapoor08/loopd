//! Supervisor — Mode A (owner).
//!
//! Spawns agent processes through a PTY (`portable-pty`; ConPTY on Windows),
//! feeds their streaming output into the agent's `StreamParser`, persists the
//! resulting `LoopEvent`s, and can stop them. This is the only part of loopd
//! that controls a process — and stopping a process is the worst action loopd
//! ever takes (it never edits the user's codebase).
//!
//! Pause is *not* a process suspend: pause = capture the agent's
//! `agent_session_id` + stop; resume = re-spawn via the agent's native
//! `--resume`/`resume` (resolved in ARCHITECTURE §8).
//!
//! Planned contents (Phase 3): `mod` — spawn/own/stream/kill + pause/resume.
