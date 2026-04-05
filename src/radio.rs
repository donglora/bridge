use std::time::Duration;

use anyhow::Result;
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
    /// Radio connected (or reconnected) with the active config.
    Connected(RadioConfig, ConfigSource),
    /// Radio disconnected — attempting reconnect.
    Disconnected,
}

/// Spawn the blocking radio thread.
///
/// Returns a receiver for radio events and a sender for TX payloads.
pub fn spawn(
    config: RadioConfig,
    port: Option<String>,
) -> (mpsc::Receiver<RadioEvent>, mpsc::Sender<Vec<u8>>) {
    let (event_tx, event_rx) = mpsc::channel::<RadioEvent>(256);
    let (tx_send, tx_recv) = mpsc::channel::<Vec<u8>>(64);

    std::thread::Builder::new()
        .name("radio".into())
        .spawn(move || radio_loop(config, port, event_tx, tx_recv))
        .ok();

    (event_rx, tx_send)
}

fn radio_loop(
    config: RadioConfig,
    port: Option<String>,
    event_tx: mpsc::Sender<RadioEvent>,
    mut tx_recv: mpsc::Receiver<Vec<u8>>,
) {
    let mut backoff = Duration::from_secs(1);
    let max_backoff = Duration::from_secs(30);

    loop {
        match connect_and_run(&config, &port, &event_tx, &mut tx_recv) {
            Ok(()) => {
                // Clean shutdown (channel closed).
                info!("radio thread exiting");
                return;
            }
            Err(e) => {
                error!("radio error: {e:#}");
                let _ = event_tx.blocking_send(RadioEvent::Disconnected);
                info!("reconnecting in {backoff:?}");
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(max_backoff);
            }
        }
    }
}

fn connect_and_run(
    config: &RadioConfig,
    port: &Option<String>,
    event_tx: &mpsc::Sender<RadioEvent>,
    tx_recv: &mut mpsc::Receiver<Vec<u8>>,
) -> Result<()> {
    let timeout = Duration::from_secs(2);

    info!("connecting to dongle...");
    let mut client = if let Some(port) = &port {
        donglora_client::connect(Some(port), timeout)?
    } else {
        donglora_client::connect_default()?
    };

    client.ping()?;
    info!("dongle connected, configuring radio");

    // Try to set our config. If the mux rejects it (config already locked by
    // another client), fall back to reading whatever config is active.
    let (active_config, config_source) = match client.set_config(*config) {
        Ok(()) => {
            info!("radio configured with our settings");
            (*config, ConfigSource::Ours)
        }
        Err(e) => {
            warn!("SetConfig failed ({e:#}), reading active config from mux");
            match client.get_config() {
                Ok(mux_config) => {
                    info!(
                        "using mux config: {}",
                        format_radio_config(&mux_config)
                    );
                    (mux_config, ConfigSource::Mux)
                }
                Err(e2) => {
                    anyhow::bail!("SetConfig failed ({e:#}) and GetConfig also failed ({e2:#})");
                }
            }
        }
    };

    client.start_rx()?;
    info!("radio receiving");

    let _ = event_tx.blocking_send(RadioEvent::Connected(active_config, config_source));

    // Set a short timeout so we can interleave RX and TX.
    client
        .transport_mut()
        .set_timeout(Duration::from_millis(100))?;

    loop {
        // Check for TX requests (non-blocking).
        while let Ok(payload) = tx_recv.try_recv() {
            debug!("TX {} bytes", payload.len());
            if let Err(e) = client.transmit(&payload, None) {
                warn!("transmit error: {e:#}");
            }
        }

        // Check for RX packets.
        match client.recv() {
            Ok(Some(Response::RxPacket { rssi, snr, payload })) => {
                let grade = snr_grade(snr, active_config.sf);
                if grade.should_forward() {
                    debug!(
                        "RX {} bytes rssi={rssi} snr={snr} grade={grade}",
                        payload.len()
                    );
                    let pkt = RadioPacket { rssi, snr, payload };
                    if event_tx.blocking_send(RadioEvent::Packet(pkt)).is_err() {
                        // Router dropped — shut down.
                        return Ok(());
                    }
                } else {
                    debug!(
                        "RX drop {} bytes rssi={rssi} snr={snr} grade={grade}",
                        payload.len()
                    );
                }
            }
            Ok(Some(_)) => {
                // Non-RxPacket unsolicited response — ignore.
            }
            Ok(None) => {
                // Timeout — no packet, loop back to check TX.
            }
            Err(e) if is_timeout_error(&e) => {
                // Mux sockets return WouldBlock on timeout instead of Ok(0).
                // Treat the same as Ok(None) — just a timeout, not a real error.
            }
            Err(e) => return Err(e),
        }

        // If the event channel is closed, the router is gone — exit cleanly.
        if event_tx.is_closed() {
            return Ok(());
        }
    }
}

/// Check if an anyhow::Error wraps a timeout-like I/O error.
///
/// Workaround: donglora-client's `read_frame` handles `TimedOut` but not
/// `WouldBlock`. On Linux, socket read timeouts return EAGAIN (WouldBlock),
/// not TimedOut. Serial ports return Ok(0) so they work fine, but mux
/// sockets surface this as an error. We catch it here instead of letting
/// it trigger the reconnect loop.
fn is_timeout_error(e: &anyhow::Error) -> bool {
    if let Some(io_err) = e.downcast_ref::<std::io::Error>() {
        matches!(
            io_err.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        )
    } else {
        false
    }
}

/// Format a RadioConfig for display.
pub fn format_radio_config(config: &RadioConfig) -> String {
    let freq_mhz = config.freq_hz as f64 / 1_000_000.0;
    let bw = match config.bw {
        donglora_client::Bandwidth::Khz7 => "7.8kHz",
        donglora_client::Bandwidth::Khz10 => "10.4kHz",
        donglora_client::Bandwidth::Khz15 => "15.6kHz",
        donglora_client::Bandwidth::Khz20 => "20.8kHz",
        donglora_client::Bandwidth::Khz31 => "31.25kHz",
        donglora_client::Bandwidth::Khz41 => "41.7kHz",
        donglora_client::Bandwidth::Khz62 => "62.5kHz",
        donglora_client::Bandwidth::Khz125 => "125kHz",
        donglora_client::Bandwidth::Khz250 => "250kHz",
        donglora_client::Bandwidth::Khz500 => "500kHz",
    };
    let power = if config.tx_power_dbm == donglora_client::TX_POWER_MAX {
        "max".to_string()
    } else {
        format!("{}dBm", config.tx_power_dbm)
    };
    let preamble = if config.preamble_len == 0 {
        16
    } else {
        config.preamble_len
    };
    let cad = if config.cad != 0 { "on" } else { "off" };
    format!(
        "{freq_mhz:.3}MHz SF{} BW{bw} CR4/{} SW0x{:04X} TX:{power} Pre:{preamble} CAD:{cad}",
        config.sf, config.cr, config.sync_word
    )
}
