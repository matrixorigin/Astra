//! Shared interaction-mode types extracted from `agentic_loop_host`.
//!
//! These live in turn-core so that modules like `chat_turn_heuristics` and
//! `stop_hooks_yaml` can reference them without depending on the full runtime.

use serde_json::Value;
use std::collections::HashSet;

use crate::tool::registry::meta::{IntentType, tool_meta};

/// Canonical name of the ask-user tool.
pub const ASK_USER_TOOL_NAME: &str = "ask_user";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TurnInteractionMode {
    #[default]
    NonInteractive,
    Prompt,
    Auto,
    Deny,
    Headless,
}

impl TurnInteractionMode {
    #[must_use]
    pub fn allows_ask_user(self) -> bool {
        matches!(self, Self::Prompt)
    }

    #[must_use]
    pub fn can_pause_for_user(self) -> bool {
        matches!(self, Self::Prompt)
    }

    /// Whether advisory policy feedback should also be rendered as a
    /// user-facing status line.
    ///
    /// Auto mode keeps the policy-to-model feedback lane active, but avoids
    /// turning each advisory into visible UI chatter. Permission automation
    /// and feedback delivery are independent concerns.
    #[must_use]
    pub fn shows_policy_feedback_status(self) -> bool {
        !matches!(self, Self::Auto)
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::NonInteractive => "non_interactive",
            Self::Prompt => "prompt",
            Self::Auto => "auto",
            Self::Deny => "deny",
            Self::Headless => "headless",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnInteractionPolicy {
    pub mode: TurnInteractionMode,
    pub visible_tool_names: Vec<String>,
    pub observation_tool_names: Vec<String>,
    pub can_pause_for_user: bool,
    pub allow_ask_user: bool,
}

impl Default for TurnInteractionPolicy {
    fn default() -> Self {
        Self::from_visible_tool_names(TurnInteractionMode::NonInteractive, Vec::new())
    }
}

impl TurnInteractionPolicy {
    #[must_use]
    pub fn from_visible_tool_names(
        mode: TurnInteractionMode,
        visible_tool_names: Vec<String>,
    ) -> Self {
        let mut deduped_visible = Vec::new();
        let mut seen = HashSet::new();
        for name in visible_tool_names {
            if seen.insert(name.clone()) {
                deduped_visible.push(name);
            }
        }
        let observation_tool_names = deduped_visible
            .iter()
            .filter(|name| tool_counts_as_external_observation(name))
            .cloned()
            .collect();
        Self {
            mode,
            visible_tool_names: deduped_visible,
            observation_tool_names,
            can_pause_for_user: mode.can_pause_for_user(),
            allow_ask_user: mode.allows_ask_user(),
        }
    }

    #[must_use]
    pub fn from_tool_schemas(mode: TurnInteractionMode, schemas: &[Value]) -> Self {
        let mut names = Vec::new();
        let mut seen = HashSet::new();
        for schema in schemas {
            if let Some(name) = schema
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
            {
                let owned = name.to_string();
                if seen.insert(owned.clone()) {
                    names.push(owned);
                }
            }
        }
        Self::from_visible_tool_names(mode, names)
    }

    #[must_use]
    pub fn has_observation_tools(&self) -> bool {
        !self.observation_tool_names.is_empty()
    }
}

/// Returns the set of tool names that should be restricted for the given interaction mode.
#[must_use]
pub fn interaction_scoped_tool_restrictions(mode: TurnInteractionMode) -> HashSet<String> {
    if mode.allows_ask_user() {
        HashSet::new()
    } else {
        HashSet::from([ASK_USER_TOOL_NAME.to_string()])
    }
}

/// Whether a tool invocation observes user, world, or workspace state.
///
/// Control-plane and runtime-self-observation tools remain useful for steering,
/// but are tracked separately from external observations in performance facts.
#[must_use]
pub fn tool_counts_as_external_observation(tool_name: &str) -> bool {
    if tool_name == ASK_USER_TOOL_NAME {
        return false;
    }
    if matches!(
        tool_name,
        "introspect"
            | "tool_search"
            | "compress_context"
            | "rollback_session_state"
            | "enter_plan_mode"
            | "exit_plan_mode"
    ) {
        return false;
    }
    if let Some(meta) = tool_meta(tool_name) {
        if meta.intents.contains(&IntentType::Introspect) {
            return false;
        }
        if !meta.intents.iter().any(|intent| {
            matches!(
                intent,
                IntentType::CodeRead
                    | IntentType::CodeEdit
                    | IntentType::Git
                    | IntentType::GitHub
                    | IntentType::Memory
                    | IntentType::Database
            )
        }) {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_changes_advisory_presentation_not_policy_delivery() {
        assert!(!TurnInteractionMode::Auto.shows_policy_feedback_status());
        for mode in [
            TurnInteractionMode::Prompt,
            TurnInteractionMode::NonInteractive,
            TurnInteractionMode::Headless,
            TurnInteractionMode::Deny,
        ] {
            assert!(mode.shows_policy_feedback_status(), "mode={mode:?}");
        }
    }

    #[test]
    fn runtime_control_tools_do_not_count_as_external_observations() {
        for name in [
            ASK_USER_TOOL_NAME,
            "introspect",
            "reflect",
            "session",
            "tool_search",
            "compress_context",
            "rollback_session_state",
            "enter_plan_mode",
            "exit_plan_mode",
        ] {
            assert!(
                !tool_counts_as_external_observation(name),
                "{name} must not count as an external observation"
            );
        }

        for name in [
            "read_file",
            "grep",
            "glob",
            "bash",
            "git",
            "github",
            "mo_query",
        ] {
            assert!(
                tool_counts_as_external_observation(name),
                "{name} should count as an external observation"
            );
        }
    }

    #[test]
    fn interaction_policy_excludes_control_tools_from_evidence_list() {
        let policy = TurnInteractionPolicy::from_visible_tool_names(
            TurnInteractionMode::Auto,
            vec![
                "introspect".into(),
                "read_file".into(),
                "ask_user".into(),
                "grep".into(),
            ],
        );

        assert_eq!(policy.observation_tool_names, vec!["read_file", "grep"]);
        assert!(policy.has_observation_tools());
    }
}
