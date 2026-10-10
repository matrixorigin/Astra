//! Production store/ledger boundary coverage. Live tests are opt-in:
//! ASTRA_TEST_DB_IT=1 cargo test -p astra-services --test collaborator_stage_db_it -- --ignored --test-threads=1
mod common;

use astra_core::SharedPool;
use astra_services::runs::{
    AtomicRunInteractionBatchRegistration, AtomicRunInteractionBatchRegistrationRequest,
    AtomicRunInteractionWaitRequest, COLLABORATOR_NATIVE_SESSION_METADATA_KEY,
    CollaboratorAssociation, CollaboratorExecutionBoundary, CollaboratorNativeExecutionLocator,
    CollaboratorNativeSession, CollaboratorProvider, CollaboratorStageAdmission,
    CollaboratorStageReceipt, CollaboratorStoreError, DatabaseRunStateStore,
    DurableRunInteractionKind, DurableRunInteractionResolveOutcome,
    DurableRunInteractionWaitOutcome, DurableRunRecord, ExecutorBindingRequest,
    ExecutorBindingRequestKind, ExecutorStatusRequest, InMemoryRunStateStore, RunStateStore,
    ToolInteractionAdmissionError, ToolInvocationInteractionOrigin, ToolTransportKindRequest,
    WorkspaceAuthorityRequest, WorkspaceBindingRequest, WorkspaceBindingRequestKind,
    WorkspaceSourceRequest,
};
use astra_services::tool_invocation_ledger::{
    DatabaseToolInvocationLedger, ToolInvocationDispatchAdmission,
};
use astra_turn_types::{
    DEFAULT_CONVERSATION_BRANCH_ID, DispatchCertainty, DurableToolReference, SessionKeyV1,
    ToolInvocationDecision, ToolInvocationFingerprint, ToolInvocationIdentity,
    ToolInvocationResultPayload, ToolInvocationState, ToolInvocationTerminalOutcome,
};
use serde_json::json;
use std::{collections::BTreeMap, time::Duration};
use uuid::Uuid;

fn record(user_id: &str, session_id: &str, parent: Option<&str>) -> DurableRunRecord {
    DurableRunRecord {
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
        checkpoint_version: None,
        checkpoint_json: None,
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

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1 and real MatrixOne"]
async fn terminal_presentation_survives_maximum_durable_observation_window() {
    let fixture = Fixture::new().await;
    let full = json!({
        "type": "tool_call_end", "run_id": fixture.parent_id,
        "session_id": fixture.session_id, "call_id": "large-call", "tool": "native_codex",
        "result": "界\\\"\n".repeat(12_000), "status": "completed", "success": true,
        "native_stage_observation": {
            "native_session_id": "thread", "native_turn_id": "turn", "dispatch_state": "acknowledged",
            "settlement_authoritative": true, "native_terminal": "completed",
            "stage_inclusive_input_tokens": 30, "stage_usage": {"cached_input_tokens": 10, "output_tokens": 5},
            "last_request_input_tokens": null, "model_context_window": null,
            "acknowledged_model": null, "provider_error_code": null, "provider_error_class": null
        }
    });
    let event = astra_services::runs::project_tool_terminal_presentation(full.clone(), 4096 - 128);
    assert!(serde_json::to_vec(&event).unwrap().len() <= 4096 - 128);
    assert_eq!(
        event["native_stage_observation"],
        full["native_stage_observation"]
    );
    let replay =
        astra_services::runs::project_tool_terminal_presentation(event.clone(), 4096 - 128);
    assert_eq!(
        replay, event,
        "replay must preserve original integrity facts"
    );
    fixture
        .store
        .append_event(
            &fixture.user_id,
            &fixture.session_id,
            &fixture.parent_id,
            event.clone(),
        )
        .await
        .unwrap();
    let observation = fixture
        .store
        .load_run_observation(&fixture.user_id, &fixture.parent_id, 256)
        .await
        .unwrap()
        .unwrap();
    // insert_run already records run_created; presentation adds exactly one
    // event, not a second observation ledger or auxiliary fact row.
    assert_eq!(observation.total_event_count, 2);
    assert_eq!(observation.run.events.len(), 2);
    let retained = observation
        .run
        .events
        .iter()
        .find(|event| event["call_id"] == "large-call")
        .unwrap();
    assert_eq!(retained["index"], 1);
    assert_eq!(retained["run_id"], fixture.parent_id);
    assert_eq!(retained["session_id"], fixture.session_id);
    assert_eq!(retained["call_id"], "large-call");
    assert_eq!(
        retained["native_stage_observation"],
        event["native_stage_observation"]
    );
    assert_eq!(retained["result_sha256"], event["result_sha256"]);
    assert!(
        fixture
            .store
            .load_run_observation("foreign-owner", &fixture.parent_id, 256)
            .await
            .unwrap()
            .is_none()
    );
    let initial = astra_services::runs::project_tool_terminal_presentation(
        json!({
            "type":"tool_call_end", "call_id":"lifecycle-call", "tool":"agent_fanout",
            "result":{
                "group_id":"group", "target_count":3, "results":[{"result":"x".repeat(10_000)}],
                "fanout":{"group_id":"group", "slots":[{"run_id":"child-a"},{"run_id":"child-b"},{"run_id":"r".repeat(512)}]},
                "work_unit_observation":{"id":"group", "kind":"agent_fanout", "status":"completed",
                    "revision":1, "mode":"current", "wake_policy":"none"}
            }
        }),
        4096 - 128,
    );
    let mut rebound = initial.clone();
    rebound["run_id"] = json!(fixture.parent_id);
    rebound["session_id"] = json!(fixture.session_id);
    rebound["run_generation"] = json!(1);
    rebound["idempotency_key"] = json!("subrun-tool:1:lifecycle-call");
    rebound["workspace"] = json!({"path":""});
    let padding =
        (4096 - 128 + 64usize).saturating_sub(serde_json::to_vec(&rebound).unwrap().len());
    rebound["workspace"]["path"] = json!("x".repeat(padding));
    assert!(serde_json::to_vec(&rebound).unwrap().len() > 4096 - 128);
    let rebound = astra_services::runs::project_tool_terminal_presentation(rebound, 4096 - 128);
    assert_eq!(rebound["result"]["group_id"], "group");
    assert_eq!(
        rebound["result"]["content_sha256"],
        initial["result"]["content_sha256"]
    );
    assert_eq!(
        rebound["result"]["original_bytes"],
        initial["result"]["original_bytes"]
    );
    assert_eq!(
        astra_services::runs::project_tool_terminal_presentation(rebound.clone(), 4096 - 128),
        rebound
    );
    fixture
        .store
        .append_event(
            &fixture.user_id,
            &fixture.session_id,
            &fixture.parent_id,
            rebound.clone(),
        )
        .await
        .unwrap();
    let observation = fixture
        .store
        .load_run_observation(&fixture.user_id, &fixture.parent_id, 256)
        .await
        .unwrap()
        .unwrap();
    let retained = observation
        .run
        .events
        .iter()
        .find(|event| event["call_id"] == "lifecycle-call")
        .unwrap();
    assert_eq!(retained["result"], rebound["result"]);
    assert_eq!(retained["result_sha256"], initial["result_sha256"]);
    assert_eq!(retained["run_generation"], 1);
    fixture.cleanup().await;
}

fn admission(anchor_run_id: &str, provider: CollaboratorProvider) -> CollaboratorStageAdmission {
    let native_execution = (provider != CollaboratorProvider::InternalModel).then(|| {
        let tool_name = match provider {
            CollaboratorProvider::Claude => "native_claude",
            CollaboratorProvider::OpenCode => "native_opencode",
            CollaboratorProvider::Codex => "native_codex",
            CollaboratorProvider::InternalModel => unreachable!(),
        };
        CollaboratorNativeExecutionLocator {
            descriptor: astra_turn_types::ResolvedToolDescriptorRef::new(
                astra_turn_types::ToolIdentity::new(
                    astra_turn_types::ProviderBindingRef::new("selected-runner-provider").unwrap(),
                    astra_turn_types::NativeToolId::new(tool_name).unwrap(),
                ),
                "native-stage-v1",
            )
            .unwrap(),
            public_tool_name: tool_name.into(),
            requested_model: Some("explicit-native-model".into()),
            policy_content_id: "admitted-policy-content-id".into(),
        }
    });
    let execution_boundary = if provider == CollaboratorProvider::InternalModel {
        CollaboratorExecutionBoundary::ServerManaged
    } else {
        CollaboratorExecutionBoundary::UserRunner {
            runner_id: "selected-runner".into(),
        }
    };
    CollaboratorStageAdmission {
        anchor_run_id: anchor_run_id.into(),
        source_message_id: Uuid::new_v4().to_string(),
        request_fingerprint: "immutable-source-intent-digest".into(),
        execution_identity_fingerprint: "prepared-identity-digest".into(),
        native_execution: native_execution.clone(),
        expected_previous_stage_run_id: None,
        expected_parent_generation: 1,
        association: CollaboratorAssociation {
            provider,
            execution_boundary,
        },
    }
}

struct Fixture {
    pool: SharedPool,
    store: DatabaseRunStateStore,
    user_id: String,
    session_id: String,
    parent_id: String,
    owner: String,
}

impl Fixture {
    fn physical_workspace_id(&self) -> String {
        astra_services::SessionExecutionBindingV1::edge_materialization_physical_identity(
            "fixture-materialization",
            "/selected-runner",
        )
    }

    async fn cleanup(&self) {
        // This fixture owns a freshly generated user. Never widen cleanup to
        // other users, even when test binaries share the same database.
        for table in [
            "tool_invocation_ledger",
            "tool_invocation_archive_chunks",
            "edge_pending_dispatch",
        ] {
            sqlx::query(&format!(
                "DELETE FROM {table} WHERE user_id = ? AND session_id = ?"
            ))
            .bind(&self.user_id)
            .bind(&self.session_id)
            .execute(self.pool.get())
            .await
            .unwrap();
        }
        sqlx::query(
            "DELETE FROM session_execution_bindings WHERE owner_user_id = ? AND session_id = ?",
        )
        .bind(&self.user_id)
        .bind(&self.session_id)
        .execute(self.pool.get())
        .await
        .unwrap();
        common::cleanup_work_owner(&self.pool, &self.user_id).await;
    }
    async fn new() -> Self {
        let (pool, _) = common::setup_pool_and_settings().await;
        let user_id = Uuid::new_v4().to_string();
        let session_id = Uuid::new_v4().to_string();
        let owner = Uuid::new_v4().to_string();
        let store = DatabaseRunStateStore::new(pool.clone())
            .with_owner_pod_id(&owner)
            .with_lease_ttl(Duration::from_secs(300));
        sqlx::query("INSERT INTO agent_sessions (user_id, session_id, status, created_at, updated_at, last_active_at)
                     VALUES (?, ?, 'active', NOW(6), NOW(6), NOW(6))")
            .bind(&user_id).bind(&session_id).execute(pool.get()).await.unwrap();
        let parent = record(&user_id, &session_id, None);
        let parent_id = parent.run_id.clone();
        store.insert_run(parent).await.unwrap();
        Self {
            pool,
            store,
            user_id,
            session_id,
            parent_id,
            owner,
        }
    }

    fn child(&self) -> DurableRunRecord {
        record(&self.user_id, &self.session_id, Some(&self.parent_id))
    }

    async fn first(
        &self,
        provider: CollaboratorProvider,
    ) -> (CollaboratorStageAdmission, CollaboratorStageReceipt) {
        let child = self.child();
        let request = admission(&child.run_id, provider);
        let receipt = self
            .store
            .insert_run_with_collaborator_stage(child, request.clone(), None)
            .await
            .unwrap();
        assert!(!receipt.replayed);
        (request, receipt)
    }

    async fn settle(&self, run_id: &str) {
        self.settle_as(run_id, "completed").await;
    }

    async fn settle_as(&self, run_id: &str, status: &str) {
        assert!(
            self.store
                .update_run_status_with_events_if_current(
                    &self.user_id,
                    &self.session_id,
                    run_id,
                    &["running"],
                    Some(1),
                    status,
                    None,
                    None,
                    &[json!({"event_type": "turn_completed"})],
                )
                .await
                .unwrap()
        );
    }

    async fn invocation(
        &self,
        run_id: &str,
    ) -> (DatabaseToolInvocationLedger, ToolInvocationIdentity) {
        let identity = ToolInvocationIdentity::new(
            &self.user_id,
            &self.session_id,
            run_id,
            "native-session-stage",
            Uuid::new_v4().to_string(),
        )
        .unwrap();
        let ledger = DatabaseToolInvocationLedger::new(self.pool.clone());
        let decision = ToolInvocationDecision::new(
            &json!({"route":"user_runner", "runner_id":"selected-runner"}),
        )
        .unwrap();
        let association = self
            .store
            .load_collaborator_association(&self.user_id, &self.session_id, run_id)
            .await
            .unwrap()
            .unwrap();
        let tool = association
            .latest_native_execution
            .map(|locator| DurableToolReference::Provider {
                descriptor: locator.descriptor,
            })
            .unwrap_or_else(|| DurableToolReference::built_in("bash", "registry-v1").unwrap());
        let fingerprint = ToolInvocationFingerprint::new(
            tool,
            &json!({"command":"native-session-create"}),
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
                "native-session-worker",
                90_000,
                ToolInvocationDispatchAdmission {
                    expected_control_epoch: -1,
                    expected_owner_generation: 1,
                    expected_owner_pod_id: self.owner.clone(),
                    expected_execution_binding_generation: None,
                },
            )
            .await
            .unwrap();
        (ledger, identity)
    }

    // Real public owners create every executable fact; tests never hand-insert
    // an interaction, invocation row, action grant, or Edge dispatch status.
    async fn interaction_invocation(
        &self,
    ) -> (DatabaseToolInvocationLedger, ToolInvocationIdentity) {
        use astra_services::multi_agent::{
            DatabaseEdgeDispatchService, EdgeDispatchIdentity, EdgeDispatchService,
        };
        use astra_services::{DatabaseSessionContextCoordinator, SessionExecutionBindingV1};
        let key = SessionKeyV1::owner_session(
            "server",
            &self.user_id,
            &self.session_id,
            DEFAULT_CONVERSATION_BRANCH_ID,
        );
        let mut binding = SessionExecutionBindingV1::server_work_default("native-test-workspace");
        binding.physical_workspace_id = Some(
            SessionExecutionBindingV1::edge_materialization_physical_identity(
                "fixture-materialization",
                "/selected-runner",
            ),
        );
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
        DatabaseSessionContextCoordinator::new(self.pool.clone())
            .load_or_initialize_execution_binding(&key, &binding)
            .await
            .unwrap();
        let mut child = self.child();
        child.events = vec![json!({"event_type":"run_started", "data":{
            "execution_binding_generation": binding.generation,
            "workspace": {"kind":"edge_workspace", "cwd":"/selected-runner", "authority":"read_write"},
            "executor": {"kind":"edge_agent", "executor_id":"selected-runner", "transport":"edge_ws"}
        }})];
        let stage = admission(&child.run_id, CollaboratorProvider::Codex);
        let descriptor = stage.native_execution.as_ref().unwrap().descriptor.clone();
        let receipt = self
            .store
            .insert_run_with_collaborator_stage(child, stage, None)
            .await
            .unwrap();
        let run = self
            .store
            .load_run(&self.user_id, &receipt.run_id)
            .await
            .unwrap()
            .unwrap();
        let identity = ToolInvocationIdentity::new(
            &self.user_id,
            &self.session_id,
            &receipt.run_id,
            "native-turn",
            "native-stage",
        )
        .unwrap();
        let ledger = DatabaseToolInvocationLedger::new(self.pool.clone());
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
                    expected_owner_pod_id: self.owner.clone(),
                    expected_execution_binding_generation: Some(binding.generation),
                },
            )
            .await
            .unwrap();
        let dispatch = DatabaseEdgeDispatchService::from_shared(&self.pool);
        let dispatch_identity = EdgeDispatchIdentity::new(
            &self.user_id,
            &self.session_id,
            &identity.run_id,
            &identity.turn_chain_id,
            identity.storage_key(),
        );
        assert!(matches!(dispatch.admit_and_claim_direct_dispatch(&dispatch_identity, "selected-runner", &json!({
            "identity":identity, "request_id":identity.storage_key(), "tool":"native_codex", "args":{"task":"native interaction"}
        }).to_string()).await.unwrap(), astra_services::multi_agent::EdgeDirectDispatchAdmission::Claimed));
        (ledger, identity)
    }
}

fn interaction_event(
    origin: &ToolInvocationInteractionOrigin,
    native_id: serde_json::Value,
) -> serde_json::Value {
    json!({"event_type":"provider_interaction_required", "idempotency_key":"server-provider-interaction-required:native-question", "data":{
        "request_id":"native-question", "session_id":origin.identity.session_id, "run_id":origin.identity.run_id,
        "tool_invocation_origin":origin,
        "interaction":{"request_id":"native-question", "payload":{"native_request_id":native_id, "question":"Continue?"}, "timeout_ms":60000},
        "delivery":"durable", "timeout_ms":60000
    }})
}

fn interaction_event_with_stage_input(
    origin: &ToolInvocationInteractionOrigin,
    native_id: serde_json::Value,
    stage_input_id: &str,
) -> serde_json::Value {
    let mut event = interaction_event(origin, native_id);
    event["data"]["interaction"]["provider_stage_input_id"] = json!(stage_input_id);
    event
}

async fn register_interaction(
    store: &DatabaseRunStateStore,
    origin: &ToolInvocationInteractionOrigin,
    event: &serde_json::Value,
) -> Result<AtomicRunInteractionBatchRegistration, String> {
    eprintln!("native interaction: register start");
    let started = std::time::Instant::now();
    let result = store
        .register_guarded_interaction_batch(AtomicRunInteractionBatchRegistrationRequest {
            user_id: &origin.identity.user_id,
            run_id: &origin.identity.run_id,
            expected_session_id: &origin.identity.session_id,
            expected_control_epoch: origin.control_epoch,
            expected_owner_generation: origin.owner_generation,
            events: std::slice::from_ref(event),
        })
        .await;
    eprintln!(
        "native interaction: register complete {:?}",
        started.elapsed()
    );
    result
}

async fn begin_interaction(
    store: &DatabaseRunStateStore,
    origin: &ToolInvocationInteractionOrigin,
) -> Result<DurableRunInteractionWaitOutcome, String> {
    eprintln!("native interaction: begin wait start");
    let started = std::time::Instant::now();
    let result = store
        .begin_run_interaction_wait(AtomicRunInteractionWaitRequest {
            user_id: &origin.identity.user_id,
            run_id: &origin.identity.run_id,
            expected_session_id: &origin.identity.session_id,
            request_id: "native-question",
            kind: DurableRunInteractionKind::Provider,
            expected_control_epoch: origin.control_epoch,
            expected_owner_generation: origin.owner_generation,
        })
        .await;
    eprintln!(
        "native interaction: begin wait complete {:?}",
        started.elapsed()
    );
    result
}

async fn resolve_interaction(
    store: &DatabaseRunStateStore,
    origin: &ToolInvocationInteractionOrigin,
    response: serde_json::Value,
) -> DurableRunInteractionResolveOutcome {
    eprintln!("native interaction: resolve start");
    let started = std::time::Instant::now();
    let result = store
        .resolve_run_interaction(
            &origin.identity.user_id,
            &origin.identity.session_id,
            &origin.identity.run_id,
            "native-question",
            DurableRunInteractionKind::Provider,
            response,
        )
        .await
        .unwrap();
    eprintln!(
        "native interaction: resolve complete {:?}",
        started.elapsed()
    );
    result
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1 and real MatrixOne"]
async fn native_interaction_real_producer_register_begin_resolve_and_early_answer() {
    for (early, native_id) in [(false, json!(7)), (true, json!("7"))] {
        let fixture = Fixture::new().await;
        let (ledger, identity) = fixture.interaction_invocation().await;
        let origin = fixture
            .store
            .derive_tool_interaction_origin(
                &identity,
                "selected-runner",
                &fixture.physical_workspace_id(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(origin.owner_generation, 1);
        assert_eq!(origin.execution_binding_generation, 1);
        // A different server handles callbacks; it must not acquire or replace
        // the execution owner merely to register/wait/answer an interaction.
        let callback =
            DatabaseRunStateStore::new(fixture.pool.clone()).with_owner_pod_id("callback-node");
        let event = interaction_event(&origin, native_id.clone());
        assert_eq!(
            register_interaction(&callback, &origin, &event)
                .await
                .unwrap(),
            AtomicRunInteractionBatchRegistration::Registered
        );
        assert_eq!(
            register_interaction(&callback, &origin, &event)
                .await
                .unwrap(),
            AtomicRunInteractionBatchRegistration::Registered
        );
        let response = json!({"request_id":"native-question", "outcome":"submitted", "payload":{"answer":"yes"}});
        if early {
            assert!(matches!(
                resolve_interaction(&callback, &origin, response.clone()).await,
                DurableRunInteractionResolveOutcome::Queued(_)
            ));
            assert!(matches!(
                begin_interaction(&callback, &origin).await.unwrap(),
                DurableRunInteractionWaitOutcome::AlreadyResolved(_)
            ));
        } else {
            assert_eq!(
                begin_interaction(&callback, &origin).await.unwrap(),
                DurableRunInteractionWaitOutcome::Waiting
            );
            let waiting = fixture
                .store
                .load_run(&fixture.user_id, &identity.run_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(waiting.status, "waiting");
            assert_eq!(waiting.waiting_for.as_deref(), Some("provider_interaction"));
            assert!(matches!(
                resolve_interaction(&callback, &origin, response.clone()).await,
                DurableRunInteractionResolveOutcome::Resolved(_)
            ));
        }
        let resumed = fixture
            .store
            .load_run(&fixture.user_id, &identity.run_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resumed.status, "running");
        assert_eq!(
            resumed.owner_pod_id.as_deref(),
            Some(fixture.owner.as_str())
        );
        assert_eq!(resumed.run_generation, 1);
        assert_eq!(
            ledger.get(&identity).await.unwrap().unwrap().state,
            ToolInvocationState::Dispatched
        );
        let required = fixture
            .store
            .load_run_interaction_event(
                &fixture.user_id,
                &identity.run_id,
                "native-question",
                "provider_interaction_required",
            )
            .await
            .unwrap()
            .unwrap();
        assert!(
            required
                .get("index")
                .and_then(serde_json::Value::as_u64)
                .is_some(),
            "native ACK must use the committed required event cursor"
        );
        assert_eq!(
            required["data"]["interaction"]["payload"]["native_request_id"],
            native_id
        );
        assert!(matches!(
            resolve_interaction(&callback, &origin, response).await,
            DurableRunInteractionResolveOutcome::Idempotent(_)
        ));
        assert!(matches!(resolve_interaction(&callback, &origin, json!({"request_id":"native-question", "outcome":"submitted", "payload":{"answer":"no"}})).await, DurableRunInteractionResolveOutcome::Conflict(_)));
        fixture.cleanup().await;
    }
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1 and real MatrixOne"]
async fn native_interaction_provisional_stage_fence_does_not_consume_unacknowledged_guidance() {
    let fixture = Fixture::new().await;
    let (_ledger, identity) = fixture.interaction_invocation().await;
    let intent_id = "guidance-before-provider-ack";
    fixture
        .store
        .append_event(
            &fixture.user_id,
            &fixture.session_id,
            &identity.run_id,
            json!({
                "event_type": "user_intent",
                "idempotency_key": format!("user_intent:{intent_id}"),
                "data": {
                    "intent_id": intent_id,
                    "delivery": "guide_current_run",
                    "input": {"content": "continue with the review"}
                }
            }),
        )
        .await
        .unwrap();
    let source = fixture
        .store
        .load_run_event_by_idempotency_key(
            &fixture.user_id,
            &identity.run_id,
            "user_intent",
            &format!("user_intent:{intent_id}"),
        )
        .await
        .unwrap()
        .unwrap();
    let source_index = source["index"].as_i64().unwrap();
    let provisional = fixture
        .store
        .derive_tool_interaction_origin(
            &identity,
            "selected-runner",
            &fixture.physical_workspace_id(),
            Some(intent_id),
        )
        .await
        .unwrap();
    assert_eq!(source_index, provisional.control_epoch);
    let event = interaction_event_with_stage_input(&provisional, json!(7), intent_id);

    assert_eq!(
        register_interaction(&fixture.store, &provisional, &event)
            .await
            .unwrap(),
        AtomicRunInteractionBatchRegistration::Registered
    );
    assert_eq!(
        begin_interaction(&fixture.store, &provisional)
            .await
            .unwrap(),
        DurableRunInteractionWaitOutcome::Waiting
    );

    let durable = fixture
        .store
        .load_run(&fixture.user_id, &identity.run_id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        durable.events.iter().all(|event| {
            event.get("event_type").and_then(|value| value.as_str()) != Some("user_intent_applied")
        }),
        "registering a pre-ACK provider question must not consume guidance"
    );
    fixture.cleanup().await;
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1 and real MatrixOne"]
async fn native_interaction_real_producer_accepts_reconnect_and_rejects_wrong_workspace() {
    let fixture = Fixture::new().await;
    let (_ledger, identity) = fixture.interaction_invocation().await;
    assert!(matches!(
        fixture
            .store
            .derive_tool_interaction_origin(&identity, "other-runner", "other-physical", None)
            .await,
        Err(ToolInteractionAdmissionError::Unproven)
    ));
    let origin = fixture
        .store
        .derive_tool_interaction_origin(
            &identity,
            "reconnected-runner",
            &fixture.physical_workspace_id(),
            None,
        )
        .await
        .unwrap();
    let before = fixture
        .store
        .load_run(&fixture.user_id, &identity.run_id)
        .await
        .unwrap()
        .unwrap();
    for field in [
        "user",
        "session",
        "run",
        "invocation",
        "binding",
        "epoch",
        "descriptor",
        "physical_workspace",
        "owner",
    ] {
        let mut forged = origin.clone();
        match field {
            "user" => forged.identity.user_id = Uuid::new_v4().to_string(),
            "session" => forged.identity.session_id = Uuid::new_v4().to_string(),
            "run" => forged.identity.run_id = Uuid::new_v4().to_string(),
            "invocation" => forged.identity.invocation_id = Uuid::new_v4().to_string(),
            "binding" => forged.execution_binding_generation += 1,
            "epoch" => forged.control_epoch += 1,
            "descriptor" => {
                forged.descriptor = admission("other", CollaboratorProvider::Claude)
                    .native_execution
                    .unwrap()
                    .descriptor
            }
            "physical_workspace" => forged.physical_workspace_id = "other-physical".into(),
            "owner" => forged.owner_generation += 1,
            _ => unreachable!(),
        }
        // Keep the authoritative target: only the persisted origin is forged.
        // Changing the target too would merely test lookup of a missing Run.
        assert!(
            register_interaction(
                &fixture.store,
                &origin,
                &interaction_event(&forged, json!(7))
            )
            .await
            .is_err(),
            "{field}"
        );
        assert!(
            fixture
                .store
                .load_run_interaction_event(
                    &fixture.user_id,
                    &identity.run_id,
                    "native-question",
                    "provider_interaction_required"
                )
                .await
                .unwrap()
                .is_none(),
            "{field}"
        );
        let after = fixture
            .store
            .load_run(&fixture.user_id, &identity.run_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                after.status,
                after.last_event_idx,
                after.run_generation,
                after.owner_pod_id
            ),
            (
                before.status.clone(),
                before.last_event_idx,
                before.run_generation,
                before.owner_pod_id.clone()
            ),
            "{field}"
        );
    }
    // Rejection must not poison admission of the authoritative producer facts.
    assert_eq!(
        register_interaction(
            &fixture.store,
            &origin,
            &interaction_event(&origin, json!(7))
        )
        .await
        .unwrap(),
        AtomicRunInteractionBatchRegistration::Registered
    );
    fixture.cleanup().await;
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1 and real MatrixOne"]
async fn native_interaction_real_producer_unknown_dispatch_fences_answer() {
    let fixture = Fixture::new().await;
    let (ledger, identity) = fixture.interaction_invocation().await;
    let origin = fixture
        .store
        .derive_tool_interaction_origin(
            &identity,
            "selected-runner",
            &fixture.physical_workspace_id(),
            None,
        )
        .await
        .unwrap();
    register_interaction(
        &fixture.store,
        &origin,
        &interaction_event(&origin, json!(7)),
    )
    .await
    .unwrap();
    assert_eq!(
        begin_interaction(&fixture.store, &origin).await.unwrap(),
        DurableRunInteractionWaitOutcome::Waiting
    );
    ledger
        .mark_outcome_unknown(&identity, "native-interaction-worker")
        .await
        .unwrap();
    assert!(matches!(
        fixture
            .store
            .derive_tool_interaction_origin(
                &identity,
                "selected-runner",
                &fixture.physical_workspace_id(),
                None,
            )
            .await,
        Err(ToolInteractionAdmissionError::Unproven)
    ));
    assert!(matches!(resolve_interaction(&fixture.store, &origin,
        json!({"request_id":"native-question", "outcome":"submitted", "payload":{"answer":"yes"}})).await,
        DurableRunInteractionResolveOutcome::NoLongerWaiting));
    let run = fixture
        .store
        .load_run(&fixture.user_id, &identity.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(run.status, "waiting");
    assert_eq!(run.run_generation, origin.owner_generation);
    assert_eq!(
        ledger.get(&identity).await.unwrap().unwrap().state,
        ToolInvocationState::OutcomeUnknown
    );
    fixture.cleanup().await;
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1 and real MatrixOne"]
async fn native_interaction_real_producer_settlement_lease_and_cancel_fence_answer() {
    for fence in ["settled", "dispatch_lease", "owner_lease", "cancelled"] {
        let fixture = Fixture::new().await;
        let (ledger, identity) = fixture.interaction_invocation().await;
        let origin = fixture
            .store
            .derive_tool_interaction_origin(
                &identity,
                "selected-runner",
                &fixture.physical_workspace_id(),
                None,
            )
            .await
            .unwrap();
        register_interaction(
            &fixture.store,
            &origin,
            &interaction_event(&origin, json!(7)),
        )
        .await
        .unwrap();
        assert_eq!(
            begin_interaction(&fixture.store, &origin).await.unwrap(),
            DurableRunInteractionWaitOutcome::Waiting
        );
        match fence {
            "settled" => {
                let outcome = ToolInvocationTerminalOutcome::Succeeded {
                    result: ToolInvocationResultPayload::new("done", BTreeMap::new(), None)
                        .unwrap(),
                };
                ledger
                    .compare_and_complete(
                        &identity,
                        ToolInvocationState::Dispatched,
                        Some("native-interaction-worker"),
                        &outcome,
                    )
                    .await
                    .unwrap();
            }
            "cancelled" => {
                assert!(
                    fixture
                        .store
                        .request_run_cancellation(&fixture.user_id, &identity.run_id)
                        .await
                        .unwrap()
                );
            }
            "dispatch_lease" => {
                // Deterministic clock-expiry fixture, not a fabricated producer fact.
                let result = sqlx::query("UPDATE tool_invocation_ledger SET dispatch_lease_expires_at = TIMESTAMPADD(SECOND, -1, NOW(6)) WHERE user_id = ? AND session_id = ? AND run_id = ? AND turn_chain_id = ? AND invocation_id = ?")
                    .bind(&identity.user_id).bind(&identity.session_id).bind(&identity.run_id)
                    .bind(&identity.turn_chain_id).bind(&identity.invocation_id).execute(fixture.pool.get()).await.unwrap();
                assert_eq!(result.rows_affected(), 1);
            }
            "owner_lease" => {
                let result = sqlx::query("UPDATE agent_runs SET owner_lease_expires_at = TIMESTAMPADD(SECOND, -1, NOW(6)) WHERE user_id = ? AND session_id = ? AND run_id = ?")
                    .bind(&identity.user_id).bind(&identity.session_id).bind(&identity.run_id)
                    .execute(fixture.pool.get()).await.unwrap();
                assert_eq!(result.rows_affected(), 1);
            }
            _ => unreachable!(),
        }
        let before = fixture
            .store
            .load_run(&fixture.user_id, &identity.run_id)
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(
                fixture
                    .store
                    .derive_tool_interaction_origin(
                        &identity,
                        "selected-runner",
                        &fixture.physical_workspace_id(),
                        None,
                    )
                    .await,
                Err(ToolInteractionAdmissionError::Unproven)
            ),
            "{fence}"
        );
        assert!(matches!(resolve_interaction(&fixture.store, &origin,
            json!({"request_id":"native-question", "outcome":"submitted", "payload":{"answer":"yes"}})).await,
            DurableRunInteractionResolveOutcome::NoLongerWaiting), "{fence}");
        let after = fixture
            .store
            .load_run(&fixture.user_id, &identity.run_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                after.status,
                after.last_event_idx,
                after.run_generation,
                after.owner_pod_id
            ),
            (
                before.status,
                before.last_event_idx,
                before.run_generation,
                before.owner_pod_id
            ),
            "{fence}"
        );
        fixture.cleanup().await;
    }
}

#[test]
fn receipt_wire_is_observation_not_execution_authority_and_all_providers_roundtrip() {
    for provider in [
        CollaboratorProvider::InternalModel,
        CollaboratorProvider::Claude,
        CollaboratorProvider::OpenCode,
        CollaboratorProvider::Codex,
    ] {
        let request = admission("anchor", provider);
        assert_eq!(
            serde_json::from_value::<CollaboratorStageAdmission>(
                serde_json::to_value(&request).unwrap()
            )
            .unwrap(),
            request
        );
    }
    let receipt = CollaboratorStageReceipt {
        anchor_run_id: "anchor".into(),
        source_message_id: "source".into(),
        run_id: "original-child".into(),
        owner_generation: 1,
        replayed: true,
    };
    assert!(receipt.replayed);
    assert!(
        serde_json::from_value::<CollaboratorStageReceipt>(serde_json::to_value(&receipt).unwrap())
            .unwrap()
            .replayed
    );
    // A reconstructed durable receipt is not a dispatch grant. The store
    // explicitly marks source replay; runtime must reconcile the original ID.
    assert_eq!(receipt.run_id, "original-child");
}

#[tokio::test]
async fn memory_store_does_not_invent_durable_ledger_or_session_evidence() {
    let store = InMemoryRunStateStore::new();
    let child = record("user", "session", Some("parent"));
    let request = admission(&child.run_id, CollaboratorProvider::Codex);
    assert!(matches!(
        store
            .insert_run_with_collaborator_stage(child, request, None)
            .await,
        Err(CollaboratorStoreError::Unsupported)
    ));
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn source_replay_survives_store_recreation_and_does_not_create_candidate() {
    let fixture = Fixture::new().await;
    let (request, first) = fixture.first(CollaboratorProvider::InternalModel).await;
    let reopened =
        DatabaseRunStateStore::new(fixture.pool.clone()).with_owner_pod_id(&fixture.owner);
    let candidate = fixture.child();
    let candidate_id = candidate.run_id.clone();
    let replay = reopened
        .insert_run_with_collaborator_stage(candidate, request.clone(), None)
        .await
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.run_id, first.run_id);
    assert!(
        reopened
            .load_run(&fixture.user_id, &candidate_id)
            .await
            .unwrap()
            .is_none()
    );
    let mut conflict = request;
    conflict.request_fingerprint = "changed-intent".into();
    assert!(matches!(
        reopened
            .insert_run_with_collaborator_stage(fixture.child(), conflict, None)
            .await,
        Err(CollaboratorStoreError::SourceMessageConflict { .. })
    ));
    let association = reopened
        .load_collaborator_association(&fixture.user_id, &fixture.session_id, &first.anchor_run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(association.latest_stage.run_id, first.run_id);
    assert!(association.native_session.is_none());
    fixture.cleanup().await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn concurrent_next_stages_have_one_winner_and_preserve_terminal_anchor() {
    let fixture = Fixture::new().await;
    let (mut request, first) = fixture.first(CollaboratorProvider::InternalModel).await;
    request.expected_previous_stage_run_id = Some(first.run_id.clone());
    request.source_message_id = Uuid::new_v4().to_string();
    assert!(matches!(
        fixture
            .store
            .insert_run_with_collaborator_stage(fixture.child(), request.clone(), None)
            .await,
        Err(CollaboratorStoreError::PreviousStageActive { .. })
    ));
    fixture.settle(&first.run_id).await;
    let before = fixture
        .store
        .load_run(&fixture.user_id, &first.run_id)
        .await
        .unwrap()
        .unwrap();
    let other_store =
        DatabaseRunStateStore::new(fixture.pool.clone()).with_owner_pod_id(&fixture.owner);
    let mut other_request = request.clone();
    other_request.source_message_id = Uuid::new_v4().to_string();
    let (left, right) = tokio::join!(
        fixture
            .store
            .insert_run_with_collaborator_stage(fixture.child(), request, None),
        other_store.insert_run_with_collaborator_stage(fixture.child(), other_request, None),
    );
    assert!(matches!(
        (&left, &right),
        (
            Ok(_),
            Err(CollaboratorStoreError::PreviousStageChanged { .. })
        ) | (
            Err(CollaboratorStoreError::PreviousStageChanged { .. }),
            Ok(_)
        )
    ));
    let after = fixture
        .store
        .load_run(&fixture.user_id, &first.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.status, "completed");
    assert_eq!(after.checkpoint_json, before.checkpoint_json);
    assert_eq!(after.run_generation, before.run_generation);
    assert_eq!(after.last_event_idx, before.last_event_idx + 1);
    fixture.cleanup().await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn native_binding_uses_real_ledger_terminal_outcome_for_each_external_provider() {
    for provider in [
        CollaboratorProvider::Claude,
        CollaboratorProvider::OpenCode,
        CollaboratorProvider::Codex,
    ] {
        for task_succeeded in [true, false] {
            let fixture = Fixture::new().await;
            let (mut request, first) = fixture.first(provider.clone()).await;
            let native = CollaboratorNativeSession {
                anchor_run_id: first.anchor_run_id.clone(),
                provider: provider.clone(),
                native_session_id: Uuid::new_v4().to_string(),
            };
            let (ledger, identity) = fixture.invocation(&first.run_id).await;
            assert!(matches!(
                fixture
                    .store
                    .confirm_collaborator_native_session(&identity, &native, 1)
                    .await,
                Err(CollaboratorStoreError::NativeSessionUnproven)
            ));
            let mut metadata = BTreeMap::new();
            metadata.insert(
                COLLABORATOR_NATIVE_SESSION_METADATA_KEY.into(),
                serde_json::to_value(&native).unwrap(),
            );
            let result =
                ToolInvocationResultPayload::new("session acknowledged", metadata, None).unwrap();
            let outcome = if task_succeeded {
                ToolInvocationTerminalOutcome::Succeeded { result }
            } else {
                ToolInvocationTerminalOutcome::Failed {
                    result,
                    error_kind: Some("provider_quota_exceeded".into()),
                    retryable: false,
                }
            };
            if !task_succeeded {
                ledger
                    .mark_outcome_unknown(&identity, "native-session-worker")
                    .await
                    .unwrap();
                assert!(matches!(
                    fixture
                        .store
                        .confirm_collaborator_native_session(&identity, &native, 1)
                        .await,
                    Err(CollaboratorStoreError::NativeSessionUnproven)
                ));
                assert!(
                    fixture
                        .store
                        .load_collaborator_association(
                            &fixture.user_id,
                            &fixture.session_id,
                            &first.anchor_run_id
                        )
                        .await
                        .unwrap()
                        .unwrap()
                        .native_session
                        .is_none(),
                    "unknown dispatch cannot establish a native session"
                );
            }
            let completed = ledger
                .compare_and_complete(
                    &identity,
                    if task_succeeded {
                        ToolInvocationState::Dispatched
                    } else {
                        ToolInvocationState::OutcomeUnknown
                    },
                    task_succeeded.then_some("native-session-worker"),
                    &outcome,
                )
                .await
                .unwrap();
            assert_eq!(
                completed.state,
                if task_succeeded {
                    ToolInvocationState::Succeeded
                } else {
                    ToolInvocationState::Failed
                }
            );
            assert_eq!(completed.dispatch_certainty, DispatchCertainty::Dispatched);
            assert!(matches!(
                fixture
                    .store
                    .confirm_collaborator_native_session(&identity, &native, 2)
                    .await,
                Err(CollaboratorStoreError::ParentNotCurrent { .. })
            ));
            fixture
                .store
                .confirm_collaborator_native_session(&identity, &native, 1)
                .await
                .unwrap();
            let mut unrelated = identity.clone();
            unrelated.invocation_id = Uuid::new_v4().to_string();
            assert!(matches!(
                fixture
                    .store
                    .confirm_collaborator_native_session(&unrelated, &native, 1)
                    .await,
                Err(CollaboratorStoreError::NativeSessionUnproven)
            ));
            fixture
                .store
                .confirm_collaborator_native_session(&identity, &native, 1)
                .await
                .unwrap();
            let reopened =
                DatabaseRunStateStore::new(fixture.pool.clone()).with_owner_pod_id(&fixture.owner);
            let association = reopened
                .load_collaborator_association(
                    &fixture.user_id,
                    &fixture.session_id,
                    &first.anchor_run_id,
                )
                .await
                .unwrap()
                .unwrap();
            assert_eq!(association.native_session, Some(native));
            assert_eq!(
                association.latest_native_execution, request.native_execution,
                "reopening the owner preserves the native model and descriptor, not only their digest"
            );
            fixture
                .settle_as(
                    &first.run_id,
                    if task_succeeded {
                        "completed"
                    } else {
                        "failed"
                    },
                )
                .await;
            request.expected_previous_stage_run_id = Some(first.run_id);
            request.source_message_id = Uuid::new_v4().to_string();
            let next = reopened
                .insert_run_with_collaborator_stage(fixture.child(), request, None)
                .await
                .unwrap();
            assert!(!next.replayed);
            assert_ne!(next.run_id, next.anchor_run_id);
            fixture.cleanup().await;
        }
    }
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn outcome_unknown_and_dispatched_previous_stages_fence_admission() {
    let fixture = Fixture::new().await;
    let (mut request, first) = fixture.first(CollaboratorProvider::InternalModel).await;
    let (ledger, identity) = fixture.invocation(&first.run_id).await;
    fixture.settle(&first.run_id).await;
    request.expected_previous_stage_run_id = Some(first.run_id);
    request.source_message_id = Uuid::new_v4().to_string();
    for unknown in [false, true] {
        if unknown {
            ledger
                .mark_outcome_unknown(&identity, "native-session-worker")
                .await
                .unwrap();
        }
        assert!(matches!(
            fixture
                .store
                .insert_run_with_collaborator_stage(fixture.child(), request.clone(), None)
                .await,
            Err(CollaboratorStoreError::UnsettledDispatch { .. })
        ));
    }
    fixture.cleanup().await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn tenant_generation_cancel_and_failed_insert_do_not_publish_association() {
    let fixture = Fixture::new().await;
    let child = fixture.child();
    let mut request = admission(&child.run_id, CollaboratorProvider::InternalModel);
    let mut foreign = child.clone();
    foreign.user_id = Uuid::new_v4().to_string();
    assert!(matches!(
        fixture
            .store
            .insert_run_with_collaborator_stage(foreign, request.clone(), None)
            .await,
        Err(CollaboratorStoreError::SessionUnavailable)
    ));
    request.expected_parent_generation = 2;
    assert!(matches!(
        fixture
            .store
            .insert_run_with_collaborator_stage(child.clone(), request.clone(), None)
            .await,
        Err(CollaboratorStoreError::ParentNotCurrent { .. })
    ));
    request.expected_parent_generation = 1;
    let existing_child_id = child.run_id.clone();
    fixture.store.insert_run(child.clone()).await.unwrap();
    assert!(matches!(
        fixture
            .store
            .insert_run_with_collaborator_stage(child, request, None)
            .await,
        Err(CollaboratorStoreError::AssociationConflict { .. })
    ));
    assert!(
        fixture
            .store
            .load_collaborator_association(
                &fixture.user_id,
                &fixture.session_id,
                &existing_child_id
            )
            .await
            .unwrap()
            .is_none()
    );
    fixture
        .store
        .request_run_cancellation(&fixture.user_id, &fixture.parent_id)
        .await
        .unwrap();
    let child = fixture.child();
    let request = admission(&child.run_id, CollaboratorProvider::InternalModel);
    assert!(matches!(
        fixture
            .store
            .insert_run_with_collaborator_stage(child.clone(), request, None)
            .await,
        Err(CollaboratorStoreError::ParentNotCurrent { .. })
    ));
    assert!(
        fixture
            .store
            .load_run(&fixture.user_id, &child.run_id)
            .await
            .unwrap()
            .is_none()
    );
    fixture.cleanup().await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn failed_child_insert_rolls_back_already_staged_anchor_receipt() {
    let fixture = Fixture::new().await;
    let (mut request, first) = fixture.first(CollaboratorProvider::InternalModel).await;
    fixture.settle(&first.run_id).await;
    let candidate = fixture.child();
    fixture.store.insert_run(candidate.clone()).await.unwrap();
    request.expected_previous_stage_run_id = Some(first.run_id.clone());
    request.source_message_id = Uuid::new_v4().to_string();
    let before = fixture
        .store
        .load_run(&fixture.user_id, &first.run_id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        fixture
            .store
            .insert_run_with_collaborator_stage(candidate, request, None)
            .await
            .is_err()
    );
    let after = fixture
        .store
        .load_run(&fixture.user_id, &first.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.last_event_idx, before.last_event_idx);
    let association = fixture
        .store
        .load_collaborator_association(&fixture.user_id, &fixture.session_id, &first.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(association.latest_stage.run_id, first.run_id);
    fixture.cleanup().await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn stage_association_index_misses_preserve_ordinary_children_and_release_session_locks() {
    let fixture = Fixture::new().await;
    let (_, stage) = fixture.first(CollaboratorProvider::InternalModel).await;
    let ordinary = fixture.child();
    fixture.store.insert_run(ordinary.clone()).await.unwrap();
    let association = fixture
        .store
        .load_collaborator_association_for_stage(
            &fixture.user_id,
            &fixture.session_id,
            &stage.run_id,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(association.latest_stage.run_id, stage.run_id);
    for target in [&ordinary.run_id, &Uuid::new_v4().to_string()] {
        let next = fixture.child();
        let (missing, inserted) = tokio::time::timeout(Duration::from_secs(15), async {
            tokio::join!(
                fixture.store.load_collaborator_association_for_stage(
                    &fixture.user_id,
                    &fixture.session_id,
                    target
                ),
                fixture.store.insert_run(next),
            )
        })
        .await
        .expect("indexed miss must release session locks for concurrent writes");
        assert!(missing.unwrap().is_none());
        inserted.unwrap();
    }
    assert_eq!(
        fixture
            .store
            .load_run(&fixture.user_id, &ordinary.run_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        "running"
    );
    fixture.cleanup().await;
}

#[tokio::test]
#[ignore = "requires MatrixOne; set ASTRA_TEST_DB_IT=1"]
async fn expired_budget_is_typed_and_external_native_binding_is_required() {
    let fixture = Fixture::new().await;
    let child = fixture.child();
    let request = admission(&child.run_id, CollaboratorProvider::Codex);
    assert!(matches!(
        fixture
            .store
            .insert_run_with_collaborator_stage(
                child.clone(),
                request,
                Some(tokio::time::Instant::now()),
            )
            .await,
        Err(CollaboratorStoreError::DeadlineExpired)
    ));
    assert!(
        fixture
            .store
            .load_run(&fixture.user_id, &child.run_id)
            .await
            .unwrap()
            .is_none()
    );
    let (mut request, first) = fixture.first(CollaboratorProvider::Codex).await;
    fixture.settle(&first.run_id).await;
    request.expected_previous_stage_run_id = Some(first.run_id);
    request.source_message_id = Uuid::new_v4().to_string();
    assert!(matches!(
        fixture
            .store
            .insert_run_with_collaborator_stage(fixture.child(), request, None)
            .await,
        Err(CollaboratorStoreError::NativeSessionUnbound { .. })
    ));
    fixture.cleanup().await;
}
