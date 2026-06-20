//! `loop kill <id>` — stop a run (the worst action loopd ever takes).
//!
//! Thin client: `POST /runs/:id/kill`. The daemon flags the run and, for owned
//! runs, tears down the process tree. Observed runs only get the flag.

use anyhow::Result;
use clap::Args;

use crate::config::Config;
use crate::daemon::client::DaemonClient;

/// Arguments for `loop kill`.
#[derive(Args, Debug)]
pub struct KillArgs {
    /// The run id to stop (from `loop ps`).
    pub id: String,
}

pub fn kill(args: KillArgs) -> Result<()> {
    let config = Config::load()?;
    let client = DaemonClient::from_config(&config);
    client.ensure_running(&config)?;

    client.request_kill(&args.id)?;
    println!("kill requested for run {}", args.id);
    Ok(())
}
