//! Central packet bus bridging radio and gossip swarm.
//!
//! Applies blake3 content-hash deduplication, token bucket rate limiting,
//! and TX queue management.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use iroh::PublicKey;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::gossip::GossipEvent;
use crate::packet::{GossipFrame, content_hash};
use crate::radio::{ConfigSource, RadioEvent, TxRequest, TxRetryReason};
use crate::rate_limit::RateLimiter;

/// Active radio config info, sent to the TUI via watch channel.
#[derive(Debug, Clone)]
pub struct RadioConfigInfo {
    pub active: donglora_client::LoRaConfig,
    pub requested: donglora_client::LoRaConfig,
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
    /// When set, indicates this entry is a TX-attempt update: `Some((attempt, total))`.
    pub tx_retry: Option<TxRetryInfo>,
}

/// Per-attempt retry metadata attached to a TX `PacketLogEntry`.
#[derive(Debug, Clone, Copy)]
pub struct TxRetryInfo {
    pub attempt: u8,
    pub total_attempts: u8,
    pub state: TxRetryState,
}

/// State a TX entry is currently in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxRetryState {
    /// First attempt in flight.
    InFlight,
    /// A previous attempt failed; this attempt is a retry.
    Retrying(TxRetryReason),
    /// Final success, possibly after retries.
    Succeeded,
    /// All attempts exhausted; terminal failure.
    Failed,
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
    /// Cumulative TX retries across the bridge lifetime (each retry after
    /// the first attempt increments by 1).
    pub tx_retries: AtomicU64,
    /// TX calls that exhausted the retry policy.
    pub tx_failures: AtomicU64,
}

impl Stats {
    #[must_use]
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
            tx_retries: self.tx_retries.load(Ordering::Relaxed),
            tx_failures: self.tx_failures.load(Ordering::Relaxed),
        }
    }
}

/// Point-in-time snapshot of bridge statistics, cloned from atomic counters.
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
    pub tx_retries: u64,
    pub tx_failures: u64,
}

/// Time-bounded dedup cache.
struct DedupCache {
    entries: HashMap<[u8; 32], Instant>,
    ttl: Duration,
}

impl DedupCache {
    fn new(ttl: Duration) -> Self {
        Self { entries: HashMap::new(), ttl }
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
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub async fn run(
    our_id: PublicKey,
    dedup_window: Duration,
    tx_queue_size: usize,
    requested_radio_config: donglora_client::LoRaConfig,
    mut rate_limiter: RateLimiter,
    mut radio_rx: mpsc::Receiver<RadioEvent>,
    radio_tx: mpsc::Sender<TxRequest>,
    mut gossip_rx: mpsc::Receiver<GossipEvent>,
    gossip_tx: mpsc::Sender<GossipFrame>,
    stats: &Stats,
    log_tx: mpsc::Sender<PacketLogEntry>,
    config_tx: tokio::sync::watch::Sender<RadioConfigInfo>,
) {
    let mut dedup = DedupCache::new(dedup_window);
    let mut next_tx_seq: u64 = 0;
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
                                tx_retry: None,
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
                            tx_retry: None,
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
                    RadioEvent::TxAttempt { .. } => {
                        // No log entry: the GossipIn Bridged row that enqueued
                        // this TX already represents it. Only retries /
                        // failures produce follow-up rows.
                    }
                    RadioEvent::TxRetry { seq, attempt_that_failed, total_attempts, reason, backoff_ms: _ } => {
                        stats.tx_retries.fetch_add(1, Ordering::Relaxed);
                        let _ = log_tx.send(PacketLogEntry {
                            timestamp: Instant::now(),
                            hash: tx_seq_hash(seq),
                            direction: PacketDirection::GossipIn,
                            size: 0,
                            snr: None,
                            rssi: None,
                            action: PacketAction::Bridged,
                            tx_retry: Some(TxRetryInfo {
                                attempt: attempt_that_failed.saturating_add(1),
                                total_attempts,
                                state: TxRetryState::Retrying(reason),
                            }),
                        }).await;
                    }
                    RadioEvent::TxSucceeded { seq, attempts_used, final_airtime_us: _, size } => {
                        stats.radio_tx.fetch_add(1, Ordering::Relaxed);
                        // Only emit a success row if retries were involved —
                        // otherwise the original GossipIn row covers it.
                        if attempts_used > 1 {
                            let _ = log_tx.send(PacketLogEntry {
                                timestamp: Instant::now(),
                                hash: tx_seq_hash(seq),
                                direction: PacketDirection::GossipIn,
                                size,
                                snr: None,
                                rssi: None,
                                action: PacketAction::Bridged,
                                tx_retry: Some(TxRetryInfo {
                                    attempt: attempts_used,
                                    total_attempts: attempts_used,
                                    state: TxRetryState::Succeeded,
                                }),
                            }).await;
                        }
                    }
                    RadioEvent::TxFailed { seq, attempts_used, reason, size } => {
                        stats.tx_failures.fetch_add(1, Ordering::Relaxed);
                        warn!("TX #{seq} exhausted retries: {reason}");
                        let _ = log_tx.send(PacketLogEntry {
                            timestamp: Instant::now(),
                            hash: tx_seq_hash(seq),
                            direction: PacketDirection::GossipIn,
                            size,
                            snr: None,
                            rssi: None,
                            action: PacketAction::Bridged,
                            tx_retry: Some(TxRetryInfo {
                                attempt: attempts_used,
                                total_attempts: attempts_used,
                                state: TxRetryState::Failed,
                            }),
                        }).await;
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
                                tx_retry: None,
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
                                tx_retry: None,
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
                                tx_retry: None,
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
                            tx_retry: None,
                        }).await;
                    }
                    GossipEvent::NeighborChanged(count) => {
                        #[allow(clippy::cast_possible_truncation)]
                        stats.neighbor_count.store(count as u64, Ordering::Relaxed);
                    }
                }
            }
        }

        // Drain TX queue to the async radio task. radio_tx is a bounded
        // channel; if the radio task is busy we just wait on the next
        // loop iteration rather than blocking here.
        while let Some(payload) = tx_queue.pop_front() {
            let seq = {
                next_tx_seq = next_tx_seq.wrapping_add(1);
                next_tx_seq
            };
            if radio_tx.send(TxRequest { seq, data: payload }).await.is_err() {
                break;
            }
        }
    }
}

/// Build a stable per-TX "hash" for packet-log correlation. Not a real
/// content hash — just a 32-byte encoding of the seq so the TUI can
/// index/update log rows by it.
const fn tx_seq_hash(seq: u64) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[0] = 0x01; // tag marking this as a TX-seq pseudo-hash
    let bytes = seq.to_le_bytes();
    out[1] = bytes[0];
    out[2] = bytes[1];
    out[3] = bytes[2];
    out[4] = bytes[3];
    out[5] = bytes[4];
    out[6] = bytes[5];
    out[7] = bytes[6];
    out[8] = bytes[7];
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn dedup_cache_new_key_not_duplicate() {
        let mut cache = DedupCache::new(Duration::from_secs(10));
        assert!(!cache.check_and_insert([0u8; 32]));
    }

    #[test]
    fn dedup_cache_duplicate_detected() {
        let mut cache = DedupCache::new(Duration::from_secs(10));
        cache.check_and_insert([1u8; 32]);
        assert!(cache.check_and_insert([1u8; 32]));
    }

    #[test]
    fn dedup_cache_different_keys_independent() {
        let mut cache = DedupCache::new(Duration::from_secs(10));
        cache.check_and_insert([1u8; 32]);
        assert!(!cache.check_and_insert([2u8; 32]));
    }

    #[test]
    fn dedup_cache_expired_entry_not_duplicate() {
        let mut cache = DedupCache::new(Duration::from_millis(1));
        cache.check_and_insert([1u8; 32]);
        std::thread::sleep(Duration::from_millis(5));
        assert!(!cache.check_and_insert([1u8; 32]));
    }

    #[test]
    fn dedup_cache_prune_removes_expired() {
        let mut cache = DedupCache::new(Duration::from_millis(1));
        cache.check_and_insert([1u8; 32]);
        cache.check_and_insert([2u8; 32]);
        std::thread::sleep(Duration::from_millis(5));
        cache.prune();
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn dedup_cache_prune_keeps_fresh() {
        let mut cache = DedupCache::new(Duration::from_secs(10));
        cache.check_and_insert([1u8; 32]);
        cache.prune();
        assert_eq!(cache.entries.len(), 1);
    }

    #[test]
    fn stats_snapshot_reflects_increments() {
        let stats = Stats::default();
        stats.radio_rx.fetch_add(5, Ordering::Relaxed);
        stats.gossip_tx.fetch_add(3, Ordering::Relaxed);
        stats.radio_connected.store(1, Ordering::Relaxed);
        let snap = stats.snapshot();
        assert_eq!(snap.radio_rx, 5);
        assert_eq!(snap.gossip_tx, 3);
        assert!(snap.radio_connected);
    }

    #[test]
    fn stats_default_is_zeroed() {
        let snap = Stats::default().snapshot();
        assert_eq!(snap.radio_rx, 0);
        assert_eq!(snap.gossip_rx, 0);
        assert!(!snap.radio_connected);
    }

    #[test]
    fn packet_action_display() {
        assert_eq!(format!("{}", PacketAction::Bridged), "");
        assert_eq!(format!("{}", PacketAction::DroppedDedup), "DUP");
        assert_eq!(format!("{}", PacketAction::DroppedQueueFull), "FULL");
        assert_eq!(format!("{}", PacketAction::DroppedRateLimit), "RATE");
    }

    #[test]
    fn dedup_cache_ttl_boundary_check_and_insert() {
        // Force an entry's timestamp to be exactly TTL ago. With `< ttl`, the entry
        // at exactly TTL is NOT a duplicate. With `<= ttl` (the mutant), it would
        // wrongly be treated as still live.
        let ttl = Duration::from_secs(10);
        let mut cache = DedupCache::new(ttl);
        cache.check_and_insert([9u8; 32]);
        // Backdate the entry to exactly TTL ago.
        let exactly_ttl_ago = Instant::now().checked_sub(ttl).unwrap();
        cache.entries.insert([9u8; 32], exactly_ttl_ago);
        // duration_since(exactly_ttl_ago) == ttl, so `< ttl` is false → not duplicate.
        // The mutant `<= ttl` would say true → duplicate. This catches it.
        assert!(!cache.check_and_insert([9u8; 32]), "entry at exact TTL must be treated as expired");
    }

    #[test]
    fn dedup_cache_ttl_boundary_prune() {
        let ttl = Duration::from_secs(10);
        let mut cache = DedupCache::new(ttl);
        cache.check_and_insert([8u8; 32]);
        // Backdate to exactly TTL ago.
        let exactly_ttl_ago = Instant::now().checked_sub(ttl).unwrap();
        cache.entries.insert([8u8; 32], exactly_ttl_ago);
        cache.prune();
        assert!(cache.entries.is_empty(), "entry at exact TTL must be pruned");
    }
}
