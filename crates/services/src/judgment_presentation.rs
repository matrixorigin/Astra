//! Shared, deterministic vocabulary for judgment diagnostics.
//!
//! These helpers are presentation-only. They do not classify requests, infer
//! adoption, or create a second usage ledger. All callers must continue to
//! source facts from their owning physical, semantic, or application records.

/// Keep the provider name useful to a user while preserving the actual model
/// identity beside it. `typesafe` is the wire provider for Jev.
pub fn provider_label(provider: &str) -> &str {
    if provider == "typesafe" {
        "Jev"
    } else {
        provider
    }
}

/// Stable user-facing labels for the supported judgment operations.
pub fn purpose_label(operation: &str, purpose: &str) -> &'static str {
    match operation {
        "request_judgment" => "Request classification",
        "completion_proxy:turn_intent" => "Request classification",
        "skill_auto_route" => "Skill selection",
        "work_plan" => "Work planning",
        "work_direction" => "Work planning",
        "memory_relevance" | "memory_feedback" | "memory_retrieval_rerank" => "Memory judgment",
        "completion_proxy:memory_retrieval_rerank" => "Memory judgment",
        "tool_result_rerank" | "completion_proxy:tool_result_rerank" => "Tool result selection",
        "verification_judge" | "completion_proxy:verification_judge" => "Verification",
        "memory_extraction" => "Memory extraction",
        "introspection" => "Request analysis",
        "reflection" => "Reflection",
        "required_compaction" => "Context summary",
        _ => match purpose {
            "memory_retrieval_rerank" => "Memory judgment",
            "tool_result_rerank" => "Tool result selection",
            "memory_extraction" => "Memory extraction",
            "introspection" => "Request analysis",
            "verification_judge" => "Verification",
            "reflection" => "Reflection",
            "required_compaction" => "Context summary",
            _ => "Auxiliary inference",
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_wire_provider_and_known_operations_without_guessing_unknowns() {
        assert_eq!(provider_label("typesafe"), "Jev");
        assert_eq!(provider_label("deepseek"), "deepseek");
        assert_eq!(
            purpose_label("request_judgment", ""),
            "Request classification"
        );
        assert_eq!(
            purpose_label("completion_proxy:memory_retrieval_rerank", ""),
            "Memory judgment"
        );
        assert_eq!(purpose_label("verification_judge", ""), "Verification");
        assert_eq!(
            purpose_label("unknown", "memory_retrieval_rerank"),
            "Memory judgment"
        );
        assert_eq!(purpose_label("unknown", "other"), "Auxiliary inference");
    }
}
