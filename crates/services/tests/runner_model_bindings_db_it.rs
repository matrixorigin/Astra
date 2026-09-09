//! ASTRA_TEST_DB_IT=1 cargo test -p astra-services --test runner_model_bindings_db_it -- --ignored --test-threads=1
mod common;

use std::sync::Arc;

use astra_services::runner_model_bindings::{
    AuthenticatedRunnerConnection, enroll_runner_inference, list_effective_runner_model_bindings,
    list_runner_model_catalog_bindings, publish_runner_binding, resolve_runner_model_binding,
    resolve_runner_offering, runner_offering_id,
};
use astra_services::{
    DatabaseModelService, FernetTokenEncryptor, ModelExecutionMaterial, ModelService,
};
use astra_turn_types::runner_inference::*;
use serial_test::serial;

fn id(value: &str) -> RunnerInferenceId {
    RunnerInferenceId::new(value).unwrap()
}

#[tokio::test]
#[ignore = "requires live MatrixOne"]
#[serial]
async fn reconnect_after_real_unregister_preserves_journal() {
    use astra_services::multi_agent::edge_registry::{
        DatabaseEdgeRegistryService, EdgeRegistryService,
    };
    let pool = common::setup_pool().await;
    let user = format!("runner-disconnect-{}", uuid::Uuid::new_v4());
    let old = register(&pool, &user).await;
    enroll_runner_inference(&pool, &old, 1, &id("journal"), &id("boot-1"))
        .await
        .unwrap();
    publish_runner_binding(&pool, &old, &publication("publish-1", 0, 1))
        .await
        .unwrap();
    let registry = DatabaseEdgeRegistryService::new(pool.get().clone());
    assert!(
        registry
            .unregister_generation(&user, "personal", "socket-1")
            .await
            .unwrap()
    );
    assert!(
        list_effective_runner_model_bindings(&pool, &user)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        !list_runner_model_catalog_bindings(&pool, &user)
            .await
            .unwrap()[0]
            .online
    );
    assert!(
        !registry
            .unregister_generation(&user, "personal", "socket-1")
            .await
            .unwrap()
    );
    assert!(
        publish_runner_binding(&pool, &old, &publication("stale-publish", 1, 2))
            .await
            .is_err()
    );
    for finalize in [false, true] {
        let failed = registry
            .register_or_update_with_lease(
                &user,
                "personal",
                "failed-socket",
                None,
                None,
                None,
                None,
            )
            .await
            .unwrap();
        assert!(failed.previous.is_none());
        if finalize {
            assert!(registry.finalize_registration(&failed).await.unwrap());
        }
        assert!(registry.rollback_registration(&failed).await.unwrap());
        assert!(!registry.rollback_registration(&failed).await.unwrap());
        assert!(
            list_effective_runner_model_bindings(&pool, &user)
                .await
                .unwrap()
                .is_empty()
        );
    }
    let reconnected = registry
        .register_or_update_with_lease(&user, "personal", "socket-2", None, None, None, None)
        .await
        .unwrap();
    assert!(registry.finalize_registration(&reconnected).await.unwrap());
    assert!(registry.release_registration(&reconnected).await.unwrap());
    let current = AuthenticatedRunnerConnection {
        edge_id: "socket-2".into(),
        ..old.clone()
    };
    enroll_runner_inference(&pool, &current, 1, &id("journal"), &id("boot-2"))
        .await
        .expect("ordinary disconnect must not retire the durable Runner journal");
    let replay = publish_runner_binding(&pool, &current, &publication("publish-1", 0, 1))
        .await
        .unwrap();
    assert_eq!(replay.publication_revision.get(), 1);
    assert_eq!(
        list_effective_runner_model_bindings(&pool, &user)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        !registry
            .unregister_generation(&user, "personal", "socket-1")
            .await
            .unwrap()
    );
    assert_eq!(
        list_effective_runner_model_bindings(&pool, &user)
            .await
            .unwrap()
            .len(),
        1
    );
    enroll_runner_inference(
        &pool,
        &current,
        1,
        &id("replacement-journal"),
        &id("boot-3"),
    )
    .await
    .unwrap();
    assert!(
        enroll_runner_inference(&pool, &current, 1, &id("journal"), &id("boot-4"))
            .await
            .is_err(),
        "explicit journal replacement must still fence the retired journal"
    );
}

fn publication(operation: &str, expected: u64, revision: u64) -> RunnerInferenceBindingPublication {
    serde_json::from_value(serde_json::json!({
        "protocol_version": 1, "operation_id": operation,
        "expected_publication_revision": expected,
        "change": {"action": "publish", "definition": {
            "identity": {"runner_id": "personal", "journal_id": "journal", "binding_id": "model",
                "binding_revision": revision, "profile_revision": 1},
            "display_name": "Work", "model_name": "public-model", "protocol": "openai_chat_completions",
            "context_window": 8192, "max_output_tokens": 1024
        }}
    }))
    .unwrap()
}

async fn register(pool: &astra_core::SharedPool, user: &str) -> AuthenticatedRunnerConnection {
    let connection = AuthenticatedRunnerConnection {
        user_id: user.into(),
        runner_id: id("personal"),
        edge_id: "socket-1".into(),
    };
    sqlx::query(
        "INSERT INTO edge_agent_registry
        (user_id, registry_id, edge_agent_id, edge_id, registration_state)
        VALUES (?, 'registry', 'personal', 'socket-1', 1)",
    )
    .bind(user)
    .execute(pool.get())
    .await
    .unwrap();
    connection
}

#[tokio::test]
#[ignore = "requires live MatrixOne"]
#[serial]
async fn runner_publication_receipts_are_owner_scoped_and_cannot_restore_disabled_inventory() {
    let pool = common::setup_pool().await;
    let user = format!("runner-binding-{}", uuid::Uuid::new_v4());
    let other_user = format!("runner-binding-{}", uuid::Uuid::new_v4());
    let connection = register(&pool, &user).await;
    let other = register(&pool, &other_user).await;
    let first = publication("publish-1", 0, 1);
    let identity = first.change.identity().clone();
    assert!(
        publish_runner_binding(&pool, &connection, &first)
            .await
            .is_err(),
        "tool registration is not inference enrollment"
    );
    enroll_runner_inference(&pool, &connection, 1, &id("journal"), &id("boot-1"))
        .await
        .unwrap();
    let receipt = publish_runner_binding(&pool, &connection, &first)
        .await
        .unwrap();
    assert_eq!(receipt.publication_revision.get(), 1);
    assert!(
        resolve_runner_model_binding(&pool, &other.user_id, &identity)
            .await
            .is_err()
    );
    assert!(publish_runner_binding(&pool, &other, &first).await.is_err());
    enroll_runner_inference(&pool, &other, 1, &id("journal"), &id("other-boot"))
        .await
        .unwrap();
    assert_eq!(
        publish_runner_binding(&pool, &other, &first).await.unwrap(),
        receipt,
        "receipt payload may match but each owner has independent authority"
    );
    let mut conflicting = first.clone();
    conflicting.expected_publication_revision = 99;
    assert!(
        publish_runner_binding(&pool, &connection, &conflicting)
            .await
            .is_err()
    );
    let disable = RunnerInferenceBindingPublication {
        protocol_version: 1,
        operation_id: id("disable"),
        expected_publication_revision: 1,
        change: RunnerInferenceBindingChange::Disable {
            identity: identity.clone(),
        },
    };
    publish_runner_binding(&pool, &connection, &disable)
        .await
        .unwrap();
    assert_eq!(
        publish_runner_binding(&pool, &connection, &first)
            .await
            .unwrap(),
        receipt
    );
    assert!(
        resolve_runner_model_binding(&pool, &user, &identity)
            .await
            .is_err(),
        "replayed historical receipt cannot re-enable"
    );
    assert!(
        resolve_runner_model_binding(&pool, &other_user, &identity)
            .await
            .is_ok(),
        "disable is owner scoped"
    );
    let next = publication("publish-2", 2, 2);
    publish_runner_binding(&pool, &connection, &next)
        .await
        .unwrap();
    let offering = runner_offering_id(&user, &identity);
    assert_eq!(
        offering,
        runner_offering_id(&user, next.change.identity()),
        "rotation retains the target Offering"
    );
    assert_ne!(
        offering,
        runner_offering_id(&other_user, &identity),
        "Offering identity is personal"
    );
    let refreshed = resolve_runner_offering(&pool, &user, &offering)
        .await
        .unwrap();
    assert_eq!(&refreshed.definition.identity, next.change.identity());
    let catalog = list_effective_runner_model_bindings(&pool, &user)
        .await
        .unwrap();
    assert_eq!(catalog.len(), 1);
    assert_eq!(catalog[0].catalog_item().offering_id, offering);
    assert!(
        resolve_runner_model_binding(&pool, &user, &identity)
            .await
            .is_err(),
        "old binding revision fenced"
    );
    assert!(
        resolve_runner_model_binding(&pool, &user, next.change.identity())
            .await
            .is_ok()
    );
}

#[tokio::test]
#[ignore = "requires live MatrixOne"]
#[serial]
async fn runner_reconnect_requires_executor_enrollment_but_preserves_publication_receipts() {
    let pool = common::setup_pool().await;
    let user = format!("runner-reconnect-{}", uuid::Uuid::new_v4());
    let old = register(&pool, &user).await;
    enroll_runner_inference(&pool, &old, 1, &id("journal"), &id("boot-1"))
        .await
        .unwrap();
    let first = publication("publish-1", 0, 1);
    let receipt = publish_runner_binding(&pool, &old, &first).await.unwrap();
    sqlx::query("UPDATE edge_agent_registry SET edge_id = 'socket-2' WHERE user_id = ?")
        .bind(&user)
        .execute(pool.get())
        .await
        .unwrap();
    assert!(
        list_effective_runner_model_bindings(&pool, &user)
            .await
            .unwrap()
            .is_empty(),
        "a superseded socket is never selectable"
    );
    let known = list_runner_model_catalog_bindings(&pool, &user)
        .await
        .unwrap();
    assert_eq!(known.len(), 1);
    assert!(!known[0].online);
    assert!(!known[0].catalog_item().is_active);
    assert!(
        known[0]
            .catalog_item()
            .description
            .as_deref()
            .is_some_and(|description| description.contains("offline")),
        "known capacity carries a repairable unavailable projection"
    );
    let current = AuthenticatedRunnerConnection {
        edge_id: "socket-2".into(),
        ..old.clone()
    };
    assert!(publish_runner_binding(&pool, &old, &first).await.is_err());
    assert!(
        resolve_runner_model_binding(&pool, &user, first.change.identity())
            .await
            .is_err()
    );
    assert!(
        publish_runner_binding(&pool, &current, &first)
            .await
            .is_err()
    );
    assert!(
        enroll_runner_inference(&pool, &current, 2, &id("journal"), &id("boot-2"))
            .await
            .is_err()
    );
    enroll_runner_inference(&pool, &current, 1, &id("journal"), &id("boot-2"))
        .await
        .unwrap();
    assert_eq!(
        publish_runner_binding(&pool, &current, &first)
            .await
            .unwrap(),
        receipt
    );
    let resolved = resolve_runner_model_binding(&pool, &user, first.change.identity())
        .await
        .unwrap();
    assert_eq!(resolved.process_boot_nonce, id("boot-2"));
    let mut forged = publication("forged", 1, 2);
    if let RunnerInferenceBindingChange::Publish { definition } = &mut forged.change {
        definition.identity.runner_id = id("someone-else");
    }
    assert!(
        publish_runner_binding(&pool, &current, &forged)
            .await
            .is_err()
    );
    enroll_runner_inference(&pool, &current, 1, &id("new-journal"), &id("boot-3"))
        .await
        .unwrap();
    assert!(
        resolve_runner_model_binding(&pool, &user, first.change.identity())
            .await
            .is_err()
    );
    assert!(
        enroll_runner_inference(&pool, &current, 1, &id("journal"), &id("boot-4"))
            .await
            .is_err(),
        "retired journal cannot restore stale inventory"
    );
}

#[tokio::test]
#[ignore = "requires live MatrixOne"]
#[serial]
async fn effective_model_catalog_and_admission_are_authenticated_owner_scoped() {
    let (pool, settings) = common::setup_pool_and_settings().await;
    let user = format!("runner-catalog-{}", uuid::Uuid::new_v4());
    let other_user = format!("runner-catalog-{}", uuid::Uuid::new_v4());
    let connection = register(&pool, &user).await;
    let other = register(&pool, &other_user).await;
    for connection in [&connection, &other] {
        enroll_runner_inference(
            &pool,
            connection,
            RUNNER_INFERENCE_PROTOCOL_VERSION,
            &id("journal"),
            &id("boot"),
        )
        .await
        .unwrap();
        publish_runner_binding(&pool, connection, &publication("publish", 0, 1))
            .await
            .unwrap();
    }
    let offering_id = runner_offering_id(&user, publication("identity", 0, 1).change.identity());
    let other_offering_id = runner_offering_id(
        &other_user,
        publication("other-identity", 0, 1).change.identity(),
    );
    let service = DatabaseModelService::new(
        settings.clone(),
        Arc::new(FernetTokenEncryptor::new("runner-catalog-test-key").unwrap()),
    )
    .with_pool(pool.clone());

    // Delegated selection and durable recovery call the shared resolver
    // directly, without a ModelService or inherited execution snapshot.
    let encryptor = FernetTokenEncryptor::new("runner-catalog-test-key").unwrap();
    for _ in 0..2 {
        let restored = astra_services::revalidate_admitted_model_execution(
            &settings,
            &encryptor,
            &user,
            &offering_id,
            Some(&pool),
        )
        .await
        .unwrap();
        assert_eq!(restored.offering_id, offering_id);
        assert!(matches!(
            restored.execution_material,
            ModelExecutionMaterial::Runner(_)
        ));
    }
    assert!(
        astra_services::revalidate_admitted_model_execution(
            &settings,
            &encryptor,
            &other_user,
            &offering_id,
            Some(&pool),
        )
        .await
        .is_err()
    );

    let catalog = service.list_models(user.clone(), false).await.unwrap();
    assert!(catalog.iter().any(|item| item.offering_id == offering_id));
    assert!(
        !catalog
            .iter()
            .any(|item| item.offering_id == other_offering_id)
    );

    let admitted = service
        .revalidate_model_execution(user.clone(), offering_id.clone())
        .await
        .unwrap();
    assert!(matches!(
        admitted.execution_material,
        ModelExecutionMaterial::Runner(_)
    ));
    let error = service
        .revalidate_model_execution(other_user, offering_id.clone())
        .await
        .expect_err("another principal must not resolve this personal Offering");
    assert_eq!(error.0, axum::http::StatusCode::NOT_FOUND);

    sqlx::query("UPDATE edge_agent_registry SET last_heartbeat_at = DATE_SUB(NOW(6), INTERVAL 120 SECOND) WHERE user_id = ?")
        .bind(&user).execute(pool.get()).await.unwrap();
    assert!(
        astra_services::revalidate_admitted_model_execution(
            &settings,
            &encryptor,
            &user,
            &offering_id,
            Some(&pool),
        )
        .await
        .is_err(),
        "offline Runner cannot fall back to Server credentials"
    );

    sqlx::query("UPDATE edge_agent_registry SET last_heartbeat_at = NOW(6) WHERE user_id = ?")
        .bind(&user)
        .execute(pool.get())
        .await
        .unwrap();
    publish_runner_binding(
        &pool,
        &connection,
        &RunnerInferenceBindingPublication {
            protocol_version: RUNNER_INFERENCE_PROTOCOL_VERSION,
            operation_id: id("disable-catalog"),
            expected_publication_revision: 1,
            change: RunnerInferenceBindingChange::Disable {
                identity: publication("identity", 0, 1).change.identity().clone(),
            },
        },
    )
    .await
    .unwrap();
    assert!(
        astra_services::revalidate_admitted_model_execution(
            &settings,
            &encryptor,
            &user,
            &offering_id,
            Some(&pool),
        )
        .await
        .is_err(),
        "disabled Runner cannot reuse previously admitted material"
    );
}
