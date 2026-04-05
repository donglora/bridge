mod config;
mod network;
mod packet;
mod radio;
mod router;
mod tui;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::network::{Network, PeerConfig};
use crate::router::Stats;

#[derive(Parser)]
#[command(name = "donglora-bridge", about = "Peer-to-peer LoRa bridge using iroh")]
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
    /// Print this bridge's NodeId and exit
    ShowId,
    /// Generate an example config and print to stdout
    GenerateConfig,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Resolve config path.
    let config_path = match &cli.config {
        Some(p) => std::path::PathBuf::from(p),
        None => config::config_path()?,
    };

    // Load or create secret key.
    // Secret key lives alongside the config file (same dir, .key extension).
    let key_path = config_path.with_extension("key");
    let secret_key = config::load_or_create_secret_key(&key_path)?;
    let node_id = secret_key.public();
    let node_id_str = node_id.to_string();

    // Handle subcommands.
    match cli.command {
        Some(Commands::ShowId) => {
            println!("{node_id_str}");
            return Ok(());
        }
        Some(Commands::GenerateConfig) => {
            print!("{}", config::generate_default_config(&node_id_str));
            return Ok(());
        }
        None => {}
    }

    // First-run: if config doesn't exist, generate it and exit.
    if !config_path.exists() {
        config::write_default_config(&config_path, &node_id_str)?;
        println!("Generated config at {}", config_path.display());
        println!("Your NodeId: {node_id_str}");
        println!();
        println!("Add peers to your config and run again!");
        return Ok(());
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
        // Log to file in TUI mode.
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

    info!("donglora-bridge starting, NodeId: {node_id_str}");

    // Parse radio config.
    let radio_config = cfg.radio.to_radio_config()?;
    let radio_config_str = radio::format_radio_config(&radio_config);

    // Parse peer configs.
    let mut peer_configs = Vec::new();
    let mut peer_names: HashMap<iroh::PublicKey, String> = HashMap::new();
    for entry in &cfg.peers {
        let pk = entry.public_key()?;
        let name = entry.display_name();
        peer_names.insert(pk, name.clone());
        peer_configs.push(PeerConfig {
            public_key: pk,
            name,
            addresses: entry.addresses.clone(),
        });
    }

    if peer_configs.is_empty() && !cli.log_only {
        println!("No peers configured. Add peers to {} and run again.", config_path.display());
        println!("Your NodeId: {node_id_str}");
        return Ok(());
    }

    // Create network.
    let network = Arc::new(Network::new(secret_key, peer_configs).await?);
    info!("iroh endpoint bound, id: {node_id_str}");

    // Start network.
    let (net_event_rx, net_send_tx) = network.start();

    // Start radio.
    let (radio_event_rx, radio_tx) = radio::spawn(radio_config, cfg.radio.port.clone());

    // Stats + log channel + radio config watch.
    let stats = Arc::new(Stats::default());
    let (log_tx, log_rx) = mpsc::channel(512);
    let (config_watch_tx, config_watch_rx) = tokio::sync::watch::channel(
        router::RadioConfigInfo {
            config_str: radio_config_str,
            source: radio::ConfigSource::Ours,
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
    let router_peer_names = peer_names;

    tokio::spawn(async move {
        router::run(
            router_our_id,
            dedup_window,
            tx_queue_size,
            router_peer_names,
            radio_event_rx,
            radio_tx,
            net_event_rx,
            net_send_tx,
            &router_stats,
            log_tx,
            config_watch_tx,
        )
        .await;
        router_cancel.cancel();
    });

    // Run TUI or headless.
    if cli.log_only {
        info!("running in headless mode");
        // Just wait for cancellation or ctrl-c.
        tokio::select! {
            () = cancel.cancelled() => {},
            result = tokio::signal::ctrl_c() => {
                if let Err(e) = result {
                    tracing::error!("signal handler error: {e}");
                }
                info!("received ctrl-c, shutting down");
            }
        }
    } else {
        // Run TUI (blocks until q or cancel).
        if let Err(e) = tui::run(
            node_id,
            config_watch_rx,
            network.clone(),
            stats,
            log_rx,
            cancel.clone(),
            start_time,
        )
        .await
        {
            // Restore terminal before printing error.
            eprintln!("TUI error: {e:#}");
        }
    }

    // Shutdown.
    cancel.cancel();
    network.shutdown().await;
    info!("donglora-bridge stopped");

    Ok(())
}
