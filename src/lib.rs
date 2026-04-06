//! Peer-to-peer `LoRa` bridge connecting `DongLoRa` USB dongles across the internet.
//!
//! Uses iroh-gossip for swarm-based packet broadcast and mainline DHT for
//! zero-configuration peer discovery. Nodes sharing a passphrase automatically
//! discover each other and relay radio packets through a gossip swarm.

pub mod config;
pub mod gossip;
pub mod packet;
pub mod radio;
pub mod rate_limit;
pub mod router;
pub mod setup;

// TUI is public only so its types are visible to the binary; not intended for
// external consumption.
#[doc(hidden)]
pub mod tui;
