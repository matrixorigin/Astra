use base64::{Engine as _, engine::general_purpose::URL_SAFE};
use fernet::Fernet;
use sha2::{Digest, Sha256};
use std::env;

pub(super) fn sha256_hex(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

#[derive(Clone)]
pub struct FernetTokenEncryptor {
    fernet: Fernet,
}

impl FernetTokenEncryptor {
    pub fn from_env() -> Result<Self, String> {
        Self::from_key(None)
    }

    /// Create from an explicit key, or fall back to `ASTRA_TOKEN_ENCRYPTION_KEY` when `None`.
    pub fn from_key(key: Option<&str>) -> Result<Self, String> {
        match key {
            Some(k) => Self::new(k),
            None => {
                let k = env::var("ASTRA_TOKEN_ENCRYPTION_KEY").map_err(|_| {
                    "ASTRA_TOKEN_ENCRYPTION_KEY environment variable must be set.".to_string()
                })?;
                Self::new(&k)
            }
        }
    }

    pub fn new(secret_key: &str) -> Result<Self, String> {
        let derived_key = derive_fernet_key(secret_key);
        let fernet = Fernet::new(&derived_key)
            .ok_or_else(|| "failed to initialize fernet token encryptor".to_string())?;
        Ok(Self { fernet })
    }

    pub fn encrypt(&self, plaintext: &str) -> Result<String, String> {
        Ok(self.fernet.encrypt(plaintext.as_bytes()))
    }

    pub fn decrypt(&self, ciphertext: &str) -> Result<String, String> {
        let bytes = self
            .fernet
            .decrypt(ciphertext)
            .map_err(|e| format!("decryption failed: {}", e))?;
        String::from_utf8(bytes).map_err(|e| format!("decrypted data is not valid UTF-8: {}", e))
    }
}

fn derive_fernet_key(secret_key: &str) -> String {
    let digest = Sha256::digest(secret_key.as_bytes());
    URL_SAFE.encode(digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_encryptor(key: &str) -> FernetTokenEncryptor {
        FernetTokenEncryptor::new(key).expect("failed to create encryptor")
    }

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let enc = make_encryptor("my-secret-key");
        let plaintext = "hello world";
        let ciphertext = enc.encrypt(plaintext).unwrap();
        assert_ne!(ciphertext, plaintext);
        assert_eq!(enc.decrypt(&ciphertext).unwrap(), plaintext);
    }

    #[test]
    fn decrypt_with_wrong_key_fails() {
        let enc1 = make_encryptor("key-one");
        let enc2 = make_encryptor("key-two");
        let ciphertext = enc1.encrypt("secret data").unwrap();
        let result = enc2.decrypt(&ciphertext);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("decryption failed"));
    }

    #[test]
    fn encrypt_decrypt_empty_plaintext() {
        let enc = make_encryptor("some-key");
        let ciphertext = enc.encrypt("").unwrap();
        assert_eq!(enc.decrypt(&ciphertext).unwrap(), "");
    }

    #[test]
    fn encrypt_decrypt_unicode_and_cjk() {
        let enc = make_encryptor("unicode-key");
        for text in &[
            "你好世界",
            "日本語テスト",
            "한국어",
            "émojis: 🚀🔥✅",
            "café résumé",
        ] {
            let ciphertext = enc.encrypt(text).unwrap();
            assert_eq!(enc.decrypt(&ciphertext).unwrap(), *text);
        }
    }

    #[test]
    fn decrypt_invalid_ciphertext_fails() {
        let enc = make_encryptor("any-key");
        let result = enc.decrypt("not-a-valid-fernet-token");
        assert!(result.is_err());
    }

    #[test]
    fn sha256_hex_produces_correct_digest() {
        // Known SHA-256 of "hello"
        let digest = sha256_hex("hello");
        assert_eq!(
            digest,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert_eq!(digest.len(), 64);
    }

    #[test]
    fn derive_fernet_key_is_deterministic() {
        let key1 = derive_fernet_key("test-secret");
        let key2 = derive_fernet_key("test-secret");
        assert_eq!(key1, key2);
    }

    #[test]
    fn derive_fernet_key_differs_for_different_inputs() {
        assert_ne!(derive_fernet_key("key-a"), derive_fernet_key("key-b"));
    }

    // -- from_key --

    #[test]
    fn from_key_some_uses_provided_key() {
        let enc = FernetTokenEncryptor::from_key(Some("my-explicit-key")).unwrap();
        let expected = FernetTokenEncryptor::new("my-explicit-key").unwrap();
        let ct = enc.encrypt("hello").unwrap();
        assert_eq!(expected.decrypt(&ct).unwrap(), "hello");
    }

    #[test]
    #[serial_test::serial]
    fn from_key_some_ignores_env_var() {
        // Even if ASTRA_TOKEN_ENCRYPTION_KEY is absent, Some(key) must succeed.
        let prev = std::env::var("ASTRA_TOKEN_ENCRYPTION_KEY").ok();
        unsafe { std::env::remove_var("ASTRA_TOKEN_ENCRYPTION_KEY") };
        let result = FernetTokenEncryptor::from_key(Some("standalone-key"));
        // restore
        if let Some(v) = prev {
            unsafe { std::env::set_var("ASTRA_TOKEN_ENCRYPTION_KEY", v) };
        }
        assert!(result.is_ok(), "from_key(Some) must not need env var");
    }

    #[test]
    #[serial_test::serial]
    fn from_key_none_falls_back_to_env() {
        let prev = std::env::var("ASTRA_TOKEN_ENCRYPTION_KEY").ok();
        unsafe { std::env::set_var("ASTRA_TOKEN_ENCRYPTION_KEY", "env-key") };
        let enc = FernetTokenEncryptor::from_key(None);
        // restore
        match prev {
            Some(v) => unsafe { std::env::set_var("ASTRA_TOKEN_ENCRYPTION_KEY", v) },
            None => unsafe { std::env::remove_var("ASTRA_TOKEN_ENCRYPTION_KEY") },
        }
        let enc = enc.unwrap();
        let expected = FernetTokenEncryptor::new("env-key").unwrap();
        let ct = enc.encrypt("world").unwrap();
        assert_eq!(expected.decrypt(&ct).unwrap(), "world");
    }

    #[test]
    #[serial_test::serial]
    fn from_key_none_without_env_fails() {
        let prev = std::env::var("ASTRA_TOKEN_ENCRYPTION_KEY").ok();
        unsafe { std::env::remove_var("ASTRA_TOKEN_ENCRYPTION_KEY") };
        let result = FernetTokenEncryptor::from_key(None);
        // restore
        if let Some(v) = prev {
            unsafe { std::env::set_var("ASTRA_TOKEN_ENCRYPTION_KEY", v) };
        }
        assert!(result.is_err());
    }

    #[test]
    fn same_plaintext_produces_different_ciphertexts() {
        let enc = make_encryptor("nonce-key");
        let c1 = enc.encrypt("same").unwrap();
        let c2 = enc.encrypt("same").unwrap();
        // Fernet includes a random IV, so ciphertexts should differ
        assert_ne!(c1, c2);
        assert_eq!(enc.decrypt(&c1).unwrap(), "same");
        assert_eq!(enc.decrypt(&c2).unwrap(), "same");
    }

    #[test]
    fn non_utf8_decrypt_fails() {
        // Manually encrypt raw non-UTF-8 bytes via the underlying fernet,
        // then verify our decrypt wrapper rejects them.
        let derived = derive_fernet_key("raw-key");
        let fernet = Fernet::new(&derived).unwrap();
        let non_utf8: &[u8] = &[0xFF, 0xFE, 0x80];
        let ciphertext = fernet.encrypt(non_utf8);
        let enc = make_encryptor("raw-key");
        let result = enc.decrypt(&ciphertext);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not valid UTF-8"));
    }
}
