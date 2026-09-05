//! Borrowed native OpenAI response projection, shared by streaming and complete
//! response consumers. Tool authorization, degraded markup recovery, usage
//! accounting, and continuation remain in the canonical higher-level owner.

use serde_json::{Map, Value};

pub struct OpenAiPayload<'a> {
    pub response_id: Option<&'a str>,
    pub usage: Option<&'a Map<String, Value>>,
    pub finish_reason: Option<&'a str>,
    pub message_present: bool,
    pub text: Option<&'a str>,
    pub reasoning: Option<&'a str>,
    pub tool_calls: Option<&'a [Value]>,
}

impl std::fmt::Debug for OpenAiPayload<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiPayload")
            .field("message_present", &self.message_present)
            .field("terminal_present", &self.finish_reason.is_some())
            .finish()
    }
}

impl<'a> OpenAiPayload<'a> {
    pub fn stream(value: &'a Value) -> Self {
        Self::project(value, "delta")
    }

    pub fn response(value: &'a Value) -> Self {
        Self::project(value, "message")
    }

    fn project(value: &'a Value, message_field: &str) -> Self {
        let choice = value
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first());
        let message = choice
            .and_then(|choice| choice.get(message_field))
            .and_then(Value::as_object);
        Self {
            response_id: value.get("id").and_then(Value::as_str),
            usage: value.get("usage").and_then(Value::as_object),
            finish_reason: choice
                .and_then(|choice| choice.get("finish_reason"))
                .and_then(Value::as_str),
            message_present: message.is_some(),
            text: message
                .and_then(|message| message.get("content"))
                .and_then(Value::as_str),
            reasoning: message
                .and_then(|message| message.get("reasoning_content"))
                .and_then(Value::as_str),
            tool_calls: message
                .and_then(|message| message.get("tool_calls"))
                .and_then(Value::as_array)
                .map(Vec::as_slice),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn streaming_and_complete_projection_share_fields_without_copying_payload() {
        for field in ["delta", "message"] {
            let value = json!({"id":"response-canary", "usage":{"prompt_tokens":7}, "choices":[{"finish_reason":"tool_calls", field:{"content":"text-canary", "reasoning_content":"reasoning-canary", "tool_calls":[{"index":0,"function":{"name":"read_file","arguments":"{\"path\":\"你好"}}]}}]});
            let payload = if field == "delta" {
                OpenAiPayload::stream(&value)
            } else {
                OpenAiPayload::response(&value)
            };
            assert_eq!(payload.text, Some("text-canary"));
            assert_eq!(payload.reasoning, Some("reasoning-canary"));
            assert_eq!(payload.finish_reason, Some("tool_calls"));
            assert_eq!(payload.usage.unwrap()["prompt_tokens"], 7);
            assert_eq!(
                payload.tool_calls.unwrap().as_ptr(),
                value["choices"][0][field]["tool_calls"]
                    .as_array()
                    .unwrap()
                    .as_ptr()
            );
            assert!(!format!("{payload:?}").contains("canary"));
        }
    }

    #[test]
    fn missing_or_malformed_fields_never_synthesize_progress_or_completion() {
        for value in [
            json!({}),
            json!({"choices":{}}),
            json!({"choices":[{"delta":false,"finish_reason":1}]}),
        ] {
            let payload = OpenAiPayload::stream(&value);
            assert!(!payload.message_present);
            assert!(payload.finish_reason.is_none());
            assert!(payload.text.is_none());
            assert!(payload.tool_calls.is_none());
        }
        let value = json!({"usage":{"completion_tokens":2},"choices":[]});
        let payload = OpenAiPayload::stream(&value);
        assert!(payload.usage.is_some());
        assert!(!payload.message_present);
        assert!(payload.finish_reason.is_none());
    }
}
