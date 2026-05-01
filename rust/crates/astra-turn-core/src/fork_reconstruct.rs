//! Reconstruct a child agent's first-request message array from a
//! captured parent [`ForkPrefix`].
//!
//! ## Role in the fork-prefix pipeline
//!
//! - PR 1: [`ForkPrefix`] type (frozen canonical bytes)
//! - PR 2: [`PrefixCaptureSink`] store
//! - PR 3: capture-site helper
//! - PR 4: spawn-side resolver
//! - PR 4.5: runtime wires resolver into spawner
//! - PR 5a: turn-loop hook
//! - **PR 5b (this)**: the reconstructor — parses prefix canonical
//!   bytes back into a JSON `messages` array and concatenates a
//!   child suffix. Downstream provider adapters (astra's
//!   `chat_turn_base_payload` in particular) use the result as the
//!   `messages` field of the child's first request.
//! - PR 5c: `ForkCacheEvent` telemetry on child's first response.
//!
//! ## Why this lives in turn-core
//!
//! The astra turn payload is built in multiple crates (CLI, server,
//! bridge) via `chat_turn_base_payload`. All of them need the same
//! prefix deserialization / concatenation invariants; keeping the
//! helper in `turn-core` is the single source of truth.
//!
//! ## What the prefix bytes contain
//!
//! By convention (enforced by the capture-site contract, PR 3), the
//! canonical bytes hold a **JSON-serialized array of message
//! objects** — the same shape as the `messages` field of
//! `chat_turn_base_payload`. The reconstructor round-trips through
//! `serde_json::Value` so the prefix survives formatting quirks in
//! the serializer (whitespace, key-order on write), as long as the
//! bytes produced by capture and the bytes produced by reconstruct
//! are both canonicalized by [`crate::fork_prefix::hash_tool_schema`]'s
//! key-sorting serializer. Callers that need byte-identical output
//! should re-serialize via that canonicalizer after assembling the
//! full request.
//!
//! This module does NOT:
//! - Attach the result to a live HTTP request (provider glue in
//!   CLI / server).
//! - Consume `system_blocks` / `tool_schemas` / `beta_headers` —
//!   those are attached by the provider adapter at request-build
//!   time; reconstruct only handles `messages`.
//! - Emit telemetry on success / failure — the caller does.

use serde_json::Value;
use thiserror::Error;

use crate::fork_prefix::ForkPrefix;

/// Outcome of a successful `reconstruct_messages` call.
#[derive(Debug, Clone)]
pub struct ReconstructedMessages {
    /// Parent prefix messages (from canonical bytes) followed by the
    /// caller-supplied child suffix, concatenated in order.
    pub messages: Vec<Value>,
    /// Number of messages contributed by the prefix. Equals
    /// `messages.len() - child_suffix_len`. Callers that want to
    /// assert "the prefix region is byte-identical to the parent's
    /// captured bytes" should re-canonicalize `messages[..prefix_len]`
    /// and compare against `prefix.canonical_prefix_bytes()`.
    pub prefix_len: usize,
}

/// Errors that prevent reconstruction. All are caller-facing — the
/// caller should fall back to a fresh (non-inheriting) spawn and
/// emit a telemetry event naming the cause.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ReconstructError {
    /// The canonical prefix bytes did not parse as valid JSON.
    #[error("prefix canonical bytes are not valid JSON: {detail}")]
    MalformedPrefixJson { detail: String },
    /// Parsed but was not a top-level array. The capture-site
    /// contract requires a JSON array of message objects; anything
    /// else means the capturer used a different schema and the
    /// reconstructor cannot safely guess.
    #[error("prefix canonical bytes are not a JSON array (got {kind})")]
    NotAnArray { kind: &'static str },
    /// One of the prefix array elements is not a JSON object. Message
    /// wire format is `{"role":..., "content":...}`; non-objects
    /// (nulls, strings, numbers) signal a captured garbage state.
    #[error("prefix message at index {index} is not an object (got {kind})")]
    NonObjectElement { index: usize, kind: &'static str },
    /// A child suffix element is not a JSON object. Caught here, at
    /// the reconstruction boundary, so downstream payload builders
    /// don't have to guard per-index.
    #[error("child suffix message at index {index} is not an object (got {kind})")]
    NonObjectChildElement { index: usize, kind: &'static str },
}

/// Reconstruct the child's first-request `messages` array from a
/// captured prefix and a caller-supplied suffix.
///
/// On success the returned `ReconstructedMessages.messages` is the
/// concatenation `[..prefix_msgs, ..child_suffix]`. `prefix_len` is
/// `prefix_msgs.len()` so the caller can assert byte identity on the
/// prefix region.
///
/// On failure, the caller drops the cache inheritance attempt and
/// falls back to a fresh spawn. The error is structured so telemetry
/// can bucket failures (malformed / wrong-type / child-malformed)
/// independently.
pub fn reconstruct_messages(
    prefix: &ForkPrefix,
    child_suffix: Vec<Value>,
) -> Result<ReconstructedMessages, ReconstructError> {
    let bytes: &[u8] = prefix.canonical_prefix_bytes().as_ref();
    let value: Value =
        serde_json::from_slice(bytes).map_err(|e| ReconstructError::MalformedPrefixJson {
            detail: e.to_string(),
        })?;

    let mut prefix_msgs = match value {
        Value::Array(arr) => arr,
        other => {
            return Err(ReconstructError::NotAnArray {
                kind: json_kind(&other),
            });
        }
    };

    for (index, msg) in prefix_msgs.iter().enumerate() {
        if !msg.is_object() {
            return Err(ReconstructError::NonObjectElement {
                index,
                kind: json_kind(msg),
            });
        }
    }

    for (index, msg) in child_suffix.iter().enumerate() {
        if !msg.is_object() {
            return Err(ReconstructError::NonObjectChildElement {
                index,
                kind: json_kind(msg),
            });
        }
    }

    let prefix_len = prefix_msgs.len();
    prefix_msgs.extend(child_suffix);
    Ok(ReconstructedMessages {
        messages: prefix_msgs,
        prefix_len,
    })
}

/// Stable label for telemetry — matches `Value::*` variant names so
/// dashboards can group by type without custom mapping.
fn json_kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fork_prefix::{
        CacheMode, ForkPrefix, ProviderKind, SystemBlock, ThinkingConfigSlice, ToolSchemaEntry,
        hash_tool_schema,
    };
    use serde_json::json;

    /// Build a `ForkPrefix` with the given canonical bytes. All other
    /// fields get defaults from the prefix_bytes_shape we're testing.
    fn prefix_with_bytes(bytes: Vec<u8>) -> ForkPrefix {
        let schema = json!({"function": {"name": "bash"}});
        let (schema_bytes, schema_hash) = hash_tool_schema(&schema);
        ForkPrefix::build(
            "pfx-test",
            "run-parent",
            1,
            1_700_000_000,
            ProviderKind::Anthropic,
            "claude-opus-4-6",
            Some(ThinkingConfigSlice {
                enabled: false,
                budget_tokens: 0,
                kind: "disabled".into(),
            }),
            vec![SystemBlock {
                bytes: b"sys".to_vec(),
                has_cache_control: true,
            }],
            vec![ToolSchemaEntry {
                name: "bash".into(),
                canonical_bytes: schema_bytes,
                hash: schema_hash,
            }],
            vec![],
            bytes,
            CacheMode::Write,
        )
    }

    /// Serialize a `Vec<Value>` to bytes via `serde_json` — this is
    /// the CAPTURE-SIDE convention: capture serialises messages as a
    /// plain JSON array, reconstruct parses them back.
    fn encode_messages(msgs: &[Value]) -> Vec<u8> {
        serde_json::to_vec(&msgs).unwrap()
    }

    // --- Happy path --------------------------------------------------

    #[test]
    fn reconstructs_prefix_plus_suffix_in_order() {
        let parent_msgs = vec![
            json!({"role": "user", "content": "hello"}),
            json!({"role": "assistant", "content": "hi"}),
        ];
        let prefix = prefix_with_bytes(encode_messages(&parent_msgs));
        let suffix = vec![json!({"role": "user", "content": "new task"})];

        let out = reconstruct_messages(&prefix, suffix.clone()).unwrap();
        assert_eq!(out.prefix_len, 2);
        assert_eq!(out.messages.len(), 3);
        assert_eq!(out.messages[0], parent_msgs[0]);
        assert_eq!(out.messages[1], parent_msgs[1]);
        assert_eq!(out.messages[2], suffix[0]);
    }

    #[test]
    fn empty_suffix_returns_only_prefix() {
        let parent_msgs = vec![json!({"role": "user", "content": "hi"})];
        let prefix = prefix_with_bytes(encode_messages(&parent_msgs));
        let out = reconstruct_messages(&prefix, vec![]).unwrap();
        assert_eq!(out.prefix_len, 1);
        assert_eq!(out.messages, parent_msgs);
    }

    #[test]
    fn empty_prefix_returns_only_suffix() {
        // A degenerate but legal prefix: empty messages array. The
        // capture helper's NoCacheableState guard (PR 3) should
        // have rejected this upstream, but the reconstructor must
        // still handle it without panicking.
        let prefix = prefix_with_bytes(b"[]".to_vec());
        let suffix = vec![json!({"role": "user", "content": "fresh"})];
        let out = reconstruct_messages(&prefix, suffix.clone()).unwrap();
        assert_eq!(out.prefix_len, 0);
        assert_eq!(out.messages, suffix);
    }

    #[test]
    fn prefix_region_byte_identical_to_captured_bytes_after_recanonicalize() {
        // The CRITICAL invariant for cache reuse: once we reconstruct
        // and then re-serialize the prefix region, the bytes must
        // equal the original capture. We use serde_json's default
        // serializer here (the capture used the same) — which is
        // stable for the shape messages take (objects + arrays of
        // primitives / objects).
        let parent_msgs = vec![
            json!({"role": "user", "content": "q1"}),
            json!({
                "role": "assistant",
                "content": [{"type": "text", "text": "a1"}]
            }),
        ];
        let original = encode_messages(&parent_msgs);
        let prefix = prefix_with_bytes(original.clone());
        let out = reconstruct_messages(&prefix, vec![]).unwrap();
        let reserialized = encode_messages(&out.messages[..out.prefix_len]);
        assert_eq!(
            reserialized, original,
            "re-serialized prefix region must equal captured bytes"
        );
    }

    // --- Errors -----------------------------------------------------

    #[test]
    fn malformed_json_returns_error() {
        let prefix = prefix_with_bytes(b"not json {{".to_vec());
        let err = reconstruct_messages(&prefix, vec![]).unwrap_err();
        assert!(matches!(err, ReconstructError::MalformedPrefixJson { .. }));
    }

    #[test]
    fn non_array_prefix_returns_error() {
        // Parses as JSON but it's an object, not an array.
        let prefix = prefix_with_bytes(br#"{"role": "user"}"#.to_vec());
        match reconstruct_messages(&prefix, vec![]).unwrap_err() {
            ReconstructError::NotAnArray { kind } => assert_eq!(kind, "object"),
            other => panic!("expected NotAnArray, got {other:?}"),
        }
    }

    #[test]
    fn non_object_prefix_element_returns_error() {
        // A top-level array, but one element is a string instead
        // of an object — garbage capture, don't let it through.
        let prefix = prefix_with_bytes(br#"[{"role":"user"},"stray"]"#.to_vec());
        match reconstruct_messages(&prefix, vec![]).unwrap_err() {
            ReconstructError::NonObjectElement { index, kind } => {
                assert_eq!(index, 1);
                assert_eq!(kind, "string");
            }
            other => panic!("expected NonObjectElement, got {other:?}"),
        }
    }

    #[test]
    fn non_object_child_suffix_element_returns_error() {
        // Reconstructor catches malformed suffix at the boundary so
        // downstream payload builders don't have to.
        let prefix = prefix_with_bytes(b"[]".to_vec());
        let suffix = vec![json!({"role": "user"}), json!("oops not an object")];
        match reconstruct_messages(&prefix, suffix).unwrap_err() {
            ReconstructError::NonObjectChildElement { index, kind } => {
                assert_eq!(index, 1);
                assert_eq!(kind, "string");
            }
            other => panic!("expected NonObjectChildElement, got {other:?}"),
        }
    }

    // --- Invariants --------------------------------------------------

    #[test]
    fn prefix_len_matches_captured_message_count() {
        // prefix_len MUST equal the number of messages the capture
        // recorded. Downstream code indexes `messages[..prefix_len]`
        // to re-hash / diff the prefix region; an off-by-one here
        // would silently break cache-break attribution.
        for n in [0usize, 1, 2, 5, 10] {
            let msgs: Vec<Value> = (0..n)
                .map(|i| json!({"role": "user", "content": format!("m{i}")}))
                .collect();
            let prefix = prefix_with_bytes(encode_messages(&msgs));
            let out = reconstruct_messages(&prefix, vec![]).unwrap();
            assert_eq!(out.prefix_len, n, "prefix_len mismatch at n={n}");
        }
    }

    #[test]
    fn large_suffix_does_not_affect_prefix_len() {
        let parent = vec![json!({"role": "user", "content": "hi"})];
        let prefix = prefix_with_bytes(encode_messages(&parent));
        let suffix: Vec<Value> = (0..50)
            .map(|i| json!({"role": "user", "content": format!("s{i}")}))
            .collect();
        let out = reconstruct_messages(&prefix, suffix).unwrap();
        assert_eq!(out.prefix_len, 1);
        assert_eq!(out.messages.len(), 51);
    }

    #[test]
    fn json_kind_labels_are_stable() {
        // Tripwire: dashboards may bucket by these label strings.
        // Changing them is an observable external contract change.
        assert_eq!(json_kind(&Value::Null), "null");
        assert_eq!(json_kind(&Value::Bool(true)), "bool");
        assert_eq!(json_kind(&json!(42)), "number");
        assert_eq!(json_kind(&json!("x")), "string");
        assert_eq!(json_kind(&json!([])), "array");
        assert_eq!(json_kind(&json!({})), "object");
    }
}
