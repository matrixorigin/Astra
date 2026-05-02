//! Centralized LLM prompt strings and builders.
//!
//! All user-visible LLM instructions live here so they can be audited, tested,
//! and tuned in one place.  Callers import specific items rather than
//! scattering string literals through the codebase.

mod context;
mod system;

pub use astra_prompts::extraction::{
    COMPACT_SUMMARY_REQUEST, MEMORY_EXTRACTOR_PROMPT, parse_extracted_facts,
};
pub use astra_prompts::skills::{
    SystemSkill, build_skill_dev_prefix, build_skill_instructions, builtin_concise_skill,
    builtin_markdown_skill, builtin_system_skills,
};
pub use context::{
    CacheAwareEstimate, CompactConfig, CompactionTier, ContextBudget, DEFAULT_SYSTEM_PROMPT_TOKENS,
    budget_for_model, capped_output_tokens, compaction_tier_calibrated, estimate_str_tokens,
    estimate_tokens, estimate_tokens_cache_aware, estimate_tokens_precise,
};
pub use system::{
    CacheScope, LOW_CONFIDENCE_THRESHOLD, PARALLEL_BATCHING_NUDGE_THRESHOLD, PromptSection,
    PromptTokenBucket, ROUND_BUDGET_HARD_LIMIT, ROUND_BUDGET_THRESHOLD, STALL_NUDGE,
    SYSTEM_PROMPT_BASE, build_main_system_prompt, build_main_system_prompt_with_style,
    build_system_prompt_sections, build_system_prompt_sections_with_style,
    build_system_prompt_trace, detect_task_type, parallel_batching_nudge_directive,
    parallel_execution_feedback, round_budget_directive, round_budget_directive_with,
    sections_to_string, self_awareness_prompt_section, synthesize_or_batch_directive,
    tool_round_guidance, tool_round_guidance_trace_with, tool_round_guidance_with,
    trailing_single_tool_round_streak,
};

pub use astra_prompts::memory_lifecycle;
pub use astra_prompts::memory_proto;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_main_system_prompt_no_tools_warns_no_tools() {
        let p = build_main_system_prompt(&[], "", 1.0, None);
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
    fn build_main_system_prompt_includes_tool_names() {
        let p = build_main_system_prompt(&["read_file", "write_file"], "", 1.0, None);
        assert!(p.contains("read_file, write_file"), "should list tools");
        assert!(p.contains("Core Rules"), "should include rules");
        assert!(
            p.contains("NEVER fabricate"),
            "should include anti-fabrication rule"
        );
        assert!(
            p.contains("check conversation history"),
            "should include history awareness"
        );
        assert!(p.contains("Planning Protocol"), "should include protocol");
    }

    #[test]
    fn build_main_system_prompt_includes_profile() {
        let p =
            build_main_system_prompt(&["tool_a"], "\n\n## User Memories\nprefers Rust", 1.0, None);
        assert!(p.contains("prefers Rust"), "profile should be appended");
    }

    #[test]
    fn build_skill_dev_prefix_format() {
        let prefix =
            build_skill_dev_prefix("my_skill", "skills/my_skill/SKILL.md", "def run(): pass");
        assert!(prefix.starts_with("[SKILL DEV: my_skill]"));
        assert!(prefix.contains("def run(): pass"));
        // Must tell LLM not to re-read the file
        assert!(prefix.contains("do NOT"));
        // Must include actual file path (not hardcoded .astra/skills/)
        assert!(prefix.contains("skills/my_skill/SKILL.md"));
        // Must include dev guidelines
        assert!(prefix.contains("Dev Guidelines"));
        assert!(prefix.contains("when_to_use"));
        assert!(prefix.contains("Success criteria"));
        assert!(prefix.contains("allowed_tools"));
    }

    #[test]
    fn compact_summary_request_is_non_empty() {
        assert!(!COMPACT_SUMMARY_REQUEST.is_empty());
        assert!(COMPACT_SUMMARY_REQUEST.contains("250 words"));
        assert!(COMPACT_SUMMARY_REQUEST.contains("### Goals"));
        assert!(COMPACT_SUMMARY_REQUEST.contains("### Key Facts"));
        assert!(COMPACT_SUMMARY_REQUEST.contains("5 sections"));
    }

    #[test]
    fn builtin_system_skills_has_markdown_and_concise() {
        let skills = builtin_system_skills();
        assert!(skills.len() >= 2);
        assert!(skills.iter().any(|s| s.name == "markdown"));
        assert!(skills.iter().any(|s| s.name == "concise"));
    }

    #[test]
    fn builtin_markdown_skill_has_formatting_rules() {
        let skill = builtin_markdown_skill();
        assert!(skill.instructions.contains("headers"));
        assert!(skill.instructions.contains("bullet"));
        assert!(skill.instructions.contains("code blocks"));
    }

    #[test]
    fn build_skill_instructions_empty_returns_empty() {
        assert!(build_skill_instructions(&[]).is_empty());
    }

    #[test]
    fn build_skill_instructions_injects_instructions() {
        let skills = vec![builtin_markdown_skill()];
        let block = build_skill_instructions(&skills);
        assert!(block.contains("Output Format: Markdown"));
    }

    #[test]
    fn build_skill_instructions_multiple_skills() {
        let skills = builtin_system_skills();
        let block = build_skill_instructions(&skills);
        assert!(block.contains("Output Format: Markdown"));
        assert!(block.contains("Output Constraint: Concise"));
    }

    #[test]
    fn memory_rules_include_negative_examples() {
        let p = build_main_system_prompt(&["memory_store", "memory_retrieve"], "", 1.0, None);
        assert!(
            p.contains("What NOT to save"),
            "should have negative guidance (What NOT to save)"
        );
    }

    // ── Conditional prompt sections ──

    /// When no memory tools are selected, memory rules must be omitted.
    /// This enforces: "prompt mentions tool X ⟹ tool X is available".
    #[test]
    fn no_memory_tools_omits_memory_section() {
        let p = build_main_system_prompt(&["bash", "read_file"], "", 1.0, None);
        assert!(
            !p.contains("memory_store"),
            "should NOT mention memory_store when no memory tools selected"
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
        let p = build_main_system_prompt(&["bash", "memory_store"], "", 1.0, None);
        assert!(
            !p.contains("github_list_prs"),
            "should NOT mention github_list_prs when no GitHub tools selected"
        );
    }

    /// When GitHub tools are selected, GitHub rules are included.
    #[test]
    fn github_tools_include_github_rules() {
        let p = build_main_system_prompt(&["github_list_prs", "github_get_pr"], "", 1.0, None);
        assert!(
            p.contains("github_list_prs"),
            "should mention github_list_prs when GitHub tools selected"
        );
    }

    /// Implicit preference instruction is present when memory tools available.
    #[test]
    fn memory_rules_include_store_guidance() {
        let p = build_main_system_prompt(&["memory_store", "memory_retrieve"], "", 1.0, None);
        assert!(
            p.contains("memory_store"),
            "should mention memory_store when memory tools available"
        );
    }

    /// History awareness rule prevents re-reading data already in context.
    #[test]
    fn prompt_includes_history_awareness() {
        let p = build_main_system_prompt(&["read_file"], "", 1.0, None);
        assert!(
            p.contains("check conversation history"),
            "should instruct checking history before calling tools"
        );
    }

    /// Tool selection guidance: prefer specific tools for single ops, bash for compound.
    #[test]
    fn git_tools_get_compound_bash_guidance() {
        let p = build_main_system_prompt(&["git_diff", "git_log", "bash"], "", 1.0, None);
        assert!(
            p.contains("SINGLE operations"),
            "should guide git tools for single operations"
        );
        assert!(
            p.contains("COMPOUND git operations"),
            "should guide bash for compound git operations"
        );
    }

    #[test]
    fn no_git_tools_omits_git_guidance() {
        let p = build_main_system_prompt(&["bash", "read_file"], "", 1.0, None);
        assert!(
            !p.contains("COMPOUND git operations"),
            "should NOT include git guidance when no git tools selected"
        );
    }

    /// Compressed prompt is shorter than the old version.
    #[test]
    fn compressed_prompt_under_token_budget() {
        let p = build_main_system_prompt(
            &[
                "read_file",
                "bash",
                "memory_store",
                "github_list_prs",
                "git_diff",
            ],
            "",
            1.0,
            None,
        );
        // Full prompt with all sections should be under ~2600 tokens (~10400 chars)
        // Enhanced prompt adds: Planning Protocol, Context Strategy, Discovery Before Access,
        // Coding Discipline (including Executor rule), Turn Discipline (announce/summary/no-narration),
        // Parallel Tool Calls (with Limit/Anti-pattern), Token Efficiency, Build/Test Guidance,
        // Plan Execution, Search Strategy (with Simple vs Complex).
        // Headroom: ~200 chars above measured size. Bump when adding new rules.
        assert!(
            p.len() < 13000,
            "compressed prompt should be under 13000 chars, got {}",
            p.len()
        );
    }

    /// Discovery Before Access guidance prevents LLMs from guessing file paths.
    #[test]
    fn prompt_includes_discovery_before_access() {
        let p = build_main_system_prompt(&["read_file", "list_dir", "glob"], "", 1.0, None);
        assert!(
            p.contains("Discovery Before Access"),
            "should include discovery-first discipline section"
        );
        assert!(
            p.contains("NEVER guess file paths"),
            "should warn against guessing paths"
        );
    }

    // ── Memory Extractor tests ──

    #[test]
    fn memory_extractor_prompt_is_well_formed() {
        assert!(!MEMORY_EXTRACTOR_PROMPT.is_empty());
        assert!(MEMORY_EXTRACTOR_PROMPT.contains("JSON array"));
        assert!(MEMORY_EXTRACTOR_PROMPT.contains("\"fact\""));
        assert!(MEMORY_EXTRACTOR_PROMPT.contains("\"type\""));
        assert!(MEMORY_EXTRACTOR_PROMPT.contains("semantic"));
        assert!(MEMORY_EXTRACTOR_PROMPT.contains("profile"));
        assert!(MEMORY_EXTRACTOR_PROMPT.contains("procedural"));
        assert!(MEMORY_EXTRACTOR_PROMPT.contains("working"));
    }

    #[test]
    fn parse_extracted_facts_valid_json() {
        let raw = r#"[
            {"fact": "User prefers Rust.", "type": "profile"},
            {"fact": "Project uses cargo workspace.", "type": "procedural"}
        ]"#;
        let facts = parse_extracted_facts(raw);
        assert_eq!(facts.len(), 2);
        assert_eq!(facts[0].0, "User prefers Rust.");
        assert_eq!(facts[0].1, "profile");
        assert_eq!(facts[1].0, "Project uses cargo workspace.");
        assert_eq!(facts[1].1, "procedural");
    }

    #[test]
    fn parse_extracted_facts_with_code_fences() {
        let raw = "```json\n[\n  {\"fact\": \"Uses cargo test.\", \"type\": \"semantic\"}\n]\n```";
        let facts = parse_extracted_facts(raw);
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].0, "Uses cargo test.");
    }

    #[test]
    fn parse_extracted_facts_empty_array() {
        let raw = "[]";
        let facts = parse_extracted_facts(raw);
        assert!(facts.is_empty());
    }

    #[test]
    fn parse_extracted_facts_with_preamble() {
        let raw =
            "Here are the facts:\n[{\"fact\": \"Prefers vim.\", \"type\": \"profile\"}]\nDone.";
        let facts = parse_extracted_facts(raw);
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].0, "Prefers vim.");
    }

    #[test]
    fn parse_extracted_facts_invalid_type_defaults_to_semantic() {
        let raw = r#"[{"fact": "Some fact.", "type": "unknown_type"}]"#;
        let facts = parse_extracted_facts(raw);
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].1, "semantic");
    }

    #[test]
    fn parse_extracted_facts_missing_type_defaults_to_semantic() {
        let raw = r#"[{"fact": "No type field."}]"#;
        let facts = parse_extracted_facts(raw);
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].1, "semantic");
    }

    #[test]
    fn parse_extracted_facts_skips_empty_facts() {
        let raw = r#"[{"fact": "", "type": "semantic"}, {"fact": "Good one.", "type": "profile"}]"#;
        let facts = parse_extracted_facts(raw);
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].0, "Good one.");
    }

    #[test]
    fn parse_extracted_facts_garbage_returns_empty() {
        let raw = "This is not JSON at all, sorry!";
        let facts = parse_extracted_facts(raw);
        assert!(facts.is_empty());
    }

    // ── Token estimation & context budget tests ──

    #[test]
    fn estimate_tokens_empty() {
        // Empty messages still have fixed overhead (system prompt + tools)
        assert_eq!(estimate_tokens(&[]), 3000);
    }

    #[test]
    fn estimate_tokens_basic() {
        let msgs = vec![serde_json::json!({"role": "user", "content": "hello world"})];
        let est = estimate_tokens(&msgs);
        // "hello world" = 11 chars → 11/4 = 2 tokens + 4 overhead + 3000 fixed ≈ 3006
        assert!(est > 3000 && est < 3020, "got {est}");
    }

    #[test]
    fn estimate_tokens_scales_with_content() {
        let short = vec![serde_json::json!({"role": "user", "content": "hi"})];
        let long = vec![serde_json::json!({"role": "user", "content": "a".repeat(4000)})];
        // Long should be significantly more than short (not just overhead)
        assert!(estimate_tokens(&long) > estimate_tokens(&short) + 500);
    }

    #[test]
    fn context_budget_default_values() {
        let b = ContextBudget::default();
        assert_eq!(b.model_limit, 128_000);
        assert!((b.compact_threshold - 0.75).abs() < 0.01);
        assert_eq!(b.keep_recent_turns, 6);
    }

    #[test]
    fn context_budget_should_compact() {
        let b = ContextBudget::default();
        // effective_input_limit = 128K * (1 - 0.15) = 108,800
        // compact_trigger = 108,800 * 0.75 = 81,600
        assert!(!b.should_compact(75_000));
        assert!(b.should_compact(85_000));
    }

    #[test]
    fn budget_for_model_claude() {
        let b = budget_for_model(Some("claude-3.5-sonnet"));
        assert_eq!(b.model_limit, 200_000);
    }

    #[test]
    fn budget_for_model_gpt35() {
        let b = budget_for_model(Some("gpt-3.5-turbo"));
        assert_eq!(b.model_limit, 16_000);
    }

    #[test]
    fn budget_for_model_unknown_uses_default() {
        let b = budget_for_model(Some("some-unknown-model"));
        assert_eq!(b.model_limit, 128_000);
    }

    #[test]
    fn budget_for_model_none_uses_default() {
        let b = budget_for_model(None);
        assert_eq!(b.model_limit, 128_000);
    }

    // ── Memory Entry Protocol tests ──

    #[test]
    fn entry_encode_v1_format() {
        use memory_proto::*;
        let e = MemoryEntry::new(NS_TASK, ST_PENDING, "Review PR #42");
        assert_eq!(e.encode(), "[@task/pending] Review PR #42");
    }

    #[test]
    fn entry_roundtrip_v1() {
        use memory_proto::*;
        let original = MemoryEntry::new(NS_PLAN, ST_ACTIVE, "Finish API by Friday");
        let encoded = original.encode();
        let parsed = MemoryEntry::parse(&encoded).expect("should parse v1");
        assert_eq!(parsed, original);
    }

    #[test]
    fn entry_parse_all_namespaces() {
        use memory_proto::*;
        let cases = [
            ("[@task/pending] Do thing", NS_TASK, ST_PENDING, "Do thing"),
            ("[@task/done] Did thing", NS_TASK, ST_DONE, "Did thing"),
            ("[@plan/active] My plan", NS_PLAN, ST_ACTIVE, "My plan"),
            ("[@fact/semantic] A fact", NS_FACT, "semantic", "A fact"),
            ("[@fact/profile] User info", NS_FACT, "profile", "User info"),
            (
                "[@episode/summary] Goals...",
                NS_EPISODE,
                ST_SUMMARY,
                "Goals...",
            ),
            ("[@episode/auto] Auto sum", NS_EPISODE, ST_AUTO, "Auto sum"),
            (
                "[@pref/active] Likes Rust",
                NS_PREF,
                ST_ACTIVE,
                "Likes Rust",
            ),
            ("[@swap/archived] Old ctx", NS_SWAP, ST_ARCHIVED, "Old ctx"),
            (
                "[@insight/active] Pattern",
                NS_INSIGHT,
                ST_ACTIVE,
                "Pattern",
            ),
        ];
        for (input, ns, status, body) in &cases {
            let e = MemoryEntry::parse(input).unwrap_or_else(|| panic!("failed to parse: {input}"));
            assert_eq!(e.ns, *ns, "ns mismatch for: {input}");
            assert_eq!(e.status, *status, "status mismatch for: {input}");
            assert_eq!(e.body, *body, "body mismatch for: {input}");
        }
    }

    #[test]
    fn entry_parse_unstructured_returns_none() {
        use memory_proto::MemoryEntry;
        assert!(MemoryEntry::parse("just a random fact").is_none());
        assert!(MemoryEntry::parse("User prefers Rust.").is_none());
        assert!(MemoryEntry::parse("").is_none());
    }

    #[test]
    fn entry_memory_type_mapping() {
        use memory_proto::*;
        assert_eq!(
            MemoryEntry::new(NS_TASK, ST_PENDING, "x").memory_type(),
            "working"
        );
        assert_eq!(
            MemoryEntry::new(NS_PLAN, ST_ACTIVE, "x").memory_type(),
            "procedural"
        );
        assert_eq!(
            MemoryEntry::new(NS_FACT, "semantic", "x").memory_type(),
            "semantic"
        );
        assert_eq!(
            MemoryEntry::new(NS_EPISODE, ST_SUMMARY, "x").memory_type(),
            "episodic"
        );
        assert_eq!(
            MemoryEntry::new(NS_PREF, ST_ACTIVE, "x").memory_type(),
            "profile"
        );
        assert_eq!(
            MemoryEntry::new(NS_SWAP, ST_ARCHIVED, "x").memory_type(),
            "working"
        );
        assert_eq!(
            MemoryEntry::new(NS_INSIGHT, ST_ACTIVE, "x").memory_type(),
            "semantic"
        );
    }

    #[test]
    fn entry_to_store_payload() {
        use memory_proto::*;
        let e = MemoryEntry::new(NS_TASK, ST_PENDING, "Review PR");
        let payload = e.to_store_payload();
        assert_eq!(payload["content"], "[@task/pending] Review PR");
        assert_eq!(payload["memory_type"], "working");
    }

    #[test]
    fn entry_purge_payload() {
        use memory_proto::*;
        let p = MemoryEntry::purge_payload(NS_TASK);
        assert_eq!(p["topic"], "[@task/");
    }

    #[test]
    fn entry_purge_ns_status_payload() {
        use memory_proto::*;
        let p = MemoryEntry::purge_ns_status_payload(NS_TASK, ST_PENDING);
        assert_eq!(p["topic"], "[@task/pending]");
    }

    #[test]
    fn entry_search_query_ns_only() {
        use memory_proto::*;
        let q = MemoryEntry::search_query(NS_PLAN, "");
        assert_eq!(q["query"], "[@plan/");
        assert_eq!(q["top_k"], 20);
    }

    #[test]
    fn entry_search_query_with_terms() {
        use memory_proto::*;
        let q = MemoryEntry::search_query(NS_TASK, "pending done");
        assert_eq!(q["query"], "[@task/] pending done");
    }

    #[test]
    fn filter_ns_works() {
        use memory_proto::*;
        let contents = &[
            "[@task/pending] Task A",
            "[@task/done] Task B",
            "[@plan/active] My plan",
            "[@fact/semantic] A fact",
        ];
        let tasks = filter_ns(contents, NS_TASK);
        assert_eq!(tasks.len(), 2);
        assert!(tasks[0].body == "Task A");
        assert!(tasks[1].body == "Task B");
    }

    #[test]
    fn filter_ns_status_works() {
        use memory_proto::*;
        let contents = &[
            "[@task/pending] Task A",
            "[@task/done] Task B",
            "[@task/pending] Task C",
        ];
        let pending = filter_ns_status(contents, NS_TASK, ST_PENDING);
        assert_eq!(pending.len(), 2);
        let done = filter_ns_status(contents, NS_TASK, ST_DONE);
        assert_eq!(done.len(), 1);
    }

    #[test]
    fn partition_memories_separates() {
        use memory_proto::*;
        let contents = &[
            "[@task/pending] Task A",
            "Unstructured fact about Rust",
            "[@plan/active] Plan text",
            "",
            "Another unstructured thing",
        ];
        let (structured, unstructured) = partition_memories(contents);
        assert_eq!(structured.len(), 2);
        assert_eq!(unstructured.len(), 2);
    }

    #[test]
    fn format_for_llm_groups_namespaces() {
        use memory_proto::*;
        let contents = &[
            "[@pref/active] Focus: matrixorigin/memoria",
            "[@fact/semantic] Project uses cargo workspace",
            "[@task/pending] Review PR #42",
            "[@task/done] Fix login bug",
            "[@plan/active] Finish API by Friday",
            "Unstructured old memory",
        ];
        let formatted = format_for_llm(contents);
        assert!(formatted.contains("**Preferences:**"), "got: {formatted}");
        assert!(formatted.contains("**Knowledge:**"), "got: {formatted}");
        assert!(formatted.contains("**Tasks:**"), "got: {formatted}");
        assert!(formatted.contains("**Active Plan:**"), "got: {formatted}");
        assert!(formatted.contains("**Context:**"), "got: {formatted}");
        assert!(formatted.contains("✓ Fix login bug"), "got: {formatted}");
        assert!(formatted.contains("○ Review PR #42"), "got: {formatted}");
    }

    #[test]
    fn entry_display_task_line() {
        use memory_proto::*;
        let pending = MemoryEntry::new(NS_TASK, ST_PENDING, "Do thing");
        assert_eq!(pending.display_task_line(), "○ Do thing");
        let done = MemoryEntry::new(NS_TASK, ST_DONE, "Did thing");
        assert_eq!(done.display_task_line(), "✓ Did thing");
    }

    #[test]
    fn entry_multiline_body_preserves() {
        use memory_proto::*;
        let e = MemoryEntry::new(
            NS_EPISODE,
            ST_SUMMARY,
            "### Goals\nFix auth\n### Status\nDone",
        );
        let encoded = e.encode();
        let parsed = MemoryEntry::parse(&encoded).unwrap();
        assert!(parsed.body.contains("### Goals"));
        assert!(parsed.body.contains("### Status"));
    }

    // ── EntryMeta provenance tests ──────────────────────────────────
    #[test]
    fn entry_meta_from_session_has_all_fields() {
        use memory_proto::*;
        let meta = EntryMeta::from_session(Some("sess-123"), 5, SRC_USER);
        assert_eq!(meta.session_id, Some("sess-123".to_string()));
        assert_eq!(meta.turn, Some(5));
        assert_eq!(meta.source, Some("user".to_string()));
        assert!(meta.created_at.is_some());
        let ts = meta.created_at.unwrap();
        assert!(ts.contains("T"), "timestamp should be ISO 8601: {ts}");
    }

    #[test]
    fn entry_meta_to_json_includes_all_fields() {
        use memory_proto::*;
        let meta = EntryMeta {
            session_id: Some("s1".into()),
            turn: Some(3),
            source: Some("compact".into()),
            created_at: Some("2025-01-01T00:00:00Z".into()),
            trust_tier: None,
        };
        let j = meta.to_json();
        assert_eq!(j["session_id"], "s1");
        assert_eq!(j["turn"], 3);
        assert_eq!(j["source"], "compact");
        assert_eq!(j["created_at"], "2025-01-01T00:00:00Z");
    }

    #[test]
    fn entry_meta_default_has_no_fields() {
        use memory_proto::*;
        let meta = EntryMeta::default();
        let j = meta.to_json();
        assert!(j.as_object().unwrap().is_empty());
    }

    #[test]
    fn store_payload_with_meta_includes_metadata() {
        use memory_proto::*;
        let entry = MemoryEntry::new(NS_TASK, ST_PENDING, "Review PR");
        let meta = EntryMeta {
            session_id: Some("sess-42".into()),
            turn: Some(7),
            source: Some(SRC_USER.into()),
            created_at: Some("2025-06-01T12:00:00Z".into()),
            trust_tier: None,
        };
        let payload = entry.to_store_payload_with_meta(&meta);
        assert_eq!(payload["content"], "[@task/pending] Review PR");
        assert_eq!(payload["memory_type"], "working");
        assert_eq!(payload["metadata"]["session_id"], "sess-42");
        assert_eq!(payload["metadata"]["turn"], 7);
        assert_eq!(payload["metadata"]["source"], "user");
    }

    #[test]
    fn store_payload_without_meta_has_no_metadata() {
        use memory_proto::*;
        let entry = MemoryEntry::new(NS_FACT, ST_ACTIVE, "Rust is fast");
        let payload = entry.to_store_payload();
        assert!(payload.get("metadata").is_none());
        assert!(payload.get("trust_tier").is_none());
    }

    #[test]
    fn store_payload_with_trust_tier_emits_top_level_field() {
        use memory_proto::*;
        let entry = MemoryEntry::new(NS_FACT, ST_ACTIVE, "User prefers Rust");
        let meta = EntryMeta::from_session_with_tier(Some("sess-1"), 1, SRC_USER, TIER_VERIFIED);
        let payload = entry.to_store_payload_with_meta(&meta);
        // trust_tier is top-level for Memoria API
        assert_eq!(payload["trust_tier"], "T1");
        // session_id also top-level
        assert_eq!(payload["session_id"], "sess-1");
        // metadata still has provenance
        assert_eq!(payload["metadata"]["source"], "user");
    }

    // ── Task type detection tests ──

    #[test]
    fn detect_task_type_code_review_english() {
        assert_eq!(detect_task_type("review this PR"), Some("code_review"));
        assert_eq!(detect_task_type("code review please"), Some("code_review"));
        assert_eq!(
            detect_task_type("check the pull request"),
            Some("code_review")
        );
        assert_eq!(detect_task_type("show me the diff"), Some("code_review"));
    }

    #[test]
    fn detect_task_type_code_review_chinese() {
        assert_eq!(detect_task_type("评审一下这个PR"), Some("code_review"));
        assert_eq!(detect_task_type("代码审查"), Some("code_review"));
    }

    #[test]
    fn detect_task_type_debugging_english() {
        assert_eq!(detect_task_type("debug this error"), Some("debugging"));
        assert_eq!(
            detect_task_type("there's a bug in the code"),
            Some("debugging")
        );
        assert_eq!(detect_task_type("exception on line 42"), Some("debugging"));
        assert_eq!(detect_task_type("the server crashed"), Some("debugging"));
    }

    #[test]
    fn detect_task_type_debugging_chinese() {
        assert_eq!(detect_task_type("帮我调试一下"), Some("debugging"));
        assert_eq!(detect_task_type("代码报错了"), Some("debugging"));
    }

    #[test]
    fn detect_task_type_general_returns_none() {
        // Queries that match newly added task types should be detected
        assert_eq!(
            detect_task_type("how does this function work?"),
            Some("exploration")
        );
        assert_eq!(
            detect_task_type("explain the architecture"),
            Some("exploration")
        );
        // "write a new feature" doesn't match "write code" or "add feature"
        assert_eq!(detect_task_type("write a new feature"), None);
        // Truly ambiguous queries should still return None
        assert_eq!(detect_task_type(""), None);
        assert_eq!(detect_task_type("hello there"), None);
        assert_eq!(detect_task_type("thanks"), None);
    }

    #[test]
    fn detect_task_type_disambiguates_best_match() {
        // "review" alone → code_review
        assert_eq!(detect_task_type("review"), Some("code_review"));
        // "error" alone → debugging
        assert_eq!(detect_task_type("got an error"), Some("debugging"));
    }

    // ── Task-type specific prompt content tests ──

    #[test]
    fn prompt_includes_code_review_strategy_when_task_type_set() {
        let p = build_main_system_prompt(&["bash", "git_diff"], "", 1.0, Some("code_review"));
        assert!(
            p.contains("Code Review Strategy"),
            "code_review task_type should inject review strategy"
        );
        assert!(
            p.contains("Evidence BEFORE conclusions"),
            "review strategy should emphasize evidence-first approach"
        );
    }

    #[test]
    fn prompt_includes_debugging_strategy_when_task_type_set() {
        let p = build_main_system_prompt(&["bash", "read_file"], "", 1.0, Some("debugging"));
        assert!(
            p.contains("Debugging Strategy"),
            "debugging task_type should inject debugging strategy"
        );
        assert!(
            p.contains("hypothesis"),
            "debugging strategy should mention hypothesis"
        );
    }

    #[test]
    fn prompt_omits_task_strategy_for_general() {
        let p = build_main_system_prompt(&["bash", "read_file"], "", 1.0, None);
        assert!(
            !p.contains("Code Review Strategy"),
            "general should not have review strategy"
        );
        assert!(
            !p.contains("Debugging Strategy"),
            "general should not have debugging strategy"
        );
    }

    #[test]
    fn prompt_omits_task_strategy_for_unknown_type() {
        let p = build_main_system_prompt(&["bash"], "", 1.0, Some("planning"));
        assert!(
            !p.contains("Code Review Strategy"),
            "planning should not have review strategy"
        );
        assert!(
            !p.contains("Debugging Strategy"),
            "planning should not have debugging strategy"
        );
    }

    // ─── Tool guidance section tests ────────────────────────────────────

    #[test]
    fn prompt_includes_code_navigation_guidance() {
        let p = build_main_system_prompt(
            &["find_definition", "find_references", "read_file"],
            "",
            1.0,
            None,
        );
        assert!(
            p.contains("## Code Navigation"),
            "should include code nav section"
        );
        assert!(
            p.contains("find_definition"),
            "should mention find_definition"
        );
        assert!(
            p.contains("find_references"),
            "should mention find_references"
        );
    }

    #[test]
    fn prompt_includes_call_graph_guidance() {
        let p = build_main_system_prompt(
            &["find_definition", "call_graph", "read_file"],
            "",
            1.0,
            None,
        );
        assert!(p.contains("call_graph"), "should mention call_graph tool");
        assert!(
            p.contains("refactoring") || p.contains("dependencies"),
            "should mention use case"
        );
    }

    #[test]
    fn prompt_includes_editing_strategy_guidance() {
        let p = build_main_system_prompt(
            &["multi_edit", "str_replace", "delete_file", "read_file"],
            "",
            1.0,
            None,
        );
        assert!(
            p.contains("## Editing Strategy"),
            "should include editing section"
        );
        assert!(p.contains("multi_edit"), "should mention multi_edit");
        assert!(p.contains("dry_run"), "should mention dry_run preview");
        assert!(p.contains("delete_file"), "should mention delete_file");
    }

    #[test]
    fn prompt_omits_editing_guidance_without_multi_edit() {
        let p = build_main_system_prompt(&["str_replace", "read_file"], "", 1.0, None);
        assert!(
            !p.contains("## Editing Strategy"),
            "should not include editing section without multi_edit"
        );
    }

    #[test]
    fn prompt_full_toolset_under_budget() {
        // Test with a realistic full toolset that triggers all guidance sections
        let p = build_main_system_prompt(
            &[
                "read_file",
                "bash",
                "str_replace",
                "write_file",
                "delete_file",
                "multi_edit",
                "list_dir",
                "grep",
                "glob",
                "find_definition",
                "find_references",
                "call_graph",
                "symbols",
                "rename_symbol",
                "dead_code",
                "extract_members",
                "type_hierarchy",
                "hover_info",
                "symbol_search",
                "run_build_test",
                "git_diff",
                "git_log",
                "git_commit",
                "git_stash",
                "github_list_prs",
                "github_repo_stats",
                "memory_store",
                "memory_retrieve",
            ],
            "",
            1.0,
            Some("code_review"),
        );
        // All sections should be present
        assert!(p.contains("## Code Navigation"));
        assert!(p.contains("## Editing Strategy"));
        assert!(p.contains("## Build & Test Loop"));
        assert!(p.contains("## Git Workflow"));
        assert!(p.contains("## Memory Rules"));
        // Budget: full prompt should still be reasonable (allows for Executor rule addition,
        // Parallel Tool Calls Limit/Anti-pattern, Search Strategy Simple vs Complex,
        // Batching read-only tool calls, and Turn Discipline (announce/summary/no-narration)).
        // Headroom: ~200 chars above measured size. Bump when adding new rules.
        assert!(
            p.len() < 24000,
            "full toolset prompt should be under 24000 chars, got {}",
            p.len()
        );
    }

    #[test]
    fn prompt_includes_plan_execution_guidance() {
        // Plan Execution section is always included (not tool-conditional)
        let p = build_main_system_prompt(&["read_file", "bash"], "", 1.0, None);
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
