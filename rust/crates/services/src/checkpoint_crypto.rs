//! Shared at-rest encryption for local checkpoint/session artifacts.
//!
//! The active key comes from `ASTRA_JOURNAL_KEY` (hex) or a locally persisted
//! random secret under the session-artifact root.

use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::session_artifact_store::SessionArtifactStore;

const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;
const JOURNAL_KEY_ENV: &str = "ASTRA_JOURNAL_KEY";
const JOURNAL_KEY_FILE: &str = ".journal.key";

/// Encryption context for local checkpoint/session artifacts.
pub struct JournalCrypto {
    key: LessSafeKey,
}

impl JournalCrypto {
    /// Create from a raw 32-byte key.
    pub fn new(key_bytes: &[u8; KEY_LEN]) -> Self {
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

        let mut result = nonce_bytes.to_vec();
        result.extend_from_slice(&in_out);
        result
    }

    /// Decrypt `nonce || ciphertext` and return plaintext.
    ///
    /// Returns `None` on authentication failure.
    pub fn decrypt(&self, encrypted: &[u8]) -> Option<Vec<u8>> {
        if encrypted.len() < NONCE_LEN + 16 {
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

/// Decrypt a hex-encoded encrypted text payload.
pub fn decrypt_text(content: &str) -> Option<String> {
    let bytes = hex_decode(content.trim())?;
    let decrypted = journal_crypto().decrypt(&bytes)?;
    String::from_utf8(decrypted).ok()
}

/// Encrypt a UTF-8 text payload to hex-encoded ciphertext.
pub fn encrypt_text(content: &str) -> String {
    let encrypted = journal_crypto().encrypt(content.as_bytes());
    hex_encode(&encrypted)
}

fn journal_crypto() -> &'static JournalCrypto {
    use std::sync::OnceLock;

    static CRYPTO: OnceLock<JournalCrypto> = OnceLock::new();
    CRYPTO.get_or_init(JournalCrypto::from_env_or_local_key)
}

fn parse_hex_32(hex: &str) -> Option<[u8; KEY_LEN]> {
    if hex.len() != KEY_LEN * 2 {
        return None;
    }
    let mut bytes = [0u8; KEY_LEN];
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
    crate::local_session_artifact_store()
        .sessions_root()
        .join(JOURNAL_KEY_FILE)
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
        let mut key = [0u8; KEY_LEN];
        SystemRandom::new().fill(&mut key).unwrap();
        let crypto = JournalCrypto::new(&key);

        let ciphertext = crypto.encrypt(b"hello checkpoint");
        assert_ne!(ciphertext, b"hello checkpoint");
        assert_eq!(crypto.decrypt(&ciphertext).unwrap(), b"hello checkpoint");
    }

    #[test]
    fn decrypt_rejects_tampering() {
        let mut key = [0u8; KEY_LEN];
        SystemRandom::new().fill(&mut key).unwrap();
        let crypto = JournalCrypto::new(&key);

        let mut ciphertext = crypto.encrypt(b"secret");
        let last = ciphertext.len() - 1;
        ciphertext[last] ^= 0x01;
        assert!(crypto.decrypt(&ciphertext).is_none());
    }

    #[test]
    fn hex_roundtrip() {
        let bytes = b"\x00\x01\xfe\xff";
        let encoded = hex_encode(bytes);
        assert_eq!(encoded, "0001feff");
        assert_eq!(hex_decode(&encoded).unwrap(), bytes);
    }

    #[test]
    fn decrypt_text_rejects_plaintext_json() {
        assert!(decrypt_text(r#"{"snapshots":[]}"#).is_none());
    }
}
