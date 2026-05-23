//! Issue #326 P6 / R2 Major 4: three-event audit trail.
//!
//! ## Why three events
//!
//! Plan v3 §P6 calls out that scenarios #47 / #48 / #50 require
//! the audit log to answer:
//!
//! - "what was evaluated and what did the engine decide?"
//! - "what did the user choose?"  (Allow / Reject / Always +
//!   which scope)
//! - "what got persisted, and to which file, and did the save
//!   succeed?"
//!
//! A single `PermissionEvaluatedEvent` only captures the first
//! question. R2 Major 4 specifically calls this out as a gap:
//! exporting the audit log shouldn't say "Yes, it was approved"
//! without saying "by user X, choosing Always-Project-User-Trusted,
//! saved to .kiro/permissions.json successfully".
//!
//! The three events:
//!
//! 1. [`PermissionEvaluatedEvent`] — fires every time the engine
//!    runs `evaluate_permission`. Payload includes the request,
//!    the trace, and the engine's decision.
//!
//! 2. [`ApprovalResolvedEvent`] — fires once per `NeedExternal`
//!    decision after the user (or fail-closed sink) responds.
//!    Payload includes which response variant was picked and
//!    which scope (if AlwaysAllow).
//!
//! 3. [`RulePersistedEvent`] — fires when a rule is written to
//!    `.kiro/permissions.json` or `~/.astra/permissions.json`.
//!    Payload includes the file, the new rule, and whether the
//!    save succeeded.
//!
//! ## Local ring buffer (default on)
//!
//! Plan v3 §P6 defaults the local ring buffer ON for all three
//! event types. Previously the design had Allow events
//! verbose-only; R1 Major 9 / R2 Critical 1 noted that the
//! `/permissions trace` view is useless without Allow events,
//! and "verbose only" is a UX trap. Default-on means the local
//! 1000-entry-per-type ring buffer always has fresh data when
//! the user invokes `/permissions trace --export`.
//!
//! Sensitive value redaction (path prefixes, secret-looking
//! values) is applied at export time, not at insert time, so the
//! ring buffer stores the full payload and the user can configure
//! redaction without losing history.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use crate::approval_request_key::ApprovalRequestKey;
use crate::approval_sink::ApprovalResponse;
use crate::permission::engine::{
    DecisionEnvelope, DecisionSource, HardDecision, RiskTag, RuleOrigin,
};
use crate::permission::match_target::AllowMatchTarget;

/// Global ring-buffer cap per event kind.
const RING_BUFFER_CAPACITY: usize = 1000;

/// Approval scope — what the user chose when they pressed
/// Always. Mirrors the AllowScope enum from plan v3 §P3.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AllowScope {
    /// One-shot approval; nothing persists.
    OnceThisCall,
    /// Auto-approve identical fingerprints for the rest of the
    /// current LLM round.
    RestOfTurn,
    /// Auto-approve identical fingerprints until the session ends.
    /// Per-fingerprint, NOT a global mode change.
    RestOfSession,
    /// Persist a rule to `.kiro/permissions.json` (project file).
    Project,
    /// Persist a rule to `~/.astra/permissions.json` (user file).
    User,
}

/// Where a persisted rule landed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PersistTarget {
    Project,
    User,
}

impl From<RuleOrigin> for Option<PersistTarget> {
    fn from(origin: RuleOrigin) -> Self {
        match origin {
            RuleOrigin::Project => Some(PersistTarget::Project),
            RuleOrigin::User => Some(PersistTarget::User),
            _ => None,
        }
    }
}

/// Event 1: every `evaluate_permission` invocation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PermissionEvaluatedEvent {
    /// UNIX-millis at the moment the engine returned.
    pub timestamp_ms: u64,
    /// Identifier for cross-event correlation; the resolved /
    /// persisted events that follow share this id.
    pub correlation_id: String,
    /// Request being evaluated.
    pub request_key: ApprovalRequestKey,
    /// Decision (`allow` / `deny` / `need_external`).
    pub decision: String,
    /// Which step of EVALUATION_ORDER produced the decision.
    pub source: SourceLabel,
    /// Risk tags computed during evaluation.
    pub risk_tags: Vec<RiskTag>,
    /// Whether the engine had to fall through to the prompt sink.
    pub need_external: bool,
}

/// JSON-friendly projection of [`DecisionSource`]. We don't
/// serialize the whole enum because some variants carry strings
/// (e.g. `DenyRule { rule, origin }`); the audit format wants a
/// stable shape.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SourceLabel {
    pub step: String,
    pub matched_rule: Option<String>,
    pub origin: Option<String>,
}

impl From<&DecisionSource> for SourceLabel {
    fn from(src: &DecisionSource) -> Self {
        match src {
            DecisionSource::SchemaIdentity => Self {
                step: "schema_identity".into(),
                matched_rule: None,
                origin: None,
            },
            DecisionSource::DenyRule { rule, origin } => Self {
                step: "deny_rule".into(),
                matched_rule: Some(rule.clone()),
                origin: Some(format!("{origin:?}").to_lowercase()),
            },
            DecisionSource::SafetyMiddleware { reason } => Self {
                step: "safety_middleware".into(),
                matched_rule: Some(reason.clone()),
                origin: None,
            },
            DecisionSource::GitSafety { violation } => Self {
                step: "git_safety".into(),
                matched_rule: Some(violation.clone()),
                origin: None,
            },
            DecisionSource::SensitivePath { path } => Self {
                step: "sensitive_path".into(),
                matched_rule: Some(path.clone()),
                origin: None,
            },
            DecisionSource::ExecuteHardDeny { reason } => Self {
                step: "execute_hard_deny".into(),
                matched_rule: Some(reason.clone()),
                origin: None,
            },
            DecisionSource::SandboxExpansion => Self {
                step: "sandbox_expansion".into(),
                matched_rule: None,
                origin: None,
            },
            DecisionSource::AskRule { rule, origin } => Self {
                step: "ask_rule".into(),
                matched_rule: Some(rule.clone()),
                origin: Some(format!("{origin:?}").to_lowercase()),
            },
            DecisionSource::ReadShortCircuit => Self {
                step: "read_short_circuit".into(),
                matched_rule: None,
                origin: None,
            },
            DecisionSource::SessionOverride { allowed } => Self {
                step: "session_override".into(),
                matched_rule: Some(format!("allowed={allowed}")),
                origin: None,
            },
            DecisionSource::ExplicitApprovalGate { reason } => Self {
                step: "explicit_approval".into(),
                matched_rule: Some(reason.clone()),
                origin: None,
            },
            DecisionSource::AllowRule { rule, origin } => Self {
                step: "allow_rule".into(),
                matched_rule: Some(rule.clone()),
                origin: Some(format!("{origin:?}").to_lowercase()),
            },
            DecisionSource::Mode { mode } => Self {
                step: "mode".into(),
                matched_rule: Some(mode.clone()),
                origin: None,
            },
            DecisionSource::UnmatchedFallback => Self {
                step: "unmatched_fallback".into(),
                matched_rule: None,
                origin: None,
            },
        }
    }
}

/// Event 2: how the user (or fail-closed sink) resolved the
/// `NeedExternal` from the engine. Fires at most once per
/// `Evaluated` event whose `need_external = true`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApprovalResolvedEvent {
    pub timestamp_ms: u64,
    pub correlation_id: String,
    pub request_key: ApprovalRequestKey,
    pub response: ApprovalResponse,
    /// `Some(_)` when the response was AlwaysAllow.
    pub scope: Option<AllowScope>,
    /// `Some(_)` when the user selected what future request the
    /// scoped approval should match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub match_target: Option<AllowMatchTarget>,
    /// True if the executor's pre-execute revalidation (P5f
    /// stale-check) confirmed the file/payload hadn't changed
    /// since approval. Always true for non-edit tools.
    pub stale_revalidation_passed: bool,
}

/// Event 3: rule-write attempt (project or user file). Fires
/// after every `add_allow_rule` / `PermissionSettings::modify`
/// regardless of success.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RulePersistedEvent {
    pub timestamp_ms: u64,
    pub correlation_id: String,
    pub target: PersistTarget,
    /// The rule string we tried to persist, in v2 grammar form.
    pub rule_text: String,
    /// True iff the on-disk save succeeded.
    pub saved: bool,
    /// Failure message when `saved = false`.
    pub failure_reason: Option<String>,
}

/// Combined event for export / `/permissions trace`. Each entry
/// is a tagged variant so the JSONL output is unambiguous.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PermissionAuditEvent {
    Evaluated(PermissionEvaluatedEvent),
    Resolved(ApprovalResolvedEvent),
    Persisted(RulePersistedEvent),
}

impl PermissionAuditEvent {
    #[must_use]
    pub fn correlation_id(&self) -> &str {
        match self {
            Self::Evaluated(e) => &e.correlation_id,
            Self::Resolved(e) => &e.correlation_id,
            Self::Persisted(e) => &e.correlation_id,
        }
    }

    #[must_use]
    pub fn timestamp_ms(&self) -> u64 {
        match self {
            Self::Evaluated(e) => e.timestamp_ms,
            Self::Resolved(e) => e.timestamp_ms,
            Self::Persisted(e) => e.timestamp_ms,
        }
    }
}

/// Process-wide ring buffer for permission audit events.
///
/// `RING_BUFFER_CAPACITY` entries per kind (3000 total). Default
/// on; the slash command `/permissions trace --export <file>`
/// drains the buffer to JSONL.
pub struct PermissionAuditRing {
    evaluated: VecDeque<PermissionEvaluatedEvent>,
    resolved: VecDeque<ApprovalResolvedEvent>,
    persisted: VecDeque<RulePersistedEvent>,
}

impl PermissionAuditRing {
    fn new() -> Self {
        Self {
            evaluated: VecDeque::with_capacity(RING_BUFFER_CAPACITY),
            resolved: VecDeque::with_capacity(RING_BUFFER_CAPACITY),
            persisted: VecDeque::with_capacity(RING_BUFFER_CAPACITY),
        }
    }

    pub fn push_evaluated(&mut self, event: PermissionEvaluatedEvent) {
        if self.evaluated.len() >= RING_BUFFER_CAPACITY {
            self.evaluated.pop_front();
        }
        self.evaluated.push_back(event);
    }

    pub fn push_resolved(&mut self, event: ApprovalResolvedEvent) {
        if self.resolved.len() >= RING_BUFFER_CAPACITY {
            self.resolved.pop_front();
        }
        self.resolved.push_back(event);
    }

    pub fn push_persisted(&mut self, event: RulePersistedEvent) {
        if self.persisted.len() >= RING_BUFFER_CAPACITY {
            self.persisted.pop_front();
        }
        self.persisted.push_back(event);
    }

    /// Drain into a single time-sorted JSONL stream.
    /// Used by `/permissions trace [--export]`.
    pub fn snapshot_jsonl(&self) -> Vec<PermissionAuditEvent> {
        let mut all: Vec<PermissionAuditEvent> = Vec::new();
        all.extend(
            self.evaluated
                .iter()
                .cloned()
                .map(PermissionAuditEvent::Evaluated),
        );
        all.extend(
            self.resolved
                .iter()
                .cloned()
                .map(PermissionAuditEvent::Resolved),
        );
        all.extend(
            self.persisted
                .iter()
                .cloned()
                .map(PermissionAuditEvent::Persisted),
        );
        all.sort_by_key(PermissionAuditEvent::timestamp_ms);
        all
    }

    /// Number of entries currently stored across all three rings.
    #[must_use]
    pub fn total_len(&self) -> usize {
        self.evaluated.len() + self.resolved.len() + self.persisted.len()
    }

    /// Per-kind counts for the status-line indicator.
    #[must_use]
    pub fn counts(&self) -> (usize, usize, usize) {
        (
            self.evaluated.len(),
            self.resolved.len(),
            self.persisted.len(),
        )
    }
}

static GLOBAL_RING: OnceLock<Mutex<PermissionAuditRing>> = OnceLock::new();

fn ring() -> &'static Mutex<PermissionAuditRing> {
    GLOBAL_RING.get_or_init(|| Mutex::new(PermissionAuditRing::new()))
}

/// Append an [`PermissionEvaluatedEvent`] to the global ring.
pub fn record_evaluated(event: PermissionEvaluatedEvent) {
    record_evaluated_for_session(None, event);
}

/// Append a [`PermissionEvaluatedEvent`] to the global ring and, when a
/// session id is available, to that session's durable JSONL journal.
pub fn record_evaluated_for_session(session_id: Option<&str>, event: PermissionEvaluatedEvent) {
    let journal_event = PermissionAuditEvent::Evaluated(event.clone());
    if let Ok(mut r) = ring().lock() {
        r.push_evaluated(event);
    }
    append_to_session_journal(session_id, &journal_event);
}

/// Append an [`ApprovalResolvedEvent`] to the global ring.
pub fn record_resolved(event: ApprovalResolvedEvent) {
    record_resolved_for_session(None, event);
}

/// Append an [`ApprovalResolvedEvent`] to the global ring and, when a
/// session id is available, to that session's durable JSONL journal.
pub fn record_resolved_for_session(session_id: Option<&str>, event: ApprovalResolvedEvent) {
    let journal_event = PermissionAuditEvent::Resolved(event.clone());
    if let Ok(mut r) = ring().lock() {
        r.push_resolved(event);
    }
    append_to_session_journal(session_id, &journal_event);
}

/// Append a [`RulePersistedEvent`] to the global ring.
pub fn record_persisted(event: RulePersistedEvent) {
    record_persisted_for_session(None, event);
}

/// Append a [`RulePersistedEvent`] to the global ring and, when a session id
/// is available, to that session's durable JSONL journal.
pub fn record_persisted_for_session(session_id: Option<&str>, event: RulePersistedEvent) {
    let journal_event = PermissionAuditEvent::Persisted(event.clone());
    if let Ok(mut r) = ring().lock() {
        r.push_persisted(event);
    }
    append_to_session_journal(session_id, &journal_event);
}

/// Convenience wrapper for call sites that already have a
/// [`DecisionEnvelope`]. Keeps CLI/runtime evaluated-event wiring consistent.
pub fn record_evaluated_envelope(
    tool_name: &str,
    args: &serde_json::Value,
    envelope: &DecisionEnvelope,
    correlation_prefix: &str,
    source_agent: Option<String>,
) {
    record_evaluated_envelope_for_session(
        None,
        tool_name,
        args,
        envelope,
        correlation_prefix,
        source_agent,
    );
}

/// Session-aware variant of [`record_evaluated_envelope`].
pub fn record_evaluated_envelope_for_session(
    session_id: Option<&str>,
    tool_name: &str,
    args: &serde_json::Value,
    envelope: &DecisionEnvelope,
    correlation_prefix: &str,
    source_agent: Option<String>,
) {
    let timestamp_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let cwd = std::env::current_dir().unwrap_or_default();
    let request_key = ApprovalRequestKey::new(
        tool_name.to_string(),
        cwd,
        args,
        source_agent,
        uuid::Uuid::nil(),
    );
    let decision = match &envelope.decision {
        HardDecision::Allow => "allow",
        HardDecision::Deny { .. } => "deny",
        HardDecision::NeedExternal { .. } => "need_external",
    }
    .to_string();
    record_evaluated_for_session(
        session_id,
        PermissionEvaluatedEvent {
            timestamp_ms,
            correlation_id: format!("{correlation_prefix}-{timestamp_ms}-{tool_name}"),
            request_key,
            decision,
            source: (&envelope.source).into(),
            risk_tags: envelope.risk_tags.clone(),
            need_external: matches!(&envelope.decision, HardDecision::NeedExternal { .. }),
        },
    );
}

fn append_to_session_journal(session_id: Option<&str>, event: &PermissionAuditEvent) {
    let Some(session_id) = session_id.filter(|value| !value.trim().is_empty()) else {
        return;
    };
    let Ok(payload) = serde_json::to_value(event) else {
        tracing::warn!("permission_audit: failed to serialize audit event for journal");
        return;
    };
    let journal_event = astra_services::session_journal::JournalEvent::permission_audit(
        Some(session_id),
        None,
        payload,
    );
    match astra_services::session_journal::JournalWriter::new(session_id) {
        Ok(writer) => {
            if let Err(error) = writer.append(&journal_event) {
                tracing::warn!(
                    session_id,
                    correlation_id = event.correlation_id(),
                    error = %error,
                    "permission_audit: failed to append session journal event"
                );
            }
        }
        Err(error) => {
            tracing::warn!(
                session_id,
                correlation_id = event.correlation_id(),
                error = %error,
                "permission_audit: failed to open session journal"
            );
        }
    }
}

/// Snapshot the global ring as time-sorted JSONL events.
#[must_use]
pub fn snapshot() -> Vec<PermissionAuditEvent> {
    ring()
        .lock()
        .map(|r| r.snapshot_jsonl())
        .unwrap_or_default()
}

/// Snapshot as redacted JSONL lines suitable for writing to disk.
/// Export redaction happens here, not at insert time: the in-memory
/// ring keeps full fidelity for local diagnostics, while persisted
/// exports remove cwd path prefixes and credential-looking text.
#[must_use]
pub fn snapshot_redacted_jsonl_lines() -> Vec<String> {
    snapshot()
        .into_iter()
        .map(redact_event_for_export)
        .filter_map(|event| serde_json::to_string(&event).ok())
        .collect()
}

/// Current ring-buffer counts: `(evaluated, resolved, persisted)`.
#[must_use]
pub fn counts() -> (usize, usize, usize) {
    ring().lock().map(|r| r.counts()).unwrap_or_default()
}

/// Human-readable trace lines for CLI/TUI surfaces.
#[must_use]
pub fn format_snapshot_lines(limit: usize) -> Vec<String> {
    let events = snapshot();
    if events.is_empty() {
        return vec!["No permission audit events recorded yet.".to_string()];
    }

    let total = events.len();
    let start = total.saturating_sub(limit.max(1));
    let mut lines = Vec::with_capacity(total - start + 1);
    lines.push(format!(
        "Permission audit trace: showing {} of {} events",
        total - start,
        total
    ));
    for event in events.into_iter().skip(start) {
        lines.push(format_audit_event(&event));
    }
    lines
}

fn format_audit_event(event: &PermissionAuditEvent) -> String {
    match event {
        PermissionAuditEvent::Evaluated(e) => {
            let risks = if e.risk_tags.is_empty() {
                "none".to_string()
            } else {
                e.risk_tags
                    .iter()
                    .map(|tag| format!("{tag:?}"))
                    .collect::<Vec<_>>()
                    .join(",")
            };
            format!(
                "{} evaluated {} -> {} via {} risks={}",
                e.timestamp_ms, e.request_key.tool, e.decision, e.source.step, risks
            )
        }
        PermissionAuditEvent::Resolved(e) => {
            let scope = e
                .scope
                .map(|s| format!("{s:?}"))
                .unwrap_or_else(|| "none".to_string());
            let match_target = e
                .match_target
                .as_ref()
                .map(|target| format!("{target:?}"))
                .unwrap_or_else(|| "none".to_string());
            format!(
                "{} resolved {} -> {:?} scope={} match_target={} stale_ok={}",
                e.timestamp_ms,
                e.request_key.tool,
                e.response,
                scope,
                match_target,
                e.stale_revalidation_passed
            )
        }
        PermissionAuditEvent::Persisted(e) => {
            let status = if e.saved {
                "saved".to_string()
            } else {
                format!(
                    "failed:{}",
                    e.failure_reason.as_deref().unwrap_or("unknown")
                )
            };
            format!(
                "{} persisted {:?} {} -> {}",
                e.timestamp_ms, e.target, e.rule_text, status
            )
        }
    }
}

fn redact_text_for_export(text: String) -> String {
    crate::safety_middleware::redact_credentials_in_text(&text).0
}

fn redact_request_key_for_export(mut key: ApprovalRequestKey) -> ApprovalRequestKey {
    key.canonical_cwd = PathBuf::from("<redacted-cwd>");
    key.source_agent = key.source_agent.map(redact_text_for_export);
    key
}

fn redact_source_label_for_export(mut source: SourceLabel) -> SourceLabel {
    source.matched_rule = source.matched_rule.map(redact_text_for_export);
    source.origin = source.origin.map(redact_text_for_export);
    source
}

fn redact_event_for_export(event: PermissionAuditEvent) -> PermissionAuditEvent {
    match event {
        PermissionAuditEvent::Evaluated(mut e) => {
            e.request_key = redact_request_key_for_export(e.request_key);
            e.source = redact_source_label_for_export(e.source);
            PermissionAuditEvent::Evaluated(e)
        }
        PermissionAuditEvent::Resolved(mut e) => {
            e.request_key = redact_request_key_for_export(e.request_key);
            e.match_target = e.match_target.map(redact_match_target_for_export);
            PermissionAuditEvent::Resolved(e)
        }
        PermissionAuditEvent::Persisted(mut e) => {
            e.rule_text = redact_text_for_export(e.rule_text);
            e.failure_reason = e.failure_reason.map(redact_text_for_export);
            PermissionAuditEvent::Persisted(e)
        }
    }
}

fn redact_match_target_for_export(target: AllowMatchTarget) -> AllowMatchTarget {
    match target {
        AllowMatchTarget::Prefix(prefix) => {
            AllowMatchTarget::Prefix(redact_text_for_export(prefix))
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval_request_key::ApprovalRequestKey;
    use uuid::Uuid;

    fn fixture_request() -> ApprovalRequestKey {
        ApprovalRequestKey {
            tool: "bash".to_string(),
            canonical_cwd: std::env::temp_dir(),
            args_hash: [0; 32],
            payload_hash: None,
            source_agent: None,
            turn_id: Uuid::nil(),
        }
    }

    #[test]
    fn ring_caps_at_capacity() {
        let mut r = PermissionAuditRing::new();
        for i in 0..(RING_BUFFER_CAPACITY + 5) {
            r.push_evaluated(PermissionEvaluatedEvent {
                timestamp_ms: i as u64,
                correlation_id: format!("c-{i}"),
                request_key: fixture_request(),
                decision: "allow".into(),
                source: SourceLabel {
                    step: "mode".into(),
                    matched_rule: None,
                    origin: None,
                },
                risk_tags: vec![],
                need_external: false,
            });
        }
        let (e, _, _) = r.counts();
        assert_eq!(e, RING_BUFFER_CAPACITY);
        // Oldest entries (0-4) should have been evicted.
        assert_eq!(r.evaluated.front().unwrap().timestamp_ms, 5);
    }

    #[test]
    fn snapshot_sorts_across_kinds_by_timestamp() {
        let mut r = PermissionAuditRing::new();
        r.push_evaluated(PermissionEvaluatedEvent {
            timestamp_ms: 200,
            correlation_id: "c-1".into(),
            request_key: fixture_request(),
            decision: "need_external".into(),
            source: SourceLabel {
                step: "mode".into(),
                matched_rule: None,
                origin: None,
            },
            risk_tags: vec![],
            need_external: true,
        });
        r.push_resolved(ApprovalResolvedEvent {
            timestamp_ms: 300,
            correlation_id: "c-1".into(),
            request_key: fixture_request(),
            response: ApprovalResponse::AllowOnce,
            scope: Some(AllowScope::OnceThisCall),
            match_target: Some(AllowMatchTarget::Exact),
            stale_revalidation_passed: true,
        });
        r.push_persisted(RulePersistedEvent {
            timestamp_ms: 100,
            correlation_id: "c-0".into(),
            target: PersistTarget::Project,
            rule_text: "Bash(npm test:*)".into(),
            saved: true,
            failure_reason: None,
        });

        let events = r.snapshot_jsonl();
        let timestamps: Vec<u64> = events
            .iter()
            .map(PermissionAuditEvent::timestamp_ms)
            .collect();
        assert_eq!(timestamps, vec![100, 200, 300]);
    }

    #[test]
    fn jsonl_export_is_self_describing() {
        let mut r = PermissionAuditRing::new();
        r.push_evaluated(PermissionEvaluatedEvent {
            timestamp_ms: 1,
            correlation_id: "c".into(),
            request_key: fixture_request(),
            decision: "allow".into(),
            source: SourceLabel {
                step: "mode".into(),
                matched_rule: None,
                origin: None,
            },
            risk_tags: vec![RiskTag::BashExecute],
            need_external: false,
        });
        let line = serde_json::to_string(&r.snapshot_jsonl().pop().unwrap()).unwrap();
        // The kind tag must be present so the consumer can route
        // events without inspecting fields.
        assert!(line.contains("\"kind\":\"evaluated\""));
        assert!(line.contains("\"decision\":\"allow\""));
    }

    #[test]
    fn resolved_event_serializes_scope_and_match_target() {
        let event = PermissionAuditEvent::Resolved(ApprovalResolvedEvent {
            timestamp_ms: 2,
            correlation_id: "c-resolved".into(),
            request_key: fixture_request(),
            response: ApprovalResponse::AlwaysAllow,
            scope: Some(AllowScope::RestOfSession),
            match_target: Some(AllowMatchTarget::Tool),
            stale_revalidation_passed: true,
        });

        let line = serde_json::to_string(&event).unwrap();
        assert!(line.contains("\"kind\":\"resolved\""));
        assert!(line.contains("\"scope\":\"rest_of_session\""));
        assert!(line.contains("\"match_target\""));
        assert!(line.contains("\"kind\":\"tool\""));
    }

    #[test]
    fn session_journal_receives_permission_audit_event() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(tmp.path());
        let session_id = format!("perm-audit-{}", Uuid::new_v4());

        record_evaluated_for_session(
            Some(&session_id),
            PermissionEvaluatedEvent {
                timestamp_ms: 1,
                correlation_id: "c-journal".into(),
                request_key: fixture_request(),
                decision: "need_external".into(),
                source: SourceLabel {
                    step: "mode".into(),
                    matched_rule: Some("prompt".into()),
                    origin: None,
                },
                risk_tags: vec![],
                need_external: true,
            },
        );

        let events = astra_services::session_journal::read_journal(&session_id).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].event_type,
            astra_services::session_journal::JournalEventType::PermissionAudit
        );
        let metadata = events[0].metadata.as_ref().expect("metadata");
        assert_eq!(
            metadata.get("kind").and_then(serde_json::Value::as_str),
            Some("evaluated")
        );
        assert_eq!(
            metadata
                .get("correlation_id")
                .and_then(serde_json::Value::as_str),
            Some("c-journal")
        );
        assert_eq!(
            metadata.get("decision").and_then(serde_json::Value::as_str),
            Some("need_external")
        );
    }

    #[test]
    fn redacted_export_removes_cwd_and_secret_text() {
        let event = PermissionAuditEvent::Evaluated(PermissionEvaluatedEvent {
            timestamp_ms: 1,
            correlation_id: "c".into(),
            request_key: ApprovalRequestKey {
                tool: "bash".to_string(),
                canonical_cwd: PathBuf::from("/Users/alice/private/project"),
                args_hash: [0; 32],
                payload_hash: None,
                source_agent: Some("agent OPENAI_API_KEY=sk-1234567890abcdef".to_string()),
                turn_id: Uuid::nil(),
            },
            decision: "deny".into(),
            source: SourceLabel {
                step: "deny_rule".into(),
                matched_rule: Some("OPENAI_API_KEY=sk-1234567890abcdef".into()),
                origin: Some("project".into()),
            },
            risk_tags: vec![RiskTag::CredentialAccess],
            need_external: false,
        });

        let line = serde_json::to_string(&redact_event_for_export(event)).unwrap();
        assert!(!line.contains("/Users/alice/private/project"));
        assert!(!line.contains("sk-1234567890abcdef"));
        assert!(line.contains("<redacted-cwd>"));
    }

    #[test]
    fn audit_event_formatter_is_compact() {
        let line = format_audit_event(&PermissionAuditEvent::Evaluated(PermissionEvaluatedEvent {
            timestamp_ms: 1,
            correlation_id: "c".into(),
            request_key: fixture_request(),
            decision: "allow".into(),
            source: SourceLabel {
                step: "mode".into(),
                matched_rule: None,
                origin: None,
            },
            risk_tags: vec![RiskTag::BashExecute],
            need_external: false,
        }));
        assert_eq!(line, "1 evaluated bash -> allow via mode risks=BashExecute");
    }

    #[test]
    fn allow_scope_serializes_snake_case() {
        let s = serde_json::to_string(&AllowScope::RestOfSession).unwrap();
        assert_eq!(s, "\"rest_of_session\"");
    }

    #[test]
    fn persist_target_from_rule_origin() {
        assert_eq!(
            <RuleOrigin as Into<Option<PersistTarget>>>::into(RuleOrigin::Project),
            Some(PersistTarget::Project)
        );
        assert_eq!(
            <RuleOrigin as Into<Option<PersistTarget>>>::into(RuleOrigin::User),
            Some(PersistTarget::User)
        );
        assert_eq!(
            <RuleOrigin as Into<Option<PersistTarget>>>::into(RuleOrigin::Session),
            None
        );
    }
}
