use serde_json::Value;

#[derive(Debug, Clone)]
pub(crate) struct CanonicalRewriteProof {
    base_root: String,
    base_compaction_generation: u64,
    base_prefix_len: usize,
    authorized_prefix_len: usize,
    authorized_prefix_root: String,
    rewritten: bool,
    valid: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct CanonicalRewritePermit {
    valid: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ProviderWalReplacementAuthorization {
    pub(crate) generation: u64,
}

impl CanonicalRewriteProof {
    pub(crate) fn new(
        admitted_prefix: &[Value],
        base_root: &str,
        base_compaction_generation: u64,
    ) -> Self {
        let admitted_root = astra_turn_types::canonical_conversation_root(admitted_prefix);
        Self {
            base_root: base_root.to_string(),
            base_compaction_generation,
            base_prefix_len: admitted_prefix.len(),
            authorized_prefix_len: admitted_prefix.len(),
            authorized_prefix_root: admitted_root.clone(),
            rewritten: false,
            valid: admitted_root == base_root,
        }
    }

    pub(crate) fn begin(&self, messages: &[Value]) -> CanonicalRewritePermit {
        let valid = self.valid
            && messages.len() >= self.authorized_prefix_len
            && astra_turn_types::canonical_conversation_root(
                &messages[..self.authorized_prefix_len],
            ) == self.authorized_prefix_root;
        CanonicalRewritePermit { valid }
    }

    pub(crate) fn finish(&mut self, permit: CanonicalRewritePermit, messages: &[Value]) {
        if !permit.valid {
            self.valid = false;
            return;
        }
        self.authorized_prefix_len = messages.len();
        self.authorized_prefix_root = astra_turn_types::canonical_conversation_root(messages);
        self.rewritten = true;
    }

    fn authorizes(&self, messages: &[Value]) -> bool {
        self.valid
            && self.rewritten
            && messages.len() >= self.authorized_prefix_len
            && astra_turn_types::canonical_conversation_root(
                &messages[..self.authorized_prefix_len],
            ) == self.authorized_prefix_root
    }

    fn authorizes_scratch_normalization(
        &self,
        prior_messages: &[Value],
        messages: &[Value],
    ) -> bool {
        self.valid
            && !self.rewritten
            && prior_messages.len() == self.authorized_prefix_len
            && astra_turn_types::canonical_conversation_root(prior_messages)
                == self.authorized_prefix_root
            && messages.starts_with(prior_messages)
    }

    pub(crate) fn replacement_generation(&self) -> Option<u64> {
        self.valid
            .then(|| self.base_compaction_generation.saturating_add(1))
    }

    pub(crate) fn base_root(&self) -> &str {
        &self.base_root
    }

    pub(crate) fn provider_wal_replacement_authorization(
        &self,
        durable_base: &astra_turn_types::CanonicalPrefixIdentityV1,
        messages: &[Value],
    ) -> Option<ProviderWalReplacementAuthorization> {
        let base_count = usize::try_from(durable_base.message_count).ok()?;
        if base_count != self.base_prefix_len
            || durable_base.root_hash != self.base_root
            || !self.authorizes(messages)
        {
            return None;
        }
        Some(ProviderWalReplacementAuthorization {
            generation: self.base_compaction_generation.saturating_add(1),
        })
    }

    pub(crate) fn recover_provider_wal_replacement(
        &mut self,
        durable_base: &astra_turn_types::CanonicalPrefixIdentityV1,
        transition: &astra_turn_types::ProviderCanonicalTransitionV2,
        recovered_messages: &[Value],
    ) -> Result<(), String> {
        transition.validate().map_err(|error| error.to_string())?;
        if transition.recovery_mode
            != astra_turn_types::ProviderCanonicalRecoveryModeV2::ReplaceFromDurableBase
            || &transition.durable_base != durable_base
            || transition.replacement_compaction_generation
                != Some(self.base_compaction_generation.saturating_add(1))
            || usize::try_from(durable_base.message_count).ok() != Some(self.base_prefix_len)
            || durable_base.root_hash != self.base_root
        {
            return Err("provider WAL replacement does not match the admitted rewrite base".into());
        }
        let result_count = usize::try_from(transition.result.message_count)
            .map_err(|_| "provider WAL replacement result count overflow".to_string())?;
        if recovered_messages.len() < result_count
            || astra_turn_types::canonical_conversation_root(&recovered_messages[..result_count])
                != transition.result.root_hash
        {
            return Err("provider WAL replacement result is absent from recovered history".into());
        }
        self.authorized_prefix_len = result_count;
        self.authorized_prefix_root = transition.result.root_hash.clone();
        self.rewritten = true;
        self.valid = true;
        Ok(())
    }
}

pub(crate) fn canonical_commit_delta(
    prior_messages: &[Value],
    had_canonical_head: bool,
    messages: &[Value],
    rewrite_proof: Option<&CanonicalRewriteProof>,
    preserve_execution_scratch: bool,
) -> Result<Option<(astra_turn_types::CanonicalDeltaModeV1, Vec<Vec<Value>>)>, String> {
    let prefix_preserved = messages.starts_with(prior_messages);
    let rewrite_authorized =
        had_canonical_head && rewrite_proof.is_some_and(|proof| proof.authorizes(messages));
    let scratch_normalization_authorized = had_canonical_head
        && !preserve_execution_scratch
        && contains_execution_scratch(prior_messages)
        && rewrite_proof
            .is_some_and(|proof| proof.authorizes_scratch_normalization(prior_messages, messages));
    let mode = if rewrite_authorized || scratch_normalization_authorized {
        astra_turn_types::CanonicalDeltaModeV1::Replace
    } else if prefix_preserved {
        astra_turn_types::CanonicalDeltaModeV1::Append
    } else {
        return Err("canonical conversation mutated the admitted prefix without a verified compaction rewrite".into());
    };
    let changed_messages = if mode == astra_turn_types::CanonicalDeltaModeV1::Replace {
        messages
    } else {
        &messages[prior_messages.len()..]
    };
    let canonical_changed = if preserve_execution_scratch {
        // An interrupted turn may resume from its tool boundary, so retain
        // complete call/result groups until recovery has settled it.
        astra_turn_core::prompt_facing::sanitize_compacted_canonical_continuation_messages_with_turn_semantics(
            changed_messages.to_vec(),
        )
    } else {
        // Tool frames are execution scratch, not cross-turn conversation.
        // Durable typed state and the final assistant response carry the
        // completed turn's semantics without replaying every intermediate
        // payload into all future model requests.
        astra_turn_core::prompt_facing::sanitize_completed_canonical_turn_messages_with_turn_semantics(
            changed_messages.to_vec(),
        )
    }
    .map_err(|error| format!("canonical turn contains invalid user-turn semantics: {error}"))?;
    let logical_segments = pack_canonical_turn_segments(canonical_changed);
    if logical_segments.is_empty() {
        return if preserve_execution_scratch {
            Ok(None)
        } else {
            Err("canonical turn produced no committable messages".into())
        };
    }
    Ok(Some((mode, logical_segments)))
}

fn contains_execution_scratch(messages: &[Value]) -> bool {
    messages.iter().any(|message| {
        message.get("role").and_then(Value::as_str) == Some("tool")
            || opens_structured_tool_group(message)
    })
}

pub(crate) fn pack_canonical_turn_segments(mut messages: Vec<Value>) -> Vec<Vec<Value>> {
    const TARGET_PACK_BYTES: u64 = 512 * 1024;

    let mut packs = Vec::new();
    let mut current = Vec::new();
    let mut current_bytes = 2_u64;
    let mut index = 0;
    while index < messages.len() {
        let group_start = index;
        let keeps_tool_results = opens_structured_tool_group(&messages[index]);
        index += 1;
        if keeps_tool_results {
            while index < messages.len() && is_structured_tool_result(&messages[index]) {
                index += 1;
            }
        }
        let group = &mut messages[group_start..index];
        let group_bytes = astra_turn_types::canonical_conversation_serialized_len(group);
        let projected = current_bytes
            .saturating_add(group_bytes.saturating_sub(2))
            .saturating_add(u64::from(!current.is_empty()));
        if !current.is_empty() && projected > TARGET_PACK_BYTES {
            packs.push(std::mem::take(&mut current));
            current_bytes = 2;
        }
        current_bytes = current_bytes
            .saturating_add(group_bytes.saturating_sub(2))
            .saturating_add(u64::from(!current.is_empty()));
        current.extend(group.iter_mut().map(Value::take));
    }
    if !current.is_empty() {
        packs.push(current);
    }
    packs
}

fn opens_structured_tool_group(message: &Value) -> bool {
    message.get("role").and_then(Value::as_str) == Some("assistant")
        && (message
            .get("tool_calls")
            .and_then(Value::as_array)
            .is_some_and(|calls| !calls.is_empty())
            || message
                .get("content")
                .and_then(Value::as_array)
                .is_some_and(|content| {
                    content
                        .iter()
                        .any(|item| item.get("type").and_then(Value::as_str) == Some("tool_use"))
                }))
}

fn is_structured_tool_result(message: &Value) -> bool {
    message.get("role").and_then(Value::as_str) == Some("tool")
        || message
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(|content| {
                !content.is_empty()
                    && content
                        .iter()
                        .all(|item| item.get("type").and_then(Value::as_str) == Some("tool_result"))
            })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn authority() -> Value {
        let content = astra_turn_types::render_append_only_runtime_authority_frame(
            "continue",
            astra_turn_types::RuntimeAuthorityLifetime::NextAssistantDecision,
            "continue safely",
        )
        .unwrap();
        let mut message = json!({"role": "user", "content": content});
        astra_turn_types::mark_append_only_required_context(
            &mut message,
            "continue",
            astra_turn_types::RuntimeAuthorityLifetime::NextAssistantDecision,
        );
        message
    }

    #[test]
    fn provider_replacement_requires_a_valid_pre_mutation_rewrite_permit() {
        let durable = vec![
            json!({"role": "user", "content": "old"}),
            json!({"role": "assistant", "content": "answer"}),
        ];
        let base = astra_turn_types::CanonicalPrefixIdentityV1::from_messages(&durable).unwrap();

        let mut invalid = CanonicalRewriteProof::new(&durable, &base.root_hash, 4);
        let rewritten = vec![json!({"role": "system", "content": "summary"})];
        let invalid_permit = invalid.begin(&rewritten);
        invalid.finish(invalid_permit, &rewritten);
        assert_eq!(
            invalid.provider_wal_replacement_authorization(&base, &rewritten),
            None
        );

        let mut valid = CanonicalRewriteProof::new(&durable, &base.root_hash, 4);
        let permit = valid.begin(&durable);
        valid.finish(permit, &rewritten);
        let authorization = valid
            .provider_wal_replacement_authorization(&base, &rewritten)
            .expect("valid rewrite authorization");
        assert_eq!(authorization.generation, 5);
        assert_eq!(
            valid.provider_wal_replacement_authorization(&base, &durable),
            None,
            "authorization is bound to the exact rewritten predecessor"
        );
    }

    #[test]
    fn recovered_provider_replacement_advances_the_rewrite_proof() {
        let durable = vec![
            json!({"role": "user", "content": "old"}),
            json!({"role": "assistant", "content": "answer"}),
        ];
        let base = astra_turn_types::CanonicalPrefixIdentityV1::from_messages(&durable).unwrap();
        let mut live = CanonicalRewriteProof::new(&durable, &base.root_hash, 7);
        let mut source = durable.clone();
        source.push(json!({"role": "user", "content": "current"}));
        let permit = live.begin(&source);
        let rewritten = vec![json!({"role": "user", "content": "typed summary"})];
        live.finish(permit, &rewritten);
        let authorization = live
            .provider_wal_replacement_authorization(&base, &rewritten)
            .unwrap();
        let transition =
            astra_turn_types::ProviderCanonicalTransitionV2::new_replacement_from_durable_base(
                None,
                base.clone(),
                authorization.generation,
                &rewritten,
                vec![authority()],
            )
            .unwrap();

        let mut recovered = durable.clone();
        transition.apply_to(&mut recovered).unwrap();
        let mut restored_proof = CanonicalRewriteProof::new(&durable, &base.root_hash, 7);
        restored_proof
            .recover_provider_wal_replacement(&base, &transition, &recovered)
            .unwrap();
        let mut completed = recovered.clone();
        completed.push(json!({"role": "assistant", "content": "done"}));
        let (mode, _) =
            canonical_commit_delta(&durable, true, &completed, Some(&restored_proof), false)
                .unwrap()
                .expect("recovered replacement remains committable");
        assert_eq!(mode, astra_turn_types::CanonicalDeltaModeV1::Replace);

        let second_permit = restored_proof.begin(&completed);
        let second_rewrite = vec![json!({"role": "user", "content": "summary two"})];
        restored_proof.finish(second_permit, &second_rewrite);
        let second_authorization = restored_proof
            .provider_wal_replacement_authorization(&base, &second_rewrite)
            .unwrap();
        assert_eq!(second_authorization.generation, 8);
    }
}
