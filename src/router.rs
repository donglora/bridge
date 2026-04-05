use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use iroh::PublicKey;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::network::NetworkEvent;
use crate::packet::{Envelope, RadioPacket, content_hash};
use crate::radio::{ConfigSource, RadioEvent, format_radio_config};

/// Active radio config info, sent to the TUI via watch channel.
#[derive(Debug, Clone)]
pub struct RadioConfigInfo {
    pub config_str: String,
    pub source: ConfigSource,
}

/// Packet event for the TUI log.
#[derive(Debug, Clone)]
pub struct PacketLogEntry {
    pub timestamp: Instant,
    pub direction: PacketDirection,
    pub size: usize,
    pub snr: Option<i16>,
    pub rssi: Option<i16>,
    pub peer_name: Option<String>,
    pub action: PacketAction,
}

#[derive(Debug, Clone, Copy)]
pub enum PacketDirection {
    RadioRx,
    RadioTx,
    NetRx,
}

#[derive(Debug, Clone, Copy)]
pub enum PacketAction {
    Forwarded,
    Transmitted,
    DroppedDedup,
    DroppedQueueFull,
}

impl std::fmt::Display for PacketDirection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RadioRx => write!(f, "RX "),
            Self::RadioTx => write!(f, "TX "),
            Self::NetRx => write!(f, "NET"),
        }
    }
}

impl std::fmt::Display for PacketAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Forwarded => write!(f, "FWD"),
            Self::Transmitted => write!(f, "TX"),
            Self::DroppedDedup => write!(f, "DUP"),
            Self::DroppedQueueFull => write!(f, "FULL"),
        }
    }
}

/// Aggregate stats for the TUI.
#[derive(Debug, Default)]
pub struct Stats {
    pub radio_rx: AtomicU64,
    pub radio_tx: AtomicU64,
    pub forwarded: AtomicU64,
    pub dropped_snr: AtomicU64,
    pub dropped_dedup: AtomicU64,
    pub dropped_queue: AtomicU64,
    pub radio_connected: AtomicU64, // 0 = disconnected, 1 = connected
}

impl Stats {
    pub fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            radio_rx: self.radio_rx.load(Ordering::Relaxed),
            radio_tx: self.radio_tx.load(Ordering::Relaxed),
            forwarded: self.forwarded.load(Ordering::Relaxed),
            dropped_snr: self.dropped_snr.load(Ordering::Relaxed),
            dropped_dedup: self.dropped_dedup.load(Ordering::Relaxed),
            dropped_queue: self.dropped_queue.load(Ordering::Relaxed),
            radio_connected: self.radio_connected.load(Ordering::Relaxed) != 0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct StatsSnapshot {
    pub radio_rx: u64,
    pub radio_tx: u64,
    pub forwarded: u64,
    pub dropped_snr: u64,
    pub dropped_dedup: u64,
    pub dropped_queue: u64,
    pub radio_connected: bool,
}

/// Time-bounded dedup cache.
struct DedupCache<K: std::hash::Hash + Eq> {
    entries: HashMap<K, Instant>,
    ttl: Duration,
}

impl<K: std::hash::Hash + Eq> DedupCache<K> {
    fn new(ttl: Duration) -> Self {
        Self {
            entries: HashMap::new(),
            ttl,
        }
    }

    /// Returns true if the key was already present (i.e., duplicate).
    fn check_and_insert(&mut self, key: K) -> bool {
        let now = Instant::now();
        self.prune(now);
        if self.entries.contains_key(&key) {
            return true;
        }
        self.entries.insert(key, now);
        false
    }

    fn insert(&mut self, key: K) {
        self.entries.insert(key, Instant::now());
    }

    fn prune(&mut self, now: Instant) {
        self.entries.retain(|_, ts| now.duration_since(*ts) < self.ttl);
    }
}

/// Peer name lookup for log entries.
struct PeerNames {
    names: HashMap<PublicKey, String>,
}

impl PeerNames {
    fn get(&self, pk: &PublicKey) -> Option<String> {
        self.names.get(pk).cloned()
    }
}

/// Run the router — the central packet bus.
///
/// This is an async task that mediates between the radio thread, the iroh
/// network layer, and the TUI.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    our_id: PublicKey,
    dedup_window: Duration,
    tx_queue_size: usize,
    peer_names: HashMap<PublicKey, String>,
    mut radio_rx: mpsc::Receiver<RadioEvent>,
    radio_tx: mpsc::Sender<Vec<u8>>,
    mut net_rx: mpsc::Receiver<NetworkEvent>,
    net_tx: mpsc::Sender<(Envelope, Option<PublicKey>)>,
    stats: &Stats,
    log_tx: mpsc::Sender<PacketLogEntry>,
    config_tx: tokio::sync::watch::Sender<RadioConfigInfo>,
) {
    let names = PeerNames { names: peer_names };
    let mut seq: u64 = 0;
    let mut iroh_dedup: DedupCache<(PublicKey, u64)> = DedupCache::new(dedup_window);
    let mut echo_dedup: DedupCache<[u8; 16]> = DedupCache::new(dedup_window);
    let mut tx_queue: VecDeque<Vec<u8>> = VecDeque::with_capacity(tx_queue_size);

    loop {
        tokio::select! {
            // Radio events.
            event = radio_rx.recv() => {
                let Some(event) = event else { break };
                match event {
                    RadioEvent::Packet(pkt) => {
                        handle_radio_packet(
                            &our_id, &mut seq, &pkt,
                            &mut echo_dedup, &net_tx,
                            stats, &log_tx, &names,
                        ).await;
                    }
                    RadioEvent::Connected(active_config, source) => {
                        stats.radio_connected.store(1, Ordering::Relaxed);
                        let _ = config_tx.send(RadioConfigInfo {
                            config_str: format_radio_config(&active_config),
                            source,
                        });
                    }
                    RadioEvent::Disconnected => {
                        stats.radio_connected.store(0, Ordering::Relaxed);
                    }
                }
            }

            // Network events.
            event = net_rx.recv() => {
                let Some(event) = event else { break };
                match event {
                    NetworkEvent::Packet { from, envelope } => {
                        handle_net_packet(
                            &from, envelope, &mut iroh_dedup, &mut echo_dedup,
                            &mut tx_queue, tx_queue_size,
                            stats, &log_tx, &names,
                        ).await;
                    }
                    NetworkEvent::PeerConnected(_) | NetworkEvent::PeerDisconnected(_) => {
                        // State tracked in network layer; TUI reads from there.
                    }
                }
            }
        }

        // Drain TX queue to radio.
        while let Some(payload) = tx_queue.pop_front() {
            let size = payload.len();
            stats.radio_tx.fetch_add(1, Ordering::Relaxed);
            let _ = log_tx
                .send(PacketLogEntry {
                    timestamp: Instant::now(),
                    direction: PacketDirection::RadioTx,
                    size,
                    snr: None,
                    rssi: None,
                    peer_name: None,
                    action: PacketAction::Transmitted,
                })
                .await;
            if radio_tx.send(payload).await.is_err() {
                break;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_radio_packet(
    our_id: &PublicKey,
    seq: &mut u64,
    pkt: &RadioPacket,
    echo_dedup: &mut DedupCache<[u8; 16]>,
    net_tx: &mpsc::Sender<(Envelope, Option<PublicKey>)>,
    stats: &Stats,
    log_tx: &mpsc::Sender<PacketLogEntry>,
    names: &PeerNames,
) {
    stats.radio_rx.fetch_add(1, Ordering::Relaxed);

    // Check echo suppression.
    let hash = content_hash(&pkt.payload);
    if echo_dedup.check_and_insert(hash) {
        stats.dropped_dedup.fetch_add(1, Ordering::Relaxed);
        debug!("radio RX echo suppressed ({} bytes)", pkt.payload.len());
        let _ = log_tx
            .send(PacketLogEntry {
                timestamp: Instant::now(),
                direction: PacketDirection::RadioRx,
                size: pkt.payload.len(),
                snr: Some(pkt.snr),
                rssi: Some(pkt.rssi),
                peer_name: None,
                action: PacketAction::DroppedDedup,
            })
            .await;
        return;
    }

    // Wrap and forward to all peers.
    *seq += 1;
    let envelope = Envelope {
        origin: *our_id,
        seq: *seq,
        rssi: pkt.rssi,
        snr: pkt.snr,
        payload: pkt.payload.clone(),
    };

    stats.forwarded.fetch_add(1, Ordering::Relaxed);
    let _ = net_tx.send((envelope, None)).await;
    let _ = log_tx
        .send(PacketLogEntry {
            timestamp: Instant::now(),
            direction: PacketDirection::RadioRx,
            size: pkt.payload.len(),
            snr: Some(pkt.snr),
            rssi: Some(pkt.rssi),
            peer_name: names.get(our_id),
            action: PacketAction::Forwarded,
        })
        .await;
}

#[allow(clippy::too_many_arguments)]
async fn handle_net_packet(
    from: &PublicKey,
    envelope: Envelope,
    iroh_dedup: &mut DedupCache<(PublicKey, u64)>,
    echo_dedup: &mut DedupCache<[u8; 16]>,
    tx_queue: &mut VecDeque<Vec<u8>>,
    tx_queue_size: usize,
    stats: &Stats,
    log_tx: &mpsc::Sender<PacketLogEntry>,
    names: &PeerNames,
) {
    // Check iroh-level dedup.
    if iroh_dedup.check_and_insert((envelope.origin, envelope.seq)) {
        stats.dropped_dedup.fetch_add(1, Ordering::Relaxed);
        debug!("net dedup ({} bytes from {})", envelope.payload.len(), fmt_key(from));
        let _ = log_tx
            .send(PacketLogEntry {
                timestamp: Instant::now(),
                direction: PacketDirection::NetRx,
                size: envelope.payload.len(),
                snr: Some(envelope.snr),
                rssi: Some(envelope.rssi),
                peer_name: names.get(from),
                action: PacketAction::DroppedDedup,
            })
            .await;
        return;
    }

    // Insert content hash for radio echo suppression.
    let hash = content_hash(&envelope.payload);
    echo_dedup.insert(hash);

    // Enqueue for radio TX.
    if tx_queue.len() >= tx_queue_size {
        stats.dropped_queue.fetch_add(1, Ordering::Relaxed);
        warn!("TX queue full, dropping packet ({} bytes)", envelope.payload.len());
        let _ = log_tx
            .send(PacketLogEntry {
                timestamp: Instant::now(),
                direction: PacketDirection::NetRx,
                size: envelope.payload.len(),
                snr: Some(envelope.snr),
                rssi: Some(envelope.rssi),
                peer_name: names.get(from),
                action: PacketAction::DroppedQueueFull,
            })
            .await;
        return;
    }

    tx_queue.push_back(envelope.payload);
    let _ = log_tx
        .send(PacketLogEntry {
            timestamp: Instant::now(),
            direction: PacketDirection::NetRx,
            size: tx_queue.back().map(Vec::len).unwrap_or(0),
            snr: Some(envelope.snr),
            rssi: Some(envelope.rssi),
            peer_name: names.get(from),
            action: PacketAction::Transmitted,
        })
        .await;
}

fn fmt_key(pk: &PublicKey) -> String {
    let s = pk.to_string();
    if s.len() > 12 {
        format!("{}...", &s[..12])
    } else {
        s
    }
}
