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
    /// Count of integrity conflicts detected after parsing (for example a
    /// reused nested tool-call identity with divergent payloads). Such a
    /// capture is incomplete even when every JSON line parsed successfully.
    #[serde(default)]
    pub integrity_errors: u32,
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

    /// Whether this capture contains any evidence conflict. The dynamic
    /// nested-call check also covers in-memory test seams and deserialized
    /// reports created before the explicit counter was introduced.
    pub fn has_integrity_errors(&self) -> bool {
        self.integrity_errors > 0 || nested_tool_identity_conflict(&self.events)
    }

    /// Latest canonical conversation turn represented in durable journal
    /// evidence. Child step-event local counters are intentionally excluded.
    pub fn latest_canonical_turn(&self) -> Option<u64> {
        self.events
            .iter()
            .filter(|event| {
                matches!(
                    event.event_type.as_str(),
                    "turn" | "turn_error" | "session_memory_extraction"
                )
            })
            .filter_map(|event| event.raw.get("turn").and_then(|value| value.as_u64()))
            .max()
    }

    /// Verify that this capture contains a canonical turn written by the
    /// terminal run and after the invocation began. A settled session UUID is
    /// not sufficient evidence: an old, already-settled journal can be
    /// replayed with a stale terminal envelope. The producer persists the
    /// server run identity in canonical turn metadata; the timestamp bounds
    /// the identity to this invocation rather than any older turn.
    pub fn has_canonical_run_evidence_since(
        &self,
        run_id: &str,
        started_at: chrono::DateTime<chrono::Utc>,
    ) -> bool {
        if run_id.trim().is_empty() {
            return false;
        }
        self.events.iter().any(|event| {
            if !matches!(event.event_type.as_str(), "turn" | "turn_error") {
                return false;
            }
            let event_run_id = event
                .raw
                .get("metadata")
                .and_then(|metadata| metadata.get("run_id"))
                .and_then(|value| value.as_str())
                .or_else(|| {
                    event
                        .raw
                        .get("producer_scope")
                        .and_then(|scope| scope.get("run_id"))
                        .and_then(|value| value.as_str())
                });
            if event_run_id != Some(run_id) {
                return false;
            }
            let Some(ts) = event.raw.get("ts").and_then(|value| value.as_str()) else {
                return false;
            };
            chrono::DateTime::parse_from_rfc3339(ts)
                .map(|timestamp| timestamp.with_timezone(&chrono::Utc) >= started_at)
                .unwrap_or(false)
        })
    }

    /// Return only journal events causally attributable to this invocation.
    ///
    /// A session UUID is intentionally not an evidence boundary: sessions may
    /// be resumed and therefore contain older turns. Canonical journal rows
    /// carry RFC3339 `ts`; typed StepEvent rows carry epoch-millisecond
    /// `created_at`. Events without a typed run identity and timestamp are not
    /// eligible for current-run certification.
    pub fn scoped_to_invocation(
        &self,
        run_ids: &[String],
        started_at: chrono::DateTime<chrono::Utc>,
    ) -> Self {
        let invocation_run_ids = self.invocation_run_ids_with_descendants(run_ids, started_at);
        let events = self
            .events
            .iter()
            .filter(|event| {
                let Some((run_id, timestamp)) = event_invocation_binding(&event.raw) else {
                    return false;
                };
                invocation_run_ids.contains(run_id) && timestamp >= started_at
            })
            .cloned()
            .collect();

        Self {
            session_id: self.session_id.clone(),
            journal_path: self.journal_path.clone(),
            events,
            skipped_lines: self.skipped_lines,
            dropped_lines: self.dropped_lines,
            integrity_errors: self.integrity_errors,
        }
    }

    /// Expand a root/step invocation through the server-owned delegation
    /// graph. A child run is admissible only when its StepEvent is causally
    /// linked to an admitted `agent`/`agent_fanout` dispatch, or when a complete
    /// typed fanout result names it under the matching parent run. This keeps
    /// resumable-session rows and unrelated child sessions out of hard
    /// evidence while preserving genuine fanout work.
    fn invocation_run_ids_with_descendants(
        &self,
        run_ids: &[String],
        started_at: chrono::DateTime<chrono::Utc>,
    ) -> std::collections::HashSet<String> {
        let mut admitted = run_ids
            .iter()
            .map(String::as_str)
            .filter(|run_id| !run_id.trim().is_empty())
            .map(ToOwned::to_owned)
            .collect::<std::collections::HashSet<_>>();
        if admitted.is_empty() {
            return admitted;
        }

        // Build the causal index once. The subsequent queue walk is O(events
        // + causal edges), rather than rescanning a large resumable journal
        // once per delegation depth.
        let mut children_by_cause = std::collections::HashMap::<String, Vec<String>>::new();
        let mut children_by_parent = std::collections::HashMap::<String, Vec<String>>::new();
        let mut fanout_by_parent =
            std::collections::HashMap::<String, Vec<(String, Vec<String>)>>::new();
        for event in &self.events {
            let Some((run_id, timestamp)) = event_invocation_binding(&event.raw) else {
                continue;
            };
            if timestamp < started_at {
                continue;
            }
            let logical = logical_event(&event.raw);
            // Canonical agent lifecycle rows are the producer-owned
            // delegation edge for server fanout.  They are not StepEvents and
            // therefore have no `caused_by` dispatch chain, but their typed
            // metadata carries the same parent/child identities used by the
            // durable lifecycle writer.  Admit only this exact event kind;
            // never treat an arbitrary parent_run_id in an unrelated row as
            // a descendant edge.
            if event_type(&event.raw) == Some("agent_spawned")
                && let Some(metadata) = event.raw.get("metadata")
            {
                let parent_run_id = metadata
                    .get("parent_run_id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty());
                let child_run_id = metadata
                    .get("run_id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty());
                if let (Some(parent_run_id), Some(child_run_id)) = (parent_run_id, child_run_id)
                    && child_run_id == run_id
                    && child_run_id != parent_run_id
                {
                    let children = children_by_parent
                        .entry(parent_run_id.to_string())
                        .or_default();
                    if !children.iter().any(|seen| seen == child_run_id) {
                        children.push(child_run_id.to_string());
                    }
                }
            }
            if let Some(caused_by) = logical
                .get("caused_by")
                .and_then(serde_json::Value::as_array)
            {
                for cause in caused_by.iter().filter_map(serde_json::Value::as_str) {
                    children_by_cause
                        .entry(cause.to_string())
                        .or_default()
                        .push(run_id.to_string());
                }
            }
            if is_agent_dispatch_event(&event.raw)
                && let Some(event_id) = logical_event_id(&event.raw)
            {
                fanout_by_parent
                    .entry(run_id.to_string())
                    .or_default()
                    .push((
                        event_id.to_string(),
                        fanout_result_child_run_ids(&event.raw, run_id),
                    ));
            }
        }

        let mut pending = std::collections::VecDeque::from_iter(admitted.iter().cloned());
        while let Some(parent_run_id) = pending.pop_front() {
            for child_run_id in children_by_parent.get(&parent_run_id).into_iter().flatten() {
                if admitted.insert(child_run_id.clone()) {
                    pending.push_back(child_run_id.clone());
                }
            }
            for (dispatch_id, result_child_run_ids) in
                fanout_by_parent.get(&parent_run_id).into_iter().flatten()
            {
                for child_run_id in result_child_run_ids
                    .iter()
                    .chain(children_by_cause.get(dispatch_id).into_iter().flatten())
                {
                    if admitted.insert(child_run_id.clone()) {
                        pending.push_back(child_run_id.clone());
                    }
                }
            }
        }

        admitted
    }

    /// Count canonical user-facing turns whose persisted tool surface exposes
    /// `tool_name`.  `turn` is deliberately used instead of child-step
    /// events: children may legitimately receive narrower, attempt-bound
    /// tools that must never leak into the coordinator's catalog.
    pub fn canonical_turn_tool_surface_count(&self, tool_name: &str) -> (usize, usize) {
        let mut turns = 0;
        let mut exposed = 0;
        for event in &self.events {
            if event.event_type != "turn" {
                continue;
            }
            let Some(visible_tools) = event
                .raw
                .get("visible_tools")
                .and_then(|value| value.as_array())
            else {
                continue;
            };
            turns += 1;
            if visible_tools
                .iter()
                .any(|value| value.as_str() == Some(tool_name))
            {
                exposed += 1;
            }
        }
        (turns, exposed)
    }

    /// Count provider rounds whose durable action union contains `action` for
    /// `tool_name`. This is stronger than `canonical_turn_tool_surface_count`:
    /// a tool name may be present while an authority projection removes the
    /// requested branch (for example `agent_fanout.start`).
    pub fn canonical_round_tool_action_count(
        &self,
        tool_name: &str,
        action: &str,
    ) -> (usize, usize) {
        let mut rounds = 0;
        let mut exposed = 0;
        for event in &self.events {
            if event.event_type != "llm_round" {
                continue;
            }
            let Some(actions) = event
                .raw
                .get("metadata")
                .and_then(|metadata| metadata.get("visible_tool_actions"))
                .and_then(|value| value.as_object())
            else {
                continue;
            };
            rounds += 1;
            if actions
                .get(tool_name)
                .and_then(|value| value.as_array())
                .is_some_and(|values| values.iter().any(|value| value.as_str() == Some(action)))
            {
                exposed += 1;
            }
        }
        (rounds, exposed)
    }

    /// Return the first root provider round that was asked to continue an
    /// assigned WorkItem without the typed settlement operation on its exact
    /// wire surface.
    ///
    /// Older journals do not persist round-level tool names; those rounds are
    /// skipped so backward compatibility does not become a false failure.
    /// New captures record this bounded projection specifically so the
    /// harness can diagnose a broken lifecycle surface at its first boundary
    /// instead of only observing the later "assigned but never settled"
    /// consequence.
    pub fn first_work_assignment_surface_gap(&self) -> Option<String> {
        fn tool_result(call: &serde_json::Value) -> Option<serde_json::Value> {
            call.get("result").cloned().or_else(|| {
                call.get("result_full")
                    .and_then(serde_json::Value::as_str)
                    .and_then(|raw| serde_json::from_str(raw).ok())
            })
        }

        let mut assignment_active = false;
        for event in &self.events {
            if event.event_type != "llm_round"
                || event
                    .raw
                    .get("producer_scope")
                    .and_then(|scope| scope.get("agent_id"))
                    .and_then(serde_json::Value::as_str)
                    .is_some()
                || event
                    .raw
                    .get("metadata")
                    .and_then(|metadata| metadata.get("purpose"))
                    .and_then(serde_json::Value::as_str)
                    != Some("primary_agent")
            {
                continue;
            }

            if assignment_active
                && let Some(visible_tools) = event
                    .raw
                    .get("metadata")
                    .and_then(|metadata| metadata.get("visible_tools"))
                    .and_then(serde_json::Value::as_array)
                && !visible_tools
                    .iter()
                    .any(|name| name.as_str() == Some("settle_work_item"))
            {
                let round = event
                    .raw
                    .get("round")
                    .and_then(serde_json::Value::as_u64)
                    .map(|round| round.to_string())
                    .unwrap_or_else(|| "unknown".to_string());
                return Some(format!(
                    "provider round {round} had an active WorkItem assignment but its exact wire surface omitted settle_work_item"
                ));
            }

            for call in event
                .raw
                .get("tool_calls")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
            {
                if call.get("ok").and_then(serde_json::Value::as_bool) == Some(false) {
                    continue;
                }
                let Some(name) = call.get("name").and_then(serde_json::Value::as_str) else {
                    continue;
                };
                let Some(result) = tool_result(call) else {
                    continue;
                };
                let status = result.get("status").and_then(serde_json::Value::as_str);
                match name {
                    "start_work" if status == Some("started") => {
                        assignment_active = result
                            .get("initial_task")
                            .and_then(|task| task.get("status"))
                            .and_then(serde_json::Value::as_str)
                            == Some("assigned");
                    }
                    "run_next_work_item" if status == Some("assigned") => {
                        assignment_active = true;
                    }
                    "settle_work_item" if status == Some("recorded") => {
                        assignment_active = result
                            .get("next_task")
                            .and_then(|task| task.get("status"))
                            .and_then(serde_json::Value::as_str)
                            == Some("assigned");
                    }
                    _ => {}
                }
            }
        }
        None
    }

    /// True only when `subsystem` settled for the latest canonical turn.
    /// An older marker from a resumed long session is not completion evidence
    /// for the current run.
    pub fn subsystem_settled_for_latest_turn(&self, subsystem: &str) -> bool {
        let Some(latest_turn) = self.latest_canonical_turn() else {
            return false;
        };
        self.events.iter().any(|event| {
            event.event_type == "subsystem_settled"
                && event.raw.get("turn").and_then(|value| value.as_u64()) == Some(latest_turn)
                && event
                    .raw
                    .get("metadata")
                    .and_then(|value| value.get("subsystem"))
                    .and_then(|value| value.as_str())
                    == Some(subsystem)
        })
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
        // Nested turn records are the authoritative legacy tool surface. Use
        // the same identity/conflict gate as the richer call projections so a
        // reused call id with divergent arguments/results cannot still make a
        // JournalToolCalled criterion pass through this simpler view.
        for call in self.journal_tool_calls() {
            push_unique(&call.name, &mut out);
        }
        for e in &self.events {
            // Shape 1: legacy llm_round with nested tool_calls array.
            if e.event_type == "llm_round" || e.event_type == "turn" {
                // Nested records were already projected above with identity
                // conflict detection; do not re-read them as an unchecked
                // alternate path.
                continue;
            }
            // Shape 2: step-events ToolCallCompleted.
            if e.event_type == "ToolCallCompleted" {
                let name = e
                    .raw
                    .get("payload")
                    .and_then(|p| p.get("tool_name"))
                    .and_then(|n| n.as_str());
                if let Some(name) = name {
                    push_unique(name, &mut out);
                }
            }
        }
        out
    }

    /// Count successful typed child tool completions causally descended from
    /// a parent dispatch tool. The child run must differ from the dispatching
    /// run; a coordinator using the same tool itself is not child evidence.
    pub fn causal_child_tool_call_count(&self, parent_tool: &str, child_tool: &str) -> usize {
        let mut children_by_cause = std::collections::HashMap::<String, Vec<String>>::new();
        let mut event_run_ids = std::collections::HashMap::<String, String>::new();
        let mut completed_tools =
            std::collections::HashMap::<String, (String, String, bool)>::new();
        let mut dispatches = Vec::<(String, String)>::new();

        for event in &self.events {
            if !is_step_event_type(&event.raw) {
                continue;
            }
            let logical = logical_event(&event.raw);
            let Some(event_id) = logical_event_id(&event.raw) else {
                continue;
            };
            let Some(run_id) = logical
                .get("run_id")
                .and_then(serde_json::Value::as_str)
                .filter(|run_id| !run_id.trim().is_empty())
            else {
                continue;
            };
            event_run_ids.insert(event_id.to_string(), run_id.to_string());
            if let Some(caused_by) = logical
                .get("caused_by")
                .and_then(serde_json::Value::as_array)
            {
                for cause in caused_by.iter().filter_map(serde_json::Value::as_str) {
                    children_by_cause
                        .entry(cause.to_string())
                        .or_default()
                        .push(event_id.to_string());
                }
            }

            let Some(payload) = logical.get("payload") else {
                continue;
            };
            let Some(tool_name) = payload.get("tool_name").and_then(serde_json::Value::as_str)
            else {
                continue;
            };
            match event_type(logical) {
                Some("ToolCallStarted") if tool_name == parent_tool => {
                    dispatches.push((event_id.to_string(), run_id.to_string()));
                }
                Some("ToolCallCompleted") => {
                    completed_tools.insert(
                        event_id.to_string(),
                        (
                            run_id.to_string(),
                            tool_name.to_string(),
                            payload.get("is_error").and_then(serde_json::Value::as_bool)
                                == Some(false),
                        ),
                    );
                    if tool_name == parent_tool
                        && payload.get("is_error").and_then(serde_json::Value::as_bool)
                            == Some(false)
                    {
                        dispatches.push((event_id.to_string(), run_id.to_string()));
                    }
                }
                _ => {}
            }
        }

        let mut child_event_ids = std::collections::HashSet::new();
        for (dispatch_id, parent_run_id) in dispatches {
            let mut pending = std::collections::VecDeque::from([dispatch_id]);
            let mut reachable = std::collections::HashSet::new();
            while let Some(event_id) = pending.pop_front() {
                if !reachable.insert(event_id.clone()) {
                    continue;
                }
                for child_event_id in children_by_cause
                    .get(&event_id)
                    .into_iter()
                    .flatten()
                    .cloned()
                {
                    pending.push_back(child_event_id);
                }
            }
            for event_id in reachable {
                let Some((run_id, tool_name, succeeded)) = completed_tools.get(&event_id) else {
                    continue;
                };
                if *succeeded && run_id != &parent_run_id && tool_name == child_tool {
                    child_event_ids.insert(event_id);
                }
            }
        }
        child_event_ids.len()
    }

    /// Complete, de-duplicated tool calls persisted in turn records.
    pub fn journal_tool_calls(&self) -> Vec<JournalToolCall> {
        if nested_tool_identity_conflict(&self.events) {
            return Vec::new();
        }
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
                let arguments =
                    embedded_json(record.get("args_full").or_else(|| record.get("args")));
                let result =
                    embedded_json(record.get("result_full").or_else(|| record.get("result")));
                if let Some(call_id) = &call_id
                    && !seen_ids.insert(nested_tool_identity(event, call_id))
                {
                    continue;
                }
                calls.push(JournalToolCall {
                    call_id,
                    name: name.to_string(),
                    ok: record.get("ok").and_then(|value| value.as_bool()),
                    arguments,
                    result,
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

/// Extract the producer-owned invocation binding from either journal family.
/// Canonical session events carry RFC3339 `ts` plus metadata.run_id. Typed
/// StepEvent artifacts carry run_id and epoch-ms created_at on the event
/// itself. No session/task/step label is accepted as a substitute identity.
fn event_invocation_binding(
    raw: &serde_json::Value,
) -> Option<(&str, chrono::DateTime<chrono::Utc>)> {
    let logical = logical_event(raw);
    let run_id = logical
        .get("run_id")
        .and_then(|value| value.as_str())
        .or_else(|| {
            raw.get("metadata")
                .and_then(|metadata| metadata.get("run_id"))
                .and_then(|value| value.as_str())
        })
        .or_else(|| {
            raw.get("producer_scope")
                .and_then(|scope| scope.get("run_id"))
                .and_then(|value| value.as_str())
        })
        .or_else(|| raw.get("run_id").and_then(|value| value.as_str()))
        .or_else(|| {
            logical
                .get("payload")
                .and_then(|payload| payload.get("trace_context"))
                .and_then(|context| context.get("run_id"))
                .and_then(|value| value.as_str())
        })
        .filter(|value| !value.trim().is_empty())?;

    if let Some(ts) = raw.get("ts").and_then(|value| value.as_str()) {
        let timestamp = chrono::DateTime::parse_from_rfc3339(ts)
            .ok()?
            .with_timezone(&chrono::Utc);
        return Some((run_id, timestamp));
    }
    let created_at = logical.get("created_at").and_then(|value| value.as_u64())?;
    let created_at = i64::try_from(created_at).ok()?;
    Some((run_id, chrono::DateTime::from_timestamp_millis(created_at)?))
}

fn logical_event_id(raw: &serde_json::Value) -> Option<&str> {
    let logical = logical_event(raw);
    event_id(logical).or_else(|| event_id(raw))
}

/// Whether an event is a server-owned delegation dispatch whose causal children
/// may be admitted to the same invocation. Completed events must be
/// successful; a failed result cannot authorize arbitrary child identities.
fn is_agent_dispatch_event(raw: &serde_json::Value) -> bool {
    if !is_step_event_type(raw) || logical_event_id(raw).is_none() {
        return false;
    }
    let logical = logical_event(raw);
    match event_type(logical) {
        Some("ToolCallStarted") => {
            let Some(payload) = logical.get("payload") else {
                return false;
            };
            matches!(
                payload.get("tool_name").and_then(serde_json::Value::as_str),
                Some("agent" | "agent_fanout")
            ) && payload
                .get("call_id")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|call_id| !call_id.trim().is_empty())
        }
        Some("ToolCallCompleted") => {
            let Some(payload) = logical.get("payload") else {
                return false;
            };
            matches!(
                payload.get("tool_name").and_then(serde_json::Value::as_str),
                Some("agent" | "agent_fanout")
            ) && payload
                .get("call_id")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|call_id| !call_id.trim().is_empty())
                && payload.get("is_error").and_then(serde_json::Value::as_bool) == Some(false)
        }
        _ => false,
    }
}

/// Extract child run identities only from a complete fanout result whose
/// typed parent identity matches the dispatching run. Truncated previews,
/// malformed JSON, missing per-agent projections, and foreign parent IDs are
/// intentionally non-authorizing.
fn fanout_result_child_run_ids(raw: &serde_json::Value, parent_run_id: &str) -> Vec<String> {
    let logical = logical_event(raw);
    let mut result_values = Vec::new();

    if event_type(logical) == Some("ToolCallCompleted") {
        let Some(payload) = logical.get("payload") else {
            return Vec::new();
        };
        if payload.get("tool_name").and_then(serde_json::Value::as_str) == Some("agent_fanout")
            && payload.get("is_error").and_then(serde_json::Value::as_bool) == Some(false)
            && let Some(result) = embedded_json(payload.get("output"))
        {
            result_values.push(result);
        }
    }

    let mut child_run_ids = Vec::new();
    for result in result_values {
        let Some(fanout) = result.get("fanout").and_then(serde_json::Value::as_object) else {
            continue;
        };
        if fanout
            .get("parent_run_id")
            .and_then(serde_json::Value::as_str)
            != Some(parent_run_id)
        {
            continue;
        }
        // A response has exactly one canonical per-agent projection. New
        // launch receipts use `agents`, result collection uses `results`,
        // and older control responses use `fanout.slots`. Never concatenate
        // these views: a response that carries both a compatibility list and
        // its canonical list must still authorize each child once.
        let per_agent_entries = result
            .get("agents")
            .and_then(serde_json::Value::as_array)
            .filter(|entries| !entries.is_empty())
            .or_else(|| {
                result
                    .get("results")
                    .and_then(serde_json::Value::as_array)
                    .filter(|entries| !entries.is_empty())
            })
            .or_else(|| fanout.get("slots").and_then(serde_json::Value::as_array));
        let Some(per_agent_entries) = per_agent_entries else {
            continue;
        };
        for run_id in per_agent_entries.iter().filter_map(|slot| {
            slot.get("run_id")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|run_id| !run_id.is_empty())
        }) {
            if !child_run_ids.iter().any(|seen| seen == run_id) {
                child_run_ids.push(run_id.to_string());
            }
        }
    }
    child_run_ids
}

fn embedded_json(value: Option<&serde_json::Value>) -> Option<serde_json::Value> {
    match value? {
        serde_json::Value::String(text) => serde_json::from_str(text)
            .ok()
            .or_else(|| Some(serde_json::Value::String(text.clone()))),
        value => Some(value.clone()),
    }
}

fn nested_tool_identity_conflict(events: &[JournalEvent]) -> bool {
    let mut seen = std::collections::HashMap::<String, String>::new();
    for event in events {
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
            let Some(call_id) = record
                .get("tool_call_id")
                .or_else(|| record.get("call_id"))
                .and_then(|value| value.as_str())
                .filter(|value| !value.trim().is_empty())
            else {
                continue;
            };
            let Some(name) = record.get("name").and_then(|value| value.as_str()) else {
                return true;
            };
            let fingerprint = serde_json::to_string(&serde_json::json!({
                "name": name,
                "ok": record.get("ok"),
                "arguments": embedded_json(record.get("args_full").or_else(|| record.get("args"))),
                "result": embedded_json(
                    record.get("result_full").or_else(|| record.get("result")),
                ),
            }))
            .unwrap_or_default();
            let identity = nested_tool_identity(event, call_id);
            if let Some(existing) = seen.get(&identity)
                && existing != &fingerprint
            {
                return true;
            }
            seen.insert(identity, fingerprint);
        }
    }
    false
}

fn nested_tool_identity(event: &JournalEvent, call_id: &str) -> String {
    let run_id = event
        .raw
        .get("producer_scope")
        .and_then(|scope| scope.get("run_id"))
        .or_else(|| event.raw.get("run_id"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("legacy-unscoped");
    format!("{run_id}:{call_id}")
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

/// Return the discriminator from either journal representation. A syntactically
/// valid JSON value without a non-empty discriminator is not an event: keeping
/// it would make a truncated/foreign payload look like complete evidence.
fn event_type(value: &serde_json::Value) -> Option<&str> {
    let object = value.as_object()?;
    ["type", "event_type"]
        .into_iter()
        .filter_map(|key| object.get(key).and_then(|value| value.as_str()))
        .find(|value| !value.trim().is_empty())
}

fn is_step_event_type(value: &serde_json::Value) -> bool {
    value.get("artifact_kind").and_then(|kind| kind.as_str()) == Some("step_event")
        || matches!(
            event_type(value),
            Some(
                "StepCreated"
                    | "StepAssigned"
                    | "StepStarted"
                    | "StepCompleted"
                    | "StepIncomplete"
                    | "StepEvaluated"
                    | "StepFailed"
                    | "StepRetried"
                    | "LlmRoundStarted"
                    | "LlmRoundCompleted"
                    | "ToolCallStarted"
                    | "ToolCallCompleted"
                    | "ToolCallFailed"
                    | "ToolCallSkipped"
                    | "ToolsConverged"
                    | "CheckpointSaved"
                    | "CheckpointRestored"
                    | "MemoryRetrieved"
                    | "MemoryRecorded"
                    | "MemoryGovernanceApplied"
                    | "CompactionFired"
                    | "StallDetected"
                    | "DivergenceDetected"
                    | "RetryScheduled"
            )
        )
}

/// Stable identity for the current step-event protocol. Legacy session
/// journal events do not carry an event id, so the caller may use an exact
/// payload fingerprint when merging those records. Step events, however, are
/// required to carry `event_id` so counters can be deduplicated across owner
/// artifacts without guessing from turn numbers.
fn event_id(value: &serde_json::Value) -> Option<&str> {
    let object = value.as_object()?;
    object
        .get("event_id")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
}

fn logical_with_event_id(value: &serde_json::Value) -> serde_json::Value {
    let logical = logical_event(value);
    let id = event_id(value).or_else(|| event_id(logical));
    let mut logical = logical.clone();
    if let Some(id) = id
        && let Some(object) = logical.as_object_mut()
        && !object.contains_key("event_id")
    {
        object.insert("event_id".into(), serde_json::Value::String(id.into()));
    }
    logical
}

fn canonical_json(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(fields) => {
            let mut ordered = std::collections::BTreeMap::new();
            for (key, value) in fields {
                ordered.insert(key.clone(), canonical_json(value));
            }
            serde_json::Value::Object(ordered.into_iter().collect())
        }
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.iter().map(canonical_json).collect())
        }
        _ => value.clone(),
    }
}

fn canonical_event_fingerprint(value: &serde_json::Value) -> String {
    serde_json::to_string(&canonical_json(value)).unwrap_or_default()
}

fn legacy_event_is_valid(value: &serde_json::Value, session_id: &str) -> bool {
    let Ok(event) =
        serde_json::from_value::<astra_services::session_journal::JournalEvent>(value.clone())
    else {
        return false;
    };
    if event.ts.trim().is_empty() {
        return false;
    }
    event
        .session_id
        .as_deref()
        .is_none_or(|event_session| event_session == session_id)
}

fn legacy_event_identity(value: &serde_json::Value) -> Option<String> {
    let object = value.as_object()?;
    let event_type = event_type(value)?;
    let ts = object.get("ts")?.as_str()?;
    let session_id = object
        .get("session_id")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let producer_run = object
        .get("producer_scope")
        .and_then(|scope| scope.get("run_id"))
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let turn = object
        .get("turn")
        .and_then(|value| value.as_u64())
        .map(|value| value.to_string())
        .unwrap_or_default();
    let agentic_step = object
        .get("agentic_step")
        .and_then(|value| value.as_u64())
        .map(|value| value.to_string())
        .unwrap_or_default();
    Some(format!(
        "legacy:{event_type}|{ts}|{session_id}|{producer_run}|{turn}|{agentic_step}"
    ))
}

fn event_identity_key(value: &serde_json::Value) -> String {
    event_id(value)
        .map(|id| format!("step:{id}"))
        .or_else(|| legacy_event_identity(value))
        .unwrap_or_else(|| format!("raw:{}", canonical_event_fingerprint(value)))
}

fn valid_step_event_shape(value: &serde_json::Value) -> bool {
    let Ok(event) =
        serde_json::from_value::<astra_pipeline::step_protocol::StepEvent>(value.clone())
    else {
        return false;
    };
    if event.event_id.trim().is_empty()
        || event.run_id.trim().is_empty()
        || event.step_id.trim().is_empty()
    {
        return false;
    }
    if event.caused_by.iter().any(|id| id.trim().is_empty()) {
        return false;
    }
    match event.event_type {
        astra_pipeline::step_protocol::StepEventType::ToolCallStarted
        | astra_pipeline::step_protocol::StepEventType::ToolCallCompleted
        | astra_pipeline::step_protocol::StepEventType::ToolCallFailed
        | astra_pipeline::step_protocol::StepEventType::ToolCallSkipped => {
            let Some(payload) = event.payload.as_ref().and_then(|value| value.as_object()) else {
                return false;
            };
            let has_tool_name = payload
                .get("tool_name")
                .and_then(|value| value.as_str())
                .is_some_and(|value| !value.trim().is_empty());
            let has_call_id = payload
                .get("call_id")
                .and_then(|value| value.as_str())
                .is_some_and(|value| !value.trim().is_empty());
            if !has_tool_name || !has_call_id {
                return false;
            }
            if matches!(
                event.event_type,
                astra_pipeline::step_protocol::StepEventType::ToolCallCompleted
                    | astra_pipeline::step_protocol::StepEventType::ToolCallFailed
            ) && (!payload
                .get("cached")
                .is_some_and(serde_json::Value::is_boolean)
                || !payload
                    .get("is_error")
                    .is_some_and(serde_json::Value::is_boolean))
            {
                return false;
            }
        }
        _ => {}
    }
    true
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
        .filter_map(|path| load_session_from_path(session_id, &path))
        .collect::<Vec<_>>();
    let primary_index = captures
        .iter()
        .enumerate()
        .max_by_key(|(_, capture)| capture.events.len())
        .map(|(index, _)| index)?;
    // The capture is a union, but one path is retained for diagnostics. Point
    // it at the richest contributing artifact instead of whichever owner was
    // listed first; CLI profile journals are often only a thin permission lane
    // while the account-scoped server journal contains the model/tool trace.
    let mut capture = captures.swap_remove(primary_index);
    for additional in captures {
        let mut seen = std::collections::HashMap::<String, String>::new();
        for event in &capture.events {
            let key = event_identity_key(&event.raw);
            seen.insert(key, canonical_event_fingerprint(&event.raw));
        }
        for event in additional.events {
            let fingerprint = canonical_event_fingerprint(&event.raw);
            let key = event_identity_key(&event.raw);
            match seen.get(&key) {
                Some(existing) if existing == &fingerprint => {
                    // The same owner-mirrored event is already represented.
                }
                Some(_) => {
                    // One stable event id with conflicting payloads cannot be
                    // resolved by the harness. Keep the first copy for
                    // diagnostics, but mark the capture incomplete so no
                    // criterion can certify the ambiguous union.
                    capture.skipped_lines = capture.skipped_lines.saturating_add(1);
                    capture.integrity_errors = capture.integrity_errors.saturating_add(1);
                }
                None => {
                    seen.insert(key, fingerprint);
                    capture.events.push(event);
                }
            }
        }
        capture.skipped_lines = capture
            .skipped_lines
            .saturating_add(additional.skipped_lines);
        capture.dropped_lines = capture
            .dropped_lines
            .saturating_add(additional.dropped_lines);
        capture.integrity_errors = capture
            .integrity_errors
            .saturating_add(additional.integrity_errors);
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
    let mut seen_event_ids = std::collections::HashMap::<String, String>::new();
    let mut integrity_errors: u32 = 0;
    let mut skipped: u32 = 0;
    let mut dropped_from_head: u32 = 0;
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<serde_json::Value>(line) {
            Ok(v) => {
                let raw_logical = logical_event(&v);
                if is_step_event_type(raw_logical)
                    && (event_id(raw_logical).is_none() || !valid_step_event_shape(raw_logical))
                {
                    // Step-event evidence is a typed production artifact. A
                    // discriminator alone is not enough to certify a tool or
                    // lifecycle event; reject partial rows so required
                    // journal criteria fail closed.
                    skipped = skipped.saturating_add(1);
                    continue;
                }
                if !is_step_event_type(raw_logical)
                    && !legacy_event_is_valid(raw_logical, session_id)
                {
                    skipped = skipped.saturating_add(1);
                    continue;
                }
                let logical = logical_with_event_id(&v);
                let Some(event_type) = event_type(&logical).map(str::to_string) else {
                    // Valid JSON is not automatically a valid journal event.
                    // Reject scalars, empty objects, and wrong-typed/missing
                    // discriminators as incomplete evidence instead of
                    // retaining an event_type="" placeholder.
                    skipped = skipped.saturating_add(1);
                    continue;
                };
                let identity = event_id(&logical)
                    .map(|id| format!("step:{id}"))
                    .or_else(|| legacy_event_identity(&logical));
                if let Some(identity) = identity {
                    let fingerprint = canonical_event_fingerprint(&logical);
                    if let Some(existing) = seen_event_ids.get(&identity) {
                        if existing != &fingerprint {
                            skipped = skipped.saturating_add(1);
                            integrity_errors = integrity_errors.saturating_add(1);
                        }
                        continue;
                    }
                    seen_event_ids.insert(identity, fingerprint);
                }
                if events.len() >= max_events {
                    events.pop_front();
                    dropped_from_head = dropped_from_head.saturating_add(1);
                }
                events.push_back(JournalEvent {
                    event_type,
                    raw: logical,
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
    integrity_errors = integrity_errors.saturating_add(u32::from(nested_tool_identity_conflict(
        &events.iter().cloned().collect::<Vec<_>>(),
    )));
    Some(SessionCapture {
        session_id: session_id.to_string(),
        journal_path: path.to_path_buf(),
        events: events.into(),
        skipped_lines: skipped,
        dropped_lines: dropped_from_head,
        integrity_errors,
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
            r#"{"type":"llm_request_full","ts":"2026-08-09T00:00:00Z","session_id":"s1"}"#,
            r#"{"type":"llm_round","ts":"2026-08-09T00:00:01Z","session_id":"s1","turn":1}"#,
            r#"{"type":"llm_round","ts":"2026-08-09T00:00:02Z","session_id":"s1","turn":2}"#,
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
    fn canonical_run_evidence_requires_matching_run_id_and_fresh_timestamp() {
        let started_at = chrono::DateTime::parse_from_rfc3339("2026-08-09T00:00:02Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let capture = SessionCapture {
            session_id: "s".into(),
            journal_path: PathBuf::from("/fake"),
            events: vec![
                JournalEvent {
                    event_type: "turn".into(),
                    raw: serde_json::json!({
                        "type": "turn",
                        "ts": "2026-08-09T00:00:01Z",
                        "metadata": {"run_id": "old"},
                        "turn": 1
                    }),
                },
                JournalEvent {
                    event_type: "turn".into(),
                    raw: serde_json::json!({
                        "type": "turn",
                        "ts": "2026-08-09T00:00:03Z",
                        "metadata": {"run_id": "new"},
                        "turn": 2
                    }),
                },
            ],
            skipped_lines: 0,
            dropped_lines: 0,
            integrity_errors: 0,
        };
        assert!(!capture.has_canonical_run_evidence_since("old", started_at));
        assert!(capture.has_canonical_run_evidence_since("new", started_at));
        assert!(!capture.has_canonical_run_evidence_since("missing", started_at));
    }

    #[test]
    fn scoped_invocation_preserves_typed_step_events_and_drops_old_runs() {
        let started_at = chrono::DateTime::parse_from_rfc3339("2026-08-09T00:00:02Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let current_ms = started_at.timestamp_millis() as u64 + 1_000;
        let capture = SessionCapture {
            session_id: "s".into(),
            journal_path: PathBuf::from("/fake"),
            events: vec![
                JournalEvent {
                    event_type: "ToolCallCompleted".into(),
                    raw: serde_json::json!({
                        "event_id": "old-tool",
                        "run_id": "old-run",
                        "event_type": "ToolCallCompleted",
                        "step_id": "old-step",
                        "caused_by": [],
                        "created_at": 1_754_697_601_000u64,
                        "payload": {
                            "tool_name": "read_file",
                            "call_id": "old-call",
                            "cached": false,
                            "is_error": false
                        }
                    }),
                },
                JournalEvent {
                    event_type: "StepCompleted".into(),
                    raw: serde_json::json!({
                        "event_id": "current-step",
                        "run_id": "current-run",
                        "event_type": "StepCompleted",
                        "step_id": "current-step",
                        "caused_by": [],
                        "created_at": current_ms,
                        "payload": {}
                    }),
                },
                JournalEvent {
                    event_type: "pipeline_feedback".into(),
                    raw: serde_json::json!({
                        "type": "pipeline_feedback",
                        "ts": "2026-08-09T00:00:03Z",
                        "session_id": "s",
                        "producer_scope": {"run_id": "current-run"},
                        "turn": 2,
                        "metadata": {"cache_hit_ratio": 0.9}
                    }),
                },
            ],
            skipped_lines: 0,
            dropped_lines: 0,
            integrity_errors: 0,
        };
        let scoped = capture.scoped_to_invocation(&["current-run".into()], started_at);
        assert_eq!(scoped.count_events("ToolCallCompleted"), 0);
        assert_eq!(scoped.count_events("StepCompleted"), 1);
        assert_eq!(scoped.count_events("pipeline_feedback"), 1);
    }

    #[test]
    fn scoped_invocation_follows_typed_fanout_causality_to_child_tools() {
        let started_at = chrono::DateTime::parse_from_rfc3339("2026-08-09T00:00:02Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let root_ms = started_at.timestamp_millis() as u64 + 1_000;
        let child_ms = root_ms + 1;
        let capture = SessionCapture {
            session_id: "fanout-session".into(),
            journal_path: PathBuf::from("/fake"),
            events: vec![
                JournalEvent {
                    event_type: "ToolCallStarted".into(),
                    raw: serde_json::json!({
                        "event_id": "fanout-start",
                        "run_id": "root-run",
                        "event_type": "ToolCallStarted",
                        "step_id": "root-step",
                        "caused_by": [],
                        "created_at": root_ms,
                        "payload": {
                            "tool_name": "agent_fanout",
                            "call_id": "fanout-call"
                        }
                    }),
                },
                JournalEvent {
                    event_type: "StepCreated".into(),
                    raw: serde_json::json!({
                        "event_id": "child-created",
                        "run_id": "child-run",
                        "event_type": "StepCreated",
                        "step_id": "child-step",
                        "caused_by": ["fanout-start"],
                        "created_at": child_ms,
                        "payload": {}
                    }),
                },
                JournalEvent {
                    event_type: "ToolCallCompleted".into(),
                    raw: serde_json::json!({
                        "event_id": "root-read",
                        "run_id": "root-run",
                        "event_type": "ToolCallCompleted",
                        "step_id": "root-step",
                        "caused_by": ["fanout-start"],
                        "created_at": child_ms,
                        "payload": {
                            "tool_name": "read_file",
                            "call_id": "root-read-call",
                            "cached": false,
                            "is_error": false
                        }
                    }),
                },
                JournalEvent {
                    event_type: "ToolCallCompleted".into(),
                    raw: serde_json::json!({
                        "event_id": "child-read",
                        "run_id": "child-run",
                        "event_type": "ToolCallCompleted",
                        "step_id": "child-step",
                        "caused_by": ["child-created"],
                        "created_at": child_ms + 1,
                        "payload": {
                            "tool_name": "read_file",
                            "call_id": "read-call",
                            "cached": false,
                            "is_error": false
                        }
                    }),
                },
                JournalEvent {
                    event_type: "ToolCallCompleted".into(),
                    raw: serde_json::json!({
                        "event_id": "foreign-read",
                        "run_id": "unrelated-run",
                        "event_type": "ToolCallCompleted",
                        "step_id": "foreign-step",
                        "caused_by": [],
                        "created_at": child_ms + 2,
                        "payload": {
                            "tool_name": "read_file",
                            "call_id": "foreign-read-call",
                            "cached": false,
                            "is_error": false
                        }
                    }),
                },
            ],
            skipped_lines: 0,
            dropped_lines: 0,
            integrity_errors: 0,
        };

        let scoped = capture.scoped_to_invocation(&["root-run".into()], started_at);
        assert_eq!(scoped.count_events("ToolCallStarted"), 1);
        assert_eq!(scoped.count_events("StepCreated"), 1);
        assert_eq!(scoped.count_events("ToolCallCompleted"), 2);
        assert_eq!(scoped.tools_invoked(), vec!["read_file"]);
        assert_eq!(
            scoped.causal_child_tool_call_count("agent_fanout", "read_file"),
            1
        );
        assert!(!scoped.events.iter().any(|event| {
            event.raw.get("run_id").and_then(serde_json::Value::as_str) == Some("unrelated-run")
        }));
    }

    #[test]
    fn scoped_invocation_follows_canonical_agent_lifecycle_children() {
        let started_at = chrono::DateTime::parse_from_rfc3339("2026-08-09T00:00:02Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let capture = SessionCapture {
            session_id: "canonical-fanout-session".into(),
            journal_path: PathBuf::from("/fake"),
            events: vec![
                JournalEvent {
                    event_type: "agent_spawned".into(),
                    raw: serde_json::json!({
                        "type": "agent_spawned",
                        "ts": "2026-08-09T00:00:03Z",
                        "session_id": "canonical-fanout-session",
                        "metadata": {
                            "parent_run_id": "root-run",
                            "run_id": "child-a",
                            "agent_type": "explore"
                        }
                    }),
                },
                JournalEvent {
                    event_type: "agent_spawned".into(),
                    raw: serde_json::json!({
                        "type": "agent_spawned",
                        "ts": "2026-08-09T00:00:03Z",
                        "session_id": "canonical-fanout-session",
                        "metadata": {
                            "parent_run_id": "root-run",
                            "run_id": "child-b",
                            "agent_type": "explore"
                        }
                    }),
                },
                JournalEvent {
                    event_type: "agent_terminated".into(),
                    raw: serde_json::json!({
                        "type": "agent_terminated",
                        "ts": "2026-08-09T00:00:04Z",
                        "session_id": "canonical-fanout-session",
                        "metadata": {
                            "run_id": "child-a",
                            "status": "completed"
                        }
                    }),
                },
                JournalEvent {
                    event_type: "agent_terminated".into(),
                    raw: serde_json::json!({
                        "type": "agent_terminated",
                        "ts": "2026-08-09T00:00:04Z",
                        "session_id": "canonical-fanout-session",
                        "metadata": {
                            "run_id": "child-b",
                            "status": "completed"
                        }
                    }),
                },
                JournalEvent {
                    event_type: "agent_spawned".into(),
                    raw: serde_json::json!({
                        "type": "agent_spawned",
                        "ts": "2026-08-09T00:00:03Z",
                        "session_id": "canonical-fanout-session",
                        "metadata": {
                            "parent_run_id": "foreign-root",
                            "run_id": "foreign-child",
                            "agent_type": "explore"
                        }
                    }),
                },
                JournalEvent {
                    event_type: "agent_terminated".into(),
                    raw: serde_json::json!({
                        "type": "agent_terminated",
                        "ts": "2026-08-09T00:00:04Z",
                        "session_id": "canonical-fanout-session",
                        "metadata": {
                            "run_id": "foreign-child",
                            "status": "completed"
                        }
                    }),
                },
            ],
            skipped_lines: 0,
            dropped_lines: 0,
            integrity_errors: 0,
        };

        let scoped = capture.scoped_to_invocation(&["root-run".into()], started_at);
        assert_eq!(scoped.count_events("agent_spawned"), 2);
        assert_eq!(scoped.count_events("agent_terminated"), 2);
        assert!(!scoped.events.iter().any(|event| {
            event
                .raw
                .get("metadata")
                .and_then(|metadata| metadata.get("run_id"))
                .and_then(serde_json::Value::as_str)
                == Some("foreign-child")
        }));
    }

    #[test]
    fn scoped_invocation_follows_typed_single_agent_causality_to_child_tools() {
        let started_at = chrono::DateTime::parse_from_rfc3339("2026-08-09T00:00:02Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let root_ms = started_at.timestamp_millis() as u64 + 1_000;
        let capture = SessionCapture {
            session_id: "agent-session".into(),
            journal_path: PathBuf::from("/fake"),
            events: vec![
                JournalEvent {
                    event_type: "ToolCallStarted".into(),
                    raw: serde_json::json!({
                        "event_id": "agent-start",
                        "run_id": "root-run",
                        "event_type": "ToolCallStarted",
                        "step_id": "root-step",
                        "caused_by": [],
                        "created_at": root_ms,
                        "payload": {"tool_name": "agent", "call_id": "agent-call"}
                    }),
                },
                JournalEvent {
                    event_type: "StepCreated".into(),
                    raw: serde_json::json!({
                        "event_id": "child-created",
                        "run_id": "child-run",
                        "event_type": "StepCreated",
                        "step_id": "child-step",
                        "caused_by": ["agent-start"],
                        "created_at": root_ms + 1,
                        "payload": {}
                    }),
                },
                JournalEvent {
                    event_type: "ToolCallCompleted".into(),
                    raw: serde_json::json!({
                        "event_id": "child-search",
                        "run_id": "child-run",
                        "event_type": "ToolCallCompleted",
                        "step_id": "child-step",
                        "caused_by": ["child-created"],
                        "created_at": root_ms + 2,
                        "payload": {
                            "tool_name": "tool_search",
                            "call_id": "search-call",
                            "cached": false,
                            "is_error": false
                        }
                    }),
                },
            ],
            skipped_lines: 0,
            dropped_lines: 0,
            integrity_errors: 0,
        };

        let scoped = capture.scoped_to_invocation(&["root-run".into()], started_at);
        assert!(scoped.events.iter().any(|event| {
            event.raw.get("run_id").and_then(serde_json::Value::as_str) == Some("child-run")
        }));
        assert_eq!(
            scoped.causal_child_tool_call_count("agent", "tool_search"),
            1
        );
    }

    #[test]
    fn scoped_invocation_rejects_foreign_fanout_parent_and_unrelated_child() {
        let started_at = chrono::DateTime::parse_from_rfc3339("2026-08-09T00:00:02Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let root_ms = started_at.timestamp_millis() as u64 + 1_000;
        let capture = SessionCapture {
            session_id: "fanout-session".into(),
            journal_path: PathBuf::from("/fake"),
            events: vec![
                JournalEvent {
                    event_type: "ToolCallCompleted".into(),
                    raw: serde_json::json!({
                        "event_id": "fanout-complete",
                        "run_id": "root-run",
                        "event_type": "ToolCallCompleted",
                        "step_id": "root-step",
                        "caused_by": [],
                        "created_at": root_ms,
                        "payload": {
                            "tool_name": "agent_fanout",
                            "call_id": "fanout-call",
                            "is_error": false,
                            "output": {
                                "fanout": {
                                    "parent_run_id": "other-root",
                                    "slots": [{"run_id": "foreign-child"}]
                                }
                            }
                        }
                    }),
                },
                JournalEvent {
                    event_type: "ToolCallCompleted".into(),
                    raw: serde_json::json!({
                        "event_id": "foreign-read",
                        "run_id": "foreign-child",
                        "event_type": "ToolCallCompleted",
                        "step_id": "foreign-step",
                        "caused_by": [],
                        "created_at": root_ms + 1,
                        "payload": {
                            "tool_name": "read_file",
                            "call_id": "foreign-read-call",
                            "cached": false,
                            "is_error": false
                        }
                    }),
                },
            ],
            skipped_lines: 0,
            dropped_lines: 0,
            integrity_errors: 0,
        };

        let scoped = capture.scoped_to_invocation(&["root-run".into()], started_at);
        assert_eq!(scoped.count_events("ToolCallCompleted"), 1);
        assert_eq!(scoped.tools_invoked(), vec!["agent_fanout"]);
    }

    #[test]
    fn load_session_skips_malformed_lines_but_keeps_going() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s2.jsonl");
        let body = [
            r#"{"type":"llm_round","ts":"2026-08-09T00:00:01Z","session_id":"s2"}"#,
            r#"this is not json"#,
            r#"{"type":"turn","ts":"2026-08-09T00:00:02Z","session_id":"s2","turn":1,"tool_calls":[{"name":"Read","ok":true,"ms":1}]}"#,
        ]
        .join("\n");
        std::fs::write(&path, body).unwrap();

        let cap = load_session_from_path("s2", &path).unwrap();
        assert_eq!(cap.events.len(), 2);
        assert_eq!(cap.skipped_lines, 1);
        assert_eq!(cap.tools_invoked(), vec!["Read".to_string()]);
    }

    #[test]
    fn load_session_rejects_json_without_a_typed_event_discriminator() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("invalid-envelope.jsonl");
        let body = [
            r#"{}"#,
            r#"42"#,
            r#"{"event_type":17}"#,
            r#"{"type":"turn","ts":"2026-08-09T00:00:01Z","session_id":"invalid-envelope","turn":1}"#,
        ]
        .join("\n");
        std::fs::write(&path, body).unwrap();

        let capture = load_session_from_path("invalid-envelope", &path).unwrap();
        assert_eq!(capture.events.len(), 1);
        assert_eq!(capture.count_events("turn"), 1);
        assert_eq!(capture.skipped_lines, 3);
    }

    #[test]
    fn load_session_deduplicates_and_rejects_conflicting_event_ids() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("duplicate-events.jsonl");
        let first = r#"{"event_id":"evt-1","run_id":"run-1","event_type":"StepStarted","step_id":"step-1","caused_by":[],"created_at":1}"#;
        let conflicting = r#"{"event_id":"evt-1","run_id":"run-1","event_type":"StepCompleted","step_id":"step-1","caused_by":[],"created_at":2}"#;
        std::fs::write(&path, format!("{first}\n{first}\n{conflicting}")).unwrap();

        let capture = load_session_from_path("duplicate-events", &path).unwrap();
        assert_eq!(capture.events.len(), 1);
        assert_eq!(capture.count_events("StepStarted"), 1);
        assert_eq!(capture.skipped_lines, 1);
    }

    #[test]
    fn legacy_event_duplicates_are_canonicalized_and_conflicts_incomplete() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("legacy-duplicates.jsonl");
        let first = r#"{"type":"turn","ts":"2026-08-09T00:00:01Z","session_id":"legacy-duplicates","turn":1,"assistant_output":"ok"}"#;
        let reordered = r#"{"assistant_output":"ok","turn":1,"session_id":"legacy-duplicates","ts":"2026-08-09T00:00:01Z","type":"turn"}"#;
        let conflicting = r#"{"type":"turn","ts":"2026-08-09T00:00:01Z","session_id":"legacy-duplicates","turn":1,"assistant_output":"different"}"#;
        std::fs::write(&path, format!("{first}\n{reordered}\n{conflicting}")).unwrap();

        let capture = load_session_from_path("legacy-duplicates", &path).unwrap();
        assert_eq!(capture.events.len(), 1);
        assert_eq!(capture.skipped_lines, 1);
        assert!(capture.has_integrity_errors());
    }

    #[test]
    fn owner_merge_deduplicates_identical_step_event_mirrors() {
        let dir = tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(dir.path());
        let owner_a = astra_services::OwnerScope::user("mirror-a").unwrap();
        let owner_b = astra_services::OwnerScope::user("mirror-b").unwrap();
        let session_id = "mirror-session";
        for owner in [&owner_a, &owner_b] {
            let journal =
                astra_services::session_journal::journal_file_path_for_owner(owner, session_id)
                    .unwrap();
            let steps = journal
                .parent()
                .unwrap()
                .join(session_id)
                .join("step_events.jsonl");
            std::fs::create_dir_all(steps.parent().unwrap()).unwrap();
            let event = format!(
                r#"{{"schema_version":2,"layout_version":"v1","artifact_kind":"step_event","user_id":"{}","session_id":"{}","payload":{{"event_id":"same-event","run_id":"run-1","event_type":"StepStarted","step_id":"step-1","caused_by":[],"created_at":1,"payload":{{}}}}}}"#,
                owner.id(),
                session_id
            );
            std::fs::write(steps, event).unwrap();
        }

        let capture = load_session_for_owners(session_id, &[owner_a, owner_b]).unwrap();
        assert_eq!(capture.count_events("StepStarted"), 1);
        assert_eq!(capture.skipped_lines, 0);
        let stats = load_step_event_stats_for_owners(
            session_id,
            &[
                astra_services::OwnerScope::user("mirror-a").unwrap(),
                astra_services::OwnerScope::user("mirror-b").unwrap(),
            ],
        )
        .unwrap();
        assert_eq!(stats.turn_rounds, 1);
    }

    #[test]
    fn owner_merge_canonicalizes_legacy_duplicates_and_rejects_conflicts() {
        let dir = tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(dir.path());
        let owner_a = astra_services::OwnerScope::user("legacy-mirror-a").unwrap();
        let owner_b = astra_services::OwnerScope::user("legacy-mirror-b").unwrap();
        let session_id = "legacy-owner-merge";
        let journal_a =
            astra_services::session_journal::journal_file_path_for_owner(&owner_a, session_id)
                .unwrap();
        let journal_b =
            astra_services::session_journal::journal_file_path_for_owner(&owner_b, session_id)
                .unwrap();
        std::fs::create_dir_all(journal_a.parent().unwrap()).unwrap();
        std::fs::create_dir_all(journal_b.parent().unwrap()).unwrap();
        let first = r#"{"type":"turn","ts":"2026-08-09T00:00:01Z","session_id":"legacy-owner-merge","turn":1,"assistant_output":"ok"}"#;
        let reordered = r#"{"assistant_output":"ok","turn":1,"session_id":"legacy-owner-merge","ts":"2026-08-09T00:00:01Z","type":"turn"}"#;
        std::fs::write(&journal_a, first).unwrap();
        std::fs::write(&journal_b, reordered).unwrap();

        let capture =
            load_session_for_owners(session_id, &[owner_a.clone(), owner_b.clone()]).unwrap();
        assert_eq!(capture.count_events("turn"), 1);
        assert!(!capture.has_integrity_errors());

        std::fs::write(
            &journal_b,
            r#"{"type":"turn","ts":"2026-08-09T00:00:01Z","session_id":"legacy-owner-merge","turn":1,"assistant_output":"tampered"}"#,
        )
        .unwrap();
        let conflicting = load_session_for_owners(session_id, &[owner_a, owner_b]).unwrap();
        assert!(conflicting.has_integrity_errors());
        assert_eq!(conflicting.skipped_lines, 1);
    }

    #[test]
    fn load_session_returns_none_when_missing() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("nope.jsonl");
        assert!(load_session_from_path("nope", &missing).is_none());
    }

    #[test]
    fn subsystem_settlement_must_cover_latest_canonical_turn() {
        let capture = SessionCapture {
            session_id: "resumed".into(),
            journal_path: PathBuf::from("/fixture"),
            events: vec![
                JournalEvent {
                    event_type: "turn".into(),
                    raw: serde_json::json!({"turn": 8}),
                },
                JournalEvent {
                    event_type: "subsystem_settled".into(),
                    raw: serde_json::json!({
                        "turn": 7,
                        "metadata": {"subsystem": "post_loop_memory"}
                    }),
                },
            ],
            skipped_lines: 0,
            dropped_lines: 0,
            integrity_errors: 0,
        };

        assert_eq!(capture.latest_canonical_turn(), Some(8));
        assert!(!capture.subsystem_settled_for_latest_turn("post_loop_memory"));
    }

    #[test]
    fn tools_invoked_dedups_and_supports_flat_shape() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s3.jsonl");
        // Cover both the metadata-nested shape and the flat shape.
        let body = [
            r#"{"type":"turn","ts":"2026-08-09T00:00:01Z","session_id":"s3","turn":1,"tool_calls":[{"name":"Read","ok":true,"ms":1}]}"#,
            r#"{"type":"turn","ts":"2026-08-09T00:00:02Z","session_id":"s3","turn":2,"tool_calls":[{"name":"Read","ok":true,"ms":1}]}"#,
            r#"{"type":"turn","ts":"2026-08-09T00:00:03Z","session_id":"s3","turn":3,"tool_calls":[{"name":"Grep","ok":true,"ms":1}]}"#,
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
            r#"{"type":"llm_request_full","ts":"2026-08-09T00:00:00Z","session_id":"legacy","turn":1}"#,
            r#"{"type":"llm_round","ts":"2026-08-09T00:00:01Z","session_id":"legacy","turn":1,"tool_calls":[{"name":"read_file","ok":true,"ms":12},{"name":"list_dir","ok":true,"ms":8}]}"#,
            r#"{"type":"llm_round","ts":"2026-08-09T00:00:02Z","session_id":"legacy","turn":2,"tool_calls":[{"name":"list_dir","ok":true,"ms":3}]}"#,
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
            r#"{"type":"llm_round","ts":"2026-08-09T00:00:01Z","session_id":"merge-test","turn":1,"tool_calls":[{"name":"read_file","ok":true,"ms":5}]}"#,
        )
        .unwrap();
        // Step-events: one ToolCallCompleted with a DIFFERENT tool.
        let step_dir = sessions.join(sid);
        std::fs::create_dir_all(&step_dir).unwrap();
        std::fs::write(
            step_dir.join("step_events.jsonl"),
            r#"{"event_id":"tool-merge","run_id":"run-1","event_type":"ToolCallCompleted","step_id":"step-merge","caused_by":["step-merge"],"created_at":1,"payload":{"tool_name":"list_dir","call_id":"call-merge","cached":false,"is_error":false}}"#,
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
            .map(|i| format!(r#"{{"type":"llm_round","ts":"2026-08-09T00:00:{:02}Z","session_id":"many","turn":{},"seq":{}}}"#, i % 60, i, i))
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
        let body = [
            r#"{"type":"llm_round","ts":"2026-08-09T00:00:01Z","session_id":"small","turn":1}"#,
            r#"{"type":"llm_round","ts":"2026-08-09T00:00:02Z","session_id":"small","turn":2}"#,
        ]
        .join("\n");
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
            r#"{"event_id":"step-1","run_id":"run-1","event_type":"StepStarted","step_id":"step-1","caused_by":[],"created_at":1}"#,
            r#"{"event_id":"tool-1","run_id":"run-1","event_type":"ToolCallCompleted","step_id":"step-1","caused_by":["step-1"],"created_at":2,"payload":{"tool_name":"Read","call_id":"call-1","cached":true,"is_error":false}}"#,
            r#"{"event_id":"tool-2","run_id":"run-1","event_type":"ToolCallCompleted","step_id":"step-1","caused_by":["tool-1"],"created_at":3,"payload":{"tool_name":"Write","call_id":"call-2","cached":false,"is_error":false}}"#,
        ]
        .join("\n");
        std::fs::write(&path, &body).unwrap();
        let stats = super::load_step_event_stats_from_path(&path, 1024 * 1024).unwrap();
        assert_eq!(stats.turn_rounds, 1);
        assert_eq!(stats.total_tool_calls, 2);
        assert_eq!(stats.cache_hits, 1);
    }

    #[test]
    fn step_event_stats_prefers_provider_rounds_over_legacy_step_markers() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("step_events.jsonl");
        let body = [
            r#"{"event_id":"step-1","run_id":"run-1","event_type":"StepStarted","step_id":"step-1","caused_by":[],"created_at":1}"#,
            r#"{"event_id":"round-1","run_id":"run-1","event_type":"LlmRoundStarted","step_id":"step-1","caused_by":["step-1"],"created_at":2}"#,
            r#"{"event_id":"round-2","run_id":"run-1","event_type":"LlmRoundStarted","step_id":"step-1","caused_by":["round-1"],"created_at":3}"#,
        ]
        .join("\n");
        std::fs::write(&path, body).unwrap();

        let stats = super::load_step_event_stats_from_path(&path, 1024 * 1024).unwrap();
        assert_eq!(stats.turn_rounds, 2);
    }

    #[test]
    fn step_event_stats_rejects_partial_or_identityless_evidence() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("partial-step-events.jsonl");
        let body = [
            r#"{"event_id":"step-1","event_type":"StepStarted","step_id":"step-1","caused_by":[],"created_at":1}"#,
            r#"{"event_id":"tool-1","event_type":"ToolCallCompleted","step_id":"step-1","caused_by":["step-1"],"created_at":2,"payload":{"tool_name":"Read","call_id":"call-1","cached":true,"is_error":false}}"#,
            r#"{"event_id":"tool-2","event_type":"ToolCallCompleted","payload":{"cached":false}"#,
        ]
        .join("\n");
        std::fs::write(&path, body).unwrap();
        assert!(load_step_event_stats_from_path(&path, 1024 * 1024).is_none());

        std::fs::write(&path, r#"{"event_id":"step-1","event_type":"StepStarted"}"#).unwrap();
        assert!(load_step_event_stats_from_path(&path, 1024 * 1024).is_none());

        std::fs::write(
            &path,
            [
                r#"{"event_id":"step-1","event_type":"StepStarted","step_id":"step-1","caused_by":[],"created_at":1}"#,
                r#"{"event_id":"tool-1","event_type":"ToolCallCompleted","step_id":"step-1","caused_by":["step-1"],"created_at":2,"payload":{"tool_name":"Read","cached":true,"is_error":false}}"#,
            ]
            .join("\n"),
        )
        .unwrap();
        assert!(load_step_event_stats_from_path(&path, 1024 * 1024).is_none());
    }

    #[test]
    fn current_step_event_envelope_is_unwrapped_for_capture_and_stats() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("step_events.jsonl");
        let body = [
            r#"{"event_id":"step-1","schema_version":2,"artifact_kind":"step_event","payload":{"event_id":"step-1","run_id":"run-1","event_type":"StepStarted","step_id":"step-1","caused_by":[],"created_at":1,"payload":{"trace_context":{"round_index":0,"run_id":"run-1"}}}}"#,
            r#"{"event_id":"tool-1","schema_version":2,"artifact_kind":"step_event","payload":{"event_id":"tool-1","run_id":"run-1","event_type":"ToolCallCompleted","step_id":"step-1","caused_by":["step-1"],"created_at":2,"payload":{"tool_name":"memory","call_id":"call-1","cached":true,"is_error":false}}}"#,
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
            r#"{"type":"session_start","ts":"2026-08-09T00:00:00Z","session_id":"explicit-owner-namespace-test"}"#,
        )
        .unwrap();

        let account_journal = astra_services::session_journal::journal_file_path_for_owner(
            &account_owner,
            session_id,
        )
        .unwrap();
        std::fs::create_dir_all(account_journal.parent().unwrap()).unwrap();
        std::fs::write(
            &account_journal,
            [
                r#"{"type":"llm_round","ts":"2026-08-09T00:00:01Z","session_id":"explicit-owner-namespace-test","turn":0,"round":0}"#,
                r#"{"type":"llm_round","ts":"2026-08-09T00:00:02Z","session_id":"explicit-owner-namespace-test","turn":1,"round":1}"#,
            ]
            .join("\n"),
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
            r#"{"event_id":"account-step-1","schema_version":2,"artifact_kind":"step_event","payload":{"event_id":"account-step-1","run_id":"run-1","event_type":"StepStarted","step_id":"step-1","caused_by":[],"created_at":1}}"#,
        )
        .unwrap();

        let profile_only =
            super::load_session_for_owners(session_id, std::slice::from_ref(&profile_owner))
                .unwrap();
        assert_eq!(profile_only.count_events("StepStarted"), 0);

        let owners = [profile_owner, account_owner];
        let capture = super::load_session_for_owners(session_id, &owners).unwrap();
        assert_eq!(capture.count_events("session_start"), 1);
        assert_eq!(capture.count_events("llm_round"), 2);
        assert_eq!(capture.count_events("StepStarted"), 1);
        assert_eq!(capture.journal_path, account_journal);
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
            integrity_errors: 0,
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
                            "ok": true,
                            "args_full": r#"{"action":"start","target_count":3}"#,
                            "result_full": r#"{"fanout":{"terminal":3},"provenance":{"all_slots_delivered":true}}"#
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
    fn fanout_child_identity_extraction_accepts_canonical_results_projection() {
        let raw = serde_json::json!({
            "event_type": "ToolCallCompleted",
            "payload": {
                "tool_name": "agent_fanout",
                "is_error": false,
                "output": serde_json::json!({
                    "fanout": {"parent_run_id": "parent-run"},
                    "results": [
                        {"slot_index": 0, "run_id": "child-a"},
                        {"slot_index": 1, "run_id": "child-b"}
                    ]
                }).to_string()
            }
        });
        assert_eq!(
            super::fanout_result_child_run_ids(&raw, "parent-run"),
            vec!["child-a".to_string(), "child-b".to_string()]
        );
    }

    #[test]
    fn fanout_child_identity_extraction_prefers_one_canonical_launch_projection() {
        let raw = serde_json::json!({
            "event_type": "ToolCallCompleted",
            "payload": {
                "tool_name": "agent_fanout",
                "is_error": false,
                "output": serde_json::json!({
                    "fanout": {
                        "parent_run_id": "parent-run",
                        "slots": [{"slot_index": 0, "run_id": "legacy-child"}]
                    },
                    "agents": [
                        {"slot_index": 0, "run_id": "child-a"},
                        {"slot_index": 1, "run_id": "child-b"}
                    ],
                    "results": [{"slot_index": 0, "run_id": "child-a"}]
                })
                .to_string()
            }
        });

        assert_eq!(
            super::fanout_result_child_run_ids(&raw, "parent-run"),
            vec!["child-a".to_string(), "child-b".to_string()]
        );
    }

    #[test]
    fn journal_tool_calls_reject_conflicting_reused_identity() {
        let capture = SessionCapture {
            session_id: "conflict".into(),
            journal_path: PathBuf::from("/tmp/conflict.jsonl"),
            events: vec![
                JournalEvent {
                    event_type: "turn".into(),
                    raw: serde_json::json!({
                        "tool_calls": [{
                            "tool_call_id": "call-1",
                            "name": "start_work",
                            "ok": true,
                            "args": r#"{"task":"build"}"#,
                            "result": r#"{"status":"started"}"#
                        }]
                    }),
                },
                JournalEvent {
                    event_type: "turn".into(),
                    raw: serde_json::json!({
                        "tool_calls": [{
                            "tool_call_id": "call-1",
                            "name": "start_work",
                            "ok": false,
                            "args": r#"{"task":"different"}"#,
                            "result": r#"{"status":"failed"}"#
                        }]
                    }),
                },
            ],
            skipped_lines: 0,
            dropped_lines: 0,
            integrity_errors: 0,
        };

        assert!(
            capture.journal_tool_calls().is_empty(),
            "conflicting call identity must invalidate the entire nested projection"
        );
        assert!(
            capture.tools_invoked().is_empty(),
            "the simpler tool-name projection must fail closed on the same conflict"
        );
    }

    #[test]
    fn journal_tool_calls_scope_provider_call_ids_to_their_run() {
        let capture = SessionCapture {
            session_id: "continuation".into(),
            journal_path: PathBuf::from("/tmp/continuation.jsonl"),
            events: vec![
                JournalEvent {
                    event_type: "turn".into(),
                    raw: serde_json::json!({
                        "producer_scope": {"run_id": "run-1"},
                        "tool_calls": [{
                            "tool_call_id": "call-00",
                            "name": "start_work",
                            "ok": true,
                            "args": r#"{"activation":"start"}"#,
                            "result": r#"{"status":"started"}"#
                        }]
                    }),
                },
                JournalEvent {
                    event_type: "turn".into(),
                    raw: serde_json::json!({
                        "producer_scope": {"run_id": "run-2"},
                        "tool_calls": [{
                            "tool_call_id": "call-00",
                            "name": "inspect_work_plan",
                            "ok": true,
                            "args": "{}",
                            "result": r#"{"status":"active"}"#
                        }]
                    }),
                },
            ],
            skipped_lines: 0,
            dropped_lines: 0,
            integrity_errors: 0,
        };

        let calls = capture.journal_tool_calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "start_work");
        assert_eq!(calls[1].name, "inspect_work_plan");
        assert!(!capture.has_integrity_errors());
    }

    #[test]
    fn created_memory_ids_selects_only_session_owned_store_responses() {
        let capture = SessionCapture {
            session_id: "case-session".into(),
            journal_path: PathBuf::from("/tmp/case.jsonl"),
            skipped_lines: 0,
            dropped_lines: 0,
            integrity_errors: 0,
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
    let mut records = Vec::new();
    let mut found = false;
    for path in session_artifact_paths_for_owners(session_id, owner_scopes)
        .into_iter()
        .filter(|path| path.file_name().and_then(|name| name.to_str()) == Some("step_events.jsonl"))
    {
        if !path.is_file() {
            continue;
        }
        found = true;
        records.extend(load_step_event_records_from_path(
            &path,
            DEFAULT_MAX_STEP_EVENTS_BYTES,
        )?);
    }
    found.then_some(aggregate_step_event_records(records)?)
}

/// Cap-configurable version — test seam.
pub fn load_step_event_stats_from_path(path: &Path, max_bytes: u64) -> Option<StepEventStats> {
    aggregate_step_event_records(load_step_event_records_from_path(path, max_bytes)?)
}

#[derive(Debug, Clone)]
struct StepEventRecord {
    event_id: String,
    event_type: String,
    cached: bool,
    fingerprint: String,
}

fn load_step_event_records_from_path(path: &Path, max_bytes: u64) -> Option<Vec<StepEventRecord>> {
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
    let mut records = Vec::new();
    let mut seen = std::collections::HashMap::<String, String>::new();
    for line in content.lines() {
        let ev = serde_json::from_str::<serde_json::Value>(line).ok()?;
        let logical = logical_with_event_id(&ev);
        let event_type = event_type(&logical)?.to_string();
        let event_id = event_id(&ev)
            .or_else(|| event_id(&logical))
            .map(str::to_string)?;
        if !valid_step_event_shape(&logical) {
            return None;
        }
        // Owner-scoped VersionedStepArtifact envelopes intentionally carry
        // storage identity (user/session/layout) outside the logical event.
        // Fingerprint only the normalized logical StepEvent so two authorized
        // owner mirrors compare equal without weakening event-id conflicts.
        let fingerprint = serde_json::to_string(&logical).ok()?;
        if let Some(existing) = seen.get(&event_id) {
            if existing != &fingerprint {
                // Reusing one event id for different payloads is an
                // integrity conflict, not two independent observations.
                return None;
            }
            continue;
        }
        seen.insert(event_id.clone(), fingerprint.clone());
        let cached = logical
            .get("payload")
            .and_then(|payload| payload.get("cached"))
            .and_then(|cached| cached.as_bool())
            == Some(true);
        records.push(StepEventRecord {
            event_id,
            event_type,
            cached,
            fingerprint,
        });
    }
    Some(records)
}

fn aggregate_step_event_records(records: Vec<StepEventRecord>) -> Option<StepEventStats> {
    let mut stats = StepEventStats::default();
    let mut legacy_step_rounds = 0_u32;
    let mut provider_rounds = 0_u32;
    let mut seen = std::collections::HashMap::<String, String>::new();
    for record in records {
        if let Some(existing) = seen.get(&record.event_id) {
            if existing != &record.fingerprint {
                return None;
            }
            continue;
        }
        seen.insert(record.event_id, record.fingerprint);
        match record.event_type.as_str() {
            // Current producers emit one LlmRoundStarted per provider
            // request, while older artifacts only exposed StepStarted. Use
            // the precise provider-round signal whenever it exists and keep
            // the legacy count as a fallback; counting both would inflate
            // turn-round bounds on mixed-version journals.
            "LlmRoundStarted" => provider_rounds = provider_rounds.saturating_add(1),
            "StepStarted" => legacy_step_rounds = legacy_step_rounds.saturating_add(1),
            "ToolCallCompleted" => {
                stats.total_tool_calls = stats.total_tool_calls.saturating_add(1);
                if record.cached {
                    stats.cache_hits = stats.cache_hits.saturating_add(1);
                }
            }
            _ => {}
        }
    }
    stats.turn_rounds = if provider_rounds > 0 {
        provider_rounds
    } else {
        legacy_step_rounds
    };
    Some(stats)
}
