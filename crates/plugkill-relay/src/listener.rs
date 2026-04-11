use crate::config::Config;
use crate::crypto::{self, NonceCache};
use crate::protocol::{self, PacketType};
use crate::sender;
use crate::trigger;
use log::{info, warn};
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

const TIMESTAMP_WINDOW_SECS: u64 = 5;

/// Run the UDP listener. Blocks until `running` is set to false.
/// When `dry_run` is true, KILL packets are validated and ACKed but
/// no local kill or chain propagation is triggered.
pub fn run(
    config: &Config,
    private_key: &[u8; 32],
    our_pubkey: &[u8; 32],
    running: Arc<AtomicBool>,
    dry_run: bool,
) {
    let bind_addr = format!("0.0.0.0:{}", config.general.listen_port);
    let socket = match UdpSocket::bind(&bind_addr) {
        Ok(s) => s,
        Err(e) => {
            log::error!("failed to bind UDP listener on {}: {e}", bind_addr);
            return;
        }
    };
    socket
        .set_read_timeout(Some(std::time::Duration::from_millis(500)))
        .expect("failed to set read timeout");

    info!("listening on UDP {}", bind_addr);

    let mut nonce_cache = NonceCache::new();
    let mut recv_buf = [0u8; protocol::MAX_PACKET_SIZE];

    while running.load(Ordering::Relaxed) {
        let (len, src) = match socket.recv_from(&mut recv_buf) {
            Ok(r) => r,
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => continue,
            Err(e) => {
                warn!("recv error: {e}");
                continue;
            }
        };

        let data = &recv_buf[..len];

        let (packet, msg_bytes, sig) = match protocol::deserialize(data) {
            Ok(r) => r,
            Err(_) => continue,
        };

        let peer = match config.peers.iter().find(|p| {
            p.decode_pubkey()
                .map(|pk| pk == packet.sender_id)
                .unwrap_or(false)
        }) {
            Some(p) => p,
            None => continue,
        };

        if !crypto::verify(&packet.sender_id, msg_bytes, &sig) {
            warn!("invalid signature from peer '{}' ({})", peer.name, src);
            continue;
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let diff = if now > packet.timestamp {
            now - packet.timestamp
        } else {
            packet.timestamp - now
        };
        if diff > TIMESTAMP_WINDOW_SECS {
            warn!(
                "stale packet from '{}': timestamp delta {}s",
                peer.name, diff
            );
            continue;
        }

        if nonce_cache.check_and_insert(&packet.nonce) {
            continue;
        }

        match packet.packet_type {
            PacketType::Kill => {
                info!(
                    "KILL received from '{}' ({}): reason={}",
                    peer.name, src, packet.reason
                );

                send_ack(&socket, src, &packet.nonce, private_key, our_pubkey);

                if dry_run {
                    warn!("DRY-RUN: would trigger local kill and relay to peers — skipping");
                } else {
                    let _ = sender::fan_out(
                        config,
                        private_key,
                        our_pubkey,
                        &format!("relay:{}", packet.reason),
                        Some(&packet.sender_id),
                    );

                    trigger::trigger_local_kill(&config.general.plugkill_socket);
                }
            }
            PacketType::Ack => {}
        }
    }
}

fn send_ack(
    socket: &UdpSocket,
    dest: std::net::SocketAddr,
    kill_nonce: &[u8; 16],
    private_key: &[u8; 32],
    our_pubkey: &[u8; 32],
) {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let ack_nonce = crypto::random_nonce();
    let reason = hex::encode(kill_nonce);

    let packet = protocol::serialize(
        PacketType::Ack,
        timestamp,
        &ack_nonce,
        our_pubkey,
        &reason,
        |msg| crypto::sign(private_key, msg),
    );

    if let Err(e) = socket.send_to(&packet, dest) {
        warn!("failed to send ACK to {}: {e}", dest);
    }
}
