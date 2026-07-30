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

/// One complete tool record from a durable `turn`/`llm_round` journal event.
/// Step events deliberately carry previews, so contract assertions use this
/// full record instead of mistaking truncated observability for lifecycle
/// evidence.
#[derive(Debug, Clone)]
pub struct JournalToolCall {
    pub call_id: Option<String>,
    pub name: String,
    pub ok: Option<bool>,
    pub arguments: Option<serde_json::Value>,
    pub result: Option<serde_json::Value>,
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
    /// Count of older events discarded because a configured capture cap was
    /// reached. A non-zero value means the capture is not complete and must
    /// not drive destructive, session-scoped cleanup.
    #[serde(default)]
    pub dropped_lines: u32,
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
            if (e.event_type == "llm_round" || e.event_type == "turn")
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

    /// Complete, de-duplicated tool calls persisted in turn records.
    pub fn journal_tool_calls(&self) -> Vec<JournalToolCall> {
        let mut calls = Vec::new();
        let mut seen_ids = std::collections::HashSet::new();
        for event in &self.events {
            if event.event_type != "turn" && event.event_type != "llm_round" {
                continue;
            }
            let Some(records) = event
                .raw
                .get("tool_calls")
                .and_then(|value| value.as_array())
            else {
                continue;
            };
            for record in records {
                let Some(name) = record.get("name").and_then(|value| value.as_str()) else {
                    continue;
                };
                let call_id = record
                    .get("tool_call_id")
                    .or_else(|| record.get("call_id"))
                    .and_then(|value| value.as_str())
                    .map(str::to_string);
                if let Some(call_id) = &call_id
                    && !seen_ids.insert(call_id.clone())
                {
                    continue;
                }
                calls.push(JournalToolCall {
                    call_id,
                    name: name.to_string(),
                    ok: record.get("ok").and_then(|value| value.as_bool()),
                    arguments: embedded_json(
                        record.get("args_full").or_else(|| record.get("args")),
                    ),
                    result: embedded_json(
                        record.get("result_full").or_else(|| record.get("result")),
                    ),
                });
            }
        }
        calls
    }

    /// Bounded durable evidence supplied to an LLM judger. The journal stays
    /// authoritative; this is only a transparent projection that lets a
    /// cross-family judge verify arguments and results instead of trusting the
    /// final assistant's self-report.
    pub fn render_tool_evidence(&self, max_chars: usize) -> String {
        let mut rendered = String::new();
        for call in self.journal_tool_calls() {
            let record = serde_json::json!({
                "call_id": call.call_id,
                "name": call.name,
                "ok": call.ok,
                "arguments": call.arguments,
                "result": call.result,
            });
            let line = serde_json::to_string(&record).unwrap_or_default();
            if rendered.chars().count() + line.chars().count() + 1 > max_chars {
                rendered.push_str("[remaining durable tool evidence elided by harness bound]\n");
                break;
            }
            rendered.push_str(&line);
            rendered.push('\n');
        }
        rendered
    }

    /// Exact memory records created by this session according to the store
    /// response contract. This is intentionally stricter than merely looking
    /// for `memory_id`: recall results also carry IDs and must never become
    /// deletion candidates.
    ///
    /// A qualifying record is a successful `memory` ToolCallCompleted event
    /// whose JSON-object response names this capture's session, has a newly
    /// assigned `memory_id`, and retains the store-only null values for
    /// `created_at` and `retrieval_score`. Results are de-duplicated in first
    /// seen order.
    pub fn created_memory_ids(&self) -> Vec<String> {
        let mut ids = Vec::new();
        for event in &self.events {
            if event.event_type != "ToolCallCompleted" {
                continue;
            }
            let Some(payload) = event.raw.get("payload") else {
                continue;
            };
            if payload.get("tool_name").and_then(|v| v.as_str()) != Some("memory")
                || payload.get("is_error").and_then(|v| v.as_bool()) != Some(false)
            {
                continue;
            }
            let Some(output) = payload.get("output").and_then(|v| v.as_str()) else {
                continue;
            };
            let Ok(record) = serde_json::from_str::<serde_json::Value>(output) else {
                continue;
            };
            let Some(record) = record.as_object() else {
                continue;
            };
            let Some(id) = record
                .get("memory_id")
                .and_then(|v| v.as_str())
                .filter(|id| !id.trim().is_empty())
            else {
                continue;
            };
            if record.get("session_id").and_then(|v| v.as_str()) != Some(&self.session_id)
                || !record
                    .get("created_at")
                    .is_some_and(serde_json::Value::is_null)
                || !record
                    .get("retrieval_score")
                    .is_some_and(serde_json::Value::is_null)
            {
                continue;
            }
            if !ids.iter().any(|seen| seen == id) {
                ids.push(id.to_string());
            }
        }
        ids
    }
}

fn embedded_json(value: Option<&serde_json::Value>) -> Option<serde_json::Value> {
    match value? {
        serde_json::Value::String(text) => serde_json::from_str(text)
            .ok()
            .or_else(|| Some(serde_json::Value::String(text.clone()))),
        value => Some(value.clone()),
    }
}

/// Legacy flat session directory (`~/.astra/sessions/`).
///
/// Newer Astra versions place artifacts below an owner-scoped `v1/` layout;
/// use [`load_session_for_owners`] for production reads. This stays public for
/// tests and for callers that intentionally inspect legacy fixtures.
pub fn default_sessions_dir() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".astra").join("sessions")
    } else {
        PathBuf::from(".astra").join("sessions")
    }
}

/// Resolve current owner-scoped artifact paths, then the historic flat paths.
///
/// The journal writer owns this layout decision. Reusing its public path API
/// keeps the harness in lock-step with storage migrations instead of copying a
/// directory convention that will eventually drift again.
fn session_artifact_paths_for_owners(
    session_id: &str,
    owner_scopes: &[astra_services::OwnerScope],
) -> Vec<PathBuf> {
    if astra_services::session_journal::validate_session_id(session_id).is_err() {
        return Vec::new();
    }
    let mut candidates = Vec::with_capacity(owner_scopes.len().saturating_mul(2) + 2);
    for owner_scope in owner_scopes {
        let Ok(scoped_journal) =
            astra_services::session_journal::journal_file_path_for_owner(owner_scope, session_id)
        else {
            continue;
        };
        let scoped_steps = scoped_journal
            .parent()
            .expect("session journal path always has a parent")
            .join(session_id)
            .join("step_events.jsonl");
        candidates.push(scoped_journal);
        candidates.push(scoped_steps);
    }
    let legacy_dir = default_sessions_dir();
    candidates.push(legacy_dir.join(format!("{session_id}.jsonl")));
    candidates.push(legacy_dir.join(session_id).join("step_events.jsonl"));
    let mut unique = Vec::with_capacity(candidates.len());
    for path in candidates {
        if !unique.contains(&path) {
            unique.push(path);
        }
    }
    unique
}

/// Return the logical event carried by a storage envelope.
///
/// `step_events.jsonl` moved from a direct `{event_type, payload}` shape to
/// `{artifact_kind: "step_event", payload: {event_type, payload}}`. Consumers
/// reason about the logical event in both cases, so unwrap only the explicit
/// artifact envelope and leave all legacy journal records unchanged.
fn logical_event(value: &serde_json::Value) -> &serde_json::Value {
    if value.get("artifact_kind").and_then(|kind| kind.as_str()) == Some("step_event") {
        value.get("payload").unwrap_or(value)
    } else {
        value
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
    load_session_for_owners(session_id, &[astra_services::local_owner_scope()])
}

/// Load complementary artifacts from an explicit, authorized set of owners.
///
/// Local CLI journals are profile-scoped while server-produced step events are
/// account-scoped. Callers that cross that process boundary must supply both
/// identities rather than scanning every owner directory for a matching id.
pub fn load_session_for_owners(
    session_id: &str,
    owner_scopes: &[astra_services::OwnerScope],
) -> Option<SessionCapture> {
    let mut captures = session_artifact_paths_for_owners(session_id, owner_scopes)
        .into_iter()
        .filter(|path| path.is_file())
        .filter_map(|path| load_session_from_path(session_id, &path));
    let mut capture = captures.next()?;
    for additional in captures {
        capture.events.extend(additional.events);
        capture.skipped_lines = capture
            .skipped_lines
            .saturating_add(additional.skipped_lines);
    }
    Some(capture)
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
    let mut events: std::collections::VecDeque<JournalEvent> =
        std::collections::VecDeque::with_capacity(128);
    let mut skipped: u32 = 0;
    let mut dropped_from_head: u32 = 0;
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<serde_json::Value>(line) {
            Ok(v) => {
                let logical = logical_event(&v);
                let event_type = logical
                    .get("type")
                    .and_then(|t| t.as_str())
                    .or_else(|| logical.get("event_type").and_then(|t| t.as_str()))
                    .unwrap_or("")
                    .to_string();
                if events.len() >= max_events {
                    events.pop_front();
                    dropped_from_head = dropped_from_head.saturating_add(1);
                }
                events.push_back(JournalEvent {
                    event_type,
                    raw: logical.clone(),
                });
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
        events: events.into(),
        skipped_lines: skipped,
        dropped_lines: dropped_from_head,
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

    #[test]
    fn step_event_stats_rejects_oversize_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("step_events.jsonl");
        let big = vec![b'x'; 2048];
        std::fs::write(&path, &big).unwrap();
        let result = super::load_step_event_stats_from_path(&path, 1024);
        assert!(result.is_none(), "oversize step_events must return None");
    }

    #[test]
    fn step_event_stats_parses_within_cap() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("step_events.jsonl");
        let body = [
            r#"{"event_type":"StepStarted"}"#,
            r#"{"event_type":"ToolCallCompleted","payload":{"tool_name":"Read","cached":true}}"#,
            r#"{"event_type":"ToolCallCompleted","payload":{"tool_name":"Write","cached":false}}"#,
        ]
        .join("\n");
        std::fs::write(&path, &body).unwrap();
        let stats = super::load_step_event_stats_from_path(&path, 1024 * 1024).unwrap();
        assert_eq!(stats.turn_rounds, 1);
        assert_eq!(stats.total_tool_calls, 2);
        assert_eq!(stats.cache_hits, 1);
    }

    #[test]
    fn current_step_event_envelope_is_unwrapped_for_capture_and_stats() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("step_events.jsonl");
        let body = [
            r#"{"schema_version":1,"artifact_kind":"step_event","payload":{"event_type":"StepStarted","payload":{"trace_context":{"round_index":0}}}}"#,
            r#"{"schema_version":1,"artifact_kind":"step_event","payload":{"event_type":"ToolCallCompleted","payload":{"tool_name":"memory","cached":true}}}"#,
        ]
        .join("\n");
        std::fs::write(&path, body).unwrap();

        let capture = load_session_from_path("current", &path).unwrap();
        assert_eq!(capture.count_events("StepStarted"), 1);
        assert_eq!(capture.tools_invoked(), vec!["memory"]);

        let stats = super::load_step_event_stats_from_path(&path, 1024 * 1024).unwrap();
        assert_eq!(stats.turn_rounds, 1);
        assert_eq!(stats.total_tool_calls, 1);
        assert_eq!(stats.cache_hits, 1);
    }

    #[test]
    fn explicit_owner_set_merges_profile_journal_and_account_step_events() {
        let dir = tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(dir.path());
        let profile_owner = astra_services::OwnerScope::user("profile-owner").unwrap();
        let account_owner = astra_services::OwnerScope::user("account-owner").unwrap();
        let session_id = "explicit-owner-namespace-test";

        let journal = astra_services::session_journal::journal_file_path_for_owner(
            &profile_owner,
            session_id,
        )
        .unwrap();
        std::fs::create_dir_all(journal.parent().unwrap()).unwrap();
        std::fs::write(
            &journal,
            r#"{"type":"session_start","session_id":"explicit-owner-namespace-test"}"#,
        )
        .unwrap();

        let account_journal = astra_services::session_journal::journal_file_path_for_owner(
            &account_owner,
            session_id,
        )
        .unwrap();
        let steps = account_journal
            .parent()
            .unwrap()
            .join(session_id)
            .join("step_events.jsonl");
        std::fs::create_dir_all(steps.parent().unwrap()).unwrap();
        std::fs::write(
            &steps,
            r#"{"schema_version":1,"artifact_kind":"step_event","payload":{"event_type":"StepStarted"}}"#,
        )
        .unwrap();

        let profile_only =
            super::load_session_for_owners(session_id, std::slice::from_ref(&profile_owner))
                .unwrap();
        assert_eq!(profile_only.count_events("StepStarted"), 0);

        let owners = [profile_owner, account_owner];
        let capture = super::load_session_for_owners(session_id, &owners).unwrap();
        assert_eq!(capture.count_events("session_start"), 1);
        assert_eq!(capture.count_events("StepStarted"), 1);
        let stats = super::load_step_event_stats_for_owners(session_id, &owners).unwrap();
        assert_eq!(stats.turn_rounds, 1);
        assert!(super::load_session_for_owners("../escape", &owners).is_none());
    }

    #[test]
    fn complete_turn_tool_records_drive_contract_evidence() {
        let capture = SessionCapture {
            session_id: "fanout-session".into(),
            journal_path: PathBuf::from("/tmp/fanout.jsonl"),
            skipped_lines: 0,
            dropped_lines: 0,
            events: vec![
                JournalEvent {
                    event_type: "turn".into(),
                    raw: serde_json::json!({
                        "tool_calls": [{
                            "tool_call_id": "call-1",
                            "name": "agent_fanout",
                            "ok": true,
                            "args_full": r#"{"action":"start","target_count":3}"#,
                            "result_full": r#"{"fanout":{"terminal":3},"provenance":{"all_slots_delivered":true}}"#
                        }]
                    }),
                },
                JournalEvent {
                    event_type: "llm_round".into(),
                    raw: serde_json::json!({
                        "tool_calls": [{
                            "tool_call_id": "call-1",
                            "name": "agent_fanout",
                            "args_full": "duplicate must be ignored"
                        }]
                    }),
                },
                JournalEvent {
                    event_type: "ToolCallCompleted".into(),
                    raw: serde_json::json!({
                        "payload": {"tool_name":"agent_fanout","output":"truncated preview"}
                    }),
                },
            ],
        };

        assert_eq!(capture.tools_invoked(), vec!["agent_fanout"]);
        let calls = capture.journal_tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].arguments.as_ref().unwrap()["target_count"], 3);
        assert_eq!(calls[0].result.as_ref().unwrap()["fanout"]["terminal"], 3);
        let evidence = capture.render_tool_evidence(4096);
        assert!(evidence.contains("all_slots_delivered"), "{evidence}");
        assert!(!evidence.contains("truncated preview"), "{evidence}");
    }

    #[test]
    fn created_memory_ids_selects_only_session_owned_store_responses() {
        let capture = SessionCapture {
            session_id: "case-session".into(),
            journal_path: PathBuf::from("/tmp/case.jsonl"),
            skipped_lines: 0,
            dropped_lines: 0,
            events: vec![
                JournalEvent {
                    event_type: "ToolCallCompleted".into(),
                    raw: serde_json::json!({
                        "payload": {
                            "tool_name": "memory", "is_error": false,
                            "output": r#"{"memory_id":"created","session_id":"case-session","created_at":null,"retrieval_score":null}"#
                        }
                    }),
                },
                JournalEvent {
                    event_type: "ToolCallCompleted".into(),
                    raw: serde_json::json!({
                        "payload": {
                            "tool_name": "memory", "is_error": false,
                            "output": r#"[{"memory_id":"recalled","session_id":"case-session","created_at":"2026-07-14T00:00:00Z","retrieval_score":0.9}]"#
                        }
                    }),
                },
                JournalEvent {
                    event_type: "ToolCallCompleted".into(),
                    raw: serde_json::json!({
                        "payload": {
                            "tool_name": "memory", "is_error": false,
                            "output": r#"{"memory_id":"other-session","session_id":"other","created_at":null,"retrieval_score":null}"#
                        }
                    }),
                },
            ],
        };

        assert_eq!(capture.created_memory_ids(), vec!["created"]);
    }

    #[test]
    fn step_event_stats_since_returns_only_append_delta() {
        let before = super::StepEventStats {
            turn_rounds: 3,
            cache_hits: 1,
            total_tool_calls: 4,
        };
        let after = super::StepEventStats {
            turn_rounds: 5,
            cache_hits: 2,
            total_tool_calls: 7,
        };
        let delta = after.since(&before);
        assert_eq!(delta.turn_rounds, 2);
        assert_eq!(delta.cache_hits, 1);
        assert_eq!(delta.total_tool_calls, 3);
    }
}

/// Metrics extracted from `step_events.jsonl`.
#[derive(Debug, Default)]
pub struct StepEventStats {
    pub turn_rounds: u32,
    pub cache_hits: u32,
    pub total_tool_calls: u32,
}

impl StepEventStats {
    /// Events added since an earlier snapshot of the same session.
    ///
    /// Multi-turn harness cases execute separate CLI processes against one
    /// session. Stats files are append-only, so reporting the full file for
    /// every process would multiply earlier rounds in the aggregate. Treat a
    /// decreased counter as a rotation/reset rather than underflowing.
    pub fn since(&self, before: &Self) -> Self {
        Self {
            turn_rounds: self.turn_rounds.saturating_sub(before.turn_rounds),
            cache_hits: self.cache_hits.saturating_sub(before.cache_hits),
            total_tool_calls: self
                .total_tool_calls
                .saturating_sub(before.total_tool_calls),
        }
    }

    fn saturating_add_assign(&mut self, other: &Self) {
        self.turn_rounds = self.turn_rounds.saturating_add(other.turn_rounds);
        self.cache_hits = self.cache_hits.saturating_add(other.cache_hits);
        self.total_tool_calls = self.total_tool_calls.saturating_add(other.total_tool_calls);
    }
}

/// Maximum step_events.jsonl file size the stats loader will read.
/// Guards against a pathological file OOM-ing the harness process.
pub const DEFAULT_MAX_STEP_EVENTS_BYTES: u64 = 64 * 1024 * 1024;

/// Parse `~/.astra/sessions/<session_id>/step_events.jsonl` and extract
/// turn rounds and cache hit counts. Returns `None` if the file doesn't
/// exist, is unreadable, or exceeds the size cap.
pub fn load_step_event_stats(session_id: &str) -> Option<StepEventStats> {
    load_step_event_stats_for_owners(session_id, &[astra_services::local_owner_scope()])
}

/// Load step-event counters from the explicit owner namespaces participating
/// in one CLI-to-server run.
pub fn load_step_event_stats_for_owners(
    session_id: &str,
    owner_scopes: &[astra_services::OwnerScope],
) -> Option<StepEventStats> {
    let mut stats = StepEventStats::default();
    let mut found = false;
    for path in session_artifact_paths_for_owners(session_id, owner_scopes)
        .into_iter()
        .filter(|path| path.file_name().and_then(|name| name.to_str()) == Some("step_events.jsonl"))
    {
        if let Some(part) = load_step_event_stats_from_path(&path, DEFAULT_MAX_STEP_EVENTS_BYTES) {
            stats.saturating_add_assign(&part);
            found = true;
        }
    }
    found.then_some(stats)
}

/// Cap-configurable version — test seam.
pub fn load_step_event_stats_from_path(path: &Path, max_bytes: u64) -> Option<StepEventStats> {
    if let Ok(meta) = std::fs::metadata(path)
        && meta.len() > max_bytes
    {
        eprintln!(
            "[astra-test] WARNING: step_events {} bytes exceeds {}-byte cap; skipping stats.",
            meta.len(),
            max_bytes,
        );
        return None;
    }
    let content = std::fs::read_to_string(path).ok()?;
    let mut stats = StepEventStats::default();
    for line in content.lines() {
        if let Ok(ev) = serde_json::from_str::<serde_json::Value>(line) {
            let logical = logical_event(&ev);
            match logical.get("event_type").and_then(|e| e.as_str()) {
                Some("StepStarted") => stats.turn_rounds += 1,
                Some("ToolCallCompleted") => {
                    stats.total_tool_calls += 1;
                    if logical
                        .get("payload")
                        .and_then(|p| p.get("cached"))
                        .and_then(|c| c.as_bool())
                        == Some(true)
                    {
                        stats.cache_hits += 1;
                    }
                }
                _ => {}
            }
        }
    }
    Some(stats)
}
