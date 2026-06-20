//! `loop logs <id> [--follow]` — print a run's event log.
//!
//! Thin client: `GET /runs/:id/events` (newest-first from the store; we reverse
//! to chronological for display). `--follow` polls until the run reaches a
//! terminal state, printing only newly-arrived events each tick.

use std::time::Duration;

use anyhow::{bail, Result};
use clap::Args;

use crate::config::Config;
use crate::core::events::{LoopEvent, RunStatus};
use crate::daemon::client::DaemonClient;

/// Arguments for `loop logs`.
#[derive(Args, Debug)]
pub struct LogsArgs {
    /// The run id to show (from `loop ps`).
    pub id: String,

    /// Keep streaming new events until the run finishes.
    #[arg(long, short)]
    pub follow: bool,

    /// How many recent events to show (most-recent window).
    #[arg(long, default_value_t = 200)]
    pub limit: u32,
}

pub fn logs(args: LogsArgs) -> Result<()> {
    let config = Config::load()?;
    let client = DaemonClient::from_config(&config);
    client.ensure_running(&config)?;

    // Confirm the run exists up front so a typo'd id is a clear error.
    if client.get_run(&args.id)?.is_none() {
        bail!("no such run: {}", args.id);
    }

    if !args.follow {
        let events = chronological(client.events_for_run(&args.id, args.limit)?);
        for ev in &events {
            println!("{}", fmt_event(ev));
        }
        return Ok(());
    }

    // Follow mode: poll a generous window, print the tail beyond what we've shown,
    // and stop once the run is no longer live. Count-based dedup is reliable while
    // total events stay within the window (fine for tailing a live run).
    let window = args.limit.max(500);
    let mut printed = 0usize;
    loop {
        let events = chronological(client.events_for_run(&args.id, window)?);
        if events.len() > printed {
            for ev in &events[printed..] {
                println!("{}", fmt_event(ev));
            }
            printed = events.len();
        }

        match client.get_run(&args.id)? {
            Some(run) if is_live(run.status) => {}
            _ => break, // gone or terminal — final events are already printed.
        }
        std::thread::sleep(Duration::from_millis(700));
    }
    Ok(())
}

/// The store returns events newest-first; reverse to read top-to-bottom in time.
fn chronological(mut events: Vec<LoopEvent>) -> Vec<LoopEvent> {
    events.reverse();
    events
}

fn is_live(status: RunStatus) -> bool {
    matches!(status, RunStatus::Running | RunStatus::Paused)
}

/// One log line: `HH:MM:SS  kind  [tool]  text`.
fn fmt_event(ev: &LoopEvent) -> String {
    let time = fmt_time(ev.ts);
    let kind = kind_str(ev.kind);
    let mut line = format!("{time}  {kind:<11}");
    if let Some(tool) = &ev.tool {
        line.push_str(&format!("  {tool}"));
        if let Some(status) = ev.tool_status {
            line.push_str(&format!(" [{}]", tool_status_str(status)));
        }
    }
    if let Some(text) = &ev.text {
        let snippet = one_line(text, 160);
        if !snippet.is_empty() {
            line.push_str(&format!("  {snippet}"));
        }
    }
    line
}

fn fmt_time(ts_ms: i64) -> String {
    // Local-ish wall clock without pulling in a date crate: seconds-of-day.
    let secs = (ts_ms / 1000).rem_euclid(86_400);
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    format!("{h:02}:{m:02}:{s:02}")
}

fn one_line(text: &str, max: usize) -> String {
    let flat = text.replace('\n', " ").replace('\r', " ");
    let flat = flat.trim();
    if flat.chars().count() <= max {
        flat.to_string()
    } else {
        let cut: String = flat.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

fn kind_str(k: crate::core::events::EventKind) -> &'static str {
    use crate::core::events::EventKind::*;
    match k {
        RunStart => "run_start",
        RunEnd => "run_end",
        Assistant => "assistant",
        User => "user",
        ToolUse => "tool_use",
        ToolResult => "tool_result",
        Thinking => "thinking",
        Output => "output",
        Error => "error",
        TokenUsage => "token_usage",
        Stop => "stop",
    }
}

fn tool_status_str(s: crate::core::events::ToolStatus) -> &'static str {
    use crate::core::events::ToolStatus::*;
    match s {
        Ok => "ok",
        Error => "error",
        Denied => "denied",
        TimedOut => "timed_out",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::events::{EventKind, LoopEvent, Source};

    #[test]
    fn formats_a_tool_event_compactly() {
        let mut ev = LoopEvent::new("r", Source::Supervisor, EventKind::ToolUse);
        ev.tool = Some("bash".into());
        ev.text = Some("ls -la\nsecond line".into());
        let line = fmt_event(&ev);
        assert!(line.contains("tool_use"));
        assert!(line.contains("bash"));
        // newlines collapsed to one line.
        assert!(!line.contains('\n'));
    }

    #[test]
    fn chronological_reverses_newest_first() {
        let a = LoopEvent::new("r", Source::Supervisor, EventKind::RunStart);
        let b = LoopEvent::new("r", Source::Supervisor, EventKind::RunEnd);
        // store order is newest-first: [b, a]; chronological → [a, b].
        let got = chronological(vec![b.clone(), a.clone()]);
        assert_eq!(got[0].kind, EventKind::RunStart);
        assert_eq!(got[1].kind, EventKind::RunEnd);
    }
}
