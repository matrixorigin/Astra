//! Spawn agent tool schema and types.

use serde::{Deserialize, Serialize};
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
    /// `agent(action='spawn')`" — the resolver substitutes the caller's run id
    /// at resolution time.
    #[serde(default)]
    pub from_run_id: Option<String>,

    /// Whether a missing or incompatible prefix is a hard failure.
    /// Default `false` keeps the spawn robust to eviction / TTL /
    /// feature-flag transitions.
    #[serde(default)]
    pub required: bool,
}

/// Input for `agent(action='spawn')`.
///
/// **Field order is load-bearing.** The struct is serialized to
/// JSON for the agent-spawn schema fragment and included in tool-schema
/// cache-break attribution (see `cache_diagnostics.rs::per_tool_hashes`).
/// Reordering fields changes the canonical JSON bytes and invalidates
/// every captured parent prefix across a deploy. Add new fields at
/// the end; never reorder existing ones without a coordinated
/// migration.
///
/// The caller never supplies `agent_id` here: the runtime generates the
/// child agent's id and returns it in [`SpawnAgentOutput`].
#[derive(Debug, Clone, Deserialize)]
pub struct SpawnAgentInput {
    /// Short (3-5 word) description of the task.
    pub description: String,

    /// Detailed task prompt for the agent.
    pub prompt: String,

    /// Agent type: "explore", "code-review", "task", "general-purpose".
    #[serde(default = "default_agent_type")]
    #[serde(alias = "type")]
    pub agent_type: String,

    /// Optional model override.
    pub model: Option<String>,

    /// Run in background (async). Default false — synchronous mode
    /// ensures the parent receives the child's result in the tool-call
    /// response before its turn budget is consumed.
    #[serde(default, deserialize_with = "deserialize_bool_lenient")]
    pub run_in_background: bool,

    /// Name for agent-to-agent messaging.
    pub name: Option<String>,

    /// Max turns before auto-stopping.
    #[serde(default, deserialize_with = "deserialize_option_u32_lenient")]
    pub max_turns: Option<u32>,

    /// Max output tokens for the child's first API call. When a
    /// `ForkPrefix` is inherited with thinking enabled, the
    /// resolver will refuse the inheritance if this cap would
    /// clamp the effective thinking budget below the captured
    /// parent value (cache key drift).
    #[serde(default)]
    #[serde(deserialize_with = "deserialize_option_u32_lenient")]
    pub max_output_tokens: Option<u32>,

    /// Create isolated git worktree for this agent.
    #[serde(default, deserialize_with = "deserialize_bool_lenient")]
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
    #[serde(
        default,
        alias = "inherit_context",
        deserialize_with = "deserialize_inherit_prefix_lenient"
    )]
    pub inherit_prefix: Option<InheritPrefixSpec>,

    /// Optional task-complexity hint used to scale the default
    /// turn budget when `max_turns` is not explicitly set. Accepted
    /// values: `"light"` (≈10 turns, simple one-shot queries),
    /// `"normal"` (agent-type default, the status quo), `"deep"`
    /// (2× agent-type default, for reviewing diffs across many
    /// files, multi-step refactors, or anything that would
    /// routinely exhaust the default budget).
    ///
    /// `max_turns` always wins when both are set. `complexity`
    /// exists so callers that don't want to guess a numeric budget
    /// can still pick the right shape.
    ///
    /// APPENDED at the end of the struct: earlier fields are
    /// load-bearing for schema cache; do NOT reorder.
    #[serde(default)]
    pub complexity: Option<String>,
}

/// Lenient bool deserializer: accepts `true`, `false`, `"true"`,
/// `"false"`, `"1"`, `"0"`, `1`, `0`, or null/absent (→ false).
/// LLMs frequently serialize booleans as strings (session 7e3fecb5:
/// `"run_in_background": "true"` caused 3 consecutive InvalidInput errors
/// that triggered ToolHealthTracker restriction).
fn deserialize_bool_lenient<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de;
    struct BoolVisitor;
    impl<'de> de::Visitor<'de> for BoolVisitor {
        type Value = bool;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a boolean or string-encoded boolean")
        }
        fn visit_bool<E: de::Error>(self, v: bool) -> Result<bool, E> {
            Ok(v)
        }
        fn visit_str<E: de::Error>(self, v: &str) -> Result<bool, E> {
            // Empty string is NOT a valid bool — fail loudly so LLM bugs
            // surface instead of being silently coerced to `false`.
            match v.to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" => Ok(true),
                "false" | "0" | "no" => Ok(false),
                other => Err(E::custom(format!(
                    "unrecognized boolean string: \"{other}\" (expected true/false/1/0/yes/no)"
                ))),
            }
        }
        fn visit_i64<E: de::Error>(self, v: i64) -> Result<bool, E> {
            // Accept only the canonical 0/1 contract; reject arbitrary
            // integers so malformed input doesn't silently become `true`.
            match v {
                0 => Ok(false),
                1 => Ok(true),
                other => Err(E::custom(format!(
                    "unrecognized boolean integer: {other} (expected 0 or 1)"
                ))),
            }
        }
        fn visit_u64<E: de::Error>(self, v: u64) -> Result<bool, E> {
            match v {
                0 => Ok(false),
                1 => Ok(true),
                other => Err(E::custom(format!(
                    "unrecognized boolean integer: {other} (expected 0 or 1)"
                ))),
            }
        }
        fn visit_none<E: de::Error>(self) -> Result<bool, E> {
            Ok(false)
        }
        fn visit_unit<E: de::Error>(self) -> Result<bool, E> {
            Ok(false)
        }
    }
    deserializer.deserialize_any(BoolVisitor)
}

/// Lenient optional-u32 deserializer: accepts bare integers, quoted
/// decimal strings like `"10"`, null, or absence.
///
/// Models sometimes stringify numeric inputs (`"max_turns":"10"`)
/// which default serde rejects with `invalid type: string "10",
/// expected u32`, failing the whole spawn call before the child can
/// launch. Accept only canonical non-negative integer strings; reject
/// floats, negatives, empty strings, and non-scalar types.
fn deserialize_option_u32_lenient<'de, D>(deserializer: D) -> Result<Option<u32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    let value: Option<serde_json::Value> = Option::deserialize(deserializer)?;
    let Some(value) = value else {
        return Ok(None);
    };
    match value {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::Number(number) => {
            let raw = number
                .as_u64()
                .ok_or_else(|| Error::custom("expected a non-negative integer"))?;
            let parsed = u32::try_from(raw)
                .map_err(|_| Error::custom(format!("integer out of range for u32: {raw}")))?;
            Ok(Some(parsed))
        }
        serde_json::Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                return Err(Error::custom(
                    "unrecognized integer string: \"\" (expected decimal digits)",
                ));
            }
            let raw = trimmed.parse::<u64>().map_err(|_| {
                Error::custom(format!(
                    "unrecognized integer string: \"{trimmed}\" (expected decimal digits)"
                ))
            })?;
            let parsed = u32::try_from(raw)
                .map_err(|_| Error::custom(format!("integer out of range for u32: {raw}")))?;
            Ok(Some(parsed))
        }
        serde_json::Value::Bool(_) => Err(Error::custom("expected integer; got bool")),
        serde_json::Value::Array(_) => Err(Error::custom("expected integer; got array")),
        serde_json::Value::Object(_) => Err(Error::custom("expected integer; got object")),
    }
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
            run_in_background: false,
            name: None,
            max_turns: None,
            max_output_tokens: None,
            isolated: false,
            allowed_tools: None,
            inherit_prefix: None,
            complexity: None,
        }
    }
}

/// Resolve the effective turn budget given an explicit `max_turns`,
/// an optional `complexity` hint, and the agent-type default. Rules:
///
///  * `max_turns=Some(n)` → always wins (explicit beats hint).
///  * `complexity=Some("light")` → `max(10, default / 2)`.
///  * `complexity=Some("normal")` / None → default.
///  * `complexity=Some("deep")` / `"thorough"` → `2 × default`.
///  * Unknown complexity strings fall back to default + a
///    `tracing::debug!` so operators can spot typos.
///
/// Pure function — no side effects beyond the one debug log on
/// unknown input. Callers: the spawner right after reading the
/// agent_def.
pub fn resolve_turn_budget(
    explicit_max_turns: Option<u32>,
    complexity: Option<&str>,
    default_max_turns: u32,
) -> u32 {
    if let Some(n) = explicit_max_turns {
        return n;
    }
    match complexity
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        // `light` caps at 10 turns but never raises a smaller
        // agent-type default to 10 (i.e. min-of-two). Short tasks
        // don't need more than ~10 even when the type's default is
        // 30+.
        Some("light") | Some("short") | Some("quick") => default_max_turns.min(10),
        Some("normal") | Some("default") | None | Some("") => default_max_turns,
        Some("deep") | Some("thorough") | Some("heavy") => default_max_turns.saturating_mul(2),
        Some(other) => {
            tracing::debug!(
                target: "astra_turn_core::spawn",
                complexity = other,
                "unknown complexity hint — falling back to agent-type default"
            );
            default_max_turns
        }
    }
}

#[cfg(test)]
mod budget_resolve_tests {
    use super::resolve_turn_budget;

    #[test]
    fn explicit_max_turns_wins_over_complexity() {
        assert_eq!(resolve_turn_budget(Some(7), Some("deep"), 20), 7);
    }

    #[test]
    fn complexity_light_caps_to_10() {
        // Default > 10: cap to 10.
        assert_eq!(resolve_turn_budget(None, Some("light"), 20), 10);
        assert_eq!(resolve_turn_budget(None, Some("light"), 60), 10);
        // Default < 10: keep the lower default (a small agent type
        // knows its own budget; light shouldn't inflate it).
        assert_eq!(resolve_turn_budget(None, Some("light"), 4), 4);
        assert_eq!(resolve_turn_budget(None, Some("light"), 8), 8);
    }

    #[test]
    fn complexity_deep_doubles_default() {
        assert_eq!(resolve_turn_budget(None, Some("deep"), 20), 40);
        assert_eq!(resolve_turn_budget(None, Some("thorough"), 15), 30);
    }

    #[test]
    fn complexity_normal_is_default() {
        assert_eq!(resolve_turn_budget(None, Some("normal"), 20), 20);
        assert_eq!(resolve_turn_budget(None, None, 20), 20);
        assert_eq!(resolve_turn_budget(None, Some(""), 20), 20);
    }

    #[test]
    fn unknown_complexity_falls_back_to_default() {
        assert_eq!(resolve_turn_budget(None, Some("moderate"), 20), 20);
        assert_eq!(resolve_turn_budget(None, Some("🙃"), 20), 20);
    }

    #[test]
    fn complexity_is_case_insensitive() {
        assert_eq!(resolve_turn_budget(None, Some("DEEP"), 20), 40);
        assert_eq!(resolve_turn_budget(None, Some("Deep"), 20), 40);
    }
}

fn default_agent_type() -> String {
    "general-purpose".to_string()
}

/// Output from spawn_agent tool.
///
/// Marked `#[must_use]`: every variant carries information the
/// caller MUST act on. Forgetting `Launched` leaks a background
/// agent (no one will ever call `get_result`); ignoring
/// `Completed`/`Failed` discards the agent's actual output. The
/// attribute makes the compiler nag if a spawn() return value is
/// dropped without inspection.
#[must_use = "spawning an agent without inspecting the result leaks the run \
              (Launched: caller must follow up with get_result; \
              Completed/Failed/Cancelled: caller must surface the agent's output)"]
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
    Failed { error: String, duration_ms: u64 },
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_input() {
        let json = r#"{"description": "Test", "prompt": "Do the thing"}"#;
        let input: SpawnAgentInput = serde_json::from_str(json).unwrap();
        assert_eq!(input.description, "Test");
        assert_eq!(input.agent_type, "general-purpose");
        // Default is synchronous (run_in_background=false) so the parent
        // receives the child's result in the tool-call response.
        assert!(!input.run_in_background);
        // Inheritance defaults to None — existing clients get no
        // behavior change when they don't set inherit_prefix.
        assert!(input.inherit_prefix.is_none());
        assert!(input.max_output_tokens.is_none());
    }

    #[test]
    fn agent_type_accepts_legacy_type_alias() {
        let json = r#"{"description":"Test","prompt":"Do the thing","type":"task"}"#;
        let input: SpawnAgentInput = serde_json::from_str(json).unwrap();
        assert_eq!(input.agent_type, "task");
    }

    #[test]
    fn run_in_background_default_is_false() {
        let input = SpawnAgentInput::default();
        assert!(
            !input.run_in_background,
            "run_in_background must default to false — synchronous spawn \
             ensures the parent receives the child's result before \
             its turn budget is consumed"
        );
    }

    #[test]
    fn run_in_background_true_requires_explicit_opt_in() {
        let json = r#"{"description": "D", "prompt": "P", "run_in_background": true}"#;
        let input: SpawnAgentInput = serde_json::from_str(json).unwrap();
        assert!(
            input.run_in_background,
            "explicit run_in_background: true must be honored"
        );
    }

    #[test]
    fn run_in_background_populates_canonical_field() {
        let json = r#"{"description": "D", "prompt": "P", "run_in_background": true}"#;
        let input: SpawnAgentInput = serde_json::from_str(json).unwrap();
        assert!(
            input.run_in_background,
            "run_in_background must populate the canonical field"
        );
    }

    #[test]
    fn run_in_background_false_matches_sync_default() {
        let json = r#"{"description": "D", "prompt": "P", "run_in_background": false}"#;
        let input: SpawnAgentInput = serde_json::from_str(json).unwrap();
        assert!(
            !input.run_in_background,
            "run_in_background: false must produce the sync-default spawn"
        );
    }

    #[test]
    fn legacy_task_field_is_rejected() {
        let json = r#"{"description":"D","task":"Use the old field"}"#;
        let err = serde_json::from_str::<SpawnAgentInput>(json)
            .expect_err("deprecated task field must not deserialize");
        assert!(
            err.to_string().contains("missing field `prompt`"),
            "legacy task payloads should fail because prompt is the only canonical field: {err}"
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
    fn inherit_context_alias_populates_inherit_prefix() {
        let json = r#"{
            "description": "D",
            "prompt": "P",
            "inherit_context": {"required": true}
        }"#;
        let input: SpawnAgentInput = serde_json::from_str(json).unwrap();
        let spec = input
            .inherit_prefix
            .expect("inherit_context must opt into prefix inheritance");
        assert!(spec.required);
        assert_eq!(spec.from_run_id, None);
    }

    #[test]
    fn inherit_context_conflicts_with_inherit_prefix() {
        let err = serde_json::from_str::<SpawnAgentInput>(
            r#"{
                "description": "D",
                "prompt": "P",
                "inherit_prefix": {"required": false},
                "inherit_context": {"required": true}
            }"#,
        )
        .expect_err("ambiguous dual inheritance fields must fail");
        assert!(
            err.to_string().contains("duplicate field"),
            "error should reject conflicting inheritance aliases as duplicate fields, got: {err}"
        );
    }

    #[test]
    fn inherit_context_alias_accepts_empty_object_string() {
        let input: SpawnAgentInput =
            serde_json::from_str(r#"{"description":"d","prompt":"p","inherit_context":"{}"}"#)
                .unwrap();
        let spec = input
            .inherit_prefix
            .expect("inherit_context alias should use the same lenient parser");
        assert_eq!(spec.from_run_id, None);
        assert!(!spec.required);
    }

    #[test]
    fn inherit_context_alias_accepts_null() {
        let input: SpawnAgentInput =
            serde_json::from_str(r#"{"description":"d","prompt":"p","inherit_context":null}"#)
                .unwrap();
        assert!(
            input.inherit_prefix.is_none(),
            "null alias should behave like null inherit_prefix"
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
}

#[cfg(test)]
mod bool_lenient_tests {
    use super::SpawnAgentInput;

    #[test]
    fn run_in_background_accepts_string_true() {
        // Regression (session 7e3fecb5): LLM passed "true" (string)
        // instead of true (bool) → serde rejected → 3 failures →
        // tool restricted. Lenient deserializer fixes this.
        let input: SpawnAgentInput = serde_json::from_str(
            r#"{"description":"test","prompt":"p","run_in_background":"true"}"#,
        )
        .expect("string 'true' must deserialize");
        assert!(input.run_in_background);
    }

    #[test]
    fn run_in_background_accepts_bool_true() {
        let input: SpawnAgentInput =
            serde_json::from_str(r#"{"description":"test","prompt":"p","run_in_background":true}"#)
                .expect("bool true must deserialize");
        assert!(input.run_in_background);
    }

    #[test]
    fn run_in_background_defaults_false_on_absence() {
        let input: SpawnAgentInput = serde_json::from_str(r#"{"description":"test","prompt":"p"}"#)
            .expect("absent run_in_background must default to false");
        assert!(!input.run_in_background);
    }

    #[test]
    fn isolated_accepts_string_false() {
        let input: SpawnAgentInput =
            serde_json::from_str(r#"{"description":"test","prompt":"p","isolated":"false"}"#)
                .expect("string 'false' must deserialize");
        assert!(!input.isolated);
    }

    #[test]
    fn run_in_background_rejects_unknown_string() {
        // Contract: only {true,false,1,0,yes,no} are accepted. Anything
        // else must error instead of silently defaulting to false — so
        // genuine LLM bugs surface rather than masquerading as success.
        let err = serde_json::from_str::<SpawnAgentInput>(
            r#"{"description":"test","prompt":"p","run_in_background":"maybe"}"#,
        )
        .expect_err("unknown string must be rejected");
        assert!(
            err.to_string().contains("unrecognized boolean string"),
            "error must name the contract violation; got: {err}"
        );
    }

    #[test]
    fn run_in_background_rejects_empty_string() {
        // Regression guard: empty string used to coerce to `false`,
        // hiding malformed `"run_in_background": ""` input. Now must error.
        let err = serde_json::from_str::<SpawnAgentInput>(
            r#"{"description":"test","prompt":"p","run_in_background":""}"#,
        )
        .expect_err("empty string must be rejected");
        assert!(err.to_string().contains("unrecognized boolean string"));
    }

    #[test]
    fn run_in_background_rejects_arbitrary_integer() {
        // Contract: only 0/1 are accepted. `42` used to silently
        // coerce to `true`; now must error.
        let err = serde_json::from_str::<SpawnAgentInput>(
            r#"{"description":"test","prompt":"p","run_in_background":42}"#,
        )
        .expect_err("arbitrary integer must be rejected");
        assert!(
            err.to_string().contains("unrecognized boolean integer"),
            "error must name the contract violation; got: {err}"
        );
    }

    #[test]
    fn run_in_background_accepts_integer_one() {
        let input: SpawnAgentInput =
            serde_json::from_str(r#"{"description":"test","prompt":"p","run_in_background":1}"#)
                .expect("integer 1 must deserialize to true");
        assert!(input.run_in_background);
    }
}

#[cfg(test)]
mod u32_lenient_tests {
    use super::SpawnAgentInput;

    #[test]
    fn max_turns_accepts_string_integer() {
        let input: SpawnAgentInput =
            serde_json::from_str(r#"{"description":"test","prompt":"p","max_turns":"10"}"#)
                .expect("string '10' must deserialize");
        assert_eq!(input.max_turns, Some(10));
    }

    #[test]
    fn max_output_tokens_accepts_string_integer() {
        let input: SpawnAgentInput = serde_json::from_str(
            r#"{"description":"test","prompt":"p","max_output_tokens":"8000"}"#,
        )
        .expect("string max_output_tokens must deserialize");
        assert_eq!(input.max_output_tokens, Some(8000));
    }

    #[test]
    fn max_turns_rejects_empty_string() {
        let err = serde_json::from_str::<SpawnAgentInput>(
            r#"{"description":"test","prompt":"p","max_turns":""}"#,
        )
        .expect_err("empty string must not silently coerce");
        assert!(err.to_string().contains("unrecognized integer string"));
    }

    #[test]
    fn max_turns_rejects_float_string() {
        let err = serde_json::from_str::<SpawnAgentInput>(
            r#"{"description":"test","prompt":"p","max_turns":"10.5"}"#,
        )
        .expect_err("float string must not deserialize as u32");
        assert!(err.to_string().contains("unrecognized integer string"));
    }
}
