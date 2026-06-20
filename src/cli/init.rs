//! `loop init` — first-run bootstrap: create config, diagnose agents, and ensure
//! the daemon is up. The Mode-B hook-install step is a Phase-7 stub (forward
//! dep — `loop init` does not block on it).

use anyhow::{Context, Result};

use crate::agents::{adapter_for, find_on_path};
use crate::config::{config_path, ensure_loopd_dir, Config};
use crate::daemon::client::DaemonClient;

pub fn init() -> Result<()> {
    // 1. Create the config file if it's missing (defaults from PLAN Part 10).
    let path = config_path();
    if path.exists() {
        println!("config already present at {}", path.display());
    } else {
        ensure_loopd_dir()?;
        let yaml = serde_yaml::to_string(&Config::default()).context("serializing config")?;
        std::fs::write(&path, yaml).with_context(|| format!("writing {}", path.display()))?;
        println!("created {}", path.display());
    }

    let config = Config::load()?;

    // 2. Agent diagnostics — is each configured agent's binary on PATH? (Uses the
    //    same preflight check `loop run` does.) Note which adapters are wired yet.
    println!("\nagents:");
    for (name, agent) in &config.agents {
        let implemented = adapter_for(name).is_some();
        let note = if implemented {
            ""
        } else {
            "  (adapter lands in a later phase)"
        };
        match find_on_path(&agent.cmd) {
            Some(p) => println!("  {name}: `{}` found at {}{note}", agent.cmd, p.display()),
            None => println!(
                "  {name}: `{}` NOT FOUND on PATH — install it to use `--agent {name}`{note}",
                agent.cmd
            ),
        }
    }

    // 3. Ensure the daemon is running and healthy.
    let client = DaemonClient::from_config(&config);
    client
        .ensure_running(&config)
        .context("starting the loopd daemon")?;
    println!(
        "\ndaemon healthy at http://127.0.0.1:{}",
        config.daemon.port
    );

    // 4. Mode-B hooks — available now (Phase 7). Don't auto-edit the user's CC
    //    settings; point them at the explicit opt-in command.
    println!(
        "\nMode-B observation (see `claude` sessions you start yourself) is opt-in:\n  \
         loop hooks install   # merges loopd's hooks into ~/.claude/settings.json"
    );
    println!("\nready — start a run with `loop run \"<task>\"`, then `loop ps` / `loop dash`.");
    Ok(())
}
