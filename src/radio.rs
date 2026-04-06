//! `DongLoRa` hardware communication on a blocking thread.
//!
//! Handles connection, config negotiation, RX/TX, liveness pings, and
//! exponential backoff reconnection.

use std::time::Duration;

use anyhow::{Context, Result};
use donglora_client::{RadioConfig, Response, Transport};
use tokio::sync::mpsc;
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

/// Messages from the radio thread to the router.
pub enum RadioEvent {
    /// A valid packet received from the radio (already SNR-filtered).
    Packet(RadioPacket),
    /// Radio connected (or reconnected) with the active config and device info.
    Connected(RadioConfig, ConfigSource, String),
    /// Radio disconnected — attempting reconnect.
    Disconnected,
}

/// Spawn the blocking radio thread.
///
/// Returns a receiver for radio events and a sender for TX payloads.
pub fn spawn(config: RadioConfig, port: Option<String>) -> Result<(mpsc::Receiver<RadioEvent>, mpsc::Sender<Vec<u8>>)> {
    let (event_tx, event_rx) = mpsc::channel::<RadioEvent>(256);
    let (tx_send, tx_recv) = mpsc::channel::<Vec<u8>>(64);

    std::thread::Builder::new()
        .name("radio".into())
        .spawn(move || radio_loop(config, port, event_tx, tx_recv))
        .context("failed to spawn radio thread")?;

    Ok((event_rx, tx_send))
}

#[allow(clippy::needless_pass_by_value)] // values moved across thread boundary in spawn()
fn radio_loop(
    config: RadioConfig,
    port: Option<String>,
    event_tx: mpsc::Sender<RadioEvent>,
    mut tx_recv: mpsc::Receiver<Vec<u8>>,
) {
    let initial_backoff = Duration::from_millis(250);
    let max_backoff = Duration::from_secs(5);
    let mut backoff = initial_backoff;

    loop {
        let started = std::time::Instant::now();
        match connect_and_run(&config, port.as_deref(), &event_tx, &mut tx_recv) {
            Ok(()) => {
                info!("radio thread exiting");
                return;
            }
            Err(e) => {
                error!("radio error: {e:#}");
                let _ = event_tx.blocking_send(RadioEvent::Disconnected);

                // If we were connected for a while, reset backoff so
                // the next reconnect attempt is fast.
                if started.elapsed() > Duration::from_secs(5) {
                    backoff = initial_backoff;
                }

                info!("reconnecting in {backoff:?}");
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(max_backoff);
            }
        }
    }
}

fn connect_and_run(
    config: &RadioConfig,
    port: Option<&str>,
    event_tx: &mpsc::Sender<RadioEvent>,
    tx_recv: &mut mpsc::Receiver<Vec<u8>>,
) -> Result<()> {
    // Long initial timeout: the mux serializes commands through the dongle,
    // so our `GetConfig` might have to wait behind other clients' commands.
    let timeout = Duration::from_secs(10);

    info!("connecting to dongle...");
    let (mut client, device) = if let Some(port) = port {
        let c = donglora_client::connect(Some(port), timeout)?;
        (c, shorten_path(port))
    } else if let Ok(c) = donglora_client::connect_mux_auto(timeout) {
        (c, "mux".to_string())
    } else {
        let port_path = donglora_client::find_port().ok_or_else(|| anyhow::anyhow!("no mux or USB dongle found"))?;
        info!("no mux found, connecting to serial port {port_path}");
        let c = donglora_client::connect(Some(&port_path), timeout)?;
        (c, shorten_path(&port_path))
    };
    info!("[radio] transport connected: {device}");

    info!("[radio] sending ping...");
    match client.ping() {
        Ok(()) => info!("[radio] ping OK"),
        Err(e) => {
            info!("[radio] ping FAILED: {e:#}");
            return Err(e);
        }
    }

    info!("[radio] negotiating config...");
    let (active_config, config_source) = match negotiate_config(&mut client, config, &device) {
        Ok(result) => {
            info!("[radio] config negotiated: source={:?}, config={}", result.1, format_radio_config(&result.0));
            result
        }
        Err(e) => {
            info!("[radio] config negotiation FAILED: {e:#}");
            return Err(e);
        }
    };

    info!("[radio] sending start_rx...");
    match client.start_rx() {
        Ok(()) => info!("[radio] start_rx OK"),
        Err(e) => {
            info!("[radio] start_rx FAILED: {e:#}");
            return Err(e);
        }
    }

    let _ = event_tx.blocking_send(RadioEvent::Connected(active_config, config_source, device));

    // Set a short timeout so we can interleave RX and TX.
    client.transport_mut().set_timeout(Duration::from_millis(100))?;
    info!("[radio] entering main loop");

    // Liveness: ping the mux periodically when idle to detect a dead connection.
    // Without this, a killed mux causes recv() to silently return Ok(None) forever.
    let liveness_interval = Duration::from_secs(2);
    let mut last_activity = std::time::Instant::now();

    loop {
        // Check for TX requests (non-blocking).
        while let Ok(payload) = tx_recv.try_recv() {
            debug!("TX {} bytes", payload.len());
            if let Err(e) = client.transmit(&payload, None) {
                warn!("transmit error: {e:#}");
                return Err(e);
            }
            last_activity = std::time::Instant::now();
        }

        // Check for RX packets.
        match client.recv() {
            Ok(Some(Response::RxPacket { rssi, snr, payload })) => {
                last_activity = std::time::Instant::now();
                let grade = snr_grade(snr, active_config.sf);
                if grade.should_forward() {
                    debug!("RX {} bytes rssi={rssi} snr={snr} grade={grade}", payload.len());
                    let pkt = RadioPacket { rssi, snr, payload };
                    if event_tx.blocking_send(RadioEvent::Packet(pkt)).is_err() {
                        return Ok(());
                    }
                } else {
                    debug!("RX drop {} bytes rssi={rssi} snr={snr} grade={grade}", payload.len());
                }
            }
            Ok(Some(_)) => {
                last_activity = std::time::Instant::now();
            }
            Ok(None) => {}
            Err(e) if is_timeout_error(&e) => {}
            Err(e) => return Err(e),
        }

        // Liveness check: if no real activity for a while, ping the mux.
        if last_activity.elapsed() >= liveness_interval {
            // Temporarily extend the timeout for the ping command.
            let _ = client.transport_mut().set_timeout(Duration::from_secs(2));
            match client.ping() {
                Ok(()) => {
                    last_activity = std::time::Instant::now();
                }
                Err(e) => {
                    warn!("liveness ping failed: {e:#}");
                    return Err(e);
                }
            }
            // Restore the short timeout.
            let _ = client.transport_mut().set_timeout(Duration::from_millis(100));
        }

        if event_tx.is_closed() {
            return Ok(());
        }
    }
}

/// Negotiate radio config with the dongle/mux.
///
/// Strategy:
/// 1. `GetConfig` first to read what's active.
/// 2. If it matches our desired config, use it — no `SetConfig` needed.
/// 3. If it differs and we're on the mux, accept the mux's config.
/// 4. If it differs and we're on direct serial, `SetConfig` to push ours.
/// 5. If `GetConfig` fails, try `SetConfig` with our desired config.
fn negotiate_config<T: Transport>(
    client: &mut donglora_client::Client<T>,
    desired: &RadioConfig,
    device: &str,
) -> Result<(RadioConfig, ConfigSource)> {
    info!("[negotiate] get_config...");
    match client.get_config() {
        Ok(cfg) => {
            if configs_match(&cfg, desired) {
                info!("[negotiate] get_config OK (matches desired): {}", format_radio_config(&cfg));
                Ok((cfg, ConfigSource::Ours))
            } else if device == "mux" {
                info!("[negotiate] get_config OK (mux config differs, accepting): {}", format_radio_config(&cfg));
                Ok((cfg, ConfigSource::Mux))
            } else {
                info!("[negotiate] get_config OK (serial config differs, setting ours)");
                client.set_config(*desired)?;
                info!("[negotiate] set_config OK");
                Ok((*desired, ConfigSource::Ours))
            }
        }
        Err(e) => {
            info!("[negotiate] get_config FAILED: {e:#}");
            // No config on radio — try to set ours.
            info!("[negotiate] trying set_config...");
            match client.set_config(*desired) {
                Ok(()) => {
                    info!("[negotiate] set_config OK");
                    Ok((*desired, ConfigSource::Ours))
                }
                Err(e2) => {
                    info!("[negotiate] set_config FAILED: {e2:#}");
                    anyhow::bail!("GetConfig failed ({e:#}) and SetConfig also failed ({e2:#})");
                }
            }
        }
    }
}

/// Shorten a device path to just the filename for display.
fn shorten_path(path: &str) -> String {
    std::path::Path::new(path).file_name().map_or_else(|| path.to_string(), |f| f.to_string_lossy().into_owned())
}

fn configs_match(a: &RadioConfig, b: &RadioConfig) -> bool {
    a.freq_hz == b.freq_hz
        && a.bw == b.bw
        && a.sf == b.sf
        && a.cr == b.cr
        && a.sync_word == b.sync_word
        && a.tx_power_dbm == b.tx_power_dbm
        && a.preamble_len == b.preamble_len
        && a.cad == b.cad
}

fn is_timeout_error(e: &anyhow::Error) -> bool {
    e.downcast_ref::<std::io::Error>()
        .is_some_and(|io_err| matches!(io_err.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut))
}

/// Format a bandwidth value for display.
#[must_use]
pub const fn format_bandwidth(bw: donglora_client::Bandwidth) -> &'static str {
    match bw {
        donglora_client::Bandwidth::Khz7 => "7.8 kHz",
        donglora_client::Bandwidth::Khz10 => "10.4 kHz",
        donglora_client::Bandwidth::Khz15 => "15.6 kHz",
        donglora_client::Bandwidth::Khz20 => "20.8 kHz",
        donglora_client::Bandwidth::Khz31 => "31.25 kHz",
        donglora_client::Bandwidth::Khz41 => "41.7 kHz",
        donglora_client::Bandwidth::Khz62 => "62.5 kHz",
        donglora_client::Bandwidth::Khz125 => "125 kHz",
        donglora_client::Bandwidth::Khz250 => "250 kHz",
        donglora_client::Bandwidth::Khz500 => "500 kHz",
    }
}

/// Format a `RadioConfig` for display.
#[must_use]
pub fn format_radio_config(config: &RadioConfig) -> String {
    let freq_mhz = f64::from(config.freq_hz) / 1_000_000.0;
    let bw = format_bandwidth(config.bw);
    let power = if config.tx_power_dbm == donglora_client::TX_POWER_MAX {
        "max".to_string()
    } else {
        format!("{} dBm", config.tx_power_dbm)
    };
    let preamble = if config.preamble_len == 0 { 16 } else { config.preamble_len };
    let cad = if config.cad != 0 { "on" } else { "off" };
    format!(
        "{freq_mhz:.3}MHz SF{} BW{bw} CR4/{} SW0x{:04X} TX:{power} Pre:{preamble} CAD:{cad}",
        config.sf, config.cr, config.sync_word
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn default_config() -> RadioConfig {
        RadioConfig {
            freq_hz: 910_525_000,
            bw: donglora_client::Bandwidth::Khz62,
            sf: 7,
            cr: 5,
            sync_word: 0x3444,
            tx_power_dbm: 22,
            preamble_len: 16,
            cad: 1,
        }
    }

    #[test]
    fn configs_match_identical() {
        let a = default_config();
        let b = default_config();
        assert!(configs_match(&a, &b));
    }

    #[test]
    fn configs_match_differs_each_field() {
        let base = default_config();

        let mut c = base;
        c.freq_hz = 915_000_000;
        assert!(!configs_match(&base, &c));

        let mut c = base;
        c.sf = 12;
        assert!(!configs_match(&base, &c));

        let mut c = base;
        c.cr = 8;
        assert!(!configs_match(&base, &c));

        let mut c = base;
        c.sync_word = 0x1234;
        assert!(!configs_match(&base, &c));

        let mut c = base;
        c.tx_power_dbm = 10;
        assert!(!configs_match(&base, &c));

        let mut c = base;
        c.preamble_len = 8;
        assert!(!configs_match(&base, &c));

        let mut c = base;
        c.cad = 0;
        assert!(!configs_match(&base, &c));
    }

    #[test]
    fn is_timeout_would_block() {
        let e = anyhow::Error::from(std::io::Error::new(std::io::ErrorKind::WouldBlock, "test"));
        assert!(is_timeout_error(&e));
    }

    #[test]
    fn is_timeout_timed_out() {
        let e = anyhow::Error::from(std::io::Error::new(std::io::ErrorKind::TimedOut, "test"));
        assert!(is_timeout_error(&e));
    }

    #[test]
    fn is_timeout_false_for_other() {
        let e = anyhow::Error::from(std::io::Error::new(std::io::ErrorKind::ConnectionReset, "test"));
        assert!(!is_timeout_error(&e));
    }

    #[test]
    fn shorten_path_extracts_filename() {
        assert_eq!(shorten_path("/dev/ttyACM0"), "ttyACM0");
        assert_eq!(shorten_path("ttyACM0"), "ttyACM0");
    }

    #[test]
    fn format_bandwidth_all_variants() {
        assert_eq!(format_bandwidth(donglora_client::Bandwidth::Khz7), "7.8 kHz");
        assert_eq!(format_bandwidth(donglora_client::Bandwidth::Khz125), "125 kHz");
        assert_eq!(format_bandwidth(donglora_client::Bandwidth::Khz500), "500 kHz");
    }

    #[test]
    fn format_radio_config_typical() {
        let cfg = default_config();
        let s = format_radio_config(&cfg);
        assert!(s.contains("910.525MHz"));
        assert!(s.contains("SF7"));
        assert!(s.contains("CR4/5"));
        assert!(s.contains("0x3444"));
        assert!(s.contains("CAD:on"), "cad=1 should show CAD:on: {s}");
    }

    #[test]
    fn format_radio_config_cad_off() {
        let mut cfg = default_config();
        cfg.cad = 0;
        let s = format_radio_config(&cfg);
        assert!(s.contains("CAD:off"), "cad=0 should show CAD:off: {s}");
    }

    #[test]
    fn format_radio_config_max_power() {
        let mut cfg = default_config();
        cfg.tx_power_dbm = donglora_client::TX_POWER_MAX;
        let s = format_radio_config(&cfg);
        assert!(s.contains("TX:max"));
        assert!(!s.contains("dBm"), "max power should not contain 'dBm': {s}");
    }

    #[test]
    fn format_radio_config_non_max_power() {
        let mut cfg = default_config();
        cfg.tx_power_dbm = 22;
        assert_ne!(cfg.tx_power_dbm, donglora_client::TX_POWER_MAX);
        let s = format_radio_config(&cfg);
        assert!(s.contains("22 dBm"), "non-max power should contain '22 dBm': {s}");
        assert!(!s.contains("max"), "non-max power should not contain 'max': {s}");
    }

    #[test]
    fn format_radio_config_zero_preamble() {
        let mut cfg = default_config();
        cfg.preamble_len = 0;
        let s = format_radio_config(&cfg);
        assert!(s.contains("Pre:16"));
    }
}
