use iroh::PublicKey;

/// Wire protocol version.
const VERSION: u8 = 0;

/// Envelope header size: version(1) + origin(32) + seq(8) + rssi(2) + snr(2) + payload_len(2) = 47
const HEADER_SIZE: usize = 47;

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
/// spreading factor. See PROTOCOL.md for the formula.
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

/// A packet received from the radio, before wrapping in an envelope.
#[derive(Debug, Clone)]
pub struct RadioPacket {
    pub rssi: i16,
    pub snr: i16,
    pub payload: Vec<u8>,
}

/// A bridge envelope — the wire format sent between iroh peers.
#[derive(Debug, Clone)]
pub struct Envelope {
    /// NodeId of the bridge that first heard this packet on radio.
    pub origin: PublicKey,
    /// Per-origin sequence number (monotonically increasing).
    pub seq: u64,
    /// RSSI at the originating bridge (informational).
    pub rssi: i16,
    /// SNR at the originating bridge (informational).
    pub snr: i16,
    /// Raw LoRa payload.
    pub payload: Vec<u8>,
}

impl Envelope {
    /// Encode this envelope into bytes for sending as a QUIC datagram.
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(HEADER_SIZE + self.payload.len());
        buf.push(VERSION);
        buf.extend_from_slice(self.origin.as_bytes());
        buf.extend_from_slice(&self.seq.to_le_bytes());
        buf.extend_from_slice(&self.rssi.to_le_bytes());
        buf.extend_from_slice(&self.snr.to_le_bytes());
        let len = self.payload.len() as u16;
        buf.extend_from_slice(&len.to_le_bytes());
        buf.extend_from_slice(&self.payload);
        buf
    }

    /// Decode an envelope from bytes received as a QUIC datagram.
    pub fn decode(data: &[u8]) -> Option<Self> {
        if data.len() < HEADER_SIZE {
            return None;
        }
        if data[0] != VERSION {
            return None;
        }
        let origin = PublicKey::from_bytes(data[1..33].try_into().ok()?).ok()?;
        let seq = u64::from_le_bytes(data[33..41].try_into().ok()?);
        let rssi = i16::from_le_bytes(data[41..43].try_into().ok()?);
        let snr = i16::from_le_bytes(data[43..45].try_into().ok()?);
        let payload_len = u16::from_le_bytes(data[45..47].try_into().ok()?) as usize;
        if data.len() != HEADER_SIZE + payload_len {
            return None;
        }
        Some(Self {
            origin,
            seq,
            rssi,
            snr,
            payload: data[HEADER_SIZE..].to_vec(),
        })
    }
}

/// Compute a 16-byte content hash for deduplication.
pub fn content_hash(payload: &[u8]) -> [u8; 16] {
    let hash = blake3::hash(payload);
    let bytes = hash.as_bytes();
    let mut out = [0u8; 16];
    out.copy_from_slice(&bytes[..16]);
    out
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
    fn envelope_roundtrip() {
        let key = iroh::SecretKey::generate(&mut rand::rng());
        let env = Envelope {
            origin: key.public(),
            seq: 42,
            rssi: -85,
            snr: 8,
            payload: vec![1, 2, 3, 4, 5],
        };
        let encoded = env.encode();
        let decoded = Envelope::decode(&encoded).unwrap();
        assert_eq!(decoded.origin, env.origin);
        assert_eq!(decoded.seq, env.seq);
        assert_eq!(decoded.rssi, env.rssi);
        assert_eq!(decoded.snr, env.snr);
        assert_eq!(decoded.payload, env.payload);
    }

    #[test]
    fn envelope_reject_short() {
        assert!(Envelope::decode(&[0u8; 10]).is_none());
    }

    #[test]
    fn envelope_reject_bad_version() {
        let mut data = vec![1u8]; // version 1
        data.extend_from_slice(&[0u8; 46]);
        assert!(Envelope::decode(&data).is_none());
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
