// Wire format:
//   magic(4) | version(1) | type(1) | timestamp(8) | nonce(16) |
//   sender_id(32) | reason_len(2) | reason(var) | signature(64)

pub const MAGIC: &[u8; 4] = b"PKRL";
pub const VERSION: u8 = 1;
pub const HEADER_SIZE: usize = 4 + 1 + 1 + 8 + 16 + 32 + 2; // 64 bytes
pub const SIGNATURE_SIZE: usize = 64;
pub const MAX_REASON_LEN: usize = 256;
pub const MIN_PACKET_SIZE: usize = HEADER_SIZE + SIGNATURE_SIZE; // 128 bytes
pub const MAX_PACKET_SIZE: usize = HEADER_SIZE + MAX_REASON_LEN + SIGNATURE_SIZE; // 384 bytes

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PacketType {
    Kill = 0x01,
    Ack = 0x02,
}

impl PacketType {
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            0x01 => Some(Self::Kill),
            0x02 => Some(Self::Ack),
            _ => None,
        }
    }
}

/// Parsed packet (signature already stripped and verified externally).
#[derive(Debug, Clone)]
pub struct Packet {
    pub packet_type: PacketType,
    pub timestamp: u64,
    pub nonce: [u8; 16],
    pub sender_id: [u8; 32],
    pub reason: String,
}

/// Serialize a packet into bytes, sign it, and return the full datagram.
pub fn serialize(
    packet_type: PacketType,
    timestamp: u64,
    nonce: &[u8; 16],
    sender_pubkey: &[u8; 32],
    reason: &str,
    sign_fn: impl FnOnce(&[u8]) -> [u8; 64],
) -> Vec<u8> {
    let reason_bytes = reason.as_bytes();
    let reason_len = reason_bytes.len().min(MAX_REASON_LEN);
    let total = HEADER_SIZE + reason_len + SIGNATURE_SIZE;
    let mut buf = Vec::with_capacity(total);

    buf.extend_from_slice(MAGIC);
    buf.push(VERSION);
    buf.push(packet_type as u8);
    buf.extend_from_slice(&timestamp.to_be_bytes());
    buf.extend_from_slice(nonce);
    buf.extend_from_slice(sender_pubkey);
    buf.extend_from_slice(&(reason_len as u16).to_be_bytes());
    buf.extend_from_slice(&reason_bytes[..reason_len]);

    let signature = sign_fn(&buf);
    buf.extend_from_slice(&signature);
    buf
}

/// Deserialize a packet. Returns (packet, message_bytes_for_verification, signature).
/// Caller must verify the signature externally.
pub fn deserialize(data: &[u8]) -> Result<(Packet, &[u8], [u8; 64]), &'static str> {
    if data.len() < MIN_PACKET_SIZE {
        return Err("packet too short");
    }
    if &data[0..4] != MAGIC {
        return Err("invalid magic");
    }
    if data[4] != VERSION {
        return Err("unknown version");
    }
    let packet_type = PacketType::from_byte(data[5]).ok_or("unknown packet type")?;
    let timestamp = u64::from_be_bytes(data[6..14].try_into().unwrap());
    let nonce: [u8; 16] = data[14..30].try_into().unwrap();
    let sender_id: [u8; 32] = data[30..62].try_into().unwrap();
    let reason_len = u16::from_be_bytes(data[62..64].try_into().unwrap()) as usize;

    if reason_len > MAX_REASON_LEN {
        return Err("reason too long");
    }
    let expected_size = HEADER_SIZE + reason_len + SIGNATURE_SIZE;
    if data.len() < expected_size {
        return Err("packet truncated");
    }

    let reason_end = HEADER_SIZE + reason_len;
    let reason = std::str::from_utf8(&data[HEADER_SIZE..reason_end])
        .map_err(|_| "reason not valid UTF-8")?
        .to_string();

    let message_bytes = &data[..reason_end];
    let sig_bytes: [u8; 64] = data[reason_end..reason_end + SIGNATURE_SIZE]
        .try_into()
        .unwrap();

    let packet = Packet {
        packet_type,
        timestamp,
        nonce,
        sender_id,
        reason,
    };

    Ok((packet, message_bytes, sig_bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto;

    #[test]
    fn test_roundtrip_kill_packet() {
        let (privkey, pubkey) = crypto::generate_keypair();
        let nonce = crypto::random_nonce();
        let timestamp = 1712835600u64;
        let reason = "usb_violation";

        let data = serialize(
            PacketType::Kill,
            timestamp,
            &nonce,
            &pubkey,
            reason,
            |msg| crypto::sign(&privkey, msg),
        );

        let (packet, msg_bytes, sig) = deserialize(&data).unwrap();
        assert_eq!(packet.packet_type, PacketType::Kill);
        assert_eq!(packet.timestamp, timestamp);
        assert_eq!(packet.nonce, nonce);
        assert_eq!(packet.sender_id, pubkey);
        assert_eq!(packet.reason, reason);
        assert!(crypto::verify(&pubkey, msg_bytes, &sig));
    }

    #[test]
    fn test_roundtrip_ack_packet() {
        let (privkey, pubkey) = crypto::generate_keypair();
        let nonce = crypto::random_nonce();
        let ack_reason = hex::encode(nonce);

        let data = serialize(
            PacketType::Ack,
            1712835600,
            &nonce,
            &pubkey,
            &ack_reason,
            |msg| crypto::sign(&privkey, msg),
        );

        let (packet, _, _) = deserialize(&data).unwrap();
        assert_eq!(packet.packet_type, PacketType::Ack);
        assert_eq!(packet.reason, ack_reason);
    }

    #[test]
    fn test_bad_magic_rejected() {
        let mut data = vec![0u8; MIN_PACKET_SIZE];
        data[0..4].copy_from_slice(b"XXXX");
        assert_eq!(deserialize(&data).unwrap_err(), "invalid magic");
    }

    #[test]
    fn test_too_short_rejected() {
        let data = vec![0u8; MIN_PACKET_SIZE - 1];
        assert_eq!(deserialize(&data).unwrap_err(), "packet too short");
    }

    #[test]
    fn test_empty_reason() {
        let (privkey, pubkey) = crypto::generate_keypair();
        let nonce = crypto::random_nonce();
        let data = serialize(PacketType::Kill, 0, &nonce, &pubkey, "", |msg| {
            crypto::sign(&privkey, msg)
        });
        let (packet, _, _) = deserialize(&data).unwrap();
        assert_eq!(packet.reason, "");
    }

    #[test]
    fn test_tampered_signature_fails_verify() {
        let (privkey, pubkey) = crypto::generate_keypair();
        let nonce = crypto::random_nonce();
        let mut data = serialize(PacketType::Kill, 0, &nonce, &pubkey, "test", |msg| {
            crypto::sign(&privkey, msg)
        });
        // flip a byte in the timestamp (covered by signature, not UTF-8 validated)
        data[6] ^= 0xFF;
        let (_, msg_bytes, sig) = deserialize(&data).unwrap();
        assert!(!crypto::verify(&pubkey, msg_bytes, &sig));
    }
}
