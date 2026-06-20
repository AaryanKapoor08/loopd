//! CLI — the command surface, all thin clients over the daemon.
//!
//! Each subcommand declared in `main.rs` (`init`, `run`, `ps`, `dash`, `kill`,
//! `logs`, `set`, `policy`, `daemon`, `hooks`) gets a handler here that simply
//! translates arguments into a daemon API call and renders the response. No
//! business logic lives in the CLI.
//!
//! Contents:
//! - `daemon` — `loop daemon {start,stop,status,serve}` (Phase 2).
//! - `run`/`ps`/`kill`/`logs` — the owned-run command surface (Phase 4).
//! - `dash`/`hooks` — stubs forwarding to Phase 5 / Phase 7.
//! - `init`/`set`/`policy` — config bootstrap + on-disk config edits (Phase 4).

pub mod daemon;
pub mod dash;
pub mod fmt;
pub mod hooks;
pub mod ingest;
pub mod init;
pub mod kill;
pub mod logs;
pub mod policy;
pub mod ps;
pub mod run;
pub mod set;
