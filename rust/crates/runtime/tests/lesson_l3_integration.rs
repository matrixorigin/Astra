//! End-to-end verification of the lesson L3 integration:
//!
//! 1. Session-end backflow: L1b narrative → extracted lessons → Memoria-ready
//! 2. Incremental checkpoint: tool failures → delta extraction → dedup
//! 3. LLM synthesis: prompt template + quality gate
//! 4. Compact/detail content model
//!
//! These tests prove the full data loop WITHOUT live Memoria or LLM —
//! they verify the pure logic at each stage.

use astra_runtime::lesson_checkpoint::LessonCheckpointer;
use astra_runtime::lesson_extractor::{self, SessionSummary};
use astra_runtime::lesson_synthesizer::{
    self, ExtractedLesson, LessonContext, build_synthesis_user_prompt,
    is_synthesized_lesson_acceptable,
};
use astra_services::LessonKind;

// ── 1. Session-end L3 backflow ─────────────────────────────────────────────

#[test]
fn backflow_end_to_end_narrative_to_memoria_lessons() {
    let raw = "\
[session-memory:v1]
# Task Specification
Implement OAuth for the API

# User Corrections
- Use RS256 not HS256 for JWT signing
- Always validate refresh token expiry server-side

# Learnings
- This repo uses pnpm workspaces; npm install breaks cross-deps
- The auth middleware chain order matters: rate-limit before JWT verify

# Decisions
- Use Redis for refresh token storage
";
    let narrative =
        astra_runtime::turn::cloud::session_memory_protocol::SessionMemory::parse(raw).unwrap();

    let lessons = lesson_synthesizer::extract_learnings_for_backflow(Some(&narrative));

    // 2 corrections (T2) + 2 learnings (T3) = 4
    assert_eq!(lessons.len(), 4);

    let corrections: Vec<&ExtractedLesson> =
        lessons.iter().filter(|l| l.trust_tier == "T2").collect();
    let learnings: Vec<&ExtractedLesson> =
        lessons.iter().filter(|l| l.trust_tier == "T3").collect();

    assert_eq!(corrections.len(), 2);
    assert_eq!(learnings.len(), 2);

    // Corrections have emoji prefix matching goal-driven-evolution protocol
    assert!(corrections[0].content.starts_with("🔧 CORRECTION:"));
    assert!(corrections[0].content.contains("RS256"));
    assert!(learnings[0].content.starts_with("💡 LESSON:"));
    assert!(learnings[0].content.contains("pnpm"));

    // Episodic summary also generated
    let episodic =
        lesson_synthesizer::build_episodic_summary("sess-oauth", 25, Some(&narrative)).unwrap();
    assert_eq!(episodic.memory_type, "episodic");
    assert!(episodic.content.contains("Implement OAuth"));
    assert!(episodic.content.contains("25 turns"));
    assert!(episodic.content.contains("Redis"));
}

// ── 2. Incremental checkpoint ──────────────────────────────────────────────

#[test]
fn checkpoint_produces_delta_across_turns() {
    let mut cp = LessonCheckpointer::new();

    // Turn 5: grep fails 5 times
    let mut s1 = SessionSummary::default();
    s1.tool_failures.insert("grep".into(), 5);
    let delta1 = cp.maybe_checkpoint(&s1, 5, "u1", "generic", None);
    assert_eq!(delta1.len(), 1);
    assert!(delta1[0].trigger_signal.contains("grep"));

    // Turn 10: grep still failing + rg also fails + stalls
    let mut s2 = SessionSummary::default();
    s2.tool_failures.insert("grep".into(), 8);
    s2.tool_failures.insert("rg".into(), 4);
    s2.stall_events = 3;
    let delta2 = cp.maybe_checkpoint(&s2, 10, "u1", "generic", None);
    // grep already recorded → only rg + stall are new
    assert_eq!(delta2.len(), 2);
    assert!(delta2.iter().any(|l| l.trigger_signal.contains("rg")));
    assert!(delta2.iter().any(|l| l.kind == LessonKind::PromptShape));

    // Turn 10 again → same turn, no re-checkpoint
    let delta3 = cp.maybe_checkpoint(&s2, 10, "u1", "generic", None);
    assert!(delta3.is_empty());

    assert_eq!(cp.recorded_count(), 3); // grep + rg + stall
}

// ── 4. LLM synthesis quality gate ──────────────────────────────────────────

#[test]
fn synthesis_quality_gate_accepts_only_specific_lessons() {
    // Template-like → rejected
    assert!(!is_synthesized_lesson_acceptable(
        "consider alternatives to grep"
    ));
    assert!(!is_synthesized_lesson_acceptable("tighten the plan"));
    assert!(!is_synthesized_lesson_acceptable("maybe use rg instead"));
    assert!(!is_synthesized_lesson_acceptable("x".repeat(5).as_str()));

    // Specific, actionable → accepted
    assert!(is_synthesized_lesson_acceptable(
        "In this 280k-file monorepo, use `rg --glob '!node_modules'` instead of `grep -r`"
    ));
    assert!(is_synthesized_lesson_acceptable(
        "Always pass --filter to pnpm commands in this workspace"
    ));
}

#[test]
fn synthesis_prompt_carries_full_context() {
    let ctx = LessonContext {
        signal_type: "tool_failure".into(),
        tool_name: Some("grep".into()),
        detail: "timed out after 30s scanning 280k files".into(),
        recent_user_messages: vec!["find the auth config".into()],
        project_hint: Some("astra-engine".into()),
    };
    let prompt = build_synthesis_user_prompt(&ctx);
    // All context present for the LLM to produce a specific lesson
    assert!(prompt.contains("grep"));
    assert!(prompt.contains("280k"));
    assert!(prompt.contains("astra-engine"));
    assert!(prompt.contains("auth config"));
}

// ── 5. Full loop: extract → sanitize → verify prompt-ready ────────────────

#[test]
fn full_loop_lessons_are_prompt_ready() {
    let mut summary = SessionSummary::default();
    summary.tool_failures.insert("grep".into(), 5);
    summary.stall_events = 4;
    summary
        .user_corrections
        .extend(["use rg".to_string(), "add --filter".to_string()]);
    summary.unmet_postconditions = 3;

    let lessons = lesson_extractor::extract_lessons(&summary, "u1", "generic", None);
    assert_eq!(lessons.len(), 4);

    for l in &lessons {
        let sanitized = astra_services::sanitize_for_prompt(&l.action);
        assert_eq!(l.action, sanitized, "action must be sanitization-safe");
        // Template lessons must PASS the basic quality gate (hedging + length)
        // but NOT be blocked by the template blocklist (that's for LLM output).
        assert!(
            lesson_synthesizer::is_high_quality_lesson(&l.action),
            "template lesson must pass basic quality gate: {}",
            l.action
        );
        if l.kind == LessonKind::ToolDeprioritize {
            assert!(
                l.trigger_signal.starts_with("tool_failures:"),
                "trigger must use stable prefix: {}",
                l.trigger_signal
            );
        }
    }
}

// ── 6. Compact/detail content model ────────────────────────────────────

#[test]
fn compact_field_generated_for_long_actions() {
    let hint = astra_services::LessonHint::from_lesson(&astra_services::Lesson {
        id: "id".into(),
        user_id: "u".into(),
        persona: "p".into(),
        workload_tag: None,
        kind: LessonKind::ToolDeprioritize,
        trigger_signal: "tool_failures:grep".into(),
        action: "Use rg --glob '!node_modules' instead of grep -r. This repo has 280k files and grep times out on node_modules traversal every time.".into(),
        confidence: 0.6,
        hit_count: 0,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    });
    assert!(hint.compact.is_some(), "long action should have compact");
    let compact = hint.compact.unwrap();
    assert!(compact.len() < hint.action.len(), "compact must be shorter");
    assert!(compact.contains("rg"), "compact must contain key info");
}

#[test]
fn short_action_has_no_compact() {
    let hint = astra_services::LessonHint::from_lesson(&astra_services::Lesson {
        id: "id".into(),
        user_id: "u".into(),
        persona: "p".into(),
        workload_tag: None,
        kind: LessonKind::PromptShape,
        trigger_signal: "stall_events".into(),
        action: "restate scope before tool calls".into(),
        confidence: 0.6,
        hit_count: 0,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    });
    assert!(hint.compact.is_none(), "short action needs no compact");
}

// ── 7. Lifecycle: session-end lessons vs mid-session observations ──────

#[test]
fn session_end_lessons_are_semantic_t3_mid_session_would_be_working_t4() {
    // Session-end backflow: Corrections → T2, Learnings → T3, all semantic.
    // These are validated by the full session's L1b narrative.
    let narrative = astra_runtime::turn::cloud::session_memory_protocol::SessionMemory::parse(
        "[session-memory:v1]\n# User Corrections\n- Use rg not grep\n# Learnings\n- Repo has 280k files\n"
    ).unwrap();

    let session_end_lessons = lesson_synthesizer::extract_learnings_for_backflow(Some(&narrative));
    for l in &session_end_lessons {
        assert_eq!(
            l.memory_type, "semantic",
            "session-end lessons are durable semantic memories"
        );
        assert!(
            l.trust_tier == "T2" || l.trust_tier == "T3",
            "session-end trust: {} (expected T2 or T3)",
            l.trust_tier
        );
    }

    // Episodic summary is also a durable memory.
    let episodic = lesson_synthesizer::build_episodic_summary("s", 10, Some(&narrative)).unwrap();
    assert_eq!(episodic.memory_type, "episodic");
    assert_eq!(episodic.trust_tier, "T3");
}
