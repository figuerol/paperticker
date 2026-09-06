//! Rendering — ratatui widgets per tab plus a colored Braille line chart
//! for price + Bollinger bands.

use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    symbols,
    text::{Line, Span},
    widgets::{
        Axis, Block, Borders, Cell, Chart, Dataset, GraphType, Padding, Paragraph, Row, Table,
        Tabs, Wrap,
    },
    Frame,
};
use ticker_proto::{HoldingRow, PortfolioSummary, TickerHistory, TransactionRow};

use crate::app::{App, Status, StatusKind, Tab, TradeField, TradeMode, TradeSide};

pub fn draw(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    // The footer grows by a line while a job runs, so the progress bar never
    // displaces the key hints or the status message — a user watching a
    // refresh still needs to see what else they can press.
    let footer = if app.job.is_some() { 3 } else { 2 };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(footer),
        ])
        .split(area);

    draw_banner(frame, chunks[0], app);
    draw_tabs(frame, chunks[1], app.tab);

    match app.tab {
        Tab::Portfolio => draw_portfolio(frame, chunks[2], app),
        Tab::Detail => draw_detail(frame, chunks[2], app),
        Tab::Trade => draw_trade(frame, chunks[2], app),
        Tab::Transactions => draw_transactions(frame, chunks[2], app),
    }

    draw_footer(frame, chunks[3], app);
}

/// Braille spinner. Animated off the event-loop frame counter, so it turns
/// whenever the loop is alive — which is exactly the reassurance it exists to
/// give while a job is running.
const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

fn draw_footer(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(job) = app.job.as_ref() else {
        draw_status(frame, area, &app.status, app.tab);
        return;
    };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(area);
    frame.render_widget(Paragraph::new(progress_line(app.frame, job)), rows[0]);
    draw_status(frame, rows[1], &app.status, app.tab);
}

fn progress_line(tick: usize, job: &crate::app::JobState) -> Line<'static> {
    // Roughly a third of the width, so the bar stays readable on a narrow
    // terminal without crowding out the label and the current item.
    const BAR: usize = 24;
    let filled = (job.fraction() * BAR as f64).round() as usize;
    let spinner = SPINNER[tick / 2 % SPINNER.len()];

    let mut spans = vec![
        Span::styled(format!(" {spinner} "), Style::default().fg(Color::Cyan).bold()),
        Span::styled(format!("{} ", job.label), Style::default().fg(Color::White).bold()),
    ];
    // A single-item job (a trade) has no meaningful bar — a spinner and the
    // label already say everything there is to say about its progress.
    if job.total > 1 {
        spans.push(Span::styled(
            "█".repeat(filled),
            Style::default().fg(Color::Green),
        ));
        spans.push(Span::styled(
            "░".repeat(BAR.saturating_sub(filled)),
            Style::default().fg(Color::DarkGray),
        ));
        spans.push(Span::styled(
            format!(" {}/{} ", job.done, job.total),
            Style::default().fg(Color::Gray),
        ));
    }
    if !job.current.is_empty() {
        spans.push(Span::styled(job.current.clone(), Style::default().fg(Color::Yellow)));
    }
    Line::from(spans)
}

fn draw_banner(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let right = provider_banner(app);
    // Reserve the right-hand slice for the provider label so the two never
    // overlap on a narrow terminal; the left text truncates instead.
    let right_width = right.width() as u16 + 1;
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(right_width.min(area.width))])
        .split(area);

    let left = Paragraph::new(Line::from(vec![
        Span::styled(" SIMULATION ", Style::default().bg(Color::Yellow).fg(Color::Black).bold()),
        Span::raw("  paper portfolio — no real money, no real trades "),
    ]));
    frame.render_widget(left, cols[0]);
    frame.render_widget(Paragraph::new(right).alignment(Alignment::Right), cols[1]);
}

/// The provider indicator. Always on screen, on every tab — whether prices can
/// be fetched at all is state you want before you press anything, not an error
/// you discover by pressing 'r'.
fn provider_banner(app: &App) -> Line<'static> {
    let Some(st) = app.provider.as_ref() else {
        return Line::from(Span::styled(
            "provider: ? ",
            Style::default().fg(Color::DarkGray),
        ));
    };

    let alarm = Style::default().bg(Color::Red).fg(Color::White).bold();
    match &st.selected {
        // A configured-but-unrecognized provider is not the same as never
        // having chosen one, and the banner is the only place the difference
        // shows before a fetch fails. The full reason is in `tickerctl
        // provider status`; there is no room for it here.
        None if st.unavailable.is_some() => Line::from(Span::styled(
            "⚠ configured provider unavailable — run: tickerctl provider set ",
            alarm,
        )),
        None => Line::from(Span::styled(
            "⚠ no provider — run: tickerctl provider set ",
            alarm,
        )),
        Some(p) if !st.ready => {
            Line::from(Span::styled(format!("⚠ {} — no API key ", p.label), alarm))
        }
        Some(p) => Line::from(Span::styled(
            format!("provider: {} ", p.label),
            Style::default().fg(Color::Green),
        )),
    }
}

fn draw_tabs(frame: &mut Frame<'_>, area: Rect, current: Tab) {
    let titles: Vec<Line> = Tab::ALL
        .iter()
        .enumerate()
        .map(|(i, t)| Line::from(format!(" {} {} ", i + 1, t.title())))
        .collect();
    let idx = Tab::ALL.iter().position(|t| *t == current).unwrap_or(0);
    let tabs = Tabs::new(titles)
        .select(idx)
        .block(Block::default().borders(Borders::ALL).title(" Paper Portfolio "))
        .highlight_style(Style::default().fg(Color::Black).bg(Color::Cyan).bold());
    frame.render_widget(tabs, area);
}

fn draw_portfolio(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(5), Constraint::Min(0)])
        .split(area);

    if let Some(s) = &app.summary {
        draw_summary_cards(frame, chunks[0], s);
    } else {
        frame.render_widget(
            Paragraph::new("Loading…").block(Block::default().borders(Borders::ALL)),
            chunks[0],
        );
    }

    let rows: Vec<Row> = app
        .summary
        .as_ref()
        .map(|s| s.rows.iter().map(holding_row).collect())
        .unwrap_or_default();

    let header = Row::new(vec![
        "Ticker", "Shares", "Avg Cost", "Price", "Value", "Weight", "Gain", "Return",
    ])
    .style(Style::default().fg(Color::Gray).add_modifier(Modifier::BOLD));

    let widths = [
        Constraint::Length(8),
        Constraint::Length(11),
        Constraint::Length(11),
        Constraint::Length(11),
        Constraint::Length(13),
        Constraint::Length(8),
        Constraint::Length(13),
        Constraint::Length(10),
    ];
    let title = match app.summary.as_ref().and_then(|s| s.last_refresh.clone()) {
        Some(d) => {
            let today = chrono::Utc::now().date_naive();
            let suffix = chrono::NaiveDate::parse_from_str(&d, "%Y-%m-%d")
                .ok()
                .map(|cached| (today - cached).num_days())
                .map(|days| match days {
                    n if n <= 0 => "today".to_string(),
                    1 => "yesterday".to_string(),
                    n => format!("{n} days ago"),
                })
                .unwrap_or_default();
            if suffix.is_empty() {
                format!(" Holdings — last refresh {d} ")
            } else {
                format!(" Holdings — last refresh {d} ({suffix}) ")
            }
        }
        None => " Holdings ".to_string(),
    };
    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(title))
        .row_highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
        .highlight_symbol("> ");
    frame.render_stateful_widget(table, chunks[1], &mut app.holdings_state);
}

fn holding_row(h: &HoldingRow) -> Row<'static> {
    let gain_style = if h.gain >= 0.0 { Style::default().fg(Color::Green) } else { Style::default().fg(Color::Red) };
    Row::new(vec![
        Cell::from(h.ticker.clone()).style(Style::default().fg(Color::Cyan).bold()),
        Cell::from(format!("{:>10.4}", h.shares)),
        Cell::from(format!("${:>9.2}", h.avg_cost)),
        Cell::from(format!("${:>9.2}", h.current_price)),
        Cell::from(format!("${:>11.2}", h.value)),
        Cell::from(format!("{:>6.1}%", h.weight)),
        Cell::from(format!("${:>+11.2}", h.gain)).style(gain_style),
        Cell::from(format!("{:>+8.2}%", h.gain_pct)).style(gain_style),
    ])
}

fn draw_summary_cards(frame: &mut Frame<'_>, area: Rect, s: &PortfolioSummary) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
        ])
        .split(area);

    card(frame, cols[0], "Value", &format!("${:.2}", s.total_value), Color::White);
    card(frame, cols[1], "Cost", &format!("${:.2}", s.total_cost), Color::White);
    let gain_color = if s.total_gain >= 0.0 { Color::Green } else { Color::Red };
    card(frame, cols[2], "Gain", &format!("${:+.2}", s.total_gain), gain_color);
    card(frame, cols[3], "Return", &format!("{:+.2}%", s.total_gain_pct), gain_color);
}

fn card(frame: &mut Frame<'_>, area: Rect, title: &str, value: &str, color: Color) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {title} "))
        .padding(Padding::new(1, 1, 0, 0));
    let p = Paragraph::new(Span::styled(value.to_string(), Style::default().fg(color).bold()))
        .alignment(Alignment::Center)
        .block(block);
    frame.render_widget(p, area);
}

fn draw_detail(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(h) = &app.detail else {
        frame.render_widget(
            Paragraph::new("Pick a holding on the Portfolio tab (Enter), then return here.")
                .block(Block::default().borders(Borders::ALL).title(" Detail "))
                .wrap(Wrap { trim: true }),
            area,
        );
        return;
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(5), Constraint::Min(0)])
        .split(area);

    draw_detail_header(frame, chunks[0], h);
    draw_chart(frame, chunks[1], h);
}

fn draw_detail_header(frame: &mut Frame<'_>, area: Rect, h: &TickerHistory) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(25); 4])
        .split(area);

    card(frame, cols[0], "Price", &format!("${:.2}", h.current_price), Color::Cyan);
    let updated = h.last_updated.to_string();
    card(frame, cols[1], "Updated", &updated, Color::Gray);
    if let Some(p) = &h.holding {
        card(frame, cols[2], "Shares", &format!("{:.4}", p.shares), Color::White);
        let gain_color = if p.gain_pct >= 0.0 { Color::Green } else { Color::Red };
        card(frame, cols[3], "Return", &format!("{:+.2}%", p.gain_pct), gain_color);
    } else {
        card(frame, cols[2], "Shares", "—", Color::DarkGray);
        card(frame, cols[3], "Return", "—", Color::DarkGray);
    }
}

fn draw_chart(frame: &mut Frame<'_>, area: Rect, h: &TickerHistory) {
    if h.points.is_empty() {
        frame.render_widget(
            Paragraph::new("No price history.").block(Block::default().borders(Borders::ALL)),
            area,
        );
        return;
    }
    let n = h.points.len();
    let close: Vec<(f64, f64)> = h
        .points
        .iter()
        .enumerate()
        .map(|(i, p)| (i as f64, p.close))
        .collect();
    let sma: Vec<(f64, f64)> = h
        .bands
        .iter()
        .enumerate()
        .filter_map(|(i, b)| b.sma.map(|v| (i as f64, v)))
        .collect();
    let upper: Vec<(f64, f64)> = h
        .bands
        .iter()
        .enumerate()
        .filter_map(|(i, b)| b.upper.map(|v| (i as f64, v)))
        .collect();
    let lower: Vec<(f64, f64)> = h
        .bands
        .iter()
        .enumerate()
        .filter_map(|(i, b)| b.lower.map(|v| (i as f64, v)))
        .collect();

    let mut min_y = f64::INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    for series in [&close, &sma, &upper, &lower] {
        for (_, v) in series {
            if *v < min_y { min_y = *v; }
            if *v > max_y { max_y = *v; }
        }
    }
    if !min_y.is_finite() || !max_y.is_finite() {
        min_y = 0.0;
        max_y = 1.0;
    }
    let pad = ((max_y - min_y) * 0.05).max(0.01);
    min_y -= pad;
    max_y += pad;

    let datasets = vec![
        Dataset::default()
            .name("Upper")
            .marker(symbols::Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(Color::DarkGray))
            .data(&upper),
        Dataset::default()
            .name("Lower")
            .marker(symbols::Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(Color::DarkGray))
            .data(&lower),
        Dataset::default()
            .name("SMA(20)")
            .marker(symbols::Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(Color::Yellow))
            .data(&sma),
        Dataset::default()
            .name("Close")
            .marker(symbols::Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
            .data(&close),
    ];

    let x_labels = {
        let first = h.points.first().map(|p| p.date.clone()).unwrap_or_default();
        let mid = h.points.get(n / 2).map(|p| p.date.clone()).unwrap_or_default();
        let last = h.points.last().map(|p| p.date.clone()).unwrap_or_default();
        vec![Span::raw(first), Span::raw(mid), Span::raw(last)]
    };
    let y_labels = vec![
        Span::raw(format!("${:.2}", min_y)),
        Span::raw(format!("${:.2}", (min_y + max_y) / 2.0)),
        Span::raw(format!("${:.2}", max_y)),
    ];

    let chart = Chart::new(datasets)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" {} — Close · SMA(20) · Bollinger ±2σ ", h.ticker)),
        )
        .x_axis(
            Axis::default()
                .style(Style::default().fg(Color::DarkGray))
                .bounds([0.0, (n.saturating_sub(1)).max(1) as f64])
                .labels(x_labels),
        )
        .y_axis(
            Axis::default()
                .style(Style::default().fg(Color::DarkGray))
                .bounds([min_y, max_y])
                .labels(y_labels),
        );
    frame.render_widget(chart, area);
}

fn draw_trade(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let side_label = match app.trade.side {
        TradeSide::Buy => "BUY",
        TradeSide::Sell => "SELL",
    };
    let side_color = match app.trade.side {
        TradeSide::Buy => Color::Green,
        TradeSide::Sell => Color::Red,
    };
    let (mode_label, mode_bg) = match app.trade.mode {
        TradeMode::Nav => (" NAV ", Color::Blue),
        TradeMode::Edit => (" EDIT ", Color::Magenta),
    };
    let outer = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(vec![
            Span::raw(" Trade · "),
            Span::styled(side_label, Style::default().fg(side_color).bold()),
            Span::raw(" · "),
            Span::styled(mode_label, Style::default().bg(mode_bg).fg(Color::White).bold()),
            Span::raw(" "),
        ]));
    let inner = outer.inner(area);
    frame.render_widget(outer, area);

    let cols = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(0),
        ])
        .horizontal_margin(2)
        .vertical_margin(1)
        .split(inner);

    let banner = match app.trade.mode {
        TradeMode::Nav => Paragraph::new(Line::from(vec![
            Span::styled(
                " i / Enter ",
                Style::default().bg(Color::Green).fg(Color::Black).bold(),
            ),
            Span::raw(" edit field  "),
            Span::styled(
                " j / k ",
                Style::default().bg(Color::Cyan).fg(Color::Black).bold(),
            ),
            Span::raw(" move between fields  "),
            Span::styled(
                " h / l / Tab ",
                Style::default().bg(Color::Blue).fg(Color::White).bold(),
            ),
            Span::raw(" switch tabs  "),
            Span::styled(
                " Esc ",
                Style::default().bg(Color::Yellow).fg(Color::Black).bold(),
            ),
            Span::raw(" back to Portfolio "),
        ])),
        TradeMode::Edit => Paragraph::new(Line::from(vec![
            Span::styled(
                " Esc ",
                Style::default().bg(Color::Yellow).fg(Color::Black).bold(),
            ),
            Span::raw(" leave EDIT (back to NAV)  "),
            Span::styled(
                " Tab ",
                Style::default().bg(Color::Cyan).fg(Color::Black).bold(),
            ),
            Span::raw(" next field  "),
            Span::styled(
                " Enter ",
                Style::default().bg(Color::Green).fg(Color::Black).bold(),
            ),
            Span::raw(" submit  "),
            Span::styled(
                " Ctrl-←/→ ",
                Style::default().bg(Color::Magenta).fg(Color::Black).bold(),
            ),
            Span::raw(" flip BUY/SELL "),
        ])),
    };
    frame.render_widget(banner, cols[0]);

    let editing = app.trade.mode == TradeMode::Edit;
    draw_field(
        frame,
        cols[1],
        "Ticker",
        &app.trade.ticker,
        app.trade.field == TradeField::Ticker,
        editing,
    );
    draw_field(
        frame,
        cols[2],
        "Shares",
        &app.trade.shares,
        app.trade.field == TradeField::Shares,
        editing,
    );
    if app.trade.side == TradeSide::Buy {
        let hint = if app.trade.price.is_empty() {
            "(blank = last cached close)".to_string()
        } else {
            app.trade.price.clone()
        };
        draw_field(
            frame,
            cols[3],
            "Price",
            &hint,
            app.trade.field == TradeField::Price,
            editing,
        );
    } else {
        let p = Paragraph::new("Sells execute at the last cached close.")
            .style(Style::default().fg(Color::DarkGray));
        frame.render_widget(p, cols[3]);
    }
}

fn draw_field(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    value: &str,
    focused: bool,
    editing: bool,
) {
    let border_style = if focused && editing {
        Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD)
    } else if focused {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {label} "))
        .border_style(border_style);
    let cursor = if focused && editing { "█" } else { "" };
    let p = Paragraph::new(format!("{value}{cursor}")).block(block);
    frame.render_widget(p, area);
}

fn draw_transactions(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let rows: Vec<Row> = app.txns.iter().map(txn_row).collect();
    let header = Row::new(vec!["Date", "Ticker", "Shares", "Price", "Total"])
        .style(Style::default().fg(Color::Gray).bold());
    let widths = [
        Constraint::Length(12),
        Constraint::Length(8),
        Constraint::Length(12),
        Constraint::Length(12),
        Constraint::Length(14),
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(" Transactions "))
        .row_highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
        .highlight_symbol("> ");
    frame.render_stateful_widget(table, area, &mut app.txns_state);
}

fn txn_row(t: &TransactionRow) -> Row<'static> {
    let side = if t.shares >= 0.0 { Color::Green } else { Color::Red };
    Row::new(vec![
        Cell::from(t.txn_date.clone()),
        Cell::from(t.ticker.clone()).style(Style::default().fg(Color::Cyan).bold()),
        Cell::from(format!("{:+.4}", t.shares)).style(Style::default().fg(side)),
        Cell::from(format!("${:.2}", t.price)),
        Cell::from(format!("${:+.2}", t.shares * t.price)).style(Style::default().fg(side)),
    ])
}

fn draw_status(frame: &mut Frame<'_>, area: Rect, status: &Status, tab: Tab) {
    let hints = match tab {
        Tab::Portfolio => "[↑↓/jk] move  [Enter] detail  [b]uy  [s]ell  [r]efresh  [R] force  [Tab/h/l] tabs  [q]uit",
        Tab::Detail => "[↑↓/jk] cycle holdings  [r]efresh  [R] force  [b]uy  [s]ell  [Tab/h/l] tabs  [q]uit",
        Tab::Trade => "NAV: [i/Enter] edit  [j/k] field  [h/l/Tab] tabs  [Esc] back   |   EDIT: [Esc] leave  [Tab] field  [Enter] submit",
        Tab::Transactions => "[↑↓/jk] move  [r]efresh  [R] force  [Tab/h/l] tabs  [q]uit",
    };
    let style = match status.kind {
        StatusKind::Info => Style::default().fg(Color::Gray),
        StatusKind::Success => Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
        StatusKind::Warn => Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        StatusKind::Error => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
    };
    let line1 = Line::from(Span::styled(hints, Style::default().fg(Color::White).bg(Color::Blue)));
    let line2 = Line::from(Span::styled(status.message.clone(), style));
    let p = Paragraph::new(vec![line1, line2]);
    frame.render_widget(p, area);
}

#[cfg(test)]
mod tests {
    use super::progress_line;
    use crate::app::JobState;

    fn job(done: usize, total: usize, current: &str) -> JobState {
        JobState {
            label: "Refreshing".into(),
            done,
            total,
            current: current.into(),
        }
    }

    /// The rendered line, with styling dropped.
    fn text(tick: usize, job: &JobState) -> String {
        progress_line(tick, job).spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn a_multi_item_job_shows_a_bar_a_count_and_the_current_item() {
        let line = text(0, &job(3, 7, "NVDA"));
        assert!(line.contains("Refreshing"), "{line}");
        assert!(line.contains("3/7"), "{line}");
        assert!(line.contains("NVDA"), "{line}");
        assert!(line.contains('█') && line.contains('░'), "no bar: {line}");
    }

    #[test]
    fn the_bar_fills_as_the_job_progresses() {
        let filled = |done| text(0, &job(done, 8, "X")).matches('█').count();
        assert_eq!(filled(0), 0);
        assert!(filled(4) > 0 && filled(4) < filled(8));
        // Never over-fills: `repeat` on a saturating_sub would silently
        // truncate, but an over-long bar would wrap the footer line.
        assert_eq!(filled(8), 24);
    }

    #[test]
    fn a_single_item_job_gets_a_spinner_but_no_bar() {
        // A trade has nothing meaningful to show as a fraction — a half-full
        // bar would imply progress the client cannot actually observe.
        let line = text(0, &job(0, 1, "Buying 5 AAPL"));
        assert!(!line.contains('█') && !line.contains('░'), "{line}");
        assert!(line.contains("Buying 5 AAPL"), "{line}");
    }

    #[test]
    fn the_spinner_advances_with_the_frame_counter() {
        let j = job(1, 4, "IBM");
        let frames: std::collections::HashSet<char> =
            (0..16).map(|t| text(t, &j).chars().nth(1).unwrap()).collect();
        assert!(frames.len() > 1, "spinner never moved: {frames:?}");
    }
}
