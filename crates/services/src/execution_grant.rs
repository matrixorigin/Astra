use std::time::Duration;

use astra_turn_types::{
    ConversationWriterLeaseV1, EXECUTION_GRANT_SCHEMA_VERSION, ExecutionGrantClaimsV1,
    SignedExecutionGrantV1,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use thiserror::Error;
use uuid::Uuid;

const MIN_SIGNING_KEY_BYTES: usize = 32;
const MAX_EXECUTION_GRANT_TTL: Duration = Duration::from_secs(5 * 60);
const SIGNATURE_DOMAIN: &[u8] = b"astra.execution-grant.v1\0";

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ExecutionGrantError {
    #[error("execution grant signing key must contain at least 32 bytes")]
    WeakSigningKey,
    #[error("execution grant TTL must be between 1 ms and 5 minutes")]
    InvalidTtl,
    #[error("execution grant timestamp overflow")]
    TimestampOverflow,
    #[error("execution grant encoding is invalid")]
    InvalidEncoding,
    #[error("execution grant signature is invalid")]
    InvalidSignature,
    #[error("execution grant expired")]
    Expired,
    #[error("execution grant does not match current authority")]
    Fenced,
}

#[derive(Clone)]
pub struct ExecutionGrantSigner {
    signing_key: std::sync::Arc<[u8]>,
}

impl ExecutionGrantSigner {
    pub fn new(signing_key: impl AsRef<[u8]>) -> Result<Self, ExecutionGrantError> {
        let signing_key = signing_key.as_ref();
        if signing_key.len() < MIN_SIGNING_KEY_BYTES {
            return Err(ExecutionGrantError::WeakSigningKey);
        }
        Ok(Self {
            signing_key: std::sync::Arc::from(signing_key),
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn issue(
        &self,
        lease: &ConversationWriterLeaseV1,
        run_id: impl Into<String>,
        run_generation: u64,
        provider_binding_id: Option<String>,
        provider_generation: u64,
        now_unix_ms: i64,
        ttl: Duration,
    ) -> Result<SignedExecutionGrantV1, ExecutionGrantError> {
        lease
            .key
            .validate()
            .map_err(|_| ExecutionGrantError::InvalidEncoding)?;
        lease
            .actor
            .validate_for(&lease.key)
            .map_err(|_| ExecutionGrantError::InvalidEncoding)?;
        if lease.schema_version != astra_turn_types::SESSION_COORDINATION_SCHEMA_VERSION
            || lease.lease_id.trim().is_empty()
        {
            return Err(ExecutionGrantError::InvalidEncoding);
        }
        let run_id = run_id.into();
        if invalid_identity(&run_id) || provider_binding_id.as_deref().is_some_and(invalid_identity)
        {
            return Err(ExecutionGrantError::InvalidEncoding);
        }
        if ttl.is_zero() || ttl > MAX_EXECUTION_GRANT_TTL {
            return Err(ExecutionGrantError::InvalidTtl);
        }
        let ttl_ms =
            i64::try_from(ttl.as_millis()).map_err(|_| ExecutionGrantError::TimestampOverflow)?;
        let expires_at_unix_ms = now_unix_ms
            .checked_add(ttl_ms)
            .ok_or(ExecutionGrantError::TimestampOverflow)?
            .min(lease.expires_at_unix_ms);
        if expires_at_unix_ms <= now_unix_ms {
            return Err(ExecutionGrantError::Expired);
        }
        let claims = ExecutionGrantClaimsV1 {
            schema_version: EXECUTION_GRANT_SCHEMA_VERSION,
            key: lease.key.clone(),
            actor_id: lease.actor.actor_id.clone(),
            lease_id: lease.lease_id.clone(),
            writer_epoch: lease.writer_epoch,
            authority_epochs: lease.actor.authority_epochs,
            run_id,
            run_generation,
            provider_binding_id,
            provider_generation,
            issued_at_unix_ms: now_unix_ms,
            expires_at_unix_ms,
            nonce: Uuid::new_v4().to_string(),
        };
        let signature = self.sign(&claims)?;
        Ok(SignedExecutionGrantV1 {
            schema_version: EXECUTION_GRANT_SCHEMA_VERSION,
            claims,
            signature,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn verify<'a>(
        &self,
        grant: &'a SignedExecutionGrantV1,
        active_lease: &ConversationWriterLeaseV1,
        expected_run_id: &str,
        expected_run_generation: u64,
        expected_provider_binding_id: Option<&str>,
        expected_provider_generation: u64,
        now_unix_ms: i64,
    ) -> Result<&'a ExecutionGrantClaimsV1, ExecutionGrantError> {
        if grant.schema_version != EXECUTION_GRANT_SCHEMA_VERSION
            || grant.claims.schema_version != EXECUTION_GRANT_SCHEMA_VERSION
            || invalid_identity(&grant.claims.actor_id)
            || invalid_identity(&grant.claims.lease_id)
            || invalid_identity(&grant.claims.run_id)
            || invalid_identity(&grant.claims.nonce)
            || grant
                .claims
                .provider_binding_id
                .as_deref()
                .is_some_and(invalid_identity)
            || grant.claims.issued_at_unix_ms > now_unix_ms
            || grant.claims.expires_at_unix_ms <= grant.claims.issued_at_unix_ms
        {
            return Err(ExecutionGrantError::InvalidEncoding);
        }
        let signature = URL_SAFE_NO_PAD
            .decode(&grant.signature)
            .map_err(|_| ExecutionGrantError::InvalidEncoding)?;
        let mut mac = self.mac()?;
        mac.update(SIGNATURE_DOMAIN);
        mac.update(
            &serde_json::to_vec(&grant.claims).map_err(|_| ExecutionGrantError::InvalidEncoding)?,
        );
        mac.verify_slice(&signature)
            .map_err(|_| ExecutionGrantError::InvalidSignature)?;
        if grant.claims.expires_at_unix_ms <= now_unix_ms {
            return Err(ExecutionGrantError::Expired);
        }
        if grant.claims.key != active_lease.key
            || grant.claims.lease_id != active_lease.lease_id
            || grant.claims.writer_epoch != active_lease.writer_epoch
            || grant.claims.actor_id != active_lease.actor.actor_id
            || grant.claims.authority_epochs != active_lease.actor.authority_epochs
            || active_lease.expires_at_unix_ms <= now_unix_ms
            || grant.claims.run_id != expected_run_id
            || grant.claims.run_generation != expected_run_generation
            || grant.claims.provider_binding_id.as_deref() != expected_provider_binding_id
            || grant.claims.provider_generation != expected_provider_generation
        {
            return Err(ExecutionGrantError::Fenced);
        }
        Ok(&grant.claims)
    }

    fn sign(&self, claims: &ExecutionGrantClaimsV1) -> Result<String, ExecutionGrantError> {
        let mut mac = self.mac()?;
        mac.update(SIGNATURE_DOMAIN);
        mac.update(&serde_json::to_vec(claims).map_err(|_| ExecutionGrantError::InvalidEncoding)?);
        Ok(URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes()))
    }

    fn mac(&self) -> Result<Hmac<Sha256>, ExecutionGrantError> {
        Hmac::<Sha256>::new_from_slice(&self.signing_key)
            .map_err(|_| ExecutionGrantError::InvalidEncoding)
    }
}

fn invalid_identity(value: &str) -> bool {
    value.is_empty() || value.len() > 512 || value.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use astra_turn_types::{
        ActorContextV1, ActorKindV1, AuthorityEpochsV1, ConversationWriterLeaseV1,
        SESSION_COORDINATION_SCHEMA_VERSION, SessionKeyV1, SessionSurfaceV1,
    };

    use super::*;

    fn lease(owner: &str, epoch: u64) -> ConversationWriterLeaseV1 {
        let key = SessionKeyV1::owner_session("test", owner, "session", "main");
        ConversationWriterLeaseV1 {
            schema_version: SESSION_COORDINATION_SCHEMA_VERSION,
            key,
            lease_id: "lease-1".into(),
            writer_epoch: epoch,
            actor: ActorContextV1::owner_user(
                owner,
                "actor-1",
                ActorKindV1::Server,
                SessionSurfaceV1::Server,
                None,
                AuthorityEpochsV1 {
                    authorization_epoch: 3,
                    device_trust_epoch: 4,
                    permission_epoch: 5,
                },
            ),
            expected_cursor: None,
            acquired_at_unix_ms: 1_000,
            expires_at_unix_ms: 60_000,
            idempotency_key: "acquire".into(),
        }
    }

    #[test]
    fn grant_binds_owner_and_all_mutable_generations() {
        let signer = ExecutionGrantSigner::new([7_u8; 32]).unwrap();
        let lease = lease("owner-a", 9);
        let grant = signer
            .issue(
                &lease,
                "run-1",
                11,
                Some("provider-1".into()),
                13,
                2_000,
                Duration::from_secs(30),
            )
            .unwrap();

        signer
            .verify(&grant, &lease, "run-1", 11, Some("provider-1"), 13, 2_001)
            .unwrap();
        assert_eq!(
            signer.verify(
                &grant,
                &ConversationWriterLeaseV1 {
                    key: SessionKeyV1::owner_session("test", "owner-b", "session", "main",),
                    ..lease.clone()
                },
                "run-1",
                11,
                Some("provider-1"),
                13,
                2_001,
            ),
            Err(ExecutionGrantError::Fenced)
        );
        assert_eq!(
            signer.verify(&grant, &lease, "run-1", 12, Some("provider-1"), 13, 2_001,),
            Err(ExecutionGrantError::Fenced)
        );
        assert_eq!(
            signer.verify(&grant, &lease, "run-1", 11, Some("provider-2"), 13, 2_001,),
            Err(ExecutionGrantError::Fenced)
        );
    }

    #[test]
    fn tampering_and_expiry_fail_closed() {
        let signer = ExecutionGrantSigner::new([8_u8; 32]).unwrap();
        let lease = lease("owner-a", 1);
        let mut grant = signer
            .issue(&lease, "run-1", 1, None, 1, 2_000, Duration::from_secs(1))
            .unwrap();
        grant.claims.writer_epoch += 1;
        assert_eq!(
            signer.verify(&grant, &lease, "run-1", 1, None, 1, 2_001,),
            Err(ExecutionGrantError::InvalidSignature)
        );

        let grant = signer
            .issue(&lease, "run-1", 1, None, 1, 2_000, Duration::from_secs(1))
            .unwrap();
        assert_eq!(
            signer.verify(&grant, &lease, "run-1", 1, None, 1, 3_000,),
            Err(ExecutionGrantError::Expired)
        );
    }

    #[test]
    fn transfer_fences_a_previously_valid_grant() {
        let signer = ExecutionGrantSigner::new([9_u8; 32]).unwrap();
        let old_lease = lease("owner-a", 7);
        let grant = signer
            .issue(
                &old_lease,
                "run-1",
                1,
                None,
                1,
                2_000,
                Duration::from_secs(30),
            )
            .unwrap();
        let mut transferred = old_lease.clone();
        transferred.lease_id = "lease-2".into();
        transferred.writer_epoch += 1;
        transferred.actor.actor_id = "actor-2".into();

        assert_eq!(
            signer.verify(&grant, &transferred, "run-1", 1, None, 1, 2_001),
            Err(ExecutionGrantError::Fenced)
        );
    }
}
