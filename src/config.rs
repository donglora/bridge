use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use donglora_client::{Bandwidth, RadioConfig, TX_POWER_MAX};
use serde::{Deserialize, Serialize};

/// Top-level bridge configuration.
#[derive(Debug, Deserialize, Serialize)]
pub struct Config {
    #[serde(default)]
    pub radio: RadioSection,
    #[serde(default)]
    pub bridge: BridgeSection,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
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
    #[serde(default = "default_preamble")]
    pub preamble: u16,
    #[serde(default = "default_true")]
    pub cad: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BridgeSection {
    /// Shared passphrase. All bridges with the same passphrase join the same swarm.
    #[serde(default)]
    pub passphrase: String,
    #[serde(default = "default_dedup_window")]
    pub dedup_window_secs: u64,
    #[serde(default = "default_tx_queue_size")]
    pub tx_queue_size: usize,
    /// Optional manual override for the radio TX rate limit (packets per second).
    /// If omitted, a rate is calculated from the radio config.
    pub rate_limit_pps: Option<f64>,
    pub log_file: Option<PathBuf>,
}

// ── Defaults (MeshCore US/Canada Recommended) ────────────────────

fn default_frequency() -> u32 {
    910_525_000
}
fn default_bandwidth() -> String {
    "62.5kHz".into()
}
fn default_sf() -> u8 {
    7
}
fn default_cr() -> u8 {
    5
}
fn default_sync_word() -> u16 {
    0x3444
}
fn default_tx_power() -> String {
    "22".into()
}
fn default_preamble() -> u16 {
    16
}
fn default_true() -> bool {
    true
}
fn default_dedup_window() -> u64 {
    300
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
            preamble: default_preamble(),
            cad: true,
        }
    }
}

impl Default for BridgeSection {
    fn default() -> Self {
        Self {
            passphrase: String::new(),
            dedup_window_secs: default_dedup_window(),
            tx_queue_size: default_tx_queue_size(),
            rate_limit_pps: None,
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

/// Default log file path: `$XDG_STATE_HOME/donglora-bridge/bridge.log`.
pub fn default_log_path() -> Result<PathBuf> {
    let dir = dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .context("cannot determine state directory")?
        .join("donglora-bridge");
    Ok(dir.join("bridge.log"))
}

/// Load config from a TOML file. Returns None-passphrase error if not set.
pub fn load_config(path: &Path) -> Result<Config> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading config: {}", path.display()))?;
    let cfg: Config = toml::from_str(&text).context("parsing config TOML")?;
    if cfg.bridge.passphrase.is_empty() || cfg.bridge.passphrase == "change-me" {
        bail!(
            "passphrase is required — run `donglora-bridge config` to set it"
        );
    }
    Ok(cfg)
}
