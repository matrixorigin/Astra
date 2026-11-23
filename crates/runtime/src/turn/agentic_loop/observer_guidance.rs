//! Process-local evidence for a best-effort observation. Acknowledgement records
//! a fixed-size fingerprint of public predecessors, never tool output or prompts.
//! Settlement locates the acknowledged batches by identity and verifies that the
//! same evidence still exists before sending it to Memoria.
use super::host::AgenticLoopState;
use astra_turn_types::{
    is_human_user_message, is_runtime_owned_message, tool_result_count, turn_message_provenance,
};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub(crate) const OBSERVER_MESSAGE_CHARS: usize = 500;
// Memoria counts the UTF-8 role prefix and newline in its complete input.
pub(crate) const OBSERVER_EXCHANGE_BYTES: usize = 6_000;
pub(crate) const OBSERVER_DIALOGUE_BYTES: usize = OBSERVER_EXCHANGE_BYTES - "[user]: \n".len();

pub(crate) struct ObserverGuidanceBoundary {
    first_intent: usize,
    pub(crate) standalone: bool,
    first_batch: String,
    preceding_chain: Option<String>,
    fingerprint: Option<[u8; 32]>,
    pub(crate) settled_tool_count: usize,
}

impl AgenticLoopState {
    pub(crate) fn begin_observer_guidance(&mut self, batch_id: &str) {
        let calls = &self.stall.tool_call_records;
        let previous = self.user_intents.observer_guidance.take();
        let applied = self.user_intents.applied_user_intents();
        let since = calls.get(previous.as_ref().map_or(0, |w| w.settled_tool_count)..);
        let recovered = previous.is_none() && !applied.is_empty();
        // A completed tool or a restored Run leaves private or missing context.
        // Start at this user batch. Later verified user batches can add evidence.
        let reset = recovered || since.is_none_or(|calls| !calls.is_empty());
        let mut window = previous
            .filter(|_| !reset)
            .unwrap_or_else(|| ObserverGuidanceBoundary {
                first_intent: applied.len(),
                standalone: reset,
                first_batch: batch_id.to_owned(),
                preceding_chain: self.canonical_turn_chain_id.clone(),
                fingerprint: Some([0; 32]),
                settled_tool_count: 0,
            });
        if reset {
            window.settled_tool_count = calls.len();
            self.user_intents.observer_guidance = Some(window);
            return;
        }
        window.fingerprint = window.fingerprint.and_then(|previous| {
            let prior = self.messages.iter().rposition(is_human_user_message)?;
            let anchor = &self.messages[prior];
            if let Some(last) = applied.last() {
                if turn_message_provenance(anchor).ok()??.turn_chain_id != last.intent_id {
                    return None;
                }
            } else if anchor["content"].as_str()?.trim() != self.message.trim() {
                return None;
            }
            self.observer_predecessor(
                previous,
                batch_id,
                &self.messages[prior + 1..],
                self.canonical_turn_chain_id.as_deref(),
                window.standalone,
                |_, _| Some(()),
            )
        });
        window.settled_tool_count = calls.len();
        self.user_intents.observer_guidance = Some(window);
    }

    /// Hash only the public projection. Use length-delimited bytes and a batch
    /// identity so that reordering, replacing or dropping a reply changes the proof.
    fn observer_predecessor<'a>(
        &self,
        previous: [u8; 32],
        batch_id: &str,
        messages: &'a [Value],
        chain: Option<&str>,
        standalone: bool,
        mut emit: impl FnMut(&'static str, &'a str) -> Option<()>,
    ) -> Option<[u8; 32]> {
        let mut hash = Sha256::new();
        hash.update(previous);
        hash.update(batch_id.len().to_le_bytes());
        hash.update(batch_id.as_bytes());
        let mut remaining = OBSERVER_DIALOGUE_BYTES;
        for item in messages {
            let role = item["role"].as_str()?;
            if tool_result_count(item) > 0 || item.get("tool_calls").is_some() {
                return None;
            }
            if role != "assistant" {
                if is_runtime_owned_message(item) || matches!(role, "system" | "developer") {
                    continue;
                }
                return None;
            }
            if item["content"].is_null() || item["content"] == "" {
                continue;
            }
            let text = item["content"].as_str()?;
            // Standalone extraction accepts only user text. Omitting this
            // reply could change what a subsequent confirmation means.
            if standalone
                || is_runtime_owned_message(item)
                || super::finalization::terminal_context_omitted(item)
                || turn_message_provenance(item).ok()??.turn_chain_id != chain?
            {
                return None;
            }
            if item
                .get(astra_turn_types::MODEL_REPLAY_ORIGIN_FIELD)
                .is_some()
            {
                let origin = astra_turn_types::model_replay_origin(item)?;
                if origin.runtime_generated
                    || Some(origin.run_id.as_str()) != self.current_run_id.as_deref()
                    || Some(origin.session_id.as_str()) != self.current_session_id.as_deref()
                {
                    return None;
                }
            }
            remaining = remaining.checked_sub(text.len())?;
            if text.trim().is_empty() {
                return None;
            }
            emit("assistant", text)?;
            hash.update([3]);
            hash.update(text.len().to_le_bytes());
            hash.update(text.as_bytes());
        }
        Some(hash.finalize().into())
    }

    pub(crate) fn guided_observer_content<'a>(&'a self, opening: &'a str) -> Option<String> {
        let window = self.user_intents.observer_guidance.as_ref()?;
        let expected = window.fingerprint?;
        let mut intents = self
            .user_intents
            .applied_user_intents()
            .get(window.first_intent..)?;
        if intents.is_empty() || intents.len() > OBSERVER_DIALOGUE_BYTES {
            return None;
        }
        let batch_index = |messages: &[Value], id: &str| {
            messages.iter().rposition(|item| {
                is_human_user_message(item)
                    && turn_message_provenance(item)
                        .is_ok_and(|p| p.is_some_and(|p| p.turn_chain_id == id))
            })
        };
        let last = batch_index(&self.messages, &intents.last()?.intent_id)?;
        let history = &self.messages[..=last];
        let first = batch_index(history, &window.first_batch)?;
        let mut prior = if window.standalone {
            // Prior tool/assistant material is intentionally outside the extractor's input.
            first
        } else {
            history[..first].iter().rposition(is_human_user_message)?
        };
        let mut chain = window.preceding_chain.as_deref();
        let mut fingerprint = [0; 32];
        let mut parts = Vec::new();
        let mut remaining = OBSERVER_DIALOGUE_BYTES;
        let mut push = |role, text: &'a str| -> Option<()> {
            remaining = remaining.checked_sub(text.len())?;
            if text.trim().is_empty() {
                return None;
            }
            parts.push((role, text));
            Some(())
        };
        if !window.standalone {
            push("user", opening)?;
            if history[prior]["content"].as_str()?.trim() != opening.trim() {
                return None;
            }
        }
        for (i, item) in history
            .iter()
            .enumerate()
            .skip(first)
            .filter(|(_, m)| is_human_user_message(m))
        {
            let stamp = turn_message_provenance(item).ok()??;
            let end = intents
                .iter()
                .position(|intent| intent.intent_id == stamp.turn_chain_id)?;
            let (batch, rest) = intents.split_at(end + 1);
            if !window.standalone || i != first {
                fingerprint = self.observer_predecessor(
                    fingerprint,
                    &stamp.turn_chain_id,
                    &history[prior + 1..i],
                    chain,
                    window.standalone,
                    &mut push,
                )?;
            }
            let mut canonical = item["content"].as_str()?;
            for (n, intent) in batch.iter().enumerate() {
                if n != 0 {
                    canonical = canonical.strip_prefix("\n\n")?;
                }
                push("user", &intent.content)?;
                canonical = canonical.strip_prefix(&intent.content)?;
            }
            if !canonical.is_empty() {
                return None;
            }
            chain = Some(&batch.last()?.intent_id);
            intents = rest;
            prior = i;
        }
        if !intents.is_empty() || fingerprint != expected {
            return None;
        }
        let content = if parts.iter().all(|(role, _)| *role == "user") {
            parts
                .into_iter()
                .map(|(_, text)| text)
                .collect::<Vec<_>>()
                .join("\n\n")
        } else {
            serde_json::to_string(&parts).ok()?
        };
        (content.len() <= OBSERVER_DIALOGUE_BYTES).then_some(content)
    }
}
