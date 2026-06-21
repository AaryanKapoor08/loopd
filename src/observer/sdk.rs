//! SDK ingest — Surface 2 (`@loopd/sdk`), the third surface.
//!
//! Phases 3–7 cover agents loopd *spawns* (Mode A) or *observes* (Mode B). This
//! module governs a **programmatic** loop — a plain Anthropic-SDK / API loop, or
//! later a LangGraph graph — that reports into loopd over HTTP and obeys the
//! verdict it gets back. loopd owns no OS process here, so enforcement is the
//! **return channel** (ARCHITECTURE §4): the SDK's `check()` throws when the
//! daemon's [`Verdict`] is `pause`/`kill`. That makes loopd a control plane, not
//! just a CLI babysitter.
//!
//! Three calls, all returning the run's current verdict via [`IngestResponse`]:
//! - [`track`]  — register a new `source = Sdk` run (`owned = false`,
//!   `run_reason = Sdk`) and return its id;
//! - [`report`] — fold one [`LoopEvent`] into the run, roll up its metrics,
//!   **govern it synchronously** (so a tripped cap halts the loop on the very
//!   report that tripped it, not a tick later), and return the verdict;
//! - [`verdict`] — read back the current verdict (what `check()` polls).
//!
//! A `source = Sdk` run is governed by the exact same [`Governor`] + [`crate::policies`]
//! as owned runs — it gets caps and detectors for free. The only difference is the
//! *application*: an SDK trip routes through [`fold_remote_action`] (set the run's
//! verdict state) instead of the supervisor (stop a process), because the worst
//! loopd can do to a loop it doesn't own is return `kill` and trust the client.

use std::sync::{Arc, Mutex};

use serde::Deserialize;

use crate::cli::fmt::chronological;
use crate::config::{Config, OnTrip};
use crate::core::detector::{fold_remote_action, Governor};
use crate::core::events::{
    new_run_id, now_ms, EventKind, LoopEvent, Run, RunReason, RunStatus, Source, ToolStatus,
    Verdict,
};
use crate::core::store::Store;
use crate::observer::webhook::IngestResponse;

use super::verdict_for;

/// Body of `POST /sdk/track`: register a programmatic loop. All optional — caps
/// map onto the same per-run overrides the CLI's `--max-*`/`--on-trip` set, so an
/// SDK run is governed identically to a `loop run`. camelCase to match the wire.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SdkTrackReq {
    /// Human-readable label for the cockpit (defaults to the run id).
    #[serde(default)]
    pub label: Option<String>,
    /// Display agent/vendor (e.g. `anthropic`); defaults to `sdk`.
    #[serde(default)]
    pub agent: Option<String>,
    /// Working directory the loop runs in (for the no-progress signal).
    #[serde(default)]
    pub cwd: Option<String>,
    /// Model id the loop drives, if known (display + future pricing).
    #[serde(default)]
    pub model: Option<String>,
    /// Per-run cap: max cumulative cost in USD (else config default).
    #[serde(default)]
    pub max_cost_usd: Option<f64>,
    /// Per-run cap: max iterations (else config default).
    #[serde(default)]
    pub max_iterations: Option<u32>,
    /// Per-run cap: max wall-clock minutes (else config default).
    #[serde(default)]
    pub max_duration_min: Option<u32>,
    /// Per-run on-trip action. `kill`/`pause` are honored for SDK runs (the client
    /// obeys the verdict); `None` falls back to the config default (`warn`).
    #[serde(default)]
    pub on_trip: Option<OnTrip>,
}

/// Body of `POST /sdk/report`: one event from a tracked loop, plus the metric
/// **deltas** it carries. The daemon owns [`LoopEvent`] construction (stamping
/// `ts`/`source = Sdk`), so the SDK never hand-builds the wire event — it sends
/// this thin envelope and lets the single Rust model stay the source of truth.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SdkReportReq {
    /// The run this report belongs to (from [`track`]).
    pub run_id: String,
    /// What kind of event this is; defaults to `output`.
    #[serde(default)]
    pub kind: Option<EventKind>,
    /// Tool name, for a `tool_use`/`tool_result` event (also feeds
    /// repeated-action detection via a stable hash).
    #[serde(default)]
    pub tool: Option<String>,
    /// Tool outcome, for a `tool_result` event (feeds error-streak detection).
    #[serde(default)]
    pub tool_status: Option<ToolStatus>,
    /// Free-text payload (assistant text, error message, …).
    #[serde(default)]
    pub text: Option<String>,
    /// Iterations to add to the run's turn count (1 from `iteration()`).
    #[serde(default)]
    pub iterations: u32,
    /// Input tokens to add to the cumulative total.
    #[serde(default)]
    pub tokens_in: Option<u32>,
    /// Output tokens to add to the cumulative total.
    #[serde(default)]
    pub tokens_out: Option<u32>,
    /// Cost (USD) to add to the cumulative total — the cost cap reads the sum.
    #[serde(default)]
    pub cost_usd: Option<f64>,
}

/// Register a programmatic loop and return its run id with an initial `ok`
/// verdict. The run is `owned = false` (loopd never started it) but
/// `run_reason = Sdk` so it is [`Run::enforced_remotely`] — caps act via the
/// verdict channel, not a process. Returns `None` only if the store write fails.
pub fn track(store: &Arc<Mutex<Store>>, req: &SdkTrackReq) -> Option<IngestResponse> {
    let run_id = new_run_id();
    let mut run = Run::new(&run_id);
    run.agent = req.agent.clone().unwrap_or_else(|| "sdk".to_string());
    run.label = req.label.clone().unwrap_or_else(|| run_id.clone());
    run.owned = false;
    run.run_reason = RunReason::Sdk;
    run.status = RunStatus::Running;
    run.model = req.model.clone();
    run.cwd = req.cwd.clone().unwrap_or_default();
    run.max_cost_usd = req.max_cost_usd;
    run.max_iterations = req.max_iterations;
    run.max_duration_min = req.max_duration_min;
    run.on_trip = req.on_trip;
    let now = now_ms();
    run.started_at = now;
    run.last_event_at = now;
    run.updated_at = now;

    {
        let store = store.lock().ok()?;
        store.upsert_run(&run).ok()?;
        // One Sdk-sourced RunStart so the run has a lifecycle anchor in the stream
        // (no other surface emits one for this id, so it can't collide).
        let ev = LoopEvent::new(&run_id, Source::Sdk, EventKind::RunStart);
        let _ = store.insert_event(&ev);
    }

    Some(IngestResponse {
        run_id: Some(run_id),
        verdict: Verdict::Ok,
    })
}

/// Ingest one report from a tracked loop, govern the run synchronously, and
/// return the resulting verdict. `None` means the run id is unknown (the route
/// maps that to 404). A report against a run that has already left `Running`/
/// `Stuck` is a harmless no-op that just echoes the standing verdict — the loop
/// may report once more before its `check()` observes the `kill`.
pub fn report(
    store: &Arc<Mutex<Store>>,
    config: &Config,
    req: &SdkReportReq,
) -> Option<IngestResponse> {
    // 1. Load, roll up the deltas, and persist the event — all under the lock.
    let (mut run, recent) = {
        let store = store.lock().ok()?;
        let mut run = store.get_run(&req.run_id).ok().flatten()?;

        // Already terminal/paused: nothing to roll up; report the standing verdict.
        if !matches!(run.status, RunStatus::Running | RunStatus::Stuck) {
            return Some(IngestResponse {
                run_id: Some(run.run_id.clone()),
                verdict: verdict_for(&run),
            });
        }

        run.iteration = run.iteration.saturating_add(req.iterations);
        run.cost_usd += req.cost_usd.unwrap_or(0.0);
        run.tokens_in = run.tokens_in.saturating_add(req.tokens_in.unwrap_or(0));
        run.tokens_out = run.tokens_out.saturating_add(req.tokens_out.unwrap_or(0));
        let now = now_ms();
        run.last_event_at = now;
        run.updated_at = now;

        let ev = build_event(&run.run_id, req, run.iteration);
        let _ = store.insert_event(&ev);
        let _ = store.upsert_run(&run);

        let recent = chronological(store.events_for_run(&run.run_id, 64).unwrap_or_default());
        (run, recent)
    };

    // 2. Govern off the lock (a fresh Governor — caps/streaks are pure given the
    //    run + events; the stateful no-progress signal is opt-in and skipped
    //    without a configured test command).
    let decision = Governor::new().evaluate(&run, &recent, config);
    run.flags = decision.flags;
    fold_remote_action(&mut run, decision.action);

    // 3. Persist the verdict state and return it (the SDK's check() obeys it).
    if let Ok(store) = store.lock() {
        let _ = store.upsert_run(&run);
    }
    Some(IngestResponse {
        run_id: Some(run.run_id.clone()),
        verdict: verdict_for(&run),
    })
}

/// Read back the current verdict for a tracked run — what `check()` polls at the
/// top of each turn. `None` if the run id is unknown.
pub fn verdict(store: &Arc<Mutex<Store>>, run_id: &str) -> Option<IngestResponse> {
    let store = store.lock().ok()?;
    let run = store.get_run(run_id).ok().flatten()?;
    Some(IngestResponse {
        verdict: verdict_for(&run),
        run_id: Some(run.run_id),
    })
}

/// Build the [`LoopEvent`] for one report. The per-event values are the *deltas*
/// the loop just incurred (not the cumulative totals on the run); `iteration` is
/// the run's post-update turn number so the event sorts correctly.
fn build_event(run_id: &str, req: &SdkReportReq, iteration: u32) -> LoopEvent {
    let mut ev = LoopEvent::new(run_id, Source::Sdk, req.kind.unwrap_or(EventKind::Output));
    ev.tool = req.tool.clone();
    ev.tool_status = req.tool_status;
    ev.text = req.text.clone();
    ev.tokens_in = req.tokens_in;
    ev.tokens_out = req.tokens_out;
    ev.cost_usd = req.cost_usd;
    ev.iteration = Some(iteration);
    // A stable hash of the tool name lets repeated-action detection work for SDK
    // loops too (a loop hammering the same tool reads as a streak). Matches the
    // adapters' `hash_str` shape.
    if let Some(tool) = &req.tool {
        ev.tool_input_hash = Some(hash_str(tool));
    }
    ev
}

/// Stable hash of a string, mirroring the adapters' `hash_str` so the Phase-6
/// detector treats SDK tool calls like CLI ones.
fn hash_str(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::events::new_run_id;

    fn test_store() -> Arc<Mutex<Store>> {
        let dir = std::env::temp_dir().join(format!("loopd_sdk_{}", new_run_id()));
        std::fs::create_dir_all(&dir).unwrap();
        Arc::new(Mutex::new(Store::open(dir.join("t.db")).unwrap()))
    }

    fn track_kill(store: &Arc<Mutex<Store>>, max_cost_usd: f64) -> String {
        let req = SdkTrackReq {
            label: Some("api loop".into()),
            agent: Some("anthropic".into()),
            max_cost_usd: Some(max_cost_usd),
            on_trip: Some(OnTrip::Kill),
            ..Default::default()
        };
        track(store, &req).unwrap().run_id.unwrap()
    }

    #[test]
    fn track_registers_an_unowned_remotely_enforced_run() {
        let store = test_store();
        let run_id = track_kill(&store, 1.0);

        let run = store.lock().unwrap().get_run(&run_id).unwrap().unwrap();
        assert!(!run.owned, "loopd does not own an SDK loop");
        assert_eq!(run.run_reason, RunReason::Sdk);
        assert!(run.enforced_remotely());
        assert_eq!(run.status, RunStatus::Running);
        assert_eq!(run.agent, "anthropic");
        // One Sdk RunStart anchors the stream.
        let events = store.lock().unwrap().events_for_run(&run_id, 10).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].source, Source::Sdk);
        assert_eq!(events[0].kind, EventKind::RunStart);
    }

    #[test]
    fn report_rolls_up_cost_and_keeps_ok_under_the_cap() {
        let store = test_store();
        let run_id = track_kill(&store, 1.0);

        let req = SdkReportReq {
            run_id: run_id.clone(),
            kind: Some(EventKind::TokenUsage),
            cost_usd: Some(0.25),
            tokens_in: Some(1_000),
            tokens_out: Some(200),
            ..Default::default()
        };
        let resp = report(&store, &Config::default(), &req).unwrap();
        assert_eq!(resp.verdict, Verdict::Ok);

        let run = store.lock().unwrap().get_run(&run_id).unwrap().unwrap();
        assert!((run.cost_usd - 0.25).abs() < 1e-9);
        assert_eq!(run.tokens_in, 1_000);
        assert_eq!(run.status, RunStatus::Running);
    }

    #[test]
    fn report_kills_the_run_the_moment_the_cost_cap_trips() {
        let store = test_store();
        let run_id = track_kill(&store, 0.50);
        let cfg = Config::default();

        // First report stays under the cap.
        let r1 = report(
            &store,
            &cfg,
            &SdkReportReq {
                run_id: run_id.clone(),
                kind: Some(EventKind::TokenUsage),
                cost_usd: Some(0.30),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(r1.verdict, Verdict::Ok);

        // The report that pushes cumulative cost over the cap returns Kill *now*
        // (synchronous governance — not a tick later) and marks the run Killed.
        let r2 = report(
            &store,
            &cfg,
            &SdkReportReq {
                run_id: run_id.clone(),
                kind: Some(EventKind::TokenUsage),
                cost_usd: Some(0.30),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(r2.verdict, Verdict::Kill, "the cost cap must halt the loop");

        let run = store.lock().unwrap().get_run(&run_id).unwrap().unwrap();
        assert_eq!(run.status, RunStatus::Killed);
        assert!(run.kill_requested);
        assert!(run.flags.contains(&"cap-cost".to_string()));
        assert!(run.ended_at.is_some());

        // verdict() echoes the standing kill for check() to poll.
        assert_eq!(verdict(&store, &run_id).unwrap().verdict, Verdict::Kill);
    }

    #[test]
    fn report_against_an_unknown_run_is_none() {
        let store = test_store();
        let req = SdkReportReq {
            run_id: "run_nope".into(),
            ..Default::default()
        };
        assert!(report(&store, &Config::default(), &req).is_none());
        assert!(verdict(&store, "run_nope").is_none());
    }

    #[test]
    fn iteration_reports_advance_the_turn_count() {
        let store = test_store();
        let run_id = track_kill(&store, 100.0);
        for _ in 0..3 {
            report(
                &store,
                &Config::default(),
                &SdkReportReq {
                    run_id: run_id.clone(),
                    kind: Some(EventKind::Assistant),
                    iterations: 1,
                    ..Default::default()
                },
            )
            .unwrap();
        }
        let run = store.lock().unwrap().get_run(&run_id).unwrap().unwrap();
        assert_eq!(run.iteration, 3);
    }
}
