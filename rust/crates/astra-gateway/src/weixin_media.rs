//! WeChat media encryption/decryption (AES-128-ECB with PKCS#7).
//!
//! CDN pipeline: download → decrypt → plaintext (inbound)
//!               plaintext → encrypt → upload → send (outbound)

use aes::cipher::{BlockDecryptMut, BlockEncryptMut, KeyInit};

type Aes128EcbEnc = ecb::Encryptor<aes::Aes128>;
type Aes128EcbDec = ecb::Decryptor<aes::Aes128>;

const BLOCK_SIZE: usize = 16;
const CDN_BASE_URL: &str = "https://novac2c.cdn.weixin.qq.com/c2c";

/// Parse AES key from base64-encoded value.
/// Handles two formats:
/// - 16 raw bytes (direct key)
/// - 32 bytes that are a hex string (decode hex → 16 bytes)
pub fn parse_aes_key(aes_key_b64: &str) -> Result<[u8; 16], String> {
    use base64::Engine;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(aes_key_b64)
        .map_err(|e| format!("base64 decode: {e}"))?;

    if decoded.len() == 16 {
        let mut key = [0u8; 16];
        key.copy_from_slice(&decoded);
        return Ok(key);
    }
    if decoded.len() == 32
        && let Ok(text) = std::str::from_utf8(&decoded)
        && text.chars().all(|c| c.is_ascii_hexdigit())
    {
        let bytes = hex::decode(text).map_err(|e| format!("hex decode: {e}"))?;
        if bytes.len() == 16 {
            let mut key = [0u8; 16];
            key.copy_from_slice(&bytes);
            return Ok(key);
        }
    }
    Err(format!(
        "unexpected aes_key format ({} decoded bytes)",
        decoded.len()
    ))
}

/// Encode AES key for the iLink API: base64(hex_string_of_key_bytes).
pub fn encode_aes_key_for_api(key: &[u8; 16]) -> String {
    use base64::Engine;
    let hex_str = hex::encode(key);
    base64::engine::general_purpose::STANDARD.encode(hex_str.as_bytes())
}

fn pkcs7_pad(data: &[u8]) -> Vec<u8> {
    let pad_len = BLOCK_SIZE - (data.len() % BLOCK_SIZE);
    let mut padded = data.to_vec();
    padded.extend(std::iter::repeat_n(pad_len as u8, pad_len));
    padded
}

fn pkcs7_unpad(data: &[u8]) -> &[u8] {
    if data.is_empty() {
        return data;
    }
    let pad_len = *data.last().unwrap() as usize;
    if (1..=BLOCK_SIZE).contains(&pad_len) && data.len() >= pad_len {
        let pad_start = data.len() - pad_len;
        if data[pad_start..].iter().all(|&b| b == pad_len as u8) {
            return &data[..pad_start];
        }
    }
    data
}

pub fn aes128_ecb_encrypt(plaintext: &[u8], key: &[u8; 16]) -> Vec<u8> {
    let padded = pkcs7_pad(plaintext);
    let mut buf = padded;
    let enc = Aes128EcbEnc::new(key.into());
    for chunk in buf.chunks_mut(BLOCK_SIZE) {
        enc.clone().encrypt_block_mut(chunk.into());
    }
    buf
}

pub fn aes128_ecb_decrypt(ciphertext: &[u8], key: &[u8; 16]) -> Vec<u8> {
    if ciphertext.is_empty() || !ciphertext.len().is_multiple_of(BLOCK_SIZE) {
        return ciphertext.to_vec();
    }
    let mut buf = ciphertext.to_vec();
    let dec = Aes128EcbDec::new(key.into());
    for chunk in buf.chunks_mut(BLOCK_SIZE) {
        dec.clone().decrypt_block_mut(chunk.into());
    }
    pkcs7_unpad(&buf).to_vec()
}

pub fn aes_padded_size(raw_size: usize) -> usize {
    (raw_size + 1).div_ceil(16) * 16
}

/// Build CDN download URL.
pub fn cdn_download_url(encrypted_query_param: &str) -> String {
    let encoded = urlencoding::encode(encrypted_query_param);
    format!("{CDN_BASE_URL}/download?encrypted_query_param={encoded}")
}

/// Build CDN upload URL.
pub fn cdn_upload_url(upload_param: &str, filekey: &str) -> String {
    let ep = urlencoding::encode(upload_param);
    let fk = urlencoding::encode(filekey);
    format!("{CDN_BASE_URL}/upload?encrypted_query_param={ep}&filekey={fk}")
}

/// Generate a random 32-char hex filekey.
pub fn random_filekey() -> String {
    use rand::Rng;
    let mut rng = rand::rng();
    let bytes: [u8; 16] = rng.random();
    hex::encode(bytes)
}

/// Generate a random AES-128 key.
pub fn random_aes_key() -> [u8; 16] {
    use rand::Rng;
    rand::rng().random()
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    #[test]
    fn pkcs7_pad_exact_block() {
        let data = vec![0u8; 16];
        let padded = pkcs7_pad(&data);
        assert_eq!(padded.len(), 32); // adds full block of padding
        assert_eq!(padded[16..], vec![16u8; 16]);
    }

    #[test]
    fn pkcs7_pad_partial() {
        let data = vec![0u8; 10];
        let padded = pkcs7_pad(&data);
        assert_eq!(padded.len(), 16);
        assert_eq!(padded[10..], vec![6u8; 6]);
    }

    #[test]
    fn pkcs7_unpad_valid() {
        let mut data = vec![1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10];
        data.extend(vec![6u8; 6]);
        let unpadded = pkcs7_unpad(&data);
        assert_eq!(unpadded, &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
    }

    #[test]
    fn pkcs7_unpad_invalid_preserved() {
        let data = vec![1, 2, 3, 99]; // invalid padding
        let unpadded = pkcs7_unpad(&data);
        assert_eq!(unpadded, &data);
    }

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let key = [0x42u8; 16];
        let plaintext = b"hello world, this is a test message for AES encryption";
        let ciphertext = aes128_ecb_encrypt(plaintext, &key);
        assert_ne!(ciphertext, plaintext);
        assert_eq!(ciphertext.len() % BLOCK_SIZE, 0);
        let decrypted = aes128_ecb_decrypt(&ciphertext, &key);
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn encrypt_empty() {
        let key = [0u8; 16];
        let ct = aes128_ecb_encrypt(b"", &key);
        assert_eq!(ct.len(), 16); // one block of padding
        let pt = aes128_ecb_decrypt(&ct, &key);
        assert!(pt.is_empty());
    }

    #[test]
    fn encrypt_exact_block_size() {
        let key = [0xABu8; 16];
        let plaintext = [0x11u8; 16];
        let ct = aes128_ecb_encrypt(&plaintext, &key);
        assert_eq!(ct.len(), 32); // 16 + 16 padding
        let pt = aes128_ecb_decrypt(&ct, &key);
        assert_eq!(pt, plaintext);
    }

    #[test]
    fn parse_key_raw_16_bytes() {
        let key = [0x42u8; 16];
        let b64 = base64::engine::general_purpose::STANDARD.encode(key);
        let parsed = parse_aes_key(&b64).unwrap();
        assert_eq!(parsed, key);
    }

    #[test]
    fn parse_key_hex_string_32_bytes() {
        let key = [0x42u8; 16];
        let hex_str = hex::encode(key);
        assert_eq!(hex_str.len(), 32);
        let b64 = base64::engine::general_purpose::STANDARD.encode(hex_str.as_bytes());
        let parsed = parse_aes_key(&b64).unwrap();
        assert_eq!(parsed, key);
    }

    #[test]
    fn parse_key_invalid() {
        let b64 = base64::engine::general_purpose::STANDARD.encode(vec![0u8; 5]);
        assert!(parse_aes_key(&b64).is_err());
    }

    #[test]
    fn encode_key_for_api_format() {
        let key = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        let encoded = encode_aes_key_for_api(&key);
        // Should be base64(hex_string), not base64(raw_bytes)
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&encoded)
            .unwrap();
        let hex_str = String::from_utf8(decoded).unwrap();
        assert_eq!(hex_str, "00112233445566778899aabbccddeeff");
    }

    #[test]
    fn key_roundtrip_api_format() {
        let original = random_aes_key();
        let api_encoded = encode_aes_key_for_api(&original);
        let parsed = parse_aes_key(&api_encoded).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn aes_padded_size_values() {
        assert_eq!(aes_padded_size(0), 16);
        assert_eq!(aes_padded_size(1), 16);
        assert_eq!(aes_padded_size(15), 16);
        assert_eq!(aes_padded_size(16), 32);
        assert_eq!(aes_padded_size(100), 112);
    }

    #[test]
    fn cdn_download_url_encoding() {
        let url = cdn_download_url("a b+c=d");
        assert!(url.starts_with("https://novac2c.cdn.weixin.qq.com/c2c/download?"));
        assert!(url.contains("encrypted_query_param="));
        assert!(!url.contains(' ')); // spaces should be encoded
    }

    #[test]
    fn cdn_upload_url_encoding() {
        let url = cdn_upload_url("param", "key123");
        assert!(url.contains("/upload?"));
        assert!(url.contains("filekey=key123"));
    }

    #[test]
    fn random_filekey_format() {
        let fk = random_filekey();
        assert_eq!(fk.len(), 32);
        assert!(fk.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
