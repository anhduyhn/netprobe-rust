use chrono::Local;
use ratatui::{
    layout::{Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Cell, Paragraph, Row, Scrollbar, ScrollbarOrientation, ScrollbarState,
        Table, Tabs, Wrap,
    },
    Frame,
};

use crate::app::{App, HostStatus, InputMode, TableRow};

/// Braille spinner frames (render fine in Windows Terminal).
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn draw(frame: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header
            Constraint::Length(1), // tab bar
            Constraint::Min(10),   // table
            Constraint::Length(6), // detail
        ])
        .split(frame.area());

    draw_header(frame, app, chunks[0]);
    draw_tabs(frame, app, chunks[1]);

    if app.show_debug {
        draw_debug(frame, app, chunks[2]);
    } else if app.show_help {
        draw_help(frame, chunks[2]);
    } else {
        draw_table(frame, app, chunks[2]);
    }

    draw_detail(frame, app, chunks[3]);
}

fn draw_tabs(frame: &mut Frame, app: &App, area: Rect) {
    let titles: Vec<Line> = app.tabs.iter().map(|t| Line::from(format!(" {t} "))).collect();
    let tabs = Tabs::new(titles)
        .select(app.active_tab)
        .style(Style::default().fg(Color::DarkGray))
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .divider("");
    frame.render_widget(tabs, area);
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let (up, down, unknown) = app.counts();
    let total = app.visible_host_count();
    let poll_text = app
        .last_poll
        .map(|t| t.format(" │ %H:%M:%S").to_string())
        .unwrap_or_default();
    let status = app.status_message.as_deref().unwrap_or("");

    let mut spans = vec![
        Span::styled(
            " netprobe ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!("│ {total} hosts │ ")),
        Span::styled(format!("● {up}"), Style::default().fg(Color::Green)),
        Span::raw(" "),
        Span::styled(format!("✗ {down}"), Style::default().fg(Color::Red)),
        Span::raw(" "),
        Span::styled(format!("? {unknown}"), Style::default().fg(Color::DarkGray)),
        Span::styled(poll_text, Style::default().fg(Color::DarkGray)),
    ];

    // Scanning spinner, or an idle countdown to the next scan.
    if app.is_scanning {
        let frame_idx = (Local::now().timestamp_millis() / 100) as usize % SPINNER.len();
        spans.push(Span::styled(
            format!(" {} scanning", SPINNER[frame_idx]),
            Style::default().fg(Color::Yellow),
        ));
    } else if let Some(secs) = app.seconds_until_next_scan() {
        spans.push(Span::styled(
            format!(" │ next scan {secs}s"),
            Style::default().fg(Color::DarkGray),
        ));
    }

    // Search filter indicator (live draft while editing, committed otherwise).
    if app.input_mode == InputMode::Filter {
        spans.push(Span::styled(
            format!(" │ /{}▏", app.effective_query()),
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    } else if !app.filter_query.is_empty() {
        spans.push(Span::styled(
            format!(" │ /{}", app.filter_query),
            Style::default().fg(Color::Yellow),
        ));
    }

    if app.triage {
        spans.push(Span::styled(
            " │ [triage]",
            Style::default().fg(Color::Magenta),
        ));
    }

    if !status.is_empty() {
        spans.push(Span::styled(
            format!(" {status}"),
            Style::default().fg(Color::Yellow),
        ));
    }

    let block = Block::default().borders(Borders::ALL);
    frame.render_widget(Paragraph::new(Line::from(spans)).block(block), area);
}

fn draw_table(frame: &mut Frame, app: &mut App, area: Rect) {
    let header = Row::new(vec![
        Cell::from("St"),
        Cell::from("Host"),
        Cell::from("IP"),
        Cell::from("Role"),
        Cell::from("Ports"),
        Cell::from("ms"),
        Cell::from("Seen"),
    ])
    .style(
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    )
    .height(1);

    let rows: Vec<Row> = app
        .rows
        .iter()
        .map(|row| match row {
            TableRow::GroupHeader { name } => Row::new(vec![
                Cell::from("──"),
                Cell::from(format!(" {name} ")),
                Cell::from("──────────────"),
                Cell::from("──────"),
                Cell::from("────────────────"),
                Cell::from("────"),
                Cell::from("────"),
            ])
            .style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            TableRow::HostEntry {
                group_idx,
                host_idx,
            } => {
                let host = &app.groups[*group_idx].hosts[*host_idx];

                let status_style = match host.status {
                    HostStatus::Up => Style::default().fg(Color::Green),
                    HostStatus::Down => Style::default().fg(Color::Red),
                    HostStatus::Unknown => Style::default().fg(Color::DarkGray),
                };

                let name_prefix = if host.is_child { "  └ " } else { "" };

                let name_display = format!("{name_prefix}{}", host.name);

                let latency = host
                    .latency_ms
                    .map(|ms| format!("{ms}"))
                    .unwrap_or_else(|| "—".into());

                let seen = host
                    .last_seen
                    .map(|t| t.format("%H:%M").to_string())
                    .unwrap_or_else(|| "—".into());

                let role_label = host.role_label();
                let role_style = match role_label {
                    "DC" => Style::default().fg(Color::Yellow),
                    "Hyper-V" => Style::default().fg(Color::Magenta),
                    "SCCM" => Style::default().fg(Color::Blue),
                    "NxWitness" => Style::default().fg(Color::Cyan),
                    "SSH" => Style::default().fg(Color::Green),
                    _ => Style::default(),
                };

                let row_style = match host.status {
                    HostStatus::Down => Style::default().fg(Color::Red),
                    HostStatus::Unknown => Style::default().fg(Color::DarkGray),
                    _ => Style::default(),
                };

                Row::new(vec![
                    Cell::from(format!(" {}", host.status.symbol())).style(status_style),
                    Cell::from(name_display),
                    Cell::from(host.ip.as_str()),
                    Cell::from(role_label).style(role_style),
                    Cell::from(host.ports_display()),
                    Cell::from(latency),
                    Cell::from(seen),
                ])
                .style(row_style)
            }
        })
        .collect();

    let widths = [
        Constraint::Length(3),  // status
        Constraint::Length(24), // name
        Constraint::Length(16), // IP
        Constraint::Length(10), // role
        Constraint::Min(20),    // ports
        Constraint::Length(6),  // latency
        Constraint::Length(6),  // seen
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Infrastructure "),
        )
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));

    frame.render_stateful_widget(table, area, &mut app.table_state);

    // Scrollbar — only when the list is taller than the viewport. Borders (2)
    // and the header row (1) don't hold data rows.
    let visible_rows = area.height.saturating_sub(3) as usize;
    if app.rows.len() > visible_rows {
        let mut sb_state = ScrollbarState::new(app.rows.len()).position(app.table_state.offset());
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None);
        frame.render_stateful_widget(
            scrollbar,
            area.inner(Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut sb_state,
        );
    }
}

fn draw_detail(frame: &mut Frame, app: &App, area: Rect) {
    let content = match app.selected_host() {
        Some(host) => {
            let ports = if host.open_ports.is_empty() {
                "none".into()
            } else {
                host.open_ports
                    .iter()
                    .map(|p| p.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            };

            let last_checked = host
                .last_checked
                .map(|t| t.format("%H:%M:%S").to_string())
                .unwrap_or_else(|| "never".into());

            vec![
                Line::from(vec![
                    Span::styled("Host: ", Style::default().fg(Color::DarkGray)),
                    Span::raw(&host.name),
                    Span::raw(format!("  ({})", host.ip)),
                    Span::styled(
                        format!("  id={}", host.id),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("Ports: ", Style::default().fg(Color::DarkGray)),
                    Span::raw(ports),
                ]),
                Line::from(vec![
                    Span::styled("Checked: ", Style::default().fg(Color::DarkGray)),
                    Span::raw(last_checked),
                    Span::raw(format!("  │  Failures: {}", host.consecutive_failures)),
                ]),
            ]
        }
        None => vec![Line::from("Select a host")],
    };

    let keybinds = " ↑↓ nav │ ←→ tab │ / find │ t triage │ e export │ r rescan │ ? help │ q quit ";
    let block = Block::default().borders(Borders::ALL).title(keybinds);
    frame.render_widget(
        Paragraph::new(content)
            .block(block)
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn draw_debug(frame: &mut Frame, app: &App, area: Rect) {
    let (up, down, unknown) = app.counts();
    let selected = app
        .table_state
        .selected()
        .map(|i| i.to_string())
        .unwrap_or_else(|| "—".into());

    let mut lines = vec![
        Line::from(Span::styled(
            " Debug — internal state (d to close, Shift-D clears log)",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(format!(
            " scanning={} groups={} rows={} selected_row={}",
            app.is_scanning,
            app.groups.len(),
            app.rows.len(),
            selected,
        )),
        Line::from(format!(
            " counts: up={up} down={down} unknown={unknown}  poll_interval={}s timeout={}ms",
            app.poll_interval, app.connect_timeout_ms,
        )),
    ];

    for (gi, group) in app.groups.iter().enumerate() {
        lines.push(Line::from(Span::styled(
            format!(" group[{gi}] {:<28} hosts={}", group.name, group.hosts.len()),
            Style::default().fg(Color::Cyan),
        )));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " Event log (newest last):",
        Style::default().add_modifier(Modifier::BOLD),
    )));

    // Show as many of the most recent log lines as fit in the pane.
    let header_lines = lines.len() as u16;
    let avail = area.height.saturating_sub(2).saturating_sub(header_lines) as usize;
    let skip = app.debug_log.len().saturating_sub(avail.max(1));
    for line in app.debug_log.iter().skip(skip) {
        lines.push(Line::from(Span::styled(
            format!(" {line}"),
            Style::default().fg(Color::DarkGray),
        )));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Debug (d to close) ");
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_help(frame: &mut Frame, area: Rect) {
    let lines = vec![
        Line::from(Span::styled(
            " Keybinds",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(" ↑/k, ↓/j    Navigate hosts"),
        Line::from(" ←/h, →/l    Switch group tab (All + one per group)"),
        Line::from(" Tab/Shift-Tab  Switch group tab"),
        Line::from(" /           Search: filter by name or IP (Enter apply, Esc clear)"),
        Line::from(" t           Triage: show only hosts that are not Up"),
        Line::from(" e           Export a CSV snapshot next to the executable"),
        Line::from(" r           Force rescan all hosts"),
        Line::from(" d           Toggle debug overlay (internal state + event log)"),
        Line::from(" Shift-D     Clear the debug event log"),
        Line::from(" ?           Toggle this help"),
        Line::from(" q / Esc     Quit (Esc clears an active search first)"),
        Line::from(""),
        Line::from(Span::styled(
            " Config",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(" Edit config.toml to add/remove hosts and groups."),
        Line::from(" Roles are inferred from open ports (e.g. 88+389 = DC)."),
        Line::from(" default_tab opens a chosen tab; the last tab is remembered."),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Help (? to close) ");
    frame.render_widget(Paragraph::new(lines).block(block), area);
}
