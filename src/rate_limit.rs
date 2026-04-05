use std::time::{Duration, Instant};

use donglora_client::Bandwidth;

/// Token-bucket rate limiter for radio transmissions.
///
/// Calculates a safe transmit rate from the LoRa radio configuration (spreading factor,
/// bandwidth, coding rate) and enforces it with a token bucket that allows short bursts.
pub struct RateLimiter {
    tokens: f64,
    max_tokens: f64,
    refill_rate: f64, // tokens per second
    last_refill: Instant,
}

/// Default burst size (max tokens).
const DEFAULT_BURST: f64 = 3.0;

/// Reference payload size (bytes) used to estimate air time for rate calculation.
const REFERENCE_PAYLOAD_BYTES: usize = 50;

/// Target duty cycle fraction (0.5 = 50%).
const TARGET_DUTY_CYCLE: f64 = 0.5;

impl RateLimiter {
    /// Create a rate limiter derived from radio config.
    ///
    /// If `override_pps` is `Some`, use that rate directly instead of calculating.
    pub fn from_radio_config(
        sf: u8,
        bw: Bandwidth,
        cr: u8,
        preamble: u16,
        override_pps: Option<f64>,
    ) -> Self {
        let refill_rate = if let Some(pps) = override_pps {
            pps.max(0.01)
        } else {
            let air_time = lora_air_time(REFERENCE_PAYLOAD_BYTES, sf, bw, cr, preamble);
            let secs = air_time.as_secs_f64();
            if secs > 0.0 {
                TARGET_DUTY_CYCLE / secs
            } else {
                10.0 // fallback
            }
        };
        Self {
            tokens: DEFAULT_BURST,
            max_tokens: DEFAULT_BURST,
            refill_rate,
            last_refill: Instant::now(),
        }
    }

    /// Try to consume one token. Returns `true` if the transmission is allowed.
    pub fn try_acquire(&mut self) -> bool {
        self.refill();
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Current rate in packets per second.
    pub fn rate_pps(&self) -> f64 {
        self.refill_rate
    }

    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.refill_rate).min(self.max_tokens);
        self.last_refill = now;
    }
}

/// Estimate LoRa packet air time using the Semtech SX1276 formula.
///
/// Parameters:
/// - `payload_bytes`: payload size in bytes
/// - `sf`: spreading factor (7–12)
/// - `bw`: bandwidth
/// - `cr`: coding rate denominator (5–8, meaning 4/5 to 4/8)
/// - `preamble`: preamble length in symbols (0 = firmware default of 16)
pub fn lora_air_time(payload_bytes: usize, sf: u8, bw: Bandwidth, cr: u8, preamble: u16) -> Duration {
    let sf_f = f64::from(sf);
    let bw_hz = bandwidth_hz(bw);
    let preamble_symbols = if preamble == 0 { 16.0 } else { f64::from(preamble) };

    // Symbol time in seconds.
    let t_sym = 2.0_f64.powf(sf_f) / bw_hz;

    // Preamble time.
    let t_preamble = (preamble_symbols + 4.25) * t_sym;

    // Low data rate optimize: enabled when SF >= 11 and BW <= 125 kHz.
    let de: f64 = if sf >= 11 && bw_hz <= 125_000.0 {
        1.0
    } else {
        0.0
    };

    // Payload symbol count (Semtech formula with CRC enabled, explicit header).
    let numerator = 8.0 * payload_bytes as f64 - 4.0 * sf_f + 28.0 + 16.0;
    let denominator = 4.0 * (sf_f - 2.0 * de);
    let n_payload = 8.0 + (numerator / denominator).ceil().max(0.0) * f64::from(cr);

    let t_payload = n_payload * t_sym;
    let total = t_preamble + t_payload;

    Duration::from_secs_f64(total)
}

fn bandwidth_hz(bw: Bandwidth) -> f64 {
    match bw {
        Bandwidth::Khz7 => 7_800.0,
        Bandwidth::Khz10 => 10_400.0,
        Bandwidth::Khz15 => 15_600.0,
        Bandwidth::Khz20 => 20_800.0,
        Bandwidth::Khz31 => 31_250.0,
        Bandwidth::Khz41 => 41_700.0,
        Bandwidth::Khz62 => 62_500.0,
        Bandwidth::Khz125 => 125_000.0,
        Bandwidth::Khz250 => 250_000.0,
        Bandwidth::Khz500 => 500_000.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn air_time_sf7_bw125() {
        let t = lora_air_time(50, 7, Bandwidth::Khz125, 5, 16);
        // SF7/125kHz/CR4/5/16-preamble/50 bytes: ~100ms range
        assert!(t.as_millis() > 50, "air time too short: {t:?}");
        assert!(t.as_millis() < 300, "air time too long: {t:?}");
    }

    #[test]
    fn air_time_sf12_bw125() {
        let t = lora_air_time(50, 12, Bandwidth::Khz125, 5, 16);
        // SF12/125kHz: should be multiple seconds
        assert!(t.as_secs() >= 1, "air time too short: {t:?}");
        assert!(t.as_secs() < 10, "air time too long: {t:?}");
    }

    #[test]
    fn rate_limiter_allows_burst() {
        let mut rl = RateLimiter::from_radio_config(7, Bandwidth::Khz125, 5, 16, None);
        // Should allow burst of 3
        assert!(rl.try_acquire());
        assert!(rl.try_acquire());
        assert!(rl.try_acquire());
        // 4th should fail (no time for refill)
        assert!(!rl.try_acquire());
    }

    #[test]
    fn rate_limiter_override() {
        let rl = RateLimiter::from_radio_config(7, Bandwidth::Khz125, 5, 16, Some(42.0));
        assert!((rl.rate_pps() - 42.0).abs() < f64::EPSILON);
    }

    #[test]
    fn air_time_zero_preamble_uses_default() {
        let t_default = lora_air_time(50, 7, Bandwidth::Khz125, 5, 0);
        let t_explicit = lora_air_time(50, 7, Bandwidth::Khz125, 5, 16);
        assert_eq!(t_default, t_explicit);
    }
}
