use super::*;
use astra_core::SharedPool;
use astra_services::runs::{DatabaseRunStateStore, RunStateStore};
use astra_services::tuning::rollout::*;
use serde_json::{Value, json};

struct OutcomeFixture {
    pool: SharedPool,
    owner: String,
    run_id: String,
    deployment_id: String,
    app: Router,
}

impl OutcomeFixture {
    async fn new(status: &str) -> Self {
        isolated_router_database::require_isolated_database(
            &rollout_db_common::require_db_it_env().database,
        );
        let pool = rollout_db_common::setup_pool().await;
        let owner = format!("router-outcome-{}", Uuid::new_v4());
        let session = Uuid::new_v4().to_string();
        let run_id = Uuid::new_v4().to_string();
        let deployment = rollout_db_fixture::deployment(&owner);
        let deployment_id = deployment.deployment_id.clone();
        DatabaseRouterRolloutStore(pool.clone())
            .change(
                &owner,
                0,
                "admin-1",
                RolloutChange::Publish(Box::new(deployment.clone())),
            )
            .await
            .unwrap();
        sqlx::query("INSERT INTO agent_sessions (user_id, session_id, status, created_at, updated_at, last_active_at) VALUES (?, ?, 'active', NOW(6), NOW(6), NOW(6))")
            .bind(&owner).bind(&session).execute(pool.get()).await.unwrap();
        let store = Arc::new(DatabaseRunStateStore::new(pool.clone()));
        let engine = astra_runtime::server::run::engine::RunEngine::new(store.clone());
        let mut context = astra_runtime::server::run::engine::RunStartContext::default();
        context.model_selection = Some(astra_turn_types::ModelSelection {
            offering_id: "strong".into(),
        });
        context.resolved_model_selection = Some(astra_services::runs::ResolvedModelSelection {
            offering_id: "strong".into(),
            model_name: "strong-model".into(),
        });
        let authority = engine
            .start_run_with_context(&run_id, &owner, &session, context)
            .await
            .unwrap();
        let mut decision =
            serde_json::from_str::<astra_services::evaluation::router::RouterDatasetInput>(
                include_str!("../../../../fixtures/contracts/model_router_offline.json"),
            )
            .unwrap()
            .sources
            .remove(0)
            .decision;
        decision.run_id = run_id.clone();
        decision.session_id = session.clone();
        decision.policy.strong_offering_id = "strong".into();
        decision.policy.economy_offering_id = "economy".into();
        decision.selected_offering_id = "strong".into();
        decision.selected_model = "strong-model".into();
        decision.rollout = Some(RouterRolloutDecision {
            deployment_id: deployment_id.clone(),
            revision: 1,
            candidate_sha256: deployment.tuning.candidate_sha256,
            rubric_version: "rubric-1".into(),
            routing_failure: None,
            cohort: RolloutCohort::Shadow,
            cohort_probability_basis_points: 10_000,
            proposed_offering_id: "economy".into(),
            abstained: false,
            admission_rejected: false,
            routing_overhead_us: 500,
        });
        assert!(
            store
                .append_events_if_current_generation_and_status(
                    &owner,
                    &session,
                    &run_id,
                    authority.owner_generation,
                    &["running"],
                    &[
                        json!({"event_type":astra_services::model_routing::EVENT_TYPE,
                    "idempotency_key":astra_services::model_routing::DECISION_KEY,
                    "data":decision})
                    ],
                )
                .await
                .unwrap()
        );
        assert!(
            store
                .update_run_status_with_events_if_current(
                    &owner,
                    &session,
                    &run_id,
                    &["running"],
                    Some(authority.owner_generation),
                    status,
                    None,
                    None,
                    &[json!({"event_type":"run_finished","data":{"status":status}})],
                )
                .await
                .unwrap()
        );
        let app = Self::app(&pool, Arc::new(StubAdminAuthorizer));
        Self {
            pool,
            owner,
            run_id,
            deployment_id,
            app,
        }
    }

    fn app(pool: &SharedPool, authorizer: Arc<dyn AdminAuthorizer>) -> Router {
        build_app(
            AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
                .with_admin_authorizer(authorizer)
                .with_shared_pool(pool.clone()),
        )
    }

    fn outcome(critical: bool) -> Value {
        json!({"rubric_version":"rubric-1","evidence_reference":"verifier-1",
            "acceptable":true,"corrected":false,"full_episode_cost_usd":0.2,
            "episode_latency_ms":1000,"critical_violation":critical})
    }

    fn request(&self, outcome: Value) -> Request<body::Body> {
        build_request_with_json(
            "POST",
            &format!(
                "/admin/model-router/{}/outcomes/{}",
                self.owner, self.run_id
            ),
            &[("authorization", "Bearer admin-token")],
            outcome,
        )
    }

    async fn post(&self, outcome: Value) -> StatusCode {
        self.app
            .clone()
            .oneshot(self.request(outcome))
            .await
            .unwrap()
            .status()
    }

    async fn saved(&self) -> Value {
        DatabaseRunStateStore::new(self.pool.clone())
            .load_run_event_by_idempotency_key(
                &self.owner,
                &self.run_id,
                OUTCOME_EVENT,
                OUTCOME_KEY,
            )
            .await
            .unwrap()
            .unwrap()
    }

    async fn state(&self) -> RouterRolloutState {
        DatabaseRouterRolloutStore(self.pool.clone())
            .load(&self.owner)
            .await
            .unwrap()
    }

    async fn audit_count(&self) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM auth_audit_logs WHERE resource_type = 'model_router' AND resource_id = ?")
            .bind(&self.deployment_id).fetch_one(self.pool.get()).await.unwrap()
    }

    async fn dashboard(&self) -> Value {
        let (status, report) = read_json(
            self.app.clone(),
            &format!("/admin/model-router/{}", self.owner),
            &[("authorization", "Bearer admin-token")],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        report
    }

    async fn cleanup(self) {
        for table in [
            "agent_run_events",
            "run_display_projections",
            "agent_session_execution_slots",
            "agent_runs",
            "agent_sessions",
            "model_router_deployments",
        ] {
            sqlx::query(&format!("DELETE FROM {table} WHERE user_id = ?"))
                .bind(&self.owner)
                .execute(self.pool.get())
                .await
                .unwrap();
        }
        sqlx::query(
            "DELETE FROM auth_audit_logs WHERE resource_type = 'model_router' AND resource_id = ?",
        )
        .bind(&self.deployment_id)
        .execute(self.pool.get())
        .await
        .unwrap();
    }
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1 and explicitly isolated ASTRA_TEST_DATABASE"]
async fn conflicting_critical_outcome_preserves_active_deployment_and_audit() {
    let f = OutcomeFixture::new("completed").await;
    assert_eq!(
        f.post(OutcomeFixture::outcome(false)).await,
        StatusCode::CREATED
    );
    let saved = f.saved().await;
    let before = serde_json::to_value(f.state().await).unwrap();
    let audits = f.audit_count().await;
    assert_eq!(
        f.post(OutcomeFixture::outcome(true)).await,
        StatusCode::CONFLICT
    );
    assert_eq!(serde_json::to_value(f.state().await).unwrap(), before);
    assert_eq!(f.audit_count().await, audits);
    assert_eq!(f.saved().await, saved);
    let dashboard = f.dashboard().await;
    assert_eq!(dashboard["mode"], "shadow");
    assert_eq!(dashboard["cohorts"]["shadow"]["critical_violations"], 0);
    f.cleanup().await;
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1 and explicitly isolated ASTRA_TEST_DATABASE"]
async fn delegated_terminal_outcome_is_reviewed_and_stops_its_deployment() {
    let f = OutcomeFixture::new("delegated").await;
    assert_eq!(
        f.post(OutcomeFixture::outcome(true)).await,
        StatusCode::CREATED
    );
    let current = f.state().await;
    assert_eq!(current.revision, 2);
    assert_eq!(current.deployment.unwrap().mode, RolloutMode::RolledBack);
    let dashboard = f.dashboard().await;
    assert_eq!(dashboard["cohorts"]["shadow"]["known_quality"], 1);
    assert_eq!(dashboard["cohorts"]["shadow"]["known_cost"], 1);
    assert_eq!(dashboard["cohorts"]["shadow"]["critical_violations"], 1);
    assert_eq!(
        dashboard["cohorts"]["shadow"]["cost_per_acceptable_task"],
        0.2
    );
    assert_eq!(
        f.saved().await["data"]["outcome"],
        OutcomeFixture::outcome(true)
    );
    f.cleanup().await;
}

struct OverlappingReviewers(tokio::sync::Barrier);

#[async_trait]
impl AdminAuthorizer for OverlappingReviewers {
    async fn require_admin(
        &self,
        headers: &axum::http::HeaderMap,
    ) -> Result<AuthenticatedUser, (StatusCode, axum::Json<ErrorResponse>)> {
        let mut admin = StubAdminAuthorizer.require_admin(headers).await?;
        // Distinct server-owned envelopes must still accept the same payload.
        admin.user_id = headers["test-reviewer"].to_str().unwrap().to_string();
        self.0.wait().await;
        Ok(admin)
    }
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1 and explicitly isolated ASTRA_TEST_DATABASE"]
async fn overlapping_identical_outcomes_preserve_one_original_envelope() {
    overlapping_outcomes(false).await;
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1 and explicitly isolated ASTRA_TEST_DATABASE"]
async fn overlapping_critical_outcomes_commit_one_stop_and_audit() {
    overlapping_outcomes(true).await;
}

async fn overlapping_outcomes(critical: bool) {
    let f = OutcomeFixture::new("completed").await;
    let mut settings = rollout_db_common::require_db_it_env();
    // Noncritical requests use FIFO acquisition to overlap their preflight
    // reads. Critical requests need two connections to contend on the stop.
    settings.db_pool_max_connections = if critical { 2 } else { 1 };
    settings.db_pool_min_connections = 1;
    let pool = SharedPool::new(&settings).await.unwrap();
    let app = OutcomeFixture::app(
        &pool,
        Arc::new(OverlappingReviewers(tokio::sync::Barrier::new(2))),
    );
    // Hold the deployment row while both critical requests accept the outcome
    // and read the active deployment. Each then waits to commit its stop.
    let mut fence = if critical {
        let mut tx = f.pool.get().begin().await.unwrap();
        sqlx::query("SELECT revision FROM model_router_deployments WHERE user_id = ? FOR UPDATE")
            .bind(&f.owner)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        Some(tx)
    } else {
        None
    };
    let mut a = f.request(OutcomeFixture::outcome(critical));
    let mut b = f.request(OutcomeFixture::outcome(critical));
    a.headers_mut()
        .insert("test-reviewer", "reviewer-a".parse().unwrap());
    b.headers_mut()
        .insert("test-reviewer", "reviewer-b".parse().unwrap());
    let (a, b) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let (a, b, ()) = tokio::join!(app.clone().oneshot(a), app.oneshot(b), async {
            if let Some(tx) = fence.take() {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                tx.rollback().await.unwrap();
            }
        });
        (a, b)
    })
    .await
    .expect("overlapping outcome requests finish");
    let (a, b) = (a.unwrap().status(), b.unwrap().status());
    assert!(
        matches!(
            (a, b),
            (StatusCode::CREATED, StatusCode::OK) | (StatusCode::OK, StatusCode::CREATED)
        ),
        "{a}, {b}"
    );
    let saved = f.saved().await;
    assert_eq!(
        saved["data"]["reviewed_by"],
        if a == StatusCode::CREATED {
            "reviewer-a"
        } else {
            "reviewer-b"
        }
    );
    assert_eq!(saved["data"]["outcome"], OutcomeFixture::outcome(critical));
    assert!(saved["data"]["reviewed_at"].is_string());
    assert_eq!(
        f.post(OutcomeFixture::outcome(critical)).await,
        StatusCode::OK
    );
    assert_eq!(
        f.saved().await,
        saved,
        "retry must preserve the winning envelope"
    );
    let events: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_run_events WHERE user_id = ? AND run_id = ? AND idempotency_key = ?")
        .bind(&f.owner).bind(&f.run_id).bind(OUTCOME_KEY).fetch_one(f.pool.get()).await.unwrap();
    assert_eq!(events, 1);
    assert_eq!(f.state().await.revision, if critical { 2 } else { 1 });
    assert_eq!(
        f.state().await.deployment.unwrap().mode,
        if critical {
            RolloutMode::RolledBack
        } else {
            RolloutMode::Shadow
        }
    );
    assert_eq!(f.audit_count().await, if critical { 2 } else { 1 });
    f.cleanup().await;
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1 and explicitly isolated ASTRA_TEST_DATABASE"]
async fn accepted_critical_outcome_retry_finishes_stop_before_acknowledgment() {
    let f = OutcomeFixture::new("completed").await;
    let store = DatabaseRunStateStore::new(f.pool.clone());
    let run = store.load_run(&f.owner, &f.run_id).await.unwrap().unwrap();
    // Simulate an interrupted request after durable acceptance, before stop.
    let event = json!({"event_type":OUTCOME_EVENT,"idempotency_key":OUTCOME_KEY,"data":{
        "deployment_id":f.deployment_id,"reviewed_by":"original-reviewer",
        "reviewed_at":chrono::Utc::now(),"outcome":OutcomeFixture::outcome(true)}});
    assert!(
        store
            .append_events_if_current_generation_and_status(
                &f.owner,
                &run.session_id,
                &f.run_id,
                run.run_generation,
                &["completed"],
                &[event],
            )
            .await
            .unwrap()
    );
    let saved = f.saved().await;
    assert_eq!(
        f.state().await.deployment.unwrap().mode,
        RolloutMode::Shadow
    );
    assert_eq!(f.post(OutcomeFixture::outcome(true)).await, StatusCode::OK);
    assert_eq!(
        f.state().await.deployment.unwrap().mode,
        RolloutMode::RolledBack
    );
    assert_eq!(f.audit_count().await, 2);
    assert_eq!(f.saved().await, saved);
    f.cleanup().await;
}
