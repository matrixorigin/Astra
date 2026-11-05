//! Protect provider-private Responses reasoning within the canonical message.
use astra_core::model_wire::responses::{CONTINUATION_KEY, ENCODED_REASONING, SEALED_REASONING};
use astra_services::FernetTokenEncryptor;
use base64::{Engine as _, engine::general_purpose::URL_SAFE};
use serde_json::{Value, json};
use std::collections::HashSet;

// Bound the serialized plaintext before encrypting and base64-encoding it.
// The complete assistant/tool group is checked separately before dispatch.
const MAX_PRIVATE_REASONING_BYTES: u64 = 4 * 1024 * 1024;

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
    let calls: Vec<String> = continuation["items"]
        .as_array()
        .ok_or("missing items")?
        .iter()
        .filter(|item| item["type"] == "function_call")
        .map(|item| {
            item["call_id"]
                .as_str()
                .map(str::to_owned)
                .ok_or("missing call identity")
        })
        .collect::<Result<_, _>>()?;
    let items = continuation["items"]
        .as_array_mut()
        .ok_or("missing items")?;
    let mut remaining = MAX_PRIVATE_REASONING_BYTES;
    for item in items {
        if item.get(SEALED_REASONING).is_some() {
            return Err("provider supplied reserved reasoning metadata");
        }
        if item["type"] != "reasoning"
            || ["content", "encrypted_content", ENCODED_REASONING]
                .iter()
                .all(|key| item.get(*key).is_none_or(Value::is_null))
        {
            continue;
        }
        if owner.is_empty() || origin.is_empty() {
            return Err("missing reasoning owner");
        }
        let object = item.as_object_mut().ok_or("invalid item")?;
        let mut content = object.remove("content");
        if content.as_mut().is_some_and(|content| {
            astra_text_utils::credential_redaction::redact_credentials_in_json(content) > 0
        }) {
            return Err("private reasoning contains credentials");
        }
        let encoded = object.remove(ENCODED_REASONING);
        let raw = object.remove("encrypted_content");
        if encoded.is_some() && raw.is_some() {
            return Err("duplicate opaque reasoning");
        }
        let encrypted_content = if let Some(encoded) = encoded {
            let encoded = encoded.as_str().ok_or("invalid opaque reasoning")?;
            let bytes = astra_core::model_wire::responses::decode_opaque_reasoning(encoded)
                .map_err(|_| "invalid opaque reasoning")?;
            Some(String::from_utf8(bytes).map_err(|_| "invalid opaque reasoning")?)
        } else {
            raw.map(|value| {
                value
                    .as_str()
                    .map(str::to_owned)
                    .ok_or("invalid opaque reasoning")
            })
            .transpose()?
        };
        let payload = json!({
            "domain":"astra.responses.reasoning.v1",
            "owner":owner,
            "origin":origin,
            "scope":scope,
            "model":model,
            "calls":calls,
            "item_id":object.get("id").cloned().unwrap_or(Value::Null),
            "content":content,
            "encrypted_content":encrypted_content,
        });
        if !astra_turn_types::json_serialized_fits(&payload, remaining)
            .map_err(|_| "invalid reasoning")?
        {
            return Err("private reasoning exceeds storage budget");
        }
        let plaintext = serde_json::to_string(&payload).map_err(|_| "invalid reasoning")?;
        remaining = remaining
            .checked_sub(plaintext.len() as u64)
            .ok_or("private reasoning exceeds storage budget")?;
        let ciphertext = encryptor
            .encrypt(&plaintext)
            .map_err(|_| "cannot seal reasoning")?;
        // Fernet's base64 may coincidentally contain a credential-shaped
        // substring. Encode its bytes with an alphabet that cannot match the
        // credential redactor, so canonical sanitization cannot corrupt it.
        let ciphertext = URL_SAFE
            .decode(ciphertext)
            .map_err(|_| "invalid sealed reasoning")?;
        let ciphertext = astra_core::model_wire::responses::encode_opaque_reasoning(&ciphertext);
        object.insert(
            SEALED_REASONING.into(),
            json!({"origin":origin,"ciphertext":ciphertext}),
        );
    }
    Ok(())
}

pub(crate) fn origins(messages: &[Value], scope: &str, model: &str) -> HashSet<String> {
    messages
        .iter()
        .filter_map(|message| message.get(CONTINUATION_KEY))
        .filter(|saved| saved["connection_id"] == scope && saved["model"] == model)
        .filter_map(|saved| saved["items"].as_array())
        .flatten()
        .filter_map(|item| item[SEALED_REASONING]["origin"].as_str())
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
    if route
        .is_some_and(|(scope, model)| saved["connection_id"] != scope || saved["model"] != model)
    {
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
    for message in messages {
        let Some(saved) = message.get_mut(CONTINUATION_KEY) else {
            continue;
        };
        if saved["connection_id"] != scope || saved["model"] != model {
            continue;
        }
        let calls: Vec<String> = saved["items"]
            .as_array()
            .ok_or("invalid continuation")?
            .iter()
            .filter(|item| item["type"] == "function_call")
            .map(|item| {
                item["call_id"]
                    .as_str()
                    .map(str::to_owned)
                    .ok_or("missing call identity")
            })
            .collect::<Result<_, _>>()?;
        let items = saved["items"]
            .as_array_mut()
            .ok_or("invalid continuation")?;
        for item in items {
            if item["type"] == "reasoning"
                && ["content", "encrypted_content", ENCODED_REASONING]
                    .iter()
                    .any(|key| item.get(*key).is_some_and(|value| !value.is_null()))
            {
                return Err("unsealed reasoning");
            }
            let Some(sealed) = item.get(SEALED_REASONING) else {
                continue;
            };
            let origin = sealed["origin"].as_str().ok_or("invalid origin")?;
            if !allowed_origins.contains(origin) {
                return Err("origin not authorized");
            }
            let ciphertext = sealed["ciphertext"].as_str().ok_or("invalid ciphertext")?;
            let ciphertext = astra_core::model_wire::responses::decode_opaque_reasoning(ciphertext)
                .map_err(|_| "invalid ciphertext")?;
            let plaintext = encryptor
                .decrypt(&URL_SAFE.encode(ciphertext))
                .map_err(|_| "cannot open reasoning")?;
            let payload: Value =
                serde_json::from_str(&plaintext).map_err(|_| "invalid reasoning")?;
            if payload["domain"] != "astra.responses.reasoning.v1"
                || payload["owner"] != owner
                || payload["origin"] != origin
                || payload["scope"] != scope
                || payload["model"] != model
                || payload["calls"] != json!(calls)
                || payload["item_id"] != item.get("id").cloned().unwrap_or(Value::Null)
            {
                return Err("reasoning binding mismatch");
            }
            let object = item.as_object_mut().ok_or("invalid item")?;
            object.remove(SEALED_REASONING);
            for key in ["content", "encrypted_content"] {
                if let Some(value) = payload.get(key).filter(|value| !value.is_null()) {
                    object.insert(key.into(), value.clone());
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn provider_opaque_reasoning_survives_credential_sanitization() {
        use astra_core::model_wire::responses::{ResponseOutput, request};
        let scope = uuid::Uuid::new_v4().to_string();
        let opaque = "AKIAABCDEFGHIJKLMNOP";
        let output = ResponseOutput {
            items: vec![
                json!({"type":"reasoning", "id":"r", "summary":[], "encrypted_content":opaque}),
                json!({"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":"answer"}]}),
            ],
            ..Default::default()
        };
        let key = FernetTokenEncryptor::new("fixture-key").unwrap();
        let mut continuation = output.continuation(&scope, "model");
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
            !stored.contains(&astra_core::model_wire::responses::encode_opaque_reasoning(
                opaque.as_bytes()
            ))
        );
        assert!(!astra_core::model_wire::responses::has_unsealed_reasoning(
            &sanitized[1]
        ));
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
    }
    #[test]
    fn private_reasoning_survives_canonical_credential_sanitization() {
        let key = FernetTokenEncryptor::new("fixture-key").unwrap();
        let mut saved = json!({"connection_id":"route", "model":"model", "items":[
            {"type":"reasoning", "id":"r", "content":[{"type":"reasoning_text", "text":"private-sentinel"}]},
            {"type":"function_call", "call_id":"call-1", "name":"read_file"}
        ]});
        seal(&mut saved, &key, "owner", "session").unwrap();
        let ciphertext = saved["items"][0][SEALED_REASONING]["ciphertext"]
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
            {"type":"function_call", "call_id":"call-1", "name":"read_file"}
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
        let mut altered_ciphertext = original.clone();
        altered_ciphertext[0][CONTINUATION_KEY]["items"][0][SEALED_REASONING]["ciphertext"] =
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
            "connection_id":"one", "model":"model", "items":[
                {"type":"reasoning", SEALED_REASONING:{"origin":"session", "ciphertext":"opaque"}},
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
