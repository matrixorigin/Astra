//! Per-step latency attribution from authoritative recorded step events.

use crate::step_protocol::{StepEvent, StepEventType};

/// Per-step latency attribution derived from persisted step events.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct StepLatencyBreakdown {
    pub step_id: String,
    pub started_at_ms: u64,
    pub ended_at_ms: Option<u64>,
    pub total_ms: Option<u64>,
    /// Time from StepStarted until the first tool-call event. This is model /
    /// planner wait, not tool or database execution.
    pub pre_tool_wait_ms: Option<u64>,
    pub first_tool_name: Option<String>,
    pub tool_call_count: u32,
    pub skipped_tool_count: u32,
    pub tool_execution_ms: u64,
    pub max_tool_execution_ms: u64,
    pub terminal_event_kind: Option<String>,
    pub dominant_phase: StepLatencyPhase,
}

/// High-level latency owner for a step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StepLatencyPhase {
    ModelWait,
    ToolExecution,
    NoTool,
    Unknown,
}

impl StepLatencyPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ModelWait => "model_wait",
            Self::ToolExecution => "tool_execution",
            Self::NoTool => "no_tool",
            Self::Unknown => "unknown",
        }
    }
}

/// Attribute latency from the executing caller's already-loaded step events.
pub fn step_latency_breakdown_from_events(events: &[StepEvent]) -> Vec<StepLatencyBreakdown> {
    build_step_latency_breakdown(events.iter().collect())
}

fn build_step_latency_breakdown(mut events: Vec<&StepEvent>) -> Vec<StepLatencyBreakdown> {
    events.sort_by_key(|event| {
        (
            event.step_id.as_str(),
            event.created_at,
            event.event_id.as_str(),
        )
    });

    let mut steps = std::collections::BTreeMap::<String, StepLatencyBuilder>::new();
    for event in events {
        let builder = steps.entry(event.step_id.clone()).or_default();
        builder.observe(event);
    }

    steps
        .into_iter()
        .filter_map(|(step_id, builder)| builder.finish(step_id))
        .collect()
}

#[derive(Default)]
struct StepLatencyBuilder {
    started_at_ms: Option<u64>,
    first_tool_started_at_ms: Option<u64>,
    first_tool_name: Option<String>,
    ended_at_ms: Option<u64>,
    terminal_event_kind: Option<String>,
    tool_call_count: u32,
    skipped_tool_count: u32,
    tool_execution_ms: u64,
    max_tool_execution_ms: u64,
}

impl StepLatencyBuilder {
    fn observe(&mut self, event: &StepEvent) {
        match event.event_type {
            StepEventType::StepStarted => {
                self.started_at_ms = Some(
                    self.started_at_ms
                        .map_or(event.created_at, |t| t.min(event.created_at)),
                );
            }
            StepEventType::ToolCallStarted => {
                self.tool_call_count = self.tool_call_count.saturating_add(1);
                if self
                    .first_tool_started_at_ms
                    .is_none_or(|t| event.created_at < t)
                {
                    self.first_tool_started_at_ms = Some(event.created_at);
                    self.first_tool_name = payload_string(event, "tool_name");
                }
            }
            StepEventType::ToolCallCompleted | StepEventType::ToolCallFailed => {
                let elapsed_ms = payload_u64(event, "elapsed_ms").unwrap_or(0);
                self.tool_execution_ms = self.tool_execution_ms.saturating_add(elapsed_ms);
                self.max_tool_execution_ms = self.max_tool_execution_ms.max(elapsed_ms);
            }
            StepEventType::ToolCallSkipped => {
                self.skipped_tool_count = self.skipped_tool_count.saturating_add(1);
            }
            StepEventType::StepCompleted
            | StepEventType::StepIncomplete
            | StepEventType::StepFailed
            | StepEventType::StepRetried
                if self.ended_at_ms.is_none_or(|t| event.created_at >= t) =>
            {
                self.ended_at_ms = Some(event.created_at);
                self.terminal_event_kind = Some(format!("{:?}", event.event_type));
            }
            _ => {}
        }
    }

    fn finish(self, step_id: String) -> Option<StepLatencyBreakdown> {
        let started_at_ms = self.started_at_ms?;
        let ended_at_ms = self.ended_at_ms;
        let total_ms = ended_at_ms.map(|ended| ended.saturating_sub(started_at_ms));
        let pre_tool_wait_ms = self
            .first_tool_started_at_ms
            .map(|tool_start| tool_start.saturating_sub(started_at_ms));
        let dominant_phase = match (pre_tool_wait_ms, total_ms) {
            (Some(wait_ms), _) if wait_ms > 0 && wait_ms >= self.tool_execution_ms => {
                StepLatencyPhase::ModelWait
            }
            (_, _) if self.tool_execution_ms > 0 => StepLatencyPhase::ToolExecution,
            (_, Some(_)) => StepLatencyPhase::NoTool,
            _ => StepLatencyPhase::Unknown,
        };

        Some(StepLatencyBreakdown {
            step_id,
            started_at_ms,
            ended_at_ms,
            total_ms,
            pre_tool_wait_ms,
            first_tool_name: self.first_tool_name,
            tool_call_count: self.tool_call_count,
            skipped_tool_count: self.skipped_tool_count,
            tool_execution_ms: self.tool_execution_ms,
            max_tool_execution_ms: self.max_tool_execution_ms,
            terminal_event_kind: self.terminal_event_kind,
            dominant_phase,
        })
    }
}

fn payload_u64(event: &StepEvent, key: &str) -> Option<u64> {
    event.payload.as_ref()?.get(key)?.as_u64()
}

fn payload_string(event: &StepEvent, key: &str) -> Option<String> {
    event
        .payload
        .as_ref()?
        .get(key)?
        .as_str()
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_step_event(
        id: &str,
        step: &str,
        event_type: StepEventType,
        created_at: u64,
        canonical: Option<&str>,
        parents: &[&str],
        payload: Option<serde_json::Value>,
    ) -> StepEvent {
        StepEvent {
            event_id: id.to_string(),
            run_id: "test-run".into(),
            canonical_event_id: canonical.map(|s| s.to_string()),
            step_id: step.to_string(),
            event_type,
            agent_id: None,
            caused_by: parents.iter().map(|s| s.to_string()).collect(),
            payload,
            created_at,
        }
    }

    #[test]
    fn step_latency_breakdown_attributes_slow_step_to_pre_tool_model_wait() {
        let mut events = Vec::new();
        events.push(make_step_event(
            "e1",
            "s1",
            StepEventType::StepStarted,
            1_000,
            None,
            &[],
            None,
        ));
        events.push(make_step_event(
            "e2",
            "s1",
            StepEventType::ToolCallStarted,
            9_000,
            None,
            &["e1"],
            Some(serde_json::json!({"tool_name": "bash"})),
        ));
        events.push(make_step_event(
            "e3",
            "s1",
            StepEventType::ToolCallCompleted,
            9_010,
            None,
            &["e2"],
            Some(serde_json::json!({"tool_name": "bash", "elapsed_ms": 8})),
        ));
        events.push(make_step_event(
            "e4",
            "s1",
            StepEventType::StepIncomplete,
            9_978,
            None,
            &["e3"],
            None,
        ));

        let breakdown = step_latency_breakdown_from_events(&events);

        assert_eq!(breakdown.len(), 1);
        let step = &breakdown[0];
        assert_eq!(step.step_id, "s1");
        assert_eq!(step.total_ms, Some(8_978));
        assert_eq!(step.pre_tool_wait_ms, Some(8_000));
        assert_eq!(step.first_tool_name.as_deref(), Some("bash"));
        assert_eq!(step.tool_execution_ms, 8);
        assert_eq!(step.max_tool_execution_ms, 8);
        assert_eq!(step.tool_call_count, 1);
        assert_eq!(step.dominant_phase, StepLatencyPhase::ModelWait);
    }

    #[test]
    fn step_latency_breakdown_handles_no_tool_terminal_step() {
        let mut events = Vec::new();
        events.push(make_step_event(
            "e1",
            "s1",
            StepEventType::StepStarted,
            1_000,
            None,
            &[],
            None,
        ));
        events.push(make_step_event(
            "e2",
            "s1",
            StepEventType::StepCompleted,
            1_250,
            None,
            &["e1"],
            None,
        ));

        let breakdown = step_latency_breakdown_from_events(&events);

        assert_eq!(breakdown.len(), 1);
        assert_eq!(breakdown[0].total_ms, Some(250));
        assert_eq!(breakdown[0].pre_tool_wait_ms, None);
        assert_eq!(breakdown[0].tool_execution_ms, 0);
        assert_eq!(breakdown[0].dominant_phase, StepLatencyPhase::NoTool);
        assert_eq!(
            breakdown[0].terminal_event_kind.as_deref(),
            Some("StepCompleted")
        );
    }

    #[test]
    fn step_latency_breakdown_from_events_uses_in_memory_recorder_events() {
        let events = vec![
            make_step_event(
                "e1",
                "s1",
                StepEventType::StepStarted,
                1_000,
                None,
                &[],
                None,
            ),
            make_step_event(
                "e2",
                "s1",
                StepEventType::ToolCallStarted,
                1_010,
                None,
                &["e1"],
                Some(serde_json::json!({"tool_name": "grep"})),
            ),
            make_step_event(
                "e3",
                "s1",
                StepEventType::ToolCallCompleted,
                1_240,
                None,
                &["e2"],
                Some(serde_json::json!({"tool_name": "grep", "elapsed_ms": 220})),
            ),
            make_step_event(
                "e4",
                "s1",
                StepEventType::StepRetried,
                1_250,
                None,
                &["e3"],
                None,
            ),
        ];

        let breakdown = step_latency_breakdown_from_events(&events);

        assert_eq!(breakdown.len(), 1);
        assert_eq!(breakdown[0].pre_tool_wait_ms, Some(10));
        assert_eq!(breakdown[0].tool_execution_ms, 220);
        assert_eq!(breakdown[0].dominant_phase, StepLatencyPhase::ToolExecution);
        assert_eq!(
            breakdown[0].terminal_event_kind.as_deref(),
            Some("StepRetried")
        );
    }

    #[test]
    fn step_latency_breakdown_keeps_zero_duration_tool_step_unknown() {
        let events = vec![
            make_step_event(
                "e1",
                "s1",
                StepEventType::StepStarted,
                1_000,
                None,
                &[],
                None,
            ),
            make_step_event(
                "e2",
                "s1",
                StepEventType::ToolCallStarted,
                1_000,
                None,
                &["e1"],
                Some(serde_json::json!({"tool_name": "noop"})),
            ),
            make_step_event(
                "e3",
                "s1",
                StepEventType::ToolCallCompleted,
                1_000,
                None,
                &["e2"],
                Some(serde_json::json!({"tool_name": "noop", "elapsed_ms": 0})),
            ),
        ];

        let breakdown = step_latency_breakdown_from_events(&events);

        assert_eq!(breakdown.len(), 1);
        assert_eq!(breakdown[0].pre_tool_wait_ms, Some(0));
        assert_eq!(breakdown[0].tool_execution_ms, 0);
        assert_eq!(breakdown[0].dominant_phase, StepLatencyPhase::Unknown);
    }

    #[test]
    fn step_latency_breakdown_treats_terminal_zero_duration_tool_step_as_no_tool() {
        let events = vec![
            make_step_event(
                "e1",
                "s1",
                StepEventType::StepStarted,
                1_000,
                None,
                &[],
                None,
            ),
            make_step_event(
                "e2",
                "s1",
                StepEventType::ToolCallStarted,
                1_000,
                None,
                &["e1"],
                Some(serde_json::json!({"tool_name": "noop"})),
            ),
            make_step_event(
                "e3",
                "s1",
                StepEventType::ToolCallCompleted,
                1_000,
                None,
                &["e2"],
                Some(serde_json::json!({"tool_name": "noop", "elapsed_ms": 0})),
            ),
            make_step_event(
                "e4",
                "s1",
                StepEventType::StepCompleted,
                1_000,
                None,
                &["e3"],
                None,
            ),
        ];

        let breakdown = step_latency_breakdown_from_events(&events);

        assert_eq!(breakdown.len(), 1);
        assert_eq!(breakdown[0].pre_tool_wait_ms, Some(0));
        assert_eq!(breakdown[0].tool_execution_ms, 0);
        assert_eq!(breakdown[0].dominant_phase, StepLatencyPhase::NoTool);
    }
}
