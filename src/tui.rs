use std::io::{self, Stdout};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::ExecutableCommand;
use iroh::PublicKey;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Terminal;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::network::Network;
use crate::radio::ConfigSource;
use crate::router::{PacketAction, PacketDirection, PacketLogEntry, RadioConfigInfo, Stats, StatsSnapshot};

const TICK_RATE: Duration = Duration::from_millis(250);
const MAX_LOG_ENTRIES: usize = 200;

/// Run the TUI event loop. Blocks until `q` is pressed or cancel is triggered.
pub async fn run(
    node_id: PublicKey,
    config_watch: tokio::sync::watch::Receiver<RadioConfigInfo>,
    network: Arc<Network>,
    stats: Arc<Stats>,
    mut log_rx: mpsc::Receiver<PacketLogEntry>,
    cancel: CancellationToken,
    start_time: Instant,
) -> anyhow::Result<()> {
    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let mut log_entries: Vec<PacketLogEntry> = Vec::with_capacity(MAX_LOG_ENTRIES);

    let result = tui_loop(
        &mut terminal,
        node_id,
        config_watch,
        &network,
        &stats,
        &mut log_rx,
        &cancel,
        start_time,
        &mut log_entries,
    )
    .await;

    disable_raw_mode()?;
    io::stdout().execute(LeaveAlternateScreen)?;

    result
}

#[allow(clippy::too_many_arguments)]
async fn tui_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    node_id: PublicKey,
    config_watch: tokio::sync::watch::Receiver<RadioConfigInfo>,
    network: &Network,
    stats: &Stats,
    log_rx: &mut mpsc::Receiver<PacketLogEntry>,
    cancel: &CancellationToken,
    start_time: Instant,
    log_entries: &mut Vec<PacketLogEntry>,
) -> anyhow::Result<()> {
    let mut last_tick = Instant::now();

    loop {
        // Drain log entries.
        while let Ok(entry) = log_rx.try_recv() {
            if log_entries.len() >= MAX_LOG_ENTRIES {
                log_entries.remove(0);
            }
            log_entries.push(entry);
        }

        if cancel.is_cancelled() {
            return Ok(());
        }

        let snap = stats.snapshot();
        let peers = network.peer_states().await;
        let config_info = config_watch.borrow().clone();

        terminal.draw(|frame| {
            let area = frame.area();
            draw_ui(
                frame,
                area,
                &node_id,
                &config_info,
                &snap,
                &peers,
                log_entries,
                start_time,
            );
        })?;

        // Handle input with timeout for tick rate.
        let timeout = TICK_RATE
            .checked_sub(last_tick.elapsed())
            .unwrap_or(Duration::ZERO);

        if event::poll(timeout)?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
            && key.code == KeyCode::Char('q')
        {
            cancel.cancel();
            return Ok(());
        }

        if last_tick.elapsed() >= TICK_RATE {
            last_tick = Instant::now();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_ui(
    frame: &mut ratatui::Frame,
    area: Rect,
    node_id: &PublicKey,
    config_info: &RadioConfigInfo,
    stats: &StatsSnapshot,
    peers: &[crate::network::PeerState],
    log_entries: &[PacketLogEntry],
    start_time: Instant,
) {
    // Outer layout: header + body.
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(area);

    draw_header(frame, outer[0], node_id, start_time, stats);

    // Body: 2x2 grid.
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(outer[1]);

    let top_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[0]);

    let bot_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
        .split(rows[1]);

    draw_peers(frame, top_cols[0], peers);
    draw_radio(frame, top_cols[1], config_info);
    draw_stats(frame, bot_cols[0], stats, start_time);
    draw_log(frame, bot_cols[1], log_entries);
}

fn draw_header(
    frame: &mut ratatui::Frame,
    area: Rect,
    node_id: &PublicKey,
    start_time: Instant,
    stats: &StatsSnapshot,
) {
    let id_str = node_id.to_string();
    let id_short = if id_str.len() > 16 {
        format!("{}...", &id_str[..16])
    } else {
        id_str
    };
    let radio_indicator = if stats.radio_connected {
        Span::styled(" RADIO ", Style::default().fg(Color::Black).bg(Color::Green))
    } else {
        Span::styled(" RADIO ", Style::default().fg(Color::Black).bg(Color::Red))
    };
    let uptime = format_duration(start_time.elapsed());
    let now = chrono_time();

    let line = Line::from(vec![
        Span::styled(
            " donglora-bridge ",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        radio_indicator,
        Span::raw("  "),
        Span::styled("ID: ", Style::default().fg(Color::DarkGray)),
        Span::styled(id_short, Style::default().fg(Color::Cyan)),
        Span::raw("  "),
        Span::styled(format!("up {uptime}"), Style::default().fg(Color::DarkGray)),
        Span::raw("  "),
        Span::styled(now, Style::default().fg(Color::DarkGray)),
        Span::styled("  q=quit", Style::default().fg(Color::DarkGray)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn draw_peers(frame: &mut ratatui::Frame, area: Rect, peers: &[crate::network::PeerState]) {
    let block = Block::default()
        .title(" Peers ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    let mut lines: Vec<Line> = Vec::new();
    if peers.is_empty() {
        lines.push(Line::styled(
            "  No peers configured",
            Style::default().fg(Color::DarkGray),
        ));
    }
    for peer in peers {
        let (dot, color) = if peer.connected {
            ("●", Color::Green)
        } else {
            ("○", Color::Red)
        };
        let status = if peer.connected {
            let ago = peer
                .last_seen
                .map(|t| format_short_duration(t.elapsed()))
                .unwrap_or_default();
            format!("  {ago}")
        } else {
            "  reconnecting".into()
        };
        let pk_short = {
            let s = peer.public_key.to_string();
            if s.len() > 8 { format!(" ({}...)", &s[..8]) } else { format!(" ({s})") }
        };
        let pkts = format!("  in:{} out:{}", peer.packets_in, peer.packets_out);
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(dot, Style::default().fg(color)),
            Span::raw(" "),
            Span::styled(
                peer.name.clone(),
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            ),
            Span::styled(pk_short, Style::default().fg(Color::DarkGray)),
            Span::styled(status, Style::default().fg(Color::DarkGray)),
            Span::styled(pkts, Style::default().fg(Color::DarkGray)),
        ]));
    }
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_radio(frame: &mut ratatui::Frame, area: Rect, config_info: &RadioConfigInfo) {
    let title = match config_info.source {
        ConfigSource::Ours => " Radio ",
        ConfigSource::Mux => " Radio (mux config) ",
    };
    let border_color = match config_info.source {
        ConfigSource::Ours => Color::DarkGray,
        ConfigSource::Mux => Color::Yellow,
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

    let mut lines: Vec<Line> = Vec::new();

    if matches!(config_info.source, ConfigSource::Mux) {
        lines.push(Line::styled(
            "  Using mux locked config",
            Style::default().fg(Color::Yellow),
        ));
    }

    lines.extend(config_info.config_str.split_whitespace().map(|part| {
        let (label, value) = part.split_once(':').unwrap_or(("", part));
        if label.is_empty() {
            Line::from(vec![
                Span::raw("  "),
                Span::styled(value, Style::default().fg(Color::Cyan)),
            ])
        } else {
            Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("{label}: "), Style::default().fg(Color::DarkGray)),
                Span::styled(value, Style::default().fg(Color::Cyan)),
            ])
        }
    }));

    frame.render_widget(Paragraph::new(lines).block(block).wrap(Wrap { trim: false }), area);
}

fn draw_stats(
    frame: &mut ratatui::Frame,
    area: Rect,
    stats: &StatsSnapshot,
    start_time: Instant,
) {
    let block = Block::default()
        .title(" Stats ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    let stat_line = |label: &str, value: u64, color: Color| -> Line {
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                format!("{label:<14}"),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(format!("{value:>8}"), Style::default().fg(color)),
        ])
    };

    let uptime = format_duration(start_time.elapsed());
    let lines = vec![
        Line::from(vec![
            Span::raw("  "),
            Span::styled("Uptime        ", Style::default().fg(Color::DarkGray)),
            Span::styled(format!("{uptime:>8}"), Style::default().fg(Color::White)),
        ]),
        stat_line("Radio RX", stats.radio_rx, Color::Green),
        stat_line("Radio TX", stats.radio_tx, Color::Blue),
        stat_line("Forwarded", stats.forwarded, Color::Cyan),
        stat_line("Dropped SNR", stats.dropped_snr, Color::Yellow),
        stat_line("Deduped", stats.dropped_dedup, Color::DarkGray),
        stat_line("Queue drops", stats.dropped_queue, Color::Red),
    ];

    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_log(frame: &mut ratatui::Frame, area: Rect, entries: &[PacketLogEntry]) {
    let block = Block::default()
        .title(" Packet Log ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    let inner_height = area.height.saturating_sub(2) as usize;
    let start = entries.len().saturating_sub(inner_height);
    let visible = &entries[start..];

    let lines: Vec<Line> = visible
        .iter()
        .map(|e| {
            let age = format_short_duration(e.timestamp.elapsed());
            let dir_color = match e.direction {
                PacketDirection::RadioRx => Color::Green,
                PacketDirection::RadioTx => Color::Blue,
                PacketDirection::NetRx => Color::Magenta,
            };
            let action_color = match e.action {
                PacketAction::Forwarded => Color::Cyan,
                PacketAction::Transmitted => Color::Blue,
                PacketAction::DroppedDedup => Color::DarkGray,
                PacketAction::DroppedQueueFull => Color::Red,
            };
            let rssi_str = e
                .rssi
                .map(|r| format!(" {r}dBm"))
                .unwrap_or_default();
            let snr_str = e
                .snr
                .map(|s| format!(" snr:{s}"))
                .unwrap_or_default();
            let peer_str = e
                .peer_name
                .as_deref()
                .map(|n| format!(" {n}"))
                .unwrap_or_default();

            Line::from(vec![
                Span::styled(
                    format!(" {age:>4} "),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(
                    format!("{}", e.direction),
                    Style::default().fg(dir_color),
                ),
                Span::styled(
                    format!(" {:>4}B", e.size),
                    Style::default().fg(Color::White),
                ),
                Span::styled(rssi_str, Style::default().fg(Color::DarkGray)),
                Span::styled(snr_str, Style::default().fg(Color::DarkGray)),
                Span::styled(peer_str, Style::default().fg(Color::DarkGray)),
                Span::raw(" "),
                Span::styled(
                    format!("{}", e.action),
                    Style::default().fg(action_color),
                ),
            ])
        })
        .collect();

    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    }
}

fn format_short_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h", secs / 3600)
    }
}

fn chrono_time() -> String {
    // Use a simple approach without chrono dependency.
    let now = std::time::SystemTime::now();
    let since_epoch = now
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = since_epoch.as_secs();
    let hours = (secs / 3600) % 24;
    let minutes = (secs % 3600) / 60;
    let seconds = secs % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}")
}
