//! Protect provider-private Responses item metadata within the canonical message.
use astra_core::model_wire::responses::{CONTINUATION_KEY, SEALED_ITEM, model_binding};
use astra_services::FernetTokenEncryptor;
use base64::{Engine as _, engine::general_purpose::URL_SAFE};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashSet;

fn calls_digest(calls: &[(String, String)]) -> Result<String, &'static str> {
    let serialized = serde_json::to_vec(calls).map_err(|_| "invalid call identity")?;
    Ok(format!("{:x}", Sha256::digest(serialized)))
}

fn item_slot(item: &Value) -> Result<usize, &'static str> {
    match item["type"].as_str() {
        Some("reasoning") => Ok(0),
        Some("function_call") => Ok(1),
        Some("message") => Ok(2),
        _ => Err("invalid continuation item"),
    }
}

pub(crate) fn seal(
    continuation: &mut Value,
    encryptor: &FernetTokenEncryptor,
    owner: &str,
    origin: &str,
) -> Result<(), &'static str> {
    let scope = continuation["connection_id"]
        .as_str()
        .ok_or("missing scope")?
        .to_owned();
    let model = continuation["model"]
        .as_str()
        .ok_or("missing model")?
        .to_owned();
    let model_binding = model_binding(&model);
    continuation["model"] = json!(model_binding);
    let calls: Vec<(String, String)> = continuation["items"]
        .as_array()
        .ok_or("missing items")?
        .iter()
        .filter(|item| item["type"] == "function_call")
        .map(|item| {
            Ok((
                item["call_id"]
                    .as_str()
                    .ok_or("missing call identity")?
                    .to_owned(),
                item["name"].as_str().ok_or("missing call name")?.to_owned(),
            ))
        })
        .collect::<Result<_, &'static str>>()?;
    let calls_digest = calls_digest(&calls)?;
    let items = continuation["items"]
        .as_array_mut()
        .ok_or("missing items")?;
    // Account for the encoded capsules against the existing commit budget.
    // The complete assistant message and its tool results are checked against
    // the tighter segment and remaining batch budgets by their owning paths.
    let mut remaining = astra_core::runtime_limits::MAX_CANONICAL_CONTEXT_BATCH_BYTES;
    let mut non_reasoning = 0_u64;
    for (index, item) in items.iter_mut().enumerate() {
        let slot = item_slot(item)?;
        let ordinal = if slot == 0 {
            index as u64
        } else {
            let ordinal = non_reasoning;
            non_reasoning += 1;
            ordinal
        };
        if item.get(SEALED_ITEM).is_some() {
            return Err("provider supplied reserved item metadata");
        }
        if owner.is_empty() || origin.is_empty() {
            return Err("missing reasoning owner");
        }
        let object = item.as_object_mut().ok_or("invalid item")?;
        // Preserve the exact provider state inside the authenticated capsule.
        // Redacting it before replay would silently change the continuation.
        let content = (slot == 0).then(|| object.remove("content")).flatten();
        let summary = (slot == 0).then(|| object.remove("summary")).flatten();
        let item_id = object.remove("id");
        let phase = object.remove("phase");
        let encrypted_content = (slot == 0)
            .then(|| object.remove("encrypted_content"))
            .flatten()
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_owned)
                    .ok_or("invalid opaque reasoning")
            })
            .transpose()?;
        let payload = json!({
            "domain":"astra.responses.item.v2",
            "owner":owner,
            "origin":origin,
            "scope":scope,
            "model":model_binding,
            "calls_digest":calls_digest,
            "item_type":slot,
            "item_ordinal":ordinal,
            "item_id":item_id,
            "phase":phase,
            "content":content,
            "summary":summary,
            "encrypted_content":encrypted_content,
        });
        if !astra_turn_types::json_serialized_fits(&payload, remaining)
            .map_err(|_| "invalid reasoning")?
        {
            return Err("private reasoning exceeds storage budget");
        }
        let plaintext = serde_json::to_string(&payload).map_err(|_| "invalid reasoning")?;
        let ciphertext = encryptor
            .encrypt(&plaintext)
            .map_err(|_| "cannot seal reasoning")?;
        // Fernet's base64 may coincidentally contain a credential-shaped
        // substring. Encode its bytes with an alphabet that cannot match the
        // credential redactor, so canonical sanitization cannot corrupt it.
        let ciphertext = URL_SAFE
            .decode(ciphertext)
            .map_err(|_| "invalid sealed reasoning")?;
        let ciphertext = data_encoding::BASE32_NOPAD
            .encode(&ciphertext)
            .to_ascii_lowercase();
        remaining = remaining
            .checked_sub(ciphertext.len() as u64)
            .ok_or("private reasoning exceeds storage budget")?;
        object.insert(
            SEALED_ITEM.into(),
            json!({"origin":origin,"ciphertext":ciphertext}),
        );
    }
    Ok(())
}

pub(crate) fn origins(messages: &[Value], scope: &str, model: &str) -> HashSet<String> {
    let model = model_binding(model);
    messages
        .iter()
        .filter_map(|message| message.get(CONTINUATION_KEY))
        .filter(|saved| saved["connection_id"] == scope && saved["model"] == model)
        .filter_map(|saved| saved["items"].as_array())
        .flatten()
        .filter_map(|item| item[SEALED_ITEM]["origin"].as_str())
        .map(str::to_owned)
        .collect()
}

/// Apply a route's declared policy before projecting runtime-owned messages to
/// provider roles, while the actual human-turn boundary is still available.
pub(crate) fn discard_completed_turn_reasoning(messages: &mut [Value]) {
    let start = messages
        .iter()
        .rposition(astra_turn_types::is_human_user_message)
        .map_or(0, |index| index + 1);
    for message in &mut messages[..start] {
        discard_reasoning(message, None);
    }
}

/// Drop reasoning only for the route whose admitted contract permits it.
/// This operates on newly committed messages, never on the immutable prefix.
pub(crate) fn discard_reasoning(message: &mut Value, route: Option<(&str, &str)>) {
    let Some(object) = message.as_object_mut() else {
        return;
    };
    let Some(saved) = object.get_mut(CONTINUATION_KEY) else {
        return;
    };
    if route.is_some_and(|(scope, model)| {
        saved["connection_id"] != scope || saved["model"] != model_binding(model)
    }) {
        return;
    }
    if saved[astra_core::model_wire::responses::HISTORY_INCOMPLETE] == true {
        object.remove(CONTINUATION_KEY);
        return;
    }
    if saved["invalid"] == true && route.is_none() {
        object.remove(CONTINUATION_KEY);
        return;
    }
    let Some(items) = saved["items"].as_array_mut() else {
        return;
    };
    items.retain(|item| item["type"] != "reasoning");
    if items.is_empty() {
        object.remove(CONTINUATION_KEY);
    }
}

pub(crate) fn open(
    messages: &mut [Value],
    encryptor: &FernetTokenEncryptor,
    owner: &str,
    allowed_origins: &HashSet<String>,
    scope: &str,
    model: &str,
) -> Result<(), &'static str> {
    let model_binding = model_binding(model);
    for message in messages {
        let Some(saved) = message.get_mut(CONTINUATION_KEY) else {
            continue;
        };
        if saved["connection_id"] != scope || saved["model"] != model_binding {
            continue;
        }
        if saved[astra_core::model_wire::responses::HISTORY_INCOMPLETE] == true {
            saved["model"] = json!(model);
            continue;
        }
        let calls: Vec<(String, String)> = saved["items"]
            .as_array()
            .ok_or("invalid continuation")?
            .iter()
            .filter(|item| item["type"] == "function_call")
            .map(|item| {
                Ok((
                    item["call_id"]
                        .as_str()
                        .ok_or("missing call identity")?
                        .to_owned(),
                    item["name"].as_str().ok_or("missing call name")?.to_owned(),
                ))
            })
            .collect::<Result<_, &'static str>>()?;
        let calls_digest = calls_digest(&calls)?;
        let items = saved["items"]
            .as_array_mut()
            .ok_or("invalid continuation")?;
        let mut non_reasoning = 0_u64;
        for (index, item) in items.iter_mut().enumerate() {
            let slot = item_slot(item)?;
            let ordinal = if slot == 0 {
                index as u64
            } else {
                let ordinal = non_reasoning;
                non_reasoning += 1;
                ordinal
            };
            if astra_core::model_wire::responses::has_unsealed_reasoning_item(item) {
                return Err("unsealed reasoning");
            }
            let Some(sealed) = item.get(SEALED_ITEM) else {
                return Err("unsealed item metadata");
            };
            if item.get("id").is_some_and(|value| !value.is_null())
                || item.get("phase").is_some_and(|value| !value.is_null())
            {
                return Err("unsealed item metadata");
            }
            let origin = sealed["origin"].as_str().ok_or("invalid origin")?;
            if !allowed_origins.contains(origin) {
                return Err("origin not authorized");
            }
            let ciphertext = sealed["ciphertext"].as_str().ok_or("invalid ciphertext")?;
            let ciphertext = data_encoding::BASE32_NOPAD
                .decode(ciphertext.to_ascii_uppercase().as_bytes())
                .map_err(|_| "invalid ciphertext")?;
            let plaintext = encryptor
                .decrypt(&URL_SAFE.encode(ciphertext))
                .map_err(|_| "cannot open reasoning")?;
            let payload: Value =
                serde_json::from_str(&plaintext).map_err(|_| "invalid reasoning")?;
            if payload["domain"] != "astra.responses.item.v2"
                || payload["owner"] != owner
                || payload["origin"] != origin
                || payload["scope"] != scope
                || payload["model"] != model_binding
                || payload["calls_digest"] != calls_digest
                || payload["item_type"].as_u64() != Some(slot as u64)
                || payload["item_ordinal"].as_u64() != Some(ordinal)
            {
                return Err("reasoning binding mismatch");
            }
            let object = item.as_object_mut().ok_or("invalid item")?;
            object.remove(SEALED_ITEM);
            if let Some(id) = payload.get("item_id").filter(|value| !value.is_null()) {
                object.insert("id".into(), id.clone());
            }
            if let Some(phase) = payload.get("phase").filter(|value| !value.is_null()) {
                object.insert("phase".into(), phase.clone());
            }
            for key in ["content", "summary", "encrypted_content"] {
                if let Some(value) = payload.get(key).filter(|value| !value.is_null()) {
                    object.insert(key.into(), value.clone());
                }
            }
        }
        saved["model"] = json!(model);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn many_tool_calls_do_not_multiply_each_reasoning_capsule() {
        let key = FernetTokenEncryptor::new("fixture-key").unwrap();
        let mut items: Vec<_> = (0..512)
            .map(|index| {
                json!({"type":"reasoning", "id":format!("r{index}"),
                "content":[{"type":"reasoning_text", "text":"reason"}]})
            })
            .collect();
        items.extend((0..128).map(|index| {
            json!({
                "type":"function_call", "call_id":format!("{index:03}{}", "x".repeat(509)),
                "name":"read_file", "arguments":"{}"
            })
        }));
        let model = "\"".repeat(255);
        let mut saved = json!({"connection_id":"route", "model":model, "items":items});
        seal(&mut saved, &key, "owner", "session").unwrap();
        assert!(serde_json::to_vec(&saved).unwrap().len() < 1_000_000);
        let mut messages = vec![json!({"role":"assistant", CONTINUATION_KEY:saved})];
        open(
            &mut messages,
            &key,
            "owner",
            &HashSet::from(["session".to_string()]),
            "route",
            &model,
        )
        .unwrap();
        assert_eq!(
            messages[0][CONTINUATION_KEY]["items"][0]["content"][0]["text"],
            "reason"
        );
    }

    #[test]
    fn four_mib_reasoning_fits_the_real_canonical_segment_and_round_trips() {
        let key = FernetTokenEncryptor::new("fixture-key").unwrap();
        let content = "x".repeat(4 * 1024 * 1024);
        let mut continuation = json!({"connection_id":"route", "model":"model", "items":[
            {"type":"reasoning", "id":"reason", "content":[{"type":"reasoning_text", "text":content}]},
            {"type":"message", "id":"message", "role":"assistant", "content":[{"type":"output_text", "text":"answer"}]}
        ]});
        seal(&mut continuation, &key, "owner", "session").unwrap();
        let mut messages =
            vec![json!({"role":"assistant", "content":"answer", CONTINUATION_KEY:continuation})];
        assert!(
            astra_turn_types::json_serialized_fits(
                &messages[0],
                astra_core::runtime_limits::MAX_CANONICAL_CONTEXT_SEGMENT_BYTES
            )
            .unwrap()
        );
        open(
            &mut messages,
            &key,
            "owner",
            &HashSet::from(["session".to_string()]),
            "route",
            "model",
        )
        .unwrap();
        assert!(
            messages[0][CONTINUATION_KEY]["items"][0]["content"][0]["text"].as_str()
                == Some(content.as_str())
        );
    }

    #[test]
    fn credential_shaped_reasoning_is_sealed_and_replayed_without_disclosure() {
        let key = FernetTokenEncryptor::new("fixture-key").unwrap();
        let secret = "AKIAIOSFODNN7EXAMPLE";
        let call_id = "AKIAABCDEFGHIJKLMNOP";
        let item_id = "AKIA1111111111111111";
        let model = "AKIAQRSTUVWXYZABCDEF";
        let route = "11111111-1111-4111-8111-111111111111";
        let mut continuation = json!({"connection_id":route, "model":model, "items":[
            {"type":"reasoning", "id":secret, "content":[{"type":"reasoning_text", "text":secret}],
                "summary":[{"type":"summary_text", "text":secret}]},
            {"type":"function_call", "id":item_id, "call_id":call_id, "name":"read_file"}
        ]});
        seal(&mut continuation, &key, "owner", "session").unwrap();
        assert!(!continuation.to_string().contains(secret));
        assert!(!continuation.to_string().contains(item_id));
        assert!(!continuation.to_string().contains(model));
        let messages = vec![
            json!({"role":"user", "content":"question"}),
            json!({"role":"assistant", "content":"", "tool_calls":[{"id":call_id, "function":{"name":"read_file", "arguments":"{}"}}], CONTINUATION_KEY:continuation}),
            json!({"role":"tool", "tool_call_id":call_id, "content":"result"}),
        ];
        let canonical =
            astra_turn_core::prompt_facing::sanitize_canonical_turn_delta_with_turn_semantics(
                messages, false,
            )
            .unwrap();
        let mut messages =
            astra_turn_core::runtime_scaffolding::sanitize_durable_message_values(canonical);
        assert!(!serde_json::to_string(&messages).unwrap().contains(secret));
        let mut forged = messages.clone();
        forged[1][CONTINUATION_KEY]["items"][0]["summary"] =
            json!([{"type":"summary_text", "text":"forged"}]);
        assert!(
            open(
                &mut forged,
                &key,
                "owner",
                &HashSet::from(["session".into()]),
                route,
                model
            )
            .is_err()
        );
        open(
            &mut messages,
            &key,
            "owner",
            &HashSet::from(["session".into()]),
            route,
            model,
        )
        .unwrap();
        let request =
            astra_core::model_wire::responses::request(model, &messages, None, route, false, None)
                .unwrap();
        let reasoning = request["input"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["type"] == "reasoning")
            .unwrap();
        assert_eq!(reasoning["summary"][0]["text"], secret);
        assert_eq!(reasoning["content"][0]["text"], secret);
        assert_eq!(reasoning["id"], secret);
        let call = request["input"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["type"] == "function_call")
            .unwrap();
        assert_eq!(call["id"], item_id);
        assert_eq!(call["call_id"], call_id);
    }

    #[test]
    fn provider_opaque_reasoning_survives_credential_sanitization() {
        use astra_core::model_wire::responses::{ResponseOutput, request};
        let scope = uuid::Uuid::new_v4().to_string();
        let opaque = "AKIAABCDEFGHIJKLMNOP";
        let output = ResponseOutput {
            items: vec![
                json!({"type":"reasoning", "id":"r", "summary":[], "encrypted_content":opaque}),
                json!({"type":"message", "id":"message-1", "role":"assistant", "phase":"final_answer", "content":[{"type":"output_text", "text":"answer"}]}),
            ],
            ..Default::default()
        };
        let key = FernetTokenEncryptor::new("fixture-key").unwrap();
        let mut continuation = output.continuation(&scope, "model");
        assert!(
            astra_core::model_wire::responses::has_unsealed_continuation(
                &json!({"role":"assistant", CONTINUATION_KEY:continuation})
            )
        );
        seal(&mut continuation, &key, "owner", "session").unwrap();
        let messages = vec![
            json!({"role":"user", "content":"question"}),
            json!({"role":"assistant", "content":"answer", CONTINUATION_KEY:continuation}),
            json!({"role":"user", "content":"continue"}),
        ];
        let mut sanitized =
            astra_turn_core::runtime_scaffolding::sanitize_durable_message_values(messages.clone());
        assert_eq!(sanitized, messages);
        let stored = serde_json::to_string(&sanitized).unwrap();
        assert!(!stored.contains(opaque));
        assert!(
            !stored.contains(
                &data_encoding::BASE32_NOPAD
                    .encode(opaque.as_bytes())
                    .to_ascii_lowercase()
            )
        );
        assert!(!astra_core::model_wire::responses::has_unsealed_continuation(&sanitized[1]));
        open(
            &mut sanitized,
            &key,
            "owner",
            &HashSet::from(["session".to_owned()]),
            &scope,
            "model",
        )
        .unwrap();
        let body = request("model", &sanitized, None, &scope, false, None).unwrap();
        assert!(
            body["input"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["encrypted_content"] == opaque)
        );
        assert!(body["input"].as_array().unwrap().iter().any(|item| {
            item["type"] == "message"
                && item["id"] == "message-1"
                && item["phase"] == "final_answer"
        }));
    }

    #[test]
    fn metadata_only_reasoning_survives_commit_and_replay() {
        let key = FernetTokenEncryptor::new("fixture-key").unwrap();
        let route = uuid::Uuid::new_v4().to_string();
        let secret = "AKIAIOSFODNN7EXAMPLE";
        let mut saved = json!({"connection_id":route, "model":"model", "items":[
            {"type":"reasoning", "id":secret, "summary":[{"type":"summary_text", "text":secret}]},
            {"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":"answer"}]}
        ]});
        seal(&mut saved, &key, "owner", "session").unwrap();
        let messages = vec![
            json!({"role":"user", "content":"question"}),
            json!({"role":"assistant", "content":"answer", CONTINUATION_KEY:saved}),
            json!({"role":"user", "content":"continue"}),
        ];
        let committed =
            astra_turn_core::prompt_facing::sanitize_canonical_turn_delta_with_turn_semantics(
                messages, false,
            )
            .unwrap();
        let mut restored = astra_turn_core::prompt_facing::sanitize_canonical_continuation_messages_with_turn_semantics(
            astra_turn_core::runtime_scaffolding::sanitize_durable_message_values(committed),
        ).unwrap();
        assert!(!serde_json::to_string(&restored).unwrap().contains(secret));
        open(
            &mut restored,
            &key,
            "owner",
            &HashSet::from(["session".into()]),
            &route,
            "model",
        )
        .unwrap();
        let body = astra_core::model_wire::responses::request(
            "model", &restored, None, &route, false, None,
        )
        .unwrap();
        let reasoning = body["input"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["type"] == "reasoning")
            .unwrap();
        assert_eq!(reasoning["id"], secret);
        assert_eq!(reasoning["summary"][0]["text"], secret);
    }

    #[test]
    fn redacted_tool_name_cannot_change_a_sealed_call() {
        let key = FernetTokenEncryptor::new("fixture-key").unwrap();
        let name = "AKIAIOSFODNN7EXAMPLE";
        let mut saved = json!({"connection_id":"route", "model":"model", "items":[
            {"type":"function_call", "call_id":"call", "name":name}
        ]});
        seal(&mut saved, &key, "owner", "session").unwrap();
        let mut messages = astra_turn_core::runtime_scaffolding::sanitize_durable_message_values(
            vec![
                json!({"role":"assistant", "tool_calls":[{"id":"call", "function":{"name":name, "arguments":"{}"}}], CONTINUATION_KEY:saved}),
            ],
        );
        assert_ne!(messages[0][CONTINUATION_KEY]["items"][0]["name"], name);
        assert!(
            open(
                &mut messages,
                &key,
                "owner",
                &HashSet::from(["session".into()]),
                "route",
                "model"
            )
            .is_err()
        );
    }
    #[test]
    fn private_reasoning_survives_canonical_credential_sanitization() {
        let key = FernetTokenEncryptor::new("fixture-key").unwrap();
        let mut saved = json!({"connection_id":"route", "model":"model", "items":[
            {"type":"reasoning", "id":"r", "content":[{"type":"reasoning_text", "text":"private-sentinel"}]},
            {"type":"function_call", "call_id":"call-1", "name":"read_file"}
        ]});
        seal(&mut saved, &key, "owner", "session").unwrap();
        let ciphertext = saved["items"][0][SEALED_ITEM]["ciphertext"]
            .as_str()
            .unwrap();
        assert!(
            ciphertext
                .bytes()
                .all(|b| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b))
        );
        let original = vec![json!({"role":"assistant", CONTINUATION_KEY:saved})];
        let mut sanitized =
            astra_turn_core::runtime_scaffolding::sanitize_durable_message_values(original.clone());
        assert_eq!(sanitized, original);
        open(
            &mut sanitized,
            &key,
            "owner",
            &HashSet::from(["session".to_owned()]),
            "route",
            "model",
        )
        .unwrap();
        assert!(sanitized[0].to_string().contains("private-sentinel"));
    }
    #[test]
    fn private_reasoning_is_sealed_and_bound_to_owner_origin_route_and_item() {
        let key = FernetTokenEncryptor::new("fixture-key").unwrap();
        let mut saved = json!({"connection_id":"route", "model":"model", "items":[
            {"type":"reasoning", "id":"r", "content":[{"type":"reasoning_text", "text":"private-sentinel"}]},
            {"type":"function_call", "call_id":"call-1", "name":"read_file"},
            {"type":"function_call", "call_id":"call-2", "name":"read_file"}
        ]});
        seal(&mut saved, &key, "owner", "session").unwrap();
        assert!(!saved.to_string().contains("private-sentinel"));
        let original = vec![json!({CONTINUATION_KEY:saved})];
        let origins = HashSet::from(["session".to_owned()]);
        let mut restored = original.clone();
        open(&mut restored, &key, "owner", &origins, "route", "model").unwrap();
        assert!(restored[0].to_string().contains("private-sentinel"));
        let mut foreign = original.clone();
        assert!(open(&mut foreign, &key, "other", &origins, "route", "model").is_err());
        let mut wrong_key = original.clone();
        let other_key = FernetTokenEncryptor::new("different-fixture-key").unwrap();
        assert!(
            open(
                &mut wrong_key,
                &other_key,
                "owner",
                &origins,
                "route",
                "model"
            )
            .is_err()
        );
        let mut changed_call = original.clone();
        changed_call[0][CONTINUATION_KEY]["items"][1]["call_id"] = json!("different");
        assert!(open(&mut changed_call, &key, "owner", &origins, "route", "model").is_err());
        let mut changed_name = original.clone();
        changed_name[0][CONTINUATION_KEY]["items"][1]["name"] = json!("other");
        assert!(open(&mut changed_name, &key, "owner", &origins, "route", "model").is_err());
        let mut reordered = original.clone();
        reordered[0][CONTINUATION_KEY]["items"]
            .as_array_mut()
            .unwrap()
            .swap(1, 2);
        assert!(open(&mut reordered, &key, "owner", &origins, "route", "model").is_err());
        let mut reordered_types = original.clone();
        reordered_types[0][CONTINUATION_KEY]["items"]
            .as_array_mut()
            .unwrap()
            .swap(0, 1);
        assert!(
            open(
                &mut reordered_types,
                &key,
                "owner",
                &origins,
                "route",
                "model"
            )
            .is_err()
        );
        let mut missing_capsule = original.clone();
        missing_capsule[0][CONTINUATION_KEY]["items"][1]
            .as_object_mut()
            .unwrap()
            .remove(SEALED_ITEM);
        assert!(
            open(
                &mut missing_capsule,
                &key,
                "owner",
                &origins,
                "route",
                "model"
            )
            .is_err()
        );
        let mut completed = original.clone();
        discard_reasoning(&mut completed[0], None);
        open(&mut completed, &key, "owner", &origins, "route", "model").unwrap();
        assert_eq!(
            completed[0][CONTINUATION_KEY]["items"][0]["call_id"],
            "call-1"
        );
        let mut altered_ciphertext = original.clone();
        altered_ciphertext[0][CONTINUATION_KEY]["items"][0][SEALED_ITEM]["ciphertext"] =
            json!("bad");
        assert!(
            open(
                &mut altered_ciphertext,
                &key,
                "owner",
                &origins,
                "route",
                "model"
            )
            .is_err()
        );
        let mut foreign = original.clone();
        assert!(
            open(
                &mut foreign,
                &key,
                "owner",
                &HashSet::new(),
                "route",
                "model"
            )
            .is_err()
        );
        let mut altered = original;
        altered[0][CONTINUATION_KEY]["items"][0]["id"] = json!("different");
        assert!(open(&mut altered, &key, "owner", &origins, "route", "model").is_err());
    }

    #[test]
    fn current_turn_policy_omits_only_completed_turn_reasoning() {
        let prior = json!({"role":"assistant", CONTINUATION_KEY:{"items":[
            {"type":"reasoning", "content":[{"type":"reasoning_text", "text":"old"}]},
            {"type":"function_call", "call_id":"a", "name":"read_file"}
        ]}});
        let active = json!({"role":"assistant", CONTINUATION_KEY:{"items":[
            {"type":"reasoning", "content":[{"type":"reasoning_text", "text":"current"}]}
        ]}});
        let mut messages = vec![
            json!({"role":"user", "content":"first"}),
            prior,
            json!({"role":"tool", "tool_call_id":"a", "content":"result"}),
            json!({"role":"user", "content":"next"}),
            active.clone(),
        ];
        discard_completed_turn_reasoning(&mut messages);
        assert_eq!(
            messages[1][CONTINUATION_KEY]["items"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            messages[1][CONTINUATION_KEY]["items"][0]["type"],
            "function_call"
        );
        assert_eq!(messages[4], active);
        assert_eq!(messages[2]["content"], "result");
    }

    #[test]
    fn completed_commit_discards_only_the_declared_route() {
        let mut message = json!({"role":"assistant", CONTINUATION_KEY:{
            "connection_id":"one", "model":model_binding("model"), "items":[
                {"type":"reasoning", SEALED_ITEM:{"origin":"session", "ciphertext":"opaque"}},
                {"type":"function_call", "call_id":"call", "name":"read_file"}
            ]
        }});
        let original = message.clone();
        discard_reasoning(&mut message, Some(("other", "model")));
        assert_eq!(message, original);
        discard_reasoning(&mut message, Some(("one", "model")));
        assert_eq!(
            message[CONTINUATION_KEY]["items"].as_array().unwrap().len(),
            1
        );
        assert_eq!(message[CONTINUATION_KEY]["items"][0]["call_id"], "call");
    }

    #[test]
    fn current_turn_projection_avoids_historical_reasoning_growth() {
        let key = FernetTokenEncryptor::new("fixture-key").unwrap();
        let mut history = Vec::new();
        for index in 0..50 {
            history.push(json!({"role":"user", "content":format!("turn {index}")}));
            let mut saved = json!({"connection_id":"route", "model":"model", "items":[
                {"type":"reasoning", "id":format!("r{index}"),
                    "content":[{"type":"reasoning_text", "text":"x".repeat(8192)}]},
                {"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":"done"}]}
            ]});
            seal(&mut saved, &key, "owner", "session").unwrap();
            history.push(json!({"role":"assistant", "content":"done", CONTINUATION_KEY:saved}));
        }
        history.push(json!({"role":"user", "content":"next"}));
        let origins = HashSet::from(["session".to_owned()]);
        let mut all = history.clone();
        open(&mut all, &key, "owner", &origins, "route", "model").unwrap();
        let all_request =
            astra_core::model_wire::responses::request("model", &all, None, "route", false, None)
                .unwrap();
        discard_completed_turn_reasoning(&mut history);
        open(&mut history, &key, "owner", &origins, "route", "model").unwrap();
        let current_request = astra_core::model_wire::responses::request(
            "model", &history, None, "route", false, None,
        )
        .unwrap();
        assert!(all_request.to_string().len() - current_request.to_string().len() > 400_000);
        assert_eq!(
            current_request["input"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|item| item["type"] == "reasoning")
                .count(),
            0
        );
    }
}
