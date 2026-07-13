//! ContextBreakdown contract.

#![cfg(test)]

use super::model::{CategoryKind, ContextBreakdown, PressureBand, summarize_session_read_activity};
use astra_turn_core::context_assembly_trace::{
    Alternative, CompressionMethod, ContextAssemblyTrace, DecisionExplanation, DecisionType,
    HistorySelectionTrace, MemoryInjection, MemoryRejection, MemorySelection, MemorySource,
    PromptContextSignals, PromptGuidanceSignals, RejectionReason, SkillInjection,
    SystemPromptBreakdown, TokenBudgetTrace, TurnCompression, TurnRetention, VisibleTool,
};

fn trace(max: u32, sys: u32, hist: u32, mem: u32, tools: u32, user: u32) -> ContextAssemblyTrace {
    let total = sys + hist + mem + tools + user;
    let pressure = if max == 0 {
        0.0
    } else {
        total as f64 / max as f64
    };
    let mut t = ContextAssemblyTrace::default();
    t.token_budget = TokenBudgetTrace {
        max_tokens: max,
        system_prompt_tokens: sys,
        history_tokens: hist,
        memory_tokens: mem,
        tool_schema_tokens: tools,
        user_message_tokens: user,
        total_used: total,
        usage_source: Default::default(),
        budget_pressure: pressure,
        compression_triggered: false,
    };
    t
}

// ─── Basic construction ───────────────────────────────────────────

#[test]
fn empty_has_no_categories() {
    let b = ContextBreakdown::empty();
    assert_eq!(b.total_used, 0);
    assert_eq!(b.limit, 0);
    assert!(b.categories.is_empty());
}

#[test]
fn empty_band_is_low() {
    let b = ContextBreakdown::empty();
    assert_eq!(b.band(), PressureBand::Low);
}

#[test]
fn session_read_activity_counts_execution_and_repeat_evidence_without_parsing_output() {
    use astra_services::session_journal::{JournalEvent, ToolCallDisposition, ToolCallRecord};

    let read = |disposition, args_full: Option<&str>| ToolCallRecord {
        name: "read_file".into(),
        ok: true,
        file_path: Some("src/lib.rs".into()),
        args_full: args_full.map(str::to_owned),
        disposition: Some(disposition),
        ..Default::default()
    };
    let exact = r#"{"path":"src/lib.rs","start_line":10,"end_line":20}"#;
    let different_range = r#"{"path":"src/lib.rs","start_line":21,"end_line":40}"#;

    let first = JournalEvent::turn(Some("session"), 1, None, "read", "done", 1, 0, 0, 0)
        .with_tool_calls(vec![read(ToolCallDisposition::Executed, Some(exact))]);
    let mut compacted = ContextAssemblyTrace::default();
    compacted.token_budget.compression_triggered = true;
    let compaction =
        JournalEvent::context_assembly_recorded(Some("session"), 1, compacted.to_json_value());
    let repeated = JournalEvent::turn(Some("session"), 2, None, "again", "done", 1, 0, 0, 0)
        .with_tool_calls(vec![read(ToolCallDisposition::Reused, Some(exact))]);
    let different =
        JournalEvent::turn(Some("session"), 3, None, "more", "done", 1, 0, 0, 0).with_tool_calls(
            vec![read(ToolCallDisposition::Suppressed, Some(different_range))],
        );
    let unknown = JournalEvent::turn(Some("session"), 4, None, "bad", "done", 1, 0, 0, 0)
        .with_tool_calls(vec![read(ToolCallDisposition::Rejected, None)]);

    let summary =
        summarize_session_read_activity(&[first, compaction, repeated, different, unknown]);

    assert_eq!(summary.requested, 4);
    assert_eq!(summary.executed, 1);
    assert_eq!(summary.reused_or_suppressed, 2);
    assert_eq!(summary.other_not_executed, 1);
    assert_eq!(summary.distinct_files, 1);
    assert_eq!(summary.requests_with_exact_identity, 3);
    assert_eq!(summary.exact_repeat_requests, 1);
    assert_eq!(summary.repeats_after_recorded_compaction, 1);
}

// ─── from_trace ───────────────────────────────────────────────────

#[test]
fn from_trace_populates_five_categories_in_order() {
    let t = trace(100_000, 2_000, 8_000, 1_000, 4_000, 500);
    let b = ContextBreakdown::from_trace(&t);
    assert_eq!(b.limit, 100_000);
    assert_eq!(b.total_used, 15_500);
    let kinds: Vec<CategoryKind> = b.categories.iter().map(|c| c.kind).collect();
    assert_eq!(
        kinds,
        vec![
            CategoryKind::System,
            CategoryKind::Tools,
            CategoryKind::Memory,
            CategoryKind::History,
            CategoryKind::UserMessage,
        ],
        "category order drives the stacked-bar layout"
    );
}

#[test]
fn category_tokens_match_trace_fields() {
    let t = trace(100_000, 2_000, 8_000, 1_000, 4_000, 500);
    let b = ContextBreakdown::from_trace(&t);
    let by_kind = |k: CategoryKind| {
        b.categories
            .iter()
            .find(|c| c.kind == k)
            .expect("category present")
            .tokens
    };
    assert_eq!(by_kind(CategoryKind::System), 2_000);
    assert_eq!(by_kind(CategoryKind::Tools), 4_000);
    assert_eq!(by_kind(CategoryKind::Memory), 1_000);
    assert_eq!(by_kind(CategoryKind::History), 8_000);
    assert_eq!(by_kind(CategoryKind::UserMessage), 500);
}

#[test]
fn category_tokens_sum_to_total_used() {
    let t = trace(100_000, 2_000, 8_000, 1_000, 4_000, 500);
    let b = ContextBreakdown::from_trace(&t);
    let sum: u32 = b.categories.iter().map(|c| c.tokens).sum();
    assert_eq!(sum, b.total_used);
}

#[test]
fn pct_of_limit_scales_correctly() {
    let t = trace(100_000, 2_000, 8_000, 1_000, 4_000, 500);
    let b = ContextBreakdown::from_trace(&t);
    let hist = b
        .categories
        .iter()
        .find(|c| c.kind == CategoryKind::History)
        .unwrap();
    // 8_000 / 100_000 = 8%
    assert!((hist.pct_of_limit - 8.0).abs() < 0.001);
}

#[test]
fn zero_limit_keeps_percentages_at_zero() {
    let t = trace(0, 100, 200, 0, 50, 10);
    let b = ContextBreakdown::from_trace(&t);
    assert_eq!(b.limit, 0);
    assert!(b.categories.iter().all(|c| c.pct_of_limit == 0.0));
}

// ─── Pressure bands ───────────────────────────────────────────────

#[test]
fn band_low_under_60_percent() {
    let t = trace(100_000, 10_000, 20_000, 5_000, 10_000, 5_000);
    let b = ContextBreakdown::from_trace(&t);
    assert_eq!(b.band(), PressureBand::Low);
}

#[test]
fn band_warning_at_60_percent() {
    let t = trace(100_000, 10_000, 40_000, 0, 10_000, 0);
    let b = ContextBreakdown::from_trace(&t);
    assert_eq!(b.band(), PressureBand::Warning);
}

#[test]
fn band_critical_at_85_percent_or_more() {
    let t = trace(100_000, 20_000, 50_000, 5_000, 10_000, 500);
    let b = ContextBreakdown::from_trace(&t);
    assert_eq!(b.band(), PressureBand::Critical);
}

#[test]
fn usage_percent_matches_total_over_limit() {
    let t = trace(100_000, 10_000, 40_000, 5_000, 10_000, 500);
    let b = ContextBreakdown::from_trace(&t);
    let expected = 100.0 * 65_500.0 / 100_000.0;
    assert!((b.usage_percent() - expected).abs() < 0.001);
}

#[test]
fn category_filters_zero_token_categories() {
    // Categories with 0 tokens should be hidden so the bar doesn't
    // render micro-slices users can't read.
    let t = trace(100_000, 2_000, 8_000, 0, 0, 500);
    let b = ContextBreakdown::from_trace(&t);
    let kinds: Vec<CategoryKind> = b.categories.iter().map(|c| c.kind).collect();
    assert!(!kinds.contains(&CategoryKind::Memory));
    assert!(!kinds.contains(&CategoryKind::Tools));
}

#[test]
fn free_space_equals_limit_minus_used() {
    let t = trace(100_000, 2_000, 8_000, 1_000, 4_000, 500);
    let b = ContextBreakdown::from_trace(&t);
    assert_eq!(b.free_space_tokens, 100_000 - 15_500);
}

#[test]
fn free_space_zero_when_over_budget() {
    // Pathological case: total_used > max. Should not panic and
    // should clamp free_space at zero.
    let mut t = trace(10_000, 2_000, 8_000, 1_000, 4_000, 500);
    t.token_budget.total_used = 20_000;
    let b = ContextBreakdown::from_trace(&t);
    assert_eq!(b.free_space_tokens, 0);
}

// ─── Nested sections (tools/memory/skills) ─────────────────────────

#[test]
fn tools_sorted_by_trace_order_and_zero_filtered() {
    let mut t = trace(100_000, 1_000, 1_000, 0, 300, 0);
    t.tools.visible_tools = vec![
        VisibleTool {
            tool_name: "alpha".into(),
            tokens: 120,
        },
        VisibleTool {
            tool_name: "zero".into(),
            tokens: 0,
        },
        VisibleTool {
            tool_name: "bravo".into(),
            tokens: 180,
        },
    ];
    let b = ContextBreakdown::from_trace(&t);
    let names: Vec<&str> = b.tools.iter().map(|x| x.name.as_str()).collect();
    assert_eq!(names, vec!["alpha", "bravo"], "zero filtered, order kept");
}

#[test]
fn memories_sorted_desc_by_tokens() {
    let mut t = trace(100_000, 1_000, 1_000, 500, 0, 0);
    t.memory.memories_selected = vec![
        MemorySelection {
            memory_id: "a".into(),
            memory_type: "semantic".into(),
            content_preview: "small".into(),
            relevance_score: 0.9,
            tokens: 50,
            source: MemorySource::Memoria,
        },
        MemorySelection {
            memory_id: "b".into(),
            memory_type: "semantic".into(),
            content_preview: "big".into(),
            relevance_score: 0.6,
            tokens: 400,
            source: MemorySource::Memoria,
        },
        MemorySelection {
            memory_id: "c".into(),
            memory_type: "semantic".into(),
            content_preview: "medium".into(),
            relevance_score: 0.8,
            tokens: 150,
            source: MemorySource::Memoria,
        },
    ];
    let b = ContextBreakdown::from_trace(&t);
    let previews: Vec<&str> = b.memories.iter().map(|m| m.preview.as_str()).collect();
    assert_eq!(previews, vec!["big", "medium", "small"]);
}

#[test]
fn skills_sorted_desc_by_tokens() {
    let mut t = trace(100_000, 1_000, 1_000, 0, 0, 0);
    t.system_prompt = SystemPromptBreakdown {
        skills_injected: vec![
            SkillInjection {
                skill_name: "tiny".into(),
                skill_version: None,
                tokens: 20,
                selection_reason: String::new(),
            },
            SkillInjection {
                skill_name: "huge".into(),
                skill_version: None,
                tokens: 500,
                selection_reason: String::new(),
            },
        ],
        ..SystemPromptBreakdown::default()
    };
    let b = ContextBreakdown::from_trace(&t);
    let names: Vec<&str> = b.skills.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["huge", "tiny"]);
}

// ─── History summary ───────────────────────────────────────────────

#[test]
fn history_summary_counts_retained_compressed_dropped() {
    let mut t = trace(100_000, 1_000, 5_000, 0, 0, 0);
    t.history = HistorySelectionTrace {
        total_turns_available: 10,
        turns_retained: vec![TurnRetention {
            turn_index: 0,
            role: "user".into(),
            tokens: 200,
            has_tool_calls: false,
            content_preview: String::new(),
        }],
        turns_compressed: vec![
            TurnCompression {
                turn_index: 1,
                role: "assistant".into(),
                original_tokens: 400,
                compressed_tokens: 80,
                compression_method: CompressionMethod::ReactiveCompact,
                information_lost: Vec::new(),
            },
            TurnCompression {
                turn_index: 2,
                role: "assistant".into(),
                original_tokens: 300,
                compressed_tokens: 60,
                compression_method: CompressionMethod::ReactiveCompact,
                information_lost: Vec::new(),
            },
        ],
        turns_dropped: vec![3, 4, 5],
        compression_stages: vec![],
        compression_ratio: 0.2,
        tokens_before: 10_000,
        tokens_after: 2_000,
    };
    let b = ContextBreakdown::from_trace(&t);
    assert_eq!(b.history.total_turns, 10);
    assert_eq!(b.history.retained, 1);
    assert_eq!(b.history.compressed, 2);
    assert_eq!(b.history.dropped, 3);
    assert_eq!(b.history.tokens_before, 10_000);
    assert_eq!(b.history.tokens_after, 2_000);
}

#[test]
fn history_summary_is_empty_when_trace_has_no_history_data() {
    let t = trace(100_000, 1_000, 5_000, 0, 0, 0);
    let b = ContextBreakdown::from_trace(&t);
    assert!(b.history.is_empty());
}

// ─── Memory focus ─────────────────────────────────────────────────

#[test]
fn memory_focus_carries_query_candidates_and_latency() {
    let mut t = trace(100_000, 1_000, 0, 1_000, 0, 0);
    t.memory.query = "benchmark optimization".into();
    t.memory.candidates_considered = 42;
    t.memory.retrieval_latency_ms = 87;
    let b = ContextBreakdown::from_trace(&t);
    assert_eq!(b.memory_focus.query, "benchmark optimization");
    assert_eq!(b.memory_focus.candidates_considered, 42);
    assert_eq!(b.memory_focus.retrieval_latency_ms, 87);
}

#[test]
fn memory_rejection_reasons_render_human_readable() {
    let mut t = trace(100_000, 1_000, 0, 500, 0, 0);
    t.memory.memories_rejected = vec![
        MemoryRejection {
            memory_id: "m-low".into(),
            relevance_score: 0.3,
            rejection_reason: RejectionReason::BelowThreshold {
                threshold: 0.5,
                score: 0.3,
            },
        },
        MemoryRejection {
            memory_id: "m-big".into(),
            relevance_score: 0.9,
            rejection_reason: RejectionReason::TokenBudgetExceeded {
                available: 200,
                required: 800,
            },
        },
        MemoryRejection {
            memory_id: "m-dup".into(),
            relevance_score: 0.8,
            rejection_reason: RejectionReason::Duplicate {
                of_memory_id: "m-kept".into(),
            },
        },
        MemoryRejection {
            memory_id: "m-old".into(),
            relevance_score: 0.7,
            rejection_reason: RejectionReason::Stale { age_days: 120 },
        },
    ];
    let b = ContextBreakdown::from_trace(&t);
    let reasons: Vec<&str> = b
        .memory_focus
        .rejected
        .iter()
        .map(|r| r.reason.as_str())
        .collect();
    assert!(reasons[0].contains("below threshold"));
    assert!(reasons[1].contains("token budget"));
    assert!(reasons[2].contains("duplicate of m-kept"));
    assert!(reasons[3].contains("stale"));
}

#[test]
fn repository_memories_lifted_from_system_prompt() {
    let mut t = trace(100_000, 2_000, 0, 500, 0, 0);
    t.system_prompt = SystemPromptBreakdown {
        repository_memories: vec![
            MemoryInjection {
                memory_id: "repo-1".into(),
                memory_type: "repository".into(),
                tokens: 180,
                relevance_score: 0.85,
                content_preview: "# Project guide".into(),
            },
            MemoryInjection {
                memory_id: "zero".into(),
                memory_type: "repository".into(),
                tokens: 0,
                relevance_score: 0.5,
                content_preview: "".into(),
            },
        ],
        ..SystemPromptBreakdown::default()
    };
    let b = ContextBreakdown::from_trace(&t);
    let ids: Vec<&str> = b
        .memory_focus
        .repository_injected
        .iter()
        .map(|r| r.memory_id.as_str())
        .collect();
    assert_eq!(ids, vec!["repo-1"], "zero-token entries filtered");
}

// ─── Prompt signals ────────────────────────────────────────────────

#[test]
fn prompt_signals_flip_matches_trace_flags() {
    let mut t = trace(100_000, 1_000, 0, 0, 0, 0);
    t.system_prompt.context_signals = PromptContextSignals {
        memory_signal_detected: true,
        ..PromptContextSignals::default()
    };
    t.system_prompt.guidance_signals = PromptGuidanceSignals {
        parallel_batching_nudge: true,
        ..PromptGuidanceSignals::default()
    };
    let b = ContextBreakdown::from_trace(&t);
    let names: Vec<&str> = b.prompt_signals.iter().map(|s| s.name).collect();
    assert!(names.contains(&"memory_signal_detected"));
    assert!(names.contains(&"parallel_batching_nudge"));
    assert_eq!(names.len(), 2, "only set flags should appear: {names:?}");
}

#[test]
fn prompt_signals_empty_when_no_flags_set() {
    let t = trace(100_000, 1_000, 0, 0, 0, 0);
    let b = ContextBreakdown::from_trace(&t);
    assert!(b.prompt_signals.is_empty());
}

// ─── Decisions ─────────────────────────────────────────────────────

#[test]
fn decisions_populated_from_explanations() {
    let mut t = trace(100_000, 1_000, 0, 0, 0, 0);
    t.explanations = vec![DecisionExplanation {
        decision_type: DecisionType::ToolSurface {
            visible_tools: vec!["bash".into(), "read_file".into()],
        },
        reasoning: "Query mentioned files and shell commands.".into(),
        alternatives_considered: vec![Alternative {
            description: "grep-only shortlist".into(),
            score: 0.4,
            why_not_chosen: "user's prompt referenced shell execution".into(),
        }],
        confidence: 0.82,
    }];
    let b = ContextBreakdown::from_trace(&t);
    assert_eq!(b.decisions.len(), 1);
    let d = &b.decisions[0];
    assert!(d.label.contains("Tool surface"));
    assert!(d.label.contains("bash"));
    assert!((d.confidence - 0.82).abs() < 0.001);
    assert_eq!(d.alternatives.len(), 1);
}

// ─── Session summary ───────────────────────────────────────────────

// ─── ActiveSkill fallback + Compaction section ──────────────────

#[test]
fn skills_fall_back_to_last_turn_selected_skills_before_active_system_skills() {
    use super::model::{ActiveSkill, ContextSnapshot};
    let t = trace(100_000, 1_000, 0, 0, 0, 0);
    let mut snap = ContextSnapshot::default();
    snap.selected_skills = vec!["review_changes".into(), "verify_task".into()];
    snap.active_skills = vec![ActiveSkill {
        name: "loaded_only".into(),
        description: "loaded".into(),
    }];
    let b = ContextBreakdown::from_trace_with(&t, &snap);
    let names: Vec<&str> = b.skills.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["review_changes", "verify_task"]);
    assert!(
        b.skills
            .iter()
            .all(|s| s.source.as_deref() == Some("selected")),
        "selected-skill fallback should outrank loaded-skill fallback"
    );
}

#[test]
fn skills_fall_back_to_active_system_skills_when_trace_silent() {
    use super::model::{ActiveSkill, ContextSnapshot};
    let t = trace(100_000, 1_000, 0, 0, 0, 0);
    let mut snap = ContextSnapshot::default();
    snap.active_skills = vec![
        ActiveSkill {
            name: "concise".into(),
            description: "Keep replies short".into(),
        },
        ActiveSkill {
            name: "markdown".into(),
            description: "Output markdown".into(),
        },
    ];
    let b = ContextBreakdown::from_trace_with(&t, &snap);
    let names: Vec<&str> = b.skills.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["concise", "markdown"]);
    assert!(
        b.skills.iter().all(|s| s.tokens == 0),
        "ActiveSkill fallback carries no token counts"
    );
}

#[test]
fn active_system_skills_do_not_override_trace_injected_skills() {
    use super::model::{ActiveSkill, ContextSnapshot};
    let mut t = trace(100_000, 1_000, 0, 0, 0, 0);
    t.system_prompt = SystemPromptBreakdown {
        skills_injected: vec![SkillInjection {
            skill_name: "real_from_trace".into(),
            skill_version: None,
            tokens: 200,
            selection_reason: String::new(),
        }],
        ..SystemPromptBreakdown::default()
    };
    let mut snap = ContextSnapshot::default();
    snap.active_skills = vec![ActiveSkill {
        name: "fallback_ignored".into(),
        description: String::new(),
    }];
    let b = ContextBreakdown::from_trace_with(&t, &snap);
    let names: Vec<&str> = b.skills.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["real_from_trace"]);
}

#[test]
fn compaction_section_empty_when_no_events_and_not_triggered() {
    let t = trace(100_000, 1_000, 0, 0, 0, 0);
    let b = ContextBreakdown::from_trace(&t);
    assert!(b.compaction.is_empty());
}

#[test]
fn compaction_section_populated_from_trace_and_snapshot() {
    use super::model::ContextSnapshot;
    let mut t = trace(100_000, 1_000, 10_000, 0, 0, 0);
    t.token_budget.compression_triggered = true;
    t.history.tokens_before = 15_000;
    t.history.tokens_after = 8_000;
    t.history.turns_compressed = vec![TurnCompression {
        turn_index: 3,
        role: "assistant".into(),
        original_tokens: 5_000,
        compressed_tokens: 500,
        compression_method: CompressionMethod::ReactiveCompact,
        information_lost: vec![
            "Tool outputs from turn 3 were truncated".into(),
            "Assistant reasoning shortened to 1 sentence".into(),
        ],
    }];
    let mut snap = ContextSnapshot::default();
    snap.compressed_turns = vec![3, 7, 12];
    let b = ContextBreakdown::from_trace_with(&t, &snap);
    assert!(b.compaction.triggered_this_turn);
    assert_eq!(b.compaction.compressed_turns, vec![3, 7, 12]);
    assert_eq!(b.compaction.events.len(), 1);
    let e = &b.compaction.events[0];
    assert_eq!(e.turn_index, 3);
    assert!(e.method.contains("ReactiveCompact"));
    assert_eq!(e.original_tokens, 5_000);
    assert_eq!(e.information_lost.len(), 2);
    assert_eq!(b.compaction.tokens_saved(), 7_000);
}

#[test]
fn session_summary_flows_through_snapshot() {
    use super::model::{ContextSnapshot, SessionSummary};
    let t = trace(100_000, 1_000, 0, 0, 0, 0);
    let mut snap = ContextSnapshot::default();
    snap.session = Some(SessionSummary {
        session_id: "abcdef12-full-uuid".into(),
        turn: 5,
        model: Some("test-model-x".into()),
        total_cost: 0.1234,
        max_budget: 1.0,
        prompt_tokens: 12_000,
        completion_tokens: 3_000,
        cache_read_tokens: 8_000,
        cache_creation_tokens: 500,
        request_context: None,
        continuation_anchor: Some("refactoring auth".into()),
        queued_message: None,
        diagnostics_context: None,
        read_activity: Default::default(),
    });
    let b = ContextBreakdown::from_trace_with(&t, &snap);
    let s = b.session_summary.expect("session populated");
    assert_eq!(s.turn, 5);
    assert!((s.total_cost - 0.1234).abs() < 1e-9);
    assert_eq!(s.continuation_anchor.as_deref(), Some("refactoring auth"));
}

#[test]
fn system_sections_populated_from_scalar_fields_and_zero_filtered() {
    let mut t = trace(100_000, 5_000, 1_000, 0, 0, 0);
    t.system_prompt = SystemPromptBreakdown {
        base_persona_tokens: 1_200,
        environment_tokens: 800,
        user_preferences_tokens: 0,
        ..SystemPromptBreakdown::default()
    };
    let b = ContextBreakdown::from_trace(&t);
    let names: Vec<&str> = b.system_sections.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["Persona", "Environment"]);
}
