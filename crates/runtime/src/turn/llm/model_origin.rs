//! Attested model ownership for provider-private continuation.
//!
//! Public text and complete tool results remain in the conversation. Private
//! continuation is replayed only to the same admitted route; a different
//! route receives the public group after its pairing has been validated.
use std::borrow::Cow;
use std::collections::HashSet;

use astra_services::FernetTokenEncryptor;
use astra_turn_types::ModelReplayOrigin;
use serde_json::{Value, json};

/// An internal marker on the disposable request view, never canonical input.
pub(crate) const FOREIGN_GROUP: &str = "_astra_foreign_model_group";
const DOMAIN: &str = "astra.model-replay-origin.v1";

pub(crate) struct ReplayScope<'a> {
    pub encryptor: &'a FernetTokenEncryptor,
    pub user_id: &'a str,
    pub session_id: &'a str,
    pub allowed_session_ids: Option<&'a HashSet<String>>,
    pub run_id: &'a str,
    pub selection_revision: Option<i64>,
    pub route: &'a str,
}

fn proof_input(user: &str, origin: &ModelReplayOrigin) -> String {
    astra_core::canonical_json_string(&json!([
        user,
        origin.session_id,
        origin.run_id,
        origin.item_id,
        origin.selection_revision,
        origin.route,
        origin.runtime_generated,
        origin.tool_calls_digest,
        origin.reasoning_digest,
        origin.durable_tool_calls_digest,
        origin.durable_reasoning_digest
    ]))
}

// Hash only the stable public call shape. Provider-specific call IDs are
// intentionally excluded because a foreign wire view removes them.
fn tool_calls_digest(scope: &ReplayScope<'_>, message: &Value) -> Option<String> {
    let calls = message.get("tool_calls")?.as_array()?;
    if calls.is_empty() {
        return None;
    }
    let calls: Vec<_> = calls
        .iter()
        .map(|call| {
            json!({
                "id": call.get("id"), "type": call.get("type"), "function": call.get("function")
            })
        })
        .collect();
    Some(scope.encryptor.keyed_digest(
        "astra.model-replay-calls.v1",
        &astra_core::canonical_json_string(&json!(calls)),
    ))
}

fn reasoning_digest(scope: &ReplayScope<'_>, message: &Value) -> Option<String> {
    let content = message.get("reasoning_content").and_then(Value::as_str);
    let signature = message.get("reasoning_signature").and_then(Value::as_str);
    if content.is_none() && signature.is_none() {
        return None;
    }
    Some(scope.encryptor.keyed_digest(
        "astra.model-replay-reasoning.v1",
        &astra_core::canonical_json_string(&json!([content, signature])),
    ))
}

pub(crate) fn attest(scope: &ReplayScope<'_>, message: &mut Value) {
    attest_with_kind(scope, message, false);
}

pub(crate) fn attest_runtime(scope: &ReplayScope<'_>, message: &mut Value) {
    attest_with_kind(scope, message, true);
}

fn attest_with_kind(scope: &ReplayScope<'_>, message: &mut Value, runtime_generated: bool) {
    if message["role"] != "assistant"
        || (!has_calls(message)
            && message.get("reasoning_content").is_none()
            && message.get("reasoning_signature").is_none()
            && message
                .get(astra_core::model_wire::responses::CONTINUATION_KEY)
                .is_none())
    {
        return;
    }
    let item_id = astra_turn_types::model_item_id(message)
        .map(str::to_owned)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    astra_turn_types::mark_model_message(message, Some(&item_id));
    let calls = tool_calls_digest(scope, message);
    let reasoning = reasoning_digest(scope, message);
    let (durable_calls, durable_reasoning) = if calls.is_some() || reasoning.is_some() {
        let mut sanitized =
            astra_turn_core::runtime_scaffolding::sanitize_durable_message_values(vec![
                message.clone(),
            ]);
        let durable = sanitized.pop().expect("one durable message");
        let safe_calls = tool_calls_digest(scope, &durable);
        let safe_reasoning = reasoning_digest(scope, &durable);
        (
            safe_calls.filter(|digest| Some(digest) != calls.as_ref()),
            safe_reasoning.filter(|digest| Some(digest) != reasoning.as_ref()),
        )
    } else {
        (None, None)
    };
    let mut origin = ModelReplayOrigin {
        session_id: scope.session_id.to_owned(),
        run_id: scope.run_id.to_owned(),
        item_id,
        selection_revision: scope.selection_revision,
        route: scope.route.to_owned(),
        runtime_generated,
        tool_calls_digest: calls,
        reasoning_digest: reasoning,
        durable_tool_calls_digest: durable_calls,
        durable_reasoning_digest: durable_reasoning,
        proof: String::new(),
    };
    origin.proof = scope
        .encryptor
        .keyed_digest(DOMAIN, &proof_input(scope.user_id, &origin));
    astra_turn_types::mark_model_replay_origin(message, &origin);
}

fn foreign(
    scope: &ReplayScope<'_>,
    message: &Value,
    historical: bool,
) -> Result<bool, astra_core::ClassifiedError> {
    let Some(origin) = astra_turn_types::model_replay_origin(message) else {
        return if historical { Ok(true) } else { Err(invalid()) };
    };
    if astra_turn_types::model_item_id(message) != Some(origin.item_id.as_str())
        || !scope.encryptor.verify_keyed_digest(
            DOMAIN,
            &proof_input(scope.user_id, &origin),
            &origin.proof,
        )
    {
        return Err(invalid());
    }
    let calls = tool_calls_digest(scope, message);
    let reasoning = reasoning_digest(scope, message);
    let calls_match = calls == origin.tool_calls_digest
        || origin
            .durable_tool_calls_digest
            .as_ref()
            .is_some_and(|digest| calls.as_ref() == Some(digest));
    let missing_tool_reasoning = reasoning.is_none()
        && origin.reasoning_digest.is_some()
        && origin.tool_calls_digest.is_some()
        && message[FOREIGN_GROUP] != true;
    if (!calls_match && !(historical && incomplete(message)))
        || missing_tool_reasoning
        || reasoning.is_some_and(|digest| {
            Some(&digest) != origin.reasoning_digest.as_ref()
                && Some(&digest) != origin.durable_reasoning_digest.as_ref()
        })
    {
        return Err(invalid());
    }
    if origin.session_id != scope.session_id
        && !scope
            .allowed_session_ids
            .is_some_and(|ids| ids.contains(&origin.session_id))
    {
        // Fork replacement can retain a completed ancestor's public transcript
        // after its private-reasoning authority has expired. Never re-open that
        // private state; an active group still needs its exact owner.
        return if historical { Ok(true) } else { Err(invalid()) };
    }
    let same_route_reasoning_loss = origin.route == scope.route
        && message[astra_core::model_wire::responses::CONTINUATION_KEY]
            [astra_core::model_wire::responses::HISTORY_INCOMPLETE]
            == true;
    if historical && incomplete(message) && !same_route_reasoning_loss {
        return Ok(true);
    }
    if origin.runtime_generated {
        return Ok(true);
    }
    if origin.run_id == scope.run_id
        && origin.selection_revision == scope.selection_revision
        && origin.route != scope.route
    {
        // A route drift without an admitted user selection is not a handoff.
        return Err(invalid());
    }
    if origin.run_id == scope.run_id
        && origin.selection_revision.unwrap_or(-1) > scope.selection_revision.unwrap_or(-1)
    {
        return Err(invalid());
    }
    Ok(origin.route != scope.route)
}

fn invalid() -> astra_core::ClassifiedError {
    astra_core::ClassifiedError::new(
        astra_core::ErrorKind::InvalidRequest,
        "The model continuation could not be verified. Start a new turn to continue.",
    )
}

fn has_calls(message: &Value) -> bool {
    message
        .get("tool_calls")
        .and_then(Value::as_array)
        .is_some_and(|calls| !calls.is_empty())
}

/// A summary is a new model request, not a provider continuation. Its input
/// consists of the public conversation and complete tool evidence only.
pub(crate) fn public_summary_history(
    messages: &[Value],
) -> Result<Vec<Value>, astra_core::ClassifiedError> {
    let public: Vec<Value> = messages
        .iter()
        .filter_map(|message| {
            let mut public = message.clone();
            if let Some(object) = public.as_object_mut()
                && object.get("role").and_then(Value::as_str) == Some("assistant")
            {
                object.remove(astra_core::model_wire::responses::CONTINUATION_KEY);
                object.remove("reasoning_content");
                object.remove("reasoning_signature");
                object.remove(astra_turn_types::MODEL_REPLAY_ORIGIN_FIELD);
                object.remove(FOREIGN_GROUP);
                if let Some(calls) = object.get_mut("tool_calls").and_then(Value::as_array_mut) {
                    for call in calls {
                        if let Some(call) = call.as_object_mut() {
                            call.remove(
                                astra_turn_core::tool::args::shape::PROVIDER_TOOL_CALL_ID_FIELD,
                            );
                        }
                    }
                }
                let empty = match object.get("content") {
                    None | Some(Value::Null) => true,
                    Some(Value::String(text)) => text.is_empty(),
                    Some(Value::Array(parts)) => parts.is_empty(),
                    _ => false,
                };
                let calls = object
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .is_some_and(|calls| !calls.is_empty());
                if empty && !calls {
                    if incomplete(message) {
                        object.insert(
                            "content".into(),
                            json!("[An earlier response or tool result was not fully recorded.]"),
                        );
                    } else {
                        return None;
                    }
                }
            }
            Some(public)
        })
        .collect();
    super::client::validate_append_only_openai_history(&public)?;
    Ok(public)
}

fn incomplete(message: &Value) -> bool {
    message
        .get(astra_turn_core::prompt_facing::INCOMPLETE_TOOL_GROUP_FIELD)
        .is_some()
        || message
            .get(astra_core::model_wire::responses::CONTINUATION_KEY)
            .is_some_and(|saved| {
                saved["invalid"] == true
                    || saved[astra_core::model_wire::responses::HISTORY_INCOMPLETE] == true
                    || astra_core::model_wire::responses::validated_continuation(message).is_none()
            })
}

fn validate_foreign_group(
    messages: &[Value],
    index: usize,
    historical: bool,
) -> Result<(), astra_core::ClassifiedError> {
    let message = &messages[index];
    if incomplete(message) && !historical {
        return Err(invalid());
    }
    let mut end = index + 1;
    while end < messages.len() && messages[end]["role"] == "tool" {
        end += 1;
    }
    if has_calls(message) {
        super::client::validate_append_only_openai_history(&messages[index..end])
            .map_err(|_| invalid())?;
    }
    if let Some(saved) = message.get(astra_core::model_wire::responses::CONTINUATION_KEY)
        && !(historical && incomplete(message))
    {
        if saved["invalid"] == true
            || saved[astra_core::model_wire::responses::HISTORY_INCOMPLETE] == true
            || astra_core::model_wire::responses::validated_continuation(message).is_none()
            || (has_calls(message)
                && !astra_core::model_wire::responses::continuation_group_matches(
                    message,
                    &messages[index + 1..end],
                    astra_turn_core::tool::args::shape::provider_tool_call_id,
                ))
        {
            return Err(invalid());
        }
    }
    Ok(())
}

/// Only authenticated producer metadata may identify a foreign active group.
/// Old untagged completed turns are public history; untagged active groups
/// cannot establish continuation authority.
pub(crate) fn project<'a>(
    scope: &ReplayScope<'_>,
    messages: &'a [Value],
) -> Result<Cow<'a, [Value]>, astra_core::ClassifiedError> {
    let active = messages
        .iter()
        .rposition(astra_turn_types::is_human_user_message)
        .map_or(0, |i| i + 1);
    let mut foreign_indices = Vec::new();
    for (index, message) in messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m["role"] == "assistant")
    {
        let private = message
            .get(astra_core::model_wire::responses::CONTINUATION_KEY)
            .is_some()
            || message.get("reasoning_content").is_some()
            || message.get("reasoning_signature").is_some()
            || has_calls(message)
            || message
                .get(astra_turn_core::prompt_facing::INCOMPLETE_TOOL_GROUP_FIELD)
                .is_some();
        if !private {
            continue;
        }
        if foreign(scope, message, index < active)? {
            validate_foreign_group(messages, index, index < active)?;
            foreign_indices.push(index);
        }
    }
    if foreign_indices.is_empty() {
        return Ok(Cow::Borrowed(messages));
    }
    let mut projected = messages.to_vec();
    for index in foreign_indices {
        let incomplete_history = index < active && incomplete(&projected[index]);
        let object = projected[index].as_object_mut().ok_or_else(invalid)?;
        object.remove(astra_core::model_wire::responses::CONTINUATION_KEY);
        object.remove("reasoning_content");
        object.remove("reasoning_signature");
        if incomplete_history {
            const NOTE: &str = "[An earlier response or tool result was not fully recorded. Inspect the current state before relying on it.]";
            object.insert(
                astra_turn_core::prompt_facing::INCOMPLETE_TOOL_GROUP_FIELD.into(),
                Value::Bool(true),
            );
            match object.get_mut("content") {
                Some(Value::String(content)) if !content.is_empty() => {
                    content.push('\n');
                    content.push_str(NOTE);
                }
                Some(Value::Array(parts)) => parts.push(json!({"type":"text", "text":NOTE})),
                _ => {
                    object.insert("content".into(), json!(NOTE));
                }
            }
        }
        if let Some(calls) = object.get_mut("tool_calls").and_then(Value::as_array_mut) {
            for call in calls {
                if let Some(call) = call.as_object_mut() {
                    call.remove(astra_turn_core::tool::args::shape::PROVIDER_TOOL_CALL_ID_FIELD);
                }
            }
        }
        object.insert(FOREIGN_GROUP.into(), Value::Bool(true));
    }
    projected.retain(|message| {
        let empty = match message.get("content") {
            None | Some(Value::Null) => true,
            Some(Value::String(text)) => text.is_empty(),
            Some(Value::Array(parts)) => parts.is_empty(),
            _ => false,
        };
        !(message[FOREIGN_GROUP] == true && !has_calls(message) && empty)
    });
    Ok(Cow::Owned(projected))
}

/// Recheck the disposable view at dispatch; callers cannot forge a skip flag.
pub(crate) fn verify_projected(
    scope: &ReplayScope<'_>,
    messages: &[Value],
) -> Result<(), astra_core::ClassifiedError> {
    let active = messages
        .iter()
        .rposition(astra_turn_types::is_human_user_message)
        .map_or(0, |i| i + 1);
    for (index, message) in messages.iter().enumerate() {
        if message["role"] != "assistant" {
            if message.get(FOREIGN_GROUP).is_some() {
                return Err(invalid());
            }
            continue;
        }
        let marked = message.get(FOREIGN_GROUP).is_some();
        let relevant = marked
            || has_calls(message)
            || message
                .get(astra_core::model_wire::responses::CONTINUATION_KEY)
                .is_some()
            || message.get("reasoning_content").is_some()
            || message.get("reasoning_signature").is_some()
            || message
                .get(astra_turn_core::prompt_facing::INCOMPLETE_TOOL_GROUP_FIELD)
                .is_some();
        if !relevant {
            continue;
        }
        let expected = foreign(scope, message, index < active)?;
        if expected != marked {
            return Err(invalid());
        }
        if !marked {
            continue;
        }
        validate_foreign_group(messages, index, index < active)?;
        if message[FOREIGN_GROUP] != true
            || message
                .get(astra_core::model_wire::responses::CONTINUATION_KEY)
                .is_some()
            || message.get("reasoning_content").is_some()
            || message.get("reasoning_signature").is_some()
            || message
                .get("tool_calls")
                .and_then(Value::as_array)
                .is_some_and(|calls| {
                    calls.iter().any(|call| {
                        call.get(astra_turn_core::tool::args::shape::PROVIDER_TOOL_CALL_ID_FIELD)
                            .is_some()
                    })
                })
        {
            return Err(invalid());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope<'a>(
        key: &'a FernetTokenEncryptor,
        route: &'a str,
        revision: Option<i64>,
    ) -> ReplayScope<'a> {
        ReplayScope {
            encryptor: key,
            user_id: "user",
            session_id: "session",
            allowed_session_ids: None,
            run_id: "run",
            selection_revision: revision,
            route,
        }
    }

    fn tool_group(id: &str, reasoning: &str) -> Vec<Value> {
        vec![
            json!({"role":"assistant", "content":"working", "reasoning_content":reasoning,
                "tool_calls":[{"id":id,"type":"function","function":{"name":"read_file","arguments":"{}"}}]}),
            json!({"role":"tool", "tool_call_id":id, "content":"found"}),
        ]
    }

    #[test]
    fn switch_preserves_public_evidence_and_current_private_context() {
        let key = FernetTokenEncryptor::new("model-origin-test").unwrap();
        let first = scope(&key, "route-a", None);
        let second = scope(&key, "route-b", Some(7));
        let mut messages = vec![json!({"role":"user","content":"work"})];
        let mut a = tool_group("call-a", "private-a");
        attest(&first, &mut a[0]);
        messages.extend(a);
        messages.push(json!({"role":"user","content":"also check this"}));
        let mut b = tool_group("call-b", "private-b");
        attest(&second, &mut b[0]);
        messages.extend(b);

        let view = project(&second, &messages).unwrap();
        verify_projected(&second, &view).unwrap();
        assert!(view[1].get("reasoning_content").is_none());
        assert_eq!(view[2]["content"], "found");
        assert_eq!(view[4]["reasoning_content"], "private-b");
        assert_eq!(messages[1]["reasoning_content"], "private-a");

        // Re-selecting a prior route retains its own continuation while
        // other providers remain public-only.
        let again = scope(&key, "route-a", Some(8));
        let view = project(&again, &messages).unwrap();
        verify_projected(&again, &view).unwrap();
        assert_eq!(view[1]["reasoning_content"], "private-a");
        assert!(view[4].get("reasoning_content").is_none());
    }

    #[test]
    fn incomplete_or_unattested_active_groups_cannot_be_replayed() {
        let key = FernetTokenEncryptor::new("model-origin-test").unwrap();
        let first = scope(&key, "route-a", None);
        let second = scope(&key, "route-b", Some(1));
        let mut messages = vec![json!({"role":"user","content":"work"})];
        let mut group = tool_group("call", "private");
        attest(&first, &mut group[0]);
        messages.extend(group);

        let mut missing = messages.clone();
        missing.pop();
        assert!(project(&second, &missing).is_err());
        let mut tampered = messages.clone();
        tampered[1][astra_turn_types::MODEL_REPLAY_ORIGIN_FIELD]["route"] = json!("route-b");
        assert!(project(&second, &tampered).is_err());
        let mut tampered_reasoning = messages.clone();
        tampered_reasoning[1]["reasoning_content"] = json!("other model's private reasoning");
        assert!(project(&second, &tampered_reasoning).is_err());
        tampered_reasoning[1]
            .as_object_mut()
            .unwrap()
            .remove("reasoning_content");
        assert!(project(&first, &tampered_reasoning).is_err());
        let mut tampered_call = messages.clone();
        tampered_call[1]["tool_calls"][0]["function"]["arguments"] = json!("{\"other\":true}");
        assert!(project(&second, &tampered_call).is_err());
        let mut untrusted = messages.clone();
        untrusted[1]
            .as_object_mut()
            .unwrap()
            .remove(astra_turn_types::MODEL_REPLAY_ORIGIN_FIELD);
        assert!(project(&second, &untrusted).is_err());
        assert!(verify_projected(&second, &messages).is_err());

        let drift = scope(&key, "different-route", None);
        assert!(project(&drift, &messages).is_err());
    }

    #[test]
    fn only_authorized_ancestor_sessions_can_supply_model_context() {
        let key = FernetTokenEncryptor::new("model-origin-test").unwrap();
        let parent = scope(&key, "route-a", None);
        let mut group = tool_group("call", "private");
        attest(&parent, &mut group[0]);
        let history = vec![
            json!({"role":"user","content":"old"}),
            group[0].clone(),
            group[1].clone(),
            json!({"role":"user","content":"new"}),
        ];
        let allowed = HashSet::from(["session".to_string()]);
        let child = ReplayScope {
            session_id: "child",
            run_id: "child-run",
            allowed_session_ids: Some(&allowed),
            ..parent
        };
        assert!(project(&child, &history).is_ok());
        let unrelated = ReplayScope {
            allowed_session_ids: None,
            ..child
        };
        let public = project(&unrelated, &history).unwrap();
        assert!(public[1].get("reasoning_content").is_none());
        assert_eq!(public[2]["content"], "found");
        assert!(project(&unrelated, &history[..3]).is_err());
    }

    #[test]
    fn a_new_turn_can_continue_after_an_unrecorded_tool_result() {
        let key = FernetTokenEncryptor::new("model-origin-test").unwrap();
        let first = scope(&key, "route-a", None);
        let second = scope(&key, "route-b", Some(1));
        let mut incomplete = tool_group("call", "private");
        attest(&first, &mut incomplete[0]);
        let canonical = astra_turn_core::prompt_facing::sanitize_canonical_continuation_messages_with_turn_semantics(
            vec![json!({"role":"user","content":"work"}), incomplete.remove(0)],
        ).unwrap();
        assert!(project(&second, &canonical).is_err());
        let mut next_turn = canonical;
        next_turn.push(json!({"role":"user","content":"continue"}));
        let view = project(&second, &next_turn).unwrap();
        verify_projected(&second, &view).unwrap();
        let visible = json!(view.as_ref()).to_string();
        assert!(visible.contains("not recorded") || visible.contains("not fully recorded"));
        assert!(!visible.contains("private\""));
    }

    #[test]
    fn foreign_reasoning_only_retry_does_not_create_an_empty_assistant_message() {
        let key = FernetTokenEncryptor::new("model-origin-test").unwrap();
        let old = scope(&key, "old", None);
        let new = scope(&key, "new", Some(1));
        let mut private = json!({"role":"assistant", "content":"", "reasoning_content":"secret"});
        attest(&old, &mut private);
        let messages = vec![json!({"role":"user","content":"work"}), private];
        let view = project(&new, &messages).unwrap();
        verify_projected(&new, &view).unwrap();
        assert_eq!(view.len(), 1);
        assert_eq!(view[0]["content"], "work");
    }

    #[test]
    fn durable_redaction_keeps_verified_tool_and_reasoning_identity() {
        let key = FernetTokenEncryptor::new("model-origin-test").unwrap();
        let old = scope(&key, "old", None);
        let new = scope(&key, "new", Some(1));
        let secret = "hf_abcdefghijklmnopqrstuvwxyz123456";
        let mut group = tool_group("call", &format!("tool --token {secret}"));
        group[0]["tool_calls"][0]["function"]["arguments"] =
            json!(format!("{{\"api_key\":\"{secret}\"}}"));
        attest(&old, &mut group[0]);
        let mut messages = vec![json!({"role":"user","content":"work"})];
        messages.extend(group);
        let durable =
            astra_turn_core::runtime_scaffolding::sanitize_durable_message_values(messages);
        assert!(!json!(&durable).to_string().contains(secret));
        let same = project(&old, &durable).unwrap();
        verify_projected(&old, &same).unwrap();
        let switched = project(&new, &durable).unwrap();
        verify_projected(&new, &switched).unwrap();
        assert!(switched[1].get("reasoning_content").is_none());
        assert_eq!(switched[2]["content"], "found");
    }

    #[test]
    fn same_route_keeps_reasoning_loss_visible_to_its_provider_contract() {
        use astra_core::model_wire::responses::{CONTINUATION_KEY, HISTORY_INCOMPLETE};
        let key = FernetTokenEncryptor::new("model-origin-test").unwrap();
        let old = scope(&key, "old", None);
        let new = scope(&key, "new", Some(1));
        let mut answer = json!({"role":"assistant", "content":"partial", CONTINUATION_KEY:{
            "connection_id":"00000000-0000-0000-0000-000000000001", "model":"model", HISTORY_INCOMPLETE:true
        }});
        attest(&old, &mut answer);
        let messages = vec![
            json!({"role":"user","content":"work"}),
            answer,
            json!({"role":"user","content":"continue"}),
        ];
        let same = project(&old, &messages).unwrap();
        verify_projected(&old, &same).unwrap();
        assert_eq!(same[1][CONTINUATION_KEY][HISTORY_INCOMPLETE], true);
        let switched = project(&new, &messages).unwrap();
        verify_projected(&new, &switched).unwrap();
        assert!(switched[1].get(CONTINUATION_KEY).is_none());
        assert!(
            switched[1]["content"]
                .as_str()
                .unwrap()
                .contains("not fully recorded")
        );
    }

    #[test]
    fn summaries_receive_public_history_without_provider_private_state() {
        let key = FernetTokenEncryptor::new("model-origin-test").unwrap();
        let old = scope(&key, "old", None);
        let mut group = tool_group("call", "private-reasoning");
        group[0]["reasoning_signature"] = json!("private-signature");
        group[0]["tool_calls"][0]
            [astra_turn_core::tool::args::shape::PROVIDER_TOOL_CALL_ID_FIELD] =
            json!("private-provider-id");
        attest(&old, &mut group[0]);
        let messages = vec![
            json!({"role":"user","content":"work"}),
            group[0].clone(),
            group[1].clone(),
        ];
        let public = public_summary_history(&messages).unwrap();
        let encoded = json!(&public).to_string();
        assert!(!encoded.contains("private-reasoning"));
        assert!(!encoded.contains("private-signature"));
        assert!(!encoded.contains("private-provider-id"));
        assert_eq!(public[2]["content"], "found");
        assert_eq!(messages[1]["reasoning_content"], "private-reasoning");
    }
}
