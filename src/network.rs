use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use bytes::Bytes;
use iroh::endpoint::Connection;
use iroh::{Endpoint, PublicKey, SecretKey, endpoint::presets};
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::packet::Envelope;

/// ALPN protocol identifier for donglora-bridge.
const ALPN: &[u8] = b"donglora-bridge/0";

/// Info about a configured peer.
#[derive(Debug, Clone)]
pub struct PeerConfig {
    pub public_key: PublicKey,
    pub name: String,
    pub addresses: Vec<String>,
}

/// Observable state of a peer connection.
#[derive(Debug, Clone)]
pub struct PeerState {
    pub name: String,
    pub public_key: PublicKey,
    pub connected: bool,
    pub last_seen: Option<Instant>,
    pub packets_in: u64,
    pub packets_out: u64,
}

/// Event from the network to the router.
#[allow(dead_code)]
pub enum NetworkEvent {
    /// A packet received from an iroh peer.
    Packet {
        from: PublicKey,
        envelope: Envelope,
    },
    /// A peer connected.
    PeerConnected(PublicKey),
    /// A peer disconnected.
    PeerDisconnected(PublicKey),
}

/// Shared peer connection map.
type PeerConnections = Arc<Mutex<HashMap<PublicKey, Connection>>>;

/// Shared peer state map.
type PeerStates = Arc<Mutex<HashMap<PublicKey, PeerState>>>;

/// The network layer: manages the iroh endpoint and peer connections.
pub struct Network {
    endpoint: Endpoint,
    peer_configs: Vec<PeerConfig>,
    connections: PeerConnections,
    states: PeerStates,
    cancel: CancellationToken,
}

impl Network {
    /// Create a new network layer with the given identity and peer configuration.
    pub async fn new(secret_key: SecretKey, peer_configs: Vec<PeerConfig>) -> Result<Self> {
        let endpoint = Endpoint::builder(presets::N0)
            .secret_key(secret_key)
            .alpns(vec![ALPN.to_vec()])
            .bind()
            .await
            .context("binding iroh endpoint")?;

        // Wait for relay connection before trying to reach peers.
        info!("waiting for iroh relay connection...");
        endpoint.online().await;
        let addr = endpoint.addr();
        info!("iroh online, addr: {addr:?}");

        let states = Arc::new(Mutex::new(HashMap::new()));
        {
            let mut s = states.lock().await;
            for pc in &peer_configs {
                s.insert(
                    pc.public_key,
                    PeerState {
                        name: pc.name.clone(),
                        public_key: pc.public_key,
                        connected: false,
                        last_seen: None,
                        packets_in: 0,
                        packets_out: 0,
                    },
                );
            }
        }

        Ok(Self {
            endpoint,
            peer_configs,
            connections: Arc::new(Mutex::new(HashMap::new())),
            states,
            cancel: CancellationToken::new(),
        })
    }

    /// Get a snapshot of all peer states.
    pub async fn peer_states(&self) -> Vec<PeerState> {
        self.states.lock().await.values().cloned().collect()
    }

    /// Start the network: spawn outbound connection tasks + inbound accept loop.
    ///
    /// Returns a receiver for network events and a sender for outbound envelopes.
    pub fn start(
        &self,
    ) -> (mpsc::Receiver<NetworkEvent>, mpsc::Sender<(Envelope, Option<PublicKey>)>) {
        let (event_tx, event_rx) = mpsc::channel::<NetworkEvent>(256);
        let (send_tx, send_rx) = mpsc::channel::<(Envelope, Option<PublicKey>)>(256);

        // Spawn outbound connection tasks for each configured peer.
        for pc in &self.peer_configs {
            let endpoint = self.endpoint.clone();
            let pk = pc.public_key;
            let addrs = pc.addresses.clone();
            let connections = self.connections.clone();
            let states = self.states.clone();
            let event_tx = event_tx.clone();
            let cancel = self.cancel.clone();

            tokio::spawn(async move {
                outbound_connection_loop(endpoint, pk, addrs, connections, states, event_tx, cancel).await;
            });
        }

        // Spawn inbound accept loop.
        {
            let endpoint = self.endpoint.clone();
            let connections = self.connections.clone();
            let states = self.states.clone();
            let event_tx = event_tx.clone();
            let cancel = self.cancel.clone();
            let known_peers: Vec<PublicKey> =
                self.peer_configs.iter().map(|pc| pc.public_key).collect();

            tokio::spawn(async move {
                accept_loop(endpoint, known_peers, connections, states, event_tx, cancel).await;
            });
        }

        // Spawn broadcast sender task.
        {
            let connections = self.connections.clone();
            let states = self.states.clone();
            let cancel = self.cancel.clone();

            tokio::spawn(async move {
                broadcast_loop(connections, states, send_rx, cancel).await;
            });
        }

        (event_rx, send_tx)
    }

    /// Shut down the network layer.
    pub async fn shutdown(&self) {
        self.cancel.cancel();
        self.endpoint.close().await;
    }
}

/// Maintain a persistent outbound connection to a peer, reconnecting on failure.
async fn outbound_connection_loop(
    endpoint: Endpoint,
    peer_key: PublicKey,
    addresses: Vec<String>,
    connections: PeerConnections,
    states: PeerStates,
    event_tx: mpsc::Sender<NetworkEvent>,
    cancel: CancellationToken,
) {
    let mut backoff = std::time::Duration::from_secs(1);
    let max_backoff = std::time::Duration::from_secs(30);

    loop {
        if cancel.is_cancelled() {
            return;
        }

        // Skip if we already have a connection (e.g., from inbound accept).
        {
            let conns = connections.lock().await;
            if let Some(c) = conns.get(&peer_key)
                && c.stable_id() != 0
            {
                // Connection exists and looks alive — wait and retry check.
                drop(conns);
                tokio::select! {
                    () = cancel.cancelled() => return,
                    () = tokio::time::sleep(std::time::Duration::from_secs(5)) => continue,
                }
            }
        }

        info!("connecting to peer {}", fmt_key(&peer_key));
        let mut addr = iroh::EndpointAddr::new(peer_key);
        // Add our relay URL as a hint — the peer is likely on the same relay network.
        for relay_url in endpoint.addr().relay_urls() {
            addr = addr.with_relay_url(relay_url.clone());
        }
        for a in &addresses {
            if let Ok(sock) = a.parse::<std::net::SocketAddr>() {
                addr = addr.with_ip_addr(sock);
            }
        }
        debug!("connect addr: {addr:?}");
        match endpoint.connect(addr, ALPN).await {
            Ok(conn) => {
                backoff = std::time::Duration::from_secs(1);
                let remote = conn.remote_id();
                info!("connected to peer {}", fmt_key(&remote));

                {
                    let mut conns = connections.lock().await;
                    conns.insert(remote, conn.clone());
                }
                update_peer_connected(&states, &remote, true).await;
                let _ = event_tx.send(NetworkEvent::PeerConnected(remote)).await;

                // Read datagrams from this connection.
                read_datagrams(conn, remote, connections.clone(), states.clone(), event_tx.clone(), cancel.clone()).await;

                update_peer_connected(&states, &remote, false).await;
                let _ = event_tx
                    .send(NetworkEvent::PeerDisconnected(remote))
                    .await;
            }
            Err(e) => {
                warn!("connect to {} failed: {e:#}", fmt_key(&peer_key));
            }
        }

        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(backoff) => {},
        }
        backoff = (backoff * 2).min(max_backoff);
    }
}

/// Accept inbound connections from known peers.
async fn accept_loop(
    endpoint: Endpoint,
    known_peers: Vec<PublicKey>,
    connections: PeerConnections,
    states: PeerStates,
    event_tx: mpsc::Sender<NetworkEvent>,
    cancel: CancellationToken,
) {
    loop {
        let incoming = tokio::select! {
            () = cancel.cancelled() => return,
            inc = endpoint.accept() => match inc {
                Some(i) => i,
                None => return,
            },
        };

        let conn = match incoming.await {
            Ok(c) => c,
            Err(e) => {
                warn!("inbound connection failed: {e:#}");
                continue;
            }
        };

        let remote = conn.remote_id();
        if !known_peers.contains(&remote) {
            warn!("rejecting unknown peer {}", fmt_key(&remote));
            conn.close(0u32.into(), b"unknown peer");
            continue;
        }

        info!("accepted connection from peer {}", fmt_key(&remote));
        {
            let mut conns = connections.lock().await;
            conns.insert(remote, conn.clone());
        }
        update_peer_connected(&states, &remote, true).await;
        let _ = event_tx.send(NetworkEvent::PeerConnected(remote)).await;

        // Spawn a reader for this inbound connection.
        let conns = connections.clone();
        let sts = states.clone();
        let etx = event_tx.clone();
        let ct = cancel.clone();
        tokio::spawn(async move {
            read_datagrams(conn, remote, conns.clone(), sts.clone(), etx.clone(), ct).await;
            update_peer_connected(&sts, &remote, false).await;
            let _ = etx.send(NetworkEvent::PeerDisconnected(remote)).await;
        });
    }
}

/// Read datagrams from a connection until it closes.
async fn read_datagrams(
    conn: Connection,
    remote: PublicKey,
    connections: PeerConnections,
    states: PeerStates,
    event_tx: mpsc::Sender<NetworkEvent>,
    cancel: CancellationToken,
) {
    loop {
        let data = tokio::select! {
            () = cancel.cancelled() => break,
            result = conn.read_datagram() => match result {
                Ok(d) => d,
                Err(e) => {
                    debug!("datagram read from {} ended: {e}", fmt_key(&remote));
                    break;
                }
            },
        };

        if let Some(envelope) = Envelope::decode(&data) {
            {
                let mut sts = states.lock().await;
                if let Some(ps) = sts.get_mut(&remote) {
                    ps.packets_in += 1;
                    ps.last_seen = Some(Instant::now());
                }
            }
            let _ = event_tx
                .send(NetworkEvent::Packet {
                    from: remote,
                    envelope,
                })
                .await;
        } else {
            warn!("malformed envelope from {}", fmt_key(&remote));
        }
    }

    // Clean up connection.
    let mut conns = connections.lock().await;
    if let Some(existing) = conns.get(&remote)
        && existing.stable_id() == conn.stable_id()
    {
        conns.remove(&remote);
    }
}

/// Broadcast envelopes to all connected peers (except the optional skip key).
async fn broadcast_loop(
    connections: PeerConnections,
    states: PeerStates,
    mut send_rx: mpsc::Receiver<(Envelope, Option<PublicKey>)>,
    cancel: CancellationToken,
) {
    loop {
        let (envelope, skip) = tokio::select! {
            () = cancel.cancelled() => return,
            msg = send_rx.recv() => match msg {
                Some(m) => m,
                None => return,
            },
        };

        let data = Bytes::from(envelope.encode());
        let conns = connections.lock().await;

        for (pk, conn) in conns.iter() {
            if Some(*pk) == skip {
                continue;
            }
            match conn.send_datagram(data.clone()) {
                Ok(()) => {
                    let mut sts = states.lock().await;
                    if let Some(ps) = sts.get_mut(pk) {
                        ps.packets_out += 1;
                    }
                }
                Err(e) => {
                    debug!("send to {} failed: {e}", fmt_key(pk));
                }
            }
        }
    }
}

async fn update_peer_connected(states: &PeerStates, pk: &PublicKey, connected: bool) {
    let mut sts = states.lock().await;
    if let Some(ps) = sts.get_mut(pk) {
        ps.connected = connected;
        if connected {
            ps.last_seen = Some(Instant::now());
        }
    }
}

fn fmt_key(pk: &PublicKey) -> String {
    let s = pk.to_string();
    if s.len() > 12 {
        format!("{}...", &s[..12])
    } else {
        s
    }
}
