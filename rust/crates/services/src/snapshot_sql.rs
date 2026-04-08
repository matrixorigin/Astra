//! MatrixOne snapshot SQL helpers.
//!
//! Snapshots should target the specific database, not the entire account/cluster.
//! Syntax: `CREATE SNAPSHOT {name} FOR DATABASE {db}`
//! Restore: `RESTORE ACCOUNT {account} DATABASE {db} FROM SNAPSHOT {name}`

use std::sync::OnceLock;

use sqlx::Row;

/// Cached account name — queried once per process via `SELECT current_account_name()`.
static ACCOUNT_NAME: OnceLock<String> = OnceLock::new();

/// Resolve the current MatrixOne account name, caching the result for the process lifetime.
pub async fn resolve_account_name(pool: &sqlx::Pool<sqlx::MySql>) -> Result<String, String> {
    if let Some(name) = ACCOUNT_NAME.get() {
        return Ok(name.clone());
    }
    let row = sqlx::query("SELECT current_account_name() AS name")
        .fetch_one(pool)
        .await
        .map_err(|e| format!("resolve_account_name: {e}"))?;
    let name: String = row.try_get("name").map_err(|e| format!("resolve_account_name column: {e}"))?;
    Ok(ACCOUNT_NAME.get_or_init(|| name).clone())
}

/// `CREATE SNAPSHOT {name} FOR DATABASE {db}`.
pub fn create_snapshot_for_db_sql(name: &str, db: &str) -> String {
    format!("CREATE SNAPSHOT {name} FOR DATABASE {db}")
}

/// `RESTORE ACCOUNT {account} DATABASE {db} FROM SNAPSHOT {snap}`.
pub fn restore_snapshot_db_sql(snapshot: &str, account: &str, db: &str) -> String {
    format!("RESTORE ACCOUNT {account} DATABASE {db} FROM SNAPSHOT {snapshot}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_snapshot_for_database() {
        assert_eq!(
            create_snapshot_for_db_sql("sp1", "astra_runtime"),
            "CREATE SNAPSHOT sp1 FOR DATABASE astra_runtime"
        );
    }

    #[test]
    fn restore_snapshot_for_database() {
        assert_eq!(
            restore_snapshot_db_sql("sp1", "sys", "astra_runtime"),
            "RESTORE ACCOUNT sys DATABASE astra_runtime FROM SNAPSHOT sp1"
        );
    }
}
