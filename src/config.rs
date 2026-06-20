//! Config — `~/.loopd/config.yaml` (serde_yaml), with sane defaults.
//!
//! Lives at the crate root (not under `core`) because both the daemon (Phase 2)
//! and the agent adapters (Phase 3, `headlessArgs`) need it before the rest of
//! the domain is built. Defaults come from PLAN Part 10.
//!
//! Loading is forgiving: a missing file yields the full defaults, and a partial
//! file is merged onto them field-by-field. Every nested struct carries
//! `#[serde(default)]` + a matching `Default` impl, so a YAML that sets only
//! `daemon.port` still gets the default caps, runaway thresholds, and agents.
//! The home directory helpers ([`loopd_dir`]/[`ensure_loopd_dir`]) are shared
//! with the store so `~/.loopd` resolves the same way everywhere.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// The `~/.loopd` directory (not created — see [`ensure_loopd_dir`]). Falls back
/// to `.loopd` in the current directory if the home directory can't be resolved.
pub fn loopd_dir() -> PathBuf {
    match dirs::home_dir() {
        Some(home) => home.join(".loopd"),
        None => PathBuf::from(".loopd"),
    }
}

/// Like [`loopd_dir`] but creates the directory if it doesn't exist.
pub fn ensure_loopd_dir() -> Result<PathBuf> {
    let dir = loopd_dir();
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating loopd dir {}", dir.display()))?;
    Ok(dir)
}

/// Path to the config file, `~/.loopd/config.yaml`.
pub fn config_path() -> PathBuf {
    loopd_dir().join("config.yaml")
}

/// Top-level config. `#[serde(default)]` lets any missing field fall back to
/// `Config::default()`, so partial YAML files merge cleanly onto the defaults.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Config {
    /// Daemon settings (HTTP port).
    pub daemon: DaemonConfig,
    /// Default run settings + governance policy.
    pub defaults: Defaults,
    /// Known agents, keyed by name (`claude`, `codex`, …). A `BTreeMap` keeps
    /// the order stable for deterministic serialization.
    pub agents: BTreeMap<String, AgentConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            daemon: DaemonConfig::default(),
            defaults: Defaults::default(),
            agents: default_agents(),
        }
    }
}

/// Daemon settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DaemonConfig {
    /// Local HTTP API port.
    pub port: u16,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self { port: 7777 }
    }
}

/// Default run settings and the governance policy applied unless overridden.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Defaults {
    /// Which agent `loop run` uses when none is specified.
    pub agent: String,
    /// What to do when a cap/detector trips.
    pub on_trip: OnTrip,
    /// Cap thresholds (iterations / cost / duration).
    pub caps: Caps,
    /// Runaway detector thresholds.
    pub runaway: Runaway,
    /// No-progress detector settings.
    pub no_progress: NoProgress,
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            agent: "claude".to_string(),
            on_trip: OnTrip::Warn,
            caps: Caps::default(),
            runaway: Runaway::default(),
            no_progress: NoProgress::default(),
        }
    }
}

/// What loopd does when a cap or detector trips. Default `Warn` keeps governance
/// flag-only until the user explicitly opts into pause/kill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnTrip {
    /// Surface a warning, take no action.
    Warn,
    /// Send a notification, take no action.
    Notify,
    /// Pause the run (checkpoint + stop; resumable). Owned runs only.
    Pause,
    /// Kill the run. Owned runs only.
    Kill,
}

/// Cap thresholds. Defaults from PLAN Part 10.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Caps {
    /// Trip after this many agent iterations.
    pub max_iterations: u32,
    /// Trip when cumulative cost exceeds this many USD.
    pub max_cost_usd: f64,
    /// Trip after this many minutes of wall-clock runtime.
    pub max_duration_min: u32,
}

impl Default for Caps {
    fn default() -> Self {
        Self {
            max_iterations: 50,
            max_cost_usd: 2.00,
            max_duration_min: 30,
        }
    }
}

/// Runaway detector thresholds. Defaults from PLAN Part 10.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Runaway {
    /// Flag when the same tool+input repeats this many times consecutively.
    pub repeated_action: u32,
    /// Flag when this many tool errors occur in a row with no success between.
    pub error_streak: u32,
}

impl Default for Runaway {
    fn default() -> Self {
        Self {
            repeated_action: 3,
            error_streak: 4,
        }
    }
}

/// No-progress detector settings. Best-effort: needs a git repo and an opt-in
/// `test_command` (default `None`); skipped otherwise. Defaults from PLAN Part 10.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct NoProgress {
    /// Flag after this many iterations with no git diff change and failing tests.
    pub iterations: u32,
    /// User-provided test command (e.g. `"npm test"`). `None` disables the
    /// tests half of the signal — loopd never authors this command.
    pub test_command: Option<String>,
}

impl Default for NoProgress {
    fn default() -> Self {
        Self {
            iterations: 5,
            test_command: None,
        }
    }
}

/// How to invoke one agent in headless mode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentConfig {
    /// The executable to run (`claude`, `codex`, …).
    pub cmd: String,
    /// Headless invocation args (the task text is appended by the adapter).
    pub headless_args: Vec<String>,
}

/// The default agent registry (Claude Code + Codex), from PLAN Part 10.
fn default_agents() -> BTreeMap<String, AgentConfig> {
    let mut agents = BTreeMap::new();
    agents.insert(
        "claude".to_string(),
        AgentConfig {
            cmd: "claude".to_string(),
            headless_args: ["-p", "--output-format", "stream-json", "--verbose"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        },
    );
    agents.insert(
        "codex".to_string(),
        AgentConfig {
            cmd: "codex".to_string(),
            headless_args: ["exec", "--json"].iter().map(|s| s.to_string()).collect(),
        },
    );
    agents
}

impl Config {
    /// Load config from `~/.loopd/config.yaml`. A missing file yields the full
    /// defaults; a present file is parsed, merged onto defaults, and validated.
    pub fn load() -> Result<Config> {
        Self::load_from(&config_path())
    }

    /// Load config from an explicit path (used by tests). Same semantics as
    /// [`Config::load`].
    pub fn load_from(path: &std::path::Path) -> Result<Config> {
        if !path.exists() {
            return Ok(Config::default());
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let config: Config = serde_yaml::from_str(&text)
            .with_context(|| format!("parsing config {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    /// Reject configs that would break the daemon or agents downstream.
    pub fn validate(&self) -> Result<()> {
        if self.daemon.port == 0 {
            bail!("daemon.port must be non-zero");
        }
        if self.defaults.caps.max_cost_usd < 0.0 {
            bail!("defaults.caps.maxCostUsd must not be negative");
        }
        if !self.agents.contains_key(&self.defaults.agent) {
            bail!(
                "defaults.agent '{}' is not defined in agents",
                self.defaults.agent
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_plan_part_10() {
        let config = Config::default();
        assert_eq!(config.daemon.port, 7777);
        assert_eq!(config.defaults.agent, "claude");
        assert_eq!(config.defaults.on_trip, OnTrip::Warn);
        assert_eq!(config.defaults.caps.max_iterations, 50);
        assert_eq!(config.defaults.caps.max_cost_usd, 2.00);
        assert_eq!(config.defaults.caps.max_duration_min, 30);
        assert_eq!(config.defaults.runaway.repeated_action, 3);
        assert_eq!(config.defaults.runaway.error_streak, 4);
        assert_eq!(config.defaults.no_progress.iterations, 5);
        assert!(config.defaults.no_progress.test_command.is_none());
        assert!(config.agents.contains_key("claude"));
        assert!(config.agents.contains_key("codex"));
        config.validate().expect("defaults are valid");
    }

    #[test]
    fn missing_file_yields_defaults() {
        let path = loopd_dir().join("does-not-exist-config.yaml");
        let config = Config::load_from(&path).expect("load missing");
        assert_eq!(config, Config::default());
    }

    #[test]
    fn partial_yaml_merges_onto_defaults() {
        // Only the port is set; everything else must fall back to defaults.
        let dir = std::env::temp_dir().join(format!("loopd_cfg_{}", crate::core::events::new_run_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.yaml");
        std::fs::write(&path, "daemon:\n  port: 9000\n").unwrap();

        let config = Config::load_from(&path).expect("load partial");
        assert_eq!(config.daemon.port, 9000);
        // Untouched sections keep their defaults.
        assert_eq!(config.defaults.caps.max_iterations, 50);
        assert!(config.agents.contains_key("claude"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn validate_rejects_unknown_default_agent() {
        let mut config = Config::default();
        config.defaults.agent = "ghost".to_string();
        assert!(config.validate().is_err());
    }
}
