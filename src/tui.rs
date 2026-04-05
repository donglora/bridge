use std::io::{self, Stdout};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Sparkline};
use ratatui::Terminal;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::gossip::Gossip;
use crate::radio::ConfigSource;
use crate::router::{PacketAction, PacketDirection, PacketLogEntry, RadioConfigInfo, Stats, StatsSnapshot};

const TICK_RATE: Duration = Duration::from_millis(250);
const MAX_LOG_ENTRIES: usize = 200;
const BUCKET_SECS: u64 = 10;
const MAX_BUCKETS: usize = 60;

// ── Color palette (consistent across panels) ─────────────────────

const C_RADIO_RX: Color = Color::Green;
const C_RADIO_TX: Color = Color::Blue;
const C_NET_RX: Color = Color::Magenta;
const C_NET_TX: Color = Color::Cyan;
const C_DEDUP: Color = Color::DarkGray;
const C_RATE_DROP: Color = Color::Yellow;
const C_QUEUE_DROP: Color = Color::Red;
const C_SNR: Color = Color::Yellow;
const C_LABEL: Color = Color::DarkGray;
const C_VALUE: Color = Color::White;
const C_ACCENT: Color = Color::Cyan;
const C_BORDER: Color = Color::DarkGray;
const C_TITLE: Color = Color::White;

// ── Time series ──────────────────────────────────────────────────

#[derive(Default, Clone, Copy)]
struct Bucket {
    radio_rx: u64,
    radio_tx: u64,
    snr_sum: i64,
    snr_count: u64,
}

struct TimeSeries {
    buckets: Vec<Bucket>,
    last_snap: StatsSnapshot,
    last_bucket_time: Instant,
}

impl TimeSeries {
    fn new(snap: &StatsSnapshot) -> Self {
        Self {
            buckets: vec![Bucket::default(); MAX_BUCKETS],
            last_snap: snap.clone(),
            last_bucket_time: Instant::now(),
        }
    }

    fn update(&mut self, snap: &StatsSnapshot, log_entries: &[PacketLogEntry]) {
        let now = Instant::now();
        if now.duration_since(self.last_bucket_time).as_secs() >= BUCKET_SECS {
            let cutoff = self.last_bucket_time;
            let (snr_sum, snr_count) = snr_stats_since(log_entries, cutoff);
            let bucket = Bucket {
                radio_rx: snap.radio_rx.saturating_sub(self.last_snap.radio_rx),
                radio_tx: snap.radio_tx.saturating_sub(self.last_snap.radio_tx),
                snr_sum,
                snr_count,
            };
            self.buckets.push(bucket);
            if self.buckets.len() > MAX_BUCKETS {
                self.buckets.remove(0);
            }
            self.last_snap = snap.clone();
            self.last_bucket_time = now;
        }
    }

    fn radio_rx(&self) -> Vec<u64> {
        self.buckets.iter().map(|b| b.radio_rx).collect()
    }
    fn radio_tx(&self) -> Vec<u64> {
        self.buckets.iter().map(|b| b.radio_tx).collect()
    }
    fn avg_snr(&self) -> Vec<u64> {
        self.buckets
            .iter()
            .map(|b| {
                if b.snr_count > 0 {
                    let avg = b.snr_sum / b.snr_count as i64;
                    (avg + 30).max(0) as u64
                } else {
                    0
                }
            })
            .collect()
    }

    fn peak_rx(&self) -> u64 {
        self.buckets.iter().map(|b| b.radio_rx).max().unwrap_or(0)
    }
    fn peak_tx(&self) -> u64 {
        self.buckets.iter().map(|b| b.radio_tx).max().unwrap_or(0)
    }
    fn latest_avg_snr(&self) -> Option<i64> {
        self.buckets.iter().rev().find(|b| b.snr_count > 0).map(|b| b.snr_sum / b.snr_count as i64)
    }
}

fn snr_stats_since(entries: &[PacketLogEntry], since: Instant) -> (i64, u64) {
    let mut sum = 0i64;
    let mut count = 0u64;
    for e in entries.iter().rev() {
        if e.timestamp <= since {
            break;
        }
        if let Some(s) = e.snr {
            sum += i64::from(s);
            count += 1;
        }
    }
    (sum, count)
}

// ── Public API ───────────────────────────────────────────────────

type Term = Terminal<CrosstermBackend<Stdout>>;

pub async fn run(
    config_watch: tokio::sync::watch::Receiver<RadioConfigInfo>,
    gossip: Arc<Gossip>,
    stats: Arc<Stats>,
    mut log_rx: mpsc::Receiver<PacketLogEntry>,
    cancel: CancellationToken,
    start_time: Instant,
) -> anyhow::Result<Term> {
    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let mut log_entries: Vec<PacketLogEntry> = Vec::with_capacity(MAX_LOG_ENTRIES);
    let mut series = TimeSeries::new(&stats.snapshot());

    let _result = tui_loop(
        &mut terminal, config_watch, &gossip, &stats,
        &mut log_rx, &cancel, start_time, &mut log_entries, &mut series,
    ).await;

    terminal.draw(|frame| draw_shutdown_overlay(frame, frame.area()))?;
    Ok(terminal)
}

pub fn restore_terminal(terminal: &mut Term) {
    let _ = disable_raw_mode();
    let _ = io::stdout().execute(LeaveAlternateScreen);
    let _ = terminal.show_cursor();
}

#[allow(clippy::too_many_arguments)]
async fn tui_loop(
    terminal: &mut Term,
    config_watch: tokio::sync::watch::Receiver<RadioConfigInfo>,
    gossip: &Gossip,
    stats: &Stats,
    log_rx: &mut mpsc::Receiver<PacketLogEntry>,
    cancel: &CancellationToken,
    start_time: Instant,
    log_entries: &mut Vec<PacketLogEntry>,
    series: &mut TimeSeries,
) -> anyhow::Result<()> {
    let mut last_tick = Instant::now();
    loop {
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
        series.update(&snap, log_entries);
        let swarm_state = gossip.swarm_state();
        let config_info = config_watch.borrow().clone();
        terminal.draw(|frame| {
            draw_ui(frame, frame.area(), &config_info, &snap, &swarm_state, log_entries, start_time, series);
        })?;
        let timeout = TICK_RATE.checked_sub(last_tick.elapsed()).unwrap_or(Duration::ZERO);
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

// ── Layout ───────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn draw_ui(
    frame: &mut ratatui::Frame,
    area: Rect,
    config_info: &RadioConfigInfo,
    stats: &StatsSnapshot,
    swarm_state: &crate::gossip::SwarmState,
    log_entries: &[PacketLogEntry],
    start_time: Instant,
    series: &TimeSeries,
) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Percentage(65), Constraint::Percentage(35)])
        .split(area);

    draw_header(frame, outer[0], stats);

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(30), Constraint::Min(40)])
        .split(outer[1]);

    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(6),
            Constraint::Length(12),
            Constraint::Min(5),
        ])
        .split(cols[0]);

    draw_network(frame, left[0], swarm_state);
    draw_radio(frame, left[1], config_info);
    draw_stats(frame, left[2], stats, start_time);
    draw_log(frame, cols[1], log_entries);
    draw_activity(frame, outer[2], series);
}

// ── Header ───────────────────────────────────────────────────────

fn draw_header(frame: &mut ratatui::Frame, area: Rect, stats: &StatsSnapshot) {
    let now = chrono_time();
    let line = Line::from(vec![
        Span::styled(" donglora-bridge ", Style::default().fg(C_TITLE).add_modifier(Modifier::BOLD)),
        status_pill("RADIO", stats.radio_connected),
        Span::raw(" "),
        status_pill("NET", stats.neighbor_count > 0),
        Span::raw("  "),
        Span::styled(now, Style::default().fg(C_LABEL)),
        Span::styled("  q=quit", Style::default().fg(C_LABEL)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn status_pill(label: &str, ok: bool) -> Span<'static> {
    let (fg, bg) = if ok {
        (Color::Black, Color::Green)
    } else {
        (Color::White, C_LABEL)
    };
    Span::styled(format!(" {label} "), Style::default().fg(fg).bg(bg))
}

// ── Network panel (was Swarm) ────────────────────────────────────

fn draw_network(frame: &mut ratatui::Frame, area: Rect, state: &crate::gossip::SwarmState) {
    let block = panel_block(" Network ");

    let dht_color = match state.dht_status {
        crate::gossip::DhtStatus::Ready => Color::Green,
        crate::gossip::DhtStatus::Bootstrapping => C_RATE_DROP,
        crate::gossip::DhtStatus::PublishFailed => C_QUEUE_DROP,
    };
    let neighbor_color = if state.neighbor_count > 0 { Color::Green } else { C_RATE_DROP };
    let last_pub = state
        .last_dht_publish
        .map(|t| format!("{}s ago", t.elapsed().as_secs()))
        .unwrap_or_else(|| "-".into());

    let lines = vec![
        kv_line("ID", &state.topic_hash, C_ACCENT),
        kv_line("Peers", &format!("{}", state.neighbor_count), neighbor_color),
        kv_line("DHT", &format!("{}", state.dht_status), dht_color),
        kv_line("Published", &last_pub, C_LABEL),
    ];
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

// ── Radio panel ──────────────────────────────────────────────────

fn draw_radio(frame: &mut ratatui::Frame, area: Rect, config_info: &RadioConfigInfo) {
    let is_mux = matches!(config_info.source, ConfigSource::Mux);
    let border_color = if is_mux { C_RATE_DROP } else { C_BORDER };
    let block = Block::default()
        .title(Span::styled(
            if is_mux { " Radio (mux) " } else { " Radio " },
            Style::default().fg(C_TITLE).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

    let a = &config_info.active;
    let r = &config_info.requested;

    let freq_mhz = a.freq_hz as f64 / 1_000_000.0;
    let bw_str = crate::radio::format_bandwidth(a.bw);
    let power_str = if a.tx_power_dbm == donglora_client::TX_POWER_MAX {
        "max".to_string()
    } else {
        format!("{} dBm", a.tx_power_dbm)
    };
    let preamble = if a.preamble_len == 0 { 16 } else { a.preamble_len };
    let cad = if a.cad != 0 { "on" } else { "off" };

    let cf = |label: &str, value: String, matches: bool| -> Line<'static> {
        let color = if !is_mux || matches { C_ACCENT } else { C_RATE_DROP };
        let mark = if is_mux && !matches { " !" } else { "" };
        kv_line(label, &format!("{value}{mark}"), color)
    };

    let dev = if config_info.device.is_empty() { "-" } else { &config_info.device };
    let lines = vec![
        kv_line("Device", dev, C_ACCENT),
        cf("Freq", format!("{freq_mhz:.3} MHz"), a.freq_hz == r.freq_hz),
        cf("BW", bw_str.to_string(), a.bw == r.bw),
        cf("SF", format!("{}", a.sf), a.sf == r.sf),
        cf("CR", format!("4/{}", a.cr), a.cr == r.cr),
        cf("Sync", format!("0x{:04X}", a.sync_word), a.sync_word == r.sync_word),
        cf("TX", power_str, a.tx_power_dbm == r.tx_power_dbm),
        cf("Preamble", format!("{preamble}"), a.preamble_len == r.preamble_len),
        cf("CAD", cad.to_string(), a.cad == r.cad),
    ];
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

// ── Stats panel ──────────────────────────────────────────────────

fn draw_stats(frame: &mut ratatui::Frame, area: Rect, stats: &StatsSnapshot, start_time: Instant) {
    let block = panel_block(" Stats ");

    let s = |label: &str, value: u64, color: Color| -> Line<'static> {
        Line::from(vec![
            Span::raw(" "),
            Span::styled(format!("{label:<9}"), Style::default().fg(C_LABEL)),
            Span::styled(format!("{value:>6}"), Style::default().fg(color)),
        ])
    };

    let uptime = format_duration(start_time.elapsed());
    let lines = vec![
        Line::from(vec![
            Span::raw(" "),
            Span::styled("Uptime   ", Style::default().fg(C_LABEL)),
            Span::styled(format!("{uptime:>6}"), Style::default().fg(C_VALUE)),
        ]),
        s("RF RX", stats.radio_rx, C_RADIO_RX),
        s("RF TX", stats.radio_tx, C_RADIO_TX),
        s("Net RX", stats.gossip_rx, C_NET_RX),
        s("Net TX", stats.gossip_tx, C_NET_TX),
        s("Deduped", stats.dedup_hits, C_DEDUP),
        s("Rate lim", stats.rate_limit_drops, C_RATE_DROP),
        s("Q drop", stats.dropped_queue, C_QUEUE_DROP),
    ];
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

// ── Packet log ───────────────────────────────────────────────────

fn draw_log(frame: &mut ratatui::Frame, area: Rect, entries: &[PacketLogEntry]) {
    let block = panel_block(" Packet Log ");

    let inner_height = area.height.saturating_sub(2) as usize;
    let data_height = inner_height.saturating_sub(1); // header row
    let start = entries.len().saturating_sub(data_height);
    let visible = &entries[start..];

    // Column header with underline effect via bold.
    let header = Line::from(vec![
        Span::styled("  Age", Style::default().fg(C_LABEL).add_modifier(Modifier::BOLD)),
        Span::styled("   RF", Style::default().fg(C_LABEL).add_modifier(Modifier::BOLD)),
        Span::styled("  Net", Style::default().fg(C_LABEL).add_modifier(Modifier::BOLD)),
        Span::styled("  Size", Style::default().fg(C_LABEL).add_modifier(Modifier::BOLD)),
        Span::styled("  RSSI", Style::default().fg(C_LABEL).add_modifier(Modifier::BOLD)),
        Span::styled("  SNR", Style::default().fg(C_LABEL).add_modifier(Modifier::BOLD)),
        Span::styled("  Act", Style::default().fg(C_LABEL).add_modifier(Modifier::BOLD)),
    ]);

    let mut lines = vec![header];

    for e in visible {
        let age = format_short_duration(e.timestamp.elapsed());

        let dash = "  ───";
        let (rf_col, net_col) = match e.direction {
            PacketDirection::RadioRx => (
                Span::styled("   RX", Style::default().fg(C_RADIO_RX)),
                Span::styled(dash, Style::default().fg(C_BORDER)),
            ),
            PacketDirection::RadioTx => (
                Span::styled("   TX", Style::default().fg(C_RADIO_TX)),
                Span::styled(dash, Style::default().fg(C_BORDER)),
            ),
            PacketDirection::GossipRx => (
                Span::styled(dash, Style::default().fg(C_BORDER)),
                Span::styled("   RX", Style::default().fg(C_NET_RX)),
            ),
        };

        let action_color = match e.action {
            PacketAction::Forwarded => C_NET_TX,
            PacketAction::Transmitted => C_RADIO_TX,
            PacketAction::DroppedDedup => C_DEDUP,
            PacketAction::DroppedQueueFull => C_QUEUE_DROP,
            PacketAction::DroppedRateLimit => C_RATE_DROP,
        };

        let rssi_span = match e.rssi {
            Some(r) => Span::styled(format!("{r:>6}"), Style::default().fg(rssi_color(r))),
            None => Span::styled("     -", Style::default().fg(C_LABEL)),
        };
        let snr_span = match e.snr {
            Some(s) => Span::styled(format!("{s:>5}"), Style::default().fg(snr_color(s))),
            None => Span::styled("    -", Style::default().fg(C_LABEL)),
        };

        lines.push(Line::from(vec![
            Span::styled(format!(" {age:>3} "), Style::default().fg(C_LABEL)),
            rf_col,
            net_col,
            Span::styled(format!("{:>5}B", e.size), Style::default().fg(C_VALUE)),
            rssi_span,
            snr_span,
            Span::styled(format!("  {}", e.action), Style::default().fg(action_color)),
        ]));
    }

    frame.render_widget(Paragraph::new(lines).block(block), area);
}

// ── Activity chart ───────────────────────────────────────────────

fn draw_activity(frame: &mut ratatui::Frame, area: Rect, series: &TimeSeries) {
    let block = panel_block(" Activity (10s buckets) ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height < 3 || inner.width < 20 {
        return;
    }

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Ratio(1, 3),
            Constraint::Ratio(1, 3),
            Constraint::Ratio(1, 3),
        ])
        .split(inner);

    let rx_data = series.radio_rx();
    let tx_data = series.radio_tx();
    let snr_data = series.avg_snr();

    let label_w = 12u16;
    let spark_w = inner.width.saturating_sub(label_w) as usize;

    let peak_rx = series.peak_rx();
    let peak_tx = series.peak_tx();
    let snr_label = series
        .latest_avg_snr()
        .map(|s| format!("avg:{s}dB"))
        .unwrap_or_else(|| "-".into());

    let rx_padded = pad_right_align(&rx_data, spark_w);
    let tx_padded = pad_right_align(&tx_data, spark_w);
    let snr_padded = pad_right_align(&snr_data, spark_w);

    draw_sparkline_row(frame, rows[0], "RF RX", &format!("pk:{peak_rx}"), &rx_padded, C_RADIO_RX, label_w);
    draw_sparkline_row(frame, rows[1], "RF TX", &format!("pk:{peak_tx}"), &tx_padded, C_RADIO_TX, label_w);
    draw_sparkline_row(frame, rows[2], "SNR", &snr_label, &snr_padded, C_SNR, label_w);
}

fn draw_sparkline_row(
    frame: &mut ratatui::Frame,
    area: Rect,
    label: &str,
    detail: &str,
    data: &[u64],
    color: Color,
    label_width: u16,
) {
    if area.width < label_width + 5 || area.height < 1 {
        return;
    }

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(label_width), Constraint::Min(1)])
        .split(area);

    // Label + detail stacked if room, otherwise just label.
    let mut label_lines = vec![
        Line::from(Span::styled(format!(" {label}"), Style::default().fg(color).add_modifier(Modifier::BOLD))),
    ];
    if cols[0].height > 1 {
        label_lines.push(Line::from(Span::styled(format!(" {detail}"), Style::default().fg(C_LABEL))));
    }
    frame.render_widget(Paragraph::new(label_lines), cols[0]);

    let spark = Sparkline::default()
        .data(data)
        .bar_set(symbols::bar::NINE_LEVELS)
        .style(Style::default().fg(color));
    frame.render_widget(spark, cols[1]);
}

/// Pad data with leading zeros so values are right-aligned in `width` columns.
fn pad_right_align(data: &[u64], width: usize) -> Vec<u64> {
    let tail_start = data.len().saturating_sub(width);
    let tail = &data[tail_start..];
    let mut padded = vec![0u64; width.saturating_sub(tail.len())];
    padded.extend_from_slice(tail);
    padded
}

// ── Shutdown overlay ─────────────────────────────────────────────

fn draw_shutdown_overlay(frame: &mut ratatui::Frame, area: Rect) {
    let dim = Paragraph::new("").style(Style::default().bg(Color::Black));
    frame.render_widget(dim, area);

    let w = 36u16;
    let h = 3u16;
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    let popup = Rect::new(x, y, w.min(area.width), h.min(area.height));

    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(C_BORDER));
    let text = Paragraph::new(Line::from(vec![
        Span::styled(" Shutting down", Style::default().fg(C_RATE_DROP)),
        Span::styled(" — updating DHT...", Style::default().fg(C_LABEL)),
    ]))
    .alignment(Alignment::Center)
    .block(block);
    frame.render_widget(text, popup);
}

// ── Shared helpers ───────────────────────────────────────────────

fn panel_block(title: &str) -> Block<'static> {
    Block::default()
        .title(Span::styled(
            title.to_string(),
            Style::default().fg(C_TITLE).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(C_BORDER))
}

fn rssi_color(rssi: i16) -> Color {
    if rssi >= -70 { Color::Green } else if rssi >= -90 { C_RATE_DROP } else { C_QUEUE_DROP }
}

fn snr_color(snr: i16) -> Color {
    if snr >= 5 { Color::Green } else if snr >= 0 { C_RATE_DROP } else { C_QUEUE_DROP }
}

fn kv_line(label: &str, value: &str, color: Color) -> Line<'static> {
    Line::from(vec![
        Span::raw(" "),
        Span::styled(format!("{label:<10}"), Style::default().fg(C_LABEL)),
        Span::styled(value.to_string(), Style::default().fg(color)),
    ])
}

fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 { format!("{secs}s") }
    else if secs < 3600 { format!("{}m{}s", secs / 60, secs % 60) }
    else { format!("{}h{}m", secs / 3600, (secs % 3600) / 60) }
}

fn format_short_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 { format!("{secs}s") }
    else if secs < 3600 { format!("{}m", secs / 60) }
    else { format!("{}h", secs / 3600) }
}

fn chrono_time() -> String {
    let now = std::time::SystemTime::now();
    let since_epoch = now.duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let secs = since_epoch.as_secs();
    format!("{:02}:{:02}:{:02}", (secs / 3600) % 24, (secs % 3600) / 60, secs % 60)
}
