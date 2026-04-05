/// Gossip frame header size: sender(32) + rssi(2) + snr(2) + payload_len(2) = 38
const FRAME_HEADER_SIZE: usize = 38;

/// SNR quality grade for a received LoRa packet.
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
    pub fn should_forward(self) -> bool {
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
    /// EndpointId (PublicKey bytes) of the bridge that first heard this packet on radio.
    pub sender: [u8; 32],
    /// RSSI at the originating bridge (informational).
    pub rssi: i16,
    /// SNR at the originating bridge (informational).
    pub snr: i16,
    /// Raw LoRa payload.
    pub payload: Vec<u8>,
}

impl GossipFrame {
    /// Create a new gossip frame from a public key and radio packet data.
    pub fn new(sender: &iroh::PublicKey, rssi: i16, snr: i16, payload: Vec<u8>) -> Self {
        Self {
            sender: *sender.as_bytes(),
            rssi,
            snr,
            payload,
        }
    }

    /// Encode this frame into bytes for gossip broadcast.
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(FRAME_HEADER_SIZE + self.payload.len());
        buf.extend_from_slice(&self.sender);
        buf.extend_from_slice(&self.rssi.to_le_bytes());
        buf.extend_from_slice(&self.snr.to_le_bytes());
        let len = self.payload.len() as u16;
        buf.extend_from_slice(&len.to_le_bytes());
        buf.extend_from_slice(&self.payload);
        buf
    }

    /// Decode a gossip frame from bytes received from the swarm.
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
        Some(Self {
            sender,
            rssi,
            snr,
            payload: data[FRAME_HEADER_SIZE..].to_vec(),
        })
    }
}

/// Compute a 32-byte blake3 content hash for deduplication.
pub fn content_hash(payload: &[u8]) -> [u8; 32] {
    *blake3::hash(payload).as_bytes()
}

#[cfg(test)]
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
        let key = iroh::SecretKey::generate(&mut rand::rng());
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
    fn gossip_frame_reject_wrong_length() {
        // Header says payload_len=5 but only 3 bytes follow.
        let mut data = vec![0u8; 38 + 3];
        data[36..38].copy_from_slice(&5u16.to_le_bytes());
        assert!(GossipFrame::decode(&data).is_none());
    }

    #[test]
    fn content_hash_deterministic() {
        let h1 = content_hash(b"hello");
        let h2 = content_hash(b"hello");
        assert_eq!(h1, h2);
        let h3 = content_hash(b"world");
        assert_ne!(h1, h3);
    }
}
