//! Phase R3 — prompt-cache design-verification matrix.
//!
//! Pins the **dispatch-level contract** for cache-token accounting across
//! a multi-turn sequence driven by mocked usage events. The wire shape
//! is produced by [`astra_runtime::turn::token_usage::TokenUsage::to_json_map`]
//! and consumed by [`dispatch_chat_turn_sse_event_block`]. The fields are:
//!
//! - `input_tokens`          — fresh input (billed full rate)
//! - `cached_input_tokens`   — cache read (discount rate)
//! - `cache_creation_tokens` — cache write (premium rate)
//! - `output_tokens`         — output
//! - `total_tokens`          — sum (canonical; derived on send)
//!
//! Local `ChatTurnSseAccum` maps `input_tokens → prompt_tokens`,
//! `output_tokens → completion_tokens`, `cached_input_tokens →
//! cache_read_tokens` (legacy local field names retained).
//!
//! ## Contract being pinned
//!
//! 1. **Cold turn**: `cache_read_tokens == 0`, `cache_creation_tokens > 0`.
//! 2. **Warm turn**: `cache_read_tokens > 0`, creation may be `0`.
//! 3. Each turn's accumulator is **independent** (not cumulative).
//! 4. **Missing cache fields default to 0** without erroring.
//! 5. **Null-valued cache fields are tolerated** — same as missing.
//! 6. Usage event missing BOTH `input_tokens` AND `output_tokens` is
//!    **rejected** (`has_usage == false`, error recorded).
//! 7. Negative cache values fall back to 0 (no panic, no wrap).

use astra_turn_core::chat_turn_sse_dispatch::{
    ChatTurnEdgePending, ChatTurnSseAccum, dispatch_chat_turn_sse_event_block,
};
use serde_json::{Value, json};

fn sse_block(event: &Value) -> String {
    format!("data: {}\n\n", event)
}

fn drive(event: &Value) -> ChatTurnSseAccum {
    let mut accum = ChatTurnSseAccum::default();
    let mut edge: Vec<ChatTurnEdgePending> = Vec::new();
    dispatch_chat_turn_sse_event_block(&sse_block(event), &mut accum, &mut edge);
    accum
}

#[test]
fn cold_turn_usage_populates_creation_only() {
    let cold = json!({
        "type": "usage",
        "input_tokens": 1000u64,
        "cached_input_tokens": 0u64,
        "cache_creation_tokens": 800u64,
        "output_tokens": 200u64,
        "total_tokens": 2000u64,
    });
    let accum = drive(&cold);
    assert!(accum.has_usage);
    assert_eq!(accum.prompt_tokens, 1000);
    assert_eq!(accum.completion_tokens, 200);
    assert_eq!(
        accum.cache_read_tokens, 0,
        "cold turn: nothing in cache yet, read must be 0"
    );
    assert_eq!(
        accum.cache_creation_tokens, 800,
        "cold turn: prefix must be ingested into cache"
    );
}

#[test]
fn warm_turn_usage_populates_reads_dominant() {
    let warm = json!({
        "type": "usage",
        "input_tokens": 200u64,
        "cached_input_tokens": 1000u64,
        "cache_creation_tokens": 0u64,
        "output_tokens": 150u64,
        "total_tokens": 1350u64,
    });
    let accum = drive(&warm);
    assert!(accum.has_usage);
    assert_eq!(accum.cache_read_tokens, 1000);
    assert_eq!(accum.cache_creation_tokens, 0);
    // Canonical contract: `input_tokens` is DISJOINT from `cached_input_tokens`
    // (unlike OpenAI's raw API which bundles them). After normalization, fresh
    // input is smaller than cached input on a warm turn.
    assert!(
        accum.cache_read_tokens > accum.prompt_tokens,
        "warm turn: cache_read dominates fresh input_tokens"
    );
}

#[test]
fn per_turn_accum_is_independent_of_prior_turn() {
    let cold = json!({
        "type": "usage",
        "input_tokens": 500u64,
        "cached_input_tokens": 0u64,
        "cache_creation_tokens": 400u64,
        "output_tokens": 100u64,
        "total_tokens": 1000u64,
    });
    let accum1 = drive(&cold);
    assert_eq!(accum1.cache_creation_tokens, 400);

    let warm = json!({
        "type": "usage",
        "input_tokens": 100u64,
        "cached_input_tokens": 500u64,
        "cache_creation_tokens": 0u64,
        "output_tokens": 120u64,
        "total_tokens": 720u64,
    });
    let mut shared = accum1;
    let mut edge: Vec<ChatTurnEdgePending> = Vec::new();
    dispatch_chat_turn_sse_event_block(&sse_block(&warm), &mut shared, &mut edge);

    assert_eq!(
        shared.cache_read_tokens, 500,
        "second dispatch overwrites, does not accumulate"
    );
    assert_eq!(
        shared.cache_creation_tokens, 0,
        "second dispatch overwrites the creation field too — if this \
         read 400, the dispatch accidentally started summing and cache \
         telemetry is now wrong"
    );
}

#[test]
fn missing_cache_fields_default_to_zero() {
    let no_cache = json!({
        "type": "usage",
        "input_tokens": 200u64,
        "output_tokens": 50u64,
    });
    let accum = drive(&no_cache);
    assert!(accum.has_usage);
    assert_eq!(accum.cache_read_tokens, 0);
    assert_eq!(accum.cache_creation_tokens, 0);
}

#[test]
fn null_cache_fields_are_tolerated() {
    let nullish = json!({
        "type": "usage",
        "input_tokens": 200u64,
        "output_tokens": 50u64,
        "cached_input_tokens": Value::Null,
        "cache_creation_tokens": Value::Null,
    });
    let accum = drive(&nullish);
    assert!(accum.has_usage);
    assert_eq!(accum.cache_read_tokens, 0);
    assert_eq!(accum.cache_creation_tokens, 0);
}

#[test]
fn usage_missing_both_token_counts_is_rejected() {
    let bad = json!({
        "type": "usage",
        "cached_input_tokens": 999u64,
        "cache_creation_tokens": 999u64,
    });
    let accum = drive(&bad);
    assert!(
        !accum.has_usage,
        "usage with neither input nor output tokens is invalid"
    );
    assert_eq!(
        accum.cache_read_tokens, 0,
        "invalid usage must not partially populate cache fields"
    );
    assert_eq!(accum.cache_creation_tokens, 0);
    assert!(accum.error_message.is_some());
}

#[test]
fn negative_cache_values_fall_back_to_zero() {
    let negatives = json!({
        "type": "usage",
        "input_tokens": 100u64,
        "output_tokens": 50u64,
        "cached_input_tokens": -5,
        "cache_creation_tokens": -10,
    });
    let accum = drive(&negatives);
    assert!(accum.has_usage);
    assert_eq!(accum.cache_read_tokens, 0);
    assert_eq!(accum.cache_creation_tokens, 0);
}

#[test]
fn partial_usage_with_only_one_token_count_is_accepted() {
    let only_output = json!({
        "type": "usage",
        "output_tokens": 50u64,
    });
    let accum = drive(&only_output);
    assert!(
        accum.has_usage,
        "output-only usage is valid (streaming accounting)"
    );
    assert_eq!(accum.prompt_tokens, 0);
    assert_eq!(accum.completion_tokens, 50);
}
