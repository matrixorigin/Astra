//! Shared interaction-mode types extracted from `agentic_loop_host`.
//!
//! These live in turn-core so that modules like `chat_turn_heuristics` and
//! `stop_hooks_yaml` can reference them without depending on the full runtime.

use serde_json::Value;
use std::collections::HashSet;

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

    /// True when the runtime should suppress its own *interruption-style*
    /// nudges (execution-escalation, parallel-batching force, cache-waste,
    /// redundant-reads, exploration-family, round-budget phase1, circuit-
    /// breaker finalization).
    ///
    /// Motivation: when the user explicitly chose `Auto` they are
    /// signalling "trust the model to finish the task end-to-end — don't
    /// pepper it with corrections". Every nudge we inject costs tokens,
    /// tanks cache (it changes the message tail), and is visible to the
    /// user as "being interrupted". Observed in session `3b7ac18f`:
    /// 10+ nudge injections in Auto mode across 15 turns, many with
    /// overlapping guidance.
    ///
    /// Safety critical corrections (sandbox denial, hard tool failures,
    /// ask-user protocol) go through separate paths and are NOT gated by
    /// this predicate.
    #[must_use]
    pub fn suppresses_loop_nudges(self) -> bool {
        matches!(self, Self::Auto)
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

/// Returns the set of tool names that should be restricted for the given interaction mode.
#[must_use]
pub fn interaction_scoped_tool_restrictions(mode: TurnInteractionMode) -> HashSet<String> {
    if mode.allows_ask_user() {
        HashSet::new()
    } else {
        HashSet::from([ASK_USER_TOOL_NAME.to_string()])
    }
}

/// Whether a tool invocation counts as factual evidence (all tools except ask_user).
#[must_use]
pub fn tool_counts_as_factual_evidence(tool_name: &str) -> bool {
    tool_name != ASK_USER_TOOL_NAME
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── suppresses_loop_nudges: the gate that closes the session 3b7ac18f
    //    complaint "不停的被打断，不一气呵成"
    //
    // The user explicitly chose Auto mode. We interpret that as a
    // signal to *stop* injecting the whole family of interruption
    // nudges (parallel-batching force, execution escalation, cache
    // waste, etc.) which the model otherwise has to acknowledge
    // round-by-round. Pinning the mapping here so the gate stays one
    // line in each call site.

    #[test]
    fn auto_suppresses_loop_nudges() {
        assert!(TurnInteractionMode::Auto.suppresses_loop_nudges());
    }

    #[test]
    fn prompt_mode_keeps_nudges() {
        // Prompt mode is "I want to see / approve each step" — keep
        // the nudges so the user sees when the runtime thinks the
        // model is wandering.
        assert!(!TurnInteractionMode::Prompt.suppresses_loop_nudges());
    }

    #[test]
    fn noninteractive_keeps_nudges() {
        // Sub-runs, plan subtasks, piped stdin — these paths should
        // still be course-corrected automatically because no human is
        // watching.
        assert!(!TurnInteractionMode::NonInteractive.suppresses_loop_nudges());
    }

    #[test]
    fn headless_keeps_nudges() {
        // Harness / eval runs — we want the model to be nudged into
        // convergence for reproducibility.
        assert!(!TurnInteractionMode::Headless.suppresses_loop_nudges());
    }

    #[test]
    fn deny_keeps_nudges() {
        // Deny mode blocks tool execution — the nudges are irrelevant
        // (tools are already blocked), but we still report them so the
        // user sees WHY nothing is progressing. No reason to silence.
        assert!(!TurnInteractionMode::Deny.suppresses_loop_nudges());
    }

    #[test]
    fn default_mode_keeps_nudges() {
        // Safety: whatever the default is (NonInteractive), it must
        // NOT be the silencing mode. Silencing is opt-in via explicit
        // Auto.
        assert!(!TurnInteractionMode::default().suppresses_loop_nudges());
    }
}
