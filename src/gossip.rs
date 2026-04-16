//! iroh-gossip swarm management and mainline DHT peer discovery.
//!
//! Derives cryptographic keys from a shared passphrase, manages the gossip
//! subscription, and maintains a DHT heartbeat for zero-configuration peer
//! bootstrapping.

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bytes::Bytes;
use futures_lite::StreamExt;
use iroh::endpoint::Incoming;
use iroh::{Endpoint, PublicKey, SecretKey};
use iroh_gossip::TopicId;
use iroh_gossip::api::Event;
use iroh_gossip::net::{GOSSIP_ALPN, Gossip as GossipActor};
use mainline::async_dht::AsyncDht;
use mainline::{Dht, MutableItem, SigningKey};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::packet::GossipFrame;

/// Maximum number of peers to store in a DHT record.
/// 28 × 32 bytes = 896 bytes raw. Bencoded: 28 × ~36 = ~1008.
/// Conservative cap to stay under BEP44's 1000-byte value limit.
const MAX_DHT_PEERS: usize = 27;

/// DHT heartbeat interval (base, before jitter). Each bridge periodically
/// publishes `[self] + live_neighbors` as a blind overwrite. Any live bridge
/// publishing at this cadence guarantees that the DHT record is overwritten
/// with a fresh snapshot of a real, connected sub-swarm within this window —
/// so stale entries from crashed / restarted peers age out within one cycle.
const DHT_HEARTBEAT_SECS: u64 = 60;

/// Maximum random jitter added to heartbeat. Prevents synchronized publish
/// storms when multiple bridges start simultaneously.
const DHT_HEARTBEAT_JITTER_SECS: u64 = 15;

/// DHT read interval while we have no neighbors. Stays aggressive so two
/// freshly-started bridges find each other quickly.
const DHT_LONELY_READ_SECS: u64 = 3;

/// How long to try dialing DHT-discovered peers before we give up and publish
/// ourselves alone. If we connect to at least one peer in this window we
/// publish `[self, neighbors...]` right away. If we don't, we publish `[self]`
/// so newcomers find us — but we delay this so we don't overwrite a
/// potentially useful DHT record with just our own lonely entry.
const DHT_BOOTSTRAP_GRACE_SECS: u64 = 10;

// ── Key derivation ───────────────────────────────────────────────

/// Cryptographic keys derived from a shared passphrase.
pub struct PassphraseKeys {
    pub topic_id: TopicId,
    pub dht_signing_key: SigningKey,
    pub dht_salt: [u8; 20],
}

impl PassphraseKeys {
    /// Derive all keys from a passphrase.
    ///
    /// Intermediate buffers containing the passphrase are zeroized after use to
    /// minimize time sensitive material spends in memory.
    #[must_use]
    pub fn derive(passphrase: &str) -> Self {
        // Application-specific prefix to avoid DHT collisions with other apps.
        const APP_PREFIX: &[u8] = b"donglora-bridge:";

        // Helper: build a zeroized buffer of [prefix | suffix | passphrase].
        let keyed_buf = |suffix: &[u8]| -> Vec<u8> {
            let mut buf = Vec::with_capacity(APP_PREFIX.len() + suffix.len() + passphrase.len());
            buf.extend_from_slice(APP_PREFIX);
            buf.extend_from_slice(suffix);
            buf.extend_from_slice(passphrase.as_bytes());
            buf
        };

        // Topic ID: blake3 hash of prefixed passphrase.
        let mut buf = keyed_buf(b"");
        let topic_bytes = *blake3::hash(&buf).as_bytes();
        let topic_id = TopicId::from_bytes(topic_bytes);
        zeroize::Zeroize::zeroize(&mut buf);

        // DHT signing key: ed25519 from SHA-256 of prefixed passphrase + "sign:".
        let mut buf = keyed_buf(b"sign:");
        let sign_seed = Sha256::digest(&buf);
        let seed_bytes: [u8; 32] = sign_seed.into();
        let dht_signing_key = SigningKey::from_bytes(&seed_bytes);
        zeroize::Zeroize::zeroize(&mut buf);

        // DHT salt: first 20 bytes of SHA-256 of prefixed passphrase + "salt:".
        let mut buf = keyed_buf(b"salt:");
        let salt_hash = Sha256::digest(&buf);
        let mut dht_salt = [0u8; 20];
        dht_salt.copy_from_slice(&salt_hash[..20]);
        zeroize::Zeroize::zeroize(&mut buf);

        Self { topic_id, dht_signing_key, dht_salt }
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

/// Status of the mainline DHT connection.
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
    actor: GossipActor,
    // std::sync::Mutex (not tokio::sync::Mutex) because locks are never held across
    // .await points. This avoids async-aware mutex overhead for brief critical sections.
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
        let actor = GossipActor::builder().spawn(endpoint.clone());

        // Bootstrap from DHT.
        let bootstrap_peers = dht_get_peers(keys).await;
        if bootstrap_peers.is_empty() {
            info!("no peers found in DHT — we are the first node");
        } else {
            info!("found {} peer(s) in DHT", bootstrap_peers.len());
        }

        // Subscribe to gossip topic.
        let topic = actor.subscribe(keys.topic_id, bootstrap_peers).await.context("subscribing to gossip topic")?;

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
            actor: actor.clone(),
            state: state.clone(),
            neighbors: neighbors.clone(),
            dht_signing_key: keys.dht_signing_key.clone(),
            dht_salt: keys.dht_salt,
            cancel: cancel.clone(),
        };

        // Spawn accept loop.
        spawn_accept_loop(endpoint.clone(), actor.clone(), cancel.clone());

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
    #[must_use]
    pub fn swarm_state(&self) -> SwarmState {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }

    /// Graceful shutdown. Publishes neighbors (without ourselves) to DHT
    /// so other nodes can still bootstrap from live peers.
    pub async fn shutdown(&self) {
        // Publish neighbors-only to DHT before shutting down.
        let neighbor_ids: Vec<[u8; 32]> = self
            .neighbors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|id| *id.as_bytes())
            .collect();

        if let Ok(dht) = Dht::client() {
            let dht = dht.as_async();
            let value = encode_peer_list(&neighbor_ids);
            let seq = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs().cast_signed())
                .unwrap_or(0);
            let item = MutableItem::new(self.dht_signing_key.clone(), &value, seq, Some(self.dht_salt.as_ref()));
            match dht.put_mutable(item, None).await {
                Ok(_) => info!("shutdown: published {} neighbor(s) to DHT (removed self)", neighbor_ids.len()),
                Err(e) => debug!("shutdown: DHT publish failed: {e:#}"),
            }
        }

        self.cancel.cancel();
        let _ = self.actor.shutdown().await;
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
        // Log a hard warning if the receiver stream ever ends — this is the
        // "silent one-way gossip" failure mode. Track why it exited so the
        // log tells us whether it was a stream-end (None), an error
        // (Some(Err)), or a cancel.
        let exit_reason: &'static str = loop {
            let event = tokio::select! {
                event = receiver.next() => event,
                () = cancel.cancelled() => break "cancelled",
            };
            let event = match event {
                None => break "stream ended (receiver closed)",
                Some(Err(e)) => {
                    error!("gossip receiver error: {e:#}");
                    break "receiver error";
                }
                Some(Ok(ev)) => ev,
            };
            match event {
                Event::Received(msg) => {
                    if let Some(frame) = GossipFrame::decode(&msg.content) {
                        let _ = event_tx.send(GossipEvent::Frame(frame)).await;
                    } else {
                        debug!("received malformed gossip frame ({} bytes)", msg.content.len());
                    }
                }
                // Neighbor count is tracked in both AtomicUsize (for lock-free reads in DHT
                // heartbeat) and SwarmState (for TUI snapshots). Brief inconsistency between
                // the two is acceptable — the TUI polls at 250ms intervals.
                Event::NeighborUp(id) => {
                    let count = neighbor_count.fetch_add(1, Ordering::Relaxed) + 1;
                    info!("neighbor up: {} (total: {count})", id.fmt_short());
                    if let Ok(mut n) = neighbors.lock() {
                        n.insert(id);
                    }
                    if let Ok(mut s) = state.lock() {
                        s.neighbor_count = count;
                    }
                    let _ = event_tx.send(GossipEvent::NeighborChanged(count)).await;
                }
                Event::NeighborDown(id) => {
                    let count = neighbor_count.fetch_sub(1, Ordering::Relaxed).saturating_sub(1);
                    info!("neighbor down: {} (total: {count})", id.fmt_short());
                    if let Ok(mut n) = neighbors.lock() {
                        n.remove(&id);
                    }
                    if let Ok(mut s) = state.lock() {
                        s.neighbor_count = count;
                    }
                    let _ = event_tx.send(GossipEvent::NeighborChanged(count)).await;
                }
                Event::Lagged => {
                    warn!("gossip receiver lagged — some messages were dropped");
                }
            }
        };
        // A shutdown cancel is expected; any other exit means we silently
        // stopped receiving gossip. That's the "one-way bridge" bug —
        // broadcast still works because spawn_broadcaster is independent.
        if exit_reason == "cancelled" {
            debug!("gossip receiver stopped ({exit_reason})");
        } else {
            error!("gossip receiver STOPPED ({exit_reason}) — this bridge will no longer receive from the swarm",);
        }
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

/// Periodically publish `[self] + live_neighbors` to the DHT and read it to
/// discover new peers.
///
/// Strategy — deliberately simple:
/// 1. **Startup**: read DHT, try to join discovered peers. Do NOT publish
///    yet — we don't know if any of them are still alive, and publishing
///    `[self]` alone would overwrite a potentially useful record.
/// 2. **Bootstrap grace** (10 s): keep reading fast (every
///    `DHT_LONELY_READ_SECS`) looking for peers to connect to. If a
///    `NeighborUp` fires inside this window, publish immediately with the
///    live neighbor set. If nothing shows up, publish `[self]` so anyone
///    else joining finds us.
/// 3. **Steady state**: publish `[self, neighbors...]` every
///    `DHT_HEARTBEAT_SECS` (+ jitter), a blind overwrite. Any live bridge
///    doing this naturally ages out crashed/stale peers from the DHT
///    record within one cycle.
/// 4. **Re-read cadence**: fast (`DHT_LONELY_READ_SECS`) while
///    `neighbor_count == 0`, relaxed (= heartbeat interval) once
///    connected.
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
        let mut last_publish: Option<Instant> = None;
        let mut was_alone = true;
        let started = Instant::now();

        // Step 1: read the DHT to find peers to dial.
        dht_read_and_join(&dht, &my_id, &pub_key_bytes, &salt, &gossip_sender, &state).await;

        loop {
            let alone = neighbor_count.load(Ordering::Relaxed) == 0;

            // Transition: alone → connected. Publish immediately with live
            // neighbors so anyone who was about to dial us-as-lonely finds
            // the full picture.
            if was_alone && !alone {
                info!("first neighbor joined — publishing to DHT");
                dht_publish(&dht, &my_id, &signing_key, &salt, &neighbors, &state).await;
                last_publish = Some(Instant::now());
            }
            was_alone = alone;

            // Sleep: fast reads while lonely, relaxed once connected.
            let sleep_dur = if alone {
                Duration::from_secs(DHT_LONELY_READ_SECS)
            } else {
                let jitter = rand::random::<u64>() % DHT_HEARTBEAT_JITTER_SECS.max(1);
                Duration::from_secs(DHT_HEARTBEAT_SECS + jitter)
            };

            tokio::select! {
                () = tokio::time::sleep(sleep_dur) => {}
                () = cancel.cancelled() => break,
            }

            // Always read — cheap, keeps fresh peers coming in.
            dht_read_and_join(&dht, &my_id, &pub_key_bytes, &salt, &gossip_sender, &state).await;

            // Steady-state: publish on every heartbeat tick once we've
            // published at least once. Pre-first-publish, hold off until
            // either a neighbor appears OR the bootstrap grace elapses —
            // this avoids wiping a potentially-useful DHT record with
            // just our lonely [self] entry on startup.
            let should_publish = last_publish.map_or_else(
                || !alone || started.elapsed() >= Duration::from_secs(DHT_BOOTSTRAP_GRACE_SECS),
                |t| t.elapsed() >= Duration::from_secs(DHT_HEARTBEAT_SECS),
            );
            if should_publish {
                dht_publish(&dht, &my_id, &signing_key, &salt, &neighbors, &state).await;
                last_publish = Some(Instant::now());
            }
        }
        debug!("DHT heartbeat stopped");
    });
}

/// Read the DHT and feed any discovered peers into the gossip swarm.
async fn dht_read_and_join(
    dht: &AsyncDht,
    my_id: &PublicKey,
    pub_key_bytes: &[u8; 32],
    salt: &[u8; 20],
    gossip_sender: &iroh_gossip::api::GossipSender,
    state: &Arc<Mutex<SwarmState>>,
) {
    if let Some(record) = dht.get_mutable_most_recent(pub_key_bytes, Some(salt)).await {
        let peers = decode_peer_list(record.value());
        let new_peers: Vec<_> = peers.into_iter().filter(|id| *id != *my_id).collect();
        if !new_peers.is_empty() {
            info!("DHT discovered {} peer(s), feeding to gossip", new_peers.len());
            if let Err(e) = gossip_sender.join_peers(new_peers).await {
                debug!("join_peers error: {e:#}");
            }
        }
        if let Ok(mut s) = state.lock() {
            s.dht_status = DhtStatus::Ready;
        }
    }
}

/// Publish `[self] + live_neighbors` to the DHT as a blind overwrite.
///
/// No merge with existing DHT content. Stale peers age out naturally
/// because every live bridge publishes a snapshot of its actual live
/// connections within `DHT_HEARTBEAT_SECS`, and higher-seq records win.
async fn dht_publish(
    dht: &AsyncDht,
    my_id: &PublicKey,
    signing_key: &SigningKey,
    salt: &[u8; 20],
    neighbors: &Arc<Mutex<HashSet<PublicKey>>>,
    state: &Arc<Mutex<SwarmState>>,
) {
    let mut peer_list: Vec<[u8; 32]> = Vec::with_capacity(MAX_DHT_PEERS);
    peer_list.push(*my_id.as_bytes());

    // Snapshot current live neighbors — those we have an active gossip
    // connection to right now. No inherited junk from prior DHT content.
    if let Ok(n) = neighbors.lock() {
        for id in n.iter() {
            if peer_list.len() >= MAX_DHT_PEERS {
                break;
            }
            peer_list.push(*id.as_bytes());
        }
    }

    let value = encode_peer_list(&peer_list);
    let seq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().cast_signed())
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
    dht.get_mutable_most_recent(&pub_key_bytes, Some(&keys.dht_salt)).await.map_or_else(
        || {
            debug!("DHT bootstrap: no existing record");
            vec![]
        },
        |record| {
            let peers = decode_peer_list(record.value());
            debug!("DHT bootstrap: found {} peer(s)", peers.len());
            peers
        },
    )
}

/// Encode a list of 32-byte peer IDs into bytes for DHT storage.
/// Format: `[count: u16 LE] [id_0: 32B] [id_1: 32B] ...`
#[must_use]
pub fn encode_peer_list(peers: &[[u8; 32]]) -> Vec<u8> {
    #[allow(clippy::cast_possible_truncation)] // MAX_DHT_PEERS (27) fits in u16
    let count = peers.len().min(MAX_DHT_PEERS) as u16;
    let mut buf = Vec::with_capacity(2 + (count as usize) * 32);
    buf.extend_from_slice(&count.to_le_bytes());
    for id in peers.iter().take(MAX_DHT_PEERS) {
        buf.extend_from_slice(id);
    }
    buf
}

/// Decode a peer list from DHT record value.
#[must_use]
pub fn decode_peer_list(data: &[u8]) -> Vec<PublicKey> {
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

    #[test]
    fn dht_status_display() {
        assert_eq!(format!("{}", DhtStatus::Bootstrapping), "Bootstrapping");
        assert_eq!(format!("{}", DhtStatus::Ready), "Ready");
        assert_eq!(format!("{}", DhtStatus::PublishFailed), "Failed");
    }

    #[test]
    fn decode_peer_list_exact_boundary() {
        // Build data with count=2 and exactly 2 + 32 + 32 = 66 bytes.
        // Both peers should decode successfully.
        let sk0 = iroh::SecretKey::generate(&mut rand::rng());
        let sk1 = iroh::SecretKey::generate(&mut rand::rng());
        let id0 = *sk0.public().as_bytes();
        let id1 = *sk1.public().as_bytes();

        let mut data = vec![2, 0]; // count = 2 (u16 LE)
        data.extend_from_slice(&id0);
        data.extend_from_slice(&id1);
        assert_eq!(data.len(), 66);

        let decoded = decode_peer_list(&data);
        assert_eq!(decoded.len(), 2);
        assert_eq!(*decoded[0].as_bytes(), id0);
        assert_eq!(*decoded[1].as_bytes(), id1);
    }

    #[test]
    fn decode_peer_list_one_byte_short() {
        // Build data with count=2 but only 2 + 32 + 31 = 65 bytes.
        // First peer decodes, second is truncated and skipped.
        let sk0 = iroh::SecretKey::generate(&mut rand::rng());
        let sk1 = iroh::SecretKey::generate(&mut rand::rng());
        let id0 = *sk0.public().as_bytes();
        let id1 = *sk1.public().as_bytes();

        let mut data = vec![2, 0]; // count = 2 (u16 LE)
        data.extend_from_slice(&id0);
        data.extend_from_slice(&id1[..31]); // only 31 bytes of second peer
        assert_eq!(data.len(), 65);

        let decoded = decode_peer_list(&data);
        assert_eq!(decoded.len(), 1);
        assert_eq!(*decoded[0].as_bytes(), id0);
    }

    #[test]
    fn decode_peer_list_count_header_only() {
        // Data with exactly 2 bytes (count=0). Must not be rejected by the length check.
        // Kills mutant: `data.len() < 2` → `data.len() <= 2`.
        let data = [0u8, 0]; // count = 0
        let decoded = decode_peer_list(&data);
        assert!(decoded.is_empty());

        // Data with 2 bytes but count=1 and no peer data — decodes 0 (truncated).
        let data = [1u8, 0]; // count = 1 but no peer bytes
        let decoded = decode_peer_list(&data);
        assert!(decoded.is_empty());
    }

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn peer_list_roundtrip_prop(count in 0usize..=30) {
                let keys: Vec<[u8; 32]> = (0..count)
                    .map(|_| *iroh::SecretKey::generate(&mut rand::rng()).public().as_bytes())
                    .collect();
                let encoded = encode_peer_list(&keys);
                let decoded = decode_peer_list(&encoded);
                let expected = count.min(MAX_DHT_PEERS);
                prop_assert_eq!(decoded.len(), expected);
            }
        }
    }
}
