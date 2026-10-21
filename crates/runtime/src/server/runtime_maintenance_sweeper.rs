use std::{sync::Arc, time::Duration};

use astra_core::SharedPool;

pub(crate) fn spawn_runtime_maintenance_sweeper(
    pool: SharedPool,
    fork_coordinator: Option<Arc<astra_services::DatabaseSessionForkCoordinator>>,
    lease: Arc<crate::server::sweeper_lease::SweeperLease>,
    cancel: tokio_util::sync::CancellationToken,
    idle_interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let policy = astra_services::runtime_maintenance::RuntimeMaintenancePolicy::default();
        let mut interval = tokio::time::interval(idle_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        interval.tick().await;
        let mut cursor = Default::default();
        let mut context_more = false;
        loop {
            let full_sweep = tokio::select! {
                _ = cancel.cancelled() => break,
                _ = interval.tick() => true,
                _ = tokio::time::sleep(Duration::from_secs(1)), if context_more => false,
            };

            match lease.check_leader().await {
                crate::server::sweeper_lease::LeaderStatus::Leader => {}
                crate::server::sweeper_lease::LeaderStatus::NotLeader => {
                    context_more = false;
                    cursor = Default::default();
                    continue;
                }
                crate::server::sweeper_lease::LeaderStatus::Unavailable(error) => {
                    tracing::warn!(
                        target: "astra_runtime::runtime_maintenance_sweeper",
                        %error,
                        "runtime maintenance lease check unavailable, skipping sweep"
                    );
                    continue;
                }
            }

            if full_sweep {
                let result = astra_services::runtime_maintenance::maintain_runtime_storage(
                    &pool,
                    fork_coordinator.as_deref(),
                    &policy,
                    &mut cursor,
                )
                .await;
                context_more = result.model_request_context_more;
                if result.total_processed() > 0 || !result.cleanup_errors.is_empty() {
                    tracing::info!(
                        target: "astra_runtime::runtime_maintenance_sweeper",
                        explicit_delete_intents_reconciled = result.explicit_delete_intents_reconciled,
                        model_request_context_events_expired = result.model_request_context_events_expired,
                        prompt_request_records_expired = result.prompt_request_records_expired,
                        prompt_deltas_expired = result.prompt_deltas_expired,
                        expired_fork_pins_released = result.expired_fork_pins_released,
                        orphaned_forks_collected = result.orphaned_forks_collected,
                        orphaned_manifests_collected = result.orphaned_manifests_collected,
                        orphaned_manifest_references_collected = result.orphaned_manifest_references_collected,
                        unreferenced_segments_collected = result.unreferenced_segments_collected,
                        cleanup_error_count = result.cleanup_errors.len(),
                        "runtime storage maintenance sweep"
                    );
                }
            }
            // Catch up only request diagnostics. Other maintenance remains on
            // its normal cadence; one cursor and bounded transactions suffice.
            let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
            for _ in usize::from(full_sweep)..20 {
                if !context_more || tokio::time::Instant::now() >= deadline {
                    break;
                }
                if !matches!(
                    lease.check_leader().await,
                    crate::server::sweeper_lease::LeaderStatus::Leader
                ) {
                    break;
                }
                let page = tokio::select! {
                    _ = cancel.cancelled() => return,
                    page = tokio::time::timeout_at(deadline, astra_services::model_request_context::expire_model_request_context_events(&pool, policy.batch_limit, &mut cursor)) => page,
                };
                match page {
                    Ok(Ok(page)) => {
                        context_more = page.more;
                        tracing::debug!(
                            examined = page.examined,
                            deleted = page.deleted,
                            more = page.more,
                            "request diagnostic expiry catch-up"
                        );
                    }
                    Ok(Err(error)) => {
                        tracing::warn!(%error, "request diagnostic expiry catch-up failed");
                        break;
                    }
                    Err(_) => break,
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires ASTRA_TEST_DB_IT=1 and MatrixOne; creates an isolated fixture database"]
    async fn maintenance_walks_pinned_pages_and_stops_on_cancellation() {
        use astra_services::*;
        use astra_turn_types::{InferenceInvocationScope, InferencePurpose};
        assert_eq!(std::env::var("ASTRA_TEST_DB_IT").as_deref(), Ok("1"));
        let mut settings = astra_core::MatrixOneSettings::from_env();
        let control = SharedPool::new(&settings).await.unwrap();
        let database = format!("astra_test_maintenance_{}", uuid::Uuid::new_v4().simple());
        settings.database.clone_from(&database);
        settings.db_pool_max_connections = 4;
        astra_services::ensure_core_schema(&settings, "mysql")
            .await
            .unwrap();
        let pool = SharedPool::new(&settings).await.unwrap();
        sqlx::query("INSERT INTO agent_sessions (session_id,user_id,status,event_count,project_retention_policy,created_at,updated_at,last_active_at) VALUES ('settled-session','settled-owner','active',0,'session',NOW(6),NOW(6),NOW(6))")
            .execute(pool.get()).await.unwrap();
        let plan = plan_inference_invocation(InferenceInvocationInput {
            user_id: "settled-owner".into(),
            scope: InferenceInvocationScope::Session {
                session_id: "settled-session".into(),
                turn: 1,
                round: 0,
                operation_id: "maintenance".into(),
                logical_attempt: 0,
            },
            offering_id: "fixture-offering".into(),
            resolved_model_name: "fixture-model".into(),
            upstream_model_name: "fixture-model".into(),
            provider: "openai".into(),
            purpose: InferencePurpose::Introspection,
            execution_placement: ModelExecutionPlacement::Server,
            access_kind: ModelAccessKind::SelfHosted,
            run_authority: None,
        })
        .unwrap();
        let attempt = plan_inference_provider_attempt(
            &plan,
            0,
            InferenceProviderWireIdentity::new("openai_compatible", "a".repeat(64), 2).unwrap(),
        );
        admit_inference_invocation_with_first_provider_attempt(&pool, &plan, &attempt)
            .await
            .unwrap();
        finish_successful_inference_provider_attempt_and_invocation(
            &pool,
            &plan,
            &attempt,
            &InferenceInvocationTerminal::succeeded(InferenceUsage::default(), None),
        )
        .await
        .unwrap();
        sqlx::query(
            "UPDATE model_request_context_events SET created_at=DATE_SUB(NOW(6),INTERVAL 31 DAY)",
        )
        .execute(pool.get())
        .await
        .unwrap();
        // These consumer fixtures have no canonical owner and must be retained.
        // They fill the first raw page; a zero-deletion page must still advance.
        for chunk in (0..500).collect::<Vec<_>>().chunks(100) {
            let mut query = sqlx::QueryBuilder::<sqlx::MySql>::new(
                "INSERT INTO model_request_context_events (user_id,session_id,event_id,attempt_id,invocation_id,event_stage,terminal_status,topology,provider,model_family,purpose,event_json,created_at) ",
            );
            query.push_values(chunk, |mut row, index| {
                row.push("'orphan-owner'")
                    .push("'orphan-session'")
                    .push_bind(format!("event-{index}"))
                    .push_bind(format!("attempt-{index}"))
                    .push_bind(format!("invocation-{index}"))
                    .push("'accepted'")
                    .push("NULL")
                    .push("'server_only'")
                    .push("'openai'")
                    .push("'fixture'")
                    .push("'introspection'")
                    .push("'{}'")
                    .push("DATE_SUB(NOW(6),INTERVAL 32 DAY)");
            });
            query.build().execute(pool.get()).await.unwrap();
        }
        let lease = Arc::new(crate::server::sweeper_lease::SweeperLease::new(
            pool.clone(),
            "fixture-pod".into(),
            "maintenance-fixture".into(),
            "sweeper_leases".into(),
        ));
        let cancel = tokio_util::sync::CancellationToken::new();
        let handle = spawn_runtime_maintenance_sweeper(
            pool.clone(),
            None,
            lease,
            cancel.clone(),
            Duration::from_millis(10),
        );
        let drained = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM model_request_context_events WHERE user_id='settled-owner'").fetch_one(pool.get()).await.unwrap();
                if count == 0 { break; }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await;
        cancel.cancel();
        let stopped = tokio::time::timeout(Duration::from_secs(2), handle).await;
        let protected: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM model_request_context_events WHERE user_id='orphan-owner'",
        )
        .fetch_one(pool.get())
        .await
        .unwrap();
        pool.close().await;
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP DATABASE `{database}`")))
            .execute(control.get())
            .await
            .unwrap();
        control.close().await;
        assert!(
            drained.is_ok(),
            "the actual maintenance task must advance beyond a pinned page"
        );
        assert!(
            stopped.is_ok_and(|join| join.is_ok()),
            "cancellation must finish the task"
        );
        assert_eq!(protected, 500);
    }
}
