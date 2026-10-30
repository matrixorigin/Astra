//! Canonical transcript persistence, hook writes, observers and runtime promotions.
//! Optional diagnostics use the existing ingestion owner; they never gate a turn.
//!
//! Extracted from [`super`] to keep the lifecycle module manageable.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use sqlx::{Acquire, Row};
use tracing;
use uuid::Uuid;

use astra_core::{
    ErrorResponse, STATUS_CANCELLED, STATUS_FAILED, STATUS_PAUSED, STATUS_RUNNING, STATUS_WAITING,
    SharedPool, connect_matrixone,
};
use astra_services::coordination::AgentProfile;
use astra_services::runs::{
    CanonicalRunTerminalReceipt, CanonicalRunTerminalRequest, CanonicalTerminalClaim,
    CanonicalTerminalContext, CanonicalTerminalOutcome, CanonicalTerminalTranscript,
    DatabaseRunStateStore, DurableCancellationOrigin, RunStateStore,
};
use astra_services::session_audit::{RUNTIME_PROMOTION_EVENT_TYPE, RuntimePromotionEventData};
use astra_services::session_context_coordinator::{
    DatabaseSessionContextCoordinator, PreparedSessionContextCommit, RunContextAuthority,
};
use astra_services::skills::SkillService;
use astra_services::{CancellationSafePoolConnection, EdgeContext};
use astra_services::{
    WorkspaceCleanupDebtEntry, WorkspaceRecordEntry as StoredWorkspaceRecordEntry,
    WorkspaceRecordStoreError, WorkspaceStateStore,
};
use astra_turn_core::contracts::{
    TurnHookDbPersistPlan, TurnHookDbWriter, TurnObserverOutcome, TurnObserverRequest,
    TurnObserverWorker, TurnSkillSelectionRecord,
};
use astra_turn_core::observer::filter_memory_operation_turns;
use astra_turn_core::trace_event::TraceContext;
use astra_turn_types::{SessionCursorV1, transcript_content_hash, transcript_payload_identity};

use crate::MatrixOneSettings;
use crate::server::run::engine::{RunOwnerLeaseTerminalAuthority, RunOwnerLeaseTerminalOperation};
use crate::turn::agentic_loop::host::AgenticLoopState;
use crate::{DatabaseEventService, EventCreateRequestData, EventService};
use astra_services::storage::admit_session_event_write;

use super::{
    build_runtime_event_service, build_runtime_turn_evaluation_event, flush_turn_observability,
    persist_runtime_promotion_events, persist_turn_evaluation_journal,
};

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

/// Bundles all handles needed by post-loop best-effort persistence calls.
///
/// Root execution shares canonical transcript settlement and derived hooks,
/// observers and promotions. These consumers cannot authorize conversation writes.
pub(crate) struct PostLoopPersistContext {
    pub(crate) matrixone: MatrixOneSettings,
    pub(crate) shared_pool: Option<SharedPool>,
    pub(crate) user_id: String,
    pub(crate) session_id: String,
    pub(crate) run_id: String,
    pub(crate) expected_owner_generation: Option<u64>,
    pub(crate) owner_lease_duration: Option<Duration>,
    pub(crate) terminal_authority: Option<RunOwnerLeaseTerminalAuthority>,
    pub(crate) owner_pod_id: Option<String>,
    pub(crate) user_message: String,
    pub(crate) hook_db_writer: Option<Arc<dyn TurnHookDbWriter>>,
    pub(crate) observer_worker: Option<Arc<dyn TurnObserverWorker>>,
    pub(crate) metrics_registry: Option<Arc<astra_turn_core::pipeline_metrics::MetricsRegistry>>,
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
            trace_context: None,
            user_message: &self.user_message,
            // A durable terminal must never become visible before its
            // user-visible assistant transcript is recoverable. Reasoning and
            // cursor annotations remain post-terminal projections, but the
            // final answer belongs in this exact-generation transaction with
            // canonical evidence, usage, and status.
            include_terminal_assistant: true,
        }
    }

    /// Persist projections and observers after the canonical core transaction.
    ///
    /// Callers must pass the exact result of
    /// [`Self::persist_canonical_transcript_in_transaction`]. This keeps the order
    /// `canonical journal -> context-head CAS -> derived consumers` explicit.
    pub(crate) async fn run_after_core(
        &self,
        state: &AgenticLoopState,
        loop_success: bool,
        canonical_result: Result<(), String>,
    ) -> Result<(), String> {
        let mut errors = Vec::new();
        if let Err(error) = canonical_result {
            // Hook rows, memory extraction, session-end hooks, and promotion
            // events are derived from the canonical turn. Publishing any of
            // them after the canonical transaction
            // failed creates a second, contradictory source of truth. Retain
            // the classified terminal failure and append-only provider/tool
            // evidence, but fail closed before derived state escapes.
            return Err(format!("canonical transaction failed: {error}"));
        }

        crate::turn::agentic_loop::finalization::persist_completed_context_trace(state).await;

        // The remaining consumers all read the immutable completed loop state
        // and write independent sinks. The writes are awaited together;
        // their completion is not the streaming terminal-delivery boundary.
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
        let (hook_result, observer_result, (), promotion_result) = tokio::join!(
            hook_persist,
            observer_dispatch,
            session_end,
            promotion_persist,
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

    /// Persist the raw conversation atomically under the current Run owner.
    /// Diagnostics do not authorize or gate conversation durability.
    pub(crate) async fn persist_canonical_transcript_in_transaction(
        &self,
        state: &AgenticLoopState,
        include_terminal_assistant: bool,
    ) -> Result<Option<String>, String> {
        let Some(pool) = self.shared_pool.as_ref() else {
            // Persistence is an explicit deployment capability. An ephemeral
            // runtime performs no durable writes; this is distinct from a
            // configured database failing, which must fail closed below.
            return Ok(None);
        };

        persist_server_loop_canonical_append(
            pool,
            CanonicalLoopAppend {
                user_id: &self.user_id,
                session_id: &self.session_id,
                run_id: &self.run_id,
                expected_owner_generation: self.expected_owner_generation,
                owner_lease_duration: self.owner_lease_duration,
                trace_context: None,
                user_message: &self.user_message,
                include_terminal_assistant,
            },
            state,
        )
        .await
    }

    /// Atomically settle a root run's canonical DB evidence, semantic usage,
    /// and durable terminal. `Ok(None)` requests the existing fallback for an
    /// ephemeral deployment. Durable control convergence never falls back to
    /// an independent append.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn persist_atomic_terminal_settlement(
        &self,
        state: &AgenticLoopState,
        expected_statuses: &[&str],
        expected_owner_generation: u64,
        status: &str,
        waiting_for: Option<&str>,
        error_message: Option<&str>,
        error_code: Option<&str>,
        error_kind: Option<astra_core::ErrorKind>,
        error_metadata: Option<&astra_services::models::ModelGatewayErrorMetadata>,
        cancellation_origin: Option<astra_turn_core::orchestration_types::CancellationOrigin>,
        context_commit: Option<&PreparedSessionContextCommit>,
    ) -> Result<Option<CanonicalTerminalSettlementCommit>, String> {
        let Some(pool) = self.shared_pool.as_ref() else {
            return Ok(None);
        };
        let terminal_operation = match self.terminal_authority.as_ref() {
            Some(authority) => Some(authority.begin().await?),
            None => None,
        };
        persist_server_loop_canonical_terminal_settlement(
            pool,
            self.atomic_terminal_canonical_append(expected_owner_generation),
            state,
            CanonicalTerminalSettlement {
                expected_statuses,
                expected_owner_pod_id: self
                    .owner_pod_id
                    .as_deref()
                    .ok_or("terminal has no configured execution owner")?,
                expected_owner_generation,
                status,
                waiting_for,
                error_message,
                error_code,
                error_kind,
                error_metadata,
                cancellation_origin,
                prompt_tokens: state.provider_input_tokens(),
                completion_tokens: state.total_completion,
                tool_calls: state.total_tool_calls,
                context_commit,
            },
            terminal_operation,
        )
        .await
        .map(Some)
    }

    pub(crate) async fn materialize_run_transcript_evidence(
        &self,
        state: &AgenticLoopState,
        terminal_assistant_source_event_id: Option<&str>,
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
        )
        .filter(|item| terminal_assistant_source_event_id == Some(item.source_event_id.as_str()));
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
    pub(crate) trace_context: Option<TraceContext>,
    pub(crate) user_message: &'a str,
    pub(crate) include_terminal_assistant: bool,
}

#[derive(Clone, Copy)]
pub(crate) struct CanonicalTerminalSettlement<'a> {
    pub(crate) expected_statuses: &'a [&'a str],
    pub(crate) expected_owner_pod_id: &'a str,
    pub(crate) expected_owner_generation: u64,
    pub(crate) status: &'a str,
    pub(crate) waiting_for: Option<&'a str>,
    pub(crate) error_message: Option<&'a str>,
    pub(crate) error_code: Option<&'a str>,
    pub(crate) error_kind: Option<astra_core::ErrorKind>,
    pub(crate) error_metadata: Option<&'a astra_services::models::ModelGatewayErrorMetadata>,
    pub(crate) cancellation_origin:
        Option<astra_turn_core::orchestration_types::CancellationOrigin>,
    pub(crate) prompt_tokens: u64,
    pub(crate) completion_tokens: u64,
    pub(crate) tool_calls: u32,
    pub(crate) context_commit: Option<&'a PreparedSessionContextCommit>,
}

#[derive(Clone, Debug)]
pub(crate) struct CanonicalTerminalSettlementCommit {
    pub(crate) terminal_status: Option<&'static str>,
    pub(crate) canonical_context_cursor: Option<SessionCursorV1>,
    /// Completion projection plus controls newly committed by this attempt.
    /// Idempotent readback has no new controls; reconnect reads durable events.
    pub(crate) terminal_events: Vec<Value>,
    pub(crate) terminal_assistant_source_event_id: Option<String>,
}

fn atomic_terminal_request<'a>(
    append: &'a CanonicalLoopAppend<'a>,
    settlement: CanonicalTerminalSettlement<'a>,
    interruption: Option<&'a astra_turn_core::interruption::InterruptionRecord>,
) -> CanonicalRunTerminalRequest<'a> {
    CanonicalRunTerminalRequest {
        user_id: append.user_id,
        run_id: append.run_id,
        expected_session_id: append.session_id,
        expected_statuses: settlement.expected_statuses,
        expected_owner_generation: settlement.expected_owner_generation,
        status: settlement.status,
        waiting_for: settlement.waiting_for,
        error_message: settlement.error_message,
        error_code: settlement.error_code,
        error_kind: settlement.error_kind,
        error_metadata: settlement.error_metadata,
        interruption,
        cancellation_origin: settlement.cancellation_origin.map(|origin| match origin {
            astra_turn_core::orchestration_types::CancellationOrigin::User => {
                DurableCancellationOrigin::User
            }
            astra_turn_core::orchestration_types::CancellationOrigin::Runtime => {
                DurableCancellationOrigin::Runtime
            }
            astra_turn_core::orchestration_types::CancellationOrigin::Unverified => {
                DurableCancellationOrigin::Unverified
            }
        }),
        prompt_tokens: settlement.prompt_tokens,
        completion_tokens: settlement.completion_tokens,
        tool_calls: settlement.tool_calls,
    }
}

async fn verify_canonical_append_evidence(
    pool: &SharedPool,
    append: &CanonicalLoopAppend<'_>,
    state: &AgenticLoopState,
    expected_cursor: Option<&SessionCursorV1>,
) -> Result<(), String> {
    let items = transcript_items_from_server_loop(
        append.user_id,
        append.session_id,
        append.run_id,
        append.trace_context.as_ref(),
        append.user_message,
        state,
        append.include_terminal_assistant,
    );
    let mut offset = 0;
    while offset < items.len() {
        let chunk = transcript_chunk(&items, offset).map_err(|error| error.to_string())?;
        let missing = validate_transcript_chunk(
            pool.get(),
            append.user_id,
            append.session_id,
            &chunk,
            expected_cursor,
        )
        .await
        .map_err(|error| error.to_string())?;
        if !missing.is_empty() {
            return Err("canonical transcript evidence is missing".into());
        }
        offset += chunk.len();
    }
    Ok(())
}

async fn verify_terminal_context_evidence(
    pool: &SharedPool,
    settlement: CanonicalTerminalSettlement<'_>,
    receipt: &CanonicalRunTerminalReceipt,
) -> Result<(), String> {
    if let CanonicalTerminalContext::Advanced {
        cursor,
        commit_hash,
    } = &receipt.context
    {
        DatabaseSessionContextCoordinator::new(pool.clone())
            .verify_prepared_run_context_commit(
                settlement
                    .context_commit
                    .ok_or("advanced terminal has no prepared Context")?,
                cursor,
                commit_hash,
            )
            .await
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// Append the complete durable loop record through one transaction for roots
/// and delegated agents alike. There is deliberately no independent-write
/// fallback: a failed append must not leave a plausible partial history.
pub(crate) async fn persist_server_loop_canonical_append(
    pool: &SharedPool,
    append: CanonicalLoopAppend<'_>,
    state: &AgenticLoopState,
) -> Result<Option<String>, String> {
    persist_server_loop_canonical_append_inner(pool, append, state, None, None)
        .await
        .map(|commit| commit.terminal_assistant_source_event_id)
}

/// Atomically commit canonical loop evidence, semantic run usage, and the
/// exact-generation durable terminal and display projection.
pub(crate) async fn persist_server_loop_canonical_terminal_settlement(
    pool: &SharedPool,
    append: CanonicalLoopAppend<'_>,
    state: &AgenticLoopState,
    settlement: CanonicalTerminalSettlement<'_>,
    terminal_operation: Option<RunOwnerLeaseTerminalOperation<'_>>,
) -> Result<CanonicalTerminalSettlementCommit, String> {
    // Once an exact-generation terminal transaction may have reached COMMIT,
    // dropping its future at the lease deadline would bypass the existing
    // lost-acknowledgement resolution and report a committed run as failed.
    // The terminal authority bounds lock acquisition; the transaction's
    // durable generation fence owns settlement and its uncertainty handling.
    let commit = persist_server_loop_canonical_append_inner(
        pool,
        append,
        state,
        Some(settlement),
        terminal_operation,
    )
    .await?;
    Ok(commit)
}

struct PreparedCanonicalTerminal {
    receipt: CanonicalRunTerminalReceipt,
    authority: Option<RunContextAuthority>,
}

async fn prepare_canonical_terminal(
    pool: &SharedPool,
    append: &CanonicalLoopAppend<'_>,
    state: &AgenticLoopState,
    settlement: CanonicalTerminalSettlement<'_>,
    items: &[TranscriptPersistItem],
) -> Result<PreparedCanonicalTerminal, String> {
    let (opening_hash, authority) = if let Some(context) = settlement.context_commit {
        let authority = context
            .run_context_authority()
            .ok_or("Run terminal cannot borrow Controller authority")?;
        (
            context
                .opening_hash()
                .ok_or("terminal Context has no opening identity")?
                .to_owned(),
            Some(authority.clone()),
        )
    } else {
        // Legitimate empty/no-Context outcomes use immutable opening data. This
        // is a cold preparation read, never an execution or mutation grant.
        let store = DatabaseRunStateStore::new(pool.clone())
            .with_owner_pod_id(settlement.expected_owner_pod_id);
        let stored = store
            .load_run_admission(append.user_id, append.run_id)
            .await?
            .ok_or("terminal Run is missing")?;
        let run = stored.run();
        let mut authority = run
            .opening
            .as_ref()
            .and_then(|opening| opening.context_authority())
            .cloned();
        if let Some(authority) = &mut authority {
            authority.generation = settlement.expected_owner_generation;
        }
        (
            run.opening_hash
                .clone()
                .ok_or("terminal Run has no immutable opening")?,
            authority,
        )
    };
    let request = atomic_terminal_request(append, settlement, None);
    let outcome = CanonicalRunTerminalReceipt::outcome_for_request(request)?;
    let context = if authority.is_none() {
        CanonicalTerminalContext::Unowned
    } else if matches!(outcome, CanonicalTerminalOutcome::Cancelled { .. }) {
        CanonicalTerminalContext::Revoked
    } else if let Some(prepared) = settlement.context_commit {
        CanonicalTerminalContext::Advanced {
            cursor: prepared.cursor(),
            commit_hash: prepared.request_hash().into(),
        }
    } else {
        CanonicalTerminalContext::Unchanged
    };
    let mut hash = Sha256::new();
    hash.update(b"astra.run-terminal-transcript.v1\0");
    hash.update((items.len() as u64).to_be_bytes());
    let mut offset = 0;
    while offset < items.len() {
        let chunk = transcript_chunk(items, offset).map_err(|error| error.to_string())?;
        for prepared in &chunk {
            for field in [
                prepared.item.source_event_id.as_str(),
                prepared.item.run_id.as_deref().unwrap_or(""),
            ] {
                hash.update((field.len() as u64).to_be_bytes());
                hash.update(field.as_bytes());
            }
            hash.update(
                transcript_content_hash(
                    prepared.item.role,
                    &prepared.item.content,
                    prepared.payload_identity.as_deref(),
                )
                .as_bytes(),
            );
        }
        offset += chunk.len();
    }
    let ledger = state
        .tool_ledger_receipt
        .receipt(append.run_id, settlement.expected_owner_generation);
    let receipt = CanonicalRunTerminalReceipt {
        schema_version: 1,
        user_id: append.user_id.into(),
        session_id: append.session_id.into(),
        run_id: append.run_id.into(),
        generation: settlement.expected_owner_generation,
        opening_hash,
        context_admission_hash: authority
            .as_ref()
            .map(RunContextAuthority::admission_fingerprint),
        outcome,
        error_hash: CanonicalRunTerminalReceipt::error_hash(settlement.error_message),
        error_code: request.error_code.map(str::to_owned),
        error_kind: request.error_kind,
        error_metadata: settlement.error_metadata.cloned(),
        interruption: state
            .interruption
            .as_ref()
            .filter(|interruption| {
                settlement.status == STATUS_FAILED
                    && settlement.error_kind.is_none()
                    && settlement.error_metadata.is_none()
                    && interruption.kind
                        == astra_turn_core::interruption::InterruptionKind::ExecutionIncomplete
            })
            .map(astra_turn_core::interruption::InterruptionRecord::public_projection),
        context,
        transcript: CanonicalTerminalTranscript {
            item_count: u32::try_from(items.len()).map_err(|_| "raw transcript count overflow")?,
            digest: format!("{:x}", hash.finalize()),
            assistant_source_event_id: items
                .iter()
                .rev()
                .find(|item| item.role == "assistant")
                .map(|item| item.source_event_id.clone()),
        },
        usage: state.qualified_usage,
        tool_ledger: (ledger.validate().is_ok() && ledger.attempted == settlement.tool_calls)
            .then_some(ledger),
    };
    receipt.validate()?;
    Ok(PreparedCanonicalTerminal { receipt, authority })
}

async fn persist_server_loop_canonical_append_inner(
    pool: &SharedPool,
    append: CanonicalLoopAppend<'_>,
    state: &AgenticLoopState,
    mut settlement: Option<CanonicalTerminalSettlement<'_>>,
    mut terminal_operation: Option<RunOwnerLeaseTerminalOperation<'_>>,
) -> Result<CanonicalTerminalSettlementCommit, String> {
    if let Some(settlement) = settlement
        && append.expected_owner_generation != Some(settlement.expected_owner_generation)
    {
        return Err("canonical append and terminal disagree on execution generation".into());
    }
    let items = transcript_items_from_server_loop(
        append.user_id,
        append.session_id,
        append.run_id,
        append.trace_context.as_ref(),
        append.user_message,
        state,
        append.include_terminal_assistant,
    );
    let mut plan = match settlement {
        Some(settlement) => {
            Some(prepare_canonical_terminal(pool, &append, state, settlement, &items).await?)
        }
        None => None,
    };
    let store = match settlement {
        Some(settlement) => DatabaseRunStateStore::new(pool.clone())
            .with_owner_pod_id(settlement.expected_owner_pod_id),
        None => DatabaseRunStateStore::new(pool.clone()),
    };
    let mut connection = CancellationSafePoolConnection::acquire(pool.get())
        .await
        .map_err(|error| error.to_string())?;
    let mut tx = connection
        .begin()
        .await
        .map_err(|error| error.to_string())?;
    if append.expected_owner_generation.is_some() {
        astra_services::storage::admit_session_execution_write(
            &mut tx,
            append.session_id,
            append.user_id,
        )
        .await
    } else {
        astra_services::storage::admit_session_scoped_run_write(
            &mut tx,
            append.session_id,
            append.user_id,
            append.run_id,
        )
        .await
    }
    .map_err(|error| format!("canonical session admission failed: {error}"))?;
    let mut control_events = Vec::new();
    let mut admission = None;
    if let (Some(current), Some(prepared)) = (settlement.as_mut(), plan.as_mut()) {
        // At most one accepted cancellation can change the proposed outcome.
        // It converges here, under the same fence and transaction as raw/Ctx.
        for attempt in 0..2 {
            match store
                .claim_canonical_terminal_with_controls_in_transaction(
                    &mut tx,
                    atomic_terminal_request(
                        &append,
                        *current,
                        prepared.receipt.interruption.as_ref(),
                    ),
                    &prepared.receipt,
                )
                .await?
            {
                CanonicalTerminalClaim::Claimed {
                    admission: proof,
                    control_events: controls,
                } => {
                    admission = Some(proof);
                    control_events = controls;
                    break;
                }
                CanonicalTerminalClaim::CancellationRequested if attempt == 0 => {
                    *current = CanonicalTerminalSettlement {
                        status: STATUS_CANCELLED,
                        expected_statuses: &[STATUS_RUNNING, STATUS_WAITING, STATUS_PAUSED],
                        context_commit: None,
                        cancellation_origin: Some(
                            astra_turn_core::orchestration_types::CancellationOrigin::User,
                        ),
                        error_code: None,
                        error_kind: None,
                        error_metadata: None,
                        ..*current
                    };
                    prepared.receipt.outcome = CanonicalTerminalOutcome::Cancelled {
                        origin: DurableCancellationOrigin::User,
                    };
                    prepared.receipt.context = if prepared.authority.is_some() {
                        CanonicalTerminalContext::Revoked
                    } else {
                        CanonicalTerminalContext::Unowned
                    };
                    prepared.receipt.error_code = None;
                    prepared.receipt.error_kind = None;
                    prepared.receipt.error_metadata = None;
                    prepared.receipt.interruption = None;
                }
                CanonicalTerminalClaim::Paused => {
                    return Err(
                        "Run paused before canonical terminal; checkpoint custody remains active"
                            .into(),
                    );
                }
                _ => break,
            }
        }
        if admission.is_none() {
            tx.rollback().await.map_err(|error| error.to_string())?;
            drop(connection);
            if store
                .resolve_canonical_terminal_receipt(
                    atomic_terminal_request(
                        &append,
                        *current,
                        prepared.receipt.interruption.as_ref(),
                    ),
                    &prepared.receipt,
                )
                .await?
                .is_none()
            {
                return Err(format!(
                    "canonical terminal lost durable execution authority at generation {}",
                    current.expected_owner_generation
                ));
            }
            let cursor = match &prepared.receipt.context {
                CanonicalTerminalContext::Advanced { cursor, .. } => Some(cursor),
                _ => None,
            };
            verify_canonical_append_evidence(pool, &append, state, cursor).await?;
            verify_terminal_context_evidence(pool, *current, &prepared.receipt).await?;
            if let Some(operation) = terminal_operation.take() {
                operation.retire();
            }
            return canonical_terminal_projection(
                &append,
                &items,
                prepared,
                current.error_message,
                Vec::new(),
            );
        }
    } else if let Some(generation) = append.expected_owner_generation {
        let duration = append
            .owner_lease_duration
            .ok_or("canonical append has no durable lease duration")?;
        let affected = sqlx::query("UPDATE agent_runs SET owner_lease_expires_at = DATE_ADD(NOW(6), INTERVAL ? MICROSECOND)
            WHERE user_id = ? AND session_id = ? AND run_id = ? AND run_generation = ?
              AND owner_lease_expires_at >= NOW(6) AND status IN ('running','waiting')")
            .bind(i64::try_from(duration.as_micros()).unwrap_or(i64::MAX)).bind(append.user_id).bind(append.session_id).bind(append.run_id)
            .bind(i64::try_from(generation).map_err(|_| "Run generation exceeds BIGINT")?).execute(&mut *tx).await.map_err(|error| error.to_string())?.rows_affected();
        if affected != 1 {
            return Err(format!(
                "canonical append lost durable execution authority at generation {generation}"
            ));
        }
    }
    let canonical_context_cursor = if let (Some(current), Some(prepared), Some(admission)) =
        (settlement, plan.as_ref(), admission.as_ref())
    {
        let coordinator = DatabaseSessionContextCoordinator::new(pool.clone());
        match &prepared.receipt.context {
            CanonicalTerminalContext::Advanced { .. } => Some(
                coordinator
                    .commit_turn_in_terminal_transaction(
                        &mut tx,
                        current
                            .context_commit
                            .ok_or("advanced terminal has no prepared Context")?,
                        admission,
                    )
                    .await
                    .map_err(|error| error.to_string())?,
            ),
            CanonicalTerminalContext::Unchanged => {
                coordinator
                    .validate_unchanged_run_context_in_terminal_transaction(
                        &mut tx,
                        prepared
                            .authority
                            .as_ref()
                            .ok_or("empty terminal has no admitted base")?,
                        &prepared.receipt.opening_hash,
                        admission,
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                None
            }
            CanonicalTerminalContext::Revoked | CanonicalTerminalContext::Unowned => None,
        }
    } else {
        None
    };
    let fresh = admission.as_ref().map(|admission| FreshTerminalTranscript {
        admission,
        run_id: append.run_id,
        cursor: canonical_context_cursor.as_ref(),
    });
    append_session_transcript_items_in_tx(
        &mut tx,
        append.user_id,
        append.session_id,
        &items,
        fresh,
    )
    .await
    .map_err(|error| format!("canonical raw append failed: {error}"))?;
    match tx.commit().await {
        Ok(()) => connection.release(),
        Err(error) => {
            drop(connection);
            let (Some(current), Some(prepared)) = (settlement, plan.as_ref()) else {
                return Err(format!("canonical commit acknowledgement failed: {error}"));
            };
            if store
                .resolve_canonical_terminal_receipt(
                    atomic_terminal_request(
                        &append,
                        current,
                        prepared.receipt.interruption.as_ref(),
                    ),
                    &prepared.receipt,
                )
                .await?
                .is_none()
            {
                return Err(format!(
                    "canonical commit acknowledgement unresolved: {error}"
                ));
            }
            verify_canonical_append_evidence(
                pool,
                &append,
                state,
                canonical_context_cursor.as_ref(),
            )
            .await?;
            verify_terminal_context_evidence(pool, current, &prepared.receipt).await?;
        }
    }
    if admission.is_some()
        && let Some(operation) = terminal_operation.take()
    {
        operation.retire();
    }
    if let (Some(current), Some(prepared)) = (settlement, plan.as_ref()) {
        canonical_terminal_projection(
            &append,
            &items,
            prepared,
            current.error_message,
            control_events,
        )
    } else {
        Ok(CanonicalTerminalSettlementCommit {
            terminal_status: None,
            canonical_context_cursor: None,
            terminal_events: Vec::new(),
            terminal_assistant_source_event_id: items
                .iter()
                .rev()
                .find(|item| item.role == "assistant")
                .map(|item| item.source_event_id.clone()),
        })
    }
}

fn canonical_terminal_projection(
    _append: &CanonicalLoopAppend<'_>,
    items: &[TranscriptPersistItem],
    plan: &PreparedCanonicalTerminal,
    error_message: Option<&str>,
    mut controls: Vec<Value>,
) -> Result<CanonicalTerminalSettlementCommit, String> {
    let assistant = items.iter().find(|item| {
        Some(&item.source_event_id) == plan.receipt.transcript.assistant_source_event_id.as_ref()
    });
    controls.extend(
        plan.receipt.completion_events(
            assistant.map(|item| item.content.as_str()),
            assistant
                .and_then(|item| item.payload.as_ref())
                .and_then(|payload| payload.model_item_id.as_deref()),
            error_message,
            None,
            -1,
            i64::MAX,
        )?,
    );
    Ok(CanonicalTerminalSettlementCommit {
        terminal_status: Some(plan.receipt.outcome.status()),
        canonical_context_cursor: match &plan.receipt.context {
            CanonicalTerminalContext::Advanced { cursor, .. } => Some(cursor.clone()),
            _ => None,
        },
        terminal_events: controls,
        terminal_assistant_source_event_id: plan
            .receipt
            .transcript
            .assistant_source_event_id
            .clone(),
    })
}

pub(crate) fn restore_step_checkpoint_runtime_state(
    restored: astra_pipeline::step_restore::RestoredSession,
    current_date: &str,
    loop_state: &mut AgenticLoopState,
) -> Result<(), astra_core::ClassifiedError> {
    // This entrypoint restores advisory session state, not execution custody.
    // The recovery scanner preserves/pauses handoffs but has no reconstructed
    // executor consumer. Do not erase a same-run fence by accepting its history
    // into fresh MessagingState. A claimed newer generation is still the same
    // execution and requires the real adoption/frontier proof, not an equality
    // check against the old producer generation.
    if let Some(astra_pipeline::step_protocol::RunExecutionBudget::V1 { run_id, .. }) =
        &restored.run_execution_budget
        && loop_state.current_run_id.as_deref() == Some(run_id.as_str())
    {
        return Err(astra_core::ClassifiedError::new(
            astra_core::ErrorKind::ContractViolation,
            "same-run checkpoint requires validated execution reconstruction; session warm-start cannot restore its completion fence",
        ));
    }
    loop_state.deferred_tool_activations =
        astra_turn_core::tool::deferred_activation::merged_deferred_tool_activations(
            &restored.messages,
            std::mem::take(&mut loop_state.deferred_tool_activations)
                .into_iter()
                .chain(restored.deferred_tool_activations),
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
    // `blocked_tools` is an unscoped snapshot from an older execution
    // boundary. It has no owner, request, capability, or expiry provenance,
    // so restoring it would turn a transient/possibly stale decision into a
    // silent hard schema filter for a new request. Re-derive hard restrictions
    // from the current request, permission, capability, and interaction
    // boundaries instead. Explicit resume restrictions have their own typed
    // interruption field and are applied by the owning client boundary.
    if !restored.blocked_tools.is_empty() {
        tracing::warn!(
            target: "astra_runtime::recovery",
            blocked_tools = ?restored.blocked_tools,
            "ignoring unscoped checkpoint tool restrictions; re-derived from current request policy"
        );
    }
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
    Ok(())
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

pub(crate) fn server_input_source_event_id(session_id: &str, run_id: &str) -> String {
    trace_event_id("user", &[session_id, &server_turn_id(run_id)])
}

pub(crate) fn server_trace_context(
    user_id: &str,
    session_id: &str,
    run_id: &str,
    turn_seq: u32,
) -> TraceContext {
    let turn_id = server_turn_id(run_id);
    TraceContext {
        root_event_id: server_input_source_event_id(session_id, run_id),
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
        root_event_id: server_input_source_event_id(&session_id, run_id),
        session_id,
        user_id: context.get("trace_user_id")?.as_str()?.to_string(),
        turn_id,
        turn_seq,
        causal_chain_id: context.get("trace_causal_chain_id")?.as_str()?.to_string(),
    })
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) model_item_id: Option<String>,
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
        payload: state
            .final_text_model_item_id
            .as_ref()
            .map(|id| TranscriptPersistPayload {
                model_item_id: Some(id.clone()),
                ..Default::default()
            }),
        source_event_id: trace_event_id("response", &[run_id, &trace.turn_id]),
    })
}

/// Persist accepted input under the enclosing Session/Run transaction.
#[cfg(test)]
pub(crate) async fn persist_root_turn_input(
    pool: &SharedPool,
    user_id: &str,
    session_id: &str,
    item: TranscriptPersistItem,
) -> Result<(), String> {
    let mut connection = CancellationSafePoolConnection::acquire(pool.get())
        .await
        .map_err(|error| error.to_string())?;
    let mut tx = connection
        .begin()
        .await
        .map_err(|error| error.to_string())?;
    admit_session_event_write(&mut tx, session_id, user_id, false)
        .await
        .map_err(|error| error.to_string())?;
    append_session_transcript_items_admitted_in_tx(&mut tx, user_id, session_id, &[item])
        .await
        .map_err(|error| error.to_string())?;
    tx.commit().await.map_err(|error| error.to_string())?;
    connection.release();
    Ok(())
}

#[cfg(test)]
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
    let result = async {
        admit_session_event_write(&mut tx, session_id, user_id, false).await?;
        append_session_transcript_items_admitted_in_tx(&mut tx, user_id, session_id, items).await
    }
    .await;
    if let Err(error) = result {
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
    model_item_id: Option<String>,
    text: String,
    done: bool,
}

impl TranscriptReasoningProjection {
    fn append_delta(&mut self, delta: &str) {
        if !delta.is_empty() {
            self.text.push_str(delta);
        }
    }

    fn is_empty(&self) -> bool {
        self.text.is_empty() && !self.done
    }

    fn matches_payload(&self, payload: Option<&TranscriptPersistPayload>) -> bool {
        payload.and_then(|payload| payload.model_item_id.as_ref()) == self.model_item_id.as_ref()
    }
}

fn payload_with_reasoning_projection(
    payload: Option<TranscriptPersistPayload>,
    reasoning: &TranscriptReasoningProjection,
) -> Option<TranscriptPersistPayload> {
    if reasoning.is_empty() {
        return payload;
    }
    if !reasoning.matches_payload(payload.as_ref()) {
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
    if matches!(
        event_type,
        Some(
            "reasoning_delta"
                | "thinking_delta"
                | "reasoning_message_content"
                | "reasoning_done"
                | "thinking_done"
        )
    ) {
        let model_item_id = payload
            .get("model_item_id")
            .or_else(|| payload.pointer("/data/model_item_id"))
            .and_then(Value::as_str)
            .map(str::to_string);
        if projection.model_item_id != model_item_id {
            *projection = TranscriptReasoningProjection {
                model_item_id,
                ..Default::default()
            };
        }
    }
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
    if !reasoning.matches_payload(payload.as_ref()) {
        return Ok(());
    }
    let Some(payload) = payload_with_reasoning_projection(payload, reasoning) else {
        // Reasoning from another model item cannot materialize this row.
        return Ok(());
    };
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
    Ok(())
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
    if let Err(error) = admit_session_event_write(&mut tx, session_id, user_id, false).await {
        return Err(rollback_materialized_transcript_transaction(
            tx,
            "admitting transcript materialization",
            error,
        )
        .await);
    }
    if let Err(error) = append_session_transcript_items_admitted_in_tx(
        &mut tx,
        user_id,
        session_id,
        &evidence_items,
    )
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
            bind_run_transcript_to_cursor_in_tx(&mut tx, user_id, session_id, run_id, cursor).await
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

fn validated_transcript_cursor<'a>(
    user_id: &str,
    session_id: &str,
    cursor: &'a SessionCursorV1,
) -> Result<(i64, i64, &'a str), sqlx::Error> {
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
    let completed_turn = i64::from(cursor.completed_turn);
    let conversation_seq = transcript_cursor_i64("conversation_seq", cursor.conversation_seq)?;
    Ok((
        completed_turn,
        conversation_seq,
        cursor.canonical_root_hash.as_str(),
    ))
}

/// Bind original raw history to the Context cursor issued in this transaction.
/// The caller owns the Session fence and the canonical commit; this helper does
/// not derive conversation truth from trace, a display row, or a second head.
async fn bind_run_transcript_to_cursor_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    user_id: &str,
    session_id: &str,
    run_id: &str,
    cursor: &SessionCursorV1,
) -> Result<(), sqlx::Error> {
    let (completed_turn, conversation_seq, root) =
        validated_transcript_cursor(user_id, session_id, cursor)?;
    sqlx::query(
        "UPDATE session_transcript_items
         SET canonical_completed_turn = ?, canonical_conversation_seq = ?,
             canonical_root_hash = ?
         WHERE user_id = ? AND session_id = ? AND run_id = ?
           AND canonical_completed_turn IS NULL",
    )
    .bind(completed_turn)
    .bind(conversation_seq)
    .bind(root)
    .bind(user_id)
    .bind(session_id)
    .bind(run_id)
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

// Bounds apply to SQL parameters and their encoded payload, independently.
// A single indivisible item may exceed the byte target; it is sent alone.
const TRANSCRIPT_BATCH_BINDS: usize = 512;
const TRANSCRIPT_BATCH_BYTES: usize = 256 * 1024;
const TRANSCRIPT_MEMBERSHIP_ROWS: usize = 128;
const TRANSCRIPT_INSERT_BINDS: usize = 6;

/// A raw identity is immutable. Reasoning is a separate presentation enrichment;
/// all other JSON fields, including unknown fields, remain part of identity.

struct PreparedTranscriptItem<'a> {
    item: &'a TranscriptPersistItem,
    payload_json: Option<String>,
    payload_identity: Option<String>,
    bytes: usize,
}

impl<'a> PreparedTranscriptItem<'a> {
    fn new(item: &'a TranscriptPersistItem) -> Result<Self, sqlx::Error> {
        let payload_json = item
            .payload
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|error| {
                sqlx::Error::Protocol(format!("serialize transcript payload: {error}"))
            })?;
        let payload_identity = transcript_payload_identity(item.role, payload_json.as_deref())
            .map_err(sqlx::Error::Protocol)?;
        let bytes = item.source_event_id.len()
            + item.run_id.as_ref().map_or(0, String::len)
            + item.role.len()
            + item.content.len()
            + payload_json.as_ref().map_or(0, String::len)
            + 128;
        Ok(Self {
            item,
            payload_json,
            payload_identity,
            bytes,
        })
    }

    fn matches_candidate(&self, other: &Self) -> bool {
        self.item.run_id == other.item.run_id
            && self.item.role == other.item.role
            && self.item.content == other.item.content
            && self.payload_identity == other.payload_identity
    }

    fn matches_stored(
        &self,
        stored: &StoredCanonicalTranscriptEvidence,
    ) -> Result<bool, sqlx::Error> {
        Ok(self.item.run_id == stored.run_id
            && self.item.role == stored.role
            && self.item.content == stored.content
            && stored.content_hash
                == transcript_content_hash(
                    &stored.role,
                    &stored.content,
                    stored.payload_json.as_deref(),
                )
            && self.payload_identity
                == transcript_payload_identity(&stored.role, stored.payload_json.as_deref())
                    .map_err(sqlx::Error::Protocol)?)
    }
}

fn transcript_chunk<'a>(
    items: &'a [TranscriptPersistItem],
    start: usize,
) -> Result<Vec<PreparedTranscriptItem<'a>>, sqlx::Error> {
    let mut chunk = Vec::new();
    let mut bytes = 0_usize;
    for item in items.iter().skip(start).take(TRANSCRIPT_MEMBERSHIP_ROWS) {
        let prepared = PreparedTranscriptItem::new(item)?;
        if !chunk.is_empty() && bytes.saturating_add(prepared.bytes) > TRANSCRIPT_BATCH_BYTES {
            break;
        }
        bytes = bytes.saturating_add(prepared.bytes);
        chunk.push(prepared);
    }
    Ok(chunk)
}

/// Return absent representative ordinals after validating every raw identity.
/// The database supplies alias groups, preserving its column equality rules.
async fn validate_transcript_chunk<'e, E>(
    executor: E,
    user_id: &str,
    session_id: &str,
    items: &[PreparedTranscriptItem<'_>],
    expected_cursor: Option<&SessionCursorV1>,
) -> Result<Vec<usize>, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::MySql>,
{
    if items.is_empty() {
        return Ok(Vec::new());
    }
    let mut query = sqlx::QueryBuilder::<sqlx::MySql>::new(
        "WITH candidates AS (SELECT -1 AS input_ordinal, source_event_id FROM session_transcript_items WHERE 1 = 0",
    );
    for (ordinal, item) in items.iter().enumerate() {
        query
            .push(" UNION ALL SELECT ")
            .push(ordinal)
            .push(", ")
            .push_bind(&item.item.source_event_id);
    }
    query.push(") , identities AS (SELECT source_event_id, MIN(input_ordinal) AS first_ordinal FROM candidates GROUP BY source_event_id)
        SELECT c.input_ordinal, i.first_ordinal, s.item_seq,
               s.source_event_id AS stored_source_event_id, s.run_id, s.role, s.content, s.payload_json, s.content_hash,
               s.canonical_completed_turn, s.canonical_conversation_seq, s.canonical_root_hash
        FROM candidates c JOIN identities i ON i.source_event_id = c.source_event_id
        LEFT JOIN session_transcript_items s ON c.input_ordinal = i.first_ordinal AND s.source_event_id = i.source_event_id AND s.user_id = ");
    query
        .push_bind(user_id)
        .push(" AND s.session_id = ")
        .push_bind(session_id)
        .push(" ORDER BY c.input_ordinal, s.item_seq");
    let rows = query.build().fetch_all(executor).await?;
    let mut missing = Vec::new();
    let mut next_ordinal = 0;
    for row in rows {
        let ordinal = usize::try_from(row.try_get::<i64, _>("input_ordinal")?)
            .map_err(|_| sqlx::Error::Protocol("negative transcript ordinal".into()))?;
        let first = usize::try_from(row.try_get::<i64, _>("first_ordinal")?)
            .map_err(|_| sqlx::Error::Protocol("negative transcript representative".into()))?;
        if ordinal != next_ordinal || first > ordinal || first >= items.len() {
            return Err(sqlx::Error::Protocol(
                "ambiguous or invalid transcript identity evidence".into(),
            ));
        }
        next_ordinal += 1;
        let candidate = &items[ordinal];
        let representative = &items[first];
        if !candidate.matches_candidate(representative) {
            return Err(sqlx::Error::Protocol(format!(
                "conflicting transcript identity {}",
                candidate.item.source_event_id
            )));
        }
        if first != ordinal {
            continue;
        }
        if row.try_get::<Option<i64>, _>("item_seq")?.is_none() {
            missing.push(ordinal);
            continue;
        }
        let stored = StoredCanonicalTranscriptEvidence {
            source_event_id: row.try_get("stored_source_event_id")?,
            run_id: row.try_get("run_id")?,
            role: row.try_get("role")?,
            content: row.try_get("content")?,
            payload_json: row.try_get("payload_json")?,
            content_hash: row.try_get("content_hash")?,
        };
        if !representative.matches_stored(&stored)? {
            return Err(sqlx::Error::Protocol(format!(
                "conflicting transcript identity {}",
                stored.source_event_id
            )));
        }
        if let Some(cursor) = expected_cursor {
            if row.try_get::<Option<i64>, _>("canonical_completed_turn")?
                != Some(i64::from(cursor.completed_turn))
                || row.try_get::<Option<i64>, _>("canonical_conversation_seq")?
                    != Some(i64::try_from(cursor.conversation_seq).map_err(|_| {
                        sqlx::Error::Protocol("transcript cursor exceeds BIGINT".into())
                    })?)
                || row
                    .try_get::<Option<String>, _>("canonical_root_hash")?
                    .as_deref()
                    != Some(cursor.canonical_root_hash.as_str())
            {
                return Err(sqlx::Error::Protocol(
                    "transcript commitment does not match canonical Context".into(),
                ));
            }
        }
    }
    if next_ordinal != items.len() {
        return Err(sqlx::Error::Protocol(
            "incomplete transcript identity evidence".into(),
        ));
    }
    Ok(missing)
}

/// Append only after canonical session admission in this same transaction.
/// The caller owns commit/rollback, including any enclosing event capture.
/// Identity equality belongs to the database column's collation, not Rust.
pub(crate) async fn append_session_transcript_items_admitted_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    user_id: &str,
    session_id: &str,
    items: &[TranscriptPersistItem],
) -> Result<(), sqlx::Error> {
    append_session_transcript_items_in_tx(tx, user_id, session_id, items, None).await
}

struct FreshTerminalTranscript<'a> {
    admission: &'a astra_services::runs::RunMutationAdmission,
    run_id: &'a str,
    cursor: Option<&'a SessionCursorV1>,
}

/// The one raw writer. Native UNIQUE source identities and a freshly claimed
/// terminal authorize a direct append; generic producers retain D14's complete
/// immutable-membership validation. Every chunk still allocates inline from
/// the current Session tail, including child interleaving.
async fn append_session_transcript_items_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    user_id: &str,
    session_id: &str,
    items: &[TranscriptPersistItem],
    fresh: Option<FreshTerminalTranscript<'_>>,
) -> Result<(), sqlx::Error> {
    let cursor_columns = if let Some(fresh) = &fresh {
        if !fresh
            .admission
            .authorizes_terminal_transcript(user_id, session_id, fresh.run_id)
            || items
                .iter()
                .any(|item| item.run_id.as_deref() != Some(fresh.run_id))
        {
            return Err(sqlx::Error::Protocol(
                "raw append requires this Run's exact terminal proof".into(),
            ));
        }
        fresh
            .cursor
            .map(|cursor| validated_transcript_cursor(user_id, session_id, cursor))
            .transpose()?
    } else {
        None
    };
    let mut offset = 0;
    while offset < items.len() {
        let chunk = transcript_chunk(items, offset)?;
        let end = offset + chunk.len();
        let ordinals = if fresh.is_some() {
            (0..chunk.len()).collect()
        } else {
            validate_transcript_chunk(&mut **tx, user_id, session_id, &chunk, None).await?
        };
        let mut pending = Vec::new();
        let mut pending_bytes = 0_usize;
        for ordinal in ordinals {
            let prepared = &chunk[ordinal];
            let item_bytes = prepared
                .bytes
                .saturating_add(session_id.len())
                .saturating_add(user_id.len());
            pending.push((prepared.item, &prepared.payload_json, item_bytes));
        }
        let mut start = 0;
        while start < pending.len() {
            let mut stop = start;
            while stop < pending.len()
                && (stop - start + 1) * TRANSCRIPT_INSERT_BINDS + 7 <= TRANSCRIPT_BATCH_BINDS
            {
                let item_bytes = pending[stop].2;
                if stop > start && pending_bytes.saturating_add(item_bytes) > TRANSCRIPT_BATCH_BYTES
                {
                    break;
                }
                pending_bytes = pending_bytes.saturating_add(item_bytes);
                stop += 1;
            }
            let mut insert = sqlx::QueryBuilder::<sqlx::MySql>::new(
                "INSERT INTO session_transcript_items
                 (session_id, item_seq, user_id, run_id, role, content, payload_json,
                  source_event_id, content_hash, canonical_completed_turn,
                  canonical_conversation_seq, canonical_root_hash, created_at) ",
            );
            // Session admission serializes this namespace. Allocate from its
            // current tail inside the INSERT, including across bounded chunks;
            // no cached sequence or separate MAX round trip is required.
            insert.push("SELECT ").push_bind(session_id)
                .push(", base.seq + candidates.ordinal, ").push_bind(user_id)
                .push(", candidates.run_id, candidates.role, candidates.content, candidates.payload_json,
                    candidates.source_event_id, candidates.content_hash, ")
                .push_bind(cursor_columns.map(|(turn, _, _)| turn)).push(", ")
                .push_bind(cursor_columns.map(|(_, sequence, _)| sequence)).push(", ")
                .push_bind(cursor_columns.map(|(_, _, root)| root))
                .push(", NOW(6)
                    FROM (SELECT COALESCE(MAX(item_seq), 0) AS seq FROM session_transcript_items
                          WHERE user_id = ")
                .push_bind(user_id).push(" AND session_id = ").push_bind(session_id)
                .push(") base CROSS JOIN (VALUES ");
            for (ordinal, (item, payload, _)) in pending[start..stop].iter().enumerate() {
                if ordinal != 0 {
                    insert.push(", ");
                }
                insert
                    .push("ROW(")
                    .push(ordinal + 1)
                    .push(", ")
                    .push_bind(&item.run_id)
                    .push(", ")
                    .push_bind(item.role)
                    .push(", ")
                    .push_bind(&item.content)
                    .push(", ")
                    .push_bind(payload)
                    .push(", ")
                    .push_bind(&item.source_event_id)
                    .push(", ")
                    .push_bind(transcript_content_hash(
                        item.role,
                        &item.content,
                        payload.as_deref(),
                    ))
                    .push(")");
            }
            insert.push(") candidates(ordinal, run_id, role, content, payload_json, source_event_id, content_hash)
                WHERE base.seq <= ").push(i64::MAX - (stop - start) as i64);
            insert.push(astra_core::matrixone_null_shape_comment(
                [cursor_columns.is_some(); 3].into_iter().chain(
                    pending[start..stop]
                        .iter()
                        .flat_map(|(item, payload, _)| [item.run_id.is_some(), payload.is_some()]),
                ),
            ));
            let affected = insert.build().execute(&mut **tx).await?.rows_affected();
            if affected != (stop - start) as u64 {
                return Err(sqlx::Error::Protocol(
                    "transcript sequence exhausted".into(),
                ));
            }
            start = stop;
            pending_bytes = 0;
        }
        offset = end;
    }
    Ok(())
}

/// Persist the existing skill-selection telemetry after the server loop.
async fn persist_server_loop_hook_events(
    hook_db_writer: &dyn TurnHookDbWriter,
    user_id: &str,
    session_id: &str,
    user_message: &str,
    state: &AgenticLoopState,
) -> Result<(), String> {
    // Skill selection is an explicit judgment, not an inference from ordinary
    // tool execution. Tool lifecycle and result facts are already persisted by
    // the canonical trace path; duplicating tool names as skill selections
    // adds a write and corrupts skill metrics.
    let selected_skills = state.telemetry.all_selected_skills.clone();
    let Some(skill_name) = selected_skills.first().cloned() else {
        return Ok(());
    };
    let plan = TurnHookDbPersistPlan {
        skill_selection: Some(TurnSkillSelectionRecord {
            event_id: Uuid::now_v7().to_string(),
            session_id: session_id.to_string(),
            user_id: user_id.to_string(),
            agent_id: None,
            user_query: truncate_for_audit(user_message, 2000),
            selected_skills,
            skill_name,
            skill_version: None,
            selection_method: "llm_skill_choice".to_string(),
            execution_success: Some(1),
            execution_time_ms: None,
        }),
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
    let outcome = match observer_worker.run(request).await {
        Ok(TurnObserverOutcome::Observed) => "success",
        Ok(TurnObserverOutcome::NotConfigured) => "not_configured",
        Ok(TurnObserverOutcome::AccessDisabled) => "access_disabled",
        Ok(TurnObserverOutcome::Empty) => "empty",
        Err(e) => {
            astra_core::agent_error!(
                "server-loop",
                "failed to run observer for session {session_id}: {e}"
            );
            "error"
        }
    };
    record_turn_observer_run_metrics(metrics_registry, mode, outcome);
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
    use sqlx::Row;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Notify;
    use tracing::instrument::WithSubscriber;
    use tracing_subscriber::prelude::*;
    use uuid::Uuid;

    #[derive(Clone, Default)]
    struct PersistenceQueries(Arc<std::sync::Mutex<Vec<String>>>);

    impl tracing::field::Visit for PersistenceQueries {
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            // SQLx emits transaction BEGIN through summary, with an empty
            // db.statement. Ordinary statements retain their full SQL field.
            if field.name() == "db.statement" || (field.name() == "summary" && value == "BEGIN") {
                self.0.lock().unwrap().push(value.to_owned());
            }
        }
        fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for PersistenceQueries {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if event.metadata().target() == "sqlx::query" {
                event.record(&mut self.clone());
            }
        }
    }

    // Cache schema bootstrap, not sockets owned by a previous test's Tokio runtime.
    static SHARED_BOOTSTRAP: tokio::sync::OnceCell<MatrixOneSettings> =
        tokio::sync::OnceCell::const_new();

    async fn assert_session_event_count(
        db: &sqlx::MySqlPool,
        user_id: &str,
        session_id: &str,
    ) -> i64 {
        let (recorded, actual): (i64, i64) = sqlx::query_as(
            "SELECT sessions.event_count,
                    (SELECT COUNT(*) FROM agent_events events
                     WHERE events.user_id = sessions.user_id
                       AND events.session_id = sessions.session_id)
             FROM agent_sessions sessions WHERE user_id = ? AND session_id = ?",
        )
        .bind(user_id)
        .bind(session_id)
        .fetch_one(db)
        .await
        .expect("read session counter and committed events");
        assert_eq!(
            recorded, actual,
            "session counter must match committed events"
        );
        recorded
    }

    async fn setup_pool() -> astra_core::SharedPool {
        assert_eq!(
            std::env::var("ASTRA_TEST_DB_IT").as_deref(),
            Ok("1"),
            "set ASTRA_TEST_DB_IT=1 for ignored integration tests"
        );
        let settings = SHARED_BOOTSTRAP
            .get_or_init(|| async {
                let settings = astra_core::MatrixOneSettings::from_env();
                let catalog = std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG")
                    .unwrap_or_else(|_| "mysql".to_string());
                astra_services::ensure_core_schema(&settings, &catalog)
                    .await
                    .expect("ensure_core_schema");
                settings
            })
            .await;
        astra_core::SharedPool::new(settings)
            .await
            .expect("SharedPool::new")
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
    async fn root_input_is_atomic_replay_safe_and_collision_fenced() {
        let pool = setup_pool().await;
        let db = pool.get();
        let user = Uuid::new_v4().to_string();
        let session = Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO agent_sessions (session_id,user_id,title,status,event_count) VALUES (?,?,'input','active',0)")
            .bind(&session).bind(&user).execute(db).await.unwrap();
        let input = |run: &str, content: &str| TranscriptPersistItem {
            run_id: Some(run.into()),
            role: "user",
            content: content.into(),
            payload: None,
            source_event_id: server_trace_context(&user, &session, run, 1).root_event_id,
        };
        for _ in 0..2 {
            persist_root_turn_input(&pool, &user, &session, input("r", "original request"))
                .await
                .unwrap();
        }
        persist_root_turn_input(&pool, &user, &session, input("r", "different content"))
            .await
            .unwrap_err();
        let rows: Vec<(String,String)> = sqlx::query_as("SELECT source_event_id, content FROM session_transcript_items WHERE user_id=? AND session_id=?")
            .bind(&user).bind(&session).fetch_all(db).await.unwrap();
        assert_eq!(
            rows,
            vec![(input("r", "").source_event_id, "original request".into())]
        );
        assert_eq!(
            assert_session_event_count(db, &user, &session).await,
            0,
            "ordinary input has no diagnostic copy"
        );
        sqlx::query(
            "UPDATE session_transcript_items SET item_seq=? WHERE user_id=? AND session_id=?",
        )
        .bind(i64::MAX)
        .bind(&user)
        .bind(&session)
        .execute(db)
        .await
        .unwrap();
        persist_root_turn_input(&pool, &user, &session, input("next", "new input"))
            .await
            .unwrap_err();
        sqlx::query(
            "UPDATE session_transcript_items SET item_seq=1 WHERE user_id=? AND session_id=?",
        )
        .bind(&user)
        .bind(&session)
        .execute(db)
        .await
        .unwrap();
        persist_root_turn_input(&pool, &user, &session, input("next", "new input"))
            .await
            .unwrap();
        persist_root_turn_input(
            &pool,
            "foreign-owner",
            &session,
            input("foreign", "wrong owner"),
        )
        .await
        .unwrap_err();
        sqlx::query("UPDATE agent_sessions SET status='deleting' WHERE user_id=? AND session_id=?")
            .bind(&user)
            .bind(&session)
            .execute(db)
            .await
            .unwrap();
        persist_root_turn_input(&pool, &user, &session, input("late", "late input"))
            .await
            .unwrap_err();
        cleanup_core_persist_fixture_for_owner(db, &session, &user).await;
        persist_root_turn_input(&pool, &user, &session, input("missing", "missing root"))
            .await
            .unwrap_err();
        let roots: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_sessions WHERE user_id=? AND session_id=?",
        )
        .bind(&user)
        .bind(&session)
        .fetch_one(db)
        .await
        .unwrap();
        assert_eq!(roots, 0, "input must not recreate a deleted root");
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

    fn test_post_loop_persist_context(session_id: &str) -> PostLoopPersistContext {
        PostLoopPersistContext {
            matrixone: MatrixOneSettings::default(),
            shared_pool: None,
            user_id: "user-1".to_string(),
            session_id: session_id.to_string(),
            run_id: "run-1".to_string(),
            expected_owner_generation: None,
            owner_lease_duration: None,
            terminal_authority: None,
            owner_pod_id: None,

            user_message: "work".to_string(),
            hook_db_writer: None,
            observer_worker: None,
            metrics_registry: None,
        }
    }

    #[test]
    fn root_atomic_terminal_append_includes_recoverable_assistant_transcript() {
        let persist = test_post_loop_persist_context("root-terminal-transcript");
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
    async fn server_loop_empty_hook_skips_writer_and_selected_skill_is_retained() {
        let database_writer =
            crate::turn::services::DatabaseTurnHookDbWriter::new(MatrixOneSettings::default());
        database_writer
            .persist(TurnHookDbPersistPlan {
                skill_selection: None,
            })
            .await
            .unwrap();
        assert!(
            database_writer
                .persist(TurnHookDbPersistPlan {
                    skill_selection: Some(retained_skill_record("user-1", "session-1", "review")),
                })
                .await
                .is_err()
        );
        let writer = CaptureHookDbWriter::default();
        let mut state = crate::turn::agentic_loop::host::make_test_loop_state();
        state.final_text = "Ordinary answer".into();
        state.telemetry.all_tools_used.insert("read_file".into());
        persist_server_loop_hook_events(&writer, "user-1", "session-1", "work", &state)
            .await
            .expect("ordinary tools do not need a hook write");
        assert!(
            writer.plans.lock().expect("capture lock").is_empty(),
            "ordinary tools must not be persisted as skill selection"
        );

        state.telemetry.all_selected_skills.push("review".into());
        persist_server_loop_hook_events(&writer, "user-1", "session-1", "work", &state)
            .await
            .expect("selected skill persists");
        let plans = writer.plans.lock().expect("capture lock");
        assert_eq!(plans.len(), 1);
        let skill = plans[0].skill_selection.as_ref().expect("selected skill");
        assert_eq!(skill.skill_name, "review");
        assert_eq!(skill.selected_skills, vec!["review".to_string()]);
        assert_eq!(skill.selection_method, "llm_skill_choice");
        assert_eq!(skill.execution_success, Some(1));
    }

    fn retained_skill_record(user: &str, session: &str, skill: &str) -> TurnSkillSelectionRecord {
        TurnSkillSelectionRecord {
            event_id: Uuid::new_v4().to_string(),
            user_id: user.into(),
            session_id: session.into(),
            agent_id: None,
            user_query: "retained write".into(),
            selected_skills: vec![skill.into()],
            skill_name: skill.into(),
            skill_version: Some("supplied".into()),
            selection_method: "llm_skill_choice".into(),
            execution_success: Some(1),
            execution_time_ms: None,
        }
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne DB: run with ASTRA_TEST_DB_IT=1"]
    async fn ordinary_post_loop_needs_no_database_but_selected_skills_still_persist() {
        let pool = setup_pool().await;
        let user = format!("post-loop-user-{}", Uuid::new_v4());
        let session = format!("post-loop-session-{}", Uuid::new_v4());
        crate::server::run::insert_active_run_session_fixture(&pool, &user, &session).await;
        let mut persist = test_post_loop_persist_context(&session);
        persist.user_id = user.clone();
        persist.shared_pool = Some(pool.clone());
        persist.hook_db_writer = Some(Arc::new(
            crate::turn::services::DatabaseTurnHookDbWriter::new(pool.settings().clone())
                .with_pool(pool.clone()),
        ));
        let mut state = crate::turn::agentic_loop::host::make_test_loop_state();
        state.final_text = "Ordinary answer".into();
        state.telemetry.all_selected_skills.push("review".into());
        persist.run_after_core(&state, true, Ok(())).await.unwrap();
        let selected: Vec<String> = sqlx::query_scalar(
            "SELECT skill_name FROM skill_selection_events WHERE user_id = ? AND session_id = ?",
        )
        .bind(&user)
        .bind(&session)
        .fetch_all(pool.get())
        .await
        .unwrap();
        assert_eq!(selected, ["review"]);
        let skill = format!("retained-{}", Uuid::new_v4());
        let skill_id = Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO skills_registry (skill_id, skill_name, version, description, skill_definition, is_active, status, source, category, created_by) VALUES (?, ?, '2.0', 'retained test', '{}', 1, 'active', 'user', 'general', ?)")
            .bind(&skill_id).bind(&skill).bind(&user).execute(pool.get()).await.unwrap();
        let mut settings = pool.settings().clone();
        settings.db_pool_max_connections = 1;
        settings.db_pool_min_connections = 1;
        let single = astra_core::SharedPool::new(&settings).await.unwrap();
        let writer = crate::turn::services::DatabaseTurnHookDbWriter::new(settings.clone())
            .with_pool(single.clone());
        let other_user = Uuid::new_v4().to_string();
        let other_session = Uuid::new_v4().to_string();
        for (owner, sid) in [(&other_user, &session), (&user, &other_session)] {
            crate::server::run::insert_active_run_session_fixture(&pool, owner, sid).await;
        }
        let original = retained_skill_record(&user, &session, &skill);
        for (selected, expected) in [
            (vec![skill.clone()], "2.0"),
            (vec![format!("missing-{skill}"), skill.clone()], "supplied"),
            (vec![], "supplied"),
        ] {
            let record = TurnSkillSelectionRecord {
                event_id: Uuid::new_v4().to_string(),
                selected_skills: selected,
                ..original.clone()
            };
            tokio::time::timeout(
                Duration::from_secs(10),
                writer.persist(TurnHookDbPersistPlan {
                    skill_selection: Some(record.clone()),
                }),
            )
            .await
            .unwrap()
            .unwrap();
            let version: String = sqlx::query_scalar(
                "SELECT skill_version FROM skill_selection_events WHERE event_id = ?",
            )
            .bind(&record.event_id)
            .fetch_one(pool.get())
            .await
            .unwrap();
            assert_eq!(version, expected);
            for (owner, sid) in [
                (&user, &session),
                (&other_user, &session),
                (&user, &other_session),
            ] {
                let replay = TurnSkillSelectionRecord {
                    user_id: owner.clone(),
                    session_id: sid.clone(),
                    skill_version: Some("overwrite".into()),
                    selected_skills: vec![],
                    ..record.clone()
                };
                assert!(
                    writer
                        .persist(TurnHookDbPersistPlan {
                            skill_selection: Some(replay)
                        })
                        .await
                        .is_err()
                );
                assert_eq!(assert_session_event_count(pool.get(), owner, sid).await, 0);
            }
            let stored: (String, String, String) = sqlx::query_as("SELECT user_id, session_id, skill_version FROM skill_selection_events WHERE event_id = ?")
                .bind(&record.event_id).fetch_one(pool.get()).await.unwrap();
            assert_eq!(
                stored,
                (user.clone(), session.clone(), expected.to_string())
            );
        }
        // The real receipt read shares the only checkout with admission/INSERT.
        use astra_services::{ContextService, DecisionService, EventService};
        let event = DatabaseEventService::new(settings.clone())
            .with_pool(pool.clone())
            .create_event(
                user.clone(),
                EventCreateRequestData {
                    ingestion_source: astra_services::EventIngestionSource::Client,
                    event_id: None,
                    session_id: session.clone(),
                    event_type: "retained_test".into(),
                    content: "{}".into(),
                    agent_id: None,
                    agent_version: None,
                    parent_event_id: None,
                    parent_event_ids: None,
                    causal_chain_id: None,
                    metadata: None,
                },
            )
            .await
            .unwrap();
        let snapshot = astra_services::DatabaseContextService::new(settings.clone())
            .with_pool(pool.clone())
            .create_snapshot(
                user.clone(),
                astra_services::SnapshotCreateRequestData {
                    session_id: session.clone(),
                    event_id: event.record.event_id.clone(),
                    context_data: json!({"retained": true}),
                },
            )
            .await
            .unwrap();
        let decision_service = astra_services::DatabaseDecisionService::new(settings.clone())
            .with_pool(single.clone());
        let request = astra_services::DecisionCreateRequestData {
            session_id: session.clone(),
            event_id: event.record.event_id.clone(),
            context_capture_id: snapshot.context_capture_id,
            decision_type: "retained_test".into(),
            decision_output: json!({"ok": true}),
            model_params: None,
        };
        tokio::time::timeout(
            Duration::from_secs(10),
            decision_service.record_decision(user.clone(), request.clone()),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            assert_session_event_count(pool.get(), &user, &session).await,
            1
        );
        for (owner, sid) in [(&other_user, &session), (&user, &other_session)] {
            crate::server::run::cleanup_run_session_fixture(&pool, owner, sid).await;
        }
        let missing = TurnSkillSelectionRecord {
            session_id: Uuid::new_v4().to_string(),
            ..original.clone()
        };
        assert!(
            writer
                .persist(TurnHookDbPersistPlan {
                    skill_selection: Some(missing)
                })
                .await
                .is_err()
        );
        sqlx::query("UPDATE agent_session_lifecycle_fences SET delete_requested_at = NOW(6) WHERE user_id = ? AND session_id = ?")
            .bind(&user).bind(&session).execute(pool.get()).await.unwrap();
        assert!(
            writer
                .persist(TurnHookDbPersistPlan {
                    skill_selection: Some(original.clone())
                })
                .await
                .is_err()
        );
        assert_eq!(
            decision_service
                .record_decision(user.clone(), request.clone())
                .await
                .unwrap_err()
                .0,
            axum::http::StatusCode::NOT_FOUND
        );
        use astra_services::SessionService;
        astra_services::DatabaseSessionService::new(settings.clone())
            .with_pool(pool.clone())
            .delete_session(session.clone(), user.clone())
            .await
            .unwrap();
        assert!(
            writer
                .persist(TurnHookDbPersistPlan {
                    skill_selection: Some(original.clone())
                })
                .await
                .is_err()
        );
        assert_eq!(
            decision_service
                .record_decision(user.clone(), request.clone())
                .await
                .unwrap_err()
                .0,
            axum::http::StatusCode::NOT_FOUND
        );
        for table in [
            "agent_sessions",
            "skill_selection_events",
            "ctx_decision_audits",
        ] {
            let count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT COUNT(*) FROM {table} WHERE user_id = ? AND session_id = ?"
            )))
            .bind(&user)
            .bind(&session)
            .fetch_one(pool.get())
            .await
            .unwrap();
            assert_eq!(count, 0, "{table}");
        }
        sqlx::query("DELETE FROM skills_registry WHERE skill_id = ?")
            .bind(&skill_id)
            .execute(pool.get())
            .await
            .unwrap();
        single.close().await;
        assert_eq!(
            decision_service
                .record_decision(user.clone(), request)
                .await
                .unwrap_err()
                .0,
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "unavailable storage is not a missing session"
        );
        writer
            .persist(TurnHookDbPersistPlan {
                skill_selection: None,
            })
            .await
            .unwrap();
        assert!(
            writer
                .persist(TurnHookDbPersistPlan {
                    skill_selection: Some(original)
                })
                .await
                .is_err()
        );
        sqlx::query("DELETE FROM skill_selection_events WHERE user_id = ? AND session_id = ?")
            .bind(&user)
            .bind(&session)
            .execute(pool.get())
            .await
            .unwrap();
        crate::server::run::cleanup_run_session_fixture(&pool, &user, &session).await;
        pool.close().await;

        // Await the entire post-loop boundary, not just terminal SSE. A closed
        // pool makes an accidental COUNT/transaction deterministic, not a race
        // between our assertions and background persistence.
        state.telemetry.all_selected_skills.clear();
        persist
            .run_after_core(&state, true, Ok(()))
            .await
            .expect("ordinary post-loop must not access the closed database");
        state.telemetry.all_selected_skills.push("review".into());
        assert!(
            persist.run_after_core(&state, true, Ok(())).await.is_err(),
            "positive control: a real skill write must still require the database"
        );
    }

    #[cfg(feature = "e2e-hooks")]
    async fn wait_for_retained_fence_query(pool: &astra_core::SharedPool, connection_id: u64) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let queries: Vec<String> = sqlx::query_scalar(
                    "SELECT info FROM information_schema.processlist WHERE db = ? AND conn_id = ? AND info IS NOT NULL",
                ).bind(&pool.settings().database).bind(connection_id).fetch_all(pool.get()).await.unwrap();
                if queries.iter().any(|query| query.contains("agent_session_lifecycle_fences")) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.expect("observe the exact connection executing the blocked fence SQL");
    }

    #[cfg(feature = "e2e-hooks")]
    async fn write_retained_fixture(
        pool: astra_core::SharedPool,
        user: String,
        request: astra_services::DecisionCreateRequestData,
        skill: TurnSkillSelectionRecord,
        decision: bool,
        hook: Option<Arc<astra_services::decisions::RetainedWriteTestHook>>,
    ) -> Result<(), String> {
        if decision {
            use astra_services::DecisionService;
            let mut writer = astra_services::DatabaseDecisionService::new(pool.settings().clone())
                .with_pool(pool);
            if let Some(hook) = hook {
                writer = writer.with_retained_write_test_hook(hook);
            }
            writer
                .record_decision(user, request)
                .await
                .map(|_| ())
                .map_err(|error| {
                    assert!(matches!(
                        error.0,
                        axum::http::StatusCode::NOT_FOUND
                            | axum::http::StatusCode::INTERNAL_SERVER_ERROR
                    ));
                    error.1.0.detail
                })
        } else {
            let mut writer =
                crate::turn::services::DatabaseTurnHookDbWriter::new(pool.settings().clone())
                    .with_pool(pool);
            if let Some(hook) = hook {
                writer = writer.with_retained_write_test_hook(hook);
            }
            writer
                .persist(TurnHookDbPersistPlan {
                    skill_selection: Some(skill),
                })
                .await
        }
    }

    #[cfg(feature = "e2e-hooks")]
    #[tokio::test]
    #[ignore = "requires MatrixOne DB: run with ASTRA_TEST_DB_IT=1 and e2e-hooks"]
    async fn retained_skill_writer_orders_delete_rollback_and_cancellation_on_live_matrixone() {
        assert_retained_writer_lifecycle(false).await;
    }

    #[cfg(feature = "e2e-hooks")]
    #[tokio::test]
    #[ignore = "requires MatrixOne DB: run with ASTRA_TEST_DB_IT=1 and e2e-hooks"]
    async fn retained_decision_writer_orders_delete_rollback_and_cancellation_on_live_matrixone() {
        assert_retained_writer_lifecycle(true).await;
    }

    #[cfg(feature = "e2e-hooks")]
    async fn assert_retained_writer_lifecycle(decision: bool) {
        use astra_services::{ContextService, DecisionCreateRequestData, SessionService};
        use tokio_util::task::AbortOnDropHandle;
        // The strict-online profile owns the total live-DB test budget.
        // Individual contended operations keep their bounded waits below.
        let observer = setup_pool().await;
        let mut settings = observer.settings().clone();
        settings.db_pool_max_connections = 1;
        settings.db_pool_min_connections = 1;
        let worker = astra_core::SharedPool::new(&settings).await.unwrap();
        let deleter = astra_core::SharedPool::new(&settings).await.unwrap();
        let skill_name = format!("retained-race-{}", Uuid::new_v4());
        let skill_id = Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO skills_registry (skill_id, skill_name, version, is_active) VALUES (?, ?, '3.0', 1)")
            .bind(&skill_id).bind(&skill_name).execute(observer.get()).await.unwrap();
        for outcome in [
            "error",
            "cancel",
            "cancel_admission",
            "writer_first",
            "delete_first",
        ] {
            let user = Uuid::new_v4().to_string();
            let session = Uuid::new_v4().to_string();
            crate::server::run::insert_active_run_session_fixture(&observer, &user, &session).await;
            let event = DatabaseEventService::new(settings.clone())
                .with_pool(observer.clone())
                .create_event(
                    user.clone(),
                    EventCreateRequestData {
                        ingestion_source: astra_services::EventIngestionSource::Client,
                        event_id: None,
                        session_id: session.clone(),
                        event_type: "retained_race".into(),
                        content: "{}".into(),
                        agent_id: None,
                        agent_version: None,
                        parent_event_id: None,
                        parent_event_ids: None,
                        causal_chain_id: None,
                        metadata: None,
                    },
                )
                .await
                .unwrap();
            let snapshot = astra_services::DatabaseContextService::new(settings.clone())
                .with_pool(observer.clone())
                .create_snapshot(
                    user.clone(),
                    astra_services::SnapshotCreateRequestData {
                        session_id: session.clone(),
                        event_id: event.record.event_id,
                        context_data: json!({"race": outcome}),
                    },
                )
                .await
                .unwrap();
            let request = DecisionCreateRequestData {
                session_id: session.clone(),
                event_id: snapshot.event_id.clone(),
                context_capture_id: snapshot.context_capture_id,
                decision_type: "retained_race".into(),
                decision_output: json!({"ok": true}),
                model_params: None,
            };
            let skill = retained_skill_record(&user, &session, &skill_name);
            if matches!(outcome, "delete_first" | "cancel_admission") {
                let connection_id: u64 = sqlx::query_scalar("SELECT CONNECTION_ID()")
                    .fetch_one(worker.get())
                    .await
                    .unwrap();
                let mut holder = observer.get().begin().await.unwrap();
                admit_session_event_write(&mut holder, &session, &user, false)
                    .await
                    .unwrap();
                let task = AbortOnDropHandle::new(tokio::spawn(write_retained_fixture(
                    worker.clone(),
                    user.clone(),
                    request.clone(),
                    skill.clone(),
                    decision,
                    None,
                )));
                wait_for_retained_fence_query(&observer, connection_id).await;
                // Real writers for A/T and B/S must finish while A/S is fenced.
                for (independent_user, independent_session) in [
                    (user.clone(), Uuid::new_v4().to_string()),
                    (Uuid::new_v4().to_string(), session.clone()),
                ] {
                    crate::server::run::insert_active_run_session_fixture(
                        &observer,
                        &independent_user,
                        &independent_session,
                    )
                    .await;
                    let event = DatabaseEventService::new(settings.clone())
                        .with_pool(observer.clone())
                        .create_event(
                            independent_user.clone(),
                            EventCreateRequestData {
                                ingestion_source: astra_services::EventIngestionSource::Client,
                                event_id: None,
                                session_id: independent_session.clone(),
                                event_type: "retained_independent".into(),
                                content: "{}".into(),
                                agent_id: None,
                                agent_version: None,
                                parent_event_id: None,
                                parent_event_ids: None,
                                causal_chain_id: None,
                                metadata: None,
                            },
                        )
                        .await
                        .unwrap();
                    let independent_request = DecisionCreateRequestData {
                        session_id: independent_session.clone(),
                        event_id: event.record.event_id,
                        context_capture_id: String::new(),
                        ..request.clone()
                    };
                    tokio::time::timeout(
                        Duration::from_secs(10),
                        write_retained_fixture(
                            observer.clone(),
                            independent_user.clone(),
                            independent_request,
                            retained_skill_record(
                                &independent_user,
                                &independent_session,
                                &skill_name,
                            ),
                            decision,
                            None,
                        ),
                    )
                    .await
                    .unwrap()
                    .unwrap();
                    assert_eq!(
                        assert_session_event_count(
                            observer.get(),
                            &independent_user,
                            &independent_session
                        )
                        .await,
                        1
                    );
                    // Deleting an unrelated identity must also make progress.
                    tokio::time::timeout(
                        Duration::from_secs(10),
                        astra_services::DatabaseSessionService::new(settings.clone())
                            .with_pool(observer.clone())
                            .delete_session(independent_session, independent_user),
                    )
                    .await
                    .unwrap()
                    .unwrap();
                }
                if outcome == "cancel_admission" {
                    // Cancel during the observed MySQL exchange, not merely
                    // at the post-INSERT test pause.
                    task.abort();
                    assert!(task.await.unwrap_err().is_cancelled());
                    holder.rollback().await.unwrap();
                } else {
                    sqlx::query("UPDATE agent_session_lifecycle_fences SET delete_requested_at = NOW(6) WHERE user_id = ? AND session_id = ?")
                            .bind(&user).bind(&session).execute(&mut *holder).await.unwrap();
                    holder.commit().await.unwrap();
                    assert!(
                        tokio::time::timeout(Duration::from_secs(10), task)
                            .await
                            .unwrap()
                            .unwrap()
                            .is_err()
                    );
                    assert_eq!(
                        assert_session_event_count(worker.get(), &user, &session).await,
                        1
                    );
                }
            } else {
                let hook = Arc::new(astra_services::decisions::RetainedWriteTestHook {
                    user_id: user.clone(),
                    session_id: session.clone(),
                    inserted: Notify::new(),
                    resume: Notify::new(),
                    fail: outcome == "error",
                });
                let task = AbortOnDropHandle::new(tokio::spawn(write_retained_fixture(
                    worker.clone(),
                    user.clone(),
                    request.clone(),
                    skill.clone(),
                    decision,
                    Some(hook.clone()),
                )));
                tokio::time::timeout(Duration::from_secs(10), hook.inserted.notified())
                    .await
                    .unwrap();
                if outcome == "writer_first" {
                    let connection_id: u64 = sqlx::query_scalar("SELECT CONNECTION_ID()")
                        .fetch_one(deleter.get())
                        .await
                        .unwrap();
                    let service = astra_services::DatabaseSessionService::new(settings.clone())
                        .with_pool(deleter.clone());
                    let sid = session.clone();
                    let uid = user.clone();
                    let deletion = AbortOnDropHandle::new(tokio::spawn(async move {
                        service.delete_session(sid, uid).await
                    }));
                    wait_for_retained_fence_query(&observer, connection_id).await;
                    hook.resume.notify_one();
                    tokio::time::timeout(Duration::from_secs(10), task)
                        .await
                        .unwrap()
                        .unwrap()
                        .unwrap();
                    tokio::time::timeout(Duration::from_secs(10), deletion)
                        .await
                        .unwrap()
                        .unwrap()
                        .unwrap();
                } else {
                    if outcome == "cancel" {
                        task.abort();
                        assert!(task.await.unwrap_err().is_cancelled());
                    } else {
                        hook.resume.notify_one();
                        let error = tokio::time::timeout(Duration::from_secs(10), task)
                            .await
                            .unwrap()
                            .unwrap()
                            .unwrap_err();
                        assert!(
                            error.contains(if decision {
                                "model_params"
                            } else {
                                "injected retained write failure"
                            }),
                            "{error}"
                        );
                    }
                }
            }
            if matches!(outcome, "error" | "cancel" | "cancel_admission") {
                // Both cancellation points and receipt failure must leave
                // the same single-connection pool reusable and no stray rows.
                {
                    let mut tx = worker.get().begin().await.unwrap();
                    admit_session_event_write(&mut tx, &session, &user, false)
                        .await
                        .unwrap();
                    tx.rollback().await.unwrap();
                }
                for table in ["skill_selection_events", "ctx_decision_audits"] {
                    let count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                        "SELECT COUNT(*) FROM {table} WHERE user_id = ? AND session_id = ?"
                    )))
                    .bind(&user)
                    .bind(&session)
                    .fetch_one(worker.get())
                    .await
                    .unwrap();
                    assert_eq!(count, 0, "{decision}/{outcome}/{table}");
                }
                assert_eq!(
                    assert_session_event_count(worker.get(), &user, &session).await,
                    1
                );
                tokio::time::timeout(
                    Duration::from_secs(10),
                    write_retained_fixture(
                        worker.clone(),
                        user.clone(),
                        request.clone(),
                        skill.clone(),
                        decision,
                        None,
                    ),
                )
                .await
                .unwrap()
                .unwrap();
                assert_eq!(
                    assert_session_event_count(worker.get(), &user, &session).await,
                    1
                );
            }
            if outcome != "writer_first" {
                astra_services::DatabaseSessionService::new(settings.clone())
                    .with_pool(observer.clone())
                    .delete_session(session.clone(), user.clone())
                    .await
                    .unwrap();
            }
            assert!(
                write_retained_fixture(
                    worker.clone(),
                    user.clone(),
                    request,
                    skill,
                    decision,
                    None
                )
                .await
                .is_err()
            );
            for table in [
                "agent_sessions",
                "skill_selection_events",
                "ctx_decision_audits",
            ] {
                let count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                    "SELECT COUNT(*) FROM {table} WHERE user_id = ? AND session_id = ?"
                )))
                .bind(&user)
                .bind(&session)
                .fetch_one(observer.get())
                .await
                .unwrap();
                assert_eq!(count, 0, "{decision}/{outcome}/{table}");
            }
        }
        sqlx::query("DELETE FROM skills_registry WHERE skill_id = ?")
            .bind(&skill_id)
            .execute(observer.get())
            .await
            .unwrap();
        worker.close().await;
        deleter.close().await;
        observer.close().await;
    }

    #[tokio::test]
    async fn canonical_failure_does_not_publish_derived_hook_state() {
        let writer = Arc::new(CaptureHookDbWriter::default());
        let mut persist = test_post_loop_persist_context("canonical-failure-session");
        persist.hook_db_writer = Some(writer.clone());
        let mut state = crate::turn::agentic_loop::host::make_test_loop_state();
        state.telemetry.all_selected_skills.push("review".into());

        let error = persist
            .run_after_core(
                &state,
                false,
                Err("canonical transaction unavailable".to_string()),
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
        async fn run(&self, request: TurnObserverRequest) -> Result<TurnObserverOutcome, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.requests.lock().expect("capture lock").push(request);
            self.started.notify_waiters();
            if self.block_until_released {
                self.release.notified().await;
            }
            Ok(TurnObserverOutcome::Observed)
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

    #[tokio::test]
    async fn server_loop_observer_unconfigured_is_not_reported_as_success_or_denial() {
        let registry = Arc::new(astra_turn_core::pipeline_metrics::MetricsRegistry::new());
        let worker = crate::DatabaseTurnObserverWorker::new(None);
        let request =
            build_server_loop_observer_request("user-1", "session-1", &observer_test_state())
                .unwrap();
        run_server_loop_observer_request(&worker, "session-1", request, "async", Some(&registry))
            .await;
        let rendered = registry.render_prometheus();
        assert!(rendered.contains(
            "astra_turn_observer_runs_total{mode=\"async\",outcome=\"not_configured\"} 1"
        ));
        assert!(!rendered.contains("outcome=\"success\""));
        assert!(!rendered.contains("outcome=\"access_disabled\""));
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

        assert_eq!(json_value_payload_bytes(&messages[0]), 27);
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
    fn conversation_history_keeps_original_input_and_final_answer_without_tool_copies() {
        let mut state = crate::turn::agentic_loop::host::make_test_loop_state();
        state.final_text = "implemented the fix".into();
        state.messages = vec![json!({"role":"user","content":"compacted context"})];
        state
            .stall
            .tool_call_records
            .push(astra_services::session_journal::ToolCallRecord {
                tool_call_id: Some("call-1".into()),
                name: "read_file".into(),
                ok: true,
                result_full: Some("large tool output".into()),
                ..Default::default()
            });
        let items = transcript_items_from_server_loop(
            "u",
            "s",
            "r",
            None,
            "original request",
            &state,
            true,
        );
        assert_eq!(
            items
                .iter()
                .map(|item| (item.role, item.content.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("user", "original request"),
                ("assistant", "implemented the fix")
            ]
        );
        assert!(items.iter().all(|item| item.payload.is_none()));
        let pending = transcript_items_from_server_loop(
            "u",
            "s",
            "r",
            None,
            "original request",
            &state,
            false,
        );
        assert_eq!(
            pending.len(),
            1,
            "paused partial output must not freeze the final-answer identity"
        );
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
    fn terminal_model_item_identity_covers_only_accepted_item_and_matching_reasoning() {
        let mut state = crate::turn::agentic_loop::host::make_test_loop_state();
        state.final_text = "same answer".into();
        state.final_text_model_item_id = Some("accepted-A".into());
        state.current_model_item_id = Some("partial-P".into());
        let accepted =
            terminal_assistant_transcript_item("u", "s", "run", None, "", &state).unwrap();
        let payload = accepted.payload.as_ref().unwrap();
        assert_eq!(payload.model_item_id.as_deref(), Some("accepted-A"));
        assert!(payload.reasoning.is_none());
        assert_ne!(accepted.source_event_id, "accepted-A");

        let mut reasoning = TranscriptReasoningProjection::default();
        apply_reasoning_event_payload(
            &mut reasoning,
            &json!({"type":"reasoning_delta", "model_item_id":"partial-P", "content":"unfinished reasoning"}),
        );
        assert!(!reasoning.matches_payload(None));
        assert!(!reasoning.matches_payload(accepted.payload.as_ref()));
        let projected =
            payload_with_reasoning_projection(accepted.payload.clone(), &reasoning).unwrap();
        assert_eq!(projected.model_item_id.as_deref(), Some("accepted-A"));
        assert!(
            projected.reasoning.is_none(),
            "another response cannot claim reasoning coverage"
        );

        apply_reasoning_event_payload(
            &mut reasoning,
            &json!({"type":"reasoning_delta", "model_item_id":"accepted-A", "content":"echo"}),
        );
        assert!(reasoning.matches_payload(accepted.payload.as_ref()));
        apply_reasoning_event_payload(
            &mut reasoning,
            &json!({"type":"reasoning_delta", "model_item_id":"accepted-A", "content":"echo"}),
        );
        let projected =
            payload_with_reasoning_projection(accepted.payload.clone(), &reasoning).unwrap();
        assert_eq!(
            projected.reasoning.as_deref(),
            Some("echoecho"),
            "equal fragments are distinct, not text-deduplicated"
        );

        state.final_text_model_item_id = Some("resume-B".into());
        let resumed =
            terminal_assistant_transcript_item("u", "s", "run", None, "", &state).unwrap();
        assert_eq!(resumed.content, accepted.content);
        let stale = stored_transcript_fixture(&accepted, accepted.payload.clone());
        assert!(
            !PreparedTranscriptItem::new(&resumed)
                .unwrap()
                .matches_stored(&stale)
                .unwrap()
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
    fn raw_identity_allows_reasoning_enrichment_without_rewriting_the_answer() {
        let expected = TranscriptPersistItem {
            run_id: Some("r".into()),
            role: "assistant",
            content: "answer".into(),
            payload: Some(TranscriptPersistPayload {
                model_item_id: Some("item-1".into()),
                ..Default::default()
            }),
            source_event_id: "response".into(),
        };
        let prepared = PreparedTranscriptItem::new(&expected).unwrap();
        let mut payload = expected.payload.clone().unwrap();
        payload.reasoning = Some("independent presentation".into());
        payload.reasoning_status = Some("done".into());
        assert!(
            prepared
                .matches_stored(&stored_transcript_fixture(&expected, Some(payload)))
                .unwrap()
        );
        assert!(
            prepared
                .matches_stored(&stored_transcript_fixture(
                    &expected,
                    expected.payload.clone()
                ))
                .unwrap()
        );
        assert_eq!(
            transcript_payload_identity(
                "assistant",
                Some(r#"{"reasoning":"x","reasoning_status":"done"}"#)
            )
            .unwrap(),
            None
        );
        assert_ne!(
            transcript_payload_identity("user", Some(r#"{"reasoning":"x"}"#)).unwrap(),
            None
        );
    }

    #[test]
    fn raw_identity_rejects_changed_facts_unknown_payload_fields_and_corrupt_hashes() {
        let expected = TranscriptPersistItem {
            run_id: Some("r".into()),
            role: "assistant",
            content: "answer".into(),
            payload: Some(TranscriptPersistPayload {
                model_item_id: Some("item-1".into()),
                ..Default::default()
            }),
            source_event_id: "response".into(),
        };
        let prepared = PreparedTranscriptItem::new(&expected).unwrap();
        for field in [
            "role",
            "content",
            "run",
            "model_item",
            "tool",
            "unknown",
            "hash",
        ] {
            let mut stored = stored_transcript_fixture(&expected, expected.payload.clone());
            match field {
                "role" => stored.role = "user".into(),
                "content" => stored.content.push('!'),
                "run" => stored.run_id = None,
                "model_item" => stored.payload_json = Some(r#"{"model_item_id":"item-2"}"#.into()),
                "tool" => {
                    stored.payload_json =
                        Some(r#"{"model_item_id":"item-1","tool_calls":[{}]}"#.into())
                }
                "unknown" => {
                    stored.payload_json =
                        Some(r#"{"model_item_id":"item-1","future_identity":true}"#.into())
                }
                "hash" => {}
                _ => unreachable!(),
            }
            stored.content_hash = transcript_content_hash(
                &stored.role,
                &stored.content,
                stored.payload_json.as_deref(),
            );
            if field == "hash" {
                stored.content_hash = "0".repeat(64);
            }
            assert!(!prepared.matches_stored(&stored).unwrap(), "{field}");
        }
        assert_eq!(
            transcript_payload_identity("assistant", Some(r#"{"b":2,"a":1}"#)).unwrap(),
            transcript_payload_identity("assistant", Some(r#"{ "a":1, "b":2 }"#)).unwrap()
        );
        assert!(transcript_payload_identity("assistant", Some("[]")).is_err());
        assert!(transcript_payload_identity("assistant", Some("invalid")).is_err());
    }

    async fn cleanup_transcript_fixture_for_owner(
        db: &sqlx::Pool<sqlx::MySql>,
        session_id: &str,
        user_id: &str,
    ) {
        sqlx::query("DELETE FROM session_transcript_items WHERE session_id = ? AND user_id = ?")
            .bind(session_id)
            .bind(user_id)
            .execute(db)
            .await
            .expect("cleanup transcript fixture session_transcript_items");
        sqlx::query(
            "DELETE FROM agent_session_lifecycle_fences WHERE session_id = ? AND user_id = ?",
        )
        .bind(session_id)
        .bind(user_id)
        .execute(db)
        .await
        .expect("cleanup transcript fixture lifecycle fence");
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
        sqlx::query(
            "DELETE FROM agent_session_lifecycle_fences WHERE session_id = ? AND user_id = ?",
        )
        .bind(session_id)
        .bind(user_id)
        .execute(db)
        .await
        .expect("cleanup core persist lifecycle fence");
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
    async fn canonical_transcript_keeps_deferred_inputs_without_diagnostic_copies() {
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
        let engine = crate::server::run::engine::RunEngine::new(store.clone());
        let authority = engine
            .start_run(&run_id, &user_id, &session_id)
            .await
            .expect("start durable run");
        let mut state = crate::turn::agentic_loop::host::make_test_loop_state();
        state.session_turn = 7;
        state.final_text = "assistant final".to_string();
        state.push_recent_round(crate::turn::agentic_loop::host::RecentRoundSummary {
            qualified_usage: None,
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
            terminal_authority: None,
            owner_pod_id: None,

            user_message: "initial one".to_string(),
            hook_db_writer: None,
            observer_worker: None,
            metrics_registry: None,
        };
        let terminal_assistant_source_event_id = persist
            .persist_canonical_transcript_in_transaction(&state, true)
            .await
            .expect("persist atomic core, trace, and transcript prefix");
        persist
            .materialize_run_transcript_evidence(
                &state,
                terminal_assistant_source_event_id.as_deref(),
                None,
            )
            .await
            .expect("materialize terminal assistant transcript evidence");

        assert_eq!(
            assert_session_event_count(&db, &user_id, &session_id).await,
            0
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
                ("assistant".to_string(), "assistant final".to_string()),
            ],
            "transcript preserves user inputs and final answer without duplicate tool detail"
        );

        cleanup_core_persist_fixture_for_owner(&db, &session_id, &user_id).await;
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
    async fn canonical_append_identity_and_authority_rejections_leave_no_effects() {
        let pool = setup_pool().await;
        let db = pool.get();
        let user = Uuid::new_v4().to_string();
        let foreign = Uuid::new_v4().to_string();
        let session = Uuid::new_v4().to_string();
        let other_session = Uuid::new_v4().to_string();
        let foreign_session = Uuid::new_v4().to_string();
        let run = Uuid::new_v4().to_string();
        for (owner, session) in [
            (&user, &session),
            (&user, &other_session),
            (&foreign, &foreign_session),
        ] {
            sqlx::query("INSERT INTO agent_sessions (session_id, user_id, status, event_count) VALUES (?, ?, 'active', 0)")
                .bind(session).bind(owner).execute(db).await.unwrap();
        }
        let engine = crate::server::run::engine::RunEngine::new(Arc::new(
            DatabaseRunStateStore::new(pool.clone()),
        ));
        let authority = engine.start_run(&run, &user, &session).await.unwrap();
        let state = crate::turn::agentic_loop::host::make_test_loop_state();
        let mut persist = test_post_loop_persist_context(&session);
        persist.shared_pool = Some(pool.clone());
        persist.user_id = user.clone();
        persist.run_id = run.clone();
        persist.expected_owner_generation = Some(authority.owner_generation);
        persist.owner_lease_duration = Some(Duration::from_secs(45));
        for case in [
            "session",
            "owner",
            "missing",
            "generation",
            "expired",
            "paused",
            "terminal",
            "unowned_missing",
        ] {
            persist.user_id = if case == "owner" {
                foreign.clone()
            } else {
                user.clone()
            };
            persist.session_id = match case {
                "owner" => foreign_session.clone(),
                "session" => other_session.clone(),
                _ => session.clone(),
            };
            persist.run_id = if case.ends_with("missing") {
                Uuid::new_v4().to_string()
            } else {
                run.clone()
            };
            persist.expected_owner_generation = (case != "unowned_missing")
                .then_some(authority.owner_generation + u64::from(case == "generation"));
            sqlx::query("UPDATE agent_runs SET status = ?, owner_lease_expires_at = DATE_ADD(NOW(6), INTERVAL ? SECOND) WHERE user_id = ? AND run_id = ?")
                .bind(match case { "paused" => "paused", "terminal" => "completed", _ => "running" })
                .bind(if case == "expired" { -60_i64 } else { 60_i64 }).bind(&user).bind(&run).execute(db).await.unwrap();
            let snapshot_sql = "SELECT status, owner_lease_expires_at, total_prompt_tokens, total_completion_tokens, total_tool_calls, last_event_idx FROM agent_runs WHERE user_id = ? AND run_id = ?";
            let before: (String, Option<chrono::NaiveDateTime>, i64, i64, i64, i64) =
                sqlx::query_as(snapshot_sql)
                    .bind(&user)
                    .bind(&run)
                    .fetch_one(db)
                    .await
                    .unwrap();
            let queries = PersistenceQueries::default();
            let subscriber = tracing_subscriber::registry().with(queries.clone());
            persist
                .persist_canonical_transcript_in_transaction(&state, true)
                .with_subscriber(subscriber)
                .await
                .expect_err(case);
            let after: (String, Option<chrono::NaiveDateTime>, i64, i64, i64, i64) =
                sqlx::query_as(snapshot_sql)
                    .bind(&user)
                    .bind(&run)
                    .fetch_one(db)
                    .await
                    .unwrap();
            assert_eq!(before, after, "{case}: rejected append changed run");
            for table in ["agent_events", "session_transcript_items"] {
                let count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                    "SELECT COUNT(*) FROM {table} WHERE user_id IN (?, ?)"
                )))
                .bind(&user)
                .bind(&foreign)
                .fetch_one(db)
                .await
                .unwrap();
                assert_eq!(count, 0, "{case}: unexpected {table}");
            }
            let count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM agent_sessions WHERE user_id IN (?, ?) AND event_count <> 0",
            )
            .bind(&user)
            .bind(&foreign)
            .fetch_one(db)
            .await
            .unwrap();
            assert_eq!(count, 0, "{case}: rejected append changed session counters");
            let statements = queries.0.lock().unwrap();
            assert_eq!(
                statements
                    .iter()
                    .filter(|sql| sql.trim_start().starts_with("SELECT 1 FROM agent_runs"))
                    .count(),
                usize::from(case == "unowned_missing"),
                "{case}: existence admission"
            );
        }
        for (owner, session) in [
            (&user, &session),
            (&user, &other_session),
            (&foreign, &foreign_session),
        ] {
            cleanup_core_persist_fixture_for_owner(db, session, owner).await;
        }
        for table in ["agent_run_events", "agent_runs"] {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "DELETE FROM {table} WHERE user_id = ? AND run_id = ?"
            )))
            .bind(&user)
            .bind(&run)
            .execute(db)
            .await
            .unwrap();
        }
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
        let engine = crate::server::run::engine::RunEngine::new(store.clone());
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
        let error = persist_server_loop_canonical_terminal_settlement(
            &pool,
            CanonicalLoopAppend {
                user_id: &user_id,
                session_id: &session_id,
                run_id: &run_id,
                expected_owner_generation: Some(authority.owner_generation),
                owner_lease_duration: Some(Duration::from_secs(45)),
                trace_context: None,
                user_message: "produce an answer",
                include_terminal_assistant: true,
            },
            &state,
            CanonicalTerminalSettlement {
                expected_owner_pod_id: store.owner_pod_id(),
                expected_statuses: &[astra_core::STATUS_RUNNING],
                expected_owner_generation: authority.owner_generation,
                status: astra_core::STATUS_COMPLETED,
                waiting_for: None,
                error_message: None,
                error_code: None,
                error_kind: None,
                error_metadata: None,
                cancellation_origin: None,
                prompt_tokens: 11,
                completion_tokens: 7,
                tool_calls: 3,
                context_commit: None,
            },
            None,
        )
        .await
        .expect_err("stale generation must fail before writing any settlement evidence");
        assert!(
            error.contains("canonical terminal lost durable execution authority")
                && error.contains(&format!("generation {}", authority.owner_generation)),
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
        let engine = crate::server::run::engine::RunEngine::new(store.clone());
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
        let error = persist_server_loop_canonical_terminal_settlement(
            &pool,
            CanonicalLoopAppend {
                user_id: &user_id,
                session_id: &session_id,
                run_id: &run_id,
                expected_owner_generation: Some(authority.owner_generation),
                owner_lease_duration: Some(Duration::from_secs(45)),
                trace_context: None,
                user_message: "produce an answer",
                include_terminal_assistant: true,
            },
            &state,
            CanonicalTerminalSettlement {
                expected_owner_pod_id: store.owner_pod_id(),
                // The terminal claim must reject this status before publishing
                // any evidence or usage. The collision case below independently
                // exercises rollback after canonical rows have been staged.
                expected_statuses: &[astra_core::STATUS_WAITING],
                expected_owner_generation: authority.owner_generation,
                status: astra_core::STATUS_COMPLETED,
                waiting_for: None,
                error_message: None,
                error_code: None,
                error_kind: None,
                error_metadata: None,
                cancellation_origin: None,
                prompt_tokens: 31,
                completion_tokens: 13,
                tool_calls: 5,
                context_commit: None,
            },
            None,
        )
        .await
        .expect_err("terminal precondition conflict must roll back canonical writes");
        assert!(
            error
                == format!(
                    "canonical terminal lost durable execution authority at generation {}",
                    authority.owner_generation
                ),
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
        assert_eq!(
            assert_session_event_count(&db, &user_id, &session_id).await,
            0
        );
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
        assert_canonical_terminal(TerminalCase::Completed).await;
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
    async fn canonical_terminal_rejects_colliding_evidence_before_settlement() {
        assert_canonical_terminal(TerminalCase::RawCollision).await;
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
    async fn canonical_terminal_failure_keeps_classification_through_commit_and_replay() {
        assert_canonical_terminal(TerminalCase::Failed).await;
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
    async fn canonical_incomplete_terminal_preserves_public_partial_on_commit_and_replay() {
        assert_canonical_terminal(TerminalCase::Incomplete).await;
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
    async fn canonical_user_cancellation_converges_with_raw_in_same_transaction() {
        assert_canonical_terminal(TerminalCase::UserCancellation).await;
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
    async fn canonical_terminal_returns_late_guidance_without_claiming_model_application() {
        assert_canonical_terminal(TerminalCase::LateGuidance).await;
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
    async fn canonical_terminal_cannot_promote_an_accepted_pause() {
        assert_canonical_terminal(TerminalCase::Paused).await;
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum TerminalCase {
        Completed,
        Failed,
        Incomplete,
        LateGuidance,
        RawCollision,
        UserCancellation,
        Paused,
    }

    async fn assert_canonical_terminal(case: TerminalCase) {
        use super::super::{
            AgenticRunLifecycleService, RootTurnStartParticipant, durable_event_type,
        };
        use astra_services::{SessionContextCoordinator, runs::RunLifecycleService};
        let pool = setup_pool().await;
        let (user, session, run_id) = (
            Uuid::new_v4().to_string(),
            Uuid::new_v4().to_string(),
            Uuid::new_v4().to_string(),
        );
        crate::server::run::insert_active_run_session_fixture(&pool, &user, &session).await;
        let owner = "canonical-terminal-owner";
        let service = super::super::tests::db_backed_test_service(&pool, owner);
        let request = super::super::tests::prepared_test_request("accepted question");
        let mut participant = RootTurnStartParticipant {
            service: &service,
            user_id: &user,
            session_id: &session,
            run_id: &run_id,
            request: &request,
            admission: None,
            prepared: None,
            rejection: None,
        };
        assert!(matches!(
            service
                .run_engine
                .admit_run_with_context(
                    &run_id,
                    &user,
                    &session,
                    Some(&session),
                    crate::server::run::engine::RunStartContext {
                        original_input: Some(request.message.clone()),
                        ..Default::default()
                    },
                    true,
                    &mut participant
                )
                .await
                .unwrap(),
            astra_services::runs::DurableRunStartClaim::Started { .. }
        ));
        let admitted = participant.admission.take().unwrap();
        let generation = admitted.authority.generation;
        let store = DatabaseRunStateStore::new(pool.clone()).with_owner_pod_id(owner);
        let mut state = crate::turn::agentic_loop::host::make_test_loop_state();
        state.final_text = "committed answer 中文🙂".into();
        state.final_text_model_item_id = Some("answer-model-item".into());
        state.messages = vec![
            json!({"role":"user","content":request.message}),
            json!({"role":"assistant","content":state.final_text}),
        ];
        state.total_prompt = 101;
        state.total_cache_read = 202;
        state.total_cache_creation = 303;
        state.total_completion = 404;
        state.qualified_usage = Some(
            astra_turn_types::CanonicalTokenUsage::new(Some(101), Some(202), Some(303), Some(404))
                .unwrap(),
        );
        let incomplete = matches!(
            case,
            TerminalCase::Incomplete | TerminalCase::UserCancellation
        );
        if incomplete {
            use astra_turn_core::interruption::{
                InterruptionKind, InterruptionRecord, InterruptionStateSummary, ResumeAction,
            };
            state.interruption = Some(InterruptionRecord::new(
                InterruptionKind::ExecutionIncomplete,
                ResumeAction::ContinueImmediately,
                InterruptionStateSummary {
                    tool_calls_completed: 3,
                    turns_completed: 2,
                    remaining_turns: 5,
                    error_detail: Some("private-terminal-diagnostic".into()),
                    stall_signal: Some("private-stall-signal".into()),
                    ..Default::default()
                },
            ));
        }
        let context = AgenticRunLifecycleService::prepare_canonical_turn_commit(
            Some(&admitted),
            &state.messages,
            None,
            false,
            &run_id,
        )
        .unwrap()
        .unwrap();
        let trace = server_trace_context(&user, &session, &run_id, state.session_turn);
        if case == TerminalCase::LateGuidance {
            store.append_event(&user, &session, &run_id, json!({"event_type":"user_intent", "idempotency_key":"user_intent:late",
                "data":{"intent_id":"late", "delivery":"guide_current_run", "input":{"content":"late guidance"}}})).await.unwrap();
        }
        if case == TerminalCase::UserCancellation {
            assert!(
                store
                    .request_run_cancellation(&user, &run_id)
                    .await
                    .unwrap()
            );
        }
        if case == TerminalCase::RawCollision {
            persist_root_turn_input(
                &pool,
                &user,
                &session,
                TranscriptPersistItem {
                    run_id: Some(run_id.clone()),
                    role: "user",
                    content: "conflicting input".into(),
                    payload: None,
                    source_event_id: trace.root_event_id,
                },
            )
            .await
            .unwrap();
        }
        if case == TerminalCase::Paused {
            service
                .pause_run(run_id.clone(), user.clone())
                .await
                .unwrap();
        }
        let proposed_failure = matches!(
            case,
            TerminalCase::Failed | TerminalCase::Incomplete | TerminalCase::UserCancellation
        );
        let public_error = state
            .interruption
            .as_ref()
            .map(|record| record.user_message.clone());
        let settlement = CanonicalTerminalSettlement {
            expected_owner_pod_id: owner,
            expected_statuses: &[STATUS_RUNNING, STATUS_WAITING],
            expected_owner_generation: generation,
            status: if proposed_failure {
                astra_core::STATUS_FAILED
            } else {
                astra_core::STATUS_COMPLETED
            },
            waiting_for: None,
            error_message: if incomplete {
                public_error.as_deref()
            } else {
                proposed_failure.then_some("provider disconnected")
            },
            error_code: if incomplete {
                Some("execution_incomplete")
            } else {
                proposed_failure.then_some("provider_connection_lost")
            },
            error_kind: (proposed_failure && !incomplete).then_some(astra_core::ErrorKind::Network),
            error_metadata: None,
            cancellation_origin: None,
            prompt_tokens: 606,
            completion_tokens: 404,
            tool_calls: 0,
            context_commit: Some(&context),
        };
        let append = || CanonicalLoopAppend {
            user_id: &user,
            session_id: &session,
            run_id: &run_id,
            expected_owner_generation: Some(generation),
            owner_lease_duration: Some(Duration::from_secs(45)),
            trace_context: None,
            user_message: &request.message,
            include_terminal_assistant: true,
        };
        let result = persist_server_loop_canonical_terminal_settlement(
            &pool,
            append(),
            &state,
            settlement,
            None,
        )
        .await;
        let current = store.load_run(&user, &run_id).await.unwrap().unwrap();
        let slot: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_runs WHERE user_id = ? AND session_id = ? AND (status IN ('running', 'waiting') OR (status = 'paused' AND waiting_for IS NOT NULL) OR (owner_pod_id IS NOT NULL AND owner_lease_expires_at >= NOW(6)))")
            .bind(&user).bind(&session).fetch_one(pool.get()).await.unwrap();
        let coordinator = DatabaseSessionContextCoordinator::new(pool.clone());
        let head = coordinator
            .load_head(&admitted.authority.key)
            .await
            .unwrap();
        if matches!(case, TerminalCase::RawCollision | TerminalCase::Paused) {
            assert!(result.is_err());
            assert_eq!(
                current.status,
                if case == TerminalCase::Paused {
                    STATUS_PAUSED
                } else {
                    STATUS_RUNNING
                }
            );
            assert!(current.terminal_receipt_json.is_none());
            assert!(current.owner_pod_id.is_some());
            assert_eq!(slot, 1);
            assert!(
                head.is_none(),
                "raw failure must roll Context and terminal back together"
            );
            let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM conversation_manifest_nodes WHERE owner_user_id = ? AND session_id = ?")
                .bind(&user).bind(&session).fetch_one(pool.get()).await.unwrap();
            assert_eq!(
                count, 0,
                "failed transaction must not leak immutable Context nodes"
            );
        } else {
            let commit = result.unwrap();
            let expected_status = if case == TerminalCase::UserCancellation {
                STATUS_CANCELLED
            } else if matches!(case, TerminalCase::Failed | TerminalCase::Incomplete) {
                astra_core::STATUS_FAILED
            } else {
                astra_core::STATUS_COMPLETED
            };
            assert_eq!(commit.terminal_status, Some(expected_status));
            assert_eq!(current.status, expected_status);
            assert!(current.owner_pod_id.is_none());
            assert_eq!(slot, 0);
            let expected_cursor =
                (case != TerminalCase::UserCancellation).then(|| context.cursor());
            assert_eq!(
                head.as_ref().map(|head| head.cursor.clone()),
                expected_cursor
            );
            let raw = sqlx::query("SELECT role,content,canonical_completed_turn,canonical_root_hash FROM session_transcript_items WHERE user_id=? AND session_id=? ORDER BY item_seq")
                .bind(&user).bind(&session).fetch_all(pool.get()).await.unwrap();
            assert_eq!(raw.len(), 2);
            assert_eq!(
                raw[0].try_get::<String, _>("content").unwrap(),
                request.message
            );
            assert_eq!(
                raw[1].try_get::<String, _>("content").unwrap(),
                state.final_text
            );
            assert_eq!(
                raw[1]
                    .try_get::<Option<String>, _>("canonical_root_hash")
                    .unwrap(),
                expected_cursor
                    .as_ref()
                    .map(|cursor| cursor.canonical_root_hash.clone())
            );
            let physical: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_run_events WHERE user_id=? AND run_id=? AND event_type IN ('text_done','run_finished','run_settlement_started','run_settlement_finished','run_accounting_finalized')")
                .bind(&user).bind(&run_id).fetch_one(pool.get()).await.unwrap();
            assert_eq!(
                physical, 0,
                "completion has one C0/C1 owner, without mirrored event payloads or markers"
            );
            let completion = current
                .events
                .iter()
                .filter(|event| durable_event_type(event) == Some("run_finished"))
                .collect::<Vec<_>>();
            assert_eq!(completion.len(), 1);
            assert_eq!(completion[0]["data"]["status"], expected_status);
            let expected_kind =
                (case == TerminalCase::Failed).then_some(astra_core::ErrorKind::Network);
            assert_eq!(completion[0]["data"]["error_kind"], json!(expected_kind));
            let stored = astra_services::runs::CommittedRunTerminalReceipt::decode(
                current.terminal_receipt_json.as_deref().unwrap(),
                current.last_event_idx,
            )
            .unwrap();
            assert_eq!(stored.receipt.error_kind, expected_kind);
            if case == TerminalCase::UserCancellation {
                assert!(stored.receipt.error_code.is_none());
                assert!(stored.receipt.error_metadata.is_none());
                assert!(stored.receipt.interruption.is_none());
            }

            let text = current
                .events
                .iter()
                .find(|event| durable_event_type(event) == Some("text_done"))
                .unwrap();
            assert_eq!(text["data"]["full_text"], state.final_text);
            assert_eq!(text["data"]["model_item_id"], "answer-model-item");
            let delta = store
                .load_run_event_delta(&user, &run_id, text["index"].as_i64().unwrap())
                .await
                .unwrap()
                .unwrap();
            assert!(
                delta
                    .events
                    .iter()
                    .all(|event| durable_event_type(event) != Some("text_done")),
                "reconnect after the answer must not replay it"
            );
            assert!(
                delta
                    .events
                    .iter()
                    .any(|event| durable_event_type(event) == Some("run_finished"))
            );
            if case == TerminalCase::Incomplete {
                let expected = state.interruption.as_ref().unwrap().public_projection();
                assert_eq!(stored.receipt.interruption.as_ref(), Some(&expected));
                // Exercise committed live events and a cold reconnect through
                // the same public formatter and SSE terminal consumer.
                for events in [commit.terminal_events.clone(), current.events.clone()] {
                    let projected =
                        crate::server::run::handlers::transform_stream_run_events_for_client(
                            &run_id, events,
                        );
                    let wire = serde_json::to_string(&projected).unwrap();
                    assert!(!wire.contains("private-terminal-diagnostic"));
                    assert!(!wire.contains("private-stall-signal"));
                    let terminal = projected
                        .iter()
                        .find(|event| event["type"] == "run_finished")
                        .unwrap();
                    let terminal =
                        astra_turn_core::chat_turn_sse_dispatch::durable_run_terminal_from_event(
                            terminal,
                        )
                        .unwrap()
                        .unwrap();
                    assert_eq!(terminal.interruption, Some(expected.clone()));
                    assert!(terminal.error_kind.is_none());
                    let text = projected
                        .iter()
                        .find(|event| event["type"] == "text_done")
                        .unwrap();
                    assert_eq!(text["full_text"], state.final_text);
                    assert_eq!(text["partial"], true);
                }
                assert_eq!(
                    delta
                        .events
                        .iter()
                        .find(|event| durable_event_type(event) == Some("run_finished"))
                        .unwrap()["data"]["interruption"],
                    expected.to_json()
                );
            }
            let replay = persist_server_loop_canonical_terminal_settlement(
                &pool,
                append(),
                &state,
                settlement,
                None,
            )
            .await
            .unwrap();
            assert_eq!(replay.terminal_status, commit.terminal_status);
            assert_eq!(
                replay.terminal_events,
                commit
                    .terminal_events
                    .iter()
                    .filter(|event| matches!(
                        durable_event_type(event),
                        Some("text_done" | "run_error" | "run_finished")
                    ))
                    .cloned()
                    .collect::<Vec<_>>(),
                "idempotent confirmation repeats completion without new control notifications"
            );
            if case == TerminalCase::Incomplete {
                state.interruption.as_mut().unwrap().remaining_turns += 1;
                // Use the same public error; changing resume facts must still
                // fail exact-commit resolution.
                let retry = CanonicalTerminalSettlement {
                    error_message: current.error_message.as_deref(),
                    ..settlement
                };
                assert!(
                    persist_server_loop_canonical_terminal_settlement(
                        &pool,
                        append(),
                        &state,
                        retry,
                        None
                    )
                    .await
                    .is_err()
                );
            }
            if case == TerminalCase::Failed {
                for kind in [None, Some(astra_core::ErrorKind::Unknown)] {
                    assert!(
                        persist_server_loop_canonical_terminal_settlement(
                            &pool,
                            append(),
                            &state,
                            CanonicalTerminalSettlement {
                                error_kind: kind,
                                ..settlement
                            },
                            None,
                        )
                        .await
                        .is_err(),
                        "changed classification cannot reconcile an exact commit"
                    );
                }
            }

            let replayed = store.load_run(&user, &run_id).await.unwrap().unwrap();
            assert_eq!(replayed.last_event_idx, current.last_event_idx);
            assert_eq!(replayed.events, current.events);
            if case == TerminalCase::LateGuidance {
                let returned = |events: &[Value]| {
                    events
                        .iter()
                        .filter(|event| durable_event_type(event) == Some("user_intent_returned"))
                        .cloned()
                        .collect::<Vec<_>>()
                };
                let fresh = returned(&commit.terminal_events);
                assert_eq!(fresh.len(), 1);
                assert_eq!(fresh[0]["data"]["intent_id"], "late");
                assert_eq!(fresh[0]["data"]["status"], "returned");
                assert_eq!(fresh[0]["data"]["content"], "late guidance");
                let durable = returned(&replayed.events);
                assert_eq!(durable.len(), 1);
                assert_eq!(durable[0]["data"], fresh[0]["data"]);
                let returned_index = durable[0]["index"].as_i64().unwrap();
                assert!(returned_index < text["index"].as_i64().unwrap());
                let before_return = store
                    .load_run_event_delta(&user, &run_id, returned_index - 1)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(returned(&before_return.events), durable);
                let after_return = store
                    .load_run_event_delta(&user, &run_id, returned_index)
                    .await
                    .unwrap()
                    .unwrap();
                assert!(returned(&after_return.events).is_empty());
                assert!(
                    after_return
                        .events
                        .iter()
                        .any(|event| durable_event_type(event) == Some("run_finished"))
                );
                assert!(
                    !current
                        .events
                        .iter()
                        .any(|event| durable_event_type(event) == Some("user_intent_applied"))
                );
            }
            if case == TerminalCase::Completed {
                use astra_services::AcquireWriterAndReserveTurnOutcome;
                use astra_turn_types::{
                    ActorContextV1, ActorKindV1, CANONICAL_TURN_DELTA_SCHEMA_VERSION,
                    CanonicalDeltaModeV1, CanonicalTurnDeltaV1, SessionSurfaceV1,
                };
                let coordinator = DatabaseSessionContextCoordinator::new(pool.clone());
                let authority = context.run_context_authority().unwrap();
                let key = &authority.key;
                let old_cursor = context.cursor();
                let old_manifest = head.as_ref().unwrap().latest_manifest_root.clone();
                let actor = ActorContextV1::owner_user(
                    &user,
                    "later-controller",
                    ActorKindV1::Server,
                    SessionSurfaceV1::Server,
                    None,
                    authority.authority_epochs,
                );
                let AcquireWriterAndReserveTurnOutcome::Ready { reservation, .. } = coordinator
                    .acquire_writer_and_reserve_turn(
                        key,
                        Some(&old_cursor),
                        &actor,
                        Duration::from_secs(30),
                        "later-writer",
                        "later-turn",
                        None,
                    )
                    .await
                    .unwrap()
                else {
                    panic!("later independent controller must own its turn")
                };
                coordinator
                    .commit_turn_and_release_writer(
                        &reservation,
                        CanonicalTurnDeltaV1 {
                            schema_version: CANONICAL_TURN_DELTA_SCHEMA_VERSION,
                            completed_turn: old_cursor.completed_turn + 1,
                            journal_event_seq: old_cursor.journal_event_seq + 1,
                            conversation_seq: old_cursor.conversation_seq + 1,
                            compaction_generation: old_cursor.compaction_generation,
                            config_version_id: None,
                            mode: CanonicalDeltaModeV1::Append,
                            logical_segments: vec![vec![
                                json!({"role":"assistant","content":"later turn"}),
                            ]],
                        },
                        "later-commit",
                    )
                    .await
                    .unwrap();
                assert_ne!(
                    coordinator.load_head(key).await.unwrap().unwrap().cursor,
                    old_cursor
                );
                assert!(
                    persist_server_loop_canonical_terminal_settlement(
                        &pool,
                        append(),
                        &state,
                        settlement,
                        None,
                    )
                    .await
                    .is_ok(),
                    "a genuine later turn cannot invalidate the earlier exact commit"
                );

                let row = sqlx::query("SELECT manifest_json,content_json,total_message_count,compaction_generation,conversation_seq FROM conversation_manifest_nodes WHERE owner_user_id=? AND session_id=? AND manifest_root=?")
                    .bind(&user).bind(&session).bind(&old_manifest).fetch_one(pool.get()).await.unwrap();
                let manifest: String = row.try_get("manifest_json").unwrap();
                let content: String = row.try_get("content_json").unwrap();
                let count: i64 = row.try_get("total_message_count").unwrap();
                let generation: i64 = row.try_get("compaction_generation").unwrap();
                let sequence: i64 = row.try_get("conversation_seq").unwrap();
                let mut reordered: Vec<Vec<Value>> = serde_json::from_str(&content).unwrap();
                assert!(reordered[0].len() >= 2);
                reordered[0].reverse();
                // Exact resolution must verify both the immutable body and
                // its ordering, as well as the original indexed coordinates.
                for (column, bad, saved) in [
                    (
                        "compaction_generation",
                        (generation + 1).to_string(),
                        generation.to_string(),
                    ),
                    (
                        "conversation_seq",
                        (sequence + 1000).to_string(),
                        sequence.to_string(),
                    ),
                    ("manifest_json", "{}".into(), manifest),
                    ("content_json", "[]".into(), content.clone()),
                    (
                        "content_json",
                        serde_json::to_string(&reordered).unwrap(),
                        content,
                    ),
                    (
                        "total_message_count",
                        (count + 1).to_string(),
                        count.to_string(),
                    ),
                ] {
                    let sql = format!(
                        "UPDATE conversation_manifest_nodes SET {column}=? WHERE owner_user_id=? AND session_id=? AND manifest_root=?"
                    );
                    assert_eq!(
                        sqlx::query(sqlx::AssertSqlSafe(sql.clone()))
                            .bind(bad)
                            .bind(&user)
                            .bind(&session)
                            .bind(&old_manifest)
                            .execute(pool.get())
                            .await
                            .unwrap()
                            .rows_affected(),
                        1
                    );
                    assert!(
                        persist_server_loop_canonical_terminal_settlement(
                            &pool,
                            append(),
                            &state,
                            settlement,
                            None,
                        )
                        .await
                        .is_err(),
                        "{column} corruption cannot resolve as an exact commit"
                    );
                    sqlx::query(sqlx::AssertSqlSafe(sql))
                        .bind(saved)
                        .bind(&user)
                        .bind(&session)
                        .bind(&old_manifest)
                        .execute(pool.get())
                        .await
                        .unwrap();
                }
                assert!(
                    persist_server_loop_canonical_terminal_settlement(
                        &pool,
                        append(),
                        &state,
                        settlement,
                        None,
                    )
                    .await
                    .is_ok(),
                    "only the unchanged, fully stored immutable commit may resolve"
                );
            }
            state.final_text = "changed answer".into();
            assert!(
                persist_server_loop_canonical_terminal_settlement(
                    &pool,
                    append(),
                    &state,
                    settlement,
                    None
                )
                .await
                .is_err(),
                "changed raw identity cannot resolve as an exact commit"
            );
        }
        for table in [
            "conversation_manifest_nodes",
            "session_context_heads",
            "session_context_operation_receipts",
            "session_context_authority_events",
        ] {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "DELETE FROM {table} WHERE owner_user_id = ? AND session_id = ?"
            )))
            .bind(&user)
            .bind(&session)
            .execute(pool.get())
            .await
            .unwrap();
        }
        for table in ["agent_run_events", "agent_runs"] {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "DELETE FROM {table} WHERE user_id = ? AND session_id = ?"
            )))
            .bind(&user)
            .bind(&session)
            .execute(pool.get())
            .await
            .unwrap();
        }
        cleanup_core_persist_fixture_for_owner(pool.get(), &session, &user).await;
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
    async fn transcript_persistence_batches_items_and_enforces_admission() {
        let pool = setup_pool().await;
        let db = pool.get().clone();
        let session_id = Uuid::new_v4().to_string();
        let owner_user_id = Uuid::new_v4().to_string();
        let other_user_id = Uuid::new_v4().to_string();
        let run_id = Uuid::new_v4().to_string();

        cleanup_transcript_fixture_for_owner(&db, &session_id, &owner_user_id).await;
        cleanup_transcript_fixture_for_owner(&db, &session_id, &other_user_id).await;
        persist_session_transcript_items(&pool, &owner_user_id, &session_id, &[])
            .await
            .expect("empty standalone append needs no root");
        persist_session_transcript_items(
            &pool,
            &owner_user_id,
            &session_id,
            &[TranscriptPersistItem {
                run_id: None,
                role: "user",
                content: "missing root".into(),
                payload: None,
                source_event_id: "missing-root".into(),
            }],
        )
        .await
        .expect_err("standalone append cannot lazily create a root");

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

        let _wrong_owner = persist_session_transcript_items(
            &pool,
            &other_user_id,
            &session_id,
            &[TranscriptPersistItem {
                run_id: Some(run_id.clone()),
                role: "assistant",
                content: "wrong owner".to_string(),
                payload: None,
                source_event_id: Uuid::new_v4().to_string(),
            }],
        )
        .await
        .expect_err("wrong owner must not persist transcript rows");
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

        // Probe equality using the actual column, so both binary and
        // case-insensitive databases exercise their own identity contract.
        let make_item = |id: &str, content: &str| TranscriptPersistItem {
            run_id: None,
            role: "user",
            content: content.to_string(),
            payload: None,
            source_event_id: id.to_string(),
        };
        persist_session_transcript_items(
            &pool,
            &owner_user_id,
            &session_id,
            &[make_item("CaseProbe", "probe")],
        )
        .await
        .expect("seed collation probe");
        let case_equivalent: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM session_transcript_items
             WHERE user_id = ? AND session_id = ? AND source_event_id = ?",
        )
        .bind(&owner_user_id)
        .bind(&session_id)
        .bind("caseprobe")
        .fetch_one(&db)
        .await
        .expect("read column equality");

        let mut mixed = vec![
            TranscriptPersistItem {
                run_id: items[0].run_id.clone(),
                role: items[0].role,
                content: items[0].content.clone(),
                payload: None,
                source_event_id: items[0].source_event_id.clone(),
            },
            make_item("within-chunk", "first within"),
            make_item("WITHIN-CHUNK", "first within"),
            make_item("within-chunk", "first within"),
            make_item("across-chunks", "first across"),
            make_item("equal-text-one", "equal text"),
            make_item("equal-text-two", "equal text"),
        ];
        let filler_count = TRANSCRIPT_MEMBERSHIP_ROWS * 2 + 3;
        for index in 0..filler_count {
            mixed.push(make_item(&format!("filler-{index}"), &"x".repeat(6000)));
        }
        mixed.push(make_item("ACROSS-CHUNKS", "first across"));
        mixed.push(make_item("across-chunks", "first across"));
        mixed.push(make_item("last-new", "last"));
        persist_session_transcript_items(&pool, &owner_user_id, &session_id, &mixed)
            .await
            .expect("mixed replay and new identities across bounded chunks");
        // An exact replay must keep all existing sequences and first content.
        persist_session_transcript_items(&pool, &owner_user_id, &session_id, &mixed)
            .await
            .expect("all replay");
        let actual = sqlx::query_as::<_, (i64, String, String)>(
            "SELECT item_seq, source_event_id, content FROM session_transcript_items
             WHERE user_id = ? AND session_id = ? ORDER BY item_seq",
        )
        .bind(&owner_user_id)
        .bind(&session_id)
        .fetch_all(&db)
        .await
        .expect("read ordered batch");
        let mut expected = items
            .iter()
            .map(|item| (item.source_event_id.clone(), item.content.clone()))
            .collect::<Vec<_>>();
        expected.push(("CaseProbe".into(), "probe".into()));
        expected.push(("within-chunk".into(), "first within".into()));
        if case_equivalent == 0 {
            expected.push(("WITHIN-CHUNK".into(), "first within".into()));
        }
        expected.extend([
            ("across-chunks".into(), "first across".into()),
            ("equal-text-one".into(), "equal text".into()),
            ("equal-text-two".into(), "equal text".into()),
        ]);
        for index in 0..filler_count {
            expected.push((format!("filler-{index}"), "x".repeat(6000)));
        }
        if case_equivalent == 0 {
            expected.push(("ACROSS-CHUNKS".into(), "first across".into()));
        }
        expected.push(("last-new".into(), "last".into()));
        assert_eq!(actual.len(), expected.len());
        for (index, ((sequence, id, content), expected)) in actual.iter().zip(&expected).enumerate()
        {
            assert_eq!(*sequence, index as i64 + 1);
            assert_eq!((id, content), (&expected.0, &expected.1));
        }

        // Late conflicts must roll back new siblings from earlier chunks, too.
        for (index, conflict) in [
            make_item("within-chunk", "changed content"),
            TranscriptPersistItem {
                run_id: Some("different-run".into()),
                ..make_item("within-chunk", "first within")
            },
            TranscriptPersistItem {
                role: "assistant",
                ..make_item("within-chunk", "first within")
            },
            TranscriptPersistItem {
                payload: Some(TranscriptPersistPayload {
                    model_item_id: Some("different-item".into()),
                    ..Default::default()
                }),
                ..make_item("within-chunk", "first within")
            },
        ]
        .into_iter()
        .enumerate()
        {
            let mut batch = (0..TRANSCRIPT_MEMBERSHIP_ROWS + 1)
                .map(|ordinal| make_item(&format!("rollback-{index}-{ordinal}"), "must roll back"))
                .collect::<Vec<_>>();
            batch.push(conflict);
            persist_session_transcript_items(&pool, &owner_user_id, &session_id, &batch)
                .await
                .unwrap_err();
            let after: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM session_transcript_items WHERE user_id=? AND session_id=?",
            )
            .bind(&owner_user_id)
            .bind(&session_id)
            .fetch_one(&db)
            .await
            .unwrap();
            assert_eq!(
                after,
                expected.len() as i64,
                "late identity conflict rolls back every chunk"
            );
        }
        let conflicting_duplicates = [
            make_item("new-duplicate", "one"),
            make_item("new-duplicate", "two"),
        ];
        persist_session_transcript_items(
            &pool,
            &owner_user_id,
            &session_id,
            &conflicting_duplicates,
        )
        .await
        .unwrap_err();
        if case_equivalent != 0 {
            let aliases = [
                make_item("unseen-case", "one"),
                make_item("UNSEEN-CASE", "two"),
            ];
            persist_session_transcript_items(&pool, &owner_user_id, &session_id, &aliases)
                .await
                .unwrap_err();
        }
        let mut admitted = db.begin().await.expect("hold session admission");
        admit_session_event_write(&mut admitted, &session_id, &owner_user_id, false)
            .await
            .expect("admit first concurrent writer");
        append_session_transcript_items_admitted_in_tx(
            &mut admitted,
            &owner_user_id,
            &session_id,
            &[make_item("concurrent-shared", "first writer")],
        )
        .await
        .expect("first writer's uncommitted item");
        let overlapping = [
            make_item("concurrent-shared", "second writer conflict"),
            make_item("concurrent-new", "second writer new"),
        ];
        let mut waiting = Box::pin(persist_session_transcript_items(
            &pool,
            &owner_user_id,
            &session_id,
            &overlapping,
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut waiting)
                .await
                .is_err(),
            "same-session admission must wait"
        );
        let independent_session = Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count)
             VALUES (?, ?, 'independent transcript', 'active', 0)",
        )
        .bind(&independent_session)
        .bind(&other_user_id)
        .execute(&db)
        .await
        .unwrap();
        tokio::time::timeout(
            Duration::from_secs(5),
            persist_session_transcript_items(
                &pool,
                &other_user_id,
                &independent_session,
                &[make_item("concurrent-shared", "independent owner/session")],
            ),
        )
        .await
        .expect("unrelated session is not serialized")
        .expect("independent writer succeeds");
        admitted.commit().await.expect("release first writer");
        tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .expect("same-session writer resumes")
            .expect_err("conflicting overlap must roll back its new sibling");
        let concurrent = sqlx::query_as::<_, (i64, String, String)>(
            "SELECT item_seq, source_event_id, content FROM session_transcript_items
             WHERE user_id = ? AND session_id = ? AND item_seq > ? ORDER BY item_seq",
        )
        .bind(&owner_user_id)
        .bind(&session_id)
        .bind(expected.len() as i64)
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(
            concurrent,
            vec![(
                expected.len() as i64 + 1,
                "concurrent-shared".into(),
                "first writer".into()
            ),]
        );
        cleanup_transcript_fixture_for_owner(&db, &independent_session, &other_user_id).await;

        // The old unconditional MAX + 1 overflows here even for empty/replay
        // appends. Neither operation needs allocation or an ownership lookup.
        sqlx::query(
            "UPDATE session_transcript_items SET item_seq = ?
             WHERE user_id = ? AND session_id = ? AND source_event_id = 'last-new'",
        )
        .bind(i64::MAX)
        .bind(&owner_user_id)
        .bind(&session_id)
        .execute(&db)
        .await
        .expect("seed allocation boundary");
        persist_session_transcript_items(&pool, &owner_user_id, &session_id, &mixed)
            .await
            .expect("all replay must not allocate");
        let mut empty_tx = db.begin().await.expect("empty transaction");
        append_session_transcript_items_admitted_in_tx(
            &mut empty_tx,
            &owner_user_id,
            &session_id,
            &[],
        )
        .await
        .expect("empty append has no SQL");
        empty_tx.rollback().await.expect("finish empty transaction");

        // Root status and both durable deletion fence states reject even a
        // replay. Standalone writes must use the canonical admission boundary.
        sqlx::query(
            "UPDATE agent_sessions SET status = 'deleting' WHERE user_id = ? AND session_id = ?",
        )
        .bind(&owner_user_id)
        .bind(&session_id)
        .execute(&db)
        .await
        .unwrap();
        persist_session_transcript_items(&pool, &owner_user_id, &session_id, &items)
            .await
            .expect_err("deleting root rejects transcript append");
        sqlx::query(
            "UPDATE agent_sessions SET status = 'active' WHERE user_id = ? AND session_id = ?",
        )
        .bind(&owner_user_id)
        .bind(&session_id)
        .execute(&db)
        .await
        .unwrap();
        for completed in [false, true] {
            let sql = if completed {
                "UPDATE agent_session_lifecycle_fences SET database_deleted_at = NOW(6)
                 WHERE user_id = ? AND session_id = ?"
            } else {
                "UPDATE agent_session_lifecycle_fences SET delete_requested_at = NOW(6)
                 WHERE user_id = ? AND session_id = ?"
            };
            let mut deleting = db.begin().await.expect("begin deletion fence");
            sqlx::query(sql)
                .bind(&owner_user_id)
                .bind(&session_id)
                .execute(&mut *deleting)
                .await
                .expect("set deletion fence");
            let mut racing = Box::pin(persist_session_transcript_items(
                &pool,
                &owner_user_id,
                &session_id,
                &items,
            ));
            assert!(
                tokio::time::timeout(Duration::from_millis(100), &mut racing)
                    .await
                    .is_err(),
                "append waits for pending deletion transaction"
            );
            deleting.commit().await.expect("publish deletion fence");
            tokio::time::timeout(Duration::from_secs(5), racing)
                .await
                .expect("append observes committed deletion")
                .expect_err("deletion fence rejects transcript append");
        }

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
        let engine = crate::server::run::engine::RunEngine::new(store.clone());
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

        let early_items = transcript_items_from_server_loop(
            &user_id,
            &session_id,
            &run_id,
            None,
            "inspect identity",
            &state,
            false,
        );
        persist_session_transcript_items(&pool, &user_id, &session_id, &early_items[..1])
            .await
            .expect("early user is visible before terminal capture");
        let committed = persist_server_loop_canonical_append(
            &pool,
            CanonicalLoopAppend {
                user_id: &user_id,
                session_id: &session_id,
                run_id: &run_id,
                expected_owner_generation: Some(authority.owner_generation),
                owner_lease_duration: Some(Duration::from_secs(45)),
                trace_context: None,
                user_message: "inspect identity",
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

        assert_eq!(
            assert_session_event_count(&db, &user_id, &session_id).await,
            0
        );
        let early_user_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM session_transcript_items
             WHERE user_id = ? AND session_id = ? AND run_id = ? AND role = 'user'",
        )
        .bind(&user_id)
        .bind(&session_id)
        .bind(&run_id)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(
            early_user_count, 1,
            "terminal replay preserves early user identity"
        );

        store
            .append_event(
                &user_id,
                &session_id,
                &run_id,
                json!({
                    "event_type": "reasoning_message_content",
                    "data": {"content": "durable reasoning"}
                }),
            )
            .await
            .expect("durable reasoning event");
        materialize_server_run_transcript_evidence(
            &pool,
            &user_id,
            &session_id,
            &run_id,
            None,
            None,
        )
        .await
        .expect("reasoning-only materialization acquires admission");
        let enriched: String = sqlx::query_scalar(
            "SELECT payload_json FROM session_transcript_items
             WHERE user_id = ? AND session_id = ? AND source_event_id = ?",
        )
        .bind(&user_id)
        .bind(&session_id)
        .bind(&expected)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&enriched).unwrap()["reasoning"],
            "durable reasoning"
        );
        let replay = terminal_assistant_transcript_item(
            &user_id,
            &session_id,
            &run_id,
            None,
            "inspect identity",
            &state,
        )
        .unwrap();
        persist_session_transcript_items(&pool, &user_id, &session_id, &[replay])
            .await
            .expect("replay after enrichment");
        let after_replay: String = sqlx::query_scalar(
            "SELECT payload_json FROM session_transcript_items
             WHERE user_id = ? AND session_id = ? AND source_event_id = ?",
        )
        .bind(&user_id)
        .bind(&session_id)
        .bind(&expected)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(after_replay, enriched);
        sqlx::query(
            "UPDATE agent_sessions SET status = 'deleting' WHERE user_id = ? AND session_id = ?",
        )
        .bind(&user_id)
        .bind(&session_id)
        .execute(&db)
        .await
        .unwrap();
        materialize_server_run_transcript_evidence(
            &pool,
            &user_id,
            &session_id,
            &run_id,
            None,
            None,
        )
        .await
        .expect_err("reasoning-only materialization rejects deleting root");
        sqlx::query(
            "UPDATE agent_sessions SET status = 'active' WHERE user_id = ? AND session_id = ?",
        )
        .bind(&user_id)
        .bind(&session_id)
        .execute(&db)
        .await
        .unwrap();
        for sql in [
            "UPDATE agent_session_lifecycle_fences SET delete_requested_at = NOW(6) WHERE user_id = ? AND session_id = ?",
            "UPDATE agent_session_lifecycle_fences SET database_deleted_at = NOW(6) WHERE user_id = ? AND session_id = ?",
        ] {
            sqlx::query(sql)
                .bind(&user_id)
                .bind(&session_id)
                .execute(&db)
                .await
                .unwrap();
            materialize_server_run_transcript_evidence(
                &pool,
                &user_id,
                &session_id,
                &run_id,
                None,
                None,
            )
            .await
            .expect_err("reasoning-only materialization rejects durable deletion");
        }

        cleanup_core_persist_fixture_for_owner(&db, &session_id, &user_id).await;
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
    async fn canonical_append_rolls_back_when_transcript_ownership_or_late_allocation_fails() {
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
                trace_context: None,
                user_message: "wrong owner append",
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

        // Fail sequence allocation after one complete membership chunk and
        // several insert chunks, inside the actual canonical capture boundary.
        let store = Arc::new(
            DatabaseRunStateStore::new(pool.clone()).with_owner_pod_id("transcript-chunk-rollback"),
        );
        let engine = crate::server::run::engine::RunEngine::new(store.clone());
        let authority = engine
            .start_run(&run_id, &owner_user_id, &session_id)
            .await
            .expect("start rollback run");
        sqlx::query(
            "INSERT INTO session_transcript_items
             (user_id, session_id, item_seq, role, content, content_hash)
             VALUES (?, ?, ?, 'system', 'existing boundary', 'boundary-hash')",
        )
        .bind(&owner_user_id)
        .bind(&session_id)
        .bind(i64::MAX - TRANSCRIPT_MEMBERSHIP_ROWS as i64 - 1)
        .execute(&db)
        .await
        .expect("seed sequence exhaustion boundary");
        let guidance = (0..TRANSCRIPT_MEMBERSHIP_ROWS + 1)
            .map(|index| crate::turn::agentic_loop::host::AppliedUserIntent {
                intent_id: format!("rollback-guidance-{index}"),
                delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                status: astra_turn_types::UserIntentStatus::Applied,
                event_index: index,
                content: format!("applied guidance {index}"),
            })
            .collect::<Vec<_>>();
        state.user_intents.record_applied_user_intents(&guidance);
        let failure = persist_server_loop_canonical_append(
            &pool,
            CanonicalLoopAppend {
                user_id: &owner_user_id,
                session_id: &session_id,
                run_id: &run_id,
                expected_owner_generation: Some(authority.owner_generation),
                owner_lease_duration: Some(Duration::from_secs(45)),
                trace_context: None,
                user_message: "chunk rollback",
                include_terminal_assistant: true,
            },
            &state,
        )
        .await
        .expect_err("late guidance allocation aborts the whole canonical append");
        assert!(
            failure.contains("transcript sequence exhausted"),
            "{failure}"
        );
        let remaining: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM session_transcript_items WHERE user_id = ? AND session_id = ?",
        )
        .bind(&owner_user_id)
        .bind(&session_id)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(remaining, 1, "earlier insert chunks roll back together");
        let captured: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_events WHERE user_id = ? AND session_id = ? AND run_id = ?",
        )
        .bind(&owner_user_id)
        .bind(&session_id)
        .bind(&run_id)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(
            captured, 0,
            "canonical event capture rolls back with transcript chunks"
        );

        cleanup_core_persist_fixture_for_owner(&db, &session_id, &wrong_user_id).await;
        cleanup_core_persist_fixture_for_owner(&db, &session_id, &owner_user_id).await;
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
    async fn raw_transcript_cursor_binding_is_owner_scoped_and_idempotent() {
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
            materialize_server_run_transcript_evidence(
                &pool,
                &user_id,
                &session_id,
                &run_one,
                None,
                Some(&cursor_one),
            )
            .await
            .expect("cursor-only materialization and exact retry");
        }

        let committed: (Option<i64>, Option<i64>, Option<String>) = sqlx::query_as(
            "SELECT canonical_completed_turn, canonical_conversation_seq, canonical_root_hash FROM session_transcript_items WHERE user_id = ? AND session_id = ? AND run_id = ?",
        ).bind(&user_id).bind(&session_id).bind(&run_one).fetch_one(&db).await.unwrap();
        assert_eq!(
            committed,
            (
                Some(1),
                Some(1),
                Some(cursor_one.canonical_root_hash.clone())
            )
        );
        let wrong_cursor = SessionCursorV1 {
            owner_id: "another-owner".into(),
            ..cursor_one.clone()
        };
        let mut tx = db.begin().await.unwrap();
        assert!(
            bind_run_transcript_to_cursor_in_tx(
                &mut tx,
                &user_id,
                &session_id,
                &run_one,
                &wrong_cursor
            )
            .await
            .is_err()
        );
        tx.rollback().await.unwrap();
        sqlx::query(
            "UPDATE agent_sessions SET status = 'deleting' WHERE user_id = ? AND session_id = ?",
        )
        .bind(&user_id)
        .bind(&session_id)
        .execute(&db)
        .await
        .unwrap();
        materialize_server_run_transcript_evidence(
            &pool,
            &user_id,
            &session_id,
            &run_one,
            None,
            Some(&cursor_one),
        )
        .await
        .expect_err("cursor-only materialization rejects deleting root");

        cleanup_transcript_fixture_for_owner(&db, &session_id, &user_id).await;
    }
}
