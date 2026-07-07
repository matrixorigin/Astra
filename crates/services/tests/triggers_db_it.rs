mod common;

use astra_services::{
    DatabaseTriggerService, TriggerCreateRequestData, TriggerService, WebhookFireData,
};
use axum::http::StatusCode;
use serial_test::serial;
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn database_trigger_corrupt_context_and_missing_secret_fail_loud() {
    let (shared_pool, settings) = common::setup_pool_and_settings().await;
    let pool = shared_pool.get().clone();
    let service = DatabaseTriggerService::new(settings).with_pool(shared_pool);
    let user_id = Uuid::new_v4().to_string();

    let trigger = service
        .create_trigger(
            user_id.clone(),
            TriggerCreateRequestData {
                trigger_type: "webhook".to_string(),
                name: format!("trigger-{}", Uuid::new_v4().simple()),
                agent_id: "agent-db-it".to_string(),
                user_input: "run unhappy trigger path".to_string(),
                context: Some(serde_json::json!({"source": "triggers-db-it"})),
                cron_expr: None,
                session_id: None,
            },
        )
        .await
        .expect("create trigger");
    let secret = trigger.secret.clone().expect("webhook secret");

    sqlx::query("UPDATE wf_triggers SET context = ? WHERE trigger_id = ?")
        .bind("{not valid json")
        .bind(&trigger.trigger_id)
        .execute(&pool)
        .await
        .expect("corrupt context");

    let err = service
        .list_triggers(user_id.clone())
        .await
        .expect_err("invalid persisted trigger context must fail loudly");
    assert_eq!(err.0, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        err.1.detail.contains("wf_triggers.context"),
        "unexpected error detail: {}",
        err.1.detail
    );

    sqlx::query("UPDATE wf_triggers SET context = NULL, secret = NULL WHERE trigger_id = ?")
        .bind(&trigger.trigger_id)
        .execute(&pool)
        .await
        .expect("remove webhook secret");

    let err = service
        .fire_webhook(
            trigger.trigger_id.clone(),
            WebhookFireData {
                secret,
                payload: None,
            },
        )
        .await
        .expect_err("missing persisted webhook secret must fail loudly");
    assert_eq!(err.0, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        err.1.detail.contains("wf_triggers.secret"),
        "unexpected error detail: {}",
        err.1.detail
    );

    let _ = sqlx::query("DELETE FROM wf_triggers WHERE trigger_id = ?")
        .bind(&trigger.trigger_id)
        .execute(&pool)
        .await;
}
