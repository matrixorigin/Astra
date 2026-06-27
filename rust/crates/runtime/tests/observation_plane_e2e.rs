//! E2E tests for the Observation Plane — end-to-end data flow verification.
//!
//! Covers the complete pipeline from tool execution through observation
//! dispatch, journal persistence, inspection enrichment, reflect fallback,
//! and adaptive tuning signal generation.
//!
//! Data-flow paths tested:
//!   e3 — FileObservationStore + FileTuningSink persistence round-trip
//!   e4 — InspectionService enrichment + live metrics in snapshots
//!   e5 — Journal facts → RuntimePolicy.decide() → framework actions
//!   e6 — Reflect local fallback from journal + snapshot
//!   e7 — Unhappy paths (empty journal, corrupt file, failing sinks)

use std::sync::Arc;

use astra_core::observation::{ObservationFacet, TuningJob, TuningSignalType, TurnMetrics};
use astra_core::observation_journal::{
    BudgetSnapshot, JournalFacts, ObservationJournal, ObservationStore, PerformanceSnapshot,
    StoredEntry, StreakSnapshot, TaskSnapshot, TuningStore,
};
use astra_runtime::turn::agentic_loop::host;
use astra_runtime::turn::inspection_service::{local_reflect_from_snapshot, InspectionService};
use astra_runtime::turn::local_provider::LocalSessionProvider;
use astra_runtime::turn::observation_dispatcher::{
    FileSink, FileTuningSink, MemorySink, ObservationDispatcher, ObservationEvent, ObservationSink,
    TuningSink,
};
use astra_runtime::turn::observation_store::FileObservationStore;
use astra_runtime::turn::providers::{LiveRuntimeProvider, ObservationProvider};
use astra_runtime::turn::runtime_policy::{FrameworkAction, RuntimePolicy, TuningPolicy};
use astra_turn_core::introspect::{IntrospectSnapshot, StallSnapshotSummary};

// ─── Helpers ─────────────────────────────────────────────────────────────

fn temp_store() -> (
    tempfile::TempDir,
    FileObservationStore,
    Arc<FileObservationStore>,
) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = FileObservationStore::new(dir.path().to_path_buf());
    let arc: Arc<FileObservationStore> =
        Arc::new(FileObservationStore::new(dir.path().to_path_buf()));
    (dir, store, arc)
}

fn make_provider<'a>(state: &'a host::AgenticLoopState) -> LocalSessionProvider<'a> {
    LocalSessionProvider::new(state)
}

fn make_turn_metrics(errors: u32, tools: u32, mutations: u32) -> TurnMetrics {
    TurnMetrics {
        rounds_completed: 1,
        tool_calls_total: tools,
        mutation_count: mutations,
        error_count: errors,
        cache_hits: 3,
        tokens_consumed: 1500,
        ..Default::default()
    }
}

fn populate_journal(journal: &mut ObservationJournal, entries: u32) {
    for _ in 0..entries {
        journal.record_turn(&make_turn_metrics(0, 5, 2));
    }
}

/// Build a state with high token pressure by adding fake messages.
/// `introspect_token_pressure` uses `estimate_tokens(&state.messages)`
/// divided by `max_turn_input_tokens`. The `estimate_str_tokens` function
/// uses a 2-byte divisor for JSON-like content (starts with `{`), so we
/// use a JSON-object-prefixed large string to reach >80% pressure
/// efficiently.
fn make_high_pressure_state() -> host::AgenticLoopState {
    let mut state = host::make_test_loop_state();
    state.max_turn_input_tokens = 100_000;
    // ~200K JSON-prefixed chars at 2 bytes/token ≈ 100K tokens of content,
    // plus DEFAULT_SYSTEM_PROMPT_TOKENS (14K) → well over 80% pressure.
    let big_msg = format!("{{\"data\":\"{}\"}}", "x".repeat(200_000));
    state.messages = vec![
        serde_json::json!({"role": "system", "content": big_msg}),
        serde_json::json!({"role": "user", "content": "test"}),
    ];
    state
}

// ═══════════════════════════════════════════════════════════════════════════
// e3 — FileObservationStore + FileTuningSink persistence
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn e2e_store_save_and_load_round_trip() {
    let sid = "e2e-rtt-1";
    let (_dir, store, _arc) = temp_store();

    let metrics = make_turn_metrics(0, 5, 2);
    let facts = JournalFacts::default();
    store.save_entry(sid, 1, &metrics, &facts).expect("save");

    let entries: Vec<StoredEntry> = store.load_entries(sid);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].session_id, sid);
    assert!(!entries[0].metrics_json.is_empty());
}

#[test]
fn e2e_store_multiple_entries_preserve_order() {
    let sid = "e2e-order";
    let (_dir, store, _arc) = temp_store();

    let metrics = make_turn_metrics(0, 3, 1);
    for _ in 0..10 {
        store
            .save_entry(sid, 1, &metrics, &JournalFacts::default())
            .expect("save");
    }

    let entries: Vec<StoredEntry> = store.load_entries(sid);
    assert_eq!(entries.len(), 10);
}

#[test]
fn e2e_store_empty_session_returns_empty() {
    let (_dir, store, _arc) = temp_store();
    let entries: Vec<StoredEntry> = store.load_entries("no-such-session");
    assert!(entries.is_empty());
}

#[test]
fn e2e_store_delete_removes_file() {
    let sid = "e2e-delete";
    let (_dir, store, _arc) = temp_store();

    store
        .save_entry(
            sid,
            1,
            &make_turn_metrics(0, 1, 0),
            &JournalFacts::default(),
        )
        .expect("save");
    assert_eq!(store.entry_count(sid), 1);

    store.delete_session(sid).expect("delete");
    assert_eq!(store.entry_count(sid), 0);
}

#[test]
fn e2e_tuning_sink_persists_jobs() {
    let sid = "e2e-tuning";
    let (dir, store, _arc) = temp_store();

    let jobs = vec![
        TuningJob {
            signal: TuningSignalType::PromptCompaction,
            trigger_value: 0.85,
            reason: "high token pressure".to_string(),
            created_at_ms: 1000,
            turn_index: 1,
            session_id: sid.to_string(),
            priority: 7,
        },
        TuningJob {
            signal: TuningSignalType::CircuitBreakerTuning,
            trigger_value: 0.60,
            reason: "error rate spike".to_string(),
            created_at_ms: 2000,
            turn_index: 2,
            session_id: sid.to_string(),
            priority: 9,
        },
    ];
    let raw_json = serde_json::to_string(&jobs).expect("serialize jobs");
    store
        .save_tuning_entry(sid, 1, &raw_json)
        .expect("save_tuning");

    // FileObservationStore uses `{safe}.tuning.jsonl` (dot, not hyphen)
    let safe = sid.replace('/', "_").replace("..", "_");
    let tuning_path = dir.path().join(format!("{safe}.tuning.jsonl"));
    let content = std::fs::read_to_string(&tuning_path).expect("read tuning file");
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(lines.len(), 1, "should have 1 JSONL line (array of 2 jobs)");
    assert!(lines[0].contains("prompt_compaction") || lines[0].contains("PromptCompaction"));
    assert!(
        lines[0].contains("circuit_breaker_tuning") || lines[0].contains("CircuitBreakerTuning")
    );
    drop(store);
    drop(dir);
}

#[test]
fn e2e_tuning_sink_empty_batch_is_noop() {
    let sid = "e2e-tuning-empty";
    let (dir, store, _arc) = temp_store();
    store.save_tuning_entry(sid, 0, "").expect("empty save");
    let safe = sid.replace('/', "_").replace("..", "_");
    let tuning_path = dir.path().join(format!("{safe}.tuning.jsonl"));
    // With empty raw_json, save_tuning_entry writes only a newline.
    // Accept either: file doesn't exist, or file is empty/just whitespace.
    if tuning_path.exists() {
        let content = std::fs::read_to_string(&tuning_path).unwrap_or_default();
        assert!(
            content.trim().is_empty(),
            "tuning file for empty batch should be empty, got: {:?}",
            content
        );
    }
    drop(store);
    drop(dir);
}

// ═══════════════════════════════════════════════════════════════════════════
// e4 — InspectionService enrichment + introspect snapshot
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn e2e_inspection_enriches_snapshot_with_live_metrics() {
    let mut state = host::make_test_loop_state();
    state.observation_journal = ObservationJournal::default();
    populate_journal(&mut state.observation_journal, 5);
    state.total_cache_read = 750;
    state.total_prompt = 200;
    state.total_cache_creation = 50;

    let provider = make_provider(&state);
    let service = InspectionService::new(&provider, &provider, &provider);

    let mut snapshot = IntrospectSnapshot::default();
    service.enrich_snapshot(&mut snapshot);

    // enrich_snapshot sets individual fields directly
    assert!(
        (snapshot.cache_hit_ratio - 0.75).abs() < 0.001,
        "cache_hit_ratio should be ~0.75, got {}",
        snapshot.cache_hit_ratio
    );
    assert!(
        snapshot.circuit_breaker.is_some(),
        "circuit_breaker should be set"
    );
}

#[test]
fn e2e_inspection_live_metrics_reflects_error_rate() {
    let mut state = host::make_test_loop_state();
    state.observation_journal = ObservationJournal::default();
    for t in 1..=6 {
        let errors = if t % 2 == 0 { 1u32 } else { 0u32 };
        let metrics = TurnMetrics {
            error_count: errors,
            tool_calls_total: 5,
            ..Default::default()
        };
        state.observation_journal.record_turn(&metrics);
    }

    let provider = make_provider(&state);
    let metrics = InspectionService::new(&provider, &provider, &provider).build_live_metrics();

    assert!(!metrics.current_error_rate.is_nan());
    assert_eq!(metrics.turns_remaining, 10);
}

#[test]
fn e2e_inspection_enrich_preserves_existing_snapshot_fields() {
    let state = host::make_test_loop_state();
    let provider = make_provider(&state);
    let service = InspectionService::new(&provider, &provider, &provider);

    let mut snapshot = IntrospectSnapshot {
        turns_completed: 42,
        compaction_tier: "custom-tier".to_string(),
        ..Default::default()
    };
    service.enrich_snapshot(&mut snapshot);

    assert_eq!(snapshot.turns_completed, 42);
    assert_eq!(snapshot.compaction_tier, "custom-tier");
}

#[test]
fn e2e_inspection_produces_non_empty_alerts_on_high_pressure() {
    let state = make_high_pressure_state();

    let provider = make_provider(&state);
    let metrics = InspectionService::new(&provider, &provider, &provider).build_live_metrics();

    assert!(
        !metrics.alerts.is_empty(),
        "should have at least high_token_pressure alert"
    );
    assert!(
        metrics
            .alerts
            .iter()
            .any(|a| a.contains("high_token_pressure")),
        "alerts should contain high_token_pressure: {:?}",
        metrics.alerts
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// e5 — Journal facts → RuntimePolicy.decide() → framework actions
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn e2e_policy_decide_signals_context_pressure_on_high_token_pressure() {
    // Test decide() directly with explicit facts — token_pressure > 0.70 threshold
    let facts = JournalFacts {
        performance: PerformanceSnapshot {
            token_pressure: 0.85,
            ..Default::default()
        },
        budget: BudgetSnapshot {
            budget_remaining: 10,
            budget_max: 10,
            ..Default::default()
        },
        ..Default::default()
    };
    let policy = RuntimePolicy::default();
    let decisions = policy.decide(&facts);

    let has_pressure_signal = decisions
        .iter()
        .any(|d| matches!(d, FrameworkAction::SignalContextPressure { .. }));
    assert!(
        has_pressure_signal,
        "expected SignalContextPressure at 85% pressure, got: {:?}",
        decisions
    );
}

#[test]
fn e2e_policy_decide_aggressive_context_pressure_on_critical_pressure() {
    let facts = JournalFacts {
        performance: PerformanceSnapshot {
            token_pressure: 0.92,
            ..Default::default()
        },
        budget: BudgetSnapshot {
            budget_remaining: 10,
            budget_max: 10,
            ..Default::default()
        },
        ..Default::default()
    };
    let policy = RuntimePolicy::default();
    let decisions = policy.decide(&facts);

    let has_aggressive = decisions.iter().any(|d| {
        matches!(
            d,
            FrameworkAction::SignalContextPressure {
                urgency: astra_runtime::turn::runtime_policy::ContextPressureUrgency::Aggressive
            }
        )
    });
    assert!(
        has_aggressive,
        "expected aggressive context-pressure signal at 92% pressure, got: {:?}",
        decisions
    );
}

#[test]
fn e2e_policy_decide_continue_when_healthy() {
    let facts = JournalFacts {
        budget: BudgetSnapshot {
            budget_remaining: 10,
            budget_max: 10,
            ..Default::default()
        },
        ..Default::default()
    };
    let policy = RuntimePolicy::default();
    let decisions = policy.decide(&facts);

    let has_aggressive = decisions
        .iter()
        .any(|d| matches!(d, FrameworkAction::SignalContextPressure { .. }));
    assert!(
        !has_aggressive,
        "healthy state should not trigger aggressive actions: {:?}",
        decisions
    );
}

#[test]
fn e2e_policy_decide_guidance_on_high_error_rate() {
    let facts = JournalFacts {
        performance: PerformanceSnapshot {
            current_error_rate: 0.45,
            ..Default::default()
        },
        budget: BudgetSnapshot {
            budget_remaining: 10,
            budget_max: 10,
            ..Default::default()
        },
        ..Default::default()
    };
    let policy = RuntimePolicy::default();
    let decisions = policy.decide(&facts);

    let has_guidance = decisions
        .iter()
        .any(|d| matches!(d, FrameworkAction::InjectSignal { message } if message.contains("tool error rate")));
    assert!(
        has_guidance,
        "expected guidance signal for 45% error rate, got: {:?}",
        decisions
    );
}

#[test]
fn e2e_policy_decide_guidance_on_read_only_streak() {
    let facts = JournalFacts {
        streaks: StreakSnapshot {
            consecutive_read_only: 9,
            ..Default::default()
        },
        budget: BudgetSnapshot {
            budget_remaining: 10,
            budget_max: 10,
            ..Default::default()
        },
        ..Default::default()
    };
    let policy = RuntimePolicy::default();
    let decisions = policy.decide(&facts);

    let has_guidance = decisions.iter().any(
        |d| matches!(d, FrameworkAction::InjectSignal { message } if message.contains("read-only")),
    );
    assert!(
        has_guidance,
        "expected guidance signal for read-only streak, got: {:?}",
        decisions
    );
}

#[test]
fn e2e_policy_decide_transition_phase_on_completion() {
    let facts = JournalFacts {
        task: TaskSnapshot {
            task_completion_ratio: 1.0,
            ..Default::default()
        },
        budget: BudgetSnapshot {
            budget_remaining: 5,
            budget_max: 10,
            ..Default::default()
        },
        ..Default::default()
    };
    let policy = RuntimePolicy::default();
    let decisions = policy.decide(&facts);

    let has_transition = decisions.iter().any(|d| {
        matches!(
            d,
            FrameworkAction::TransitionPhase {
                target: astra_runtime::turn::runtime_policy::PhaseTarget::Completion
            }
        )
    });
    assert!(
        has_transition,
        "expected TransitionPhase::Completion, got: {:?}",
        decisions
    );
}

#[test]
fn e2e_policy_decide_expand_budget_on_outcome_streak() {
    let facts = JournalFacts {
        streaks: StreakSnapshot {
            consecutive_rounds_with_outcome: 3,
            ..Default::default()
        },
        budget: BudgetSnapshot {
            budget_remaining: 2,
            budget_max: 10,
            ..Default::default()
        },
        ..Default::default()
    };
    let policy = RuntimePolicy::default();
    let decisions = policy.decide(&facts);

    let has_expand = decisions
        .iter()
        .any(|d| matches!(d, FrameworkAction::ExpandBudget { .. }));
    assert!(
        has_expand,
        "expected ExpandBudget for outcome streak with tight budget, got: {:?}",
        decisions
    );
}

#[test]
fn e2e_inspection_generates_tuning_jobs_from_signals() {
    let state = make_high_pressure_state();

    let provider = make_provider(&state);
    let service = InspectionService::new(&provider, &provider, &provider);

    let jobs = service.generate_tuning_signals(10, "e2e-tuning-test", &TuningPolicy::default());
    assert!(
        !jobs.is_empty(),
        "should generate at least one tuning signal, got 0"
    );
    let has_compaction = jobs.iter().any(|j| {
        matches!(
            j.signal,
            TuningSignalType::PromptCompaction | TuningSignalType::AggressiveCompaction
        )
    });
    assert!(
        has_compaction,
        "should include compaction signal: {:?}",
        jobs.iter().map(|j| j.signal).collect::<Vec<_>>()
    );
}

#[test]
fn e2e_tuning_jobs_persist_through_store() {
    let sid = "e2e-dispatch-tuning";
    let (dir, _store, arc) = temp_store();

    let state = make_high_pressure_state();

    let provider = make_provider(&state);
    let service = InspectionService::new(&provider, &provider, &provider);
    let jobs = service.generate_tuning_signals(5, sid, &TuningPolicy::default());

    if !jobs.is_empty() {
        let raw_json = serde_json::to_string(&jobs).expect("serialize");
        arc.save_tuning_entry(sid, 5, &raw_json)
            .expect("save_tuning");
        let safe = sid.replace('/', "_").replace("..", "_");
        let tuning_path = dir.path().join(format!("{safe}.tuning.jsonl"));
        let content = std::fs::read_to_string(&tuning_path).expect("read tuning");
        assert!(content.lines().count() >= 1);
    }
    drop(dir);
}

// ═══════════════════════════════════════════════════════════════════════════
// e6 — Reflect local fallback
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn e2e_reflect_local_session_summary() {
    let mut state = host::make_test_loop_state();
    state.observation_journal = ObservationJournal::default();
    populate_journal(&mut state.observation_journal, 8);

    let provider = make_provider(&state);
    let service = InspectionService::new(&provider, &provider, &provider);

    let summary = service.local_reflect_summary(ObservationFacet::Session, 10);
    assert!(!summary.is_empty(), "summary should not be empty");
    assert!(
        summary.contains("session") || summary.contains("turn") || summary.contains("entry"),
        "summary should describe session: {}",
        summary
    );
}

#[test]
fn e2e_reflect_local_errors_summary() {
    let mut state = host::make_test_loop_state();
    state.observation_journal = ObservationJournal::default();
    for _ in 0..5 {
        let metrics = TurnMetrics {
            error_count: 2,
            tool_calls_total: 5,
            ..Default::default()
        };
        state.observation_journal.record_turn(&metrics);
    }

    let provider = make_provider(&state);
    let service = InspectionService::new(&provider, &provider, &provider);

    let summary = service.local_reflect_summary(ObservationFacet::Errors, 10);
    assert!(!summary.is_empty(), "error summary should not be empty");
}

#[test]
fn e2e_reflect_from_snapshot_session_facet() {
    let snapshot = IntrospectSnapshot {
        turns_completed: 5,
        turns_remaining: 10,
        token_pressure: 0.3,
        cache_hit_ratio: 0.75,
        compaction_tier: "standard".to_string(),
        circuit_breaker: Some(astra_turn_core::introspect::CircuitBreakerSnapshot {
            state: "armed".to_string(),
            failure_count: 0,
            success_count: 0,
            consecutive_failures: 0,
        }),
        ..Default::default()
    };

    let summary = local_reflect_from_snapshot(&snapshot, ObservationFacet::Session);
    assert!(!summary.is_empty());
    assert!(
        summary.contains("turns"),
        "summary should mention turns: {}",
        summary
    );
}

#[test]
fn e2e_reflect_from_snapshot_stall_facet() {
    let snapshot = IntrospectSnapshot {
        stall_state: StallSnapshotSummary {
            nudge_count: 5,
            events: vec!["stall detected".to_string()],
            ..Default::default()
        },
        ..Default::default()
    };

    let summary = local_reflect_from_snapshot(&snapshot, ObservationFacet::Stall);
    assert!(!summary.is_empty());
    assert!(
        summary.contains("stall"),
        "stall facet should mention stall: {}",
        summary
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// e7 — Unhappy paths
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn e2e_empty_journal_produces_safe_defaults() {
    let state = host::make_test_loop_state();
    let provider = make_provider(&state);

    assert!(provider.journal_is_empty());
    assert_eq!(provider.journal_len(), 0);

    let facts = provider.extract_facts();
    assert_eq!(facts.streaks.consecutive_rounds_with_outcome, 0);
    assert_eq!(facts.streaks.consecutive_rounds_without_outcome, 0);
    assert!((facts.performance.current_error_rate - 0.0).abs() < f64::EPSILON);
    assert!((facts.performance.cache_hit_ratio - 0.0).abs() < f64::EPSILON);

    let service = InspectionService::new(&provider, &provider, &provider);
    let metrics = service.build_live_metrics();
    assert!(!metrics.current_error_rate.is_nan());
    assert!(!metrics.cache_hit_ratio.is_nan());
    assert!(!metrics.token_pressure.is_nan());
}

#[test]
fn e2e_corrupt_jsonl_line_is_skipped() {
    let sid = "e2e-corrupt";
    let dir = tempfile::tempdir().expect("tempdir");
    let store = FileObservationStore::new(dir.path().to_path_buf());

    store
        .save_entry(
            sid,
            1,
            &make_turn_metrics(0, 3, 1),
            &JournalFacts::default(),
        )
        .expect("save");
    let path = dir.path().join(format!("{}.jsonl", sid.replace('/', "_")));
    std::fs::write(&path, "this is not json\n{\"session_id\":\"valid\"}\n").expect("write corrupt");
    store
        .save_entry(
            sid,
            3,
            &make_turn_metrics(0, 3, 1),
            &JournalFacts::default(),
        )
        .expect("save3");

    let entries: Vec<StoredEntry> = store.load_entries(sid);
    assert!(!entries.is_empty(), "should parse at least one valid entry");
    drop(store);
    drop(dir);
}

#[test]
fn e2e_reflect_empty_snapshot_returns_safe_message() {
    let snapshot = IntrospectSnapshot::default();
    let summary = local_reflect_from_snapshot(&snapshot, ObservationFacet::Session);
    assert!(!summary.is_empty(), "should return a message, not panic");
}

#[test]
fn e2e_dispatcher_fan_out_to_multiple_sinks() {
    let mut journal_a = ObservationJournal::default();
    let mut journal_b = ObservationJournal::default();
    let (_dir, store, _arc) = temp_store();

    {
        let mut dispatcher = ObservationDispatcher::new();
        dispatcher.register(MemorySink::new(&mut journal_a));
        dispatcher.register(MemorySink::new(&mut journal_b));
        dispatcher.register(FileSink::new(
            Some(Arc::new(store)),
            "e2e-fanout".to_string(),
        ));

        let event = ObservationEvent::TurnCompleted {
            metrics: make_turn_metrics(0, 5, 2),
            facts: JournalFacts::default(),
        };
        dispatcher.dispatch(event);
        assert_eq!(dispatcher.event_count(), 1);
        assert_eq!(dispatcher.failure_count(), 0);
        assert_eq!(dispatcher.sink_count(), 3);
    } // dispatcher dropped here, releasing borrows

    assert_eq!(journal_a.len(), 1);
    assert_eq!(journal_b.len(), 1);
}

#[test]
fn e2e_dispatcher_tolerates_failing_sink_and_continues() {
    let mut journal = ObservationJournal::default();

    struct FailingSink;
    impl ObservationSink for FailingSink {
        fn name(&self) -> &'static str {
            "failing"
        }
        fn consume(&mut self, _event: &ObservationEvent) -> Result<(), String> {
            Err("simulated failure".to_string())
        }
    }

    {
        let mut dispatcher = ObservationDispatcher::new();
        dispatcher.register(FailingSink);
        dispatcher.register(MemorySink::new(&mut journal));

        let event = ObservationEvent::TurnCompleted {
            metrics: make_turn_metrics(0, 3, 1),
            facts: JournalFacts::default(),
        };
        dispatcher.dispatch(event);
        assert_eq!(dispatcher.failure_count(), 1);
    } // drop dispatcher

    assert_eq!(
        journal.len(),
        1,
        "MemorySink should still receive the event"
    );
}

#[test]
fn e2e_dispatcher_all_event_variants_flow_to_sinks() {
    let mut journal = ObservationJournal::default();

    {
        let mut dispatcher = ObservationDispatcher::new();
        dispatcher.register(MemorySink::new(&mut journal));

        dispatcher.dispatch(ObservationEvent::TurnCompleted {
            metrics: make_turn_metrics(0, 5, 2),
            facts: JournalFacts::default(),
        });
        dispatcher.dispatch(ObservationEvent::TurnCompleted {
            metrics: make_turn_metrics(1, 3, 0),
            facts: JournalFacts::default(),
        });
        dispatcher.dispatch(ObservationEvent::PhaseTransition {
            from: "execution",
            to: "reflection",
        });

        assert_eq!(dispatcher.event_count(), 3);
        assert_eq!(dispatcher.failure_count(), 0);
    } // drop dispatcher

    assert_eq!(journal.len(), 2);
}

#[test]
fn e2e_session_id_sanitization_prevents_path_traversal() {
    let malicious_sid = "../../etc/passwd";
    let dir = tempfile::tempdir().expect("tempdir");
    let store = FileObservationStore::new(dir.path().to_path_buf());

    store
        .save_entry(
            malicious_sid,
            1,
            &make_turn_metrics(0, 1, 0),
            &JournalFacts::default(),
        )
        .expect("save");
    let entries: Vec<StoredEntry> = store.load_entries(malicious_sid);
    assert!(!entries.is_empty());

    assert!(!dir.path().join("../../etc/passwd.jsonl").exists());
    let has_files = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .count()
        > 0;
    assert!(has_files, "file should exist within the intended directory");
    drop(store);
    drop(dir);
}

#[test]
fn e2e_none_observation_store_is_graceful() {
    let mut journal = ObservationJournal::default();

    {
        let mut dispatcher = ObservationDispatcher::new();
        dispatcher.register(MemorySink::new(&mut journal));
        dispatcher.register(FileSink::new(None, "no-store-session".to_string()));

        dispatcher.dispatch(ObservationEvent::TurnCompleted {
            metrics: make_turn_metrics(0, 4, 2),
            facts: JournalFacts::default(),
        });
        assert_eq!(dispatcher.failure_count(), 0);
    } // drop dispatcher

    assert_eq!(journal.len(), 1);
}

#[test]
fn e2e_file_tuning_sink_none_store_skips() {
    let mut sink = FileTuningSink::new(None, "no-store".to_string());
    let jobs = vec![TuningJob {
        signal: TuningSignalType::PromptCompaction,
        trigger_value: 0.5,
        reason: "test".to_string(),
        created_at_ms: 1000,
        turn_index: 1,
        session_id: "no-store".to_string(),
        priority: 5,
    }];
    let result = sink.consume_batch(&jobs);
    assert!(
        result.is_ok(),
        "None store should not error: {:?}",
        result.err()
    );
}

#[test]
fn e2e_inspection_without_tool_calls_produces_zero_error_rate() {
    let mut state = host::make_test_loop_state();
    state.observation_journal = ObservationJournal::default();
    for _ in 0..3 {
        let metrics = TurnMetrics {
            tool_calls_total: 0,
            error_count: 0,
            ..Default::default()
        };
        state.observation_journal.record_turn(&metrics);
    }

    let provider = make_provider(&state);
    let facts = provider.extract_facts();
    assert!(!facts.performance.current_error_rate.is_nan());
    assert!((facts.performance.current_error_rate - 0.0).abs() < f64::EPSILON);
}

#[test]
fn e2e_policy_respects_context_pressure_policy_thresholds() {
    // Use explicit facts — token_pressure=0.60 with custom threshold 0.50
    let facts = JournalFacts {
        performance: PerformanceSnapshot {
            token_pressure: 0.60,
            ..Default::default()
        },
        budget: BudgetSnapshot {
            budget_remaining: 10,
            budget_max: 10,
            ..Default::default()
        },
        ..Default::default()
    };

    let mut policy = RuntimePolicy::default();
    policy.context_pressure.pressure_threshold = 0.5;

    let decisions = policy.decide(&facts);
    let has_pressure_signal = decisions
        .iter()
        .any(|d| matches!(d, FrameworkAction::SignalContextPressure { .. }));
    assert!(
        has_pressure_signal,
        "custom threshold 0.5 should trigger at 60%: {:?}",
        decisions
    );
}

#[test]
fn e2e_policy_circuit_breaker_respects_custom_max_errors() {
    let mut state = host::make_test_loop_state();
    state.observation_journal = ObservationJournal::default();
    for _ in 0..3 {
        let metrics = TurnMetrics {
            mutation_count: 0,
            error_count: 1,
            tool_calls_total: 3,
            ..Default::default()
        };
        state.observation_journal.record_turn(&metrics);
    }

    let mut policy = RuntimePolicy::default();
    policy.circuit.max_consecutive_errors = 2;

    let provider = make_provider(&state);
    let mut facts = provider.extract_facts();
    facts.performance.token_pressure = provider.token_pressure();

    let decisions = policy.decide(&facts);
    let has_adjustment = decisions.iter().any(|d| {
        matches!(d, FrameworkAction::InjectSignal { message } if message.contains("Circuit-breaker risk") || message.contains("error"))
    });
    assert!(
        has_adjustment,
        "custom max_errors=2 should trigger at 3: {:?}",
        decisions
    );
}
