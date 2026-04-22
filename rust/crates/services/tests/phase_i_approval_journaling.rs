//! Phase I: Approval journaling + mid-session permission race —
//! narrower integration slice that exercises the approval/journal surface
//! without the full web_agent_e2e.rs SSE harness. Covers the "mock-LLM
//! records an approval → persistence → next turn sees recorded verdict"
//! path that Phase D/E only touched obliquely.
//!
//! Targets:
//!  - [`astra_services::session_journal::find_latest_approval_decision`]
//!  - [`astra_services::session_journal::find_latest_approval_required`]
//!  - Journal reshape across ApprovalRequired / ApprovalDecision /
//!    ApprovalTimeout events.

use astra_services::session_journal::{
    ApprovalJournalDecision, JournalDirGuard, JournalEvent, JournalEventType, JournalWriter,
    find_latest_approval_decision, find_latest_approval_required, read_journal,
};
use tempfile::tempdir;

fn write_pair(
    writer: &JournalWriter,
    session: &str,
    turn: u32,
    request_id: &str,
    tool: &str,
    decision: &str,
) {
    writer
        .append(&JournalEvent::approval_required(
            Some(session),
            Some(turn),
            request_id,
            tool,
            "standard",
            Some(&format!("{tool} needs approval")),
        ))
        .unwrap();
    writer
        .append(&JournalEvent::approval_decision(
            Some(session),
            Some(turn),
            request_id,
            Some(tool),
            Some("standard"),
            decision,
            Some("user responded via prompt"),
        ))
        .unwrap();
}

#[test]
fn approval_decision_roundtrips_through_journal() {
    let tmp = tempdir().unwrap();
    let _guard = JournalDirGuard::new(tmp.path());
    let writer = JournalWriter::new("sess-appr").unwrap();

    write_pair(&writer, "sess-appr", 1, "req-1", "bash", "allow");

    let found = find_latest_approval_decision("sess-appr", "req-1")
        .unwrap()
        .expect("decision must be readable");
    assert_eq!(
        found,
        ApprovalJournalDecision {
            request_id: "req-1".into(),
            decision: "allow".into(),
            reason: Some("user responded via prompt".into()),
            tool_name: Some("bash".into()),
            approval_kind: Some("standard".into()),
        }
    );
}

#[test]
fn multiple_decisions_for_same_request_last_wins() {
    // Simulates a retry where the same request_id is decided twice
    // (e.g., user changed mind after TUI re-prompt). `find_latest_*`
    // iterates in reverse, so the most recent decision must win.
    let tmp = tempdir().unwrap();
    let _guard = JournalDirGuard::new(tmp.path());
    let writer = JournalWriter::new("sess-retry").unwrap();

    write_pair(&writer, "sess-retry", 1, "req-A", "write_file", "deny");
    write_pair(&writer, "sess-retry", 2, "req-A", "write_file", "allow");

    let found = find_latest_approval_decision("sess-retry", "req-A")
        .unwrap()
        .unwrap();
    assert_eq!(found.decision, "allow", "latest (turn 2) must win");
}

#[test]
fn unrelated_request_ids_do_not_cross_contaminate() {
    let tmp = tempdir().unwrap();
    let _guard = JournalDirGuard::new(tmp.path());
    let writer = JournalWriter::new("sess-multi").unwrap();

    write_pair(&writer, "sess-multi", 1, "req-A", "bash", "allow");
    write_pair(&writer, "sess-multi", 2, "req-B", "write_file", "deny");

    let a = find_latest_approval_decision("sess-multi", "req-A")
        .unwrap()
        .unwrap();
    let b = find_latest_approval_decision("sess-multi", "req-B")
        .unwrap()
        .unwrap();
    assert_eq!(a.decision, "allow");
    assert_eq!(a.tool_name.as_deref(), Some("bash"));
    assert_eq!(b.decision, "deny");
    assert_eq!(b.tool_name.as_deref(), Some("write_file"));
}

#[test]
fn approval_required_without_decision_is_findable_until_decision_lands() {
    let tmp = tempdir().unwrap();
    let _guard = JournalDirGuard::new(tmp.path());
    let writer = JournalWriter::new("sess-pending").unwrap();

    writer
        .append(&JournalEvent::approval_required(
            Some("sess-pending"),
            Some(1),
            "req-pending",
            "bash",
            "standard",
            Some("ls /"),
        ))
        .unwrap();

    let pending = find_latest_approval_required("sess-pending", "req-pending")
        .unwrap()
        .expect("required request must be findable");
    assert_eq!(pending.request_id, "req-pending");

    // No decision yet.
    let no_decision = find_latest_approval_decision("sess-pending", "req-pending").unwrap();
    assert!(
        no_decision.is_none(),
        "decision must not be falsely reported before it's written"
    );

    // Decision lands → readable.
    writer
        .append(&JournalEvent::approval_decision(
            Some("sess-pending"),
            Some(1),
            "req-pending",
            Some("bash"),
            Some("standard"),
            "allow",
            None,
        ))
        .unwrap();
    let decided = find_latest_approval_decision("sess-pending", "req-pending")
        .unwrap()
        .unwrap();
    assert_eq!(decided.decision, "allow");
}

#[test]
fn approval_decision_persists_across_simulated_restart() {
    // Restart-across-session: the journal file is the ground truth, so a
    // brand new JournalWriter handle opening the same session ID must see
    // the previously written decisions.
    let tmp = tempdir().unwrap();
    let _guard = JournalDirGuard::new(tmp.path());

    {
        let writer = JournalWriter::new("sess-restart").unwrap();
        write_pair(&writer, "sess-restart", 1, "req-X", "str_replace", "allow");
        // writer goes out of scope — like process exit.
    }

    // "New process" re-enters and only has the session_id.
    let found = find_latest_approval_decision("sess-restart", "req-X")
        .unwrap()
        .unwrap();
    assert_eq!(found.decision, "allow");
    assert_eq!(found.tool_name.as_deref(), Some("str_replace"));

    let all_events = read_journal("sess-restart").unwrap();
    let required = all_events
        .iter()
        .filter(|e| e.event_type == JournalEventType::ApprovalRequired)
        .count();
    let decided = all_events
        .iter()
        .filter(|e| e.event_type == JournalEventType::ApprovalDecision)
        .count();
    assert_eq!(required, 1);
    assert_eq!(decided, 1);
}

#[test]
fn approval_timeout_recorded_distinctly_from_decision() {
    let tmp = tempdir().unwrap();
    let _guard = JournalDirGuard::new(tmp.path());
    let writer = JournalWriter::new("sess-timeout").unwrap();

    writer
        .append(&JournalEvent::approval_required(
            Some("sess-timeout"),
            Some(1),
            "req-T",
            "bash",
            "standard",
            Some("rm file"),
        ))
        .unwrap();
    writer
        .append(&JournalEvent::approval_timeout(
            Some("sess-timeout"),
            Some(1),
            "req-T",
            "bash",
            "standard",
        ))
        .unwrap();

    // A timeout event is NOT a decision — find_latest_approval_decision
    // must return None so the caller knows to surface "timed out" rather
    // than conflating it with deny.
    assert!(
        find_latest_approval_decision("sess-timeout", "req-T")
            .unwrap()
            .is_none(),
        "timeout must not masquerade as a decision"
    );

    // But the request itself is still findable so UI can explain state.
    assert!(
        find_latest_approval_required("sess-timeout", "req-T")
            .unwrap()
            .is_some()
    );
}

#[test]
fn malformed_approval_metadata_is_skipped_not_errored() {
    // Hand-craft an approval_decision event with a missing decision field
    // (simulating an older/buggy writer). The finder must skip it and
    // return the next well-formed match rather than returning a partial
    // record or panicking.
    let tmp = tempdir().unwrap();
    let _guard = JournalDirGuard::new(tmp.path());
    let writer = JournalWriter::new("sess-malformed").unwrap();

    // Write a well-formed decision first.
    write_pair(&writer, "sess-malformed", 1, "req-good", "bash", "allow");

    // Manually append a malformed decision record for a DIFFERENT request_id.
    let mut bad =
        JournalEvent::base_public(JournalEventType::ApprovalDecision, Some("sess-malformed"));
    bad.metadata = Some(serde_json::json!({
        "approval": {
            "request_id": "req-malformed",
            "tool_name": "bash",
            // decision field intentionally omitted
        }
    }));
    writer.append(&bad).unwrap();

    // Good one still findable.
    let g = find_latest_approval_decision("sess-malformed", "req-good")
        .unwrap()
        .unwrap();
    assert_eq!(g.decision, "allow");

    // Malformed one returns None (not an error, not a panic).
    let m = find_latest_approval_decision("sess-malformed", "req-malformed").unwrap();
    assert!(m.is_none(), "malformed entry must be skipped gracefully");
}

#[test]
fn deny_then_allow_across_turns_picks_up_allow() {
    // Mock-LLM style scenario: the user first denies then, on an
    // explicit re-prompt next turn, allows. Both decisions use the SAME
    // request_id (retry semantics). The next-turn reader must see Allow.
    let tmp = tempdir().unwrap();
    let _guard = JournalDirGuard::new(tmp.path());
    let writer = JournalWriter::new("sess-flip").unwrap();

    // Turn 1 — denied.
    write_pair(&writer, "sess-flip", 1, "req-F", "bash", "deny");
    // Turn 3 — user reconsiders.
    write_pair(&writer, "sess-flip", 3, "req-F", "bash", "allow");

    let latest = find_latest_approval_decision("sess-flip", "req-F")
        .unwrap()
        .unwrap();
    assert_eq!(latest.decision, "allow");
}
