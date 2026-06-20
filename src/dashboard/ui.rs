//! Pure rendering of the dashboard from `&App`. No I/O, no daemon calls — this
//! module only turns state into ratatui widgets so the draw path stays cheap and
//! testable.

use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, List, ListItem, Paragraph, Row, Table, TableState, Wrap};
use ratatui::Frame;

use crate::cli::fmt::{fmt_ctx_pct, fmt_elapsed, fmt_event, human, status_str, truncate};
use crate::config::{Caps, Config};
use crate::core::events::{now_ms, Run, RunStatus};

use super::app::{App, Mode, View};

/// Top-level draw: header bar, the active pane, then a status line — plus the
/// confirm-kill popup overlaid on top when one is staged.
pub fn draw(frame: &mut Frame, app: &App, config: &Config) {
    let chunks = Layout::vertical([
        Constraint::Length(1), // header
        Constraint::Min(0),    // body
        Constraint::Length(1), // status
    ])
    .split(frame.area());

    draw_header(frame, chunks[0], app);
    match app.view {
        View::List => draw_list(frame, chunks[1], app),
        View::Detail => draw_detail(frame, chunks[1], app, config),
    }
    draw_status(frame, chunks[2], app);

    if let Mode::ConfirmKill(id) = &app.mode {
        draw_confirm(frame, frame.area(), id);
    }
}

fn draw_header(frame: &mut Frame, area: Rect, app: &App) {
    let help = match app.view {
        View::List => "↑↓ select · enter detail · k kill · p pause · r retry · f filter · q quit",
        View::Detail => "esc back · k kill · p pause · r retry · q quit",
    };
    let mut title = format!("loopd dashboard — {} run(s)", app.visible_runs().len());
    if !app.filter.is_empty() {
        title.push_str(&format!(" · filter: {}", app.filter));
    }
    frame.render_widget(
        Paragraph::new(format!("{title}    {help}")).style(Style::new().bold()),
        area,
    );
}

// --- list view ---------------------------------------------------------------

fn draw_list(frame: &mut Frame, area: Rect, app: &App) {
    let header = Row::new([
        "LABEL", "AGENT", "STATUS", "ITER", "ELAPSED", "TOKENS", "COST", "CTX%", "FLAGS", "LAST",
        "OWN",
    ])
    .style(Style::new().bold().underlined());

    let now = now_ms();
    let visible = app.visible_runs();
    let rows: Vec<Row> = visible.iter().map(|r| run_row(r, now)).collect();

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
    if !visible.is_empty() {
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

// --- detail view -------------------------------------------------------------

fn draw_detail(frame: &mut Frame, area: Rect, app: &App, config: &Config) {
    let Some(run) = app.detail_run() else {
        frame.render_widget(
            Paragraph::new("run not found (it may have been removed)")
                .block(Block::default().borders(Borders::ALL).title("detail")),
            area,
        );
        return;
    };

    let chunks = Layout::vertical([Constraint::Length(9), Constraint::Min(0)]).split(area);
    draw_detail_info(frame, chunks[0], run, &config.defaults.caps);
    draw_detail_events(frame, chunks[1], app);
}

fn draw_detail_info(frame: &mut Frame, area: Rect, run: &Run, caps: &Caps) {
    let now = now_ms();
    let end = run.ended_at.unwrap_or(now);
    let elapsed_min = (end.saturating_sub(run.started_at) / 60_000) as u32;

    let mut lines = vec![
        Line::from(vec![
            Span::raw("label:  "),
            Span::styled(run.label.clone(), Style::new().bold()),
            Span::raw(if run.owned { "   (owned)" } else { "   (observed)" }),
        ]),
        Line::from(vec![
            Span::raw("agent:  "),
            Span::raw(run.agent.clone()),
            Span::raw("   status: "),
            Span::styled(status_str(run.status), Style::new().fg(status_color(run.status))),
            Span::raw(format!("   model: {}", run.model.as_deref().unwrap_or("-"))),
        ]),
        Line::from(format!("prompt: {}", run.prompt)),
        Line::from(vec![
            Span::raw("caps:   "),
            cap_span(format!("iter {}/{}", run.iteration, caps.max_iterations), run.iteration as f64, caps.max_iterations as f64),
            Span::raw("   "),
            cap_span(format!("cost ${:.4}/${:.2}", run.cost_usd, caps.max_cost_usd), run.cost_usd, caps.max_cost_usd),
            Span::raw("   "),
            cap_span(format!("dur {elapsed_min}m/{}m", caps.max_duration_min), elapsed_min as f64, caps.max_duration_min as f64),
            Span::raw(format!("   ctx {}", fmt_ctx_pct(run.context_tokens, run.context_window))),
        ]),
        Line::from(Span::styled(
            "(caps are config defaults — recorded but not enforced until the governance engine, Phase 6)",
            Style::new().fg(Color::DarkGray),
        )),
    ];
    lines.push(Line::from(""));

    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .block(Block::default().borders(Borders::ALL).title(format!("run {}", run.run_id))),
        area,
    );
}

/// A cap value, colored red once usage meets/exceeds the threshold.
fn cap_span(text: String, used: f64, limit: f64) -> Span<'static> {
    if limit > 0.0 && used >= limit {
        Span::styled(text, Style::new().fg(Color::Red).bold())
    } else {
        Span::raw(text)
    }
}

fn draw_detail_events(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title("events (newest at bottom)");
    // Show the tail that fits inside the borders.
    let inner = area.height.saturating_sub(2) as usize;
    let start = app.detail_events.len().saturating_sub(inner);
    let items: Vec<ListItem> = app.detail_events[start..]
        .iter()
        .map(|ev| ListItem::new(fmt_event(ev)))
        .collect();
    frame.render_widget(List::new(items).block(block), area);
}

// --- status + overlays -------------------------------------------------------

fn draw_status(frame: &mut Frame, area: Rect, app: &App) {
    let (text, style) = match &app.mode {
        Mode::Filtering => (
            format!("filter> {}_", app.filter),
            Style::new().fg(Color::Yellow),
        ),
        _ if !app.daemon_ok => (
            format!("⚠ {}", app.status),
            Style::new().fg(Color::Red).bold(),
        ),
        _ => (app.status.clone(), Style::new().fg(Color::DarkGray)),
    };
    frame.render_widget(Paragraph::new(text).style(style), area);
}

fn draw_confirm(frame: &mut Frame, area: Rect, id: &str) {
    let popup = centered_rect(60, 30, area);
    frame.render_widget(Clear, popup);
    let body = Paragraph::new(vec![
        Line::from(""),
        Line::from(format!("Kill run {id}?")),
        Line::from(""),
        Line::from("y = yes, stop it    n = cancel"),
    ])
    .alignment(Alignment::Center)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title("confirm kill")
            .border_style(Style::new().fg(Color::Red).bold()),
    );
    frame.render_widget(body, popup);
}

/// A rectangle centered in `area`, sized as a percentage of it.
fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ])
    .split(area);
    Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .split(vertical[1])[1]
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
    use crate::core::events::{EventKind, LoopEvent, Source};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// Flatten the rendered buffer into a single string for `contains` asserts.
    fn rendered(app: &App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(120, 20)).unwrap();
        terminal.draw(|f| draw(f, app, &Config::default())).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    fn sample_run() -> Run {
        let mut run = Run::new("run_abc");
        run.label = "fix the parser".into();
        run.agent = "claude".into();
        run.status = RunStatus::Running;
        run.iteration = 3;
        run.prompt = "make the tests pass".into();
        run.owned = true;
        run
    }

    #[test]
    fn renders_header_and_a_run_row() {
        let mut app = App::new();
        app.runs.push(sample_run());
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

    #[test]
    fn detail_view_shows_prompt_caps_and_events() {
        let mut app = App::new();
        app.runs.push(sample_run());
        app.view = View::Detail;
        app.detail_run_id = Some("run_abc".into());
        let mut ev = LoopEvent::new("run_abc", Source::Supervisor, EventKind::ToolUse);
        ev.tool = Some("bash".into());
        app.detail_events.push(ev);

        let out = rendered(&app);
        assert!(out.contains("make the tests pass")); // prompt
        assert!(out.contains("iter 3/50")); // caps progress vs config default
        assert!(out.contains("tool_use")); // event stream
    }

    #[test]
    fn confirm_popup_names_the_run() {
        let mut app = App::new();
        app.runs.push(sample_run());
        app.mode = Mode::ConfirmKill("run_abc".into());
        let out = rendered(&app);
        assert!(out.contains("confirm kill"));
        assert!(out.contains("Kill run run_abc?"));
    }

    #[test]
    fn filter_hides_non_matching_runs() {
        let mut app = App::new();
        app.runs.push(sample_run());
        let mut other = Run::new("run_xyz");
        other.label = "write docs".into();
        other.agent = "codex".into();
        app.runs.push(other);
        app.filter = "parser".into();

        let out = rendered(&app);
        assert!(out.contains("fix the parser"));
        assert!(!out.contains("write docs"));
        assert!(out.contains("1 run(s)"));
    }
}
