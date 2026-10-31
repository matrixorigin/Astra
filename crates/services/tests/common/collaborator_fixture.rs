// Shared real-owner seeding for service and runtime integration tests.
use astra_core::SharedPool;
use astra_services::runs::{
    CollaboratorAssociation, CollaboratorExecutionBoundary, CollaboratorNativeExecutionLocator,
    CollaboratorProvider, CollaboratorStageAdmission, DatabaseRunStateStore, DurableRunRecord,
    ExecutorBindingRequest, ExecutorBindingRequestKind, ExecutorStatusRequest, RunStateStore,
    ToolTransportKindRequest, WorkspaceAuthorityRequest, WorkspaceBindingRequest,
    WorkspaceBindingRequestKind, WorkspaceSourceRequest,
};
use astra_services::tool_invocation_ledger::{
    DatabaseToolInvocationLedger, ToolInvocationDispatchAdmission,
};
use astra_turn_types::{
    DEFAULT_CONVERSATION_BRANCH_ID, DurableToolReference, SessionKeyV1, ToolInvocationDecision,
    ToolInvocationFingerprint, ToolInvocationIdentity,
};
use serde_json::json;
use uuid::Uuid;

pub(crate) fn physical_workspace_id() -> String {
    astra_services::SessionExecutionBindingV1::edge_materialization_physical_identity(
        "fixture-materialization",
        "/selected-runner",
    )
}

pub(crate) fn record(user_id: &str, session_id: &str, parent: Option<&str>) -> DurableRunRecord {
    DurableRunRecord {
        opening: Default::default(),
        opening_hash: None,
        context_admission_hash: None,
        terminal_receipt_json: None,
        run_id: Uuid::new_v4().to_string(),
        user_id: user_id.into(),
        session_id: session_id.into(),
        parent_run_id: parent.map(Into::into),
        root_run_id: None,
        ancestor_path: None,
        depth: 0,
        delegation_id: None,
        agent_id: parent.map(|_| "collaborator".into()),
        retry_of: None,
        retry_scope: None,
        status: "running".into(),
        waiting_for: None,
        owner_pod_id: None,
        owner_lease_expires_at: None,
        run_generation: 1,
        last_event_idx: -1,
        current_checkpoint: None,
        error_code: None,
        error_message: None,
        total_prompt_tokens: 0,
        total_completion_tokens: 0,
        total_tool_calls: 0,
        agent_binding_id: None,
        agent_binding_name: None,
        agent_binding_schema_version: None,
        model_offering_id: None,
        resolved_model_name: None,
        runtime_profile: None,
        start_request_fingerprint: None,
        work_binding: None,
        events: vec![],
        created_at: String::new(),
        updated_at: String::new(),
    }
}

pub(crate) fn admission(anchor_run_id: &str) -> CollaboratorStageAdmission {
    let native_execution = CollaboratorNativeExecutionLocator {
        descriptor: astra_turn_types::ResolvedToolDescriptorRef::new(
            astra_turn_types::ToolIdentity::new(
                astra_turn_types::ProviderBindingRef::new("selected-runner-provider").unwrap(),
                astra_turn_types::NativeToolId::new("native_codex").unwrap(),
            ),
            "native-stage-v1",
        )
        .unwrap(),
        public_tool_name: "native_codex".into(),
        requested_model: Some("explicit-native-model".into()),
        policy_content_id: "admitted-policy-content-id".into(),
    };
    let execution_boundary = CollaboratorExecutionBoundary::UserRunner {
        runner_id: physical_workspace_id(),
    };
    CollaboratorStageAdmission {
        anchor_run_id: anchor_run_id.into(),
        source_message_id: Uuid::new_v4().to_string(),
        request_fingerprint: "immutable-source-intent-digest".into(),
        execution_identity_fingerprint: "prepared-identity-digest".into(),
        native_execution,
        expected_previous_stage_run_id: None,
        expected_parent_generation: 1,
        association: CollaboratorAssociation {
            provider: CollaboratorProvider::Codex,
            execution_boundary,
        },
    }
}

pub(crate) async fn seed_execution_binding(
    pool: &SharedPool,
    user_id: &str,
    session_id: &str,
) -> astra_services::SessionExecutionBindingV1 {
    use astra_services::{DatabaseSessionContextCoordinator, SessionExecutionBindingV1};
    let key = SessionKeyV1::owner_session(
        "server",
        user_id,
        session_id,
        DEFAULT_CONVERSATION_BRANCH_ID,
    );
    let mut binding = SessionExecutionBindingV1::server_work_default("native-test-workspace");
    binding.physical_workspace_id = Some(physical_workspace_id());
    binding.workspace = WorkspaceBindingRequest {
        kind: WorkspaceBindingRequestKind::EdgeWorkspace,
        display_name: None,
        root: Some("/selected-runner".into()),
        source: Some(WorkspaceSourceRequest::EdgePath {
            path: "/selected-runner".into(),
        }),
        authority: Some(WorkspaceAuthorityRequest::ReadWrite),
    };
    binding.executor = ExecutorBindingRequest {
        kind: ExecutorBindingRequestKind::EdgeAgent,
        executor_id: Some("selected-runner".into()),
        display_name: None,
        transport: Some(ToolTransportKindRequest::EdgeWs),
        status: Some(ExecutorStatusRequest::Online),
    };
    DatabaseSessionContextCoordinator::new(pool.clone())
        .load_or_initialize_execution_binding(&key, &binding)
        .await
        .unwrap();
    binding
}

pub(crate) async fn seed_live_edge_invocation(
    pool: &SharedPool,
    store: &DatabaseRunStateStore,
    mut child: DurableRunRecord,
    stage: CollaboratorStageAdmission,
    owner: &str,
) -> (DatabaseToolInvocationLedger, ToolInvocationIdentity) {
    let user_id = child.user_id.clone();
    let session_id = child.session_id.clone();
    use astra_services::multi_agent::{
        DatabaseEdgeDispatchService, EdgeDispatchIdentity, EdgeDispatchService,
    };
    let key = SessionKeyV1::owner_session(
        "server",
        &user_id,
        &session_id,
        DEFAULT_CONVERSATION_BRANCH_ID,
    );
    let binding = astra_services::DatabaseSessionContextCoordinator::new(pool.clone())
        .load_execution_binding(&key)
        .await
        .unwrap()
        .expect("prepare the canonical binding before creating the parent Run");
    child.events = vec![json!({"event_type":"run_started", "data":{
        "execution_binding_generation": binding.generation,
        "workspace": {"kind":"edge_workspace", "cwd":"/selected-runner", "authority":"read_write"},
        "executor": {"kind":"edge_agent", "executor_id":"selected-runner", "transport":"edge_ws"}
    }})];
    let descriptor = stage.native_execution.descriptor.clone();
    let receipt = store
        .insert_run_with_collaborator_stage(child, stage, None)
        .await
        .unwrap();
    let run = store
        .load_run(&user_id, &receipt.run_id)
        .await
        .unwrap()
        .unwrap();
    let identity = ToolInvocationIdentity::new(
        &user_id,
        &session_id,
        &receipt.run_id,
        "native-turn",
        "native-stage",
    )
    .unwrap();
    let ledger = DatabaseToolInvocationLedger::new(pool.clone());
    let decision = ToolInvocationDecision::new(&json!({
            "route":"edge_ws", "executor":{"kind":"edge_agent", "executor_id":"selected-runner", "transport":"edge_ws"}
        })).unwrap();
    let fingerprint = ToolInvocationFingerprint::new(
        DurableToolReference::Provider { descriptor },
        &json!({"task":"native interaction"}),
        &decision.decision_id,
    )
    .unwrap();
    ledger
        .prepare(&identity, &fingerprint, &decision)
        .await
        .unwrap();
    ledger
        .claim_dispatch(
            &identity,
            "native-interaction-worker",
            90_000,
            ToolInvocationDispatchAdmission {
                expected_control_epoch: run.last_event_idx,
                expected_owner_generation: run.run_generation,
                expected_owner_pod_id: owner.to_owned(),
                expected_execution_binding_generation: Some(binding.generation),
            },
        )
        .await
        .unwrap();
    let dispatch = DatabaseEdgeDispatchService::from_shared(pool);
    let dispatch_identity = EdgeDispatchIdentity::new(
        &user_id,
        &session_id,
        &identity.run_id,
        &identity.turn_chain_id,
        identity.storage_key(),
    );
    assert!(matches!(dispatch.admit_and_claim_direct_dispatch(&dispatch_identity, "selected-runner", &json!({
            "identity":identity, "request_id":identity.storage_key(), "tool":"native_codex", "args":{"task":"native interaction"}
        }).to_string()).await.unwrap(), astra_services::multi_agent::EdgeDirectDispatchAdmission::Claimed));
    (ledger, identity)
}
