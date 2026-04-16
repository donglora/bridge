//! `LoRa` air-time-aware token bucket rate limiter.
//!
//! Calculates transmit rate from the Semtech `LoRa` air time formula and
//! enforces it with a configurable burst allowance.

use std::time::{Duration, Instant};

use donglora_client::LoRaBandwidth;

/// Token-bucket rate limiter for radio transmissions.
///
/// Calculates a safe transmit rate from the `LoRa` radio configuration (spreading factor,
/// bandwidth, coding rate) and enforces it with a token bucket that allows short bursts.
pub struct RateLimiter {
    tokens: f64,
    max_tokens: f64,
    refill_rate: f64, // tokens per second
    last_refill: Instant,
}

/// Default burst size (max tokens). Allows short bursts of 3 back-to-back packets
/// to handle bursty mesh traffic without triggering rate limiting on every packet.
const DEFAULT_BURST: f64 = 3.0;

/// Reference payload size (bytes) used to estimate air time for rate calculation.
/// 50 bytes is a representative `MeshCore` packet size (header + small payload).
const REFERENCE_PAYLOAD_BYTES: usize = 50;

/// Target duty cycle fraction. 50% leaves headroom for other radio traffic sharing
/// the channel while still forwarding bridge traffic aggressively.
const TARGET_DUTY_CYCLE: f64 = 0.5;

impl RateLimiter {
    /// Create a rate limiter derived from radio config.
    ///
    /// If `override_pps` is `Some`, use that rate directly instead of calculating.
    #[must_use]
    pub fn from_radio_config(sf: u8, bw: LoRaBandwidth, cr: u8, preamble: u16, override_pps: Option<f64>) -> Self {
        let refill_rate = override_pps.map_or_else(
            || {
                let air_time = lora_air_time(REFERENCE_PAYLOAD_BYTES, sf, bw, cr, preamble);
                let secs = air_time.as_secs_f64();
                if secs > 0.0 { TARGET_DUTY_CYCLE / secs } else { 10.0 }
            },
            |pps| pps.max(0.01),
        );
        Self { tokens: DEFAULT_BURST, max_tokens: DEFAULT_BURST, refill_rate, last_refill: Instant::now() }
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
    #[must_use]
    pub const fn rate_pps(&self) -> f64 {
        self.refill_rate
    }

    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.tokens = elapsed.mul_add(self.refill_rate, self.tokens).min(self.max_tokens);
        self.last_refill = now;
    }
}

/// Estimate `LoRa` packet air time using the Semtech `LoRa` time-on-air formula.
///
/// Parameters:
/// - `payload_bytes`: payload size in bytes
/// - `sf`: spreading factor (7–12)
/// - `bw`: bandwidth
/// - `cr`: coding rate denominator (5–8, meaning 4/5 to 4/8)
/// - `preamble`: preamble length in symbols (0 = firmware default of 16)
#[must_use]
pub fn lora_air_time(payload_bytes: usize, sf: u8, bw: LoRaBandwidth, cr: u8, preamble: u16) -> Duration {
    let sf_f = f64::from(sf);
    let bw_hz = bandwidth_hz(bw);
    let preamble_symbols = if preamble == 0 { 16.0 } else { f64::from(preamble) };

    // Symbol time in seconds.
    let t_sym = sf_f.exp2() / bw_hz;

    // Preamble time.
    let t_preamble = (preamble_symbols + 4.25) * t_sym;

    // Low data rate optimize: enabled when SF >= 11 and BW <= 125 kHz.
    let de: f64 = if sf >= 11 && bw_hz <= 125_000.0 { 1.0 } else { 0.0 };

    // Payload symbol count (Semtech formula with CRC enabled, explicit header).
    #[allow(clippy::cast_precision_loss)] // payload_bytes is always < 256
    let payload_f = payload_bytes as f64;
    let numerator = 8.0f64.mul_add(payload_f, (-4.0f64).mul_add(sf_f, 44.0));
    let denominator = 4.0f64.mul_add(sf_f, -8.0 * de);
    let n_payload = (numerator / denominator).ceil().max(0.0).mul_add(f64::from(cr), 8.0);

    let t_payload = n_payload * t_sym;
    let total = t_preamble + t_payload;

    Duration::from_secs_f64(total)
}

const fn bandwidth_hz(bw: LoRaBandwidth) -> f64 {
    match bw {
        LoRaBandwidth::Khz7 => 7_800.0,
        LoRaBandwidth::Khz10 => 10_400.0,
        LoRaBandwidth::Khz15 => 15_600.0,
        LoRaBandwidth::Khz20 => 20_800.0,
        LoRaBandwidth::Khz31 => 31_250.0,
        LoRaBandwidth::Khz41 => 41_700.0,
        LoRaBandwidth::Khz62 => 62_500.0,
        LoRaBandwidth::Khz125 => 125_000.0,
        LoRaBandwidth::Khz250 => 250_000.0,
        LoRaBandwidth::Khz500 => 500_000.0,
        // SX128x 2.4 GHz bandwidths — not used on the bridge today, but
        // plumb through so SET_CONFIG with one of these still yields a
        // usable rate rather than panicking.
        LoRaBandwidth::Khz200 => 200_000.0,
        LoRaBandwidth::Khz400 => 400_000.0,
        LoRaBandwidth::Khz800 => 800_000.0,
        LoRaBandwidth::Khz1600 => 1_600_000.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn air_time_sf7_bw125() {
        let t = lora_air_time(50, 7, LoRaBandwidth::Khz125, 5, 16);
        // SF7/125kHz/CR4/5/16-preamble/50 bytes: ~100ms range
        assert!(t.as_millis() > 50, "air time too short: {t:?}");
        assert!(t.as_millis() < 300, "air time too long: {t:?}");
    }

    #[test]
    fn air_time_sf12_bw125() {
        let t = lora_air_time(50, 12, LoRaBandwidth::Khz125, 5, 16);
        // SF12/125kHz: should be multiple seconds
        assert!(t.as_secs() >= 1, "air time too short: {t:?}");
        assert!(t.as_secs() < 10, "air time too long: {t:?}");
    }

    #[test]
    fn rate_limiter_allows_burst() {
        let mut rl = RateLimiter::from_radio_config(7, LoRaBandwidth::Khz125, 5, 16, None);
        // Should allow burst of 3
        assert!(rl.try_acquire());
        assert!(rl.try_acquire());
        assert!(rl.try_acquire());
        // 4th should fail (no time for refill)
        assert!(!rl.try_acquire());
    }

    #[test]
    fn rate_limiter_override() {
        let rl = RateLimiter::from_radio_config(7, LoRaBandwidth::Khz125, 5, 16, Some(42.0));
        assert!((rl.rate_pps() - 42.0).abs() < f64::EPSILON);
    }

    #[test]
    fn air_time_zero_preamble_uses_default() {
        let t_default = lora_air_time(50, 7, LoRaBandwidth::Khz125, 5, 0);
        let t_explicit = lora_air_time(50, 7, LoRaBandwidth::Khz125, 5, 16);
        assert_eq!(t_default, t_explicit);
    }

    // ── Mutant-killing tests for from_radio_config (items 1-5) ────────

    #[test]
    fn from_radio_config_calculates_reasonable_rate() {
        // SF7/BW125/CR5/preamble16 with 50-byte reference payload:
        // air time ~105.7ms, so rate = 0.5 / 0.1057 ~= 4.73 pps.
        let rl = RateLimiter::from_radio_config(7, LoRaBandwidth::Khz125, 5, 16, None);
        let rate = rl.rate_pps();
        // Must be between 0.1 and 100 (rules out fallback of 10.0 indirectly,
        // and catches if secs<=0 path is taken or division is wrong).
        assert!(rate > 0.1, "rate too low: {rate}");
        assert!(rate < 100.0, "rate too high: {rate}");
        // Specifically verify it is NOT the 10.0 fallback, proving secs > 0.0 was true.
        assert!((rate - 10.0).abs() > 0.5, "rate should not be the 10.0 fallback: {rate}");
        // Tight check around the expected ~4.73 pps.
        assert!(rate > 4.0, "rate should be near 4.73: {rate}");
        assert!(rate < 5.5, "rate should be near 4.73: {rate}");
    }

    #[test]
    fn from_radio_config_sf12_rate_much_slower() {
        // SF12/BW125: air time ~2564ms, rate = 0.5 / 2.564 ~= 0.195 pps.
        let rl = RateLimiter::from_radio_config(12, LoRaBandwidth::Khz125, 5, 16, None);
        let rate = rl.rate_pps();
        assert!(rate > 0.1, "rate too low: {rate}");
        assert!(rate < 0.5, "rate too high: {rate}");
        // Definitely not 10.0 fallback.
        assert!((rate - 10.0).abs() > 1.0, "rate should not be the fallback: {rate}");
    }

    // ── Mutant-killing tests for refill (item 6) ───────────────────

    #[test]
    fn rate_limiter_refills_after_waiting() {
        // Use override_pps for predictable timing: 10 pps = 1 token per 100ms.
        let mut rl = RateLimiter::from_radio_config(7, LoRaBandwidth::Khz125, 5, 16, Some(10.0));
        // Exhaust all burst tokens.
        assert!(rl.try_acquire());
        assert!(rl.try_acquire());
        assert!(rl.try_acquire());
        assert!(!rl.try_acquire(), "burst should be exhausted");

        // Sleep enough for at least 1 token to refill (10 pps -> 100ms per token).
        std::thread::sleep(Duration::from_millis(200));

        // After sleeping, refill should have added tokens, so acquire succeeds.
        assert!(rl.try_acquire(), "should succeed after refill");
    }

    // ── Mutant-killing tests for lora_air_time (items 7-14) ────────

    #[test]
    fn air_time_exact_sf7_bw125() {
        // SF7, BW125kHz, CR4/5, preamble=16, payload=50 bytes.
        // Expected: 105.728 ms (computed from Semtech formula).
        let t = lora_air_time(50, 7, LoRaBandwidth::Khz125, 5, 16);
        let ms = t.as_secs_f64() * 1000.0;
        assert!((ms - 105.728).abs() < 0.01, "SF7/BW125 air time should be ~105.728ms, got {ms:.3}ms");
    }

    #[test]
    fn air_time_exact_sf12_bw125() {
        // SF12, BW125kHz, CR4/5, preamble=16, payload=50 bytes.
        // Expected: 2564.096 ms.
        let t = lora_air_time(50, 12, LoRaBandwidth::Khz125, 5, 16);
        let ms = t.as_secs_f64() * 1000.0;
        assert!((ms - 2564.096).abs() < 0.01, "SF12/BW125 air time should be ~2564.096ms, got {ms:.3}ms");
    }

    #[test]
    fn air_time_ldro_sf11_bw125_vs_sf10_bw125() {
        // SF>=11 && BW<=125kHz triggers LDRO. SF10 does not.
        // SF11/BW125: 1445.888 ms (de=1), SF10/BW125: 681.984 ms (de=0).
        let t11 = lora_air_time(50, 11, LoRaBandwidth::Khz125, 5, 16);
        let t10 = lora_air_time(50, 10, LoRaBandwidth::Khz125, 5, 16);

        let ms11 = t11.as_secs_f64() * 1000.0;
        let ms10 = t10.as_secs_f64() * 1000.0;

        // Verify exact values.
        assert!((ms11 - 1445.888).abs() < 0.01, "SF11/BW125 should be ~1445.888ms, got {ms11:.3}ms");
        assert!((ms10 - 681.984).abs() < 0.01, "SF10/BW125 should be ~681.984ms, got {ms10:.3}ms");

        // SF11 must be more than double SF10 due to LDRO + SF increase.
        // (Kills mutant: replace >= with < in sf >= 11 check.)
        assert!(t11 > t10 * 2, "SF11 with LDRO should be >2x SF10");
    }

    #[test]
    fn air_time_ldro_not_triggered_high_bw() {
        // SF11/BW250: LDRO should NOT be active (BW > 125kHz).
        // Expected: 641.024 ms (de=0).
        let t11_250 = lora_air_time(50, 11, LoRaBandwidth::Khz250, 5, 16);
        let ms = t11_250.as_secs_f64() * 1000.0;
        assert!((ms - 641.024).abs() < 0.01, "SF11/BW250 should be ~641.024ms (no LDRO), got {ms:.3}ms");

        // Compare with SF11/BW125 which HAS LDRO.
        let t11_125 = lora_air_time(50, 11, LoRaBandwidth::Khz125, 5, 16);
        // Even accounting for halved BW doubling the base time, the LDRO effect
        // on SF11/BW125 makes it much longer than simply 2x SF11/BW250.
        assert!(t11_125 > t11_250 * 2, "SF11/BW125 (LDRO on) should be >2x SF11/BW250 (LDRO off)");
    }

    #[test]
    fn air_time_preamble_scaling() {
        // Changing preamble by N symbols changes air time by N * t_sym.
        // t_sym for SF7/BW125 = 2^7 / 125000 = 1.024 ms.
        let t8 = lora_air_time(50, 7, LoRaBandwidth::Khz125, 5, 8);
        let t16 = lora_air_time(50, 7, LoRaBandwidth::Khz125, 5, 16);
        let t32 = lora_air_time(50, 7, LoRaBandwidth::Khz125, 5, 32);

        let diff_8_to_16 = t16.as_secs_f64() - t8.as_secs_f64();
        let diff_16_to_32 = t32.as_secs_f64() - t16.as_secs_f64();

        let t_sym = 128.0 / 125_000.0; // 1.024 ms

        // 8 -> 16: 8 symbols difference.
        let expected_8 = 8.0f64.mul_add(t_sym, -diff_8_to_16).abs();
        assert!(expected_8 < 1e-9, "preamble 8->16 should add 8*t_sym, got {diff_8_to_16:.6}s");

        // 16 -> 32: 16 symbols difference.
        let expected_16 = 16.0f64.mul_add(t_sym, -diff_16_to_32).abs();
        assert!(expected_16 < 1e-9, "preamble 16->32 should add 16*t_sym, got {diff_16_to_32:.6}s");
    }

    #[test]
    fn air_time_sensible_range_all_sf_bw() {
        // Verify reasonable bounds for all SF/BW combos with 50-byte payload.
        // This catches gross formula errors (e.g. division replaced with modulo).
        let bandwidths = [
            LoRaBandwidth::Khz7,
            LoRaBandwidth::Khz10,
            LoRaBandwidth::Khz15,
            LoRaBandwidth::Khz20,
            LoRaBandwidth::Khz31,
            LoRaBandwidth::Khz41,
            LoRaBandwidth::Khz62,
            LoRaBandwidth::Khz125,
            LoRaBandwidth::Khz250,
            LoRaBandwidth::Khz500,
        ];
        for sf in 7..=12u8 {
            for &bw in &bandwidths {
                let t = lora_air_time(50, sf, bw, 5, 16);
                let ms = t.as_secs_f64() * 1000.0;
                // Fastest: SF7/BW500 ~26ms. Slowest: SF12/BW7.8kHz ~many seconds.
                // All should be > 1ms and < 300s.
                assert!(ms > 1.0, "SF{sf}/BW{bw:?}: air time too short: {ms:.3}ms");
                assert!(ms < 300_000.0, "SF{sf}/BW{bw:?}: air time too long: {ms:.3}ms");
            }
        }
    }

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn air_time_always_positive(
                payload in 0usize..256,
                sf in 7u8..=12u8,
                cr in 5u8..=8u8,
                preamble in 0u16..=64u16,
            ) {
                for bw in [LoRaBandwidth::Khz7, LoRaBandwidth::Khz62, LoRaBandwidth::Khz125, LoRaBandwidth::Khz500] {
                    let t = lora_air_time(payload, sf, bw, cr, preamble);
                    prop_assert!(t > Duration::ZERO, "air time must be positive");
                    prop_assert!(t < Duration::from_secs(600), "air time should be < 600s");
                }
            }
        }
    }
}
