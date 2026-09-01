//! Public Work read contract over the real Axum router and MatrixOne.

use std::sync::{Arc, Mutex};

use astra_core::{ErrorResponse, MatrixOneSettings, SharedPool};
use astra_runtime::{AppState, HealthChecker, ServiceInfo, build_app};
use astra_runtime_env::{
    WorkspaceAuthority, WorkspaceBindingKind, WorkspaceOwnerScope, WorkspacePersistence,
    WorkspaceRecord, WorkspaceSource,
};
use astra_server_types::WORK_API_MAJOR_HEADER;
use astra_services::{
    AcquireWriterOutcome, DatabaseSessionContextCoordinator, DatabaseSessionForkCoordinator,
    DatabaseSessionHandoffService, DatabaseWorkspaceRecordStore, ExecutionGrantSigner,
    PrepareSessionForkV1, ReserveTurnOutcome, SessionContextCoordinator, WorkspaceRecordEntry,
    WorkspaceRecordStore,
    auth::{
        AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
        AuthTokenRecord, AuthUserRecord, ReauthenticationPurpose,
    },
    ensure_core_schema,
    runs::{
        CancelRunRecord, ChatRequestData, ChatRunRecord, ChatStreamRecord, DatabaseRunStateStore,
        DurableRunRecord, DurableWorkRunBinding, ModelSelectionMode, RunLifecycleService,
        RunListCursor, RunListRecord, RunStartIdempotencyKind, RunStateStore, RunStatusRecord,
    },
    work::{
        CheckCoverage, CheckEvidenceRef, CheckOutcome, CheckRunId, CheckVerifierKind,
        CriterionCommand, CriterionDefinition, CriterionId, CriterionRevision,
        CriterionRevisionRef, CriterionSetMemberChange, CriterionSetRevision, CriterionStatement,
        DatabaseWorkBranchControlService, DatabaseWorkBranchCreationService,
        DatabaseWorkRepository, ForkCursorRef, GraphRevision, InternalSessionId, NewWorkCheckRun,
        NewWorkCriteriaProposal, NewWorkCriterion, OriginalIntentRef, WorkBranchBasisChange,
        WorkBranchCreationRequest, WorkBranchId, WorkBranchRevision, WorkBranchSubjectChange,
        WorkChangeRef, WorkContentHash, WorkCriteriaChange, WorkCriteriaProposalMember,
        WorkGenesis, WorkGenesisParts, WorkGoal, WorkId, WorkItemAttemptId, WorkItemId,
        WorkItemRevision, WorkItemRevisionRef, WorkOwnerId, WorkProposalId, WorkProposalSourceKind,
        WorkRepository, WorkRepositoryError, WorkRevision, WorkSubjectRef,
    },
};
use astra_tools::patch_materialization::observe_git_worktree_revision;
use astra_turn_types::{
    ActorContextV1, ActorKindV1, AuthorityEpochsV1, CANONICAL_TURN_DELTA_SCHEMA_VERSION,
    CanonicalDeltaModeV1, CanonicalTurnDeltaV1, CoordinatorMutationV1, SessionCursorV1,
    SessionKeyV1, SessionSurfaceV1,
};
use async_trait::async_trait;
use axum::{
    Json, Router, body,
    http::{HeaderMap, Request, StatusCode},
};
use serde_json::Value;
use sqlx::Row;
use tower::util::ServiceExt;
use uuid::Uuid;

#[derive(Clone)]
struct HealthyStub;

#[async_trait]
impl HealthChecker for HealthyStub {
    async fn database_healthy(&self) -> bool {
        true
    }
}

#[derive(Clone)]
struct BearerAuthService;

#[async_trait]
impl AuthService for BearerAuthService {
    async fn register(
        &self,
        _request: AuthRegisterRequestData,
    ) -> Result<AuthUserRecord, (StatusCode, Json<ErrorResponse>)> {
        unreachable!()
    }

    async fn login(
        &self,
        _request: AuthLoginRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, Json<ErrorResponse>)> {
        unreachable!()
    }

    async fn refresh(
        &self,
        _request: AuthRefreshRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, Json<ErrorResponse>)> {
        unreachable!()
    }

    async fn logout(
        &self,
        _request: AuthRefreshRequestData,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        unreachable!()
    }

    async fn current_user(
        &self,
        headers: &HeaderMap,
    ) -> Result<AuthUserRecord, (StatusCode, Json<ErrorResponse>)> {
        let user_id = headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .filter(|value| !value.is_empty())
            .unwrap_or("owner-test");
        Ok(AuthUserRecord {
            user_id: user_id.to_string(),
            username: user_id.to_string(),
            email: format!("{user_id}@example.test"),
            display_name: None,
        })
    }

    async fn consume_reauthentication_proof(
        &self,
        _user_id: &str,
        purpose: ReauthenticationPurpose,
        proof: &str,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        if purpose == ReauthenticationPurpose::SessionForcedTakeover && proof == "valid-step-up" {
            Ok(())
        } else {
            Err(astra_core::error_response(
                StatusCode::FORBIDDEN,
                "invalid test reauthentication",
            ))
        }
    }
}

#[derive(Default)]
struct WorkTurnRecordingLifecycle {
    requests: Mutex<Vec<(String, ChatRequestData)>>,
}

#[async_trait]
impl RunLifecycleService for WorkTurnRecordingLifecycle {
    async fn create_run(
        &self,
        _user_id: String,
        _request: ChatRequestData,
    ) -> Result<ChatRunRecord, (StatusCode, Json<ErrorResponse>)> {
        unreachable!("Work continuation uses the streaming boundary")
    }

    async fn stream_chat(
        &self,
        user_id: String,
        request: ChatRequestData,
    ) -> Result<ChatStreamRecord, (StatusCode, Json<ErrorResponse>)> {
        let session_id = request
            .session_id
            .clone()
            .expect("Work handler supplies its internal session binding");
        let run_id = request
            .run_start_idempotency
            .as_ref()
            .expect("Work handler supplies exact start identity")
            .run_id()
            .to_string();
        self.requests
            .lock()
            .expect("recording lifecycle lock")
            .push((user_id, request));
        Ok(ChatStreamRecord {
            session_id: session_id.clone(),
            run_id: run_id.clone(),
            events: vec![
                serde_json::json!({
                    "event_type": "run_started",
                    "data": {"run_id": run_id, "session_id": session_id}
                }),
                serde_json::json!({
                    "event_type": "text_delta",
                    "data": {"chunk": "working"}
                }),
                serde_json::json!({
                    "event_type": "run_finished",
                    "data": {"run_id": run_id, "status": "completed"}
                }),
            ],
            event_rx: None,
        })
    }

    async fn get_run_status(
        &self,
        _run_id: String,
        _user_id: String,
    ) -> Result<RunStatusRecord, (StatusCode, Json<ErrorResponse>)> {
        unreachable!()
    }

    async fn stream_run(
        &self,
        _run_id: String,
        _user_id: String,
        _last_index: u32,
    ) -> Result<Vec<Value>, (StatusCode, Json<ErrorResponse>)> {
        unreachable!()
    }

    async fn cancel_run(
        &self,
        _run_id: String,
        _user_id: String,
    ) -> Result<CancelRunRecord, (StatusCode, Json<ErrorResponse>)> {
        unreachable!()
    }

    async fn cancel_session_runs(
        &self,
        _session_id: String,
        _user_id: String,
    ) -> Result<Vec<CancelRunRecord>, (StatusCode, Json<ErrorResponse>)> {
        Ok(Vec::new())
    }

    async fn list_runs_cursor(
        &self,
        _user_id: String,
        _limit: u32,
        _cursor: Option<RunListCursor>,
    ) -> Result<RunListRecord, (StatusCode, Json<ErrorResponse>)> {
        unreachable!()
    }
}

static SCHEMA_SETUP_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn setup_pool() -> Option<SharedPool> {
    let _ = dotenvy::dotenv();
    if std::env::var("ASTRA_TEST_DB_IT").as_deref() != Ok("1") {
        return None;
    }
    let settings = MatrixOneSettings::from_env();
    let _setup_guard = SCHEMA_SETUP_LOCK.lock().await;
    let catalog =
        std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG").unwrap_or_else(|_| "mysql".into());
    ensure_core_schema(&settings, &catalog)
        .await
        .expect("ensure schema");
    Some(SharedPool::new(&settings).await.expect("pool"))
}

async fn setup() -> Option<(Router, SharedPool)> {
    let pool = setup_pool().await?;
    let coordinator: Arc<dyn SessionContextCoordinator> =
        Arc::new(DatabaseSessionContextCoordinator::new(pool.clone()));
    let handoff = Arc::new(DatabaseSessionHandoffService::new(
        pool.clone(),
        Arc::clone(&coordinator),
    ));
    let fork = Arc::new(DatabaseSessionForkCoordinator::new(
        pool.clone(),
        Arc::clone(&coordinator),
    ));
    let state = AppState::new(ServiceInfo::default(), Arc::new(HealthyStub))
        .with_auth_service(Arc::new(BearerAuthService))
        .with_shared_pool(pool.clone())
        .with_session_context_authority(
            coordinator,
            Arc::new(ExecutionGrantSigner::new([7_u8; 32]).expect("test signer")),
        )
        .with_session_handoff_service(handoff)
        .with_session_fork_coordinator(fork);
    Some((build_app(state), pool))
}

async fn setup_with_run_lifecycle(
    lifecycle: Arc<dyn RunLifecycleService>,
) -> Option<(Router, SharedPool)> {
    let pool = setup_pool().await?;
    let coordinator: Arc<dyn SessionContextCoordinator> =
        Arc::new(DatabaseSessionContextCoordinator::new(pool.clone()));
    let handoff = Arc::new(DatabaseSessionHandoffService::new(
        pool.clone(),
        Arc::clone(&coordinator),
    ));
    let fork = Arc::new(DatabaseSessionForkCoordinator::new(
        pool.clone(),
        Arc::clone(&coordinator),
    ));
    let state = AppState::new(ServiceInfo::default(), Arc::new(HealthyStub))
        .with_auth_service(Arc::new(BearerAuthService))
        .with_shared_pool(pool.clone())
        .with_session_context_authority(
            coordinator,
            Arc::new(ExecutionGrantSigner::new([7_u8; 32]).expect("test signer")),
        )
        .with_session_handoff_service(handoff)
        .with_session_fork_coordinator(fork)
        .with_run_lifecycle_service(lifecycle);
    Some((build_app(state), pool))
}

fn id(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4())
}

fn hash(byte: char) -> WorkContentHash {
    WorkContentHash::parse(format!("sha256:{}", byte.to_string().repeat(64))).expect("hash")
}

fn assert_field_absent(value: &Value, forbidden: &str) {
    match value {
        Value::Object(object) => {
            assert!(
                !object.contains_key(forbidden),
                "unexpected field {forbidden}"
            );
            for child in object.values() {
                assert_field_absent(child, forbidden);
            }
        }
        Value::Array(values) => {
            for child in values {
                assert_field_absent(child, forbidden);
            }
        }
        _ => {}
    }
}

async fn cleanup_owner(pool: &SharedPool, owner_id: &str) {
    for (table, owner_column) in [
        ("session_transcript_projection_heads", "user_id"),
        ("session_transcript_items", "user_id"),
        ("transcript_pages", "user_id"),
        ("session_context_operation_receipts", "owner_user_id"),
        ("session_context_authority_events", "owner_user_id"),
        ("session_fork_events", "owner_user_id"),
        ("session_forks", "owner_user_id"),
        ("conversation_manifest_segments", "owner_user_id"),
        ("conversation_manifest_pins", "owner_user_id"),
        ("conversation_manifest_nodes", "owner_user_id"),
        ("session_context_heads", "owner_user_id"),
        ("conversation_segments", "owner_user_id"),
        ("session_handoff_events", "owner_user_id"),
        ("session_handoffs", "owner_user_id"),
        ("session_attachments", "owner_user_id"),
        ("session_handoff_slots", "owner_user_id"),
        ("work_branch_creation_operations", "owner_id"),
        ("work_branch_control_operations", "owner_id"),
        ("work_branch_deletion_operations", "owner_id"),
        ("agent_session_execution_slots", "user_id"),
        ("tool_invocation_ledger", "user_id"),
        ("agent_runs", "user_id"),
        ("work_item_attempts", "owner_id"),
        ("session_artifact_references", "user_id"),
        ("session_artifacts", "user_id"),
        ("work_patch_commit_operations", "owner_id"),
        ("work_patch_materialization_operations", "owner_id"),
        ("work_patch_artifacts", "owner_id"),
        ("work_runtime_event_outbox", "owner_id"),
        ("work_runtime_event_outbox_slots", "owner_id"),
        ("work_events", "owner_id"),
        ("work_attention_receipts", "owner_id"),
        ("work_event_sequences", "owner_id"),
        ("work_current_gap_acceptances", "owner_id"),
        ("work_acceptance_decisions", "owner_id"),
        ("work_check_runs", "owner_id"),
        ("work_proposals", "owner_id"),
        ("work_proposal_sequences", "owner_id"),
        ("work_branch_subjects", "owner_id"),
        ("work_branches", "owner_id"),
        ("work_item_edges", "owner_id"),
        ("work_item_revisions", "owner_id"),
        ("work_items", "owner_id"),
        ("work_graph_revisions", "owner_id"),
        ("work_graph_sequences", "owner_id"),
        ("work_criterion_sets", "owner_id"),
        ("work_criterion_revisions", "owner_id"),
        ("work_criteria", "owner_id"),
        ("work_goal_revisions", "owner_id"),
        ("works", "owner_id"),
        ("agent_sessions", "user_id"),
        ("workspace_records", "owner_id"),
    ] {
        let statement = format!("DELETE FROM {table} WHERE {owner_column} = ?");
        sqlx::query(&statement)
            .bind(owner_id)
            .execute(pool.get())
            .await
            .unwrap_or_else(|error| panic!("clean {table}: {error}"));
    }
}

async fn post_work_patch_export(
    app: Router,
    user_id: &str,
    work_id: &str,
    branch_id: &str,
    payload: Value,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri(format!(
            "/v1/works/{work_id}/branches/{branch_id}/patch-artifacts"
        ))
        .header("authorization", format!("Bearer {user_id}"))
        .header(WORK_API_MAJOR_HEADER, "1")
        .header("content-type", "application/json")
        .body(body::Body::from(
            serde_json::to_vec(&payload).expect("patch export request JSON"),
        ))
        .expect("request");
    let response = app.oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("patch export response is bounded");
    (
        status,
        serde_json::from_slice(&bytes).expect("JSON response"),
    )
}

async fn get_work_patch_content(
    app: Router,
    user_id: &str,
    work_id: &str,
    branch_id: &str,
    patch_artifact_id: &str,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let request = Request::builder()
        .uri(format!(
            "/v1/works/{work_id}/branches/{branch_id}/patch-artifacts/{patch_artifact_id}/content"
        ))
        .header("authorization", format!("Bearer {user_id}"))
        .header(WORK_API_MAJOR_HEADER, "1")
        .body(body::Body::empty())
        .expect("patch content request");
    let response = app.oneshot(request).await.expect("response");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = body::to_bytes(response.into_body(), 16 * 1024 * 1024 + 1)
        .await
        .expect("patch content response respects its hard limit");
    (status, headers, bytes.to_vec())
}

async fn get_work_patch_artifacts(
    app: Router,
    user_id: &str,
    work_id: &str,
    branch_id: &str,
    query: &str,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .uri(format!(
            "/v1/works/{work_id}/branches/{branch_id}/patch-artifacts{query}"
        ))
        .header("authorization", format!("Bearer {user_id}"))
        .header(WORK_API_MAJOR_HEADER, "1")
        .body(body::Body::empty())
        .expect("patch artifact page request");
    let response = app.oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("patch artifact metadata page is bounded");
    (
        status,
        serde_json::from_slice(&bytes).expect("patch artifact page JSON"),
    )
}

async fn get_work_patch_materializations(
    app: Router,
    user_id: &str,
    work_id: &str,
    branch_id: &str,
    query: &str,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .uri(format!(
            "/v1/works/{work_id}/branches/{branch_id}/patch-materializations{query}"
        ))
        .header("authorization", format!("Bearer {user_id}"))
        .header(WORK_API_MAJOR_HEADER, "1")
        .body(body::Body::empty())
        .expect("patch materialization page request");
    let response = app.oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), 128 * 1024)
        .await
        .expect("patch materialization page is bounded");
    (
        status,
        serde_json::from_slice(&bytes).expect("patch materialization page JSON"),
    )
}

async fn post_work_patch_commit(
    app: Router,
    user_id: &str,
    work_id: &str,
    branch_id: &str,
    payload: Value,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri(format!(
            "/v1/works/{work_id}/branches/{branch_id}/patch-commits"
        ))
        .header("authorization", format!("Bearer {user_id}"))
        .header(WORK_API_MAJOR_HEADER, "1")
        .header("content-type", "application/json")
        .body(body::Body::from(
            serde_json::to_vec(&payload).expect("patch commit request JSON"),
        ))
        .expect("patch commit request");
    let response = app.oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("patch commit response is bounded");
    (
        status,
        serde_json::from_slice(&bytes).expect("patch commit JSON"),
    )
}

async fn get_work_patch_commits(
    app: Router,
    user_id: &str,
    work_id: &str,
    branch_id: &str,
    suffix: &str,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .uri(format!(
            "/v1/works/{work_id}/branches/{branch_id}/patch-commits{suffix}"
        ))
        .header("authorization", format!("Bearer {user_id}"))
        .header(WORK_API_MAJOR_HEADER, "1")
        .body(body::Body::empty())
        .expect("patch commit read request");
    let response = app.oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), 128 * 1024)
        .await
        .expect("patch commit read response is bounded");
    (
        status,
        serde_json::from_slice(&bytes).expect("patch commit read JSON"),
    )
}

async fn delete_work_patch_commit(
    app: Router,
    user_id: &str,
    work_id: &str,
    branch_id: &str,
    operation_id: &str,
) -> StatusCode {
    let request = Request::builder()
        .method("DELETE")
        .uri(format!(
            "/v1/works/{work_id}/branches/{branch_id}/patch-commits/{operation_id}"
        ))
        .header("authorization", format!("Bearer {user_id}"))
        .header(WORK_API_MAJOR_HEADER, "1")
        .body(body::Body::empty())
        .expect("abort patch commit request");
    app.oneshot(request).await.expect("response").status()
}

async fn get_work(app: Router, user_id: &str, work_id: &str) -> (StatusCode, Value, usize) {
    let request = Request::builder()
        .uri(format!("/v1/works/{work_id}"))
        .header("authorization", format!("Bearer {user_id}"))
        .header(WORK_API_MAJOR_HEADER, "1")
        .body(body::Body::empty())
        .expect("request");
    let response = app.oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("Work response must stay below the shell limit");
    let length = bytes.len();
    let value = serde_json::from_slice(&bytes).expect("JSON response");
    (status, value, length)
}

async fn get_work_branches(
    app: Router,
    user_id: &str,
    work_id: &str,
) -> (StatusCode, Value, usize) {
    let request = Request::builder()
        .uri(format!("/v1/works/{work_id}/branches"))
        .header("authorization", format!("Bearer {user_id}"))
        .header(WORK_API_MAJOR_HEADER, "1")
        .body(body::Body::empty())
        .expect("request");
    let response = app.oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("branch catalog must stay bounded");
    let length = bytes.len();
    let value = serde_json::from_slice(&bytes).expect("JSON branch catalog");
    (status, value, length)
}

async fn get_archived_work_branches(
    app: Router,
    user_id: &str,
    work_id: &str,
    query: &str,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .uri(format!("/v1/works/{work_id}/branches/archived{query}"))
        .header("authorization", format!("Bearer {user_id}"))
        .header(WORK_API_MAJOR_HEADER, "1")
        .body(body::Body::empty())
        .expect("request");
    let response = app.oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("archived branch page must stay bounded");
    let value = serde_json::from_slice(&bytes).expect("JSON archived branch page");
    (status, value)
}

async fn post_work_branch_deletion(
    app: Router,
    user_id: &str,
    work_id: &str,
    branch_id: &str,
    payload: Value,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri(format!(
            "/v1/works/{work_id}/branches/{branch_id}/deletion-operations"
        ))
        .header("authorization", format!("Bearer {user_id}"))
        .header(WORK_API_MAJOR_HEADER, "1")
        .header("content-type", "application/json")
        .body(body::Body::from(payload.to_string()))
        .expect("request");
    let response = app.oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("bounded branch deletion response");
    (
        status,
        serde_json::from_slice(&bytes).expect("JSON deletion response"),
    )
}

async fn get_work_branch_deletion(
    app: Router,
    user_id: &str,
    work_id: &str,
    branch_id: &str,
    operation_id: &str,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .uri(format!(
            "/v1/works/{work_id}/branches/{branch_id}/deletion-operations/{operation_id}"
        ))
        .header("authorization", format!("Bearer {user_id}"))
        .header(WORK_API_MAJOR_HEADER, "1")
        .body(body::Body::empty())
        .expect("request");
    let response = app.oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("bounded branch deletion response");
    (
        status,
        serde_json::from_slice(&bytes).expect("JSON deletion response"),
    )
}

async fn post_work_branch_comparison(
    app: Router,
    user_id: &str,
    work_id: &str,
    payload: Value,
) -> (StatusCode, Value, usize) {
    let request = Request::builder()
        .method("POST")
        .uri(format!("/v1/works/{work_id}/branch-comparisons"))
        .header("authorization", format!("Bearer {user_id}"))
        .header(WORK_API_MAJOR_HEADER, "1")
        .header("content-type", "application/json")
        .body(body::Body::from(payload.to_string()))
        .expect("request");
    let response = app.oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("branch comparison must stay bounded");
    let length = bytes.len();
    let value = serde_json::from_slice(&bytes).expect("JSON branch comparison");
    (status, value, length)
}

async fn post_work_action(
    app: Router,
    user_id: &str,
    work_id: &str,
    payload: Value,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri(format!("/v1/works/{work_id}/actions"))
        .header("authorization", format!("Bearer {user_id}"))
        .header(WORK_API_MAJOR_HEADER, "1")
        .header("content-type", "application/json")
        .body(body::Body::from(payload.to_string()))
        .expect("request");
    let response = app.oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("Work action response must stay bounded");
    let value = serde_json::from_slice(&bytes).expect("JSON Work action response");
    (status, value)
}

async fn post_work_branch_action(
    app: Router,
    user_id: &str,
    work_id: &str,
    branch_id: &str,
    payload: Value,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri(format!("/v1/works/{work_id}/branches/{branch_id}/actions"))
        .header("authorization", format!("Bearer {user_id}"))
        .header(WORK_API_MAJOR_HEADER, "1")
        .header("content-type", "application/json")
        .body(body::Body::from(payload.to_string()))
        .expect("request");
    let response = app.oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("Work branch action response must stay bounded");
    let value = serde_json::from_slice(&bytes).expect("JSON Work branch action response");
    (status, value)
}

async fn get_works(app: Router, user_id: &str, query: &str) -> (StatusCode, Value, usize) {
    let request = Request::builder()
        .uri(format!("/v1/works{query}"))
        .header("authorization", format!("Bearer {user_id}"))
        .header(WORK_API_MAJOR_HEADER, "1")
        .body(body::Body::empty())
        .expect("request");
    let response = app.oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), 512 * 1024)
        .await
        .expect("Work catalog response is bounded");
    let length = bytes.len();
    let value = serde_json::from_slice(&bytes).expect("JSON response");
    (status, value, length)
}

async fn attach_work_branch(
    app: Router,
    user_id: &str,
    work_id: &str,
    branch_id: &str,
    request_id: &str,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri(format!(
            "/v1/works/{work_id}/branches/{branch_id}/attachments"
        ))
        .header("authorization", format!("Bearer {user_id}"))
        .header(WORK_API_MAJOR_HEADER, "1")
        .header("content-type", "application/json")
        .body(body::Body::from(
            serde_json::json!({"request_id": request_id}).to_string(),
        ))
        .expect("request");
    let response = app.oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("bounded attachment response");
    let value = serde_json::from_slice(&bytes).expect("JSON response");
    (status, value)
}

async fn get_work_transcript(
    app: Router,
    user_id: &str,
    work_id: &str,
    branch_id: &str,
    query: &str,
) -> (StatusCode, Value, String) {
    let request = Request::builder()
        .uri(format!(
            "/v1/works/{work_id}/branches/{branch_id}/transcript{query}"
        ))
        .header("authorization", format!("Bearer {user_id}"))
        .header(WORK_API_MAJOR_HEADER, "1")
        .body(body::Body::empty())
        .expect("request");
    let response = app.oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), 512 * 1024)
        .await
        .expect("bounded transcript page");
    let raw = String::from_utf8(bytes.to_vec()).expect("UTF-8 transcript response");
    let value = serde_json::from_str(&raw).expect("JSON transcript response");
    (status, value, raw)
}

async fn post_work_control(
    app: Router,
    user_id: &str,
    work_id: &str,
    branch_id: &str,
    body_json: Value,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri(format!(
            "/v1/works/{work_id}/branches/{branch_id}/control-operations"
        ))
        .header("authorization", format!("Bearer {user_id}"))
        .header(WORK_API_MAJOR_HEADER, "1")
        .header("content-type", "application/json")
        .body(body::Body::from(body_json.to_string()))
        .expect("request");
    let response = app.oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("bounded control response");
    let value = serde_json::from_slice(&bytes).expect("JSON control response");
    (status, value)
}

async fn post_work_fork(
    app: Router,
    user_id: &str,
    work_id: &str,
    branch_id: &str,
    body_json: Value,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri(format!("/v1/works/{work_id}/branches/{branch_id}/forks"))
        .header("authorization", format!("Bearer {user_id}"))
        .header(WORK_API_MAJOR_HEADER, "1")
        .header("content-type", "application/json")
        .body(body::Body::from(body_json.to_string()))
        .expect("request");
    let response = app.oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("bounded fork response");
    let value = serde_json::from_slice(&bytes).expect("JSON fork response");
    (status, value)
}

async fn get_work_fork(
    app: Router,
    user_id: &str,
    work_id: &str,
    branch_id: &str,
    operation_id: &str,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .uri(format!(
            "/v1/works/{work_id}/branches/{branch_id}/forks/{operation_id}"
        ))
        .header("authorization", format!("Bearer {user_id}"))
        .header(WORK_API_MAJOR_HEADER, "1")
        .body(body::Body::empty())
        .expect("request");
    let response = app.oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("bounded fork response");
    let value = serde_json::from_slice(&bytes).expect("JSON fork response");
    (status, value)
}

async fn delete_work_fork(
    app: Router,
    user_id: &str,
    work_id: &str,
    branch_id: &str,
    operation_id: &str,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("DELETE")
        .uri(format!(
            "/v1/works/{work_id}/branches/{branch_id}/forks/{operation_id}"
        ))
        .header("authorization", format!("Bearer {user_id}"))
        .header(WORK_API_MAJOR_HEADER, "1")
        .body(body::Body::empty())
        .expect("request");
    let response = app.oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("bounded fork response");
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("JSON fork response")
    };
    (status, value)
}

fn public_work_cursor(cursor: &SessionCursorV1) -> Value {
    serde_json::json!({
        "completed_turn": cursor.completed_turn,
        "journal_event_seq": cursor.journal_event_seq,
        "conversation_seq": cursor.conversation_seq,
        "canonical_root_hash": cursor.canonical_root_hash,
        "projection_schema": cursor.projection_schema,
        "compaction_generation": cursor.compaction_generation,
        "config_version_id": cursor.config_version_id,
    })
}

async fn get_work_control(
    app: Router,
    user_id: &str,
    work_id: &str,
    branch_id: &str,
    operation_id: &str,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .uri(format!(
            "/v1/works/{work_id}/branches/{branch_id}/control-operations/{operation_id}"
        ))
        .header("authorization", format!("Bearer {user_id}"))
        .header(WORK_API_MAJOR_HEADER, "1")
        .body(body::Body::empty())
        .expect("request");
    let response = app.oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("bounded control response");
    let value = serde_json::from_slice(&bytes).expect("JSON control response");
    (status, value)
}

async fn wait_work_control_terminal(
    app: Router,
    user_id: &str,
    work_id: &str,
    branch_id: &str,
    operation_id: &str,
) -> Value {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let (status, operation) =
                get_work_control(app.clone(), user_id, work_id, branch_id, operation_id).await;
            assert_eq!(status, StatusCode::OK, "{operation}");
            if operation["state"] != "pending" {
                return operation;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("control operation must reach a durable terminal state")
}

async fn delete_work_control(
    app: Router,
    user_id: &str,
    work_id: &str,
    branch_id: &str,
    operation_id: &str,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("DELETE")
        .uri(format!(
            "/v1/works/{work_id}/branches/{branch_id}/control-operations/{operation_id}"
        ))
        .header("authorization", format!("Bearer {user_id}"))
        .header(WORK_API_MAJOR_HEADER, "1")
        .body(body::Body::empty())
        .expect("request");
    let response = app.oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("bounded control response");
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("JSON control response")
    };
    (status, value)
}

async fn commit_test_conversation_turn(
    pool: &SharedPool,
    owner_id: &str,
    session_id: &str,
    expected: Option<&SessionCursorV1>,
    turn: u32,
) -> SessionCursorV1 {
    let coordinator = DatabaseSessionContextCoordinator::new(pool.clone());
    let key = SessionKeyV1::owner_session("server", owner_id, session_id, "main");
    let actor = ActorContextV1::owner_user(
        owner_id,
        "work-transcript-it",
        ActorKindV1::Server,
        SessionSurfaceV1::Server,
        None,
        AuthorityEpochsV1::default(),
    );
    let lease = match coordinator
        .acquire_writer(
            &key,
            expected,
            &actor,
            std::time::Duration::from_secs(60),
            &format!("work-transcript-lease-{turn}"),
        )
        .await
        .expect("acquire transcript writer")
    {
        AcquireWriterOutcome::Acquired(lease) | AcquireWriterOutcome::AlreadyAcquired(lease) => {
            lease
        }
        AcquireWriterOutcome::Conflict { .. } => panic!("unexpected transcript writer conflict"),
    };
    let reservation = match coordinator
        .reserve_turn(
            &lease,
            expected,
            std::time::Duration::from_secs(60),
            &format!("work-transcript-turn-{turn}"),
        )
        .await
        .expect("reserve transcript turn")
    {
        ReserveTurnOutcome::Reserved(reservation)
        | ReserveTurnOutcome::AlreadyReserved(reservation) => reservation,
        ReserveTurnOutcome::Conflict { .. } => panic!("unexpected transcript reservation conflict"),
    };
    let outcome = coordinator
        .commit_turn(
            &reservation,
            CanonicalTurnDeltaV1 {
                schema_version: CANONICAL_TURN_DELTA_SCHEMA_VERSION,
                completed_turn: turn,
                journal_event_seq: u64::from(turn),
                conversation_seq: u64::from(turn),
                compaction_generation: 0,
                config_version_id: None,
                mode: CanonicalDeltaModeV1::Append,
                logical_segments: vec![vec![serde_json::json!({
                    "role": "user",
                    "content": format!("turn {turn}")
                })]],
            },
            &format!("work-transcript-commit-{turn}"),
        )
        .await
        .expect("commit transcript turn");
    let cursor = match outcome {
        CoordinatorMutationV1::Applied { cursor }
        | CoordinatorMutationV1::AlreadyApplied { cursor } => cursor,
        other => panic!("unexpected transcript commit outcome: {other:?}"),
    };
    coordinator
        .release_writer(&lease)
        .await
        .expect("release transcript writer");
    cursor
}

async fn detach_work_branch(
    app: Router,
    user_id: &str,
    work_id: &str,
    branch_id: &str,
    attachment_id: &str,
) -> StatusCode {
    let request = Request::builder()
        .method("DELETE")
        .uri(format!(
            "/v1/works/{work_id}/branches/{branch_id}/attachments/{attachment_id}"
        ))
        .header("authorization", format!("Bearer {user_id}"))
        .header(WORK_API_MAJOR_HEADER, "1")
        .body(body::Body::empty())
        .expect("request");
    app.oneshot(request).await.expect("response").status()
}

async fn post_work(app: Router, user_id: &str, body_json: Value) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri("/v1/works")
        .header("authorization", format!("Bearer {user_id}"))
        .header(WORK_API_MAJOR_HEADER, "1")
        .header("content-type", "application/json")
        .body(body::Body::from(
            serde_json::to_vec(&body_json).expect("request JSON"),
        ))
        .expect("request");
    let response = app.oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("Start Work response is bounded");
    let value = serde_json::from_slice(&bytes).expect("JSON response");
    (status, value)
}

async fn get_work_criteria(
    app: Router,
    user_id: &str,
    work_id: &str,
    query: &str,
) -> (StatusCode, Value, usize) {
    let request = Request::builder()
        .uri(format!("/v1/works/{work_id}/criteria{query}"))
        .header("authorization", format!("Bearer {user_id}"))
        .header(WORK_API_MAJOR_HEADER, "1")
        .body(body::Body::empty())
        .expect("request");
    let response = app.oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("Work criteria page is bounded");
    let length = bytes.len();
    let value = serde_json::from_slice(&bytes).expect("JSON response");
    (status, value, length)
}

async fn put_read_cursor(
    app: Router,
    user_id: &str,
    work_id: &str,
    body_json: Value,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("PUT")
        .uri(format!("/v1/works/{work_id}/read-cursor"))
        .header("authorization", format!("Bearer {user_id}"))
        .header(WORK_API_MAJOR_HEADER, "1")
        .header("content-type", "application/json")
        .body(body::Body::from(
            serde_json::to_vec(&body_json).expect("request JSON"),
        ))
        .expect("request");
    let response = app.oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), 8 * 1024)
        .await
        .expect("read-cursor response is bounded");
    let value = serde_json::from_slice(&bytes).expect("JSON response");
    (status, value)
}

async fn get_work_events(
    app: Router,
    user_id: &str,
    work_id: &str,
    query: &str,
) -> (StatusCode, Value, usize) {
    let request = Request::builder()
        .uri(format!("/v1/works/{work_id}/events{query}"))
        .header("authorization", format!("Bearer {user_id}"))
        .header(WORK_API_MAJOR_HEADER, "1")
        .body(body::Body::empty())
        .expect("request");
    let response = app.oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("event page response is bounded");
    let length = bytes.len();
    let value = serde_json::from_slice(&bytes).expect("JSON response");
    (status, value, length)
}

async fn post_work_turn(
    app: Router,
    user_id: &str,
    work_id: &str,
    branch_id: &str,
    body_json: Value,
) -> (StatusCode, Vec<Value>, String) {
    let request = Request::builder()
        .method("POST")
        .uri(format!("/v1/works/{work_id}/branches/{branch_id}/turns"))
        .header("authorization", format!("Bearer {user_id}"))
        .header(WORK_API_MAJOR_HEADER, "1")
        .header("content-type", "application/json")
        .body(body::Body::from(
            serde_json::to_vec(&body_json).expect("request JSON"),
        ))
        .expect("request");
    let response = app.oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("Work turn response is bounded in the recording seam");
    let raw = String::from_utf8(bytes.to_vec()).expect("SSE is UTF-8");
    let events = raw
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|json| serde_json::from_str(json).expect("SSE data JSON"))
        .collect();
    (status, events, raw)
}

async fn get_work_task_graph(
    app: Router,
    user_id: &str,
    work_id: &str,
    branch_id: &str,
    query: &str,
) -> (StatusCode, Value, usize) {
    let request = Request::builder()
        .uri(format!(
            "/v1/works/{work_id}/branches/{branch_id}/task-graph{query}"
        ))
        .header("authorization", format!("Bearer {user_id}"))
        .header(WORK_API_MAJOR_HEADER, "1")
        .body(body::Body::empty())
        .expect("request");
    let response = app.oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), 256 * 1024)
        .await
        .expect("Task Graph page is bounded");
    let length = bytes.len();
    let value = serde_json::from_slice(&bytes).expect("JSON response");
    (status, value, length)
}

async fn get_work_session_binding(
    app: Router,
    user_id: &str,
    session_id: &str,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .uri(format!("/v1/works/session-bindings/{session_id}"))
        .header("authorization", format!("Bearer {user_id}"))
        .header(WORK_API_MAJOR_HEADER, "1")
        .body(body::Body::empty())
        .expect("request");
    let response = app.oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), 16 * 1024)
        .await
        .expect("session Work binding is constant-size");
    let value = serde_json::from_slice(&bytes).expect("JSON response");
    (status, value)
}

async fn post_work_session_binding(
    app: Router,
    user_id: &str,
    session_id: &str,
    body_json: Value,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri(format!("/v1/works/session-bindings/{session_id}"))
        .header("authorization", format!("Bearer {user_id}"))
        .header(WORK_API_MAJOR_HEADER, "1")
        .header("content-type", "application/json")
        .body(body::Body::from(
            serde_json::to_vec(&body_json).expect("request JSON"),
        ))
        .expect("request");
    let response = app.oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("session promotion response is bounded");
    let value = serde_json::from_slice(&bytes).expect("JSON response");
    (status, value)
}

async fn criteria_proposal_request(
    app: Router,
    user_id: &str,
    method: &str,
    work_id: &str,
    branch_id: &str,
    proposal_id: Option<&str>,
    decision: Option<Value>,
) -> (StatusCode, Value, usize) {
    let suffix = proposal_id
        .map(|proposal_id| format!("/{proposal_id}"))
        .unwrap_or_default();
    let suffix = if decision.is_some() {
        format!("{suffix}/decision")
    } else {
        suffix
    };
    let mut request = Request::builder()
        .method(method)
        .uri(format!(
            "/v1/works/{work_id}/branches/{branch_id}/criteria-proposals{suffix}"
        ))
        .header("authorization", format!("Bearer {user_id}"))
        .header(WORK_API_MAJOR_HEADER, "1");
    let body = if let Some(decision) = decision {
        request = request.header("content-type", "application/json");
        body::Body::from(serde_json::to_vec(&decision).expect("decision JSON"))
    } else {
        body::Body::empty()
    };
    let response = app
        .oneshot(request.body(body).expect("request"))
        .await
        .expect("response");
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), 3 * 1024 * 1024)
        .await
        .expect("criteria proposal response is bounded");
    let length = bytes.len();
    let value = serde_json::from_slice(&bytes).expect("JSON response");
    (status, value, length)
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn post_work_is_atomic_owner_scoped_and_exactly_idempotent_under_race() {
    let Some((app, pool)) = setup().await else {
        return;
    };
    let owner_id = id("owner");
    let other_owner_id = id("other-owner");
    cleanup_owner(&pool, &owner_id).await;
    cleanup_owner(&pool, &other_owner_id).await;
    let request = serde_json::json!({
        "request_id": "start-work-race",
        "goal": "Ship one atomic public Work creation boundary.",
        "criteria": [
            {
                "criterion_id": "tests-pass",
                "kind": "test_check",
                "statement": "Relevant tests pass.",
                "command": "cargo test -p astra-runtime work_handlers"
            },
            {
                "criterion_id": "review-complete",
                "kind": "human_review",
                "statement": "The user accepts the reviewable result."
            }
        ]
    });

    let (left, right) = tokio::join!(
        post_work(app.clone(), &owner_id, request.clone()),
        post_work(app.clone(), &owner_id, request.clone())
    );
    assert_eq!(left.0, StatusCode::CREATED, "left: {}", left.1);
    assert_eq!(right.0, StatusCode::CREATED, "right: {}", right.1);
    assert_eq!(left.1, right.1, "exact retries must return one Work");
    let work_id = left.1["overview"]["work_id"]
        .as_str()
        .expect("public Work id");
    assert_eq!(left.1["overview"]["graph"]["item_count"], 1);
    assert_eq!(left.1["overview"]["graph"]["edge_count"], 0);
    assert_eq!(
        left.1["overview"]["delivery"]["status"],
        "subject_unavailable"
    );
    assert_eq!(left.1["overview"]["criteria"]["member_count"], 2);
    assert_eq!(left.1["overview"]["event_head"], 1);
    assert_field_absent(&left.1, "session_id");
    let work_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM works WHERE owner_id = ? AND work_id = ?")
            .bind(&owner_id)
            .bind(work_id)
            .fetch_one(pool.get())
            .await
            .expect("Work count");
    let session_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM agent_sessions WHERE user_id = ?")
            .bind(&owner_id)
            .fetch_one(pool.get())
            .await
            .expect("session count");
    assert_eq!(work_count, 1);
    assert_eq!(session_count, 1);
    let criterion_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM work_criterion_revisions WHERE owner_id = ? AND work_id = ?",
    )
    .bind(&owner_id)
    .bind(work_id)
    .fetch_one(pool.get())
    .await
    .expect("criterion revision count");
    assert_eq!(criterion_count, 2);
    let initial_set = sqlx::query(
        "SELECT member_count, accepted_by_kind, accepted_by_id, member_manifest_json
         FROM work_criterion_sets
         WHERE owner_id = ? AND work_id = ? AND revision = 1",
    )
    .bind(&owner_id)
    .bind(work_id)
    .fetch_one(pool.get())
    .await
    .expect("initial criterion set");
    assert_eq!(initial_set.try_get::<i64, _>("member_count").unwrap(), 2);
    assert_eq!(
        initial_set
            .try_get::<String, _>("accepted_by_kind")
            .unwrap(),
        "user"
    );
    assert_eq!(
        initial_set.try_get::<String, _>("accepted_by_id").unwrap(),
        owner_id
    );
    let member_manifest: Value = serde_json::from_str(
        &initial_set
            .try_get::<String, _>("member_manifest_json")
            .unwrap(),
    )
    .expect("criterion manifest");
    assert_eq!(
        member_manifest["members"],
        serde_json::json!([
            {"criterion_id": "review-complete", "revision": 1},
            {"criterion_id": "tests-pass", "revision": 1}
        ])
    );

    let mut reordered = request.clone();
    reordered["criteria"]
        .as_array_mut()
        .expect("criteria array")
        .reverse();
    let (reordered_status, reordered_body) = post_work(app.clone(), &owner_id, reordered).await;
    assert_eq!(reordered_status, StatusCode::CREATED);
    assert_eq!(reordered_body, left.1, "criterion order is not semantic");

    let (mismatch_status, mismatch) = post_work(
        app.clone(),
        &owner_id,
        serde_json::json!({
            "request_id": "start-work-race",
            "goal": "Ship one atomic public Work creation boundary.",
            "criteria": [{
                "criterion_id": "tests-pass",
                "kind": "test_check",
                "statement": "A different verification contract.",
                "command": "cargo test -p astra-runtime work_handlers"
            }]
        }),
    )
    .await;
    assert_eq!(mismatch_status, StatusCode::CONFLICT);
    assert_eq!(mismatch["code"], "idempotency_mismatch");

    let (other_status, other) = post_work(app.clone(), &other_owner_id, request).await;
    assert_eq!(other_status, StatusCode::CREATED, "other owner: {other}");
    assert_ne!(other["overview"]["work_id"], work_id);

    let (invalid_status, invalid) = post_work(
        app,
        &owner_id,
        serde_json::json!({
            "request_id": "another-request",
            "goal": "Do not accept client-owned continuity.",
            "criteria": [],
            "session_id": "client-session"
        }),
    )
    .await;
    assert_eq!(invalid_status, StatusCode::BAD_REQUEST);
    assert_eq!(invalid["code"], "invalid_work_create_request");

    cleanup_owner(&pool, &owner_id).await;
    cleanup_owner(&pool, &other_owner_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn patch_export_is_server_owned_exact_and_idempotent() {
    let Some((app, pool)) = setup().await else {
        return;
    };
    let owner_id = id("patch-export-owner");
    let other_owner_id = id("patch-export-other");
    cleanup_owner(&pool, &owner_id).await;
    cleanup_owner(&pool, &other_owner_id).await;
    let (status, created) = post_work(
        app.clone(),
        &owner_id,
        serde_json::json!({
            "request_id": "start-patch-export-work",
            "goal": "Export one exact reviewable patch.",
            "criteria": [{
                "criterion_id": "patch-review",
                "kind": "human_review",
                "statement": "The exported patch is reviewable."
            }]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create Work: {created}");
    let work_id = created["overview"]["work_id"].as_str().expect("work id");
    let branch_id = created["overview"]["delivery_branch"]["branch_id"]
        .as_str()
        .expect("delivery branch");
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner = WorkOwnerId::parse(&owner_id).expect("owner");
    let work = WorkId::parse(work_id).expect("work");
    let branch = WorkBranchId::parse(branch_id).expect("branch");
    let binding = repository
        .load_branch_runtime_binding(&owner, &work, &branch)
        .await
        .expect("branch runtime binding");

    let workspace = tempfile::tempdir().expect("Git workspace");
    let git = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(workspace.path())
            .args(args)
            .env("GIT_AUTHOR_NAME", "Astra Test")
            .env("GIT_AUTHOR_EMAIL", "astra@example.invalid")
            .env("GIT_COMMITTER_NAME", "Astra Test")
            .env("GIT_COMMITTER_EMAIL", "astra@example.invalid")
            .status()
            .expect("Git fixture command");
        assert!(status.success(), "Git fixture command failed: {args:?}");
    };
    git(&["init", "--quiet"]);
    std::fs::write(workspace.path().join("file.txt"), "before\n").expect("seed file");
    git(&["add", "file.txt"]);
    git(&["commit", "--quiet", "-m", "initial"]);
    std::fs::write(workspace.path().join("file.txt"), "after\n").expect("source change");
    let result_revision = observe_git_worktree_revision(workspace.path())
        .await
        .expect("observe source result");
    let subject_ref = WorkSubjectRef::parse(format!("workspace/{}", binding.session_id.as_str()))
        .expect("subject ref");
    let subject = repository
        .set_branch_subject(WorkBranchSubjectChange {
            owner_id: owner.clone(),
            work_id: work.clone(),
            branch_id: branch.clone(),
            expected_branch_revision: binding.branch_revision,
            graph_revision: binding.graph_revision,
            subject_ref: subject_ref.clone(),
            subject_revision: result_revision.clone(),
            source_ref: WorkChangeRef::parse(id("patch-subject")).expect("subject source"),
        })
        .await
        .expect("record exact branch subject");
    DatabaseWorkspaceRecordStore::new(pool.clone())
        .upsert_workspace_record(WorkspaceRecordEntry::new(
            owner_id.clone(),
            Some(binding.session_id.as_str().to_string()),
            None,
            WorkspaceRecord {
                workspace_id: binding.session_id.as_str().to_string(),
                owner_scope: WorkspaceOwnerScope::Tenant,
                kind: WorkspaceBindingKind::ServerSandbox,
                authority: WorkspaceAuthority::ReadWrite,
                root_or_volume_ref: workspace.path().display().to_string(),
                source: WorkspaceSource::Scratch,
                persistence: WorkspacePersistence::Session,
                revision: "workspace-revision-1".into(),
                display_name: "Patch export fixture".into(),
            },
        ))
        .await
        .expect("persist Server-owned workspace");
    let export_request = serde_json::json!({
        "request_id": "export-patch-once",
        "expected_branch_revision": subject.branch_revision.get(),
        "expected_graph_revision": subject.graph_revision.get(),
    });
    let first = post_work_patch_export(
        app.clone(),
        &owner_id,
        work_id,
        branch_id,
        export_request.clone(),
    )
    .await;
    assert_eq!(first.0, StatusCode::OK, "export patch: {}", first.1);
    assert_eq!(first.1["source_ref"], "export-patch-once");
    assert_eq!(first.1["result_subject_revision"], result_revision.as_str());
    assert_field_absent(&first.1, "session_id");
    assert_field_absent(&first.1, "payload_artifact_id");
    assert_field_absent(&first.1, "source_subject_record_revision");
    assert_field_absent(&first.1, "subject_ref");
    let patch_artifact_id = first.1["patch_artifact_id"]
        .as_str()
        .expect("patch artifact id");
    let (content_status, content_headers, content) = get_work_patch_content(
        app.clone(),
        &owner_id,
        work_id,
        branch_id,
        patch_artifact_id,
    )
    .await;
    assert_eq!(content_status, StatusCode::OK);
    assert_eq!(
        content_headers
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/x-diff; charset=utf-8")
    );
    assert_eq!(
        content_headers
            .get("etag")
            .and_then(|value| value.to_str().ok()),
        Some(format!("\"{}\"", first.1["payload_hash"].as_str().unwrap()).as_str())
    );
    assert_eq!(
        content.len() as u64,
        first.1["payload_bytes"].as_u64().unwrap()
    );
    let patch_text = std::str::from_utf8(&content).expect("UTF-8 patch content");
    assert!(patch_text.contains("-before\n"));
    assert!(patch_text.contains("+after\n"));

    let hidden_content = get_work_patch_content(
        app.clone(),
        &other_owner_id,
        work_id,
        branch_id,
        patch_artifact_id,
    )
    .await;
    assert_eq!(hidden_content.0, StatusCode::NOT_FOUND);
    let wrong_branch_content = get_work_patch_content(
        app.clone(),
        &owner_id,
        work_id,
        "another-branch",
        patch_artifact_id,
    )
    .await;
    assert_eq!(wrong_branch_content.0, StatusCode::NOT_FOUND);
    let replay = post_work_patch_export(
        app.clone(),
        &owner_id,
        work_id,
        branch_id,
        export_request.clone(),
    )
    .await;
    assert_eq!(replay, first, "same request and basis must converge");

    std::fs::write(workspace.path().join("file.txt"), "second\n")
        .expect("advance workspace for a second exact export");
    let second_result_revision = observe_git_worktree_revision(workspace.path())
        .await
        .expect("observe second source result");
    let second_subject = repository
        .set_branch_subject(WorkBranchSubjectChange {
            owner_id: owner.clone(),
            work_id: work.clone(),
            branch_id: branch.clone(),
            expected_branch_revision: subject.branch_revision,
            graph_revision: subject.graph_revision,
            subject_ref: subject_ref.clone(),
            subject_revision: second_result_revision.clone(),
            source_ref: WorkChangeRef::parse(id("second-patch-subject"))
                .expect("second subject source"),
        })
        .await
        .expect("record second exact branch subject");
    let second_export_request = serde_json::json!({
        "request_id": "export-second-patch",
        "expected_branch_revision": second_subject.branch_revision.get(),
        "expected_graph_revision": second_subject.graph_revision.get(),
    });
    let second = post_work_patch_export(
        app.clone(),
        &owner_id,
        work_id,
        branch_id,
        second_export_request.clone(),
    )
    .await;
    assert_eq!(second.0, StatusCode::OK, "second export: {}", second.1);

    let (first_page_status, first_page) =
        get_work_patch_artifacts(app.clone(), &owner_id, work_id, branch_id, "?limit=1").await;
    assert_eq!(
        first_page_status,
        StatusCode::OK,
        "first page: {first_page}"
    );
    assert_eq!(first_page["artifacts"].as_array().unwrap().len(), 1);
    assert_eq!(
        first_page["artifacts"][0]["patch_artifact_id"],
        second.1["patch_artifact_id"]
    );
    let cursor = &first_page["next_cursor"];
    assert_eq!(
        cursor["patch_artifact_id"],
        first_page["artifacts"][0]["patch_artifact_id"]
    );
    let continuation_query = format!(
        "?before_created_at={}&before_patch_artifact_id={}&limit=1",
        cursor["created_at"].as_str().unwrap(),
        cursor["patch_artifact_id"].as_str().unwrap(),
    );
    let (second_page_status, second_page) = get_work_patch_artifacts(
        app.clone(),
        &owner_id,
        work_id,
        branch_id,
        &continuation_query,
    )
    .await;
    assert_eq!(
        second_page_status,
        StatusCode::OK,
        "second page: {second_page}"
    );
    assert_eq!(
        second_page["artifacts"][0]["patch_artifact_id"],
        first.1["patch_artifact_id"]
    );
    assert!(second_page["next_cursor"].is_null());
    let invalid_cursor = get_work_patch_artifacts(
        app.clone(),
        &owner_id,
        work_id,
        branch_id,
        "?before_patch_artifact_id=patch-only",
    )
    .await;
    assert_eq!(invalid_cursor.0, StatusCode::BAD_REQUEST);
    let hidden_page =
        get_work_patch_artifacts(app.clone(), &other_owner_id, work_id, branch_id, "").await;
    assert_eq!(hidden_page.0, StatusCode::NOT_FOUND);

    let materialization_id = id("patch-materialization");
    sqlx::query(
        "INSERT INTO work_patch_materialization_operations
         (owner_id, work_id, operation_id, request_id, request_digest,
          patch_artifact_id, source_branch_id, target_branch_id,
          target_branch_revision, target_graph_revision,
          target_subject_record_revision, subject_ref,
          base_subject_revision, result_subject_revision, payload_hash,
          provider_ref, policy_decision_ref, operation_state, operation_phase,
          completed_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?,
                 'aborted', 'complete', NOW(6))",
    )
    .bind(&owner_id)
    .bind(work_id)
    .bind(&materialization_id)
    .bind(id("materialization-request"))
    .bind("0".repeat(64))
    .bind(patch_artifact_id)
    .bind(branch_id)
    .bind(branch_id)
    .bind(subject.branch_revision.get())
    .bind(subject.graph_revision.get())
    .bind(subject.subject_record_revision.get())
    .bind(subject_ref.as_str())
    .bind(first.1["base_subject_revision"].as_str().unwrap())
    .bind(first.1["result_subject_revision"].as_str().unwrap())
    .bind(first.1["payload_hash"].as_str().unwrap())
    .bind("server-git-worktree-v1")
    .bind(id("materialization-policy"))
    .execute(pool.get())
    .await
    .expect("insert durable materialization read fixture");
    let materialization_query = format!("?source_branch_id={branch_id}&limit=1");
    let (materialization_status, materialization_page) = get_work_patch_materializations(
        app.clone(),
        &owner_id,
        work_id,
        branch_id,
        &materialization_query,
    )
    .await;
    assert_eq!(materialization_status, StatusCode::OK);
    assert_eq!(materialization_page["source_branch_id"], branch_id);
    assert_eq!(
        materialization_page["operations"][0]["operation_id"],
        materialization_id
    );
    assert_field_absent(&materialization_page["operations"][0], "subject_ref");
    assert_field_absent(
        &materialization_page["operations"][0],
        "target_subject_record_revision",
    );
    assert_field_absent(&materialization_page["operations"][0], "executor_token");
    let hidden_materializations = get_work_patch_materializations(
        app.clone(),
        &other_owner_id,
        work_id,
        branch_id,
        &materialization_query,
    )
    .await;
    assert_eq!(hidden_materializations.0, StatusCode::NOT_FOUND);
    let incomplete_materialization_cursor = get_work_patch_materializations(
        app.clone(),
        &owner_id,
        work_id,
        branch_id,
        &format!("?source_branch_id={branch_id}&before_operation_id={materialization_id}"),
    )
    .await;
    assert_eq!(incomplete_materialization_cursor.0, StatusCode::BAD_REQUEST);

    let commit_request = serde_json::json!({
        "request_id": "commit-reviewed-patch-once",
        "patch_artifact_id": second.1["patch_artifact_id"],
        "expected_target_branch_revision": second_subject.branch_revision.get(),
        "expected_target_graph_revision": second_subject.graph_revision.get(),
        "message": "Commit the reviewed patch"
    });
    let admitted_commit = post_work_patch_commit(
        app.clone(),
        &owner_id,
        work_id,
        branch_id,
        commit_request.clone(),
    )
    .await;
    assert_eq!(
        admitted_commit.0,
        StatusCode::ACCEPTED,
        "admit patch commit: {}",
        admitted_commit.1
    );
    assert_eq!(admitted_commit.1["state"], "pending");
    assert_eq!(admitted_commit.1["phase"], "awaiting_dispatch");
    assert_field_absent(&admitted_commit.1, "subject_ref");
    assert_field_absent(&admitted_commit.1, "target_subject_record_revision");
    assert_field_absent(&admitted_commit.1, "author_name");
    assert_field_absent(&admitted_commit.1, "author_email");
    let commit_operation_id = admitted_commit.1["operation_id"]
        .as_str()
        .expect("commit operation id");
    assert_eq!(
        post_work_patch_commit(
            app.clone(),
            &owner_id,
            work_id,
            branch_id,
            commit_request.clone(),
        )
        .await,
        admitted_commit,
        "same authenticated command must replay one durable operation"
    );
    let hidden_commit = post_work_patch_commit(
        app.clone(),
        &other_owner_id,
        work_id,
        branch_id,
        commit_request.clone(),
    )
    .await;
    assert_eq!(hidden_commit.0, StatusCode::NOT_FOUND);
    let (commit_page_status, commit_page) =
        get_work_patch_commits(app.clone(), &owner_id, work_id, branch_id, "?limit=1").await;
    assert_eq!(commit_page_status, StatusCode::OK);
    assert_eq!(commit_page["operations"][0], admitted_commit.1);
    let loaded_commit = get_work_patch_commits(
        app.clone(),
        &owner_id,
        work_id,
        branch_id,
        &format!("/{commit_operation_id}"),
    )
    .await;
    assert_eq!(loaded_commit, (StatusCode::OK, admitted_commit.1.clone()));
    assert_eq!(
        get_work_patch_commits(app.clone(), &other_owner_id, work_id, branch_id, "")
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get_work_patch_commits(
            app.clone(),
            &owner_id,
            work_id,
            branch_id,
            &format!("?before_operation_id={commit_operation_id}"),
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let mut forged_identity = commit_request;
    forged_identity
        .as_object_mut()
        .expect("commit request object")
        .insert(
            "author_email".into(),
            serde_json::json!("forged@example.test"),
        );
    assert_eq!(
        post_work_patch_commit(app.clone(), &owner_id, work_id, branch_id, forged_identity,)
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        delete_work_patch_commit(
            app.clone(),
            &owner_id,
            work_id,
            branch_id,
            commit_operation_id,
        )
        .await,
        StatusCode::NO_CONTENT
    );
    let aborted_commit = get_work_patch_commits(
        app.clone(),
        &owner_id,
        work_id,
        branch_id,
        &format!("/{commit_operation_id}"),
    )
    .await;
    assert_eq!(aborted_commit.0, StatusCode::OK);
    assert_eq!(aborted_commit.1["state"], "aborted");
    assert_eq!(aborted_commit.1["phase"], "complete");

    std::fs::write(workspace.path().join("file.txt"), "unobserved\n")
        .expect("advance workspace without canonical observation");
    let stale = post_work_patch_export(
        app.clone(),
        &owner_id,
        work_id,
        branch_id,
        second_export_request,
    )
    .await;
    assert_eq!(stale.0, StatusCode::CONFLICT);
    assert_eq!(stale.1["code"], "work_patch_export_basis_conflict");
    let hidden = post_work_patch_export(
        app.clone(),
        &other_owner_id,
        work_id,
        branch_id,
        export_request,
    )
    .await;
    assert_eq!(hidden.0, StatusCode::NOT_FOUND);

    let payload_artifact_id: String = sqlx::query_scalar(
        "SELECT payload_artifact_id FROM work_patch_artifacts
         WHERE owner_id = ? AND work_id = ? AND patch_artifact_id = ?",
    )
    .bind(&owner_id)
    .bind(work_id)
    .bind(patch_artifact_id)
    .fetch_one(pool.get())
    .await
    .expect("internal patch payload identity");
    sqlx::query(
        "UPDATE session_artifacts SET content_json = JSON_SET(content_json, '$.sha256', ?)
         WHERE user_id = ? AND artifact_id = ?",
    )
    .bind("0".repeat(64))
    .bind(&owner_id)
    .bind(payload_artifact_id)
    .execute(pool.get())
    .await
    .expect("tamper backing payload fixture");
    let tampered =
        get_work_patch_content(app, &owner_id, work_id, branch_id, patch_artifact_id).await;
    assert_eq!(tampered.0, StatusCode::SERVICE_UNAVAILABLE);
    let tampered_error: Value =
        serde_json::from_slice(&tampered.2).expect("typed tamper error response");
    assert_eq!(
        tampered_error["code"],
        "work_patch_artifact_content_unavailable"
    );
    assert_eq!(tampered_error["retryable"], false);
    assert_eq!(tampered_error["action_hints"], serde_json::json!([]));

    cleanup_owner(&pool, &owner_id).await;
    cleanup_owner(&pool, &other_owner_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn work_catalog_is_owner_scoped_keyset_bounded_and_server_classified() {
    let Some((app, pool)) = setup().await else {
        return;
    };
    let owner_id = id("catalog-owner");
    let other_owner_id = id("catalog-other-owner");
    cleanup_owner(&pool, &owner_id).await;
    cleanup_owner(&pool, &other_owner_id).await;

    let (_, older) = post_work(
        app.clone(),
        &owner_id,
        serde_json::json!({
            "request_id": "catalog-older",
            "goal": "Older owner-scoped Work.",
            "criteria": []
        }),
    )
    .await;
    let (_, newer) = post_work(
        app.clone(),
        &owner_id,
        serde_json::json!({
            "request_id": "catalog-newer",
            "goal": "Newer Work with a decision waiting.",
            "criteria": []
        }),
    )
    .await;
    let (_, other) = post_work(
        app.clone(),
        &other_owner_id,
        serde_json::json!({
            "request_id": "catalog-other",
            "goal": "A different owner's Work.",
            "criteria": []
        }),
    )
    .await;
    let older_id = older["overview"]["work_id"].as_str().expect("older id");
    let newer_id = newer["overview"]["work_id"].as_str().expect("newer id");
    let newer_branch = newer["overview"]["delivery_branch"]["branch_id"]
        .as_str()
        .expect("newer branch");
    let other_id = other["overview"]["work_id"].as_str().expect("other id");
    for (work_id, created_at) in [
        (older_id, "2026-08-01 00:00:00.000001"),
        (newer_id, "2026-08-01 00:01:00.000001"),
    ] {
        sqlx::query("UPDATE works SET created_at = ? WHERE owner_id = ? AND work_id = ?")
            .bind(created_at)
            .bind(&owner_id)
            .bind(work_id)
            .execute(pool.get())
            .await
            .expect("set deterministic catalog order");
    }
    let repository = DatabaseWorkRepository::new(pool.clone());
    repository
        .propose_criteria(NewWorkCriteriaProposal {
            owner_id: WorkOwnerId::parse(&owner_id).expect("owner"),
            work_id: WorkId::parse(newer_id).expect("work"),
            branch_id: WorkBranchId::parse(newer_branch).expect("branch"),
            proposal_id: WorkProposalId::parse(id("catalog-proposal")).expect("proposal"),
            expected_work_revision: WorkRevision::INITIAL,
            expected_goal_revision: astra_services::work::GoalRevision::INITIAL,
            expected_criteria_set_revision: CriterionSetRevision::INITIAL,
            expected_branch_revision: WorkBranchRevision::INITIAL,
            expected_graph_revision: GraphRevision::INITIAL,
            members: vec![WorkCriteriaProposalMember::New {
                criterion_id: CriterionId::parse("catalog-review").expect("criterion"),
                definition: CriterionDefinition::HumanReview {
                    statement: CriterionStatement::parse("The result is reviewable.")
                        .expect("statement"),
                },
            }],
            source_kind: WorkProposalSourceKind::Model,
            source_ref: WorkChangeRef::parse(id("catalog-source")).expect("source"),
        })
        .await
        .expect("propose catalog decision");
    let binding = repository
        .load_branch_runtime_binding(
            &WorkOwnerId::parse(&owner_id).expect("owner"),
            &WorkId::parse(newer_id).expect("work"),
            &WorkBranchId::parse(newer_branch).expect("branch"),
        )
        .await
        .expect("delivery runtime binding");
    let active_run_id = id("catalog-active-run");
    let now = chrono::Utc::now().to_rfc3339();
    DatabaseRunStateStore::new(pool.clone())
        .insert_run(DurableRunRecord {
            run_id: active_run_id.clone(),
            user_id: owner_id.clone(),
            session_id: binding.session_id.as_str().to_string(),
            parent_run_id: None,
            root_run_id: Some(active_run_id.clone()),
            ancestor_path: Some(active_run_id.clone()),
            depth: 0,
            delegation_id: None,
            agent_id: None,
            retry_of: None,
            retry_scope: Some("node".to_string()),
            status: "running".to_string(),
            waiting_for: None,
            owner_pod_id: None,
            owner_lease_expires_at: None,
            run_generation: 0,
            last_event_idx: -1,
            checkpoint_version: None,
            checkpoint_json: None,
            error_code: None,
            error_message: None,
            retry_count: 0,
            total_prompt_tokens: 0,
            total_completion_tokens: 0,
            total_tool_calls: 0,
            agent_binding_id: None,
            agent_binding_name: None,
            agent_binding_schema_version: None,
            model_offering_id: None,
            resolved_model_name: None,
            capability_server_refs_json: None,
            runtime_profile: None,
            start_request_fingerprint: None,
            work_binding: Some(DurableWorkRunBinding::new(
                WorkId::parse(newer_id).expect("work"),
                WorkBranchId::parse(newer_branch).expect("branch"),
                GraphRevision::INITIAL,
            )),
            events: vec![serde_json::json!({"event_type": "run_started", "data": {}})],
            created_at: now.clone(),
            updated_at: now,
        })
        .await
        .expect("insert active delivery run");

    let (first_status, first, first_bytes) = get_works(app.clone(), &owner_id, "?limit=1").await;
    assert_eq!(first_status, StatusCode::OK, "first page: {first}");
    assert!(first_bytes < 64 * 1024);
    assert_eq!(first["entries"].as_array().map(Vec::len), Some(1));
    assert_eq!(first["entries"][0]["work_id"], newer_id);
    assert_eq!(first["entries"][0]["attention"], "needs_review");
    assert_eq!(first["entries"][0]["delivery_branch_activity"], "working");
    assert_eq!(first["entries"][0]["pending_decision_count"], 1);
    assert_eq!(first["entries"][0]["unseen_event_count"], 2);
    assert_field_absent(&first, "session_id");
    let cursor_time = first["next_cursor"]["created_at"]
        .as_str()
        .expect("cursor time");
    let cursor_id = first["next_cursor"]["work_id"]
        .as_str()
        .expect("cursor Work");
    let query = format!("?before_created_at={cursor_time}&before_work_id={cursor_id}&limit=1");
    let (second_status, second, _) = get_works(app.clone(), &owner_id, &query).await;
    assert_eq!(second_status, StatusCode::OK, "second page: {second}");
    assert_eq!(second["entries"][0]["work_id"], older_id);
    assert_eq!(second["entries"][0]["delivery_branch_activity"], "idle");
    assert_eq!(second["next_cursor"], Value::Null);

    let (other_status, other_page, _) = get_works(app.clone(), &other_owner_id, "").await;
    assert_eq!(other_status, StatusCode::OK);
    assert_eq!(other_page["entries"].as_array().map(Vec::len), Some(1));
    assert_eq!(other_page["entries"][0]["work_id"], other_id);
    assert!(!other_page.to_string().contains(newer_id));

    let (partial_status, partial, _) = get_works(
        app,
        &owner_id,
        "?before_created_at=2026-08-01T00:00:00.000001Z",
    )
    .await;
    assert_eq!(partial_status, StatusCode::BAD_REQUEST);
    assert_eq!(partial["code"], "invalid_work_catalog_cursor");

    cleanup_owner(&pool, &owner_id).await;
    cleanup_owner(&pool, &other_owner_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn work_branch_read_attachment_is_idempotent_owner_scoped_and_reaped() {
    let Some((app, pool)) = setup().await else {
        return;
    };
    let owner_id = id("attachment-owner");
    let other_owner_id = id("attachment-other-owner");
    cleanup_owner(&pool, &owner_id).await;
    cleanup_owner(&pool, &other_owner_id).await;

    let (create_status, created) = post_work(
        app.clone(),
        &owner_id,
        serde_json::json!({
            "request_id": "attachment-work",
            "goal": "Keep read continuity durable and bounded.",
            "criteria": []
        }),
    )
    .await;
    assert_eq!(create_status, StatusCode::CREATED, "create Work: {created}");
    let work_id = created["overview"]["work_id"].as_str().expect("Work id");
    let branch_id = created["overview"]["delivery_branch"]["branch_id"]
        .as_str()
        .expect("branch id");

    let (first_status, first) = attach_work_branch(
        app.clone(),
        &owner_id,
        work_id,
        branch_id,
        "attachment-open-1",
    )
    .await;
    assert_eq!(first_status, StatusCode::OK, "first attach: {first}");
    assert_eq!(first["work_id"], work_id);
    assert_eq!(first["branch_id"], branch_id);
    assert_eq!(first["mode"], "read_only");
    assert_eq!(first["sync"], "current");
    assert_eq!(first["head"], Value::Null);
    assert_field_absent(&first, "session_id");

    let (retry_status, retry) = attach_work_branch(
        app.clone(),
        &owner_id,
        work_id,
        branch_id,
        "attachment-open-1",
    )
    .await;
    assert_eq!(retry_status, StatusCode::OK);
    assert_eq!(retry["attachment_id"], first["attachment_id"]);
    assert_eq!(retry["attachment_epoch"], first["attachment_epoch"]);

    let (second_status, second) = attach_work_branch(
        app.clone(),
        &owner_id,
        work_id,
        branch_id,
        "attachment-open-2",
    )
    .await;
    assert_eq!(second_status, StatusCode::OK);
    assert_ne!(second["attachment_id"], first["attachment_id"]);
    assert!(
        second["attachment_epoch"].as_u64().expect("second epoch")
            > first["attachment_epoch"].as_u64().expect("first epoch"),
        "attachment epochs must be monotonic"
    );

    let (foreign_status, foreign) = attach_work_branch(
        app.clone(),
        &other_owner_id,
        work_id,
        branch_id,
        "attachment-foreign",
    )
    .await;
    assert_eq!(
        foreign_status,
        StatusCode::NOT_FOUND,
        "foreign attach: {foreign}"
    );

    let first_attachment_id = first["attachment_id"].as_str().expect("attachment id");
    assert_eq!(
        detach_work_branch(
            app.clone(),
            &other_owner_id,
            work_id,
            branch_id,
            first_attachment_id,
        )
        .await,
        StatusCode::NOT_FOUND,
        "another owner cannot observe or detach the attachment"
    );
    assert_eq!(
        detach_work_branch(
            app.clone(),
            &owner_id,
            work_id,
            branch_id,
            first_attachment_id,
        )
        .await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        detach_work_branch(app, &owner_id, work_id, branch_id, first_attachment_id,).await,
        StatusCode::NO_CONTENT,
        "detach retries are idempotent"
    );
    let active_after_detach: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM session_attachments WHERE owner_user_id = ?")
            .bind(&owner_id)
            .fetch_one(pool.get())
            .await
            .expect("count attachments after detach");
    assert_eq!(
        active_after_detach, 1,
        "detach must not remove sibling readers"
    );

    sqlx::query("UPDATE session_attachments SET expires_at_ms = 0 WHERE owner_user_id = ?")
        .bind(&owner_id)
        .execute(pool.get())
        .await
        .expect("expire owner attachments");
    let policy = astra_services::runtime_maintenance::RuntimeMaintenancePolicy {
        batch_limit: 16,
        ..Default::default()
    };
    let reaped =
        astra_services::runtime_maintenance::maintain_runtime_storage(&pool, None, &policy).await;
    assert!(reaped.expired_session_attachments_deleted >= 1);
    let retained: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM session_attachments WHERE owner_user_id = ?")
            .bind(&owner_id)
            .fetch_one(pool.get())
            .await
            .expect("count retained attachments");
    assert_eq!(retained, 0);

    cleanup_owner(&pool, &owner_id).await;
    cleanup_owner(&pool, &other_owner_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn work_branch_control_is_durable_owner_scoped_and_cas_guarded() {
    let Some((app, pool)) =
        setup_with_run_lifecycle(Arc::new(WorkTurnRecordingLifecycle::default())).await
    else {
        return;
    };
    let owner_id = id("control-owner");
    let other_owner_id = id("control-other-owner");
    cleanup_owner(&pool, &owner_id).await;
    cleanup_owner(&pool, &other_owner_id).await;
    let (create_status, created) = post_work(
        app.clone(),
        &owner_id,
        serde_json::json!({
            "request_id": "control-work",
            "goal": "Move branch control explicitly without stealing execution authority.",
            "criteria": []
        }),
    )
    .await;
    assert_eq!(create_status, StatusCode::CREATED, "create Work: {created}");
    let work_id = created["overview"]["work_id"].as_str().expect("Work id");
    let branch_id = created["overview"]["delivery_branch"]["branch_id"]
        .as_str()
        .expect("branch id");
    let (_, first) = attach_work_branch(
        app.clone(),
        &owner_id,
        work_id,
        branch_id,
        "control-reader-1",
    )
    .await;
    let (_, second) = attach_work_branch(
        app.clone(),
        &owner_id,
        work_id,
        branch_id,
        "control-reader-2",
    )
    .await;
    assert_eq!(first["branch_revision"], 1);
    assert_eq!(first["control_basis"]["writer_epoch"], 0);
    assert_eq!(first["control_basis"]["canonical_root_hash"], Value::Null);
    let first_id = first["attachment_id"].as_str().expect("first attachment");
    let second_id = second["attachment_id"].as_str().expect("second attachment");
    let command = |request_id: &str,
                   attachment_id: &str,
                   kind: &str,
                   branch_revision: i64,
                   writer_epoch: u64| {
        serde_json::json!({
            "request_id": request_id,
            "expected_branch_revision": branch_revision,
            "expected_writer_epoch": writer_epoch,
            "expected_canonical_root_hash": null,
            "command": {"kind": kind, "attachment_id": attachment_id}
        })
    };

    let acquire_first = command("acquire-first", first_id, "acquire_branch_control", 1, 0);
    let (acquire_status, acquired) = post_work_control(
        app.clone(),
        &owner_id,
        work_id,
        branch_id,
        acquire_first.clone(),
    )
    .await;
    assert_eq!(acquire_status, StatusCode::CREATED, "acquire: {acquired}");
    assert_eq!(acquired["state"], "succeeded");
    assert_eq!(acquired["outcome"], "acquired");
    assert_eq!(
        acquired["control_basis"]["canonical_root_hash"],
        Value::Null
    );
    assert_field_absent(&acquired, "session_id");
    let operation_id = acquired["operation_id"].as_str().expect("operation id");
    let (reattach_status, reattached) = attach_work_branch(
        app.clone(),
        &owner_id,
        work_id,
        branch_id,
        "control-reader-1",
    )
    .await;
    assert_eq!(reattach_status, StatusCode::OK);
    assert_eq!(reattached["mode"], "controller");

    let (retry_status, retried) =
        post_work_control(app.clone(), &owner_id, work_id, branch_id, acquire_first).await;
    assert_eq!(retry_status, StatusCode::CREATED);
    assert_eq!(
        retried, acquired,
        "exact retry must return the durable result"
    );
    let (reuse_status, reuse) = post_work_control(
        app.clone(),
        &owner_id,
        work_id,
        branch_id,
        command("acquire-first", second_id, "acquire_branch_control", 1, 0),
    )
    .await;
    assert_eq!(reuse_status, StatusCode::CONFLICT);
    assert_eq!(reuse["code"], "idempotency_mismatch");

    let (take_status, taken) = post_work_control(
        app.clone(),
        &owner_id,
        work_id,
        branch_id,
        command("take-second", second_id, "acquire_branch_control", 1, 0),
    )
    .await;
    assert_eq!(take_status, StatusCode::CREATED, "take: {taken}");
    assert_eq!(taken["outcome"], "acquired");

    let (branch_conflict_status, branch_conflict) = post_work_control(
        app.clone(),
        &owner_id,
        work_id,
        branch_id,
        command("stale-branch", first_id, "acquire_branch_control", 2, 0),
    )
    .await;
    assert_eq!(branch_conflict_status, StatusCode::CREATED);
    assert_eq!(branch_conflict["state"], "conflict");
    assert_eq!(branch_conflict["outcome"], "branch_revision_conflict");
    assert_eq!(branch_conflict["branch_revision"], 1);
    assert_eq!(branch_conflict["control_basis"], Value::Null);

    let (head_conflict_status, head_conflict) = post_work_control(
        app.clone(),
        &owner_id,
        work_id,
        branch_id,
        command("stale-head", first_id, "acquire_branch_control", 1, 1),
    )
    .await;
    assert_eq!(head_conflict_status, StatusCode::CREATED);
    assert_eq!(head_conflict["state"], "conflict");
    assert_eq!(head_conflict["outcome"], "head_conflict");
    assert_eq!(head_conflict["control_basis"]["writer_epoch"], 0);

    let (get_status, loaded) =
        get_work_control(app.clone(), &owner_id, work_id, branch_id, operation_id).await;
    assert_eq!(get_status, StatusCode::OK);
    assert_eq!(loaded, acquired);
    let (abort_status, abort) =
        delete_work_control(app.clone(), &owner_id, work_id, branch_id, operation_id).await;
    assert_eq!(abort_status, StatusCode::CONFLICT);
    assert_eq!(abort["code"], "control_operation_terminal");
    let (foreign_status, foreign) = get_work_control(
        app.clone(),
        &other_owner_id,
        work_id,
        branch_id,
        operation_id,
    )
    .await;
    assert_eq!(foreign_status, StatusCode::NOT_FOUND);
    assert_eq!(foreign["code"], "control_operation_not_found");
    let (foreign_abort_status, _) = delete_work_control(
        app.clone(),
        &other_owner_id,
        work_id,
        branch_id,
        operation_id,
    )
    .await;
    assert_eq!(foreign_abort_status, StatusCode::NOT_FOUND);

    let session_id: String = sqlx::query_scalar(
        "SELECT session_id FROM work_branches
         WHERE owner_id = ? AND work_id = ? AND branch_id = ?",
    )
    .bind(&owner_id)
    .bind(work_id)
    .bind(branch_id)
    .fetch_one(pool.get())
    .await
    .expect("load internal branch binding");
    let key = SessionKeyV1::owner_session(
        "server",
        &owner_id,
        &session_id,
        astra_turn_types::DEFAULT_CONVERSATION_BRANCH_ID,
    );
    let target_attachment: astra_turn_types::SessionAttachmentV1 = serde_json::from_str(
        &sqlx::query_scalar::<_, String>(
            "SELECT attachment_json FROM session_attachments
             WHERE owner_user_id = ? AND session_id = ? AND attachment_id = ?",
        )
        .bind(&owner_id)
        .bind(&session_id)
        .bind(first_id)
        .fetch_one(pool.get())
        .await
        .expect("load force target attachment"),
    )
    .expect("decode force target attachment");
    let coordinator = DatabaseSessionContextCoordinator::new(pool.clone());
    let source_lease = match coordinator
        .acquire_writer(
            &key,
            None,
            &target_attachment.actor,
            std::time::Duration::from_secs(60),
            "work-force-source-writer",
        )
        .await
        .expect("acquire active source writer")
    {
        AcquireWriterOutcome::Acquired(lease) | AcquireWriterOutcome::AlreadyAcquired(lease) => {
            lease
        }
        other => panic!("unexpected writer acquisition: {other:?}"),
    };
    let stale_transfer = astra_services::WriterTransferRequestV1 {
        handoff_id: "work-force-stale-epoch-proof".into(),
        idempotency_key: "work-force-stale-epoch-proof".into(),
        key: key.clone(),
        mode: astra_turn_types::SessionHandoffModeV1::Forced,
        source_lease: None,
        expected_writer_epoch: Some(source_lease.writer_epoch - 1),
        expected_cursor: None,
        target_actor: target_attachment.actor.clone(),
        risk: astra_turn_types::HandoffRiskEvidenceV1 {
            forced_authorization_id: Some("verified-test-authorization".into()),
            ..astra_turn_types::HandoffRiskEvidenceV1::default()
        },
    };
    assert!(matches!(
        coordinator
            .transfer_writer(&stale_transfer, std::time::Duration::from_secs(60))
            .await
            .expect("reject stale forced transfer"),
        astra_services::TransferWriterOutcome::Conflict {
            reason: astra_services::WriterTransferConflictV1::SourceWriterChanged,
            ..
        }
    ));
    let uncertain_identity = format!("sha256:{}", "c".repeat(64));
    let effect_run_id = id("force-effect-run");
    sqlx::query(
        "INSERT INTO agent_runs
         (run_id, user_id, session_id, root_run_id, ancestor_path, depth, status)
         VALUES (?, ?, ?, ?, '', 0, 'cancelled')",
    )
    .bind(&effect_run_id)
    .bind(&owner_id)
    .bind(&session_id)
    .bind(&effect_run_id)
    .execute(pool.get())
    .await
    .expect("insert terminal effect run");
    sqlx::query(
        "INSERT INTO tool_invocation_ledger
         (user_id, session_id, run_id, turn_chain_id, invocation_id,
          identity_key, fingerprint_json, decision_json, state,
          dispatch_certainty, attempt_count)
         VALUES (?, ?, ?, 'turn-force', 'effect-force', ?, '{}', '{}',
                 'dispatched', 'dispatched', 1)",
    )
    .bind(&owner_id)
    .bind(&session_id)
    .bind(&effect_run_id)
    .bind(&uncertain_identity)
    .execute(pool.get())
    .await
    .expect("insert uncertain effect");
    let force_command = |proof: &str| {
        serde_json::json!({
            "request_id": "force-first",
            "expected_branch_revision": 1,
            "expected_writer_epoch": source_lease.writer_epoch,
            "expected_canonical_root_hash": null,
            "command": {
                "kind": "force_takeover",
                "attachment_id": first_id,
                "reauthentication_proof": proof
            }
        })
    };
    let (abort_admission_status, abort_admission) = post_work_control(
        app.clone(),
        &owner_id,
        work_id,
        branch_id,
        serde_json::json!({
            "request_id": "force-abort-before-fence",
            "expected_branch_revision": 1,
            "expected_writer_epoch": source_lease.writer_epoch,
            "expected_canonical_root_hash": null,
            "command": {
                "kind": "force_takeover",
                "attachment_id": second_id,
                "reauthentication_proof": "invalid-step-up"
            }
        }),
    )
    .await;
    assert_eq!(
        abort_admission_status,
        StatusCode::FORBIDDEN,
        "{abort_admission}"
    );
    let abort_operation_id: String = sqlx::query_scalar(
        "SELECT operation_id FROM work_branch_control_operations
         WHERE owner_id = ? AND operation_kind = 'force_takeover'
           AND operation_state = 'pending' AND attachment_id = ?",
    )
    .bind(&owner_id)
    .bind(second_id)
    .fetch_one(pool.get())
    .await
    .expect("load abortable force operation");
    let (pending_status, pending) = get_work_control(
        app.clone(),
        &owner_id,
        work_id,
        branch_id,
        &abort_operation_id,
    )
    .await;
    assert_eq!(pending_status, StatusCode::OK);
    assert_eq!(pending["state"], "pending");
    assert_eq!(pending["progress"]["phase"], "awaiting_reauthentication");
    assert_eq!(pending["progress"]["abortable"], true);
    let control_service = DatabaseWorkBranchControlService::new(pool.clone());
    let owner = WorkOwnerId::parse(&owner_id).expect("owner id");
    let work = WorkId::parse(work_id).expect("work id");
    let branch = WorkBranchId::parse(branch_id).expect("branch id");
    assert!(
        control_service
            .claim_force_executor(&owner, &work, &branch, &abort_operation_id, 60)
            .await
            .expect("reject unauthorised executor")
            .is_none(),
        "a pending command cannot execute before step-up authorization",
    );
    let (abort_status, _) = delete_work_control(
        app.clone(),
        &owner_id,
        work_id,
        branch_id,
        &abort_operation_id,
    )
    .await;
    assert_eq!(abort_status, StatusCode::NO_CONTENT);
    let (abort_retry_status, _) = delete_work_control(
        app.clone(),
        &owner_id,
        work_id,
        branch_id,
        &abort_operation_id,
    )
    .await;
    assert_eq!(abort_retry_status, StatusCode::NO_CONTENT);
    let (_, aborted) = get_work_control(
        app.clone(),
        &owner_id,
        work_id,
        branch_id,
        &abort_operation_id,
    )
    .await;
    assert_eq!(aborted["state"], "aborted");
    assert_eq!(aborted["outcome"], "aborted");
    assert_field_absent(&aborted, "progress");

    let (handoff_abort_admission_status, handoff_abort_admission) = post_work_control(
        app.clone(),
        &owner_id,
        work_id,
        branch_id,
        serde_json::json!({
            "request_id": "force-abort-after-handoff",
            "expected_branch_revision": 1,
            "expected_writer_epoch": source_lease.writer_epoch,
            "expected_canonical_root_hash": null,
            "command": {
                "kind": "force_takeover",
                "attachment_id": first_id,
                "reauthentication_proof": "invalid-step-up"
            }
        }),
    )
    .await;
    assert_eq!(
        handoff_abort_admission_status,
        StatusCode::FORBIDDEN,
        "{handoff_abort_admission}"
    );
    let handoff_abort_operation_id: String = sqlx::query_scalar(
        "SELECT operation_id FROM work_branch_control_operations
         WHERE owner_id = ? AND operation_kind = 'force_takeover'
           AND operation_state = 'pending' AND attachment_id = ?",
    )
    .bind(&owner_id)
    .bind(first_id)
    .fetch_one(pool.get())
    .await
    .expect("load handoff-backed abort operation");
    sqlx::query(
        "UPDATE work_branch_control_operations
         SET forced_authorization_id = 'consumed-test-authorization'
         WHERE owner_id = ? AND operation_id = ?",
    )
    .bind(&owner_id)
    .bind(&handoff_abort_operation_id)
    .execute(pool.get())
    .await
    .expect("record test authorization crash boundary");
    let executor_token = control_service
        .claim_force_executor(&owner, &work, &branch, &handoff_abort_operation_id, 60)
        .await
        .expect("claim first executor")
        .expect("first executor owns the pending operation");
    assert!(
        control_service
            .claim_force_executor(&owner, &work, &branch, &handoff_abort_operation_id, 60,)
            .await
            .expect("reject concurrent executor")
            .is_none(),
        "one pending operation must not run two takeover executors",
    );
    assert!(
        !control_service
            .renew_force_executor(
                &owner,
                &work,
                &branch,
                &handoff_abort_operation_id,
                "not-the-owner",
                60,
            )
            .await
            .expect("reject stale executor renewal"),
        "an executor token is a fencing identity, not only a lease hint",
    );
    assert!(
        control_service
            .renew_force_executor(
                &owner,
                &work,
                &branch,
                &handoff_abort_operation_id,
                &executor_token,
                60,
            )
            .await
            .expect("renew active executor"),
    );
    control_service
        .release_force_executor(
            &owner,
            &work,
            &branch,
            &handoff_abort_operation_id,
            &executor_token,
        )
        .await
        .expect("release test executor");
    let handoff_coordinator: Arc<dyn SessionContextCoordinator> =
        Arc::new(DatabaseSessionContextCoordinator::new(pool.clone()));
    let handoff_service =
        DatabaseSessionHandoffService::new(pool.clone(), Arc::clone(&handoff_coordinator));
    let requested_handoff = handoff_service
        .request_handoff(
            &astra_services::RequestSessionHandoffV1 {
                idempotency_key: format!("work-force:{handoff_abort_operation_id}"),
                key: key.clone(),
                mode: astra_turn_types::SessionHandoffModeV1::Forced,
                from_attachment_id: None,
                to_attachment_id: target_attachment.attachment_id.clone(),
                from_placement: astra_turn_types::SessionPlacementV1::Server,
                to_placement: target_attachment.placement,
                target_actor: target_attachment.actor.clone(),
                base_cursor: None,
                authority_epochs: handoff_coordinator
                    .load_authority_epochs(&key)
                    .await
                    .expect("load handoff authority epochs")
                    .unwrap_or_default(),
                workspace: target_attachment.workspace.clone(),
                watermarks: astra_turn_types::HandoffOperationWatermarksV1::default(),
                risk: astra_turn_types::HandoffRiskEvidenceV1 {
                    forced_authorization_id: Some("consumed-test-authorization".into()),
                    ..astra_turn_types::HandoffRiskEvidenceV1::default()
                },
                reason: "test_safe_abort".into(),
            },
            std::time::Duration::from_secs(60),
        )
        .await
        .expect("request crash-boundary handoff");
    let (_, handoff_pending) = get_work_control(
        app.clone(),
        &owner_id,
        work_id,
        branch_id,
        &handoff_abort_operation_id,
    )
    .await;
    assert_eq!(handoff_pending["progress"]["phase"], "preparing");
    assert_eq!(handoff_pending["progress"]["abortable"], true);
    let (handoff_abort_status, _) = delete_work_control(
        app.clone(),
        &owner_id,
        work_id,
        branch_id,
        &handoff_abort_operation_id,
    )
    .await;
    assert_eq!(handoff_abort_status, StatusCode::NO_CONTENT);
    assert_eq!(
        handoff_service
            .load_handoff(&key, &requested_handoff.handoff_id)
            .await
            .expect("load aborted handoff")
            .state,
        astra_turn_types::SessionHandoffStateV1::Aborted
    );
    let (invalid_force_status, invalid_force) = post_work_control(
        app.clone(),
        &owner_id,
        work_id,
        branch_id,
        force_command("invalid-step-up"),
    )
    .await;
    assert_eq!(invalid_force_status, StatusCode::FORBIDDEN);
    assert_eq!(invalid_force["code"], "reauthentication_required");
    let (force_status, forced) = post_work_control(
        app.clone(),
        &owner_id,
        work_id,
        branch_id,
        force_command("valid-step-up"),
    )
    .await;
    assert_eq!(
        force_status,
        StatusCode::ACCEPTED,
        "force takeover: {forced}"
    );
    assert_eq!(forced["state"], "pending");
    assert_eq!(forced["outcome"], "pending");
    assert_eq!(forced["progress"]["phase"], "preparing");
    let forced = wait_work_control_terminal(
        app.clone(),
        &owner_id,
        work_id,
        branch_id,
        forced["operation_id"].as_str().expect("operation id"),
    )
    .await;
    assert_eq!(forced["state"], "succeeded");
    assert_eq!(forced["outcome"], "taken_over");
    assert!(forced["control_basis"]["writer_epoch"].as_u64().unwrap() > source_lease.writer_epoch);
    assert_field_absent(&forced, "session_id");
    let (force_retry_status, force_retry) = post_work_control(
        app.clone(),
        &owner_id,
        work_id,
        branch_id,
        force_command("valid-step-up"),
    )
    .await;
    assert_eq!(force_retry_status, StatusCode::CREATED);
    assert_eq!(
        force_retry, forced,
        "force retry must not consume step-up twice"
    );
    let recovery_command = serde_json::json!({
        "request_id": "force-resume-after-fence",
        "expected_branch_revision": 1,
        "expected_writer_epoch": forced["control_basis"]["writer_epoch"],
        "expected_canonical_root_hash": null,
        "command": {
            "kind": "force_takeover",
            "attachment_id": second_id,
            "reauthentication_proof": "invalid-step-up"
        }
    });
    let (recovery_admission_status, _) = post_work_control(
        app.clone(),
        &owner_id,
        work_id,
        branch_id,
        recovery_command.clone(),
    )
    .await;
    assert_eq!(recovery_admission_status, StatusCode::FORBIDDEN);
    let recovery_operation_id: String = sqlx::query_scalar(
        "SELECT operation_id FROM work_branch_control_operations
         WHERE owner_id = ? AND work_id = ? AND branch_id = ?
           AND attachment_id = ? AND operation_state = 'pending'",
    )
    .bind(&owner_id)
    .bind(work_id)
    .bind(branch_id)
    .bind(second_id)
    .fetch_one(pool.get())
    .await
    .expect("load crash-recovery operation");
    sqlx::query(
        "UPDATE work_branch_control_operations
         SET forced_authorization_id = 'consumed-recovery-authorization'
         WHERE owner_id = ? AND operation_id = ?",
    )
    .bind(&owner_id)
    .bind(&recovery_operation_id)
    .execute(pool.get())
    .await
    .expect("record crash-recovery authorization");
    let second_target: astra_turn_types::SessionAttachmentV1 = serde_json::from_str(
        &sqlx::query_scalar::<_, String>(
            "SELECT attachment_json FROM session_attachments
             WHERE owner_user_id = ? AND session_id = ? AND attachment_id = ?",
        )
        .bind(&owner_id)
        .bind(&session_id)
        .bind(second_id)
        .fetch_one(pool.get())
        .await
        .expect("load recovery target attachment"),
    )
    .expect("decode recovery target attachment");
    let recovery_handoff = handoff_service
        .request_handoff(
            &astra_services::RequestSessionHandoffV1 {
                idempotency_key: format!("work-force:{recovery_operation_id}"),
                key: key.clone(),
                mode: astra_turn_types::SessionHandoffModeV1::Forced,
                from_attachment_id: None,
                to_attachment_id: second_target.attachment_id.clone(),
                from_placement: astra_turn_types::SessionPlacementV1::Server,
                to_placement: second_target.placement,
                target_actor: second_target.actor.clone(),
                base_cursor: None,
                authority_epochs: handoff_coordinator
                    .load_authority_epochs(&key)
                    .await
                    .expect("load recovery authority epochs")
                    .unwrap_or_default(),
                workspace: second_target.workspace.clone(),
                watermarks: astra_turn_types::HandoffOperationWatermarksV1::default(),
                risk: astra_turn_types::HandoffRiskEvidenceV1 {
                    forced_authorization_id: Some("consumed-recovery-authorization".into()),
                    ..astra_turn_types::HandoffRiskEvidenceV1::default()
                },
                reason: "test_resume_after_fence".into(),
            },
            std::time::Duration::from_secs(60),
        )
        .await
        .expect("prepare recovery handoff");
    let recovery_handoff = handoff_service
        .transition_handoff(&astra_services::TransitionSessionHandoffV1 {
            idempotency_key: format!("work-force:{recovery_operation_id}:validate"),
            key: key.clone(),
            handoff_id: recovery_handoff.handoff_id,
            expected_state: recovery_handoff.state,
            expected_transition_seq: recovery_handoff.transition_seq,
            next_state: astra_turn_types::SessionHandoffStateV1::Validating,
            patch: astra_services::HandoffTransitionPatchV1::default(),
        })
        .await
        .expect("validate recovery handoff");
    let recovery_handoff = handoff_service
        .fence_writer(
            &key,
            &recovery_handoff.handoff_id,
            None,
            forced["control_basis"]["writer_epoch"].as_u64(),
            std::time::Duration::from_secs(60),
            &format!("work-force:{recovery_operation_id}:fence"),
        )
        .await
        .expect("fence recovery handoff")
        .handoff;
    assert_eq!(
        recovery_handoff.state,
        astra_turn_types::SessionHandoffStateV1::Fenced
    );
    sqlx::query(
        "UPDATE work_branch_control_operations SET handoff_id = ?
         WHERE owner_id = ? AND operation_id = ?",
    )
    .bind(&recovery_handoff.handoff_id)
    .bind(&owner_id)
    .bind(&recovery_operation_id)
    .execute(pool.get())
    .await
    .expect("bind recovery handoff at the simulated crash boundary");
    let (recovery_status, recovery_pending) =
        post_work_control(app.clone(), &owner_id, work_id, branch_id, recovery_command).await;
    assert_eq!(recovery_status, StatusCode::ACCEPTED, "{recovery_pending}");
    let recovered = wait_work_control_terminal(
        app.clone(),
        &owner_id,
        work_id,
        branch_id,
        &recovery_operation_id,
    )
    .await;
    assert_eq!(recovered["state"], "succeeded");
    assert_eq!(recovered["outcome"], "taken_over");
    let effect_state: String = sqlx::query_scalar(
        "SELECT state FROM tool_invocation_ledger
         WHERE user_id = ? AND session_id = ? AND identity_key = ?",
    )
    .bind(&owner_id)
    .bind(&session_id)
    .bind(&uncertain_identity)
    .fetch_one(pool.get())
    .await
    .expect("load sealed force effect");
    assert_eq!(effect_state, "outcome_unknown");
    assert!(matches!(
        coordinator
            .reserve_turn(
                &source_lease,
                None,
                std::time::Duration::from_secs(10),
                "work-force-stale-source"
            )
            .await,
        Err(astra_services::SessionContextCoordinatorError::Fenced)
    ));

    let (abandoned_status, abandoned) = post_work_control(
        app.clone(),
        &owner_id,
        work_id,
        branch_id,
        serde_json::json!({
            "request_id": "force-abandoned",
            "expected_branch_revision": 1,
            "expected_writer_epoch": recovered["control_basis"]["writer_epoch"],
            "expected_canonical_root_hash": null,
            "command": {
                "kind": "force_takeover",
                "attachment_id": second_id,
                "reauthentication_proof": "invalid-step-up"
            }
        }),
    )
    .await;
    assert_eq!(abandoned_status, StatusCode::FORBIDDEN, "{abandoned}");
    sqlx::query(
        "UPDATE work_branch_control_operations
         SET created_at = DATE_SUB(NOW(6), INTERVAL 2 DAY)
         WHERE owner_id = ? AND operation_kind = 'force_takeover'
           AND operation_state = 'pending'",
    )
    .bind(&owner_id)
    .execute(pool.get())
    .await
    .expect("age abandoned force operation");

    let controller_ids: Vec<String> = sqlx::query_scalar(
        "SELECT attachment_id FROM session_attachments
         WHERE owner_user_id = ? AND mode = 'controller' AND expires_at_ms > 0",
    )
    .bind(&owner_id)
    .fetch_all(pool.get())
    .await
    .expect("load controller attachments");
    assert_eq!(controller_ids, vec![second_id.to_string()]);
    sqlx::query(
        "UPDATE work_branch_control_operations
         SET completed_at = DATE_SUB(NOW(6), INTERVAL 31 DAY)
         WHERE owner_id = ? AND operation_id = ?",
    )
    .bind(&owner_id)
    .bind(operation_id)
    .execute(pool.get())
    .await
    .expect("age terminal control operation");
    let reaped = astra_services::runtime_maintenance::maintain_runtime_storage(
        &pool,
        None,
        &astra_services::runtime_maintenance::RuntimeMaintenancePolicy {
            batch_limit: 16,
            ..Default::default()
        },
    )
    .await;
    assert_eq!(reaped.work_branch_control_operations_expired, 2);
    cleanup_owner(&pool, &owner_id).await;
    cleanup_owner(&pool, &other_owner_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn work_criteria_route_is_bounded_owner_scoped_and_revision_pinned() {
    let Some((app, pool)) = setup().await else {
        return;
    };
    let owner_id = id("owner");
    let other_owner_id = id("other-owner");
    cleanup_owner(&pool, &owner_id).await;
    cleanup_owner(&pool, &other_owner_id).await;
    let (create_status, created) = post_work(
        app.clone(),
        &owner_id,
        serde_json::json!({
            "request_id": "start-for-criteria-read",
            "goal": "Show the accepted Done-when contract.",
            "criteria": [
                {
                    "criterion_id": "tests-pass",
                    "kind": "test_check",
                    "statement": "Relevant tests pass.",
                    "command": "cargo test -p astra-runtime work_handlers"
                },
                {
                    "criterion_id": "review-complete",
                    "kind": "human_review",
                    "statement": "The result is reviewable."
                }
            ]
        }),
    )
    .await;
    assert_eq!(create_status, StatusCode::CREATED, "created: {created}");
    let work_id = created["overview"]["work_id"].as_str().expect("work id");

    let (first_status, first, first_bytes) =
        get_work_criteria(app.clone(), &owner_id, work_id, "?limit=1").await;
    assert_eq!(first_status, StatusCode::OK, "first page: {first}");
    assert!(first_bytes < 128 * 1024);
    assert_eq!(first["schema_version"], 1);
    assert_eq!(first["basis"]["criteria_set_revision"], 1);
    assert_eq!(first["basis"]["member_count"], 2);
    assert_eq!(
        first["criteria"]["entries"][0]["criterion_id"],
        "review-complete"
    );
    assert_eq!(first["criteria"]["entries"][0]["kind"], "human_review");
    assert_eq!(first["next_cursor"]["offset"], 1);

    let (second_status, second, _) = get_work_criteria(
        app.clone(),
        &owner_id,
        work_id,
        "?criteria_set_revision=1&offset=1&limit=1",
    )
    .await;
    assert_eq!(second_status, StatusCode::OK, "second page: {second}");
    assert_eq!(
        second["criteria"]["entries"][0]["criterion_id"],
        "tests-pass"
    );
    assert_eq!(second["criteria"]["entries"][0]["kind"], "test_check");
    assert_eq!(
        second["criteria"]["entries"][0]["command"],
        "cargo test -p astra-runtime work_handlers"
    );
    assert!(second["next_cursor"].is_null());

    let (unpinned_status, unpinned, _) =
        get_work_criteria(app.clone(), &owner_id, work_id, "?offset=1").await;
    assert_eq!(unpinned_status, StatusCode::BAD_REQUEST);
    assert_eq!(unpinned["code"], "invalid_work_criteria_query");
    let (other_status, other, _) =
        get_work_criteria(app.clone(), &other_owner_id, work_id, "").await;
    assert_eq!(other_status, StatusCode::NOT_FOUND);
    assert_eq!(other["code"], "work_not_found");

    DatabaseWorkRepository::new(pool.clone())
        .accept_criteria(WorkCriteriaChange {
            owner_id: WorkOwnerId::parse(&owner_id).expect("owner"),
            work_id: WorkId::parse(work_id).expect("work"),
            expected_work_revision: WorkRevision::INITIAL,
            expected_criteria_set_revision: CriterionSetRevision::INITIAL,
            members: ["review-complete", "tests-pass"]
                .into_iter()
                .map(|criterion_id| {
                    CriterionSetMemberChange::Existing(CriterionRevisionRef {
                        criterion_id: CriterionId::parse(criterion_id).expect("criterion"),
                        revision: CriterionRevision::INITIAL,
                    })
                })
                .collect(),
            source_ref: WorkChangeRef::parse(id("criteria-change")).expect("source"),
            reason: None,
        })
        .await
        .expect("advance criterion set");
    let (stale_status, stale, _) = get_work_criteria(
        app.clone(),
        &owner_id,
        work_id,
        "?criteria_set_revision=1&offset=1&limit=1",
    )
    .await;
    assert_eq!(stale_status, StatusCode::CONFLICT);
    assert_eq!(stale["code"], "work_criteria_revision_conflict");

    let (current_status, current, _) = get_work_criteria(app, &owner_id, work_id, "?limit=1").await;
    assert_eq!(current_status, StatusCode::OK);
    assert_eq!(current["basis"]["criteria_set_revision"], 2);
    cleanup_owner(&pool, &owner_id).await;
    cleanup_owner(&pool, &other_owner_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn criteria_proposal_http_contract_is_bounded_scoped_and_exactly_idempotent() {
    let Some((app, pool)) = setup().await else {
        return;
    };
    let owner_id = id("owner");
    let other_owner_id = id("other-owner");
    cleanup_owner(&pool, &owner_id).await;
    cleanup_owner(&pool, &other_owner_id).await;
    let (create_status, created) = post_work(
        app.clone(),
        &owner_id,
        serde_json::json!({
            "request_id": "start-for-criteria-proposal",
            "goal": "Review an explicit provisional Done-when contract.",
            "criteria": []
        }),
    )
    .await;
    assert_eq!(create_status, StatusCode::CREATED, "created: {created}");
    let work_id = created["overview"]["work_id"].as_str().expect("work id");
    let branch_id = created["overview"]["delivery_branch"]["branch_id"]
        .as_str()
        .expect("branch id");
    let proposal_id = id("criteria-proposal");
    let repository = DatabaseWorkRepository::new(pool.clone());
    let proposed = repository
        .propose_criteria(NewWorkCriteriaProposal {
            owner_id: WorkOwnerId::parse(&owner_id).expect("owner"),
            work_id: WorkId::parse(work_id).expect("work"),
            branch_id: WorkBranchId::parse(branch_id).expect("branch"),
            proposal_id: WorkProposalId::parse(&proposal_id).expect("proposal"),
            expected_work_revision: WorkRevision::INITIAL,
            expected_goal_revision: astra_services::work::GoalRevision::INITIAL,
            expected_criteria_set_revision: CriterionSetRevision::INITIAL,
            expected_branch_revision: WorkBranchRevision::INITIAL,
            expected_graph_revision: GraphRevision::INITIAL,
            members: vec![WorkCriteriaProposalMember::New {
                criterion_id: CriterionId::parse("relevant-tests-pass").expect("criterion"),
                definition: CriterionDefinition::TestCheck {
                    statement: CriterionStatement::parse("Relevant tests pass.")
                        .expect("statement"),
                    command: CriterionCommand::parse("cargo test -p astra-runtime work_handlers")
                        .expect("command"),
                },
            }],
            source_kind: WorkProposalSourceKind::Model,
            source_ref: WorkChangeRef::parse(id("model-invocation")).expect("source"),
        })
        .await
        .expect("propose criteria");

    let (list_status, list, list_bytes) = criteria_proposal_request(
        app.clone(),
        &owner_id,
        "GET",
        work_id,
        branch_id,
        None,
        None,
    )
    .await;
    assert_eq!(list_status, StatusCode::OK, "list: {list}");
    assert!(list_bytes < 16 * 1024, "discovery remains summary-sized");
    assert_eq!(list["proposals"].as_array().expect("proposals").len(), 1);
    assert_eq!(list["proposals"][0]["member_count"], 1);
    assert_field_absent(&list, "members");
    assert_field_absent(&list, "source_ref");

    let (detail_status, detail, _) = criteria_proposal_request(
        app.clone(),
        &owner_id,
        "GET",
        work_id,
        branch_id,
        Some(&proposal_id),
        None,
    )
    .await;
    assert_eq!(detail_status, StatusCode::OK, "detail: {detail}");
    assert_eq!(detail["proposal"]["status"], "pending");
    assert_eq!(detail["members"][0]["member_kind"], "new");
    assert_field_absent(&detail, "source_ref");

    for (request_owner, request_branch) in [
        (other_owner_id.as_str(), branch_id),
        (owner_id.as_str(), "branch-other"),
    ] {
        let (status, response, _) = criteria_proposal_request(
            app.clone(),
            request_owner,
            "GET",
            work_id,
            request_branch,
            Some(&proposal_id),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "isolated: {response}");
        assert_eq!(response["code"], "work_criteria_proposal_not_found");
    }

    let decision = serde_json::json!({
        "request_id": "accept-criteria-proposal",
        "decision": "accept",
        "payload_hash": proposed.payload_hash.as_str(),
        "expected_work_revision": 1,
        "expected_goal_revision": 1,
        "expected_criteria_set_revision": 1,
        "expected_branch_revision": 1,
        "expected_graph_revision": 1
    });
    let (accepted_status, accepted, _) = criteria_proposal_request(
        app.clone(),
        &owner_id,
        "PUT",
        work_id,
        branch_id,
        Some(&proposal_id),
        Some(decision.clone()),
    )
    .await;
    assert_eq!(accepted_status, StatusCode::OK, "accepted: {accepted}");
    assert_eq!(accepted["proposal"]["status"], "accepted");
    assert_eq!(accepted["resolution"]["result_work_revision"], 2);
    assert_eq!(accepted["resolution"]["result_criteria_set_revision"], 2);

    let (retry_status, retry, _) = criteria_proposal_request(
        app.clone(),
        &owner_id,
        "PUT",
        work_id,
        branch_id,
        Some(&proposal_id),
        Some(decision),
    )
    .await;
    assert_eq!(retry_status, StatusCode::OK, "retry: {retry}");
    assert_eq!(retry, accepted, "exact retries return one resolution");

    let (changed_status, changed, _) = criteria_proposal_request(
        app,
        &owner_id,
        "PUT",
        work_id,
        branch_id,
        Some(&proposal_id),
        Some(serde_json::json!({
            "request_id": "reject-after-accept",
            "decision": "reject",
            "payload_hash": proposed.payload_hash.as_str(),
            "expected_work_revision": 1,
            "expected_goal_revision": 1,
            "expected_criteria_set_revision": 1,
            "expected_branch_revision": 1,
            "expected_graph_revision": 1
        })),
    )
    .await;
    assert_eq!(changed_status, StatusCode::CONFLICT, "changed: {changed}");
    assert_eq!(changed["code"], "work_proposal_already_resolved");

    cleanup_owner(&pool, &owner_id).await;
    cleanup_owner(&pool, &other_owner_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn work_transcript_is_bounded_committed_owner_scoped_and_causally_honest() {
    let Some((app, pool)) = setup().await else {
        return;
    };
    let owner_id = id("transcript-owner");
    let other_owner_id = id("transcript-other-owner");
    cleanup_owner(&pool, &owner_id).await;
    cleanup_owner(&pool, &other_owner_id).await;
    let (create_status, created) = post_work(
        app.clone(),
        &owner_id,
        serde_json::json!({
            "request_id": "start-for-transcript",
            "goal": "Read only committed bounded conversation history.",
            "criteria": []
        }),
    )
    .await;
    assert_eq!(create_status, StatusCode::CREATED, "created: {created}");
    let work_id = created["overview"]["work_id"].as_str().expect("work id");
    let branch_id = created["overview"]["delivery_branch"]["branch_id"]
        .as_str()
        .expect("branch id");
    let binding = DatabaseWorkRepository::new(pool.clone())
        .load_branch_runtime_binding(
            &WorkOwnerId::parse(owner_id.clone()).expect("owner"),
            &WorkId::parse(work_id).expect("work"),
            &WorkBranchId::parse(branch_id).expect("branch"),
        )
        .await
        .expect("runtime binding");
    let session_id = binding.session_id.as_str();
    let first_cursor = commit_test_conversation_turn(&pool, &owner_id, session_id, None, 1).await;

    sqlx::query(
        "INSERT INTO session_transcript_projection_heads
         (user_id, session_id, completed_turn, journal_event_seq, conversation_seq,
          canonical_root_hash, projection_schema, compaction_generation,
          config_version_id, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, NOW(6))",
    )
    .bind(&owner_id)
    .bind(session_id)
    .bind(i64::from(first_cursor.completed_turn))
    .bind(i64::try_from(first_cursor.journal_event_seq).unwrap())
    .bind(i64::try_from(first_cursor.conversation_seq).unwrap())
    .bind(&first_cursor.canonical_root_hash)
    .bind(i64::from(first_cursor.projection_schema))
    .bind(i64::try_from(first_cursor.compaction_generation).unwrap())
    .bind(&first_cursor.config_version_id)
    .execute(pool.get())
    .await
    .expect("insert transcript projection head");
    let large_content = "x".repeat(9_000);
    let large_payload = serde_json::json!({"detail": "y".repeat(17_000)}).to_string();
    for (item_seq, content, payload, committed_turn) in [
        (1_i64, "oldest".to_string(), None, Some(1_i64)),
        (2_i64, large_content, Some(large_payload), Some(1_i64)),
        (3_i64, "active and uncommitted".to_string(), None, None),
    ] {
        sqlx::query(
            "INSERT INTO session_transcript_items
             (session_id, item_seq, user_id, run_id, role, content, payload_json,
              source_event_id, source_event_idx, content_hash,
              canonical_completed_turn, canonical_conversation_seq,
              canonical_root_hash, created_at)
             VALUES (?, ?, ?, NULL, 'assistant', ?, ?, ?, NULL, ?, ?, ?, ?, NOW(6))",
        )
        .bind(session_id)
        .bind(item_seq)
        .bind(&owner_id)
        .bind(content)
        .bind(payload)
        .bind(format!("transcript-source-{item_seq}"))
        .bind(
            item_seq
                .to_string()
                .repeat(64)
                .chars()
                .take(64)
                .collect::<String>(),
        )
        .bind(committed_turn)
        .bind(committed_turn.map(|_| 1_i64))
        .bind(committed_turn.map(|_| first_cursor.canonical_root_hash.clone()))
        .execute(pool.get())
        .await
        .expect("insert transcript item");
    }

    let (status, latest, raw) =
        get_work_transcript(app.clone(), &owner_id, work_id, branch_id, "?limit=1").await;
    assert_eq!(status, StatusCode::OK, "latest: {raw}");
    assert_eq!(latest["sync"], "current");
    assert_eq!(latest["items"].as_array().unwrap().len(), 1);
    assert_eq!(latest["items"][0]["item_seq"], 2);
    assert_eq!(latest["items"][0]["content_truncated"], true);
    assert_eq!(latest["items"][0]["payload"], Value::Null);
    assert_eq!(latest["items"][0]["payload_omitted"], true);
    assert_eq!(latest["next_before_item_seq"], 2);
    assert_eq!(latest["has_more"], true);
    assert!(
        !raw.contains(session_id),
        "internal session identity leaked"
    );
    assert!(!raw.contains("active and uncommitted"));
    assert!(raw.len() < 64 * 1024, "bounded preview unexpectedly large");

    let (older_status, older, _) = get_work_transcript(
        app.clone(),
        &owner_id,
        work_id,
        branch_id,
        "?before_item_seq=2&limit=1",
    )
    .await;
    assert_eq!(older_status, StatusCode::OK);
    assert_eq!(older["items"][0]["item_seq"], 1);
    assert_eq!(older["has_more"], false);

    let second_cursor =
        commit_test_conversation_turn(&pool, &owner_id, session_id, Some(&first_cursor), 2).await;
    let (stale_status, stale, _) =
        get_work_transcript(app.clone(), &owner_id, work_id, branch_id, "?limit=10").await;
    assert_eq!(stale_status, StatusCode::OK);
    assert_eq!(stale["sync"], "projection_stale");
    assert_eq!(stale["canonical_head"]["completed_turn"], 2);
    assert_eq!(stale["transcript_cursor"]["completed_turn"], 1);
    assert_eq!(
        stale["canonical_head"]["canonical_root_hash"],
        second_cursor.canonical_root_hash
    );
    assert_eq!(stale["items"].as_array().unwrap().len(), 2);

    let (foreign_status, _, _) =
        get_work_transcript(app, &other_owner_id, work_id, branch_id, "").await;
    assert_eq!(foreign_status, StatusCode::NOT_FOUND);
    cleanup_owner(&pool, &owner_id).await;
    cleanup_owner(&pool, &other_owner_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn work_fork_is_exact_durable_owner_scoped_and_preserves_a_retained_prefix() {
    let Some((app, pool)) = setup().await else {
        return;
    };
    let owner_id = id("fork-owner");
    let other_owner_id = id("fork-other-owner");
    cleanup_owner(&pool, &owner_id).await;
    cleanup_owner(&pool, &other_owner_id).await;

    let (create_status, created) = post_work(
        app.clone(),
        &owner_id,
        serde_json::json!({
            "request_id": "start-for-fork",
            "goal": "Explore alternatives without changing the original conversation.",
            "criteria": []
        }),
    )
    .await;
    assert_eq!(create_status, StatusCode::CREATED, "created: {created}");
    let work_id = created["overview"]["work_id"].as_str().expect("work id");
    let origin_branch_id = created["overview"]["delivery_branch"]["branch_id"]
        .as_str()
        .expect("branch id");
    let binding = DatabaseWorkRepository::new(pool.clone())
        .load_branch_runtime_binding(
            &WorkOwnerId::parse(owner_id.clone()).expect("owner"),
            &WorkId::parse(work_id).expect("work"),
            &WorkBranchId::parse(origin_branch_id).expect("branch"),
        )
        .await
        .expect("runtime binding");
    let origin_session_id = binding.session_id.as_str();
    let first = commit_test_conversation_turn(&pool, &owner_id, origin_session_id, None, 1).await;
    let retained =
        commit_test_conversation_turn(&pool, &owner_id, origin_session_id, Some(&first), 2).await;
    let current =
        commit_test_conversation_turn(&pool, &owner_id, origin_session_id, Some(&retained), 3)
            .await;

    let fork_request = serde_json::json!({
        "request_id": "fork-retained-prefix",
        "expected_branch_revision": 1,
        "committed_cursor": public_work_cursor(&retained),
    });
    let (left, right) = tokio::join!(
        post_work_fork(
            app.clone(),
            &owner_id,
            work_id,
            origin_branch_id,
            fork_request.clone(),
        ),
        post_work_fork(
            app.clone(),
            &owner_id,
            work_id,
            origin_branch_id,
            fork_request.clone(),
        ),
    );
    for (status, response) in [&left, &right] {
        assert!(
            matches!(*status, StatusCode::CREATED | StatusCode::ACCEPTED),
            "fork: {response}"
        );
        if *status == StatusCode::CREATED {
            assert_eq!(response["state"], "succeeded");
            assert_eq!(response["outcome"], "created");
        } else {
            assert_eq!(response["state"], "pending");
            assert_eq!(response["outcome"], "pending");
        }
        assert_eq!(response["origin_branch_revision"], 1);
        assert_field_absent(response, "session_id");
    }
    assert_eq!(left.1["operation_id"], right.1["operation_id"]);
    assert_eq!(left.1["child_branch_id"], right.1["child_branch_id"]);
    assert!(
        left.0 == StatusCode::CREATED || right.0 == StatusCode::CREATED,
        "one executor must durably complete the operation"
    );
    let operation_id = left.1["operation_id"].as_str().expect("operation id");
    let (terminal_status, operation) = get_work_fork(
        app.clone(),
        &owner_id,
        work_id,
        origin_branch_id,
        operation_id,
    )
    .await;
    assert_eq!(terminal_status, StatusCode::OK);
    assert_eq!(operation["state"], "succeeded");
    assert_eq!(operation["outcome"], "created");
    let child_branch_id = operation["child_branch_id"].as_str().expect("child branch");

    let child = sqlx::query(
        "SELECT session_id, origin_branch_id, fork_cursor
         FROM work_branches
         WHERE owner_id = ? AND work_id = ? AND branch_id = ?",
    )
    .bind(&owner_id)
    .bind(work_id)
    .bind(child_branch_id)
    .fetch_one(pool.get())
    .await
    .expect("one visible child branch");
    assert_eq!(
        child.try_get::<String, _>("origin_branch_id").unwrap(),
        origin_branch_id
    );
    assert_eq!(
        child.try_get::<String, _>("fork_cursor").unwrap(),
        operation["fork_cursor"].as_str().unwrap()
    );
    let child_session_id = child.try_get::<String, _>("session_id").unwrap();
    let child_key = SessionKeyV1::owner_session(
        "server",
        &owner_id,
        &child_session_id,
        astra_turn_types::DEFAULT_CONVERSATION_BRANCH_ID,
    );
    let coordinator = DatabaseSessionContextCoordinator::new(pool.clone());
    let child_head = coordinator
        .load_head(&child_key)
        .await
        .expect("load child head")
        .expect("active child head");
    assert_eq!(child_head.key, child_key);
    assert_eq!(child_head.cursor.session_id, child_session_id);
    assert_eq!(child_head.cursor.completed_turn, retained.completed_turn);
    assert_eq!(
        child_head.cursor.journal_event_seq,
        retained.journal_event_seq
    );
    assert_eq!(
        child_head.cursor.conversation_seq,
        retained.conversation_seq
    );
    assert_eq!(
        child_head.cursor.canonical_root_hash,
        retained.canonical_root_hash
    );
    assert_eq!(
        child_head.cursor.compaction_generation,
        retained.compaction_generation
    );
    let child_conversation = coordinator
        .materialize(&child_head)
        .await
        .expect("materialize child prefix");
    assert_eq!(child_conversation.messages.len(), 2);
    assert_eq!(child_conversation.messages[0]["content"], "turn 1");
    assert_eq!(child_conversation.messages[1]["content"], "turn 2");
    assert!(
        !child_conversation
            .messages
            .iter()
            .any(|message| message["content"] == "turn 3"),
        "fork must not silently advance from its committed cursor"
    );
    let active_writer: Option<String> = sqlx::query_scalar(
        "SELECT active_writer_json FROM session_context_heads
         WHERE isolation_domain = 'server' AND owner_user_id = ?
           AND session_id = ? AND branch_id = 'main'",
    )
    .bind(&owner_id)
    .bind(&child_session_id)
    .fetch_one(pool.get())
    .await
    .expect("child authority");
    assert!(
        active_writer.is_none(),
        "new child must be idle after creation"
    );

    let (get_status, fetched) = get_work_fork(
        app.clone(),
        &owner_id,
        work_id,
        origin_branch_id,
        operation_id,
    )
    .await;
    assert_eq!(get_status, StatusCode::OK);
    assert_eq!(fetched, operation);
    let (foreign_status, foreign) = get_work_fork(
        app.clone(),
        &other_owner_id,
        work_id,
        origin_branch_id,
        operation_id,
    )
    .await;
    assert_eq!(foreign_status, StatusCode::NOT_FOUND);
    assert_eq!(foreign["code"], "work_fork_not_found");
    let (delete_status, delete_error) = delete_work_fork(
        app.clone(),
        &owner_id,
        work_id,
        origin_branch_id,
        operation_id,
    )
    .await;
    assert_eq!(delete_status, StatusCode::CONFLICT);
    assert_eq!(delete_error["code"], "work_fork_terminal");

    let mut reused = fork_request.clone();
    reused["committed_cursor"] = public_work_cursor(&current);
    let (reused_status, reused_error) =
        post_work_fork(app.clone(), &owner_id, work_id, origin_branch_id, reused).await;
    assert_eq!(reused_status, StatusCode::CONFLICT);
    assert_eq!(reused_error["code"], "idempotency_mismatch");

    let (revision_status, revision_conflict) = post_work_fork(
        app.clone(),
        &owner_id,
        work_id,
        origin_branch_id,
        serde_json::json!({
            "request_id": "fork-stale-branch",
            "expected_branch_revision": 2,
            "committed_cursor": public_work_cursor(&current),
        }),
    )
    .await;
    assert_eq!(revision_status, StatusCode::CREATED);
    assert_eq!(revision_conflict["state"], "conflict");
    assert_eq!(revision_conflict["outcome"], "branch_revision_conflict");

    let mut missing_cursor = current.clone();
    missing_cursor.canonical_root_hash = "f".repeat(64);
    let (cursor_status, cursor_conflict) = post_work_fork(
        app.clone(),
        &owner_id,
        work_id,
        origin_branch_id,
        serde_json::json!({
            "request_id": "fork-missing-cursor",
            "expected_branch_revision": 1,
            "committed_cursor": public_work_cursor(&missing_cursor),
        }),
    )
    .await;
    assert_eq!(cursor_status, StatusCode::CREATED);
    assert_eq!(cursor_conflict["state"], "conflict");
    assert_eq!(cursor_conflict["outcome"], "cursor_conflict");
    let pending: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM work_branch_creation_operations
         WHERE owner_id = ? AND work_id = ? AND operation_state = 'pending'",
    )
    .bind(&owner_id)
    .bind(work_id)
    .fetch_one(pool.get())
    .await
    .expect("pending creation count");
    assert_eq!(
        pending, 0,
        "deterministic conflicts must not consume capacity"
    );
    let branch_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM work_branches WHERE owner_id = ? AND work_id = ?")
            .bind(&owner_id)
            .bind(work_id)
            .fetch_one(pool.get())
            .await
            .expect("branch count");
    assert_eq!(
        branch_count, 2,
        "only the successful fork may become visible"
    );
    let (catalog_status, catalog, catalog_bytes) =
        get_work_branches(app.clone(), &owner_id, work_id).await;
    assert_eq!(catalog_status, StatusCode::OK, "catalog: {catalog}");
    assert!(catalog_bytes < 64 * 1024);
    assert_eq!(catalog["delivery_branch_id"], origin_branch_id);
    assert_eq!(catalog["branches"].as_array().unwrap().len(), 2);
    assert_eq!(catalog["branches"][0]["branch_id"], origin_branch_id);
    assert_eq!(catalog["branches"][0]["is_delivery"], true);
    assert_eq!(
        catalog["branches"][0]["materialization"],
        serde_json::Value::Null
    );
    assert_eq!(catalog["branches"][1]["branch_id"], child_branch_id);
    assert_eq!(catalog["branches"][1]["origin_branch_id"], origin_branch_id);
    assert_eq!(catalog["branches"][1]["is_delivery"], false);
    assert_eq!(
        catalog["branches"][1]["materialization"],
        serde_json::json!([
            {"dimension": "conversation", "disposition": "shared"},
            {"dimension": "goal", "disposition": "shared"},
            {"dimension": "criteria", "disposition": "shared"},
            {"dimension": "task_graph", "disposition": "shared"},
            {"dimension": "checkpoint", "disposition": "gap"},
            {"dimension": "workspace", "disposition": "gap"},
            {"dimension": "artifacts", "disposition": "gap"},
            {"dimension": "transient_authority", "disposition": "excluded"}
        ])
    );
    assert_field_absent(&catalog, "session_id");
    let comparison_request = serde_json::json!({
        "left_branch_id": origin_branch_id,
        "right_branch_id": child_branch_id,
    });
    let (comparison_status, comparison, comparison_bytes) =
        post_work_branch_comparison(app.clone(), &owner_id, work_id, comparison_request.clone())
            .await;
    assert_eq!(
        comparison_status,
        StatusCode::OK,
        "comparison: {comparison}"
    );
    assert!(comparison_bytes < 64 * 1024);
    assert_eq!(comparison["schema_version"], 2);
    assert_eq!(comparison["directly_comparable"], true);
    assert_eq!(comparison["blockers"], serde_json::json!([]));
    assert_eq!(comparison["graph_relation"], "same");
    assert_eq!(comparison["subject_relation"], "unavailable");
    assert_eq!(comparison["evidence_relation"], "same");
    assert_eq!(comparison["left"]["branch_id"], origin_branch_id);
    assert_eq!(comparison["left"]["is_delivery"], true);
    assert_eq!(comparison["right"]["branch_id"], child_branch_id);
    assert_eq!(comparison["right"]["is_delivery"], false);
    assert_eq!(comparison["left_evidence"]["required_count"], 0);
    assert_eq!(comparison["left_evidence"]["fresh_check_count"], 0);
    assert_eq!(comparison["right_evidence"]["required_count"], 0);
    assert_eq!(comparison["right_evidence"]["fresh_check_count"], 0);
    assert_eq!(
        comparison["coverage_gaps"],
        serde_json::json!(["change_details", "risks", "time_cost"])
    );
    assert_field_absent(&comparison, "session_id");
    let (same_status, same_error, _) = post_work_branch_comparison(
        app.clone(),
        &owner_id,
        work_id,
        serde_json::json!({
            "left_branch_id": origin_branch_id,
            "right_branch_id": origin_branch_id,
        }),
    )
    .await;
    assert_eq!(same_status, StatusCode::BAD_REQUEST);
    assert_eq!(same_error["code"], "invalid_work_branch_comparison_request");
    let (foreign_comparison_status, foreign_comparison, _) =
        post_work_branch_comparison(app.clone(), &other_owner_id, work_id, comparison_request)
            .await;
    assert_eq!(foreign_comparison_status, StatusCode::NOT_FOUND);
    assert_eq!(foreign_comparison["code"], "work_branch_not_found");

    let selection_request = serde_json::json!({
        "request_id": "select-fork-result",
        "expected_work_revision": comparison["work_revision"],
        "action": {
            "kind": "select_delivery_branch",
            "branch_id": child_branch_id,
            "expected_branch_revision": comparison["right"]["branch_revision"],
            "expected_goal_revision": comparison["right"]["goal_revision_ref"],
            "expected_criteria_set_revision": comparison["right"]["criteria"]["revision"],
            "expected_graph_revision": comparison["right"]["graph"]["current_revision"],
            "expected_subject": comparison["right"]["subject"],
            "expected_evidence_manifest_hash": comparison["right_evidence"]["manifest_hash"],
        }
    });
    let (selection_left, selection_right) = tokio::join!(
        post_work_action(app.clone(), &owner_id, work_id, selection_request.clone(),),
        post_work_action(app.clone(), &owner_id, work_id, selection_request.clone(),),
    );
    assert_eq!(
        selection_left.0,
        StatusCode::OK,
        "selection: {}",
        selection_left.1
    );
    assert_eq!(
        selection_right.0,
        StatusCode::OK,
        "selection: {}",
        selection_right.1
    );
    assert_eq!(
        selection_left.1, selection_right.1,
        "identical concurrent requests replay exactly"
    );
    assert_eq!(selection_left.1["schema_version"], 1);
    assert_eq!(selection_left.1["outcome"], "selected");
    assert_eq!(selection_left.1["delivery_branch_id"], child_branch_id);
    assert_eq!(selection_left.1["work_revision"], 2);
    assert_eq!(
        selection_left.1["evidence_manifest_hash"],
        comparison["right_evidence"]["manifest_hash"]
    );
    assert_field_absent(&selection_left.1, "session_id");
    let selection_event_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM work_events
         WHERE owner_id = ? AND work_id = ? AND event_kind = 'delivery_branch_selected'",
    )
    .bind(&owner_id)
    .bind(work_id)
    .fetch_one(pool.get())
    .await
    .expect("selection event count");
    assert_eq!(
        selection_event_count, 1,
        "concurrent replay emits one causal event"
    );

    let (replay_status, replay) =
        post_work_action(app.clone(), &owner_id, work_id, selection_request.clone()).await;
    assert_eq!(replay_status, StatusCode::OK);
    assert_eq!(replay, selection_left.1);
    let mut mismatched_selection = selection_request.clone();
    mismatched_selection["action"]["branch_id"] = serde_json::json!(origin_branch_id);
    let (mismatch_status, mismatch) =
        post_work_action(app.clone(), &owner_id, work_id, mismatched_selection).await;
    assert_eq!(mismatch_status, StatusCode::CONFLICT);
    assert_eq!(mismatch["code"], "idempotency_mismatch");
    let mut stale_selection = selection_request;
    stale_selection["request_id"] = serde_json::json!("select-stale-basis");
    let (stale_status, stale) =
        post_work_action(app.clone(), &owner_id, work_id, stale_selection).await;
    assert_eq!(stale_status, StatusCode::CONFLICT);
    assert_eq!(stale["code"], "work_delivery_selection_conflict");

    let (selected_catalog_status, selected_catalog, _) =
        get_work_branches(app.clone(), &owner_id, work_id).await;
    assert_eq!(selected_catalog_status, StatusCode::OK);
    assert_eq!(selected_catalog["work_revision"], 2);
    assert_eq!(selected_catalog["delivery_branch_id"], child_branch_id);
    assert_eq!(selected_catalog["branches"][0]["is_delivery"], false);
    assert_eq!(selected_catalog["branches"][1]["is_delivery"], true);
    let (selected_work_status, selected_work, _) = get_work(app.clone(), &owner_id, work_id).await;
    assert_eq!(selected_work_status, StatusCode::OK);
    assert_eq!(
        selected_work["overview"]["delivery_branch"]["branch_id"],
        child_branch_id
    );

    let archive_request = serde_json::json!({
        "request_id": "archive-origin-branch",
        "expected_work_revision": 2,
        "expected_branch_revision": 1,
        "action": { "kind": "archive" }
    });
    let (archive_status, archived) = post_work_branch_action(
        app.clone(),
        &owner_id,
        work_id,
        origin_branch_id,
        archive_request.clone(),
    )
    .await;
    assert_eq!(archive_status, StatusCode::OK, "archive: {archived}");
    assert_eq!(archived["kind"], "archive");
    assert_eq!(archived["outcome"], "applied");
    assert_eq!(archived["work_revision"], 3);
    assert_eq!(archived["branch_revision"], 2);
    let (archive_replay_status, archive_replay) = post_work_branch_action(
        app.clone(),
        &owner_id,
        work_id,
        origin_branch_id,
        archive_request,
    )
    .await;
    assert_eq!(archive_replay_status, StatusCode::OK);
    assert_eq!(archive_replay, archived);
    let (_, archived_catalog, _) = get_work_branches(app.clone(), &owner_id, work_id).await;
    assert_eq!(archived_catalog["work_revision"], 3);
    assert_eq!(archived_catalog["branches"].as_array().unwrap().len(), 1);
    assert_eq!(
        archived_catalog["branches"][0]["branch_id"],
        child_branch_id
    );
    let (archived_page_status, archived_page) =
        get_archived_work_branches(app.clone(), &owner_id, work_id, "?limit=1").await;
    assert_eq!(archived_page_status, StatusCode::OK);
    assert_eq!(archived_page["work_revision"], 3);
    assert_eq!(archived_page["branches"].as_array().unwrap().len(), 1);
    assert_eq!(archived_page["branches"][0]["branch_id"], origin_branch_id);
    assert_eq!(archived_page["next_cursor"], Value::Null);

    let (protected_status, protected) = post_work_branch_action(
        app.clone(),
        &owner_id,
        work_id,
        child_branch_id,
        serde_json::json!({
            "request_id": "archive-delivery-branch",
            "expected_work_revision": 3,
            "expected_branch_revision": 1,
            "action": { "kind": "archive" }
        }),
    )
    .await;
    assert_eq!(protected_status, StatusCode::CONFLICT);
    assert_eq!(protected["code"], "work_delivery_branch_protected");

    let (restore_status, restored) = post_work_branch_action(
        app.clone(),
        &owner_id,
        work_id,
        origin_branch_id,
        serde_json::json!({
            "request_id": "restore-origin-branch",
            "expected_work_revision": 3,
            "expected_branch_revision": 2,
            "action": { "kind": "restore" }
        }),
    )
    .await;
    assert_eq!(restore_status, StatusCode::OK, "restore: {restored}");
    assert_eq!(restored["kind"], "restore");
    assert_eq!(restored["outcome"], "applied");
    assert_eq!(restored["work_revision"], 4);
    assert_eq!(restored["branch_revision"], 3);
    let (_, restored_catalog, _) = get_work_branches(app.clone(), &owner_id, work_id).await;
    assert_eq!(restored_catalog["work_revision"], 4);
    assert_eq!(restored_catalog["branches"].as_array().unwrap().len(), 2);
    let (_, empty_archive) = get_archived_work_branches(app.clone(), &owner_id, work_id, "").await;
    assert_eq!(empty_archive["work_revision"], 4);
    assert_eq!(empty_archive["branches"], serde_json::json!([]));

    let (foreign_catalog_status, foreign_catalog, _) =
        get_work_branches(app.clone(), &other_owner_id, work_id).await;
    assert_eq!(foreign_catalog_status, StatusCode::NOT_FOUND);
    assert_eq!(foreign_catalog["code"], "work_not_found");
    for index in 0..30 {
        sqlx::query(
            "INSERT INTO work_branches
             (owner_id, work_id, branch_id, branch_revision, session_id,
              origin_branch_id, fork_cursor, goal_revision_ref,
              criteria_set_revision_ref, basis_graph_revision, current_graph_revision)
             SELECT owner_id, work_id, ?, 1, ?, branch_id, ?, goal_revision_ref,
                    criteria_set_revision_ref, current_graph_revision, current_graph_revision
             FROM work_branches
             WHERE owner_id = ? AND work_id = ? AND branch_id = ?",
        )
        .bind(format!("capacity-branch-{index}"))
        .bind(id("capacity-session"))
        .bind(format!("sha256:{index:064x}"))
        .bind(&owner_id)
        .bind(work_id)
        .bind(origin_branch_id)
        .execute(pool.get())
        .await
        .expect("fill bounded active branch capacity");
    }
    let (full_catalog_status, full_catalog, full_catalog_bytes) =
        get_work_branches(app.clone(), &owner_id, work_id).await;
    assert_eq!(
        full_catalog_status,
        StatusCode::OK,
        "catalog: {full_catalog}"
    );
    assert_eq!(full_catalog["branches"].as_array().unwrap().len(), 32);
    assert!(
        full_catalog_bytes < 64 * 1024,
        "the complete admitted catalog must remain a bounded response"
    );
    let (capacity_status, capacity_conflict) = post_work_fork(
        app.clone(),
        &owner_id,
        work_id,
        origin_branch_id,
        serde_json::json!({
            "request_id": "fork-over-capacity",
            "expected_branch_revision": 3,
            "committed_cursor": public_work_cursor(&current),
        }),
    )
    .await;
    assert_eq!(capacity_status, StatusCode::CREATED);
    assert_eq!(capacity_conflict["state"], "conflict");
    assert_eq!(capacity_conflict["outcome"], "capacity_exceeded");
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM work_branch_creation_operations
             WHERE owner_id = ? AND work_id = ? AND operation_state = 'pending'",
        )
        .bind(&owner_id)
        .bind(work_id)
        .fetch_one(pool.get())
        .await
        .expect("pending count at capacity"),
        0
    );

    sqlx::query(
        "DELETE FROM work_graph_revisions
         WHERE owner_id = ? AND work_id = ? AND revision = 1",
    )
    .bind(&owner_id)
    .bind(work_id)
    .execute(pool.get())
    .await
    .expect("remove referenced graph to prove repair classification");
    let (repair_status, repair_error, _) = post_work_branch_comparison(
        app,
        &owner_id,
        work_id,
        serde_json::json!({
            "left_branch_id": origin_branch_id,
            "right_branch_id": child_branch_id,
        }),
    )
    .await;
    assert_eq!(repair_status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(repair_error["code"], "work_branch_comparison_unavailable");

    cleanup_owner(&pool, &owner_id).await;
    cleanup_owner(&pool, &other_owner_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn work_fork_abort_recovers_a_prepared_but_invisible_child() {
    let Some((app, pool)) = setup().await else {
        return;
    };
    let owner = WorkOwnerId::parse(id("fork-abort-owner")).expect("owner");
    let owner_id = owner.as_str();
    cleanup_owner(&pool, owner_id).await;
    let (create_status, created) = post_work(
        app.clone(),
        owner_id,
        serde_json::json!({
            "request_id": "start-for-fork-abort",
            "goal": "Abort an alternative before it becomes visible.",
            "criteria": []
        }),
    )
    .await;
    assert_eq!(create_status, StatusCode::CREATED, "created: {created}");
    let work_id = WorkId::parse(created["overview"]["work_id"].as_str().unwrap()).unwrap();
    let origin_branch_id = WorkBranchId::parse(
        created["overview"]["delivery_branch"]["branch_id"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    let binding = DatabaseWorkRepository::new(pool.clone())
        .load_branch_runtime_binding(&owner, &work_id, &origin_branch_id)
        .await
        .expect("runtime binding");
    let cursor =
        commit_test_conversation_turn(&pool, owner_id, binding.session_id.as_str(), None, 1).await;
    let request = WorkBranchCreationRequest {
        request_id: "prepared-fork-abort".into(),
        owner_id: owner.clone(),
        work_id: work_id.clone(),
        origin_branch_id: origin_branch_id.clone(),
        expected_branch_revision: WorkBranchRevision::new(1).unwrap(),
        fork_cursor: ForkCursorRef::parse(format!("sha256:{}", "e".repeat(64))).unwrap(),
    };
    let service = DatabaseWorkBranchCreationService::new(pool.clone());
    let admission = service.admit(&request).await.expect("admit fork");
    let executor_token = service
        .claim_execution(
            &owner,
            &work_id,
            &origin_branch_id,
            &admission.operation.operation_id,
        )
        .await
        .expect("claim fork executor")
        .expect("executor token");
    let parent_key = SessionKeyV1::owner_session(
        "server",
        owner_id,
        admission.origin_session_id.as_str(),
        astra_turn_types::DEFAULT_CONVERSATION_BRANCH_ID,
    );
    let child_key = SessionKeyV1::owner_session(
        "server",
        owner_id,
        admission.child_session_id.as_str(),
        astra_turn_types::DEFAULT_CONVERSATION_BRANCH_ID,
    );
    let dimensions = [
        astra_turn_types::ForkBasisDimensionV1::Conversation,
        astra_turn_types::ForkBasisDimensionV1::TaskBoard,
        astra_turn_types::ForkBasisDimensionV1::Checkpoint,
        astra_turn_types::ForkBasisDimensionV1::Workspace,
        astra_turn_types::ForkBasisDimensionV1::Artifacts,
    ]
    .into_iter()
    .map(|dimension| {
        let conversation = dimension == astra_turn_types::ForkBasisDimensionV1::Conversation;
        astra_turn_types::ForkDimensionEvidenceV1 {
            dimension,
            disposition: if conversation {
                astra_turn_types::ForkDimensionDispositionV1::SharedPrefix
            } else {
                astra_turn_types::ForkDimensionDispositionV1::Gap
            },
            source_cursor: conversation.then(|| cursor.clone()),
            evidence_digest: conversation.then(|| cursor.canonical_root_hash.clone()),
            detail: (!conversation).then(|| "not materialized before abort".into()),
        }
    })
    .collect();
    let context: Arc<dyn SessionContextCoordinator> =
        Arc::new(DatabaseSessionContextCoordinator::new(pool.clone()));
    let fork_coordinator = DatabaseSessionForkCoordinator::new(pool.clone(), context);
    let manifest = fork_coordinator
        .prepare(&PrepareSessionForkV1 {
            idempotency_key: format!("work-fork:{}", admission.operation.operation_id),
            parent_key,
            child_key,
            expected_parent_cursor: cursor,
            dimensions,
            reason: "work_alternative_branch".into(),
        })
        .await
        .expect("prepare session fork");
    service
        .record_session_fork(
            &request,
            &admission.operation.operation_id,
            &executor_token,
            &manifest.fork_id,
        )
        .await
        .expect("bind prepared session fork");
    sqlx::query(
        "UPDATE work_branch_creation_operations
         SET executor_lease_expires_at = DATE_SUB(NOW(6), INTERVAL 1 SECOND)
         WHERE owner_id = ? AND work_id = ? AND origin_branch_id = ?
           AND operation_id = ? AND executor_token = ?",
    )
    .bind(owner.as_str())
    .bind(work_id.as_str())
    .bind(origin_branch_id.as_str())
    .bind(&admission.operation.operation_id)
    .bind(&executor_token)
    .execute(pool.get())
    .await
    .expect("simulate executor crash after prepare");
    let visible_before_abort: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM work_branches
         WHERE owner_id = ? AND work_id = ? AND branch_id = ?",
    )
    .bind(owner_id)
    .bind(work_id.as_str())
    .bind(admission.operation.child_branch_id.as_str())
    .fetch_one(pool.get())
    .await
    .expect("child visibility");
    assert_eq!(visible_before_abort, 0);

    let (abort_status, abort_body) = delete_work_fork(
        app.clone(),
        owner_id,
        work_id.as_str(),
        origin_branch_id.as_str(),
        &admission.operation.operation_id,
    )
    .await;
    assert_eq!(abort_status, StatusCode::NO_CONTENT, "abort: {abort_body}");
    let (retry_status, _) = delete_work_fork(
        app,
        owner_id,
        work_id.as_str(),
        origin_branch_id.as_str(),
        &admission.operation.operation_id,
    )
    .await;
    assert_eq!(retry_status, StatusCode::NO_CONTENT);
    let stored = service
        .load(
            &owner,
            &work_id,
            &origin_branch_id,
            &admission.operation.operation_id,
        )
        .await
        .expect("aborted Work fork");
    assert_eq!(
        stored.operation.state,
        astra_services::work::WorkBranchCreationState::Aborted
    );
    let session_fork_state: String = sqlx::query_scalar(
        "SELECT state FROM session_forks
         WHERE isolation_domain = 'server' AND owner_user_id = ? AND fork_id = ?",
    )
    .bind(owner_id)
    .bind(&manifest.fork_id)
    .fetch_one(pool.get())
    .await
    .expect("aborted session fork");
    assert_eq!(session_fork_state, "aborted");
    let visible_after_abort: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM work_branches
         WHERE owner_id = ? AND work_id = ? AND branch_id = ?",
    )
    .bind(owner_id)
    .bind(work_id.as_str())
    .bind(admission.operation.child_branch_id.as_str())
    .fetch_one(pool.get())
    .await
    .expect("child visibility after abort");
    assert_eq!(visible_after_abort, 0);
    cleanup_owner(&pool, owner_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn work_turn_route_binds_server_runtime_without_exposing_internal_session() {
    let lifecycle = Arc::new(WorkTurnRecordingLifecycle::default());
    let Some((app, pool)) = setup_with_run_lifecycle(lifecycle.clone()).await else {
        return;
    };
    let owner_id = id("owner");
    let other_owner_id = id("other-owner");
    cleanup_owner(&pool, &owner_id).await;
    cleanup_owner(&pool, &other_owner_id).await;
    let (create_status, created) = post_work(
        app.clone(),
        &owner_id,
        serde_json::json!({
            "request_id": "start-for-turn",
            "goal": "Continue through the public Work branch boundary.",
            "criteria": []
        }),
    )
    .await;
    assert_eq!(create_status, StatusCode::CREATED, "created: {created}");
    let work_id = created["overview"]["work_id"].as_str().expect("work id");
    let branch_id = created["overview"]["delivery_branch"]["branch_id"]
        .as_str()
        .expect("branch id");
    let (first_attach_status, first_attachment) = attach_work_branch(
        app.clone(),
        &owner_id,
        work_id,
        branch_id,
        "turn-controller-1",
    )
    .await;
    assert_eq!(first_attach_status, StatusCode::OK);
    let first_attachment_id = first_attachment["attachment_id"]
        .as_str()
        .expect("first attachment id");
    let (second_attach_status, second_attachment) = attach_work_branch(
        app.clone(),
        &owner_id,
        work_id,
        branch_id,
        "turn-controller-2",
    )
    .await;
    assert_eq!(second_attach_status, StatusCode::OK);
    let second_attachment_id = second_attachment["attachment_id"]
        .as_str()
        .expect("second attachment id");

    let first_turn = post_work_turn(
        app.clone(),
        &owner_id,
        work_id,
        branch_id,
        serde_json::json!({
            "request_id": "continue-1",
            "attachment_id": first_attachment_id,
            "message": "Continue from the current Work facts."
        }),
    );
    let second_turn = post_work_turn(
        app.clone(),
        &owner_id,
        work_id,
        branch_id,
        serde_json::json!({
            "request_id": "continue-2",
            "attachment_id": second_attachment_id,
            "message": "Continue from the other current reader."
        }),
    );
    let (first_result, second_result) = tokio::join!(first_turn, second_turn);
    let first_won = first_result.0 == StatusCode::OK;
    assert_eq!(
        usize::from(first_result.0 == StatusCode::OK)
            + usize::from(second_result.0 == StatusCode::OK),
        1,
        "exactly one concurrent attachment may enter the runtime: first={:?}; second={:?}",
        first_result.0,
        second_result.0
    );
    assert_eq!(
        usize::from(first_result.0 == StatusCode::CONFLICT)
            + usize::from(second_result.0 == StatusCode::CONFLICT),
        1,
        "the competing attachment must receive a typed conflict: first={:?}; second={:?}",
        first_result.0,
        second_result.0
    );
    let (turn_status, events, raw, winning_message, conflict_status, conflict_raw) = if first_won {
        (
            first_result.0,
            first_result.1,
            first_result.2,
            "Continue from the current Work facts.",
            second_result.0,
            second_result.2,
        )
    } else {
        (
            second_result.0,
            second_result.1,
            second_result.2,
            "Continue from the other current reader.",
            first_result.0,
            first_result.2,
        )
    };

    assert_eq!(turn_status, StatusCode::OK, "turn response: {raw}");
    assert_eq!(events[0]["type"], "work_turn_started");
    assert_eq!(events[0]["schema_version"], 1);
    assert_eq!(events[0]["work_id"], work_id);
    assert_eq!(events[0]["branch_id"], branch_id);
    assert_eq!(events[1]["type"], "run_started");
    assert_eq!(events[2]["type"], "text_delta");
    assert_eq!(events[3]["type"], "run_finished");
    for event in &events {
        assert_field_absent(event, "session_id");
    }

    {
        let requests = lifecycle.requests.lock().expect("recorded request");
        assert_eq!(
            requests.len(),
            1,
            "the losing controller must be rejected before runtime admission"
        );
        let (recorded_owner, request) = &requests[0];
        assert_eq!(recorded_owner, &owner_id);
        assert_eq!(request.message, winning_message);
        assert_eq!(
            request.model_selection_mode,
            ModelSelectionMode::ServerDefault
        );
        assert!(request.model_selection.is_none());
        assert!(request.resolved_model_selection.is_none());
        let work_binding = request.work_binding.as_ref().expect("runtime Work binding");
        assert_eq!(work_binding.work_id, work_id);
        assert_eq!(work_binding.branch_id, branch_id);
        let start = request
            .run_start_idempotency
            .as_ref()
            .expect("exact run start");
        assert_eq!(start.kind(), RunStartIdempotencyKind::WorkTurn);
        assert_eq!(events[0]["run_id"], start.run_id());
        let item = work_binding.item.as_ref().expect("root WorkItem attempt");
        assert_eq!(item.item_id, "root");
        assert_eq!(item.item_revision, 1);
        assert_eq!(item.attempt_id, start.run_id());
        let internal_session_id = request.session_id.as_deref().expect("internal session");
        assert!(!raw.contains(internal_session_id));
    }

    assert_eq!(conflict_status, StatusCode::CONFLICT);
    let conflict: Value = serde_json::from_str(&conflict_raw).expect("typed conflict");
    assert_eq!(conflict["code"], "writer_conflict");
    assert_eq!(
        lifecycle.requests.lock().expect("requests").len(),
        1,
        "controller conflict must stop before runtime admission"
    );

    let (other_status, other_events, other_raw) = post_work_turn(
        app,
        &other_owner_id,
        work_id,
        branch_id,
        serde_json::json!({
            "request_id": "continue-1",
            "attachment_id": first_attachment_id,
            "message": "Attempt another owner's Work."
        }),
    )
    .await;
    assert_eq!(other_status, StatusCode::NOT_FOUND);
    assert!(other_events.is_empty());
    let error: Value = serde_json::from_str(&other_raw).expect("typed Work error");
    assert_eq!(error["code"], "branch_not_found");
    assert_eq!(lifecycle.requests.lock().expect("requests").len(), 1);

    cleanup_owner(&pool, &owner_id).await;
    cleanup_owner(&pool, &other_owner_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn task_graph_route_is_bounded_owner_scoped_and_revision_pinned() {
    let Some((app, pool)) = setup().await else {
        return;
    };
    let owner_id = id("owner");
    let other_owner_id = id("other-owner");
    cleanup_owner(&pool, &owner_id).await;
    cleanup_owner(&pool, &other_owner_id).await;
    let (create_status, created) = post_work(
        app.clone(),
        &owner_id,
        serde_json::json!({
            "request_id": "start-for-task-graph",
            "goal": "Expose one bounded canonical Task Graph page.",
            "criteria": []
        }),
    )
    .await;
    assert_eq!(create_status, StatusCode::CREATED, "created: {created}");
    let work_id = created["overview"]["work_id"].as_str().expect("work id");
    let branch_id = created["overview"]["delivery_branch"]["branch_id"]
        .as_str()
        .expect("branch id");

    let (status, page, page_bytes) = get_work_task_graph(
        app.clone(),
        &owner_id,
        work_id,
        branch_id,
        "?item_limit=1&dependency_limit=1",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "page: {page}");
    assert!(page_bytes < 64 * 1024, "genesis page stays shell-sized");
    assert_eq!(page["schema_version"], 2);
    assert_eq!(page["scope"], "declared_work");
    assert_eq!(page["basis"]["work_id"], work_id);
    assert_eq!(page["basis"]["branch_id"], branch_id);
    assert_eq!(page["basis"]["graph_item_count"], 1);
    assert_eq!(page["items"]["entries"][0]["item_id"], "root");
    assert_eq!(
        page["items"]["entries"][0]["execution"],
        serde_json::json!({
            "status": "not_started",
            "terminal": false,
            "run": null
        })
    );
    assert_eq!(
        page["items"]["entries"][0]["delivery"],
        serde_json::json!({
            "status": "unreported",
            "summary": null,
            "blocker_kind": null,
            "unavailable_capabilities": []
        })
    );
    assert_eq!(page["items"]["entries"].as_array().map(Vec::len), Some(1));
    assert_eq!(page["next_cursor"], Value::Null);
    assert_field_absent(&page, "session_id");

    let old_run_id = id("old-attempt");
    let latest_run_id = id("latest-attempt");
    for (run_id, parent_run_id, root_run_id, depth, status, created_at) in [
        (
            &old_run_id,
            None,
            old_run_id.as_str(),
            0,
            "completed",
            "2026-08-01 00:00:00.000000",
        ),
        (
            &latest_run_id,
            Some("parent-run"),
            "parent-run",
            1,
            "waiting",
            "2026-08-01 00:00:01.000000",
        ),
    ] {
        sqlx::query(
            "INSERT INTO agent_runs
             (run_id, user_id, session_id, parent_run_id, root_run_id, ancestor_path, depth, status,
              work_id, work_branch_id, work_graph_revision,
              work_item_id, work_item_revision, work_item_attempt_id,
              run_generation, last_event_idx, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1, 'root', 1, ?, 2, 4, ?, ?)",
        )
        .bind(run_id)
        .bind(&owner_id)
        .bind(id("internal-session"))
        .bind(parent_run_id)
        .bind(root_run_id)
        .bind(if depth == 0 {
            run_id.to_string()
        } else {
            format!("{root_run_id}/{run_id}")
        })
        .bind(depth)
        .bind(status)
        .bind(work_id)
        .bind(branch_id)
        .bind(run_id)
        .bind(created_at)
        .bind(created_at)
        .execute(pool.get())
        .await
        .expect("insert exact root WorkItem attempt");
        sqlx::query(
            "INSERT INTO work_item_attempts
             (owner_id, work_id, branch_id, work_item_id, work_item_revision,
              attempt_id, executor_run_id, execution_mode, status, graph_revision,
              run_generation, last_event_idx, unavailable_capabilities_json,
              started_at, updated_at)
             VALUES (?, ?, ?, 'root', 1, ?, ?, 'primary', ?, 1, 2, 4, '[]', ?, ?)",
        )
        .bind(&owner_id)
        .bind(work_id)
        .bind(branch_id)
        .bind(run_id)
        .bind(run_id)
        .bind(status)
        .bind(created_at)
        .bind(created_at)
        .execute(pool.get())
        .await
        .expect("insert canonical WorkItem attempt projection");
    }
    let (status, executing, _) =
        get_work_task_graph(app.clone(), &owner_id, work_id, branch_id, "").await;
    assert_eq!(status, StatusCode::OK, "executing page: {executing}");
    let execution = &executing["items"]["entries"][0]["execution"];
    assert_eq!(execution["status"], "waiting");
    assert_eq!(execution["terminal"], false);
    assert_eq!(execution["run"]["run_id"], latest_run_id);
    assert_eq!(execution["run"]["attempt_id"], latest_run_id);
    assert_eq!(execution["run"]["graph_revision"], 1);
    assert_eq!(execution["run"]["run_generation"], 2);
    assert_eq!(execution["run"]["last_event_idx"], 4);

    sqlx::query(
        "UPDATE agent_runs
         SET status = 'completed', run_generation = 3, last_event_idx = 6,
             updated_at = '2026-08-01 00:00:02.000000'
         WHERE user_id = ? AND run_id = ?",
    )
    .bind(&owner_id)
    .bind(&latest_run_id)
    .execute(pool.get())
    .await
    .expect("advance durable Run fact");
    sqlx::query(
        "UPDATE work_item_attempts
         SET status = 'completed', run_generation = 3, last_event_idx = 6,
             updated_at = '2026-08-01 00:00:02.000000'
         WHERE owner_id = ? AND attempt_id = ?",
    )
    .bind(&owner_id)
    .bind(&latest_run_id)
    .execute(pool.get())
    .await
    .expect("advance canonical WorkItem attempt projection");
    let (status, terminal, _) =
        get_work_task_graph(app.clone(), &owner_id, work_id, branch_id, "").await;
    assert_eq!(status, StatusCode::OK, "terminal page: {terminal}");
    let execution = &terminal["items"]["entries"][0]["execution"];
    assert_eq!(execution["status"], "completed");
    assert_eq!(execution["terminal"], true);
    assert_eq!(execution["run"]["run_generation"], 3);
    assert_eq!(execution["run"]["last_event_idx"], 6);

    let criterion_id = id("criterion");
    let repository = DatabaseWorkRepository::new(pool.clone());
    repository
        .accept_criteria(WorkCriteriaChange {
            owner_id: WorkOwnerId::parse(&owner_id).expect("owner"),
            work_id: WorkId::parse(work_id).expect("work"),
            expected_work_revision: WorkRevision::INITIAL,
            expected_criteria_set_revision: CriterionSetRevision::INITIAL,
            members: vec![CriterionSetMemberChange::New(NewWorkCriterion {
                criterion_id: CriterionId::parse(&criterion_id).expect("criterion"),
                definition: CriterionDefinition::TestCheck {
                    statement: astra_services::work::CriterionStatement::parse(
                        "The exact subject passes its registered verifier.",
                    )
                    .expect("statement"),
                    command: CriterionCommand::parse("cargo test -p example exact_test")
                        .expect("command"),
                },
            })],
            source_ref: WorkChangeRef::parse(id("criteria-source")).expect("source"),
            reason: None,
        })
        .await
        .expect("accept typed criterion");
    repository
        .adopt_branch_basis(WorkBranchBasisChange {
            owner_id: WorkOwnerId::parse(&owner_id).expect("owner"),
            work_id: WorkId::parse(work_id).expect("work"),
            branch_id: WorkBranchId::parse(branch_id).expect("branch"),
            expected_work_revision: WorkRevision::new(2).expect("work r2"),
            expected_branch_revision: WorkBranchRevision::INITIAL,
            expected_goal_revision: astra_services::work::GoalRevision::INITIAL,
            expected_criteria_set_revision: CriterionSetRevision::INITIAL,
            target_goal_revision: astra_services::work::GoalRevision::INITIAL,
            target_criteria_set_revision: CriterionSetRevision::new(2).expect("set r2"),
            source_ref: WorkChangeRef::parse(id("basis-source")).expect("source"),
        })
        .await
        .expect("adopt current typed criterion set");
    let subject_ref = WorkSubjectRef::parse("workspace/repository/head").expect("subject");
    repository
        .set_branch_subject(WorkBranchSubjectChange {
            owner_id: WorkOwnerId::parse(&owner_id).expect("owner"),
            work_id: WorkId::parse(work_id).expect("work"),
            branch_id: WorkBranchId::parse(branch_id).expect("branch"),
            expected_branch_revision: WorkBranchRevision::new(2).expect("branch r2"),
            graph_revision: GraphRevision::INITIAL,
            subject_ref: subject_ref.clone(),
            subject_revision: hash('c'),
            source_ref: WorkChangeRef::parse(id("subject-source")).expect("source"),
        })
        .await
        .expect("establish current subject");
    let now = chrono::Utc::now();
    let expired_check_run_id = id("expired-check");
    let mut check = NewWorkCheckRun {
        owner_id: WorkOwnerId::parse(&owner_id).expect("owner"),
        work_id: WorkId::parse(work_id).expect("work"),
        branch_id: WorkBranchId::parse(branch_id).expect("branch"),
        check_run_id: CheckRunId::parse(&expired_check_run_id).expect("check"),
        graph_revision: GraphRevision::INITIAL,
        item: WorkItemRevisionRef {
            item_id: WorkItemId::root(),
            revision: WorkItemRevision::INITIAL,
        },
        attempt_id: WorkItemAttemptId::parse(&latest_run_id).expect("attempt"),
        criterion_set_revision: CriterionSetRevision::new(2).expect("set r2"),
        criterion: CriterionRevisionRef {
            criterion_id: CriterionId::parse(&criterion_id).expect("criterion"),
            revision: CriterionRevision::INITIAL,
        },
        subject_ref: subject_ref.clone(),
        subject_revision: hash('c'),
        artifact_digest: Some(hash('d')),
        run_ref: WorkChangeRef::parse(&latest_run_id).expect("run"),
        invocation_ref: WorkChangeRef::parse(id("invocation")).expect("invocation"),
        verifier_kind: CheckVerifierKind::Test,
        verifier_fingerprint: hash('e'),
        environment_fingerprint: hash('f'),
        outcome: CheckOutcome::Passed,
        error_kind: None,
        coverage: CheckCoverage::Complete,
        coverage_gaps: Vec::new(),
        evidence_refs: vec![
            CheckEvidenceRef::parse("urn:astra:artifact:cloud:check/result").expect("evidence"),
        ],
        source_cursor: WorkChangeRef::parse(id("expired-check-cursor")).expect("cursor"),
        produced_at: now - chrono::Duration::minutes(2),
        expires_at: Some(now - chrono::Duration::minutes(1)),
    };
    repository
        .record_check_run(check.clone())
        .await
        .expect("record expired check fact");
    let (status, expired, _) =
        get_work_task_graph(app.clone(), &owner_id, work_id, branch_id, "").await;
    assert_eq!(status, StatusCode::OK, "expired page: {expired}");
    let verification = &expired["items"]["entries"][0]["verification"];
    assert_eq!(verification["status"], "stale_evidence");
    assert_eq!(verification["latest_check"]["freshness"], "expired");

    let check_run_id = id("check");
    check.check_run_id = CheckRunId::parse(&check_run_id).expect("check");
    check.source_cursor = WorkChangeRef::parse(id("check-cursor")).expect("cursor");
    check.produced_at = now;
    check.expires_at = None;
    repository
        .record_check_run(check.clone())
        .await
        .expect("record current check fact");
    let (status, checked, _) =
        get_work_task_graph(app.clone(), &owner_id, work_id, branch_id, "").await;
    assert_eq!(status, StatusCode::OK, "checked page: {checked}");
    let verification = &checked["items"]["entries"][0]["verification"];
    assert_eq!(verification["status"], "evidence_available");
    assert_eq!(verification["latest_check"]["check_run_id"], check_run_id);
    assert_eq!(verification["latest_check"]["outcome"], "passed");
    assert_eq!(verification["latest_check"]["freshness"], "current");

    let comparison_branch_id = id("evidence-comparison-branch");
    sqlx::query(
        "INSERT INTO work_branches
         (owner_id, work_id, branch_id, branch_revision, session_id,
          origin_branch_id, fork_cursor, goal_revision_ref,
          criteria_set_revision_ref, basis_graph_revision, current_graph_revision)
         SELECT owner_id, work_id, ?, 1, ?, branch_id, ?, goal_revision_ref,
                criteria_set_revision_ref, current_graph_revision, current_graph_revision
         FROM work_branches
         WHERE owner_id = ? AND work_id = ? AND branch_id = ?",
    )
    .bind(&comparison_branch_id)
    .bind(id("evidence-comparison-session"))
    .bind(format!("sha256:{}", "9".repeat(64)))
    .bind(&owner_id)
    .bind(work_id)
    .bind(branch_id)
    .execute(pool.get())
    .await
    .expect("insert an evidence-free comparison branch");
    let comparison_request = serde_json::json!({
        "left_branch_id": branch_id,
        "right_branch_id": comparison_branch_id,
    });
    let (comparison_status, comparison, _) =
        post_work_branch_comparison(app.clone(), &owner_id, work_id, comparison_request.clone())
            .await;
    assert_eq!(
        comparison_status,
        StatusCode::OK,
        "comparison: {comparison}"
    );
    assert_eq!(comparison["left_evidence"]["required_count"], 1);
    assert_eq!(comparison["left_evidence"]["fresh_check_count"], 1);
    assert_eq!(comparison["right_evidence"]["required_count"], 1);
    assert_eq!(comparison["right_evidence"]["fresh_check_count"], 0);
    assert_eq!(comparison["evidence_relation"], "different");
    assert!(
        !comparison["coverage_gaps"]
            .as_array()
            .expect("coverage gaps")
            .iter()
            .any(|gap| gap == "fresh_checks"),
        "fresh checks are now exact comparison facts"
    );

    // A newer check does not advance Work or branch revisions, so this proves
    // the evidence manifest itself participates in selection admission.
    check.check_run_id = CheckRunId::parse(id("newer-check")).expect("check");
    check.source_cursor = WorkChangeRef::parse(id("newer-check-cursor")).expect("cursor");
    check.produced_at = now + chrono::Duration::seconds(1);
    repository
        .record_check_run(check)
        .await
        .expect("record a newer exact evidence fact");
    let (evidence_drift_status, evidence_drift) = post_work_action(
        app.clone(),
        &owner_id,
        work_id,
        serde_json::json!({
            "request_id": "select-with-stale-evidence",
            "expected_work_revision": comparison["work_revision"],
            "action": {
                "kind": "select_delivery_branch",
                "branch_id": branch_id,
                "expected_branch_revision": comparison["left"]["branch_revision"],
                "expected_goal_revision": comparison["left"]["goal_revision_ref"],
                "expected_criteria_set_revision": comparison["left"]["criteria"]["revision"],
                "expected_graph_revision": comparison["left"]["graph"]["current_revision"],
                "expected_subject": comparison["left"]["subject"],
                "expected_evidence_manifest_hash": comparison["left_evidence"]["manifest_hash"],
            }
        }),
    )
    .await;
    assert_eq!(evidence_drift_status, StatusCode::CONFLICT);
    assert_eq!(evidence_drift["code"], "work_delivery_selection_conflict");
    let delivery_after_conflict: String = sqlx::query_scalar(
        "SELECT delivery_branch_id FROM works WHERE owner_id = ? AND work_id = ?",
    )
    .bind(&owner_id)
    .bind(work_id)
    .fetch_one(pool.get())
    .await
    .expect("delivery identity after evidence conflict");
    assert_eq!(delivery_after_conflict, branch_id);

    repository
        .set_branch_subject(WorkBranchSubjectChange {
            owner_id: WorkOwnerId::parse(&owner_id).expect("owner"),
            work_id: WorkId::parse(work_id).expect("work"),
            branch_id: WorkBranchId::parse(branch_id).expect("branch"),
            expected_branch_revision: WorkBranchRevision::new(3).expect("branch r3"),
            graph_revision: GraphRevision::INITIAL,
            subject_ref,
            subject_revision: hash('a'),
            source_ref: WorkChangeRef::parse(id("subject-advanced")).expect("source"),
        })
        .await
        .expect("advance subject");
    let (status, stale_evidence, _) =
        get_work_task_graph(app.clone(), &owner_id, work_id, branch_id, "").await;
    assert_eq!(status, StatusCode::OK, "stale page: {stale_evidence}");
    let verification = &stale_evidence["items"]["entries"][0]["verification"];
    assert_eq!(verification["status"], "stale_evidence");
    assert_eq!(verification["latest_check"]["freshness"], "subject_changed");
    let (stale_comparison_status, stale_comparison, _) =
        post_work_branch_comparison(app.clone(), &owner_id, work_id, comparison_request).await;
    assert_eq!(stale_comparison_status, StatusCode::OK);
    assert_eq!(stale_comparison["left_evidence"]["fresh_check_count"], 0);
    assert_eq!(stale_comparison["right_evidence"]["fresh_check_count"], 0);
    assert_eq!(stale_comparison["evidence_relation"], "same");

    let (stale_status, stale, _) = get_work_task_graph(
        app.clone(),
        &owner_id,
        work_id,
        branch_id,
        "?graph_revision=2",
    )
    .await;
    assert_eq!(stale_status, StatusCode::CONFLICT);
    assert_eq!(stale["code"], "work_graph_revision_conflict");

    let (unpinned_status, unpinned, _) =
        get_work_task_graph(app.clone(), &owner_id, work_id, branch_id, "?item_offset=1").await;
    assert_eq!(unpinned_status, StatusCode::BAD_REQUEST);
    assert_eq!(unpinned["code"], "invalid_work_task_graph_query");

    let (other_status, other, _) =
        get_work_task_graph(app, &other_owner_id, work_id, branch_id, "").await;
    assert_eq!(other_status, StatusCode::NOT_FOUND);
    assert_eq!(other["code"], "branch_not_found");

    cleanup_owner(&pool, &owner_id).await;
    cleanup_owner(&pool, &other_owner_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn work_session_binding_bootstraps_public_identity_without_cross_owner_leakage() {
    let Some((app, pool)) = setup().await else {
        return;
    };
    let owner_id = id("binding-owner");
    let other_owner_id = id("binding-other-owner");
    let work_id = id("binding-work");
    let branch_id = id("binding-branch");
    let session_id = id("binding-session");
    cleanup_owner(&pool, &owner_id).await;
    cleanup_owner(&pool, &other_owner_id).await;
    DatabaseWorkRepository::new(pool.clone())
        .create_genesis(
            WorkGenesis::new(WorkGenesisParts {
                owner_id: WorkOwnerId::parse(&owner_id).expect("owner"),
                work_id: WorkId::parse(&work_id).expect("work"),
                branch_id: WorkBranchId::parse(&branch_id).expect("branch"),
                session_id: InternalSessionId::parse(&session_id).expect("session"),
                project_id: None,
                original_intent_ref: OriginalIntentRef::parse(id("binding-intent"))
                    .expect("intent"),
                goal: WorkGoal::parse("Resolve one canonical Work binding.").expect("goal"),
                criteria: Vec::new(),
            })
            .expect("Work genesis"),
        )
        .await
        .expect("genesis");

    let (status, binding) = get_work_session_binding(app.clone(), &owner_id, &session_id).await;
    assert_eq!(status, StatusCode::OK, "binding: {binding}");
    assert_eq!(
        binding,
        serde_json::json!({
            "schema_version": 1,
            "work_id": work_id,
            "branch_id": branch_id,
            "graph_revision": 1
        })
    );

    let (foreign_status, foreign) =
        get_work_session_binding(app, &other_owner_id, &session_id).await;
    assert_eq!(foreign_status, StatusCode::NOT_FOUND);
    assert_eq!(foreign["code"], "work_session_binding_not_found");

    cleanup_owner(&pool, &owner_id).await;
    cleanup_owner(&pool, &other_owner_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn existing_session_promotion_is_atomic_idle_only_and_exactly_idempotent() {
    let Some((app, pool)) = setup().await else {
        return;
    };
    let owner_id = id("promote-owner");
    let other_owner_id = id("promote-other-owner");
    let session_id = id("promote-session");
    let busy_session_id = id("promote-busy-session");
    let orphan_run_session_id = id("promote-orphan-run-session");
    let closed_session_id = id("promote-closed-session");
    cleanup_owner(&pool, &owner_id).await;
    cleanup_owner(&pool, &other_owner_id).await;
    for (session, status) in [
        (&session_id, "active"),
        (&busy_session_id, "active"),
        (&orphan_run_session_id, "active"),
        (&closed_session_id, "closed"),
    ] {
        sqlx::query(
            "INSERT INTO agent_sessions
             (user_id, session_id, status, event_count, project_id)
             VALUES (?, ?, ?, 0, 'project-promoted')",
        )
        .bind(&owner_id)
        .bind(session)
        .bind(status)
        .execute(pool.get())
        .await
        .expect("existing session fixture");
    }
    sqlx::query(
        "INSERT INTO agent_session_execution_slots (user_id, session_id, run_id)
         VALUES (?, ?, 'active-run')",
    )
    .bind(&owner_id)
    .bind(&busy_session_id)
    .execute(pool.get())
    .await
    .expect("active run fixture");
    sqlx::query(
        "INSERT INTO agent_runs
         (run_id, user_id, session_id, root_run_id, ancestor_path, depth, status)
         VALUES ('active-run', ?, ?, 'active-run', 'active-run', 0, 'running')",
    )
    .bind(&owner_id)
    .bind(&busy_session_id)
    .execute(pool.get())
    .await
    .expect("active durable run fixture");
    sqlx::query(
        "INSERT INTO agent_session_execution_slots (user_id, session_id, run_id)
         VALUES (?, ?, 'missing-durable-run')",
    )
    .bind(&owner_id)
    .bind(&orphan_run_session_id)
    .execute(pool.get())
    .await
    .expect("orphan execution slot fixture");

    let request = serde_json::json!({
        "request_id": "promote-current-session",
        "goal": "Keep this conversation and track its delivery as one Work.",
        "criteria": []
    });
    let (left, right) = tokio::join!(
        post_work_session_binding(app.clone(), &owner_id, &session_id, request.clone()),
        post_work_session_binding(app.clone(), &owner_id, &session_id, request.clone())
    );
    assert_eq!(left.0, StatusCode::CREATED, "left: {}", left.1);
    assert_eq!(right.0, StatusCode::CREATED, "right: {}", right.1);
    assert_eq!(left.1, right.1, "exact retries return the same Work");
    let work_id = left.1["overview"]["work_id"]
        .as_str()
        .expect("created Work id");
    let branch_session: String = sqlx::query_scalar(
        "SELECT session_id FROM work_branches WHERE owner_id = ? AND work_id = ?",
    )
    .bind(&owner_id)
    .bind(work_id)
    .fetch_one(pool.get())
    .await
    .expect("branch session");
    assert_eq!(branch_session, session_id);
    let work_project: Option<String> =
        sqlx::query_scalar("SELECT project_id FROM works WHERE owner_id = ? AND work_id = ?")
            .bind(&owner_id)
            .bind(work_id)
            .fetch_one(pool.get())
            .await
            .expect("Work project");
    assert_eq!(work_project.as_deref(), Some("project-promoted"));
    let session_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM agent_sessions WHERE user_id = ?")
            .bind(&owner_id)
            .fetch_one(pool.get())
            .await
            .expect("session count");
    assert_eq!(
        session_count, 4,
        "promotion must not create a hidden session"
    );

    let (binding_status, binding) =
        get_work_session_binding(app.clone(), &owner_id, &session_id).await;
    assert_eq!(binding_status, StatusCode::OK);
    assert_eq!(binding["work_id"], work_id);
    let (foreign_status, foreign) =
        post_work_session_binding(app.clone(), &other_owner_id, &session_id, request.clone()).await;
    assert_eq!(foreign_status, StatusCode::NOT_FOUND);
    assert_eq!(foreign["code"], "work_session_not_bindable");

    let (second_status, second) = post_work_session_binding(
        app.clone(),
        &owner_id,
        &session_id,
        serde_json::json!({
            "request_id": "different-promotion",
            "goal": "A different Work cannot claim an already-bound session.",
            "criteria": []
        }),
    )
    .await;
    assert_eq!(second_status, StatusCode::CONFLICT);
    assert_eq!(second["code"], "work_session_already_bound");

    sqlx::query(
        "INSERT INTO agent_session_execution_slots (user_id, session_id, run_id)
         VALUES (?, ?, 'later-run')",
    )
    .bind(&owner_id)
    .bind(&session_id)
    .execute(pool.get())
    .await
    .expect("later active run fixture");
    let (busy_replay_status, busy_replay) =
        post_work_session_binding(app.clone(), &owner_id, &session_id, request.clone()).await;
    assert_eq!(busy_replay_status, StatusCode::CREATED);
    assert_eq!(busy_replay, left.1, "exact replay survives a later run");
    sqlx::query("DELETE FROM agent_session_execution_slots WHERE user_id = ? AND session_id = ?")
        .bind(&owner_id)
        .bind(&session_id)
        .execute(pool.get())
        .await
        .expect("remove later active run fixture");
    sqlx::query("UPDATE agent_sessions SET status = 'closed' WHERE user_id = ? AND session_id = ?")
        .bind(&owner_id)
        .bind(&session_id)
        .execute(pool.get())
        .await
        .expect("close promoted session");
    let (closed_replay_status, closed_replay) =
        post_work_session_binding(app.clone(), &owner_id, &session_id, request).await;
    assert_eq!(closed_replay_status, StatusCode::CREATED);
    assert_eq!(
        closed_replay, left.1,
        "exact replay survives later terminal lifecycle state"
    );

    let (busy_status, busy) = post_work_session_binding(
        app.clone(),
        &owner_id,
        &busy_session_id,
        serde_json::json!({
            "request_id": "busy-promotion",
            "goal": "Do not race an active turn.",
            "criteria": []
        }),
    )
    .await;
    assert_eq!(busy_status, StatusCode::CONFLICT);
    assert_eq!(busy["code"], "work_session_busy");

    let running_work_id = id("running-promotion-work");
    let running_branch_id = id("running-promotion-branch");
    let running_genesis = WorkGenesis::new(WorkGenesisParts {
        owner_id: WorkOwnerId::parse(&owner_id).expect("owner"),
        work_id: WorkId::parse(&running_work_id).expect("work"),
        branch_id: WorkBranchId::parse(&running_branch_id).expect("branch"),
        session_id: InternalSessionId::parse(&busy_session_id).expect("session"),
        project_id: None,
        original_intent_ref: OriginalIntentRef::parse(id("running-promotion-intent"))
            .expect("intent"),
        goal: WorkGoal::parse("Track this exact active run without racing another writer.")
            .expect("goal"),
        criteria: Vec::new(),
    })
    .expect("running-session genesis");
    let repository = DatabaseWorkRepository::new(pool.clone());
    assert!(matches!(
        repository
            .create_genesis_in_running_session(running_genesis.clone(), "different-run")
            .await,
        Err(WorkRepositoryError::SessionBusy)
    ));
    let rejected_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM works WHERE owner_id = ? AND work_id = ?")
            .bind(&owner_id)
            .bind(&running_work_id)
            .fetch_one(pool.get())
            .await
            .expect("rejected running promotion count");
    assert_eq!(rejected_count, 0, "wrong run must roll back all Work rows");
    let rejected_run_binding: (Option<String>, Option<String>, Option<i64>) = sqlx::query_as(
        "SELECT work_id, work_branch_id, work_graph_revision
             FROM agent_runs WHERE user_id = ? AND run_id = 'active-run'",
    )
    .bind(&owner_id)
    .fetch_one(pool.get())
    .await
    .expect("run binding after rejected promotion");
    assert_eq!(
        rejected_run_binding,
        (None, None, None),
        "a rejected promotion must not partially bind its run"
    );
    let running_created = repository
        .create_genesis_in_running_session(running_genesis, "active-run")
        .await
        .expect("the exact slot-owning run may establish Work");
    assert_eq!(
        running_created.delivery_branch.parts().session_id.as_str(),
        busy_session_id
    );
    assert_eq!(
        running_created
            .work
            .parts()
            .project_id
            .as_ref()
            .map(|id| id.as_str()),
        Some("project-promoted")
    );
    type RunningBinding = (
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<String>,
        Option<i64>,
        Option<String>,
    );
    let running_binding: RunningBinding = sqlx::query_as(
        "SELECT work_id, work_branch_id, work_graph_revision,
                work_item_id, work_item_revision, work_item_attempt_id
         FROM agent_runs WHERE user_id = ? AND run_id = 'active-run'",
    )
    .bind(&owner_id)
    .fetch_one(pool.get())
    .await
    .expect("exact run Work binding");
    assert_eq!(
        running_binding,
        (
            Some(running_work_id.clone()),
            Some(running_branch_id.clone()),
            Some(1),
            None,
            None,
            None,
        ),
        "the coordinator inherits Work authority without claiming an item attempt"
    );
    let orphan_work_id = id("orphan-promotion-work");
    let orphan_genesis = WorkGenesis::new(WorkGenesisParts {
        owner_id: WorkOwnerId::parse(&owner_id).expect("owner"),
        work_id: WorkId::parse(&orphan_work_id).expect("work"),
        branch_id: WorkBranchId::parse(id("orphan-promotion-branch")).expect("branch"),
        session_id: InternalSessionId::parse(&orphan_run_session_id).expect("session"),
        project_id: None,
        original_intent_ref: OriginalIntentRef::parse(id("orphan-promotion-intent"))
            .expect("intent"),
        goal: WorkGoal::parse("Reject a slot whose durable run authority is missing.")
            .expect("goal"),
        criteria: Vec::new(),
    })
    .expect("orphan running-session genesis");
    assert!(matches!(
        repository
            .create_genesis_in_running_session(orphan_genesis, "missing-durable-run")
            .await,
        Err(WorkRepositoryError::SessionBusy)
    ));
    let orphan_work_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM works WHERE owner_id = ? AND work_id = ?")
            .bind(&owner_id)
            .bind(&orphan_work_id)
            .fetch_one(pool.get())
            .await
            .expect("orphan promotion rollback count");
    assert_eq!(
        orphan_work_count, 0,
        "a missing durable run must roll back every genesis row"
    );
    let (closed_status, closed) = post_work_session_binding(
        app,
        &owner_id,
        &closed_session_id,
        serde_json::json!({
            "request_id": "closed-promotion",
            "goal": "Do not bind a terminal session.",
            "criteria": []
        }),
    )
    .await;
    assert_eq!(closed_status, StatusCode::NOT_FOUND);
    assert_eq!(closed["code"], "work_session_not_bindable");

    let work_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM works WHERE owner_id = ?")
        .bind(&owner_id)
        .fetch_one(pool.get())
        .await
        .expect("Work count");
    assert_eq!(
        work_count, 2,
        "idle and exact-running promotions succeed; rejected promotions roll back"
    );
    cleanup_owner(&pool, &owner_id).await;
    cleanup_owner(&pool, &other_owner_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn get_work_is_exact_version_owner_scoped_and_never_exposes_session_identity() {
    let Some((app, pool)) = setup().await else {
        return;
    };
    let owner_id = id("owner");
    let other_owner_id = id("other-owner");
    let work_id = id("work");
    let branch_id = id("branch");
    cleanup_owner(&pool, &owner_id).await;
    cleanup_owner(&pool, &other_owner_id).await;
    DatabaseWorkRepository::new(pool.clone())
        .create_genesis(
            WorkGenesis::new(WorkGenesisParts {
                owner_id: WorkOwnerId::parse(&owner_id).expect("owner"),
                work_id: WorkId::parse(&work_id).expect("work"),
                branch_id: WorkBranchId::parse(&branch_id).expect("branch"),
                session_id: InternalSessionId::parse(id("session")).expect("session"),
                project_id: None,
                original_intent_ref: OriginalIntentRef::parse(id("intent")).expect("intent"),
                goal: WorkGoal::parse("Return the canonical Work read model.").expect("goal"),
                criteria: Vec::new(),
            })
            .expect("Work genesis"),
        )
        .await
        .expect("genesis");

    let (status, body, length) = get_work(app.clone(), &owner_id, &work_id).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert!(length < 64 * 1024);
    assert_eq!(body["schema_version"], 1);
    assert_eq!(body["scope"], "declared_work");
    assert_eq!(body["coherence"], "coherent");
    assert_eq!(body["coverage_gaps"], serde_json::json!([]));
    assert_eq!(
        body["finding"],
        serde_json::json!({
            "fact_code": "criteria_not_accepted",
            "cause_code": "accepted_criteria_empty",
        })
    );
    assert_eq!(body["satisfaction_evidence_refs"], serde_json::json!([]));
    assert_eq!(body["overview"]["work_id"], work_id);
    assert_eq!(
        body["overview"]["delivery"]["status"],
        "criteria_not_accepted"
    );
    assert_eq!(body["as_of"]["event_head"], body["overview"]["event_head"]);
    assert!(
        body["overview"]["delivery_branch"]
            .get("session_id")
            .is_none()
    );

    let (cross_status, mut cross_body, _) = get_work(app.clone(), &other_owner_id, &work_id).await;
    let missing_work_id = id("missing-work");
    let (missing_status, mut missing_body, _) =
        get_work(app, &other_owner_id, &missing_work_id).await;
    assert_eq!(cross_status, StatusCode::NOT_FOUND);
    assert_eq!(missing_status, StatusCode::NOT_FOUND);
    assert_eq!(cross_body["code"], "work_not_found");
    assert!(cross_body["request_id"].is_string());
    assert!(missing_body["request_id"].is_string());
    cross_body
        .as_object_mut()
        .expect("cross-owner error object")
        .remove("request_id");
    missing_body
        .as_object_mut()
        .expect("missing error object")
        .remove("request_id");
    assert_eq!(
        cross_body, missing_body,
        "cross-owner reads must not become an existence oracle"
    );

    cleanup_owner(&pool, &owner_id).await;
    cleanup_owner(&pool, &other_owner_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn work_branch_deletion_is_terminal_owner_scoped_and_delivery_safe() {
    let Some((app, pool)) =
        setup_with_run_lifecycle(Arc::new(WorkTurnRecordingLifecycle::default())).await
    else {
        return;
    };
    let owner_id = id("owner");
    let other_owner_id = id("other-owner");
    let work_id = id("work");
    let delivery_branch_id = id("delivery-branch");
    let delivery_session_id = id("delivery-session");
    let branch_id = id("deletable-branch");
    let branch_session_id = id("deletable-session");
    cleanup_owner(&pool, &owner_id).await;
    cleanup_owner(&pool, &other_owner_id).await;
    DatabaseWorkRepository::new(pool.clone())
        .create_genesis(
            WorkGenesis::new(WorkGenesisParts {
                owner_id: WorkOwnerId::parse(&owner_id).expect("owner"),
                work_id: WorkId::parse(&work_id).expect("work"),
                branch_id: WorkBranchId::parse(&delivery_branch_id).expect("delivery branch"),
                session_id: InternalSessionId::parse(&delivery_session_id)
                    .expect("delivery session"),
                project_id: None,
                original_intent_ref: OriginalIntentRef::parse(id("intent")).expect("intent"),
                goal: WorkGoal::parse("Delete one superseded approach safely.").expect("goal"),
                criteria: Vec::new(),
            })
            .expect("Work genesis"),
        )
        .await
        .expect("genesis");
    sqlx::query(
        "INSERT INTO work_branches
         (owner_id, work_id, branch_id, branch_revision, session_id,
          origin_branch_id, fork_cursor, goal_revision_ref,
          criteria_set_revision_ref, basis_graph_revision, current_graph_revision)
         SELECT owner_id, work_id, ?, 1, ?, branch_id, ?, goal_revision_ref,
                criteria_set_revision_ref, current_graph_revision, current_graph_revision
         FROM work_branches
         WHERE owner_id = ? AND work_id = ? AND branch_id = ?",
    )
    .bind(&branch_id)
    .bind(&branch_session_id)
    .bind(format!("sha256:{}", "7".repeat(64)))
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&delivery_branch_id)
    .execute(pool.get())
    .await
    .expect("insert deletable branch");
    sqlx::query(
        "INSERT INTO agent_sessions
         (user_id, session_id, status, created_at, updated_at, last_active_at)
         VALUES (?, ?, 'active', NOW(6), NOW(6), NOW(6))",
    )
    .bind(&owner_id)
    .bind(&branch_session_id)
    .execute(pool.get())
    .await
    .expect("insert deletable branch session");

    let request = serde_json::json!({
        "request_id": "delete-superseded-approach",
        "expected_work_revision": 1,
        "expected_branch_revision": 1,
    });
    let (status, operation) = post_work_branch_deletion(
        app.clone(),
        &owner_id,
        &work_id,
        &branch_id,
        request.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "operation: {operation}");
    assert_eq!(operation["schema_version"], 1);
    assert_eq!(operation["work_id"], work_id);
    assert_eq!(operation["branch_id"], branch_id);
    assert_eq!(operation["state"], "succeeded");
    assert_eq!(operation["phase"], "complete");
    assert_eq!(operation["outcome"], "deleted");
    assert_eq!(operation["work_revision"], 2);
    assert_eq!(operation["branch_revision"], 2);
    assert!(operation["completed_at"].is_string());
    assert_field_absent(&operation, "session_id");
    assert_field_absent(&operation, "owner_id");
    let operation_id = operation["operation_id"]
        .as_str()
        .expect("public operation id");

    let (get_status, loaded) =
        get_work_branch_deletion(app.clone(), &owner_id, &work_id, &branch_id, operation_id).await;
    assert_eq!(get_status, StatusCode::OK);
    assert_eq!(
        loaded, operation,
        "GET must expose the terminal receipt exactly"
    );
    let (replay_status, replay) =
        post_work_branch_deletion(app.clone(), &owner_id, &work_id, &branch_id, request).await;
    assert_eq!(replay_status, StatusCode::CREATED);
    assert_eq!(replay, operation, "request-id replay must be exact");

    let (foreign_status, foreign) = get_work_branch_deletion(
        app.clone(),
        &other_owner_id,
        &work_id,
        &branch_id,
        operation_id,
    )
    .await;
    assert_eq!(foreign_status, StatusCode::NOT_FOUND);
    assert_eq!(foreign["code"], "work_branch_deletion_not_found");
    let deleted_branch_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM work_branches
         WHERE owner_id = ? AND work_id = ? AND branch_id = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&branch_id)
    .fetch_one(pool.get())
    .await
    .expect("deleted branch count");
    let deleted_session_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_sessions WHERE user_id = ? AND session_id = ?",
    )
    .bind(&owner_id)
    .bind(&branch_session_id)
    .fetch_one(pool.get())
    .await
    .expect("deleted session count");
    assert_eq!(deleted_branch_count, 0);
    assert_eq!(deleted_session_count, 0);

    let (protected_status, protected) = post_work_branch_deletion(
        app,
        &owner_id,
        &work_id,
        &delivery_branch_id,
        serde_json::json!({
            "request_id": "delete-delivery-approach",
            "expected_work_revision": 2,
            "expected_branch_revision": 1,
        }),
    )
    .await;
    assert_eq!(protected_status, StatusCode::CREATED);
    assert_eq!(protected["state"], "conflict");
    assert_eq!(protected["phase"], "complete");
    assert_eq!(protected["outcome"], "delivery_branch_protected");
    let delivery_identity: String = sqlx::query_scalar(
        "SELECT delivery_branch_id FROM works WHERE owner_id = ? AND work_id = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .fetch_one(pool.get())
    .await
    .expect("delivery branch identity");
    assert_eq!(delivery_identity, delivery_branch_id);

    cleanup_owner(&pool, &owner_id).await;
    cleanup_owner(&pool, &other_owner_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn put_read_cursor_is_exact_monotonic_owner_scoped_and_conflict_typed() {
    let Some((app, pool)) = setup().await else {
        return;
    };
    let owner_id = id("owner");
    let other_owner_id = id("other-owner");
    let work_id = id("work");
    cleanup_owner(&pool, &owner_id).await;
    cleanup_owner(&pool, &other_owner_id).await;
    DatabaseWorkRepository::new(pool.clone())
        .create_genesis(
            WorkGenesis::new(WorkGenesisParts {
                owner_id: WorkOwnerId::parse(&owner_id).expect("owner"),
                work_id: WorkId::parse(&work_id).expect("work"),
                branch_id: WorkBranchId::parse(id("branch")).expect("branch"),
                session_id: InternalSessionId::parse(id("session")).expect("session"),
                project_id: None,
                original_intent_ref: OriginalIntentRef::parse(id("intent")).expect("intent"),
                goal: WorkGoal::parse("Persist one exact user read watermark.").expect("goal"),
                criteria: Vec::new(),
            })
            .expect("Work genesis"),
        )
        .await
        .expect("genesis");

    let (status, receipt) = put_read_cursor(
        app.clone(),
        &owner_id,
        &work_id,
        serde_json::json!({"through_event_seq": 1}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {receipt}");
    assert_eq!(receipt["schema_version"], 1);
    assert_eq!(receipt["work_id"], work_id);
    assert_eq!(receipt["through_event_seq"], 1);
    assert_eq!(receipt["receipt_revision"], 2);
    assert!(
        receipt["receipt_hash"]
            .as_str()
            .is_some_and(|hash| hash.starts_with("sha256:") && hash.len() == 71)
    );
    assert!(receipt.get("owner_id").is_none());
    assert!(receipt.get("delivered_through_event_seq").is_none());

    let (events_status, events, events_length) =
        get_work_events(app.clone(), &owner_id, &work_id, "?limit=1").await;
    assert_eq!(events_status, StatusCode::OK, "body: {events}");
    assert!(events_length < 64 * 1024);
    assert_eq!(events["schema_version"], 1);
    assert_eq!(events["event_head"], 1);
    assert_eq!(events["seen_through_event_seq"], 1);
    assert_eq!(events["coverage"], "complete");
    assert_eq!(events["events"][0]["kind"], "work_created");
    assert!(events["events"][0].get("session_id").is_none());
    let (future_events_status, future_events, _) =
        get_work_events(app.clone(), &owner_id, &work_id, "?after_event_seq=2").await;
    assert_eq!(future_events_status, StatusCode::CONFLICT);
    assert_eq!(future_events["code"], "work_event_cursor_ahead");

    let (replay_status, replay) = put_read_cursor(
        app.clone(),
        &owner_id,
        &work_id,
        serde_json::json!({"through_event_seq": 1}),
    )
    .await;
    assert_eq!(replay_status, StatusCode::OK);
    assert_eq!(replay, receipt, "exact PUT replay must return one receipt");

    let (future_status, future) = put_read_cursor(
        app.clone(),
        &owner_id,
        &work_id,
        serde_json::json!({"through_event_seq": 2}),
    )
    .await;
    assert_eq!(future_status, StatusCode::CONFLICT);
    assert_eq!(future["code"], "work_event_cursor_ahead");
    assert_eq!(future["action_hints"], serde_json::json!(["refresh_work"]));

    let (foreign_status, foreign) = put_read_cursor(
        app.clone(),
        &other_owner_id,
        &work_id,
        serde_json::json!({"through_event_seq": 1}),
    )
    .await;
    assert_eq!(foreign_status, StatusCode::NOT_FOUND);
    assert_eq!(foreign["code"], "work_not_found");
    let (foreign_events_status, foreign_events, _) =
        get_work_events(app.clone(), &other_owner_id, &work_id, "").await;
    assert_eq!(foreign_events_status, StatusCode::NOT_FOUND);
    assert_eq!(foreign_events["code"], "work_not_found");

    let (limit_status, limit_error, _) =
        get_work_events(app.clone(), &owner_id, &work_id, "?limit=101").await;
    assert_eq!(limit_status, StatusCode::BAD_REQUEST);
    assert_eq!(limit_error["code"], "invalid_work_event_limit");

    let (unknown_status, unknown) = put_read_cursor(
        app.clone(),
        &owner_id,
        &work_id,
        serde_json::json!({"through_event_seq": 1, "session_id": "legacy"}),
    )
    .await;
    assert_eq!(unknown_status, StatusCode::BAD_REQUEST);
    assert_eq!(unknown["code"], "invalid_work_read_cursor_request");
    let (zero_status, zero) = put_read_cursor(
        app,
        &owner_id,
        &work_id,
        serde_json::json!({"through_event_seq": 0}),
    )
    .await;
    assert_eq!(zero_status, StatusCode::BAD_REQUEST);
    assert_eq!(zero["code"], "invalid_work_event_cursor");

    let stored_revision = sqlx::query(
        "SELECT receipt_revision FROM work_attention_receipts
         WHERE owner_id = ? AND work_id = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .fetch_one(pool.get())
    .await
    .expect("stored read receipt")
    .try_get::<i64, _>("receipt_revision")
    .expect("receipt revision");
    assert_eq!(
        stored_revision, 2,
        "replay and rejected requests must not mutate the receipt"
    );

    cleanup_owner(&pool, &owner_id).await;
    cleanup_owner(&pool, &other_owner_id).await;
}
