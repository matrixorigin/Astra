//! wip-7 TDD contract: `transform_run_event_for_client` is an
//! allowlist, not a passthrough. Internal diagnostic events — in
//! particular `injection_freshness` — MUST be dropped before reaching
//! external API clients.
//!
//! Motivation: the pre-wip-7 transform returned any unknown
//! `{"type": ...}`-shaped event unchanged (see the early return at
//! line 541). wip-5's `injection_freshness` event carried raw channel
//! text (self-awareness, learned feedback rules, implicit feedback,
//! memoria recall digests) for observation purposes — that text
//! leaked to any authenticated API caller hitting `/chat/turn`. The
//! fix is two-layered: (a) wip-7 bridge emits fingerprints only, and
//! (b) the transform explicitly drops `injection_freshness` regardless
//! of payload shape so even future diagnostic events don't leak by
//! accident. This test locks in (b).

use astra_services::runs::transform_run_event_for_client;
use serde_json::{Value, json};

#[test]
fn injection_freshness_is_dropped() {
    // Even with only fingerprints on the wire, this event is a
    // diagnostic side-channel whose stability no external API
    // consumer should depend on. Drop it outright.
    let event = json!({
        "type": "injection_freshness",
        "channels": [
            { "tag": "self_awareness", "hash": 1u64, "bytes": 42u64, "is_empty": false }
        ]
    });
    let out = transform_run_event_for_client(event);
    assert!(
        out.is_null(),
        "injection_freshness must be dropped at the external transform boundary, got: {out}"
    );
}

#[test]
fn injection_freshness_with_legacy_texts_shape_also_dropped() {
    // Regression guard: if something (mis-)emits the wip-5 shape with
    // `texts:` carrying raw strings, the transform must still drop it.
    let event = json!({
        "type": "injection_freshness",
        "texts": {
            "self_awareness": "I just failed three bash calls in a row",
            "feedback_rules": "[Learned] user always wants verbose output"
        }
    });
    let out = transform_run_event_for_client(event);
    assert!(
        out.is_null(),
        "legacy-shaped injection_freshness must still be dropped, got: {out}"
    );
}

#[test]
fn unknown_event_type_is_dropped() {
    // Allowlist semantics: anything not in the known set is stripped.
    // This catches future internal events that someone forgets to
    // route via the allowlist.
    let unknown = json!({
        "type": "some_future_internal_event",
        "payload": "should never leave the process"
    });
    let out = transform_run_event_for_client(unknown);
    assert!(
        out.is_null(),
        "unknown event types must be dropped by the allowlist transform, got: {out}"
    );
}

#[test]
fn known_text_delta_still_passes_through() {
    // Sanity check: the allowlist doesn't break legitimate events.
    let ok = json!({
        "type": "text_delta",
        "content": "hello"
    });
    let out = transform_run_event_for_client(ok);
    assert!(!out.is_null(), "text_delta must pass through; got null");
    let obj = out.as_object().expect("object");
    assert_eq!(obj.get("type").and_then(Value::as_str), Some("text_delta"));
}

#[test]
fn known_run_finished_still_passes_through() {
    let ok = json!({
        "type": "run_finished",
        "run_id": "abc"
    });
    let out = transform_run_event_for_client(ok);
    assert!(!out.is_null(), "run_finished must pass through");
}
