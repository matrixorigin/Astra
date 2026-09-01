use astra_services::work::{
    NewWorkItem, NewWorkPlanProposal, RecordedWorkPlanProposal, WorkBranchRevision,
    WorkChangeReason, WorkChangeRef, WorkDomainError, WorkItemDeclarationState, WorkItemEdge,
    WorkItemEdgeKind, WorkItemId, WorkItemKind, WorkItemRevision, WorkItemRevisionChange,
    WorkItemText, WorkObservationQuery, WorkObservationReport, WorkPlanProposalAcceptance,
    WorkPlanProposalViolation, WorkProposalBasisResource, WorkProposalId, WorkProposalSourceKind,
    WorkProposalStatus, WorkRepository, WorkRepositoryError,
};
use astra_tools::tool_engine::{ToolInvocationAdmissionSource, ToolInvocationMetadata};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::runtime_tool_executor::{RuntimeToolExecutor, WorkRuntimeBinding};
use super::tool_work_proposal::{RuntimeWorkProposalKind, invocation_identity};

const INSPECT_WORK_PLAN_ITEM_PAGE_SIZE: usize = 8;
const INSPECT_WORK_PLAN_DEPENDENCY_PAGE_SIZE: usize = 128;
const INSPECT_WORK_PLAN_MAX_OUTPUT_BYTES: usize = 256 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InspectWorkPlanArgs {
    context_id: Option<String>,
    item_offset: Option<usize>,
    dependency_offset: Option<usize>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProposeWorkPlanArgs {
    context_id: String,
    reason: String,
    additions: Vec<ProposedWorkItem>,
    revisions: Vec<ProposedWorkItemRevision>,
    dependencies: Vec<ProposedDependency>,
    dependency_removals: Vec<ProposedDependency>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProposedWorkItem {
    item_id: String,
    kind: ProposedWorkItemKind,
    objective: String,
    expected_result: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProposedWorkItemRevision {
    item_id: String,
    expected_revision: i64,
    kind: ProposedWorkItemKind,
    objective: String,
    expected_result: String,
    declaration_state: ProposedWorkItemDeclarationState,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum ProposedWorkItemDeclarationState {
    Active,
    Superseded,
    Cancelled,
}

impl ProposedWorkItemDeclarationState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Superseded => "superseded",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum ProposedWorkItemKind {
    Milestone,
    Task,
}

impl ProposedWorkItemKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Milestone => "milestone",
            Self::Task => "task",
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProposedDependency {
    predecessor_item_id: String,
    successor_item_id: String,
}

fn cancelled() -> astra_tools::ToolResult {
    work_plan_error(
        "work_plan_cancelled",
        "Work planning was not executed because the run was cancelled",
        true,
    )
}

fn work_plan_error(code: &str, message: &str, retryable: bool) -> astra_tools::ToolResult {
    let output = json!({
        "status": "error",
        "error": {
            "code": code,
            "message": message,
            "retryable": retryable,
        }
    })
    .to_string();
    let mut result = astra_tools::ToolResult::error(output);
    result.metadata = Some(serde_json::Map::from_iter([
        ("error_kind".to_string(), Value::String(code.to_string())),
        ("retryable".to_string(), Value::Bool(retryable)),
    ]));
    result
}

fn map_repository_error(error: WorkRepositoryError) -> astra_tools::ToolResult {
    if matches!(
        &error,
        WorkRepositoryError::Persistence { .. }
            | WorkRepositoryError::Corrupt { .. }
            | WorkRepositoryError::ManifestEncoding { .. }
    ) {
        tracing::warn!(error = %error, "canonical Work planning repository degraded");
    }
    match error {
        WorkRepositoryError::InvalidMutation { source } => {
            let message = match source {
                WorkDomainError::InvalidPlanProposal {
                    violation: WorkPlanProposalViolation::ConflictingItemChange,
                } => "Invalid Work plan proposal: an addition must use a fresh item_id. If the same semantic work continues, submit only an active successor revision. If work is replaced, retire the old item with a cancelled or superseded revision and add the replacement under a fresh item_id."
                    .to_string(),
                WorkDomainError::InvalidPlanProposal { violation } => {
                    format!("Invalid Work plan proposal: {violation}")
                }
                other => format!("Invalid Work graph mutation: {other}"),
            };
            work_plan_error("work_plan_proposal_invalid", &message, false)
        }
        WorkRepositoryError::InvalidWorkProposalBasis {
            resource:
                WorkProposalBasisResource::DependencyEndpoint
                | WorkProposalBasisResource::NewItemIdentity,
        } => work_plan_error(
            "work_plan_proposal_invalid",
            "A proposed new item_id already exists or a dependency endpoint is not present in the resulting graph. Revise an existing item by revision; use a fresh item_id for a replacement addition.",
            false,
        ),
        WorkRepositoryError::InvalidWorkProposalBasis {
            resource: WorkProposalBasisResource::BranchIdentity,
        } => work_plan_error(
            "work_plan_binding_changed",
            "The session is no longer bound to the validated Work branch",
            false,
        ),
        WorkRepositoryError::InvalidWorkProposalBasis { .. } => work_plan_error(
            "work_plan_context_stale",
            "The Work plan changed; inspect the current context before proposing again",
            true,
        ),
        WorkRepositoryError::WorkProposalCapacityExceeded => work_plan_error(
            "work_plan_proposal_capacity",
            "This Work branch has reached its bounded pending-proposal capacity",
            true,
        ),
        WorkRepositoryError::WorkProposalAlreadyResolved { .. } => work_plan_error(
            "work_plan_proposal_resolved",
            "This plan proposal has already reached a terminal state",
            false,
        ),
        WorkRepositoryError::NotFound | WorkRepositoryError::Archived => work_plan_error(
            "work_plan_binding_not_found",
            "The bound canonical Work branch is no longer available",
            false,
        ),
        WorkRepositoryError::Conflict { .. } => work_plan_error(
            "work_plan_conflict",
            "The plan proposal conflicts with an existing canonical identity",
            false,
        ),
        WorkRepositoryError::Persistence { .. }
        | WorkRepositoryError::Corrupt { .. }
        | WorkRepositoryError::ManifestEncoding { .. } => work_plan_error(
            "work_planning_unavailable",
            "Canonical Work planning is temporarily unavailable",
            true,
        ),
        _ => work_plan_error(
            "work_plan_rejected",
            "The canonical Work repository rejected this plan operation",
            false,
        ),
    }
}

fn validate_context(
    binding: &WorkRuntimeBinding,
    context: &astra_services::work::WorkPlanContext,
) -> Result<(), astra_tools::ToolResult> {
    if context.basis().work_id != binding.work_id || context.basis().branch_id != binding.branch_id
    {
        return Err(work_plan_error(
            "work_plan_binding_changed",
            "The session is no longer bound to the validated Work branch",
            false,
        ));
    }
    Ok(())
}

async fn load_context(
    binding: &WorkRuntimeBinding,
) -> Result<astra_services::work::WorkPlanContext, astra_tools::ToolResult> {
    let context = binding
        .repository
        .load_plan_context_for_session(&binding.owner_id, &binding.session_id)
        .await
        .map_err(map_repository_error)?;
    validate_context(binding, &context)?;
    Ok(context)
}

async fn load_observation(
    binding: &WorkRuntimeBinding,
    context: &astra_services::work::WorkPlanContext,
) -> Result<WorkObservationReport, astra_tools::ToolResult> {
    let report = binding
        .repository
        .observe_declared_work(WorkObservationQuery {
            owner_id: binding.owner_id.clone(),
            work_id: binding.work_id.clone(),
        })
        .await
        .map_err(map_repository_error)?;
    let basis = context.basis();
    let as_of = report.as_of();
    if report.overview().work_id != basis.work_id
        || as_of.work_revision != basis.work_revision
        || as_of.goal_revision != basis.goal_revision
        || as_of.criteria_set_revision != basis.criteria_set_revision
    {
        return Err(work_plan_error(
            "work_plan_context_stale",
            "The Work observation changed while its plan was being inspected",
            true,
        ));
    }
    if report.overview().delivery_branch.branch_id == basis.branch_id
        && (as_of.delivery_branch_revision != basis.branch_revision
            || as_of.graph_revision != basis.graph_revision)
    {
        return Err(work_plan_error(
            "work_plan_context_stale",
            "The delivery branch changed while its plan was being inspected",
            true,
        ));
    }
    Ok(report)
}

pub(super) async fn inspect(
    executor: &RuntimeToolExecutor,
    args: &Value,
    cancel_token: Option<&CancellationToken>,
) -> astra_tools::ToolResult {
    if cancel_token.is_some_and(CancellationToken::is_cancelled) {
        return cancelled();
    }
    let args: InspectWorkPlanArgs = match serde_json::from_value(args.clone()) {
        Ok(args) => args,
        Err(_) => {
            return work_plan_error(
                "work_plan_arguments_invalid",
                "inspect_work_plan requires the exact typed pagination contract",
                false,
            );
        }
    };
    if args
        .context_id
        .as_ref()
        .is_some_and(|context_id| context_id.is_empty() || context_id.len() > 96)
    {
        return work_plan_error(
            "work_plan_arguments_invalid",
            "inspect_work_plan context identity is invalid",
            false,
        );
    }
    let Some(binding) = executor.work_binding.get() else {
        return work_plan_error(
            "work_plan_binding_required",
            "This session has no validated canonical Work binding",
            false,
        );
    };
    let context = match load_context(binding).await {
        Ok(context) => context,
        Err(error) => return error,
    };
    let observation = match load_observation(binding, &context).await {
        Ok(observation) => observation,
        Err(error) => return error,
    };
    if args
        .context_id
        .as_deref()
        .is_some_and(|expected| expected != context.context_id())
    {
        return work_plan_error(
            "work_plan_context_stale",
            "The Work plan changed; restart inspection from the current context",
            true,
        );
    }
    let item_offset = args.item_offset.unwrap_or(0);
    let dependency_offset = args.dependency_offset.unwrap_or(0);
    if item_offset > context.items().len() || dependency_offset > context.dependencies().len() {
        return work_plan_error(
            "work_plan_arguments_invalid",
            "inspect_work_plan pagination offset exceeds the current context",
            false,
        );
    }
    let item_end = item_offset
        .saturating_add(INSPECT_WORK_PLAN_ITEM_PAGE_SIZE)
        .min(context.items().len());
    let dependency_end = dependency_offset
        .saturating_add(INSPECT_WORK_PLAN_DEPENDENCY_PAGE_SIZE)
        .min(context.dependencies().len());
    let output = match serde_json::to_string(&json!({
        "schema_version": 1,
        "context_id": context.context_id(),
        "content_hash": context.content_hash(),
        "observation": {
            "report_id": observation.report_id(),
            "content_hash": observation.content_hash(),
            "scope": observation.scope(),
            "as_of": observation.as_of(),
            "coherence": observation.coherence(),
            "coverage_gaps": observation.coverage_gaps(),
            "finding": observation.finding(),
            "satisfaction_evidence_refs": observation.satisfaction_evidence_refs(),
        },
        "basis": context.basis(),
        "execution_contract": {
            "executable_kind": "task",
            "structural_kind": "milestone",
            "milestones_own_attempts": false,
            "milestones_require_settlement": false,
        },
        "items": {
            "offset": item_offset,
            "entries": &context.items()[item_offset..item_end],
            "next_offset": (item_end < context.items().len()).then_some(item_end),
        },
        "dependencies": {
            "offset": dependency_offset,
            "entries": &context.dependencies()[dependency_offset..dependency_end],
            "next_offset": (dependency_end < context.dependencies().len())
                .then_some(dependency_end),
        },
    })) {
        Ok(output) => output,
        Err(_) => {
            return work_plan_error(
                "work_planning_unavailable",
                "The canonical Work context could not be encoded",
                true,
            );
        }
    };
    if output.len() > INSPECT_WORK_PLAN_MAX_OUTPUT_BYTES {
        tracing::warn!(
            output_bytes = output.len(),
            context_id = context.context_id(),
            "bounded Work planning projection exceeded its invariant"
        );
        return work_plan_error(
            "work_planning_unavailable",
            "The bounded canonical Work context could not be projected",
            true,
        );
    }
    astra_tools::ToolResult::text(output)
}

fn pending_result(
    proposal: &RecordedWorkPlanProposal,
    reason: &'static str,
) -> astra_tools::ToolResult {
    astra_tools::ToolResult::text(
        json!({
            "status": "pending",
            "proposal_id": proposal.proposal.proposal_id.as_str(),
            "payload_hash": proposal.payload_hash.as_str(),
            "admission": {
                "mode": "needs_review",
                "reason": reason,
            },
        })
        .to_string(),
    )
}

fn admission_resolution_ref(
    proposal: &RecordedWorkPlanProposal,
    source: ToolInvocationAdmissionSource,
) -> WorkChangeRef {
    let source = match source {
        ToolInvocationAdmissionSource::Policy => "policy",
        ToolInvocationAdmissionSource::ImplicitPolicy => "implicit-policy",
        ToolInvocationAdmissionSource::ParentApproval => "parent-approval",
    };
    let digest = proposal
        .payload_hash
        .as_str()
        .strip_prefix("sha256:")
        .expect("Work content hashes have a validated sha256 prefix");
    WorkChangeRef::parse(format!("work-plan-{source}-v1-{digest}"))
        .expect("a versioned admission decision is a valid change reference")
}

fn acceptance(
    proposal: &RecordedWorkPlanProposal,
    resolution_ref: WorkChangeRef,
) -> WorkPlanProposalAcceptance {
    WorkPlanProposalAcceptance {
        owner_id: proposal.proposal.owner_id.clone(),
        work_id: proposal.proposal.work_id.clone(),
        branch_id: proposal.proposal.branch_id.clone(),
        proposal_id: proposal.proposal.proposal_id.clone(),
        payload_hash: proposal.payload_hash.clone(),
        expected_work_revision: proposal.proposal.expected_work_revision,
        expected_goal_revision: proposal.proposal.expected_goal_revision,
        expected_criteria_set_revision: proposal.proposal.expected_criteria_set_revision,
        expected_branch_revision: proposal.proposal.expected_branch_revision,
        expected_graph_revision: proposal.proposal.expected_graph_revision,
        resolution_ref,
    }
}

async fn admit_or_hold(
    binding: &WorkRuntimeBinding,
    proposal: RecordedWorkPlanProposal,
    admission_source: Option<ToolInvocationAdmissionSource>,
) -> astra_tools::ToolResult {
    let Some(admission_source) = admission_source else {
        return pending_result(&proposal, "invocation_not_policy_admitted");
    };
    let resolution_ref = admission_resolution_ref(&proposal, admission_source);
    binding
        .repository
        .accept_plan_proposal(acceptance(&proposal, resolution_ref))
        .await
        .map_or_else(map_repository_error, |accepted| accepted_result(&accepted))
}

fn accepted_result(proposal: &RecordedWorkPlanProposal) -> astra_tools::ToolResult {
    let resolution = proposal
        .resolution
        .as_ref()
        .expect("an accepted proposal has a resolution");
    astra_tools::ToolResult::text(
        json!({
            "status": "accepted",
            "proposal_id": proposal.proposal.proposal_id.as_str(),
            "payload_hash": proposal.payload_hash.as_str(),
            "result_branch_revision": resolution.result_branch_revision.map(WorkBranchRevision::get),
            "result_graph_revision": resolution.result_graph_revision.map(|revision| revision.get()),
        })
        .to_string(),
    )
}

fn verify_retry_identity(
    binding: &WorkRuntimeBinding,
    existing: &RecordedWorkPlanProposal,
    source_ref: &WorkChangeRef,
) -> Result<(), astra_tools::ToolResult> {
    if existing.proposal.owner_id != binding.owner_id
        || existing.proposal.work_id != binding.work_id
        || existing.proposal.branch_id != binding.branch_id
        || existing.proposal.source_kind != WorkProposalSourceKind::Model
        || &existing.proposal.source_ref != source_ref
    {
        return Err(work_plan_error(
            "work_plan_invocation_conflict",
            "The trusted invocation identity conflicts with an existing proposal",
            false,
        ));
    }
    Ok(())
}

fn parse_additions(
    additions: Vec<ProposedWorkItem>,
) -> Result<Vec<NewWorkItem>, astra_tools::ToolResult> {
    additions
        .into_iter()
        .map(|item| {
            Ok(NewWorkItem {
                item_id: WorkItemId::parse(item.item_id).map_err(|_| {
                    work_plan_error(
                        "work_plan_arguments_invalid",
                        "A proposed Work item identity is invalid",
                        false,
                    )
                })?,
                kind: match item.kind {
                    ProposedWorkItemKind::Milestone => WorkItemKind::Milestone,
                    ProposedWorkItemKind::Task => WorkItemKind::Task,
                },
                objective: WorkItemText::parse(item.objective).map_err(|_| {
                    work_plan_error(
                        "work_plan_arguments_invalid",
                        "A proposed Work item objective is invalid",
                        false,
                    )
                })?,
                expected_result: WorkItemText::parse(item.expected_result).map_err(|_| {
                    work_plan_error(
                        "work_plan_arguments_invalid",
                        "A proposed Work item expected result is invalid",
                        false,
                    )
                })?,
            })
        })
        .collect()
}

fn parse_dependencies(
    dependencies: Vec<ProposedDependency>,
) -> Result<Vec<WorkItemEdge>, astra_tools::ToolResult> {
    dependencies
        .into_iter()
        .map(|edge| {
            Ok(WorkItemEdge {
                predecessor_item_id: WorkItemId::parse(edge.predecessor_item_id).map_err(|_| {
                    work_plan_error(
                        "work_plan_arguments_invalid",
                        "A dependency predecessor identity is invalid",
                        false,
                    )
                })?,
                successor_item_id: WorkItemId::parse(edge.successor_item_id).map_err(|_| {
                    work_plan_error(
                        "work_plan_arguments_invalid",
                        "A dependency successor identity is invalid",
                        false,
                    )
                })?,
                kind: WorkItemEdgeKind::Dependency,
            })
        })
        .collect()
}

fn parse_revisions(
    revisions: Vec<ProposedWorkItemRevision>,
) -> Result<Vec<WorkItemRevisionChange>, astra_tools::ToolResult> {
    revisions
        .into_iter()
        .map(|item| {
            Ok(WorkItemRevisionChange::new(
                WorkItemId::parse(item.item_id).map_err(|_| {
                    work_plan_error(
                        "work_plan_arguments_invalid",
                        "A revised Work item identity is invalid",
                        false,
                    )
                })?,
                WorkItemRevision::new(item.expected_revision).map_err(|_| {
                    work_plan_error(
                        "work_plan_arguments_invalid",
                        "A revised Work item basis revision is invalid",
                        false,
                    )
                })?,
                match item.kind {
                    ProposedWorkItemKind::Milestone => WorkItemKind::Milestone,
                    ProposedWorkItemKind::Task => WorkItemKind::Task,
                },
                WorkItemText::parse(item.objective).map_err(|_| {
                    work_plan_error(
                        "work_plan_arguments_invalid",
                        "A revised Work item objective is invalid",
                        false,
                    )
                })?,
                WorkItemText::parse(item.expected_result).map_err(|_| {
                    work_plan_error(
                        "work_plan_arguments_invalid",
                        "A revised Work item expected result is invalid",
                        false,
                    )
                })?,
                match item.declaration_state {
                    ProposedWorkItemDeclarationState::Active => WorkItemDeclarationState::Active,
                    ProposedWorkItemDeclarationState::Superseded => {
                        WorkItemDeclarationState::Superseded
                    }
                    ProposedWorkItemDeclarationState::Cancelled => {
                        WorkItemDeclarationState::Cancelled
                    }
                },
            ))
        })
        .collect()
}

pub(super) async fn propose(
    executor: &RuntimeToolExecutor,
    args: &Value,
    invocation: ToolInvocationMetadata<'_>,
    cancel_token: Option<&CancellationToken>,
) -> astra_tools::ToolResult {
    if cancel_token.is_some_and(CancellationToken::is_cancelled) {
        return cancelled();
    }
    let Some(binding) = executor.work_binding.get() else {
        return work_plan_error(
            "work_plan_binding_required",
            "This session has no validated canonical Work binding",
            false,
        );
    };
    let mut args: ProposeWorkPlanArgs = match serde_json::from_value(args.clone()) {
        Ok(args) => args,
        Err(_) => {
            return work_plan_error(
                "work_plan_arguments_invalid",
                "propose_work_plan requires the exact typed argument contract",
                false,
            );
        }
    };
    if args.context_id.is_empty()
        || args.context_id.len() > 96
        || args.reason.trim().is_empty()
        || args.additions.len() + args.revisions.len() > 64
        || args.dependencies.len() + args.dependency_removals.len() > 256
        || (args.additions.is_empty()
            && args.revisions.is_empty()
            && args.dependencies.is_empty()
            && args.dependency_removals.is_empty())
    {
        return work_plan_error(
            "work_plan_arguments_invalid",
            "propose_work_plan arguments exceed the bounded planning contract",
            false,
        );
    }
    args.additions.sort_unstable_by(|left, right| {
        left.item_id
            .cmp(&right.item_id)
            .then_with(|| left.kind.as_str().cmp(right.kind.as_str()))
            .then_with(|| left.objective.cmp(&right.objective))
            .then_with(|| left.expected_result.cmp(&right.expected_result))
    });
    args.revisions.sort_unstable_by(|left, right| {
        left.item_id
            .cmp(&right.item_id)
            .then_with(|| left.expected_revision.cmp(&right.expected_revision))
            .then_with(|| left.kind.as_str().cmp(right.kind.as_str()))
            .then_with(|| left.objective.cmp(&right.objective))
            .then_with(|| left.expected_result.cmp(&right.expected_result))
            .then_with(|| {
                left.declaration_state
                    .as_str()
                    .cmp(right.declaration_state.as_str())
            })
    });
    let revised_bases = args
        .revisions
        .iter()
        .map(|revision| (revision.item_id.clone(), revision.expected_revision))
        .collect::<Vec<_>>();
    args.dependencies.sort_unstable_by(|left, right| {
        left.predecessor_item_id
            .cmp(&right.predecessor_item_id)
            .then_with(|| left.successor_item_id.cmp(&right.successor_item_id))
    });
    args.dependency_removals.sort_unstable_by(|left, right| {
        left.predecessor_item_id
            .cmp(&right.predecessor_item_id)
            .then_with(|| left.successor_item_id.cmp(&right.successor_item_id))
    });
    let canonical_arguments = match serde_json::to_vec(&args) {
        Ok(arguments) => arguments,
        Err(_) => {
            return work_plan_error(
                "work_planning_unavailable",
                "The typed plan proposal could not be encoded",
                true,
            );
        }
    };
    let (proposal_id, source_ref) = match invocation_identity(
        binding,
        invocation,
        RuntimeWorkProposalKind::Plan,
        &canonical_arguments,
    ) {
        Ok(identity) => identity,
        Err(()) => {
            return work_plan_error(
                "work_plan_invocation_identity_required",
                "propose_work_plan requires a complete trusted run/turn/tool-call identity",
                false,
            );
        }
    };

    let existing = match binding
        .repository
        .load_plan_proposal(&binding.owner_id, &binding.work_id, &proposal_id)
        .await
    {
        Ok(existing) => existing,
        Err(error) => return map_repository_error(error),
    };
    if let Some(existing) = existing {
        if let Err(error) = verify_retry_identity(binding, &existing, &source_ref) {
            return error;
        }
        let result = match existing.status {
            WorkProposalStatus::Accepted => accepted_result(&existing),
            WorkProposalStatus::Pending => {
                admit_or_hold(binding, existing, invocation.admission_source).await
            }
            WorkProposalStatus::Rejected
            | WorkProposalStatus::Stale
            | WorkProposalStatus::Superseded
            | WorkProposalStatus::Expired => work_plan_error(
                "work_plan_proposal_resolved",
                "This plan proposal has already reached a terminal state",
                false,
            ),
        };
        return reconcile_retired_active_attempt(executor, &revised_bases, result);
    }

    let context = match load_context(binding).await {
        Ok(context) => context,
        Err(error) => return error,
    };
    if args.context_id != context.context_id()
        || context.basis().branch_goal_revision != context.basis().goal_revision
        || context.basis().branch_criteria_set_revision != context.basis().criteria_set_revision
    {
        return work_plan_error(
            "work_plan_context_stale",
            "The Work plan changed; inspect the current context before proposing again",
            true,
        );
    }
    let additions = match parse_additions(args.additions) {
        Ok(additions) => additions,
        Err(error) => return error,
    };
    let revisions = match parse_revisions(args.revisions) {
        Ok(revisions) => revisions,
        Err(error) => return error,
    };
    let dependencies = match parse_dependencies(args.dependencies) {
        Ok(dependencies) => dependencies,
        Err(error) => return error,
    };
    let dependency_removals = match parse_dependencies(args.dependency_removals) {
        Ok(dependencies) => dependencies,
        Err(error) => return error,
    };
    let reason = match WorkChangeReason::parse(args.reason) {
        Ok(reason) => reason,
        Err(_) => {
            return work_plan_error(
                "work_plan_arguments_invalid",
                "A plan change reason is invalid",
                false,
            );
        }
    };
    let proposal = NewWorkPlanProposal {
        owner_id: binding.owner_id.clone(),
        work_id: binding.work_id.clone(),
        branch_id: binding.branch_id.clone(),
        proposal_id,
        expected_work_revision: context.basis().work_revision,
        expected_goal_revision: context.basis().goal_revision,
        expected_criteria_set_revision: context.basis().criteria_set_revision,
        expected_branch_revision: context.basis().branch_revision,
        expected_graph_revision: context.basis().graph_revision,
        additions,
        revisions,
        dependencies,
        dependency_removals,
        reason,
        source_kind: WorkProposalSourceKind::Model,
        source_ref,
    };
    let result = match binding.repository.propose_plan(proposal).await {
        Ok(proposed) => admit_or_hold(binding, proposed, invocation.admission_source).await,
        Err(error) => map_repository_error(error),
    };
    reconcile_retired_active_attempt(executor, &revised_bases, result)
}

fn reconcile_retired_active_attempt(
    executor: &RuntimeToolExecutor,
    revised_bases: &[(String, i64)],
    mut result: astra_tools::ToolResult,
) -> astra_tools::ToolResult {
    let accepted = !result.is_error
        && serde_json::from_str::<Value>(&result.output)
            .ok()
            .and_then(|payload| {
                payload
                    .get("status")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .as_deref()
            == Some("accepted");
    if !accepted {
        return result;
    }
    for (item_id, revision) in revised_bases {
        if let Err(error) = executor.retire_active_primary_work_attempt(item_id, *revision) {
            tracing::error!(%error, item_id, revision, "accepted Work revision could not retire local attempt projection");
        }
    }
    let active_attempt_present = executor.has_active_primary_work_attempt();
    let next_action = if active_attempt_present {
        "continue_active_work_item"
    } else {
        "call_run_next_work_item"
    };
    if let Ok(mut payload) = serde_json::from_str::<Value>(&result.output)
        && let Some(receipt) = payload.as_object_mut()
    {
        receipt.insert("next_action".into(), Value::String(next_action.into()));
        receipt.insert(
            "active_attempt_present".into(),
            Value::Bool(active_attempt_present),
        );
        result.output = payload.to_string();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_core::{MatrixOneSettings, SharedPool};
    use astra_services::ensure_core_schema;
    use astra_services::work::{
        InternalSessionId, OriginalIntentRef, WorkBranchId, WorkGenesis, WorkGenesisParts,
        WorkGoal, WorkId, WorkOwnerId,
    };
    use serde_json::json;
    use tempfile::TempDir;
    use uuid::Uuid;

    static TEST_POOL: tokio::sync::OnceCell<SharedPool> = tokio::sync::OnceCell::const_new();

    fn id(prefix: &str) -> String {
        format!("{prefix}-{}", Uuid::new_v4())
    }

    async fn setup_pool() -> SharedPool {
        let _ = dotenvy::dotenv();
        assert_eq!(
            std::env::var("ASTRA_TEST_DB_IT").as_deref(),
            Ok("1"),
            "set ASTRA_TEST_DB_IT=1 for the ignored Work planning integration test"
        );
        TEST_POOL
            .get_or_init(|| async {
                let settings = MatrixOneSettings::from_env();
                let catalog = std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG")
                    .unwrap_or_else(|_| "mysql".to_string());
                ensure_core_schema(&settings, &catalog)
                    .await
                    .expect("ensure canonical schema");
                SharedPool::new(&settings).await.expect("MatrixOne pool")
            })
            .await
            .clone()
    }

    fn invocation<'a>(call_id: &'a str) -> ToolInvocationMetadata<'a> {
        invocation_with_admission(call_id, Some(ToolInvocationAdmissionSource::Policy))
    }

    fn invocation_with_admission<'a>(
        call_id: &'a str,
        admission_source: Option<ToolInvocationAdmissionSource>,
    ) -> ToolInvocationMetadata<'a> {
        ToolInvocationMetadata {
            run_id: Some("run-1"),
            turn_chain_id: Some("turn-1"),
            tool_call_id: Some(call_id),
            admission_source,
            expected_control_epoch: None,
        }
    }

    fn proposal_args(context_id: &str, item_id: &str) -> Value {
        json!({
            "context_id": context_id,
            "reason": format!("Add {item_id} to the executable plan"),
            "additions": [{
                "item_id": item_id,
                "kind": "task",
                "objective": format!("Implement {item_id}"),
                "expected_result": format!("{item_id} is deterministically verified")
            }],
            "revisions": [],
            "dependencies": [],
            "dependency_removals": []
        })
    }

    #[test]
    fn repository_validation_preserves_actionable_plan_violation() {
        let result = map_repository_error(WorkRepositoryError::InvalidMutation {
            source: WorkDomainError::InvalidPlanProposal {
                violation: astra_services::work::WorkPlanProposalViolation::ConflictingItemChange,
            },
        });
        let payload: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(payload["error"]["code"], "work_plan_proposal_invalid");
        assert_eq!(payload["error"]["retryable"], false);
        assert!(
            payload["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("fresh item_id")
                    && message.contains("cancelled or superseded revision")),
            "{}",
            result.output
        );
    }

    #[test]
    fn accepted_revision_retires_matching_local_attempt_only() {
        let temp = TempDir::new().expect("temp workspace");
        let executor = RuntimeToolExecutor::new(
            temp.path().to_path_buf(),
            "owner".to_string(),
            "session".to_string(),
            None,
            None,
        );
        executor
            .install_active_primary_work_attempt(
                super::super::runtime_tool_executor::ActivePrimaryWorkAttempt {
                    attempt_id: "attempt-1".to_string(),
                    executor_run_id: "run-1".to_string(),
                    item_id: "task-2".to_string(),
                    item_revision: 1,
                    objective: "old task".to_string(),
                    expected_result: "old result".to_string(),
                },
            )
            .expect("install active attempt");

        let pending = astra_tools::ToolResult::text(json!({"status": "pending"}).to_string());
        let pending =
            reconcile_retired_active_attempt(&executor, &[("task-2".to_string(), 1)], pending);
        assert!(!pending.is_error);
        assert!(executor.has_active_primary_work_attempt());

        let accepted = astra_tools::ToolResult::text(json!({"status": "accepted"}).to_string());
        let accepted =
            reconcile_retired_active_attempt(&executor, &[("task-2".to_string(), 1)], accepted);
        assert!(!accepted.is_error);
        assert!(!executor.has_active_primary_work_attempt());
        let receipt: Value = serde_json::from_str(&accepted.output).expect("accepted receipt");
        assert_eq!(receipt["active_attempt_present"], false);
        assert_eq!(receipt["next_action"], "call_run_next_work_item");

        let retry = astra_tools::ToolResult::text(json!({"status": "accepted"}).to_string());
        let retry =
            reconcile_retired_active_attempt(&executor, &[("task-2".to_string(), 1)], retry);
        let retry_receipt: Value =
            serde_json::from_str(&retry.output).expect("idempotent retry receipt");
        assert_eq!(retry_receipt["active_attempt_present"], false);
        assert_eq!(retry_receipt["next_action"], "call_run_next_work_item");
    }

    #[test]
    fn accepted_plan_receipt_closes_the_execution_loop_without_guessing() {
        let temp = TempDir::new().expect("temp workspace");
        let executor = RuntimeToolExecutor::new(
            temp.path().to_path_buf(),
            "owner".to_string(),
            "session".to_string(),
            None,
            None,
        );
        executor
            .install_active_primary_work_attempt(
                super::super::runtime_tool_executor::ActivePrimaryWorkAttempt {
                    attempt_id: "attempt-1".to_string(),
                    executor_run_id: "run-1".to_string(),
                    item_id: "task-1".to_string(),
                    item_revision: 1,
                    objective: "current task".to_string(),
                    expected_result: "current result".to_string(),
                },
            )
            .expect("install active attempt");

        let accepted = astra_tools::ToolResult::text(json!({"status": "accepted"}).to_string());
        let accepted = reconcile_retired_active_attempt(&executor, &[], accepted);
        let receipt: Value = serde_json::from_str(&accepted.output).expect("accepted receipt");

        assert_eq!(receipt["active_attempt_present"], true);
        assert_eq!(receipt["next_action"], "continue_active_work_item");
    }

    #[tokio::test]
    async fn cancellation_wins_before_arguments_binding_or_persistence() {
        let temp = TempDir::new().expect("temp workspace");
        let executor = RuntimeToolExecutor::new(
            temp.path().to_path_buf(),
            "owner".to_string(),
            "session".to_string(),
            None,
            None,
        );
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        for result in [
            inspect(&executor, &json!({"unknown": true}), Some(&cancellation)).await,
            propose(
                &executor,
                &Value::Null,
                ToolInvocationMetadata::default(),
                Some(&cancellation),
            )
            .await,
        ] {
            assert!(result.is_error);
            assert_eq!(
                serde_json::from_str::<Value>(&result.output).unwrap()["error"]["code"],
                "work_plan_cancelled"
            );
        }
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
    async fn typed_work_plan_tool_is_owner_bound_stale_safe_and_exactly_idempotent() {
        let pool = setup_pool().await;
        let owner = WorkOwnerId::parse(id("owner")).expect("owner");
        let other_owner = WorkOwnerId::parse(id("other-owner")).expect("other owner");
        let work = WorkId::parse(id("work")).expect("work");
        let branch = WorkBranchId::parse(id("branch")).expect("branch");
        let session = InternalSessionId::parse(id("session")).expect("session");
        crate::server::work_test_support::cleanup_work_owner(&pool, owner.as_str()).await;
        crate::server::work_test_support::cleanup_work_owner(&pool, other_owner.as_str()).await;
        let repository = astra_services::work::DatabaseWorkRepository::new(pool.clone());
        repository
            .create_genesis(
                WorkGenesis::new(WorkGenesisParts {
                    owner_id: owner.clone(),
                    work_id: work.clone(),
                    branch_id: branch.clone(),
                    session_id: session.clone(),
                    project_id: None,
                    original_intent_ref: OriginalIntentRef::parse(id("intent")).expect("intent"),
                    goal: WorkGoal::parse("Prove typed root-loop Work planning.").expect("goal"),
                    criteria: Vec::new(),
                })
                .expect("Work genesis"),
            )
            .await
            .expect("create Work genesis");

        let temp = TempDir::new().expect("temp workspace");
        let mut executor = RuntimeToolExecutor::new(
            temp.path().to_path_buf(),
            owner.as_str().to_string(),
            session.as_str().to_string(),
            None,
            None,
        )
        .with_capabilities(crate::capabilities::lifecycle_server_capabilities(
            true, false,
        ));
        assert!(!executor.supports_server_tool_name("inspect_work_plan"));
        executor.set_context_manifest_pool(pool.clone());
        executor.set_work_binding(WorkRuntimeBinding::new(
            pool.clone(),
            owner.clone(),
            session.clone(),
            work.clone(),
            branch.clone(),
        ));
        assert!(executor.supports_server_tool_name("inspect_work_plan"));
        assert!(executor.supports_server_tool_name("propose_work_plan"));

        let inspected = inspect(&executor, &json!({}), None).await;
        assert!(!inspected.is_error, "inspect failed: {inspected:?}");
        let inspected: Value = serde_json::from_str(&inspected.output).expect("context JSON");
        assert_eq!(inspected["observation"]["coherence"], "coherent");
        assert_eq!(
            inspected["observation"]["finding"]["fact_code"],
            "criteria_not_accepted"
        );
        assert_eq!(
            inspected["observation"]["as_of"]["work_revision"],
            inspected["basis"]["work_revision"]
        );
        assert!(
            inspected["observation"]["report_id"]
                .as_str()
                .expect("observation report identity")
                .starts_with("work-observation:")
        );
        assert_eq!(inspected["items"]["entries"][0]["item_id"], "root");
        assert_eq!(inspected["items"]["entries"][0]["kind"], "milestone");
        assert_eq!(inspected["execution_contract"]["executable_kind"], "task");
        assert_eq!(
            inspected["execution_contract"]["milestones_own_attempts"],
            false
        );
        assert_eq!(
            inspected["execution_contract"]["milestones_require_settlement"],
            false
        );
        let first_context_id = inspected["context_id"].as_str().expect("context id");
        let first_args = proposal_args(first_context_id, "task-first");
        let missing_identity = propose(
            &executor,
            &first_args,
            ToolInvocationMetadata::default(),
            None,
        )
        .await;
        assert!(missing_identity.is_error);
        assert_eq!(
            serde_json::from_str::<Value>(&missing_identity.output).unwrap()["error"]["code"],
            "work_plan_invocation_identity_required"
        );
        let proposal_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM work_proposals WHERE owner_id = ? AND work_id = ?",
        )
        .bind(owner.as_str())
        .bind(work.as_str())
        .fetch_one(pool.get())
        .await
        .expect("proposal count before trusted invocation");
        assert_eq!(
            proposal_count, 0,
            "untrusted identity must leave no residue"
        );
        let first = propose(&executor, &first_args, invocation("call-first"), None).await;
        assert!(!first.is_error, "first proposal failed: {first:?}");
        let first_output: Value = serde_json::from_str(&first.output).expect("accepted proposal");
        assert_eq!(first_output["status"], "accepted");
        let retry = propose(&executor, &first_args, invocation("call-first"), None).await;
        assert_eq!(retry.output, first.output, "exact retry must be stable");

        let changed_retry = propose(
            &executor,
            &proposal_args(first_context_id, "task-different"),
            invocation("call-first"),
            None,
        )
        .await;
        assert!(changed_retry.is_error);
        assert_eq!(
            serde_json::from_str::<Value>(&changed_retry.output).unwrap()["error"]["code"],
            "work_plan_invocation_conflict"
        );

        let first_proposal_id = WorkProposalId::parse(
            first_output["proposal_id"]
                .as_str()
                .expect("proposal identity"),
        )
        .expect("typed proposal identity");
        let admitted = repository
            .load_plan_proposal(&owner, &work, &first_proposal_id)
            .await
            .expect("load proposal")
            .expect("admitted proposal");
        assert_eq!(admitted.status, WorkProposalStatus::Accepted);
        assert!(
            admitted
                .resolution
                .as_ref()
                .expect("policy resolution")
                .resolution_ref
                .as_str()
                .starts_with("work-plan-policy-v1-"),
            "automatic admission must retain the existing policy grant source"
        );

        let stale = propose(
            &executor,
            &proposal_args(first_context_id, "task-stale"),
            invocation("call-stale"),
            None,
        )
        .await;
        assert!(stale.is_error);
        assert_eq!(
            serde_json::from_str::<Value>(&stale.output).unwrap()["error"]["code"],
            "work_plan_context_stale"
        );
        let proposal_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM work_proposals WHERE owner_id = ? AND work_id = ?",
        )
        .bind(owner.as_str())
        .bind(work.as_str())
        .fetch_one(pool.get())
        .await
        .expect("proposal count after stale rejection");
        assert_eq!(
            proposal_count, 1,
            "stale input must leave no proposal residue"
        );

        let current = inspect(&executor, &json!({}), None).await;
        let current: Value = serde_json::from_str(&current.output).expect("current context JSON");
        let current_context_id = current["context_id"].as_str().expect("current context id");
        let needs_review_args = json!({
            "context_id": current_context_id,
            "reason": "Add a newly discovered prerequisite",
            "additions": [{
                "item_id": "task-needs-review",
                "kind": "task",
                "objective": "Insert a prerequisite before accepted work",
                "expected_result": "The accepted task is newly gated"
            }],
            "revisions": [],
            "dependencies": [{
                "predecessor_item_id": "task-needs-review",
                "successor_item_id": "task-first"
            }],
            "dependency_removals": []
        });
        let needs_review = propose(
            &executor,
            &needs_review_args,
            invocation_with_admission("call-needs-review", None),
            None,
        )
        .await;
        assert!(!needs_review.is_error, "pending review is a valid outcome");
        let needs_review: Value =
            serde_json::from_str(&needs_review.output).expect("review outcome");
        assert_eq!(needs_review["status"], "pending");
        assert_eq!(
            needs_review["admission"]["reason"],
            "invocation_not_policy_admitted"
        );
        let unchanged = inspect(&executor, &json!({}), None).await;
        let unchanged: Value =
            serde_json::from_str(&unchanged.output).expect("unchanged context JSON");
        assert_eq!(unchanged["context_id"], current_context_id);

        let approved = propose(
            &executor,
            &needs_review_args,
            invocation_with_admission(
                "call-needs-review",
                Some(ToolInvocationAdmissionSource::ParentApproval),
            ),
            None,
        )
        .await;
        let approved: Value = serde_json::from_str(&approved.output).expect("approved outcome");
        assert_eq!(approved["status"], "accepted");
        let approved_record = repository
            .load_plan_proposal(
                &owner,
                &work,
                &WorkProposalId::parse(needs_review["proposal_id"].as_str().expect("proposal id"))
                    .expect("proposal id"),
            )
            .await
            .expect("load approved proposal")
            .expect("approved proposal");
        assert!(
            approved_record
                .resolution
                .as_ref()
                .expect("approval resolution")
                .resolution_ref
                .as_str()
                .starts_with("work-plan-parent-approval-v1-")
        );

        let replan_basis = inspect(&executor, &json!({}), None).await;
        let replan_basis: Value = serde_json::from_str(&replan_basis.output).expect("replan basis");
        let replan_context_id = replan_basis["context_id"]
            .as_str()
            .expect("replan context id");
        let obsolete = replan_basis["items"]["entries"]
            .as_array()
            .expect("item entries")
            .iter()
            .find(|item| item["item_id"] == "task-needs-review")
            .expect("accepted prerequisite");
        let replan_args = json!({
            "context_id": replan_context_id,
            "reason": "New evidence makes the prerequisite obsolete",
            "additions": [],
            "revisions": [{
                "item_id": "task-needs-review",
                "expected_revision": obsolete["revision"],
                "kind": obsolete["kind"],
                "objective": obsolete["objective"],
                "expected_result": obsolete["expected_result"],
                "declaration_state": "cancelled"
            }],
            "dependencies": [],
            "dependency_removals": [{
                "predecessor_item_id": "task-needs-review",
                "successor_item_id": "task-first"
            }]
        });
        let pending_replan = propose(
            &executor,
            &replan_args,
            invocation_with_admission("call-replan", None),
            None,
        )
        .await;
        assert!(!pending_replan.is_error, "pending replan is valid");
        let pending_replan: Value =
            serde_json::from_str(&pending_replan.output).expect("pending replan");
        assert_eq!(pending_replan["status"], "pending");
        let unchanged_after_replan = inspect(&executor, &json!({}), None).await;
        let unchanged_after_replan: Value = serde_json::from_str(&unchanged_after_replan.output)
            .expect("unchanged pending replan context");
        assert_eq!(unchanged_after_replan["context_id"], replan_context_id);

        let accepted_replan = propose(
            &executor,
            &replan_args,
            invocation_with_admission(
                "call-replan",
                Some(ToolInvocationAdmissionSource::ParentApproval),
            ),
            None,
        )
        .await;
        let accepted_replan: Value =
            serde_json::from_str(&accepted_replan.output).expect("accepted replan");
        assert_eq!(accepted_replan["status"], "accepted");
        let replanned = inspect(&executor, &json!({}), None).await;
        let replanned: Value = serde_json::from_str(&replanned.output).expect("replanned context");
        let obsolete = replanned["items"]["entries"]
            .as_array()
            .expect("replanned items")
            .iter()
            .find(|item| item["item_id"] == "task-needs-review")
            .expect("revised prerequisite");
        assert_eq!(obsolete["revision"], 2);
        assert_eq!(obsolete["declaration_state"], "cancelled");
        assert!(
            replanned["dependencies"]["entries"]
                .as_array()
                .expect("dependencies")
                .iter()
                .all(|edge| edge["predecessor_item_id"] != "task-needs-review")
        );

        let concurrent_basis = inspect(&executor, &json!({}), None).await;
        let concurrent_basis: Value =
            serde_json::from_str(&concurrent_basis.output).expect("concurrent basis");
        let concurrent_context_id = concurrent_basis["context_id"]
            .as_str()
            .expect("concurrent context id");
        let concurrent_args = proposal_args(concurrent_context_id, "task-concurrent");
        let (left, right) = tokio::join!(
            propose(
                &executor,
                &concurrent_args,
                invocation("call-concurrent"),
                None
            ),
            propose(
                &executor,
                &concurrent_args,
                invocation("call-concurrent"),
                None
            )
        );
        assert!(!left.is_error, "left concurrent proposal: {left:?}");
        assert!(!right.is_error, "right concurrent proposal: {right:?}");
        assert_eq!(left.output, right.output);
        let proposal_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM work_proposals WHERE owner_id = ? AND work_id = ?",
        )
        .bind(owner.as_str())
        .bind(work.as_str())
        .fetch_one(pool.get())
        .await
        .expect("proposal count after concurrent retry");
        assert_eq!(
            proposal_count, 4,
            "each logical invocation creates exactly one durable proposal"
        );

        let stale_page = inspect(
            &executor,
            &json!({"context_id": current_context_id, "item_offset": 0}),
            None,
        )
        .await;
        assert!(stale_page.is_error);
        assert_eq!(
            serde_json::from_str::<Value>(&stale_page.output).unwrap()["error"]["code"],
            "work_plan_context_stale"
        );

        let paging_basis = inspect(&executor, &json!({}), None).await;
        let paging_basis: Value = serde_json::from_str(&paging_basis.output).expect("paging basis");
        let paging_context_id = paging_basis["context_id"]
            .as_str()
            .expect("paging context id");
        let page_additions = (0..6)
            .map(|index| {
                json!({
                    "item_id": format!("task-page-{index}"),
                    "kind": "task",
                    "objective": format!("Implement page task {index}"),
                    "expected_result": format!("Page task {index} is verified")
                })
            })
            .collect::<Vec<_>>();
        let page_expansion = propose(
            &executor,
            &json!({
                "context_id": paging_context_id,
                "reason": "Expand the next bounded planning window",
                "additions": page_additions,
                "revisions": [],
                "dependencies": [],
                "dependency_removals": []
            }),
            invocation("call-page-expansion"),
            None,
        )
        .await;
        assert!(
            !page_expansion.is_error,
            "page expansion: {page_expansion:?}"
        );

        let first_page = inspect(&executor, &json!({}), None).await;
        assert!(
            first_page.output.len() <= INSPECT_WORK_PLAN_MAX_OUTPUT_BYTES,
            "model-facing inspection must have a hard payload bound"
        );
        let first_page: Value = serde_json::from_str(&first_page.output).expect("first page");
        assert_eq!(
            first_page["items"]["entries"]
                .as_array()
                .expect("item entries")
                .len(),
            INSPECT_WORK_PLAN_ITEM_PAGE_SIZE
        );
        assert_eq!(first_page["items"]["next_offset"], 8);
        let page_context_id = first_page["context_id"].as_str().expect("page context id");
        let final_page = inspect(
            &executor,
            &json!({"context_id": page_context_id, "item_offset": 8}),
            None,
        )
        .await;
        let final_page: Value = serde_json::from_str(&final_page.output).expect("final page");
        assert_eq!(
            final_page["items"]["entries"]
                .as_array()
                .expect("final item entries")
                .len(),
            2
        );
        assert!(final_page["items"]["next_offset"].is_null());

        // One production owner may have many live conversations. Session
        // identity, not merely owner identity, must select the canonical
        // branch, and concurrent reads must never accept another session's
        // revision token.
        let second_work = WorkId::parse(id("work-second")).expect("second work");
        let second_branch = WorkBranchId::parse(id("branch-second")).expect("second branch");
        let second_session =
            InternalSessionId::parse(id("session-second")).expect("second session");
        repository
            .create_genesis(
                WorkGenesis::new(WorkGenesisParts {
                    owner_id: owner.clone(),
                    work_id: second_work.clone(),
                    branch_id: second_branch.clone(),
                    session_id: second_session.clone(),
                    project_id: None,
                    original_intent_ref: OriginalIntentRef::parse(id("intent-second"))
                        .expect("second intent"),
                    goal: WorkGoal::parse("Keep a second session isolated.").expect("second goal"),
                    criteria: Vec::new(),
                })
                .expect("second Work genesis"),
            )
            .await
            .expect("create second Work genesis");
        let mut second_executor = RuntimeToolExecutor::new(
            temp.path().to_path_buf(),
            owner.as_str().to_string(),
            second_session.as_str().to_string(),
            None,
            None,
        );
        second_executor.set_context_manifest_pool(pool.clone());
        second_executor.set_work_binding(WorkRuntimeBinding::new(
            pool.clone(),
            owner.clone(),
            second_session,
            second_work,
            second_branch,
        ));
        let first_session_args = json!({});
        let second_session_args = json!({});
        let (first_session_read, second_session_read) = tokio::join!(
            inspect(&executor, &first_session_args, None),
            inspect(&second_executor, &second_session_args, None),
        );
        assert!(!first_session_read.is_error);
        assert!(!second_session_read.is_error);
        let second_session_read: Value =
            serde_json::from_str(&second_session_read.output).expect("second session context");
        assert_ne!(second_session_read["context_id"], page_context_id);
        let cross_session = inspect(
            &second_executor,
            &json!({"context_id": page_context_id}),
            None,
        )
        .await;
        assert!(cross_session.is_error);
        assert_eq!(
            serde_json::from_str::<Value>(&cross_session.output).unwrap()["error"]["code"],
            "work_plan_context_stale"
        );

        let mut wrong_owner_executor = RuntimeToolExecutor::new(
            temp.path().to_path_buf(),
            other_owner.as_str().to_string(),
            session.as_str().to_string(),
            None,
            None,
        );
        wrong_owner_executor.set_work_binding(WorkRuntimeBinding::new(
            pool.clone(),
            other_owner.clone(),
            session,
            work,
            branch,
        ));
        let isolated = inspect(&wrong_owner_executor, &json!({}), None).await;
        assert!(isolated.is_error, "cross-owner context must not be visible");
        assert_eq!(
            serde_json::from_str::<Value>(&isolated.output).unwrap()["error"]["code"],
            "work_plan_binding_not_found"
        );

        crate::server::work_test_support::cleanup_work_owner(&pool, owner.as_str()).await;
        crate::server::work_test_support::cleanup_work_owner(&pool, other_owner.as_str()).await;
    }
}
