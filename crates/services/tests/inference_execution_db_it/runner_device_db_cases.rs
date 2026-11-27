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
    dispatch_plan_with_deadline(user, session, run, binding, operation, 120_000)
}

fn dispatch_plan_with_deadline(
    user: &str,
    session: &str,
    run: &str,
    binding: &ResolvedRunnerModelBinding,
    operation: &str,
    budget_ms: i64,
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
        stream_idle_ms: None,
        deadline_unix_ms: (chrono::Utc::now().timestamp_millis() + budget_ms) as u64,
    })
    .unwrap();
    (expected_input, plan)
}

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
    unavailable: Arc<AtomicBool>,
    pat_calls: Arc<AtomicUsize>,
    policy_calls: Arc<AtomicUsize>,
}

async fn test_model_service(
    shared: &astra_core::SharedPool,
    settings: astra_core::MatrixOneSettings,
    user: &str,
) -> (
    DatabaseModelService,
    Authority,
    tokio_util::task::AbortOnDropHandle<()>,
) {
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
        .route("/newmoi/aistudio/model-policy", get(|| async {
            Json(serde_json::json!({"code":"OK","data":{"default_model":"fixture-org","models":["fixture-org"]}}))
        }))
        .route("/api/v1/taas/llm/model-offerings", get(|| async {
            Json(serde_json::json!({"items":[{
                "id":"fixture-org-id","name":"fixture-org","type":"chat_text","status":"enabled",
                "context_window":32000,"max_output_tokens":4096
            }],"next_page_token":""}))
        }))
        .route("/newmoi/workspaces/synthetic-workspace/model-sources/chatgpt-device/admission", get(|State(state): State<Authority>| async move {
            state.policy_calls.fetch_add(1, Ordering::SeqCst);
            if state.unavailable.load(Ordering::SeqCst) {
                (StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({})))
            } else if state.policy.load(Ordering::SeqCst) {
                (StatusCode::OK, Json(serde_json::json!({"code":"OK", "data":{"workspace_id":"synthetic-workspace", "source":"chatgpt_device", "policy_revision":format!("sha256:{}", "a".repeat(64))}})))
            } else {
                (StatusCode::FORBIDDEN, Json(serde_json::json!({"code":"ErrDeviceModelSourceDenied", "msg":"Source disabled"})))
            }
        })).with_state(authority.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    sqlx::query("INSERT INTO auth_external_identities (provider_id,external_subject,astra_user_id) VALUES (?,?,?)")
        .bind(format!("uc:{issuer}")).bind("synthetic-uc-user").bind(user).execute(shared.get()).await.unwrap();
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
    (
        models,
        authority,
        tokio_util::task::AbortOnDropHandle::new(server),
    )
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn late_device_terminal_uses_completed_deletion_fence_and_preserves_other_session() {
    use astra_services::auth::session::{DatabaseSessionService, SessionService};
    use astra_services::inference_execution::runner_wait::{
        RunnerContinuationWaiters, RunnerReadiness,
    };
    let (shared, settings) = common::setup_pool_and_settings().await;
    let user = Uuid::new_v4().to_string();
    let (connection, binding) = runner_fixture(&shared, &user).await;
    let (models, _authority, _server) = test_model_service(&shared, settings.clone(), &user).await;
    let sessions = DatabaseSessionService::new(settings).with_pool(shared.clone());
    let terminal = InferenceInvocationTerminal::succeeded(InferenceUsage::default(), None);
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
    let digest =
        astra_services::inference_execution::runner::runner_terminal_digest(&terminal, &response)
            .unwrap();
    for (delete, lost_terminal) in [(true, false), (false, false), (false, true)] {
        let session = Uuid::new_v4().to_string();
        let run = Uuid::new_v4().to_string();
        native_run(&shared, &user, &session, &run, "synthetic-uc-session").await;
        let (_, plan) = dispatch_plan(&user, &session, &run, &binding);
        let grant = admit_runner_provider_attempt_dispatch(&shared, &plan)
            .await
            .unwrap();
        let identity = &grant.attempt;
        assert_eq!(
            runner_preview_socket(&shared, identity, &grant.process_boot_nonce)
                .await
                .unwrap(),
            Some((
                connection.edge_id.clone(),
                if delete {
                    None
                } else {
                    Some("http://preview.invalid".into())
                }
            ))
        );
        assert!(
            runner_preview_socket(&shared, identity, &id("different-boot"))
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            retired_runner_terminal(&shared, &connection, identity, &digest)
                .await
                .unwrap()
                .is_none()
        );
        let lease = claim_runner_delivery_with_transport(&shared, &connection, identity, true)
            .await
            .unwrap()
            .unwrap();
        let ready = authorize_runner_dispatch(&shared, &models, &connection, &grant, &lease.token)
            .await
            .unwrap()
            .unwrap();
        let RunnerDeliveryAction::Dispatch(issued) = ready.action else {
            panic!("expected start grant")
        };
        record_runner_start_evidence(
            &shared,
            &connection,
            &issued,
            RunnerInferenceStartEvidence::FenceCommitted,
        )
        .await
        .unwrap();
        if delete {
            sqlx::query(
                "UPDATE edge_agent_registry SET capabilities_json=? WHERE user_id=? AND edge_id=?",
            )
            .bind(r#"{"capacity_relay_v1":{"origin":"http://preview.invalid"}}"#)
            .bind(&user)
            .bind(&connection.edge_id)
            .execute(shared.get())
            .await
            .unwrap();
            sessions
                .delete_session(session, user.clone())
                .await
                .unwrap();
            assert!(
                validate_runner_terminal_attempt(&shared, &connection, identity)
                    .await
                    .is_err()
            );
            let receipt = retired_runner_terminal(&shared, &connection, identity, &digest)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(receipt.attempt, *identity);
            assert_eq!(receipt.terminal_sha256, digest);
        } else {
            let waiters = Arc::new(RunnerContinuationWaiters::default());
            let mut readiness = waiters.subscribe(shared.clone(), identity).await.unwrap();
            assert_eq!(*readiness.borrow(), RunnerReadiness::Waiting);
            let observed_until = record_runner_start_evidence(
                &shared,
                &connection,
                &issued,
                RunnerInferenceStartEvidence::ExecutionActive,
            )
            .await
            .unwrap();
            let expires = observed_until.expect("active observation was committed");
            if !lost_terminal {
                waiters.note_local_execution(
                    identity,
                    Some((expires, tokio_util::sync::CancellationToken::new())),
                );
            }
            let executing =
                readiness.wait_for(|state| matches!(state, RunnerReadiness::Executing(_)));
            drop(
                tokio::time::timeout(std::time::Duration::from_secs(5), executing)
                    .await
                    .unwrap()
                    .unwrap(),
            );
            if lost_terminal {
                record_runner_start_evidence(
                    &shared,
                    &connection,
                    &issued,
                    RunnerInferenceStartEvidence::ExecutionEndedWithoutTerminal,
                )
                .await
                .unwrap();
                let ended = readiness.wait_for(|state| *state == RunnerReadiness::DeliveryUnknown);
                drop(
                    tokio::time::timeout(std::time::Duration::from_secs(5), ended)
                        .await
                        .unwrap()
                        .unwrap(),
                );
                assert!(
                    claim_runner_delivery_with_transport(&shared, &connection, identity, true)
                        .await
                        .unwrap()
                        .is_none()
                );
                continue;
            }
            let invalid = RunnerInferenceDigest::new("a".repeat(64)).unwrap();
            let rejection = take_runner_terminal_custody(
                &shared,
                &connection,
                identity,
                &terminal,
                &response,
                &invalid,
            )
            .await
            .unwrap_err();
            assert!(matches!(rejection.kind, ServiceErrorKind::Invalid));
            assert!(
                retired_runner_terminal(&shared, &connection, identity, &invalid)
                    .await
                    .unwrap()
                    .is_none()
            );
            let ack = take_runner_terminal_custody(
                &shared,
                &connection,
                identity,
                &terminal,
                &response,
                &digest,
            )
            .await
            .unwrap();
            assert_eq!(ack.terminal_sha256, digest);
            waiters.terminal_committed(identity);
            assert!(matches!(*readiness.borrow(), RunnerReadiness::Ready));
        }
    }
}

#[tokio::test]
#[ignore = "requires isolated MatrixOne: ASTRA_TEST_DB_IT=1"]
async fn device_admission_rechecks_original_login_before_pat_and_workspace_policy() {
    let (shared, settings) = common::setup_pool_and_settings().await;
    let user = format!("device-{}", Uuid::new_v4());
    let (connection, binding) = runner_fixture(&shared, &user).await;
    let (models, authority, _server) = test_model_service(&shared, settings, &user).await;
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
    models
        .admit_model_offering(user.clone(), offering.clone())
        .await
        .unwrap();
    assert_eq!(authority.pat_calls.load(Ordering::SeqCst), 3);
    assert_eq!(authority.policy_calls.load(Ordering::SeqCst), 3);
    let session = Uuid::new_v4().to_string();
    let run = Uuid::new_v4().to_string();
    native_run(&shared, &user, &session, &run, "synthetic-uc-session").await;
    let (_, plan) = dispatch_plan(&user, &session, &run, &binding);
    let first = admit_runner_provider_attempt_dispatch(&shared, &plan)
        .await
        .unwrap();
    let reserve = claim_runner_delivery_with_transport(&shared, &connection, &first.attempt, true)
        .await
        .unwrap()
        .unwrap();
    authority.unavailable.store(true, Ordering::SeqCst);
    assert!(
        authorize_runner_dispatch(&shared, &models, &connection, &first, &reserve.token)
            .await
            .unwrap()
            .is_none()
    );
    let held: Option<String> = sqlx::query_scalar("SELECT runner_dispatch_claim_token FROM inference_provider_attempts WHERE user_id=? AND attempt_id=?")
        .bind(&user).bind(first.attempt.attempt_id.as_str()).fetch_one(shared.get()).await.unwrap();
    assert!(held.is_none());
    authority.unavailable.store(false, Ordering::SeqCst);
    let expired_token = reserve.token;
    let reserve = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Some(claim) =
                claim_runner_delivery_with_transport(&shared, &connection, &first.attempt, true)
                    .await
                    .unwrap()
            {
                break claim;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("temporary outage should not hold the crash lease");
    authority.unavailable.store(true, Ordering::SeqCst);
    assert!(
        authorize_runner_dispatch(&shared, &models, &connection, &first, &expired_token)
            .await
            .unwrap()
            .is_none()
    );
    authority.unavailable.store(false, Ordering::SeqCst);
    // A delayed failure for the old reservation must not release the new holder.
    let issued = authorize_runner_dispatch(&shared, &models, &connection, &first, &reserve.token)
        .await
        .unwrap()
        .unwrap();
    let delivery_token = issued.token;
    let RunnerDeliveryAction::Dispatch(issued) = issued.action else {
        panic!("expected first dispatch")
    };
    assert!(
        load_runner_request_custody(&shared, &connection, &issued, &delivery_token)
            .await
            .is_ok()
    );
    let mut previous_reservation = issued;
    for revoke_session in [false, true] {
        let (input, plan) = dispatch_plan_for_operation(
            &user,
            &session,
            &run,
            &binding,
            if revoke_session {
                "revoked-session"
            } else {
                "revoked-policy"
            },
        );
        let queued = admit_runner_provider_attempt_dispatch(&shared, &plan)
            .await
            .unwrap();
        assert!(
            claim_runner_delivery_with_transport(&shared, &connection, &queued.attempt, true)
                .await
                .unwrap()
                .is_none()
        );
        if revoke_session {
            authority.active.store(false, Ordering::SeqCst);
        } else {
            authority.policy.store(false, Ordering::SeqCst);
        }
        // Release the slot only after the owner starts denying admission.
        let held = &previous_reservation;
        request_runner_cancellation(&shared, &user, &held.attempt)
            .await
            .unwrap();
        assert!(
            load_runner_request_custody(&shared, &connection, held, &delivery_token)
                .await
                .is_err()
        );
        record_runner_start_evidence(
            &shared,
            &connection,
            held,
            RunnerInferenceStartEvidence::CancelledWithoutFence,
        )
        .await
        .unwrap();
        let lease =
            claim_runner_delivery_with_transport(&shared, &connection, &queued.attempt, true)
                .await
                .unwrap()
                .unwrap();
        let denied =
            authorize_runner_dispatch(&shared, &models, &connection, &queued, &lease.token)
                .await
                .unwrap()
                .unwrap();
        assert!(
            matches!(denied.action, RunnerDeliveryAction::Cancel(ref grant) if grant == &queued)
        );
        assert!(
            load_runner_request_custody(&shared, &connection, &queued, &denied.token)
                .await
                .is_err()
        );
        let rejection: String = sqlx::query_scalar("SELECT runner_admission_rejection FROM inference_provider_attempts WHERE user_id=? AND attempt_id=?")
            .bind(&user).bind(queued.attempt.attempt_id.as_str()).fetch_one(shared.get()).await.unwrap();
        let rejection: astra_core::ClassifiedError = serde_json::from_str(&rejection).unwrap();
        let (kind, code) = if revoke_session {
            (astra_core::ErrorKind::Auth, "uc_session_invalid")
        } else {
            (
                astra_core::ErrorKind::PolicyDenied,
                "ErrDeviceModelSourceDenied",
            )
        };
        assert_eq!(rejection.kind, kind);
        let details: serde_json::Value =
            serde_json::from_str(rejection.details_json.as_deref().unwrap()).unwrap();
        assert_eq!(details["code"], code);
        if revoke_session {
            record_runner_start_evidence(
                &shared,
                &connection,
                &queued,
                RunnerInferenceStartEvidence::CancelledWithoutFence,
            )
            .await
            .unwrap();
            let terminal = InferenceInvocationTerminal {
                status: InferenceTerminalStatus::Cancelled,
                usage: InferenceUsage::default(),
                usage_status: InferenceUsageStatus::Unavailable,
                provider_response_id: None,
                error_kind: Some("cancelled".into()),
                error_message: None,
            };
            let response = serde_json::to_vec(&RunnerInferenceResponse {
                events: vec![RunnerInferenceProviderEvent::Eof],
                transport: RunnerInferenceTransportTerminal {
                    status: RunnerInferenceTransportStatus::Cancelled,
                    delivery: RunnerInferenceDeliveryEvidence::NotDispatched,
                    provider_bytes: 0,
                    events_delivered: 1,
                },
            })
            .unwrap();
            let hash = astra_services::inference_execution::runner::runner_terminal_digest(
                &terminal, &response,
            )
            .unwrap();
            take_runner_terminal_custody(
                &shared,
                &connection,
                &queued.attempt,
                &terminal,
                &response,
                &hash,
            )
            .await
            .unwrap();
            let recovered = load_run_owned_runner_response(&shared, &input, &queued.attempt)
                .await
                .unwrap();
            assert_eq!(recovered.physical_terminal, terminal);
            assert_eq!(recovered.admission_rejection.as_ref(), Some(&rejection));
        }
        previous_reservation = queued;
        authority.active.store(true, Ordering::SeqCst);
        authority.policy.store(true, Ordering::SeqCst);
    }
    let healthy = models.user_model_catalog(user.clone()).await.unwrap();
    assert!(!healthy.device_inventory_unavailable);
    sqlx::query(
        "UPDATE runner_model_binding_publications SET definition_json = '{}' WHERE user_id = ?",
    )
    .bind(&user)
    .execute(shared.get())
    .await
    .unwrap();
    let degraded = models.user_model_catalog(user).await.unwrap();
    assert!(degraded.device_inventory_unavailable);
    assert_eq!(healthy.default_offering_id, degraded.default_offering_id);
    assert_eq!(degraded.items.len(), 1);
    assert_eq!(degraded.items[0].name, "fixture-org");
    let sources = astra_services::models::server_model_access_declarations_for_models(
        degraded.allows_deployment,
        degraded.device_inventory_unavailable,
        &degraded.items,
    );
    assert!(sources.iter().any(|source| matches!(
        source.availability,
        astra_services::models::ModelAccessAvailability::Unavailable {
            reason: astra_services::models::ModelAccessReason::ConnectionUnavailable,
            ..
        }
    )));
}

#[tokio::test]
#[ignore = "requires isolated MatrixOne: ASTRA_TEST_DB_IT=1"]
async fn device_attempt_has_one_run_owner_and_replayable_custody() {
    let (shared, settings) = common::setup_pool_and_settings().await;
    let user = format!("device-{}", Uuid::new_v4());
    let (models, _authority, _server) = test_model_service(&shared, settings, &user).await;
    let session = Uuid::new_v4().to_string();
    let run = Uuid::new_v4().to_string();
    let (connection, binding) = runner_fixture(&shared, &user).await;
    native_run(&shared, &user, &session, &run, "synthetic-uc-session").await;
    let (input, plan) =
        dispatch_plan_with_deadline(&user, &session, &run, &binding, "device-primary", 12_000);
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
    let lease = claim_runner_delivery_with_transport(&shared, &connection, &grant.attempt, true)
        .await
        .unwrap()
        .unwrap();
    let ready = authorize_runner_dispatch(&shared, &models, &connection, &grant, &lease.token)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(ready.action, RunnerDeliveryAction::Dispatch(_)));
    assert!(
        claim_runner_delivery_with_transport(&shared, &connection, &grant.attempt, true)
            .await
            .unwrap()
            .is_none()
    );
    let RunnerDeliveryAction::Dispatch(issued) = ready.action else {
        unreachable!()
    };
    assert_eq!(issued.start_before_unix_ms, issued.deadline_unix_ms);
    sqlx::query("UPDATE inference_provider_attempts SET runner_dispatch_claim_expires_at = NOW(6) WHERE attempt_id = ? AND user_id = ?")
        .bind(grant.attempt.attempt_id.as_str()).bind(&user).execute(shared.get()).await.unwrap();
    let replay = claim_runner_delivery_with_transport(&shared, &connection, &grant.attempt, true)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        replay.action,
        RunnerDeliveryAction::Dispatch(issued.clone())
    );
    assert!(
        load_runner_request_custody(&shared, &connection, &issued, &ready.token)
            .await
            .is_err()
    );
    assert!(
        load_runner_request_custody(&shared, &connection, &issued, &replay.token)
            .await
            .is_ok()
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
    assert!(recovered.admission_rejection.is_none());
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
            .bindings
            .len(),
        1
    );
    sqlx::query("UPDATE edge_agent_registry SET last_heartbeat_at = DATE_SUB(NOW(6), INTERVAL 120 SECOND) WHERE user_id = ?").bind(&user).execute(shared.get()).await.unwrap();
    let offline = list_runner_model_catalog_bindings(&shared, &user)
        .await
        .unwrap();
    assert_eq!(offline.bindings.len(), 1);
    assert!(!offline.bindings[0].online);
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
            .bindings
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
    let (shared, settings) = common::setup_pool_and_settings().await;
    let user = format!("device-{}", Uuid::new_v4());
    let (models, _authority, _server) = test_model_service(&shared, settings, &user).await;
    let session = Uuid::new_v4().to_string();
    let run = Uuid::new_v4().to_string();
    let (connection, old_binding) = runner_fixture(&shared, &user).await;
    native_run(&shared, &user, &session, &run, "synthetic-uc-session").await;
    let (_, old_plan) = dispatch_plan(&user, &session, &run, &old_binding);
    let old_grant = admit_runner_provider_attempt_dispatch(&shared, &old_plan)
        .await
        .unwrap();
    assert!(
        claim_runner_delivery_with_transport(&shared, &connection, &old_grant.attempt, true)
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
        claim_runner_delivery_with_transport(&shared, &connection, &old_grant.attempt, true)
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
    let lease = claim_runner_delivery_with_transport(&shared, &connection, &grant.attempt, true)
        .await
        .unwrap()
        .unwrap();
    let ready = authorize_runner_dispatch(&shared, &models, &connection, &grant, &lease.token)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(ready.action, RunnerDeliveryAction::Dispatch(_)));
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
async fn compacted_native_binding_republishes_and_replays_by_journal_revision() {
    let shared = common::setup_pool().await;
    let user = format!("device-{}", Uuid::new_v4());
    let (connection, binding) = runner_fixture(&shared, &user).await;
    let mut definition = binding.definition;
    definition.identity.binding_id = id("native-synthetic");
    let publish = |expected: u64, definition| RunnerInferenceBindingPublication {
        protocol_version: RUNNER_INFERENCE_PROTOCOL_VERSION,
        operation_id: id(Uuid::new_v4().to_string()),
        expected_publication_revision: expected,
        change: RunnerInferenceBindingChange::Publish { definition },
    };
    definition.identity.binding_revision = NonZeroU64::new(2).unwrap();
    publish_runner_binding(&shared, &connection, &publish(1, definition.clone()))
        .await
        .unwrap();
    let disable = RunnerInferenceBindingPublication {
        protocol_version: RUNNER_INFERENCE_PROTOCOL_VERSION,
        operation_id: id(Uuid::new_v4().to_string()),
        expected_publication_revision: 2,
        change: RunnerInferenceBindingChange::Disable {
            identity: definition.identity.clone(),
        },
    };
    publish_runner_binding(&shared, &connection, &disable)
        .await
        .unwrap();
    definition.identity.binding_revision = NonZeroU64::new(4).unwrap();
    let operation = publish(3, definition.clone());
    let receipt = publish_runner_binding(&shared, &connection, &operation)
        .await
        .unwrap();
    assert_eq!(receipt.publication_revision.get(), 4);
    assert_eq!(
        publish_runner_binding(&shared, &connection, &operation)
            .await
            .unwrap(),
        receipt
    );
    assert_eq!(
        resolve_runner_model_binding(&shared, &user, &definition.identity)
            .await
            .unwrap()
            .definition,
        definition
    );
    assert!(
        publish_runner_binding(&shared, &connection, &publish(4, definition))
            .await
            .is_err()
    );
}

#[tokio::test]
#[ignore = "requires isolated MatrixOne: ASTRA_TEST_DB_IT=1"]
async fn queued_device_attempts_keep_their_deadline_until_capacity_is_available() {
    let (shared, settings) = common::setup_pool_and_settings().await;
    let user = format!("device-{}", Uuid::new_v4());
    let (models, authority, _server) = test_model_service(&shared, settings, &user).await;
    let (connection, binding) = runner_fixture(&shared, &user).await;
    let mut queued = Vec::new();
    let mut first = None;
    for index in 0..5 {
        let session = Uuid::new_v4().to_string();
        let run = Uuid::new_v4().to_string();
        native_run(&shared, &user, &session, &run, "synthetic-uc-session").await;
        let (_, plan) = dispatch_plan(&user, &session, &run, &binding);
        let grant = admit_runner_provider_attempt_dispatch(&shared, &plan)
            .await
            .unwrap();
        assert_eq!(grant.start_before_unix_ms, 0);
        if index == 0 {
            let (_, sibling) =
                dispatch_plan_for_operation(&user, &session, &run, &binding, "device-sibling");
            let sibling = admit_runner_provider_attempt_dispatch(&shared, &sibling)
                .await
                .unwrap();
            assert_eq!(sibling.start_before_unix_ms, 0);
            queued.push(sibling);
        }
        let claim =
            claim_runner_delivery_with_transport(&shared, &connection, &grant.attempt, true)
                .await
                .unwrap();
        if index < 4 {
            let claim = claim.unwrap();
            assert!(matches!(claim.action, RunnerDeliveryAction::Authorize(_)));
            let ready =
                authorize_runner_dispatch(&shared, &models, &connection, &grant, &claim.token)
                    .await
                    .unwrap()
                    .unwrap();
            let RunnerDeliveryAction::Dispatch(issued) = ready.action else {
                panic!("missing authorized grant")
            };
            assert!(
                issued.start_before_unix_ms > 0
                    && issued.start_before_unix_ms <= issued.deadline_unix_ms
            );
            if index == 0 {
                first = Some(issued);
            }
        } else {
            assert!(claim.is_none());
            queued.push(grant);
        }
    }
    assert!(
        claim_runner_delivery_with_transport(&shared, &connection, &queued[0].attempt, true)
            .await
            .unwrap()
            .is_none()
    );
    let scope = &queued[0].attempt.scope;
    for index in 0..122 {
        let (_, plan) = dispatch_plan_for_operation(
            &user,
            scope.session_id().unwrap(),
            scope.run_id().unwrap(),
            &binding,
            &format!("queued-{index}"),
        );
        admit_runner_provider_attempt_dispatch(&shared, &plan)
            .await
            .unwrap();
    }
    let owner = (connection.runner_id.as_str().to_owned(), user.clone());
    assert_eq!(
        pending_runner_owners(&shared, std::slice::from_ref(&connection.edge_id))
            .await
            .unwrap(),
        vec![owner]
    );
    assert!(
        pending_runner_owners(&shared, &["obsolete-connection".to_owned()])
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        list_runner_reconciliation(&shared, &connection, 8)
            .await
            .unwrap()
            .is_empty(),
        "occupied slots must not produce candidate transactions"
    );
    let first = first.unwrap();
    request_runner_cancellation(&shared, &user, &first.attempt)
        .await
        .unwrap();
    assert!(
        list_runner_reconciliation(&shared, &connection, 8)
            .await
            .unwrap()
            .iter()
            .any(|candidate| candidate == &first),
        "cancellation is schedulable even when all slots are occupied"
    );
    record_runner_start_evidence(
        &shared,
        &connection,
        &first,
        RunnerInferenceStartEvidence::CancelledWithoutFence,
    )
    .await
    .unwrap();
    assert!(
        claim_runner_delivery_with_transport(&shared, &connection, &queued[0].attempt, false)
            .await
            .unwrap()
            .is_none()
    );
    let claim =
        claim_runner_delivery_with_transport(&shared, &connection, &queued[1].attempt, true)
            .await
            .unwrap()
            .unwrap();
    assert!(matches!(claim.action, RunnerDeliveryAction::Authorize(_)));
    // Admission succeeded when this request was queued. Its live source or UC
    // session has since been revoked. The reserved slot cannot issue a grant.
    authority.policy.store(false, Ordering::SeqCst);
    let denied = authorize_runner_dispatch(&shared, &models, &connection, &queued[1], &claim.token)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(denied.action, RunnerDeliveryAction::Cancel(ref grant) if grant.start_before_unix_ms == 0)
    );
    authority.policy.store(true, Ordering::SeqCst);
    assert!(
        authorize_runner_dispatch(&shared, &models, &connection, &queued[1], &claim.token)
            .await
            .unwrap()
            .is_none()
    );
    request_runner_cancellation(&shared, &user, &queued[1].attempt)
        .await
        .unwrap();
    record_runner_start_evidence(
        &shared,
        &connection,
        &queued[1],
        RunnerInferenceStartEvidence::CancelledWithoutFence,
    )
    .await
    .unwrap();
    let claim =
        claim_runner_delivery_with_transport(&shared, &connection, &queued[0].attempt, true)
            .await
            .unwrap()
            .unwrap();
    assert!(matches!(claim.action, RunnerDeliveryAction::Authorize(_)));
    let ready = authorize_runner_dispatch(&shared, &models, &connection, &queued[0], &claim.token)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(ready.action, RunnerDeliveryAction::Dispatch(grant) if grant.start_before_unix_ms > 0 && grant.attempt == queued[0].attempt)
    );
}

#[tokio::test]
#[ignore = "requires isolated MatrixOne: ASTRA_TEST_DB_IT=1"]
async fn recovery_batches_are_local_to_socket_owners_and_independent_of_admission_pod() {
    let (shared, settings) = common::setup_pool_and_settings().await;
    let tail_user = format!("tail-{}", Uuid::new_v4());
    let (models, _authority, _server) = test_model_service(&shared, settings, &tail_user).await;
    let (socket, binding) = runner_fixture(&shared, &tail_user).await;
    let session = Uuid::new_v4().to_string();
    let run = Uuid::new_v4().to_string();
    native_run(&shared, &tail_user, &session, &run, "synthetic-uc-session").await;
    let user = format!("scan-{}", Uuid::new_v4());
    sqlx::query(
        "INSERT INTO edge_agent_registry
        (user_id, registry_id, edge_agent_id, edge_id, inference_edge_id, inference_journal_id)
        SELECT ?, CONCAT('reg',result), CONCAT('r',result), CONCAT(?,result), CONCAT(?,result), 'j'
        FROM generate_series(0,4094) AS numbers",
    )
    .bind(&user)
    .bind(&user)
    .bind(&user)
    .execute(shared.get())
    .await
    .unwrap();
    // This writer owns neither socket set. Both readers must recover its
    // inserts, including a cancellation, without a process-local notification.
    sqlx::query("INSERT INTO inference_provider_attempts
        (user_id,attempt_id,invocation_id,session_id,attempt_index,provider,admission_token,provider_protocol,
         provider_wire_hash,provider_wire_bytes,status,usage_status,runner_id,runner_journal_id,runner_cancel_requested_at)
        SELECT ?,CONCAT('a',result),CONCAT('i',result),'s',0,'test','test','openai_compatible',
           REPEAT('a',64),1,'started','unavailable',CONCAT('r',result),'j',IF(result=4094,NOW(6),NULL)
        FROM generate_series(0,4094) AS numbers")
        .bind(&user).execute(shared.get()).await.unwrap();
    // The last socket belongs to a real, short-deadline attempt admitted on
    // the writer. Its owner must progress all the way to request custody.
    let (_, plan) =
        dispatch_plan_with_deadline(&tail_user, &session, &run, &binding, "short-tail", 12_000);
    let queued = admit_runner_provider_attempt_dispatch(&shared, &plan)
        .await
        .unwrap();
    let scan = |start: usize| {
        let shared = shared.clone();
        let user = user.clone();
        let tail = &socket;
        async move {
            let mut owners = (start..4095)
                .step_by(2)
                .map(|i| (format!("r{i}"), user.clone()))
                .collect::<Vec<_>>();
            let mut reads = 0;
            let mut found = std::collections::HashSet::new();
            let mut connection_ids = (start..4095)
                .step_by(2)
                .map(|i| format!("{user}{i}"))
                .collect::<Vec<_>>();
            if start == 1 {
                owners.push((tail.runner_id.as_str().to_owned(), tail.user_id.clone()));
                connection_ids.push(tail.edge_id.clone());
            }
            for batch in connection_ids.chunks(RECOVERY_OWNER_BATCH) {
                found.extend(pending_runner_owners(&shared, batch).await.unwrap());
                reads += 1;
            }
            assert_eq!(reads, 8);
            assert_eq!(found, owners.into_iter().collect());
        }
    };
    let started = std::time::Instant::now();
    tokio::join!(scan(0), scan(1));
    let discovery = started.elapsed();
    let candidates = list_runner_reconciliation(&shared, &socket, 8)
        .await
        .unwrap();
    assert_eq!(candidates, vec![queued.clone()]);
    let lease = claim_runner_delivery_with_transport(&shared, &socket, &queued.attempt, true)
        .await
        .unwrap()
        .unwrap();
    let claimed = authorize_runner_dispatch(&shared, &models, &socket, &queued, &lease.token)
        .await
        .unwrap()
        .unwrap();
    let RunnerDeliveryAction::Dispatch(issued) = claimed.action else {
        panic!("tail attempt must be dispatchable")
    };
    assert!(
        load_runner_request_custody(&shared, &socket, &issued, &claimed.token)
            .await
            .is_ok()
    );
    eprintln!(
        "4096 owners, 16 recovery reads in {discovery:?}; tail dispatch grant and request custody in {:?}",
        started.elapsed()
    );
    sqlx::query("DELETE FROM inference_provider_attempts WHERE user_id=?")
        .bind(&user)
        .execute(shared.get())
        .await
        .unwrap();
    sqlx::query("DELETE FROM edge_agent_registry WHERE user_id=?")
        .bind(&user)
        .execute(shared.get())
        .await
        .unwrap();
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

#[tokio::test]
#[ignore = "requires isolated MatrixOne: ASTRA_TEST_DB_IT=1"]
async fn large_inventory_keeps_exact_offerings_resolvable_across_runners() {
    let shared = common::setup_pool().await;
    let user = format!("device-{}", Uuid::new_v4());
    let mut published = Vec::new();
    for _ in 0..2 {
        let (connection, binding) = runner_fixture(&shared, &user).await;
        for revision in 1..129 {
            let mut definition = binding.definition.clone();
            definition.identity.binding_id = id(format!("device-model-{revision:03}"));
            let publication = RunnerInferenceBindingPublication {
                protocol_version: RUNNER_INFERENCE_PROTOCOL_VERSION,
                operation_id: id(Uuid::new_v4().to_string()),
                expected_publication_revision: revision,
                change: RunnerInferenceBindingChange::Publish {
                    definition: definition.clone(),
                },
            };
            publish_runner_binding(&shared, &connection, &publication)
                .await
                .unwrap();
            published.push((connection.clone(), definition.identity));
        }
        published.push((connection, binding.definition.identity));
    }
    let inventory = list_runner_model_catalog_bindings(&shared, &user)
        .await
        .unwrap();
    assert!(inventory.truncated);
    assert_eq!(inventory.bindings.len(), 256);
    let (connection, omitted) = published
        .into_iter()
        .find(|(_, identity)| {
            !inventory
                .bindings
                .iter()
                .any(|item| &item.definition.identity == identity)
        })
        .expect("at least one published Offering must be outside the catalog window");
    let offering = runner_offering_id(&user, &omitted);
    assert_eq!(
        resolve_runner_offering(&shared, &user, &offering)
            .await
            .unwrap()
            .definition
            .identity,
        omitted
    );
    assert!(
        resolve_runner_offering(&shared, "another-user", &offering)
            .await
            .is_err()
    );
    publish_runner_binding(
        &shared,
        &connection,
        &RunnerInferenceBindingPublication {
            protocol_version: RUNNER_INFERENCE_PROTOCOL_VERSION,
            operation_id: id(Uuid::new_v4().to_string()),
            expected_publication_revision: 129,
            change: RunnerInferenceBindingChange::Disable { identity: omitted },
        },
    )
    .await
    .unwrap();
    assert!(
        resolve_runner_offering(&shared, &user, &offering)
            .await
            .is_err()
    );
}
