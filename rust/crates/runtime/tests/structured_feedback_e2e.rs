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

    // User says "wrong, don't use mocks in tests"
    // Signal detector catches "wrong", heuristic extracts "don't use mocks in tests"
    let user_text = "wrong, don't use mocks in tests";
    let signal = detect_implicit_feedback_signal(user_text, Some("I'll mock the database"));
    assert_eq!(signal.signal_type, "correction");

    // Context injection fires immediately
    let injection = implicit_feedback_context_injection(&signal);
    assert!(injection.is_some());

    // Learning pipeline records + heuristic extracts from user text
    let feedback = writer.record_implicit_feedback(
        &signal,
        "don't use mocks in tests", // the directive portion
        "code",
        Some(DomainHint::Code),
        TaskType::Code,
    );
    assert!(feedback.is_some());
    let fb = feedback.unwrap();
    assert_eq!(fb.rule, "don't use mocks in tests");

    // Store and verify injection for next turn
    store.add(fb);
    let next_turn_injection = store.build_injection();
    assert!(next_turn_injection.contains("[Learned Feedback Rules]"));
    assert!(next_turn_injection.contains("don't use mocks in tests"));
}

#[test]
fn full_cycle_chinese_correction() {
    let cal = Arc::new(Mutex::new(ProgressiveCalibrator::new(0.15)));
    let writer = PipelineLearningWriter::new().with_progressive_calibrator(cal);
    let store = FeedbackStore::new();

    // "不对" triggers correction, "不要用bash执行git命令" is the directive
    let user_text = "不对，不要用bash执行git命令";
    let signal = detect_implicit_feedback_signal(user_text, Some("我用bash执行git log"));
    assert_eq!(signal.signal_type, "correction");

    let feedback = writer.record_implicit_feedback(
        &signal,
        "不要用bash执行git命令",
        "code",
        Some(DomainHint::Code),
        TaskType::Code,
    );
    assert!(feedback.is_some());
    store.add(feedback.unwrap());

    let injection = store.build_injection();
    assert!(injection.contains("不要用bash执行git命令"));
}

#[test]
fn full_cycle_complex_correction_heuristic_returns_none() {
    let cal = Arc::new(Mutex::new(ProgressiveCalibrator::new(0.15)));
    let writer = PipelineLearningWriter::new().with_progressive_calibrator(cal.clone());
    let store = FeedbackStore::new();

    // "that's wrong" triggers correction, but the rest is too complex for heuristic
    let user_text = "that's wrong, the approach doesn't work for this codebase";
    let signal = detect_implicit_feedback_signal(user_text, Some("I used method A"));
    assert_eq!(signal.signal_type, "correction");

    let feedback = writer.record_implicit_feedback(
        &signal,
        user_text,
        "code",
        Some(DomainHint::Code),
        TaskType::Code,
    );
    assert!(feedback.is_none(), "complex correction should return None for LLM fallback");
    assert!(store.is_empty());

    // Calibrator still recorded the correction
    let c = cal.lock().unwrap();
    assert!(c.intent_stats("code").unwrap().correction_rate() > 0.0);
}

#[test]
fn full_cycle_positive_signal_no_feedback() {
    let cal = Arc::new(Mutex::new(ProgressiveCalibrator::new(0.15)));
    let writer = PipelineLearningWriter::new().with_progressive_calibrator(cal);
    let store = FeedbackStore::new();

    let user_text = "perfect, that's exactly right";
    let signal = detect_implicit_feedback_signal(user_text, None);
    assert_eq!(signal.signal_type, "positive");

    let feedback = writer.record_implicit_feedback(
        &signal, user_text, "code", Some(DomainHint::Code), TaskType::Code,
    );
    assert!(feedback.is_none());
    assert!(store.is_empty());
}

#[test]
fn full_cycle_neutral_signal_no_feedback() {
    let cal = Arc::new(Mutex::new(ProgressiveCalibrator::new(0.15)));
    let writer = PipelineLearningWriter::new().with_progressive_calibrator(cal);

    let user_text = "tell me about Rust generics";
    let signal = detect_implicit_feedback_signal(user_text, None);
    assert_eq!(signal.signal_type, "neutral");

    let feedback = writer.record_implicit_feedback(
        &signal, user_text, "code", None, TaskType::Code,
    );
    assert!(feedback.is_none());
}

// ─── Multi-turn accumulation ────────────────────────────────────────────────

#[test]
fn multi_turn_rules_accumulate() {
    let cal = Arc::new(Mutex::new(ProgressiveCalibrator::new(0.15)));
    let writer = PipelineLearningWriter::new().with_progressive_calibrator(cal);
    let store = FeedbackStore::new();

    // Turn 1: "wrong" triggers, "don't use mocks" extracted
    let s1 = detect_implicit_feedback_signal("wrong, don't use mocks", Some("I'll mock"));
    if let Some(fb) = writer.record_implicit_feedback(
        &s1, "don't use mocks", "code", Some(DomainHint::Code), TaskType::Code,
    ) {
        store.add(fb);
    }

    // Turn 2: "incorrect" triggers, "never use force push" extracted
    let s2 = detect_implicit_feedback_signal("incorrect, never use force push on main", Some("I'll force push"));
    if let Some(fb) = writer.record_implicit_feedback(
        &s2, "never use force push on main", "git", Some(DomainHint::Git), TaskType::Code,
    ) {
        store.add(fb);
    }

    assert_eq!(store.len(), 2);
    let injection = store.build_injection();
    assert!(injection.contains("don't use mocks"));
    assert!(injection.contains("never use force push"));
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
        ) {
            store.add(fb);
        }
    }

    assert_eq!(store.len(), 1);
}

// ─── Injection format ───────────────────────────────────────────────────────

#[test]
fn injection_format_is_llm_friendly() {
    let store = FeedbackStore::new();
    store.add(astra_turn_types::StructuredFeedback {
        rule: "Use moerr instead of fmt.Errorf".into(),
        reason: "MatrixOne coding standard".into(),
        apply_when: "Go error handling".into(),
        source_signal: "correction".into(),
        confidence: 0.9,
    });

    let injection = store.build_injection();
    assert!(injection.starts_with("[Learned Feedback Rules]"));
    assert!(injection.contains("- Rule: Use moerr instead of fmt.Errorf"));
    assert!(injection.contains("Why: MatrixOne coding standard"));
    assert!(injection.contains("When: Go error handling"));
}

// ─── Simulated LLM extraction (no actual LLM) ──────────────────────────────

#[test]
fn store_accepts_parsed_llm_response() {
    use astra_runtime::pipeline::feedback_extraction::parse_extraction_response;

    let store = FeedbackStore::new();
    let llm_json = r#"{"rule": "Use real database in integration tests", "reason": "Mock/prod divergence caused outage", "apply_when": "Integration tests for DB services"}"#;

    let fb = parse_extraction_response(llm_json, "correction", 0.9).unwrap();
    store.add(fb);

    let injection = store.build_injection();
    assert!(injection.contains("Use real database"));
    assert!(injection.contains("Mock/prod divergence"));
    assert!(injection.contains("Integration tests for DB services"));
}

#[test]
fn malformed_llm_response_produces_nothing() {
    use astra_runtime::pipeline::feedback_extraction::parse_extraction_response;

    assert!(parse_extraction_response("not json", "correction", 0.9).is_none());
    assert!(parse_extraction_response("", "correction", 0.9).is_none());
    assert!(parse_extraction_response(r#"{"rule": ""}"#, "correction", 0.9).is_none());
}

// ─── Bridge wiring: FeedbackStore on InProcessChatTurnBridge ────────────────

#[test]
fn bridge_feedback_store_is_shared_across_clones() {
    use astra_runtime::turn::bridge_inprocess::InProcessChatTurnBridge;
    use astra_runtime::FernetTokenEncryptor;

    let encryptor = std::sync::Arc::new(
        FernetTokenEncryptor::new("dGVzdGtleXRlc3RrZXl0ZXN0a2V5dGVzdGtleTE=")
            .unwrap(),
    );
    let matrixone = astra_runtime::MatrixOneSettings {
        host: "localhost".into(),
        port: 6001,
        user: "test".into(),
        password: "test".into(),
        database: "test".into(),
    };
    let bridge = InProcessChatTurnBridge::new(matrixone, encryptor);

    // Clone the bridge (as happens when shared across turns)
    let bridge2 = bridge.clone();

    // Add feedback via one clone
    bridge.feedback_store.add(astra_turn_types::StructuredFeedback {
        rule: "don't use mocks".into(),
        reason: "Not stated".into(),
        apply_when: "General".into(),
        source_signal: "correction".into(),
        confidence: 0.9,
    });

    // Verify visible from the other clone (Arc sharing)
    assert_eq!(bridge2.feedback_store.len(), 1);
    let injection = bridge2.feedback_store.build_injection();
    assert!(injection.contains("don't use mocks"));
}

#[test]
fn bridge_feedback_store_starts_empty() {
    use astra_runtime::turn::bridge_inprocess::InProcessChatTurnBridge;
    use astra_runtime::FernetTokenEncryptor;

    let encryptor = std::sync::Arc::new(
        FernetTokenEncryptor::new("dGVzdGtleXRlc3RrZXl0ZXN0a2V5dGVzdGtleTE=")
            .unwrap(),
    );
    let matrixone = astra_runtime::MatrixOneSettings {
        host: "localhost".into(),
        port: 6001,
        user: "test".into(),
        password: "test".into(),
        database: "test".into(),
    };
    let bridge = InProcessChatTurnBridge::new(matrixone, encryptor);

    assert!(bridge.feedback_store.is_empty());
    assert!(bridge.feedback_store.build_injection().is_empty());
}

#[test]
fn bridge_feedback_store_accumulates_across_simulated_turns() {
    use astra_runtime::pipeline::feedback_store::FeedbackStore;
    use astra_runtime::pipeline::feedback_extraction::heuristic_extract;
    use astra_runtime::turn::implicit_feedback::detect_implicit_feedback_signal;

    let store = std::sync::Arc::new(FeedbackStore::new());

    // Simulate 3 turns with corrections
    let corrections = [
        ("wrong, don't use mocks in tests", "don't use mocks in tests"),
        ("incorrect, never force push on main", "never force push on main"),
        ("that's not right, stop using SELECT *", "stop using SELECT *"),
    ];

    for (user_msg, directive) in &corrections {
        let signal = detect_implicit_feedback_signal(user_msg, Some("prior response"));
        // Only process if signal detected a correction
        if signal.signal_type == "correction" || signal.signal_type == "frustration" {
            if let Some(fb) = heuristic_extract(directive, &signal.signal_type, signal.confidence) {
                store.add(fb);
            }
        }
    }

    assert_eq!(store.len(), 3);
    let injection = store.build_injection();
    assert!(injection.contains("don't use mocks"));
    assert!(injection.contains("never force push"));
    assert!(injection.contains("stop using SELECT"));

    // Verify the injection is a single block suitable for system prompt
    assert!(injection.starts_with("[Learned Feedback Rules]"));
    let line_count = injection.lines().count();
    assert_eq!(line_count, 4, "header + 3 rules = 4 lines, got {line_count}");
}
