use donglora_bridge::{config, gossip, radio, rate_limit, router, setup, tui};

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use iroh::SecretKey;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::rate_limit::RateLimiter;
use crate::router::Stats;

#[derive(Parser)]
#[command(name = "donglora-bridge", about = "LoRa gossip bridge using iroh")]
struct Cli {
    /// Path to config file (default: ~/.config/donglora-bridge/config.toml)
    #[arg(long)]
    config: Option<String>,

    /// Run in headless mode (structured logs to stdout, no TUI)
    #[arg(long)]
    log_only: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Interactive configuration wizard
    Config,
}

#[tokio::main]
#[allow(clippy::too_many_lines)]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Resolve config path.
    let config_path = match &cli.config {
        Some(p) => std::path::PathBuf::from(p),
        None => config::config_path()?,
    };

    // Config subcommand: run wizard with existing config as defaults.
    if matches!(cli.command, Some(Commands::Config)) {
        let existing = if config_path.exists() { config::load_config(&config_path).ok() } else { None };
        return setup::run_wizard(&config_path, existing.as_ref());
    }

    // First-run: no config file → run wizard automatically.
    if !config_path.exists() {
        println!("  No config found. Let's set one up!\n");
        return setup::run_wizard(&config_path, None);
    }

    // Load config.
    let cfg =
        config::load_config(&config_path).with_context(|| format!("loading config from {}", config_path.display()))?;

    // Set up logging.
    setup_logging(cli.log_only, &cfg);

    // Ephemeral identity — fresh keypair each launch.
    let secret_key = SecretKey::generate();
    let node_id = secret_key.public();
    info!("donglora-bridge starting, ephemeral id: {}", node_id.fmt_short());

    // Parse radio config.
    let radio_config = cfg.radio.to_lora_config()?;
    let retry_policy = cfg.tx.to_retry_policy()?;

    // Create rate limiter.
    let cr_denom: u8 = match radio_config.cr {
        donglora_client::LoRaCodingRate::Cr4_5 => 5,
        donglora_client::LoRaCodingRate::Cr4_6 => 6,
        donglora_client::LoRaCodingRate::Cr4_7 => 7,
        donglora_client::LoRaCodingRate::Cr4_8 => 8,
    };
    let rate_limiter = RateLimiter::from_radio_config(
        radio_config.sf,
        radio_config.bw,
        cr_denom,
        radio_config.preamble_len,
        cfg.bridge.rate_limit_pps,
    );
    info!("rate limiter: {:.1} pps", rate_limiter.rate_pps());

    // Create gossip network + DHT rendezvous.
    let (swarm, gossip_event_rx, gossip_frame_tx) =
        gossip::Gossip::new(secret_key, &cfg.bridge.passphrase).await?;
    let swarm = Arc::new(swarm);

    // Start radio.
    let (radio_event_rx, radio_tx, _radio_handle) = radio::spawn(radio_config, cfg.radio.port.clone(), retry_policy)?;

    // Stats + log channel + radio config watch.
    let stats = Arc::new(Stats::default());
    let (log_tx, log_rx) = mpsc::channel(512);
    let (config_watch_tx, config_watch_rx) = tokio::sync::watch::channel(router::RadioConfigInfo {
        active: radio_config,
        requested: radio_config,
        source: radio::ConfigSource::Ours,
        device: String::new(),
        connected: false,
    });

    // Cancellation.
    let cancel = CancellationToken::new();
    let start_time = Instant::now();

    // Spawn router.
    let router_stats = stats.clone();
    let router_cancel = cancel.clone();
    let router_our_id = node_id;
    let dedup_window = std::time::Duration::from_secs(cfg.bridge.dedup_window_secs);
    let tx_queue_size = cfg.bridge.tx_queue_size;

    tokio::spawn(async move {
        router::run(
            router_our_id,
            dedup_window,
            tx_queue_size,
            radio_config,
            rate_limiter,
            radio_event_rx,
            radio_tx,
            gossip_event_rx,
            gossip_frame_tx,
            &router_stats,
            log_tx,
            config_watch_tx,
        )
        .await;
        router_cancel.cancel();
    });

    // Spawn a task that cancels on ctrl-c or SIGTERM.
    {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            let sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate());
            match sigterm {
                Ok(mut sigterm) => {
                    tokio::select! {
                        _ = tokio::signal::ctrl_c() => info!("received SIGINT"),
                        _ = sigterm.recv() => info!("received SIGTERM"),
                    }
                }
                Err(e) => {
                    warn!("failed to register SIGTERM handler: {e:#}");
                    let _ = tokio::signal::ctrl_c().await;
                    info!("received SIGINT");
                }
            }
            cancel.cancel();
        });
    }

    // Run TUI or headless.
    if cli.log_only {
        info!("running in headless mode");
        // In TUI mode the TUI drains `log_rx`; in headless mode nobody
        // does unless we spawn a consumer. Without this, the router's
        // bounded log channel backs up and router throughput stalls.
        // Emit each entry at INFO level so operators can see what the
        // bridge is bridging (RX/TX/drop/retry).
        let log_cancel = cancel.clone();
        tokio::spawn(headless_packet_logger(log_rx, log_cancel));
        cancel.cancelled().await;
    } else {
        let terminal = tui::run(config_watch_rx, swarm.clone(), stats, log_rx, cancel.clone(), start_time);

        // Restore the terminal first so the user's shell reappears the moment
        // they hit `q`. iroh-gossip-rendezvous needs no synchronous DHT work
        // on shutdown — entries age out — so the only remaining wait is
        // `endpoint.close()` flushing in-flight connections. Bound it.
        if let Ok(mut term) = terminal {
            tui::restore_terminal(&mut term);
        }
        cancel.cancel();
        if tokio::time::timeout(Duration::from_secs(2), swarm.shutdown()).await.is_err() {
            warn!("graceful shutdown timed out — exiting anyway");
        }
        info!("donglora-bridge stopped");

        return Ok(());
    }

    // Shutdown (headless path).
    swarm.shutdown().await;
    info!("donglora-bridge stopped");

    Ok(())
}

/// Drain the router's packet-log channel in headless mode and emit
/// tracing events so `--log-only` output shows every bridged / dropped /
/// retried packet. Exits when `cancel` fires or the channel closes.
async fn headless_packet_logger(mut log_rx: mpsc::Receiver<router::PacketLogEntry>, cancel: CancellationToken) {
    use router::{PacketAction, PacketDirection, TxRetryState};
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            entry = log_rx.recv() => {
                let Some(e) = entry else { return; };
                let dir = match e.direction {
                    PacketDirection::RadioIn => "RF→NET",
                    PacketDirection::GossipIn => "NET→RF",
                };
                let hash_str = hex::encode(&e.hash[..4]);
                let rssi_str = e.rssi.map_or_else(|| "-".to_string(), |r| r.to_string());
                let snr_str = e.snr.map_or_else(|| "-".to_string(), |s| s.to_string());

                // TX-retry lifecycle wins over the plain action label when set.
                if let Some(r) = e.tx_retry {
                    match r.state {
                        TxRetryState::Retrying(reason) => {
                            tracing::info!(
                                target: "packet",
                                "{} retry  hash={} attempt={}/{} reason={}",
                                dir, hash_str, r.attempt, r.total_attempts, reason,
                            );
                        }
                        TxRetryState::Succeeded if r.attempt > 1 => {
                            tracing::info!(
                                target: "packet",
                                "{} sent   hash={} size={}B attempts={}/{}",
                                dir, hash_str, e.size, r.attempt, r.total_attempts,
                            );
                        }
                        TxRetryState::Failed => {
                            tracing::warn!(
                                target: "packet",
                                "{} FAILED hash={} size={}B attempts={}/{}",
                                dir, hash_str, e.size, r.attempt, r.total_attempts,
                            );
                        }
                        _ => {}
                    }
                    continue;
                }

                match e.action {
                    PacketAction::Bridged => {
                        tracing::info!(
                            target: "packet",
                            "{} bridged hash={} size={}B rssi={} snr={}",
                            dir, hash_str, e.size, rssi_str, snr_str,
                        );
                    }
                    PacketAction::DroppedDedup => {
                        // Log at INFO, not DEBUG — a run of dedup lines is
                        // proof of bidirectional gossip flow. Hiding it
                        // masks whether we're actually hearing peers.
                        tracing::info!(
                            target: "packet",
                            "{} dedup   hash={} size={}B rssi={} snr={}",
                            dir, hash_str, e.size, rssi_str, snr_str,
                        );
                    }
                    PacketAction::DroppedRateLimit => {
                        tracing::warn!(
                            target: "packet",
                            "{} ratelim hash={} size={}B",
                            dir, hash_str, e.size,
                        );
                    }
                    PacketAction::DroppedQueueFull => {
                        tracing::warn!(
                            target: "packet",
                            "{} QFULL   hash={} size={}B",
                            dir, hash_str, e.size,
                        );
                    }
                }
            }
        }
    }
}

fn setup_logging(log_only: bool, cfg: &config::Config) {
    let env_filter = || {
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
    };

    if log_only {
        tracing_subscriber::fmt().with_env_filter(env_filter()).init();
    } else {
        let log_path = cfg
            .bridge
            .log_file
            .clone()
            .unwrap_or_else(|| config::default_log_path().unwrap_or_else(|_| "/tmp/donglora-bridge.log".into()));
        if let Some(parent) = log_path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            eprintln!("warning: failed to create log directory {}: {e}", parent.display());
        }
        let file_appender = tracing_appender::rolling::never(
            log_path.parent().unwrap_or_else(|| std::path::Path::new("/tmp")),
            log_path.file_name().unwrap_or_else(|| std::ffi::OsStr::new("donglora-bridge.log")),
        );
        tracing_subscriber::fmt().with_env_filter(env_filter()).with_writer(file_appender).with_ansi(false).init();
    }
}
