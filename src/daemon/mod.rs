//! Daemon — the background brain.
//!
//! A long-lived process that owns all state (the `Store`) and all supervised
//! agent processes, and hosts a local HTTP API on `127.0.0.1:7777`. The CLI and
//! TUI are thin clients: they never touch the database or processes directly,
//! they call this API. Keeping a single owner means one writer and one place
//! the governance detector runs.
//!
//! Planned contents (Phase 2):
//! - `server`    — `axum` routes (`/health`, `/runs`, `/runs/:id`, `/ingest`, ...).
//! - `lifecycle` — start detached, write `~/.loopd/daemon.pid` + log; stop; status.
//! - `client`    — `DaemonClient` used by every CLI command; auto-starts the daemon.
