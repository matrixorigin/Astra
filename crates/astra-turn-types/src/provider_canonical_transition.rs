//! Write-ahead identity for canonical messages that a provider request owns.
//!
//! A transition is committed with physical-attempt admission before HTTP is
//! authorized.  It is intentionally independent of provider request JSON:
//! canonical recovery must never infer semantic ownership from a wire role,
//! model name, prompt text, or transport error.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{canonical_conversation_identity, parse_append_only_runtime_authority_frame};

pub const PROVIDER_CANONICAL_TRANSITION_SCHEMA_VERSION: u32 = 2;
pub const MAX_PROVIDER_CANONICAL_TRANSITION_BYTES: u64 = 512 * 1024;
pub const MAX_PROVIDER_CANONICAL_RECOVERY_BYTES: u64 = 16 * 1024 * 1024;
/// Recovery plus one append and bounded structural metadata. Keeping this
/// derived from the two semantic payload limits prevents the type layer from
/// accepting an entry that the durable WAL must later reject.
pub const MAX_PROVIDER_CANONICAL_TRANSITION_DURABLE_BYTES: u64 =
    MAX_PROVIDER_CANONICAL_RECOVERY_BYTES + MAX_PROVIDER_CANONICAL_TRANSITION_BYTES + 1024 * 1024;
pub const MAX_PROVIDER_CANONICAL_WAL_ENTRIES: u32 = 4_096;
pub const MAX_PROVIDER_CANONICAL_WAL_BYTES: u64 = 32 * 1024 * 1024;
const TRANSITION_ID_DOMAIN: &[u8] = b"astra.provider-canonical-transition.v2\0";
const HISTORY_ID_DOMAIN: &[u8] = b"astra.provider-canonical-history.v2\0";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CanonicalPrefixIdentityV1 {
    pub message_count: u32,
    pub root_hash: String,
}

impl CanonicalPrefixIdentityV1 {
    pub fn from_messages(messages: &[Value]) -> Result<Self, ProviderCanonicalTransitionError> {
        Ok(Self {
            message_count: u32::try_from(messages.len())
                .map_err(|_| ProviderCanonicalTransitionError::MessageCountOverflow)?,
            root_hash: canonical_conversation_identity(messages).0,
        })
    }
}

/// Append-friendly identity for the uncommitted provider-owned history.
///
/// The committed durable base keeps the canonical conversation root used by
/// the session coordinator. Everything after that base uses this hash chain,
/// so admitting one provider delta never has to re-hash the complete growing
/// turn history.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProviderCanonicalHistoryIdentityV2 {
    pub message_count: u32,
    pub root_hash: String,
}

impl ProviderCanonicalHistoryIdentityV2 {
    #[must_use]
    pub fn empty() -> Self {
        let mut digest = Sha256::new();
        digest.update(HISTORY_ID_DOMAIN);
        digest.update(0_u32.to_be_bytes());
        Self {
            message_count: 0,
            root_hash: format!("{:x}", digest.finalize()),
        }
    }

    pub fn from_messages(messages: &[Value]) -> Result<Self, ProviderCanonicalTransitionError> {
        Self::empty().extended(messages)
    }

    pub fn extended(&self, messages: &[Value]) -> Result<Self, ProviderCanonicalTransitionError> {
        validate_hash(&self.root_hash)?;
        let mut identity = self.clone();
        for message in messages {
            let next_count = identity
                .message_count
                .checked_add(1)
                .ok_or(ProviderCanonicalTransitionError::MessageCountOverflow)?;
            let message_root = canonical_conversation_identity(std::slice::from_ref(message)).0;
            let mut digest = Sha256::new();
            digest.update(HISTORY_ID_DOMAIN);
            digest.update(identity.root_hash.as_bytes());
            digest.update(next_count.to_be_bytes());
            digest.update(message_root.as_bytes());
            identity = Self {
                message_count: next_count,
                root_hash: format!("{:x}", digest.finalize()),
            };
        }
        Ok(identity)
    }
}

/// The committed canonical base and its append-friendly equivalent. Both are
/// computed once when a canonical turn is admitted and are bound into every
/// WAL entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProviderCanonicalWalBaseV2 {
    pub canonical: CanonicalPrefixIdentityV1,
    pub history: ProviderCanonicalHistoryIdentityV2,
}

impl ProviderCanonicalWalBaseV2 {
    pub fn from_messages(messages: &[Value]) -> Result<Self, ProviderCanonicalTransitionError> {
        Ok(Self {
            canonical: CanonicalPrefixIdentityV1::from_messages(messages)?,
            history: ProviderCanonicalHistoryIdentityV2::from_messages(messages)?,
        })
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCanonicalRecoveryModeV2 {
    AppendFromDurableBase,
    /// A lossless roll-up of the current append-only chain. Unlike a canonical
    /// replacement, this mode proves the parent result is still an exact
    /// prefix and therefore needs no rewrite authority.
    CheckpointFromParent,
    ReplaceFromDurableBase,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProviderCanonicalTransitionV2 {
    pub schema_version: u32,
    pub transition_id: String,
    /// Exact causal predecessor in the per-turn provider WAL. `None` is valid
    /// only for the first transition admitted against the durable turn base.
    pub parent_transition_id: Option<String>,
    /// Exact result identity of `parent_transition_id`. Linked entries recover
    /// messages produced after the parent admission from this boundary.
    pub parent_result: Option<ProviderCanonicalHistoryIdentityV2>,
    pub durable_base: ProviderCanonicalWalBaseV2,
    pub recovery_mode: ProviderCanonicalRecoveryModeV2,
    pub replacement_compaction_generation: Option<u64>,
    pub recovery_messages: Vec<Value>,
    pub predecessor: ProviderCanonicalHistoryIdentityV2,
    pub result: ProviderCanonicalHistoryIdentityV2,
    pub appended_messages: Vec<Value>,
}

struct ProviderCanonicalRecoveryPlan {
    parent_transition_id: Option<String>,
    parent_result: Option<ProviderCanonicalHistoryIdentityV2>,
    durable_base: ProviderCanonicalWalBaseV2,
    recovery_mode: ProviderCanonicalRecoveryModeV2,
    replacement_compaction_generation: Option<u64>,
    recovery_messages: Vec<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderCanonicalTransitionApply {
    Applied,
    AlreadyApplied,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ProviderCanonicalTransitionError {
    #[error("unsupported provider canonical transition schema {0}")]
    UnsupportedSchema(u32),
    #[error("provider canonical transition message count overflow")]
    MessageCountOverflow,
    #[error("provider canonical transition exceeds its serialized-byte bound")]
    TooManyBytes,
    #[error("provider canonical transition contains an invalid append shape")]
    InvalidAppendShape,
    #[error("provider canonical transition recovery suffix exceeds its serialized-byte bound")]
    TooManyRecoveryBytes,
    #[error("provider canonical transition exceeds its total durable-byte bound")]
    TooManyDurableBytes,
    #[error("provider canonical transition contains invalid canonical recovery messages")]
    InvalidCanonicalRecovery,
    #[error("provider canonical transition recovery counts do not reconstruct its predecessor")]
    RecoveryCountMismatch,
    #[error("linked provider canonical transition requires its exact predecessor")]
    MissingLinkedPredecessor,
    #[error("provider canonical append predecessor does not preserve its durable base")]
    DurableBaseNotPrefix,
    #[error("provider canonical replacement has invalid compaction authorization evidence")]
    InvalidReplacementAuthorization,
    #[error("provider canonical replacement does not match its predecessor identity")]
    RecoveryRootMismatch,
    #[error("provider canonical transition contains an invalid runtime authority")]
    InvalidRuntimeAuthority,
    #[error("provider canonical transition contains an invalid hash")]
    InvalidHash,
    #[error("provider canonical transition WAL chain exceeds its entry bound")]
    TooManyWalEntries,
    #[error("provider canonical transition WAL chain exceeds its byte bound")]
    TooManyWalBytes,
    #[error("provider canonical transition WAL chain is discontinuous")]
    DiscontinuousChain,
    #[error("provider canonical replacement must anchor the WAL chain")]
    ReplacementNotChainAnchor,
    #[error("provider canonical transition identity does not match its content")]
    IdentityMismatch,
    #[error("provider canonical transition result count does not extend its predecessor")]
    ResultCountMismatch,
    #[error("canonical history does not match the transition predecessor or result prefix")]
    PrefixConflict,
    #[error("provider canonical transition result root could not be reproduced")]
    ResultRootMismatch,
}

impl ProviderCanonicalTransitionV2 {
    pub fn new(
        parent_transition_id: Option<String>,
        predecessor_messages: &[Value],
        appended_messages: Vec<Value>,
    ) -> Result<Self, ProviderCanonicalTransitionError> {
        Self::new_from_durable_base(
            parent_transition_id,
            ProviderCanonicalWalBaseV2::from_messages(predecessor_messages)?,
            predecessor_messages,
            appended_messages,
        )
    }

    pub fn new_from_durable_base(
        parent_transition_id: Option<String>,
        durable_base: ProviderCanonicalWalBaseV2,
        predecessor_messages: &[Value],
        appended_messages: Vec<Value>,
    ) -> Result<Self, ProviderCanonicalTransitionError> {
        validate_appended_messages(&appended_messages)?;
        let durable_base_count = usize::try_from(durable_base.canonical.message_count)
            .map_err(|_| ProviderCanonicalTransitionError::MessageCountOverflow)?;
        let base_is_preserved = predecessor_messages.len() >= durable_base_count
            && canonical_conversation_identity(&predecessor_messages[..durable_base_count]).0
                == durable_base.canonical.root_hash
            && ProviderCanonicalHistoryIdentityV2::from_messages(
                &predecessor_messages[..durable_base_count],
            )? == durable_base.history;
        if !base_is_preserved {
            return Err(ProviderCanonicalTransitionError::DurableBaseNotPrefix);
        }
        // The first entry is a self-contained recovery anchor. This convenience
        // constructor models an immediately adjacent successor; callers with
        // provider/tool messages between admissions use
        // `new_linked_from_durable_base` to persist that incremental gap.
        let (parent_result, recovery_messages) = if parent_transition_id.is_none() {
            (None, predecessor_messages[durable_base_count..].to_vec())
        } else {
            (
                Some(ProviderCanonicalHistoryIdentityV2::from_messages(
                    predecessor_messages,
                )?),
                Vec::new(),
            )
        };
        Self::new_with_recovery(
            ProviderCanonicalRecoveryPlan {
                parent_transition_id,
                parent_result,
                durable_base,
                recovery_mode: ProviderCanonicalRecoveryModeV2::AppendFromDurableBase,
                replacement_compaction_generation: None,
                recovery_messages,
            },
            predecessor_messages,
            appended_messages,
        )
    }

    /// Construct a linked entry whose predecessor contains messages produced
    /// after the parent provider admission. Only that gap and this entry's own
    /// append become durable payload.
    pub fn new_linked_from_durable_base(
        parent_transition_id: String,
        parent_result: ProviderCanonicalHistoryIdentityV2,
        durable_base: ProviderCanonicalWalBaseV2,
        predecessor_messages: &[Value],
        appended_messages: Vec<Value>,
    ) -> Result<Self, ProviderCanonicalTransitionError> {
        validate_appended_messages(&appended_messages)?;
        let durable_base_count = usize::try_from(durable_base.canonical.message_count)
            .map_err(|_| ProviderCanonicalTransitionError::MessageCountOverflow)?;
        let parent_count = usize::try_from(parent_result.message_count)
            .map_err(|_| ProviderCanonicalTransitionError::MessageCountOverflow)?;
        if predecessor_messages.len() < durable_base_count
            || CanonicalPrefixIdentityV1::from_messages(
                &predecessor_messages[..durable_base_count],
            )? != durable_base.canonical
            || ProviderCanonicalHistoryIdentityV2::from_messages(
                &predecessor_messages[..durable_base_count],
            )? != durable_base.history
        {
            return Err(ProviderCanonicalTransitionError::DurableBaseNotPrefix);
        }
        if predecessor_messages.len() < parent_count
            || ProviderCanonicalHistoryIdentityV2::from_messages(
                &predecessor_messages[..parent_count],
            )? != parent_result
        {
            return Err(ProviderCanonicalTransitionError::MissingLinkedPredecessor);
        }
        Self::new_linked_from_deltas(
            parent_transition_id,
            parent_result,
            durable_base,
            predecessor_messages[parent_count..].to_vec(),
            appended_messages,
        )
    }

    /// Construct a linked entry from only the messages added since its parent.
    /// The caller owns the in-memory parent identity; the durable service later
    /// serializes admission against the exact database head.
    pub fn new_linked_from_deltas(
        parent_transition_id: String,
        parent_result: ProviderCanonicalHistoryIdentityV2,
        durable_base: ProviderCanonicalWalBaseV2,
        recovery_messages: Vec<Value>,
        appended_messages: Vec<Value>,
    ) -> Result<Self, ProviderCanonicalTransitionError> {
        let predecessor = parent_result.extended(&recovery_messages)?;
        Self::new_with_recovery_and_predecessor(
            ProviderCanonicalRecoveryPlan {
                parent_transition_id: Some(parent_transition_id),
                parent_result: Some(parent_result),
                durable_base,
                recovery_mode: ProviderCanonicalRecoveryModeV2::AppendFromDurableBase,
                replacement_compaction_generation: None,
                recovery_messages,
            },
            predecessor,
            appended_messages,
        )
    }

    /// Collapse an append-only chain without changing canonical history. The
    /// complete predecessor is stored as the new recovery anchor, and its
    /// parent prefix is cryptographically checked before construction.
    pub fn new_checkpoint_from_parent(
        parent_transition_id: String,
        parent_result: ProviderCanonicalHistoryIdentityV2,
        durable_base: ProviderCanonicalWalBaseV2,
        predecessor_messages: &[Value],
        appended_messages: Vec<Value>,
    ) -> Result<Self, ProviderCanonicalTransitionError> {
        let durable_base_count = usize::try_from(durable_base.canonical.message_count)
            .map_err(|_| ProviderCanonicalTransitionError::MessageCountOverflow)?;
        let parent_count = usize::try_from(parent_result.message_count)
            .map_err(|_| ProviderCanonicalTransitionError::MessageCountOverflow)?;
        if predecessor_messages.len() < durable_base_count
            || CanonicalPrefixIdentityV1::from_messages(
                &predecessor_messages[..durable_base_count],
            )? != durable_base.canonical
            || ProviderCanonicalHistoryIdentityV2::from_messages(
                &predecessor_messages[..durable_base_count],
            )? != durable_base.history
        {
            return Err(ProviderCanonicalTransitionError::DurableBaseNotPrefix);
        }
        if predecessor_messages.len() < parent_count
            || ProviderCanonicalHistoryIdentityV2::from_messages(
                &predecessor_messages[..parent_count],
            )? != parent_result
        {
            return Err(ProviderCanonicalTransitionError::MissingLinkedPredecessor);
        }
        let predecessor = ProviderCanonicalHistoryIdentityV2::from_messages(predecessor_messages)?;
        Self::new_with_recovery_and_predecessor(
            ProviderCanonicalRecoveryPlan {
                parent_transition_id: Some(parent_transition_id),
                parent_result: Some(parent_result),
                durable_base,
                recovery_mode: ProviderCanonicalRecoveryModeV2::CheckpointFromParent,
                replacement_compaction_generation: None,
                recovery_messages: predecessor_messages.to_vec(),
            },
            predecessor,
            appended_messages,
        )
    }

    /// Construct a recovery replacement only with an explicit compaction
    /// generation issued by the runtime's canonical rewrite proof. Prefix
    /// mismatch alone is never replacement authority.
    pub fn new_replacement_from_durable_base(
        parent_transition_id: Option<String>,
        durable_base: ProviderCanonicalWalBaseV2,
        replacement_compaction_generation: u64,
        predecessor_messages: &[Value],
        appended_messages: Vec<Value>,
    ) -> Result<Self, ProviderCanonicalTransitionError> {
        validate_appended_messages(&appended_messages)?;
        Self::new_with_recovery(
            ProviderCanonicalRecoveryPlan {
                parent_transition_id,
                parent_result: None,
                durable_base,
                recovery_mode: ProviderCanonicalRecoveryModeV2::ReplaceFromDurableBase,
                replacement_compaction_generation: Some(replacement_compaction_generation),
                recovery_messages: predecessor_messages.to_vec(),
            },
            predecessor_messages,
            appended_messages,
        )
    }

    fn new_with_recovery(
        recovery: ProviderCanonicalRecoveryPlan,
        predecessor_messages: &[Value],
        appended_messages: Vec<Value>,
    ) -> Result<Self, ProviderCanonicalTransitionError> {
        let predecessor = ProviderCanonicalHistoryIdentityV2::from_messages(predecessor_messages)?;
        Self::new_with_recovery_and_predecessor(recovery, predecessor, appended_messages)
    }

    fn new_with_recovery_and_predecessor(
        recovery: ProviderCanonicalRecoveryPlan,
        predecessor: ProviderCanonicalHistoryIdentityV2,
        appended_messages: Vec<Value>,
    ) -> Result<Self, ProviderCanonicalTransitionError> {
        let ProviderCanonicalRecoveryPlan {
            parent_transition_id,
            parent_result,
            durable_base,
            recovery_mode,
            replacement_compaction_generation,
            recovery_messages,
        } = recovery;
        validate_recovery_messages(&recovery_messages)?;
        let result = predecessor.extended(&appended_messages)?;
        let mut transition = Self {
            schema_version: PROVIDER_CANONICAL_TRANSITION_SCHEMA_VERSION,
            transition_id: String::new(),
            parent_transition_id,
            parent_result,
            durable_base,
            recovery_mode,
            replacement_compaction_generation,
            recovery_messages,
            predecessor,
            result,
            appended_messages,
        };
        transition.transition_id = transition_identity(&transition);
        transition.validate()?;
        Ok(transition)
    }

    pub fn durable_payload_bytes(&self) -> Result<u64, ProviderCanonicalTransitionError> {
        crate::json_serialized_len(std::slice::from_ref(self))
            .map_err(|_| ProviderCanonicalTransitionError::TooManyDurableBytes)
    }

    pub fn validate(&self) -> Result<(), ProviderCanonicalTransitionError> {
        if self.schema_version != PROVIDER_CANONICAL_TRANSITION_SCHEMA_VERSION {
            return Err(ProviderCanonicalTransitionError::UnsupportedSchema(
                self.schema_version,
            ));
        }
        validate_hash(&self.predecessor.root_hash)?;
        validate_hash(&self.result.root_hash)?;
        validate_hash(&self.durable_base.canonical.root_hash)?;
        validate_hash(&self.durable_base.history.root_hash)?;
        if self.durable_base.canonical.message_count != self.durable_base.history.message_count {
            return Err(ProviderCanonicalTransitionError::RecoveryCountMismatch);
        }
        validate_hash(&self.transition_id)?;
        if let Some(parent_transition_id) = self.parent_transition_id.as_deref() {
            validate_hash(parent_transition_id)?;
        }
        if let Some(parent_result) = self.parent_result.as_ref() {
            validate_hash(&parent_result.root_hash)?;
        }
        validate_recovery_messages(&self.recovery_messages)?;
        validate_appended_messages(&self.appended_messages)?;
        let recovery_count = u32::try_from(self.recovery_messages.len())
            .map_err(|_| ProviderCanonicalTransitionError::MessageCountOverflow)?;
        match self.recovery_mode {
            ProviderCanonicalRecoveryModeV2::AppendFromDurableBase => {
                if self.replacement_compaction_generation.is_some() {
                    return Err(ProviderCanonicalTransitionError::InvalidReplacementAuthorization);
                }
                match (&self.parent_transition_id, &self.parent_result) {
                    (Some(_), Some(parent_result)) => {
                        if parent_result.message_count.checked_add(recovery_count)
                            != Some(self.predecessor.message_count)
                        {
                            return Err(ProviderCanonicalTransitionError::RecoveryCountMismatch);
                        }
                        if parent_result.extended(&self.recovery_messages)? != self.predecessor {
                            return Err(ProviderCanonicalTransitionError::RecoveryRootMismatch);
                        }
                    }
                    (None, None) => {
                        if self
                            .durable_base
                            .history
                            .message_count
                            .checked_add(recovery_count)
                            != Some(self.predecessor.message_count)
                        {
                            return Err(ProviderCanonicalTransitionError::RecoveryCountMismatch);
                        }
                        if self
                            .durable_base
                            .history
                            .extended(&self.recovery_messages)?
                            != self.predecessor
                        {
                            return Err(ProviderCanonicalTransitionError::RecoveryRootMismatch);
                        }
                    }
                    _ => return Err(ProviderCanonicalTransitionError::RecoveryCountMismatch),
                }
            }
            ProviderCanonicalRecoveryModeV2::CheckpointFromParent => {
                if self.replacement_compaction_generation.is_some() {
                    return Err(ProviderCanonicalTransitionError::InvalidReplacementAuthorization);
                }
                let (Some(_), Some(parent_result)) =
                    (&self.parent_transition_id, &self.parent_result)
                else {
                    return Err(ProviderCanonicalTransitionError::MissingLinkedPredecessor);
                };
                if recovery_count != self.predecessor.message_count {
                    return Err(ProviderCanonicalTransitionError::RecoveryCountMismatch);
                }
                let durable_base_count = usize::try_from(self.durable_base.canonical.message_count)
                    .map_err(|_| ProviderCanonicalTransitionError::MessageCountOverflow)?;
                let parent_count = usize::try_from(parent_result.message_count)
                    .map_err(|_| ProviderCanonicalTransitionError::MessageCountOverflow)?;
                if self.recovery_messages.len() < durable_base_count
                    || CanonicalPrefixIdentityV1::from_messages(
                        &self.recovery_messages[..durable_base_count],
                    )? != self.durable_base.canonical
                    || ProviderCanonicalHistoryIdentityV2::from_messages(
                        &self.recovery_messages[..durable_base_count],
                    )? != self.durable_base.history
                {
                    return Err(ProviderCanonicalTransitionError::DurableBaseNotPrefix);
                }
                if self.recovery_messages.len() < parent_count
                    || ProviderCanonicalHistoryIdentityV2::from_messages(
                        &self.recovery_messages[..parent_count],
                    )? != *parent_result
                    || ProviderCanonicalHistoryIdentityV2::from_messages(&self.recovery_messages)?
                        != self.predecessor
                {
                    return Err(ProviderCanonicalTransitionError::RecoveryRootMismatch);
                }
            }
            ProviderCanonicalRecoveryModeV2::ReplaceFromDurableBase => {
                if self.replacement_compaction_generation.is_none() || self.parent_result.is_some()
                {
                    return Err(ProviderCanonicalTransitionError::InvalidReplacementAuthorization);
                }
                if recovery_count != self.predecessor.message_count {
                    return Err(ProviderCanonicalTransitionError::RecoveryCountMismatch);
                }
                if ProviderCanonicalHistoryIdentityV2::from_messages(&self.recovery_messages)?
                    != self.predecessor
                {
                    return Err(ProviderCanonicalTransitionError::RecoveryRootMismatch);
                }
            }
        }
        let append_count = u32::try_from(self.appended_messages.len())
            .map_err(|_| ProviderCanonicalTransitionError::MessageCountOverflow)?;
        if self.predecessor.message_count.checked_add(append_count)
            != Some(self.result.message_count)
        {
            return Err(ProviderCanonicalTransitionError::ResultCountMismatch);
        }
        if self.predecessor.extended(&self.appended_messages)? != self.result {
            return Err(ProviderCanonicalTransitionError::ResultRootMismatch);
        }
        if self.transition_id != transition_identity(self) {
            return Err(ProviderCanonicalTransitionError::IdentityMismatch);
        }
        let durable_bytes = crate::json_serialized_len(self)
            .map_err(|_| ProviderCanonicalTransitionError::TooManyDurableBytes)?;
        if durable_bytes > MAX_PROVIDER_CANONICAL_TRANSITION_DURABLE_BYTES {
            return Err(ProviderCanonicalTransitionError::TooManyDurableBytes);
        }
        Ok(())
    }

    pub fn reconstruct_predecessor_from_durable_base(
        &self,
        durable_base_messages: &[Value],
    ) -> Result<Vec<Value>, ProviderCanonicalTransitionError> {
        self.validate()?;
        if CanonicalPrefixIdentityV1::from_messages(durable_base_messages)?
            != self.durable_base.canonical
            || ProviderCanonicalHistoryIdentityV2::from_messages(durable_base_messages)?
                != self.durable_base.history
        {
            return Err(ProviderCanonicalTransitionError::PrefixConflict);
        }
        let predecessor = match self.recovery_mode {
            ProviderCanonicalRecoveryModeV2::AppendFromDurableBase => {
                if self.parent_transition_id.is_some() {
                    return Err(ProviderCanonicalTransitionError::MissingLinkedPredecessor);
                }
                let mut messages = durable_base_messages.to_vec();
                messages.extend(self.recovery_messages.iter().cloned());
                messages
            }
            ProviderCanonicalRecoveryModeV2::CheckpointFromParent => {
                return Err(ProviderCanonicalTransitionError::MissingLinkedPredecessor);
            }
            ProviderCanonicalRecoveryModeV2::ReplaceFromDurableBase => {
                self.recovery_messages.clone()
            }
        };
        if ProviderCanonicalHistoryIdentityV2::from_messages(&predecessor)? != self.predecessor {
            return Err(ProviderCanonicalTransitionError::RecoveryRootMismatch);
        }
        Ok(predecessor)
    }

    /// Apply to a WAL-owned history prefix. Callers must detach any fresh
    /// post-crash suffix at the durable-base boundary before replay; message
    /// equality cannot prove whether repeated user input is old or fresh.
    pub fn apply_to(
        &self,
        messages: &mut Vec<Value>,
    ) -> Result<ProviderCanonicalTransitionApply, ProviderCanonicalTransitionError> {
        Self::apply_chain_to(std::slice::from_ref(self), messages)
    }

    /// Atomically apply an ordered WAL chain with work proportional to the
    /// materialized history and transition payloads. Intermediate prefix
    /// identities are causally bound by transition ids and parent-result
    /// links; the fully reconstructed append-friendly identity is hashed once.
    pub fn apply_chain_to(
        transitions: &[Self],
        messages: &mut Vec<Value>,
    ) -> Result<ProviderCanonicalTransitionApply, ProviderCanonicalTransitionError> {
        if transitions.is_empty() {
            return Ok(ProviderCanonicalTransitionApply::AlreadyApplied);
        }
        if transitions.len() > MAX_PROVIDER_CANONICAL_WAL_ENTRIES as usize {
            return Err(ProviderCanonicalTransitionError::TooManyWalEntries);
        }

        let mut durable_bytes = 0_u64;
        for (index, transition) in transitions.iter().enumerate() {
            transition.validate()?;
            let entry_bytes = crate::json_serialized_len(std::slice::from_ref(transition))
                .map_err(|_| ProviderCanonicalTransitionError::TooManyWalBytes)?;
            durable_bytes = durable_bytes
                .checked_add(entry_bytes)
                .ok_or(ProviderCanonicalTransitionError::TooManyWalBytes)?;
            if durable_bytes > MAX_PROVIDER_CANONICAL_WAL_BYTES {
                return Err(ProviderCanonicalTransitionError::TooManyWalBytes);
            }
            if index == 0 {
                continue;
            }
            let parent = &transitions[index - 1];
            if transition.recovery_mode != ProviderCanonicalRecoveryModeV2::AppendFromDurableBase {
                return Err(ProviderCanonicalTransitionError::ReplacementNotChainAnchor);
            }
            if transition.parent_transition_id.as_deref() != Some(parent.transition_id.as_str())
                || transition.parent_result.as_ref() != Some(&parent.result)
                || transition.durable_base != parent.durable_base
            {
                return Err(ProviderCanonicalTransitionError::DiscontinuousChain);
            }
        }

        let first = &transitions[0];
        let current = ProviderCanonicalHistoryIdentityV2::from_messages(messages)?;
        let final_result = &transitions
            .last()
            .expect("non-empty canonical transition chain")
            .result;
        if current == *final_result {
            return Ok(ProviderCanonicalTransitionApply::AlreadyApplied);
        }

        let mut candidate = messages.clone();
        if current != first.predecessor {
            match first.recovery_mode {
                ProviderCanonicalRecoveryModeV2::AppendFromDurableBase => {
                    let expected_base = match (
                        first.parent_transition_id.as_ref(),
                        first.parent_result.as_ref(),
                    ) {
                        (Some(_), Some(parent_result)) => parent_result,
                        (None, None) => &first.durable_base.history,
                        _ => {
                            return Err(ProviderCanonicalTransitionError::RecoveryCountMismatch);
                        }
                    };
                    if current != *expected_base {
                        return Err(if first.parent_transition_id.is_some() {
                            ProviderCanonicalTransitionError::MissingLinkedPredecessor
                        } else {
                            ProviderCanonicalTransitionError::PrefixConflict
                        });
                    }
                    candidate.extend(first.recovery_messages.iter().cloned());
                }
                ProviderCanonicalRecoveryModeV2::CheckpointFromParent => {
                    if CanonicalPrefixIdentityV1::from_messages(messages)?
                        != first.durable_base.canonical
                    {
                        return Err(ProviderCanonicalTransitionError::PrefixConflict);
                    }
                    candidate.clear();
                    candidate.extend(first.recovery_messages.iter().cloned());
                }
                ProviderCanonicalRecoveryModeV2::ReplaceFromDurableBase => {
                    if CanonicalPrefixIdentityV1::from_messages(messages)?
                        != first.durable_base.canonical
                    {
                        return Err(ProviderCanonicalTransitionError::PrefixConflict);
                    }
                    candidate.clear();
                    candidate.extend(first.recovery_messages.iter().cloned());
                }
            }
        }

        for (index, transition) in transitions.iter().enumerate() {
            if index != 0 {
                candidate.extend(transition.recovery_messages.iter().cloned());
            }
            if u32::try_from(candidate.len()).ok() != Some(transition.predecessor.message_count) {
                return Err(ProviderCanonicalTransitionError::RecoveryCountMismatch);
            }
            candidate.extend(transition.appended_messages.iter().cloned());
            if u32::try_from(candidate.len()).ok() != Some(transition.result.message_count) {
                return Err(ProviderCanonicalTransitionError::ResultCountMismatch);
            }
        }

        if ProviderCanonicalHistoryIdentityV2::from_messages(&candidate)? != *final_result {
            return Err(ProviderCanonicalTransitionError::ResultRootMismatch);
        }
        crate::validate_canonical_tool_pairing(&candidate)
            .map_err(|_| ProviderCanonicalTransitionError::InvalidCanonicalRecovery)?;
        *messages = candidate;
        Ok(ProviderCanonicalTransitionApply::Applied)
    }
}

fn validate_recovery_messages(messages: &[Value]) -> Result<(), ProviderCanonicalTransitionError> {
    let bytes = crate::json_serialized_len(messages)
        .map_err(|_| ProviderCanonicalTransitionError::TooManyRecoveryBytes)?;
    if bytes > MAX_PROVIDER_CANONICAL_RECOVERY_BYTES {
        return Err(ProviderCanonicalTransitionError::TooManyRecoveryBytes);
    }
    for message in messages {
        let Some(object) = message.as_object() else {
            return Err(ProviderCanonicalTransitionError::InvalidCanonicalRecovery);
        };
        if !matches!(
            object.get("role").and_then(Value::as_str),
            Some("system" | "user" | "assistant" | "tool")
        ) {
            return Err(ProviderCanonicalTransitionError::InvalidCanonicalRecovery);
        }
        if crate::is_runtime_owned_message(message) {
            match crate::runtime_message_delivery(message) {
                Some(crate::RuntimeMessageDelivery::AppendOnlyRequiredContext) => {
                    parse_append_only_runtime_authority_frame(message)
                        .map_err(|_| ProviderCanonicalTransitionError::InvalidCanonicalRecovery)?;
                }
                Some(_) | None => {
                    return Err(ProviderCanonicalTransitionError::InvalidCanonicalRecovery);
                }
            }
        }
    }
    Ok(())
}

fn validate_appended_messages(messages: &[Value]) -> Result<(), ProviderCanonicalTransitionError> {
    let bytes = crate::json_serialized_len(messages)
        .map_err(|_| ProviderCanonicalTransitionError::TooManyBytes)?;
    if bytes > MAX_PROVIDER_CANONICAL_TRANSITION_BYTES {
        return Err(ProviderCanonicalTransitionError::TooManyBytes);
    }

    let Some(first) = messages.first() else {
        return Ok(());
    };
    let authority_start = if first.get("role").and_then(Value::as_str) == Some("assistant") {
        if messages.len() < 2 {
            return Err(ProviderCanonicalTransitionError::InvalidAppendShape);
        }
        1
    } else {
        0
    };
    if messages[authority_start..]
        .iter()
        .any(|message| !valid_runtime_authority(message))
    {
        return Err(ProviderCanonicalTransitionError::InvalidRuntimeAuthority);
    }
    Ok(())
}

fn valid_runtime_authority(message: &Value) -> bool {
    parse_append_only_runtime_authority_frame(message).is_ok()
}

fn validate_hash(hash: &str) -> Result<(), ProviderCanonicalTransitionError> {
    if hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(ProviderCanonicalTransitionError::InvalidHash)
    }
}

fn transition_identity(transition: &ProviderCanonicalTransitionV2) -> String {
    let recovery_root = canonical_conversation_identity(&transition.recovery_messages).0;
    let appended_root = canonical_conversation_identity(&transition.appended_messages).0;
    let mut digest = Sha256::new();
    digest.update(TRANSITION_ID_DOMAIN);
    digest.update(PROVIDER_CANONICAL_TRANSITION_SCHEMA_VERSION.to_be_bytes());
    match transition.parent_transition_id.as_deref() {
        Some(parent_transition_id) => {
            digest.update([1]);
            digest.update(parent_transition_id.as_bytes());
        }
        None => digest.update([0]),
    }
    match transition.parent_result.as_ref() {
        Some(parent_result) => {
            digest.update([1]);
            digest.update(parent_result.message_count.to_be_bytes());
            digest.update(parent_result.root_hash.as_bytes());
        }
        None => digest.update([0]),
    }
    digest.update(
        transition
            .durable_base
            .canonical
            .message_count
            .to_be_bytes(),
    );
    digest.update(transition.durable_base.canonical.root_hash.as_bytes());
    digest.update(transition.durable_base.history.root_hash.as_bytes());
    digest.update([match transition.recovery_mode {
        ProviderCanonicalRecoveryModeV2::AppendFromDurableBase => 0,
        ProviderCanonicalRecoveryModeV2::CheckpointFromParent => 1,
        ProviderCanonicalRecoveryModeV2::ReplaceFromDurableBase => 2,
    }]);
    match transition.replacement_compaction_generation {
        Some(generation) => {
            digest.update([1]);
            digest.update(generation.to_be_bytes());
        }
        None => digest.update([0]),
    }
    digest.update(recovery_root.as_bytes());
    digest.update(transition.predecessor.message_count.to_be_bytes());
    digest.update(transition.predecessor.root_hash.as_bytes());
    digest.update(transition.result.message_count.to_be_bytes());
    digest.update(transition.result.root_hash.as_bytes());
    digest.update(appended_root.as_bytes());
    format!("{:x}", digest.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        RuntimeAuthorityLifetime, mark_append_only_required_context,
        render_append_only_runtime_authority_frame,
    };
    use serde_json::json;

    fn authority(kind: &str) -> Value {
        let content = render_append_only_runtime_authority_frame(
            kind,
            RuntimeAuthorityLifetime::NextAssistantDecision,
            &format!("opaque {kind}"),
        )
        .expect("frame");
        let mut message = json!({"role": "user", "content": content});
        mark_append_only_required_context(
            &mut message,
            kind,
            RuntimeAuthorityLifetime::NextAssistantDecision,
        );
        message
    }

    #[test]
    fn transition_applies_once_before_a_fresh_user_suffix() {
        let base = vec![json!({"role": "user", "content": "goal"})];
        let transition = ProviderCanonicalTransitionV2::new(
            None,
            &base,
            vec![authority("work"), authority("budget")],
        )
        .unwrap();
        let fresh = json!({"role": "user", "content": "fresh follow-up"});
        let mut restored = base.clone();
        assert_eq!(
            transition.apply_to(&mut restored).unwrap(),
            ProviderCanonicalTransitionApply::Applied
        );
        assert_eq!(restored[1], transition.appended_messages[0]);
        assert_eq!(restored[2], transition.appended_messages[1]);
        assert_eq!(
            transition.apply_to(&mut restored).unwrap(),
            ProviderCanonicalTransitionApply::AlreadyApplied
        );
        restored.push(fresh);

        let mut ambiguous = vec![base[0].clone(), base[0].clone()];
        assert_eq!(
            transition.apply_to(&mut ambiguous),
            Err(ProviderCanonicalTransitionError::PrefixConflict),
            "the transition layer must not guess where a repeated fresh suffix begins"
        );
    }

    #[test]
    fn assistant_and_authority_are_one_atomic_transition() {
        let base = vec![json!({"role": "user", "content": "goal"})];
        let appended = vec![
            json!({"role": "assistant", "content": "partial"}),
            authority("continue"),
        ];
        let transition = ProviderCanonicalTransitionV2::new(None, &base, appended.clone()).unwrap();
        let mut restored = base;
        transition.apply_to(&mut restored).unwrap();
        assert_eq!(&restored[1..], appended.as_slice());
        assert!(
            ProviderCanonicalTransitionV2::new(
                None,
                &restored[..1],
                vec![json!({"role": "assistant", "content": "orphan"})],
            )
            .is_err()
        );
    }

    #[test]
    fn valid_authority_cardinality_is_bounded_by_bytes_not_an_arbitrary_count() {
        let base = vec![json!({"role": "user", "content": "goal"})];
        let authorities = (0..24).map(|_| authority("skill")).collect::<Vec<_>>();
        ProviderCanonicalTransitionV2::new(None, &base, authorities)
            .expect("all valid prompt authorities must fit when the byte budget fits");

        let content = render_append_only_runtime_authority_frame(
            "skill",
            RuntimeAuthorityLifetime::NextAssistantDecision,
            &"x".repeat(MAX_PROVIDER_CANONICAL_TRANSITION_BYTES as usize),
        )
        .unwrap();
        let mut oversized = json!({"role": "user", "content": content});
        mark_append_only_required_context(
            &mut oversized,
            "skill",
            RuntimeAuthorityLifetime::NextAssistantDecision,
        );
        assert_eq!(
            ProviderCanonicalTransitionV2::new(None, &base, vec![oversized]),
            Err(ProviderCanonicalTransitionError::TooManyBytes)
        );
    }

    #[test]
    fn transition_validation_rejects_tampering_without_inspecting_text() {
        let base = vec![json!({"role": "user", "content": "goal"})];
        let mut transition =
            ProviderCanonicalTransitionV2::new(None, &base, vec![authority("work")]).unwrap();
        let content = transition.appended_messages[0]["content"]
            .as_str()
            .expect("framed content")
            .replace("opaque work", "changed");
        transition.appended_messages[0]["content"] = Value::String(content);
        assert_eq!(
            transition.validate(),
            Err(ProviderCanonicalTransitionError::ResultRootMismatch)
        );
    }

    #[test]
    fn parent_identity_is_explicit_and_covered_by_the_transition_hash() {
        let base = vec![json!({"role": "user", "content": "goal"})];
        let first =
            ProviderCanonicalTransitionV2::new(None, &base, vec![authority("work")]).unwrap();
        let mut predecessor = base;
        predecessor.extend(first.appended_messages.iter().cloned());
        let mut child = ProviderCanonicalTransitionV2::new(
            Some(first.transition_id.clone()),
            &predecessor,
            vec![authority("budget")],
        )
        .unwrap();
        assert_eq!(
            child.parent_transition_id.as_deref(),
            Some(first.transition_id.as_str())
        );

        child.parent_transition_id = Some("0".repeat(64));
        assert_eq!(
            child.validate(),
            Err(ProviderCanonicalTransitionError::IdentityMismatch)
        );
    }

    #[test]
    fn linked_wal_entries_store_only_their_own_delta_and_require_ordered_replay() {
        let durable = vec![json!({"role": "user", "content": "goal"})];
        let durable_base = ProviderCanonicalWalBaseV2::from_messages(&durable).unwrap();
        let first = ProviderCanonicalTransitionV2::new_from_durable_base(
            None,
            durable_base.clone(),
            &durable,
            vec![authority("work")],
        )
        .unwrap();
        let mut after_first = durable.clone();
        after_first.extend(first.appended_messages.iter().cloned());
        let second = ProviderCanonicalTransitionV2::new_from_durable_base(
            Some(first.transition_id.clone()),
            durable_base,
            &after_first,
            vec![authority("budget")],
        )
        .unwrap();

        assert!(second.recovery_messages.is_empty());
        assert_eq!(
            second.apply_to(&mut durable.clone()),
            Err(ProviderCanonicalTransitionError::MissingLinkedPredecessor)
        );

        let mut restored = durable;
        first.apply_to(&mut restored).unwrap();
        second.apply_to(&mut restored).unwrap();
        assert_eq!(restored, {
            let mut expected = after_first;
            expected.extend(second.appended_messages.iter().cloned());
            expected
        });
    }

    #[test]
    fn linked_entry_recovers_messages_produced_after_parent_admission() {
        let durable = vec![json!({"role": "user", "content": "goal"})];
        let durable_base = ProviderCanonicalWalBaseV2::from_messages(&durable).unwrap();
        let first = ProviderCanonicalTransitionV2::new_from_durable_base(
            None,
            durable_base.clone(),
            &durable,
            vec![authority("first")],
        )
        .unwrap();
        let mut after_parent_admission = durable.clone();
        after_parent_admission.extend(first.appended_messages.iter().cloned());
        let provider_response = json!({"role": "assistant", "content": "provider response"});
        let mut next_predecessor = after_parent_admission.clone();
        next_predecessor.push(provider_response.clone());
        let second = ProviderCanonicalTransitionV2::new_linked_from_durable_base(
            first.transition_id.clone(),
            first.result.clone(),
            durable_base,
            &next_predecessor,
            vec![authority("second")],
        )
        .unwrap();

        assert_eq!(second.parent_result.as_ref(), Some(&first.result));
        assert_eq!(second.recovery_messages, vec![provider_response]);

        let mut restored = durable;
        first.apply_to(&mut restored).unwrap();
        second.apply_to(&mut restored).unwrap();
        let mut expected = next_predecessor;
        expected.extend(second.appended_messages.iter().cloned());
        assert_eq!(restored, expected);
    }

    #[test]
    fn corrupt_linked_gap_never_partially_mutates_history() {
        let durable = vec![json!({"role": "user", "content": "goal"})];
        let durable_base = ProviderCanonicalWalBaseV2::from_messages(&durable).unwrap();
        let first = ProviderCanonicalTransitionV2::new_from_durable_base(
            None,
            durable_base.clone(),
            &durable,
            vec![authority("first")],
        )
        .unwrap();
        let mut parent_result_messages = durable.clone();
        parent_result_messages.extend(first.appended_messages.iter().cloned());
        let mut predecessor = parent_result_messages.clone();
        predecessor.push(json!({"role": "assistant", "content": "provider response"}));
        let mut second = ProviderCanonicalTransitionV2::new_linked_from_durable_base(
            first.transition_id,
            first.result,
            durable_base,
            &predecessor,
            vec![authority("second")],
        )
        .unwrap();

        second.recovery_messages[0]["content"] = Value::String("corrupted response".into());
        second.transition_id = transition_identity(&second);
        let before = parent_result_messages.clone();

        assert_eq!(
            second.apply_to(&mut parent_result_messages),
            Err(ProviderCanonicalTransitionError::RecoveryRootMismatch)
        );
        assert_eq!(parent_result_messages, before);
    }

    #[test]
    fn fixed_size_linked_entries_have_linear_serialized_storage() {
        const ENTRIES: usize = 256;
        let durable = vec![json!({"role": "user", "content": "goal"})];
        let durable_base = ProviderCanonicalWalBaseV2::from_messages(&durable).unwrap();
        let mut history = durable.clone();
        let mut encoded_lengths = Vec::with_capacity(ENTRIES);
        let mut transitions = Vec::with_capacity(ENTRIES);

        let first = ProviderCanonicalTransitionV2::new_from_durable_base(
            None,
            durable_base.clone(),
            &history,
            vec![authority("work")],
        )
        .unwrap();
        encoded_lengths.push(serde_json::to_vec(&[&first]).unwrap().len());
        history.extend(first.appended_messages.iter().cloned());
        transitions.push(first);

        for _ in 1..ENTRIES {
            let parent = transitions.last().expect("first transition");
            let transition = ProviderCanonicalTransitionV2::new_linked_from_deltas(
                parent.transition_id.clone(),
                parent.result.clone(),
                durable_base.clone(),
                Vec::new(),
                vec![authority("work")],
            )
            .unwrap();
            encoded_lengths.push(serde_json::to_vec(&[&transition]).unwrap().len());
            history.extend(transition.appended_messages.iter().cloned());
            transitions.push(transition);
        }

        let minimum = encoded_lengths.iter().skip(1).copied().min().unwrap();
        let maximum = encoded_lengths.iter().skip(1).copied().max().unwrap();
        assert!(maximum <= minimum + 128);
        assert!(encoded_lengths.iter().sum::<usize>() <= ENTRIES * 2_048);

        let mut restored = durable;
        assert_eq!(
            ProviderCanonicalTransitionV2::apply_chain_to(&transitions, &mut restored).unwrap(),
            ProviderCanonicalTransitionApply::Applied
        );
        assert_eq!(restored, history);
        assert_eq!(
            ProviderCanonicalTransitionV2::apply_chain_to(&transitions, &mut restored).unwrap(),
            ProviderCanonicalTransitionApply::AlreadyApplied
        );
    }

    #[test]
    fn incremental_history_identity_matches_one_pass_materialization() {
        let prefix = vec![
            json!({"role": "user", "content": "goal"}),
            json!({"role": "assistant", "content": "working"}),
        ];
        let suffix = vec![authority("work"), authority("budget")];
        let mut materialized = prefix.clone();
        materialized.extend(suffix.iter().cloned());

        let incrementally_extended = ProviderCanonicalHistoryIdentityV2::from_messages(&prefix)
            .unwrap()
            .extended(&suffix)
            .unwrap();
        assert_eq!(
            incrementally_extended,
            ProviderCanonicalHistoryIdentityV2::from_messages(&materialized).unwrap()
        );
    }

    #[test]
    fn checkpoint_is_a_lossless_anchor_without_rewrite_authority() {
        let durable = vec![json!({"role": "user", "content": "goal"})];
        let durable_base = ProviderCanonicalWalBaseV2::from_messages(&durable).unwrap();
        let first = ProviderCanonicalTransitionV2::new_from_durable_base(
            None,
            durable_base.clone(),
            &durable,
            vec![authority("first")],
        )
        .unwrap();
        let mut predecessor = durable.clone();
        predecessor.extend(first.appended_messages.iter().cloned());
        predecessor.push(json!({"role": "assistant", "content": "provider result"}));
        let checkpoint = ProviderCanonicalTransitionV2::new_checkpoint_from_parent(
            first.transition_id,
            first.result,
            durable_base,
            &predecessor,
            vec![authority("next")],
        )
        .unwrap();
        assert_eq!(
            checkpoint.recovery_mode,
            ProviderCanonicalRecoveryModeV2::CheckpointFromParent
        );
        assert_eq!(checkpoint.replacement_compaction_generation, None);

        let mut recovered = durable;
        assert_eq!(
            ProviderCanonicalTransitionV2::apply_chain_to(
                std::slice::from_ref(&checkpoint),
                &mut recovered,
            )
            .unwrap(),
            ProviderCanonicalTransitionApply::Applied
        );
        predecessor.extend(checkpoint.appended_messages.iter().cloned());
        assert_eq!(recovered, predecessor);
    }

    #[test]
    fn chain_replay_rejects_discontinuity_without_mutating_history() {
        let durable = vec![json!({"role": "user", "content": "goal"})];
        let first =
            ProviderCanonicalTransitionV2::new(None, &durable, vec![authority("first")]).unwrap();
        let unrelated =
            ProviderCanonicalTransitionV2::new(None, &durable, vec![authority("unrelated")])
                .unwrap();
        let mut restored = durable.clone();

        assert_eq!(
            ProviderCanonicalTransitionV2::apply_chain_to(&[first, unrelated], &mut restored,),
            Err(ProviderCanonicalTransitionError::DiscontinuousChain)
        );
        assert_eq!(restored, durable);
    }

    #[test]
    fn chain_replay_enforces_its_own_entry_budget_before_materialization() {
        let durable = vec![json!({"role": "user", "content": "goal"})];
        let transition =
            ProviderCanonicalTransitionV2::new(None, &durable, vec![authority("work")]).unwrap();
        let oversized = vec![transition; MAX_PROVIDER_CANONICAL_WAL_ENTRIES as usize + 1];
        let mut restored = durable.clone();

        assert_eq!(
            ProviderCanonicalTransitionV2::apply_chain_to(&oversized, &mut restored),
            Err(ProviderCanonicalTransitionError::TooManyWalEntries)
        );
        assert_eq!(restored, durable);
    }

    #[test]
    fn chain_replay_rejects_a_non_anchor_replacement_atomically() {
        let durable = vec![json!({"role": "user", "content": "goal"})];
        let durable_base = ProviderCanonicalWalBaseV2::from_messages(&durable).unwrap();
        let first = ProviderCanonicalTransitionV2::new_from_durable_base(
            None,
            durable_base.clone(),
            &durable,
            vec![authority("first")],
        )
        .unwrap();
        let replacement = ProviderCanonicalTransitionV2::new_replacement_from_durable_base(
            Some(first.transition_id.clone()),
            durable_base,
            1,
            &[json!({"role": "user", "content": "checkpoint"})],
            vec![authority("replacement")],
        )
        .unwrap();
        let mut restored = durable.clone();

        assert_eq!(
            ProviderCanonicalTransitionV2::apply_chain_to(&[first, replacement], &mut restored,),
            Err(ProviderCanonicalTransitionError::ReplacementNotChainAnchor)
        );
        assert_eq!(restored, durable);
    }

    #[test]
    fn replacement_anchor_accepts_incremental_successors() {
        let durable = vec![json!({"role": "user", "content": "original"})];
        let durable_base = ProviderCanonicalWalBaseV2::from_messages(&durable).unwrap();
        let rewritten = vec![json!({"role": "system", "content": "typed summary"})];
        let replacement = ProviderCanonicalTransitionV2::new_replacement_from_durable_base(
            None,
            durable_base.clone(),
            1,
            &rewritten,
            vec![authority("replacement")],
        )
        .unwrap();
        let provider_response = json!({"role": "assistant", "content": "intermediate"});
        let successor = ProviderCanonicalTransitionV2::new_linked_from_deltas(
            replacement.transition_id.clone(),
            replacement.result.clone(),
            durable_base,
            vec![provider_response.clone()],
            vec![authority("successor")],
        )
        .unwrap();

        let mut recovered = durable;
        ProviderCanonicalTransitionV2::apply_chain_to(
            &[replacement.clone(), successor.clone()],
            &mut recovered,
        )
        .unwrap();
        let mut expected = rewritten;
        expected.extend(replacement.appended_messages);
        expected.push(provider_response);
        expected.extend(successor.appended_messages);
        assert_eq!(recovered, expected);
    }

    #[test]
    fn wrong_result_root_never_partially_mutates_history() {
        let base = vec![json!({"role": "user", "content": "goal"})];
        let mut transition =
            ProviderCanonicalTransitionV2::new(None, &base, vec![authority("budget")]).unwrap();
        transition.result.root_hash = "0".repeat(64);
        transition.transition_id = transition_identity(&transition);
        let mut history = base;
        let before = history.clone();

        assert_eq!(
            transition.apply_to(&mut history),
            Err(ProviderCanonicalTransitionError::ResultRootMismatch)
        );
        assert_eq!(history, before);
    }

    #[test]
    fn crash_before_checkpoint_recovers_lossless_turn_before_fresh_user_input() {
        let durable_head = vec![
            json!({"role": "user", "content": "older request"}),
            json!({"role": "assistant", "content": "older answer"}),
        ];
        let mut predecessor = durable_head.clone();
        predecessor.extend([
            json!({"role": "user", "content": "run the command"}),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call-1",
                    "type": "function",
                    "function": {"name": "bash", "arguments": "{}"}
                }]
            }),
            json!({"role": "tool", "tool_call_id": "call-1", "content": "done"}),
        ]);
        let authority = authority("budget");
        let transition = ProviderCanonicalTransitionV2::new_from_durable_base(
            None,
            ProviderCanonicalWalBaseV2::from_messages(&durable_head).unwrap(),
            &predecessor,
            vec![authority.clone()],
        )
        .unwrap();
        assert_eq!(
            transition.recovery_mode,
            ProviderCanonicalRecoveryModeV2::AppendFromDurableBase
        );

        // Simulate process loss after provider delivery authorization but
        // before any step/canonical checkpoint, followed by a fresh `hi`.
        let fresh_user = json!({"role": "user", "content": "hi"});
        let mut restored = durable_head.clone();
        transition.apply_to(&mut restored).unwrap();
        restored.push(fresh_user.clone());

        let mut old_wire_suffix = predecessor;
        old_wire_suffix.push(authority);
        assert!(restored.starts_with(&old_wire_suffix));
        assert_eq!(restored.last(), Some(&fresh_user));
    }

    #[test]
    fn recovery_only_transition_preserves_a_request_without_runtime_frames() {
        let durable_head = vec![json!({"role": "assistant", "content": "ready"})];
        let mut predecessor = durable_head.clone();
        predecessor.push(json!({"role": "user", "content": "old request"}));
        let transition = ProviderCanonicalTransitionV2::new_from_durable_base(
            None,
            ProviderCanonicalWalBaseV2::from_messages(&durable_head).unwrap(),
            &predecessor,
            Vec::new(),
        )
        .unwrap();
        assert_eq!(transition.predecessor, transition.result);

        let mut restored = durable_head;
        assert_eq!(
            transition.apply_to(&mut restored).unwrap(),
            ProviderCanonicalTransitionApply::Applied
        );
        assert_eq!(restored, predecessor);
    }

    #[test]
    fn authorized_rewrite_recovers_as_explicit_base_replacement() {
        let durable_head = vec![
            json!({"role": "user", "content": "large old request"}),
            json!({"role": "assistant", "content": "large old answer"}),
        ];
        let rewritten = vec![
            json!({"role": "system", "content": "typed compacted summary"}),
            json!({"role": "user", "content": "current request"}),
        ];
        let authority = authority("continue");
        assert_eq!(
            ProviderCanonicalTransitionV2::new_from_durable_base(
                None,
                ProviderCanonicalWalBaseV2::from_messages(&durable_head).unwrap(),
                &rewritten,
                vec![authority.clone()],
            ),
            Err(ProviderCanonicalTransitionError::DurableBaseNotPrefix)
        );
        let transition = ProviderCanonicalTransitionV2::new_replacement_from_durable_base(
            None,
            ProviderCanonicalWalBaseV2::from_messages(&durable_head).unwrap(),
            1,
            &rewritten,
            vec![authority.clone()],
        )
        .unwrap();
        assert_eq!(
            transition.recovery_mode,
            ProviderCanonicalRecoveryModeV2::ReplaceFromDurableBase
        );

        let fresh_user = json!({"role": "user", "content": "hi"});
        let mut restored = durable_head;
        transition.apply_to(&mut restored).unwrap();
        restored.push(fresh_user.clone());
        assert_eq!(&restored[..rewritten.len()], rewritten.as_slice());
        assert_eq!(restored[rewritten.len()], authority);
        assert_eq!(restored.last(), Some(&fresh_user));
    }

    #[test]
    fn non_append_runtime_controls_cannot_enter_canonical_recovery() {
        let durable_base = ProviderCanonicalWalBaseV2::from_messages(&[]).unwrap();
        for delivery in [
            crate::RuntimeMessageDelivery::EphemeralControl,
            crate::RuntimeMessageDelivery::RequiredContext,
            crate::RuntimeMessageDelivery::Projection,
        ] {
            let recovery = vec![crate::runtime_owned_message(
                "system",
                "must remain process local",
                delivery,
            )];
            assert_eq!(
                ProviderCanonicalTransitionV2::new_from_durable_base(
                    None,
                    durable_base.clone(),
                    &recovery,
                    vec![authority("budget")],
                ),
                Err(ProviderCanonicalTransitionError::InvalidCanonicalRecovery)
            );
        }
    }
}
