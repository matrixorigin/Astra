//! Contract: when an auto-invoked diagnostic skill produces a
//! [`SkillDiagnosis`], it must (1) attach to [`SelfModel`] via a dedicated
//! builder, (2) appear in the self-awareness prompt section, and (3) clear
//! cleanly on `None` so stale diagnoses never linger.
//!
//! This is the load-bearing end of P0.2: without the prompt-injection step,
//! automatic skill invocation would be a write-only event.

use astra_runtime::self_model::SelfModel;
use astra_skills::auto_invoke::{AutoInvokeCause, SKILL_DIAGNOSIS_SCHEMA_VERSION, SkillDiagnosis};

// ─── Minimal no-I/O SelfModel fixture ───────────────────────────────────────
//
// Matches the pattern already used in self_model_unmet_postconditions.rs —
// build a bare model via serde so the test focuses on ONE behaviour: does
// `with_skill_diagnosis` round-trip through the render path.
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
            "turn_number": 7,
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

fn sample_diagnosis() -> SkillDiagnosis {
    let cause = AutoInvokeCause::ConsecutiveStalls { count: 4 };
    SkillDiagnosis::new(
        "analyze_session",
        &cause,
        "agent looping on grep in deep subtree",
        [
            "tried grep 4× with identical args".to_string(),
            "no new matches since turn 3".to_string(),
        ],
        Some("switch to rg or narrow scope to `src/`".to_string()),
    )
}

// ─── Invariant 1: attached diagnosis is visible on the struct ───────────────
#[test]
fn attached_skill_diagnosis_is_visible_on_the_struct() {
    let diag = sample_diagnosis();
    let sm = minimal_self_model().with_skill_diagnosis(Some(diag.clone()));

    let got = sm.skill_diagnosis.as_ref().expect("diagnosis attached");
    assert_eq!(got.schema_version, SKILL_DIAGNOSIS_SCHEMA_VERSION);
    assert_eq!(got.skill, "analyze_session");
    assert_eq!(got.cause, "consecutive_stalls");
    assert_eq!(got, &diag);
}

// ─── Invariant 2: rendered prompt shows headline + findings + action ────────
//
// The LLM only sees the system prompt. If the struct carries a diagnosis but
// the renderer drops it, auto-invocation is write-only and the whole P0.2
// loop is dead. The header fragment and bullet shape are part of the
// contract.
#[test]
fn prompt_renders_auto_diagnosis_block() {
    let sm = minimal_self_model().with_skill_diagnosis(Some(sample_diagnosis()));
    let rendered = sm.to_system_prompt_section();

    assert!(
        rendered.contains("⚙ Auto-diagnosis [analyze_session]"),
        "expected auto-diagnosis header in prompt, got:\n{rendered}"
    );
    assert!(
        rendered.contains("(cause: consecutive_stalls)"),
        "expected cause tag in header, got:\n{rendered}"
    );
    assert!(
        rendered.contains("agent looping on grep"),
        "expected headline in prompt, got:\n{rendered}"
    );
    assert!(
        rendered.contains("  - tried grep 4× with identical args"),
        "expected first finding as bullet, got:\n{rendered}"
    );
    assert!(
        rendered.contains("  → switch to rg"),
        "expected recommended_action with arrow prefix, got:\n{rendered}"
    );
}

// ─── Invariant 3: None clears any previously-attached diagnosis ─────────────
//
// Once the triggering condition has cleared, stale skill output must not
// keep appearing in the prompt.
#[test]
fn passing_none_clears_previously_attached_diagnosis() {
    let sm = minimal_self_model()
        .with_skill_diagnosis(Some(sample_diagnosis()))
        .with_skill_diagnosis(None);

    assert!(sm.skill_diagnosis.is_none());
    let rendered = sm.to_system_prompt_section();
    assert!(
        !rendered.contains("Auto-diagnosis"),
        "stale diagnosis must not render after None clears, got:\n{rendered}"
    );
}

// ─── Invariant 4: no diagnosis → no auto-diagnosis block in prompt ──────────
#[test]
fn empty_diagnosis_produces_no_auto_diagnosis_block() {
    let sm = minimal_self_model();
    assert!(sm.skill_diagnosis.is_none());
    let rendered = sm.to_system_prompt_section();
    assert!(!rendered.contains("Auto-diagnosis"));
    assert!(!rendered.contains("⚙"));
}

// ─── Invariant 5: serde round-trip preserves the diagnosis ──────────────────
#[test]
fn self_model_serde_roundtrip_preserves_skill_diagnosis() {
    let sm = minimal_self_model().with_skill_diagnosis(Some(sample_diagnosis()));
    let json = serde_json::to_string(&sm).unwrap();
    let back: SelfModel = serde_json::from_str(&json).unwrap();
    assert_eq!(
        back.skill_diagnosis.as_ref().map(|d| d.skill.clone()),
        Some("analyze_session".to_string())
    );
    assert_eq!(
        back.skill_diagnosis.as_ref().map(|d| d.cause.clone()),
        Some("consecutive_stalls".to_string())
    );
}
