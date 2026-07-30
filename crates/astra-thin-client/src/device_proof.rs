use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const ASTRA_DEVICE_ID_HEADER: &str = "x-astra-device-id";
pub const ASTRA_DEVICE_FINGERPRINT_HEADER: &str = "x-astra-device-fingerprint";
pub const ASTRA_DEVICE_CHALLENGE_ID_HEADER: &str = "x-astra-device-challenge-id";
pub const ASTRA_DEVICE_PROOF_HEADER: &str = "x-astra-device-proof";

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeviceProofPurpose {
    Hydrate,
    Trust,
}

impl DeviceProofPurpose {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hydrate => "hydrate",
            Self::Trust => "trust",
        }
    }
}

fn append_device_proof_field(message: &mut String, value: &str) {
    message.push_str(&value.len().to_string());
    message.push(':');
    message.push_str(value);
    message.push('\n');
}

/// Build the protocol-canonical HMAC message from the server-stored challenge
/// digest. Both proof generation and verification use this function so the
/// domain prefix, authority-field order, and framing cannot drift.
#[allow(clippy::too_many_arguments)]
pub fn canonical_device_proof_message(
    purpose: DeviceProofPurpose,
    user_id: &str,
    session_id: &str,
    device_id: &str,
    device_fingerprint: &str,
    challenge_id: &str,
    challenge_digest: &str,
) -> Vec<u8> {
    let mut message = String::from("astra-device-proof-v1\n");
    for value in [
        purpose.as_str(),
        user_id,
        session_id,
        device_id,
        device_fingerprint,
        challenge_id,
        challenge_digest,
    ] {
        append_device_proof_field(&mut message, value);
    }
    message.into_bytes()
}

/// Produce the one-time proof for a Server-issued device challenge.
///
/// The device key and raw challenge are never placed in the result. Every
/// authority dimension is length-prefixed and authenticated, so identifiers
/// containing punctuation cannot create an ambiguous encoding.
#[allow(clippy::too_many_arguments)]
pub fn device_challenge_proof(
    device_key: &str,
    purpose: DeviceProofPurpose,
    user_id: &str,
    session_id: &str,
    device_id: &str,
    device_fingerprint: &str,
    challenge_id: &str,
    challenge: &str,
) -> String {
    let device_key_hash = format!("{:x}", Sha256::digest(device_key.as_bytes()));
    let challenge_digest = format!("{:x}", Sha256::digest(challenge.as_bytes()));
    let message = canonical_device_proof_message(
        purpose,
        user_id,
        session_id,
        device_id,
        device_fingerprint,
        challenge_id,
        challenge_digest.as_str(),
    );
    let mut mac = Hmac::<Sha256>::new_from_slice(device_key_hash.as_bytes())
        .expect("SHA-256 hex is a valid HMAC key");
    mac.update(&message);
    URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_proof_matches_protocol_golden_vector() {
        assert_eq!(
            device_challenge_proof(
                "dk_test_secret",
                DeviceProofPurpose::Hydrate,
                "user-17",
                "session:a",
                "laptop-2",
                "sha256:abcdef",
                "challenge-9",
                "dc_test_nonce",
            ),
            "eynZMCSGx0fBYdE0b-maiJBHVaZgLBrmOOYV6j5CXYo"
        );
    }
}
