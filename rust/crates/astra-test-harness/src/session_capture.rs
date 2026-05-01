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

    /// Distinct tool names seen in the journal. Journal is the
    /// source of truth — `RunOutcome.tools_used` (from the CLI
    /// envelope) can miss tools emitted inside sub-agent runs.
    ///
    /// Names are returned in first-seen order and de-duplicated. The
    /// loader supports all four shapes we've seen in real
    /// `~/.astra/sessions/` files:
    ///
    /// 1. **Legacy `llm_round`** events with a nested
    ///    `tool_calls: [{name, ok, ms, ...}]` array. This is the
    ///    dominant shape on `~/.astra/sessions/<id>.jsonl` — most
    ///    sessions on disk use it.
    /// 2. Step-events `ToolCallCompleted` with `payload.tool_name`
    ///    (live under `<id>/step_events.jsonl`).
    /// 3. Hypothetical `tool_invocation` events with
    ///    `metadata.tool_name` (kept for forward compat).
    /// 4. Hypothetical `tool_invocation` with flat `tool_name` field.
    ///
    /// Until we verified against real journals (R4 review), the
    /// method silently missed shape #1 and returned empty lists on
    /// every real session. That was the root cause of
    /// `JournalToolCalled` never matching in practice.
    pub fn tools_invoked(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let push_unique = |name: &str, out: &mut Vec<String>| {
            if !name.is_empty() && !out.iter().any(|x: &String| x == name) {
                out.push(name.to_string());
            }
        };
        for e in &self.events {
            // Shape 1: legacy llm_round with nested tool_calls array.
            if e.event_type == "llm_round"
                && let Some(calls) = e.raw.get("tool_calls").and_then(|v| v.as_array())
            {
                for c in calls {
                    if let Some(name) = c.get("name").and_then(|n| n.as_str()) {
                        push_unique(name, &mut out);
                    }
                }
                continue;
            }
            // Shape 2: step-events ToolCallCompleted.
            // Shapes 3/4: tool_invocation with nested or flat name.
            if e.event_type == "ToolCallCompleted" || e.event_type == "tool_invocation" {
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
                if let Some(name) = name {
                    push_unique(name, &mut out);
                }
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

/// Load a session journal by id. Merges both layouts when both
/// exist:
///
/// 1. Legacy: `~/.astra/sessions/<id>.jsonl` with `type`
///    discriminator. Carries `llm_round` (nested tool_calls),
///    `llm_request_full`, `llm_response_full`, `turn`, etc.
/// 2. Step-events: `~/.astra/sessions/<id>/step_events.jsonl` with
///    `event_type` discriminator. Carries `StepCreated`,
///    `StepStarted`, `ToolCallCompleted`, `StepEvaluated`,
///    `StepCompleted`.
///
/// Both files are complementary — legacy has token and tool_call
/// detail, step_events has per-step lifecycle. Prior to the R4 fix
/// this function returned early on the legacy path, so any criterion
/// asking for a step_events-only event (`ToolCallCompleted`) never
/// matched on any real session.
///
/// Returns `None` only if neither file exists. Malformed lines are
/// counted in `skipped_lines` — never an error. The returned
/// `journal_path` points at whichever file existed (legacy
/// preferred for backward-compatible user-facing hints); the actual
/// events are the union.
pub fn load_session(session_id: &str) -> Option<SessionCapture> {
    let dir = default_sessions_dir();
    let legacy = dir.join(format!("{session_id}.jsonl"));
    let step_events = dir.join(session_id).join("step_events.jsonl");

    let legacy_cap = if legacy.is_file() {
        load_session_from_path(session_id, &legacy)
    } else {
        None
    };
    let step_cap = if step_events.is_file() {
        load_session_from_path(session_id, &step_events)
    } else {
        None
    };

    match (legacy_cap, step_cap) {
        (None, None) => None,
        (Some(c), None) | (None, Some(c)) => Some(c),
        (Some(mut legacy_c), Some(step_c)) => {
            // Legacy's path wins as the primary identifier (the jq
            // hint points there); events are the union.
            legacy_c.events.extend(step_c.events);
            legacy_c.skipped_lines = legacy_c.skipped_lines.saturating_add(step_c.skipped_lines);
            Some(legacy_c)
        }
    }
}

/// Maximum journal file size the loader will read. Long-running
/// sessions with tens of thousands of events can legitimately blow
/// past the legacy default; the cap guards against a pathological
/// case pulling a 1GB jsonl into memory + cloning it into every
/// CaseRunReport. Override with
/// `load_session_from_path_with_caps` when a suite needs bigger.
pub const DEFAULT_MAX_JOURNAL_BYTES: u64 = 64 * 1024 * 1024;

/// Maximum events the loader will retain. Beyond this we evict
/// from the head (oldest events) on each insert, so the newest
/// events — where observability criteria (`[fork-cache]` lines,
/// `ToolCallCompleted` near the end of the run) usually live —
/// survive in the returned `SessionCapture` even for very long
/// sessions.
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
    let mut dropped_from_head: u32 = 0;
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
                    // Ring-buffer: drop the head (oldest event) so
                    // the tail (newest events — where `[fork-cache]`
                    // and closing `ToolCallCompleted` live) survives.
                    events.remove(0);
                    dropped_from_head = dropped_from_head.saturating_add(1);
                }
                events.push(JournalEvent { event_type, raw: v });
            }
            Err(_) => skipped = skipped.saturating_add(1),
        }
    }
    if dropped_from_head > 0 {
        eprintln!(
            "[astra-test] WARNING: session {session_id} journal truncated to most-recent \
             {max_events} events ({dropped_from_head} older events dropped from the head). \
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

    #[test]
    fn tools_invoked_walks_legacy_llm_round_nested_tool_calls() {
        // R4 Blocker regression. Real `~/.astra/sessions/<id>.jsonl`
        // files emit `llm_round` events with a nested
        // `tool_calls: [{name, ok, ms, ...}]` array — NOT top-level
        // `tool_invocation`. Before this fix, `tools_invoked()`
        // returned empty on every real session so
        // `journal_tool_called` criteria could never match.
        let dir = tempdir().unwrap();
        let path = dir.path().join("legacy.jsonl");
        let body = [
            r#"{"type":"llm_request_full","session_id":"s","turn":1}"#,
            r#"{"type":"llm_round","session_id":"s","turn":1,"tool_calls":[{"name":"read_file","ok":true,"ms":12},{"name":"list_dir","ok":true,"ms":8}]}"#,
            r#"{"type":"llm_round","session_id":"s","turn":2,"tool_calls":[{"name":"list_dir","ok":true,"ms":3}]}"#,
        ]
        .join("\n");
        std::fs::write(&path, body).unwrap();

        let cap = load_session_from_path("legacy", &path).unwrap();
        let tools = cap.tools_invoked();
        // Both names surfaced, de-duplicated, first-seen order.
        assert_eq!(
            tools,
            vec!["read_file".to_string(), "list_dir".to_string()],
            "tools must walk llm_round.tool_calls[] in first-seen order"
        );
    }

    #[test]
    fn load_session_merges_both_layouts_when_both_exist() {
        // R4 Blocker follow-up: when both `<id>.jsonl` and
        // `<id>/step_events.jsonl` exist, the loader must return the
        // UNION of events. Previously it returned the legacy file
        // only, which made step-events-only event types
        // (`ToolCallCompleted`) unmatchable on any session that also
        // had legacy output (all of them).
        use std::env;
        let dir = tempdir().unwrap();
        // Redirect HOME so `default_sessions_dir` points at our tmp.
        // Safe: single-threaded #[test]; restored after.
        let prev_home = env::var_os("HOME");
        // SAFETY: single-threaded test context; restored immediately
        // after the load. A compile-time `#[serial_test]` would be
        // better but adding a crate for one test is overkill.
        unsafe {
            env::set_var("HOME", dir.path());
        }
        let sessions = dir.path().join(".astra").join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();

        let sid = "merge-test";
        // Legacy: llm_round with one nested tool_call.
        std::fs::write(
            sessions.join(format!("{sid}.jsonl")),
            r#"{"type":"llm_round","session_id":"merge-test","turn":1,"tool_calls":[{"name":"read_file","ok":true,"ms":5}]}"#,
        )
        .unwrap();
        // Step-events: one ToolCallCompleted with a DIFFERENT tool.
        let step_dir = sessions.join(sid);
        std::fs::create_dir_all(&step_dir).unwrap();
        std::fs::write(
            step_dir.join("step_events.jsonl"),
            r#"{"event_type":"ToolCallCompleted","payload":{"tool_name":"list_dir"}}"#,
        )
        .unwrap();

        let cap = load_session(sid).expect("both layouts must load");
        let tools = cap.tools_invoked();

        // Restore HOME before asserts so a panic doesn't leak the override.
        unsafe {
            match prev_home {
                Some(h) => env::set_var("HOME", h),
                None => env::remove_var("HOME"),
            }
        }

        assert!(
            tools.contains(&"read_file".to_string()),
            "legacy tool must survive merge: {tools:?}"
        );
        assert!(
            tools.contains(&"list_dir".to_string()),
            "step-events tool must survive merge: {tools:?}"
        );
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
    fn load_session_drops_from_head_when_event_cap_exceeded() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("many.jsonl");
        // 500 events; cap at 10. Oldest 490 should be dropped from
        // the head of the buffer; the newest 10 (seq 490..=499) kept.
        let lines: Vec<String> = (0..500)
            .map(|i| format!(r#"{{"type":"evt","seq":{i}}}"#))
            .collect();
        std::fs::write(&path, lines.join("\n")).unwrap();

        let cap = load_session_from_path_with_caps("many", &path, u64::MAX, 10).expect("load");
        assert_eq!(cap.events.len(), 10);
        // Ring-buffer drops from head (oldest) so the buffer retains
        // the tail of the jsonl (newest events). Check by seq field.
        let first_seq = cap.events[0].raw.get("seq").and_then(|v| v.as_u64());
        assert_eq!(
            first_seq,
            Some(490),
            "retained buffer must start at seq=490 (oldest of the kept window); \
             got first={first_seq:?}"
        );
        let last_seq = cap.events[9].raw.get("seq").and_then(|v| v.as_u64());
        assert_eq!(last_seq, Some(499));
    }

    #[test]
    fn load_session_under_caps_behaves_as_before() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("small.jsonl");
        let body = [r#"{"type":"a"}"#, r#"{"type":"b"}"#].join("\n");
        std::fs::write(&path, body).unwrap();
        let cap = load_session_from_path_with_caps("small", &path, 1024, 100).unwrap();
        assert_eq!(cap.events.len(), 2);
        assert_eq!(cap.skipped_lines, 0);
    }
}
