//! `loop hooks {install,remove,status}` — manage Claude Code hooks for Mode-B
//! observation. Installing merges loopd's `loop ingest` hook into the user's
//! `~/.claude/settings.json` for `PostToolUse`/`Stop`/`SessionStart`, so a
//! user-started `claude` session POSTs liveness to the daemon and shows up in the
//! cockpit read-only. The merge is **non-destructive**: existing hooks (the
//! user's own, other tools') are preserved, and `remove` takes out only loopd's
//! entries. Completes the Phase-4 `loop init` hook-install stub.
//!
//! Shape we merge into (verified against a real `settings.json` + vibe-kanban
//! `get_hooks`): `hooks.{Event}[] = { matcher?, hooks: [{ type, command }] }`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Subcommand;
use serde_json::{json, Value};

/// The hook events loopd installs into. `PostToolUse`/`Stop` are liveness;
/// `SessionEnd` closes the observed run the moment the session exits (else it
/// would sit `Running` until the idle-timeout sweep). The canonical token rollup
/// comes from the transcript tailer, so these four are enough to make a session
/// appear, stay live, and end.
const EVENTS: [&str; 4] = ["SessionStart", "PostToolUse", "Stop", "SessionEnd"];

/// `loop hooks <action>`.
#[derive(Subcommand, Debug)]
pub enum HooksAction {
    /// Install loopd's hooks into Claude Code's settings (non-destructive merge).
    Install,
    /// Remove loopd's hooks (leaves every other hook untouched).
    Remove,
    /// Show whether loopd's hooks are installed.
    Status,
}

pub fn hooks(action: HooksAction) -> Result<()> {
    let path = settings_path();
    match action {
        HooksAction::Install => {
            let cmd = ingest_command()?;
            let installed = install_into(&path, &cmd)?;
            if installed.is_empty() {
                println!("loopd hooks already installed in {}", path.display());
            } else {
                println!(
                    "installed loopd hooks for [{}] in {}",
                    installed.join(", "),
                    path.display()
                );
                println!("  start `claude` in any terminal — it now shows up in `loop dash` (observed).");
            }
        }
        HooksAction::Remove => {
            let removed = remove_from(&path)?;
            if removed.is_empty() {
                println!("no loopd hooks found in {}", path.display());
            } else {
                println!("removed loopd hooks for [{}] from {}", removed.join(", "), path.display());
            }
        }
        HooksAction::Status => {
            let status = status_of(&path)?;
            println!("loopd hooks in {}:", path.display());
            for (event, present) in status {
                println!("  {event:<13} {}", if present { "installed" } else { "—" });
            }
        }
    }
    Ok(())
}

/// `~/.claude/settings.json` — the user-level CC settings (where hooks merge).
fn settings_path() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    home.join(".claude").join("settings.json")
}

/// The hook command CC runs: this very binary, `ingest`. Quoted so a path with
/// spaces survives the shell CC invokes it through.
fn ingest_command() -> Result<String> {
    let exe = std::env::current_exe().context("resolving the loopd executable path")?;
    Ok(format!("\"{}\" ingest", exe.display()))
}

/// Is this hook command one of loopd's `… ingest` invocations? Matches by the
/// trailing `ingest` token plus a `loop` reference, so it survives the exe path
/// moving without matching unrelated hooks.
fn is_loopd_ingest(cmd: &str) -> bool {
    let lc = cmd.to_lowercase();
    lc.contains("loop") && lc.split_whitespace().last() == Some("ingest")
}

/// Does a `hooks.{Event}[]` entry contain a loopd ingest command?
fn entry_is_loopd(entry: &Value) -> bool {
    entry
        .get("hooks")
        .and_then(|h| h.as_array())
        .map(|hs| {
            hs.iter().any(|h| {
                h.get("command")
                    .and_then(|c| c.as_str())
                    .map(is_loopd_ingest)
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

/// Merge loopd's hook into each target event if absent. Returns the events newly
/// installed (empty = already present). Writes only when something changed.
fn install_into(path: &Path, cmd: &str) -> Result<Vec<String>> {
    let mut settings = load_settings(path)?;
    let obj = settings
        .as_object_mut()
        .context("~/.claude/settings.json is not a JSON object")?;
    let hooks = obj
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("settings.json `hooks` is not an object")?;

    let mut installed = Vec::new();
    for event in EVENTS {
        let arr = hooks
            .entry(event)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .with_context(|| format!("settings.json `hooks.{event}` is not an array"))?;
        if !arr.iter().any(entry_is_loopd) {
            arr.push(json!({ "hooks": [ { "type": "command", "command": cmd } ] }));
            installed.push(event.to_string());
        }
    }

    if !installed.is_empty() {
        write_settings(path, &settings)?;
    }
    Ok(installed)
}

/// Remove loopd's hook entries from every event, pruning empties. Returns the
/// events changed. Leaves all non-loopd hooks intact.
fn remove_from(path: &Path) -> Result<Vec<String>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let mut settings = load_settings(path)?;
    let Some(obj) = settings.as_object_mut() else {
        return Ok(Vec::new());
    };

    let mut removed = Vec::new();
    let mut prune_hooks = false;
    if let Some(hooks) = obj.get_mut("hooks").and_then(|h| h.as_object_mut()) {
        for event in EVENTS {
            if let Some(arr) = hooks.get_mut(event).and_then(|a| a.as_array_mut()) {
                let before = arr.len();
                arr.retain(|entry| !entry_is_loopd(entry));
                if arr.len() != before {
                    removed.push(event.to_string());
                }
            }
        }
        // Drop event arrays we emptied, then the `hooks` object if it's now empty.
        hooks.retain(|_, v| !v.as_array().map(|a| a.is_empty()).unwrap_or(false));
        prune_hooks = hooks.is_empty();
    }
    if prune_hooks {
        obj.remove("hooks");
    }

    if !removed.is_empty() {
        write_settings(path, &settings)?;
    }
    Ok(removed)
}

/// Per-event install status, in [`EVENTS`] order.
fn status_of(path: &Path) -> Result<Vec<(String, bool)>> {
    let settings = load_settings(path)?;
    let hooks = settings.get("hooks").and_then(|h| h.as_object());
    Ok(EVENTS
        .iter()
        .map(|event| {
            let present = hooks
                .and_then(|h| h.get(*event))
                .and_then(|a| a.as_array())
                .map(|arr| arr.iter().any(entry_is_loopd))
                .unwrap_or(false);
            (event.to_string(), present)
        })
        .collect())
}

/// Load settings.json, treating a missing/empty file as `{}`.
fn load_settings(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(json!({}));
    }
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    if text.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// Write settings.json back: snapshot a `.loopd.bak` first, then write a temp
/// sibling and atomically rename it over the original. `preserve_order` (serde_json
/// feature) keeps the user's key order, so the diff is limited to our change.
fn write_settings(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    if path.exists() {
        let _ = std::fs::copy(path, sibling(path, ".loopd.bak"));
    }
    let body = serde_json::to_string_pretty(value).context("serializing settings.json")? + "\n";
    let tmp = sibling(path, ".loopd.tmp");
    std::fs::write(&tmp, body).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

/// `path` with `suffix` appended to its filename (e.g. `…/settings.json.loopd.bak`).
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut s: OsString = path.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_settings(body: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "loopd_hooks_{}",
            crate::core::events::new_run_id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        if !body.is_empty() {
            std::fs::write(&path, body).unwrap();
        }
        path
    }

    const CMD: &str = "\"C:\\loopd.exe\" ingest";

    // A real-shaped settings.json that already has a non-loopd SessionStart hook.
    const EXISTING: &str = r#"{
  "model": "opus",
  "hooks": {
    "SessionStart": [
      { "hooks": [ { "type": "command", "command": "node heal.mjs" } ] }
    ]
  },
  "theme": "dark"
}"#;

    #[test]
    fn install_is_non_destructive_and_idempotent() {
        let path = temp_settings(EXISTING);
        let installed = install_into(&path, CMD).unwrap();
        assert_eq!(installed.len(), EVENTS.len(), "every event installed");

        let v = load_settings(&path).unwrap();
        // The user's own settings survive untouched.
        assert_eq!(v["model"], "opus");
        assert_eq!(v["theme"], "dark");
        // The pre-existing SessionStart hook is preserved alongside ours.
        let ss = v["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(ss.len(), 2, "existing + loopd entry");
        assert!(ss.iter().any(|e| e["hooks"][0]["command"] == "node heal.mjs"));
        assert!(ss.iter().any(entry_is_loopd));

        // Re-install is a no-op (no duplicate entry).
        let again = install_into(&path, CMD).unwrap();
        assert!(again.is_empty());
        let v = load_settings(&path).unwrap();
        assert_eq!(v["hooks"]["SessionStart"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn status_reflects_install_then_remove() {
        let path = temp_settings(EXISTING);
        assert!(status_of(&path).unwrap().iter().all(|(_, p)| !p));

        install_into(&path, CMD).unwrap();
        assert!(status_of(&path).unwrap().iter().all(|(_, p)| *p));

        let removed = remove_from(&path).unwrap();
        assert_eq!(removed.len(), EVENTS.len());
        assert!(status_of(&path).unwrap().iter().all(|(_, p)| !p));

        // Remove preserved the user's own SessionStart hook (not a loopd one).
        let v = load_settings(&path).unwrap();
        let ss = v["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(ss.len(), 1);
        assert_eq!(ss[0]["hooks"][0]["command"], "node heal.mjs");
        // PostToolUse/Stop arrays we created and emptied are pruned away.
        assert!(v["hooks"].get("PostToolUse").is_none());
        assert!(v["hooks"].get("Stop").is_none());
    }

    #[test]
    fn install_into_a_missing_file_creates_it() {
        let path = temp_settings(""); // no file written
        assert!(!path.exists());
        let installed = install_into(&path, CMD).unwrap();
        assert_eq!(installed.len(), EVENTS.len());
        assert!(path.exists());
        let v = load_settings(&path).unwrap();
        assert!(v["hooks"]["PostToolUse"].as_array().unwrap().iter().any(entry_is_loopd));
    }

    #[test]
    fn remove_with_no_loopd_hooks_is_a_noop() {
        let path = temp_settings(EXISTING);
        let removed = remove_from(&path).unwrap();
        assert!(removed.is_empty());
        // The user's settings are unchanged.
        let v = load_settings(&path).unwrap();
        assert_eq!(v["hooks"]["SessionStart"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn loopd_ingest_matcher_is_specific() {
        assert!(is_loopd_ingest("\"C:\\path\\loopd.exe\" ingest"));
        assert!(is_loopd_ingest("/usr/local/bin/loop ingest"));
        assert!(!is_loopd_ingest("node heal.mjs"));
        assert!(!is_loopd_ingest("\"C:\\loopd.exe\" daemon serve"));
    }
}
