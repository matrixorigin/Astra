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
use crate::step_protocol::{StepEvent, StepEventStore};

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
                        if visited.insert(parent_id.clone())
                            && let Some(parent) =
                                self.events.iter().find(|e| e.event_id == *parent_id)
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
        StepEvent {
            event_id: id.to_string(),
            canonical_event_id: canonical.map(|s| s.to_string()),
            step_id: step.to_string(),
            event_type: StepEventType::StepStarted,
            agent_id: None,
            caused_by: parents.iter().map(|s| s.to_string()).collect(),
            payload: None,
            created_at: 1000,
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
