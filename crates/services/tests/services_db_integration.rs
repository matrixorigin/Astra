//! Live MatrixOne checks for list endpoints (`pagination` caps, `skills_registry` seek list + index),
//! cross-session audit aggregates (`get_cross_session_stats`, `list_sessions`), session restore, and owner-bound sync behavior.
//!
//! ```text
//! ASTRA_TEST_DB_IT=1 cargo test -p astra-services --test services_db_integration -- --ignored
//! ```
//!
//! Uses `MATRIXONE_*` after `dotenvy` (defaults match `.env.example`).
//!
//! **Index note:** `ensure_core_schema` only applies new indexes on first `CREATE TABLE`. For dev DBs
//! created before `idx_skill_active_name_ver` existed, this suite runs a **test-only** `CREATE INDEX`
//! (ignores duplicate-name errors) so the listing path is validated against the intended DDL.

use astra_core::{EvidenceRef, MatrixOneSettings, SharedPool};
use astra_services::event_ingestion::{EventIngestionWorker, IngestionConfig, IngestionEvent};
use astra_services::introspection::{
    IntentDriftAssessmentStatus, IntentDriftLevel, IntentDriftVerdict,
};
use astra_services::session_audit::TurnListParams;
use astra_services::session_audit::{
    AuditSessionListParams, CrossSessionStatsParams, DatabaseSessionAuditService,
    SessionAuditService,
};
use astra_services::session_restore::SessionRestoreService;
use astra_services::session_workspace::{
    ContextTraceSignal, ContextTraceToolSurface, WORKSPACE_METADATA_ARTIFACT_KIND,
    WORKSPACE_METADATA_PROJECTION_ID, WorkspaceMetadata, persist_remote_workspace,
    to_remote_artifact_record,
};
use astra_services::{
    AcquireWriterOutcome, AdminAuditFilter, AdminAuditReader, DatabaseAdminAuditReader,
    DatabaseEventService, DatabaseIntrospectionService, DatabaseMarketplaceService,
    DatabaseReflectService, DatabaseSessionArtifactStore, DatabaseSessionContextCoordinator,
    DatabaseSessionService, DatabaseSkillService, EventCreateRequestData, EventListFilter,
    EventService, IntrospectionService, MAX_API_LIST_LIMIT, MarketplaceService, ReflectService,
    ReserveTurnOutcome, SessionArtifactJsonStore, SessionArtifactReference,
    SessionArtifactReferenceKind, SessionArtifactStore, SessionArtifactStoreError,
    SessionContextCoordinator, SessionContextCoordinatorError, SessionListFilter, SessionService,
    SkillService,
};
use astra_turn_types::{
    ActorContextV1, ActorKindV1, AuthorityEpochsV1, SessionKeyV1, SessionSurfaceV1,
};
use sha2::Digest;
use sqlx::Row;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

mod common;
use tracing::instrument::WithSubscriber;
use tracing_subscriber::prelude::*;

// Historical rows are read-side inputs, not a substitute checkpoint upload API.
async fn seed_restore_session_fixture(
    pool: &sqlx::Pool<sqlx::MySql>,
    user_id: &str,
    session_id: &str,
    metadata: serde_json::Value,
) {
    sqlx::query("INSERT INTO agent_sessions (session_id, user_id, title, status, event_count, metadata) VALUES (?, ?, 'restore fixture', 'active', 0, CAST(? AS JSON))")
        .bind(session_id).bind(user_id).bind(metadata.to_string())
        .execute(pool).await.expect("seed historical session");
}

fn context_trace_request_fixture(
    session_id: &str,
    signal: &ContextTraceSignal,
) -> EventCreateRequestData {
    EventCreateRequestData {
        ingestion_source: astra_services::events::EventIngestionSource::Client,
        event_id: None,
        session_id: session_id.into(),
        event_type: "context_trace_signal".into(),
        content: signal.preview(),
        agent_id: Some("fixture-agent".into()),
        agent_version: Some(env!("CARGO_PKG_VERSION").into()),
        parent_event_id: None,
        parent_event_ids: Some(Vec::new()),
        causal_chain_id: Some(format!("{session_id}:context-trace:{}", signal.turn_id)),
        metadata: Some(serde_json::to_value(signal).unwrap()),
    }
}

fn agent_event_fixture_payload_hash(payload: serde_json::Value) -> String {
    astra_services::observation_capture::canonical_agent_event_payload_hash(&payload)
}

fn model_request_fixture_accepted_event(
    user_id: &str,
    session_id: &str,
    request_id: &str,
    run_id: &str,
    parent_run_id: Option<&str>,
    agent_id: &str,
) -> astra_services::ModelRequestContextEvent {
    use astra_services::{
        ModelRequestContextEvent, ModelRequestEventStage, ModelRequestIdentity,
        ModelRequestTopology,
    };

    ModelRequestContextEvent {
        schema: astra_services::MODEL_REQUEST_CONTEXT_SCHEMA.into(),
        stage: ModelRequestEventStage::Accepted,
        identity: ModelRequestIdentity {
            request_id: request_id.into(),
            provider_response_id: None,
            owner_scope: user_id.into(),
            session_id: Some(session_id.into()),
            run_id: Some(run_id.into()),
            turn: Some(1),
            round: Some(0),
            logical_attempt: 0,
            physical_attempt: 0,
            actor_id: Some("fixture-actor".into()),
            execution_principal: Some("server".into()),
            billing_scope: Some(user_id.into()),
            auth_session_id: None,
            device_instance_id: None,
            agent_id: Some(agent_id.into()),
            parent_run_id: parent_run_id.map(str::to_owned),
            topology: ModelRequestTopology::ServerOnly,
            interaction_owner: "server".into(),
            loop_owner: "server".into(),
            execution_binding: "server".into(),
            provider: "fixture-provider".into(),
            model: "glm-5.2".into(),
            offering_id: "glm-5.2-offering".into(),
            inference_purpose: "agent_turn".into(),
            operation_id: "agent_turn".into(),
            provider_protocol: "openai_compatible".into(),
            provider_wire_hash: "fixture-wire-hash".into(),
            provider_wire_bytes: 100,
        },
        route: astra_services::ModelRequestRoute {
            route_id: format!("route-{request_id}"),
            invocation_id: format!("invocation-{request_id}"),
            upstream_model: "glm-5.2".into(),
            execution_placement: astra_services::ModelExecutionPlacement::Server,
            access_kind: astra_services::ModelAccessKind::SelfHosted,
        },
        lineage: Default::default(),
        budget: Default::default(),
        usage: None,
        composition: Default::default(),
        wire_composition: Default::default(),
        cache: Default::default(),
        compaction: Default::default(),
        terminal_status: None,
        usage_status: None,
        error_kind: None,
    }
}

async fn insert_model_request_fixture(
    pool: &sqlx::Pool<sqlx::MySql>,
    user_id: &str,
    session_id: &str,
    attempt_id: &str,
    event: &astra_services::ModelRequestContextEvent,
    created_at: &str,
) {
    let usage = event.usage.as_ref();
    // Consumer fixture for the current stored contract, not a writer simulation.
    let payload = match event.stage {
        astra_services::ModelRequestEventStage::Accepted => serde_json::to_value(event).unwrap(),
        astra_services::ModelRequestEventStage::Terminal => serde_json::json!({
            "schema": "model_request_terminal_v1",
            "terminal_status": event.terminal_status,
            "provider_response_id": event.identity.provider_response_id,
            "usage_status": event.usage_status,
            "usage": event.usage,
            "error_kind": event.error_kind,
            "usage_source": "provider_terminal",
        }),
    };
    sqlx::query(
        "INSERT INTO model_request_context_events \
         (user_id, session_id, run_id, event_id, attempt_id, invocation_id, event_stage, \
          terminal_status, topology, provider, model_family, purpose, input_tokens, \
          output_tokens, cache_read_tokens, cache_creation_tokens, event_json, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(user_id)
    .bind(session_id)
    .bind(&event.identity.run_id)
    .bind(Uuid::new_v4().to_string())
    .bind(attempt_id)
    .bind(format!("invocation-{attempt_id}"))
    .bind(event.stage.as_str())
    .bind(event.terminal_status.as_deref())
    .bind(event.identity.topology.as_str())
    .bind(&event.identity.provider)
    .bind(&event.identity.model)
    .bind(&event.identity.inference_purpose)
    .bind(usage.map(|usage| usage.input.total_input_tokens() as i64))
    .bind(usage.map(|usage| usage.output_tokens as i64))
    .bind(usage.map(|usage| usage.input.cache_read_tokens as i64))
    .bind(usage.map(|usage| usage.input.cache_creation_tokens as i64))
    .bind(serde_json::to_string(&payload).expect("serialize model request fixture"))
    .bind(created_at)
    .execute(pool)
    .await
    .expect("insert model request context fixture");
}

async fn cleanup_model_request_fixtures(
    pool: &sqlx::Pool<sqlx::MySql>,
    user_id: &str,
    attempt_ids: &[String],
) {
    for attempt_id in attempt_ids {
        sqlx::query(
            "DELETE FROM model_request_context_events WHERE user_id = ? AND attempt_id = ?",
        )
        .bind(user_id)
        .bind(attempt_id)
        .execute(pool)
        .await
        .expect("delete model request context fixture");
    }
}

async fn setup_pool_and_settings() -> (SharedPool, MatrixOneSettings) {
    common::setup_pool_and_settings().await
}

#[tokio::test]
#[ignore = "requires live MatrixOne (ASTRA_TEST_DB_IT=1)"]
async fn session_creation_returns_database_generated_fields_after_commit() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();
    let user_id = format!("session-create-it-{}", Uuid::new_v4());
    let service = DatabaseSessionService::new(settings).with_pool(shared);
    let metadata = serde_json::Map::from_iter([("marker".into(), serde_json::json!(user_id))]);
    let record = service
        .create_session(
            user_id.clone(),
            astra_services::auth::session::SessionCreateRequestData {
                title: Some("Session creation visibility".into()),
                agent_id: Some("test-agent".into()),
                metadata: Some(metadata.clone()),
            },
        )
        .await
        .expect("create session and commit");

    assert_eq!(record.user_id, user_id);
    assert_eq!(record.metadata, metadata);
    assert_eq!(record.title.as_deref(), Some("Session creation visibility"));
    assert_eq!(record.agent_id.as_deref(), Some("test-agent"));
    assert_eq!(record.status, "active");
    assert_eq!(record.event_count, 0);
    chrono::NaiveDateTime::parse_from_str(&record.created_at, "%Y-%m-%dT%H:%M:%S")
        .expect("database-generated creation timestamp");
    assert_eq!(
        record.updated_at.as_deref(),
        Some(record.created_at.as_str())
    );
    assert_eq!(record.ended_at, None);

    // Do not verify persistence with an immediate pooled SELECT: its snapshot
    // may predate the commit. The separate transaction contract test guards the
    // read-before-commit ordering; this test checks the database-decoded result.

    for table in [
        "agent_sessions",
        "agent_session_lifecycle_fences",
        "auth_audit_logs",
    ] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM {table} WHERE user_id = ?"
        )))
        .bind(&user_id)
        .execute(&pool)
        .await
        .expect("clean up test-owned rows");
    }
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn work_establishment_admission_is_idempotent_on_matrixone() {
    use astra_services::work::{
        DatabaseWorkEstablishmentService, InternalSessionId, WorkBranchId,
        WorkEstablishmentActivation, WorkEstablishmentRequest, WorkId, WorkOwnerId,
    };

    let (shared, _) = setup_pool_and_settings().await;
    let service = DatabaseWorkEstablishmentService::new(shared.clone());
    let suffix = Uuid::new_v4().simple().to_string();
    let owner_id = WorkOwnerId::parse(format!("it-work-owner-{suffix}")).expect("owner id");
    let operation_id = format!("it-work-operation-{suffix}");
    let session_id = format!("it-session-{suffix}");
    let request = WorkEstablishmentRequest {
        operation_id: operation_id.clone(),
        request_hash: format!("it-work-request-hash-{suffix}"),
        payload_json: r#"{"goal":"matrixone admission probe"}"#.to_string(),
        owner_id: owner_id.clone(),
        work_id: WorkId::parse(format!("it-work-{suffix}")).expect("work id"),
        branch_id: WorkBranchId::parse(format!("it-branch-{suffix}")).expect("branch id"),
        session_id: InternalSessionId::parse(&session_id).expect("session id"),
        run_id: format!("it-run-{suffix}"),
        activation: WorkEstablishmentActivation::Start,
    };

    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
         VALUES (?, ?, 'it Work admission', 'active', 0)",
    )
    .bind(&session_id)
    .bind(owner_id.as_str())
    .execute(shared.get())
    .await
    .expect("insert canonical session admission fence");

    let first = service
        .admit_with_disposition(&request)
        .await
        .expect("first admission");
    assert_eq!(
        first.disposition,
        astra_services::work::WorkEstablishmentAdmissionDisposition::Created
    );
    let second = service
        .admit_with_disposition(&request)
        .await
        .expect("repeated admission must remain idempotent");
    assert_eq!(
        second.disposition,
        astra_services::work::WorkEstablishmentAdmissionDisposition::Existing
    );
    assert_eq!(
        first.operation, second.operation,
        "duplicate admission must return the immutable row"
    );
    assert!(!second.operation.is_complete());

    sqlx::query(
        "DELETE FROM work_establishment_operations
         WHERE owner_id = ? AND operation_id = ?",
    )
    .bind(owner_id.as_str())
    .bind(operation_id)
    .execute(shared.get())
    .await
    .expect("cleanup admission probe");
    sqlx::query("DELETE FROM agent_sessions WHERE user_id = ? AND session_id = ?")
        .bind(owner_id.as_str())
        .bind(&session_id)
        .execute(shared.get())
        .await
        .expect("cleanup canonical session admission fence");
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn work_establishment_terminal_cancel_releases_session_for_new_turn() {
    use astra_services::work::{
        DatabaseWorkEstablishmentService, InternalSessionId, WorkBranchId,
        WorkEstablishmentActivation, WorkEstablishmentPhase, WorkEstablishmentRequest,
        WorkEstablishmentState, WorkId, WorkOwnerId,
    };

    let (shared, _) = setup_pool_and_settings().await;
    let service = DatabaseWorkEstablishmentService::new(shared.clone());
    let suffix = Uuid::new_v4().simple().to_string();
    let owner_id = WorkOwnerId::parse(format!("it-work-owner-cancel-{suffix}")).expect("owner id");
    let session_id = format!("it-session-cancel-{suffix}");
    let session = InternalSessionId::parse(&session_id).expect("session id");
    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
         VALUES (?, ?, 'it Work cancellation', 'active', 0)",
    )
    .bind(&session_id)
    .bind(owner_id.as_str())
    .execute(shared.get())
    .await
    .expect("insert canonical session admission fence");

    let make_request = |ordinal: &str, turn_chain_id: &str| WorkEstablishmentRequest {
        operation_id: format!("it-work-operation-cancel-{ordinal}-{suffix}"),
        request_hash: format!("it-work-request-hash-cancel-{ordinal}-{suffix}"),
        payload_json: format!(
            r#"{{"schema_version":2,"turn_chain_id":"{turn_chain_id}","goal":"cancel probe {ordinal}"}}"#
        ),
        owner_id: owner_id.clone(),
        work_id: WorkId::parse(format!("it-work-cancel-{ordinal}-{suffix}")).expect("work id"),
        branch_id: WorkBranchId::parse(format!("it-branch-cancel-{ordinal}-{suffix}"))
            .expect("branch id"),
        session_id: session.clone(),
        run_id: format!("it-run-cancel-{ordinal}-{suffix}"),
        activation: WorkEstablishmentActivation::Start,
    };

    let request_admit = make_request("admit", "turn-admit");
    let admitted = service
        .admit(&request_admit)
        .await
        .expect("admit operation");
    let cancelled = service
        .cancel(&request_admit, "user cancelled before genesis")
        .await
        .expect("cancel after admission");
    assert_eq!(cancelled.state, WorkEstablishmentState::Cancelled);
    assert!(cancelled.is_aborted());

    let request_genesis = make_request("genesis", "turn-genesis");
    service
        .admit(&request_genesis)
        .await
        .expect("admit genesis operation");
    service
        .advance_phase(
            &request_genesis,
            WorkEstablishmentPhase::AwaitingGenesis,
            WorkEstablishmentPhase::AwaitingPlan,
        )
        .await
        .expect("record genesis");
    let cancelled = service
        .cancel(&request_genesis, "user cancelled after genesis")
        .await
        .expect("cancel after genesis");
    assert_eq!(cancelled.state, WorkEstablishmentState::Cancelled);

    let request_plan = make_request("plan", "turn-plan");
    service
        .admit(&request_plan)
        .await
        .expect("admit plan operation");
    service
        .advance_phase(
            &request_plan,
            WorkEstablishmentPhase::AwaitingGenesis,
            WorkEstablishmentPhase::AwaitingPlan,
        )
        .await
        .expect("record plan genesis");
    service
        .advance_phase(
            &request_plan,
            WorkEstablishmentPhase::AwaitingPlan,
            WorkEstablishmentPhase::AwaitingAssignment,
        )
        .await
        .expect("record plan");
    let cancelled = service
        .cancel(&request_plan, "user cancelled after plan")
        .await
        .expect("cancel after plan");
    assert_eq!(cancelled.state, WorkEstablishmentState::Cancelled);

    // A newer turn can replace an abandoned pending carrier without a manual
    // repair operation, while the exact cancelled operation remains
    // idempotently replayable as a terminal fact.
    let request_stale = make_request("stale", "turn-stale");
    service
        .admit(&request_stale)
        .await
        .expect("admit stale carrier");
    assert!(
        service
            .cancel_pending_for_new_turn(
                &owner_id,
                &session,
                "turn-new",
                "new user turn replaced stale carrier",
            )
            .await
            .expect("cancel stale carrier")
    );
    let request_new = make_request("new", "turn-new");
    let new_admission = service
        .admit_with_disposition(&request_new)
        .await
        .expect("new turn admission");
    assert_eq!(
        new_admission.disposition,
        astra_services::work::WorkEstablishmentAdmissionDisposition::Created
    );
    assert_eq!(admitted.state, WorkEstablishmentState::Pending);

    sqlx::query("DELETE FROM work_establishment_operations WHERE owner_id = ? AND session_id = ?")
        .bind(owner_id.as_str())
        .bind(&session_id)
        .execute(shared.get())
        .await
        .expect("cleanup cancellation operations");
    sqlx::query("DELETE FROM agent_sessions WHERE user_id = ? AND session_id = ?")
        .bind(owner_id.as_str())
        .bind(&session_id)
        .execute(shared.get())
        .await
        .expect("cleanup cancellation session fence");
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn work_establishment_admission_serializes_concurrent_sessions_on_matrixone() {
    use astra_services::work::{
        DatabaseWorkEstablishmentService, InternalSessionId, WorkBranchId,
        WorkEstablishmentActivation, WorkEstablishmentAdmissionDisposition,
        WorkEstablishmentRequest, WorkId, WorkOwnerId,
    };

    let (shared, _) = setup_pool_and_settings().await;
    let service = DatabaseWorkEstablishmentService::new(shared.clone());
    let suffix = Uuid::new_v4().simple().to_string();
    let owner_id =
        WorkOwnerId::parse(format!("it-work-owner-concurrent-{suffix}")).expect("owner id");
    let session_id = format!("it-session-concurrent-{suffix}");
    let session = InternalSessionId::parse(&session_id).expect("session id");
    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
         VALUES (?, ?, 'it Work concurrent admission', 'active', 0)",
    )
    .bind(&session_id)
    .bind(owner_id.as_str())
    .execute(shared.get())
    .await
    .expect("insert canonical session admission fence");

    let make_request = |ordinal: &str| WorkEstablishmentRequest {
        operation_id: format!("it-work-operation-concurrent-{ordinal}-{suffix}"),
        request_hash: format!("it-work-request-hash-concurrent-{ordinal}-{suffix}"),
        payload_json: format!(r#"{{"goal":"concurrent admission {ordinal}"}}"#),
        owner_id: owner_id.clone(),
        work_id: WorkId::parse(format!("it-work-concurrent-{ordinal}-{suffix}")).expect("work id"),
        branch_id: WorkBranchId::parse(format!("it-branch-concurrent-{ordinal}-{suffix}"))
            .expect("branch id"),
        session_id: session.clone(),
        run_id: format!("it-run-concurrent-{ordinal}-{suffix}"),
        activation: WorkEstablishmentActivation::Start,
    };
    let request_a = make_request("a");
    let request_b = make_request("b");
    let service_a = service.clone();
    let service_b = service.clone();
    let start = Arc::new(tokio::sync::Barrier::new(2));
    let start_a = start.clone();
    let start_b = start.clone();
    let task_a = tokio::spawn(async move {
        start_a.wait().await;
        service_a.admit_with_disposition(&request_a).await
    });
    let task_b = tokio::spawn(async move {
        start_b.wait().await;
        service_b.admit_with_disposition(&request_b).await
    });
    let (result_a, result_b) = tokio::join!(task_a, task_b);
    let result_a = result_a.expect("concurrent admission task A");
    let result_b = result_b.expect("concurrent admission task B");

    let dispositions = [result_a.as_ref(), result_b.as_ref()]
        .into_iter()
        .filter_map(|result| result.as_ref().ok().map(|admission| admission.disposition))
        .collect::<Vec<_>>();
    assert_eq!(
        dispositions
            .iter()
            .filter(|disposition| {
                **disposition == WorkEstablishmentAdmissionDisposition::Created
            })
            .count(),
        1,
        "exactly one concurrent admission may create the session owner: {result_a:?}; {result_b:?}"
    );
    assert!(
        [result_a.as_ref(), result_b.as_ref()]
            .into_iter()
            .any(|result| matches!(
                result,
                Err(astra_services::work::WorkEstablishmentError::PendingSessionConflict)
            )),
        "the losing concurrent admission must be a typed session conflict: {result_a:?}; {result_b:?}"
    );

    sqlx::query("DELETE FROM work_establishment_operations WHERE owner_id = ? AND session_id = ?")
        .bind(owner_id.as_str())
        .bind(&session_id)
        .execute(shared.get())
        .await
        .expect("cleanup concurrent admission operations");
    sqlx::query("DELETE FROM agent_sessions WHERE user_id = ? AND session_id = ?")
        .bind(owner_id.as_str())
        .bind(&session_id)
        .execute(shared.get())
        .await
        .expect("cleanup concurrent session admission fence");
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn work_establishment_same_operation_concurrent_admission_is_one_create_one_existing() {
    use astra_services::work::{
        DatabaseWorkEstablishmentService, InternalSessionId, WorkBranchId,
        WorkEstablishmentActivation, WorkEstablishmentAdmissionDisposition,
        WorkEstablishmentRequest, WorkId, WorkOwnerId,
    };

    let (shared, _) = setup_pool_and_settings().await;
    let service = DatabaseWorkEstablishmentService::new(shared.clone());
    let suffix = Uuid::new_v4().simple().to_string();
    let owner_id = WorkOwnerId::parse(format!("it-work-owner-same-{suffix}")).expect("owner id");
    let session_id = format!("it-session-same-{suffix}");
    let request = WorkEstablishmentRequest {
        operation_id: format!("it-work-operation-same-{suffix}"),
        request_hash: format!("it-work-request-hash-same-{suffix}"),
        payload_json: r#"{"goal":"same operation admission"}"#.to_string(),
        owner_id: owner_id.clone(),
        work_id: WorkId::parse(format!("it-work-same-{suffix}")).expect("work id"),
        branch_id: WorkBranchId::parse(format!("it-branch-same-{suffix}")).expect("branch id"),
        session_id: InternalSessionId::parse(&session_id).expect("session id"),
        run_id: format!("it-run-same-{suffix}"),
        activation: WorkEstablishmentActivation::Start,
    };
    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
         VALUES (?, ?, 'it Work same-operation admission', 'active', 0)",
    )
    .bind(&session_id)
    .bind(owner_id.as_str())
    .execute(shared.get())
    .await
    .expect("insert canonical session admission fence");

    let start = Arc::new(tokio::sync::Barrier::new(2));
    let start_a = start.clone();
    let start_b = start.clone();
    let service_a = service.clone();
    let service_b = service.clone();
    let request_a = request.clone();
    let request_b = request.clone();
    let task_a = tokio::spawn(async move {
        start_a.wait().await;
        service_a.admit_with_disposition(&request_a).await
    });
    let task_b = tokio::spawn(async move {
        start_b.wait().await;
        service_b.admit_with_disposition(&request_b).await
    });
    let (result_a, result_b) = tokio::join!(task_a, task_b);
    let result_a = result_a.expect("same-operation admission task A");
    let result_b = result_b.expect("same-operation admission task B");
    let admissions = [result_a.as_ref(), result_b.as_ref()]
        .into_iter()
        .filter_map(Result::ok)
        .collect::<Vec<_>>();
    assert_eq!(
        admissions.len(),
        2,
        "both identical admissions must succeed"
    );
    assert_eq!(
        admissions
            .iter()
            .filter(|admission| {
                admission.disposition == WorkEstablishmentAdmissionDisposition::Created
            })
            .count(),
        1,
        "same operation must have exactly one creator: {result_a:?}; {result_b:?}"
    );
    assert_eq!(
        admissions
            .iter()
            .filter(|admission| {
                admission.disposition == WorkEstablishmentAdmissionDisposition::Existing
            })
            .count(),
        1,
        "same operation must have exactly one replay: {result_a:?}; {result_b:?}"
    );
    let pending_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM work_establishment_operations \
         WHERE owner_id = ? AND session_id = ? AND operation_state = 'pending'",
    )
    .bind(owner_id.as_str())
    .bind(&session_id)
    .fetch_one(shared.get())
    .await
    .expect("count same-operation pending rows");
    assert_eq!(pending_count, 1, "one durable pending row is the authority");

    sqlx::query("DELETE FROM work_establishment_operations WHERE owner_id = ? AND session_id = ?")
        .bind(owner_id.as_str())
        .bind(&session_id)
        .execute(shared.get())
        .await
        .expect("cleanup same-operation admission");
    sqlx::query("DELETE FROM agent_sessions WHERE user_id = ? AND session_id = ?")
        .bind(owner_id.as_str())
        .bind(&session_id)
        .execute(shared.get())
        .await
        .expect("cleanup same-operation session fence");
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn work_establishment_replays_exact_operation_while_another_is_pending() {
    use astra_services::work::{
        DatabaseWorkEstablishmentService, InternalSessionId, WorkBranchId,
        WorkEstablishmentActivation, WorkEstablishmentAdmissionDisposition, WorkEstablishmentPhase,
        WorkEstablishmentRequest, WorkId, WorkOwnerId,
    };

    let (shared, _) = setup_pool_and_settings().await;
    let service = DatabaseWorkEstablishmentService::new(shared.clone());
    let suffix = Uuid::new_v4().simple().to_string();
    let owner_id = WorkOwnerId::parse(format!("it-work-owner-replay-{suffix}")).expect("owner id");
    let session_id = format!("it-session-replay-{suffix}");
    let session = InternalSessionId::parse(&session_id).expect("session id");
    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
         VALUES (?, ?, 'it Work replay admission', 'active', 0)",
    )
    .bind(&session_id)
    .bind(owner_id.as_str())
    .execute(shared.get())
    .await
    .expect("insert canonical session admission fence");

    let make_request = |ordinal: &str| WorkEstablishmentRequest {
        operation_id: format!("it-work-operation-replay-{ordinal}-{suffix}"),
        request_hash: format!("it-work-request-hash-replay-{ordinal}-{suffix}"),
        payload_json: format!(r#"{{"goal":"replay operation {ordinal}"}}"#),
        owner_id: owner_id.clone(),
        work_id: WorkId::parse(format!("it-work-replay-{ordinal}-{suffix}")).expect("work id"),
        branch_id: WorkBranchId::parse(format!("it-branch-replay-{ordinal}-{suffix}"))
            .expect("branch id"),
        session_id: session.clone(),
        run_id: format!("it-run-replay-{ordinal}-{suffix}"),
        activation: WorkEstablishmentActivation::Start,
    };
    let request_a = make_request("a");
    let request_b = make_request("b");
    let admitted_a = service.admit(&request_a).await.expect("admit operation A");
    let mut completed_a = admitted_a.clone();
    for (expected, next) in [
        (
            WorkEstablishmentPhase::AwaitingGenesis,
            WorkEstablishmentPhase::AwaitingPlan,
        ),
        (
            WorkEstablishmentPhase::AwaitingPlan,
            WorkEstablishmentPhase::AwaitingAssignment,
        ),
        (
            WorkEstablishmentPhase::AwaitingAssignment,
            WorkEstablishmentPhase::Complete,
        ),
    ] {
        completed_a = service
            .advance_phase(&request_a, expected, next)
            .await
            .expect("complete operation A");
    }
    let admitted_b = service
        .admit_with_disposition(&request_b)
        .await
        .expect("admit pending operation B");
    assert_eq!(
        admitted_b.disposition,
        WorkEstablishmentAdmissionDisposition::Created
    );

    let replayed_a = service
        .admit_with_disposition(&request_a)
        .await
        .expect("completed operation A must remain replayable");
    assert_eq!(
        replayed_a.disposition,
        WorkEstablishmentAdmissionDisposition::Existing
    );
    assert_eq!(replayed_a.operation, completed_a);

    let mut mismatched_a = request_a.clone();
    mismatched_a.request_hash = format!("it-work-request-hash-replay-mismatch-{suffix}");
    mismatched_a.payload_json = r#"{"goal":"mutated operation identity"}"#.to_string();
    assert!(matches!(
        service.admit_with_disposition(&mismatched_a).await,
        Err(astra_services::work::WorkEstablishmentError::IdempotencyMismatch)
    ));

    sqlx::query("DELETE FROM work_establishment_operations WHERE owner_id = ? AND session_id = ?")
        .bind(owner_id.as_str())
        .bind(&session_id)
        .execute(shared.get())
        .await
        .expect("cleanup replay admission operations");
    sqlx::query("DELETE FROM agent_sessions WHERE user_id = ? AND session_id = ?")
        .bind(owner_id.as_str())
        .bind(&session_id)
        .execute(shared.get())
        .await
        .expect("cleanup replay session fence");
}

async fn cleanup_skills_by_ids(pool: &sqlx::Pool<sqlx::MySql>, ids: &[String]) {
    for id in ids {
        let _ = sqlx::query("DELETE FROM skills_registry WHERE skill_id = ?")
            .bind(id)
            .execute(pool)
            .await;
    }
}

async fn cleanup_session_bundle(
    pool: &sqlx::Pool<sqlx::MySql>,
    session_id: &str,
    session_id_2: &str,
    user_id: &str,
    event_ids: &[String],
    audit_ids: &[String],
) {
    for eid in event_ids {
        let _ = sqlx::query(
            "DELETE FROM agent_event_edges
             WHERE user_id = ? AND (child_event_id = ? OR parent_event_id = ?)",
        )
        .bind(user_id)
        .bind(eid)
        .bind(eid)
        .execute(pool)
        .await;
        let _ = sqlx::query("DELETE FROM agent_events WHERE event_id = ? AND user_id = ?")
            .bind(eid)
            .bind(user_id)
            .execute(pool)
            .await;
    }
    let _ = sqlx::query("DELETE FROM skill_installations WHERE user_id = ?")
        .bind(user_id)
        .execute(pool)
        .await;
    for aid in audit_ids {
        let _ = sqlx::query("DELETE FROM auth_audit_logs WHERE log_id = ?")
            .bind(aid)
            .execute(pool)
            .await;
    }
    let _ = sqlx::query("DELETE FROM agent_sessions WHERE user_id = ? AND session_id IN (?, ?)")
        .bind(user_id)
        .bind(session_id)
        .bind(session_id_2)
        .execute(pool)
        .await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn skills_registry_index_list_order_and_get_skill_definition() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let idx_row = sqlx::query(
        "SELECT COUNT(*) AS c FROM information_schema.statistics \
         WHERE table_schema = DATABASE() AND table_name = 'skills_registry' \
         AND index_name = 'idx_skill_active_name_ver'",
    )
    .fetch_one(&pool)
    .await
    .expect("information_schema query");
    let index_count: i64 = idx_row
        .try_get("c")
        .expect("decode skills_registry index count");
    assert!(
        index_count >= 1,
        "skills_registry list index must exist, found {index_count}"
    );

    let id_a = format!("{}-a", Uuid::new_v4());
    let id_b = format!("{}-b", Uuid::new_v4());
    let id_c = format!("{}-c", Uuid::new_v4());
    let name_base = format!("zzzz-it-skill-{}", Uuid::new_v4());
    let name_a = format!("{name_base}-a");
    let name_b = format!("{name_base}-b");
    let name_c = format!("{name_base}-c");
    let owner_id = format!("test-user-{}", Uuid::new_v4());

    for (sid, skill_name, ts) in [
        (&id_a, &name_a, "2026-04-01 10:00:00.000000"),
        (&id_b, &name_b, "2026-04-01 12:00:00.000000"),
        (&id_c, &name_c, "2026-04-01 11:00:00.000000"),
    ] {
        sqlx::query(
            "INSERT INTO skills_registry \
             (skill_id, skill_name, version, description, skill_definition, \
              is_active, status, source, category, created_by, created_at, updated_at) \
             VALUES (?, ?, '1.0', 'd', CAST(? AS JSON), 1, 'active', 'user', 'c', ?, ?, ?)",
        )
        .bind(sid)
        .bind(skill_name)
        .bind(serde_json::json!({"marker": sid, "blob": "x".repeat(4000)}).to_string())
        .bind(&owner_id)
        .bind(ts)
        .bind(ts)
        .execute(&pool)
        .await
        .expect("insert skill row");
    }

    let svc = DatabaseSkillService::new(settings.clone()).with_pool(shared.clone());
    let page = svc
        .list_skills(owner_id.clone(), u32::MAX, None)
        .await
        .expect("list_skills");
    assert_eq!(page.limit, MAX_API_LIST_LIMIT);

    let fixture_start_cursor = astra_services::skills::SkillListCursor {
        skill_name: name_base,
        version: "0".to_string(),
        skill_id: "0".to_string(),
    };
    let fixture_page = svc
        .list_skills(owner_id.clone(), 3, Some(fixture_start_cursor.clone()))
        .await
        .expect("list fixture skills");
    let want: HashSet<String> = [id_a.clone(), id_b.clone(), id_c.clone()]
        .into_iter()
        .collect();
    let ours: Vec<_> = fixture_page
        .skills
        .iter()
        .filter(|s| want.contains(&s.skill_id))
        .collect();
    assert_eq!(ours.len(), 3);
    assert_eq!(ours[0].skill_id, id_a, "name seek order");
    assert_eq!(ours[1].skill_id, id_b);
    assert_eq!(ours[2].skill_id, id_c);

    let first_skill_page = svc
        .list_skills(owner_id.clone(), 1, Some(fixture_start_cursor))
        .await
        .expect("first skill page");
    assert_eq!(
        first_skill_page
            .skills
            .iter()
            .filter(|skill| want.contains(&skill.skill_id))
            .map(|skill| skill.skill_id.as_str())
            .collect::<Vec<_>>(),
        vec![id_a.as_str()]
    );
    let second_skill_page = svc
        .list_skills(owner_id.clone(), 1, first_skill_page.next_cursor.clone())
        .await
        .expect("second skill page");
    assert_eq!(
        second_skill_page
            .skills
            .iter()
            .filter(|skill| want.contains(&skill.skill_id))
            .map(|skill| skill.skill_id.as_str())
            .collect::<Vec<_>>(),
        vec![id_b.as_str()]
    );

    let detail = svc
        .get_skill(owner_id.clone(), id_b.clone(), None)
        .await
        .expect("get_skill");
    assert!(
        detail.metadata.is_some(),
        "detail path must still read skill_definition / metadata"
    );

    cleanup_skills_by_ids(&pool, &[id_a, id_b, id_c]).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn skills_registry_visibility_is_user_owned_union_public() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let owner_id = format!("owner-{}", Uuid::new_v4());
    let other_id = format!("other-{}", Uuid::new_v4());
    let owner_private = format!("owner-private-{}", Uuid::new_v4());
    let other_private = format!("other-private-{}", Uuid::new_v4());
    let public_skill = format!("public-{}", Uuid::new_v4());
    let skill_ids = vec![
        format!("{owner_private}@1.0"),
        format!("{other_private}@1.0"),
        format!("{public_skill}@1.0"),
    ];

    for (skill_name, created_by, is_public) in [
        (&owner_private, &owner_id, 0_i16),
        (&other_private, &other_id, 0_i16),
        (&public_skill, &other_id, 1_i16),
    ] {
        sqlx::query(
            "INSERT INTO skills_registry \
             (skill_id, skill_name, version, description, skill_definition, \
              is_active, status, source, is_public, created_by, created_at, updated_at) \
             VALUES (?, ?, '1.0', 'visibility test', CAST(? AS JSON), \
                     1, 'active', 'user', ?, ?, NOW(6), NOW(6))",
        )
        .bind(format!("{skill_name}@1.0"))
        .bind(skill_name)
        .bind(serde_json::json!({"instructions": skill_name}).to_string())
        .bind(is_public)
        .bind(created_by)
        .execute(&pool)
        .await
        .expect("insert visibility skill row");
    }

    let svc = DatabaseSkillService::new(settings).with_pool(shared);
    let owner_page = svc
        .list_skills(owner_id.clone(), 100, None)
        .await
        .expect("owner list_skills should succeed");
    let owner_names: HashSet<_> = owner_page
        .skills
        .iter()
        .map(|skill| skill.skill_name.as_str())
        .collect();
    assert!(
        owner_names.contains(owner_private.as_str()),
        "owner must see their private skill"
    );
    assert!(
        owner_names.contains(public_skill.as_str()),
        "owner must see public skills"
    );
    assert!(
        !owner_names.contains(other_private.as_str()),
        "owner must not see another user's private skill"
    );

    let hidden = svc
        .get_skill(owner_id.clone(), format!("{other_private}@1.0"), None)
        .await
        .expect_err("private skill owned by another user must be hidden");
    assert_eq!(hidden.0, axum::http::StatusCode::NOT_FOUND);

    let public_detail = svc
        .get_skill(owner_id, format!("{public_skill}@1.0"), None)
        .await
        .expect("public skill detail should be visible");
    assert_eq!(public_detail.skill_name, public_skill);

    cleanup_skills_by_ids(&pool, &skill_ids).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn unpublish_skill_is_owner_bound_for_same_name_versions() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let skill_name = format!("owner-bound-unpublish-{}", Uuid::new_v4());
    let owner_a = format!("owner-a-{}", Uuid::new_v4());
    let owner_b = format!("owner-b-{}", Uuid::new_v4());
    let skill_id_a = format!("{skill_name}@1.0.0");
    let skill_id_b = format!("{skill_name}@2.0.0");
    let skill_ids = vec![skill_id_a.clone(), skill_id_b.clone()];
    cleanup_skills_by_ids(&pool, &skill_ids).await;

    for (skill_id, version, owner) in [
        (&skill_id_a, "1.0.0", &owner_a),
        (&skill_id_b, "2.0.0", &owner_b),
    ] {
        sqlx::query(
            "INSERT INTO skills_registry \
             (skill_id, skill_name, version, description, skill_definition, \
              is_active, status, source, is_public, created_by, created_at, updated_at) \
             VALUES (?, ?, ?, 'owner bound unpublish', CAST(? AS JSON), \
                     1, 'active', 'user', 0, ?, NOW(6), NOW(6))",
        )
        .bind(skill_id)
        .bind(&skill_name)
        .bind(version)
        .bind(serde_json::json!({"instructions": skill_id}).to_string())
        .bind(owner)
        .execute(&pool)
        .await
        .expect("insert owner-bound skill row");
    }

    let svc = DatabaseSkillService::new(settings).with_pool(shared);
    let result = svc
        .unpublish_skill(owner_a.clone(), skill_name.clone())
        .await
        .expect("owner A should unpublish only their own active skill version");
    assert_eq!(result["result"], "unpublished");

    let row_a = sqlx::query("SELECT is_active, status FROM skills_registry WHERE skill_id = ?")
        .bind(&skill_id_a)
        .fetch_one(&pool)
        .await
        .expect("owner A skill row");
    assert_eq!(row_a.try_get::<i16, _>("is_active").unwrap(), 0);
    assert_eq!(row_a.try_get::<String, _>("status").unwrap(), "unpublished");

    let row_b = sqlx::query("SELECT is_active, status FROM skills_registry WHERE skill_id = ?")
        .bind(&skill_id_b)
        .fetch_one(&pool)
        .await
        .expect("owner B skill row");
    assert_eq!(
        row_b.try_get::<i16, _>("is_active").unwrap(),
        1,
        "unpublish must not touch another owner's same-name version"
    );
    assert_eq!(row_b.try_get::<String, _>("status").unwrap(), "active");

    cleanup_skills_by_ids(&pool, &skill_ids).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn events_sessions_admin_and_marketplace_installed_pagination() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();
    let session_id_2 = Uuid::new_v4().to_string();
    let e1 = Uuid::new_v4().to_string();
    let e2 = Uuid::new_v4().to_string();
    let e3 = Uuid::new_v4().to_string();
    let installation_id = Uuid::new_v4().to_string();
    let installation_id_2 = Uuid::new_v4().to_string();
    let installation_id_3 = Uuid::new_v4().to_string();
    let skill_name = format!("skill-{}", Uuid::new_v4());
    let skill_name_2 = format!("skill-{}", Uuid::new_v4());
    let skill_name_3 = format!("skill-{}", Uuid::new_v4());
    let a0 = Uuid::new_v4().to_string();
    let a1 = Uuid::new_v4().to_string();
    let a2 = Uuid::new_v4().to_string();
    let activity_id = Uuid::new_v4().to_string();
    let activity_id_2 = Uuid::new_v4().to_string();
    let activity_id_3 = Uuid::new_v4().to_string();
    let audit_ids = vec![
        a0.clone(),
        a1.clone(),
        a2.clone(),
        activity_id.clone(),
        activity_id_2.clone(),
        activity_id_3.clone(),
    ];
    let event_ids = vec![e1.clone(), e2.clone(), e3.clone()];

    cleanup_session_bundle(
        &pool,
        &session_id,
        &session_id_2,
        &user_id,
        &event_ids,
        &audit_ids,
    )
    .await;

    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
         VALUES (?, ?, 'it', 'active', 3)",
    )
    .bind(&session_id)
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("insert session");

    for (eid, ts) in [
        (&e1, "2026-05-01 10:00:00.000000"),
        (&e2, "2026-05-01 12:00:00.000000"),
        (&e3, "2026-05-01 11:00:00.000000"),
    ] {
        sqlx::query(
            "INSERT INTO agent_events (event_id, session_id, user_id, event_type, content, \
             causal_chain_id, payload_hash, ingestion_write_id, created_at) \
             VALUES (?, ?, ?, 'it_evt', '{}', '', ?, ?, ?)",
        )
        .bind(eid)
        .bind(&session_id)
        .bind(&user_id)
        .bind(agent_event_fixture_payload_hash(serde_json::json!({
            "event_id": eid,
            "session_id": &session_id,
            "user_id": &user_id,
            "event_type": "it_evt",
            "content": "{}",
            "causal_chain_id": "",
            "created_at": ts,
        })))
        .bind(Uuid::new_v4().to_string())
        .bind(ts)
        .execute(&pool)
        .await
        .expect("insert event");
    }

    let ev = DatabaseEventService::new(settings.clone()).with_pool(shared.clone());
    let listed = ev
        .list_events(EventListFilter {
            user_id: user_id.clone(),
            session_id: None,
            event_type: None,
            agent_id: None,
            causal_chain_id: None,
            limit: u32::MAX,
            cursor: None,
        })
        .await
        .expect("list_events");
    assert_eq!(listed.limit, MAX_API_LIST_LIMIT);
    assert_eq!(listed.events.len(), 3);
    assert!(listed.events[0].created_at >= listed.events[1].created_at);
    assert!(listed.next_cursor.is_none());

    let first_event_page = ev
        .list_events(EventListFilter {
            user_id: user_id.clone(),
            session_id: None,
            event_type: None,
            agent_id: None,
            causal_chain_id: None,
            limit: 2,
            cursor: None,
        })
        .await
        .expect("list first event page");
    assert_eq!(
        first_event_page
            .events
            .iter()
            .map(|event| event.event_id.as_str())
            .collect::<Vec<_>>(),
        vec![e2.as_str(), e3.as_str()]
    );
    let second_event_page = ev
        .list_events(EventListFilter {
            user_id: user_id.clone(),
            session_id: None,
            event_type: None,
            agent_id: None,
            causal_chain_id: None,
            limit: 2,
            cursor: first_event_page.next_cursor.clone(),
        })
        .await
        .expect("list second event page");
    assert_eq!(
        second_event_page
            .events
            .iter()
            .map(|event| event.event_id.as_str())
            .collect::<Vec<_>>(),
        vec![e1.as_str()]
    );
    assert!(second_event_page.next_cursor.is_none());

    let first_session_page = ev
        .get_session_events(session_id.clone(), user_id.clone(), 2, None)
        .await
        .expect("session events first page");
    assert_eq!(first_session_page.total, Some(3));
    assert_eq!(
        first_session_page
            .events
            .iter()
            .map(|event| event.event_id.as_str())
            .collect::<Vec<_>>(),
        vec![e1.as_str(), e3.as_str()]
    );
    let second_session_page = ev
        .get_session_events(
            session_id.clone(),
            user_id.clone(),
            2,
            first_session_page.next_cursor.clone(),
        )
        .await
        .expect("session events second page");
    assert_eq!(
        second_session_page
            .events
            .iter()
            .map(|event| event.event_id.as_str())
            .collect::<Vec<_>>(),
        vec![e2.as_str()]
    );
    assert!(second_session_page.next_cursor.is_none());

    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
         VALUES (?, ?, 'it2', 'active', 0)",
    )
    .bind(&session_id_2)
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("insert session 2");

    for (sid, ts) in [
        (&session_id, "2026-05-01 12:00:00.000000"),
        (&session_id_2, "2026-05-01 10:00:00.000000"),
    ] {
        sqlx::query(
            "UPDATE agent_sessions SET created_at = ?, updated_at = ?, last_active_at = ? \
             WHERE session_id = ? AND user_id = ?",
        )
        .bind(ts)
        .bind(ts)
        .bind(ts)
        .bind(sid)
        .bind(&user_id)
        .execute(&pool)
        .await
        .expect("update session timestamps");
    }

    let sess = DatabaseSessionService::new(settings.clone()).with_pool(shared.clone());
    let slist = sess
        .list_sessions(SessionListFilter {
            user_id: user_id.clone(),
            agent_id: None,
            status: None,
            limit: 50_000,
            cursor: None,
        })
        .await
        .expect("list_sessions");
    assert_eq!(slist.limit, MAX_API_LIST_LIMIT);

    let first_session_page = sess
        .list_sessions(SessionListFilter {
            user_id: user_id.clone(),
            agent_id: None,
            status: None,
            limit: 1,
            cursor: None,
        })
        .await
        .expect("list first session page");
    assert_eq!(
        first_session_page
            .sessions
            .iter()
            .map(|session| session.session_id.as_str())
            .collect::<Vec<_>>(),
        vec![session_id.as_str()]
    );
    let second_session_page = sess
        .list_sessions(SessionListFilter {
            user_id: user_id.clone(),
            agent_id: None,
            status: None,
            limit: 1,
            cursor: first_session_page.next_cursor.clone(),
        })
        .await
        .expect("list second session page");
    assert_eq!(
        second_session_page
            .sessions
            .iter()
            .map(|session| session.session_id.as_str())
            .collect::<Vec<_>>(),
        vec![session_id_2.as_str()]
    );
    assert!(second_session_page.next_cursor.is_none());

    for (log_id, action, ts) in [
        (
            &activity_id,
            "it_session_activity_1",
            "2026-05-01 10:00:00.000000",
        ),
        (
            &activity_id_2,
            "it_session_activity_2",
            "2026-05-01 12:00:00.000000",
        ),
        (
            &activity_id_3,
            "it_session_activity_3",
            "2026-05-01 11:00:00.000000",
        ),
    ] {
        sqlx::query(
            "INSERT INTO auth_audit_logs \
             (log_id, user_id, action, resource_type, resource_id, details, created_at) \
             VALUES (?, ?, ?, 'session', ?, CAST('{}' AS JSON), ?)",
        )
        .bind(log_id)
        .bind(&user_id)
        .bind(action)
        .bind(&session_id)
        .bind(ts)
        .execute(&pool)
        .await
        .expect("insert session activity");
    }

    let first_activity_page = sess
        .get_session_activity(session_id.clone(), user_id.clone(), 2, None)
        .await
        .expect("list first session activity page");
    assert_eq!(first_activity_page.total, 3);
    assert_eq!(first_activity_page.limit, 2);
    assert_eq!(
        first_activity_page
            .activities
            .iter()
            .map(|activity| activity.log_id.as_str())
            .collect::<Vec<_>>(),
        vec![activity_id_2.as_str(), activity_id_3.as_str()]
    );
    let second_activity_page = sess
        .get_session_activity(
            session_id.clone(),
            user_id.clone(),
            2,
            first_activity_page.next_cursor.clone(),
        )
        .await
        .expect("list second session activity page");
    assert_eq!(
        second_activity_page
            .activities
            .iter()
            .map(|activity| activity.log_id.as_str())
            .collect::<Vec<_>>(),
        vec![activity_id.as_str()]
    );
    assert!(second_activity_page.next_cursor.is_none());

    for (log_id, k) in [(a0.clone(), 0_i32), (a1.clone(), 1), (a2.clone(), 2)] {
        sqlx::query(
            "INSERT INTO auth_audit_logs (log_id, user_id, action, resource_type, resource_id, details) \
             VALUES (?, ?, ?, 'it_res', 'rid', CAST('{}' AS JSON))",
        )
        .bind(&log_id)
        .bind(&user_id)
        .bind(format!("it_svc_{k}"))
        .execute(&pool)
        .await
        .expect("insert audit");
    }

    let audit = DatabaseAdminAuditReader::new(settings.clone()).with_pool(shared.clone());
    let logs = audit
        .list_audit_logs(AdminAuditFilter {
            user_id: Some(user_id.clone()),
            since: None,
            limit: 9999,
        })
        .await
        .expect("list_audit_logs");
    assert_eq!(logs.len(), 6);
    let audit_actions = logs
        .iter()
        .map(|record| record.action.as_str())
        .collect::<HashSet<_>>();
    for expected_action in [
        "it_session_activity_1",
        "it_session_activity_2",
        "it_session_activity_3",
        "it_svc_0",
        "it_svc_1",
        "it_svc_2",
    ] {
        assert!(
            audit_actions.contains(expected_action),
            "missing expected audit action {expected_action}"
        );
    }

    for (installation_id, skill_name, ts) in [
        (&installation_id, &skill_name, "2026-05-01 10:00:00.000000"),
        (
            &installation_id_2,
            &skill_name_2,
            "2026-05-01 12:00:00.000000",
        ),
        (
            &installation_id_3,
            &skill_name_3,
            "2026-05-01 11:00:00.000000",
        ),
    ] {
        sqlx::query(
            "INSERT INTO skill_installations \
             (installation_id, user_id, skill_name, skill_version, status, installed_at, updated_at) \
             VALUES (?, ?, ?, '1.0.0', 'installed', ?, ?)",
        )
        .bind(installation_id)
        .bind(&user_id)
        .bind(skill_name)
        .bind(ts)
        .bind(ts)
        .execute(&pool)
        .await
        .expect("insert skill installation");
    }

    let marketplace = DatabaseMarketplaceService::new(settings.clone()).with_pool(shared.clone());
    let first_installed_page = marketplace
        .list_installed(user_id.clone(), 2, None)
        .await
        .expect("list first installed page");
    assert_eq!(
        first_installed_page
            .installations
            .iter()
            .map(|installation| installation.installation_id.as_str())
            .collect::<Vec<_>>(),
        vec![installation_id_2.as_str(), installation_id_3.as_str()]
    );
    let second_installed_page = marketplace
        .list_installed(user_id.clone(), 2, first_installed_page.next_cursor.clone())
        .await
        .expect("list second installed page");
    assert_eq!(
        second_installed_page
            .installations
            .iter()
            .map(|installation| installation.installation_id.as_str())
            .collect::<Vec<_>>(),
        vec![installation_id.as_str()]
    );
    assert!(second_installed_page.next_cursor.is_none());

    cleanup_session_bundle(
        &pool,
        &session_id,
        &session_id_2,
        &user_id,
        &event_ids,
        &audit_ids,
    )
    .await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn get_session_events_uses_session_event_count_summary_without_event_scan() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let session_id = Uuid::new_v4().to_string();
    let user_id = Uuid::new_v4().to_string();
    let event_id = Uuid::new_v4().to_string();
    cleanup_agent_sessions_and_events_for_owner(
        &pool,
        &user_id,
        std::slice::from_ref(&session_id),
        std::slice::from_ref(&event_id),
    )
    .await;

    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
         VALUES (?, ?, 'session-event-summary', 'active', 7)",
    )
    .bind(&session_id)
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("insert session root");
    let causal_chain_id = Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO agent_events (event_id, session_id, user_id, event_type, content, \
         causal_chain_id, payload_hash, ingestion_write_id) \
         VALUES (?, ?, ?, 'raw_event', 'not authoritative for session total', ?, ?, ?)",
    )
    .bind(&event_id)
    .bind(&session_id)
    .bind(&user_id)
    .bind(&causal_chain_id)
    .bind(agent_event_fixture_payload_hash(serde_json::json!({
        "event_id": &event_id,
        "session_id": &session_id,
        "user_id": &user_id,
        "event_type": "raw_event",
        "content": "not authoritative for session total",
        "causal_chain_id": &causal_chain_id,
    })))
    .bind(Uuid::new_v4().to_string())
    .execute(&pool)
    .await
    .expect("insert raw event row");

    let event_service = DatabaseEventService::new(settings).with_pool(shared);
    let events = event_service
        .get_session_events(session_id.clone(), user_id.clone(), 50, None)
        .await
        .expect("get session events");

    assert_eq!(
        events.total,
        Some(7),
        "session event total should use agent_sessions.event_count summary, not COUNT(agent_events)"
    );
    assert_eq!(events.events.len(), 1);
    assert_eq!(events.events[0].event_id, event_id);

    let listed = event_service
        .list_events(EventListFilter {
            user_id: user_id.clone(),
            session_id: Some(session_id.clone()),
            event_type: None,
            agent_id: None,
            causal_chain_id: None,
            limit: 50,
            cursor: None,
        })
        .await
        .expect("list unfiltered session events");
    assert_eq!(
        listed.total,
        Some(7),
        "unfiltered list_events session total should use agent_sessions.event_count summary"
    );

    cleanup_agent_sessions_and_events_for_owner(&pool, &user_id, &[session_id], &[event_id]).await;
}

/// Matches [`astra_services::session_audit::MAX_AUDIT_SESSIONS_PER_PAGE`] (not exported).
const MAX_AUDIT_SESSIONS_PER_PAGE: u32 = 100;

async fn cleanup_agent_sessions_and_events_for_owner(
    pool: &sqlx::Pool<sqlx::MySql>,
    user_id: &str,
    session_ids: &[String],
    event_ids: &[String],
) {
    for sid in session_ids {
        let _ = sqlx::query("DELETE FROM agent_event_edges WHERE session_id = ? AND user_id = ?")
            .bind(sid)
            .bind(user_id)
            .execute(pool)
            .await;
    }
    for eid in event_ids {
        let _ = sqlx::query("DELETE FROM agent_events WHERE event_id = ? AND user_id = ?")
            .bind(eid)
            .bind(user_id)
            .execute(pool)
            .await;
    }
}

async fn count_user_session_rows(
    pool: &sqlx::Pool<sqlx::MySql>,
    table: &'static str,
    user_id: &str,
    session_id: &str,
) -> i64 {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT COUNT(*) AS c FROM {table} WHERE session_id = ? AND user_id = ?"
    )))
    .bind(session_id)
    .bind(user_id)
    .fetch_one(pool)
    .await
    .unwrap_or_else(|error| panic!("count {table} rows for owner/session: {error}"))
    .try_get::<i64, _>("c")
    .expect("decode owner/session row count")
}

async fn load_session_delete_audit_details(
    pool: &sqlx::Pool<sqlx::MySql>,
    user_id: &str,
    session_id: &str,
) -> serde_json::Value {
    let details_json: Option<String> = sqlx::query_scalar(
        "SELECT CAST(details AS CHAR) \
         FROM auth_audit_logs \
         WHERE user_id = ? \
           AND action = 'session_delete' \
           AND resource_type = 'session' \
           AND resource_id = ? \
         ORDER BY created_at DESC, log_id DESC \
         LIMIT 1",
    )
    .bind(user_id)
    .bind(session_id)
    .fetch_optional(pool)
    .await
    .expect("load session delete audit details")
    .flatten();
    let details_json = details_json.expect("session_delete audit details must exist");
    serde_json::from_str(&details_json).unwrap_or_else(|error| {
        panic!("session_delete audit details must be valid JSON: {error}; raw={details_json}")
    })
}

fn deleted_rows_for_table(details: &serde_json::Value, label: &str) -> u64 {
    let tables = details
        .get("database_tables_deleted")
        .and_then(serde_json::Value::as_array)
        .expect("database_tables_deleted must be an array");
    let table = tables
        .iter()
        .find(|table| table.get("label").and_then(serde_json::Value::as_str) == Some(label))
        .unwrap_or_else(|| panic!("database_tables_deleted missing {label}: {tables:?}"));
    table
        .get("rows_deleted")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_else(|| panic!("database_tables_deleted.{label}.rows_deleted must be u64"))
}

async fn cleanup_session_delete_fixture_for_owner(
    pool: &sqlx::Pool<sqlx::MySql>,
    user_id: &str,
    session_id: &str,
) {
    let _ = sqlx::query(
        "DELETE FROM auth_audit_logs \
         WHERE user_id = ? AND resource_type = 'session' AND resource_id = ?",
    )
    .bind(user_id)
    .bind(session_id)
    .execute(pool)
    .await;

    for table in [
        "session_artifacts",
        "agent_event_edges",
        "agent_events",
        "agent_runs",
        "agent_sessions",
    ] {
        let sql = format!("DELETE FROM {table} WHERE session_id = ? AND user_id = ?");
        let _ = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(session_id)
            .bind(user_id)
            .execute(pool)
            .await;
    }

    for table in ["workspace_records", "workspace_cleanup_debts"] {
        let sql = format!("DELETE FROM {table} WHERE session_id = ? AND owner_id = ?");
        let _ = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(session_id)
            .bind(user_id)
            .execute(pool)
            .await;
    }
}

fn create_owner_local_session_files(
    user_id: &str,
    session_id: &str,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let owner_scope = astra_services::OwnerScope::user(user_id).expect("owner scope");
    let owner_journal_path =
        astra_services::session_journal::journal_file_path_for_user(user_id, session_id)
            .expect("owner journal path");
    let owner_session_dir = astra_services::local_session_artifact_store()
        .session_dir_for_owner(&owner_scope, session_id)
        .expect("owner session dir");
    let owner_checkpoint_path = owner_session_dir
        .join("step_checkpoints")
        .join("000001-heavy.json");
    let owner_artifact_path = owner_session_dir.join("artifacts").join("output.json");
    std::fs::create_dir_all(owner_journal_path.parent().expect("journal parent"))
        .expect("create journal parent");
    std::fs::create_dir_all(owner_checkpoint_path.parent().expect("checkpoint parent"))
        .expect("create checkpoint parent");
    std::fs::create_dir_all(owner_artifact_path.parent().expect("artifact parent"))
        .expect("create artifact parent");
    std::fs::write(&owner_journal_path, "{}").expect("write owner journal");
    std::fs::write(&owner_checkpoint_path, "{}").expect("write owner checkpoint");
    std::fs::write(&owner_artifact_path, "{}").expect("write owner artifact");
    (owner_journal_path, owner_session_dir)
}

async fn cleanup_restore_fixture_for_owner(
    pool: &sqlx::Pool<sqlx::MySql>,
    user_id: &str,
    session_ids: &[String],
) {
    for sid in session_ids {
        for table in [
            "agent_event_edges",
            "session_artifact_references",
            "session_artifacts",
            "agent_events",
            "agent_sessions",
            "agent_session_lifecycle_fences",
        ] {
            let sql = format!("DELETE FROM {table} WHERE session_id = ? AND user_id = ?");
            let _ = sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(sid)
                .bind(user_id)
                .execute(pool)
                .await;
        }
    }
}

async fn cleanup_restore_fixture_for_owners(
    pool: &sqlx::Pool<sqlx::MySql>,
    session_ids: &[String],
    user_ids: &[&str],
) {
    for user_id in user_ids {
        cleanup_restore_fixture_for_owner(pool, user_id, session_ids).await;
    }
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn bump_agent_session_event_count_applies_delta_without_count_reconcile() {
    let (shared, _settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let session_id = Uuid::new_v4().to_string();
    let user_id = Uuid::new_v4().to_string();
    let other_user_id = Uuid::new_v4().to_string();
    cleanup_restore_fixture_for_owner(&pool, &user_id, std::slice::from_ref(&session_id)).await;
    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
         VALUES (?, ?, 'delta-event-count', 'active', 0)",
    )
    .bind(&session_id)
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("insert session root");

    astra_services::storage::bump_agent_session_event_count(
        &pool,
        &session_id,
        &user_id,
        3,
        Some("event-3"),
    )
    .await
    .expect("positive delta");
    astra_services::storage::bump_agent_session_event_count(&pool, &session_id, &user_id, -1, None)
        .await
        .expect("negative delta");
    astra_services::storage::bump_agent_session_event_count(
        &pool,
        &session_id,
        &user_id,
        -99,
        None,
    )
    .await
    .expect("saturating negative delta");

    let row = sqlx::query(
        "SELECT event_count, last_event_id FROM agent_sessions WHERE session_id = ? AND user_id = ?",
    )
    .bind(&session_id)
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .expect("load session root");
    assert_eq!(row.try_get::<i64, _>("event_count").unwrap(), 0);
    assert_eq!(
        row.try_get::<Option<String>, _>("last_event_id").unwrap(),
        Some("event-3".to_string())
    );

    astra_services::storage::touch_agent_session_activity(
        &pool,
        &session_id,
        &user_id,
        Some("event-activity"),
    )
    .await
    .expect("touch activity for existing owner");
    let row = sqlx::query(
        "SELECT event_count, last_event_id FROM agent_sessions WHERE session_id = ? AND user_id = ?",
    )
    .bind(&session_id)
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .expect("load touched session root");
    assert_eq!(row.try_get::<i64, _>("event_count").unwrap(), 0);
    assert_eq!(
        row.try_get::<Option<String>, _>("last_event_id").unwrap(),
        Some("event-activity".to_string())
    );

    let missing_owner = astra_services::storage::bump_agent_session_event_count(
        &pool,
        &session_id,
        &other_user_id,
        1,
        Some("event-other"),
    )
    .await;
    assert!(
        matches!(missing_owner, Err(sqlx::Error::RowNotFound)),
        "owner mismatch must fail instead of creating or stealing a session: {missing_owner:?}"
    );

    let missing_touch = astra_services::storage::touch_agent_session_activity(
        &pool,
        &session_id,
        &other_user_id,
        Some("event-other"),
    )
    .await;
    assert!(
        matches!(missing_touch, Err(sqlx::Error::RowNotFound)),
        "owner mismatch activity touch must fail instead of creating or stealing a session: {missing_touch:?}"
    );

    cleanup_restore_fixture_for_owner(&pool, &user_id, &[session_id]).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn add_agent_session_event_count_or_create_is_owner_bound_delta_upsert() {
    let (shared, _settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let session_id = Uuid::new_v4().to_string();
    let owner_user_id = Uuid::new_v4().to_string();
    let other_user_id = Uuid::new_v4().to_string();
    cleanup_restore_fixture_for_owners(
        &pool,
        std::slice::from_ref(&session_id),
        &[&owner_user_id, &other_user_id],
    )
    .await;

    let mut tx = pool.begin().await.expect("begin owner session count tx");
    astra_services::storage::add_agent_session_event_count_or_create(
        &mut tx,
        &session_id,
        &owner_user_id,
        2,
        Some("event-2"),
    )
    .await
    .expect("create owner session count");
    tx.commit().await.expect("commit owner session count tx");

    let mut tx = pool.begin().await.expect("begin owner session delta tx");
    astra_services::storage::add_agent_session_event_count_or_create(
        &mut tx,
        &session_id,
        &owner_user_id,
        3,
        None,
    )
    .await
    .expect("same-owner delta upsert");
    tx.commit().await.expect("commit owner session delta tx");

    let row = sqlx::query(
        "SELECT user_id, status, event_count, last_event_id FROM agent_sessions WHERE session_id = ? AND user_id = ?",
    )
    .bind(&session_id)
    .bind(&owner_user_id)
    .fetch_one(&pool)
    .await
    .expect("load owner session count");
    assert_eq!(
        row.try_get::<String, _>("user_id").expect("decode user_id"),
        owner_user_id
    );
    assert_eq!(
        row.try_get::<String, _>("status").expect("decode status"),
        "active"
    );
    assert_eq!(
        row.try_get::<i64, _>("event_count")
            .expect("decode event_count"),
        5
    );
    assert_eq!(
        row.try_get::<Option<String>, _>("last_event_id")
            .expect("decode last_event_id"),
        Some("event-2".to_string()),
        "None last_event_id delta must preserve the previous summary pointer"
    );

    let mut tx = pool.begin().await.expect("begin foreign owner tx");
    astra_services::storage::add_agent_session_event_count_or_create(
        &mut tx,
        &session_id,
        &other_user_id,
        1,
        Some("event-other"),
    )
    .await
    .expect("the second owner has an independent session identity");
    tx.commit().await.expect("commit second owner tx");
    let other_count: i64 = sqlx::query_scalar(
        "SELECT event_count FROM agent_sessions WHERE session_id = ? AND user_id = ?",
    )
    .bind(&session_id)
    .bind(&other_user_id)
    .fetch_one(&pool)
    .await
    .expect("load second owner session count");
    assert_eq!(other_count, 1);

    let mut tx = pool.begin().await.expect("begin negative delta tx");
    let negative_delta = astra_services::storage::add_agent_session_event_count_or_create(
        &mut tx,
        &session_id,
        &owner_user_id,
        -1,
        Some("event-negative"),
    )
    .await;
    tx.rollback().await.expect("rollback negative delta tx");
    assert!(
        matches!(negative_delta, Err(sqlx::Error::Protocol(_))),
        "create-or-add helper only supports non-negative insert deltas: {negative_delta:?}"
    );

    let row = sqlx::query(
        "SELECT user_id, event_count, last_event_id FROM agent_sessions WHERE session_id = ? AND user_id = ?",
    )
    .bind(&session_id)
    .bind(&owner_user_id)
    .fetch_one(&pool)
    .await
    .expect("load unchanged owner session count");
    assert_eq!(
        row.try_get::<String, _>("user_id").expect("decode user_id"),
        owner_user_id
    );
    assert_eq!(
        row.try_get::<i64, _>("event_count")
            .expect("decode event_count"),
        5
    );
    assert_eq!(
        row.try_get::<Option<String>, _>("last_event_id")
            .expect("decode last_event_id"),
        Some("event-2".to_string())
    );

    cleanup_restore_fixture_for_owners(
        &pool,
        std::slice::from_ref(&session_id),
        &[&owner_user_id, &other_user_id],
    )
    .await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn session_artifact_latest_and_list_use_stable_tiebreaker_for_tied_timestamps() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();
    let older_id = Uuid::now_v7().to_string();
    let newer_id = loop {
        let candidate = Uuid::now_v7().to_string();
        if candidate > older_id {
            break candidate;
        }
    };
    let tied_ts = "2026-10-01 12:34:56.123456";

    cleanup_restore_fixture_for_owner(&pool, &user_id, std::slice::from_ref(&session_id)).await;

    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
         VALUES (?, ?, 'artifact-ordering', 'active', 0)",
    )
    .bind(&session_id)
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("insert session");

    for (artifact_id, turn, marker) in [(&older_id, 1_i32, "older"), (&newer_id, 2_i32, "newer")] {
        sqlx::query(
            "INSERT INTO session_artifacts \
             (artifact_id, session_id, user_id, artifact_kind, source, turn, round, content_json, metadata, created_at) \
             VALUES (?, ?, ?, 'llm_capture', 'ordering_probe', ?, 0, ?, CAST(? AS JSON), ?)",
        )
        .bind(artifact_id)
        .bind(&session_id)
        .bind(&user_id)
        .bind(turn)
        .bind(
            serde_json::json!({
                "response": { "full_text": marker }
            })
            .to_string(),
        )
        .bind(serde_json::json!({ "marker": marker }).to_string())
        .bind(tied_ts)
        .execute(&pool)
        .await
        .expect("insert session artifact");
    }

    let store = DatabaseSessionArtifactStore::new(settings).with_pool(shared);
    let latest = store
        .load_latest_json_artifact(&user_id, &session_id, "llm_capture")
        .await
        .expect("load latest artifact")
        .expect("latest artifact row");
    assert_eq!(
        latest.artifact_id, newer_id,
        "latest artifact selection must stay deterministic when created_at ties"
    );
    assert_eq!(
        latest.content["response"]["full_text"].as_str(),
        Some("newer"),
        "latest artifact should surface the newest payload under a tied timestamp"
    );

    let listed = store
        .list_json_artifacts(&user_id, &session_id, Some("llm_capture"), 10, None)
        .await
        .expect("list session artifacts");
    assert_eq!(listed.artifacts.len(), 2);
    assert_eq!(
        listed.artifacts[0].artifact_id, newer_id,
        "artifact lists should use the same stable latest-first ordering"
    );
    assert_eq!(listed.artifacts[1].artifact_id, older_id);
    assert!(listed.next_cursor.is_none());

    let first_page = store
        .list_json_artifacts(&user_id, &session_id, Some("llm_capture"), 1, None)
        .await
        .expect("list first artifact page");
    assert_eq!(first_page.artifacts.len(), 1);
    assert_eq!(first_page.artifacts[0].artifact_id, newer_id);
    let second_page = store
        .list_json_artifacts(
            &user_id,
            &session_id,
            Some("llm_capture"),
            1,
            first_page.next_cursor.clone(),
        )
        .await
        .expect("list second artifact page");
    assert_eq!(second_page.artifacts.len(), 1);
    assert_eq!(second_page.artifacts[0].artifact_id, older_id);
    assert!(second_page.next_cursor.is_none());

    cleanup_restore_fixture_for_owner(&pool, &user_id, &[session_id]).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn session_artifact_persist_uses_microsecond_created_at_for_latest_ordering() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();
    let first_id = format!("zz-{}", Uuid::new_v4());
    let second_id = format!("aa-{}", Uuid::new_v4());

    cleanup_restore_fixture_for_owner(&pool, &user_id, std::slice::from_ref(&session_id)).await;
    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
         VALUES (?, ?, 'artifact-created-at-precision', 'active', 0)",
    )
    .bind(&session_id)
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("insert session");

    let store = DatabaseSessionArtifactStore::new(settings).with_pool(shared);
    let first = store
        .persist_json_artifact(astra_services::SessionArtifactJsonRecord {
            artifact_id: first_id.clone(),
            session_id: session_id.clone(),
            user_id: user_id.clone(),
            artifact_kind: "llm_capture".into(),
            source: Some("created-at-precision-test".into()),
            turn: Some(1),
            round: Some(0),
            content: serde_json::json!({"marker": "first"}),
            metadata: None,
            references: Vec::new(),
        })
        .await
        .expect("persist first artifact");
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let second = store
        .persist_json_artifact(astra_services::SessionArtifactJsonRecord {
            artifact_id: second_id.clone(),
            session_id: session_id.clone(),
            user_id: user_id.clone(),
            artifact_kind: "llm_capture".into(),
            source: Some("created-at-precision-test".into()),
            turn: Some(2),
            round: Some(0),
            content: serde_json::json!({"marker": "second"}),
            metadata: None,
            references: Vec::new(),
        })
        .await
        .expect("persist second artifact");

    let created_ats = [
        first.created_at.as_deref().expect("first created_at"),
        second.created_at.as_deref().expect("second created_at"),
    ];
    assert!(
        created_ats.iter().all(|created_at| {
            created_at
                .rsplit_once('.')
                .is_some_and(|(_, fraction)| fraction.len() == 6)
        }),
        "persisted artifacts must expose DATETIME(6) created_at values: {created_ats:?}"
    );
    assert!(
        created_ats.iter().any(|created_at| {
            created_at
                .rsplit_once('.')
                .is_some_and(|(_, fraction)| fraction != "000000")
        }),
        "artifact writes must use sub-second database time, not second-precision NOW(): {created_ats:?}"
    );

    let latest = store
        .load_latest_json_artifact(&user_id, &session_id, "llm_capture")
        .await
        .expect("load latest")
        .expect("latest artifact");
    assert_eq!(
        latest.artifact_id, second_id,
        "latest ordering must prefer the later created_at even when its artifact_id sorts lower"
    );

    cleanup_restore_fixture_for_owner(&pool, &user_id, &[session_id]).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn session_artifact_commit_and_reload_reuses_a_one_connection_pool() {
    let (_, mut settings) = setup_pool_and_settings().await;
    settings.db_pool_min_connections = 0;
    settings.db_pool_max_connections = 1;
    let shared = SharedPool::new(&settings)
        .await
        .expect("create one-connection artifact pool");
    let pool = shared.get().clone();
    let user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();
    let artifact_id = Uuid::new_v4().to_string();

    cleanup_restore_fixture_for_owner(&pool, &user_id, std::slice::from_ref(&session_id)).await;
    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
         VALUES (?, ?, 'artifact-one-connection', 'active', 0)",
    )
    .bind(&session_id)
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("insert one-connection artifact session");

    let store = DatabaseSessionArtifactStore::new(settings).with_pool(shared);
    let persisted = store
        .persist_json_artifact(astra_services::SessionArtifactJsonRecord {
            artifact_id: artifact_id.clone(),
            session_id: session_id.clone(),
            user_id: user_id.clone(),
            artifact_kind: "llm_capture".into(),
            source: Some("one-connection-regression".into()),
            turn: Some(1),
            round: Some(0),
            content: serde_json::json!({"marker": "committed"}),
            metadata: None,
            references: Vec::new(),
        })
        .await
        .expect("persist artifact and reload it after commit");
    assert_eq!(persisted.artifact_id, artifact_id);
    let reloaded = store
        .load_json_artifact(&user_id, &session_id, &artifact_id)
        .await
        .expect("reload artifact with the same one-connection pool")
        .expect("persisted artifact exists");
    assert_eq!(reloaded.content["marker"], "committed");

    cleanup_restore_fixture_for_owner(&pool, &user_id, &[session_id]).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn session_artifact_and_durable_reference_persist_atomically() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();
    let user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();
    let artifact_id = Uuid::new_v4().to_string();
    cleanup_restore_fixture_for_owner(&pool, &user_id, std::slice::from_ref(&session_id)).await;
    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
         VALUES (?, ?, 'artifact-reference-atomicity', 'active', 0)",
    )
    .bind(&session_id)
    .bind(&user_id)
    .execute(&pool)
    .await
    .unwrap();

    let store = DatabaseSessionArtifactStore::new(settings).with_pool(shared);
    store
        .persist_json_artifact(astra_services::SessionArtifactJsonRecord {
            artifact_id: artifact_id.clone(),
            session_id: session_id.clone(),
            user_id: user_id.clone(),
            artifact_kind: "tool_result_evidence_v1".to_string(),
            source: Some("runtime_tool_invocation".to_string()),
            turn: Some(1),
            round: None,
            content: serde_json::json!({"result": "governed"}),
            metadata: None,
            references: vec![SessionArtifactReference {
                kind: SessionArtifactReferenceKind::InvocationLedger,
                reference_id: "sha256:logical-invocation".to_string(),
            }],
        })
        .await
        .unwrap();

    let row = sqlx::query(
        "SELECT
            (SELECT COUNT(*) FROM session_artifact_references
             WHERE user_id = ? AND session_id = ? AND artifact_id = ?
               AND reference_kind = 'invocation_ledger'
               AND reference_id = 'sha256:logical-invocation') AS reference_count,
            (SELECT CASE WHEN retention_until IS NOT NULL THEN 1 ELSE 0 END FROM session_artifacts
             WHERE user_id = ? AND session_id = ? AND artifact_id = ?) AS has_retention",
    )
    .bind(&user_id)
    .bind(&session_id)
    .bind(&artifact_id)
    .bind(&user_id)
    .bind(&session_id)
    .bind(&artifact_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.try_get::<i64, _>("reference_count").unwrap(), 1);
    assert_eq!(row.try_get::<i64, _>("has_retention").unwrap(), 1);

    let replayable_reference = SessionArtifactReference {
        kind: SessionArtifactReferenceKind::Manifest,
        reference_id: "manifest:durable-owner".to_string(),
    };
    sqlx::query(
        "UPDATE session_artifacts
         SET retention_until = TIMESTAMPADD(DAY, 365, CURRENT_TIMESTAMP(6))
         WHERE user_id = ? AND session_id = ? AND artifact_id = ?",
    )
    .bind(&user_id)
    .bind(&session_id)
    .bind(&artifact_id)
    .execute(&pool)
    .await
    .unwrap();
    assert!(
        store
            .retain_json_artifact_reference(
                &user_id,
                &session_id,
                &artifact_id,
                &replayable_reference,
            )
            .await
            .unwrap(),
        "the first retain must create the durable edge"
    );
    assert!(
        !store
            .retain_json_artifact_reference(
                &user_id,
                &session_id,
                &artifact_id,
                &replayable_reference,
            )
            .await
            .unwrap(),
        "retain replay must be idempotent"
    );
    let retained_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM session_artifact_references
         WHERE user_id = ? AND session_id = ? AND artifact_id = ?
           AND reference_kind = 'manifest' AND reference_id = ?",
    )
    .bind(&user_id)
    .bind(&session_id)
    .bind(&artifact_id)
    .bind(&replayable_reference.reference_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(retained_count, 1);
    let loaded = store
        .load_json_artifact(&user_id, &session_id, &artifact_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(loaded.referenced_by_durable_count, 2);
    let references = store
        .list_json_artifact_references(&user_id, &session_id, &artifact_id, 100)
        .await
        .unwrap();
    assert_eq!(references.len(), 2);
    assert!(references.contains(&replayable_reference));
    assert!(references.contains(&SessionArtifactReference {
        kind: SessionArtifactReferenceKind::InvocationLedger,
        reference_id: "sha256:logical-invocation".to_string(),
    }));
    assert_eq!(
        store
            .list_json_artifacts_for_reference(&user_id, &session_id, &replayable_reference, 100,)
            .await
            .unwrap(),
        vec![artifact_id.clone()]
    );
    let retention_days: i64 = sqlx::query_scalar(
        "SELECT TIMESTAMPDIFF(DAY, CURRENT_TIMESTAMP(6), retention_until)
         FROM session_artifacts
         WHERE user_id = ? AND session_id = ? AND artifact_id = ?",
    )
    .bind(&user_id)
    .bind(&session_id)
    .bind(&artifact_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        retention_days >= 364,
        "retaining an existing edge must never shorten a longer retention policy"
    );

    let foreign_release = store
        .release_json_artifact_reference(
            "another-user",
            &session_id,
            &artifact_id,
            &replayable_reference,
        )
        .await;
    assert!(matches!(
        foreign_release,
        Err(SessionArtifactStoreError::SessionNotOwned { .. })
    ));
    assert!(
        store
            .release_json_artifact_reference(
                &user_id,
                &session_id,
                &artifact_id,
                &replayable_reference,
            )
            .await
            .unwrap(),
        "the first release must remove the durable edge"
    );
    assert!(
        !store
            .release_json_artifact_reference(
                &user_id,
                &session_id,
                &artifact_id,
                &replayable_reference,
            )
            .await
            .unwrap(),
        "release replay must be idempotent"
    );
    assert!(
        store
            .list_json_artifacts_for_reference(&user_id, &session_id, &replayable_reference, 100,)
            .await
            .unwrap()
            .is_empty()
    );

    sqlx::query(
        "UPDATE session_artifacts SET status = 'expiring'
         WHERE user_id = ? AND session_id = ? AND artifact_id = ?",
    )
    .bind(&user_id)
    .bind(&session_id)
    .bind(&artifact_id)
    .execute(&pool)
    .await
    .unwrap();
    let terminal_retain = store
        .retain_json_artifact_reference(&user_id, &session_id, &artifact_id, &replayable_reference)
        .await;
    assert!(matches!(
        terminal_retain,
        Err(SessionArtifactStoreError::ArtifactNotRetainable { .. })
    ));

    let rejected_id = Uuid::new_v4().to_string();
    let rejected = store
        .persist_json_artifact(astra_services::SessionArtifactJsonRecord {
            artifact_id: rejected_id.clone(),
            session_id: session_id.clone(),
            user_id: user_id.clone(),
            artifact_kind: "tool_result_evidence_v1".to_string(),
            source: None,
            turn: None,
            round: None,
            content: serde_json::json!({"must": "not persist"}),
            metadata: None,
            references: vec![SessionArtifactReference {
                kind: SessionArtifactReferenceKind::InvocationLedger,
                reference_id: "x".repeat(129),
            }],
        })
        .await;
    assert!(matches!(
        rejected,
        Err(SessionArtifactStoreError::InvalidReferenceId(_))
    ));
    let rejected_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM session_artifacts
         WHERE user_id = ? AND session_id = ? AND artifact_id = ?",
    )
    .bind(&user_id)
    .bind(&session_id)
    .bind(&rejected_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(rejected_count, 0);

    cleanup_restore_fixture_for_owner(&pool, &user_id, &[session_id]).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn session_artifact_store_is_owner_bound_on_reads_and_writes() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let owner_user_id = Uuid::new_v4().to_string();
    let other_user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();
    let other_owner_session_id = Uuid::new_v4().to_string();
    let foreign_session_id = Uuid::new_v4().to_string();
    cleanup_restore_fixture_for_owner(
        &pool,
        &owner_user_id,
        &[session_id.clone(), other_owner_session_id.clone()],
    )
    .await;
    cleanup_restore_fixture_for_owner(
        &pool,
        &other_user_id,
        std::slice::from_ref(&foreign_session_id),
    )
    .await;

    for (sid, uid, title) in [
        (&session_id, &owner_user_id, "artifact-owner-session"),
        (
            &other_owner_session_id,
            &owner_user_id,
            "artifact-other-session",
        ),
        (
            &foreign_session_id,
            &other_user_id,
            "artifact-foreign-session",
        ),
    ] {
        sqlx::query(
            "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
             VALUES (?, ?, ?, 'active', 0)",
        )
        .bind(sid)
        .bind(uid)
        .bind(title)
        .execute(&pool)
        .await
        .expect("insert owner session");
    }

    let store = DatabaseSessionArtifactStore::new(settings).with_pool(shared);
    let shared_artifact_id = Uuid::now_v7().to_string();
    let foreign_artifact = store
        .persist_json_artifact(astra_services::SessionArtifactJsonRecord {
            artifact_id: shared_artifact_id.clone(),
            session_id: foreign_session_id.clone(),
            user_id: other_user_id.clone(),
            artifact_kind: "llm_capture".into(),
            source: Some("owner-bound-test".into()),
            turn: Some(0),
            round: Some(0),
            content: serde_json::json!({"owner": false, "payload": "foreign row"}),
            metadata: Some(serde_json::json!({"scope": "foreign"})),
            references: Vec::new(),
        })
        .await
        .expect("foreign owner can persist same artifact id in its own session");
    let artifact = store
        .persist_json_artifact(astra_services::SessionArtifactJsonRecord {
            artifact_id: shared_artifact_id.clone(),
            session_id: session_id.clone(),
            user_id: owner_user_id.clone(),
            artifact_kind: "llm_capture".into(),
            source: Some("owner-bound-test".into()),
            turn: Some(1),
            round: Some(0),
            content: serde_json::json!({"owner": true, "payload": "visible only to owner"}),
            metadata: Some(serde_json::json!({"scope": "owner"})),
            references: Vec::new(),
        })
        .await
        .expect("owner can persist same artifact id without foreign collision");

    assert_eq!(artifact.artifact_id, foreign_artifact.artifact_id);
    let same_id_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM session_artifacts WHERE artifact_id = ?")
            .bind(&shared_artifact_id)
            .fetch_one(&pool)
            .await
            .expect("count same artifact id rows");
    assert_eq!(
        same_id_count, 2,
        "artifact identity must include owner/session, not artifact_id alone"
    );

    assert!(
        store
            .load_json_artifact(&owner_user_id, &session_id, &artifact.artifact_id)
            .await
            .expect("owner load by id")
            .is_some(),
        "owner can load artifact by id in the owning session"
    );
    assert_eq!(
        store
            .load_latest_json_artifact(&owner_user_id, &session_id, "llm_capture")
            .await
            .expect("owner latest")
            .map(|artifact| artifact.artifact_id),
        Some(artifact.artifact_id.clone()),
        "owner latest query returns the artifact"
    );
    assert_eq!(
        store
            .list_json_artifacts(&owner_user_id, &session_id, Some("llm_capture"), 10, None)
            .await
            .expect("owner list")
            .artifacts
            .len(),
        1,
        "owner list sees exactly the session artifact"
    );

    assert!(
        store
            .load_json_artifact(&other_user_id, &session_id, &artifact.artifact_id)
            .await
            .expect("non-owner load by id")
            .is_none(),
        "non-owner cannot infer artifact existence by id"
    );
    assert!(
        store
            .load_json_artifact(&other_user_id, &foreign_session_id, &artifact.artifact_id)
            .await
            .expect("foreign owner load by id")
            .is_some(),
        "same artifact_id remains visible to the foreign owner in the foreign session"
    );
    assert!(
        store
            .load_latest_json_artifact(&other_user_id, &session_id, "llm_capture")
            .await
            .expect("non-owner latest")
            .is_none(),
        "non-owner latest query is indistinguishable from not found"
    );
    assert!(
        store
            .list_json_artifacts(&other_user_id, &session_id, None, 10, None)
            .await
            .expect("non-owner list")
            .artifacts
            .is_empty(),
        "non-owner list does not leak another user's artifacts"
    );
    assert!(
        store
            .load_json_artifact(
                &owner_user_id,
                &other_owner_session_id,
                &artifact.artifact_id
            )
            .await
            .expect("owner wrong-session load")
            .is_none(),
        "artifact_id alone cannot cross the session boundary"
    );

    let non_owner_write = store
        .persist_json_artifact(astra_services::SessionArtifactJsonRecord {
            artifact_id: Uuid::now_v7().to_string(),
            session_id: session_id.clone(),
            user_id: other_user_id.clone(),
            artifact_kind: "llm_capture".into(),
            source: Some("owner-bound-test".into()),
            turn: Some(2),
            round: Some(0),
            content: serde_json::json!({"owner": false, "payload": "must not persist"}),
            metadata: None,
            references: Vec::new(),
        })
        .await;
    assert!(
        matches!(
            non_owner_write,
            Err(SessionArtifactStoreError::SessionNotOwned { .. })
        ),
        "store write path must reject artifacts for sessions not owned by record.user_id"
    );

    let artifact_count: i64 = sqlx::query(
        "SELECT COUNT(*) AS c FROM session_artifacts
         WHERE user_id = ? AND session_id = ? AND artifact_kind = 'llm_capture'",
    )
    .bind(&owner_user_id)
    .bind(&session_id)
    .fetch_one(&pool)
    .await
    .expect("count artifacts after rejected non-owner write")
    .try_get("c")
    .expect("artifact count");
    assert_eq!(
        artifact_count, 1,
        "rejected non-owner write must not mutate session_artifacts"
    );

    cleanup_restore_fixture_for_owner(&pool, &owner_user_id, &[session_id, other_owner_session_id])
        .await;
    cleanup_restore_fixture_for_owner(&pool, &other_user_id, &[foreign_session_id]).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn cross_session_stats_and_audit_list_sessions_match_seeded_events() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let user_id = Uuid::new_v4().to_string();
    let s1 = Uuid::new_v4().to_string();
    let s2 = Uuid::new_v4().to_string();
    let since = "2026-06-15 09:00:00.000000".to_string();
    let until = "2026-06-15 14:00:00.000000".to_string();

    let e_turn_a1 = Uuid::new_v4().to_string();
    let e_turn_a2 = Uuid::new_v4().to_string();
    let e_turn_b1 = Uuid::new_v4().to_string();
    let e_tool_ok = Uuid::new_v4().to_string();
    let e_tool_err = Uuid::new_v4().to_string();
    let e_tool_call2 = Uuid::new_v4().to_string();
    let e_turn_err = Uuid::new_v4().to_string();
    let e_stall = Uuid::new_v4().to_string();
    let mut event_ids = vec![
        e_turn_a1.clone(),
        e_turn_a2.clone(),
        e_turn_b1.clone(),
        e_tool_ok.clone(),
        e_tool_err.clone(),
        e_tool_call2.clone(),
        e_turn_err.clone(),
        e_stall.clone(),
    ];

    cleanup_agent_sessions_and_events_for_owner(
        &pool,
        &user_id,
        &[s1.clone(), s2.clone()],
        &event_ids,
    )
    .await;

    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count, created_at, updated_at, last_active_at) \
         VALUES (?, ?, 's1', 'active', 0, ?, ?, ?)",
    )
    .bind(&s1)
    .bind(&user_id)
    .bind("2026-06-15 09:30:00.000000")
    .bind("2026-06-15 09:30:00.000000")
    .bind("2026-06-15 09:30:00.000000")
    .execute(&pool)
    .await
    .expect("insert session s1");
    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count, created_at, updated_at, last_active_at) \
         VALUES (?, ?, 's2', 'active', 0, ?, ?, ?)",
    )
    .bind(&s2)
    .bind(&user_id)
    .bind("2026-06-15 09:31:00.000000")
    .bind("2026-06-15 09:31:00.000000")
    .bind("2026-06-15 09:31:00.000000")
    .execute(&pool)
    .await
    .expect("insert session s2");

    // Session s1: two turns (model m1), two successful and one failed terminal
    // tool lifecycle events on "bash", plus one stall.
    for (eid, typ, tin, tout, ttot, model, tool, ts) in [
        (
            &e_turn_a1,
            "user_query",
            10_i64,
            5_i64,
            15_i64,
            Some("m1"),
            None::<&str>,
            "2026-06-15 10:00:00.000000",
        ),
        (
            &e_turn_a2,
            "user_query",
            20_i64,
            10_i64,
            30_i64,
            Some("m1"),
            None::<&str>,
            "2026-06-15 10:01:00.000000",
        ),
        (
            &e_tool_ok,
            "tool_call_completed",
            0_i64,
            0_i64,
            0_i64,
            Some("m1"),
            Some("bash"),
            "2026-06-15 10:02:00.000000",
        ),
        (
            &e_tool_err,
            "tool_call_failed",
            0_i64,
            0_i64,
            0_i64,
            Some("m1"),
            Some("bash"),
            "2026-06-15 10:03:00.000000",
        ),
        (
            &e_tool_call2,
            "tool_call_completed",
            0_i64,
            0_i64,
            0_i64,
            Some("m1"),
            Some("bash"),
            "2026-06-15 10:04:00.000000",
        ),
        (
            &e_stall,
            "stall_detected",
            0_i64,
            0_i64,
            0_i64,
            None::<&str>,
            None::<&str>,
            "2026-06-15 10:05:00.000000",
        ),
    ] {
        let token_usage = serde_json::json!({
            "input_tokens": tin,
            "cached_input_tokens": 0,
            "cache_creation_tokens": 0,
            "output_tokens": tout,
            "total_tokens": ttot,
        });
        let insert_sql = astra_core::matrixone_statement_with_null_shape(
            "INSERT INTO agent_events (event_id, session_id, user_id, event_type, content, \
             causal_chain_id, token_usage, token_input, token_output, token_total, meta_tool_name, \
             llm_model_used, payload_hash, ingestion_write_id, created_at) \
             VALUES (?, ?, ?, ?, '{}', '', CAST(? AS JSON), ?, ?, ?, ?, ?, ?, ?, ?)",
            [tool.is_some(), model.is_some()],
        );
        sqlx::query(insert_sql)
            .bind(eid)
            .bind(&s1)
            .bind(&user_id)
            .bind(typ)
            .bind(token_usage.to_string())
            .bind(tin)
            .bind(tout)
            .bind(ttot)
            .bind(tool)
            .bind(model)
            .bind(agent_event_fixture_payload_hash(serde_json::json!({
                "event_id": eid,
                "session_id": &s1,
                "user_id": &user_id,
                "event_type": typ,
                "content": "{}",
                "causal_chain_id": "",
                "token_usage": &token_usage,
                "token_input": tin,
                "token_output": tout,
                "token_total": ttot,
                "meta_tool_name": tool,
                "llm_model_used": model,
                "created_at": ts,
            })))
            .bind(Uuid::new_v4().to_string())
            .bind(ts)
            .execute(&pool)
            .await
            .expect("insert event s1");
    }

    let turn_b1_usage = serde_json::json!({
        "input_tokens": 5,
        "cached_input_tokens": 0,
        "cache_creation_tokens": 0,
        "output_tokens": 5,
        "total_tokens": 10,
    });
    sqlx::query(
        "INSERT INTO agent_events (event_id, session_id, user_id, event_type, content, \
         causal_chain_id, token_usage, token_input, token_output, token_total, meta_tool_name, \
         llm_model_used, payload_hash, ingestion_write_id, created_at) \
         VALUES (?, ?, ?, 'user_query', '{}', '', CAST(? AS JSON), 5, 5, 10, NULL, 'm1', ?, ?, ?)",
    )
    .bind(&e_turn_b1)
    .bind(&s2)
    .bind(&user_id)
    .bind(turn_b1_usage.to_string())
    .bind(agent_event_fixture_payload_hash(serde_json::json!({
        "event_id": &e_turn_b1,
        "session_id": &s2,
        "user_id": &user_id,
        "event_type": "user_query",
        "content": "{}",
        "causal_chain_id": "",
        "token_usage": &turn_b1_usage,
        "token_input": 5,
        "token_output": 5,
        "token_total": 10,
        "meta_tool_name": null,
        "llm_model_used": "m1",
        "created_at": "2026-06-15 10:10:00.000000",
    })))
    .bind(Uuid::new_v4().to_string())
    .bind("2026-06-15 10:10:00.000000")
    .execute(&pool)
    .await
    .expect("insert turn s2");

    sqlx::query(
        "INSERT INTO agent_events (event_id, session_id, user_id, event_type, content, \
         causal_chain_id, token_input, token_output, token_total, meta_tool_name, llm_model_used, \
         payload_hash, ingestion_write_id, created_at) \
         VALUES (?, ?, ?, 'turn_error', '{}', '', 0, 0, 0, NULL, NULL, ?, ?, ?)",
    )
    .bind(&e_turn_err)
    .bind(&s2)
    .bind(&user_id)
    .bind(agent_event_fixture_payload_hash(serde_json::json!({
        "event_id": &e_turn_err,
        "session_id": &s2,
        "user_id": &user_id,
        "event_type": "turn_error",
        "content": "{}",
        "causal_chain_id": "",
        "token_input": 0,
        "token_output": 0,
        "token_total": 0,
        "meta_tool_name": null,
        "llm_model_used": null,
        "created_at": "2026-06-15 10:11:00.000000",
    })))
    .bind(Uuid::new_v4().to_string())
    .bind("2026-06-15 10:11:00.000000")
    .execute(&pool)
    .await
    .expect("insert turn_error s2");

    let audit = DatabaseSessionAuditService::new(settings.clone()).with_pool(shared.clone());
    let queries = common::SqlQueries::default();
    let stats = audit
        .get_cross_session_stats(
            &user_id,
            &CrossSessionStatsParams {
                since: Some(since.clone()),
                until: Some(until.clone()),
            },
        )
        .with_subscriber(tracing_subscriber::registry().with(queries.clone()))
        .await
        .expect("get_cross_session_stats");

    assert_eq!(stats.session_count, 2);
    assert_eq!(stats.total_turns, 3);
    assert_eq!(stats.total_tokens_in, 35);
    assert_eq!(stats.total_tokens_out, 20);
    assert_eq!(stats.total_tool_calls, 3);
    assert_eq!(stats.total_tool_failures, 1);
    assert_eq!(
        stats.total_errors, 2,
        "the canonical error set contains one failed tool lifecycle and one turn error"
    );
    assert_eq!(stats.total_stalls, 1);
    assert!((stats.avg_turns_per_session - 1.5).abs() < 1e-9);
    assert!((stats.avg_tokens_per_session - 27.5).abs() < 1e-9);
    assert!((stats.tool_error_rate - (1.0_f64 / 3.0)).abs() < 1e-9);

    assert_eq!(stats.top_tools.len(), 1);
    assert_eq!(stats.top_tools[0].name, "bash");
    assert_eq!(stats.top_tools[0].call_count, 3);
    assert!((stats.top_tools[0].success_rate - (2.0_f64 / 3.0)).abs() < 1e-9);

    assert_eq!(stats.top_models.len(), 1);
    assert_eq!(stats.top_models[0].model, "m1");
    assert_eq!(stats.top_models[0].session_count, 2);
    assert_eq!(stats.top_models[0].total_tokens, 55);

    let statements = queries.0.lock().unwrap().join(" ");
    assert!(
        !statements.contains("ROW_NUMBER()"),
        "count-only statistics must not rank discarded error previews"
    );
    assert!(
        !statements.contains("SELECT event_id, SUBSTRING(content"),
        "count-only statistics must not fetch discarded error text"
    );
    assert!(statements.contains("GROUP BY e.session_id, e.meta_tool_name"));
    assert!(statements.contains("llm_model_used AS model"));

    let list = audit
        .list_sessions(
            &user_id,
            &AuditSessionListParams {
                page: 1,
                per_page: 9999,
                status: None,
                model: None,
                since: Some(since.clone()),
                until: Some(until.clone()),
                min_turns: None,
                sort: "created".into(),
                order: "desc".into(),
                after_sort_value: None,
                after_session_id: None,
            },
        )
        .await
        .expect("list_sessions");
    assert_eq!(list.per_page, MAX_AUDIT_SESSIONS_PER_PAGE);
    assert_eq!(list.total, 2);
    assert_eq!(list.sessions.len(), 2);

    // A real immutable opening, with a bound root agent, must be counted
    // without a physical run_started row or hiding unrelated Journal facts.
    let mut run = common::collaborator_fixture::record(&user_id, &s1, None);
    let durable_run = run.run_id.clone();
    run.agent_id = Some("bound-root".into());
    run.events = vec![
        serde_json::json!({"event_type":"run_started", "data":{
            "resolved_model_selection":{"model_name":"durable-model"}
        }}),
        serde_json::json!({"event_type":"tool_call", "data":{"name":"read_file", "id":"durable-call"}}),
        serde_json::json!({"event_type":"tool_call_end", "data":{"name":"read_file", "id":"durable-call", "success":true}}),
    ];
    astra_services::RunStateStore::insert_run(
        &astra_services::DatabaseRunStateStore::new(shared.clone()),
        run,
    )
    .await
    .unwrap();
    for table in ["agent_runs", "agent_run_events"] {
        sqlx::query(sqlx::AssertSqlSafe(format!("UPDATE {table} SET created_at = '2026-06-15 10:12:00.000000' WHERE user_id = ? AND run_id = ?")))
            .bind(&user_id).bind(&durable_run).execute(&pool).await.unwrap();
    }
    let queries = common::SqlQueries::default();
    let durable = audit
        .get_cross_session_stats(
            &user_id,
            &CrossSessionStatsParams {
                since: Some(since),
                until: Some(until),
            },
        )
        .with_subscriber(tracing_subscriber::registry().with(queries.clone()))
        .await
        .unwrap();
    assert_eq!(durable.session_count, 2);
    assert_eq!(durable.total_turns, 3);
    assert_eq!(durable.total_tool_calls, 4);
    assert_eq!(durable.total_tool_failures, 1);
    assert!(
        durable
            .top_models
            .iter()
            .any(|model| model.model == "durable-model" && model.session_count == 1)
    );
    assert!(
        durable.top_models.iter().any(|model| model.model == "m1"
            && model.session_count == 2
            && model.total_tokens == 55)
    );
    assert_eq!(durable.top_tools.len(), 2);
    assert_eq!(durable.top_tools[0].name, "bash");
    assert_eq!(durable.top_tools[0].call_count, 3);
    assert_eq!(durable.top_tools[1].name, "read_file");
    assert_eq!(durable.top_tools[1].call_count, 1);
    let statements = queries.0.lock().unwrap().join(" ");
    assert!(statements.contains("agent_run_openings"));
    assert!(!statements.contains("opening_context_json"));
    let store = astra_services::DatabaseRunStateStore::new(shared.clone());
    astra_services::RunStateStore::append_events_batch(
        &store,
        &user_id,
        &s1,
        &durable_run,
        &[serde_json::json!({"type":"tool_call", "name":"bash", "id":"pending-call"})],
    )
    .await
    .unwrap();
    // A known durable success wins a conflicting Journal terminal. A pending
    // durable call, however, must retain a known Journal failure of that call.
    for (call, name) in [("durable-call", "read_file"), ("pending-call", "bash")] {
        let id = Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO agent_events (event_id, session_id, user_id, event_type, content, run_id, tool_call_id, meta_tool_name, meta_duration_ms, metadata, payload_hash, ingestion_write_id) VALUES (?, ?, ?, 'tool_call_failed', 'journal failure preview', ?, ?, ?, 17, CAST('{}' AS JSON), ?, ?)")
            .bind(&id).bind(&s1).bind(&user_id).bind(&durable_run).bind(call).bind(name)
            .bind(agent_event_fixture_payload_hash(serde_json::json!({"event_id":id})))
            .bind(Uuid::new_v4().to_string()).execute(&pool).await.unwrap();
        event_ids.push(id);
    }
    let all = CrossSessionStatsParams {
        since: None,
        until: None,
    };
    let mixed = audit.get_cross_session_stats(&user_id, &all).await.unwrap();
    assert_eq!((mixed.total_tool_calls, mixed.total_tool_failures), (5, 2));
    let tools = audit.get_cross_session_tools(&user_id, &all).await.unwrap();
    let bash = tools.iter().find(|tool| tool.name == "bash").unwrap();
    assert_eq!(
        (bash.total_calls, bash.total_success, bash.total_failures),
        (4, 2, 2)
    );
    assert_eq!(
        bash.sessions_used_in, 1,
        "a Session present in both sources is counted once"
    );
    assert_eq!(bash.last_error.as_deref(), Some("journal failure preview"));
    let read = tools.iter().find(|tool| tool.name == "read_file").unwrap();
    assert_eq!(
        (read.total_calls, read.total_success, read.total_failures),
        (1, 1, 0)
    );
    astra_services::RunStateStore::append_events_batch(
        &store,
        &user_id,
        &s1,
        &durable_run,
        &[serde_json::json!({"type":"tool_call_end", "call_id":"pending-call", "success":true})],
    )
    .await
    .unwrap();
    let settled = audit.get_cross_session_stats(&user_id, &all).await.unwrap();
    assert_eq!(
        (settled.total_tool_calls, settled.total_tool_failures),
        (5, 1)
    );
    let summary = audit.get_summary(&user_id, &s1).await.unwrap();
    assert_eq!(
        (summary.tool_calls_total, summary.tool_calls_failed),
        (5, 1)
    );
    let listed = audit
        .list_sessions(
            &user_id,
            &AuditSessionListParams {
                page: 1,
                per_page: 1,
                status: None,
                model: None,
                since: None,
                until: None,
                min_turns: None,
                sort: "created".into(),
                order: "asc".into(),
                after_sort_value: None,
                after_session_id: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(listed.sessions.len(), 1);
    assert_eq!(listed.sessions[0].session_id, s1);
    assert_eq!(
        listed.sessions[0].tool_calls_total,
        summary.tool_calls_total
    );
    for (call, name, synthetic) in [
        ("durable-call", "read_file", true),
        ("DURABLE-CALL", "read_file", false),
        ("unnamed", "", false),
    ] {
        let id = Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO agent_events (event_id, session_id, user_id, event_type, content, run_id, tool_call_id, meta_tool_name, metadata, payload_hash, ingestion_write_id) VALUES (?, ?, ?, 'tool_call_failed', 'independent fact', ?, ?, ?, CAST(? AS JSON), ?, ?)")
            .bind(&id).bind(&s1).bind(&user_id).bind(&durable_run).bind(call).bind(name)
            .bind(serde_json::json!({"tool_call_id_is_synthetic":synthetic}).to_string())
            .bind(agent_event_fixture_payload_hash(serde_json::json!({"event_id":id})))
            .bind(Uuid::new_v4().to_string()).execute(&pool).await.unwrap();
        event_ids.push(id);
    }
    let independent = audit.get_cross_session_stats(&user_id, &all).await.unwrap();
    assert_eq!(
        (
            independent.total_tool_calls,
            independent.total_tool_failures
        ),
        (8, 4),
        "synthetic, differently cased and unnamed facts cannot be erased by a guessed match"
    );
    let tools = audit.get_cross_session_tools(&user_id, &all).await.unwrap();
    assert_eq!(
        tools.iter().map(|tool| tool.total_calls).sum::<u32>(),
        7,
        "unnamed terminals contribute to totals but not named-tool analytics"
    );
    for index in 0..101 {
        let id = Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO agent_events (event_id, session_id, user_id, event_type, content, meta_tool_name, metadata, payload_hash, ingestion_write_id) VALUES (?, ?, ?, 'tool_call_failed', 'bounded winning error preview', ?, CAST('{}' AS JSON), ?, ?)")
            .bind(&id).bind(&s2).bind(&user_id).bind(format!("z-tool-{index:03}"))
            .bind(agent_event_fixture_payload_hash(serde_json::json!({"event_id":id})))
            .bind(Uuid::new_v4().to_string()).execute(&pool).await.unwrap();
        event_ids.push(id);
    }
    let queries = common::SqlQueries::default();
    let tools = audit
        .get_cross_session_tools(&user_id, &all)
        .with_subscriber(tracing_subscriber::registry().with(queries.clone()))
        .await
        .unwrap();
    assert_eq!(tools.len(), 100);
    assert!(!tools.iter().any(|tool| tool.name == "z-tool-100"));
    assert!(tools.iter().all(|tool| tool.last_error.is_some()));
    {
        let statements = queries.0.lock().unwrap();
        let previews = statements
            .iter()
            .filter(|sql| sql.contains("SELECT event_id, SUBSTRING(content"))
            .collect::<Vec<_>>();
        assert_eq!(previews.len(), 1);
        assert_eq!(
            previews[0].matches('?').count(),
            101,
            "one owner bind and only the 100 returned winning errors"
        );
    }
    let queries = common::SqlQueries::default();
    let counts = audit
        .get_cross_session_stats(&user_id, &all)
        .with_subscriber(tracing_subscriber::registry().with(queries.clone()))
        .await
        .unwrap();
    assert_eq!(
        (counts.total_tool_calls, counts.total_tool_failures),
        (109, 105)
    );
    assert_eq!(counts.top_tools.len(), 10);
    assert!(
        !queries
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|sql| sql.contains("ROW_NUMBER()")
                || sql.contains("SELECT event_id, SUBSTRING(content"))
    );
    // A later empty failure or successful output is not error evidence. Within
    // one second, preserve microseconds first and use binary IDs only for ties.
    let error_prefix = Uuid::new_v4().to_string();
    for (suffix, at, kind, content) in [
        (
            "z",
            "2026-06-15 12:00:00.000001",
            "tool_call_failed",
            Some("older error"),
        ),
        (
            "Z",
            "2026-06-15 12:00:00.100001",
            "tool_call_failed",
            Some("same-time upper ID"),
        ),
        (
            "a",
            "2026-06-15 12:00:00.100001",
            "tool_call_failed",
            Some("microsecond winner"),
        ),
        (
            "empty",
            "2026-06-15 12:00:01.000000",
            "tool_call_failed",
            Some(""),
        ),
        (
            "null",
            "2026-06-15 12:00:02.000000",
            "tool_call_failed",
            None,
        ),
        (
            "ok",
            "2026-06-15 12:00:03.000000",
            "tool_call_completed",
            Some("successful output"),
        ),
    ] {
        let id = format!("{error_prefix}-{suffix}");
        sqlx::query("INSERT INTO agent_events (event_id, session_id, user_id, event_type, content, meta_tool_name, metadata, payload_hash, ingestion_write_id, created_at) VALUES (?, ?, ?, ?, ?, 'error-order', CAST('{}' AS JSON), ?, ?, ?)")
            .bind(&id).bind(&s2).bind(&user_id).bind(kind).bind(content)
            .bind(agent_event_fixture_payload_hash(serde_json::json!({"event_id":id})))
            .bind(Uuid::new_v4().to_string()).bind(at).execute(&pool).await.unwrap();
        event_ids.push(id);
    }
    let queries = common::SqlQueries::default();
    let named = audit
        .get_tool_analytics(&user_id, &s2)
        .with_subscriber(tracing_subscriber::registry().with(queries.clone()))
        .await
        .unwrap();
    let error_order = named
        .iter()
        .find(|tool| tool.name == "error-order")
        .unwrap();
    assert_eq!(
        (
            error_order.call_count,
            error_order.success_count,
            error_order.fail_count
        ),
        (6, 1, 5)
    );
    assert_eq!(
        error_order.last_error.as_deref(),
        Some("microsecond winner")
    );
    assert_eq!(
        error_order.total_duration_ms, 0,
        "NULL durations remain unavailable samples"
    );
    {
        let statements = queries.0.lock().unwrap();
        assert!(!statements.iter().any(|sql| sql.contains("ROW_NUMBER()")));
        assert!(
            statements
                .iter()
                .any(|sql| sql.contains("MAX_BY_NON_NULL("))
        );
    }
    let global = audit.get_cross_session_tools(&user_id, &all).await.unwrap();
    assert_eq!(
        global
            .iter()
            .find(|tool| tool.name == "error-order")
            .unwrap()
            .last_error
            .as_deref(),
        Some("microsecond winner")
    );
    let before_pending = audit.get_cross_session_stats(&user_id, &all).await.unwrap();
    astra_services::RunStateStore::append_events_batch(&store,&user_id,&s1,&durable_run,&[
        serde_json::json!({"type":"tool_call","name":"pending-rate","id":"rate-pending"}),
        serde_json::json!({"type":"tool_call","name":"rejected-rate","id":"rate-rejected"}),
        serde_json::json!({"type":"tool_call_end","call_id":"rate-rejected","success":false,"executed":false}),
    ]).await.unwrap();
    let after_pending = audit.get_cross_session_stats(&user_id, &all).await.unwrap();
    assert_eq!(
        after_pending.total_tool_calls,
        before_pending.total_tool_calls + 2
    );
    assert_eq!(
        after_pending.total_tool_failures,
        before_pending.total_tool_failures
    );
    assert_eq!(
        after_pending.tool_error_rate, before_pending.tool_error_rate,
        "pending and rejected calls cannot dilute the known-outcome rate"
    );
    sqlx::query("DELETE FROM agent_run_events WHERE user_id = ? AND run_id = ?")
        .bind(&user_id)
        .bind(&durable_run)
        .execute(&pool)
        .await
        .unwrap();

    for table in ["agent_run_openings", "agent_runs"] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM {table} WHERE user_id = ? AND run_id = ?"
        )))
        .bind(&user_id)
        .bind(&durable_run)
        .execute(&pool)
        .await
        .unwrap();
    }

    // Former promotion labels remain generic facts, including metadata that
    // never satisfied the retired controller-specific reporting schema.
    let events = DatabaseEventService::new(settings).with_pool(shared);
    let metadata = serde_json::json!({"client_note":"ordinary owner fact"});
    let created = events
        .create_event(
            user_id.clone(),
            EventCreateRequestData {
                ingestion_source: astra_services::events::EventIngestionSource::Client,
                event_id: None,
                session_id: s1.clone(),
                event_type: "runtime_promotion_verdict".into(),
                content: "historical client fact".into(),
                agent_id: None,
                agent_version: None,
                parent_event_id: None,
                parent_event_ids: None,
                causal_chain_id: None,
                metadata: Some(metadata.clone()),
            },
        )
        .await
        .unwrap();
    let id = created.record.event_id;
    assert_eq!(
        events
            .get_event(id.clone(), user_id.clone())
            .await
            .unwrap()
            .metadata,
        metadata
    );
    assert_eq!(
        events
            .get_event(id.clone(), "other-owner".into())
            .await
            .unwrap_err()
            .0,
        axum::http::StatusCode::NOT_FOUND
    );
    event_ids.push(id);
    let queries = common::SqlQueries::default();
    let stats = audit
        .get_cross_session_stats(
            &user_id,
            &CrossSessionStatsParams {
                since: None,
                until: None,
            },
        )
        .with_subscriber(tracing_subscriber::registry().with(queries.clone()))
        .await
        .unwrap();
    let json = serde_json::to_value(stats).unwrap();
    assert_eq!(json["session_count"], 2);
    assert!(json.get("total_runtime_promotions").is_none());
    assert!(!queries.0.lock().unwrap().iter().any(|sql| {
        sql.contains("SELECT event_id, session_id, CAST(metadata AS CHAR) AS metadata")
    }));

    cleanup_agent_sessions_and_events_for_owner(&pool, &user_id, &[s1, s2], &event_ids).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn session_audit_session_turn_count_uses_turn_seq_high_watermark() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();
    let event_one = Uuid::new_v4().to_string();
    let event_four = Uuid::new_v4().to_string();
    let event_ids = vec![event_one.clone(), event_four.clone()];

    cleanup_agent_sessions_and_events_for_owner(
        &pool,
        &user_id,
        std::slice::from_ref(&session_id),
        &event_ids,
    )
    .await;

    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count, created_at, updated_at, last_active_at) \
         VALUES (?, ?, 'turn-high-watermark', 'active', 0, ?, ?, ?)",
    )
    .bind(&session_id)
    .bind(&user_id)
    .bind("2026-06-16 09:00:00.000000")
    .bind("2026-06-16 09:00:00.000000")
    .bind("2026-06-16 09:00:00.000000")
    .execute(&pool)
    .await
    .expect("insert audit session");

    for (event_id, turn_seq, ts) in [
        (&event_one, 1_i64, "2026-06-16 09:01:00.000000"),
        (&event_four, 4_i64, "2026-06-16 09:04:00.000000"),
    ] {
        sqlx::query(
            "INSERT INTO agent_events \
             (event_id, session_id, user_id, event_type, content, causal_chain_id, \
              token_input, token_output, token_total, turn_seq, payload_hash, \
              ingestion_write_id, created_at) \
             VALUES (?, ?, ?, 'user_query', '{}', '', 1, 1, 2, ?, ?, ?, ?)",
        )
        .bind(event_id)
        .bind(&session_id)
        .bind(&user_id)
        .bind(turn_seq)
        .bind(agent_event_fixture_payload_hash(serde_json::json!({
            "event_id": event_id,
            "session_id": &session_id,
            "user_id": &user_id,
            "event_type": "user_query",
            "content": "{}",
            "causal_chain_id": "",
            "token_input": 1,
            "token_output": 1,
            "token_total": 2,
            "turn_seq": turn_seq,
            "created_at": ts,
        })))
        .bind(Uuid::new_v4().to_string())
        .bind(ts)
        .execute(&pool)
        .await
        .expect("insert sparse turn event");
    }

    let audit = DatabaseSessionAuditService::new(settings).with_pool(shared);
    let summary = audit
        .get_summary(&user_id, &session_id)
        .await
        .expect("get sparse-turn summary");
    assert_eq!(
        summary.turn_count, 4,
        "session audit summary must report turn_seq high watermark, not user_query row count"
    );

    let turns = audit
        .list_turns(
            &user_id,
            &session_id,
            &TurnListParams {
                page: 1,
                per_page: 10,
                after_created_at: None,
                after_event_id: None,
            },
        )
        .await
        .expect("list sparse-turn turns");
    assert_eq!(
        turns.total, 4,
        "turn list total must use turn_seq high watermark, not user_query row count"
    );
    assert_eq!(turns.turns.len(), 2);

    let matching = audit
        .list_sessions(
            &user_id,
            &AuditSessionListParams {
                page: 1,
                per_page: 10,
                status: None,
                model: None,
                since: None,
                until: None,
                min_turns: Some(3),
                sort: "turns".into(),
                order: "desc".into(),
                after_sort_value: None,
                after_session_id: None,
            },
        )
        .await
        .expect("list sparse-turn sessions");
    assert_eq!(matching.total, 1);
    assert_eq!(matching.sessions[0].session_id, session_id);
    assert_eq!(matching.sessions[0].turn_count, 4);

    let filtered = audit
        .list_sessions(
            &user_id,
            &AuditSessionListParams {
                page: 1,
                per_page: 10,
                status: None,
                model: None,
                since: None,
                until: None,
                min_turns: Some(5),
                sort: "turns".into(),
                order: "desc".into(),
                after_sort_value: None,
                after_session_id: None,
            },
        )
        .await
        .expect("filter sparse-turn sessions");
    assert_eq!(filtered.total, 0);
    assert!(filtered.sessions.is_empty());

    cleanup_agent_sessions_and_events_for_owner(&pool, &user_id, &[session_id], &event_ids).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn audit_session_pages_preserve_filters_order_and_owner_scope() {
    let (_bootstrap, mut settings) = setup_pool_and_settings().await;
    settings.db_pool_max_connections = 1;
    settings.db_pool_min_connections = 1;
    let shared = SharedPool::new(&settings).await.unwrap();
    let pool = shared.get().clone();
    let owner = Uuid::new_v4().to_string();
    let foreign = Uuid::new_v4().to_string();
    let prefix = Uuid::new_v4().to_string();
    let ids = [
        format!("{prefix}-a"),
        format!("{prefix}-b"),
        format!("{prefix}-c"),
    ];
    for (index, session) in ids.iter().enumerate() {
        seed_restore_session_fixture(&pool, &owner, session, serde_json::json!({})).await;
        sqlx::query("UPDATE agent_sessions SET created_at = '2026-06-15 09:30:00.000000', ended_at = DATE_ADD('2026-06-15 09:30:00.000000', INTERVAL ? SECOND) WHERE user_id = ? AND session_id = ?")
            .bind([10_i64, 30, 20][index]).bind(&owner).bind(session).execute(&pool).await.unwrap();
    }
    seed_restore_session_fixture(&pool, &foreign, &ids[0], serde_json::json!({})).await;
    let mut events = Vec::new();
    for (user, session, turn, tokens, at, model) in [
        (
            &owner,
            &ids[0],
            1_i64,
            5_i64,
            "2026-06-15 10:00:00.100000",
            None,
        ),
        (&owner, &ids[0], 2, 5, "2026-06-15 10:05:00.250000", None),
        (
            &owner,
            &ids[1],
            1,
            30,
            "2026-06-15 10:01:00",
            Some("journal-model"),
        ),
        (
            &foreign,
            &ids[0],
            99,
            900,
            "2026-06-15 11:00:00",
            Some("foreign-model"),
        ),
    ] {
        let id = Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO agent_events (event_id, session_id, user_id, event_type, content, turn_seq, token_usage, token_input, token_output, token_total, llm_model_used, metadata, payload_hash, ingestion_write_id, created_at) VALUES (?, ?, ?, 'user_query', 'input', ?, CAST('{}' AS JSON), ?, 0, ?, ?, CAST('{}' AS JSON), ?, ?, ?)")
            .bind(&id).bind(session).bind(user).bind(turn).bind(tokens).bind(tokens).bind(model)
            .bind(agent_event_fixture_payload_hash(serde_json::json!({"event_id":id})))
            .bind(Uuid::new_v4().to_string()).bind(at).execute(&pool).await.unwrap();
        events.push(id);
    }
    let store = astra_services::DatabaseRunStateStore::new(shared.clone());
    let mut parent = None;
    for (user, session, model, child) in [
        (&owner, &ids[0], Some("admitted-model"), false),
        (&owner, &ids[0], Some("child-model"), true),
        (&owner, &ids[1], None, false),
        (&foreign, &ids[0], Some("foreign-model"), false),
    ] {
        let mut record = common::collaborator_fixture::record(
            user,
            session,
            if child { parent.as_deref() } else { None },
        );
        if let Some(model) = model {
            record.events = vec![
                serde_json::json!({"event_type":"run_started","data":{"model_selection":{"model_name":model}}}),
            ];
        }
        let run = record.run_id.clone();
        astra_services::RunStateStore::insert_run(&store, record)
            .await
            .unwrap();
        if parent.is_none() {
            parent = Some(run.clone());
        }
        sqlx::query("UPDATE agent_runs SET created_at = ? WHERE user_id = ? AND run_id = ?")
            .bind(if session == &ids[1] {
                "2026-06-15 10:01:00"
            } else {
                "2026-06-15 10:00:00"
            })
            .bind(user)
            .bind(&run)
            .execute(&pool)
            .await
            .unwrap();
        // Empty openings own a physical run_created receipt. Keep every
        // fixture observation on the same historical clock as its C0 row.
        sqlx::query("UPDATE agent_run_events SET created_at = ? WHERE user_id = ? AND run_id = ?")
            .bind(if session == &ids[1] {
                "2026-06-15 10:01:00"
            } else {
                "2026-06-15 10:00:00"
            })
            .bind(user)
            .bind(&run)
            .execute(&pool)
            .await
            .unwrap();
        if user == &foreign {
            sqlx::query("UPDATE agent_run_openings SET opening_events_json = '{invalid' WHERE user_id = ? AND run_id = ?")
                .bind(user).bind(&run).execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO agent_run_events (id, run_id, event_idx, user_id, session_id, event_type, event_id, event_hash, payload_json) VALUES (?, ?, 1, ?, ?, 'tool_call', ?, ?, '{invalid')")
                .bind(Uuid::new_v4().to_string()).bind(&run).bind(user).bind(session)
                .bind(Uuid::new_v4().to_string()).bind("0".repeat(64)).execute(&pool).await.unwrap();
        }
    }
    let audit = DatabaseSessionAuditService::new(settings).with_pool(shared);
    for sort in ["created", "unknown", "turns", "tokens", "duration"] {
        for order in ["asc", "desc"] {
            for minimum in [None, Some(1)] {
                let mut expected = match sort {
                    "turns" => vec![2, 1, 0],
                    "tokens" => vec![2, 0, 1],
                    "duration" => vec![1, 2, 0],
                    _ => vec![0, 1, 2],
                };
                if minimum.is_some() {
                    expected.retain(|index| *index != 2);
                }
                if order == "desc" {
                    expected.reverse();
                }
                let mut params: AuditSessionListParams =
                    serde_json::from_value(serde_json::json!({
                        "per_page":1, "sort":sort, "order":order, "min_turns":minimum,
                    }))
                    .unwrap();
                let first = audit.list_sessions(&owner, &params).await.unwrap();
                assert_eq!(
                    first.total as usize,
                    expected.len(),
                    "{sort}/{order}/{minimum:?}"
                );
                assert_eq!(
                    first.sessions[0].session_id, ids[expected[0]],
                    "{sort}/{order}/{minimum:?}: {:?}",
                    first.sessions
                );
                let cursor = first.next_cursor.unwrap();
                params.page = 2;
                let numbered = audit.list_sessions(&owner, &params).await.unwrap();
                assert_eq!(numbered.sessions[0].session_id, ids[expected[1]]);
                if sort == "duration" {
                    let left = first.sessions[0].duration_secs;
                    let right = numbered.sessions[0].duration_secs;
                    assert!(if order == "asc" {
                        left <= right
                    } else {
                        left >= right
                    });
                    if expected[0] == 0 {
                        assert!((left - 300.25).abs() < 1e-9);
                    }
                }
                params.page = 7;
                params.after_sort_value = Some(cursor.sort_value);
                params.after_session_id = Some(cursor.session_id);
                let explicit = audit.list_sessions(&owner, &params).await.unwrap();
                assert_eq!(explicit.page, 7);
                assert_eq!(
                    explicit.total, first.total,
                    "total is independent of the cursor"
                );
                assert_eq!(
                    explicit.sessions[0].session_id,
                    numbered.sessions[0].session_id
                );
            }
        }
    }
    let mut params: AuditSessionListParams =
        serde_json::from_value(serde_json::json!({"per_page":1,"order":"asc"})).unwrap();
    let queries = common::SqlQueries::default();
    params.page = 2;
    let page = audit
        .list_sessions(&owner, &params)
        .with_subscriber(tracing_subscriber::registry().with(queries.clone()))
        .await
        .unwrap();
    assert_eq!(page.sessions[0].session_id, ids[1]);
    {
        let statements = queries
            .0
            .lock()
            .unwrap()
            .iter()
            .map(|sql| sql.split_whitespace().collect::<Vec<_>>().join(" "))
            .collect::<Vec<_>>();
        let skip = statements
            .iter()
            .find(|sql| sql.starts_with("SELECT session_id,"))
            .unwrap();
        let count = statements
            .iter()
            .find(|sql| sql.starts_with("SELECT COUNT(*) AS cnt"))
            .unwrap();
        for sql in [skip, count] {
            assert!(!sql.contains("agent_run_events") && !sql.contains("agent_events"));
            assert!(!sql.contains("GROUP BY"));
        }
        let page_sql = statements
            .iter()
            .find(|sql| sql.starts_with("WITH selected_sessions AS ("))
            .unwrap();
        assert_eq!(
            page_sql.matches("JOIN selected_sessions selected").count(),
            2
        );
    }
    for (model, expected) in [
        ("child-model", Some(0)),
        ("journal-model", Some(1)),
        ("foreign-model", None),
    ] {
        params.page = 1;
        params.model = Some(model.into());
        params.status = Some("active".into());
        params.since = Some("2026-06-15T09:30:00Z".into());
        params.until = params.since.clone();
        let filtered = audit.list_sessions(&owner, &params).await.unwrap();
        assert_eq!(filtered.total, u32::from(expected.is_some()));
        if let Some(index) = expected {
            assert_eq!(filtered.sessions[0].session_id, ids[index]);
        }
    }
    params.model = None;
    params.page = 99;
    let queries = common::SqlQueries::default();
    let empty = audit
        .list_sessions(&owner, &params)
        .with_subscriber(tracing_subscriber::registry().with(queries.clone()))
        .await
        .unwrap();
    assert!(empty.sessions.is_empty());
    assert_eq!(empty.total, 3);
    assert!(
        !queries
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|sql| sql.contains("WITH selected_runs AS")),
        "an empty page must not fall through to owner-wide tool analytics"
    );
    // A creation page observes its own rows, including the has-more row. A
    // corrupt cold opening must not poison scalar observation or its count.
    let bad_session = format!("{prefix}-d");
    seed_restore_session_fixture(&pool, &owner, &bad_session, serde_json::json!({})).await;
    sqlx::query("UPDATE agent_sessions SET created_at = '2026-06-15 09:31:00' WHERE user_id = ? AND session_id = ?")
        .bind(&owner).bind(&bad_session).execute(&pool).await.unwrap();
    let mut record = common::collaborator_fixture::record(&owner, &bad_session, None);
    record.events = vec![
        serde_json::json!({"event_type":"run_started","data":{"model_selection":{"model_name":"bad-model"}}}),
    ];
    let bad_run = record.run_id.clone();
    astra_services::RunStateStore::insert_run(&store, record)
        .await
        .unwrap();
    sqlx::query("UPDATE agent_run_openings SET opening_events_json = '{invalid' WHERE user_id = ? AND run_id = ?")
        .bind(&owner).bind(&bad_run).execute(&pool).await.unwrap();
    params = serde_json::from_value(serde_json::json!({"per_page":1,"order":"asc"})).unwrap();
    let healthy = audit.list_sessions(&owner, &params).await.unwrap();
    assert_eq!(healthy.total, 4);
    assert_eq!(healthy.sessions[0].session_id, ids[0]);
    params.after_sort_value = Some("2026-06-15 09:30:00.000000".into());
    params.after_session_id = Some(ids[2].clone());
    let selected = audit.list_sessions(&owner, &params).await.unwrap();
    assert_eq!(selected.sessions[0].session_id, bad_session);
    assert_eq!(selected.sessions[0].model.as_deref(), Some("bad-model"));
    assert!(
        astra_services::RunStateStore::load_run(&store, &owner, &bad_run)
            .await
            .is_err(),
        "execution recovery still rejects the corrupted immutable opening"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT 1")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    let mut cleanup_sessions = ids.to_vec();
    cleanup_sessions.push(bad_session);
    for user in [&owner, &foreign] {
        for table in ["agent_run_events", "agent_run_openings", "agent_runs"] {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "DELETE FROM {table} WHERE user_id = ?"
            )))
            .bind(user)
            .execute(&pool)
            .await
            .unwrap();
        }
        cleanup_agent_sessions_and_events_for_owner(&pool, user, &cleanup_sessions, &events).await;
    }
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn audit_canonical_runs_count_bound_roots_and_keep_admitted_models() {
    let (_bootstrap, mut settings) = setup_pool_and_settings().await;
    settings.db_pool_max_connections = 1;
    settings.db_pool_min_connections = 1;
    let shared = SharedPool::new(&settings).await.unwrap();
    let pool = shared.get().clone();
    let owner = Uuid::new_v4().to_string();
    let session = Uuid::new_v4().to_string();
    seed_restore_session_fixture(&pool, &owner, &session, serde_json::json!({})).await;
    let store = astra_services::DatabaseRunStateStore::new(shared.clone());
    let mut root_id = None;
    for case in 0..4 {
        let mut run = common::collaborator_fixture::record(
            &owner,
            &session,
            if case >= 2 { root_id.as_deref() } else { None },
        );
        run.agent_id = Some("bound-agent".into());
        if case == 1 {
            run.status = "waiting".into();
        }
        let model = match case {
            1 => serde_json::json!("null"),
            2 => serde_json::json!("child-model"),
            3 => serde_json::json!(123),
            _ => serde_json::json!("initial-model"),
        };
        run.events = vec![serde_json::json!({"event_type":"run_started", "data":{
            "resolved_model_selection":{"model_name": model}
        }})];
        let id = run.run_id.clone();
        astra_services::RunStateStore::insert_run(&store, run)
            .await
            .unwrap();
        let captured: Option<String> = sqlx::query_scalar(
            "SELECT admitted_model_name FROM agent_run_openings WHERE user_id = ? AND run_id = ?",
        )
        .bind(&owner)
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            captured.as_deref(),
            match case {
                0 => Some("initial-model"),
                1 => Some("null"),
                2 => Some("child-model"),
                _ => None,
            }
        );
        if case == 0 {
            root_id = Some(id.clone());
        }
        sqlx::query("UPDATE agent_runs SET resolved_model_name = 'later-auto-model' WHERE user_id = ? AND run_id = ?")
            .bind(&owner).bind(&id).execute(&pool).await.unwrap();
    }
    let audit = DatabaseSessionAuditService::new(settings).with_pool(shared);
    let summary = audit.get_summary(&owner, &session).await.unwrap();
    assert_eq!(
        summary.turn_count, 2,
        "bound roots count, child Runs do not"
    );
    assert!(summary.models_used.contains(&"initial-model".into()));
    assert!(summary.models_used.contains(&"child-model".into()));
    assert!(!summary.models_used.contains(&"later-auto-model".into()));
    assert!(
        summary.models_used.contains(&"null".into()),
        "a literal string model name is not JSON null"
    );
    assert!(
        !summary.models_used.contains(&"123".into()),
        "non-string admission values are not model names"
    );
    let stats = audit
        .get_cross_session_stats(
            &owner,
            &CrossSessionStatsParams {
                since: None,
                until: None,
            },
        )
        .await
        .unwrap();
    assert_eq!((stats.session_count, stats.total_turns), (1, 2));
    assert_eq!(stats.top_models.len(), 3);
    assert!(
        stats
            .top_models
            .iter()
            .all(|model| model.model != "later-auto-model")
    );
    let params = AuditSessionListParams {
        page: 1,
        per_page: 10,
        status: None,
        model: Some("child-model".into()),
        since: None,
        until: None,
        min_turns: None,
        sort: "created".into(),
        order: "asc".into(),
        after_sort_value: None,
        after_session_id: None,
    };
    let list = audit.list_sessions(&owner, &params).await.unwrap();
    assert_eq!(
        list.total, 1,
        "model filtering matches any admission, including a child's model"
    );
    assert_eq!(list.sessions[0].turn_count, 2);
    assert_eq!(
        audit
            .list_sessions("another-owner", &params)
            .await
            .unwrap()
            .total,
        0
    );
    // A materialized Journal root gets its admission/terminal facts from the
    // selected canonical Run, never from another Run's opening or mutable model.
    let run = root_id.unwrap();
    let event = Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO agent_events (event_id, session_id, user_id, event_type, content, run_id, turn_seq, metadata, payload_hash, ingestion_write_id) VALUES (?, ?, ?, 'user_query', 'root input', ?, 1, CAST('{}' AS JSON), ?, ?)")
        .bind(&event).bind(&session).bind(&owner).bind(&run)
        .bind(agent_event_fixture_payload_hash(serde_json::json!({"event_id":event})))
        .bind(Uuid::new_v4().to_string()).execute(&pool).await.unwrap();
    let turn_params = TurnListParams {
        page: 1,
        per_page: 1,
        after_created_at: None,
        after_event_id: None,
    };
    let queries = common::SqlQueries::default();
    let page = audit
        .list_turns(&owner, &session, &turn_params)
        .with_subscriber(tracing_subscriber::registry().with(queries.clone()))
        .await
        .unwrap();
    assert_eq!(page.turns[0].model.as_deref(), Some("initial-model"));
    assert!(!page.turns[0].has_error);
    {
        let statements = queries.0.lock().unwrap();
        let facts = statements
            .iter()
            .filter(|sql| sql.contains("AS started_at"))
            .collect::<Vec<_>>();
        assert_eq!(
            facts.len(),
            1,
            "one batch for admission, terminal and independent errors"
        );
        assert!(facts[0].contains("UNION ALL"));
        assert!(!facts[0].contains("opening_context_json"));
        assert!(!facts[0].contains("'run_finished'"));
    }
    sqlx::query("UPDATE agent_runs SET created_at = DATE_SUB(NOW(6), INTERVAL 1 SECOND) WHERE user_id = ? AND run_id = ?")
        .bind(&owner).bind(&run).execute(&pool).await.unwrap();
    assert!(
        astra_services::RunStateStore::update_run_status_with_events_if_current(
            &store,
            &owner,
            &session,
            &run,
            &["running"],
            None,
            "failed",
            None,
            Some("canonical failure"),
            &[],
        )
        .await
        .unwrap()
    );
    let failed = audit.get_turn_detail(&owner, &session, 1).await.unwrap();
    assert!(failed.has_error);
    assert_eq!(failed.error_message.as_deref(), Some("canonical failure"));
    assert!(failed.duration_ms >= 1_000);
    astra_services::RunStateStore::append_events_batch(&store, &owner, &session, &run, &[
        serde_json::json!({"event_type":"run_error","data":{"error":"independent failure detail"}}),
        serde_json::json!({"event_type":"run_error","data":{"error":null}}),
    ]).await.unwrap();
    let later = audit.get_turn_detail(&owner, &session, 1).await.unwrap();
    assert_eq!(
        later.error_message.as_deref(),
        Some("independent failure detail")
    );
    assert_eq!(
        later.duration_ms, failed.duration_ms,
        "post-terminal events do not move terminal time"
    );
    sqlx::query("UPDATE agent_events SET metadata = CAST('{\"duration_ms\":0}' AS JSON) WHERE user_id = ? AND event_id = ?")
        .bind(&owner).bind(&event).execute(&pool).await.unwrap();
    assert_eq!(
        audit
            .get_turn_detail(&owner, &session, 1)
            .await
            .unwrap()
            .duration_ms,
        0,
        "a measured zero must not become a Run lifetime estimate"
    );
    sqlx::query("UPDATE agent_run_events SET payload_json = '{invalid' WHERE user_id = ? AND run_id = ? AND event_type = 'run_error' AND event_idx = 1")
        .bind(&owner).bind(&run).execute(&pool).await.unwrap();
    let error = audit
        .get_turn_detail(&owner, &session, 1)
        .await
        .unwrap_err();
    assert!(error.1.0.detail.contains("invalid payload JSON"));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT 1")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1,
        "selected error decoding must release the sole connection"
    );
    for table in ["agent_run_events", "agent_run_openings", "agent_runs"] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM {table} WHERE user_id = ? AND session_id = ?"
        )))
        .bind(&owner)
        .bind(&session)
        .execute(&pool)
        .await
        .unwrap();
    }
    cleanup_agent_sessions_and_events_for_owner(&pool, &owner, &[session], &[event]).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn audit_turn_page_decodes_only_selected_runs() {
    let (_bootstrap, mut settings) = setup_pool_and_settings().await;
    settings.db_pool_max_connections = 1;
    settings.db_pool_min_connections = 1;
    let shared = SharedPool::new(&settings).await.unwrap();
    let pool = shared.get().clone();
    let owner = Uuid::new_v4().to_string();
    let session = Uuid::new_v4().to_string();
    let runs = [Uuid::new_v4().to_string(), Uuid::new_v4().to_string()];
    seed_restore_session_fixture(&pool, &owner, &session, serde_json::json!({})).await;
    let mut events = Vec::new();
    for (turn, run) in runs.iter().enumerate() {
        let event = Uuid::new_v4().to_string();
        let timestamp = format!("2026-10-01 00:0{turn}:00.000000");
        sqlx::query("INSERT INTO agent_events (event_id, session_id, user_id, event_type, content, run_id, turn_seq, metadata, payload_hash, ingestion_write_id, created_at) VALUES (?, ?, ?, 'user_query', 'page fixture', ?, ?, CAST('{}' AS JSON), ?, ?, ?)")
            .bind(&event).bind(&session).bind(&owner).bind(run).bind(turn as i32 + 1)
            .bind(agent_event_fixture_payload_hash(serde_json::json!({"event_id":event})))
            .bind(Uuid::new_v4().to_string()).bind(timestamp).execute(&pool).await.unwrap();
        events.push(event);
    }
    for (user, run, index, kind, payload) in [
        (
            owner.as_str(),
            runs[0].as_str(),
            1_i64,
            "tool_call",
            r#"{"name":"bash","id":"selected"}"#,
        ),
        (
            owner.as_str(),
            runs[0].as_str(),
            2,
            "tool_call_end",
            r#"{"name":"bash","id":"selected","success":true}"#,
        ),
        (
            owner.as_str(),
            runs[0].as_str(),
            3,
            "tool_call",
            r#"{"name":"read_file","id":"pending"}"#,
        ),
        (owner.as_str(), runs[1].as_str(), 1, "tool_call", "{invalid"),
        ("other-owner", runs[0].as_str(), 1, "tool_call", "{invalid"),
    ] {
        sqlx::query("INSERT INTO agent_run_events (id, run_id, event_idx, user_id, session_id, event_type, event_id, event_hash, payload_json) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)")
            .bind(Uuid::new_v4().to_string()).bind(run).bind(index).bind(user).bind(&session)
            .bind(kind).bind(Uuid::new_v4().to_string())
            .bind(format!("{:x}", sha2::Sha256::digest(payload.as_bytes())))
            .bind(payload).execute(&pool).await.unwrap();
    }
    for (call, name, kind) in [
        ("pending", "read_file", "tool_call_failed"),
        ("independent", "rg", "tool_call_completed"),
    ] {
        let id = Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO agent_events (event_id, session_id, user_id, event_type, content, run_id, parent_event_id, tool_call_id, meta_tool_name, metadata, payload_hash, ingestion_write_id) VALUES (?, ?, ?, ?, 'known Journal outcome', ?, ?, ?, ?, CAST('{}' AS JSON), ?, ?)")
            .bind(&id).bind(&session).bind(&owner).bind(kind).bind(&runs[0]).bind(&events[0]).bind(call).bind(name)
            .bind(agent_event_fixture_payload_hash(serde_json::json!({"event_id":id})))
            .bind(Uuid::new_v4().to_string()).execute(&pool).await.unwrap();
        events.push(id);
    }
    let audit = DatabaseSessionAuditService::new(settings).with_pool(shared);
    let mut params = TurnListParams {
        page: 1,
        per_page: 1,
        after_created_at: None,
        after_event_id: None,
    };
    let queries = common::SqlQueries::default();
    let page = audit
        .list_turns(&owner, &session, &params)
        .with_subscriber(tracing_subscriber::registry().with(queries.clone()))
        .await
        .unwrap();
    assert_eq!(page.total, 2);
    assert_eq!(page.turns.len(), 1);
    assert_eq!(page.turns[0].turn, 1);
    assert_eq!(page.turns[0].tool_calls.len(), 3);
    let bash = page.turns[0]
        .tool_calls
        .iter()
        .find(|tool| tool.name == "bash")
        .unwrap();
    assert!(bash.ok);
    assert!(
        page.turns[0]
            .tool_calls
            .iter()
            .any(|tool| tool.name == "rg" && tool.ok)
    );
    assert!(
        page.turns[0]
            .tool_calls
            .iter()
            .any(|tool| tool.name == "read_file" && !tool.ok)
    );
    assert!(
        page.turns[0].has_error,
        "pending durable calls retain known Journal failures"
    );
    let detail = audit.get_turn_detail(&owner, &session, 1).await.unwrap();
    assert_eq!(detail.tool_calls.len(), 3);
    assert!(detail.has_error);
    let payload = r#"{"call_id":"pending","success":true}"#;
    sqlx::query("INSERT INTO agent_run_events (id, run_id, event_idx, user_id, session_id, event_type, event_id, event_hash, payload_json) VALUES (?, ?, 4, ?, ?, 'tool_call_end', ?, ?, ?)")
        .bind(Uuid::new_v4().to_string()).bind(&runs[0]).bind(&owner).bind(&session)
        .bind(Uuid::new_v4().to_string()).bind(format!("{:x}", sha2::Sha256::digest(payload.as_bytes())))
        .bind(payload).execute(&pool).await.unwrap();
    let settled = audit.list_turns(&owner, &session, &params).await.unwrap();
    assert_eq!(
        settled.turns[0].tool_calls.len(),
        3,
        "exact call identity prevents a duplicate brief"
    );
    assert!(settled.turns[0].tool_calls.iter().all(|tool| tool.ok));
    assert!(
        !settled.turns[0].has_error,
        "a conflicting Journal tool result is replaced by the known canonical result"
    );
    let payload = r#"{"call_id":"pending","success":false,"executed":false}"#;
    sqlx::query("INSERT INTO agent_run_events (id, run_id, event_idx, user_id, session_id, event_type, event_id, event_hash, payload_json) VALUES (?, ?, 6, ?, ?, 'tool_call_end', ?, ?, ?)")
        .bind(Uuid::new_v4().to_string()).bind(&runs[0]).bind(&owner).bind(&session)
        .bind(Uuid::new_v4().to_string()).bind(format!("{:x}", sha2::Sha256::digest(payload.as_bytes())))
        .bind(payload).execute(&pool).await.unwrap();
    let rejected = audit.list_turns(&owner, &session, &params).await.unwrap();
    assert_eq!(rejected.turns[0].tool_calls.len(), 2);
    assert!(!rejected.turns[0].has_error);
    let payload = r#"{"event_type":"run_error","data":{"error":"standalone error fact"}}"#;
    sqlx::query("INSERT INTO agent_run_events (id, run_id, event_idx, user_id, session_id, event_type, event_id, event_hash, payload_json) VALUES (?, ?, 5, ?, ?, 'run_error', ?, ?, ?)")
        .bind(Uuid::new_v4().to_string()).bind(&runs[0]).bind(&owner).bind(&session)
        .bind(Uuid::new_v4().to_string()).bind(format!("{:x}", sha2::Sha256::digest(payload.as_bytes())))
        .bind(payload).execute(&pool).await.unwrap();
    let standalone = audit.get_turn_detail(&owner, &session, 1).await.unwrap();
    assert!(standalone.has_error);
    assert_eq!(
        standalone.error_message.as_deref(),
        Some("standalone error fact")
    );
    let shared_root = Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO agent_events (event_id, session_id, user_id, event_type, content, run_id, turn_seq, metadata, payload_hash, ingestion_write_id, created_at) VALUES (?, ?, ?, 'user_query', 'another turn in the same Run', ?, 3, CAST('{}' AS JSON), ?, ?, '2026-10-01 00:02:00.000000')")
        .bind(&shared_root).bind(&session).bind(&owner).bind(&runs[0])
        .bind(agent_event_fixture_payload_hash(serde_json::json!({"event_id":shared_root})))
        .bind(Uuid::new_v4().to_string()).execute(&pool).await.unwrap();
    events.push(shared_root);
    let ambiguous = audit.list_turns(&owner, &session, &params).await.unwrap();
    assert_eq!(
        ambiguous.turns[0].tool_calls.len(),
        1,
        "Run-only calls are not assigned to an arbitrary turn"
    );
    assert!(
        ambiguous.turns[0].tool_calls.iter().all(|tool| tool.ok),
        "exact Journal parent/call identity still admits the canonical result"
    );
    assert!(
        audit
            .get_turn_detail(&owner, &session, 3)
            .await
            .unwrap()
            .tool_calls
            .is_empty()
    );
    let statements = queries.0.lock().unwrap().join(" ");
    assert!(statements.contains("AND run_id IN ("));
    assert_eq!(
        audit
            .list_turns("other-owner", &session, &params)
            .await
            .unwrap_err()
            .0,
        axum::http::StatusCode::NOT_FOUND
    );
    let cursor = page.next_cursor.unwrap();
    params.after_created_at = Some(cursor.created_at);
    params.after_event_id = Some(cursor.event_id);
    let error = audit
        .list_turns(&owner, &session, &params)
        .await
        .unwrap_err();
    assert_eq!(error.0, axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    assert!(error.1.0.detail.contains("invalid payload JSON"));
    // Whole-Session analytics intentionally still validates every selected fact.
    assert_eq!(
        audit
            .get_tool_analytics(&owner, &session)
            .await
            .unwrap_err()
            .0,
        axum::http::StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT 1")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1,
        "a decoding failure must release or replace the sole streaming connection"
    );
    sqlx::query("DELETE FROM agent_run_events WHERE session_id = ?")
        .bind(&session)
        .execute(&pool)
        .await
        .unwrap();
    cleanup_agent_sessions_and_events_for_owner(&pool, &owner, &[session], &events).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn session_audit_turn_views_decode_json_columns_on_live_matrixone() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();
    let turn_event_id = Uuid::new_v4().to_string();
    let child_event_id = Uuid::new_v4().to_string();
    let error_event_id = Uuid::new_v4().to_string();

    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
         VALUES (?, ?, 'audit-turn-it', 'active', 3)",
    )
    .bind(&session_id)
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("insert audit session");

    let turn_metadata = serde_json::json!({
        "turn": 1,
        "assistant_output": "assistant reply",
        "visible_tools": ["bash", "rg"],
        "tools_used": ["bash"],
        "duration_ms": 987,
        "ttft_ms": 42,
        "context_ms": 18,
        "budget_pressure": 0.25,
        "tool_calls": [
            {"name": "bash", "ok": true, "ms": 123}
        ]
    });
    let turn_token_usage = serde_json::json!({
        "input_tokens": 21,
        "cached_input_tokens": 0,
        "cache_creation_tokens": 0,
        "output_tokens": 8,
        "total_tokens": 29,
    });
    sqlx::query(
        "INSERT INTO agent_events \
         (event_id, session_id, user_id, event_type, content, token_usage, llm_model_used, metadata, \
          payload_hash, ingestion_write_id, created_at) \
         VALUES (?, ?, ?, 'user_query', ?, CAST(? AS JSON), ?, CAST(? AS JSON), ?, ?, \
                 '2026-09-05 09:00:00.000000')",
    )
    .bind(&turn_event_id)
    .bind(&session_id)
    .bind(&user_id)
    .bind("show audit turn")
    .bind(turn_token_usage.to_string())
    .bind("gpt-5.4")
    .bind(turn_metadata.to_string())
    .bind(agent_event_fixture_payload_hash(serde_json::json!({
        "event_id": &turn_event_id,
        "session_id": &session_id,
        "user_id": &user_id,
        "event_type": "user_query",
        "content": "show audit turn",
        "token_usage": &turn_token_usage,
        "llm_model_used": "gpt-5.4",
        "metadata": &turn_metadata,
        "created_at": "2026-09-05 09:00:00.000000",
    })))
    .bind(Uuid::new_v4().to_string())
    .execute(&pool)
    .await
    .expect("insert audit turn event");

    let child_metadata = serde_json::json!({"tool_name": "bash", "ok": true});
    sqlx::query(
        "INSERT INTO agent_events \
         (event_id, session_id, user_id, event_type, parent_event_id, content, metadata, \
          payload_hash, ingestion_write_id, created_at) \
         VALUES (?, ?, ?, 'tool_call_completed', ?, 'tool child', CAST(? AS JSON), ?, ?, \
                 '2026-09-05 09:00:01.000000')",
    )
    .bind(&child_event_id)
    .bind(&session_id)
    .bind(&user_id)
    .bind(&turn_event_id)
    .bind(child_metadata.to_string())
    .bind(agent_event_fixture_payload_hash(serde_json::json!({
        "event_id": &child_event_id,
        "session_id": &session_id,
        "user_id": &user_id,
        "event_type": "tool_call_completed",
        "parent_event_id": &turn_event_id,
        "content": "tool child",
        "metadata": &child_metadata,
        "created_at": "2026-09-05 09:00:01.000000",
    })))
    .bind(Uuid::new_v4().to_string())
    .execute(&pool)
    .await
    .expect("insert audit child event");

    let error_metadata = serde_json::json!({"turn": 1, "error": "boom"});
    sqlx::query(
        "INSERT INTO agent_events \
         (event_id, session_id, user_id, event_type, content, metadata, payload_hash, \
          ingestion_write_id, created_at) \
         VALUES (?, ?, ?, 'turn_error', 'turn failed', CAST(? AS JSON), ?, ?, \
                 '2026-09-05 09:00:02.000000')",
    )
    .bind(&error_event_id)
    .bind(&session_id)
    .bind(&user_id)
    .bind(error_metadata.to_string())
    .bind(agent_event_fixture_payload_hash(serde_json::json!({
        "event_id": &error_event_id,
        "session_id": &session_id,
        "user_id": &user_id,
        "event_type": "turn_error",
        "content": "turn failed",
        "metadata": &error_metadata,
        "created_at": "2026-09-05 09:00:02.000000",
    })))
    .bind(Uuid::new_v4().to_string())
    .execute(&pool)
    .await
    .expect("insert audit error event");

    let audit = DatabaseSessionAuditService::new(settings.clone()).with_pool(shared.clone());
    let turns = audit
        .list_turns(
            &user_id,
            &session_id,
            &TurnListParams {
                page: 1,
                per_page: 20,
                after_created_at: None,
                after_event_id: None,
            },
        )
        .await
        .expect("list turns");
    assert_eq!(turns.total, 1);
    assert_eq!(turns.turns.len(), 1);
    assert_eq!(turns.turns[0].turn, 1);
    assert_eq!(turns.turns[0].tokens_in, Some(21));
    assert_eq!(turns.turns[0].tokens_out, Some(8));
    assert_eq!(turns.turns[0].duration_ms, 987);
    assert_eq!(turns.turns[0].model.as_deref(), Some("gpt-5.4"));
    assert_eq!(turns.turns[0].tool_calls.len(), 1);
    assert_eq!(turns.turns[0].tool_calls[0].name, "bash");

    let detail = audit
        .get_turn_detail(&user_id, &session_id, 1)
        .await
        .expect("get turn detail");
    assert_eq!(detail.turn, 1);
    assert_eq!(detail.user_input, "show audit turn");
    assert_eq!(detail.assistant_output, "assistant reply");
    assert_eq!(detail.tokens_in, Some(21));
    assert_eq!(detail.tokens_out, Some(8));
    assert_eq!(detail.duration_ms, 987);
    assert_eq!(detail.ttft_ms, Some(42));
    assert_eq!(detail.context_ms, Some(18));
    assert_eq!(detail.budget_pressure, Some(0.25));
    assert_eq!(
        detail.visible_tools,
        vec!["bash".to_string(), "rg".to_string()]
    );
    assert_eq!(detail.tools_used, vec!["bash".to_string()]);
    assert_eq!(detail.child_events.len(), 1);
    assert_eq!(detail.child_events[0].event_id, child_event_id);
    assert_eq!(
        detail.child_events[0].metadata.get("tool_name"),
        Some(&serde_json::json!("bash"))
    );

    let errors = audit
        .list_errors(&user_id, &session_id)
        .await
        .expect("list errors");
    assert_eq!(errors.total, 1);
    assert_eq!(errors.errors.len(), 1);
    assert_eq!(errors.errors[0].event_id, error_event_id);
    assert_eq!(errors.errors[0].turn, Some(1));
    assert_eq!(
        errors.errors[0].metadata.get("error"),
        Some(&serde_json::json!("boom"))
    );

    cleanup_agent_sessions_and_events_for_owner(
        &pool,
        &user_id,
        std::slice::from_ref(&session_id),
        &[turn_event_id, child_event_id, error_event_id],
    )
    .await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn session_audit_prices_historical_physical_attempts_without_catalog_repricing() {
    let (_bootstrap, mut settings) = setup_pool_and_settings().await;
    settings.db_pool_max_connections = 1;
    settings.db_pool_min_connections = 1;
    let shared = SharedPool::new(&settings).await.unwrap();
    let pool = shared.get().clone();

    let user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();
    let event_id = Uuid::new_v4().to_string();
    let model_id = Uuid::new_v4().to_string();
    let model_name = format!("audit-cost-{}", Uuid::new_v4().simple());

    sqlx::query(
        "INSERT INTO infra_llm_models \
         (model_id, model_name, provider, is_active, context_window, input_modalities, \
          output_modalities, supported_parameters, pricing, tags, quirks) \
         VALUES (?, ?, 'mock', 1, 128000, CAST(? AS JSON), CAST(? AS JSON), \
          CAST(? AS JSON), CAST(? AS JSON), CAST(? AS JSON), CAST(? AS JSON))",
    )
    .bind(&model_id)
    .bind(&model_name)
    .bind(r#"["text"]"#)
    .bind(r#"["text"]"#)
    .bind("[]")
    .bind(
        r#"{"currency":"USD","unit":"per_token","prompt":0.000002,"completion":0.000008,"cache_read":0.0000005,"cache_write":0.0000015}"#,
    )
    .bind("[]")
    .bind("{}")
    .execute(&pool)
    .await
    .expect("insert priced model");

    sqlx::query(
        "INSERT INTO agent_sessions \
         (session_id, user_id, title, status, event_count, created_at, updated_at, last_active_at) \
         VALUES (?, ?, 'audit-cost-it', 'active', 1, NOW(6), NOW(6), NOW(6))",
    )
    .bind(&session_id)
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("insert audit session");

    let usage = serde_json::json!({
        "input_tokens": 1_000_000,
        "cached_input_tokens": 2_000_000,
        "cache_creation_tokens": 1_000_000,
        "output_tokens": 500_000,
        "total_tokens": 4_500_000,
    });
    sqlx::query(
        "INSERT INTO agent_events \
         (event_id, session_id, user_id, event_type, content, token_usage, llm_model_used, \
          token_input, token_output, token_total, turn_seq, payload_hash, ingestion_write_id, \
          created_at) \
         VALUES (?, ?, ?, 'user_query', '{}', CAST(? AS JSON), ?, 3000000, 1500000, \
          4500000, 1, ?, ?, NOW(6))",
    )
    .bind(&event_id)
    .bind(&session_id)
    .bind(&user_id)
    .bind(usage.to_string())
    .bind(&model_name)
    .bind(agent_event_fixture_payload_hash(serde_json::json!({
        "event_id": &event_id,
        "session_id": &session_id,
        "user_id": &user_id,
        "event_type": "user_query",
        "content": "{}",
        "token_usage": &usage,
        "llm_model_used": &model_name,
        "token_input": 3_000_000,
        "token_output": 1_500_000,
        "token_total": 4_500_000,
        "turn_seq": 1,
    })))
    .bind(Uuid::new_v4().to_string())
    .execute(&pool)
    .await
    .expect("insert priced audit event");

    let historical_price_snapshot = r#"{"calculation_version":1,"currency":"USD","unit":"per_token","source":"configured","prompt":0.000002,"completion":0.000008,"cache_read":0.0000005,"cache_write":0.0000015,"configuration_updated_at":"2026-09-30"}"#;
    let route_ids = [Uuid::new_v4().to_string(), Uuid::new_v4().to_string()];
    let invocation_ids = [Uuid::new_v4().to_string(), Uuid::new_v4().to_string()];
    for (route_id, invocation_id, run_id, operation_id) in [
        (
            &route_ids[0],
            &invocation_ids[0],
            "audit-root-run",
            "audit_root_request",
        ),
        (
            &route_ids[1],
            &invocation_ids[1],
            "audit-child-run",
            "audit_child_request",
        ),
    ] {
        sqlx::query(
            "INSERT INTO inference_routes \
             (route_id, user_id, session_id, scope_kind, run_id, offering_id, \
              resolved_model_name, upstream_model_name, provider, execution_placement, \
              access_kind, purpose, price_snapshot_json, created_at) \
             VALUES (?, ?, ?, 'run', ?, 'audit-offering', ?, ?, 'mock', 'server', \
                     'self_hosted', 'primary_agent', CAST(? AS JSON), NOW(6))",
        )
        .bind(route_id)
        .bind(&user_id)
        .bind(&session_id)
        .bind(run_id)
        .bind(&model_name)
        .bind(&model_name)
        .bind(historical_price_snapshot)
        .execute(&pool)
        .await
        .expect("insert historical priced route");

        sqlx::query(
            "INSERT INTO inference_invocations \
             (invocation_id, route_id, user_id, session_id, scope_kind, run_id, \
              admission_token, owner_token, owner_generation, owner_lease_expires_at, \
              turn_index, round_index, operation_id, logical_attempt, purpose, status, \
              terminal_fingerprint, usage_status, provider_delivery_state, created_at, terminal_at) \
             VALUES (?, ?, ?, ?, 'run', ?, ?, ?, 1, DATE_ADD(NOW(6), INTERVAL 60 SECOND), \
                     1, 0, ?, 0, 'primary_agent', 'succeeded', NULL, \
                     'provider_exact', 'delivery_authorized', NOW(6), NOW(6))",
        )
        .bind(invocation_id)
        .bind(route_id)
        .bind(&user_id)
        .bind(&session_id)
        .bind(run_id)
        .bind("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        .bind("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
        .bind(operation_id)
        .execute(&pool)
        .await
        .expect("insert historical priced invocation");
    }

    let other_user = Uuid::new_v4().to_string();
    let attempt_ids = [
        Uuid::new_v4().to_string(),
        Uuid::new_v4().to_string(),
        Uuid::new_v4().to_string(),
        Uuid::new_v4().to_string(),
        Uuid::new_v4().to_string(),
    ];
    let foreign_invocation_id = Uuid::new_v4().to_string();
    for (attempt_id, invocation_id, owner, run_id, attempt_index, status, coverage, counts) in [
        (
            &attempt_ids[0],
            &invocation_ids[0],
            &user_id,
            "audit-root-run",
            0_i64,
            "succeeded",
            "provider_exact",
            [1_000_000, 500_000, 2_000_000, 1_000_000],
        ),
        (
            &attempt_ids[1],
            &invocation_ids[1],
            &user_id,
            "audit-child-run",
            0_i64,
            "failed",
            "provider_exact",
            [30_000, 4_000, 100, 20],
        ),
        (
            &attempt_ids[2],
            &invocation_ids[1],
            &user_id,
            "audit-child-run",
            1_i64,
            "succeeded",
            "provider_exact",
            [10_000, 2_000, 10, 5],
        ),
        (
            &attempt_ids[3],
            &foreign_invocation_id,
            &other_user,
            "foreign-run",
            0_i64,
            "succeeded",
            "provider_exact",
            [99_000_000, 0, 0, 0],
        ),
    ] {
        sqlx::query(
            "INSERT INTO inference_provider_attempts \
             (attempt_id, invocation_id, user_id, session_id, run_id, attempt_index, provider, \
              admission_token, provider_protocol, provider_wire_hash, provider_wire_bytes, \
              status, usage_status, input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens) \
             VALUES (?, ?, ?, ?, ?, ?, 'mock', ?, 'openai_compatible', ?, 1, ?, ?, ?, ?, ?, ?)",
        )
        .bind(attempt_id)
        .bind(invocation_id)
        .bind(owner)
        .bind(&session_id)
        .bind(run_id)
        .bind(attempt_index)
        .bind("00000000000000000000000000000000")
        .bind("0".repeat(64))
        .bind(status)
        .bind(coverage)
        .bind(counts[0])
        .bind(counts[1])
        .bind(counts[2])
        .bind(counts[3])
        .execute(&pool)
        .await
        .expect("insert physical attempt");
    }

    let audit = DatabaseSessionAuditService::new(settings).with_pool(shared);
    let summary = audit
        .get_summary(&user_id, &session_id)
        .await
        .expect("get session summary");
    assert_eq!(summary.request_usage.request_count, 3);
    assert_eq!(
        summary.request_usage.fresh_input_tokens.known_tokens,
        Some(1_040_000)
    );
    assert_eq!(
        summary.request_usage.fresh_input_tokens.observed_attempts,
        3
    );
    assert_eq!(
        summary.request_usage.output_tokens.known_tokens,
        Some(506_000)
    );
    assert_eq!(summary.request_usage.output_tokens.observed_attempts, 3);
    assert_eq!(
        summary.request_usage.cache_read_tokens.known_tokens,
        Some(2_000_110)
    );
    assert_eq!(summary.request_usage.cache_read_tokens.observed_attempts, 3);
    assert_eq!(
        summary.request_usage.cache_creation_tokens.known_tokens,
        Some(1_000_025)
    );
    assert_eq!(
        summary
            .request_usage
            .cache_creation_tokens
            .observed_attempts,
        3
    );
    assert!((summary.cost.estimated_cost_usd.unwrap() - 8.6280925).abs() < 1e-10);
    assert_eq!(summary.cost.unavailable_reason, None);

    sqlx::query("UPDATE infra_llm_models SET pricing = CAST(? AS JSON) WHERE model_id = ?")
        .bind(
            r#"{"currency":"USD","unit":"per_token","prompt":0.2,"completion":0.8,"cache_read":0.05,"cache_write":0.15}"#,
        )
        .bind(&model_id)
        .execute(&pool)
        .await
        .expect("change current catalog price");
    let after_catalog_change = audit
        .get_summary(&user_id, &session_id)
        .await
        .expect("get summary after catalog change");
    assert!((after_catalog_change.cost.estimated_cost_usd.unwrap() - 8.6280925).abs() < 1e-10);

    sqlx::query(
        "INSERT INTO inference_provider_attempts \
         (attempt_id, invocation_id, user_id, session_id, run_id, attempt_index, provider, \
          admission_token, provider_protocol, provider_wire_hash, provider_wire_bytes, \
          status, usage_status, input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens) \
         VALUES (?, ?, ?, ?, ?, 0, 'mock', ?, 'openai_compatible', ?, 1, \
                 'succeeded', 'provider_exact', 99, 99, 99, 99)",
    )
    .bind(&attempt_ids[4])
    .bind(Uuid::new_v4().to_string())
    .bind(&user_id)
    .bind(&session_id)
    .bind("orphan-run")
    .bind("00000000000000000000000000000000")
    .bind("0".repeat(64))
    .execute(&pool)
    .await
    .expect("insert orphan physical attempt");
    let with_orphan = audit
        .get_summary(&user_id, &session_id)
        .await
        .expect("get summary with orphan attempt");
    assert_eq!(with_orphan.request_usage.request_count, 4);
    assert_eq!(with_orphan.cost.estimated_cost_usd, None);
    assert_eq!(
        with_orphan.cost.unavailable_reason,
        Some(
            astra_services::session_audit::SessionCostUnavailableReason::HistoricalAttemptCoverageIncomplete
        )
    );

    sqlx::query(
        "DELETE FROM inference_provider_attempts WHERE session_id = ? AND attempt_id IN (?, ?, ?, ?, ?)",
    )
    .bind(&session_id)
    .bind(&attempt_ids[0])
    .bind(&attempt_ids[1])
    .bind(&attempt_ids[2])
    .bind(&attempt_ids[3])
    .bind(&attempt_ids[4])
    .execute(&pool)
    .await
    .expect("delete physical attempts");

    sqlx::query("DELETE FROM inference_invocations WHERE user_id = ? AND session_id = ?")
        .bind(&user_id)
        .bind(&session_id)
        .execute(&pool)
        .await
        .expect("delete audit invocations");
    sqlx::query("DELETE FROM inference_routes WHERE user_id = ? AND session_id = ?")
        .bind(&user_id)
        .bind(&session_id)
        .execute(&pool)
        .await
        .expect("delete audit routes");

    cleanup_agent_sessions_and_events_for_owner(
        &pool,
        &user_id,
        std::slice::from_ref(&session_id),
        std::slice::from_ref(&event_id),
    )
    .await;
    sqlx::query("DELETE FROM infra_llm_models WHERE model_id = ?")
        .bind(&model_id)
        .execute(&pool)
        .await
        .expect("delete priced model");
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn session_restore_cloud_roundtrip_separates_causal_resume_from_picker_metadata() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();
    let event_service = DatabaseEventService::new(settings).with_pool(shared.clone());

    let user_id = Uuid::new_v4().to_string();
    let session_a = Uuid::new_v4().to_string();
    let session_b = Uuid::new_v4().to_string();
    let existing_metadata_a =
        serde_json::json!({"agent_id":"astra-server","note":"keep me","git_branch":"feature/cloud-sync","model":"gpt-5.4"}).to_string();

    cleanup_restore_fixture_for_owner(&pool, &user_id, &[session_a.clone(), session_b.clone()])
        .await;

    sqlx::query(
        "INSERT INTO agent_sessions \
         (session_id, user_id, title, status, event_count, metadata, created_at, updated_at, last_active_at) \
         VALUES (?, ?, 'Cloud restore A', 'active', 0, CAST(? AS JSON), NOW(), NOW(), NOW())",
    )
    .bind(&session_a)
    .bind(&user_id)
    .bind(&existing_metadata_a)
    .execute(&pool)
    .await
    .expect("insert existing session A");

    seed_restore_session_fixture(
        &pool,
        &user_id,
        &session_b,
        serde_json::json!({"git_branch": Some("legacy-fallback"), "model": null}),
    )
    .await;

    for (session_id, title) in [
        (&session_a, "Cloud restore A"),
        (&session_b, "Cloud restore B"),
    ] {
        sqlx::query("UPDATE agent_sessions SET title = ? WHERE session_id = ? AND user_id = ?")
            .bind(title)
            .bind(session_id)
            .bind(&user_id)
            .execute(&pool)
            .await
            .expect("update title");
    }

    for (event_id, session_id, turn_seq, content, token_in, token_out, model, ts) in [
        (
            Uuid::new_v4().to_string(),
            session_a.clone(),
            1_i64,
            "first turn",
            120_i64,
            30_i64,
            "gpt-5.4".to_string(),
            "2026-09-01 10:00:00.000000".to_string(),
        ),
        (
            Uuid::new_v4().to_string(),
            session_a.clone(),
            2_i64,
            "second turn",
            80_i64,
            20_i64,
            "gpt-5.4".to_string(),
            "2026-09-01 10:01:00.000000".to_string(),
        ),
        (
            Uuid::new_v4().to_string(),
            session_b.clone(),
            1_i64,
            "legacy turn",
            40_i64,
            10_i64,
            "claude-sonnet-4.5".to_string(),
            "2026-09-02 08:00:00.000000".to_string(),
        ),
    ] {
        let token_total = token_in + token_out;
        let token_usage = serde_json::json!({
            "input_tokens": token_in,
            "cached_input_tokens": 0,
            "cache_creation_tokens": 0,
            "output_tokens": token_out,
            "total_tokens": token_total,
        });
        sqlx::query(
            "INSERT INTO agent_events \
             (event_id, session_id, user_id, event_type, content, token_usage, llm_model_used, \
              token_input, token_output, token_total, turn_seq, payload_hash, ingestion_write_id, \
              created_at) \
             VALUES (?, ?, ?, 'user_query', ?, CAST(? AS JSON), ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&event_id)
        .bind(&session_id)
        .bind(&user_id)
        .bind(content)
        .bind(token_usage.to_string())
        .bind(&model)
        .bind(token_in)
        .bind(token_out)
        .bind(token_total)
        .bind(turn_seq)
        .bind(agent_event_fixture_payload_hash(serde_json::json!({
            "event_id": &event_id,
            "session_id": &session_id,
            "user_id": &user_id,
            "event_type": "user_query",
            "content": content,
            "token_usage": &token_usage,
            "llm_model_used": &model,
            "token_input": token_in,
            "token_output": token_out,
            "token_total": token_total,
            "turn_seq": turn_seq,
            "created_at": &ts,
        })))
        .bind(Uuid::new_v4().to_string())
        .bind(&ts)
        .execute(&pool)
        .await
        .expect("insert user_query");
    }

    // These fixture rows bypass the event writer, so keep the denormalized
    // counter faithful before testing context-trace delta updates.
    for (session_id, event_count) in [(&session_a, 2_i64), (&session_b, 1_i64)] {
        sqlx::query(
            "UPDATE agent_sessions SET event_count = ? WHERE session_id = ? AND user_id = ?",
        )
        .bind(event_count)
        .bind(session_id)
        .bind(&user_id)
        .execute(&pool)
        .await
        .expect("seed session event_count");
    }

    let trace_a = ContextTraceSignal {
        turn_id: "turn-a".into(),
        captured_at: Some("2026-09-01T10:02:30Z".into()),
        tool_surface: Some(ContextTraceToolSurface {
            tools_available: 8,
            visible_tools: vec!["view".into()],
            surface_scope: "latest_round".into(),
            latency_ms: 11,
        }),
        memory: None,
        history: None,
        budget: None,
        timing: None,
    };
    let trace_b = ContextTraceSignal {
        turn_id: "turn-b".into(),
        captured_at: Some("2026-09-02T08:00:30Z".into()),
        tool_surface: Some(ContextTraceToolSurface {
            tools_available: 6,
            visible_tools: vec!["grep".into(), "view".into()],
            surface_scope: "latest_round".into(),
            latency_ms: 9,
        }),
        memory: None,
        history: None,
        budget: None,
        timing: None,
    };

    event_service
        .create_event(
            user_id.clone(),
            context_trace_request_fixture(&session_a, &trace_a),
        )
        .await
        .expect("canonical Client trace write");
    event_service
        .create_event(
            user_id.clone(),
            context_trace_request_fixture(&session_b, &trace_b),
        )
        .await
        .expect("canonical Client trace write");

    let session_a_event_count: i64 =
        sqlx::query("SELECT event_count FROM agent_sessions WHERE session_id = ? AND user_id = ?")
            .bind(&session_a)
            .bind(&user_id)
            .fetch_one(&pool)
            .await
            .expect("load session A event_count")
            .try_get("event_count")
            .expect("session A event_count");
    let session_b_event_count: i64 =
        sqlx::query("SELECT event_count FROM agent_sessions WHERE session_id = ? AND user_id = ?")
            .bind(&session_b)
            .bind(&user_id)
            .fetch_one(&pool)
            .await
            .expect("load session B event_count")
            .try_get("event_count")
            .expect("session B event_count");
    assert_eq!(
        session_a_event_count, 3,
        "context-trace push should add its inserted event delta to session A"
    );
    assert_eq!(
        session_b_event_count, 2,
        "context-trace push should add its inserted event delta to session B"
    );

    let restore = SessionRestoreService::new(pool.clone());
    let restored_a = restore
        .load_cloud_summary(&user_id, &session_a)
        .await
        .expect("restore session A")
        .expect("session A restored");
    assert!(restored_a.restored_from_cloud);
    assert_eq!(restored_a.turn_count, 2);
    assert_eq!(restored_a.total_tokens_in, 200);
    assert_eq!(restored_a.total_tokens_out, 50);
    assert_eq!(restored_a.checkpoint_count, 0);
    assert!(
        restored_a.resume_bundle.is_none(),
        "isolated user_query rows do not prove a complete causal conversation"
    );
    assert!(
        restored_a.recent_tools.is_empty(),
        "an ordinary checkpoint without the selected conversation cursor cannot supply prompt state"
    );
    assert_eq!(
        restored_a.git_branch.as_deref(),
        Some("feature/cloud-sync"),
        "cloud restore should recover git_branch from session metadata"
    );
    assert!(
        restored_a.model.is_none(),
        "picker metadata without a cursor cannot select the resumed provider"
    );
    assert!(
        restored_a.last_context_trace.is_none(),
        "diagnostic context traces are not causal resume projections"
    );

    let restored_b = restore
        .load_cloud_summary(&user_id, &session_b)
        .await
        .expect("restore session B")
        .expect("session B restored");
    assert!(restored_b.restored_from_cloud);
    assert_eq!(restored_b.turn_count, 1);
    assert_eq!(restored_b.total_tokens_in, 40);
    assert_eq!(restored_b.total_tokens_out, 10);
    assert_eq!(restored_b.checkpoint_count, 0);
    assert!(
        restored_b.resume_bundle.is_none(),
        "an incomplete transcript must not manufacture a causal resume bundle"
    );
    assert!(
        restored_b.recent_tools.is_empty(),
        "context-trace tool visibility is diagnostic, not resumable tool state"
    );
    assert_eq!(restored_b.git_branch.as_deref(), Some("legacy-fallback"));
    assert!(
        restored_b.model.is_none(),
        "an event-level model observation without the selected cursor cannot bind the next request"
    );

    let resumable = restore
        .list_resumable_sessions(&user_id)
        .await
        .expect("list resumable sessions");
    let listed_a = resumable
        .iter()
        .find(|session| session.session_id == session_a)
        .expect("session A listed");
    assert_eq!(
        listed_a.turn_count, 0,
        "directory requires a canonical head"
    );
    assert_eq!(listed_a.model.as_deref(), Some("gpt-5.4"));
    assert_eq!(listed_a.git_branch.as_deref(), Some("feature/cloud-sync"));
    let listed_b = resumable
        .iter()
        .find(|session| session.session_id == session_b)
        .expect("session B listed");
    assert_eq!(
        listed_b.turn_count, 0,
        "diagnostic events do not supply directory turns"
    );
    assert!(
        listed_b.model.is_none(),
        "directory does not infer a model from events"
    );
    assert_eq!(listed_b.git_branch.as_deref(), Some("legacy-fallback"));

    cleanup_restore_fixture_for_owner(&pool, &user_id, &[session_a, session_b]).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn session_restore_turn_count_uses_turn_seq_high_watermark() {
    let (shared, _settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let user_id = Uuid::new_v4().to_string();
    let other_user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();
    let first_event_id = Uuid::new_v4().to_string();
    let fourth_event_id = Uuid::new_v4().to_string();

    cleanup_restore_fixture_for_owner(&pool, &user_id, std::slice::from_ref(&session_id)).await;

    seed_restore_session_fixture(
        &pool,
        &user_id,
        &session_id,
        serde_json::json!({"git_branch": Some("feature/sparse-turns"), "model": Some("gpt-5.4")}),
    )
    .await;

    sqlx::query(
        "UPDATE agent_sessions SET title = 'sparse-turn-restore', event_count = 2 \
         WHERE session_id = ? AND user_id = ?",
    )
    .bind(&session_id)
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("seed sparse-turn session root");

    for (event_id, turn_seq, content, ts) in [
        (
            &first_event_id,
            1_i64,
            "first sparse restore turn",
            "2026-09-06 08:00:00.000000",
        ),
        (
            &fourth_event_id,
            4_i64,
            "fourth sparse restore turn",
            "2026-09-06 08:04:00.000000",
        ),
    ] {
        let token_usage = serde_json::json!({
            "input_tokens": 1,
            "cached_input_tokens": 0,
            "cache_creation_tokens": 0,
            "output_tokens": 1,
            "total_tokens": 2,
        });
        sqlx::query(
            "INSERT INTO agent_events \
             (event_id, session_id, user_id, event_type, content, token_usage, \
              token_input, token_output, token_total, turn_seq, payload_hash, \
              ingestion_write_id, created_at) \
             VALUES (?, ?, ?, 'user_query', ?, CAST(? AS JSON), 1, 1, 2, ?, ?, ?, ?)",
        )
        .bind(event_id)
        .bind(&session_id)
        .bind(&user_id)
        .bind(content)
        .bind(token_usage.to_string())
        .bind(turn_seq)
        .bind(agent_event_fixture_payload_hash(serde_json::json!({
            "event_id": event_id,
            "session_id": &session_id,
            "user_id": &user_id,
            "event_type": "user_query",
            "content": content,
            "token_usage": &token_usage,
            "token_input": 1,
            "token_output": 1,
            "token_total": 2,
            "turn_seq": turn_seq,
            "created_at": ts,
        })))
        .bind(Uuid::new_v4().to_string())
        .bind(ts)
        .execute(&pool)
        .await
        .expect("insert sparse-turn restore event");
    }

    // A different owner may independently use the same session ID. Its
    // history and checkpoints must not affect this owner's restore summary.
    seed_restore_session_fixture(&pool, &other_user_id, &session_id, serde_json::json!({})).await;
    sqlx::query(
        "INSERT INTO agent_events \
         (event_id, session_id, user_id, event_type, turn_seq, token_usage, \
          token_input, token_output, llm_model_used, payload_hash, ingestion_write_id) \
         VALUES (?, ?, ?, 'user_query', 99, CAST(? AS JSON), 1000, 1000, 'foreign-model', ?, ?)",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(&session_id)
    .bind(&other_user_id)
    .bind(r#"{"input_tokens":1000,"output_tokens":1000}"#)
    .bind("foreign-payload")
    .bind(Uuid::new_v4().to_string())
    .execute(&pool)
    .await
    .expect("insert foreign owner event");

    let restore = SessionRestoreService::new(pool.clone());
    let restored = restore
        .load_cloud_summary(&user_id, &session_id)
        .await
        .expect("restore sparse-turn session")
        .expect("sparse-turn session restored");
    assert_eq!(
        restored.turn_count, 4,
        "restore_session must report turn_seq high watermark, not user_query row count"
    );
    assert_eq!(restored.total_tokens_in, 2);
    assert_eq!(restored.total_tokens_out, 2);
    assert_eq!(restored.checkpoint_count, 0);

    let listed = restore
        .list_resumable_sessions(&user_id)
        .await
        .expect("list sparse-turn resumable sessions");
    let listed_session = listed
        .iter()
        .find(|session| session.session_id == session_id)
        .expect("sparse-turn session listed");
    assert_eq!(
        listed_session.turn_count, 0,
        "directory omits diagnostic turn counts; selected detail recovery retains its high watermark"
    );

    cleanup_restore_fixture_for_owners(&pool, &[session_id], &[&user_id, &other_user_id]).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn sync_audit_no_longer_persists_session_sync_log_on_live_matrixone() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();
    let event_service = DatabaseEventService::new(settings).with_pool(shared.clone());

    let user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();

    cleanup_restore_fixture_for_owner(&pool, &user_id, std::slice::from_ref(&session_id)).await;

    seed_restore_session_fixture(
        &pool,
        &user_id,
        &session_id,
        serde_json::json!({"git_branch":"feature/prune","model":"gpt-5.4"}),
    )
    .await;

    for idx in 0..6 {
        let trace = ContextTraceSignal {
            turn_id: format!("turn-{idx}"),
            captured_at: Some(format!("2026-09-03T10:{:02}:00Z", idx % 60)),
            tool_surface: Some(ContextTraceToolSurface {
                tools_available: 8,
                visible_tools: vec!["view".into()],
                surface_scope: "latest_round".into(),
                latency_ms: 7,
            }),
            memory: None,
            history: None,
            budget: None,
            timing: None,
        };
        event_service
            .create_event(
                user_id.clone(),
                context_trace_request_fixture(&session_id, &trace),
            )
            .await
            .expect("canonical Client trace write");
    }

    let sync_log_query = sqlx::query("SELECT COUNT(*) AS c FROM session_sync_log")
        .fetch_one(&pool)
        .await;
    assert!(
        sync_log_query.is_err(),
        "session_sync_log must not be part of the current schema"
    );

    let context_trace_count: i64 = sqlx::query(
        "SELECT COUNT(*) AS c FROM agent_events \
         WHERE user_id = ? AND session_id = ? AND event_type = 'context_trace_signal'",
    )
    .bind(&user_id)
    .bind(&session_id)
    .fetch_one(&pool)
    .await
    .expect("load context trace fact count")
    .try_get("c")
    .expect("context trace fact count");

    assert_eq!(
        context_trace_count, 6,
        "context trace pushes write durable agent_events facts"
    );

    cleanup_restore_fixture_for_owner(&pool, &user_id, &[session_id]).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn monotonic_remote_workspace_projection_restores_picker_metadata_on_live_matrixone() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();

    cleanup_restore_fixture_for_owner(&pool, &user_id, std::slice::from_ref(&session_id)).await;
    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
         VALUES (?, ?, 'remote-workspace-restore-it', 'active', 0)",
    )
    .bind(&session_id)
    .bind(&user_id)
    .execute(&pool)
    .await
    .expect("insert owner session for remote workspace restore");

    let mut older_workspace = WorkspaceMetadata::with_context(
        &session_id,
        "gpt-5.4",
        "/srv/remote-agent",
        Some("feature/remote-workspace-old"),
    );
    older_workspace.record_turn(120, 45, 0, 0);
    older_workspace.projection_revision = 1;
    older_workspace.last_context_trace = Some(ContextTraceSignal {
        turn_id: "turn-remote-workspace-old".into(),
        captured_at: Some("2026-09-07T10:00:00Z".into()),
        tool_surface: Some(ContextTraceToolSurface {
            tools_available: 12,
            visible_tools: vec!["bash".into(), "rg".into()],
            surface_scope: "latest_round".into(),
            latency_ms: 4,
        }),
        memory: None,
        history: None,
        budget: None,
        timing: None,
    });

    let mut newer_workspace = WorkspaceMetadata::with_context(
        &session_id,
        "gpt-5.5",
        "/srv/remote-agent",
        Some("feature/remote-workspace-new"),
    );
    newer_workspace.record_turn(120, 45, 0, 0);
    newer_workspace.record_turn(240, 90, 0, 0);
    newer_workspace.projection_revision = 2;
    newer_workspace.last_context_trace = Some(ContextTraceSignal {
        turn_id: "turn-remote-workspace-new".into(),
        captured_at: Some("2026-09-07T10:00:00Z".into()),
        tool_surface: Some(ContextTraceToolSurface {
            tools_available: 12,
            visible_tools: vec!["git".into(), "rg".into()],
            surface_scope: "latest_round".into(),
            latency_ms: 3,
        }),
        memory: None,
        history: None,
        budget: None,
        timing: None,
    });

    let artifact_store = DatabaseSessionArtifactStore::new(settings.clone()).with_pool(shared);
    let older_artifact = persist_remote_workspace(&older_workspace, &user_id, &artifact_store)
        .await
        .expect("persist initial revision 1 workspace");
    assert_eq!(older_artifact.content["projection_revision"], 1);
    let newer_artifact = persist_remote_workspace(&newer_workspace, &user_id, &artifact_store)
        .await
        .expect("persist newer remote workspace");
    let late_older = persist_remote_workspace(&older_workspace, &user_id, &artifact_store)
        .await
        .expect("late older projection is ignored");
    let idempotent = persist_remote_workspace(&newer_workspace, &user_id, &artifact_store)
        .await
        .expect("same projection revision and content is idempotent");
    let mut conflict = newer_workspace.clone();
    conflict.status = "completed".into();
    let conflict = artifact_store
        .upsert_monotonic_workspace_projection(
            to_remote_artifact_record(&conflict, &user_id).unwrap(),
            conflict.projection_revision,
        )
        .await
        .expect_err("same revision with different content must conflict");

    assert!(matches!(
        conflict,
        SessionArtifactStoreError::WorkspaceProjectionRevisionConflict { revision: 2 }
    ));
    assert_eq!(newer_artifact.artifact_id, WORKSPACE_METADATA_PROJECTION_ID);
    assert_eq!(late_older.content, newer_artifact.content);
    assert_eq!(idempotent.content, newer_artifact.content);
    assert_eq!(newer_artifact.session_id, session_id);
    assert_eq!(newer_artifact.user_id, user_id);
    assert_eq!(
        newer_artifact.artifact_kind,
        WORKSPACE_METADATA_ARTIFACT_KIND
    );
    assert_eq!(newer_artifact.turn, Some(newer_workspace.turn_count));

    let projection = artifact_store
        .load_json_artifact(&user_id, &session_id, WORKSPACE_METADATA_PROJECTION_ID)
        .await
        .expect("load remote workspace projection")
        .expect("remote workspace projection exists");
    assert_eq!(projection.content, newer_artifact.content);

    let restore = SessionRestoreService::new(pool.clone());
    let restored = restore
        .load_cloud_summary(&user_id, &session_id)
        .await
        .expect("restore session")
        .expect("session restored from remote workspace artifact");

    assert!(restored.restored_from_cloud);
    assert_eq!(
        restored.turn_count, 0,
        "workspace hints do not supply the canonical turn clock"
    );
    assert_eq!(restored.total_tokens_in, newer_workspace.total_tokens_in);
    assert_eq!(restored.total_tokens_out, newer_workspace.total_tokens_out);
    assert!(
        restored.recent_tools.is_empty(),
        "an unversioned workspace artifact cannot grant prompt-facing tool history"
    );
    assert_eq!(
        restored.git_branch.as_deref(),
        newer_workspace.git_branch.as_deref()
    );
    assert!(restored.model.is_none());
    assert!(restored.last_context_trace.is_none());
    assert!(restored.workspace.is_none());

    cleanup_restore_fixture_for_owner(&pool, &user_id, &[session_id]).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn restore_recent_tools_ignores_agent_events_turn_complete_metadata_on_live_matrixone() {
    let (shared, _settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();

    cleanup_restore_fixture_for_owner(&pool, &user_id, std::slice::from_ref(&session_id)).await;

    seed_restore_session_fixture(&pool, &user_id, &session_id, serde_json::json!({"git_branch": Some("feature/checkpoint-tools"), "model": Some("gpt-5.4")})).await;

    let query_event_id = Uuid::new_v4().to_string();
    let query_token_usage = serde_json::json!({
        "input_tokens": 20,
        "cached_input_tokens": 0,
        "cache_creation_tokens": 0,
        "output_tokens": 10,
        "total_tokens": 30,
    });
    sqlx::query(
        "INSERT INTO agent_events \
         (event_id, session_id, user_id, event_type, content, token_usage, token_input, \
          token_output, token_total, turn_seq, payload_hash, ingestion_write_id, created_at) \
         VALUES (?, ?, ?, 'user_query', 'agent events only recent tools turn', CAST(? AS JSON), \
                 20, 10, 30, 1, ?, ?, '2026-09-05 08:00:00.000000')",
    )
    .bind(&query_event_id)
    .bind(&session_id)
    .bind(&user_id)
    .bind(query_token_usage.to_string())
    .bind(agent_event_fixture_payload_hash(serde_json::json!({
        "event_id": &query_event_id,
        "session_id": &session_id,
        "user_id": &user_id,
        "event_type": "user_query",
        "content": "agent events only recent tools turn",
        "token_usage": &query_token_usage,
        "token_input": 20,
        "token_output": 10,
        "token_total": 30,
        "turn_seq": 1,
        "created_at": "2026-09-05 08:00:00.000000",
    })))
    .bind(Uuid::new_v4().to_string())
    .execute(&pool)
    .await
    .expect("insert user_query");

    for (event_id, created_at, tools_used) in [
        (
            Uuid::new_v4().to_string(),
            "2026-09-05 08:01:00.000000",
            serde_json::json!(["bash", "rg"]),
        ),
        (
            Uuid::new_v4().to_string(),
            "2026-09-05 08:02:00.000000",
            serde_json::json!(["view", "rg"]),
        ),
    ] {
        let metadata = serde_json::json!({ "tools_used": tools_used });
        sqlx::query(
            "INSERT INTO agent_events \
             (event_id, session_id, user_id, event_type, content, metadata, payload_hash, \
              ingestion_write_id, created_at) \
             VALUES (?, ?, ?, 'turn_complete', 'non-authoritative tool summary', \
                     CAST(? AS JSON), ?, ?, ?)",
        )
        .bind(&event_id)
        .bind(&session_id)
        .bind(&user_id)
        .bind(metadata.to_string())
        .bind(agent_event_fixture_payload_hash(serde_json::json!({
            "event_id": &event_id,
            "session_id": &session_id,
            "user_id": &user_id,
            "event_type": "turn_complete",
            "content": "non-authoritative tool summary",
            "metadata": &metadata,
            "created_at": created_at,
        })))
        .bind(Uuid::new_v4().to_string())
        .bind(created_at)
        .execute(&pool)
        .await
        .expect("insert non-authoritative turn_complete");
    }

    let restore = SessionRestoreService::new(pool.clone());
    let restored = restore
        .load_cloud_summary(&user_id, &session_id)
        .await
        .expect("restore session")
        .expect("session restored");

    assert_eq!(restored.turn_count, 1);
    assert_eq!(restored.checkpoint_count, 0);
    assert!(
        restored.recent_tools.is_empty(),
        "agent_events turn_complete metadata is not authoritative recent-tool state; \
         diagnostic event traces must not supply resumable tool state"
    );
    assert!(restored.last_context_trace.is_none());

    cleanup_restore_fixture_for_owner(&pool, &user_id, &[session_id]).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn client_context_trace_requires_owned_session_on_live_matrixone() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();
    let event_service = DatabaseEventService::new(settings).with_pool(shared.clone());

    let user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();

    cleanup_restore_fixture_for_owner(&pool, &user_id, std::slice::from_ref(&session_id)).await;

    let trace = ContextTraceSignal {
        turn_id: "turn-missing-row".into(),
        captured_at: Some("2026-09-06T09:00:00Z".into()),
        tool_surface: Some(ContextTraceToolSurface {
            tools_available: 8,
            visible_tools: vec!["rg".into(), "view".into()],
            surface_scope: "latest_round".into(),
            latency_ms: 5,
        }),
        memory: None,
        history: None,
        budget: None,
        timing: None,
    };

    let rejected = event_service
        .create_event(
            user_id.clone(),
            context_trace_request_fixture(&session_id, &trace),
        )
        .await
        .expect_err("Client cannot create a missing session");
    assert_eq!(rejected.0, axum::http::StatusCode::NOT_FOUND);
    for table in ["agent_sessions", "agent_events"] {
        let (rows,): (i64,) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT COUNT(*) FROM {table} WHERE user_id = ? AND session_id = ?"
        )))
        .bind(&user_id)
        .bind(&session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(rows, 0, "rejection must not create session or event rows");
    }
    seed_restore_session_fixture(&pool, &user_id, &session_id, serde_json::json!({})).await;
    event_service
        .create_event(
            user_id.clone(),
            context_trace_request_fixture(&session_id, &trace),
        )
        .await
        .expect("canonical Client trace write");

    let session_row = sqlx::query(
        "SELECT user_id, event_count FROM agent_sessions WHERE session_id = ? AND user_id = ?",
    )
    .bind(&session_id)
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .expect("load admitted session row");
    assert_eq!(
        session_row
            .try_get::<String, _>("user_id")
            .expect("session user_id"),
        user_id
    );
    assert_eq!(
        session_row
            .try_get::<i64, _>("event_count")
            .expect("session event_count"),
        1,
        "canonical trace write must update the admitted session event count"
    );
    let trace_row = sqlx::query(
        "SELECT event_type, CAST(metadata AS CHAR) AS metadata_json \
         FROM agent_events \
         WHERE session_id = ? AND user_id = ? AND event_type = 'context_trace_signal'",
    )
    .bind(&session_id)
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .expect("load persisted context trace event");
    assert_eq!(
        trace_row
            .try_get::<String, _>("event_type")
            .expect("trace event type"),
        "context_trace_signal"
    );
    let persisted_trace = serde_json::from_str::<ContextTraceSignal>(
        &trace_row
            .try_get::<String, _>("metadata_json")
            .expect("trace metadata"),
    )
    .expect("parse persisted context trace");
    assert_eq!(
        persisted_trace
            .tool_surface
            .as_ref()
            .map(|surface| surface.visible_tools.as_slice()),
        Some(["rg".to_string(), "view".to_string()].as_slice()),
        "the diagnostic event must retain its exact tool-surface evidence"
    );

    let restore = SessionRestoreService::new(pool.clone());
    let restored = restore
        .load_cloud_summary(&user_id, &session_id)
        .await
        .expect("restore session")
        .expect("session restored");
    assert!(restored.restored_from_cloud);
    assert_eq!(restored.turn_count, 0);
    assert_eq!(restored.checkpoint_count, 0);
    assert!(
        restored.recent_tools.is_empty(),
        "a cursorless context trace is diagnostic evidence, not admissible prompt state"
    );
    assert!(
        restored.last_context_trace.is_none(),
        "cursorless diagnostic events must not be projected into a causal resume bundle"
    );

    cleanup_restore_fixture_for_owner(&pool, &user_id, &[session_id]).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn event_service_binds_session_event_reads_and_counts_to_owner_on_live_matrixone() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let owner_user_id = Uuid::new_v4().to_string();
    let other_user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();
    let stray_event_id = Uuid::new_v4().to_string();
    let non_owner_session_end_event_id = Uuid::new_v4().to_string();

    cleanup_agent_sessions_and_events_for_owner(
        &pool,
        &owner_user_id,
        std::slice::from_ref(&session_id),
        &[],
    )
    .await;
    cleanup_agent_sessions_and_events_for_owner(
        &pool,
        &other_user_id,
        std::slice::from_ref(&session_id),
        &[
            stray_event_id.clone(),
            non_owner_session_end_event_id.clone(),
        ],
    )
    .await;

    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
         VALUES (?, ?, 'owner-bound-it', 'active', 0)",
    )
    .bind(&session_id)
    .bind(&owner_user_id)
    .execute(&pool)
    .await
    .expect("insert owner session");

    sqlx::query(
        "INSERT INTO agent_events (event_id, session_id, user_id, event_type, content, \
         causal_chain_id, payload_hash, ingestion_write_id) \
         VALUES (?, ?, ?, 'stray_evt', '{}', '', ?, ?)",
    )
    .bind(&stray_event_id)
    .bind(&session_id)
    .bind(&other_user_id)
    .bind(agent_event_fixture_payload_hash(serde_json::json!({
        "event_id": &stray_event_id,
        "session_id": &session_id,
        "user_id": &other_user_id,
        "event_type": "stray_evt",
        "content": "{}",
        "causal_chain_id": "",
    })))
    .bind(Uuid::new_v4().to_string())
    .execute(&pool)
    .await
    .expect("insert stray event");

    let event_service = DatabaseEventService::new(settings).with_pool(shared);
    let owner_event = event_service
        .create_event(
            owner_user_id.clone(),
            EventCreateRequestData {
                ingestion_source: astra_services::events::EventIngestionSource::Client,
                event_id: None,
                session_id: session_id.clone(),
                event_type: "tool_call_completed".into(),
                content: "owner visible".into(),
                agent_id: None,
                agent_version: None,
                parent_event_id: None,
                parent_event_ids: None,
                causal_chain_id: None,
                metadata: Some(serde_json::json!({
                    "tool_call_id": " call-owner-1 ",
                    "tool_name": " bash ",
                    "duration_ms": 17
                })),
            },
        )
        .await
        .expect("owner can create event");

    let event_projection = sqlx::query(
        "SELECT tool_call_id, meta_tool_name, meta_duration_ms \
         FROM agent_events WHERE event_id = ? AND user_id = ?",
    )
    .bind(&owner_event.record.event_id)
    .bind(&owner_user_id)
    .fetch_one(&pool)
    .await
    .expect("load canonical tool lifecycle projections");
    assert_eq!(
        event_projection
            .try_get::<String, _>("tool_call_id")
            .expect("decode tool_call_id"),
        "call-owner-1"
    );
    assert_eq!(
        event_projection
            .try_get::<String, _>("meta_tool_name")
            .expect("decode meta_tool_name"),
        "bash"
    );
    assert_eq!(
        event_projection
            .try_get::<i32, _>("meta_duration_ms")
            .expect("decode meta_duration_ms"),
        17
    );

    let stored_count =
        sqlx::query("SELECT event_count FROM agent_sessions WHERE session_id = ? AND user_id = ?")
            .bind(&session_id)
            .bind(&owner_user_id)
            .fetch_one(&pool)
            .await
            .expect("load owner session count")
            .try_get::<i64, _>("event_count")
            .expect("decode owner event_count");
    assert_eq!(
        stored_count, 1,
        "event_count delta must apply only to rows owned by the session owner"
    );

    let owner_events = event_service
        .get_session_events(session_id.clone(), owner_user_id.clone(), 100, None)
        .await
        .expect("owner can list session events");
    assert_eq!(owner_events.events.len(), 1);
    assert_eq!(owner_events.events[0].event_id, owner_event.record.event_id);

    let other_session_result = event_service
        .get_session_events(session_id.clone(), other_user_id.clone(), 100, None)
        .await;
    assert_eq!(
        other_session_result
            .expect_err("non-owner cannot list session")
            .0,
        axum::http::StatusCode::NOT_FOUND
    );

    let other_get_result = event_service
        .get_event(owner_event.record.event_id.clone(), other_user_id.clone())
        .await;
    assert_eq!(
        other_get_result
            .expect_err("non-owner cannot load owner event")
            .0,
        axum::http::StatusCode::NOT_FOUND
    );

    let other_delete_result = event_service
        .delete_event(owner_event.record.event_id.clone(), other_user_id.clone())
        .await;
    assert_eq!(
        other_delete_result
            .expect_err("non-owner cannot delete owner event")
            .0,
        axum::http::StatusCode::NOT_FOUND
    );

    let owner_event_still_exists =
        sqlx::query("SELECT COUNT(*) AS c FROM agent_events WHERE event_id = ? AND user_id = ?")
            .bind(&owner_event.record.event_id)
            .bind(&owner_user_id)
            .fetch_one(&pool)
            .await
            .expect("count owner event")
            .try_get::<i64, _>("c")
            .expect("decode owner event count");
    assert_eq!(
        owner_event_still_exists, 1,
        "failed non-owner delete must not remove the owner event"
    );

    let config = IngestionConfig {
        batch_size: 1,
        flush_interval_secs: 300,
        channel_capacity: 2,
        ..Default::default()
    };
    let (sender, shutdown, stats, join) = EventIngestionWorker::spawn(pool.clone(), config);
    sender.enqueue(IngestionEvent {
        run_id: None,
        parent_run_id: None,
        turn_seq: None,
        event_id: non_owner_session_end_event_id.clone(),
        session_id: session_id.clone(),
        user_id: other_user_id.clone(),
        event_type: "session_end".into(),
        content: Some("non-owner close attempt".into()),
        token_usage: None,
        llm_model_used: None,
        skill_name: None,
        metadata: None,
        created_at: "2026-09-03T08:30:00Z".into(),
        parent_event_id: None,
        parent_event_ids: Vec::new(),
        causal_chain_id: None,
        history_work_queue_reservation: None,
        ingestion_enqueued_at: None,
    });
    shutdown.signal();
    sender.shutdown();
    tokio::time::timeout(std::time::Duration::from_secs(10), join)
        .await
        .expect("owner-bound ingestion worker join timeout")
        .expect("owner-bound ingestion worker join");
    let ingestion_error = {
        let stats = stats.lock().expect("owner-bound ingestion stats");
        stats.last_error.clone()
    };
    assert!(
        ingestion_error.is_none(),
        "the other owner must be able to create its own same-named session"
    );

    let session_after_non_owner_end = sqlx::query(
        "SELECT status, event_count FROM agent_sessions WHERE session_id = ? AND user_id = ?",
    )
    .bind(&session_id)
    .bind(&owner_user_id)
    .fetch_one(&pool)
    .await
    .expect("load owner session after non-owner session_end");
    assert_eq!(
        session_after_non_owner_end
            .try_get::<String, _>("status")
            .expect("decode status"),
        "active",
        "non-owner session_end must not close owner session"
    );
    assert_eq!(
        session_after_non_owner_end
            .try_get::<i64, _>("event_count")
            .expect("decode event_count"),
        1,
        "the other owner's ingestion must not change owner event_count"
    );
    let other_session = sqlx::query(
        "SELECT status, event_count FROM agent_sessions WHERE session_id = ? AND user_id = ?",
    )
    .bind(&session_id)
    .bind(&other_user_id)
    .fetch_one(&pool)
    .await
    .expect("load independently created same-named session");
    assert_eq!(
        other_session
            .try_get::<String, _>("status")
            .expect("decode other status"),
        "ended"
    );
    assert_eq!(
        other_session
            .try_get::<i64, _>("event_count")
            .expect("decode other event_count"),
        1
    );

    cleanup_agent_sessions_and_events_for_owner(
        &pool,
        &owner_user_id,
        std::slice::from_ref(&session_id),
        &[owner_event.record.event_id],
    )
    .await;
    cleanup_agent_sessions_and_events_for_owner(
        &pool,
        &other_user_id,
        &[session_id],
        &[stray_event_id, non_owner_session_end_event_id],
    )
    .await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn session_owned_services_isolate_same_session_id_across_owners_on_live_matrixone() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let owner_user_id = Uuid::new_v4().to_string();
    let other_user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();
    let event_id = Uuid::new_v4().to_string();

    cleanup_restore_fixture_for_owner(&pool, &owner_user_id, std::slice::from_ref(&session_id))
        .await;
    cleanup_restore_fixture_for_owner(&pool, &other_user_id, std::slice::from_ref(&session_id))
        .await;

    let owner_metadata = serde_json::json!({"owner": true, "branch": "main"}).to_string();
    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count, metadata) \
         VALUES (?, ?, 'owner-bound-side-effects-it', 'active', 0, CAST(? AS JSON))",
    )
    .bind(&session_id)
    .bind(&owner_user_id)
    .bind(&owner_metadata)
    .execute(&pool)
    .await
    .expect("insert owner session");

    let event_service = DatabaseEventService::new(settings.clone()).with_pool(shared.clone());
    let rejected = event_service
        .create_event(
            other_user_id.clone(),
            EventCreateRequestData {
                ingestion_source: astra_services::EventIngestionSource::Client,
                event_id: Some(event_id),
                session_id: session_id.clone(),
                event_type: "owner_isolation".into(),
                content: "{}".into(),
                agent_id: None,
                agent_version: None,
                parent_event_id: None,
                parent_event_ids: None,
                causal_chain_id: None,
                metadata: None,
            },
        )
        .await
        .expect_err("foreign owner cannot write retained Session events");
    assert_eq!(rejected.0, axum::http::StatusCode::NOT_FOUND);
    seed_restore_session_fixture(
        &pool,
        &other_user_id,
        &session_id,
        serde_json::json!({"git_branch": Some("other-owner-branch"), "model": Some("gpt-5.4")}),
    )
    .await;

    let restore = SessionRestoreService::new(pool.clone());
    assert!(
        restore
            .load_cloud_summary(&other_user_id, &session_id)
            .await
            .expect("other owner restore should not error")
            .is_some(),
        "the other owner restores only its independently created session"
    );
    let metadata_after: Option<String> = sqlx::query(
        "SELECT CAST(metadata AS CHAR) AS metadata_json FROM agent_sessions WHERE session_id = ? AND user_id = ?",
    )
    .bind(&session_id)
    .bind(&owner_user_id)
    .fetch_one(&pool)
    .await
    .expect("load owner session metadata")
    .try_get("metadata_json")
    .expect("decode owner session metadata");
    let metadata_after: serde_json::Value =
        serde_json::from_str(metadata_after.as_deref().unwrap_or("{}"))
            .expect("parse owner metadata");
    assert_eq!(
        metadata_after
            .get("branch")
            .and_then(serde_json::Value::as_str),
        Some("main"),
        "same-named session sync must not mutate owner metadata"
    );

    cleanup_restore_fixture_for_owner(&pool, &owner_user_id, std::slice::from_ref(&session_id))
        .await;
    cleanup_restore_fixture_for_owner(&pool, &other_user_id, &[session_id]).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn reflect_and_introspection_ignore_mixed_owner_derived_rows_on_live_matrixone() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let owner_user_id = Uuid::new_v4().to_string();
    let other_user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();
    let owner_root_attempt = Uuid::new_v4().to_string();
    let owner_child_attempt = Uuid::new_v4().to_string();
    let other_attempt = Uuid::new_v4().to_string();
    let owner_query_event_id = Uuid::new_v4().to_string();
    let owner_llm_event_id = Uuid::new_v4().to_string();
    let owner_drift_assessment_event_id = Uuid::new_v4().to_string();
    let owner_legacy_drift_event_id = Uuid::new_v4().to_string();
    let other_query_event_id = Uuid::new_v4().to_string();
    let other_llm_event_id = Uuid::new_v4().to_string();
    let other_drift_assessment_event_id = Uuid::new_v4().to_string();
    let owner_tool_event_id = Uuid::new_v4().to_string();
    let owner_boundary_event_id = Uuid::new_v4().to_string();

    cleanup_agent_sessions_and_events_for_owner(
        &pool,
        &owner_user_id,
        std::slice::from_ref(&session_id),
        &[
            owner_query_event_id.clone(),
            owner_llm_event_id.clone(),
            owner_drift_assessment_event_id.clone(),
            owner_legacy_drift_event_id.clone(),
            owner_tool_event_id.clone(),
            owner_boundary_event_id.clone(),
        ],
    )
    .await;
    cleanup_agent_sessions_and_events_for_owner(
        &pool,
        &other_user_id,
        std::slice::from_ref(&session_id),
        &[
            other_query_event_id.clone(),
            other_llm_event_id.clone(),
            other_drift_assessment_event_id.clone(),
        ],
    )
    .await;
    cleanup_model_request_fixtures(
        &pool,
        &owner_user_id,
        &[owner_root_attempt.clone(), owner_child_attempt.clone()],
    )
    .await;
    cleanup_model_request_fixtures(&pool, &other_user_id, std::slice::from_ref(&other_attempt))
        .await;

    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
         VALUES (?, ?, 'mixed-owner-derived-it', 'active', 6)",
    )
    .bind(&session_id)
    .bind(&owner_user_id)
    .execute(&pool)
    .await
    .expect("insert owner session");

    for (user_id, attempt_id, run_id, parent_run_id, agent_id, status, response_id, created_at) in [
        (
            &owner_user_id,
            &owner_root_attempt,
            "root-run",
            None,
            "parent-agent",
            Some("succeeded"),
            Some("root-provider-response"),
            "2026-06-01 10:00:10.000000",
        ),
        (
            &owner_user_id,
            &owner_child_attempt,
            "child-run",
            Some("root-run"),
            "child-agent",
            Some("delivery_unknown"),
            None,
            "2026-06-01 10:00:20.000000",
        ),
        (
            &other_user_id,
            &other_attempt,
            "foreign-run",
            None,
            "foreign-agent",
            Some("succeeded"),
            Some("foreign-provider-response"),
            "2026-06-01 10:00:30.000000",
        ),
    ] {
        let mut event = model_request_fixture_accepted_event(
            user_id,
            &session_id,
            attempt_id,
            run_id,
            parent_run_id,
            agent_id,
        );
        insert_model_request_fixture(
            &pool,
            user_id,
            &session_id,
            attempt_id,
            &event,
            "2026-06-01 10:00:09.000000",
        )
        .await;
        if let Some(status) = status {
            event.stage = astra_services::ModelRequestEventStage::Terminal;
            event.terminal_status = Some(status.into());
            event.usage_status = Some(if status == "delivery_unknown" {
                "unavailable".into()
            } else {
                "provider_exact".into()
            });
            event.usage =
                (status != "delivery_unknown").then(|| astra_services::ModelRequestUsage {
                    input: astra_turn_types::NormalizedPromptCacheUsage::new(8, 32, 0),
                    output_tokens: 4,
                });
        }
        event.identity.provider_response_id = response_id.map(str::to_owned);
        insert_model_request_fixture(&pool, user_id, &session_id, attempt_id, &event, created_at)
            .await;
    }

    for (event_id, user_id, event_type, content, skill_name, token_usage, created_at) in [
        (
            &owner_query_event_id,
            &owner_user_id,
            "user_query",
            "owner original intent",
            "owner_skill",
            None,
            "2026-06-01 10:00:00.000000",
        ),
        (
            &owner_llm_event_id,
            &owner_user_id,
            "llm_response",
            "owner current focus",
            "owner_skill",
            Some(serde_json::json!({
                "input_tokens": 100,
                "cached_input_tokens": 0,
                "cache_creation_tokens": 0,
                "output_tokens": 10,
                "total_tokens": 110
            })),
            "2026-06-01 10:01:00.000000",
        ),
        (
            &other_query_event_id,
            &other_user_id,
            "user_query",
            "other secret intent",
            "other_skill",
            None,
            "2026-06-01 10:02:00.000000",
        ),
        (
            &other_llm_event_id,
            &other_user_id,
            "llm_response",
            "other secret focus",
            "other_skill",
            Some(serde_json::json!({
                "input_tokens": 900,
                "cached_input_tokens": 0,
                "cache_creation_tokens": 0,
                "output_tokens": 90,
                "total_tokens": 990
            })),
            "2026-06-01 10:03:00.000000",
        ),
    ] {
        let token_usage = token_usage.unwrap_or_else(|| serde_json::json!({}));
        sqlx::query(
            "INSERT INTO agent_events \
             (event_id, session_id, user_id, event_type, content, skill_name, token_usage, \
              causal_chain_id, payload_hash, ingestion_write_id, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, CAST(? AS JSON), '', ?, ?, ?)",
        )
        .bind(event_id)
        .bind(&session_id)
        .bind(user_id)
        .bind(event_type)
        .bind(content)
        .bind(skill_name)
        .bind(token_usage.to_string())
        .bind(agent_event_fixture_payload_hash(serde_json::json!({
            "event_id": event_id,
            "session_id": &session_id,
            "user_id": user_id,
            "event_type": event_type,
            "content": content,
            "skill_name": skill_name,
            "token_usage": &token_usage,
            "causal_chain_id": "",
            "created_at": created_at,
        })))
        .bind(Uuid::new_v4().to_string())
        .bind(created_at)
        .execute(&pool)
        .await
        .expect("insert mixed owner event");
    }

    for (event_id, user_id, event_type, metadata, created_at) in [
        (
            &owner_drift_assessment_event_id,
            &owner_user_id,
            astra_services::introspection::INTENT_DRIFT_ASSESSMENT_EVENT_TYPE,
            serde_json::json!({
                "schema_version": 1,
                "provenance": {
                    "kind": "llm_judge",
                    "invocation_id": "owner-drift-invocation",
                    "provider": "owner-provider",
                    "model": "owner-model",
                    "provider_response_id": "owner-response"
                },
                "verdict": "drifting",
                "score": 0.42,
                "level": "moderate",
                "evidence": ["owner-visible structured evidence"],
                "turn": 1,
                "round": 2
            }),
            "2026-06-01 10:04:00.000000",
        ),
        (
            &owner_legacy_drift_event_id,
            &owner_user_id,
            "drift_detected",
            serde_json::json!({
                "severity": 0.99,
                "evidence": ["ambiguous legacy evidence must not be projected"]
            }),
            "2026-06-01 10:06:00.000000",
        ),
        (
            &other_drift_assessment_event_id,
            &other_user_id,
            astra_services::introspection::INTENT_DRIFT_ASSESSMENT_EVENT_TYPE,
            serde_json::json!({
                "schema_version": 1,
                "provenance": {
                    "kind": "llm_judge",
                    "invocation_id": "other-drift-invocation",
                    "provider": "other-provider",
                    "model": "other-model",
                    "provider_response_id": "other-response"
                },
                "verdict": "drifting",
                "score": 0.99,
                "level": "high",
                "evidence": ["foreign evidence must not be projected"],
                "turn": 9,
                "round": 9
            }),
            "2026-06-01 10:05:00.000000",
        ),
    ] {
        sqlx::query(
            "INSERT INTO agent_events \
             (event_id, session_id, user_id, event_type, metadata, payload_hash, \
              ingestion_write_id, created_at) \
             VALUES (?, ?, ?, ?, CAST(? AS JSON), ?, ?, ?)",
        )
        .bind(event_id)
        .bind(&session_id)
        .bind(user_id)
        .bind(event_type)
        .bind(metadata.to_string())
        .bind(agent_event_fixture_payload_hash(serde_json::json!({
            "event_id": event_id,
            "session_id": &session_id,
            "user_id": user_id,
            "event_type": event_type,
            "metadata": &metadata,
            "created_at": created_at,
        })))
        .bind(Uuid::new_v4().to_string())
        .bind(created_at)
        .execute(&pool)
        .await
        .expect("insert mixed owner drift assessment");
    }

    let private_args = "PRIVATE-TOOL-ARGS";
    let private_result = "PRIVATE-TOOL-RESULT";
    let tool_metadata = serde_json::json!({
        "ok": true,
        "disposition": "executed",
        "result_class": "success",
        "exit_semantics": "success",
        "args_preview": private_args,
        "result_preview": private_result,
    });
    let tool_content = "command completed successfully";
    let tool_created_at = "2026-06-01 10:01:30.000000";
    sqlx::query(
        "INSERT INTO agent_events \
         (event_id, session_id, user_id, agent_id, event_type, content, run_id, parent_run_id, \
          tool_call_id, meta_tool_name, metadata, payload_hash, ingestion_write_id, created_at) \
         VALUES (?, ?, ?, ?, 'tool_call_completed', ?, ?, ?, ?, 'shell', CAST(? AS JSON), ?, ?, ?)",
    )
    .bind(&owner_tool_event_id)
    .bind(&session_id)
    .bind(&owner_user_id)
    .bind("child-agent")
    .bind(tool_content)
    .bind("child-run")
    .bind("root-run")
    .bind("child-shell-call")
    .bind(tool_metadata.to_string())
    .bind(agent_event_fixture_payload_hash(serde_json::json!({
        "event_id": &owner_tool_event_id,
        "session_id": &session_id,
        "user_id": &owner_user_id,
        "agent_id": "child-agent",
        "event_type": "tool_call_completed",
        "content": tool_content,
        "run_id": "child-run",
        "parent_run_id": "root-run",
        "tool_call_id": "child-shell-call",
        "meta_tool_name": "shell",
        "metadata": &tool_metadata,
        "created_at": tool_created_at,
    })))
    .bind(Uuid::new_v4().to_string())
    .bind(tool_created_at)
    .execute(&pool)
    .await
    .expect("insert scoped tool terminal fixture");

    let boundary_metadata = serde_json::json!({
        "span_id": "dependency-boundary",
        "name": "agent_dependency_boundary",
        "trace_id": "root-run",
        "attrs": {
            "parent_run_id": "root-run",
            "outcome": "results_adopted",
            "tool_call_id": "wait-for-child",
            "children": "[{\"agent_id\":\"child-agent\",\"run_id\":\"child-run\",\"status\":\"completed\"}]"
        }
    });
    let boundary_created_at = "2026-06-01 10:01:35.000000";
    sqlx::query(
        "INSERT INTO agent_events \
         (event_id, session_id, user_id, event_type, run_id, metadata, payload_hash, \
          ingestion_write_id, created_at) \
         VALUES (?, ?, ?, 'trace_span', ?, CAST(? AS JSON), ?, ?, ?)",
    )
    .bind(&owner_boundary_event_id)
    .bind(&session_id)
    .bind(&owner_user_id)
    .bind("root-run")
    .bind(boundary_metadata.to_string())
    .bind(agent_event_fixture_payload_hash(serde_json::json!({
        "event_id": &owner_boundary_event_id,
        "session_id": &session_id,
        "user_id": &owner_user_id,
        "event_type": "trace_span",
        "run_id": "root-run",
        "metadata": &boundary_metadata,
        "created_at": boundary_created_at,
    })))
    .bind(Uuid::new_v4().to_string())
    .bind(boundary_created_at)
    .execute(&pool)
    .await
    .expect("insert canonical dependency boundary fixture");

    let queries = common::SqlQueries::default();
    let reflect = DatabaseReflectService::new(settings.clone()).with_pool(shared.clone());
    let report = reflect
        .build_evidence(
            &owner_user_id,
            &session_id,
            astra_services::reflect::ReflectRequest::from_observation_params(
                Some("overview"),
                Some("overview"),
                None,
                None,
                10,
                "what happened?",
            ),
        )
        .with_subscriber(tracing_subscriber::registry().with(queries.clone()))
        .await
        .expect("owner reflect report");
    {
        let statements = queries.0.lock().unwrap();
        let delivery_queries = statements
            .iter()
            .filter(|sql| sql.contains("AS spawned"))
            .collect::<Vec<_>>();
        assert_eq!(delivery_queries.len(), 1);
        assert!(delivery_queries[0].contains("AS total_events"));
    }
    assert_eq!(report.data_coverage.events, 6);
    assert!(
        !report
            .data_coverage
            .providers
            .contains_key("decision_audits")
    );
    assert_eq!(report.model_requests.records_observed, 4);
    assert_eq!(report.model_requests.accepted_records_observed, 2);
    assert_eq!(
        report.model_requests.coverage,
        astra_services::reflect::ModelRequestCaptureCoverage::WindowObserved
    );
    let model_requests = report
        .model_requests
        .terminal
        .as_ref()
        .expect("owner terminal request evidence");
    assert_eq!(model_requests.terminal_requests, 2);
    assert_eq!(model_requests.terminal_statuses.succeeded, 1);
    assert_eq!(model_requests.terminal_statuses.delivery_unknown, 1);
    assert_eq!(model_requests.usage_coverage.exact, 1);
    assert_eq!(model_requests.usage_coverage.unavailable, 1);
    assert_eq!(model_requests.provider_response_id_observations, 1);
    let child_request = model_requests
        .groups
        .iter()
        .find(|group| group.run_id.as_deref() == Some("child-run"))
        .expect("child request group");
    assert_eq!(child_request.parent_run_id.as_deref(), Some("root-run"));
    assert_eq!(child_request.agent_id.as_deref(), Some("child-agent"));
    assert_eq!(child_request.terminal_statuses.delivery_unknown, 1);
    assert_eq!(child_request.provider_response_id_observations, 0);
    let view = report.view.as_ref().expect("reflect report includes view");
    assert_eq!(view.topic, "overview");
    assert_eq!(view.facet, "overview");
    assert_eq!(view.data_coverage.events, 6);

    assert!(!report.summary.is_empty());
    assert!(
        report.observations.iter().any(|observation| {
            EvidenceRef::parse(&observation.ref_id)
                .is_ok_and(|ref_id| ref_id.kind() == "observation" && ref_id.namespace() == "graph")
        }),
        "reflect report should include normalized observations"
    );
    assert!(
        report.evidence.iter().any(|evidence| {
            EvidenceRef::parse(&evidence.ref_id).is_ok_and(|ref_id| {
                matches!(ref_id.kind(), "decision" | "event")
                    && matches!(ref_id.namespace(), "cloud" | "edge" | "local")
            })
        }),
        "reflect report should include standardized evidence refs"
    );
    assert!(
        !report
            .evidence
            .iter()
            .any(|evidence| evidence.summary.contains("other secret"))
    );
    let execution_spine = report
        .graph_slice
        .nodes
        .iter()
        .find(|node| node.label == "execution_spine")
        .expect("scoped tool event is visible in execution trace");
    let facts = execution_spine.metadata.as_ref().unwrap()["execution_spine"]["facts"]
        .as_array()
        .expect("execution facts");
    let tool_fact = facts
        .iter()
        .find(|fact| fact["event_id"] == owner_tool_event_id)
        .expect("tool event fact");
    assert_eq!(tool_fact["run_id"], "child-run");
    assert_eq!(tool_fact["parent_run_id"], "root-run");
    assert_eq!(tool_fact["tool_outcome"]["tool_name"], "shell");
    assert_eq!(tool_fact["tool_outcome"]["disposition"], "executed");
    assert_eq!(tool_fact["tool_outcome"]["result_class"], "success");
    assert_eq!(tool_fact["tool_outcome"]["exit_semantics"], "success");
    let boundary_fact = facts
        .iter()
        .find(|fact| fact["event_id"] == owner_boundary_event_id)
        .expect("dependency boundary event fact");
    assert_eq!(boundary_fact["kind"], "agent_dependency_boundary");
    assert_eq!(boundary_fact["tool_call_id"], "wait-for-child");
    let report_json = serde_json::to_string(&report).unwrap();
    assert!(!report_json.contains(private_args));
    assert!(!report_json.contains(private_result));
    assert!(report.graph_slice.edges.is_empty());
    assert!(report.graph_slice.edges.is_empty());

    let mut delivery_events = Vec::new();
    for (user, kind, run) in [
        (&owner_user_id, "agent_spawned", "delivery-completed"),
        (&owner_user_id, "agent_spawned", "delivery-interrupted"),
        (&owner_user_id, "agent_spawned", "delivery-failed"),
        (&owner_user_id, "agent_spawned", "delivery-cancelled"),
        (&owner_user_id, "agent_completed", "delivery-completed"),
        (&owner_user_id, "agent_completed", "delivery-completed"),
        (&owner_user_id, "agent_interrupted", "delivery-interrupted"),
        (&owner_user_id, "agent_failed", "delivery-failed"),
        (&owner_user_id, "agent_cancelled", "delivery-cancelled"),
        (&other_user_id, "agent_spawned", "foreign-delivery"),
        (&other_user_id, "agent_completed", "foreign-delivery"),
    ] {
        let id = Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO agent_events (event_id, user_id, session_id, event_type, run_id, content, payload_hash, ingestion_write_id, created_at) VALUES (?, ?, ?, ?, ?, 'delivery fixture', ?, ?, '2026-05-31 09:00:00.000000')")
            .bind(&id).bind(user).bind(&session_id).bind(kind).bind(run)
            .bind(agent_event_fixture_payload_hash(serde_json::json!({"event_id":id})))
            .bind(Uuid::new_v4().to_string()).execute(&pool).await.unwrap();
        delivery_events.push((user.clone(), id));
    }
    // Exercise all delivery outcomes separately from the original evidence
    // projection: adding facts can legitimately change its bounded selection.
    let delivery_report = reflect
        .build_evidence(
            &owner_user_id,
            &session_id,
            astra_services::reflect::ReflectRequest::from_observation_params(
                Some("overview"),
                Some("overview"),
                None,
                None,
                10,
                "delivery outcomes?",
            ),
        )
        .await
        .unwrap();
    assert_eq!(delivery_report.data_coverage.events, 15);
    assert!(
        delivery_report
            .summary
            .contains("4 spawned, 1 completed, 1 interrupted, 1 failed, 1 cancelled")
    );

    let introspection =
        DatabaseIntrospectionService::new(settings.clone()).with_pool(shared.clone());
    let drift = introspection
        .get_drift_check(&owner_user_id, &session_id)
        .await
        .expect("owner drift check");
    assert_eq!(drift.schema_version, 2);
    assert_eq!(
        drift.assessment_status,
        IntentDriftAssessmentStatus::Assessed
    );
    assert_eq!(drift.verdict, Some(IntentDriftVerdict::Drifting));
    assert_eq!(drift.score, Some(0.42));
    assert_eq!(drift.level, Some(IntentDriftLevel::Moderate));
    assert_eq!(
        drift
            .provenance
            .as_ref()
            .expect("assessed drift must carry LLM provenance")
            .invocation_id,
        "owner-drift-invocation",
    );
    assert_eq!(
        drift.evidence,
        vec!["owner-visible structured evidence".to_string()]
    );
    assert_ne!(
        drift.source_event_id.as_deref(),
        Some(owner_legacy_drift_event_id.as_str())
    );
    assert_ne!(
        drift.source_event_id.as_deref(),
        Some(other_drift_assessment_event_id.as_str())
    );

    cleanup_agent_sessions_and_events_for_owner(
        &pool,
        &owner_user_id,
        std::slice::from_ref(&session_id),
        &[
            owner_query_event_id,
            owner_llm_event_id,
            owner_drift_assessment_event_id,
            owner_legacy_drift_event_id,
            owner_tool_event_id,
            owner_boundary_event_id,
        ],
    )
    .await;
    cleanup_agent_sessions_and_events_for_owner(
        &pool,
        &other_user_id,
        &[session_id],
        &[
            other_query_event_id,
            other_llm_event_id,
            other_drift_assessment_event_id,
        ],
    )
    .await;
    cleanup_model_request_fixtures(
        &pool,
        &owner_user_id,
        &[owner_root_attempt, owner_child_attempt],
    )
    .await;
    for (user, id) in delivery_events {
        sqlx::query("DELETE FROM agent_events WHERE user_id = ? AND event_id = ?")
            .bind(user)
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
    }
    cleanup_model_request_fixtures(&pool, &other_user_id, &[other_attempt]).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn session_delete_is_owner_scoped_and_preserves_foreign_rows_on_live_matrixone() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let owner_user_id = Uuid::new_v4().to_string();
    let other_user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();
    let owner_event_id = Uuid::new_v4().to_string();
    let foreign_event_id = Uuid::new_v4().to_string();
    let owner_run_id = Uuid::new_v4().to_string();
    let foreign_run_id = Uuid::new_v4().to_string();
    let owner_artifact_id = Uuid::new_v4().to_string();
    let foreign_artifact_id = Uuid::new_v4().to_string();
    let owner_workspace_id = format!("workspace-{}", Uuid::new_v4());
    let owner_workspace_without_debt_id = format!("workspace-{}", Uuid::new_v4());
    let foreign_workspace_id = format!("workspace-{}", Uuid::new_v4());
    let owner_cleanup_debt_id = format!("debt-{}", Uuid::new_v4());

    cleanup_session_delete_fixture_for_owner(&pool, &owner_user_id, &session_id).await;
    cleanup_session_delete_fixture_for_owner(&pool, &other_user_id, &session_id).await;

    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
         VALUES (?, ?, 'mixed-owner-delete-it', 'active', 1)",
    )
    .bind(&session_id)
    .bind(&owner_user_id)
    .execute(&pool)
    .await
    .expect("insert owner session");

    for (event_id, user_id, event_type) in [
        (&owner_event_id, &owner_user_id, "owner_evt"),
        (&foreign_event_id, &other_user_id, "foreign_evt"),
    ] {
        sqlx::query(
            "INSERT INTO agent_events (event_id, session_id, user_id, event_type, content, \
             causal_chain_id, payload_hash, ingestion_write_id) \
             VALUES (?, ?, ?, ?, '{}', '', ?, ?)",
        )
        .bind(event_id)
        .bind(&session_id)
        .bind(user_id)
        .bind(event_type)
        .bind(agent_event_fixture_payload_hash(serde_json::json!({
            "event_id": event_id,
            "session_id": &session_id,
            "user_id": user_id,
            "event_type": event_type,
            "content": "{}",
            "causal_chain_id": "",
        })))
        .bind(Uuid::new_v4().to_string())
        .execute(&pool)
        .await
        .expect("insert event");
    }
    for (event_id, user_id, marker) in [
        (&owner_event_id, &owner_user_id, "owner"),
        (&foreign_event_id, &other_user_id, "foreign"),
    ] {
        sqlx::query(
            "INSERT INTO agent_event_edges \
             (user_id, session_id, child_event_id, parent_event_id, relation_kind, parent_order) \
             VALUES (?, ?, ?, ?, 'causal', 0)",
        )
        .bind(user_id)
        .bind(&session_id)
        .bind(event_id)
        .bind(format!("parent-{marker}"))
        .execute(&pool)
        .await
        .expect("insert event edge");
    }

    for (user_id, run_id) in [
        (&owner_user_id, &owner_run_id),
        (&other_user_id, &foreign_run_id),
    ] {
        sqlx::query("INSERT INTO agent_runs (run_id, user_id, session_id, root_run_id, ancestor_path, status) VALUES (?, ?, ?, ?, '', 'running')")
            .bind(run_id).bind(user_id).bind(&session_id).bind(run_id).execute(&pool).await.unwrap();
    }

    for (user_id, run_id, artifact_id, marker) in [
        (&owner_user_id, &owner_run_id, &owner_artifact_id, "owner"),
        (
            &other_user_id,
            &foreign_run_id,
            &foreign_artifact_id,
            "foreign",
        ),
    ] {
        sqlx::query(
            "INSERT INTO session_artifacts \
             (artifact_id, session_id, user_id, owner_run_id, root_run_id, artifact_kind, source, content_json, metadata) \
             VALUES (?, ?, ?, ?, ?, 'delete_fixture', 'session_delete_it', ?, CAST(? AS JSON))",
        )
        .bind(artifact_id)
        .bind(&session_id)
        .bind(user_id)
        .bind(run_id)
        .bind(run_id)
        .bind(format!("{{\"marker\":\"{marker}\"}}"))
        .bind(format!("{{\"marker\":\"{marker}\"}}"))
        .execute(&pool)
        .await
        .expect("insert session artifact");
    }

    for (user_id, workspace_id, run_id, marker) in [
        (&owner_user_id, &owner_workspace_id, &owner_run_id, "owner"),
        (
            &owner_user_id,
            &owner_workspace_without_debt_id,
            &owner_run_id,
            "owner-without-debt",
        ),
        (
            &other_user_id,
            &foreign_workspace_id,
            &foreign_run_id,
            "foreign",
        ),
    ] {
        let root_or_volume_ref = format!("/tmp/workspace-{marker}");
        let source_json = serde_json::json!({"kind": "scratch"}).to_string();
        let record_json = serde_json::json!({
            "workspace_id": workspace_id,
            "owner_scope": "tenant",
            "kind": "cloud_workspace",
            "authority": "read_write",
            "root_or_volume_ref": root_or_volume_ref.clone(),
            "source": {"kind": "scratch"},
            "persistence": "session",
            "revision": "rev-1",
            "display_name": format!("Workspace {marker}"),
        })
        .to_string();
        sqlx::query(
            "INSERT INTO workspace_records \
             (workspace_id, owner_id, session_id, run_id, kind, authority, persistence, \
              root_or_volume_ref, source_json, revision, display_name, source_key, record_json) \
             VALUES (?, ?, ?, ?, 'cloud_workspace', 'read_write', 'session', ?, ?, 'rev-1', ?, NULL, ?)",
        )
        .bind(workspace_id)
        .bind(user_id)
        .bind(&session_id)
        .bind(run_id)
        .bind(root_or_volume_ref)
        .bind(source_json)
        .bind(format!("Workspace {marker}"))
        .bind(record_json)
        .execute(&pool)
        .await
        .expect("insert workspace record");
    }

    let owner_cleanup_debt_record_json = serde_json::json!({
        "workspace_id": owner_workspace_id.clone(),
        "owner_scope": "tenant",
        "kind": "cloud_workspace",
        "authority": "read_write",
        "root_or_volume_ref": "/tmp/workspace-owner",
        "source": {"kind": "scratch"},
        "persistence": "session",
        "revision": "rev-1",
        "display_name": "Workspace owner",
    })
    .to_string();
    sqlx::query(
        "INSERT INTO workspace_cleanup_debts \
         (debt_id, owner_id, session_id, run_id, workspace_id, reason, message, attempts, record_json) \
         VALUES (?, ?, ?, ?, ?, 'failed', 'cleanup still pending', 0, ?)",
    )
    .bind(&owner_cleanup_debt_id)
    .bind(&owner_user_id)
    .bind(&session_id)
    .bind(&owner_run_id)
    .bind(&owner_workspace_id)
    .bind(owner_cleanup_debt_record_json)
    .execute(&pool)
    .await
    .expect("insert unresolved cleanup debt");

    let event_service = DatabaseEventService::new(settings.clone()).with_pool(shared.clone());
    let retained_request = EventCreateRequestData {
        ingestion_source: astra_services::EventIngestionSource::Client,
        event_id: None,
        session_id: session_id.clone(),
        event_type: "retained_delete_fixture".into(),
        content: "{}".into(),
        agent_id: None,
        agent_version: None,
        parent_event_id: None,
        parent_event_ids: None,
        causal_chain_id: None,
        metadata: None,
    };
    event_service
        .create_event(owner_user_id.clone(), retained_request.clone())
        .await
        .unwrap();
    let session_service = DatabaseSessionService::new(settings).with_pool(shared);
    session_service
        .delete_session(session_id.clone(), owner_user_id.clone())
        .await
        .unwrap();
    let error = event_service
        .create_event(owner_user_id.clone(), retained_request)
        .await
        .unwrap_err();
    assert_eq!(error.0, axum::http::StatusCode::NOT_FOUND);

    let delete_audit = load_session_delete_audit_details(&pool, &owner_user_id, &session_id).await;
    assert_eq!(
        delete_audit
            .get("workspace_cleanup_debts_enqueued")
            .and_then(serde_json::Value::as_u64),
        Some(1),
        "session delete audit must report live workspace cleanup debt enqueue"
    );
    assert!(
        delete_audit
            .get("database_rows_deleted")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|rows| rows > 0),
        "session delete audit must report a positive live DB delete count: {delete_audit}"
    );
    assert_eq!(
        deleted_rows_for_table(&delete_audit, "agent_event_edges"),
        1
    );
    assert_eq!(deleted_rows_for_table(&delete_audit, "agent_events"), 2);
    assert_eq!(deleted_rows_for_table(&delete_audit, "agent_sessions"), 1);
    assert_eq!(
        deleted_rows_for_table(&delete_audit, "session_artifacts"),
        1
    );
    assert_eq!(
        deleted_rows_for_table(&delete_audit, "workspace_records"),
        2
    );

    for (label, table) in [
        ("agent_sessions", "agent_sessions"),
        ("agent_events", "agent_events"),
        ("session_artifacts", "session_artifacts"),
    ] {
        let owner_remaining = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT COUNT(*) AS c FROM {table} WHERE session_id = ? AND user_id = ?"
        )))
        .bind(&session_id)
        .bind(&owner_user_id)
        .fetch_one(&pool)
        .await
        .unwrap_or_else(|error| panic!("count owner {label}: {error}"))
        .try_get::<i64, _>("c")
        .expect("decode owner row count");
        assert_eq!(owner_remaining, 0, "{label} owner rows must be deleted");

        let foreign_remaining = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT COUNT(*) AS c FROM {table} WHERE session_id = ? AND user_id = ?"
        )))
        .bind(&session_id)
        .bind(&other_user_id)
        .fetch_one(&pool)
        .await
        .unwrap_or_else(|error| panic!("count foreign {label}: {error}"))
        .try_get::<i64, _>("c")
        .expect("decode foreign row count");
        let expected_foreign = if table == "agent_sessions" { 0 } else { 1 };
        assert_eq!(
            foreign_remaining, expected_foreign,
            "{label} foreign rows must not be touched by owner delete"
        );
    }

    let owner_edge_remaining = sqlx::query(
        "SELECT COUNT(*) AS c FROM agent_event_edges WHERE user_id = ? AND child_event_id = ?",
    )
    .bind(&owner_user_id)
    .bind(&owner_event_id)
    .fetch_one(&pool)
    .await
    .expect("count owner event edge")
    .try_get::<i64, _>("c")
    .expect("decode owner event edge count");
    assert_eq!(
        owner_edge_remaining, 0,
        "owner event edge must be deleted through owner/session edge scope"
    );

    let foreign_edge_remaining = sqlx::query(
        "SELECT COUNT(*) AS c FROM agent_event_edges WHERE user_id = ? AND child_event_id = ?",
    )
    .bind(&other_user_id)
    .bind(&foreign_event_id)
    .fetch_one(&pool)
    .await
    .expect("count foreign event edge")
    .try_get::<i64, _>("c")
    .expect("decode foreign event edge count");
    assert_eq!(
        foreign_edge_remaining, 1,
        "foreign event edge must not be touched by owner delete"
    );

    let owner_workspace_remaining = sqlx::query(
        "SELECT COUNT(*) AS c FROM workspace_records WHERE session_id = ? AND owner_id = ?",
    )
    .bind(&session_id)
    .bind(&owner_user_id)
    .fetch_one(&pool)
    .await
    .expect("count owner workspace records")
    .try_get::<i64, _>("c")
    .expect("decode owner workspace record count");
    assert_eq!(
        owner_workspace_remaining, 0,
        "owner workspace records must be removed by hard delete"
    );

    let foreign_workspace_remaining = sqlx::query(
        "SELECT COUNT(*) AS c FROM workspace_records WHERE session_id = ? AND owner_id = ?",
    )
    .bind(&session_id)
    .bind(&other_user_id)
    .fetch_one(&pool)
    .await
    .expect("count foreign workspace records")
    .try_get::<i64, _>("c")
    .expect("decode foreign workspace record count");
    assert_eq!(
        foreign_workspace_remaining, 1,
        "foreign workspace records must not be touched by owner delete"
    );

    let owner_cleanup_debt_remaining = sqlx::query(
        "SELECT COUNT(*) AS c FROM workspace_cleanup_debts WHERE debt_id = ? AND owner_id = ?",
    )
    .bind(&owner_cleanup_debt_id)
    .bind(&owner_user_id)
    .fetch_one(&pool)
    .await
    .expect("count unresolved workspace cleanup debt")
    .try_get::<i64, _>("c")
    .expect("decode cleanup debt count");
    assert_eq!(
        owner_cleanup_debt_remaining, 1,
        "existing unresolved workspace cleanup debt must survive without duplication"
    );

    let generated_cleanup_debt_remaining = sqlx::query(
        "SELECT COUNT(*) AS c FROM workspace_cleanup_debts WHERE workspace_id = ? AND owner_id = ? AND reason = 'operator_requested'",
    )
    .bind(&owner_workspace_without_debt_id)
    .bind(&owner_user_id)
    .fetch_one(&pool)
    .await
    .expect("count generated workspace cleanup debt")
    .try_get::<i64, _>("c")
    .expect("decode generated cleanup debt count");
    assert_eq!(
        generated_cleanup_debt_remaining, 1,
        "session delete must enqueue cleanup debt before deleting cloud workspace records"
    );

    cleanup_session_delete_fixture_for_owner(&pool, &other_user_id, &session_id).await;
    cleanup_session_delete_fixture_for_owner(&pool, &owner_user_id, &session_id).await;
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn session_delete_removes_owner_scoped_database_rows_and_local_files_on_live_matrixone() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let owner_user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();

    cleanup_session_delete_fixture_for_owner(&pool, &owner_user_id, &session_id).await;

    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
         VALUES (?, ?, 'owner-delete-it', 'active', 0)",
    )
    .bind(&session_id)
    .bind(&owner_user_id)
    .execute(&pool)
    .await
    .expect("insert owner session");
    let temp_sessions = tempfile::tempdir().expect("temp sessions dir");
    let _journal_guard =
        astra_services::session_journal::JournalDirGuard::new(temp_sessions.path());
    let (owner_journal_path, owner_session_dir) =
        create_owner_local_session_files(&owner_user_id, &session_id);

    let session_service = DatabaseSessionService::new(settings).with_pool(shared);
    session_service
        .delete_session(session_id.clone(), owner_user_id.clone())
        .await
        .expect("owner-only session delete");

    assert!(
        !owner_journal_path.exists(),
        "session service delete must remove owner journal file"
    );
    assert!(
        !owner_session_dir.exists(),
        "session service delete must remove owner checkpoint/artifact directory"
    );

    for label in ["agent_sessions", "agent_events"] {
        let remaining = count_user_session_rows(&pool, label, &owner_user_id, &session_id).await;
        assert_eq!(remaining, 0, "{label} must be removed by hard delete");
    }
    let completed_fences: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_session_lifecycle_fences
         WHERE user_id = ? AND session_id = ?
           AND delete_requested_at IS NOT NULL
           AND database_deleted_at IS NOT NULL",
    )
    .bind(&owner_user_id)
    .bind(&session_id)
    .fetch_one(&pool)
    .await
    .expect("count durable session deletion fences");
    assert_eq!(
        completed_fences, 1,
        "hard delete must retain its late-writer fence"
    );
    sqlx::query("DELETE FROM agent_session_lifecycle_fences WHERE user_id = ? AND session_id = ?")
        .bind(&owner_user_id)
        .bind(&session_id)
        .execute(&pool)
        .await
        .expect("clean deletion fence fixture");
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn session_delete_rejects_non_owner_without_removing_local_files_on_live_matrixone() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let owner_user_id = Uuid::new_v4().to_string();
    let other_user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();

    cleanup_session_delete_fixture_for_owner(&pool, &owner_user_id, &session_id).await;

    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
         VALUES (?, ?, 'non-owner-delete-it', 'active', 0)",
    )
    .bind(&session_id)
    .bind(&owner_user_id)
    .execute(&pool)
    .await
    .expect("insert owner session");

    let temp_sessions = tempfile::tempdir().expect("temp sessions dir");
    let _journal_guard =
        astra_services::session_journal::JournalDirGuard::new(temp_sessions.path());
    let (owner_journal_path, owner_session_dir) =
        create_owner_local_session_files(&owner_user_id, &session_id);

    let session_service = DatabaseSessionService::new(settings).with_pool(shared);
    let err = session_service
        .delete_session(session_id.clone(), other_user_id)
        .await
        .expect_err("non-owner delete must fail before file deletion");
    assert_eq!(err.0, axum::http::StatusCode::NOT_FOUND);

    let remaining = sqlx::query(
        "SELECT COUNT(*) AS c FROM agent_sessions WHERE session_id = ? AND user_id = ?",
    )
    .bind(&session_id)
    .bind(&owner_user_id)
    .fetch_one(&pool)
    .await
    .expect("count owner session")
    .try_get::<i64, _>("c")
    .expect("decode owner session count");
    assert_eq!(remaining, 1, "non-owner delete must not remove DB rows");
    assert!(
        owner_journal_path.exists(),
        "non-owner delete must not remove owner journal"
    );
    assert!(
        owner_session_dir.exists(),
        "non-owner delete must not remove owner checkpoint/artifact directory"
    );

    cleanup_session_delete_fixture_for_owner(&pool, &owner_user_id, &session_id).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn sync_outbox_create_events_batch_contract_live_matrixone() {
    let (_bootstrap, settings) = setup_pool_and_settings().await;
    let shared = SharedPool::new(&settings).await.unwrap();
    let pool = shared.get().clone();
    let owner = Uuid::new_v4().to_string();
    let session = Uuid::new_v4().to_string();
    let service = DatabaseEventService::new(settings).with_pool(shared);
    let make_request = |sequence: u32| {
        let journal_event = astra_services::session_journal::JournalEvent::turn(
            Some(&session),
            sequence + 1,
            Some("sync-batch-test"),
            "question",
            "answer",
            0,
            10,
            5,
            1,
        );
        let payload = serde_json::to_value(&journal_event).unwrap();
        let payload_hash = astra_services::sync_outbox_canonical_payload_hash(&payload);
        let event_id =
            astra_services::sync_outbox_stable_event_id(&journal_event, &payload_hash).unwrap();
        EventCreateRequestData {
            ingestion_source: astra_services::events::EventIngestionSource::SyncOutbox,
            event_id: Some(event_id),
            session_id: session.clone(),
            event_type: "turn".into(),
            content: payload.to_string(),
            agent_id: None,
            agent_version: None,
            parent_event_id: None,
            parent_event_ids: None,
            causal_chain_id: None,
            metadata: Some(serde_json::json!({
                "sync_outbox": {"payload_hash": payload_hash}
            })),
        }
    };
    fn accepted(
        entries: Vec<astra_services::events::EventBatchEntry>,
    ) -> Vec<astra_services::events::EventCreateOutcome> {
        entries
            .into_iter()
            .map(|entry| match entry {
                astra_services::events::EventBatchEntry::Accepted(outcome) => outcome,
                astra_services::events::EventBatchEntry::Rejected(rejection) => {
                    panic!("unexpected record rejection: {rejection:?}")
                }
            })
            .collect()
    }
    let requests = (0..3).map(&make_request).collect::<Vec<_>>();

    assert!(
        service
            .create_events(owner.clone(), Vec::new())
            .await
            .unwrap()
            .is_empty()
    );
    let created = accepted(
        service
            .create_events(owner.clone(), requests.clone())
            .await
            .unwrap(),
    );
    assert_eq!(created.len(), 3);
    assert!(created.iter().all(|outcome| !outcome.idempotent_replay));
    assert_eq!(
        created
            .iter()
            .map(|outcome| outcome.record.content.as_str())
            .collect::<Vec<_>>(),
        requests
            .iter()
            .map(|request| request.content.as_str())
            .collect::<Vec<_>>()
    );

    let replayed = accepted(
        service
            .create_events(owner.clone(), requests.clone())
            .await
            .unwrap(),
    );
    assert!(replayed.iter().all(|outcome| outcome.idempotent_replay));
    assert_eq!(
        replayed
            .iter()
            .map(|outcome| &outcome.record)
            .collect::<Vec<_>>(),
        created
            .iter()
            .map(|outcome| &outcome.record)
            .collect::<Vec<_>>()
    );
    let event_ids = requests
        .iter()
        .map(|request| request.event_id.clone().unwrap())
        .chain(Some("missing-sync-event".to_string()))
        .collect::<Vec<_>>();
    let reconciled = service
        .get_sync_outbox_events(owner.clone(), event_ids.clone())
        .await
        .unwrap();
    assert_eq!(reconciled.len(), requests.len());
    for request in &requests {
        let event_id = request.event_id.as_deref().unwrap();
        let record = reconciled
            .iter()
            .find(|record| record.event_id == event_id)
            .expect("stored sync event");
        let expected_hash = request
            .metadata
            .as_ref()
            .and_then(astra_services::events::sync_outbox_payload_hash)
            .unwrap();
        assert_eq!(record.payload_hash, expected_hash);
    }
    assert!(
        service
            .get_sync_outbox_events(Uuid::new_v4().to_string(), event_ids)
            .await
            .unwrap()
            .is_empty(),
        "batch reconciliation must remain tenant-scoped"
    );

    let same_batch_duplicate = make_request(3);
    let duplicate_outcomes = accepted(
        service
            .create_events(
                owner.clone(),
                vec![same_batch_duplicate.clone(), same_batch_duplicate.clone()],
            )
            .await
            .unwrap(),
    );
    assert_eq!(duplicate_outcomes.len(), 2);
    assert!(!duplicate_outcomes[0].idempotent_replay);
    assert!(duplicate_outcomes[1].idempotent_replay);
    assert_eq!(
        duplicate_outcomes[0].record.event_id,
        duplicate_outcomes[1].record.event_id
    );

    let valid = make_request(4);
    let invalid = EventCreateRequestData {
        content: "not-json".into(),
        ..make_request(5)
    };
    let entries = service
        .create_events(owner.clone(), vec![valid, invalid])
        .await
        .unwrap();
    assert_eq!(entries.len(), 2);
    assert!(matches!(
        entries[0],
        astra_services::events::EventBatchEntry::Accepted(_)
    ));
    match &entries[1] {
        astra_services::events::EventBatchEntry::Rejected(rejection) => {
            assert_eq!(rejection.status.as_u16(), 400);
            assert_eq!(
                rejection.error_code,
                astra_sync_protocol::SYNC_OUTBOX_INVALID_RECORD_ERROR_CODE
            );
        }
        other => panic!("invalid record must be rejected, got {other:?}"),
    }

    let event_count =
        sqlx::query("SELECT COUNT(*) AS c FROM agent_events WHERE user_id = ? AND session_id = ?")
            .bind(&owner)
            .bind(&session)
            .fetch_one(&pool)
            .await
            .unwrap()
            .try_get::<i64, _>("c")
            .unwrap();
    assert_eq!(
        event_count, 5,
        "a rejected record must not roll back committed siblings"
    );
    let session_event_count =
        sqlx::query("SELECT event_count FROM agent_sessions WHERE user_id = ? AND session_id = ?")
            .bind(&owner)
            .bind(&session)
            .fetch_one(&pool)
            .await
            .unwrap()
            .try_get::<i64, _>("event_count")
            .unwrap();
    assert_eq!(
        session_event_count, 5,
        "rejections and replays must not change the committed count"
    );

    let conflict = make_request(6);
    let stored_content = serde_json::json!({"stored": "client"}).to_string();
    let stored_conflict = EventCreateRequestData {
        ingestion_source: astra_services::events::EventIngestionSource::Client,
        event_id: conflict.event_id.clone(),
        session_id: session.clone(),
        event_type: "client_marker".into(),
        content: stored_content.clone(),
        agent_id: None,
        agent_version: None,
        parent_event_id: None,
        parent_event_ids: None,
        causal_chain_id: None,
        metadata: None,
    };
    service
        .create_event(owner.clone(), stored_conflict)
        .await
        .unwrap();
    let prefix = make_request(7);
    let later = make_request(8);
    let entries = service
        .create_events(
            owner.clone(),
            vec![prefix.clone(), conflict.clone(), later.clone()],
        )
        .await
        .unwrap();
    assert_eq!(entries.len(), 3);
    assert!(matches!(
        entries[0],
        astra_services::events::EventBatchEntry::Accepted(_)
    ));
    match &entries[1] {
        astra_services::events::EventBatchEntry::Rejected(rejection) => {
            assert_eq!(rejection.status.as_u16(), 409);
            assert_eq!(
                rejection.error_code,
                astra_sync_protocol::SYNC_OUTBOX_RECORD_CONFLICT_ERROR_CODE
            );
        }
        other => panic!("conflicting record must be rejected, got {other:?}"),
    }
    assert!(
        matches!(
            entries[2],
            astra_services::events::EventBatchEntry::Accepted(_)
        ),
        "a rejected record must not block later siblings"
    );

    let rows = sqlx::query(
        "SELECT event_id, content FROM agent_events WHERE user_id = ? AND session_id = ? AND event_id IN (?, ?, ?)",
    )
    .bind(&owner)
    .bind(&session)
    .bind(prefix.event_id.as_ref().unwrap())
    .bind(conflict.event_id.as_ref().unwrap())
    .bind(later.event_id.as_ref().unwrap())
    .fetch_all(&pool)
    .await
    .unwrap();
    let visible_ids = rows
        .iter()
        .map(|row| row.try_get::<String, _>("event_id").unwrap())
        .collect::<Vec<_>>();
    assert!(visible_ids.contains(prefix.event_id.as_ref().unwrap()));
    assert!(visible_ids.contains(conflict.event_id.as_ref().unwrap()));
    assert!(
        visible_ids.contains(later.event_id.as_ref().unwrap()),
        "a rejected record must not suppress later siblings"
    );
    let stored_row = rows
        .iter()
        .find(|row| {
            row.try_get::<String, _>("event_id").unwrap() == *conflict.event_id.as_ref().unwrap()
        })
        .unwrap();
    assert_eq!(
        stored_row.try_get::<String, _>("content").unwrap(),
        stored_content,
        "conflict must not overwrite the stored event"
    );
    let session_event_count =
        sqlx::query("SELECT event_count FROM agent_sessions WHERE user_id = ? AND session_id = ?")
            .bind(&owner)
            .bind(&session)
            .fetch_one(&pool)
            .await
            .unwrap()
            .try_get::<i64, _>("event_count")
            .unwrap();
    assert_eq!(
        session_event_count, 7,
        "a hash conflict rejects one record and commits its siblings"
    );

    let mixed_insert = make_request(9);
    let mixed = accepted(
        service
            .create_events(
                owner.clone(),
                vec![requests[0].clone(), mixed_insert.clone()],
            )
            .await
            .unwrap(),
    );
    assert!(mixed[0].idempotent_replay);
    assert!(!mixed[1].idempotent_replay);
    let summary = sqlx::query(
        "SELECT event_count, last_event_id FROM agent_sessions WHERE user_id = ? AND session_id = ?",
    )
    .bind(&owner)
    .bind(&session)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(summary.try_get::<i64, _>("event_count").unwrap(), 8);
    assert_eq!(
        summary
            .try_get::<Option<String>, _>("last_event_id")
            .unwrap(),
        mixed_insert.event_id,
        "a replay followed by an insert repairs once and keeps the final insert as the summary pointer"
    );

    cleanup_session_delete_fixture_for_owner(&pool, &owner, &session).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn sync_outbox_projects_verified_journal_columns_without_changing_identity() {
    use astra_services::session_journal::{JournalEvent, JournalProducerScope};
    let (_bootstrap, mut settings) = setup_pool_and_settings().await;
    settings.db_pool_max_connections = 1;
    settings.db_pool_min_connections = 1;
    let shared = SharedPool::new(&settings).await.unwrap();
    let pool = shared.get().clone();
    let owner = Uuid::new_v4().to_string();
    let session = Uuid::new_v4().to_string();
    let service = DatabaseEventService::new(settings.clone()).with_pool(shared.clone());
    let root = JournalEvent::turn(
        Some(&session),
        4,
        Some("projection-model"),
        "question",
        "answer",
        0,
        10,
        5,
        1,
    )
    .with_cache_tokens(3, 2)
    .with_run_id(Some("local-root"));
    let mut root = root;
    root.tool_calls = Some(vec![
        serde_json::from_value(
            serde_json::json!({"name":"bash","ok":true,"ms":15,"tool_call_id":"cli-call"}),
        )
        .unwrap(),
    ]);
    let mut ids = Vec::new();
    for case in 0..3 {
        let mut event = root.clone();
        if case == 1 {
            event.producer_scope = Some(JournalProducerScope {
                run_id: "local-child".into(),
                parent_run_id: Some("local-root".into()),
                agent_id: Some("agent".into()),
                local_turn: Some(99),
            });
        } else if case == 2 {
            event.tokens_in = None;
            event.tokens_out = None;
            event.cache_read_tokens = None;
            event.cache_creation_tokens = None;
            event.metadata.as_mut().unwrap()["qualified_usage"] = serde_json::json!({});
        }
        let payload = serde_json::to_value(&event).unwrap();
        let hash = astra_services::sync_outbox_canonical_payload_hash(&payload);
        let id = astra_services::sync_outbox_stable_event_id(&event, &hash).unwrap();
        let request = EventCreateRequestData {
            ingestion_source: astra_services::events::EventIngestionSource::SyncOutbox,
            event_id: Some(id.clone()),
            session_id: session.clone(),
            event_type: "turn".into(),
            content: payload.to_string(),
            agent_id: None,
            agent_version: None,
            parent_event_id: None,
            parent_event_ids: None,
            causal_chain_id: None,
            metadata: Some(serde_json::json!({"sync_outbox":{"payload_hash":hash}})),
        };
        let created = service
            .create_event(owner.clone(), request.clone())
            .await
            .unwrap();
        assert!(!created.idempotent_replay);
        assert_eq!(created.record.content, request.content);
        assert_eq!(
            created.record.metadata,
            request.metadata.as_ref().unwrap().clone()
        );
        let replay = service
            .create_event(
                owner.clone(),
                EventCreateRequestData {
                    metadata: Some(created.record.metadata.clone()),
                    ..request
                },
            )
            .await
            .unwrap();
        assert!(replay.idempotent_replay);
        assert_eq!(replay.record, created.record);
        let row = sqlx::query("SELECT run_id, parent_run_id, turn_seq, llm_model_used, token_input, token_output, token_total, CAST(token_usage AS CHAR) AS usage_json FROM agent_events WHERE user_id = ? AND event_id = ?")
            .bind(&owner).bind(&id).fetch_one(&pool).await.unwrap();
        assert_eq!(
            row.try_get::<String, _>("run_id").unwrap(),
            if case == 1 {
                "local-child"
            } else {
                "local-root"
            }
        );
        assert_eq!(
            row.try_get::<Option<String>, _>("parent_run_id")
                .unwrap()
                .as_deref(),
            if case == 1 { Some("local-root") } else { None }
        );
        assert_eq!(
            row.try_get::<Option<i64>, _>("turn_seq").unwrap(),
            if case == 1 { None } else { Some(4) }
        );
        assert_eq!(
            row.try_get::<String, _>("llm_model_used").unwrap(),
            "projection-model"
        );
        let expected = if case == 2 {
            (None, None, None)
        } else {
            (Some(15), Some(5), Some(20))
        };
        assert_eq!(
            (
                row.try_get::<Option<i64>, _>("token_input").unwrap(),
                row.try_get::<Option<i64>, _>("token_output").unwrap(),
                row.try_get::<Option<i64>, _>("token_total").unwrap()
            ),
            expected
        );
        let usage: serde_json::Value =
            serde_json::from_str(&row.try_get::<String, _>("usage_json").unwrap()).unwrap();
        if case == 2 {
            assert_eq!(usage, serde_json::json!({}));
        }
        ids.push(id);
    }
    let count: i64 = sqlx::query_scalar(
        "SELECT event_count FROM agent_sessions WHERE user_id = ? AND session_id = ?",
    )
    .bind(&owner)
    .bind(&session)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 3, "projection must not add rows on exact replay");
    let audit = DatabaseSessionAuditService::new(settings).with_pool(shared);
    let summary = audit.get_summary(&owner, &session).await.unwrap();
    assert_eq!(
        summary.turn_count, 4,
        "real CLI turns retain their high watermark and exclude child-local turns"
    );
    assert_eq!((summary.tokens_in, summary.tokens_out), (30, 10));
    assert_eq!(summary.models_used, vec!["projection-model"]);
    let stats = audit
        .get_cross_session_stats(
            &owner,
            &CrossSessionStatsParams {
                since: None,
                until: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(stats.total_turns, 4);
    assert_eq!((stats.total_tokens_in, stats.total_tokens_out), (30, 10));
    assert_eq!(stats.top_models[0].model, "projection-model");
    assert_eq!(stats.top_models[0].total_tokens, 40);
    let params = TurnListParams {
        page: 1,
        per_page: 1,
        after_created_at: None,
        after_event_id: None,
    };
    let first = audit.list_turns(&owner, &session, &params).await.unwrap();
    assert_eq!(first.total, 4);
    assert_eq!(first.turns.len(), 1);
    assert_eq!(first.turns[0].user_input_preview, "question");
    assert_eq!(first.turns[0].tool_calls[0].name, "bash");
    assert_eq!(first.turns[0].tool_calls[0].duration_ms, 15);
    let detail = audit.get_turn_detail(&owner, &session, 4).await.unwrap();
    assert_eq!(detail.user_input, "question");
    assert_eq!(detail.assistant_output, "answer");
    assert_eq!(detail.tool_calls.len(), 1);
    assert_eq!(detail.duration_ms, 1);
    assert!(!detail.has_error);
    let cursor = first.next_cursor.unwrap();
    let sentinel = [&ids[0], &ids[2]]
        .into_iter()
        .find(|id| **id != cursor.event_id)
        .unwrap();
    let sentinel_content: String =
        sqlx::query_scalar("SELECT content FROM agent_events WHERE user_id = ? AND event_id = ?")
            .bind(&owner)
            .bind(sentinel)
            .fetch_one(&pool)
            .await
            .unwrap();
    sqlx::query("UPDATE agent_events SET content = '{invalid' WHERE user_id = ? AND event_id = ?")
        .bind(&owner)
        .bind(sentinel)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        audit.list_turns(&owner, &session, &params).await.is_ok(),
        "unselected sentinel JSON must not be decoded"
    );
    let next = TurnListParams {
        after_created_at: Some(cursor.created_at.clone()),
        after_event_id: Some(cursor.event_id.clone()),
        ..params.clone()
    };
    assert!(audit.list_turns(&owner, &session, &next).await.is_err());
    sqlx::query("UPDATE agent_events SET content = ? WHERE user_id = ? AND event_id = ?")
        .bind(sentinel_content)
        .bind(&owner)
        .bind(sentinel)
        .execute(&pool)
        .await
        .unwrap();
    let original: (String,String) = sqlx::query_as("SELECT CAST(token_usage AS CHAR),CAST(metadata AS CHAR) FROM agent_events WHERE user_id = ? AND event_id = ?")
        .bind(&owner).bind(sentinel).fetch_one(&pool).await.unwrap();
    for (usage, metadata) in [
        (
            serde_json::json!({"input_tokens":"invalid"}).to_string(),
            original.1.clone(),
        ),
        (original.0.clone(), "[]".into()),
    ] {
        sqlx::query("UPDATE agent_events SET token_usage = CAST(? AS JSON), metadata = CAST(? AS JSON) WHERE user_id = ? AND event_id = ?")
            .bind(usage).bind(metadata).bind(&owner).bind(sentinel).execute(&pool).await.unwrap();
        assert!(
            audit.list_turns(&owner, &session, &params).await.is_ok(),
            "unused sentinel fields are not decoded"
        );
    }
    sqlx::query("UPDATE agent_events SET token_usage = CAST(? AS JSON),metadata = CAST(? AS JSON) WHERE user_id = ? AND event_id = ?")
        .bind(original.0).bind(original.1).bind(&owner).bind(sentinel).execute(&pool).await.unwrap();
    let selected_content: String =
        sqlx::query_scalar("SELECT content FROM agent_events WHERE user_id = ? AND event_id = ?")
            .bind(&owner)
            .bind(&cursor.event_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let mut changed: serde_json::Value = serde_json::from_str(&selected_content).unwrap();
    changed["user_input"] = "tampered".into();
    for corrupt in ["{invalid".to_string(), changed.to_string()] {
        sqlx::query("UPDATE agent_events SET content = ? WHERE user_id = ? AND event_id = ?")
            .bind(corrupt)
            .bind(&owner)
            .bind(&cursor.event_id)
            .execute(&pool)
            .await
            .unwrap();
        assert!(audit.list_turns(&owner, &session, &params).await.is_err());
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT 1")
                .fetch_one(&pool)
                .await
                .unwrap(),
            1
        );
    }
    sqlx::query("UPDATE agent_events SET content = ? WHERE user_id = ? AND event_id = ?")
        .bind(selected_content)
        .bind(&owner)
        .bind(&cursor.event_id)
        .execute(&pool)
        .await
        .unwrap();
    let ordinary = service
        .create_event(
            owner.clone(),
            EventCreateRequestData {
                ingestion_source: astra_services::events::EventIngestionSource::Client,
                event_id: None,
                session_id: session.clone(),
                event_type: "user_query".into(),
                content: "ordinary input".into(),
                agent_id: None,
                agent_version: None,
                parent_event_id: None,
                parent_event_ids: None,
                causal_chain_id: None,
                metadata: Some(serde_json::json!({"sync_outbox":{},"turn":1,"tool_calls":[{"name":"bash","ok":true,"ms":123}]})),
            },
        )
        .await
        .unwrap()
        .record
        .event_id;
    for at in ["2000-01-01 00:00:00", "2099-01-01 00:00:00"] {
        sqlx::query("UPDATE agent_events SET created_at = ? WHERE user_id = ? AND event_id = ?")
            .bind(at)
            .bind(&owner)
            .bind(&ordinary)
            .execute(&pool)
            .await
            .unwrap();
        let mut cursor_params = params.clone();
        let mut collected = Vec::new();
        loop {
            let page = audit
                .list_turns(&owner, &session, &cursor_params)
                .await
                .unwrap();
            assert_eq!(page.total, 5);
            for item in page.turns {
                let detail = audit
                    .get_turn_detail(&owner, &session, item.turn)
                    .await
                    .unwrap();
                assert_eq!(detail.user_input, item.user_input_preview);
                if item.user_input_preview == "ordinary input" {
                    assert_eq!(item.tool_calls[0].duration_ms, 123);
                    assert_eq!(detail.tool_calls[0].duration_ms, 123);
                }
                collected.push(item.turn);
            }
            if let Some(cursor) = page.next_cursor {
                cursor_params.after_created_at = Some(cursor.created_at);
                cursor_params.after_event_id = Some(cursor.event_id);
            } else {
                break;
            }
        }
        assert_eq!(
            collected,
            if at.starts_with("2000") {
                vec![5, 4, 4]
            } else {
                vec![4, 4, 5]
            }
        );
        let numbered = audit
            .list_turns(
                &owner,
                &session,
                &TurnListParams {
                    page: 2,
                    ..params.clone()
                },
            )
            .await
            .unwrap();
        assert_eq!(numbered.turns[0].turn, collected[1]);
        assert_eq!(
            audit
                .get_summary(&owner, &session)
                .await
                .unwrap()
                .turn_count,
            5
        );
        assert_eq!(
            audit
                .get_cross_session_stats(
                    &owner,
                    &CrossSessionStatsParams {
                        since: None,
                        until: None
                    }
                )
                .await
                .unwrap()
                .total_turns,
            5
        );
        assert_eq!(
            audit
                .get_turn_detail(&owner, &session, 2)
                .await
                .unwrap_err()
                .0,
            axum::http::StatusCode::NOT_FOUND
        );
    }
    ids.push(ordinary);
    let batch_queries = common::SqlQueries::default();
    let batch_page = audit
        .list_turns(
            &owner,
            &session,
            &TurnListParams {
                per_page: 100,
                ..params.clone()
            },
        )
        .with_subscriber(tracing_subscriber::registry().with(batch_queries.clone()))
        .await
        .unwrap();
    assert_eq!(batch_page.turns.len(), 3);
    {
        let statements = batch_queries.0.lock().unwrap();
        // Count every root-body read first, including any accidental per-row
        // reads that do not use IN. Then check the single batch's restriction.
        let bodies = statements
            .iter()
            .filter(|sql| {
                sql.contains("FROM agent_events")
                    && sql.contains(" AS content,")
                    && sql.contains("parent_run_id IS NULL")
            })
            .collect::<Vec<_>>();
        assert_eq!(bodies.len(), 1, "a multi-row page hydrates its roots once");
        assert!(bodies[0].contains("event_id IN ("));
    }

    // Delete after scalar key selection but before the one body batch. This
    // deterministic interleaving needs no production hook or retry stress.
    #[derive(Clone)]
    struct DeleteSelectedRoot {
        pool: sqlx::MySqlPool,
        owner: String,
        event: String,
        handle: tokio::runtime::Handle,
        fired: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }
    impl tracing::field::Visit for DeleteSelectedRoot {
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            if field.name() == "db.statement"
                && value.contains("WITH numbered_roots AS")
                && !self.fired.swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                let writer = self.clone();
                std::thread::spawn(move || {
                    writer.handle.block_on(async {
                        sqlx::query("DELETE FROM agent_events WHERE user_id = ? AND event_id = ?")
                            .bind(writer.owner)
                            .bind(writer.event)
                            .execute(&writer.pool)
                            .await
                            .unwrap();
                    })
                })
                .join()
                .unwrap();
            }
        }
        fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
    }
    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for DeleteSelectedRoot {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if event.metadata().target() == "sqlx::query" {
                event.record(&mut self.clone());
            }
        }
    }
    let deleted = DeleteSelectedRoot {
        pool: _bootstrap.get().clone(),
        owner: owner.clone(),
        event: cursor.event_id.clone(),
        handle: tokio::runtime::Handle::current(),
        fired: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    let queries = common::SqlQueries::default();
    let shortened = audit
        .list_turns(&owner, &session, &params)
        .with_subscriber(
            tracing_subscriber::registry()
                .with(deleted.clone())
                .with(queries.clone()),
        )
        .await
        .unwrap();
    assert!(deleted.fired.load(std::sync::atomic::Ordering::SeqCst));
    assert!(shortened.turns.is_empty());
    let advance = shortened.next_cursor.unwrap();
    assert_eq!(
        advance.event_id, cursor.event_id,
        "deleted bodies cannot strand continuation on the same page"
    );
    {
        let statements = queries.0.lock().unwrap();
        let bodies = statements
            .iter()
            .filter(|sql| sql.contains(" AS content,") && sql.contains("event_id IN ("))
            .collect::<Vec<_>>();
        assert_eq!(
            bodies.len(),
            1,
            "one owner-scoped body batch, not one query per row"
        );
        assert!(!bodies[0].contains("JOIN"));
    }
    let continued = audit
        .list_turns(
            &owner,
            &session,
            &TurnListParams {
                after_created_at: Some(advance.created_at),
                after_event_id: Some(advance.event_id),
                ..params.clone()
            },
        )
        .await
        .unwrap();
    assert_eq!(continued.turns[0].user_input_preview, "question");

    // Keep accepted edge sequences addressable without wrapping display counts.
    for sequence in [0, u32::MAX] {
        let mut event = root.clone();
        event.turn = Some(sequence);
        event.tokens_in = None;
        event.tokens_out = None;
        event.cache_read_tokens = None;
        event.cache_creation_tokens = None;
        event.metadata.as_mut().unwrap()["qualified_usage"] = serde_json::json!({});
        let payload = serde_json::to_value(&event).unwrap();
        let hash = astra_services::sync_outbox_canonical_payload_hash(&payload);
        let id = astra_services::sync_outbox_stable_event_id(&event, &hash).unwrap();
        service
            .create_event(
                owner.clone(),
                EventCreateRequestData {
                    ingestion_source: astra_services::events::EventIngestionSource::SyncOutbox,
                    event_id: Some(id.clone()),
                    session_id: session.clone(),
                    event_type: "turn".into(),
                    content: payload.to_string(),
                    agent_id: None,
                    agent_version: None,
                    parent_event_id: None,
                    parent_event_ids: None,
                    causal_chain_id: None,
                    metadata: Some(serde_json::json!({"sync_outbox":{"payload_hash":hash}})),
                },
            )
            .await
            .unwrap();
        assert_eq!(
            audit
                .get_turn_detail(&owner, &session, sequence)
                .await
                .unwrap()
                .turn,
            sequence
        );
        if sequence == u32::MAX {
            assert!(
                audit.list_turns(&owner, &session, &params).await.is_err(),
                "the display watermark cannot wrap its u32 API bound"
            );
            assert_eq!(
                sqlx::query_scalar::<_, i64>("SELECT 1")
                    .fetch_one(&pool)
                    .await
                    .unwrap(),
                1
            );
        }
        ids.push(id);
    }
    cleanup_agent_sessions_and_events_for_owner(&pool, &owner, &[session], &ids).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn sync_outbox_create_event_upserts_missing_session_header_live_matrixone() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();
    let mut journal_event = astra_services::session_journal::JournalEvent::config_change(
        Some(session_id.as_str()),
        "model",
        "db-it",
    );
    journal_event.ts = "2026-07-08T00:00:00Z".to_string();
    let content = serde_json::to_value(&journal_event).expect("journal content");
    let payload_hash = astra_services::sync_outbox_canonical_payload_hash(&content);
    let event_id = astra_services::sync_outbox_stable_event_id(&journal_event, &payload_hash)
        .expect("stable sync outbox event id");
    let event_service = DatabaseEventService::new(settings).with_pool(shared);

    let created = event_service
        .create_event(
            user_id.clone(),
            EventCreateRequestData {
                ingestion_source: astra_services::events::EventIngestionSource::SyncOutbox,
                event_id: Some(event_id),
                session_id: session_id.clone(),
                event_type: "config_change".into(),
                content: content.to_string(),
                agent_id: None,
                agent_version: None,
                parent_event_id: None,
                parent_event_ids: None,
                causal_chain_id: None,
                metadata: Some(serde_json::json!({
                    "sync_outbox": {
                        "payload_hash": payload_hash
                    }
                })),
            },
        )
        .await
        .expect("sync outbox create event should upsert session header");

    let row = sqlx::query(
        "SELECT event_count, last_event_id FROM agent_sessions WHERE session_id = ? AND user_id = ?",
    )
    .bind(&session_id)
    .bind(&user_id)
    .fetch_one(&pool)
    .await
    .expect("load upserted session header");
    assert_eq!(
        row.try_get::<i64, _>("event_count")
            .expect("decode event_count"),
        1
    );
    assert_eq!(
        row.try_get::<String, _>("last_event_id")
            .expect("decode last_event_id"),
        created.record.event_id
    );

    cleanup_agent_sessions_and_events_for_owner(
        &pool,
        &user_id,
        std::slice::from_ref(&session_id),
        &[created.record.event_id],
    )
    .await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn sync_outbox_late_event_cannot_recreate_deleted_session_live_matrixone() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();
    let user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();
    let event_id = format!("late-sync-{}", Uuid::new_v4());
    sqlx::query(
        "INSERT INTO agent_sessions (user_id, session_id, status, event_count)
         VALUES (?, ?, 'deleting', 0)",
    )
    .bind(&user_id)
    .bind(&session_id)
    .execute(&pool)
    .await
    .expect("seed deleting session parent");
    sqlx::query(
        "INSERT INTO agent_session_lifecycle_fences
         (user_id, session_id, delete_requested_at, database_deleted_at)
         VALUES (?, ?, CURRENT_TIMESTAMP(6), CURRENT_TIMESTAMP(6))",
    )
    .bind(&user_id)
    .bind(&session_id)
    .execute(&pool)
    .await
    .expect("seed completed deletion fence");
    let event_service = DatabaseEventService::new(settings).with_pool(shared);

    let result = event_service
        .create_event(
            user_id.clone(),
            EventCreateRequestData {
                ingestion_source: astra_services::events::EventIngestionSource::SyncOutbox,
                event_id: Some(event_id.clone()),
                session_id: session_id.clone(),
                event_type: "config_change".into(),
                content: "{}".into(),
                agent_id: None,
                agent_version: None,
                parent_event_id: None,
                parent_event_ids: None,
                causal_chain_id: None,
                metadata: Some(serde_json::json!({
                    "sync_outbox": {"payload_hash": "late-after-delete"}
                })),
            },
        )
        .await;
    assert!(
        result.is_err(),
        "deleted sessions must reject late sync events"
    );
    let session_status: String = sqlx::query_scalar(
        "SELECT status FROM agent_sessions WHERE user_id = ? AND session_id = ?",
    )
    .bind(&user_id)
    .bind(&session_id)
    .fetch_one(&pool)
    .await
    .expect("load deleting sync session parent");
    let event_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM agent_events WHERE user_id = ? AND event_id = ?")
            .bind(&user_id)
            .bind(&event_id)
            .fetch_one(&pool)
            .await
            .expect("count late sync events");
    assert_eq!(session_status, "deleting");
    assert_eq!(event_rows, 0);
    sqlx::query("DELETE FROM agent_sessions WHERE user_id = ? AND session_id = ?")
        .bind(&user_id)
        .bind(&session_id)
        .execute(&pool)
        .await
        .expect("clean deleting session parent");
    sqlx::query("DELETE FROM agent_session_lifecycle_fences WHERE user_id = ? AND session_id = ?")
        .bind(&user_id)
        .bind(&session_id)
        .execute(&pool)
        .await
        .expect("clean deletion fence");
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn sync_outbox_create_event_isolates_same_session_id_across_owners_live_matrixone() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let owner_user_id = Uuid::new_v4().to_string();
    let other_user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();
    let mut tx = pool.begin().await.expect("begin session header tx");
    astra_services::storage::add_agent_session_event_count_or_create(
        &mut tx,
        &session_id,
        &owner_user_id,
        0,
        None,
    )
    .await
    .expect("create owner session header");
    tx.commit().await.expect("commit session header tx");

    let mut journal_event = astra_services::session_journal::JournalEvent::config_change(
        Some(session_id.as_str()),
        "model",
        "foreign-owner-attempt",
    );
    journal_event.ts = "2026-07-08T00:00:01Z".to_string();
    let content = serde_json::to_value(&journal_event).expect("journal content");
    let payload_hash = astra_services::sync_outbox_canonical_payload_hash(&content);
    let event_id = astra_services::sync_outbox_stable_event_id(&journal_event, &payload_hash)
        .expect("stable sync outbox event id");
    let event_service = DatabaseEventService::new(settings).with_pool(shared);

    let created = event_service
        .create_event(
            other_user_id.clone(),
            EventCreateRequestData {
                ingestion_source: astra_services::events::EventIngestionSource::SyncOutbox,
                event_id: Some(event_id.clone()),
                session_id: session_id.clone(),
                event_type: "config_change".into(),
                content: content.to_string(),
                agent_id: None,
                agent_version: None,
                parent_event_id: None,
                parent_event_ids: None,
                causal_chain_id: None,
                metadata: Some(serde_json::json!({
                    "sync_outbox": {
                        "payload_hash": payload_hash
                    }
                })),
            },
        )
        .await
        .expect("other owner creates an isolated same-named session event");
    assert_eq!(created.record.user_id, other_user_id);
    assert_eq!(created.record.session_id, session_id);

    let owner_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_sessions WHERE session_id = ? AND user_id = ?",
    )
    .bind(&session_id)
    .bind(&owner_user_id)
    .fetch_one(&pool)
    .await
    .expect("count owner session rows");
    let foreign_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_sessions WHERE session_id = ? AND user_id = ?",
    )
    .bind(&session_id)
    .bind(&other_user_id)
    .fetch_one(&pool)
    .await
    .expect("count foreign session rows");
    let foreign_events: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM agent_events WHERE event_id = ? AND user_id = ?")
            .bind(&event_id)
            .bind(&other_user_id)
            .fetch_one(&pool)
            .await
            .expect("count foreign event rows");
    assert_eq!(owner_rows, 1);
    assert_eq!(foreign_rows, 1);
    assert_eq!(foreign_events, 1);

    cleanup_agent_sessions_and_events_for_owner(
        &pool,
        &owner_user_id,
        std::slice::from_ref(&session_id),
        &[],
    )
    .await;
    cleanup_agent_sessions_and_events_for_owner(
        &pool,
        &other_user_id,
        std::slice::from_ref(&session_id),
        &[event_id],
    )
    .await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn ordinary_create_event_requires_existing_session_header_live_matrixone() {
    let (shared, settings) = setup_pool_and_settings().await;

    let user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();
    let event_service = DatabaseEventService::new(settings).with_pool(shared);

    let error = event_service
        .create_event(
            user_id,
            EventCreateRequestData {
                ingestion_source: astra_services::events::EventIngestionSource::Client,
                event_id: None,
                session_id,
                event_type: "it_requires_session_header".into(),
                content: "ordinary events must not create sessions".into(),
                agent_id: None,
                agent_version: None,
                parent_event_id: None,
                parent_event_ids: None,
                causal_chain_id: None,
                metadata: None,
            },
        )
        .await
        .expect_err("ordinary create_event must require a pre-existing session");
    assert_eq!(error.0, axum::http::StatusCode::NOT_FOUND);
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn event_count_delta_service_live_matrixone() {
    let (shared, settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();

    let user_id = Uuid::new_v4().to_string();
    let service_session = Uuid::new_v4().to_string();

    for session_id in [&service_session] {
        sqlx::query(
            "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count) \
             VALUES (?, ?, 'write-path-it', 'active', 0)",
        )
        .bind(session_id)
        .bind(&user_id)
        .execute(&pool)
        .await
        .expect("insert session");
    }

    let event_service = DatabaseEventService::new(settings.clone()).with_pool(shared.clone());
    let created_one = event_service
        .create_event(
            user_id.clone(),
            EventCreateRequestData {
                ingestion_source: astra_services::events::EventIngestionSource::Client,
                event_id: None,
                session_id: service_session.clone(),
                event_type: "it_create".into(),
                content: "first".into(),
                agent_id: None,
                agent_version: None,
                parent_event_id: None,
                parent_event_ids: None,
                causal_chain_id: None,
                metadata: Some(serde_json::json!({"ordinal": 1})),
            },
        )
        .await
        .expect("create first event");
    let created_two = event_service
        .create_event(
            user_id.clone(),
            EventCreateRequestData {
                ingestion_source: astra_services::events::EventIngestionSource::Client,
                event_id: None,
                session_id: service_session.clone(),
                event_type: "it_create".into(),
                content: "second".into(),
                agent_id: None,
                agent_version: None,
                parent_event_id: None,
                parent_event_ids: None,
                causal_chain_id: None,
                metadata: Some(serde_json::json!({"ordinal": 2})),
            },
        )
        .await
        .expect("create second event");

    let service_count =
        sqlx::query("SELECT event_count FROM agent_sessions WHERE session_id = ? AND user_id = ?")
            .bind(&service_session)
            .bind(&user_id)
            .fetch_one(&pool)
            .await
            .expect("load service session count")
            .try_get::<i64, _>("event_count")
            .expect("decode service event_count");
    assert_eq!(
        service_count, 2,
        "DatabaseEventService::create_event should add one event_count delta per persisted row"
    );

    let event_ids = vec![created_one.record.event_id, created_two.record.event_id];
    cleanup_agent_sessions_and_events_for_owner(&pool, &user_id, &[service_session], &event_ids)
        .await;
}

// ── At-most-once idempotency integration tests ───────────────────────────────
// Gated by ASTRA_TEST_DB_IT=1. Document the end-to-end compare-before-reject
// contract introduced by the idempotency audit (PR: fix/at-most-once-idempotency-audit).

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
async fn it_publish_skill_idempotent_retry_returns_200() {
    let (shared_pool, settings) = setup_pool_and_settings().await;
    let raw_pool = shared_pool.get().clone();
    let svc = DatabaseSkillService::new(settings).with_pool(shared_pool);
    let name = format!("it-idem-pub-{}", Uuid::new_v4().simple());
    let request = astra_services::SkillPublishRequestData {
        name: name.clone(),
        version: "1.0.0".to_string(),
        description: "idempotency test".to_string(),
        dependencies: None,
        manifest: None,
        skill_type: "local".to_string(),
        remote_url: None,
        category: "user".to_string(),
        priority: 5,
        publisher_id: None,
        trust_tier: None,
    };

    let first = svc
        .publish_skill("it-user".to_string(), request.clone())
        .await
        .expect("first publish_skill should succeed");
    assert_eq!(first["status"], "published");

    // Retry with identical payload — must return 200.
    let second = svc
        .publish_skill("it-user".to_string(), request)
        .await
        .expect("idempotent retry of publish_skill must return 200");
    assert_eq!(second["status"], "published");

    let skill_id = format!("{}@1.0.0", name);
    sqlx::query("DELETE FROM skills_registry WHERE skill_id = ?")
        .bind(&skill_id)
        .execute(&raw_pool)
        .await
        .ok();
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn database_expired_reservation_fences_refreshed_writer() {
    let (shared_pool, _) = setup_pool_and_settings().await;
    let pool = shared_pool.get().clone();
    let owner = format!("it-context-owner-{}", Uuid::new_v4().simple());
    let session_id = format!("it-context-session-{}", Uuid::new_v4().simple());
    let key = SessionKeyV1::owner_session("test", &owner, &session_id, "main");
    let actor = ActorContextV1::owner_user(
        &owner,
        "bridge-owner",
        ActorKindV1::Server,
        SessionSurfaceV1::Cli,
        None,
        AuthorityEpochsV1::default(),
    );
    let coordinator = DatabaseSessionContextCoordinator::new(shared_pool);

    let lease = match coordinator
        .acquire_writer(
            &key,
            None,
            &actor,
            Duration::from_secs(30),
            "writer-heartbeat",
        )
        .await
        .expect("acquire writer")
    {
        AcquireWriterOutcome::Acquired(lease) => lease,
        other => panic!("expected new writer, got {other:?}"),
    };
    let reservation = match coordinator
        .reserve_turn(
            &lease,
            None,
            Duration::from_secs(30),
            "turn-heartbeat",
            None,
        )
        .await
        .expect("reserve turn")
    {
        ReserveTurnOutcome::Reserved(reservation) => reservation,
        other => panic!("expected new reservation, got {other:?}"),
    };

    // Make only the durable reservation stale. The writer remains live, which
    // deterministically exercises the acquire/reserve boundary without a sleep.
    let mut expired_reservation = reservation;
    expired_reservation.expires_at_unix_ms = 0;
    sqlx::query(
        "UPDATE session_context_heads
         SET active_reservation_json = ?, active_reservation_expires_at_ms = ?
         WHERE isolation_domain = ? AND owner_user_id = ?
           AND session_id = ? AND branch_id = ?",
    )
    .bind(serde_json::to_string(&expired_reservation).expect("serialize reservation"))
    .bind(0_i64)
    .bind(&key.isolation_domain)
    .bind(&key.owner_user_id)
    .bind(&key.session_id)
    .bind(&key.branch_id)
    .execute(&pool)
    .await
    .expect("expire durable reservation");

    let refreshed_lease = match coordinator
        .acquire_writer(
            &key,
            None,
            &actor,
            Duration::from_secs(30),
            "writer-heartbeat",
        )
        .await
        .expect("refresh writer")
    {
        AcquireWriterOutcome::AlreadyAcquired(lease) => lease,
        other => panic!("expected writer heartbeat, got {other:?}"),
    };
    assert!(matches!(
        coordinator
            .reserve_turn(
                &refreshed_lease,
                None,
                Duration::from_secs(30),
                "turn-heartbeat",
                None,
            )
            .await,
        Err(SessionContextCoordinatorError::Expired)
    ));
    assert!(
        coordinator
            .load_active_writer(&key)
            .await
            .unwrap()
            .is_none(),
        "expired reservation must fence the just-refreshed writer"
    );

    let recovery_actor = ActorContextV1::owner_user(
        &owner,
        "bridge-recovery",
        ActorKindV1::Server,
        SessionSurfaceV1::Cli,
        None,
        AuthorityEpochsV1::default(),
    );
    assert!(matches!(
        coordinator
            .acquire_writer(
                &key,
                None,
                &recovery_actor,
                Duration::from_secs(30),
                "writer-recovery",
            )
            .await
            .expect("recover writer after expiry fence"),
        AcquireWriterOutcome::Acquired(_)
    ));

    let _ = sqlx::query(
        "DELETE FROM session_context_authority_events
         WHERE isolation_domain = ? AND owner_user_id = ?
           AND session_id = ? AND branch_id = ?",
    )
    .bind(&key.isolation_domain)
    .bind(&key.owner_user_id)
    .bind(&key.session_id)
    .bind(&key.branch_id)
    .execute(&pool)
    .await;
    let _ = sqlx::query(
        "DELETE FROM session_context_heads
         WHERE isolation_domain = ? AND owner_user_id = ?
           AND session_id = ? AND branch_id = ?",
    )
    .bind(&key.isolation_domain)
    .bind(&key.owner_user_id)
    .bind(&key.session_id)
    .bind(&key.branch_id)
    .execute(&pool)
    .await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
async fn preferences_preserve_owner_values_and_observable_write_failures_on_live_matrixone() {
    let (shared, _settings) = setup_pool_and_settings().await;
    let pool = shared.get().clone();
    let service = astra_services::MatrixOneSyncService::new(pool.clone());
    let owner = Uuid::new_v4().to_string();
    let other = Uuid::new_v4().to_string();

    assert!(
        service
            .pull_all_preferences(&owner)
            .await
            .unwrap()
            .is_empty()
    );
    service
        .push_preference(&owner, "explain_mode", "true")
        .await
        .unwrap();
    service
        .push_preference(&other, "explain_mode", "false")
        .await
        .unwrap();
    for (user_id, value) in [(&owner, "true"), (&other, "false")] {
        assert_eq!(
            service.pull_all_preferences(user_id).await.unwrap(),
            vec![("explain_mode".into(), value.into())]
        );
    }
    service
        .push_preference(&owner, "explain_mode", "true")
        .await
        .unwrap();
    let version: i32 = sqlx::query_scalar(
        "SELECT version FROM user_preferences WHERE user_id = ? AND pref_key = 'explain_mode'",
    )
    .bind(&owner)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        version, 1,
        "an unchanged value must not write a new version"
    );
    service
        .push_preference(&owner, "explain_mode", "verbose")
        .await
        .unwrap();
    let version: i32 = sqlx::query_scalar(
        "SELECT version FROM user_preferences WHERE user_id = ? AND pref_key = 'explain_mode'",
    )
    .bind(&owner)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(version, 2);
    service
        .push_preference(&owner, "blocked_tools", "[]")
        .await
        .unwrap();
    assert_eq!(
        service.pull_all_preferences(&owner).await.unwrap(),
        vec![
            ("blocked_tools".into(), "[]".into()),
            ("explain_mode".into(), "verbose".into()),
        ]
    );
    assert_eq!(
        service.pull_all_preferences(&other).await.unwrap(),
        vec![("explain_mode".into(), "false".into()),]
    );

    sqlx::query(
        "UPDATE user_preferences SET version = ? WHERE user_id = ? AND pref_key = 'explain_mode'",
    )
    .bind(i32::MAX)
    .bind(&owner)
    .execute(&pool)
    .await
    .unwrap();
    let error = service
        .push_preference(&owner, "explain_mode", "true")
        .await
        .unwrap_err();
    assert!(error.contains("preference version overflow"));
    assert_eq!(
        service.pull_all_preferences(&owner).await.unwrap()[1].1,
        "verbose"
    );
    let version: i32 = sqlx::query_scalar(
        "SELECT version FROM user_preferences WHERE user_id = ? AND pref_key = 'explain_mode'",
    )
    .bind(&owner)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        version,
        i32::MAX,
        "a rejected write must leave the row unchanged"
    );

    sqlx::query("DELETE FROM user_preferences WHERE user_id IN (?, ?)")
        .bind(&owner)
        .bind(&other)
        .execute(&pool)
        .await
        .unwrap();
}
