//! `loop run "<task>"` — start an owned agent loop under loopd.
//!
//! A thin client: it preflights the agent binary (so a missing `claude` fails
//! with a clear message, not a buried spawn error), ensures the daemon is up,
//! then `POST /runs`. The daemon does the actual spawning/owning — the CLI holds
//! no supervision logic.

use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use clap::Args;

use crate::agents::adapter_for;
use crate::config::Config;
use crate::daemon::client::{DaemonClient, NewRun};

/// Arguments for `loop run`.
#[derive(Args, Debug)]
pub struct RunArgs {
    /// The task text the agent should work on.
    pub task: String,

    /// Which agent to run (defaults to `defaults.agent` from config).
    #[arg(long)]
    pub agent: Option<String>,

    /// Working directory to run in (the agent edits here; defaults to the
    /// current directory). loopd itself never writes to it.
    #[arg(long)]
    pub cwd: Option<PathBuf>,

    /// Human-readable label for the run (defaults to the run id).
    #[arg(long)]
    pub label: Option<String>,

    /// Override the model the agent uses.
    #[arg(long)]
    pub model: Option<String>,

    /// Cap: max cumulative cost in USD before the on-trip action fires.
    #[arg(long)]
    pub max_cost: Option<f64>,

    /// Cap: max agent iterations before the on-trip action fires.
    #[arg(long)]
    pub max_iterations: Option<u32>,

    /// Cap: max wall-clock minutes before the on-trip action fires.
    #[arg(long)]
    pub max_duration: Option<u32>,

    /// What to do when a cap/detector trips (`warn`/`notify`/`pause`/`kill`).
    #[arg(long)]
    pub on_trip: Option<String>,
}

pub fn run(args: RunArgs) -> Result<()> {
    let config = Config::load()?;

    // Resolve the agent (CLI flag wins, else the configured default).
    let agent = args
        .agent
        .clone()
        .unwrap_or_else(|| config.defaults.agent.clone());

    // Preflight: resolve the adapter and confirm its binary is installed *before*
    // we ask the daemon to spawn — turns a missing `claude` into one clear line.
    let adapter = adapter_for(&agent).ok_or_else(|| {
        anyhow!(
            "unknown agent `{agent}` — known agents this build can run: {}",
            crate::agents::KNOWN_AGENTS.join(", ")
        )
    })?;
    adapter.preflight().map_err(|e| anyhow!("{e}"))?;

    // Default the working directory to the process CWD; make it absolute so the
    // daemon (a different process) resolves the same path.
    let cwd = match args.cwd {
        Some(p) => p,
        None => std::env::current_dir().context("resolving current directory")?,
    };
    let cwd = cwd.to_string_lossy().to_string();

    // Validate --on-trip up front so a typo is a clean message, not a 400 body.
    if let Some(t) = args.on_trip.as_deref() {
        if !matches!(t, "warn" | "notify" | "pause" | "kill") {
            return Err(anyhow!(
                "invalid --on-trip `{t}` — expected one of: warn, notify, pause, kill"
            ));
        }
    }

    let client = DaemonClient::from_config(&config);
    client.ensure_running(&config)?;

    let run = client.create_run(&NewRun {
        prompt: &args.task,
        agent: Some(&agent),
        cwd: Some(&cwd),
        label: args.label.as_deref(),
        model: args.model.as_deref(),
        max_iterations: args.max_iterations,
        max_cost_usd: args.max_cost,
        max_duration_min: args.max_duration,
        on_trip: args.on_trip.as_deref(),
        ..Default::default()
    })?;

    println!("started run {} ({})", run.run_id, run.agent);
    println!("  watch it:  loop dash      tail logs:  loop logs {} --follow", run.run_id);

    // Per-run caps are now enforced by the governance engine (Phase 6). Echo the
    // active overrides so the user can see what loopd will hold this run to.
    if args.max_cost.is_some()
        || args.max_iterations.is_some()
        || args.max_duration.is_some()
        || args.on_trip.is_some()
    {
        let mut parts = Vec::new();
        if let Some(v) = args.max_iterations {
            parts.push(format!("iter ≤ {v}"));
        }
        if let Some(v) = args.max_cost {
            parts.push(format!("cost ≤ ${v:.2}"));
        }
        if let Some(v) = args.max_duration {
            parts.push(format!("dur ≤ {v}m"));
        }
        let on_trip = args
            .on_trip
            .clone()
            .unwrap_or_else(|| config.defaults.on_trip.word().to_string());
        println!("  caps: {} (on-trip: {on_trip})", parts.join(", "));
    }

    Ok(())
}
