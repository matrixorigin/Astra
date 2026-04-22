//! Phase G: Crash / partial-write recovery for the session journal.
//!
//! Audit gap 4.3: after a process crash, the JSONL journal may contain a torn
//! final line, garbage bytes, or interleaved fragments. Readers must never
//! panic, must drop malformed lines, and must keep readable events. A fresh
//! append after a crash must not make valid pre-crash events unreadable.
//!
//! These tests target [`astra_services::session_journal`]:
//!  - `read_journal`
//!  - `JournalWriter::append` / `append_bulk`
//!  - `TurnEventBuffer::flush_interrupted` → `flush`

use std::fs;
use std::io::Write;

use astra_services::session_journal::{
    JournalDirGuard, JournalEvent, JournalEventType, JournalWriter, LlmRoundRecord,
    TurnEventBuffer, read_journal,
};
use tempfile::tempdir;

/// Write some valid JSONL lines then append an incomplete (torn) line without
/// a trailing newline. `read_journal` must recover the valid events and skip
/// the torn tail rather than panicking.
#[test]
fn read_journal_recovers_valid_events_after_torn_last_line() {
    let tmp = tempdir().unwrap();
    let _guard = JournalDirGuard::new(tmp.path());
    let writer = JournalWriter::new("sess-torn").unwrap();

    let events = vec![
        JournalEvent::session_start(Some("sess-torn"), Some("gpt-4")),
        JournalEvent::base_public(JournalEventType::Turn, Some("sess-torn")),
        JournalEvent::base_public(JournalEventType::Turn, Some("sess-torn")),
    ];
    writer.append_bulk(&events).unwrap();

    // Simulate a process crash mid-line: append a truncated JSON fragment
    // with no trailing newline. This is exactly what a kill -9 during writeln
    // can leave on disk if the buffered write was only partially flushed.
    let mut f = fs::OpenOptions::new()
        .append(true)
        .open(writer.path())
        .unwrap();
    f.write_all(b"{\"type\":\"turn\",\"ts\":\"2026-04-2")
        .unwrap();
    drop(f);

    let recovered = read_journal("sess-torn").expect("read must not error");
    assert_eq!(
        recovered.len(),
        3,
        "torn tail must be skipped, valid events preserved"
    );
    assert_eq!(recovered[0].event_type, JournalEventType::SessionStart);
    assert_eq!(recovered[1].event_type, JournalEventType::Turn);
    assert_eq!(recovered[2].event_type, JournalEventType::Turn);
}

/// A zero-byte journal file (e.g. `open(... O_CREAT)` that crashed before the
/// first flush) must read as an empty event list without error.
#[test]
fn read_journal_handles_empty_file() {
    let tmp = tempdir().unwrap();
    let _guard = JournalDirGuard::new(tmp.path());
    let path = tmp.path().join("sess-empty.jsonl");
    fs::File::create(&path).unwrap();
    assert_eq!(fs::metadata(&path).unwrap().len(), 0);

    let events = read_journal("sess-empty").expect("empty file must not error");
    assert!(events.is_empty());
}

/// Garbage interleaved with valid JSON lines (simulating FS corruption or a
/// concurrent writer that died mid-line leaving a partial row between valid
/// ones) must yield just the valid events.
#[test]
fn read_journal_skips_malformed_lines_between_valid_ones() {
    let tmp = tempdir().unwrap();
    let _guard = JournalDirGuard::new(tmp.path());
    let path = tmp.path().join("sess-garbage.jsonl");

    let good = serde_json::to_string(&JournalEvent::session_start(
        Some("sess-garbage"),
        Some("gpt-4"),
    ))
    .unwrap();
    let good2 = serde_json::to_string(&JournalEvent::base_public(
        JournalEventType::Turn,
        Some("sess-garbage"),
    ))
    .unwrap();

    let content = format!("{good}\nnot-json-at-all\n{{broken:\"json\",}}\n{good2}\n\n   \n");
    fs::write(&path, content).unwrap();

    let events = read_journal("sess-garbage").unwrap();
    assert_eq!(events.len(), 2, "blank lines and junk must be dropped");
    assert_eq!(events[0].event_type, JournalEventType::SessionStart);
    assert_eq!(events[1].event_type, JournalEventType::Turn);
}

/// After a torn write, a subsequent append proceeds normally. New events
/// separated by a leading newline remain readable. The torn fragment
/// concatenates with whatever follows on the same line — that single merged
/// line is the only one lost.
#[test]
fn append_after_torn_line_keeps_new_events_readable() {
    let tmp = tempdir().unwrap();
    let _guard = JournalDirGuard::new(tmp.path());
    let writer = JournalWriter::new("sess-resume").unwrap();

    let ev = JournalEvent::session_start(Some("sess-resume"), Some("gpt-4"));
    writer.append(&ev).unwrap();

    // Torn fragment: no trailing newline, incomplete JSON.
    let mut f = fs::OpenOptions::new()
        .append(true)
        .open(writer.path())
        .unwrap();
    f.write_all(b"{\"type\":\"turn\",\"ts\":\"abc").unwrap();
    drop(f);

    // Resume: legitimate append. Because `append` uses OpenOptions::append
    // (atomic O_APPEND seek-to-end + write) and writeln adds a trailing \n,
    // the new event still ends up on its own physical line — provided we
    // accept that the one merged "garbage + new-event-header" line is lost.
    // After the first resumed append any further events come back cleanly.
    let ev2 = JournalEvent::base_public(JournalEventType::Turn, Some("sess-resume"));
    let ev3 = JournalEvent::session_end(Some("sess-resume"), 1);
    writer.append(&ev2).unwrap();
    writer.append(&ev3).unwrap();

    let events = read_journal("sess-resume").unwrap();
    // At minimum, the initial SessionStart and the final SessionEnd survive.
    // The middle event may or may not, depending on whether it got concatenated
    // with the torn fragment on the same physical line.
    let types: Vec<JournalEventType> = events.iter().map(|e| e.event_type.clone()).collect();
    assert!(
        types.contains(&JournalEventType::SessionStart),
        "pre-crash event must survive: {types:?}",
    );
    assert!(
        types.contains(&JournalEventType::SessionEnd),
        "post-crash event must be readable: {types:?}",
    );
}

/// `flush_interrupted` on a partial turn buffer followed by a regular `flush`
/// of more events must produce a journal where BOTH sets of events are
/// recoverable, and the interrupted events retain their `partial: true`
/// metadata marker.
#[test]
fn flush_interrupted_then_regular_flush_preserves_both() {
    let tmp = tempdir().unwrap();
    let _guard = JournalDirGuard::new(tmp.path());
    let writer = JournalWriter::new("sess-mixed").unwrap();

    let mut buf = TurnEventBuffer::begin_turn(Some("sess-mixed"), 1);
    buf.record_llm_round(LlmRoundRecord {
        ttft_ms: Some(42),
        duration_ms: 100,
        prompt_tokens: 500,
        completion_tokens: 30,
        cache_read_tokens: 0,
        tool_calls_returned: 1,
        tool_call_names: vec!["read_file".into()],
        finish_reason: None,
        agentic_step: Some(0),
        source: None,
        run_id: None,
    });
    buf.flush_interrupted(&writer).unwrap();
    assert!(buf.is_empty());

    // Next turn (started fresh, proper completion) writes via regular flush.
    let mut buf2 = TurnEventBuffer::begin_turn(Some("sess-mixed"), 2);
    buf2.record_llm_round(LlmRoundRecord {
        ttft_ms: Some(20),
        duration_ms: 50,
        prompt_tokens: 400,
        completion_tokens: 20,
        cache_read_tokens: 200,
        tool_calls_returned: 0,
        tool_call_names: vec![],
        finish_reason: Some("stop".into()),
        agentic_step: Some(0),
        source: None,
        run_id: None,
    });
    buf2.flush(&writer).unwrap();

    let events = read_journal("sess-mixed").unwrap();
    assert_eq!(events.len(), 2, "both rounds must be recoverable");

    let partial_flag = events[0]
        .metadata
        .as_ref()
        .and_then(|m| m.get("partial"))
        .and_then(|v| v.as_bool());
    assert_eq!(
        partial_flag,
        Some(true),
        "interrupted event must keep partial:true marker"
    );

    let second_partial = events[1]
        .metadata
        .as_ref()
        .and_then(|m| m.get("partial"))
        .and_then(|v| v.as_bool());
    assert_ne!(
        second_partial,
        Some(true),
        "subsequent regular flush must NOT be marked partial"
    );
}

/// A journal that was never written to (session started but no event flushed
/// before crash) must read as empty without creating a spurious error.
#[test]
fn read_journal_missing_file_returns_empty() {
    let tmp = tempdir().unwrap();
    let _guard = JournalDirGuard::new(tmp.path());
    // No writer, no file.
    let events = read_journal("sess-never-written").unwrap();
    assert!(events.is_empty());
}

/// Simulate the exact torn-line-in-the-middle pattern: a valid line, then a
/// line with a truncated JSON payload terminated by `\n` (because the kernel
/// did flush the newline but the process died before writing the closing
/// brace), then more valid lines. The malformed middle line must be dropped
/// while subsequent valid lines remain readable.
#[test]
fn read_journal_recovers_when_torn_line_sits_in_middle() {
    let tmp = tempdir().unwrap();
    let _guard = JournalDirGuard::new(tmp.path());
    let path = tmp.path().join("sess-mid.jsonl");

    let a = serde_json::to_string(&JournalEvent::session_start(
        Some("sess-mid"),
        Some("gpt-4"),
    ))
    .unwrap();
    let c = serde_json::to_string(&JournalEvent::session_end(Some("sess-mid"), 2)).unwrap();

    let content = format!("{a}\n{{\"type\":\"turn\",\"ts\":\"trunc\n{c}\n");
    fs::write(&path, content).unwrap();

    let events = read_journal("sess-mid").unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].event_type, JournalEventType::SessionStart);
    assert_eq!(events[1].event_type, JournalEventType::SessionEnd);
}

/// After a crash mid-turn, `flush_interrupted` writes partial events without
/// fsync. A subsequent process start that does *not* see those events (e.g.,
/// because only the kernel page-cache held them and the machine lost power)
/// must still handle the journal safely — either reading what made it to
/// disk, or reading nothing at all, without corrupting future appends.
#[test]
fn append_after_flush_interrupted_still_produces_readable_journal() {
    let tmp = tempdir().unwrap();
    let _guard = JournalDirGuard::new(tmp.path());
    let writer = JournalWriter::new("sess-postcrash").unwrap();

    let mut buf = TurnEventBuffer::begin_turn(Some("sess-postcrash"), 1);
    buf.record_llm_round(LlmRoundRecord {
        ttft_ms: Some(10),
        duration_ms: 20,
        prompt_tokens: 100,
        completion_tokens: 5,
        cache_read_tokens: 0,
        tool_calls_returned: 0,
        tool_call_names: vec![],
        finish_reason: None,
        agentic_step: Some(0),
        source: None,
        run_id: None,
    });
    buf.flush_interrupted(&writer).unwrap();

    // "Next process start" — append a fresh turn marker and session end.
    writer
        .append(&JournalEvent::base_public(
            JournalEventType::Turn,
            Some("sess-postcrash"),
        ))
        .unwrap();
    writer
        .append(&JournalEvent::session_end(Some("sess-postcrash"), 2))
        .unwrap();

    let events = read_journal("sess-postcrash").unwrap();
    assert!(events.len() >= 2, "post-crash appends must be readable");
    let last = events.last().unwrap();
    assert_eq!(last.event_type, JournalEventType::SessionEnd);
}
