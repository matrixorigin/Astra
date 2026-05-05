//! Pipeline journal events — structured records for observability and audit.
//!
//! These events are emitted after each turn and flow into the session journal
//! for persistence, cloud sync, and post-hoc analysis.

use serde::{Deserialize, Serialize};

use crate::context_feedback::ContextFeedback;
use crate::trace_alert::TraceAlert;

/// Discriminator for pipeline journal events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PipelineEventKind {
    /// Per-turn feedback snapshot (cache ratio, tokens, tier).
    Feedback,
    /// A trace alert fired (cache break, recovery loop, etc.).
    Alert,
    /// Compaction operation audit (what was dropped/cleared).
    CompactionAudit,
}

/// A structured pipeline journal event.
///
/// Serialized into the session journal's `metadata` field when emitted
/// as a `JournalEventType::ContextAssemblyRecorded` (or similar) event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineJournalEvent {
    pub kind: PipelineEventKind,
    pub turn: u32,

    // Feedback fields
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_hit_ratio: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,

    // Alert fields
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alert_rule: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alert_severity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alert_message: Option<String>,

    // Compaction audit fields
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compaction_strategy: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items_affected: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_freed: Option<u32>,
}

impl PipelineJournalEvent {
    /// Create a feedback event from API response metrics.
    #[must_use]
    pub fn from_feedback(turn: u32, model_id: &str, feedback: &ContextFeedback) -> Self {
        Self {
            kind: PipelineEventKind::Feedback,
            turn,
            cache_hit_ratio: Some(feedback.cache_hit_ratio),
            prompt_tokens: Some(feedback.tokens.prompt),
            completion_tokens: Some(feedback.tokens.completion),
            model_id: Some(model_id.to_string()),
            alert_rule: None,
            alert_severity: None,
            alert_message: None,
            compaction_strategy: None,
            items_affected: None,
            tokens_freed: None,
        }
    }

    /// Create an alert event from a trace alert.
    #[must_use]
    pub fn from_alert(alert: &TraceAlert) -> Self {
        Self {
            kind: PipelineEventKind::Alert,
            turn: alert.turn,
            cache_hit_ratio: None,
            prompt_tokens: None,
            completion_tokens: None,
            model_id: None,
            alert_rule: Some(alert.rule.clone()),
            alert_severity: Some(format!("{:?}", alert.severity)),
            alert_message: Some(alert.message.clone()),
            compaction_strategy: None,
            items_affected: None,
            tokens_freed: None,
        }
    }

    /// Create a compaction audit event.
    #[must_use]
    pub fn compaction_audit(
        turn: u32,
        strategy: &str,
        items_affected: u32,
        tokens_freed: u32,
    ) -> Self {
        Self {
            kind: PipelineEventKind::CompactionAudit,
            turn,
            cache_hit_ratio: None,
            prompt_tokens: None,
            completion_tokens: None,
            model_id: None,
            alert_rule: None,
            alert_severity: None,
            alert_message: None,
            compaction_strategy: Some(strategy.to_string()),
            items_affected: Some(items_affected),
            tokens_freed: Some(tokens_freed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feedback_event_fields() {
        let fb = ContextFeedback::from_usage(1000, 800, 200, 500, false);
        let evt = PipelineJournalEvent::from_feedback(3, "claude", &fb);
        assert_eq!(evt.kind, PipelineEventKind::Feedback);
        assert_eq!(evt.turn, 3);
        assert!((evt.cache_hit_ratio.unwrap() - 0.8).abs() < 1e-9);
        assert_eq!(evt.completion_tokens, Some(500));
        assert_eq!(evt.model_id.as_deref(), Some("claude"));
    }

    #[test]
    fn alert_event_fields() {
        use crate::trace_alert::AlertSeverity;
        let alert = TraceAlert {
            severity: AlertSeverity::Warning,
            rule: "cache_cold_start".into(),
            message: "Cache hit 0% on turn 2".into(),
            turn: 2,
        };
        let evt = PipelineJournalEvent::from_alert(&alert);
        assert_eq!(evt.kind, PipelineEventKind::Alert);
        assert_eq!(evt.alert_rule.as_deref(), Some("cache_cold_start"));
        assert_eq!(evt.alert_severity.as_deref(), Some("Warning"));
    }

    #[test]
    fn compaction_audit_fields() {
        let evt = PipelineJournalEvent::compaction_audit(5, "schema_prune", 8, 1200);
        assert_eq!(evt.kind, PipelineEventKind::CompactionAudit);
        assert_eq!(evt.items_affected, Some(8));
        assert_eq!(evt.tokens_freed, Some(1200));
    }

    #[test]
    fn serde_roundtrip() {
        let evt = PipelineJournalEvent::compaction_audit(1, "round_dropping", 4, 5000);
        let json = serde_json::to_string(&evt).unwrap();
        let restored: PipelineJournalEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.kind, PipelineEventKind::CompactionAudit);
        assert_eq!(restored.tokens_freed, Some(5000));
    }
}
