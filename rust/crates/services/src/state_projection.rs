use astra_core::SharedPool;
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

/// One row to be inserted into `session_plan_todos` when a plan is
/// approved via `exit_plan_mode`. The struct mirrors the columns the
/// seed step writes; richer fields (`payload_json`, `acceptance_checks`,
/// `effort`, `files`) round-trip through the original plan_step_runs
/// table and are not duplicated here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanTodoSeed {
    pub todo_id: String,
    pub user_id: String,
    pub session_id: String,
    pub plan_id: String,
    pub title: String,
    /// 0-based index in the plan's subtask order. Used as both the
    /// SQL `priority` column and as the implicit dependency chain
    /// when subtasks declare `depends_on` against earlier IDs.
    pub priority: i32,
    /// Reserved for nested subtask trees; flat plans use 0.
    pub depth: i32,
}

/// Sink for `PlanTodoSeed` rows. Production uses the SQLx-backed
/// `DatabaseStateProjectionStore::seed_session_plan_todos`; tests use
/// an in-memory `Vec<PlanTodoSeed>` capture so they can verify the
/// shape of the seed without standing up MatrixOne.
#[async_trait::async_trait]
pub trait PlanTodoSink: Send + Sync + std::fmt::Debug {
    async fn seed(&self, todos: Vec<PlanTodoSeed>) -> Result<(), String>;
    /// U-6: mark prior plan's seeded rows as `superseded` so they
    /// don't show alongside the new plan's items in `task.list`.
    /// `keep_plan_id` is the active plan whose rows MUST stay
    /// active. Default impl is a no-op for sinks that don't track
    /// plan_id (e.g. legacy tests).
    async fn supersede_other_plans(
        &self,
        _user_id: &str,
        _session_id: &str,
        _keep_plan_id: &str,
    ) -> Result<u64, String> {
        Ok(0)
    }
}

/// Adapter wrapping `DatabaseStateProjectionStore` so it implements
/// `PlanTodoSink`. Kept as a thin wrapper so the production sink can
/// delegate to the existing `seed_session_plan_todos` method without
/// the trait bound polluting `DatabaseStateProjectionStore`'s public
/// API.
#[derive(Clone, Debug)]
pub struct DatabasePlanTodoSink {
    store: DatabaseStateProjectionStore,
}

impl DatabasePlanTodoSink {
    pub fn new(store: DatabaseStateProjectionStore) -> Self {
        Self { store }
    }
}

#[async_trait::async_trait]
impl PlanTodoSink for DatabasePlanTodoSink {
    async fn seed(&self, todos: Vec<PlanTodoSeed>) -> Result<(), String> {
        self.store
            .seed_session_plan_todos(&todos)
            .await
            .map_err(|e| e.to_string())
    }

    async fn supersede_other_plans(
        &self,
        user_id: &str,
        session_id: &str,
        keep_plan_id: &str,
    ) -> Result<u64, String> {
        self.store
            .supersede_session_plan_todos(user_id, session_id, keep_plan_id)
            .await
            .map_err(|e| e.to_string())
    }
}

#[derive(Clone, Debug)]
pub struct DatabaseStateProjectionStore {
    pool: SharedPool,
}

impl DatabaseStateProjectionStore {
    pub fn new(pool: SharedPool) -> Self {
        Self { pool }
    }

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
        let active_count = row.try_get::<i64, _>("active_count").unwrap_or(0);
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
        let mut out = Vec::with_capacity(COMPACTION_INVARIANT_SQL.len());
        for invariant in COMPACTION_INVARIANT_SQL {
            let mut query = sqlx::query(invariant.sql).bind(user_id).bind(session_id);
            if invariant.binds_compaction_run_id {
                query = query.bind(compaction_run_id);
            }
            let row = query.fetch_one(self.pool.get()).await.map_err(|source| {
                StateProjectionError::Database {
                    operation: "run_compaction_invariant",
                    entity: invariant.id.to_string(),
                    source,
                }
            })?;
            let violations = row.try_get::<i64, _>("violations").unwrap_or(0);
            out.push((invariant.id.to_string(), violations));
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
             (item_id, user_id, session_id, category, item_key, mutation, next_hash,
              payload_json, provenance_event_id, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, NOW(6))",
        )
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
                 WHERE artifact_id = ?",
            )
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
        child_run_id: &str,
        status: &str,
        agent_id_hint: Option<&str>,
        last_summary_text: Option<&str>,
    ) -> Result<(), StateProjectionError> {
        let Some(child) = self.load_run_projection(child_run_id).await? else {
            return Ok(());
        };
        let Some(parent_run_id) = child.parent_run_id.clone() else {
            return Ok(());
        };
        let Some(delegation_id) = child.delegation_id.clone() else {
            return Ok(());
        };
        let parent = self.load_run_projection(&parent_run_id).await?;
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
        sqlx::query(
            "UPDATE agent_runs
             SET root_run_id = ?, ancestor_path = ?, depth = ?, updated_at = NOW(6)
             WHERE run_id = ?",
        )
        .bind(&root_run_id)
        .bind(&ancestor_path)
        .bind(i64::from(depth))
        .bind(child_run_id)
        .execute(self.pool.get())
        .await
        .map_err(|source| StateProjectionError::Database {
            operation: "sync_delegation_run_tree",
            entity: child_run_id.to_string(),
            source,
        })?;
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
            status: status.to_string(),
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

        sqlx::query(
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
        )
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
             (item_id, user_id, session_id, category, item_key, mutation, next_hash,
              payload_json, created_at)
             VALUES (?, ?, ?, 'delegation_state', ?, 'insert', ?, ?, NOW(6))",
        )
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
                 (item_id, user_id, session_id, category, item_key, mutation, next_hash,
                  payload_json, created_at)
                 VALUES (?, ?, ?, 'delegation_state', ?, 'bubble_up', ?, ?, NOW(6))",
            )
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
            let estimate = row.try_get::<i64, _>("token_estimate").unwrap_or(0).max(0) as u32;
            if used.saturating_add(estimate) > token_budget {
                continue;
            }
            used = used.saturating_add(estimate);
            out.push(UserAnchorMemoryItem {
                item_id: row.try_get("item_id").unwrap_or_default(),
                category: row.try_get("category").unwrap_or_default(),
                item_key: row.try_get("item_key").unwrap_or_default(),
                summary_text: row.try_get("summary_text").ok(),
                token_estimate: estimate,
            });
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
        sqlx::query(
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
             (item_id, user_id, session_id, category, item_key, mutation, next_hash,
              payload_json, provenance_event_id, created_at)
             VALUES (?, ?, ?, 'active_skill', ?, 'activate', ?, ?, ?, NOW(6))",
        )
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
        let Some(artifact) = sqlx::query(
            "SELECT artifact_id, user_id, session_id, access_scope, owner_run_id, root_run_id, status
             FROM session_artifacts WHERE artifact_id = ?",
        )
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
        let status: String = artifact
            .try_get("status")
            .unwrap_or_else(|_| "active".to_string());
        if status != "active" {
            return Ok(false);
        }
        let artifact_user: String = artifact.try_get("user_id").unwrap_or_default();
        let scope: String = artifact
            .try_get("access_scope")
            .unwrap_or_else(|_| "private".to_string());
        let owner_run_id: Option<String> = artifact.try_get("owner_run_id").ok();
        let artifact_root_run_id: Option<String> = artifact.try_get("root_run_id").ok();

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
            .has_artifact_grant(artifact_id, requester_run_id, requester_delegation_id)
            .await?
        {
            return Ok(true);
        }
        let Some(requester_run) = self.load_run_acl(requester_run_id).await? else {
            return Ok(false);
        };
        let artifact_root = artifact_root_run_id.unwrap_or_default();
        match scope.as_str() {
            "private" => Ok(false),
            "delegation" | "same_root_tree" => Ok(!artifact_root.is_empty()
                && requester_run.root_run_id.as_deref() == Some(artifact_root.as_str())),
            "delegation_direct" => {
                let Some(owner_run_id) = owner_run_id else {
                    return Ok(false);
                };
                let Some(owner_run) = self.load_run_acl(&owner_run_id).await? else {
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

    /// Mark every active `session_plan_todos` row for this session
    /// that does NOT belong to `keep_plan_id` as `superseded`. Used
    /// when the user re-enters plan mode with a fresh plan: the prior
    /// plan's seeded items would otherwise stay `active` and pollute
    /// `task.list` results next to the new plan's items.
    ///
    /// Returns the number of rows transitioned. `keep_plan_id` is
    /// the new active plan; passing `None` would supersede every
    /// active row in the session — currently no caller wants that
    /// so the API requires the keep id explicitly to prevent a
    /// careless mass-supersede.
    pub async fn supersede_session_plan_todos(
        &self,
        user_id: &str,
        session_id: &str,
        keep_plan_id: &str,
    ) -> Result<u64, StateProjectionError> {
        let result = sqlx::query(
            "UPDATE session_plan_todos \
             SET status = 'superseded', updated_at = CURRENT_TIMESTAMP(6) \
             WHERE user_id = ? \
               AND session_id = ? \
               AND status = 'active' \
               AND (plan_id IS NULL OR plan_id <> ?)",
        )
        .bind(user_id)
        .bind(session_id)
        .bind(keep_plan_id)
        .execute(self.pool.get())
        .await
        .map_err(|source| StateProjectionError::Database {
            operation: "supersede_session_plan_todos",
            entity: session_id.to_string(),
            source,
        })?;
        Ok(result.rows_affected())
    }

    /// Insert one row per `PlanTodoSeed` into `session_plan_todos`.
    /// Idempotent on `todo_id`: re-running the same seed batch is a no-op
    /// because we use `INSERT IGNORE`. Order is preserved via `priority`
    /// so the next turn picks subtasks in the order the model authored
    /// them — `depends_on` chains still hold.
    ///
    /// This is the production sink wired into `tool_exit_plan_mode`
    /// after the model's `approved=true` path. The trait-based
    /// `PlanTodoSink` indirection keeps the executor testable without
    /// standing up MatrixOne.
    pub async fn seed_session_plan_todos(
        &self,
        seeds: &[PlanTodoSeed],
    ) -> Result<(), StateProjectionError> {
        if seeds.is_empty() {
            return Ok(());
        }
        for seed in seeds {
            sqlx::query(
                "INSERT IGNORE INTO session_plan_todos
                    (todo_id, user_id, session_id, plan_id, title, status, priority, depth)
                 VALUES (?, ?, ?, ?, ?, 'active', ?, ?)",
            )
            .bind(&seed.todo_id)
            .bind(&seed.user_id)
            .bind(&seed.session_id)
            .bind(&seed.plan_id)
            .bind(&seed.title)
            .bind(seed.priority)
            .bind(seed.depth)
            .execute(self.pool.get())
            .await
            .map_err(|source| StateProjectionError::Database {
                operation: "seed_session_plan_todos",
                entity: seed.todo_id.clone(),
                source,
            })?;
        }
        Ok(())
    }

    pub async fn restore_backlog_pool(
        &self,
        user_id: &str,
        backlog_pool_id: &str,
    ) -> Result<Vec<String>, StateProjectionError> {
        let rows = sqlx::query(
            "SELECT todo_id FROM session_plan_todos FORCE INDEX (idx_session_plan_todos_pool)
             WHERE user_id = ? AND backlog_pool_id = ? AND status = 'backlog'
             ORDER BY updated_at DESC LIMIT 100",
        )
        .bind(user_id)
        .bind(backlog_pool_id)
        .fetch_all(self.pool.get())
        .await
        .map_err(|source| StateProjectionError::Database {
            operation: "restore_backlog_pool",
            entity: backlog_pool_id.to_string(),
            source,
        })?;
        Ok(rows
            .into_iter()
            .filter_map(|row| row.try_get::<String, _>("todo_id").ok())
            .collect())
    }

    pub async fn create_retry_run_and_supersede(
        &self,
        old_run_id: &str,
        new_run_id: &str,
        retry_scope: &str,
    ) -> Result<(), StateProjectionError> {
        validate_retry_scope(new_run_id, retry_scope)?;
        let old =
            self.load_run_acl(old_run_id)
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
        sqlx::query(
            "UPDATE agent_runs SET status = 'superseded', updated_at = NOW(6) WHERE run_id = ?",
        )
        .bind(old_run_id)
        .execute(&mut *tx)
        .await
        .map_err(|source| StateProjectionError::Database {
            operation: "supersede_old_run",
            entity: old_run_id.to_string(),
            source,
        })?;
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
        requester_run_id: &str,
        requester_delegation_id: Option<&str>,
    ) -> Result<bool, StateProjectionError> {
        if self
            .has_artifact_run_grant(artifact_id, requester_run_id)
            .await?
        {
            return Ok(true);
        }

        let Some(requester_delegation_id) = requester_delegation_id else {
            return Ok(false);
        };

        self.has_artifact_delegation_grant(artifact_id, requester_delegation_id)
            .await
    }

    async fn has_artifact_run_grant(
        &self,
        artifact_id: &str,
        requester_run_id: &str,
    ) -> Result<bool, StateProjectionError> {
        let row = sqlx::query(
            "SELECT grant_id FROM session_artifacts_grants FORCE INDEX (idx_artifacts_grants_target)
             WHERE target_run_id = ?
               AND artifact_id = ?
               AND (expires_at IS NULL OR expires_at > NOW(6))
             LIMIT 1",
        )
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
        requester_delegation_id: &str,
    ) -> Result<bool, StateProjectionError> {
        let row = sqlx::query(
            "SELECT grant_id FROM session_artifacts_grants FORCE INDEX (idx_artifacts_grants_delegation_target)
             WHERE target_delegation_id = ?
               AND artifact_id = ?
               AND (expires_at IS NULL OR expires_at > NOW(6))
             LIMIT 1",
        )
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

    async fn load_run_acl(&self, run_id: &str) -> Result<Option<RunAclRow>, StateProjectionError> {
        let row = sqlx::query(
            "SELECT run_id, user_id, session_id, root_run_id, ancestor_path, depth
             FROM agent_runs WHERE run_id = ?",
        )
        .bind(run_id)
        .fetch_optional(self.pool.get())
        .await
        .map_err(|source| StateProjectionError::Database {
            operation: "load_run_acl",
            entity: run_id.to_string(),
            source,
        })?;
        Ok(row.map(|row| RunAclRow {
            user_id: row.try_get("user_id").unwrap_or_default(),
            session_id: row.try_get("session_id").unwrap_or_default(),
            root_run_id: row.try_get("root_run_id").ok(),
            ancestor_path: row.try_get("ancestor_path").ok(),
            depth: row.try_get::<i64, _>("depth").unwrap_or(0).max(0) as u32,
        }))
    }

    async fn load_run_projection(
        &self,
        run_id: &str,
    ) -> Result<Option<RunProjectionRow>, StateProjectionError> {
        let row = sqlx::query(
            "SELECT run_id, user_id, session_id, parent_run_id, root_run_id, ancestor_path,
                    depth, delegation_id, agent_id, retry_of, retry_scope
             FROM agent_runs WHERE run_id = ?",
        )
        .bind(run_id)
        .fetch_optional(self.pool.get())
        .await
        .map_err(|source| StateProjectionError::Database {
            operation: "load_run_projection",
            entity: run_id.to_string(),
            source,
        })?;
        Ok(row.map(|row| RunProjectionRow {
            run_id: row.try_get("run_id").unwrap_or_default(),
            user_id: row.try_get("user_id").unwrap_or_default(),
            session_id: row.try_get("session_id").unwrap_or_default(),
            parent_run_id: row.try_get("parent_run_id").ok(),
            root_run_id: row.try_get("root_run_id").ok(),
            ancestor_path: row.try_get("ancestor_path").ok(),
            depth: row.try_get::<i64, _>("depth").unwrap_or(0).max(0) as u32,
            delegation_id: row.try_get("delegation_id").ok(),
            agent_id: row.try_get("agent_id").ok(),
            retry_of: row.try_get("retry_of").ok(),
            retry_scope: row.try_get("retry_scope").ok(),
        }))
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
}
