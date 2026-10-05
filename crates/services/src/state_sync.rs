//! Owner-scoped user preferences in MatrixOne.
//!
//! CLI clients use the authenticated Server REST surface. Updates keep a
//! version counter; it is metadata, not a compare-and-swap admission token.

use astra_core::is_duplicate_key_error;
use sqlx::Row;

trait PreferenceSyncRow {
    fn string_column(&self, column: &str) -> Result<String, sqlx::Error>;
    fn i32_column(&self, column: &str) -> Result<i32, sqlx::Error>;
}

impl PreferenceSyncRow for sqlx::mysql::MySqlRow {
    fn string_column(&self, column: &str) -> Result<String, sqlx::Error> {
        self.try_get(column)
    }

    fn i32_column(&self, column: &str) -> Result<i32, sqlx::Error> {
        self.try_get(column)
    }
}

fn decode_existing_preference_row(row: &impl PreferenceSyncRow) -> Result<(String, i32), String> {
    let value = row
        .string_column("pref_value")
        .map_err(|e| format!("push_pref decode pref_value: {e}"))?;
    let version = row
        .i32_column("version")
        .map_err(|e| format!("push_pref decode version: {e}"))?;
    Ok((value, version))
}

fn decode_preference_pair(row: &impl PreferenceSyncRow) -> Result<(String, String), String> {
    let key = row
        .string_column("pref_key")
        .map_err(|e| format!("pull_all_prefs decode pref_key: {e}"))?;
    let value = row
        .string_column("pref_value")
        .map_err(|e| format!("pull_all_prefs decode pref_value: {e}"))?;
    Ok((key, value))
}

fn next_preference_version(old_version: i32) -> Result<i32, String> {
    old_version
        .checked_add(1)
        .ok_or_else(|| format!("preference version overflow: {old_version}"))
}

const MAX_PREFERENCE_SYNC_ROWS: i64 = 128;

pub struct MatrixOneSyncService {
    pool: sqlx::Pool<sqlx::MySql>,
}

impl MatrixOneSyncService {
    pub fn new(pool: sqlx::Pool<sqlx::MySql>) -> Self {
        Self { pool }
    }

    pub async fn push_preference(
        &self,
        user_id: &str,
        key: &str,
        value: &str,
    ) -> Result<(), String> {
        let pref_id = uuid::Uuid::new_v4().to_string();

        // Read the current value and update version.
        let old_row = sqlx::query(
            "SELECT pref_value, version FROM user_preferences WHERE user_id = ? AND pref_key = ?",
        )
        .bind(user_id)
        .bind(key)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| format!("push_pref read existing preference: {e}"))?;

        let (old_value, old_version): (Option<String>, i32) = match &old_row {
            Some(row) => {
                let (value, version) = decode_existing_preference_row(row)?;
                (Some(value), version)
            }
            None => (None, 0),
        };

        // Skip write if value unchanged
        if old_value.as_deref() == Some(value) {
            return Ok(());
        }

        let new_version = next_preference_version(old_version)?;

        // Upsert with version increment
        let update_result = sqlx::query(
            "UPDATE user_preferences SET pref_value = ?, version = ?, updated_at = NOW() \
             WHERE user_id = ? AND pref_key = ?",
        )
        .bind(value)
        .bind(new_version)
        .bind(user_id)
        .bind(key)
        .execute(&self.pool)
        .await;

        let result = match update_result {
            Ok(r) if r.rows_affected() > 0 => Ok(r),
            Ok(_) => {
                let inserted = sqlx::query(
                    "INSERT INTO user_preferences (pref_id, user_id, pref_key, pref_value, version, updated_at) \
                     VALUES (?, ?, ?, ?, ?, NOW())",
                )
                .bind(&pref_id)
                .bind(user_id)
                .bind(key)
                .bind(value)
                .bind(new_version)
                .execute(&self.pool)
                .await;

                match inserted {
                    Ok(r) => Ok(r),
                    Err(e) if is_duplicate_key_error(&e) => {
                        sqlx::query(
                            "UPDATE user_preferences SET pref_value = ?, version = ?, updated_at = NOW() \
                             WHERE user_id = ? AND pref_key = ?",
                        )
                        .bind(value)
                        .bind(new_version)
                        .bind(user_id)
                        .bind(key)
                        .execute(&self.pool)
                        .await
                    }
                    Err(e) => Err(e),
                }
            }
            Err(e) => Err(e),
        };

        match result {
            Ok(_) => Ok(()),
            Err(e) => Err(format!("push_pref: {e}")),
        }
    }

    pub async fn pull_all_preferences(
        &self,
        user_id: &str,
    ) -> Result<Vec<(String, String)>, String> {
        let rows = sqlx::query(
            "SELECT pref_key, pref_value \
             FROM user_preferences \
             WHERE user_id = ? \
             ORDER BY pref_key \
             LIMIT ?",
        )
        .bind(user_id)
        .bind(MAX_PREFERENCE_SYNC_ROWS)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| format!("pull_all_prefs: {e}"))?;

        rows.iter().map(decode_preference_pair).collect()
    }
}

/// Well-known keys consumed by CLI preference projection.
pub mod pref_keys {
    pub const EXPLAIN_MODE: &str = "explain_mode";
    /// JSON array of persistently blocked tool names (survives across sessions).
    pub const BLOCKED_TOOLS: &str = "blocked_tools";
    /// Background memory-extraction agent. "true"/"false". Default: true.
    pub const AUTO_MEMORY_ENABLED: &str = "auto_memory_enabled";
    /// Desktop notifications on turn completion. "true"/"false". Default: true.
    pub const NOTIFICATIONS_ENABLED: &str = "notifications_enabled";
    /// Notification delivery method: "auto", "osc9", "bell", or "off". Default: "auto".
    pub const NOTIFICATION_METHOD: &str = "notification_method";
    /// Minimum elapsed seconds before a desktop notification is sent. Default: 10.
    pub const NOTIFICATION_THRESHOLD_SECS: &str = "notification_threshold_secs";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pref_keys_are_defined() {
        assert_eq!(pref_keys::EXPLAIN_MODE, "explain_mode");
        assert_eq!(pref_keys::NOTIFICATION_METHOD, "notification_method");
    }

    struct FakePreferenceSyncRow {
        failed_column: Option<&'static str>,
    }

    impl FakePreferenceSyncRow {
        fn complete() -> Self {
            Self {
                failed_column: None,
            }
        }

        fn fail_on(column: &'static str) -> Self {
            Self {
                failed_column: Some(column),
            }
        }
    }

    impl PreferenceSyncRow for FakePreferenceSyncRow {
        fn string_column(&self, column: &str) -> Result<String, sqlx::Error> {
            if self.failed_column == Some(column) {
                return Err(sqlx::Error::ColumnNotFound(column.to_string()));
            }

            Ok(match column {
                "pref_key" => "model",
                "pref_value" => "gpt-5",
                _ => return Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
            .to_string())
        }

        fn i32_column(&self, column: &str) -> Result<i32, sqlx::Error> {
            if self.failed_column == Some(column) {
                return Err(sqlx::Error::ColumnNotFound(column.to_string()));
            }

            match column {
                "version" => Ok(7),
                _ => Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
        }
    }

    fn assert_pref_decode_error(result: Result<impl std::fmt::Debug, String>, column: &str) {
        let error = result.unwrap_err();
        assert!(
            error.contains(column),
            "preference decode error should identify `{column}`: {error}"
        );
    }

    #[test]
    fn existing_preference_decode_preserves_value_and_version() {
        let (value, version) = decode_existing_preference_row(&FakePreferenceSyncRow::complete())
            .expect("complete preference row should decode");

        assert_eq!(value, "gpt-5");
        assert_eq!(version, 7);
    }

    #[test]
    fn existing_preference_decode_fails_loudly_on_bad_columns() {
        for column in ["pref_value", "version"] {
            assert_pref_decode_error(
                decode_existing_preference_row(&FakePreferenceSyncRow::fail_on(column)),
                column,
            );
        }
    }

    #[test]
    fn pull_all_preference_pair_decode_preserves_key_and_value() {
        let (key, value) = decode_preference_pair(&FakePreferenceSyncRow::complete())
            .expect("complete preference pair row should decode");

        assert_eq!(key, "model");
        assert_eq!(value, "gpt-5");
    }

    #[test]
    fn pull_all_preference_pair_decode_fails_loudly_on_bad_columns() {
        for column in ["pref_key", "pref_value"] {
            assert_pref_decode_error(
                decode_preference_pair(&FakePreferenceSyncRow::fail_on(column)),
                column,
            );
        }
    }

    #[test]
    fn next_preference_version_rejects_overflow() {
        assert_eq!(next_preference_version(0).unwrap(), 1);
        assert_eq!(next_preference_version(7).unwrap(), 8);

        let err = next_preference_version(i32::MAX).unwrap_err();
        assert!(
            err.contains("preference version overflow"),
            "overflow should be explicit: {err}"
        );
    }

    #[test]
    fn preference_sync_row_limit_is_bounded() {
        assert_eq!(MAX_PREFERENCE_SYNC_ROWS, 128);
    }
}
