//! Detector — the governance brain that runs on the daemon tick.
//!
//! This is loopd's wedge: it watches every live run and **acts** when one
//! derails. The [`Governor`] is owned by the daemon's tick task (`daemon::server`
//! spawns it); on each tick it [`evaluate`](Governor::evaluate)s a run against the
//! deterministic [`crate::policies`] registry plus the best-effort, stateful
//! no-progress signal, then returns a [`Decision`]: the run's full current flag
//! set and the **on-trip action** to take, already clamped by ownership.
//!
//! Why a stateful struct and not pure functions: two signals need memory across
//! ticks — the **no-progress** git fingerprint (has the tree changed since N
//! iterations ago?) and the **warn throttle** (don't log a stuck run every tick;
//! claude-squad `log.NewEvery(60s)`). The pure, per-tick checks live in
//! `policies`; the cross-tick memory lives here.
//!
//! **Safety (ARCHITECTURE §10):** the worst action here is `Kill`/`Pause` (stop
//! an agent). No-progress only ever *reads* git (`core::git`, read-only) and runs
//! the user's **opt-in** `test_command` — loopd never edits the repo.

use std::collections::HashMap;
use std::process::Command;
use std::time::{Duration, Instant};

use crate::config::{Caps, Config, OnTrip, Runaway};
use crate::core::events::{now_ms, LoopEvent, Run};
use crate::core::git;
use crate::policies::{builtin_policies, DetectCtx, Policy};

/// Context-window fraction at/above which `context-exhaustion` trips.
const CONTEXT_PCT: f64 = 0.90;
/// Don't log the same stuck run more often than this (warn-throttle).
const WARN_EVERY: Duration = Duration::from_secs(60);
/// Don't re-run the (potentially heavy) no-progress `test_command` more often
/// than this per run — a stalled run would otherwise trigger it every tick.
const TEST_EVERY: Duration = Duration::from_secs(30);

/// The action the daemon should take for a flagged run, **after** clamping by
/// ownership (an observed run can only be `Warn`/`Notify` — there is no process
/// to stop). Mirrors [`OnTrip`] minus the un-actionable cases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// No flags — nothing to do.
    None,
    /// Flag only; surface a warning (the default).
    Warn,
    /// Flag only; emit a notification (treated as `Warn` until a notifier lands).
    Notify,
    /// Stop the agent, resumably (checkpoint + stop). Owned runs only.
    Pause,
    /// Stop the agent for good. Owned runs only.
    Kill,
}

/// The outcome of evaluating one run on one tick.
pub struct Decision {
    /// The run's complete current flag set (the tick **replaces** `Run.flags`
    /// with this, so a transient flag like `repeated-action` clears when the
    /// behavior stops; caps, which only grow, stay).
    pub flags: Vec<String>,
    /// What to do, clamped by ownership.
    pub action: Action,
    /// A throttled, human-readable log line — `Some` only when due (so a run
    /// flagged every tick logs at most once per [`WARN_EVERY`]).
    pub log: Option<String>,
}

/// Per-run no-progress memory: the last git fingerprint and the iteration at
/// which it last changed.
struct NoProgressState {
    sig: String,
    iter_at_change: u32,
}

/// The governance engine. One instance lives for the daemon's lifetime and is
/// driven by the tick task.
pub struct Governor {
    policies: Vec<Box<dyn Policy>>,
    no_progress: HashMap<String, NoProgressState>,
    last_warn: HashMap<String, Instant>,
    last_test: HashMap<String, Instant>,
    /// Runs we've already issued a `Pause`/`Kill` for, so we don't re-fire it on
    /// the next tick before the status transition lands.
    acted: HashMap<String, Action>,
}

impl Default for Governor {
    fn default() -> Self {
        Self::new()
    }
}

impl Governor {
    pub fn new() -> Self {
        Self {
            policies: builtin_policies(),
            no_progress: HashMap::new(),
            last_warn: HashMap::new(),
            last_test: HashMap::new(),
            acted: HashMap::new(),
        }
    }

    /// Evaluate one live run against every policy plus the no-progress signal,
    /// and decide the on-trip action. Pure except for: cross-tick memory (above)
    /// and best-effort git/test I/O for no-progress — callers must invoke this
    /// **off** the store lock (it may shell out to git / the test command).
    pub fn evaluate(&mut self, run: &Run, recent: &[LoopEvent], config: &Config) -> Decision {
        let ctx = DetectCtx {
            run,
            recent,
            caps: effective_caps(run, config),
            runaway: effective_runaway(run, config),
            now_ms: now_ms(),
            context_pct: CONTEXT_PCT,
        };

        let mut flags: Vec<String> = self
            .policies
            .iter()
            .filter_map(|p| p.check(&ctx))
            .collect();
        if let Some(f) = self.no_progress(run, config) {
            flags.push(f);
        }

        if flags.is_empty() {
            // Recovered: forget any prior action so a future trip can act again.
            self.acted.remove(&run.run_id);
            return Decision {
                flags,
                action: Action::None,
                log: None,
            };
        }

        let action = clamp_action(effective_on_trip(run, config), run.owned);
        // Don't re-issue Pause/Kill while the prior one is still settling.
        let action = match action {
            Action::Pause | Action::Kill if self.acted.get(&run.run_id) == Some(&action) => {
                Action::None
            }
            Action::Pause | Action::Kill => {
                self.acted.insert(run.run_id.clone(), action);
                action
            }
            other => other,
        };

        let log = Self::due(&mut self.last_warn, &run.run_id, WARN_EVERY)
            .then(|| format!("run {} flagged [{}] → {:?}", run.run_id, flags.join(","), action));

        Decision { flags, action, log }
    }

    /// Best-effort no-progress: requires an **opt-in** `test_command` *and* a git
    /// repo; otherwise returns `None` (skipped silently — the documented v1
    /// behavior). When the working tree hasn't changed for `iterations` and the
    /// user's tests are failing, the run is stuck. Git is read-only; the test
    /// command is the user's own (ARCHITECTURE §7/§10).
    fn no_progress(&mut self, run: &Run, config: &Config) -> Option<String> {
        let np = &config.defaults.no_progress;
        let test_cmd = np.test_command.as_deref()?; // None → skip silently
        if !git::is_git_repo(&run.cwd) {
            return None;
        }
        let sig = git::diff_signature(&run.cwd)?;

        // Update the per-run fingerprint memory; learn whether the tree has been
        // stalled for at least the configured number of iterations.
        let stalled = match self.no_progress.get_mut(&run.run_id) {
            Some(state) if state.sig == sig => {
                run.iteration.saturating_sub(state.iter_at_change) >= np.iterations
            }
            Some(state) => {
                state.sig = sig;
                state.iter_at_change = run.iteration;
                false
            }
            None => {
                self.no_progress.insert(
                    run.run_id.clone(),
                    NoProgressState {
                        sig,
                        iter_at_change: run.iteration,
                    },
                );
                false
            }
        };
        if !stalled {
            return None;
        }

        // Suspected stuck. Confirm with the user's tests — but rarely (the test
        // run is heavy and a stalled run would otherwise trigger it every tick).
        if !Self::due(&mut self.last_test, &run.run_id, TEST_EVERY) {
            return None;
        }
        // Only a *failing* test run means "no progress": passing tests (or a test
        // we couldn't run) are not evidence of a stuck loop.
        match run_test_command(&run.cwd, test_cmd) {
            Some(false) => Some("no-progress".to_string()),
            _ => None,
        }
    }

    /// Throttle: returns `true` (and stamps `now` into `map[id]`) when at least
    /// `every` has elapsed since this key's last stamp, or it was never stamped.
    fn due(map: &mut HashMap<String, Instant>, id: &str, every: Duration) -> bool {
        let now = Instant::now();
        let ready = map
            .get(id)
            .map(|t| now.duration_since(*t) >= every)
            .unwrap_or(true);
        if ready {
            map.insert(id.to_string(), now);
        }
        ready
    }

    /// Drop per-run memory for runs that are no longer live, so the maps don't
    /// grow without bound over a long-lived daemon.
    pub fn forget_all_except(&mut self, live: &std::collections::HashSet<String>) {
        self.no_progress.retain(|k, _| live.contains(k));
        self.last_warn.retain(|k, _| live.contains(k));
        self.last_test.retain(|k, _| live.contains(k));
        self.acted.retain(|k, _| live.contains(k));
    }
}

/// Clamp an [`OnTrip`] policy to an actionable [`Action`] given ownership. An
/// observed (unowned) run has no process loopd can stop, so `Pause`/`Kill`
/// degrade to `Notify` (ARCHITECTURE §7).
pub fn clamp_action(on_trip: OnTrip, owned: bool) -> Action {
    match on_trip {
        OnTrip::Warn => Action::Warn,
        OnTrip::Notify => Action::Notify,
        OnTrip::Pause if owned => Action::Pause,
        OnTrip::Kill if owned => Action::Kill,
        // Unowned: can't touch a process we don't own.
        OnTrip::Pause | OnTrip::Kill => Action::Notify,
    }
}

/// Effective cap thresholds for a run: each per-run override (from `loop run
/// --max-*`) wins, falling back to `config.defaults.caps` field by field.
fn effective_caps(run: &Run, config: &Config) -> Caps {
    let d = config.defaults.caps;
    Caps {
        max_iterations: run.max_iterations.unwrap_or(d.max_iterations),
        max_cost_usd: run.max_cost_usd.unwrap_or(d.max_cost_usd),
        max_duration_min: run.max_duration_min.unwrap_or(d.max_duration_min),
    }
}

/// Runaway thresholds are global (config-only) for v1 — no per-run override.
fn effective_runaway(_run: &Run, config: &Config) -> Runaway {
    config.defaults.runaway
}

/// Effective on-trip action: the per-run override (`loop run --on-trip`) wins,
/// else the config default.
fn effective_on_trip(run: &Run, config: &Config) -> OnTrip {
    run.on_trip.unwrap_or(config.defaults.on_trip)
}

/// Run the user's opt-in `test_command` in `cwd`. `Some(true)` = passed (exit 0),
/// `Some(false)` = failed, `None` = couldn't run. **This is the only place loopd
/// runs a user-configured command**; it is opt-in and never authored by loopd.
fn run_test_command(cwd: &str, cmd: &str) -> Option<bool> {
    let mut command = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/c", cmd]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", cmd]);
        c
    };
    if !cwd.is_empty() {
        command.current_dir(cwd);
    }
    command.output().ok().map(|o| o.status.success())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::events::{new_run_id, now_ms, EventKind, Run, RunStatus, Source, ToolStatus};

    fn looping_run() -> Run {
        let mut run = Run::new(new_run_id());
        run.status = RunStatus::Running;
        run.owned = true;
        run.started_at = now_ms();
        run
    }

    fn tool_use(hash: u64) -> LoopEvent {
        let mut e = LoopEvent::new("r", Source::Supervisor, EventKind::ToolUse);
        e.tool = Some("bash".into());
        e.tool_input_hash = Some(hash);
        e
    }

    fn tool_result(status: ToolStatus) -> LoopEvent {
        let mut e = LoopEvent::new("r", Source::Supervisor, EventKind::ToolResult);
        e.tool_status = Some(status);
        e
    }

    #[test]
    fn clamp_degrades_unowned_pause_and_kill() {
        assert_eq!(clamp_action(OnTrip::Pause, true), Action::Pause);
        assert_eq!(clamp_action(OnTrip::Kill, true), Action::Kill);
        assert_eq!(clamp_action(OnTrip::Pause, false), Action::Notify);
        assert_eq!(clamp_action(OnTrip::Kill, false), Action::Notify);
        assert_eq!(clamp_action(OnTrip::Warn, false), Action::Warn);
    }

    #[test]
    fn repeated_action_trips_and_default_on_trip_is_warn() {
        let mut gov = Governor::new();
        let run = looping_run();
        let events = vec![tool_use(1), tool_use(1), tool_use(1)];
        let d = gov.evaluate(&run, &events, &Config::default());
        assert!(d.flags.contains(&"repeated-action".to_string()));
        // Default config on-trip is Warn → flag only.
        assert_eq!(d.action, Action::Warn);
        // First flag logs (throttle slot was empty).
        assert!(d.log.is_some());
    }

    #[test]
    fn error_streak_with_kill_policy_kills_an_owned_run_once() {
        let mut gov = Governor::new();
        let run = looping_run();
        let mut config = Config::default();
        config.defaults.on_trip = OnTrip::Kill;
        let events = vec![
            tool_result(ToolStatus::Error),
            tool_result(ToolStatus::Error),
            tool_result(ToolStatus::Error),
            tool_result(ToolStatus::Error),
        ];
        let first = gov.evaluate(&run, &events, &config);
        assert!(first.flags.contains(&"error-streak".to_string()));
        assert_eq!(first.action, Action::Kill);
        // Second tick must not re-issue the kill while it settles.
        let second = gov.evaluate(&run, &events, &config);
        assert_eq!(second.action, Action::None);
    }

    #[test]
    fn observed_run_kill_policy_degrades_to_notify() {
        let mut gov = Governor::new();
        let mut run = looping_run();
        run.owned = false;
        let mut config = Config::default();
        config.defaults.on_trip = OnTrip::Kill;
        let events = vec![tool_use(9), tool_use(9), tool_use(9)];
        let d = gov.evaluate(&run, &events, &config);
        assert!(!d.flags.is_empty());
        assert_eq!(d.action, Action::Notify);
    }

    #[test]
    fn healthy_run_has_no_flags_and_no_action() {
        let mut gov = Governor::new();
        let run = looping_run();
        let events = vec![tool_use(1), tool_use(2), tool_result(ToolStatus::Ok)];
        let d = gov.evaluate(&run, &events, &Config::default());
        assert!(d.flags.is_empty());
        assert_eq!(d.action, Action::None);
        assert!(d.log.is_none());
    }

    #[test]
    fn no_progress_is_silent_without_a_test_command() {
        // Default config has no_progress.test_command = None → never flagged,
        // even inside a git repo with a stalled tree.
        let mut gov = Governor::new();
        let mut run = looping_run();
        run.cwd = env!("CARGO_MANIFEST_DIR").to_string(); // a real git repo
        run.iteration = 100;
        let d = gov.evaluate(&run, &[], &Config::default());
        assert!(!d.flags.contains(&"no-progress".to_string()));
    }
}
