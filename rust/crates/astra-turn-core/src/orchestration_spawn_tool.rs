//! Spawn agent tool schema and types.

use serde::{Deserialize, Serialize};
use serde_json::json;

/// Request to inherit the parent's cacheable prefix when spawning.
///
/// When present in a `SpawnAgentInput`, the runtime looks up the
/// captured `ForkPrefix` for `from_run_id` (defaulting to the caller's
/// own run) and validates compatibility (provider, model, thinking
/// budget, size). If lookup or validation fails:
/// - `required: false` (default) — spawn proceeds without cache
///   inheritance and a telemetry event is emitted.
/// - `required: true` — spawn fails with an error.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct InheritPrefixSpec {
    /// Parent run id to inherit from. `None` means "the run calling
    /// spawn_agent" — the resolver substitutes the caller's run id
    /// at resolution time.
    #[serde(default)]
    pub from_run_id: Option<String>,

    /// Whether a missing or incompatible prefix is a hard failure.
    /// Default `false` keeps the spawn robust to eviction / TTL /
    /// feature-flag transitions.
    #[serde(default)]
    pub required: bool,
}

/// Input for the spawn_agent tool.
///
/// **Field order is load-bearing.** The struct is serialized to
/// JSON for the spawn_agent tool schema and included in tool-schema
/// cache-break attribution (see `cache_diagnostics.rs::per_tool_hashes`).
/// Reordering fields changes the canonical JSON bytes and invalidates
/// every captured parent prefix across a deploy. Add new fields at
/// the end; never reorder existing ones without a coordinated
/// migration.
#[derive(Debug, Clone, Deserialize)]
pub struct SpawnAgentInput {
    /// Short (3-5 word) description of the task.
    pub description: String,

    /// Detailed task prompt for the agent.
    pub prompt: String,

    /// Agent type: "explore", "code-review", "task", "general-purpose".
    #[serde(default = "default_agent_type")]
    pub agent_type: String,

    /// Optional model override.
    pub model: Option<String>,

    /// Run in background (async). Default false — synchronous mode
    /// ensures the parent receives the child's result in the tool-call
    /// response before its turn budget is consumed.
    #[serde(default)]
    pub background: bool,

    /// Name for agent-to-agent messaging.
    pub name: Option<String>,

    /// Max turns before auto-stopping.
    pub max_turns: Option<u32>,

    /// Max output tokens for the child's first API call. When a
    /// `ForkPrefix` is inherited with thinking enabled, the
    /// resolver will refuse the inheritance if this cap would
    /// clamp the effective thinking budget below the captured
    /// parent value (cache key drift).
    #[serde(default)]
    pub max_output_tokens: Option<u32>,

    /// Create isolated git worktree for this agent.
    #[serde(default)]
    pub isolated: bool,

    /// Tool allowlist (overrides agent_type defaults).
    pub allowed_tools: Option<Vec<String>>,

    /// Optional request to reuse a captured parent ForkPrefix for
    /// cache inheritance. When absent, spawn proceeds with a fresh
    /// prefix (no cache reuse). See [`InheritPrefixSpec`].
    ///
    /// Uses a custom deserializer so models that serialize the
    /// "empty opt-in" variant as a string `"{}"` or `""` (observed
    /// with MiniMax-M2.5: "invalid type: string \"{}\", expected
    /// struct InheritPrefixSpec") still produce a valid default
    /// spec instead of the whole tool call failing with a schema
    /// validation error. Proper JSON objects continue to parse as
    /// before.
    #[serde(default, deserialize_with = "deserialize_inherit_prefix_lenient")]
    pub inherit_prefix: Option<InheritPrefixSpec>,
}

/// Lenient deserializer for `inherit_prefix`: accepts a JSON object
/// (proper shape), a string `"{}"` / `""` / `"default"` (model
/// fat-fingered the empty object as a string), null, or absence.
/// Every other shape still fails cleanly — we don't want a literal
/// `"yes"` or a JSON number silently producing a default.
fn deserialize_inherit_prefix_lenient<'de, D>(
    deserializer: D,
) -> Result<Option<InheritPrefixSpec>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    let v: Option<serde_json::Value> = Option::deserialize(deserializer)?;
    let Some(v) = v else {
        return Ok(None);
    };
    match v {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::Object(_) => {
            // Proper path: parse as the structured type.
            serde_json::from_value::<InheritPrefixSpec>(v)
                .map(Some)
                .map_err(Error::custom)
        }
        serde_json::Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() || trimmed == "{}" || trimmed.eq_ignore_ascii_case("default") {
                // Model meant "opt in with defaults"; give them that.
                Ok(Some(InheritPrefixSpec::default()))
            } else {
                // Try to parse the string AS JSON so payloads like
                // `"{\"required\": true}"` (also observed) still work.
                let parsed: serde_json::Value = serde_json::from_str(trimmed).map_err(|_| {
                    Error::custom(format!(
                        "inherit_prefix string \"{s}\" is not a recognized shorthand \
                         (allowed: \"\" / \"{{}}\" / \"default\") nor a JSON object literal"
                    ))
                })?;
                if parsed.is_object() {
                    serde_json::from_value::<InheritPrefixSpec>(parsed)
                        .map(Some)
                        .map_err(Error::custom)
                } else {
                    Err(Error::custom(format!(
                        "inherit_prefix must be an object; got stringified non-object: {s}"
                    )))
                }
            }
        }
        other => Err(Error::custom(format!(
            "inherit_prefix must be an object; got {}",
            match other {
                serde_json::Value::Bool(_) => "bool",
                serde_json::Value::Number(_) => "number",
                serde_json::Value::Array(_) => "array",
                _ => unreachable!(),
            }
        ))),
    }
}

impl Default for SpawnAgentInput {
    /// Mirror the serde `#[serde(default ...)]` defaults so struct
    /// literals using `..Default::default()` produce the same
    /// instance as an empty JSON `{"description": "", "prompt": ""}`.
    fn default() -> Self {
        Self {
            description: String::new(),
            prompt: String::new(),
            agent_type: default_agent_type(),
            model: None,
            background: false,
            name: None,
            max_turns: None,
            max_output_tokens: None,
            isolated: false,
            allowed_tools: None,
            inherit_prefix: None,
        }
    }
}

fn default_agent_type() -> String {
    "general-purpose".to_string()
}

/// Output from spawn_agent tool.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SpawnAgentOutput {
    /// Agent completed synchronously.
    Completed {
        agent_id: String,
        result: String,
        tool_calls: u32,
        duration_ms: u64,
    },
    /// Agent was cancelled synchronously.
    Cancelled {
        agent_id: String,
        reason: String,
        tool_calls: u32,
        duration_ms: u64,
    },
    /// Agent is waiting for external input synchronously.
    Waiting {
        agent_id: String,
        reason: String,
        tool_calls: u32,
        duration_ms: u64,
    },
    /// Agent launched in background.
    Launched {
        agent_id: String,
        description: String,
        messaging_address: Option<String>,
    },
    /// Failed to spawn.
    Failed { error: String },
}

impl SpawnAgentOutput {
    pub fn launched(agent_id: impl Into<String>, description: impl Into<String>) -> Self {
        Self::Launched {
            agent_id: agent_id.into(),
            description: description.into(),
            messaging_address: None,
        }
    }
}

/// Generate the JSON schema for spawn_agent tool.
/// Returns a schema in the standard format: `{ type: "function", function: { name, description, parameters } }`.
pub fn spawn_agent_schema() -> serde_json::Value {
    json!({
        "type": "function",
        "function": {
            "name": "spawn_agent",
            "description": "Launch a sub-agent for independent work. Types: explore, code-review, task, general-purpose. Use `inherit_prefix: {}` when the child builds on current context.",
            "parameters": {
                "type": "object",
                "properties": {
                    "description": {
                        "type": "string",
                        "description": "A short (3-5 word) description of the task."
                    },
                    "prompt": {
                        "type": "string",
                        "description": "Detailed task prompt for the agent. Be specific about what you want."
                    },
                    "agent_type": {
                        "type": "string",
                        "enum": ["explore", "code-review", "task", "general-purpose"],
                        "description": "Agent type: explore (research), code-review, task, or general-purpose.",
                        "default": "general-purpose"
                    },
                    "model": {
                        "type": "string",
                        "description": "Optional model override."
                    },
                    "background": {
                        "type": "boolean",
                        "description": "If true, return immediately with agent_id. Default false waits for the result.",
                        "default": false
                    },
                    "name": {
                        "type": "string",
                        "description": "Name for agent-to-agent messaging. Makes agent addressable via send_message."
                    },
                    "max_turns": {
                        "type": "integer",
                        "description": "Max turns before stopping. Default varies by agent_type.",
                        "minimum": 1,
                        "maximum": 100
                    },
                    "isolated": {
                        "type": "boolean",
                        "description": "Create isolated git worktree for this agent.",
                        "default": false
                    },
                    "allowed_tools": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Tool allowlist (overrides agent_type defaults)."
                    },
                    "max_output_tokens": {
                        "type": "integer",
                        "description": "Max output tokens for the child's first API call.",
                        "minimum": 1
                    },
                    "inherit_prefix": {
                        "type": "object",
                        "description": "RECOMMENDED when the child builds on current context: pass `{}` to inherit the parent's prompt-cache prefix, cutting first-turn input tokens and latency (often 70-95% reuse). Omit for independent tasks. Use {\"required\": true} only when missing inherited context should fail the spawn.",
                        "properties": {
                            "from_run_id": {
                                "type": "string",
                                "description": "Parent run id. Omit to use the caller's run."
                            },
                            "required": {
                                "type": "boolean",
                                "description": "If true, fail when the prefix is missing or incompatible. Default false.",
                                "default": false
                            }
                        }
                    }
                },
                "required": ["description", "prompt"]
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_spawn_agent_schema() {
        let schema = spawn_agent_schema();
        assert_eq!(schema["type"], "function");
        assert_eq!(schema["function"]["name"], "spawn_agent");
        assert!(schema["function"]["parameters"]["properties"]["description"].is_object());
    }

    #[test]
    fn test_deserialize_input() {
        let json = r#"{"description": "Test", "prompt": "Do the thing"}"#;
        let input: SpawnAgentInput = serde_json::from_str(json).unwrap();
        assert_eq!(input.description, "Test");
        assert_eq!(input.agent_type, "general-purpose");
        // Default is synchronous (background=false) so the parent
        // receives the child's result in the tool-call response.
        assert!(!input.background);
        // Inheritance defaults to None — existing clients get no
        // behavior change when they don't set inherit_prefix.
        assert!(input.inherit_prefix.is_none());
        assert!(input.max_output_tokens.is_none());
    }

    #[test]
    fn background_default_is_false() {
        let input = SpawnAgentInput::default();
        assert!(
            !input.background,
            "background must default to false — synchronous spawn \
             ensures the parent receives the child's result before \
             its turn budget is consumed"
        );
    }

    #[test]
    fn background_true_requires_explicit_opt_in() {
        let json = r#"{"description": "D", "prompt": "P", "background": true}"#;
        let input: SpawnAgentInput = serde_json::from_str(json).unwrap();
        assert!(
            input.background,
            "explicit background: true must be honored"
        );
    }

    #[test]
    fn test_deserialize_inherit_prefix_defaults() {
        let json = r#"{
            "description": "D",
            "prompt": "P",
            "inherit_prefix": {}
        }"#;
        let input: SpawnAgentInput = serde_json::from_str(json).unwrap();
        let spec = input.inherit_prefix.expect("inherit_prefix present");
        assert_eq!(spec.from_run_id, None);
        assert!(!spec.required, "required defaults to false");
    }

    #[test]
    fn test_deserialize_inherit_prefix_explicit() {
        let json = r#"{
            "description": "D",
            "prompt": "P",
            "inherit_prefix": {"from_run_id": "run-parent", "required": true},
            "max_output_tokens": 8000
        }"#;
        let input: SpawnAgentInput = serde_json::from_str(json).unwrap();
        let spec = input.inherit_prefix.unwrap();
        assert_eq!(spec.from_run_id.as_deref(), Some("run-parent"));
        assert!(spec.required);
        assert_eq!(input.max_output_tokens, Some(8000));
    }

    #[test]
    fn test_schema_exposes_inherit_prefix() {
        let schema = spawn_agent_schema();
        let props = &schema["function"]["parameters"]["properties"];
        assert!(props["inherit_prefix"].is_object());
        assert!(props["max_output_tokens"].is_object());
    }

    #[test]
    fn schema_top_level_description_hints_at_inherit_prefix() {
        // Tripwire: models discover `inherit_prefix` partly from the
        // tool's top-level description. If the hint gets rewritten
        // away, observed inherit_prefix usage drops to ~zero in the
        // wild (verified: MiniMax-M2.5 doesn't pass the field unless
        // it's suggested somewhere the model sees).
        let schema = spawn_agent_schema();
        let desc = schema["function"]["description"].as_str().unwrap();
        assert!(
            desc.contains("inherit_prefix"),
            "top-level description must mention inherit_prefix so \
             models discover the opportunity; got: {desc}"
        );
    }

    // ─── Lenient deserializer for inherit_prefix (MiniMax regression) ───
    //
    // MiniMax-M2.5 was observed sending `"inherit_prefix": "{}"`
    // (the empty-object shorthand as a JSON *string*) which the
    // default serde derive rejected with
    // `invalid type: string "{}", expected struct InheritPrefixSpec`,
    // failing the entire spawn_agent tool call. These tests pin the
    // lenient deserializer that accepts a handful of common model
    // "fat-fingered empty object" shapes while still rejecting
    // anything that's genuinely ambiguous.

    #[test]
    fn inherit_prefix_accepts_proper_object() {
        let input: SpawnAgentInput = serde_json::from_str(
            r#"{"description":"d","prompt":"p","inherit_prefix":{"required":true}}"#,
        )
        .unwrap();
        let spec = input.inherit_prefix.unwrap();
        assert!(spec.required);
    }

    #[test]
    fn inherit_prefix_accepts_empty_object_string() {
        // The observed MiniMax bug shape.
        let input: SpawnAgentInput =
            serde_json::from_str(r#"{"description":"d","prompt":"p","inherit_prefix":"{}"}"#)
                .unwrap();
        let spec = input
            .inherit_prefix
            .expect("\"{}\" must produce a default InheritPrefixSpec, not None");
        assert_eq!(spec.from_run_id, None);
        assert!(!spec.required, "default spec must have required=false");
    }

    #[test]
    fn inherit_prefix_accepts_empty_string() {
        let input: SpawnAgentInput =
            serde_json::from_str(r#"{"description":"d","prompt":"p","inherit_prefix":""}"#)
                .unwrap();
        assert!(input.inherit_prefix.is_some());
    }

    #[test]
    fn inherit_prefix_accepts_default_keyword() {
        // Natural-language shorthand some models emit.
        let input: SpawnAgentInput =
            serde_json::from_str(r#"{"description":"d","prompt":"p","inherit_prefix":"default"}"#)
                .unwrap();
        assert!(input.inherit_prefix.is_some());
    }

    #[test]
    fn inherit_prefix_accepts_stringified_object_with_fields() {
        // Models occasionally stringify the whole JSON object — as
        // long as it's parseable as an object, we accept.
        let input: SpawnAgentInput = serde_json::from_str(
            r#"{"description":"d","prompt":"p","inherit_prefix":"{\"required\":true}"}"#,
        )
        .unwrap();
        assert!(input.inherit_prefix.unwrap().required);
    }

    #[test]
    fn inherit_prefix_accepts_null() {
        let input: SpawnAgentInput =
            serde_json::from_str(r#"{"description":"d","prompt":"p","inherit_prefix":null}"#)
                .unwrap();
        assert!(input.inherit_prefix.is_none());
    }

    #[test]
    fn inherit_prefix_rejects_ambiguous_strings() {
        // We don't want "yes" / "true" / "on" to silently produce a
        // default — operators might intend something provider-
        // specific we don't know about. Hard-fail so they notice.
        for bad in ["yes", "on", "enabled", "1", "prefix"] {
            let json = format!(r#"{{"description":"d","prompt":"p","inherit_prefix":"{bad}"}}"#);
            let out: Result<SpawnAgentInput, _> = serde_json::from_str(&json);
            assert!(
                out.is_err(),
                "ambiguous string {bad:?} must be rejected, got {:?}",
                out.ok().and_then(|i| i.inherit_prefix)
            );
        }
    }

    #[test]
    fn inherit_prefix_rejects_numbers_arrays_bools() {
        for bad in [r#"123"#, r#"true"#, r#"[]"#] {
            let json = format!(r#"{{"description":"d","prompt":"p","inherit_prefix":{bad}}}"#);
            let out: Result<SpawnAgentInput, _> = serde_json::from_str(&json);
            assert!(
                out.is_err(),
                "type {bad} must be rejected, got parsed result"
            );
        }
    }

    #[test]
    fn schema_inherit_prefix_description_recommends_opting_in() {
        // Tripwire: the field-level description must actively
        // recommend passing `{}` when appropriate. A passive "This
        // field allows..." rewording produces near-zero uptake in
        // practice. The word "RECOMMENDED" is the dominant signal.
        let schema = spawn_agent_schema();
        let ip_desc =
            schema["function"]["parameters"]["properties"]["inherit_prefix"]["description"]
                .as_str()
                .unwrap();
        assert!(
            ip_desc.contains("RECOMMENDED"),
            "inherit_prefix description must carry an explicit \
             recommendation signal; got: {ip_desc}"
        );
        assert!(
            ip_desc.contains("{}"),
            "inherit_prefix description must show the `{{}}` opt-in \
             shorthand so models know the minimum call form; got: {ip_desc}"
        );
    }
}
