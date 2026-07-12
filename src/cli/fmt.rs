//! Shared formatters for the CLI and the TUI.
//!
//! These are the small, pure rendering helpers that both `loop ps`/`loop logs`
//! (the Phase-4 CLI tables/logs) and `loop dash` (the Phase-5 ratatui cockpit)
//! need. Defining them once here keeps the table column, the log line, and the
//! dashboard cell all formatting a `Run`/`LoopEvent` identically — there is no
//! second copy to drift.
//!
//! Two groups:
//! - **Run formatters** (`human`, `fmt_elapsed`, `fmt_ctx_pct`, `status_str`,
//!   `truncate`) — used by the `ps` table and the dashboard list row.
//! - **Event formatters** (`chronological`, `is_live`, `fmt_event`, `fmt_time`,
//!   `one_line`, `kind_str`, `tool_status_str`) — used by `logs` and the
//!   dashboard detail pane.

use crate::core::events::{EventKind, LoopEvent, RunStatus, ToolStatus};

// --- run formatters ----------------------------------------------------------

/// `123`, `1.2k`, `3.4M` — compact token counts.
pub fn human(n: u32) -> String {
    let n = n as f64;
    if n < 1_000.0 {
        format!("{}", n as u32)
    } else if n < 1_000_000.0 {
        format!("{:.1}k", n / 1_000.0)
    } else {
        format!("{:.1}M", n / 1_000_000.0)
    }
}

/// Elapsed wall-clock from milliseconds → `12s`, `3m04s`, `1h02m`.
pub fn fmt_elapsed(ms: i64) -> String {
    let secs = (ms / 1000).max(0);
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}h{m:02}m")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

/// Context-window usage as a whole-number percent (`25%`), or `-` when the
/// window is unknown (zero).
pub fn fmt_ctx_pct(used: u32, window: u32) -> String {
    if window == 0 {
        return "-".into();
    }
    format!("{:.0}%", (used as f64 / window as f64) * 100.0)
}

/// The lowercase wire word for a run status (`running`, `done`, …).
pub fn status_str(s: RunStatus) -> &'static str {
    match s {
        RunStatus::Running => "running",
        RunStatus::Done => "done",
        RunStatus::Failed => "failed",
        RunStatus::Killed => "killed",
        RunStatus::Stuck => "stuck",
        RunStatus::Paused => "paused",
    }
}

/// Reset ANSI styling.
pub const ANSI_RESET: &str = "\x1b[0m";
/// Yellow — used for raised flags in the `ps` table.
pub const ANSI_YELLOW: &str = "\x1b[33m";

/// The ANSI color code for a run status — the same at-a-glance health cue the
/// TUI's `status_color` gives, for plain-terminal output (`loop ps`).
pub fn status_ansi(s: RunStatus) -> &'static str {
    match s {
        RunStatus::Running => "\x1b[32m", // green
        RunStatus::Done => "\x1b[90m",    // dim gray
        RunStatus::Failed => "\x1b[31m",  // red
        RunStatus::Killed => "\x1b[35m",  // magenta
        RunStatus::Stuck => "\x1b[33m",   // yellow
        RunStatus::Paused => "\x1b[36m",  // cyan
    }
}

/// Truncate `s` to at most `max` characters, appending `…` when it had to cut.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

// --- event formatters --------------------------------------------------------

/// The store returns events newest-first; reverse to read top-to-bottom in time.
pub fn chronological(mut events: Vec<LoopEvent>) -> Vec<LoopEvent> {
    events.reverse();
    events
}

/// Whether a run is in a live (non-terminal) state.
pub fn is_live(status: RunStatus) -> bool {
    matches!(status, RunStatus::Running | RunStatus::Paused)
}

/// One log line: `HH:MM:SS  kind  [tool]  text`.
pub fn fmt_event(ev: &LoopEvent) -> String {
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

/// Wall-clock `HH:MM:SS` from epoch ms, without pulling in a date crate
/// (seconds-of-day).
pub fn fmt_time(ts_ms: i64) -> String {
    let secs = (ts_ms / 1000).rem_euclid(86_400);
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    format!("{h:02}:{m:02}:{s:02}")
}

/// Flatten `text` to a single line and clip to `max` chars with an ellipsis.
pub fn one_line(text: &str, max: usize) -> String {
    let flat = text.replace(['\n', '\r'], " ");
    let flat = flat.trim();
    if flat.chars().count() <= max {
        flat.to_string()
    } else {
        let cut: String = flat.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

/// The lowercase wire word for an event kind.
pub fn kind_str(k: EventKind) -> &'static str {
    use EventKind::*;
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

/// The lowercase wire word for a tool outcome.
pub fn tool_status_str(s: ToolStatus) -> &'static str {
    use ToolStatus::*;
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
    use crate::core::events::Source;

    #[test]
    fn human_is_compact() {
        assert_eq!(human(0), "0");
        assert_eq!(human(950), "950");
        assert_eq!(human(1_500), "1.5k");
        assert_eq!(human(2_400_000), "2.4M");
    }

    #[test]
    fn elapsed_formats_by_magnitude() {
        assert_eq!(fmt_elapsed(12_000), "12s");
        assert_eq!(fmt_elapsed(184_000), "3m04s");
        assert_eq!(fmt_elapsed(3_720_000), "1h02m");
        assert_eq!(fmt_elapsed(-5), "0s");
    }

    #[test]
    fn ctx_pct_guards_zero_window() {
        assert_eq!(fmt_ctx_pct(0, 0), "-");
        assert_eq!(fmt_ctx_pct(50_000, 200_000), "25%");
    }

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
