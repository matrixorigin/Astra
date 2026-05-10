//! ContextBreakdown contract (RED).

#![cfg(test)]

use super::model::{CategoryKind, ContextBreakdown, PressureBand};
use astra_turn_core::context_assembly_trace::TokenBudgetTrace;

fn trace(max: u32, sys: u32, hist: u32, mem: u32, tools: u32, user: u32) -> TokenBudgetTrace {
    let total = sys + hist + mem + tools + user;
    let pressure = if max == 0 {
        0.0
    } else {
        total as f64 / max as f64
    };
    TokenBudgetTrace {
        max_tokens: max,
        system_prompt_tokens: sys,
        history_tokens: hist,
        memory_tokens: mem,
        tool_schema_tokens: tools,
        user_message_tokens: user,
        total_used: total,
        budget_pressure: pressure,
        compression_triggered: false,
    }
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
