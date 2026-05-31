//! Journal at-rest encryption for local trace/session artifacts.
//!
//! Wraps file I/O with AES-256-GCM encryption via the `ring` crate.
//! The active key comes from `ASTRA_JOURNAL_KEY` (hex) or a locally persisted
//! random secret under the session-artifact root.

use astra_services::SessionArtifactStore;
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

const NONCE_LEN: usize = 12; // AES-256-GCM uses 96-bit (12-byte) nonces
const KEY_LEN: usize = 32;
const JOURNAL_KEY_ENV: &str = "ASTRA_JOURNAL_KEY";
const JOURNAL_KEY_FILE: &str = ".journal.key";

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

    /// Load from `ASTRA_JOURNAL_KEY` (hex) or a locally persisted random key.
    pub fn from_env_or_local_key() -> Self {
        if let Ok(hex) = std::env::var(JOURNAL_KEY_ENV) {
            let trimmed = hex.trim();
            let bytes = parse_hex_32(trimmed)
                .unwrap_or_else(|| panic!("{JOURNAL_KEY_ENV} must be exactly 64 hex characters"));
            return Self::new(&bytes);
        }
        let bytes = load_or_create_local_key_bytes().unwrap_or_else(|err| {
            panic!(
                "failed to load or create journal key {}: {err}",
                local_key_path().display()
            )
        });
        Self::new(&bytes)
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
        let plaintext = self
            .key
            .open_in_place(nonce, Aad::empty(), &mut in_out)
            .ok()?;
        Some(plaintext.to_vec())
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

fn load_or_create_local_key_bytes() -> io::Result<[u8; KEY_LEN]> {
    let path = local_key_path();
    match read_key_file(&path)? {
        Some(bytes) => Ok(bytes),
        None => create_key_file(&path),
    }
}

fn read_key_file(path: &Path) -> io::Result<Option<[u8; KEY_LEN]>> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let trimmed = content.trim();
    let bytes = parse_hex_32(trimmed).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid hex in journal key file {}", path.display()),
        )
    })?;
    Ok(Some(bytes))
}

fn create_key_file(path: &Path) -> io::Result<[u8; KEY_LEN]> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut key = [0u8; KEY_LEN];
    SystemRandom::new()
        .fill(&mut key)
        .map_err(|_| io::Error::other("system random unavailable"))?;

    let encoded = format!("{}\n", hex_encode(&key));
    #[cfg(unix)]
    let file_result = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
    };
    #[cfg(not(unix))]
    let file_result = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path);

    match file_result {
        Ok(mut file) => {
            file.write_all(encoded.as_bytes())?;
            file.sync_all()?;
            sync_parent_dir(path)?;
            Ok(key)
        }
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => read_key_file(path)?
            .ok_or_else(|| io::Error::other("journal key file disappeared after creation race")),
        Err(err) => Err(err),
    }
}

fn local_key_path() -> PathBuf {
    let sessions_root = astra_services::local_session_artifact_store().sessions_root();
    sessions_root.join(JOURNAL_KEY_FILE)
}

fn sync_parent_dir(path: &Path) -> io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    std::fs::File::open(parent)?.sync_all()
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
                JOURNAL_KEY_ENV,
                "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
            );
        }
        let crypto = JournalCrypto::from_env_or_local_key();
        let pt = b"env key test";
        let ct = crypto.encrypt(pt);
        assert_eq!(crypto.decrypt(&ct).unwrap(), pt);
        unsafe {
            std::env::remove_var(JOURNAL_KEY_ENV);
        }
    }

    #[test]
    fn local_key_file_round_trips_and_is_reused() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions_root = tmp.path().join("sessions");
        let _guard = astra_services::session_journal::JournalDirGuard::new(&sessions_root);

        unsafe {
            std::env::remove_var(JOURNAL_KEY_ENV);
        }

        let crypto = JournalCrypto::from_env_or_local_key();
        let encrypted = crypto.encrypt(b"persisted key");
        assert_eq!(crypto.decrypt(&encrypted).unwrap(), b"persisted key");

        let key_path = sessions_root.join(JOURNAL_KEY_FILE);
        assert!(key_path.exists(), "local key file should be created");

        let crypto_reloaded = JournalCrypto::from_env_or_local_key();
        assert_eq!(
            crypto_reloaded.decrypt(&encrypted).unwrap(),
            b"persisted key",
            "reloaded crypto should reuse the same local key"
        );
    }

    #[test]
    fn decrypt_preserves_full_plaintext_length() {
        let mut key = [0u8; KEY_LEN];
        SystemRandom::new().fill(&mut key).unwrap();
        let crypto = JournalCrypto::new(&key);
        let plaintext = b"0123456789abcdef-tail";
        let encrypted = crypto.encrypt(plaintext);
        assert_eq!(crypto.decrypt(&encrypted).unwrap(), plaintext);
    }
}
