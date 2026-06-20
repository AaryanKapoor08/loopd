//! loopd — a vendor- and framework-neutral control plane for AI agent loops.
//!
//! This single binary will grow into the whole engine: a background daemon that
//! owns agent processes, stream adapters that normalize every surface into one
//! `LoopEvent`, a governance detector, and a `ratatui` cockpit. See
//! `claude/BuildFlow.md` for the phase-by-phase plan.
//!
//! Phase 0 (this commit) is just the CLI entry point: parse arguments with
//! `clap` and print help. Subcommands are declared so `--help` documents where
//! the tool is going, but their handlers are stubs until their phase lands.

use clap::{CommandFactory, Parser, Subcommand};

/// Top-level CLI: `loopd <command>`.
///
/// CLI and TUI are deliberately *thin* — every command will call the daemon's
/// local HTTP API rather than holding business logic. That logic lives in
/// `daemon`/`core`/`supervisor`/`observer` (added in later phases).
#[derive(Parser)]
#[command(
    name = "loopd",
    version,
    about = "Control plane for AI agent loops — see, unify, and govern every loop you run.",
    long_about = None,
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

/// The command surface loopd is building toward. Handlers are stubbed in
/// Phase 0; each is wired up in the phase noted beside it.
#[derive(Subcommand)]
enum Command {
    /// Create config and ensure the daemon is running. (Phase 4)
    Init,
    /// Spawn an agent loop under loopd's supervision. (Phase 3/4)
    Run,
    /// List runs and their live status. (Phase 4)
    Ps,
    /// Open the live TUI cockpit. (Phase 5)
    Dash,
    /// Stop a run (worst action loopd ever takes). (Phase 4)
    Kill,
    /// Print a run's event log. (Phase 4)
    Logs,
    /// Manage the background daemon (start/stop/status). (Phase 2)
    Daemon,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        // With no subcommand, behave like `--help` so a bare `loopd` is useful.
        None => {
            Cli::command().print_help()?;
            println!();
        }
        Some(_) => {
            println!("loopd: command not yet implemented (Phase 0 scaffold).");
            println!("See claude/BuildFlow.md for the build sequence.");
        }
    }

    Ok(())
}
