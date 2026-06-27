//! Observation Dispatcher — unified Event→Router→Executor→Sinks pipeline.
//!
//! # Design
//!
//! The dispatcher decouples observation event *production* (in the agentic loop)
//! from event *consumption* (journal recording, store persistence, trend
//! computation). It implements a synchronous pipeline (no channel needed for
//! single-threaded turn processing):
//!
//! ```text
//! agentic loop                     dispatcher
//! ────────────                     ──────────
//! tool_phase produces event ───→ dispatch(event)
//!                                     │
//!                                     ├─ MemorySink: journal.record_turn()
//!                                     ├─ FileSink:   store.save_entry()
//!                                     └─ (future) CloudSink
//! ```
//!
//! # Unhappy-path guarantees
//!
//! * **Sink failure is non-fatal** — if one sink fails (e.g. disk full), the
//!   dispatcher logs the error and continues to the next sink.
//! * **No unwind** — dispatch() never panics on malformed events or missing data.
//! * **Empty store** — when `store` is `None`, the file sink is simply skipped.

use std::sync::Arc;

use astra_core::observation::{TuningJob, TurnMetrics};
use astra_core::observation_journal::{
    JournalFacts, ObservationJournal, ObservationStore, TuningStore,
};

use super::runtime_policy::FrameworkAction;

// ── ObservationEvent ────────────────────────────────────────────────────────

/// Events emitted by the agentic loop that the observation plane consumes.
///
/// Each variant carries enough data for all registered sinks to operate
/// without further state lookups.
#[derive(Debug, Clone)]
pub enum ObservationEvent {
    /// Emitted after every tool phase completes with tool call samples.
    TurnCompleted {
        /// The computed metrics for this turn.
        metrics: TurnMetrics,
        /// Journal facts extracted from the updated journal.
        facts: JournalFacts,
    },

    /// Emitted when the runtime policy produces a framework action.
    PolicyDecision {
        /// The action decided by [`RuntimePolicy::decide`].
        action: FrameworkAction,
    },

    /// Emitted when the agentic loop transitions between phases.
    PhaseTransition {
        /// Phase label before the transition.
        from: &'static str,
        /// Phase label after the transition.
        to: &'static str,
    },
}

// ── Sink trait ───────────────────────────────────────────────────────────────

/// A named destination that consumes observation events.
///
/// Implementations are stateless (or hold references to shared state).
/// Sinks are invoked in registration order.
pub trait ObservationSink {
    /// Human-readable name for logging and diagnostics.
    fn name(&self) -> &'static str;

    /// Consume an observation event.
    ///
    /// Returns `Ok(())` on success, or an error message on failure.
    /// Errors are informational; the dispatcher never unwinds on sink failure.
    fn consume(&mut self, event: &ObservationEvent) -> Result<(), String>;
}

// ── Memory Sink ─────────────────────────────────────────────────────────────

/// Records [`TurnMetrics`] into the in-memory [`ObservationJournal`].
#[derive(Debug)]
pub struct MemorySink<'j> {
    journal: &'j mut ObservationJournal,
}

impl<'j> MemorySink<'j> {
    pub fn new(journal: &'j mut ObservationJournal) -> Self {
        Self { journal }
    }
}

impl ObservationSink for MemorySink<'_> {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn consume(&mut self, event: &ObservationEvent) -> Result<(), String> {
        if let ObservationEvent::TurnCompleted { metrics, .. } = event {
            self.journal.record_turn(metrics);
        }
        Ok(())
    }
}

// ── File Sink ───────────────────────────────────────────────────────────────

/// Persists turn metrics + facts to an [`ObservationStore`] backend.
pub struct FileSink {
    store: Option<Arc<dyn ObservationStore>>,
    session_id: String,
}

impl std::fmt::Debug for FileSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileSink")
            .field("session_id", &self.session_id)
            .field("has_store", &self.store.is_some())
            .finish()
    }
}

impl FileSink {
    pub fn new(store: Option<Arc<dyn ObservationStore>>, session_id: String) -> Self {
        Self { store, session_id }
    }
}

impl ObservationSink for FileSink {
    fn name(&self) -> &'static str {
        "file"
    }

    fn consume(&mut self, event: &ObservationEvent) -> Result<(), String> {
        let store = match &self.store {
            Some(s) => s,
            None => return Ok(()), // No store configured — skip silently.
        };

        if let ObservationEvent::TurnCompleted { metrics, facts, .. } = event {
            store.save_entry(&self.session_id, metrics.rounds_completed, metrics, facts)?;
        }
        Ok(())
    }
}

// ── Tuning Sink ─────────────────────────────────────────────────────────────

/// Consumes [`TuningJob`] entries and writes them to a persistent store.
///
/// Unlike [`ObservationSink`] which handles real-time turn events, `TuningSink`
/// handles *derived* tuning signals generated after analysis. Sinks are
/// fire-and-forget: failure is logged but never propagated.
pub trait TuningSink {
    /// Human-readable name for logging and diagnostics.
    fn name(&self) -> &'static str;

    /// Consume a batch of tuning jobs.
    ///
    /// Returns `Ok(())` on success, or an error message on failure.
    /// Errors are informational; callers never unwind on sink failure.
    fn consume_batch(&mut self, jobs: &[TuningJob]) -> Result<(), String>;
}

// ── File Tuning Sink ────────────────────────────────────────────────────────

/// Persists [`TuningJob`] entries as JSON lines to a file.
///
/// The file path follows the same convention as [`FileObservationStore`]:
/// `~/.astra/observations/{session_id}.tuning.jsonl`
pub struct FileTuningSink {
    store: Option<Arc<dyn TuningStore>>,
    session_id: String,
}

impl std::fmt::Debug for FileTuningSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileTuningSink")
            .field("session_id", &self.session_id)
            .field("has_store", &self.store.is_some())
            .finish()
    }
}

impl FileTuningSink {
    pub fn new(store: Option<Arc<dyn TuningStore>>, session_id: String) -> Self {
        Self { store, session_id }
    }
}

impl TuningSink for FileTuningSink {
    fn name(&self) -> &'static str {
        "file_tuning"
    }

    fn consume_batch(&mut self, jobs: &[TuningJob]) -> Result<(), String> {
        let store = match &self.store {
            Some(s) => s,
            None => return Ok(()), // No store configured — skip silently.
        };

        for job in jobs {
            let json = serde_json::to_string(job)
                .map_err(|e| format!("tuning_job serialization failed: {e}"))?;
            store.save_tuning_entry(&self.session_id, job.turn_index, &json)?;
        }
        Ok(())
    }
}

// ── ObservationDispatcher ───────────────────────────────────────────────────

/// Synchronous event pipeline: receives events and fans out to registered sinks.
///
/// # Usage
///
/// ```ignore
/// let mut dispatcher = ObservationDispatcher::new();
/// dispatcher.register(MemorySink::new(&mut journal));
/// dispatcher.register(FileSink::new(store.clone(), session_id));
///
/// dispatcher.dispatch(ObservationEvent::TurnCompleted {
///     metrics: turn_metrics,
///     facts: journal_facts,
/// });
/// ```
pub struct ObservationDispatcher<'a> {
    sinks: Vec<Box<dyn ObservationSink + 'a>>,
    /// How many events have been dispatched (for diagnostics).
    event_count: u64,
    /// How many sink failures have occurred.
    failure_count: u64,
}

impl<'a> Default for ObservationDispatcher<'a> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> ObservationDispatcher<'a> {
    pub fn new() -> Self {
        Self {
            sinks: Vec::new(),
            event_count: 0,
            failure_count: 0,
        }
    }

    /// Register a sink. Sinks are invoked in registration order.
    pub fn register(&mut self, sink: impl ObservationSink + 'a) {
        self.sinks.push(Box::new(sink));
    }

    /// Dispatch an event to all registered sinks.
    ///
    /// Each sink consumes the event independently. If a sink fails, the error
    /// is logged and the dispatcher continues to the next sink. The event is
    /// counted even if some sinks failed.
    pub fn dispatch(&mut self, event: ObservationEvent) {
        self.event_count += 1;

        for sink in &mut self.sinks {
            if let Err(e) = sink.consume(&event) {
                self.failure_count += 1;
                tracing::warn!(
                    sink = sink.name(),
                    error = %e,
                    event_count = self.event_count,
                    "observation sink failure (non-fatal)"
                );
            }
        }
    }

    /// Total events dispatched.
    pub fn event_count(&self) -> u64 {
        self.event_count
    }

    /// Total sink failures across all events.
    pub fn failure_count(&self) -> u64 {
        self.failure_count
    }

    /// Number of registered sinks.
    pub fn sink_count(&self) -> usize {
        self.sinks.len()
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use astra_core::observation::TurnMetrics;

    /// A sink that records every event variant it receives.
    #[derive(Debug, Default)]
    struct RecordingSink {
        events: Vec<String>,
    }

    impl RecordingSink {
        fn new() -> Self {
            Self::default()
        }
    }

    impl ObservationSink for RecordingSink {
        fn name(&self) -> &'static str {
            "recording"
        }

        fn consume(&mut self, event: &ObservationEvent) -> Result<(), String> {
            match event {
                ObservationEvent::TurnCompleted { .. } => {
                    self.events.push("turn_completed".to_string());
                }
                ObservationEvent::PolicyDecision { .. } => {
                    self.events.push("policy_decision".to_string());
                }
                ObservationEvent::PhaseTransition { .. } => {
                    self.events.push("phase_transition".to_string());
                }
            }
            Ok(())
        }
    }

    /// A sink that always fails.
    #[derive(Debug)]
    struct FailingSink;

    impl ObservationSink for FailingSink {
        fn name(&self) -> &'static str {
            "failing"
        }

        fn consume(&mut self, _event: &ObservationEvent) -> Result<(), String> {
            Err("simulated failure".to_string())
        }
    }

    #[test]
    fn dispatcher_fans_out_to_all_sinks() {
        let mut dispatcher = ObservationDispatcher::new();
        let rec1 = RecordingSink::new();
        let rec2 = RecordingSink::new();

        dispatcher.register(rec1);
        dispatcher.register(rec2);

        dispatcher.dispatch(ObservationEvent::TurnCompleted {
            metrics: TurnMetrics::default(),
            facts: JournalFacts::default(),
        });

        assert_eq!(dispatcher.event_count(), 1);
        assert_eq!(dispatcher.failure_count(), 0);
    }

    #[test]
    fn dispatcher_tolerates_sink_failure() {
        let mut dispatcher = ObservationDispatcher::new();
        dispatcher.register(RecordingSink::new());
        dispatcher.register(FailingSink);
        dispatcher.register(RecordingSink::new());

        dispatcher.dispatch(ObservationEvent::PhaseTransition {
            from: "planning",
            to: "execution",
        });

        // The failing sink failed, but the other two succeeded.
        assert_eq!(dispatcher.event_count(), 1);
        assert_eq!(dispatcher.failure_count(), 1);
    }

    #[test]
    fn all_event_variants_reach_sinks() {
        let mut dispatcher = ObservationDispatcher::new();

        // Use Arc<RecordingSink> to read results after dispatch.
        let _sink = std::cell::RefCell::new(RecordingSink::new());

        // We can't easily extract from Box<dyn>, so let's use an alternative approach:
        // Use a shared Vec behind Arc<Mutex>.
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));

        struct SharedSink {
            events: Arc<std::sync::Mutex<Vec<String>>>,
        }

        impl std::fmt::Debug for SharedSink {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_struct("SharedSink").finish()
            }
        }

        impl ObservationSink for SharedSink {
            fn name(&self) -> &'static str {
                "shared"
            }

            fn consume(&mut self, event: &ObservationEvent) -> Result<(), String> {
                let label = match event {
                    ObservationEvent::TurnCompleted { .. } => "turn_completed",
                    ObservationEvent::PolicyDecision { .. } => "policy_decision",
                    ObservationEvent::PhaseTransition { .. } => "phase_transition",
                };
                self.events.lock().unwrap().push(label.to_string());
                Ok(())
            }
        }

        let shared = SharedSink {
            events: events.clone(),
        };
        dispatcher.register(shared);

        dispatcher.dispatch(ObservationEvent::TurnCompleted {
            metrics: TurnMetrics::default(),
            facts: JournalFacts::default(),
        });
        dispatcher.dispatch(ObservationEvent::PolicyDecision {
            action: FrameworkAction::Continue,
        });
        dispatcher.dispatch(ObservationEvent::PhaseTransition {
            from: "tool_phase",
            to: "execution_phase",
        });

        let captured = events.lock().unwrap();
        assert_eq!(
            *captured,
            vec!["turn_completed", "policy_decision", "phase_transition"]
        );
        assert_eq!(dispatcher.event_count(), 3);
    }

    #[test]
    fn empty_dispatcher_noops() {
        let mut dispatcher = ObservationDispatcher::new();
        dispatcher.dispatch(ObservationEvent::TurnCompleted {
            metrics: TurnMetrics::default(),
            facts: JournalFacts::default(),
        });
        assert_eq!(dispatcher.event_count(), 1);
        assert_eq!(dispatcher.failure_count(), 0);
    }

    // ── FileTuningSink tests ────────────────────────────────────────────

    use astra_core::observation::TuningSignalType;

    fn make_tuning_job(signal: TuningSignalType, turn: u32) -> TuningJob {
        TuningJob {
            signal,
            trigger_value: 0.85,
            reason: "test tuning signal".to_string(),
            created_at_ms: 1_700_000_000_000,
            turn_index: turn,
            session_id: "test-session".to_string(),
            priority: 5,
        }
    }

    #[test]
    fn file_tuning_sink_none_store_skips_silently() {
        let mut sink = FileTuningSink::new(None, "test-session".to_string());
        let jobs = vec![make_tuning_job(TuningSignalType::PromptCompaction, 1)];
        let result = sink.consume_batch(&jobs);
        assert!(
            result.is_ok(),
            "None store should skip silently: {:?}",
            result
        );
    }

    #[test]
    fn file_tuning_sink_empty_batch_is_noop() {
        let mut sink = FileTuningSink::new(None, "test-session".to_string());
        let result = sink.consume_batch(&[]);
        assert!(result.is_ok());
    }

    #[test]
    fn file_tuning_sink_name_is_file_tuning() {
        let sink = FileTuningSink::new(None, "test-session".to_string());
        assert_eq!(sink.name(), "file_tuning");
    }

    #[test]
    fn file_tuning_sink_debug_shows_session_and_store() {
        let sink_none = FileTuningSink::new(None, "s1".to_string());
        let dbg_none = format!("{:?}", sink_none);
        assert!(dbg_none.contains("s1"));
        assert!(dbg_none.contains("has_store: false"));

        let store = crate::turn::observation_store::test_store();
        let store_tuning: Option<Arc<dyn TuningStore>> = store.map(|s| s as Arc<dyn TuningStore>);
        let sink_some = FileTuningSink::new(store_tuning, "s2".to_string());
        let dbg_some = format!("{:?}", sink_some);
        assert!(dbg_some.contains("s2"));
        assert!(dbg_some.contains("has_store: true"));
    }

    #[test]
    fn file_tuning_sink_writes_jobs_to_temp_store() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let store: Arc<dyn TuningStore> = Arc::new(
            crate::turn::observation_store::FileObservationStore::new(dir.path().to_path_buf()),
        );
        let mut sink = FileTuningSink::new(Some(store.clone()), "tsink-session".to_string());

        let jobs = vec![
            make_tuning_job(TuningSignalType::PromptCompaction, 1),
            make_tuning_job(TuningSignalType::CacheWarming, 3),
        ];
        sink.consume_batch(&jobs).expect("write should succeed");

        // Verify file persistence — tuning entries go to .tuning.jsonl, not .jsonl
        let tuning_path = dir.path().join("tsink-session.tuning.jsonl");
        assert!(
            tuning_path.exists(),
            "tuning file should exist at {tuning_path:?}"
        );
        let raw = std::fs::read_to_string(&tuning_path).expect("read tuning file");
        assert!(
            raw.contains("PromptCompaction"),
            "missing PromptCompaction in {raw}"
        );
        assert!(
            raw.contains("CacheWarming"),
            "missing CacheWarming in {raw}"
        );
        assert_eq!(raw.lines().count(), 2, "should have 2 lines, got: {raw}");
    }
}
