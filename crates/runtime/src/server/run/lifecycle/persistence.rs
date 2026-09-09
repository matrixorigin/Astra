//! Post-loop persistence: core events, trace events, hook DB
//! writes, observer, runtime promotions, and session transcript management.
//!
//! Extracted from [`super`] to keep the lifecycle module manageable.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use sqlx::Row;
use tracing;
use uuid::Uuid;

use astra_core::{
    ErrorResponse, STATUS_CANCELLED, STATUS_PAUSED, STATUS_RUNNING, STATUS_WAITING, SharedPool,
    connect_matrixone,
};
use astra_services::EdgeContext;
use astra_services::coordination::AgentProfile;
use astra_services::runs::{
    AtomicRunTerminalEventReceipt, AtomicRunTerminalSettlementRequest,
    AtomicRunTerminalSettlementResolution, DatabaseRunStateStore, RunStateStore,
};
use astra_services::session_audit::{RUNTIME_PROMOTION_EVENT_TYPE, RuntimePromotionEventData};
use astra_services::skills::SkillService;
use astra_services::{
    DatabaseContextManifestStore, DatabaseStateProjectionStore, RetrievalStage, StateItemUpsert,
};
use astra_services::{
    WorkspaceCleanupDebtEntry, WorkspaceRecordEntry as StoredWorkspaceRecordEntry,
    WorkspaceRecordStoreError, WorkspaceStateStore,
};
use astra_turn_core::contracts::{
    TurnDecisionAuditRecord, TurnHookDbPersistPlan, TurnHookDbWriter, TurnObserverRequest,
    TurnObserverWorker, TurnSkillSelectionRecord,
};
use astra_turn_core::observer::filter_memory_operation_turns;
use astra_turn_core::trace_event::{TraceContext, TraceEvent, TraceEventWriter};
use astra_turn_types::SessionCursorV1;

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

    let billable_input = usage.normalized_prompt_cache_usage().total_input_tokens();
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
    pub(crate) expected_owner_generation: Option<u64>,
    pub(crate) owner_lease_duration: Option<Duration>,
    pub(crate) agent_id: Option<String>,
    pub(crate) model_name: Option<String>,
    pub(crate) user_message: String,
    pub(crate) hook_db_writer: Option<Arc<dyn TurnHookDbWriter>>,
    pub(crate) observer_worker: Option<Arc<dyn TurnObserverWorker>>,
    pub(crate) metrics_registry: Option<Arc<astra_turn_core::pipeline_metrics::MetricsRegistry>>,
    pub(crate) csl_manager:
        Option<tokio::sync::Mutex<astra_turn_core::conversation_log::manager::CslManager>>,
}

impl PostLoopPersistContext {
    fn atomic_terminal_canonical_append(
        &self,
        expected_owner_generation: u64,
    ) -> CanonicalLoopAppend<'_> {
        CanonicalLoopAppend {
            user_id: &self.user_id,
            session_id: &self.session_id,
            run_id: &self.run_id,
            expected_owner_generation: Some(expected_owner_generation),
            owner_lease_duration: self.owner_lease_duration,
            parent_run_id: None,
            parent_event_id: None,
            agent_id: self.agent_id.as_deref(),
            parent_agent_id: None,
            trace_context: None,
            user_message: &self.user_message,
            model_name: self.model_name.as_deref(),
            // A durable terminal must never become visible before its
            // user-visible assistant transcript is recoverable. Reasoning and
            // cursor annotations remain post-terminal projections, but the
            // final answer belongs in this exact-generation transaction with
            // canonical evidence, usage, and status.
            include_terminal_assistant: true,
        }
    }

    /// Persist the immutable root turn before execution starts.
    ///
    /// Terminal projection remains a single post-loop transaction, but an
    /// accepted turn must not disappear from audit merely because execution
    /// is cancelled, the client disconnects, or terminal persistence fails.
    /// This is one idempotent write per root run; it is never called per token
    /// or per tool event.
    pub(crate) async fn persist_turn_start(&self, state: &AgenticLoopState) -> Result<(), String> {
        let Some(pool) = self.shared_pool.as_ref() else {
            return Ok(());
        };
        let trace = server_trace_context(
            &self.user_id,
            &self.session_id,
            &self.run_id,
            state.session_turn,
        );
        let Some(event) = server_loop_user_query_event(
            &self.user_id,
            &self.session_id,
            &self.run_id,
            None,
            None,
            self.agent_id.as_deref(),
            None,
            &trace,
            &self.user_message,
            state
                .turn_event_buffer
                .as_ref()
                .map(astra_services::session_journal::TurnEventBuffer::turn_started_at)
                .unwrap_or_else(chrono::Utc::now),
        ) else {
            return Ok(());
        };
        DatabaseTraceEventWriter::new(self.matrixone.clone())
            .with_pool(pool.clone())
            .write(event)
            .await
            .map_err(|error| error.to_string())
    }

    /// Persist projections and observers after the canonical core transaction.
    ///
    /// Callers must pass the exact result of
    /// [`Self::persist_core_and_trace_in_transaction`]. This keeps the order
    /// `canonical journal -> context-head CAS -> CSL/projections` explicit.
    pub(crate) async fn run_after_core(
        &self,
        state: &AgenticLoopState,
        loop_success: bool,
        core_trace_result: Result<(), String>,
        canonical_context_persisted: bool,
    ) -> Result<(), String> {
        let mut errors = Vec::new();
        if let Err(error) = core_trace_result {
            // Hook rows, memory extraction, session-end hooks, promotion
            // events, and state projections are derived from the canonical
            // turn. Publishing any of them after the canonical transaction
            // failed creates a second, contradictory source of truth. Retain
            // the classified terminal failure and append-only provider/tool
            // evidence, but fail closed before derived state escapes.
            return Err(format!("core+trace transaction failed: {error}"));
        }

        // 1. Persist CSL via CslManager only after core+trace persistence
        // succeeds. If CSL fails later, restore can fall back to transcript
        // messages; if core+trace failed, advancing CSL would create history
        // without canonical durable events behind it.
        self.persist_csl_if_canonical_ready(state, canonical_context_persisted, &mut errors)
            .await;

        // The remaining consumers all read the immutable completed loop state
        // and write independent sinks. Streaming callers complete this phase
        // before publishing terminal SSE; the writes are awaited together so
        // their independent database latency remains overlapped.
        let hook_persist = async {
            let Some(writer) = self.hook_db_writer.as_ref() else {
                return Ok(());
            };
            persist_server_loop_hook_events(
                writer.as_ref(),
                &self.user_id,
                &self.session_id,
                &self.user_message,
                state,
                self.model_name.as_deref(),
            )
            .await
        };
        let observer_dispatch = async {
            let Some(worker) = self.observer_worker.clone() else {
                return Ok(());
            };
            fire_server_loop_observer(
                worker,
                &self.user_id,
                &self.session_id,
                state,
                self.metrics_registry.clone(),
            )
            .await
        };
        let session_end = crate::skills::hooks::fire_session_end(
            &state.skills.session_event_hooks,
            state.current_session_id.as_deref().unwrap_or(""),
        );
        let promotion_persist = persist_runtime_promotion_events(
            &self.matrixone,
            self.shared_pool.as_ref(),
            &self.user_id,
            &self.session_id,
            &self.run_id,
            &state.telemetry.promotion_events,
        );
        let projection_persist = persist_server_loop_projection_state(
            self.shared_pool.as_ref(),
            &self.user_id,
            &self.session_id,
            &self.run_id,
            self.agent_id.as_deref(),
            self.model_name.as_deref(),
            state,
        );
        let (hook_result, observer_result, (), promotion_result, projection_result) = tokio::join!(
            hook_persist,
            observer_dispatch,
            session_end,
            promotion_persist,
            projection_persist,
        );
        if let Err(error) = hook_result {
            errors.push(format!("hook events persist failed: {error}"));
        }
        if let Err(error) = observer_result {
            errors.push(format!("observer fire failed: {error}"));
        }
        if let Err(error) = promotion_result {
            errors.push(format!("promotion events persist failed: {error}"));
        }
        if let Err(error) = projection_result {
            errors.push(format!("projection state persist failed: {error}"));
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

    async fn persist_csl_if_canonical_ready(
        &self,
        state: &AgenticLoopState,
        canonical_ready: bool,
        errors: &mut Vec<String>,
    ) {
        let Some(ref mgr) = self.csl_manager else {
            return;
        };
        if !canonical_ready {
            tracing::warn!(
                session_id = %self.session_id,
                run_id = %self.run_id,
                "skipping CSL persist because canonical journal or context-head persistence failed"
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
    pub(crate) async fn persist_core_and_trace_in_transaction(
        &self,
        state: &AgenticLoopState,
    ) -> Result<(), String> {
        let Some(pool) = self.shared_pool.as_ref() else {
            // Persistence is an explicit deployment capability. An ephemeral
            // runtime performs no durable writes; this is distinct from a
            // configured database failing, which must fail closed below.
            return Ok(());
        };

        persist_server_loop_canonical_append(
            pool,
            CanonicalLoopAppend {
                user_id: &self.user_id,
                session_id: &self.session_id,
                run_id: &self.run_id,
                expected_owner_generation: self.expected_owner_generation,
                owner_lease_duration: self.owner_lease_duration,
                parent_run_id: None,
                parent_event_id: None,
                agent_id: self.agent_id.as_deref(),
                parent_agent_id: None,
                trace_context: None,
                user_message: &self.user_message,
                model_name: self.model_name.as_deref(),
                include_terminal_assistant: false,
            },
            state,
        )
        .await
        .map(|_| ())
    }

    /// Persist append-only provider/tool observations after another control
    /// request has already committed the run terminal (most commonly Ctrl+C).
    /// Terminal ownership and observed execution facts are independent: the
    /// former must not be replayed, while dropping the latter makes audit and
    /// debugging report a zero-round run.  Fence this repair by the exact
    /// owner generation and already-committed status, but do not require a
    /// live lease or rewrite canonical conversation state.
    pub(crate) async fn persist_trace_after_authoritative_terminal(
        &self,
        state: &AgenticLoopState,
        expected_status: &str,
    ) -> Result<(), String> {
        let Some(pool) = self.shared_pool.as_ref() else {
            return Ok(());
        };
        let expected_generation = self.expected_owner_generation.ok_or_else(|| {
            "terminal trace repair requires exact execution owner generation".to_string()
        })?;
        let expected_generation = i64::try_from(expected_generation)
            .map_err(|_| "execution owner generation exceeds i64".to_string())?;
        let mut tx = pool
            .get()
            .begin()
            .await
            .map_err(|error| error.to_string())?;
        astra_services::storage::admit_session_scoped_run_write(
            &mut tx,
            &self.session_id,
            &self.user_id,
            &self.run_id,
            false,
        )
        .await
        .map_err(|error| format!("terminal trace session admission failed: {error}"))?;
        let row = sqlx::query(
            "SELECT status, run_generation
             FROM agent_runs
             WHERE user_id = ? AND session_id = ? AND run_id = ?
             FOR UPDATE",
        )
        .bind(&self.user_id)
        .bind(&self.session_id)
        .bind(&self.run_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "authoritative terminal run disappeared before trace repair".to_string())?;
        let status = row
            .try_get::<String, _>("status")
            .map_err(|error| error.to_string())?;
        let generation = row
            .try_get::<i64, _>("run_generation")
            .map_err(|error| error.to_string())?;
        if status != expected_status || generation != expected_generation {
            return Err(format!(
                "terminal trace repair authority mismatch: expected status={expected_status} generation={expected_generation}, observed status={status} generation={generation}"
            ));
        }
        let (turn_started_at, _) = turn_trace_time_bounds(state);
        let deltas = persist_server_loop_trace_events_impl(
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
            turn_started_at,
        )
        .await?;
        tx.commit().await.map_err(|error| error.to_string())?;
        if let Some((delta, last_event_id)) =
            deltas.get(&(self.user_id.clone(), self.session_id.clone()))
            && *delta > 0
        {
            crate::data_layer::storage::bump_agent_session_event_count(
                pool.get(),
                &self.session_id,
                &self.user_id,
                *delta,
                last_event_id.as_deref(),
            )
            .await
            .map_err(|error| format!("bump terminal trace session event count: {error}"))?;
        }
        Ok(())
    }

    /// Atomically settle a root run's canonical DB evidence, semantic usage,
    /// and durable terminal. `Ok(None)` is reserved for deployments without a
    /// configured durable pool; those callers retain their in-memory fallback.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn persist_atomic_terminal_settlement(
        &self,
        state: &AgenticLoopState,
        expected_statuses: &[&str],
        expected_owner_generation: u64,
        status: &str,
        waiting_for: Option<&str>,
        error_message: Option<&str>,
        events: &[Value],
    ) -> Result<Option<CanonicalTerminalSettlementCommit>, String> {
        let Some(pool) = self.shared_pool.as_ref() else {
            return Ok(None);
        };
        match persist_server_loop_canonical_terminal_settlement(
            pool,
            self.atomic_terminal_canonical_append(expected_owner_generation),
            state,
            CanonicalTerminalSettlement {
                expected_statuses,
                expected_owner_generation,
                status,
                waiting_for,
                error_message,
                events,
                prompt_tokens: state.provider_input_tokens(),
                completion_tokens: state.total_completion,
                tool_calls: state.total_tool_calls,
            },
        )
        .await
        {
            Ok(commit) => Ok(Some(commit)),
            // The terminal CAS can lose to an accepted cancellation marker.
            // Rollback left no canonical loop evidence, so let the lifecycle
            // persist that evidence and use its owner-fenced fallback to
            // converge the authoritative cancelled terminal.
            Err(error) if error.contains("lost durable execution authority") => {
                let store = DatabaseRunStateStore::new(pool.clone());
                let control = store.load_run_control(&self.user_id, &self.run_id).await?;
                let snapshot = store
                    .load_run_status_snapshot(&self.user_id, &self.run_id)
                    .await?;
                let cancellation_won = control.zip(snapshot).is_some_and(|(control, snapshot)| {
                    snapshot.run_generation == expected_owner_generation
                        && matches!(
                            control.status.as_str(),
                            STATUS_RUNNING | STATUS_WAITING | STATUS_PAUSED
                        )
                        && control.cancellation_requested
                });
                if cancellation_won {
                    Ok(None)
                } else {
                    Err(error)
                }
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) async fn materialize_run_transcript_evidence(
        &self,
        state: &AgenticLoopState,
        canonical_cursor: Option<&SessionCursorV1>,
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
            canonical_cursor,
        )
        .await
    }
}

pub(crate) struct CanonicalLoopAppend<'a> {
    pub(crate) user_id: &'a str,
    pub(crate) session_id: &'a str,
    pub(crate) run_id: &'a str,
    pub(crate) expected_owner_generation: Option<u64>,
    pub(crate) owner_lease_duration: Option<Duration>,
    pub(crate) parent_run_id: Option<&'a str>,
    /// Causal parent event is independent from run ownership. Delegated
    /// producers receive their own local turn root while retaining this edge
    /// to the parent turn that created them.
    pub(crate) parent_event_id: Option<&'a str>,
    pub(crate) agent_id: Option<&'a str>,
    pub(crate) parent_agent_id: Option<&'a str>,
    pub(crate) trace_context: Option<TraceContext>,
    pub(crate) user_message: &'a str,
    pub(crate) model_name: Option<&'a str>,
    pub(crate) include_terminal_assistant: bool,
}

#[derive(Clone, Copy)]
pub(crate) struct CanonicalTerminalSettlement<'a> {
    pub(crate) expected_statuses: &'a [&'a str],
    pub(crate) expected_owner_generation: u64,
    pub(crate) status: &'a str,
    pub(crate) waiting_for: Option<&'a str>,
    pub(crate) error_message: Option<&'a str>,
    pub(crate) events: &'a [Value],
    pub(crate) prompt_tokens: u64,
    pub(crate) completion_tokens: u64,
    pub(crate) tool_calls: u32,
}

#[derive(Clone, Debug)]
pub(crate) struct CanonicalTerminalSettlementCommit {
    pub(crate) terminal_events: Vec<Value>,
    pub(crate) terminal_assistant_source_event_id: Option<String>,
}

fn should_acknowledge_runner_continuations(status: &str) -> bool {
    astra_services::runs::durable_run_status_is_terminal(status)
}

fn atomic_terminal_request<'a>(
    append: &'a CanonicalLoopAppend<'a>,
    settlement: CanonicalTerminalSettlement<'a>,
) -> AtomicRunTerminalSettlementRequest<'a> {
    AtomicRunTerminalSettlementRequest {
        user_id: append.user_id,
        run_id: append.run_id,
        expected_session_id: append.session_id,
        expected_statuses: settlement.expected_statuses,
        expected_owner_generation: settlement.expected_owner_generation,
        status: settlement.status,
        waiting_for: settlement.waiting_for,
        error_message: settlement.error_message,
        events: settlement.events,
        prompt_tokens: settlement.prompt_tokens,
        completion_tokens: settlement.completion_tokens,
        tool_calls: settlement.tool_calls,
    }
}

async fn verify_canonical_append_evidence(
    pool: &SharedPool,
    append: &CanonicalLoopAppend<'_>,
    state: &AgenticLoopState,
) -> Result<(), String> {
    let trace = append.trace_context.clone().unwrap_or_else(|| {
        server_trace_context(
            append.user_id,
            append.session_id,
            append.run_id,
            state.session_turn,
        )
    });
    let (turn_started_at, terminal_offset_ms) = turn_trace_time_bounds(state);
    let mut expected_trace = build_server_loop_core_events(
        append.user_id,
        append.session_id,
        append.run_id,
        append.parent_run_id,
        append.parent_event_id,
        append.agent_id,
        append.parent_agent_id,
        Some(trace.clone()),
        append.user_message,
        state,
        append.model_name,
        turn_started_at,
        terminal_offset_ms,
    );
    expected_trace.extend(build_llm_round_trace_events(
        &trace,
        turn_started_at,
        append.run_id,
        append.parent_run_id,
        append.agent_id,
        append.parent_agent_id,
        append.model_name,
        &state.recent_rounds,
    ));
    expected_trace.extend(build_tool_trace_events(
        &trace,
        turn_started_at,
        append.run_id,
        append.parent_run_id,
        append.agent_id,
        append.parent_agent_id,
        &state.stall.tool_call_records,
    ));
    for expected in expected_trace {
        let row = sqlx::query(
            "SELECT event_type, run_id, content, reasoning_content
             FROM agent_events
             WHERE user_id = ? AND session_id = ? AND event_id = ?
             LIMIT 1",
        )
        .bind(append.user_id)
        .bind(append.session_id)
        .bind(&expected.event_id)
        .fetch_optional(pool.get())
        .await
        .map_err(|error| {
            format!(
                "resolve canonical trace evidence {}: {error}",
                expected.event_id
            )
        })?;
        let Some(row) = row else {
            return Err(format!(
                "canonical trace evidence {} is missing",
                expected.event_id
            ));
        };
        let event_type = row
            .try_get::<String, _>("event_type")
            .map_err(|error| format!("decode canonical trace type: {error}"))?;
        let run_id = row
            .try_get::<Option<String>, _>("run_id")
            .map_err(|error| format!("decode canonical trace run: {error}"))?;
        let content = row
            .try_get::<Option<String>, _>("content")
            .map_err(|error| format!("decode canonical trace content: {error}"))?;
        let reasoning_content = row
            .try_get::<Option<String>, _>("reasoning_content")
            .map_err(|error| format!("decode canonical trace reasoning: {error}"))?;
        if event_type != expected.event_type
            || run_id.as_deref() != Some(append.run_id)
            || content != expected.content
            || reasoning_content != expected.reasoning_content
        {
            return Err(format!(
                "canonical trace evidence {} conflicts with replay",
                expected.event_id
            ));
        }
    }

    let expected_transcript = transcript_items_from_server_loop(
        append.user_id,
        append.session_id,
        append.run_id,
        Some(&trace),
        append.user_message,
        state,
        append.include_terminal_assistant,
    );
    let reasoning_source_event_id = append
        .include_terminal_assistant
        .then(|| {
            terminal_assistant_transcript_item(
                append.user_id,
                append.session_id,
                append.run_id,
                Some(&trace),
                append.user_message,
                state,
            )
            .map(|item| item.source_event_id)
        })
        .flatten();
    let reasoning_projection = if reasoning_source_event_id.is_some() {
        load_durable_run_transcript_projection(
            pool,
            append.user_id,
            append.session_id,
            append.run_id,
        )
        .await?
        .reasoning
    } else {
        TranscriptReasoningProjection::default()
    };
    for expected in expected_transcript {
        let row = sqlx::query(
            "SELECT source_event_id, role, content, payload_json, content_hash, run_id
             FROM session_transcript_items
             WHERE user_id = ? AND session_id = ? AND source_event_id = ?
             LIMIT 1",
        )
        .bind(append.user_id)
        .bind(append.session_id)
        .bind(&expected.source_event_id)
        .fetch_optional(pool.get())
        .await
        .map_err(|error| {
            format!(
                "resolve canonical transcript evidence {}: {error}",
                expected.source_event_id
            )
        })?;
        let Some(row) = row else {
            return Err(format!(
                "canonical transcript evidence {} is missing",
                expected.source_event_id
            ));
        };
        let actual = StoredCanonicalTranscriptEvidence {
            source_event_id: row
                .try_get::<String, _>("source_event_id")
                .map_err(|error| format!("decode canonical transcript source: {error}"))?,
            role: row
                .try_get::<String, _>("role")
                .map_err(|error| format!("decode canonical transcript role: {error}"))?,
            content: row
                .try_get::<String, _>("content")
                .map_err(|error| format!("decode canonical transcript content: {error}"))?,
            payload_json: row
                .try_get::<Option<String>, _>("payload_json")
                .map_err(|error| format!("decode canonical transcript payload: {error}"))?,
            content_hash: row
                .try_get::<String, _>("content_hash")
                .map_err(|error| format!("decode canonical transcript hash: {error}"))?,
            run_id: row
                .try_get::<Option<String>, _>("run_id")
                .map_err(|error| format!("decode canonical transcript run: {error}"))?,
        };
        if !stored_transcript_matches_canonical_or_reasoning_projection(
            &expected,
            &actual,
            append.run_id,
            reasoning_source_event_id.as_deref(),
            &reasoning_projection,
        )? {
            return Err(format!(
                "canonical transcript evidence {} conflicts with replay",
                expected.source_event_id
            ));
        }
    }
    Ok(())
}

async fn resolve_existing_atomic_terminal_settlement(
    pool: &SharedPool,
    append: &CanonicalLoopAppend<'_>,
    state: &AgenticLoopState,
    settlement: CanonicalTerminalSettlement<'_>,
    expected_receipts: Option<&[AtomicRunTerminalEventReceipt]>,
) -> Result<
    Option<(
        DatabaseRunStateStore,
        astra_services::runs::AtomicRunTerminalSettlementCommit,
    )>,
    String,
> {
    let store = DatabaseRunStateStore::new(pool.clone());
    let resolution = store
        .resolve_atomic_terminal_settlement(
            atomic_terminal_request(append, settlement),
            expected_receipts,
        )
        .await?;
    let Some(commit) = classify_authoritative_terminal_resolution(resolution)? else {
        return Ok(None);
    };
    verify_canonical_append_evidence(pool, append, state).await?;
    Ok(Some((store, commit)))
}

fn classify_authoritative_terminal_resolution(
    resolution: AtomicRunTerminalSettlementResolution,
) -> Result<Option<astra_services::runs::AtomicRunTerminalSettlementCommit>, String> {
    match resolution {
        AtomicRunTerminalSettlementResolution::NotCommitted => Ok(None),
        AtomicRunTerminalSettlementResolution::Conflict(reason) => Err(format!(
            "authoritative atomic terminal resolution conflict: {reason}"
        )),
        AtomicRunTerminalSettlementResolution::Exact(commit) => Ok(Some(commit)),
    }
}

async fn finish_authoritatively_resolved_terminal(
    store: &DatabaseRunStateStore,
    terminal: astra_services::runs::AtomicRunTerminalSettlementCommit,
    append: &CanonicalLoopAppend<'_>,
    state: &AgenticLoopState,
    log_message: &'static str,
) -> CanonicalTerminalSettlementCommit {
    if let Err(error) = store
        .repair_projection_after_atomic_terminal_settlement(
            append.user_id,
            append.session_id,
            append.run_id,
        )
        .await
    {
        tracing::warn!(
            target: "astra_runtime::run_lifecycle",
            user_id = append.user_id,
            run_id = append.run_id,
            error = %error,
            "{log_message}"
        );
    }
    CanonicalTerminalSettlementCommit {
        terminal_events: terminal.committed_events,
        terminal_assistant_source_event_id: append
            .include_terminal_assistant
            .then(|| {
                terminal_assistant_transcript_item(
                    append.user_id,
                    append.session_id,
                    append.run_id,
                    append.trace_context.as_ref(),
                    append.user_message,
                    state,
                )
                .map(|item| item.source_event_id)
            })
            .flatten(),
    }
}

/// Append the complete durable loop record through one transaction for roots
/// and delegated agents alike. There is deliberately no independent-write
/// fallback: a failed append must not leave a plausible partial history.
pub(crate) async fn persist_server_loop_canonical_append(
    pool: &SharedPool,
    append: CanonicalLoopAppend<'_>,
    state: &AgenticLoopState,
) -> Result<Option<String>, String> {
    persist_server_loop_canonical_append_inner(pool, append, state, None)
        .await
        .map(|commit| commit.terminal_assistant_source_event_id)
}

/// Atomically commit canonical loop evidence, semantic run usage, and the
/// exact-generation durable terminal. Derived projections run only after the
/// database commit and cannot reverse it.
pub(crate) async fn persist_server_loop_canonical_terminal_settlement(
    pool: &SharedPool,
    append: CanonicalLoopAppend<'_>,
    state: &AgenticLoopState,
    settlement: CanonicalTerminalSettlement<'_>,
) -> Result<CanonicalTerminalSettlementCommit, String> {
    persist_server_loop_canonical_append_inner(pool, append, state, Some(settlement)).await
}

async fn persist_server_loop_canonical_append_inner(
    pool: &SharedPool,
    append: CanonicalLoopAppend<'_>,
    state: &AgenticLoopState,
    settlement: Option<CanonicalTerminalSettlement<'_>>,
) -> Result<CanonicalTerminalSettlementCommit, String> {
    if let Some(settlement) = settlement {
        if append.expected_owner_generation != Some(settlement.expected_owner_generation) {
            return Err(
                "canonical append and terminal settlement disagree on execution generation"
                    .to_string(),
            );
        }
    }
    let mut tx = match pool.get().begin().await {
        Ok(tx) => tx,
        Err(error) => {
            let msg = format!("failed to begin MO transaction: {}", error);
            tracing::warn!(
                session_id = %append.session_id,
                error = %error,
                "post-loop: failed to begin canonical MO transaction; no persistence attempted"
            );
            return Err(msg);
        }
    };
    astra_services::storage::admit_session_scoped_run_write(
        &mut tx,
        append.session_id,
        append.user_id,
        append.run_id,
        false,
    )
    .await
    .map_err(|error| format!("canonical session admission failed: {error}"))?;

    if let Some(expected_owner_generation) = append.expected_owner_generation {
        let Some(lease_duration) = append.owner_lease_duration else {
            let _ = tx.rollback().await;
            return Err(
                "canonical append has execution authority but no durable lease duration"
                    .to_string(),
            );
        };
        let lease_micros = i64::try_from(lease_duration.as_micros()).unwrap_or(i64::MAX);
        let owner_fence_sql =
            if settlement.is_some_and(|settlement| settlement.status == STATUS_CANCELLED) {
                "UPDATE agent_runs
             SET owner_lease_expires_at = DATE_ADD(NOW(6), INTERVAL ? MICROSECOND)
             WHERE user_id = ? AND run_id = ? AND run_generation = ?
               AND owner_lease_expires_at >= NOW(6)
               AND status IN ('running', 'waiting', 'paused')"
            } else {
                "UPDATE agent_runs
             SET owner_lease_expires_at = DATE_ADD(NOW(6), INTERVAL ? MICROSECOND)
             WHERE user_id = ? AND run_id = ? AND run_generation = ?
               AND owner_lease_expires_at >= NOW(6)
               AND status IN ('running', 'waiting')"
            };
        let fenced = sqlx::query(owner_fence_sql)
            .bind(lease_micros)
            .bind(append.user_id)
            .bind(append.run_id)
            .bind(expected_owner_generation as i64)
            .execute(&mut *tx)
            .await
            .map_err(|error| format!("canonical execution-authority fence failed: {error}"))?;
        if fenced.rows_affected() == 0 {
            let _ = tx.rollback().await;
            if let Some(settlement) = settlement
                && let Some((store, terminal)) = resolve_existing_atomic_terminal_settlement(
                    pool, &append, state, settlement, None,
                )
                .await?
            {
                return Ok(finish_authoritatively_resolved_terminal(
                    &store,
                    terminal,
                    &append,
                    state,
                    "replayed canonical terminal is authoritative but display projection repair failed",
                )
                .await);
            }
            return Err(format!(
                "canonical append lost durable execution authority at generation {expected_owner_generation}"
            ));
        }
    }

    // Core events (user_query + llm_response) + transcript items.
    //
    // `persist_server_loop_core_events_in_tx` now returns `Result`; on Err the
    // transaction is poisoned (partial writes may be staged) and we MUST
    // rollback instead of continuing to write detail events into the same tx.
    let (turn_started_at, terminal_offset_ms) = turn_trace_time_bounds(state);
    let mut session_event_deltas = match persist_server_loop_core_events_in_tx(
        &mut tx,
        append.user_id,
        append.session_id,
        append.run_id,
        append.parent_run_id,
        append.parent_event_id,
        append.agent_id,
        append.parent_agent_id,
        append.trace_context.clone(),
        append.user_message,
        state,
        append.model_name,
        turn_started_at,
        terminal_offset_ms,
    )
    .await
    {
        Ok(deltas) => deltas,
        Err(error) => {
            let msg = format!("core events tx failed: {}", error);
            tracing::warn!(
                session_id = %append.session_id,
                error = %error,
                "post-loop: core events tx failed, rolling back MO transaction"
            );
            // rollback consumes the transaction; cannot use tx after this
            if let Err(rollback_err) = tx.rollback().await {
                tracing::error!(
                    session_id = %append.session_id,
                    error = %rollback_err,
                    "post-loop: rollback also failed after core events tx error"
                );
            }
            return Err(msg);
        }
    };

    // Trace detail events (LLM rounds, tool calls).
    match persist_server_loop_trace_events_in_tx(
        &mut tx,
        append.user_id,
        append.session_id,
        append.run_id,
        append.parent_run_id,
        append.agent_id,
        append.parent_agent_id,
        append.trace_context.clone(),
        state,
        append.model_name,
        turn_started_at,
    )
    .await
    {
        Ok(deltas) => {
            for (key, (delta, last_event_id)) in deltas {
                let entry = session_event_deltas.entry(key).or_default();
                entry.0 += delta;
                if last_event_id.is_some() {
                    entry.1 = last_event_id;
                }
            }
        }
        Err(error) => {
            let msg = format!("detail events tx failed: {}", error);
            tracing::warn!(
                session_id = %append.session_id,
                error = %error,
                "post-loop: detail events tx failed, rolling back MO transaction"
            );
            if let Err(rb_err) = tx.rollback().await {
                tracing::error!(
                    session_id = %append.session_id,
                    error = %rb_err,
                    "post-loop: rollback failed after detail events tx failure"
                );
            }
            return Err(msg);
        }
    }

    // The transcript gets one ordered durable sequence in this same
    // transaction. Delegated runs include their terminal assistant here;
    // roots defer it until the canonical context cursor is committed.
    if let Err(error) = persist_server_loop_transcript_items_in_tx(
        &mut tx,
        append.user_id,
        append.session_id,
        append.run_id,
        append.trace_context.as_ref(),
        append.user_message,
        state,
        append.include_terminal_assistant,
    )
    .await
    {
        let msg = format!("transcript items tx failed: {error}");
        tracing::warn!(
            session_id = %append.session_id,
            error = %error,
            "post-loop: transcript item persistence failed, rolling back MO transaction"
        );
        if let Err(rb_err) = tx.rollback().await {
            tracing::error!(
                session_id = %append.session_id,
                error = %rb_err,
                "post-loop: rollback failed after transcript item tx failure"
            );
        }
        return Err(msg);
    }

    let terminal_commit = if let Some(settlement) = settlement {
        let store = DatabaseRunStateStore::new(pool.clone());
        match store
            .settle_terminal_in_existing_transaction(
                &mut tx,
                atomic_terminal_request(&append, settlement),
            )
            .await
        {
            Ok(Some(commit)) => {
                if should_acknowledge_runner_continuations(settlement.status)
                    && let Err(error) = astra_services::inference_execution::runner::acknowledge_runner_continuations_for_terminal_run_tx(
                    &mut tx,
                    append.user_id,
                    append.session_id,
                    append.run_id,
                    settlement.expected_owner_generation,
                )
                .await
                {
                    let rollback_error = tx.rollback().await.err();
                    return Err(if let Some(rollback_error) = rollback_error {
                        format!(
                            "acknowledge Runner continuations failed: {error}; rollback also failed: {rollback_error}"
                        )
                    } else {
                        format!("acknowledge Runner continuations failed: {error}")
                    });
                }
                Some((store, commit))
            }
            Ok(None) => {
                let _ = tx.rollback().await;
                return match resolve_existing_atomic_terminal_settlement(
                    pool, &append, state, settlement, None,
                )
                .await?
                {
                    Some((store, terminal)) => Ok(finish_authoritatively_resolved_terminal(
                        &store,
                        terminal,
                        &append,
                        state,
                        "concurrent exact terminal is authoritative but display projection repair failed",
                    )
                    .await),
                    None => Err(format!(
                        "canonical terminal settlement lost durable execution authority at generation {}",
                        settlement.expected_owner_generation
                    )),
                };
            }
            Err(error) => {
                let rollback_error = tx.rollback().await.err();
                return Err(if let Some(rollback_error) = rollback_error {
                    format!(
                        "canonical terminal settlement failed: {error}; rollback also failed: {rollback_error}"
                    )
                } else {
                    format!("canonical terminal settlement failed: {error}")
                });
            }
        }
    } else {
        None
    };

    // This commit is authoritative. A failure is surfaced to the lifecycle;
    // there is no independent-write retry path.
    if let Err(error) = tx.commit().await {
        let msg = format!("MO transaction commit acknowledgement failed: {error}");
        if let (Some(settlement), Some((_, staged_terminal))) =
            (settlement, terminal_commit.as_ref())
        {
            match resolve_existing_atomic_terminal_settlement(
                pool,
                &append,
                state,
                settlement,
                Some(&staged_terminal.event_receipts),
            )
            .await
            {
                Ok(Some(_)) => {
                    tracing::warn!(
                        session_id = %append.session_id,
                        run_id = %append.run_id,
                        error = %error,
                        "post-loop: commit acknowledgement was lost; exact durable settlement resolved authoritatively"
                    );
                }
                Ok(None) => return Err(msg),
                Err(resolve_error) => {
                    return Err(format!(
                        "{msg}; authoritative commit resolution failed: {resolve_error}"
                    ));
                }
            }
        } else {
            tracing::warn!(
                session_id = %append.session_id,
                error = %error,
                "post-loop: MO transaction commit acknowledgement failed"
            );
            return Err(msg);
        }
    }
    // Event rows and run state are now durable. Update the derived session
    // counter outside the long canonical transaction so sibling fanout runs
    // never wait on `agent_sessions` while holding their own event/run locks.
    if let Some((delta, last_event_id)) =
        session_event_deltas.get(&(append.user_id.to_string(), append.session_id.to_string()))
        && *delta > 0
    {
        crate::data_layer::storage::bump_agent_session_event_count(
            pool.get(),
            append.session_id,
            append.user_id,
            *delta,
            last_event_id.as_deref(),
        )
        .await
        .map_err(|error| format!("bump canonical session event count: {error}"))?;
    }
    if let Some((store, _)) = terminal_commit.as_ref()
        && let Err(error) = store
            .repair_projection_after_atomic_terminal_settlement(
                append.user_id,
                append.session_id,
                append.run_id,
            )
            .await
    {
        tracing::warn!(
            target: "astra_runtime::run_lifecycle",
            user_id = append.user_id,
            run_id = append.run_id,
            error = %error,
            "canonical terminal settlement committed but display projection repair failed"
        );
    }
    let terminal_assistant_source_event_id = append
        .include_terminal_assistant
        .then(|| {
            terminal_assistant_transcript_item(
                append.user_id,
                append.session_id,
                append.run_id,
                append.trace_context.as_ref(),
                append.user_message,
                state,
            )
            .map(|item| item.source_event_id)
        })
        .flatten();
    Ok(CanonicalTerminalSettlementCommit {
        terminal_events: terminal_commit
            .map(|(_, terminal)| terminal.committed_events)
            .unwrap_or_default(),
        terminal_assistant_source_event_id,
    })
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
    let activated_deferred_tool_names = state
        .runtime_tool_executor
        .as_deref()
        .map(crate::server::runtime_tool_executor::RuntimeToolExecutor::activated_deferred_tool_names)
        .unwrap_or_else(|| state.activated_deferred_tool_names.clone());
    astra_turn_core::conversation_log::SessionStateCompact {
        source_cursor: None,
        // CSL is conversation materialization, not execution policy. Persisting
        // transient restrictions, approvals, interruptions, budgets, or
        // compaction pressure here makes old materialized state hard-steer later
        // turns. Deferred activation is different: it records which full
        // schemas the retained prompt has already materialized and remains
        // subject to the current surface and runtime bindings on restore.
        blocked_tools: Vec::new(),
        recent_tools: state.recent_tools.clone(),
        activated_deferred_tool_names,
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
    // Checkpoints and CSL are independent recovery projections. A compacted
    // transcript can legitimately predate a deferred-tool selection that the
    // step checkpoint already restored, so CSL must only contribute durable
    // activation facts; it must never erase a newer one.
    let mut activated = std::mem::take(&mut loop_state.activated_deferred_tool_names);
    activated.extend(ss.activated_deferred_tool_names);
    loop_state.activated_deferred_tool_names =
        astra_turn_core::tool::deferred_activation::merged_activated_tool_names(&[], activated);
    // Intentionally ignore all actual runtime-control fields in
    // SessionStateCompact. Older CSL records may contain them, but restoring
    // them here would leak stale pauses, approvals, budget pressure, and
    // compaction failures into a new user turn.
}

pub(crate) fn restore_step_checkpoint_runtime_state(
    restored: astra_pipeline::step_restore::RestoredSession,
    current_date: &str,
    loop_state: &mut AgenticLoopState,
) {
    let mut persisted_activation = std::mem::take(&mut loop_state.activated_deferred_tool_names);
    persisted_activation.extend(restored.activated_deferred_tool_names.clone());
    loop_state.activated_deferred_tool_names =
        astra_turn_core::tool::deferred_activation::merged_activated_tool_names(
            &restored.messages,
            persisted_activation,
        );
    if !restored.cache_restore_report.journal_complete {
        tracing::warn!(
            target: "astra_runtime::recovery",
            journal_bytes_read = restored.cache_restore_report.journal_bytes_read,
            events_examined = restored.cache_restore_report.events_examined,
            prefix_truncated = restored.cache_restore_report.prefix_truncated,
            events_dropped = restored.cache_restore_report.events_dropped,
            trailing_torn_line = restored.cache_restore_report.trailing_torn_line,
            degraded_reason = ?restored.cache_restore_report.degraded_reason,
            "restored session has an explicitly degraded completed-tool audit projection"
        );
    }
    if restored.cache_restore_report.rejected_unverified_entries > 0 {
        tracing::debug!(
            target: "astra_runtime::recovery",
            rejected_unverified_entries = restored.cache_restore_report.rejected_unverified_entries,
            rejected_context_bound_entries = restored
                .cache_restore_report
                .rejected_context_bound_entries,
            "restored tool-result audit remains non-executable by design"
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
    if restored.workspace_observation_quarantine.is_some() {
        loop_state.stall.workspace_observation_quarantine =
            restored.workspace_observation_quarantine;
    }
    loop_state.consecutive_context_window_errors = restored.consecutive_context_window_errors;
    // Do not derive these cursors from the attempt ledger.  The checkpoint is
    // the only durable statement that its conversation has absorbed exactly
    // these provider rounds; attempt rows can include retries, continuations,
    // and work from a later executor.
    loop_state.llm_rounds_completed = restored.llm_rounds_completed;
    loop_state.current_round_index = restored.current_round_index;
    loop_state.stall.runner_continuation_receipts = restored.runner_continuation_receipts;
    loop_state.stall.restored_from_heavy_checkpoint = true;
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

pub(crate) fn messages_for_csl_persist(state: &AgenticLoopState) -> Vec<Value> {
    let mut messages = state.messages.clone();
    if astra_core::history_work::instrumentation_enabled() {
        let (bytes, rows) = json_history_payload_work(&messages);
        astra_core::history_work::record_operation(
            astra_core::history_work::HistoryWorkSite::ServerCslPersistClone,
            bytes,
            rows,
            0,
        );
    }
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

/// Count cloned heap payload without allocating a second serialized history.
fn json_value_payload_bytes(value: &Value) -> u64 {
    match value {
        Value::Null => 0,
        Value::Bool(_) => 1,
        Value::Number(_) => {
            u64::try_from(std::mem::size_of::<serde_json::Number>()).unwrap_or(u64::MAX)
        }
        Value::String(value) => value.len().try_into().unwrap_or(u64::MAX),
        Value::Array(values) => values.iter().fold(0_u64, |bytes, value| {
            bytes.saturating_add(json_value_payload_bytes(value))
        }),
        Value::Object(values) => json_map_payload_bytes(values),
    }
}

fn json_map_payload_bytes(values: &Map<String, Value>) -> u64 {
    values.iter().fold(0_u64, |bytes, (key, value)| {
        bytes
            .saturating_add(key.len().try_into().unwrap_or(u64::MAX))
            .saturating_add(json_value_payload_bytes(value))
    })
}

fn json_history_payload_work(messages: &[Value]) -> (u64, u64) {
    (
        messages.iter().fold(0_u64, |bytes, message| {
            bytes.saturating_add(json_value_payload_bytes(message))
        }),
        messages.len().try_into().unwrap_or(u64::MAX),
    )
}

fn server_observer_request_retained_bytes(request: &TurnObserverRequest) -> u64 {
    request
        .messages
        .iter()
        .fold(
            request
                .user_id
                .len()
                .saturating_add(request.session_id.len())
                .try_into()
                .unwrap_or(u64::MAX),
            |bytes, message| bytes.saturating_add(json_map_payload_bytes(message)),
        )
        .saturating_add(
            request
                .session_start
                .as_ref()
                .map_or(0, json_value_payload_bytes),
        )
        .saturating_add(
            u64::try_from(std::mem::size_of_val(&request.turn_count)).unwrap_or(u64::MAX),
        )
}

fn reserve_server_observer_request(
    request: &TurnObserverRequest,
) -> Option<astra_core::history_work::QueueBytesReservation> {
    reserve_server_observer_request_when(
        request,
        astra_core::history_work::instrumentation_enabled(),
    )
}

fn reserve_server_observer_request_when(
    request: &TurnObserverRequest,
    instrumentation_enabled: bool,
) -> Option<astra_core::history_work::QueueBytesReservation> {
    instrumentation_enabled.then(|| {
        astra_core::history_work::QueueBytesReservation::for_site(
            astra_core::history_work::HistoryWorkSite::ServerObserverQueue,
            server_observer_request_retained_bytes(request),
        )
    })
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
    // Turn identity participates in durable event keys. A display-oriented
    // prefix is not an identity: UUIDv7 siblings commonly share their leading
    // timestamp bytes, and arbitrary run ids may normalize to the same text.
    // Hash the complete producer run id into a bounded stable identifier.
    format!("turn-{}", trace_hash(&[run_id]))
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
        // Root-turn persistence is invoked at both admission and terminal
        // settlement. The causal chain must therefore be a stable turn
        // identity, not a fresh UUID per persistence pass; otherwise the
        // user_query and llm_response rows become disconnected evidence.
        causal_chain_id: format!(
            "server-loop:{}",
            trace_hash(&[session_id, run_id, &turn_id])
        ),
        session_id: session_id.to_string(),
        user_id: user_id.to_string(),
        turn_id,
        turn_seq: i64::from(turn_seq.max(1)),
    }
}

pub(crate) fn trace_context_from_subrun_context(
    context: &HashMap<String, Value>,
    run_id: &str,
) -> Option<TraceContext> {
    let session_id = context.get("trace_session_id")?.as_str()?.to_string();
    let turn_seq = context.get("trace_turn_seq")?.as_i64()?;
    let turn_id = server_turn_id(run_id);
    Some(TraceContext {
        root_event_id: trace_event_id("user", &[&session_id, &turn_id]),
        session_id,
        user_id: context.get("trace_user_id")?.as_str()?.to_string(),
        turn_id,
        turn_seq,
        causal_chain_id: context.get("trace_causal_chain_id")?.as_str()?.to_string(),
    })
}

fn turn_trace_time_bounds(state: &AgenticLoopState) -> (chrono::DateTime<chrono::Utc>, u64) {
    let latest_round_end = state
        .recent_rounds
        .iter()
        .map(|round| round.start_offset_ms.saturating_add(round.duration_ms))
        .max()
        .unwrap_or_default();
    let latest_tool_end = state
        .stall
        .tool_call_records
        .iter()
        .map(|record| {
            record
                .start_offset_ms
                .unwrap_or_default()
                .saturating_add(record.ms)
        })
        .max()
        .unwrap_or_default();
    let buffer_offset = state
        .turn_event_buffer
        .as_ref()
        .map(astra_services::session_journal::TurnEventBuffer::offset_ms)
        .unwrap_or_default();
    let terminal_offset_ms = latest_round_end.max(latest_tool_end).max(buffer_offset);
    let turn_started_at = state
        .turn_event_buffer
        .as_ref()
        .map(astra_services::session_journal::TurnEventBuffer::turn_started_at)
        .unwrap_or_else(|| {
            chrono::Utc::now()
                - chrono::Duration::milliseconds(
                    i64::try_from(terminal_offset_ms).unwrap_or(i64::MAX),
                )
        });
    (turn_started_at, terminal_offset_ms)
}

#[allow(clippy::too_many_arguments)]
fn server_loop_user_query_event(
    user_id: &str,
    session_id: &str,
    run_id: &str,
    parent_run_id: Option<&str>,
    parent_event_id: Option<&str>,
    agent_id: Option<&str>,
    parent_agent_id: Option<&str>,
    trace: &TraceContext,
    user_message: &str,
    created_at: chrono::DateTime<chrono::Utc>,
) -> Option<TraceEvent> {
    if user_message.is_empty() {
        return None;
    }
    let mut event = TraceEvent::new(
        trace.root_event_id.clone(),
        session_id,
        user_id,
        "user_query",
        "turn",
    )
    .with_turn_context(trace);
    event.run_id = Some(run_id.to_string());
    event.parent_run_id = parent_run_id.map(ToString::to_string);
    event.parent_event_id = parent_event_id.map(ToString::to_string);
    event.agent_id = Some(agent_id.unwrap_or("root-agent").to_string());
    event.parent_agent_id = parent_agent_id.map(ToString::to_string);
    event.content = Some(user_message.to_string());
    event.created_at = created_at;
    Some(event)
}

/// Transactional variant: uses the provided transaction for all writes instead
/// of creating its own. The caller owns commit/rollback.
pub(crate) async fn persist_server_loop_core_events_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    user_id: &str,
    session_id: &str,
    run_id: &str,
    parent_run_id: Option<&str>,
    parent_event_id: Option<&str>,
    agent_id: Option<&str>,
    parent_agent_id: Option<&str>,
    trace_context: Option<TraceContext>,
    user_message: &str,
    state: &AgenticLoopState,
    model_name: Option<&str>,
    turn_started_at: chrono::DateTime<chrono::Utc>,
    terminal_offset_ms: u64,
) -> Result<std::collections::BTreeMap<(String, String), (i64, Option<String>)>, String> {
    persist_server_loop_core_events_impl(
        tx,
        user_id,
        session_id,
        run_id,
        parent_run_id,
        parent_event_id,
        agent_id,
        parent_agent_id,
        trace_context,
        user_message,
        state,
        model_name,
        turn_started_at,
        terminal_offset_ms,
    )
    .await
}

async fn persist_server_loop_core_events_impl(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    user_id: &str,
    session_id: &str,
    run_id: &str,
    parent_run_id: Option<&str>,
    parent_event_id: Option<&str>,
    agent_id: Option<&str>,
    parent_agent_id: Option<&str>,
    trace_context: Option<TraceContext>,
    user_message: &str,
    state: &AgenticLoopState,
    model_name: Option<&str>,
    turn_started_at: chrono::DateTime<chrono::Utc>,
    terminal_offset_ms: u64,
) -> Result<std::collections::BTreeMap<(String, String), (i64, Option<String>)>, String> {
    let events = build_server_loop_core_events(
        user_id,
        session_id,
        run_id,
        parent_run_id,
        parent_event_id,
        agent_id,
        parent_agent_id,
        trace_context,
        user_message,
        state,
        model_name,
        turn_started_at,
        terminal_offset_ms,
    );
    if events.is_empty() {
        return Ok(std::collections::BTreeMap::new());
    }

    match DatabaseTraceEventWriter::write_many_in_tx(tx, events).await {
        Ok(deltas) => Ok(deltas),
        Err(e) => {
            astra_core::agent_error!(
                "server-loop",
                "failed to persist core events (in tx) for session {session_id}: {e}"
            );
            // Transaction is poisoned; caller must rollback. Do not keep writing
            // transcript items into a dirty transaction.
            Err(e.to_string())
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn build_server_loop_core_events(
    user_id: &str,
    session_id: &str,
    run_id: &str,
    parent_run_id: Option<&str>,
    parent_event_id: Option<&str>,
    agent_id: Option<&str>,
    parent_agent_id: Option<&str>,
    trace_context: Option<TraceContext>,
    user_message: &str,
    state: &AgenticLoopState,
    model_name: Option<&str>,
    turn_started_at: chrono::DateTime<chrono::Utc>,
    terminal_offset_ms: u64,
) -> Vec<TraceEvent> {
    if user_message.is_empty()
        && state.final_text.is_empty()
        && state.user_intents.applied_user_intents().is_empty()
    {
        return Vec::new();
    }

    let trace = trace_context
        .unwrap_or_else(|| server_trace_context(user_id, session_id, run_id, state.session_turn));
    let user_query_event = server_loop_user_query_event(
        user_id,
        session_id,
        run_id,
        parent_run_id,
        parent_event_id,
        agent_id,
        parent_agent_id,
        &trace,
        user_message,
        turn_started_at,
    );

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
            // Deferred intents currently retain order but not a producer
            // timestamp. Anchor them to the turn root rather than fabricating
            // a post-loop time that would sort after the terminal response.
            event.created_at = turn_started_at;
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
        event.created_at = turn_started_at
            + chrono::Duration::milliseconds(i64::try_from(terminal_offset_ms).unwrap_or(i64::MAX));
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

    events
}

pub(crate) struct TranscriptPersistItem {
    /// Durable run ownership when the producer participates in the server run
    /// lifecycle. CLI bridge turns deliberately use `None`: their local run
    /// identity is not an `agent_runs` row, and attaching an orphan id would
    /// make the root-conversation transcript silently filter the item out.
    pub(crate) run_id: Option<String>,
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

#[derive(Clone, Debug)]
struct StoredCanonicalTranscriptEvidence {
    source_event_id: String,
    run_id: Option<String>,
    role: String,
    content: String,
    payload_json: Option<String>,
    content_hash: String,
}

#[derive(Default)]
struct DurableRunTranscriptProjection {
    reasoning: TranscriptReasoningProjection,
    evidence_items: Vec<TranscriptPersistItem>,
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
            run_id: Some(run_id.to_string()),
            role: "user",
            content: user_message.to_string(),
            payload: None,
            source_event_id: trace.root_event_id.clone(),
        });
    }
    for intent in state.user_intents.applied_user_intents() {
        core_items.push(TranscriptPersistItem {
            run_id: Some(run_id.to_string()),
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
            run_id: Some(run_id.to_string()),
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
        let result_content =
            transcript_tool_text(record.result_full.as_ref(), record.result_preview.as_ref());
        let result_content = if result_content.is_empty() && !record.ok {
            transcript_tool_text(record.error.as_ref(), None)
        } else {
            result_content
        };
        core_items.push(TranscriptPersistItem {
            run_id: Some(run_id.to_string()),
            role: "tool",
            content: result_content,
            payload: Some(TranscriptPersistPayload {
                tool_result: Some(astra_thin_client::SessionTranscriptToolResult {
                    tool_use_id: call_id.clone(),
                    name: Some(tool_name),
                    status: Some(
                        record
                            .canonical_terminal_event_type()
                            .trim_start_matches("tool_call_")
                            .to_string(),
                    ),
                    duration_ms: record.was_executed().then_some(record.ms),
                }),
                ..Default::default()
            }),
            source_event_id: trace_event_id(
                record.canonical_terminal_event_type(),
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
        run_id: Some(run_id.to_string()),
        role: "assistant",
        content: state.final_text.clone(),
        payload: None,
        source_event_id: trace_event_id("response", &[run_id, &trace.turn_id]),
    })
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

fn payload_with_reasoning_projection(
    payload: Option<TranscriptPersistPayload>,
    reasoning: &TranscriptReasoningProjection,
) -> Option<TranscriptPersistPayload> {
    if reasoning.is_empty() {
        return payload;
    }
    let mut payload = payload.unwrap_or_default();
    payload.reasoning = (!reasoning.text.is_empty()).then(|| reasoning.text.clone());
    payload.reasoning_status = payload.reasoning.as_ref().map(|_| {
        if reasoning.done {
            "complete".to_string()
        } else {
            "streaming".to_string()
        }
    });
    Some(payload)
}

fn serialize_transcript_payload(
    payload: Option<&TranscriptPersistPayload>,
    source_event_id: &str,
) -> Result<Option<String>, String> {
    payload
        .map(serde_json::to_string)
        .transpose()
        .map_err(|error| {
            format!("serialize canonical transcript evidence {source_event_id}: {error}")
        })
}

fn stored_transcript_matches_canonical_or_reasoning_projection(
    expected: &TranscriptPersistItem,
    actual: &StoredCanonicalTranscriptEvidence,
    expected_run_id: &str,
    reasoning_source_event_id: Option<&str>,
    reasoning: &TranscriptReasoningProjection,
) -> Result<bool, String> {
    if expected.run_id.as_deref() != Some(expected_run_id)
        || actual.source_event_id != expected.source_event_id
        || actual.run_id.as_deref() != Some(expected_run_id)
        || actual.role != expected.role
        || actual.content != expected.content
        || actual.content_hash
            != transcript_content_hash(
                &actual.role,
                &actual.content,
                actual.payload_json.as_deref(),
            )
    {
        return Ok(false);
    }

    let initial_payload =
        serialize_transcript_payload(expected.payload.as_ref(), &expected.source_event_id)?;
    if actual.payload_json == initial_payload {
        return Ok(true);
    }
    if reasoning_source_event_id != Some(expected.source_event_id.as_str()) || reasoning.is_empty()
    {
        return Ok(false);
    }
    let enriched_payload = payload_with_reasoning_projection(expected.payload.clone(), reasoning);
    let enriched_payload =
        serialize_transcript_payload(enriched_payload.as_ref(), &expected.source_event_id)?;
    Ok(actual.payload_json == enriched_payload)
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
                run_id: Some(run_id.to_string()),
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
            if !event.payload_kind.is_durable() {
                return Vec::new();
            }
            vec![TranscriptPersistItem {
                run_id: Some(run_id.to_string()),
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
    let payload = payload_json
        .as_deref()
        .map(serde_json::from_str::<TranscriptPersistPayload>)
        .transpose()
        .map_err(|error| {
            sqlx::Error::Protocol(format!(
                "decode stored transcript payload for run {run_id}: {error}"
            ))
        })?;
    let payload = payload_with_reasoning_projection(payload, reasoning)
        .expect("non-empty reasoning always materializes a transcript payload");
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

async fn load_durable_run_transcript_projection(
    pool: &SharedPool,
    user_id: &str,
    session_id: &str,
    run_id: &str,
) -> Result<DurableRunTranscriptProjection, String> {
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

    let mut projection = DurableRunTranscriptProjection::default();
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
        apply_reasoning_event_payload(&mut projection.reasoning, &payload);
        projection
            .evidence_items
            .extend(transcript_evidence_items_from_run_event(
                run_id,
                &event_id,
                &event_type,
                &payload,
            ));
    }
    Ok(projection)
}

pub(crate) async fn materialize_server_run_transcript_evidence(
    pool: &SharedPool,
    user_id: &str,
    session_id: &str,
    run_id: &str,
    terminal_assistant: Option<TranscriptPersistItem>,
    canonical_cursor: Option<&SessionCursorV1>,
) -> Result<(), String> {
    let projection =
        load_durable_run_transcript_projection(pool, user_id, session_id, run_id).await?;
    if projection.reasoning.is_empty()
        && projection.evidence_items.is_empty()
        && terminal_assistant.is_none()
        && canonical_cursor.is_none()
    {
        return Ok(());
    }

    let DurableRunTranscriptProjection {
        reasoning,
        mut evidence_items,
    } = projection;
    if let Some(terminal_assistant) = terminal_assistant {
        evidence_items.push(terminal_assistant);
    }

    let mut tx = pool
        .get()
        .begin()
        .await
        .map_err(|error| error.to_string())?;
    if let Err(error) =
        persist_session_transcript_items_inner_in_tx(&mut tx, user_id, session_id, &evidence_items)
            .await
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
    if let Some(cursor) = canonical_cursor
        && let Err(error) =
            commit_run_transcript_projection_in_tx(&mut tx, user_id, session_id, run_id, cursor)
                .await
    {
        return Err(rollback_materialized_transcript_transaction(
            tx,
            "advancing committed transcript projection",
            error,
        )
        .await);
    }
    tx.commit().await.map_err(|error| error.to_string())
}

fn transcript_cursor_i64(field: &'static str, value: u64) -> Result<i64, sqlx::Error> {
    i64::try_from(value)
        .map_err(|_| sqlx::Error::Protocol(format!("{field} exceeds MatrixOne BIGINT")))
}

fn projection_cursor_from_row(
    row: &sqlx::mysql::MySqlRow,
    user_id: &str,
    session_id: &str,
) -> Result<SessionCursorV1, sqlx::Error> {
    let completed_turn = row.try_get::<i64, _>("completed_turn")?;
    let completed_turn = u32::try_from(completed_turn).map_err(|_| {
        sqlx::Error::Protocol("transcript projection completed_turn is invalid".to_string())
    })?;
    let non_negative = |field: &'static str| -> Result<u64, sqlx::Error> {
        u64::try_from(row.try_get::<i64, _>(field)?)
            .map_err(|_| sqlx::Error::Protocol(format!("transcript projection {field} is invalid")))
    };
    Ok(SessionCursorV1 {
        schema_version: astra_turn_types::SESSION_CURSOR_SCHEMA_VERSION,
        owner_id: user_id.to_string(),
        session_id: session_id.to_string(),
        branch_id: astra_turn_types::DEFAULT_CONVERSATION_BRANCH_ID.to_string(),
        completed_turn,
        journal_event_seq: non_negative("journal_event_seq")?,
        conversation_seq: non_negative("conversation_seq")?,
        canonical_root_hash: row.try_get("canonical_root_hash")?,
        projection_schema: u32::try_from(non_negative("projection_schema")?).map_err(|_| {
            sqlx::Error::Protocol("transcript projection projection_schema is invalid".to_string())
        })?,
        compaction_generation: non_negative("compaction_generation")?,
        config_version_id: row.try_get("config_version_id")?,
    })
}

/// Promote all transcript material for one run and its causal cursor in one
/// transaction. The projection head advances only one canonical turn at a
/// time, so a missed/crashed projection cannot be hidden by a later turn.
async fn commit_run_transcript_projection_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    user_id: &str,
    session_id: &str,
    run_id: &str,
    cursor: &SessionCursorV1,
) -> Result<(), sqlx::Error> {
    if cursor.schema_version != astra_turn_types::SESSION_CURSOR_SCHEMA_VERSION
        || cursor.owner_id != user_id
        || cursor.session_id != session_id
        || cursor.branch_id != astra_turn_types::DEFAULT_CONVERSATION_BRANCH_ID
        || cursor.completed_turn == 0
        || cursor.journal_event_seq == 0
        || cursor.conversation_seq == 0
        || cursor.projection_schema == 0
        || cursor.canonical_root_hash.len() != 64
        || !cursor
            .canonical_root_hash
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(sqlx::Error::Protocol(
            "canonical transcript cursor is structurally invalid".to_string(),
        ));
    }
    let stored = sqlx::query(
        "SELECT completed_turn, journal_event_seq, conversation_seq,
                canonical_root_hash, projection_schema, compaction_generation,
                config_version_id
         FROM session_transcript_projection_heads
         WHERE user_id = ? AND session_id = ?
         FOR UPDATE",
    )
    .bind(user_id)
    .bind(session_id)
    .fetch_optional(&mut **tx)
    .await?
    .map(|row| projection_cursor_from_row(&row, user_id, session_id))
    .transpose()?;
    match stored.as_ref() {
        Some(stored) if stored == cursor => {}
        Some(stored) if stored.completed_turn.checked_add(1) == Some(cursor.completed_turn) => {}
        None if cursor.completed_turn == 1 => {}
        Some(stored) => {
            return Err(sqlx::Error::Protocol(format!(
                "transcript projection cannot advance from canonical turn {} to {}",
                stored.completed_turn, cursor.completed_turn
            )));
        }
        None => {
            return Err(sqlx::Error::Protocol(format!(
                "transcript projection is missing the prefix before canonical turn {}",
                cursor.completed_turn
            )));
        }
    }

    let completed_turn = i64::from(cursor.completed_turn);
    let conversation_seq = transcript_cursor_i64("conversation_seq", cursor.conversation_seq)?;
    let transcript_rows = sqlx::query(
        "SELECT canonical_completed_turn, canonical_conversation_seq, canonical_root_hash
         FROM session_transcript_items
         WHERE user_id = ? AND session_id = ? AND run_id = ?
         FOR UPDATE",
    )
    .bind(user_id)
    .bind(session_id)
    .bind(run_id)
    .fetch_all(&mut **tx)
    .await?;
    if transcript_rows.is_empty() {
        return Err(sqlx::Error::Protocol(
            "run transcript has no material to commit".to_string(),
        ));
    }
    for row in transcript_rows {
        let stored_turn = row.try_get::<Option<i64>, _>("canonical_completed_turn")?;
        let stored_sequence = row.try_get::<Option<i64>, _>("canonical_conversation_seq")?;
        let stored_root = row.try_get::<Option<String>, _>("canonical_root_hash")?;
        let is_uncommitted =
            stored_turn.is_none() && stored_sequence.is_none() && stored_root.is_none();
        let is_exact = stored_turn == Some(completed_turn)
            && stored_sequence == Some(conversation_seq)
            && stored_root.as_deref() == Some(cursor.canonical_root_hash.as_str());
        if !is_uncommitted && !is_exact {
            return Err(sqlx::Error::Protocol(
                "run transcript conflicts with its canonical projection".to_string(),
            ));
        }
    }
    sqlx::query(
        "UPDATE session_transcript_items
         SET canonical_completed_turn = ?, canonical_conversation_seq = ?,
             canonical_root_hash = ?
         WHERE user_id = ? AND session_id = ? AND run_id = ?
           AND canonical_completed_turn IS NULL",
    )
    .bind(completed_turn)
    .bind(conversation_seq)
    .bind(&cursor.canonical_root_hash)
    .bind(user_id)
    .bind(session_id)
    .bind(run_id)
    .execute(&mut **tx)
    .await?;

    sqlx::query(
        "INSERT INTO session_transcript_projection_heads
         (user_id, session_id, completed_turn, journal_event_seq,
          conversation_seq, canonical_root_hash, projection_schema,
          compaction_generation, config_version_id, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, NOW(6))
         ON DUPLICATE KEY UPDATE
           completed_turn = VALUES(completed_turn),
           journal_event_seq = VALUES(journal_event_seq),
           conversation_seq = VALUES(conversation_seq),
           canonical_root_hash = VALUES(canonical_root_hash),
           projection_schema = VALUES(projection_schema),
           compaction_generation = VALUES(compaction_generation),
           config_version_id = VALUES(config_version_id),
           updated_at = NOW(6)",
    )
    .bind(user_id)
    .bind(session_id)
    .bind(completed_turn)
    .bind(transcript_cursor_i64(
        "journal_event_seq",
        cursor.journal_event_seq,
    )?)
    .bind(conversation_seq)
    .bind(&cursor.canonical_root_hash)
    .bind(i64::from(cursor.projection_schema))
    .bind(transcript_cursor_i64(
        "compaction_generation",
        cursor.compaction_generation,
    )?)
    .bind(&cursor.config_version_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
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
    turn_started_at: chrono::DateTime<chrono::Utc>,
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
            event.created_at = turn_started_at
                + chrono::Duration::milliseconds(
                    i64::try_from(round.start_offset_ms.saturating_add(round.duration_ms))
                        .unwrap_or(i64::MAX),
                );
            event.token_usage = llm_round_token_usage_json(round);
            event.parent_event_id = Some(root_event_id.clone());
            event.metadata = json!({
                "purpose": round.purpose,
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
    turn_started_at: chrono::DateTime<chrono::Utc>,
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
        let disposition = record.effective_disposition();
        let started_at = turn_started_at
            + chrono::Duration::milliseconds(
                i64::try_from(record.start_offset_ms.unwrap_or_default()).unwrap_or(i64::MAX),
            );
        let tool_name = record.name.clone();

        // The call lifecycle begins when the model-issued request enters the
        // runtime. Admission may then reject, reuse, suppress, or defer it
        // without executing the provider; the terminal disposition below
        // records that distinction instead of fabricating a provider failure.
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
        started.tool_call_id = Some(call_id.clone());
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

        let terminal_type = record.canonical_terminal_event_type();
        let mut terminal = TraceEvent::new(
            trace_event_id(terminal_type, &[run_id, &call_id]),
            "",
            "",
            terminal_type,
            "tool_call",
        )
        .with_turn_context(trace);
        terminal.run_id = Some(run_id_owned.clone());
        terminal.parent_run_id = parent_run_str.clone();
        terminal.agent_id = Some(agent_str.clone());
        terminal.parent_agent_id = parent_agent_str.clone();
        terminal.round_index = round_index;
        terminal.tool_call_id = Some(call_id);
        terminal.meta_tool_name = Some(tool_name);
        terminal.meta_duration_ms = (disposition
            == astra_services::session_journal::ToolCallDisposition::Executed)
            .then(|| i32::try_from(record.ms).ok())
            .flatten();
        terminal.created_at = started_at
            + chrono::Duration::milliseconds(i64::try_from(record.ms).unwrap_or(i64::MAX));
        terminal.parent_event_id = Some(root_event_id.clone());
        if !record.ok {
            terminal.content = record.error.clone();
        }
        terminal.metadata = json!({
            "ok": record.ok,
            "disposition": disposition,
            "action": action,
            "args_preview": record.args_preview,
            "result_preview": record.result_preview,
            "tool_args_json_redacted": redacted_json_preview(args_json),
            "tool_result_json_redacted": redacted_json_preview(result_json),
            "child_agent_id": child_agent_id,
            "child_run_id": child_run_id,
            "error": record.error,
        });
        events.push(terminal);
    }
    events
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
    turn_started_at: chrono::DateTime<chrono::Utc>,
) -> Result<std::collections::BTreeMap<(String, String), (i64, Option<String>)>, String> {
    persist_server_loop_trace_events_impl(
        tx,
        user_id,
        session_id,
        run_id,
        parent_run_id,
        agent_id,
        parent_agent_id,
        trace_context,
        state,
        model_name,
        turn_started_at,
    )
    .await
}

async fn persist_server_loop_trace_events_impl(
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
    turn_started_at: chrono::DateTime<chrono::Utc>,
) -> Result<std::collections::BTreeMap<(String, String), (i64, Option<String>)>, String> {
    let trace = trace_context
        .unwrap_or_else(|| server_trace_context(user_id, session_id, run_id, state.session_turn));
    // Detail events are persisted as one terminal batch, so `Utc::now()` here
    // would collapse the whole turn into the persistence instant. Preserve the
    // producer's wall-clock anchor and reconstruct each event from its monotonic
    // turn offset. The fallback is only for legacy/test states without a buffer.
    let mut events = build_llm_round_trace_events(
        &trace,
        turn_started_at,
        run_id,
        parent_run_id,
        agent_id,
        parent_agent_id,
        model_name,
        &state.recent_rounds,
    );
    events.extend(build_tool_trace_events(
        &trace,
        turn_started_at,
        run_id,
        parent_run_id,
        agent_id,
        parent_agent_id,
        &state.stall.tool_call_records,
    ));
    if events.is_empty() {
        return Ok(std::collections::BTreeMap::new());
    }

    match DatabaseTraceEventWriter::write_many_in_tx(tx, events).await {
        Ok(deltas) => Ok(deltas),
        Err(e) => {
            astra_core::agent_error!(
                "server-loop",
                "failed to persist trace detail events (in tx) for session {session_id}: {e}"
            );
            Err(e.to_string())
        }
    }
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
    let queue_reservation = reserve_server_observer_request(&request);
    let session_id = session_id.to_string();
    tokio::spawn(async move {
        let _queue_reservation = queue_reservation;
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
    let messages: Vec<serde_json::Map<String, serde_json::Value>> =
        filter_memory_operation_turns(&state.messages)
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
    total_observation_tool_calls: u32,
    tools_used: &[String],
    llm_rounds_completed: u32,
    tool_ledger_receipt: &astra_turn_core::tool_ledger_receipt::ToolLedgerReceipt,
    token_usage_coverage: astra_turn_core::chat_turn_sse_dispatch::TokenUsageCoverage,
    final_text: &str,
    interruption: Option<&astra_turn_core::interruption::InterruptionRecord>,
    completion_facts: &astra_turn_core::complete::TurnCompletionFacts,
    runtime_feedback: Option<&astra_turn_core::context_feedback::RuntimeFeedbackFrame>,
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
    let mut event = astra_turn_core::complete::build_turn_complete_event(
        total_tool_calls > 0,
        completion_facts,
        execution_state,
        (!final_text.is_empty()).then_some(final_text),
    );
    if let Some(interruption) = interruption {
        // `execution_state` is the compact completion projection.  Preserve
        // the complete typed record as well so thin clients do not have to
        // reconstruct lifecycle authority from display text or a lossy set
        // of counters.
        event.insert("interruption".to_string(), interruption.to_json());
    }
    // This event closes the whole Server-owned loop, not one provider round.
    // Thin clients must not replay observed tool calls through a second local
    // continuation loop after the Server has already executed and committed
    // them.
    event.insert("continuation_owner".to_string(), json!("server"));
    event.insert("tool_calls_count".to_string(), json!(total_tool_calls));
    event.insert(
        "observation_tool_calls_count".to_string(),
        json!(total_observation_tool_calls),
    );
    let mut tools_used =
        astra_core::canonical_names::normalize_name_list(tools_used.iter().cloned());
    tools_used.sort_unstable();
    event.insert("tools_used".to_string(), json!(tools_used));
    event.insert("llm_rounds".to_string(), json!(llm_rounds_completed));
    event.insert(
        "tool_ledger_receipt".to_string(),
        json!(tool_ledger_receipt),
    );
    event.insert(
        "token_usage_coverage".to_string(),
        json!({
            "scope": "logical_provider_calls",
            "attempts": token_usage_coverage.attempts,
            "provider_reported": token_usage_coverage.provider_reported,
            "unavailable": token_usage_coverage.unavailable,
            "status": token_usage_coverage.status(),
        }),
    );
    if let Some(runtime_feedback) = runtime_feedback.filter(|frame| frame.is_valid()) {
        event.insert("runtime_feedback".to_string(), json!(runtime_feedback));
    }
    Value::Object(event)
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

    #[test]
    fn runner_continuation_ack_is_reserved_for_irreversible_run_terminals() {
        for status in ["completed", "delegated", "failed", "cancelled"] {
            assert!(should_acknowledge_runner_continuations(status), "{status}");
        }
        for status in ["running", "waiting", "paused", "unknown"] {
            assert!(!should_acknowledge_runner_continuations(status), "{status}");
        }
    }

    fn resolved_terminal_fixture() -> astra_services::runs::AtomicRunTerminalSettlementCommit {
        astra_services::runs::AtomicRunTerminalSettlementCommit {
            committed_events: vec![json!({
                "event_type": "run_finished",
                "data": {"status": "completed"}
            })],
            event_receipts: vec![AtomicRunTerminalEventReceipt {
                event_idx: 7,
                event_type: "run_finished".to_string(),
                event_id: "terminal-event-7".to_string(),
                idempotency_key: Some("terminal:run-1:completed".to_string()),
                event_hash: "event-hash".to_string(),
                settlement_batch_id: Some("settlement-batch".to_string()),
            }],
            last_event_idx: 7,
            latest_event_type: Some("run_finished".to_string()),
        }
    }

    #[test]
    fn commit_ack_lost_accepts_only_exact_authoritative_resolution() {
        let expected = resolved_terminal_fixture();
        let resolved = classify_authoritative_terminal_resolution(
            AtomicRunTerminalSettlementResolution::Exact(expected.clone()),
        )
        .expect("exact durable facts resolve a lost commit acknowledgement")
        .expect("commit is authoritative");

        assert_eq!(resolved, expected);
    }

    #[test]
    fn commit_ack_lost_mismatch_fails_closed() {
        let error = classify_authoritative_terminal_resolution(
            AtomicRunTerminalSettlementResolution::Conflict(
                "terminal event receipt mismatch".to_string(),
            ),
        )
        .expect_err("a mismatched durable terminal must remain ambiguous");

        assert!(error.contains("terminal event receipt mismatch"));
    }

    #[test]
    fn exact_terminal_replay_is_authoritative() {
        let expected = resolved_terminal_fixture();
        let resolved = classify_authoritative_terminal_resolution(
            AtomicRunTerminalSettlementResolution::Exact(expected.clone()),
        )
        .expect("an exact terminal replay is idempotent")
        .expect("replay returns the committed terminal");

        assert_eq!(resolved, expected);
        assert_eq!(resolved.last_event_idx, 7);
    }

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
            expected_owner_generation: None,
            owner_lease_duration: None,
            agent_id: Some("agent-1".to_string()),
            model_name: Some("model-1".to_string()),
            user_message: "work".to_string(),
            hook_db_writer: None,
            observer_worker: None,
            metrics_registry: None,
            csl_manager: csl_manager.map(tokio::sync::Mutex::new),
        }
    }

    #[test]
    fn root_atomic_terminal_append_includes_recoverable_assistant_transcript() {
        let persist = test_post_loop_persist_context("root-terminal-transcript", None);
        let append = persist.atomic_terminal_canonical_append(7);
        assert!(append.include_terminal_assistant);
        assert_eq!(append.expected_owner_generation, Some(7));

        let mut state = crate::turn::agentic_loop::host::make_test_loop_state();
        state.final_text = "durable final answer".to_string();
        let items = transcript_items_from_server_loop(
            append.user_id,
            append.session_id,
            append.run_id,
            append.trace_context.as_ref(),
            append.user_message,
            &state,
            append.include_terminal_assistant,
        );
        assert!(
            items
                .iter()
                .any(|item| { item.role == "assistant" && item.content == "durable final answer" })
        );
    }

    #[tokio::test]
    async fn canonical_failure_does_not_publish_derived_hook_state() {
        let writer = Arc::new(CaptureHookDbWriter::default());
        let mut persist = test_post_loop_persist_context("canonical-failure-session", None);
        persist.hook_db_writer = Some(writer.clone());
        let state = crate::turn::agentic_loop::host::make_test_loop_state();

        let error = persist
            .run_after_core(
                &state,
                false,
                Err("canonical transaction unavailable".to_string()),
                false,
            )
            .await
            .expect_err("derived state must fail closed without canonical facts");

        assert!(error.contains("canonical transaction unavailable"));
        assert!(
            writer.plans.lock().expect("capture lock").is_empty(),
            "a failed canonical transaction must not publish hook projections"
        );
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
    #[serial_test::serial(history_work)]
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
    #[serial_test::serial(history_work)]
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
    #[serial_test::serial(history_work)]
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
    fn server_loop_observer_excludes_structured_memory_operation_turns() {
        let mut state = observer_test_state();
        state.messages = vec![
            json!({"role":"user","content":"Remember the release date."}),
            json!({
                "role":"assistant",
                "tool_calls":[{"function":{"name":"memory","arguments":"{}"}}]
            }),
            json!({"role":"tool","name":"memory","content":"{\"memory_id\":\"m1\"}"}),
            json!({"role":"assistant","content":"Stored. Memory m1 is active."}),
            json!({"role":"user","content":"Rust ownership prevents data races."}),
            json!({"role":"assistant","content":"That is a useful fact."}),
        ];

        let request = build_server_loop_observer_request("user-1", "session-1", &state)
            .expect("ordinary turn should remain observable");
        assert_eq!(request.messages.len(), 2);
        assert_eq!(
            request.messages[0]["content"],
            "Rust ownership prevents data races."
        );
        assert_eq!(request.messages[1]["content"], "That is a useful fact.");
    }

    #[test]
    fn history_payload_work_counts_nested_payload_without_json_serialization() {
        let messages = vec![json!({
            "role": "user",
            "content": ["hi", {"text": "nested"}],
        })];

        assert_eq!(json_history_payload_work(&messages), (27, 1));
    }

    #[test]
    #[serial_test::serial(history_work)]
    fn server_observer_queue_reservation_releases_bytes_on_drop() {
        let state = observer_test_state();
        let request = build_server_loop_observer_request("user-1", "session-1", &state)
            .expect("observer request");
        assert!(
            reserve_server_observer_request_when(&request, false).is_none(),
            "disabled instrumentation must not retain or inspect queue payload"
        );
        let expected_bytes = server_observer_request_retained_bytes(&request);
        let scenario =
            astra_core::history_work::HistoryWorkScenario::begin("server-observer-queue-drop")
                .expect("exclusive history-work scenario");

        {
            let reservation = reserve_server_observer_request_when(&request, true)
                .expect("explicitly enabled instrumentation");
            assert_eq!(reservation.bytes(), expected_bytes);
        }

        let report = scenario.finish().expect("history-work report");
        let measurement = report
            .scoped
            .measurement(astra_core::history_work::HistoryWorkSite::ServerObserverQueue);
        assert_eq!(measurement.events, 1);
        assert_eq!(measurement.bytes, expected_bytes);
        assert_eq!(measurement.queue_peak_bytes, expected_bytes);
        assert_eq!(measurement.queue_current_bytes, 0);
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
        let mut objective = json!({"role": "user", "content": "old review"});
        astra_turn_types::mark_user_turn_semantics(
            &mut objective,
            astra_turn_types::UserTurnSemantics::new(
                astra_turn_types::ObjectiveRelation::Replace,
                None,
            ),
        );
        state.messages = vec![
            objective,
            json!({"role": "system", "content": "arbitrary compaction boundary", "_compact_boundary": true}),
            json!({"role": "user", "content": "不要review啊！"}),
            json!({"role": "assistant", "reasoning_content": "trace"}),
            astra_turn_types::runtime_owned_message(
                "system",
                "arbitrary runtime projection",
                astra_turn_types::RuntimeMessageDelivery::Projection,
            ),
            json!({"role": "tool", "tool_call_id": "c1", "content": "tool output"}),
        ];
        state.final_text = "ok".to_string();

        let messages = messages_for_csl_persist(&state);

        assert_eq!(messages.len(), 7);
        assert_eq!(messages[0]["content"], "old review");
        assert_eq!(
            astra_turn_types::user_turn_semantics(&messages[0])
                .expect("valid semantics")
                .map(|semantics| semantics.objective_relation),
            Some(astra_turn_types::ObjectiveRelation::Replace)
        );
        assert_eq!(messages[1]["_compact_boundary"], true);
        assert_eq!(messages[2]["content"], "不要review啊！");
        assert_eq!(messages[3]["reasoning_content"], "trace");
        assert!(astra_turn_types::is_runtime_owned_message(&messages[4]));
        assert_eq!(messages[5]["role"], "tool");
        assert_eq!(messages[6]["content"], "ok");
    }

    #[tokio::test]
    async fn csl_persist_is_skipped_when_canonical_backbone_is_incomplete() {
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

        ctx.persist_csl_if_canonical_ready(&state, false, &mut errors)
            .await;

        assert!(errors.is_empty());
        let entries = store.load_after(session_id, 0).await.expect("load csl");
        assert!(
            entries.is_empty(),
            "CSL must not advance when core+trace persistence failed: {entries:?}"
        );
    }

    #[tokio::test]
    async fn csl_persist_runs_after_canonical_backbone_is_ready() {
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

        ctx.persist_csl_if_canonical_ready(&state, true, &mut errors)
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
    fn delegated_trace_has_producer_local_root_and_inherited_causal_chain() {
        let parent = server_trace_context("user-1", "session-1", "root-run", 4);
        let context = HashMap::from([
            ("trace_session_id".to_string(), json!(parent.session_id)),
            ("trace_user_id".to_string(), json!(parent.user_id)),
            ("trace_turn_seq".to_string(), json!(parent.turn_seq)),
            (
                "trace_causal_chain_id".to_string(),
                json!(parent.causal_chain_id),
            ),
            (
                "trace_parent_event_id".to_string(),
                json!(parent.root_event_id),
            ),
        ]);

        let child = trace_context_from_subrun_context(&context, "child-run").unwrap();
        let sibling = trace_context_from_subrun_context(&context, "sibling-run").unwrap();

        assert_ne!(child.root_event_id, context["trace_parent_event_id"]);
        assert_ne!(child.root_event_id, sibling.root_event_id);
        assert_eq!(child.causal_chain_id, parent.causal_chain_id);
        assert_eq!(child.turn_seq, parent.turn_seq);
        assert_eq!(child.turn_id, server_turn_id("child-run"));
    }

    #[test]
    fn root_trace_context_is_stable_across_admission_and_terminal_persistence() {
        let admitted = server_trace_context("user-1", "session-1", "run-1", 3);
        let terminal = server_trace_context("user-1", "session-1", "run-1", 3);

        assert_eq!(admitted.root_event_id, terminal.root_event_id);
        assert_eq!(admitted.causal_chain_id, terminal.causal_chain_id);
        assert!(admitted.causal_chain_id.starts_with("server-loop:"));
    }

    #[test]
    fn accepted_root_turn_uses_the_same_canonical_identity_as_terminal_persistence() {
        let trace = server_trace_context("user-1", "session-1", "run-1", 3);
        let started_at = chrono::Utc::now();
        let event = server_loop_user_query_event(
            "user-1",
            "session-1",
            "run-1",
            None,
            None,
            None,
            None,
            &trace,
            "do the work",
            started_at,
        )
        .expect("non-empty accepted turn");

        assert_eq!(event.event_id, trace.root_event_id);
        assert_eq!(event.turn_seq, Some(3));
        assert_eq!(event.run_id.as_deref(), Some("run-1"));
        assert_eq!(event.parent_run_id, None);
        assert_eq!(event.content.as_deref(), Some("do the work"));
        assert_eq!(event.created_at, started_at);
        assert!(
            server_loop_user_query_event(
                "user-1",
                "session-1",
                "run-empty",
                None,
                None,
                None,
                None,
                &server_trace_context("user-1", "session-1", "run-empty", 4),
                "",
                started_at,
            )
            .is_none()
        );
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
    fn canonical_run_transcript_does_not_persist_runtime_configuration_as_speech() {
        let mut state = crate::turn::agentic_loop::host::make_test_loop_state();
        state.messages = vec![
            json!({"role": "user", "content": "翻译下面的内容：你好"}),
            astra_turn_types::runtime_owned_message(
                "system",
                "## Turn Budget\n59/60\n\n## Capabilities\n52 tools",
                astra_turn_types::RuntimeMessageDelivery::Projection,
            ),
        ];
        state.final_text = "你好".to_string();

        let items = transcript_items_from_server_loop(
            "user-1",
            "session-1",
            "run-1",
            None,
            "翻译下面的内容：你好",
            &state,
            true,
        );

        assert_eq!(
            items.iter().map(|item| item.role).collect::<Vec<_>>(),
            vec!["user", "assistant"]
        );
        assert_eq!(items[0].content, "翻译下面的内容：你好");
        assert_eq!(items[1].content, "你好");
        assert!(items.iter().all(|item| {
            !item.content.contains("Turn Budget")
                && !item.content.contains("Capabilities")
                && !item.content.contains("<system-reminder>")
        }));
    }

    #[test]
    fn canonical_run_transcript_preserves_rejected_tool_error_evidence() {
        let mut state = crate::turn::agentic_loop::host::make_test_loop_state();
        state
            .stall
            .tool_call_records
            .push(astra_services::session_journal::ToolCallRecord {
                tool_call_id: Some("call-rejected".to_string()),
                name: "web_search".to_string(),
                ok: false,
                error: Some("Unknown tool 'web_search' for the current capability binding".into()),
                disposition: Some(astra_services::session_journal::ToolCallDisposition::Rejected),
                ..Default::default()
            });

        let items = transcript_items_from_server_loop(
            "user-1",
            "session-1",
            "run-1",
            None,
            "try it",
            &state,
            false,
        );
        let tool_result = items
            .iter()
            .find(|item| item.role == "tool")
            .expect("rejected tool result");

        assert!(tool_result.content.contains("Unknown tool 'web_search'"));
        assert_eq!(
            tool_result
                .payload
                .as_ref()
                .and_then(|payload| payload.tool_result.as_ref())
                .and_then(|result| result.status.as_deref()),
            Some("rejected")
        );
        assert_eq!(
            tool_result
                .payload
                .as_ref()
                .and_then(|payload| payload.tool_result.as_ref())
                .and_then(|result| result.duration_ms),
            None
        );
        assert_eq!(
            tool_result.source_event_id,
            trace_event_id("tool_call_rejected", &["run-1", "call-rejected"])
        );
    }

    #[test]
    fn canonical_run_transcript_does_not_label_error_text_as_a_successful_result() {
        let mut state = crate::turn::agentic_loop::host::make_test_loop_state();
        state
            .stall
            .tool_call_records
            .push(astra_services::session_journal::ToolCallRecord {
                tool_call_id: Some("call-empty-success".to_string()),
                name: "write_file".to_string(),
                ok: true,
                error: Some("stale diagnostic that must not become output".into()),
                result_full: Some(String::new()),
                ..Default::default()
            });

        let items = transcript_items_from_server_loop(
            "user-1",
            "session-1",
            "run-1",
            None,
            "write it",
            &state,
            false,
        );
        let tool_result = items
            .iter()
            .find(|item| item.role == "tool")
            .expect("successful tool result");

        assert_eq!(tool_result.content, "");
        assert_eq!(
            tool_result
                .payload
                .as_ref()
                .and_then(|payload| payload.tool_result.as_ref())
                .and_then(|result| result.status.as_deref()),
            Some("completed")
        );
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

        let communication_fields = |payload_kind: &str| {
            json!({
                "schema_version": "astra.agent_communication.v1",
                "observed_by": {"run_id": "run-review", "agent_id": "reviewer"},
                "direction": "received",
                "message_id": format!("message-{payload_kind}"),
                "from": {"run_id": "run-code", "agent_id": "coder"},
                "to": {"kind": "direct", "address": {"run_id": "run-review", "agent_id": "reviewer"}},
                "payload_kind": payload_kind,
                "summary": "bounded summary",
                "timestamp_ms": 42,
                "requires_ack": false
            })
        };
        assert_eq!(
            transcript_evidence_items_from_run_event(
                "run-1",
                "event-message",
                "agent_communication",
                &communication_fields("text"),
            )
            .len(),
            1,
            "recoverable peer messages remain canonical evidence"
        );
        assert!(
            transcript_evidence_items_from_run_event(
                "run-1",
                "event-progress",
                "agent_communication",
                &communication_fields("progress"),
            )
            .is_empty(),
            "transient progress is never materialized into the canonical transcript"
        );
    }

    fn stored_transcript_fixture(
        expected: &TranscriptPersistItem,
        payload: Option<TranscriptPersistPayload>,
    ) -> StoredCanonicalTranscriptEvidence {
        let payload_json = payload
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .expect("serialize transcript fixture");
        StoredCanonicalTranscriptEvidence {
            source_event_id: expected.source_event_id.clone(),
            run_id: expected.run_id.clone(),
            role: expected.role.to_string(),
            content: expected.content.clone(),
            content_hash: transcript_content_hash(
                expected.role,
                &expected.content,
                payload_json.as_deref(),
            ),
            payload_json,
        }
    }

    #[test]
    fn canonical_transcript_verifier_accepts_initial_or_exact_reasoning_projection() {
        let expected = TranscriptPersistItem {
            run_id: Some("run-1".to_string()),
            role: "assistant",
            content: "final answer".to_string(),
            payload: None,
            source_event_id: "response-1".to_string(),
        };
        let initial = stored_transcript_fixture(&expected, None);
        assert!(
            stored_transcript_matches_canonical_or_reasoning_projection(
                &expected,
                &initial,
                "run-1",
                Some("response-1"),
                &TranscriptReasoningProjection::default(),
            )
            .unwrap(),
            "the immutable transcript written by the atomic transaction remains authoritative"
        );

        let reasoning = TranscriptReasoningProjection {
            text: "checked the invariant".to_string(),
            done: true,
        };
        let enriched_payload = payload_with_reasoning_projection(None, &reasoning);
        let enriched = stored_transcript_fixture(&expected, enriched_payload);
        assert!(
            stored_transcript_matches_canonical_or_reasoning_projection(
                &expected,
                &enriched,
                "run-1",
                Some("response-1"),
                &reasoning,
            )
            .unwrap(),
            "only the reasoning projection deterministically derived from durable events is allowed"
        );
    }

    #[test]
    fn canonical_transcript_verifier_rejects_non_reasoning_or_mismatched_enrichment() {
        let expected = TranscriptPersistItem {
            run_id: Some("run-1".to_string()),
            role: "assistant",
            content: "final answer".to_string(),
            payload: None,
            source_event_id: "response-1".to_string(),
        };
        let reasoning = TranscriptReasoningProjection {
            text: "checked the invariant".to_string(),
            done: true,
        };
        let exact_payload = payload_with_reasoning_projection(None, &reasoning).unwrap();

        let mut wrong_reasoning = exact_payload.clone();
        wrong_reasoning.reasoning = Some("different reasoning".to_string());
        let mut wrong_status = exact_payload.clone();
        wrong_status.reasoning_status = Some("streaming".to_string());
        let mut injected_tool = exact_payload.clone();
        injected_tool.tool_calls = vec![astra_thin_client::SessionTranscriptToolCall {
            tool_use_id: "forged-call".to_string(),
            name: "bash".to_string(),
            arguments: "{}".to_string(),
        }];
        let mut injected_evidence = exact_payload.clone();
        injected_evidence.evidence = Some(
            astra_turn_types::AgentTranscriptEvidence::ApprovalRequired {
                request_id: "forged-approval".to_string(),
                tool: "bash".to_string(),
                approval_kind: "standard".to_string(),
                display_label: None,
                detail: None,
            },
        );

        for (label, actual) in [
            (
                "wrong reasoning",
                stored_transcript_fixture(&expected, Some(wrong_reasoning)),
            ),
            (
                "wrong status",
                stored_transcript_fixture(&expected, Some(wrong_status)),
            ),
            (
                "injected tool",
                stored_transcript_fixture(&expected, Some(injected_tool)),
            ),
            (
                "injected evidence",
                stored_transcript_fixture(&expected, Some(injected_evidence)),
            ),
        ] {
            assert!(
                !stored_transcript_matches_canonical_or_reasoning_projection(
                    &expected,
                    &actual,
                    "run-1",
                    Some("response-1"),
                    &reasoning,
                )
                .unwrap(),
                "{label} must fail closed"
            );
        }

        let exact = stored_transcript_fixture(&expected, Some(exact_payload));
        for (label, mut actual) in [
            ("wrong content", exact.clone()),
            ("wrong run", exact.clone()),
            ("wrong source", exact.clone()),
            ("wrong role", exact.clone()),
            ("wrong hash", exact),
        ] {
            match label {
                "wrong content" => {
                    actual.content = "altered answer".to_string();
                    actual.content_hash = transcript_content_hash(
                        &actual.role,
                        &actual.content,
                        actual.payload_json.as_deref(),
                    );
                }
                "wrong run" => actual.run_id = Some("run-2".to_string()),
                "wrong source" => actual.source_event_id = "response-2".to_string(),
                "wrong role" => {
                    actual.role = "user".to_string();
                    actual.content_hash = transcript_content_hash(
                        &actual.role,
                        &actual.content,
                        actual.payload_json.as_deref(),
                    );
                }
                "wrong hash" => actual.content_hash = "forged-hash".to_string(),
                _ => unreachable!(),
            }
            assert!(
                !stored_transcript_matches_canonical_or_reasoning_projection(
                    &expected,
                    &actual,
                    "run-1",
                    Some("response-1"),
                    &reasoning,
                )
                .unwrap(),
                "{label} must fail closed"
            );
        }
        assert!(
            !stored_transcript_matches_canonical_or_reasoning_projection(
                &expected,
                &stored_transcript_fixture(
                    &expected,
                    payload_with_reasoning_projection(None, &reasoning)
                ),
                "run-1",
                Some("some-other-response"),
                &reasoning,
            )
            .unwrap(),
            "reasoning cannot enrich a transcript item outside its authorized source identity"
        );
    }

    #[test]
    fn llm_round_trace_events_use_canonical_token_usage() {
        let trace = server_trace_context("user-1", "session-1", "run-1", 3);
        let turn_started_at = chrono::Utc::now();
        let events = build_llm_round_trace_events(
            &trace,
            turn_started_at,
            "run-1",
            Some("parent-run"),
            Some("agent-1"),
            Some("root-agent"),
            Some("fallback-model"),
            &[crate::turn::agentic_loop::host::RecentRoundSummary {
                purpose: astra_turn_types::InferencePurpose::PrimaryAgent,
                turn: 1,
                round: 2,
                provider: "openai".to_string(),
                model: "model-1".to_string(),
                prompt_tokens: 10,
                cache_read_tokens: 4,
                cache_creation_tokens: 3,
                completion_tokens: 5,
                tool_calls_returned: 0,
                tool_call_names: Vec::new(),
                start_offset_ms: 1_000,
                duration_ms: 123,
                finish_reason: Some("stop".to_string()),
            }],
        );

        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.event_type, "llm_round_completed");
        assert_eq!(event.trace_kind, "llm_round");
        assert_eq!(event.round_index, Some(2));
        assert_eq!(
            event.created_at,
            turn_started_at + chrono::Duration::milliseconds(1_123)
        );
        assert_eq!(event.metadata["purpose"], "primary_agent");
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
        sqlx::query(
            "DELETE FROM session_transcript_projection_heads WHERE session_id = ? AND user_id = ?",
        )
        .bind(session_id)
        .bind(user_id)
        .execute(db)
        .await
        .expect("cleanup transcript fixture projection head");
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
        sqlx::query(
            "DELETE FROM session_transcript_projection_heads WHERE session_id = ? AND user_id = ?",
        )
        .bind(session_id)
        .bind(user_id)
        .execute(db)
        .await
        .expect("cleanup core persist projection head");
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
        let store = Arc::new(
            astra_services::runs::DatabaseRunStateStore::new(pool.clone())
                .with_owner_pod_id("core-persist-deferred-owner"),
        );
        let engine = crate::server::run::engine::RunEngine::new(store);
        let authority = engine
            .start_run(&run_id, &user_id, &session_id)
            .await
            .expect("start durable run");

        let mut state = crate::turn::agentic_loop::host::make_test_loop_state();
        state.session_turn = 7;
        state.final_text = "assistant final".to_string();
        state.push_recent_round(crate::turn::agentic_loop::host::RecentRoundSummary {
            purpose: astra_turn_types::InferencePurpose::PrimaryAgent,
            turn: 7,
            round: 1,
            provider: "test".to_string(),
            model: "test-model".to_string(),
            prompt_tokens: 5,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            completion_tokens: 2,
            tool_calls_returned: 1,
            tool_call_names: vec!["read_file".to_string()],
            start_offset_ms: 10,
            duration_ms: 20,
            finish_reason: Some("tool_calls".to_string()),
        });
        state
            .stall
            .tool_call_records
            .push(astra_services::session_journal::ToolCallRecord {
                tool_call_id: Some("timeline-tool".to_string()),
                name: "read_file".to_string(),
                ok: true,
                ms: 10,
                start_offset_ms: Some(40),
                round: Some(1),
                ..Default::default()
            });
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
            expected_owner_generation: Some(authority.owner_generation),
            owner_lease_duration: Some(Duration::from_secs(45)),
            agent_id: None,
            model_name: Some("test-model".to_string()),
            user_message: "initial one".to_string(),
            hook_db_writer: None,
            observer_worker: None,
            metrics_registry: None,
            csl_manager: None,
        };
        persist
            .persist_core_and_trace_in_transaction(&state)
            .await
            .expect("persist atomic core, trace, and transcript prefix");
        persist
            .materialize_run_transcript_evidence(&state, None)
            .await
            .expect("materialize terminal assistant transcript evidence");

        let event_rows = sqlx::query(
            "SELECT event_id, event_type, content, parent_event_id, created_at
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
        let timestamp_for = |event_type: &str| {
            event_rows
                .iter()
                .find(|row| row.try_get::<String, _>("event_type").unwrap() == event_type)
                .unwrap_or_else(|| panic!("missing {event_type} event"))
                .try_get::<chrono::NaiveDateTime, _>("created_at")
                .expect("event timestamp")
        };
        let user_at = timestamp_for("user_query");
        let round_at = timestamp_for("llm_round_completed");
        let tool_start_at = timestamp_for("tool_call_started");
        let tool_end_at = timestamp_for("tool_call_completed");
        let response_at = timestamp_for("llm_response");
        assert!(user_at <= round_at, "user query must anchor the turn");
        assert!(
            round_at <= tool_start_at,
            "round must precede its tool call"
        );
        assert!(
            tool_start_at <= tool_end_at,
            "tool start must precede its terminal event"
        );
        assert!(
            tool_end_at <= response_at,
            "terminal response must close the observed timeline"
        );

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
                ("assistant".to_string(), String::new()),
                ("tool".to_string(), String::new()),
                ("assistant".to_string(), "assistant final".to_string()),
            ],
            "transcript preserves the ordered conversation and exact tool evidence"
        );

        cleanup_core_persist_fixture_for_owner(&db, &session_id, &user_id).await;
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
    async fn canonical_append_rejects_a_superseded_execution_generation() {
        let pool = setup_pool().await;
        let db = pool.get().clone();
        let user_id = Uuid::new_v4().to_string();
        let session_id = Uuid::new_v4().to_string();
        let run_id = Uuid::new_v4().to_string();
        cleanup_core_persist_fixture_for_owner(&db, &session_id, &user_id).await;
        sqlx::query(
            "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count)
             VALUES (?, ?, 'canonical-owner-fence-it', 'active', 0)",
        )
        .bind(&session_id)
        .bind(&user_id)
        .execute(&db)
        .await
        .expect("insert owner session");
        let store = Arc::new(
            astra_services::runs::DatabaseRunStateStore::new(pool.clone())
                .with_owner_pod_id("canonical-owner-a"),
        );
        let engine = crate::server::run::engine::RunEngine::new(store);
        let authority = engine
            .start_run(&run_id, &user_id, &session_id)
            .await
            .expect("start durable run");
        sqlx::query(
            "UPDATE agent_runs
             SET run_generation = 1, owner_pod_id = 'canonical-owner-b',
                 owner_lease_expires_at = DATE_ADD(NOW(6), INTERVAL 60 SECOND)
             WHERE user_id = ? AND run_id = ?",
        )
        .bind(&user_id)
        .bind(&run_id)
        .execute(&db)
        .await
        .expect("supersede execution owner");

        let mut state = crate::turn::agentic_loop::host::make_test_loop_state();
        state.final_text = "stale answer must not become canonical".into();
        let baseline_run_event_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_run_events WHERE user_id = ? AND run_id = ?",
        )
        .bind(&user_id)
        .bind(&run_id)
        .fetch_one(&db)
        .await
        .expect("count baseline durable run events");
        let terminal_events = vec![json!({
            "event_type": "run_finished",
            "data": { "status": astra_core::STATUS_COMPLETED }
        })];
        let error = persist_server_loop_canonical_terminal_settlement(
            &pool,
            CanonicalLoopAppend {
                user_id: &user_id,
                session_id: &session_id,
                run_id: &run_id,
                expected_owner_generation: Some(authority.owner_generation),
                owner_lease_duration: Some(Duration::from_secs(45)),
                parent_run_id: None,
                parent_event_id: None,
                agent_id: Some("root-agent"),
                parent_agent_id: None,
                trace_context: None,
                user_message: "produce an answer",
                model_name: Some("test-model"),
                include_terminal_assistant: true,
            },
            &state,
            CanonicalTerminalSettlement {
                expected_statuses: &[astra_core::STATUS_RUNNING],
                expected_owner_generation: authority.owner_generation,
                status: astra_core::STATUS_COMPLETED,
                waiting_for: None,
                error_message: None,
                events: &terminal_events,
                prompt_tokens: 11,
                completion_tokens: 7,
                tool_calls: 3,
            },
        )
        .await
        .expect_err("stale generation must fail before writing any settlement evidence");
        assert!(
            error.contains("authoritative atomic terminal resolution conflict")
                && error.contains("run generation mismatch"),
            "unexpected canonical append error: {error}"
        );
        let event_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_events WHERE user_id = ? AND session_id = ? AND run_id = ?",
        )
        .bind(&user_id)
        .bind(&session_id)
        .bind(&run_id)
        .fetch_one(&db)
        .await
        .expect("count canonical events");
        assert_eq!(event_count, 0);
        let transcript_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM session_transcript_items
             WHERE user_id = ? AND session_id = ? AND run_id = ?",
        )
        .bind(&user_id)
        .bind(&session_id)
        .bind(&run_id)
        .fetch_one(&db)
        .await
        .expect("count canonical transcript items");
        assert_eq!(transcript_count, 0);
        let run_event_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_run_events WHERE user_id = ? AND run_id = ?",
        )
        .bind(&user_id)
        .bind(&run_id)
        .fetch_one(&db)
        .await
        .expect("count durable run events after stale settlement");
        assert_eq!(run_event_count, baseline_run_event_count);
        let run_row = sqlx::query(
            "SELECT status, total_prompt_tokens, total_completion_tokens, total_tool_calls
             FROM agent_runs WHERE user_id = ? AND run_id = ?",
        )
        .bind(&user_id)
        .bind(&run_id)
        .fetch_one(&db)
        .await
        .expect("load superseded durable run");
        assert_eq!(
            run_row.try_get::<String, _>("status").unwrap(),
            astra_core::STATUS_RUNNING
        );
        assert_eq!(run_row.try_get::<i64, _>("total_prompt_tokens").unwrap(), 0);
        assert_eq!(
            run_row
                .try_get::<i64, _>("total_completion_tokens")
                .unwrap(),
            0
        );
        assert_eq!(run_row.try_get::<i64, _>("total_tool_calls").unwrap(), 0);
        sqlx::query("DELETE FROM agent_run_events WHERE user_id = ? AND run_id = ?")
            .bind(&user_id)
            .bind(&run_id)
            .execute(&db)
            .await
            .expect("cleanup durable run events");
        sqlx::query(
            "DELETE FROM agent_session_execution_slots WHERE user_id = ? AND session_id = ?",
        )
        .bind(&user_id)
        .bind(&session_id)
        .execute(&db)
        .await
        .expect("cleanup execution slot");
        sqlx::query("DELETE FROM agent_runs WHERE user_id = ? AND run_id = ?")
            .bind(&user_id)
            .bind(&run_id)
            .execute(&db)
            .await
            .expect("cleanup durable run");
        cleanup_core_persist_fixture_for_owner(&db, &session_id, &user_id).await;
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
    async fn canonical_terminal_status_conflict_rolls_back_canonical_evidence_and_usage() {
        let pool = setup_pool().await;
        let db = pool.get().clone();
        let user_id = Uuid::new_v4().to_string();
        let session_id = Uuid::new_v4().to_string();
        let run_id = Uuid::new_v4().to_string();
        cleanup_core_persist_fixture_for_owner(&db, &session_id, &user_id).await;
        sqlx::query(
            "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count)
             VALUES (?, ?, 'canonical-terminal-rollback-it', 'active', 0)",
        )
        .bind(&session_id)
        .bind(&user_id)
        .execute(&db)
        .await
        .expect("insert owner session");
        let store = Arc::new(
            astra_services::runs::DatabaseRunStateStore::new(pool.clone())
                .with_owner_pod_id("canonical-terminal-rollback-owner"),
        );
        let engine = crate::server::run::engine::RunEngine::new(store);
        let authority = engine
            .start_run(&run_id, &user_id, &session_id)
            .await
            .expect("start durable run");
        let baseline_run_event_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_run_events WHERE user_id = ? AND run_id = ?",
        )
        .bind(&user_id)
        .bind(&run_id)
        .fetch_one(&db)
        .await
        .expect("count baseline durable run events");

        let mut state = crate::turn::agentic_loop::host::make_test_loop_state();
        state.final_text = "must roll back with the terminal conflict".into();
        let terminal_events = vec![json!({
            "event_type": "run_finished",
            "data": { "status": astra_core::STATUS_COMPLETED }
        })];
        let error = persist_server_loop_canonical_terminal_settlement(
            &pool,
            CanonicalLoopAppend {
                user_id: &user_id,
                session_id: &session_id,
                run_id: &run_id,
                expected_owner_generation: Some(authority.owner_generation),
                owner_lease_duration: Some(Duration::from_secs(45)),
                parent_run_id: None,
                parent_event_id: None,
                agent_id: Some("root-agent"),
                parent_agent_id: None,
                trace_context: None,
                user_message: "produce an answer",
                model_name: Some("test-model"),
                include_terminal_assistant: true,
            },
            &state,
            CanonicalTerminalSettlement {
                // The outer generation fence accepts the running owner. This
                // deliberately conflicts only after canonical rows have been
                // staged, proving the shared transaction rolls every class back.
                expected_statuses: &[astra_core::STATUS_WAITING],
                expected_owner_generation: authority.owner_generation,
                status: astra_core::STATUS_COMPLETED,
                waiting_for: None,
                error_message: None,
                events: &terminal_events,
                prompt_tokens: 31,
                completion_tokens: 13,
                tool_calls: 5,
            },
        )
        .await
        .expect_err("terminal precondition conflict must roll back canonical writes");
        assert!(
            error.contains("authoritative atomic terminal resolution conflict")
                && error.contains("terminal state or usage mismatch"),
            "unexpected canonical terminal error: {error}"
        );

        let canonical_event_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_events WHERE user_id = ? AND session_id = ? AND run_id = ?",
        )
        .bind(&user_id)
        .bind(&session_id)
        .bind(&run_id)
        .fetch_one(&db)
        .await
        .expect("count rolled-back canonical events");
        let transcript_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM session_transcript_items
             WHERE user_id = ? AND session_id = ? AND run_id = ?",
        )
        .bind(&user_id)
        .bind(&session_id)
        .bind(&run_id)
        .fetch_one(&db)
        .await
        .expect("count rolled-back transcript items");
        let run_event_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_run_events WHERE user_id = ? AND run_id = ?",
        )
        .bind(&user_id)
        .bind(&run_id)
        .fetch_one(&db)
        .await
        .expect("count durable run events after rollback");
        let run_row = sqlx::query(
            "SELECT status, total_prompt_tokens, total_completion_tokens, total_tool_calls
             FROM agent_runs WHERE user_id = ? AND run_id = ?",
        )
        .bind(&user_id)
        .bind(&run_id)
        .fetch_one(&db)
        .await
        .expect("load durable run after rollback");
        assert_eq!(canonical_event_count, 0);
        assert_eq!(transcript_count, 0);
        assert_eq!(run_event_count, baseline_run_event_count);
        assert_eq!(
            run_row.try_get::<String, _>("status").unwrap(),
            astra_core::STATUS_RUNNING
        );
        assert_eq!(run_row.try_get::<i64, _>("total_prompt_tokens").unwrap(), 0);
        assert_eq!(
            run_row
                .try_get::<i64, _>("total_completion_tokens")
                .unwrap(),
            0
        );
        assert_eq!(run_row.try_get::<i64, _>("total_tool_calls").unwrap(), 0);

        sqlx::query("DELETE FROM agent_run_events WHERE user_id = ? AND run_id = ?")
            .bind(&user_id)
            .bind(&run_id)
            .execute(&db)
            .await
            .expect("cleanup durable run events");
        sqlx::query(
            "DELETE FROM agent_session_execution_slots WHERE user_id = ? AND session_id = ?",
        )
        .bind(&user_id)
        .bind(&session_id)
        .execute(&db)
        .await
        .expect("cleanup execution slot");
        sqlx::query("DELETE FROM agent_runs WHERE user_id = ? AND run_id = ?")
            .bind(&user_id)
            .bind(&run_id)
            .execute(&db)
            .await
            .expect("cleanup durable run");
        cleanup_core_persist_fixture_for_owner(&db, &session_id, &user_id).await;
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
    async fn canonical_terminal_success_commits_evidence_usage_and_terminal_together() {
        let pool = setup_pool().await;
        let db = pool.get().clone();
        let user_id = Uuid::new_v4().to_string();
        let session_id = Uuid::new_v4().to_string();
        let run_id = Uuid::new_v4().to_string();
        cleanup_core_persist_fixture_for_owner(&db, &session_id, &user_id).await;
        sqlx::query(
            "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count)
             VALUES (?, ?, 'canonical-terminal-success-it', 'active', 0)",
        )
        .bind(&session_id)
        .bind(&user_id)
        .execute(&db)
        .await
        .expect("insert owner session");
        let store = Arc::new(
            astra_services::runs::DatabaseRunStateStore::new(pool.clone())
                .with_owner_pod_id("canonical-terminal-success-owner"),
        );
        let engine = crate::server::run::engine::RunEngine::new(store);
        let authority = engine
            .start_run(&run_id, &user_id, &session_id)
            .await
            .expect("start durable run");

        let mut state = crate::turn::agentic_loop::host::make_test_loop_state();
        state.final_text = "atomically committed answer".into();
        let terminal_events = vec![
            json!({
                "event_type": "text_done",
                "data": { "full_text": state.final_text }
            }),
            json!({
                "event_type": "run_finished",
                "data": { "status": astra_core::STATUS_COMPLETED }
            }),
        ];
        let commit = persist_server_loop_canonical_terminal_settlement(
            &pool,
            CanonicalLoopAppend {
                user_id: &user_id,
                session_id: &session_id,
                run_id: &run_id,
                expected_owner_generation: Some(authority.owner_generation),
                owner_lease_duration: Some(Duration::from_secs(45)),
                parent_run_id: None,
                parent_event_id: None,
                agent_id: Some("root-agent"),
                parent_agent_id: None,
                trace_context: None,
                user_message: "produce an answer",
                model_name: Some("test-model"),
                include_terminal_assistant: true,
            },
            &state,
            CanonicalTerminalSettlement {
                expected_statuses: &[astra_core::STATUS_RUNNING],
                expected_owner_generation: authority.owner_generation,
                status: astra_core::STATUS_COMPLETED,
                waiting_for: None,
                error_message: None,
                events: &terminal_events,
                prompt_tokens: 37,
                completion_tokens: 17,
                tool_calls: 6,
            },
        )
        .await
        .expect("commit canonical terminal settlement");
        assert_eq!(commit.terminal_events, terminal_events);
        assert!(commit.terminal_assistant_source_event_id.is_some());

        let canonical_event_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_events WHERE user_id = ? AND session_id = ? AND run_id = ?",
        )
        .bind(&user_id)
        .bind(&session_id)
        .bind(&run_id)
        .fetch_one(&db)
        .await
        .expect("count committed canonical events");
        let transcript_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM session_transcript_items
             WHERE user_id = ? AND session_id = ? AND run_id = ?",
        )
        .bind(&user_id)
        .bind(&session_id)
        .bind(&run_id)
        .fetch_one(&db)
        .await
        .expect("count committed transcript items");
        let terminal_event_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_run_events
             WHERE user_id = ? AND run_id = ? AND event_type IN ('text_done', 'run_finished')",
        )
        .bind(&user_id)
        .bind(&run_id)
        .fetch_one(&db)
        .await
        .expect("count committed terminal events");
        let run_row = sqlx::query(
            "SELECT status, total_prompt_tokens, total_completion_tokens, total_tool_calls
             FROM agent_runs WHERE user_id = ? AND run_id = ?",
        )
        .bind(&user_id)
        .bind(&run_id)
        .fetch_one(&db)
        .await
        .expect("load committed durable run");
        assert!(canonical_event_count >= 2);
        assert_eq!(transcript_count, 2);
        assert_eq!(terminal_event_count, 2);
        assert_eq!(
            run_row.try_get::<String, _>("status").unwrap(),
            astra_core::STATUS_COMPLETED
        );
        assert_eq!(
            run_row.try_get::<i64, _>("total_prompt_tokens").unwrap(),
            37
        );
        assert_eq!(
            run_row
                .try_get::<i64, _>("total_completion_tokens")
                .unwrap(),
            17
        );
        assert_eq!(run_row.try_get::<i64, _>("total_tool_calls").unwrap(), 6);

        sqlx::query("DELETE FROM agent_run_events WHERE user_id = ? AND run_id = ?")
            .bind(&user_id)
            .bind(&run_id)
            .execute(&db)
            .await
            .expect("cleanup durable run events");
        sqlx::query(
            "DELETE FROM agent_session_execution_slots WHERE user_id = ? AND session_id = ?",
        )
        .bind(&user_id)
        .bind(&session_id)
        .execute(&db)
        .await
        .expect("cleanup execution slot");
        sqlx::query("DELETE FROM agent_runs WHERE user_id = ? AND run_id = ?")
            .bind(&user_id)
            .bind(&run_id)
            .execute(&db)
            .await
            .expect("cleanup durable run");
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
                run_id: Some(run_id.clone()),
                role: "user",
                content: "hello".to_string(),
                payload: None,
                source_event_id: Uuid::new_v4().to_string(),
            },
            TranscriptPersistItem {
                run_id: Some(run_id.clone()),
                role: "user",
                content: "second input".to_string(),
                payload: None,
                source_event_id: Uuid::new_v4().to_string(),
            },
            TranscriptPersistItem {
                run_id: Some(run_id.clone()),
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
                run_id: Some(run_id),
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
        let store = Arc::new(
            astra_services::runs::DatabaseRunStateStore::new(pool.clone())
                .with_owner_pod_id("transcript-commit-owner"),
        );
        let engine = crate::server::run::engine::RunEngine::new(store);
        let authority = engine
            .start_run(&run_id, &user_id, &session_id)
            .await
            .expect("start durable run");

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

        let committed = persist_server_loop_canonical_append(
            &pool,
            CanonicalLoopAppend {
                user_id: &user_id,
                session_id: &session_id,
                run_id: &run_id,
                expected_owner_generation: Some(authority.owner_generation),
                owner_lease_duration: Some(Duration::from_secs(45)),
                parent_run_id: Some("parent-run"),
                parent_event_id: Some("parent-event"),
                agent_id: Some("child-agent"),
                parent_agent_id: Some("root-agent"),
                trace_context: None,
                user_message: "inspect identity",
                model_name: Some("test-model"),
                include_terminal_assistant: true,
            },
            &state,
        )
        .await
        .expect("canonical server loop append");
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

        let event_lineage = sqlx::query(
            "SELECT event_type, parent_event_id, parent_run_id, agent_id, parent_agent_id
             FROM agent_events
             WHERE user_id = ? AND session_id = ? AND run_id = ?
             ORDER BY event_type ASC",
        )
        .bind(&user_id)
        .bind(&session_id)
        .bind(&run_id)
        .fetch_all(&db)
        .await
        .expect("read canonical event lineage");
        assert_eq!(event_lineage.len(), 2);
        for row in event_lineage {
            let event_type = row
                .try_get::<String, _>("event_type")
                .expect("decode event_type");
            let parent_event_id = row
                .try_get::<Option<String>, _>("parent_event_id")
                .expect("decode parent_event_id");
            if event_type == "user_query" {
                assert_eq!(parent_event_id.as_deref(), Some("parent-event"));
            } else {
                assert!(
                    parent_event_id
                        .as_deref()
                        .is_some_and(|id| id != "parent-event"),
                    "child response must attach to its producer-local user event"
                );
            }
            assert_eq!(
                row.try_get::<Option<String>, _>("parent_run_id")
                    .expect("decode parent_run_id")
                    .as_deref(),
                Some("parent-run")
            );
            assert_eq!(
                row.try_get::<Option<String>, _>("agent_id")
                    .expect("decode agent_id")
                    .as_deref(),
                Some("child-agent")
            );
            assert_eq!(
                row.try_get::<Option<String>, _>("parent_agent_id")
                    .expect("decode parent_agent_id")
                    .as_deref(),
                Some("root-agent")
            );
        }

        cleanup_core_persist_fixture_for_owner(&db, &session_id, &user_id).await;
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
    async fn canonical_append_rolls_back_core_events_when_transcript_ownership_fails() {
        let pool = setup_pool().await;
        let db = pool.get().clone();
        let owner_user_id = Uuid::new_v4().to_string();
        let wrong_user_id = Uuid::new_v4().to_string();
        let session_id = Uuid::new_v4().to_string();
        let run_id = Uuid::new_v4().to_string();
        cleanup_core_persist_fixture_for_owner(&db, &session_id, &owner_user_id).await;
        cleanup_core_persist_fixture_for_owner(&db, &session_id, &wrong_user_id).await;
        sqlx::query(
            "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count)
             VALUES (?, ?, 'canonical-rollback-it', 'active', 0)",
        )
        .bind(&session_id)
        .bind(&owner_user_id)
        .execute(&db)
        .await
        .expect("insert owner session");

        let mut state = crate::turn::agentic_loop::host::make_test_loop_state();
        state.final_text = "must not survive rollback".into();
        state.session_turn = 3;
        persist_server_loop_canonical_append(
            &pool,
            CanonicalLoopAppend {
                user_id: &wrong_user_id,
                session_id: &session_id,
                run_id: &run_id,
                expected_owner_generation: None,
                owner_lease_duration: None,
                parent_run_id: None,
                parent_event_id: None,
                agent_id: Some("root-agent"),
                parent_agent_id: None,
                trace_context: None,
                user_message: "wrong owner append",
                model_name: Some("test-model"),
                include_terminal_assistant: true,
            },
            &state,
        )
        .await
        .expect_err("transcript ownership must abort the canonical append");

        let ghost_events = sqlx::query(
            "SELECT COUNT(*) AS c FROM agent_events
             WHERE user_id = ? AND session_id = ? AND run_id = ?",
        )
        .bind(&wrong_user_id)
        .bind(&session_id)
        .bind(&run_id)
        .fetch_one(&db)
        .await
        .expect("count rolled-back events")
        .try_get::<i64, _>("c")
        .expect("decode rolled-back event count");
        assert_eq!(ghost_events, 0);

        let ghost_transcript = sqlx::query(
            "SELECT COUNT(*) AS c FROM session_transcript_items
             WHERE user_id = ? AND session_id = ? AND run_id = ?",
        )
        .bind(&wrong_user_id)
        .bind(&session_id)
        .bind(&run_id)
        .fetch_one(&db)
        .await
        .expect("count rolled-back transcript")
        .try_get::<i64, _>("c")
        .expect("decode rolled-back transcript count");
        assert_eq!(ghost_transcript, 0);

        cleanup_core_persist_fixture_for_owner(&db, &session_id, &wrong_user_id).await;
        cleanup_core_persist_fixture_for_owner(&db, &session_id, &owner_user_id).await;
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
    async fn committed_transcript_projection_is_contiguous_atomic_and_idempotent() {
        let pool = setup_pool().await;
        let db = pool.get().clone();
        let user_id = Uuid::new_v4().to_string();
        let session_id = Uuid::new_v4().to_string();
        cleanup_transcript_fixture_for_owner(&db, &session_id, &user_id).await;
        sqlx::query(
            "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count)
             VALUES (?, ?, 'transcript-projection-it', 'active', 0)",
        )
        .bind(&session_id)
        .bind(&user_id)
        .execute(&db)
        .await
        .expect("insert projection session");

        let run_one = Uuid::new_v4().to_string();
        persist_session_transcript_items(
            &pool,
            &user_id,
            &session_id,
            &[TranscriptPersistItem {
                run_id: Some(run_one.clone()),
                role: "user",
                content: "turn one".to_string(),
                payload: None,
                source_event_id: Uuid::new_v4().to_string(),
            }],
        )
        .await
        .expect("insert first uncommitted transcript");
        let cursor_one = SessionCursorV1 {
            schema_version: astra_turn_types::SESSION_CURSOR_SCHEMA_VERSION,
            owner_id: user_id.clone(),
            session_id: session_id.clone(),
            branch_id: astra_turn_types::DEFAULT_CONVERSATION_BRANCH_ID.to_string(),
            completed_turn: 1,
            journal_event_seq: 1,
            conversation_seq: 1,
            canonical_root_hash: "a".repeat(64),
            projection_schema: 2,
            compaction_generation: 0,
            config_version_id: None,
        };
        for _ in 0..2 {
            let mut tx = db.begin().await.expect("begin exact projection retry");
            commit_run_transcript_projection_in_tx(
                &mut tx,
                &user_id,
                &session_id,
                &run_one,
                &cursor_one,
            )
            .await
            .expect("exact projection retry");
            tx.commit().await.expect("commit exact projection retry");
        }

        let run_three = Uuid::new_v4().to_string();
        persist_session_transcript_items(
            &pool,
            &user_id,
            &session_id,
            &[TranscriptPersistItem {
                run_id: Some(run_three.clone()),
                role: "user",
                content: "turn three without two".to_string(),
                payload: None,
                source_event_id: Uuid::new_v4().to_string(),
            }],
        )
        .await
        .expect("insert gap transcript");
        let cursor_three = SessionCursorV1 {
            completed_turn: 3,
            journal_event_seq: 3,
            conversation_seq: 3,
            canonical_root_hash: "c".repeat(64),
            ..cursor_one.clone()
        };
        let mut gap_tx = db.begin().await.expect("begin gap projection");
        let gap = commit_run_transcript_projection_in_tx(
            &mut gap_tx,
            &user_id,
            &session_id,
            &run_three,
            &cursor_three,
        )
        .await
        .expect_err("projection gap must fail closed");
        gap_tx.rollback().await.expect("rollback projection gap");
        assert!(matches!(gap, sqlx::Error::Protocol(_)));

        let head_turn = sqlx::query(
            "SELECT completed_turn FROM session_transcript_projection_heads
             WHERE user_id = ? AND session_id = ?",
        )
        .bind(&user_id)
        .bind(&session_id)
        .fetch_one(&db)
        .await
        .expect("projection head")
        .try_get::<i64, _>("completed_turn")
        .expect("decode projection head");
        assert_eq!(head_turn, 1);
        let uncommitted = sqlx::query(
            "SELECT canonical_completed_turn FROM session_transcript_items
             WHERE user_id = ? AND session_id = ? AND run_id = ?",
        )
        .bind(&user_id)
        .bind(&session_id)
        .bind(&run_three)
        .fetch_one(&db)
        .await
        .expect("gap item")
        .try_get::<Option<i64>, _>("canonical_completed_turn")
        .expect("decode gap item");
        assert_eq!(uncommitted, None);

        cleanup_transcript_fixture_for_owner(&db, &session_id, &user_id).await;
    }
}
