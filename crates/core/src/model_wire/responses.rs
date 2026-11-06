//! The Responses wire codec shared by subscription probes and runtime calls.
//! It owns no conversation, tool execution, credential, or retry lifecycle.
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};

pub const CONTINUATION_KEY: &str = "_astra_responses";
/// Scoped evidence that provider reasoning could not be retained.
pub const HISTORY_INCOMPLETE: &str = "history_incomplete";
pub const CONTINUATION_SCOPE_CHANGED: &str =
    "Model configuration changed during tool continuation; start a new turn";
pub const CONTINUATION_INCOMPLETE: &str = "Tool continuation is incomplete; start a new turn";
pub const CONTINUATION_HISTORY_INCOMPLETE: &str =
    "Saved model context is incomplete; start a new session or choose a different model";
pub const REASONING_CONTINUATION_MISSING: &str = "responses_reasoning_continuation_missing";
pub const INVALID_OUTPUT: &str = "responses_invalid_output";
/// Provider-declared retention policy for completed human turns.
#[derive(Clone, Copy, Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningHistory {
    CurrentTurn,
    AllTurns,
}
impl ReasoningHistory {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CurrentTurn => "current_turn",
            Self::AllTurns => "all_turns",
        }
    }
}
/// Server-sealed provider item metadata, opened only by the inference owner.
pub const SEALED_ITEM: &str = "_astra_sealed_item";
const MAX_CONTINUATION_ITEMS: usize = 4096;
const MAX_REASONING_PARTS: usize = 4096;
const MAX_REASONING_BYTES: usize = 16 * 1024 * 1024;

pub fn model_binding(model: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"astra.responses.model.v1\0");
    digest.update(model.as_bytes());
    format!("sha256:{:x}", digest.finalize())
}

pub fn is_model_binding(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

/// Project a conversation message for consumers that do not own replay.
pub fn without_continuation(message: &Value) -> Value {
    match message.as_object() {
        Some(object) if object.contains_key(CONTINUATION_KEY) => Value::Object(
            object
                .iter()
                .filter(|(key, _)| key.as_str() != CONTINUATION_KEY)
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        ),
        _ => message.clone(),
    }
}

/// Detect provider reasoning fields that have not crossed the sealing boundary.
pub fn has_unsealed_reasoning_item(item: &Value) -> bool {
    item["type"] == "reasoning"
        && ["id", "content", "summary", "encrypted_content"]
            .iter()
            .any(|key| item.get(*key).is_some_and(|value| !value.is_null()))
}

/// Reject unprotected or noncanonical provider metadata at user-facing reads.
pub fn has_unsealed_continuation(message: &Value) -> bool {
    let Some(saved) = message.get(CONTINUATION_KEY) else {
        return false;
    };
    if saved
        .as_object()
        .is_some_and(|object| object.len() == 1 && saved["invalid"] == true)
    {
        return false;
    }
    if !saved["model"].as_str().is_some_and(is_model_binding) {
        return true;
    }
    let Some(validated) = validated_continuation(message) else {
        return true;
    };
    if &validated != saved {
        return true;
    }
    saved[HISTORY_INCOMPLETE] != true
        && saved["items"]
            .as_array()
            .is_none_or(|items| items.iter().any(|item| item.get(SEALED_ITEM).is_none()))
}

/// Whether this message carries, or records the loss of, provider reasoning.
pub fn has_reasoning_continuation(message: &Value) -> bool {
    let saved = &message[CONTINUATION_KEY];
    message["role"] == "assistant"
        && (saved[HISTORY_INCOMPLETE] == true
            || saved["invalid"] == true
            || saved["items"]
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item["type"] == "reasoning")))
}

/// Whether a saved Responses item belongs to a tool round.
pub fn has_tool_continuation(message: &Value) -> bool {
    message["role"] == "assistant"
        && message.get(CONTINUATION_KEY).is_some_and(|saved| {
            saved["invalid"] == true
                || message["tool_calls"]
                    .as_array()
                    .is_some_and(|calls| !calls.is_empty())
                || saved["items"].as_array().is_some_and(|items| {
                    items.len() > MAX_CONTINUATION_ITEMS
                        || items.iter().any(|item| item["type"] == "function_call")
                })
        })
}

/// Construct the same endpoint for Responses requests and their route identity.
pub fn endpoint_url(base_url: &str) -> String {
    super::endpoint_url(base_url, "responses")
}

/// Retain only scoped, replayable assistant protocol state at canonical
/// persistence boundaries. This metadata grants no tool execution authority.
pub fn validated_continuation(message: &Value) -> Option<Value> {
    if message["role"] != "assistant" {
        return None;
    }
    let saved = message.get(CONTINUATION_KEY)?;
    let connection = saved["connection_id"].as_str()?;
    if uuid::Uuid::parse_str(connection).is_err()
        && !connection.strip_prefix("sha256:").is_some_and(|digest| {
            digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
    {
        return None;
    }
    let model = saved["model"].as_str()?;
    if model.is_empty() || model.len() > 255 || model.chars().any(char::is_control) {
        return None;
    }
    if saved[HISTORY_INCOMPLETE] == true {
        return Some(json!({"connection_id":connection, "model":model, HISTORY_INCOMPLETE:true}));
    }
    let raw = saved["items"].as_array()?;
    if raw.is_empty() || raw.len() > MAX_CONTINUATION_ITEMS {
        return None;
    }
    validate_reasoning_budget(raw.iter()).ok()?;
    let items = raw
        .iter()
        .map(|item| replay_item(item, true))
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    Some(
        ResponseOutput {
            items,
            ..Default::default()
        }
        .continuation(connection, model),
    )
}

/// Stable protocol errors and their HTTP equivalents, including SSE failures.
/// Unknown upstream prose/codes never become public error details.
pub fn provider_error(code: &str) -> Option<(&'static str, u16)> {
    match code {
        "subscription_sharing_user_not_eligible" => {
            Some(("subscription_sharing_user_not_eligible", 403))
        }
        "subscription_sharing_usage_limit_exceeded" => {
            Some(("subscription_sharing_usage_limit_exceeded", 429))
        }
        "subscription_sharing_usage_unavailable" => {
            Some(("subscription_sharing_usage_unavailable", 503))
        }
        "context_length_exceeded" | "context_window_exceeded" | "input_too_long" => {
            Some(("context_length_exceeded", 400))
        }
        _ => None,
    }
}

/// Failure to assemble a Responses stream. `Provider` keeps the upstream
/// terminal-event code untouched so each caller maps it to its own public
/// vocabulary; `Codec` carries machine-owned literals only. Neither variant
/// may be echoed into user-facing details.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResponsesStreamError {
    Provider(Option<String>),
    Codec(&'static str),
}
impl ResponsesStreamError {
    /// Canonical (code, HTTP status) for recognized provider failures.
    pub fn mapped(&self) -> Option<(&'static str, u16)> {
        match self {
            ResponsesStreamError::Provider(Some(code)) => provider_error(code),
            _ => None,
        }
    }
}

fn reasoning_size(item: &Value) -> Result<(usize, usize), &'static str> {
    if item["type"] != "reasoning" {
        return Ok((0, 0));
    }
    if let Some(sealed) = item.get(SEALED_ITEM) {
        let size = sealed["ciphertext"]
            .as_str()
            .ok_or("Invalid sealed Responses reasoning")?
            .len();
        if size > MAX_REASONING_BYTES {
            return Err("Responses reasoning limit exceeded");
        }
        return Ok((1, size));
    }
    let Some(content) = item.get("content").filter(|content| !content.is_null()) else {
        return Ok((0, 0));
    };
    let parts = content
        .as_array()
        .ok_or("Invalid Responses reasoning content")?;
    if parts.len() > MAX_REASONING_PARTS {
        return Err("Responses reasoning limit exceeded");
    }
    let mut bytes = 0usize;
    for part in parts {
        if part["type"] != "reasoning_text" {
            return Err("Invalid Responses reasoning part");
        }
        let text = part["text"]
            .as_str()
            .ok_or("Invalid Responses reasoning part")?;
        bytes = bytes
            .checked_add(text.len())
            .filter(|bytes| *bytes <= MAX_REASONING_BYTES)
            .ok_or("Responses reasoning limit exceeded")?;
    }
    Ok((parts.len(), bytes))
}

fn validate_reasoning_budget<'a>(
    items: impl Iterator<Item = &'a Value>,
) -> Result<(), &'static str> {
    let mut total = (0usize, 0usize);
    for item in items {
        let size = reasoning_size(item)?;
        total.0 = total
            .0
            .checked_add(size.0)
            .ok_or("Responses reasoning limit exceeded")?;
        total.1 = total
            .1
            .checked_add(size.1)
            .ok_or("Responses reasoning limit exceeded")?;
        if total.0 > MAX_REASONING_PARTS || total.1 > MAX_REASONING_BYTES {
            return Err("Responses reasoning limit exceeded");
        }
    }
    Ok(())
}

fn replay_item(item: &Value, allow_sealed: bool) -> Result<Value, String> {
    if !allow_sealed && item.get(SEALED_ITEM).is_some() {
        return Err("Provider output contains reserved continuation metadata".into());
    }
    if item
        .get("status")
        .is_some_and(|status| !status.is_null() && status != "completed")
    {
        return Err("Provider continuation item is unfinished".into());
    }
    if item["type"] == "function_call"
        && item.get("namespace").is_some_and(|value| !value.is_null())
    {
        return Err("Provider function call has an unexpected namespace".into());
    }
    if item["type"] == "reasoning"
        && item
            .get("encrypted_content")
            .is_some_and(|value| !value.is_null() && !value.is_string())
    {
        return Err("Invalid encrypted Responses reasoning".into());
    }
    validate_reasoning_budget(std::iter::once(item))?;
    // Reasoning text is ordinary provider content, not an opaque encrypted
    // token. Validate its wire shape before copying it into the continuation.
    let reasoning_content = if item["type"] == "reasoning" {
        item.get("content")
            .filter(|content| !content.is_null())
            .map(|content| {
                let parts = content
                    .as_array()
                    .ok_or("Invalid Responses reasoning content")?;
                parts
                    .iter()
                    .map(|part| {
                        if part["type"] != "reasoning_text" {
                            return Err("Invalid Responses reasoning part");
                        }
                        let text = part["text"]
                            .as_str()
                            .ok_or("Invalid Responses reasoning part")?;
                        Ok(json!({"type":"reasoning_text", "text":text}))
                    })
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?
    } else {
        None
    };
    if item.get(SEALED_ITEM).is_some()
        && (!matches!(
            item["type"].as_str(),
            Some("reasoning" | "message" | "function_call")
        ) || item.get("id").is_some_and(|value| !value.is_null())
            || item.get("phase").is_some_and(|value| !value.is_null())
            || has_unsealed_reasoning_item(item)
            || item[SEALED_ITEM].as_object().is_none_or(|value| {
                value
                    .get("origin")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
                    || value
                        .get("ciphertext")
                        .and_then(Value::as_str)
                        .is_none_or(str::is_empty)
            }))
    {
        return Err("Invalid sealed Responses reasoning".into());
    }
    let keys: &[&str] = match item["type"].as_str() {
        Some("message") => &[
            "id",
            "type",
            "role",
            "status",
            "phase",
            "content",
            SEALED_ITEM,
        ],
        Some("reasoning") => &[
            "id",
            "type",
            "status",
            "summary",
            "encrypted_content",
            SEALED_ITEM,
        ],
        Some("function_call") => &[
            "id",
            "type",
            "status",
            "name",
            "arguments",
            "call_id",
            SEALED_ITEM,
        ],
        _ => return Err("Unsupported provider continuation item".into()),
    };
    let mut object = Map::new();
    for key in keys {
        // Optional null metadata is unexpressed; omit it on replay rather
        // than turning a valid output null into an invalid input control.
        if let Some(value) = item.get(*key).filter(|value| !value.is_null()) {
            if *key == SEALED_ITEM {
                object.insert(
                    (*key).into(),
                    json!({"origin":value["origin"], "ciphertext":value["ciphertext"]}),
                );
            } else {
                object.insert((*key).into(), value.clone());
            }
        }
    }
    if let Some(content) = reasoning_content {
        object.insert("content".into(), Value::Array(content));
    }
    let item = Value::Object(object);
    if item["type"] == "message" {
        if item["role"] != "assistant" {
            return Err("Invalid Responses message role".into());
        }
        let parts = item["content"]
            .as_array()
            .ok_or("Invalid Responses message content")?;
        for part in parts {
            let value = match part["type"].as_str() {
                Some("output_text") => part["text"].as_str(),
                Some("refusal") => part["refusal"].as_str(),
                _ => None,
            };
            if value.is_none() {
                return Err("Invalid Responses message part".into());
            }
        }
    } else if item["type"] == "function_call" {
        let valid = |value: &Value, max_len| {
            value.as_str().is_some_and(|value| {
                !value.is_empty() && value.len() <= max_len && !value.chars().any(char::is_control)
            })
        };
        if !valid(&item["call_id"], 512) || !valid(&item["name"], 128) {
            return Err("Invalid Responses function identity".into());
        }
    }
    Ok(item)
}

fn has_replayable_reasoning(item: &Value) -> bool {
    item["type"] != "reasoning"
        || item["encrypted_content"]
            .as_str()
            .is_some_and(|content| !content.is_empty())
        || item[SEALED_ITEM].is_object()
        || item["content"].as_array().is_some_and(|parts| {
            !parts.is_empty()
                && parts
                    .iter()
                    .all(|part| part["type"] == "reasoning_text" && part["text"].is_string())
        })
}

fn continuation_calls<'a>(
    message: &'a Value,
    saved: &'a Value,
    following: &[Value],
    provider_id: impl Fn(&'a Value) -> Option<&'a str>,
    validate_items: bool,
) -> Result<HashMap<&'a str, &'a Value>, String> {
    let items = saved["items"].as_array().ok_or(CONTINUATION_INCOMPLETE)?;
    if items.is_empty()
        || items.len() > MAX_CONTINUATION_ITEMS
        || message["tool_calls"]
            .as_array()
            .is_some_and(|calls| calls.len() > MAX_CONTINUATION_ITEMS)
    {
        return Err(CONTINUATION_INCOMPLETE.into());
    }
    validate_reasoning_budget(items.iter())?;
    let mut calls = HashMap::new();
    let mut pending = HashSet::new();
    for call in message
        .get("tool_calls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let id = call["id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or(CONTINUATION_INCOMPLETE)?;
        let wire_id = provider_id(call).unwrap_or(id);
        if !pending.insert(id) || calls.insert(wire_id, call).is_some() {
            return Err(CONTINUATION_INCOMPLETE.into());
        }
    }
    let mut seen = HashSet::new();
    for item in items {
        if validate_items {
            replay_item(item, true)?;
        }
        if !calls.is_empty() && !has_replayable_reasoning(item) {
            return Err(CONTINUATION_INCOMPLETE.into());
        }
        if item["type"] != "function_call" {
            continue;
        }
        if item.get("namespace").is_some_and(|value| !value.is_null()) {
            return Err(CONTINUATION_INCOMPLETE.into());
        }
        let id = item["call_id"].as_str().ok_or(CONTINUATION_INCOMPLETE)?;
        let call = calls.get(id).ok_or(CONTINUATION_INCOMPLETE)?;
        if !seen.insert(id)
            || item["name"] != call["function"]["name"]
            || call["function"]["arguments"].as_str().is_none()
        {
            return Err(CONTINUATION_INCOMPLETE.into());
        }
    }
    if seen.len() != calls.len() {
        return Err(CONTINUATION_INCOMPLETE.into());
    }
    for result in following
        .iter()
        .take_while(|message| message["role"] == "tool")
    {
        let id = result["tool_call_id"]
            .as_str()
            .ok_or(CONTINUATION_INCOMPLETE)?;
        if !pending.remove(id) {
            return Err(CONTINUATION_INCOMPLETE.into());
        }
    }
    if !pending.is_empty() {
        return Err(CONTINUATION_INCOMPLETE.into());
    }
    Ok(calls)
}

/// Compare the saved function calls to a canonical group. Callers must also
/// validate the saved item contents at their trust boundary.
pub fn continuation_group_matches<'a>(
    message: &'a Value,
    following: &[Value],
    provider_id: impl Fn(&'a Value) -> Option<&'a str>,
) -> bool {
    message.get(CONTINUATION_KEY).is_some_and(|saved| {
        continuation_calls(message, saved, following, provider_id, false).is_ok()
    })
}

/// Validate the current human turn before runtime context is projected to user messages.
pub fn validate_active_continuations<'a>(
    messages: &'a [Value],
    model: &str,
    connection: &str,
    provider_id: impl Fn(&'a Value) -> Option<&'a str>,
) -> Result<(), String> {
    for (index, message) in messages.iter().enumerate() {
        if message["role"] != "assistant" {
            continue;
        }
        let has_calls = match message.get("tool_calls") {
            None | Some(Value::Null) => false,
            Some(Value::Array(calls)) => !calls.is_empty(),
            Some(_) => return Err(CONTINUATION_INCOMPLETE.into()),
        };
        let Some(saved) = message.get(CONTINUATION_KEY) else {
            if has_calls {
                return Err(CONTINUATION_INCOMPLETE.into());
            }
            continue;
        };
        if saved[HISTORY_INCOMPLETE] == true
            && saved["connection_id"] == connection
            && saved["model"] == model
        {
            return Err(CONTINUATION_HISTORY_INCOMPLETE.into());
        }
        if saved["invalid"] == true
            || saved["connection_id"].as_str().is_none_or(str::is_empty)
            || saved["model"].as_str().is_none_or(str::is_empty)
        {
            return Err(CONTINUATION_INCOMPLETE.into());
        }
        let has_calls = has_tool_continuation(message);
        if saved["connection_id"] != connection || saved["model"] != model {
            if has_calls {
                return Err(CONTINUATION_SCOPE_CHANGED.into());
            }
        } else {
            continuation_calls(message, saved, &messages[index + 1..], &provider_id, true)?;
        }
    }
    Ok(())
}

fn replay_continuation(
    message: &Value,
    saved: &Value,
    following: &[Value],
) -> Result<Vec<Value>, String> {
    let calls = continuation_calls(message, saved, following, |_| None, false)?;
    let items = saved["items"].as_array().ok_or(CONTINUATION_INCOMPLETE)?;
    let mut replayed = Vec::with_capacity(items.len());
    for item in items {
        if item.get(SEALED_ITEM).is_some() {
            return Err(CONTINUATION_INCOMPLETE.into());
        }
        let mut replay = replay_item(item, true)?;
        if replay["type"] == "function_call" {
            let id = replay["call_id"].as_str().ok_or(CONTINUATION_INCOMPLETE)?;
            replay["arguments"] = calls[id]["function"]["arguments"].clone();
        }
        replayed.push(replay);
    }
    Ok(replayed)
}

/// Project the same input items used by a Responses request without retaining
/// the whole request. A replay group is buffered so a failed replay can fall
/// back atomically. Callers must discard partial observations on failure.
pub fn visit_input_items(
    model: &str,
    messages: &[Value],
    connection: &str,
    mut visit: impl FnMut(Value),
) -> Result<(), String> {
    // Outer None means not yet searched; inner None means no user role was found.
    // Runtime checks the separate human boundary before projection.
    let mut last_user: Option<Option<usize>> = None;
    for (index, message) in messages.iter().enumerate() {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .ok_or("Message role is missing")?;
        if role == "assistant" && message[CONTINUATION_KEY]["invalid"] == true {
            return Err(CONTINUATION_HISTORY_INCOMPLETE.into());
        }
        if role == "assistant"
            && let Some(saved) = message.get(CONTINUATION_KEY)
            && (saved["connection_id"] != connection || saved["model"] != model)
            && has_tool_continuation(message)
            && !last_user
                .get_or_insert_with(|| {
                    messages
                        .iter()
                        .rposition(|message| message["role"] == "user")
                })
                .is_some_and(|user_index| user_index > index)
        {
            return Err(CONTINUATION_SCOPE_CHANGED.into());
        }
        if role == "assistant"
            && let Some(saved) = message.get(CONTINUATION_KEY)
            && saved["connection_id"] == connection
            && saved["model"] == model
        {
            if saved[HISTORY_INCOMPLETE] == true {
                return Err(CONTINUATION_HISTORY_INCOMPLETE.into());
            }
            match replay_continuation(message, saved, &messages[index + 1..]) {
                Ok(items) => {
                    items.into_iter().for_each(&mut visit);
                    continue;
                }
                Err(error) => {
                    let has_later_user = last_user
                        .get_or_insert_with(|| {
                            messages
                                .iter()
                                .rposition(|message| message["role"] == "user")
                        })
                        .is_some_and(|user_index| user_index > index);
                    if !has_later_user {
                        return Err(error);
                    }
                    // A route may require its earlier reasoning. Falling back
                    // to ordinary chat messages would silently discard it.
                    if has_reasoning_continuation(message) {
                        return Err(CONTINUATION_HISTORY_INCOMPLETE.into());
                    }
                }
            }
        }
        if role == "tool" {
            let id = message["tool_call_id"]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or("Tool result has no call identity")?;
            let content = message.get("content").unwrap_or(&Value::Null);
            let output = content
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| content.to_string());
            visit(json!({"type":"function_call_output","call_id":id,"output":output}));
            continue;
        }
        let role = match role {
            "system" | "developer" => "developer",
            "user" => "user",
            "assistant" => "assistant",
            _ => return Err("Unsupported Responses message role".into()),
        };
        let content = match message.get("content") {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(text)) => text.clone(),
            Some(Value::Array(parts)) => {
                let mut text = String::new();
                for part in parts {
                    if !matches!(
                        part["type"].as_str(),
                        Some("text" | "input_text" | "output_text")
                    ) {
                        return Err("ChatGPT subscription v1 supports text inputs only".into());
                    }
                    text.push_str(part["text"].as_str().ok_or("Invalid text message part")?);
                }
                text
            }
            _ => return Err("Unsupported message content".into()),
        };
        if !content.is_empty() {
            let mut item = if role == "assistant" {
                json!({"role":"assistant","content":content})
            } else {
                json!({"role":role,"content":[{"type":"input_text","text":content}]})
            };
            if role == "assistant"
                && let Some(phase) = message
                    .get("phase")
                    .filter(|v| matches!(v.as_str(), Some("commentary" | "final_answer")))
            {
                item["phase"] = phase.clone();
            }
            visit(item);
        }
        if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                let id = call["id"]
                    .as_str()
                    .ok_or("Assistant tool call has no identity")?;
                let name = call["function"]["name"]
                    .as_str()
                    .ok_or("Assistant tool call has no name")?;
                let arguments = call["function"]["arguments"]
                    .as_str()
                    .ok_or("Assistant tool arguments are invalid")?;
                visit(
                    json!({"type":"function_call","call_id":id,"name":name,"arguments":arguments}),
                );
            }
        }
    }
    Ok(())
}

pub fn request(
    model: &str,
    messages: &[Value],
    tools: Option<&[Value]>,
    connection: &str,
    no_tools: bool,
    max_output_tokens: Option<usize>,
) -> Result<Value, String> {
    let mut input = Vec::new();
    visit_input_items(model, messages, connection, |item| input.push(item))?;
    let mut body = json!({"model":model,"input":input,"store":false,"stream":true});
    if let Some(limit) = max_output_tokens.filter(|limit| *limit > 0) {
        body["max_output_tokens"] = json!(limit);
    }
    if let Some(tools) = tools.filter(|tools| !tools.is_empty()) {
        let mut definitions = Vec::new();
        for tool in tools {
            if tool["type"] != "function" {
                return Err("Unsupported Responses hosted tool".into());
            }
            let function = tool.get("function").unwrap_or(tool);
            let mut function = function
                .as_object()
                .cloned()
                .ok_or("Invalid function schema")?;
            function.retain(|key, _| matches!(key.as_str(), "name" | "description" | "parameters"));
            function.insert("type".into(), json!("function"));
            function.insert("strict".into(), json!(false));
            definitions.push(Value::Object(function));
        }
        body["tools"] = Value::Array(definitions);
    }
    body["tool_choice"] = json!(if no_tools { "none" } else { "auto" });
    Ok(body)
}

#[derive(Clone, Debug, Default)]
pub struct ResponseOutput {
    pub text: String,
    pub reasoning: String,
    pub tool_calls: Vec<Value>,
    pub usage: Map<String, Value>,
    pub response_id: Option<String>,
    pub items: Vec<Value>,
    /// Terminal finish evidence owned by the provider: `Some("length")` only
    /// when the response ended incomplete with reason `max_output_tokens`.
    /// Ordinary completions leave this `None`; callers own the default
    /// stop/tool_calls distinction.
    pub finish_reason: Option<String>,
}
impl ResponseOutput {
    pub fn continuation(&self, connection: &str, model: &str) -> Value {
        // An unfinished tool round needs replayable reasoning. A text-only
        // response may still carry provider-owned metadata for later turns.
        let has_calls = self
            .items
            .iter()
            .any(|item| item["type"] == "function_call");
        let mut items: Vec<_> = self
            .items
            .iter()
            .filter(|item| !has_calls || has_replayable_reasoning(item))
            .cloned()
            .collect();
        for item in &mut items {
            if item["type"] == "function_call" {
                // Canonical tool arguments own normalization/redaction. Replay
                // hydrates them from the assistant frame instead of retaining
                // a second raw copy in private metadata.
                if let Some(object) = item.as_object_mut() {
                    object.remove("arguments");
                }
            }
        }
        json!({"connection_id":connection,"model":model,"items":items})
    }
}

#[derive(Default)]
pub struct ResponseAccumulator {
    pub output: ResponseOutput,
    items: BTreeMap<u64, Value>,
    reasoning_parts: usize,
    reasoning_bytes: usize,
    text_ranges: BTreeMap<(u64, u64), Vec<std::ops::Range<usize>>>,
    range_count: usize,
    unindexed_text: bool,
    tracking_overflow: bool,
    completed: bool,
}
impl ResponseAccumulator {
    /// Returns a text/reasoning delta for the existing presentation owner.
    pub fn observe(
        &mut self,
        event: &Value,
    ) -> Result<Option<(String, bool)>, ResponsesStreamError> {
        let kind = event["type"].as_str().ok_or(ResponsesStreamError::Codec(
            "Responses event type is missing",
        ))?;
        if self.completed {
            if kind == "response.completed" {
                return Err(ResponsesStreamError::Codec(
                    "Duplicate Responses completion",
                ));
            }
            return Ok(None);
        }
        // Successful and failed terminal events both own response identity and
        // measured usage. Preserve that evidence before validating completion.
        let response = &event["response"];
        if let Some(id) = response["id"].as_str() {
            self.output.response_id = Some(id.to_owned());
        }
        if let Some(usage) = response["usage"].as_object() {
            self.output.usage = usage.clone();
            for (source, target) in [
                ("input_tokens", "prompt_tokens"),
                ("output_tokens", "completion_tokens"),
                ("input_tokens_details", "prompt_tokens_details"),
                ("output_tokens_details", "completion_tokens_details"),
            ] {
                if let Some(value) = usage.get(source) {
                    self.output.usage.insert(target.into(), value.clone());
                }
            }
        }
        match kind {
            "response.output_text.delta" => {
                let delta = event["delta"]
                    .as_str()
                    .ok_or(ResponsesStreamError::Codec("Invalid Responses text delta"))?;
                // Store observations once; ranges preserve their item/part ownership.
                if self.output.text.len().saturating_add(delta.len()) > 16 * 1024 * 1024 {
                    return Err(ResponsesStreamError::Codec("Responses text limit exceeded"));
                }
                if !delta.is_empty() && !self.tracking_overflow {
                    match (
                        event["output_index"].as_u64(),
                        event["content_index"].as_u64(),
                    ) {
                        (Some(item), Some(part)) => {
                            let key = (item, part);
                            if self.text_ranges.len() >= 4096
                                && !self.text_ranges.contains_key(&key)
                            {
                                self.tracking_overflow = true;
                            } else {
                                let ranges = self.text_ranges.entry(key).or_default();
                                let start = self.output.text.len();
                                let end = start + delta.len();
                                if let Some(last) =
                                    ranges.last_mut().filter(|last| last.end == start)
                                {
                                    last.end = end;
                                } else if self.range_count >= 65536 {
                                    self.tracking_overflow = true;
                                } else {
                                    ranges.push(start..end);
                                    self.range_count += 1;
                                }
                            }
                            if self.tracking_overflow {
                                self.text_ranges.clear();
                            }
                        }
                        _ => self.unindexed_text = true,
                    }
                }
                self.output.text.push_str(delta);
                return Ok(Some((delta.into(), false)));
            }
            "response.reasoning_summary_text.delta" => {
                let delta = event["delta"].as_str().ok_or(ResponsesStreamError::Codec(
                    "Invalid Responses reasoning summary",
                ))?;
                self.output.reasoning.push_str(delta);
                return Ok(Some((delta.into(), true)));
            }
            "response.reasoning_text.delta" => {
                // Raw reasoning is private. It counts as stream progress, but
                // only the completed item can authorize a later replay.
                event["delta"].as_str().ok_or(ResponsesStreamError::Codec(
                    "Invalid Responses reasoning text delta",
                ))?;
            }
            "response.output_item.done" => {
                let index = event["output_index"]
                    .as_u64()
                    .ok_or(ResponsesStreamError::Codec("Responses item index missing"))?;
                if self.items.len() >= 4096 && !self.items.contains_key(&index) {
                    return Err(ResponsesStreamError::Codec(
                        "Too many Responses output items",
                    ));
                }
                let size = reasoning_size(&event["item"]).map_err(ResponsesStreamError::Codec)?;
                let previous = self
                    .items
                    .get(&index)
                    .map(reasoning_size)
                    .transpose()
                    .map_err(ResponsesStreamError::Codec)?
                    .unwrap_or_default();
                let parts = self
                    .reasoning_parts
                    .checked_sub(previous.0)
                    .and_then(|count| count.checked_add(size.0))
                    .filter(|count| *count <= MAX_REASONING_PARTS)
                    .ok_or(ResponsesStreamError::Codec(
                        "Responses reasoning limit exceeded",
                    ))?;
                let bytes = self
                    .reasoning_bytes
                    .checked_sub(previous.1)
                    .and_then(|count| count.checked_add(size.1))
                    .filter(|count| *count <= MAX_REASONING_BYTES)
                    .ok_or(ResponsesStreamError::Codec(
                        "Responses reasoning limit exceeded",
                    ))?;
                self.items.insert(index, event["item"].clone());
                self.reasoning_parts = parts;
                self.reasoning_bytes = bytes;
            }
            "response.completed" => {
                let response = &event["response"];
                if response["status"] != "completed" {
                    return Err(ResponsesStreamError::Codec(
                        "Responses terminal status is not completed",
                    ));
                }
                self.accept_terminal_output(response)?;
                self.completed = true;
            }
            "response.incomplete" => {
                let response = &event["response"];
                let reason = response
                    .pointer("/incomplete_details/reason")
                    .and_then(Value::as_str);
                // Only a provider-declared max_output_tokens truncation is a
                // terminal budget hit: finish `length`, preserving terminal
                // output and usage. Every other or unknown incomplete reason
                // stays an error; it is never mislabeled length and never
                // silently served as success.
                if response["status"].as_str() == Some("incomplete")
                    && reason == Some("max_output_tokens")
                {
                    self.observe_truncated_output(response)?;
                    self.output.finish_reason = Some("length".into());
                    self.completed = true;
                } else {
                    let code = event
                        .pointer("/response/error/code")
                        .or_else(|| event.get("code"))
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .or_else(|| reason.map(str::to_owned));
                    return Err(ResponsesStreamError::Provider(code));
                }
            }
            // Terminal upstream failures keep the raw provider code verbatim
            // (structured field only, never prose). Callers own the mapping.
            "error" | "response.failed" => {
                let code = event
                    .pointer("/response/error/code")
                    .or_else(|| event.get("code"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                return Err(ResponsesStreamError::Provider(code));
            }
            _ => {}
        }
        Ok(None)
    }
    /// A typed budget terminal preserves observations, never execution or replay authority.
    fn observe_truncated_output(&mut self, response: &Value) -> Result<(), ResponsesStreamError> {
        let terminal = match response.get("output") {
            Some(Value::Array(items)) if !items.is_empty() => Some(items),
            None | Some(Value::Array(_)) => None,
            _ => {
                return Err(ResponsesStreamError::Codec(
                    "Invalid Responses terminal output",
                ));
            }
        };
        if terminal.is_none() && self.tracking_overflow {
            return Err(ResponsesStreamError::Codec(
                "Responses partial text tracking limit exceeded",
            ));
        }
        let mut text = String::new();
        let mut append = |part: &str| -> Result<(), ResponsesStreamError> {
            if text.len().saturating_add(part.len()) > 16 * 1024 * 1024 {
                return Err(ResponsesStreamError::Codec("Responses text limit exceeded"));
            }
            text.push_str(part);
            Ok(())
        };
        let mut parts = BTreeMap::new();
        let items: Vec<(u64, &Value)> = match terminal {
            Some(items) => items
                .iter()
                .enumerate()
                .map(|(i, item)| (i as u64, item))
                .collect(),
            None => self.items.iter().map(|(i, item)| (*i, item)).collect(),
        };
        for (index, item) in items {
            match item["type"].as_str() {
                Some("message") => {
                    if item["role"] != "assistant" {
                        return Err(ResponsesStreamError::Codec("Invalid partial message role"));
                    }
                    let content = item["content"]
                        .as_array()
                        .ok_or(ResponsesStreamError::Codec(
                            "Invalid partial message content",
                        ))?;
                    for (part, value) in content.iter().enumerate() {
                        let text = match value["type"].as_str() {
                            Some("output_text") => value["text"].as_str(),
                            Some("refusal") => value["refusal"].as_str(),
                            _ => None,
                        }
                        .ok_or(ResponsesStreamError::Codec("Invalid partial message part"))?;
                        if terminal.is_some() {
                            append(text)?;
                        } else if !text.is_empty() {
                            if parts.len() >= 4096 {
                                return Err(ResponsesStreamError::Codec(
                                    "Too many Responses text parts",
                                ));
                            }
                            parts.insert((index, part as u64), text);
                        }
                    }
                }
                Some("reasoning" | "function_call") => {}
                _ => {
                    return Err(ResponsesStreamError::Codec(
                        "Unsupported partial output item",
                    ));
                }
            }
        }
        if terminal.is_none() && (!parts.is_empty() || !self.text_ranges.is_empty()) {
            if self.unindexed_text {
                return Err(ResponsesStreamError::Codec(
                    "Ambiguous Responses partial text ownership",
                ));
            }
            let keys: std::collections::BTreeSet<_> = parts
                .keys()
                .chain(self.text_ranges.keys())
                .copied()
                .collect();
            for key in keys {
                if let Some(part) = parts.get(&key) {
                    append(part)?;
                } else if let Some(ranges) = self.text_ranges.get(&key) {
                    for range in ranges {
                        append(&self.output.text[range.clone()])?;
                    }
                }
            }
        }
        if !text.is_empty() {
            self.output.text = text;
        }
        self.output.items.clear();
        self.output.tool_calls.clear();
        Ok(())
    }

    /// Assemble replayable terminal output for a completed response.
    /// Subscription streams can leave terminal output empty after delivering
    /// complete output_item.done events. Only those finished items are
    /// eligible; text deltas alone cannot replay.
    fn accept_terminal_output(&mut self, response: &Value) -> Result<(), ResponsesStreamError> {
        let items = if let Some(items) = response["output"]
            .as_array()
            .filter(|items| !items.is_empty())
        {
            if items.len() > MAX_CONTINUATION_ITEMS {
                return Err(ResponsesStreamError::Codec(
                    "Too many Responses output items",
                ));
            }
            validate_reasoning_budget(items.iter()).map_err(ResponsesStreamError::Codec)?;
            items
                .iter()
                .map(|item| replay_item(item, false))
                .collect::<Result<Vec<_>, _>>()
        } else {
            validate_reasoning_budget(self.items.values()).map_err(ResponsesStreamError::Codec)?;
            self.items
                .values()
                .map(|item| replay_item(item, false))
                .collect::<Result<Vec<_>, _>>()
        };
        self.output.items = items.map_err(|_| ResponsesStreamError::Codec(INVALID_OUTPUT))?;
        if self.output.items.is_empty() {
            return Err(ResponsesStreamError::Codec(
                "Responses terminal output is missing",
            ));
        }
        let mut text = String::new();
        let mut call_ids = std::collections::HashSet::new();
        for item in &self.output.items {
            match item["type"].as_str() {
                Some("message") => {
                    if let Some(parts) = item["content"].as_array() {
                        for part in parts {
                            if let Some(value) =
                                part["text"].as_str().or_else(|| part["refusal"].as_str())
                            {
                                text.push_str(value);
                            }
                        }
                    }
                }
                Some("function_call") => {
                    let id = item["call_id"].as_str().filter(|s| !s.is_empty()).ok_or(
                        ResponsesStreamError::Codec("Responses tool call identity missing"),
                    )?;
                    if !call_ids.insert(id) {
                        return Err(ResponsesStreamError::Codec(
                            "Duplicate Responses tool call identity",
                        ));
                    }
                    let name = item["name"]
                        .as_str()
                        .ok_or(ResponsesStreamError::Codec("Responses tool name missing"))?;
                    let arguments =
                        item["arguments"]
                            .as_str()
                            .ok_or(ResponsesStreamError::Codec(
                                "Responses tool arguments missing",
                            ))?;
                    self.output.tool_calls.push(json!({"id":id,"type":"function","function":{"name":name,"arguments":arguments}}));
                }
                Some("reasoning") => {}
                _ => {
                    return Err(ResponsesStreamError::Codec(
                        "Unsupported Responses output item",
                    ));
                }
            }
        }
        if !call_ids.is_empty()
            && self
                .output
                .items
                .iter()
                .any(|item| !has_replayable_reasoning(item))
        {
            return Err(ResponsesStreamError::Codec(REASONING_CONTINUATION_MISSING));
        }
        // Text can still complete when a provider returned unusable
        // reasoning, but that private state cannot be replayed later.
        if call_ids.is_empty() && !self.output.items.iter().any(has_replayable_reasoning) {
            return Err(ResponsesStreamError::Codec(
                "Responses terminal output is missing",
            ));
        }
        if !text.is_empty() {
            self.output.text = text;
        }
        Ok(())
    }
    pub fn finish(self) -> Result<ResponseOutput, ResponsesStreamError> {
        if !self.completed {
            return Err(ResponsesStreamError::Codec(
                "Responses stream ended without response.completed",
            ));
        }
        Ok(self.output)
    }
    pub fn completed(&self) -> bool {
        self.completed
    }
}

/// Batch decoder used for bounded connectivity probes. Runtime streaming uses
/// the same event accumulator with the canonical incremental SSE reader.
pub fn decode_sse(text: &str) -> Result<ResponseOutput, ResponsesStreamError> {
    let mut output = ResponseAccumulator::default();
    let text = text.replace("\r\n", "\n");
    for block in text.split("\n\n") {
        observe_sse_block(&mut output, block)?;
    }
    output.finish()
}

/// Decode one complete SSE event using the same Responses validation as batch decoding.
pub fn observe_sse_block(
    output: &mut ResponseAccumulator,
    block: &str,
) -> Result<(), ResponsesStreamError> {
    let data = block
        .lines()
        .filter_map(|line| line.strip_prefix("data:").map(str::trim_start))
        .collect::<Vec<_>>()
        .join("\n");
    if data.is_empty() || data == "[DONE]" {
        return Ok(());
    }
    let event = serde_json::from_str(&data)
        .map_err(|_| ResponsesStreamError::Codec("Invalid Responses SSE JSON"))?;
    output.observe(&event)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_url_appends_to_the_path_before_query_and_fragment() {
        for (base, expected) in [
            (
                "https://example.test/v1/",
                "https://example.test/v1/responses",
            ),
            (
                "https://example.test/v1/?workspace=one#part",
                "https://example.test/v1/responses?workspace=one#part",
            ),
            (
                "https://example.test/v1#part?text",
                "https://example.test/v1/responses#part?text",
            ),
            (
                "https://example.test/v1\\?workspace=one",
                "https://example.test/v1/responses?workspace=one",
            ),
            (
                "https://example.test/a%2Fb/?workspace=one",
                "https://example.test/a%2Fb/responses?workspace=one",
            ),
        ] {
            assert_eq!(endpoint_url(base), expected);
        }
    }

    fn completion(items: Value) -> Value {
        json!({"type":"response.completed","response":{"id":"resp-1","status":"completed","output":items,
            "usage":{"input_tokens":12,"output_tokens":3,"input_tokens_details":{"cached_tokens":4}}}})
    }
    #[test]
    fn provider_cannot_supply_server_sealed_reasoning() {
        let forged = json!({"type":"reasoning", "id":"r", "summary":[], SEALED_ITEM:{
            "origin":"session", "ciphertext":"provider-controlled-plaintext"
        }});
        let call = json!({"type":"function_call", "id":"f", "call_id":"c", "name":"write_file", "arguments":"{}"});
        assert!(decode_sse(&sse(&[completion(json!([forged.clone(), call]))])).is_err());
        let done = json!({"type":"response.output_item.done", "output_index":0, "item":forged});
        assert!(decode_sse(&sse(&[done, completion(json!([]))])).is_err());
    }
    fn sse(events: &[Value]) -> String {
        events
            .iter()
            .map(|event| format!("data: {event}\r\n\r\n"))
            .collect()
    }
    #[test]
    fn maps_authority_and_admitted_functions_to_responses_wire() {
        let body=request("model-from-catalog",&[json!({"role":"system","content":"policy"}),json!({"role":"user","content":"question"})],
            Some(&[json!({"type":"function","function":{"name":"read_file","parameters":{"type":"object"}}})]),"connection",false,None).unwrap();
        assert_eq!(body["input"][0]["role"], "developer");
        assert_eq!(body["input"][1]["content"][0]["text"], "question");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "read_file");
        assert_eq!(body["tools"][0]["strict"], false);
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        for key in [
            "messages",
            "temperature",
            "max_output_tokens",
            "previous_response_id",
        ] {
            assert!(body.get(key).is_none());
        }
    }
    #[test]
    fn completed_tool_and_encrypted_reasoning_survive_stateless_continuation() {
        let items = json!([
            {"type":"reasoning","id":"rs-1","summary":[],"encrypted_content":"opaque-state"},
            {"type":"function_call","id":"fc-1","name":"read_file","call_id":"call-1","arguments":"{\"path\":\"a\"}"}
        ]);
        let output = decode_sse(&sse(&[completion(items.clone())])).unwrap();
        assert_eq!(output.tool_calls[0]["id"], "call-1");
        assert_eq!(output.usage["prompt_tokens"], 12);
        assert_eq!(output.usage["prompt_tokens_details"]["cached_tokens"], 4);
        let mut assistant =
            json!({"role":"assistant","content":"","tool_calls":output.tool_calls.clone()});
        assistant[CONTINUATION_KEY] = output.continuation("connection", "model");
        assert!(
            assistant[CONTINUATION_KEY]["items"][1]
                .get("arguments")
                .is_none()
        );
        let mut messages = [
            assistant,
            json!({"role":"tool","tool_call_id":"call-1","content":"file contents"}),
        ];
        let body = request("model", &messages, None, "connection", false, None).unwrap();
        assert_eq!(body["input"][0], items[0]);
        assert_eq!(body["input"][1], items[1]);
        assert_eq!(body["input"][2]["type"], "function_call_output");
        assert_eq!(body["input"][2]["call_id"], "call-1");
        assert!(
            request(
                "model",
                &messages,
                None,
                "different-connection",
                false,
                None
            )
            .is_err()
        );
        let mut completed = messages.to_vec();
        completed.push(json!({"role":"user", "content":"new turn"}));
        let switched = request(
            "model",
            &completed,
            None,
            "different-connection",
            false,
            None,
        )
        .unwrap();
        assert!(!switched.to_string().contains("opaque-state"));
        assert_eq!(switched["input"][0]["type"], "function_call");
        messages[0]["tool_calls"][0]["function"]["arguments"] =
            json!("canonical-normalized-arguments");
        let body = request("model", &messages, None, "connection", false, None).unwrap();
        assert_eq!(
            body["input"][1]["arguments"],
            "canonical-normalized-arguments"
        );
    }

    #[test]
    fn tool_calls_require_replayable_returned_reasoning() {
        let call =
            json!({"type":"function_call", "name":"write_file", "call_id":"c1", "arguments":"{}"});
        for encrypted in [None, Some(Value::Null), Some(json!(""))] {
            let mut reasoning = json!({"type":"reasoning", "summary":[]});
            if let Some(encrypted) = encrypted {
                reasoning["encrypted_content"] = encrypted;
            }
            let items = json!([
                {"type":"reasoning", "encrypted_content":"valid", "summary":[]},
                reasoning,
                call
            ]);
            assert!(matches!(
                decode_sse(&sse(&[completion(items)])),
                Err(ResponsesStreamError::Codec(REASONING_CONTINUATION_MISSING))
            ));
        }
        let mut namespaced = call.clone();
        namespaced["namespace"] = json!("other");
        assert!(decode_sse(&sse(&[completion(json!([namespaced]))])).is_err());
        let malformed = json!([
            {"type":"reasoning", "encrypted_content":123},
            {"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":"answer"}]}
        ]);
        assert!(decode_sse(&sse(&[completion(malformed)])).is_err());
        let output = decode_sse(&sse(&[completion(json!([call]))])).unwrap();
        assert_eq!(output.tool_calls[0]["id"], "c1");
    }

    #[test]
    fn text_without_replayable_reasoning_remains_usable_in_the_next_turn() {
        for reasoning in [
            json!({"type":"reasoning", "summary":[]}),
            json!({"type":"reasoning", "encrypted_content":null, "summary":[]}),
            json!({"type":"reasoning", "encrypted_content":"", "summary":[]}),
        ] {
            let output = decode_sse(&sse(&[completion(json!([
                reasoning,
                {"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":"answer"}]}
            ]))]))
            .unwrap();
            assert_eq!(output.text, "answer");
            assert!(output.items.iter().any(|item| item["type"] == "reasoning"));
            let mut assistant = json!({"role":"assistant", "content":output.text});
            assistant[CONTINUATION_KEY] = output.continuation("scope", "model");
            assert!(
                assistant[CONTINUATION_KEY]["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|item| item["type"] == "reasoning")
            );
            let request = request(
                "model",
                &[
                    json!({"role":"user", "content":"question"}),
                    assistant,
                    json!({"role":"user", "content":"continue"}),
                ],
                None,
                "scope",
                false,
                None,
            )
            .unwrap();
            assert!(request["input"].to_string().contains("answer"));
            assert!(
                request["input"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|item| item["type"] == "reasoning")
            );
        }
    }

    #[test]
    fn continuation_requires_bijective_calls_and_completed_results() {
        let mut assistant = json!({"role":"assistant","content":"", "tool_calls":[
            {"id":"c1","function":{"name":"read_file","arguments":"{}"}},
            {"id":"c2","function":{"name":"read_file","arguments":"{}"}}
        ]});
        assistant[CONTINUATION_KEY] = json!({"connection_id":"scope","model":"model","items":[
            {"type":"reasoning","encrypted_content":"opaque","summary":[]},
            {"type":"function_call","name":"read_file","call_id":"c1"},
            {"type":"function_call","name":"read_file","call_id":"c2"}
        ]});
        let first = json!({"role":"tool","tool_call_id":"c1","content":"one"});
        let second = json!({"role":"tool","tool_call_id":"c2","content":"two"});
        let valid = vec![assistant.clone(), first.clone(), second.clone()];
        assert!(request("model", &valid, None, "scope", false, None).is_ok());
        let mut missing_reasoning = valid.clone();
        missing_reasoning[0][CONTINUATION_KEY]["items"][0]
            .as_object_mut()
            .unwrap()
            .remove("encrypted_content");
        assert_eq!(
            request("model", &missing_reasoning, None, "scope", false, None).unwrap_err(),
            CONTINUATION_INCOMPLETE
        );
        let partial = vec![assistant.clone(), first.clone()];
        assert_eq!(
            request("model", &partial, None, "scope", false, None).unwrap_err(),
            CONTINUATION_INCOMPLETE
        );
        let mut corrupt = valid.clone();
        corrupt[0][CONTINUATION_KEY]["items"]
            .as_array_mut()
            .unwrap()
            .pop();
        assert_eq!(
            request("model", &corrupt, None, "scope", false, None).unwrap_err(),
            CONTINUATION_INCOMPLETE
        );
        corrupt[0][CONTINUATION_KEY]["items"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type":"function_call","name":"read_file","call_id":"c1"}));
        assert_eq!(
            request("model", &corrupt, None, "scope", false, None).unwrap_err(),
            CONTINUATION_INCOMPLETE
        );
        let mut wrong_namespace = valid.clone();
        wrong_namespace[0][CONTINUATION_KEY]["items"][1]["namespace"] = json!("unadmitted");
        assert_eq!(
            request("model", &wrong_namespace, None, "scope", false, None).unwrap_err(),
            CONTINUATION_INCOMPLETE
        );
        let mut empty = valid.clone();
        empty[0][CONTINUATION_KEY]["items"] = json!([]);
        assert_eq!(
            request("model", &empty, None, "scope", false, None).unwrap_err(),
            CONTINUATION_INCOMPLETE
        );
        let mut recovered = partial;
        // Canonical projection kept only completed pairs. A later human turn
        // must not silently lose reasoning a route might still require.
        recovered[0]["tool_calls"].as_array_mut().unwrap().pop();
        recovered.push(json!({"role":"user","content":"continue"}));
        assert_eq!(
            request("model", &recovered, None, "scope", false, None).unwrap_err(),
            CONTINUATION_HISTORY_INCOMPLETE
        );
        // Other routes have no claim to this state. Current-turn-only routes
        // remove old reasoning before projection and can recover as before.
        assert!(request("model", &recovered, None, "other", false, None).is_ok());
        recovered[0][CONTINUATION_KEY]["items"]
            .as_array_mut()
            .unwrap()
            .retain(|item| item["type"] != "reasoning");
        let body = request("model", &recovered, None, "scope", false, None).unwrap();
        assert!(!body.to_string().contains("opaque"));
    }

    #[test]
    fn active_continuation_maps_runtime_ids_and_blocks_a_changed_route() {
        let assistant = json!({"role":"assistant", "tool_calls":[
            {"id":"internal","provider_id":"original","function":{"name":"lookup","arguments":"{}"}}
        ], CONTINUATION_KEY:{"connection_id":"old","model":"model","items":[
            {"type":"function_call","call_id":"original","name":"lookup"}
        ]}});
        let result = json!({"role":"tool","tool_call_id":"internal","content":"found"});
        let messages = vec![assistant, result];
        fn provider_id(call: &Value) -> Option<&str> {
            call["provider_id"].as_str()
        }
        assert!(validate_active_continuations(&messages, "model", "old", provider_id).is_ok());
        assert_eq!(
            validate_active_continuations(&messages, "model", "new", provider_id).unwrap_err(),
            CONTINUATION_SCOPE_CHANGED
        );
        assert_eq!(
            validate_active_continuations(&messages[..1], "model", "old", provider_id).unwrap_err(),
            CONTINUATION_INCOMPLETE
        );
    }

    #[test]
    fn active_tool_calls_require_provider_continuation() {
        let assistant = json!({"role":"assistant", "tool_calls":[
            {"id":"c1", "function":{"name":"lookup", "arguments":"{}"}}
        ]});
        let result = json!({"role":"tool", "tool_call_id":"c1", "content":"found"});
        for saved in [
            None,
            Some(Value::Null),
            Some(json!({})),
            Some(json!({"invalid":true})),
        ] {
            let mut call = assistant.clone();
            if let Some(saved) = saved {
                call[CONTINUATION_KEY] = saved;
            }
            for messages in [vec![call.clone()], vec![call, result.clone()]] {
                assert_eq!(
                    validate_active_continuations(&messages, "model", "scope", |_| None)
                        .unwrap_err(),
                    CONTINUATION_INCOMPLETE
                );
            }
        }
        // A tool's payload is data, not a top-level assistant tool call.
        let message = json!({"role":"tool", "content":{"role":"assistant", "tool_calls":[{}]}});
        assert!(validate_active_continuations(&[message], "model", "scope", |_| None).is_ok());
        assert!(
            validate_active_continuations(
                &[json!({"role":"assistant", "content":"done"})],
                "model",
                "scope",
                |_| None
            )
            .is_ok()
        );
    }

    #[test]
    fn changed_scope_uses_the_later_user_boundary_and_preserves_error_order() {
        let assistant = json!({
            "role":"assistant", "content":"", "tool_calls":[{
                "id":"call-1", "function":{"name":"lookup","arguments":"{}"}
            }],
            CONTINUATION_KEY:{"connection_id":"old", "model":"model", "items":[]}
        });
        let user = json!({"role":"user", "content":"next turn"});
        for (model, connection) in [("model", "new"), ("other-model", "old")] {
            for messages in [
                vec![assistant.clone()],
                vec![user.clone(), assistant.clone()],
            ] {
                assert_eq!(
                    request(model, &messages, None, connection, false, None).unwrap_err(),
                    CONTINUATION_SCOPE_CHANGED
                );
            }
            let messages = vec![assistant.clone(), user.clone()];
            assert!(request(model, &messages, None, connection, false, None).is_ok());
        }

        for calls in [None, Some(Value::Null), Some(json!([]))] {
            let mut no_calls = assistant.clone();
            if let Some(calls) = calls {
                no_calls["tool_calls"] = calls;
            } else {
                no_calls.as_object_mut().unwrap().remove("tool_calls");
            }
            assert!(request("model", &[no_calls], None, "new", false, None).is_ok());
        }
        let tool_user = json!({"role":"user", "tool_call_id":"call-1", "content":"result"});
        assert!(
            request(
                "model",
                &[assistant.clone(), tool_user],
                None,
                "new",
                false,
                None
            )
            .is_ok()
        );

        for (messages, expected) in [
            (
                vec![assistant.clone(), json!({"role":42})],
                CONTINUATION_SCOPE_CHANGED,
            ),
            (
                vec![json!({"role":42}), assistant.clone()],
                "Message role is missing",
            ),
            (
                vec![assistant.clone(), json!({"role":42}), user],
                "Message role is missing",
            ),
            (
                vec![assistant, json!({"role":"user", "content":{}})],
                "Unsupported message content",
            ),
        ] {
            assert_eq!(
                request("model", &messages, None, "new", false, None).unwrap_err(),
                expected
            );
        }
        assert!(request("model", &[], None, "new", false, None).is_ok());
    }

    #[test]
    fn changed_scope_after_long_history_preserves_tool_pairs() {
        let old_scope = format!("sha256:{}", "a".repeat(64));
        let new_scope = format!("sha256:{}", "b".repeat(64));
        let mut messages = vec![json!({"role":"user", "content":"old turn"})];
        for index in 0..1000 {
            let id = format!("call-{index}");
            let output = ResponseOutput {
                items: vec![json!({
                    "type":"function_call", "name":"lookup",
                    "call_id":id, "arguments":"{}"
                })],
                ..Default::default()
            };
            let mut assistant = json!({
                "role":"assistant", "content":"", "tool_calls":[{
                    "id":id, "function":{"name":"lookup","arguments":"{}"}
                }]
            });
            assistant[CONTINUATION_KEY] = output.continuation(&old_scope, "model");
            assert!(validated_continuation(&assistant).is_some());
            messages.push(assistant);
            messages.push(json!({"role":"tool", "tool_call_id":id, "content":"ok"}));
        }
        messages.push(json!({"role":"user", "content":"new turn"}));
        let body = request("model", &messages, None, &new_scope, false, None).unwrap();
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 2002);
        for (index, pair) in input[1..2001].chunks_exact(2).enumerate() {
            let id = format!("call-{index}");
            assert_eq!(pair[0]["type"], "function_call");
            assert_eq!(pair[0]["call_id"], id);
            assert_eq!(pair[1]["type"], "function_call_output");
            assert_eq!(pair[1]["call_id"], id);
        }
        assert_eq!(input[2001]["role"], "user");
        messages.push(messages[1].clone());
        assert_eq!(
            request("model", &messages, None, &new_scope, false, None).unwrap_err(),
            CONTINUATION_SCOPE_CHANGED
        );
    }
    #[test]
    fn presentation_deltas_are_not_duplicated_by_final_items() {
        let value=decode_sse(&sse(&[
            json!({"type":"response.output_text.delta","delta":"Hello"}),
            completion(json!([{"type":"message","role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":"Hello"}]}])),
        ])).unwrap();
        assert_eq!(value.text, "Hello");
        assert_eq!(value.items[0]["phase"], "final_answer");
    }
    #[test]
    fn completed_stream_uses_finished_items_when_terminal_output_is_empty() {
        let item = json!({"type":"message","id":"message-1","role":"assistant","status":"completed","content":[{"type":"output_text","text":"OK"}]});
        let done = json!({"type":"response.output_item.done","output_index":0,"item":item});
        let output = decode_sse(&sse(&[done.clone(), completion(json!([]))])).unwrap();
        assert_eq!(output.text, "OK");
        assert_eq!(output.items, vec![item]);
        assert_eq!(output.usage["prompt_tokens"], 12);
        // Finished items alone, or text deltas plus an empty completion, do
        // not establish a completed, replayable response.
        assert!(decode_sse(&sse(&[done])).is_err());
        assert!(
            decode_sse(&sse(&[
                json!({"type":"response.output_text.delta","delta":"OK"}),
                completion(json!([])),
            ]))
            .is_err()
        );
    }
    #[test]
    fn incomplete_and_failed_streams_never_become_success() {
        assert!(
            decode_sse(&sse(&[
                json!({"type":"response.output_text.delta","delta":"partial"})
            ]))
            .is_err()
        );
        assert!(decode_sse("data: [DONE]\n\n").is_err());
        assert!(
            decode_sse(&sse(&[
                json!({"type":"response.output_text.delta","delta":"partial"}),
                completion(json!([]))
            ]))
            .is_err()
        );
        for kind in ["response.failed", "response.incomplete"] {
            let error = (kind == "response.failed").then(|| json!({"code":"subscription_sharing_usage_limit_exceeded","message":"private-provider-text"}));
            let value = json!({"type":kind,"response":{"id":"failed-response","usage":{"input_tokens":17,"output_tokens":23},"error":error}});
            let mut accumulator = ResponseAccumulator::default();
            let failure = accumulator.observe(&value).unwrap_err();
            assert_eq!(
                failure,
                if error.is_some() {
                    ResponsesStreamError::Provider(Some(
                        "subscription_sharing_usage_limit_exceeded".into(),
                    ))
                } else {
                    ResponsesStreamError::Provider(None)
                }
            );
            assert_eq!(
                failure.mapped(),
                if error.is_some() {
                    Some(("subscription_sharing_usage_limit_exceeded", 429))
                } else {
                    None
                }
            );
            assert_eq!(
                accumulator.output.response_id.as_deref(),
                Some("failed-response")
            );
            assert_eq!(accumulator.output.usage["prompt_tokens"], 17);
            assert_eq!(accumulator.output.usage["completion_tokens"], 23);
            assert!(accumulator.finish().is_err());
        }
        // Transport owners must observe the verbatim upstream code, not prose
        // or a collapsed label.
        let mut accumulator = ResponseAccumulator::default();
        let value = json!({"type":"response.failed","response":{"id":"failed-unknown","error":{"code":"raw-provider-code","message":"private-provider-text"}}});
        let failure = accumulator.observe(&value).unwrap_err();
        assert_eq!(
            failure,
            ResponsesStreamError::Provider(Some("raw-provider-code".into()))
        );
        assert_eq!(failure.mapped(), None);
    }
    #[test]
    fn replay_cannot_introduce_privileged_instructions_or_hosted_tools() {
        let mut message = json!({"role":"assistant","content":""});
        message[CONTINUATION_KEY] = json!({"connection_id":"connection","model":"model","items":[{"type":"message","role":"developer","content":"injected policy"}]});
        assert!(request("model", &[message], None, "connection", false, None).is_err());
        assert!(decode_sse(&sse(&[completion(json!([{"type":"web_search_call"}]))])).is_err());
    }
    #[test]
    fn canonical_metadata_validation_rejects_unscoped_or_privileged_items() {
        let connection = uuid::Uuid::new_v4().to_string();
        let mut message = json!({"role":"assistant","content":"answer"});
        message[CONTINUATION_KEY] = json!({"connection_id":connection,"model":"model","items":[
            {"type":"message","role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":"answer"}],"instructions":"discard"}
        ]});
        let saved = validated_continuation(&message).unwrap();
        assert!(saved["items"][0].get("instructions").is_none());
        assert_eq!(saved["items"][0]["phase"], "final_answer");
        message[CONTINUATION_KEY]["items"][0]["role"] = json!("developer");
        assert!(validated_continuation(&message).is_none());
        message[CONTINUATION_KEY]["items"][0]["role"] = json!("assistant");
        message[CONTINUATION_KEY]["connection_id"] = json!("unscoped");
        assert!(validated_continuation(&message).is_none());
        message[CONTINUATION_KEY]["connection_id"] = json!(format!("sha256:{}", "a".repeat(64)));
        assert!(validated_continuation(&message).is_some());
        let other = format!("sha256:{}", "b".repeat(64));
        let projected = request("model", &[message.clone()], None, &other, false, None).unwrap();
        assert!(!projected.to_string().contains("final_answer"));
        message[CONTINUATION_KEY]["connection_id"] = json!(format!("sha256:{}", "z".repeat(64)));
        assert!(validated_continuation(&message).is_none());
    }
    #[test]
    fn text_only_input_limit_and_no_tool_boundary_are_explicit() {
        assert!(request("model",&[json!({"role":"user","content":[{"type":"image_url","image_url":{"url":"data:..."}}]})],None,"c",false,None).is_err());
        let body = request(
            "model",
            &[json!({"role":"user","content":"hello"})],
            None,
            "c",
            true,
            None,
        )
        .unwrap();
        assert_eq!(body["tool_choice"], "none");
    }
    #[test]
    fn request_carries_max_output_tokens_only_when_budgeted() {
        let unbudgeted = request(
            "model",
            &[json!({"role":"user","content":"hello"})],
            None,
            "c",
            false,
            None,
        )
        .unwrap();
        assert!(unbudgeted.get("max_output_tokens").is_none());
        let zero = request(
            "model",
            &[json!({"role":"user","content":"hello"})],
            None,
            "c",
            false,
            Some(0),
        )
        .unwrap();
        assert!(zero.get("max_output_tokens").is_none());
        let budgeted = request(
            "model",
            &[json!({"role":"user","content":"hello"})],
            None,
            "c",
            false,
            Some(512),
        )
        .unwrap();
        assert_eq!(budgeted["max_output_tokens"], 512);
    }
    #[test]
    fn incomplete_max_output_tokens_finishes_as_length_preserving_output_and_usage() {
        let item = json!({"type":"message","id":"message-1","role":"assistant","status":"completed","content":[{"type":"output_text","text":"partial answer"}]});
        let done = json!({"type":"response.output_item.done","output_index":0,"item":item});
        let incomplete = json!({"type":"response.incomplete","response":{"id":"resp-9","status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[],"usage":{"input_tokens":10,"output_tokens":512}}});
        let output = decode_sse(&sse(&[done.clone(), incomplete.clone()])).unwrap();
        assert_eq!(output.text, "partial answer");
        assert!(output.items.is_empty());
        assert!(output.tool_calls.is_empty());
        assert_eq!(output.finish_reason.as_deref(), Some("length"));
        assert_eq!(output.response_id.as_deref(), Some("resp-9"));
        assert_eq!(output.usage["output_tokens"], 512);
        assert_eq!(output.usage["completion_tokens"], 512);
        // A truncation completing the accumulator ends the stream like a
        // completion: later events are ignored, and a duplicate completed
        // stays a protocol error.
        let mut accumulator = ResponseAccumulator::default();
        accumulator.observe(&done).unwrap();
        accumulator.observe(&incomplete).unwrap();
        assert!(accumulator.completed());
        accumulator
            .observe(&json!({"type":"response.output_text.delta","delta":"late"}))
            .unwrap();
        assert!(accumulator.finish().is_ok());
    }
    #[test]
    fn incomplete_other_or_unknown_reasons_stay_errors() {
        for details in [
            json!({"reason":"content_filter"}),
            json!({"reason":"unknown_future_reason"}),
            json!(null),
        ] {
            let event = json!({"type":"response.incomplete","response":{"id":"resp-x","status":"incomplete","incomplete_details":details,"usage":{"input_tokens":1,"output_tokens":2}}});
            assert!(decode_sse(&sse(&[event])).is_err(), "{details}");
        }
        // A max_output_tokens claim without the incomplete status is not
        // truncation evidence.
        let forged = json!({"type":"response.incomplete","response":{"id":"resp-y","status":"failed","incomplete_details":{"reason":"max_output_tokens"},"usage":{"input_tokens":1,"output_tokens":2}}});
        assert!(decode_sse(&sse(&[forged])).is_err());
        // Unknown reasons preserve structured evidence, never become length.
        let mut accumulator = ResponseAccumulator::default();
        let failure = accumulator
            .observe(&json!({"type":"response.incomplete","response":{"id":"resp-z","status":"incomplete","incomplete_details":{"reason":"content_filter"}}}))
            .unwrap_err();
        assert_eq!(
            failure,
            ResponsesStreamError::Provider(Some("content_filter".into()))
        );
        assert!(accumulator.finish().is_err());
    }
}
