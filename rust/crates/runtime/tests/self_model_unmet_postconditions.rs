//! Contract: unmet postconditions from the most recent `ActionPlan` execution
//! must reach the `SelfModel`, be rendered in the self-awareness prompt, and
//! do so without unbounded growth or noise on empty input.
//!
//! This is the load-bearing end of slice D: unless the LLM *sees* unmet
//! postconditions on the next turn, typed postcondition diff is just a pretty
//! in-memory data structure with no behavioural effect.

use astra_plan::action_plan::PostCondition;
use astra_runtime::self_model::{SelfModel, UnmetPostCondition};

// ─── Minimal no-I/O SelfModel for testing field wiring ──────────────────────
//
// `SelfModel::snapshot_with_strategy` takes 20 arguments. For a unit test we
// only need a bare SelfModel, so we build one via `serde_json::from_value`
// with default-ish fields — deliberately avoiding a dependency on the full
// runtime composition. This keeps the test honest about ONE behaviour:
// does `with_unmet_postconditions` round-trip through the render path.

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

// ─── Invariant 1: unmet postconditions attached → visible in struct ─────────
#[test]
fn attached_unmet_postconditions_are_visible_on_the_struct() {
    let unmet = vec![
        UnmetPostCondition::from(&PostCondition::ToolCallSucceeded { action_index: 2 }),
        UnmetPostCondition::from(&PostCondition::ToolCallSucceeded { action_index: 5 }),
    ];
    let sm = minimal_self_model().with_unmet_postconditions(unmet.clone());

    assert_eq!(sm.unmet_postconditions.len(), 2);
    assert_eq!(sm.unmet_postconditions[0].action_index, 2);
    assert_eq!(sm.unmet_postconditions[1].action_index, 5);
}

// ─── Invariant 2: prompt section renders a dedicated header for unmet ───────
//
// The LLM only sees the system prompt. If the struct carries unmet but the
// renderer drops it, the whole feedback loop is dead. The header string is
// part of the contract.
#[test]
fn prompt_renders_unmet_postconditions_under_a_dedicated_header() {
    let unmet = vec![UnmetPostCondition::from(&PostCondition::ToolCallSucceeded {
        action_index: 3,
    })];
    let sm = minimal_self_model().with_unmet_postconditions(unmet);
    let rendered = sm.to_system_prompt_section();

    assert!(
        rendered.contains("Unmet postconditions"),
        "expected dedicated header in prompt, got:\n{rendered}",
    );
}

// ─── Invariant 3: rendered output includes the action index ─────────────────
//
// "Unmet postconditions: 1" is useless — the LLM cannot locate the failure.
// The render must include action_index so the model can correlate with the
// plan it produced.
#[test]
fn prompt_renders_action_index_for_each_unmet() {
    let unmet = vec![
        UnmetPostCondition::from(&PostCondition::ToolCallSucceeded { action_index: 3 }),
        UnmetPostCondition::from(&PostCondition::ToolCallSucceeded { action_index: 9 }),
    ];
    let sm = minimal_self_model().with_unmet_postconditions(unmet);
    let rendered = sm.to_system_prompt_section();

    assert!(
        rendered.contains("action 3") || rendered.contains("#3"),
        "action index 3 missing from render:\n{rendered}",
    );
    assert!(
        rendered.contains("action 9") || rendered.contains("#9"),
        "action index 9 missing from render:\n{rendered}",
    );
}

// ─── Invariant 4: empty unmet ⇒ no noise in the prompt ──────────────────────
//
// Every rendered section competes for the model's attention. When there is
// nothing to report we must emit NOTHING — no header, no empty bullet list.
#[test]
fn empty_unmet_does_not_add_any_noise_to_prompt() {
    let sm = minimal_self_model(); // no unmet attached at all
    let rendered = sm.to_system_prompt_section();
    assert!(
        !rendered.contains("Unmet postconditions"),
        "empty unmet must not render a header; got:\n{rendered}",
    );

    // And explicitly empty-attached must also render nothing.
    let sm_empty = minimal_self_model().with_unmet_postconditions(vec![]);
    let rendered_empty = sm_empty.to_system_prompt_section();
    assert!(
        !rendered_empty.contains("Unmet postconditions"),
        "empty-attached unmet must not render a header; got:\n{rendered_empty}",
    );
}

// ─── Invariant 5: render is bounded — can't blow up the prompt ──────────────
//
// If the executor produced 500 unmet postconditions the renderer must NOT
// spew 500 lines. It renders the first N and indicates truncation.
#[test]
fn prompt_bounded_for_large_unmet_lists_with_truncation_marker() {
    let many: Vec<UnmetPostCondition> = (0..500)
        .map(|i| UnmetPostCondition::from(&PostCondition::ToolCallSucceeded { action_index: i }))
        .collect();
    let sm = minimal_self_model().with_unmet_postconditions(many);
    let rendered = sm.to_system_prompt_section();

    // Bound check: the section dedicated to unmet postconditions must not
    // contain all 500 indices verbatim. We assert a generous upper bound.
    let count_hits = (0u32..500)
        .filter(|i| rendered.contains(&format!("action {i}")) || rendered.contains(&format!("#{i}")))
        .count();
    assert!(
        count_hits <= 20,
        "render appears unbounded: found {count_hits} action indices in prompt",
    );
    // And a truncation marker must signal the elision explicitly.
    assert!(
        rendered.contains("…") || rendered.to_lowercase().contains("more"),
        "large list must show a truncation marker; got:\n{rendered}",
    );
}

// ─── Invariant 6: serde round-trip preserves unmet field ────────────────────
//
// Journal and CSL serialize SelfModel snapshots. If `unmet_postconditions`
// drops on re-read the whole feedback loop breaks across process boundaries.
#[test]
fn self_model_round_trips_unmet_postconditions_through_serde() {
    let unmet = vec![
        UnmetPostCondition::from(&PostCondition::ToolCallSucceeded { action_index: 1 }),
        UnmetPostCondition::from(&PostCondition::ToolCallSucceeded { action_index: 4 }),
    ];
    let sm = minimal_self_model().with_unmet_postconditions(unmet);

    let encoded = serde_json::to_string(&sm).unwrap();
    let decoded: SelfModel = serde_json::from_str(&encoded).unwrap();

    assert_eq!(decoded.unmet_postconditions.len(), 2);
    assert_eq!(decoded.unmet_postconditions[0].action_index, 1);
    assert_eq!(decoded.unmet_postconditions[1].action_index, 4);
}

// ─── Invariant 7: missing `unmet_postconditions` in older JSON still decodes ─
//
// Existing journal files predate this field. SelfModel must default to empty
// rather than fail to parse. This encodes the forward-compatible contract.
#[test]
fn self_model_decodes_legacy_json_without_unmet_field() {
    let legacy = serde_json::json!({
        "capabilities": {
            "total_tools": 0, "tool_names": [], "tool_health": [],
            "deprioritized_tools": [], "pinned_tools": [], "skills": [],
            "boosted_tools": [], "widen_selection_pending": false, "outcome_memory": []
        },
        "state": {
            "turn_number": 1, "token_budget": null, "scenario": null,
            "active_experiment": null, "session_elapsed_secs": 0,
            "correction_count": 0, "compression_count": 0
        },
        "goals": {
            "goal": null, "session_goal": null, "plan_goal": null,
            "tracked_goal": null, "goal_source": "none", "tracking_status": "idle",
            "progress": null, "recent_milestones": [], "milestone_count": 0
        },
        "recent_signals": [],
        "constraints": {
            "max_mutations_per_turn": 2, "config_drift_ceiling": 0.3,
            "min_tool_pool_size": 5, "token_reserve_fraction": 0.2
        }
    });
    let sm: SelfModel = serde_json::from_value(legacy).expect("legacy JSON must decode");
    assert!(sm.unmet_postconditions.is_empty());
}
