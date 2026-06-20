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

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::core::events::{LoopEvent, Run};
use crate::core::store::Store;

/// Placeholder for the Phase-3 supervisor registry (which will track owned PTY
/// processes by run id). Empty in Phase 2: the daemon already owns the `Store`
/// and `Config`, so the HTTP surface and lifecycle can be built and tested
/// before process supervision exists.
#[derive(Default)]
pub struct SupervisorRegistry {}

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
    axum::serve(listener, app(state))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("serving daemon HTTP API")?;
    Ok(())
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
    run.map(Json).ok_or_else(|| ApiError::NotFound(format!("run {id}")))
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

/// Spawn a run — needs the Phase-3 supervisor. Stub until then.
async fn create_run() -> ApiError {
    ApiError::NotImplemented("spawning runs lands in Phase 3 (supervisor)")
}

/// Request a kill. Flags the run via the store; the Phase-3 supervisor acts on
/// the flag for owned runs. Works today because `request_kill` exists in Phase 1.
async fn kill_run(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let store = state.store()?;
    if store.get_run(&id).map_err(ApiError::Internal)?.is_none() {
        return Err(ApiError::NotFound(format!("run {id}")));
    }
    store.request_kill(&id).map_err(ApiError::Internal)?;
    Ok(StatusCode::ACCEPTED)
}

/// Pause a run — checkpoint + stop via the supervisor (Phase 3/6). Stub.
async fn pause_run() -> ApiError {
    ApiError::NotImplemented("pause lands in Phase 3/6 (supervisor checkpoint+stop)")
}

/// Resume a run — re-spawn via the agent's native resume (Phase 3/6). Stub.
async fn resume_run() -> ApiError {
    ApiError::NotImplemented("resume lands in Phase 3/6 (supervisor re-spawn)")
}

/// Mode-B ingest (CC hooks / SDK). Stub until the observer lands.
async fn ingest() -> ApiError {
    ApiError::NotImplemented("/ingest lands in Phase 7 (observer) / Phase 9 (SDK)")
}

// --- errors ------------------------------------------------------------------

/// Route error type. Maps cleanly onto an HTTP status + a small JSON body so
/// clients can branch on the status and surface the message.
enum ApiError {
    NotFound(String),
    NotImplemented(&'static str),
    Internal(anyhow::Error),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            ApiError::NotFound(what) => (StatusCode::NOT_FOUND, format!("{what} not found")),
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
            .oneshot(Request::builder().uri("/health").body(Body::empty()).unwrap())
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
            .oneshot(Request::builder().uri("/runs/nope").body(Body::empty()).unwrap())
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
    async fn unimplemented_routes_return_501() {
        for (method, uri) in [("POST", "/runs"), ("POST", "/ingest"), ("POST", "/runs/x/pause")] {
            let resp = app(test_state())
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(uri)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED, "{method} {uri}");
        }
    }
}
