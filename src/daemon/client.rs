//! Client — the thin HTTP client every CLI/TUI command goes through.
//!
//! `DaemonClient` is the *only* way the CLI talks to the daemon: it never opens
//! the store or a process directly. It uses `reqwest`'s blocking client (which
//! owns its own runtime) so the CLI stays synchronous. [`DaemonClient::ensure_running`]
//! makes the daemon transparent — any command can call it, and the daemon is
//! auto-started (via [`super::lifecycle`]) and waited-on if it isn't already up.

use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde::de::DeserializeOwned;

use crate::config::Config;
use crate::core::events::{LoopEvent, Run};

use super::lifecycle;
use super::server::Health;

/// A blocking client bound to the daemon's local HTTP API.
pub struct DaemonClient {
    base: String,
    http: reqwest::blocking::Client,
}

/// The fields of a `POST /runs` request. Only `prompt` is required; the rest are
/// optional overrides the daemon fills from config when unset. Sent as camelCase
/// JSON (the wire convention). The on-trip/run-reason strings are the serde wire
/// words (`warn`/`pause`/…, `user_run`/`retry`/…).
#[derive(Default)]
pub struct NewRun<'a> {
    /// The task text the agent runs (required).
    pub prompt: &'a str,
    /// Adapter id (else config default agent).
    pub agent: Option<&'a str>,
    /// Working directory (else the daemon's cwd).
    pub cwd: Option<&'a str>,
    /// Human-readable label (else the run id).
    pub label: Option<&'a str>,
    /// Model override.
    pub model: Option<&'a str>,
    /// Per-run cap: max iterations.
    pub max_iterations: Option<u32>,
    /// Per-run cap: max cumulative cost in USD.
    pub max_cost_usd: Option<f64>,
    /// Per-run cap: max wall-clock minutes.
    pub max_duration_min: Option<u32>,
    /// Per-run on-trip override (wire word: `warn`/`notify`/`pause`/`kill`).
    pub on_trip: Option<&'a str>,
    /// Why the run exists (wire word: `user_run`/`retry`/…). Defaults to a user run.
    pub run_reason: Option<&'a str>,
    /// Lineage: the run this one derives from (set on a retry).
    pub parent_run_id: Option<&'a str>,
}

impl DaemonClient {
    /// Bind to `http://127.0.0.1:<port>`.
    pub fn new(port: u16) -> Self {
        let http = reqwest::blocking::Client::builder()
            // A generous ceiling for normal calls; `health` overrides it with a
            // short per-request timeout so a down daemon fails fast.
            .timeout(Duration::from_secs(30))
            .build()
            .expect("building reqwest client");
        Self {
            base: format!("http://127.0.0.1:{port}"),
            http,
        }
    }

    /// Bind using the configured daemon port.
    pub fn from_config(config: &Config) -> Self {
        Self::new(config.daemon.port)
    }

    /// The base URL (`http://127.0.0.1:<port>`).
    pub fn base_url(&self) -> &str {
        &self.base
    }

    /// `GET /health` — `true` iff the daemon answers `ok`. Fails fast (short
    /// timeout) so callers can cheaply probe a possibly-down daemon.
    pub fn health(&self) -> bool {
        let res = self
            .http
            .get(format!("{}/health", self.base))
            .timeout(Duration::from_millis(500))
            .send();
        match res {
            Ok(resp) if resp.status().is_success() => resp
                .json::<Health>()
                .map(|h| h.status == "ok")
                .unwrap_or(false),
            _ => false,
        }
    }

    /// Block until the daemon is healthy or `timeout` elapses.
    pub fn wait_healthy(&self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.health() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                bail!(
                    "daemon at {} did not become healthy within {:?}",
                    self.base,
                    timeout
                );
            }
            std::thread::sleep(Duration::from_millis(150));
        }
    }

    /// Make the daemon transparent: if `/health` is unreachable, start it
    /// detached and wait for it to come up. A no-op when already healthy.
    pub fn ensure_running(&self, config: &Config) -> Result<()> {
        if self.health() {
            return Ok(());
        }
        lifecycle::start(config).context("auto-starting the daemon")?;
        self.wait_healthy(Duration::from_secs(10))
    }

    // --- typed API calls (used by the Phase-4 CLI) ---------------------------

    /// `POST /runs` — start an owned (Mode-A) run; returns the created `Run`
    /// (with its id + pid). Unset fields are omitted from the body so the daemon
    /// applies its own defaults (agent, label, cwd, caps). A `400` (e.g. unknown
    /// agent) surfaces as the daemon's message rather than a generic HTTP error.
    pub fn create_run(&self, req: &NewRun) -> Result<Run> {
        let mut body = serde_json::Map::new();
        body.insert("prompt".into(), req.prompt.into());
        if let Some(a) = req.agent {
            body.insert("agent".into(), a.into());
        }
        if let Some(c) = req.cwd {
            body.insert("cwd".into(), c.into());
        }
        if let Some(l) = req.label {
            body.insert("label".into(), l.into());
        }
        if let Some(m) = req.model {
            body.insert("model".into(), m.into());
        }
        if let Some(v) = req.max_iterations {
            body.insert("maxIterations".into(), v.into());
        }
        if let Some(v) = req.max_cost_usd {
            body.insert("maxCostUsd".into(), v.into());
        }
        if let Some(v) = req.max_duration_min {
            body.insert("maxDurationMin".into(), v.into());
        }
        if let Some(t) = req.on_trip {
            body.insert("onTrip".into(), t.into());
        }
        if let Some(r) = req.run_reason {
            body.insert("runReason".into(), r.into());
        }
        if let Some(p) = req.parent_run_id {
            body.insert("parentRunId".into(), p.into());
        }

        let resp = self
            .http
            .post(format!("{}/runs", self.base))
            .json(&serde_json::Value::Object(body))
            .send()
            .context("POST /runs")?;
        if !resp.status().is_success() {
            // Prefer the daemon's `{ "error": … }` message over a bare status.
            let status = resp.status();
            let msg = resp
                .json::<serde_json::Value>()
                .ok()
                .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
                .unwrap_or_else(|| status.to_string());
            bail!("{msg}");
        }
        resp.json().context("decoding created run")
    }

    /// `GET /runs`.
    pub fn list_runs(&self) -> Result<Vec<Run>> {
        self.get_json("/runs")
    }

    /// `GET /runs/:id` → `None` on 404.
    pub fn get_run(&self, id: &str) -> Result<Option<Run>> {
        let resp = self
            .http
            .get(format!("{}/runs/{id}", self.base))
            .send()
            .with_context(|| format!("GET /runs/{id}"))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let resp = resp.error_for_status().context("GET run")?;
        Ok(Some(resp.json().context("decoding run")?))
    }

    /// `GET /runs/:id/events?limit=`.
    pub fn events_for_run(&self, id: &str, limit: u32) -> Result<Vec<LoopEvent>> {
        let resp = self
            .http
            .get(format!("{}/runs/{id}/events", self.base))
            .query(&[("limit", limit)])
            .send()
            .with_context(|| format!("GET /runs/{id}/events"))?
            .error_for_status()
            .context("GET events")?;
        resp.json().context("decoding events")
    }

    /// `POST /ingest` — forward a raw CC hook payload (already JSON) to the
    /// daemon and return the run's verdict. Short timeout: this runs inside the
    /// user's `claude` session and must not stall it.
    pub fn ingest(&self, payload: &str) -> Result<crate::observer::webhook::IngestResponse> {
        let resp = self
            .http
            .post(format!("{}/ingest", self.base))
            .header("content-type", "application/json")
            .timeout(Duration::from_secs(2))
            .body(payload.to_string())
            .send()
            .context("POST /ingest")?
            .error_for_status()
            .context("ingest")?;
        resp.json().context("decoding ingest response")
    }

    /// `POST /runs/:id/kill`.
    pub fn request_kill(&self, id: &str) -> Result<()> {
        let resp = self
            .http
            .post(format!("{}/runs/{id}/kill", self.base))
            .send()
            .with_context(|| format!("POST /runs/{id}/kill"))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            bail!("no such run: {id}");
        }
        resp.error_for_status().context("kill run")?;
        Ok(())
    }

    /// `POST /runs/:id/pause` — checkpoint + stop an owned run (ARCHITECTURE §4).
    /// The daemon rejects this (`400`) for observed/finished runs; its message is
    /// surfaced verbatim so the caller can show it.
    pub fn pause(&self, id: &str) -> Result<()> {
        self.post_action(id, "pause")
    }

    /// `POST /runs/:id/resume` — re-spawn a paused run with its native `--resume`.
    /// `400` (with the daemon's message) when the run isn't paused.
    pub fn resume(&self, id: &str) -> Result<()> {
        self.post_action(id, "resume")
    }

    /// POST `/runs/:id/<action>`, surfacing the daemon's `{ "error": … }` body on
    /// failure (so a `400 "cannot pause"` reads clearly, not as a bare status).
    fn post_action(&self, id: &str, action: &str) -> Result<()> {
        let resp = self
            .http
            .post(format!("{}/runs/{id}/{action}", self.base))
            .send()
            .with_context(|| format!("POST /runs/{id}/{action}"))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            bail!("no such run: {id}");
        }
        if !resp.status().is_success() {
            let status = resp.status();
            let msg = resp
                .json::<serde_json::Value>()
                .ok()
                .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
                .unwrap_or_else(|| status.to_string());
            bail!("{msg}");
        }
        Ok(())
    }

    fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let url = format!("{}{path}", self.base);
        self.http
            .get(&url)
            .send()
            .with_context(|| format!("GET {url}"))?
            .error_for_status()
            .map_err(|e| anyhow!("{e}"))?
            .json()
            .with_context(|| format!("decoding {path}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_is_localhost() {
        assert_eq!(DaemonClient::new(7777).base_url(), "http://127.0.0.1:7777");
    }

    #[test]
    fn health_is_false_when_nothing_listens() {
        // Nothing is bound here; connection refused must read as "not healthy".
        assert!(!DaemonClient::new(59_321).health());
    }
}
