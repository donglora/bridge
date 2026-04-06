//! Ratatui terminal user interface.
//!
//! Displays network status, radio configuration, aggregate statistics,
//! a scrolling packet log, and real-time activity sparklines.

use std::collections::VecDeque;
use std::io::{self, Stdout};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::ExecutableCommand;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Sparkline};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::gossip::Gossip;
use crate::radio::ConfigSource;
use crate::router::{PacketAction, PacketDirection, PacketLogEntry, RadioConfigInfo, Stats, StatsSnapshot};

const TICK_RATE: Duration = Duration::from_millis(250);
const MAX_LOG_ENTRIES: usize = 200;
const BUCKET_SECS: u64 = 1;
const MAX_BUCKETS: usize = 300;

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
    buckets: VecDeque<Bucket>,
    last_snap: StatsSnapshot,
    last_bucket_time: Instant,
}

impl TimeSeries {
    fn new(snap: &StatsSnapshot) -> Self {
        Self {
            buckets: VecDeque::from(vec![Bucket::default(); MAX_BUCKETS]),
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
            self.buckets.push_back(bucket);
            if self.buckets.len() > MAX_BUCKETS {
                self.buckets.pop_front();
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
                    let avg = b.snr_sum / b.snr_count.cast_signed();
                    (avg + 30).max(0).cast_unsigned()
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
        self.buckets.iter().rev().find(|b| b.snr_count > 0).map(|b| b.snr_sum / b.snr_count.cast_signed())
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

/// Start the terminal UI event loop. Returns the terminal handle for cleanup on exit.
#[allow(clippy::needless_pass_by_value)] // Arc/CancellationToken are cheaply cloned and owned by this scope
pub fn run(
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

    let mut log_entries: VecDeque<PacketLogEntry> = VecDeque::with_capacity(MAX_LOG_ENTRIES);
    let mut series = TimeSeries::new(&stats.snapshot());

    let _result = tui_loop(
        &mut terminal,
        config_watch,
        &gossip,
        &stats,
        &mut log_rx,
        &cancel,
        start_time,
        &mut log_entries,
        &mut series,
    );

    terminal.draw(|frame| draw_shutdown_overlay(frame, frame.area()))?;
    Ok(terminal)
}

/// Restore the terminal to its normal state (disable raw mode, leave alternate screen).
pub fn restore_terminal(terminal: &mut Term) {
    let _ = disable_raw_mode();
    let _ = io::stdout().execute(LeaveAlternateScreen);
    let _ = terminal.show_cursor();
}

#[allow(clippy::too_many_arguments, clippy::needless_pass_by_value)]
fn tui_loop(
    terminal: &mut Term,
    config_watch: tokio::sync::watch::Receiver<RadioConfigInfo>,
    gossip: &Gossip,
    stats: &Stats,
    log_rx: &mut mpsc::Receiver<PacketLogEntry>,
    cancel: &CancellationToken,
    start_time: Instant,
    log_entries: &mut VecDeque<PacketLogEntry>,
    series: &mut TimeSeries,
) -> anyhow::Result<()> {
    let mut last_tick = Instant::now();
    loop {
        while let Ok(entry) = log_rx.try_recv() {
            if log_entries.len() >= MAX_LOG_ENTRIES {
                log_entries.pop_front();
            }
            log_entries.push_back(entry);
        }
        if cancel.is_cancelled() {
            return Ok(());
        }
        let snap = stats.snapshot();
        let log_slice = log_entries.make_contiguous();
        series.update(&snap, log_slice);
        let swarm_state = gossip.swarm_state();
        let config_info = config_watch.borrow().clone();
        terminal.draw(|frame| {
            draw_ui(frame, frame.area(), &config_info, &snap, &swarm_state, log_slice, start_time, series);
        })?;
        let timeout = TICK_RATE.checked_sub(last_tick.elapsed()).unwrap_or(Duration::ZERO);
        if event::poll(timeout)?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
            && matches!(
                (key.code, key.modifiers),
                (KeyCode::Char('q'), _) | (KeyCode::Char('c'), crossterm::event::KeyModifiers::CONTROL)
            )
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
    // Minimum terminal size for the 3-column layout.
    if area.width < 80 || area.height < 12 {
        let msg = format!("Terminal too small ({}x{}), need 80x12+", area.width, area.height);
        frame.render_widget(
            Paragraph::new(msg).alignment(Alignment::Center).style(Style::default().fg(C_RATE_DROP)),
            area,
        );
        return;
    }

    // Header row + body.
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(area);

    draw_header(frame, rows[0], stats);

    // Body: 3 columns — left panels | packet log (fixed) | activity charts (fill).
    // Packet log columns: Age=5 Hash=8 RF=4 Net=4 Size=6 RSSI=6 SNR=5 Act=5 = 43 + 2 border + 1 pad = 46
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(30), Constraint::Length(46), Constraint::Min(4)])
        .split(rows[1]);

    // Left column: Network, Radio, Stats stacked.
    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(6), Constraint::Length(12), Constraint::Min(5)])
        .split(cols[0]);

    draw_network(frame, left[0], swarm_state);
    draw_radio(frame, left[1], config_info);
    draw_stats(frame, left[2], stats, start_time);
    draw_log(frame, cols[1], log_entries);
    draw_activity(frame, cols[2], series);
}

// ── Header ───────────────────────────────────────────────────────

fn draw_header(frame: &mut ratatui::Frame, area: Rect, stats: &StatsSnapshot) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(14), // pills
            Constraint::Min(0),     // fill
        ])
        .split(area);

    // Left: status pills.
    let pills = Line::from(vec![
        Span::raw(" "),
        status_pill("RADIO", stats.radio_connected),
        Span::raw(" "),
        status_pill("NET", stats.neighbor_count > 0),
    ]);
    frame.render_widget(Paragraph::new(pills), cols[0]);

    // Right: clock with timezone.
    let clock = Line::from(Span::styled(format!("{} ", chrono_time()), Style::default().fg(C_LABEL)));
    frame.render_widget(Paragraph::new(clock).alignment(Alignment::Right), cols[1]);
}

fn status_pill(label: &str, ok: bool) -> Span<'static> {
    let (fg, bg) = if ok { (Color::Black, Color::Green) } else { (Color::White, C_LABEL) };
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
    let last_pub = state.last_dht_publish.map_or_else(|| "-".into(), |t| format!("{}s ago", t.elapsed().as_secs()));

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
    let connected = config_info.connected;
    let is_mux = matches!(config_info.source, ConfigSource::Mux);

    let border_color = if !connected {
        C_QUEUE_DROP
    } else if is_mux {
        C_RATE_DROP
    } else {
        C_BORDER
    };

    let title = if !connected {
        " Radio (disconnected) "
    } else if is_mux {
        " Radio (mux) "
    } else {
        " Radio "
    };

    let block = Block::default()
        .title(Span::styled(title, Style::default().fg(C_TITLE).add_modifier(Modifier::BOLD)))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

    let a = &config_info.active;
    let r = &config_info.requested;

    let freq_mhz = f64::from(a.freq_hz) / 1_000_000.0;
    let bw_str = crate::radio::format_bandwidth(a.bw);
    let power_str = if a.tx_power_dbm == donglora_client::TX_POWER_MAX {
        "max".to_string()
    } else {
        format!("{} dBm", a.tx_power_dbm)
    };
    let preamble = if a.preamble_len == 0 { 16 } else { a.preamble_len };
    let cad = if a.cad != 0 { "on" } else { "off" };

    // Dim values when disconnected; highlight mux mismatches when connected.
    let val_color = if connected { C_ACCENT } else { C_LABEL };
    let cf = |label: &str, value: String, matches: bool| -> Line<'static> {
        let color = if !connected {
            C_LABEL
        } else if !is_mux || matches {
            C_ACCENT
        } else {
            C_RATE_DROP
        };
        let mark = if connected && is_mux && !matches { " !" } else { "" };
        kv_line(label, &format!("{value}{mark}"), color)
    };

    let dev = if config_info.device.is_empty() { "-" } else { &config_info.device };
    let lines = vec![
        kv_line("Device", dev, val_color),
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

#[allow(clippy::option_if_let_else)] // match is more readable for RSSI/SNR formatting
fn draw_log(frame: &mut ratatui::Frame, area: Rect, entries: &[PacketLogEntry]) {
    let block = panel_block(" Packet Log ");

    let inner_height = area.height.saturating_sub(2) as usize;
    let data_height = inner_height.saturating_sub(1); // header row
    let start = entries.len().saturating_sub(data_height);
    let visible = &entries[start..];

    // Fixed column widths: Age=5 Hash=8 RF=4 Net=4 Size=6 RSSI=6 SNR=5 Act=5
    // Every span (header and data) uses the same width per column.
    let hdr = Style::default().fg(Color::Gray).add_modifier(Modifier::BOLD);
    let header = Line::from(vec![
        Span::styled(format!("{:>5}", "Age"), hdr),
        Span::styled(format!("{:>8}", "Hash"), hdr),
        Span::styled(format!("{:>4}", "RF"), hdr),
        Span::styled(format!("{:>4}", "Net"), hdr),
        Span::styled(format!("{:>6}", "Size"), hdr),
        Span::styled(format!("{:>6}", "RSSI"), hdr),
        Span::styled(format!("{:>5}", "SNR"), hdr),
        Span::styled(format!("{:>5}", "Act"), hdr),
    ]);

    let mut lines = vec![header];
    let mut prev_hash: Option<[u8; 32]> = None;

    for e in visible {
        let age = format_short_duration(e.timestamp.elapsed());
        let bridged = matches!(e.action, PacketAction::Bridged);

        // Show short hash, or ellipsis if same as previous line.
        let hash_span = if prev_hash == Some(e.hash) {
            Span::styled(format!("{:>8}", "··"), Style::default().fg(C_BORDER))
        } else {
            Span::styled(format!("{:>8}", hex::encode(&e.hash[..3])), Style::default().fg(C_LABEL))
        };
        prev_hash = Some(e.hash);

        // Each row shows both columns. Bridged packets light up both sides.
        // Dropped packets only show the input side.
        let dash = Span::styled(format!("{:>4}", "──"), Style::default().fg(C_BORDER));
        let (rf_col, net_col) = match e.direction {
            PacketDirection::RadioIn => (
                Span::styled(format!("{:>4}", "RX"), Style::default().fg(C_RADIO_RX)),
                if bridged { Span::styled(format!("{:>4}", "TX"), Style::default().fg(C_NET_TX)) } else { dash },
            ),
            PacketDirection::GossipIn => (
                if bridged { Span::styled(format!("{:>4}", "TX"), Style::default().fg(C_RADIO_TX)) } else { dash },
                Span::styled(format!("{:>4}", "RX"), Style::default().fg(C_NET_RX)),
            ),
        };

        let action_color = match e.action {
            PacketAction::Bridged => C_LABEL,
            PacketAction::DroppedDedup => C_DEDUP,
            PacketAction::DroppedQueueFull => C_QUEUE_DROP,
            PacketAction::DroppedRateLimit => C_RATE_DROP,
        };

        let rssi_span = match e.rssi {
            Some(r) => Span::styled(format!("{r:>6}"), Style::default().fg(rssi_color(r))),
            None => Span::styled(format!("{:>6}", "-"), Style::default().fg(C_LABEL)),
        };
        let snr_span = match e.snr {
            Some(s) => Span::styled(format!("{s:>5}"), Style::default().fg(snr_color(s))),
            None => Span::styled(format!("{:>5}", "-"), Style::default().fg(C_LABEL)),
        };

        let action_str = match e.action {
            PacketAction::Bridged => "",
            PacketAction::DroppedDedup => "DUP",
            PacketAction::DroppedQueueFull => "FULL",
            PacketAction::DroppedRateLimit => "RATE",
        };
        let action_span = Span::styled(format!(" {action_str:>4}"), Style::default().fg(action_color));

        let size_str = format!("{}B", e.size);
        lines.push(Line::from(vec![
            Span::styled(format!("{age:>5}"), Style::default().fg(C_LABEL)),
            hash_span,
            rf_col,
            net_col,
            Span::styled(format!("{size_str:>6}"), Style::default().fg(C_VALUE)),
            rssi_span,
            snr_span,
            action_span,
        ]));
    }

    frame.render_widget(Paragraph::new(lines).block(block), area);
}

// ── Activity chart ───────────────────────────────────────────────

fn draw_activity(frame: &mut ratatui::Frame, area: Rect, series: &TimeSeries) {
    let block = panel_block(" Activity (1s) ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height < 6 || inner.width < 6 {
        return;
    }

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Ratio(1, 3), Constraint::Ratio(1, 3), Constraint::Ratio(1, 3)])
        .split(inner);

    let spark_w = inner.width as usize;

    let rx_data = pad_right_align(&series.radio_rx(), spark_w);
    let tx_data = pad_right_align(&series.radio_tx(), spark_w);
    let snr_data = pad_right_align(&series.avg_snr(), spark_w);

    let peak_rx = series.peak_rx();
    let peak_tx = series.peak_tx();
    let snr_label = series.latest_avg_snr().map_or_else(|| "-".into(), |s| format!("{s}dB"));

    draw_sparkline_section(frame, rows[0], "RX", &format!("pk:{peak_rx}"), &rx_data, C_RADIO_RX);
    draw_sparkline_section(frame, rows[1], "TX", &format!("pk:{peak_tx}"), &tx_data, C_RADIO_TX);
    draw_sparkline_section(frame, rows[2], "SNR", &snr_label, &snr_data, C_SNR);
}

/// Draw a label line on top, sparkline filling the rest below.
fn draw_sparkline_section(
    frame: &mut ratatui::Frame,
    area: Rect,
    label: &str,
    detail: &str,
    data: &[u64],
    color: Color,
) {
    if area.height < 2 || area.width < 4 {
        return;
    }

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(area);

    let label_line = Line::from(vec![
        Span::styled(format!(" {label} "), Style::default().fg(color).add_modifier(Modifier::BOLD)),
        Span::styled(detail.to_string(), Style::default().fg(C_LABEL)),
    ]);
    frame.render_widget(Paragraph::new(label_line), rows[0]);

    let spark = Sparkline::default().data(data).bar_set(symbols::bar::NINE_LEVELS).style(Style::default().fg(color));
    frame.render_widget(spark, rows[1]);
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
    let block = Block::default().borders(Borders::ALL).border_style(Style::default().fg(C_BORDER));
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
        .title(Span::styled(title.to_string(), Style::default().fg(C_TITLE).add_modifier(Modifier::BOLD)))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(C_BORDER))
}

const fn rssi_color(rssi: i16) -> Color {
    if rssi >= -70 {
        Color::Green
    } else if rssi >= -90 {
        C_RATE_DROP
    } else {
        C_QUEUE_DROP
    }
}

const fn snr_color(snr: i16) -> Color {
    if snr >= 5 {
        Color::Green
    } else if snr >= 0 {
        C_RATE_DROP
    } else {
        C_QUEUE_DROP
    }
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
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
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
    jiff::Zoned::now().strftime("%H:%M:%S %Z").to_string()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    // ── Helpers ──────────────────────────────────────────────────

    fn make_snapshot(radio_rx: u64, radio_tx: u64) -> StatsSnapshot {
        StatsSnapshot {
            radio_rx,
            radio_tx,
            gossip_rx: 0,
            gossip_tx: 0,
            dedup_hits: 0,
            rate_limit_drops: 0,
            dropped_queue: 0,
            neighbor_count: 0,
            radio_connected: true,
        }
    }

    fn make_log_entry(timestamp: Instant, snr: Option<i16>) -> PacketLogEntry {
        PacketLogEntry {
            timestamp,
            hash: [0u8; 32],
            direction: PacketDirection::RadioIn,
            size: 10,
            snr,
            rssi: Some(-80),
            action: PacketAction::Bridged,
        }
    }

    // ── format_duration tests ───────────────────────────────────

    #[test]
    fn format_duration_seconds() {
        assert_eq!(format_duration(Duration::from_secs(45)), "45s");
    }

    #[test]
    fn format_duration_minutes() {
        assert_eq!(format_duration(Duration::from_secs(125)), "2m5s");
    }

    #[test]
    fn format_duration_hours() {
        assert_eq!(format_duration(Duration::from_secs(7500)), "2h5m");
    }

    #[test]
    fn format_duration_zero() {
        assert_eq!(format_duration(Duration::ZERO), "0s");
    }

    #[test]
    fn format_duration_boundary_59s() {
        assert_eq!(format_duration(Duration::from_secs(59)), "59s");
    }

    #[test]
    fn format_duration_boundary_60s() {
        assert_eq!(format_duration(Duration::from_secs(60)), "1m0s");
    }

    #[test]
    fn format_duration_boundary_3599s() {
        assert_eq!(format_duration(Duration::from_secs(3599)), "59m59s");
    }

    #[test]
    fn format_duration_boundary_3600s() {
        assert_eq!(format_duration(Duration::from_secs(3600)), "1h0m");
    }

    // ── format_short_duration tests ─────────────────────────────

    #[test]
    fn format_short_duration_rounds() {
        assert_eq!(format_short_duration(Duration::from_secs(30)), "30s");
        assert_eq!(format_short_duration(Duration::from_secs(90)), "1m");
        assert_eq!(format_short_duration(Duration::from_secs(7500)), "2h");
    }

    #[test]
    fn format_short_duration_boundary_59s() {
        assert_eq!(format_short_duration(Duration::from_secs(59)), "59s");
    }

    #[test]
    fn format_short_duration_boundary_60s() {
        assert_eq!(format_short_duration(Duration::from_secs(60)), "1m");
    }

    #[test]
    fn format_short_duration_boundary_3599s() {
        assert_eq!(format_short_duration(Duration::from_secs(3599)), "59m");
    }

    #[test]
    fn format_short_duration_boundary_3600s() {
        assert_eq!(format_short_duration(Duration::from_secs(3600)), "1h");
    }

    // ── chrono_time tests ───────────────────────────────────────

    #[test]
    fn chrono_time_returns_non_empty_string() {
        let t = chrono_time();
        assert!(!t.is_empty(), "chrono_time() should not be empty");
    }

    #[test]
    fn chrono_time_contains_colons() {
        // HH:MM:SS format should have at least two colons
        let t = chrono_time();
        assert!(t.matches(':').count() >= 2, "chrono_time() should contain HH:MM:SS, got: {t}");
    }

    // ── rssi_color tests ────────────────────────────────────────

    #[test]
    fn rssi_color_green_at_minus_70() {
        assert_eq!(rssi_color(-70), Color::Green);
    }

    #[test]
    fn rssi_color_yellow_at_minus_71() {
        assert_eq!(rssi_color(-71), C_RATE_DROP);
    }

    #[test]
    fn rssi_color_yellow_at_minus_90() {
        assert_eq!(rssi_color(-90), C_RATE_DROP);
    }

    #[test]
    fn rssi_color_red_at_minus_91() {
        assert_eq!(rssi_color(-91), C_QUEUE_DROP);
    }

    #[test]
    fn rssi_color_green_at_zero() {
        assert_eq!(rssi_color(0), Color::Green);
    }

    // ── snr_color tests ─────────────────────────────────────────

    #[test]
    fn snr_color_green_at_5() {
        assert_eq!(snr_color(5), Color::Green);
    }

    #[test]
    fn snr_color_yellow_at_4() {
        assert_eq!(snr_color(4), C_RATE_DROP);
    }

    #[test]
    fn snr_color_yellow_at_0() {
        assert_eq!(snr_color(0), C_RATE_DROP);
    }

    #[test]
    fn snr_color_red_at_minus_1() {
        assert_eq!(snr_color(-1), C_QUEUE_DROP);
    }

    #[test]
    fn snr_color_green_at_high_value() {
        assert_eq!(snr_color(20), Color::Green);
    }

    // ── snr_stats_since tests ───────────────────────────────────

    #[test]
    fn snr_stats_since_empty() {
        assert_eq!(snr_stats_since(&[], Instant::now()), (0, 0));
    }

    #[test]
    fn snr_stats_since_sums_and_counts_correctly() {
        let now = Instant::now();
        let entries = vec![
            make_log_entry(now + Duration::from_secs(1), Some(10)),
            make_log_entry(now + Duration::from_secs(2), Some(20)),
            make_log_entry(now + Duration::from_secs(3), Some(-5)),
        ];
        let (sum, count) = snr_stats_since(&entries, now);
        assert_eq!(sum, 25); // 10 + 20 + (-5)
        assert_eq!(count, 3);
    }

    #[test]
    fn snr_stats_since_respects_cutoff() {
        let now = Instant::now();
        let entries = vec![
            make_log_entry(now + Duration::from_secs(1), Some(10)),
            make_log_entry(now + Duration::from_secs(2), Some(20)),
            make_log_entry(now + Duration::from_secs(5), Some(30)),
        ];
        // Use a cutoff after the first two entries
        let cutoff = now + Duration::from_secs(3);
        let (sum, count) = snr_stats_since(&entries, cutoff);
        assert_eq!(sum, 30);
        assert_eq!(count, 1);
    }

    #[test]
    fn snr_stats_since_skips_none_snr() {
        let now = Instant::now();
        let entries = vec![
            make_log_entry(now + Duration::from_secs(1), Some(10)),
            make_log_entry(now + Duration::from_secs(2), None),
            make_log_entry(now + Duration::from_secs(3), Some(20)),
        ];
        let (sum, count) = snr_stats_since(&entries, now);
        assert_eq!(sum, 30); // 10 + 20, skipping None
        assert_eq!(count, 2);
    }

    #[test]
    fn snr_stats_since_all_before_cutoff() {
        let now = Instant::now();
        let entries = vec![make_log_entry(now, Some(10)), make_log_entry(now, Some(20))];
        // Cutoff at `now` means entries at exactly `now` are <= since, so excluded
        let (sum, count) = snr_stats_since(&entries, now);
        assert_eq!(sum, 0);
        assert_eq!(count, 0);
    }

    #[test]
    fn snr_stats_since_all_none_snr() {
        let now = Instant::now();
        let entries = vec![
            make_log_entry(now + Duration::from_secs(1), None),
            make_log_entry(now + Duration::from_secs(2), None),
        ];
        let (sum, count) = snr_stats_since(&entries, now);
        assert_eq!(sum, 0);
        assert_eq!(count, 0);
    }

    // ── TimeSeries tests ────────────────────────────────────────

    #[test]
    fn time_series_new_has_max_buckets() {
        let snap = make_snapshot(0, 0);
        let ts = TimeSeries::new(&snap);
        assert_eq!(ts.buckets.len(), MAX_BUCKETS);
    }

    #[test]
    fn time_series_radio_rx_returns_bucket_data() {
        let snap = make_snapshot(0, 0);
        let mut ts = TimeSeries::new(&snap);
        // Manually push a bucket with known data
        ts.buckets.push_back(Bucket { radio_rx: 42, radio_tx: 0, snr_sum: 0, snr_count: 0 });
        let rx = ts.radio_rx();
        assert_eq!(*rx.last().unwrap(), 42);
    }

    #[test]
    fn time_series_radio_tx_returns_bucket_data() {
        let snap = make_snapshot(0, 0);
        let mut ts = TimeSeries::new(&snap);
        ts.buckets.push_back(Bucket { radio_rx: 0, radio_tx: 99, snr_sum: 0, snr_count: 0 });
        let tx = ts.radio_tx();
        assert_eq!(*tx.last().unwrap(), 99);
    }

    #[test]
    fn time_series_avg_snr_with_data() {
        let snap = make_snapshot(0, 0);
        let mut ts = TimeSeries::new(&snap);
        // Push bucket: snr_sum=10, snr_count=2 -> avg=5 -> shifted = 5+30 = 35
        ts.buckets.push_back(Bucket { radio_rx: 0, radio_tx: 0, snr_sum: 10, snr_count: 2 });
        let snr = ts.avg_snr();
        assert_eq!(*snr.last().unwrap(), 35);
    }

    #[test]
    fn time_series_avg_snr_zero_count() {
        let snap = make_snapshot(0, 0);
        let ts = TimeSeries::new(&snap);
        // All default buckets have snr_count=0, so avg_snr should be all zeros
        let snr = ts.avg_snr();
        assert!(snr.iter().all(|&v| v == 0));
    }

    #[test]
    fn time_series_avg_snr_negative_clamps_to_zero() {
        let snap = make_snapshot(0, 0);
        let mut ts = TimeSeries::new(&snap);
        // avg = -50, shifted = -50+30 = -20, clamped to 0
        ts.buckets.push_back(Bucket { radio_rx: 0, radio_tx: 0, snr_sum: -50, snr_count: 1 });
        let snr = ts.avg_snr();
        assert_eq!(*snr.last().unwrap(), 0);
    }

    #[test]
    fn time_series_peak_rx() {
        let snap = make_snapshot(0, 0);
        let mut ts = TimeSeries::new(&snap);
        ts.buckets.push_back(Bucket { radio_rx: 5, radio_tx: 0, snr_sum: 0, snr_count: 0 });
        ts.buckets.push_back(Bucket { radio_rx: 15, radio_tx: 0, snr_sum: 0, snr_count: 0 });
        ts.buckets.push_back(Bucket { radio_rx: 3, radio_tx: 0, snr_sum: 0, snr_count: 0 });
        assert_eq!(ts.peak_rx(), 15);
    }

    #[test]
    fn time_series_peak_tx() {
        let snap = make_snapshot(0, 0);
        let mut ts = TimeSeries::new(&snap);
        ts.buckets.push_back(Bucket { radio_rx: 0, radio_tx: 7, snr_sum: 0, snr_count: 0 });
        ts.buckets.push_back(Bucket { radio_rx: 0, radio_tx: 22, snr_sum: 0, snr_count: 0 });
        assert_eq!(ts.peak_tx(), 22);
    }

    #[test]
    fn time_series_peak_rx_default_is_zero() {
        let snap = make_snapshot(0, 0);
        let ts = TimeSeries::new(&snap);
        assert_eq!(ts.peak_rx(), 0);
    }

    #[test]
    fn time_series_peak_tx_default_is_zero() {
        let snap = make_snapshot(0, 0);
        let ts = TimeSeries::new(&snap);
        assert_eq!(ts.peak_tx(), 0);
    }

    #[test]
    fn time_series_latest_avg_snr_with_data() {
        let snap = make_snapshot(0, 0);
        let mut ts = TimeSeries::new(&snap);
        // Push an older bucket with different SNR
        ts.buckets.push_back(Bucket { radio_rx: 0, radio_tx: 0, snr_sum: 100, snr_count: 10 });
        // Push the latest bucket with known SNR: sum=20, count=4 -> avg=5
        ts.buckets.push_back(Bucket { radio_rx: 0, radio_tx: 0, snr_sum: 20, snr_count: 4 });
        assert_eq!(ts.latest_avg_snr(), Some(5));
    }

    #[test]
    fn time_series_latest_avg_snr_none_when_no_data() {
        let snap = make_snapshot(0, 0);
        let ts = TimeSeries::new(&snap);
        // All default buckets have snr_count=0
        assert_eq!(ts.latest_avg_snr(), None);
    }

    #[test]
    fn time_series_latest_avg_snr_skips_trailing_empty() {
        let snap = make_snapshot(0, 0);
        let mut ts = TimeSeries::new(&snap);
        // A bucket with data followed by an empty bucket
        ts.buckets.push_back(Bucket { radio_rx: 0, radio_tx: 0, snr_sum: -12, snr_count: 3 });
        ts.buckets.push_back(Bucket::default()); // snr_count = 0
        // latest_avg_snr scans backwards, skips the empty one, finds -12/3 = -4
        assert_eq!(ts.latest_avg_snr(), Some(-4));
    }

    #[test]
    fn time_series_update_creates_bucket() {
        let snap0 = make_snapshot(10, 5);
        let mut ts = TimeSeries::new(&snap0);
        // Force the last_bucket_time to be old enough to trigger a new bucket
        ts.last_bucket_time = Instant::now().checked_sub(Duration::from_secs(BUCKET_SECS + 1)).unwrap();
        let initial_len = ts.buckets.len();

        let snap1 = make_snapshot(15, 8);
        let entries = vec![make_log_entry(Instant::now(), Some(10))];
        ts.update(&snap1, &entries);

        // A new bucket should have been pushed
        assert!(ts.buckets.len() >= initial_len);
        let last = ts.buckets.back().unwrap();
        assert_eq!(last.radio_rx, 5); // 15 - 10
        assert_eq!(last.radio_tx, 3); // 8 - 5
    }

    #[test]
    fn time_series_update_does_not_create_bucket_when_interval_not_elapsed() {
        let snap0 = make_snapshot(10, 5);
        let mut ts = TimeSeries::new(&snap0);
        // last_bucket_time is now, so interval has NOT elapsed
        let len_before = ts.buckets.len();

        let snap1 = make_snapshot(20, 10);
        ts.update(&snap1, &[]);

        // No new bucket should be added
        assert_eq!(ts.buckets.len(), len_before);
    }

    #[test]
    fn time_series_update_rotates_when_exceeding_max() {
        let snap0 = make_snapshot(0, 0);
        let mut ts = TimeSeries::new(&snap0);
        assert_eq!(ts.buckets.len(), MAX_BUCKETS);

        // Force old timestamp so update triggers
        ts.last_bucket_time = Instant::now().checked_sub(Duration::from_secs(BUCKET_SECS + 1)).unwrap();
        let snap1 = make_snapshot(1, 1);
        ts.update(&snap1, &[]);

        // Should still be at MAX_BUCKETS (the front was popped)
        assert_eq!(ts.buckets.len(), MAX_BUCKETS);
        // The last bucket should be our new one
        let last = ts.buckets.back().unwrap();
        assert_eq!(last.radio_rx, 1);
    }

    #[test]
    fn time_series_update_no_pop_at_exactly_max() {
        // Start with MAX_BUCKETS - 1 entries. Push one to reach exactly MAX_BUCKETS.
        // With `> MAX_BUCKETS`: 300 is NOT > 300, so no pop → len stays 300. (correct)
        // With `>= MAX_BUCKETS`: 300 IS >= 300, so pop → len becomes 299. (wrong)
        let snap0 = make_snapshot(0, 0);
        let mut ts = TimeSeries::new(&snap0);
        ts.buckets.pop_front(); // now MAX_BUCKETS - 1
        assert_eq!(ts.buckets.len(), MAX_BUCKETS - 1);

        ts.last_bucket_time = Instant::now().checked_sub(Duration::from_secs(BUCKET_SECS + 1)).unwrap();
        let snap1 = make_snapshot(1, 0);
        ts.update(&snap1, &[]);

        assert_eq!(ts.buckets.len(), MAX_BUCKETS, "should be exactly MAX_BUCKETS, not popped");
    }

    // ── pad_right_align tests ───────────────────────────────────

    #[test]
    fn pad_right_align_shorter_than_width() {
        assert_eq!(pad_right_align(&[1, 2, 3], 5), vec![0, 0, 1, 2, 3]);
    }

    #[test]
    fn pad_right_align_longer_than_width() {
        assert_eq!(pad_right_align(&[1, 2, 3, 4, 5], 3), vec![3, 4, 5]);
    }

    #[test]
    fn pad_right_align_exact() {
        assert_eq!(pad_right_align(&[1, 2, 3], 3), vec![1, 2, 3]);
    }

    #[test]
    fn pad_right_align_empty() {
        assert_eq!(pad_right_align(&[], 3), vec![0, 0, 0]);
    }

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn pad_right_align_output_length(
                data in proptest::collection::vec(0u64..100, 0..500),
                width in 1usize..200,
            ) {
                let result = pad_right_align(&data, width);
                prop_assert_eq!(result.len(), width);
            }
        }
    }
}
