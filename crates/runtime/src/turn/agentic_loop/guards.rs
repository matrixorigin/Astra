//! Composable mid-loop guards extracted from `execute_turn_and_ingest_phase`.
//!
//! Each guard is a self-contained fn that checks a condition on
//! `AgenticLoopState`, optionally sets a state flag, and returns a
//! `GuardOutcome`. Guards were previously inlined as ~30-line blocks
//! inside a 1100-line function; extracting them here gives each a
//! documented home, makes them independently testable, and reduces the
//! orchestration function to a readable pipeline.
//!
//! # Adding a new guard
//!
//! 1. Add a `check_<name>(state, cfg) -> GuardOutcome` function below.
//! 2. Register it in `default_guards()`.
//! 3. Write a unit test using a minimal `AgenticLoopState` fixture.
//! 4. Remove the corresponding inline block from
//!    `execute_turn_and_ingest_phase`.

use std::collections::HashSet;

use super::execution_phase::{
    cache_waste_advisory_message, cache_wasteful_tools, parallel_batching_advisory_message,
    should_emit_cache_waste_advisory, should_emit_parallel_batching_advisory,
};
use super::host::{AgenticLoopState, VolatileKind};
use astra_turn_core::headless::body_preview::HeadlessStderrStyle;

// ── Pipeline types ─────────────────────────────────────────────────────

/// Outcome of a single guard evaluation.
#[must_use]
pub(crate) enum GuardOutcome {
    /// Guard did not fire; continue to next guard.
    Pass,
    /// Guard fired: push this signal as volatile advisory evidence, optionally
    /// emit `hint` as a yellow stderr line.
    Advisory {
        message: String,
        kind: VolatileKind,
        hint: Option<String>,
    },
}

/// Shared configuration for all guards in a turn.
#[derive(Clone)]
pub(crate) struct GuardConfig {
    pub parallel_batching_force_streak: usize,
    pub cache_waste_threshold: usize,
}

type GuardFn = fn(&mut AgenticLoopState, &GuardConfig) -> GuardOutcome;

/// Ordered list of guards to run before each LLM call.
///
/// Ordering matters: earlier guards set state flags that later guards
/// check to defer when a stronger intervention is already active
/// (e.g. redundant_reads defers to round_budget_phase1).
pub(crate) fn default_guards() -> Vec<(&'static str, GuardFn)> {
    vec![
        ("observation_reuse", check_observation_reuse),
        ("work_evidence_sufficiency", check_work_evidence_sufficiency),
        (
            "parallel_batching_advisory",
            check_parallel_batching_advisory,
        ),
        ("cache_waste", check_cache_waste),
    ]
}

/// A normalized, typed identity for a self-diagnosis request.  This is
/// deliberately separate from the provider's raw JSON/signature: aliases and
/// omitted defaults that resolve to the same observation are one request, but
/// different facets, horizons, sources, or questions remain distinct.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum ObservationRequestKey {
    Introspect {
        topic: String,
        facet: String,
        depth: String,
        horizon: String,
        source_policy: String,
        include_context: bool,
        format_json: bool,
        run_id: Option<String>,
    },
    Reflect {
        topic: String,
        facet: String,
        depth: String,
        horizon: String,
        source_policy: String,
        include_context: bool,
        last_n: i32,
        question: String,
    },
}

impl ObservationRequestKey {
    fn label(&self) -> &'static str {
        match self {
            Self::Introspect { .. } => "introspect",
            Self::Reflect { .. } => "reflect",
        }
    }
}

/// Parse only the canonical request fields that the observation tools
/// themselves use.  Failed, rejected, artifact-recovery, and unknown calls
/// are not classified; a self-diagnosis guard must never infer intent from
/// display text or a truncated argument preview.
fn observation_request_key(
    record: &astra_services::session_journal::ToolCallRecord,
) -> Option<ObservationRequestKey> {
    if !record.ok || !record.was_executed() {
        return None;
    }
    let args = serde_json::from_str::<serde_json::Value>(record.authoritative_args_full()?).ok()?;
    match record.name.as_str() {
        "introspect" if args.get("artifact").is_none() => {
            let request = astra_turn_core::introspect::IntrospectRequest::from_args(&args);
            let run_id = args
                .get("_run_id")
                .or_else(|| args.get("run_id"))
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToString::to_string);
            Some(ObservationRequestKey::Introspect {
                topic: request.topic.as_str().to_string(),
                facet: request.facet.as_str().to_string(),
                depth: request.depth.as_str().to_string(),
                horizon: request.horizon.as_str().to_string(),
                source_policy: request.source_policy.as_str().to_string(),
                include_context: request.include_context,
                format_json: request.format.is_json(),
                run_id,
            })
        }
        "reflect" => {
            let text_arg = |name: &str| {
                args.get(name)
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
            };
            let include_context = args
                .get("include_context")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let last_n = args
                .get("last_n")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(20)
                .clamp(1, 100) as i32;
            let request =
                astra_services::reflect::ReflectRequest::from_observation_params_with_source(
                    text_arg("topic"),
                    text_arg("facet"),
                    text_arg("depth"),
                    text_arg("horizon"),
                    text_arg("source_policy"),
                    include_context,
                    last_n,
                    args.get("question")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default(),
                );
            Some(ObservationRequestKey::Reflect {
                topic: request.topic.as_str().to_string(),
                facet: request.facet.as_str().to_string(),
                depth: request.depth.as_str().to_string(),
                horizon: request.horizon.as_str().to_string(),
                source_policy: request.source_policy.as_str().to_string(),
                include_context: request.include_context,
                last_n: request.last_n,
                question: request.question,
            })
        }
        _ => None,
    }
}

/// Find duplicate observation requests in the contiguous tail of successful
/// observation calls.  Any intervening tool call is a state-transition
/// boundary and makes a fresh observation potentially meaningful.  The scan is
/// bounded because this is a pre-provider hot-path guard.
fn repeated_observation_request(
    records: &[astra_services::session_journal::ToolCallRecord],
) -> Option<ObservationRequestKey> {
    const MAX_OBSERVATION_TAIL: usize = 8;
    let mut seen = HashSet::new();
    for record in records.iter().rev().take(MAX_OBSERVATION_TAIL) {
        let Some(key) = observation_request_key(record) else {
            break;
        };
        if !seen.insert(key.clone()) {
            return Some(key);
        }
    }
    None
}

fn check_observation_reuse(state: &mut AgenticLoopState, _cfg: &GuardConfig) -> GuardOutcome {
    if state.stall.observation_reuse_advisory_emitted || state.stall.any_behavior_advisory_emitted()
    {
        return GuardOutcome::Pass;
    }
    let records = state
        .stall
        .tool_call_records
        .get(state.stall.observation_reuse_record_floor..)
        .unwrap_or_default();
    let Some(request) = repeated_observation_request(records) else {
        return GuardOutcome::Pass;
    };

    state.stall.observation_reuse_advisory_emitted = true;
    tracing::info!(
        target: "astra::loop_guard",
        tool = request.label(),
        round = state.llm_rounds_completed,
        "typed observation reuse advisory observed"
    );
    GuardOutcome::Advisory {
        message: format!(
            "Observation reuse: the same typed {} request already succeeded without an intervening tool state transition; reuse its evidence or choose one explicitly missing facet. This is advisory only.",
            request.label()
        ),
        kind: VolatileKind::BehaviorAdvisory,
        hint: None,
    }
}

/// Run all registered guards. Model-facing advisory evidence is independent
/// from presentation mode; the caller decides whether returned status hints
/// should be shown to the user.
pub(crate) fn evaluate_guards(
    guards: &[(&str, GuardFn)],
    state: &mut AgenticLoopState,
    cfg: &GuardConfig,
) -> Vec<(HeadlessStderrStyle, String)> {
    let mut hints = Vec::new();
    for (name, guard_fn) in guards {
        match guard_fn(state, cfg) {
            GuardOutcome::Pass => {}
            GuardOutcome::Advisory {
                message,
                kind,
                hint,
            } => {
                state.push_volatile(kind, message.clone());
                tracing::info!(
                    target: "astra::loop_guard",
                    guard = name,
                    round = state.llm_rounds_completed,
                    "guard fired"
                );
                if let Some(hint_text) = hint {
                    hints.push((HeadlessStderrStyle::Yellow, hint_text));
                }
            }
        }
    }
    hints
}

// ═══════════════════════════════════════════════════════════════════════
// Individual guard implementations
// ═══════════════════════════════════════════════════════════════════════

/// Project existing facts, independently of the one-shot behavior guards and
/// optional semantic judges. Do not duplicate the server's settlement gate:
/// journal records do not bind a result to an exact Work attempt, and success
/// alone cannot prove that its expected result is supported.
pub(crate) fn refresh_work_evidence_context(state: &mut AgenticLoopState) {
    state.clear_volatile(VolatileKind::WorkEvidenceContext);
    let (Some(executor), Some(user), Some(session), Some(run)) = (
        state.runtime_tool_executor.as_deref(),
        state.context_manifest_user_id.as_deref(),
        state.current_session_id.as_deref(),
        state.current_run_id.as_deref(),
    ) else {
        return;
    };
    if let Some(payload) = work_evidence_context(
        executor.primary_work_handoff(user, session, run),
        &state.stall.tool_call_records,
    ) {
        state.push_volatile_payload(VolatileKind::WorkEvidenceContext, payload);
    }
}

fn work_evidence_context(
    handoff: crate::server::runtime_tool_executor::PrimaryWorkHandoff,
    records: &[astra_services::session_journal::ToolCallRecord],
) -> Option<serde_json::Value> {
    let crate::server::runtime_tool_executor::PrimaryWorkHandoff::Active { binding } = handoff
    else {
        return None;
    };
    Some(serde_json::json!({
        "schema": "work_evidence_context.v1",
        "binding": binding,
        "recent_observations": bounded_work_observations(records),
        "settlement_readiness": "unknown",
        "authority": "observation_only",
        "delivered_requirement": "A delivered settlement requires a successful non-lifecycle executable result for the assigned attempt; discovery and Work planning/inspection alone do not satisfy this requirement. Success alone does not prove expected_result.",
        "next_action": "Reuse relevant results already available for this assignment. If expected_result is supported, request settle_work_item alone; otherwise obtain the specific missing evidence or truthfully settle blocked/failed. Recent observations do not establish attempt attribution or semantic coverage.",
    }))
}

/// A bounded suffix, not an evidence ledger or a readiness classifier. Stop
/// even at failed lifecycle calls: their disposition cannot establish whether
/// the active assignment changed. Missing history never means no work happened.
fn bounded_work_observations(
    records: &[astra_services::session_journal::ToolCallRecord],
) -> serde_json::Value {
    const WINDOW: usize = 8;
    const FIELD_CHARS: usize = 128;
    let mut observations = Vec::new();
    let mut boundary_seen = false;
    for record in records.iter().rev().take(WINDOW) {
        if matches!(
            record.name.as_str(),
            "start_work" | "run_next_work_item" | "settle_work_item"
        ) {
            boundary_seen = true;
            break;
        }
        // Omit oversized identities rather than manufacture a truncated ID.
        let call_id = record
            .tool_call_id
            .as_deref()
            .filter(|id| id.chars().take(FIELD_CHARS + 1).count() <= FIELD_CHARS);
        observations.push(serde_json::json!({
            "tool": record.name.chars().take(FIELD_CHARS).collect::<String>(),
            "tool_call_id": call_id,
            "disposition": record.effective_disposition(),
            "ok": record.ok,
        }));
    }
    observations.reverse();
    serde_json::json!({
        "scope": "recent_journal_suffix_not_attempt_proof",
        "lifecycle_boundary_seen": boundary_seen,
        "window_truncated": !boundary_seen && records.len() > WINDOW,
        "calls": observations,
    })
}

/// Number of successful, non-mutating tool executions inside one owned
/// WorkItem after which the model should explicitly reassess whether its typed
/// expected result is already supported. The threshold is deliberately above
/// the ordinary small investigation path and never removes tool authority.
const WORK_EVIDENCE_REASSESS_CALLS: usize = astra_turn_core::evaluation::LLM_ROUND_CHURN_THRESHOLD;

/// Count the current WorkItem's evidence path using only typed execution
/// records. The reverse scan is strictly bounded, stops at canonical Work
/// lifecycle boundaries, and declines to classify a path that has mutated the
/// workspace. That keeps this hot-loop check O(1) per model boundary and avoids
/// prompt-text or scenario matching.
fn bounded_read_only_work_evidence_calls(
    records: &[astra_services::session_journal::ToolCallRecord],
) -> Option<usize> {
    let mut successful = 0_usize;
    for record in records
        .iter()
        .rev()
        .take(WORK_EVIDENCE_REASSESS_CALLS.saturating_mul(2))
    {
        if matches!(
            record.name.as_str(),
            "start_work" | "run_next_work_item" | "settle_work_item"
        ) {
            break;
        }
        if !record.was_executed() {
            continue;
        }
        if super::lifecycle::tool_record_is_workspace_mutation(record) {
            return None;
        }
        if record.ok {
            successful = successful.saturating_add(1);
            if successful >= WORK_EVIDENCE_REASSESS_CALLS {
                return Some(successful);
            }
        }
    }
    Some(successful)
}

fn check_work_evidence_sufficiency(
    state: &mut AgenticLoopState,
    _cfg: &GuardConfig,
) -> GuardOutcome {
    if state.stall.any_behavior_advisory_emitted()
        || state
            .runtime_tool_executor
            .as_deref()
            .is_none_or(|executor| !executor.has_active_primary_work_attempt())
    {
        return GuardOutcome::Pass;
    }
    let Some(calls) = bounded_read_only_work_evidence_calls(&state.stall.tool_call_records) else {
        return GuardOutcome::Pass;
    };
    if calls < WORK_EVIDENCE_REASSESS_CALLS {
        return GuardOutcome::Pass;
    }

    state.stall.work_evidence_advisory_emitted = true;
    tracing::info!(
        target: "astra::loop_guard",
        calls,
        round = state.llm_rounds_completed,
        "owned WorkItem evidence-sufficiency advisory observed"
    );
    GuardOutcome::Advisory {
        // Keep decision feedback compact: CurrentUserOnly providers place it
        // on the uncached tail for one request. The count remains in tracing;
        // the model needs only the decision boundary on wire.
        message: "Owned WorkItem: settle_work_item if expected_result is supported; otherwise pursue one specific missing fact.".to_string(),
        kind: VolatileKind::BehaviorAdvisory,
        hint: None,
    }
}

/// Surface batching evidence when the model has produced a long streak of
/// single-tool rounds despite prompt-layer guidance. Catches the
/// "exploratory churn" failure mode (sessions 6566d6a8, bbae8641, 6da9cf8f).
fn check_parallel_batching_advisory(
    state: &mut AgenticLoopState,
    cfg: &GuardConfig,
) -> GuardOutcome {
    if state.stall.parallel_batching_advisory_emitted
        || !should_emit_parallel_batching_advisory(state, cfg.parallel_batching_force_streak)
    {
        return GuardOutcome::Pass;
    }
    state.stall.parallel_batching_advisory_emitted = true;
    let streak = crate::prompts::trailing_single_tool_round_streak(&state.messages);
    let msg = parallel_batching_advisory_message(streak, &state.message);
    tracing::warn!(
        target: "astra::loop_guard",
        tier = "parallel_batching_advisory",
        streak,
        round = state.llm_rounds_completed,
        "behavior advisory observed"
    );
    GuardOutcome::Advisory {
        message: msg,
        kind: VolatileKind::BehaviorAdvisory,
        hint: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_services::session_journal::ToolCallRecord;

    fn successful(name: &str) -> ToolCallRecord {
        ToolCallRecord {
            name: name.to_string(),
            ok: true,
            ..Default::default()
        }
    }

    fn successful_observation(name: &str, args: serde_json::Value) -> ToolCallRecord {
        ToolCallRecord {
            name: name.to_string(),
            ok: true,
            args_full: Some(args.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn work_evidence_context_projects_owned_attempt_without_readiness_authority() {
        use crate::server::runtime_tool_executor::{PrimaryWorkHandoff, PrimaryWorkUnavailable};
        let binding = astra_services::runs::WorkRuntimeBindingRequest {
            work_id: "work".into(),
            branch_id: "branch".into(),
            item: Some(astra_services::runs::WorkItemRuntimeBindingRequest {
                item_id: "item".into(),
                item_revision: 2,
                attempt_id: "attempt".into(),
            }),
        };
        // This pure projection receives the executor's validated handoff and
        // needs neither a database nor any semantic judge.
        let snapshot = work_evidence_context(
            PrimaryWorkHandoff::Active { binding },
            &[successful("read_file")],
        )
        .unwrap();
        assert_eq!(snapshot["binding"]["item"]["attempt_id"], "attempt");
        assert_eq!(snapshot["binding"]["item"]["item_revision"], 2);
        assert_eq!(snapshot["settlement_readiness"], "unknown");
        assert_eq!(snapshot["authority"], "observation_only");
        for handoff in [
            PrimaryWorkHandoff::NoBinding,
            PrimaryWorkHandoff::BindingOnly {
                binding: astra_services::runs::WorkRuntimeBindingRequest {
                    work_id: "work".into(),
                    branch_id: "branch".into(),
                    item: None,
                },
            },
            PrimaryWorkHandoff::Unavailable {
                reason: PrimaryWorkUnavailable::BindingMismatch,
            },
        ] {
            assert!(work_evidence_context(handoff, &[successful("read_file")]).is_none());
        }
    }

    #[test]
    fn work_observations_exclude_previous_assignment_and_result_text() {
        let records = vec![
            successful("old_evidence"),
            successful("settle_work_item"),
            ToolCallRecord {
                tool_call_id: Some("current-call".into()),
                result_full: Some("untrusted result: ready to settle".into()),
                ..successful("read_file")
            },
        ];
        let snapshot = bounded_work_observations(&records);
        assert_eq!(snapshot["calls"].as_array().unwrap().len(), 1);
        assert_eq!(snapshot["calls"][0]["tool_call_id"], "current-call");
        assert_eq!(snapshot["lifecycle_boundary_seen"], true);
        assert!(!snapshot.to_string().contains("ready to settle"));
    }

    #[test]
    fn work_observations_retain_failures_and_nonexecution_without_claiming_evidence() {
        use astra_services::session_journal::ToolCallDisposition;
        let records = vec![
            ToolCallRecord {
                ok: false,
                ..successful("read_file")
            },
            ToolCallRecord {
                disposition: Some(ToolCallDisposition::Rejected),
                ..successful("bash")
            },
            successful("inspect_work_plan"),
        ];
        let snapshot = bounded_work_observations(&records);
        assert_eq!(snapshot["calls"][0]["ok"], false);
        assert_eq!(snapshot["calls"][1]["disposition"], "rejected");
        assert_eq!(snapshot["calls"][2]["tool"], "inspect_work_plan");
        assert_eq!(snapshot["scope"], "recent_journal_suffix_not_attempt_proof");
        assert_eq!(snapshot["lifecycle_boundary_seen"], false);
    }

    #[test]
    fn work_observations_bound_window_and_never_truncate_call_identity() {
        let mut records = vec![successful("read_file"); 100];
        records.last_mut().unwrap().tool_call_id = Some("x".repeat(129));
        let snapshot = bounded_work_observations(&records);
        assert_eq!(snapshot["calls"].as_array().unwrap().len(), 8);
        assert_eq!(snapshot["window_truncated"], true);
        assert!(snapshot["calls"][7]["tool_call_id"].is_null());
        let empty = bounded_work_observations(&[]);
        assert_eq!(empty["lifecycle_boundary_seen"], false);
        assert_eq!(empty["window_truncated"], false);
    }

    #[test]
    fn work_evidence_context_clears_stale_snapshot_without_executor_or_judge() {
        let mut state = crate::turn::agentic_loop::host::make_test_loop_state();
        state.push_volatile_payload(
            VolatileKind::WorkEvidenceContext,
            serde_json::json!({"binding": "old-attempt"}),
        );
        state.push_volatile(VolatileKind::BehaviorAdvisory, "unrelated advisory");
        refresh_work_evidence_context(&mut state);
        assert_eq!(state.volatile_pending.len(), 1);
        assert_eq!(
            state.volatile_pending[0].kind,
            VolatileKind::BehaviorAdvisory
        );
    }

    #[test]
    fn work_evidence_counter_is_bounded_to_current_lifecycle_item() {
        let mut records = (0..20).map(|_| successful("read_file")).collect::<Vec<_>>();
        records.push(successful("settle_work_item"));
        records.extend((0..3).map(|_| successful("grep")));

        assert_eq!(bounded_read_only_work_evidence_calls(&records), Some(3));
    }

    #[test]
    fn work_evidence_counter_reaches_threshold_without_text_classification() {
        let records = (0..WORK_EVIDENCE_REASSESS_CALLS)
            .map(|index| successful(if index % 2 == 0 { "grep" } else { "read_file" }))
            .collect::<Vec<_>>();

        assert_eq!(
            bounded_read_only_work_evidence_calls(&records),
            Some(WORK_EVIDENCE_REASSESS_CALLS)
        );
    }

    #[test]
    fn work_evidence_counter_defers_to_mutating_execution_paths() {
        let mut records = (0..WORK_EVIDENCE_REASSESS_CALLS)
            .map(|_| successful("read_file"))
            .collect::<Vec<_>>();
        records.push(ToolCallRecord {
            name: "apply_patch".to_string(),
            ok: true,
            args_full: Some(r#"{"patch":"*** Begin Patch"}"#.to_string()),
            ..Default::default()
        });

        assert_eq!(bounded_read_only_work_evidence_calls(&records), None);
    }

    #[test]
    fn observation_reuse_normalizes_typed_defaults_and_aliases() {
        let records = vec![
            successful_observation(
                "introspect",
                serde_json::json!({"topic": "runtime", "facet": "session"}),
            ),
            successful_observation("introspect", serde_json::json!({})),
        ];

        assert!(matches!(
            repeated_observation_request(&records),
            Some(ObservationRequestKey::Introspect { .. })
        ));
    }

    #[test]
    fn observation_reuse_stops_at_a_tool_state_transition() {
        let records = vec![
            successful_observation("introspect", serde_json::json!({})),
            successful("read_file"),
            successful_observation("introspect", serde_json::json!({})),
        ];

        assert_eq!(repeated_observation_request(&records), None);
    }

    #[test]
    fn observation_reuse_keeps_distinct_facets_and_reflection_questions() {
        let records = vec![
            successful_observation("introspect", serde_json::json!({"facet": "errors"})),
            successful_observation("introspect", serde_json::json!({"facet": "recent"})),
            successful_observation(
                "reflect",
                serde_json::json!({"facet": "errors", "question": "what changed?"}),
            ),
        ];

        assert_eq!(repeated_observation_request(&records), None);
    }

    #[test]
    fn observation_artifact_recovery_is_not_a_diagnostic_reuse() {
        let records = vec![
            successful_observation("introspect", serde_json::json!({"artifact": "artifact-1"})),
            successful_observation("introspect", serde_json::json!({"artifact": "artifact-1"})),
        ];

        assert_eq!(repeated_observation_request(&records), None);
    }

    #[test]
    fn observation_reuse_is_one_shot_advisory_not_tool_restriction() {
        let mut state = crate::turn::agentic_loop::host::make_test_loop_state();
        state.stall.tool_call_records = vec![
            successful_observation("introspect", serde_json::json!({})),
            successful_observation("introspect", serde_json::json!({})),
        ];
        state.restricted_tools.insert("bash".to_string());
        let cfg = GuardConfig {
            parallel_batching_force_streak: 8,
            cache_waste_threshold: 3,
        };

        assert!(matches!(
            check_observation_reuse(&mut state, &cfg),
            GuardOutcome::Advisory { .. }
        ));
        assert!(state.stall.observation_reuse_advisory_emitted);
        assert_eq!(state.restricted_tools, HashSet::from(["bash".to_string()]));
        assert!(matches!(
            check_observation_reuse(&mut state, &cfg),
            GuardOutcome::Pass
        ));
    }

    #[test]
    fn observation_reuse_does_not_cross_a_user_turn_boundary() {
        let mut state = crate::turn::agentic_loop::host::make_test_loop_state();
        state.stall.tool_call_records = vec![
            successful_observation("introspect", serde_json::json!({})),
            successful_observation("introspect", serde_json::json!({})),
        ];
        let cfg = GuardConfig {
            parallel_batching_force_streak: 8,
            cache_waste_threshold: 3,
        };
        assert!(matches!(
            check_observation_reuse(&mut state, &cfg),
            GuardOutcome::Advisory { .. }
        ));

        state.stall.begin_fresh_user_turn();
        assert!(matches!(
            check_observation_reuse(&mut state, &cfg),
            GuardOutcome::Pass
        ));
    }
}

/// Detect wasteful cache reads (repeated reads hitting the stale-cache
/// guard without follow-up writes) and surface advisory evidence.
///
/// Defers to redundant_reads when both would fire on the same round, and
/// to the same stronger interventions as `check_redundant_reads`.
fn check_cache_waste(state: &mut AgenticLoopState, cfg: &GuardConfig) -> GuardOutcome {
    if state.stall.cache_waste_advisory_emitted
        || !should_emit_cache_waste_advisory(state, cfg.cache_waste_threshold)
    {
        return GuardOutcome::Pass;
    }
    let wasteful = cache_wasteful_tools(state, cfg.cache_waste_threshold);
    if wasteful.is_empty() {
        return GuardOutcome::Pass;
    }
    state.stall.cache_waste_advisory_emitted = true;
    let msg = cache_waste_advisory_message(&wasteful, &state.message);
    tracing::warn!(
        target: "astra::loop_guard",
        tier = "cache_waste_advisory",
        round = state.llm_rounds_completed,
        tools = ?wasteful,
        threshold = cfg.cache_waste_threshold,
        "behavior advisory observed"
    );
    let tool_list = wasteful
        .iter()
        .map(|(tool, count)| format!("{tool} ({count}x)"))
        .collect::<Vec<_>>()
        .join(", ");
    GuardOutcome::Advisory {
        message: msg,
        kind: VolatileKind::BehaviorAdvisory,
        hint: Some(format!(
            "↻ repeated cached tool calls on [{tool_list}]; adding reuse advisory…"
        )),
    }
}
