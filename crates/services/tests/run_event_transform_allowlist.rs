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

#[test]
fn runtime_feedback_projects_only_the_server_authored_frame() {
    let frame = json!({
        "schema_version": 4,
        "identity": {
            "session_id": "session-1",
            "run_id": "run-1",
            "topology": "cli_server"
        }
    });
    let out = transform_run_event_for_client(json!({
        "type": "runtime_feedback",
        "runtime_feedback": frame,
        "internal_diagnostic": "must not cross the client boundary"
    }));
    assert_eq!(
        out,
        json!({
            "type": "runtime_feedback",
            "runtime_feedback": frame,
        })
    );

    assert!(
        transform_run_event_for_client(json!({"type": "runtime_feedback"})).is_null(),
        "a missing canonical frame must not become an empty public observation"
    );
    assert!(
        transform_run_event_for_client(json!({
            "type": "runtime_feedback",
            "runtime_feedback": "not-a-frame"
        }))
        .is_null(),
        "a non-object frame must fail closed at the public boundary"
    );
}

#[test]
fn known_agent_interrupted_still_passes_through() {
    let ok = json!({
        "type": "agent_interrupted",
        "agent_id": "agent-1",
        "reason": "budget_exhausted"
    });
    let out = transform_run_event_for_client(ok);
    assert!(!out.is_null(), "agent_interrupted must pass through");
    let obj = out.as_object().expect("object");
    assert_eq!(
        obj.get("type").and_then(Value::as_str),
        Some("agent_interrupted")
    );
}
