//! TOML configuration loading and validation.
//!
//! Handles the `[radio]` and `[bridge]` sections with `MeshCore` US/Canada recommended
//! defaults, and conversion to donglora-client `RadioConfig`.

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
const fn default_sync_word() -> u16 {
    0x3444
}
fn default_tx_power() -> String {
    "22".into()
}
const fn default_preamble() -> u16 {
    16
}
const fn default_true() -> bool {
    true
}
const fn default_dedup_window() -> u64 {
    300
}
const fn default_tx_queue_size() -> usize {
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
    /// Convert to the donglora-client `RadioConfig`.
    ///
    /// Validates that all radio parameters are within hardware-supported ranges
    /// before constructing the config.
    pub fn to_radio_config(&self) -> Result<RadioConfig> {
        if !(7..=12).contains(&self.spreading_factor) {
            bail!("spreading_factor must be 7-12, got {}", self.spreading_factor);
        }
        if !(5..=8).contains(&self.coding_rate) {
            bail!("coding_rate must be 5-8 (meaning 4/5 to 4/8), got {}", self.coding_rate);
        }
        if self.frequency < 150_000_000 || self.frequency > 1_020_000_000 {
            bail!("frequency {:.3} MHz out of range (150 MHz to 1.02 GHz)", f64::from(self.frequency) / 1_000_000.0);
        }
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
        assert!(matches!(parse_bandwidth("7.8kHz"), Ok(Bandwidth::Khz7)));
        assert!(matches!(parse_bandwidth("62.5kHz"), Ok(Bandwidth::Khz62)));
        assert!(matches!(parse_bandwidth("125kHz"), Ok(Bandwidth::Khz125)));
        assert!(matches!(parse_bandwidth("500kHz"), Ok(Bandwidth::Khz500)));
    }

    #[test]
    fn parse_bandwidth_aliases() {
        assert!(matches!(parse_bandwidth("7khz"), Ok(Bandwidth::Khz7)));
        assert!(matches!(parse_bandwidth("khz125"), Ok(Bandwidth::Khz125)));
        assert!(matches!(parse_bandwidth("10khz"), Ok(Bandwidth::Khz10)));
    }

    #[test]
    fn parse_bandwidth_case_insensitive() {
        assert!(matches!(parse_bandwidth("125KHZ"), Ok(Bandwidth::Khz125)));
        assert!(matches!(parse_bandwidth("62.5KHZ"), Ok(Bandwidth::Khz62)));
    }

    #[test]
    fn parse_bandwidth_rejects_unknown() {
        assert!(parse_bandwidth("999kHz").is_err());
        assert!(parse_bandwidth("").is_err());
        assert!(parse_bandwidth("not_a_bandwidth").is_err());
    }

    #[test]
    fn parse_tx_power_numeric() {
        assert_eq!(parse_tx_power("22").unwrap(), 22);
        assert_eq!(parse_tx_power("-4").unwrap(), -4);
        assert_eq!(parse_tx_power("0").unwrap(), 0);
    }

    #[test]
    fn parse_tx_power_max_keyword() {
        assert_eq!(parse_tx_power("max").unwrap(), TX_POWER_MAX);
        assert_eq!(parse_tx_power("MAX").unwrap(), TX_POWER_MAX);
        assert_eq!(parse_tx_power("Max").unwrap(), TX_POWER_MAX);
    }

    #[test]
    fn parse_tx_power_rejects_garbage() {
        assert!(parse_tx_power("abc").is_err());
        assert!(parse_tx_power("").is_err());
    }

    #[test]
    fn radio_section_defaults_to_radio_config() {
        let section = RadioSection::default();
        let config = section.to_radio_config().unwrap();
        assert_eq!(config.freq_hz, 910_525_000);
        assert_eq!(config.sf, 7);
        assert_eq!(config.cr, 5);
        assert_eq!(config.sync_word, 0x3444);
        assert_eq!(config.preamble_len, 16);
    }

    #[test]
    fn radio_section_max_power() {
        let section = RadioSection { tx_power: "max".into(), ..RadioSection::default() };
        let config = section.to_radio_config().unwrap();
        assert_eq!(config.tx_power_dbm, TX_POWER_MAX);
    }

    #[test]
    fn to_radio_config_rejects_invalid_sf() {
        let section = RadioSection { spreading_factor: 6, ..RadioSection::default() };
        assert!(section.to_radio_config().is_err());
        let section = RadioSection { spreading_factor: 13, ..RadioSection::default() };
        assert!(section.to_radio_config().is_err());
    }

    #[test]
    fn to_radio_config_rejects_invalid_cr() {
        let section = RadioSection { coding_rate: 4, ..RadioSection::default() };
        assert!(section.to_radio_config().is_err());
        let section = RadioSection { coding_rate: 9, ..RadioSection::default() };
        assert!(section.to_radio_config().is_err());
    }

    #[test]
    fn to_radio_config_rejects_invalid_frequency() {
        let section = RadioSection { frequency: 0, ..RadioSection::default() };
        assert!(section.to_radio_config().is_err());
        let section = RadioSection { frequency: 2_000_000_000, ..RadioSection::default() };
        assert!(section.to_radio_config().is_err());
    }

    #[test]
    fn load_config_valid_minimal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[bridge]\npassphrase = \"my-secret\"\n").unwrap();
        let cfg = load_config(&path).unwrap();
        assert_eq!(cfg.bridge.passphrase, "my-secret");
        assert_eq!(cfg.radio.frequency, 910_525_000);
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

    // ── Mutant killers: default values (items 1-5) ─────────────────

    #[test]
    fn default_radio_cad_is_true() {
        let radio = RadioSection::default();
        assert!(radio.cad);
        // Also test via serde (exercises the default_true function used by #[serde(default)])
        let radio: RadioSection = toml::from_str("").unwrap();
        assert!(radio.cad);
    }

    #[test]
    fn default_dedup_window_is_300() {
        let bridge = BridgeSection::default();
        assert_eq!(bridge.dedup_window_secs, 300);
    }

    #[test]
    fn default_tx_queue_size_is_32() {
        let bridge = BridgeSection::default();
        assert_eq!(bridge.tx_queue_size, 32);
    }

    // ── Mutant killers: missing bandwidth variants (items 6-10) ──

    #[test]
    fn parse_bandwidth_all_canonical_variants() {
        assert!(matches!(parse_bandwidth("15.6kHz"), Ok(Bandwidth::Khz15)));
        assert!(matches!(parse_bandwidth("20.8kHz"), Ok(Bandwidth::Khz20)));
        assert!(matches!(parse_bandwidth("31.25kHz"), Ok(Bandwidth::Khz31)));
        assert!(matches!(parse_bandwidth("41.7kHz"), Ok(Bandwidth::Khz41)));
        assert!(matches!(parse_bandwidth("250kHz"), Ok(Bandwidth::Khz250)));
    }

    // ── Mutant killers: frequency boundary (items 11-12) ─────────

    #[test]
    fn to_radio_config_accepts_boundary_frequencies() {
        let low = RadioSection { frequency: 150_000_000, ..RadioSection::default() };
        assert!(low.to_radio_config().is_ok());

        let high = RadioSection { frequency: 1_020_000_000, ..RadioSection::default() };
        assert!(high.to_radio_config().is_ok());
    }

    // ── Mutant killers: path helpers (items 13-15) ───────────────

    #[test]
    fn config_dir_is_non_empty() {
        let dir = config_dir().unwrap();
        assert!(dir.components().count() > 0);
    }

    #[test]
    fn config_path_ends_with_config_toml() {
        let path = config_path().unwrap();
        assert_eq!(path.file_name().unwrap(), "config.toml");
    }

    #[test]
    fn default_log_path_ends_with_bridge_log() {
        let path = default_log_path().unwrap();
        assert_eq!(path.file_name().unwrap(), "bridge.log");
    }

    #[test]
    fn load_config_full_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[radio]
frequency = 915000000
bandwidth = "125kHz"
spreading_factor = 12
coding_rate = 8
sync_word = 0x1234
tx_power = "max"
preamble = 8
cad = false

[bridge]
passphrase = "test-phrase"
dedup_window_secs = 600
tx_queue_size = 64
"#,
        )
        .unwrap();
        let cfg = load_config(&path).unwrap();
        assert_eq!(cfg.radio.frequency, 915_000_000);
        assert_eq!(cfg.radio.spreading_factor, 12);
        assert_eq!(cfg.bridge.dedup_window_secs, 600);
        assert_eq!(cfg.bridge.tx_queue_size, 64);
    }
}
