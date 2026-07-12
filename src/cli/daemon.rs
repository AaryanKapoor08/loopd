//! `loop daemon {start,stop,status,serve}` — thin handlers over `daemon::*`.
//!
//! These hold no business logic: `start`/`stop`/`status` drive
//! [`crate::daemon::lifecycle`] and the [`crate::daemon::client::DaemonClient`];
//! `serve` is the hidden entry point the detached child runs to actually become
//! the server. The CLI never opens the store or a process directly.

use std::time::Duration;

use anyhow::{Context, Result};

use crate::config::Config;
use crate::core::store::Store;
use crate::daemon::client::DaemonClient;
use crate::daemon::lifecycle::{self, StartOutcome, Status, StopOutcome};
use crate::daemon::server::{self, AppState};

/// `loop daemon start` — spawn the daemon detached (if needed) and wait until
/// `/health` is green.
pub fn start() -> Result<()> {
    let config = Config::load()?;
    let port = config.daemon.port;
    let outcome = lifecycle::start(&config)?;
    let client = DaemonClient::from_config(&config);
    client
        .wait_healthy(Duration::from_secs(10))
        .context("daemon was started but never became healthy (see ~/.loopd/daemon.log)")?;
    match outcome {
        StartOutcome::AlreadyRunning { pid } => {
            println!("daemon already running (pid {pid}) at http://127.0.0.1:{port}");
        }
        StartOutcome::Started { pid } => {
            println!("daemon started (pid {pid}) at http://127.0.0.1:{port}");
        }
    }
    Ok(())
}

/// `loop daemon stop` — stop the daemon and remove the pidfile. Idempotent.
pub fn stop() -> Result<()> {
    match lifecycle::stop()? {
        StopOutcome::Stopped { pid } => println!("daemon stopped (pid {pid})"),
        StopOutcome::NotRunning => println!("daemon not running"),
    }
    Ok(())
}

/// `loop daemon status` — report pidfile/liveness, and confirm `/health`.
pub fn status() -> Result<()> {
    let config = Config::load()?;
    let port = config.daemon.port;
    match lifecycle::status() {
        Status::Running { pid } => {
            let healthy = DaemonClient::from_config(&config).health();
            let health = if healthy { "healthy" } else { "not responding" };
            println!("daemon running (pid {pid}) at http://127.0.0.1:{port} — {health}");
        }
        Status::Stopped => println!("daemon not running"),
        Status::Stale { pid } => {
            println!(
                "daemon not running (stale pidfile for pid {pid}; `loop daemon stop` clears it)"
            );
        }
    }
    Ok(())
}

/// `loop daemon serve` (hidden) — become the server. The detached child runs
/// this; it builds the runtime, the `Store`, and `AppState`, then blocks in
/// [`server::serve`] until a shutdown signal.
pub fn serve() -> Result<()> {
    // Logs flow to stdout, which the detached parent redirected to
    // ~/.loopd/daemon.log. `try_init` so a double-init never panics.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let config = Config::load()?;
    let store = Store::open_default()?;
    let state = AppState::new(store, config);

    let rt = tokio::runtime::Runtime::new().context("building tokio runtime")?;
    rt.block_on(server::serve(state))
}
