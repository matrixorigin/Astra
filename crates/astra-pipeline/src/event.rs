//! Minimal event log for trace query and step recording.
//!
//! Contains only the types and functions used by `step_recorder` and
//! `trace_query`. The full event system that powered the dead
//! EvaluateStage/ReflectStage pipeline was removed.

use std::time::Instant;
use uuid::Uuid;

// Re-export shared trace types from astra-core
pub use astra_core::{TraceCategory, TraceLevel, TraceVerbosity};

/// Maximum Unicode scalars kept in `ToolCallOutput`'s `output_preview`.
pub const TOOL_OUTPUT_PREVIEW_CHARS: usize = 500;

/// Maximum Unicode scalars in non-verbose mode.
pub const TOOL_OUTPUT_PREVIEW_CHARS_COMPACT: usize = 200;

/// Clip `s` to at most [`TOOL_OUTPUT_PREVIEW_CHARS`] Unicode scalars,
/// appending … when the output was longer.
pub fn clip_output_preview(s: &str) -> String {
    let mut chars = s.chars();
    let preview: String = chars.by_ref().take(TOOL_OUTPUT_PREVIEW_CHARS).collect();
    if chars.next().is_some() {
        format!("{preview}…")
    } else {
        preview
    }
}

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
    /// Optional tracing span id that was active when this event was emitted.
    pub span_id: Option<tracing::Id>,
    /// Canonical UUID v7 shared across all storage layers.
    pub canonical_event_id: Uuid,
}

/// The kinds of events emitted during a turn.
#[derive(Debug, Clone)]
pub enum EventKind {
    // ── Phase transitions ──
    PhaseTransition {
        from: String,
        to: String,
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
    ThinkingChunk {
        text: String,
    },
    LlmRequest {
        model: String,
        provider: String,
        message_count: usize,
        tool_count: usize,
        max_output_tokens: Option<usize>,
        round: u32,
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
    ToolCallOutput {
        call_id: String,
        tool_name: String,
        output_preview: String,
    },

    // ── Memory ──
    MemoryQuery {
        query: String,
        top_k: usize,
        source: String,
    },
    MemoryRetrieved {
        result_count: usize,
        duration_ms: u64,
    },

    // ── Skill lifecycle ──
    SkillStarted {
        skill_name: String,
    },
    SkillCompleted {
        skill_name: String,
        duration_ms: u64,
        success: bool,
    },

    // ── Prompt assembly ──
    PromptAssembled {
        component_count: usize,
        estimated_tokens: usize,
    },

    // ── Guard evaluation ──
    GuardEvaluated {
        guard_name: String,
        allowed: bool,
        reason: Option<String>,
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

impl EventKind {
    /// The default trace level for this event kind.
    pub fn default_level(&self) -> TraceLevel {
        match self {
            EventKind::CircuitBreakerTripped { .. } => TraceLevel::Error,
            EventKind::StallDetected { .. } => TraceLevel::Error,
            EventKind::BudgetUpdate { .. } => TraceLevel::Warn,
            EventKind::ProgressRecorded { .. } => TraceLevel::Warn,
            EventKind::PhaseTransition { .. } => TraceLevel::Info,
            EventKind::ToolCallStarted { .. } => TraceLevel::Info,
            EventKind::ToolCallCompleted { .. } => TraceLevel::Info,
            EventKind::ToolsSelected { .. } => TraceLevel::Info,
            EventKind::TurnCompleted { .. } => TraceLevel::Info,
            EventKind::IntentDetected { .. } => TraceLevel::Info,
            EventKind::EntityExtracted { .. } => TraceLevel::Info,
            EventKind::BudgetSet { .. } => TraceLevel::Info,
            EventKind::SkillStarted { .. } => TraceLevel::Info,
            EventKind::SkillCompleted { .. } => TraceLevel::Info,
            EventKind::GuardEvaluated { .. } => TraceLevel::Info,
            EventKind::BudgetExpanded { .. } => TraceLevel::Info,
            EventKind::ReflectionGenerated { .. } => TraceLevel::Info,
            EventKind::LlmChunk { .. } => TraceLevel::Trace,
            EventKind::ThinkingChunk { .. } => TraceLevel::Trace,
            EventKind::LlmRequest { .. } => TraceLevel::Trace,
            EventKind::ToolCallOutput { .. } => TraceLevel::Trace,
            EventKind::MemoryQuery { .. } => TraceLevel::Debug,
            EventKind::MemoryRetrieved { .. } => TraceLevel::Debug,
            EventKind::PromptAssembled { .. } => TraceLevel::Debug,
        }
    }

    /// The default trace category for this event kind.
    pub fn default_category(&self) -> TraceCategory {
        match self {
            EventKind::ThinkingChunk { .. } => TraceCategory::Thinking,
            EventKind::LlmRequest { .. } => TraceCategory::LlmExchanges,
            EventKind::ToolCallOutput { .. } => TraceCategory::ToolCalls,
            EventKind::MemoryQuery { .. } | EventKind::MemoryRetrieved { .. } => {
                TraceCategory::MemoryRetrieval
            }
            EventKind::SkillStarted { .. } | EventKind::SkillCompleted { .. } => {
                TraceCategory::SkillExecution
            }
            EventKind::PromptAssembled { .. } => TraceCategory::PromptAssembly,
            EventKind::GuardEvaluated { .. } => TraceCategory::GuardEvaluation,
            _ => TraceCategory::ContextAssembly,
        }
    }
}

// ─── EventLog ─────────────────────────────────────────────────────────────────

/// Append-only causal event log.
#[derive(Debug)]
pub struct EventLog {
    events: Vec<TurnEvent>,
    next_id: u64,
    start: Instant,
    pub min_level: TraceLevel,
    enabled_categories: Vec<TraceCategory>,
}

impl EventLog {
    pub fn new() -> Self {
        Self {
            events: Vec::new(),
            next_id: 0,
            start: Instant::now(),
            min_level: TraceLevel::Info,
            enabled_categories: Vec::new(),
        }
    }

    pub fn with_min_level(min_level: TraceLevel) -> Self {
        Self {
            events: Vec::new(),
            next_id: 0,
            start: Instant::now(),
            min_level,
            enabled_categories: Vec::new(),
        }
    }

    pub fn emit(&mut self, kind: EventKind, caused_by: Option<EventId>) -> Option<EventId> {
        let level = kind.default_level();
        if level > self.min_level {
            return None;
        }
        if !self.enabled_categories.is_empty() {
            let cat = kind.default_category();
            if !self.enabled_categories.contains(&TraceCategory::All)
                && !self.enabled_categories.contains(&cat)
            {
                return None;
            }
        }
        let id = EventId(self.next_id);
        self.next_id += 1;
        let span_id = tracing::Span::current().id();
        let canonical_event_id = Uuid::now_v7();
        let event = TurnEvent {
            id,
            kind,
            caused_by,
            elapsed_ms: self.start.elapsed().as_millis() as u64,
            span_id,
            canonical_event_id,
        };
        self.events.push(event);
        Some(id)
    }

    pub fn events(&self) -> &[TurnEvent] {
        &self.events
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

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
        chain.reverse();
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
        let mut log = EventLog::with_min_level(TraceLevel::Trace);
        assert!(log.is_empty());

        let id1 = log
            .emit(
                EventKind::PhaseTransition {
                    from: "Perceive".into(),
                    to: "Plan".into(),
                },
                None,
            )
            .unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(id1, EventId(0));

        let id2 = log
            .emit(
                EventKind::ToolsSelected {
                    tools: vec!["bash".into()],
                    confidence: 0.8,
                    boost_terms: vec![],
                },
                Some(id1),
            )
            .unwrap();
        assert_eq!(id2, EventId(1));
    }

    #[test]
    fn causal_chain_traces_correctly() {
        let mut log = EventLog::with_min_level(TraceLevel::Trace);
        let e0 = log
            .emit(
                EventKind::IntentDetected {
                    signals: vec!["is_fetch".into()],
                    confidence: 0.7,
                },
                None,
            )
            .unwrap();
        let e1 = log
            .emit(
                EventKind::ToolsSelected {
                    tools: vec!["github".into()],
                    confidence: 0.7,
                    boost_terms: vec![],
                },
                Some(e0),
            )
            .unwrap();
        let e2 = log
            .emit(
                EventKind::ToolCallStarted {
                    call_id: "c1".into(),
                    tool_name: "github".into(),
                },
                Some(e1),
            )
            .unwrap();
        let e3 = log
            .emit(
                EventKind::ToolCallCompleted {
                    call_id: "c1".into(),
                    tool_name: "github".into(),
                    duration_ms: 150,
                    success: false,
                    error: Some("404".into()),
                },
                Some(e2),
            )
            .unwrap();

        let root = log.root_cause(e3);
        assert_eq!(root, e0);

        let chain = log.causal_chain(e3);
        assert_eq!(chain.len(), 4);
    }

    #[test]
    fn event_ids_monotonic() {
        let mut log = EventLog::with_min_level(TraceLevel::Trace);
        let ids: Vec<EventId> = (0..5)
            .map(|_| {
                log.emit(EventKind::LlmChunk { text: "hi".into() }, None)
                    .unwrap()
            })
            .collect();
        for i in 1..ids.len() {
            assert!(ids[i].0 > ids[i - 1].0);
        }
    }
}
