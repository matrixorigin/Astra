//! wip-5 TDD contract: bridge emits per-turn injection-channel texts
//! via a dedicated SSE event (`injection_freshness`), CLI SSE dispatch
//! captures them into `ChatTurnSseAccum::bridge_injection_texts`, and
//! the CLI caller forwards them to
//! `ObservabilitySession::observe_bridge_injections`.
//!
//! This closes the gap from wip-4: 5 bridge-internal channels
//! (implicit_feedback, feedback_rules, memoria_prefetch,
//! tool_round_guidance, volatile) had variants in `InjectionChannel`
//! but no observation path — CLI couldn't see them because they're
//! generated inside `bridge_inprocess::forward`.

use astra_turn_core::chat_turn_sse_dispatch::{
    ChatTurnSseAccum, dispatch_chat_turn_sse_event_block,
};
use serde_json::json;

fn dispatch(accum: &mut ChatTurnSseAccum, event: serde_json::Value) {
    let data = serde_json::to_string(&event).unwrap();
    let block = format!("data: {data}\n");
    let mut pending = Vec::new();
    dispatch_chat_turn_sse_event_block(&block, accum, &mut pending);
}

#[test]
fn accum_captures_injection_freshness_event() {
    let mut accum = ChatTurnSseAccum::default();
    dispatch(
        &mut accum,
        json!({
            "type": "injection_freshness",
            "texts": {
                "implicit_feedback": "⚠ user corrected in last turn",
                "feedback_rules": "[Learned] do X not Y",
                "memoria_insights": "[Memoria] recent file ops: ...",
                "memoria_prefetch": "[pre-fetched] 3 memories",
                "self_awareness": "",
                "recent_arg_hints": "",
                "skill_listing": "",
                "lessons": "",
                "tool_round_guidance": "Sequential round #4 — batch next round",
                "volatile": ""
            }
        }),
    );

    let bundle = accum
        .bridge_injection_texts
        .as_ref()
        .expect("bridge_injection_texts not populated — SSE event dropped");

    assert_eq!(bundle.implicit_feedback, "⚠ user corrected in last turn");
    assert_eq!(bundle.feedback_rules, "[Learned] do X not Y");
    assert_eq!(bundle.memoria_insights, "[Memoria] recent file ops: ...");
    assert_eq!(bundle.memoria_prefetch, "[pre-fetched] 3 memories");
    assert_eq!(
        bundle.tool_round_guidance,
        "Sequential round #4 — batch next round"
    );
    assert_eq!(bundle.lessons, "");
    assert_eq!(bundle.volatile, "");
}

#[test]
fn missing_event_leaves_bundle_none() {
    let accum = ChatTurnSseAccum::default();
    assert!(
        accum.bridge_injection_texts.is_none(),
        "default accum should not spuriously report texts"
    );
}

#[test]
fn partial_event_fills_only_present_fields() {
    // A bridge path that doesn't populate every channel (e.g. a
    // request that skipped Memoria prefetch) should leave the absent
    // fields as empty strings, not crash parsing.
    let mut accum = ChatTurnSseAccum::default();
    dispatch(
        &mut accum,
        json!({
            "type": "injection_freshness",
            "texts": {
                "tool_round_guidance": "First round — no guidance"
            }
        }),
    );

    let bundle = accum
        .bridge_injection_texts
        .expect("partial bundle should still populate");
    assert_eq!(bundle.tool_round_guidance, "First round — no guidance");
    assert_eq!(bundle.implicit_feedback, "");
    assert_eq!(bundle.feedback_rules, "");
}
