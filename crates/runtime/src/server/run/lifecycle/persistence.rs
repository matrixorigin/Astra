//! Post-loop persistence: core events, trace events, tool events, hook DB
//! writes, observer, runtime promotions, and session transcript management.
//!
//! Extracted from [`super`] to keep the lifecycle module manageable.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use sqlx::Row;
use tracing;
use uuid::Uuid;

use astra_core::{ErrorResponse, SharedPool, connect_matrixone};
use astra_services::coordination::AgentProfile;
use astra_services::session_audit::{RUNTIME_PROMOTION_EVENT_TYPE, RuntimePromotionEventData};
use astra_services::skills::SkillService;
use astra_services::{
    DatabaseContextManifestStore, DatabaseStateProjectionStore, RetrievalStage, StateItemUpsert,
};
use astra_services::{EdgeContext, LlmTokenServiceConfig};
use astra_services::{
    WorkspaceCleanupDebtEntry, WorkspaceRecordEntry as StoredWorkspaceRecordEntry,
    WorkspaceRecordStoreError, WorkspaceStateStore,
};
use astra_tools::task_mgmt::SessionTask;
use astra_turn_core::contracts::{
    TurnDecisionAuditRecord, TurnHookDbPersistPlan, TurnHookDbWriter, TurnObserverRequest,
    TurnObserverWorker, TurnSkillSelectionRecord, TurnToolEventPersistPlan, TurnToolEventRecord,
    TurnToolEventWriter,
};
use astra_turn_core::trace_event::{TraceContext, TraceEvent, TraceEventWriter};

use crate::MatrixOneSettings;
use crate::turn::agentic_loop::host::AgenticLoopState;
use crate::turn::token_usage::TokenUsage;
use crate::{
    DatabaseEvaluationService, DatabaseEventService, DatabaseTraceEventWriter,
    EventCreateRequestData, EventService,
};
use astra_services::db_row::{RowDecoder, RowExt};

use super::{
    build_runtime_event_service, build_runtime_turn_evaluation_event, flush_turn_observability,
    persist_runtime_promotion_events, persist_turn_evaluation_journal,
};

#[derive(Clone, Debug, PartialEq, Eq)]
struct TranscriptPageItemRow {
    item_seq: i64,
    role: String,
    content_hash: String,
}

const DEFAULT_TURN_OBSERVER_ASYNC_CONCURRENCY: usize = 4;
const METRIC_TURN_OBSERVER_DISPATCHES_TOTAL: &str = "astra_turn_observer_dispatches_total";
const METRIC_TURN_OBSERVER_RUNS_TOTAL: &str = "astra_turn_observer_runs_total";
static TURN_OBSERVER_ASYNC_IN_FLIGHT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

struct TurnObserverAsyncPermit;

impl Drop for TurnObserverAsyncPermit {
    fn drop(&mut self) {
        TURN_OBSERVER_ASYNC_IN_FLIGHT.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

fn try_acquire_turn_observer_async_permit(limit: usize) -> Option<TurnObserverAsyncPermit> {
    if limit == 0 {
        return None;
    }

    let mut current = TURN_OBSERVER_ASYNC_IN_FLIGHT.load(std::sync::atomic::Ordering::Acquire);
    loop {
        if current >= limit {
            return None;
        }
        match TURN_OBSERVER_ASYNC_IN_FLIGHT.compare_exchange_weak(
            current,
            current + 1,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        ) {
            Ok(_) => return Some(TurnObserverAsyncPermit),
            Err(observed) => current = observed,
        }
    }
}

fn register_turn_observer_metrics(registry: &astra_turn_core::pipeline_metrics::MetricsRegistry) {
    registry.register_counter(
        METRIC_TURN_OBSERVER_DISPATCHES_TOTAL,
        "Server-loop turn observer dispatches by mode and low-cardinality outcome.",
    );
    registry.register_counter(
        METRIC_TURN_OBSERVER_RUNS_TOTAL,
        "Server-loop turn observer worker runs by mode and low-cardinality outcome.",
    );
}

fn record_turn_observer_dispatch_metrics(
    registry: Option<&Arc<astra_turn_core::pipeline_metrics::MetricsRegistry>>,
    mode: &'static str,
    outcome: &'static str,
) {
    let Some(registry) = registry else {
        return;
    };
    register_turn_observer_metrics(registry);
    registry.increment_counter(
        METRIC_TURN_OBSERVER_DISPATCHES_TOTAL,
        &[("mode", mode), ("outcome", outcome)],
        1,
    );
}

fn record_turn_observer_run_metrics(
    registry: Option<&Arc<astra_turn_core::pipeline_metrics::MetricsRegistry>>,
    mode: &'static str,
    outcome: &'static str,
) {
    let Some(registry) = registry else {
        return;
    };
    register_turn_observer_metrics(registry);
    registry.increment_counter(
        METRIC_TURN_OBSERVER_RUNS_TOTAL,
        &[("mode", mode), ("outcome", outcome)],
        1,
    );
}

fn lifecycle_token_usage_json(
    input_tokens: u64,
    cached_input_tokens: u64,
    cache_creation_tokens: u64,
    output_tokens: u64,
) -> Option<serde_json::Value> {
    let usage = TokenUsage {
        input_tokens,
        cached_input_tokens,
        cache_creation_tokens,
        output_tokens,
    };
    if usage.is_empty() {
        return None;
    }

    let billable_input = input_tokens
        .saturating_add(cached_input_tokens)
        .saturating_add(cache_creation_tokens);
    let total_tokens = usage.total_tokens();
    let cache_hit_ratio = if billable_input == 0 {
        0.0
    } else {
        cached_input_tokens as f64 / billable_input as f64
    };
    let mut usage_json = usage.to_json_map();
    usage_json.insert("prompt".into(), Value::from(billable_input));
    usage_json.insert("completion".into(), Value::from(output_tokens));
    usage_json.insert("cache_read".into(), Value::from(cached_input_tokens));
    usage_json.insert("cache_write".into(), Value::from(cache_creation_tokens));
    usage_json.insert("raw_prompt_tokens".into(), Value::from(billable_input));
    usage_json.insert("uncached_input_tokens".into(), Value::from(input_tokens));
    usage_json.insert("effective_input_tokens".into(), Value::from(input_tokens));
    usage_json.insert(
        "prompt_cache_hit_ratio".into(),
        Value::from(cache_hit_ratio),
    );
    usage_json.insert("total".into(), Value::from(total_tokens));
    Some(Value::Object(usage_json))
}

fn decode_post_compaction_manifest_count(row: &impl RowExt) -> Result<i64, String> {
    RowDecoder::new(row, "post-compaction context manifest count").non_negative_i64("count")
}

fn decode_transcript_page_item_row(row: &impl RowExt) -> Result<TranscriptPageItemRow, String> {
    let dec = RowDecoder::new(row, "transcript page item row");
    Ok(TranscriptPageItemRow {
        item_seq: dec.positive_i64("item_seq")?,
        role: dec.non_empty_string("role")?,
        content_hash: dec.non_empty_string("content_hash")?,
    })
}

/// Bundles all handles needed by post-loop best-effort persistence calls.
///
/// Both `create_run` and `stream_chat` run the same set of side effects after
/// the agentic loop finishes: core event persistence, tool event persistence,
/// hook DB writes, Memoria observer, pipeline learning, session-end hooks,
/// runtime promotion events, and learning-stack save.  This struct captures
/// the shared state so both paths can call `run()` instead of duplicating
/// ~60 lines of glue code.
pub(crate) struct PostLoopPersistContext {
    pub(crate) matrixone: MatrixOneSettings,
    pub(crate) shared_pool: Option<SharedPool>,
    pub(crate) user_id: String,
    pub(crate) session_id: String,
    pub(crate) run_id: String,
    pub(crate) agent_id: Option<String>,
    pub(crate) model_name: Option<String>,
    pub(crate) user_message: String,
    pub(crate) hook_db_writer: Option<Arc<dyn TurnHookDbWriter>>,
    pub(crate) observer_worker: Option<Arc<dyn TurnObserverWorker>>,
    pub(crate) tool_event_writer: Option<Arc<dyn TurnToolEventWriter>>,
    pub(crate) metrics_registry: Option<Arc<astra_turn_core::pipeline_metrics::MetricsRegistry>>,
    pub(crate) csl_manager:
        Option<tokio::sync::Mutex<astra_turn_core::conversation_log::manager::CslManager>>,
}

impl PostLoopPersistContext {
    /// Run all best-effort post-loop persistence side effects.
    ///
    /// The `loop_success` flag comes from `outcome.is_ok()` (before consuming
    /// the outcome in `finalize_run_events`).
    pub(crate) async fn run(
        &self,
        state: &AgenticLoopState,
        loop_success: bool,
    ) -> Result<(), String> {
        let mut errors = Vec::new();

        // 0. Persist core events + trace detail events in a single MatrixOne
        // transaction FIRST, so that a crash between writes leaves a consistent
        // state. If core+trace fails, CSL is never written — preserving the
        // invariant that CSL never advances beyond the canonical core events.
        // If core+trace succeeds and CSL later fails, the next restore falls
        // back to transcript messages, which is a recoverable degradation.
        let core_trace_persisted = match self.persist_core_and_trace_in_transaction(state).await {
            Ok(()) => true,
            Err(e) => {
                errors.push(format!("core+trace transaction failed: {}", e));
                false
            }
        };

        // 1. Persist CSL via CslManager only after core+trace persistence
        // succeeds. If CSL fails later, restore can fall back to transcript
        // messages; if core+trace failed, advancing CSL would create history
        // without canonical durable events behind it.
        self.persist_csl_if_core_trace_persisted(state, core_trace_persisted, &mut errors)
            .await;

        // 2. Persist audit-facing tool_call events for session_audit metrics.
        if let Some(ref writer) = self.tool_event_writer {
            if let Err(e) = persist_server_loop_tool_events(
                writer.as_ref(),
                &self.user_id,
                &self.session_id,
                self.agent_id.as_deref(),
                state,
            )
            .await
            {
                errors.push(format!("tool events persist failed: {}", e));
            }
        }

        // 3. Persist decision audit + skill selection to hook DB.
        if let Some(ref writer) = self.hook_db_writer {
            if let Err(e) = persist_server_loop_hook_events(
                writer.as_ref(),
                &self.user_id,
                &self.session_id,
                &self.user_message,
                state,
                self.model_name.as_deref(),
            )
            .await
            {
                errors.push(format!("hook events persist failed: {}", e));
            }
        }

        // 4. Fire Memoria observer (cross-session knowledge extraction).
        if let Some(worker) = self.observer_worker.clone() {
            if let Err(e) = fire_server_loop_observer(
                worker,
                &self.user_id,
                &self.session_id,
                state,
                self.metrics_registry.clone(),
            )
            .await
            {
                errors.push(format!("observer fire failed: {}", e));
            }
        }

        // 5. Fire SessionEnd hooks.
        crate::skills::hooks::fire_session_end(
            &state.skills.session_event_hooks,
            state.current_session_id.as_deref().unwrap_or(""),
        )
        .await;

        // 6. Persist runtime promotion events.
        if let Err(e) = persist_runtime_promotion_events(
            &self.matrixone,
            self.shared_pool.as_ref(),
            &self.user_id,
            &self.session_id,
            &self.run_id,
            &state.telemetry.promotion_events,
        )
        .await
        {
            errors.push(format!("promotion events persist failed: {}", e));
        }

        // 7. Persist web-agent state projection rows generated by the agentic loop.
        if let Err(e) = persist_server_loop_projection_state(
            self.shared_pool.as_ref(),
            &self.user_id,
            &self.session_id,
            &self.run_id,
            self.agent_id.as_deref(),
            self.model_name.as_deref(),
            state,
        )
        .await
        {
            errors.push(format!("projection state persist failed: {}", e));
        }

        // Use loop_success to conditionally log severity
        if loop_success && !errors.is_empty() {
            tracing::warn!(
                session_id = %self.session_id,
                run_id = %self.run_id,
                error_count = errors.len(),
                "post-loop persistence completed with errors"
            );
        } else if !loop_success && !errors.is_empty() {
            tracing::error!(
                session_id = %self.session_id,
                run_id = %self.run_id,
                error_count = errors.len(),
                "post-loop persistence failed on failed run"
            );
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    async fn persist_csl_if_core_trace_persisted(
        &self,
        state: &AgenticLoopState,
        core_trace_persisted: bool,
        errors: &mut Vec<String>,
    ) {
        let Some(ref mgr) = self.csl_manager else {
            return;
        };
        if !core_trace_persisted {
            tracing::warn!(
                session_id = %self.session_id,
                run_id = %self.run_id,
                "skipping CSL persist because core+trace persistence failed"
            );
            return;
        }

        let mut mgr = mgr.lock().await;
        let session_state = extract_session_state_compact(state);
        let messages = messages_for_csl_persist(state);
        if let Err(e) = mgr
            .persist_turn(state.session_turn, &messages, &session_state)
            .await
        {
            let msg = format!("CSL persist failed: {}", e);
            tracing::warn!(
                session_id = %self.session_id,
                error = %e,
                "CSL persist failed"
            );
            errors.push(msg);
        }
    }

    /// Persist core events and trace detail events in a single MatrixOne
    /// transaction. If the transaction fails, all writes are rolled back
    /// atomically — preventing partial state on crash.
    async fn persist_core_and_trace_in_transaction(
        &self,
        state: &AgenticLoopState,
    ) -> Result<(), String> {
        let Some(pool) = self.shared_pool.as_ref() else {
            // Without a pool, fall back to individual non-transactional calls.
            persist_server_loop_core_events(
                &self.matrixone,
                None,
                &self.user_id,
                &self.session_id,
                &self.run_id,
                None,
                self.agent_id.as_deref(),
                None,
                None,
                &self.user_message,
                state,
                self.model_name.as_deref(),
            )
            .await;
            persist_server_loop_trace_events(
                &self.matrixone,
                None,
                &self.user_id,
                &self.session_id,
                &self.run_id,
                None,
                self.agent_id.as_deref(),
                None,
                None,
                state,
                self.model_name.as_deref(),
            )
            .await;
            return Ok(());
        };

        let mut tx = match pool.get().begin().await {
            Ok(tx) => tx,
            Err(error) => {
                let msg = format!("failed to begin MO transaction: {}", error);
                tracing::warn!(
                    session_id = %self.session_id,
                    error = %error,
                    "post-loop: failed to begin MO transaction, falling back to non-transactional"
                );
                persist_server_loop_core_events(
                    &self.matrixone,
                    Some(pool),
                    &self.user_id,
                    &self.session_id,
                    &self.run_id,
                    None,
                    self.agent_id.as_deref(),
                    None,
                    None,
                    &self.user_message,
                    state,
                    self.model_name.as_deref(),
                )
                .await;
                persist_server_loop_trace_events(
                    &self.matrixone,
                    Some(pool),
                    &self.user_id,
                    &self.session_id,
                    &self.run_id,
                    None,
                    self.agent_id.as_deref(),
                    None,
                    None,
                    state,
                    self.model_name.as_deref(),
                )
                .await;
                let _ = persist_server_loop_transcript_items(
                    Some(pool),
                    &self.user_id,
                    &self.session_id,
                    &self.run_id,
                    None,
                    &self.user_message,
                    state,
                    false,
                )
                .await;
                return Err(msg);
            }
        };

        // Core events (user_query + llm_response) + transcript items.
        //
        // `persist_server_loop_core_events_in_tx` now returns `Result`; on Err the
        // transaction is poisoned (partial writes may be staged) and we MUST
        // rollback instead of continuing to write detail events into the same tx.
        match persist_server_loop_core_events_in_tx(
            &mut tx,
            &self.user_id,
            &self.session_id,
            &self.run_id,
            None,
            self.agent_id.as_deref(),
            None,
            None,
            &self.user_message,
            state,
            self.model_name.as_deref(),
        )
        .await
        {
            Ok(()) => {}
            Err(error) => {
                let msg = format!("core events tx failed: {}", error);
                tracing::warn!(
                    session_id = %self.session_id,
                    error = %error,
                    "post-loop: core events tx failed, rolling back MO transaction"
                );
                // rollback consumes the transaction; cannot use tx after this
                if let Err(rollback_err) = tx.rollback().await {
                    tracing::error!(
                        session_id = %self.session_id,
                        error = %rollback_err,
                        "post-loop: rollback also failed after core events tx error"
                    );
                }
                return Err(msg);
            }
        }

        // Trace detail events (LLM rounds, tool calls).
        if let Err(error) = persist_server_loop_trace_events_in_tx(
            &mut tx,
            &self.user_id,
            &self.session_id,
            &self.run_id,
            None,
            self.agent_id.as_deref(),
            None,
            None,
            state,
            self.model_name.as_deref(),
        )
        .await
        {
            let msg = format!("detail events tx failed: {}", error);
            tracing::warn!(
                session_id = %self.session_id,
                error = %error,
                "post-loop: detail events tx failed, rolling back MO transaction"
            );
            if let Err(rb_err) = tx.rollback().await {
                tracing::error!(
                    session_id = %self.session_id,
                    error = %rb_err,
                    "post-loop: rollback failed after detail events tx failure"
                );
            }
            return Err(msg);
        }

        // The transcript gets one ordered durable sequence in this same
        // transaction. The terminal assistant item is committed after durable
        // run evidence, so approval/coordination boundaries remain before the
        // answer without reader-side reordering.
        if let Err(error) = persist_server_loop_transcript_items_in_tx(
            &mut tx,
            &self.user_id,
            &self.session_id,
            &self.run_id,
            None,
            &self.user_message,
            state,
            false,
        )
        .await
        {
            let msg = format!("transcript items tx failed: {error}");
            tracing::warn!(
                session_id = %self.session_id,
                error = %error,
                "post-loop: transcript item persistence failed, rolling back MO transaction"
            );
            if let Err(rb_err) = tx.rollback().await {
                tracing::error!(
                    session_id = %self.session_id,
                    error = %rb_err,
                    "post-loop: rollback failed after transcript item tx failure"
                );
            }
            return Err(msg);
        }

        // Best-effort commit: on failure, rollback naturally drops the tx.
        if let Err(error) = tx.commit().await {
            let msg = format!("MO transaction commit failed: {}", error);
            tracing::warn!(
                session_id = %self.session_id,
                error = %error,
                "post-loop: MO transaction commit failed, writes rolled back"
            );
            return Err(msg);
        }
        Ok(())
    }

    pub(crate) async fn materialize_run_transcript_evidence(
        &self,
        state: &AgenticLoopState,
    ) -> Result<(), String> {
        let Some(pool) = self.shared_pool.as_ref() else {
            return Ok(());
        };
        let terminal_assistant = terminal_assistant_transcript_item(
            &self.user_id,
            &self.session_id,
            &self.run_id,
            None,
            &self.user_message,
            state,
        );
        materialize_server_run_transcript_evidence(
            pool,
            &self.user_id,
            &self.session_id,
            &self.run_id,
            terminal_assistant,
        )
        .await
    }
}

async fn persist_server_loop_projection_state(
    shared_pool: Option<&SharedPool>,
    user_id: &str,
    session_id: &str,
    run_id: &str,
    agent_id: Option<&str>,
    model_name: Option<&str>,
    state: &AgenticLoopState,
) -> Result<(), String> {
    let Some(pool) = shared_pool else {
        return Ok(());
    };
    let store = DatabaseStateProjectionStore::new(pool.clone());
    let final_text = state.final_text.trim();
    if !final_text.is_empty() {
        let preview = truncate_for_projection(final_text, 480);
        let result = store
            .upsert_state_item(StateItemUpsert {
                item_id: Some(format!(
                    "state-decision-{session_id}-{run_id}-{}",
                    state.session_turn
                )),
                user_id: user_id.to_string(),
                session_id: session_id.to_string(),
                scope: "session".to_string(),
                category: "decision".to_string(),
                item_key: format!("turn:{}:final_response", state.session_turn),
                status: "active".to_string(),
                priority: 50,
                source: "agentic_loop".to_string(),
                provenance_event_id: None,
                run_id: Some(run_id.to_string()),
                title: Some(format!("Turn {} final decision", state.session_turn)),
                summary_text: Some(preview.clone()),
                payload_json: json!({
                    "run_id": run_id,
                    "agent_id": agent_id,
                    "model_name": model_name,
                    "session_turn": state.session_turn,
                    "summary": preview,
                    "source": "server_agentic_loop_final_text",
                }),
                token_estimate: astra_turn_core::section_types::estimate_text_tokens(final_text)
                    .clamp(20, 240),
                mutation: "insert".to_string(),
            })
            .await;
        if let Err(error) = result {
            tracing::warn!(
                target: "astra_runtime::state_projection",
                session_id = %session_id,
                run_id = %run_id,
                error = %error,
                "failed to persist agentic-loop decision projection"
            );
        }
    }

    let post_compaction_count_row = match sqlx::query(
        "SELECT COUNT(*) AS count FROM context_manifests \
         WHERE user_id = ? AND session_id = ? AND run_id = ? AND reason = 'post_compaction'",
    )
    .bind(user_id)
    .bind(session_id)
    .bind(run_id)
    .fetch_one(pool.get())
    .await
    {
        Ok(row) => row,
        Err(error) => {
            let error_msg = format!(
                "failed to inspect post-compaction context manifest count: {}",
                error
            );
            tracing::warn!(
                target: "astra_runtime::state_projection",
                session_id = %session_id,
                run_id = %run_id,
                error = %error,
                "failed to inspect post-compaction context manifest count"
            );
            return Err(error_msg);
        }
    };
    let post_compaction_count =
        match decode_post_compaction_manifest_count(&post_compaction_count_row) {
            Ok(count) => count,
            Err(error) => {
                let error_msg = format!(
                    "failed to decode post-compaction context manifest count: {}",
                    error
                );
                tracing::warn!(
                    target: "astra_runtime::state_projection",
                    session_id = %session_id,
                    run_id = %run_id,
                    error = %error,
                    "failed to decode post-compaction context manifest count"
                );
                return Err(error_msg);
            }
        };
    if post_compaction_count > 0 {
        match store
            .run_compaction_assertions(user_id, session_id, run_id)
            .await
        {
            Ok(results) if results.iter().all(|(_, violations)| *violations == 0) => {
                let result = store
                    .upsert_state_item(StateItemUpsert {
                        item_id: Some(format!("state-summary-{session_id}-{run_id}")),
                        user_id: user_id.to_string(),
                        session_id: session_id.to_string(),
                        scope: "session".to_string(),
                        category: "summary".to_string(),
                        item_key: format!("compaction:{run_id}"),
                        status: "active".to_string(),
                        priority: 40,
                        source: "agentic_loop_compaction".to_string(),
                        provenance_event_id: None,
                        run_id: Some(run_id.to_string()),
                        title: Some("Post-compaction summary".to_string()),
                        summary_text: Some(
                            "Compaction completed with invariant checks passing".to_string(),
                        ),
                        payload_json: json!({
                            "reason": "post_compaction",
                            "invariant_results": results,
                        }),
                        token_estimate: 80,
                        mutation: "insert".to_string(),
                    })
                    .await;
                if let Err(error) = result {
                    let error_msg = format!(
                        "failed to persist post-compaction summary projection: {}",
                        error
                    );
                    tracing::warn!(
                        target: "astra_runtime::state_projection",
                        session_id = %session_id,
                        run_id = %run_id,
                        error = %error,
                        "failed to persist post-compaction summary projection"
                    );
                    return Err(error_msg);
                }
            }
            Ok(results) => {
                let error_msg = format!("post-compaction invariant check failed: {:?}", results);
                tracing::warn!(
                    target: "astra_runtime::state_projection",
                    session_id = %session_id,
                    run_id = %run_id,
                    ?results,
                    "post-compaction invariant check failed after loop"
                );
                return Err(error_msg);
            }
            Err(error) => {
                let error_msg =
                    format!("failed to run post-compaction invariant checks: {}", error);
                tracing::warn!(
                    target: "astra_runtime::state_projection",
                    session_id = %session_id,
                    run_id = %run_id,
                    error = %error,
                    "failed to run post-compaction invariant checks"
                );
                return Err(error_msg);
            }
        }
    }
    Ok(())
}

fn truncate_for_projection(text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for ch in text.chars().take(max_chars) {
        out.push(ch);
    }
    out
}

pub(crate) fn extract_session_state_compact(
    state: &AgenticLoopState,
) -> astra_turn_core::conversation_log::SessionStateCompact {
    astra_turn_core::conversation_log::SessionStateCompact {
        // CSL is conversation materialization, not execution policy. Persisting
        // transient restrictions, approvals, interruptions, budgets, or
        // compaction pressure here makes old materialized state hard-steer later
        // turns. Runtime controls are restored only from explicit runtime
        // checkpoints/interruption contracts.
        blocked_tools: Vec::new(),
        recent_tools: state.recent_tools.clone(),
        activated_deferred_tool_names: Vec::new(),
        approval_overrides: None,
        budget_remaining_tokens: 0,
        budget_remaining_rounds: 0,
        consecutive_ctx_errors: 0,
        interruption: None,
        delegation: None,
        compaction_tracker: None,
    }
}

pub(crate) fn restore_session_state_compact(
    ss: astra_turn_core::conversation_log::SessionStateCompact,
    loop_state: &mut AgenticLoopState,
) {
    if !ss.recent_tools.is_empty() {
        loop_state.recent_tools = ss.recent_tools;
    }
    // Intentionally ignore all runtime-control fields in SessionStateCompact.
    // Older CSL records may contain them, but restoring them here would leak
    // stale pauses, approvals, budget pressure, and compaction failures into a
    // new user turn.
}

pub(crate) fn restore_step_checkpoint_runtime_state(
    restored: astra_pipeline::step_restore::RestoredSession,
    current_date: &str,
    loop_state: &mut AgenticLoopState,
) {
    if restored.cache_restore_report.rejected_unverified_entries > 0 {
        tracing::warn!(
            target: "astra_runtime::recovery",
            rejected_unverified_entries = restored.cache_restore_report.rejected_unverified_entries,
            rejected_context_bound_entries = restored
                .cache_restore_report
                .rejected_context_bound_entries,
            "restored tool results remain audit-only; invocation identity or current freshness is required before reuse"
        );
    }
    loop_state.restricted_tools.extend(restored.blocked_tools);
    if !restored.recent_tools.is_empty() {
        loop_state.recent_tools = restored.recent_tools;
    }
    // Never carry event-derived semantic observations across a process
    // recovery boundary. The local cache and TurnGuard workspace epoch share
    // one agent-turn scope; durable replay is owned by the invocation ledger
    // instead.
    loop_state.idempotency_cache = Default::default();
    loop_state.consecutive_context_window_errors = restored.consecutive_context_window_errors;
    if let Some(compaction_state) = restored.compaction_state.as_ref() {
        loop_state.compaction_effectiveness =
            crate::turn::compaction_replay::CompactionEffectivenessTracker::from_json_lossy(
                compaction_state,
            );
    }
    if restored.pipeline_state.is_some() {
        loop_state.pipeline_session = Some(
            astra_turn_core::pipeline_session_serde::restore_or_new_with_current_date(
                astra_turn_core::pipeline_config::PipelineConfig::default(),
                restored.pipeline_state.as_ref(),
                current_date,
            ),
        );
    }
}

pub(crate) fn format_task_board_resume_hint(tasks: &[SessionTask]) -> Option<String> {
    let open: Vec<&SessionTask> = tasks
        .iter()
        .filter(|task| task.status.is_open_work())
        .collect();
    if open.is_empty() {
        return None;
    }

    let next = open
        .iter()
        .copied()
        .find(|task| task.status.is_in_progress())
        .or_else(|| open.iter().copied().find(|task| task.status.is_pending()))
        .or_else(|| open.first().copied())?;
    let title = next.title.chars().take(120).collect::<String>();
    let more = open.len().saturating_sub(1);
    let more_suffix = if more > 0 {
        format!(" · +{more} more open")
    } else {
        String::new()
    };
    Some(format!(
        "open={} · next=[{}] {}: {}{}",
        open.len(),
        next.status,
        next.id,
        title,
        more_suffix
    ))
}

pub(crate) fn messages_for_csl_persist(state: &AgenticLoopState) -> Vec<Value> {
    let mut messages = state.messages.clone();
    let final_text = state.final_text.trim();
    if !final_text.is_empty() {
        let already_has_final = messages
            .last()
            .and_then(|message| {
                let role = message.get("role")?.as_str()?;
                let content = message.get("content")?.as_str()?;
                Some(role == "assistant" && content.trim() == final_text)
            })
            .unwrap_or(false);
        if !already_has_final {
            messages.push(json!({
                "role": "assistant",
                "content": final_text,
            }));
        }
    }
    messages
}

pub(crate) fn server_loop_causal_chain_id(kind: &str) -> String {
    let chain_id = format!("{kind}:{}", Uuid::now_v7());
    debug_assert!(
        chain_id.len() <= 64,
        "server loop causal_chain_id must fit agent_events VARCHAR(64)"
    );
    chain_id
}

fn trace_hash(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    let digest = hasher.finalize();
    digest[..12]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
}

fn trace_event_id(kind: &str, parts: &[&str]) -> String {
    format!("trace:{kind}:{}", trace_hash(parts))
}

fn server_turn_id(run_id: &str) -> String {
    let prefix: String = run_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(16)
        .collect();
    format!(
        "turn-{}",
        if prefix.is_empty() {
            "unknown"
        } else {
            &prefix
        }
    )
}

pub(crate) fn server_trace_context(
    user_id: &str,
    session_id: &str,
    run_id: &str,
    turn_seq: u32,
) -> TraceContext {
    let turn_id = server_turn_id(run_id);
    TraceContext {
        root_event_id: trace_event_id("user", &[session_id, &turn_id]),
        causal_chain_id: server_loop_causal_chain_id("server-loop"),
        session_id: session_id.to_string(),
        user_id: user_id.to_string(),
        turn_id,
        turn_seq: i64::from(turn_seq.max(1)),
    }
}

pub(crate) fn trace_context_from_subrun_context(
    context: &HashMap<String, Value>,
) -> Option<TraceContext> {
    Some(TraceContext {
        session_id: context.get("trace_session_id")?.as_str()?.to_string(),
        user_id: context.get("trace_user_id")?.as_str()?.to_string(),
        turn_id: context.get("trace_turn_id")?.as_str()?.to_string(),
        turn_seq: context.get("trace_turn_seq")?.as_i64()?,
        causal_chain_id: context.get("trace_causal_chain_id")?.as_str()?.to_string(),
        root_event_id: context.get("trace_root_event_id")?.as_str()?.to_string(),
    })
}

async fn persist_trace_degraded_event(
    writer: &dyn TraceEventWriter,
    trace: &TraceContext,
    run_id: &str,
    agent_id: Option<&str>,
    parent_run_id: Option<&str>,
    parent_agent_id: Option<&str>,
    stage: &str,
    error: &str,
) {
    let mut event = TraceEvent::new(
        trace_event_id("degraded", &[run_id, stage, error]),
        trace.session_id.clone(),
        trace.user_id.clone(),
        "trace_persistence_degraded",
        "trace_health",
    )
    .with_turn_context(trace);
    event.run_id = Some(run_id.to_string());
    event.parent_run_id = parent_run_id.map(ToString::to_string);
    event.agent_id = Some(agent_id.unwrap_or("root-agent").to_string());
    event.parent_agent_id = parent_agent_id.map(ToString::to_string);
    event.parent_event_id = Some(trace.root_event_id.clone());
    event.metadata = json!({
        "stage": stage,
        "error": truncate_for_audit(error, 500),
    });
    if let Err(error) = writer.write(event).await {
        astra_core::agent_error!(
            "server-loop",
            "failed to persist trace_persistence_degraded for session {}: {}",
            trace.session_id,
            error
        );
    }
}

/// Persist `user_query` + `llm_response` core events to `agent_events` after
/// the server-driven agentic loop completes.  This closes the persistence gap
/// where the bridge path (`/chat/turn`) wrote these events but the server loop
/// path (`/chat/stream`) did not, breaking session replay and cloud sync.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn persist_server_loop_core_events(
    matrixone: &MatrixOneSettings,
    shared_pool: Option<&SharedPool>,
    user_id: &str,
    session_id: &str,
    run_id: &str,
    parent_run_id: Option<&str>,
    agent_id: Option<&str>,
    parent_agent_id: Option<&str>,
    trace_context: Option<TraceContext>,
    user_message: &str,
    state: &AgenticLoopState,
    model_name: Option<&str>,
) {
    persist_server_loop_core_events_impl(
        matrixone,
        shared_pool,
        None,
        user_id,
        session_id,
        run_id,
        parent_run_id,
        agent_id,
        parent_agent_id,
        trace_context,
        user_message,
        state,
        model_name,
    )
    .await
    .ok();
}

/// Transactional variant: uses the provided transaction for all writes instead
/// of creating its own. The caller owns commit/rollback.
pub(crate) async fn persist_server_loop_core_events_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    user_id: &str,
    session_id: &str,
    run_id: &str,
    parent_run_id: Option<&str>,
    agent_id: Option<&str>,
    parent_agent_id: Option<&str>,
    trace_context: Option<TraceContext>,
    user_message: &str,
    state: &AgenticLoopState,
    model_name: Option<&str>,
) -> Result<(), String> {
    persist_server_loop_core_events_impl(
        &MatrixOneSettings::default(),
        None,
        Some(tx),
        user_id,
        session_id,
        run_id,
        parent_run_id,
        agent_id,
        parent_agent_id,
        trace_context,
        user_message,
        state,
        model_name,
    )
    .await
}

async fn persist_server_loop_core_events_impl(
    matrixone: &MatrixOneSettings,
    shared_pool: Option<&SharedPool>,
    external_tx: Option<&mut sqlx::Transaction<'_, sqlx::MySql>>,
    user_id: &str,
    session_id: &str,
    run_id: &str,
    parent_run_id: Option<&str>,
    agent_id: Option<&str>,
    parent_agent_id: Option<&str>,
    trace_context: Option<TraceContext>,
    user_message: &str,
    state: &AgenticLoopState,
    model_name: Option<&str>,
) -> Result<(), String> {
    if user_message.is_empty()
        && state.final_text.is_empty()
        && state.user_intents.applied_user_intents().is_empty()
    {
        return Ok(());
    }

    let pool_owned = if external_tx.is_some() {
        None
    } else {
        let Some(p) = shared_pool else {
            tracing::debug!(
                session_id,
                "persistence skipped: shared_pool not configured"
            );
            return Ok(());
        };
        Some(p.clone())
    };
    let pool = pool_owned.as_ref();

    let writer = DatabaseTraceEventWriter::new(matrixone.clone());
    let writer = if let Some(p) = pool {
        writer.with_pool(p.clone())
    } else {
        writer
    };
    let trace = trace_context
        .unwrap_or_else(|| server_trace_context(user_id, session_id, run_id, state.session_turn));

    let user_query_event = if !user_message.is_empty() {
        let mut event = TraceEvent::new(
            trace.root_event_id.clone(),
            session_id,
            user_id,
            "user_query",
            "turn",
        )
        .with_turn_context(&trace);
        event.run_id = Some(run_id.to_string());
        event.parent_run_id = parent_run_id.map(ToString::to_string);
        event.agent_id = Some(agent_id.unwrap_or("root-agent").to_string());
        event.parent_agent_id = parent_agent_id.map(ToString::to_string);
        event.content = Some(user_message.to_string());
        Some(event)
    } else {
        None
    };

    let user_intent_events = state
        .user_intents
        .applied_user_intents()
        .iter()
        .map(|intent| {
            let mut event = TraceEvent::new(
                trace_event_id("user_intent", &[run_id, &intent.intent_id]),
                session_id,
                user_id,
                "user_message",
                "turn",
            )
            .with_turn_context(&trace);
            event.run_id = Some(run_id.to_string());
            event.parent_run_id = parent_run_id.map(ToString::to_string);
            event.agent_id = Some(agent_id.unwrap_or("root-agent").to_string());
            event.parent_agent_id = parent_agent_id.map(ToString::to_string);
            event.content = Some(intent.content.clone());
            event.metadata = serde_json::json!({
                "intent_id": intent.intent_id,
                "delivery": intent.delivery,
                "status": intent.status,
                "event_index": intent.event_index,
            });
            event.parent_event_id = user_query_event
                .as_ref()
                .map(|event| event.event_id.clone());
            event
        })
        .collect::<Vec<_>>();

    let llm_response_event = if !state.final_text.is_empty() {
        let usage = lifecycle_token_usage_json(
            state.total_prompt,
            state.total_cache_read,
            state.total_cache_creation,
            state.total_completion,
        );
        let mut event = TraceEvent::new(
            trace_event_id("response", &[run_id, &trace.turn_id]),
            session_id,
            user_id,
            "llm_response",
            "turn",
        )
        .with_turn_context(&trace);
        event.run_id = Some(run_id.to_string());
        event.parent_run_id = parent_run_id.map(ToString::to_string);
        event.agent_id = Some(agent_id.unwrap_or("root-agent").to_string());
        event.parent_agent_id = parent_agent_id.map(ToString::to_string);
        event.content = Some(state.final_text.clone());
        event.parent_event_id = user_query_event
            .as_ref()
            .map(|event| event.event_id.clone())
            .or_else(|| Some(trace.root_event_id.clone()));
        event.llm_model_used = model_name.map(ToString::to_string);
        event.token_usage = usage;
        Some(event)
    } else {
        None
    };

    let mut events = Vec::with_capacity(2 + user_intent_events.len());
    if let Some(event) = user_query_event.clone() {
        events.push(event);
    }
    events.extend(user_intent_events.iter().cloned());
    if let Some(event) = llm_response_event.clone() {
        events.push(event);
    }

    match external_tx {
        Some(tx) => {
            if let Err(e) = DatabaseTraceEventWriter::write_many_in_tx(tx, events).await {
                astra_core::agent_error!(
                    "server-loop",
                    "failed to persist core events (in tx) for session {session_id}: {e}"
                );
                // Transaction is poisoned; caller must rollback. Do not keep
                // writing transcript items into a dirty transaction.
                return Err(e.to_string());
            }
        }
        None => {
            if let Err(e) = writer.write_many(events).await {
                astra_core::agent_error!(
                    "server-loop",
                    "failed to persist core events for session {session_id}: {e}"
                );
                persist_trace_degraded_event(
                    &writer,
                    &trace,
                    run_id,
                    agent_id,
                    parent_run_id,
                    parent_agent_id,
                    "core_events",
                    &e.to_string(),
                )
                .await;
            }
        }
    }
    Ok(())
}

pub(crate) struct TranscriptPersistItem {
    pub(crate) run_id: String,
    pub(crate) role: &'static str,
    pub(crate) content: String,
    /// Structured transcript-only data. This stays outside prompt-facing
    /// content while preserving tool, reasoning, and evidence identity across
    /// the server/edge boundary.
    pub(crate) payload: Option<TranscriptPersistPayload>,
    pub(crate) source_event_id: String,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct TranscriptPersistPayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) reasoning_status: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) tool_calls: Vec<astra_thin_client::SessionTranscriptToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) tool_result: Option<astra_thin_client::SessionTranscriptToolResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) evidence: Option<astra_turn_types::AgentTranscriptEvidence>,
}

fn transcript_tool_text(full: Option<&String>, preview: Option<&String>) -> String {
    let Some(text) = full.or(preview) else {
        return String::new();
    };
    parse_json_str(Some(text))
        .map(|value| redact_trace_value(&value).to_string())
        .unwrap_or_else(|| truncate_trace_text(text, 2_000))
}

fn transcript_items_from_server_loop(
    user_id: &str,
    session_id: &str,
    run_id: &str,
    trace_context: Option<&TraceContext>,
    user_message: &str,
    state: &AgenticLoopState,
    include_terminal_assistant: bool,
) -> Vec<TranscriptPersistItem> {
    let trace = trace_context
        .cloned()
        .unwrap_or_else(|| server_trace_context(user_id, session_id, run_id, state.session_turn));
    let mut core_items = Vec::new();
    if !user_message.is_empty() {
        core_items.push(TranscriptPersistItem {
            run_id: run_id.to_string(),
            role: "user",
            content: user_message.to_string(),
            payload: None,
            source_event_id: trace.root_event_id.clone(),
        });
    }
    for intent in state.user_intents.applied_user_intents() {
        core_items.push(TranscriptPersistItem {
            run_id: run_id.to_string(),
            role: "user",
            content: intent.content.clone(),
            payload: None,
            source_event_id: trace_event_id("user_intent", &[run_id, &intent.intent_id]),
        });
    }
    let assistant = terminal_assistant_transcript_item(
        user_id,
        session_id,
        run_id,
        Some(&trace),
        user_message,
        state,
    );

    for (index, record) in state.stall.tool_call_records.iter().enumerate() {
        if record.is_synthetic_placeholder() {
            continue;
        }
        let call_id = tool_trace_call_id(run_id, index, record);
        let tool_name = record.name.clone();
        core_items.push(TranscriptPersistItem {
            run_id: run_id.to_string(),
            role: "assistant",
            content: String::new(),
            payload: Some(TranscriptPersistPayload {
                tool_calls: vec![astra_thin_client::SessionTranscriptToolCall {
                    tool_use_id: call_id.clone(),
                    name: tool_name.clone(),
                    arguments: transcript_tool_text(
                        record.args_full.as_ref(),
                        record.args_preview.as_ref(),
                    ),
                }],
                ..Default::default()
            }),
            source_event_id: trace_event_id("tool_start", &[run_id, &call_id]),
        });
        core_items.push(TranscriptPersistItem {
            run_id: run_id.to_string(),
            role: "tool",
            content: transcript_tool_text(
                record.result_full.as_ref(),
                record.result_preview.as_ref(),
            ),
            payload: Some(TranscriptPersistPayload {
                tool_result: Some(astra_thin_client::SessionTranscriptToolResult {
                    tool_use_id: call_id.clone(),
                    name: Some(tool_name),
                    status: Some(if record.ok { "completed" } else { "failed" }.to_string()),
                    duration_ms: Some(record.ms),
                }),
                ..Default::default()
            }),
            source_event_id: trace_event_id(
                if record.ok {
                    "tool_call_completed"
                } else {
                    "tool_call_failed"
                },
                &[run_id, &call_id],
            ),
        });
    }
    if include_terminal_assistant && let Some(assistant) = assistant {
        core_items.push(assistant);
    }
    core_items
}

fn terminal_assistant_transcript_item(
    user_id: &str,
    session_id: &str,
    run_id: &str,
    trace_context: Option<&TraceContext>,
    _user_message: &str,
    state: &AgenticLoopState,
) -> Option<TranscriptPersistItem> {
    if state.final_text.is_empty() {
        return None;
    }
    let trace = trace_context
        .cloned()
        .unwrap_or_else(|| server_trace_context(user_id, session_id, run_id, state.session_turn));
    Some(TranscriptPersistItem {
        run_id: run_id.to_string(),
        role: "assistant",
        content: state.final_text.clone(),
        payload: None,
        source_event_id: trace_event_id("response", &[run_id, &trace.turn_id]),
    })
}

pub(crate) async fn persist_server_loop_transcript_items(
    pool: Option<&SharedPool>,
    user_id: &str,
    session_id: &str,
    run_id: &str,
    trace_context: Option<&TraceContext>,
    user_message: &str,
    state: &AgenticLoopState,
    include_terminal_assistant: bool,
) -> Result<Option<String>, String> {
    let Some(pool) = pool else {
        return Ok(None);
    };
    let items = transcript_items_from_server_loop(
        user_id,
        session_id,
        run_id,
        trace_context,
        user_message,
        state,
        include_terminal_assistant,
    );
    let committed_assistant = items
        .iter()
        .rev()
        .find(|item| item.role == "assistant" && !item.content.trim().is_empty())
        .map(|item| item.source_event_id.clone());
    persist_session_transcript_items(pool, user_id, session_id, &items).await?;
    Ok(committed_assistant)
}

async fn persist_server_loop_transcript_items_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    user_id: &str,
    session_id: &str,
    run_id: &str,
    trace_context: Option<&TraceContext>,
    user_message: &str,
    state: &AgenticLoopState,
    include_terminal_assistant: bool,
) -> Result<(), String> {
    let items = transcript_items_from_server_loop(
        user_id,
        session_id,
        run_id,
        trace_context,
        user_message,
        state,
        include_terminal_assistant,
    );
    persist_session_transcript_items_inner_in_tx(tx, user_id, session_id, &items)
        .await
        .map_err(|error| error.to_string())
}

pub(crate) async fn persist_session_transcript_items(
    pool: &SharedPool,
    user_id: &str,
    session_id: &str,
    items: &[TranscriptPersistItem],
) -> Result<(), String> {
    if items.is_empty() {
        return Ok(());
    }
    let mut tx = match pool.get().begin().await {
        Ok(tx) => tx,
        Err(error) => {
            astra_core::agent_error!(
                "server-loop",
                "failed to begin transaction for transcript items for session {session_id}: {error}"
            );
            return Err(format!("failed to begin transcript transaction: {error}"));
        }
    };
    if let Err(error) =
        persist_session_transcript_items_inner_in_tx(&mut tx, user_id, session_id, items).await
    {
        astra_core::agent_error!(
            "server-loop",
            "failed to persist transcript items for session {session_id}: {error}"
        );
        if let Err(rb_err) = tx.rollback().await {
            astra_core::agent_error!(
                "server-loop",
                "failed to rollback after transcript items failure for session {session_id}: {rb_err}"
            );
        }
        return Err(format!("failed to persist transcript items: {error}"));
    }
    if let Err(error) = tx.commit().await {
        astra_core::agent_error!(
            "server-loop",
            "failed to commit transcript items for session {session_id}: {error}"
        );
        return Err(format!("failed to commit transcript items: {error}"));
    }
    Ok(())
}

#[derive(Clone, Debug, Default)]
struct TranscriptReasoningProjection {
    text: String,
    done: bool,
}

impl TranscriptReasoningProjection {
    fn append_delta(&mut self, delta: &str) {
        if !delta.is_empty() && !self.text.ends_with(delta) {
            self.text.push_str(delta);
        }
    }

    fn is_empty(&self) -> bool {
        self.text.is_empty() && !self.done
    }
}

fn transcript_event_fields(payload: &Value) -> &Value {
    payload
        .get("data")
        .filter(|value| value.is_object())
        .unwrap_or(payload)
}

fn transcript_event_string(payload: &Value, field: &str) -> Option<String> {
    payload
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn apply_reasoning_event_payload(projection: &mut TranscriptReasoningProjection, payload: &Value) {
    let event_type = payload
        .get("event_type")
        .or_else(|| payload.get("type"))
        .and_then(Value::as_str);
    match event_type {
        Some("reasoning_delta" | "thinking_delta" | "reasoning_message_content") => {
            let content = payload
                .get("content")
                .and_then(Value::as_str)
                .or_else(|| payload.pointer("/data/content").and_then(Value::as_str))
                .or_else(|| payload.pointer("/data/chunk").and_then(Value::as_str))
                .or_else(|| payload.pointer("/data/reasoning").and_then(Value::as_str))
                .filter(|content| !content.trim().is_empty());
            if let Some(content) = content {
                projection.append_delta(content);
            }
        }
        Some("reasoning_done" | "thinking_done") => projection.done = true,
        _ => {}
    }
}

fn transcript_evidence_items_from_run_event(
    run_id: &str,
    event_id: &str,
    event_type: &str,
    payload: &Value,
) -> Vec<TranscriptPersistItem> {
    let approval_item = |source_event_id: String, fields: &Value| {
        let request_id = transcript_event_string(fields, "request_id")
            .or_else(|| transcript_event_string(fields, "approval_id"));
        let tool = transcript_event_string(fields, "tool")
            .or_else(|| transcript_event_string(fields, "tool_name"));
        request_id
            .zip(tool)
            .map(|(request_id, tool)| TranscriptPersistItem {
                run_id: run_id.to_string(),
                role: "event",
                content: String::new(),
                payload: Some(TranscriptPersistPayload {
                    evidence: Some(
                        astra_turn_types::AgentTranscriptEvidence::ApprovalRequired {
                            request_id,
                            tool,
                            approval_kind: transcript_event_string(fields, "approval_kind")
                                .unwrap_or_else(|| "standard".to_string()),
                            display_label: transcript_event_string(fields, "display_label"),
                            detail: transcript_event_string(fields, "detail"),
                        },
                    ),
                    ..Default::default()
                }),
                source_event_id,
            })
    };

    match event_type {
        "approval_request" | "approval_required" => {
            approval_item(event_id.to_string(), transcript_event_fields(payload))
                .into_iter()
                .collect()
        }
        "approval_batch_required" => transcript_event_fields(payload)
            .get("requests")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|request| {
                let request_id = transcript_event_string(request, "request_id")
                    .or_else(|| transcript_event_string(request, "approval_id"))?;
                let source_event_id = format!("{event_id}:approval:{}", trace_hash(&[&request_id]));
                approval_item(source_event_id, request)
            })
            .collect(),
        "agent_communication" => {
            let Ok(event) = serde_json::from_value::<astra_turn_types::AgentCommunicationEvent>(
                transcript_event_fields(payload).clone(),
            ) else {
                tracing::warn!(
                    event_id,
                    "skipping malformed agent communication transcript evidence"
                );
                return Vec::new();
            };
            vec![TranscriptPersistItem {
                run_id: run_id.to_string(),
                role: "event",
                content: String::new(),
                payload: Some(TranscriptPersistPayload {
                    evidence: Some(
                        astra_turn_types::AgentTranscriptEvidence::AgentCommunication { event },
                    ),
                    ..Default::default()
                }),
                source_event_id: event_id.to_string(),
            }]
        }
        _ => Vec::new(),
    }
}

async fn update_run_assistant_transcript_reasoning_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    user_id: &str,
    session_id: &str,
    run_id: &str,
    reasoning: &TranscriptReasoningProjection,
) -> Result<(), sqlx::Error> {
    if reasoning.is_empty() {
        return Ok(());
    }
    let row = sqlx::query(
        "SELECT item_seq, role, content, payload_json
         FROM session_transcript_items
         WHERE session_id = ? AND user_id = ? AND run_id = ?
           AND role = 'assistant' AND content <> ''
         ORDER BY item_seq DESC
         LIMIT 1
         FOR UPDATE",
    )
    .bind(session_id)
    .bind(user_id)
    .bind(run_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(row) = row else {
        return Ok(());
    };
    let item_seq = row.try_get::<i64, _>("item_seq")?;
    let role = row.try_get::<String, _>("role")?;
    let content = row.try_get::<String, _>("content")?;
    let payload_json = row.try_get::<Option<String>, _>("payload_json")?;
    let mut payload = payload_json
        .as_deref()
        .map(serde_json::from_str::<TranscriptPersistPayload>)
        .transpose()
        .map_err(|error| {
            sqlx::Error::Protocol(format!(
                "decode stored transcript payload for run {run_id}: {error}"
            ))
        })?
        .unwrap_or_default();
    payload.reasoning = (!reasoning.text.is_empty()).then(|| reasoning.text.clone());
    payload.reasoning_status = payload.reasoning.as_ref().map(|_| {
        if reasoning.done {
            "complete".to_string()
        } else {
            "streaming".to_string()
        }
    });
    let payload_json = serde_json::to_string(&payload).map_err(|error| {
        sqlx::Error::Protocol(format!(
            "serialize transcript reasoning for run {run_id}: {error}"
        ))
    })?;
    sqlx::query(
        "UPDATE session_transcript_items
         SET payload_json = ?, content_hash = ?
         WHERE session_id = ? AND user_id = ? AND item_seq = ?",
    )
    .bind(&payload_json)
    .bind(transcript_content_hash(
        &role,
        &content,
        Some(&payload_json),
    ))
    .bind(session_id)
    .bind(user_id)
    .bind(item_seq)
    .execute(&mut **tx)
    .await?;
    sync_transcript_page_inner(tx, user_id, session_id, transcript_page_seq(item_seq)).await
}

pub(crate) async fn materialize_server_run_transcript_evidence(
    pool: &SharedPool,
    user_id: &str,
    session_id: &str,
    run_id: &str,
    terminal_assistant: Option<TranscriptPersistItem>,
) -> Result<(), String> {
    let rows = sqlx::query(
        "SELECT event_id, event_type, payload_json
         FROM agent_run_events
         WHERE session_id = ? AND user_id = ? AND run_id = ?
           AND event_type IN (
                'reasoning_delta', 'reasoning_message_content', 'reasoning_done',
                'thinking_delta', 'thinking_done',
                'approval_request', 'approval_required', 'approval_batch_required',
                'agent_communication'
           )
         ORDER BY event_idx ASC",
    )
    .bind(session_id)
    .bind(user_id)
    .bind(run_id)
    .fetch_all(pool.get())
    .await
    .map_err(|error| error.to_string())?;
    if rows.is_empty() && terminal_assistant.is_none() {
        return Ok(());
    }

    let mut reasoning = TranscriptReasoningProjection::default();
    let mut items = Vec::new();
    for row in rows {
        let event_id = row
            .try_get::<String, _>("event_id")
            .map_err(|error| error.to_string())?;
        let event_type = row
            .try_get::<String, _>("event_type")
            .map_err(|error| error.to_string())?;
        let payload_json = row
            .try_get::<String, _>("payload_json")
            .map_err(|error| error.to_string())?;
        let payload = match serde_json::from_str::<Value>(&payload_json) {
            Ok(payload) => payload,
            Err(error) => {
                tracing::warn!(event_id, error = %error, "skipping malformed durable transcript event");
                continue;
            }
        };
        apply_reasoning_event_payload(&mut reasoning, &payload);
        items.extend(transcript_evidence_items_from_run_event(
            run_id,
            &event_id,
            &event_type,
            &payload,
        ));
    }
    if let Some(terminal_assistant) = terminal_assistant {
        items.push(terminal_assistant);
    }

    let mut tx = pool
        .get()
        .begin()
        .await
        .map_err(|error| error.to_string())?;
    if let Err(error) =
        persist_session_transcript_items_inner_in_tx(&mut tx, user_id, session_id, &items).await
    {
        return Err(rollback_materialized_transcript_transaction(
            tx,
            "persisting transcript items",
            error,
        )
        .await);
    }
    if let Err(error) = update_run_assistant_transcript_reasoning_in_tx(
        &mut tx, user_id, session_id, run_id, &reasoning,
    )
    .await
    {
        return Err(rollback_materialized_transcript_transaction(
            tx,
            "updating assistant reasoning projection",
            error,
        )
        .await);
    }
    tx.commit().await.map_err(|error| error.to_string())
}

async fn rollback_materialized_transcript_transaction(
    tx: sqlx::Transaction<'_, sqlx::MySql>,
    failed_stage: &str,
    operation_error: sqlx::Error,
) -> String {
    match tx.rollback().await {
        Ok(()) => format!("{failed_stage} failed: {operation_error}"),
        Err(rollback_error) => format!(
            "{failed_stage} failed: {operation_error}; transaction rollback also failed: {rollback_error}"
        ),
    }
}

fn truncate_trace_text(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        let prefix: String = text.chars().take(max_chars).collect();
        format!("{prefix}...")
    }
}

pub(crate) fn redact_trace_value(value: &Value) -> Value {
    const SECRET_KEYS: &[&str] = &[
        "api_key",
        "apikey",
        "authorization",
        "cookie",
        "password",
        "secret",
        "token",
    ];
    match value {
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, value) in map {
                let key_lc = key.to_ascii_lowercase();
                if SECRET_KEYS.iter().any(|needle| key_lc.contains(needle)) {
                    out.insert(key.clone(), Value::String("[REDACTED]".to_string()));
                } else {
                    out.insert(key.clone(), redact_trace_value(value));
                }
            }
            Value::Object(out)
        }
        Value::Array(values) => Value::Array(values.iter().map(redact_trace_value).collect()),
        Value::String(text) if text.chars().count() > 2_000 => {
            Value::String(truncate_trace_text(text, 2_000))
        }
        other => other.clone(),
    }
}

fn parse_json_str(input: Option<&String>) -> Option<Value> {
    input.and_then(|text| serde_json::from_str::<Value>(text).ok())
}

fn redacted_json_preview(value: Option<Value>) -> Option<Value> {
    value.map(|value| redact_trace_value(&value))
}

fn tool_action_from_args(args: Option<&Value>) -> Option<String> {
    args.and_then(|value| value.get("action"))
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

fn child_agent_id_from_tool_result(result: Option<&Value>) -> Option<String> {
    result
        .and_then(|value| value.get("agent_id"))
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

fn child_run_id_from_tool_result(result: Option<&Value>) -> Option<String> {
    result
        .and_then(|value| value.get("run_id").or_else(|| value.get("child_run_id")))
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

fn build_llm_round_trace_events(
    trace: &TraceContext,
    run_id: &str,
    parent_run_id: Option<&str>,
    agent_id: Option<&str>,
    parent_agent_id: Option<&str>,
    model_name: Option<&str>,
    rounds: &[crate::turn::agentic_loop::host::RecentRoundSummary],
) -> Vec<TraceEvent> {
    // Hoist repeated per-round allocations. session_id/user_id in new() are
    // overwritten by with_turn_context, so pass empty strings to skip 2 clones.
    let run_id_owned = run_id.to_string();
    let agent_str = agent_id.unwrap_or("root-agent").to_string();
    let parent_run_str = parent_run_id.map(|s| s.to_string());
    let parent_agent_str = parent_agent_id.map(|s| s.to_string());
    let root_event_id = trace.root_event_id.clone();
    let model_default = model_name.map(|s| s.to_string());

    rounds
        .iter()
        .enumerate()
        .map(|(idx, round)| {
            let round_index = i64::from(round.round);
            let round_key = round_index.to_string();
            let mut event = TraceEvent::new(
                trace_event_id("round_done", &[run_id, &round_key, &trace.turn_id]),
                "",
                "",
                "llm_round_completed",
                "llm_round",
            )
            .with_turn_context(trace);
            event.run_id = Some(run_id_owned.clone());
            event.parent_run_id = parent_run_str.clone();
            event.agent_id = Some(agent_str.clone());
            event.parent_agent_id = parent_agent_str.clone();
            event.round_index = Some(round_index);
            event.llm_model_used = (!round.model.is_empty())
                .then(|| round.model.clone())
                .or_else(|| model_default.clone());
            event.meta_duration_ms = i32::try_from(round.duration_ms).ok();
            event.token_usage = llm_round_token_usage_json(round);
            event.parent_event_id = Some(root_event_id.clone());
            event.metadata = json!({
                "finish_reason": round.finish_reason,
                "tool_calls_returned": round.tool_calls_returned,
                "tool_call_names": round.tool_call_names,
                "round_event_index": idx,
            });
            event
        })
        .collect()
}

fn llm_round_token_usage_json(
    round: &crate::turn::agentic_loop::host::RecentRoundSummary,
) -> Option<serde_json::Value> {
    lifecycle_token_usage_json(
        round.prompt_tokens,
        round.cache_read_tokens,
        round.cache_creation_tokens,
        round.completion_tokens,
    )
}

fn tool_trace_call_id(
    run_id: &str,
    index: usize,
    record: &astra_services::session_journal::ToolCallRecord,
) -> String {
    record.tool_call_id.clone().unwrap_or_else(|| {
        let round = record.round.map(|v| v.to_string()).unwrap_or_default();
        format!(
            "tool-{}",
            trace_hash(&[run_id, &round, &index.to_string(), &record.name])
        )
    })
}

pub(crate) fn build_tool_trace_events(
    trace: &TraceContext,
    run_id: &str,
    parent_run_id: Option<&str>,
    agent_id: Option<&str>,
    parent_agent_id: Option<&str>,
    records: &[astra_services::session_journal::ToolCallRecord],
) -> Vec<TraceEvent> {
    // Hoist repeated per-record allocations. session_id/user_id in new() are
    // overwritten by with_turn_context, so pass empty strings to skip 4 clones
    // per record (2 events x 2 fields).
    let run_id_owned = run_id.to_string();
    let agent_str = agent_id.unwrap_or("root-agent").to_string();
    let parent_run_str = parent_run_id.map(|s| s.to_string());
    let parent_agent_str = parent_agent_id.map(|s| s.to_string());
    let root_event_id = trace.root_event_id.clone();

    let mut events = Vec::with_capacity(records.len().saturating_mul(2));
    for (index, record) in records.iter().enumerate() {
        if record.is_synthetic_placeholder() {
            continue;
        }
        let call_id = tool_trace_call_id(run_id, index, record);
        let args_json = parse_json_str(record.args_full.as_ref());
        let result_json = parse_json_str(record.result_full.as_ref());
        let action = if record.name == "agent" {
            tool_action_from_args(args_json.as_ref())
        } else {
            None
        };
        let child_agent_id = child_agent_id_from_tool_result(result_json.as_ref());
        let child_run_id = child_run_id_from_tool_result(result_json.as_ref());
        let round_index = record.round.map(i64::from);
        let started_at = chrono::Utc::now();
        // Clone call_id once, share for both events.
        let call_id_clone = call_id.clone();
        let tool_name = record.name.clone();

        let mut started = TraceEvent::new(
            trace_event_id("tool_start", &[run_id, &call_id]),
            "",
            "",
            "tool_call_started",
            "tool_call",
        )
        .with_turn_context(trace);
        started.run_id = Some(run_id_owned.clone());
        started.parent_run_id = parent_run_str.clone();
        started.agent_id = Some(agent_str.clone());
        started.parent_agent_id = parent_agent_str.clone();
        started.round_index = round_index;
        started.tool_call_id = Some(call_id_clone);
        started.meta_tool_name = Some(tool_name.clone());
        started.parent_event_id = Some(root_event_id.clone());
        started.created_at = started_at;
        started.metadata = json!({
            "args_preview": record.args_preview,
            "tool_args_json_redacted": redacted_json_preview(args_json.clone()),
            "action": action,
            "start_offset_ms": record.start_offset_ms,
        });
        events.push(started);

        let terminal_type = if record.ok {
            "tool_call_completed"
        } else {
            "tool_call_failed"
        };
        let mut completed = TraceEvent::new(
            trace_event_id(terminal_type, &[run_id, &call_id]),
            "",
            "",
            terminal_type,
            "tool_call",
        )
        .with_turn_context(trace);
        completed.run_id = Some(run_id_owned.clone());
        completed.parent_run_id = parent_run_str.clone();
        completed.agent_id = Some(agent_str.clone());
        completed.parent_agent_id = parent_agent_str.clone();
        completed.round_index = round_index;
        completed.tool_call_id = Some(call_id);
        completed.meta_tool_name = Some(tool_name);
        completed.meta_duration_ms = i32::try_from(record.ms).ok();
        completed.parent_event_id = Some(root_event_id.clone());
        completed.metadata = json!({
            "ok": record.ok,
            "action": action,
            "args_preview": record.args_preview,
            "result_preview": record.result_preview,
            "tool_args_json_redacted": redacted_json_preview(args_json),
            "tool_result_json_redacted": redacted_json_preview(result_json),
            "child_agent_id": child_agent_id,
            "child_run_id": child_run_id,
            "error": record.error,
        });
        events.push(completed);
    }
    events
}

pub(crate) async fn persist_server_loop_trace_events(
    matrixone: &MatrixOneSettings,
    shared_pool: Option<&SharedPool>,
    user_id: &str,
    session_id: &str,
    run_id: &str,
    parent_run_id: Option<&str>,
    agent_id: Option<&str>,
    parent_agent_id: Option<&str>,
    trace_context: Option<TraceContext>,
    state: &AgenticLoopState,
    model_name: Option<&str>,
) {
    persist_server_loop_trace_events_impl(
        matrixone,
        shared_pool,
        None,
        user_id,
        session_id,
        run_id,
        parent_run_id,
        agent_id,
        parent_agent_id,
        trace_context,
        state,
        model_name,
    )
    .await
    .ok();
}

/// Transactional variant: uses the provided transaction for all writes.
/// The caller owns commit/rollback.
pub(crate) async fn persist_server_loop_trace_events_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    user_id: &str,
    session_id: &str,
    run_id: &str,
    parent_run_id: Option<&str>,
    agent_id: Option<&str>,
    parent_agent_id: Option<&str>,
    trace_context: Option<TraceContext>,
    state: &AgenticLoopState,
    model_name: Option<&str>,
) -> Result<(), String> {
    persist_server_loop_trace_events_impl(
        &MatrixOneSettings::default(),
        None,
        Some(tx),
        user_id,
        session_id,
        run_id,
        parent_run_id,
        agent_id,
        parent_agent_id,
        trace_context,
        state,
        model_name,
    )
    .await
}

async fn persist_server_loop_trace_events_impl(
    matrixone: &MatrixOneSettings,
    shared_pool: Option<&SharedPool>,
    external_tx: Option<&mut sqlx::Transaction<'_, sqlx::MySql>>,
    user_id: &str,
    session_id: &str,
    run_id: &str,
    parent_run_id: Option<&str>,
    agent_id: Option<&str>,
    parent_agent_id: Option<&str>,
    trace_context: Option<TraceContext>,
    state: &AgenticLoopState,
    model_name: Option<&str>,
) -> Result<(), String> {
    let pool_owned = if external_tx.is_some() {
        None
    } else {
        let Some(p) = shared_pool else {
            return Ok(());
        };
        Some(p.clone())
    };
    let pool = pool_owned.as_ref();

    let writer = DatabaseTraceEventWriter::new(matrixone.clone());
    let writer = if let Some(p) = pool {
        writer.with_pool(p.clone())
    } else {
        writer
    };
    let trace = trace_context
        .unwrap_or_else(|| server_trace_context(user_id, session_id, run_id, state.session_turn));
    let mut events = build_llm_round_trace_events(
        &trace,
        run_id,
        parent_run_id,
        agent_id,
        parent_agent_id,
        model_name,
        &state.recent_rounds,
    );
    events.extend(build_tool_trace_events(
        &trace,
        run_id,
        parent_run_id,
        agent_id,
        parent_agent_id,
        &state.stall.tool_call_records,
    ));
    if events.is_empty() {
        return Ok(());
    }

    match external_tx {
        Some(tx) => {
            if let Err(e) = DatabaseTraceEventWriter::write_many_in_tx(tx, events).await {
                astra_core::agent_error!(
                    "server-loop",
                    "failed to persist trace detail events (in tx) for session {session_id}: {e}"
                );
                return Err(e.to_string());
            }
        }
        None => {
            if let Err(e) = writer.write_many(events).await {
                astra_core::agent_error!(
                    "server-loop",
                    "failed to persist trace detail events for session {session_id}: {e}"
                );
                persist_trace_degraded_event(
                    &writer,
                    &trace,
                    run_id,
                    agent_id,
                    parent_run_id,
                    parent_agent_id,
                    "detail_events",
                    &e.to_string(),
                )
                .await;
            }
        }
    }
    Ok(())
}

/// Variant that uses an existing transaction instead of creating its own.
/// The caller owns commit/rollback.
pub(crate) async fn persist_session_transcript_items_inner_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    user_id: &str,
    session_id: &str,
    items: &[TranscriptPersistItem],
) -> Result<(), sqlx::Error> {
    let owned_session = sqlx::query(
        "SELECT 1 AS owned
         FROM agent_sessions
         WHERE session_id = ? AND user_id = ?
         LIMIT 1
         FOR UPDATE",
    )
    .bind(session_id)
    .bind(user_id)
    .fetch_optional(&mut **tx)
    .await?;
    if owned_session.is_none() {
        return Err(sqlx::Error::RowNotFound);
    }

    let row = sqlx::query(
        "SELECT COALESCE(MAX(item_seq), 0) + 1 AS next_seq
         FROM session_transcript_items
         WHERE session_id = ? AND user_id = ?",
    )
    .bind(session_id)
    .bind(user_id)
    .fetch_one(&mut **tx)
    .await?;
    let mut next_seq = row.try_get::<i64, _>("next_seq")?;
    let mut dirty_pages = BTreeSet::new();

    for item in items {
        let existing = sqlx::query(
            "SELECT 1 AS existing
             FROM session_transcript_items
             WHERE session_id = ? AND user_id = ? AND source_event_id = ?
             LIMIT 1",
        )
        .bind(session_id)
        .bind(user_id)
        .bind(&item.source_event_id)
        .fetch_optional(&mut **tx)
        .await?;
        if existing.is_some() {
            continue;
        }

        let item_seq = next_seq;
        let payload_json = item
            .payload
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|error| {
                sqlx::Error::Protocol(format!(
                    "serialize transcript payload for {}: {error}",
                    item.source_event_id
                ))
            })?;
        sqlx::query(
            "INSERT INTO session_transcript_items
             (session_id, item_seq, user_id, run_id, role, content, payload_json,
              source_event_id, source_event_idx, content_hash, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, NULL, ?, NOW(6))",
        )
        .bind(session_id)
        .bind(item_seq)
        .bind(user_id)
        .bind(&item.run_id)
        .bind(item.role)
        .bind(&item.content)
        .bind(&payload_json)
        .bind(&item.source_event_id)
        .bind(transcript_content_hash(
            item.role,
            &item.content,
            payload_json.as_deref(),
        ))
        .execute(&mut **tx)
        .await?;
        dirty_pages.insert(transcript_page_seq(item_seq));
        next_seq += 1;
    }

    for page_seq in dirty_pages {
        sync_transcript_page_inner(tx, user_id, session_id, page_seq).await?;
    }

    Ok(())
}

const TRANSCRIPT_PAGE_SIZE: i64 = 50;

pub(crate) fn transcript_page_seq(item_seq: i64) -> i64 {
    ((item_seq.max(1) - 1) / TRANSCRIPT_PAGE_SIZE) + 1
}

pub(crate) fn transcript_page_bounds(page_seq: i64) -> (i64, i64) {
    let page_seq = page_seq.max(1);
    let start = ((page_seq - 1) * TRANSCRIPT_PAGE_SIZE) + 1;
    let end = start + TRANSCRIPT_PAGE_SIZE - 1;
    (start, end)
}

async fn sync_transcript_page_inner(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    user_id: &str,
    session_id: &str,
    page_seq: i64,
) -> Result<(), sqlx::Error> {
    let (start_item_seq, end_item_seq) = transcript_page_bounds(page_seq);
    let rows = sqlx::query(
        "SELECT item_seq, role, content_hash
         FROM session_transcript_items
         WHERE session_id = ? AND user_id = ? AND item_seq BETWEEN ? AND ?
         ORDER BY item_seq ASC",
    )
    .bind(session_id)
    .bind(user_id)
    .bind(start_item_seq)
    .bind(end_item_seq)
    .fetch_all(&mut **tx)
    .await?;
    if rows.is_empty() {
        sqlx::query(
            "DELETE FROM transcript_pages WHERE session_id = ? AND user_id = ? AND page_seq = ?",
        )
        .bind(session_id)
        .bind(user_id)
        .bind(page_seq)
        .execute(&mut **tx)
        .await?;
        return Ok(());
    }

    let page_items = rows
        .iter()
        .map(decode_transcript_page_item_row)
        .collect::<Result<Vec<_>, _>>()
        .map_err(sqlx::Error::Protocol)?;
    let Some(first_page_item) = page_items.first() else {
        return Err(sqlx::Error::Protocol(
            "non-empty transcript page rows decoded into an empty page item set".to_string(),
        ));
    };
    let Some(last_page_item) = page_items.last() else {
        return Err(sqlx::Error::Protocol(
            "non-empty transcript page rows decoded into an empty page item set".to_string(),
        ));
    };
    let first_item_seq = first_page_item.item_seq;
    let last_item_seq = last_page_item.item_seq;
    let mut hasher = Sha256::new();
    for item in &page_items {
        hasher.update(item.item_seq.to_string().as_bytes());
        hasher.update([0]);
        hasher.update(item.role.as_bytes());
        hasher.update([0]);
        hasher.update(item.content_hash.as_bytes());
        hasher.update([0xff]);
    }
    let page_hash = format!("{:x}", hasher.finalize());
    sqlx::query(
        "INSERT INTO transcript_pages
         (user_id, session_id, page_seq, start_item_seq, end_item_seq, item_count, page_hash, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, NOW(6), NOW(6))
         ON DUPLICATE KEY UPDATE
           start_item_seq = VALUES(start_item_seq),
           end_item_seq = VALUES(end_item_seq),
           item_count = VALUES(item_count),
           page_hash = VALUES(page_hash),
           updated_at = NOW(6)",
    )
    .bind(user_id)
    .bind(session_id)
    .bind(page_seq)
    .bind(first_item_seq)
    .bind(last_item_seq)
    .bind(rows.len() as i64)
    .bind(page_hash)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn transcript_content_hash(role: &str, content: &str, payload_json: Option<&str>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(role.as_bytes());
    hasher.update([0]);
    hasher.update(content.as_bytes());
    hasher.update([0]);
    if let Some(payload_json) = payload_json {
        hasher.update(payload_json.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

/// Persist audit-facing `tool_call` events to `agent_events` for tools used
/// during the server-driven agentic loop. The bridge path creates detailed
/// per-call records; here we create one event per unique tool name from
/// `state.telemetry.all_tools_used` with metadata containing `tool_name`
/// so that `session_audit` aggregate queries (`meta_tool_name`,
/// `tool_calls_total`) return correct results for server-loop sessions.
async fn persist_server_loop_tool_events(
    writer: &dyn TurnToolEventWriter,
    user_id: &str,
    session_id: &str,
    agent_id: Option<&str>,
    state: &AgenticLoopState,
) -> Result<(), String> {
    if state.telemetry.all_tools_used.is_empty() {
        return Ok(());
    }

    let chain_id = server_loop_causal_chain_id("server-loop-tools");
    let mut events = Vec::with_capacity(state.telemetry.all_tools_used.len());

    for tool_name in &state.telemetry.all_tools_used {
        events.push(TurnToolEventRecord {
            event_id: Uuid::now_v7().to_string(),
            user_id: user_id.to_string(),
            session_id: session_id.to_string(),
            run_id: state.current_run_id.clone(),
            tool_call_id: None,
            agent_id: agent_id.map(|s| s.to_string()),
            event_type: "tool_call".to_string(),
            content: format!("server-loop tool: {tool_name}"),
            parent_event_id: None,
            parent_event_ids: Vec::new(),
            causal_chain_id: chain_id.clone(),
            metadata: Some(json!({ "tool_name": tool_name })),
            skill_name: None,
            skill_version: None,
            reasoning_content: None,
        });
    }

    let plan = TurnToolEventPersistPlan { events };
    writer
        .persist(plan)
        .await
        .map_err(|e| format!("tool events persist failed: {}", e))
}

/// Persist decision audit + skill selection to hook DB tables after the
/// server-driven agentic loop completes.  This ensures the decisions API
/// (`ctx_decision_audits`, `skill_selection_events`) has data for server-loop
/// sessions, matching what the bridge path persisted via hook side effects.
#[allow(clippy::too_many_arguments)]
async fn persist_server_loop_hook_events(
    hook_db_writer: &dyn TurnHookDbWriter,
    user_id: &str,
    session_id: &str,
    user_message: &str,
    state: &AgenticLoopState,
    model_name: Option<&str>,
) -> Result<(), String> {
    // Use the telemetry accumulator — state.telemetry.all_tools_used tracks every
    // tool name across all rounds.  state.messages does NOT carry assistant
    // tool_call objects in the server loop path.
    let tool_call_names: Vec<String> = state.telemetry.all_tools_used.iter().cloned().collect();
    let selected_skills = state.telemetry.all_selected_skills.clone();
    let event_id = Uuid::now_v7().to_string();

    let decision_audit = Some(TurnDecisionAuditRecord {
        decision_id: Uuid::now_v7().to_string(),
        user_id: user_id.to_string(),
        session_id: session_id.to_string(),
        event_id: event_id.clone(),
        decision_type: if tool_call_names.is_empty() {
            "response_generation".to_string()
        } else {
            "tool_surface".to_string()
        },
        decision_output: json!({
            "text": truncate_for_audit(&state.final_text, 500),
            "tool_calls": tool_call_names,
            "model_used": model_name,
            "total_tool_calls": state.total_tool_calls,
            "total_prompt_tokens": state.provider_input_tokens(),
            "total_completion_tokens": state.total_completion,
        }),
        model_used: model_name.map(|s| s.to_string()),
        context_capture_id: None,
    });

    let skill_selection = if let Some(first_skill) = selected_skills.first() {
        Some(TurnSkillSelectionRecord {
            event_id: Uuid::now_v7().to_string(),
            session_id: session_id.to_string(),
            user_id: user_id.to_string(),
            agent_id: None,
            user_query: truncate_for_audit(user_message, 2000),
            selected_skills: selected_skills.clone(),
            skill_name: first_skill.clone(),
            skill_version: None,
            selection_method: "llm_skill_choice".to_string(),
            execution_success: Some(1),
            execution_time_ms: None,
        })
    } else {
        tool_call_names
            .first()
            .map(|first_tool| TurnSkillSelectionRecord {
                event_id: Uuid::now_v7().to_string(),
                session_id: session_id.to_string(),
                user_id: user_id.to_string(),
                agent_id: None,
                user_query: truncate_for_audit(user_message, 2000),
                selected_skills: tool_call_names.clone(),
                skill_name: first_tool.clone(),
                skill_version: None,
                selection_method: "llm_tool_choice".to_string(),
                execution_success: Some(1),
                execution_time_ms: None,
            })
    };
    let plan = TurnHookDbPersistPlan {
        decision_audit,
        skill_selection,
        reflection_mark: None,
        reflection_lesson: None,
    };

    hook_db_writer
        .persist(plan)
        .await
        .map_err(|e| format!("hook events persist failed: {}", e))
}

/// Fire the Memoria observer after the server-driven loop completes.
/// This sends the conversation messages to the Memoria `/v1/observe` endpoint
/// for cross-session knowledge extraction.
async fn fire_server_loop_observer(
    observer_worker: Arc<dyn TurnObserverWorker>,
    user_id: &str,
    session_id: &str,
    state: &AgenticLoopState,
    metrics_registry: Option<Arc<astra_turn_core::pipeline_metrics::MetricsRegistry>>,
) -> Result<(), String> {
    fire_server_loop_observer_with_async_limit(
        observer_worker,
        user_id,
        session_id,
        state,
        metrics_registry,
        DEFAULT_TURN_OBSERVER_ASYNC_CONCURRENCY,
    )
    .await
}

async fn fire_server_loop_observer_with_async_limit(
    observer_worker: Arc<dyn TurnObserverWorker>,
    user_id: &str,
    session_id: &str,
    state: &AgenticLoopState,
    metrics_registry: Option<Arc<astra_turn_core::pipeline_metrics::MetricsRegistry>>,
    async_concurrency_limit: usize,
) -> Result<(), String> {
    let Some(request) = build_server_loop_observer_request(user_id, session_id, state) else {
        record_turn_observer_dispatch_metrics(metrics_registry.as_ref(), "none", "skipped_empty");
        return Ok(());
    };

    let Some(permit) = try_acquire_turn_observer_async_permit(async_concurrency_limit) else {
        record_turn_observer_dispatch_metrics(metrics_registry.as_ref(), "async", "dropped_full");
        tracing::warn!(
            session_id = %session_id,
            concurrency_limit = async_concurrency_limit,
            "server-loop observer async concurrency full; durable turn was saved but observer evidence was not scheduled"
        );
        return Err(format!(
            "observer capacity exhausted for session {session_id}; evidence requires retry"
        ));
    };
    record_turn_observer_dispatch_metrics(metrics_registry.as_ref(), "async", "scheduled");
    let session_id = session_id.to_string();
    tokio::spawn(async move {
        let _permit = permit;
        run_server_loop_observer_request(
            observer_worker.as_ref(),
            &session_id,
            request,
            "async",
            metrics_registry.as_ref(),
        )
        .await;
    });
    Ok(())
}

fn build_server_loop_observer_request(
    user_id: &str,
    session_id: &str,
    state: &AgenticLoopState,
) -> Option<TurnObserverRequest> {
    let messages: Vec<serde_json::Map<String, serde_json::Value>> = state
        .messages
        .iter()
        .filter_map(|m| m.as_object().cloned())
        .collect();

    if messages.is_empty() {
        return None;
    }

    let turn_count = if state.session_turn > 0 {
        state.session_turn
    } else {
        state.max_turns.saturating_sub(state.remaining_turns) as u32
    } as i64;
    Some(TurnObserverRequest {
        user_id: user_id.to_string(),
        session_id: session_id.to_string(),
        messages,
        turn_count,
        session_start: None,
    })
}

async fn run_server_loop_observer_request(
    observer_worker: &dyn TurnObserverWorker,
    session_id: &str,
    request: TurnObserverRequest,
    mode: &'static str,
    metrics_registry: Option<&Arc<astra_turn_core::pipeline_metrics::MetricsRegistry>>,
) {
    if let Err(e) = observer_worker.run(request).await {
        record_turn_observer_run_metrics(metrics_registry, mode, "error");
        astra_core::agent_error!(
            "server-loop",
            "failed to run observer for session {session_id}: {e}"
        );
    } else {
        record_turn_observer_run_metrics(metrics_registry, mode, "success");
    }
}

/// Truncate text for audit records, preserving UTF-8 boundaries.
fn truncate_for_audit(text: &str, max_chars: usize) -> String {
    if text.len() <= max_chars {
        text.to_string()
    } else {
        let truncated: String = text.chars().take(max_chars).collect();
        format!("{truncated}…")
    }
}

/// Walk `messages` (chronological) and return the content of the latest
/// assistant entry, if any. Kept for tests that exercise implicit-feedback
/// detection against assistant history.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn extract_prev_assistant_text(messages: &[serde_json::Value]) -> Option<String> {
    for msg in messages.iter().rev() {
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
        if role != "assistant" {
            continue;
        }
        if let Some(text) = msg.get("content").and_then(|v| v.as_str()) {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
        if let Some(arr) = msg.get("content").and_then(|v| v.as_array()) {
            let mut buf = String::new();
            for part in arr {
                if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                    if !buf.is_empty() {
                        buf.push('\n');
                    }
                    buf.push_str(t);
                }
            }
            let trimmed = buf.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

pub(crate) fn build_run_turn_complete_event_with_interruption(
    total_tool_calls: u32,
    final_text: &str,
    interruption: Option<&astra_turn_core::interruption::InterruptionRecord>,
    completion_facts: &astra_turn_core::complete::TurnCompletionFacts,
) -> Value {
    let execution_state = interruption.map(|record| {
        serde_json::json!({
            "status": "interrupted",
            "interrupted": true,
            "interruption_kind": record.kind.label(),
            "resume_action": &record.resume_action,
            "resumable": record.kind.is_resumable(),
            "has_checkpoint": record.has_checkpoint,
            "tool_calls_completed": record.tool_calls_completed,
            "turns_completed": record.turns_completed,
            "remaining_turns": record.remaining_turns,
            "error_detail": record.error_detail,
        })
    });
    Value::Object(astra_turn_core::complete::build_turn_complete_event(
        total_tool_calls > 0,
        completion_facts,
        execution_state,
        (!final_text.is_empty()).then_some(final_text),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_turn_core::conversation_log::CslStore;
    use astra_turn_core::conversation_log::file_store::FileCslStore;
    use astra_turn_core::conversation_log::manager::{CslManager, CslManagerConfig};
    use sqlx::Row;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Notify;
    use uuid::Uuid;

    static SHARED_BOOTSTRAP: tokio::sync::OnceCell<astra_core::SharedPool> =
        tokio::sync::OnceCell::const_new();

    async fn setup_pool() -> astra_core::SharedPool {
        assert_eq!(
            std::env::var("ASTRA_TEST_DB_IT").as_deref(),
            Ok("1"),
            "set ASTRA_TEST_DB_IT=1 for ignored integration tests"
        );
        SHARED_BOOTSTRAP
            .get_or_init(|| async {
                let settings = astra_core::MatrixOneSettings::from_env();
                let catalog = std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG")
                    .unwrap_or_else(|_| "mysql".to_string());
                astra_services::ensure_core_schema(&settings, &catalog)
                    .await
                    .expect("ensure_core_schema");
                astra_core::SharedPool::new(&settings)
                    .await
                    .expect("SharedPool::new")
            })
            .await
            .clone()
    }

    #[derive(Default)]
    struct CaptureToolEventWriter {
        plans: std::sync::Mutex<Vec<TurnToolEventPersistPlan>>,
    }

    #[async_trait::async_trait]
    impl TurnToolEventWriter for CaptureToolEventWriter {
        async fn persist(&self, plan: TurnToolEventPersistPlan) -> Result<(), String> {
            self.plans.lock().expect("capture lock").push(plan);
            Ok(())
        }
    }

    #[derive(Default)]
    struct CaptureHookDbWriter {
        plans: std::sync::Mutex<Vec<TurnHookDbPersistPlan>>,
    }

    #[async_trait::async_trait]
    impl TurnHookDbWriter for CaptureHookDbWriter {
        async fn persist(&self, plan: TurnHookDbPersistPlan) -> Result<(), String> {
            self.plans.lock().expect("capture lock").push(plan);
            Ok(())
        }
    }

    fn test_post_loop_persist_context(
        session_id: &str,
        csl_manager: Option<CslManager>,
    ) -> PostLoopPersistContext {
        PostLoopPersistContext {
            matrixone: MatrixOneSettings::default(),
            shared_pool: None,
            user_id: "user-1".to_string(),
            session_id: session_id.to_string(),
            run_id: "run-1".to_string(),
            agent_id: Some("agent-1".to_string()),
            model_name: Some("model-1".to_string()),
            user_message: "work".to_string(),
            hook_db_writer: None,
            observer_worker: None,
            tool_event_writer: None,
            metrics_registry: None,
            csl_manager: csl_manager.map(tokio::sync::Mutex::new),
        }
    }

    struct CaptureObserverWorker {
        calls: AtomicUsize,
        requests: std::sync::Mutex<Vec<TurnObserverRequest>>,
        started: Notify,
        release: Notify,
        block_until_released: bool,
    }

    impl CaptureObserverWorker {
        fn new(block_until_released: bool) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                requests: std::sync::Mutex::new(Vec::new()),
                started: Notify::new(),
                release: Notify::new(),
                block_until_released,
            }
        }

        fn call_count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl TurnObserverWorker for CaptureObserverWorker {
        async fn run(&self, request: TurnObserverRequest) -> Result<(), String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.requests.lock().expect("capture lock").push(request);
            self.started.notify_waiters();
            if self.block_until_released {
                self.release.notified().await;
            }
            Ok(())
        }
    }

    fn observer_test_state() -> AgenticLoopState {
        let mut state = crate::turn::agentic_loop::host::make_test_loop_state();
        state.messages = vec![
            json!({"role": "user", "content": "hello"}),
            json!({"role": "assistant", "content": "world"}),
        ];
        state.session_turn = 3;
        state.max_turns = 12;
        state.remaining_turns = 8;
        state
    }

    async fn wait_for_observer_calls(worker: &CaptureObserverWorker, expected: usize) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(500);
        loop {
            if worker.call_count() >= expected {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "observer call count did not reach {expected}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    async fn wait_for_observer_in_flight(expected: usize) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(500);
        loop {
            let current = TURN_OBSERVER_ASYNC_IN_FLIGHT.load(Ordering::SeqCst);
            if current == expected {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "observer in-flight count stayed at {current}, expected {expected}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    #[serial_test::serial(turn_observer_async)]
    async fn server_loop_observer_async_does_not_block_caller() {
        let observer = Arc::new(CaptureObserverWorker::new(true));
        let started = observer.started.notified();
        let state = observer_test_state();

        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            fire_server_loop_observer_with_async_limit(
                observer.clone(),
                "user-1",
                "session-1",
                &state,
                None,
                DEFAULT_TURN_OBSERVER_ASYNC_CONCURRENCY,
            ),
        )
        .await
        .expect("async observer dispatch should return without waiting for worker")
        .expect("available observer capacity should schedule the worker");

        tokio::time::timeout(std::time::Duration::from_millis(500), started)
            .await
            .expect("spawned observer should start");
        wait_for_observer_calls(observer.as_ref(), 1).await;
        observer.release.notify_waiters();
        wait_for_observer_in_flight(0).await;
    }

    #[tokio::test]
    #[serial_test::serial(turn_observer_async)]
    async fn server_loop_observer_async_limit_reports_saturation_without_blocking() {
        let first = Arc::new(CaptureObserverWorker::new(true));
        let second = Arc::new(CaptureObserverWorker::new(false));
        let first_started = first.started.notified();
        let state = observer_test_state();

        fire_server_loop_observer_with_async_limit(
            first.clone(),
            "user-1",
            "session-1",
            &state,
            None,
            1,
        )
        .await
        .expect("first observer should acquire the only permit");
        tokio::time::timeout(std::time::Duration::from_millis(500), first_started)
            .await
            .expect("first observer should start");
        wait_for_observer_calls(first.as_ref(), 1).await;
        wait_for_observer_in_flight(1).await;

        let saturated = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            fire_server_loop_observer_with_async_limit(
                second.clone(),
                "user-1",
                "session-2",
                &state,
                None,
                1,
            ),
        )
        .await
        .expect("full async observer pool should report without blocking");
        assert!(
            saturated.is_err(),
            "saturation must be visible to the persistence caller"
        );

        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        assert_eq!(second.call_count(), 0);
        first.release.notify_waiters();
        wait_for_observer_in_flight(0).await;
    }

    #[tokio::test]
    #[serial_test::serial(turn_observer_async)]
    async fn server_loop_observer_metrics_stay_low_cardinality() {
        let registry = Arc::new(astra_turn_core::pipeline_metrics::MetricsRegistry::new());
        let observer = Arc::new(CaptureObserverWorker::new(false));
        let state = observer_test_state();

        fire_server_loop_observer_with_async_limit(
            observer.clone(),
            "user-1",
            "session-1",
            &state,
            Some(registry.clone()),
            DEFAULT_TURN_OBSERVER_ASYNC_CONCURRENCY,
        )
        .await
        .expect("available observer capacity should schedule the worker");
        wait_for_observer_calls(observer.as_ref(), 1).await;

        let rendered = registry.render_prometheus();
        assert!(
            rendered.contains(
                "astra_turn_observer_dispatches_total{mode=\"async\",outcome=\"scheduled\"} 1"
            ),
            "{rendered}"
        );
        assert!(
            rendered
                .contains("astra_turn_observer_runs_total{mode=\"async\",outcome=\"success\"} 1"),
            "{rendered}"
        );
        assert!(
            !rendered.contains("user_id=")
                && !rendered.contains("session_id=")
                && !rendered.contains("run_id="),
            "observer metrics must stay low-cardinality: {rendered}"
        );
    }

    #[test]
    fn server_loop_observer_request_uses_session_turn_count() {
        let state = observer_test_state();
        let request = build_server_loop_observer_request("user-1", "session-1", &state)
            .expect("observer request");

        assert_eq!(request.user_id, "user-1");
        assert_eq!(request.session_id, "session-1");
        assert_eq!(request.messages.len(), 2);
        assert_eq!(request.turn_count, 3);
    }

    #[test]
    fn server_loop_observer_turn_count_does_not_use_agent_loop_rounds_when_session_turn_exists() {
        let mut state = observer_test_state();
        state.session_turn = 1;
        state.max_turns = 12;
        state.remaining_turns = 10;
        let request = build_server_loop_observer_request("user-1", "session-1", &state)
            .expect("observer request");

        assert_eq!(
            request.turn_count, 1,
            "observer turn_count is a session/user-turn sequence, not the \
             number of LLM/tool-loop rounds consumed inside this turn",
        );
    }

    #[test]
    fn messages_for_csl_persist_keeps_raw_canonical_history() {
        let mut state = crate::turn::agentic_loop::host::make_test_loop_state();
        state.messages = vec![
            json!({"role": "user", "content": "old review"}),
            json!({"role": "system", "content": "[Context compacted: older messages were removed to reduce token pressure. The conversation continues below.]"}),
            json!({"role": "user", "content": "不要review啊！"}),
            json!({"role": "assistant", "reasoning_content": "trace"}),
            json!({"role": "system", "content": "[Session runtime recap]\nRecent tools: stale"}),
            json!({"role": "tool", "tool_call_id": "c1", "content": "tool output"}),
        ];
        state.final_text = "ok".to_string();

        let messages = messages_for_csl_persist(&state);

        assert_eq!(messages.len(), 7);
        assert_eq!(messages[0]["content"], "old review");
        assert_eq!(messages[2]["content"], "不要review啊！");
        assert_eq!(messages[3]["reasoning_content"], "trace");
        assert_eq!(
            messages[4]["content"],
            "[Session runtime recap]\nRecent tools: stale"
        );
        assert_eq!(messages[5]["role"], "tool");
        assert_eq!(messages[6]["content"], "ok");
    }

    #[tokio::test]
    async fn csl_persist_is_skipped_when_core_trace_persistence_fails() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store: Arc<dyn CslStore> = Arc::new(FileCslStore::new(dir.path()));
        let session_id = "post-loop-csl-gated-on-core";
        let manager = CslManager::new(
            Arc::clone(&store),
            session_id.to_string(),
            CslManagerConfig::default(),
        )
        .expect("csl manager");
        let ctx = test_post_loop_persist_context(session_id, Some(manager));
        let mut state = crate::turn::agentic_loop::host::make_test_loop_state();
        state.session_turn = 3;
        state.messages = vec![json!({"role": "user", "content": "question"})];
        state.final_text = "answer".to_string();
        let mut errors = Vec::new();

        ctx.persist_csl_if_core_trace_persisted(&state, false, &mut errors)
            .await;

        assert!(errors.is_empty());
        let entries = store.load_after(session_id, 0).await.expect("load csl");
        assert!(
            entries.is_empty(),
            "CSL must not advance when core+trace persistence failed: {entries:?}"
        );
    }

    #[tokio::test]
    async fn csl_persist_runs_after_core_trace_persistence_succeeds() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store: Arc<dyn CslStore> = Arc::new(FileCslStore::new(dir.path()));
        let session_id = "post-loop-csl-after-core";
        let manager = CslManager::new(
            Arc::clone(&store),
            session_id.to_string(),
            CslManagerConfig::default(),
        )
        .expect("csl manager");
        let ctx = test_post_loop_persist_context(session_id, Some(manager));
        let mut state = crate::turn::agentic_loop::host::make_test_loop_state();
        state.session_turn = 3;
        state.messages = vec![json!({"role": "user", "content": "question"})];
        state.final_text = "answer".to_string();
        let mut errors = Vec::new();

        ctx.persist_csl_if_core_trace_persisted(&state, true, &mut errors)
            .await;

        assert!(errors.is_empty());
        let entries = store.load_after(session_id, 0).await.expect("load csl");
        assert_eq!(entries.len(), 1, "successful core+trace must allow CSL");
    }

    #[test]
    fn lifecycle_token_usage_json_uses_canonical_prompt_cache_shape() {
        let usage = lifecycle_token_usage_json(10, 4, 3, 5).expect("non-empty usage");

        assert_eq!(usage["input_tokens"], 10);
        assert_eq!(usage["cached_input_tokens"], 4);
        assert_eq!(usage["cache_creation_tokens"], 3);
        assert_eq!(usage["output_tokens"], 5);
        assert_eq!(usage["total_tokens"], 22);
        assert_eq!(usage["prompt"], 17);
        assert_eq!(usage["completion"], 5);
        assert_eq!(usage["cache_read"], 4);
        assert_eq!(usage["cache_write"], 3);
        assert_eq!(usage["raw_prompt_tokens"], 17);
        assert_eq!(usage["uncached_input_tokens"], 10);
        assert_eq!(usage["effective_input_tokens"], 10);
        assert_eq!(
            usage["prompt_cache_hit_ratio"],
            serde_json::json!(4.0 / 17.0)
        );
        assert_eq!(usage["total"], 22);
        assert!(
            usage.get("cache_read_tokens").is_none(),
            "persisted runtime events must use canonical prompt-cache field names"
        );
        assert!(lifecycle_token_usage_json(0, 0, 0, 0).is_none());
    }

    #[test]
    fn canonical_run_transcript_keeps_tool_history_before_terminal_assistant() {
        let trace = server_trace_context("user-1", "session-1", "run-1", 3);
        let mut state = crate::turn::agentic_loop::host::make_test_loop_state();
        state.final_text = "implemented the fix".to_string();
        state
            .stall
            .tool_call_records
            .push(astra_services::session_journal::ToolCallRecord {
                tool_call_id: Some("call-1".to_string()),
                name: "read_file".to_string(),
                ok: true,
                ms: 12,
                args_full: Some(r#"{"path":"src/lib.rs"}"#.to_string()),
                result_full: Some("pub fn main() {}".to_string()),
                ..Default::default()
            });

        let items = transcript_items_from_server_loop(
            "user-1",
            "session-1",
            "run-1",
            Some(&trace),
            "inspect the implementation",
            &state,
            true,
        );

        assert_eq!(
            items.iter().map(|item| item.role).collect::<Vec<_>>(),
            vec!["user", "assistant", "tool", "assistant"]
        );
        assert_eq!(items[0].source_event_id, trace.root_event_id);
        assert_eq!(
            items[1].source_event_id,
            trace_event_id("tool_start", &["run-1", "call-1"])
        );
        assert_eq!(
            items[2].source_event_id,
            trace_event_id("tool_call_completed", &["run-1", "call-1"])
        );
        assert_eq!(
            items[3].source_event_id,
            trace_event_id("response", &["run-1", &trace.turn_id])
        );
        let call = items[1]
            .payload
            .as_ref()
            .and_then(|payload| payload.tool_calls.first())
            .expect("tool call remains typed in canonical transcript");
        assert_eq!(call.name, "read_file");
        assert_eq!(call.arguments, r#"{"path":"src/lib.rs"}"#);
        let result = items[2]
            .payload
            .as_ref()
            .and_then(|payload| payload.tool_result.as_ref())
            .expect("tool result remains typed in canonical transcript");
        assert_eq!(result.tool_use_id, "call-1");
        assert_eq!(result.status.as_deref(), Some("completed"));
        assert_eq!(items[3].content, "implemented the fix");
    }

    #[test]
    fn durable_run_evidence_and_reasoning_keep_typed_identity() {
        let mut reasoning = TranscriptReasoningProjection::default();
        apply_reasoning_event_payload(
            &mut reasoning,
            &json!({"event_type": "reasoning_message_content", "data": {"content": "checking invariants"}}),
        );
        apply_reasoning_event_payload(&mut reasoning, &json!({"type": "thinking_done"}));
        assert_eq!(reasoning.text, "checking invariants");
        assert!(reasoning.done);

        let items = transcript_evidence_items_from_run_event(
            "run-1",
            "event-approval-batch",
            "approval_batch_required",
            &json!({
                "data": {
                    "requests": [
                        {"request_id": "approval-1", "tool": "bash", "detail": "cargo test"},
                        {"request_id": "approval-2", "tool": "write_file", "detail": "src/lib.rs"}
                    ]
                }
            }),
        );

        assert_eq!(items.len(), 2);
        assert_ne!(items[0].source_event_id, items[1].source_event_id);
        let stable_keys = items
            .iter()
            .map(|item| {
                item.payload
                    .as_ref()
                    .and_then(|payload| payload.evidence.as_ref())
                    .expect("approval remains structured evidence")
                    .stable_key()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            stable_keys,
            vec!["approval:approval-1", "approval:approval-2"]
        );
    }

    #[test]
    fn llm_round_trace_events_use_canonical_token_usage() {
        let trace = server_trace_context("user-1", "session-1", "run-1", 3);
        let events = build_llm_round_trace_events(
            &trace,
            "run-1",
            Some("parent-run"),
            Some("agent-1"),
            Some("root-agent"),
            Some("fallback-model"),
            &[crate::turn::agentic_loop::host::RecentRoundSummary {
                round: 2,
                model: "model-1".to_string(),
                prompt_tokens: 10,
                cache_read_tokens: 4,
                cache_creation_tokens: 3,
                completion_tokens: 5,
                duration_ms: 123,
                ..Default::default()
            }],
        );

        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.event_type, "llm_round_completed");
        assert_eq!(event.trace_kind, "llm_round");
        assert_eq!(event.round_index, Some(2));
        let token_usage = event.token_usage.as_ref().expect("token usage");
        assert_eq!(token_usage["input_tokens"], 10);
        assert_eq!(token_usage["cached_input_tokens"], 4);
        assert_eq!(token_usage["cache_creation_tokens"], 3);
        assert_eq!(token_usage["output_tokens"], 5);
        assert_eq!(token_usage["total_tokens"], 22);
        assert_eq!(token_usage["prompt"], 17);
        assert_eq!(token_usage["completion"], 5);
        assert_eq!(token_usage["cache_read"], 4);
        assert_eq!(token_usage["cache_write"], 3);
        assert_eq!(token_usage["total"], 22);
    }

    #[tokio::test]
    async fn server_loop_tool_events_are_audit_facing_tool_call_records() {
        let writer = CaptureToolEventWriter::default();
        let mut state = crate::turn::agentic_loop::host::make_test_loop_state();
        state.telemetry.all_tools_used = ["bash", "read_file"]
            .into_iter()
            .map(str::to_string)
            .collect();

        persist_server_loop_tool_events(&writer, "user-1", "session-1", Some("agent-1"), &state)
            .await
            .expect("tool event persistence should succeed");

        let plans = writer.plans.lock().expect("capture lock");
        assert_eq!(plans.len(), 1);
        let events = &plans[0].events;
        assert_eq!(events.len(), 2);
        let mut tool_names = events
            .iter()
            .map(|event| {
                assert!(!event.event_id.is_empty());
                assert_eq!(event.user_id, "user-1");
                assert_eq!(event.session_id, "session-1");
                assert_eq!(event.agent_id.as_deref(), Some("agent-1"));
                assert_eq!(event.event_type, "tool_call");
                assert_eq!(event.parent_event_id, None);
                assert!(event.parent_event_ids.is_empty());
                assert!(!event.causal_chain_id.is_empty());
                event
                    .metadata
                    .as_ref()
                    .and_then(|metadata| metadata.get("tool_name"))
                    .and_then(Value::as_str)
                    .expect("tool_name metadata")
                    .to_string()
            })
            .collect::<Vec<_>>();
        tool_names.sort();
        assert_eq!(tool_names, vec!["bash", "read_file"]);
    }

    #[tokio::test]
    async fn server_loop_hook_audit_prompt_tokens_include_cache_buckets() {
        let writer = CaptureHookDbWriter::default();
        let mut state = crate::turn::agentic_loop::host::make_test_loop_state();
        state.final_text = "done".to_string();
        state.total_prompt = 10;
        state.total_cache_read = 4;
        state.total_cache_creation = 3;
        state.total_completion = 5;

        persist_server_loop_hook_events(
            &writer,
            "user-1",
            "session-1",
            "work",
            &state,
            Some("model-a"),
        )
        .await
        .expect("hook event persistence should succeed");

        let plans = writer.plans.lock().expect("capture lock");
        assert_eq!(plans.len(), 1);
        let decision = plans[0].decision_audit.as_ref().expect("decision audit");
        assert_eq!(decision.decision_output["total_prompt_tokens"], 17);
        assert_eq!(decision.decision_output["total_completion_tokens"], 5);
    }

    #[tokio::test]
    async fn server_loop_tool_events_skip_empty_tool_sets() {
        let writer = CaptureToolEventWriter::default();
        let state = crate::turn::agentic_loop::host::make_test_loop_state();

        persist_server_loop_tool_events(&writer, "user-1", "session-1", None, &state)
            .await
            .expect("empty tool set persistence should succeed");

        let plans = writer.plans.lock().expect("capture lock");
        assert!(plans.is_empty());
    }

    struct FakeRunLifecyclePersistenceRow {
        failed_column: Option<&'static str>,
        count: i64,
        item_seq: i64,
        role: &'static str,
        content_hash: &'static str,
    }

    impl Default for FakeRunLifecyclePersistenceRow {
        fn default() -> Self {
            Self {
                failed_column: None,
                count: 2,
                item_seq: 7,
                role: "assistant",
                content_hash: "sha256:page-item",
            }
        }
    }

    impl FakeRunLifecyclePersistenceRow {
        fn fail_on(column: &'static str) -> Self {
            Self {
                failed_column: Some(column),
                ..Self::default()
            }
        }

        fn with_count(count: i64) -> Self {
            Self {
                count,
                ..Self::default()
            }
        }

        fn with_item_seq(item_seq: i64) -> Self {
            Self {
                item_seq,
                ..Self::default()
            }
        }

        fn with_role(role: &'static str) -> Self {
            Self {
                role,
                ..Self::default()
            }
        }

        fn with_content_hash(content_hash: &'static str) -> Self {
            Self {
                content_hash,
                ..Self::default()
            }
        }
    }

    impl RowExt for FakeRunLifecyclePersistenceRow {
        fn i64_column(&self, column: &str) -> Result<i64, sqlx::Error> {
            if self.failed_column == Some(column) {
                return Err(sqlx::Error::ColumnNotFound(column.to_string()));
            }
            match column {
                "count" => Ok(self.count),
                "item_seq" => Ok(self.item_seq),
                _ => Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
        }

        fn string_column(&self, column: &str) -> Result<String, sqlx::Error> {
            if self.failed_column == Some(column) {
                return Err(sqlx::Error::ColumnNotFound(column.to_string()));
            }
            match column {
                "role" => Ok(self.role.to_string()),
                "content_hash" => Ok(self.content_hash.to_string()),
                _ => Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
        }
    }

    #[test]
    fn post_compaction_manifest_count_decode_preserves_zero_and_fails_loudly() {
        assert_eq!(
            decode_post_compaction_manifest_count(&FakeRunLifecyclePersistenceRow::with_count(0))
                .unwrap(),
            0
        );
        assert_eq!(
            decode_post_compaction_manifest_count(&FakeRunLifecyclePersistenceRow::with_count(2))
                .unwrap(),
            2
        );

        let missing = decode_post_compaction_manifest_count(
            &FakeRunLifecyclePersistenceRow::fail_on("count"),
        )
        .unwrap_err();
        assert!(
            missing.contains("post-compaction context manifest count") && missing.contains("count"),
            "missing count should fail loudly: {missing}"
        );

        let negative =
            decode_post_compaction_manifest_count(&FakeRunLifecyclePersistenceRow::with_count(-1))
                .unwrap_err();
        assert!(
            negative.contains("count") && negative.contains("non-negative integer"),
            "negative count should fail loudly: {negative}"
        );
    }

    #[test]
    fn transcript_page_item_row_decode_preserves_database_values() {
        let row =
            decode_transcript_page_item_row(&FakeRunLifecyclePersistenceRow::default()).unwrap();

        assert_eq!(
            row,
            TranscriptPageItemRow {
                item_seq: 7,
                role: "assistant".to_string(),
                content_hash: "sha256:page-item".to_string(),
            }
        );
    }

    #[test]
    fn transcript_page_item_row_decode_fails_loudly_on_any_selected_column_error() {
        for column in ["item_seq", "role", "content_hash"] {
            let error =
                decode_transcript_page_item_row(&FakeRunLifecyclePersistenceRow::fail_on(column))
                    .unwrap_err();
            assert!(
                error.contains("transcript page item row") && error.contains(column),
                "decode error should identify selected column `{column}`: {error}"
            );
        }
    }

    #[test]
    fn transcript_page_item_row_decode_rejects_invalid_page_identity() {
        for item_seq in [0, -1] {
            let error = decode_transcript_page_item_row(
                &FakeRunLifecyclePersistenceRow::with_item_seq(item_seq),
            )
            .unwrap_err();
            assert!(
                error.contains("item_seq") && error.contains("positive integer"),
                "invalid item_seq should fail loudly: {error}"
            );
        }

        for (column, row) in [
            ("role", FakeRunLifecyclePersistenceRow::with_role("   ")),
            (
                "content_hash",
                FakeRunLifecyclePersistenceRow::with_content_hash(""),
            ),
        ] {
            let error = decode_transcript_page_item_row(&row).unwrap_err();
            assert!(
                error.contains(column) && error.contains("non-empty string"),
                "empty transcript page identity column should fail loudly for `{column}`: {error}"
            );
        }
    }

    async fn cleanup_transcript_fixture_for_owner(
        db: &sqlx::Pool<sqlx::MySql>,
        session_id: &str,
        user_id: &str,
    ) {
        sqlx::query("DELETE FROM transcript_pages WHERE session_id = ? AND user_id = ?")
            .bind(session_id)
            .bind(user_id)
            .execute(db)
            .await
            .expect("cleanup transcript fixture transcript_pages");
        sqlx::query("DELETE FROM session_transcript_items WHERE session_id = ? AND user_id = ?")
            .bind(session_id)
            .bind(user_id)
            .execute(db)
            .await
            .expect("cleanup transcript fixture session_transcript_items");
        sqlx::query("DELETE FROM agent_sessions WHERE session_id = ? AND user_id = ?")
            .bind(session_id)
            .bind(user_id)
            .execute(db)
            .await
            .expect("cleanup transcript fixture agent_sessions");
    }

    async fn cleanup_core_persist_fixture_for_owner(
        db: &sqlx::Pool<sqlx::MySql>,
        session_id: &str,
        user_id: &str,
    ) {
        sqlx::query("DELETE FROM transcript_pages WHERE session_id = ? AND user_id = ?")
            .bind(session_id)
            .bind(user_id)
            .execute(db)
            .await
            .expect("cleanup core persist transcript_pages");
        sqlx::query("DELETE FROM session_transcript_items WHERE session_id = ? AND user_id = ?")
            .bind(session_id)
            .bind(user_id)
            .execute(db)
            .await
            .expect("cleanup core persist session_transcript_items");
        sqlx::query("DELETE FROM agent_event_edges WHERE session_id = ? AND user_id = ?")
            .bind(session_id)
            .bind(user_id)
            .execute(db)
            .await
            .expect("cleanup core persist agent_event_edges");
        sqlx::query("DELETE FROM agent_events WHERE session_id = ? AND user_id = ?")
            .bind(session_id)
            .bind(user_id)
            .execute(db)
            .await
            .expect("cleanup core persist agent_events");
        sqlx::query("DELETE FROM agent_sessions WHERE session_id = ? AND user_id = ?")
            .bind(session_id)
            .bind(user_id)
            .execute(db)
            .await
            .expect("cleanup core persist agent_sessions");
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
    async fn core_persistence_records_deferred_inputs_without_splitting_audit_turns() {
        let pool = setup_pool().await;
        let db = pool.get().clone();
        let matrixone = MatrixOneSettings::from_env();
        let session_id = Uuid::new_v4().to_string();
        let user_id = Uuid::new_v4().to_string();
        let run_id = Uuid::new_v4().to_string();

        cleanup_core_persist_fixture_for_owner(&db, &session_id, &user_id).await;
        sqlx::query(
            "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count)
             VALUES (?, ?, 'core-persist-deferred-it', 'active', 0)",
        )
        .bind(&session_id)
        .bind(&user_id)
        .execute(&db)
        .await
        .expect("insert owner session");

        let mut state = crate::turn::agentic_loop::host::make_test_loop_state();
        state.session_turn = 7;
        state.final_text = "assistant final".to_string();
        state.user_intents.record_applied_user_intents(&[
            crate::turn::agentic_loop::host::AppliedUserIntent {
                intent_id: "intent-2".to_string(),
                delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                status: astra_turn_types::UserIntentStatus::Applied,
                event_index: 3,
                content: "queued two".to_string(),
            },
            crate::turn::agentic_loop::host::AppliedUserIntent {
                intent_id: "intent-3".to_string(),
                delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                status: astra_turn_types::UserIntentStatus::Applied,
                event_index: 4,
                content: "queued three".to_string(),
            },
        ]);

        let persist = PostLoopPersistContext {
            matrixone,
            shared_pool: Some(pool.clone()),
            user_id: user_id.clone(),
            session_id: session_id.clone(),
            run_id: run_id.clone(),
            agent_id: None,
            model_name: Some("test-model".to_string()),
            user_message: "initial one".to_string(),
            hook_db_writer: None,
            observer_worker: None,
            tool_event_writer: None,
            metrics_registry: None,
            csl_manager: None,
        };
        persist
            .persist_core_and_trace_in_transaction(&state)
            .await
            .expect("persist atomic core, trace, and transcript prefix");
        persist
            .materialize_run_transcript_evidence(&state)
            .await
            .expect("materialize terminal assistant transcript evidence");

        let event_rows = sqlx::query(
            "SELECT event_id, event_type, content, parent_event_id
             FROM agent_events
             WHERE session_id = ? AND user_id = ?
             ORDER BY created_at ASC, event_id ASC",
        )
        .bind(&session_id)
        .bind(&user_id)
        .fetch_all(&db)
        .await
        .expect("agent events");
        let user_query_rows = event_rows
            .iter()
            .filter(|row| row.try_get::<String, _>("event_type").unwrap() == "user_query")
            .collect::<Vec<_>>();
        let user_message_rows = event_rows
            .iter()
            .filter(|row| row.try_get::<String, _>("event_type").unwrap() == "user_message")
            .collect::<Vec<_>>();
        let response_rows = event_rows
            .iter()
            .filter(|row| row.try_get::<String, _>("event_type").unwrap() == "llm_response")
            .collect::<Vec<_>>();
        assert_eq!(user_query_rows.len(), 1, "run must have one audit turn");
        assert_eq!(user_message_rows.len(), 2, "user intents are messages");
        assert_eq!(response_rows.len(), 1, "run must have one final response");
        let root_event_id = user_query_rows[0].try_get::<String, _>("event_id").unwrap();
        assert_eq!(
            response_rows[0]
                .try_get::<Option<String>, _>("parent_event_id")
                .unwrap()
                .as_deref(),
            Some(root_event_id.as_str()),
            "final response must remain attached to the audit turn root"
        );
        assert!(user_message_rows.iter().all(|row| {
            row.try_get::<Option<String>, _>("parent_event_id")
                .unwrap()
                .as_deref()
                == Some(root_event_id.as_str())
        }));

        let transcript_rows = sqlx::query(
            "SELECT role, content
             FROM session_transcript_items
             WHERE session_id = ? AND user_id = ?
             ORDER BY item_seq ASC",
        )
        .bind(&session_id)
        .bind(&user_id)
        .fetch_all(&db)
        .await
        .expect("transcript rows");
        let transcript_items = transcript_rows
            .iter()
            .map(|row| {
                (
                    row.try_get::<String, _>("role").unwrap(),
                    row.try_get::<String, _>("content").unwrap(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            transcript_items,
            vec![
                ("user".to_string(), "initial one".to_string()),
                ("user".to_string(), "queued two".to_string()),
                ("user".to_string(), "queued three".to_string()),
                ("assistant".to_string(), "assistant final".to_string()),
            ],
            "transcript remains the ordered user-facing conversation"
        );

        cleanup_core_persist_fixture_for_owner(&db, &session_id, &user_id).await;
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
    async fn transcript_persistence_writes_owner_scoped_pages_and_rejects_wrong_owner() {
        let pool = setup_pool().await;
        let db = pool.get().clone();
        let session_id = Uuid::new_v4().to_string();
        let owner_user_id = Uuid::new_v4().to_string();
        let other_user_id = Uuid::new_v4().to_string();
        let run_id = Uuid::new_v4().to_string();

        cleanup_transcript_fixture_for_owner(&db, &session_id, &owner_user_id).await;
        cleanup_transcript_fixture_for_owner(&db, &session_id, &other_user_id).await;

        sqlx::query(
            "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count)
             VALUES (?, ?, 'transcript-persist-it', 'active', 0)",
        )
        .bind(&session_id)
        .bind(&owner_user_id)
        .execute(&db)
        .await
        .expect("insert owner session");
        sqlx::query(
            "INSERT INTO session_transcript_items
             (user_id, session_id, item_seq, run_id, role, content, source_event_id, content_hash, created_at)
             VALUES (?, ?, 1, ?, 'assistant', 'foreign dirty row', ?, 'foreign-hash', NOW(6))",
        )
        .bind(&other_user_id)
        .bind(&session_id)
        .bind(&run_id)
        .bind(Uuid::new_v4().to_string())
        .execute(&db)
        .await
        .expect("insert foreign dirty transcript item");
        sqlx::query(
            "INSERT INTO transcript_pages
             (user_id, session_id, page_seq, start_item_seq, end_item_seq, item_count, page_hash, created_at, updated_at)
             VALUES (?, ?, 1, 1, 1, 1, 'foreign-page', NOW(6), NOW(6))",
        )
        .bind(&other_user_id)
        .bind(&session_id)
        .execute(&db)
        .await
        .expect("insert foreign dirty transcript page");

        let items = [
            TranscriptPersistItem {
                run_id: run_id.clone(),
                role: "user",
                content: "hello".to_string(),
                payload: None,
                source_event_id: Uuid::new_v4().to_string(),
            },
            TranscriptPersistItem {
                run_id: run_id.clone(),
                role: "user",
                content: "second input".to_string(),
                payload: None,
                source_event_id: Uuid::new_v4().to_string(),
            },
            TranscriptPersistItem {
                run_id: run_id.clone(),
                role: "assistant",
                content: "world".to_string(),
                payload: None,
                source_event_id: Uuid::new_v4().to_string(),
            },
        ];
        persist_session_transcript_items(&pool, &owner_user_id, &session_id, &items)
            .await
            .expect("owner transcript persist");

        let page = sqlx::query(
            "SELECT user_id, start_item_seq, end_item_seq, item_count
             FROM transcript_pages
             WHERE user_id = ? AND session_id = ? AND page_seq = 1",
        )
        .bind(&owner_user_id)
        .bind(&session_id)
        .fetch_one(&db)
        .await
        .expect("owner transcript page");
        assert_eq!(page.try_get::<String, _>("user_id").unwrap(), owner_user_id);
        assert_eq!(page.try_get::<i64, _>("start_item_seq").unwrap(), 1);
        assert_eq!(page.try_get::<i64, _>("end_item_seq").unwrap(), 3);
        assert_eq!(page.try_get::<i64, _>("item_count").unwrap(), 3);

        let owner_rows = sqlx::query(
            "SELECT role, content
             FROM session_transcript_items
             WHERE user_id = ? AND session_id = ?
             ORDER BY item_seq ASC",
        )
        .bind(&owner_user_id)
        .bind(&session_id)
        .fetch_all(&db)
        .await
        .expect("owner transcript rows");
        let owner_items = owner_rows
            .iter()
            .map(|row| {
                (
                    row.try_get::<String, _>("role").unwrap(),
                    row.try_get::<String, _>("content").unwrap(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            owner_items,
            vec![
                ("user".to_string(), "hello".to_string()),
                ("user".to_string(), "second input".to_string()),
                ("assistant".to_string(), "world".to_string()),
            ],
            "transcript must preserve multiple user inputs in one run"
        );

        let same_item_seq_rows = sqlx::query(
            "SELECT COUNT(*) AS c
             FROM session_transcript_items
             WHERE session_id = ? AND item_seq = 1",
        )
        .bind(&session_id)
        .fetch_one(&db)
        .await
        .expect("count shared item_seq rows")
        .try_get::<i64, _>("c")
        .expect("decode shared item_seq count");
        assert_eq!(
            same_item_seq_rows, 2,
            "transcript item identity must include owner"
        );

        let same_page_seq_rows = sqlx::query(
            "SELECT COUNT(*) AS c
             FROM transcript_pages
             WHERE session_id = ? AND page_seq = 1",
        )
        .bind(&session_id)
        .fetch_one(&db)
        .await
        .expect("count shared page_seq rows")
        .try_get::<i64, _>("c")
        .expect("decode shared page_seq count");
        assert_eq!(
            same_page_seq_rows, 2,
            "transcript page identity must include owner"
        );

        let mut wrong_owner_tx = db.begin().await.expect("begin wrong-owner transcript tx");
        let wrong_owner = persist_session_transcript_items_inner_in_tx(
            &mut wrong_owner_tx,
            &other_user_id,
            &session_id,
            &[TranscriptPersistItem {
                run_id,
                role: "assistant",
                content: "wrong owner".to_string(),
                payload: None,
                source_event_id: Uuid::new_v4().to_string(),
            }],
        )
        .await
        .expect_err("wrong owner must not persist transcript rows");
        wrong_owner_tx
            .rollback()
            .await
            .expect("rollback wrong-owner transcript tx");
        assert!(
            matches!(&wrong_owner, sqlx::Error::RowNotFound),
            "wrong owner should fail closed before writing, got {wrong_owner}"
        );

        let wrong_owner_rows = sqlx::query(
            "SELECT COUNT(*) AS c
             FROM session_transcript_items
             WHERE session_id = ? AND user_id = ?",
        )
        .bind(&session_id)
        .bind(&other_user_id)
        .fetch_one(&db)
        .await
        .expect("count wrong owner rows")
        .try_get::<i64, _>("c")
        .expect("decode wrong owner count");
        assert_eq!(
            wrong_owner_rows, 1,
            "wrong owner attempt must not add rows beyond the seeded dirty row"
        );
        let wrong_owner_attempt_rows = sqlx::query(
            "SELECT COUNT(*) AS c
             FROM session_transcript_items
             WHERE session_id = ? AND user_id = ? AND content = 'wrong owner'",
        )
        .bind(&session_id)
        .bind(&other_user_id)
        .fetch_one(&db)
        .await
        .expect("count wrong owner attempted content")
        .try_get::<i64, _>("c")
        .expect("decode wrong owner attempted content count");
        assert_eq!(wrong_owner_attempt_rows, 0);

        cleanup_transcript_fixture_for_owner(&db, &session_id, &owner_user_id).await;
        cleanup_transcript_fixture_for_owner(&db, &session_id, &other_user_id).await;
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
    async fn server_transcript_commit_returns_exact_committed_assistant_identity() {
        let pool = setup_pool().await;
        let db = pool.get().clone();
        let session_id = Uuid::new_v4().to_string();
        let user_id = Uuid::new_v4().to_string();
        let run_id = Uuid::new_v4().to_string();
        cleanup_transcript_fixture_for_owner(&db, &session_id, &user_id).await;
        sqlx::query(
            "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count)
             VALUES (?, ?, 'transcript-commit-it', 'active', 0)",
        )
        .bind(&session_id)
        .bind(&user_id)
        .execute(&db)
        .await
        .expect("insert transcript commit session");

        let mut state = crate::turn::agentic_loop::host::make_test_loop_state();
        state.final_text = "identity-backed answer".into();
        state.session_turn = 7;
        let expected = terminal_assistant_transcript_item(
            &user_id,
            &session_id,
            &run_id,
            None,
            "inspect identity",
            &state,
        )
        .expect("terminal assistant item")
        .source_event_id;

        let committed = persist_server_loop_transcript_items(
            Some(&pool),
            &user_id,
            &session_id,
            &run_id,
            None,
            "inspect identity",
            &state,
            true,
        )
        .await
        .expect("server transcript commit");
        assert_eq!(committed.as_deref(), Some(expected.as_str()));

        let stored = sqlx::query(
            "SELECT source_event_id
             FROM session_transcript_items
             WHERE user_id = ? AND session_id = ? AND run_id = ? AND role = 'assistant'
             ORDER BY item_seq DESC LIMIT 1",
        )
        .bind(&user_id)
        .bind(&session_id)
        .bind(&run_id)
        .fetch_one(&db)
        .await
        .expect("read committed assistant identity")
        .try_get::<String, _>("source_event_id")
        .expect("decode source_event_id");
        assert_eq!(stored, expected);

        cleanup_transcript_fixture_for_owner(&db, &session_id, &user_id).await;
    }
}
