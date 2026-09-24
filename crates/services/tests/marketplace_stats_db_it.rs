mod common;

use astra_services::{DatabaseMarketplaceStatsService, MarketplaceStatsService, SkillSearchQuery};
use axum::http::StatusCode;
use serial_test::serial;
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn database_marketplace_stats_rejects_corrupt_required_fields() {
    let (shared_pool, settings) = common::setup_pool_and_settings().await;
    let pool = shared_pool.get().clone();
    let service = DatabaseMarketplaceStatsService::new(settings).with_pool(shared_pool);
    let skill_id = Uuid::new_v4().to_string();
    let metric_id = Uuid::new_v4().to_string();
    let skill_name = format!("stats_skill_{}", Uuid::new_v4().simple());
    let description = format!("stats description {}", Uuid::new_v4().simple());
    let installation_users: Vec<_> = (0..7)
        .map(|_| format!("stats-install-user-{}", Uuid::new_v4().simple()))
        .collect();

    sqlx::query(
        "INSERT INTO skills_registry \
         (skill_id, skill_name, version, description, category, status, source, is_active, is_public) \
         VALUES (?, ?, '1.0.0', ?, 'testing', 'active', 'integration_test', 1, 1)",
    )
    .bind(&skill_id)
    .bind(&skill_name)
    .bind(&description)
    .execute(&pool)
    .await
    .expect("insert skill registry row");

    for installation_user in &installation_users {
        sqlx::query(
            "INSERT INTO skill_installations \
             (installation_id, user_id, skill_name, skill_version, status, installed_at, updated_at) \
             VALUES (?, ?, ?, '1.0.0', 'installed', NOW(6), NOW(6))",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(installation_user)
        .bind(&skill_name)
        .execute(&pool)
        .await
        .expect("insert skill installation");
    }

    sqlx::query(
        "INSERT INTO skill_metrics \
         (metric_id, skill_name, metric_type, metric_slot, publisher_id, total_installs, \
          active_users_7d, avg_quality, avg_rating, report_count, compatibility_score, trust_tier) \
         VALUES (?, ?, 'aggregate', 'aggregate', 'publisher-it', 99, 3, 0.9, 0.8, 2, 0.95, 'verified')",
    )
    .bind(&metric_id)
    .bind(&skill_name)
    .execute(&pool)
    .await
    .expect("insert aggregate metric row");

    let search = service
        .search_ranked(SkillSearchQuery {
            query: Some(description.clone()),
            category: None,
            trust_tier: None,
            limit: Some(10),
            after_ranking_score: None,
            after_skill_name: None,
            after_version: None,
        })
        .await
        .expect("search valid marketplace row");
    assert_eq!(search.results.len(), 1);
    assert_eq!(search.results[0].skill_name, skill_name);
    assert_eq!(search.results[0].total_installs, 7);

    let stats = service
        .get_skill_stats(skill_name.clone())
        .await
        .expect("get valid marketplace stats");
    assert_eq!(stats.total_installs, 7);
    assert_eq!(stats.trust_tier.as_deref(), Some("verified"));

    sqlx::query("UPDATE skills_registry SET version = '' WHERE skill_id = ?")
        .bind(&skill_id)
        .execute(&pool)
        .await
        .expect("corrupt skills_registry.version");

    let err = service
        .search_ranked(SkillSearchQuery {
            query: Some(description),
            category: None,
            trust_tier: None,
            limit: Some(10),
            after_ranking_score: None,
            after_skill_name: None,
            after_version: None,
        })
        .await
        .expect_err("empty persisted skill version must fail loudly");
    assert_eq!(err.0, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        err.1.detail.contains("skills_registry.version"),
        "unexpected error detail: {}",
        err.1.detail
    );

    sqlx::query("UPDATE skill_metrics SET trust_tier = '' WHERE metric_id = ?")
        .bind(&metric_id)
        .execute(&pool)
        .await
        .expect("corrupt skill_metrics.trust_tier");

    let err = service
        .get_skill_stats(skill_name.clone())
        .await
        .expect_err("empty persisted trust_tier must fail loudly");
    assert_eq!(err.0, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        err.1.detail.contains("skill_metrics.trust_tier"),
        "unexpected error detail: {}",
        err.1.detail
    );

    let _ = sqlx::query("DELETE FROM skill_metrics WHERE metric_id = ?")
        .bind(&metric_id)
        .execute(&pool)
        .await;
    let _ = sqlx::query("DELETE FROM skill_installations WHERE skill_name = ?")
        .bind(&skill_name)
        .execute(&pool)
        .await;
    let _ = sqlx::query("DELETE FROM skills_registry WHERE skill_id = ?")
        .bind(&skill_id)
        .execute(&pool)
        .await;
}
