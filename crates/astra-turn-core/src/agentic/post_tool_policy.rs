//! After a headless tool round: record TurnGuard observations and checkpoint.
//!
//! First-principles rule: behavioral signals are evidence, not authority.
//! Repeated reads, exploratory churn, or tool-shape mistakes may indicate
//! waste, but they do not prove the model must be interrupted. This policy
//! records verdicts for telemetry/checkpointing and leaves loop control to
//! explicit safety/capability/budget boundaries.

use std::collections::HashSet;

use serde_json::Value;

use crate::guardrails::turn_guard::{TurnGuard, VerdictSeverity};
use crate::guardrails::verdict_audit::AgenticVerdictAuditEvent;
use crate::interaction_types::TurnInteractionMode;
use astra_pipeline::step_checkpoint;
use astra_pipeline::step_protocol::StepCheckpoint;
use astra_pipeline::step_recorder::StepRecorder;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PolicyAdvisory {
    pub kind: String,
    pub severity: String,
    pub evidence: Vec<String>,
    pub recommendation: String,
    pub ttl_rounds: u32,
    pub dedupe_key: String,
}

#[must_use]
pub fn policy_advisory_bundle_value(advisories: &[PolicyAdvisory]) -> Option<Value> {
    if advisories.is_empty() {
        return None;
    }
    Some(serde_json::json!({
        "schema": "policy_advisory.v1",
        "authority": "advisory_evidence_only",
        "advisories": advisories,
    }))
}

pub struct AgenticPostToolPolicyRequest<'a> {
    pub turn_index: u32,
    pub messages: &'a mut Vec<Value>,
    pub turn_guard: &'a mut TurnGuard,
    pub verdict_events: &'a mut Vec<AgenticVerdictAuditEvent>,
    pub restricted_tools: &'a mut HashSet<String>,
    pub remaining_turns: &'a mut usize,
    pub step_recorder: &'a mut StepRecorder,
    pub current_user_id: Option<&'a String>,
    pub current_session_id: Option<&'a String>,
    /// Sticky workspace safety state that must accompany any warning-driven
    /// heavy checkpoint written by this policy.  The policy is not an
    /// authority source; it only preserves the runtime's existing state.
    pub workspace_observation_quarantine:
        Option<&'a astra_pipeline::step_protocol::WorkspaceObservationQuarantineV1>,
    pub max_turns: usize,
    pub recent_tools: &'a [String],
    pub last_heavy_checkpoint: &'a mut Option<StepCheckpoint>,
    pub interaction_mode: TurnInteractionMode,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AgenticPostToolPolicyOutcome {
    ProceedEndTurn { advisories: Vec<PolicyAdvisory> },
}

/// Maps [`AgenticPostToolPolicyOutcome`] for host loop control (CLI maps to `AgenticLoopTurnExit`).
#[derive(Debug, PartialEq, Eq)]
pub enum AgenticPostToolIterationControl {
    ProceedEndTurn { advisories: Vec<PolicyAdvisory> },
}

#[must_use]
pub fn map_post_tool_policy_outcome(
    outcome: AgenticPostToolPolicyOutcome,
) -> AgenticPostToolIterationControl {
    match outcome {
        AgenticPostToolPolicyOutcome::ProceedEndTurn { advisories } => {
            AgenticPostToolIterationControl::ProceedEndTurn { advisories }
        }
    }
}

pub fn apply_agentic_post_tool_policy(
    ctx: AgenticPostToolPolicyRequest<'_>,
) -> AgenticPostToolPolicyOutcome {
    let AgenticPostToolPolicyRequest {
        turn_index,
        messages,
        turn_guard,
        verdict_events,
        restricted_tools,
        remaining_turns,
        step_recorder,
        current_user_id,
        current_session_id,
        workspace_observation_quarantine,
        max_turns,
        recent_tools,
        last_heavy_checkpoint,
        interaction_mode,
    } = ctx;
    let mut advisories = Vec::new();

    {
        let verdict = turn_guard.evaluate();

        if verdict.severity > VerdictSeverity::Healthy {
            let severity_str = match verdict.severity {
                VerdictSeverity::Critical => "critical",
                VerdictSeverity::Warning => "warning",
                VerdictSeverity::Info => "info",
                VerdictSeverity::Healthy => unreachable!(),
            };
            let health_summary = turn_guard.health.summary();
            let health_avoidance_tools = turn_guard
                .health
                .health_avoidance_tools()
                .iter()
                .map(|tool| (*tool).to_string())
                .collect::<Vec<_>>();
            let timeout_dominant_tools = turn_guard
                .health
                .timeout_dominant_tools()
                .iter()
                .map(|tool| (*tool).to_string())
                .collect::<Vec<_>>();
            verdict_events.push(AgenticVerdictAuditEvent {
                turn: turn_index,
                severity: severity_str.to_string(),
                injections: verdict.injections.clone(),
                avoid_tools: verdict.avoid_tools.clone(),
                health_avoidance_tools,
                advisory_threshold_reached: verdict.advisory_threshold_reached,
                nudge_count: turn_guard.nudge_count,
                interaction_mode: interaction_mode.label().to_string(),
                recent_error_pressure: turn_guard.errors.recent_error_pressure(),
                recent_timeout_pressure: turn_guard
                    .errors
                    .recent_error_count(crate::error_recovery::ErrorCategory::ToolTimeout),
                total_errors: turn_guard.errors.total_errors,
                health_avoidance_count: health_summary.health_avoidance_count,
                total_timeouts: health_summary.total_timeouts,
                timeout_dominant_tools,
                total_cache_hits: health_summary.total_cache_hits,
                flaky_count: health_summary.flaky_count,
            });
            if let Some(advisory) = policy_advisory_from_verdict(&verdict) {
                advisories.push(advisory);
            }
        }

        // `avoid_tools` is advisory stall-recovery guidance at every severity.
        // Tool failures are observations, not capability facts: the model may
        // have used the tool incorrectly, passed stale arguments, or hit a
        // transient environment issue. Do not mutate `restricted_tools` from a
        // verdict; only explicit capability/permission policy may do that.
        // Likewise, do not turn verdict severity into hidden budget penalties.
        // The model should get the configured turn budget unless an explicit
        // budget/capability/safety boundary is reached.

        let severity_label = match verdict.severity {
            VerdictSeverity::Critical => "critical",
            VerdictSeverity::Warning => "warning",
            VerdictSeverity::Info => "info",
            VerdictSeverity::Healthy => "healthy",
        };
        step_recorder.record_verdict(
            severity_label,
            verdict.stall_detected,
            verdict.is_diverging,
            verdict.advisory_threshold_reached,
            verdict.injections.len(),
        );

        // Ordinary tool completion is already journaled and receives a
        // terminal heavy checkpoint from the runtime. A durable, fsync-backed
        // full-message checkpoint after every healthy tool round turns an
        // O(rounds) transcript into O(rounds²) serialized bytes. Persist here
        // only when the policy discovered material recovery evidence; healthy
        // and informational observations do not define a new recovery boundary.
        let checkpoint_blocked_tools = checkpoint_blocked_tools(restricted_tools);
        if verdict.severity >= VerdictSeverity::Warning
            && let Some(sid) = current_session_id
        {
            let checkpoint_messages =
                crate::runtime_scaffolding::sanitize_durable_message_values(messages.clone());
            if let Some(heavy) = step_recorder.build_heavy_checkpoint(
                &checkpoint_messages,
                0,
                (*remaining_turns).min(max_turns) as u32,
                &checkpoint_blocked_tools,
                recent_tools,
            ) {
                let cp = StepCheckpoint::Heavy(Box::new(heavy));
                let mut cp = cp;
                if let StepCheckpoint::Heavy(heavy) = &mut cp {
                    heavy.workspace_observation_quarantine =
                        workspace_observation_quarantine.cloned();
                }
                let _ = step_checkpoint::write_step_checkpoint(
                    current_user_id.map(|s| s.as_str()).unwrap_or(""),
                    sid,
                    step_recorder.summary().checkpoints,
                    &cp,
                );
                *last_heavy_checkpoint = Some(cp);
            }
        }

        // The threshold remains observable in verdict events and checkpoints,
        // but does not imply failure or stopping authority.
    }

    AgenticPostToolPolicyOutcome::ProceedEndTurn { advisories }
}

fn policy_advisory_from_verdict(
    verdict: &crate::guardrails::turn_guard::TurnVerdict,
) -> Option<PolicyAdvisory> {
    if verdict.severity < VerdictSeverity::Warning {
        return None;
    }
    let severity = match verdict.severity {
        VerdictSeverity::Critical => "critical",
        VerdictSeverity::Warning => "warning",
        VerdictSeverity::Info => "info",
        VerdictSeverity::Healthy => "healthy",
    };
    let kind = if verdict.stall_detected {
        "stall"
    } else if verdict.is_diverging {
        "divergence"
    } else if !verdict.avoid_tools.is_empty() {
        "tool_behavior"
    } else if verdict.advisory_threshold_reached {
        "behavior_threshold"
    } else {
        "policy"
    };
    let mut evidence = Vec::new();
    if verdict.stall_detected {
        evidence.push("TurnGuard detected a repeated/stalled tool-use pattern.".to_string());
    }
    if verdict.is_diverging {
        evidence
            .push("TurnGuard detected exploratory divergence from the current task.".to_string());
    }
    if !verdict.avoid_tools.is_empty() {
        let mut tools = verdict.avoid_tools.clone();
        tools.sort();
        tools.dedup();
        evidence.push(format!("Advisory avoid_tools: {}.", tools.join(", ")));
    }
    if verdict.advisory_threshold_reached {
        evidence.push(
            "TurnGuard recorded that a configured behavioral threshold was reached.".to_string(),
        );
    }
    if evidence.is_empty() {
        evidence.push("TurnGuard emitted a warning-or-higher behavioral verdict.".to_string());
    }
    let mut dedupe_parts = vec![kind.to_string(), severity.to_string()];
    let mut tools = verdict.avoid_tools.clone();
    tools.sort();
    tools.dedup();
    dedupe_parts.extend(tools);
    Some(PolicyAdvisory {
        kind: kind.to_string(),
        severity: severity.to_string(),
        evidence,
        recommendation: "Consider this evidence before the next tool call: continue, change approach, or synthesize based on the user goal. Do not treat this advisory as a tool restriction.".to_string(),
        ttl_rounds: 1,
        dedupe_key: dedupe_parts.join("|"),
    })
}

fn checkpoint_blocked_tools(restricted_tools: &HashSet<String>) -> Vec<String> {
    let mut blocked_tools: Vec<String> = restricted_tools.iter().cloned().collect();
    blocked_tools.sort();
    blocked_tools.dedup();
    blocked_tools
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn proceed_advisories(out: AgenticPostToolPolicyOutcome) -> Vec<PolicyAdvisory> {
        match out {
            AgenticPostToolPolicyOutcome::ProceedEndTurn { advisories } => advisories,
        }
    }

    #[test]
    fn healthy_guard_proceeds_end_turn() {
        let mut messages = Vec::new();
        let mut verdict_events = Vec::new();
        let mut restricted_tools = HashSet::new();
        let mut remaining_turns = 10usize;
        let mut step_recorder =
            StepRecorder::with_persistence_for_run("uid", "sid", "tid", "test-run");
        let mut last_heavy_checkpoint: Option<StepCheckpoint> = None;
        let mut turn_guard = TurnGuard::new();

        let out = apply_agentic_post_tool_policy(AgenticPostToolPolicyRequest {
            turn_index: 0,
            messages: &mut messages,
            turn_guard: &mut turn_guard,
            verdict_events: &mut verdict_events,
            restricted_tools: &mut restricted_tools,
            remaining_turns: &mut remaining_turns,
            step_recorder: &mut step_recorder,
            current_user_id: None,
            current_session_id: None,
            workspace_observation_quarantine: None,
            max_turns: 8,
            recent_tools: &[],
            last_heavy_checkpoint: &mut last_heavy_checkpoint,
            interaction_mode: TurnInteractionMode::Prompt,
        });

        assert!(proceed_advisories(out).is_empty());
    }

    #[test]
    #[serial_test::serial(session_journal_dir)]
    fn healthy_rounds_do_not_multiply_heavy_checkpoint_storage() {
        let sessions_dir = tempfile::tempdir().expect("temporary session journal");
        let _journal_guard =
            astra_services::session_journal::JournalDirGuard::new(sessions_dir.path());
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before epoch")
            .as_nanos();
        let user_id = format!("uid-{suffix}");
        let session_id = format!("healthy-checkpoint-{suffix}");
        let mut messages = vec![json!({"role": "user", "content": "inspect"})];
        let mut verdict_events = Vec::new();
        let mut restricted_tools = HashSet::new();
        let mut remaining_turns = 128usize;
        let mut step_recorder =
            StepRecorder::with_persistence_for_run(&user_id, &session_id, "tid", "test-run");
        step_recorder.begin_turn(0);
        let mut last_heavy_checkpoint = None;
        let mut turn_guard = TurnGuard::new();

        for turn_index in 0..64 {
            let out = apply_agentic_post_tool_policy(AgenticPostToolPolicyRequest {
                turn_index,
                messages: &mut messages,
                turn_guard: &mut turn_guard,
                verdict_events: &mut verdict_events,
                restricted_tools: &mut restricted_tools,
                remaining_turns: &mut remaining_turns,
                step_recorder: &mut step_recorder,
                current_user_id: Some(&user_id),
                current_session_id: Some(&session_id),
                workspace_observation_quarantine: None,
                max_turns: 128,
                recent_tools: &[],
                last_heavy_checkpoint: &mut last_heavy_checkpoint,
                interaction_mode: TurnInteractionMode::Prompt,
            });
            assert!(proceed_advisories(out).is_empty());
        }

        assert!(last_heavy_checkpoint.is_none());
        assert!(
            astra_pipeline::step_checkpoint::list_checkpoints(&user_id, &session_id)
                .expect("checkpoint listing")
                .is_empty(),
            "healthy tool cadence is journal evidence, not 64 durable transcript copies"
        );
    }

    #[test]
    fn map_post_tool_outcome_round_trip_variants() {
        assert_eq!(
            map_post_tool_policy_outcome(AgenticPostToolPolicyOutcome::ProceedEndTurn {
                advisories: vec![PolicyAdvisory {
                    kind: "stall".into(),
                    severity: "warning".into(),
                    evidence: vec!["evidence".into()],
                    recommendation: "recommendation".into(),
                    ttl_rounds: 1,
                    dedupe_key: "stall|warning".into(),
                }],
            }),
            AgenticPostToolIterationControl::ProceedEndTurn {
                advisories: vec![PolicyAdvisory {
                    kind: "stall".into(),
                    severity: "warning".into(),
                    evidence: vec!["evidence".into()],
                    recommendation: "recommendation".into(),
                    ttl_rounds: 1,
                    dedupe_key: "stall|warning".into(),
                }],
            }
        );
    }

    #[test]
    fn policy_advisory_bundle_preserves_structured_short_lived_evidence() {
        let payload = policy_advisory_bundle_value(&[PolicyAdvisory {
            kind: "stall".into(),
            severity: "warning".into(),
            evidence: vec!["same tool call shape repeated".into()],
            recommendation: "consider changing approach".into(),
            ttl_rounds: 1,
            dedupe_key: "stall|warning".into(),
        }])
        .expect("non-empty advisories should produce a payload");

        assert_eq!(payload["schema"], "policy_advisory.v1");
        assert_eq!(payload["authority"], "advisory_evidence_only");
        assert_eq!(payload["advisories"][0]["kind"], "stall");
        assert_eq!(payload["advisories"][0]["ttl_rounds"], 1);
        assert_eq!(
            payload["advisories"][0]["evidence"][0],
            "same tool call shape repeated"
        );
    }

    #[test]
    fn reward_hacking_warning_records_without_retry_or_schema_restriction() {
        let mut messages = Vec::new();
        let mut verdict_events = Vec::new();
        let mut restricted_tools = HashSet::new();
        let mut remaining_turns = 10usize;
        let mut step_recorder =
            StepRecorder::with_persistence_for_run("uid", "sid", "tid", "test-run");
        let mut last_heavy_checkpoint: Option<StepCheckpoint> = None;
        let mut turn_guard = TurnGuard::new();
        let tool_calls = vec![
            json!({"name": "read_file", "arguments": {"path": "src/lib.rs"}}),
            json!({"name": "read_file", "arguments": {"path": "src/lib.rs"}}),
        ];
        turn_guard.record_tool_calls(&tool_calls);
        turn_guard.record_tool_result("read_file", "fn main() {}");
        turn_guard.record_tool_result("read_file", "fn main() {}");
        let out = apply_agentic_post_tool_policy(AgenticPostToolPolicyRequest {
            turn_index: 0,
            messages: &mut messages,
            turn_guard: &mut turn_guard,
            verdict_events: &mut verdict_events,
            restricted_tools: &mut restricted_tools,
            remaining_turns: &mut remaining_turns,
            step_recorder: &mut step_recorder,
            current_user_id: None,
            current_session_id: None,
            workspace_observation_quarantine: None,
            max_turns: 8,
            recent_tools: &[],
            last_heavy_checkpoint: &mut last_heavy_checkpoint,
            interaction_mode: TurnInteractionMode::Prompt,
        });

        let advisories = proceed_advisories(out);
        assert_eq!(advisories.len(), 1);
        assert_eq!(advisories[0].severity, "warning");
        assert_eq!(remaining_turns, 10);
        // Reward-hacking guidance is advisory and must not hide the schema.
        assert!(
            !restricted_tools.contains("read_file"),
            "read-only tools must not be added to restricted_tools"
        );
        assert!(
            messages.is_empty(),
            "behavioral warnings must not inject corrective prompt messages"
        );
        assert_eq!(verdict_events.len(), 1);
        assert_eq!(verdict_events[0].severity, "warning");
    }

    #[test]
    #[serial_test::serial(session_journal_dir)]
    fn warning_checkpoint_preserves_configured_remaining_turns() {
        let sessions_dir = tempfile::tempdir().expect("temporary session journal");
        let _journal_guard =
            astra_services::session_journal::JournalDirGuard::new(sessions_dir.path());
        let mut messages = vec![json!({"role": "user", "content": "inspect the code"})];
        let mut verdict_events = Vec::new();
        let mut restricted_tools = HashSet::new();
        let mut remaining_turns = 10usize;
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before epoch")
            .as_nanos();
        let user_id = format!("uid-{suffix}");
        let session_id = format!("post-tool-policy-{suffix}");
        let mut step_recorder =
            StepRecorder::with_persistence_for_run(&user_id, &session_id, "tid", "test-run");
        step_recorder.begin_turn(0);
        let mut last_heavy_checkpoint: Option<StepCheckpoint> = None;
        let mut turn_guard = TurnGuard::new();
        let tool_calls = vec![
            json!({"name": "read_file", "arguments": {"path": "src/lib.rs"}}),
            json!({"name": "read_file", "arguments": {"path": "src/lib.rs"}}),
        ];
        turn_guard.record_tool_calls(&tool_calls);
        turn_guard.record_tool_result("read_file", "fn main() {}");
        turn_guard.record_tool_result("read_file", "fn main() {}");
        let quarantine =
            astra_pipeline::step_protocol::WorkspaceObservationQuarantineV1::weak_process_ownership(
                Some("warning-call".into()),
            );

        let out = apply_agentic_post_tool_policy(AgenticPostToolPolicyRequest {
            turn_index: 0,
            messages: &mut messages,
            turn_guard: &mut turn_guard,
            verdict_events: &mut verdict_events,
            restricted_tools: &mut restricted_tools,
            remaining_turns: &mut remaining_turns,
            step_recorder: &mut step_recorder,
            current_user_id: Some(&user_id),
            current_session_id: Some(&session_id),
            workspace_observation_quarantine: Some(&quarantine),
            max_turns: 20,
            recent_tools: &["read_file".to_string()],
            last_heavy_checkpoint: &mut last_heavy_checkpoint,
            interaction_mode: TurnInteractionMode::Prompt,
        });

        let advisories = proceed_advisories(out);
        assert_eq!(advisories.len(), 1);
        assert_eq!(advisories[0].severity, "warning");
        assert_eq!(remaining_turns, 10);
        let checkpoint = last_heavy_checkpoint
            .as_ref()
            .expect("warning verdict should write a heavy checkpoint");
        let StepCheckpoint::Heavy(heavy) = checkpoint else {
            panic!("expected heavy checkpoint");
        };
        assert_eq!(
            heavy.budget_remaining_rounds, 10,
            "behavioral warning must not apply hidden budget penalties"
        );
        assert_eq!(
            heavy.workspace_observation_quarantine,
            Some(quarantine),
            "warning checkpoint must not erase sticky workspace quarantine"
        );
    }

    #[test]
    #[serial_test::serial(session_journal_dir)]
    fn cache_waste_info_records_without_retry_or_restriction() {
        let sessions_dir = tempfile::tempdir().expect("temporary session journal");
        let _journal_guard =
            astra_services::session_journal::JournalDirGuard::new(sessions_dir.path());
        let mut messages = Vec::new();
        let mut verdict_events = Vec::new();
        let mut restricted_tools = HashSet::new();
        let mut remaining_turns = 10usize;
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before epoch")
            .as_nanos();
        let user_id = format!("uid-{suffix}");
        let session_id = format!("info-checkpoint-{suffix}");
        let mut step_recorder =
            StepRecorder::with_persistence_for_run(&user_id, &session_id, "tid", "test-run");
        step_recorder.begin_turn(0);
        let mut last_heavy_checkpoint: Option<StepCheckpoint> = None;
        let mut turn_guard = TurnGuard::new();
        turn_guard.record_cache_hit("read_file");
        turn_guard.record_cache_hit("read_file");
        turn_guard.record_cache_hit("read_file");

        let out = apply_agentic_post_tool_policy(AgenticPostToolPolicyRequest {
            turn_index: 0,
            messages: &mut messages,
            turn_guard: &mut turn_guard,
            verdict_events: &mut verdict_events,
            restricted_tools: &mut restricted_tools,
            remaining_turns: &mut remaining_turns,
            step_recorder: &mut step_recorder,
            current_user_id: Some(&user_id),
            current_session_id: Some(&session_id),
            workspace_observation_quarantine: None,
            max_turns: 8,
            recent_tools: &[],
            last_heavy_checkpoint: &mut last_heavy_checkpoint,
            interaction_mode: TurnInteractionMode::Prompt,
        });

        assert!(proceed_advisories(out).is_empty());
        assert!(
            messages.is_empty(),
            "info-level cache guidance must not pollute model messages"
        );
        // read_file is read-only — the filter prevents it from entering restricted_tools.
        assert!(
            !restricted_tools.contains("read_file"),
            "read-only tools must not be added to restricted_tools"
        );
        assert_eq!(remaining_turns, 10);
        assert_eq!(verdict_events.len(), 1);
        assert_eq!(verdict_events[0].severity, "info");
        assert!(
            last_heavy_checkpoint.is_none(),
            "informational observations are not durable recovery boundaries"
        );
        assert!(
            astra_pipeline::step_checkpoint::list_checkpoints(&user_id, &session_id)
                .expect("checkpoint listing")
                .is_empty()
        );
    }

    #[test]
    fn advisory_avoid_tools_do_not_remove_visible_tool_schema() {
        let mut messages = Vec::new();
        let mut verdict_events = Vec::new();
        let mut restricted_tools = HashSet::new();
        let mut remaining_turns = 10usize;
        let mut step_recorder =
            StepRecorder::with_persistence_for_run("uid", "sid", "tid", "test-run");
        let mut last_heavy_checkpoint: Option<StepCheckpoint> = None;
        let mut turn_guard = TurnGuard::new();
        let tool_calls = vec![
            json!({"name": "agent_fanout", "arguments": {"action": "get_results", "group_id": "review"}}),
            json!({"name": "agent_fanout", "arguments": {"action": "get_results", "group_id": "review"}}),
            json!({"name": "agent_fanout", "arguments": {"action": "get_results", "group_id": "review"}}),
        ];
        turn_guard.record_tool_calls(&tool_calls);

        let out = apply_agentic_post_tool_policy(AgenticPostToolPolicyRequest {
            turn_index: 0,
            messages: &mut messages,
            turn_guard: &mut turn_guard,
            verdict_events: &mut verdict_events,
            restricted_tools: &mut restricted_tools,
            remaining_turns: &mut remaining_turns,
            step_recorder: &mut step_recorder,
            current_user_id: None,
            current_session_id: None,
            workspace_observation_quarantine: None,
            max_turns: 8,
            recent_tools: &[],
            last_heavy_checkpoint: &mut last_heavy_checkpoint,
            interaction_mode: TurnInteractionMode::Prompt,
        });

        let advisories = proceed_advisories(out);
        assert_eq!(advisories.len(), 1);
        assert_eq!(advisories[0].kind, "tool_behavior");
        assert_eq!(verdict_events.len(), 1);
        assert!(
            verdict_events[0]
                .avoid_tools
                .contains(&"agent_fanout".to_string())
        );
        assert!(
            !turn_guard.health.is_avoidance_advised("agent_fanout"),
            "stall advice alone must not mark the tool unhealthy"
        );
        assert!(
            !restricted_tools.contains("agent_fanout"),
            "advisory avoid_tools must not remove the tool schema"
        );
        assert!(
            messages.is_empty(),
            "advisory avoid_tools must not be injected back into the prompt"
        );
    }

    #[test]
    fn critical_verdict_does_not_physically_restrict_avoid_tools() {
        // First-Critical verdict remains below the strong-advisory threshold.
        // can still name tools in retry guidance, but it must not remove them
        // from the schema. A failure does not prove the tool is unusable.
        let mut messages = Vec::new();
        let mut verdict_events = Vec::new();
        let mut restricted_tools = HashSet::new();
        let mut remaining_turns = 10usize;
        let mut step_recorder =
            StepRecorder::with_persistence_for_run("uid", "sid", "tid", "test-run");
        let mut last_heavy_checkpoint: Option<StepCheckpoint> = None;
        let mut turn_guard = TurnGuard::new();
        // Drive the guard to a first-Critical verdict using only public API:
        // `record_tool_result` with an error-looking string sets
        // `round_had_error` and records actionable errors internally.
        // With nudge_count = 4, three actionable errors escalate to Critical.
        turn_guard.nudge_count = 4;
        for _ in 0..3 {
            turn_guard.record_tool_result("read_file", "Error: file not found");
        }

        let tool_calls = vec![json!({"name": "bash", "arguments": "{}"})];
        turn_guard.record_tool_calls(&tool_calls);

        let out = apply_agentic_post_tool_policy(AgenticPostToolPolicyRequest {
            turn_index: 0,
            messages: &mut messages,
            turn_guard: &mut turn_guard,
            verdict_events: &mut verdict_events,
            restricted_tools: &mut restricted_tools,
            remaining_turns: &mut remaining_turns,
            step_recorder: &mut step_recorder,
            current_user_id: None,
            current_session_id: None,
            workspace_observation_quarantine: None,
            max_turns: 8,
            recent_tools: &[],
            last_heavy_checkpoint: &mut last_heavy_checkpoint,
            interaction_mode: TurnInteractionMode::Prompt,
        });

        // First Critical remains below the strong-advisory threshold.
        let verdict = verdict_events.first().expect("verdict recorded");
        assert_eq!(verdict.severity, "critical");
        assert!(
            verdict.avoid_tools.contains(&"bash".to_string()),
            "first Critical must name bash in avoid_tools"
        );
        assert!(
            restricted_tools.is_empty(),
            "critical health/stall guidance must not physically restrict avoid_tools"
        );
        let advisories = proceed_advisories(out);
        assert_eq!(advisories.len(), 1);
        assert_eq!(advisories[0].severity, "critical");
        assert_eq!(remaining_turns, 10);
        assert!(
            messages.is_empty(),
            "critical behavioral verdicts are recorded, not injected"
        );
    }

    #[test]
    fn health_deprioritized_tools_remain_same_turn_advisory() {
        let mut messages = Vec::new();
        let mut verdict_events = Vec::new();
        let mut restricted_tools = HashSet::new();
        let mut remaining_turns = 10usize;
        let mut step_recorder =
            StepRecorder::with_persistence_for_run("uid", "sid", "tid", "test-run");
        let mut last_heavy_checkpoint: Option<StepCheckpoint> = None;
        let mut turn_guard = TurnGuard::new();
        for _ in 0..3 {
            turn_guard.health.record_failure("write_file");
        }

        let out = apply_agentic_post_tool_policy(AgenticPostToolPolicyRequest {
            turn_index: 0,
            messages: &mut messages,
            turn_guard: &mut turn_guard,
            verdict_events: &mut verdict_events,
            restricted_tools: &mut restricted_tools,
            remaining_turns: &mut remaining_turns,
            step_recorder: &mut step_recorder,
            current_user_id: None,
            current_session_id: None,
            workspace_observation_quarantine: None,
            max_turns: 8,
            recent_tools: &[],
            last_heavy_checkpoint: &mut last_heavy_checkpoint,
            interaction_mode: TurnInteractionMode::Prompt,
        });

        let advisories = proceed_advisories(out);
        assert_eq!(advisories.len(), 1);
        assert_eq!(advisories[0].kind, "tool_behavior");
        assert!(turn_guard.health.is_avoidance_advised("write_file"));
        assert!(
            !restricted_tools.contains("write_file"),
            "soft health-deprioritized tools must not be hidden"
        );
        assert!(
            messages.is_empty(),
            "soft health-deprioritization must remain out-of-band"
        );
    }

    #[test]
    fn checkpoint_blocked_tools_uses_only_hard_restrictions() {
        let mut restricted_tools = HashSet::new();
        restricted_tools.insert("bash".to_string());
        restricted_tools.insert("write_file".to_string());

        let mut turn_guard = TurnGuard::new();
        for _ in 0..3 {
            turn_guard.health.record_failure("flaky_soft_tool");
        }
        assert!(turn_guard.health.is_avoidance_advised("flaky_soft_tool"));

        let blocked = super::checkpoint_blocked_tools(&restricted_tools);
        assert_eq!(blocked, vec!["bash".to_string(), "write_file".to_string()]);
        assert!(
            !blocked.contains(&"flaky_soft_tool".to_string()),
            "soft tool-health deprioritization must not persist as hard checkpoint blocked_tools"
        );
    }
}
