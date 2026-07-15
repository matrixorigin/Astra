//! Bounded semantic read-observation cache state machine.
//!
//! This is the executable contract shared by ephemeral and durable adapters.
//! It owns fill fencing and resource limits; it does not resolve provider
//! policy, freshness, permission, or presentation hooks.

use std::collections::BTreeMap;

use astra_turn_types::{
    SemanticReadCacheContractError, SemanticReadCacheKey, SemanticReadObservation,
};
use thiserror::Error;

const MAX_FILL_OWNER_BYTES: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SemanticReadCacheLimits {
    pub max_ready_entries: usize,
    pub max_ready_bytes: usize,
    pub max_in_flight_fills: usize,
}

impl Default for SemanticReadCacheLimits {
    fn default() -> Self {
        Self {
            max_ready_entries: 512,
            max_ready_bytes: 32 * 1024 * 1024,
            max_in_flight_fills: 64,
        }
    }
}

impl SemanticReadCacheLimits {
    pub fn validate(self) -> Result<Self, SemanticReadObservationStoreError> {
        if self.max_ready_entries == 0 {
            return Err(SemanticReadObservationStoreError::InvalidLimit {
                field: "max_ready_entries",
            });
        }
        if self.max_ready_bytes == 0 {
            return Err(SemanticReadObservationStoreError::InvalidLimit {
                field: "max_ready_bytes",
            });
        }
        if self.max_in_flight_fills == 0 {
            return Err(SemanticReadObservationStoreError::InvalidLimit {
                field: "max_in_flight_fills",
            });
        }
        Ok(self)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SemanticReadCacheLookup {
    Hit(Box<SemanticReadObservation>),
    FillClaimed,
    FillInProgress { lease_expires_at_epoch_ms: u64 },
    FillCapacityExceeded,
}

#[derive(Clone, Debug)]
enum CacheEntry {
    Filling {
        key: SemanticReadCacheKey,
        lease: SemanticReadCacheFillLease,
    },
    Ready {
        key: SemanticReadCacheKey,
        observation: Box<SemanticReadObservation>,
        encoded_bytes: usize,
        last_access_sequence: u64,
    },
}

#[derive(Clone, Debug)]
struct SemanticReadCacheFillLease {
    owner_id: String,
    expires_at_epoch_ms: u64,
}

impl SemanticReadCacheFillLease {
    fn new(
        owner_id: &str,
        expires_at_epoch_ms: u64,
        now_epoch_ms: u64,
    ) -> Result<Self, SemanticReadObservationStoreError> {
        if owner_id.trim().is_empty() {
            return Err(SemanticReadObservationStoreError::EmptyFillOwner);
        }
        if owner_id.len() > MAX_FILL_OWNER_BYTES {
            return Err(SemanticReadObservationStoreError::FillOwnerTooLong {
                actual_bytes: owner_id.len(),
                max_bytes: MAX_FILL_OWNER_BYTES,
            });
        }
        if expires_at_epoch_ms <= now_epoch_ms {
            return Err(SemanticReadObservationStoreError::InvalidFillLeaseDeadline);
        }
        Ok(Self {
            owner_id: owner_id.to_string(),
            expires_at_epoch_ms,
        })
    }

    fn is_expired_at(&self, now_epoch_ms: u64) -> bool {
        self.expires_at_epoch_ms <= now_epoch_ms
    }
}

pub struct InMemorySemanticReadObservationStore {
    limits: SemanticReadCacheLimits,
    entries: BTreeMap<String, CacheEntry>,
    ready_bytes: usize,
    access_sequence: u64,
}

impl InMemorySemanticReadObservationStore {
    pub fn new(limits: SemanticReadCacheLimits) -> Result<Self, SemanticReadObservationStoreError> {
        Ok(Self {
            limits: limits.validate()?,
            entries: BTreeMap::new(),
            ready_bytes: 0,
            access_sequence: 0,
        })
    }

    pub fn lookup_or_claim(
        &mut self,
        key: &SemanticReadCacheKey,
        fill_owner: &str,
        lease_expires_at_epoch_ms: u64,
        now_epoch_ms: u64,
    ) -> Result<SemanticReadCacheLookup, SemanticReadObservationStoreError> {
        key.validate()?;
        let proposed_lease =
            SemanticReadCacheFillLease::new(fill_owner, lease_expires_at_epoch_ms, now_epoch_ms)?;
        self.remove_expired_fills(now_epoch_ms);
        let next_access = self.next_access_sequence();
        if let Some(entry) = self.entries.get_mut(&key.key_id) {
            match entry {
                CacheEntry::Ready {
                    key: stored_key,
                    observation,
                    last_access_sequence,
                    ..
                } => {
                    ensure_same_key(stored_key, key)?;
                    *last_access_sequence = next_access;
                    return Ok(SemanticReadCacheLookup::Hit(observation.clone()));
                }
                CacheEntry::Filling {
                    key: stored_key,
                    lease,
                } => {
                    ensure_same_key(stored_key, key)?;
                    return Ok(SemanticReadCacheLookup::FillInProgress {
                        lease_expires_at_epoch_ms: lease.expires_at_epoch_ms,
                    });
                }
            }
        }

        if self.in_flight_fills() >= self.limits.max_in_flight_fills {
            return Ok(SemanticReadCacheLookup::FillCapacityExceeded);
        }
        self.entries.insert(
            key.key_id.clone(),
            CacheEntry::Filling {
                key: key.clone(),
                lease: proposed_lease,
            },
        );
        Ok(SemanticReadCacheLookup::FillClaimed)
    }

    pub fn complete_fill(
        &mut self,
        key: &SemanticReadCacheKey,
        fill_owner: &str,
        now_epoch_ms: u64,
        observation: SemanticReadObservation,
    ) -> Result<(), SemanticReadObservationStoreError> {
        key.validate()?;
        observation.validate()?;
        if observation.key != *key {
            return Err(SemanticReadObservationStoreError::ObservationKeyMismatch);
        }
        let encoded_bytes = observation.encoded_len()?;
        let entry = self
            .entries
            .get(&key.key_id)
            .ok_or(SemanticReadObservationStoreError::MissingFill)?;
        let CacheEntry::Filling {
            key: stored_key,
            lease,
        } = entry
        else {
            return Err(SemanticReadObservationStoreError::FillAlreadyCompleted);
        };
        ensure_same_key(stored_key, key)?;
        if lease.owner_id != fill_owner {
            return Err(SemanticReadObservationStoreError::FillOwnerMismatch);
        }
        if lease.is_expired_at(now_epoch_ms) {
            return Err(SemanticReadObservationStoreError::FillLeaseExpired);
        }
        if encoded_bytes > self.limits.max_ready_bytes {
            self.entries.remove(&key.key_id);
            return Err(
                SemanticReadObservationStoreError::ObservationExceedsStoreCapacity {
                    observation_bytes: encoded_bytes,
                    max_ready_bytes: self.limits.max_ready_bytes,
                },
            );
        }

        while self.ready_entries() >= self.limits.max_ready_entries
            || self.ready_bytes.saturating_add(encoded_bytes) > self.limits.max_ready_bytes
        {
            if !self.evict_least_recently_used_ready() {
                return Err(SemanticReadObservationStoreError::ReadyCapacityInvariant);
            }
        }
        let last_access_sequence = self.next_access_sequence();
        self.entries.insert(
            key.key_id.clone(),
            CacheEntry::Ready {
                key: key.clone(),
                observation: Box::new(observation),
                encoded_bytes,
                last_access_sequence,
            },
        );
        self.ready_bytes = self.ready_bytes.saturating_add(encoded_bytes);
        Ok(())
    }

    pub fn abandon_fill(
        &mut self,
        key: &SemanticReadCacheKey,
        fill_owner: &str,
    ) -> Result<(), SemanticReadObservationStoreError> {
        key.validate()?;
        let entry = self
            .entries
            .get(&key.key_id)
            .ok_or(SemanticReadObservationStoreError::MissingFill)?;
        let CacheEntry::Filling {
            key: stored_key,
            lease,
        } = entry
        else {
            return Err(SemanticReadObservationStoreError::FillAlreadyCompleted);
        };
        ensure_same_key(stored_key, key)?;
        if lease.owner_id != fill_owner {
            return Err(SemanticReadObservationStoreError::FillOwnerMismatch);
        }
        self.entries.remove(&key.key_id);
        Ok(())
    }

    pub fn ready_entries(&self) -> usize {
        self.entries
            .values()
            .filter(|entry| matches!(entry, CacheEntry::Ready { .. }))
            .count()
    }

    pub fn ready_bytes(&self) -> usize {
        self.ready_bytes
    }

    pub fn in_flight_fills(&self) -> usize {
        self.entries
            .values()
            .filter(|entry| matches!(entry, CacheEntry::Filling { .. }))
            .count()
    }

    fn evict_least_recently_used_ready(&mut self) -> bool {
        let Some((key_id, encoded_bytes)) = self
            .entries
            .iter()
            .filter_map(|(key_id, entry)| match entry {
                CacheEntry::Ready {
                    encoded_bytes,
                    last_access_sequence,
                    ..
                } => Some((key_id, *encoded_bytes, *last_access_sequence)),
                CacheEntry::Filling { .. } => None,
            })
            .min_by_key(|(key_id, _, sequence)| (*sequence, key_id.as_str()))
            .map(|(key_id, encoded_bytes, _)| (key_id.clone(), encoded_bytes))
        else {
            return false;
        };
        self.entries.remove(&key_id);
        self.ready_bytes = self.ready_bytes.saturating_sub(encoded_bytes);
        true
    }

    fn remove_expired_fills(&mut self, now_epoch_ms: u64) {
        self.entries.retain(|_, entry| match entry {
            CacheEntry::Filling { lease, .. } => !lease.is_expired_at(now_epoch_ms),
            CacheEntry::Ready { .. } => true,
        });
    }

    fn next_access_sequence(&mut self) -> u64 {
        if let Some(next) = self.access_sequence.checked_add(1) {
            self.access_sequence = next;
            return next;
        }
        let mut ready = self
            .entries
            .iter_mut()
            .filter_map(|(key, entry)| match entry {
                CacheEntry::Ready {
                    last_access_sequence,
                    ..
                } => Some((key.clone(), last_access_sequence)),
                CacheEntry::Filling { .. } => None,
            })
            .collect::<Vec<_>>();
        ready.sort_by_key(|(key, sequence)| (**sequence, key.clone()));
        for (index, (_, sequence)) in ready.into_iter().enumerate() {
            *sequence = index as u64 + 1;
        }
        self.access_sequence = self.ready_entries() as u64 + 1;
        self.access_sequence
    }
}

fn ensure_same_key(
    stored: &SemanticReadCacheKey,
    requested: &SemanticReadCacheKey,
) -> Result<(), SemanticReadObservationStoreError> {
    if stored == requested {
        Ok(())
    } else {
        Err(SemanticReadObservationStoreError::CacheKeyCollision)
    }
}

#[derive(Debug, Error)]
pub enum SemanticReadObservationStoreError {
    #[error("semantic read observation store limit '{field}' must be positive")]
    InvalidLimit { field: &'static str },
    #[error("semantic read cache key ID resolved to different key content")]
    CacheKeyCollision,
    #[error("semantic read observation does not match its claimed cache key")]
    ObservationKeyMismatch,
    #[error("semantic read cache fill does not exist")]
    MissingFill,
    #[error("semantic read cache fill was already completed")]
    FillAlreadyCompleted,
    #[error("semantic read cache fill owner is stale or incorrect")]
    FillOwnerMismatch,
    #[error("semantic read cache fill lease expired before completion")]
    FillLeaseExpired,
    #[error("semantic read cache fill lease deadline must be after the lookup time")]
    InvalidFillLeaseDeadline,
    #[error("semantic read cache fill owner must not be empty")]
    EmptyFillOwner,
    #[error(
        "semantic read cache fill owner is {actual_bytes} bytes but the limit is {max_bytes} bytes"
    )]
    FillOwnerTooLong {
        actual_bytes: usize,
        max_bytes: usize,
    },
    #[error(
        "semantic read observation is {observation_bytes} bytes but this store permits {max_ready_bytes} ready bytes"
    )]
    ObservationExceedsStoreCapacity {
        observation_bytes: usize,
        max_ready_bytes: usize,
    },
    #[error("semantic read cache ready-capacity accounting has no evictable entry")]
    ReadyCapacityInvariant,
    #[error(transparent)]
    Contract(#[from] SemanticReadCacheContractError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_turn_types::{
        DurableToolReference, ProviderBindingRef, ResolvedToolDescriptorRef, SemanticFreshnessFact,
        SemanticFreshnessScope, SemanticReadFreshnessContext, ToolIdentity, ToolInvocationDecision,
        ToolInvocationResultPayload, ToolInvocationTerminalOutcome,
    };
    use serde_json::json;
    use std::collections::BTreeMap;

    fn key(label: &str) -> SemanticReadCacheKey {
        let resource = format!("resource-{label}");
        let freshness = SemanticReadFreshnessContext::new(
            "user:session",
            vec![
                SemanticFreshnessFact::new(SemanticFreshnessScope::Resource, &resource, "rev-1")
                    .unwrap(),
            ],
        )
        .unwrap();
        let decision = ToolInvocationDecision::new(&json!({"policy": label})).unwrap();
        SemanticReadCacheKey::new(
            DurableToolReference::Provider {
                descriptor: ResolvedToolDescriptorRef::new(
                    ToolIdentity::new(
                        ProviderBindingRef::new("binding").unwrap(),
                        astra_turn_types::NativeToolId::new("read").unwrap(),
                    ),
                    "descriptor-v1",
                )
                .unwrap(),
            },
            &json!({"query": label}),
            &decision.decision_id,
            &freshness,
        )
        .unwrap()
    }

    fn observation(key: SemanticReadCacheKey, output: &str) -> SemanticReadObservation {
        SemanticReadObservation::from_terminal_outcome(
            key,
            &ToolInvocationTerminalOutcome::Succeeded {
                result: ToolInvocationResultPayload {
                    output: output.to_string(),
                    metadata: BTreeMap::new(),
                    exit_semantics: None,
                },
            },
        )
        .unwrap()
    }

    fn store(limits: SemanticReadCacheLimits) -> InMemorySemanticReadObservationStore {
        InMemorySemanticReadObservationStore::new(limits).unwrap()
    }

    #[test]
    fn hit_exists_only_after_owner_completes_a_valid_success() {
        let mut store = store(SemanticReadCacheLimits::default());
        let key = key("a");
        assert_eq!(
            store.lookup_or_claim(&key, "owner-a", 100, 1).unwrap(),
            SemanticReadCacheLookup::FillClaimed
        );
        assert_eq!(
            store.lookup_or_claim(&key, "owner-b", 101, 2).unwrap(),
            SemanticReadCacheLookup::FillInProgress {
                lease_expires_at_epoch_ms: 100,
            }
        );
        store
            .complete_fill(&key, "owner-a", 3, observation(key.clone(), "fresh"))
            .unwrap();
        assert!(matches!(
            store.lookup_or_claim(&key, "owner-c", 200, 4).unwrap(),
            SemanticReadCacheLookup::Hit(observation) if observation.result.output == "fresh"
        ));
        assert_eq!(store.ready_entries(), 1);
        assert_eq!(store.in_flight_fills(), 0);
    }

    #[test]
    fn expired_fill_is_reclaimed_and_stale_owner_is_fenced() {
        let mut store = store(SemanticReadCacheLimits::default());
        let key = key("a");
        store.lookup_or_claim(&key, "owner-a", 10, 1).unwrap();
        assert_eq!(
            store.lookup_or_claim(&key, "owner-b", 20, 10).unwrap(),
            SemanticReadCacheLookup::FillClaimed
        );
        assert!(matches!(
            store.complete_fill(&key, "owner-a", 11, observation(key.clone(), "stale")),
            Err(SemanticReadObservationStoreError::FillOwnerMismatch)
        ));
        store
            .complete_fill(&key, "owner-b", 11, observation(key.clone(), "current"))
            .unwrap();
        assert!(matches!(
            store.lookup_or_claim(&key, "owner-c", 30, 12).unwrap(),
            SemanticReadCacheLookup::Hit(observation) if observation.result.output == "current"
        ));
    }

    #[test]
    fn fill_and_ready_limits_are_hard_and_ready_eviction_is_lru() {
        let probe = observation(key("probe"), "12345678");
        let bytes = probe.encoded_len().unwrap();
        let mut store = store(SemanticReadCacheLimits {
            max_ready_entries: 2,
            max_ready_bytes: bytes * 2 + 64,
            max_in_flight_fills: 1,
        });
        let first = key("first");
        let second = key("second");
        let third = key("third");
        store.lookup_or_claim(&first, "owner-1", 100, 1).unwrap();
        assert_eq!(
            store.lookup_or_claim(&second, "owner-2", 100, 1).unwrap(),
            SemanticReadCacheLookup::FillCapacityExceeded
        );
        store
            .complete_fill(&first, "owner-1", 2, observation(first.clone(), "first"))
            .unwrap();
        store.lookup_or_claim(&second, "owner-2", 100, 3).unwrap();
        store
            .complete_fill(&second, "owner-2", 4, observation(second.clone(), "second"))
            .unwrap();
        assert!(matches!(
            store.lookup_or_claim(&first, "read", 100, 5).unwrap(),
            SemanticReadCacheLookup::Hit(_)
        ));
        store.lookup_or_claim(&third, "owner-3", 100, 6).unwrap();
        store
            .complete_fill(&third, "owner-3", 7, observation(third.clone(), "third"))
            .unwrap();

        assert!(matches!(
            store.lookup_or_claim(&second, "owner-new", 200, 8).unwrap(),
            SemanticReadCacheLookup::FillClaimed
        ));
        assert!(matches!(
            store.lookup_or_claim(&first, "read", 200, 8).unwrap(),
            SemanticReadCacheLookup::Hit(_)
        ));
        assert!(matches!(
            store.lookup_or_claim(&third, "read", 200, 8).unwrap(),
            SemanticReadCacheLookup::Hit(_)
        ));
    }

    #[test]
    fn store_capacity_rejection_releases_fill_without_fabricating_hit() {
        let key = key("large");
        let observation = observation(key.clone(), "larger-than-store");
        let mut store = store(SemanticReadCacheLimits {
            max_ready_entries: 1,
            max_ready_bytes: observation.encoded_len().unwrap() - 1,
            max_in_flight_fills: 1,
        });
        store.lookup_or_claim(&key, "owner", 100, 1).unwrap();
        assert!(matches!(
            store.complete_fill(&key, "owner", 2, observation),
            Err(SemanticReadObservationStoreError::ObservationExceedsStoreCapacity { .. })
        ));
        assert_eq!(store.ready_entries(), 0);
        assert_eq!(store.in_flight_fills(), 0);
        assert_eq!(
            store.lookup_or_claim(&key, "owner-new", 200, 3).unwrap(),
            SemanticReadCacheLookup::FillClaimed
        );
    }
}
