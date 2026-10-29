//! The Responses wire codec shared by subscription probes and runtime calls.
//! It owns no conversation, tool execution, credential, or retry lifecycle.
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

pub const CONTINUATION_KEY: &str = "_astra_responses";
const ENCODED_REASONING: &str = "_astra_encrypted_content_b64";

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

fn replay_item(item: &Value) -> Result<Value, String> {
    let keys: &[&str] = match item["type"].as_str() {
        Some("message") => {
            if item.get("role").is_some_and(|role| role != "assistant") {
                return Err("Provider continuation cannot introduce instructions".into());
            }
            &["id", "type", "role", "status", "phase", "content"]
        }
        Some("reasoning") => &["id", "type", "status", "summary", "encrypted_content"],
        Some("function_call") => &[
            "id",
            "type",
            "status",
            "namespace",
            "name",
            "arguments",
            "call_id",
        ],
        _ => return Err("Unsupported provider continuation item".into()),
    };
    let mut object = Map::new();
    for key in keys {
        if let Some(value) = item.get(*key) {
            object.insert((*key).into(), value.clone());
        }
    }
    if item["type"] == "reasoning"
        && object.get("encrypted_content").is_none()
        && let Some(encoded) = item.get(ENCODED_REASONING).and_then(Value::as_str)
    {
        let bytes = STANDARD
            .decode(encoded)
            .map_err(|_| "Invalid encrypted reasoning encoding")?;
        let value = String::from_utf8(bytes).map_err(|_| "Invalid encrypted reasoning encoding")?;
        object.insert("encrypted_content".into(), json!(value));
    }
    Ok(Value::Object(object))
}

pub fn request(
    model: &str,
    messages: &[Value],
    tools: Option<&[Value]>,
    connection: &str,
    no_tools: bool,
) -> Result<Value, String> {
    let mut input = Vec::new();
    for message in messages {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .ok_or("Message role is missing")?;
        if role == "assistant"
            && let Some(saved) = message.get(CONTINUATION_KEY)
            && saved["connection_id"] == connection
            && saved["model"] == model
        {
            let items = saved["items"]
                .as_array()
                .ok_or("Invalid Responses continuation")?;
            for item in items {
                let mut replay = replay_item(item)?;
                if replay["type"] == "function_call" {
                    let call = message
                        .get("tool_calls")
                        .and_then(Value::as_array)
                        .and_then(|calls| calls.iter().find(|call| call["id"] == replay["call_id"]))
                        .ok_or("Continuation tool call has no canonical assistant call")?;
                    replay["arguments"] = json!(
                        call["function"]["arguments"]
                            .as_str()
                            .ok_or("Invalid canonical tool arguments")?
                    );
                }
                input.push(replay);
            }
            continue;
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
            input.push(json!({"type":"function_call_output","call_id":id,"output":output}));
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
            input.push(item);
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
                input.push(json!({"type":"function_call","call_id":id,"name":name,"namespace":"astra","arguments":arguments}));
            }
        }
    }
    let mut body = json!({"model":model,"input":input,"store":false,"stream":true});
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
        body["tools"] = json!([{"type":"namespace","name":"astra","description":"Tools admitted by Astra","tools":definitions}]);
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
}
impl ResponseOutput {
    pub fn continuation(&self, connection: &str, model: &str) -> Value {
        let mut items = self.items.clone();
        for item in &mut items {
            if item["type"] == "reasoning" {
                // This is already provider-encrypted, opaque state. Encoding
                // prevents credential-shaped ciphertext from being corrupted
                // by the canonical history's generic text redaction.
                if let Some(value) = item
                    .as_object_mut()
                    .and_then(|object| object.remove("encrypted_content"))
                    && let Some(value) = value.as_str()
                {
                    item[ENCODED_REASONING] = json!(STANDARD.encode(value));
                }
            } else if item["type"] == "function_call" {
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
    completed: bool,
}
impl ResponseAccumulator {
    /// Returns a text/reasoning delta for the existing presentation owner.
    pub fn observe(&mut self, event: &Value) -> Result<Option<(String, bool)>, String> {
        let kind = event["type"]
            .as_str()
            .ok_or("Responses event type is missing")?;
        if self.completed {
            if kind == "response.completed" {
                return Err("Duplicate Responses completion".into());
            }
            return Ok(None);
        }
        match kind {
            "response.created" => {
                self.output.response_id = event["response"]["id"].as_str().map(str::to_owned);
            }
            "response.output_text.delta" => {
                let delta = event["delta"]
                    .as_str()
                    .ok_or("Invalid Responses text delta")?;
                self.output.text.push_str(delta);
                return Ok(Some((delta.into(), false)));
            }
            "response.reasoning_summary_text.delta" => {
                let delta = event["delta"]
                    .as_str()
                    .ok_or("Invalid Responses reasoning summary")?;
                self.output.reasoning.push_str(delta);
                return Ok(Some((delta.into(), true)));
            }
            "response.output_item.done" => {
                let index = event["output_index"]
                    .as_u64()
                    .ok_or("Responses item index missing")?;
                if self.items.len() >= 4096 && !self.items.contains_key(&index) {
                    return Err("Too many Responses output items".into());
                }
                self.items.insert(index, event["item"].clone());
            }
            "response.completed" => {
                let response = &event["response"];
                if response["status"] != "completed" {
                    return Err("Responses terminal status is not completed".into());
                }
                let items = if let Some(items) = response["output"].as_array() {
                    items.clone()
                } else {
                    self.items.values().cloned().collect()
                };
                self.output.items = items.iter().map(replay_item).collect::<Result<_, _>>()?;
                if self.output.items.is_empty() {
                    return Err("Responses terminal output is missing".into());
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
                            let id = item["call_id"]
                                .as_str()
                                .filter(|s| !s.is_empty())
                                .ok_or("Responses tool call identity missing")?;
                            if !call_ids.insert(id) {
                                return Err("Duplicate Responses tool call identity".into());
                            }
                            if item
                                .get("namespace")
                                .and_then(Value::as_str)
                                .is_some_and(|n| n != "astra")
                            {
                                return Err("Responses tool namespace was not admitted".into());
                            }
                            let name =
                                item["name"].as_str().ok_or("Responses tool name missing")?;
                            let arguments = item["arguments"]
                                .as_str()
                                .ok_or("Responses tool arguments missing")?;
                            self.output.tool_calls.push(json!({"id":id,"type":"function","function":{"name":name,"arguments":arguments}}));
                        }
                        Some("reasoning") => {}
                        _ => return Err("Unsupported Responses output item".into()),
                    }
                }
                if !text.is_empty() {
                    self.output.text = text;
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
                self.output.response_id = response["id"]
                    .as_str()
                    .map(str::to_owned)
                    .or(self.output.response_id.take());
                self.completed = true;
            }
            "error" | "response.failed" | "response.incomplete" => {
                let code = event
                    .pointer("/response/error/code")
                    .or_else(|| event.get("code"))
                    .and_then(Value::as_str)
                    .unwrap_or("response_incomplete");
                let code = provider_error(code)
                    .map(|value| value.0)
                    .unwrap_or("response_incomplete");
                return Err(code.into());
            }
            _ => {}
        }
        Ok(None)
    }
    pub fn finish(self) -> Result<ResponseOutput, String> {
        if !self.completed {
            return Err("Responses stream ended without response.completed".into());
        }
        Ok(self.output)
    }
    pub fn completed(&self) -> bool {
        self.completed
    }
}

/// Batch decoder used for bounded connectivity probes. Runtime streaming uses
/// the same event accumulator with the canonical incremental SSE reader.
pub fn decode_sse(text: &str) -> Result<ResponseOutput, String> {
    let mut output = ResponseAccumulator::default();
    let text = text.replace("\r\n", "\n");
    for block in text.split("\n\n") {
        let data = block
            .lines()
            .filter_map(|line| line.strip_prefix("data:").map(str::trim_start))
            .collect::<Vec<_>>()
            .join("\n");
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        output.observe(&serde_json::from_str(&data).map_err(|_| "Invalid Responses SSE JSON")?)?;
    }
    output.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn completion(items: Value) -> Value {
        json!({"type":"response.completed","response":{"id":"resp-1","status":"completed","output":items,
            "usage":{"input_tokens":12,"output_tokens":3,"input_tokens_details":{"cached_tokens":4}}}})
    }
    fn sse(events: &[Value]) -> String {
        events
            .iter()
            .map(|event| format!("data: {event}\r\n\r\n"))
            .collect()
    }
    #[test]
    fn maps_authority_and_admitted_functions_to_subscription_wire() {
        let body=request("model-from-catalog",&[json!({"role":"system","content":"policy"}),json!({"role":"user","content":"question"})],
            Some(&[json!({"type":"function","function":{"name":"read_file","parameters":{"type":"object"}}})]),"connection",false).unwrap();
        assert_eq!(body["input"][0]["role"], "developer");
        assert_eq!(body["input"][1]["content"][0]["text"], "question");
        assert_eq!(body["tools"][0]["type"], "namespace");
        assert_eq!(body["tools"][0]["tools"][0]["name"], "read_file");
        assert_eq!(body["tools"][0]["tools"][0]["strict"], false);
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
            {"type":"function_call","id":"fc-1","namespace":"astra","name":"read_file","call_id":"call-1","arguments":"{\"path\":\"a\"}"}
        ]);
        let output = decode_sse(&sse(&[completion(items.clone())])).unwrap();
        assert_eq!(output.tool_calls[0]["id"], "call-1");
        assert_eq!(output.usage["prompt_tokens"], 12);
        assert_eq!(output.usage["prompt_tokens_details"]["cached_tokens"], 4);
        let mut assistant =
            json!({"role":"assistant","content":"","tool_calls":output.tool_calls.clone()});
        assistant[CONTINUATION_KEY] = output.continuation("connection", "model");
        let messages = [
            assistant,
            json!({"role":"tool","tool_call_id":"call-1","content":"file contents"}),
        ];
        let body = request("model", &messages, None, "connection", false).unwrap();
        assert_eq!(body["input"][0], items[0]);
        assert_eq!(body["input"][1], items[1]);
        assert_eq!(body["input"][2]["type"], "function_call_output");
        assert_eq!(body["input"][2]["call_id"], "call-1");
        let switched = request("model", &messages, None, "different-connection", false).unwrap();
        assert!(!switched.to_string().contains("opaque-state"));
        assert_eq!(switched["input"][0]["type"], "function_call");
    }
    #[test]
    fn continuation_reuses_canonical_arguments_without_storing_a_raw_copy() {
        let output=decode_sse(&sse(&[completion(json!([{"type":"function_call","namespace":"astra","name":"read_file","call_id":"call-1","arguments":"raw-provider-arguments"}]))])).unwrap();
        let saved = output.continuation("connection", "model");
        assert!(!saved.to_string().contains("raw-provider-arguments"));
        let message = json!({"role":"assistant","content":"","tool_calls":[{"id":"call-1","function":{"name":"read_file","arguments":"canonical-normalized-arguments"}}],CONTINUATION_KEY:saved});
        let body = request("model", &[message], None, "connection", false).unwrap();
        assert_eq!(
            body["input"][0]["arguments"],
            "canonical-normalized-arguments"
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
        let value = json!({"type":"response.failed","response":{"error":{"code":"subscription_sharing_usage_limit_exceeded","message":"do-not-forward-private-provider-text"}}});
        let error = decode_sse(&sse(&[value])).unwrap_err();
        assert_eq!(error, "subscription_sharing_usage_limit_exceeded");
        assert!(!error.contains("private-provider-text"));
    }
    #[test]
    fn replay_cannot_introduce_privileged_instructions_or_hosted_tools() {
        let mut message = json!({"role":"assistant","content":""});
        message[CONTINUATION_KEY] = json!({"connection_id":"connection","model":"model","items":[{"type":"message","role":"developer","content":"injected policy"}]});
        assert!(request("model", &[message], None, "connection", false).is_err());
        assert!(decode_sse(&sse(&[completion(json!([{"type":"web_search_call"}]))])).is_err());
    }
    #[test]
    fn text_only_input_limit_and_no_tool_boundary_are_explicit() {
        assert!(request("model",&[json!({"role":"user","content":[{"type":"image_url","image_url":{"url":"data:..."}}]})],None,"c",false).is_err());
        let body = request(
            "model",
            &[json!({"role":"user","content":"hello"})],
            None,
            "c",
            true,
        )
        .unwrap();
        assert_eq!(body["tool_choice"], "none");
    }
}
