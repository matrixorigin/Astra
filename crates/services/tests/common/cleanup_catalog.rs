//! Per-test catalog for tests that exercise global retention sweeps.

use astra_core::{MatrixOneSettings, SharedPool};
use futures_util::FutureExt;
use sqlx::MySqlPool;
use std::{future::Future, panic::AssertUnwindSafe};

pub struct CleanupCatalog {
    pub pool: SharedPool,
    pub settings: MatrixOneSettings,
    admin: MySqlPool,
}

impl CleanupCatalog {
    pub async fn create() -> Self {
        let mut settings = super::require_db_it_env();
        assert_eq!(
            std::env::var("ASTRA_TEST_DATABASE").as_deref(),
            Ok(settings.database.as_str()),
            "global cleanup tests require an explicitly designated test database"
        );
        let catalog =
            std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG").unwrap_or_else(|_| "mysql".into());
        let mut admin_settings = settings.clone();
        admin_settings.database = catalog.clone();
        let admin = sqlx::mysql::MySqlPoolOptions::new()
            .max_connections(1)
            .connect(&admin_settings.database_url_with_password())
            .await
            .expect("connect bootstrap catalog");
        settings.database = format!("astra_cleanup_it_{}", uuid::Uuid::new_v4().simple());
        settings.db_pool_max_connections = 8;
        settings.db_pool_min_connections = 0;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "CREATE DATABASE `{}`",
            settings.database
        )))
        .execute(&admin)
        .await
        .expect("create isolated cleanup catalog");
        let setup = async {
            astra_services::storage::ensure_core_schema(&settings, &catalog).await?;
            SharedPool::new(&settings).await
        }
        .await;
        let pool = match setup {
            Ok(pool) => pool,
            Err(error) => {
                sqlx::query(sqlx::AssertSqlSafe(format!(
                    "DROP DATABASE `{}`",
                    settings.database
                )))
                .execute(&admin)
                .await
                .expect("remove failed cleanup catalog");
                panic!("initialize isolated cleanup catalog: {error}");
            }
        };
        Self {
            pool,
            settings,
            admin,
        }
    }

    pub async fn finish(self) {
        self.pool.get().close().await;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE `{}`",
            self.settings.database
        )))
        .execute(&self.admin)
        .await
        .expect("remove isolated cleanup catalog");
    }

    /// Always drop this test's catalog after assertions, including on panic.
    pub async fn run(self, test: impl Future<Output = ()>) {
        let result = AssertUnwindSafe(test).catch_unwind().await;
        self.finish().await;
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
    }
}
