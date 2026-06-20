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

// Module skeleton — each is documented in its own `mod.rs`/file and filled in
// during the phase noted there. `#[allow(dead_code)]` keeps the scaffold quiet
// until the modules actually expose items (removed as each phase lands).
#[allow(dead_code)]
mod agents;
#[allow(dead_code)]
mod cli;
#[allow(dead_code)]
mod config;
#[allow(dead_code)]
mod core;
#[allow(dead_code)]
mod daemon;
mod dashboard;
#[allow(dead_code)]
mod observer;
#[allow(dead_code)]
mod supervisor;

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
    /// Spawn an agent loop under loopd's supervision. (Phase 4)
    Run(cli::run::RunArgs),
    /// List runs and their live status. (Phase 4)
    Ps,
    /// Open the live TUI cockpit. (Phase 5)
    Dash,
    /// Stop a run (worst action loopd ever takes). (Phase 4)
    Kill(cli::kill::KillArgs),
    /// Print a run's event log. (Phase 4)
    Logs(cli::logs::LogsArgs),
    /// Set a config value in ~/.loopd/config.yaml. (Phase 4)
    Set(cli::set::SetArgs),
    /// Show or edit the governance policy. (Phase 4)
    Policy(cli::policy::PolicyArgs),
    /// Manage the background daemon. (Phase 2)
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// Manage Claude Code hooks for Mode-B observation. (Phase 7)
    Hooks {
        #[command(subcommand)]
        action: cli::hooks::HooksAction,
    },
}

/// `loop daemon <action>`.
#[derive(Subcommand)]
enum DaemonAction {
    /// Start the daemon (detached) and wait until it is healthy.
    Start,
    /// Stop the daemon and remove its pidfile (idempotent).
    Stop,
    /// Show whether the daemon is running and healthy.
    Status,
    /// Internal: run the HTTP server in the foreground. The detached child runs
    /// this; users call `start`/`stop`/`status`.
    #[command(hide = true)]
    Serve,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        // With no subcommand, behave like `--help` so a bare `loopd` is useful.
        None => {
            Cli::command().print_help()?;
            println!();
        }
        Some(Command::Daemon { action }) => match action {
            DaemonAction::Start => cli::daemon::start()?,
            DaemonAction::Stop => cli::daemon::stop()?,
            DaemonAction::Status => cli::daemon::status()?,
            DaemonAction::Serve => cli::daemon::serve()?,
        },
        Some(Command::Run(args)) => cli::run::run(args)?,
        Some(Command::Ps) => cli::ps::ps()?,
        Some(Command::Kill(args)) => cli::kill::kill(args)?,
        Some(Command::Logs(args)) => cli::logs::logs(args)?,
        Some(Command::Dash) => cli::dash::dash()?,
        Some(Command::Hooks { action }) => cli::hooks::hooks(action)?,
        Some(Command::Init) => cli::init::init()?,
        Some(Command::Set(args)) => cli::set::set(args)?,
        Some(Command::Policy(args)) => cli::policy::policy(args)?,
    }

    Ok(())
}
