//! CLI — the command surface, all thin clients over the daemon.
//!
//! Each subcommand declared in `main.rs` (`init`, `run`, `ps`, `dash`, `kill`,
//! `logs`, `set`, `policy`, `daemon`, `hooks`) gets a handler here that simply
//! translates arguments into a daemon API call and renders the response. No
//! business logic lives in the CLI.
//!
//! Contents:
//! - `daemon` — `loop daemon {start,stop,status,serve}` (Phase 2).
//!
//! Planned (Phase 4): one module/function per remaining command.

pub mod daemon;
