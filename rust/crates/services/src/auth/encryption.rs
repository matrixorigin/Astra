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
        let key_source = env::var("TOKEN_ENCRYPTION_KEY")
            .map_err(|_| "TOKEN_ENCRYPTION_KEY environment variable must be set.".to_string())?;
        Self::new(&key_source)
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
