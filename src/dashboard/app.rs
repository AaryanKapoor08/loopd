//! Dashboard state and input handling.
//!
//! [`App`] holds everything the UI renders plus the small amount of view state
//! (selection, which pane is focused). It never touches the store or a process
//! directly — every refresh and action goes through [`DaemonClient`]. Keeping
//! the daemon's last good snapshot on a failed poll lets the UI show a
//! "daemon unreachable" banner instead of crashing.

use crossterm::event::{KeyCode, KeyEvent};

use crate::core::events::Run;
use crate::daemon::client::DaemonClient;

/// The whole dashboard state.
pub struct App {
    /// Latest `GET /runs` snapshot (kept on a failed poll).
    pub runs: Vec<Run>,
    /// Index into `runs` of the highlighted row.
    pub selected: usize,
    /// One-line status / last-action message shown at the bottom.
    pub status: String,
    /// Whether the last daemon poll succeeded (drives the unreachable banner).
    pub daemon_ok: bool,
    /// Set when the user asks to quit; the event loop exits next iteration.
    pub should_quit: bool,
}

impl App {
    pub fn new() -> Self {
        Self {
            runs: Vec::new(),
            selected: 0,
            status: "loading…".into(),
            daemon_ok: true,
            should_quit: false,
        }
    }

    /// The currently-selected run, if the list is non-empty.
    pub fn selected_run(&self) -> Option<&Run> {
        self.runs.get(self.selected)
    }

    /// Poll `GET /runs`. On success refresh the snapshot; on failure keep the
    /// previous one and flag the daemon down so the UI can show a banner.
    pub fn refresh(&mut self, client: &DaemonClient) {
        match client.list_runs() {
            Ok(runs) => {
                self.runs = runs;
                self.daemon_ok = true;
                self.clamp_selection();
                if self.runs.is_empty() {
                    self.status = "no runs yet — start one with `loop run \"<task>\"`".into();
                }
            }
            Err(e) => {
                self.daemon_ok = false;
                self.status = format!("daemon unreachable: {e}");
            }
        }
    }

    /// Periodic refresh (the ~1s tick).
    pub fn on_tick(&mut self, client: &DaemonClient) {
        self.refresh(client);
    }

    /// Handle a keypress. List-view only for now (detail + actions land next).
    pub fn on_key(&mut self, key: KeyEvent, _client: &DaemonClient) {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Down => self.move_selection(1),
            KeyCode::Up => self.move_selection(-1),
            _ => {}
        }
    }

    /// Move the selection by `delta`, wrapping around the list.
    fn move_selection(&mut self, delta: i32) {
        if self.runs.is_empty() {
            return;
        }
        let len = self.runs.len() as i32;
        self.selected = (self.selected as i32 + delta).rem_euclid(len) as usize;
    }

    /// Keep `selected` in bounds after the run list changes.
    fn clamp_selection(&mut self) {
        if self.runs.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.runs.len() {
            self.selected = self.runs.len() - 1;
        }
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}
