//! Step Recorder: observation layer that records chat_stream execution as Step events.
//!
//! This module wraps the existing chat_stream.rs execution loop with Step recording,
//! mapping implicit phases to explicit StepAction transitions. It is purely additive —
//! existing control flow is unchanged.
//!
//! # Usage in chat_stream.rs
//!
//! ```ignore
//! let mut recorder = StepRecorder::new("user-123", "session-123", "task-1");
//!
//! // Before main loop:
//! recorder.begin_turn(turn_number);
//!
//! // After tool surface (PLAN phase):
//! recorder.record_plan(visible_tools, budget_pressure, budget_tokens);
//!
//! // Before each tool execution:
//! recorder.begin_tool(tool_name, &args);
//!
//! // After each tool result:
//! recorder.complete_tool(tool_name, is_error, elapsed_ms);
//!
//! // After turn_guard.evaluate():
//! recorder.record_verdict(severity, stall, divergence, strong_advisory, injections);
//!
//! // After main loop:
//! let summary = recorder.finalize();
//! ```

use crate::event::clip_output_preview;
use crate::step_checkpoint::FileBackedEventStore;
use crate::step_protocol::*;
use astra_turn_types::InferencePurpose;
use regex::Regex;
use std::collections::HashMap;
use std::sync::OnceLock;

/// Redact common credential patterns from tool output before persisting to disk.
/// Returns (redacted_text, redaction_count).
fn redact_credentials_for_storage(text: &str) -> (String, usize) {
    let mut count = 0usize;
    let lines: Vec<String> = text
        .lines()
        .map(|line| {
            if let Some(redacted) = redact_sensitive_assignment_line(line) {
                count += 1;
                redacted
            } else {
                line.to_string()
            }
        })
        .collect();
    let mut result = lines.join("\n");
    count += redact_pem_blocks(&mut result);
    count += redact_shell_credentials(&mut result);

    // Inline redaction for common standalone token shapes.
    let inline_patterns = [
        ("eyJ", 30, "JWT_TOKEN"),
        ("sk-", 20, "API_KEY"),
        ("sk-proj-", 20, "API_KEY"),
        ("AKIA", 20, "AWS_ACCESS_KEY"),
        ("ASIA", 20, "AWS_SESSION_KEY"),
        ("ghp_", 36, "GITHUB_PAT"),
        ("gho_", 36, "GITHUB_OAUTH"),
        ("ghu_", 36, "GITHUB_USER"),
        ("ghs_", 36, "GITHUB_SERVER"),
        ("ghr_", 36, "GITHUB_REFRESH"),
        ("xoxb-", 20, "SLACK_TOKEN"),
        ("xoxp-", 20, "SLACK_TOKEN"),
        ("xoxa-", 20, "SLACK_TOKEN"),
        ("xoxs-", 20, "SLACK_TOKEN"),
    ];
    for (prefix, min_len, label) in inline_patterns {
        while let Some(start) = result.find(prefix) {
            // Find end: next whitespace or end of string
            let end = result[start..]
                .find(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == ',')
                .map(|i| start + i)
                .unwrap_or(result.len());
            if end - start >= min_len {
                result.replace_range(start..end, &format!("[REDACTED_{}]", label));
                count += 1;
            } else {
                break;
            }
        }
    }

    (result, count)
}

/// Redact credentials embedded in shell syntax before persisting a tool
/// preview or result. Assignment-only redaction is insufficient here: command
/// lines commonly pass a password as the argument to a credential-bearing
/// flag, which otherwise ends up in a durable step event.
///
/// This deliberately recognizes only flags with unambiguous secret semantics
/// (`sshpass -p` and `--password`). Broad flags such as `-p` remain untouched
/// because they are frequently ordinary process IDs, paths, or ports.
fn redact_shell_credentials(text: &mut String) -> usize {
    static SECRET_ASSIGNMENT: OnceLock<Regex> = OnceLock::new();
    static SSHPASS_PASSWORD: OnceLock<Regex> = OnceLock::new();
    static LONG_PASSWORD_FLAG: OnceLock<Regex> = OnceLock::new();

    let assignment = SECRET_ASSIGNMENT.get_or_init(|| {
        Regex::new(
            r#"(?ix)\b([a-z_][a-z0-9_]*(?:password|passwd|token|secret|api_key)\s*=\s*)(?:\"[^\"]*\"|'[^']*'|[^\s;|&]+)"#,
        )
        .expect("shell credential assignment regex must compile")
    });
    let sshpass = SSHPASS_PASSWORD.get_or_init(|| {
        Regex::new(r#"(?ix)\b(sshpass\s+-[a-z]*p[a-z]*\s+)(?:\"[^\"]*\"|'[^']*'|\S+)"#)
            .expect("sshpass password regex must compile")
    });
    let long_password = LONG_PASSWORD_FLAG.get_or_init(|| {
        Regex::new(r#"(?ix)(--(?:password|passwd|token|secret|api-key)(?:=|\s+))(?:\"[^\"]*\"|'[^']*'|\S+)"#)
            .expect("long password flag regex must compile")
    });

    let mut count = 0;
    for matcher in [assignment, sshpass, long_password] {
        let redacted = matcher.replace_all(text, |captures: &regex::Captures<'_>| {
            let whole = captures
                .get(0)
                .expect("credential regex always captures the full match")
                .as_str();
            let prefix = captures
                .get(1)
                .expect("credential regex always captures its prefix")
                .as_str();
            if &whole[prefix.len()..] == "[REDACTED]" {
                return whole.to_string();
            }
            count += 1;
            format!("{prefix}[REDACTED]")
        });
        if redacted.as_ref() != text {
            *text = redacted.into_owned();
        }
    }
    count
}

fn sanitize_args_preview_for_storage(args_preview: Option<&str>) -> (Option<String>, usize) {
    let Some(args_preview) = args_preview else {
        return (None, 0);
    };
    let clipped = clip_output_preview(args_preview);
    let (redacted, count) = redact_credentials_for_storage(&clipped);
    (Some(redacted), count)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SensitiveKeyKind {
    SecretValue,
    AuthValue,
}

fn redact_sensitive_assignment_line(line: &str) -> Option<String> {
    let separator = find_sensitive_separator(line)?;
    let key = line[..separator].trim();
    // This path owns configuration-style lines such as `DB_PASSWORD=value`.
    // A shell command can contain the same assignment after another command
    // (`env DB_PASSWORD=value mysql ...`); redacting the entire remainder of
    // that line would discard useful diagnostics and hide later credentials
    // from the token-aware sanitizer below.
    if line.as_bytes().get(separator) == Some(&b'=')
        && !key
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
    {
        return None;
    }
    let suffix = &line[separator + 1..];
    let leading_ws_len = suffix.len() - suffix.trim_start().len();
    let value = suffix.trim();
    let kind = classify_sensitive_key(key)?;
    if value.is_empty() {
        return None;
    }
    let normalized_value = value.trim_matches(|c| matches!(c, '"' | '\'' | '`'));
    let should_redact = match kind {
        SensitiveKeyKind::SecretValue => true,
        SensitiveKeyKind::AuthValue => looks_like_auth_or_secret_value(normalized_value),
    };
    should_redact.then(|| {
        format!(
            "{}{}[REDACTED]",
            &line[..=separator],
            &suffix[..leading_ws_len]
        )
    })
}

fn find_sensitive_separator(line: &str) -> Option<usize> {
    let equals = line.find('=');
    let colon = (!line.contains("://")).then(|| line.find(':')).flatten();
    match (equals, colon) {
        (Some(eq), Some(colon)) => Some(eq.min(colon)),
        (Some(eq), None) => Some(eq),
        (None, Some(colon)) => Some(colon),
        (None, None) => None,
    }
}

fn classify_sensitive_key(key: &str) -> Option<SensitiveKeyKind> {
    let normalized = key
        .trim()
        .to_ascii_lowercase()
        .replace([' ', '-'], "_")
        .trim_matches('_')
        .to_string();
    if normalized.is_empty() {
        return None;
    }

    let strong_secret = normalized == "apikey"
        || normalized.ends_with("api_key")
        || normalized == "secret"
        || normalized.ends_with("_secret")
        || normalized.ends_with("secret_key")
        || normalized.ends_with("access_key")
        || normalized.ends_with("private_key")
        || normalized.ends_with("password")
        || normalized.ends_with("passwd")
        || normalized.ends_with("credential")
        || normalized.ends_with("credentials");
    if strong_secret {
        return Some(SensitiveKeyKind::SecretValue);
    }

    let auth_like = normalized == "authorization"
        || normalized.ends_with("_authorization")
        || normalized.ends_with("auth_token")
        || normalized.ends_with("_token")
        || normalized == "token"
        || normalized.ends_with("_bearer")
        || normalized == "bearer";
    auth_like.then_some(SensitiveKeyKind::AuthValue)
}

fn looks_like_auth_or_secret_value(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return false;
    }
    let lowered = trimmed.to_ascii_lowercase();
    if lowered.starts_with("bearer ") || lowered.starts_with("basic ") {
        return true;
    }
    let compact = trimmed.trim_matches(|c| matches!(c, ',' | ';'));
    let known_prefix = [
        "eyJ", "sk-", "sk-proj-", "AKIA", "ASIA", "ghp_", "gho_", "ghu_", "ghs_", "ghr_", "xoxb-",
        "xoxp-", "xoxa-", "xoxs-",
    ]
    .iter()
    .any(|prefix| compact.starts_with(prefix));
    if known_prefix {
        return true;
    }
    compact.len() >= 20
        && !compact.chars().any(char::is_whitespace)
        && compact
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '+' | '='))
}

fn redact_pem_blocks(text: &mut String) -> usize {
    let mut count = 0usize;
    let mut search_start = 0usize;
    const REDACTION: &str = "[REDACTED_PEM_BLOCK]";

    while let Some(begin_rel) = text[search_start..].find("-----BEGIN ") {
        let begin = search_start + begin_rel;
        let Some(end_marker_rel) = text[begin..].find("-----END ") else {
            break;
        };
        let end_marker = begin + end_marker_rel;
        let Some(end_suffix_rel) = text[end_marker + "-----END ".len()..].find("-----") else {
            break;
        };
        let end = end_marker + "-----END ".len() + end_suffix_rel + "-----".len();
        text.replace_range(begin..end, REDACTION);
        count += 1;
        search_start = begin + REDACTION.len();
    }

    count
}

/// Records chat_stream execution as Step lifecycle events.
/// Wraps the implicit state machine with explicit StepAction tracking.
pub struct StepRecorder {
    user_id: String,
    session_id: String,
    task_id: String,
    events: Vec<StepEvent>,
    current_step: Option<Step>,
    turn_number: u32,
    round_index: u32,
    step_sequence: u32,
    current_step_sequence: Option<u32>,
    slot_counter: u32,
    /// Per-tool timing for lightweight profiling
    tool_timings: HashMap<String, Vec<u64>>,
    /// Phase transitions recorded for debugging
    phase_log: Vec<(u32, StepAction, u64)>,
    /// Light checkpoint after each tool, heavy after each turn
    checkpoint_count: u32,
    /// Optional file-backed persistence (JSONL) for events
    file_store: Option<FileBackedEventStore>,
    /// A host explicitly requested persistence once an authoritative session
    /// id arrives from the first streamed response.
    attach_persistence_on_session_adoption: bool,
    persistence_required: bool,
    persisted_tail_event_id: Option<String>,
    persistence_error: Option<String>,
}

impl StepRecorder {
    pub fn new(user_id: &str, session_id: &str, task_id: &str) -> Self {
        Self {
            user_id: user_id.to_string(),
            session_id: session_id.to_string(),
            task_id: task_id.to_string(),
            events: Vec::new(),
            current_step: None,
            turn_number: 0,
            round_index: 0,
            step_sequence: 0,
            current_step_sequence: None,
            slot_counter: 0,
            tool_timings: HashMap::new(),
            phase_log: Vec::new(),
            checkpoint_count: 0,
            file_store: None,
            attach_persistence_on_session_adoption: false,
            persistence_required: false,
            persisted_tail_event_id: None,
            persistence_error: None,
        }
    }

    /// Create with file-backed persistence (events written to JSONL on disk).
    ///
    /// Scans existing checkpoints so `checkpoint_count` starts after the
    /// highest existing file number, preventing cross-turn overwrites.
    pub fn with_persistence(user_id: &str, session_id: &str, task_id: &str) -> Self {
        let file_store = FileBackedEventStore::empty(user_id, session_id);
        let persisted_summary = persisted_event_summary(user_id, session_id);
        let existing_max = crate::step_checkpoint::list_checkpoints(user_id, session_id)
            .unwrap_or_default()
            .iter()
            .map(|(n, _)| *n)
            .max()
            .unwrap_or(0);
        Self {
            file_store: Some(file_store),
            attach_persistence_on_session_adoption: false,
            persistence_required: true,
            events: Vec::new(),
            step_sequence: persisted_summary.next_step_sequence,
            checkpoint_count: existing_max.saturating_add(1),
            persisted_tail_event_id: persisted_summary.tail_event_id,
            ..Self::new(user_id, session_id, task_id)
        }
    }

    /// Create an in-memory recorder whose events become durable once the
    /// authoritative session id is learned from the runtime.
    ///
    /// This mode is for a CLI-created first turn: using a fake durable
    /// `ephemeral` session would leave forensic history under the wrong owner,
    /// while tying this decision to unrelated context-manifest persistence can
    /// silently lose the first turn altogether.
    pub fn with_deferred_persistence(
        user_id: &str,
        provisional_session_id: &str,
        task_id: &str,
    ) -> Self {
        let mut recorder = Self::new(user_id, provisional_session_id, task_id);
        recorder.attach_persistence_on_session_adoption = true;
        recorder
    }

    /// Attach persistence only when this recorder was explicitly constructed
    /// in deferred-persistence mode.
    pub fn attach_persistence_if_configured(&mut self, session_id: &str) {
        if self.attach_persistence_on_session_adoption {
            self.attach_persistence(session_id);
        }
    }

    /// Attach file-backed persistence after the authoritative session id becomes known.
    ///
    /// Existing in-memory events are rebound to the adopted session id before being
    /// flushed to disk so first-turn forensic artifacts land under the real session.
    pub fn attach_persistence(&mut self, session_id: &str) {
        if self.file_store.is_some() && self.session_id == session_id {
            self.attach_persistence_on_session_adoption = false;
            return;
        }

        self.rebind_session_id(session_id);

        let existing_max = crate::step_checkpoint::list_checkpoints(&self.user_id, session_id)
            .unwrap_or_default()
            .iter()
            .map(|(n, _)| *n)
            .max()
            .unwrap_or(0);
        self.checkpoint_count = self.checkpoint_count.max(existing_max.saturating_add(1));

        let persisted_summary = persisted_event_summary(&self.user_id, session_id);
        self.step_sequence = self.step_sequence.max(persisted_summary.next_step_sequence);
        self.persisted_tail_event_id = if self.events.is_empty() {
            persisted_summary.tail_event_id
        } else {
            None
        };
        let mut file_store = FileBackedEventStore::empty(&self.user_id, session_id);
        self.attach_persistence_on_session_adoption = false;
        self.persistence_required = true;
        for event in &self.events {
            if let Err(error) = file_store.append(event.clone()) {
                self.record_persistence_error(format!(
                    "failed to attach step-event persistence for {}: {}",
                    event.event_id, error
                ));
                self.file_store = None;
                return;
            }
        }
        self.file_store = Some(file_store);
    }

    /// Begin a new turn. Creates a PERCEIVE step.
    pub fn begin_turn(&mut self, turn: u32) {
        self.begin_turn_with_context(turn, turn);
    }

    /// Begin a new agentic round for a visible user turn.
    pub fn begin_turn_with_context(&mut self, visible_turn: u32, round_index: u32) {
        self.turn_number = visible_turn;
        self.round_index = round_index;
        self.slot_counter = 0;
        let step_sequence = self.step_sequence;
        self.step_sequence = self.step_sequence.saturating_add(1);
        self.current_step_sequence = Some(step_sequence);

        let step = Step::new(
            format!(
                "{}-turn-{}-step-{}",
                self.session_id, visible_turn, step_sequence
            ),
            self.task_id.clone(),
            format!("turn-{}", visible_turn),
            StepAction::Perceive,
            StepPayload::Perceive {
                user_query: String::new(), // filled later
                memory_context: vec![],
            },
        );

        self.emit(step.step_id(), StepEventType::StepCreated);
        self.phase_log
            .push((visible_turn, StepAction::Perceive, epoch_ms()));
        self.current_step = Some(step);
    }

    /// Record that memory context was loaded (PERCEIVE phase completion).
    pub fn record_perceive(&mut self, query: &str, memory_ids: &[String], domain_hints: &[String]) {
        if let Some(ref mut step) = self.current_step {
            step.execution.memory_context = Some(MemoryContext {
                retrieved_memory_ids: memory_ids.to_vec(),
                domain_hints: domain_hints.to_vec(),
                provenance: memory_ids.to_vec(),
                governance_actions: memory_ids
                    .iter()
                    .map(|id| MemoryGovernanceAction::Retrieved {
                        memory_id: id.clone(),
                    })
                    .collect(),
                cluster_insights: vec![],
                snapshot_id: None,
            });
            step.execution.payload = StepPayload::Perceive {
                user_query: query.to_string(),
                memory_context: memory_ids.to_vec(),
            };
        }
    }

    /// Record tool surface completion (PLAN phase).
    pub fn record_plan(
        &mut self,
        visible_tools: &[String],
        budget_pressure: f64,
        budget_tokens: u64,
    ) {
        self.transition_phase(StepAction::Plan);
        self.emit_with_payload(
            StepEventType::StepStarted,
            serde_json::json!({
                "visible_tools": visible_tools,
                "budget_pressure": budget_pressure,
            }),
        );

        if let Some(ref mut step) = self.current_step {
            step.execution.payload = StepPayload::Plan {
                available_tool_count: visible_tools.len(),
                budget_tokens,
                restricted_tools: vec![],
            };
            step.execution.result = Some(StepResult::Plan {
                visible_tools: visible_tools.to_vec(),
            });
        }
    }

    /// Transition to ACT phase (before LLM call).
    /// Record the start of an LLM API call. Pairs with [`Self::end_llm_round`].
    pub fn begin_llm_round(&mut self, model: &str, purpose: InferencePurpose) {
        self.emit_with_payload(
            StepEventType::LlmRoundStarted,
            serde_json::json!({
                "model": model,
                "purpose": purpose,
            }),
        );
    }

    /// Record the completion of an LLM API call with usage metrics.
    pub fn end_llm_round(
        &mut self,
        model: &str,
        purpose: InferencePurpose,
        input_tokens: u64,
        output_tokens: u64,
        cache_read: u64,
        cache_creation: u64,
        latency_ms: u64,
    ) {
        self.emit_with_payload(
            StepEventType::LlmRoundCompleted,
            serde_json::json!({
                "model": model,
                "purpose": purpose,
                "input_tokens": input_tokens,
                "output_tokens": output_tokens,
                "cache_read_tokens": cache_read,
                "cache_creation_tokens": cache_creation,
                "latency_ms": latency_ms,
            }),
        );
    }

    pub fn begin_act(&mut self, tool_count: usize) {
        self.transition_phase(StepAction::Act);
        if let Some(ref mut step) = self.current_step {
            step.execution.cursor = ExecutionCursor::for_act(tool_count);
            // Initialize Act result so record_tokens() can populate it
            step.execution.result = Some(StepResult::Act {
                tool_results_count: 0,
                assistant_text: None,
                tokens_in: 0,
                tokens_out: 0,
            });
        }
    }

    pub fn begin_act_with_slots(&mut self, slots: Vec<ExecutionSlotSpec>) {
        self.transition_phase(StepAction::Act);
        if let Some(ref mut step) = self.current_step {
            step.execution.cursor = ExecutionCursor::for_act_slots(slots);
            // Initialize Act result so record_tokens() can populate it.
            step.execution.result = Some(StepResult::Act {
                tool_results_count: 0,
                assistant_text: None,
                tokens_in: 0,
                tokens_out: 0,
            });
        }
    }

    /// Record start of a tool execution (within ACT phase).
    /// Optionally accepts an idempotency key for cache correlation.
    pub fn begin_tool(&mut self, tool_name: &str, call_id: &str) {
        self.begin_tool_with_key(tool_name, call_id, None);
    }

    /// Record start of a tool execution with idempotency key for cache tracking.
    pub fn begin_tool_with_key(
        &mut self,
        tool_name: &str,
        call_id: &str,
        idempotency_key: Option<&str>,
    ) {
        self.begin_tool_with_key_and_args_preview(tool_name, call_id, idempotency_key, None);
    }

    /// Record start of a tool execution with idempotency key and argument preview.
    pub fn begin_tool_with_key_and_args_preview(
        &mut self,
        tool_name: &str,
        call_id: &str,
        idempotency_key: Option<&str>,
        args_preview: Option<&str>,
    ) {
        let (sanitized_args_preview, redactions) = sanitize_args_preview_for_storage(args_preview);
        let slot_idx = self.slot_counter;
        self.slot_counter += 1;

        if let Some(ref mut step) = self.current_step
            && let Some(slot) = step.execution.cursor.slots.get_mut(slot_idx as usize)
        {
            slot.tool_name = tool_name.to_string();
            slot.call_id = call_id.to_string();
            slot.state = SlotState::Running;
            slot.idempotency_key = idempotency_key.map(|k| k.to_string());
            slot.args_preview = sanitized_args_preview.clone();
        }

        let mut payload = serde_json::json!({
            "tool_name": tool_name,
            "slot_index": slot_idx,
            "call_id": call_id,
            "idempotency_key": idempotency_key,
        });
        if let Some(args_preview) = sanitized_args_preview {
            payload["args_preview"] = serde_json::json!(args_preview);
        }
        if redactions > 0 {
            payload["args_preview_redactions"] = serde_json::json!(redactions);
        }
        self.emit_with_payload(StepEventType::ToolCallStarted, payload);
    }

    /// Record a cache hit on the current slot.
    /// Call this instead of complete_tool() when the idempotency cache provides the result.
    pub fn record_cache_hit(&mut self, tool_name: &str, cached: CachedToolResult) {
        self.record_cache_hit_with_reason(tool_name, cached, "idempotency_cache_hit");
    }

    /// Record a cache hit with an explicit trace reason.
    ///
    /// Use a scoped reason (for example `cached_cross_turn`) when the cache
    /// source matters for loop diagnostics and trace replay.
    pub fn record_cache_hit_with_reason(
        &mut self,
        tool_name: &str,
        cached: CachedToolResult,
        reason: &str,
    ) {
        let fallback_call_id = self
            .active_call_id_for_tool(tool_name)
            .filter(|call_id| !call_id.is_empty());
        self.record_cache_hit_with_reason_and_metadata(
            tool_name,
            fallback_call_id.as_deref(),
            None,
            cached,
            reason,
        );
    }

    /// Record a cache hit with explicit correlation metadata.
    pub fn record_cache_hit_with_reason_and_metadata(
        &mut self,
        tool_name: &str,
        call_id: Option<&str>,
        args_preview: Option<&str>,
        cached: CachedToolResult,
        reason: &str,
    ) {
        let is_error = cached.is_error;
        let output = cached.output.clone();
        let cached_for_slot = cached.clone();
        let mut extra = serde_json::Map::new();
        extra.insert("reason".to_string(), serde_json::json!(reason));
        self.complete_tool_inner(
            tool_name,
            is_error,
            0,
            true,
            Some(&output),
            call_id,
            args_preview,
            Some(cached_for_slot),
            Some(extra),
        );
    }

    /// Record a short-circuit skip for the current tool slot.
    ///
    /// Use this for duplicate blocks, permission/restriction blocks, semantic dedup,
    /// and other paths where the model requested a tool but runtime intentionally
    /// did not execute it.
    pub fn skip_tool_with_reason(
        &mut self,
        tool_name: &str,
        reason: &str,
        was_cached: bool,
        output: Option<&str>,
    ) {
        let fallback_call_id = self
            .active_call_id_for_tool(tool_name)
            .filter(|call_id| !call_id.is_empty());
        self.skip_tool_with_reason_and_metadata(
            tool_name,
            fallback_call_id.as_deref(),
            None,
            reason,
            was_cached,
            output,
        );
    }

    /// Record a short-circuit skip with explicit correlation metadata.
    pub fn skip_tool_with_reason_and_metadata(
        &mut self,
        tool_name: &str,
        call_id: Option<&str>,
        args_preview: Option<&str>,
        reason: &str,
        was_cached: bool,
        output: Option<&str>,
    ) {
        let (sanitized_args_preview, preview_redactions) =
            sanitize_args_preview_for_storage(args_preview);
        let slot_idx = self.resolve_terminal_slot_index(call_id);
        self.ensure_terminal_slot_started(
            slot_idx,
            tool_name,
            call_id,
            sanitized_args_preview.clone(),
            preview_redactions,
        );
        let slot_meta = self.slot_trace_meta(slot_idx);

        if let Some(ref mut step) = self.current_step {
            if let Some(slot) = step.execution.cursor.slots.get_mut(slot_idx) {
                slot.state = SlotState::Skipped;
            }
            if let Some(StepResult::Act {
                ref mut tool_results_count,
                ..
            }) = step.execution.result
            {
                *tool_results_count += 1;
            }
        }

        let mut payload = serde_json::json!({
            "tool_name": tool_name,
            "slot_index": slot_idx,
            "reason": reason,
            "cached": was_cached,
        });
        if let Some((call_id, idem_key, args_preview)) = slot_meta {
            payload["call_id"] = serde_json::json!(call_id);
            if let Some(key) = idem_key {
                payload["idempotency_key"] = serde_json::json!(key);
            }
            if let Some(args_preview) = args_preview {
                payload["args_preview"] = serde_json::json!(args_preview);
            }
        }
        if preview_redactions > 0 {
            payload["args_preview_redactions"] = serde_json::json!(preview_redactions);
        }
        if let Some(output) = output {
            let clipped = clip_output_preview(output);
            let (redacted, redactions) = redact_credentials_for_storage(&clipped);
            if redactions > 0 {
                payload["redactions"] = serde_json::json!(redactions);
            }
            payload["output"] = serde_json::json!(redacted);
        }
        self.emit_with_payload(StepEventType::ToolCallSkipped, payload);
        self.checkpoint_count += 1;
    }

    /// Attach a cached result to the most recently completed slot.
    /// Called after `complete_tool()` when the result is stored in the idempotency cache.
    pub fn attach_cached_result(&mut self, cached: CachedToolResult) {
        let slot_idx = self.resolve_terminal_slot_index(None);
        if let Some(ref mut step) = self.current_step
            && let Some(slot) = step.execution.cursor.slots.get_mut(slot_idx)
        {
            slot.cached_result = Some(cached);
        }
    }

    /// Attach a cached result to a specific tool call.
    pub fn attach_cached_result_for_call(&mut self, call_id: &str, cached: CachedToolResult) {
        let slot_idx = self.resolve_terminal_slot_index(Some(call_id));
        if let Some(ref mut step) = self.current_step
            && let Some(slot) = step.execution.cursor.slots.get_mut(slot_idx)
        {
            slot.cached_result = Some(cached);
        }
    }

    /// Record tool execution result.
    pub fn complete_tool(
        &mut self,
        tool_name: &str,
        is_error: bool,
        elapsed_ms: u64,
        was_cached: bool,
    ) {
        self.complete_tool_inner(
            tool_name, is_error, elapsed_ms, was_cached, None, None, None, None, None,
        );
    }

    /// Record tool execution result with output for crash recovery cache warming.
    /// The output is included in the event payload so that `warm_cache_from_events()`
    /// can reconstruct the idempotency cache on session restore.
    pub fn complete_tool_with_result(
        &mut self,
        tool_name: &str,
        is_error: bool,
        elapsed_ms: u64,
        was_cached: bool,
        output: &str,
    ) {
        self.complete_tool_inner(
            tool_name,
            is_error,
            elapsed_ms,
            was_cached,
            Some(output),
            None,
            None,
            None,
            None,
        );
    }

    /// Record tool execution result with explicit trace metadata. Use this for
    /// runtime paths that already know the call id and arguments so payloads stay
    /// actionable even if slot metadata is incomplete.
    pub fn complete_tool_with_result_and_metadata(
        &mut self,
        tool_name: &str,
        call_id: &str,
        args_preview: Option<&str>,
        is_error: bool,
        elapsed_ms: u64,
        was_cached: bool,
        output: &str,
    ) {
        self.complete_tool_inner(
            tool_name,
            is_error,
            elapsed_ms,
            was_cached,
            Some(output),
            Some(call_id),
            args_preview,
            None,
            None,
        );
    }

    fn complete_tool_inner(
        &mut self,
        tool_name: &str,
        is_error: bool,
        elapsed_ms: u64,
        was_cached: bool,
        output: Option<&str>,
        fallback_call_id: Option<&str>,
        fallback_args_preview: Option<&str>,
        cached_result: Option<CachedToolResult>,
        extra_payload: Option<serde_json::Map<String, serde_json::Value>>,
    ) {
        let (sanitized_fallback_args_preview, fallback_preview_redactions) =
            sanitize_args_preview_for_storage(fallback_args_preview);
        let slot_idx = self.resolve_terminal_slot_index(fallback_call_id);

        self.ensure_terminal_slot_started(
            slot_idx,
            tool_name,
            fallback_call_id,
            sanitized_fallback_args_preview.clone(),
            fallback_preview_redactions,
        );

        // Extract trace metadata from slot before mutation.
        let slot_meta = self.slot_trace_meta(slot_idx);

        if let Some(ref mut step) = self.current_step {
            let state = if was_cached {
                if is_error {
                    SlotState::Failed
                } else {
                    SlotState::Completed
                }
            } else if is_error {
                SlotState::Failed
            } else {
                SlotState::Completed
            };
            if let Some(cached_result) = cached_result
                && let Some(slot) = step.execution.cursor.slots.get_mut(slot_idx)
            {
                slot.cached_result = Some(cached_result);
            }
            step.execution.cursor.advance_slot(slot_idx, state);

            // Track completed tool count in Act result
            if let Some(StepResult::Act {
                ref mut tool_results_count,
                ..
            }) = step.execution.result
            {
                *tool_results_count += 1;
            }
        }

        let event_type = if was_cached {
            if is_error {
                StepEventType::ToolCallFailed
            } else {
                StepEventType::ToolCallCompleted
            }
        } else if is_error {
            StepEventType::ToolCallFailed
        } else {
            StepEventType::ToolCallCompleted
        };

        let mut payload = serde_json::json!({
            "tool_name": tool_name,
            "slot_index": slot_idx,
            "elapsed_ms": elapsed_ms,
            "cached": was_cached,
            "is_error": is_error,
        });
        if let Some(extra) = extra_payload
            && let Some(payload_obj) = payload.as_object_mut()
        {
            for (key, value) in extra {
                payload_obj.insert(key, value);
            }
        }
        if let Some((call_id, idem_key, args_preview)) = slot_meta {
            let call_id = if call_id.is_empty() {
                fallback_call_id.unwrap_or("")
            } else {
                call_id.as_str()
            };
            if !call_id.is_empty() {
                payload["call_id"] = serde_json::json!(call_id);
            }
            if let Some(key) = idem_key {
                payload["idempotency_key"] = serde_json::json!(key);
            }
            if let Some(args_preview) = args_preview
                .as_deref()
                .or(sanitized_fallback_args_preview.as_deref())
            {
                payload["args_preview"] = serde_json::json!(args_preview);
            }
        } else if let Some(call_id) = fallback_call_id.filter(|value| !value.is_empty()) {
            payload["call_id"] = serde_json::json!(call_id);
            if let Some(args_preview) = sanitized_fallback_args_preview.as_deref() {
                payload["args_preview"] = serde_json::json!(args_preview);
            }
        }
        if fallback_preview_redactions > 0 {
            payload["args_preview_redactions"] = serde_json::json!(fallback_preview_redactions);
        }
        if let Some(out) = output {
            // Security: clip and redact tool output before persisting to disk.
            // Full output is already available in-memory for the LLM context;
            // the persisted copy should never contain unbounded or sensitive data.
            let clipped = clip_output_preview(out);
            let (redacted, redactions) = redact_credentials_for_storage(&clipped);
            if redactions > 0 {
                payload["redactions"] = serde_json::json!(redactions);
            }
            payload["output"] = serde_json::json!(redacted);
            if is_error {
                payload["error"] = serde_json::json!(redacted);
            }
        } else if is_error {
            payload["error"] = serde_json::json!("tool failed without captured error");
        }

        self.emit_with_payload(event_type, payload);

        self.tool_timings
            .entry(tool_name.to_string())
            .or_default()
            .push(elapsed_ms);

        self.checkpoint_count += 1;
    }

    fn active_call_id_for_tool(&self, tool_name: &str) -> Option<String> {
        self.current_step.as_ref().and_then(|step| {
            step.execution
                .cursor
                .slots
                .iter()
                .rev()
                .find(|slot| slot.tool_name == tool_name && slot.state == SlotState::Running)
                .map(|slot| slot.call_id.clone())
        })
    }

    fn resolve_terminal_slot_index(&self, call_id: Option<&str>) -> usize {
        if let Some(call_id) = call_id.filter(|value| !value.is_empty())
            && let Some(idx) = self.find_slot_index_by_call_id(call_id)
        {
            return idx;
        }

        if call_id.is_none()
            && let Some(idx) = self.find_latest_running_slot_index()
        {
            return idx;
        }

        if let Some(idx) = self.find_next_pending_slot_index() {
            return idx;
        }

        self.slot_counter.saturating_sub(1) as usize
    }

    fn find_slot_index_by_call_id(&self, call_id: &str) -> Option<usize> {
        self.current_step.as_ref().and_then(|step| {
            step.execution
                .cursor
                .slots
                .iter()
                .position(|slot| slot.call_id == call_id)
        })
    }

    fn find_next_pending_slot_index(&self) -> Option<usize> {
        self.current_step.as_ref().and_then(|step| {
            step.execution
                .cursor
                .slots
                .iter()
                .position(|slot| slot.state == SlotState::Pending)
        })
    }

    fn find_latest_running_slot_index(&self) -> Option<usize> {
        self.current_step.as_ref().and_then(|step| {
            step.execution
                .cursor
                .slots
                .iter()
                .rposition(|slot| slot.state == SlotState::Running)
        })
    }

    fn ensure_terminal_slot_started(
        &mut self,
        slot_idx: usize,
        tool_name: &str,
        fallback_call_id: Option<&str>,
        args_preview: Option<String>,
        args_preview_redactions: usize,
    ) {
        let needs_started = self
            .current_step
            .as_ref()
            .and_then(|step| step.execution.cursor.slots.get(slot_idx))
            .is_some_and(|slot| slot.state == SlotState::Pending);
        if !needs_started {
            return;
        }

        let call_id = fallback_call_id.unwrap_or("");
        if let Some(ref mut step) = self.current_step
            && let Some(slot) = step.execution.cursor.slots.get_mut(slot_idx)
        {
            slot.tool_name = tool_name.to_string();
            slot.call_id = call_id.to_string();
            slot.state = SlotState::Running;
            slot.args_preview = args_preview.clone();
        }
        self.slot_counter = self.slot_counter.max(slot_idx as u32 + 1);

        let mut started_payload = serde_json::json!({
            "tool_name": tool_name,
            "slot_index": slot_idx,
            "call_id": call_id,
        });
        if let Some(ap) = args_preview.as_deref() {
            started_payload["args_preview"] = serde_json::json!(ap);
        }
        if args_preview_redactions > 0 {
            started_payload["args_preview_redactions"] = serde_json::json!(args_preview_redactions);
        }
        self.emit_with_payload(StepEventType::ToolCallStarted, started_payload);
    }

    fn slot_trace_meta(&self, slot_idx: usize) -> Option<(String, Option<String>, Option<String>)> {
        self.current_step.as_ref().and_then(|step| {
            step.execution.cursor.slots.get(slot_idx).map(|s| {
                (
                    s.call_id.clone(),
                    s.idempotency_key.clone(),
                    s.args_preview.clone(),
                )
            })
        })
    }

    /// Record that microcompact or compression fired this turn.
    pub fn record_compaction(&mut self, results_compacted: u32, tokens_saved: u64, pressure: f64) {
        self.record_compaction_with_kind("unspecified", results_compacted, tokens_saved, pressure);
    }

    /// Record compaction with the concrete execution path that produced it.
    ///
    /// `kind` is deliberately an open string rather than a closed enum: the
    /// pipeline crate is below runtime-specific compaction policy, and durable
    /// telemetry must remain forward-compatible as new strategies are added.
    pub fn record_compaction_with_kind(
        &mut self,
        kind: &str,
        results_compacted: u32,
        tokens_saved: u64,
        pressure: f64,
    ) {
        self.emit_with_payload(
            StepEventType::CompactionFired,
            serde_json::json!({
                "kind": kind,
                "results_compacted": results_compacted,
                "tokens_saved": tokens_saved,
                "pressure": (pressure * 1000.0).round() / 1000.0,
            }),
        );
    }

    /// Record tool-level retry.
    pub fn record_retry(&mut self, tool_name: &str, attempt: u32, succeeded: bool) {
        self.emit_with_payload(
            StepEventType::RetryScheduled,
            serde_json::json!({
                "tool_name": tool_name,
                "attempt": attempt,
                "succeeded": succeeded,
            }),
        );

        if succeeded {
            let slot_idx = self.slot_counter.saturating_sub(1);
            if let Some(ref mut step) = self.current_step
                && let Some(slot) = step.execution.cursor.slots.get_mut(slot_idx as usize)
            {
                slot.retry_count = attempt;
                slot.state = SlotState::Completed;
            }
        }
    }

    /// Record turn_guard verdict (EVALUATE phase).
    pub fn record_verdict(
        &mut self,
        severity: &str,
        is_stall: bool,
        is_diverging: bool,
        advisory_threshold_reached: bool,
        injections_count: usize,
    ) {
        self.transition_phase(StepAction::Evaluate);

        let verdict = if is_stall {
            StepVerdict::Stalled
        } else if is_diverging {
            StepVerdict::Diverging
        } else {
            StepVerdict::Continue
        };

        if let Some(ref mut step) = self.current_step {
            let progress = step
                .execution
                .cursor
                .slots
                .iter()
                .filter(|s| s.state == SlotState::Completed)
                .count() as f64
                / step.execution.cursor.slots.len().max(1) as f64;

            step.execution.result = Some(StepResult::Evaluate {
                verdict,
                progress,
                should_continue: true,
                next_action: StepAction::Act,
            });
        }

        self.emit_with_payload(
            if is_stall {
                StepEventType::StallDetected
            } else if is_diverging {
                StepEventType::DivergenceDetected
            } else {
                StepEventType::StepEvaluated
            },
            serde_json::json!({
                "severity": severity,
                "advisory_threshold_reached": advisory_threshold_reached,
                "injections": injections_count,
            }),
        );

        self.checkpoint_count += 1;
    }

    /// Record LLM token usage for the turn.
    pub fn record_tokens(&mut self, prompt_tokens: u64, completion_tokens: u64) {
        if let Some(ref mut step) = self.current_step
            && let Some(StepResult::Act {
                ref mut tokens_in,
                ref mut tokens_out,
                ..
            }) = step.execution.result
        {
            *tokens_in = prompt_tokens;
            *tokens_out = completion_tokens;
        }
    }

    /// Finalize the current turn's step.
    ///
    /// **Idempotent guard**: if `completed_at` is already set, this is a no-op.
    /// This prevents duplicate terminal events when multiple code paths could
    /// reach `end_turn` (e.g., rate-limit early exit + tool phase fallback).
    pub fn end_turn(&mut self, completed: bool) {
        if let Some(ref mut step) = self.current_step {
            if step.execution.completed_at.is_some() {
                return; // already finalized — idempotent guard
            }
            if completed {
                step.execution.status = StepStatus::Completed;
            }
            step.execution.completed_at = Some(epoch_ms());
        }

        let event_type = if completed {
            StepEventType::StepCompleted
        } else {
            StepEventType::StepIncomplete
        };
        let step_id = self
            .current_step
            .as_ref()
            .map_or("unknown".to_string(), |s| s.step_id().to_string());
        self.emit(&step_id, event_type);
    }

    /// Get the execution summary after all turns complete.
    pub fn summary(&self) -> RecorderSummary {
        let total_tools: usize = self.tool_timings.values().map(|v| v.len()).sum();
        let total_tool_time_ms: u64 = self.tool_timings.values().flatten().sum();

        let mut slowest_tools: Vec<(String, u64)> = self
            .tool_timings
            .iter()
            .map(|(name, times)| {
                let avg = times.iter().sum::<u64>() / times.len().max(1) as u64;
                (name.clone(), avg)
            })
            .collect();
        slowest_tools.sort_by_key(|b| std::cmp::Reverse(b.1));
        slowest_tools.truncate(5);

        RecorderSummary {
            user_id: self.user_id.clone(),
            session_id: self.session_id.clone(),
            task_id: self.task_id.clone(),
            iterations: if self.events.is_empty() {
                0
            } else {
                self.turn_number + 1
            },
            total_events: self.events.len(),
            total_tools,
            total_tool_time_ms,
            slowest_tools,
            checkpoints: self.checkpoint_count,
            phase_log: self.phase_log.clone(),
        }
    }

    /// Get all recorded events (for persistence/audit).
    pub fn events(&self) -> &[StepEvent] {
        &self.events
    }

    /// Last durable event persistence error, if any.
    pub fn persistence_error(&self) -> Option<&str> {
        self.persistence_error.as_deref()
    }

    /// Get current step reference.
    pub fn current_step(&self) -> Option<&Step> {
        self.current_step.as_ref()
    }

    /// Access the scheduling contract for the current step.
    /// Returns default if no step is active.
    pub fn scheduling(&self) -> SchedulingContract {
        self.current_step
            .as_ref()
            .map(|s| s.descriptor.scheduling.clone())
            .unwrap_or_default()
    }

    /// Build a light checkpoint from current recorder state.
    /// Light checkpoints capture cursor position only — fast, small, frequent.
    pub fn build_light_checkpoint(&self) -> Option<LightCheckpoint> {
        let step = self.current_step.as_ref()?;
        Some(LightCheckpoint {
            protocol_version: PROTOCOL_VERSION,
            cursor: step.execution.cursor.clone(),
            step_id: step.step_id().to_string(),
            task_id: self.task_id.clone(),
            agent_id: self.session_id.clone(),
            progress: step
                .execution
                .cursor
                .slots
                .iter()
                .filter(|s| s.state == SlotState::Completed)
                .count() as f64
                / step.execution.cursor.slots.len().max(1) as f64,
            total_tokens: 0, // caller fills in
            created_at: epoch_ms(),
        })
    }

    /// Build a heavy checkpoint with full conversation state for crash recovery.
    pub fn build_heavy_checkpoint(
        &self,
        messages: &[serde_json::Value],
        budget_remaining_tokens: u64,
        budget_remaining_rounds: u32,
        blocked_tools: &[String],
        recent_tools: &[String],
    ) -> Option<HeavyCheckpoint> {
        self.build_heavy_checkpoint_with_interruption(
            messages,
            budget_remaining_tokens,
            budget_remaining_rounds,
            blocked_tools,
            recent_tools,
            None,
            None,
            0,
        )
    }

    /// Build a heavy checkpoint, optionally including a structured interruption record
    /// and approval overrides for session continuity.
    pub fn build_heavy_checkpoint_with_interruption(
        &self,
        messages: &[serde_json::Value],
        budget_remaining_tokens: u64,
        budget_remaining_rounds: u32,
        blocked_tools: &[String],
        recent_tools: &[String],
        interruption: Option<serde_json::Value>,
        approval_overrides: Option<serde_json::Value>,
        consecutive_context_window_errors: u32,
    ) -> Option<HeavyCheckpoint> {
        let light = self.build_light_checkpoint()?;
        astra_core::history_work::record_serialized_value(
            astra_core::history_work::HistoryWorkSite::PipelineHeavyCheckpointClone,
            messages,
        );
        Some(HeavyCheckpoint {
            light,
            conversation_cursor: None,
            messages: messages.to_vec(),
            budget_remaining_tokens,
            budget_remaining_rounds,
            blocked_tools: blocked_tools.to_vec(),
            recent_tools: recent_tools.to_vec(),
            activated_deferred_tool_names: Vec::new(),
            memory_context: self
                .current_step
                .as_ref()
                .and_then(|s| s.execution.memory_context.clone()),
            delegation_id: None,
            delegation_pattern: None,
            delegation_sub_run_summaries: Vec::new(),
            interruption,
            approval_overrides,
            consecutive_context_window_errors,
            pipeline_state: None,    // Set by caller after construction
            compaction_state: None,  // Set by caller after construction
            config_version_id: None, // Set by caller after construction (Step 2a)
        })
    }

    // ── Internal helpers ──

    fn transition_phase(&mut self, action: StepAction) {
        self.phase_log.push((self.turn_number, action, epoch_ms()));
        if let Some(ref mut step) = self.current_step {
            step.execution.cursor.phase = action;
        }
    }

    fn caused_by_for_next_event(&self) -> Vec<String> {
        if let Some(event) = self.events.last() {
            return vec![event.event_id.clone()];
        }
        self.persisted_tail_event_id
            .as_ref()
            .map(|event_id| vec![event_id.clone()])
            .unwrap_or_default()
    }

    fn emit(&mut self, step_id: &str, event_type: StepEventType) {
        let event = StepEvent {
            event_id: format!("evt-{}-{}", self.events.len(), epoch_ms()),
            canonical_event_id: None,
            step_id: step_id.to_string(),
            event_type,
            agent_id: None,
            caused_by: self.caused_by_for_next_event(),
            payload: Some(self.trace_context_payload()),
            created_at: epoch_ms(),
        };
        self.append_recorded_event(event);
    }

    fn emit_with_payload(&mut self, event_type: StepEventType, payload: serde_json::Value) {
        let step_id = self
            .current_step
            .as_ref()
            .map_or("unknown".to_string(), |s| s.step_id().to_string());
        let caused_by = self.caused_by_for_next_event();
        let event = StepEvent {
            event_id: format!("evt-{}-{}", self.events.len(), epoch_ms()),
            canonical_event_id: None,
            step_id,
            event_type,
            agent_id: None,
            caused_by,
            payload: Some(self.with_trace_context(payload)),
            created_at: epoch_ms(),
        };
        self.append_recorded_event(event);
    }

    fn append_recorded_event(&mut self, event: StepEvent) {
        if let Some(ref mut fs) = self.file_store {
            if let Err(error) = fs.append(event.clone()) {
                self.record_persistence_error(format!(
                    "failed to persist step event {}: {}",
                    event.event_id, error
                ));
                if self.persistence_required {
                    return;
                }
            }
        } else if self.persistence_required {
            self.record_persistence_error(format!(
                "step-event persistence unavailable; dropping event {}",
                event.event_id
            ));
            return;
        }
        self.events.push(event);
    }

    fn record_persistence_error(&mut self, message: String) {
        astra_core::agent_warn!("event_store", "{}", message);
        self.persistence_error = Some(message);
    }

    fn trace_context_payload(&self) -> serde_json::Value {
        serde_json::json!({
            "trace_context": {
                "visible_turn": self.turn_number,
                "round_index": self.round_index,
                "step_sequence": self.current_step_sequence,
            }
        })
    }

    fn with_trace_context(&self, mut payload: serde_json::Value) -> serde_json::Value {
        let trace_context = self.trace_context_payload()["trace_context"].clone();
        if let Some(object) = payload.as_object_mut() {
            object.insert("trace_context".to_string(), trace_context);
            payload
        } else {
            serde_json::json!({
                "value": payload,
                "trace_context": trace_context,
            })
        }
    }

    fn rebind_session_id(&mut self, session_id: &str) {
        let previous_session_id = self.session_id.clone();
        if previous_session_id == session_id {
            return;
        }

        self.session_id = session_id.to_string();

        if let Some(step) = self.current_step.as_mut() {
            rebind_step(step, &previous_session_id, session_id);
        }
        for event in &mut self.events {
            rebind_step_id(&mut event.step_id, &previous_session_id, session_id);
        }
    }
}

fn rebind_step(step: &mut Step, previous_session_id: &str, session_id: &str) {
    rebind_step_id(
        &mut step.descriptor.step_id,
        previous_session_id,
        session_id,
    );
    if let Some(parent_step_id) = step.descriptor.parent_step_id.as_mut() {
        rebind_step_id(parent_step_id, previous_session_id, session_id);
    }
    if let Some(checkpoint) = step.checkpoint.as_mut() {
        match checkpoint {
            StepCheckpoint::Light(light) => {
                rebind_step_id(&mut light.step_id, previous_session_id, session_id);
            }
            StepCheckpoint::Heavy(heavy) => {
                rebind_step_id(&mut heavy.light.step_id, previous_session_id, session_id);
            }
        }
    }
}

fn rebind_step_id(step_id: &mut String, previous_session_id: &str, session_id: &str) {
    let previous_prefix = format!("{previous_session_id}-turn-");
    if let Some(suffix) = step_id.strip_prefix(&previous_prefix) {
        *step_id = format!("{session_id}-turn-{suffix}");
    }
}

#[derive(Default)]
struct PersistedEventSummary {
    next_step_sequence: u32,
    tail_event_id: Option<String>,
}

fn persisted_event_summary(user_id: &str, session_id: &str) -> PersistedEventSummary {
    let mut max_sequence = None;
    let mut tail_event_id = None;
    if let Err(error) = FileBackedEventStore::for_each_event(user_id, session_id, |event| {
        if let Some(sequence) = step_sequence_from_event(event) {
            max_sequence = Some(max_sequence.map_or(sequence, |max: u32| max.max(sequence)));
        }
        tail_event_id = Some(event.event_id.clone());
    }) {
        astra_core::agent_warn!(
            "step_recorder",
            "Failed to scan persisted step sequence for session {}: {}",
            session_id,
            error
        );
    }
    PersistedEventSummary {
        next_step_sequence: max_sequence.map_or(0, |seq| seq.saturating_add(1)),
        tail_event_id,
    }
}

fn step_sequence_from_event(event: &StepEvent) -> Option<u32> {
    event
        .payload
        .as_ref()
        .and_then(|payload| payload.get("trace_context"))
        .and_then(|ctx| ctx.get("step_sequence"))
        .and_then(serde_json::Value::as_u64)
        .and_then(|seq| u32::try_from(seq).ok())
        .or_else(|| {
            event
                .step_id
                .rsplit("-step-")
                .next()
                .and_then(|seq| seq.parse::<u32>().ok())
        })
}

/// Summary of a recorded session for debugging/audit.
#[derive(Debug, Clone)]
pub struct RecorderSummary {
    pub user_id: String,
    pub session_id: String,
    pub task_id: String,
    pub iterations: u32,
    pub total_events: usize,
    pub total_tools: usize,
    pub total_tool_time_ms: u64,
    pub slowest_tools: Vec<(String, u64)>,
    pub checkpoints: u32,
    pub phase_log: Vec<(u32, StepAction, u64)>,
}

fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_USER_ID: &str = "test-user";

    #[test]
    fn heavy_checkpoint_clone_preserves_structured_history_independently() {
        let mut recorder = StepRecorder::new(TEST_USER_ID, "session-1", "task-1");
        recorder.begin_turn(1);
        let mut messages = vec![
            serde_json::json!({"role": "user", "content": "run the structured tool round"}),
            serde_json::json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call-1",
                    "type": "function",
                    "function": {"name": "bash", "arguments": "{\"command\":\"true\"}"}
                }]
            }),
            serde_json::json!({"role": "tool", "tool_call_id": "call-1", "content": "ok"}),
        ];

        let checkpoint = recorder
            .build_heavy_checkpoint(&messages, 20_000, 5, &[], &[])
            .expect("active recorder should build a heavy checkpoint");
        messages[0]["content"] = serde_json::json!("mutated after checkpoint");
        messages.push(serde_json::json!({"role": "assistant", "content": "later"}));

        assert_eq!(checkpoint.messages.len(), 3);
        assert_eq!(
            checkpoint.messages[0]["content"],
            "run the structured tool round"
        );
        assert_eq!(
            checkpoint.messages[1]["tool_calls"][0]["function"]["name"],
            "bash"
        );
        assert_eq!(checkpoint.messages[2]["tool_call_id"], "call-1");
    }

    #[test]
    fn redact_credentials_redacts_assignments_but_keeps_token_counters() {
        let input = "OPENAI_API_KEY=sk-test-secret\npassword: hunter2\ntoken_count: 3";
        let (redacted, count) = redact_credentials_for_storage(input);
        assert_eq!(count, 2);
        assert!(redacted.contains("OPENAI_API_KEY=[REDACTED]"));
        assert!(redacted.contains("password: [REDACTED]"));
        assert!(redacted.contains("token_count: 3"));
    }

    #[test]
    fn redact_credentials_redacts_auth_headers_and_standalone_tokens() {
        let input = "Authorization: Bearer sk-test-secret-value\nclipboard sk-test-secret-value";
        let (redacted, count) = redact_credentials_for_storage(input);
        assert_eq!(count, 2);
        assert!(redacted.contains("Authorization: [REDACTED]"));
        assert!(redacted.contains("clipboard [REDACTED_API_KEY]"));
    }

    #[test]
    fn redact_credentials_redacts_pem_blocks() {
        let input = "-----BEGIN PRIVATE KEY-----\nabc123\n-----END PRIVATE KEY-----\nstatus: ok";
        let (redacted, count) = redact_credentials_for_storage(input);
        assert_eq!(count, 1);
        assert!(redacted.contains("[REDACTED_PEM_BLOCK]"));
        assert!(redacted.contains("status: ok"));
    }

    #[test]
    fn sanitize_args_preview_redacts_secrets_before_persistence() {
        let (sanitized, count) = sanitize_args_preview_for_storage(Some(
            "Authorization: Bearer sk-test-secret-value path=src/main.rs",
        ));
        assert_eq!(count, 1);
        let sanitized = sanitized.expect("sanitized preview");
        assert!(sanitized.contains("Authorization: [REDACTED]"));
        assert!(!sanitized.contains("sk-test-secret-value"));
    }

    #[test]
    fn sanitize_args_preview_redacts_shell_credentials_before_persistence() {
        let (sanitized, count) = sanitize_args_preview_for_storage(Some(
            "sshpass -p 'opaque-password' ssh host; DB_PASSWORD=another-secret mysql --password=third-secret",
        ));
        let sanitized = sanitized.expect("sanitized preview");

        assert_eq!(count, 3, "every durable shell secret must be accounted for");
        assert!(sanitized.contains("sshpass -p [REDACTED]"));
        assert!(sanitized.contains("DB_PASSWORD=[REDACTED]"));
        assert!(sanitized.contains("--password=[REDACTED]"));
        for secret in ["opaque-password", "another-secret", "third-secret"] {
            assert!(!sanitized.contains(secret), "leaked shell secret: {secret}");
        }
    }

    #[test]
    fn recorder_basic_lifecycle() {
        let mut rec = StepRecorder::new(TEST_USER_ID, "sess-1", "task-1");
        rec.begin_turn(0);

        assert!(rec.current_step().is_some());
        assert_eq!(rec.current_step().unwrap().action(), StepAction::Perceive);
        assert_eq!(rec.events().len(), 1); // StepCreated
    }

    #[test]
    fn llm_round_events_preserve_inference_purpose() {
        let mut rec = StepRecorder::new(TEST_USER_ID, "sess-purpose", "task-1");
        rec.begin_llm_round("model-a", InferencePurpose::SubAgent);
        rec.end_llm_round("model-a", InferencePurpose::SubAgent, 10, 4, 3, 2, 25);

        let round_events = rec
            .events()
            .iter()
            .filter(|event| {
                matches!(
                    event.event_type,
                    StepEventType::LlmRoundStarted | StepEventType::LlmRoundCompleted
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(round_events.len(), 2);
        for event in round_events {
            let purpose = serde_json::from_value::<InferencePurpose>(
                event.payload.as_ref().expect("round payload")["purpose"].clone(),
            )
            .expect("typed inference purpose");
            assert_eq!(purpose, InferencePurpose::SubAgent);
        }
    }

    #[test]
    fn compaction_event_preserves_the_execution_path_kind() {
        let mut rec = StepRecorder::new(TEST_USER_ID, "sess-compaction-kind", "task-1");
        rec.begin_turn_with_context(4, 2);
        rec.record_compaction_with_kind("microcompact", 3, 1_500, 0.812);

        let event = rec
            .events()
            .iter()
            .find(|event| event.event_type == StepEventType::CompactionFired)
            .expect("compaction event");
        let payload = event.payload.as_ref().expect("compaction payload");
        assert_eq!(
            payload.get("kind").and_then(serde_json::Value::as_str),
            Some("microcompact")
        );
        assert_eq!(
            payload
                .pointer("/trace_context/visible_turn")
                .and_then(serde_json::Value::as_u64),
            Some(4)
        );
        assert_eq!(
            payload
                .pointer("/trace_context/round_index")
                .and_then(serde_json::Value::as_u64),
            Some(2)
        );
    }

    #[test]
    fn recorder_perceive_records_memory() {
        let mut rec = StepRecorder::new(TEST_USER_ID, "sess-1", "task-1");
        rec.begin_turn(0);
        rec.record_perceive(
            "show me PRs",
            &["mem-1".into(), "mem-2".into()],
            &["github".into()],
        );

        let step = rec.current_step().unwrap();
        let mc = step.execution.memory_context.as_ref().unwrap();
        assert_eq!(mc.retrieved_memory_ids.len(), 2);
        assert_eq!(mc.domain_hints, vec!["github"]);
        assert_eq!(mc.governance_actions.len(), 2); // 2 Retrieved
    }

    #[test]
    fn recorder_plan_phase_transition() {
        let mut rec = StepRecorder::new(TEST_USER_ID, "sess-1", "task-1");
        rec.begin_turn(0);
        rec.record_plan(&["github".into(), "grep".into()], 0.3, 4000);

        let step = rec.current_step().unwrap();
        assert_eq!(step.execution.cursor.phase, StepAction::Plan);
        assert!(rec.events().len() >= 2); // Created + Started
    }

    #[test]
    fn recorder_act_with_tools() {
        let mut rec = StepRecorder::new(TEST_USER_ID, "sess-1", "task-1");
        rec.begin_turn(0);
        rec.begin_act(2);

        let step = rec.current_step().unwrap();
        assert_eq!(step.execution.cursor.phase, StepAction::Act);
        assert_eq!(step.execution.cursor.slots.len(), 2);

        rec.begin_tool("grep", "call-1");
        rec.complete_tool("grep", false, 50, false);

        rec.begin_tool("read_file", "call-2");
        rec.complete_tool("read_file", false, 10, false);

        let step = rec.current_step().unwrap();
        assert_eq!(step.execution.cursor.slots[0].state, SlotState::Completed);
        assert_eq!(step.execution.cursor.slots[1].state, SlotState::Completed);
        assert!(step.execution.cursor.all_slots_done());
    }

    #[test]
    fn recorder_act_prepopulates_pending_tool_slots() {
        let mut rec = StepRecorder::new(TEST_USER_ID, "sess-1", "task-1");
        rec.begin_turn(0);
        rec.begin_act_with_slots(vec![
            ExecutionSlotSpec {
                tool_name: "web_fetch".into(),
                call_id: "call-1".into(),
                idempotency_key: Some("idem-1".into()),
                args_preview: Some("url=https://example.com".into()),
            },
            ExecutionSlotSpec {
                tool_name: "task".into(),
                call_id: "call-2".into(),
                idempotency_key: None,
                args_preview: Some("action=update".into()),
            },
        ]);

        let step = rec.current_step().unwrap();
        assert_eq!(step.execution.cursor.phase, StepAction::Act);
        assert_eq!(step.execution.cursor.slots.len(), 2);
        assert_eq!(step.execution.cursor.slots[0].tool_name, "web_fetch");
        assert_eq!(step.execution.cursor.slots[0].call_id, "call-1");
        assert_eq!(step.execution.cursor.slots[0].state, SlotState::Pending);
        assert_eq!(
            step.execution.cursor.slots[0].idempotency_key.as_deref(),
            Some("idem-1")
        );
        assert_eq!(
            step.execution.cursor.slots[0].args_preview.as_deref(),
            Some("url=https://example.com")
        );
        assert_eq!(step.execution.cursor.slots[1].tool_name, "task");
        assert_eq!(step.execution.cursor.slots[1].call_id, "call-2");
        assert!(
            step.execution
                .cursor
                .slots
                .iter()
                .all(|slot| !slot.tool_name.is_empty() && !slot.call_id.is_empty())
        );
    }

    #[test]
    fn recorder_cached_tool_completes_with_cached_marker() {
        let mut rec = StepRecorder::new(TEST_USER_ID, "sess-1", "task-1");
        rec.begin_turn(0);
        rec.begin_act(1);
        rec.begin_tool("grep", "call-1");
        rec.complete_tool("grep", false, 0, true); // cached

        let step = rec.current_step().unwrap();
        assert_eq!(step.execution.cursor.slots[0].state, SlotState::Completed);
        let last = rec.events().last().unwrap();
        assert_eq!(last.event_type, StepEventType::ToolCallCompleted);
        assert_eq!(
            last.payload
                .as_ref()
                .and_then(|payload| payload.get("cached"))
                .and_then(serde_json::Value::as_bool),
            Some(true)
        );
    }

    #[test]
    fn recorder_tool_failure_and_retry() {
        let mut rec = StepRecorder::new(TEST_USER_ID, "sess-1", "task-1");
        rec.begin_turn(0);
        rec.begin_act(1);
        rec.begin_tool("bash", "call-1");
        rec.complete_tool("bash", true, 100, false); // fails

        let step = rec.current_step().unwrap();
        assert_eq!(step.execution.cursor.slots[0].state, SlotState::Failed);

        rec.record_retry("bash", 1, true); // retry succeeds
        let step = rec.current_step().unwrap();
        assert_eq!(step.execution.cursor.slots[0].state, SlotState::Completed);
        assert_eq!(step.execution.cursor.slots[0].retry_count, 1);
    }

    #[test]
    fn recorder_verdict_stall() {
        let mut rec = StepRecorder::new(TEST_USER_ID, "sess-1", "task-1");
        rec.begin_turn(0);
        rec.begin_act(1);
        rec.begin_tool("grep", "call-1");
        rec.complete_tool("grep", false, 50, false);

        rec.record_verdict("Warning", true, false, false, 1);

        let step = rec.current_step().unwrap();
        assert_eq!(step.execution.cursor.phase, StepAction::Evaluate);
        if let Some(StepResult::Evaluate { verdict, .. }) = &step.execution.result {
            assert_eq!(*verdict, StepVerdict::Stalled);
        } else {
            panic!("expected Evaluate result");
        }

        // Should have StallDetected event
        assert!(
            rec.events()
                .iter()
                .any(|e| e.event_type == StepEventType::StallDetected)
        );
    }

    #[test]
    fn skip_tool_with_reason_marks_slot_and_records_payload() {
        let mut rec = StepRecorder::new(TEST_USER_ID, "sess-1", "task-1");
        rec.begin_turn(0);
        rec.begin_act(1);
        rec.begin_tool_with_key("grep", "call-1", Some("sem:grep"));

        rec.skip_tool_with_reason(
            "grep",
            "duplicate_within_turn",
            false,
            Some("blocked duplicate output"),
        );

        let step = rec.current_step().unwrap();
        assert_eq!(step.execution.cursor.slots[0].state, SlotState::Skipped);
        let last = rec.events().last().unwrap();
        assert_eq!(last.event_type, StepEventType::ToolCallSkipped);
        let payload = last.payload.as_ref().unwrap();
        assert_eq!(
            payload.get("reason").and_then(serde_json::Value::as_str),
            Some("duplicate_within_turn")
        );
        assert_eq!(
            payload
                .get("idempotency_key")
                .and_then(serde_json::Value::as_str),
            Some("sem:grep")
        );
        assert_eq!(
            payload.get("output").and_then(serde_json::Value::as_str),
            Some("blocked duplicate output")
        );
    }

    #[test]
    fn recorder_verdict_advisory_threshold_reached() {
        let mut rec = StepRecorder::new(TEST_USER_ID, "sess-1", "task-1");
        rec.begin_turn(0);
        rec.begin_act(0);
        rec.record_verdict("Critical", false, false, true, 2);

        let step = rec.current_step().unwrap();
        if let Some(StepResult::Evaluate {
            should_continue,
            next_action,
            ..
        }) = &step.execution.result
        {
            assert!(*should_continue);
            assert_eq!(*next_action, StepAction::Act);
        }
    }

    #[test]
    fn recorder_end_turn_completed() {
        let mut rec = StepRecorder::new(TEST_USER_ID, "sess-1", "task-1");
        rec.begin_turn(0);
        rec.end_turn(true);

        let step = rec.current_step().unwrap();
        assert_eq!(step.status(), StepStatus::Completed);
        assert!(step.execution.completed_at.is_some());
    }

    #[test]
    fn regression_incomplete_turn_is_not_recorded_as_retry() {
        let mut rec = StepRecorder::new(TEST_USER_ID, "sess-regression", "task-1");
        rec.begin_turn(0);
        rec.begin_act(1);
        rec.begin_tool("read_file", "call-1");
        rec.complete_tool("read_file", false, 12, false);

        rec.end_turn(false);

        let last = rec.events().last().unwrap();
        assert_eq!(
            last.event_type,
            StepEventType::StepIncomplete,
            "normal incomplete turn progression must not be mislabeled as retry"
        );
        assert!(
            !rec.events()
                .iter()
                .any(|event| event.event_type == StepEventType::StepRetried),
            "StepRetried should be reserved for actual retry scheduling"
        );
    }

    #[test]
    fn regression_incomplete_turn_has_single_terminal_event() {
        let mut rec = StepRecorder::new(TEST_USER_ID, "sess-regression", "task-1");
        rec.begin_turn(0);
        rec.begin_act(1);
        rec.begin_tool("read_file", "call-1");
        rec.complete_tool("read_file", false, 12, false);
        rec.record_verdict("Healthy", false, false, false, 0);

        rec.end_turn(false);

        let terminal_events: Vec<_> = rec
            .events()
            .iter()
            .filter(|event| {
                matches!(
                    event.event_type,
                    StepEventType::StepCompleted
                        | StepEventType::StepIncomplete
                        | StepEventType::StepFailed
                        | StepEventType::StepRetried
                )
            })
            .collect();
        assert_eq!(
            terminal_events.len(),
            1,
            "a step must not record both StepCompleted and StepIncomplete: {terminal_events:?}"
        );
        assert_eq!(terminal_events[0].event_type, StepEventType::StepIncomplete);
    }

    #[test]
    fn regression_failed_tool_event_carries_actionable_payload() {
        let mut rec = StepRecorder::new(TEST_USER_ID, "sess-regression", "task-1");
        rec.begin_turn(0);
        rec.begin_act(1);
        rec.begin_tool_with_key_and_args_preview(
            "str_replace",
            "call-1",
            Some("sem:str_replace:file.rs"),
            Some("path=file.rs old_str=fn_old"),
        );

        rec.complete_tool_with_result(
            "str_replace",
            true,
            7,
            false,
            "❌ STR_REPLACE FAILED — FILE NOT MODIFIED\n\nWHAT: old_str not found in file.\nWHY:  The exact byte sequence does not appear in the current file content.\nNEXT: Re-read the target region with read_file, copy the actual bytes into old_str (including indentation), then retry. Diagnostic hints below:\n",
        );

        let failed = rec
            .events()
            .iter()
            .find(|event| event.event_type == StepEventType::ToolCallFailed)
            .expect("expected failed tool event");
        let payload = failed.payload.as_ref().expect("failed event payload");
        assert_eq!(
            payload.get("tool_name").and_then(serde_json::Value::as_str),
            Some("str_replace")
        );
        assert_eq!(
            payload.get("call_id").and_then(serde_json::Value::as_str),
            Some("call-1")
        );
        assert_eq!(
            payload
                .get("idempotency_key")
                .and_then(serde_json::Value::as_str),
            Some("sem:str_replace:file.rs")
        );
        assert_eq!(
            payload
                .get("args_preview")
                .and_then(serde_json::Value::as_str),
            Some("path=file.rs old_str=fn_old")
        );
        assert!(
            payload
                .get("error")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|error| error.contains("old_str not found")),
            "failed event should carry actionable error, got: {payload:?}"
        );
    }

    #[test]
    fn recorder_redacts_args_preview_in_started_and_completed_events() {
        let mut rec = StepRecorder::new(TEST_USER_ID, "sess-redaction", "task-1");
        rec.begin_turn(0);
        rec.begin_act(1);
        rec.begin_tool_with_key_and_args_preview(
            "write_file",
            "call-1",
            None,
            Some("path=secret.txt Authorization: Bearer sk-test-secret-value"),
        );
        rec.complete_tool_with_result("write_file", false, 8, false, "ok");

        let started = rec
            .events()
            .iter()
            .find(|event| event.event_type == StepEventType::ToolCallStarted)
            .expect("started event");
        let started_payload = started.payload.as_ref().unwrap();
        let started_preview = started_payload
            .get("args_preview")
            .and_then(serde_json::Value::as_str)
            .unwrap();
        assert!(started_preview.contains("Authorization:"));
        assert!(started_preview.contains("[REDACTED"));
        assert!(!started_preview.contains("sk-test-secret-value"));
        assert_eq!(
            started_payload
                .get("args_preview_redactions")
                .and_then(serde_json::Value::as_u64),
            Some(1)
        );

        let completed = rec
            .events()
            .iter()
            .find(|event| event.event_type == StepEventType::ToolCallCompleted)
            .expect("completed event");
        let completed_payload = completed.payload.as_ref().unwrap();
        let completed_preview = completed_payload
            .get("args_preview")
            .and_then(serde_json::Value::as_str)
            .unwrap();
        assert!(completed_preview.contains("Authorization:"));
        assert!(completed_preview.contains("[REDACTED"));
        assert!(!completed_preview.contains("sk-test-secret-value"));
    }

    #[test]
    fn skip_tool_with_reason_redacts_persisted_output() {
        let mut rec = StepRecorder::new(TEST_USER_ID, "sess-redaction", "task-1");
        rec.begin_turn(0);
        rec.begin_act(1);
        rec.begin_tool("bash", "call-1");
        rec.skip_tool_with_reason(
            "bash",
            "duplicate_within_turn",
            false,
            Some("Authorization: Bearer sk-test-secret-value"),
        );

        let payload = rec.events().last().unwrap().payload.as_ref().unwrap();
        let output = payload
            .get("output")
            .and_then(serde_json::Value::as_str)
            .unwrap();
        assert_eq!(output, "Authorization: [REDACTED]");
        assert_eq!(
            payload
                .get("redactions")
                .and_then(serde_json::Value::as_u64),
            Some(1)
        );
    }

    #[test]
    fn complete_tool_with_metadata_backfills_actionable_payload() {
        let mut rec = StepRecorder::new(TEST_USER_ID, "sess-regression", "task-1");
        rec.begin_turn(0);
        rec.begin_act(1);

        rec.complete_tool_with_result_and_metadata(
            "read_file",
            "call-read-1",
            Some("src/main.rs"),
            false,
            12,
            false,
            "file contents",
        );

        let last = rec.events().last().unwrap();
        assert_eq!(last.event_type, StepEventType::ToolCallCompleted);
        let payload = last.payload.as_ref().unwrap();
        assert_eq!(
            payload.get("call_id").and_then(serde_json::Value::as_str),
            Some("call-read-1")
        );
        assert_eq!(
            payload
                .get("args_preview")
                .and_then(serde_json::Value::as_str),
            Some("src/main.rs")
        );
    }

    #[test]
    fn recorder_summary() {
        let mut rec = StepRecorder::new(TEST_USER_ID, "sess-1", "task-1");

        // Turn 0: 2 tools
        rec.begin_turn(0);
        rec.begin_act(2);
        rec.begin_tool("grep", "c1");
        rec.complete_tool("grep", false, 100, false);
        rec.begin_tool("read_file", "c2");
        rec.complete_tool("read_file", false, 30, false);
        rec.end_turn(false);

        // Turn 1: 1 tool
        rec.begin_turn(1);
        rec.begin_act(1);
        rec.begin_tool("grep", "c3");
        rec.complete_tool("grep", false, 80, false);
        rec.end_turn(true);

        let summary = rec.summary();
        assert_eq!(summary.user_id, TEST_USER_ID);
        assert_eq!(summary.session_id, "sess-1");
        assert_eq!(summary.task_id, "task-1");
        assert_eq!(summary.iterations, 2);
        assert_eq!(summary.total_tools, 3);
        assert_eq!(summary.total_tool_time_ms, 210);
        assert!(!summary.slowest_tools.is_empty());
        assert_eq!(summary.slowest_tools[0].0, "grep"); // grep is slowest (avg 90ms)
    }

    #[test]
    fn recorder_events_form_causal_chain() {
        let mut rec = StepRecorder::new(TEST_USER_ID, "sess-1", "task-1");
        rec.begin_turn(0);
        rec.begin_act(1);
        rec.begin_tool("grep", "c1");
        rec.complete_tool("grep", false, 50, false);

        // Every event after the first should reference the previous
        for i in 1..rec.events().len() {
            assert!(
                !rec.events()[i].caused_by.is_empty(),
                "Event {} should have a causal parent",
                i
            );
            assert_eq!(rec.events()[i].caused_by[0], rec.events()[i - 1].event_id);
        }
    }

    #[test]
    fn recorder_multi_turn_phase_log() {
        let mut rec = StepRecorder::new(TEST_USER_ID, "sess-1", "task-1");
        rec.begin_turn(0);
        rec.record_plan(&["grep".into()], 0.0, 4000);
        rec.begin_act(1);
        rec.record_verdict("Healthy", false, false, false, 0);
        rec.end_turn(false);

        rec.begin_turn(1);
        rec.begin_act(1);
        rec.end_turn(true);

        // Phase log should capture all transitions
        let phases: Vec<StepAction> = rec.summary().phase_log.iter().map(|(_, a, _)| *a).collect();
        // Turn 0: Perceive, Plan, Act, Evaluate
        // Turn 1: Perceive, Act
        assert!(phases.contains(&StepAction::Perceive));
        assert!(phases.contains(&StepAction::Plan));
        assert!(phases.contains(&StepAction::Act));
        assert!(phases.contains(&StepAction::Evaluate));
    }

    #[test]
    fn with_persistence_starts_after_existing_checkpoints() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(tmp.path());
        let sid = "test-cp-resume";

        let light = crate::step_protocol::StepCheckpoint::light(
            "step-3".to_string(),
            "task-1".to_string(),
            sid.to_string(),
            crate::step_protocol::ExecutionCursor::default(),
        );
        crate::step_checkpoint::write_step_checkpoint(TEST_USER_ID, sid, 3, &light).unwrap();
        let heavy = crate::step_protocol::StepCheckpoint::heavy(
            "step-5".to_string(),
            "task-1".to_string(),
            sid.to_string(),
            crate::step_protocol::ExecutionCursor::default(),
        );
        crate::step_checkpoint::write_step_checkpoint(TEST_USER_ID, sid, 5, &heavy).unwrap();

        let rec = StepRecorder::with_persistence(TEST_USER_ID, sid, "task-1");
        // checkpoint_count should be max(5,3) + 1 = 6
        assert_eq!(
            rec.summary().checkpoints,
            6,
            "checkpoint_count must start after existing max"
        );
        // tmp is dropped here, cleaning up automatically
    }

    #[test]
    fn attach_persistence_rebinds_existing_events_to_adopted_session() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(tmp.path());

        let mut rec = StepRecorder::new(TEST_USER_ID, "ephemeral", "task-1");
        rec.begin_turn(0);
        rec.record_plan(&["bash".into()], 0.0, 4000);
        rec.attach_persistence("sess-adopted");
        rec.end_turn(true);

        assert_eq!(rec.summary().user_id, TEST_USER_ID);
        assert_eq!(rec.summary().session_id, "sess-adopted");
        assert_eq!(
            rec.current_step().unwrap().step_id(),
            "sess-adopted-turn-0-step-0"
        );
        assert!(
            rec.events()
                .iter()
                .all(|event| event.step_id == "sess-adopted-turn-0-step-0")
        );

        let parsed =
            crate::step_checkpoint::FileBackedEventStore::new(TEST_USER_ID, "sess-adopted")
                .all_events()
                .to_vec();
        assert!(
            parsed
                .iter()
                .any(|event| event.step_id == "sess-adopted-turn-0-step-0")
        );
        assert!(
            !crate::step_checkpoint::owner_session_dir_for(TEST_USER_ID, "ephemeral")
                .unwrap()
                .exists()
        );
    }

    #[test]
    fn deferred_persistence_attaches_on_adoption_but_plain_recorder_stays_memory_only() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(tmp.path());

        let mut deferred =
            StepRecorder::with_deferred_persistence(TEST_USER_ID, "ephemeral", "task-deferred");
        deferred.begin_turn(1);
        deferred.record_plan(&["read_file".into()], 0.0, 4000);
        deferred.attach_persistence_if_configured("sess-deferred");
        deferred.end_turn(true);

        let deferred_events =
            crate::step_checkpoint::FileBackedEventStore::new(TEST_USER_ID, "sess-deferred")
                .all_events()
                .to_vec();
        assert!(!deferred_events.is_empty());
        assert!(
            deferred_events
                .iter()
                .all(|event| event.step_id.starts_with("sess-deferred-turn-1-step-"))
        );

        let mut memory_only = StepRecorder::new(TEST_USER_ID, "ephemeral", "task-memory");
        memory_only.begin_turn(1);
        memory_only.record_plan(&["read_file".into()], 0.0, 4000);
        memory_only.attach_persistence_if_configured("sess-memory-only");
        memory_only.end_turn(true);

        assert!(
            crate::step_checkpoint::FileBackedEventStore::new(TEST_USER_ID, "sess-memory-only")
                .all_events()
                .is_empty(),
            "ordinary in-memory recorders must not gain disk side effects merely because a response carries a session id"
        );
    }

    #[test]
    fn required_persistence_drops_new_events_after_attach_failure() {
        let mut rec = StepRecorder::new(TEST_USER_ID, "ephemeral", "task-1");
        rec.begin_turn(0);
        rec.attach_persistence("../invalid-session-id");
        let events_after_failed_attach = rec.events().len();

        rec.record_plan(&["bash".into()], 0.0, 4000);

        assert!(rec.persistence_error().is_some());
        assert_eq!(
            rec.events().len(),
            events_after_failed_attach,
            "required persistence must fail closed instead of reverting to memory-only events"
        );
    }

    #[test]
    fn with_persistence_continues_step_sequence_and_causal_chain_across_recorders() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(tmp.path());
        let sid = "sess-continued";

        let mut first = StepRecorder::with_persistence(TEST_USER_ID, sid, "task-1");
        first.begin_turn_with_context(0, 0);
        first.end_turn(false);
        let previous_tail = first.events().last().unwrap().event_id.clone();
        drop(first);

        let mut second = StepRecorder::with_persistence(TEST_USER_ID, sid, "task-2");
        assert!(
            second.events().is_empty(),
            "persistent recorder must not materialize historical journals into memory"
        );
        second.begin_turn_with_context(1, 0);

        assert_eq!(
            second.current_step().unwrap().step_id(),
            "sess-continued-turn-1-step-1"
        );
        let created = second.events().last().unwrap();
        assert_eq!(created.event_type, StepEventType::StepCreated);
        assert_eq!(created.caused_by, vec![previous_tail]);
        let trace_context = created
            .payload
            .as_ref()
            .and_then(|payload| payload.get("trace_context"))
            .unwrap();
        assert_eq!(
            trace_context
                .get("visible_turn")
                .and_then(serde_json::Value::as_u64),
            Some(1)
        );
        assert_eq!(
            trace_context
                .get("round_index")
                .and_then(serde_json::Value::as_u64),
            Some(0)
        );
        assert_eq!(
            trace_context
                .get("step_sequence")
                .and_then(serde_json::Value::as_u64),
            Some(1)
        );
    }

    #[test]
    fn regression_recreated_visible_turns_have_unique_step_ids_and_context() {
        let mut rec = StepRecorder::new(TEST_USER_ID, "sess-regression", "task-1");
        rec.begin_turn(6);
        rec.end_turn(false);
        rec.begin_turn(6);
        rec.end_turn(false);

        let created: Vec<&StepEvent> = rec
            .events()
            .iter()
            .filter(|event| event.event_type == StepEventType::StepCreated)
            .collect();
        assert_eq!(created.len(), 2);
        assert_ne!(
            created[0].step_id, created[1].step_id,
            "re-created visible turns need unique step ids for trace correlation"
        );
        for (idx, event) in created.iter().enumerate() {
            let trace_context = event
                .payload
                .as_ref()
                .and_then(|payload| payload.get("trace_context"))
                .expect("step events should carry trace context");
            assert_eq!(
                trace_context
                    .get("visible_turn")
                    .and_then(serde_json::Value::as_u64),
                Some(6)
            );
            assert_eq!(
                trace_context
                    .get("step_sequence")
                    .and_then(serde_json::Value::as_u64),
                Some(idx as u64)
            );
        }
    }

    #[test]
    fn regression_step_events_jsonl_satisfies_trace_invariants() {
        let mut rec = StepRecorder::new(TEST_USER_ID, "sess-regression", "task-1");
        rec.begin_turn(3);
        rec.record_plan(&["read_file".into()], 0.2, 4000);
        rec.begin_act(1);
        rec.begin_tool_with_key_and_args_preview(
            "read_file",
            "call-read-1",
            Some("sem:read:file.rs"),
            Some("path=file.rs start=1 end=20"),
        );
        rec.record_cache_hit_with_reason(
            "read_file",
            CachedToolResult {
                tool_name: "read_file".to_string(),
                output: "cached output".to_string(),
                is_error: false,
                cached_at: 42,
                context_signature: None,
            },
            "cached_cross_turn",
        );
        rec.end_turn(false);

        let jsonl = rec
            .events()
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .join("\n");
        let parsed = jsonl
            .lines()
            .map(serde_json::from_str::<StepEvent>)
            .collect::<Result<Vec<_>, _>>()
            .expect("events should parse as JSONL");

        let mut event_ids = std::collections::HashSet::new();
        let mut created_step_ids = std::collections::HashSet::new();
        for (idx, event) in parsed.iter().enumerate() {
            assert!(
                event_ids.insert(event.event_id.clone()),
                "event_id must be unique: {}",
                event.event_id
            );
            if idx == 0 {
                assert!(event.caused_by.is_empty());
            } else {
                assert!(
                    event
                        .caused_by
                        .iter()
                        .all(|parent| event_ids.contains(parent)),
                    "all causal parents must refer to earlier events: {:?}",
                    event.caused_by
                );
            }
            let trace_context = event
                .payload
                .as_ref()
                .and_then(|payload| payload.get("trace_context"))
                .expect("every trace event should carry trace_context");
            assert_eq!(
                trace_context
                    .get("visible_turn")
                    .and_then(serde_json::Value::as_u64),
                Some(3)
            );
            if event.event_type == StepEventType::StepCreated {
                assert!(
                    created_step_ids.insert(event.step_id.clone()),
                    "StepCreated step_id must be unique: {}",
                    event.step_id
                );
            }
            if event.event_type == StepEventType::ToolCallCompleted {
                let payload = event.payload.as_ref().unwrap();
                assert_eq!(
                    payload.get("reason").and_then(serde_json::Value::as_str),
                    Some("cached_cross_turn")
                );
                assert_eq!(
                    payload.get("cached").and_then(serde_json::Value::as_bool),
                    Some(true)
                );
                assert_eq!(
                    payload.get("output").and_then(serde_json::Value::as_str),
                    Some("cached output")
                );
            }
            assert_ne!(
                event.event_type,
                StepEventType::StepRetried,
                "StepRetried must not represent normal cross-round progression"
            );
        }
    }

    #[test]
    fn complete_without_begin_auto_injects_started_event() {
        // When a tool is completed WITHOUT a preceding begin_tool call
        // (e.g., fast-path tools dispatched through bridge where CLI
        // only sees the result), the recorder must auto-inject a
        // ToolCallStarted event so the event stream is always a
        // valid span: Started → Completed/Failed/Skipped.
        let mut rec = StepRecorder::new(TEST_USER_ID, "sess-started-fix", "task-1");
        rec.begin_turn(0);
        rec.begin_act(1);
        // Skip begin_tool — go straight to complete (simulates fast-path)
        rec.complete_tool_with_result_and_metadata(
            "read_file",
            "call-fast-1",
            Some("src/lib.rs"),
            false,
            5,
            false,
            "file contents here",
        );

        let events = rec.events();
        let tool_events: Vec<_> = events
            .iter()
            .filter(|e| {
                matches!(
                    e.event_type,
                    StepEventType::ToolCallStarted
                        | StepEventType::ToolCallCompleted
                        | StepEventType::ToolCallFailed
                        | StepEventType::ToolCallSkipped
                )
            })
            .collect();

        assert_eq!(
            tool_events.len(),
            2,
            "must have exactly Started + Completed: {:?}",
            tool_events
                .iter()
                .map(|e| &e.event_type)
                .collect::<Vec<_>>()
        );
        assert_eq!(tool_events[0].event_type, StepEventType::ToolCallStarted);
        assert_eq!(tool_events[1].event_type, StepEventType::ToolCallCompleted);

        // The auto-injected Started must carry the tool_name and call_id
        let started_payload = tool_events[0].payload.as_ref().unwrap();
        assert_eq!(
            started_payload.get("tool_name").and_then(|v| v.as_str()),
            Some("read_file")
        );
        assert_eq!(
            started_payload.get("call_id").and_then(|v| v.as_str()),
            Some("call-fast-1")
        );
    }

    #[test]
    fn begin_then_complete_does_not_double_emit_started() {
        // Normal path: begin_tool → complete_tool. Must emit exactly one
        // ToolCallStarted (from begin_tool), not a second from complete.
        let mut rec = StepRecorder::new(TEST_USER_ID, "sess-no-double", "task-1");
        rec.begin_turn(0);
        rec.begin_act(1);
        rec.begin_tool_with_key_and_args_preview("bash", "call-1", None, Some("ls"));
        rec.complete_tool_with_result_and_metadata(
            "bash",
            "call-1",
            Some("ls"),
            false,
            10,
            false,
            "output",
        );

        let started_count = rec
            .events()
            .iter()
            .filter(|e| e.event_type == StepEventType::ToolCallStarted)
            .count();
        assert_eq!(
            started_count, 1,
            "begin_tool + complete_tool must produce exactly 1 Started, not 2"
        );
    }

    #[test]
    fn parallel_completions_are_correlated_by_call_id() {
        let mut rec = StepRecorder::new(TEST_USER_ID, "sess-parallel", "task-1");
        rec.begin_turn(0);
        rec.begin_act(3);
        rec.begin_tool_with_key_and_args_preview("read_file", "call-a", None, Some("a.rs"));
        rec.begin_tool_with_key_and_args_preview("read_file", "call-b", None, Some("b.rs"));
        rec.begin_tool_with_key_and_args_preview("read_file", "call-c", None, Some("c.rs"));

        rec.complete_tool_with_result_and_metadata(
            "read_file",
            "call-a",
            Some("a.rs"),
            false,
            3,
            false,
            "a",
        );
        rec.complete_tool_with_result_and_metadata(
            "read_file",
            "call-b",
            Some("b.rs"),
            false,
            4,
            false,
            "b",
        );
        rec.complete_tool_with_result_and_metadata(
            "read_file",
            "call-c",
            Some("c.rs"),
            false,
            5,
            false,
            "c",
        );

        let completed: Vec<_> = rec
            .events()
            .iter()
            .filter(|event| event.event_type == StepEventType::ToolCallCompleted)
            .collect();
        assert_eq!(completed.len(), 3);

        for (idx, call_id, output) in [
            (0_u64, "call-a", "a"),
            (1_u64, "call-b", "b"),
            (2_u64, "call-c", "c"),
        ] {
            let payload = completed[idx as usize].payload.as_ref().unwrap();
            assert_eq!(
                payload
                    .get("slot_index")
                    .and_then(serde_json::Value::as_u64),
                Some(idx)
            );
            assert_eq!(
                payload.get("call_id").and_then(serde_json::Value::as_str),
                Some(call_id)
            );
            assert_eq!(
                payload.get("output").and_then(serde_json::Value::as_str),
                Some(output)
            );
        }
    }
}
