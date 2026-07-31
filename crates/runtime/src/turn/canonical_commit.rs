use serde_json::Value;

pub(crate) fn canonical_commit_delta(
    prior_message_count: usize,
    had_canonical_head: bool,
    messages: &[Value],
    compaction_rewrote_prefix: bool,
    cancellation_requested: bool,
) -> Result<Option<(astra_turn_types::CanonicalDeltaModeV1, Vec<Vec<Value>>)>, String> {
    let mode = if compaction_rewrote_prefix && had_canonical_head {
        astra_turn_types::CanonicalDeltaModeV1::Replace
    } else {
        astra_turn_types::CanonicalDeltaModeV1::Append
    };
    let changed_messages = if mode == astra_turn_types::CanonicalDeltaModeV1::Replace {
        messages
    } else {
        messages.get(prior_message_count..).ok_or_else(|| {
            "canonical conversation became shorter than the admitted prefix".to_string()
        })?
    };
    let canonical_changed =
        astra_turn_core::prompt_facing::sanitize_canonical_continuation_messages_with_turn_semantics(
            changed_messages.to_vec(),
        )
        .map_err(|error| {
            format!("canonical turn contains invalid user-turn semantics: {error}")
        })?;
    let logical_segments = pack_canonical_turn_segments(canonical_changed);
    if logical_segments.is_empty() {
        return if cancellation_requested {
            Ok(None)
        } else {
            Err("canonical turn produced no committable messages".into())
        };
    }
    Ok(Some((mode, logical_segments)))
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
