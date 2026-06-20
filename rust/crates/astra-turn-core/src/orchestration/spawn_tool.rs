//! Spawn agent tool schema and types.

use super::fanout_group::AgentFanoutSlotIdentity;
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
    pub run_in_background: bool,

    /// Name for agent-to-agent messaging.
    pub name: Option<String>,

    /// Max turns before auto-stopping.
    #[serde(default)]
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
    #[serde(default, deserialize_with = "deserialize_inherit_prefix_strict")]
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

    /// Optional fanout group identity. When present with
    /// `fanout_target_count` and `fanout_slot_index`, this spawn is a
    /// specific slot in a fixed-size group rather than an independent
    /// child. The runtime uses it to preserve the user's requested N
    /// slots across spawn failures, retries, cancellation, and result
    /// collection.
    #[serde(default)]
    pub fanout_group_id: Option<String>,

    /// Optional user-facing title for the fanout group. This does not
    /// participate in slot identity; it lets UI projections render the
    /// user's group label instead of falling back to the group id.
    #[serde(default)]
    pub fanout_group_title: Option<String>,

    /// Fixed target count for the fanout group. Must be >= 1 when
    /// provided by callers.
    #[serde(default)]
    pub fanout_target_count: Option<usize>,

    /// Zero-based slot index within the fanout group. The slot index
    /// identifies replacement/retry intent; a later retry for slot 1
    /// must not silently become a fourth requested agent.
    #[serde(default)]
    pub fanout_slot_index: Option<usize>,

    /// Optional stable caller-facing label for the fanout slot. This is
    /// not the runtime-generated `agent_id`; it exists so callers can
    /// correlate start/results/status projections without inventing
    /// extra top-level fields.
    #[serde(default)]
    pub fanout_slot_id: Option<String>,
}

impl SpawnAgentInput {
    pub fn validate_fanout_metadata(&self) -> Result<(), String> {
        self.fanout_slot_identity().map(|_| ())
    }

    pub fn fanout_slot_identity(&self) -> Result<Option<AgentFanoutSlotIdentity>, String> {
        let any = self.fanout_group_id.is_some()
            || self.fanout_group_title.is_some()
            || self.fanout_target_count.is_some()
            || self.fanout_slot_index.is_some()
            || self.fanout_slot_id.is_some();
        if !any {
            return Ok(None);
        }
        if self
            .fanout_group_title
            .as_deref()
            .is_some_and(|title| title.trim().is_empty())
        {
            return Err("fanout metadata requires non-empty fanout_group_title".to_string());
        }
        let group_id = self
            .fanout_group_id
            .as_deref()
            .map(str::trim)
            .ok_or_else(|| "fanout metadata requires non-empty fanout_group_id".to_string())?;
        let target_count = self
            .fanout_target_count
            .ok_or_else(|| format!("fanout group '{group_id}' requires fanout_target_count"))?;
        let slot_index = self
            .fanout_slot_index
            .ok_or_else(|| format!("fanout group '{group_id}' requires fanout_slot_index"))?;
        AgentFanoutSlotIdentity::new(
            group_id,
            target_count,
            slot_index,
            self.fanout_slot_id.clone(),
        )
        .map(Some)
    }
}

fn deserialize_inherit_prefix_strict<'de, D>(
    deserializer: D,
) -> Result<Option<InheritPrefixSpec>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::Object(_) => serde_json::from_value::<InheritPrefixSpec>(value)
            .map(Some)
            .map_err(Error::custom),
        serde_json::Value::String(_) => Err(Error::custom(
            "inherit_prefix must be an object or null; got string",
        )),
        serde_json::Value::Bool(_) => Err(Error::custom(
            "inherit_prefix must be an object or null; got bool",
        )),
        serde_json::Value::Number(_) => Err(Error::custom(
            "inherit_prefix must be an object or null; got number",
        )),
        serde_json::Value::Array(_) => Err(Error::custom(
            "inherit_prefix must be an object or null; got array",
        )),
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
            fanout_group_id: None,
            fanout_group_title: None,
            fanout_target_count: None,
            fanout_slot_index: None,
            fanout_slot_id: None,
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
              Completed/Interrupted/Failed/Cancelled: caller must surface the agent's output)"]
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
    /// Agent produced partial output but stopped before normal completion.
    Interrupted {
        agent_id: String,
        result: String,
        finish_reason: String,
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
    Failed {
        error: String,
        finish_reason: String,
        duration_ms: u64,
    },
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
    use serde_json::json;

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
    fn agent_type_rejects_type_alias() {
        let json = r#"{"description":"Test","prompt":"Do the thing","type":"task"}"#;
        let err = serde_json::from_str::<SpawnAgentInput>(json)
            .expect_err("type alias must not deserialize");
        assert!(
            err.to_string().contains("unknown field `type`"),
            "canonical field is agent_type; got: {err}"
        );
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
    fn fanout_metadata_round_trips_explicit_slot_identity() {
        let json = r#"{
            "description": "Review storage",
            "prompt": "Review storage layer",
            "run_in_background": true,
            "fanout_group_id": "review-1",
            "fanout_group_title": "Review fanout",
            "fanout_target_count": 3,
            "fanout_slot_index": 1,
            "fanout_slot_id": "storage"
        }"#;
        let input: SpawnAgentInput = serde_json::from_str(json).unwrap();
        input.validate_fanout_metadata().unwrap();
        assert_eq!(input.fanout_group_id.as_deref(), Some("review-1"));
        assert_eq!(input.fanout_group_title.as_deref(), Some("Review fanout"));
        assert_eq!(input.fanout_target_count, Some(3));
        assert_eq!(input.fanout_slot_index, Some(1));
        assert_eq!(input.fanout_slot_id.as_deref(), Some("storage"));
        assert_eq!(
            input
                .fanout_slot_identity()
                .unwrap()
                .unwrap()
                .slot_id
                .as_deref(),
            Some("storage")
        );
    }

    #[test]
    fn fanout_metadata_rejects_partial_or_out_of_range_identity() {
        let title_without_identity: SpawnAgentInput = serde_json::from_str(
            r#"{
                "description": "Review storage",
                "prompt": "Review storage layer",
                "fanout_group_title": "Review fanout"
            }"#,
        )
        .unwrap();
        let err = title_without_identity
            .validate_fanout_metadata()
            .unwrap_err();
        assert!(err.contains("fanout_group_id"), "{err}");

        let empty_title: SpawnAgentInput = serde_json::from_str(
            r#"{
                "description": "Review storage",
                "prompt": "Review storage layer",
                "fanout_group_id": "review-1",
                "fanout_group_title": "   ",
                "fanout_target_count": 3,
                "fanout_slot_index": 1
            }"#,
        )
        .unwrap();
        let err = empty_title.validate_fanout_metadata().unwrap_err();
        assert!(err.contains("fanout_group_title"), "{err}");

        let missing_slot: SpawnAgentInput = serde_json::from_str(
            r#"{
                "description": "Review storage",
                "prompt": "Review storage layer",
                "fanout_group_id": "review-1",
                "fanout_target_count": 3
            }"#,
        )
        .unwrap();
        let err = missing_slot.validate_fanout_metadata().unwrap_err();
        assert!(err.contains("fanout_slot_index"), "{err}");

        let out_of_range: SpawnAgentInput = serde_json::from_str(
            r#"{
                "description": "Review storage",
                "prompt": "Review storage layer",
                "fanout_group_id": "review-1",
                "fanout_target_count": 3,
                "fanout_slot_index": 3
            }"#,
        )
        .unwrap();
        let err = out_of_range.validate_fanout_metadata().unwrap_err();
        assert!(err.contains("outside target_count"), "{err}");

        let empty_slot_id: SpawnAgentInput = serde_json::from_str(
            r#"{
                "description": "Review storage",
                "prompt": "Review storage layer",
                "fanout_group_id": "review-1",
                "fanout_target_count": 3,
                "fanout_slot_index": 1,
                "fanout_slot_id": "   "
            }"#,
        )
        .unwrap();
        let err = empty_slot_id.validate_fanout_metadata().unwrap_err();
        assert!(err.contains("fanout_slot_id"), "{err}");
    }

    #[test]
    fn legacy_task_field_is_rejected() {
        let json = r#"{"description":"D","task":"Use the old field"}"#;
        let err = serde_json::from_str::<SpawnAgentInput>(json)
            .expect_err("deprecated task field must not deserialize");
        assert!(
            err.to_string().contains("unknown field `task`"),
            "legacy task payloads should fail because prompt is the only canonical field and task is not accepted: {err}"
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
    fn inherit_context_alias_is_rejected() {
        let json = r#"{
            "description": "D",
            "prompt": "P",
            "inherit_context": {"required": true}
        }"#;
        let err = serde_json::from_str::<SpawnAgentInput>(json)
            .expect_err("inherit_context alias must not deserialize");
        assert!(
            err.to_string().contains("unknown field `inherit_context`"),
            "canonical field is inherit_prefix; got: {err}"
        );
    }

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
    fn inherit_prefix_rejects_empty_object_string() {
        let err = serde_json::from_str::<SpawnAgentInput>(
            r#"{"description":"d","prompt":"p","inherit_prefix":"{}"}"#,
        )
        .expect_err("inherit_prefix must be an object, not a string");
        assert!(err.to_string().contains("got string"), "{err}");
    }

    #[test]
    fn inherit_prefix_rejects_empty_string() {
        let err = serde_json::from_str::<SpawnAgentInput>(
            r#"{"description":"d","prompt":"p","inherit_prefix":""}"#,
        )
        .expect_err("inherit_prefix must be an object, not a string");
        assert!(err.to_string().contains("got string"), "{err}");
    }

    #[test]
    fn inherit_prefix_rejects_default_keyword() {
        let err = serde_json::from_str::<SpawnAgentInput>(
            r#"{"description":"d","prompt":"p","inherit_prefix":"default"}"#,
        )
        .expect_err("inherit_prefix must be an object, not a string");
        assert!(err.to_string().contains("got string"), "{err}");
    }

    #[test]
    fn inherit_prefix_rejects_stringified_object_with_fields() {
        let err = serde_json::from_str::<SpawnAgentInput>(
            r#"{"description":"d","prompt":"p","inherit_prefix":"{\"required\":true}"}"#,
        )
        .expect_err("inherit_prefix must be an object, not a stringified object");
        assert!(err.to_string().contains("got string"), "{err}");
    }

    #[test]
    fn inherit_prefix_rejects_unknown_object_fields() {
        let err = serde_json::from_str::<SpawnAgentInput>(
            r#"{"description":"d","prompt":"p","inherit_prefix":{"mode":"default"}}"#,
        )
        .expect_err("inherit_prefix object must reject unknown fields");
        assert!(err.to_string().contains("unknown field `mode`"), "{err}");
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
    fn interrupted_spawn_output_serializes_as_distinct_wire_status() {
        let value = serde_json::to_value(SpawnAgentOutput::Interrupted {
            agent_id: "reviewer@abc123".to_string(),
            result: "partial findings".to_string(),
            finish_reason: "budget_exhausted".to_string(),
            tool_calls: 3,
            duration_ms: 1250,
        })
        .unwrap();

        assert_eq!(
            value,
            json!({
                "status": "interrupted",
                "agent_id": "reviewer@abc123",
                "result": "partial findings",
                "finish_reason": "budget_exhausted",
                "tool_calls": 3,
                "duration_ms": 1250
            })
        );
    }
}

#[cfg(test)]
mod strict_type_tests {
    use super::SpawnAgentInput;

    #[test]
    fn run_in_background_rejects_string_true() {
        let err = serde_json::from_str::<SpawnAgentInput>(
            r#"{"description":"test","prompt":"p","run_in_background":"true"}"#,
        )
        .expect_err("string true must not deserialize");
        assert!(err.to_string().contains("expected a boolean"), "{err}");
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
    fn isolated_rejects_string_false() {
        let err = serde_json::from_str::<SpawnAgentInput>(
            r#"{"description":"test","prompt":"p","isolated":"false"}"#,
        )
        .expect_err("string false must not deserialize");
        assert!(err.to_string().contains("expected a boolean"), "{err}");
    }

    #[test]
    fn run_in_background_rejects_unknown_string() {
        let err = serde_json::from_str::<SpawnAgentInput>(
            r#"{"description":"test","prompt":"p","run_in_background":"maybe"}"#,
        )
        .expect_err("unknown string must be rejected");
        assert!(err.to_string().contains("expected a boolean"), "{err}");
    }

    #[test]
    fn run_in_background_rejects_empty_string() {
        let err = serde_json::from_str::<SpawnAgentInput>(
            r#"{"description":"test","prompt":"p","run_in_background":""}"#,
        )
        .expect_err("empty string must be rejected");
        assert!(err.to_string().contains("expected a boolean"), "{err}");
    }

    #[test]
    fn run_in_background_rejects_arbitrary_integer() {
        let err = serde_json::from_str::<SpawnAgentInput>(
            r#"{"description":"test","prompt":"p","run_in_background":42}"#,
        )
        .expect_err("arbitrary integer must be rejected");
        assert!(err.to_string().contains("expected a boolean"), "{err}");
    }

    #[test]
    fn run_in_background_rejects_integer_one() {
        let err = serde_json::from_str::<SpawnAgentInput>(
            r#"{"description":"test","prompt":"p","run_in_background":1}"#,
        )
        .expect_err("integer 1 must not deserialize");
        assert!(err.to_string().contains("expected a boolean"), "{err}");
    }

    #[test]
    fn max_turns_rejects_string_integer() {
        let err = serde_json::from_str::<SpawnAgentInput>(
            r#"{"description":"test","prompt":"p","max_turns":"10"}"#,
        )
        .expect_err("string max_turns must not deserialize");
        assert!(err.to_string().contains("expected u32"), "{err}");
    }

    #[test]
    fn max_output_tokens_rejects_string_integer() {
        let err = serde_json::from_str::<SpawnAgentInput>(
            r#"{"description":"test","prompt":"p","max_output_tokens":"8000"}"#,
        )
        .expect_err("string max_output_tokens must not deserialize");
        assert!(err.to_string().contains("expected u32"), "{err}");
    }

    #[test]
    fn max_turns_rejects_empty_string() {
        let err = serde_json::from_str::<SpawnAgentInput>(
            r#"{"description":"test","prompt":"p","max_turns":""}"#,
        )
        .expect_err("empty string must not silently coerce");
        assert!(err.to_string().contains("expected u32"), "{err}");
    }

    #[test]
    fn max_turns_rejects_float_string() {
        let err = serde_json::from_str::<SpawnAgentInput>(
            r#"{"description":"test","prompt":"p","max_turns":"10.5"}"#,
        )
        .expect_err("float string must not deserialize as u32");
        assert!(err.to_string().contains("expected u32"), "{err}");
    }

    #[test]
    fn fanout_target_count_rejects_string_integer() {
        let err = serde_json::from_str::<SpawnAgentInput>(
            r#"{"description":"test","prompt":"p","fanout_group_id":"g","fanout_target_count":"3","fanout_slot_index":0}"#,
        )
        .expect_err("string fanout_target_count must not deserialize");
        assert!(err.to_string().contains("expected usize"), "{err}");
    }
}
