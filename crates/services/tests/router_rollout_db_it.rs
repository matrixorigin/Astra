//! Real MatrixOne deployment CAS, audit and owner isolation.
mod common;
#[path = "common/router_deployment.rs"]
mod fixture;
#[path = "common/isolated_database.rs"]
mod isolated_database;
use astra_services::tuning::rollout::*;
use fixture::deployment;

#[test]
fn dashboard_preserves_unknown_outcomes_and_all_operational_failures() {
    let d = deployment("owner");
    let state = RouterRolloutState {
        revision: 1,
        deployment: Some(d.clone()),
        ..Default::default()
    };
    let first = RolloutRun {
        run_id: "first".into(),
        session_id: "session".into(),
        status: "failed".into(),
        selected_offering_id: "strong".into(),
        economy_offering_id: "economy".into(),
        reason: astra_turn_types::model_routing::ModelRoutingReason::LearnedAbstention,
        rollout: RouterRolloutDecision {
            deployment_id: d.deployment_id.clone(),
            revision: 1,
            candidate_sha256: d.tuning.candidate_sha256.clone(),
            rubric_version: "rubric-1".into(),
            routing_failure: None,
            cohort: RolloutCohort::Shadow,
            cohort_probability_basis_points: 10_000,
            proposed_offering_id: "strong".into(),
            abstained: true,
            admission_rejected: false,
            routing_overhead_us: 100,
        },
        outcome: None,
    };
    let mut later = first.clone();
    later.run_id = "later".into();
    later.status = "completed".into();
    later.rollout.admission_rejected = true;
    later.rollout.routing_overhead_us = 10_000;
    later.outcome = Some(ReviewedOutcomeRecord {
        deployment_id: d.deployment_id,
        reviewed_by: "reviewer".into(),
        reviewed_at: chrono::Utc::now(),
        outcome: RouterReviewedOutcome {
            rubric_version: d.rubric_version,
            evidence_reference: "review".into(),
            acceptable: Some(true),
            corrected: Some(false),
            full_episode_cost_usd: Some(0.1),
            episode_latency_ms: Some(100),
            critical_violation: true,
        },
    });
    let report = dashboard(&state, &[first.clone(), later.clone()], false);
    let m = &report.cohorts["shadow"];
    assert_eq!((m.observed_runs, m.sessions, m.completed), (2, 1, 0));
    assert_eq!((m.known_quality, m.known_cost), (0, 0));
    assert_eq!(m.cost_per_acceptable_task, None);
    assert_eq!((m.admission_failures, m.critical_violations), (1, 1));
    assert_eq!(m.p95_routing_overhead_us, Some(10_000));

    // A failure's spend remains in the numerator when coverage is complete.
    let mut failure = first;
    failure.outcome = later.outcome.clone();
    failure.outcome.as_mut().unwrap().outcome.acceptable = Some(false);
    failure
        .outcome
        .as_mut()
        .unwrap()
        .outcome
        .full_episode_cost_usd = Some(0.2);
    later.session_id = "independent-session".into();
    let report = dashboard(&state, &[failure, later], false);
    assert!((report.cohorts["shadow"].cost_per_acceptable_task.unwrap() - 0.3).abs() < 1e-9);
}

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1 and explicitly isolated ASTRA_TEST_DATABASE"]
async fn router_rollout_transactions_are_atomic_audited_and_owner_scoped() {
    isolated_database::require_isolated_database(&common::require_db_it_env().database);
    let pool = common::setup_pool().await;
    let owner = format!("router-{}", uuid::Uuid::new_v4());
    let actor = format!("admin-{}", uuid::Uuid::new_v4());
    let store = DatabaseRouterRolloutStore(pool.clone());
    let d = deployment(&owner);
    assert_eq!(store.load(&owner).await.unwrap().revision, 0);
    let first = store
        .change(
            &owner,
            0,
            &actor,
            RolloutChange::Publish(Box::new(d.clone())),
        )
        .await
        .unwrap();
    assert_eq!(first.revision, 1);
    assert_eq!(store.load("unrelated-owner").await.unwrap().revision, 0);
    let (a, b) = tokio::join!(
        store.change(
            &owner,
            1,
            &actor,
            RolloutChange::Canary { basis_points: 100 }
        ),
        store.change(
            &owner,
            1,
            &actor,
            RolloutChange::Rollback {
                reason: "operator_stop".into()
            }
        ),
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    assert_eq!(store.load(&owner).await.unwrap().revision, 2);
    assert!(
        store
            .change(
                &owner,
                1,
                &actor,
                RolloutChange::Canary { basis_points: 100 }
            )
            .await
            .is_err()
    );
    // Safety stops target the deployment under lock, even if another operation
    // advanced its revision after the reviewer loaded it.
    let stopped = store
        .change(
            &owner,
            1,
            &actor,
            RolloutChange::CriticalViolation {
                deployment_id: d.deployment_id.clone(),
            },
        )
        .await
        .unwrap();
    assert_eq!(stopped.revision, 3);
    assert_eq!(
        stopped.deployment.as_ref().unwrap().mode,
        RolloutMode::RolledBack
    );
    assert_eq!(
        stopped.deployment.as_ref().unwrap().stop_reason.as_deref(),
        Some("critical_violation")
    );
    assert!(
        store
            .change(
                &owner,
                1,
                &actor,
                RolloutChange::CriticalViolation {
                    deployment_id: "different-deployment".into(),
                }
            )
            .await
            .is_err()
    );
    let revoked = store
        .change(
            &owner,
            3,
            &actor,
            RolloutChange::Revoke {
                source_ids: vec!["source-1".into()],
            },
        )
        .await
        .unwrap();
    assert_eq!(
        revoked.deployment.as_ref().unwrap().mode,
        RolloutMode::RolledBack
    );
    assert!(
        store
            .change(
                &owner,
                4,
                &actor,
                RolloutChange::Publish(Box::new(d.clone()))
            )
            .await
            .is_err()
    );
    assert!(
        store
            .change(
                "different-owner",
                0,
                &actor,
                RolloutChange::Publish(Box::new(d))
            )
            .await
            .is_err()
    );
    let audits: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM auth_audit_logs WHERE user_id = ? AND action = 'router_rollout'",
    )
    .bind(&actor)
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert_eq!(
        audits, 4,
        "failed CAS/validation must not commit audit or state"
    );
    assert_eq!(store.load(&owner).await.unwrap().revision, 4);
    let (runs, truncated) = store.runs(&owner, &revoked).await.unwrap();
    assert!(runs.is_empty() && !truncated);
    sqlx::query("DELETE FROM model_router_deployments WHERE user_id = ?")
        .bind(&owner)
        .execute(pool.get())
        .await
        .unwrap();
    sqlx::query("DELETE FROM auth_audit_logs WHERE user_id = ? AND action = 'router_rollout'")
        .bind(&actor)
        .execute(pool.get())
        .await
        .unwrap();
}
