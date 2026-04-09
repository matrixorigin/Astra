//! MatrixOne snapshot SQL helpers.
//!
//! Snapshots should target the specific database, not the entire account/cluster.
//! Identifiers are backtick-quoted, and embedded backticks are escaped to prevent
//! SQL injection.
//! Syntax: ``CREATE SNAPSHOT `{name}` FOR DATABASE `{db}` ``
//! Restore: ``RESTORE ACCOUNT `{account}` DATABASE `{db}` FROM SNAPSHOT `{name}` ``

use std::sync::OnceLock;

use sqlx::Row;

/// Validate a SQL identifier: non-empty, alphanumeric + underscore only.
/// Rejects backticks, quotes, spaces, and other special characters.
pub fn validate_sql_identifier(value: &str, label: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("empty {label}"));
    }
    if !value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(format!(
            "invalid {label} '{value}': only [a-zA-Z0-9_] allowed"
        ));
    }
    Ok(())
}

fn quote_identifier(value: &str) -> String {
    format!("`{}`", value.replace('`', "``"))
}

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
    let name: String = row
        .try_get("name")
        .map_err(|e| format!("resolve_account_name column: {e}"))?;
    Ok(ACCOUNT_NAME.get_or_init(|| name).clone())
}

/// ``CREATE SNAPSHOT `{name}` FOR DATABASE `{db}` ``.
///
/// All identifiers are backtick-quoted, with embedded backticks escaped.
pub fn create_snapshot_for_db_sql(name: &str, db: &str) -> String {
    format!(
        "CREATE SNAPSHOT {} FOR DATABASE {}",
        quote_identifier(name),
        quote_identifier(db)
    )
}

/// ``RESTORE ACCOUNT `{account}` DATABASE `{db}` FROM SNAPSHOT `{snap}` ``.
///
/// All identifiers are backtick-quoted, with embedded backticks escaped.
pub fn restore_snapshot_db_sql(snapshot: &str, account: &str, db: &str) -> String {
    format!(
        "RESTORE ACCOUNT {} DATABASE {} FROM SNAPSHOT {}",
        quote_identifier(account),
        quote_identifier(db),
        quote_identifier(snapshot)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_snapshot_for_database() {
        assert_eq!(
            create_snapshot_for_db_sql("sp1", "astra_runtime"),
            "CREATE SNAPSHOT `sp1` FOR DATABASE `astra_runtime`"
        );
    }

    #[test]
    fn restore_snapshot_for_database() {
        assert_eq!(
            restore_snapshot_db_sql("sp1", "sys", "astra_runtime"),
            "RESTORE ACCOUNT `sys` DATABASE `astra_runtime` FROM SNAPSHOT `sp1`"
        );
    }

    #[test]
    fn create_snapshot_escapes_backticks() {
        assert_eq!(
            create_snapshot_for_db_sql("sp`1", "astra`runtime"),
            "CREATE SNAPSHOT `sp``1` FOR DATABASE `astra``runtime`"
        );
    }

    #[test]
    fn restore_snapshot_escapes_backticks() {
        assert_eq!(
            restore_snapshot_db_sql("sp`1", "sy`s", "astra`runtime"),
            "RESTORE ACCOUNT `sy``s` DATABASE `astra``runtime` FROM SNAPSHOT `sp``1`"
        );
    }

    #[test]
    fn validate_sql_identifier_accepts_valid() {
        assert!(validate_sql_identifier("task_123", "name").is_ok());
        assert!(validate_sql_identifier("astra_runtime", "db").is_ok());
        assert!(validate_sql_identifier("sys", "account").is_ok());
    }

    #[test]
    fn validate_sql_identifier_rejects_injection() {
        assert!(validate_sql_identifier("", "name").is_err());
        assert!(validate_sql_identifier("x'; DROP--", "name").is_err());
        assert!(validate_sql_identifier("has spaces", "name").is_err());
        assert!(validate_sql_identifier("back`tick", "name").is_err());
        assert!(validate_sql_identifier("path/sep", "name").is_err());
    }
}
