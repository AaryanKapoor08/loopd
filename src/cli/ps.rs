//! `loop ps` — list runs and their live status as a table.
//!
//! Thin client: `GET /runs` and render. All metrics come from the daemon's
//! rolled-up `Run` rows; the CLI only formats them.

use anyhow::Result;

use crate::config::Config;
use crate::core::events::{now_ms, Run, RunStatus};
use crate::daemon::client::DaemonClient;

pub fn ps() -> Result<()> {
    let config = Config::load()?;
    let client = DaemonClient::from_config(&config);
    client.ensure_running(&config)?;

    let runs = client.list_runs()?;
    if runs.is_empty() {
        println!("no runs yet — start one with `loop run \"<task>\"`");
        return Ok(());
    }

    // Columns: id · label · agent · status · iter · elapsed · tokens(in/out) ·
    // cost · ctx% · flags · owned?
    let header = Row {
        id: "ID".into(),
        label: "LABEL".into(),
        agent: "AGENT".into(),
        status: "STATUS".into(),
        iter: "ITER".into(),
        elapsed: "ELAPSED".into(),
        tokens: "TOKENS(in/out)".into(),
        cost: "COST".into(),
        ctx: "CTX%".into(),
        flags: "FLAGS".into(),
        owned: "OWNED".into(),
    };
    let rows: Vec<Row> = runs.iter().map(Row::from_run).collect();

    // Width each column to its widest cell (header included).
    let all = std::iter::once(&header).chain(rows.iter());
    let w = Widths::measure(all);

    print_row(&header, &w);
    for r in &rows {
        print_row(r, &w);
    }
    Ok(())
}

/// One pre-formatted table line.
struct Row {
    id: String,
    label: String,
    agent: String,
    status: String,
    iter: String,
    elapsed: String,
    tokens: String,
    cost: String,
    ctx: String,
    flags: String,
    owned: String,
}

impl Row {
    fn from_run(run: &Run) -> Self {
        let end = run.ended_at.unwrap_or_else(now_ms);
        Row {
            id: run.run_id.clone(),
            label: truncate(&run.label, 24),
            agent: run.agent.clone(),
            status: status_str(run.status).to_string(),
            iter: run.iteration.to_string(),
            elapsed: fmt_elapsed(end.saturating_sub(run.started_at)),
            tokens: format!("{}/{}", human(run.tokens_in), human(run.tokens_out)),
            cost: format!("${:.4}", run.cost_usd),
            ctx: fmt_ctx_pct(run.context_tokens, run.context_window),
            flags: if run.flags.is_empty() {
                "-".into()
            } else {
                run.flags.join(",")
            },
            owned: if run.owned { "own" } else { "obs" }.into(),
        }
    }
}

/// Per-column widths.
struct Widths {
    id: usize,
    label: usize,
    agent: usize,
    status: usize,
    iter: usize,
    elapsed: usize,
    tokens: usize,
    cost: usize,
    ctx: usize,
    flags: usize,
    owned: usize,
}

impl Widths {
    fn measure<'a>(rows: impl Iterator<Item = &'a Row>) -> Self {
        let mut w = Widths {
            id: 0,
            label: 0,
            agent: 0,
            status: 0,
            iter: 0,
            elapsed: 0,
            tokens: 0,
            cost: 0,
            ctx: 0,
            flags: 0,
            owned: 0,
        };
        for r in rows {
            w.id = w.id.max(r.id.len());
            w.label = w.label.max(r.label.len());
            w.agent = w.agent.max(r.agent.len());
            w.status = w.status.max(r.status.len());
            w.iter = w.iter.max(r.iter.len());
            w.elapsed = w.elapsed.max(r.elapsed.len());
            w.tokens = w.tokens.max(r.tokens.len());
            w.cost = w.cost.max(r.cost.len());
            w.ctx = w.ctx.max(r.ctx.len());
            w.flags = w.flags.max(r.flags.len());
            w.owned = w.owned.max(r.owned.len());
        }
        w
    }
}

fn print_row(r: &Row, w: &Widths) {
    println!(
        "{:<id$}  {:<label$}  {:<agent$}  {:<status$}  {:>iter$}  {:>elapsed$}  {:>tokens$}  {:>cost$}  {:>ctx$}  {:<flags$}  {:<owned$}",
        r.id, r.label, r.agent, r.status, r.iter, r.elapsed, r.tokens, r.cost, r.ctx, r.flags, r.owned,
        id = w.id, label = w.label, agent = w.agent, status = w.status, iter = w.iter,
        elapsed = w.elapsed, tokens = w.tokens, cost = w.cost, ctx = w.ctx, flags = w.flags, owned = w.owned,
    );
}

fn status_str(s: RunStatus) -> &'static str {
    match s {
        RunStatus::Running => "running",
        RunStatus::Done => "done",
        RunStatus::Failed => "failed",
        RunStatus::Killed => "killed",
        RunStatus::Stuck => "stuck",
        RunStatus::Paused => "paused",
    }
}

/// `123`, `1.2k`, `3.4M` — compact token counts.
fn human(n: u32) -> String {
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
fn fmt_elapsed(ms: i64) -> String {
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

fn fmt_ctx_pct(used: u32, window: u32) -> String {
    if window == 0 {
        return "-".into();
    }
    format!("{:.0}%", (used as f64 / window as f64) * 100.0)
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max.saturating_sub(1)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
