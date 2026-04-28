//! E2E coverage for the SelfModel injection plumbing added in
//! `ObservabilitySession::ingest_self_model_inputs`: verifies the four
//! previously-hardcoded arguments (`skills`, `tool_health`, `scenario`,
//! `recent_signals`) reach the rendered `SelfModel`.

use astra_pipeline::ToolHealthEntry;
use astra_runtime::auto_tuning::{FeedbackSignal, SignalType};
use astra_runtime::observability_integration::ObservabilitySession;
use astra_runtime::self_model::SelfModel;
use astra_runtime::user_profile::Scenario;
use astra_turn_core::tool_health::ToolHealthTracker;

fn make_entry(name: &str, total_calls: usize, total_failures: usize) -> ToolHealthEntry {
    let failure_rate = if total_calls == 0 {
        0.0
    } else {
        total_failures as f64 / total_calls as f64
    };
    ToolHealthEntry {
        name: name.to_string(),
        total_calls,
        total_failures,
        failure_rate,
        last_updated_epoch: 0,
        recent_outcomes: Vec::new(),
    }
}

fn build_snapshot(session: &ObservabilitySession, tool_names: &[&str]) -> SelfModel {
    let tracker = if session.last_tool_health_export.is_empty() {
        None
    } else {
        Some(ToolHealthTracker::from_entries(
            &session.last_tool_health_export,
        ))
    };

    SelfModel::snapshot_with_strategy(
        tool_names,
        &[],
        &[],
        &session.cached_skill_names,
        tracker.as_ref(),
        session.turn_number,
        None,
        session.active_scenario.as_ref(),
        None,
        session.started_at.elapsed().as_secs(),
        session.user_corrections.len(),
        session.compressed_turns.len(),
        None,
        None,
        None,
        None,
        None,
        &session.last_feedback_signals,
        &session.config,
        session.last_strategy_application.as_ref(),
    )
}

#[test]
fn happy_all_four_fields_populated() {
    let mut session = ObservabilitySession::new_simple("happy-session");

    session.ingest_self_model_inputs(
        vec!["skill_a".to_string(), "skill_b".to_string()],
        // 10 calls × 8 failures => failure_rate 0.8 => deprioritized in from_entries.
        vec![make_entry("grep", 10, 8), make_entry("bash", 10, 9)],
        Some(Scenario::Debugging),
        vec![FeedbackSignal::new(SignalType::TaskSuccess)],
    );

    let snapshot = build_snapshot(&session, &["grep", "bash", "view"]);
    let detailed = snapshot.to_detailed_text();
    let prompt = snapshot.to_system_prompt_section();

    assert!(detailed.contains("skill_a"), "skills missing: {detailed}");
    assert!(detailed.contains("skill_b"));
    assert!(detailed.contains("grep"), "tool_health missing: {detailed}");
    assert!(
        prompt.contains("Debugging"),
        "scenario missing in prompt: {prompt}"
    );
    assert!(
        prompt.contains("TaskSuccess"),
        "signal missing in prompt: {prompt}"
    );
    assert_eq!(snapshot.capabilities.skills, vec!["skill_a", "skill_b"]);
    assert_eq!(snapshot.recent_signals.len(), 1);
}

#[test]
fn unhappy_all_empty_still_renders() {
    let session = ObservabilitySession::new_simple("unhappy-session");

    let snapshot = build_snapshot(&session, &["bash"]);
    let prompt = snapshot.to_system_prompt_section();
    let detailed = snapshot.to_detailed_text();

    assert!(snapshot.capabilities.skills.is_empty());
    assert!(snapshot.capabilities.tool_health.is_empty());
    assert!(snapshot.capabilities.deprioritized_tools.is_empty());
    assert!(snapshot.recent_signals.is_empty());
    assert!(snapshot.state.scenario.is_none());
    assert!(!prompt.is_empty(), "prompt is empty");
    assert!(!detailed.is_empty(), "detailed is empty");
    assert!(detailed.contains("# Agent Self-Model"));
}

#[test]
fn complex_12_health_10_signals_deprioritized() {
    let mut session = ObservabilitySession::new_simple("complex-session");

    // 12 tool_health entries: 4 with high failure rates (deprioritized after
    // seeding via `from_entries`), 8 healthy.
    let tools = [
        ("alpha", 10usize, 9usize),
        ("bravo", 10, 8),
        ("charlie", 5, 1),
        ("delta", 10, 0),
        ("echo", 3, 3),
        ("foxtrot", 9, 7),
        ("golf", 10, 9),
        ("hotel", 10, 0),
        ("india", 4, 0),
        ("juliet", 5, 0),
        ("kilo", 10, 10),
        ("lima", 2, 0),
    ];
    let entries: Vec<ToolHealthEntry> = tools
        .iter()
        .map(|(n, c, f)| make_entry(n, *c, *f))
        .collect();

    // 10 signals of mixed types.
    let signals = vec![
        FeedbackSignal::new(SignalType::TaskSuccess),
        FeedbackSignal::new(SignalType::TaskFailure {
            reason: "io".into(),
        }),
        FeedbackSignal::new(SignalType::Correction),
        FeedbackSignal::new(SignalType::Retry { count: 2 }),
        FeedbackSignal::new(SignalType::FocusDrift),
        FeedbackSignal::new(SignalType::Acceptance),
        FeedbackSignal::new(SignalType::Interruption),
        FeedbackSignal::new(SignalType::ThumbsRating { positive: true }),
        FeedbackSignal::new(SignalType::HighTokenUsage {
            tokens: 9000,
            threshold: 8000,
        }),
        FeedbackSignal::new(SignalType::ToolChurn {
            calls: 12,
            unique_tools: 3,
        }),
    ];

    let skills = vec![
        "skill_zeta".to_string(),
        "skill_yankee".to_string(),
        "skill_xray".to_string(),
        "skill_whiskey".to_string(),
        "skill_victor".to_string(),
    ];

    session.ingest_self_model_inputs(
        skills.clone(),
        entries,
        Some(Scenario::Refactoring),
        signals,
    );

    let tool_names: Vec<&str> = tools.iter().map(|(n, _, _)| *n).collect();
    let snapshot = build_snapshot(&session, &tool_names);

    // Tool health summaries are sorted alphabetically by name for stability
    // (see `snapshot_with_strategy`'s sort by `a.name.cmp(&b.name)`).
    let names: Vec<&str> = snapshot
        .capabilities
        .tool_health
        .iter()
        .map(|t| t.name.as_str())
        .collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted, "tool_health must be alphabetically sorted");
    assert_eq!(names.len(), 12, "all 12 health entries should surface");

    // Tools with >= 8 calls and >= 0.7 failure rate are deprioritized by
    // `ToolHealthTracker::from_entries`.
    let dep = &snapshot.capabilities.deprioritized_tools;
    for name in ["alpha", "bravo", "foxtrot", "golf", "kilo"] {
        assert!(
            dep.iter().any(|d| d == name),
            "expected {name} in deprioritized, got {dep:?}"
        );
    }

    // The session-level ingest bounds `last_feedback_signals` to 16. All 10
    // should be retained here; the SelfModel itself keeps summaries for each.
    assert_eq!(session.last_feedback_signals.len(), 10);
    assert_eq!(snapshot.recent_signals.len(), 10);

    // `to_system_prompt_section` truncates the rendered signal list to 5.
    let prompt = snapshot.to_system_prompt_section();
    let signal_lines: usize = prompt
        .lines()
        .filter(|l| l.starts_with("Recent signals:"))
        .map(|l| l.matches(',').count() + 1)
        .sum();
    assert!(
        signal_lines <= 5,
        "prompt should render at most 5 signals, got {signal_lines}: {prompt}"
    );

    // Scenario name + at least one deprioritized tool appear in the prompt.
    assert!(prompt.contains("Refactoring"), "scenario missing: {prompt}");
    assert!(
        prompt.contains("Deprioritized tools:"),
        "deprioritized section missing: {prompt}"
    );

    // Skills surface in the detailed rendering.
    let detailed = snapshot.to_detailed_text();
    for s in &skills {
        assert!(detailed.contains(s), "missing {s}: {detailed}");
    }
}

#[test]
fn ingest_bounds_feedback_signals_to_most_recent_16() {
    let mut session = ObservabilitySession::new_simple("bound-session");
    let mut signals = Vec::new();
    for i in 0..25u32 {
        let mut sig = FeedbackSignal::new(SignalType::Retry { count: i });
        sig = sig.with_turn(format!("turn-{i}"));
        signals.push(sig);
    }
    session.ingest_self_model_inputs(Vec::new(), Vec::new(), None, signals);

    assert_eq!(session.last_feedback_signals.len(), 16);
    // Must be the TAIL (most recent) — oldest turn-0 dropped, newest turn-24 kept.
    let first_turn = session.last_feedback_signals[0].turn_id.as_deref();
    let last_turn = session.last_feedback_signals[15].turn_id.as_deref();
    assert_eq!(first_turn, Some("turn-9"));
    assert_eq!(last_turn, Some("turn-24"));
}
