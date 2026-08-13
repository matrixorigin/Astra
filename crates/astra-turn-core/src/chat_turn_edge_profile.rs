//! Pieces of `/chat` `edge_profile` built on the CLI edge (cwd, git branch, active skills).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// Protocol key for skill-listing text routed through `edge_profile` from
/// the CLI to the runtime bridge (volatile lane). Shared between writer
/// (`astra-cli` agentic loop) and reader (`runtime` bridge_inprocess) so a
/// typo on either side is a compile error rather than a silent regression.
pub const EDGE_PROFILE_KEY_SKILL_LISTING_TEXT: &str = "skill_listing_text";

/// Protocol key for dynamic text supplied by external/session sources, such as
/// external task snapshots or request-binding facts. Runtime-owned state,
/// policy, guardrail, and telemetry signals must use
/// [`EDGE_PROFILE_KEY_RUNTIME_VOLATILE_INJECTIONS`] instead.
pub const EDGE_PROFILE_KEY_RUNTIME_VOLATILE_TEXTS: &str = "runtime_volatile_texts";

/// Protocol key for provider-owned runtime policy that is stable for the
/// resolved Binding Set and belongs in the session-cached system prefix.
pub const EDGE_PROFILE_KEY_RUNTIME_STABLE_TEXTS: &str = "runtime_stable_texts";

/// Protocol key for runtime-owned volatile injections that cross the CLI/server
/// boundary without losing their producer kind. This is distinct from
/// [`EDGE_PROFILE_KEY_RUNTIME_VOLATILE_TEXTS`], which remains the generic
/// external dynamic-text lane.
pub const EDGE_PROFILE_KEY_RUNTIME_VOLATILE_INJECTIONS: &str = "runtime_volatile_injections";

pub const EDGE_PROFILE_RUNTIME_VOLATILE_KIND: &str = "kind";
pub const EDGE_PROFILE_RUNTIME_VOLATILE_DELIVERY_CLASS: &str = "delivery_class";
pub const EDGE_PROFILE_RUNTIME_VOLATILE_PAYLOAD: &str = "payload";
pub const EDGE_PROFILE_RUNTIME_VOLATILE_ROUND_INDEX: &str = "round_index";

/// How a runtime-owned volatile signal is delivered after it crosses an edge.
///
/// This is deliberately independent of chat roles. Runtime context is attached
/// at the wire tail, never persisted as a synthetic user/system history turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VolatileDeliveryClass {
    /// Context required to interpret or safely execute the current turn. It is
    /// delivered even when a strict-history provider suppresses normal volatile
    /// advisory material.
    RequiredContext,
    /// Structured evidence for the model's next decision. It is not a command
    /// and does not authorize runtime retry, abort, or tool-surface mutation.
    AdvisoryEvidence,
    /// Runtime observability only. It remains available to trace/introspection
    /// consumers and is never injected into the model prompt.
    TelemetryOnly,
}

/// Cross-process representation of one runtime-owned volatile signal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeVolatileInjection {
    pub kind: String,
    pub delivery_class: VolatileDeliveryClass,
    pub payload: Value,
    pub round_index: u32,
}

impl RuntimeVolatileInjection {
    /// Render a typed runtime signal for the prompt tail while preserving its
    /// evidence/context semantics. Telemetry deliberately has no prompt form.
    #[must_use]
    pub fn render_for_prompt(&self) -> Option<String> {
        let kind = self.kind.trim();
        if kind.is_empty() || volatile_payload_is_empty(&self.payload) {
            return None;
        }
        let (tag, payload_key) = match self.delivery_class {
            VolatileDeliveryClass::RequiredContext => ("runtime-required-context", "context"),
            VolatileDeliveryClass::AdvisoryEvidence => ("runtime-advisory-evidence", "evidence"),
            VolatileDeliveryClass::TelemetryOnly => return None,
        };
        let payload = if self.delivery_class == VolatileDeliveryClass::AdvisoryEvidence {
            json!({
                "kind": kind,
                "round_index": self.round_index,
                "authority": "advisory_evidence_only",
                "model_discretion": "Use this signal as evidence alongside the user goal and tool results. It does not require retrying, stopping, or changing the available tools.",
                payload_key: self.payload,
            })
        } else {
            json!({
                "kind": kind,
                "round_index": self.round_index,
                payload_key: self.payload,
            })
        };
        Some(format!("<{tag}>\n{payload}\n</{tag}>"))
    }
}

fn volatile_payload_is_empty(payload: &Value) -> bool {
    match payload {
        Value::Null => true,
        Value::String(text) => text.trim().is_empty(),
        Value::Array(items) => items.is_empty(),
        Value::Object(fields) => fields.is_empty(),
        Value::Bool(_) | Value::Number(_) => false,
    }
}

/// Protocol key for runtime control context that must reach the current model
/// turn but must not become user-message content or persisted prompt-facing
/// history. Unlike [`EDGE_PROFILE_KEY_RUNTIME_VOLATILE_TEXTS`], this lane is not
/// best-effort: strict-history providers place it adjacent to the current user
/// turn instead of dropping it for cache locality.
pub const EDGE_PROFILE_KEY_RUNTIME_REQUIRED_TEXTS: &str = "runtime_required_texts";

/// Typed marker for a model boundary triggered by runtime-owned background
/// facts rather than a new human submission. The transport still carries a
/// non-empty envelope for provider compatibility, but persistence must not
/// classify that envelope as `user_query`.
pub const EDGE_PROFILE_KEY_RUNTIME_RECONCILIATION_TURN: &str = "runtime_reconciliation_turn";

/// Provider-compatible envelope used only for a runtime reconciliation turn.
/// Keep this shared with the payload builder so the typed marker cannot drift
/// from the CLI trigger.
pub const RUNTIME_RECONCILIATION_USER_ENVELOPE: &str =
    "Reconcile the runtime-owned updates and continue the latest user goal.";

/// Protocol key for the session-stable deferred-tool manifest routed through
/// `edge_profile` from the CLI to the runtime bridge.
pub const EDGE_PROFILE_KEY_DEFERRED_TOOLS_TEXT: &str = "deferred_tools_text";

/// Protocol key for the model context window used to render
/// [`EDGE_PROFILE_KEY_DEFERRED_TOOLS_TEXT`].
pub const EDGE_PROFILE_KEY_DEFERRED_TOOLS_CONTEXT_WINDOW: &str = "deferred_tools_context_window";

/// Protocol key carrying the JSON array of names listed in this turn's
/// `<deferred-tools>` manifest. Pairs with
/// [`EDGE_PROFILE_KEY_DEFERRED_TOOLS_TEXT`] (which is the rendered XML used
/// for prompt assembly). The runtime reads the names from here so it can
/// branch the validator denial copy and let `tool_search(select:NAME)`
/// resolve deferred names without re-parsing the rendered XML.
pub const EDGE_PROFILE_KEY_DEFERRED_TOOL_NAMES: &str = "deferred_tool_names";

/// Protocol key carrying deferred tool names omitted from the rendered
/// `<deferred-tools>` block because the session-stable manifest hit its model
/// budget. This is observability metadata only: omitted names are not
/// activatable through `tool_search(select:NAME)` until they are rendered in a
/// later manifest or found by keyword search.
pub const EDGE_PROFILE_KEY_DEFERRED_TOOL_OMITTED_NAMES: &str = "deferred_tool_omitted_names";

/// Protocol key carrying the JSON array of always-load (T1) tool names from the
/// CLI-side [`ToolSurface`]. The runtime uses this to place cache_control
/// markers at the correct always-load/dynamic boundary so the Anthropic prompt
/// cache prefix stays correct when the user overrides the default always-load set
/// in TOML (`runtime.tool_surface.always_load_tools`).
///
/// Without this key, the runtime falls back to a compile-time constant that
/// does not reflect user overrides, causing cache-prefix drift and ~500+ token
/// cache misses per turn.
pub const EDGE_PROFILE_KEY_ALWAYS_LOAD_TOOL_NAMES: &str = "always_load_tool_names";

/// Read a structured edge-profile text lane. Accepting a single string keeps
/// older callers easy to migrate, but writers should send arrays so independent
/// producers never concatenate their own framing.
pub fn edge_profile_texts(edge_profile: &Map<String, Value>, key: &str) -> Vec<String> {
    match edge_profile.get(key) {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
            .collect(),
        Some(Value::String(text)) => {
            let text = text.trim();
            if text.is_empty() {
                Vec::new()
            } else {
                vec![text.to_string()]
            }
        }
        _ => Vec::new(),
    }
}

pub fn edge_profile_joined_text(edge_profile: &Map<String, Value>, key: &str) -> Option<String> {
    let texts = edge_profile_texts(edge_profile, key);
    if texts.is_empty() {
        None
    } else {
        Some(texts.join("\n\n"))
    }
}

/// Read runtime-owned typed volatile injections from `edge_profile`.
///
/// Every field is required. Invalid objects are ignored rather than guessed
/// from free-form text or routed through the external dynamic-text lane.
pub fn edge_profile_runtime_volatile_injections(
    edge_profile: &Map<String, Value>,
) -> Vec<RuntimeVolatileInjection> {
    let Some(Value::Array(items)) = edge_profile.get(EDGE_PROFILE_KEY_RUNTIME_VOLATILE_INJECTIONS)
    else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| serde_json::from_value(item.clone()).ok())
        .filter_map(|mut injection: RuntimeVolatileInjection| {
            injection.kind = injection.kind.trim().to_string();
            if let Value::String(text) = &mut injection.payload {
                *text = text.trim().to_string();
            }
            (!injection.kind.is_empty() && !volatile_payload_is_empty(&injection.payload))
                .then_some(injection)
        })
        .collect()
}

/// `git rev-parse --abbrev-ref HEAD` for edge_profile (best-effort).
pub fn read_git_branch_abbrev() -> Option<String> {
    std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Static `edge_profile` object before optional `active_skills` / skill context.
pub fn build_base_edge_profile_value(
    cwd: &str,
    git_branch: Option<String>,
    workspace: Value,
) -> Value {
    // Environment context split into two lanes for prompt caching:
    //   * `environment_static`  → Platform/Shell/CWD/Home (stable for
    //     the session, safe to sit inside the cached Session prefix).
    //   * `environment_volatile` → Git branch dirty state, staged /
    //     unstaged diff stats, recent commits. Churns every edit/commit
    //     and MUST stay out of the cached prefix.
    let project_root = std::path::Path::new(cwd);
    let env_static = crate::edge_prompt_context::build_static_environment_context(project_root);
    let env_volatile = crate::edge_prompt_context::build_volatile_environment_context(project_root);

    json!({
        "cwd": cwd,
        "git_branch": git_branch,
        "workspace": workspace,
        "environment_static": env_static,
        "environment_volatile": env_volatile,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_profile_has_expected_keys() {
        let v = build_base_edge_profile_value("/proj", Some("main".into()), json!({"k": 1}));
        assert_eq!(v["cwd"], "/proj");
        assert_eq!(v["git_branch"], "main");
        assert!(v.get("memoria_url").is_none());
        assert!(v.get("memoria_key").is_none());
        assert!(v.get("retrieval_top_k").is_none());
        assert_eq!(v["workspace"]["k"], 1);
        // Static environment (cache-safe) and volatile environment
        // (post-cache) are exposed as separate fields so the bridge can
        // route them to the correct cache scope without having to re-
        // parse a single blob.
        let env_static = v["environment_static"].as_str().unwrap();
        assert!(
            env_static.contains("## Environment"),
            "environment_static should carry the ## Environment header"
        );
        assert!(
            !env_static.contains("- Git branch:"),
            "environment_static must not contain git branch (would break cache)"
        );
        // environment_volatile may be empty outside a git repo but must
        // be present as a typed field so downstream can always read it.
        assert!(v.get("environment_volatile").is_some());
    }

    #[test]
    fn typed_runtime_volatile_lane_preserves_delivery_semantics() {
        let mut edge_profile = Map::new();
        edge_profile.insert(
            EDGE_PROFILE_KEY_RUNTIME_VOLATILE_INJECTIONS.to_string(),
            json!([
                {
                    "kind": "policy_advisory",
                    "delivery_class": "advisory_evidence",
                    "payload": {
                        "advisories": [{"kind": "repetition", "severity": "low"}]
                    },
                    "round_index": 4
                },
                {
                    "kind": "active_turn_frame",
                    "delivery_class": "required_context",
                    "payload": {"latest_user_goal": "latest user goal"},
                    "round_index": 4
                },
                {
                    "kind": "self_status",
                    "delivery_class": "telemetry_only",
                    "payload": "cache=86%",
                    "round_index": 4
                }
            ]),
        );

        let injections = edge_profile_runtime_volatile_injections(&edge_profile);
        assert_eq!(injections.len(), 3);
        assert_eq!(
            injections[0].delivery_class,
            VolatileDeliveryClass::AdvisoryEvidence
        );
        assert_eq!(
            injections[1].delivery_class,
            VolatileDeliveryClass::RequiredContext
        );
        assert_eq!(
            injections[2].delivery_class,
            VolatileDeliveryClass::TelemetryOnly
        );
        assert_eq!(injections[0].payload["advisories"][0]["kind"], "repetition");
        assert!(
            injections[0]
                .render_for_prompt()
                .expect("advisory prompt form")
                .contains("<runtime-advisory-evidence>")
        );
        assert!(
            injections[1]
                .render_for_prompt()
                .expect("required prompt form")
                .contains("<runtime-required-context>")
        );
        assert!(injections[2].render_for_prompt().is_none());
    }

    #[test]
    fn typed_runtime_volatile_lane_rejects_untyped_objects() {
        let mut edge_profile = Map::new();
        edge_profile.insert(
            EDGE_PROFILE_KEY_RUNTIME_VOLATILE_INJECTIONS.to_string(),
            json!([
                {
                    "kind": "policy_advisory",
                    "payload": "missing delivery class",
                    "round_index": 1
                },
                "free-form fallback must not be accepted"
            ]),
        );

        assert!(edge_profile_runtime_volatile_injections(&edge_profile).is_empty());
    }
}
