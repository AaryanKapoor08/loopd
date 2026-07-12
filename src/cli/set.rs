//! `loop set <key> <value>` — edit `~/.loopd/config.yaml` on disk.
//!
//! Unlike the run/ps/kill/logs commands, `set`/`policy` operate on the config
//! file directly (there is no daemon write path for config yet). The daemon
//! re-reads config on its next start, so a note reminds the user to restart.
//! A config-reload route can come later — not built here.
//!
//! Keys are dotted paths into the YAML using its camelCase field names, e.g.
//! `daemon.port`, `defaults.agent`, `defaults.caps.maxCostUsd`. The edit is
//! type-checked and validated (by round-tripping through [`Config`]) before it
//! is written, so a bad key or wrong-typed value fails without corrupting the file.

use anyhow::{anyhow, bail, Context, Result};
use clap::Args;
use serde_yaml::Value;

use crate::config::{config_path, ensure_loopd_dir, Config};

/// Arguments for `loop set`.
#[derive(Args, Debug)]
pub struct SetArgs {
    /// Dotted config key (camelCase), e.g. `defaults.caps.maxCostUsd`.
    pub key: String,
    /// New value. Parsed as YAML scalar: `5` → number, `true` → bool, else string.
    pub value: String,
}

pub fn set(args: SetArgs) -> Result<()> {
    edit_config(&[(args.key.clone(), args.value.clone())])?;
    println!("set {} = {}", args.key, args.value);
    println!("{}", apply_note());
    Ok(())
}

/// Apply a batch of dotted-key updates to the on-disk config and write it back.
/// Used by both `loop set` and `loop policy`. Loads the merged config (defaults +
/// file) so every section exists, applies each update onto the full tree,
/// re-validates by deserializing into [`Config`], then persists. Returns the new
/// config so callers can echo it.
pub fn edit_config(updates: &[(String, String)]) -> Result<Config> {
    let config = Config::load()?;
    let mut tree = serde_yaml::to_value(&config).context("serializing current config")?;

    for (key, raw) in updates {
        let parts: Vec<&str> = key.split('.').collect();
        if parts.iter().any(|p| p.is_empty()) {
            bail!("malformed config key `{key}`");
        }
        set_path(&mut tree, &parts, parse_scalar(raw), key)?;
    }

    let new_config: Config = serde_yaml::from_value(tree).context(
        "that change produced an invalid config — check the key's expected type \
         (e.g. maxCostUsd is a number, onTrip is warn/notify/pause/kill)",
    )?;
    new_config.validate()?;
    write_config(&new_config)?;
    Ok(new_config)
}

/// The reminder printed after any config edit.
pub fn apply_note() -> &'static str {
    "restart the daemon to apply: `loop daemon stop` then `loop daemon start` \
     (config is read at startup)"
}

/// Parse a CLI value string as a YAML scalar so numbers/bools keep their type,
/// falling back to a plain string (so `npm test`, `claude`, `warn` work).
fn parse_scalar(raw: &str) -> Value {
    serde_yaml::from_str::<Value>(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

/// Replace the leaf at `parts` within `node`. Every intermediate key must already
/// exist (we start from a full config), so an unknown segment is a clear error
/// rather than silently creating a junk key.
fn set_path(node: &mut Value, parts: &[&str], leaf: Value, full_key: &str) -> Result<()> {
    let map = node
        .as_mapping_mut()
        .ok_or_else(|| anyhow!("`{full_key}` does not address a config section"))?;
    let head = Value::String(parts[0].to_string());
    if parts.len() == 1 {
        if !map.contains_key(&head) {
            bail!("unknown config key `{full_key}`");
        }
        map.insert(head, leaf);
        Ok(())
    } else {
        let child = map
            .get_mut(&head)
            .ok_or_else(|| anyhow!("unknown config key `{full_key}`"))?;
        set_path(child, &parts[1..], leaf, full_key)
    }
}

fn write_config(config: &Config) -> Result<()> {
    ensure_loopd_dir()?;
    let path = config_path();
    let yaml = serde_yaml::to_string(config).context("serializing config")?;
    std::fs::write(&path, yaml).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_tree() -> Value {
        serde_yaml::to_value(Config::default()).unwrap()
    }

    #[test]
    fn parse_scalar_keeps_types() {
        assert!(parse_scalar("5").is_number());
        assert!(parse_scalar("2.5").is_number());
        assert!(parse_scalar("true").is_bool());
        assert_eq!(parse_scalar("claude"), Value::String("claude".into()));
        assert_eq!(parse_scalar("npm test"), Value::String("npm test".into()));
    }

    #[test]
    fn set_path_updates_a_nested_leaf() {
        let mut tree = full_tree();
        set_path(
            &mut tree,
            &["defaults", "caps", "maxCostUsd"],
            parse_scalar("5"),
            "defaults.caps.maxCostUsd",
        )
        .unwrap();
        // Round-trips into a valid Config with the new value.
        let cfg: Config = serde_yaml::from_value(tree).unwrap();
        assert_eq!(cfg.defaults.caps.max_cost_usd, 5.0);
    }

    #[test]
    fn set_path_rejects_unknown_key() {
        let mut tree = full_tree();
        let err = set_path(
            &mut tree,
            &["daemon", "bogus"],
            parse_scalar("1"),
            "daemon.bogus",
        )
        .expect_err("unknown key must error");
        assert!(err.to_string().contains("unknown config key"));
    }

    #[test]
    fn wrong_type_round_trips_into_an_error() {
        // maxCostUsd is f64; a string value must fail the deserialize, not write.
        let mut tree = full_tree();
        set_path(
            &mut tree,
            &["defaults", "caps", "maxCostUsd"],
            parse_scalar("not-a-number"),
            "defaults.caps.maxCostUsd",
        )
        .unwrap();
        let res: Result<Config, _> = serde_yaml::from_value(tree);
        assert!(res.is_err(), "string into a numeric cap must fail");
    }
}
