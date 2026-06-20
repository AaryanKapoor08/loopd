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
            Ok(resp) if resp.status().is_success() => {
                resp.json::<Health>().map(|h| h.status == "ok").unwrap_or(false)
            }
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
