use astra_core::{SharedPool, matrixone_statement_with_null_shape};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::Row;
use thiserror::Error;
use uuid::Uuid;

use crate::context_manifest::{
    BudgetV1_8k, ContextManifestItemWrite, ContextManifestWrite, DatabaseContextManifestStore,
    artifact_id_from_raw_ref,
};
use crate::db_row::RowExt as StateProjectionDbRow;

pub const PROTECTED_COMPACTION_CATEGORIES: &[&str] = &[
    "plan_state",
    "decision",
    "finding",
    "benchmark",
    "citation",
    "todo_state",
    "error_state",
    "delegation_state",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompactionInvariant {
    pub id: &'static str,
    pub description: &'static str,
    pub sql: &'static str,
    pub binds_compaction_run_id: bool,
}

pub const COMPACTION_INVARIANT_SQL: &[CompactionInvariant] = &[
    CompactionInvariant {
        id: "no_archived_active_durable_facts",
        description: "Active durable facts must survive compaction.",
        sql: "SELECT COUNT(*) AS violations FROM session_state_items \
	              WHERE user_id = ? AND session_id = ? \
	                AND category IN ('plan_state', 'decision', 'finding', 'benchmark', 'citation') \
	                AND status NOT IN ('active', 'backlog')",
        binds_compaction_run_id: false,
    },
    CompactionInvariant {
        id: "no_archived_active_operational_state",
        description: "Active todo, error, and delegation state must survive compaction.",
        sql: "SELECT COUNT(*) AS violations FROM session_state_items \
	              WHERE user_id = ? AND session_id = ? \
	                AND category IN ('todo_state', 'error_state', 'delegation_state') \
	                AND status NOT IN ('active', 'backlog')",
        binds_compaction_run_id: false,
    },
    CompactionInvariant {
        id: "plan_state_not_replaced",
        description: "Compaction must not replace/archive/delete plan_state.",
        sql: "SELECT COUNT(*) AS violations FROM session_state_item_events \
	              WHERE user_id = ? AND session_id = ? \
	                AND category = 'plan_state' \
	                AND mutation IN ('replace', 'archive', 'delete')",
        binds_compaction_run_id: false,
    },
    CompactionInvariant {
        id: "no_active_run_compaction",
        description: "Session-level compaction must not run while a run is active.",
        sql: "SELECT COUNT(*) AS violations FROM agent_runs \
	              WHERE user_id = ? AND session_id = ? AND status IN ('running', 'waiting')",
        binds_compaction_run_id: false,
    },
    CompactionInvariant {
        id: "exactly_one_post_compaction_manifest",
        description: "Each compaction writes exactly one post_compaction manifest.",
        sql: "SELECT ABS(COUNT(*) - 1) AS violations FROM context_manifests \
	              WHERE user_id = ? AND session_id = ? AND run_id = ? AND reason = 'post_compaction'",
        binds_compaction_run_id: true,
    },
    CompactionInvariant {
        id: "plan_todo_zone_cap",
        description: "Post-compaction plan_todo context must stay within 800 tokens.",
        sql: "SELECT COUNT(*) AS violations \
	              FROM context_manifest_items i \
	              JOIN context_manifests m ON m.manifest_id = i.manifest_id \
	              WHERE m.user_id = ? AND m.session_id = ? AND m.run_id = ? AND m.reason = 'post_compaction' \
	                AND i.zone = 'plan_todo' AND i.token_estimate > 800",
        binds_compaction_run_id: true,
    },
    CompactionInvariant {
        id: "user_scope_not_compacted",
        description: "User-scope state must not be archived by session compaction.",
        sql: "SELECT COUNT(*) AS violations FROM session_state_items \
	              WHERE user_id = ? AND session_id = ? AND scope = 'user' AND status NOT IN ('active', 'backlog')",
        binds_compaction_run_id: false,
    },
    CompactionInvariant {
        id: "no_delete_mutations_for_protected_state",
        description: "Compaction must not write delete mutations for protected projection state.",
        sql: "SELECT COUNT(*) AS violations FROM session_state_item_events \
	              WHERE user_id = ? AND session_id = ? AND mutation = 'delete' \
	                AND category IN ('plan_state', 'decision', 'finding', 'benchmark', 'citation', \
	                                 'todo_state', 'error_state', 'delegation_state')",
        binds_compaction_run_id: false,
    },
];

fn compaction_invariant_batch_sql() -> String {
    COMPACTION_INVARIANT_SQL
        .iter()
        .enumerate()
        .map(|(idx, invariant)| {
            format!(
                "SELECT '{}' AS invariant_id, violations FROM ({}) AS invariant_{}",
                invariant.id, invariant.sql, idx
            )
        })
        .collect::<Vec<_>>()
        .join(" UNION ALL ")
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
    #[error("invalid mutation {mutation}")]
    InvalidMutation { mutation: String },
    #[error("invalid retry_scope for run {run_id}: {retry_scope}")]
    InvalidRetryScope { run_id: String, retry_scope: String },
    #[error("invalid database value: operation={operation}, column={column}, reason={reason}")]
    InvalidDatabaseValue {
        operation: &'static str,
        entity: String,
        column: &'static str,
        value: String,
        reason: &'static str,
    },
    #[error("session {session_id} has active run count {active_count}; compaction rejected")]
    ActiveRunCompaction {
        session_id: String,
        active_count: i64,
    },
    #[error("compaction invariant failed: {id} violations={violations}")]
    CompactionInvariantFailed { id: String, violations: i64 },
}

#[derive(Clone, Debug)]
pub struct DelegationProjectionUpsert {
    pub delegation_id: String,
    pub user_id: String,
    pub session_id: String,
    pub parent_run_id: String,
    pub child_run_id: String,
    pub root_run_id: String,
    pub ancestor_path: String,
    pub depth: u32,
    pub agent_id: Option<String>,
    pub title: Option<String>,
    pub status: String,
    pub retry_of: Option<String>,
    pub retry_scope: String,
    pub last_summary_ref: Option<String>,
    pub last_summary_text: Option<String>,
    pub sibling_exposed_artifacts_json: Option<String>,
}

impl DelegationProjectionUpsert {
    fn nullable_shape(&self) -> [bool; 6] {
        [
            self.agent_id.is_some(),
            self.title.is_some(),
            self.retry_of.is_some(),
            self.last_summary_ref.is_some(),
            self.last_summary_text.is_some(),
            self.sibling_exposed_artifacts_json.is_some(),
        ]
    }
}

#[derive(Clone, Debug)]
pub struct StateItemUpsert {
    pub item_id: Option<String>,
    pub user_id: String,
    pub session_id: String,
    pub scope: String,
    pub category: String,
    pub item_key: String,
    pub status: String,
    pub priority: i32,
    pub source: String,
    pub provenance_event_id: Option<String>,
    pub run_id: Option<String>,
    pub title: Option<String>,
    pub summary_text: Option<String>,
    pub payload_json: serde_json::Value,
    pub token_estimate: u32,
    pub mutation: String,
}

#[derive(Clone, Debug)]
pub struct BubbleUpTarget {
    pub session_id: String,
    pub run_id: String,
    pub depth: u32,
}

pub trait SkillActivationLlmProbe: Send + Sync {
    fn record_llm_call(&self);
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

fn state_projection_row_non_negative_i64(
    row: &impl StateProjectionDbRow,
    operation: &'static str,
    entity: &str,
    column: &'static str,
) -> Result<i64, StateProjectionError> {
    let value = state_projection_row_i64(row, operation, entity, column)?;
    if value < 0 {
        return Err(StateProjectionError::InvalidDatabaseValue {
            operation,
            entity: entity.to_string(),
            column,
            value: value.to_string(),
            reason: "expected non-negative integer",
        });
    }
    Ok(value)
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
struct ArtifactAclRow {
    user_id: String,
    access_scope: String,
    owner_run_id: Option<String>,
    root_run_id: Option<String>,
    status: String,
}

fn decode_artifact_acl_row(
    row: &impl StateProjectionDbRow,
    artifact_id: &str,
) -> Result<ArtifactAclRow, StateProjectionError> {
    const OPERATION: &str = "load_artifact_acl";
    Ok(ArtifactAclRow {
        user_id: state_projection_row_string(row, OPERATION, artifact_id, "user_id")?,
        access_scope: state_projection_row_string(row, OPERATION, artifact_id, "access_scope")?,
        owner_run_id: state_projection_row_optional_string(
            row,
            OPERATION,
            artifact_id,
            "owner_run_id",
        )?,
        root_run_id: state_projection_row_optional_string(
            row,
            OPERATION,
            artifact_id,
            "root_run_id",
        )?,
        status: state_projection_row_string(row, OPERATION, artifact_id, "status")?,
    })
}

fn decode_run_acl_row(
    row: &impl StateProjectionDbRow,
    run_id: &str,
) -> Result<RunAclRow, StateProjectionError> {
    const OPERATION: &str = "load_run_acl_for_user";
    Ok(RunAclRow {
        user_id: state_projection_row_string(row, OPERATION, run_id, "user_id")?,
        session_id: state_projection_row_string(row, OPERATION, run_id, "session_id")?,
        root_run_id: state_projection_row_optional_string(row, OPERATION, run_id, "root_run_id")?,
        ancestor_path: state_projection_row_optional_string(
            row,
            OPERATION,
            run_id,
            "ancestor_path",
        )?,
        depth: state_projection_row_u32(row, OPERATION, run_id, "depth")?,
    })
}

fn decode_run_projection_row(
    row: &impl StateProjectionDbRow,
    run_id: &str,
) -> Result<RunProjectionRow, StateProjectionError> {
    const OPERATION: &str = "load_run_projection_for_user";
    Ok(RunProjectionRow {
        run_id: state_projection_row_string(row, OPERATION, run_id, "run_id")?,
        user_id: state_projection_row_string(row, OPERATION, run_id, "user_id")?,
        session_id: state_projection_row_string(row, OPERATION, run_id, "session_id")?,
        status: state_projection_row_string(row, OPERATION, run_id, "status")?,
        parent_run_id: state_projection_row_optional_string(
            row,
            OPERATION,
            run_id,
            "parent_run_id",
        )?,
        root_run_id: state_projection_row_optional_string(row, OPERATION, run_id, "root_run_id")?,
        ancestor_path: state_projection_row_optional_string(
            row,
            OPERATION,
            run_id,
            "ancestor_path",
        )?,
        depth: state_projection_row_u32(row, OPERATION, run_id, "depth")?,
        delegation_id: state_projection_row_optional_string(
            row,
            OPERATION,
            run_id,
            "delegation_id",
        )?,
        agent_id: state_projection_row_optional_string(row, OPERATION, run_id, "agent_id")?,
        retry_of: state_projection_row_optional_string(row, OPERATION, run_id, "retry_of")?,
        retry_scope: state_projection_row_optional_string(row, OPERATION, run_id, "retry_scope")?,
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

    /// Check whether the session can be compacted (no active runs).
    pub async fn can_compact_session(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> Result<(), StateProjectionError> {
        let row = sqlx::query(
            "SELECT COUNT(*) AS active_count FROM agent_runs \
             WHERE user_id = ? AND session_id = ? AND status IN ('running', 'waiting')",
        )
        .bind(user_id)
        .bind(session_id)
        .fetch_one(self.pool.get())
        .await
        .map_err(|source| StateProjectionError::Database {
            operation: "can_compact_session",
            entity: session_id.to_string(),
            source,
        })?;
        let active_count = state_projection_row_non_negative_i64(
            &row,
            "can_compact_session",
            session_id,
            "active_count",
        )?;
        if active_count == 0 {
            Ok(())
        } else {
            Err(StateProjectionError::ActiveRunCompaction {
                session_id: session_id.to_string(),
                active_count,
            })
        }
    }

    pub async fn run_compaction_assertions(
        &self,
        user_id: &str,
        session_id: &str,
        compaction_run_id: &str,
    ) -> Result<Vec<(String, i64)>, StateProjectionError> {
        let sql = compaction_invariant_batch_sql();
        let mut query = sqlx::query(&sql);
        for invariant in COMPACTION_INVARIANT_SQL {
            query = query.bind(user_id).bind(session_id);
            if invariant.binds_compaction_run_id {
                query = query.bind(compaction_run_id);
            }
        }

        let rows = query.fetch_all(self.pool.get()).await.map_err(|source| {
            StateProjectionError::Database {
                operation: "run_compaction_invariant",
                entity: "compaction_invariants".to_string(),
                source,
            }
        })?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let invariant_id = row.try_get::<String, _>("invariant_id").unwrap_or_default();
            let violations = state_projection_row_non_negative_i64(
                &row,
                "run_compaction_invariant",
                &invariant_id,
                "violations",
            )?;
            out.push((invariant_id, violations));
        }
        Ok(out)
    }

    pub async fn compact_session_state(
        &self,
        user_id: &str,
        session_id: &str,
        compaction_run_id: &str,
        plan_todo_tokens: u32,
    ) -> Result<Vec<(String, i64)>, StateProjectionError> {
        self.can_compact_session(user_id, session_id).await?;
        let budget = BudgetV1_8k::standard();
        let manifest_id = format!("manifest-{}", Uuid::new_v4());
        let manifest = ContextManifestWrite {
            manifest_id: manifest_id.clone(),
            user_id: user_id.to_string(),
            session_id: session_id.to_string(),
            run_id: Some(compaction_run_id.to_string()),
            turn_id: format!("{compaction_run_id}:compaction"),
            model_provider: "runtime".to_string(),
            model_name: "compaction_engine".to_string(),
            context_window_tokens: 8_000,
            max_output_tokens: budget.reserved_output,
            total_estimated_tokens: plan_todo_tokens.min(800),
            policy_version: "context_manifest_v1".to_string(),
            tokenizer_id: Some("estimated_v1".to_string()),
            budget_template_id: Some("budget_v1_8k".to_string()),
            turn_intent: Some("compaction".to_string()),
            reason: "post_compaction".to_string(),
            manifest_json: json!({
                "source": "phase4_compaction_engine",
                "invariants": COMPACTION_INVARIANT_SQL.iter().map(|i| i.id).collect::<Vec<_>>(),
                "zones": {
                    "plan_todo": {"used_tokens": plan_todo_tokens.min(800), "budget_tokens": 800}
                }
            }),
        };
        let items = vec![ContextManifestItemWrite {
            session_id: session_id.to_string(),
            item_order: 0,
            zone: "plan_todo".to_string(),
            source_table: "session_state_items".to_string(),
            source_id: format!("{session_id}:plan_todo"),
            source_hash: None,
            included: true,
            token_estimate: plan_todo_tokens.min(800),
            budget_tokens: 800,
            reason: "post_compaction".to_string(),
            render_mode: "summary".to_string(),
            raw_ref: Some(format!(
                "conversation_log://{session_id}/compaction@{manifest_id}"
            )),
        }];
        DatabaseContextManifestStore::new(self.pool.clone())
            .save_manifest(manifest, items)
            .await
            .map_err(|source| StateProjectionError::Database {
                operation: "save_post_compaction_manifest",
                entity: session_id.to_string(),
                source: match source {
                    crate::context_manifest::ContextManifestError::Database { source, .. } => {
                        source
                    }
                    other => sqlx::Error::Protocol(other.to_string()),
                },
            })?;
        self.upsert_state_item(StateItemUpsert {
            item_id: Some(format!("state-summary-{session_id}-{compaction_run_id}")),
            user_id: user_id.to_string(),
            session_id: session_id.to_string(),
            scope: "session".to_string(),
            category: "summary".to_string(),
            item_key: format!("compaction:{compaction_run_id}"),
            status: "active".to_string(),
            priority: 40,
            source: "compaction_engine".to_string(),
            provenance_event_id: None,
            run_id: Some(compaction_run_id.to_string()),
            title: Some("Post-compaction summary".to_string()),
            summary_text: Some(format!(
                "Post-compaction plan/todo skeleton retained within {} tokens",
                plan_todo_tokens.min(800)
            )),
            payload_json: json!({
                "reason": "post_compaction",
                "plan_todo_tokens": plan_todo_tokens.min(800),
                "source_manifest_run_id": compaction_run_id,
            }),
            token_estimate: 120,
            mutation: "insert".to_string(),
        })
        .await?;

        // A local-only advisory lock would not serialize against run starts
        // unless every run-start path acquired the same lock. Re-check the DB
        // invariant instead of silently accepting stale compaction output.
        self.can_compact_session(user_id, session_id).await?;

        let results = self
            .run_compaction_assertions(user_id, session_id, compaction_run_id)
            .await?;
        for (id, violations) in &results {
            if *violations != 0 {
                return Err(StateProjectionError::CompactionInvariantFailed {
                    id: id.clone(),
                    violations: *violations,
                });
            }
        }
        Ok(results)
    }

    pub async fn upsert_state_item(
        &self,
        item: StateItemUpsert,
    ) -> Result<String, StateProjectionError> {
        validate_state_mutation(&item.mutation)?;
        let item_id = item
            .item_id
            .clone()
            .unwrap_or_else(|| format!("state-{}-{}", item.category, Uuid::new_v4()));
        let payload_json = serde_json::to_string(&item.payload_json).map_err(|source| {
            StateProjectionError::Json {
                operation: "serialize_state_item",
                entity: item_id.clone(),
                source,
            }
        })?;
        let payload_hash = content_hash(&payload_json);
        let mut tx =
            self.pool
                .get()
                .begin()
                .await
                .map_err(|source| StateProjectionError::Database {
                    operation: "begin_state_item_upsert",
                    entity: item_id.clone(),
                    source,
                })?;
        sqlx::query(
            "INSERT INTO session_state_items
             (item_id, user_id, session_id, scope, category, item_key, status, priority, source,
              provenance_event_id, run_id, title, summary_text, payload_json, payload_hash,
              token_estimate, version, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1, NOW(6), NOW(6))
             ON DUPLICATE KEY UPDATE
              status = VALUES(status), priority = VALUES(priority), source = VALUES(source),
              provenance_event_id = VALUES(provenance_event_id), run_id = VALUES(run_id),
              title = VALUES(title), summary_text = VALUES(summary_text),
              payload_json = VALUES(payload_json), payload_hash = VALUES(payload_hash),
              token_estimate = VALUES(token_estimate), version = version + 1, updated_at = NOW(6)",
        )
        .bind(&item_id)
        .bind(&item.user_id)
        .bind(&item.session_id)
        .bind(&item.scope)
        .bind(&item.category)
        .bind(&item.item_key)
        .bind(&item.status)
        .bind(i64::from(item.priority))
        .bind(&item.source)
        .bind(&item.provenance_event_id)
        .bind(&item.run_id)
        .bind(&item.title)
        .bind(&item.summary_text)
        .bind(&payload_json)
        .bind(&payload_hash)
        .bind(i64::from(item.token_estimate))
        .execute(&mut *tx)
        .await
        .map_err(|source| StateProjectionError::Database {
            operation: "upsert_state_item",
            entity: item_id.clone(),
            source,
        })?;
        sqlx::query(
            "INSERT INTO session_state_item_events
             (event_id, item_id, user_id, session_id, category, item_key, mutation, next_hash,
              payload_json, provenance_event_id, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW(6))",
        )
        .bind(new_state_item_event_id())
        .bind(&item_id)
        .bind(&item.user_id)
        .bind(&item.session_id)
        .bind(&item.category)
        .bind(&item.item_key)
        .bind(&item.mutation)
        .bind(&payload_hash)
        .bind(&payload_json)
        .bind(&item.provenance_event_id)
        .execute(&mut *tx)
        .await
        .map_err(|source| StateProjectionError::Database {
            operation: "insert_state_item_event",
            entity: item_id.clone(),
            source,
        })?;
        for artifact_id in artifact_ids_from_state_payload(&item.payload_json) {
            sqlx::query(
                "UPDATE session_artifacts
                 SET referenced_by_state_items_count = referenced_by_state_items_count + 1,
                     updated_at = NOW(6)
                 WHERE user_id = ? AND session_id = ? AND artifact_id = ?",
            )
            .bind(&item.user_id)
            .bind(&item.session_id)
            .bind(&artifact_id)
            .execute(&mut *tx)
            .await
            .map_err(|source| StateProjectionError::Database {
                operation: "increment_state_item_artifact_ref",
                entity: artifact_id,
                source,
            })?;
        }
        tx.commit()
            .await
            .map_err(|source| StateProjectionError::Database {
                operation: "commit_state_item_upsert",
                entity: item_id.clone(),
                source,
            })?;
        Ok(item_id)
    }

    pub async fn upsert_delegation_projection_for_run(
        &self,
        user_id: &str,
        child_run_id: &str,
        agent_id_hint: Option<&str>,
        last_summary_text: Option<&str>,
    ) -> Result<(), StateProjectionError> {
        let Some(child) = self
            .load_run_projection_for_user(user_id, child_run_id)
            .await?
        else {
            return Ok(());
        };
        let Some(parent_run_id) = child.parent_run_id.clone() else {
            return Ok(());
        };
        let Some(delegation_id) = child.delegation_id.clone() else {
            return Ok(());
        };
        let parent = self
            .load_run_projection_for_user(user_id, &parent_run_id)
            .await?;
        let (root_run_id, ancestor_path, depth) = if let Some(parent) = parent {
            let parent_root = parent.root_run_id.unwrap_or(parent.run_id.clone());
            let parent_path = parent.ancestor_path.unwrap_or(parent.run_id);
            (
                parent_root,
                format!("{parent_path}/{child_run_id}"),
                parent.depth.saturating_add(1),
            )
        } else {
            (
                child.root_run_id.unwrap_or_else(|| parent_run_id.clone()),
                child
                    .ancestor_path
                    .unwrap_or_else(|| format!("{parent_run_id}/{child_run_id}")),
                child.depth.max(1),
            )
        };
        let tree_update = sqlx::query(
            "UPDATE agent_runs
             SET root_run_id = ?, ancestor_path = ?, depth = ?, updated_at = NOW(6)
             WHERE user_id = ? AND run_id = ?",
        )
        .bind(&root_run_id)
        .bind(&ancestor_path)
        .bind(i64::from(depth))
        .bind(user_id)
        .bind(child_run_id)
        .execute(self.pool.get())
        .await
        .map_err(|source| StateProjectionError::Database {
            operation: "sync_delegation_run_tree",
            entity: child_run_id.to_string(),
            source,
        })?;
        if tree_update.rows_affected() == 0 {
            return Err(StateProjectionError::Database {
                operation: "sync_delegation_run_tree",
                entity: child_run_id.to_string(),
                source: sqlx::Error::RowNotFound,
            });
        }
        self.upsert_delegation_projection(DelegationProjectionUpsert {
            delegation_id,
            user_id: child.user_id,
            session_id: child.session_id,
            parent_run_id,
            child_run_id: child_run_id.to_string(),
            root_run_id,
            ancestor_path,
            depth,
            agent_id: child
                .agent_id
                .or_else(|| agent_id_hint.map(ToString::to_string)),
            title: agent_id_hint.map(|agent_id| format!("Delegated run {agent_id}")),
            status: child.status,
            retry_of: child.retry_of,
            retry_scope: child.retry_scope.unwrap_or_else(|| "node".to_string()),
            last_summary_ref: None,
            last_summary_text: last_summary_text.map(ToString::to_string),
            sibling_exposed_artifacts_json: None,
        })
        .await
    }

    pub async fn upsert_delegation_projection(
        &self,
        record: DelegationProjectionUpsert,
    ) -> Result<(), StateProjectionError> {
        validate_retry_scope(&record.child_run_id, &record.retry_scope)?;
        let payload = json!({
            "delegation_id": record.delegation_id,
            "parent_run_id": record.parent_run_id,
            "child_run_id": record.child_run_id,
            "root_run_id": record.root_run_id,
            "ancestor_path": record.ancestor_path,
            "depth": record.depth,
            "status": record.status,
            "last_summary_ref": record.last_summary_ref,
            "last_summary_text": record.last_summary_text,
        });
        let payload_json =
            serde_json::to_string(&payload).map_err(|source| StateProjectionError::Json {
                operation: "serialize_delegation_state",
                entity: record.delegation_id.clone(),
                source,
            })?;
        let payload_hash = content_hash(&payload_json);
        let item_id = format!("state-delegation-{}", record.delegation_id);
        let mut tx =
            self.pool
                .get()
                .begin()
                .await
                .map_err(|source| StateProjectionError::Database {
                    operation: "begin_delegation_projection",
                    entity: record.delegation_id.clone(),
                    source,
                })?;

        let delegation_insert_sql = matrixone_statement_with_null_shape(
            "INSERT INTO session_delegations
             (delegation_id, user_id, session_id, parent_run_id, child_run_id, root_run_id,
              ancestor_path, depth, agent_id, title, status, retry_of, retry_scope,
              last_summary_ref, last_summary_text, sibling_exposed_artifacts_json, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW(6), NOW(6))
             ON DUPLICATE KEY UPDATE
              status = VALUES(status), last_summary_ref = VALUES(last_summary_ref),
              last_summary_text = VALUES(last_summary_text),
              sibling_exposed_artifacts_json = VALUES(sibling_exposed_artifacts_json),
              updated_at = NOW(6)",
            record.nullable_shape(),
        );
        sqlx::query(&delegation_insert_sql)
            .bind(&record.delegation_id)
            .bind(&record.user_id)
            .bind(&record.session_id)
            .bind(&record.parent_run_id)
            .bind(&record.child_run_id)
            .bind(&record.root_run_id)
            .bind(&record.ancestor_path)
            .bind(i64::from(record.depth))
            .bind(&record.agent_id)
            .bind(&record.title)
            .bind(&record.status)
            .bind(&record.retry_of)
            .bind(&record.retry_scope)
            .bind(&record.last_summary_ref)
            .bind(&record.last_summary_text)
            .bind(&record.sibling_exposed_artifacts_json)
            .execute(&mut *tx)
            .await
            .map_err(|source| StateProjectionError::Database {
                operation: "upsert_session_delegation",
                entity: record.delegation_id.clone(),
                source,
            })?;

        sqlx::query(
            "INSERT INTO session_state_items
             (item_id, user_id, session_id, scope, category, item_key, status, priority, source,
              run_id, title, summary_text, payload_json, payload_hash, token_estimate, version,
              created_at, updated_at)
             VALUES (?, ?, ?, 'session', 'delegation_state', ?, ?, ?, 'delegation_engine',
                     ?, ?, ?, ?, ?, 120, 1, NOW(6), NOW(6))
             ON DUPLICATE KEY UPDATE
              status = VALUES(status), summary_text = VALUES(summary_text),
              payload_json = VALUES(payload_json), payload_hash = VALUES(payload_hash),
              version = version + 1, updated_at = NOW(6)",
        )
        .bind(&item_id)
        .bind(&record.user_id)
        .bind(&record.session_id)
        .bind(&record.delegation_id)
        .bind(&record.status)
        .bind(i64::from(record.depth))
        .bind(&record.child_run_id)
        .bind(&record.title)
        .bind(&record.last_summary_text)
        .bind(&payload_json)
        .bind(&payload_hash)
        .execute(&mut *tx)
        .await
        .map_err(|source| StateProjectionError::Database {
            operation: "upsert_delegation_state_item",
            entity: record.delegation_id.clone(),
            source,
        })?;

        sqlx::query(
            "INSERT INTO session_state_item_events
             (event_id, item_id, user_id, session_id, category, item_key, mutation, next_hash,
              payload_json, created_at)
             VALUES (?, ?, ?, ?, 'delegation_state', ?, 'insert', ?, ?, NOW(6))",
        )
        .bind(new_state_item_event_id())
        .bind(&item_id)
        .bind(&record.user_id)
        .bind(&record.session_id)
        .bind(&record.delegation_id)
        .bind(&payload_hash)
        .bind(&payload_json)
        .execute(&mut *tx)
        .await
        .map_err(|source| StateProjectionError::Database {
            operation: "insert_delegation_state_event",
            entity: record.delegation_id,
            source,
        })?;

        tx.commit()
            .await
            .map_err(|source| StateProjectionError::Database {
                operation: "commit_delegation_projection",
                entity: item_id,
                source,
            })
    }

    pub async fn bubble_up_finding(
        &self,
        user_id: &str,
        source_run_id: &str,
        original_item_id: &str,
        severity: &str,
        summary: &str,
        targets: &[BubbleUpTarget],
    ) -> Result<(), StateProjectionError> {
        let mut tx =
            self.pool
                .get()
                .begin()
                .await
                .map_err(|source| StateProjectionError::Database {
                    operation: "begin_bubble_up",
                    entity: source_run_id.to_string(),
                    source,
                })?;
        for (idx, target) in targets.iter().enumerate() {
            let item_key = format!("bubble:{source_run_id}:{}", target.depth);
            let item_id = format!("state-{item_key}");
            let payload = json!({
                "bubble_seq": idx + 1,
                "severity": severity,
                "source_run_id": source_run_id,
                "original_item_id": original_item_id,
                "bubble_target_scope": "root_session",
                "summary": summary,
                "target_run_id": target.run_id,
                "target_depth": target.depth,
            });
            let payload_json =
                serde_json::to_string(&payload).map_err(|source| StateProjectionError::Json {
                    operation: "serialize_bubble_up",
                    entity: source_run_id.to_string(),
                    source,
                })?;
            let payload_hash = content_hash(&payload_json);
            sqlx::query(
                "INSERT INTO session_state_items
                 (item_id, user_id, session_id, scope, category, item_key, status, priority,
                  source, run_id, title, summary_text, payload_json, payload_hash,
                  token_estimate, version, created_at, updated_at)
                 VALUES (?, ?, ?, 'session', 'delegation_state', ?, 'active', 100,
                         'delegation_bubble_up', ?, ?, ?, ?, ?, 80, 1, NOW(6), NOW(6))
                 ON DUPLICATE KEY UPDATE
                  summary_text = VALUES(summary_text), payload_json = VALUES(payload_json),
                  payload_hash = VALUES(payload_hash), version = version + 1, updated_at = NOW(6)",
            )
            .bind(&item_id)
            .bind(user_id)
            .bind(&target.session_id)
            .bind(&item_key)
            .bind(&target.run_id)
            .bind(format!("Critical finding from {source_run_id}"))
            .bind(summary)
            .bind(&payload_json)
            .bind(&payload_hash)
            .execute(&mut *tx)
            .await
            .map_err(|source| StateProjectionError::Database {
                operation: "upsert_bubble_state_item",
                entity: item_id.clone(),
                source,
            })?;
            sqlx::query(
                "INSERT INTO session_state_item_events
                 (event_id, item_id, user_id, session_id, category, item_key, mutation, next_hash,
                  payload_json, created_at)
                 VALUES (?, ?, ?, ?, 'delegation_state', ?, 'bubble_up', ?, ?, NOW(6))",
            )
            .bind(new_state_item_event_id())
            .bind(&item_id)
            .bind(user_id)
            .bind(&target.session_id)
            .bind(&item_key)
            .bind(&payload_hash)
            .bind(&payload_json)
            .execute(&mut *tx)
            .await
            .map_err(|source| StateProjectionError::Database {
                operation: "insert_bubble_up_event",
                entity: item_id,
                source,
            })?;
        }
        tx.commit()
            .await
            .map_err(|source| StateProjectionError::Database {
                operation: "commit_bubble_up",
                entity: source_run_id.to_string(),
                source,
            })
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
        self.activate_personal_skill_from_ui_with_probe(
            user_id, session_id, skill_name, version_id, None,
        )
        .await
    }

    pub async fn activate_personal_skill_from_ui_with_probe(
        &self,
        user_id: &str,
        session_id: &str,
        skill_name: &str,
        version_id: &str,
        _llm_probe: Option<&dyn SkillActivationLlmProbe>,
    ) -> Result<(), StateProjectionError> {
        let event_id = format!("event-{}", Uuid::new_v4());
        let item_id = format!("state-active-skill-{session_id}-{skill_name}");
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
        let mut tx =
            self.pool
                .get()
                .begin()
                .await
                .map_err(|source| StateProjectionError::Database {
                    operation: "begin_skill_activation",
                    entity: session_id.to_string(),
                    source,
                })?;
        let insert_result = sqlx::query(
            "INSERT INTO agent_events
             (event_id, session_id, user_id, event_type, content, metadata, created_at)
             VALUES (?, ?, ?, 'ui.skill.activate', ?, ?, NOW(6))",
        )
        .bind(&event_id)
        .bind(session_id)
        .bind(user_id)
        .bind(skill_name)
        .bind(&payload_json)
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
        crate::storage::add_agent_session_event_count_or_create(
            &mut tx,
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
            })
    }

    pub async fn can_access_artifact(
        &self,
        artifact_id: &str,
        requester_user_id: &str,
        requester_run_id: &str,
        requester_delegation_id: Option<&str>,
    ) -> Result<bool, StateProjectionError> {
        let Some(requester_run) = self
            .load_run_acl_for_user(requester_user_id, requester_run_id)
            .await?
        else {
            return Ok(false);
        };
        let Some(artifact) = sqlx::query(
            "SELECT artifact_id, user_id, session_id, access_scope, owner_run_id, root_run_id, status
             FROM session_artifacts
             WHERE user_id = ? AND session_id = ? AND artifact_id = ?
             LIMIT 1",
        )
        .bind(requester_user_id)
        .bind(&requester_run.session_id)
        .bind(artifact_id)
        .fetch_optional(self.pool.get())
        .await
        .map_err(|source| StateProjectionError::Database {
            operation: "load_artifact_acl",
            entity: artifact_id.to_string(),
            source,
        })?
        else {
            return Ok(false);
        };
        let artifact = decode_artifact_acl_row(&artifact, artifact_id)?;
        let status = artifact.status;
        if status != "active" {
            return Ok(false);
        }
        let artifact_user = artifact.user_id;
        let scope = artifact.access_scope;
        let owner_run_id = artifact.owner_run_id;
        let artifact_root_run_id = artifact.root_run_id;

        if owner_run_id.as_deref() == Some(requester_run_id) {
            return Ok(true);
        }
        if scope == "user" {
            return Ok(artifact_user == requester_user_id);
        }
        if artifact_user != requester_user_id {
            return Ok(false);
        }
        if self
            .has_artifact_grant(
                artifact_id,
                requester_user_id,
                &requester_run.session_id,
                requester_run_id,
                requester_delegation_id,
            )
            .await?
        {
            return Ok(true);
        }
        let artifact_root = artifact_root_run_id.unwrap_or_default();
        match scope.as_str() {
            "private" => Ok(false),
            "delegation" | "same_root_tree" => Ok(!artifact_root.is_empty()
                && requester_run.root_run_id.as_deref() == Some(artifact_root.as_str())),
            "delegation_direct" => {
                let Some(owner_run_id) = owner_run_id else {
                    return Ok(false);
                };
                let Some(owner_run) = self
                    .load_run_acl_for_user(requester_user_id, &owner_run_id)
                    .await?
                else {
                    return Ok(false);
                };
                let same_root = requester_run.root_run_id == owner_run.root_run_id;
                let requester_path = requester_run.ancestor_path.unwrap_or_default();
                let owner_path = owner_run.ancestor_path.unwrap_or_default();
                Ok(same_root
                    && (owner_path.starts_with(&requester_path)
                        || requester_path.starts_with(&owner_path)))
            }
            _ => Ok(false),
        }
    }

    pub async fn create_retry_run_and_supersede(
        &self,
        user_id: &str,
        old_run_id: &str,
        new_run_id: &str,
        retry_scope: &str,
    ) -> Result<(), StateProjectionError> {
        validate_retry_scope(new_run_id, retry_scope)?;
        let old = self
            .load_run_acl_for_user(user_id, old_run_id)
            .await?
            .ok_or_else(|| StateProjectionError::Database {
                operation: "load_old_retry_run",
                entity: old_run_id.to_string(),
                source: sqlx::Error::RowNotFound,
            })?;
        let mut tx =
            self.pool
                .get()
                .begin()
                .await
                .map_err(|source| StateProjectionError::Database {
                    operation: "begin_retry_supersede",
                    entity: old_run_id.to_string(),
                    source,
                })?;
        let supersede = sqlx::query(
            "UPDATE agent_runs
             SET status = 'superseded', updated_at = NOW(6)
             WHERE user_id = ? AND run_id = ?",
        )
        .bind(user_id)
        .bind(old_run_id)
        .execute(&mut *tx)
        .await
        .map_err(|source| StateProjectionError::Database {
            operation: "supersede_old_run",
            entity: old_run_id.to_string(),
            source,
        })?;
        if supersede.rows_affected() == 0 {
            return Err(StateProjectionError::Database {
                operation: "supersede_old_run",
                entity: old_run_id.to_string(),
                source: sqlx::Error::RowNotFound,
            });
        }
        let root = old.root_run_id.unwrap_or_else(|| old_run_id.to_string());
        let parent_path = old.ancestor_path.unwrap_or_else(|| old_run_id.to_string());
        sqlx::query(
            "INSERT INTO agent_runs
             (run_id, user_id, session_id, parent_run_id, root_run_id, ancestor_path, depth,
              retry_of, retry_scope, status, last_event_idx, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'running', -1, NOW(6), NOW(6))",
        )
        .bind(new_run_id)
        .bind(old.user_id)
        .bind(old.session_id)
        .bind(old_run_id)
        .bind(&root)
        .bind(format!("{parent_path}/{new_run_id}"))
        .bind(i64::from(old.depth.saturating_add(1)))
        .bind(old_run_id)
        .bind(retry_scope)
        .execute(&mut *tx)
        .await
        .map_err(|source| StateProjectionError::Database {
            operation: "insert_retry_run",
            entity: new_run_id.to_string(),
            source,
        })?;
        tx.commit()
            .await
            .map_err(|source| StateProjectionError::Database {
                operation: "commit_retry_supersede",
                entity: old_run_id.to_string(),
                source,
            })
    }

    async fn has_artifact_grant(
        &self,
        artifact_id: &str,
        user_id: &str,
        session_id: &str,
        requester_run_id: &str,
        requester_delegation_id: Option<&str>,
    ) -> Result<bool, StateProjectionError> {
        if self
            .has_artifact_run_grant(artifact_id, user_id, session_id, requester_run_id)
            .await?
        {
            return Ok(true);
        }

        let Some(requester_delegation_id) = requester_delegation_id else {
            return Ok(false);
        };

        self.has_artifact_delegation_grant(
            artifact_id,
            user_id,
            session_id,
            requester_delegation_id,
        )
        .await
    }

    async fn has_artifact_run_grant(
        &self,
        artifact_id: &str,
        user_id: &str,
        session_id: &str,
        requester_run_id: &str,
    ) -> Result<bool, StateProjectionError> {
        let row = sqlx::query(
            "SELECT grant_id FROM session_artifacts_grants FORCE INDEX (idx_artifacts_grants_target)
             WHERE user_id = ?
               AND session_id = ?
               AND target_run_id = ?
               AND artifact_id = ?
               AND (expires_at IS NULL OR expires_at > NOW(6))
             LIMIT 1",
        )
        .bind(user_id)
        .bind(session_id)
        .bind(requester_run_id)
        .bind(artifact_id)
        .fetch_optional(self.pool.get())
        .await
        .map_err(|source| StateProjectionError::Database {
            operation: "load_artifact_run_grant",
            entity: artifact_id.to_string(),
            source,
        })?;
        Ok(row.is_some())
    }

    async fn has_artifact_delegation_grant(
        &self,
        artifact_id: &str,
        user_id: &str,
        session_id: &str,
        requester_delegation_id: &str,
    ) -> Result<bool, StateProjectionError> {
        let row = sqlx::query(
            "SELECT grant_id FROM session_artifacts_grants FORCE INDEX (idx_artifacts_grants_delegation_target)
             WHERE user_id = ?
               AND session_id = ?
               AND target_delegation_id = ?
               AND artifact_id = ?
               AND (expires_at IS NULL OR expires_at > NOW(6))
             LIMIT 1",
        )
        .bind(user_id)
        .bind(session_id)
        .bind(requester_delegation_id)
        .bind(artifact_id)
        .fetch_optional(self.pool.get())
        .await
        .map_err(|source| StateProjectionError::Database {
            operation: "load_artifact_delegation_grant",
            entity: artifact_id.to_string(),
            source,
        })?;
        Ok(row.is_some())
    }

    async fn load_run_acl_for_user(
        &self,
        user_id: &str,
        run_id: &str,
    ) -> Result<Option<RunAclRow>, StateProjectionError> {
        let row = sqlx::query(
            "SELECT run_id, user_id, session_id, root_run_id, ancestor_path, depth
             FROM agent_runs WHERE user_id = ? AND run_id = ?",
        )
        .bind(user_id)
        .bind(run_id)
        .fetch_optional(self.pool.get())
        .await
        .map_err(|source| StateProjectionError::Database {
            operation: "load_run_acl_for_user",
            entity: run_id.to_string(),
            source,
        })?;
        row.map(|row| decode_run_acl_row(&row, run_id)).transpose()
    }

    async fn load_run_projection_for_user(
        &self,
        user_id: &str,
        run_id: &str,
    ) -> Result<Option<RunProjectionRow>, StateProjectionError> {
        let row = sqlx::query(
            "SELECT run_id, user_id, session_id, parent_run_id, root_run_id, ancestor_path,
                    depth, delegation_id, agent_id, status, retry_of, retry_scope
             FROM agent_runs WHERE user_id = ? AND run_id = ?",
        )
        .bind(user_id)
        .bind(run_id)
        .fetch_optional(self.pool.get())
        .await
        .map_err(|source| StateProjectionError::Database {
            operation: "load_run_projection_for_user",
            entity: run_id.to_string(),
            source,
        })?;
        row.map(|row| decode_run_projection_row(&row, run_id))
            .transpose()
    }
}

#[derive(Clone, Debug)]
struct RunAclRow {
    user_id: String,
    session_id: String,
    root_run_id: Option<String>,
    ancestor_path: Option<String>,
    depth: u32,
}

#[derive(Clone, Debug)]
struct RunProjectionRow {
    run_id: String,
    user_id: String,
    session_id: String,
    status: String,
    parent_run_id: Option<String>,
    root_run_id: Option<String>,
    ancestor_path: Option<String>,
    depth: u32,
    delegation_id: Option<String>,
    agent_id: Option<String>,
    retry_of: Option<String>,
    retry_scope: Option<String>,
}

fn validate_retry_scope(run_id: &str, retry_scope: &str) -> Result<(), StateProjectionError> {
    match retry_scope {
        "node" | "subtree" | "siblings" => Ok(()),
        other => Err(StateProjectionError::InvalidRetryScope {
            run_id: run_id.to_string(),
            retry_scope: other.to_string(),
        }),
    }
}

fn content_hash(content: &str) -> String {
    let digest = Sha256::digest(content.as_bytes());
    format!("sha256:{digest:x}")
}

fn new_state_item_event_id() -> String {
    Uuid::new_v4().to_string()
}

fn artifact_ids_from_state_payload(payload: &serde_json::Value) -> Vec<String> {
    fn visit(value: &serde_json::Value, out: &mut Vec<String>) {
        match value {
            serde_json::Value::String(raw) => {
                if let Some(artifact_id) = artifact_id_from_raw_ref(raw) {
                    out.push(artifact_id);
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    visit(item, out);
                }
            }
            serde_json::Value::Object(map) => {
                for (key, value) in map {
                    if matches!(
                        key.as_str(),
                        "artifact_id" | "source_artifact_id" | "derived_from_artifact_id"
                    ) && let Some(raw) = value.as_str()
                    {
                        out.push(raw.to_string());
                    }
                    visit(value, out);
                }
            }
            _ => {}
        }
    }
    let mut ids = Vec::new();
    visit(payload, &mut ids);
    ids.sort();
    ids.dedup();
    ids
}

pub fn validate_state_mutation(mutation: &str) -> Result<(), StateProjectionError> {
    match mutation {
        "insert" | "update" | "replace" | "archive" | "delete" | "bubble_up"
        | "apply_suggestion" | "activate" => Ok(()),
        other => Err(StateProjectionError::InvalidMutation {
            mutation: other.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delegation_projection_upsert() -> DelegationProjectionUpsert {
        DelegationProjectionUpsert {
            delegation_id: "delegation-1".to_string(),
            user_id: "user-1".to_string(),
            session_id: "session-1".to_string(),
            parent_run_id: "run-parent".to_string(),
            child_run_id: "run-child".to_string(),
            root_run_id: "run-root".to_string(),
            ancestor_path: "run-root/run-child".to_string(),
            depth: 1,
            agent_id: None,
            title: None,
            status: "running".to_string(),
            retry_of: None,
            retry_scope: "node".to_string(),
            last_summary_ref: None,
            last_summary_text: None,
            sibling_exposed_artifacts_json: None,
        }
    }

    #[test]
    fn delegation_statement_identity_includes_agent_and_title_nullness() {
        let without_agent = delegation_projection_upsert();
        let without_agent_sql = matrixone_statement_with_null_shape(
            "INSERT INTO session_delegations VALUES (?)",
            without_agent.nullable_shape(),
        );

        let mut with_agent = delegation_projection_upsert();
        with_agent.agent_id = Some("agent-1".to_string());
        with_agent.title = Some("Delegated run agent-1".to_string());
        let with_agent_sql = matrixone_statement_with_null_shape(
            "INSERT INTO session_delegations VALUES (?)",
            with_agent.nullable_shape(),
        );

        assert_ne!(without_agent_sql, with_agent_sql);
        assert!(with_agent_sql.contains("astra-null-shape:11"));
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
                "user_id" => "user-1",
                "session_id" => "session-1",
                "status" => "active",
                "access_scope" => "delegation",
                "todo_id" => "todo-1",
                "run_id" => "run-1",
                _ => return Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
            .to_string())
        }

        fn optional_string_column(&self, column: &str) -> Result<Option<String>, sqlx::Error> {
            self.fail_if_needed(column)?;
            Ok(match column {
                "summary_text" => Some("summary".to_string()),
                "owner_run_id" => Some("owner-run".to_string()),
                "root_run_id" => Some("root-run".to_string()),
                "ancestor_path" => Some("root-run/run-1".to_string()),
                "parent_run_id" => Some("parent-run".to_string()),
                "delegation_id" => Some("delegation-1".to_string()),
                "agent_id" => Some("agent-1".to_string()),
                "retry_of" => Some("old-run".to_string()),
                "retry_scope" => Some("node".to_string()),
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
                "active_count" => 1,
                "violations" => 0,
                "token_estimate" => 42,
                "depth" => 2,
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
    fn compaction_invariants_are_owner_bound() {
        for invariant in COMPACTION_INVARIANT_SQL {
            assert!(
                invariant.sql.contains("user_id = ?"),
                "{} must bind user_id explicitly",
                invariant.id
            );
        }
    }

    #[test]
    fn compaction_invariant_batch_sql_keeps_one_row_per_invariant() {
        let sql = compaction_invariant_batch_sql();
        for invariant in COMPACTION_INVARIANT_SQL {
            assert!(
                sql.contains(&format!("SELECT '{}' AS invariant_id", invariant.id)),
                "batch SQL missing invariant id {}",
                invariant.id
            );
        }
        assert_eq!(
            sql.matches(" UNION ALL ").count(),
            COMPACTION_INVARIANT_SQL.len().saturating_sub(1),
            "batch SQL must combine invariants into one round trip"
        );
        assert_eq!(
            sql.matches(") AS invariant_").count(),
            COMPACTION_INVARIANT_SQL.len(),
            "each invariant subquery must have a stable alias"
        );
    }

    #[test]
    fn state_mutation_validator_accepts_only_current_operations() {
        for mutation in [
            "insert",
            "update",
            "replace",
            "archive",
            "delete",
            "bubble_up",
            "apply_suggestion",
            "activate",
        ] {
            validate_state_mutation(mutation).expect("valid mutation");
        }

        let error = validate_state_mutation("teleport").expect_err("unknown mutation");
        assert!(matches!(
            error,
            StateProjectionError::InvalidMutation { mutation } if mutation == "teleport"
        ));
    }

    #[test]
    fn state_projection_counter_decoders_fail_loudly() {
        assert_eq!(
            state_projection_row_non_negative_i64(
                &FakeStateProjectionRow::complete(),
                "can_compact_session",
                "session-1",
                "active_count",
            )
            .expect("active_count decodes"),
            1
        );
        assert_database_error_mentions(
            state_projection_row_non_negative_i64(
                &FakeStateProjectionRow::fail_on("active_count"),
                "can_compact_session",
                "session-1",
                "active_count",
            ),
            "active_count",
        );
        assert_invalid_database_value(
            state_projection_row_non_negative_i64(
                &FakeStateProjectionRow::with_i64("active_count", -1),
                "can_compact_session",
                "session-1",
                "active_count",
            ),
            "active_count",
        );
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

    #[test]
    fn artifact_acl_decode_fails_loudly() {
        let artifact = decode_artifact_acl_row(&FakeStateProjectionRow::complete(), "artifact-1")
            .expect("artifact acl decodes");
        assert_eq!(artifact.user_id, "user-1");
        assert_eq!(artifact.access_scope, "delegation");
        assert_eq!(artifact.owner_run_id.as_deref(), Some("owner-run"));
        assert_eq!(artifact.root_run_id.as_deref(), Some("root-run"));
        assert_eq!(artifact.status, "active");

        for column in [
            "user_id",
            "access_scope",
            "owner_run_id",
            "root_run_id",
            "status",
        ] {
            assert_database_error_mentions(
                decode_artifact_acl_row(&FakeStateProjectionRow::fail_on(column), "artifact-1"),
                column,
            );
        }
    }

    #[test]
    fn run_acl_and_projection_decode_fail_loudly() {
        let acl =
            decode_run_acl_row(&FakeStateProjectionRow::complete(), "run-1").expect("acl decodes");
        assert_eq!(acl.user_id, "user-1");
        assert_eq!(acl.session_id, "session-1");
        assert_eq!(acl.root_run_id.as_deref(), Some("root-run"));
        assert_eq!(acl.ancestor_path.as_deref(), Some("root-run/run-1"));
        assert_eq!(acl.depth, 2);

        for column in [
            "user_id",
            "session_id",
            "root_run_id",
            "ancestor_path",
            "depth",
        ] {
            assert_database_error_mentions(
                decode_run_acl_row(&FakeStateProjectionRow::fail_on(column), "run-1"),
                column,
            );
        }
        assert_invalid_database_value(
            decode_run_acl_row(&FakeStateProjectionRow::with_i64("depth", -1), "run-1"),
            "depth",
        );
        assert_invalid_database_value(
            decode_run_acl_row(
                &FakeStateProjectionRow::with_i64("depth", i64::from(u32::MAX) + 1),
                "run-1",
            ),
            "depth",
        );

        let projection = decode_run_projection_row(&FakeStateProjectionRow::complete(), "run-1")
            .expect("projection decodes");
        assert_eq!(projection.run_id, "run-1");
        assert_eq!(projection.user_id, "user-1");
        assert_eq!(projection.session_id, "session-1");
        assert_eq!(projection.status, "active");
        assert_eq!(projection.parent_run_id.as_deref(), Some("parent-run"));
        assert_eq!(projection.root_run_id.as_deref(), Some("root-run"));
        assert_eq!(projection.ancestor_path.as_deref(), Some("root-run/run-1"));
        assert_eq!(projection.depth, 2);
        assert_eq!(projection.delegation_id.as_deref(), Some("delegation-1"));
        assert_eq!(projection.agent_id.as_deref(), Some("agent-1"));
        assert_eq!(projection.retry_of.as_deref(), Some("old-run"));
        assert_eq!(projection.retry_scope.as_deref(), Some("node"));

        for column in [
            "run_id",
            "user_id",
            "session_id",
            "status",
            "parent_run_id",
            "root_run_id",
            "ancestor_path",
            "depth",
            "delegation_id",
            "agent_id",
            "retry_of",
            "retry_scope",
        ] {
            assert_database_error_mentions(
                decode_run_projection_row(&FakeStateProjectionRow::fail_on(column), "run-1"),
                column,
            );
        }
    }
}
