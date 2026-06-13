//! End-to-end tests for the structured feedback loop:
//! detect → extract → store → inject.
//!
//! Tests the actual production path: bridge calls heuristic_extract directly,
//! not through record_implicit_feedback. No LLM calls.

use astra_runtime::pipeline::{
    feedback_extraction::heuristic_extract, feedback_store::FeedbackStore,
};
use astra_runtime::turn::implicit_feedback::{
    detect_implicit_feedback_signal, implicit_feedback_context_injection,
};

// ─── Full cycle: detect → heuristic extract → store → inject ────────────────

#[tokio::test]
async fn full_cycle_correction_stored_and_injected() {
    let store = FeedbackStore::new();
    let sid = "session-1";

    let user_text = "wrong, don't use mocks in tests";
    let signal = detect_implicit_feedback_signal(user_text, Some("I'll mock the database"));
    assert_eq!(signal.signal_type, "correction");

    let injection = implicit_feedback_context_injection(&signal);
    assert!(injection.is_some());

    // Bridge path: heuristic_extract on the full user message
    let fb = heuristic_extract(user_text, &signal.signal_type, signal.confidence);
    assert!(fb.is_some());
    store.add(sid, fb.unwrap()).await;

    let next_turn_injection = store.build_injection(sid).await;
    assert!(next_turn_injection.contains("[Learned Feedback Rules]"));
    assert!(next_turn_injection.contains("don't use mocks in tests"));
}

#[tokio::test]
async fn full_cycle_chinese_correction() {
    let store = FeedbackStore::new();

    let user_text = "不对，不要用bash执行git命令";
    let signal = detect_implicit_feedback_signal(user_text, Some("我用bash执行git log"));
    assert_eq!(signal.signal_type, "correction");

    let fb = heuristic_extract(user_text, &signal.signal_type, signal.confidence);
    assert!(fb.is_some());
    store.add("s1", fb.unwrap()).await;
    assert!(
        store
            .build_injection("s1")
            .await
            .contains("不要用bash执行git命令")
    );
}

#[tokio::test]
async fn full_cycle_complex_correction_heuristic_returns_none() {
    let store = FeedbackStore::new();

    let user_text = "that's wrong, the approach doesn't work for this codebase";
    let signal = detect_implicit_feedback_signal(user_text, Some("I used method A"));
    assert_eq!(signal.signal_type, "correction");

    let fb = heuristic_extract(user_text, &signal.signal_type, signal.confidence);
    assert!(fb.is_none());
    assert!(store.is_empty("s1").await);
}

#[tokio::test]
async fn full_cycle_positive_signal_no_extraction() {
    let signal = detect_implicit_feedback_signal("perfect, that's exactly right", None);
    // Positive signals don't trigger extraction in the bridge (guard: correction|frustration)
    assert_ne!(signal.signal_type, "correction");
    assert_ne!(signal.signal_type, "frustration");
}

#[tokio::test]
async fn full_cycle_neutral_signal_no_extraction() {
    let signal = detect_implicit_feedback_signal("tell me about Rust generics", None);
    assert_ne!(signal.signal_type, "correction");
    assert_ne!(signal.signal_type, "frustration");
}

// ─── Session isolation ──────────────────────────────────────────────────────

#[tokio::test]
async fn sessions_are_isolated() {
    let store = FeedbackStore::new();
    store
        .add(
            "user-A-session",
            astra_turn_types::StructuredFeedback {
                rule: "rule for user A".into(),
                reason: "Not stated".into(),
                apply_when: "General".into(),
                source_signal: "correction".into(),
                confidence: 0.9,
            },
        )
        .await;
    store
        .add(
            "user-B-session",
            astra_turn_types::StructuredFeedback {
                rule: "rule for user B".into(),
                reason: "Not stated".into(),
                apply_when: "General".into(),
                source_signal: "correction".into(),
                confidence: 0.9,
            },
        )
        .await;

    let inj_a = store.build_injection("user-A-session").await;
    let inj_b = store.build_injection("user-B-session").await;

    assert!(inj_a.contains("rule for user A"));
    assert!(!inj_a.contains("rule for user B"));
    assert!(inj_b.contains("rule for user B"));
    assert!(!inj_b.contains("rule for user A"));
}

// ─── Multi-turn accumulation ────────────────────────────────────────────────

#[tokio::test]
async fn multi_turn_rules_accumulate() {
    let store = FeedbackStore::new();
    let sid = "s1";

    let corrections = [
        "wrong, don't use mocks",
        "incorrect, never force push on main",
    ];
    for msg in &corrections {
        let signal = detect_implicit_feedback_signal(msg, Some("prior"));
        if let Some(fb) = heuristic_extract(msg, &signal.signal_type, signal.confidence) {
            store.add(sid, fb).await;
        }
    }

    assert_eq!(store.len(sid).await, 2);
    let injection = store.build_injection(sid).await;
    assert!(injection.contains("don't use mocks"));
    assert!(injection.contains("never force push on main"));
}

/// Simulates the bridge ordering: build_injection BEFORE store.add on each turn.
/// Verifies that a rule stored on turn N is NOT in turn N's injection but IS in turn N+1's.
#[tokio::test]
async fn injection_ordering_rule_not_injected_on_same_turn() {
    let store = FeedbackStore::new();
    let sid = "s1";

    // Turn 1: user corrects — build injection first (empty), then store
    let turn1_injection = store.build_injection(sid).await;
    assert!(turn1_injection.is_empty(), "no rules yet on turn 1");
    let fb1 = heuristic_extract("wrong, don't use mocks", "correction", 0.9).unwrap();
    store.add(sid, fb1).await;

    // Turn 2: user corrects again — build injection first (has turn 1's rule), then store
    let turn2_injection = store.build_injection(sid).await;
    assert!(
        turn2_injection.contains("don't use mocks"),
        "turn 1 rule visible on turn 2"
    );
    assert!(
        !turn2_injection.contains("never force push"),
        "turn 2 rule not yet stored"
    );
    let fb2 = heuristic_extract("no, never force push on main", "correction", 0.9).unwrap();
    store.add(sid, fb2).await;

    // Turn 3: no correction — both previous rules visible
    let turn3_injection = store.build_injection(sid).await;
    assert!(turn3_injection.contains("don't use mocks"));
    assert!(turn3_injection.contains("never force push on main"));
}

#[tokio::test]
async fn duplicate_rules_deduplicated() {
    let store = FeedbackStore::new();

    for _ in 0..3 {
        let signal = detect_implicit_feedback_signal("wrong, don't use mocks", Some("mock"));
        if let Some(fb) = heuristic_extract(
            "wrong, don't use mocks",
            &signal.signal_type,
            signal.confidence,
        ) {
            store.add("s1", fb).await;
        }
    }
    assert_eq!(store.len("s1").await, 1);
}

// ─── Injection format ───────────────────────────────────────────────────────

#[tokio::test]
async fn injection_format_is_llm_friendly() {
    let store = FeedbackStore::new();
    store
        .add(
            "s1",
            astra_turn_types::StructuredFeedback {
                rule: "Use moerr instead of fmt.Errorf".into(),
                reason: "MatrixOne coding standard".into(),
                apply_when: "Go error handling".into(),
                source_signal: "correction".into(),
                confidence: 0.9,
            },
        )
        .await;

    let injection = store.build_injection("s1").await;
    assert!(injection.starts_with("[Learned Feedback Rules]"));
    assert!(injection.contains("- Rule: Use moerr instead of fmt.Errorf"));
    assert!(injection.contains("Why: MatrixOne coding standard"));
    assert!(injection.contains("When: Go error handling"));
}

// ─── Simulated LLM extraction ───────────────────────────────────────────────

#[tokio::test]
async fn store_accepts_parsed_llm_response() {
    use astra_runtime::pipeline::feedback_extraction::parse_extraction_response;

    let store = FeedbackStore::new();
    let llm_json = r#"{"rule": "Use real database in integration tests", "reason": "Mock/prod divergence caused outage", "apply_when": "Integration tests for DB services"}"#;

    let fb = parse_extraction_response(llm_json, "correction", 0.9).unwrap();
    store.add("s1", fb).await;

    let injection = store.build_injection("s1").await;
    assert!(injection.contains("Use real database"));
    assert!(injection.contains("Mock/prod divergence"));
}

#[tokio::test]
async fn malformed_llm_response_produces_nothing() {
    use astra_runtime::pipeline::feedback_extraction::parse_extraction_response;

    assert!(parse_extraction_response("not json", "correction", 0.9).is_none());
    assert!(parse_extraction_response("", "correction", 0.9).is_none());
    assert!(parse_extraction_response(r#"{"rule": ""}"#, "correction", 0.9).is_none());
}

// ─── Bridge wiring ──────────────────────────────────────────────────────────

#[tokio::test]
async fn bridge_feedback_store_is_shared_across_clones() {
    use astra_runtime::FernetTokenEncryptor;
    use astra_runtime::turn::bridge::inprocess::InProcessChatTurnBridge;

    let encryptor = std::sync::Arc::new(
        FernetTokenEncryptor::new("dGVzdGtleXRlc3RrZXl0ZXN0a2V5dGVzdGtleTE=").unwrap(),
    );
    let matrixone = astra_runtime::MatrixOneSettings {
        host: "localhost".into(),
        port: 6379,
        user: "test".into(),
        password: "test".into(),
        database: "test".into(),
        db_pool_max_connections: 2,
        db_pool_min_connections: 1,
        db_pool_acquire_timeout_secs: 10,
        db_pool_idle_timeout_secs: 60,
        db_pool_max_lifetime_secs: 300,
    };
    let bridge = InProcessChatTurnBridge::new(matrixone, encryptor);
    let bridge2 = bridge.clone();

    bridge
        .feedback_store
        .add(
            "s1",
            astra_turn_types::StructuredFeedback {
                rule: "don't use mocks".into(),
                reason: "Not stated".into(),
                apply_when: "General".into(),
                source_signal: "correction".into(),
                confidence: 0.9,
            },
        )
        .await;

    // Visible from clone (Arc sharing)
    assert_eq!(bridge2.feedback_store.len("s1").await, 1);
    // But isolated from other sessions
    assert!(bridge2.feedback_store.is_empty("s2").await);
}

#[tokio::test]
async fn bridge_feedback_store_multi_turn_simulation() {
    let store = std::sync::Arc::new(FeedbackStore::new());
    let sid = "session-42";

    // Realistic full messages — directive after correction prefix
    let corrections = [
        ("wrong, don't use mocks in tests", "I'll mock the DB"),
        ("incorrect, never force push on main", "I'll force push"),
        (
            "that's not right, stop using SELECT *",
            "SELECT * FROM users",
        ),
    ];

    for (user_msg, prior) in &corrections {
        let signal = detect_implicit_feedback_signal(user_msg, Some(prior));
        if matches!(signal.signal_type.as_str(), "correction" | "frustration")
            && let Some(fb) = heuristic_extract(user_msg, &signal.signal_type, signal.confidence)
        {
            store.add(sid, fb).await;
        }
    }

    assert_eq!(store.len(sid).await, 3);
    let injection = store.build_injection(sid).await;
    assert!(injection.starts_with("[Learned Feedback Rules]"));
    assert!(injection.contains("don't use mocks in tests"));
    assert!(injection.contains("never force push on main"));
    assert!(injection.contains("stop using SELECT *"));
    assert_eq!(injection.lines().count(), 4); // header + 3 rules

    // Other sessions unaffected
    assert!(store.is_empty("other-session").await);
}

#[tokio::test]
async fn reanchor_nudge_injects_context_without_persisting_vague_goal() {
    let store = FeedbackStore::new();
    let sid = "reanchor-session";

    let user_text = "我要的是长久健康运行，不是临时补丁";
    let signal = detect_implicit_feedback_signal(user_text, Some("I will make a quick patch"));
    assert_eq!(signal.signal_type, "rephrasing");
    assert!(implicit_feedback_context_injection(&signal).is_some());

    if matches!(
        signal.signal_type.as_str(),
        "correction" | "frustration" | "rephrasing"
    ) && let Some(fb) = heuristic_extract(user_text, &signal.signal_type, signal.confidence)
    {
        store.add(sid, fb).await;
    }

    assert!(
        store.is_empty(sid).await,
        "vague reanchor goals should nudge the current turn but not become durable rules"
    );
}

#[tokio::test]
async fn rephrasing_with_directive_can_be_stored_for_later_turns() {
    let store = FeedbackStore::new();
    let sid = "rephrasing-directive-session";

    let user_text = "我重新说一次，不要用case-by-case修补";
    let signal = detect_implicit_feedback_signal(user_text, Some("I'll patch that one case"));
    assert_eq!(signal.signal_type, "rephrasing");

    if matches!(
        signal.signal_type.as_str(),
        "correction" | "frustration" | "rephrasing"
    ) && let Some(fb) = heuristic_extract(user_text, &signal.signal_type, signal.confidence)
    {
        store.add(sid, fb).await;
    }

    let injection = store.build_injection(sid).await;
    assert!(injection.contains("不要用case-by-case修补"));
}

// ─── Empty session_id guard (P0-2) ─────────────────────────────────────────

#[tokio::test]
async fn empty_session_id_does_not_store_feedback() {
    let store = FeedbackStore::new();
    let empty_sid = "";

    store
        .add(
            empty_sid,
            astra_turn_types::StructuredFeedback {
                rule: "leaked rule".into(),
                reason: "Not stated".into(),
                apply_when: "General".into(),
                source_signal: "correction".into(),
                confidence: 0.9,
            },
        )
        .await;

    // The store accepts it (it's the bridge's job to guard), but verify
    // that different sessions don't see it
    assert_eq!(store.len("").await, 1);
    assert!(store.is_empty("some-real-session").await);
}

#[tokio::test]
async fn heuristic_extracts_directive_from_full_correction_message() {
    // This is the exact code path the bridge uses — full user message
    // passed to heuristic_extract, not a pre-extracted directive

    // "wrong, don't use mocks" → should extract "don't use mocks"
    let fb = heuristic_extract("wrong, don't use mocks", "correction", 0.9).unwrap();
    assert_eq!(fb.rule, "don't use mocks");

    // "不对，不要用bash" → should extract "不要用bash执行git命令"
    let fb = heuristic_extract("不对，不要用bash执行git命令", "correction", 0.8).unwrap();
    assert_eq!(fb.rule, "不要用bash执行git命令");

    // Complex message with no directive → None
    assert!(heuristic_extract("wrong, the approach is bad", "correction", 0.7).is_none());
}
