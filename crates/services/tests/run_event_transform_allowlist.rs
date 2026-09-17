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

#[test]
fn typed_tool_execution_fact_survives_live_and_replay_projection() {
    // A pre-admission rejection is terminal even when the human-readable
    // result is just an error string.  The typed fact must survive both the
    // live client-shaped path and the durable event replay path.
    let live = transform_run_event_for_client(json!({
        "type": "tool_call_end",
        "call_id": "call-stale",
        "tool": "agent_fanout",
        "status": "rejected",
        "success": false,
        "executed": false,
        "result": "deferred tool descriptor is stale",
        "internal_diagnostic": "must not cross the boundary",
    }));
    assert_eq!(live["executed"], false);
    assert!(live.get("internal_diagnostic").is_none());

    let replay = transform_run_event_for_client(json!({
        "event_type": "tool_result",
        "data": {
            "tool_call_id": "call-stale",
            "name": "agent_fanout",
            "status": "rejected",
            "success": false,
            "executed": false,
            "output": "deferred tool descriptor is stale",
        },
    }));
    assert_eq!(replay["type"], "tool_call_end");
    assert_eq!(replay["call_id"], "call-stale");
    assert_eq!(replay["executed"], false);
}

#[test]
fn oversized_terminal_preserves_unknown_execution_as_distinct_from_missing() {
    let out = transform_run_event_for_client(json!({
        "type": "tool_call_end",
        "call_id": "call-unknown",
        "tool": "agent_fanout",
        "status": "failed",
        "success": false,
        "executed": null,
        "result": "Execution outcome could not be confirmed",
        "executor": {"extension": "x".repeat(128 * 1024)},
    }));
    assert_eq!(out["payload_truncated"], true);
    assert_eq!(out.get("executed"), Some(&Value::Null));
}

#[test]
fn reused_terminal_disposition_survives_live_replay_and_size_projection() {
    let terminal = json!({
        "call_id": "call-reused", "tool": "agent_fanout",
        "status": "completed", "success": true,
        "executed": false, "disposition": "reused",
        "result": {"group_id": "existing-group", "executed": true},
    });
    for oversized in [false, true] {
        let mut data = terminal.clone();
        if oversized {
            data["executor"] = json!({"extension": "x".repeat(128 * 1024)});
        }
        let replay = json!({"event_type": "tool_result", "data": data});
        data["type"] = json!("tool_call_end");
        for event in [data, replay] {
            let out = transform_run_event_for_client(event);
            assert_eq!(out["disposition"], "reused");
            assert_eq!(out["executed"], false);
            assert_eq!(out["result"]["group_id"], "existing-group");
            assert_eq!(out["result"]["executed"], true);
        }
    }
}

#[test]
fn bounded_lifecycle_summary_never_changes_control_identity() {
    let group_id = "g".repeat(1500);
    let out = transform_run_event_for_client(json!({
        "type": "tool_call_end", "call_id": "call-large-group",
        "tool": "agent_fanout", "executed": true,
        "result": {
            "status": "completed", "group_id": group_id,
            "results": ["x".repeat(70_000)],
            "work_unit_observation": {
                "id": group_id, "kind": "agent_fanout", "status": "completed",
                "revision": 1, "mode": "current", "wake_policy": "none"
            }
        }
    }));
    assert_eq!(out["result"]["truncated"], true);
    assert_eq!(out["result"]["group_id"], group_id);
    assert_eq!(out["result"]["work_unit_observation"]["id"], group_id);
}
