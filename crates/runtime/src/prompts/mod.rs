//! Centralized LLM prompt strings and builders.
//!
//! All user-visible LLM instructions live here so they can be audited, tested,
//! and tuned in one place.  Callers import specific items rather than
//! scattering string literals through the codebase.

mod context;
mod system;

pub use astra_prompts::extraction::{COMPACT_UNIFIED_PROMPT, parse_compact_response};
pub use astra_prompts::memory_proto;
pub use astra_prompts::skills::{
    SystemSkill, build_skill_dev_context, build_skill_instructions, builtin_concise_skill,
    builtin_system_skills,
};
pub use context::{
    CacheAwareEstimate, CompactConfig, CompactionTier, ContextBudget,
    DEFAULT_CONTEXT_WINDOW_TOKENS, DEFAULT_SYSTEM_PROMPT_TOKENS, budget_for_model,
    budget_for_model_with_override, capped_output_tokens, estimate_json_value_tokens,
    estimate_str_tokens, estimate_tokens, estimate_tokens_cache_aware,
    estimate_tokens_cache_aware_split,
};
pub(crate) use context::{PER_MESSAGE_OVERHEAD, estimate_single_message_tokens};
pub use system::{
    CacheScope, DeferredToolsPromptBlock, PARALLEL_BATCHING_NUDGE_THRESHOLD, PromptOverrides,
    PromptSection, PromptTokenBucket, STALL_NUDGE, SYSTEM_PROMPT_BASE,
    SYSTEM_PROMPT_DYNAMIC_BOUNDARY, SystemPromptBuilder, apply_overrides,
    build_deferred_tools_prompt_block_with_budget, build_deferred_tools_section,
    build_deferred_tools_section_with_budget, build_main_system_prompt,
    build_main_system_prompt_with_style, build_pipeline_static_sections,
    build_skill_listing_section, build_skill_listing_section_for_model,
    build_skill_listing_section_with_caps,
    build_skill_listing_section_with_context_window_and_caps, build_system_prompt_sections,
    build_system_prompt_sections_with_style, build_system_prompt_trace, default_overrides_dir,
    load_overrides, parallel_batching_nudge_directive, parallel_execution_feedback,
    sections_to_string, self_awareness_prompt_section, tool_round_guidance,
    tool_round_guidance_trace, trailing_single_tool_round_streak,
};
pub(crate) use system::{self_model_section, tool_conditional_section};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_main_system_prompt_no_tools_warns_no_tools() {
        let p = build_main_system_prompt(&[], "");
        assert!(p.contains(SYSTEM_PROMPT_BASE), "should include base");
        assert!(
            p.contains("NO tools available"),
            "should warn about missing tools"
        );
        assert!(
            p.contains("Do NOT generate fake data"),
            "should have anti-hallucination rule"
        );
    }

    #[test]
    fn build_main_system_prompt_core_rules_and_protocol() {
        let p = build_main_system_prompt(&["read_file", "write_file"], "");
        assert!(p.contains("Core Rules"), "should include rules");
        assert!(
            p.contains("NEVER fabricate"),
            "should include anti-fabrication rule"
        );
        assert!(
            p.contains("Reuse history"),
            "should include history awareness"
        );
        assert!(
            p.contains("Plan, Batch, Execute"),
            "should include protocol"
        );
    }

    #[test]
    fn build_main_system_prompt_includes_profile() {
        let p = build_main_system_prompt(&["tool_a"], "\n\n## User Memories\nprefers Rust");
        assert!(p.contains("prefers Rust"), "profile should be appended");
    }

    // ── Conditional prompt sections ──

    /// When no memory tools are selected, memory rules must be omitted.
    /// This enforces: "prompt mentions tool X ⟹ tool X is available".
    #[test]
    fn no_memory_tools_omits_memory_section() {
        let p = build_main_system_prompt(&["bash", "read_file"], "");
        assert!(
            !p.contains("`memory(action="),
            "should NOT mention the memory tool when no memory tools selected"
        );
        assert!(
            !p.contains("Memory rules"),
            "should NOT include Memory section when no memory tools selected"
        );
        // Core rules still present
        assert!(p.contains("Core Rules"));
        assert!(p.contains("NEVER fabricate"));
    }

    /// When no GitHub tools are selected, GitHub-specific rules must be omitted.
    #[test]
    fn no_github_tools_omits_github_rules() {
        let p = build_main_system_prompt(&["bash", "memory"], "");
        assert!(
            !p.contains("github(action="),
            "should NOT mention the github tool when no GitHub tools selected"
        );
    }

    /// History awareness rule prevents re-reading data already in context.
    #[test]
    fn prompt_includes_history_awareness() {
        let p = build_main_system_prompt(&["read_file"], "");
        assert!(
            p.contains("check history") || p.contains("Reuse history"),
            "should instruct checking history before calling tools"
        );
    }

    #[test]
    fn no_git_tools_omits_git_guidance() {
        let p = build_main_system_prompt(&["bash", "read_file"], "");
        assert!(
            !p.contains("COMPOUND git operations"),
            "should NOT include git guidance when no git tools selected"
        );
    }

    /// Compressed prompt is shorter than the old version.
    #[test]
    fn compressed_prompt_under_token_budget() {
        let p = build_main_system_prompt(&["read_file", "bash", "memory", "github", "git"], "");
        assert!(
            p.len() < 13000,
            "compressed prompt should be under 13000 chars, got {}",
            p.len()
        );
    }

    /// Discovery Before Access guidance prevents LLMs from guessing file paths.
    #[test]
    fn prompt_includes_discovery_before_access() {
        let p = build_main_system_prompt(&["read_file", "list_dir", "glob"], "");
        assert!(
            p.contains("Discover before reading"),
            "should include discovery-first discipline guidance"
        );
        assert!(
            p.contains("Never guess"),
            "should warn against guessing paths"
        );
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
        let p = build_main_system_prompt(&["str_replace", "read_file"], "");
        assert!(
            !p.contains("## Editing Strategy"),
            "should not include editing section without multi_edit"
        );
    }

    #[test]
    fn prompt_includes_plan_execution_guidance() {
        let p = build_main_system_prompt(&["read_file", "bash"], "");
        assert!(
            p.contains("## Plan Execution"),
            "should include plan execution section"
        );
        assert!(
            p.contains("acceptance criteria"),
            "should mention acceptance criteria"
        );
        assert!(p.contains("Don't skip ahead"), "should warn about ordering");
    }
}
