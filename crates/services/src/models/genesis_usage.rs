//! Server-held provenance for one admitted Genesis execution. This capability
//! never grants model access or changes settlement; ordinary PAT auth still applies.
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use reqwest::Url;
use reqwest::header::{HeaderMap, HeaderValue};
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub const GENESIS_USAGE_TIMESTAMP: &str = "x-genesis-usage-timestamp";
pub const GENESIS_USAGE_EXECUTION: &str = "x-genesis-usage-execution";
pub const GENESIS_USAGE_SIGNATURE: &str = "x-genesis-usage-signature";
const INVALID: &str = "Genesis usage attestation does not match the admitted execution";

/// An opaque, non-serializable capability minted only by Genesis admission.
/// Clones retain the same admission identity, not a process-wide source label.
#[derive(Clone, PartialEq, Eq)]
pub struct GenesisUsageAttestation {
    key: Arc<str>,
    base: Url,
    model: String,
    credential_hash: String,
    execution_id: Uuid,
}

impl std::fmt::Debug for GenesisUsageAttestation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GenesisUsageAttestation")
            .finish_non_exhaustive()
    }
}

impl GenesisUsageAttestation {
    pub(super) fn new(
        key: &str,
        base_url: &str,
        model: &str,
        credential: &str,
    ) -> Result<Self, &'static str> {
        let base = Url::parse(base_url).map_err(|_| INVALID)?;
        if key.len() < 32
            || key.trim() != key
            || model.is_empty()
            || model.contains(['\r', '\n'])
            || credential.is_empty()
            || base.query().is_some()
            || base.fragment().is_some()
            || !base.username().is_empty()
            || base.password().is_some()
        {
            return Err(INVALID);
        }
        Ok(Self {
            key: key.into(),
            base,
            model: model.into(),
            credential_hash: format!("{:x}", Sha256::digest(credential.as_bytes())),
            execution_id: Uuid::new_v4(),
        })
    }

    /// Call immediately before each HTTP attempt, after any awaited admission.
    /// Only digests enter the canonical string; no key or plaintext credential
    /// is sent. The body hash is the transport's exact serialized wire hash.
    pub fn headers_for_request(
        &self,
        method: &str,
        url: &str,
        model: &str,
        credential: &str,
        body_hash: &str,
    ) -> Result<HeaderMap, &'static str> {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| INVALID)?
            .as_secs();
        self.headers_at(method, url, model, credential, body_hash, timestamp)
    }

    fn headers_at(
        &self,
        method: &str,
        url: &str,
        model: &str,
        credential: &str,
        body_hash: &str,
        timestamp: u64,
    ) -> Result<HeaderMap, &'static str> {
        let url = Url::parse(url).map_err(|_| INVALID)?;
        let base_path = self.base.path().trim_end_matches('/');
        if method != "POST"
            || url.origin() != self.base.origin()
            || url.query().is_some()
            || url.fragment().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
            || ![
                format!("{base_path}/chat/completions"),
                format!("{base_path}/responses"),
            ]
            .contains(&url.path().to_owned())
            || model != self.model
            || format!("{:x}", Sha256::digest(credential.as_bytes())) != self.credential_hash
            || body_hash.len() != 64
            || !body_hash
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(INVALID);
        }
        let timestamp = timestamp.to_string();
        let execution = self.execution_id.to_string();
        let canonical = format!(
            "astra-runtime-usage-v1\n{timestamp}\n{execution}\n{method}\n{}\n{model}\n{}\n{body_hash}",
            url.path(),
            self.credential_hash
        );
        let mut mac = Hmac::<Sha256>::new_from_slice(self.key.as_bytes()).map_err(|_| INVALID)?;
        mac.update(canonical.as_bytes());
        let signature = format!("{:x}", mac.finalize().into_bytes());
        let mut headers = HeaderMap::new();
        for (name, value) in [
            (GENESIS_USAGE_TIMESTAMP, timestamp),
            (GENESIS_USAGE_EXECUTION, execution),
            (GENESIS_USAGE_SIGNATURE, signature),
        ] {
            let mut value = HeaderValue::from_str(&value).map_err(|_| INVALID)?;
            value.set_sensitive(true);
            headers.insert(name, value);
        }
        Ok(headers)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "synthetic-attribution-key-32-bytes";
    const BASE: &str = "https://genesis.example.test/v1";
    const CREDENTIAL: &str = "synthetic-account-pat";

    #[test]
    fn shared_rust_go_protocol_vector() {
        let vector: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/genesis_usage_v1.json"))
                .unwrap();
        let text = |key: &str| vector[key].as_str().unwrap();
        let mut capability =
            GenesisUsageAttestation::new(text("key"), BASE, text("model"), text("credential"))
                .unwrap();
        capability.execution_id = text("execution_id").parse().unwrap();
        let body_hash = format!("{:x}", Sha256::digest(text("body").as_bytes()));
        assert_eq!(body_hash, text("body_sha256"));
        let headers = capability
            .headers_at(
                text("method"),
                &format!("https://genesis.example.test{}", text("path")),
                text("model"),
                text("credential"),
                &body_hash,
                vector["timestamp"].as_u64().unwrap(),
            )
            .unwrap();
        assert_eq!(headers[GENESIS_USAGE_SIGNATURE], text("signature"));
    }

    #[test]
    fn capability_binds_the_actual_route_and_does_not_export_secrets() {
        let proof = GenesisUsageAttestation::new(KEY, BASE, "model-1", CREDENTIAL).unwrap();
        let hash = format!("{:x}", Sha256::digest(b"synthetic body"));
        let url = format!("{BASE}/chat/completions");
        let first = proof
            .headers_at("POST", &url, "model-1", CREDENTIAL, &hash, 100)
            .unwrap();
        let next = proof
            .headers_at("POST", &url, "model-1", CREDENTIAL, &hash, 500)
            .unwrap();
        assert_eq!(
            first[GENESIS_USAGE_EXECUTION],
            next[GENESIS_USAGE_EXECUTION]
        );
        assert_ne!(
            first[GENESIS_USAGE_SIGNATURE],
            next[GENESIS_USAGE_SIGNATURE]
        );
        assert!(
            proof
                .headers_at(
                    "POST",
                    &format!("{BASE}/responses"),
                    "model-1",
                    CREDENTIAL,
                    &hash,
                    100
                )
                .is_ok()
        );
        for (method, url, model, credential) in [
            ("GET", url.as_str(), "model-1", CREDENTIAL),
            (
                "POST",
                "https://byok.example.test/v1/chat/completions",
                "model-1",
                CREDENTIAL,
            ),
            (
                "POST",
                "https://genesis.example.test/v1/embeddings",
                "model-1",
                CREDENTIAL,
            ),
            ("POST", url.as_str(), "model-2", CREDENTIAL),
            ("POST", url.as_str(), "model-1", "other-pat"),
        ] {
            assert!(
                proof
                    .headers_at(method, url, model, credential, &hash, 100)
                    .is_err()
            );
        }
        let debug = format!("{proof:?} {first:?}");
        assert!(first[GENESIS_USAGE_SIGNATURE].is_sensitive());
        assert!(!debug.contains(first[GENESIS_USAGE_SIGNATURE].to_str().unwrap()));
        assert!(!debug.contains(KEY));
        assert!(!debug.contains(CREDENTIAL));
        assert!(!debug.contains("genesis.example.test"));
    }
}
