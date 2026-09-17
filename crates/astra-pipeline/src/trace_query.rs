//! Unified trace query interface for cross-layer event correlation.
//!
//! Layers:
//! 1. EventLog (in-memory, per-turn) — event.rs with causal chains via EventId
//! 2. StepRecorder (JSONL, per-session) — step_protocol.rs with DAG via Vec<String>
//! 3. TraceEvent (SQL DB, per-session) — astra-turn-core trace_event.rs with SQL
//!
//! All layers share canonical_event_id (UUID v7) for cross-layer joins.
//! Note: EventLog uses `Uuid` directly, StepRecorder uses `Option<String>`.

use crate::event::{EventLog, TurnEvent};
use crate::step_protocol::{StepEvent, StepEventStore, StepEventType};

fn collect_step_events(step_store: &dyn StepEventStore) -> Vec<&StepEvent> {
    let mut seen = std::collections::HashSet::new();
    let mut events = Vec::new();
    for leaf in step_store.leaves() {
        if seen.insert(leaf.event_id.clone()) {
            events.push(leaf);
        }
        for ancestor in step_store.ancestors(&leaf.event_id) {
            if seen.insert(ancestor.event_id.clone()) {
                events.push(ancestor);
            }
        }
    }
    events
}

/// Normalized event from any storage layer.
#[derive(Debug, Clone, serde::Serialize)]
pub struct UnifiedEvent {
    pub canonical_event_id: Option<String>,
    pub layer_event_id: Option<String>,
    pub source_layer: &'static str,
    pub event_kind: String,
    pub timestamp_ms: Option<u64>,
    pub payload: serde_json::Value,
}

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

/// Default max results returned by query methods.
const DEFAULT_QUERY_LIMIT: usize = 1000;

/// Cross-layer query using EventLog iteration + StepEventStore DAG traversal.
pub struct TraceQuery;

impl TraceQuery {
    /// Find events across all layers matching a canonical_event_id.
    /// Returns at most `limit` results (default 1000).
    pub fn find_by_canonical_id(
        event_log: &EventLog,
        step_store: &dyn StepEventStore,
        canonical_id: &str,
        limit: Option<usize>,
    ) -> Vec<UnifiedEvent> {
        let max = limit.unwrap_or(DEFAULT_QUERY_LIMIT);
        let mut results = Vec::with_capacity(max.min(64));
        let step_events = collect_step_events(step_store);

        // Layer 1: EventLog — canonical_event_id is Uuid
        for event in event_log.events() {
            if results.len() >= max {
                return results;
            }
            if event.canonical_event_id.to_string() == canonical_id {
                results.push(event_to_unified(event, "EventLog"));
            }
        }

        // Layer 2: StepRecorder — traverse the DAG once, then filter.
        for event in step_events {
            if results.len() >= max {
                return results;
            }
            if event.canonical_event_id.as_deref() == Some(canonical_id) {
                results.push(step_to_unified(event, "StepRecorder"));
            }
        }

        results
    }

    /// Walk the causal chain from a given canonical_event_id, across layers.
    /// Returns at most `limit` events (default 1000).
    pub fn causal_chain(
        event_log: &EventLog,
        step_store: &dyn StepEventStore,
        start_canonical_id: &str,
        limit: Option<usize>,
    ) -> Vec<UnifiedEvent> {
        let max = limit.unwrap_or(DEFAULT_QUERY_LIMIT);
        let mut chain = Vec::with_capacity(max.min(64));

        // Build EventLog lookup by canonical_id string
        let log_by_canonical: std::collections::HashMap<String, &TurnEvent> = event_log
            .events()
            .iter()
            .map(|e| (e.canonical_event_id.to_string(), e))
            .collect();

        // Build StepRecorder lookup from leaves + ancestors
        let mut step_by_canonical: std::collections::HashMap<String, &StepEvent> =
            std::collections::HashMap::new();
        for event in collect_step_events(step_store) {
            if let Some(ref id) = event.canonical_event_id {
                step_by_canonical.entry(id.clone()).or_insert(event);
            }
        }

        let mut current_id = Some(start_canonical_id.to_string());
        // Walk causal chain using take() to avoid borrow conflicts
        while let Some(id) = current_id.take() {
            if chain.len() >= max {
                break;
            }
            let mut found = false;

            // Check EventLog
            if let Some(event) = log_by_canonical.get(id.as_str()) {
                chain.push(UnifiedEvent {
                    canonical_event_id: Some(event.canonical_event_id.to_string()),
                    layer_event_id: Some(event.id.0.to_string()),
                    source_layer: "EventLog",
                    event_kind: format!("{:?}", event.kind),
                    timestamp_ms: None,
                    payload: serde_json::json!({"causal_parent": event.caused_by.map(|id| id.0.to_string())}),
                });
                // Follow the EventId causal chain
                current_id = match event.caused_by {
                    Some(parent_id) => event_log
                        .events()
                        .iter()
                        .find(|e| e.id == parent_id)
                        .map(|e| e.canonical_event_id.to_string()),
                    None => None,
                };
                found = true;
            }

            // Check StepRecorder
            if let Some(event) = step_by_canonical.get(id.as_str()) {
                if !found {
                    chain.push(step_to_unified(event, "StepRecorder"));
                    if let Some(first_parent) = event.caused_by.first() {
                        let parent_canonical = step_by_canonical
                            .iter()
                            .find(|(_, e)| e.event_id == *first_parent)
                            .map(|(id, _)| id.clone());
                        current_id = parent_canonical;
                    } else {
                        current_id = None;
                    }
                }
                found = true;
            }

            if !found {
                break;
            }
        }

        chain.reverse();
        chain
    }

    /// Count events per layer.
    pub fn layer_counts(
        event_log: &EventLog,
        step_store: &dyn StepEventStore,
    ) -> serde_json::Value {
        let step_count = collect_step_events(step_store).len();
        serde_json::json!({
            "event_log": event_log.len(),
            "step_recorder": step_count,
        })
    }

    /// Attribute each step's wall-clock time to model wait vs tool execution.
    ///
    /// This keeps performance diagnosis grounded in recorded events: if
    /// `pre_tool_wait_ms` dominates while `tool_execution_ms` is tiny, the slow
    /// path is before tool execution and should not be blamed on DB/tool calls.
    pub fn step_latency_breakdown(step_store: &dyn StepEventStore) -> Vec<StepLatencyBreakdown> {
        build_step_latency_breakdown(collect_step_events(step_store))
    }

    /// Attribute step latency from an already-loaded event slice.
    ///
    /// Use this when the caller owns a [`StepRecorder`] in memory rather than
    /// a durable [`StepEventStore`].
    pub fn step_latency_breakdown_from_events(events: &[StepEvent]) -> Vec<StepLatencyBreakdown> {
        build_step_latency_breakdown(events.iter().collect())
    }

    /// Find events appearing in multiple layers (dedup candidates).
    /// Returns at most `limit` results (default 1000).
    pub fn cross_layer_duplicates(
        event_log: &EventLog,
        step_store: &dyn StepEventStore,
        limit: Option<usize>,
    ) -> Vec<UnifiedEvent> {
        let max = limit.unwrap_or(DEFAULT_QUERY_LIMIT);
        let mut duplicates = Vec::with_capacity(max.min(64));

        let mut step_ids = std::collections::HashSet::new();
        for event in collect_step_events(step_store) {
            if let Some(ref id) = event.canonical_event_id {
                step_ids.insert(id.clone());
            }
        }

        for event in event_log.events() {
            if duplicates.len() >= max {
                break;
            }
            let id_str = event.canonical_event_id.to_string();
            if step_ids.contains(&id_str) {
                duplicates.push(event_to_unified(event, "EventLog (dup in StepRecorder)"));
            }
        }

        duplicates
    }

    /// Filter events by time range (timestamp_ms).
    pub fn filter_by_time_range(
        events: Vec<UnifiedEvent>,
        start_ms: Option<u64>,
        end_ms: Option<u64>,
    ) -> Vec<UnifiedEvent> {
        events
            .into_iter()
            .filter(|e| {
                if let Some(ts) = e.timestamp_ms {
                    let after_start = start_ms.map(|s| ts >= s).unwrap_or(true);
                    let before_end = end_ms.map(|e| ts <= e).unwrap_or(true);
                    after_start && before_end
                } else {
                    // Events without timestamp are excluded from time filtering
                    start_ms.is_none() && end_ms.is_none()
                }
            })
            .collect()
    }

    /// Filter events by event kind (substring match).
    pub fn filter_by_event_kind(events: Vec<UnifiedEvent>, pattern: &str) -> Vec<UnifiedEvent> {
        let pattern_lower = pattern.to_lowercase();
        events
            .into_iter()
            .filter(|e| e.event_kind.to_lowercase().contains(&pattern_lower))
            .collect()
    }

    /// Filter events by source layer.
    pub fn filter_by_layer(events: Vec<UnifiedEvent>, layer: &str) -> Vec<UnifiedEvent> {
        events
            .into_iter()
            .filter(|e| e.source_layer == layer)
            .collect()
    }

    /// Export events to JSON format.
    pub fn export_json(events: &[UnifiedEvent]) -> serde_json::Value {
        serde_json::json!({
            "total_events": events.len(),
            "events": events,
            "exported_at": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        })
    }

    /// Export events to CSV format (simplified: id, layer, kind, timestamp).
    pub fn export_csv(events: &[UnifiedEvent]) -> String {
        let mut csv = String::from(
            "canonical_event_id,layer_event_id,source_layer,event_kind,timestamp_ms\n",
        );
        for e in events {
            csv.push_str(&format!(
                "{},{},{},{},{}\n",
                e.canonical_event_id.as_deref().unwrap_or(""),
                e.layer_event_id.as_deref().unwrap_or(""),
                e.source_layer,
                e.event_kind.replace(',', "_"),
                e.timestamp_ms.map(|t| t.to_string()).unwrap_or_default()
            ));
        }
        csv
    }
}

// --- helpers ---

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

fn event_to_unified(event: &TurnEvent, layer: &'static str) -> UnifiedEvent {
    UnifiedEvent {
        canonical_event_id: Some(event.canonical_event_id.to_string()),
        layer_event_id: Some(event.id.0.to_string()),
        source_layer: layer,
        event_kind: format!("{:?}", event.kind),
        timestamp_ms: None,
        payload: serde_json::Value::Null,
    }
}

fn step_to_unified(event: &StepEvent, layer: &'static str) -> UnifiedEvent {
    UnifiedEvent {
        canonical_event_id: event.canonical_event_id.clone(),
        layer_event_id: Some(event.event_id.clone()),
        source_layer: layer,
        event_kind: format!("{:?}", event.event_type),
        timestamp_ms: Some(event.created_at),
        payload: event.payload.clone().unwrap_or(serde_json::Value::Null),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::TraceLevel;
    use crate::event::{EventKind, EventLog};
    use crate::step_protocol::{StepEvent, StepEventStore, StepEventType};

    struct MockStepStore {
        events: Vec<StepEvent>,
    }

    impl StepEventStore for MockStepStore {
        fn append(&mut self, event: StepEvent) -> std::io::Result<()> {
            self.events.push(event);
            Ok(())
        }
        fn events_for_step(&self, step_id: &str) -> Vec<&StepEvent> {
            self.events
                .iter()
                .filter(|e| e.step_id == step_id)
                .collect()
        }
        fn ancestors(&self, event_id: &str) -> Vec<&StepEvent> {
            let mut result = Vec::new();
            let mut to_visit = vec![event_id.to_string()];
            let mut visited = std::collections::HashSet::new();
            while let Some(id) = to_visit.pop() {
                if !visited.insert(id.clone()) {
                    continue;
                }
                // Find the event with this id and walk UP its caused_by chain
                if let Some(event) = self.events.iter().find(|e| e.event_id == id) {
                    for parent_id in &event.caused_by {
                        if let Some(parent) = self.events.iter().find(|e| e.event_id == *parent_id)
                        {
                            result.push(parent);
                            to_visit.push(parent_id.clone());
                        }
                    }
                }
            }
            result
        }
        fn descendants(&self, event_id: &str) -> Vec<&StepEvent> {
            let mut result = Vec::new();
            if let Some(start) = self.events.iter().find(|e| e.event_id == event_id) {
                let mut to_visit: Vec<String> = start.caused_by.clone();
                let mut visited = std::collections::HashSet::new();
                visited.insert(event_id.to_string());
                while let Some(id) = to_visit.pop() {
                    if !visited.insert(id.clone()) {
                        continue;
                    }
                    for e in &self.events {
                        if e.caused_by.contains(&id) && visited.insert(e.event_id.clone()) {
                            result.push(e);
                            to_visit.push(e.event_id.clone());
                        }
                    }
                }
            }
            result
        }
        fn leaves(&self) -> Vec<&StepEvent> {
            let parent_ids: std::collections::HashSet<_> = self
                .events
                .iter()
                .flat_map(|e| e.caused_by.clone())
                .collect();
            self.events
                .iter()
                .filter(|e| !parent_ids.contains(&e.event_id))
                .collect()
        }
        fn len(&self) -> usize {
            self.events.len()
        }
    }

    fn make_step(id: &str, step: &str, canonical: Option<&str>, parents: &[&str]) -> StepEvent {
        make_step_event(
            id,
            step,
            StepEventType::StepStarted,
            1000,
            canonical,
            parents,
            None,
        )
    }

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
    fn find_by_canonical_id_cross_layer() {
        let mut log = EventLog::new();
        log.emit(
            EventKind::PhaseTransition {
                from: "Plan".into(),
                to: "Execute".into(),
            },
            None,
        );
        let id = log.events()[0].canonical_event_id.to_string();

        let mut store = MockStepStore { events: vec![] };
        let _ = store.append(make_step("e1", "step-1", Some(&id), &[]));

        let results = TraceQuery::find_by_canonical_id(&log, &store, &id, None);
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn layer_counts_accurate() {
        let mut log = EventLog::with_min_level(TraceLevel::Debug);
        log.emit(
            EventKind::PhaseTransition {
                from: "Plan".into(),
                to: "Execute".into(),
            },
            None,
        );
        log.emit(
            EventKind::PhaseTransition {
                from: "Execute".into(),
                to: "Complete".into(),
            },
            None,
        );

        let mut store = MockStepStore { events: vec![] };
        let _ = store.append(make_step("e1", "s1", None, &[]));
        let _ = store.append(make_step("e2", "s1", None, &["e1"]));

        let counts = TraceQuery::layer_counts(&log, &store);
        assert_eq!(counts["event_log"], 2);
        assert_eq!(counts["step_recorder"], 2);
    }

    #[test]
    fn step_latency_breakdown_attributes_slow_step_to_pre_tool_model_wait() {
        let mut store = MockStepStore { events: vec![] };
        let _ = store.append(make_step_event(
            "e1",
            "s1",
            StepEventType::StepStarted,
            1_000,
            None,
            &[],
            None,
        ));
        let _ = store.append(make_step_event(
            "e2",
            "s1",
            StepEventType::ToolCallStarted,
            9_000,
            None,
            &["e1"],
            Some(serde_json::json!({"tool_name": "bash"})),
        ));
        let _ = store.append(make_step_event(
            "e3",
            "s1",
            StepEventType::ToolCallCompleted,
            9_010,
            None,
            &["e2"],
            Some(serde_json::json!({"tool_name": "bash", "elapsed_ms": 8})),
        ));
        let _ = store.append(make_step_event(
            "e4",
            "s1",
            StepEventType::StepIncomplete,
            9_978,
            None,
            &["e3"],
            None,
        ));

        let breakdown = TraceQuery::step_latency_breakdown(&store);

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
        let mut store = MockStepStore { events: vec![] };
        let _ = store.append(make_step_event(
            "e1",
            "s1",
            StepEventType::StepStarted,
            1_000,
            None,
            &[],
            None,
        ));
        let _ = store.append(make_step_event(
            "e2",
            "s1",
            StepEventType::StepCompleted,
            1_250,
            None,
            &["e1"],
            None,
        ));

        let breakdown = TraceQuery::step_latency_breakdown(&store);

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

        let breakdown = TraceQuery::step_latency_breakdown_from_events(&events);

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

        let breakdown = TraceQuery::step_latency_breakdown_from_events(&events);

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

        let breakdown = TraceQuery::step_latency_breakdown_from_events(&events);

        assert_eq!(breakdown.len(), 1);
        assert_eq!(breakdown[0].pre_tool_wait_ms, Some(0));
        assert_eq!(breakdown[0].tool_execution_ms, 0);
        assert_eq!(breakdown[0].dominant_phase, StepLatencyPhase::NoTool);
    }

    #[test]
    fn cross_layer_duplicates_finds_overlap() {
        let mut log = EventLog::new();
        log.emit(
            EventKind::PhaseTransition {
                from: "Plan".into(),
                to: "Execute".into(),
            },
            None,
        );
        let id = log.events()[0].canonical_event_id.to_string();

        let mut store = MockStepStore { events: vec![] };
        let _ = store.append(make_step("e1", "s1", Some(&id), &[]));

        let dups = TraceQuery::cross_layer_duplicates(&log, &store, None);
        assert_eq!(dups.len(), 1);
    }

    #[test]
    fn causal_chain_empty_for_missing_id() {
        let log = EventLog::new();
        let store = MockStepStore { events: vec![] };
        let chain = TraceQuery::causal_chain(&log, &store, "nonexistent", None);
        assert!(chain.is_empty());
    }

    #[test]
    fn filter_by_time_range_inclusive() {
        let events = vec![
            UnifiedEvent {
                canonical_event_id: Some("a".into()),
                layer_event_id: Some("1".into()),
                source_layer: "Test",
                event_kind: "A".into(),
                timestamp_ms: Some(100),
                payload: serde_json::Value::Null,
            },
            UnifiedEvent {
                canonical_event_id: Some("b".into()),
                layer_event_id: Some("2".into()),
                source_layer: "Test",
                event_kind: "B".into(),
                timestamp_ms: Some(200),
                payload: serde_json::Value::Null,
            },
            UnifiedEvent {
                canonical_event_id: Some("c".into()),
                layer_event_id: Some("3".into()),
                source_layer: "Test",
                event_kind: "C".into(),
                timestamp_ms: Some(300),
                payload: serde_json::Value::Null,
            },
        ];
        let filtered = TraceQuery::filter_by_time_range(events, Some(100), Some(200));
        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0].event_kind, "A");
        assert_eq!(filtered[1].event_kind, "B");
    }

    #[test]
    fn filter_by_event_kind_case_insensitive() {
        let events = vec![
            UnifiedEvent {
                canonical_event_id: None,
                layer_event_id: None,
                source_layer: "Test",
                event_kind: "ToolCallStarted".into(),
                timestamp_ms: None,
                payload: serde_json::Value::Null,
            },
            UnifiedEvent {
                canonical_event_id: None,
                layer_event_id: None,
                source_layer: "Test",
                event_kind: "PhaseTransition".into(),
                timestamp_ms: None,
                payload: serde_json::Value::Null,
            },
        ];
        let filtered = TraceQuery::filter_by_event_kind(events, "tool");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].event_kind, "ToolCallStarted");
    }

    #[test]
    fn filter_by_layer_exact() {
        let events = vec![
            UnifiedEvent {
                canonical_event_id: None,
                layer_event_id: None,
                source_layer: "EventLog",
                event_kind: "A".into(),
                timestamp_ms: None,
                payload: serde_json::Value::Null,
            },
            UnifiedEvent {
                canonical_event_id: None,
                layer_event_id: None,
                source_layer: "StepRecorder",
                event_kind: "B".into(),
                timestamp_ms: None,
                payload: serde_json::Value::Null,
            },
        ];
        let filtered = TraceQuery::filter_by_layer(events, "StepRecorder");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].event_kind, "B");
    }

    #[test]
    fn export_json_includes_metadata() {
        let events = vec![UnifiedEvent {
            canonical_event_id: Some("test".into()),
            layer_event_id: Some("1".into()),
            source_layer: "EventLog",
            event_kind: "Test".into(),
            timestamp_ms: None,
            payload: serde_json::Value::Null,
        }];
        let json = TraceQuery::export_json(&events);
        assert_eq!(json["total_events"], 1);
        assert!(json["exported_at"].as_u64().unwrap() > 0);
    }

    #[test]
    fn export_csv_header_and_rows() {
        let events = vec![UnifiedEvent {
            canonical_event_id: Some("id1".into()),
            layer_event_id: Some("e1".into()),
            source_layer: "EventLog",
            event_kind: "Test".into(),
            timestamp_ms: Some(12345),
            payload: serde_json::Value::Null,
        }];
        let csv = TraceQuery::export_csv(&events);
        assert!(csv.starts_with("canonical_event_id,"));
        assert!(csv.contains("id1,e1,EventLog,Test,12345"));
    }
}
