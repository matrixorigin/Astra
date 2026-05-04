// Contract: cross-session lessons must flow from `agent_lessons` into the
// next session's [`SelfModel`] and appear in the self-awareness prompt, so
// the agent can act on what it learned in prior sessions.
//
// The unit layer (no DB) pins the [`SelfModel::with_lessons`] builder and
// the render shape. The live DB E2E lives in `self_model_lessons_db_it.rs`
// and is gated on `ASTRA_TEST_DB_IT=1`.

use astra_runtime::self_model::{LessonHint, SelfModel};

fn minimal_self_model() -> SelfModel {
    let empty = serde_json::json!({
        "capabilities": {
            "total_tools": 0,
            "tool_names": [],
            "tool_health": [],
            "deprioritized_tools": [],
            "pinned_tools": [],
            "skills": [],
            "boosted_tools": [],
            "widen_selection_pending": false,
            "outcome_memory": [],
        },
        "state": {
            "turn_number": 1,
            "token_budget": null,
            "scenario": null,
            "active_experiment": null,
            "session_elapsed_secs": 0,
            "correction_count": 0,
            "compression_count": 0,
        },
        "goals": {
            "goal": null,
            "session_goal": null,
            "plan_goal": null,
            "tracked_goal": null,
            "goal_source": "none",
            "tracking_status": "idle",
            "progress": null,
            "recent_milestones": [],
            "milestone_count": 0,
        },
        "recent_signals": [],
        "constraints": {
            "max_mutations_per_turn": 2,
            "config_drift_ceiling": 0.3,
            "min_tool_pool_size": 5,
            "token_reserve_fraction": 0.2,
        }
    });
    serde_json::from_value(empty).expect("minimal SelfModel fixture")
}

fn lesson(kind: &str, trigger: &str, action: &str, tag: Option<&str>) -> LessonHint {
    LessonHint {
        kind: astra_services::LessonKind::parse_tag(kind)
            .unwrap_or(astra_services::LessonKind::PromptShape),
        trigger_signal: trigger.to_string(),
        action: action.to_string(),
        workload_tag: tag.map(str::to_string),
        compact: None,
    }
}

// ─── Invariant 1: builder round-trips lessons onto the struct ────────────────

#[test]
fn attached_lessons_are_visible_on_struct() {
    let lessons = vec![
        lesson(
            "tool_deprioritize",
            "3 consecutive stalls on grep",
            "switch to rg",
            None,
        ),
        lesson(
            "prompt_shape",
            "selector picked wrong tool",
            "restate scope before tool call",
            Some("code-review"),
        ),
    ];
    let sm = minimal_self_model().with_lessons(lessons.clone());
    assert_eq!(sm.lessons.len(), 2);
    assert_eq!(sm.lessons, lessons);
}

// ─── Invariant 2: prompt renders a dedicated lessons header ──────────────────

#[test]
fn prompt_renders_lessons_block() {
    let sm = minimal_self_model().with_lessons(vec![lesson(
        "tool_deprioritize",
        "3 stalls on grep",
        "deprioritize grep for regex-heavy tasks",
        None,
    )]);
    let rendered = sm.to_system_prompt_section();
    assert!(
        rendered.contains("📚 Lessons from prior sessions"),
        "expected lessons header, got:\n{rendered}"
    );
    assert!(
        rendered.contains("[tool_deprioritize]"),
        "expected kind tag in bullet, got:\n{rendered}"
    );
    assert!(
        rendered.contains("3 stalls on grep"),
        "expected trigger in bullet, got:\n{rendered}"
    );
    assert!(
        rendered.contains("deprioritize grep for regex-heavy tasks"),
        "expected action in bullet, got:\n{rendered}"
    );
}

// ─── Invariant 3: workload-tagged lessons show scope marker ──────────────────

#[test]
fn tagged_lesson_renders_scope_marker() {
    let sm = minimal_self_model().with_lessons(vec![lesson(
        "prompt_shape",
        "selector drifted",
        "restate scope",
        Some("code-review"),
    )]);
    let rendered = sm.to_system_prompt_section();
    assert!(
        rendered.contains("[prompt_shape @code-review]"),
        "expected scope marker `@code-review` in tag, got:\n{rendered}"
    );
}

// ─── Invariant 4: top-5 cap with overflow marker ─────────────────────────────

#[test]
fn prompt_caps_lessons_to_five_with_overflow_marker() {
    let many: Vec<LessonHint> = (0..8)
        .map(|i| lesson("tool_boost", &format!("sig {i}"), &format!("act {i}"), None))
        .collect();
    let sm = minimal_self_model().with_lessons(many);
    let rendered = sm.to_system_prompt_section();

    // First five must render.
    for i in 0..5 {
        assert!(
            rendered.contains(&format!("sig {i}")),
            "missing sig {i} in:\n{rendered}"
        );
    }
    // Overflow marker
    assert!(
        rendered.contains("… 3 more"),
        "expected overflow marker, got:\n{rendered}"
    );
    // Trailing entries must NOT be rendered individually.
    assert!(!rendered.contains("sig 5"));
    assert!(!rendered.contains("sig 6"));
}

// ─── Invariant 5: empty lesson vec produces no block ─────────────────────────

#[test]
fn no_lessons_produces_no_lessons_block() {
    let sm = minimal_self_model();
    assert!(sm.lessons.is_empty());
    let rendered = sm.to_system_prompt_section();
    assert!(!rendered.contains("Lessons from prior sessions"));
    assert!(!rendered.contains("📚"));
}

// ─── Invariant 6: empty vec clears previously-attached lessons ───────────────

#[test]
fn passing_empty_clears_previously_attached_lessons() {
    let sm = minimal_self_model()
        .with_lessons(vec![lesson("tool_boost", "t", "a", None)])
        .with_lessons(Vec::new());
    assert!(sm.lessons.is_empty());
    let rendered = sm.to_system_prompt_section();
    assert!(!rendered.contains("Lessons from prior sessions"));
}

// ─── Invariant 7: LessonHint::from_lesson drops metadata fields ──────────────

#[test]
fn from_lesson_projects_only_prompt_relevant_fields() {
    use astra_services::{Lesson, LessonKind};
    use chrono::Utc;

    let persisted = Lesson {
        id: "uuid-123".into(),
        user_id: "u1".into(),
        persona: "generic".into(),
        workload_tag: Some("debug".into()),
        kind: LessonKind::ErrorRecovery,
        trigger_signal: "repeated EACCES on scratch dir".into(),
        action: "ensure chmod before write".into(),
        confidence: 0.82,
        hit_count: 17,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    let hint = LessonHint::from_lesson(&persisted);
    assert_eq!(hint.kind, astra_services::LessonKind::ErrorRecovery);
    assert_eq!(hint.trigger_signal, "repeated EACCES on scratch dir");
    assert_eq!(hint.action, "ensure chmod before write");
    assert_eq!(hint.workload_tag.as_deref(), Some("debug"));
    // Serialised form must not leak id / confidence / hit_count / timestamps.
    let json = serde_json::to_string(&hint).unwrap();
    for leaked in [
        "uuid-123",
        "confidence",
        "hit_count",
        "created_at",
        "updated_at",
    ] {
        assert!(
            !json.contains(leaked),
            "projection leaked metadata field `{leaked}`: {json}"
        );
    }
}

// ─── Invariant 8: serde round-trip preserves lessons ─────────────────────────

#[test]
fn self_model_serde_roundtrip_preserves_lessons() {
    let sm =
        minimal_self_model().with_lessons(vec![lesson("tool_deprioritize", "t", "a", Some("x"))]);
    let json = serde_json::to_string(&sm).unwrap();
    let back: SelfModel = serde_json::from_str(&json).unwrap();
    assert_eq!(back.lessons.len(), 1);
    assert_eq!(back.lessons[0].workload_tag.as_deref(), Some("x"));
}

// ── E: Relevance filtering — tool-specific lessons gate on tool_names ────────

fn model_with_tools(tools: &[&str]) -> SelfModel {
    let mut sm = minimal_self_model();
    sm.capabilities.tool_names = tools.iter().map(|s| s.to_string()).collect();
    sm
}

#[test]
fn tool_deprioritize_lesson_hidden_when_tool_not_in_capabilities() {
    // "deprioritize grep" should NOT appear when grep is not available.
    let sm = model_with_tools(&["bash", "read_file"]).with_lessons(vec![lesson(
        "tool_deprioritize",
        "tool_failures:grep",
        "avoid grep",
        None,
    )]);
    let rendered = sm.to_system_prompt_section();
    assert!(
        !rendered.contains("tool_failures:grep"),
        "grep lesson should be hidden when grep is not in tool_names, got:\n{rendered}"
    );
}

#[test]
fn tool_deprioritize_lesson_shown_when_tool_in_capabilities() {
    let sm = model_with_tools(&["grep", "bash"]).with_lessons(vec![lesson(
        "tool_deprioritize",
        "tool_failures:grep",
        "avoid grep",
        None,
    )]);
    let rendered = sm.to_system_prompt_section();
    assert!(
        rendered.contains("tool_failures:grep"),
        "grep lesson should be shown when grep is in tool_names, got:\n{rendered}"
    );
}

#[test]
fn prompt_shape_lesson_always_shown_regardless_of_tools() {
    // Non-tool lessons (PromptShape, PostconditionPattern, ErrorRecovery)
    // are general advice — always rendered.
    let sm = model_with_tools(&["bash"]).with_lessons(vec![lesson(
        "prompt_shape",
        "stall_events",
        "restate scope",
        None,
    )]);
    let rendered = sm.to_system_prompt_section();
    assert!(
        rendered.contains("stall_events"),
        "prompt_shape lesson must always render, got:\n{rendered}"
    );
}

#[test]
fn mixed_lessons_filter_correctly() {
    let sm = model_with_tools(&["rg"]).with_lessons(vec![
        lesson(
            "tool_deprioritize",
            "tool_failures:grep",
            "avoid grep",
            None,
        ), // grep NOT available → hidden
        lesson("tool_deprioritize", "tool_failures:rg", "avoid rg", None), // rg available → shown
        lesson("prompt_shape", "stall_events", "restate scope", None),     // always shown
    ]);
    let rendered = sm.to_system_prompt_section();
    assert!(!rendered.contains("tool_failures:grep"));
    assert!(rendered.contains("tool_failures:rg"));
    assert!(rendered.contains("stall_events"));
}

// ─── Invariant 10: pressure-adaptive rendering ──────────────────────────────

fn model_with_pressure(pressure: f64) -> SelfModel {
    let json = serde_json::json!({
        "capabilities": {
            "total_tools": 0, "tool_names": [], "tool_health": [],
            "deprioritized_tools": [], "pinned_tools": [], "skills": [],
            "boosted_tools": [], "widen_selection_pending": false,
            "outcome_memory": [],
        },
        "state": {
            "turn_number": 10, "scenario": null,
            "active_experiment": null, "session_elapsed_secs": 0,
            "correction_count": 0, "compression_count": 0,
            "token_budget": {
                "max_tokens": 100000,
                "total_used": (100000.0 * pressure) as u64,
                "remaining": (100000.0 * (1.0 - pressure)) as u64,
                "pressure": pressure,
                "compression_triggered": false,
            },
        },
        "goals": {
            "goal": null, "session_goal": null, "plan_goal": null,
            "tracked_goal": null, "goal_source": "none",
            "tracking_status": "idle", "progress": null,
            "recent_milestones": [], "milestone_count": 0,
        },
        "recent_signals": [],
        "constraints": {
            "max_mutations_per_turn": 2, "config_drift_ceiling": 0.3,
            "min_tool_pool_size": 5, "token_reserve_fraction": 0.2,
        }
    });
    serde_json::from_value(json).expect("model with pressure")
}

#[test]
fn high_pressure_shows_fewer_compact_lessons() {
    let lessons: Vec<LessonHint> = (0..6)
        .map(|i| LessonHint {
            kind: astra_services::LessonKind::PromptShape,
            trigger_signal: format!("sig_{i}"),
            action: format!("This is a detailed action for lesson {i}. It contains enough text to have a compact version generated automatically by the make_compact function."),
            compact: Some(format!("compact {i}")),
            workload_tag: None,
        })
        .collect();

    // Low pressure: 5 lessons, full text
    let low = model_with_pressure(0.3).with_lessons(lessons.clone());
    let rendered_low = low.to_system_prompt_section();
    assert!(rendered_low.contains("sig_4"), "low pressure shows 5");
    assert!(!rendered_low.contains("sig_5"), "6th hidden");
    assert!(
        rendered_low.contains("detailed action"),
        "low pressure uses full text"
    );

    // High pressure: 2 lessons, compact text
    let high = model_with_pressure(0.8).with_lessons(lessons);
    let rendered_high = high.to_system_prompt_section();
    assert!(rendered_high.contains("sig_0"), "high shows first");
    assert!(rendered_high.contains("sig_1"), "high shows second");
    assert!(
        !rendered_high.contains("sig_2"),
        "3rd hidden at high pressure"
    );
    assert!(
        rendered_high.contains("compact 0"),
        "high pressure uses compact"
    );
    assert!(
        !rendered_high.contains("detailed action"),
        "high pressure hides full text"
    );
}
