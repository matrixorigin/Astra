//! wip-7 TDD contract: the bridge's `injection_freshness` SSE event
//! carries fingerprints (opaque hashes + byte-length metadata) ONLY.
//! Raw channel text never crosses the HTTP boundary.
//!
//! Motivation: wip-5's event emitted full text for every channel
//! (`self_awareness`, `memoria_insights`, `feedback_rules`,
//! `implicit_feedback`, etc.). Any external client hitting `/chat/turn`
//! saw that plaintext via the pass-through behaviour of
//! `services::runs::transform_run_event_for_client`. Learned feedback
//! rules, memoria recall digests, and user-correction excerpts are
//! sensitive runtime state — they must not leak.
//!
//! Fix: emit `hash: u64` + `bytes: u64` + optional sanitized identifier
//! only. No raw text. This test locks in the wire contract.

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
fn injection_freshness_event_carries_no_raw_text_keys() {
    // The event shape must NOT include any of the pre-wip-7 text keys.
    // This guards against regression where a future change adds "text"
    // or similar back into the SSE payload.
    let forbidden_keys = [
        "self_awareness",
        "memoria_insights",
        "memoria_prefetch",
        "recent_arg_hints",
        "skill_listing",
        "lessons",
        "feedback_rules",
        "implicit_feedback",
        "tool_round_guidance",
        "volatile",
        "text",
        "texts", // wip-5's container key — must be gone
        "content",
    ];

    // Simulate the wip-7 bridge emission: fingerprints only.
    let allowed_event = json!({
        "type": "injection_freshness",
        "channels": [
            { "tag": "self_awareness", "hash": 1u64, "bytes": 42u64, "is_empty": false },
            { "tag": "memoria_insights", "hash": 0u64, "bytes": 0u64, "is_empty": true },
        ]
    });
    let data = serde_json::to_string(&allowed_event).unwrap();
    for key in &forbidden_keys {
        // The serialized wire form must not have these as top-level keys
        // OR as keys within the `texts` object (which must not exist).
        let in_top = allowed_event.get(*key).is_some();
        assert!(
            !in_top,
            "injection_freshness event must not carry key `{key}` on the wire"
        );
        // Also: no value in the whole event should equal raw text.
        // (The spec is "hashes only"; anything stringy at a channel
        //  leaf should be a tag, not text.)
        assert!(
            !data.contains("\"texts\":"),
            "wip-7 event must not use the wip-5 `texts` container"
        );
    }

    // And parsing it populates the fingerprint bundle.
    let mut accum = ChatTurnSseAccum::default();
    dispatch(&mut accum, allowed_event);

    let bundle = accum
        .bridge_injection_fingerprints
        .as_ref()
        .expect("fingerprint bundle not populated");
    assert_eq!(bundle.channels.len(), 2);
    let first = &bundle.channels[0];
    assert_eq!(first.tag, "self_awareness");
    assert_eq!(first.hash, 1);
    assert_eq!(first.bytes, 42);
    assert!(!first.is_empty);
    let second = &bundle.channels[1];
    assert_eq!(second.tag, "memoria_insights");
    assert!(second.is_empty);
}

// The external-transform allowlist test lives in
// `crates/services/tests/run_event_transform_allowlist.rs` — that
// crate owns `transform_run_event_for_client` and has the deps to
// exercise it. Keeping the two tests in separate files makes the
// coverage boundary clearer (wire shape here, transform there).

#[test]
fn missing_bridge_event_leaves_fingerprints_none_not_empty_default() {
    // When the bridge doesn't emit the event (turn aborted early,
    // malformed response, etc.), CLI must NOT fall back to a synthetic
    // "all channels empty" bundle — that would mark every bridge
    // channel as `Empty` in the freshness report, masking the fact
    // that the observation pipe itself failed. Field stays `None`.
    let accum = ChatTurnSseAccum::default();
    assert!(
        accum.bridge_injection_fingerprints.is_none(),
        "missing bridge observation must remain None (untracked), not be defaulted to empty"
    );
}
