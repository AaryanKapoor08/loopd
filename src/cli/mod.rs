//! CLI — the command surface, all thin clients over the daemon.
//!
//! Each subcommand declared in `main.rs` (`init`, `run`, `ps`, `dash`, `kill`,
//! `logs`, `set`, `policy`, `daemon`, `hooks`) gets a handler here that simply
//! translates arguments into a daemon API call and renders the response. No
//! business logic lives in the CLI.
//!
//! Planned contents (Phase 4): one module/function per command.
