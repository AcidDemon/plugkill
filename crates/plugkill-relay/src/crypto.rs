use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use rand::RngCore;
use rand::rngs::OsRng;
use std::collections::VecDeque;

const NONCE_CACHE_SIZE: usize = 1000;

/// Generate a new ed25519 keypair. Returns (private_key_bytes, public_key_bytes).
pub fn generate_keypair() -> ([u8; 32], [u8; 32]) {
    let signing_key = SigningKey::generate(&mut OsRng);
    let verifying_key = signing_key.verifying_key();
    (signing_key.to_bytes(), verifying_key.to_bytes())
}

/// Sign a message with a private key. Returns 64-byte signature.
pub fn sign(private_key: &[u8; 32], message: &[u8]) -> [u8; 64] {
    let signing_key = SigningKey::from_bytes(private_key);
    let signature = signing_key.sign(message);
    signature.to_bytes()
}

/// Verify a signature against a public key and message.
pub fn verify(public_key: &[u8; 32], message: &[u8], signature: &[u8; 64]) -> bool {
    let Ok(verifying_key) = VerifyingKey::from_bytes(public_key) else {
        return false;
    };
    let sig = ed25519_dalek::Signature::from_bytes(signature);
    verifying_key.verify(message, &sig).is_ok()
}

/// Ring buffer of recently seen nonces for replay protection.
pub struct NonceCache {
    seen: VecDeque<[u8; 16]>,
}

impl NonceCache {
    pub fn new() -> Self {
        Self {
            seen: VecDeque::with_capacity(NONCE_CACHE_SIZE),
        }
    }

    /// Returns true if the nonce was already seen. Inserts it if new.
    pub fn check_and_insert(&mut self, nonce: &[u8; 16]) -> bool {
        if self.seen.contains(nonce) {
            return true;
        }
        if self.seen.len() >= NONCE_CACHE_SIZE {
            self.seen.pop_front();
        }
        self.seen.push_back(*nonce);
        false
    }
}

/// Generate a random 16-byte nonce.
pub fn random_nonce() -> [u8; 16] {
    let mut nonce = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut nonce);
    nonce
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_keypair_roundtrip() {
        let (privkey, pubkey) = generate_keypair();
        let message = b"test message";
        let sig = sign(&privkey, message);
        assert!(verify(&pubkey, message, &sig));
    }

    #[test]
    fn test_wrong_key_rejects() {
        let (privkey, _) = generate_keypair();
        let (_, other_pubkey) = generate_keypair();
        let message = b"test message";
        let sig = sign(&privkey, message);
        assert!(!verify(&other_pubkey, message, &sig));
    }

    #[test]
    fn test_tampered_message_rejects() {
        let (privkey, pubkey) = generate_keypair();
        let sig = sign(&privkey, b"original");
        assert!(!verify(&pubkey, b"tampered", &sig));
    }

    #[test]
    fn test_nonce_cache_detects_replay() {
        let mut cache = NonceCache::new();
        let nonce = random_nonce();
        assert!(!cache.check_and_insert(&nonce));
        assert!(cache.check_and_insert(&nonce));
    }

    #[test]
    fn test_nonce_cache_eviction() {
        let mut cache = NonceCache::new();
        let first_nonce = random_nonce();
        cache.check_and_insert(&first_nonce);

        for _ in 0..NONCE_CACHE_SIZE {
            cache.check_and_insert(&random_nonce());
        }

        // first nonce should be evicted
        assert!(!cache.check_and_insert(&first_nonce));
    }
}
