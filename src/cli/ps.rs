//! `loop ps` — list runs and their live status as a table.
//!
//! Thin client: `GET /runs` and render. All metrics come from the daemon's
//! rolled-up `Run` rows; the CLI only formats them.

use std::io::IsTerminal;

use anyhow::Result;

use crate::cli::fmt::{
    fmt_ctx_pct, fmt_elapsed, human, status_ansi, status_str, truncate, ANSI_RESET, ANSI_YELLOW,
};
use crate::config::Config;
use crate::core::events::{now_ms, Run};
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

    // Color the health-signal columns only on a real terminal, and honor the
    // NO_COLOR convention (https://no-color.org). Widths are measured on the
    // plain strings, so alignment never depends on escape codes.
    let color = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();

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
        status_color: "",
        flags_color: "",
    };
    let rows: Vec<Row> = runs.iter().map(Row::from_run).collect();

    // Width each column to its widest cell (header included).
    let all = std::iter::once(&header).chain(rows.iter());
    let w = Widths::measure(all);

    print_row(&header, &w, false);
    for r in &rows {
        print_row(r, &w, color);
    }
    Ok(())
}

/// One pre-formatted table line, plus the ANSI codes its health cells paint
/// with (empty = plain; the header row stays plain).
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
    status_color: &'static str,
    flags_color: &'static str,
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
            status_color: status_ansi(run.status),
            flags_color: if run.flags.is_empty() {
                ""
            } else {
                ANSI_YELLOW
            },
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

fn print_row(r: &Row, w: &Widths, color: bool) {
    // Pad first, then wrap in color — escape codes have zero display width but
    // would confuse `format!`'s width specifiers if embedded before padding.
    let status = paint(
        format!("{:<w$}", r.status, w = w.status),
        r.status_color,
        color,
    );
    let flags = paint(
        format!("{:<w$}", r.flags, w = w.flags),
        r.flags_color,
        color,
    );
    println!(
        "{:<id$}  {:<label$}  {:<agent$}  {status}  {:>iter$}  {:>elapsed$}  {:>tokens$}  {:>cost$}  {:>ctx$}  {flags}  {:<owned$}",
        r.id, r.label, r.agent, r.iter, r.elapsed, r.tokens, r.cost, r.ctx, r.owned,
        id = w.id, label = w.label, agent = w.agent, iter = w.iter,
        elapsed = w.elapsed, tokens = w.tokens, cost = w.cost, ctx = w.ctx, owned = w.owned,
    );
}

/// Wrap an already-padded cell in an ANSI color, when colors are on and the
/// cell has one.
fn paint(cell: String, code: &str, color: bool) -> String {
    if color && !code.is_empty() {
        format!("{code}{cell}{ANSI_RESET}")
    } else {
        cell
    }
}
