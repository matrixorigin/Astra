//! Centralized LLM prompt strings and builders.
//!
//! All user-visible LLM instructions live here so they can be audited, tested,
//! and tuned in one place.  Callers import specific items rather than
//! scattering string literals through the codebase.

mod context;
mod system;

pub use astra_prompts::extraction::{COMPACT_UNIFIED_PROMPT, parse_compact_response};
pub use astra_prompts::memory_proto;
pub use context::{
    CacheAwareEstimate, CompactConfig, CompactionTier, ContextBudget, ContextWindowPolicy,
    ContextWindowPolicySource, DEFAULT_CONTEXT_WINDOW_TOKENS, DEFAULT_SYSTEM_PROMPT_TOKENS,
    budget_for_model, budget_for_model_with_metadata, budget_for_model_with_override,
    capped_output_tokens, estimate_json_value_tokens, estimate_str_tokens, estimate_tokens,
    estimate_tokens_cache_aware, estimate_tokens_cache_aware_split,
};
pub(crate) use context::{
    MODEL_FRAMING_TOKENS, PER_MESSAGE_OVERHEAD, estimate_single_message_tokens,
    estimate_wire_input_tokens, measured_prompt_tokens_from_manifest,
};
pub use system::{
    CacheScope, DeferredToolsPromptBlock, PARALLEL_BATCHING_NUDGE_THRESHOLD, PromptOverrides,
    PromptSection, PromptTokenBucket, STALL_NUDGE, SYSTEM_PROMPT_BASE,
    build_deferred_tool_names_prompt_block_with_budget,
    build_deferred_tools_prompt_block_with_budget, build_deferred_tools_section_with_budget,
    build_pipeline_static_sections, build_skill_listing_section,
    build_skill_listing_section_for_model, build_skill_listing_section_with_caps,
    build_skill_listing_section_with_context_window_and_caps, default_overrides_dir,
    execution_slice_guidance, load_overrides, parallel_batching_nudge_directive,
    parallel_execution_feedback, tool_round_guidance, tool_round_guidance_trace,
    trailing_single_tool_round_streak,
};
pub(crate) use system::{
    DURABLE_WORK_ATTEMPT_CONTINUATION_INSTRUCTION, DURABLE_WORK_ATTEMPT_FRAME_INSTRUCTION,
    tool_conditional_section,
};

#[cfg(test)]
mod tests {
    use super::*;

    // ── Conditional prompt sections ──

    /// When no memory tools are selected, memory rules must be omitted.
    /// This enforces: "prompt mentions tool X ⟹ tool X is available".
    #[test]
    fn no_memory_tools_omits_memory_section() {
        let p = tool_conditional_section(&["bash", "read_file"]);
        assert!(
            !p.contains("`memory(action="),
            "should NOT mention the memory tool when no memory tools selected"
        );
        assert!(
            !p.contains("Memory rules"),
            "should NOT include Memory section when no memory tools selected"
        );
    }

    /// When no GitHub tools are selected, GitHub-specific rules must be omitted.
    #[test]
    fn no_github_tools_omits_github_rules() {
        let p = tool_conditional_section(&["bash", "memory"]);
        assert!(
            !p.contains("github(action="),
            "should NOT mention the github tool when no GitHub tools selected"
        );
    }

    #[test]
    fn no_git_tools_omits_git_guidance() {
        let p = tool_conditional_section(&["bash", "read_file"]);
        assert!(
            !p.contains("COMPOUND git operations"),
            "should NOT include git guidance when no git tools selected"
        );
    }

    /// Compressed prompt is shorter than the old version.
    #[test]
    fn builtin_rules_with_common_tools_stay_within_byte_budget() {
        let sections = system::static_sections_for_test(None);
        let bytes = sections
            .as_vec()
            .iter()
            .map(|section| section.text.len())
            .sum::<usize>()
            + tool_conditional_section(&["read_file", "bash", "memory", "github", "git"]).len();
        assert!(
            bytes < 13_000,
            "built-in rules and common guidance use {bytes} bytes"
        );
    }

    /// Discovery Before Access guidance prevents LLMs from guessing file paths.
    #[test]
    fn prompt_includes_discovery_before_access() {
        let sections = system::static_sections_for_test(None);
        assert!(
            sections
                .planning_protocol
                .text
                .contains("Discover before reading")
        );
        assert!(sections.planning_protocol.text.contains("Never guess"));
    }

    // ── Token estimation & context budget tests ──

    #[test]
    fn estimate_tokens_empty() {
        let est = estimate_tokens(&[], 0, 0);
        assert!(est >= 14_000, "should have base overhead, got {est}");
    }

    #[test]
    fn estimate_tokens_basic() {
        let msgs = vec![serde_json::json!({"role": "user", "content": "hello world"})];
        let est = estimate_tokens(&msgs, 0, 0);
        assert!(est > 14_000 && est < 14_500, "got {est}");
    }

    #[test]
    fn estimate_tokens_scales_with_content() {
        let short = vec![serde_json::json!({"role": "user", "content": "hi"})];
        let long = vec![serde_json::json!({"role": "user", "content": "a".repeat(4000)})];
        assert!(estimate_tokens(&long, 0, 0) > estimate_tokens(&short, 0, 0) + 500);
    }

    #[test]
    fn context_budget_default_values() {
        let b = ContextBudget::default();
        assert_eq!(b.model_limit, 200_000);
        assert!((b.compact_threshold - 0.75).abs() < 0.01);
        assert_eq!(b.keep_recent_turns, 6);
    }

    #[test]
    fn context_budget_should_compact() {
        let b = ContextBudget::default();
        assert!(!b.should_compact(134_999));
        assert!(b.should_compact(135_001));
    }

    #[test]
    fn budget_for_model_claude() {
        let b = budget_for_model(Some("claude-3.5-sonnet"));
        assert_eq!(b.model_limit, 200_000);
    }

    #[test]
    fn budget_for_model_gpt35() {
        let b = budget_for_model(Some("gpt-3.5-turbo"));
        assert_eq!(b.model_limit, 200_000);
    }

    #[test]
    fn budget_for_model_unknown_uses_default() {
        let b = budget_for_model(Some("some-unknown-model"));
        assert_eq!(b.model_limit, 200_000);
    }

    #[test]
    fn budget_for_model_none_uses_default() {
        let b = budget_for_model(None);
        assert_eq!(b.model_limit, 200_000);
    }

    #[test]
    fn prompt_omits_editing_guidance_without_multi_edit() {
        let p = tool_conditional_section(&["str_replace", "read_file"]);
        assert!(
            !p.contains("## Editing Strategy"),
            "should not include editing section without multi_edit"
        );
    }

    #[test]
    fn prompt_includes_plan_execution_guidance() {
        let sections = system::static_sections_for_test(None);
        assert!(sections.plan_execution.text.contains("## Plan Execution"));
        assert!(sections.plan_execution.text.contains("Don't skip ahead"));
        assert!(
            sections
                .planning_protocol
                .text
                .contains("Executable acceptance")
        );
    }
}
