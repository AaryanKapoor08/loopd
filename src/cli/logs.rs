//! `loop logs <id> [--follow]` — print a run's event log.
//!
//! Thin client: `GET /runs/:id/events` (newest-first from the store; we reverse
//! to chronological for display). `--follow` polls until the run reaches a
//! terminal state, printing only newly-arrived events each tick.

use std::time::Duration;

use anyhow::{bail, Result};
use clap::Args;

use crate::cli::fmt::{chronological, fmt_event, is_live};
use crate::config::Config;
use crate::daemon::client::DaemonClient;

/// Arguments for `loop logs`.
#[derive(Args, Debug)]
pub struct LogsArgs {
    /// The run id to show (from `loop ps`).
    pub id: String,

    /// Keep streaming new events until the run finishes.
    #[arg(long, short)]
    pub follow: bool,

    /// How many recent events to show (most-recent window).
    #[arg(long, default_value_t = 200)]
    pub limit: u32,
}

pub fn logs(args: LogsArgs) -> Result<()> {
    let config = Config::load()?;
    let client = DaemonClient::from_config(&config);
    client.ensure_running(&config)?;

    // Confirm the run exists up front so a typo'd id is a clear error.
    if client.get_run(&args.id)?.is_none() {
        bail!("no such run: {}", args.id);
    }

    if !args.follow {
        let events = chronological(client.events_for_run(&args.id, args.limit)?);
        for ev in &events {
            println!("{}", fmt_event(ev));
        }
        return Ok(());
    }

    // Follow mode: poll a generous window, print the tail beyond what we've shown,
    // and stop once the run is no longer live. Count-based dedup is reliable while
    // total events stay within the window (fine for tailing a live run).
    let window = args.limit.max(500);
    let mut printed = 0usize;
    loop {
        let events = chronological(client.events_for_run(&args.id, window)?);
        if events.len() > printed {
            for ev in &events[printed..] {
                println!("{}", fmt_event(ev));
            }
            printed = events.len();
        }

        match client.get_run(&args.id)? {
            Some(run) if is_live(run.status) => {}
            _ => break, // gone or terminal — final events are already printed.
        }
        std::thread::sleep(Duration::from_millis(700));
    }
    Ok(())
}
