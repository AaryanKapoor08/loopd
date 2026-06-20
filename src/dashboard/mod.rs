//! Dashboard — the TUI cockpit (`loop dash`).
//!
//! A `ratatui` + `crossterm` terminal UI and a thin client over the daemon: it
//! polls `GET /runs` (~1s) for the list view and `GET /runs/:id/events` for the
//! detail view. It holds no business logic — every action (kill, pause, retry)
//! is a `DaemonClient` call. Observed (un-owned) runs render `(ro)` with the
//! process-affecting keys disabled.
//!
//! Layout of this module:
//! - `mod.rs` (here) — the entry point, the panic-safe terminal guard, and the
//!   draw → poll-input → tick event loop.
//! - `app.rs` — the [`App`] state and all input/action handling.
//! - `ui.rs` — pure rendering of the list and detail views from `&App`.

mod app;
mod ui;

use std::io::{self, Stdout};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use crate::config::Config;
use crate::daemon::client::DaemonClient;

use app::App;

/// How often the list/detail polls the daemon.
const TICK_RATE: Duration = Duration::from_millis(1000);

/// Entry point for `loop dash`: ensure the daemon is up, enter the TUI, and run
/// the event loop until the user quits. The terminal is always restored — even
/// on a panic — by [`TerminalGuard`]'s `Drop`.
pub fn run() -> Result<()> {
    let config = Config::load()?;
    let client = DaemonClient::from_config(&config);
    client
        .ensure_running(&config)
        .context("starting the daemon for the dashboard")?;

    let mut guard = TerminalGuard::enter().context("entering the TUI")?;
    let result = run_loop(&mut guard.terminal, &client, &config);
    drop(guard); // restore the terminal before printing any error
    result
}

/// The draw → poll-input → tick loop. Input is polled with a timeout that shrinks
/// toward the next tick, so the UI both stays responsive to keypresses and
/// refreshes from the daemon ~once a second.
fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    client: &DaemonClient,
    config: &Config,
) -> Result<()> {
    let mut app = App::new();
    app.refresh(client);

    let mut last_tick = Instant::now();
    loop {
        terminal.draw(|frame| ui::draw(frame, &app, config))?;

        let timeout = TICK_RATE.saturating_sub(last_tick.elapsed());
        if event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                // On Windows crossterm reports both Press and Release; act on
                // Press only so a single keystroke isn't handled twice.
                if key.kind == KeyEventKind::Press {
                    app.on_key(key, client);
                }
            }
        }
        if last_tick.elapsed() >= TICK_RATE {
            app.on_tick(client);
            last_tick = Instant::now();
        }
        if app.should_quit {
            return Ok(());
        }
    }
}

/// RAII guard around the terminal: enables raw mode + the alternate screen on
/// `enter`, and restores both on `Drop`. Because `Drop` runs during panic
/// unwinding too, a crash inside the event loop can never leave the user's
/// terminal in raw mode.
struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
        Ok(Self { terminal })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}
