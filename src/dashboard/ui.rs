//! Pure rendering of the dashboard from `&App`. No I/O, no daemon calls — this
//! module only turns state into ratatui widgets so the draw path stays cheap and
//! testable.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState};
use ratatui::Frame;

use crate::cli::fmt::{fmt_ctx_pct, fmt_elapsed, human, status_str, truncate};
use crate::config::Config;
use crate::core::events::{now_ms, Run, RunStatus};

use super::app::App;

/// Top-level draw: header bar, the run list, then a status line.
pub fn draw(frame: &mut Frame, app: &App, _config: &Config) {
    let chunks = Layout::vertical([
        Constraint::Length(1), // header
        Constraint::Min(0),    // list
        Constraint::Length(1), // status
    ])
    .split(frame.area());

    draw_header(frame, chunks[0], app);
    draw_list(frame, chunks[1], app);
    draw_status(frame, chunks[2], app);
}

fn draw_header(frame: &mut Frame, area: Rect, app: &App) {
    let title = format!("loopd dashboard — {} run(s)", app.runs.len());
    let help = "↑↓ select · enter detail · k kill · p pause · r retry · f filter · q quit";
    let line = format!("{title}    {help}");
    frame.render_widget(
        Paragraph::new(line).style(Style::new().bold()),
        area,
    );
}

fn draw_list(frame: &mut Frame, area: Rect, app: &App) {
    let header = Row::new([
        "LABEL", "AGENT", "STATUS", "ITER", "ELAPSED", "TOKENS", "COST", "CTX%", "FLAGS", "LAST",
        "OWN",
    ])
    .style(Style::new().bold().underlined());

    let now = now_ms();
    let rows: Vec<Row> = app.runs.iter().map(|r| run_row(r, now)).collect();

    let widths = [
        Constraint::Min(10),    // label
        Constraint::Length(8),  // agent
        Constraint::Length(8),  // status
        Constraint::Length(5),  // iter
        Constraint::Length(8),  // elapsed
        Constraint::Length(13), // tokens
        Constraint::Length(9),  // cost
        Constraint::Length(6),  // ctx%
        Constraint::Min(8),     // flags
        Constraint::Length(8),  // last
        Constraint::Length(4),  // owned
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .highlight_style(Style::new().reversed())
        .highlight_symbol("▌ ")
        .block(Block::default().borders(Borders::ALL).title("runs"));

    let mut state = TableState::default();
    if !app.runs.is_empty() {
        state.select(Some(app.selected));
    }
    frame.render_stateful_widget(table, area, &mut state);
}

fn run_row(r: &Run, now: i64) -> Row<'static> {
    let end = r.ended_at.unwrap_or(now);
    let cells = vec![
        Cell::from(truncate(&r.label, 24)),
        Cell::from(r.agent.clone()),
        Cell::from(status_str(r.status).to_string()).style(Style::new().fg(status_color(r.status))),
        Cell::from(r.iteration.to_string()),
        Cell::from(fmt_elapsed(end.saturating_sub(r.started_at))),
        Cell::from(format!("{}/{}", human(r.tokens_in), human(r.tokens_out))),
        Cell::from(format!("${:.4}", r.cost_usd)),
        Cell::from(fmt_ctx_pct(r.context_tokens, r.context_window)),
        Cell::from(if r.flags.is_empty() {
            "-".to_string()
        } else {
            r.flags.join(",")
        }),
        Cell::from(fmt_elapsed(now.saturating_sub(r.last_event_at))),
        Cell::from(if r.owned { "own" } else { "ro" }),
    ];
    Row::new(cells)
}

fn draw_status(frame: &mut Frame, area: Rect, app: &App) {
    let (text, style) = if app.daemon_ok {
        (app.status.clone(), Style::new().fg(Color::DarkGray))
    } else {
        (
            format!("⚠ {}", app.status),
            Style::new().fg(Color::Red).bold(),
        )
    };
    frame.render_widget(Paragraph::new(text).style(style), area);
}

/// Row color by lifecycle state — the at-a-glance "is this loop healthy?" cue.
fn status_color(s: RunStatus) -> Color {
    match s {
        RunStatus::Running => Color::Green,
        RunStatus::Done => Color::Gray,
        RunStatus::Failed => Color::Red,
        RunStatus::Killed => Color::Magenta,
        RunStatus::Stuck => Color::Yellow,
        RunStatus::Paused => Color::Cyan,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// Flatten the rendered buffer into a single string for `contains` asserts.
    fn rendered(app: &App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(120, 12)).unwrap();
        terminal
            .draw(|f| draw(f, app, &Config::default()))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    #[test]
    fn renders_header_and_a_run_row() {
        let mut app = App::new();
        let mut run = Run::new("run_abc");
        run.label = "fix the parser".into();
        run.agent = "claude".into();
        run.status = RunStatus::Running;
        run.iteration = 3;
        app.runs.push(run);

        let out = rendered(&app);
        assert!(out.contains("loopd dashboard"));
        assert!(out.contains("fix the parser"));
        assert!(out.contains("claude"));
        assert!(out.contains("running"));
    }

    #[test]
    fn shows_empty_state_when_no_runs() {
        let mut app = App::new();
        app.status = "no runs yet — start one with `loop run \"<task>\"`".into();
        let out = rendered(&app);
        assert!(out.contains("0 run(s)"));
        assert!(out.contains("no runs yet"));
    }
}
