//! Async radio task — drives the `donglora-client` [`Dongle`] session.
//!
//! Owns a single tokio task that connects (with exponential backoff reconnect),
//! negotiates config, starts RX, and multiplexes TX requests from the router
//! against the RX stream. TX is dispatched via [`Dongle::tx_with_retry`] so
//! `TX_DONE(CHANNEL_BUSY)` / `EBUSY` errors are retried per spec §6.10 with
//! randomized backoff, and per-attempt state is surfaced to the router (and
//! onwards to the TUI packet log) via the [`RadioEvent::Tx*`] variants.
//!
//! Keepalive pings are handled inside the [`Dongle`] itself — no explicit
//! liveness loop here.

use std::time::Duration;

use anyhow::{Context, Result};
use donglora_client::{
    ConnectOptions, Dongle, LoRaBandwidth, LoRaConfig, Modulation, RetryPolicy, RxPayload, connect_mux_auto_with,
    connect_with,
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use crate::packet::{RadioPacket, snr_grade};

/// How the radio config was resolved.
#[derive(Debug, Clone, Copy)]
pub enum ConfigSource {
    /// We set the config ourselves.
    Ours,
    /// The mux already had a locked config; we adopted it.
    Mux,
}

/// Messages from the radio task to the router.
#[derive(Debug)]
pub enum RadioEvent {
    /// A valid packet received from the radio (already SNR-filtered).
    Packet(RadioPacket),
    /// Radio connected (or reconnected) with the active config and device info.
    Connected(LoRaConfig, ConfigSource, String),
    /// Radio disconnected — attempting reconnect.
    Disconnected,
    /// TX attempt started. `attempt` is 1-indexed.
    TxAttempt { seq: u64, attempt: u8, total_attempts: u8, size: usize },
    /// Previous TX attempt failed and a retry is scheduled.
    TxRetry { seq: u64, attempt_that_failed: u8, total_attempts: u8, reason: TxRetryReason, backoff_ms: u32 },
    /// TX completed successfully (perhaps after retries).
    TxSucceeded { seq: u64, attempts_used: u8, final_airtime_us: u32, size: usize },
    /// TX policy exhausted or errored terminally.
    TxFailed { seq: u64, attempts_used: u8, reason: String, size: usize },
}

/// Why a TX attempt was retried — carried alongside the retry event for
/// the TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxRetryReason {
    /// CAD detected channel activity before TX.
    ChannelBusy,
    /// Firmware TX queue was full.
    QueueFull,
    /// Other retryable condition (future-compat).
    Other,
}

impl std::fmt::Display for TxRetryReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ChannelBusy => write!(f, "channel_busy"),
            Self::QueueFull => write!(f, "queue_full"),
            Self::Other => write!(f, "other"),
        }
    }
}

/// One outbound TX request from the router: the payload plus a sequence id so
/// the event stream can correlate back to the packet log entry.
#[derive(Debug)]
pub struct TxRequest {
    pub seq: u64,
    pub data: Vec<u8>,
}

/// Spawn the async radio task.
///
/// Returns a receiver for radio events and a sender for [`TxRequest`]s.
pub fn spawn(
    config: LoRaConfig,
    port: Option<String>,
    retry_policy: RetryPolicy,
) -> Result<(mpsc::Receiver<RadioEvent>, mpsc::Sender<TxRequest>, JoinHandle<()>)> {
    let (event_tx, event_rx) = mpsc::channel::<RadioEvent>(256);
    let (tx_send, tx_recv) = mpsc::channel::<TxRequest>(64);
    let handle = tokio::spawn(radio_loop(config, port, retry_policy, event_tx, tx_recv));
    Ok((event_rx, tx_send, handle))
}

async fn radio_loop(
    config: LoRaConfig,
    port: Option<String>,
    retry_policy: RetryPolicy,
    event_tx: mpsc::Sender<RadioEvent>,
    mut tx_recv: mpsc::Receiver<TxRequest>,
) {
    let initial_backoff = Duration::from_millis(250);
    let max_backoff = Duration::from_secs(5);
    let mut backoff = initial_backoff;
    let mut stick_to_mux = false;

    loop {
        let started = std::time::Instant::now();
        match connect_and_run(&config, port.as_deref(), stick_to_mux, &retry_policy, &event_tx, &mut tx_recv).await {
            Ok(was_mux) => {
                if event_tx.is_closed() {
                    info!("radio task exiting (router closed)");
                    return;
                }
                // We had a clean run_session exit but not shutdown — loop back.
                if was_mux {
                    stick_to_mux = true;
                }
                debug!("radio session exited cleanly; reconnecting");
            }
            Err((was_mux, e)) => {
                if was_mux {
                    stick_to_mux = true;
                }
                error!("radio error: {e:#}");
                let _ = event_tx.send(RadioEvent::Disconnected).await;

                if started.elapsed() > Duration::from_secs(5) {
                    backoff = initial_backoff;
                }
                info!("reconnecting in {backoff:?}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(max_backoff);
            }
        }
    }
}

async fn connect_and_run(
    config: &LoRaConfig,
    port: Option<&str>,
    stick_to_mux: bool,
    retry_policy: &RetryPolicy,
    event_tx: &mpsc::Sender<RadioEvent>,
    tx_recv: &mut mpsc::Receiver<TxRequest>,
) -> std::result::Result<bool, (bool, anyhow::Error)> {
    info!("connecting to dongle...");
    let mut opts = ConnectOptions::new().config(Modulation::LoRa(*config));
    if let Some(p) = port {
        opts = opts.port(p);
    }

    // Attempt mux-only if we've committed to it, otherwise the normal
    // auto-discovery chain.
    let dongle = if stick_to_mux {
        debug!("reconnecting via mux only (previously connected via mux)");
        connect_mux_auto_with(opts).await.map_err(|e| (true, anyhow::Error::from(e)))?
    } else {
        connect_with(opts).await.map_err(|e| (false, anyhow::Error::from(e)))?
    };

    // Ask the Dongle which transport it actually ended up on. That's
    // authoritative — don't infer from firmware capability bits.
    let kind = dongle.transport_kind().clone();
    let is_mux = kind.is_mux();
    let device = kind.short_label();
    info!("[radio] connected: {device}");

    let outcome = run_session(dongle, config, is_mux, &device, retry_policy, event_tx, tx_recv).await;
    outcome.map(|()| is_mux).map_err(|e| (is_mux, e))
}

async fn run_session(
    dongle: Dongle,
    config: &LoRaConfig,
    is_mux: bool,
    device: &str,
    retry_policy: &RetryPolicy,
    event_tx: &mpsc::Sender<RadioEvent>,
    tx_recv: &mut mpsc::Receiver<TxRequest>,
) -> Result<()> {
    // The Dongle's auto-configure already applied our requested config at
    // connect time (via `ConnectOptions::config`). Read back the active
    // config — on the mux, another client may have locked a different one.
    let active_config = match dongle.config().await {
        Some(Modulation::LoRa(c)) => c,
        Some(other) => {
            warn!("[radio] mux has a non-LoRa config locked ({other:?}); using it anyway");
            // This branch is unreachable today since the bridge only supports
            // LoRa, but we surface a warning rather than hard-fail so the
            // mux doesn't brick the bridge.
            *config
        }
        None => {
            warn!("[radio] Dongle::config() returned None after connect; falling back to requested");
            *config
        }
    };
    let source = if lora_configs_match(&active_config, config) { ConfigSource::Ours } else { ConfigSource::Mux };
    info!("[radio] config: source={source:?}, {}", format_lora_config(&active_config));

    dongle.rx_start().await.context("rx_start failed")?;
    if event_tx.send(RadioEvent::Connected(active_config, source, device.to_string())).await.is_err() {
        return Ok(());
    }

    info!("[radio] entering main loop");

    // TX requests from the router and RX packets from the radio both drive
    // the loop; use tokio::select! to multiplex them.
    loop {
        tokio::select! {
            tx = tx_recv.recv() => {
                let Some(req) = tx else { return Ok(()); };
                handle_tx(&dongle, req, retry_policy, event_tx).await;
            }
            rx = dongle.next_rx() => {
                match rx {
                    Some(pkt) => {
                        handle_rx(pkt, &active_config, event_tx).await;
                    }
                    None => {
                        // Dongle's RX stream ended — session closed.
                        return Ok(());
                    }
                }
            }
        }

        if event_tx.is_closed() {
            return Ok(());
        }

        // Pull any async errors the Dongle accumulated (EFRAME / async
        // ERADIO). These get logged but don't kill the session.
        for err in dongle.drain_async_errors().await {
            warn!("[radio] async error: {err}");
        }

        // Intentionally unused so callers don't think is_mux matters here.
        let _ = is_mux;
    }
}

async fn handle_tx(dongle: &Dongle, req: TxRequest, retry_policy: &RetryPolicy, event_tx: &mpsc::Sender<RadioEvent>) {
    let size = req.data.len();
    let _ = event_tx
        .send(RadioEvent::TxAttempt { seq: req.seq, attempt: 1, total_attempts: retry_policy.max_attempts, size })
        .await;

    let result = dongle.tx_with_retry(&req.data, retry_policy).await;
    match result {
        Ok(outcome) => {
            // Fire a TxRetry event for every attempt beyond the first, so the
            // TUI log can show the escalation.
            let mut prev_attempt: u8 = 0;
            for att in &outcome.attempts {
                if prev_attempt != 0 {
                    let reason = match &outcome.attempts[(prev_attempt - 1) as usize].result {
                        Err(donglora_client::ClientError::ChannelBusy) => TxRetryReason::ChannelBusy,
                        Err(donglora_client::ClientError::Busy) => TxRetryReason::QueueFull,
                        _ => TxRetryReason::Other,
                    };
                    let _ = event_tx
                        .send(RadioEvent::TxRetry {
                            seq: req.seq,
                            attempt_that_failed: prev_attempt,
                            total_attempts: retry_policy.max_attempts,
                            reason,
                            backoff_ms: retry_policy.backoff_ms_min,
                        })
                        .await;
                }
                prev_attempt = att.attempt;
            }
            let _ = event_tx
                .send(RadioEvent::TxSucceeded {
                    seq: req.seq,
                    attempts_used: outcome.attempts_used(),
                    final_airtime_us: outcome.final_airtime_us,
                    size,
                })
                .await;
        }
        Err(e) => {
            warn!("TX #{} failed after retries: {e:#}", req.seq);
            let _ = event_tx
                .send(RadioEvent::TxFailed {
                    seq: req.seq,
                    attempts_used: retry_policy.max_attempts,
                    reason: format!("{e}"),
                    size,
                })
                .await;
        }
    }
}

async fn handle_rx(pkt: RxPayload, active_config: &LoRaConfig, event_tx: &mpsc::Sender<RadioEvent>) {
    // Tenths-of-dB → whole dB for the existing bridge packet format.
    // `rssi_tenths_dbm` and `snr_tenths_db` are i16 and the values stay
    // within i16 after the /10 rounding; the `as i16` is safe.
    #[allow(clippy::cast_possible_truncation)]
    let rssi = (f32::from(pkt.rssi_tenths_dbm) / 10.0).round() as i16;
    #[allow(clippy::cast_possible_truncation)]
    let snr = (f32::from(pkt.snr_tenths_db) / 10.0).round() as i16;

    let grade = snr_grade(snr, active_config.sf);
    if !grade.should_forward() {
        debug!("RX drop {} bytes rssi={rssi} snr={snr} grade={grade}", pkt.data.len());
        return;
    }
    debug!("RX {} bytes rssi={rssi} snr={snr} grade={grade}", pkt.data.len());
    let radio_pkt = RadioPacket { rssi, snr, payload: pkt.data.to_vec() };
    let _ = event_tx.send(RadioEvent::Packet(radio_pkt)).await;
}

// ── Display helpers ───────────────────────────────────────────────

fn lora_configs_match(a: &LoRaConfig, b: &LoRaConfig) -> bool {
    a.freq_hz == b.freq_hz
        && a.bw == b.bw
        && a.sf == b.sf
        && a.cr == b.cr
        && a.sync_word == b.sync_word
        && a.tx_power_dbm == b.tx_power_dbm
        && a.preamble_len == b.preamble_len
        && a.header_mode == b.header_mode
        && a.payload_crc == b.payload_crc
        && a.iq_invert == b.iq_invert
}

/// Format a bandwidth value for display.
#[must_use]
pub const fn format_bandwidth(bw: LoRaBandwidth) -> &'static str {
    match bw {
        LoRaBandwidth::Khz7 => "7.8 kHz",
        LoRaBandwidth::Khz10 => "10.4 kHz",
        LoRaBandwidth::Khz15 => "15.6 kHz",
        LoRaBandwidth::Khz20 => "20.8 kHz",
        LoRaBandwidth::Khz31 => "31.25 kHz",
        LoRaBandwidth::Khz41 => "41.7 kHz",
        LoRaBandwidth::Khz62 => "62.5 kHz",
        LoRaBandwidth::Khz125 => "125 kHz",
        LoRaBandwidth::Khz250 => "250 kHz",
        LoRaBandwidth::Khz500 => "500 kHz",
        LoRaBandwidth::Khz200 => "200 kHz",
        LoRaBandwidth::Khz400 => "400 kHz",
        LoRaBandwidth::Khz800 => "800 kHz",
        LoRaBandwidth::Khz1600 => "1600 kHz",
    }
}

/// Format a `LoRaConfig` for display.
#[must_use]
pub fn format_lora_config(config: &LoRaConfig) -> String {
    let freq_mhz = f64::from(config.freq_hz) / 1_000_000.0;
    let bw = format_bandwidth(config.bw);
    let cr_denom: u8 = match config.cr {
        donglora_client::LoRaCodingRate::Cr4_5 => 5,
        donglora_client::LoRaCodingRate::Cr4_6 => 6,
        donglora_client::LoRaCodingRate::Cr4_7 => 7,
        donglora_client::LoRaCodingRate::Cr4_8 => 8,
    };
    let preamble = config.preamble_len;
    format!(
        "{freq_mhz:.3}MHz SF{} BW{bw} CR4/{cr_denom} SW0x{:04X} TX:{} dBm Pre:{preamble}",
        config.sf, config.sync_word, config.tx_power_dbm,
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use donglora_client::{LoRaCodingRate, LoRaHeaderMode};

    fn default_config() -> LoRaConfig {
        LoRaConfig {
            freq_hz: 910_525_000,
            bw: LoRaBandwidth::Khz62,
            sf: 7,
            cr: LoRaCodingRate::Cr4_5,
            sync_word: 0x1424,
            tx_power_dbm: 20,
            preamble_len: 16,
            header_mode: LoRaHeaderMode::Explicit,
            payload_crc: true,
            iq_invert: false,
        }
    }

    #[test]
    fn lora_configs_match_identical() {
        assert!(lora_configs_match(&default_config(), &default_config()));
    }

    #[test]
    fn lora_configs_match_differs_each_field() {
        let base = default_config();

        let mut c = base;
        c.freq_hz = 915_000_000;
        assert!(!lora_configs_match(&base, &c));

        let mut c = base;
        c.sf = 12;
        assert!(!lora_configs_match(&base, &c));

        let mut c = base;
        c.cr = LoRaCodingRate::Cr4_8;
        assert!(!lora_configs_match(&base, &c));

        let mut c = base;
        c.sync_word = 0x1234;
        assert!(!lora_configs_match(&base, &c));

        let mut c = base;
        c.tx_power_dbm = 10;
        assert!(!lora_configs_match(&base, &c));

        let mut c = base;
        c.preamble_len = 8;
        assert!(!lora_configs_match(&base, &c));
    }

    #[test]
    fn format_bandwidth_all_variants() {
        assert_eq!(format_bandwidth(LoRaBandwidth::Khz7), "7.8 kHz");
        assert_eq!(format_bandwidth(LoRaBandwidth::Khz125), "125 kHz");
        assert_eq!(format_bandwidth(LoRaBandwidth::Khz500), "500 kHz");
    }

    #[test]
    fn format_lora_config_typical() {
        let cfg = default_config();
        let s = format_lora_config(&cfg);
        assert!(s.contains("910.525MHz"));
        assert!(s.contains("SF7"));
        assert!(s.contains("CR4/5"));
        assert!(s.contains("0x1424"));
        assert!(s.contains("20 dBm"));
    }

    #[test]
    fn tx_retry_reason_display() {
        assert_eq!(format!("{}", TxRetryReason::ChannelBusy), "channel_busy");
        assert_eq!(format!("{}", TxRetryReason::QueueFull), "queue_full");
        assert_eq!(format!("{}", TxRetryReason::Other), "other");
    }
}
