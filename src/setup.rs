//! Interactive console setup wizard for bridge configuration.

use std::io::{self, Write};
use std::path::Path;

use anyhow::{Context, Result};

use crate::config::{BridgeSection, Config, RadioSection};

/// ANSI escape helpers.
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const CYAN: &str = "\x1b[36m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RESET: &str = "\x1b[0m";

/// Run the interactive setup wizard. If `existing` is Some, use those values as defaults.
pub fn run_wizard(config_path: &Path, existing: Option<&Config>) -> Result<()> {
    let defaults = existing
        .map_or_else(|| (RadioSection::default(), BridgeSection::default()), |c| (c.radio.clone(), c.bridge.clone()));
    let (radio, bridge) = defaults;

    println!();
    println!("  {BOLD}donglora-bridge setup{RESET}");
    println!("  {DIM}Configure your LoRa gossip bridge.{RESET}");
    println!();

    // ── Passphrase ───────────────────────────────────────────────

    let default_pass = if bridge.passphrase.is_empty() || bridge.passphrase == "change-me" {
        None
    } else {
        Some(bridge.passphrase.as_str())
    };
    let passphrase = prompt_required("Passphrase", default_pass)?;

    println!();
    println!("  {BOLD}Radio Settings{RESET} {DIM}(Enter to keep default){RESET}");
    println!("  {DIM}────────────────────────────────────{RESET}");

    // ── Radio ────────────────────────────────────────────────────

    let freq_default = format!("{:.3}", f64::from(radio.frequency) / 1_000_000.0);
    let freq_mhz: f64 = prompt_parse("Frequency (MHz)", &freq_default)?;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // validated by prompt_parse
    let frequency = (freq_mhz * 1_000_000.0) as u32;

    let bandwidth = prompt_default("Bandwidth", &radio.bandwidth)?;

    let sf_default = format!("{}", radio.spreading_factor);
    let spreading_factor: u8 = prompt_parse("Spreading Factor", &sf_default)?;

    let cr_default = format!("{}", radio.coding_rate);
    let coding_rate: u8 = prompt_parse("Coding Rate", &cr_default)?;

    let sw_default = format!("0x{:04X}", radio.sync_word);
    let sync_word_str = prompt_default("Sync Word", &sw_default)?;
    let sync_word = parse_sync_word(&sync_word_str)?;

    let tx_power = prompt_default("TX Power (dBm or 'max')", &radio.tx_power)?;

    let pre_default = format!("{}", radio.preamble);
    let preamble: u16 = prompt_parse("Preamble", &pre_default)?;

    let cad_default = if radio.cad { "on" } else { "off" };
    let cad_str = prompt_default("CAD", cad_default)?;
    let cad = matches!(cad_str.to_lowercase().as_str(), "on" | "true" | "yes" | "1");

    let port_default = radio.port.as_deref().unwrap_or("auto");
    let port_str = prompt_default("Serial Port", port_default)?;
    let port = if port_str == "auto" || port_str.is_empty() { None } else { Some(port_str) };

    // ── Build and save ───────────────────────────────────────────

    let cfg = Config {
        radio: RadioSection {
            port,
            frequency,
            bandwidth,
            spreading_factor,
            coding_rate,
            sync_word,
            tx_power,
            preamble,
            cad,
        },
        bridge: BridgeSection {
            passphrase,
            dedup_window_secs: bridge.dedup_window_secs,
            tx_queue_size: bridge.tx_queue_size,
            rate_limit_pps: bridge.rate_limit_pps,
            log_file: bridge.log_file,
        },
    };

    write_config(config_path, &cfg)?;

    println!();
    println!("  {GREEN}Config saved to {}{RESET}", config_path.display());
    println!("  Run {CYAN}donglora-bridge{RESET} to start!");
    println!();

    Ok(())
}

fn write_config(path: &Path, cfg: &Config) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("creating directory: {}", parent.display()))?;
    }
    let content = toml::to_string_pretty(cfg).context("serializing config")?;
    std::fs::write(path, content).with_context(|| format!("writing config: {}", path.display()))?;
    Ok(())
}

// ── Prompt helpers ───────────────────────────────────────────────

fn prompt_default(label: &str, default: &str) -> Result<String> {
    print!("  {label} {DIM}[{default}]{RESET}: ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let trimmed = input.trim();
    if trimmed.is_empty() { Ok(default.to_string()) } else { Ok(trimmed.to_string()) }
}

fn prompt_required(label: &str, default: Option<&str>) -> Result<String> {
    loop {
        let prompt = default.map_or_else(|| format!("  {label}: "), |d| format!("  {label} {DIM}[{d}]{RESET}: "));
        print!("{prompt}");
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let trimmed = input.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
        if let Some(d) = default {
            return Ok(d.to_string());
        }
        println!("  {YELLOW}This field is required.{RESET}");
    }
}

fn prompt_parse<T: std::str::FromStr>(label: &str, default: &str) -> Result<T>
where
    T::Err: std::fmt::Display,
{
    loop {
        let input = prompt_default(label, default)?;
        match input.parse::<T>() {
            Ok(v) => return Ok(v),
            Err(e) => println!("  {YELLOW}Invalid input: {e}{RESET}"),
        }
    }
}

fn parse_sync_word(s: &str) -> Result<u16> {
    let s = s.trim().to_lowercase();
    let s = s.strip_prefix("0x").unwrap_or(&s);
    u16::from_str_radix(s, 16).with_context(|| format!("invalid sync word: {s}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn parse_sync_word_hex_prefix() {
        assert_eq!(parse_sync_word("0x3444").unwrap(), 0x3444);
    }

    #[test]
    fn parse_sync_word_no_prefix() {
        assert_eq!(parse_sync_word("3444").unwrap(), 0x3444);
    }

    #[test]
    fn parse_sync_word_case_insensitive() {
        assert_eq!(parse_sync_word("0xABCD").unwrap(), 0xABCD);
    }

    #[test]
    fn parse_sync_word_whitespace() {
        assert_eq!(parse_sync_word("  0x3444  ").unwrap(), 0x3444);
    }

    #[test]
    fn parse_sync_word_invalid() {
        assert!(parse_sync_word("ZZZZ").is_err());
        assert!(parse_sync_word("").is_err());
    }
}
