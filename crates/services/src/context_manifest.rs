use std::collections::BTreeMap;

use crate::CancellationSafePoolConnection;
use crate::db_row::RowExt as ContextManifestDbRow;
use crate::observation_capture::{
    DurableCaptureOutcome, ObservationCollisionReceipt, ObservationPayloadDomain,
    canonical_observation_payload_hash, classify_capture, record_observation_collisions,
};
use astra_core::{SharedPool, matrixone_null_shape_comment, matrixone_statement_with_null_shape};
use serde::{Deserialize, Serialize};
use sqlx::{Connection, MySql, QueryBuilder};
use thiserror::Error;
use uuid::Uuid;

pub const BUDGET_V1_8K_TOTAL_CAP: u32 = 7_300;
pub const BUDGET_V1_8K_PROMPT_CAP: u32 = 8_000;
pub const BENCHMARK_TOOL_PREVIEW_BUDGET: u32 = 2_500;
pub const RECENT_TAIL_BENCHMARK_FLOOR: u32 = 1_600;
pub const SYSTEM_TOOL_SCHEMAS_MAX: u32 = 3_400;
pub const TURN_INTENT_BENCHMARK_COMPARISON: &str = "benchmark_comparison";
pub const SESSION_ARTIFACT_STATUS_EXPIRED: &str = "expired";

/// Keep one manifest write bounded even when a future context assembler emits
/// many more sections than the current projection. A chunk is still inserted
/// in the same transaction and under the same session admission lock.
const CONTEXT_MANIFEST_ITEM_INSERT_BATCH_SIZE: usize = 128;
const CONTEXT_MANIFEST_ITEM_INSERT_SQL: &str = "INSERT INTO context_manifest_items
     (user_id, manifest_id, session_id, item_order, zone, source_table, source_id, source_hash,
      included, token_estimate, budget_tokens, reason, render_mode, raw_ref, created_at)
     ";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionArtifactStatusKind {
    Expired,
    Other,
}

pub fn session_artifact_status_kind(status: &str) -> SessionArtifactStatusKind {
    match status {
        SESSION_ARTIFACT_STATUS_EXPIRED => SessionArtifactStatusKind::Expired,
        _ => SessionArtifactStatusKind::Other,
    }
}

pub fn session_artifact_raw_payload_is_available(status: &str) -> bool {
    session_artifact_status_kind(status) != SessionArtifactStatusKind::Expired
}

pub const CONTEXT_MANIFEST_REASONS: &[(&str, &str, Option<&str>)] = &[
    ("initial_turn", "lifecycle", Some("session_anchor")),
    ("normal_turn", "lifecycle", Some("recent_tail")),
    ("post_compaction", "compaction", Some("summary")),
    (
        "history_recall_structured",
        "retrieval",
        Some("retrieved_facts"),
    ),
    ("history_recall_fts", "retrieval", Some("retrieved_facts")),
    (
        "history_recall_vector",
        "retrieval",
        Some("retrieved_facts"),
    ),
    ("large_tool_output_gated", "artifact", Some("tool_previews")),
    ("plan_subtree_query", "plan", Some("plan_todo")),
    ("tree_structured_report", "plan", Some("plan_todo")),
    ("workspace_switch", "workspace", Some("workspace")),
    ("approval_resume", "approval", Some("safety_approvals")),
    ("cross_session_recall", "retrieval", Some("retrieved_facts")),
    ("delegation_poll", "delegation", Some("delegation_state")),
    (
        "partial_blocker_review",
        "delegation",
        Some("delegation_state"),
    ),
    (
        "delegation_aggregate",
        "delegation",
        Some("delegation_state"),
    ),
    ("cross_skill_alignment", "skills", Some("skills")),
    ("skill_quality_review", "skills", Some("skills")),
    ("final_delivery_summary", "lifecycle", Some("summary")),
    ("ambiguity_clarification", "next_action", Some("plan_todo")),
    (
        "execute_after_clarification",
        "next_action",
        Some("plan_todo"),
    ),
    ("user_memory_promote", "memory", Some("session_anchor")),
    ("user_memory_archive", "memory", Some("session_anchor")),
    ("user_memory_revise", "memory", Some("session_anchor")),
    (
        "user_memory_loaded_on_init",
        "memory",
        Some("session_anchor"),
    ),
    ("progressive_loading", "budget", None),
    (
        "intent_driven_preview_expand",
        "budget",
        Some("tool_previews"),
    ),
    ("other", "fallback", None),
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BudgetV1_8k {
    pub anchor: u32,
    pub plan_todo: u32,
    pub recent_tail: u32,
    pub summary: u32,
    pub retrieved: u32,
    pub tool_previews: u32,
    pub system_tool_schemas: u32,
    pub reserved_output: u32,
    pub safety_buffer: u32,
}

impl BudgetV1_8k {
    pub fn standard() -> Self {
        Self {
            anchor: 200,
            plan_todo: 400,
            recent_tail: 2000,
            summary: 500,
            retrieved: 1000,
            tool_previews: 500,
            system_tool_schemas: SYSTEM_TOOL_SCHEMAS_MAX,
            reserved_output: 500,
            safety_buffer: 200,
        }
    }

    pub fn prompt_cap(&self) -> u32 {
        self.anchor
            + self.plan_todo
            + self.recent_tail
            + self.summary
            + self.retrieved
            + self.tool_previews
            + self.system_tool_schemas
    }

    pub fn input_context_cap(&self) -> u32 {
        self.prompt_cap()
            .saturating_sub(self.reserved_output + self.safety_buffer)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnIntentBudgetAllocation {
    pub budget: BudgetV1_8k,
    pub borrowed_from_recent_tail: u32,
    pub flex_applied: bool,
}

pub fn budget_for_turn_intent(turn_intent: Option<&str>) -> TurnIntentBudgetAllocation {
    let mut budget = BudgetV1_8k::standard();
    if turn_intent == Some(TURN_INTENT_BENCHMARK_COMPARISON) {
        let borrowed = budget
            .recent_tail
            .saturating_sub(RECENT_TAIL_BENCHMARK_FLOOR);
        budget.recent_tail = RECENT_TAIL_BENCHMARK_FLOOR;
        budget.tool_previews = BENCHMARK_TOOL_PREVIEW_BUDGET;
        let overflow = budget.prompt_cap().saturating_sub(BUDGET_V1_8K_PROMPT_CAP);
        budget.system_tool_schemas = budget.system_tool_schemas.saturating_sub(overflow);
        return TurnIntentBudgetAllocation {
            budget,
            borrowed_from_recent_tail: borrowed,
            flex_applied: true,
        };
    }

    TurnIntentBudgetAllocation {
        budget,
        borrowed_from_recent_tail: 0,
        flex_applied: false,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RetrievalStage {
    Structured,
    Fts,
    Vector,
}

impl RetrievalStage {
    pub fn timeout_ms(&self) -> u64 {
        match self {
            RetrievalStage::Structured => 50,
            RetrievalStage::Fts => 200,
            RetrievalStage::Vector => 500,
        }
    }

    pub fn event_type(&self, reason: &str) -> String {
        match self {
            RetrievalStage::Structured => format!("retrieval.structured_{reason}"),
            RetrievalStage::Fts => format!("retrieval.fts_{reason}"),
            RetrievalStage::Vector => format!("retrieval.vector_{reason}"),
        }
    }

    pub fn next_stage(&self) -> Option<RetrievalStage> {
        match self {
            RetrievalStage::Structured => Some(RetrievalStage::Fts),
            RetrievalStage::Fts => Some(RetrievalStage::Vector),
            RetrievalStage::Vector => None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContextManifestWrite {
    pub manifest_id: String,
    pub user_id: String,
    pub session_id: String,
    pub run_id: Option<String>,
    pub turn_id: String,
    pub model_provider: String,
    pub model_name: String,
    pub context_window_tokens: u32,
    pub max_output_tokens: u32,
    pub total_estimated_tokens: u32,
    pub policy_version: String,
    pub tokenizer_id: Option<String>,
    pub budget_template_id: Option<String>,
    pub turn_intent: Option<String>,
    pub reason: String,
    pub manifest_json: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContextManifestItemWrite {
    pub session_id: String,
    pub item_order: i32,
    pub zone: String,
    pub source_table: String,
    pub source_id: String,
    pub source_hash: Option<String>,
    pub included: bool,
    pub token_estimate: u32,
    pub budget_tokens: u32,
    pub reason: String,
    pub render_mode: String,
    pub raw_ref: Option<String>,
}

#[derive(Debug, Error)]
pub enum ContextManifestError {
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
    #[error("session is not active: owner={user_id}, session={session_id}")]
    SessionNotActive { user_id: String, session_id: String },
}

fn context_manifest_session_admission_error(
    source: sqlx::Error,
    user_id: &str,
    session_id: &str,
    entity: &str,
) -> ContextManifestError {
    match source {
        sqlx::Error::RowNotFound => ContextManifestError::SessionNotActive {
            user_id: user_id.to_string(),
            session_id: session_id.to_string(),
        },
        source => ContextManifestError::Database {
            operation: "admit_context_manifest_session",
            entity: entity.to_string(),
            source,
        },
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionArtifactManifestRow {
    status: String,
    content_json: String,
    metadata: Option<String>,
}

fn context_manifest_decode_error(
    operation: &'static str,
    entity: &str,
    column: &str,
    source: sqlx::Error,
) -> ContextManifestError {
    ContextManifestError::Database {
        operation,
        entity: format!("{entity}.{column}"),
        source,
    }
}

fn context_manifest_row_string(
    row: &impl ContextManifestDbRow,
    operation: &'static str,
    entity: &str,
    column: &str,
) -> Result<String, ContextManifestError> {
    row.string_column(column)
        .map_err(|source| context_manifest_decode_error(operation, entity, column, source))
}

fn context_manifest_row_optional_string(
    row: &impl ContextManifestDbRow,
    operation: &'static str,
    entity: &str,
    column: &str,
) -> Result<Option<String>, ContextManifestError> {
    row.optional_string_column(column)
        .map_err(|source| context_manifest_decode_error(operation, entity, column, source))
}

fn decode_session_artifact_manifest_row(
    row: &impl ContextManifestDbRow,
    artifact_id: &str,
) -> Result<SessionArtifactManifestRow, ContextManifestError> {
    let operation = "render_manifest_artifact_decode";
    Ok(SessionArtifactManifestRow {
        status: context_manifest_row_string(row, operation, artifact_id, "status")?,
        content_json: context_manifest_row_string(row, operation, artifact_id, "content_json")?,
        metadata: context_manifest_row_optional_string(row, operation, artifact_id, "metadata")?,
    })
}

#[derive(Clone)]
pub struct DatabaseContextManifestStore {
    pool: SharedPool,
}

struct SessionEventInsert<'a> {
    user_id: &'a str,
    session_id: &'a str,
    event_type: &'a str,
    content: &'a str,
    metadata: serde_json::Value,
    operation: &'static str,
    entity: &'a str,
}

impl DatabaseContextManifestStore {
    pub fn new(pool: SharedPool) -> Self {
        Self { pool }
    }

    async fn insert_session_event_and_bump_count(
        &self,
        event: SessionEventInsert<'_>,
    ) -> Result<String, ContextManifestError> {
        let event_id = Uuid::new_v4().to_string();
        let mut connection = CancellationSafePoolConnection::acquire(self.pool.get())
            .await
            .map_err(|source| ContextManifestError::Database {
                operation: event.operation,
                entity: event.entity.to_string(),
                source,
            })?;
        let mut tx = connection
            .connection_mut()
            .begin()
            .await
            .map_err(|source| ContextManifestError::Database {
                operation: event.operation,
                entity: event.entity.to_string(),
                source,
            })?;
        crate::storage::admit_session_event_write(&mut tx, event.session_id, event.user_id, true)
            .await
            .map_err(|source| ContextManifestError::Database {
                operation: event.operation,
                entity: event.entity.to_string(),
                source,
            })?;
        Self::insert_session_event_in_transaction(&mut tx, &event_id, &event).await?;
        tx.commit()
            .await
            .map_err(|source| ContextManifestError::Database {
                operation: event.operation,
                entity: event.entity.to_string(),
                source,
            })?;
        connection.release();
        Ok(event_id)
    }

    async fn insert_session_event_in_transaction(
        tx: &mut sqlx::Transaction<'_, MySql>,
        event_id: &str,
        event: &SessionEventInsert<'_>,
    ) -> Result<(), ContextManifestError> {
        let payload_hash = canonical_observation_payload_hash(
            ObservationPayloadDomain::AgentEvent,
            &serde_json::json!({
                "event_id": event_id, "session_id": event.session_id,
                "user_id": event.user_id, "event_type": event.event_type,
                "content": event.content, "metadata": event.metadata,
            }),
        );
        let insert_result = sqlx::query(
            "INSERT INTO agent_events
             (event_id, session_id, user_id, event_type, content, metadata,
              payload_hash, ingestion_write_id, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, NOW(6))",
        )
        .bind(event_id)
        .bind(event.session_id)
        .bind(event.user_id)
        .bind(event.event_type)
        .bind(event.content)
        .bind(event.metadata.to_string())
        .bind(payload_hash)
        .bind(Uuid::new_v4().to_string())
        .execute(&mut **tx)
        .await
        .map_err(|source| ContextManifestError::Database {
            operation: event.operation,
            entity: event.entity.to_string(),
            source,
        })?;
        let inserted_events =
            crate::storage::rows_affected_to_i64(insert_result.rows_affected(), event.operation)
                .map_err(|source| ContextManifestError::Database {
                    operation: event.operation,
                    entity: event.entity.to_string(),
                    source,
                })?;
        if inserted_events <= 0 {
            return Err(ContextManifestError::Database {
                operation: event.operation,
                entity: event.entity.to_string(),
                source: sqlx::Error::Protocol("session event insert affected no rows".into()),
            });
        }
        crate::storage::bump_agent_session_event_count(
            &mut **tx,
            event.session_id,
            event.user_id,
            inserted_events,
            Some(event_id),
        )
        .await
        .map_err(|source| ContextManifestError::Database {
            operation: event.operation,
            entity: event.entity.to_string(),
            source,
        })?;
        Ok(())
    }

    pub async fn normalize_reason(
        &self,
        user_id: &str,
        proposed_reason: &str,
        session_id: &str,
        run_id: Option<&str>,
        turn_id: &str,
        component: &str,
    ) -> Result<String, ContextManifestError> {
        let known = CONTEXT_MANIFEST_REASONS
            .iter()
            .any(|(reason, _, _)| *reason == proposed_reason);
        if known {
            return Ok(proposed_reason.to_string());
        }
        self.insert_session_event_and_bump_count(SessionEventInsert {
            user_id,
            session_id,
            event_type: "manifest.reason_unknown",
            content: proposed_reason,
            metadata: serde_json::json!({
                "proposed_reason": proposed_reason,
                "turn_id": turn_id,
                "run_id": run_id,
                "component": component,
            }),
            operation: "manifest_reason_unknown_event",
            entity: session_id,
        })
        .await
        .map(|_| ())?;
        Ok("other".to_string())
    }

    pub async fn save_manifest(
        &self,
        manifest: ContextManifestWrite,
        mut items: Vec<ContextManifestItemWrite>,
    ) -> Result<DurableCaptureOutcome, ContextManifestError> {
        items.sort_by_key(|item| item.item_order);
        let known_reason = CONTEXT_MANIFEST_REASONS
            .iter()
            .any(|(reason, _, _)| *reason == manifest.reason);
        let reason = if known_reason {
            manifest.reason.as_str()
        } else {
            "other"
        };
        let payload_hash = canonical_observation_payload_hash(
            ObservationPayloadDomain::ContextManifest,
            &serde_json::json!({"manifest": &manifest, "items": &items}),
        );
        let write_id = Uuid::new_v4().to_string();
        let dropped_count = items.iter().filter(|item| !item.included).count() as i64;
        let manifest_json = serde_json::to_string(&manifest.manifest_json).map_err(|source| {
            ContextManifestError::Json {
                operation: "serialize_context_manifest",
                entity: manifest.manifest_id.clone(),
                source,
            }
        })?;
        let artifact_references = aggregate_artifact_references(&items);
        let mut connection = CancellationSafePoolConnection::acquire(self.pool.get())
            .await
            .map_err(|source| ContextManifestError::Database {
                operation: "acquire_context_manifest_connection",
                entity: manifest.manifest_id.clone(),
                source,
            })?;
        let mut tx = connection
            .connection_mut()
            .begin()
            .await
            .map_err(|source| ContextManifestError::Database {
                operation: "begin_context_manifest",
                entity: manifest.manifest_id.clone(),
                source,
            })?;
        crate::storage::admit_session_event_write(
            &mut tx,
            &manifest.session_id,
            &manifest.user_id,
            false,
        )
        .await
        .map_err(|source| {
            context_manifest_session_admission_error(
                source,
                &manifest.user_id,
                &manifest.session_id,
                &manifest.manifest_id,
            )
        })?;
        let manifest_insert_sql = matrixone_statement_with_null_shape(
            "INSERT IGNORE INTO context_manifests
             (manifest_id, user_id, session_id, run_id, turn_id, model_provider, model_name,
              context_window_tokens, max_output_tokens, total_estimated_tokens, policy_version,
              tokenizer_id, budget_template_id, turn_intent, reason, dropped_count, manifest_json,
              payload_hash, ingestion_write_id, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW(6))",
            [
                manifest.run_id.is_some(),
                manifest.tokenizer_id.is_some(),
                manifest.budget_template_id.is_some(),
                manifest.turn_intent.is_some(),
            ],
        );
        sqlx::query(&manifest_insert_sql)
            .bind(&manifest.manifest_id)
            .bind(&manifest.user_id)
            .bind(&manifest.session_id)
            .bind(&manifest.run_id)
            .bind(&manifest.turn_id)
            .bind(&manifest.model_provider)
            .bind(&manifest.model_name)
            .bind(i64::from(manifest.context_window_tokens))
            .bind(i64::from(manifest.max_output_tokens))
            .bind(i64::from(manifest.total_estimated_tokens))
            .bind(&manifest.policy_version)
            .bind(&manifest.tokenizer_id)
            .bind(&manifest.budget_template_id)
            .bind(&manifest.turn_intent)
            .bind(reason)
            .bind(dropped_count)
            .bind(manifest_json)
            .bind(&payload_hash)
            .bind(&write_id)
            .execute(&mut *tx)
            .await
            .map_err(|source| ContextManifestError::Database {
                operation: "insert_context_manifest",
                entity: manifest.manifest_id.clone(),
                source,
            })?;
        let stored: (String, String) = sqlx::query_as(
            "SELECT payload_hash, ingestion_write_id FROM context_manifests
             WHERE user_id = ? AND manifest_id = ?",
        )
        .bind(&manifest.user_id)
        .bind(&manifest.manifest_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|source| ContextManifestError::Database {
            operation: "classify_context_manifest",
            entity: manifest.manifest_id.clone(),
            source,
        })?;
        let outcome = classify_capture(&stored.0, &stored.1, &payload_hash, &write_id);
        if let DurableCaptureOutcome::Collision {
            stored_payload_hash,
            attempted_payload_hash,
        } = &outcome
        {
            record_observation_collisions(
                &mut tx,
                &[ObservationCollisionReceipt {
                    user_id: &manifest.user_id,
                    domain: ObservationPayloadDomain::ContextManifest,
                    identity_id: &manifest.manifest_id,
                    session_id: &manifest.session_id,
                    stored_payload_hash,
                    attempted_payload_hash,
                    source: "context_manifest",
                }],
            )
            .await
            .map_err(|source| ContextManifestError::Database {
                operation: "record_context_manifest_collision",
                entity: manifest.manifest_id.clone(),
                source,
            })?;
        }
        if outcome != DurableCaptureOutcome::Inserted {
            tx.commit()
                .await
                .map_err(|source| ContextManifestError::Database {
                    operation: "commit_context_manifest_replay",
                    entity: manifest.manifest_id.clone(),
                    source,
                })?;
            connection.release();
            return Ok(outcome);
        }
        if !known_reason {
            Self::insert_session_event_in_transaction(
                &mut tx,
                &Uuid::new_v4().to_string(),
                &SessionEventInsert {
                    user_id: &manifest.user_id,
                    session_id: &manifest.session_id,
                    event_type: "manifest.reason_unknown",
                    content: &manifest.reason,
                    metadata: serde_json::json!({
                        "proposed_reason": manifest.reason,
                        "turn_id": manifest.turn_id,
                        "run_id": manifest.run_id,
                        "component": "context_manifest_store",
                    }),
                    operation: "manifest_reason_unknown_event",
                    entity: &manifest.manifest_id,
                },
            )
            .await?;
        }
        for item_batch in items.chunks(CONTEXT_MANIFEST_ITEM_INSERT_BATCH_SIZE) {
            let mut query = QueryBuilder::<MySql>::new(CONTEXT_MANIFEST_ITEM_INSERT_SQL);
            query.push_values(item_batch, |mut values, item| {
                values
                    .push_bind(&manifest.user_id)
                    .push_bind(&manifest.manifest_id)
                    .push_bind(&item.session_id)
                    .push_bind(item.item_order)
                    .push_bind(&item.zone)
                    .push_bind(&item.source_table)
                    .push_bind(&item.source_id)
                    .push_bind(&item.source_hash)
                    .push_bind(if item.included { 1_i8 } else { 0_i8 })
                    .push_bind(i64::from(item.token_estimate))
                    .push_bind(i64::from(item.budget_tokens))
                    .push_bind(&item.reason)
                    .push_bind(&item.render_mode)
                    .push_bind(&item.raw_ref)
                    .push("NOW(6)");
            });
            query.push(matrixone_null_shape_comment(
                item_batch
                    .iter()
                    .flat_map(context_manifest_item_nullable_shape),
            ));
            query.build().execute(&mut *tx).await.map_err(|source| {
                ContextManifestError::Database {
                    operation: "insert_context_manifest_items",
                    entity: manifest.manifest_id.clone(),
                    source,
                }
            })?;
        }
        for ((session_id, artifact_id), reference_count) in artifact_references {
            sqlx::query(
                "UPDATE session_artifacts
                 SET referenced_by_manifest_count = referenced_by_manifest_count + ?,
                     updated_at = NOW(6)
                 WHERE user_id = ? AND session_id = ? AND artifact_id = ?",
            )
            .bind(reference_count)
            .bind(&manifest.user_id)
            .bind(&session_id)
            .bind(&artifact_id)
            .execute(&mut *tx)
            .await
            .map_err(|source| ContextManifestError::Database {
                operation: "increment_manifest_artifact_ref",
                entity: artifact_id,
                source,
            })?;
        }
        tx.commit()
            .await
            .map_err(|source| ContextManifestError::Database {
                operation: "commit_context_manifest",
                entity: manifest.manifest_id.clone(),
                source,
            })?;
        connection.release();
        Ok(DurableCaptureOutcome::Inserted)
    }

    pub async fn record_retrieval_degrade_event(
        &self,
        user_id: &str,
        session_id: &str,
        run_id: Option<&str>,
        stage: RetrievalStage,
        reason: &str,
        elapsed_ms: u64,
    ) -> Result<Option<RetrievalStage>, ContextManifestError> {
        let event_type = stage.event_type(reason);
        let next_stage = stage.next_stage();
        self.insert_session_event_and_bump_count(SessionEventInsert {
            user_id,
            session_id,
            event_type: &event_type,
            content: reason,
            metadata: serde_json::json!({
                "run_id": run_id,
                "stage": format!("{stage:?}"),
                "reason": reason,
                "elapsed_ms": elapsed_ms,
                "sla_ms": stage.timeout_ms(),
                "next_stage": next_stage.as_ref().map(|stage| format!("{stage:?}")),
            }),
            operation: "insert_retrieval_degrade_event",
            entity: session_id,
        })
        .await
        .map(|_| ())?;
        Ok(next_stage)
    }

    pub async fn render_artifact_manifest_item(
        &self,
        user_id: &str,
        session_id: &str,
        artifact_id: &str,
        summary_hint: Option<&str>,
    ) -> Result<String, ContextManifestError> {
        let row = sqlx::query(
            "SELECT status, content_json, CAST(metadata AS CHAR) AS metadata
             FROM session_artifacts
             WHERE user_id = ? AND session_id = ? AND artifact_id = ?
             LIMIT 1",
        )
        .bind(user_id)
        .bind(session_id)
        .bind(artifact_id)
        .fetch_optional(self.pool.get())
        .await
        .map_err(|source| ContextManifestError::Database {
            operation: "render_manifest_artifact_lookup",
            entity: artifact_id.to_string(),
            source,
        })?;

        let Some(row) = row else {
            return Ok(expired_artifact_placeholder(artifact_id, summary_hint));
        };

        let artifact_row = decode_session_artifact_manifest_row(&row, artifact_id)?;
        let summary = artifact_summary_for_placeholder(
            summary_hint,
            artifact_row.metadata.as_deref(),
            artifact_row.content_json.as_str(),
        );

        if !session_artifact_raw_payload_is_available(&artifact_row.status) {
            return Ok(expired_artifact_placeholder(
                artifact_id,
                summary.as_deref(),
            ));
        }
        Ok(summary.unwrap_or(artifact_row.content_json))
    }
}

fn context_manifest_item_nullable_shape(item: &ContextManifestItemWrite) -> [bool; 2] {
    [item.source_hash.is_some(), item.raw_ref.is_some()]
}

pub fn expired_artifact_placeholder(artifact_id: &str, summary: Option<&str>) -> String {
    match summary.filter(|value| !value.trim().is_empty()) {
        Some(summary) => format!(
            "artifact {artifact_id}: historical, raw no longer available, summary preserved: {summary}"
        ),
        None => format!(
            "artifact {artifact_id}: historical, raw no longer available, summary preserved"
        ),
    }
}

fn artifact_summary_for_placeholder(
    summary_hint: Option<&str>,
    metadata_json: Option<&str>,
    content_json: &str,
) -> Option<String> {
    if let Some(summary) = summary_hint
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Some(summary.to_string());
    }
    for source in [metadata_json, Some(content_json)].into_iter().flatten() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(source) else {
            continue;
        };
        if let Some(summary) = value
            .get("summary")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return Some(summary.to_string());
        }
        if let Some(preview) = value
            .get("preview_text")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return Some(preview.chars().take(240).collect());
        }
    }
    None
}

pub fn artifact_id_from_raw_ref(raw_ref: &str) -> Option<String> {
    let rest = raw_ref.strip_prefix("artifact://")?;
    let path = rest.split('@').next().unwrap_or(rest);
    path.trim_matches('/')
        .split('/')
        .rfind(|part| !part.is_empty())
        .map(ToString::to_string)
}

fn referenced_artifact_id_from_manifest_item(item: &ContextManifestItemWrite) -> Option<String> {
    if item.source_table == "session_artifacts" {
        return Some(item.source_id.clone());
    }
    item.raw_ref.as_deref().and_then(artifact_id_from_raw_ref)
}

fn aggregate_artifact_references(
    items: &[ContextManifestItemWrite],
) -> BTreeMap<(String, String), i64> {
    let mut references = BTreeMap::new();
    for item in items {
        let Some(artifact_id) = referenced_artifact_id_from_manifest_item(item) else {
            continue;
        };
        *references
            .entry((item.session_id.clone(), artifact_id))
            .or_insert(0) += 1;
    }
    references
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retrieval_stage_metadata_is_stable() {
        for (stage, timeout, event, next) in [
            (
                RetrievalStage::Structured,
                50,
                "retrieval.structured_stale",
                Some(RetrievalStage::Fts),
            ),
            (
                RetrievalStage::Fts,
                200,
                "retrieval.fts_stale",
                Some(RetrievalStage::Vector),
            ),
            (RetrievalStage::Vector, 500, "retrieval.vector_stale", None),
        ] {
            assert_eq!(stage.timeout_ms(), timeout);
            assert_eq!(stage.event_type("stale"), event);
            assert_eq!(stage.next_stage(), next);
        }
    }

    #[test]
    fn projection_budget_preserves_normal_and_benchmark_caps() {
        let standard = BudgetV1_8k::standard();
        assert_eq!(
            (standard.anchor, standard.plan_todo, standard.recent_tail),
            (200, 400, 2000)
        );
        assert_eq!(
            (standard.summary, standard.retrieved, standard.tool_previews),
            (500, 1000, 500)
        );
        assert_eq!(standard.system_tool_schemas, 3400);
        assert_eq!(
            (standard.reserved_output, standard.safety_buffer),
            (500, 200)
        );
        for intent in [None, Some("normal"), Some("unknown")] {
            let normal = budget_for_turn_intent(intent);
            assert_eq!(normal.budget, standard);
            assert!(!normal.flex_applied);
            assert_eq!(normal.borrowed_from_recent_tail, 0);
        }
        let benchmark = budget_for_turn_intent(Some(TURN_INTENT_BENCHMARK_COMPARISON));
        assert!(benchmark.flex_applied);
        assert_eq!(
            benchmark.budget.tool_previews,
            BENCHMARK_TOOL_PREVIEW_BUDGET
        );
        assert_eq!(benchmark.budget.recent_tail, RECENT_TAIL_BENCHMARK_FLOOR);
        assert_eq!(
            benchmark.borrowed_from_recent_tail,
            standard.recent_tail - benchmark.budget.recent_tail
        );
        for budget in [standard, benchmark.budget] {
            assert_eq!(budget.prompt_cap(), BUDGET_V1_8K_PROMPT_CAP);
            assert_eq!(budget.input_context_cap(), BUDGET_V1_8K_TOTAL_CAP);
        }
    }

    #[test]
    fn context_manifest_session_admission_only_reclassifies_row_not_found() {
        let inactive = context_manifest_session_admission_error(
            sqlx::Error::RowNotFound,
            "user-1",
            "session-1",
            "manifest-1",
        );
        assert!(matches!(
            inactive,
            ContextManifestError::SessionNotActive { .. }
        ));

        let database = context_manifest_session_admission_error(
            sqlx::Error::Protocol("database connection lost".to_string()),
            "user-1",
            "session-1",
            "manifest-1",
        );
        assert!(matches!(
            database,
            ContextManifestError::Database {
                operation: "admit_context_manifest_session",
                ..
            }
        ));
    }

    #[derive(Clone)]
    struct FakeContextManifestRow {
        failed_column: Option<&'static str>,
        metadata: Option<&'static str>,
    }

    impl FakeContextManifestRow {
        fn complete() -> Self {
            Self {
                failed_column: None,
                metadata: Some(r#"{"summary":"metadata summary"}"#),
            }
        }

        fn fail_on(column: &'static str) -> Self {
            Self {
                failed_column: Some(column),
                ..Self::complete()
            }
        }

        fn with_metadata(metadata: Option<&'static str>) -> Self {
            Self {
                metadata,
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

    impl ContextManifestDbRow for FakeContextManifestRow {
        fn string_column(&self, column: &str) -> Result<String, sqlx::Error> {
            self.fail_if_needed(column)?;
            Ok(match column {
                "status" => "active",
                "content_json" => r#"{"summary":"content summary","body":"payload"}"#,
                _ => return Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
            .to_string())
        }

        fn optional_string_column(&self, column: &str) -> Result<Option<String>, sqlx::Error> {
            self.fail_if_needed(column)?;
            Ok(match column {
                "metadata" => self.metadata.map(ToString::to_string),
                _ => return Err(sqlx::Error::ColumnNotFound(column.to_string())),
            })
        }
    }

    fn assert_context_manifest_db_error_mentions(
        result: Result<impl std::fmt::Debug, ContextManifestError>,
        needle: &str,
    ) {
        let error = result.expect_err("decode should fail");
        match error {
            ContextManifestError::Database { entity, source, .. } => {
                assert!(
                    entity.contains(needle) || source.to_string().contains(needle),
                    "error should identify `{needle}`, got entity={entity}, source={source}"
                );
            }
            other => panic!("expected database decode error, got {other:?}"),
        }
    }

    #[test]
    fn session_artifact_status_helpers_treat_expired_as_non_downloadable() {
        assert_eq!(
            session_artifact_status_kind(SESSION_ARTIFACT_STATUS_EXPIRED),
            SessionArtifactStatusKind::Expired
        );
        assert!(!session_artifact_raw_payload_is_available(
            SESSION_ARTIFACT_STATUS_EXPIRED
        ));
        assert!(session_artifact_raw_payload_is_available("active"));
    }

    #[test]
    fn expired_artifact_placeholder_preserves_summary_hint() {
        let rendered =
            expired_artifact_placeholder("artifact-1", Some("important preserved summary"));
        assert!(rendered.contains("historical, raw no longer available"));
        assert!(rendered.contains("important preserved summary"));
    }

    #[test]
    fn session_artifact_manifest_row_decode_preserves_values_and_fails_loudly() {
        let row =
            decode_session_artifact_manifest_row(&FakeContextManifestRow::complete(), "artifact-1")
                .expect("artifact manifest row decodes");
        assert_eq!(row.status, "active");
        assert_eq!(
            row.metadata.as_deref(),
            Some(r#"{"summary":"metadata summary"}"#)
        );
        assert_eq!(
            artifact_summary_for_placeholder(None, row.metadata.as_deref(), &row.content_json)
                .as_deref(),
            Some("metadata summary")
        );

        let row = decode_session_artifact_manifest_row(
            &FakeContextManifestRow::with_metadata(None),
            "artifact-1",
        )
        .expect("NULL metadata is valid");
        assert_eq!(row.metadata, None);
        assert_eq!(
            artifact_summary_for_placeholder(None, row.metadata.as_deref(), &row.content_json)
                .as_deref(),
            Some("content summary")
        );

        for column in ["status", "content_json", "metadata"] {
            assert_context_manifest_db_error_mentions(
                decode_session_artifact_manifest_row(
                    &FakeContextManifestRow::fail_on(column),
                    "artifact-1",
                ),
                column,
            );
        }
    }

    fn manifest_item_for_reference(
        session_id: &str,
        item_order: i32,
        source_table: &str,
        source_id: &str,
        included: bool,
    ) -> ContextManifestItemWrite {
        ContextManifestItemWrite {
            session_id: session_id.to_string(),
            item_order,
            zone: "tool_previews".to_string(),
            source_table: source_table.to_string(),
            source_id: source_id.to_string(),
            source_hash: None,
            included,
            token_estimate: 10,
            budget_tokens: 20,
            reason: "test".to_string(),
            render_mode: "reference_only".to_string(),
            raw_ref: None,
        }
    }

    #[test]
    fn artifact_reference_aggregation_preserves_multiplicity_and_scope() {
        let items = vec![
            manifest_item_for_reference("source-a", 0, "session_artifacts", "artifact-1", true),
            manifest_item_for_reference("source-a", 1, "session_artifacts", "artifact-1", false),
            manifest_item_for_reference("source-b", 2, "session_artifacts", "artifact-1", true),
            manifest_item_for_reference("source-a", 3, "runtime_messages", "message-1", true),
        ];

        let references = aggregate_artifact_references(&items);

        assert_eq!(
            references.get(&("source-a".to_string(), "artifact-1".to_string())),
            Some(&2)
        );
        assert_eq!(
            references.get(&("source-b".to_string(), "artifact-1".to_string())),
            Some(&1)
        );
        assert_eq!(references.len(), 2);
    }
}
