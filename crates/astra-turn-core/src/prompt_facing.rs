//! Prompt-facing conversation message normalization.
//!
//! Display projections omit execution detail. Canonical continuation retains
//! complete tool call/result groups as model evidence; only the context
//! optimizer decides when pressure justifies compaction. Do not substitute a
//! display projection for canonical history.

use crate::conversation_log::SessionStateCompact;
use astra_turn_types::{
    RuntimeMessageDelivery, is_runtime_owned_message, runtime_message_delivery,
    runtime_owned_message,
};
use serde_json::{Value, json};

const MAX_PROMPT_FACING_MESSAGES: usize = 40;
const RUNTIME_RECAP_HEADING: &str = "[Session runtime recap]";

pub fn extract_text_content(msg: &Value) -> Option<String> {
    if let Some(s) = msg.get("content").and_then(|c| c.as_str()) {
        return Some(s.to_string());
    }
    if let Some(arr) = msg.get("content").and_then(|c| c.as_array()) {
        let texts: Vec<&str> = arr
            .iter()
            .filter_map(|block| {
                let kind = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match kind {
                    "text" | "output_text" => block
                        .get("text")
                        .or_else(|| block.get("content"))
                        .and_then(|t| t.as_str()),
                    _ => None,
                }
            })
            .collect();
        if !texts.is_empty() {
            return Some(texts.join("\n"));
        }
    }
    None
}

pub fn sanitize_prompt_facing_messages(messages: Vec<Value>) -> Vec<Value> {
    sanitize_prompt_facing_messages_impl(messages, false)
}

/// Canonical continuation/resume projection that retains validated user-turn
/// semantics between runtime boundaries. Provider/session-memory projections
/// use [`sanitize_prompt_facing_messages`] and therefore never receive this
/// internal metadata.
pub fn sanitize_prompt_facing_messages_with_turn_semantics(messages: Vec<Value>) -> Vec<Value> {
    sanitize_prompt_facing_messages_impl(messages, true)
}

/// Strict canonical continuation projection. Invalid producer-owned metadata
/// is returned to the restore boundary instead of being rewritten as absence.
pub fn try_sanitize_prompt_facing_messages_with_turn_semantics(
    messages: Vec<Value>,
) -> Result<Vec<Value>, astra_turn_types::UserTurnSemanticsError> {
    for message in &messages {
        if message
            .get(astra_turn_types::USER_TURN_SEMANTICS_FIELD)
            .is_some()
        {
            astra_turn_types::user_turn_semantics(message)?;
        }
    }
    Ok(sanitize_prompt_facing_messages_with_turn_semantics(
        messages,
    ))
}

fn sanitize_prompt_facing_messages_impl(
    messages: Vec<Value>,
    preserve_turn_semantics: bool,
) -> Vec<Value> {
    let mut out = Vec::new();
    let start = latest_compaction_boundary_start(&messages).unwrap_or(0);
    let mut has_user_context = false;

    for msg in messages.into_iter().skip(start) {
        if msg.get("_compact_boundary").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        if is_runtime_owned_message(&msg) {
            continue;
        }
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
        if role == "tool" {
            continue;
        }
        if !matches!(role, "user" | "assistant" | "system") {
            continue;
        }

        if role == "assistant" {
            if contains_tool_call_frame(&msg) {
                continue;
            }
        }

        let Some(raw_content) = extract_text_content(&msg) else {
            continue;
        };
        let Some(content) = prompt_facing_content_for_role(role, &raw_content) else {
            continue;
        };
        if content.trim().is_empty() {
            continue;
        }
        if role == "assistant" && !has_user_context {
            continue;
        }
        let mut projected = json!({
            "role": role,
            "content": content,
        });
        if preserve_turn_semantics && role == "user" {
            match astra_turn_types::user_turn_semantics(&msg) {
                Ok(Some(semantics)) => {
                    astra_turn_types::mark_user_turn_semantics(&mut projected, semantics);
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        "dropping invalid user-turn semantics from prompt-facing projection"
                    );
                }
            }
        }
        out.push(projected);
        if role == "user" {
            has_user_context = true;
        }
    }

    trim_to_recent_messages(out)
}

pub fn sanitize_prompt_facing_messages_with_state(
    messages: Vec<Value>,
    state: &SessionStateCompact,
) -> Result<Vec<Value>, astra_turn_types::UserTurnSemanticsError> {
    // This boundary feeds canonical continuation/resume state. Preserve typed
    // objective metadata while provider-facing sanitizers continue to strip it.
    let mut out = try_sanitize_prompt_facing_messages_with_turn_semantics(messages)?;
    if let Some(recap) = runtime_recap_message(state) {
        out.push(recap);
    }
    Ok(trim_to_recent_messages(out))
}

/// Project canonical runtime history into a continuation-safe conversation.
///
/// Unlike the compact prompt-facing transcript above, a continuation is fed
/// back through the context optimizer. It therefore retains completed tool
/// call/result groups as model evidence instead of deleting them before the
/// optimizer can make a pressure-aware decision. Durable append-only required
/// controls retain their exact position and provenance for provider-prefix
/// reconstruction; other runtime-owned controls and orphaned tool frames are
/// still removed at this trust boundary.
pub fn sanitize_canonical_continuation_messages_with_turn_semantics(
    messages: Vec<Value>,
) -> Result<Vec<Value>, astra_turn_types::UserTurnSemanticsError> {
    sanitize_canonical_continuation_messages_impl(messages, false, false)
}

/// Project a current-turn delta for canonical commit, independent of outcome.
///
/// Tiered compaction has already removed the compacted middle from this
/// vector. Only an explicit typed replacement in the retained tail supersedes
/// the protected head. Refinements, corrections, continuations, and unjudged
/// user messages retain the objective they depend on. `has_prior_user_context`
/// comes only from the immutable admitted prefix, not from runtime scaffolding;
/// it lets a resumed suffix continue that user turn without copying its anchor.
pub fn sanitize_canonical_turn_delta_with_turn_semantics(
    messages: Vec<Value>,
    has_prior_user_context: bool,
) -> Result<Vec<Value>, astra_turn_types::UserTurnSemanticsError> {
    sanitize_canonical_continuation_messages_impl(messages, true, has_prior_user_context)
}

fn sanitize_canonical_continuation_messages_impl(
    messages: Vec<Value>,
    preserve_compacted_head: bool,
    mut has_user_context: bool,
) -> Result<Vec<Value>, astra_turn_types::UserTurnSemanticsError> {
    for message in &messages {
        if message
            .get(astra_turn_types::USER_TURN_SEMANTICS_FIELD)
            .is_some()
        {
            astra_turn_types::user_turn_semantics(message)?;
        }
    }

    let start = if preserve_compacted_head {
        compacted_canonical_turn_start(&messages)?
    } else {
        latest_compaction_boundary_start(&messages).unwrap_or(0)
    };
    let messages = messages
        .into_iter()
        .skip(start)
        .filter_map(|mut message| {
            let append_only_required = runtime_message_delivery(&message)
                == Some(RuntimeMessageDelivery::AppendOnlyRequiredContext);
            let keep = message.get("_compact_boundary").and_then(Value::as_bool) != Some(true)
                && (append_only_required || !is_runtime_owned_message(&message));
            if !keep {
                return None;
            }
            astra_turn_types::clear_turn_message_provenance(&mut message);
            Some(message)
        })
        .collect::<Vec<_>>();

    let mut out = Vec::new();
    let mut index = 0;
    while index < messages.len() {
        let message = &messages[index];
        if runtime_message_delivery(message)
            == Some(RuntimeMessageDelivery::AppendOnlyRequiredContext)
        {
            out.push(message.clone());
            index += 1;
            continue;
        }
        let role = message.get("role").and_then(Value::as_str).unwrap_or("");
        match role {
            "user" | "system" => {
                if let Some(projected) = canonical_text_message(message, role, role == "user") {
                    out.push(projected);
                    has_user_context |= role == "user";
                }
                index += 1;
            }
            "assistant" if contains_tool_call_frame(message) => {
                let block_end = consecutive_tool_block_end(&messages, index + 1);
                if has_user_context {
                    append_complete_tool_group(&mut out, message, &messages[index + 1..block_end]);
                }
                index = block_end;
            }
            "assistant" => {
                if has_user_context
                    && let Some(projected) = canonical_text_message(message, role, false)
                {
                    out.push(projected);
                }
                index += 1;
            }
            // Tool messages are admitted only by `append_complete_tool_group`,
            // which guarantees that every retained result has a matching call.
            _ => index += 1,
        }
    }
    Ok(out)
}

/// Recovery projection for continuation sources that may contain one corrupt
/// producer-owned turn-semantics field.
///
/// Invalid metadata is removed per message before the normal strict
/// continuation sanitizer runs. The raw message vector is never returned:
/// compaction boundaries, runtime-owned controls, and tool-call pairing remain
/// enforced even when one metadata field is damaged.
pub fn recover_canonical_continuation_messages_with_turn_semantics(
    mut messages: Vec<Value>,
) -> (Vec<Value>, usize) {
    let mut invalid_turn_semantics_dropped = 0;
    for message in &mut messages {
        let has_semantics = message
            .get(astra_turn_types::USER_TURN_SEMANTICS_FIELD)
            .is_some();
        if has_semantics && astra_turn_types::user_turn_semantics(message).is_err() {
            if let Some(object) = message.as_object_mut() {
                object.remove(astra_turn_types::USER_TURN_SEMANTICS_FIELD);
                invalid_turn_semantics_dropped += 1;
            }
        }
    }

    let sanitized = sanitize_canonical_continuation_messages_with_turn_semantics(messages)
        .expect("invalid turn semantics were removed before strict continuation sanitization");
    (sanitized, invalid_turn_semantics_dropped)
}

pub fn sanitize_canonical_continuation_messages_with_state(
    messages: Vec<Value>,
    state: &SessionStateCompact,
) -> Result<Vec<Value>, astra_turn_types::UserTurnSemanticsError> {
    let mut out = sanitize_canonical_continuation_messages_with_turn_semantics(messages)?;
    if let Some(recap) = runtime_recap_message(state) {
        out.push(recap);
    }
    Ok(out)
}

fn canonical_text_message(
    message: &Value,
    role: &str,
    preserve_turn_semantics: bool,
) -> Option<Value> {
    let raw_content = extract_text_content(message)?;
    let content = prompt_facing_content_for_role(role, &raw_content)?;
    let mut projected = json!({
        "role": role,
        "content": content,
    });
    if preserve_turn_semantics
        && let Ok(Some(semantics)) = astra_turn_types::user_turn_semantics(message)
    {
        astra_turn_types::mark_user_turn_semantics(&mut projected, semantics);
    }
    Some(projected)
}

fn consecutive_tool_block_end(messages: &[Value], mut index: usize) -> usize {
    while messages
        .get(index)
        .and_then(|message| message.get("role"))
        .and_then(Value::as_str)
        == Some("tool")
    {
        index += 1;
    }
    index
}

fn append_complete_tool_group(out: &mut Vec<Value>, assistant: &Value, tools: &[Value]) {
    let Some(calls) = assistant.get("tool_calls").and_then(Value::as_array) else {
        if let Some(projected) = canonical_text_message(assistant, "assistant", false) {
            out.push(projected);
        }
        return;
    };

    let mut results_by_id = std::collections::HashMap::<&str, &Value>::new();
    for tool in tools {
        if let Some(id) = tool.get("tool_call_id").and_then(Value::as_str)
            && !id.is_empty()
        {
            results_by_id.entry(id).or_insert(tool);
        }
    }

    let mut matched_calls = Vec::new();
    let mut matched_results = Vec::new();
    let mut seen_ids = std::collections::HashSet::new();
    for call in calls {
        let Some(id) = call.get("id").and_then(Value::as_str) else {
            continue;
        };
        if id.is_empty() || !seen_ids.insert(id) {
            continue;
        }
        let Some(result) = results_by_id.get(id) else {
            continue;
        };
        matched_calls.push(call.clone());
        matched_results.push(project_tool_result(result, id));
    }

    if matched_calls.is_empty() {
        if let Some(projected) = canonical_text_message(assistant, "assistant", false) {
            out.push(projected);
        }
        return;
    }

    let mut projected = serde_json::Map::from_iter([
        ("role".to_string(), Value::String("assistant".to_string())),
        ("tool_calls".to_string(), Value::Array(matched_calls)),
    ]);
    if let Some(content) = assistant.get("content") {
        projected.insert("content".to_string(), content.clone());
    }
    if let Some(reasoning) = assistant.get("reasoning_content") {
        projected.insert("reasoning_content".to_string(), reasoning.clone());
    }
    out.push(Value::Object(projected));
    out.extend(matched_results);
}

fn project_tool_result(tool: &Value, tool_call_id: &str) -> Value {
    let content = Value::String(canonical_tool_result_content(
        tool.get("content"),
        tool_call_id,
    ));
    let mut projected = serde_json::Map::from_iter([
        ("role".to_string(), Value::String("tool".to_string())),
        (
            "tool_call_id".to_string(),
            Value::String(tool_call_id.to_string()),
        ),
        ("content".to_string(), content),
    ]);
    if let Some(name) = tool.get("name").and_then(Value::as_str)
        && !name.is_empty()
    {
        projected.insert("name".to_string(), Value::String(name.to_string()));
    }
    crate::tool::result::advisory::set_advisories(
        &mut projected,
        &crate::tool::result::advisory::advisories(tool.as_object()),
    );
    Value::Object(projected)
}

fn canonical_tool_result_content(content: Option<&Value>, tool_call_id: &str) -> String {
    let Some(content) = content else {
        return String::new();
    };
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks)
            if !blocks.is_empty()
                && blocks
                    .iter()
                    .all(|block| is_tool_result_block_for(block, tool_call_id)) =>
        {
            blocks
                .iter()
                .filter_map(|block| block.get("content"))
                .map(canonical_tool_result_payload)
                .collect::<Vec<_>>()
                .join("\n")
        }
        other => canonical_tool_result_payload(other),
    }
}

fn canonical_tool_result_payload(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) if !blocks.is_empty() && blocks.iter().all(is_text_block) => blocks
            .iter()
            .filter_map(|block| {
                block
                    .get("text")
                    .or_else(|| block.get("content"))
                    .and_then(Value::as_str)
            })
            .collect::<Vec<_>>()
            .join(""),
        other => serde_json::to_string(other).unwrap_or_else(|_| other.to_string()),
    }
}

fn is_tool_result_block_for(value: &Value, tool_call_id: &str) -> bool {
    value.get("type").and_then(Value::as_str) == Some("tool_result")
        && value.get("tool_use_id").and_then(Value::as_str) == Some(tool_call_id)
        && value.get("content").is_some()
}

fn is_text_block(value: &Value) -> bool {
    matches!(
        value.get("type").and_then(Value::as_str),
        Some("text" | "output_text")
    ) && value
        .get("text")
        .or_else(|| value.get("content"))
        .and_then(Value::as_str)
        .is_some()
}

pub fn sanitize_user_visible_messages(messages: Vec<Value>) -> Vec<Value> {
    messages
        .into_iter()
        .filter_map(|msg| user_visible_message(&msg))
        .collect()
}

pub fn user_visible_message(msg: &Value) -> Option<Value> {
    if is_runtime_owned_message(msg) {
        return None;
    }
    let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
    if !matches!(role, "user" | "assistant" | "system") {
        return None;
    }
    let raw_content = extract_text_content(msg)?;
    let content = prompt_facing_content_for_role(role, &raw_content)?;
    let content = sanitize_user_visible_text(&content);
    if content.trim().is_empty() {
        return None;
    }
    Some(json!({
        "role": role,
        "content": content,
    }))
}

pub fn runtime_recap_message(state: &SessionStateCompact) -> Option<Value> {
    let mut lines = Vec::new();
    if !state.recent_tools.is_empty() {
        lines.push(format!("Recent tools: {}", state.recent_tools.join(", ")));
    }
    // Deferred activation is represented structurally by the injected tool
    // schema and retained as recovery sidecar state. Repeating it as prose
    // creates a second, stale-able source of truth and needlessly changes the
    // provider cache prefix.
    // Checkpoint headroom is recovery diagnostics, not model policy. In
    // particular, `budget_remaining_tokens` historically measured assembled
    // context headroom while `budget_remaining_rounds` measured agent-loop
    // rounds. Rendering those unrelated values as one "budget" made a normal
    // `tokens=0, rounds=N` checkpoint look like an execution stop signal and
    // allowed a local accounting mismatch to steer the next turn. Runtime
    // budget guidance is injected from live authoritative state instead.
    if state.consecutive_ctx_errors > 0 {
        lines.push(format!(
            "Context-window recovery attempts: {}",
            state.consecutive_ctx_errors
        ));
    }
    if let Some(delegation) = &state.delegation {
        lines.push(format!(
            "Delegation: id={}, pattern={}, completed_sub_runs={}",
            delegation.id,
            delegation.pattern,
            delegation.completed_sub_runs.len()
        ));
    }
    if lines.is_empty() {
        return None;
    }
    Some(runtime_owned_message(
        "system",
        format!("{RUNTIME_RECAP_HEADING}\n{}", lines.join("\n")),
        RuntimeMessageDelivery::Projection,
    ))
}

fn latest_compaction_boundary_start(messages: &[Value]) -> Option<usize> {
    messages.iter().rposition(|message| {
        message.get("_compact_boundary").and_then(Value::as_bool) == Some(true)
    })
}

fn compacted_canonical_turn_start(
    messages: &[Value],
) -> Result<usize, astra_turn_types::UserTurnSemanticsError> {
    let Some(boundary_index) = latest_compaction_boundary_start(messages) else {
        return Ok(0);
    };
    for (index, message) in messages.iter().enumerate().skip(boundary_index + 1).rev() {
        if !is_runtime_owned_message(message)
            && message.get("role").and_then(Value::as_str) == Some("user")
            && canonical_text_message(message, "user", false).is_some()
            && astra_turn_types::user_turn_semantics(message)?.is_some_and(|semantics| {
                semantics.objective_relation == astra_turn_types::ObjectiveRelation::Replace
            })
        {
            return Ok(index);
        }
    }
    Ok(0)
}

fn prompt_facing_content_for_role(role: &str, content: &str) -> Option<String> {
    let _ = role;
    let content = content.trim().to_string();
    if content.trim().is_empty() {
        return None;
    }
    Some(content)
}

fn contains_tool_call_frame(msg: &Value) -> bool {
    if let Some(tool_calls) = msg.get("tool_calls").and_then(|v| v.as_array()) {
        return !tool_calls.is_empty();
    }
    if let Some(content) = msg.get("content").and_then(|c| c.as_array()) {
        return content
            .iter()
            .any(|block| block.get("type").and_then(|t| t.as_str()) == Some("tool_use"));
    }
    false
}

fn sanitize_user_visible_text(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut chars = content.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            strip_escape_sequence(&mut chars);
            continue;
        }
        if ch.is_control() && !matches!(ch, '\n' | '\r' | '\t') {
            continue;
        }
        out.push(ch);
    }
    out.trim().to_string()
}

fn strip_escape_sequence<I>(chars: &mut std::iter::Peekable<I>)
where
    I: Iterator<Item = char>,
{
    match chars.peek().copied() {
        Some('[') => {
            chars.next();
            for ch in chars.by_ref() {
                if ('@'..='~').contains(&ch) {
                    break;
                }
            }
        }
        Some(']') => {
            chars.next();
            while let Some(ch) = chars.next() {
                if ch == '\u{7}' {
                    break;
                }
                if ch == '\u{1b}' && chars.peek().copied() == Some('\\') {
                    chars.next();
                    break;
                }
            }
        }
        Some(_) => {
            chars.next();
        }
        None => {}
    }
}

fn trim_to_recent_messages(mut messages: Vec<Value>) -> Vec<Value> {
    if messages.len() <= MAX_PROMPT_FACING_MESSAGES {
        return messages;
    }
    messages.drain(0..messages.len() - MAX_PROMPT_FACING_MESSAGES);
    messages
}

#[cfg(test)]
mod tests {
    use super::{
        recover_canonical_continuation_messages_with_turn_semantics, runtime_recap_message,
        sanitize_canonical_continuation_messages_with_state,
        sanitize_canonical_turn_delta_with_turn_semantics, sanitize_prompt_facing_messages,
        sanitize_prompt_facing_messages_with_state, sanitize_user_visible_messages,
    };
    use crate::conversation_log::{DelegationCompact, SessionStateCompact};
    use astra_turn_types::{
        RuntimeAuthorityLifetime, RuntimeMessageDelivery, mark_append_only_required_context,
        runtime_owned_message,
    };
    use serde_json::{Value, json};

    #[test]
    fn compresses_completed_tool_pair_and_drops_reasoning_only_messages() {
        let messages = vec![
            json!({"role": "user", "content": "fix it"}),
            json!({"role": "assistant", "reasoning_content": "I should inspect files"}),
            json!({"role": "assistant", "tool_calls": [{"id": "c1", "function": {"name": "read_file"}}]}),
            json!({"role": "tool", "tool_call_id": "c1", "content": "file"}),
            json!({"role": "assistant", "content": "done"}),
        ];

        let got = sanitize_prompt_facing_messages(messages);

        assert_eq!(
            got,
            vec![
                json!({"role": "user", "content": "fix it"}),
                json!({"role": "assistant", "content": "done"}),
            ]
        );
    }

    #[test]
    fn canonical_continuation_keeps_complete_tool_evidence_as_an_atomic_group() {
        let runtime_scaffold = runtime_owned_message(
            "user",
            "retry this tool round",
            RuntimeMessageDelivery::EphemeralControl,
        );
        let messages = vec![
            json!({"role": "user", "content": "inspect the manifest"}),
            runtime_scaffold,
            json!({
                "role": "assistant",
                "content": "",
                "reasoning_content": "I should inspect it",
                "tool_calls": [
                    {
                        "id": "call-1",
                        "type": "function",
                        "function": {
                            "name": "read_file",
                            "arguments": "{\"path\":\"Cargo.toml\"}"
                        }
                    }
                ]
            }),
            json!({
                "role": "tool",
                "tool_call_id": "call-1",
                "content": "[package]\nname = \"astra\"",
                "_astra_tool_result_advisories": ["inspect the relevant section"]
            }),
            json!({"role": "assistant", "content": "manifest inspected"}),
        ];

        let got = sanitize_canonical_continuation_messages_with_state(
            messages,
            &SessionStateCompact::default(),
        )
        .expect("valid canonical history");

        assert_eq!(got.len(), 4);
        assert_eq!(got[0]["role"], "user");
        assert_eq!(got[1]["tool_calls"][0]["id"], "call-1");
        assert_eq!(got[2]["role"], "tool");
        assert_eq!(got[2]["content"], "[package]\nname = \"astra\"");
        assert_eq!(
            got[2]["_astra_tool_result_advisories"],
            json!(["inspect the relevant section"])
        );
        let mut wire_tool = got[2].clone();
        crate::tool::result::advisory::project_advisories(&mut wire_tool);
        assert!(
            wire_tool["content"]
                .as_str()
                .unwrap()
                .contains("inspect the relevant section")
        );
        assert_eq!(got[3]["content"], "manifest inspected");
        assert!(
            got.iter()
                .all(|message| message["content"] != "retry this tool round"),
            "runtime-owned control messages must not cross a continuation boundary"
        );
    }

    fn typed_user(content: &str, relation: astra_turn_types::ObjectiveRelation) -> Value {
        let mut message = json!({"role": "user", "content": content});
        astra_turn_types::mark_user_turn_semantics(
            &mut message,
            astra_turn_types::UserTurnSemantics::new(relation, None),
        );
        message
    }

    #[test]
    fn compacted_current_turn_preserves_objective_until_typed_replacement() {
        use astra_turn_types::ObjectiveRelation;
        for relation in [
            None,
            Some(ObjectiveRelation::Unknown),
            Some(ObjectiveRelation::Acknowledge),
            Some(ObjectiveRelation::Continue),
            Some(ObjectiveRelation::Refine),
            Some(ObjectiveRelation::Correct),
            Some(ObjectiveRelation::Replace),
        ] {
            let head = typed_user("initial objective", ObjectiveRelation::Replace);
            // Identical text deliberately cannot tell the projection what to do.
            let tail = relation.map_or_else(
                || json!({"role": "user", "content": "follow-up input"}),
                |relation| typed_user("follow-up input", relation),
            );
            let answer = json!({"role": "assistant", "content": "done"});
            let messages = vec![
                head.clone(),
                json!({"role": "system", "content": "boundary", "_compact_boundary": true}),
                tail.clone(),
                answer.clone(),
            ];
            let mut expected = vec![tail, answer];
            if relation != Some(ObjectiveRelation::Replace) {
                expected.insert(0, head);
            }
            let projected = sanitize_canonical_turn_delta_with_turn_semantics(messages, false)
                .expect("valid compacted turn");
            assert_eq!(projected, expected, "relation: {relation:?}");
        }
    }

    #[test]
    fn compacted_current_turn_starts_at_last_explicit_replacement() {
        use astra_turn_types::ObjectiveRelation;
        let replacement = typed_user("replacement objective", ObjectiveRelation::Replace);
        let refinement = typed_user("additional constraint", ObjectiveRelation::Refine);
        let messages = vec![
            typed_user("obsolete head", ObjectiveRelation::Replace),
            json!({"role": "system", "content": "boundary", "_compact_boundary": true}),
            typed_user("obsolete refinement", ObjectiveRelation::Refine),
            typed_user("superseded replacement", ObjectiveRelation::Replace),
            json!({"role": "assistant", "content": "obsolete answer"}),
            replacement.clone(),
            refinement.clone(),
        ];
        let projected = sanitize_canonical_turn_delta_with_turn_semantics(messages, false)
            .expect("valid compacted turn");
        assert_eq!(projected, vec![replacement, refinement]);
    }

    #[test]
    fn compacted_current_turn_ignores_runtime_owned_replacement() {
        use astra_turn_types::ObjectiveRelation;
        let head = typed_user("initial objective", ObjectiveRelation::Replace);
        let mut control =
            runtime_owned_message("user", "control", RuntimeMessageDelivery::EphemeralControl);
        astra_turn_types::mark_user_turn_semantics(
            &mut control,
            astra_turn_types::UserTurnSemantics::new(ObjectiveRelation::Replace, None),
        );
        let messages = vec![
            head.clone(),
            json!({"role": "system", "content": "boundary", "_compact_boundary": true}),
            control,
            typed_user("  ", ObjectiveRelation::Replace),
        ];
        let projected = sanitize_canonical_turn_delta_with_turn_semantics(messages, false)
            .expect("valid compacted turn");
        assert_eq!(projected, vec![head]);
    }

    #[test]
    fn compacted_current_turn_rejects_corrupt_semantics() {
        let messages = vec![
            json!({"role": "user", "content": "objective"}),
            json!({"role": "system", "content": "boundary", "_compact_boundary": true}),
            json!({
                "role": "user", "content": "input",
                (astra_turn_types::USER_TURN_SEMANTICS_FIELD): {
                    "schema_version": 1, "objective_relation": "invalid",
                },
            }),
        ];
        assert!(sanitize_canonical_turn_delta_with_turn_semantics(messages, false).is_err());
    }

    #[test]
    fn canonical_continuation_preserves_append_only_authority_without_making_it_user_visible() {
        let mut authority = json!({
            "role": "user",
            "content": "<runtime-authority-frame>\nsettlement\n</runtime-authority-frame>"
        });
        mark_append_only_required_context(
            &mut authority,
            "final_answer_settlement",
            RuntimeAuthorityLifetime::NextAssistantDecision,
        );
        let messages = vec![
            json!({"role": "user", "content": "finish the change"}),
            authority.clone(),
            json!({"role": "assistant", "content": "I need one verification"}),
        ];

        let continuation = sanitize_canonical_continuation_messages_with_state(
            messages.clone(),
            &SessionStateCompact::default(),
        )
        .expect("valid canonical history");
        assert_eq!(continuation[1], authority);
        let completed = sanitize_canonical_turn_delta_with_turn_semantics(messages.clone(), false)
            .expect("valid completed history");
        assert_eq!(completed[1], authority);
        assert_eq!(sanitize_prompt_facing_messages(messages).len(), 2);
    }

    #[test]
    fn canonical_continuation_normalizes_structured_tool_results_to_provider_neutral_text() {
        let messages = vec![
            json!({"role": "user", "content": "inspect"}),
            json!({
                "role": "assistant",
                "tool_calls": [
                    {
                        "id": "object-result",
                        "type": "function",
                        "function": {"name": "read_file", "arguments": "{}"}
                    },
                    {
                        "id": "annotated-result",
                        "type": "function",
                        "function": {"name": "grep", "arguments": "{}"}
                    },
                    {
                        "id": "empty-array-result",
                        "type": "function",
                        "function": {"name": "query", "arguments": "{}"}
                    },
                    {
                        "id": "lookalike-result",
                        "type": "function",
                        "function": {"name": "query", "arguments": "{}"}
                    }
                ]
            }),
            json!({
                "role": "tool",
                "tool_call_id": "object-result",
                "content": {"error": "boom", "code": 42}
            }),
            json!({
                "role": "tool",
                "tool_call_id": "annotated-result",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "annotated-result",
                    "content": [{"type": "text", "text": "durable evidence"}],
                    "cache_control": {"type": "ephemeral"}
                }]
            }),
            json!({
                "role": "tool",
                "tool_call_id": "empty-array-result",
                "content": []
            }),
            json!({
                "role": "tool",
                "tool_call_id": "lookalike-result",
                "content": [{"type": "tool_result", "value": {"row": 1}}]
            }),
        ];

        let got = sanitize_canonical_continuation_messages_with_state(
            messages,
            &SessionStateCompact::default(),
        )
        .expect("valid canonical history");

        assert_eq!(
            got[2]["content"].as_str(),
            Some("{\"code\":42,\"error\":\"boom\"}"),
            "provider-neutral history must keep structured JSON as readable text"
        );
        assert_eq!(
            got[3]["content"].as_str(),
            Some("durable evidence"),
            "provider cache annotations must not leak across continuation/provider boundaries"
        );
        assert_eq!(
            got[4]["content"].as_str(),
            Some("[]"),
            "unknown structured results, including empty arrays, must round-trip as stable JSON text"
        );
        assert_eq!(
            got[5]["content"].as_str(),
            Some("[{\"type\":\"tool_result\",\"value\":{\"row\":1}}]"),
            "a discriminant lookalike without the provider envelope must remain ordinary JSON data"
        );
    }

    #[test]
    fn canonical_continuation_does_not_relabel_a_mismatched_provider_tool_envelope() {
        let messages = vec![
            json!({"role": "user", "content": "inspect"}),
            json!({
                "role": "assistant",
                "tool_calls": [{
                    "id": "outer-call",
                    "type": "function",
                    "function": {"name": "read_file", "arguments": "{}"}
                }]
            }),
            json!({
                "role": "tool",
                "tool_call_id": "outer-call",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "different-call",
                    "content": "evidence owned by a different call"
                }]
            }),
        ];

        let got = sanitize_canonical_continuation_messages_with_state(
            messages,
            &SessionStateCompact::default(),
        )
        .expect("valid canonical history");

        assert_eq!(
            got[2]["content"].as_str(),
            Some(
                "[{\"content\":\"evidence owned by a different call\",\"tool_use_id\":\"different-call\",\"type\":\"tool_result\"}]"
            ),
            "a provider envelope may be unwrapped only when its tool identity matches the canonical result"
        );
    }

    #[test]
    fn recovery_drops_corrupt_semantics_and_pre_boundary_history() {
        let messages = vec![
            json!({"role": "user", "content": "stale objective"}),
            json!({"role": "system", "content": "boundary", "_compact_boundary": true}),
            runtime_owned_message(
                "system",
                "runtime-only control",
                RuntimeMessageDelivery::EphemeralControl,
            ),
            json!({
                "role": "user",
                "content": "current objective",
                (astra_turn_types::USER_TURN_SEMANTICS_FIELD): {
                    "schema_version": "invalid",
                    "objective_relation": "replace"
                }
            }),
            json!({"role": "tool", "tool_call_id": "orphan", "content": "orphan result"}),
            json!({"role": "assistant", "content": "current answer"}),
        ];

        let (got, invalid_turn_semantics_dropped) =
            recover_canonical_continuation_messages_with_turn_semantics(messages);

        assert_eq!(invalid_turn_semantics_dropped, 1);
        assert_eq!(
            got,
            vec![
                json!({"role": "user", "content": "current objective"}),
                json!({"role": "assistant", "content": "current answer"}),
            ]
        );
    }

    #[test]
    fn canonical_continuation_drops_orphaned_tool_frames_without_splitting_valid_pairs() {
        let messages = vec![
            json!({"role": "tool", "tool_call_id": "orphan-result", "content": "ignore me"}),
            json!({"role": "user", "content": "inspect"}),
            json!({
                "role": "assistant",
                "tool_calls": [
                    {"id": "paired", "type": "function", "function": {"name": "read_file", "arguments": "{}"}},
                    {"id": "missing", "type": "function", "function": {"name": "read_file", "arguments": "{}"}}
                ]
            }),
            json!({"role": "tool", "tool_call_id": "paired", "content": "durable evidence"}),
            json!({"role": "assistant", "content": "done"}),
        ];

        let got = sanitize_canonical_continuation_messages_with_state(
            messages,
            &SessionStateCompact::default(),
        )
        .expect("valid canonical history");

        assert_eq!(got.len(), 4);
        assert_eq!(got[1]["tool_calls"].as_array().unwrap().len(), 1);
        assert_eq!(got[1]["tool_calls"][0]["id"], "paired");
        assert_eq!(got[2]["tool_call_id"], "paired");
        assert!(
            got.iter().all(|message| {
                message["tool_call_id"] != "orphan-result"
                    && message
                        .get("tool_calls")
                        .and_then(Value::as_array)
                        .is_none_or(|calls| calls.iter().all(|call| call["id"] != "missing"))
            }),
            "projection must retain only provider-valid complete tool groups"
        );
    }

    #[test]
    fn tool_results_do_not_become_prompt_facing_system_messages() {
        let messages = vec![
            json!({"role": "user", "content": "inspect"}),
            json!({"role": "assistant", "tool_calls": [
                {"id": "c1", "function": {"name": " read_file "}},
                {"id": "c2", "function": {"name": "  "}}
            ]}),
            json!({"role": "tool", "tool_call_id": "c1", "content": "file"}),
            json!({"role": "tool", "tool_call_id": "c2", "content": "blank-name result"}),
        ];

        let got = sanitize_prompt_facing_messages(messages);

        assert_eq!(got, vec![json!({"role": "user", "content": "inspect"})]);
    }

    #[test]
    fn drops_assistant_only_visible_text_without_user_context() {
        let messages = vec![
            json!({"role": "assistant", "content": "Earlier context compacted."}),
            json!({"role": "system", "content": "status note"}),
            json!({"role": "assistant", "content": "orphan answer"}),
            json!({"role": "user", "content": "continue"}),
            json!({"role": "assistant", "content": "ok"}),
        ];

        let got = sanitize_prompt_facing_messages(messages);

        assert_eq!(
            got,
            vec![
                json!({"role": "system", "content": "status note"}),
                json!({"role": "user", "content": "continue"}),
                json!({"role": "assistant", "content": "ok"}),
            ]
        );
    }

    #[test]
    fn compaction_boundary_replaces_older_goal_stack() {
        let messages = vec![
            json!({"role": "user", "content": "3 agents review everything"}),
            json!({"role": "assistant", "content": "review summary"}),
            json!({"role": "system", "content": "arbitrary boundary text", "_compact_boundary": true}),
            json!({"role": "user", "content": "不要review啊！"}),
            json!({"role": "assistant", "reasoning_content": "Maybe review anyway"}),
            json!({"role": "assistant", "content": "明白，不做 review。"}),
        ];

        let got = sanitize_prompt_facing_messages(messages);

        assert_eq!(got.len(), 2);
        assert_eq!(got[0]["content"], "不要review啊！");
        assert_eq!(got[1]["content"], "明白，不做 review。");
        assert!(
            got.iter()
                .all(|msg| !msg["content"].as_str().unwrap_or("").contains("3 agents"))
        );
    }

    #[test]
    fn orphan_tool_result_and_unresolved_tool_call_do_not_reach_prompt() {
        let messages = vec![
            json!({"role": "user", "content": "continue"}),
            json!({"role": "tool", "tool_call_id": "missing", "content": "stale"}),
            json!({"role": "assistant", "content": "I will run bash.", "tool_calls": [{"id": "dangling", "function": {"name": "bash"}}]}),
            json!({"role": "assistant", "content": "visible"}),
        ];

        let got = sanitize_prompt_facing_messages(messages);

        assert_eq!(
            got,
            vec![
                json!({"role": "user", "content": "continue"}),
                json!({"role": "assistant", "content": "visible"}),
            ]
        );
    }

    #[test]
    fn runtime_ownership_not_text_controls_prompt_projection() {
        let ordinary = json!({"role": "user", "content": "Tools used: literal user text"});
        let owned = runtime_owned_message(
            "user",
            "arbitrary owned payload",
            RuntimeMessageDelivery::EphemeralControl,
        );
        let messages = vec![
            ordinary.clone(),
            owned,
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "skill-auto-route-analyze-session",
                    "type": "function",
                    "function": {
                        "name": "skill",
                        "arguments": "{\"skill_name\":\"analyze-session\",\"task\":\"我说过的所有话，还有回复\"}"
                    }
                }]
            }),
            json!({
                "role": "tool",
                "tool_call_id": "skill-auto-route-analyze-session",
                "content": "<skill-loaded name=\"analyze-session\"/>\nUse this workflow."
            }),
            json!({"role": "assistant", "content": "done"}),
        ];

        let got = sanitize_prompt_facing_messages(messages);

        assert_eq!(
            got,
            vec![ordinary, json!({"role": "assistant", "content": "done"}),]
        );
    }

    #[test]
    fn anthropic_style_tool_blocks_are_compacted_without_provider_frames() {
        let messages = vec![
            json!({"role": "user", "content": "inspect"}),
            json!({
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "I will inspect."},
                    {"type": "tool_use", "id": "toolu_1", "name": "read_file", "input": {"path": "a.rs"}},
                ],
            }),
            json!({
                "role": "user",
                "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": [{"type": "text", "text": "line 1"}]},
                ],
            }),
            json!({"role": "assistant", "content": "done"}),
        ];

        let got = sanitize_prompt_facing_messages(messages);

        assert_eq!(
            got,
            vec![
                json!({"role": "user", "content": "inspect"}),
                json!({"role": "assistant", "content": "done"}),
            ]
        );
    }

    #[test]
    fn tool_result_rounds_are_dropped_instead_of_recap_injected() {
        let mut messages = vec![json!({"role": "user", "content": "inspect"})];
        for i in 0..12 {
            messages.push(json!({
                "role": "assistant",
                "tool_calls": [{"id": format!("c{i}"), "function": {"name": "grep"}}],
            }));
            messages.push(json!({
                "role": "tool",
                "tool_call_id": format!("c{i}"),
                "content": format!("match {i} {}", "x".repeat(900)),
            }));
        }

        let got = sanitize_prompt_facing_messages(messages);

        assert_eq!(got, vec![json!({"role": "user", "content": "inspect"})]);
    }

    #[test]
    fn runtime_recap_surfaces_structured_state_without_legacy_controls() {
        let state = SessionStateCompact {
            blocked_tools: vec!["stale_block".into()],
            recent_tools: vec!["read_file".into(), "grep".into()],
            budget_remaining_tokens: 1234,
            budget_remaining_rounds: 7,
            consecutive_ctx_errors: 2,
            delegation: Some(DelegationCompact {
                id: "del-1".into(),
                pattern: "fanout".into(),
                completed_sub_runs: vec![],
            }),
            ..Default::default()
        };

        let recap = runtime_recap_message(&state).expect("runtime recap");
        let content = recap["content"].as_str().unwrap();

        assert!(content.starts_with("[Session runtime recap]"));
        assert!(content.contains("Recent tools: read_file, grep"));
        assert!(
            !content.contains("write_file"),
            "schema materialization belongs in the tool surface, not recap prose"
        );
        assert!(!content.contains("checkpoint budget"));
        assert!(!content.contains("tokens=1234"));
        assert!(content.contains("Context-window recovery attempts: 2"));
        assert!(content.contains("Delegation: id=del-1, pattern=fanout, completed_sub_runs=0"));
        assert!(!content.contains("stale_block"));
    }

    #[test]
    fn sanitize_with_state_replaces_stale_runtime_recap_for_resume_prompt() {
        let semantics = astra_turn_types::UserTurnSemantics::new(
            astra_turn_types::ObjectiveRelation::Continue,
            None,
        );
        let mut user = json!({"role": "user", "content": "continue"});
        astra_turn_types::mark_user_turn_semantics(&mut user, semantics);
        let messages = vec![
            user,
            runtime_owned_message(
                "system",
                "stale projection",
                RuntimeMessageDelivery::Projection,
            ),
        ];
        let state = SessionStateCompact {
            recent_tools: vec!["bash".into()],
            ..Default::default()
        };

        let got = sanitize_prompt_facing_messages_with_state(messages, &state)
            .expect("valid typed continuation metadata");

        assert_eq!(got.len(), 2);
        assert_eq!(got[0]["content"], "continue");
        assert_eq!(
            astra_turn_types::user_turn_semantics(&got[0]).unwrap(),
            Some(semantics)
        );
        let recap = got[1]["content"].as_str().unwrap();
        assert!(recap.contains("Recent tools: bash"));
        assert!(!recap.contains("stale"));
    }

    #[test]
    fn sanitize_with_state_rejects_corrupt_typed_metadata() {
        let messages = vec![json!({
            "role": "user",
            "content": "continue",
            (astra_turn_types::USER_TURN_SEMANTICS_FIELD): {
                "schema_version": "invalid",
                "objective_relation": "continue"
            }
        })];

        assert!(matches!(
            sanitize_prompt_facing_messages_with_state(messages, &SessionStateCompact::default(),),
            Err(astra_turn_types::UserTurnSemanticsError::Malformed(_))
        ));
    }

    #[test]
    fn user_visible_messages_drop_prompt_internal_recaps_and_control_bytes() {
        let messages = vec![
            json!({"role": "user", "content": "hello\u{0}"}),
            runtime_owned_message(
                "system",
                "arbitrary internal trace",
                RuntimeMessageDelivery::EphemeralControl,
            ),
            runtime_owned_message(
                "system",
                "arbitrary internal recap",
                RuntimeMessageDelivery::Projection,
            ),
            json!({"role": "tool", "content": "raw tool output"}),
            json!({"role": "assistant", "content": ""}),
            json!({"role": "assistant", "content": "\u{1b}[31mdone\u{1b}[0m"}),
            json!({"role": "system", "content": "visible status"}),
        ];

        let got = sanitize_user_visible_messages(messages);

        assert_eq!(
            got,
            vec![
                json!({"role": "user", "content": "hello"}),
                json!({"role": "assistant", "content": "done"}),
                json!({"role": "system", "content": "visible status"}),
            ]
        );
    }
}
