//! TOML configuration loading and validation.
//!
//! Handles the `[radio]`, `[bridge]`, and `[tx]` sections with `MeshCore` US/Canada
//! recommended defaults, and conversion to the donglora-client `LoRaConfig`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use donglora_client::{LoRaBandwidth, LoRaCodingRate, LoRaConfig, LoRaHeaderMode, RetryPolicy};
use serde::{Deserialize, Serialize};

/// Top-level bridge configuration.
#[derive(Debug, Deserialize, Serialize)]
pub struct Config {
    #[serde(default)]
    pub radio: RadioSection,
    #[serde(default)]
    pub bridge: BridgeSection,
    #[serde(default)]
    pub tx: TxSection,
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
    pub tx_power_dbm: i8,
    #[serde(default = "default_preamble")]
    pub preamble: u16,
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

/// Per-TX retry policy. Applied to every `tx_with_retry` call. Defaults quote
/// spec `PROTOCOL.md §C.5.5`: 3 attempts, randomized 20–100 ms backoff on the
/// first retry, doubling up to a 500 ms cap.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TxSection {
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u8,
    #[serde(default = "default_backoff_min_ms")]
    pub backoff_min_ms: u32,
    #[serde(default = "default_backoff_max_ms")]
    pub backoff_max_ms: u32,
    #[serde(default = "default_backoff_cap_ms")]
    pub backoff_cap_ms: u32,
    #[serde(default = "default_backoff_multiplier")]
    pub backoff_multiplier: f32,
    #[serde(default = "default_per_attempt_timeout_secs")]
    pub per_attempt_timeout_secs: u64,
}

// ── Defaults (MeshCore US/Canada Recommended) ────────────────────

const fn default_frequency() -> u32 {
    910_525_000
}
fn default_bandwidth() -> String {
    "62.5kHz".into()
}
const fn default_sf() -> u8 {
    7
}
const fn default_cr() -> u8 {
    5
}
/// `MeshCore` private sync word encoded for the `SX126x` (see client-py
/// `examples/_common.py` — `RADIOLIB_SX126X_SYNC_WORD_PRIVATE`).
const fn default_sync_word() -> u16 {
    0x1424
}
const fn default_tx_power() -> i8 {
    20
}
const fn default_preamble() -> u16 {
    16
}
const fn default_dedup_window() -> u64 {
    300
}
const fn default_tx_queue_size() -> usize {
    32
}
const fn default_max_attempts() -> u8 {
    3
}
const fn default_backoff_min_ms() -> u32 {
    20
}
const fn default_backoff_max_ms() -> u32 {
    100
}
const fn default_backoff_cap_ms() -> u32 {
    500
}
const fn default_backoff_multiplier() -> f32 {
    2.0
}
const fn default_per_attempt_timeout_secs() -> u64 {
    5
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
            tx_power_dbm: default_tx_power(),
            preamble: default_preamble(),
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

impl Default for TxSection {
    fn default() -> Self {
        Self {
            max_attempts: default_max_attempts(),
            backoff_min_ms: default_backoff_min_ms(),
            backoff_max_ms: default_backoff_max_ms(),
            backoff_cap_ms: default_backoff_cap_ms(),
            backoff_multiplier: default_backoff_multiplier(),
            per_attempt_timeout_secs: default_per_attempt_timeout_secs(),
        }
    }
}

// ── Conversion ────────────────────────────────────────────────────

fn parse_bandwidth(s: &str) -> Result<LoRaBandwidth> {
    match s.to_lowercase().as_str() {
        "7.8khz" | "7khz" | "khz7" => Ok(LoRaBandwidth::Khz7),
        "10.4khz" | "10khz" | "khz10" => Ok(LoRaBandwidth::Khz10),
        "15.6khz" | "15khz" | "khz15" => Ok(LoRaBandwidth::Khz15),
        "20.8khz" | "20khz" | "khz20" => Ok(LoRaBandwidth::Khz20),
        "31.25khz" | "31khz" | "khz31" => Ok(LoRaBandwidth::Khz31),
        "41.7khz" | "41khz" | "khz41" => Ok(LoRaBandwidth::Khz41),
        "62.5khz" | "62khz" | "khz62" => Ok(LoRaBandwidth::Khz62),
        "125khz" | "khz125" => Ok(LoRaBandwidth::Khz125),
        "250khz" | "khz250" => Ok(LoRaBandwidth::Khz250),
        "500khz" | "khz500" => Ok(LoRaBandwidth::Khz500),
        _ => bail!("unknown bandwidth: {s}"),
    }
}

fn parse_coding_rate(v: u8) -> Result<LoRaCodingRate> {
    match v {
        5 => Ok(LoRaCodingRate::Cr4_5),
        6 => Ok(LoRaCodingRate::Cr4_6),
        7 => Ok(LoRaCodingRate::Cr4_7),
        8 => Ok(LoRaCodingRate::Cr4_8),
        _ => bail!("coding_rate must be 5-8 (meaning 4/5 to 4/8), got {v}"),
    }
}

impl RadioSection {
    /// Convert to the donglora-client `LoRaConfig`.
    ///
    /// Validates that all radio parameters are within hardware-supported ranges
    /// before constructing the config.
    pub fn to_lora_config(&self) -> Result<LoRaConfig> {
        if !(7..=12).contains(&self.spreading_factor) {
            bail!("spreading_factor must be 7-12, got {}", self.spreading_factor);
        }
        if self.frequency < 150_000_000 || self.frequency > 1_020_000_000 {
            bail!("frequency {:.3} MHz out of range (150 MHz to 1.02 GHz)", f64::from(self.frequency) / 1_000_000.0);
        }
        Ok(LoRaConfig {
            freq_hz: self.frequency,
            bw: parse_bandwidth(&self.bandwidth)?,
            sf: self.spreading_factor,
            cr: parse_coding_rate(self.coding_rate)?,
            sync_word: self.sync_word,
            tx_power_dbm: self.tx_power_dbm,
            preamble_len: self.preamble,
            header_mode: LoRaHeaderMode::Explicit,
            payload_crc: true,
            iq_invert: false,
        })
    }
}

impl TxSection {
    /// Materialize the TOML fields into a [`RetryPolicy`].
    pub fn to_retry_policy(&self) -> Result<RetryPolicy> {
        if self.max_attempts == 0 {
            bail!("tx.max_attempts must be >= 1");
        }
        if self.backoff_min_ms > self.backoff_max_ms {
            bail!(
                "tx.backoff_min_ms ({}) must not exceed backoff_max_ms ({})",
                self.backoff_min_ms,
                self.backoff_max_ms
            );
        }
        Ok(RetryPolicy {
            max_attempts: self.max_attempts,
            backoff_ms_min: self.backoff_min_ms,
            backoff_ms_max: self.backoff_max_ms,
            backoff_multiplier: self.backoff_multiplier,
            backoff_cap_ms: self.backoff_cap_ms,
            per_attempt_timeout: Duration::from_secs(self.per_attempt_timeout_secs),
            skip_cad: false,
        })
    }
}

// ── File I/O ──────────────────────────────────────────────────────

/// Default config directory: `$XDG_CONFIG_HOME/donglora-bridge/` or `~/.config/donglora-bridge/`.
pub fn config_dir() -> Result<PathBuf> {
    let dir = dirs::config_dir().context("cannot determine config directory")?.join("donglora-bridge");
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
    let text = std::fs::read_to_string(path).with_context(|| format!("reading config: {}", path.display()))?;
    let cfg: Config = toml::from_str(&text).context("parsing config TOML")?;
    if cfg.bridge.passphrase.is_empty() || cfg.bridge.passphrase == "change-me" {
        bail!("passphrase is required — run `donglora-bridge config` to set it");
    }
    Ok(cfg)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn parse_bandwidth_canonical() {
        assert!(matches!(parse_bandwidth("7.8kHz"), Ok(LoRaBandwidth::Khz7)));
        assert!(matches!(parse_bandwidth("62.5kHz"), Ok(LoRaBandwidth::Khz62)));
        assert!(matches!(parse_bandwidth("125kHz"), Ok(LoRaBandwidth::Khz125)));
        assert!(matches!(parse_bandwidth("500kHz"), Ok(LoRaBandwidth::Khz500)));
    }

    #[test]
    fn parse_bandwidth_aliases() {
        assert!(matches!(parse_bandwidth("7khz"), Ok(LoRaBandwidth::Khz7)));
        assert!(matches!(parse_bandwidth("khz125"), Ok(LoRaBandwidth::Khz125)));
    }

    #[test]
    fn parse_bandwidth_case_insensitive() {
        assert!(matches!(parse_bandwidth("125KHZ"), Ok(LoRaBandwidth::Khz125)));
    }

    #[test]
    fn parse_bandwidth_rejects_unknown() {
        assert!(parse_bandwidth("999kHz").is_err());
        assert!(parse_bandwidth("").is_err());
    }

    #[test]
    fn parse_coding_rate_canonical() {
        assert!(matches!(parse_coding_rate(5), Ok(LoRaCodingRate::Cr4_5)));
        assert!(matches!(parse_coding_rate(8), Ok(LoRaCodingRate::Cr4_8)));
    }

    #[test]
    fn parse_coding_rate_rejects_out_of_range() {
        assert!(parse_coding_rate(4).is_err());
        assert!(parse_coding_rate(9).is_err());
    }

    #[test]
    fn radio_section_defaults_to_lora_config() {
        let section = RadioSection::default();
        let cfg = section.to_lora_config().unwrap();
        assert_eq!(cfg.freq_hz, 910_525_000);
        assert_eq!(cfg.sf, 7);
        assert!(matches!(cfg.cr, LoRaCodingRate::Cr4_5));
        assert_eq!(cfg.sync_word, 0x1424);
        assert_eq!(cfg.preamble_len, 16);
        assert_eq!(cfg.tx_power_dbm, 20);
    }

    #[test]
    fn to_lora_config_rejects_invalid_sf() {
        let section = RadioSection { spreading_factor: 6, ..RadioSection::default() };
        assert!(section.to_lora_config().is_err());
        let section = RadioSection { spreading_factor: 13, ..RadioSection::default() };
        assert!(section.to_lora_config().is_err());
    }

    #[test]
    fn to_lora_config_rejects_invalid_cr() {
        let section = RadioSection { coding_rate: 4, ..RadioSection::default() };
        assert!(section.to_lora_config().is_err());
        let section = RadioSection { coding_rate: 9, ..RadioSection::default() };
        assert!(section.to_lora_config().is_err());
    }

    #[test]
    fn to_lora_config_rejects_invalid_frequency() {
        let section = RadioSection { frequency: 0, ..RadioSection::default() };
        assert!(section.to_lora_config().is_err());
        let section = RadioSection { frequency: 2_000_000_000, ..RadioSection::default() };
        assert!(section.to_lora_config().is_err());
    }

    #[test]
    fn tx_section_defaults_produce_valid_policy() {
        let p = TxSection::default().to_retry_policy().unwrap();
        assert_eq!(p.max_attempts, 3);
        assert_eq!(p.backoff_ms_min, 20);
        assert_eq!(p.backoff_ms_max, 100);
        assert_eq!(p.backoff_cap_ms, 500);
        assert!((p.backoff_multiplier - 2.0).abs() < f32::EPSILON);
    }

    #[test]
    fn tx_section_rejects_zero_attempts() {
        let s = TxSection { max_attempts: 0, ..TxSection::default() };
        assert!(s.to_retry_policy().is_err());
    }

    #[test]
    fn tx_section_rejects_inverted_backoff_bounds() {
        let s = TxSection { backoff_min_ms: 500, backoff_max_ms: 100, ..TxSection::default() };
        assert!(s.to_retry_policy().is_err());
    }

    #[test]
    fn load_config_valid_minimal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[bridge]\npassphrase = \"my-secret\"\n").unwrap();
        let cfg = load_config(&path).unwrap();
        assert_eq!(cfg.bridge.passphrase, "my-secret");
        assert_eq!(cfg.radio.frequency, 910_525_000);
        assert_eq!(cfg.tx.max_attempts, 3);
    }

    #[test]
    fn load_config_empty_passphrase_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[bridge]\npassphrase = \"\"\n").unwrap();
        assert!(load_config(&path).is_err());
    }

    #[test]
    fn load_config_change_me_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[bridge]\npassphrase = \"change-me\"\n").unwrap();
        assert!(load_config(&path).is_err());
    }

    #[test]
    fn load_config_missing_file() {
        assert!(load_config(std::path::Path::new("/nonexistent/config.toml")).is_err());
    }

    #[test]
    fn load_config_invalid_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "not valid toml {{{{").unwrap();
        assert!(load_config(&path).is_err());
    }
}
