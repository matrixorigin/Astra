//! TurnEvent: causal event log for observability, replay, and learning.
//!
//! Every state mutation in the cognitive runtime emits a TurnEvent.
//! Events form a causal graph via the `caused_by` field, enabling
//! automatic root-cause analysis on failures.

use std::time::Instant;

/// Unique event identifier (monotonically increasing within a turn).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EventId(pub u64);

/// A single event in the causal event log.
#[derive(Debug, Clone)]
pub struct TurnEvent {
    pub id: EventId,
    pub kind: EventKind,
    /// The event that caused this one (for causal tracing).
    pub caused_by: Option<EventId>,
    /// Monotonic timestamp.
    pub elapsed_ms: u64,
}

/// The kinds of events emitted during a turn.
#[derive(Debug, Clone)]
pub enum EventKind {
    // ── Phase transitions ──
    PhaseTransition {
        from: crate::state::AgentPhase,
        to: crate::state::AgentPhase,
    },

    // ── Perception ──
    IntentDetected {
        signals: Vec<String>,
        confidence: f64,
    },
    EntityExtracted {
        entities: Vec<String>,
        domains: Vec<String>,
    },

    // ── Planning ──
    ToolsSelected {
        tools: Vec<String>,
        confidence: f64,
        boost_terms: Vec<String>,
    },
    BudgetSet {
        max_rounds: u32,
        max_tokens: u64,
    },

    // ── Execution ──
    LlmChunk {
        text: String,
    },
    ToolCallStarted {
        call_id: String,
        tool_name: String,
    },
    ToolCallCompleted {
        call_id: String,
        tool_name: String,
        duration_ms: u64,
        success: bool,
        error: Option<String>,
    },

    // ── Evaluation ──
    ProgressRecorded {
        score: f64,
        rate: Option<f64>,
    },
    BudgetUpdate {
        tokens_consumed: u64,
        rounds_used: u32,
        elapsed_ms: u64,
    },
    StallDetected {
        round: u32,
        reason: String,
    },
    CircuitBreakerTripped {
        tool: String,
        failure_count: usize,
    },

    // ── Budget adjustments ──
    BudgetExpanded {
        new_max_rounds: u32,
        factor: f64,
    },

    // ── Reflection ──
    ReflectionGenerated {
        what_happened: String,
        what_to_try: String,
        confidence: f64,
    },

    // ── Terminal ──
    TurnCompleted {
        status: String,
        total_rounds: u32,
        total_tokens: u64,
    },
}

/// Append-only causal event log.
///
/// Used for:
/// - **Observability**: Stream events to metrics/UI
/// - **Replay**: Reconstruct turn state from events
/// - **Learning**: Analyze successful/failed patterns
/// - **Root-cause analysis**: Trace `caused_by` chains on failure
#[derive(Debug)]
pub struct EventLog {
    events: Vec<TurnEvent>,
    next_id: u64,
    start: Instant,
}

impl EventLog {
    pub fn new() -> Self {
        Self {
            events: Vec::new(),
            next_id: 0,
            start: Instant::now(),
        }
    }

    /// Emit a new event, returning its ID for use as `caused_by` in future events.
    pub fn emit(&mut self, kind: EventKind, caused_by: Option<EventId>) -> EventId {
        let id = EventId(self.next_id);
        self.next_id += 1;
        let event = TurnEvent {
            id,
            kind,
            caused_by,
            elapsed_ms: self.start.elapsed().as_millis() as u64,
        };
        self.events.push(event);
        id
    }

    /// All events in order.
    pub fn events(&self) -> &[TurnEvent] {
        &self.events
    }

    /// Number of events logged.
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Whether the log is empty.
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Find the root cause of a given event by tracing `caused_by` chains.
    pub fn root_cause(&self, id: EventId) -> EventId {
        let mut current = id;
        let mut visited = std::collections::HashSet::new();
        visited.insert(current);
        while let Some(event) = self.events.iter().find(|e| e.id == current) {
            match event.caused_by {
                Some(parent) if !visited.contains(&parent) => {
                    visited.insert(parent);
                    current = parent;
                }
                _ => break,
            }
        }
        current
    }

    /// Get all events in the causal chain leading to a given event.
    pub fn causal_chain(&self, id: EventId) -> Vec<&TurnEvent> {
        let mut chain = Vec::new();
        let mut current = id;
        let mut visited = std::collections::HashSet::new();
        visited.insert(current);
        while let Some(event) = self.events.iter().find(|e| e.id == current) {
            chain.push(event);
            match event.caused_by {
                Some(parent) if !visited.contains(&parent) => {
                    visited.insert(parent);
                    current = parent;
                }
                _ => break,
            }
        }
        chain.reverse(); // root first
        chain
    }
}

impl Default for EventLog {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_log_basic_emit() {
        let mut log = EventLog::new();
        assert!(log.is_empty());

        let id1 = log.emit(
            EventKind::PhaseTransition {
                from: crate::state::AgentPhase::Perceive,
                to: crate::state::AgentPhase::Plan,
            },
            None,
        );
        assert_eq!(log.len(), 1);
        assert_eq!(id1, EventId(0));

        let id2 = log.emit(
            EventKind::ToolsSelected {
                tools: vec!["bash".into()],
                confidence: 0.8,
                boost_terms: vec![],
            },
            Some(id1),
        );
        assert_eq!(id2, EventId(1));
        assert_eq!(log.len(), 2);
    }

    #[test]
    fn causal_chain_traces_correctly() {
        let mut log = EventLog::new();
        let e0 = log.emit(
            EventKind::IntentDetected {
                signals: vec!["is_fetch".into()],
                confidence: 0.7,
            },
            None,
        );
        let e1 = log.emit(
            EventKind::ToolsSelected {
                tools: vec!["github_list_prs".into()],
                confidence: 0.7,
                boost_terms: vec![],
            },
            Some(e0),
        );
        let e2 = log.emit(
            EventKind::ToolCallStarted {
                call_id: "c1".into(),
                tool_name: "github_list_prs".into(),
            },
            Some(e1),
        );
        let e3 = log.emit(
            EventKind::ToolCallCompleted {
                call_id: "c1".into(),
                tool_name: "github_list_prs".into(),
                duration_ms: 150,
                success: false,
                error: Some("404 Not Found".into()),
            },
            Some(e2),
        );

        // Root cause of the failure should be the intent detection
        let root = log.root_cause(e3);
        assert_eq!(root, e0);

        // Causal chain should include all 4 events
        let chain = log.causal_chain(e3);
        assert_eq!(chain.len(), 4);
        assert_eq!(chain[0].id, e0); // root first
        assert_eq!(chain[3].id, e3); // leaf last
    }

    #[test]
    fn root_cause_single_event() {
        let mut log = EventLog::new();
        let e0 = log.emit(
            EventKind::StallDetected {
                round: 3,
                reason: "repeated tool calls".into(),
            },
            None,
        );
        assert_eq!(log.root_cause(e0), e0);
    }

    #[test]
    fn event_ids_monotonic() {
        let mut log = EventLog::new();
        let ids: Vec<EventId> = (0..5)
            .map(|_| log.emit(EventKind::LlmChunk { text: "hi".into() }, None))
            .collect();
        for i in 1..ids.len() {
            assert!(ids[i].0 > ids[i - 1].0);
        }
    }

    #[test]
    fn causal_chain_no_infinite_loop() {
        // Even if caused_by points to self (shouldn't happen, but defense)
        let mut log = EventLog::new();
        let e0 = log.emit(EventKind::LlmChunk { text: "a".into() }, None);
        // Manually can't create a cycle since emit always uses increasing IDs.
        // But test root_cause terminates.
        let chain = log.causal_chain(e0);
        assert_eq!(chain.len(), 1);
    }
}
