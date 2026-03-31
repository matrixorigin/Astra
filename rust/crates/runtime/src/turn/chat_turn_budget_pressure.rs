//! Compaction-tier budget pressure for `/chat` payload assembly (selector + schemas).

use serde_json::Value;

use crate::prompts;

/// Same pressure value used when building `SelectionContext` for tool selection.
#[must_use]
pub fn budget_pressure_for_chat_turn(
    messages: &[Value],
    model: Option<&str>,
    pinned_schema_tokens: usize,
) -> f64 {
    let estimated = prompts::estimate_tokens_precise(messages, pinned_schema_tokens, 0);
    let budget = prompts::budget_for_model(model);
    let tier = budget.compaction_tier(estimated);
    tier.budget_pressure()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn budget_pressure_is_finite_for_minimal_messages() {
        let p = budget_pressure_for_chat_turn(&[json!({"role":"user","content":"hi"})], None, 0);
        assert!(p.is_finite());
    }
}
