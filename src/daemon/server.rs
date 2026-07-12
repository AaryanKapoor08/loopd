//! Server — the daemon's local HTTP API (`axum`).
//!
//! The daemon is the only long-lived process and the only DB writer. Every CLI
//! and TUI action goes through these routes; no client opens the store or a
//! process directly. Routes serialize the same `serde` shapes as the Phase-1
//! wire types (camelCase), so the `ts-rs`-generated SDK types match on the wire.
//!
//! Phase 2 wires the read surface end-to-end (`/health`, `/runs`, `/runs/:id`,
//! `/runs/:id/events`) and `kill` (which just flags the run via the store). The
//! routes that need process supervision or the observer are explicit stubs:
//! `POST /runs` (spawn) lands in Phase 3, `pause`/`resume` in Phase 3/6, and
//! `POST /ingest` (Mode B) in Phase 7. Stubs return `501 Not Implemented` with a
//! clear message rather than pretending to work.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::agents::{adapter_for, RunOpts};
use crate::cli::fmt::chronological;
use crate::config::{Config, OnTrip};
use crate::core::detector::{Action, Governor};
use crate::core::events::{new_run_id, now_ms, LoopEvent, Run, RunReason, RunStatus};
use crate::core::store::Store;
use crate::observer::sdk::{SdkReportReq, SdkTrackReq};
use crate::observer::webhook::IngestResponse;
use crate::supervisor::SupervisorRegistry;

/// How often the governance detector sweeps live runs.
const GOVERNANCE_TICK: Duration = Duration::from_millis(1500);

/// Observed (Mode-B) runs with no activity for this long are presumed over and
/// closed as `Done`. Backstop for sessions that never fire a `SessionEnd` hook
/// (hooks not installed, terminal killed, machine slept) — without it an
/// abandoned session sits `Running` forever and trips `cap-duration`. Fresh
/// activity on the same session id revives the run (`observer::revive_if_observed`).
const OBSERVED_IDLE_TIMEOUT_MS: i64 = 30 * 60 * 1000;

/// Shared state handed to every route. `Clone` is cheap — everything is behind
/// an `Arc`. The `Store` is additionally wrapped in a `Mutex` because a
/// `rusqlite::Connection` is `Send` but not `Sync`; handlers lock it, do their
/// synchronous query, and drop the guard before returning (never held across an
/// `.await`).
#[derive(Clone)]
pub struct AppState {
    /// The single store. Daemon-only writer (the non-negotiable invariant).
    pub store: Arc<Mutex<Store>>,
    /// Loaded config (daemon port, defaults, agents).
    pub config: Arc<Config>,
    /// Owned-process registry — populated in Phase 3.
    pub supervisor: Arc<SupervisorRegistry>,
}

impl AppState {
    /// Build state from an owned `Store` + `Config`.
    pub fn new(store: Store, config: Config) -> Self {
        Self {
            store: Arc::new(Mutex::new(store)),
            config: Arc::new(config),
            supervisor: Arc::new(SupervisorRegistry::default()),
        }
    }

    /// Lock the store, mapping a poisoned mutex to an internal error.
    fn store(&self) -> std::result::Result<std::sync::MutexGuard<'_, Store>, ApiError> {
        self.store
            .lock()
            .map_err(|_| ApiError::Internal(anyhow!("store mutex poisoned")))
    }
}

/// Build the router. Separated from [`serve`] so tests can exercise routes via
/// `tower`'s `oneshot` without binding a socket.
pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/runs", get(list_runs).post(create_run))
        .route("/runs/:id", get(get_run))
        .route("/runs/:id/events", get(events_for_run))
        .route("/runs/:id/kill", post(kill_run))
        .route("/runs/:id/pause", post(pause_run))
        .route("/runs/:id/resume", post(resume_run))
        .route("/ingest", post(ingest))
        .route("/sdk/track", post(sdk_track))
        .route("/sdk/report", post(sdk_report))
        .route("/sdk/runs/:id", get(sdk_verdict))
        .with_state(state)
}

/// Bind `127.0.0.1:<config.daemon.port>` and serve until a shutdown signal.
pub async fn serve(state: AppState) -> Result<()> {
    let port = state.config.daemon.port;
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding daemon to {addr}"))?;
    tracing::info!("loopd daemon listening on http://{addr}");

    // The governance detector runs on its own (blocking) thread, not a tokio
    // task: each sweep takes the store mutex and may shell out to git / the
    // user's test command, which would stall the async runtime. The thread polls
    // a stop flag so daemon shutdown winds it down cleanly.
    let stop = Arc::new(AtomicBool::new(false));
    let tick_handle = spawn_governance(state.clone(), stop.clone());
    // Mode-B observer: tail user-started `claude` transcripts read-only. Shares
    // the same stop flag so shutdown winds it down with the governance tick.
    let tail_handle = crate::observer::transcript::spawn(state.store.clone(), stop.clone());

    let result = axum::serve(listener, app(state))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("serving daemon HTTP API");

    stop.store(true, Ordering::Relaxed);
    if let Some(h) = tick_handle {
        let _ = h.join();
    }
    if let Some(h) = tail_handle {
        let _ = h.join();
    }
    result
}

/// Spawn the governance tick thread: sweep live runs every [`GOVERNANCE_TICK`]
/// through the [`Governor`] until `stop` is set. Returns the join handle (or
/// `None` if the thread couldn't be spawned — the daemon still serves).
fn spawn_governance(state: AppState, stop: Arc<AtomicBool>) -> Option<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("governance".into())
        .spawn(move || {
            let mut gov = Governor::new();
            while !stop.load(Ordering::Relaxed) {
                governance_tick(&state, &mut gov);
                // Sleep in small slices so shutdown is observed promptly.
                let mut waited = Duration::ZERO;
                while waited < GOVERNANCE_TICK && !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(150));
                    waited += Duration::from_millis(150);
                }
            }
        })
        .ok()
}

/// One governance sweep: gather live runs + their recent events under the store
/// lock, evaluate each off the lock (the [`Governor`] may touch git / tests),
/// then persist new flags/status and apply pause/kill via the supervisor.
///
/// The persist step **re-reads** each run and merges only flags/status, so it
/// never clobbers live metrics (iteration/cost/tokens) a supervisor reader thread
/// may have written between gather and persist.
fn governance_tick(state: &AppState, gov: &mut Governor) {
    // 1. Gather (live runs = Running | Stuck; Paused/terminal are not evaluated).
    //    Observed runs that have gone quiet past the idle timeout are closed here
    //    instead of evaluated — their session is presumed over.
    let now = now_ms();
    let snapshot: Vec<(Run, Vec<LoopEvent>)> = {
        let Ok(store) = state.store.lock() else {
            return;
        };
        let runs = store.list_runs().unwrap_or_default();
        runs.into_iter()
            .filter(|r| matches!(r.status, RunStatus::Running | RunStatus::Stuck))
            .filter(|r| {
                let observed = !r.owned && !r.enforced_remotely();
                if observed && now.saturating_sub(r.last_event_at) > OBSERVED_IDLE_TIMEOUT_MS {
                    let mut closed = r.clone();
                    closed.status = RunStatus::Done;
                    // The session's last sign of life is its effective end.
                    closed.ended_at = Some(closed.last_event_at);
                    closed.updated_at = now;
                    let _ = store.upsert_run(&closed);
                    return false;
                }
                true
            })
            .map(|r| {
                let events = store.events_for_run(&r.run_id, 64).unwrap_or_default();
                (r, chronological(events))
            })
            .collect()
    };
    let live: HashSet<String> = snapshot.iter().map(|(r, _)| r.run_id.clone()).collect();

    // 2. Evaluate each run (off the lock).
    let mut to_act: Vec<(String, Vec<String>, RunStatus, Action)> = Vec::new();
    for (run, recent) in &snapshot {
        let decision = gov.evaluate(run, recent, &state.config);
        if let Some(log) = &decision.log {
            tracing::warn!("{log}");
        }
        // Desired live status from the (non-process) action.
        let desired_status = match decision.action {
            Action::Warn | Action::Notify if !decision.flags.is_empty() => RunStatus::Stuck,
            Action::None if run.status == RunStatus::Stuck => RunStatus::Running, // recovered
            _ => run.status, // Pause/Kill: the supervisor sets the terminal status
        };
        let flags_changed = decision.flags != run.flags;
        let status_changed = desired_status != run.status;
        let acts = matches!(decision.action, Action::Pause | Action::Kill);
        if flags_changed || status_changed || acts {
            to_act.push((
                run.run_id.clone(),
                decision.flags,
                desired_status,
                decision.action,
            ));
        }
    }

    // 3. Persist flags/status (re-read + merge so live metrics aren't clobbered).
    if !to_act.is_empty() {
        if let Ok(store) = state.store.lock() {
            for (id, flags, status, _) in &to_act {
                if let Ok(Some(mut current)) = store.get_run(id) {
                    current.flags = flags.clone();
                    // Only touch status while the run is still live — never undo a
                    // terminal status the supervisor wrote in the meantime.
                    if matches!(current.status, RunStatus::Running | RunStatus::Stuck) {
                        current.status = *status;
                    }
                    current.updated_at = now_ms();
                    let _ = store.upsert_run(&current);
                }
            }
        }
    }

    // 4. Apply pause/kill (off the lock). Two enforcement paths reach here —
    //    clamp_action already degraded *observed* runs to notify, so a Pause/Kill
    //    is either an owned process or a remotely-enforced (SDK) run:
    //    - owned (Mode A): tell the supervisor to stop the real process;
    //    - SDK (`enforced_remotely`): no process to touch — fold the action into
    //      the store so the verdict the client reads back carries it.
    for (id, _, _, action) in &to_act {
        if !matches!(action, Action::Pause | Action::Kill) {
            continue;
        }
        // Default to the owned path if the row vanished, preserving prior behavior.
        let owned = state
            .store
            .lock()
            .ok()
            .and_then(|s| s.get_run(id).ok().flatten())
            .map(|r| r.owned)
            .unwrap_or(true);
        if owned {
            match action {
                Action::Pause => {
                    state.supervisor.pause(id);
                }
                Action::Kill => {
                    if let Ok(store) = state.store.lock() {
                        let _ = store.request_kill(id);
                    }
                    state.supervisor.kill(id);
                }
                _ => {}
            }
        } else if let Ok(store) = state.store.lock() {
            if let Ok(Some(mut run)) = store.get_run(id) {
                crate::core::detector::fold_remote_action(&mut run, *action);
                let _ = store.upsert_run(&run);
            }
        }
    }

    gov.forget_all_except(&live);
}

/// Resolve when the daemon should shut down: Ctrl-C on any platform, or SIGTERM
/// on unix. Phase 3's supervisor will flush in-flight supervised `Run` rows here
/// before the future returns; in Phase 2 every write is already committed
/// per-request, so there is nothing in flight to lose.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    tracing::info!("daemon received shutdown signal; exiting");
}

// --- handlers ----------------------------------------------------------------

/// Liveness probe. The `DaemonClient` polls this to decide whether to auto-start
/// the daemon, so the shape is stable and self-describing.
#[derive(Debug, Serialize, Deserialize)]
pub struct Health {
    /// Always `"ok"` when the daemon is up.
    pub status: String,
    /// Binary name.
    pub name: String,
    /// Crate version.
    pub version: String,
}

async fn health() -> Json<Health> {
    Json(Health {
        status: "ok".to_string(),
        name: env!("CARGO_PKG_NAME").to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    })
}

async fn list_runs(State(state): State<AppState>) -> Result<Json<Vec<Run>>, ApiError> {
    let runs = state.store()?.list_runs().map_err(ApiError::Internal)?;
    Ok(Json(runs))
}

async fn get_run(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Run>, ApiError> {
    let run = state.store()?.get_run(&id).map_err(ApiError::Internal)?;
    run.map(Json)
        .ok_or_else(|| ApiError::NotFound(format!("run {id}")))
}

/// How many recent events to return for `/runs/:id/events`.
#[derive(Debug, Deserialize)]
struct EventsQuery {
    limit: Option<u32>,
}

async fn events_for_run(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<EventsQuery>,
) -> Result<Json<Vec<LoopEvent>>, ApiError> {
    let limit = q.limit.unwrap_or(200);
    let events = state
        .store()?
        .events_for_run(&id, limit)
        .map_err(ApiError::Internal)?;
    Ok(Json(events))
}

/// Body of `POST /runs`. camelCase to match the wire/SDK convention.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateRunReq {
    /// The task text the agent runs.
    prompt: String,
    /// Adapter id; defaults to `claude`.
    #[serde(default)]
    agent: Option<String>,
    /// Working directory to spawn in (the agent edits here, never loopd).
    #[serde(default)]
    cwd: Option<String>,
    /// Human-readable label; defaults to the run id.
    #[serde(default)]
    label: Option<String>,
    /// Optional model override.
    #[serde(default)]
    model: Option<String>,
    /// Per-run cap override: max iterations (else config default).
    #[serde(default)]
    max_iterations: Option<u32>,
    /// Per-run cap override: max cumulative cost in USD (else config default).
    #[serde(default)]
    max_cost_usd: Option<f64>,
    /// Per-run cap override: max wall-clock minutes (else config default).
    #[serde(default)]
    max_duration_min: Option<u32>,
    /// Per-run on-trip override (`warn`/`notify`/`pause`/`kill`; else config default).
    #[serde(default)]
    on_trip: Option<OnTrip>,
    /// Why this run exists; defaults to a user-initiated run. `Retry` carries
    /// lineage via `parent_run_id` (ARCHITECTURE §3).
    #[serde(default)]
    run_reason: Option<RunReason>,
    /// The run this one derives from (set on a `Retry`).
    #[serde(default)]
    parent_run_id: Option<String>,
}

/// Spawn an owned (Mode-A) run: insert the row, then hand it to the supervisor,
/// which spawns the agent through a PTY and streams events back into the store.
async fn create_run(
    State(state): State<AppState>,
    Json(req): Json<CreateRunReq>,
) -> Result<(StatusCode, Json<Run>), ApiError> {
    let agent = req.agent.unwrap_or_else(|| "claude".to_string());
    let adapter = adapter_for(&agent)
        .ok_or_else(|| ApiError::BadRequest(format!("unknown agent `{agent}`")))?;

    let run_id = new_run_id();
    let cwd = req.cwd.unwrap_or_default();
    let mut run = Run::new(&run_id);
    run.agent = agent;
    run.label = req.label.unwrap_or_else(|| run_id.clone());
    run.prompt = req.prompt.clone();
    run.cwd = cwd.clone();
    run.owned = true;
    run.run_reason = req.run_reason.unwrap_or(RunReason::UserRun);
    run.parent_run_id = req.parent_run_id;
    run.max_iterations = req.max_iterations;
    run.max_cost_usd = req.max_cost_usd;
    run.max_duration_min = req.max_duration_min;
    run.on_trip = req.on_trip;
    run.status = RunStatus::Running;
    state
        .store()?
        .upsert_run(&run)
        .map_err(ApiError::Internal)?;

    let opts = RunOpts { model: req.model };
    state
        .supervisor
        .spawn(
            adapter.as_ref(),
            &run_id,
            &req.prompt,
            &cwd,
            &opts,
            state.store.clone(),
        )
        .map_err(ApiError::Internal)?;

    // Re-read so the response reflects the pid the supervisor just recorded.
    let run = state
        .store()?
        .get_run(&run_id)
        .map_err(ApiError::Internal)?
        .ok_or_else(|| ApiError::Internal(anyhow!("run {run_id} vanished after spawn")))?;
    Ok((StatusCode::CREATED, Json(run)))
}

/// Request a kill. Flags the run via the store (the detector/clients see it) and,
/// for owned runs, tells the supervisor to take down the process tree. Observed
/// (unowned) runs only get the flag — loopd has no process to stop.
async fn kill_run(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    {
        let store = state.store()?;
        if store.get_run(&id).map_err(ApiError::Internal)?.is_none() {
            return Err(ApiError::NotFound(format!("run {id}")));
        }
        store.request_kill(&id).map_err(ApiError::Internal)?;
    }
    state.supervisor.kill(&id); // no-op for unowned/finished runs
    Ok(StatusCode::ACCEPTED)
}

/// Pause an owned run: capture its agent session id and stop the process
/// (ARCHITECTURE §4 — no ConPTY suspend). The run becomes `Paused` and is
/// resumable. Only owned, currently-supervised runs can be paused.
async fn pause_run(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    if state
        .store()?
        .get_run(&id)
        .map_err(ApiError::Internal)?
        .is_none()
    {
        return Err(ApiError::NotFound(format!("run {id}")));
    }
    if !state.supervisor.owns(&id) {
        return Err(ApiError::BadRequest(format!(
            "run {id} is not an owned, running process — cannot pause"
        )));
    }
    state.supervisor.pause(&id);
    Ok(StatusCode::ACCEPTED)
}

/// Resume a paused run by re-spawning the agent with its native `--resume`
/// (the parser drops the replayed history). Needs the captured agent session id.
async fn resume_run(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let run = state
        .store()?
        .get_run(&id)
        .map_err(ApiError::Internal)?
        .ok_or_else(|| ApiError::NotFound(format!("run {id}")))?;
    if run.status != RunStatus::Paused {
        return Err(ApiError::BadRequest(format!(
            "run {id} is {:?}, only paused runs can be resumed",
            run.status
        )));
    }
    let session_id = run.agent_session_id.clone().ok_or_else(|| {
        ApiError::BadRequest(format!("run {id} has no agent session id to resume"))
    })?;
    let adapter = adapter_for(&run.agent)
        .ok_or_else(|| ApiError::BadRequest(format!("unknown agent `{}`", run.agent)))?;

    // Flip the row back to Running before re-spawning.
    let mut resumed = run.clone();
    resumed.status = RunStatus::Running;
    resumed.ended_at = None;
    resumed.updated_at = now_ms();
    state
        .store()?
        .upsert_run(&resumed)
        .map_err(ApiError::Internal)?;

    let opts = RunOpts {
        model: run.model.clone(),
    };
    state
        .supervisor
        .resume(
            adapter.as_ref(),
            &id,
            &session_id,
            &run.prompt,
            &run.cwd,
            &opts,
            state.store.clone(),
        )
        .map_err(ApiError::Internal)?;
    Ok(StatusCode::ACCEPTED)
}

/// Mode-B ingest (CC hooks). Normalizes the hook payload, correlates it to the
/// observed (read-only) run for its session, and returns the run's current
/// verdict — the enforcement return channel the Phase-9 SDK reuses
/// (ARCHITECTURE §4). Accepts the raw hook JSON (not our wire shape), so the body
/// is a lenient `serde_json::Value`.
async fn ingest(
    State(state): State<AppState>,
    Json(payload): Json<serde_json::Value>,
) -> Json<IngestResponse> {
    Json(crate::observer::webhook::ingest_hook(
        &state.store,
        &payload,
    ))
}

/// Surface-2 register (`POST /sdk/track`): create a `source = Sdk` run loopd does
/// not own but governs via the verdict channel (ARCHITECTURE §4). Returns the new
/// run id with an initial verdict.
async fn sdk_track(
    State(state): State<AppState>,
    Json(req): Json<SdkTrackReq>,
) -> Result<(StatusCode, Json<IngestResponse>), ApiError> {
    let resp = crate::observer::sdk::track(&state.store, &req)
        .ok_or_else(|| ApiError::Internal(anyhow!("failed to register sdk run")))?;
    Ok((StatusCode::CREATED, Json(resp)))
}

/// Surface-2 report (`POST /sdk/report`): fold one event into a tracked run, roll
/// up its metrics, govern it synchronously, and return the verdict the SDK obeys.
/// 404 if the run id is unknown.
async fn sdk_report(
    State(state): State<AppState>,
    Json(req): Json<SdkReportReq>,
) -> Result<Json<IngestResponse>, ApiError> {
    crate::observer::sdk::report(&state.store, &state.config, &req)
        .map(Json)
        .ok_or_else(|| ApiError::NotFound(format!("run {}", req.run_id)))
}

/// Surface-2 verdict poll (`GET /sdk/runs/:id`): the standing verdict for a
/// tracked run — what the SDK's `check()` reads at the top of each turn.
async fn sdk_verdict(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<IngestResponse>, ApiError> {
    crate::observer::sdk::verdict(&state.store, &id)
        .map(Json)
        .ok_or_else(|| ApiError::NotFound(format!("run {id}")))
}

// --- errors ------------------------------------------------------------------

/// Route error type. Maps cleanly onto an HTTP status + a small JSON body so
/// clients can branch on the status and surface the message.
enum ApiError {
    NotFound(String),
    BadRequest(String),
    NotImplemented(&'static str),
    Internal(anyhow::Error),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            ApiError::NotFound(what) => (StatusCode::NOT_FOUND, format!("{what} not found")),
            ApiError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
            ApiError::NotImplemented(msg) => (StatusCode::NOT_IMPLEMENTED, msg.to_string()),
            ApiError::Internal(err) => {
                tracing::error!("internal error: {err:#}");
                (StatusCode::INTERNAL_SERVER_ERROR, format!("{err}"))
            }
        };
        (status, Json(serde_json::json!({ "error": message }))).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::events::{new_run_id, EventKind, Source};
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt; // for `oneshot`

    fn test_state() -> AppState {
        let dir = std::env::temp_dir().join(format!("loopd_srv_{}", new_run_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(dir.join("test.db")).expect("open store");
        AppState::new(store, Config::default())
    }

    async fn body_json(resp: Response) -> serde_json::Value {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn health_returns_ok() {
        let resp = app(test_state())
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["status"], "ok");
        assert_eq!(v["name"], env!("CARGO_PKG_NAME"));
    }

    #[tokio::test]
    async fn runs_round_trip_through_routes() {
        let state = test_state();

        // Seed a run directly through the store.
        let run_id = new_run_id();
        {
            let store = state.store.lock().unwrap();
            let mut run = Run::new(&run_id);
            run.agent = "claude".to_string();
            store.upsert_run(&run).unwrap();
            let ev = LoopEvent::new(&run_id, Source::Supervisor, EventKind::RunStart);
            store.insert_event(&ev).unwrap();
        }

        // GET /runs returns the seeded run.
        let resp = app(state.clone())
            .oneshot(Request::builder().uri("/runs").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v.as_array().unwrap().len(), 1);
        assert_eq!(v[0]["runId"], run_id);
        // camelCase wire shape (matches the ts-rs types).
        assert_eq!(v[0]["contextWindow"], 200_000);

        // GET /runs/:id/events returns the seeded event.
        let resp = app(state.clone())
            .oneshot(
                Request::builder()
                    .uri(format!("/runs/{run_id}/events"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v.as_array().unwrap().len(), 1);
        assert_eq!(v[0]["kind"], "run_start");

        // GET an unknown run → 404.
        let resp = app(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/runs/nope")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        // POST /runs/:id/kill on the real run → 202 Accepted.
        let resp = app(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/runs/{run_id}/kill"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn ingest_creates_an_observed_run_and_returns_a_verdict() {
        // A CC SessionStart hook payload (Mode B). /ingest correlates it to a new
        // observed run and returns the verdict channel.
        let state = test_state();
        let body = serde_json::json!({
            "session_id": "sess_route_1",
            "cwd": "C:\\dev\\loop",
            "hook_event_name": "SessionStart",
            "source": "startup"
        });
        let resp = app(state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/ingest")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert!(v["runId"].is_string());
        assert_eq!(v["verdict"], "ok");

        // The run is observed (read-only) and shows up in /runs.
        let runs = state.store.lock().unwrap().list_runs().unwrap();
        assert_eq!(runs.len(), 1);
        assert!(!runs[0].owned);
    }

    #[tokio::test]
    async fn sdk_track_then_report_over_cap_kills_via_routes() {
        // The full Surface-2 seam over HTTP: register a programmatic loop, report
        // cost until the cap trips, and confirm the verdict channel returns kill
        // (what the SDK's check() throws on). Same governance as a CLI run.
        let state = test_state();

        // POST /sdk/track → 201 + a run id.
        let track_body = serde_json::json!({
            "label": "api loop",
            "agent": "anthropic",
            "maxCostUsd": 0.5,
            "onTrip": "kill"
        });
        let resp = app(state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/sdk/track")
                    .header("content-type", "application/json")
                    .body(Body::from(track_body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let v = body_json(resp).await;
        assert_eq!(v["verdict"], "ok");
        let run_id = v["runId"].as_str().unwrap().to_string();

        // The run shows up unowned in /runs (it appears in `loop dash`).
        let run = state
            .store
            .lock()
            .unwrap()
            .get_run(&run_id)
            .unwrap()
            .unwrap();
        assert!(!run.owned);
        assert_eq!(run.run_reason, RunReason::Sdk);

        // POST /sdk/report with cost over the cap → verdict kill.
        let report_body = serde_json::json!({
            "runId": run_id,
            "kind": "token_usage",
            "costUsd": 0.9
        });
        let resp = app(state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/sdk/report")
                    .header("content-type", "application/json")
                    .body(Body::from(report_body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["verdict"], "kill");

        // GET /sdk/runs/:id echoes the standing kill for check() to poll.
        let resp = app(state.clone())
            .oneshot(
                Request::builder()
                    .uri(format!("/sdk/runs/{run_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_json(resp).await["verdict"], "kill");

        let run = state
            .store
            .lock()
            .unwrap()
            .get_run(&run_id)
            .unwrap()
            .unwrap();
        assert_eq!(run.status, RunStatus::Killed);
    }

    #[tokio::test]
    async fn sdk_report_unknown_run_is_404() {
        let body = serde_json::json!({ "runId": "run_nope", "costUsd": 1.0 });
        let resp = app(test_state())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/sdk/report")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn create_run_with_unknown_agent_is_400() {
        // Validates the wiring without spawning a real agent.
        let body = serde_json::json!({ "prompt": "hi", "agent": "definitely-not-an-agent" });
        let resp = app(test_state())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/runs")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn governance_tick_flags_a_repeating_run_and_marks_it_stuck() {
        use crate::core::events::ToolStatus;

        let state = test_state();
        let run_id = new_run_id();
        {
            let store = state.store.lock().unwrap();
            let mut run = Run::new(&run_id);
            run.agent = "claude".into();
            run.owned = true;
            run.status = RunStatus::Running;
            store.upsert_run(&run).unwrap();
            // Three identical tool calls in a row → repeated-action (threshold 3).
            for _ in 0..3 {
                let mut ev = LoopEvent::new(&run_id, Source::Supervisor, EventKind::ToolUse);
                ev.tool = Some("bash".into());
                ev.tool_input_hash = Some(42);
                ev.tool_status = Some(ToolStatus::Ok);
                store.insert_event(&ev).unwrap();
            }
        }

        let mut gov = Governor::new();
        governance_tick(&state, &mut gov);

        let run = state
            .store
            .lock()
            .unwrap()
            .get_run(&run_id)
            .unwrap()
            .unwrap();
        assert!(
            run.flags.contains(&"repeated-action".to_string()),
            "expected repeated-action flag, got {:?}",
            run.flags
        );
        // Default on-trip is Warn → flag only, status flips to Stuck (not killed).
        assert_eq!(run.status, RunStatus::Stuck);
    }

    #[test]
    fn governance_tick_closes_an_idle_observed_run_but_not_a_fresh_one() {
        let state = test_state();
        let (idle_id, fresh_id) = (new_run_id(), new_run_id());
        {
            let store = state.store.lock().unwrap();
            for (id, idle) in [(&idle_id, true), (&fresh_id, false)] {
                let mut run = Run::new(id);
                run.agent = "claude".into();
                run.owned = false;
                run.run_reason = RunReason::Observed;
                run.status = RunStatus::Running;
                if idle {
                    run.last_event_at = now_ms() - OBSERVED_IDLE_TIMEOUT_MS - 60_000;
                }
                store.upsert_run(&run).unwrap();
            }
        }

        let mut gov = Governor::new();
        governance_tick(&state, &mut gov);

        let store = state.store.lock().unwrap();
        let idle = store.get_run(&idle_id).unwrap().unwrap();
        assert_eq!(idle.status, RunStatus::Done, "idle observed run must close");
        assert_eq!(idle.ended_at, Some(idle.last_event_at));
        let fresh = store.get_run(&fresh_id).unwrap().unwrap();
        assert_eq!(fresh.status, RunStatus::Running, "fresh run must stay live");
    }

    /// A throwaway adapter whose program is a long-running system command; it
    /// reuses the Claude parser (non-JSON output → harmless Output events). Lets
    /// the tick tests act on a *real owned process* without spending agent tokens.
    struct SleeperAdapter;
    impl crate::agents::Adapter for SleeperAdapter {
        fn id(&self) -> &str {
            "sleeper"
        }
        fn program(&self) -> &str {
            if cfg!(windows) {
                "ping"
            } else {
                "sh"
            }
        }
        fn build_args(&self, _t: &str, _o: &RunOpts) -> Vec<String> {
            Vec::new()
        }
        fn resume_args(&self, _s: &str, _t: &str, _o: &RunOpts) -> Vec<String> {
            Vec::new()
        }
        fn new_parser(&self, run_id: &str) -> Box<dyn crate::agents::StreamParser> {
            crate::agents::claude::ClaudeAdapter::new().new_parser(run_id)
        }
    }

    /// Seed an owned, cost-cap-tripping run and spawn a real ~30s process under
    /// the supervisor for it, opting it into `on_trip`. Returns the run id.
    fn spawn_capped_sleeper(state: &AppState, on_trip: OnTrip) -> String {
        let run_id = new_run_id();
        {
            let store = state.store.lock().unwrap();
            let mut run = Run::new(&run_id);
            run.agent = "claude".into();
            run.owned = true;
            run.status = RunStatus::Running;
            run.cost_usd = 1.0; // already over the cap below
            run.max_cost_usd = Some(0.5);
            run.on_trip = Some(on_trip);
            store.upsert_run(&run).unwrap();
        }
        let args: Vec<String> = if cfg!(windows) {
            vec!["-n".into(), "30".into(), "127.0.0.1".into()]
        } else {
            vec!["-c".into(), "sleep 30".into()]
        };
        let handle = state
            .supervisor
            .spawn_raw(&SleeperAdapter, &run_id, &args, "", state.store.clone())
            .expect("spawn sleeper");
        assert!(handle.pid.is_some(), "need a pid to act on");
        std::thread::sleep(std::time::Duration::from_millis(400)); // let it start
        run_id
    }

    /// Wait until `run_id` leaves `Running`, or `within` elapses.
    fn await_not_running(state: &AppState, run_id: &str, within: std::time::Duration) -> Run {
        let deadline = std::time::Instant::now() + within;
        loop {
            let run = state
                .store
                .lock()
                .unwrap()
                .get_run(run_id)
                .unwrap()
                .unwrap();
            if run.status != RunStatus::Running || std::time::Instant::now() >= deadline {
                return run;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    /// Live(ish) gate: the tick **kills** a real owned process over a cost cap
    /// under `--on-trip kill`. Verifies the tick→on-trip→supervisor path the pure
    /// detector tests can't reach.
    #[test]
    fn governance_tick_kills_a_real_owned_process_over_a_cost_cap() {
        let state = test_state();
        let run_id = spawn_capped_sleeper(&state, OnTrip::Kill);

        let mut gov = Governor::new();
        governance_tick(&state, &mut gov);

        let run = await_not_running(&state, &run_id, std::time::Duration::from_secs(15));
        assert!(
            run.flags.contains(&"cap-cost".to_string()),
            "expected cap-cost flag, got {:?}",
            run.flags
        );
        assert_eq!(run.status, RunStatus::Killed, "the tick must kill the run");
    }

    /// Live(ish) gate: the tick **pauses** (checkpoint + stop) a real owned
    /// process over a cost cap under `--on-trip pause`, so the run is resumable.
    #[test]
    fn governance_tick_pauses_a_real_owned_process_over_a_cost_cap() {
        let state = test_state();
        let run_id = spawn_capped_sleeper(&state, OnTrip::Pause);

        let mut gov = Governor::new();
        governance_tick(&state, &mut gov);

        let run = await_not_running(&state, &run_id, std::time::Duration::from_secs(15));
        assert!(run.flags.contains(&"cap-cost".to_string()));
        assert_eq!(run.status, RunStatus::Paused, "the tick must pause the run");
        // A paused run is resumable, not ended.
        assert!(run.ended_at.is_none(), "a paused run is not ended");
    }

    #[tokio::test]
    async fn pause_unknown_run_is_404() {
        let resp = app(test_state())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/runs/nope/pause")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
