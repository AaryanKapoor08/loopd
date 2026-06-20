//! `loop hooks {install,remove,status}` — manage Claude Code hooks for Mode-B
//! observation. Stub until Phase 7, which installs hook entries into CC's
//! `settings.json` so user-started sessions appear in loopd read-only.

use anyhow::Result;
use clap::Subcommand;

/// `loop hooks <action>`.
#[derive(Subcommand, Debug)]
pub enum HooksAction {
    /// Install loopd's hooks into Claude Code's settings.
    Install,
    /// Remove loopd's hooks.
    Remove,
    /// Show whether loopd's hooks are installed.
    Status,
}

pub fn hooks(_action: HooksAction) -> Result<()> {
    println!("loop hooks: Mode-B observation (CC hooks) lands in Phase 7.");
    Ok(())
}
