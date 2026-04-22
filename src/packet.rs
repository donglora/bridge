//! Wire format definitions for radio packets and gossip frames.
//!
//! Includes SNR quality grading against `LoRa` demodulation floors and blake3
//! content hashing for deduplication.

/// Gossip frame header size: `sender(32) + rssi(2) + snr(2) + payload_len(2) = 38`
const FRAME_HEADER_SIZE: usize = 38;

/// SNR quality grade for a received `LoRa` packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grade {
    /// SNR well above demodulation floor (margin >= 3 dB).
    Good,
    /// SNR near demodulation floor (0 <= margin < 3 dB).
    Marginal,
    /// SNR below demodulation floor — likely corrupt.
    Unreliable,
    /// SNR outside valid range (-32..32) — firmware/decode error.
    Invalid,
}

impl Grade {
    /// Returns true if the packet is reliable enough to forward.
    #[must_use]
    pub const fn should_forward(self) -> bool {
        matches!(self, Self::Good | Self::Marginal)
    }
}

impl std::fmt::Display for Grade {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Good => write!(f, "GOOD"),
            Self::Marginal => write!(f, "MARG"),
            Self::Unreliable => write!(f, "UNRL"),
            Self::Invalid => write!(f, "INVL"),
        }
    }
}

/// Grade a received packet's SNR against the demodulation floor for the given
/// spreading factor.
#[must_use]
pub fn snr_grade(snr: i16, sf: u8) -> Grade {
    if !(-32..=32).contains(&snr) {
        return Grade::Invalid;
    }
    let min_snr = -2.5 * (f32::from(sf) - 4.0);
    let margin = f32::from(snr) - min_snr;
    if margin < 0.0 {
        Grade::Unreliable
    } else if margin < 3.0 {
        Grade::Marginal
    } else {
        Grade::Good
    }
}

/// A packet received from the radio, before wrapping in a gossip frame.
#[derive(Debug, Clone)]
pub struct RadioPacket {
    pub rssi: i16,
    pub snr: i16,
    pub payload: Vec<u8>,
}

/// A gossip frame — the wire format sent through the iroh-gossip swarm.
///
/// Wire format: `[sender: 32B] [rssi: i16 LE] [snr: i16 LE] [payload_len: u16 LE] [payload: NB]`
#[derive(Debug, Clone)]
pub struct GossipFrame {
    /// `EndpointId` (`PublicKey` bytes) of the bridge that first heard this packet on radio.
    pub sender: [u8; 32],
    /// RSSI at the originating bridge (informational).
    pub rssi: i16,
    /// SNR at the originating bridge (informational).
    pub snr: i16,
    /// Raw `LoRa` payload.
    pub payload: Vec<u8>,
}

impl GossipFrame {
    /// Create a new gossip frame from a public key and radio packet data.
    #[must_use]
    pub fn new(sender: &iroh::PublicKey, rssi: i16, snr: i16, payload: Vec<u8>) -> Self {
        Self { sender: *sender.as_bytes(), rssi, snr, payload }
    }

    /// Encode this frame into bytes for gossip broadcast.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(FRAME_HEADER_SIZE + self.payload.len());
        buf.extend_from_slice(&self.sender);
        buf.extend_from_slice(&self.rssi.to_le_bytes());
        buf.extend_from_slice(&self.snr.to_le_bytes());
        #[allow(clippy::cast_possible_truncation)] // LoRa payloads never exceed 256 bytes
        let len = self.payload.len() as u16;
        buf.extend_from_slice(&len.to_le_bytes());
        buf.extend_from_slice(&self.payload);
        buf
    }

    /// Decode a gossip frame from bytes received from the swarm.
    #[must_use]
    pub fn decode(data: &[u8]) -> Option<Self> {
        if data.len() < FRAME_HEADER_SIZE {
            return None;
        }
        let sender: [u8; 32] = data[..32].try_into().ok()?;
        let rssi = i16::from_le_bytes(data[32..34].try_into().ok()?);
        let snr = i16::from_le_bytes(data[34..36].try_into().ok()?);
        let payload_len = u16::from_le_bytes(data[36..38].try_into().ok()?) as usize;
        if data.len() != FRAME_HEADER_SIZE + payload_len {
            return None;
        }
        Some(Self { sender, rssi, snr, payload: data[FRAME_HEADER_SIZE..].to_vec() })
    }
}

/// Compute a 32-byte blake3 content hash for deduplication.
#[must_use]
pub fn content_hash(payload: &[u8]) -> [u8; 32] {
    *blake3::hash(payload).as_bytes()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn snr_grading() {
        // SF7: min_snr = -2.5 * (7 - 4) = -7.5
        assert_eq!(snr_grade(10, 7), Grade::Good); // margin = 17.5
        assert_eq!(snr_grade(-5, 7), Grade::Marginal); // margin = 2.5 < 3
        assert_eq!(snr_grade(-8, 7), Grade::Unreliable); // margin = -0.5
        assert_eq!(snr_grade(-50, 7), Grade::Invalid); // out of range

        // SF12: min_snr = -2.5 * (12 - 4) = -20.0
        assert_eq!(snr_grade(-15, 12), Grade::Good); // margin = 5.0
        assert_eq!(snr_grade(-18, 12), Grade::Marginal); // margin = 2.0
        assert_eq!(snr_grade(-22, 12), Grade::Unreliable); // margin = -2.0
    }

    #[test]
    fn gossip_frame_roundtrip() {
        let key = iroh::SecretKey::generate();
        let frame = GossipFrame::new(&key.public(), -85, 8, vec![1, 2, 3, 4, 5]);
        let encoded = frame.encode();
        let decoded = GossipFrame::decode(&encoded).expect("decode should succeed");
        assert_eq!(decoded.sender, *key.public().as_bytes());
        assert_eq!(decoded.rssi, -85);
        assert_eq!(decoded.snr, 8);
        assert_eq!(decoded.payload, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn gossip_frame_reject_short() {
        assert!(GossipFrame::decode(&[0u8; 10]).is_none());
    }

    #[test]
    fn gossip_frame_decode_empty_payload() {
        // Exactly FRAME_HEADER_SIZE (38) bytes with payload_len=0 must decode.
        // Kills mutant: `data.len() < 38` → `data.len() <= 38`.
        let mut data = vec![0u8; 38];
        data[36..38].copy_from_slice(&0u16.to_le_bytes()); // payload_len = 0
        let frame = GossipFrame::decode(&data);
        assert!(frame.is_some(), "38-byte frame with 0-length payload must decode");
        assert!(frame.unwrap().payload.is_empty());
    }

    #[test]
    fn gossip_frame_reject_wrong_length() {
        // Header says payload_len=5 but only 3 bytes follow.
        let mut data = vec![0u8; 38 + 3];
        data[36..38].copy_from_slice(&5u16.to_le_bytes());
        assert!(GossipFrame::decode(&data).is_none());
    }

    #[test]
    fn should_forward_true_for_good_and_marginal() {
        assert!(Grade::Good.should_forward());
        assert!(Grade::Marginal.should_forward());
    }

    #[test]
    fn should_forward_false_for_unreliable_and_invalid() {
        assert!(!Grade::Unreliable.should_forward());
        assert!(!Grade::Invalid.should_forward());
    }

    #[test]
    fn grade_display() {
        assert_eq!(format!("{}", Grade::Good), "GOOD");
        assert_eq!(format!("{}", Grade::Marginal), "MARG");
        assert_eq!(format!("{}", Grade::Unreliable), "UNRL");
        assert_eq!(format!("{}", Grade::Invalid), "INVL");
    }

    #[test]
    fn snr_grade_exact_boundaries() {
        // SF12: min_snr = -2.5 * (12 - 4) = -20.0
        //
        // At snr=-20, margin = -20 - (-20.0) = 0.0 exactly.
        // The code uses `margin < 0.0`, so 0.0 is NOT < 0.0 → Marginal (not Unreliable).
        assert_eq!(snr_grade(-20, 12), Grade::Marginal);
        // One below: snr=-21, margin = -1.0 → Unreliable.
        assert_eq!(snr_grade(-21, 12), Grade::Unreliable);

        // At snr=-17, margin = -17 - (-20.0) = 3.0 exactly.
        // The code uses `margin < 3.0`, so 3.0 is NOT < 3.0 → Good (not Marginal).
        assert_eq!(snr_grade(-17, 12), Grade::Good);
        // One below: snr=-18, margin = 2.0 → Marginal.
        assert_eq!(snr_grade(-18, 12), Grade::Marginal);
    }

    #[test]
    fn content_hash_deterministic() {
        let h1 = content_hash(b"hello");
        let h2 = content_hash(b"hello");
        assert_eq!(h1, h2);
        let h3 = content_hash(b"world");
        assert_ne!(h1, h3);
    }

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn gossip_frame_roundtrip_prop(
                rssi in -200i16..=200i16,
                snr in -32i16..=32i16,
                payload in proptest::collection::vec(any::<u8>(), 0..256),
            ) {
                let key = iroh::SecretKey::generate();
                let frame = GossipFrame::new(&key.public(), rssi, snr, payload.clone());
                let encoded = frame.encode();
                let decoded = GossipFrame::decode(&encoded).unwrap();
                prop_assert_eq!(decoded.rssi, rssi);
                prop_assert_eq!(decoded.snr, snr);
                prop_assert_eq!(decoded.payload, payload);
            }

            #[test]
            fn content_hash_deterministic_prop(data in proptest::collection::vec(any::<u8>(), 0..1024)) {
                prop_assert_eq!(content_hash(&data), content_hash(&data));
            }

            #[test]
            fn snr_grade_never_panics(snr in i16::MIN..=i16::MAX, sf in 5u8..=12u8) {
                let _ = snr_grade(snr, sf);
            }
        }
    }
}
