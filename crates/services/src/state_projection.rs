use astra_core::SharedPool;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::Row;
use thiserror::Error;
use uuid::Uuid;

use crate::CancellationSafePoolConnection;
use crate::db_row::RowExt as StateProjectionDbRow;

const STATE_ITEM_ID_MAX_BYTES: usize = 128;

/// Builds a stable state-item identity without exceeding the storage contract.
/// Existing readable identities are preserved; only composite identities that
/// exceed the column limit are represented by their full SHA-256 digest.
pub fn bounded_state_item_id(kind: &str, components: &[&str]) -> String {
    let readable = std::iter::once("state")
        .chain(std::iter::once(kind))
        .chain(components.iter().copied())
        .collect::<Vec<_>>()
        .join("-");
    bounded_state_item_id_from_readable(readable, kind, components)
}

fn bounded_state_item_id_from_readable(
    readable: String,
    kind: &str,
    components: &[&str],
) -> String {
    if readable.len() <= STATE_ITEM_ID_MAX_BYTES {
        return readable;
    }

    let mut hasher = Sha256::new();
    for component in std::iter::once(kind).chain(components.iter().copied()) {
        hasher.update((component.len() as u64).to_be_bytes());
        hasher.update(component.as_bytes());
    }
    let digest = hasher.finalize();
    let categorized = format!("state-{kind}-{digest:x}");
    if categorized.len() <= STATE_ITEM_ID_MAX_BYTES {
        categorized
    } else {
        format!("state-{digest:x}")
    }
}

#[derive(Debug, Error)]
pub enum StateProjectionError {
    #[error("database operation failed: operation={operation}, entity={entity}, source={source}")]
    Database {
        operation: &'static str,
        entity: String,
        #[source]
        source: sqlx::Error,
    },
    #[error("json serialization failed: operation={operation}, entity={entity}, source={source}")]
    Json {
        operation: &'static str,
        entity: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("invalid database value: operation={operation}, column={column}, reason={reason}")]
    InvalidDatabaseValue {
        operation: &'static str,
        entity: String,
        column: &'static str,
        value: String,
        reason: &'static str,
    },
    #[error("session is not active: owner={user_id}, session={session_id}")]
    SessionNotActive { user_id: String, session_id: String },
    #[error(
        "personal skill version is unavailable: owner={user_id}, skill={skill_name}, version={version_id}"
    )]
    PersonalSkillVersionUnavailable {
        user_id: String,
        skill_name: String,
        version_id: String,
    },
    #[error("personal skill version is not activatable: version={version_id}, status={status}")]
    PersonalSkillVersionNotActivatable { version_id: String, status: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UserAnchorMemoryItem {
    pub item_id: String,
    pub category: String,
    pub item_key: String,
    pub summary_text: Option<String>,
    pub token_estimate: u32,
}

fn state_projection_row_string(
    row: &impl StateProjectionDbRow,
    operation: &'static str,
    entity: &str,
    column: &'static str,
) -> Result<String, StateProjectionError> {
    row.string_column(column)
        .map_err(|source| StateProjectionError::Database {
            operation,
            entity: entity.to_string(),
            source,
        })
}

fn state_projection_row_optional_string(
    row: &impl StateProjectionDbRow,
    operation: &'static str,
    entity: &str,
    column: &'static str,
) -> Result<Option<String>, StateProjectionError> {
    row.optional_string_column(column)
        .map_err(|source| StateProjectionError::Database {
            operation,
            entity: entity.to_string(),
            source,
        })
}

fn state_projection_row_i64(
    row: &impl StateProjectionDbRow,
    operation: &'static str,
    entity: &str,
    column: &'static str,
) -> Result<i64, StateProjectionError> {
    row.i64_column(column)
        .map_err(|source| StateProjectionError::Database {
            operation,
            entity: entity.to_string(),
            source,
        })
}

fn state_projection_row_u32(
    row: &impl StateProjectionDbRow,
    operation: &'static str,
    entity: &str,
    column: &'static str,
) -> Result<u32, StateProjectionError> {
    let value = state_projection_row_i64(row, operation, entity, column)?;
    u32::try_from(value).map_err(|_| StateProjectionError::InvalidDatabaseValue {
        operation,
        entity: entity.to_string(),
        column,
        value: value.to_string(),
        reason: "expected u32 range",
    })
}

fn decode_user_anchor_memory_item(
    row: &impl StateProjectionDbRow,
    user_id: &str,
) -> Result<UserAnchorMemoryItem, StateProjectionError> {
    const OPERATION: &str = "load_user_anchor_memory";
    Ok(UserAnchorMemoryItem {
        item_id: state_projection_row_string(row, OPERATION, user_id, "item_id")?,
        category: state_projection_row_string(row, OPERATION, user_id, "category")?,
        item_key: state_projection_row_string(row, OPERATION, user_id, "item_key")?,
        summary_text: state_projection_row_optional_string(
            row,
            OPERATION,
            user_id,
            "summary_text",
        )?,
        token_estimate: state_projection_row_u32(row, OPERATION, user_id, "token_estimate")?,
    })
}

#[derive(Clone, Debug)]
pub struct DatabaseStateProjectionStore {
    pool: SharedPool,
}

impl DatabaseStateProjectionStore {
    pub fn new(pool: SharedPool) -> Self {
        Self { pool }
    }

    pub async fn load_user_anchor_memory(
        &self,
        user_id: &str,
        token_budget: u32,
    ) -> Result<Vec<UserAnchorMemoryItem>, StateProjectionError> {
        let rows = sqlx::query(
            "SELECT item_id, category, item_key, summary_text, token_estimate
             FROM session_state_items FORCE INDEX (idx_state_user_scope_category)
             WHERE user_id = ? AND scope = 'user' AND status = 'active'
             ORDER BY priority DESC, updated_at DESC
             LIMIT 32",
        )
        .bind(user_id)
        .fetch_all(self.pool.get())
        .await
        .map_err(|source| StateProjectionError::Database {
            operation: "load_user_anchor_memory",
            entity: user_id.to_string(),
            source,
        })?;
        let mut used = 0_u32;
        let mut out = Vec::new();
        for row in rows {
            let item = decode_user_anchor_memory_item(&row, user_id)?;
            let estimate = item.token_estimate;
            if used.saturating_add(estimate) > token_budget {
                continue;
            }
            used = used.saturating_add(estimate);
            out.push(item);
        }
        Ok(out)
    }

    pub async fn activate_personal_skill_from_ui(
        &self,
        user_id: &str,
        session_id: &str,
        skill_name: &str,
        version_id: &str,
    ) -> Result<(), StateProjectionError> {
        let event_id = format!("event-{}", Uuid::new_v4());
        let item_id = bounded_state_item_id("active-skill", &[session_id, skill_name]);
        let payload = json!({
            "skill_name": skill_name,
            "version_id": version_id,
            "activation_source": "ui_structured_intent",
            "llm_involved": false,
        });
        let payload_json =
            serde_json::to_string(&payload).map_err(|source| StateProjectionError::Json {
                operation: "serialize_skill_activation",
                entity: skill_name.to_string(),
                source,
            })?;
        let payload_hash = content_hash(&payload_json);
        let mut connection = CancellationSafePoolConnection::acquire(self.pool.get())
            .await
            .map_err(|source| StateProjectionError::Database {
                operation: "acquire_skill_activation_connection",
                entity: session_id.to_string(),
                source,
            })?;
        let mut tx = connection
            .begin()
            .await
            .map_err(|source| StateProjectionError::Database {
                operation: "begin_skill_activation",
                entity: session_id.to_string(),
                source,
            })?;
        let session_admission = crate::storage::admit_session_event_write_with_facts(
            &mut tx, session_id, user_id, false,
        )
        .await
        .map_err(|source| match source {
            sqlx::Error::RowNotFound => StateProjectionError::SessionNotActive {
                user_id: user_id.to_string(),
                session_id: session_id.to_string(),
            },
            source => StateProjectionError::Database {
                operation: "validate_skill_activation_session",
                entity: session_id.to_string(),
                source,
            },
        })?;
        if session_admission.session_status() != "active" {
            return Err(StateProjectionError::SessionNotActive {
                user_id: user_id.to_string(),
                session_id: session_id.to_string(),
            });
        }
        let version_status = sqlx::query(
            "SELECT status FROM user_skill_versions
             WHERE owner_user_id = ? AND skill_name = ? AND version_id = ?
             LIMIT 1 FOR UPDATE",
        )
        .bind(user_id)
        .bind(skill_name)
        .bind(version_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|source| StateProjectionError::Database {
            operation: "validate_skill_activation_version",
            entity: version_id.to_string(),
            source,
        })?
        .map(|row| row.try_get::<String, _>("status"))
        .transpose()
        .map_err(|source| StateProjectionError::Database {
            operation: "validate_skill_activation_version",
            entity: version_id.to_string(),
            source,
        })?;
        let Some(version_status) = version_status else {
            return Err(StateProjectionError::PersonalSkillVersionUnavailable {
                user_id: user_id.to_string(),
                skill_name: skill_name.to_string(),
                version_id: version_id.to_string(),
            });
        };
        if version_status != "published" {
            return Err(StateProjectionError::PersonalSkillVersionNotActivatable {
                version_id: version_id.to_string(),
                status: version_status,
            });
        }
        let event_payload_hash = crate::observation_capture::canonical_observation_payload_hash(
            crate::observation_capture::ObservationPayloadDomain::AgentEvent,
            &serde_json::json!({
                "event_id": event_id, "session_id": session_id, "user_id": user_id,
                "event_type": "ui.skill.activate", "content": skill_name,
                "metadata": serde_json::from_str::<serde_json::Value>(&payload_json).map_err(|source| StateProjectionError::Database {
                    operation: "hash_skill_activation_event",
                    entity: session_id.to_string(),
                    source: sqlx::Error::Protocol(source.to_string()),
                })?,
            }),
        );
        let insert_result = sqlx::query(
            "INSERT INTO agent_events
             (event_id, session_id, user_id, event_type, content, metadata, payload_hash, ingestion_write_id, created_at)
             VALUES (?, ?, ?, 'ui.skill.activate', ?, ?, ?, ?, NOW(6))",
        )
        .bind(&event_id)
        .bind(session_id)
        .bind(user_id)
        .bind(skill_name)
        .bind(&payload_json)
        .bind(event_payload_hash)
        .bind(uuid::Uuid::new_v4().to_string())
        .execute(&mut *tx)
        .await
        .map_err(|source| StateProjectionError::Database {
            operation: "insert_skill_activation_event",
            entity: session_id.to_string(),
            source,
        })?;
        let inserted_events = crate::storage::rows_affected_to_i64(
            insert_result.rows_affected(),
            "ui.skill.activate",
        )
        .map_err(|source| StateProjectionError::Database {
            operation: "insert_skill_activation_event",
            entity: session_id.to_string(),
            source,
        })?;
        if inserted_events <= 0 {
            return Err(StateProjectionError::Database {
                operation: "insert_skill_activation_event",
                entity: session_id.to_string(),
                source: sqlx::Error::Protocol(
                    "skill activation event insert affected no rows".into(),
                ),
            });
        }
        crate::storage::bump_agent_session_event_count(
            &mut *tx,
            session_id,
            user_id,
            inserted_events,
            Some(&event_id),
        )
        .await
        .map_err(|source| StateProjectionError::Database {
            operation: "skill_activation_event_count_delta",
            entity: session_id.to_string(),
            source,
        })?;
        sqlx::query(
            "INSERT INTO session_state_items
             (item_id, user_id, session_id, scope, category, item_key, status, priority, source,
              provenance_event_id, title, summary_text, payload_json, payload_hash,
              token_estimate, version, created_at, updated_at)
             VALUES (?, ?, ?, 'session', 'active_skill', ?, 'active', 100, 'ui_structured_intent',
                     ?, ?, ?, ?, ?, 80, 1, NOW(6), NOW(6))
             ON DUPLICATE KEY UPDATE
              provenance_event_id = VALUES(provenance_event_id), payload_json = VALUES(payload_json),
              payload_hash = VALUES(payload_hash), version = version + 1, updated_at = NOW(6)",
        )
        .bind(&item_id)
        .bind(user_id)
        .bind(session_id)
        .bind(skill_name)
        .bind(&event_id)
        .bind(skill_name)
        .bind(format!("Active personal skill {skill_name}@{version_id}"))
        .bind(&payload_json)
        .bind(&payload_hash)
        .execute(&mut *tx)
        .await
        .map_err(|source| StateProjectionError::Database {
            operation: "upsert_active_skill_state",
            entity: skill_name.to_string(),
            source,
        })?;
        sqlx::query(
            "INSERT INTO session_state_item_events
             (event_id, item_id, user_id, session_id, category, item_key, mutation, next_hash,
              payload_json, provenance_event_id, created_at)
             VALUES (?, ?, ?, ?, 'active_skill', ?, 'activate', ?, ?, ?, NOW(6))",
        )
        .bind(new_state_item_event_id())
        .bind(&item_id)
        .bind(user_id)
        .bind(session_id)
        .bind(skill_name)
        .bind(&payload_hash)
        .bind(&payload_json)
        .bind(&event_id)
        .execute(&mut *tx)
        .await
        .map_err(|source| StateProjectionError::Database {
            operation: "insert_skill_activation_state_event",
            entity: skill_name.to_string(),
            source,
        })?;
        tx.commit()
            .await
            .map_err(|source| StateProjectionError::Database {
                operation: "commit_skill_activation",
                entity: session_id.to_string(),
                source,
            })?;
        connection.release();
        Ok(())
    }
}

fn content_hash(content: &str) -> String {
    let digest = Sha256::digest(content.as_bytes());
    format!("sha256:{digest:x}")
}

fn new_state_item_event_id() -> String {
    Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_item_id_preserves_readable_identity_when_it_fits() {
        assert_eq!(
            bounded_state_item_id("summary", &["session-1", "run-1"]),
            "state-summary-session-1-run-1"
        );
    }

    #[test]
    fn state_item_id_hashes_overlong_composites_stably() {
        let session_id = "s".repeat(64);
        let run_id = "r".repeat(68);
        let first = bounded_state_item_id("decision", &[&session_id, &run_id, "2"]);
        let repeated = bounded_state_item_id("decision", &[&session_id, &run_id, "2"]);
        let next_turn = bounded_state_item_id("decision", &[&session_id, &run_id, "3"]);

        assert!(first.len() <= STATE_ITEM_ID_MAX_BYTES);
        assert!(first.starts_with("state-decision-"));
        assert_eq!(first, repeated);
        assert_ne!(first, next_turn);
    }

    #[derive(Clone)]
    struct FakeStateProjectionRow {
        failed_column: Option<&'static str>,
        i64_overrides: Vec<(&'static str, i64)>,
    }

    impl FakeStateProjectionRow {
        fn complete() -> Self {
            Self {
                failed_column: None,
                i64_overrides: Vec::new(),
            }
        }

        fn fail_on(column: &'static str) -> Self {
            Self {
                failed_column: Some(column),
                ..Self::complete()
            }
        }

        fn with_i64(column: &'static str, value: i64) -> Self {
            Self {
                i64_overrides: vec![(column, value)],
                ..Self::complete()
            }
        }

        fn fail_if_needed(&self, column: &str) -> Result<(), sqlx::Error> {
            if self.failed_column == Some(column) {
                Err(sqlx::Error::ColumnNotFound(column.to_string()))
            } else {
                Ok(())
            }
        }
    }

    impl StateProjectionDbRow for FakeStateProjectionRow {
        fn string_column(&self, column: &str) -> Result<String, sqlx::Error> {
            self.fail_if_needed(column)?;
            Ok(match column {
                "item_id" => "item-1",
                "category" => "decision",
                "item_key" => "key-1",
                _ => return Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
            .to_string())
        }

        fn optional_string_column(&self, column: &str) -> Result<Option<String>, sqlx::Error> {
            self.fail_if_needed(column)?;
            Ok(match column {
                "summary_text" => Some("summary".to_string()),
                _ => return Err(sqlx::Error::ColumnNotFound(column.to_string())),
            })
        }

        fn i64_column(&self, column: &str) -> Result<i64, sqlx::Error> {
            self.fail_if_needed(column)?;
            if let Some((_, value)) = self
                .i64_overrides
                .iter()
                .find(|(candidate, _)| *candidate == column)
            {
                return Ok(*value);
            }
            Ok(match column {
                "token_estimate" => 42,
                _ => return Err(sqlx::Error::ColumnNotFound(column.to_string())),
            })
        }
    }

    fn assert_database_error_mentions(
        result: Result<impl std::fmt::Debug, StateProjectionError>,
        needle: &str,
    ) {
        let err = result.expect_err("decode should fail");
        match err {
            StateProjectionError::Database { source, .. } => {
                assert!(
                    source.to_string().contains(needle),
                    "source error should contain `{needle}`, got `{source}`"
                );
            }
            other => panic!("expected database decode error, got {other:?}"),
        }
    }

    fn assert_invalid_database_value(
        result: Result<impl std::fmt::Debug, StateProjectionError>,
        column: &'static str,
    ) {
        let err = result.expect_err("decode should fail");
        assert!(
            matches!(err, StateProjectionError::InvalidDatabaseValue { column: actual, .. } if actual == column),
            "expected invalid database value for {column}, got {err:?}"
        );
    }

    #[test]
    fn invalid_database_value_display_omits_entity_and_value() {
        let err = StateProjectionError::InvalidDatabaseValue {
            operation: "decode_projection",
            entity: "user-sensitive/session-sensitive".to_string(),
            column: "token_estimate",
            value: "secret-value".to_string(),
            reason: "expected u32 range",
        };
        let display = err.to_string();
        assert!(display.contains("decode_projection"));
        assert!(display.contains("token_estimate"));
        assert!(!display.contains("user-sensitive"));
        assert!(!display.contains("session-sensitive"));
        assert!(!display.contains("secret-value"));
    }

    #[test]
    fn user_anchor_memory_decode_preserves_values_and_fails_loudly() {
        let item = decode_user_anchor_memory_item(&FakeStateProjectionRow::complete(), "user-1")
            .expect("anchor memory decodes");
        assert_eq!(item.item_id, "item-1");
        assert_eq!(item.category, "decision");
        assert_eq!(item.item_key, "key-1");
        assert_eq!(item.summary_text.as_deref(), Some("summary"));
        assert_eq!(item.token_estimate, 42);

        for column in [
            "item_id",
            "category",
            "item_key",
            "summary_text",
            "token_estimate",
        ] {
            assert_database_error_mentions(
                decode_user_anchor_memory_item(&FakeStateProjectionRow::fail_on(column), "user-1"),
                column,
            );
        }
        assert_invalid_database_value(
            decode_user_anchor_memory_item(
                &FakeStateProjectionRow::with_i64("token_estimate", -1),
                "user-1",
            ),
            "token_estimate",
        );
        assert_invalid_database_value(
            decode_user_anchor_memory_item(
                &FakeStateProjectionRow::with_i64("token_estimate", i64::from(u32::MAX) + 1),
                "user-1",
            ),
            "token_estimate",
        );
    }
}
