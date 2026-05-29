//! Journal at-rest encryption for Edge mode.
//!
//! Wraps file I/O with AES-256-GCM encryption via the `ring` crate.
//! Key is derived from `ASTRA_JOURNAL_KEY` env var or auto-generated
//! from the machine-id, ensuring journal files on disk are encrypted.

use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};
use ring::rand::{SecureRandom, SystemRandom};

const NONCE_LEN: usize = 12; // AES-256-GCM uses 96-bit (12-byte) nonces

/// Encryption context for journal files.
pub struct JournalCrypto {
    key: LessSafeKey,
}

impl JournalCrypto {
    /// Create from a raw 32-byte key.
    pub fn new(key_bytes: &[u8; 32]) -> Self {
        let unbound = UnboundKey::new(&AES_256_GCM, key_bytes).expect("valid AES-256-GCM key");
        Self {
            key: LessSafeKey::new(unbound),
        }
    }

    /// Derive key from env var `ASTRA_JOURNAL_KEY` (hex), or generate from machine-id.
    pub fn from_env_or_machine() -> Self {
        if let Ok(hex) = std::env::var("ASTRA_JOURNAL_KEY")
            && let Some(bytes) = parse_hex_32(&hex)
        {
            return Self::new(&bytes);
        }
        Self::from_machine_id()
    }

    fn from_machine_id() -> Self {
        use sha2::{Digest, Sha256};
        let machine_id = machine_identity();
        let mut hasher = Sha256::new();
        hasher.update(b"astra-journal-v1:");
        hasher.update(machine_id.as_bytes());
        let hash = hasher.finalize();
        let mut key = [0u8; 32];
        key.copy_from_slice(&hash);
        Self::new(&key)
    }

    pub fn encrypt(&self, plaintext: &[u8]) -> Vec<u8> {
        let rng = SystemRandom::new();
        let mut nonce_bytes = [0u8; NONCE_LEN];
        rng.fill(&mut nonce_bytes).expect("system random available");
        let nonce = Nonce::assume_unique_for_key(nonce_bytes);

        let mut in_out = plaintext.to_vec();
        self.key
            .seal_in_place_append_tag(nonce, Aad::empty(), &mut in_out)
            .expect("seal_in_place_append_tag succeeds");

        // Prepend nonce to ciphertext
        let mut result = nonce_bytes.to_vec();
        result.extend_from_slice(&in_out);
        result
    }

    /// Decrypt `nonce || ciphertext` and return plaintext.
    /// Returns `None` on authentication failure.
    pub fn decrypt(&self, encrypted: &[u8]) -> Option<Vec<u8>> {
        if encrypted.len() < NONCE_LEN + 16 {
            // Too short: needs at least nonce + GCM tag (16 bytes)
            return None;
        }
        let (nonce_bytes, ciphertext) = encrypted.split_at(NONCE_LEN);
        let nonce = Nonce::assume_unique_for_key(nonce_bytes.try_into().ok()?);

        let mut in_out = ciphertext.to_vec();
        self.key
            .open_in_place(nonce, Aad::empty(), &mut in_out)
            .ok()?;

        // Remove the appended tag
        in_out.truncate(in_out.len().saturating_sub(16));
        Some(in_out)
    }
}

/// Parse a 64-char hex string into a 32-byte key. Returns None on invalid input.
fn parse_hex_32(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        if chunk.len() != 2 {
            return None;
        }
        let hi = hex_val(chunk[0])?;
        let lo = hex_val(chunk[1])?;
        bytes[i] = (hi << 4) | lo;
    }
    Some(bytes)
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Hex-encode arbitrary bytes for safe text storage.
pub fn hex_encode(bytes: &[u8]) -> String {
    let hex_chars: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        result.push(hex_chars[(b >> 4) as usize] as char);
        result.push(hex_chars[(b & 0x0f) as usize] as char);
    }
    result
}

/// Hex-decode a hex string back to bytes. Returns None on odd length or invalid chars.
pub fn hex_decode(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    let mut bytes = Vec::with_capacity(hex.len() / 2);
    for chunk in hex.as_bytes().chunks(2) {
        let hi = hex_val(chunk[0])?;
        let lo = hex_val(chunk[1])?;
        bytes.push((hi << 4) | lo);
    }
    Some(bytes)
}

/// Best-effort machine identity for deterministic key derivation.
fn machine_identity() -> String {
    // Try /etc/machine-id (Linux/systemd)
    if let Ok(id) = std::fs::read_to_string("/etc/machine-id") {
        return id.trim().to_string();
    }
    // Try /var/lib/dbus/machine-id (alternative Linux)
    if let Ok(id) = std::fs::read_to_string("/var/lib/dbus/machine-id") {
        return id.trim().to_string();
    }
    // Fallback: hostname
    if let Ok(host) = std::process::Command::new("hostname").output() {
        return String::from_utf8_lossy(&host.stdout).trim().to_string();
    }
    "astra-default-key-00000000".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_encrypt_decrypt() {
        let mut key = [0u8; 32];
        SystemRandom::new().fill(&mut key).unwrap();
        let crypto = JournalCrypto::new(&key);

        let plaintext = b"Hello, encrypted journal!";
        let encrypted = crypto.encrypt(plaintext);
        let decrypted = crypto.decrypt(&encrypted).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn different_nonces_produce_different_ciphertexts() {
        let mut key = [0u8; 32];
        SystemRandom::new().fill(&mut key).unwrap();
        let crypto = JournalCrypto::new(&key);

        let pt = b"same plaintext";
        let ct1 = crypto.encrypt(pt);
        let ct2 = crypto.encrypt(pt);
        assert_ne!(ct1, ct2);
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let mut key = [0u8; 32];
        SystemRandom::new().fill(&mut key).unwrap();
        let crypto = JournalCrypto::new(&key);

        let mut encrypted = crypto.encrypt(b"tamper test");
        let enc_len = encrypted.len();
        if enc_len > 15 {
            encrypted[enc_len - 1] ^= 1; // flip last byte
        }
        assert!(crypto.decrypt(&encrypted).is_none());
    }

    #[test]
    fn too_short_input_returns_none() {
        let mut key = [0u8; 32];
        SystemRandom::new().fill(&mut key).unwrap();
        let crypto = JournalCrypto::new(&key);
        assert!(crypto.decrypt(b"short").is_none());
    }

    #[test]
    fn from_env_key_hex() {
        unsafe {
            std::env::set_var(
                "ASTRA_JOURNAL_KEY",
                "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
            );
        }
        let crypto = JournalCrypto::from_env_or_machine();
        let pt = b"env key test";
        let ct = crypto.encrypt(pt);
        assert_eq!(crypto.decrypt(&ct).unwrap(), pt);
        unsafe {
            std::env::remove_var("ASTRA_JOURNAL_KEY");
        }
    }
}
