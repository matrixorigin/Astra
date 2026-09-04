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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCanonicalRecoveryModeV2 {
    AppendFromDurableBase,
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
    pub parent_result: Option<CanonicalPrefixIdentityV1>,
    pub durable_base: CanonicalPrefixIdentityV1,
    pub recovery_mode: ProviderCanonicalRecoveryModeV2,
    pub replacement_compaction_generation: Option<u64>,
    pub recovery_messages: Vec<Value>,
    pub predecessor: CanonicalPrefixIdentityV1,
    pub result: CanonicalPrefixIdentityV1,
    pub appended_messages: Vec<Value>,
}

struct ProviderCanonicalRecoveryPlan {
    parent_transition_id: Option<String>,
    parent_result: Option<CanonicalPrefixIdentityV1>,
    durable_base: CanonicalPrefixIdentityV1,
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
            CanonicalPrefixIdentityV1::from_messages(predecessor_messages)?,
            predecessor_messages,
            appended_messages,
        )
    }

    pub fn new_from_durable_base(
        parent_transition_id: Option<String>,
        durable_base: CanonicalPrefixIdentityV1,
        predecessor_messages: &[Value],
        appended_messages: Vec<Value>,
    ) -> Result<Self, ProviderCanonicalTransitionError> {
        validate_appended_messages(&appended_messages)?;
        let durable_base_count = usize::try_from(durable_base.message_count)
            .map_err(|_| ProviderCanonicalTransitionError::MessageCountOverflow)?;
        let base_is_preserved = predecessor_messages.len() >= durable_base_count
            && canonical_conversation_identity(&predecessor_messages[..durable_base_count]).0
                == durable_base.root_hash;
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
                Some(CanonicalPrefixIdentityV1::from_messages(
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
        parent_result: CanonicalPrefixIdentityV1,
        durable_base: CanonicalPrefixIdentityV1,
        predecessor_messages: &[Value],
        appended_messages: Vec<Value>,
    ) -> Result<Self, ProviderCanonicalTransitionError> {
        validate_appended_messages(&appended_messages)?;
        let durable_base_count = usize::try_from(durable_base.message_count)
            .map_err(|_| ProviderCanonicalTransitionError::MessageCountOverflow)?;
        let parent_count = usize::try_from(parent_result.message_count)
            .map_err(|_| ProviderCanonicalTransitionError::MessageCountOverflow)?;
        if predecessor_messages.len() < durable_base_count
            || CanonicalPrefixIdentityV1::from_messages(
                &predecessor_messages[..durable_base_count],
            )? != durable_base
        {
            return Err(ProviderCanonicalTransitionError::DurableBaseNotPrefix);
        }
        if predecessor_messages.len() < parent_count
            || CanonicalPrefixIdentityV1::from_messages(&predecessor_messages[..parent_count])?
                != parent_result
        {
            return Err(ProviderCanonicalTransitionError::MissingLinkedPredecessor);
        }
        Self::new_with_recovery(
            ProviderCanonicalRecoveryPlan {
                parent_transition_id: Some(parent_transition_id),
                parent_result: Some(parent_result),
                durable_base,
                recovery_mode: ProviderCanonicalRecoveryModeV2::AppendFromDurableBase,
                replacement_compaction_generation: None,
                recovery_messages: predecessor_messages[parent_count..].to_vec(),
            },
            predecessor_messages,
            appended_messages,
        )
    }

    /// Construct a recovery replacement only with an explicit compaction
    /// generation issued by the runtime's canonical rewrite proof. Prefix
    /// mismatch alone is never replacement authority.
    pub fn new_replacement_from_durable_base(
        parent_transition_id: Option<String>,
        durable_base: CanonicalPrefixIdentityV1,
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
        let ProviderCanonicalRecoveryPlan {
            parent_transition_id,
            parent_result,
            durable_base,
            recovery_mode,
            replacement_compaction_generation,
            recovery_messages,
        } = recovery;
        let predecessor = CanonicalPrefixIdentityV1::from_messages(predecessor_messages)?;
        validate_recovery_messages(&recovery_messages)?;
        let mut result_messages = Vec::with_capacity(
            predecessor_messages
                .len()
                .saturating_add(appended_messages.len()),
        );
        result_messages.extend_from_slice(predecessor_messages);
        result_messages.extend(appended_messages.iter().cloned());
        crate::validate_canonical_tool_pairing(&result_messages)
            .map_err(|_| ProviderCanonicalTransitionError::InvalidCanonicalRecovery)?;
        let result = CanonicalPrefixIdentityV1::from_messages(&result_messages)?;
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

    pub fn validate(&self) -> Result<(), ProviderCanonicalTransitionError> {
        if self.schema_version != PROVIDER_CANONICAL_TRANSITION_SCHEMA_VERSION {
            return Err(ProviderCanonicalTransitionError::UnsupportedSchema(
                self.schema_version,
            ));
        }
        validate_hash(&self.predecessor.root_hash)?;
        validate_hash(&self.result.root_hash)?;
        validate_hash(&self.durable_base.root_hash)?;
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
                    }
                    (None, None) => {
                        if self.durable_base.message_count.checked_add(recovery_count)
                            != Some(self.predecessor.message_count)
                        {
                            return Err(ProviderCanonicalTransitionError::RecoveryCountMismatch);
                        }
                    }
                    _ => return Err(ProviderCanonicalTransitionError::RecoveryCountMismatch),
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
                if CanonicalPrefixIdentityV1::from_messages(&self.recovery_messages)?
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
        if CanonicalPrefixIdentityV1::from_messages(durable_base_messages)? != self.durable_base {
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
            ProviderCanonicalRecoveryModeV2::ReplaceFromDurableBase => {
                self.recovery_messages.clone()
            }
        };
        if CanonicalPrefixIdentityV1::from_messages(&predecessor)? != self.predecessor {
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
        self.validate()?;
        let predecessor_count = usize::try_from(self.predecessor.message_count)
            .map_err(|_| ProviderCanonicalTransitionError::MessageCountOverflow)?;
        let result_count = usize::try_from(self.result.message_count)
            .map_err(|_| ProviderCanonicalTransitionError::MessageCountOverflow)?;

        if messages.len() == result_count
            && canonical_conversation_identity(messages).0 == self.result.root_hash
        {
            return Ok(ProviderCanonicalTransitionApply::AlreadyApplied);
        }
        let mut candidate = messages.clone();
        let predecessor_is_present = candidate.len() == predecessor_count
            && canonical_conversation_identity(&candidate).0 == self.predecessor.root_hash;
        if !predecessor_is_present {
            if self.recovery_mode == ProviderCanonicalRecoveryModeV2::AppendFromDurableBase
                && self.parent_transition_id.is_some()
            {
                let parent_result = self
                    .parent_result
                    .as_ref()
                    .ok_or(ProviderCanonicalTransitionError::MissingLinkedPredecessor)?;
                if CanonicalPrefixIdentityV1::from_messages(&candidate)? != *parent_result {
                    return Err(ProviderCanonicalTransitionError::MissingLinkedPredecessor);
                }
                candidate.extend(self.recovery_messages.iter().cloned());
                if CanonicalPrefixIdentityV1::from_messages(&candidate)? != self.predecessor {
                    return Err(ProviderCanonicalTransitionError::RecoveryRootMismatch);
                }
            } else {
                candidate = self.reconstruct_predecessor_from_durable_base(&candidate)?;
            }
        }
        candidate.splice(
            predecessor_count..predecessor_count,
            self.appended_messages.iter().cloned(),
        );
        if canonical_conversation_identity(&candidate[..result_count]).0 != self.result.root_hash {
            return Err(ProviderCanonicalTransitionError::ResultRootMismatch);
        }
        crate::validate_canonical_tool_pairing(&candidate[..result_count])
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
    digest.update(transition.durable_base.message_count.to_be_bytes());
    digest.update(transition.durable_base.root_hash.as_bytes());
    digest.update([match transition.recovery_mode {
        ProviderCanonicalRecoveryModeV2::AppendFromDurableBase => 0,
        ProviderCanonicalRecoveryModeV2::ReplaceFromDurableBase => 1,
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
    fn transition_identity_rejects_tampering_without_inspecting_text() {
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
            Err(ProviderCanonicalTransitionError::IdentityMismatch)
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
        let durable_base = CanonicalPrefixIdentityV1::from_messages(&durable).unwrap();
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
        let durable_base = CanonicalPrefixIdentityV1::from_messages(&durable).unwrap();
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
        let durable_base = CanonicalPrefixIdentityV1::from_messages(&durable).unwrap();
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
        let durable_base = CanonicalPrefixIdentityV1::from_messages(&durable).unwrap();
        let mut history = durable.clone();
        let mut parent = None;
        let mut encoded_lengths = Vec::with_capacity(ENTRIES);

        for _ in 0..ENTRIES {
            let transition = ProviderCanonicalTransitionV2::new_from_durable_base(
                parent,
                durable_base.clone(),
                &history,
                vec![authority("work")],
            )
            .unwrap();
            encoded_lengths.push(serde_json::to_vec(&[&transition]).unwrap().len());
            history.extend(transition.appended_messages.iter().cloned());
            parent = Some(transition.transition_id);
        }

        let minimum = encoded_lengths.iter().skip(1).copied().min().unwrap();
        let maximum = encoded_lengths.iter().skip(1).copied().max().unwrap();
        assert!(maximum <= minimum + 128);
        assert!(encoded_lengths.iter().sum::<usize>() <= ENTRIES * 2_048);
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
            CanonicalPrefixIdentityV1::from_messages(&durable_head).unwrap(),
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
            CanonicalPrefixIdentityV1::from_messages(&durable_head).unwrap(),
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
                CanonicalPrefixIdentityV1::from_messages(&durable_head).unwrap(),
                &rewritten,
                vec![authority.clone()],
            ),
            Err(ProviderCanonicalTransitionError::DurableBaseNotPrefix)
        );
        let transition = ProviderCanonicalTransitionV2::new_replacement_from_durable_base(
            None,
            CanonicalPrefixIdentityV1::from_messages(&durable_head).unwrap(),
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
        let durable_base = CanonicalPrefixIdentityV1::from_messages(&[]).unwrap();
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
