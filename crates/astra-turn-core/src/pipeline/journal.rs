//! Pipeline journal events — structured records for observability and audit.
//!
//! These events are emitted after each turn and flow into the session journal
//! for persistence, cloud sync, and post-hoc analysis.

use serde::{Deserialize, Serialize};

use crate::compaction_types::CompactionTier;
use crate::context_feedback::ContextFeedback;
use crate::context_pipeline::PipelineRunMetrics;
use crate::trace_alert::TraceAlert;

/// Discriminator for pipeline journal events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PipelineEventKind {
    /// Plan-phase pressure/tier decision, emitted before the LLM call.
    Metrics,
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

    // Metrics fields (plan-phase, pre-call)
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub raw_pressure: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub predictive_pressure: Option<f64>,
    /// Typed tier, serialized in `CompactionTier`'s serde form
    /// (`snake_case`). Kept typed end-to-end so renaming or adding a
    /// variant is a loud deserialization failure downstream, never a
    /// silently mislabeled row.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub tier: Option<CompactionTier>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub spilled: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub output_reserve_tokens: Option<u32>,
    /// Where the reserve came from this call: `memory`, `journal`, or `cold`
    /// (see `overlay_session_reserves` in the bridge). Diagnostic only.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub reserve_source: Option<String>,

    // Feedback fields
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_hit_ratio: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u64>,
    /// Prompt tokens not served from cache (`prompt - cache_read`), the
    /// "fresh/miss" quantity used in billed-cost decomposition.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub fresh_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_break_reason: Option<String>,
    /// Cumulative API responses recorded for this session (explicit counter).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub api_calls_total: Option<u64>,

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
    /// Create a metrics event from a turn's plan-phase pipeline output.
    ///
    /// Emitted before the LLM call (unlike feedback/alerts, which follow the
    /// response), so a session's metrics events — read in journal order —
    /// give a real call-by-call pressure/tier trajectory even though the
    /// bridge's own `turn` numbering is coarser than one row per call.
    #[must_use]
    pub fn from_metrics(metrics: &PipelineRunMetrics, reserve_source: &str) -> Self {
        Self {
            kind: PipelineEventKind::Metrics,
            turn: metrics.turn_index,
            raw_pressure: Some(metrics.raw_pressure),
            predictive_pressure: Some(metrics.predictive_pressure),
            tier: Some(metrics.compact_tier),
            spilled: Some(metrics.spilled),
            output_reserve_tokens: Some(metrics.output_reserve_tokens),
            reserve_source: Some(reserve_source.to_string()),
            cache_hit_ratio: None,
            prompt_tokens: None,
            fresh_tokens: None,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            completion_tokens: None,
            model_id: None,
            cache_break_reason: None,
            api_calls_total: None,
            alert_rule: None,
            alert_severity: None,
            alert_message: None,
            compaction_strategy: None,
            items_affected: None,
            tokens_freed: None,
        }
    }

    /// Create a feedback event from API response metrics.
    #[must_use]
    pub fn from_feedback(turn: u32, model_id: &str, feedback: &ContextFeedback) -> Self {
        Self {
            kind: PipelineEventKind::Feedback,
            turn,
            raw_pressure: None,
            predictive_pressure: None,
            tier: None,
            spilled: None,
            output_reserve_tokens: None,
            reserve_source: None,
            cache_hit_ratio: Some(feedback.cache_hit_ratio),
            prompt_tokens: Some(feedback.tokens.prompt),
            fresh_tokens: Some(
                feedback
                    .tokens
                    .prompt
                    .saturating_sub(feedback.tokens.cache_read),
            ),
            cache_read_tokens: Some(feedback.tokens.cache_read),
            cache_creation_tokens: Some(feedback.tokens.cache_creation),
            completion_tokens: Some(feedback.tokens.completion),
            model_id: Some(model_id.to_string()),
            cache_break_reason: feedback
                .cache_break_detected
                .as_ref()
                .map(ToString::to_string),
            api_calls_total: None,
            alert_rule: None,
            alert_severity: None,
            alert_message: None,
            compaction_strategy: None,
            items_affected: None,
            tokens_freed: None,
        }
    }

    /// Attach the session's cumulative API-call counter to a feedback event.
    #[must_use]
    pub fn with_api_calls_total(mut self, api_calls_total: u64) -> Self {
        self.api_calls_total = Some(api_calls_total);
        self
    }

    /// Create an alert event from a trace alert.
    #[must_use]
    pub fn from_alert(alert: &TraceAlert) -> Self {
        Self {
            kind: PipelineEventKind::Alert,
            turn: alert.turn,
            raw_pressure: None,
            predictive_pressure: None,
            tier: None,
            spilled: None,
            output_reserve_tokens: None,
            reserve_source: None,
            cache_hit_ratio: None,
            prompt_tokens: None,
            fresh_tokens: None,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            completion_tokens: None,
            model_id: None,
            cache_break_reason: None,
            api_calls_total: None,
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
            raw_pressure: None,
            predictive_pressure: None,
            tier: None,
            spilled: None,
            output_reserve_tokens: None,
            reserve_source: None,
            cache_hit_ratio: None,
            prompt_tokens: None,
            fresh_tokens: None,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            completion_tokens: None,
            model_id: None,
            cache_break_reason: None,
            api_calls_total: None,
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
    fn metrics_event_fields() {
        use crate::compaction_types::CompactionTier;
        let metrics = PipelineRunMetrics {
            turn_index: 12,
            input_tokens: 34_934,
            output_reserve_tokens: 4_000,
            raw_pressure: 0.2665,
            predictive_pressure: 0.2985,
            compact_tier: CompactionTier::TrimSchemas,
            sections: 5,
            messages: 82,
            tool_schemas: 12,
            cache_markers: 2,
            tokens_cleared: 0,
            avg_cache_hit_ratio: 0.94,
            spilled: 1,
            api_calls_total: 38,
        };
        let evt = PipelineJournalEvent::from_metrics(&metrics, "memory");
        assert_eq!(evt.kind, PipelineEventKind::Metrics);
        assert_eq!(evt.turn, 12);
        assert!((evt.raw_pressure.unwrap() - 0.2665).abs() < 1e-9);
        assert!((evt.predictive_pressure.unwrap() - 0.2985).abs() < 1e-9);
        assert_eq!(evt.tier, Some(CompactionTier::TrimSchemas));
        assert_eq!(evt.spilled, Some(1));
        assert_eq!(evt.output_reserve_tokens, Some(4_000));
        assert_eq!(evt.reserve_source.as_deref(), Some("memory"));
        assert_eq!(evt.cache_hit_ratio, None, "metrics precede feedback");
    }

    #[test]
    fn feedback_event_fields() {
        let fb = ContextFeedback::from_usage(1000, 800, 200, 500, false);
        let evt = PipelineJournalEvent::from_feedback(3, "claude", &fb);
        assert_eq!(evt.kind, PipelineEventKind::Feedback);
        assert_eq!(evt.turn, 3);
        assert!((evt.cache_hit_ratio.unwrap() - 0.4).abs() < 1e-9);
        assert_eq!(evt.cache_read_tokens, Some(800));
        assert_eq!(evt.cache_creation_tokens, Some(200));
        assert_eq!(evt.completion_tokens, Some(500));
        assert_eq!(evt.model_id.as_deref(), Some("claude"));
        assert_eq!(
            evt.fresh_tokens,
            Some(200),
            "fresh = prompt(1000) - cache_read(800)"
        );
        assert_eq!(evt.api_calls_total, None);
    }

    #[test]
    fn feedback_event_carries_api_call_counter() {
        let fb = ContextFeedback::from_usage(1000, 800, 200, 500, false);
        let evt = PipelineJournalEvent::from_feedback(3, "claude", &fb).with_api_calls_total(42);
        assert_eq!(evt.api_calls_total, Some(42));
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
