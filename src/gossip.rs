use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bytes::Bytes;
use iroh::endpoint::Incoming;
use iroh::{Endpoint, PublicKey, SecretKey};
use futures_lite::StreamExt;
use iroh_gossip::api::Event;
use iroh_gossip::net::{Gossip as GossipActor, GOSSIP_ALPN};
use iroh_gossip::TopicId;
use mainline::async_dht::AsyncDht;
use mainline::{Dht, MutableItem, SigningKey};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::packet::GossipFrame;

/// Maximum number of peers to store in a DHT record.
/// 28 × 32 bytes = 896 bytes raw. Bencoded: 28 × ~36 = ~1008.
/// Conservative cap to stay under BEP44's 1000-byte value limit.
const MAX_DHT_PEERS: usize = 27;

/// DHT heartbeat interval (base, before jitter).
const DHT_HEARTBEAT_SECS: u64 = 300;

/// Maximum random jitter added to heartbeat interval.
const DHT_HEARTBEAT_JITTER_SECS: u64 = 60;


// ── Key derivation ───────────────────────────────────────────────

/// Cryptographic keys derived from a shared passphrase.
pub struct PassphraseKeys {
    pub topic_id: TopicId,
    pub dht_signing_key: SigningKey,
    pub dht_salt: [u8; 20],
}

impl PassphraseKeys {
    /// Derive all keys from a passphrase.
    pub fn derive(passphrase: &str) -> Self {
        // Application-specific prefix to avoid DHT collisions with other apps.
        const APP_PREFIX: &str = "donglora-bridge:";

        // Topic ID: blake3 hash of prefixed passphrase.
        let topic_bytes = *blake3::hash(format!("{APP_PREFIX}{passphrase}").as_bytes()).as_bytes();
        let topic_id = TopicId::from_bytes(topic_bytes);

        // DHT signing key: ed25519 from SHA-256 of prefixed passphrase + "sign".
        let sign_seed = Sha256::digest(format!("{APP_PREFIX}sign:{passphrase}").as_bytes());
        let seed_bytes: [u8; 32] = sign_seed.into();
        let dht_signing_key = SigningKey::from_bytes(&seed_bytes);

        // DHT salt: first 20 bytes of SHA-256 of prefixed passphrase + "salt".
        let salt_hash = Sha256::digest(format!("{APP_PREFIX}salt:{passphrase}").as_bytes());
        let mut dht_salt = [0u8; 20];
        dht_salt.copy_from_slice(&salt_hash[..20]);

        Self {
            topic_id,
            dht_signing_key,
            dht_salt,
        }
    }
}

// ── Swarm state (for TUI) ───────────────────────────────────────

/// Observable state of the gossip swarm.
#[derive(Debug, Clone)]
pub struct SwarmState {
    pub topic_hash: String,
    pub neighbor_count: usize,
    pub dht_status: DhtStatus,
    pub last_dht_publish: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DhtStatus {
    Bootstrapping,
    Ready,
    PublishFailed,
}

impl std::fmt::Display for DhtStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bootstrapping => write!(f, "Bootstrapping"),
            Self::Ready => write!(f, "Ready"),
            Self::PublishFailed => write!(f, "Failed"),
        }
    }
}

// ── Events to router ────────────────────────────────────────────

/// Events from the gossip swarm to the router.
pub enum GossipEvent {
    /// A gossip frame received from the swarm.
    Frame(GossipFrame),
    /// Neighbor count changed.
    NeighborChanged(usize),
}

// ── Gossip handle ───────────────────────────────────────────────

/// Handle to the gossip swarm. Manages iroh endpoint, gossip subscription, and DHT.
pub struct Gossip {
    endpoint: Endpoint,
    gossip: GossipActor,
    state: Arc<Mutex<SwarmState>>,
    neighbors: Arc<Mutex<HashSet<PublicKey>>>,
    dht_signing_key: SigningKey,
    dht_salt: [u8; 20],
    cancel: CancellationToken,
}

impl Gossip {
    /// Create and start the gossip network.
    ///
    /// Spawns background tasks for connection acceptance, gossip event forwarding,
    /// gossip broadcasting, and DHT heartbeat.
    ///
    /// Returns `(handle, event_rx, frame_tx)`:
    /// - `event_rx`: channel of events from the swarm to the router
    /// - `frame_tx`: channel for the router to send frames to broadcast
    pub async fn new(
        secret_key: SecretKey,
        keys: &PassphraseKeys,
    ) -> Result<(Self, mpsc::Receiver<GossipEvent>, mpsc::Sender<GossipFrame>)> {
        let cancel = CancellationToken::new();

        // Build iroh endpoint with n0 discovery.
        let endpoint = Endpoint::builder(iroh::endpoint::presets::N0)
            .secret_key(secret_key)
            .alpns(vec![GOSSIP_ALPN.to_vec()])
            .bind()
            .await
            .context("binding iroh endpoint")?;

        let my_id = endpoint.id();
        info!("iroh endpoint bound, id: {}", my_id.fmt_short());

        // Build gossip actor.
        let gossip = GossipActor::builder().spawn(endpoint.clone());

        // Bootstrap from DHT.
        let bootstrap_peers = dht_get_peers(keys).await;
        if bootstrap_peers.is_empty() {
            info!("no peers found in DHT — we are the first node");
        } else {
            info!("found {} peer(s) in DHT", bootstrap_peers.len());
        }

        // Subscribe to gossip topic.
        let topic = gossip
            .subscribe(keys.topic_id, bootstrap_peers)
            .await
            .context("subscribing to gossip topic")?;

        let (sender, receiver) = topic.split();

        // Initial swarm state.
        let topic_hash = hex::encode(&keys.topic_id.as_bytes()[..8]);
        let state = Arc::new(Mutex::new(SwarmState {
            topic_hash,
            neighbor_count: 0,
            dht_status: DhtStatus::Bootstrapping,
            last_dht_publish: None,
        }));
        let neighbor_count = Arc::new(AtomicUsize::new(0));
        let neighbors: Arc<Mutex<HashSet<PublicKey>>> = Arc::new(Mutex::new(HashSet::new()));

        // Channels.
        let (event_tx, event_rx) = mpsc::channel::<GossipEvent>(256);
        let (frame_tx, frame_rx) = mpsc::channel::<GossipFrame>(256);

        let handle = Self {
            endpoint: endpoint.clone(),
            gossip: gossip.clone(),
            state: state.clone(),
            neighbors: neighbors.clone(),
            dht_signing_key: keys.dht_signing_key.clone(),
            dht_salt: keys.dht_salt,
            cancel: cancel.clone(),
        };

        // Spawn accept loop.
        spawn_accept_loop(endpoint.clone(), gossip.clone(), cancel.clone());

        // Spawn gossip receiver task.
        spawn_receiver(receiver, event_tx, state.clone(), neighbor_count.clone(), neighbors.clone(), cancel.clone());

        // Clone sender for DHT heartbeat to feed discovered peers via join_peers.
        let dht_sender = sender.clone();

        // Spawn gossip broadcaster task.
        spawn_broadcaster(sender, frame_rx, cancel.clone());

        // Spawn DHT heartbeat.
        spawn_dht_heartbeat(
            my_id,
            keys.dht_signing_key.clone(),
            keys.dht_salt,
            dht_sender,
            neighbors,
            state,
            neighbor_count,
            cancel,
        );

        // Try to join (non-blocking — fire and forget with timeout).
        // The receiver task handles events regardless.

        Ok((handle, event_rx, frame_tx))
    }

    /// Snapshot of swarm state for TUI.
    pub fn swarm_state(&self) -> SwarmState {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Graceful shutdown. Publishes neighbors (without ourselves) to DHT
    /// so other nodes can still bootstrap from live peers.
    pub async fn shutdown(&self) {
        // Publish neighbors-only to DHT before shutting down.
        let neighbor_ids: Vec<[u8; 32]> = self
            .neighbors
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|id| *id.as_bytes())
            .collect();

        if let Ok(dht) = Dht::client() {
            let dht = dht.as_async();
            let value = encode_peer_list(&neighbor_ids);
            let seq = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let item = MutableItem::new(
                self.dht_signing_key.clone(),
                &value,
                seq,
                Some(self.dht_salt.as_ref()),
            );
            match dht.put_mutable(item, None).await {
                Ok(_) => info!("shutdown: published {} neighbor(s) to DHT (removed self)", neighbor_ids.len()),
                Err(e) => debug!("shutdown: DHT publish failed: {e:#}"),
            }
        }

        self.cancel.cancel();
        let _ = self.gossip.shutdown().await;
        self.endpoint.close().await;
    }
}

// ── Background tasks ────────────────────────────────────────────

/// Accept incoming iroh connections and forward gossip ALPN to the gossip actor.
fn spawn_accept_loop(endpoint: Endpoint, gossip: GossipActor, cancel: CancellationToken) {
    tokio::spawn(async move {
        loop {
            let incoming = tokio::select! {
                incoming = endpoint.accept() => incoming,
                () = cancel.cancelled() => break,
            };
            let Some(incoming) = incoming else { break };
            let gossip = gossip.clone();
            tokio::spawn(handle_incoming(incoming, gossip));
        }
        debug!("accept loop stopped");
    });
}

async fn handle_incoming(incoming: Incoming, gossip: GossipActor) {
    let Ok(conn) = incoming.await else { return };
    let alpn = conn.alpn();
    if *alpn == *GOSSIP_ALPN {
        if let Err(e) = gossip.handle_connection(conn).await {
            debug!("gossip handle_connection error: {e:#}");
        }
    } else {
        debug!("unexpected ALPN: {:?}", String::from_utf8_lossy(alpn));
    }
}

/// Forward gossip events to the router via channel.
fn spawn_receiver(
    mut receiver: iroh_gossip::api::GossipReceiver,
    event_tx: mpsc::Sender<GossipEvent>,
    state: Arc<Mutex<SwarmState>>,
    neighbor_count: Arc<AtomicUsize>,
    neighbors: Arc<Mutex<HashSet<PublicKey>>>,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
        loop {
            let event = tokio::select! {
                event = receiver.next() => event,
                () = cancel.cancelled() => break,
            };
            let Some(Ok(event)) = event else { break };
            match event {
                Event::Received(msg) => {
                    if let Some(frame) = GossipFrame::decode(&msg.content) {
                        let _ = event_tx.send(GossipEvent::Frame(frame)).await;
                    } else {
                        debug!("received malformed gossip frame ({} bytes)", msg.content.len());
                    }
                }
                Event::NeighborUp(id) => {
                    let count = neighbor_count.fetch_add(1, Ordering::Relaxed) + 1;
                    info!("neighbor up: {} (total: {count})", id.fmt_short());
                    if let Ok(mut n) = neighbors.lock() { n.insert(id); }
                    if let Ok(mut s) = state.lock() { s.neighbor_count = count; }
                    let _ = event_tx.send(GossipEvent::NeighborChanged(count)).await;
                }
                Event::NeighborDown(id) => {
                    let count = neighbor_count.fetch_sub(1, Ordering::Relaxed).saturating_sub(1);
                    info!("neighbor down: {} (total: {count})", id.fmt_short());
                    if let Ok(mut n) = neighbors.lock() { n.remove(&id); }
                    if let Ok(mut s) = state.lock() { s.neighbor_count = count; }
                    let _ = event_tx.send(GossipEvent::NeighborChanged(count)).await;
                }
                Event::Lagged => {
                    warn!("gossip receiver lagged — some messages were dropped");
                }
            }
        }
        debug!("gossip receiver stopped");
    });
}

/// Read frames from the router and broadcast them to the swarm.
fn spawn_broadcaster(
    sender: iroh_gossip::api::GossipSender,
    mut frame_rx: mpsc::Receiver<GossipFrame>,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
        loop {
            let frame = tokio::select! {
                frame = frame_rx.recv() => frame,
                () = cancel.cancelled() => break,
            };
            let Some(frame) = frame else { break };
            let encoded = Bytes::from(frame.encode());
            if let Err(e) = sender.broadcast(encoded).await {
                debug!("gossip broadcast error: {e:#}");
            }
        }
        debug!("gossip broadcaster stopped");
    });
}

/// Fast DHT interval while we have no neighbors.
const DHT_FAST_INTERVAL_SECS: u64 = 5;

/// Periodically publish our peer list to the DHT and discover new peers.
///
/// Interval is adaptive: fast (5s) while alone, slow (5 min) once connected.
#[allow(clippy::too_many_arguments)]
fn spawn_dht_heartbeat(
    my_id: PublicKey,
    signing_key: SigningKey,
    salt: [u8; 20],
    gossip_sender: iroh_gossip::api::GossipSender,
    neighbors: Arc<Mutex<HashSet<PublicKey>>>,
    state: Arc<Mutex<SwarmState>>,
    neighbor_count: Arc<AtomicUsize>,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
        let dht = match Dht::client() {
            Ok(d) => d.as_async(),
            Err(e) => {
                warn!("failed to create DHT client: {e:#}");
                return;
            }
        };

        let pub_key_bytes = signing_key.verifying_key().to_bytes();

        // Initial publish immediately.
        dht_publish(&dht, &my_id, &signing_key, &salt, &pub_key_bytes, &neighbors, &state).await;

        loop {
            // Fast while alone, slow once we have neighbors.
            let alone = neighbor_count.load(Ordering::Relaxed) == 0;
            let base_secs = if alone {
                DHT_FAST_INTERVAL_SECS
            } else {
                DHT_HEARTBEAT_SECS
            };
            let max_jitter = if alone { 2 } else { DHT_HEARTBEAT_JITTER_SECS };
            let jitter = rand::random::<u64>() % max_jitter.max(1);
            let sleep_dur = Duration::from_secs(base_secs + jitter);

            tokio::select! {
                () = tokio::time::sleep(sleep_dur) => {}
                () = cancel.cancelled() => break,
            }

            // GET: discover new peers and feed to gossip.
            if let Some(record) = dht.get_mutable_most_recent(&pub_key_bytes, Some(&salt)).await {
                let peers = decode_peer_list(record.value());
                let new_peers: Vec<_> = peers
                    .into_iter()
                    .filter(|id| *id != my_id)
                    .collect();
                if !new_peers.is_empty() {
                    info!("DHT discovered {} peer(s), feeding to gossip", new_peers.len());
                    if let Err(e) = gossip_sender.join_peers(new_peers).await {
                        debug!("join_peers error: {e:#}");
                    }
                }
            }

            // PUT: merge-publish our info + neighbors + existing DHT peers.
            dht_publish(&dht, &my_id, &signing_key, &salt, &pub_key_bytes, &neighbors, &state).await;
        }
        debug!("DHT heartbeat stopped");
    });
}

async fn dht_publish(
    dht: &AsyncDht,
    my_id: &PublicKey,
    signing_key: &SigningKey,
    salt: &[u8; 20],
    pub_key_bytes: &[u8; 32],
    neighbors: &Arc<Mutex<HashSet<PublicKey>>>,
    state: &Arc<Mutex<SwarmState>>,
) {
    // Merge: ourselves + live neighbors + existing DHT peers (preserve others' data).
    let mut seen = HashSet::new();
    let mut peer_list: Vec<[u8; 32]> = Vec::with_capacity(MAX_DHT_PEERS);

    // 1. Always include ourselves first.
    peer_list.push(*my_id.as_bytes());
    seen.insert(*my_id.as_bytes());

    // 2. Add our live neighbors.
    if let Ok(n) = neighbors.lock() {
        for id in n.iter() {
            if seen.insert(*id.as_bytes()) && peer_list.len() < MAX_DHT_PEERS {
                peer_list.push(*id.as_bytes());
            }
        }
    }

    // 3. Preserve existing DHT peers we don't already know about.
    if peer_list.len() < MAX_DHT_PEERS
        && let Some(record) = dht.get_mutable_most_recent(pub_key_bytes, Some(salt)).await
    {
        for existing in decode_peer_list(record.value()) {
            if seen.insert(*existing.as_bytes()) && peer_list.len() < MAX_DHT_PEERS {
                peer_list.push(*existing.as_bytes());
            }
        }
    }

    let value = encode_peer_list(&peer_list);
    let seq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let item = MutableItem::new(signing_key.clone(), &value, seq, Some(salt.as_ref()));
    match dht.put_mutable(item, None).await {
        Ok(_) => {
            debug!("published to DHT (seq={seq}, peers={})", peer_list.len());
            if let Ok(mut s) = state.lock() {
                s.dht_status = DhtStatus::Ready;
                s.last_dht_publish = Some(Instant::now());
            }
        }
        Err(e) => {
            warn!("DHT publish failed: {e:#}");
            if let Ok(mut s) = state.lock() {
                s.dht_status = DhtStatus::PublishFailed;
            }
        }
    }
}

// ── DHT helpers ─────────────────────────────────────────────────

/// Query the DHT for existing peers (blocking bootstrap, called once on startup).
async fn dht_get_peers(keys: &PassphraseKeys) -> Vec<PublicKey> {
    let dht = match Dht::client() {
        Ok(d) => d.as_async(),
        Err(e) => {
            warn!("failed to create DHT client for bootstrap: {e:#}");
            return vec![];
        }
    };

    let pub_key_bytes = keys.dht_signing_key.verifying_key().to_bytes();
    match dht.get_mutable_most_recent(&pub_key_bytes, Some(&keys.dht_salt)).await {
        Some(record) => {
            let peers = decode_peer_list(record.value());
            debug!("DHT bootstrap: found {} peer(s)", peers.len());
            peers
        }
        None => {
            debug!("DHT bootstrap: no existing record");
            vec![]
        }
    }
}

/// Encode a list of 32-byte peer IDs into bytes for DHT storage.
/// Format: [count: u16 LE] [id_0: 32B] [id_1: 32B] ...
fn encode_peer_list(peers: &[[u8; 32]]) -> Vec<u8> {
    let count = peers.len().min(MAX_DHT_PEERS) as u16;
    let mut buf = Vec::with_capacity(2 + (count as usize) * 32);
    buf.extend_from_slice(&count.to_le_bytes());
    for id in peers.iter().take(MAX_DHT_PEERS) {
        buf.extend_from_slice(id);
    }
    buf
}

/// Decode a peer list from DHT record value.
fn decode_peer_list(data: &[u8]) -> Vec<PublicKey> {
    if data.len() < 2 {
        return vec![];
    }
    let count = u16::from_le_bytes([data[0], data[1]]) as usize;
    let mut peers = Vec::with_capacity(count);
    let mut offset = 2;
    for _ in 0..count {
        if offset + 32 > data.len() {
            break;
        }
        let bytes: [u8; 32] = match data[offset..offset + 32].try_into() {
            Ok(b) => b,
            Err(_) => break,
        };
        if let Ok(pk) = PublicKey::from_bytes(&bytes) {
            peers.push(pk);
        }
        offset += 32;
    }
    peers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passphrase_keys_deterministic() {
        let k1 = PassphraseKeys::derive("test-passphrase");
        let k2 = PassphraseKeys::derive("test-passphrase");
        assert_eq!(k1.topic_id, k2.topic_id);
        assert_eq!(k1.dht_signing_key.to_bytes(), k2.dht_signing_key.to_bytes());
        assert_eq!(k1.dht_salt, k2.dht_salt);
    }

    #[test]
    fn passphrase_keys_different() {
        let k1 = PassphraseKeys::derive("alpha");
        let k2 = PassphraseKeys::derive("beta");
        assert_ne!(k1.topic_id, k2.topic_id);
        assert_ne!(k1.dht_signing_key.to_bytes(), k2.dht_signing_key.to_bytes());
    }

    #[test]
    fn peer_list_roundtrip() {
        // Generate valid ed25519 public keys.
        let keys: Vec<_> = (0..3)
            .map(|_| {
                let sk = iroh::SecretKey::generate(&mut rand::rng());
                *sk.public().as_bytes()
            })
            .collect();
        let encoded = encode_peer_list(&keys);
        let decoded = decode_peer_list(&encoded);
        assert_eq!(decoded.len(), 3);
        for (i, key) in keys.iter().enumerate() {
            assert_eq!(*decoded[i].as_bytes(), *key);
        }
    }

    #[test]
    fn peer_list_empty() {
        let encoded = encode_peer_list(&[]);
        let decoded = decode_peer_list(&encoded);
        assert!(decoded.is_empty());
    }

    #[test]
    fn peer_list_truncated_data() {
        // Only 1 byte — too short for count.
        assert!(decode_peer_list(&[0]).is_empty());
        // Count says 2 but only 1 peer present.
        let mut data = vec![2, 0]; // count = 2
        data.extend_from_slice(&[0xAA; 32]); // only 1 peer
        let decoded = decode_peer_list(&data);
        assert_eq!(decoded.len(), 1);
    }
}
