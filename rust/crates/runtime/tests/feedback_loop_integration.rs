//! Integration tests for the session feedback loop (边做边学).
//!
//! Tests the full cycle:
//! 1. User message with implicit feedback signal detected
//! 2. Signal injected into prompt context
//! 3. Denial patterns extracted and auto-rules generated
//! 4. Signal bridged to learning pipeline

use std::sync::{Arc, Mutex};

use astra_runtime::TurnLearningWriter;
use astra_runtime::orchestration::permission_sync::PermissionRule;
use astra_runtime::pipeline::{
    calibration::ProgressiveCalibrator,
    learning::PipelineLearningWriter,
    routing::{DomainHint, TaskType},
};
use astra_runtime::turn::implicit_feedback::{
    detect_implicit_feedback_signal, implicit_feedback_context_injection, implicit_feedback_rating,
};
use astra_turn_core::approval_fingerprint::{ApprovalFingerprint, DenialAction, DenialTracker};

// ─── Phase A + B: Detection → Injection ─────────────────────────────────────

#[test]
fn end_to_end_correction_detected_and_injected() {
    // User says "不对" (wrong) after a previous assistant response
    let user_input = "不对，应该用另一种方法";
    let prev_assistant = "I suggest using the bash tool to list files.";

    // Phase A/B: Detect implicit feedback
    let signal = detect_implicit_feedback_signal(user_input, Some(prev_assistant));
    assert_eq!(signal.signal_type, "correction");
    assert!(signal.confidence >= 0.7);

    // Generate context injection
    let injection = implicit_feedback_context_injection(&signal);
    assert!(injection.is_some());
    let text = injection.unwrap();

    // Verify injection contains key elements
    assert!(text.contains("[Session Feedback]"));
    assert!(text.contains("correction"));
    assert!(text.contains("confidence"));
}

#[test]
fn end_to_end_frustration_detected_and_injected() {
    // Use patterns that are in our detection rules
    let user_input = "terrible response, this is useless";
    let prev_assistant = "Here is the result of your query.";

    let signal = detect_implicit_feedback_signal(user_input, Some(prev_assistant));
    assert_eq!(signal.signal_type, "frustration");

    let injection = implicit_feedback_context_injection(&signal);
    assert!(injection.is_some());
    assert!(injection.unwrap().contains("dissatisfaction"));
}

#[test]
fn end_to_end_positive_not_injected() {
    let user_input = "太好了，这正是我需要的！";

    let signal = detect_implicit_feedback_signal(user_input, None);
    assert_eq!(signal.signal_type, "positive");

    // Positive signals should NOT produce injection
    let injection = implicit_feedback_context_injection(&signal);
    assert!(injection.is_none());
}

// ─── Phase C: Denial Pattern → Auto-Rules ───────────────────────────────────

#[test]
fn end_to_end_repeated_denials_generate_auto_rule() {
    let mut tracker = DenialTracker::default();

    // User denies the same dangerous command twice
    let fp1 = ApprovalFingerprint::shell("bash", "rm -rf /important", false);
    let fp2 = ApprovalFingerprint::shell("bash", "rm -rf /data", false);

    // First denial - no rule yet
    tracker.record_with_reason(&fp1, false, Some("too dangerous"));
    assert!(tracker.extract_auto_deny_rules().is_empty());

    // Second denial with same prefix pattern - should generate rule
    tracker.record_with_reason(&fp2, false, Some("still dangerous"));

    let rules = tracker.extract_auto_deny_rules();
    assert!(
        !rules.is_empty(),
        "should generate auto-deny rule after 2 denials"
    );

    // Verify the generated rule matches the pattern
    let bash_rm_rule = rules.iter().find(|r| {
        r.tool == "bash"
            && r.pattern
                .as_ref()
                .map(|p| p.contains("rm"))
                .unwrap_or(false)
    });
    assert!(bash_rm_rule.is_some());
}

#[test]
fn end_to_end_varied_denials_generate_bare_tool_rule() {
    let mut tracker = DenialTracker::default();

    // User denies 3 different bash commands
    tracker.record_with_reason(
        &ApprovalFingerprint::shell("bash", "curl evil.com", false),
        false,
        None,
    );
    tracker.record_with_reason(
        &ApprovalFingerprint::shell("bash", "wget malware.io", false),
        false,
        None,
    );
    tracker.record_with_reason(
        &ApprovalFingerprint::shell("bash", "nc -e /bin/sh", false),
        false,
        None,
    );

    let rules = tracker.extract_auto_deny_rules();

    // Should generate bare "bash" rule after 3 varied denials
    let bare_bash = rules
        .iter()
        .find(|r| r.tool == "bash" && r.pattern.is_none());
    assert!(
        bare_bash.is_some(),
        "should generate bare tool rule after 3 varied denials"
    );
}

#[test]
fn end_to_end_denial_action_escalates() {
    let mut tracker = DenialTracker::default();
    let fp = ApprovalFingerprint::shell("bash", "dangerous", false);

    // First two denials - continue
    assert_eq!(tracker.record(&fp, false), DenialAction::Continue);
    assert_eq!(tracker.record(&fp, false), DenialAction::Continue);

    // Third denial - skip tool (default limit is 3)
    assert_eq!(tracker.record(&fp, false), DenialAction::SkipTool);
}

// ─── Phase D: Signal → Learning Pipeline ────────────────────────────────────

#[test]
fn end_to_end_correction_updates_calibrator() {
    let cal = Arc::new(Mutex::new(ProgressiveCalibrator::new(0.15)));
    let writer = PipelineLearningWriter::new().with_progressive_calibrator(cal.clone());

    // User says "错了" (wrong)
    let signal = detect_implicit_feedback_signal("错了，这不对", None);
    assert_eq!(signal.signal_type, "correction");

    // Bridge to learning pipeline
    writer.record_implicit_feedback(&signal, "code", Some(DomainHint::Code), TaskType::Code);

    // Verify calibrator recorded the correction
    let c = cal.lock().unwrap();
    let stats = c.intent_stats("code");
    assert!(stats.is_some());
    assert!(stats.unwrap().correction_rate() > 0.0);
}

#[test]
fn end_to_end_positive_records_success() {
    let cal = Arc::new(Mutex::new(ProgressiveCalibrator::new(0.15)));
    let writer = PipelineLearningWriter::new().with_progressive_calibrator(cal.clone());

    let signal = detect_implicit_feedback_signal("perfect, thank you!", None);
    assert_eq!(signal.signal_type, "positive");

    writer.record_implicit_feedback(&signal, "fetch", Some(DomainHint::GitHub), TaskType::Fetch);

    let c = cal.lock().unwrap();
    let stats = c.intent_stats("fetch");
    assert!(stats.is_some());
    // Positive = was_corrected=false, so correction_rate should be 0
    assert_eq!(stats.unwrap().correction_rate(), 0.0);
}

#[test]
fn end_to_end_rating_mapping() {
    // Verify the feedback rating scale
    assert_eq!(implicit_feedback_rating("correction"), 1);
    assert_eq!(implicit_feedback_rating("frustration"), 1);
    assert_eq!(implicit_feedback_rating("rephrasing"), 2);
    assert_eq!(implicit_feedback_rating("clarification"), 3);
    assert_eq!(implicit_feedback_rating("neutral"), 3);
    assert_eq!(implicit_feedback_rating("positive"), 5);
}

// ─── Full Cycle Integration ─────────────────────────────────────────────────

#[test]
fn full_feedback_cycle_correction_to_learning() {
    // Simulate a full feedback cycle:
    // 1. User provides correction
    // 2. Signal detected and injection generated
    // 3. Learning pipeline updated

    let cal = Arc::new(Mutex::new(ProgressiveCalibrator::new(0.15)));
    let writer = PipelineLearningWriter::new().with_progressive_calibrator(cal.clone());

    // Step 1: User message with correction
    let user_input = "wrong, that's not what I asked for";
    let prev_response = "Here is the result.";

    // Step 2: Detect and generate injection
    let signal = detect_implicit_feedback_signal(user_input, Some(prev_response));
    let injection = implicit_feedback_context_injection(&signal);

    assert!(injection.is_some(), "correction should generate injection");

    // Step 3: Update learning pipeline
    writer.record_implicit_feedback(&signal, "reasoning", None, TaskType::Reasoning);

    // Verify the full cycle worked
    let c = cal.lock().unwrap();
    assert!(c.intent_stats("reasoning").is_some());
    assert!(c.intent_stats("reasoning").unwrap().correction_rate() > 0.0);
}

#[test]
fn full_denial_cycle_to_auto_rule() {
    // Simulate denial cycle:
    // 1. User denies tool calls
    // 2. Auto-rules extracted
    // 3. Rules can be applied to permission context

    let mut tracker = DenialTracker::default();

    // Simulate two denials of the same dangerous command pattern
    // Both have "rm -rf" as the prefix (first 2 tokens)
    let fp1 = ApprovalFingerprint::shell("bash", "rm -rf /important/data", false);
    let fp2 = ApprovalFingerprint::shell("bash", "rm -rf /sensitive/files", false);

    tracker.record_with_reason(&fp1, false, Some("dangerous deletion"));
    tracker.record_with_reason(&fp2, false, Some("another dangerous deletion"));

    // Extract rules
    let rules = tracker.extract_auto_deny_rules();

    // Verify rules are actionable - should have pattern for "rm -rf"
    assert!(
        !rules.is_empty(),
        "should generate auto-deny rule after 2 similar denials"
    );

    // Rules should match the "rm -rf" prefix
    let matches_rm = |rule: &PermissionRule| {
        rule.tool == "bash"
            && rule
                .pattern
                .as_ref()
                .map(|p| p.contains("rm"))
                .unwrap_or(false)
    };
    assert!(
        rules.iter().any(matches_rm),
        "should generate rule for rm commands"
    );
}

// ─── Edge Cases: Boundary Conditions ────────────────────────────────────────

#[test]
fn edge_case_empty_input_is_neutral() {
    let signal = detect_implicit_feedback_signal("", None);
    assert_eq!(signal.signal_type, "neutral");
    assert!(signal.evidence.is_empty());

    // Empty input should not generate injection
    let injection = implicit_feedback_context_injection(&signal);
    assert!(injection.is_none());
}

#[test]
fn edge_case_whitespace_only_is_neutral() {
    let signal = detect_implicit_feedback_signal("   \t\n  ", None);
    assert_eq!(signal.signal_type, "neutral");
}

#[test]
fn edge_case_very_long_input_still_detects() {
    // 1000 chars of padding followed by correction keyword
    let padding = "a".repeat(1000);
    let input = format!("{} 不对 this is wrong {}", padding, padding);

    let signal = detect_implicit_feedback_signal(&input, None);
    assert_eq!(signal.signal_type, "correction");
}

#[test]
fn edge_case_unicode_characters_handled() {
    // Emojis and special unicode
    let signal = detect_implicit_feedback_signal("🤬 terrible! 💢", None);
    assert_eq!(signal.signal_type, "frustration");

    // Chinese characters with unicode punctuation
    let signal2 = detect_implicit_feedback_signal("『不对』，请重试", None);
    assert_eq!(signal2.signal_type, "correction");
}

#[test]
fn edge_case_mixed_case_detection() {
    // Mixed case should still match (case insensitive)
    let signal = detect_implicit_feedback_signal("WRONG answer", None);
    assert_eq!(signal.signal_type, "correction");

    let signal2 = detect_implicit_feedback_signal("TeRrIbLe response", None);
    assert_eq!(signal2.signal_type, "frustration");
}

#[test]
fn edge_case_special_regex_characters() {
    // Input with regex special chars should not crash
    let signal = detect_implicit_feedback_signal("test [abc] (def) {ghi} .* + ? | \\ ^ $", None);
    assert_eq!(signal.signal_type, "neutral");
}

#[test]
fn edge_case_denial_with_empty_reason() {
    let mut tracker = DenialTracker::default();
    let fp = ApprovalFingerprint::shell("bash", "some cmd", false);

    // Empty string reason should not crash
    tracker.record_with_reason(&fp, false, Some(""));
    tracker.record_with_reason(&fp, false, Some(""));

    let rules = tracker.extract_auto_deny_rules();
    assert!(!rules.is_empty());
}

#[test]
fn edge_case_denial_with_none_reason() {
    let mut tracker = DenialTracker::default();
    let fp = ApprovalFingerprint::shell("bash", "cmd", false);

    // None reason should work fine
    tracker.record_with_reason(&fp, false, None);
    tracker.record_with_reason(&fp, false, None);

    let rules = tracker.extract_auto_deny_rules();
    assert!(!rules.is_empty());
}

#[test]
fn edge_case_very_long_command_prefix() {
    let long_cmd = format!("verylongcommandname{}", "x".repeat(500));
    let fp = ApprovalFingerprint::shell("bash", &long_cmd, false);

    let mut tracker = DenialTracker::default();
    tracker.record_with_reason(&fp, false, None);
    tracker.record_with_reason(&fp, false, None);

    // Should still generate rules without crashing
    let rules = tracker.extract_auto_deny_rules();
    assert!(!rules.is_empty());
}

#[test]
fn edge_case_calibrator_with_empty_intent() {
    let cal = Arc::new(Mutex::new(ProgressiveCalibrator::new(0.15)));
    let writer = PipelineLearningWriter::new().with_progressive_calibrator(cal.clone());

    let signal = detect_implicit_feedback_signal("wrong", None);

    // Empty intent string should not crash
    writer.record_implicit_feedback(&signal, "", None, TaskType::Unknown);

    let c = cal.lock().unwrap();
    // Should still record (with empty string as key)
    assert!(c.intent_stats("").is_some());
}

#[test]
fn edge_case_injection_text_content() {
    // Verify injection text is well-formed
    let signal = detect_implicit_feedback_signal("错了，不是这样", None);
    let injection = implicit_feedback_context_injection(&signal);

    let text = injection.expect("should have injection");

    // Check structure
    assert!(text.starts_with("[Session Feedback]"));
    assert!(text.contains("correction"));
    assert!(text.contains("0.7")); // confidence

    // Should be reasonable length (not too long)
    assert!(text.len() < 500, "injection should be concise");
}

#[test]
fn edge_case_prev_response_very_long() {
    // Very long previous response should still work
    let long_response = "The assistant said something ".repeat(100);
    let signal = detect_implicit_feedback_signal("wrong", Some(&long_response));

    assert_eq!(signal.signal_type, "correction");
}

#[test]
fn edge_case_approval_reset_clears_all_state() {
    let mut tracker = DenialTracker::default();

    // Add some denials
    for i in 0..5 {
        let fp = ApprovalFingerprint::shell("bash", &format!("cmd{}", i), false);
        tracker.record_with_reason(&fp, false, Some("reason"));
    }

    assert!(!tracker.extract_auto_deny_rules().is_empty());
    assert!(tracker.total_denials() > 0);

    // Reset should clear everything
    tracker.reset();

    assert!(tracker.extract_auto_deny_rules().is_empty());
    assert_eq!(tracker.total_denials(), 0);
}

// ─── Concurrent Access Tests ────────────────────────────────────────────────

use std::thread;

#[test]
fn concurrent_calibrator_writes() {
    // Multiple threads writing to calibrator simultaneously
    let cal = Arc::new(Mutex::new(ProgressiveCalibrator::new(0.15)));
    let writer = Arc::new(PipelineLearningWriter::new().with_progressive_calibrator(cal.clone()));

    let handles: Vec<_> = (0..10)
        .map(|i| {
            let writer_clone = writer.clone();
            thread::spawn(move || {
                let signal_type = if i % 2 == 0 { "correction" } else { "positive" };
                let signal = astra_runtime::turn::implicit_feedback::ImplicitSignal {
                    signal_type: signal_type.to_string(),
                    confidence: 0.8,
                    evidence: format!("thread {}", i),
                };
                let intent = format!("intent_{}", i % 3);
                writer_clone.record_implicit_feedback(&signal, &intent, None, TaskType::Code);
            })
        })
        .collect();

    for handle in handles {
        handle.join().expect("thread should complete");
    }

    // Verify calibrator is still in valid state
    let c = cal.lock().unwrap();
    // At least some intents should have been recorded
    let total = c.tracked_intent_count();
    assert!(total > 0, "calibrator should have recorded intents");
}

#[test]
fn concurrent_shared_calibrator_mixed_operations() {
    // Simulates real-world scenario: multiple turns writing to shared calibrator
    let cal = Arc::new(Mutex::new(ProgressiveCalibrator::new(0.15)));

    let handles: Vec<_> = (0..20)
        .map(|i| {
            let cal_clone = cal.clone();
            thread::spawn(move || {
                // Simulate different operations
                match i % 4 {
                    0 => {
                        // Write correction
                        let mut c = cal_clone.lock().unwrap();
                        c.record(
                            &format!("intent_{}", i % 5),
                            Some(DomainHint::Code),
                            TaskType::Code,
                            true, // was_corrected
                            Some(25),
                        );
                    }
                    1 => {
                        // Write success
                        let mut c = cal_clone.lock().unwrap();
                        c.record(
                            &format!("intent_{}", i % 5),
                            Some(DomainHint::GitHub),
                            TaskType::Fetch,
                            false,
                            Some(80),
                        );
                    }
                    2 => {
                        // Read threshold
                        let c = cal_clone.lock().unwrap();
                        let _threshold = c.calibrated_threshold(
                            &format!("intent_{}", i % 5),
                            Some(DomainHint::Code),
                            TaskType::Code,
                        );
                    }
                    _ => {
                        // Read stats
                        let c = cal_clone.lock().unwrap();
                        let _count = c.tracked_intent_count();
                    }
                }
            })
        })
        .collect();

    for handle in handles {
        handle.join().expect("thread should complete");
    }

    // Final validation
    let c = cal.lock().unwrap();
    assert!(c.tracked_intent_count() > 0);
}

#[test]
fn concurrent_injection_generation() {
    // Injection generation should be pure and thread-safe
    let handles: Vec<_> = (0..20)
        .map(|i| {
            thread::spawn(move || {
                let signal_type = match i % 4 {
                    0 => "correction",
                    1 => "frustration",
                    2 => "rephrasing",
                    _ => "neutral",
                };

                let signal = astra_runtime::turn::implicit_feedback::ImplicitSignal {
                    signal_type: signal_type.to_string(),
                    confidence: 0.5 + (i as f64 * 0.02),
                    evidence: format!("evidence {}", i),
                };

                let injection = implicit_feedback_context_injection(&signal);

                // Verify expected behavior
                match signal_type {
                    "correction" | "frustration" | "rephrasing" => {
                        assert!(injection.is_some());
                        let text = injection.unwrap();
                        assert!(text.contains("[Session Feedback]"));
                    }
                    _ => {
                        assert!(injection.is_none());
                    }
                }
            })
        })
        .collect();

    for handle in handles {
        handle.join().expect("thread should complete");
    }
}

// ─── Phase E: Correction Signal Chain (hook payload → calibrator) ────────────
//
// These tests verify the fix for the calibrator signal chain break:
// bridge_inprocess.rs now injects "is_correction" into the hook payload,
// which flows through build_learning_outcome_from_payload → record_outcome
// → ProgressiveCalibrator.

/// E2E: hook payload with is_correction=true flows through the full pipeline
/// and actually lowers the calibrated threshold.
#[tokio::test]
async fn correction_signal_chain_payload_to_calibrator_threshold() {
    let cal = Arc::new(Mutex::new(ProgressiveCalibrator::new(0.70)));
    let writer = PipelineLearningWriter::new().with_progressive_calibrator(cal.clone());

    let initial_threshold = cal.lock().unwrap().calibrated_threshold(
        "fetch",
        Some(DomainHint::GitHub),
        TaskType::Fetch,
    );

    // Simulate the hook payload that bridge_inprocess.rs now produces
    // when implicit feedback detects a correction.
    for i in 0..6 {
        let payload = serde_json::json!({
            "messages": [
                {"role": "user", "content": format!("no that's wrong, try again {i}")},
                {"role": "assistant", "content": "Here are the results..."}
            ],
            "tool_calls": [
                {"function": {"name": "github_list_prs"}}
            ],
            "tool_quality_assessments": [
                {"tool_name": "github_list_prs", "grade": "good", "quality_score": 0.8}
            ],
            "tool_results": [
                {"content": "{\"status\":\"ok\"}"}
            ],
            "is_correction": true,
            "routing_meta": {
                "task_type": "fetch",
                "domain_hint": "github"
            }
        });

        let outcome =
            astra_runtime::pipeline::learning::build_learning_outcome_from_payload(&payload)
                .expect("should parse payload");
        assert!(
            outcome.was_corrected,
            "turn {i}: was_corrected must be true when is_correction is in payload"
        );
        let _ = writer.record_outcome(outcome).await;
    }

    let final_threshold = cal.lock().unwrap().calibrated_threshold(
        "fetch",
        Some(DomainHint::GitHub),
        TaskType::Fetch,
    );

    assert!(
        final_threshold < initial_threshold,
        "threshold should decrease after corrections: initial={initial_threshold}, final={final_threshold}"
    );
}

/// E2E: payload WITHOUT is_correction → was_corrected=false → threshold unchanged.
#[tokio::test]
async fn no_correction_signal_leaves_threshold_unchanged() {
    let cal = Arc::new(Mutex::new(ProgressiveCalibrator::new(0.70)));
    let writer = PipelineLearningWriter::new().with_progressive_calibrator(cal.clone());

    let initial_threshold = cal
        .lock()
        .unwrap()
        .calibrated_threshold("code", None, TaskType::Code);

    for i in 0..6 {
        let payload = serde_json::json!({
            "messages": [
                {"role": "user", "content": format!("show me the code {i}")}
            ],
            "tool_calls": [
                {"function": {"name": "read_file"}}
            ],
            "tool_quality_assessments": [
                {"tool_name": "read_file", "grade": "good", "quality_score": 0.9}
            ],
            "tool_results": [
                {"content": "fn main() {}"}
            ]
        });

        let outcome =
            astra_runtime::pipeline::learning::build_learning_outcome_from_payload(&payload)
                .expect("should parse payload");
        assert!(
            !outcome.was_corrected,
            "turn {i}: was_corrected must be false when is_correction absent"
        );
        let _ = writer.record_outcome(outcome).await;
    }

    let final_threshold = cal
        .lock()
        .unwrap()
        .calibrated_threshold("code", None, TaskType::Code);

    assert_eq!(
        initial_threshold, final_threshold,
        "threshold should not change without corrections"
    );
}

/// Frustration signal (not just correction) also sets was_corrected=true.
#[tokio::test]
async fn frustration_signal_also_triggers_calibrator_correction() {
    let cal = Arc::new(Mutex::new(ProgressiveCalibrator::new(0.70)));
    let writer = PipelineLearningWriter::new().with_progressive_calibrator(cal.clone());

    // Frustration detected by bridge → is_correction=true in payload
    for i in 0..6 {
        let payload = serde_json::json!({
            "messages": [
                {"role": "user", "content": format!("this is completely useless {i}")},
                {"role": "assistant", "content": "Let me try again..."}
            ],
            "tool_calls": [
                {"function": {"name": "bash"}}
            ],
            "tool_quality_assessments": [
                {"tool_name": "bash", "grade": "poor", "quality_score": 0.3}
            ],
            "tool_results": [
                {"content": "error: command not found"}
            ],
            "is_correction": true
        });

        let outcome =
            astra_runtime::pipeline::learning::build_learning_outcome_from_payload(&payload)
                .expect("should parse payload");
        assert!(outcome.was_corrected);
        let _ = writer.record_outcome(outcome).await;
    }

    let guard = cal.lock().unwrap();
    let stats = guard.intent_stats("unknown");
    assert!(
        stats.is_some(),
        "calibrator should have recorded observations"
    );
    assert!(
        stats.unwrap().correction_rate() > 0.0,
        "correction rate should be positive after frustration signals"
    );
}

/// Empty messages in payload → build_learning_outcome returns None (no panic).
#[tokio::test]
async fn empty_messages_payload_returns_none() {
    let payload = serde_json::json!({
        "messages": [],
        "is_correction": true
    });
    assert!(
        astra_runtime::pipeline::learning::build_learning_outcome_from_payload(&payload).is_none(),
        "empty messages should return None"
    );
}

/// Payload with no tool_calls → outcome has empty tools, low quality, was_corrected still works.
#[tokio::test]
async fn no_tool_calls_still_propagates_correction() {
    let payload = serde_json::json!({
        "messages": [
            {"role": "user", "content": "wrong answer"}
        ],
        "is_correction": true
    });

    let outcome = astra_runtime::pipeline::learning::build_learning_outcome_from_payload(&payload)
        .expect("should parse even without tool_calls");
    assert!(outcome.was_corrected);
    assert!(outcome.tools_used.is_empty());
    assert!(!outcome.success, "no tools used → not successful");
}

/// is_correction=false explicitly set → was_corrected=false.
#[tokio::test]
async fn explicit_false_correction_flag() {
    let payload = serde_json::json!({
        "messages": [
            {"role": "user", "content": "looks good"}
        ],
        "tool_calls": [
            {"function": {"name": "read_file"}}
        ],
        "tool_quality_assessments": [
            {"quality_score": 0.9}
        ],
        "tool_results": [
            {"content": "ok"}
        ],
        "is_correction": false
    });

    let outcome = astra_runtime::pipeline::learning::build_learning_outcome_from_payload(&payload)
        .expect("should parse");
    assert!(!outcome.was_corrected);
}

/// Malformed is_correction (string instead of bool) → was_corrected=false (no panic).
#[tokio::test]
async fn malformed_correction_flag_no_panic() {
    let payload = serde_json::json!({
        "messages": [
            {"role": "user", "content": "test"}
        ],
        "tool_calls": [
            {"function": {"name": "read_file"}}
        ],
        "tool_results": [
            {"content": "ok"}
        ],
        "is_correction": "yes"
    });

    let outcome = astra_runtime::pipeline::learning::build_learning_outcome_from_payload(&payload)
        .expect("should parse without panic");
    assert!(
        !outcome.was_corrected,
        "malformed flag should default to false"
    );
}

/// Quality gate may reject trivial outcomes — correction still recorded if gate passes.
#[tokio::test]
async fn quality_gate_blocks_trivial_correction_from_calibrator() {
    let cal = Arc::new(Mutex::new(ProgressiveCalibrator::new(0.70)));
    let writer = PipelineLearningWriter::new().with_progressive_calibrator(cal.clone());

    // Very short query — quality gate should reject this
    let payload = serde_json::json!({
        "messages": [
            {"role": "user", "content": "no"}
        ],
        "is_correction": true
    });

    let outcome = astra_runtime::pipeline::learning::build_learning_outcome_from_payload(&payload)
        .expect("should parse");
    assert!(outcome.was_corrected);

    // record_outcome may be rejected by quality gate (short query)
    let _ = writer.record_outcome(outcome).await;

    // Calibrator should NOT have data — quality gate should have blocked it
    let guard = cal.lock().unwrap();
    let stats = guard.intent_stats("unknown");
    let has_data = stats.map(|s| s.total > 0).unwrap_or(false);
    assert!(
        !has_data,
        "quality gate should block trivially short correction from reaching calibrator"
    );
}
