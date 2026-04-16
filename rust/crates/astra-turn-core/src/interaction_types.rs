//! Shared interaction-mode types extracted from `agentic_loop_host`.

use serde_json::Value;
use std::collections::HashSet;

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
    pub evidence_tool_names: Vec<String>,
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
        let evidence_tool_names = deduped_visible
            .iter()
            .filter(|name| tool_counts_as_factual_evidence(name))
            .cloned()
            .collect();
        Self {
            mode,
            visible_tool_names: deduped_visible,
            evidence_tool_names,
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
    pub fn has_evidence_tools(&self) -> bool {
        !self.evidence_tool_names.is_empty()
    }
}

#[must_use]
pub fn interaction_scoped_tool_restrictions(mode: TurnInteractionMode) -> HashSet<String> {
    if mode.allows_ask_user() {
        HashSet::new()
    } else {
        HashSet::from([ASK_USER_TOOL_NAME.to_string()])
    }
}

#[must_use]
pub fn tool_counts_as_factual_evidence(tool_name: &str) -> bool {
    tool_name != ASK_USER_TOOL_NAME
}
