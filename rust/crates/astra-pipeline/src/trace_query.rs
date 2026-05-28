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

/// Cross-layer query using EventLog iteration + StepEventStore DAG traversal.
pub struct TraceQuery;

impl TraceQuery {
    /// Find events across all layers matching a canonical_event_id.
    pub fn find_by_canonical_id(
        event_log: &EventLog,
        step_store: &dyn StepEventStore,
        canonical_id: &str,
    ) -> Vec<UnifiedEvent> {
        let mut results = Vec::new();

        // Layer 1: EventLog — canonical_event_id is Uuid
        for event in event_log.events() {
            if event.canonical_event_id.to_string() == canonical_id {
                results.push(event_to_unified(event, "EventLog"));
            }
        }

        // Layer 2: StepRecorder — search from leaves + ancestors
        for leaf in step_store.leaves() {
            if leaf.canonical_event_id.as_deref() == Some(canonical_id) {
                results.push(step_to_unified(leaf, "StepRecorder"));
            }
            for a in step_store.ancestors(&leaf.event_id) {
                if a.canonical_event_id.as_deref() == Some(canonical_id)
                    && !results
                        .iter()
                        .any(|r| r.layer_event_id.as_deref() == Some(&a.event_id))
                {
                    results.push(step_to_unified(a, "StepRecorder"));
                }
            }
        }

        results
    }

    /// Walk the causal chain from a given canonical_event_id, across layers.
    pub fn causal_chain(
        event_log: &EventLog,
        step_store: &dyn StepEventStore,
        start_canonical_id: &str,
    ) -> Vec<UnifiedEvent> {
        let mut chain = Vec::new();

        // Build EventLog lookup by canonical_id string
        let log_by_canonical: std::collections::HashMap<String, &TurnEvent> = event_log
            .events()
            .iter()
            .map(|e| (e.canonical_event_id.to_string(), e))
            .collect();

        // Build StepRecorder lookup from leaves + ancestors
        let mut step_by_canonical: std::collections::HashMap<String, &StepEvent> =
            std::collections::HashMap::new();
        for leaf in step_store.leaves() {
            if let Some(ref id) = leaf.canonical_event_id {
                step_by_canonical.entry(id.clone()).or_insert(leaf);
            }
            for a in step_store.ancestors(&leaf.event_id) {
                if let Some(ref id) = a.canonical_event_id {
                    step_by_canonical.entry(id.clone()).or_insert(a);
                }
            }
        }

        let mut current_id = Some(start_canonical_id.to_string());
        // Walk causal chain using take() to avoid borrow conflicts
        while let Some(id) = current_id.take() {
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
        let mut seen_ids = std::collections::HashSet::new();
        let mut step_count = 0usize;
        for leaf in step_store.leaves() {
            if seen_ids.insert(leaf.event_id.clone()) {
                step_count += 1;
            }
            for a in step_store.ancestors(&leaf.event_id) {
                if seen_ids.insert(a.event_id.clone()) {
                    step_count += 1;
                }
            }
        }
        serde_json::json!({
            "event_log": event_log.len(),
            "step_recorder": step_count,
        })
    }

    /// Find events appearing in multiple layers (dedup candidates).
    pub fn cross_layer_duplicates(
        event_log: &EventLog,
        step_store: &dyn StepEventStore,
    ) -> Vec<UnifiedEvent> {
        let mut duplicates = Vec::new();

        let mut step_ids = std::collections::HashSet::new();
        for leaf in step_store.leaves() {
            if let Some(ref id) = leaf.canonical_event_id {
                step_ids.insert(id.clone());
            }
            for a in step_store.ancestors(&leaf.event_id) {
                if let Some(ref id) = a.canonical_event_id {
                    step_ids.insert(id.clone());
                }
            }
        }

        for event in event_log.events() {
            let id_str = event.canonical_event_id.to_string();
            if step_ids.contains(&id_str) {
                duplicates.push(event_to_unified(event, "EventLog (dup in StepRecorder)"));
            }
        }

        duplicates
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
    use crate::event::{EventKind, EventLog};
    use crate::step_protocol::{StepEvent, StepEventStore, StepEventType};

    struct MockStepStore {
        events: Vec<StepEvent>,
    }

    impl StepEventStore for MockStepStore {
        fn append(&mut self, event: StepEvent) {
            self.events.push(event);
        }
        fn events_for_step(&self, step_id: &str) -> Vec<&StepEvent> {
            self.events
                .iter()
                .filter(|e| e.step_id == step_id)
                .collect()
        }
        fn ancestors(&self, event_id: &str) -> Vec<&StepEvent> {
            let mut result = Vec::new();
            let mut current_ids: Vec<String> = vec![event_id.to_string()];
            let mut visited = std::collections::HashSet::new();
            while let Some(id) = current_ids.pop() {
                if !visited.insert(id.clone()) {
                    continue;
                }
                for e in &self.events {
                    if e.caused_by.contains(&id) && visited.insert(e.event_id.clone()) {
                        result.push(e);
                        current_ids.push(e.event_id.clone());
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
            EventKind::PhaseEntered {
                phase: "plan".into(),
            },
            None,
        );
        let id = log.events()[0].canonical_event_id.to_string();

        let mut store = MockStepStore { events: vec![] };
        store.append(make_step("e1", "step-1", Some(&id), &[]));

        let results = TraceQuery::find_by_canonical_id(&log, &store, &id);
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn layer_counts_accurate() {
        let mut log = EventLog::new();
        log.emit(
            EventKind::PhaseEntered {
                phase: "plan".into(),
            },
            None,
        );
        log.emit(
            EventKind::PhaseExited {
                phase: "plan".into(),
            },
            None,
        );

        let mut store = MockStepStore { events: vec![] };
        store.append(make_step("e1", "s1", None, &[]));
        store.append(make_step("e2", "s1", None, &["e1"]));

        let counts = TraceQuery::layer_counts(&log, &store);
        assert_eq!(counts["event_log"], 2);
        assert_eq!(counts["step_recorder"], 2);
    }

    #[test]
    fn cross_layer_duplicates_finds_overlap() {
        let mut log = EventLog::new();
        log.emit(
            EventKind::PhaseEntered {
                phase: "plan".into(),
            },
            None,
        );
        let id = log.events()[0].canonical_event_id.to_string();

        let mut store = MockStepStore { events: vec![] };
        store.append(make_step("e1", "s1", Some(&id), &[]));

        let dups = TraceQuery::cross_layer_duplicates(&log, &store);
        assert_eq!(dups.len(), 1);
    }

    #[test]
    fn causal_chain_empty_for_missing_id() {
        let log = EventLog::new();
        let store = MockStepStore { events: vec![] };
        let chain = TraceQuery::causal_chain(&log, &store, "nonexistent");
        assert!(chain.is_empty());
    }
}
