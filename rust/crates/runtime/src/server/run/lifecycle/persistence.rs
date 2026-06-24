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
    TurnCoreEventRecord, TurnCoreEventWriter, TurnCorePersistPlan, TurnDecisionAuditRecord,
    TurnHookDbPersistPlan, TurnHookDbWriter, TurnObserverRequest, TurnObserverWorker,
    TurnSkillSelectionRecord, TurnToolEventPersistPlan, TurnToolEventRecord, TurnToolEventWriter,
};
use astra_turn_core::trace_event::{TraceContext, TraceEvent, TraceEventWriter};

use crate::MatrixOneSettings;
use crate::turn::agentic_loop::host::AgenticLoopState;
use crate::{
    DatabaseEvaluationService, DatabaseEventService, DatabaseTraceEventWriter,
    EventCreateRequestData, EventService,
};

use super::{
    build_runtime_event_service, build_runtime_turn_evaluation_event, flush_turn_observability,
    persist_runtime_promotion_events, persist_turn_evaluation_journal,
};

/// Bundles all handles needed by post-loop best-effort persistence calls.
///
/// Both `create_run` and `stream_chat` run the same set of side effects after
/// the agentic loop finishes: core event persistence, tool event persistence,
/// hook DB writes, Memoria observer, pipeline learning, session-end hooks,
/// runtime promotion events, and learning-stack save.  This struct captures
/// the shared state so both paths can call `run()` instead of duplicating
/// ~60 lines of glue code.

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
    pub(crate) csl_manager:
        Option<tokio::sync::Mutex<astra_turn_core::conversation_log::manager::CslManager>>,
}

impl PostLoopPersistContext {
    /// Run all best-effort post-loop persistence side effects.
    ///
    /// The `loop_success` flag comes from `outcome.is_ok()` (before consuming
    /// the outcome in `finalize_run_events`).
    pub(crate) async fn run(&self, state: &AgenticLoopState, loop_success: bool) {
        let _ = loop_success;
        // 0. Persist CSL via CslManager.
        if let Some(ref mgr) = self.csl_manager {
            let mut mgr = mgr.lock().await;
            let session_state = extract_session_state_compact(state);
            let messages = messages_for_csl_persist(state);
            if let Err(e) = mgr
                .persist_turn(state.session_turn, &messages, &session_state)
                .await
            {
                tracing::warn!(
                    session_id = %self.session_id,
                    error = %e,
                    "CSL persist failed"
                );
            }
        }

        // 1–2. Persist core events + trace detail events in a single MatrixOne
        // transaction so that a crash between writes leaves a consistent state.
        let _mo_tx = self.persist_core_and_trace_in_transaction(state).await;

        // 3. Persist compatibility aggregate tool_call events for session_audit metrics.
        if let Some(ref writer) = self.tool_event_writer {
            persist_server_loop_tool_events(
                writer.as_ref(),
                &self.user_id,
                &self.session_id,
                self.agent_id.as_deref(),
                state,
            )
            .await;
        }

        // 4. Persist decision audit + skill selection to hook DB.
        if let Some(ref writer) = self.hook_db_writer {
            persist_server_loop_hook_events(
                writer.as_ref(),
                &self.user_id,
                &self.session_id,
                &self.user_message,
                state,
                self.model_name.as_deref(),
            )
            .await;
        }

        // 5. Fire Memoria observer (cross-session knowledge extraction).
        if let Some(ref worker) = self.observer_worker {
            fire_server_loop_observer(worker.as_ref(), &self.user_id, &self.session_id, state)
                .await;
        }

        // 6. Fire SessionEnd hooks.
        crate::skills::hooks::fire_session_end(
            &state.skills.session_event_hooks,
            state.current_session_id.as_deref().unwrap_or(""),
        )
        .await;

        // 7. Persist runtime promotion events.
        persist_runtime_promotion_events(
            &self.matrixone,
            self.shared_pool.as_ref(),
            &self.user_id,
            &self.session_id,
            &self.run_id,
            &state.telemetry.promotion_events,
        )
        .await;

        // 8. Persist web-agent state projection rows generated by the agentic loop.
        persist_server_loop_projection_state(
            self.shared_pool.as_ref(),
            &self.user_id,
            &self.session_id,
            &self.run_id,
            self.agent_id.as_deref(),
            self.model_name.as_deref(),
            state,
        )
        .await;
    }

    /// Persist core events and trace detail events in a single MatrixOne
    /// transaction. If the transaction fails, all writes are rolled back
    /// atomically — preventing partial state on crash.
    async fn persist_core_and_trace_in_transaction(&self, state: &AgenticLoopState) {
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
            return;
        };

        let mut tx = match pool.get().begin().await {
            Ok(tx) => tx,
            Err(error) => {
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
                return;
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
                tracing::warn!(
                    session_id = %self.session_id,
                    error = %error,
                    "post-loop: core events tx failed, rolling back MO transaction"
                );
                let _ = tx.rollback().await;
                return;
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
            tracing::warn!(
                session_id = %self.session_id,
                error = %error,
                "post-loop: detail events tx failed, rolling back MO transaction"
            );
            let _ = tx.rollback().await;
            return;
        }

        // Best-effort commit: on failure, rollback naturally drops the tx.
        if let Err(error) = tx.commit().await {
            tracing::warn!(
                session_id = %self.session_id,
                error = %error,
                "post-loop: MO transaction commit failed, writes rolled back"
            );
        }
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
) {
    let Some(pool) = shared_pool else {
        return;
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
                token_estimate: ((final_text.len() / 4) as u32).clamp(20, 240),
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

    let post_compaction_count = sqlx::query(
        "SELECT COUNT(*) AS count FROM context_manifests \
         WHERE user_id = ? AND session_id = ? AND run_id = ? AND reason = 'post_compaction'",
    )
    .bind(user_id)
    .bind(session_id)
    .bind(run_id)
    .fetch_one(pool.get())
    .await
    .ok()
    .and_then(|row| row.try_get::<i64, _>("count").ok())
    .unwrap_or(0);
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
                    tracing::warn!(
                        target: "astra_runtime::state_projection",
                        session_id = %session_id,
                        run_id = %run_id,
                        error = %error,
                        "failed to persist post-compaction summary projection"
                    );
                }
            }
            Ok(results) => {
                tracing::warn!(
                    target: "astra_runtime::state_projection",
                    session_id = %session_id,
                    run_id = %run_id,
                    ?results,
                    "post-compaction invariant check failed after loop"
                );
            }
            Err(error) => {
                tracing::warn!(
                    target: "astra_runtime::state_projection",
                    session_id = %session_id,
                    run_id = %run_id,
                    error = %error,
                    "failed to run post-compaction invariant checks"
                );
            }
        }
    }
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
    loop_state.restricted_tools.extend(restored.blocked_tools);
    if !restored.recent_tools.is_empty() {
        loop_state.recent_tools = restored.recent_tools;
    }
    loop_state.idempotency_cache = restored.idempotency_cache;
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

fn messages_for_csl_persist(state: &AgenticLoopState) -> Vec<Value> {
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
    astra_turn_core::prompt_facing::sanitize_prompt_facing_messages(messages)
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
    if user_message.is_empty() && state.final_text.is_empty() {
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

    let llm_response_event = if !state.final_text.is_empty() {
        let usage = if state.total_prompt > 0
            || state.total_completion > 0
            || state.total_cache_read > 0
            || state.total_cache_creation > 0
        {
            Some(json!({
                "prompt": state.total_prompt,
                "completion": state.total_completion,
                "cache_read_tokens": state.total_cache_read,
                "cache_creation_tokens": state.total_cache_creation,
                "total": state.total_prompt
                    + state.total_completion
                    + state.total_cache_read
                    + state.total_cache_creation,
            }))
        } else {
            None
        };
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

    let mut events = Vec::with_capacity(2);
    if let Some(event) = user_query_event.clone() {
        events.push(event);
    }
    if let Some(event) = llm_response_event.clone() {
        events.push(event);
    }

    let plan = TurnCorePersistPlan {
        user_query_event: user_query_event.as_ref().map(|event| TurnCoreEventRecord {
            event_id: event.event_id.clone(),
            user_id: event.user_id.clone(),
            session_id: event.session_id.clone(),
            agent_id: event.agent_id.clone(),
            event_type: "user_query".to_string(),
            content: event.content.clone().unwrap_or_default(),
            parent_event_id: None,
            parent_event_ids: Vec::new(),
            causal_chain_id: trace.causal_chain_id.clone(),
            llm_model_used: None,
            token_usage: None,
            llm_params: None,
            reasoning_content: None,
        }),
        llm_response_event: llm_response_event
            .as_ref()
            .map(|event| TurnCoreEventRecord {
                event_id: event.event_id.clone(),
                user_id: event.user_id.clone(),
                session_id: event.session_id.clone(),
                agent_id: event.agent_id.clone(),
                event_type: "llm_response".to_string(),
                content: event.content.clone().unwrap_or_default(),
                parent_event_id: event.parent_event_id.clone(),
                parent_event_ids: event.parent_event_id.iter().cloned().collect(),
                causal_chain_id: trace.causal_chain_id.clone(),
                llm_model_used: event.llm_model_used.clone(),
                token_usage: event.token_usage.clone(),
                llm_params: None,
                reasoning_content: None,
            }),
        snapshot_link_plan: None,
    };
    let transcript_items = transcript_items_from_core_plan(&plan, run_id);

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
            persist_session_transcript_items_in_tx(tx, user_id, session_id, &transcript_items)
                .await;
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
            if let Some(p) = pool {
                persist_session_transcript_items(p, user_id, session_id, &transcript_items).await;
            }
        }
    }
    Ok(())
}

pub(crate) struct TranscriptPersistItem {
    pub(crate) run_id: String,
    pub(crate) role: &'static str,
    pub(crate) content: String,
    pub(crate) source_event_id: String,
}

fn transcript_items_from_core_plan(
    plan: &TurnCorePersistPlan,
    run_id: &str,
) -> Vec<TranscriptPersistItem> {
    let mut items = Vec::with_capacity(2);
    if let Some(event) = &plan.user_query_event {
        items.push(TranscriptPersistItem {
            run_id: run_id.to_string(),
            role: "user",
            content: event.content.clone(),
            source_event_id: event.event_id.clone(),
        });
    }
    if let Some(event) = &plan.llm_response_event {
        items.push(TranscriptPersistItem {
            run_id: run_id.to_string(),
            role: "assistant",
            content: event.content.clone(),
            source_event_id: event.event_id.clone(),
        });
    }
    items
}

pub(crate) async fn persist_session_transcript_items(
    pool: &SharedPool,
    user_id: &str,
    session_id: &str,
    items: &[TranscriptPersistItem],
) {
    if items.is_empty() {
        return;
    }
    let mut tx = match pool.get().begin().await {
        Ok(tx) => tx,
        Err(error) => {
            astra_core::agent_error!(
                "server-loop",
                "failed to begin transaction for transcript items for session {session_id}: {error}"
            );
            return;
        }
    };
    if let Err(error) =
        persist_session_transcript_items_inner_in_tx(&mut tx, user_id, session_id, items).await
    {
        astra_core::agent_error!(
            "server-loop",
            "failed to persist transcript items for session {session_id}: {error}"
        );
        let _ = tx.rollback().await;
        return;
    }
    if let Err(error) = tx.commit().await {
        astra_core::agent_error!(
            "server-loop",
            "failed to commit transcript items for session {session_id}: {error}"
        );
    }
}

/// Variant that uses an existing transaction instead of creating its own.
pub(crate) async fn persist_session_transcript_items_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    user_id: &str,
    session_id: &str,
    items: &[TranscriptPersistItem],
) {
    if items.is_empty() {
        return;
    }
    if let Err(error) =
        persist_session_transcript_items_inner_in_tx(tx, user_id, session_id, items).await
    {
        astra_core::agent_error!(
            "server-loop",
            "failed to persist transcript items in tx for session {session_id}: {error}"
        );
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
            event.token_usage = Some(json!({
                "prompt": round.prompt_tokens,
                "completion": round.completion_tokens,
                "cache_read_tokens": round.cache_read_tokens,
                "cache_creation_tokens": round.cache_creation_tokens,
                "total": round.prompt_tokens
                    + round.completion_tokens
                    + round.cache_read_tokens
                    + round.cache_creation_tokens,
            }));
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
         LIMIT 1",
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
            "SELECT COUNT(*) AS count
             FROM session_transcript_items
             WHERE session_id = ? AND user_id = ? AND run_id = ? AND role = ?",
        )
        .bind(session_id)
        .bind(user_id)
        .bind(&item.run_id)
        .bind(item.role)
        .fetch_one(&mut **tx)
        .await?
        .try_get::<i64, _>("count")?;
        if existing > 0 {
            continue;
        }

        let item_seq = next_seq;
        sqlx::query(
            "INSERT INTO session_transcript_items
             (session_id, item_seq, user_id, run_id, role, content,
              source_event_id, source_event_idx, content_hash, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, NULL, ?, NOW(6))",
        )
        .bind(session_id)
        .bind(item_seq)
        .bind(user_id)
        .bind(&item.run_id)
        .bind(item.role)
        .bind(&item.content)
        .bind(&item.source_event_id)
        .bind(transcript_content_hash(item.role, &item.content))
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

    let first_item_seq = rows
        .first()
        .and_then(|row| row.try_get::<i64, _>("item_seq").ok())
        .unwrap_or(start_item_seq);
    let last_item_seq = rows
        .last()
        .and_then(|row| row.try_get::<i64, _>("item_seq").ok())
        .unwrap_or(end_item_seq);
    let mut hasher = Sha256::new();
    for row in &rows {
        hasher.update(row.try_get::<i64, _>("item_seq")?.to_string().as_bytes());
        hasher.update([0]);
        hasher.update(row.try_get::<String, _>("role")?.as_bytes());
        hasher.update([0]);
        hasher.update(row.try_get::<String, _>("content_hash")?.as_bytes());
        hasher.update([0xff]);
    }
    let page_hash = format!("{:x}", hasher.finalize());
    sqlx::query(
        "INSERT INTO transcript_pages
         (user_id, session_id, page_seq, start_item_seq, end_item_seq, item_count, page_hash, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, NOW(6), NOW(6))
         ON DUPLICATE KEY UPDATE
           user_id = VALUES(user_id),
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

fn transcript_content_hash(role: &str, content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(role.as_bytes());
    hasher.update([0]);
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Persist `tool_call` events to `agent_events` for tools used during the
/// server-driven agentic loop.  The bridge path creates detailed per-call
/// records; here we create one event per unique tool name from
/// `state.telemetry.all_tools_used` with metadata containing `tool_name`
/// so that `session_audit` aggregate queries (`meta_tool_name`, `tool_calls_total`)
/// return correct results for server-loop sessions.
async fn persist_server_loop_tool_events(
    writer: &dyn TurnToolEventWriter,
    user_id: &str,
    session_id: &str,
    agent_id: Option<&str>,
    state: &AgenticLoopState,
) {
    if state.telemetry.all_tools_used.is_empty() {
        return;
    }

    let chain_id = server_loop_causal_chain_id("server-loop-tools");
    let mut events = Vec::with_capacity(state.telemetry.all_tools_used.len());

    for tool_name in &state.telemetry.all_tools_used {
        events.push(TurnToolEventRecord {
            event_id: Uuid::now_v7().to_string(),
            user_id: user_id.to_string(),
            session_id: session_id.to_string(),
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
    if let Err(e) = writer.persist(plan).await {
        astra_core::agent_error!(
            "server-loop",
            "failed to persist tool events for session {session_id}: {e}"
        );
    }
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
) {
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
            "total_prompt_tokens": state.total_prompt,
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
    let _ = &selected_skills;

    let plan = TurnHookDbPersistPlan {
        decision_audit,
        skill_selection,
        reflection_mark: None,
        reflection_lesson: None,
    };

    if let Err(e) = hook_db_writer.persist(plan).await {
        astra_core::agent_error!(
            "server-loop",
            "failed to persist hook events for session {session_id}: {e}"
        );
    }
}

/// Fire the Memoria observer after the server-driven loop completes.
/// This sends the conversation messages to the Memoria `/v1/observe` endpoint
/// for cross-session knowledge extraction.
async fn fire_server_loop_observer(
    observer_worker: &dyn TurnObserverWorker,
    user_id: &str,
    session_id: &str,
    state: &AgenticLoopState,
) {
    let messages: Vec<serde_json::Map<String, serde_json::Value>> = state
        .messages
        .iter()
        .filter_map(|m| m.as_object().cloned())
        .collect();

    if messages.is_empty() {
        return;
    }

    let turn_count = state
        .session_turn
        .max(state.max_turns.saturating_sub(state.remaining_turns) as u32)
        as i64;
    let request = TurnObserverRequest {
        user_id: user_id.to_string(),
        session_id: session_id.to_string(),
        messages,
        turn_count,
        session_start: None,
    };

    if let Err(e) = observer_worker.run(request).await {
        astra_core::agent_error!(
            "server-loop",
            "failed to run observer for session {session_id}: {e}"
        );
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
        interruption.is_some(),
        &astra_turn_core::stall::DivergenceStatus::Healthy,
        execution_state,
        (!final_text.is_empty()).then_some(final_text),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::Row;
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

    #[test]
    fn messages_for_csl_persist_keeps_only_prompt_facing_history() {
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

        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["content"], "不要review啊！");
        assert_eq!(messages[2]["content"], "ok");
        assert!(
            messages
                .iter()
                .all(|msg| msg["role"] != "tool" && msg.get("reasoning_content").is_none())
        );
        assert!(
            messages
                .iter()
                .all(|msg| !msg["content"].as_str().unwrap_or("").contains("old review"))
        );
        assert!(messages.iter().all(|msg| {
            !msg["content"]
                .as_str()
                .unwrap_or("")
                .contains("runtime recap")
        }));
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

        let _ = sqlx::query("DELETE FROM transcript_pages WHERE session_id = ?")
            .bind(&session_id)
            .execute(&db)
            .await;
        let _ = sqlx::query("DELETE FROM session_transcript_items WHERE session_id = ?")
            .bind(&session_id)
            .execute(&db)
            .await;
        let _ = sqlx::query("DELETE FROM agent_sessions WHERE session_id = ?")
            .bind(&session_id)
            .execute(&db)
            .await;

        sqlx::query(
            "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count)
             VALUES (?, ?, 'transcript-persist-it', 'active', 0)",
        )
        .bind(&session_id)
        .bind(&owner_user_id)
        .execute(&db)
        .await
        .expect("insert owner session");

        let items = [
            TranscriptPersistItem {
                run_id: run_id.clone(),
                role: "user",
                content: "hello".to_string(),
                source_event_id: Uuid::new_v4().to_string(),
            },
            TranscriptPersistItem {
                run_id: run_id.clone(),
                role: "assistant",
                content: "world".to_string(),
                source_event_id: Uuid::new_v4().to_string(),
            },
        ];
        let mut owner_tx = db.begin().await.expect("begin owner transcript tx");
        persist_session_transcript_items_inner_in_tx(
            &mut owner_tx,
            &owner_user_id,
            &session_id,
            &items,
        )
        .await
        .expect("owner transcript persist");
        owner_tx.commit().await.expect("commit owner transcript tx");

        let page = sqlx::query(
            "SELECT user_id, start_item_seq, end_item_seq, item_count
             FROM transcript_pages
             WHERE session_id = ? AND page_seq = 1",
        )
        .bind(&session_id)
        .fetch_one(&db)
        .await
        .expect("owner transcript page");
        assert_eq!(page.try_get::<String, _>("user_id").unwrap(), owner_user_id);
        assert_eq!(page.try_get::<i64, _>("start_item_seq").unwrap(), 1);
        assert_eq!(page.try_get::<i64, _>("end_item_seq").unwrap(), 2);
        assert_eq!(page.try_get::<i64, _>("item_count").unwrap(), 2);

        let mut wrong_owner_tx = db.begin().await.expect("begin wrong-owner transcript tx");
        let wrong_owner = persist_session_transcript_items_inner_in_tx(
            &mut wrong_owner_tx,
            &other_user_id,
            &session_id,
            &[TranscriptPersistItem {
                run_id,
                role: "assistant",
                content: "wrong owner".to_string(),
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
        assert_eq!(wrong_owner_rows, 0);

        let _ = sqlx::query("DELETE FROM transcript_pages WHERE session_id = ?")
            .bind(&session_id)
            .execute(&db)
            .await;
        let _ = sqlx::query("DELETE FROM session_transcript_items WHERE session_id = ?")
            .bind(&session_id)
            .execute(&db)
            .await;
        let _ = sqlx::query("DELETE FROM agent_sessions WHERE session_id = ?")
            .bind(&session_id)
            .execute(&db)
            .await;
    }
}
