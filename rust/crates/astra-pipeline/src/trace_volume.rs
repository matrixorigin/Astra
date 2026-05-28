//! Trace data volume estimation and projection.
//!
//! Addresses: dim 1 (总数据量) and dim 4 (非verbose数据量).
//! Estimates per-event byte sizes and projects total volume at different verbosity levels.

use crate::event::{EventKind, EventLog, TraceLevel};

/// Estimated byte size for each event kind.
/// Conservative estimates based on typical field sizes in JSON serialization.
impl EventKind {
    pub fn estimated_bytes(&self) -> usize {
        match self {
            // Small events: ~80-200 bytes
            EventKind::GuardEvaluated { .. } => 140,
            EventKind::ProgressRecorded { .. } => 80,
            EventKind::BudgetUpdate { .. } => 90,
            EventKind::StallDetected { .. } => 100,

            // Medium events: ~200-500 bytes
            EventKind::PhaseTransition { .. } => 200,
            EventKind::IntentDetected { .. } => 250,
            EventKind::EntityExtracted { .. } => 300,
            EventKind::ToolsSelected { .. } => 250,
            EventKind::BudgetSet { .. } => 200,
            EventKind::ToolCallStarted { .. } => 280,
            EventKind::ToolCallCompleted { .. } => 350,
            EventKind::MemoryQuery { .. } => 250,
            EventKind::MemoryRetrieved { .. } => 350,
            EventKind::SkillStarted { .. } => 250,
            EventKind::SkillCompleted { .. } => 300,
            EventKind::PromptAssembled { .. } => 400,
            EventKind::TurnCompleted { .. } => 250,
            EventKind::BudgetExpanded { .. } => 200,
            EventKind::CircuitBreakerTripped { .. } => 200,

            // Large events: 1-10 KB
            EventKind::ThinkingChunk { .. } => 1_200,
            EventKind::LlmChunk { .. } => 800,
            EventKind::LlmRequest { .. } => 5_000,
            EventKind::ToolCallOutput { .. } => 4_000,
            EventKind::ReflectionGenerated { .. } => 1_500,
        }
    }

    /// Whether this event is emitted in non-verbose mode.
    pub fn is_verbose_only(&self) -> bool {
        matches!(self.default_level(), TraceLevel::Debug | TraceLevel::Trace)
    }

    /// Whether this event contains potentially large payload data.
    pub fn is_high_volume(&self) -> bool {
        matches!(
            self,
            EventKind::LlmRequest { .. }
                | EventKind::ToolCallOutput { .. }
                | EventKind::ThinkingChunk { .. }
                | EventKind::ReflectionGenerated { .. }
        )
    }
}

/// Volume projection for a turn at different verbosity levels.
#[derive(Debug, Clone, serde::Serialize)]
pub struct VolumeProjection {
    pub level: &'static str,
    pub total_bytes: usize,
    pub event_count: usize,
    pub filtered_out: usize,
    pub high_volume_count: usize,
    pub human_size: String,
}

/// Estimator for trace data volume.
pub struct TraceVolume;

impl TraceVolume {
    /// Project total volume for an EventLog snapshot at different TraceLevel settings.
    pub fn project(event_log: &EventLog) -> Vec<VolumeProjection> {
        let levels = [
            (TraceLevel::Error, "Error"),
            (TraceLevel::Warn, "Warn"),
            (TraceLevel::Info, "Info (non-verbose)"),
            (TraceLevel::Debug, "Debug"),
            (TraceLevel::Trace, "Trace (all)"),
        ];

        levels
            .iter()
            .map(|(level, name)| {
                let mut total_bytes = 0usize;
                let mut event_count = 0usize;
                let mut filtered_out = 0usize;
                let mut high_volume_count = 0usize;

                for event in event_log.events() {
                    let ev_level = event.kind.default_level();
                    if ev_level as u8 <= *level as u8 {
                        let bytes = event.kind.estimated_bytes();
                        total_bytes += bytes;
                        event_count += 1;
                        if event.kind.is_high_volume() {
                            high_volume_count += 1;
                        }
                    } else {
                        filtered_out += 1;
                    }
                }

                VolumeProjection {
                    level: name,
                    total_bytes,
                    event_count,
                    filtered_out,
                    high_volume_count,
                    human_size: human_size(total_bytes),
                }
            })
            .collect()
    }

    /// Quick summary: total events, estimated bytes, high-volume events.
    pub fn summary(event_log: &EventLog) -> serde_json::Value {
        let all = event_log.events();
        let mut total_bytes = 0usize;
        let mut verbose_only = 0usize;
        let mut high_volume = 0usize;
        let mut by_level = std::collections::HashMap::new();

        for event in all {
            let bytes = event.kind.estimated_bytes();
            total_bytes += bytes;
            if event.kind.is_verbose_only() {
                verbose_only += 1;
            }
            if event.kind.is_high_volume() {
                high_volume += 1;
            }
            *by_level
                .entry(format!("{:?}", event.kind.default_level()))
                .or_insert(0usize) += 1;
        }

        serde_json::json!({
            "total_events": all.len(),
            "estimated_total_bytes": total_bytes,
            "estimated_human": human_size(total_bytes),
            "verbose_only_events": verbose_only,
            "high_volume_events": high_volume,
            "events_by_level": by_level,
        })
    }

    /// Non-verbose volume: exclude Debug + Trace events.
    pub fn non_verbose_estimate(event_log: &EventLog) -> serde_json::Value {
        let all = event_log.events();
        let mut verbose_bytes = 0usize;
        let mut non_verbose_bytes = 0usize;
        let mut verbose_count = 0usize;
        let mut non_verbose_count = 0usize;

        for event in all {
            let bytes = event.kind.estimated_bytes();
            if event.kind.is_verbose_only() {
                verbose_bytes += bytes;
                verbose_count += 1;
            } else {
                non_verbose_bytes += bytes;
                non_verbose_count += 1;
            }
        }

        let total = verbose_bytes + non_verbose_bytes;
        let savings_pct = if total > 0 {
            (verbose_bytes as f64 / total as f64) * 100.0
        } else {
            0.0
        };

        serde_json::json!({
            "non_verbose_events": non_verbose_count,
            "non_verbose_bytes": non_verbose_bytes,
            "non_verbose_human": human_size(non_verbose_bytes),
            "verbose_events": verbose_count,
            "verbose_bytes": verbose_bytes,
            "verbose_human": human_size(verbose_bytes),
            "savings_percent": format!("{:.1}%", savings_pct),
            "note": "Non-verbose = excluding Debug + Trace events (thinking chunks, detailed LLM traces, etc.)",
        })
    }

    /// Edge vs Cloud comparison: Layer-wise volume breakdown.
    pub fn layer_breakdown(
        event_log: &EventLog,
        step_store_event_count: usize,
        step_store_estimated_bytes: usize,
    ) -> serde_json::Value {
        let log_bytes: usize = event_log.events().iter().map(|e| e.kind.estimated_bytes()).sum();

        serde_json::json!({
            "event_log": {
                "events": event_log.len(),
                "estimated_bytes": log_bytes,
                "human": human_size(log_bytes),
                "mode": "in-memory (edge only, cleared after turn)",
            },
            "step_recorder": {
                "events": step_store_event_count,
                "estimated_bytes": step_store_estimated_bytes,
                "human": human_size(step_store_estimated_bytes),
                "mode": "JSONL on disk (edge + cloud sync)",
            },
            "threshold_guide": {
                "edge_soft_limit": "10 MB per session",
                "cloud_soft_limit": "100 MB per session",
                "current_session_estimate_human": human_size(log_bytes + step_store_estimated_bytes),
            },
        })
    }
}

fn human_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.2} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::EventLog;

    #[test]
    fn projection_scales_with_verbosity() {
        let mut log = EventLog::new();
        // Info-level events
        log.emit(EventKind::PhaseTransition {
            phase: "plan".into(),
            direction: "enter".into(),
        }, None);
        log.emit(EventKind::ToolCallStarted {
            tool_name: "bash".into(),
            tool_call_id: "t1".into(),
        }, None);
        // Debug-level (verbose-only)
        log.emit(EventKind::ThinkingChunk {
            content: "test".into(),
        }, None);
        log.emit(EventKind::PromptAssembled {
            model: "claude".into(),
            token_count: 100,
        }, None);

        let projections = TraceVolume::project(&log);

        let info = projections.iter().find(|p| p.level == "Info (non-verbose)").unwrap();
        assert_eq!(info.event_count, 2);
        assert_eq!(info.filtered_out, 2);

        let trace = projections.iter().find(|p| p.level == "Trace (all)").unwrap();
        assert_eq!(trace.event_count, 4);
        assert_eq!(trace.filtered_out, 0);
    }

    #[test]
    fn non_verbose_saves_data() {
        let mut log = EventLog::new();
        log.emit(EventKind::PhaseTransition {
            phase: "plan".into(),
            direction: "enter".into(),
        }, None);
        log.emit(EventKind::ThinkingChunk {
            content: "long thinking...".into(),
        }, None);

        let nv = TraceVolume::non_verbose_estimate(&log);
        assert_eq!(nv["non_verbose_events"], 1);
        assert_eq!(nv["verbose_events"], 1);
    }

    #[test]
    fn summary_includes_categories() {
        let mut log = EventLog::new();
        log.emit(EventKind::PhaseTransition {
            phase: "plan".into(),
            direction: "enter".into(),
        }, None);
        log.emit(EventKind::ToolCallStarted {
            tool_name: "bash".into(),
            tool_call_id: "t1".into(),
        }, None);

        let s = TraceVolume::summary(&log);
        assert_eq!(s["total_events"], 2);
        assert!(s["estimated_total_bytes"].as_u64().unwrap() > 0);
    }
}
