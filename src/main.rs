mod config;
mod gossip;
mod packet;
mod radio;
mod rate_limit;
mod router;
mod setup;
mod tui;

use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use iroh::SecretKey;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::gossip::PassphraseKeys;
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
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Resolve config path.
    let config_path = match &cli.config {
        Some(p) => std::path::PathBuf::from(p),
        None => config::config_path()?,
    };

    // Config subcommand: run wizard with existing config as defaults.
    if let Some(Commands::Config) = cli.command {
        let existing = if config_path.exists() {
            config::load_config(&config_path).ok()
        } else {
            None
        };
        return setup::run_wizard(&config_path, existing.as_ref());
    }

    // First-run: no config file → run wizard automatically.
    if !config_path.exists() {
        println!("  No config found. Let's set one up!\n");
        return setup::run_wizard(&config_path, None);
    }

    // Load config.
    let cfg = config::load_config(&config_path)
        .with_context(|| format!("loading config from {}", config_path.display()))?;

    // Set up logging.
    if cli.log_only {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
            )
            .init();
    } else {
        let log_path = cfg
            .bridge
            .log_file
            .clone()
            .unwrap_or_else(|| config::default_log_path().unwrap_or_else(|_| "/tmp/donglora-bridge.log".into()));
        if let Some(parent) = log_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let file_appender = tracing_appender::rolling::never(
            log_path.parent().unwrap_or(std::path::Path::new("/tmp")),
            log_path.file_name().unwrap_or(std::ffi::OsStr::new("donglora-bridge.log")),
        );
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
            )
            .with_writer(file_appender)
            .with_ansi(false)
            .init();
    }

    // Ephemeral identity — fresh keypair each launch.
    let secret_key = SecretKey::generate(&mut rand::rng());
    let node_id = secret_key.public();
    info!("donglora-bridge starting, ephemeral id: {}", node_id.fmt_short());

    // Parse radio config.
    let radio_config = cfg.radio.to_radio_config()?;

    // Derive passphrase keys.
    let keys = PassphraseKeys::derive(&cfg.bridge.passphrase);
    info!("topic: {}", hex::encode(&keys.topic_id.as_bytes()[..8]));

    // Create rate limiter.
    let rate_limiter = RateLimiter::from_radio_config(
        radio_config.sf,
        radio_config.bw,
        radio_config.cr,
        radio_config.preamble_len,
        cfg.bridge.rate_limit_pps,
    );
    info!("rate limiter: {:.1} pps", rate_limiter.rate_pps());

    // Create gossip network.
    let (swarm, gossip_event_rx, gossip_frame_tx) =
        gossip::Gossip::new(secret_key, &keys).await?;
    let swarm = Arc::new(swarm);

    // Start radio.
    let (radio_event_rx, radio_tx) = radio::spawn(radio_config, cfg.radio.port.clone());

    // Stats + log channel + radio config watch.
    let stats = Arc::new(Stats::default());
    let (log_tx, log_rx) = mpsc::channel(512);
    let (config_watch_tx, config_watch_rx) = tokio::sync::watch::channel(
        router::RadioConfigInfo {
            active: radio_config,
            requested: radio_config,
            source: radio::ConfigSource::Ours,
            device: String::new(),
            connected: false,
        },
    );

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
            let sigterm = tokio::signal::unix::signal(
                tokio::signal::unix::SignalKind::terminate(),
            );
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
        cancel.cancelled().await;
    } else {
        let terminal = tui::run(
            config_watch_rx,
            swarm.clone(),
            stats,
            log_rx,
            cancel.clone(),
            start_time,
        )
        .await;

        cancel.cancel();
        swarm.shutdown().await;
        info!("donglora-bridge stopped");

        if let Ok(mut term) = terminal {
            tui::restore_terminal(&mut term);
        }

        return Ok(());
    }

    // Shutdown (headless path).
    swarm.shutdown().await;
    info!("donglora-bridge stopped");

    Ok(())
}
