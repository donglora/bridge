//! Bridge-side glue over the `iroh-gossip-rendezvous` crate.
//!
//! Starts the rendezvous, spawns a task that decodes incoming gossip events
//! into [`GossipEvent`]s the router understands, and a broadcaster task that
//! encodes outgoing [`GossipFrame`]s onto the iroh-gossip sender.

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::{Context, Result};
use bytes::Bytes;
use iroh::{PublicKey, SecretKey};
use iroh_gossip::api::Event;
use iroh_gossip_rendezvous::Rendezvous;
pub use iroh_gossip_rendezvous::{DhtStatus, RendezvousState};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, warn};

use crate::packet::GossipFrame;

/// App salt for the donglora-bridge rendezvous surface. Versioned so a
/// future breaking change gets a fresh DHT meeting place — v1 and v2
/// bridges do not see each other.
pub const APP_SALT: &str = "donglora-bridge/v2";

/// Events from the gossip swarm to the router.
pub enum GossipEvent {
    /// A gossip frame received from the swarm.
    Frame(GossipFrame),
    /// Neighbor count changed.
    NeighborChanged(usize),
}

/// Handle held by the rest of the bridge. Owns the `Rendezvous` that in turn
/// owns all DHT + gossip background work. Dropping this stops everything.
pub struct Gossip {
    rendezvous: Rendezvous,
    cancel: CancellationToken,
}

impl Gossip {
    /// Spin up the gossip swarm + DHT rendezvous.
    ///
    /// Returns `(handle, event_rx, frame_tx)`:
    /// - `event_rx`: channel of events from the swarm to the router
    /// - `frame_tx`: channel for the router to send frames to broadcast
    pub async fn new(
        secret_key: SecretKey,
        passphrase: &str,
    ) -> Result<(Self, mpsc::Receiver<GossipEvent>, mpsc::Sender<GossipFrame>)> {
        let cancel = CancellationToken::new();

        let rendezvous = Rendezvous::builder()
            .passphrase(passphrase)
            .app_salt(APP_SALT)
            .secret_key(secret_key)
            .build()
            .await
            .context("starting iroh-gossip rendezvous")?;

        let my_id = rendezvous.node_id();
        tracing::info!("iroh endpoint bound, id: {}", my_id.fmt_short());
        tracing::info!("topic: {}", hex::encode(&rendezvous.topic_id().as_bytes()[..8]));

        let (event_tx, event_rx) = mpsc::channel::<GossipEvent>(256);
        let (frame_tx, frame_rx) = mpsc::channel::<GossipFrame>(256);

        // Receiver task: drain the rendezvous event stream, translate into
        // router-side `GossipEvent`s.
        spawn_receiver(rendezvous.subscribe(), event_tx, cancel.clone());

        // Broadcaster task: pull encoded frames from the router, hand them
        // to the iroh-gossip sender.
        spawn_broadcaster(rendezvous.sender().clone(), frame_rx, cancel.clone());

        Ok((Self { rendezvous, cancel }, event_rx, frame_tx))
    }

    /// Snapshot of rendezvous state for the TUI.
    #[must_use]
    pub fn state(&self) -> RendezvousState {
        self.rendezvous.state()
    }

    /// Graceful shutdown. Cancels our own glue tasks, then the rendezvous.
    /// Idempotent.
    pub async fn shutdown(&self) {
        self.cancel.cancel();
        self.rendezvous.shutdown().await;
    }
}

// ── Internal tasks ──────────────────────────────────────────────────────

fn spawn_receiver(
    mut rx: tokio::sync::broadcast::Receiver<Event>,
    event_tx: mpsc::Sender<GossipEvent>,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
        // Track our own neighbor set so we can send accurate counts to the
        // router (the rendezvous crate tracks the same set internally for
        // its `state()`; we duplicate here to avoid a cross-crate handle).
        let mut neighbors: HashSet<PublicKey> = HashSet::new();
        let exit_reason: &'static str = loop {
            let event = tokio::select! {
                ev = rx.recv() => ev,
                () = cancel.cancelled() => break "cancelled",
            };
            let event = match event {
                Ok(ev) => ev,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break "broadcast closed",
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    warn!("gossip receiver lagged — dropped {n} events");
                    continue;
                }
            };
            match event {
                Event::Received(msg) => {
                    if let Some(frame) = GossipFrame::decode(&msg.content) {
                        let _ = event_tx.send(GossipEvent::Frame(frame)).await;
                    } else {
                        debug!("received malformed gossip frame ({} bytes)", msg.content.len());
                    }
                }
                Event::NeighborUp(id) => {
                    neighbors.insert(id);
                    tracing::info!("neighbor up: {} (total: {})", id.fmt_short(), neighbors.len());
                    let _ = event_tx.send(GossipEvent::NeighborChanged(neighbors.len())).await;
                }
                Event::NeighborDown(id) => {
                    neighbors.remove(&id);
                    tracing::info!("neighbor down: {} (total: {})", id.fmt_short(), neighbors.len());
                    let _ = event_tx.send(GossipEvent::NeighborChanged(neighbors.len())).await;
                }
                Event::Lagged => {
                    warn!("gossip receiver lagged — some messages were dropped");
                }
            }
        };
        if exit_reason == "cancelled" {
            debug!("gossip receiver stopped ({exit_reason})");
        } else {
            error!(
                "gossip receiver STOPPED ({exit_reason}) — this bridge will no longer receive from the swarm",
            );
        }
    });
}

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

// Shared via Arc<Gossip> for the TUI.
impl Gossip {
    /// Wrap in `Arc` for sharing with the TUI task.
    #[must_use]
    pub fn into_arc(self) -> Arc<Self> {
        Arc::new(self)
    }
}
