//! `loop ingest` — the hook target. Claude Code runs this command for each
//! installed hook (PostToolUse/Stop/SessionStart), piping the hook's JSON payload
//! on stdin; we forward it to the daemon's `POST /ingest`.
//!
//! This runs **inside the user's `claude` session**, so it must be fast and
//! never disruptive: if the daemon is down it silently no-ops (it does *not*
//! auto-start the daemon — that would stall every tool call), and any error is
//! swallowed. It always exits 0 so a hook can never break the user's session.
//! loopd only ever observes here — it does not touch the session or the codebase.

use std::io::Read;

use anyhow::Result;

use crate::config::Config;
use crate::daemon::client::DaemonClient;

pub fn ingest() -> Result<()> {
    let mut payload = String::new();
    // Best-effort read; an empty/garbled payload is just a no-op.
    if std::io::stdin().read_to_string(&mut payload).is_err() || payload.trim().is_empty() {
        return Ok(());
    }
    let Ok(config) = Config::load() else {
        return Ok(());
    };
    let client = DaemonClient::from_config(&config);
    // Daemon down → stay silent (don't auto-start on every tool call).
    if !client.health() {
        return Ok(());
    }
    // Fire-and-forget: the verdict matters to the SDK (Phase 9), not to a CC hook.
    let _ = client.ingest(&payload);
    Ok(())
}
