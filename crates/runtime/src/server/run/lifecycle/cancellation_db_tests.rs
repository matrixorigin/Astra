use super::*;
use astra_services::{
    AcquireWriterAndReserveTurnOutcome, DatabaseSessionContextCoordinator,
    SessionContextCoordinator, SessionExecutionBindingV1,
};
use astra_turn_types::{
    ActorContextV1, ActorKindV1, AuthorityEpochsV1, ConversationWriterLeaseV1,
    DurableToolReference, SessionKeyV1, SessionSurfaceV1, ToolInvocationDecision,
    ToolInvocationFingerprint, ToolInvocationIdentity, ToolInvocationState,
};

#[path = "cancellation_db_disconnect_tests.rs"]
mod disconnect;

fn checkout_binding(session_id: &str, physical_id: &str) -> SessionExecutionBindingV1 {
    use astra_services::runs::*;
    SessionExecutionBindingV1 {
        schema_version: astra_services::SESSION_EXECUTION_BINDING_SCHEMA_VERSION,
        generation: 1,
        state: astra_services::SessionExecutionBindingStateV1::Ready,
        logical_workspace_id: format!("session:{session_id}"),
        physical_workspace_id: Some(physical_id.to_owned()),
        workspace: WorkspaceBindingRequest {
            kind: WorkspaceBindingRequestKind::EdgeWorkspace,
            display_name: None,
            root: Some("/workspace/cancel-fixture".into()),
            source: Some(WorkspaceSourceRequest::EdgePath {
                path: "/workspace/cancel-fixture".into(),
            }),
            authority: Some(WorkspaceAuthorityRequest::ReadWrite),
        },
        executor: ExecutorBindingRequest {
            kind: ExecutorBindingRequestKind::EdgeAgent,
            executor_id: Some("cancel-fixture-edge".into()),
            display_name: None,
            transport: Some(ToolTransportKindRequest::EdgeLedger),
            status: Some(ExecutorStatusRequest::Online),
        },
    }
}

struct CancellationFixture {
    pool: SharedPool,
    service: AgenticRunLifecycleService,
    coordinator: DatabaseSessionContextCoordinator,
    key: SessionKeyV1,
    run_id: String,
    writer: ConversationWriterLeaseV1,
    binding: SessionExecutionBindingV1,
}

impl CancellationFixture {
    async fn new(external_writer: bool, start_run: bool) -> Self {
        let pool = setup_lifecycle_run_db_it().await;
        let nonce = Uuid::new_v4();
        let key = SessionKeyV1::owner_session(
            "server",
            format!("cancel-user-{nonce}"),
            format!("cancel-session-{nonce}"),
            "main",
        );
        let run_id = format!("cancel-run-{nonce}");
        eprintln!(
            "cancellation DB fixture: session_id={} run_id={run_id}",
            key.session_id
        );
        crate::server::run::insert_active_run_session_fixture(
            &pool,
            &key.owner_user_id,
            &key.session_id,
        )
        .await;
        let coordinator = DatabaseSessionContextCoordinator::new(pool.clone());
        let binding = checkout_binding(&key.session_id, &format!("cancel-checkout-{nonce}"));
        coordinator
            .load_or_initialize_execution_binding(&key, &binding)
            .await
            .unwrap();
        let actor_id = if external_writer {
            "external-controller".to_owned()
        } else {
            format!("server-run:{run_id}")
        };
        let actor = ActorContextV1::owner_user(
            &key.owner_user_id,
            actor_id,
            ActorKindV1::Server,
            SessionSurfaceV1::Server,
            None,
            AuthorityEpochsV1::default(),
        );
        let authority = coordinator
            .acquire_writer_and_reserve_turn(
                &key,
                None,
                &actor,
                Duration::from_secs(900),
                &format!("server-run:{run_id}:writer"),
                &format!("server-run:{run_id}:turn"),
                Some(1),
            )
            .await
            .unwrap();
        let AcquireWriterAndReserveTurnOutcome::Ready { lease: writer, .. } = authority else {
            panic!("writer admission failed: {authority:?}")
        };
        let service = db_backed_test_service(&pool, "cancel-session-fixture");
        if start_run {
            service
                .run_engine
                .start_run(&run_id, &key.owner_user_id, &key.session_id)
                .await
                .unwrap();
        }
        Self {
            pool,
            service,
            coordinator,
            key,
            run_id,
            writer,
            binding,
        }
    }

    async fn orphan(&self) {
        sqlx::query("UPDATE agent_runs SET owner_pod_id = NULL, owner_lease_expires_at = NULL WHERE user_id = ? AND run_id = ?")
            .bind(&self.key.owner_user_id).bind(&self.run_id).execute(self.pool.get()).await.unwrap();
    }

    async fn cancel(&self) -> astra_services::runs::CancelSessionRecord {
        ok(self
            .service
            .cancel_session_runs(self.key.session_id.clone(), self.key.owner_user_id.clone())
            .await)
    }

    async fn tool(&self, unknown: bool) -> ToolInvocationIdentity {
        let identity = ToolInvocationIdentity::new(
            &self.key.owner_user_id,
            &self.key.session_id,
            &self.run_id,
            "turn",
            "call",
        )
        .unwrap();
        let decision = ToolInvocationDecision::new(&json!({"route":"edge_ledger"})).unwrap();
        let fingerprint = ToolInvocationFingerprint::new(
            DurableToolReference::built_in("bash", "registry-v1").unwrap(),
            &json!({"command":"fixture"}),
            &decision.decision_id,
        )
        .unwrap();
        let ledger = astra_services::tool_invocation_ledger::DatabaseToolInvocationLedger::new(
            self.pool.clone(),
        );
        ledger
            .prepare(&identity, &fingerprint, &decision)
            .await
            .unwrap();
        if unknown {
            sqlx::query("UPDATE tool_invocation_ledger SET state = 'outcome_unknown', attempt_count = 1, dispatch_certainty = 'unknown' WHERE user_id = ? AND run_id = ?")
                .bind(&self.key.owner_user_id).bind(&self.run_id).execute(self.pool.get()).await.unwrap();
        }
        identity
    }

    async fn cleanup(self) {
        for table in ["tool_invocation_ledger", "agent_events"] {
            sqlx::query(&format!(
                "DELETE FROM {table} WHERE user_id = ? AND session_id = ?"
            ))
            .bind(&self.key.owner_user_id)
            .bind(&self.key.session_id)
            .execute(self.pool.get())
            .await
            .unwrap();
        }
        for table in [
            "session_context_operation_receipts",
            "session_context_authority_events",
        ] {
            sqlx::query(&format!(
                "DELETE FROM {table} WHERE owner_user_id = ? AND session_id = ?"
            ))
            .bind(&self.key.owner_user_id)
            .bind(&self.key.session_id)
            .execute(self.pool.get())
            .await
            .unwrap();
        }
        cleanup_lifecycle_run_fixture(&self.pool, &self.key.owner_user_id, &self.run_id).await;
        cleanup_lifecycle_execution_binding(&self.pool, &self.key).await;
        crate::server::run::cleanup_run_session_fixture(
            &self.pool,
            &self.key.owner_user_id,
            &self.key.session_id,
        )
        .await;
    }
}

#[tokio::test]
#[ignore = "requires disposable MatrixOne: ASTRA_TEST_DB_IT=1"]
async fn db_cancel_session_orphan_releases_exact_writer_prepared_tool_and_checkout() {
    let fixture = CancellationFixture::new(false, true).await;
    let history = json!({"event_type":"fixture_history", "data":{"text":"keep this conversation"}});
    fixture
        .service
        .run_engine
        .append_event(
            &fixture.key.owner_user_id,
            &fixture.key.session_id,
            &fixture.run_id,
            history.clone(),
        )
        .await
        .unwrap();
    let identity = fixture.tool(false).await;
    fixture.orphan().await;
    let settled = fixture.cancel().await;
    assert!(settled.execution_settled, "{settled:?}");
    assert!(
        fixture
            .coordinator
            .load_active_writer(&fixture.key)
            .await
            .unwrap()
            .is_none()
    );
    let ledger = astra_services::tool_invocation_ledger::DatabaseToolInvocationLedger::new(
        fixture.pool.clone(),
    );
    let tool = ledger.get(&identity).await.unwrap().unwrap();
    assert_eq!(tool.state, ToolInvocationState::Rejected);
    assert_eq!(tool.attempt_count, 0);
    assert!(
        fixture.cancel().await.execution_settled,
        "retry must converge too"
    );
    let next_key = SessionKeyV1::owner_session(
        "server",
        &fixture.key.owner_user_id,
        format!("next-{}", Uuid::new_v4()),
        "main",
    );
    crate::server::run::insert_active_run_session_fixture(
        &fixture.pool,
        &next_key.owner_user_id,
        &next_key.session_id,
    )
    .await;
    let next_binding = checkout_binding(
        &next_key.session_id,
        fixture.binding.physical_workspace_id.as_deref().unwrap(),
    );
    fixture
        .coordinator
        .load_or_initialize_execution_binding(&next_key, &next_binding)
        .await
        .expect("cancelled checkout must admit a fresh conversation without TTL or deletion");
    assert_eq!(
        fixture
            .coordinator
            .load_execution_binding(&fixture.key)
            .await
            .unwrap(),
        Some(fixture.binding.clone())
    );
    let old_run = fixture
        .service
        .run_engine
        .load_run(&fixture.key.owner_user_id, &fixture.run_id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        old_run
            .events
            .iter()
            .any(|event| event.get("data") == history.get("data")),
        "checkout admission must preserve old conversation history"
    );
    cleanup_lifecycle_execution_binding(&fixture.pool, &next_key).await;
    crate::server::run::cleanup_run_session_fixture(
        &fixture.pool,
        &next_key.owner_user_id,
        &next_key.session_id,
    )
    .await;
    fixture.cleanup().await;
}

#[tokio::test]
#[ignore = "requires disposable MatrixOne: ASTRA_TEST_DB_IT=1"]
async fn db_cancel_session_retains_unknown_tool_external_writer_and_preadmission() {
    for case in ["unknown", "external_writer", "preadmission", "switching"] {
        let fixture =
            CancellationFixture::new(case == "external_writer", case != "preadmission").await;
        let identity = if case == "unknown" {
            Some(fixture.tool(true).await)
        } else {
            None
        };
        if case != "preadmission" {
            fixture.orphan().await;
        }
        if case == "switching" {
            let mut binding = fixture.binding.clone();
            binding.state = astra_services::SessionExecutionBindingStateV1::Switching;
            sqlx::query("UPDATE session_execution_bindings SET binding_json = ? WHERE owner_user_id = ? AND session_id = ?")
                .bind(serde_json::to_string(&binding).unwrap()).bind(&fixture.key.owner_user_id).bind(&fixture.key.session_id).execute(fixture.pool.get()).await.unwrap();
        }
        for _ in 0..2 {
            let pending = fixture.cancel().await;
            assert!(!pending.execution_settled, "{case}: {pending:?}");
            use astra_services::session_context_coordinator::WorkspaceReuseBlocker;
            assert_eq!(
                pending.workspace_blocker,
                Some(match case {
                    "unknown" => WorkspaceReuseBlocker::UnresolvedTool,
                    "switching" => WorkspaceReuseBlocker::BindingNotReady,
                    _ => WorkspaceReuseBlocker::WriterOrReservation,
                }),
                "{case}: {pending:?}"
            );
        }
        assert!(
            fixture
                .coordinator
                .execution_reuse_blocker(&fixture.key)
                .await
                .unwrap()
                .is_some(),
            "{case}"
        );
        if let Some(identity) = identity {
            let ledger = astra_services::tool_invocation_ledger::DatabaseToolInvocationLedger::new(
                fixture.pool.clone(),
            );
            assert_eq!(
                ledger.get(&identity).await.unwrap().unwrap().state,
                ToolInvocationState::OutcomeUnknown
            );
        }
        if matches!(case, "external_writer" | "preadmission") {
            assert_eq!(
                fixture
                    .coordinator
                    .load_active_writer(&fixture.key)
                    .await
                    .unwrap(),
                Some(fixture.writer.clone())
            );
        }
        fixture.cleanup().await;
    }
}

#[tokio::test]
#[ignore = "requires disposable MatrixOne: ASTRA_TEST_DB_IT=1"]
async fn db_cancel_session_writer_release_rechecks_generation_and_new_lease() {
    let fixture = CancellationFixture::new(false, true).await;
    fixture.orphan().await;
    let control = fixture
        .service
        .run_engine
        .load_run_control(&fixture.key.owner_user_id, &fixture.run_id)
        .await
        .unwrap()
        .unwrap();
    ok(fixture
        .service
        .cancel_run(fixture.run_id.clone(), fixture.key.owner_user_id.clone())
        .await);
    sqlx::query("UPDATE agent_runs SET run_generation = run_generation + 1 WHERE user_id = ? AND run_id = ?")
        .bind(&fixture.key.owner_user_id).bind(&fixture.run_id).execute(fixture.pool.get()).await.unwrap();
    assert!(
        !fixture
            .coordinator
            .release_terminal_execution_writer(
                &fixture.writer,
                &fixture.run_id,
                control.run_generation
            )
            .await
            .unwrap()
    );
    assert_eq!(
        fixture
            .coordinator
            .load_active_writer(&fixture.key)
            .await
            .unwrap(),
        Some(fixture.writer.clone())
    );
    fixture
        .coordinator
        .release_writer(&fixture.writer)
        .await
        .unwrap();
    let actor = ActorContextV1::owner_user(
        &fixture.key.owner_user_id,
        "new-controller",
        ActorKindV1::Server,
        SessionSurfaceV1::Server,
        None,
        AuthorityEpochsV1::default(),
    );
    let new_authority = fixture
        .coordinator
        .acquire_writer_and_reserve_turn(
            &fixture.key,
            None,
            &actor,
            Duration::from_secs(900),
            "new-writer",
            "new-turn",
            Some(1),
        )
        .await
        .unwrap();
    let AcquireWriterAndReserveTurnOutcome::Ready {
        lease: new_writer, ..
    } = new_authority
    else {
        panic!("new writer failed")
    };
    assert!(
        !fixture
            .coordinator
            .release_terminal_execution_writer(
                &fixture.writer,
                &fixture.run_id,
                control.run_generation + 1
            )
            .await
            .unwrap()
    );
    assert_eq!(
        fixture
            .coordinator
            .load_active_writer(&fixture.key)
            .await
            .unwrap(),
        Some(new_writer)
    );
    fixture.cleanup().await;
}

#[tokio::test]
#[ignore = "requires disposable MatrixOne: ASTRA_TEST_DB_IT=1"]
async fn db_cancel_session_cross_pod_retains_open_settlement_without_lease() {
    use astra_services::session_context_coordinator::WorkspaceReuseBlocker;
    for closure in ["finished", "accounting"] {
        let fixture = CancellationFixture::new(false, true).await;
        fixture.orphan().await;
        let generation = fixture
            .service
            .run_engine
            .load_run_control(&fixture.key.owner_user_id, &fixture.run_id)
            .await
            .unwrap()
            .unwrap()
            .run_generation;
        let started = AgenticRunLifecycleService::settlement_started_event(generation);
        fixture
            .service
            .run_engine
            .append_event(
                &fixture.key.owner_user_id,
                &fixture.key.session_id,
                &fixture.run_id,
                started,
            )
            .await
            .unwrap();
        ok(fixture
            .service
            .cancel_run(fixture.run_id.clone(), fixture.key.owner_user_id.clone())
            .await);
        // Another generation's closed fence must not close the current one.
        let other_generation = generation + 1;
        for event in [
            AgenticRunLifecycleService::settlement_started_event(other_generation),
            AgenticRunLifecycleService::settlement_finished_event(other_generation),
        ] {
            fixture
                .service
                .run_engine
                .append_event(
                    &fixture.key.owner_user_id,
                    &fixture.key.session_id,
                    &fixture.run_id,
                    event,
                )
                .await
                .unwrap();
        }
        let peer = db_backed_test_service(&fixture.pool, "cancel-session-other-pod");
        for _ in 0..2 {
            let pending = ok(peer
                .cancel_session_runs(
                    fixture.key.session_id.clone(),
                    fixture.key.owner_user_id.clone(),
                )
                .await);
            assert!(!pending.execution_settled, "{pending:?}");
            assert_eq!(
                pending.workspace_blocker,
                Some(WorkspaceReuseBlocker::SettlementPending)
            );
            assert_eq!(pending.runs.len(), 1);
            assert!(!pending.runs[0].execution_settled);
            assert_eq!(
                fixture
                    .coordinator
                    .load_active_writer(&fixture.key)
                    .await
                    .unwrap(),
                Some(fixture.writer.clone())
            );
        }
        assert!(
            !fixture
                .coordinator
                .release_terminal_execution_writer(&fixture.writer, &fixture.run_id, generation)
                .await
                .unwrap()
        );
        // The fence must block checkout independently of any writer or lease.
        fixture
            .coordinator
            .release_writer(&fixture.writer)
            .await
            .unwrap();
        assert_eq!(
            fixture
                .coordinator
                .execution_reuse_blocker(&fixture.key)
                .await
                .unwrap(),
            Some(WorkspaceReuseBlocker::SettlementPending)
        );
        let event = if closure == "finished" {
            AgenticRunLifecycleService::settlement_finished_event(generation)
        } else {
            json!({"event_type":"run_accounting_finalized", "idempotency_key":format!("run-accounting-finalized:{generation}"), "data":{}})
        };
        fixture
            .service
            .run_engine
            .append_event(
                &fixture.key.owner_user_id,
                &fixture.key.session_id,
                &fixture.run_id,
                event,
            )
            .await
            .unwrap();
        assert!(
            ok(peer
                .cancel_session_runs(
                    fixture.key.session_id.clone(),
                    fixture.key.owner_user_id.clone()
                )
                .await)
            .execution_settled
        );
        // An old generation's open fence is not debt of the current executor.
        sqlx::query("UPDATE agent_runs SET run_generation = run_generation + 11 WHERE user_id = ? AND run_id = ?")
            .bind(&fixture.key.owner_user_id).bind(&fixture.run_id).execute(fixture.pool.get()).await.unwrap();
        fixture
            .service
            .run_engine
            .append_event(
                &fixture.key.owner_user_id,
                &fixture.key.session_id,
                &fixture.run_id,
                AgenticRunLifecycleService::settlement_started_event(generation + 10),
            )
            .await
            .unwrap();
        assert_eq!(
            fixture
                .coordinator
                .execution_reuse_blocker(&fixture.key)
                .await
                .unwrap(),
            None
        );
        fixture.cleanup().await;
    }
}

#[tokio::test]
#[ignore = "requires disposable MatrixOne: ASTRA_TEST_DB_IT=1"]
async fn db_cancel_session_bulk_intent_reaches_tail_and_preserves_other_session() {
    let fixture = CancellationFixture::new(false, true).await;
    let outside_session = format!("cancel-outside-{}", Uuid::new_v4());
    let outside_run = format!("cancel-outside-run-{}", Uuid::new_v4());
    crate::server::run::insert_active_run_session_fixture(
        &fixture.pool,
        &fixture.key.owner_user_id,
        &outside_session,
    )
    .await;
    fixture
        .service
        .run_engine
        .start_run(&outside_run, &fixture.key.owner_user_id, &outside_session)
        .await
        .unwrap();
    let mut children = Vec::new();
    for index in 0..105 {
        let child = format!("{}-{index:03}", fixture.run_id);
        fixture
            .service
            .run_engine
            .start_run_ext(
                &child,
                &fixture.key.owner_user_id,
                &fixture.key.session_id,
                Some(&fixture.run_id),
                None,
                Some("worker"),
                None,
            )
            .await
            .unwrap();
        children.push(child);
    }
    // Exercise the public lifecycle entrypoint; settlement may be partial, but
    // all intent must already be durable, even for runs beyond the first page.
    let _ = fixture.cancel().await;
    let marked: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_runs WHERE user_id = ? AND session_id = ? AND cancellation_requested_at IS NOT NULL")
        .bind(&fixture.key.owner_user_id).bind(&fixture.key.session_id).fetch_one(fixture.pool.get()).await.unwrap();
    assert_eq!(marked, 106);
    assert!(
        !fixture
            .service
            .run_engine
            .load_run_control(&fixture.key.owner_user_id, &outside_run)
            .await
            .unwrap()
            .unwrap()
            .cancellation_requested
    );
    fixture
        .service
        .run_engine
        .request_session_cancellation(&fixture.key.owner_user_id, &fixture.key.session_id)
        .await
        .unwrap();
    for child in children {
        cleanup_lifecycle_run_fixture(&fixture.pool, &fixture.key.owner_user_id, &child).await;
    }
    cleanup_lifecycle_run_fixture(&fixture.pool, &fixture.key.owner_user_id, &outside_run).await;
    crate::server::run::cleanup_run_session_fixture(
        &fixture.pool,
        &fixture.key.owner_user_id,
        &outside_session,
    )
    .await;
    fixture.cleanup().await;
}
