//! Dashboard — the TUI cockpit (`loop dash`).
//!
//! A `ratatui` + `crossterm` terminal UI and a thin client over the daemon: it
//! polls `GET /runs` (~1s) for the list view and `GET /runs/:id/events` for the
//! detail view. It holds no business logic — every action (kill, pause, retry)
//! is a daemon API call. Observed (un-owned) runs render `(ro)` with kill
//! disabled.
//!
//! Planned contents (Phase 5): `mod` — list view, detail view, keybindings.
