//! Context compression shared types.
//!
//! Pure types and traits extracted from `context_compression` so that
//! downstream modules like `compaction_replay` can depend on them
//! without pulling in the full runtime.

use crate::context_assembly_trace::CompressionMethod;
use serde_json::{Map, Value};
use std::collections::BTreeMap;

// ───────────────────────────── Message types ───────────────────────────────

/// Typed message replacing untyped `serde_json::Value` in the compaction pipeline.
///
/// Captures all fields accessed by compression layers and preserves unknown
/// fields in `extra` for lossless round-tripping.  `content` is stored as a
/// plain string regardless of original shape; `content_is_tool_result`
/// records whether the original was an Anthropic-style tool_result array.
#[derive(Debug, Clone)]
pub struct Message {
    pub role: String,
    pub content: Option<String>,
    /// Was the original content a tool_result array (Anthropic format)?
    pub content_is_tool_result: bool,
    /// Was the original content an array of any kind (tool_result OR mixed
    /// text/image blocks)? Distinct from `content_is_tool_result` because
    /// non-tool_result arrays still need to round-trip as arrays.
    pub content_was_array: bool,
    /// Set whenever a layer calls [`Message::set_content`]. When true, the
    /// stored string is the new authoritative representation and we do NOT
    /// re-type it back to an array on `Value` round-trip — re-typing a
    /// truncation/stub string into an array would corrupt the wire shape.
    pub content_modified: bool,
    /// Set when a compaction layer replaces the message content with a stub
    /// or truncation.  Synthetic messages are excluded from
    /// [`is_plain_user_task`] so that compaction does not count stubs
    /// against the protected-head budget.
    pub is_synthetic: bool,
    pub tool_calls: Option<Vec<ToolCall>>,
    pub tool_call_id: Option<String>,
    pub name: Option<String>,
    pub timestamp: Option<u64>,
    pub round_index: Option<u32>,
    /// Fields not explicitly modeled (e.g. `_compact_boundary`, `_reactive`,
    /// `_messages_removed`, `_turns_removed`, provider-specific fields).
    pub extra: BTreeMap<String, Value>,
}

/// A single tool-call entry inside an assistant message.
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    /// OpenAI spec requires `"type": "function"` on every tool call. Some
    /// providers reject the request when this field is missing, so we
    /// preserve whatever was on the wire instead of dropping it on round-trip.
    pub call_type: Option<String>,
    pub function: ToolCallFunction,
}

#[derive(Debug, Clone)]
pub struct ToolCallFunction {
    pub name: String,
    pub arguments: String,
}

impl From<Value> for Message {
    fn from(v: Value) -> Self {
        let obj = match v {
            Value::Object(o) => o,
            other => {
                // Non-Object input (e.g. bare string, number) must never
                // reach production silently — log it and return a sentinel
                // whose `role` is empty so callers can detect the error.
                tracing::error!("Message::from(Value) received non-Object: {other:?}");
                let mut m = BTreeMap::new();
                m.insert("_raw".to_string(), other);
                return Message {
                    role: String::new(),
                    content: None,
                    content_is_tool_result: false,
                    content_was_array: false,
                    content_modified: false,
                    is_synthetic: false,
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                    timestamp: None,
                    round_index: None,
                    extra: m,
                };
            }
        };

        // Known fields: read via get() instead of remove() so the
        // original map is not mutated — leaving `extra` as a faithful
        // superset of all keys.
        let role = obj
            .get("role")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_default();

        // We DELIBERATELY preserve empty-string content as Some("").
        // OpenAI assistant messages with `tool_calls` MUST carry
        // `content: ""` (not `null`) — a `null` round-trip causes some
        // strict providers (MiniMax, etc.) to reject the request.
        let (content, content_is_tool_result, content_was_array) = match obj.get("content") {
            Some(Value::String(s)) => (Some(s.clone()), false, false),
            Some(Value::Null) | None => (None, false, false),
            // Anthropic tool_result: content is an array of {type, tool_use_id, content}
            // Other arrays (text/image multi-block) are also stringified but
            // remembered via content_was_array so we can re-emit as an array.
            Some(Value::Array(arr)) => {
                let is_tool_result = arr.iter().any(|item| {
                    item.get("type")
                        .and_then(|v| v.as_str())
                        .is_some_and(|t| t == "tool_result")
                });
                (
                    Some(serde_json::to_string(arr).unwrap_or_default()),
                    is_tool_result,
                    true,
                )
            }
            Some(other) => (Some(other.to_string()), false, false),
        };

        let tool_calls = obj.get("tool_calls").and_then(|v| {
            v.as_array().map(|arr| {
                arr.iter()
                    .filter_map(|tc| {
                        let id = tc.get("id")?.as_str()?.to_string();
                        let call_type = tc.get("type").and_then(|t| t.as_str()).map(String::from);
                        let func = tc.get("function")?;
                        let name = func.get("name")?.as_str()?.to_string();
                        let arguments = func
                            .get("arguments")
                            .and_then(|a| a.as_str())
                            .unwrap_or("")
                            .to_string();
                        Some(ToolCall {
                            id,
                            call_type,
                            function: ToolCallFunction { name, arguments },
                        })
                    })
                    .collect()
            })
        });

        let tool_call_id = obj
            .get("tool_call_id")
            .and_then(|v| v.as_str().map(String::from));

        let name = obj.get("name").and_then(|v| v.as_str().map(String::from));

        let timestamp = obj.get("_timestamp").and_then(|v| v.as_u64());

        let round_index = obj
            .get("_round_index")
            .and_then(|v| v.as_u64())
            .and_then(|v| u32::try_from(v).ok());

        let is_synthetic = obj
            .get("_synthetic")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // Build extra: all keys NOT already modeled above.
        const KNOWN_KEYS: &[&str] = &[
            "role",
            "content",
            "tool_calls",
            "tool_call_id",
            "name",
            "_timestamp",
            "_round_index",
            "_synthetic",
        ];
        let extra: BTreeMap<String, Value> = obj
            .into_iter()
            .filter(|(k, _)| !KNOWN_KEYS.contains(&k.as_str()))
            .collect();

        Message {
            role,
            content,
            content_is_tool_result,
            content_was_array,
            content_modified: false,
            is_synthetic,
            tool_calls,
            tool_call_id,
            name,
            timestamp,
            round_index,
            extra,
        }
    }
}

impl From<Message> for Value {
    fn from(m: Message) -> Self {
        let mut map = Map::new();
        map.insert("role".to_string(), Value::String(m.role));
        if let Some(ref c) = m.content {
            // Restore array-shaped content (Anthropic tool_result OR multi-block
            // text/image arrays) losslessly. We ONLY parse-back when:
            //   1. The content was originally an array on ingest, AND
            //   2. No layer has overwritten it via `set_content` —
            //      a stub/truncation string must NOT be re-typed to an array,
            //      which would corrupt the wire shape.
            // A plain string that happens to look like JSON is never re-typed.
            let restored_array = if m.content_was_array && !m.content_modified {
                serde_json::from_str::<Value>(c)
                    .ok()
                    .filter(|v| v.is_array())
            } else {
                None
            };
            match restored_array {
                Some(arr) => {
                    map.insert("content".to_string(), arr);
                }
                None => {
                    map.insert("content".to_string(), Value::String(c.clone()));
                }
            }
        } else {
            map.insert("content".to_string(), Value::Null);
        }
        if let Some(tcs) = m.tool_calls {
            let calls: Vec<Value> = tcs
                .into_iter()
                .map(|tc| {
                    let mut fmap = Map::new();
                    fmap.insert("name".to_string(), Value::String(tc.function.name));
                    fmap.insert(
                        "arguments".to_string(),
                        Value::String(tc.function.arguments),
                    );
                    let mut tcmap = Map::new();
                    tcmap.insert("id".to_string(), Value::String(tc.id));
                    if let Some(t) = tc.call_type {
                        tcmap.insert("type".to_string(), Value::String(t));
                    }
                    tcmap.insert("function".to_string(), Value::Object(fmap));
                    Value::Object(tcmap)
                })
                .collect();
            map.insert("tool_calls".to_string(), Value::Array(calls));
        }
        if let Some(id) = m.tool_call_id {
            map.insert("tool_call_id".to_string(), Value::String(id));
        }
        if let Some(n) = m.name {
            map.insert("name".to_string(), Value::String(n));
        }
        if let Some(ts) = m.timestamp {
            map.insert("_timestamp".to_string(), Value::Number(ts.into()));
        }
        if let Some(ri) = m.round_index {
            map.insert("_round_index".to_string(), Value::Number(ri.into()));
        }
        if m.is_synthetic {
            map.insert("_synthetic".to_string(), Value::Bool(true));
        }
        // Dynamic compression telemetry must not survive the canonical
        // conversion because it would destabilize prompt-cache prefixes.
        // Stable user-turn semantics are different: they are durable
        // conversation metadata and are stripped only at the provider wire
        // boundary.
        for (k, v) in m.extra {
            if !k.starts_with('_')
                || matches!(
                    k.as_str(),
                    astra_turn_types::USER_TURN_SEMANTICS_FIELD
                        | astra_turn_types::TURN_MESSAGE_PROVENANCE_FIELD
                )
            {
                map.insert(k, v);
            }
        }
        Value::Object(map)
    }
}

impl Message {
    /// Returns `true` when this is a real user task message (not synthetic, not a tool_result array).
    pub fn is_plain_user_task(&self) -> bool {
        if self.role != "user" {
            return false;
        }
        // Anthropic tool_result arrays are never real user tasks.
        if self.content_is_tool_result {
            return false;
        }
        // Compaction-layer stubs / truncations are synthetic — exclude them.
        if self.is_synthetic {
            return false;
        }
        let content = match &self.content {
            Some(s) => s.as_str(),
            None => return false,
        };
        if content.trim().is_empty() || self.tool_call_id.is_some() {
            return false;
        }
        true
    }

    /// Set the content field (used for in-place stubbing/truncation).
    ///
    /// Marks the message as modified so the back-converter will not
    /// silently re-type the new string into a JSON array shape, even if
    /// the original was Anthropic-style.
    pub fn set_content(&mut self, content: String) {
        self.content = Some(content);
        self.content_modified = true;
    }
}

// ───────────────────────────── Budget / Pipeline ────────────────────────────

/// Token budget for a single turn.
#[derive(Debug, Clone)]
pub struct TokenBudget {
    /// Maximum prompt tokens for the current turn.
    pub max_prompt_tokens: u64,
    /// Last measured prompt tokens from the LLM response.
    pub last_measured_tokens: u64,
    /// Current LLM round index (0-based). Used to protect current-round tool
    /// results from compression — they haven't been seen by the LLM yet.
    pub current_round_index: Option<u32>,
    /// Current wall-clock time in seconds since UNIX epoch.
    /// Makes compaction layers deterministic when set explicitly.
    pub now_secs: u64,
}

impl TokenBudget {
    pub fn is_over_budget(&self) -> bool {
        self.max_prompt_tokens > 0 && self.last_measured_tokens > self.max_prompt_tokens
    }

    /// Estimated excess tokens (0 if under budget).
    pub fn excess_tokens(&self) -> u64 {
        self.last_measured_tokens
            .saturating_sub(self.max_prompt_tokens)
    }

    /// Rough pressure ratio (0.0 = no pressure, 1.0+ = over budget).
    pub fn pressure(&self) -> f64 {
        if self.max_prompt_tokens == 0 {
            return 0.0;
        }
        self.last_measured_tokens as f64 / self.max_prompt_tokens as f64
    }
}

/// Result of a single compression layer execution.
#[derive(Debug, Clone, Default)]
pub struct CompressionResult {
    /// How many messages were removed or replaced.
    pub messages_removed: usize,
    /// Estimated tokens freed (approximate).
    pub estimated_tokens_freed: u64,
    /// Human-readable description of what this layer did.
    pub description: String,
    /// Turn indices that were compressed/modified by this layer.
    pub affected_turns: Vec<u32>,
}

/// Outcome of running the full compression pipeline.
#[derive(Debug, Clone)]
pub struct PipelineOutcome {
    /// Per-layer results in execution order.
    pub layer_results: Vec<(String, CompressionResult)>,
    /// Total estimated tokens freed across all layers.
    pub total_tokens_freed: u64,
    /// Whether we believe the budget is now satisfied.
    pub budget_satisfied: bool,
}

/// A single compression layer.
pub trait CompressionLayer: Send + Sync {
    /// Human-readable name for logging / audit.
    fn name(&self) -> &str;

    /// Strongly-typed telemetry identity for this layer. Each impl must
    /// declare its own `CompressionMethod` variant so telemetry aggregation
    /// cannot silently drift when a layer name string is changed.
    fn method(&self) -> CompressionMethod;

    /// Minimum budget pressure (0.0–1.0) required for this layer to fire.
    /// The pipeline skips layers whose threshold exceeds the current
    /// (dynamically adjusted) pressure.
    fn trigger_pressure(&self) -> f64;

    /// Execute compression, mutating the message list in place.
    /// Returns what changed. The pipeline adjusts the running budget after
    /// each layer — layers do NOT need to second-guess previous layers.
    fn compress(&self, messages: &mut Vec<Message>, budget: &TokenBudget) -> CompressionResult;
}

// ───────────────────────────── Tests ───────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Test helper: build a default Message with the given role/content
    /// and all other fields zeroed. Replaces the boilerplate Message {..}
    /// literal so adding a new field doesn't churn every test.
    fn mk_msg(role: &str, content: Option<&str>) -> Message {
        Message {
            role: role.into(),
            content: content.map(String::from),
            content_is_tool_result: false,
            content_was_array: false,
            content_modified: false,
            is_synthetic: false,
            tool_calls: None,
            tool_call_id: None,
            name: None,
            timestamp: None,
            round_index: None,
            extra: BTreeMap::new(),
        }
    }

    // ── Round-trip tests ──────────────────────────────────────────────────

    #[test]
    fn round_trip_simple_user_message() {
        let v = json!({"role": "user", "content": "hello world"});
        let msg = Message::from(v.clone());
        let back = Value::from(msg);
        assert_eq!(v, back);
    }

    #[test]
    fn round_trip_assistant_with_tool_calls() {
        let v = json!({
            "role": "assistant",
            "content": "Let me check...",
            "tool_calls": [{
                "id": "call_1",
                "function": {"name": "grep", "arguments": "{\"pattern\": \"foo\"}"}
            }]
        });
        let msg = Message::from(v.clone());
        let back = Value::from(msg);
        assert_eq!(v, back);
    }

    #[test]
    fn round_trip_tool_result_message() {
        let v = json!({
            "role": "tool",
            "content": "42 matches found",
            "tool_call_id": "call_1",
            "name": "grep"
        });
        let msg = Message::from(v.clone());
        let back = Value::from(msg);
        assert_eq!(v, back);
    }

    #[test]
    fn round_trip_anthropic_tool_result_array() {
        // Anthropic-format: content is an array of tool_result blocks.
        let v = json!({
            "role": "user",
            "content": [
                {"type": "tool_result", "tool_use_id": "call_abc", "content": "output"}
            ]
        });
        let msg = Message::from(v.clone());
        assert!(
            msg.content_is_tool_result,
            "must detect Anthropic tool_result"
        );
        let back = Value::from(msg);
        assert_eq!(v, back, "Anthropic tool_result array must round-trip");
    }

    #[test]
    fn round_trip_anthropic_mixed_array_not_tool_result() {
        // Array content without tool_result blocks — still round-trippable.
        let v = json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "plain"},
                {"type": "image", "source": {}}
            ]
        });
        let msg = Message::from(v.clone());
        assert!(
            !msg.content_is_tool_result,
            "mixed array is not tool_result"
        );
        let back = Value::from(msg);
        assert_eq!(v, back);
    }

    #[test]
    fn round_trip_null_content() {
        let v = json!({"role": "system", "content": null});
        let msg = Message::from(v.clone());
        let back = Value::from(msg);
        assert_eq!(v, back);
    }

    #[test]
    fn round_trip_empty_string_content() {
        // Empty string MUST round-trip as empty string, not null.
        // OpenAI assistant messages with `tool_calls` carry `content: ""`,
        // and some providers (MiniMax) reject `content: null` in that
        // shape. This is a regression guard for the typed-pipeline migration.
        let v = json!({"role": "user", "content": ""});
        let msg = Message::from(v.clone());
        assert_eq!(
            msg.content.as_deref(),
            Some(""),
            "empty string preserved as Some(\"\")"
        );
        let back = Value::from(msg);
        assert_eq!(
            back["content"],
            Value::String(String::new()),
            "empty string content must round-trip as empty string, not null"
        );
    }

    #[test]
    fn round_trip_assistant_with_tool_calls_empty_content() {
        // Real-world OpenAI shape: assistant turn that emits tool_calls
        // and an empty-string content. Must NOT become content: null.
        let v = json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [{
                "id": "call_1",
                "type": "function",
                "function": {"name": "grep", "arguments": "{}"}
            }]
        });
        let back = Value::from(Message::from(v.clone()));
        assert_eq!(back["content"], Value::String(String::new()));
        assert_eq!(
            back["tool_calls"][0]["type"],
            Value::String("function".into())
        );
        assert_eq!(back, v, "OpenAI tool-call shape must round-trip exactly");
    }

    #[test]
    fn round_trip_tool_call_type_preserved() {
        // OpenAI requires `"type": "function"` on every tool call.
        // The typed migration must not drop this field on round-trip.
        let v = json!({
            "role": "assistant",
            "content": "ok",
            "tool_calls": [{
                "id": "c1",
                "type": "function",
                "function": {"name": "read_file", "arguments": "{\"path\":\"a.rs\"}"}
            }]
        });
        let back = Value::from(Message::from(v.clone()));
        assert_eq!(back, v, "tool_calls[].type must round-trip");
    }

    #[test]
    fn round_trip_tool_call_without_type_stays_without_type() {
        // Conversely, calls that arrive WITHOUT a `type` field stay
        // without one (do not invent it).
        let v = json!({
            "role": "assistant",
            "content": "ok",
            "tool_calls": [{
                "id": "c1",
                "function": {"name": "read_file", "arguments": "{}"}
            }]
        });
        let back = Value::from(Message::from(v.clone()));
        assert!(back["tool_calls"][0].get("type").is_none());
    }

    #[test]
    fn round_trip_number_content_is_stringified() {
        // Non-string content is .to_string()'d, but extra preserves the original keys.
        let v = json!({"role": "user", "content": 42});
        let msg = Message::from(v);
        assert_eq!(msg.content.as_deref(), Some("42"));
        // Round-trip: the content object key is consumed into extra; the
        // known key "content" carries the stringified version. This is
        // lossy for the shape but preserves the value.
    }

    #[test]
    fn round_trip_extra_fields_preserved() {
        // Non-`_`-prefixed extra keys must survive round-trip (these are
        // provider-visible fields the model expects to see).
        let v = json!({
            "role": "user",
            "content": "hi",
            "custom_provider_field": {"nested": true}
        });
        let msg = Message::from(v.clone());
        let back = Value::from(msg);
        assert_eq!(back["custom_provider_field"], json!({"nested": true}));
        assert_eq!(back["role"], Value::String("user".into()));
        assert_eq!(back["content"], Value::String("hi".into()));
    }

    #[test]
    fn canonical_conversion_preserves_stable_identity_but_strips_dynamic_extras() {
        // Dynamic telemetry counters (`_messages_removed`, `_turns_removed`,
        // `_reactive`, `_compact_boundary`) live in `extra` but must NOT
        // cross the provider wire: they change across re-compactions and
        // would bust the prompt-cache prefix at the boundary even when
        // `content` is stable. See `From<Message> for Value`.
        let v = json!({
            "role": "system",
            "content": "[compacted]",
            "_compact_boundary": true,
            "_reactive": false,
            "_messages_removed": 5,
            "_turns_removed": 2,
            astra_turn_types::USER_TURN_SEMANTICS_FIELD: {
                "schema_version": 1,
                "objective_relation": "refine"
            },
            astra_turn_types::TURN_MESSAGE_PROVENANCE_FIELD: {
                "schema_version": 1,
                "turn_chain_id": "chain-1"
            },
            "custom_provider_field": {"ok": true}
        });
        let msg = Message::from(v);
        let back = Value::from(msg);
        assert!(
            back.get("_compact_boundary").is_none(),
            "_compact_boundary must not cross provider wire"
        );
        assert!(
            back.get("_reactive").is_none(),
            "_reactive must not cross provider wire"
        );
        assert!(
            back.get("_messages_removed").is_none(),
            "_messages_removed must not cross provider wire"
        );
        assert!(
            back.get("_turns_removed").is_none(),
            "_turns_removed must not cross provider wire"
        );
        assert!(
            back.get(astra_turn_types::USER_TURN_SEMANTICS_FIELD)
                .is_some(),
            "stable turn semantics must survive canonical compaction"
        );
        assert!(
            back.get(astra_turn_types::TURN_MESSAGE_PROVENANCE_FIELD)
                .is_some(),
            "bridge turn identity must survive context optimization"
        );
        // Non-`_`-prefixed extras still pass through.
        assert_eq!(back["custom_provider_field"], json!({"ok": true}));
        assert_eq!(back["content"], Value::String("[compacted]".into()));
    }

    #[test]
    fn round_trip_timestamp_and_round_index() {
        let v = json!({
            "role": "assistant",
            "content": "done",
            "_timestamp": 1717000000000_u64,
            "_round_index": 3
        });
        let msg = Message::from(v.clone());
        assert_eq!(msg.timestamp, Some(1717000000000));
        assert_eq!(msg.round_index, Some(3));
        let back = Value::from(msg);
        assert_eq!(back["_timestamp"], json!(1717000000000_u64));
        assert_eq!(back["_round_index"], json!(3));
    }

    #[test]
    fn round_trip_system_message_no_content() {
        let v = json!({"role": "system"});
        let msg = Message::from(v.clone());
        let back = Value::from(msg);
        assert_eq!(back["role"], Value::String("system".into()));
        assert_eq!(back["content"], Value::Null);
    }

    #[test]
    fn round_trip_multiple_tool_calls() {
        let v = json!({
            "role": "assistant",
            "tool_calls": [
                {"id": "c1", "function": {"name": "read_file", "arguments": "{}"}},
                {"id": "c2", "function": {"name": "grep", "arguments": "{\"pat\": \"X\"}"}}
            ]
        });
        let msg = Message::from(v.clone());
        assert_eq!(msg.tool_calls.as_ref().unwrap().len(), 2);
        let back = Value::from(msg);
        assert_eq!(back["tool_calls"].as_array().unwrap().len(), 2);
        // content field is always present in the output (null if absent in input).
        assert_eq!(back["role"], Value::String("assistant".into()));
        assert_eq!(back["content"], Value::Null);
        assert_eq!(back["tool_calls"].as_array().unwrap().len(), 2);
    }

    // ── is_plain_user_task tests ──────────────────────────────────────────

    #[test]
    fn plain_user_task_normal_message() {
        let m = mk_msg("user", Some("Please write a function"));
        assert!(m.is_plain_user_task());
    }

    #[test]
    fn plain_user_task_not_user_role() {
        let m = mk_msg("assistant", Some("result"));
        assert!(!m.is_plain_user_task());
    }

    #[test]
    fn plain_user_task_empty_content() {
        let m = mk_msg("user", None);
        assert!(!m.is_plain_user_task());
    }

    #[test]
    fn plain_user_task_whitespace_only() {
        let m = mk_msg("user", Some("   "));
        assert!(!m.is_plain_user_task());
    }

    #[test]
    fn plain_user_task_has_tool_call_id() {
        let mut m = mk_msg("user", Some("real content"));
        m.tool_call_id = Some("call_123".into());
        assert!(!m.is_plain_user_task());
    }

    #[test]
    fn plain_user_task_synthetic_rejected() {
        let mut m = mk_msg("user", Some("real content"));
        m.is_synthetic = true;
        assert!(!m.is_plain_user_task());
    }

    #[test]
    fn synthetic_round_trips_through_value() {
        let v = json!({"role": "user", "content": "hi", "_synthetic": true});
        let msg = Message::from(v);
        assert!(msg.is_synthetic);
        let back = Value::from(msg);
        assert_eq!(back["_synthetic"], json!(true));
    }

    #[test]
    fn synthetic_false_not_serialized() {
        let v = json!({"role": "user", "content": "hi"});
        let msg = Message::from(v);
        assert!(!msg.is_synthetic);
        let back = Value::from(msg);
        assert!(
            back.get("_synthetic").is_none(),
            "false _synthetic must not appear in output"
        );
    }

    #[test]
    fn round_index_overflow_dropped() {
        // u64::MAX overflows u32 — must be dropped, not silently truncated.
        let v = json!({"role": "user", "content": "hi", "_round_index": 5_000_000_000u64});
        let msg = Message::from(v);
        assert!(
            msg.round_index.is_none(),
            "overflowing _round_index must be None"
        );
    }

    #[test]
    fn set_content_updates_field() {
        let mut m = mk_msg("user", None);
        m.set_content("new content".into());
        assert_eq!(m.content.as_deref(), Some("new content"));
    }

    #[test]
    fn modified_anthropic_tool_result_does_not_re_type_to_array() {
        // Ingest: Anthropic tool_result array (content_was_array=true).
        // A layer overwrites content with a stub string. Back-conversion
        // MUST emit a string (the stub), NOT silently re-type the stub
        // back into the original array shape — that would corrupt the
        // wire format and produce a malformed request.
        let v = json!({
            "role": "user",
            "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "huge"}
            ]
        });
        let mut msg = Message::from(v);
        assert!(msg.content_was_array);
        msg.set_content("[duplicate read of `foo` — content available in a later read]".into());
        let back = Value::from(msg);
        assert!(
            back["content"].is_string(),
            "modified content must serialize as String, got: {back:?}"
        );
    }

    #[test]
    fn set_content_marks_modified() {
        let mut m = mk_msg("user", Some("hi"));
        assert!(!m.content_modified);
        m.set_content("changed".into());
        assert!(
            m.content_modified,
            "set_content must flip content_modified so back-conversion does not re-type"
        );
    }

    // ── TokenBudget tests ─────────────────────────────────────────────────

    #[test]
    fn budget_over_when_exceeds() {
        let b = TokenBudget {
            max_prompt_tokens: 100,
            last_measured_tokens: 150,
            current_round_index: None,
            now_secs: 0,
        };
        assert!(b.is_over_budget());
        assert_eq!(b.excess_tokens(), 50);
        assert!(b.pressure() > 1.0);
    }

    #[test]
    fn budget_under() {
        let b = TokenBudget {
            max_prompt_tokens: 100,
            last_measured_tokens: 80,
            current_round_index: None,
            now_secs: 0,
        };
        assert!(!b.is_over_budget());
        assert_eq!(b.excess_tokens(), 0);
        assert!(b.pressure() < 1.0);
    }

    #[test]
    fn budget_zero_max() {
        let b = TokenBudget {
            max_prompt_tokens: 0,
            last_measured_tokens: 500,
            current_round_index: None,
            now_secs: 0,
        };
        assert!(!b.is_over_budget());
        assert_eq!(b.pressure(), 0.0);
    }

    #[test]
    fn message_from_non_object_returns_sentinel() {
        let m = Message::from(serde_json::Value::String("bare string".into()));
        assert!(m.role.is_empty(), "role must be empty for sentinel");
        assert!(m.content.is_none());
        assert!(m.tool_calls.is_none());
        assert!(m.tool_call_id.is_none());
        assert!(
            m.extra.contains_key("_raw"),
            "sentinel must preserve _raw for diagnostics"
        );
    }
}
