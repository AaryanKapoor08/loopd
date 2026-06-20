//! Dashboard state and input handling.
//!
//! [`App`] holds everything the UI renders plus the view state (which pane is
//! focused, the selection, the filter, and any pending confirm). It never
//! touches the store or a process directly — every refresh and action goes
//! through [`DaemonClient`]. A failed poll keeps the daemon's last good snapshot
//! so the UI shows a "daemon unreachable" banner instead of crashing.
//!
//! Actions are **clamped by ownership**: observed (un-owned) runs have no
//! process for loopd to act on, so `k`/`p`/`r` are refused with a message rather
//! than sent to the daemon (which would reject them anyway, ARCHITECTURE §7).

use crossterm::event::{KeyCode, KeyEvent};

use crate::cli::fmt::chronological;
use crate::core::events::{LoopEvent, Run, RunStatus};
use crate::daemon::client::DaemonClient;

/// How many recent events the detail pane requests per refresh.
const DETAIL_EVENT_LIMIT: u32 = 200;

/// Which pane is focused.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum View {
    /// The run list.
    List,
    /// One run's goal, caps progress, and live event stream.
    Detail,
}

/// What the keyboard is currently doing. Normal navigation, editing the filter,
/// or waiting on a kill confirmation.
#[derive(Clone, PartialEq, Eq)]
pub enum Mode {
    /// Default: keys navigate and trigger actions.
    Normal,
    /// Typing into the filter box; keys edit the filter string.
    Filtering,
    /// A destructive kill is staged for this run id, pending `y`/`n`.
    ConfirmKill(String),
}

/// The whole dashboard state.
pub struct App {
    /// Latest `GET /runs` snapshot (kept on a failed poll).
    pub runs: Vec<Run>,
    /// Index into the **visible** (filtered) list of the highlighted row.
    pub selected: usize,
    /// Which pane is focused.
    pub view: View,
    /// Keyboard mode (navigation / filtering / confirm).
    pub mode: Mode,
    /// Case-insensitive substring filter on label/agent/status (empty = all).
    pub filter: String,
    /// The run id the detail pane is showing, if any.
    pub detail_run_id: Option<String>,
    /// The detail pane's events, oldest-first (ready to render).
    pub detail_events: Vec<LoopEvent>,
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
            view: View::List,
            mode: Mode::Normal,
            filter: String::new(),
            detail_run_id: None,
            detail_events: Vec::new(),
            status: "loading…".into(),
            daemon_ok: true,
            should_quit: false,
        }
    }

    // --- derived views -------------------------------------------------------

    /// The runs passing the current filter, in list order.
    pub fn visible_runs(&self) -> Vec<&Run> {
        if self.filter.is_empty() {
            return self.runs.iter().collect();
        }
        let needle = self.filter.to_lowercase();
        self.runs
            .iter()
            .filter(|r| {
                r.label.to_lowercase().contains(&needle)
                    || r.agent.to_lowercase().contains(&needle)
                    || crate::cli::fmt::status_str(r.status).contains(&needle)
            })
            .collect()
    }

    /// The highlighted run within the visible list.
    pub fn selected_run(&self) -> Option<&Run> {
        self.visible_runs().get(self.selected).copied()
    }

    /// The run the detail pane is showing (looked up fresh from the snapshot so
    /// caps progress reflects the latest poll).
    pub fn detail_run(&self) -> Option<&Run> {
        let id = self.detail_run_id.as_ref()?;
        self.runs.iter().find(|r| &r.run_id == id)
    }

    // --- polling -------------------------------------------------------------

    /// Poll `GET /runs`. On success refresh the snapshot; on failure keep the
    /// previous one and flag the daemon down so the UI can show a banner.
    pub fn refresh(&mut self, client: &DaemonClient) {
        match client.list_runs() {
            Ok(runs) => {
                self.runs = runs;
                self.daemon_ok = true;
                self.clamp_selection();
                if self.runs.is_empty() && matches!(self.mode, Mode::Normal) {
                    self.status = "no runs yet — start one with `loop run \"<task>\"`".into();
                }
            }
            Err(e) => {
                self.daemon_ok = false;
                self.status = format!("daemon unreachable: {e}");
            }
        }
    }

    /// Periodic refresh (the ~1s tick): the list always, plus the detail event
    /// stream when the detail pane is open.
    pub fn on_tick(&mut self, client: &DaemonClient) {
        self.refresh(client);
        if self.view == View::Detail {
            self.refresh_detail(client);
        }
    }

    /// Re-fetch the detail pane's event stream (chronological, like `loop logs`).
    fn refresh_detail(&mut self, client: &DaemonClient) {
        if let Some(id) = self.detail_run_id.clone() {
            if let Ok(events) = client.events_for_run(&id, DETAIL_EVENT_LIMIT) {
                self.detail_events = chronological(events);
            }
        }
    }

    // --- input ---------------------------------------------------------------

    /// Handle a keypress, dispatched by mode then view.
    pub fn on_key(&mut self, key: KeyEvent, client: &DaemonClient) {
        match self.mode.clone() {
            Mode::Filtering => self.on_key_filtering(key),
            Mode::ConfirmKill(id) => self.on_key_confirm(key, &id, client),
            Mode::Normal => match self.view {
                View::List => self.on_key_list(key, client),
                View::Detail => self.on_key_detail(key, client),
            },
        }
    }

    fn on_key_list(&mut self, key: KeyEvent, client: &DaemonClient) {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Down => self.move_selection(1),
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Enter => self.open_detail(client),
            KeyCode::Char('k') => self.stage_kill(),
            KeyCode::Char('p') => self.pause_or_resume(client),
            KeyCode::Char('r') => self.retry(client),
            KeyCode::Char('f') => self.begin_filter(),
            _ => {}
        }
    }

    fn on_key_detail(&mut self, key: KeyEvent, client: &DaemonClient) {
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Esc | KeyCode::Enter => {
                self.view = View::List;
                self.detail_events.clear();
            }
            // Actions also work on the run being viewed.
            KeyCode::Char('k') => self.stage_kill(),
            KeyCode::Char('p') => self.pause_or_resume(client),
            KeyCode::Char('r') => self.retry(client),
            _ => {}
        }
    }

    fn on_key_filtering(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.filter.clear();
                self.mode = Mode::Normal;
                self.clamp_selection();
                self.status = "filter cleared".into();
            }
            KeyCode::Enter => {
                self.mode = Mode::Normal;
                self.status = if self.filter.is_empty() {
                    "filter cleared".into()
                } else {
                    format!("filter: {}", self.filter)
                };
            }
            KeyCode::Backspace => {
                self.filter.pop();
                self.clamp_selection();
            }
            KeyCode::Char(c) => {
                self.filter.push(c);
                self.selected = 0;
            }
            _ => {}
        }
    }

    fn on_key_confirm(&mut self, key: KeyEvent, id: &str, client: &DaemonClient) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                self.status = match client.request_kill(id) {
                    Ok(()) => format!("kill requested for {id}"),
                    Err(e) => format!("kill failed: {e}"),
                };
                self.mode = Mode::Normal;
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.status = "kill cancelled".into();
            }
            _ => {}
        }
    }

    // --- actions -------------------------------------------------------------

    /// Open the detail pane for the selected run and load its events now (so the
    /// pane isn't blank until the next tick).
    fn open_detail(&mut self, client: &DaemonClient) {
        let Some(run) = self.selected_run() else {
            return;
        };
        self.detail_run_id = Some(run.run_id.clone());
        self.view = View::Detail;
        self.refresh_detail(client);
    }

    /// Stage a kill for the targeted run, after the ownership clamp. The actual
    /// kill waits for the `y` confirmation so a stray `k` can't end a run.
    fn stage_kill(&mut self) {
        let Some((id, owned, _, _)) = self.target() else {
            return;
        };
        if !owned {
            self.status = "observed run — read-only (cannot kill)".into();
            return;
        }
        self.mode = Mode::ConfirmKill(id);
    }

    /// Toggle pause/resume on the targeted owned run.
    fn pause_or_resume(&mut self, client: &DaemonClient) {
        let Some((id, owned, status, _)) = self.target() else {
            return;
        };
        if !owned {
            self.status = "observed run — read-only (cannot pause)".into();
            return;
        }
        self.status = if status == RunStatus::Paused {
            match client.resume(&id) {
                Ok(()) => format!("resumed {id}"),
                Err(e) => format!("resume failed: {e}"),
            }
        } else {
            match client.pause(&id) {
                Ok(()) => format!("paused {id}"),
                Err(e) => format!("pause failed: {e}"),
            }
        };
    }

    /// Retry the targeted owned run as a fresh run with the same prompt/agent/cwd
    /// (v1 simple re-run — proper Retry lineage lands with governance, Phase 6).
    fn retry(&mut self, client: &DaemonClient) {
        let Some((id, owned, _, run)) = self.target() else {
            return;
        };
        if !owned {
            self.status = "observed run — read-only (cannot retry)".into();
            return;
        }
        self.status = match client.create_run(
            &run.prompt,
            Some(&run.agent),
            Some(&run.cwd),
            None,
            run.model.as_deref(),
        ) {
            Ok(new) => format!("retried {id} → new run {}", new.run_id),
            Err(e) => format!("retry failed: {e}"),
        };
    }

    /// The run an action targets: the detail run when that pane is open, else the
    /// selected list row. Returns owned copies so callers can mutate `self` after.
    fn target(&self) -> Option<(String, bool, RunStatus, Run)> {
        let run = if self.view == View::Detail {
            self.detail_run()
        } else {
            self.selected_run()
        }?;
        Some((run.run_id.clone(), run.owned, run.status, run.clone()))
    }

    fn begin_filter(&mut self) {
        self.mode = Mode::Filtering;
        self.status = "filter (type to match label/agent/status · enter apply · esc clear)".into();
    }

    // --- selection bookkeeping ----------------------------------------------

    /// Move the selection by `delta`, wrapping around the visible list.
    fn move_selection(&mut self, delta: i32) {
        let len = self.visible_runs().len() as i32;
        if len == 0 {
            return;
        }
        self.selected = (self.selected as i32 + delta).rem_euclid(len) as usize;
    }

    /// Keep `selected` in bounds after the run list or filter changes.
    fn clamp_selection(&mut self) {
        let len = self.visible_runs().len();
        if len == 0 {
            self.selected = 0;
        } else if self.selected >= len {
            self.selected = len - 1;
        }
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}
