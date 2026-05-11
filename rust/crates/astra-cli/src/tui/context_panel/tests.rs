//! ContextBreakdown contract.

#![cfg(test)]

use super::model::{CategoryKind, ContextBreakdown, PressureBand};
use astra_turn_core::context_assembly_trace::{
    ContextAssemblyTrace, MemorySelection, MemorySource, SkillInjection, SystemPromptBreakdown,
    TokenBudgetTrace, ToolSelected,
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
    t.tools.tools_selected = vec![
        ToolSelected {
            tool_name: "alpha".into(),
            score: 0.9,
            tokens: 120,
            selection_factors: Vec::new(),
        },
        ToolSelected {
            tool_name: "zero".into(),
            score: 0.5,
            tokens: 0,
            selection_factors: Vec::new(),
        },
        ToolSelected {
            tool_name: "bravo".into(),
            score: 0.8,
            tokens: 180,
            selection_factors: Vec::new(),
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
