use crate::config::{Config, PeerConfig};
use crate::crypto;
use crate::protocol::{self, PacketType};
use crate::resolve::Resolver;
use log::{info, warn};
use std::collections::{HashMap, HashSet};
use std::net::UdpSocket;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Which peers ACKed and which timed out.
pub struct FanOutResult {
    pub acked: Vec<String>,
    pub timed_out: Vec<String>,
}

/// Send a KILL to all peers with retry. Blocking.
/// `exclude_pubkey` skips a peer (chain propagation).
pub fn fan_out(
    config: &Config,
    private_key: &[u8; 32],
    our_pubkey: &[u8; 32],
    reason: &str,
    exclude_pubkey: Option<&[u8; 32]>,
) -> FanOutResult {
    let socket = match UdpSocket::bind("0.0.0.0:0") {
        Ok(s) => s,
        Err(e) => {
            warn!("failed to bind UDP socket: {e}");
            let timed_out = config.peers.iter().map(|p| p.name.clone()).collect();
            return FanOutResult {
                acked: vec![],
                timed_out,
            };
        }
    };
    socket
        .set_nonblocking(true)
        .expect("failed to set nonblocking");

    let nonce = crypto::random_nonce();
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let packet = protocol::serialize(
        PacketType::Kill,
        timestamp,
        &nonce,
        our_pubkey,
        reason,
        |msg| crypto::sign(private_key, msg),
    );

    let mut resolver = Resolver::new();
    let mut pending_peers: HashMap<String, &PeerConfig> = HashMap::new();
    let mut acked: HashSet<String> = HashSet::new();

    for peer in &config.peers {
        if peer.addresses.is_empty() {
            continue;
        }
        if let Some(exclude) = exclude_pubkey {
            if let Ok(pk) = peer.decode_pubkey() {
                if pk == *exclude {
                    continue;
                }
            }
        }
        pending_peers.insert(peer.name.clone(), peer);
    }

    if pending_peers.is_empty() {
        return FanOutResult {
            acked: vec![],
            timed_out: vec![],
        };
    }

    // Initial blast
    for peer in pending_peers.values() {
        send_to_peer(&socket, &mut resolver, peer, &packet);
    }

    let ack_timeout = Duration::from_millis(config.retry.ack_timeout_ms);
    let mut recv_buf = [0u8; protocol::MAX_PACKET_SIZE];

    for retry in 0..=config.retry.max_retries {
        let deadline = Instant::now() + ack_timeout;

        while Instant::now() < deadline {
            match socket.recv_from(&mut recv_buf) {
                Ok((len, _src)) => {
                    if let Some(peer_name) = check_ack(&recv_buf[..len], &nonce, &config.peers) {
                        if acked.insert(peer_name.clone()) {
                            info!("peer '{}' ACKed", peer_name);
                        }
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(_) => break,
            }
        }

        for name in &acked {
            pending_peers.remove(name);
        }

        if pending_peers.is_empty() || retry == config.retry.max_retries {
            break;
        }

        for peer in pending_peers.values() {
            warn!("retrying peer '{}' (attempt {})", peer.name, retry + 2);
            send_to_peer(&socket, &mut resolver, peer, &packet);
        }
    }

    let timed_out: Vec<String> = pending_peers.keys().cloned().collect();

    FanOutResult {
        acked: acked.into_iter().collect(),
        timed_out,
    }
}

fn send_to_peer(socket: &UdpSocket, resolver: &mut Resolver, peer: &PeerConfig, packet: &[u8]) {
    for addr_str in &peer.addresses {
        let addrs = resolver.resolve(addr_str);
        for addr in addrs {
            if let Err(e) = socket.send_to(packet, addr) {
                warn!("failed to send to {} ({}): {}", peer.name, addr, e);
            }
        }
    }
}

/// Check if a received packet is a valid ACK for our nonce. Returns peer name if valid.
fn check_ack(data: &[u8], expected_nonce: &[u8; 16], peers: &[PeerConfig]) -> Option<String> {
    let (packet, msg_bytes, sig) = protocol::deserialize(data).ok()?;
    if packet.packet_type != PacketType::Ack {
        return None;
    }

    let peer = peers.iter().find(|p| {
        p.decode_pubkey()
            .map(|pk| pk == packet.sender_id)
            .unwrap_or(false)
    })?;

    if !crypto::verify(&packet.sender_id, msg_bytes, &sig) {
        warn!("ACK from '{}' has invalid signature", peer.name);
        return None;
    }

    let expected_hex = hex::encode(expected_nonce);
    if packet.reason != expected_hex {
        return None;
    }

    Some(peer.name.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{PeerConfig, RetryConfig};
    use base64::prelude::*;

    fn make_peer(name: &str, pubkey: &[u8; 32]) -> PeerConfig {
        PeerConfig {
            name: name.to_string(),
            pubkey: base64::prelude::BASE64_STANDARD.encode(pubkey),
            addresses: vec!["127.0.0.1:19999".to_string()],
        }
    }

    #[test]
    fn test_check_ack_valid() {
        let (privkey, pubkey) = crypto::generate_keypair();
        let nonce = crypto::random_nonce();
        let ack_reason = hex::encode(nonce);

        let data = protocol::serialize(PacketType::Ack, 0, &nonce, &pubkey, &ack_reason, |msg| {
            crypto::sign(&privkey, msg)
        });

        let peers = vec![make_peer("alpha", &pubkey)];
        let result = check_ack(&data, &nonce, &peers);
        assert_eq!(result, Some("alpha".to_string()));
    }

    #[test]
    fn test_check_ack_wrong_nonce() {
        let (privkey, pubkey) = crypto::generate_keypair();
        let nonce = crypto::random_nonce();
        let other_nonce = crypto::random_nonce();
        let ack_reason = hex::encode(nonce);

        let data = protocol::serialize(PacketType::Ack, 0, &nonce, &pubkey, &ack_reason, |msg| {
            crypto::sign(&privkey, msg)
        });

        let peers = vec![make_peer("alpha", &pubkey)];
        // check against a different nonce — must reject
        let result = check_ack(&data, &other_nonce, &peers);
        assert!(result.is_none());
    }

    #[test]
    fn test_check_ack_kill_packet_ignored() {
        let (privkey, pubkey) = crypto::generate_keypair();
        let nonce = crypto::random_nonce();

        let data = protocol::serialize(PacketType::Kill, 0, &nonce, &pubkey, "reason", |msg| {
            crypto::sign(&privkey, msg)
        });

        let peers = vec![make_peer("alpha", &pubkey)];
        let result = check_ack(&data, &nonce, &peers);
        assert!(result.is_none());
    }

    #[test]
    fn test_check_ack_unknown_sender_ignored() {
        let (privkey, pubkey) = crypto::generate_keypair();
        let (_, other_pubkey) = crypto::generate_keypair();
        let nonce = crypto::random_nonce();
        let ack_reason = hex::encode(nonce);

        let data = protocol::serialize(PacketType::Ack, 0, &nonce, &pubkey, &ack_reason, |msg| {
            crypto::sign(&privkey, msg)
        });

        // peer list has a different pubkey — sender is unknown
        let peers = vec![make_peer("stranger", &other_pubkey)];
        let result = check_ack(&data, &nonce, &peers);
        assert!(result.is_none());
    }

    #[test]
    fn test_check_ack_bad_signature_rejected() {
        let (privkey, pubkey) = crypto::generate_keypair();
        let nonce = crypto::random_nonce();
        let ack_reason = hex::encode(nonce);

        let mut data =
            protocol::serialize(PacketType::Ack, 0, &nonce, &pubkey, &ack_reason, |msg| {
                crypto::sign(&privkey, msg)
            });

        // corrupt the signature
        let last = data.len() - 1;
        data[last] ^= 0xFF;

        let peers = vec![make_peer("alpha", &pubkey)];
        let result = check_ack(&data, &nonce, &peers);
        assert!(result.is_none());
    }

    #[test]
    fn test_fan_out_no_peers_returns_empty() {
        let (privkey, pubkey) = crypto::generate_keypair();
        let config = Config {
            general: Default::default(),
            retry: RetryConfig {
                ack_timeout_ms: 10,
                max_retries: 0,
            },
            peers: vec![],
        };
        let result = fan_out(&config, &privkey, &pubkey, "test", None);
        assert!(result.acked.is_empty());
        assert!(result.timed_out.is_empty());
    }

    #[test]
    fn test_fan_out_skips_send_only_peers() {
        let (privkey, pubkey) = crypto::generate_keypair();
        let (_, peer_pubkey) = crypto::generate_keypair();
        let config = Config {
            general: Default::default(),
            retry: RetryConfig {
                ack_timeout_ms: 10,
                max_retries: 0,
            },
            peers: vec![PeerConfig {
                name: "send-only".to_string(),
                pubkey: base64::prelude::BASE64_STANDARD.encode(peer_pubkey),
                addresses: vec![], // no addresses = send-only
            }],
        };
        let result = fan_out(&config, &privkey, &pubkey, "test", None);
        assert!(result.acked.is_empty());
        assert!(result.timed_out.is_empty());
    }

    #[test]
    fn test_fan_out_exclude_pubkey() {
        let (privkey, pubkey) = crypto::generate_keypair();
        let (_, peer_pubkey) = crypto::generate_keypair();
        let config = Config {
            general: Default::default(),
            retry: RetryConfig {
                ack_timeout_ms: 10,
                max_retries: 0,
            },
            peers: vec![make_peer("excluded", &peer_pubkey)],
        };
        // exclude the one peer — should have nothing to send
        let result = fan_out(&config, &privkey, &pubkey, "test", Some(&peer_pubkey));
        assert!(result.acked.is_empty());
        assert!(result.timed_out.is_empty());
    }

    #[test]
    fn test_fan_out_unreachable_peer_times_out() {
        let (privkey, pubkey) = crypto::generate_keypair();
        let (_, peer_pubkey) = crypto::generate_keypair();
        // Use a port that no one is listening on (port 1 is unreachable without root)
        let config = Config {
            general: Default::default(),
            retry: RetryConfig {
                ack_timeout_ms: 10,
                max_retries: 1,
            },
            peers: vec![PeerConfig {
                name: "ghost".to_string(),
                pubkey: base64::prelude::BASE64_STANDARD.encode(peer_pubkey),
                addresses: vec!["127.0.0.1:19998".to_string()],
            }],
        };
        let result = fan_out(&config, &privkey, &pubkey, "test", None);
        assert!(result.acked.is_empty());
        assert_eq!(result.timed_out, vec!["ghost".to_string()]);
    }
}
