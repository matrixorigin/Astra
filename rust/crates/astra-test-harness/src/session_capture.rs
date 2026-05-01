//! Session journal capture.
//!
//! After a case runs, the RunOutcome carries a `session_id` pointing
//! at the local journal at `~/.astra/sessions/<session_id>.jsonl`.
//! This module loads + lightly shapes that file so criteria
//! evaluators can reason over it ("verify the delegation tree has
//! exactly 2 children", "did a tool call named `spawn_agent` happen
//! at turn 3?") without each evaluator hand-rolling its own parser.
//!
//! Parsing is intentionally loose — each line is kept as a
//! `serde_json::Value` plus a pre-extracted `type` field for fast
//! discrimination. We never fail the whole harness if one line is
//! malformed; we skip it and report the count of skipped lines.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A single parsed journal line. The astra journal is jsonl; each
/// line has a `type` field (e.g. `llm_request_full`, `llm_round`,
/// `tool_invocation`, `subagent_spawned`) plus type-specific fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEvent {
    /// Discriminator from the `type` field. Empty string if the line
    /// had no `type` (malformed).
    pub event_type: String,
    /// Raw JSON value — evaluators may probe arbitrary fields.
    pub raw: serde_json::Value,
}

/// Loaded session with minimal summary counters the report uses.
///
/// `#[non_exhaustive]`: this struct serializes into `--format json`
/// reports so external consumers may deserialize it. New fields
/// (event counts by category, journal-file size, schema version)
/// should be additive without a SemVer break. In-crate construction
/// is unaffected; downstream callers must use `..` when matching.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SessionCapture {
    pub session_id: String,
    pub journal_path: PathBuf,
    pub events: Vec<JournalEvent>,
    /// Count of jsonl lines that failed to parse. Surfaced in the
    /// report so we don't silently hide corruption.
    pub skipped_lines: u32,
}

impl SessionCapture {
    /// Number of events matching `event_type`. Cheap helper used by
    /// criteria evaluators ("at least one `subagent_spawned` event").
    pub fn count_events(&self, event_type: &str) -> usize {
        self.events
            .iter()
            .filter(|e| e.event_type == event_type)
            .count()
    }

    /// Distinct tool names seen in the journal. Journal is the source
    /// of truth — `RunOutcome.tools_used` (from the CLI envelope) can
    /// miss tools emitted inside sub-agent runs.
    ///
    /// Supports three journal shapes seen in the wild:
    /// - legacy `tool_invocation` events with `metadata.tool_name`
    /// - legacy `tool_invocation` events with top-level `tool_name`
    /// - step-events `ToolCallCompleted` events with `payload.tool_name`
    pub fn tools_invoked(&self) -> Vec<String> {
        let mut out = Vec::new();
        for e in &self.events {
            let is_tool_event =
                e.event_type == "tool_invocation" || e.event_type == "ToolCallCompleted";
            if !is_tool_event {
                continue;
            }
            let name = e
                .raw
                .get("payload")
                .and_then(|p| p.get("tool_name"))
                .and_then(|n| n.as_str())
                .or_else(|| {
                    e.raw
                        .get("metadata")
                        .and_then(|m| m.get("tool_name"))
                        .and_then(|n| n.as_str())
                })
                .or_else(|| e.raw.get("tool_name").and_then(|n| n.as_str()));
            if let Some(name) = name
                && !out.iter().any(|x: &String| x == name)
            {
                out.push(name.to_string());
            }
        }
        out
    }
}

/// Default session directory — mirrors astra's `~/.astra/sessions/`.
pub fn default_sessions_dir() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".astra").join("sessions")
    } else {
        PathBuf::from(".astra").join("sessions")
    }
}

/// Load a session journal by id. Tries two layouts in order:
///
/// 1. Legacy: `~/.astra/sessions/<id>.jsonl` with `type` discriminator.
/// 2. Current: `~/.astra/sessions/<id>/step_events.jsonl` with
///    `event_type` discriminator (step events for the structured-step
///    pipeline).
///
/// Returns `None` only if neither file exists. Malformed lines are
/// counted in `skipped_lines` — never an error.
pub fn load_session(session_id: &str) -> Option<SessionCapture> {
    let dir = default_sessions_dir();
    let legacy = dir.join(format!("{session_id}.jsonl"));
    if legacy.is_file()
        && let Some(cap) = load_session_from_path(session_id, &legacy)
    {
        return Some(cap);
    }
    let step_events = dir.join(session_id).join("step_events.jsonl");
    if step_events.is_file() {
        return load_session_from_path(session_id, &step_events);
    }
    None
}

/// Maximum journal file size the loader will read. Long-running
/// sessions with tens of thousands of events can legitimately blow
/// past the legacy default; the cap guards against a pathological
/// case pulling a 1GB jsonl into memory + cloning it into every
/// CaseRunReport. Override with
/// `load_session_from_path_with_caps` when a suite needs bigger.
pub const DEFAULT_MAX_JOURNAL_BYTES: u64 = 64 * 1024 * 1024;

/// Maximum events the loader will retain. Beyond this we truncate
/// from the tail (most recent) so observability events the criteria
/// care about — `[fork-cache]` lines, `ToolCallCompleted` at the end
/// of the run — are kept even for very long sessions.
pub const DEFAULT_MAX_EVENTS: usize = 100_000;

/// Load a session journal from an explicit path — test seam. Detects
/// both layouts: lines with `type` (legacy) and lines with
/// `event_type` (step-events) both populate `event_type` so criteria
/// don't need to know which layout they got.
///
/// Uses `DEFAULT_MAX_JOURNAL_BYTES` + `DEFAULT_MAX_EVENTS` caps.
/// For custom caps, see `load_session_from_path_with_caps`.
pub fn load_session_from_path(session_id: &str, path: &Path) -> Option<SessionCapture> {
    load_session_from_path_with_caps(
        session_id,
        path,
        DEFAULT_MAX_JOURNAL_BYTES,
        DEFAULT_MAX_EVENTS,
    )
}

/// Cap-configurable version of [`load_session_from_path`] — useful
/// for tests that want to exercise the truncation paths without
/// materializing 64 MiB of jsonl.
pub fn load_session_from_path_with_caps(
    session_id: &str,
    path: &Path,
    max_bytes: u64,
    max_events: usize,
) -> Option<SessionCapture> {
    // Size pre-check: refuse to even try reading a file larger than
    // the cap. A pathological jsonl (wrong file, infinite loop bug)
    // otherwise OOMs via `read_to_string`. Reporting a zero-event
    // capture would mask the anomaly, so we emit a visible stderr
    // warning and return None (callers already treat that as
    // "session unavailable" which now fails criteria strictly).
    if let Ok(meta) = std::fs::metadata(path)
        && meta.len() > max_bytes
    {
        eprintln!(
            "[astra-test] WARNING: session {} journal {} bytes exceeds \
             {}-byte cap; skipping load. Raise with \
             load_session_from_path_with_caps or investigate the runaway.",
            session_id,
            meta.len(),
            max_bytes,
        );
        return None;
    }
    let body = std::fs::read_to_string(path).ok()?;
    let mut events: Vec<JournalEvent> = Vec::with_capacity(128);
    let mut skipped: u32 = 0;
    let mut truncated_from_tail: u32 = 0;
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<serde_json::Value>(line) {
            Ok(v) => {
                let event_type = v
                    .get("type")
                    .and_then(|t| t.as_str())
                    .or_else(|| v.get("event_type").and_then(|t| t.as_str()))
                    .unwrap_or("")
                    .to_string();
                if events.len() >= max_events {
                    // Ring-buffer-ish: drop oldest to make room for
                    // newest. Observability criteria (`[fork-cache]`,
                    // ToolCallCompleted near the end) care about the
                    // tail more than the head.
                    events.remove(0);
                    truncated_from_tail = truncated_from_tail.saturating_add(1);
                }
                events.push(JournalEvent {
                    event_type,
                    raw: v,
                });
            }
            Err(_) => skipped = skipped.saturating_add(1),
        }
    }
    if truncated_from_tail > 0 {
        eprintln!(
            "[astra-test] WARNING: session {session_id} journal truncated to most-recent \
             {max_events} events ({truncated_from_tail} older events dropped). \
             Criteria that look for early events may need the uncapped loader."
        );
    }
    Some(SessionCapture {
        session_id: session_id.to_string(),
        journal_path: path.to_path_buf(),
        events,
        skipped_lines: skipped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn load_session_parses_jsonl_and_counts_events() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s1.jsonl");
        let body = [
            r#"{"type":"llm_request_full","session_id":"s1"}"#,
            r#"{"type":"llm_round","session_id":"s1","turn":1}"#,
            r#"{"type":"llm_round","session_id":"s1","turn":2}"#,
        ]
        .join("\n");
        std::fs::write(&path, body).unwrap();

        let cap = load_session_from_path("s1", &path).unwrap();
        assert_eq!(cap.events.len(), 3);
        assert_eq!(cap.count_events("llm_round"), 2);
        assert_eq!(cap.count_events("llm_request_full"), 1);
        assert_eq!(cap.skipped_lines, 0);
    }

    #[test]
    fn load_session_skips_malformed_lines_but_keeps_going() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s2.jsonl");
        let body = [
            r#"{"type":"llm_round"}"#,
            r#"this is not json"#,
            r#"{"type":"tool_invocation","metadata":{"tool_name":"Read"}}"#,
        ]
        .join("\n");
        std::fs::write(&path, body).unwrap();

        let cap = load_session_from_path("s2", &path).unwrap();
        assert_eq!(cap.events.len(), 2);
        assert_eq!(cap.skipped_lines, 1);
        assert_eq!(cap.tools_invoked(), vec!["Read".to_string()]);
    }

    #[test]
    fn load_session_returns_none_when_missing() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("nope.jsonl");
        assert!(load_session_from_path("nope", &missing).is_none());
    }

    #[test]
    fn tools_invoked_dedups_and_supports_flat_shape() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s3.jsonl");
        // Cover both the metadata-nested shape and the flat shape.
        let body = [
            r#"{"type":"tool_invocation","metadata":{"tool_name":"Read"}}"#,
            r#"{"type":"tool_invocation","metadata":{"tool_name":"Read"}}"#,
            r#"{"type":"tool_invocation","tool_name":"Grep"}"#,
        ]
        .join("\n");
        std::fs::write(&path, body).unwrap();

        let cap = load_session_from_path("s3", &path).unwrap();
        let tools = cap.tools_invoked();
        assert!(tools.contains(&"Read".to_string()));
        assert!(tools.contains(&"Grep".to_string()));
        assert_eq!(tools.len(), 2);
    }

    // ── Caps (R3 #6) ──

    #[test]
    fn load_session_rejects_journal_exceeding_byte_cap() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("big.jsonl");
        // 2 KiB of content; cap at 1 KiB.
        let body = vec![b'{'; 2048];
        std::fs::write(&path, body).unwrap();
        // 1024-byte cap, plenty of events allowed.
        let cap = load_session_from_path_with_caps("big", &path, 1024, 1_000);
        assert!(
            cap.is_none(),
            "oversize journal must return None so callers see session-unavailable"
        );
    }

    #[test]
    fn load_session_truncates_tail_when_event_cap_exceeded() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("many.jsonl");
        // 500 events; cap at 10. Most recent 10 should be kept.
        let lines: Vec<String> = (0..500)
            .map(|i| format!(r#"{{"type":"evt","seq":{i}}}"#))
            .collect();
        std::fs::write(&path, lines.join("\n")).unwrap();

        let cap =
            load_session_from_path_with_caps("many", &path, u64::MAX, 10).expect("load");
        assert_eq!(cap.events.len(), 10);
        // First retained event should be the 491st (0-indexed 490)
        // since the ring-buffer drops oldest. Check by seq field.
        let first_seq = cap.events[0].raw.get("seq").and_then(|v| v.as_u64());
        assert_eq!(
            first_seq,
            Some(490),
            "tail-window must keep newest events; got first={first_seq:?}"
        );
        let last_seq = cap.events[9].raw.get("seq").and_then(|v| v.as_u64());
        assert_eq!(last_seq, Some(499));
    }

    #[test]
    fn load_session_under_caps_behaves_as_before() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("small.jsonl");
        let body = [
            r#"{"type":"a"}"#,
            r#"{"type":"b"}"#,
        ]
        .join("\n");
        std::fs::write(&path, body).unwrap();
        let cap = load_session_from_path_with_caps("small", &path, 1024, 100).unwrap();
        assert_eq!(cap.events.len(), 2);
        assert_eq!(cap.skipped_lines, 0);
    }
}
