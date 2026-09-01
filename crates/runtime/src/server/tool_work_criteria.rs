use astra_services::work::{
    CriterionCommand, CriterionDefinition, CriterionId, CriterionRevision, NewWorkCriteriaProposal,
    RecordedWorkCriteriaProposal, WorkCriteriaProposalMember, WorkCriteriaQuery,
    WorkProposalBasisResource, WorkProposalSourceKind, WorkProposalStatus, WorkRepository,
    WorkRepositoryError,
};
use astra_tools::{ToolResult, tool_engine::ToolInvocationMetadata};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::runtime_tool_executor::{RuntimeToolExecutor, WorkRuntimeBinding};
use super::tool_work_proposal::{RuntimeWorkProposalKind, invocation_identity};

const INSPECT_CRITERIA_PAGE_SIZE: u16 = 4;
const INSPECT_CRITERIA_MAX_OUTPUT_BYTES: usize = 384 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InspectWorkCriteriaArgs {
    context_id: Option<String>,
    offset: Option<u16>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProposeWorkCriteriaArgs {
    context_id: String,
    members: Vec<ProposedCriterionMember>,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "member_kind", rename_all = "snake_case", deny_unknown_fields)]
enum ProposedCriterionMember {
    Existing {
        criterion_id: String,
        revision: i64,
    },
    New {
        criterion_id: String,
        definition: ProposedCriterionDefinition,
    },
}

impl ProposedCriterionMember {
    fn criterion_id(&self) -> &str {
        match self {
            Self::Existing { criterion_id, .. } | Self::New { criterion_id, .. } => criterion_id,
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ProposedCriterionDefinition {
    CommandCheck { statement: String, command: String },
    TestCheck { statement: String, command: String },
    HumanReview { statement: String },
}

fn criteria_error(code: &str, message: &str, retryable: bool) -> ToolResult {
    let output = json!({
        "status": "error",
        "error": {"code": code, "message": message, "retryable": retryable}
    })
    .to_string();
    let mut result = ToolResult::error(output);
    result.metadata = Some(serde_json::Map::from_iter([
        ("error_kind".to_string(), Value::String(code.to_string())),
        ("retryable".to_string(), Value::Bool(retryable)),
    ]));
    result
}

fn map_repository_error(error: WorkRepositoryError) -> ToolResult {
    if matches!(
        &error,
        WorkRepositoryError::Persistence { .. }
            | WorkRepositoryError::Corrupt { .. }
            | WorkRepositoryError::ManifestEncoding { .. }
    ) {
        tracing::warn!(error = %error, "canonical Work criteria repository degraded");
    }
    match error {
        WorkRepositoryError::InvalidMutation { .. }
        | WorkRepositoryError::InvalidWorkProposalBasis {
            resource: WorkProposalBasisResource::NewCriterionIdentity,
        } => criteria_error(
            "work_criteria_proposal_invalid",
            "The typed Done-when proposal violates the canonical criteria contract",
            false,
        ),
        WorkRepositoryError::InvalidWorkProposalBasis {
            resource: WorkProposalBasisResource::BranchIdentity,
        } => criteria_error(
            "work_criteria_binding_changed",
            "The session is no longer bound to the validated Work branch",
            false,
        ),
        WorkRepositoryError::InvalidWorkProposalBasis { .. }
        | WorkRepositoryError::StaleCriteriaPageRevision { .. } => criteria_error(
            "work_criteria_context_stale",
            "The Work context changed; inspect criteria before proposing again",
            true,
        ),
        WorkRepositoryError::WorkProposalCapacityExceeded => criteria_error(
            "work_criteria_proposal_capacity",
            "This Work branch has reached its bounded pending-proposal capacity",
            true,
        ),
        WorkRepositoryError::WorkProposalAlreadyResolved { .. } => criteria_error(
            "work_criteria_proposal_resolved",
            "This Done-when proposal has already reached a terminal state",
            false,
        ),
        WorkRepositoryError::NotFound | WorkRepositoryError::Archived => criteria_error(
            "work_criteria_binding_not_found",
            "The bound canonical Work branch is no longer available",
            false,
        ),
        WorkRepositoryError::Conflict { .. } => criteria_error(
            "work_criteria_proposal_conflict",
            "The proposal conflicts with an existing canonical identity",
            false,
        ),
        WorkRepositoryError::Persistence { .. }
        | WorkRepositoryError::Corrupt { .. }
        | WorkRepositoryError::ManifestEncoding { .. } => criteria_error(
            "work_criteria_unavailable",
            "Canonical Work criteria are temporarily unavailable",
            true,
        ),
        _ => criteria_error(
            "work_criteria_rejected",
            "The canonical Work repository rejected this criteria operation",
            false,
        ),
    }
}

async fn load_context(
    binding: &WorkRuntimeBinding,
) -> Result<astra_services::work::WorkPlanContext, ToolResult> {
    let context = binding
        .repository
        .load_plan_context_for_session(&binding.owner_id, &binding.session_id)
        .await
        .map_err(map_repository_error)?;
    if context.basis().work_id != binding.work_id || context.basis().branch_id != binding.branch_id
    {
        return Err(criteria_error(
            "work_criteria_binding_changed",
            "The session is no longer bound to the validated Work branch",
            false,
        ));
    }
    Ok(context)
}

pub(super) async fn inspect(
    executor: &RuntimeToolExecutor,
    args: &Value,
    cancel_token: Option<&CancellationToken>,
) -> ToolResult {
    if cancel_token.is_some_and(CancellationToken::is_cancelled) {
        return criteria_error(
            "work_criteria_cancelled",
            "Work criteria inspection was cancelled",
            true,
        );
    }
    let args: InspectWorkCriteriaArgs = match serde_json::from_value(args.clone()) {
        Ok(args) => args,
        Err(_) => {
            return criteria_error(
                "work_criteria_arguments_invalid",
                "inspect_work_criteria requires the exact typed pagination contract",
                false,
            );
        }
    };
    if args
        .context_id
        .as_ref()
        .is_some_and(|context_id| context_id.is_empty() || context_id.len() > 96)
    {
        return criteria_error(
            "work_criteria_arguments_invalid",
            "inspect_work_criteria context identity is invalid",
            false,
        );
    }
    let Some(binding) = executor.work_binding.get() else {
        return criteria_error(
            "work_criteria_binding_required",
            "This session has no validated canonical Work binding",
            false,
        );
    };
    let context = match load_context(binding).await {
        Ok(context) => context,
        Err(error) => return error,
    };
    if args
        .context_id
        .as_deref()
        .is_some_and(|expected| expected != context.context_id())
    {
        return criteria_error(
            "work_criteria_context_stale",
            "The Work changed; restart criteria inspection from the current context",
            true,
        );
    }
    let offset = args.offset.unwrap_or(0);
    if offset > 0 && args.context_id.is_none() {
        return criteria_error(
            "work_criteria_arguments_invalid",
            "A continuation offset requires the exact inspected context_id",
            false,
        );
    }
    let query = match WorkCriteriaQuery::new(
        binding.owner_id.clone(),
        binding.work_id.clone(),
        Some(context.basis().criteria_set_revision),
        offset,
        INSPECT_CRITERIA_PAGE_SIZE,
    ) {
        Ok(query) => query,
        Err(_) => {
            return criteria_error(
                "work_criteria_arguments_invalid",
                "inspect_work_criteria offset exceeds the bounded criterion set",
                false,
            );
        }
    };
    let page = match binding.repository.load_criteria_page(query).await {
        Ok(page) => page,
        Err(error) => return map_repository_error(error),
    };
    let output = match serde_json::to_string(&json!({
        "schema_version": 1,
        "context_id": context.context_id(),
        "content_hash": context.content_hash(),
        "basis": context.basis(),
        "criteria": page.criteria,
        "next_offset": page.next_cursor.map(|cursor| cursor.offset),
    })) {
        Ok(output) => output,
        Err(_) => {
            return criteria_error(
                "work_criteria_unavailable",
                "The canonical criteria page could not be encoded",
                true,
            );
        }
    };
    if output.len() > INSPECT_CRITERIA_MAX_OUTPUT_BYTES {
        tracing::warn!(
            output_bytes = output.len(),
            context_id = context.context_id(),
            "bounded Work criteria projection exceeded its invariant"
        );
        return criteria_error(
            "work_criteria_unavailable",
            "The bounded canonical criteria page could not be projected",
            true,
        );
    }
    ToolResult::text(output)
}

fn parse_definition(
    definition: ProposedCriterionDefinition,
) -> Result<CriterionDefinition, ToolResult> {
    let parse_statement = |statement| {
        astra_services::work::CriterionStatement::parse(statement).map_err(|_| {
            criteria_error(
                "work_criteria_arguments_invalid",
                "A proposed criterion statement is invalid",
                false,
            )
        })
    };
    let parse_command = |command| {
        CriterionCommand::parse(command).map_err(|_| {
            criteria_error(
                "work_criteria_arguments_invalid",
                "A proposed criterion command is invalid",
                false,
            )
        })
    };
    Ok(match definition {
        ProposedCriterionDefinition::CommandCheck { statement, command } => {
            CriterionDefinition::CommandCheck {
                statement: parse_statement(statement)?,
                command: parse_command(command)?,
            }
        }
        ProposedCriterionDefinition::TestCheck { statement, command } => {
            CriterionDefinition::TestCheck {
                statement: parse_statement(statement)?,
                command: parse_command(command)?,
            }
        }
        ProposedCriterionDefinition::HumanReview { statement } => {
            CriterionDefinition::HumanReview {
                statement: parse_statement(statement)?,
            }
        }
    })
}

fn parse_members(
    members: Vec<ProposedCriterionMember>,
) -> Result<Vec<WorkCriteriaProposalMember>, ToolResult> {
    members
        .into_iter()
        .map(|member| match member {
            ProposedCriterionMember::Existing {
                criterion_id,
                revision,
            } => Ok(WorkCriteriaProposalMember::Existing {
                criterion_id: CriterionId::parse(criterion_id).map_err(|_| {
                    criteria_error(
                        "work_criteria_arguments_invalid",
                        "A proposed criterion identity is invalid",
                        false,
                    )
                })?,
                revision: CriterionRevision::new(revision).map_err(|_| {
                    criteria_error(
                        "work_criteria_arguments_invalid",
                        "A proposed criterion revision is invalid",
                        false,
                    )
                })?,
            }),
            ProposedCriterionMember::New {
                criterion_id,
                definition,
            } => Ok(WorkCriteriaProposalMember::New {
                criterion_id: CriterionId::parse(criterion_id).map_err(|_| {
                    criteria_error(
                        "work_criteria_arguments_invalid",
                        "A proposed criterion identity is invalid",
                        false,
                    )
                })?,
                definition: parse_definition(definition)?,
            }),
        })
        .collect()
}

fn proposal_result(proposal: &RecordedWorkCriteriaProposal) -> ToolResult {
    let admission = match proposal.status {
        WorkProposalStatus::Pending => json!({
            "mode": "needs_user_review",
            "reason": "done_when_changes_require_explicit_acceptance",
        }),
        WorkProposalStatus::Accepted => json!({
            "mode": "resolved",
            "reason": "explicit_decision_accepted",
        }),
        WorkProposalStatus::Rejected
        | WorkProposalStatus::Stale
        | WorkProposalStatus::Superseded
        | WorkProposalStatus::Expired => {
            unreachable!("terminal non-accepted proposals are returned as typed errors")
        }
    };
    ToolResult::text(
        json!({
            "status": proposal.status,
            "proposal_id": proposal.proposal.proposal_id.as_str(),
            "payload_hash": proposal.payload_hash.as_str(),
            "admission": admission,
            "result_work_revision": proposal.resolution.as_ref().and_then(|r| r.result_work_revision).map(|r| r.get()),
            "result_criteria_set_revision": proposal.resolution.as_ref().and_then(|r| r.result_criteria_set_revision).map(|r| r.get()),
        })
        .to_string(),
    )
}

pub(super) async fn propose(
    executor: &RuntimeToolExecutor,
    args: &Value,
    invocation: ToolInvocationMetadata<'_>,
    cancel_token: Option<&CancellationToken>,
) -> ToolResult {
    if cancel_token.is_some_and(CancellationToken::is_cancelled) {
        return criteria_error(
            "work_criteria_cancelled",
            "The Done-when proposal was not persisted because the run was cancelled",
            true,
        );
    }
    let Some(binding) = executor.work_binding.get() else {
        return criteria_error(
            "work_criteria_binding_required",
            "This session has no validated canonical Work binding",
            false,
        );
    };
    let mut args: ProposeWorkCriteriaArgs = match serde_json::from_value(args.clone()) {
        Ok(args) => args,
        Err(_) => {
            return criteria_error(
                "work_criteria_arguments_invalid",
                "propose_work_criteria requires the exact typed argument contract",
                false,
            );
        }
    };
    if args.context_id.is_empty()
        || args.context_id.len() > 96
        || args.members.is_empty()
        || args.members.len() > 128
    {
        return criteria_error(
            "work_criteria_arguments_invalid",
            "propose_work_criteria arguments exceed the bounded criteria contract",
            false,
        );
    }
    args.members
        .sort_unstable_by(|left, right| left.criterion_id().cmp(right.criterion_id()));
    let canonical_arguments = match serde_json::to_vec(&args) {
        Ok(arguments) => arguments,
        Err(_) => {
            return criteria_error(
                "work_criteria_unavailable",
                "The typed Done-when proposal could not be encoded",
                true,
            );
        }
    };
    let (proposal_id, source_ref) = match invocation_identity(
        binding,
        invocation,
        RuntimeWorkProposalKind::Criteria,
        &canonical_arguments,
    ) {
        Ok(identity) => identity,
        Err(()) => {
            return criteria_error(
                "work_criteria_invocation_identity_required",
                "propose_work_criteria requires a complete trusted run/turn/tool-call identity",
                false,
            );
        }
    };
    match binding
        .repository
        .load_criteria_proposal(&binding.owner_id, &binding.work_id, &proposal_id)
        .await
    {
        Ok(Some(existing)) => {
            if existing.proposal.owner_id != binding.owner_id
                || existing.proposal.work_id != binding.work_id
                || existing.proposal.branch_id != binding.branch_id
                || existing.proposal.source_kind != WorkProposalSourceKind::Model
                || existing.proposal.source_ref != source_ref
            {
                return criteria_error(
                    "work_criteria_invocation_conflict",
                    "The trusted invocation identity conflicts with an existing proposal",
                    false,
                );
            }
            return match existing.status {
                WorkProposalStatus::Pending | WorkProposalStatus::Accepted => {
                    proposal_result(&existing)
                }
                WorkProposalStatus::Rejected
                | WorkProposalStatus::Stale
                | WorkProposalStatus::Superseded
                | WorkProposalStatus::Expired => criteria_error(
                    "work_criteria_proposal_resolved",
                    "This Done-when proposal has already reached a terminal state",
                    false,
                ),
            };
        }
        Ok(None) => {}
        Err(error) => return map_repository_error(error),
    }
    let context = match load_context(binding).await {
        Ok(context) => context,
        Err(error) => return error,
    };
    if args.context_id != context.context_id()
        || context.basis().branch_goal_revision != context.basis().goal_revision
        || context.basis().branch_criteria_set_revision != context.basis().criteria_set_revision
    {
        return criteria_error(
            "work_criteria_context_stale",
            "The Work changed; inspect criteria before proposing again",
            true,
        );
    }
    let members = match parse_members(args.members) {
        Ok(members) => members,
        Err(error) => return error,
    };
    let proposal = NewWorkCriteriaProposal {
        owner_id: binding.owner_id.clone(),
        work_id: binding.work_id.clone(),
        branch_id: binding.branch_id.clone(),
        proposal_id,
        expected_work_revision: context.basis().work_revision,
        expected_goal_revision: context.basis().goal_revision,
        expected_criteria_set_revision: context.basis().criteria_set_revision,
        expected_branch_revision: context.basis().branch_revision,
        expected_graph_revision: context.basis().graph_revision,
        members,
        source_kind: WorkProposalSourceKind::Model,
        source_ref,
    };
    match binding.repository.propose_criteria(proposal).await {
        Ok(proposed) => proposal_result(&proposed),
        Err(error) => map_repository_error(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_core::{MatrixOneSettings, SharedPool};
    use astra_services::ensure_core_schema;
    use astra_services::work::{
        InternalSessionId, OriginalIntentRef, WorkBranchId, WorkChangeRef,
        WorkCriteriaProposalAcceptance, WorkGenesis, WorkGenesisParts, WorkGoal, WorkId,
        WorkOwnerId, WorkProposalId,
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
            "set ASTRA_TEST_DB_IT=1 for the ignored Work criteria integration test"
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

    fn invocation(call_id: &str) -> ToolInvocationMetadata<'_> {
        ToolInvocationMetadata {
            run_id: Some("run-1"),
            turn_chain_id: Some("turn-1"),
            tool_call_id: Some(call_id),
            admission_source: Some(astra_tools::tool_engine::ToolInvocationAdmissionSource::Policy),
            expected_control_epoch: None,
        }
    }

    fn proposal_args(context_id: &str) -> Value {
        json!({
            "context_id": context_id,
            "members": [
                {
                    "member_kind": "new",
                    "criterion_id": "review-complete",
                    "definition": {
                        "kind": "human_review",
                        "statement": "The result is reviewable."
                    }
                },
                {
                    "member_kind": "new",
                    "criterion_id": "tests-pass",
                    "definition": {
                        "kind": "test_check",
                        "statement": "Relevant tests pass.",
                        "command": "cargo test -p astra-runtime"
                    }
                }
            ]
        })
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
                "work_criteria_cancelled"
            );
        }
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
    async fn typed_criteria_tool_is_provisional_stale_safe_and_exactly_idempotent() {
        let pool = setup_pool().await;
        let owner = WorkOwnerId::parse(id("owner")).expect("owner");
        let work = WorkId::parse(id("work")).expect("work");
        let branch = WorkBranchId::parse(id("branch")).expect("branch");
        let session = InternalSessionId::parse(id("session")).expect("session");
        crate::server::work_test_support::cleanup_work_owner(&pool, owner.as_str()).await;
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
                    goal: WorkGoal::parse("Prove provisional typed Done-when criteria.")
                        .expect("goal"),
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
        executor.set_context_manifest_pool(pool.clone());
        executor.set_work_binding(WorkRuntimeBinding::new(
            pool.clone(),
            owner.clone(),
            session.clone(),
            work.clone(),
            branch.clone(),
        ));
        assert!(executor.supports_server_tool_name("inspect_work_criteria"));
        assert!(executor.supports_server_tool_name("propose_work_criteria"));

        let inspected = inspect(&executor, &json!({}), None).await;
        assert!(!inspected.is_error, "inspect failed: {inspected:?}");
        let inspected: Value = serde_json::from_str(&inspected.output).expect("context JSON");
        assert_eq!(inspected["criteria"]["total"], 0);
        let context_id = inspected["context_id"].as_str().expect("context id");
        let args = proposal_args(context_id);

        let missing_identity =
            propose(&executor, &args, ToolInvocationMetadata::default(), None).await;
        assert!(missing_identity.is_error);
        assert_eq!(
            serde_json::from_str::<Value>(&missing_identity.output).unwrap()["error"]["code"],
            "work_criteria_invocation_identity_required"
        );
        let proposal_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM work_proposals WHERE owner_id = ? AND work_id = ?",
        )
        .bind(owner.as_str())
        .bind(work.as_str())
        .fetch_one(pool.get())
        .await
        .expect("proposal count");
        assert_eq!(proposal_count, 0, "untrusted invocation leaves no residue");

        let first = propose(&executor, &args, invocation("criteria-call"), None).await;
        assert!(!first.is_error, "proposal failed: {first:?}");
        let first_json: Value = serde_json::from_str(&first.output).expect("proposal outcome");
        assert_eq!(first_json["status"], "pending");
        assert_eq!(first_json["admission"]["mode"], "needs_user_review");
        let retry = propose(&executor, &args, invocation("criteria-call"), None).await;
        assert_eq!(retry.output, first.output);
        let mut reordered = args.clone();
        reordered["members"]
            .as_array_mut()
            .expect("members")
            .reverse();
        let reordered_retry =
            propose(&executor, &reordered, invocation("criteria-call"), None).await;
        assert_eq!(
            reordered_retry.output, first.output,
            "member order is not semantic"
        );
        let mut changed = args.clone();
        changed["members"][1]["definition"]["statement"] =
            json!("A different verification contract.");
        let changed_retry = propose(&executor, &changed, invocation("criteria-call"), None).await;
        assert!(changed_retry.is_error);
        assert_eq!(
            serde_json::from_str::<Value>(&changed_retry.output).unwrap()["error"]["code"],
            "work_criteria_invocation_conflict"
        );

        let unchanged = repository.load(&owner, &work).await.expect("load Work");
        assert_eq!(
            unchanged.work.parts().current_criteria_set_revision.get(),
            1,
            "a model proposal cannot accept its own Done-when criteria"
        );
        let proposal_id =
            WorkProposalId::parse(first_json["proposal_id"].as_str().expect("proposal id"))
                .expect("proposal id");
        let recorded = repository
            .load_criteria_proposal(&owner, &work, &proposal_id)
            .await
            .expect("load proposal")
            .expect("proposal");
        let accepted = repository
            .accept_criteria_proposal(WorkCriteriaProposalAcceptance {
                owner_id: owner.clone(),
                work_id: work.clone(),
                branch_id: branch.clone(),
                proposal_id,
                payload_hash: recorded.payload_hash,
                expected_work_revision: recorded.proposal.expected_work_revision,
                expected_goal_revision: recorded.proposal.expected_goal_revision,
                expected_criteria_set_revision: recorded.proposal.expected_criteria_set_revision,
                expected_branch_revision: recorded.proposal.expected_branch_revision,
                expected_graph_revision: recorded.proposal.expected_graph_revision,
                resolution_ref: WorkChangeRef::parse(id("user-decision")).expect("decision"),
            })
            .await
            .expect("accept proposal");
        assert_eq!(accepted.status, WorkProposalStatus::Accepted);
        let after_accept = propose(&executor, &args, invocation("criteria-call"), None).await;
        let after_accept: Value =
            serde_json::from_str(&after_accept.output).expect("accepted retry");
        assert_eq!(after_accept["status"], "accepted");
        assert_eq!(after_accept["admission"]["mode"], "resolved");

        let stale = propose(&executor, &args, invocation("stale-call"), None).await;
        assert!(stale.is_error);
        assert_eq!(
            serde_json::from_str::<Value>(&stale.output).unwrap()["error"]["code"],
            "work_criteria_context_stale"
        );
        let final_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM work_proposals WHERE owner_id = ? AND work_id = ?",
        )
        .bind(owner.as_str())
        .bind(work.as_str())
        .fetch_one(pool.get())
        .await
        .expect("final proposal count");
        assert_eq!(final_count, 1, "stale proposal leaves no residue");

        crate::server::work_test_support::cleanup_work_owner(&pool, owner.as_str()).await;
    }
}
