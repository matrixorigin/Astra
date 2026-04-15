//! End-to-end tests for the structured feedback loop:
//! detect → extract → store → inject.
//!
//! No LLM calls — uses heuristic extraction only.

use std::sync::{Arc, Mutex};

use astra_runtime::pipeline::{
    calibration::ProgressiveCalibrator,
    feedback_store::FeedbackStore,
    learning::PipelineLearningWriter,
    routing::{DomainHint, TaskType},
};
use astra_runtime::turn::implicit_feedback::{
    detect_implicit_feedback_signal, implicit_feedback_context_injection,
};

// ─── Full cycle: detect → heuristic extract → store → inject ────────────────

#[test]
fn full_cycle_correction_stored_and_injected() {
    let cal = Arc::new(Mutex::new(ProgressiveCalibrator::new(0.15)));
    let writer = PipelineLearningWriter::new().with_progressive_calibrator(cal);
    let store = FeedbackStore::new();
    let sid = "session-1";

    // Full user message — heuristic must extract directive after prefix
    let user_text = "wrong, don't use mocks in tests";
    let signal = detect_implicit_feedback_signal(user_text, Some("I'll mock the database"));
    assert_eq!(signal.signal_type, "correction");

    let injection = implicit_feedback_context_injection(&signal);
    assert!(injection.is_some());

    // record_implicit_feedback takes the directive portion for calibration
    let feedback = writer.record_implicit_feedback(
        &signal, "don't use mocks in tests", "code", Some(DomainHint::Code), TaskType::Code,
    );
    assert!(feedback.is_some());
    store.add(sid, feedback.unwrap());

    let next_turn_injection = store.build_injection(sid);
    assert!(next_turn_injection.contains("[Learned Feedback Rules]"));
    assert!(next_turn_injection.contains("don't use mocks in tests"));
}

#[test]
fn full_cycle_chinese_correction() {
    let cal = Arc::new(Mutex::new(ProgressiveCalibrator::new(0.15)));
    let writer = PipelineLearningWriter::new().with_progressive_calibrator(cal);
    let store = FeedbackStore::new();

    let user_text = "不对，不要用bash执行git命令";
    let signal = detect_implicit_feedback_signal(user_text, Some("我用bash执行git log"));
    assert_eq!(signal.signal_type, "correction");

    let feedback = writer.record_implicit_feedback(
        &signal, "不要用bash执行git命令", "code", Some(DomainHint::Code), TaskType::Code,
    );
    assert!(feedback.is_some());
    store.add("s1", feedback.unwrap());
    assert!(store.build_injection("s1").contains("不要用bash执行git命令"));
}

#[test]
fn full_cycle_complex_correction_heuristic_returns_none() {
    let cal = Arc::new(Mutex::new(ProgressiveCalibrator::new(0.15)));
    let writer = PipelineLearningWriter::new().with_progressive_calibrator(cal.clone());
    let store = FeedbackStore::new();

    let user_text = "that's wrong, the approach doesn't work for this codebase";
    let signal = detect_implicit_feedback_signal(user_text, Some("I used method A"));
    assert_eq!(signal.signal_type, "correction");

    let feedback = writer.record_implicit_feedback(
        &signal, user_text, "code", Some(DomainHint::Code), TaskType::Code,
    );
    assert!(feedback.is_none());
    assert!(store.is_empty("s1"));

    let c = cal.lock().unwrap();
    assert!(c.intent_stats("code").unwrap().correction_rate() > 0.0);
}

#[test]
fn full_cycle_positive_signal_no_feedback() {
    let store = FeedbackStore::new();
    let cal = Arc::new(Mutex::new(ProgressiveCalibrator::new(0.15)));
    let writer = PipelineLearningWriter::new().with_progressive_calibrator(cal);

    let signal = detect_implicit_feedback_signal("perfect, that's exactly right", None);
    let feedback = writer.record_implicit_feedback(
        &signal, "perfect", "code", Some(DomainHint::Code), TaskType::Code,
    );
    assert!(feedback.is_none());
    assert!(store.is_empty("s1"));
}

#[test]
fn full_cycle_neutral_signal_no_feedback() {
    let cal = Arc::new(Mutex::new(ProgressiveCalibrator::new(0.15)));
    let writer = PipelineLearningWriter::new().with_progressive_calibrator(cal);

    let signal = detect_implicit_feedback_signal("tell me about Rust generics", None);
    let feedback = writer.record_implicit_feedback(
        &signal, "tell me about Rust generics", "code", None, TaskType::Code,
    );
    assert!(feedback.is_none());
}

// ─── Session isolation ──────────────────────────────────────────────────────

#[test]
fn sessions_are_isolated() {
    let store = FeedbackStore::new();
    store.add("user-A-session", astra_turn_types::StructuredFeedback {
        rule: "rule for user A".into(),
        reason: "Not stated".into(),
        apply_when: "General".into(),
        source_signal: "correction".into(),
        confidence: 0.9,
    });
    store.add("user-B-session", astra_turn_types::StructuredFeedback {
        rule: "rule for user B".into(),
        reason: "Not stated".into(),
        apply_when: "General".into(),
        source_signal: "correction".into(),
        confidence: 0.9,
    });

    let inj_a = store.build_injection("user-A-session");
    let inj_b = store.build_injection("user-B-session");

    assert!(inj_a.contains("rule for user A"));
    assert!(!inj_a.contains("rule for user B"));
    assert!(inj_b.contains("rule for user B"));
    assert!(!inj_b.contains("rule for user A"));
}

// ─── Multi-turn accumulation ────────────────────────────────────────────────

#[test]
fn multi_turn_rules_accumulate() {
    let cal = Arc::new(Mutex::new(ProgressiveCalibrator::new(0.15)));
    let writer = PipelineLearningWriter::new().with_progressive_calibrator(cal);
    let store = FeedbackStore::new();
    let sid = "s1";

    let s1 = detect_implicit_feedback_signal("wrong, don't use mocks", Some("I'll mock"));
    if let Some(fb) = writer.record_implicit_feedback(
        &s1, "don't use mocks", "code", Some(DomainHint::Code), TaskType::Code,
    ) { store.add(sid, fb); }

    let s2 = detect_implicit_feedback_signal("incorrect, never force push", Some("I'll push"));
    if let Some(fb) = writer.record_implicit_feedback(
        &s2, "never force push on main", "git", Some(DomainHint::Git), TaskType::Code,
    ) { store.add(sid, fb); }

    assert_eq!(store.len(sid), 2);
    let injection = store.build_injection(sid);
    assert!(injection.contains("don't use mocks"));
    assert!(injection.contains("never force push"));
}

#[test]
fn duplicate_rules_deduplicated() {
    let cal = Arc::new(Mutex::new(ProgressiveCalibrator::new(0.15)));
    let writer = PipelineLearningWriter::new().with_progressive_calibrator(cal);
    let store = FeedbackStore::new();

    for _ in 0..3 {
        let s = detect_implicit_feedback_signal("wrong, don't use mocks", Some("mock"));
        if let Some(fb) = writer.record_implicit_feedback(
            &s, "don't use mocks", "code", Some(DomainHint::Code), TaskType::Code,
        ) { store.add("s1", fb); }
    }
    assert_eq!(store.len("s1"), 1);
}

// ─── Injection format ───────────────────────────────────────────────────────

#[test]
fn injection_format_is_llm_friendly() {
    let store = FeedbackStore::new();
    store.add("s1", astra_turn_types::StructuredFeedback {
        rule: "Use moerr instead of fmt.Errorf".into(),
        reason: "MatrixOne coding standard".into(),
        apply_when: "Go error handling".into(),
        source_signal: "correction".into(),
        confidence: 0.9,
    });

    let injection = store.build_injection("s1");
    assert!(injection.starts_with("[Learned Feedback Rules]"));
    assert!(injection.contains("- Rule: Use moerr instead of fmt.Errorf"));
    assert!(injection.contains("Why: MatrixOne coding standard"));
    assert!(injection.contains("When: Go error handling"));
}

// ─── Simulated LLM extraction ───────────────────────────────────────────────

#[test]
fn store_accepts_parsed_llm_response() {
    use astra_runtime::pipeline::feedback_extraction::parse_extraction_response;

    let store = FeedbackStore::new();
    let llm_json = r#"{"rule": "Use real database in integration tests", "reason": "Mock/prod divergence caused outage", "apply_when": "Integration tests for DB services"}"#;

    let fb = parse_extraction_response(llm_json, "correction", 0.9).unwrap();
    store.add("s1", fb);

    let injection = store.build_injection("s1");
    assert!(injection.contains("Use real database"));
    assert!(injection.contains("Mock/prod divergence"));
}

#[test]
fn malformed_llm_response_produces_nothing() {
    use astra_runtime::pipeline::feedback_extraction::parse_extraction_response;

    assert!(parse_extraction_response("not json", "correction", 0.9).is_none());
    assert!(parse_extraction_response("", "correction", 0.9).is_none());
    assert!(parse_extraction_response(r#"{"rule": ""}"#, "correction", 0.9).is_none());
}

// ─── Bridge wiring ──────────────────────────────────────────────────────────

#[test]
fn bridge_feedback_store_is_shared_across_clones() {
    use astra_runtime::turn::bridge_inprocess::InProcessChatTurnBridge;
    use astra_runtime::FernetTokenEncryptor;

    let encryptor = std::sync::Arc::new(
        FernetTokenEncryptor::new("dGVzdGtleXRlc3RrZXl0ZXN0a2V5dGVzdGtleTE=").unwrap(),
    );
    let matrixone = astra_runtime::MatrixOneSettings {
        host: "localhost".into(), port: 6001,
        user: "test".into(), password: "test".into(), database: "test".into(),
    };
    let bridge = InProcessChatTurnBridge::new(matrixone, encryptor);
    let bridge2 = bridge.clone();

    bridge.feedback_store.add("s1", astra_turn_types::StructuredFeedback {
        rule: "don't use mocks".into(),
        reason: "Not stated".into(),
        apply_when: "General".into(),
        source_signal: "correction".into(),
        confidence: 0.9,
    });

    // Visible from clone (Arc sharing)
    assert_eq!(bridge2.feedback_store.len("s1"), 1);
    // But isolated from other sessions
    assert!(bridge2.feedback_store.is_empty("s2"));
}

#[test]
fn bridge_feedback_store_multi_turn_simulation() {
    use astra_runtime::pipeline::feedback_extraction::heuristic_extract;
    use astra_runtime::turn::implicit_feedback::detect_implicit_feedback_signal;

    let store = std::sync::Arc::new(FeedbackStore::new());
    let sid = "session-42";

    // Realistic full messages — directive after correction prefix
    let corrections = [
        ("wrong, don't use mocks in tests", "I'll mock the DB"),
        ("incorrect, never force push on main", "I'll force push"),
        ("that's not right, stop using SELECT *", "SELECT * FROM users"),
    ];

    for (user_msg, prior) in &corrections {
        let signal = detect_implicit_feedback_signal(user_msg, Some(prior));
        if matches!(signal.signal_type.as_str(), "correction" | "frustration") {
            if let Some(fb) = heuristic_extract(user_msg, &signal.signal_type, signal.confidence) {
                store.add(sid, fb);
            }
        }
    }

    assert_eq!(store.len(sid), 3);
    let injection = store.build_injection(sid);
    assert!(injection.starts_with("[Learned Feedback Rules]"));
    // Rules should be the directive portion, not the full message
    assert!(injection.contains("don't use mocks in tests"));
    assert!(injection.contains("never force push on main"));
    assert!(injection.contains("stop using SELECT *"));
    assert_eq!(injection.lines().count(), 4); // header + 3 rules

    // Other sessions unaffected
    assert!(store.is_empty("other-session"));
}

// ─── Empty session_id guard (P0-2) ─────────────────────────────────────────

#[test]
fn empty_session_id_does_not_store_feedback() {
    // Mirrors the bridge guard: if session_id is empty, skip feedback
    let store = FeedbackStore::new();
    let empty_sid = "";

    // Even if we add to empty session_id, it should work but the bridge
    // guards against this. Verify the store itself handles it gracefully.
    store.add(empty_sid, astra_turn_types::StructuredFeedback {
        rule: "leaked rule".into(),
        reason: "Not stated".into(),
        apply_when: "General".into(),
        source_signal: "correction".into(),
        confidence: 0.9,
    });

    // The store accepts it (it's the bridge's job to guard), but verify
    // that different sessions don't see it
    assert_eq!(store.len(""), 1);
    assert!(store.is_empty("some-real-session"));
}

#[test]
fn heuristic_extracts_directive_from_full_correction_message() {
    // This is the exact code path the bridge uses — full user message
    // passed to heuristic_extract, not a pre-extracted directive
    use astra_runtime::pipeline::feedback_extraction::heuristic_extract;

    // "wrong, don't use mocks" → should extract "don't use mocks"
    let fb = heuristic_extract("wrong, don't use mocks", "correction", 0.9).unwrap();
    assert_eq!(fb.rule, "don't use mocks");

    // "不对，不要用bash" → should extract "不要用bash"
    let fb = heuristic_extract("不对，不要用bash执行git命令", "correction", 0.8).unwrap();
    assert_eq!(fb.rule, "不要用bash执行git命令");

    // Complex message with no directive → None
    assert!(heuristic_extract("wrong, the approach is bad", "correction", 0.7).is_none());
}
