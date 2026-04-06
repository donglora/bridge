use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use iroh::PublicKey;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::gossip::GossipEvent;
use crate::packet::{GossipFrame, content_hash};
use crate::radio::{ConfigSource, RadioEvent};
use crate::rate_limit::RateLimiter;

/// Active radio config info, sent to the TUI via watch channel.
#[derive(Debug, Clone)]
pub struct RadioConfigInfo {
    pub active: donglora_client::RadioConfig,
    pub requested: donglora_client::RadioConfig,
    pub source: ConfigSource,
    pub device: String,
    pub connected: bool,
}

/// Packet event for the TUI log.
#[derive(Debug, Clone)]
pub struct PacketLogEntry {
    pub timestamp: Instant,
    pub hash: [u8; 32],
    pub direction: PacketDirection,
    pub size: usize,
    pub snr: Option<i16>,
    pub rssi: Option<i16>,
    pub action: PacketAction,
}

/// Which side the packet arrived from.
#[derive(Debug, Clone, Copy)]
pub enum PacketDirection {
    /// Heard on local radio → forwarded to gossip swarm.
    RadioIn,
    /// Received from gossip swarm → queued for radio TX.
    GossipIn,
}

/// What happened to the packet.
#[derive(Debug, Clone, Copy)]
pub enum PacketAction {
    /// Successfully bridged (radio→gossip or gossip→radio).
    Bridged,
    /// Dropped: duplicate payload.
    DroppedDedup,
    /// Dropped: TX queue full.
    DroppedQueueFull,
    /// Dropped: rate limiter.
    DroppedRateLimit,
}

impl std::fmt::Display for PacketAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bridged => write!(f, ""),
            Self::DroppedDedup => write!(f, "DUP"),
            Self::DroppedQueueFull => write!(f, "FULL"),
            Self::DroppedRateLimit => write!(f, "RATE"),
        }
    }
}

/// Aggregate stats for the TUI.
#[derive(Debug, Default)]
pub struct Stats {
    pub radio_rx: AtomicU64,
    pub radio_tx: AtomicU64,
    pub gossip_rx: AtomicU64,
    pub gossip_tx: AtomicU64,
    pub dedup_hits: AtomicU64,
    pub rate_limit_drops: AtomicU64,
    pub dropped_queue: AtomicU64,
    pub neighbor_count: AtomicU64,
    pub radio_connected: AtomicU64, // 0 = disconnected, 1 = connected
}

impl Stats {
    pub fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            radio_rx: self.radio_rx.load(Ordering::Relaxed),
            radio_tx: self.radio_tx.load(Ordering::Relaxed),
            gossip_rx: self.gossip_rx.load(Ordering::Relaxed),
            gossip_tx: self.gossip_tx.load(Ordering::Relaxed),
            dedup_hits: self.dedup_hits.load(Ordering::Relaxed),
            rate_limit_drops: self.rate_limit_drops.load(Ordering::Relaxed),
            dropped_queue: self.dropped_queue.load(Ordering::Relaxed),
            neighbor_count: self.neighbor_count.load(Ordering::Relaxed),
            radio_connected: self.radio_connected.load(Ordering::Relaxed) != 0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct StatsSnapshot {
    pub radio_rx: u64,
    pub radio_tx: u64,
    pub gossip_rx: u64,
    pub gossip_tx: u64,
    pub dedup_hits: u64,
    pub rate_limit_drops: u64,
    pub dropped_queue: u64,
    pub neighbor_count: u64,
    pub radio_connected: bool,
}

/// Time-bounded dedup cache.
struct DedupCache {
    entries: HashMap<[u8; 32], Instant>,
    ttl: Duration,
}

impl DedupCache {
    fn new(ttl: Duration) -> Self {
        Self {
            entries: HashMap::new(),
            ttl,
        }
    }

    /// Returns true if the key was already present (i.e., duplicate).
    fn check_and_insert(&mut self, key: [u8; 32]) -> bool {
        let now = Instant::now();
        if let Some(ts) = self.entries.get(&key)
            && now.duration_since(*ts) < self.ttl
        {
            return true;
        }
        self.entries.insert(key, now);
        false
    }

    fn prune(&mut self) {
        let now = Instant::now();
        self.entries.retain(|_, ts| now.duration_since(*ts) < self.ttl);
    }
}

/// Run the router — the central packet bus.
///
/// Bridges radio packets to/from the gossip swarm with deduplication and rate limiting.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    our_id: PublicKey,
    dedup_window: Duration,
    tx_queue_size: usize,
    requested_radio_config: donglora_client::RadioConfig,
    mut rate_limiter: RateLimiter,
    mut radio_rx: mpsc::Receiver<RadioEvent>,
    radio_tx: mpsc::Sender<Vec<u8>>,
    mut gossip_rx: mpsc::Receiver<GossipEvent>,
    gossip_tx: mpsc::Sender<GossipFrame>,
    stats: &Stats,
    log_tx: mpsc::Sender<PacketLogEntry>,
    config_tx: tokio::sync::watch::Sender<RadioConfigInfo>,
) {
    let mut dedup = DedupCache::new(dedup_window);
    let mut tx_queue: VecDeque<Vec<u8>> = VecDeque::with_capacity(tx_queue_size);
    let mut last_cleanup = Instant::now();
    let cleanup_interval = Duration::from_secs(30);

    loop {
        // Periodic dedup cleanup.
        if last_cleanup.elapsed() >= cleanup_interval {
            dedup.prune();
            last_cleanup = Instant::now();
        }

        tokio::select! {
            // Radio events.
            event = radio_rx.recv() => {
                let Some(event) = event else { break };
                match event {
                    RadioEvent::Packet(pkt) => {
                        stats.radio_rx.fetch_add(1, Ordering::Relaxed);

                        let hash = content_hash(&pkt.payload);
                        if dedup.check_and_insert(hash) {
                            stats.dedup_hits.fetch_add(1, Ordering::Relaxed);
                            debug!("radio RX dedup suppressed ({} bytes)", pkt.payload.len());
                            let _ = log_tx.send(PacketLogEntry {
                                timestamp: Instant::now(),
                                hash,
                                direction: PacketDirection::RadioIn,
                                size: pkt.payload.len(),
                                snr: Some(pkt.snr),
                                rssi: Some(pkt.rssi),
                                action: PacketAction::DroppedDedup,
                            }).await;
                            continue;
                        }

                        // Wrap and broadcast to gossip.
                        let frame = GossipFrame::new(&our_id, pkt.rssi, pkt.snr, pkt.payload.clone());
                        stats.gossip_tx.fetch_add(1, Ordering::Relaxed);
                        let _ = gossip_tx.send(frame).await;

                        let _ = log_tx.send(PacketLogEntry {
                            timestamp: Instant::now(),
                            hash,
                            direction: PacketDirection::RadioIn,
                            size: pkt.payload.len(),
                            snr: Some(pkt.snr),
                            rssi: Some(pkt.rssi),
                            action: PacketAction::Bridged,
                        }).await;
                    }
                    RadioEvent::Connected(active_config, source, device) => {
                        stats.radio_connected.store(1, Ordering::Relaxed);
                        let _ = config_tx.send(RadioConfigInfo {
                            active: active_config,
                            requested: requested_radio_config,
                            source,
                            device,
                            connected: true,
                        });
                    }
                    RadioEvent::Disconnected => {
                        stats.radio_connected.store(0, Ordering::Relaxed);
                        config_tx.send_modify(|info| info.connected = false);
                    }
                }
            }

            // Gossip events.
            event = gossip_rx.recv() => {
                let Some(event) = event else { break };
                match event {
                    GossipEvent::Frame(frame) => {
                        stats.gossip_rx.fetch_add(1, Ordering::Relaxed);

                        let hash = content_hash(&frame.payload);
                        if dedup.check_and_insert(hash) {
                            stats.dedup_hits.fetch_add(1, Ordering::Relaxed);
                            debug!("gossip RX dedup suppressed ({} bytes)", frame.payload.len());
                            let _ = log_tx.send(PacketLogEntry {
                                timestamp: Instant::now(),
                                hash,
                                direction: PacketDirection::GossipIn,
                                size: frame.payload.len(),
                                snr: Some(frame.snr),
                                rssi: Some(frame.rssi),
                                action: PacketAction::DroppedDedup,
                            }).await;
                            continue;
                        }

                        // Rate limit check.
                        if !rate_limiter.try_acquire() {
                            stats.rate_limit_drops.fetch_add(1, Ordering::Relaxed);
                            debug!("rate limited ({} bytes)", frame.payload.len());
                            let _ = log_tx.send(PacketLogEntry {
                                timestamp: Instant::now(),
                                hash,
                                direction: PacketDirection::GossipIn,
                                size: frame.payload.len(),
                                snr: Some(frame.snr),
                                rssi: Some(frame.rssi),
                                action: PacketAction::DroppedRateLimit,
                            }).await;
                            continue;
                        }

                        // Enqueue for radio TX.
                        if tx_queue.len() >= tx_queue_size {
                            stats.dropped_queue.fetch_add(1, Ordering::Relaxed);
                            warn!("TX queue full, dropping packet ({} bytes)", frame.payload.len());
                            let _ = log_tx.send(PacketLogEntry {
                                timestamp: Instant::now(),
                                hash,
                                direction: PacketDirection::GossipIn,
                                size: frame.payload.len(),
                                snr: Some(frame.snr),
                                rssi: Some(frame.rssi),
                                action: PacketAction::DroppedQueueFull,
                            }).await;
                            continue;
                        }

                        tx_queue.push_back(frame.payload.clone());
                        let _ = log_tx.send(PacketLogEntry {
                            timestamp: Instant::now(),
                            hash,
                            direction: PacketDirection::GossipIn,
                            size: frame.payload.len(),
                            snr: Some(frame.snr),
                            rssi: Some(frame.rssi),
                            action: PacketAction::Bridged,
                        }).await;
                    }
                    GossipEvent::NeighborChanged(count) => {
                        stats.neighbor_count.store(count as u64, Ordering::Relaxed);
                    }
                }
            }
        }

        // Drain TX queue to radio.
        while let Some(payload) = tx_queue.pop_front() {
            stats.radio_tx.fetch_add(1, Ordering::Relaxed);
            if radio_tx.send(payload).await.is_err() {
                break;
            }
        }
    }
}
