//! Build [`crate::tool_selector::SelectionContext`] for the agentic `/chat` payload path.

use crate::pipeline::routing::DomainHint;
use crate::tool_registry::ToolRegistry;
use crate::tool_selector::SelectionContext;

/// `follow_up_tool_round`: first user turn vs mid-loop (`tool_results` non-empty) — doubles schema budget.
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn build_agentic_tool_selection_context<'a>(
    query: &'a str,
    history_pair_count: usize,
    recent_tools: &'a [String],
    registry: &ToolRegistry,
    boost_terms: Vec<String>,
    budget_pressure: f64,
    memory_domain_hints: Vec<DomainHint>,
    restricted_tools: Vec<String>,
    file_context: Vec<String>,
    follow_up_tool_round: bool,
    tool_budget_override: Option<u32>,
) -> SelectionContext<'a> {
    let turn_count = history_pair_count as u32 + 1;
    let base = tool_budget_override
        .filter(|&b| b > 0)
        .unwrap_or_else(|| registry.default_budget());
    let budget_tokens = if follow_up_tool_round { base * 2 } else { base };
    SelectionContext {
        query,
        turn_count,
        recent_tools,
        budget_tokens,
        boost_terms,
        budget_pressure,
        memory_domain_hints,
        restricted_tools,
        file_context,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_registry::ToolRegistry;

    #[test]
    fn follow_up_doubles_budget_token_field() {
        let reg = ToolRegistry::new(vec![]);
        let ctx_a = build_agentic_tool_selection_context(
            "hi",
            0,
            &[],
            &reg,
            vec![],
            0.0,
            vec![],
            vec![],
            vec![],
            false,
            None,
        );
        let ctx_b = build_agentic_tool_selection_context(
            "hi",
            0,
            &[],
            &reg,
            vec![],
            0.0,
            vec![],
            vec![],
            vec![],
            true,
            None,
        );
        assert_eq!(ctx_b.budget_tokens, ctx_a.budget_tokens * 2);
        assert_eq!(ctx_a.turn_count, 1);
    }

    #[test]
    fn budget_override_replaces_registry_default() {
        let reg = ToolRegistry::new(vec![]);
        let default_budget = reg.default_budget();

        // Without override: uses registry default
        let ctx_none = build_agentic_tool_selection_context(
            "hi",
            0,
            &[],
            &reg,
            vec![],
            0.0,
            vec![],
            vec![],
            vec![],
            false,
            None,
        );
        assert_eq!(ctx_none.budget_tokens, default_budget);

        // With override: uses override value
        let ctx_override = build_agentic_tool_selection_context(
            "hi",
            0,
            &[],
            &reg,
            vec![],
            0.0,
            vec![],
            vec![],
            vec![],
            false,
            Some(1200),
        );
        assert_eq!(ctx_override.budget_tokens, 1200);

        // Override of 0 falls back to registry default
        let ctx_zero = build_agentic_tool_selection_context(
            "hi",
            0,
            &[],
            &reg,
            vec![],
            0.0,
            vec![],
            vec![],
            vec![],
            false,
            Some(0),
        );
        assert_eq!(ctx_zero.budget_tokens, default_budget);

        // Follow-up round doubles the override
        let ctx_followup = build_agentic_tool_selection_context(
            "hi",
            0,
            &[],
            &reg,
            vec![],
            0.0,
            vec![],
            vec![],
            vec![],
            true,
            Some(1200),
        );
        assert_eq!(ctx_followup.budget_tokens, 2400);
    }
}
