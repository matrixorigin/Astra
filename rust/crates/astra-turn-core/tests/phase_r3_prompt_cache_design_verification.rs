//! Phase R3 — prompt-cache design-verification matrix.
//!
//! ## Goal
//!
//! Plan item 4: "prompt cache — acknowledge the context-related bits are
//! mostly LLM-independent; mock the LLM's usage reply and verify the
//! cache semantics the *context layer* is supposed to guarantee."
//!
//! This file pins the **dispatch-level contract** for cache-token
//! accounting across a multi-turn sequence driven by mocked usage
//! events. No LLM involvement required — we're verifying that once the
//! usage shape is correct (see Phase R2), the accumulator model
//! represents cold vs. warm turns the way the rest of the system
//! expects.
//!
//! ## Design contract being pinned
//!
//! 1. **Cold turn** = fresh session, no cache hits yet:
//!    `cache_read_tokens == 0`, `cache_creation_tokens > 0` (prompt was
//!    newly ingested into cache).
//! 2. **Warm turn** = subsequent turn in the same session reusing
//!    cached prompt prefix: `cache_read_tokens > 0`, creation may be
//!    `0` (no new cacheable content) or small (delta).
//! 3. **Each turn's `ChatTurnSseAccum` is independent** — the
//!    accumulator is not shared across turns, so cache accounting pins
//!    per-turn values, not cumulative ones.
//! 4. **Missing cache fields default to 0** without erroring — this is
//!    critical for providers that don't report cache tokens at all.
//! 5. **Null-valued cache fields are tolerated** — same as missing.
//!
//! If the dispatch's usage handler changes and violates any of these,
//! downstream cost accounting and cache-effectiveness telemetry will
//! silently drift.

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

/// Contract (1): cold turn — no cache reads, some cache creation.
#[test]
fn cold_turn_usage_populates_creation_only() {
    let cold = json!({
        "type": "usage",
        "prompt_tokens": 1000u64,
        "completion_tokens": 200u64,
        "cache_read_tokens": 0u64,
        "cache_creation_tokens": 800u64,
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

/// Contract (2): warm turn — cache reads dominate, creation small/zero.
#[test]
fn warm_turn_usage_populates_reads_dominant() {
    let warm = json!({
        "type": "usage",
        "prompt_tokens": 1200u64,
        "completion_tokens": 150u64,
        "cache_read_tokens": 1000u64,
        "cache_creation_tokens": 0u64,
    });
    let accum = drive(&warm);
    assert!(accum.has_usage);
    assert_eq!(accum.cache_read_tokens, 1000);
    assert_eq!(accum.cache_creation_tokens, 0);
    assert!(
        accum.cache_read_tokens < accum.prompt_tokens,
        "cache_read should count toward prompt_tokens; the \
         non-cached delta is the difference (anthropic/openai convention)"
    );
}

/// Contract (3): each dispatch call starts fresh — accumulator isn't
/// shared across turns. If a test ever shares `ChatTurnSseAccum` across
/// turns thinking it stacks cache totals, this pin catches the misuse.
#[test]
fn per_turn_accum_is_independent_of_prior_turn() {
    let cold = json!({
        "type": "usage",
        "prompt_tokens": 500u64,
        "completion_tokens": 100u64,
        "cache_read_tokens": 0u64,
        "cache_creation_tokens": 400u64,
    });
    let accum1 = drive(&cold);
    assert_eq!(accum1.cache_creation_tokens, 400);

    // Second turn: if someone passed the SAME accum to a second dispatch
    // call it would OVERWRITE (not sum), which is the intended contract.
    let warm = json!({
        "type": "usage",
        "prompt_tokens": 600u64,
        "completion_tokens": 120u64,
        "cache_read_tokens": 500u64,
        "cache_creation_tokens": 0u64,
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

/// Contract (4): absent cache fields default to 0 silently — providers
/// like the local/mock LLM or older OpenAI responses omit them.
#[test]
fn missing_cache_fields_default_to_zero() {
    let no_cache = json!({
        "type": "usage",
        "prompt_tokens": 200u64,
        "completion_tokens": 50u64,
    });
    let accum = drive(&no_cache);
    assert!(accum.has_usage);
    assert_eq!(accum.cache_read_tokens, 0);
    assert_eq!(accum.cache_creation_tokens, 0);
}

/// Contract (5): null-valued cache fields are tolerated without blowing
/// up `has_usage`. This is the shape Anthropic emits when the cache
/// control block isn't negotiated.
#[test]
fn null_cache_fields_are_tolerated() {
    let nullish = json!({
        "type": "usage",
        "prompt_tokens": 200u64,
        "completion_tokens": 50u64,
        "cache_read_tokens": Value::Null,
        "cache_creation_tokens": Value::Null,
    });
    let accum = drive(&nullish);
    assert!(accum.has_usage);
    assert_eq!(accum.cache_read_tokens, 0);
    assert_eq!(accum.cache_creation_tokens, 0);
}

/// Contract (6): if BOTH prompt_tokens AND completion_tokens are
/// missing, the usage event is treated as invalid and `has_usage`
/// stays false; an error message is recorded. This is the guard that
/// prevents a malformed provider response from silently zeroing out
/// cost accounting.
#[test]
fn usage_missing_both_token_counts_is_rejected() {
    let bad = json!({
        "type": "usage",
        "cache_read_tokens": 999u64,
        "cache_creation_tokens": 999u64,
    });
    let accum = drive(&bad);
    assert!(
        !accum.has_usage,
        "usage with neither prompt nor completion tokens is invalid"
    );
    // Cache fields should NOT have leaked in from the invalid payload.
    assert_eq!(
        accum.cache_read_tokens, 0,
        "invalid usage must not partially populate cache fields — \
         doing so would make cost reports claim cache hits on \
         impossible turns"
    );
    assert_eq!(accum.cache_creation_tokens, 0);
    assert!(accum.error_message.is_some());
}

/// Contract (7): hallucinated/negative cache values — `as_u64()` fails
/// on negative ints, so they fall back to 0 (no panic, no arithmetic
/// overflow). Pin the silent-zero behaviour.
#[test]
fn negative_cache_values_fall_back_to_zero() {
    let negatives = json!({
        "type": "usage",
        "prompt_tokens": 100u64,
        "completion_tokens": 50u64,
        "cache_read_tokens": -5,
        "cache_creation_tokens": -10,
    });
    let accum = drive(&negatives);
    assert!(accum.has_usage);
    assert_eq!(
        accum.cache_read_tokens, 0,
        "negative cache_read_tokens must not panic and must not \
         become a huge u64 via wrap-around"
    );
    assert_eq!(accum.cache_creation_tokens, 0);
}

/// Contract (8): only one of prompt_tokens or completion_tokens is
/// present — the usage event is STILL considered valid; the missing
/// one defaults to 0. This mirrors how some completion APIs omit
/// `prompt_tokens` for streaming-only accounting.
#[test]
fn partial_usage_with_only_one_token_count_is_accepted() {
    let only_completion = json!({
        "type": "usage",
        "completion_tokens": 50u64,
    });
    let accum = drive(&only_completion);
    assert!(
        accum.has_usage,
        "completion-only usage is valid (streaming accounting)"
    );
    assert_eq!(accum.prompt_tokens, 0);
    assert_eq!(accum.completion_tokens, 50);
}
