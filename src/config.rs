use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use donglora_client::{Bandwidth, RadioConfig, TX_POWER_MAX};
use iroh::{PublicKey, SecretKey};
use serde::{Deserialize, Serialize};

/// Top-level bridge configuration.
#[derive(Debug, Deserialize, Serialize)]
pub struct Config {
    #[serde(default)]
    pub radio: RadioSection,
    #[serde(default)]
    pub bridge: BridgeSection,
    #[serde(default)]
    pub peers: Vec<PeerEntry>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct RadioSection {
    /// Serial port path (auto-detect if omitted).
    pub port: Option<String>,
    #[serde(default = "default_frequency")]
    pub frequency: u32,
    #[serde(default = "default_bandwidth")]
    pub bandwidth: String,
    #[serde(default = "default_sf")]
    pub spreading_factor: u8,
    #[serde(default = "default_cr")]
    pub coding_rate: u8,
    #[serde(default = "default_sync_word")]
    pub sync_word: u16,
    #[serde(default = "default_tx_power")]
    pub tx_power: String,
    #[serde(default)]
    pub preamble: u16,
    #[serde(default = "default_true")]
    pub cad: bool,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct BridgeSection {
    #[serde(default = "default_dedup_window")]
    pub dedup_window_secs: u64,
    #[serde(default = "default_tx_queue_size")]
    pub tx_queue_size: usize,
    pub log_file: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PeerEntry {
    pub node_id: String,
    pub name: Option<String>,
    #[serde(default)]
    pub addresses: Vec<String>,
}

// ── Defaults ──────────────────────────────────────────────────────

fn default_frequency() -> u32 {
    915_000_000
}
fn default_bandwidth() -> String {
    "125kHz".into()
}
fn default_sf() -> u8 {
    7
}
fn default_cr() -> u8 {
    5
}
fn default_sync_word() -> u16 {
    0x1424
}
fn default_tx_power() -> String {
    "max".into()
}
fn default_true() -> bool {
    true
}
fn default_dedup_window() -> u64 {
    30
}
fn default_tx_queue_size() -> usize {
    32
}

impl Default for RadioSection {
    fn default() -> Self {
        Self {
            port: None,
            frequency: default_frequency(),
            bandwidth: default_bandwidth(),
            spreading_factor: default_sf(),
            coding_rate: default_cr(),
            sync_word: default_sync_word(),
            tx_power: default_tx_power(),
            preamble: 0,
            cad: true,
        }
    }
}

impl Default for BridgeSection {
    fn default() -> Self {
        Self {
            dedup_window_secs: default_dedup_window(),
            tx_queue_size: default_tx_queue_size(),
            log_file: None,
        }
    }
}

// ── Conversion ────────────────────────────────────────────────────

fn parse_bandwidth(s: &str) -> Result<Bandwidth> {
    match s.to_lowercase().as_str() {
        "7.8khz" | "7khz" | "khz7" => Ok(Bandwidth::Khz7),
        "10.4khz" | "10khz" | "khz10" => Ok(Bandwidth::Khz10),
        "15.6khz" | "15khz" | "khz15" => Ok(Bandwidth::Khz15),
        "20.8khz" | "20khz" | "khz20" => Ok(Bandwidth::Khz20),
        "31.25khz" | "31khz" | "khz31" => Ok(Bandwidth::Khz31),
        "41.7khz" | "41khz" | "khz41" => Ok(Bandwidth::Khz41),
        "62.5khz" | "62khz" | "khz62" => Ok(Bandwidth::Khz62),
        "125khz" | "khz125" => Ok(Bandwidth::Khz125),
        "250khz" | "khz250" => Ok(Bandwidth::Khz250),
        "500khz" | "khz500" => Ok(Bandwidth::Khz500),
        _ => bail!("unknown bandwidth: {s}"),
    }
}

fn parse_tx_power(s: &str) -> Result<i8> {
    if s.eq_ignore_ascii_case("max") {
        Ok(TX_POWER_MAX)
    } else {
        s.parse::<i8>().context("invalid tx_power (use 'max' or a dBm integer)")
    }
}

impl RadioSection {
    /// Convert to the donglora-client RadioConfig.
    pub fn to_radio_config(&self) -> Result<RadioConfig> {
        Ok(RadioConfig {
            freq_hz: self.frequency,
            bw: parse_bandwidth(&self.bandwidth)?,
            sf: self.spreading_factor,
            cr: self.coding_rate,
            sync_word: self.sync_word,
            tx_power_dbm: parse_tx_power(&self.tx_power)?,
            preamble_len: self.preamble,
            cad: u8::from(self.cad),
        })
    }
}

// ── Peer parsing ──────────────────────────────────────────────────

impl PeerEntry {
    /// Parse the node_id string into an iroh PublicKey.
    pub fn public_key(&self) -> Result<PublicKey> {
        self.node_id
            .parse::<PublicKey>()
            .map_err(|e| anyhow::anyhow!("invalid node_id '{}': {e}", self.node_id))
    }

    /// Display name: the configured name, or a truncated NodeId.
    pub fn display_name(&self) -> String {
        if let Some(name) = &self.name {
            name.clone()
        } else {
            let id = &self.node_id;
            if id.len() > 12 {
                format!("{}...", &id[..12])
            } else {
                id.clone()
            }
        }
    }
}

// ── File I/O ──────────────────────────────────────────────────────

/// Default config directory: `$XDG_CONFIG_HOME/donglora-bridge/` or `~/.config/donglora-bridge/`.
pub fn config_dir() -> Result<PathBuf> {
    let dir = dirs::config_dir()
        .context("cannot determine config directory")?
        .join("donglora-bridge");
    Ok(dir)
}

/// Default config file path.
pub fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.toml"))
}

/// Default secret key file path (alongside default config).
#[allow(dead_code)]
pub fn secret_key_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.key"))
}

/// Default log file path: `$XDG_STATE_HOME/donglora-bridge/bridge.log`.
pub fn default_log_path() -> Result<PathBuf> {
    let dir = dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .context("cannot determine state directory")?
        .join("donglora-bridge");
    Ok(dir.join("bridge.log"))
}

/// Load config from a TOML file.
pub fn load_config(path: &Path) -> Result<Config> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading config: {}", path.display()))?;
    toml::from_str(&text).context("parsing config TOML")
}

/// Load or generate the persistent secret key.
pub fn load_or_create_secret_key(path: &Path) -> Result<SecretKey> {
    if path.exists() {
        let hex = std::fs::read_to_string(path)
            .with_context(|| format!("reading secret key: {}", path.display()))?;
        let hex = hex.trim();
        let bytes = hex::decode(hex).context("decoding secret key hex")?;
        let arr: [u8; 32] = bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("secret key must be 32 bytes"))?;
        Ok(SecretKey::from_bytes(&arr))
    } else {
        let key = SecretKey::generate(&mut rand::rng());
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating directory: {}", parent.display()))?;
        }
        let hex = hex::encode(key.to_bytes());
        std::fs::write(path, &hex)
            .with_context(|| format!("writing secret key: {}", path.display()))?;
        Ok(key)
    }
}

/// Generate a default config file with the given NodeId embedded as a comment.
pub fn generate_default_config(node_id: &str) -> String {
    format!(
        r#"# donglora-bridge configuration
# Auto-generated — add peers to get started!
# Your NodeId: {node_id}

[radio]
# port = "/dev/ttyACM0"        # auto-detect if omitted
frequency = 915_000_000
bandwidth = "125kHz"
spreading_factor = 7
coding_rate = 5
sync_word = 0x1424
tx_power = "max"
preamble = 0                    # 0 = firmware default (16 symbols)
cad = true

[bridge]
dedup_window_secs = 30
tx_queue_size = 32
# log_file = "/tmp/donglora-bridge.log"

# Add peers below. Each peer needs the remote bridge's NodeId.
# Share your NodeId (above) with peers so they can add you too.
#
# [[peers]]
# node_id = "paste-node-id-here"
# name = "My Friend's Bridge"
# addresses = ["1.2.3.4:1234"]  # optional direct address hints
"#
    )
}

/// Write the default config to disk (first-run flow).
pub fn write_default_config(path: &Path, node_id: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating directory: {}", parent.display()))?;
    }
    let content = generate_default_config(node_id);
    std::fs::write(path, content)
        .with_context(|| format!("writing config: {}", path.display()))?;
    Ok(())
}
