//! Deterministic coverage and trace contracts for the product harness.
//!
//! The model is intentionally outside this module.  A case describes a
//! user-visible capability, its execution topology, and the machine-readable
//! boundary that must be checked.  The trace contract then validates the
//! evidence produced by that run (hook order, monotonic counters, terminal
//! state, and context bounds).  This keeps the harness useful for real model
//! runs without making it a prompt/response similarity test.

use crate::{HookPoint, SessionTrace, TraceOutcome};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// Product capabilities are grouped by the invariant they share, not by the
/// endpoint that happens to expose them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CapabilityQuadrant {
    Interaction,
    ToolExecution,
    WorkAndTasks,
    PolicyAndApproval,
    Delegation,
    ContextAndCache,
    Memory,
    Observability,
    Recovery,
    MultiTenantAndPerformance,
}

/// The three supported execution shapes must all be exercised by the matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Topology {
    CliServer,
    ServerOnly,
    EdgeServer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CaseKind {
    Happy,
    Unhappy,
}

/// How the product capability participates in model-backed validation.
///
/// A model probe drives a real user journey, but never replaces the typed
/// `system_test` oracle below. Protocol races, tenant isolation, idempotency,
/// and fault injection are deliberately deterministic-only: asking an LLM to
/// judge those boundaries would create confidence without evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelValidation {
    Probe { case: &'static str },
    DeterministicOnly { reason: &'static str },
}

/// A coverage case is a contract anchor, not a prompt fixture. `boundary`
/// names the typed evidence that the adapter must assert; `system_test` maps
/// it to a real focused/system test when one exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityCase {
    pub id: &'static str,
    pub quadrant: CapabilityQuadrant,
    pub topology: Topology,
    pub kind: CaseKind,
    pub boundary: &'static str,
    pub system_test: &'static str,
    pub model_validation: ModelValidation,
}

/// The product-level matrix. Keep this list small enough to run on every
/// change; expensive model-backed probes belong to the referenced focused or
/// nightly system test, not to this inventory assertion.
pub const CAPABILITY_CASES: &[CapabilityCase] = &[
    CapabilityCase {
        id: "interaction.cli_server.turn_lifecycle",
        quadrant: CapabilityQuadrant::Interaction,
        topology: Topology::CliServer,
        kind: CaseKind::Happy,
        boundary: "turn_open -> llm_round -> turn_terminal",
        system_test: "stream_chat_sse_simple_text_response",
        model_validation: ModelValidation::Probe {
            case: "hello_text_contains",
        },
    },
    CapabilityCase {
        id: "interaction.edge_server.bridge_suffix_rejection",
        quadrant: CapabilityQuadrant::Interaction,
        topology: Topology::EdgeServer,
        kind: CaseKind::Unhappy,
        boundary: "bridge_suffix_count == 1 && current_turn_identity_matches",
        system_test: "e2e_matrix_edge_callback_http_boundary_failures",
        model_validation: ModelValidation::DeterministicOnly {
            reason: "edge callback corruption is a protocol fault-injection boundary",
        },
    },
    CapabilityCase {
        id: "interaction.cli_server.long_conversation_local_coherence",
        quadrant: CapabilityQuadrant::Interaction,
        topology: Topology::CliServer,
        kind: CaseKind::Happy,
        boundary: "elliptical follow-up binds to the immediate exchange while explicit session-wide recall remains available",
        system_test: "active_turn_frame_anchors_elliptical_follow_up_to_immediate_exchange",
        model_validation: ModelValidation::Probe {
            case: "multi_turn_local_followup_focus",
        },
    },
    CapabilityCase {
        id: "tools.cli_server.stable_surface",
        quadrant: CapabilityQuadrant::ToolExecution,
        topology: Topology::CliServer,
        kind: CaseKind::Happy,
        boundary: "visible_schema_set is runtime_bound and cache_stable",
        system_test: "tool_surface_decision_signals_and_priority",
        model_validation: ModelValidation::Probe {
            case: "agent_tool_visible_across_matrix",
        },
    },
    CapabilityCase {
        id: "tools.cli_server.read_only_shell_diagnostics",
        quadrant: CapabilityQuadrant::ToolExecution,
        topology: Topology::CliServer,
        kind: CaseKind::Happy,
        boundary: "read_only workspace exposes shell; inferred task semantics never override runtime authority or permission",
        system_test: "builder_runtime_surface_follows_orchestrator_read_only_binding",
        model_validation: ModelValidation::Probe {
            case: "read_only_shell_diagnostics",
        },
    },
    CapabilityCase {
        id: "tools.cli_server.git_worktree_list_contract",
        quadrant: CapabilityQuadrant::ToolExecution,
        topology: Topology::CliServer,
        kind: CaseKind::Happy,
        boundary: "git(action=worktree, sub_action=list) is projected, admitted, and executed as a read-only Edge operation",
        system_test: "git_worktree_public_contract_dispatches_sub_action",
        model_validation: ModelValidation::Probe {
            case: "git_worktree_list_contract",
        },
    },
    CapabilityCase {
        id: "tools.edge_server.duplicate_callback",
        quadrant: CapabilityQuadrant::ToolExecution,
        topology: Topology::EdgeServer,
        kind: CaseKind::Unhappy,
        boundary: "duplicate_request_id is idempotent",
        system_test: "e2e_matrix_duplicate_tool_result_idempotency",
        model_validation: ModelValidation::DeterministicOnly {
            reason: "duplicate callback delivery must be injected and checked by request identity",
        },
    },
    CapabilityCase {
        id: "work.server_only.pause_resume",
        quadrant: CapabilityQuadrant::WorkAndTasks,
        topology: Topology::ServerOnly,
        kind: CaseKind::Happy,
        boundary: "run_state: running -> paused -> running -> terminal",
        system_test: "e2e_matrix_chat_run_pause_resume_http",
        model_validation: ModelValidation::DeterministicOnly {
            reason: "pause and resume are external concurrent control-plane operations",
        },
    },
    CapabilityCase {
        id: "work.cli_server.task_board_projection",
        quadrant: CapabilityQuadrant::WorkAndTasks,
        topology: Topology::CliServer,
        kind: CaseKind::Happy,
        boundary: "typed work_task_board_update reaches the live board before remote reconciliation and keeps server-issued task identity",
        system_test: "durable_work_event_reaches_the_live_board_before_remote_reconciliation",
        model_validation: ModelValidation::Probe {
            case: "work_natural_dynamic_board_journey",
        },
    },
    CapabilityCase {
        id: "work.cli_server.atomic_graph_mutation",
        quadrant: CapabilityQuadrant::WorkAndTasks,
        topology: Topology::CliServer,
        kind: CaseKind::Unhappy,
        boundary: "cancel and add remain distinct typed operations; an unexecuted cancelled revision is never settled or silently rewritten as replacement",
        system_test: "deferred_cancel_and_add_remain_distinct_after_current_item_settlement",
        model_validation: ModelValidation::Probe {
            case: "work_implicit_replacement_journey",
        },
    },
    CapabilityCase {
        id: "work.cli_server.scheduler_failure_boundary",
        quadrant: CapabilityQuadrant::WorkAndTasks,
        topology: Topology::CliServer,
        kind: CaseKind::Unhappy,
        boundary: "scheduler evidence is required before a work plan is presented as complete",
        system_test: "e2e_matrix_stream_canonical_work_scheduler_prevents_decorative_plan",
        model_validation: ModelValidation::Probe {
            case: "work_plan_decomposition_contract",
        },
    },
    CapabilityCase {
        id: "policy.cli_server.approval",
        quadrant: CapabilityQuadrant::PolicyAndApproval,
        topology: Topology::CliServer,
        kind: CaseKind::Happy,
        boundary: "approval_decision is scoped to user/session/run",
        system_test: "approval_wait_is_user_and_namespace_scoped",
        model_validation: ModelValidation::DeterministicOnly {
            reason: "approval scope requires two authenticated principals or runs",
        },
    },
    CapabilityCase {
        id: "policy.edge_server.denial_contract",
        quadrant: CapabilityQuadrant::PolicyAndApproval,
        topology: Topology::EdgeServer,
        kind: CaseKind::Unhappy,
        boundary: "denied tool has status/error_kind/retryable fields",
        system_test: "terminal_tool_results_keep_machine_readable_failure_contract",
        model_validation: ModelValidation::DeterministicOnly {
            reason: "policy denial is a typed enforcement result, not an assistant-text property",
        },
    },
    CapabilityCase {
        id: "delegation.server_only.fanout",
        quadrant: CapabilityQuadrant::Delegation,
        topology: Topology::ServerOnly,
        kind: CaseKind::Happy,
        boundary: "child causal chains remain isolated and bounded",
        system_test: "e2e_matrix_delegate_http_boundaries",
        model_validation: ModelValidation::Probe {
            case: "fanout_completion_truth",
        },
    },
    CapabilityCase {
        id: "delegation.server_only.fanout_surface_admission",
        quadrant: CapabilityQuadrant::Delegation,
        topology: Topology::ServerOnly,
        kind: CaseKind::Happy,
        boundary: "explicit fanout is admitted without a discovery round while ordinary Work hides coordinator-only fanout",
        system_test: "work_tool_surface_separates_coordinator_and_attempt_roles",
        model_validation: ModelValidation::Probe {
            case: "work_semantic_delegation_order",
        },
    },
    CapabilityCase {
        id: "delegation.edge_server.cross_run_approval",
        quadrant: CapabilityQuadrant::Delegation,
        topology: Topology::EdgeServer,
        kind: CaseKind::Unhappy,
        boundary: "approval from another run cannot satisfy this run",
        system_test: "approval_wait_ignores_journal_decision_from_other_run",
        model_validation: ModelValidation::DeterministicOnly {
            reason: "cross-run approval isolation requires controlled concurrent run identities",
        },
    },
    CapabilityCase {
        id: "context.server_only.cache_accounting",
        quadrant: CapabilityQuadrant::ContextAndCache,
        topology: Topology::ServerOnly,
        kind: CaseKind::Happy,
        boundary: "prompt/cache usage buckets accumulate across model rounds without double-counting",
        system_test: "prompt_cache_counters_accumulate_across_turns",
        model_validation: ModelValidation::Probe {
            case: "pipeline_cache_hit_multi_turn",
        },
    },
    CapabilityCase {
        id: "context.server_only.utf8_cursor",
        quadrant: CapabilityQuadrant::ContextAndCache,
        topology: Topology::ServerOnly,
        kind: CaseKind::Unhappy,
        boundary: "artifact cursor is normalized to a UTF-8 scalar boundary",
        system_test: "artifact_windows_normalize_non_boundary_and_preserve_cross_session_lookup",
        model_validation: ModelValidation::DeterministicOnly {
            reason: "non-boundary byte cursors require exact generated offsets and byte assertions",
        },
    },
    CapabilityCase {
        id: "memory.cli_server.post_turn_detach",
        quadrant: CapabilityQuadrant::Memory,
        topology: Topology::CliServer,
        kind: CaseKind::Happy,
        boundary: "terminal response is emitted before optional extraction completes",
        system_test: "scheduled_post_loop_memory_cleanup_is_visible_to_shutdown_drain",
        model_validation: ModelValidation::Probe {
            case: "memory_full_lifecycle",
        },
    },
    CapabilityCase {
        id: "memory.server_only.cross_user_isolation",
        quadrant: CapabilityQuadrant::Memory,
        topology: Topology::ServerOnly,
        kind: CaseKind::Unhappy,
        boundary: "memory reads/writes are scoped by authenticated user",
        system_test: "e2e_matrix_memory_proxy_user_isolation",
        model_validation: ModelValidation::DeterministicOnly {
            reason: "cross-user isolation needs separately authenticated actors",
        },
    },
    CapabilityCase {
        id: "observability.server_only.audit_projection",
        quadrant: CapabilityQuadrant::Observability,
        topology: Topology::ServerOnly,
        kind: CaseKind::Happy,
        boundary: "audit turn overlays response/tool evidence without N+1 queries",
        system_test: "observed_metrics_overlay_root_projection_without_double_counting",
        model_validation: ModelValidation::Probe {
            case: "introspection_reflection_source_boundary",
        },
    },
    CapabilityCase {
        id: "observability.edge_server.typed_failure",
        quadrant: CapabilityQuadrant::Observability,
        topology: Topology::EdgeServer,
        kind: CaseKind::Unhappy,
        boundary: "terminal tool event preserves status/error_kind/retryable",
        system_test: "terminal_tool_results_keep_machine_readable_failure_contract",
        model_validation: ModelValidation::DeterministicOnly {
            reason: "typed terminal failure fields require a controlled provider failure",
        },
    },
    CapabilityCase {
        id: "recovery.cli_server.cancel",
        quadrant: CapabilityQuadrant::Recovery,
        topology: Topology::CliServer,
        kind: CaseKind::Happy,
        boundary: "cancelled run is terminal cancelled, not failed",
        system_test: "cancelled_run_interrupts_thin_client_tool_result_wait",
        model_validation: ModelValidation::DeterministicOnly {
            reason: "cancellation must race an active run from an external controller",
        },
    },
    CapabilityCase {
        id: "recovery.edge_server.malformed_callback",
        quadrant: CapabilityQuadrant::Recovery,
        topology: Topology::EdgeServer,
        kind: CaseKind::Unhappy,
        boundary: "malformed callback is terminal and non-retryable",
        system_test: "e2e_matrix_edge_callback_http_boundary_failures",
        model_validation: ModelValidation::DeterministicOnly {
            reason: "malformed edge payloads are protocol fault injection",
        },
    },
    CapabilityCase {
        id: "tenancy.server_only.cross_user_session",
        quadrant: CapabilityQuadrant::MultiTenantAndPerformance,
        topology: Topology::ServerOnly,
        kind: CaseKind::Happy,
        boundary: "session/event/audit rows are user scoped",
        system_test: "e2e_matrix_saas_session_cross_user_isolation",
        model_validation: ModelValidation::DeterministicOnly {
            reason: "tenant isolation requires separately authenticated principals",
        },
    },
    CapabilityCase {
        id: "tenancy.server_only.same_session_concurrency",
        quadrant: CapabilityQuadrant::MultiTenantAndPerformance,
        topology: Topology::ServerOnly,
        kind: CaseKind::Unhappy,
        boundary: "overlapping same-session runs are explicitly rejected without leaking ownership",
        system_test: "stream_chat_conflicts_when_same_session_already_has_active_run",
        model_validation: ModelValidation::DeterministicOnly {
            reason: "same-session concurrency requires synchronized external requests",
        },
    },
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatrixIssue {
    pub key: String,
    pub detail: String,
}

/// Validate the inventory itself. This runs fast and fails when a new feature
/// is added without both a recovery case and a topology assignment.
pub fn validate_capability_matrix() -> Result<(), Vec<MatrixIssue>> {
    let mut issues = Vec::new();
    let mut ids = HashSet::new();
    let mut quadrants: HashMap<CapabilityQuadrant, (bool, bool)> = HashMap::new();
    let mut topologies = HashSet::new();

    for case in CAPABILITY_CASES {
        if !ids.insert(case.id) {
            issues.push(MatrixIssue {
                key: "duplicate_case_id".into(),
                detail: case.id.into(),
            });
        }
        if case.boundary.trim().is_empty() || case.system_test.trim().is_empty() {
            issues.push(MatrixIssue {
                key: "incomplete_case".into(),
                detail: case.id.into(),
            });
        }
        match case.model_validation {
            ModelValidation::Probe { case: probe } if probe.trim().is_empty() => {
                issues.push(MatrixIssue {
                    key: "empty_model_probe".into(),
                    detail: case.id.into(),
                });
            }
            ModelValidation::DeterministicOnly { reason } if reason.trim().is_empty() => {
                issues.push(MatrixIssue {
                    key: "missing_deterministic_reason".into(),
                    detail: case.id.into(),
                });
            }
            ModelValidation::Probe { .. } | ModelValidation::DeterministicOnly { .. } => {}
        }
        topologies.insert(case.topology);
        let entry = quadrants.entry(case.quadrant).or_default();
        match case.kind {
            CaseKind::Happy => entry.0 = true,
            CaseKind::Unhappy => entry.1 = true,
        }
    }

    for quadrant in [
        CapabilityQuadrant::Interaction,
        CapabilityQuadrant::ToolExecution,
        CapabilityQuadrant::WorkAndTasks,
        CapabilityQuadrant::PolicyAndApproval,
        CapabilityQuadrant::Delegation,
        CapabilityQuadrant::ContextAndCache,
        CapabilityQuadrant::Memory,
        CapabilityQuadrant::Observability,
        CapabilityQuadrant::Recovery,
        CapabilityQuadrant::MultiTenantAndPerformance,
    ] {
        match quadrants.get(&quadrant).copied().unwrap_or_default() {
            (true, true) => {}
            (happy, unhappy) => issues.push(MatrixIssue {
                key: "quadrant_missing_case_kind".into(),
                detail: format!("{quadrant:?}: happy={happy}, unhappy={unhappy}"),
            }),
        }
    }
    for topology in [
        Topology::CliServer,
        Topology::ServerOnly,
        Topology::EdgeServer,
    ] {
        if !topologies.contains(&topology) {
            issues.push(MatrixIssue {
                key: "topology_missing".into(),
                detail: format!("{topology:?}"),
            });
        }
    }

    if issues.is_empty() {
        Ok(())
    } else {
        Err(issues)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceContractViolation {
    pub invariant: &'static str,
    pub detail: String,
}

/// Deterministic invariants that are safe to apply to traces from any model.
/// No assertion examines assistant prose or prompt wording.
pub fn verify_trace_contract(trace: &SessionTrace) -> Vec<TraceContractViolation> {
    let mut violations = Vec::new();
    if trace.dropped_records > 0 {
        violations.push(TraceContractViolation {
            invariant: "trace_complete",
            detail: format!(
                "{} oldest records were evicted; whole-session causality cannot be proved",
                trace.dropped_records
            ),
        });
    }
    if trace.records.is_empty() {
        violations.push(TraceContractViolation {
            invariant: "trace_non_empty",
            detail: "session emitted no harness records".into(),
        });
        return violations;
    }

    let session_id = trace.session_id.as_deref();
    let mut previous: Option<&crate::DecisionRecord> = None;
    let mut open_llm_requests: HashMap<u32, u32> = HashMap::new();
    let mut open_tool_batches: HashMap<u32, u32> = HashMap::new();
    let mut session_start_count = 0u32;
    let mut has_session_end = false;
    let mut last_point = None;
    let complete = trace.dropped_records == 0;
    let terminal = trace.outcome != TraceOutcome::InProgress;
    if terminal && complete && session_id.is_none_or(|id| id.trim().is_empty()) {
        violations.push(TraceContractViolation {
            invariant: "trace_session_identity_present",
            detail: "complete terminal trace has no session identity".into(),
        });
    }
    for (index, record) in trace.records.iter().enumerate() {
        if terminal && complete && record.session_id.trim().is_empty() {
            violations.push(TraceContractViolation {
                invariant: "record_session_identity_present",
                detail: format!("record {index} has no session identity"),
            });
        }
        if terminal && complete && record.snapshot.session_id.trim().is_empty() {
            violations.push(TraceContractViolation {
                invariant: "snapshot_session_identity_present",
                detail: format!("record {index} snapshot has no session identity"),
            });
        }
        if let Some(session_id) = session_id
            && !record.session_id.is_empty()
            && record.session_id != session_id
        {
            violations.push(TraceContractViolation {
                invariant: "session_identity_stable",
                detail: format!(
                    "record session_id {} differs from {session_id}",
                    record.session_id
                ),
            });
        }
        if !record.snapshot.session_id.is_empty() && record.snapshot.session_id != record.session_id
        {
            violations.push(TraceContractViolation {
                invariant: "snapshot_identity_matches_record",
                detail: format!(
                    "record session_id {:?} differs from snapshot session_id {:?}",
                    record.session_id, record.snapshot.session_id
                ),
            });
        }
        if let Some(previous) = previous {
            if record.turn < previous.turn {
                violations.push(TraceContractViolation {
                    invariant: "turn_order_monotonic",
                    detail: format!(
                        "turn regressed from {} to {} at hook {:?}",
                        previous.turn, record.turn, record.point
                    ),
                });
            }
            if record.monotonic_millis_since_session < previous.monotonic_millis_since_session {
                violations.push(TraceContractViolation {
                    invariant: "monotonic_time_ordered",
                    detail: format!(
                        "monotonic time regressed from {} to {} ms",
                        previous.monotonic_millis_since_session,
                        record.monotonic_millis_since_session
                    ),
                });
            }
            for (name, prior, current) in [
                (
                    "tokens_used_session",
                    previous.snapshot.tokens_used_session,
                    record.snapshot.tokens_used_session,
                ),
                (
                    "tool_calls_this_session",
                    u64::from(previous.snapshot.tool_calls_this_session),
                    u64::from(record.snapshot.tool_calls_this_session),
                ),
            ] {
                if current < prior {
                    violations.push(TraceContractViolation {
                        invariant: "counters_monotonic",
                        detail: format!("{name} regressed from {prior} to {current}"),
                    });
                }
            }
            // Delegation count is per turn and may reset at the next turn;
            // only compare it within the same turn.
            if previous.turn == record.turn
                && record.snapshot.delegations_this_turn < previous.snapshot.delegations_this_turn
            {
                violations.push(TraceContractViolation {
                    invariant: "counters_monotonic",
                    detail: format!(
                        "delegations_this_turn regressed from {} to {}",
                        previous.snapshot.delegations_this_turn,
                        record.snapshot.delegations_this_turn
                    ),
                });
            }
        }
        if let (Some(total), Some(budget)) = (
            record.snapshot.context_total_tokens,
            record.snapshot.context_budget_tokens,
        ) && total > budget
        {
            violations.push(TraceContractViolation {
                invariant: "context_within_budget",
                detail: format!("context total {total} exceeds budget {budget}"),
            });
        }
        match record.point {
            HookPoint::SessionStart => {
                session_start_count += 1;
                if index != 0 {
                    violations.push(TraceContractViolation {
                        invariant: "session_start_is_initial",
                        detail: format!("SessionStart appeared at record index {index}"),
                    });
                }
            }
            HookPoint::PreLlmRequest => {
                *open_llm_requests.entry(record.turn).or_default() += 1;
            }
            HookPoint::PostLlmResponse => {
                let open = open_llm_requests.entry(record.turn).or_default();
                if *open == 0 {
                    if complete {
                        violations.push(TraceContractViolation {
                            invariant: "llm_response_has_request",
                            detail: format!(
                                "turn {} has PostLlmResponse without an earlier unmatched PreLlmRequest",
                                record.turn
                            ),
                        });
                    }
                } else {
                    *open -= 1;
                }
            }
            HookPoint::PreToolBatch => {
                *open_tool_batches.entry(record.turn).or_default() += 1;
            }
            HookPoint::PostToolBatch => {
                let open = open_tool_batches.entry(record.turn).or_default();
                if *open == 0 {
                    if complete {
                        violations.push(TraceContractViolation {
                            invariant: "tool_batch_has_admission",
                            detail: format!(
                                "turn {} has PostToolBatch without an earlier unmatched PreToolBatch",
                                record.turn
                            ),
                        });
                    }
                } else {
                    *open -= 1;
                }
            }
            HookPoint::SessionEnd => {
                has_session_end = true;
            }
            HookPoint::PostTurn => {}
        }
        if let Some(previous_point) = last_point
            && previous_point == HookPoint::SessionEnd
        {
            violations.push(TraceContractViolation {
                invariant: "session_end_is_terminal",
                detail: format!("hook {:?} appeared after SessionEnd", record.point),
            });
        }
        last_point = Some(record.point);
        previous = Some(record);
    }
    if complete && session_start_count == 0 {
        violations.push(TraceContractViolation {
            invariant: "trace_has_session_start",
            detail: "complete trace has no SessionStart hook".into(),
        });
    }
    if session_start_count > 1 {
        violations.push(TraceContractViolation {
            invariant: "session_start_unique",
            detail: format!("trace has {session_start_count} SessionStart hooks"),
        });
    }

    if complete {
        let expected_total_turns = trace
            .records
            .iter()
            .map(|record| record.turn.saturating_add(1))
            .max()
            .unwrap_or(0);
        if trace.total_turns != expected_total_turns {
            violations.push(TraceContractViolation {
                invariant: "turn_count_matches_trace",
                detail: format!(
                    "total_turns={} but retained records imply {expected_total_turns}",
                    trace.total_turns
                ),
            });
        }
    }

    if trace.outcome != TraceOutcome::InProgress && !has_session_end {
        violations.push(TraceContractViolation {
            invariant: "terminal_trace_has_session_end",
            detail: format!("{:?} trace has no SessionEnd hook", trace.outcome),
        });
    }
    if trace.outcome != TraceOutcome::InProgress && trace.ended_at_unix_millis.is_none() {
        violations.push(TraceContractViolation {
            invariant: "terminal_trace_has_end_time",
            detail: format!("{:?} trace has no ended_at timestamp", trace.outcome),
        });
    }
    if trace.outcome == TraceOutcome::InProgress && has_session_end {
        violations.push(TraceContractViolation {
            invariant: "session_end_has_terminal_outcome",
            detail: "InProgress trace already contains SessionEnd".into(),
        });
    }

    if trace.outcome == TraceOutcome::Completed {
        for (turn, open) in open_llm_requests.iter().filter(|(_, open)| **open > 0) {
            violations.push(TraceContractViolation {
                invariant: "llm_request_has_response",
                detail: format!("completed trace left {open} LLM request(s) open in turn {turn}"),
            });
        }
        for (turn, open) in open_tool_batches.iter().filter(|(_, open)| **open > 0) {
            violations.push(TraceContractViolation {
                invariant: "tool_admission_has_result",
                detail: format!("completed trace left {open} tool batch(es) open in turn {turn}"),
            });
        }
    }

    if let Some(start) = trace
        .records
        .iter()
        .find(|record| record.point == HookPoint::SessionStart)
        && trace.started_at_unix_millis != start.wall_time_unix_millis
    {
        violations.push(TraceContractViolation {
            invariant: "start_metadata_matches_record",
            detail: format!(
                "started_at={} but SessionStart record has {}",
                trace.started_at_unix_millis, start.wall_time_unix_millis
            ),
        });
    }
    if let Some(end) = trace
        .records
        .iter()
        .find(|record| record.point == HookPoint::SessionEnd)
        && trace.ended_at_unix_millis != Some(end.wall_time_unix_millis)
    {
        violations.push(TraceContractViolation {
            invariant: "end_metadata_matches_record",
            detail: format!(
                "ended_at={:?} but SessionEnd record has {}",
                trace.ended_at_unix_millis, end.wall_time_unix_millis
            ),
        });
    }

    if trace.outcome == TraceOutcome::Completed {
        let terminal = trace.records.back().map(|record| &record.snapshot);
        if terminal.and_then(|snapshot| snapshot.final_state.as_deref()) != Some("completed") {
            violations.push(TraceContractViolation {
                invariant: "completed_state_is_explicit",
                detail: "completed trace lacks final_state=completed".into(),
            });
        }
        if terminal.is_some_and(|snapshot| !snapshot.has_final_text) {
            violations.push(TraceContractViolation {
                invariant: "completed_state_has_final_text",
                detail: "completed trace has no final text evidence".into(),
            });
        }
    }
    if trace.outcome == TraceOutcome::Interrupted {
        let terminal = trace.records.back().map(|record| &record.snapshot);
        if terminal.and_then(|snapshot| snapshot.final_state.as_deref()) != Some("interrupted") {
            violations.push(TraceContractViolation {
                invariant: "interrupted_state_is_explicit",
                detail: "interrupted trace lacks final_state=interrupted".into(),
            });
        }
        if terminal
            .and_then(|snapshot| snapshot.interruption_kind.as_deref())
            .is_none()
        {
            violations.push(TraceContractViolation {
                invariant: "interrupted_state_has_kind",
                detail: "interrupted trace lacks interruption_kind".into(),
            });
        }
    }
    violations
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RuntimeSnapshot;

    fn record(
        session: &str,
        turn: u32,
        point: HookPoint,
        snapshot: RuntimeSnapshot,
    ) -> crate::DecisionRecord {
        crate::DecisionRecord {
            session_id: session.into(),
            turn,
            point,
            wall_time_unix_millis: 1_000 + u64::from(turn),
            monotonic_millis_since_session: u64::from(turn),
            snapshot,
        }
    }

    #[test]
    fn capability_matrix_is_complete_across_quadrants_and_topologies() {
        validate_capability_matrix().expect("every quadrant needs happy and unhappy coverage");
        assert!(CAPABILITY_CASES.len() >= 20);
    }

    #[test]
    fn trace_contract_accepts_valid_terminal_tool_turn() {
        let mut trace = SessionTrace::new(Some("s1".into()));
        trace.outcome = TraceOutcome::Completed;
        trace.started_at_unix_millis = 1_001;
        trace.ended_at_unix_millis = Some(1_001);
        trace.total_turns = 2;
        trace.records.push_back(record(
            "s1",
            1,
            HookPoint::SessionStart,
            RuntimeSnapshot {
                session_id: "s1".into(),
                context_total_tokens: Some(10),
                context_budget_tokens: Some(100),
                ..RuntimeSnapshot::empty()
            },
        ));
        trace.records.push_back(record(
            "s1",
            1,
            HookPoint::PreToolBatch,
            RuntimeSnapshot {
                session_id: "s1".into(),
                tool_calls_this_session: 1,
                ..RuntimeSnapshot::empty()
            },
        ));
        trace.records.push_back(record(
            "s1",
            1,
            HookPoint::PostToolBatch,
            RuntimeSnapshot {
                session_id: "s1".into(),
                tool_calls_this_session: 1,
                ..RuntimeSnapshot::empty()
            },
        ));
        trace.records.push_back(record(
            "s1",
            1,
            HookPoint::SessionEnd,
            RuntimeSnapshot {
                session_id: "s1".into(),
                final_state: Some("completed".into()),
                has_final_text: true,
                tool_calls_this_session: 1,
                ..RuntimeSnapshot::empty()
            },
        ));
        assert!(verify_trace_contract(&trace).is_empty());
    }

    #[test]
    fn trace_contract_reports_unhappy_invariants_without_text_matching() {
        let mut trace = SessionTrace::new(Some("s1".into()));
        trace.outcome = TraceOutcome::Completed;
        trace.ended_at_unix_millis = Some(1_002);
        trace.total_turns = 3;
        trace.records.push_back(record(
            "s1",
            1,
            HookPoint::PostToolBatch,
            RuntimeSnapshot {
                session_id: "s1".into(),
                tokens_used_session: 10,
                context_total_tokens: Some(101),
                context_budget_tokens: Some(100),
                ..RuntimeSnapshot::empty()
            },
        ));
        trace.records.push_back(record(
            "other",
            2,
            HookPoint::SessionEnd,
            RuntimeSnapshot {
                session_id: "other".into(),
                tokens_used_session: 2,
                ..RuntimeSnapshot::empty()
            },
        ));
        let invariants: HashSet<_> = verify_trace_contract(&trace)
            .into_iter()
            .map(|violation| violation.invariant)
            .collect();
        assert!(invariants.contains("session_identity_stable"));
        assert!(invariants.contains("counters_monotonic"));
        assert!(invariants.contains("context_within_budget"));
        assert!(invariants.contains("tool_batch_has_admission"));
        assert!(invariants.contains("completed_state_is_explicit"));
        assert!(invariants.contains("completed_state_has_final_text"));
    }

    #[test]
    fn terminal_trace_requires_session_end_hook() {
        let mut trace = SessionTrace::new(Some("s1".into()));
        trace.outcome = TraceOutcome::Error;
        trace.records.push_back(record(
            "s1",
            1,
            HookPoint::PostTurn,
            RuntimeSnapshot::empty(),
        ));
        assert!(
            verify_trace_contract(&trace)
                .iter()
                .any(|violation| violation.invariant == "terminal_trace_has_session_end")
        );
    }

    #[test]
    fn trace_contract_rejects_events_after_session_end() {
        let mut trace = SessionTrace::new(Some("s1".into()));
        trace.records.push_back(record(
            "s1",
            1,
            HookPoint::SessionEnd,
            RuntimeSnapshot::empty(),
        ));
        trace.records.push_back(record(
            "s1",
            1,
            HookPoint::PostTurn,
            RuntimeSnapshot::empty(),
        ));
        assert!(
            verify_trace_contract(&trace)
                .iter()
                .any(|violation| violation.invariant == "session_end_is_terminal")
        );
    }

    #[test]
    fn trace_contract_checks_causal_order_not_event_set_membership() {
        let mut trace = SessionTrace::new(Some("s1".into()));
        trace.total_turns = 2;
        trace.records.push_back(record(
            "s1",
            1,
            HookPoint::PostToolBatch,
            RuntimeSnapshot {
                session_id: "s1".into(),
                ..RuntimeSnapshot::empty()
            },
        ));
        trace.records.push_back(record(
            "s1",
            1,
            HookPoint::PreToolBatch,
            RuntimeSnapshot {
                session_id: "s1".into(),
                ..RuntimeSnapshot::empty()
            },
        ));

        assert!(verify_trace_contract(&trace).iter().any(|violation| {
            violation.invariant == "tool_batch_has_admission"
                && violation.detail.contains("earlier unmatched")
        }));
    }

    #[test]
    fn trace_contract_rejects_turn_and_monotonic_time_regressions() {
        let mut trace = SessionTrace::new(Some("s1".into()));
        trace.total_turns = 3;
        let mut first = record("s1", 2, HookPoint::PostTurn, RuntimeSnapshot::empty());
        first.monotonic_millis_since_session = 20;
        let mut second = record("s1", 1, HookPoint::PostTurn, RuntimeSnapshot::empty());
        second.monotonic_millis_since_session = 10;
        trace.records.push_back(first);
        trace.records.push_back(second);

        let invariants: HashSet<_> = verify_trace_contract(&trace)
            .into_iter()
            .map(|violation| violation.invariant)
            .collect();
        assert!(invariants.contains("turn_order_monotonic"));
        assert!(invariants.contains("monotonic_time_ordered"));
    }

    #[test]
    fn trace_contract_marks_evicted_history_as_incomplete() {
        let mut trace = SessionTrace::new(Some("s1".into()));
        trace.dropped_records = 4;
        trace.records.push_back(record(
            "s1",
            7,
            HookPoint::PostTurn,
            RuntimeSnapshot::empty(),
        ));

        assert!(
            verify_trace_contract(&trace)
                .iter()
                .any(|violation| violation.invariant == "trace_complete")
        );
    }

    #[test]
    fn completed_trace_rejects_unclosed_request_boundaries() {
        let mut trace = SessionTrace::new(Some("s1".into()));
        trace.outcome = TraceOutcome::Completed;
        trace.started_at_unix_millis = 1_000;
        trace.ended_at_unix_millis = Some(1_001);
        trace.total_turns = 2;
        trace.records.push_back(record(
            "s1",
            0,
            HookPoint::SessionStart,
            RuntimeSnapshot {
                session_id: "s1".into(),
                ..RuntimeSnapshot::empty()
            },
        ));
        trace.records.push_back(record(
            "s1",
            1,
            HookPoint::PreToolBatch,
            RuntimeSnapshot {
                session_id: "s1".into(),
                ..RuntimeSnapshot::empty()
            },
        ));
        trace.records.push_back(record(
            "s1",
            1,
            HookPoint::SessionEnd,
            RuntimeSnapshot {
                session_id: "s1".into(),
                final_state: Some("completed".into()),
                has_final_text: true,
                ..RuntimeSnapshot::empty()
            },
        ));

        assert!(
            verify_trace_contract(&trace)
                .iter()
                .any(|violation| violation.invariant == "tool_admission_has_result")
        );
    }
}
