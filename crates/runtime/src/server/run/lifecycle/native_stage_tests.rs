//! Real child execution/receipt/terminal boundaries; no model calls are needed.
use super::*;
use astra_messaging::{AgentAddress, AgentMessage, MessagePayload, MessageTarget};
use astra_server_types::edge_connection_pool::{EdgeConnectionPool, EdgeToolResult};
use astra_server_types::edge_ws_protocol::EdgeServerMessage;
use astra_services::multi_agent::{
    DatabaseEdgeDispatchService, DatabaseEdgeRegistryService, EdgeRegistryService,
};
use astra_services::tool_invocation_ledger::DatabaseToolInvocationLedger;
use astra_turn_types::{
    ProviderBindingRef, ProviderDiscoverySnapshot, ProviderStageInputAck, ToolInvocationIdentity,
};
use tracing::instrument::WithSubscriber;
use tracing_subscriber::prelude::*;

struct UnknownDeliveryObservation(Arc<tokio::sync::Notify>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for UnknownDeliveryObservation {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        struct Message(bool);
        impl tracing::field::Visit for Message {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.0 = format!("{value:?}")
                        == "native provider mailbox delivery is unknown; automatic retry disabled";
                }
            }
        }
        if event.metadata().target() == "astra_runtime::run_lifecycle" {
            let mut message = Message(false);
            event.record(&mut message);
            if message.0 {
                self.0.notify_one();
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Scenario {
    BindingSuccess,
    BindingFailure,
    BindingCancelled,
    MailboxUnknown,
    MailboxUnknownRace,
    MailboxUnknownReplay,
}

async fn native_stage_receipt_journey(scenario: Scenario) {
    let pool = setup_lifecycle_run_db_it().await;
    let user = Uuid::new_v4().to_string();
    let session = Uuid::new_v4().to_string();
    let owner = Uuid::new_v4().to_string();
    let parent = Uuid::new_v4().to_string();
    let child = Uuid::new_v4().to_string();
    crate::server::run::insert_active_run_session_fixture(&pool, &user, &session).await;
    let replay = matches!(scenario, Scenario::MailboxUnknownReplay);
    let binding_barrier = (matches!(scenario, Scenario::BindingCancelled) || replay)
        .then(|| Arc::new(tokio::sync::Barrier::new(2)));
    let mut database = DatabaseRunStateStore::new(pool.clone()).with_owner_pod_id(&owner);
    if let Some(barrier) = &binding_barrier {
        database = database.with_native_binding_insert_barrier_for_test(barrier.clone());
    }
    let store = Arc::new(database);
    let engine = RunEngine::new(store.clone());
    let binding = collaborator_db_fixture::seed_execution_binding(&pool, &user, &session).await;
    let parent_authority = engine.start_run(&parent, &user, &session).await.unwrap();

    let mut discovery = authenticated_edge_discovery_fixture();
    discovery.user_id = user.clone();
    discovery.executor_id = "selected-runner".into();
    discovery.workspace_root = "/selected-runner".into();
    discovery.physical_workspace_id = collaborator_db_fixture::physical_workspace_id();
    discovery.binding_generation = binding.generation;
    let original = &discovery.snapshots[0];
    discovery.snapshots = vec![
        ProviderDiscoverySnapshot::new(
            astra_turn_types::ProviderIdentity::new(discovery.executor_id.clone()).unwrap(),
            ProviderBindingRef::new(discovery.physical_workspace_id.clone()).unwrap(),
            original.protocol.clone(),
            original.tool_declarations.clone(),
        )
        .unwrap(),
    ];
    let mut capabilities = PreparedRuntimeCapabilities::default();
    capabilities
        .bind_edge_discovery(Some(discovery.clone()))
        .unwrap();
    let policy = capabilities
        .provider_policy_index
        .resolve("fixture_native_stage")
        .unwrap();
    let policy_content_id = policy.baseline_content_id().unwrap();
    let mut admission = collaborator_db_fixture::admission(&child);
    admission.expected_parent_generation = parent_authority.owner_generation;
    admission.native_execution.descriptor = policy.descriptor.clone();
    admission.native_execution.public_tool_name = "fixture_native_stage".into();
    admission.native_execution.policy_content_id = policy_content_id.clone();
    admission.native_execution.requested_model = None;
    let native = PreparedNativeSubrunExecution {
        stage_admission: admission,
        identity: crate::orchestration::PreparedSpawnProviderIdentity {
            descriptor: policy.descriptor.clone(),
            decision_id: policy_content_id,
            execution_binding_generation: binding.generation,
            requested_model: None,
        },
        live_edge_agent_id: "selected-runner".into(),
        tool_name: "fixture_native_stage".into(),
        provider_schema: json!({"type":"function","function":{"name":"fixture_native_stage",
            "parameters":{"type":"object"}}}),
        arguments: json!({"task":"Deliver the original result.","anchor_run_id":child}),
        policy: policy.clone(),
        run_state_store: store.clone(),
    };
    let runtime_binding = astra_runtime_env::RunBinding::resolve(
        astra_runtime_env::WorkspaceBinding::edge_workspace(
            "/selected-runner",
            astra_runtime_env::WorkspaceAuthority::ReadWrite,
        ),
        astra_runtime_env::ExecutorBinding::edge_agent("selected-runner"),
        astra_runtime_env::RuntimeBinding::host_process("fixture-runtime"),
        astra_runtime_env::PolicyIntent::local_developer(),
        &astra_runtime_env::ToolRegistry::builtins(),
    );
    let mut advertisement =
        astra_runtime_env::RuntimeEnvironmentAdvertisement::new(runtime_binding);
    advertisement.provider_discovery = discovery.snapshots.clone();
    let advertised_capabilities = serde_json::to_value(advertisement).unwrap();
    let registry = Arc::new(DatabaseEdgeRegistryService::from_shared(&pool));
    let lease = registry
        .register_or_update_with_lease_and_materialization(
            &user,
            "selected-runner",
            "selected-transport",
            None,
            Some("/selected-runner"),
            Some(advertised_capabilities.clone()),
            None,
            Some("fixture-materialization"),
        )
        .await
        .unwrap();
    assert!(registry.finalize_registration(&lease).await.unwrap());
    let edge_pool = EdgeConnectionPool::new();
    let (edge_tx, mut edge_rx) = mpsc::channel(8);
    edge_pool.register_with_capabilities_registry_and_materialization_id(
        &user,
        "selected-runner",
        None,
        Some("/selected-runner".into()),
        Some(advertised_capabilities),
        None,
        Some(lease.current.registry_id.clone()),
        Some("fixture-materialization".into()),
        edge_tx,
    );
    let cancelled = Arc::new(CancellationToken::new());
    let make_config = || {
        let mut config = test_executable_subrun_config(&child, None);
        config.user_id = user.clone();
        config.session_id = session.clone();
        config.parent_run_id = parent.clone();
        config.task = "Deliver the original result.".into();
        config.agent_profile = AgentProfile::new("collaborator", "Collaborator", AgentTier::User);
        config.request_constraints = RequestConstraints::new(
            Some(HashSet::from(["fixture_native_stage".into()])),
            None,
            None,
            None,
        );
        let mut snapshot = ExecutionBindingSnapshot::inferred(
            WorkspaceBinding::edge_workspace(
                "Selected runner",
                "/selected-runner",
                WorkspaceAuthority::ReadWrite,
            ),
            ExecutorBinding::edge_agent(
                "selected-runner",
                "Selected runner",
                ToolTransportKind::EdgeWs,
                ExecutorStatus::Online,
            ),
        );
        snapshot.execution_binding_generation = Some(binding.generation);
        snapshot.physical_workspace_id = Some(collaborator_db_fixture::physical_workspace_id());
        config.execution_contract = Some((snapshot, Default::default()));
        config.cancel_token = Some(cancelled.clone());
        config
    };
    let mut config = make_config();
    let router = Arc::new(astra_messaging::AgentMailboxRouter::new(
        Arc::new(astra_messaging::InProcessTransport::new()),
        Arc::new(crate::server::delegation::engine::DelegationTracker::new()),
    ));
    config.mailbox = Some(
        router
            .register_with_capabilities(
                AgentAddress::new(&child, "collaborator"),
                None,
                astra_messaging::MailboxCapabilities::provider_stage_text_only(),
            )
            .await
            .unwrap(),
    );
    let mailbox_unknown = matches!(
        scenario,
        Scenario::MailboxUnknown | Scenario::MailboxUnknownRace | Scenario::MailboxUnknownReplay
    );
    if !mailbox_unknown || replay {
        store.fail_next_native_binding_insert_for_test();
    }
    let mut executor = ServerSubRunExecutor::new(
        pool.settings().clone(),
        test_encryptor(),
        Arc::new(TokioMutex::new(HashMap::new())),
    )
    .with_run_engine(engine.clone())
    .with_pool(pool.clone())
    .with_invocation_ledger(
        crate::server::tool_invocation_runtime::RuntimeToolInvocationLedger::new_database(
            pool.clone(),
            owner.clone(),
        ),
    )
    .with_edge_connection_pool(edge_pool.clone())
    .with_edge_dispatch_service(Arc::new(DatabaseEdgeDispatchService::from_shared(&pool)))
    .with_edge_registry_service(registry)
    .with_native_execution(native);
    // The selected Server provider owns private scratch only; the user's
    // remote workspace remains opaque and execution stays on the Edge.
    executor.workspace_provider = Some(ServerWorkspaceProvisioner::fixture(
        Arc::new(tempfile::tempdir().unwrap()),
        "test-server-scratch",
    ));
    let child_authority = executor
        .ensure_durable_subrun_started(&config, None)
        .await
        .unwrap()
        .unwrap();
    config.execution_owner_generation = Some(child_authority.owner_generation);
    let identity =
        ToolInvocationIdentity::new(&user, &session, &child, &child, "native-stage").unwrap();
    let unknown_observed = Arc::new(tokio::sync::Notify::new());
    let replay_ready = Arc::new(tokio::sync::Notify::new());
    let subscriber = tracing::Dispatch::new(
        tracing_subscriber::registry().with(UnknownDeliveryObservation(unknown_observed.clone())),
    );
    let peer = async {
        let request = tokio::time::timeout(Duration::from_secs(10), edge_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let EdgeServerMessage::ToolRequest {
            request_id,
            delivery_generation,
            identity: actual,
            ..
        } = request
        else {
            panic!("expected native dispatch: {request:?}")
        };
        assert_eq!(*actual, identity);
        let mut completion_race_ack = None;
        if mailbox_unknown {
            for content in ["First guidance", "Second guidance"] {
                router
                    .send(AgentMessage::new(
                        AgentAddress::new(&parent, "parent"),
                        MessageTarget::Direct {
                            address: AgentAddress::new(&child, "collaborator"),
                        },
                        MessagePayload::Text {
                            content: content.into(),
                            summary: None,
                        },
                    ))
                    .await
                    .unwrap();
            }
            let EdgeServerMessage::ToolInput { input, .. } =
                tokio::time::timeout(Duration::from_secs(5), edge_rx.recv())
                    .await
                    .unwrap()
                    .unwrap()
            else {
                panic!("expected guidance")
            };
            let mut ack = ProviderStageInputAck::accepted(&input, None);
            ack.reason = Some("contradictory acceptance".into());
            if matches!(scenario, Scenario::MailboxUnknownRace) {
                completion_race_ack = Some(ack);
            } else {
                assert!(edge_pool.deliver_provider_stage_input_ack(
                    &user,
                    "selected-runner",
                    &request_id,
                    delivery_generation,
                    ack
                ));
                tokio::time::timeout(Duration::from_secs(5), unknown_observed.notified())
                    .await
                    .expect("the real unknown branch must run before completion");
            }
        }
        let fields = json!({"workspace_effect_settled":true,
            "collaborator_native_session":{"anchor_run_id":child,"provider":"codex","native_session_id":"acknowledged-thread"}
        }).as_object().unwrap().clone();
        assert!(edge_pool.deliver_tool_result(
            &user,
            "selected-runner",
            &request_id,
            delivery_generation,
            EdgeToolResult {
                output: "Original delivered result.".into(),
                is_error: matches!(scenario, Scenario::BindingFailure),
                duration_ms: Some(1),
                tool_result_fields: Some(fields)
            }
        ));
        if let Some(ack) = completion_race_ack {
            // Completion is delivered while the input ACK remains outstanding.
            // The result path may already have closed the ACK waiter.
            let _ = edge_pool.deliver_provider_stage_input_ack(
                &user,
                "selected-runner",
                &request_id,
                delivery_generation,
                ack,
            );
        }
        if let Some(barrier) = &binding_barrier {
            // Confirmation starts only after the physical receipt. Gate the
            // INSERT so cancellation ordering does not depend on DB latency.
            barrier.wait().await;
            if replay {
                // Simulate loss of the running future after its real receipt
                // but before terminal publication, not cancellation of work.
                replay_ready.notify_one();
                std::future::pending::<()>().await;
            }
            let ledger = DatabaseToolInvocationLedger::new(pool.clone());
            assert!(
                ledger
                    .get(&identity)
                    .await
                    .unwrap()
                    .unwrap()
                    .state
                    .is_terminal()
            );
            cancelled.cancel();
            barrier.wait().await;
        }
    };
    let receipt = tokio::time::timeout(Duration::from_secs(20), async {
        let execution = executor.execute(config).with_subscriber(subscriber.clone());
        tokio::pin!(execution);
        tokio::select! {
            receipt = &mut execution => Some(receipt),
            () = peer => Some(execution.await),
            () = replay_ready.notified(), if replay => None,
        }
    })
    .await
    .unwrap();
    let ledger = DatabaseToolInvocationLedger::new(pool.clone());
    let before_replay = if receipt.is_none() {
        Some(ledger.get(&identity).await.unwrap().unwrap())
    } else {
        None
    };
    let receipt = match receipt {
        Some(receipt) => receipt,
        None => {
            let mut resumed = make_config();
            // Dropping the crashed executor cancels its process-local I/O token.
            // Recovery owns a new token; this scenario is not user cancellation.
            resumed.cancel_token = Some(Arc::new(CancellationToken::new()));
            let claimed = store
                .claim_exact_recovery_candidate_for_test(&user, &child)
                .await
                .unwrap();
            assert_eq!(claimed.run.status, STATUS_RUNNING);
            resumed.execution_owner_generation = Some(claimed.run.run_generation);
            tokio::time::timeout(
                Duration::from_secs(20),
                executor.execute(resumed).with_subscriber(subscriber),
            )
            .await
            .unwrap()
        }
    }
    .unwrap();
    let delivered = if matches!(scenario, Scenario::BindingFailure) {
        assert!(receipt.result.output.is_none());
        receipt.result.error.as_deref()
    } else {
        receipt.result.output.as_deref()
    };
    assert_eq!(delivered, Some("Original delivered result."), "{receipt:?}");
    assert_eq!(
        receipt.result.status,
        match scenario {
            Scenario::BindingFailure => STATUS_FAILED,
            Scenario::BindingCancelled => STATUS_CANCELLED,
            _ => STATUS_COMPLETED,
        }
    );
    let durable = store.load_run(&user, &child).await.unwrap().unwrap();
    assert_eq!(durable.status, receipt.result.status);
    assert_eq!(
        durable
            .events
            .iter()
            .filter(|event| event["event_type"] == "run_finished")
            .count(),
        1
    );
    let recorded = ledger.get(&identity).await.unwrap().unwrap();
    assert_eq!(recorded.attempt_count, 1);
    assert_eq!(
        recorded.dispatch_certainty,
        astra_turn_types::DispatchCertainty::Dispatched
    );
    assert_eq!(
        recorded.state,
        if matches!(scenario, Scenario::BindingFailure) {
            astra_turn_types::ToolInvocationState::Failed
        } else {
            astra_turn_types::ToolInvocationState::Succeeded
        }
    );
    assert_eq!(
        recorded
            .outcome
            .as_ref()
            .expect("settled native outcome")
            .result()
            .output,
        "Original delivered result."
    );
    if let Some(before) = before_replay {
        assert_eq!(
            recorded, before,
            "replay must not rewrite the settled receipt"
        );
    }
    if mailbox_unknown {
        let outcome = serde_json::to_value(&recorded.outcome).unwrap();
        assert_eq!(
            outcome.pointer("/result/metadata/guidance_disposition"),
            Some(&json!("unconfirmed"))
        );
    } else {
        let recreated = DatabaseRunStateStore::new(pool.clone()).with_owner_pod_id(&owner);
        assert!(
            recreated
                .recover_collaborator_native_session(
                    &user,
                    &session,
                    &child,
                    &child,
                    durable.run_generation
                )
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(ledger.get(&identity).await.unwrap().unwrap(), recorded);
    }
    assert!(
        edge_rx.try_recv().is_err(),
        "no second steer or native dispatch"
    );
    for table in [
        "tool_invocation_ledger",
        "edge_pending_dispatch",
        "edge_agent_registry",
    ] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM {table} WHERE user_id = ?"
        )))
        .bind(&user)
        .execute(pool.get())
        .await
        .unwrap();
    }
    for id in [&child, &parent] {
        cleanup_lifecycle_run_fixture(&pool, &user, id).await;
    }
    let key = astra_turn_types::SessionKeyV1::owner_session(
        "server",
        &user,
        &session,
        astra_turn_types::DEFAULT_CONVERSATION_BRANCH_ID,
    );
    cleanup_lifecycle_execution_binding(&pool, &key).await;
    crate::server::run::cleanup_run_session_fixture(&pool, &user, &session).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires ASTRA_TEST_DB_IT=1 and MatrixOne; no model calls"]
async fn native_binding_insert_failure_preserves_delivered_result_and_recovery() {
    native_stage_receipt_journey(Scenario::BindingSuccess).await;
    native_stage_receipt_journey(Scenario::BindingFailure).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires ASTRA_TEST_DB_IT=1 and MatrixOne; no model calls"]
async fn native_binding_insert_failure_preserves_cancelled_settlement() {
    native_stage_receipt_journey(Scenario::BindingCancelled).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires ASTRA_TEST_DB_IT=1 and MatrixOne; no model calls"]
async fn native_mailbox_unknown_survives_receipt_terminal_and_completion_race() {
    native_stage_receipt_journey(Scenario::MailboxUnknown).await;
    native_stage_receipt_journey(Scenario::MailboxUnknownRace).await;
    native_stage_receipt_journey(Scenario::MailboxUnknownReplay).await;
}
