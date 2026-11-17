//! Physical device attempts use current Run authority, never the standalone
//! logical inference owner. No provider credentials or network calls here.
use super::*;
use astra_services::{inference_execution::runner::*, runner_model_bindings::*};
use astra_turn_types::runner_inference::*;
use std::num::{NonZeroU32, NonZeroU64};

fn id(value: impl Into<String>) -> RunnerInferenceId {
    RunnerInferenceId::new(value).unwrap()
}

async fn runner_fixture(
    shared: &astra_core::SharedPool,
    user: &str,
) -> (AuthenticatedRunnerConnection, ResolvedRunnerModelBinding) {
    let connection = AuthenticatedRunnerConnection {
        user_id: user.into(),
        runner_id: id(Uuid::new_v4().to_string()),
        edge_id: Uuid::new_v4().to_string(),
    };
    sqlx::query("INSERT INTO edge_agent_registry (user_id, registry_id, edge_agent_id, edge_id) VALUES (?, ?, ?, ?)")
        .bind(user).bind(Uuid::new_v4().to_string()).bind(connection.runner_id.as_str()).bind(&connection.edge_id).execute(shared.get()).await.unwrap();
    let journal = id(Uuid::new_v4().to_string());
    enroll_runner_inference(
        shared,
        &connection,
        RUNNER_INFERENCE_PROTOCOL_VERSION,
        &journal,
        &id(Uuid::new_v4().to_string()),
    )
    .await
    .unwrap();
    let definition = RunnerInferenceBindingDefinition {
        identity: RunnerInferenceBindingIdentity {
            runner_id: connection.runner_id.clone(),
            journal_id: journal,
            binding_id: id("synthetic-device-model"),
            binding_revision: NonZeroU64::MIN,
            profile_revision: NonZeroU64::MIN,
        },
        display_name: RunnerInferenceModelName::new("Synthetic device").unwrap(),
        model_name: RunnerInferenceModelName::new("synthetic-provider-model").unwrap(),
        protocol: RunnerInferenceProtocol::OpenAiResponses,
        source: RunnerInferenceSource::Chatgpt {
            connection_ref: id(Uuid::new_v4().to_string()),
            authorization_revision: NonZeroU64::MIN,
            native_context: RunnerNativeContext {
                session_id: id("synthetic-uc-session"),
                workspace_id: id("synthetic-workspace"),
                role_candidate: None,
            },
        },
        supported_reasoning_efforts: vec!["high".into()],
        context_window: NonZeroU32::new(1024).unwrap(),
        max_output_tokens: NonZeroU32::new(64).unwrap(),
    };
    let publication = RunnerInferenceBindingPublication {
        protocol_version: RUNNER_INFERENCE_PROTOCOL_VERSION,
        operation_id: id(Uuid::new_v4().to_string()),
        expected_publication_revision: 0,
        change: RunnerInferenceBindingChange::Publish {
            definition: definition.clone(),
        },
    };
    let receipt = publish_runner_binding(shared, &connection, &publication)
        .await
        .unwrap();
    assert_eq!(
        publish_runner_binding(shared, &connection, &publication)
            .await
            .unwrap(),
        receipt
    );
    let resolved = resolve_runner_model_binding(shared, user, &definition.identity)
        .await
        .unwrap();
    (connection, resolved)
}

async fn native_run(
    shared: &astra_core::SharedPool,
    user: &str,
    session: &str,
    run: &str,
    uc_session: &str,
) {
    seed_run(shared.get(), user, session, run).await;
    let provenance =
        astra_services::auth::ExecutionAuthenticationProvenance::VerifiedProviderSession {
            user_id: user.into(),
            session_id: uc_session.into(),
            provider_id: "uc:https://uc.example.invalid/realm".into(),
            external_subject: "synthetic-uc-user".into(),
            execution_continuation: None,
        };
    let payload = serde_json::json!({"event_type":"run_started","data":{"execution_authentication":provenance}});
    sqlx::query("INSERT INTO agent_run_events (id, run_id, event_idx, user_id, session_id, event_type, event_id, idempotency_key, event_hash, producer_pod_id, payload_json, created_at)
        VALUES (?, ?, 0, ?, ?, 'run_started', ?, ?, ?, ?, ?, NOW(6))")
        .bind(Uuid::new_v4().to_string()).bind(run).bind(user).bind(session).bind(run).bind(run)
        .bind("a".repeat(64)).bind(TEST_INFERENCE_OWNER_POD_ID).bind(payload.to_string()).execute(shared.get()).await.unwrap();
    sqlx::query("UPDATE agent_runs SET last_event_idx = 0 WHERE user_id = ? AND run_id = ?")
        .bind(user)
        .bind(run)
        .execute(shared.get())
        .await
        .unwrap();
}

fn dispatch_plan(
    user: &str,
    session: &str,
    run: &str,
    binding: &ResolvedRunnerModelBinding,
) -> (InferenceInvocationInput, RunnerProviderAttemptDispatchPlan) {
    dispatch_plan_for_operation(user, session, run, binding, "device-primary")
}

fn dispatch_plan_for_operation(
    user: &str,
    session: &str,
    run: &str,
    binding: &ResolvedRunnerModelBinding,
    operation: &str,
) -> (InferenceInvocationInput, RunnerProviderAttemptDispatchPlan) {
    let mut input = run_input(user, session, run, 0, operation);
    input.offering_id = runner_offering_id(user, &binding.definition.identity);
    input.resolved_model_name = binding.definition.model_name.as_str().into();
    input.upstream_model_name = input.resolved_model_name.clone();
    input.provider = "chatgpt".into();
    input.access_kind = ModelAccessKind::ThisDevice;
    input.execution_placement = ModelExecutionPlacement::Edge;
    let expected_input = input.clone();
    let invocation = plan_inference_invocation(input).unwrap();
    let request = br#"{"model":"synthetic-provider-model","input":[{"role":"user","content":"synthetic prompt"}],"stream":true,"store":false,"max_output_tokens":64}"#;
    let wire = InferenceProviderWireIdentity::new(
        "openai_responses",
        format!("{:x}", sha2::Sha256::digest(request)),
        request.len() as u64,
    )
    .unwrap();
    let plan = plan_runner_provider_attempt_dispatch(RunnerProviderAttemptDispatchInput {
        invocation: &invocation,
        attempt_index: 0,
        wire,
        request_context: astra_services::ModelRequestContextSeed::server_default(),
        binding,
        request,
        deadline_unix_ms: (chrono::Utc::now().timestamp_millis() + 120_000) as u64,
    })
    .unwrap();
    (expected_input, plan)
}

#[tokio::test]
#[ignore = "requires isolated MatrixOne: ASTRA_TEST_DB_IT=1"]
async fn device_admission_rechecks_original_login_before_pat_and_workspace_policy() {
    use astra_services::{DatabaseModelService, FernetTokenEncryptor, ModelService};
    use axum::{
        Json, Router,
        extract::State,
        http::StatusCode,
        routing::{get, post},
    };
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    #[derive(Clone, Default)]
    struct Authority {
        active: Arc<AtomicBool>,
        policy: Arc<AtomicBool>,
        pat_calls: Arc<AtomicUsize>,
        policy_calls: Arc<AtomicUsize>,
    }
    let authority = Authority::default();
    authority.active.store(true, Ordering::SeqCst);
    authority.policy.store(true, Ordering::SeqCst);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let issuer = format!("{origin}/realms/moi");
    let returned_issuer = issuer.clone();
    let app = Router::new()
        .route("/realms/moi/protocol/openid-connect/token", post(|| async {
            Json(serde_json::json!({"access_token":"synthetic-service", "token_type":"Bearer", "expires_in":300}))
        }))
        .route("/api/v1/uc/internal/native-sessions/validate", post(move |State(state): State<Authority>, Json(body): Json<serde_json::Value>| {
            let issuer = returned_issuer.clone();
            async move {
                assert_eq!(body, serde_json::json!({"subject":"synthetic-uc-user", "session_id":"synthetic-uc-session"}));
                if !state.active.load(Ordering::SeqCst) {
                    return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"code":"ErrNativeSessionInvalid"})));
                }
                (StatusCode::OK, Json(serde_json::json!({"code":"OK", "data":{"issuer":issuer, "subject":"synthetic-uc-user", "session_id":"synthetic-uc-session", "client_id":"astra-cli", "audience":"astra-api"}})))
            }
        }))
        .route("/api/v1/uc/internal/accounts/synthetic-uc-user/aistudio-pat", get(|State(state): State<Authority>| async move {
            state.pat_calls.fetch_add(1, Ordering::SeqCst);
            Json(serde_json::json!({"api_key":"synthetic-runtime-pat"}))
        }))
        .route("/newmoi/workspaces/synthetic-workspace/model-sources/chatgpt-device/admission", get(|State(state): State<Authority>| async move {
            state.policy_calls.fetch_add(1, Ordering::SeqCst);
            if state.policy.load(Ordering::SeqCst) {
                (StatusCode::OK, Json(serde_json::json!({"code":"OK", "data":{"workspace_id":"synthetic-workspace", "source":"chatgpt_device", "policy_revision":format!("sha256:{}", "a".repeat(64))}})))
            } else {
                (StatusCode::FORBIDDEN, Json(serde_json::json!({"code":"ErrDeviceModelSourceDenied", "msg":"Source disabled"})))
            }
        })).with_state(authority.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let (shared, settings) = common::setup_pool_and_settings().await;
    let user = format!("device-{}", Uuid::new_v4());
    let (_, binding) = runner_fixture(&shared, &user).await;
    sqlx::query("INSERT INTO auth_external_identities (provider_id,external_subject,astra_user_id) VALUES (?,?,?)")
        .bind(format!("uc:{issuer}")).bind("synthetic-uc-user").bind(&user).execute(shared.get()).await.unwrap();
    let provider = astra_services::auth::uc::UcNativeProvider::new(astra_core::UcNativeSettings {
        genesis_usage_signing_key: None,
        issuer,
        adapter_url: origin.clone(),
        moi_api_url: format!("{origin}/newmoi"),
        genesis_url: origin,
        client_secret: "synthetic-service-secret".into(),
        builtin_memory: false,
    })
    .unwrap();
    let models = DatabaseModelService::new(
        settings,
        Arc::new(FernetTokenEncryptor::new("synthetic-device-test").unwrap()),
    )
    .with_pool(shared.clone())
    .with_uc_native(Some(provider));
    let offering = runner_offering_id(&user, &binding.definition.identity);
    models
        .admit_model_offering(user.clone(), offering.clone())
        .await
        .unwrap();
    authority.active.store(false, Ordering::SeqCst);
    let failure = models
        .admit_model_offering(user.clone(), offering.clone())
        .await
        .unwrap_err();
    assert_eq!(failure.0, StatusCode::UNAUTHORIZED);
    assert_eq!(failure.1.error_code.as_deref(), Some("uc_session_invalid"));
    assert_eq!(authority.pat_calls.load(Ordering::SeqCst), 1);
    assert_eq!(authority.policy_calls.load(Ordering::SeqCst), 1);
    authority.active.store(true, Ordering::SeqCst);
    authority.policy.store(false, Ordering::SeqCst);
    let failure = models
        .admit_model_offering(user.clone(), offering.clone())
        .await
        .unwrap_err();
    assert_eq!(failure.0, StatusCode::FORBIDDEN);
    assert_eq!(
        failure.1.error_code.as_deref(),
        Some("ErrDeviceModelSourceDenied")
    );
    authority.policy.store(true, Ordering::SeqCst);
    models.admit_model_offering(user, offering).await.unwrap();
    assert_eq!(authority.pat_calls.load(Ordering::SeqCst), 3);
    assert_eq!(authority.policy_calls.load(Ordering::SeqCst), 3);
    server.abort();
}

#[tokio::test]
#[ignore = "requires isolated MatrixOne: ASTRA_TEST_DB_IT=1"]
async fn device_attempt_has_one_run_owner_and_replayable_custody() {
    let shared = common::setup_pool().await;
    let user = format!("device-{}", Uuid::new_v4());
    let session = Uuid::new_v4().to_string();
    let run = Uuid::new_v4().to_string();
    let (connection, binding) = runner_fixture(&shared, &user).await;
    native_run(&shared, &user, &session, &run, "synthetic-uc-session").await;
    let (input, plan) = dispatch_plan(&user, &session, &run, &binding);
    let grant = admit_runner_provider_attempt_dispatch(&shared, &plan)
        .await
        .unwrap();
    assert_eq!(
        admit_runner_provider_attempt_dispatch(&shared, &plan)
            .await
            .unwrap(),
        grant
    );
    let logical: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM inference_invocations WHERE user_id = ?")
            .bind(&user)
            .fetch_one(shared.get())
            .await
            .unwrap();
    assert_eq!(
        logical, 0,
        "canonical Run owns device inference; no competing logical lease"
    );
    let attempts: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM inference_provider_attempts WHERE user_id = ?")
            .bind(&user)
            .fetch_one(shared.get())
            .await
            .unwrap();
    assert_eq!(attempts, 1);
    assert!(
        claim_runner_delivery(&shared, &connection, &grant.attempt)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        claim_runner_delivery(&shared, &connection, &grant.attempt)
            .await
            .unwrap()
            .is_none()
    );
    let terminal = InferenceInvocationTerminal::succeeded(
        InferenceUsage {
            input: astra_turn_types::NormalizedPromptCacheUsage::new(12, 0, 0),
            output_tokens: 3,
        },
        Some("synthetic-response".into()),
    );
    let response = serde_json::to_vec(&RunnerInferenceResponse {
        events: vec![],
        transport: RunnerInferenceTransportTerminal {
            status: RunnerInferenceTransportStatus::Complete,
            delivery: RunnerInferenceDeliveryEvidence::ResponseHeaders,
            provider_bytes: 0,
            events_delivered: 0,
        },
    })
    .unwrap();
    let hash =
        astra_services::inference_execution::runner::runner_terminal_digest(&terminal, &response)
            .unwrap();
    let ack = take_runner_terminal_custody(
        &shared,
        &connection,
        &grant.attempt,
        &terminal,
        &response,
        &hash,
    )
    .await
    .unwrap();
    assert_eq!(
        take_runner_terminal_custody(
            &shared,
            &connection,
            &grant.attempt,
            &terminal,
            &response,
            &hash
        )
        .await
        .unwrap(),
        ack
    );
    let recovered = load_run_owned_runner_response(&shared, &input, &grant.attempt)
        .await
        .unwrap();
    assert_eq!(recovered.response.as_bytes(), response);
    assert_eq!(recovered.physical_terminal, terminal);
    // Retention must pin a completed physical response until its canonical
    // Run settles, even though there is no standalone inference_invocations row.
    sqlx::query("UPDATE model_request_context_events SET created_at=DATE_SUB(NOW(6), INTERVAL 31 DAY) WHERE user_id=?")
        .bind(&user).execute(shared.get()).await.unwrap();
    let context_rows = || {
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM model_request_context_events WHERE user_id=?",
        )
        .bind(&user)
    };
    assert_eq!(context_rows().fetch_one(shared.get()).await.unwrap(), 2);
    let mut cursor = Default::default();
    while astra_services::model_request_context::expire_model_request_context_events(
        &shared,
        1000,
        &mut cursor,
    )
    .await
    .unwrap()
    .more
    {}
    assert_eq!(
        context_rows().fetch_one(shared.get()).await.unwrap(),
        2,
        "active Run evidence stays pinned"
    );
    // A new control owner cannot continue the old Run, even with exact custody.
    sqlx::query("UPDATE agent_runs SET owner_pod_id = 'synthetic-new-owner' WHERE user_id = ? AND run_id = ?").bind(&user).bind(&run).execute(shared.get()).await.unwrap();
    assert_eq!(
        load_run_owned_runner_response(&shared, &input, &grant.attempt)
            .await
            .unwrap_err()
            .kind,
        ServiceErrorKind::Conflict
    );
    sqlx::query("UPDATE agent_runs SET status='completed' WHERE user_id=? AND run_id=?")
        .bind(&user)
        .bind(&run)
        .execute(shared.get())
        .await
        .unwrap();
    let mut cursor = Default::default();
    while astra_services::model_request_context::expire_model_request_context_events(
        &shared,
        1000,
        &mut cursor,
    )
    .await
    .unwrap()
    .more
    {}
    assert_eq!(
        context_rows().fetch_one(shared.get()).await.unwrap(),
        0,
        "settled Run diagnostics can expire"
    );
}

#[tokio::test]
#[ignore = "requires isolated MatrixOne: ASTRA_TEST_DB_IT=1"]
async fn device_binding_cannot_be_dispatched_by_another_platform_session() {
    let shared = common::setup_pool().await;
    let user = format!("device-{}", Uuid::new_v4());
    let session = Uuid::new_v4().to_string();
    let run = Uuid::new_v4().to_string();
    let (_, binding) = runner_fixture(&shared, &user).await;
    native_run(&shared, &user, &session, &run, "another-uc-session").await;
    let (_, plan) = dispatch_plan(&user, &session, &run, &binding);
    assert_eq!(
        admit_runner_provider_attempt_dispatch(&shared, &plan)
            .await
            .unwrap_err()
            .kind,
        ServiceErrorKind::Conflict
    );
    let attempts: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM inference_provider_attempts WHERE user_id = ?")
            .bind(&user)
            .fetch_one(shared.get())
            .await
            .unwrap();
    assert_eq!(attempts, 0);
}

#[tokio::test]
#[ignore = "requires isolated MatrixOne: ASTRA_TEST_DB_IT=1"]
async fn reset_device_journal_retires_inventory_without_reenrolling_old_bindings() {
    let shared = common::setup_pool().await;
    let user = format!("device-{}", Uuid::new_v4());
    let (connection, binding) = runner_fixture(&shared, &user).await;
    assert_eq!(
        list_runner_model_catalog_bindings(&shared, &user)
            .await
            .unwrap()
            .len(),
        1
    );
    sqlx::query("UPDATE edge_agent_registry SET last_heartbeat_at = DATE_SUB(NOW(6), INTERVAL 120 SECOND) WHERE user_id = ?").bind(&user).execute(shared.get()).await.unwrap();
    let offline = list_runner_model_catalog_bindings(&shared, &user)
        .await
        .unwrap();
    assert_eq!(offline.len(), 1);
    assert!(!offline[0].online);
    sqlx::query("UPDATE edge_agent_registry SET last_heartbeat_at = NOW(6) WHERE user_id = ?")
        .bind(&user)
        .execute(shared.get())
        .await
        .unwrap();
    enroll_runner_inference(
        &shared,
        &connection,
        RUNNER_INFERENCE_PROTOCOL_VERSION,
        &id(Uuid::new_v4().to_string()),
        &id(Uuid::new_v4().to_string()),
    )
    .await
    .unwrap();
    assert!(
        list_runner_model_catalog_bindings(&shared, &user)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        enroll_runner_inference(
            &shared,
            &connection,
            RUNNER_INFERENCE_PROTOCOL_VERSION,
            &binding.definition.identity.journal_id,
            &id(Uuid::new_v4().to_string())
        )
        .await
        .unwrap_err()
        .kind,
        ServiceErrorKind::Conflict
    );
}

#[tokio::test]
#[ignore = "requires isolated MatrixOne: ASTRA_TEST_DB_IT=1"]
async fn replacement_journal_isolates_old_attempts_and_completes_new_work() {
    let shared = common::setup_pool().await;
    let user = format!("device-{}", Uuid::new_v4());
    let session = Uuid::new_v4().to_string();
    let run = Uuid::new_v4().to_string();
    let (connection, old_binding) = runner_fixture(&shared, &user).await;
    native_run(&shared, &user, &session, &run, "synthetic-uc-session").await;
    let (_, old_plan) = dispatch_plan(&user, &session, &run, &old_binding);
    let old_grant = admit_runner_provider_attempt_dispatch(&shared, &old_plan)
        .await
        .unwrap();
    assert!(
        claim_runner_delivery(&shared, &connection, &old_grant.attempt)
            .await
            .unwrap()
            .is_some()
    );
    sqlx::query("UPDATE inference_provider_attempts SET runner_dispatch_claim_expires_at=DATE_SUB(NOW(6), INTERVAL 1 SECOND) WHERE user_id=?")
        .bind(&user).execute(shared.get()).await.unwrap();

    let new_journal = id(Uuid::new_v4().to_string());
    enroll_runner_inference(
        &shared,
        &connection,
        RUNNER_INFERENCE_PROTOCOL_VERSION,
        &new_journal,
        &id(Uuid::new_v4().to_string()),
    )
    .await
    .unwrap();
    assert!(
        list_runner_reconciliation(&shared, &connection, 128)
            .await
            .unwrap()
            .is_empty()
    );
    // Defend direct/stale scheduling too, not only the listing query.
    assert!(
        claim_runner_delivery(&shared, &connection, &old_grant.attempt)
            .await
            .unwrap()
            .is_none()
    );
    let mut definition = old_binding.definition.clone();
    definition.identity.journal_id = new_journal;
    publish_runner_binding(
        &shared,
        &connection,
        &RunnerInferenceBindingPublication {
            protocol_version: RUNNER_INFERENCE_PROTOCOL_VERSION,
            operation_id: id(Uuid::new_v4().to_string()),
            expected_publication_revision: 0,
            change: RunnerInferenceBindingChange::Publish {
                definition: definition.clone(),
            },
        },
    )
    .await
    .unwrap();
    let binding = resolve_runner_model_binding(&shared, &user, &definition.identity)
        .await
        .unwrap();
    let (input, plan) = dispatch_plan_for_operation(
        &user,
        &session,
        &run,
        &binding,
        "replacement-journal-request",
    );
    let grant = admit_runner_provider_attempt_dispatch(&shared, &plan)
        .await
        .unwrap();
    let pending = list_runner_reconciliation(&shared, &connection, 128)
        .await
        .unwrap();
    assert_eq!(pending, vec![grant.clone()]);
    assert!(
        claim_runner_delivery(&shared, &connection, &grant.attempt)
            .await
            .unwrap()
            .is_some()
    );
    let terminal = InferenceInvocationTerminal::succeeded(
        InferenceUsage::default(),
        Some("new-journal-response".into()),
    );
    let response = br#"{"synthetic":"new-journal-custody"}"#;
    let hash =
        astra_services::inference_execution::runner::runner_terminal_digest(&terminal, response)
            .unwrap();
    take_runner_terminal_custody(
        &shared,
        &connection,
        &grant.attempt,
        &terminal,
        response,
        &hash,
    )
    .await
    .unwrap();
    let recovered = load_run_owned_runner_response(&shared, &input, &grant.attempt)
        .await
        .unwrap();
    assert_eq!(recovered.physical_terminal, terminal);
    let unresolved: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM inference_provider_attempts WHERE user_id=? AND attempt_id=? AND status='started' AND runner_terminal_hash IS NULL AND runner_capacity_retired_at IS NULL")
        .bind(&user).bind(old_grant.attempt.attempt_id.as_str()).fetch_one(shared.get()).await.unwrap();
    assert_eq!(
        unresolved, 1,
        "journal loss cannot fabricate completion or no-start evidence"
    );
}

#[tokio::test]
#[ignore = "requires isolated MatrixOne: ASTRA_TEST_DB_IT=1"]
async fn retired_unknown_attempts_do_not_permanently_exhaust_the_pending_queue() {
    let shared = common::setup_pool().await;
    let user = format!("device-{}", Uuid::new_v4());
    let session = Uuid::new_v4().to_string();
    let run = Uuid::new_v4().to_string();
    let (_, binding) = runner_fixture(&shared, &user).await;
    native_run(&shared, &user, &session, &run, "synthetic-uc-session").await;
    for index in 0..128 {
        let (_, plan) = dispatch_plan_for_operation(
            &user,
            &session,
            &run,
            &binding,
            &format!("device-queue-{index}"),
        );
        admit_runner_provider_attempt_dispatch(&shared, &plan)
            .await
            .unwrap();
    }
    let (_, next) =
        dispatch_plan_for_operation(&user, &session, &run, &binding, "device-queue-next");
    assert_eq!(
        admit_runner_provider_attempt_dispatch(&shared, &next)
            .await
            .unwrap_err()
            .kind,
        ServiceErrorKind::Conflict
    );
    // Fixture the durable result of bounded execution-right retirement. Usage
    // and physical outcome stay unknown; these are no longer dispatchable jobs.
    sqlx::query("UPDATE inference_provider_attempts SET runner_capacity_retired_at=NOW(6), runner_capacity_retired_by_boot='synthetic-replacement-boot' WHERE user_id=?")
        .bind(&user).execute(shared.get()).await.unwrap();
    admit_runner_provider_attempt_dispatch(&shared, &next)
        .await
        .unwrap();
    let evidence: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM inference_provider_attempts WHERE user_id=? AND runner_capacity_retired_at IS NOT NULL AND status='started' AND runner_terminal_hash IS NULL")
        .bind(&user).fetch_one(shared.get()).await.unwrap();
    assert_eq!(
        evidence, 128,
        "freeing queue capacity must not invent provider outcomes or erase evidence"
    );
}
