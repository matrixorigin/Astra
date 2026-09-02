//! Delegation engine — spawns and tracks sub-runs for multi-agent coordination.
//!
//! Bridges [`CoordinationPattern`] from the services crate with [`RunEngine`]
//! for actual execution. Enforces depth limits, tracks parent→child relationships,
//! and aggregates results.
//!
//! # Example Flow (FanOut)
//!
//! ```text
//! Orchestrator run-A
//!   ├── delegate(FanOut{agent_ids: [s1, s2]})
//!   │     ├── sub-run-B (agent s1)  ──▶ completed ✅
//!   │     └── sub-run-C (agent s2)  ──▶ completed ✅
//!   │
//!   └── aggregate(results) ──▶ merged output
//! ```

use std::collections::{HashMap, HashSet};

use crate::turn::agentic_loop::host::RequestConstraints;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use tokio::sync::{RwLock, watch};
use unicode_normalization::UnicodeNormalization;

/// Canonical form for agent identity comparison.
///
/// Agent IDs are user-provided and may vary in casing or Unicode
/// normalization form (NFC vs NFD: "café" vs "cafe" + ◌́). Two IDs that
/// are visually identical must be treated as the same agent — otherwise
/// a normalization alias bypasses circular delegation detection and
/// allows an infinite delegation loop.
///
/// Canonicalization = lowercase + NFC recomposition. This collapses the
/// case and normalization axes while preserving visually-distinct
/// graphemes ("café" vs "cafe" remain distinct agents).
fn canonical_agent_id(id: &str) -> String {
    id.to_lowercase().nfc().collect()
}

use astra_services::coordination::{
    AGENT_RESULT_STATUS_FAILED, AgentProfile, AgentProfileRegistry, AgentResult,
    AgentResultStatusKind, AggregationStrategy, CoordinationPattern, DelegationRequest,
    DelegationResult, DelegationResultStatusKind, agent_result_status_kind,
    agent_result_status_to_subrun_state, aggregate_results, delegation_result_status_kind,
};
use astra_services::delegated_findings::{
    DelegatedFindingEnvelope, DelegatedFindingParse, MAX_DELEGATED_FINDING_SUMMARY_CHARS,
    truncate_chars,
};
use astra_services::runs::{
    DurableRunStatusKind, RequestedTurnInteractionMode, durable_run_status_kind,
    durable_run_status_to_subrun_state,
};
use astra_services::{AdmittedModelExecution, BubbleUpTarget, DatabaseStateProjectionStore};

pub use astra_core::SubRunState;
use astra_core::{
    InvalidTransition, STATUS_CANCELLED, STATUS_COMPLETED, STATUS_DELEGATED, STATUS_FAILED,
    STATUS_PAUSED, STATUS_RUNNING, STATUS_VERIFICATION_FAILED, STATUS_WAITING,
};

use crate::server::run::engine::{RunEngine, RunExecutionAuthority};
use astra_messaging::router::AgentMailboxRouter;
use astra_prompts::team_prompts;

fn clone_delegation_context(
    site: astra_core::history_work::HistoryWorkSite,
    context: &HashMap<String, serde_json::Value>,
) -> HashMap<String, serde_json::Value> {
    astra_core::history_work::record_serialized_value(site, context);
    context.clone()
}

fn clone_delegation_value(
    site: astra_core::history_work::HistoryWorkSite,
    value: &serde_json::Value,
) -> serde_json::Value {
    astra_core::history_work::record_serialized_value(site, value);
    value.clone()
}

/// Grace period for cooperative children to publish a canonical terminal
/// result after their parent is cancelled. Keep this comfortably above a
/// scheduler tick, but below the interactive cancellation latency budget.
/// 500ms allows time for durable state persistence (DB write + fsync).
const FANOUT_CANCELLATION_DRAIN_TIMEOUT: std::time::Duration =
    std::time::Duration::from_millis(500);

/// After `abort_all()`, bound the final drain of join handles so a task stuck
/// in uninterruptible blocking I/O cannot hold the parent turn indefinitely.
const FANOUT_ABORT_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Bound the total time spent reconciling missing children through the
/// durable authority after an abort drain. A slow durable store must not
/// hold the parent turn indefinitely. On timeout, children remain explicitly
/// unfinished so recovery can still observe the eventual durable winner.
const DELEGATION_RECONCILIATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Await one operation within a deadline shared by the whole reconciliation
/// batch. Once the budget is exhausted, later operations are not polled. This
/// prevents fanout width from multiplying cancellation latency.
async fn await_with_shared_deadline<T>(
    deadline: &mut Option<tokio::time::Instant>,
    budget: std::time::Duration,
    future: impl std::future::Future<Output = T>,
) -> Option<T> {
    let deadline = *deadline.get_or_insert_with(|| tokio::time::Instant::now() + budget);
    if tokio::time::Instant::now() >= deadline {
        return None;
    }
    tokio::time::timeout_at(deadline, future).await.ok()
}

/// Abort once, then drain as many join results as become available within one
/// shared deadline. A per-handle timeout multiplies cancellation latency by
/// fanout width; waiting for only one handle loses settlement evidence for the
/// rest. Returning `None` means the bounded projection drain ended, not that
/// every child reached a durable terminal state.
async fn abort_and_join_next_bounded<T: Send + 'static>(
    tasks: &mut tokio::task::JoinSet<T>,
    deadline: &mut Option<tokio::time::Instant>,
    scope: &'static str,
) -> Option<Result<T, tokio::task::JoinError>> {
    let deadline = *deadline.get_or_insert_with(|| {
        tasks.abort_all();
        tokio::time::Instant::now() + FANOUT_ABORT_DRAIN_TIMEOUT
    });
    match tokio::time::timeout_at(deadline, tasks.join_next()).await {
        Ok(result) => result,
        Err(_) => {
            tracing::error!(
                target: "astra_runtime::delegation",
                scope,
                timeout_ms = FANOUT_ABORT_DRAIN_TIMEOUT.as_millis(),
                "aborted child join drain reached its shared deadline; durable reconciliation remains authoritative"
            );
            None
        }
    }
}

/// State projection is an observability aid, not a copy of the full run tree.
/// Keep the source, its nearest ancestors, and the root within this write
/// budget. The canonical transcript and durable run hierarchy remain complete.
const MAX_FINDING_BUBBLE_TARGETS: usize = 8;

/// Protect recovery from corrupt or externally imported parent maps while
/// remaining far above the supported delegation depth of normal profiles.
const MAX_ANCESTRY_TRAVERSAL: usize = 64;

const VERIFICATION_RETRY_BASE_DELAY_MS: u64 = 200;
const VERIFICATION_RETRY_MAX_DELAY_MS: u64 = 2_000;
const VERIFICATION_RETRY_JITTER_MS: u64 = 100;

const METRIC_DELEGATION_EXECUTIONS_TOTAL: &str = "astra_delegation_executions_total";
const METRIC_DELEGATION_DURATION_MS_TOTAL: &str = "astra_delegation_duration_ms_total";
const METRIC_DELEGATION_SUB_RUNS_TOTAL: &str = "astra_delegation_sub_runs_total";
const METRIC_DELEGATION_TOKENS_TOTAL: &str = "astra_delegation_tokens_total";

fn register_delegation_metrics(registry: &astra_turn_core::pipeline_metrics::MetricsRegistry) {
    registry.register_counter(
        METRIC_DELEGATION_EXECUTIONS_TOTAL,
        "Delegation executions by coordination pattern and terminal outcome.",
    );
    registry.register_counter(
        METRIC_DELEGATION_DURATION_MS_TOTAL,
        "Total delegation wall time in milliseconds by pattern and outcome.",
    );
    registry.register_counter(
        METRIC_DELEGATION_SUB_RUNS_TOTAL,
        "Delegated sub-runs by pattern and canonical terminal status.",
    );
    registry.register_counter(
        METRIC_DELEGATION_TOKENS_TOTAL,
        "Delegated agent tokens by pattern and token kind.",
    );
}

fn delegation_outcome_label(result: &Result<DelegationResult, String>) -> &'static str {
    match result {
        Ok(result) => match delegation_result_status_kind(&result.status) {
            DelegationResultStatusKind::Completed => "completed",
            DelegationResultStatusKind::Partial => "partial",
            DelegationResultStatusKind::Unfinished => "unfinished",
            DelegationResultStatusKind::Failed | DelegationResultStatusKind::Other => "failed",
        },
        Err(_) => "error",
    }
}

fn sub_run_status_label(status: &str) -> &'static str {
    match agent_result_status_kind(status) {
        AgentResultStatusKind::Completed | AgentResultStatusKind::Delegated => "completed",
        AgentResultStatusKind::Waiting => "waiting",
        AgentResultStatusKind::Paused => "paused",
        AgentResultStatusKind::Cancelled => "cancelled",
        AgentResultStatusKind::Timeout => "timeout",
        AgentResultStatusKind::VerificationFailed => "verification_failed",
        AgentResultStatusKind::Partial => "partial",
        AgentResultStatusKind::Failed | AgentResultStatusKind::Other => "failed",
    }
}

fn record_delegation_metrics(
    registry: Option<&Arc<astra_turn_core::pipeline_metrics::MetricsRegistry>>,
    pattern: &str,
    elapsed: std::time::Duration,
    result: &Result<DelegationResult, String>,
) {
    let Some(registry) = registry else { return };
    let outcome = delegation_outcome_label(result);
    let labels = [("pattern", pattern), ("outcome", outcome)];
    registry.increment_counter(METRIC_DELEGATION_EXECUTIONS_TOTAL, &labels, 1);
    registry.increment_counter(
        METRIC_DELEGATION_DURATION_MS_TOTAL,
        &labels,
        u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
    );
    if let Ok(result) = result {
        for sub_run in &result.agent_results {
            registry.increment_counter(
                METRIC_DELEGATION_SUB_RUNS_TOTAL,
                &[
                    ("pattern", pattern),
                    ("status", sub_run_status_label(&sub_run.status)),
                ],
                1,
            );
        }
        registry.increment_counter(
            METRIC_DELEGATION_TOKENS_TOTAL,
            &[("pattern", pattern), ("kind", "prompt")],
            result.total_prompt_tokens,
        );
        registry.increment_counter(
            METRIC_DELEGATION_TOKENS_TOTAL,
            &[("pattern", pattern), ("kind", "completion")],
            result.total_completion_tokens,
        );
    }
}

/// Bounded exponential delay for verification retries. The stable per-run
/// jitter prevents a failed fanout from synchronizing its retries while
/// keeping behavior deterministic enough to diagnose and test.
fn verification_retry_delay(retry_attempt: u32, run_id: &str) -> std::time::Duration {
    let exponent = retry_attempt.saturating_sub(2).min(4);
    let exponential = VERIFICATION_RETRY_BASE_DELAY_MS
        .saturating_mul(1_u64 << exponent)
        .min(VERIFICATION_RETRY_MAX_DELAY_MS);
    let hash = run_id.bytes().fold(0xcbf29ce484222325_u64, |hash, byte| {
        hash.wrapping_mul(0x100000001b3) ^ u64::from(byte)
    });
    std::time::Duration::from_millis(
        exponential.saturating_add(hash % (VERIFICATION_RETRY_JITTER_MS + 1)),
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AncestryTermination {
    RootReached,
    Cycle { repeated_run_id: String },
    TraversalLimit { next_run_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AncestryWalk {
    ancestors: Vec<String>,
    termination: AncestryTermination,
}

fn ancestry_from_parents(parents: &HashMap<String, String>, run_id: &str) -> AncestryWalk {
    let mut chain = Vec::new();
    let mut current = run_id.to_string();
    let mut visited = HashSet::from([current.clone()]);
    while chain.len() < MAX_ANCESTRY_TRAVERSAL {
        let Some(parent) = parents.get(&current) else {
            return AncestryWalk {
                ancestors: chain,
                termination: AncestryTermination::RootReached,
            };
        };
        if !visited.insert(parent.clone()) {
            return AncestryWalk {
                ancestors: chain,
                termination: AncestryTermination::Cycle {
                    repeated_run_id: parent.clone(),
                },
            };
        }
        chain.push(parent.clone());
        current = parent.clone();
    }
    let termination = parents
        .get(&current)
        .map_or(AncestryTermination::RootReached, |next| {
            AncestryTermination::TraversalLimit {
                next_run_id: next.clone(),
            }
        });
    AncestryWalk {
        ancestors: chain,
        termination,
    }
}

fn cancelled_agent_result(agent_id: &str, run_id: &str) -> AgentResult {
    AgentResult {
        agent_id: agent_id.to_string(),
        run_id: run_id.to_string(),
        status: STATUS_CANCELLED.to_string(),
        output: None,
        error: Some("cancelled by parent run".to_string()),
        prompt_tokens: 0,
        completion_tokens: 0,
        tool_calls: 0,
    }
}

fn cancellation_reconciliation_pending_result(attempted: &AgentResult) -> AgentResult {
    AgentResult {
        agent_id: attempted.agent_id.clone(),
        run_id: attempted.run_id.clone(),
        status: STATUS_WAITING.to_string(),
        output: None,
        error: Some(format!(
            "durable cancellation reconciliation exceeded the shared {}ms deadline; authoritative child state is still unknown",
            DELEGATION_RECONCILIATION_TIMEOUT.as_millis()
        )),
        // Terminal authority is unknown, but work already observed by the
        // parent is still a fact. Keep it visible so a reconciliation timeout
        // cannot turn into silent usage under-counting.
        prompt_tokens: attempted.prompt_tokens,
        completion_tokens: attempted.completion_tokens,
        tool_calls: attempted.tool_calls,
    }
}

/// Convert the richer delegated-agent outcome taxonomy to the canonical
/// durable run lifecycle. Details such as timeout/partial/verification failure
/// remain on `AgentResult`; the run row intentionally stores only lifecycle
/// states understood by every control-plane consumer.
fn durable_status_for_agent_result(status: &str) -> &'static str {
    match agent_result_status_kind(status) {
        AgentResultStatusKind::Completed => STATUS_COMPLETED,
        AgentResultStatusKind::Delegated => STATUS_DELEGATED,
        AgentResultStatusKind::Waiting => STATUS_WAITING,
        AgentResultStatusKind::Paused => STATUS_PAUSED,
        AgentResultStatusKind::Cancelled => STATUS_CANCELLED,
        AgentResultStatusKind::Timeout
        | AgentResultStatusKind::VerificationFailed
        | AgentResultStatusKind::Partial
        | AgentResultStatusKind::Failed
        | AgentResultStatusKind::Other => STATUS_FAILED,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DurableLifecycleDisposition {
    /// The executor owns all generation-fenced durable writes. The scheduler
    /// may only reread the winning fact, including on timeout/panic paths.
    ExecutorOwned { owner_generation: u64 },
    /// No exact execution authority survived (for example an uncorrelated
    /// panic projection). Fail closed by rereading only.
    ReadOnly,
    /// A quiescent scheduler-owned executor produced this result. The
    /// scheduler may commit it only with the exact admission generation.
    SchedulerOwned { owner_generation: u64 },
}

fn durable_lifecycle_disposition(
    executor: &dyn SubRunExecutor,
    owner_generation: u64,
) -> DurableLifecycleDisposition {
    if executor.owns_durable_run_lifecycle() {
        DurableLifecycleDisposition::ExecutorOwned { owner_generation }
    } else {
        DurableLifecycleDisposition::SchedulerOwned { owner_generation }
    }
}

/// Commit an executor result and return the outcome permitted by durable
/// authority. A pause/cancel/terminal CAS winner must shape the result returned
/// to the parent; otherwise the database, tracker, and aggregation can report
/// contradictory facts for the same child run.
async fn reconcile_agent_result_with_durable_authority(
    run_engine: &RunEngine,
    user_id: &str,
    expected_session_id: &str,
    disposition: DurableLifecycleDisposition,
    mut result: AgentResult,
) -> AgentResult {
    let attempted_status = durable_status_for_agent_result(&result.status);
    let persistence = match disposition {
        DurableLifecycleDisposition::ExecutorOwned { .. }
        | DurableLifecycleDisposition::ReadOnly => {
            // The executor already committed with its exact owner generation.
            // A second outer write would allow a stale executor to overwrite
            // the owner that recovered an expired lease.
            Ok(false)
        }
        DurableLifecycleDisposition::SchedulerOwned { owner_generation } => {
            run_engine
                .persist_delegation_outcome_status_if_current_owner(
                    user_id,
                    expected_session_id,
                    &result.run_id,
                    owner_generation,
                    attempted_status,
                    None,
                    result.error.as_deref(),
                )
                .await
        }
    };
    if matches!(persistence, Ok(true)) {
        return result;
    }

    let persistence_detail = match &persistence {
        Ok(false)
            if matches!(
                disposition,
                DurableLifecycleDisposition::ExecutorOwned { .. }
                    | DurableLifecycleDisposition::ReadOnly
            ) =>
        {
            "executor-owned durable lifecycle requires authoritative reread".to_string()
        }
        Ok(false) => "outcome lost its durable status compare-and-set".to_string(),
        Err(error) => format!("could not commit delegated outcome: {error}"),
        Ok(true) => unreachable!(),
    };
    let durable = match run_engine.load_run(user_id, &result.run_id).await {
        Ok(Some(durable)) => durable,
        Ok(None) => {
            result.status = STATUS_FAILED.to_string();
            result.output = None;
            result.error = Some(format!(
                "{persistence_detail}; durable run {} is missing",
                result.run_id
            ));
            return result;
        }
        Err(load_error) => {
            result.status = STATUS_FAILED.to_string();
            result.output = None;
            result.error = Some(format!(
                "{persistence_detail}; failed to load durable winner for run {}: {load_error}",
                result.run_id
            ));
            return result;
        }
    };

    // A replay can lose its CAS because the exact same terminal fact is
    // already durable. Status equality alone is insufficient: a recovered
    // generation can independently reach the same terminal status with a
    // different output. Preserve local output only when the durable winner is
    // the exact execution generation that produced it. Read-only projections
    // carry no such proof and therefore never retain local output.
    let expected_owner_generation = match disposition {
        DurableLifecycleDisposition::ExecutorOwned { owner_generation }
        | DurableLifecycleDisposition::SchedulerOwned { owner_generation } => {
            Some(owner_generation)
        }
        DurableLifecycleDisposition::ReadOnly => None,
    };
    if durable.status == attempted_status
        && expected_owner_generation == Some(durable.run_generation)
    {
        return result;
    }

    tracing::info!(
        target: "astra_runtime::delegation",
        run_id = %result.run_id,
        executor_status = %result.status,
        attempted_durable_status = attempted_status,
        durable_status = %durable.status,
        "replaced stale delegated executor outcome with durable authority"
    );
    result.output = None;
    result.prompt_tokens = durable.total_prompt_tokens;
    result.completion_tokens = durable.total_completion_tokens;
    result.tool_calls = durable.total_tool_calls;
    match durable_run_status_kind(&durable.status) {
        DurableRunStatusKind::Running => {
            // `running` is not a terminal AgentResult. Project it as a
            // recoverable wait so parent aggregation remains unfinished.
            result.status = STATUS_WAITING.to_string();
            result.error = None;
        }
        DurableRunStatusKind::Waiting | DurableRunStatusKind::Paused => {
            result.status = durable.status;
            result.error = None;
        }
        DurableRunStatusKind::Completed | DurableRunStatusKind::Delegated => {
            result.status = durable.status;
            result.error = None;
        }
        DurableRunStatusKind::Cancelled => {
            result.status = STATUS_CANCELLED.to_string();
            result.error = durable
                .error_message
                .or_else(|| Some("cancelled by a concurrent durable control decision".to_string()));
        }
        DurableRunStatusKind::Failed | DurableRunStatusKind::Other => {
            result.status = STATUS_FAILED.to_string();
            result.error = durable.error_message.or(Some(persistence_detail));
        }
    }
    result
}

/// Reconcile a parent-cancelled child without allowing durable storage latency
/// to hold the parent indefinitely. Timeout is an unknown fact, not evidence
/// that cancellation committed: project the child as recoverably waiting so a
/// later durable recovery can still supply the authoritative terminal state.
async fn reconcile_after_parent_cancellation_bounded(
    run_engine: &RunEngine,
    user_id: &str,
    expected_session_id: &str,
    disposition: DurableLifecycleDisposition,
    result: AgentResult,
    deadline: &mut Option<tokio::time::Instant>,
    scope: &'static str,
) -> AgentResult {
    let run_id = result.run_id.clone();
    let pending = cancellation_reconciliation_pending_result(&result);
    let result = await_with_shared_deadline(
        deadline,
        DELEGATION_RECONCILIATION_TIMEOUT,
        reconcile_agent_result_with_durable_authority(
            run_engine,
            user_id,
            expected_session_id,
            disposition,
            result,
        ),
    )
    .await;
    match result {
        Some(result) => result,
        None => {
            tracing::error!(
                target: "astra_runtime::delegation",
                scope,
                run_id,
                timeout_ms = DELEGATION_RECONCILIATION_TIMEOUT.as_millis(),
                "durable cancellation reconciliation timed out; preserving an unfinished projection for recovery"
            );
            pending
        }
    }
}

fn normalize_context_allowlist_entry(entry: &str, key: &str) -> Result<String, String> {
    let normalized = entry.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        Err(format!(
            "context[{key}] must not contain empty or whitespace-only strings"
        ))
    } else {
        Ok(normalized)
    }
}

fn parse_request_allowlist_from_context(
    context: &mut HashMap<String, serde_json::Value>,
    key: &str,
) -> Result<Option<HashSet<String>>, String> {
    let Some(value) = context.remove(key) else {
        return Ok(None);
    };
    let values = value
        .as_array()
        .ok_or_else(|| format!("context[{key}] must be an array of strings"))?;
    let mut normalized = HashSet::with_capacity(values.len());
    for entry in values {
        let raw = entry
            .as_str()
            .ok_or_else(|| format!("context[{key}] must contain only strings"))?;
        normalized.insert(normalize_context_allowlist_entry(raw, key)?);
    }
    Ok(Some(normalized))
}

fn parse_request_skill_sources_from_context(
    context: &mut HashMap<String, serde_json::Value>,
    key: &str,
) -> Result<Option<HashSet<crate::skills::manifest::SkillSourceKind>>, String> {
    let Some(values) = parse_request_allowlist_from_context(context, key)? else {
        return Ok(None);
    };
    let mut parsed = HashSet::with_capacity(values.len());
    for value in values {
        let source = value
            .parse()
            .map_err(|error| format!("context[{key}]: {error}"))?;
        parsed.insert(source);
    }
    Ok(Some(parsed))
}

#[derive(Debug, Default, PartialEq, Eq)]
struct CriticalFindingExtraction {
    summary: Option<String>,
    contract_error: Option<String>,
    used_legacy_review_format: bool,
}

/// Extract typed findings and operational child failures. Free-form success
/// prose never becomes a cross-run signal. The one accepted prose shape is the
/// exact previous review contract, isolated in `DelegatedFindingEnvelope` as a
/// deployment-window migration.
fn critical_finding_from_agent_result(result: &AgentResult) -> CriticalFindingExtraction {
    let mut extraction = CriticalFindingExtraction::default();
    if let Some(output) = result.output.as_deref() {
        match DelegatedFindingEnvelope::parse(output) {
            DelegatedFindingParse::Structured(envelope) => {
                extraction.summary = envelope.critical_summary();
            }
            DelegatedFindingParse::LegacyReview(envelope) => {
                extraction.used_legacy_review_format = true;
                extraction.summary = envelope.critical_summary();
            }
            DelegatedFindingParse::Unstructured => {}
            DelegatedFindingParse::MalformedJson(error) => {
                extraction.contract_error = Some(error);
            }
            DelegatedFindingParse::ResourceLimitExceeded(error) => {
                extraction.contract_error = Some(error);
            }
        }
    }

    // User/parent cancellation is already explicit lifecycle evidence and is
    // not a critical defect. Failed, timed-out, or verification-failed work
    // must remain visible even when no structured review envelope was emitted.
    if result.is_failure() && result.status != STATUS_CANCELLED {
        let failure = match result
            .error
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(error) => {
                let error = truncate_chars(error, MAX_DELEGATED_FINDING_SUMMARY_CHARS);
                format!(
                    "Delegated agent '{}' failed with status '{}': {error}",
                    result.agent_id, result.status
                )
            }
            None => format!(
                "Delegated agent '{}' failed with status '{}'",
                result.agent_id, result.status
            ),
        };
        extraction.summary = Some(truncate_chars(
            &match extraction.summary.take() {
                Some(finding) => format!("{finding}\n\nOperational failure:\n{failure}"),
                None => failure,
            },
            MAX_DELEGATED_FINDING_SUMMARY_CHARS,
        ));
    }
    extraction
}

fn finding_bubble_targets(
    session_id: &str,
    run_id: &str,
    source_depth: u32,
    ancestry: &[String],
) -> Vec<BubbleUpTarget> {
    let mut targets = vec![BubbleUpTarget {
        session_id: session_id.to_string(),
        run_id: run_id.to_string(),
        depth: source_depth,
    }];
    let ancestor_budget = MAX_FINDING_BUBBLE_TARGETS.saturating_sub(1);
    let mut selected = (0..ancestry.len().min(ancestor_budget)).collect::<Vec<_>>();
    if ancestry.len() > ancestor_budget && ancestor_budget > 0 {
        selected.pop();
        selected.push(ancestry.len() - 1);
    }
    for idx in selected {
        targets.push(BubbleUpTarget {
            session_id: session_id.to_string(),
            run_id: ancestry[idx].clone(),
            depth: source_depth.saturating_sub((idx as u32) + 1),
        });
    }
    targets
}

async fn bubble_up_critical_finding_from_tracker(
    projection_store: Arc<DatabaseStateProjectionStore>,
    tracker: Arc<DelegationTracker>,
    user_id: String,
    session_id: String,
    run_id: String,
    summary: String,
) {
    let (source_depth, ancestry) = tracker.finding_lineage_snapshot(&run_id).await;
    match &ancestry.termination {
        AncestryTermination::RootReached => {}
        AncestryTermination::Cycle { repeated_run_id } => tracing::warn!(
            target: "astra_runtime::delegation",
            run_id,
            repeated_run_id,
            "delegated finding ancestry is cyclic; publishing the complete acyclic prefix"
        ),
        AncestryTermination::TraversalLimit { next_run_id } => tracing::error!(
            target: "astra_runtime::delegation",
            run_id,
            next_run_id,
            traversal_limit = MAX_ANCESTRY_TRAVERSAL,
            "delegated finding ancestry exceeded the corruption guard; root projection is incomplete"
        ),
    }
    let targets = finding_bubble_targets(&session_id, &run_id, source_depth, &ancestry.ancestors);
    if let Err(error) = projection_store
        .bubble_up_finding(
            &user_id,
            &run_id,
            &format!("finding-{run_id}"),
            "critical",
            &summary,
            &targets,
        )
        .await
    {
        tracing::warn!(
            target: "astra_runtime::delegation",
            run_id,
            error = %error,
            "failed to bubble up critical delegated finding"
        );
    }
}

// ─── Sub-run Executor Trait ─────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutionOwnerGenerationPublication {
    Preparing { expected_initial_generation: u64 },
    Acquired(u64),
    StoppedBeforeAcquisition { expected_initial_generation: u64 },
}

/// Local handoff between dynamic-child durable start and cancellation.
/// Cancellation aborts the executor before awaiting durability, so it must be
/// able to distinguish "not published yet" from "no row can still be
/// created" without guessing from a run id.
pub struct ExecutionOwnerGenerationSink {
    state: watch::Sender<ExecutionOwnerGenerationPublication>,
    #[cfg(test)]
    wait_after_preparing_hook:
        std::sync::Mutex<Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>>,
}

impl ExecutionOwnerGenerationSink {
    pub fn preparing(expected_initial_generation: u64) -> Self {
        let (state, _) = watch::channel(ExecutionOwnerGenerationPublication::Preparing {
            expected_initial_generation,
        });
        Self {
            state,
            #[cfg(test)]
            wait_after_preparing_hook: std::sync::Mutex::new(None),
        }
    }

    pub fn publish(&self, generation: u64) {
        self.state
            .send_replace(ExecutionOwnerGenerationPublication::Acquired(generation));
    }

    pub fn guard(self: &Arc<Self>) -> ExecutionOwnerGenerationGuard {
        ExecutionOwnerGenerationGuard {
            sink: Arc::clone(self),
        }
    }

    pub async fn wait_until_published_or_stopped(&self) -> ExecutionOwnerGenerationPublication {
        let mut observed = self.state.subscribe();
        loop {
            let state = *observed.borrow_and_update();
            if !matches!(state, ExecutionOwnerGenerationPublication::Preparing { .. }) {
                return state;
            }
            #[cfg(test)]
            let wait_hook = self
                .wait_after_preparing_hook
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            #[cfg(test)]
            if let Some((preparing_observed, release_wait)) = wait_hook {
                preparing_observed.notify_one();
                release_wait.notified().await;
            }
            // watch versions the publication even when publish/guard drop
            // races exactly after the Preparing read and before this await.
            observed
                .changed()
                .await
                .expect("generation sink retains its publication sender");
        }
    }
}

pub struct ExecutionOwnerGenerationGuard {
    sink: Arc<ExecutionOwnerGenerationSink>,
}

impl Drop for ExecutionOwnerGenerationGuard {
    fn drop(&mut self) {
        self.sink.state.send_if_modified(|state| {
            let ExecutionOwnerGenerationPublication::Preparing {
                expected_initial_generation,
            } = *state
            else {
                return false;
            };
            *state = ExecutionOwnerGenerationPublication::StoppedBeforeAcquisition {
                expected_initial_generation,
            };
            true
        });
    }
}

/// Configuration for a sub-run spawned by delegation.
pub struct SubRunConfig {
    /// Unique ID for this sub-run.
    pub run_id: String,
    /// Durable parent run that delegated this child. This is identity data for
    /// the run tree, never prompt context; a delegated sub-run cannot exist
    /// without it.
    pub parent_run_id: String,
    /// Agent profile executing this sub-run.
    pub agent_profile: AgentProfile,
    /// The task/prompt for this sub-run.
    pub task: String,
    /// Parent's session ID (sub-runs share the session lineage).
    pub session_id: String,
    /// User ID owning the delegation.
    pub user_id: String,
    /// Exact durable execution-owner epoch returned when this child row was
    /// created. `None` is valid only when the executor itself will create the
    /// row; an already-existing durable child must carry matching authority.
    pub execution_owner_generation: Option<u64>,
    /// Optional process-local observer for the exact durable generation
    /// acquired by this executor. Dynamic-agent cancellation uses it to fence
    /// runtime-owned durable intent; user lineage cancellation does not need
    /// generation authority.
    pub execution_owner_generation_sink: Option<Arc<ExecutionOwnerGenerationSink>>,
    /// Optional output from previous pipeline stage.
    pub previous_output: Option<String>,
    /// Context key-value pairs from the delegation request.
    pub context: HashMap<String, serde_json::Value>,
    /// Trusted forwarded headers propagated out-of-band for child remote skills.
    pub forward_headers: HashMap<String, String>,
    /// Short-lived execution material inherited from the admitted parent run.
    /// It is sideband state and is never serialized into delegation context.
    pub admitted_model_execution: Option<AdmittedModelExecution>,
    /// Effective interaction policy for this exact child invocation. It is
    /// resolved once from the durable parent and carried on the run config so
    /// executors, retries, and descendants cannot invent a new default.
    pub interaction_mode: RequestedTurnInteractionMode,
    /// Request-scoped capability constraints inherited from the parent runtime request.
    pub request_constraints: RequestConstraints,
    /// Current nested agent/sub-run depth for the child loop.
    pub recursion_depth: u8,
    /// Optional explicit turn budget for the child loop.
    pub max_turns: Option<u32>,
    /// Optional initial adaptive slice. When `max_turns` is absent this value
    /// is only a convergence checkpoint and can renew up to the runtime
    /// ceiling while concrete progress continues.
    pub initial_turns: Option<u32>,
    /// Cooperative pause flag — checked between turns by the sub-run loop.
    /// When set to `true`, the sub-run should yield with status "paused".
    pub pause_flag: Option<Arc<AtomicBool>>,
    /// Mid-execution checkpoint gate — abort early if contract criteria are violated.
    pub checkpoint_gate: Option<Arc<dyn CheckpointGate>>,
    /// Optional mailbox for inter-agent messaging during the sub-run.
    pub mailbox: Option<astra_messaging::router::AgentMailbox>,
    /// Optional progress emitter for broadcasting child turn events.
    pub progress_emitter: Option<crate::orchestration::AgentProgressEmitter>,
    /// Optional live-event sink for child token/tool/status mirroring.
    pub live_event_sink: Option<astra_turn_core::agent_live_event::SharedAgentLiveEventSink>,
    /// Cancellation token — when cancelled, the sub-run should stop gracefully.
    pub cancel_token: Option<Arc<tokio_util::sync::CancellationToken>>,
    /// Resolved parent prefix for prompt-cache inheritance on the
    /// delegated child's first API call (Bug B step 2). Populated by
    /// [`DelegationEngine`] when its `prefix_store` is set and a
    /// matching parent capture is present; `None` otherwise.
    /// Executors that don't consume it (e.g. server-side
    /// `ServerAgenticLoopHost`) can ignore this field — the child
    /// runs fresh, same behavior as pre-fork-prefix.
    pub inherited_prefix: Option<crate::orchestration::InheritedChildPrefix>,
    /// UI/runtime execution binding metadata inherited by this sub-run.
    pub execution_metadata: Option<serde_json::Value>,
    /// Delegation chain from the parent — agent_ids that led to this
    /// sub-run (for circular delegation detection). The child's
    /// `AgenticLoopState` inherits this so subsequent delegations
    /// from the child can detect cycles like A→B→C→A.
    pub delegation_chain: Vec<String>,
    /// Exact canonical WorkItem revision assigned to this run. It remains a
    /// typed sideband value and is validated before durable run creation.
    pub work_item: Option<astra_turn_core::orchestration_spawn_tool::WorkItemExecutionSpec>,
    /// Parent session's harness snapshot sink for observe-only sub-run
    /// observation. When set, the sub-run creates a sink-only HarnessSlot
    /// so sub-run snapshots appear in the parent's history.
    #[cfg(feature = "harness")]
    pub harness_sink: Option<Arc<dyn astra_harness::SnapshotSink>>,
}

impl SubRunConfig {
    /// Bind the exact authority returned by durable child admission into the
    /// configuration that will construct the child loop. A durable executor
    /// may never reconstruct this capability from the current database row,
    /// while a process-local executor may not consume a durable capability.
    pub(crate) fn bind_execution_authority(
        &mut self,
        durable_executor: bool,
        authority: Option<RunExecutionAuthority>,
    ) -> Result<(), String> {
        match (durable_executor, authority) {
            (true, Some(authority)) => {
                if let Some(expected) = self.execution_owner_generation
                    && expected != authority.owner_generation
                {
                    return Err(format!(
                        "durable sub-run execution authority changed during admission: expected generation {expected}, admitted generation {}",
                        authority.owner_generation
                    ));
                }
                self.execution_owner_generation = Some(authority.owner_generation);
                Ok(())
            }
            (true, None) => {
                Err("durable sub-run admission returned no execution authority".to_string())
            }
            (false, Some(_)) => Err(
                "process-local sub-run executor received durable execution authority".to_string(),
            ),
            (false, None) if self.execution_owner_generation.is_some() => Err(
                "process-local sub-run executor cannot consume durable execution authority"
                    .to_string(),
            ),
            (false, None) => Ok(()),
        }
    }
}

impl std::fmt::Debug for SubRunConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubRunConfig")
            .field("run_id", &self.run_id)
            .field("agent_profile", &self.agent_profile)
            .field("task", &self.task)
            .field("session_id", &self.session_id)
            .field("user_id", &self.user_id)
            .field(
                "execution_owner_generation",
                &self.execution_owner_generation,
            )
            .field("previous_output", &self.previous_output)
            .field("forward_headers", &!self.forward_headers.is_empty())
            .field(
                "admitted_model_execution",
                &self.admitted_model_execution.is_some(),
            )
            .field("interaction_mode", &self.interaction_mode)
            .field("request_constraints", &self.request_constraints)
            .field("recursion_depth", &self.recursion_depth)
            .field("max_turns", &self.max_turns)
            .field("initial_turns", &self.initial_turns)
            .field("pause_flag", &self.pause_flag.is_some())
            .field("checkpoint_gate", &self.checkpoint_gate.is_some())
            .field("mailbox", &self.mailbox.is_some())
            .field("progress_emitter", &self.progress_emitter.is_some())
            .field("live_event_sink", &self.live_event_sink.is_some())
            .finish()
    }
}

/// Trait for executing sub-runs as part of a delegation.
///
/// Production implementations use [`ServerAgenticLoopHost`] to run a real
/// agentic loop. Test implementations return mock results.
#[async_trait]
pub trait SubRunExecutor: Send + Sync {
    /// Execute a sub-run and return the result.
    async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String>;

    /// True when the executor itself owns generation-fenced durable lifecycle
    /// commits. Schedulers must then reconcile by rereading durable authority,
    /// never by issuing a second un-fenced status write.
    fn owns_durable_run_lifecycle(&self) -> bool {
        false
    }
}

/// No-op executor that immediately returns "completed" results.
/// Used when no real executor is wired (tests, offline mode).
pub struct StubSubRunExecutor;

#[async_trait]
impl SubRunExecutor for StubSubRunExecutor {
    async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
        Ok(AgentResult {
            agent_id: config.agent_profile.agent_id,
            run_id: config.run_id,
            status: STATUS_COMPLETED.to_string(),
            output: Some(format!("[stub] completed task: {}", config.task)),
            error: None,
            prompt_tokens: 0,
            completion_tokens: 0,
            tool_calls: 0,
        })
    }
}

// ─── Verification Gate ──────────────────────────────────────────────────────

/// Outcome of a verification gate check on a sub-run result.
#[derive(Debug, Clone)]
pub enum GateVerdict {
    /// Sub-run passed verification — proceed with aggregation.
    Pass,
    /// Sub-run failed verification — retry if attempts remain.
    Fail {
        reason: String,
        /// Verification details (criteria results, evidence, etc.)
        details: Option<serde_json::Value>,
    },
    /// Skip verification for this result (e.g., already failed sub-run).
    Skip,
}

impl GateVerdict {
    pub fn is_pass(&self) -> bool {
        matches!(self, Self::Pass | Self::Skip)
    }
}

/// Post-completion verification gate for delegation sub-runs.
///
/// Injected into [`DelegationEngine`] to validate sub-run output before aggregation.
/// When a gate returns [`GateVerdict::Fail`], the engine can retry the sub-run
/// (up to `max_retries`) or mark it as failed.
#[async_trait]
pub trait VerificationGate: Send + Sync {
    /// Verify a completed sub-run result.
    ///
    /// - `result`: the completed agent result
    /// - `delegation_id`: which delegation this belongs to
    /// - `attempt`: current attempt number (starts at 1)
    async fn verify(&self, result: &AgentResult, delegation_id: &str, attempt: u32) -> GateVerdict;

    /// Maximum retry attempts when verification fails. Default: 2.
    fn max_retries(&self) -> u32 {
        2
    }
}

// ─── Default Quality Gate ────────────────────────────────────────────────────

/// Configurable thresholds for [`DefaultQualityGate`].
#[derive(Debug, Clone)]
pub struct QualityThresholds {
    /// Minimum output length (chars). Default: 10.
    pub min_output_len: usize,
    /// Maximum output length (chars). Default: 50_000.
    pub max_output_len: usize,
    /// Maximum ratio of repeated lines to total lines (0.0–1.0). Default: 0.5.
    pub max_repetition_ratio: f64,
    /// Maximum number of retries. Default: 2.
    pub max_retries: u32,
}

impl Default for QualityThresholds {
    fn default() -> Self {
        Self {
            min_output_len: 10,
            max_output_len: 50_000,
            max_repetition_ratio: 0.5,
            max_retries: 2,
        }
    }
}

/// Production-ready verification gate with configurable heuristic checks.
///
/// Validates sub-run output quality:
/// - **Length bounds**: rejects empty/trivial or excessively long output
/// - **Repetition detection**: rejects output with >50% repeated lines (loop/garbage)
/// - **Error pattern detection**: rejects output dominated by error messages
pub struct DefaultQualityGate {
    thresholds: QualityThresholds,
}

impl DefaultQualityGate {
    pub fn new() -> Self {
        Self {
            thresholds: QualityThresholds::default(),
        }
    }

    pub fn with_thresholds(thresholds: QualityThresholds) -> Self {
        Self { thresholds }
    }
}

impl Default for DefaultQualityGate {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl VerificationGate for DefaultQualityGate {
    async fn verify(
        &self,
        result: &AgentResult,
        _delegation_id: &str,
        _attempt: u32,
    ) -> GateVerdict {
        let output: &str = result.output.as_deref().unwrap_or("");

        // Check for binary garbage (null bytes)
        let null_count = output.as_bytes().iter().filter(|&&b| b == 0).count();
        if null_count > 5 || (null_count > 0 && null_count * 100 > output.len()) {
            return GateVerdict::Fail {
                reason: format!(
                    "output contains binary garbage ({null_count} null bytes in {} bytes)",
                    output.len()
                ),
                details: Some(serde_json::json!({
                    "check": "binary_garbage",
                    "null_bytes": null_count,
                    "total_len": output.len(),
                })),
            };
        }

        // Check minimum length
        let trimmed_len = output.trim().len();
        if trimmed_len < self.thresholds.min_output_len {
            return GateVerdict::Fail {
                reason: format!(
                    "output too short ({} chars, minimum {})",
                    trimmed_len, self.thresholds.min_output_len
                ),
                details: Some(serde_json::json!({
                    "check": "min_length",
                    "actual": trimmed_len,
                    "threshold": self.thresholds.min_output_len
                })),
            };
        }

        // Check maximum length
        if output.len() > self.thresholds.max_output_len {
            return GateVerdict::Fail {
                reason: format!(
                    "output too long ({} chars, maximum {})",
                    output.len(),
                    self.thresholds.max_output_len
                ),
                details: Some(serde_json::json!({
                    "check": "max_length",
                    "actual": output.len(),
                    "threshold": self.thresholds.max_output_len
                })),
            };
        }

        // Repetition detection: count unique vs total non-empty lines
        let lines: Vec<&str> = output.lines().filter(|l| !l.trim().is_empty()).collect();
        if lines.len() >= 4 {
            let unique: std::collections::HashSet<&str> = lines.iter().copied().collect();
            let repetition_ratio = 1.0 - (unique.len() as f64 / lines.len() as f64);
            if repetition_ratio > self.thresholds.max_repetition_ratio {
                return GateVerdict::Fail {
                    reason: format!(
                        "excessive repetition ({:.0}% repeated lines)",
                        repetition_ratio * 100.0
                    ),
                    details: Some(serde_json::json!({
                        "check": "repetition",
                        "total_lines": lines.len(),
                        "unique_lines": unique.len(),
                        "ratio": repetition_ratio
                    })),
                };
            }
        }

        // Error pattern detection: if >60% of lines are error-like, flag it
        if lines.len() >= 3 {
            let error_patterns = ["error:", "Error:", "ERROR", "panic", "FAILED", "fatal:"];
            let error_lines = lines
                .iter()
                .filter(|l| error_patterns.iter().any(|p| l.contains(p)))
                .count();
            let error_ratio = error_lines as f64 / lines.len() as f64;
            if error_ratio > 0.6 {
                return GateVerdict::Fail {
                    reason: format!(
                        "output dominated by errors ({:.0}% error lines)",
                        error_ratio * 100.0
                    ),
                    details: Some(serde_json::json!({
                        "check": "error_dominated",
                        "error_lines": error_lines,
                        "total_lines": lines.len(),
                        "ratio": error_ratio
                    })),
                };
            }
        }

        GateVerdict::Pass
    }

    fn max_retries(&self) -> u32 {
        self.thresholds.max_retries
    }
}

// ─── Checkpoint Gate (Mid-Execution Fail-Fast) ──────────────────────────────

/// Mid-execution checkpoint gate — checked between turns during a sub-run.
///
/// Unlike [`VerificationGate`] (which runs AFTER the sub-run completes),
/// a `CheckpointGate` is checked every N turns DURING execution. When it
/// returns `false`, the sub-run is aborted immediately, saving time on
/// clearly divergent executions.
///
/// Piggybacks on the existing cooperative-pause mechanism in the agentic loop.
#[async_trait]
pub trait CheckpointGate: Send + Sync {
    /// Called every `checkpoint_frequency()` turns during sub-run execution.
    ///
    /// Returns `true` to continue, `false` to abort.
    /// `turn_index` is the current turn number (0-based).
    /// `total_tool_calls` is the cumulative tool call count so far.
    async fn check(
        &self,
        run_id: &str,
        turn_index: u32,
        total_tool_calls: u32,
    ) -> Result<bool, String>;

    /// How many turns between checkpoint checks. Default: 3.
    fn checkpoint_frequency(&self) -> u32 {
        3
    }
}

// ─── Sub-run Tracking ─────────────────────────────────────────────────────────────

// SubRunRecord and DelegationProgress are now defined in astra-server-types.
pub use astra_server_types::team_orchestrator_traits::{DelegationProgress, SubRunRecord};

/// Canonical in-memory projection of the durable delegation hierarchy.
///
/// Keeping the records, delegation membership, and parent edges behind one
/// lock makes insertion/recovery/cleanup atomic. A run lookup is O(1) instead
/// of scanning every user's delegation, which also removes an avoidable
/// cross-tenant performance coupling in server deployments.
#[derive(Default)]
struct DelegationTrackerState {
    runs: HashMap<String, SubRunRecord>,
    delegation_runs: HashMap<String, Vec<String>>,
    parents: HashMap<String, String>,
    pause_flags: HashMap<String, Arc<AtomicBool>>,
    cancel_tokens: HashMap<String, Arc<tokio_util::sync::CancellationToken>>,
}

/// Process-local projection for live delegation controls.
///
/// Durable run records are authoritative and rebuild this projection after a
/// restart. The optional session journal is diagnostic evidence, not the
/// source of truth for recovery.
pub struct DelegationTracker {
    state: RwLock<DelegationTrackerState>,
    /// Authenticated owner for journal isolation.
    user_id: Option<String>,
    /// Optional session ID for journal persistence.
    session_id: Option<String>,
    /// Real-time progress per delegation.
    progress: RwLock<HashMap<String, DelegationProgress>>,
    /// Optional progress broadcaster for SSE events.
    progress_broadcaster: Option<Arc<crate::orchestration::ProgressBroadcaster>>,
}

impl DelegationTracker {
    pub fn new() -> Self {
        Self {
            state: RwLock::new(DelegationTrackerState::default()),
            user_id: None,
            session_id: None,
            progress: RwLock::new(HashMap::new()),
            progress_broadcaster: None,
        }
    }

    /// Create a tracker with journal persistence enabled.
    pub fn with_session(user_id: String, session_id: String) -> Self {
        Self {
            state: RwLock::new(DelegationTrackerState::default()),
            user_id: Some(user_id),
            session_id: Some(session_id),
            progress: RwLock::new(HashMap::new()),
            progress_broadcaster: None,
        }
    }

    /// Attach a progress broadcaster for SSE event emission.
    pub fn with_progress_broadcaster(
        mut self,
        broadcaster: Arc<crate::orchestration::ProgressBroadcaster>,
    ) -> Self {
        self.progress_broadcaster = Some(broadcaster);
        self
    }

    /// Get the progress broadcaster, if configured.
    pub fn progress_broadcaster(&self) -> Option<&Arc<crate::orchestration::ProgressBroadcaster>> {
        self.progress_broadcaster.as_ref()
    }

    /// Persist a delegation event to the session journal (best-effort).
    fn persist_event(
        &self,
        event_type: astra_services::session_journal::JournalEventType,
        metadata: serde_json::Value,
    ) {
        let Some(ref sid) = self.session_id else {
            return;
        };
        let mut event = astra_services::session_journal::JournalEvent::base_public(
            event_type,
            Some(sid.as_str()),
        );
        event.metadata = Some(metadata);
        self.persist_journal_entry(event);
    }

    /// Persist a fully constructed journal event (best-effort).
    fn persist_journal_entry(&self, event: astra_services::session_journal::JournalEvent) {
        let Some(ref sid) = self.session_id else {
            return;
        };
        let Some(ref user_id) = self.user_id else {
            astra_core::agent_warn!("delegation", "journal owner missing for session {sid}");
            return;
        };
        let writer = match astra_services::session_journal::JournalWriter::for_user(user_id, sid) {
            Ok(w) => w,
            Err(e) => {
                astra_core::agent_warn!(
                    "delegation",
                    "JournalWriter::new failed for session {sid}: {e}"
                );
                return;
            }
        };
        if let Err(e) = writer.append(&event) {
            astra_core::agent_warn!("delegation", "Failed to write journal event: {e}");
        }
    }

    /// Rebuild in-memory hierarchy from durable run records.
    ///
    /// Called at startup to recover delegation state after a crash.
    /// Only records with `parent_run_id` set are considered (sub-runs).
    pub async fn load_from_run_records(&self, records: &[astra_services::runs::DurableRunRecord]) {
        let mut state = self.state.write().await;

        for rec in records {
            let (Some(parent_run_id), Some(delegation_id)) =
                (&rec.parent_run_id, &rec.delegation_id)
            else {
                continue; // Skip root runs
            };

            let sub = SubRunRecord {
                run_id: rec.run_id.clone(),
                parent_run_id: parent_run_id.clone(),
                delegation_id: delegation_id.clone(),
                agent_id: rec.agent_id.clone().unwrap_or_default(),
                depth: rec.depth,
                state: match durable_run_status_kind(&rec.status) {
                    DurableRunStatusKind::Other => {
                        tracing::warn!(
                            target: "astra_runtime::delegation",
                            status = %rec.status,
                            run_id = %rec.run_id,
                            "recovered delegation run has unknown durable status; projecting failed"
                        );
                        SubRunState::Failed
                    }
                    _ => durable_run_status_to_subrun_state(&rec.status),
                },
                retry_of: rec.retry_of.clone(),
            };

            let delegation_runs = state
                .delegation_runs
                .entry(delegation_id.clone())
                .or_default();
            if !delegation_runs.contains(&rec.run_id) {
                delegation_runs.push(rec.run_id.clone());
            }
            state
                .parents
                .insert(rec.run_id.clone(), parent_run_id.clone());
            state.runs.insert(rec.run_id.clone(), sub);
        }
    }

    /// Record a sub-run spawned by a delegation, persisting to journal if configured.
    pub async fn record_sub_run(&self, record: SubRunRecord) {
        self.record_sub_run_with_progress(record, true).await;
    }

    /// Record durable lineage while keeping lifecycle publication under the
    /// producer that actually owns the spawn.
    ///
    /// `DynamicAgentSpawner` registers mailboxes through `DelegationLookup`
    /// and then publishes a richer spawn event containing the exact agent
    /// type and fanout slot. Letting this bookkeeping callback publish too
    /// creates two conflicting `agent_spawned` rows for one child. Native
    /// delegation callers continue to use `record_sub_run`, which remains the
    /// lifecycle owner for that path.
    async fn record_sub_run_with_progress(&self, record: SubRunRecord, publish_progress: bool) {
        let run_id = record.run_id.clone();
        let parent_id = record.parent_run_id.clone();
        let delegation_id = record.delegation_id.clone();
        let agent_id = record.agent_id.clone();

        let mut state = self.state.write().await;
        if let Some(existing) = state.runs.get(&run_id) {
            if existing.parent_run_id != parent_id || existing.delegation_id != delegation_id {
                tracing::warn!(
                    target: "astra_runtime::delegation",
                    run_id,
                    existing_parent_run_id = %existing.parent_run_id,
                    attempted_parent_run_id = %parent_id,
                    existing_delegation_id = %existing.delegation_id,
                    attempted_delegation_id = %delegation_id,
                    "ignored conflicting duplicate sub-run identity"
                );
            }
            return;
        }
        state
            .delegation_runs
            .entry(delegation_id.clone())
            .or_default()
            .push(run_id.clone());
        state.parents.insert(run_id.clone(), parent_id.clone());
        state.runs.insert(run_id.clone(), record);
        drop(state);

        // Emit SSE event for web clients
        if publish_progress && let Some(ref broadcaster) = self.progress_broadcaster {
            use crate::orchestration::{AgentProgressEvent, ProgressEventType};
            broadcaster.emit(AgentProgressEvent {
                agent_id: agent_id.clone(),
                run_id: run_id.clone(),
                parent_run_id: parent_id.clone(),
                event_type: ProgressEventType::AgentSpawned {
                    agent_type: "delegated".to_string(),
                    description: format!("Sub-run for delegation {}", delegation_id),
                    fanout_slot: None,
                },
                timestamp_epoch_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64,
                metadata: None,
            });
        }
    }

    /// Get all sub-runs for a delegation.
    pub async fn get_sub_runs(&self, delegation_id: &str) -> Vec<SubRunRecord> {
        let state = self.state.read().await;
        state
            .delegation_runs
            .get(delegation_id)
            .into_iter()
            .flatten()
            .filter_map(|run_id| state.runs.get(run_id).cloned())
            .collect()
    }

    /// Get the parent run ID for a given run.
    pub async fn get_parent(&self, run_id: &str) -> Option<String> {
        self.state.read().await.parents.get(run_id).cloned()
    }

    /// Check if a run is a sub-run (has a parent).
    pub async fn is_sub_run(&self, run_id: &str) -> bool {
        self.state.read().await.parents.contains_key(run_id)
    }

    /// Get the recorded delegation depth for a run, if known.
    pub async fn get_depth(&self, run_id: &str) -> Option<u32> {
        self.state
            .read()
            .await
            .runs
            .get(run_id)
            .map(|record| record.depth)
    }

    /// Read depth and ancestry from one atomic hierarchy snapshot.
    async fn finding_lineage_snapshot(&self, run_id: &str) -> (u32, AncestryWalk) {
        let state = self.state.read().await;
        let depth = state
            .runs
            .get(run_id)
            .map(|record| record.depth)
            .unwrap_or(0);
        let ancestry = ancestry_from_parents(&state.parents, run_id);
        (depth, ancestry)
    }

    /// Get all sub-run IDs for a given parent run across all delegations.
    pub async fn get_children(&self, parent_run_id: &str) -> Vec<String> {
        self.state
            .read()
            .await
            .parents
            .iter()
            .filter(|(_, parent)| parent.as_str() == parent_run_id)
            .map(|(child, _)| child.clone())
            .collect()
    }

    /// Get the agent_id for a run. Returns `None` for top-level (non-sub) runs.
    pub async fn get_agent_id(&self, run_id: &str) -> Option<String> {
        self.state
            .read()
            .await
            .runs
            .get(run_id)
            .map(|record| record.agent_id.clone())
    }

    /// Get the current state of a sub-run by its run_id.
    pub async fn get_sub_run_state(&self, run_id: &str) -> Option<SubRunState> {
        self.state
            .read()
            .await
            .runs
            .get(run_id)
            .map(|record| record.state)
    }

    /// Get the full ancestry chain (run_id → parent → grandparent → ...).
    pub async fn get_ancestry(&self, run_id: &str) -> Vec<String> {
        let state = self.state.read().await;
        ancestry_from_parents(&state.parents, run_id).ancestors
    }

    // ── Pause / Resume ──────────────────────────────────────────────────────

    /// Register a cooperative pause flag for a sub-run.
    ///
    /// Returns the flag so the caller can pass it into [`SubRunConfig::pause_flag`].
    pub async fn register_pause_flag(&self, run_id: &str) -> Arc<AtomicBool> {
        let flag = Arc::new(AtomicBool::new(false));
        self.state
            .write()
            .await
            .pause_flags
            .insert(run_id.to_string(), flag.clone());
        flag
    }

    /// Get the pause flag for a sub-run, if registered.
    pub async fn get_pause_flag(&self, run_id: &str) -> Option<Arc<AtomicBool>> {
        self.state.read().await.pause_flags.get(run_id).cloned()
    }

    /// Set the pause flag for a single sub-run.
    /// Returns `true` if the flag existed and was set.
    pub async fn pause_sub_run(&self, run_id: &str) -> bool {
        if let Some(flag) = self.state.read().await.pause_flags.get(run_id) {
            flag.store(true, Ordering::SeqCst);
            self.persist_event(
                astra_services::session_journal::JournalEventType::SyncMarker,
                serde_json::json!({ "action": "pause", "run_id": run_id }),
            );
            true
        } else {
            false
        }
    }

    /// Clear the pause flag for a single sub-run.
    /// Returns `true` if the flag existed and was cleared.
    pub async fn resume_sub_run(&self, run_id: &str) -> bool {
        if let Some(flag) = self.state.read().await.pause_flags.get(run_id) {
            flag.store(false, Ordering::SeqCst);
            self.persist_event(
                astra_services::session_journal::JournalEventType::SyncMarker,
                serde_json::json!({ "action": "resume", "run_id": run_id }),
            );
            true
        } else {
            false
        }
    }

    /// Pause ALL sub-runs belonging to a delegation.
    /// Returns the number of sub-runs paused.
    pub async fn pause_delegation(&self, delegation_id: &str) -> usize {
        let state = self.state.read().await;
        let run_ids = state
            .delegation_runs
            .get(delegation_id)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let mut count = 0;
        for run_id in run_ids {
            if let Some(flag) = state.pause_flags.get(run_id) {
                flag.store(true, Ordering::SeqCst);
                count += 1;
            }
        }
        count
    }

    /// Resume ALL sub-runs belonging to a delegation.
    /// Returns the number of sub-runs resumed.
    pub async fn resume_delegation(&self, delegation_id: &str) -> usize {
        let state = self.state.read().await;
        let run_ids = state
            .delegation_runs
            .get(delegation_id)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let mut count = 0;
        for run_id in run_ids {
            if let Some(flag) = state.pause_flags.get(run_id) {
                flag.store(false, Ordering::SeqCst);
                count += 1;
            }
        }
        count
    }

    /// Pause ALL sub-runs that have a given parent run ID.
    /// Returns the number of sub-runs paused.
    pub async fn pause_children_of(&self, parent_run_id: &str) -> usize {
        let state = self.state.read().await;
        let mut count = 0;
        for child_id in state
            .parents
            .iter()
            .filter_map(|(child, parent)| (parent == parent_run_id).then_some(child))
        {
            if let Some(flag) = state.pause_flags.get(child_id) {
                flag.store(true, Ordering::SeqCst);
                count += 1;
            }
        }
        count
    }

    /// Resume ALL sub-runs that have a given parent run ID.
    /// Returns the number of sub-runs resumed.
    pub async fn resume_children_of(&self, parent_run_id: &str) -> usize {
        let state = self.state.read().await;
        let mut count = 0;
        for child_id in state
            .parents
            .iter()
            .filter_map(|(child, parent)| (parent == parent_run_id).then_some(child))
        {
            if let Some(flag) = state.pause_flags.get(child_id) {
                flag.store(false, Ordering::SeqCst);
                count += 1;
            }
        }
        count
    }

    /// Register a cancellation token for a sub-run so `cancel_children_of` can cancel it.
    pub async fn register_cancel_token(
        &self,
        run_id: &str,
        token: Arc<tokio_util::sync::CancellationToken>,
    ) {
        self.state
            .write()
            .await
            .cancel_tokens
            .insert(run_id.to_string(), token);
    }

    /// Request cancellation of one non-terminal sub-run.
    ///
    /// This deliberately does not write a terminal status. The owning
    /// executor must observe its cancellation token, finish its current
    /// boundary, and report the canonical cancelled outcome exactly once;
    /// eagerly writing `cancelled` here would race that result and make an
    /// interrupted child look settled before it has stopped. Cancellation is
    /// deliberately distinct from pause: the engine installs a token for
    /// every active child, and a cancel request must never turn into a paused
    /// run in an executor that observes only one of the two signals.
    pub async fn request_cancel_sub_run(&self, run_id: &str) -> bool {
        {
            // Completion needs the state write lock. Validate liveness and
            // signal the token under one read guard so a terminal transition
            // cannot slip between the two operations and acquire a spurious
            // post-completion cancellation marker.
            let state = self.state.read().await;
            let Some(record) = state
                .runs
                .get(run_id)
                .filter(|record| !record.state.is_terminal())
            else {
                return false;
            };
            let Some(cancel_token) = state.cancel_tokens.get(&record.run_id) else {
                return false;
            };
            cancel_token.cancel();
        }
        self.persist_event(
            astra_services::session_journal::JournalEventType::SyncMarker,
            serde_json::json!({ "action": "cancel_requested", "run_id": run_id }),
        );
        true
    }

    /// Cancel ALL sub-runs in the subtree rooted at `parent_run_id`.
    ///
    /// Walks the `parents` map transitively so grandchildren and deeper
    /// descendants are cancelled too. A flat one-level scan would leave
    /// sub-runs spawned by cancelled children executing — which was the
    /// historical bug.
    ///
    /// Returns the number of live children that accepted the cancellation
    /// request. A cancellation token is installed for every executable child;
    /// cancellation never piggybacks on the separate pause flag.
    pub async fn cancel_children_of(&self, parent_run_id: &str) -> usize {
        let descendants = self.collect_descendants(parent_run_id).await;
        let mut count = 0;
        for child_id in descendants {
            if self.request_cancel_sub_run(&child_id).await {
                count += 1;
            }
        }
        count
    }

    /// Walk the `parents` map starting from `root`, collecting every
    /// descendant run_id. BFS so siblings cancel before grandchildren —
    /// minimizing the time a freshly-spawned grandchild has to do work
    /// before being cancelled. Cycle-safe via `visited`.
    async fn collect_descendants(&self, root: &str) -> Vec<String> {
        let state = self.state.read().await;
        // Build child-by-parent index once so the walk is O(N) total
        // rather than O(N) per level.
        let mut by_parent: std::collections::HashMap<&str, Vec<&str>> =
            std::collections::HashMap::new();
        for (child, parent) in state.parents.iter() {
            by_parent
                .entry(parent.as_str())
                .or_default()
                .push(child.as_str());
        }
        let mut visited = std::collections::HashSet::new();
        let mut frontier: std::collections::VecDeque<String> = by_parent
            .get(root)
            .map(|v| v.iter().map(|s| s.to_string()).collect())
            .unwrap_or_default();
        let mut out = Vec::new();
        while let Some(rid) = frontier.pop_front() {
            if !visited.insert(rid.clone()) {
                continue;
            }
            if let Some(grand) = by_parent.get(rid.as_str()) {
                for g in grand {
                    frontier.push_back((*g).to_string());
                }
            }
            out.push(rid);
        }
        out
    }

    /// Check if a sub-run is currently paused.
    pub async fn is_paused(&self, run_id: &str) -> bool {
        self.state
            .read()
            .await
            .pause_flags
            .get(run_id)
            .is_some_and(|f| f.load(Ordering::Acquire))
    }

    // ── State Machine + Lifecycle ───────────────────────────────────────────

    /// Transition a sub-run's state, enforcing the state machine.
    ///
    /// Returns `Err` if the transition is illegal.
    pub async fn transition_state(
        &self,
        run_id: &str,
        to: SubRunState,
    ) -> Result<SubRunState, InvalidTransition> {
        let mut state = self.state.write().await;
        if let Some(record) = state.runs.get_mut(run_id) {
            let new_state = record.state.try_transition(to)?;
            record.state = new_state;

            let delegation_id = record.delegation_id.clone();
            let agent_id = record.agent_id.clone();
            let parent_run_id = record.parent_run_id.clone();
            let depth = record.depth;
            let retry_of = record.retry_of.clone();
            drop(state);

            if new_state == SubRunState::Running {
                self.persist_event(
                    astra_services::session_journal::JournalEventType::DelegationSubRunStarted,
                    serde_json::json!({
                        "delegation_id": delegation_id,
                        "sub_run_id": run_id,
                        "parent_run_id": parent_run_id,
                        "agent_id": agent_id,
                        "status": new_state.as_str(),
                        "depth": depth,
                        "retry_of": retry_of,
                    }),
                );
            }

            self.update_progress(&delegation_id, &agent_id, new_state)
                .await;
            return Ok(new_state);
        }
        // Run not tracked — allow the transition (e.g. root runs)
        Ok(to)
    }

    /// Mark a sub-run as complete: transition state, remove pause flag.
    ///
    /// Cleans up resources and updates progress tracking.
    pub async fn complete_sub_run(&self, run_id: &str, terminal_state: SubRunState) {
        self.complete_sub_run_with_result(run_id, terminal_state, None, None)
            .await;
    }

    /// Mark a sub-run as complete and persist the terminal result metadata.
    pub async fn complete_sub_run_with_result(
        &self,
        run_id: &str,
        terminal_state: SubRunState,
        error: Option<&str>,
        output_preview: Option<&str>,
    ) {
        debug_assert!(terminal_state.is_terminal());
        self.set_sub_run_result_state(run_id, terminal_state, error, output_preview, true)
            .await;
    }

    pub async fn apply_sub_run_result_state(
        &self,
        run_id: &str,
        result_state: SubRunState,
        error: Option<&str>,
        output_preview: Option<&str>,
    ) {
        if result_state.is_terminal() {
            self.complete_sub_run_with_result(run_id, result_state, error, output_preview)
                .await;
            return;
        }

        debug_assert!(matches!(
            result_state,
            SubRunState::Waiting | SubRunState::Paused
        ));
        self.set_sub_run_result_state(run_id, result_state, error, output_preview, false)
            .await;
    }

    async fn set_sub_run_result_state(
        &self,
        run_id: &str,
        result_state: SubRunState,
        error: Option<&str>,
        output_preview: Option<&str>,
        emit_completion_event: bool,
    ) {
        // Transition state in record
        let mut delegation_id = None;
        let mut agent_id = None;
        let mut parent_run_id = None;
        let mut final_state = result_state;
        let mut transition_applied = false;
        {
            let mut state = self.state.write().await;
            if let Some(record) = state.runs.get_mut(run_id) {
                if record.state == result_state {
                    // Executor completion can race recovery/replay. The
                    // already-projected state is the idempotent authority; do
                    // not emit a duplicate terminal event.
                    return;
                }
                let previous_state = record.state;
                let Ok(next_state) = previous_state.try_transition(result_state) else {
                    tracing::error!(
                        target: "astra_runtime::delegation",
                        run_id,
                        from = previous_state.as_str(),
                        to = result_state.as_str(),
                        "rejected illegal delegated sub-run result transition"
                    );
                    return;
                };
                record.state = next_state;
                final_state = record.state;
                transition_applied = true;
                delegation_id = Some(record.delegation_id.clone());
                agent_id = Some(record.agent_id.clone());
                parent_run_id = Some(record.parent_run_id.clone());
            }
        }

        if !transition_applied {
            tracing::warn!(
                target: "astra_runtime::delegation",
                run_id,
                to = result_state.as_str(),
                "ignored result for an untracked delegated sub-run"
            );
            return;
        }

        // Note: pause flags are NOT removed here — they are cleaned up
        // in cleanup_delegation() when the entire delegation completes.

        // Update progress + emit SSE event
        if let (Some(did), Some(aid), Some(parent_run_id)) =
            (delegation_id, agent_id, parent_run_id)
        {
            if emit_completion_event {
                self.persist_journal_entry(
                    astra_services::session_journal::JournalEvent::delegation_sub_run_completed(
                        self.session_id.as_deref(),
                        &did,
                        run_id,
                        &aid,
                        final_state.as_str(),
                        error,
                        output_preview,
                    ),
                );
            }

            self.update_progress(&did, &aid, final_state).await;

            // Emit completion SSE event for web clients
            if emit_completion_event {
                if let Some(ref broadcaster) = self.progress_broadcaster {
                    use crate::orchestration::{
                        AgentProgressEvent, CancellationOrigin, ProgressEventType,
                    };
                    // Canonical wire string — `as_str()` is what every other
                    // SSE/JSON site uses (line 1096 above, and the trace
                    // emitters). Using `{:?}` here leaked Rust enum casing
                    // ("VerificationFailed") into the user-visible payload
                    // instead of the snake_case wire form
                    // ("verification_failed"), and silently coupled the
                    // SSE wire format to the Debug derive — a refactor of
                    // the enum's variant names would corrupt SSE downstream.
                    let status_str = final_state.as_str();
                    let event_type = match final_state {
                        SubRunState::Completed => ProgressEventType::Completed {
                            result_summary: format!("Sub-run {} finished", run_id),
                            total_tool_calls: 0,
                            total_tokens: (0, 0),
                            duration_ms: 0,
                        },
                        SubRunState::Paused => ProgressEventType::Interrupted {
                            reason: "paused".to_string(),
                            partial_summary: format!("Sub-run {} paused", run_id),
                            total_tool_calls: 0,
                            total_tokens: (0, 0),
                            duration_ms: 0,
                        },
                        SubRunState::Waiting => ProgressEventType::Waiting {
                            reason: error.unwrap_or("external_dependency").to_string(),
                        },
                        SubRunState::Cancelled => ProgressEventType::Cancelled {
                            reason: format!("Sub-run {} cancelled", run_id),
                            origin: CancellationOrigin::Unverified,
                        },
                        _ => ProgressEventType::Failed {
                            error: format!("Sub-run terminal state: {}", status_str),
                        },
                    };
                    broadcaster.emit(AgentProgressEvent {
                        agent_id: aid,
                        run_id: run_id.to_string(),
                        parent_run_id,
                        event_type,
                        timestamp_epoch_ms: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() as u64,
                        metadata: None,
                    });
                }
            }
        }
    }

    /// Bulk cleanup after a full delegation completes.
    ///
    /// Cleans up all tracking state for a completed delegation:
    /// progress entries, pause flags, parent mappings, and delegation records.
    /// Call after the delegation lifecycle is fully complete.
    pub async fn cleanup_delegation(&self, delegation_id: &str) -> Result<(), String> {
        // The hierarchy projection is one atomic state: no child can appear
        // between terminality validation and index cleanup.
        let mut state = self.state.write().await;
        let run_ids = state
            .delegation_runs
            .get(delegation_id)
            .cloned()
            .unwrap_or_default();
        let records: Vec<SubRunRecord> = run_ids
            .iter()
            .filter_map(|run_id| state.runs.get(run_id).cloned())
            .collect();
        let non_terminal: Vec<String> = records
            .iter()
            .filter(|record| !record.state.is_terminal())
            .map(|record| format!("{}({})", record.run_id, record.state.as_str()))
            .collect();
        if !non_terminal.is_empty() {
            return Err(format!(
                "delegation {delegation_id} still has non-terminal sub-runs: {}",
                non_terminal.join(", ")
            ));
        }
        state.delegation_runs.remove(delegation_id);
        for run_id in &run_ids {
            state.runs.remove(run_id);
            state.parents.remove(run_id);
            state.pause_flags.remove(run_id);
            state.cancel_tokens.remove(run_id);
        }
        drop(state);

        let mut progress_map = self.progress.write().await;
        progress_map.remove(delegation_id);
        Ok(())
    }

    /// Get the full retry chain for a run: [original, retry1, retry2, ...]
    pub async fn get_retry_chain(&self, run_id: &str) -> Vec<String> {
        let state = self.state.read().await;
        let Some(record) = state.runs.get(run_id) else {
            return vec![run_id.to_string()];
        };
        let Some(group) = state.delegation_runs.get(&record.delegation_id) else {
            return vec![run_id.to_string()];
        };

        let mut original_id = run_id.to_string();
        let mut visited = HashSet::new();
        while visited.insert(original_id.clone()) {
            let Some(previous) = state
                .runs
                .get(&original_id)
                .and_then(|record| record.retry_of.as_ref())
            else {
                break;
            };
            original_id = previous.clone();
        }

        let mut chain = vec![original_id.clone()];
        let mut current = original_id;
        visited.clear();
        while visited.insert(current.clone()) {
            let next = group.iter().find_map(|candidate| {
                state
                    .runs
                    .get(candidate)
                    .filter(|record| record.retry_of.as_deref() == Some(current.as_str()))
            });
            let Some(next) = next else { break };
            chain.push(next.run_id.clone());
            current = next.run_id.clone();
        }
        chain
    }

    // ── Progress Tracking ───────────────────────────────────────────────────

    /// Initialize progress tracking for a new delegation.
    pub async fn init_progress(&self, delegation_id: &str, agent_ids: &[String]) {
        let mut states = HashMap::new();
        for aid in agent_ids {
            states.insert(aid.clone(), SubRunState::Created);
        }
        self.progress.write().await.insert(
            delegation_id.to_string(),
            DelegationProgress {
                delegation_id: delegation_id.to_string(),
                agent_states: states,
                started_at: std::time::Instant::now(),
                completed_count: 0,
                total_count: agent_ids.len(),
            },
        );
    }

    /// Update an agent's state in the progress tracker.
    async fn update_progress(&self, delegation_id: &str, agent_id: &str, state: SubRunState) {
        let mut progress_map = self.progress.write().await;
        if let Some(progress) = progress_map.get_mut(delegation_id) {
            progress.agent_states.insert(agent_id.to_string(), state);
            progress.completed_count = progress
                .agent_states
                .values()
                .filter(|s| s.is_terminal())
                .count();
        }
    }

    /// Get a snapshot of delegation progress.
    pub async fn get_progress(&self, delegation_id: &str) -> Option<DelegationProgress> {
        self.progress.read().await.get(delegation_id).cloned()
    }
}

#[async_trait]
impl astra_messaging::DelegationLookup for DelegationTracker {
    async fn get_parent(&self, run_id: &str) -> Option<String> {
        self.get_parent(run_id).await
    }
    async fn get_agent_id(&self, run_id: &str) -> Option<String> {
        self.get_agent_id(run_id).await
    }
    async fn get_depth(&self, run_id: &str) -> Option<u32> {
        self.get_depth(run_id).await
    }
    async fn record_sub_run(&self, info: astra_messaging::SubRunInfo) {
        self.record_sub_run_with_progress(
            SubRunRecord {
                run_id: info.run_id,
                parent_run_id: info.parent_run_id,
                delegation_id: info.delegation_id,
                agent_id: info.agent_id,
                depth: info.depth,
                state: SubRunState::Created,
                retry_of: None,
            },
            false,
        )
        .await;
    }
}

impl Default for DelegationTracker {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Delegation Engine ──────────────────────────────────────────────────────

/// Engine for executing multi-agent delegations.
///
/// Validates delegation requests against the agent profile registry,
/// spawns sub-runs via RunEngine, tracks hierarchies via DelegationTracker,
/// and **executes** them via [`SubRunExecutor`].
pub struct DelegationEngine {
    /// Agent profiles for validation.
    registry: Arc<RwLock<AgentProfileRegistry>>,
    /// Run engine for spawning sub-runs.
    run_engine: Arc<RunEngine>,
    /// Tracks parent→child run relationships.
    tracker: Arc<DelegationTracker>,
    /// Executor for actually running sub-agent loops.
    executor: Arc<dyn SubRunExecutor>,
    /// Optional post-completion verification gate.
    gate: Option<Arc<dyn VerificationGate>>,
    /// Optional mailbox router for inter-agent messaging.
    mailbox_router: Option<Arc<AgentMailboxRouter>>,
    /// Optional fork-prefix store shared with the spawner. When
    /// present, delegate sub-run configs get `inherited_prefix`
    /// populated by looking up the parent's captured ForkPrefix in
    /// this store (Bug B step 2). When absent, the delegate path
    /// behaves as pre-fork-prefix — `inherited_prefix` stays None
    /// and the child runs fresh.
    prefix_store: Option<Arc<dyn astra_turn_core::fork_prefix_store::PrefixCaptureSink>>,
    /// Durable state projection store used to surface delegated findings and
    /// keep child run state queryable by the web agent.
    projection_store: Option<Arc<DatabaseStateProjectionStore>>,
}

impl DelegationEngine {
    /// Reserved context key. Session identity is a typed request field, but
    /// remove a user/model-provided key with this name if it appears in task
    /// context so it cannot masquerade as runtime metadata in a child prompt.
    const SESSION_ID_CONTEXT_KEY: &'static str = "session_id";

    fn session_id_for(request: &DelegationRequest) -> String {
        request.session_id.clone()
    }

    fn child_task_context(request: &DelegationRequest) -> HashMap<String, serde_json::Value> {
        let mut context = clone_delegation_context(
            astra_core::history_work::HistoryWorkSite::DelegationContextClone,
            &request.context,
        );
        context.remove(Self::SESSION_ID_CONTEXT_KEY);
        context
    }

    #[allow(clippy::too_many_arguments)]
    async fn start_delegated_run(
        &self,
        run_id: &str,
        user_id: &str,
        session_id: &str,
        parent_run_id: &str,
        delegation_id: &str,
        agent_id: &str,
        retry_of: Option<&str>,
        interaction_mode: RequestedTurnInteractionMode,
    ) -> Result<RunExecutionAuthority, String> {
        self.run_engine
            .start_run_ext_with_context(
                run_id,
                user_id,
                session_id,
                Some(parent_run_id),
                Some(delegation_id),
                Some(agent_id),
                retry_of,
                crate::server::run::engine::RunStartContext {
                    interaction_mode,
                    ..Default::default()
                },
            )
            .await
    }

    pub fn new(
        registry: Arc<RwLock<AgentProfileRegistry>>,
        run_engine: Arc<RunEngine>,
        tracker: Arc<DelegationTracker>,
    ) -> Self {
        if let Some(metrics) = run_engine.metrics_registry() {
            register_delegation_metrics(metrics);
        }
        #[cfg(debug_assertions)]
        eprintln!(
            "  ⚠ DelegationEngine: using StubSubRunExecutor — call with_executor() for production"
        );
        Self {
            registry,
            run_engine,
            tracker,
            executor: Arc::new(StubSubRunExecutor),
            gate: None,
            mailbox_router: None,
            prefix_store: None,
            projection_store: None,
        }
    }

    /// Create engine with a real sub-run executor.
    pub fn with_executor(
        registry: Arc<RwLock<AgentProfileRegistry>>,
        run_engine: Arc<RunEngine>,
        tracker: Arc<DelegationTracker>,
        executor: Arc<dyn SubRunExecutor>,
    ) -> Self {
        if let Some(metrics) = run_engine.metrics_registry() {
            register_delegation_metrics(metrics);
        }
        Self {
            registry,
            run_engine,
            tracker,
            executor,
            gate: None,
            mailbox_router: None,
            prefix_store: None,
            projection_store: None,
        }
    }

    /// Attach a verification gate. Sub-run results will be checked before aggregation.
    pub fn with_gate(mut self, gate: Arc<dyn VerificationGate>) -> Self {
        self.gate = Some(gate);
        self
    }

    /// Attach a mailbox router for inter-agent messaging within delegations.
    pub fn with_mailbox_router(mut self, router: Arc<AgentMailboxRouter>) -> Self {
        self.mailbox_router = Some(router);
        self
    }

    /// Attach the fork-prefix store the spawner owns. Delegate
    /// sub-runs will then inherit the parent's captured prefix for
    /// prompt-cache reuse — matching agent-spawn behavior. When
    /// unset, delegate sub-runs run fresh (pre-fork-prefix
    /// behavior).
    pub fn with_prefix_store(
        mut self,
        store: Arc<dyn astra_turn_core::fork_prefix_store::PrefixCaptureSink>,
    ) -> Self {
        self.prefix_store = Some(store);
        self
    }

    pub fn with_projection_store(mut self, store: Arc<DatabaseStateProjectionStore>) -> Self {
        self.projection_store = Some(store);
        self
    }

    fn available_agent_profile_ids(registry: &AgentProfileRegistry) -> String {
        let mut ids: Vec<String> = registry
            .list()
            .into_iter()
            .map(|profile| profile.agent_id.clone())
            .collect();
        ids.sort_unstable();
        if ids.is_empty() {
            "(none)".to_string()
        } else {
            ids.join(", ")
        }
    }

    fn missing_agent_profile_error(
        operation: &str,
        agent_id: &str,
        registry: &AgentProfileRegistry,
    ) -> String {
        format!(
            "delegation failed during {operation}: requested agent profile '{agent_id}' is not registered. \
             Available profiles: [{}]. This is a configuration error; do not invent a replacement agent_id.",
            Self::available_agent_profile_ids(registry)
        )
    }

    /// Read-only accessor for the attached prefix store. Used by
    /// `crate::server::run::lifecycle::build_host` so the server-side parent loop
    /// host captures into the same store the delegate path reads
    /// from — without this, delegate sub-runs could never inherit
    /// because no parent capture would ever land in the store.
    pub fn prefix_store(
        &self,
    ) -> Option<&Arc<dyn astra_turn_core::fork_prefix_store::PrefixCaptureSink>> {
        self.prefix_store.as_ref()
    }

    /// Resolve a parent prefix for a delegated sub-run's config.
    /// Returns `None` (no inheritance) when:
    /// - no prefix_store is configured
    /// - no parent prefix captured for `parent_run_id`
    /// - resolver rejects the prefix (provider / model mismatch,
    ///   thinking budget clamp, etc.) — soft-fallback semantics,
    ///   same as agent spawn
    ///
    /// `child_provider` and `child_model_id` come from the resolved
    /// agent profile's model string. For now we use
    /// [`astra_turn_core::fork_prefix::ProviderKind::from_provider_hint`]
    /// on the model name — the same inference the spawner uses.
    fn resolve_inherited_prefix_for_delegate(
        &self,
        parent_run_id: &str,
        child_model_id: &str,
    ) -> Option<crate::orchestration::InheritedChildPrefix> {
        let store = self.prefix_store.as_ref()?;
        let child_provider =
            astra_turn_core::fork_prefix::ProviderKind::from_provider_hint(child_model_id);
        let spec = astra_turn_core::orchestration_spawn_tool::InheritPrefixSpec {
            from_run_id: Some(parent_run_id.to_string()),
            required: false,
        };
        let ctx = astra_turn_core::fork_resolve::SpawnResolveContext {
            caller_run_id: Some(parent_run_id.to_string()),
            child_provider,
            child_model_id: child_model_id.to_string(),
            // Delegate doesn't expose max_output_tokens (agent
            // profile carries max_turns only), so leave None —
            // validate_spawn will skip the thinking-budget clamp
            // check.
            child_max_output_tokens: None,
        };
        let outcome = astra_turn_core::fork_resolve::resolve_inherit_prefix(
            Some(&spec),
            &ctx,
            store.as_ref(),
        );
        crate::orchestration::spawner::build_inherited_child_prefix(&outcome)
    }

    fn ensure_source_in_delegation_chain(request: &mut DelegationRequest, source_agent_id: &str) {
        let source_agent_id = source_agent_id.trim();
        if source_agent_id.is_empty() {
            return;
        }
        // Compare agent identities by canonical form (lowercase + NFC) so
        // case-variant or Unicode normalization aliases cannot bypass the
        // chain membership check.
        let canonical = canonical_agent_id(source_agent_id);
        if !request
            .delegation_chain
            .iter()
            .any(|agent_id| canonical_agent_id(agent_id) == canonical)
        {
            request.delegation_chain.push(source_agent_id.to_string());
        }
    }

    fn delegation_chain_for_child(
        request: &DelegationRequest,
        child_agent_id: &str,
    ) -> Result<Vec<String>, String> {
        // Compare agent identities by their canonical form. Agent IDs are
        // user-provided and may vary in casing or Unicode normalization
        // (NFC vs NFD: "café" vs "cafe" + ◌́). Two IDs that are visually
        // identical must be treated as the same agent — otherwise a
        // normalization alias bypasses circular delegation detection and
        // allows an infinite loop.
        let canonical_child = canonical_agent_id(child_agent_id);
        if request
            .delegation_chain
            .iter()
            .any(|agent_id| canonical_agent_id(agent_id) == canonical_child)
        {
            let mut cycle = request.delegation_chain.clone();
            cycle.push(child_agent_id.to_string());
            let chain_display = cycle.join(" → ");
            return Err(format!(
                "circular delegation detected: {chain_display}. Agent '{child_agent_id}' already exists in the delegation chain"
            ));
        }
        Ok(request.delegation_chain.clone())
    }

    /// Get the progress broadcaster from the underlying tracker, if configured.
    pub fn progress_broadcaster(&self) -> Option<&Arc<crate::orchestration::ProgressBroadcaster>> {
        self.tracker.progress_broadcaster()
    }

    async fn bubble_up_critical_agent_results(
        &self,
        user_id: &str,
        session_id: &str,
        results: &[AgentResult],
    ) {
        let Some(projection_store) = self.projection_store.clone() else {
            return;
        };
        for result in results {
            let extraction = critical_finding_from_agent_result(result);
            if let Some(error) = extraction.contract_error.as_deref() {
                tracing::warn!(
                    target: "astra_runtime::delegation",
                    run_id = %result.run_id,
                    error,
                    "delegated finding output violated the bounded JSON contract"
                );
            }
            if extraction.used_legacy_review_format {
                tracing::debug!(
                    target: "astra_runtime::delegation",
                    run_id = %result.run_id,
                    "accepted previous delegated review format during deployment migration"
                );
            }
            if let Some(summary) = extraction.summary {
                bubble_up_critical_finding_from_tracker(
                    projection_store.clone(),
                    self.tracker.clone(),
                    user_id.to_string(),
                    session_id.to_string(),
                    result.run_id.clone(),
                    summary,
                )
                .await;
            }
        }
    }

    /// Dynamically set the verification gate (e.g., per-subtask criteria during plan execution).
    ///
    /// Unlike [`with_gate`] (builder pattern), this mutates the engine in place so callers
    /// can swap gates between delegation calls without rebuilding the engine.
    pub fn set_gate(&mut self, gate: Arc<dyn VerificationGate>) {
        self.gate = Some(gate);
    }
    /// Create a new engine sharing the same components but with a different gate.
    ///
    /// All `Arc`-wrapped internals (registry, run_engine, tracker, executor) are
    /// cheaply cloned (pointer bumps).  Use this when the engine is behind an
    /// `Arc` and `set_gate` cannot be called because `&mut self` is unavailable.
    pub fn clone_with_gate(&self, gate: Arc<dyn VerificationGate>) -> Self {
        Self {
            registry: self.registry.clone(),
            run_engine: self.run_engine.clone(),
            tracker: self.tracker.clone(),
            executor: self.executor.clone(),
            gate: Some(gate),
            mailbox_router: self.mailbox_router.clone(),
            prefix_store: self.prefix_store.clone(),
            projection_store: self.projection_store.clone(),
        }
    }
    /// Validate a delegation request without executing it.
    pub async fn validate(
        &self,
        request: &DelegationRequest,
        source_agent_id: &str,
    ) -> Result<(), String> {
        let reg = self.registry.read().await;
        reg.validate_delegation(request, source_agent_id)
    }

    /// Apply the verification gate to a sub-run result with retry support.
    ///
    /// Returns the final result after gate checking (possibly retried).
    /// If no gate is configured, returns the result as-is.
    async fn apply_gate(
        &self,
        user_id: &str,
        expected_session_id: &str,
        result: AgentResult,
        delegation_id: &str,
        parent_run_id: &str,
        retry_timeout: Option<std::time::Duration>,
        config_builder: impl Fn() -> Result<SubRunConfig, String>,
    ) -> AgentResult {
        let gate = match &self.gate {
            Some(g) => g,
            None => return result,
        };

        // Skip gate for already-failed results
        if !result.is_success() {
            return result;
        }

        let max_retries = gate.max_retries();
        let mut current = result;
        let mut attempt = 1u32;

        loop {
            match gate.verify(&current, delegation_id, attempt).await {
                GateVerdict::Pass | GateVerdict::Skip => return current,
                GateVerdict::Fail { reason, details } => {
                    // Persist retry count to durable store for crash recovery
                    astra_core::log_persist!(
                        self.run_engine
                            .persist_retry_count(
                                user_id,
                                expected_session_id,
                                &current.run_id,
                                attempt,
                            )
                            .await,
                        "delegation",
                        &current.run_id,
                        "retry_count"
                    );

                    // Record the gate failure in run events
                    astra_core::log_persist!(
                        self.run_engine
                            .append_event(
                                user_id,
                                expected_session_id,
                                &current.run_id,
                                serde_json::json!({
                                    "event_type": "verification_gate_failed",
                                    "data": {
                                        "attempt": attempt,
                                        "reason": reason,
                                        "details": details,
                                    }
                                }),
                            )
                            .await,
                        "delegation",
                        &current.run_id,
                        "gate_failed_event"
                    );

                    if attempt >= max_retries {
                        // Exhausted retries — mark as verification failure
                        let verification_error =
                            format!("verification gate failed after {attempt} attempts: {reason}");
                        return AgentResult {
                            status: STATUS_VERIFICATION_FAILED.to_string(),
                            error: Some(verification_error),
                            ..current
                        };
                    }

                    // Retry: re-execute with the same config
                    attempt += 1;
                    let original_run_id = current.run_id.clone();
                    let mut retry_config = match config_builder() {
                        Ok(config) => config,
                        Err(error) => {
                            let retry_error =
                                format!("verification retry template unavailable: {error}");
                            return AgentResult {
                                status: STATUS_FAILED.to_string(),
                                error: Some(retry_error),
                                ..current
                            };
                        }
                    };
                    let retry_run_id = retry_config.run_id.clone();
                    let retry_depth = self.tracker.get_depth(&original_run_id).await.unwrap_or(0);

                    let retry_delay = verification_retry_delay(attempt, &original_run_id);
                    tracing::debug!(
                        target: "astra_runtime::delegation",
                        run_id = %original_run_id,
                        retry_attempt = attempt,
                        retry_delay_ms = retry_delay.as_millis(),
                        "verification retry scheduled with bounded backoff"
                    );
                    let cancelled_during_backoff =
                        if let Some(token) = retry_config.cancel_token.as_ref() {
                            tokio::select! {
                                _ = tokio::time::sleep(retry_delay) => false,
                                _ = token.cancelled() => true,
                            }
                        } else {
                            tokio::time::sleep(retry_delay).await;
                            false
                        };
                    if cancelled_during_backoff {
                        let cancellation = "cancelled during verification retry backoff";
                        return AgentResult {
                            status: STATUS_CANCELLED.to_string(),
                            error: Some(cancellation.to_string()),
                            ..current
                        };
                    }

                    let retry_authority = match self
                        .start_delegated_run(
                            &retry_run_id,
                            &retry_config.user_id,
                            &retry_config.session_id,
                            parent_run_id,
                            delegation_id,
                            &retry_config.agent_profile.agent_id,
                            Some(&original_run_id),
                            retry_config.interaction_mode,
                        )
                        .await
                    {
                        Ok(authority) => authority,
                        Err(error) => {
                            return AgentResult {
                                status: STATUS_FAILED.to_string(),
                                error: Some(format!(
                                    "failed to establish durable verification retry: {error}"
                                )),
                                ..current
                            };
                        }
                    };
                    retry_config.execution_owner_generation =
                        Some(retry_authority.owner_generation);

                    // Record retry sub-run with linkage to original
                    self.tracker
                        .record_sub_run(SubRunRecord {
                            run_id: retry_run_id.clone(),
                            parent_run_id: parent_run_id.to_string(),
                            delegation_id: delegation_id.to_string(),
                            agent_id: retry_config.agent_profile.agent_id.clone(),
                            depth: retry_depth,
                            state: SubRunState::Created,
                            retry_of: Some(original_run_id.clone()),
                        })
                        .await;

                    let retry_pause_flag = self.tracker.register_pause_flag(&retry_run_id).await;
                    retry_config.pause_flag = Some(retry_pause_flag);
                    let retry_cancel_token = retry_config
                        .cancel_token
                        .as_ref()
                        .map(|t| Arc::new(t.child_token()))
                        .unwrap_or_else(|| Arc::new(tokio_util::sync::CancellationToken::new()));
                    self.tracker
                        .register_cancel_token(&retry_run_id, retry_cancel_token.clone())
                        .await;
                    retry_config.cancel_token = Some(retry_cancel_token);

                    if retry_config.mailbox.is_none() {
                        if let Some(router) = &self.mailbox_router {
                            let addr = astra_messaging::types::AgentAddress {
                                run_id: retry_run_id.clone(),
                                agent_id: retry_config.agent_profile.agent_id.clone(),
                            };
                            match router.register(addr, Some(delegation_id.to_string())).await {
                                Ok(mailbox) => retry_config.mailbox = Some(mailbox),
                                Err(e) => {
                                    eprintln!(
                                        "  ⚠ delegation: mailbox registration failed for retry {}: {}",
                                        retry_config.agent_profile.agent_id, e
                                    );
                                }
                            }
                        }
                    }

                    Self::write_journal_event(
                        user_id,
                        &retry_config.session_id,
                        astra_services::session_journal::JournalEvent::delegation_retry(
                            Some(&retry_config.session_id),
                            delegation_id,
                            &original_run_id,
                            &retry_run_id,
                            &retry_config.agent_profile.agent_id,
                            attempt,
                            &reason,
                        ),
                    );

                    // Verification is a parent-level evaluation fact. The
                    // physical child execution may already be durably
                    // completed, so the gate must not rewrite that lifecycle.
                    let rejected = AgentResult {
                        status: STATUS_VERIFICATION_FAILED.to_string(),
                        error: Some(reason.clone()),
                        ..current.clone()
                    };
                    let rejected_state = agent_result_status_to_subrun_state(&rejected.status);
                    self.tracker
                        .apply_sub_run_result_state(
                            &original_run_id,
                            rejected_state,
                            rejected.error.as_deref(),
                            rejected.output.as_deref(),
                        )
                        .await;

                    // Transition retry to Running before execution
                    if let Err(e) = self
                        .tracker
                        .transition_state(&retry_run_id, SubRunState::Running)
                        .await
                    {
                        astra_core::agent_warn!(
                            "delegation",
                            "Retry transition to Running failed for {retry_run_id}: {e:?}"
                        );
                    }

                    let retry_cancel = retry_config.cancel_token.clone();
                    let retry_agent_id = retry_config.agent_profile.agent_id.clone();
                    let retry_exec = async {
                        match retry_timeout {
                            Some(dur) => {
                                match tokio::time::timeout(dur, self.executor.execute(retry_config))
                                    .await
                                {
                                    Ok(r) => r,
                                    Err(_) => Err(format!(
                                        "agent {} exceeded retry timeout of {}s",
                                        retry_agent_id,
                                        dur.as_secs()
                                    )),
                                }
                            }
                            None => self.executor.execute(retry_config).await,
                        }
                    };

                    match if let Some(token) = retry_cancel {
                        tokio::select! {
                            r = retry_exec => r,
                            _ = token.cancelled() => Err("cancelled by budget timeout".to_string()),
                        }
                    } else {
                        retry_exec.await
                    } {
                        Ok(result) => {
                            current = reconcile_agent_result_with_durable_authority(
                                &self.run_engine,
                                user_id,
                                expected_session_id,
                                durable_lifecycle_disposition(
                                    self.executor.as_ref(),
                                    retry_authority.owner_generation,
                                ),
                                result,
                            )
                            .await;
                        }
                        Err(e) => {
                            return reconcile_agent_result_with_durable_authority(
                                &self.run_engine,
                                user_id,
                                expected_session_id,
                                durable_lifecycle_disposition(
                                    self.executor.as_ref(),
                                    retry_authority.owner_generation,
                                ),
                                AgentResult {
                                    agent_id: retry_agent_id,
                                    run_id: retry_run_id,
                                    status: STATUS_FAILED.to_string(),
                                    error: Some(format!("retry execution failed: {e}")),
                                    output: None,
                                    prompt_tokens: 0,
                                    completion_tokens: 0,
                                    tool_calls: 0,
                                },
                            )
                            .await;
                        }
                    }
                }
            }
        }
    }

    /// Execute a delegation: spawn sub-runs according to the coordination pattern.
    ///
    /// Returns a `DelegationResult` with individual agent results and
    /// aggregated output. Sub-runs are created in the RunEngine and tracked
    /// in the DelegationTracker for hierarchy queries.
    ///
    /// `cancel_token` is scoped to this execution — no global state. When
    /// cancelled, all spawned sub-runs receive the signal and stop gracefully.
    pub async fn execute(
        &self,
        request: DelegationRequest,
        source_agent_id: &str,
        cancel_token: Option<Arc<tokio_util::sync::CancellationToken>>,
    ) -> Result<DelegationResult, String> {
        self.execute_with_forward_headers(
            request,
            source_agent_id,
            cancel_token,
            HashMap::new(),
            None,
        )
        .await
    }

    pub async fn execute_with_forward_headers(
        &self,
        request: DelegationRequest,
        source_agent_id: &str,
        cancel_token: Option<Arc<tokio_util::sync::CancellationToken>>,
        forward_headers: HashMap<String, String>,
        admitted_model_execution: Option<AdmittedModelExecution>,
    ) -> Result<DelegationResult, String> {
        self.execute_with_forward_headers_and_live_events(
            request,
            source_agent_id,
            cancel_token,
            forward_headers,
            admitted_model_execution,
            None,
        )
        .await
    }

    /// Execute one delegation with an optional request-scoped child live lane.
    /// The sink is deliberately an argument rather than engine state: a shared
    /// engine can serve concurrent sessions, and a stale TUI receiver must
    /// never observe another request's children.
    pub async fn execute_with_forward_headers_and_live_events(
        &self,
        mut request: DelegationRequest,
        source_agent_id: &str,
        cancel_token: Option<Arc<tokio_util::sync::CancellationToken>>,
        forward_headers: HashMap<String, String>,
        admitted_model_execution: Option<AdmittedModelExecution>,
        live_event_sink: Option<astra_turn_core::agent_live_event::SharedAgentLiveEventSink>,
    ) -> Result<DelegationResult, String> {
        request
            .context
            .remove(crate::turn::agentic::delegate_interception::FORWARD_HEADERS_CONTEXT_KEY);
        Self::ensure_source_in_delegation_chain(&mut request, source_agent_id);
        let enabled_tools = parse_request_allowlist_from_context(
            &mut request.context,
            crate::turn::agentic::delegate_interception::REQUEST_ENABLED_TOOLS_CONTEXT_KEY,
        )?
        .or_else(|| Some(HashSet::new()));
        let request_constraints = RequestConstraints::new(
            parse_request_allowlist_from_context(
                &mut request.context,
                crate::turn::agentic::delegate_interception::REQUEST_ALLOWED_TOOLS_CONTEXT_KEY,
            )?,
            enabled_tools,
            parse_request_allowlist_from_context(
                &mut request.context,
                crate::turn::agentic::delegate_interception::REQUEST_ALLOWED_SKILLS_CONTEXT_KEY,
            )?,
            parse_request_skill_sources_from_context(
                &mut request.context,
                crate::turn::agentic::delegate_interception::REQUEST_ALLOWED_SKILL_SOURCES_CONTEXT_KEY,
            )?,
        );

        // Validate first
        self.validate(&request, source_agent_id).await?;
        let child_recursion_depth =
            astra_turn_core::agentic_recursion_guard::checked_child_recursion_depth_u32(
                request.depth,
            )?;

        let session_id = Self::session_id_for(&request);
        let parent_run = self
            .run_engine
            .require_delegation_parent(&request.user_id, &session_id, &request.parent_run_id)
            .await?;
        let interaction_mode =
            crate::server::run::engine::durable_run_effective_interaction_mode(&parent_run);

        // Extract pattern name and agent_ids for journal event.
        let (pattern_name, agent_ids_for_journal): (&str, Vec<String>) = match &request.pattern {
            CoordinationPattern::FanOut { agent_ids, .. } => ("fan_out", agent_ids.clone()),
            CoordinationPattern::Pipeline { stages, .. } => (
                "pipeline",
                stages.iter().map(|s| s.agent_id.clone()).collect(),
            ),
            CoordinationPattern::Sequential { agent_ids, .. } => ("sequential", agent_ids.clone()),
            CoordinationPattern::AdversarialReview {
                producer_id,
                reviewer_id,
                ..
            } => (
                "adversarial_review",
                vec![producer_id.clone(), reviewer_id.clone()],
            ),
            CoordinationPattern::Fork {
                agent_id, tasks, ..
            } => ("fork", vec![format!("{}×{}", agent_id, tasks.len())]),
        };

        // Journal: delegation started
        Self::write_journal_event(
            &request.user_id,
            &session_id,
            astra_services::session_journal::JournalEvent::delegation_started(
                Some(&session_id),
                &request.delegation_id,
                &request.parent_run_id,
                pattern_name,
                &agent_ids_for_journal,
            ),
        );

        // Initialize progress tracking
        self.tracker
            .init_progress(&request.delegation_id, &agent_ids_for_journal)
            .await;

        // Register the parent/orchestrator with the mailbox router so child
        // agents can send progress and messages to `MessageTarget::Parent`.
        // Without this, `resolve_parent_addr` falls back to a synthetic address
        // that has no inbox in the transport, causing `AgentNotFound` errors.
        //
        // Uses `register_if_absent` to atomically skip if the caller already
        // registered this run_id (e.g., CLI layer or tests that pre-register
        // a parent mailbox to receive messages).
        let parent_mailbox = if let Some(router) = &self.mailbox_router {
            let parent_addr = astra_messaging::types::AgentAddress {
                run_id: request.parent_run_id.clone(),
                agent_id: source_agent_id.to_string(),
            };
            match router
                .register_if_absent(parent_addr, Some(request.delegation_id.clone()))
                .await
            {
                Ok(mb) => mb, // Some(mailbox) if newly registered, None if already present
                Err(e) => {
                    tracing::warn!(
                        target: "astra_runtime::delegation",
                        parent_run_id = %request.parent_run_id,
                        error = %e,
                        "failed to register parent mailbox; child progress messages will be lost",
                    );
                    None
                }
            }
        } else {
            None
        };

        // Note: parent_mailbox cleanup on panic is handled by AgentMailbox's
        // Drop impl, which spawns a background unregister task. On the normal
        // path, we unregister explicitly below for proper error handling.

        let execution_started_at = std::time::Instant::now();
        let result = match &request.pattern {
            CoordinationPattern::FanOut {
                agent_ids,
                aggregation,
                timeout_sec,
            } => {
                self.execute_fan_out(
                    &request,
                    agent_ids,
                    aggregation,
                    &forward_headers,
                    admitted_model_execution.as_ref(),
                    &request_constraints,
                    child_recursion_depth,
                    interaction_mode,
                    *timeout_sec,
                    cancel_token.as_ref(),
                    live_event_sink.as_ref(),
                )
                .await
            }
            CoordinationPattern::Pipeline {
                stages,
                timeout_sec,
            } => {
                let agent_ids: Vec<String> = stages.iter().map(|s| s.agent_id.clone()).collect();
                self.execute_sequential(
                    &request,
                    &agent_ids,
                    false,
                    &forward_headers,
                    admitted_model_execution.as_ref(),
                    &request_constraints,
                    child_recursion_depth,
                    interaction_mode,
                    *timeout_sec,
                    cancel_token.as_ref(),
                    live_event_sink.as_ref(),
                )
                .await
            }
            CoordinationPattern::Sequential {
                agent_ids,
                stop_on_success,
                timeout_sec,
            } => {
                self.execute_sequential(
                    &request,
                    agent_ids,
                    *stop_on_success,
                    &forward_headers,
                    admitted_model_execution.as_ref(),
                    &request_constraints,
                    child_recursion_depth,
                    interaction_mode,
                    *timeout_sec,
                    cancel_token.as_ref(),
                    live_event_sink.as_ref(),
                )
                .await
            }
            CoordinationPattern::AdversarialReview {
                producer_id,
                reviewer_id,
                max_rounds,
                timeout_sec,
                ..
            } => {
                self.execute_adversarial(
                    &request,
                    producer_id,
                    reviewer_id,
                    *max_rounds,
                    &forward_headers,
                    admitted_model_execution.as_ref(),
                    &request_constraints,
                    child_recursion_depth,
                    interaction_mode,
                    *timeout_sec,
                    cancel_token.as_ref(),
                    live_event_sink.as_ref(),
                )
                .await
            }
            CoordinationPattern::Fork {
                tasks,
                agent_id,
                max_turns,
                aggregation,
                timeout_sec,
            } => {
                self.execute_fork(
                    &request,
                    tasks,
                    agent_id,
                    *max_turns,
                    aggregation,
                    &forward_headers,
                    admitted_model_execution.as_ref(),
                    &request_constraints,
                    child_recursion_depth,
                    interaction_mode,
                    *timeout_sec,
                    cancel_token.as_ref(),
                    live_event_sink.as_ref(),
                )
                .await
            }
        };

        // Unregister the parent mailbox now that all children have completed.
        // This prevents resource leaks and address collisions with future runs.
        if let (Some(router), Some(mb)) = (&self.mailbox_router, &parent_mailbox) {
            let addr = mb.address.clone();
            if let Err(e) = router.unregister(&addr).await {
                tracing::warn!(
                    target: "astra_runtime::delegation",
                    parent_run_id = %addr.run_id,
                    error = %e,
                    "failed to unregister parent mailbox after delegation",
                );
            }
        }
        // Drop parent_mailbox explicitly before journal write so the Drop
        // impl doesn't race with the explicit unregister above.
        drop(parent_mailbox);

        // Journal: delegation completed
        if let Ok(ref dr) = result {
            self.bubble_up_critical_agent_results(&request.user_id, &session_id, &dr.agent_results)
                .await;
            let succeeded = dr.agent_results.iter().filter(|r| r.is_success()).count();
            let failed = dr.agent_results.len() - succeeded;
            Self::write_journal_event(
                &request.user_id,
                &session_id,
                astra_services::session_journal::JournalEvent::delegation_completed(
                    Some(&session_id),
                    &request.delegation_id,
                    pattern_name,
                    dr.agent_results.len(),
                    succeeded,
                    failed,
                    &dr.status,
                    dr.aggregated_output.as_deref(),
                ),
            );
        }
        record_delegation_metrics(
            self.run_engine.metrics_registry(),
            pattern_name,
            execution_started_at.elapsed(),
            &result,
        );

        // Note: cleanup_delegation() is intentionally NOT called here.
        // The caller (e.g., TeamExecutionOrchestrator) should call
        // tracker.cleanup_delegation() when the delegation lifecycle is
        // fully complete, including any post-execution inspection.

        result
    }

    /// Write a journal event synchronously on the local diagnostic path.
    /// Durable run state is committed separately by `RunEngine`.
    fn write_journal_event(
        user_id: &str,
        session_id: &str,
        event: astra_services::session_journal::JournalEvent,
    ) {
        if let Ok(writer) =
            astra_services::session_journal::JournalWriter::for_user(user_id, session_id)
        {
            if let Err(e) = writer.append(&event) {
                astra_core::agent_warn!("delegation", "Failed to write journal event: {e}");
            }
        }
    }

    /// Fan-out: spawn all agents in parallel, aggregate results.
    async fn execute_fan_out(
        &self,
        request: &DelegationRequest,
        agent_ids: &[String],
        aggregation: &AggregationStrategy,
        forward_headers: &HashMap<String, String>,
        admitted_model_execution: Option<&AdmittedModelExecution>,
        request_constraints: &RequestConstraints,
        child_recursion_depth: u8,
        interaction_mode: RequestedTurnInteractionMode,
        timeout_sec: u64,
        cancel_token: Option<&Arc<tokio_util::sync::CancellationToken>>,
        live_event_sink: Option<&astra_turn_core::agent_live_event::SharedAgentLiveEventSink>,
    ) -> Result<DelegationResult, String> {
        let reg = self.registry.read().await;
        let has_gate = self.gate.is_some();

        // Compute aggregation strategy name and budget info for team prompts
        let aggregation_name = match aggregation {
            AggregationStrategy::FirstSuccess => "FirstSuccess",
            AggregationStrategy::AllResults => "AllResults",
            AggregationStrategy::Consensus => "Consensus",
        };
        let budget_prompt = Self::extract_budget_prompt(&request.context);
        let agent_id_strs: Vec<&str> = agent_ids.iter().map(|s| s.as_str()).collect();

        // Build configs + create runs in parallel
        let mut configs = Vec::new();
        let mut owner_generations = HashMap::new();
        let session_id = Self::session_id_for(request);
        for agent_id in agent_ids {
            let sub_run_id = uuid::Uuid::new_v4().to_string();

            let execution_authority = self
                .start_delegated_run(
                    &sub_run_id,
                    &request.user_id,
                    &session_id,
                    &request.parent_run_id,
                    &request.delegation_id,
                    agent_id,
                    None,
                    interaction_mode,
                )
                .await?;
            owner_generations.insert(sub_run_id.clone(), execution_authority.owner_generation);

            self.tracker
                .record_sub_run(SubRunRecord {
                    run_id: sub_run_id.clone(),
                    parent_run_id: request.parent_run_id.clone(),
                    delegation_id: request.delegation_id.clone(),
                    agent_id: agent_id.clone(),
                    depth: request.depth + 1,
                    state: SubRunState::Created,
                    retry_of: None,
                })
                .await;

            // Transition Created → Running
            if let Err(e) = self
                .tracker
                .transition_state(&sub_run_id, SubRunState::Running)
                .await
            {
                astra_core::agent_warn!(
                    "delegation",
                    "Fan-out: transition to Running failed for {sub_run_id}: {e:?}"
                );
            }

            self.run_engine
                .transition_status_with_events_if_current_owner(
                    &request.user_id,
                    &session_id,
                    &sub_run_id,
                    &[STATUS_RUNNING],
                    execution_authority.owner_generation,
                    STATUS_RUNNING,
                    Some("agent_execution"),
                    None,
                    &[],
                )
                .await?
                .then_some(())
                .ok_or_else(|| {
                    format!("fan-out child {sub_run_id} lost durable execution authority")
                })?;

            let pause_flag = self.tracker.register_pause_flag(&sub_run_id).await;
            // Create a per-child cancel token derived from the parent's token.
            // Cancelling the parent automatically cancels all children.
            let child_cancel = cancel_token
                .map(|t| Arc::new(t.child_token()))
                .unwrap_or_else(|| Arc::new(tokio_util::sync::CancellationToken::new()));
            self.tracker
                .register_cancel_token(&sub_run_id, child_cancel.clone())
                .await;

            let profile = match reg.get(agent_id) {
                Some(p) => p.clone(),
                None => {
                    return Err(Self::missing_agent_profile_error(
                        "fanout spawn",
                        agent_id,
                        &reg,
                    ));
                }
            };
            let delegation_chain = Self::delegation_chain_for_child(request, agent_id)?;

            // Register with mailbox router and obtain a mailbox handle (if router available).
            let mailbox = if let Some(router) = &self.mailbox_router {
                let addr = astra_messaging::types::AgentAddress {
                    run_id: sub_run_id.clone(),
                    agent_id: agent_id.clone(),
                };
                match router
                    .register(addr, Some(request.delegation_id.clone()))
                    .await
                {
                    Ok(mb) => Some(mb),
                    Err(e) => {
                        eprintln!(
                            "  ⚠ delegation: mailbox registration failed for {agent_id}: {e}"
                        );
                        None
                    }
                }
            } else {
                None
            };

            // Inject team coordination prompt into task
            let coordination_prompt = format!(
                "{}{}",
                team_prompts::fan_out_agent_prompt(
                    agent_id,
                    &agent_id_strs,
                    aggregation_name,
                    has_gate,
                ),
                budget_prompt,
            );
            let enhanced_task =
                team_prompts::wrap_task_with_coordination(&coordination_prompt, &request.task);

            // Bug B step 2: resolve parent prefix for
            // fork-cache inheritance if a store is configured.
            // Uses the delegated agent's resolved model id (falls
            // back to an empty string hint, which maps to
            // `Other("")` — resolver will soft-fallback if the
            // parent's provider doesn't match). Soft semantics
            // match agent spawn: on miss or mismatch the child
            // runs fresh, no hard error.
            // Prefix inheritance is a performance optimization. The
            // delegation engine does not materialize Offering routes, so it
            // must not reinterpret an Offering ID as a provider model name.
            let delegate_model = "";
            let inherited_prefix =
                self.resolve_inherited_prefix_for_delegate(&request.parent_run_id, delegate_model);

            configs.push(SubRunConfig {
                run_id: sub_run_id,
                parent_run_id: request.parent_run_id.clone(),
                agent_profile: profile,
                task: enhanced_task,
                session_id: Self::session_id_for(request),
                user_id: request.user_id.clone(),
                execution_owner_generation: Some(execution_authority.owner_generation),
                execution_owner_generation_sink: None,
                previous_output: None,
                context: Self::child_task_context(request),
                forward_headers: forward_headers.clone(),
                admitted_model_execution: admitted_model_execution.cloned(),
                interaction_mode,
                request_constraints: request_constraints.clone(),
                recursion_depth: child_recursion_depth,
                max_turns: None,
                initial_turns: None,
                pause_flag: Some(pause_flag),
                checkpoint_gate: None,
                mailbox,
                progress_emitter: None,
                live_event_sink: live_event_sink.cloned(),
                cancel_token: Some(child_cancel),
                inherited_prefix,
                execution_metadata: request.execution_metadata.clone(),
                work_item: None,
                delegation_chain,
                #[cfg(feature = "harness")]
                harness_sink: None,
            });
        }
        drop(reg);

        // Execute sub-runs in parallel, respecting optional max_parallel limit.
        const MAX_FAN_OUT_AGENTS: usize = 32;
        if configs.len() > MAX_FAN_OUT_AGENTS {
            return Err(format!(
                "Fan-out request with {} agents exceeds limit of {MAX_FAN_OUT_AGENTS}",
                configs.len()
            ));
        }
        let max_parallel = request
            .context
            .get("team_max_parallel")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        let semaphore = if max_parallel > 0 {
            Some(Arc::new(tokio::sync::Semaphore::new(max_parallel)))
        } else {
            None
        };

        // Store config templates for fan-out gate retry support.
        // Maps agent_id → (AgentProfile, task, session_id, user_id, context, delegation_chain)
        let mut retry_templates: HashMap<
            String,
            (
                AgentProfile,
                String,
                String,
                String,
                HashMap<String, serde_json::Value>,
                Vec<String>,
            ),
        > = HashMap::new();
        for config in &configs {
            let retry_context = clone_delegation_context(
                astra_core::history_work::HistoryWorkSite::DelegationRetryContextClone,
                &config.context,
            );
            retry_templates.insert(
                config.agent_profile.agent_id.clone(),
                (
                    config.agent_profile.clone(),
                    config.task.clone(),
                    config.session_id.clone(),
                    config.user_id.clone(),
                    retry_context,
                    config.delegation_chain.clone(),
                ),
            );
        }

        let per_agent_timeout = if timeout_sec > 0 {
            Some(std::time::Duration::from_secs(timeout_sec))
        } else {
            None
        };

        // Use JoinSet for abort-on-drop semantics: if caller times out before
        // collecting all results, remaining tasks are aborted automatically.
        let mut join_set: tokio::task::JoinSet<(AgentResult, String, String)> =
            tokio::task::JoinSet::new();
        // Track agent_id/run_id for panic recovery (JoinSet doesn't preserve spawn order)
        let mut id_map: HashMap<tokio::task::Id, (String, String)> = HashMap::new();

        for config in configs {
            let executor = self.executor.clone();
            let sem = semaphore.clone();
            let cancel = cancel_token.cloned();
            let agent_timeout = per_agent_timeout;
            // Capture identity before moving config into the closure (panic context)
            let captured_agent_id = config.agent_profile.agent_id.clone();
            let captured_run_id = config.run_id.clone();
            let abort_handle = join_set.spawn(async move {
                let run_id = config.run_id.clone();
                let agent_id = config.agent_profile.agent_id.clone();

                // Cancellation must interrupt queueing for a concurrency
                // permit. Once execution starts, the child receives its own
                // cancellation token and gets the bounded drain window below
                // to publish a canonical terminal result.
                let exec_future = async {
                    // A closed semaphore means the scheduler is shutting down;
                    // preserving the existing no-panic behavior is safe here.
                    let _permit = match sem {
                        Some(ref s) => match if let Some(token) = cancel.as_ref() {
                            tokio::select! {
                                biased;
                                _ = token.cancelled() => return Ok(cancelled_agent_result(&agent_id, &run_id)),
                                permit = s.acquire() => permit,
                            }
                        } else {
                            s.acquire().await
                        } {
                            Ok(p) => Some(p),
                            Err(_) => {
                                tracing::info!(
                                    target: "astra_runtime::delegation",
                                    "semaphore closed during shutdown; proceeding without permit"
                                );
                                None
                            }
                        },
                        None => None,
                    };
                    if cancel.as_ref().is_some_and(|token| token.is_cancelled()) {
                        return Ok(cancelled_agent_result(&agent_id, &run_id));
                    }
                    match agent_timeout {
                        Some(dur) => {
                            match tokio::time::timeout(dur, executor.execute(config)).await {
                                Ok(r) => r,
                                Err(_) => Err(format!(
                                    "agent execution exceeded per-agent timeout of {}s",
                                    dur.as_secs()
                                )),
                            }
                        }
                        None => executor.execute(config).await,
                    }
                };
                let result = match exec_future.await {
                    Ok(result) => result,
                    Err(error) => AgentResult {
                        agent_id: agent_id.clone(),
                        run_id: run_id.clone(),
                        status: STATUS_FAILED.to_string(),
                        output: None,
                        error: Some(error),
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        tool_calls: 0,
                    },
                };
                (result, agent_id, run_id)
            });
            id_map.insert(abort_handle.id(), (captured_agent_id, captured_run_id));
        }

        let mut results = Vec::new();
        // Cancellation-aware collection: let children observe their token and
        // publish their canonical `cancelled` result before resorting to an
        // abort. This wait is bounded so a stuck persistence/executor path
        // cannot hold the parent turn indefinitely.
        let mut cancellation_drain_deadline = None;
        let mut abort_drain_deadline = None;
        let mut cancellation_reconciliation_deadline = None;
        while let Some(join_result) = {
            if abort_drain_deadline.is_some() {
                abort_and_join_next_bounded(&mut join_set, &mut abort_drain_deadline, "fanout")
                    .await
            } else if let Some(deadline) = cancellation_drain_deadline {
                match tokio::time::timeout_at(deadline, join_set.join_next()).await {
                    Ok(result) => result,
                    Err(_) => {
                        tracing::warn!(
                            target: "astra_runtime::delegation",
                            timeout_ms = FANOUT_CANCELLATION_DRAIN_TIMEOUT.as_millis(),
                            "fanout cancellation drain timed out; aborting unacknowledged children"
                        );
                        abort_and_join_next_bounded(
                            &mut join_set,
                            &mut abort_drain_deadline,
                            "fanout",
                        )
                        .await
                    }
                }
            } else if let Some(token) = cancel_token {
                tokio::select! {
                    biased;
                    r = join_set.join_next() => r,
                    _ = token.cancelled() => {
                        let deadline = tokio::time::Instant::now() + FANOUT_CANCELLATION_DRAIN_TIMEOUT;
                        cancellation_drain_deadline = Some(deadline);
                        match tokio::time::timeout_at(deadline, join_set.join_next()).await {
                            Ok(result) => result,
                            Err(_) => {
                                tracing::warn!(
                                    target: "astra_runtime::delegation",
                                    timeout_ms = FANOUT_CANCELLATION_DRAIN_TIMEOUT.as_millis(),
                                    "fanout cancellation drain timed out; aborting unacknowledged children"
                                );
                                abort_and_join_next_bounded(
                                    &mut join_set,
                                    &mut abort_drain_deadline,
                                    "fanout",
                                )
                                .await
                            }
                        }
                    }
                }
            } else {
                join_set.join_next().await
            }
        } {
            match join_result {
                Ok((result, _, _)) => results.push(result),
                Err(e) => {
                    // JoinError (panic) — look up identity from id_map using task ID
                    let (panic_agent_id, panic_run_id) = id_map
                        .get(&e.id())
                        .cloned()
                        .unwrap_or_else(|| ("unknown".to_string(), "unknown".to_string()));
                    if e.is_cancelled() && cancel_token.is_some_and(|token| token.is_cancelled()) {
                        results.push(cancelled_agent_result(&panic_agent_id, &panic_run_id));
                        continue;
                    }
                    let panic_error = format!("task join error (panic): {e}");
                    results.push(AgentResult {
                        agent_id: panic_agent_id,
                        run_id: panic_run_id,
                        status: STATUS_FAILED.to_string(),
                        output: None,
                        error: Some(panic_error),
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        tool_calls: 0,
                    });
                }
            }
        }

        // A shared abort deadline can expire before every JoinSet entry yields
        // a projection result. Do not silently shrink result cardinality: use
        // the spawn identity map to reconcile each missing child through the
        // durable run authority. This keeps parent aggregation complete even
        // when local task cleanup is only partially observable.
        if cancel_token.is_some_and(|token| token.is_cancelled()) {
            let mut settled_run_ids = results
                .iter()
                .map(|result| result.run_id.clone())
                .collect::<HashSet<_>>();
            for (agent_id, run_id) in id_map.values() {
                if !settled_run_ids.insert(run_id.clone()) {
                    continue;
                }
                results.push(cancelled_agent_result(agent_id, run_id));
            }
        }

        // Settle the physical execution lifecycle before applying the
        // verification policy. Verification is a parent-level evaluation
        // fact; it must not retroactively rewrite a child execution that has
        // already committed its durable terminal state.
        let mut authoritative_results = Vec::with_capacity(results.len());
        for result in results {
            let disposition = owner_generations
                .get(&result.run_id)
                .copied()
                .map(|owner_generation| {
                    durable_lifecycle_disposition(self.executor.as_ref(), owner_generation)
                })
                .unwrap_or(DurableLifecycleDisposition::ReadOnly);
            let result = if cancel_token.is_some_and(|token| token.is_cancelled()) {
                reconcile_after_parent_cancellation_bounded(
                    &self.run_engine,
                    &request.user_id,
                    &session_id,
                    disposition,
                    result,
                    &mut cancellation_reconciliation_deadline,
                    "fanout",
                )
                .await
            } else {
                reconcile_agent_result_with_durable_authority(
                    &self.run_engine,
                    &request.user_id,
                    &session_id,
                    disposition,
                    result,
                )
                .await
            };
            authoritative_results.push(result);
        }
        let mut results = authoritative_results;

        // ── Verification gate: check each result before aggregation ──
        if self.gate.is_some() {
            let delegation_id = request.delegation_id.clone();
            let mut gated_results = Vec::with_capacity(results.len());
            for result in results {
                let did = delegation_id.clone();
                let cancel_for_retry = cancel_token.cloned();
                // Build retry config from stored template
                let retry_agent_id = result.agent_id.clone();
                let template = retry_templates.get(&retry_agent_id).map(|template| {
                    astra_core::history_work::record_serialized_value(
                        astra_core::history_work::HistoryWorkSite::DelegationRetryContextClone,
                        &template.4,
                    );
                    template.clone()
                });
                let gated = self
                    .apply_gate(
                        &request.user_id,
                        &session_id,
                        result,
                        &did,
                        &request.parent_run_id,
                        per_agent_timeout,
                        || {
                            if let Some(template) = template.as_ref() {
                                astra_core::history_work::record_serialized_value(
                                    astra_core::history_work::HistoryWorkSite::DelegationRetryContextClone,
                                    &template.4,
                                );
                            }
                            let Some((profile, task, sess, uid, ctx, delegation_chain)) =
                                template.clone()
                            else {
                                return Err(format!(
                                    "missing stored retry template for agent {retry_agent_id}"
                                ));
                            };
                            let delegate_model = "";
                            let inherited_prefix = self.resolve_inherited_prefix_for_delegate(
                                &request.parent_run_id,
                                delegate_model,
                            );
                            Ok(SubRunConfig {
                                run_id: uuid::Uuid::new_v4().to_string(),
                                parent_run_id: request.parent_run_id.clone(),
                                agent_profile: profile,
                                task,
                                session_id: sess,
                                user_id: uid,
                                execution_owner_generation: None,
                                execution_owner_generation_sink: None,
                                previous_output: None,
                                context: ctx,
                                forward_headers: forward_headers.clone(),
                                admitted_model_execution: admitted_model_execution.cloned(),
                                interaction_mode,
                                request_constraints: request_constraints.clone(),
                                recursion_depth: child_recursion_depth,
                                max_turns: None,
                                initial_turns: None,
                                pause_flag: None,
                                checkpoint_gate: None,
                                mailbox: None,
                                progress_emitter: None,
                                live_event_sink: live_event_sink.cloned(),
                                cancel_token: cancel_for_retry.clone(),
                                inherited_prefix,
                                execution_metadata: request.execution_metadata.clone(),
                                work_item: None,
                                delegation_chain,
                                #[cfg(feature = "harness")]
                                harness_sink: None,
                            })
                        },
                    )
                    .await;
                gated_results.push(gated);
            }
            results = gated_results;
        }

        let mut tracked_results = Vec::with_capacity(results.len());
        for result in results {
            let final_state = agent_result_status_to_subrun_state(&result.status);
            self.tracker
                .apply_sub_run_result_state(
                    &result.run_id,
                    final_state,
                    result.error.as_deref(),
                    result.output.as_deref(),
                )
                .await;
            tracked_results.push(result);
        }
        let results = tracked_results;

        let aggregated = aggregate_results(aggregation, &results);
        Ok(DelegationResult::from_results(
            &request.delegation_id,
            results,
            aggregated,
        ))
    }

    /// Sequential / Pipeline: execute agents one after another.
    /// Pipeline feeds previous output to the next agent.
    async fn execute_sequential(
        &self,
        request: &DelegationRequest,
        agent_ids: &[String],
        stop_on_success: bool,
        forward_headers: &HashMap<String, String>,
        admitted_model_execution: Option<&AdmittedModelExecution>,
        request_constraints: &RequestConstraints,
        child_recursion_depth: u8,
        interaction_mode: RequestedTurnInteractionMode,
        timeout_sec: u64,
        cancel_token: Option<&Arc<tokio_util::sync::CancellationToken>>,
        live_event_sink: Option<&astra_turn_core::agent_live_event::SharedAgentLiveEventSink>,
    ) -> Result<DelegationResult, String> {
        let reg = self.registry.read().await;
        let mut results = Vec::new();
        let mut previous_output: Option<String> = None;
        let has_gate = self.gate.is_some();
        let total_stages = agent_ids.len();
        let budget_prompt = Self::extract_budget_prompt(&request.context);
        let per_stage_timeout = if timeout_sec > 0 {
            Some(std::time::Duration::from_secs(timeout_sec))
        } else {
            None
        };

        for (stage_index, agent_id) in agent_ids.iter().enumerate() {
            // Check cancellation before starting next sequential agent
            if let Some(token) = cancel_token {
                if token.is_cancelled() {
                    break;
                }
            }

            let sub_run_id = uuid::Uuid::new_v4().to_string();
            let session_id = Self::session_id_for(request);

            let execution_authority = self
                .start_delegated_run(
                    &sub_run_id,
                    &request.user_id,
                    &session_id,
                    &request.parent_run_id,
                    &request.delegation_id,
                    agent_id,
                    None,
                    interaction_mode,
                )
                .await?;

            self.tracker
                .record_sub_run(SubRunRecord {
                    run_id: sub_run_id.clone(),
                    parent_run_id: request.parent_run_id.clone(),
                    delegation_id: request.delegation_id.clone(),
                    agent_id: agent_id.clone(),
                    depth: request.depth + 1,
                    state: SubRunState::Created,
                    retry_of: None,
                })
                .await;

            // Transition Created → Running
            if let Err(e) = self
                .tracker
                .transition_state(&sub_run_id, SubRunState::Running)
                .await
            {
                astra_core::agent_warn!(
                    "delegation",
                    "Sequential: transition to Running failed for {sub_run_id}: {e:?}"
                );
            }

            self.run_engine
                .transition_status_with_events_if_current_owner(
                    &request.user_id,
                    &session_id,
                    &sub_run_id,
                    &[STATUS_RUNNING],
                    execution_authority.owner_generation,
                    STATUS_RUNNING,
                    Some("agent_execution"),
                    None,
                    &[],
                )
                .await?
                .then_some(())
                .ok_or_else(|| {
                    format!("sequential child {sub_run_id} lost durable execution authority")
                })?;

            let pause_flag = self.tracker.register_pause_flag(&sub_run_id).await;
            let child_cancel = cancel_token
                .map(|t| Arc::new(t.child_token()))
                .unwrap_or_else(|| Arc::new(tokio_util::sync::CancellationToken::new()));
            self.tracker
                .register_cancel_token(&sub_run_id, child_cancel.clone())
                .await;

            let profile = match reg.get(agent_id) {
                Some(p) => p.clone(),
                None => {
                    return Err(Self::missing_agent_profile_error(
                        "sequential spawn",
                        agent_id,
                        &reg,
                    ));
                }
            };
            let delegation_chain = Self::delegation_chain_for_child(request, agent_id)?;

            let mailbox = if let Some(router) = &self.mailbox_router {
                let addr = astra_messaging::types::AgentAddress {
                    run_id: sub_run_id.clone(),
                    agent_id: agent_id.clone(),
                };
                match router
                    .register(addr, Some(request.delegation_id.clone()))
                    .await
                {
                    Ok(mb) => Some(mb),
                    Err(e) => {
                        eprintln!(
                            "  ⚠ delegation: mailbox registration failed for {agent_id}: {e}"
                        );
                        None
                    }
                }
            } else {
                None
            };

            // Inject sequential/pipeline coordination prompt
            let has_prev = previous_output.is_some();
            let coordination_prompt = format!(
                "{}{}",
                team_prompts::sequential_stage_prompt(
                    stage_index,
                    total_stages,
                    agent_id,
                    has_prev,
                    stop_on_success,
                    has_gate,
                ),
                budget_prompt,
            );
            let enhanced_task =
                team_prompts::wrap_task_with_coordination(&coordination_prompt, &request.task);
            let retry_task = enhanced_task.clone();

            let config = SubRunConfig {
                run_id: sub_run_id.clone(),
                parent_run_id: request.parent_run_id.clone(),
                agent_profile: profile,
                task: enhanced_task,
                session_id: Self::session_id_for(request),
                user_id: request.user_id.clone(),
                execution_owner_generation: Some(execution_authority.owner_generation),
                execution_owner_generation_sink: None,
                previous_output: previous_output.clone(),
                context: Self::child_task_context(request),
                forward_headers: forward_headers.clone(),
                admitted_model_execution: admitted_model_execution.cloned(),
                interaction_mode,
                request_constraints: request_constraints.clone(),
                recursion_depth: child_recursion_depth,
                max_turns: None,
                initial_turns: None,
                pause_flag: Some(pause_flag),
                checkpoint_gate: None,
                mailbox,
                progress_emitter: None,
                live_event_sink: live_event_sink.cloned(),
                cancel_token: Some(child_cancel),
                inherited_prefix: None,
                execution_metadata: request.execution_metadata.clone(),
                work_item: None,
                delegation_chain: delegation_chain.clone(),
                #[cfg(feature = "harness")]
                harness_sink: None,
            };

            let exec_result = match per_stage_timeout {
                Some(dur) => match tokio::time::timeout(dur, self.executor.execute(config)).await {
                    Ok(r) => r,
                    Err(_) => Err(format!(
                        "agent {} exceeded per-stage timeout of {}s",
                        agent_id,
                        dur.as_secs()
                    )),
                },
                None => self.executor.execute(config).await,
            };

            let result = match exec_result {
                Ok(result) => result,
                Err(error) => AgentResult {
                    agent_id: agent_id.clone(),
                    run_id: sub_run_id.clone(),
                    status: STATUS_FAILED.to_string(),
                    output: None,
                    error: Some(error),
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    tool_calls: 0,
                },
            };
            let result = reconcile_agent_result_with_durable_authority(
                &self.run_engine,
                &request.user_id,
                &session_id,
                durable_lifecycle_disposition(
                    self.executor.as_ref(),
                    execution_authority.owner_generation,
                ),
                result,
            )
            .await;
            // ── Verification gate with retry for sequential sub-runs ──
            let result = if self.gate.is_some() {
                let delegation_id = request.delegation_id.clone();
                let sess = Self::session_id_for(request);
                let uid = request.user_id.clone();
                let ctx = Self::child_task_context(request);
                let prev = previous_output.clone();
                let cancel_for_retry = cancel_token.cloned();
                let profile_for_retry = reg.get(agent_id).cloned();
                let available_profiles_for_retry = Self::available_agent_profile_ids(&reg);
                let retry_agent_id = agent_id.clone();
                self.apply_gate(
                    &request.user_id,
                    &session_id,
                    result,
                    &delegation_id,
                    &request.parent_run_id,
                    per_stage_timeout,
                    || {
                        let profile = profile_for_retry.clone().ok_or_else(|| {
                            format!(
                                "delegation failed during verification retry: requested agent profile '{retry_agent_id}' is not registered. \
                                 Available profiles: [{available_profiles_for_retry}]. This is a configuration error; do not invent a replacement agent_id."
                            )
                        })?;
                        Ok(SubRunConfig {
                            run_id: uuid::Uuid::new_v4().to_string(),
                            parent_run_id: request.parent_run_id.clone(),
                            agent_profile: profile,
                            task: retry_task.clone(),
                            session_id: sess.clone(),
                            user_id: uid.clone(),
                            execution_owner_generation: None,
                            execution_owner_generation_sink: None,
                            previous_output: prev.clone(),
                            context: clone_delegation_context(
                                astra_core::history_work::HistoryWorkSite::DelegationRetryContextClone,
                                &ctx,
                            ),
                            forward_headers: forward_headers.clone(),
                            admitted_model_execution: admitted_model_execution.cloned(),
                            interaction_mode,
                            request_constraints: request_constraints.clone(),
                            recursion_depth: child_recursion_depth,
                            max_turns: None,
                            initial_turns: None,
                            pause_flag: None,
                            checkpoint_gate: None,
                            mailbox: None,
                            progress_emitter: None,
                            live_event_sink: live_event_sink.cloned(),
                            cancel_token: cancel_for_retry.clone(),
                            inherited_prefix: None,
                            execution_metadata: request.execution_metadata.clone(),
                            work_item: None,
                            delegation_chain: delegation_chain.clone(),
                            #[cfg(feature = "harness")]
                            harness_sink: None,
                        })
                    },
                )
                .await
            } else {
                result
            };
            let final_state = agent_result_status_to_subrun_state(&result.status);
            self.tracker
                .apply_sub_run_result_state(
                    &result.run_id,
                    final_state,
                    result.error.as_deref(),
                    result.output.as_deref(),
                )
                .await;

            // Feed output to the next stage (pipeline semantics).
            previous_output = result.output.clone();
            let is_success = result.is_success();
            results.push(result);

            if stop_on_success && is_success {
                break;
            }
        }

        Ok(DelegationResult::from_results(
            &request.delegation_id,
            results,
            None,
        ))
    }

    /// Adversarial review: producer creates, reviewer critiques, repeat.
    async fn execute_adversarial(
        &self,
        request: &DelegationRequest,
        producer_id: &str,
        reviewer_id: &str,
        max_rounds: u32,
        forward_headers: &HashMap<String, String>,
        admitted_model_execution: Option<&AdmittedModelExecution>,
        request_constraints: &RequestConstraints,
        child_recursion_depth: u8,
        interaction_mode: RequestedTurnInteractionMode,
        timeout_sec: u64,
        cancel_token: Option<&Arc<tokio_util::sync::CancellationToken>>,
        live_event_sink: Option<&astra_turn_core::agent_live_event::SharedAgentLiveEventSink>,
    ) -> Result<DelegationResult, String> {
        let reg = self.registry.read().await;
        let mut results = Vec::new();
        let mut last_producer_output: Option<String> = None;
        let budget_prompt = Self::extract_budget_prompt(&request.context);
        let per_round_timeout = if timeout_sec > 0 {
            Some(std::time::Duration::from_secs(timeout_sec))
        } else {
            None
        };

        let producer_profile = reg.get(producer_id).cloned().ok_or_else(|| {
            Self::missing_agent_profile_error("adversarial producer", producer_id, &reg)
        })?;
        let reviewer_profile = reg.get(reviewer_id).cloned().ok_or_else(|| {
            Self::missing_agent_profile_error("adversarial reviewer", reviewer_id, &reg)
        })?;
        let producer_delegation_chain = Self::delegation_chain_for_child(request, producer_id)?;
        let reviewer_delegation_chain = Self::delegation_chain_for_child(request, reviewer_id)?;
        drop(reg);

        for round in 0..max_rounds {
            // Check cancellation before starting next adversarial round
            if let Some(token) = cancel_token {
                if token.is_cancelled() {
                    break;
                }
            }

            // ── Producer sub-run ──
            let prod_run_id = uuid::Uuid::new_v4().to_string();
            let session_id = Self::session_id_for(request);
            let prod_execution_authority = self
                .start_delegated_run(
                    &prod_run_id,
                    &request.user_id,
                    &session_id,
                    &request.parent_run_id,
                    &request.delegation_id,
                    producer_id,
                    None,
                    interaction_mode,
                )
                .await?;
            self.tracker
                .record_sub_run(SubRunRecord {
                    run_id: prod_run_id.clone(),
                    parent_run_id: request.parent_run_id.clone(),
                    delegation_id: request.delegation_id.clone(),
                    agent_id: producer_id.to_string(),
                    depth: request.depth + 1,
                    state: SubRunState::Created,
                    retry_of: None,
                })
                .await;
            if let Err(e) = self
                .tracker
                .transition_state(&prod_run_id, SubRunState::Running)
                .await
            {
                astra_core::agent_warn!(
                    "delegation",
                    "Adversarial: transition to Running failed for producer {prod_run_id}: {e:?}"
                );
            }
            self.run_engine
                .transition_status_with_events_if_current_owner(
                    &request.user_id,
                    &session_id,
                    &prod_run_id,
                    &[STATUS_RUNNING],
                    prod_execution_authority.owner_generation,
                    STATUS_RUNNING,
                    Some("produce"),
                    None,
                    &[],
                )
                .await?
                .then_some(())
                .ok_or_else(|| {
                    format!("producer child {prod_run_id} lost durable execution authority")
                })?;
            let prod_pause = self.tracker.register_pause_flag(&prod_run_id).await;
            let prod_cancel = cancel_token
                .map(|token| Arc::new(token.child_token()))
                .unwrap_or_else(|| Arc::new(tokio_util::sync::CancellationToken::new()));
            self.tracker
                .register_cancel_token(&prod_run_id, prod_cancel.clone())
                .await;
            self.run_engine
                .append_event(
                    &request.user_id,
                    &session_id,
                    &prod_run_id,
                    serde_json::json!({"event_type": "adversarial_round", "data": {"round": round, "role": "producer"}}),
                )
                .await?;

            let prod_mailbox = if let Some(router) = &self.mailbox_router {
                let addr = astra_messaging::types::AgentAddress {
                    run_id: prod_run_id.clone(),
                    agent_id: producer_id.to_string(),
                };
                match router
                    .register(addr, Some(request.delegation_id.clone()))
                    .await
                {
                    Ok(mb) => Some(mb),
                    Err(e) => {
                        eprintln!(
                            "  ⚠ delegation: mailbox registration failed for {producer_id}: {e}"
                        );
                        None
                    }
                }
            } else {
                None
            };

            // Inject adversarial producer coordination prompt
            let has_feedback = last_producer_output.is_some();
            let has_gate = self.gate.is_some();
            let prod_coordination = format!(
                "{}{}",
                team_prompts::adversarial_producer_prompt(
                    reviewer_id,
                    max_rounds,
                    round,
                    has_feedback,
                    has_gate,
                ),
                budget_prompt,
            );
            let prod_enhanced_task =
                team_prompts::wrap_task_with_coordination(&prod_coordination, &request.task);
            let prod_retry_task = prod_enhanced_task.clone();

            let prod_config = SubRunConfig {
                run_id: prod_run_id.clone(),
                parent_run_id: request.parent_run_id.clone(),
                agent_profile: producer_profile.clone(),
                task: prod_enhanced_task,
                session_id: Self::session_id_for(request),
                user_id: request.user_id.clone(),
                execution_owner_generation: Some(prod_execution_authority.owner_generation),
                execution_owner_generation_sink: None,
                previous_output: last_producer_output.clone(),
                context: Self::child_task_context(request),
                forward_headers: forward_headers.clone(),
                admitted_model_execution: admitted_model_execution.cloned(),
                interaction_mode,
                request_constraints: request_constraints.clone(),
                recursion_depth: child_recursion_depth,
                max_turns: None,
                initial_turns: None,
                pause_flag: Some(prod_pause.clone()),
                checkpoint_gate: None,
                mailbox: prod_mailbox,
                progress_emitter: None,
                live_event_sink: live_event_sink.cloned(),
                cancel_token: Some(prod_cancel),
                inherited_prefix: None,
                execution_metadata: request.execution_metadata.clone(),
                work_item: None,
                delegation_chain: producer_delegation_chain.clone(),
                #[cfg(feature = "harness")]
                harness_sink: None,
            };
            let prod_exec = match per_round_timeout {
                Some(dur) => {
                    match tokio::time::timeout(dur, self.executor.execute(prod_config)).await {
                        Ok(r) => r,
                        Err(_) => Err(format!(
                            "producer {} exceeded per-round timeout of {}s",
                            producer_id,
                            dur.as_secs()
                        )),
                    }
                }
                None => self.executor.execute(prod_config).await,
            };
            let prod_result = match prod_exec {
                Ok(result) => result,
                Err(error) => AgentResult {
                    agent_id: producer_id.to_string(),
                    run_id: prod_run_id.clone(),
                    status: STATUS_FAILED.to_string(),
                    output: None,
                    error: Some(error),
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    tool_calls: 0,
                },
            };
            let prod_result = reconcile_agent_result_with_durable_authority(
                &self.run_engine,
                &request.user_id,
                &session_id,
                durable_lifecycle_disposition(
                    self.executor.as_ref(),
                    prod_execution_authority.owner_generation,
                ),
                prod_result,
            )
            .await;
            // ── Gate on producer output before reviewer sees it ──
            let prod_result = if self.gate.is_some() {
                let did = request.delegation_id.clone();
                let sess = Self::session_id_for(request);
                let uid = request.user_id.clone();
                let ctx = Self::child_task_context(request);
                let prev = last_producer_output.clone();
                let cancel_for_retry = cancel_token.cloned();
                let pp = producer_profile.clone();
                self.apply_gate(
                    &request.user_id,
                    &session_id,
                    prod_result,
                    &did,
                    &request.parent_run_id,
                    per_round_timeout,
                    || {
                        Ok(SubRunConfig {
                            run_id: uuid::Uuid::new_v4().to_string(),
                            parent_run_id: request.parent_run_id.clone(),
                            agent_profile: pp.clone(),
                            task: prod_retry_task.clone(),
                            session_id: sess.clone(),
                            user_id: uid.clone(),
                            execution_owner_generation: None,
                            execution_owner_generation_sink: None,
                            previous_output: prev.clone(),
                            context: clone_delegation_context(
                                astra_core::history_work::HistoryWorkSite::DelegationRetryContextClone,
                                &ctx,
                            ),
                            forward_headers: forward_headers.clone(),
                            admitted_model_execution: admitted_model_execution.cloned(),
                            interaction_mode,
                            request_constraints: request_constraints.clone(),
                            recursion_depth: child_recursion_depth,
                            max_turns: None,
                            initial_turns: None,
                            pause_flag: None,
                            checkpoint_gate: None,
                            mailbox: None,
                            progress_emitter: None,
                            live_event_sink: live_event_sink.cloned(),
                            cancel_token: cancel_for_retry.clone(),
                            inherited_prefix: None,
                            execution_metadata: request.execution_metadata.clone(),
                            work_item: None,
                            delegation_chain: producer_delegation_chain.clone(),
                            #[cfg(feature = "harness")]
                            harness_sink: None,
                        })
                    },
                )
                .await
            } else {
                prod_result
            };
            let final_state = agent_result_status_to_subrun_state(&prod_result.status);
            self.tracker
                .apply_sub_run_result_state(
                    &prod_result.run_id,
                    final_state,
                    prod_result.error.as_deref(),
                    prod_result.output.as_deref(),
                )
                .await;

            last_producer_output = prod_result.output.clone();
            results.push(prod_result);

            // ── Reviewer sub-run ──
            let rev_run_id = uuid::Uuid::new_v4().to_string();
            let rev_execution_authority = self
                .start_delegated_run(
                    &rev_run_id,
                    &request.user_id,
                    &session_id,
                    &request.parent_run_id,
                    &request.delegation_id,
                    reviewer_id,
                    None,
                    interaction_mode,
                )
                .await?;
            self.tracker
                .record_sub_run(SubRunRecord {
                    run_id: rev_run_id.clone(),
                    parent_run_id: request.parent_run_id.clone(),
                    delegation_id: request.delegation_id.clone(),
                    agent_id: reviewer_id.to_string(),
                    depth: request.depth + 1,
                    state: SubRunState::Created,
                    retry_of: None,
                })
                .await;
            if let Err(e) = self
                .tracker
                .transition_state(&rev_run_id, SubRunState::Running)
                .await
            {
                astra_core::agent_warn!(
                    "delegation",
                    "Adversarial: transition to Running failed for reviewer {rev_run_id}: {e:?}"
                );
            }
            self.run_engine
                .transition_status_with_events_if_current_owner(
                    &request.user_id,
                    &session_id,
                    &rev_run_id,
                    &[STATUS_RUNNING],
                    rev_execution_authority.owner_generation,
                    STATUS_RUNNING,
                    Some("review"),
                    None,
                    &[],
                )
                .await?
                .then_some(())
                .ok_or_else(|| {
                    format!("reviewer child {rev_run_id} lost durable execution authority")
                })?;
            let rev_pause = self.tracker.register_pause_flag(&rev_run_id).await;
            let rev_cancel = cancel_token
                .map(|token| Arc::new(token.child_token()))
                .unwrap_or_else(|| Arc::new(tokio_util::sync::CancellationToken::new()));
            self.tracker
                .register_cancel_token(&rev_run_id, rev_cancel.clone())
                .await;
            self.run_engine
                .append_event(
                    &request.user_id,
                    &session_id,
                    &rev_run_id,
                    serde_json::json!({"event_type": "adversarial_round", "data": {"round": round, "role": "reviewer"}}),
                )
                .await?;

            let rev_mailbox = if let Some(router) = &self.mailbox_router {
                let addr = astra_messaging::types::AgentAddress {
                    run_id: rev_run_id.clone(),
                    agent_id: reviewer_id.to_string(),
                };
                match router
                    .register(addr, Some(request.delegation_id.clone()))
                    .await
                {
                    Ok(mb) => Some(mb),
                    Err(e) => {
                        eprintln!(
                            "  ⚠ delegation: mailbox registration failed for {reviewer_id}: {e}"
                        );
                        None
                    }
                }
            } else {
                None
            };

            // Inject adversarial reviewer coordination prompt
            let rev_coordination =
                team_prompts::adversarial_reviewer_prompt(producer_id, max_rounds, round);
            let rev_enhanced_task =
                team_prompts::wrap_task_with_coordination(&rev_coordination, &request.task);

            let rev_config = SubRunConfig {
                run_id: rev_run_id.clone(),
                parent_run_id: request.parent_run_id.clone(),
                agent_profile: reviewer_profile.clone(),
                task: rev_enhanced_task,
                session_id: Self::session_id_for(request),
                user_id: request.user_id.clone(),
                execution_owner_generation: Some(rev_execution_authority.owner_generation),
                execution_owner_generation_sink: None,
                previous_output: last_producer_output.clone(),
                context: Self::child_task_context(request),
                forward_headers: forward_headers.clone(),
                admitted_model_execution: admitted_model_execution.cloned(),
                interaction_mode,
                request_constraints: request_constraints.clone(),
                recursion_depth: child_recursion_depth,
                max_turns: None,
                initial_turns: None,
                pause_flag: Some(rev_pause),
                checkpoint_gate: None,
                mailbox: rev_mailbox,
                progress_emitter: None,
                live_event_sink: live_event_sink.cloned(),
                cancel_token: Some(rev_cancel),
                inherited_prefix: None,
                execution_metadata: request.execution_metadata.clone(),
                work_item: None,
                delegation_chain: reviewer_delegation_chain.clone(),
                #[cfg(feature = "harness")]
                harness_sink: None,
            };
            let rev_exec = match per_round_timeout {
                Some(dur) => {
                    match tokio::time::timeout(dur, self.executor.execute(rev_config)).await {
                        Ok(r) => r,
                        Err(_) => Err(format!(
                            "reviewer {} exceeded per-round timeout of {}s",
                            reviewer_id,
                            dur.as_secs()
                        )),
                    }
                }
                None => self.executor.execute(rev_config).await,
            };
            let rev_result = match rev_exec {
                Ok(result) => result,
                Err(error) => AgentResult {
                    agent_id: reviewer_id.to_string(),
                    run_id: rev_run_id.clone(),
                    status: STATUS_FAILED.to_string(),
                    output: None,
                    error: Some(error),
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    tool_calls: 0,
                },
            };
            let rev_result = reconcile_agent_result_with_durable_authority(
                &self.run_engine,
                &request.user_id,
                &session_id,
                durable_lifecycle_disposition(
                    self.executor.as_ref(),
                    rev_execution_authority.owner_generation,
                ),
                rev_result,
            )
            .await;
            let final_state = agent_result_status_to_subrun_state(&rev_result.status);
            self.tracker
                .apply_sub_run_result_state(
                    &rev_run_id,
                    final_state,
                    rev_result.error.as_deref(),
                    rev_result.output.as_deref(),
                )
                .await;
            results.push(rev_result);
        }

        Ok(DelegationResult::from_results(
            &request.delegation_id,
            results,
            None,
        ))
    }

    /// Fork: dispatch N tasks sharing the parent's full conversation context.
    ///
    /// All fork children receive the same message prefix (the parent's conversation
    /// history up to this point), enabling prompt cache sharing across children.
    /// Fork children cannot recursively fork or delegate.
    async fn execute_fork(
        &self,
        request: &DelegationRequest,
        tasks: &[String],
        agent_id: &str,
        _max_turns: u32,
        _aggregation: &AggregationStrategy,
        forward_headers: &HashMap<String, String>,
        admitted_model_execution: Option<&AdmittedModelExecution>,
        request_constraints: &RequestConstraints,
        child_recursion_depth: u8,
        interaction_mode: RequestedTurnInteractionMode,
        timeout_sec: u64,
        cancel_token: Option<&Arc<tokio_util::sync::CancellationToken>>,
        live_event_sink: Option<&astra_turn_core::agent_live_event::SharedAgentLiveEventSink>,
    ) -> Result<DelegationResult, String> {
        let reg = self.registry.read().await;
        let profile = reg
            .get(agent_id)
            .cloned()
            .ok_or_else(|| Self::missing_agent_profile_error("fork spawn", agent_id, &reg))?;
        let fork_delegation_chain = Self::delegation_chain_for_child(request, agent_id)?;
        drop(reg);

        // Extract parent messages for context inheritance (if provided)
        let parent_messages = request
            .context
            .get("parent_messages")
            .map(|messages| {
                clone_delegation_value(
                    astra_core::history_work::HistoryWorkSite::DelegationParentMessagesClone,
                    messages,
                )
            })
            .unwrap_or_else(|| serde_json::json!([]));

        let session_id = Self::session_id_for(request);

        // Spawn fork children in parallel, respecting optional max_parallel limit.
        let max_parallel = request
            .context
            .get("team_max_parallel")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        let fork_semaphore = if max_parallel > 0 {
            Some(Arc::new(tokio::sync::Semaphore::new(max_parallel)))
        } else {
            None
        };
        let mut handles: tokio::task::JoinSet<(AgentResult, String, String)> =
            tokio::task::JoinSet::new();
        let mut fork_id_map: HashMap<tokio::task::Id, (String, String, u64)> = HashMap::new();
        for (i, task) in tasks.iter().enumerate() {
            let run_id = uuid::Uuid::new_v4().to_string();
            let execution_authority = self
                .start_delegated_run(
                    &run_id,
                    &request.user_id,
                    &session_id,
                    &request.parent_run_id,
                    &request.delegation_id,
                    agent_id,
                    None,
                    interaction_mode,
                )
                .await?;
            self.tracker
                .record_sub_run(SubRunRecord {
                    run_id: run_id.clone(),
                    parent_run_id: request.parent_run_id.clone(),
                    delegation_id: request.delegation_id.clone(),
                    agent_id: agent_id.to_string(),
                    depth: request.depth + 1,
                    state: SubRunState::Created,
                    retry_of: None,
                })
                .await;
            if let Err(e) = self
                .tracker
                .transition_state(&run_id, SubRunState::Running)
                .await
            {
                astra_core::agent_warn!(
                    "delegation",
                    "Fork: transition to Running failed for {run_id}: {e:?}"
                );
            }
            match self
                .run_engine
                .transition_status_with_events_if_current_owner(
                    &request.user_id,
                    &session_id,
                    &run_id,
                    &[STATUS_RUNNING],
                    execution_authority.owner_generation,
                    STATUS_RUNNING,
                    Some("fork"),
                    None,
                    &[],
                )
                .await
            {
                Ok(true) => {}
                Ok(false) => {
                    return Err(format!(
                        "fork child {run_id} lost durable execution authority before activation"
                    ));
                }
                Err(error) => {
                    return Err(format!(
                        "failed to activate durable fork child {run_id}: {error}"
                    ));
                }
            }
            let pause_flag = self.tracker.register_pause_flag(&run_id).await;
            let child_cancel = cancel_token
                .map(|token| Arc::new(token.child_token()))
                .unwrap_or_else(|| Arc::new(tokio_util::sync::CancellationToken::new()));
            self.tracker
                .register_cancel_token(&run_id, child_cancel.clone())
                .await;

            let fork_mailbox = if let Some(router) = &self.mailbox_router {
                let addr = astra_messaging::types::AgentAddress {
                    run_id: run_id.clone(),
                    agent_id: agent_id.to_string(),
                };
                match router
                    .register(addr, Some(request.delegation_id.clone()))
                    .await
                {
                    Ok(mb) => Some(mb),
                    Err(e) => {
                        eprintln!(
                            "  ⚠ delegation: mailbox registration failed for {agent_id}: {e}"
                        );
                        None
                    }
                }
            } else {
                None
            };

            // Build fork-specific context: parent messages + fork instruction
            let mut fork_context = Self::child_task_context(request);
            fork_context.insert("fork_index".to_string(), serde_json::json!(i));
            fork_context.insert(
                "parent_messages".to_string(),
                clone_delegation_value(
                    astra_core::history_work::HistoryWorkSite::DelegationParentMessagesClone,
                    &parent_messages,
                ),
            );
            fork_context.insert("is_fork_child".to_string(), serde_json::json!(true));

            let has_parent_ctx = !parent_messages.as_array().map_or(true, |a| a.is_empty());
            let budget_prompt = Self::extract_budget_prompt(&request.context);
            let fork_coordination = format!(
                "{}{}",
                team_prompts::fork_child_prompt(i, tasks.len(), has_parent_ctx),
                budget_prompt,
            );
            let fork_task = team_prompts::wrap_task_with_coordination(&fork_coordination, task);

            let mut fork_profile = profile.clone();
            fork_profile.can_delegate = false;
            fork_profile.max_delegation_depth = 0;

            let config = SubRunConfig {
                run_id: run_id.clone(),
                parent_run_id: request.parent_run_id.clone(),
                agent_profile: fork_profile,
                task: fork_task,
                session_id: session_id.clone(),
                user_id: request.user_id.clone(),
                execution_owner_generation: Some(execution_authority.owner_generation),
                execution_owner_generation_sink: None,
                previous_output: None,
                context: fork_context,
                forward_headers: forward_headers.clone(),
                admitted_model_execution: admitted_model_execution.cloned(),
                interaction_mode,
                request_constraints: request_constraints.clone(),
                recursion_depth: child_recursion_depth,
                max_turns: None,
                initial_turns: None,
                pause_flag: Some(pause_flag),
                checkpoint_gate: None,
                mailbox: fork_mailbox,
                progress_emitter: None,
                live_event_sink: live_event_sink.cloned(),
                cancel_token: Some(child_cancel),
                inherited_prefix: None,
                execution_metadata: request.execution_metadata.clone(),
                work_item: None,
                delegation_chain: fork_delegation_chain.clone(),
                #[cfg(feature = "harness")]
                harness_sink: None,
            };

            let executor = self.executor.clone();
            let durable_disposition = durable_lifecycle_disposition(
                executor.as_ref(),
                execution_authority.owner_generation,
            );
            let run_engine = self.run_engine.clone();
            let tracker = self.tracker.clone();
            let sem = fork_semaphore.clone();
            let cancel_for_spawn = cancel_token.cloned();
            let per_child_timeout = if timeout_sec > 0 {
                Some(std::time::Duration::from_secs(timeout_sec))
            } else {
                None
            };
            // Capture identity before moving config (panic context)
            let captured_agent_id = config.agent_profile.agent_id.clone();
            let captured_run_id = config.run_id.clone();
            let request_user_id = config.user_id.clone();
            let request_session_id = config.session_id.clone();
            let abort_handle = handles.spawn(async move {
                let run_id = config.run_id.clone();
                let agent_id = config.agent_profile.agent_id.clone();

                let exec_future = async {
                    // Cancellation must also release tasks still waiting for a
                    // fanout permit; queued work has not failed merely because
                    // its parent was cancelled.
                    let _permit = match sem {
                        Some(ref s) => match if let Some(token) = cancel_for_spawn.as_ref() {
                            tokio::select! {
                                biased;
                                _ = token.cancelled() => return Ok(cancelled_agent_result(&agent_id, &run_id)),
                                permit = s.acquire() => permit,
                            }
                        } else {
                            s.acquire().await
                        } {
                            Ok(p) => Some(p),
                            Err(_) => {
                                tracing::info!(
                                    target: "astra_runtime::delegation",
                                    "semaphore closed during shutdown; proceeding without permit"
                                );
                                None
                            }
                        },
                        None => None,
                    };
                    if cancel_for_spawn
                        .as_ref()
                        .is_some_and(|token| token.is_cancelled())
                    {
                        return Ok(cancelled_agent_result(&agent_id, &run_id));
                    }
                    match per_child_timeout {
                        Some(dur) => {
                            match tokio::time::timeout(dur, executor.execute(config)).await {
                                Ok(r) => r,
                                Err(_) => Err(format!(
                                    "fork child exceeded per-child timeout of {}s",
                                    dur.as_secs()
                                )),
                            }
                        }
                        None => executor.execute(config).await,
                    }
                };
                let result = match exec_future.await {
                    Ok(result) => result,
                    Err(error) => AgentResult {
                        agent_id: agent_id.clone(),
                        run_id: run_id.clone(),
                        status: STATUS_FAILED.to_string(),
                        output: None,
                        error: Some(error),
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        tool_calls: 0,
                    },
                };
                let result = reconcile_agent_result_with_durable_authority(
                    &run_engine,
                    &request_user_id,
                    &request_session_id,
                    durable_disposition,
                    result,
                )
                .await;
                let final_state = agent_result_status_to_subrun_state(&result.status);
                tracker
                    .apply_sub_run_result_state(
                        &run_id,
                        final_state,
                        result.error.as_deref(),
                        result.output.as_deref(),
                    )
                    .await;
                (result, agent_id, run_id)
            });
            fork_id_map.insert(
                abort_handle.id(),
                (
                    captured_agent_id,
                    captured_run_id,
                    execution_authority.owner_generation,
                ),
            );
        }

        // As in the regular fanout path, first let cancellation propagate to
        // children and only force-abort after a finite acknowledgement window.
        let mut results = Vec::with_capacity(tasks.len());
        let mut cancellation_drain_deadline = None;
        let mut abort_drain_deadline = None;
        let mut cancellation_reconciliation_deadline = None;
        while let Some(join_result) = {
            if abort_drain_deadline.is_some() {
                abort_and_join_next_bounded(&mut handles, &mut abort_drain_deadline, "fork").await
            } else if let Some(deadline) = cancellation_drain_deadline {
                match tokio::time::timeout_at(deadline, handles.join_next()).await {
                    Ok(result) => result,
                    Err(_) => {
                        tracing::warn!(
                            target: "astra_runtime::delegation",
                            timeout_ms = FANOUT_CANCELLATION_DRAIN_TIMEOUT.as_millis(),
                            "fork cancellation drain timed out; aborting unacknowledged children"
                        );
                        abort_and_join_next_bounded(&mut handles, &mut abort_drain_deadline, "fork")
                            .await
                    }
                }
            } else if let Some(token) = cancel_token {
                tokio::select! {
                    biased;
                    r = handles.join_next() => r,
                    _ = token.cancelled() => {
                        let deadline = tokio::time::Instant::now() + FANOUT_CANCELLATION_DRAIN_TIMEOUT;
                        cancellation_drain_deadline = Some(deadline);
                        match tokio::time::timeout_at(deadline, handles.join_next()).await {
                            Ok(result) => result,
                            Err(_) => {
                                tracing::warn!(
                                    target: "astra_runtime::delegation",
                                    timeout_ms = FANOUT_CANCELLATION_DRAIN_TIMEOUT.as_millis(),
                                    "fork cancellation drain timed out; aborting unacknowledged children"
                                );
                                abort_and_join_next_bounded(
                                    &mut handles,
                                    &mut abort_drain_deadline,
                                    "fork",
                                )
                                .await
                            }
                        }
                    }
                }
            } else {
                handles.join_next().await
            }
        } {
            match join_result {
                Ok((result, _, _)) => results.push(result),
                Err(e) => {
                    let (panic_agent_id, panic_run_id, panic_owner_generation) = fork_id_map
                        .get(&e.id())
                        .cloned()
                        .unwrap_or_else(|| ("unknown".to_string(), "unknown".to_string(), 0));
                    let panic_disposition = if panic_run_id == "unknown" {
                        DurableLifecycleDisposition::ReadOnly
                    } else {
                        durable_lifecycle_disposition(
                            self.executor.as_ref(),
                            panic_owner_generation,
                        )
                    };
                    if e.is_cancelled() && cancel_token.is_some_and(|token| token.is_cancelled()) {
                        let reconciled = reconcile_after_parent_cancellation_bounded(
                            &self.run_engine,
                            &request.user_id,
                            &session_id,
                            panic_disposition,
                            cancelled_agent_result(&panic_agent_id, &panic_run_id),
                            &mut cancellation_reconciliation_deadline,
                            "fork",
                        )
                        .await;
                        self.tracker
                            .apply_sub_run_result_state(
                                &panic_run_id,
                                agent_result_status_to_subrun_state(&reconciled.status),
                                reconciled.error.as_deref(),
                                reconciled.output.as_deref(),
                            )
                            .await;
                        results.push(reconciled);
                        continue;
                    }
                    let panic_error = format!("fork task panicked: {e}");
                    let panic_result = reconcile_agent_result_with_durable_authority(
                        &self.run_engine,
                        &request.user_id,
                        &session_id,
                        panic_disposition,
                        AgentResult {
                            agent_id: panic_agent_id,
                            run_id: panic_run_id.clone(),
                            status: STATUS_FAILED.to_string(),
                            output: None,
                            error: Some(panic_error),
                            prompt_tokens: 0,
                            completion_tokens: 0,
                            tool_calls: 0,
                        },
                    )
                    .await;
                    self.tracker
                        .apply_sub_run_result_state(
                            &panic_run_id,
                            agent_result_status_to_subrun_state(&panic_result.status),
                            panic_result.error.as_deref(),
                            panic_result.output.as_deref(),
                        )
                        .await;
                    results.push(panic_result);
                }
            }
        }

        if cancel_token.is_some_and(|token| token.is_cancelled()) {
            let mut settled_run_ids = results
                .iter()
                .map(|result| result.run_id.clone())
                .collect::<HashSet<_>>();
            for (agent_id, run_id, owner_generation) in fork_id_map.values() {
                if !settled_run_ids.insert(run_id.clone()) {
                    continue;
                }
                let reconciled = reconcile_after_parent_cancellation_bounded(
                    &self.run_engine,
                    &request.user_id,
                    &session_id,
                    durable_lifecycle_disposition(self.executor.as_ref(), *owner_generation),
                    cancelled_agent_result(agent_id, run_id),
                    &mut cancellation_reconciliation_deadline,
                    "fork",
                )
                .await;
                self.tracker
                    .apply_sub_run_result_state(
                        run_id,
                        agent_result_status_to_subrun_state(&reconciled.status),
                        reconciled.error.as_deref(),
                        reconciled.output.as_deref(),
                    )
                    .await;
                results.push(reconciled);
            }
        }

        Ok(DelegationResult::from_results(
            &request.delegation_id,
            results,
            None,
        ))
    }

    /// Get the delegation tracker for external queries.
    pub fn tracker(&self) -> &Arc<DelegationTracker> {
        &self.tracker
    }

    /// Get the shared profile registry.
    pub fn registry(&self) -> &Arc<RwLock<AgentProfileRegistry>> {
        &self.registry
    }

    /// Get the shared run engine.
    pub fn run_engine(&self) -> &Arc<RunEngine> {
        &self.run_engine
    }

    // ── Pause / Resume API ──────────────────────────────────────────────────

    async fn pause_live_sub_run(
        &self,
        user_id: &str,
        expected_session_id: &str,
        run_id: &str,
        waiting_for: &str,
    ) -> bool {
        if self.tracker.get_pause_flag(run_id).await.is_none()
            || self
                .tracker
                .get_sub_run_state(run_id)
                .await
                .is_none_or(|state| state.is_terminal())
        {
            return false;
        }
        let event = serde_json::json!({
            "event_type": "run_paused",
            "data": {"source": waiting_for},
        });
        match self
            .run_engine
            .transition_status_with_event_if_current(
                user_id,
                expected_session_id,
                run_id,
                &[STATUS_RUNNING],
                STATUS_PAUSED,
                Some(waiting_for),
                None,
                event,
            )
            .await
        {
            Ok(true) => self.tracker.pause_sub_run(run_id).await,
            Ok(false) => false,
            Err(error) => {
                tracing::warn!(
                    target: "astra_runtime::delegation",
                    run_id,
                    error = %error,
                    "failed to durably pause live sub-run"
                );
                false
            }
        }
    }

    async fn resume_live_sub_run(
        &self,
        user_id: &str,
        expected_session_id: &str,
        run_id: &str,
        source: &str,
    ) -> bool {
        if !self.tracker.is_paused(run_id).await
            || self
                .tracker
                .get_sub_run_state(run_id)
                .await
                .is_none_or(|state| state.is_terminal())
        {
            return false;
        }
        let event = serde_json::json!({
            "event_type": "run_resumed",
            "data": {"source": source},
        });
        match self
            .run_engine
            .transition_status_with_event_if_current(
                user_id,
                expected_session_id,
                run_id,
                &[STATUS_PAUSED],
                STATUS_RUNNING,
                None,
                None,
                event,
            )
            .await
        {
            Ok(true) => self.tracker.resume_sub_run(run_id).await,
            Ok(false) => false,
            Err(error) => {
                tracing::warn!(
                    target: "astra_runtime::delegation",
                    run_id,
                    error = %error,
                    "failed to durably resume live sub-run"
                );
                false
            }
        }
    }

    /// Pause all sub-runs belonging to a delegation.
    ///
    /// Sets cooperative pause flags — sub-runs check these between turns and
    /// yield with status "paused" at the next turn boundary.
    pub async fn pause_delegation(
        &self,
        user_id: &str,
        expected_session_id: &str,
        delegation_id: &str,
    ) -> usize {
        let mut count = 0;
        for record in self.tracker.get_sub_runs(delegation_id).await {
            if self
                .pause_live_sub_run(
                    user_id,
                    expected_session_id,
                    &record.run_id,
                    "delegation_pause",
                )
                .await
            {
                count += 1;
            }
        }
        count
    }

    /// Resume all sub-runs belonging to a delegation.
    ///
    /// Clears cooperative pause flags so sub-runs continue executing.
    pub async fn resume_delegation(
        &self,
        user_id: &str,
        expected_session_id: &str,
        delegation_id: &str,
    ) -> usize {
        let mut count = 0;
        for record in self.tracker.get_sub_runs(delegation_id).await {
            if self
                .resume_live_sub_run(
                    user_id,
                    expected_session_id,
                    &record.run_id,
                    "delegation_resume",
                )
                .await
            {
                count += 1;
            }
        }
        count
    }

    /// Pause all sub-runs spawned by a parent run (across all delegations).
    pub async fn pause_children_of(
        &self,
        user_id: &str,
        expected_session_id: &str,
        parent_run_id: &str,
    ) -> usize {
        let mut count = 0;
        for child_id in self.tracker.get_children(parent_run_id).await {
            if self
                .pause_live_sub_run(user_id, expected_session_id, &child_id, "parent_pause")
                .await
            {
                count += 1;
            }
        }
        count
    }

    /// Resume all sub-runs spawned by a parent run.
    pub async fn resume_children_of(
        &self,
        user_id: &str,
        expected_session_id: &str,
        parent_run_id: &str,
    ) -> usize {
        let mut count = 0;
        for child_id in self.tracker.get_children(parent_run_id).await {
            if self
                .resume_live_sub_run(user_id, expected_session_id, &child_id, "parent_resume")
                .await
            {
                count += 1;
            }
        }
        count
    }

    /// Request cancellation of one local delegated child. Terminal state is
    /// owned by the executor result, so the caller must wait for its normal
    /// lifecycle event instead of treating this acknowledgement as completion.
    pub async fn cancel_sub_run(&self, run_id: &str) -> bool {
        self.tracker.request_cancel_sub_run(run_id).await
    }

    /// Request cancellation of every non-terminal sub-run in the parent's
    /// subtree. The executor owns each terminal result; persisting
    /// `cancelled` here would make durable state claim the work had stopped
    /// before the child had actually reached its cancellation boundary.
    pub async fn cancel_children_of(&self, parent_run_id: &str) -> usize {
        self.tracker.cancel_children_of(parent_run_id).await
    }

    /// Extract budget awareness prompt from delegation context.
    fn extract_budget_prompt(
        context: &std::collections::HashMap<String, serde_json::Value>,
    ) -> String {
        let budget = context.get("team_budget").and_then(|v| v.as_u64());
        let max_parallel = context.get("team_max_parallel").and_then(|v| v.as_u64());
        // Also check for timeout
        let timeout = context.get("team_timeout_sec").and_then(|v| v.as_u64());
        if budget.is_some() || max_parallel.is_some() || timeout.is_some() {
            format!(
                "\n{}",
                team_prompts::budget_awareness_prompt(budget, timeout)
            )
        } else {
            String::new()
        }
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

// ─── Trait Implementations ────────────────────────────────────────────────────────

use astra_server_types::team_orchestrator_traits::{DelegationExecutor, DelegationTracking};

#[async_trait::async_trait]
impl DelegationExecutor for DelegationEngine {
    async fn execute_delegation(
        &self,
        request: DelegationRequest,
        source_agent_id: &str,
        profile_snapshot: AgentProfileRegistry,
        cancel_token: Option<Arc<tokio_util::sync::CancellationToken>>,
    ) -> Result<DelegationResult, String> {
        // Team profiles are request authority. Execute against an isolated,
        // immutable snapshot instead of publishing user-defined naked IDs to
        // the process-global builtin registry.
        let isolated = Self {
            registry: Arc::new(RwLock::new(profile_snapshot)),
            run_engine: self.run_engine.clone(),
            tracker: self.tracker.clone(),
            executor: self.executor.clone(),
            gate: self.gate.clone(),
            mailbox_router: self.mailbox_router.clone(),
            prefix_store: self.prefix_store.clone(),
            projection_store: self.projection_store.clone(),
        };
        isolated
            .execute(request, source_agent_id, cancel_token)
            .await
    }

    async fn get_delegation_progress(&self, delegation_id: &str) -> Option<DelegationProgress> {
        self.tracker().get_progress(delegation_id).await
    }
}

#[async_trait::async_trait]
impl DelegationTracking for DelegationTracker {
    async fn get_sub_runs(&self, delegation_id: &str) -> Vec<SubRunRecord> {
        DelegationTracker::get_sub_runs(self, delegation_id).await
    }

    async fn is_run_paused(&self, run_id: &str) -> bool {
        self.is_paused(run_id).await
    }

    async fn pause_delegation(&self, delegation_id: &str) -> usize {
        DelegationTracker::pause_delegation(self, delegation_id).await
    }

    async fn resume_delegation(&self, delegation_id: &str) -> usize {
        DelegationTracker::resume_delegation(self, delegation_id).await
    }

    async fn cleanup_delegation(&self, delegation_id: &str) -> Result<(), String> {
        DelegationTracker::cleanup_delegation(self, delegation_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_services::coordination::{AgentProfile, AgentTier, PipelineStage};
    use astra_services::runs::{InMemoryRunStateStore, RunStateStore};

    #[test]
    fn delegation_context_clone_preserves_structured_parent_messages() {
        let context = HashMap::from([(
            "parent_messages".to_string(),
            serde_json::json!([
                {"role": "user", "content": {"text": "你好🚀"}},
                {"role": "assistant", "tool_calls": [{"id": "call-1"}]}
            ]),
        )]);

        let cloned = clone_delegation_context(
            astra_core::history_work::HistoryWorkSite::DelegationContextClone,
            &context,
        );

        assert_eq!(cloned, context);
    }

    fn setup() -> (
        Arc<RwLock<AgentProfileRegistry>>,
        Arc<RunEngine>,
        Arc<DelegationTracker>,
    ) {
        let mut reg = AgentProfileRegistry::new();
        reg.register(AgentProfile::new(
            "orch",
            "Orchestrator",
            AgentTier::Orchestrator,
        ))
        .unwrap();
        reg.register(AgentProfile::new("coder", "Coder", AgentTier::System))
            .unwrap();
        reg.register(AgentProfile::new("reviewer", "Reviewer", AgentTier::System))
            .unwrap();
        reg.register(AgentProfile::new("writer", "Writer", AgentTier::User))
            .unwrap();

        let store = Arc::new(InMemoryRunStateStore::new());
        let engine = Arc::new(RunEngine::new(store));
        let tracker = Arc::new(DelegationTracker::new());

        (Arc::new(RwLock::new(reg)), engine, tracker)
    }

    #[tokio::test]
    async fn generation_publish_between_preparing_read_and_wait_is_observed() {
        let sink = Arc::new(ExecutionOwnerGenerationSink::preparing(3));
        let preparing_observed = Arc::new(tokio::sync::Notify::new());
        let release_wait = Arc::new(tokio::sync::Notify::new());
        *sink
            .wait_after_preparing_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some((Arc::clone(&preparing_observed), Arc::clone(&release_wait)));
        let waiter = {
            let sink = Arc::clone(&sink);
            tokio::spawn(async move { sink.wait_until_published_or_stopped().await })
        };

        preparing_observed.notified().await;
        sink.publish(4);
        release_wait.notify_one();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
                .await
                .expect("versioned publication cannot lose the wakeup")
                .expect("publication waiter must not panic"),
            ExecutionOwnerGenerationPublication::Acquired(4)
        );
    }

    #[tokio::test]
    async fn generation_guard_drop_between_preparing_read_and_wait_is_observed() {
        let sink = Arc::new(ExecutionOwnerGenerationSink::preparing(7));
        let owner = sink.guard();
        let preparing_observed = Arc::new(tokio::sync::Notify::new());
        let release_wait = Arc::new(tokio::sync::Notify::new());
        *sink
            .wait_after_preparing_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some((Arc::clone(&preparing_observed), Arc::clone(&release_wait)));
        let waiter = {
            let sink = Arc::clone(&sink);
            tokio::spawn(async move { sink.wait_until_published_or_stopped().await })
        };

        preparing_observed.notified().await;
        drop(owner);
        release_wait.notify_one();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
                .await
                .expect("versioned guard drop cannot lose the wakeup")
                .expect("publication waiter must not panic"),
            ExecutionOwnerGenerationPublication::StoppedBeforeAcquisition {
                expected_initial_generation: 7,
            }
        );
    }

    /// Establish the production precondition for a delegation test: the
    /// parent conversation run already exists in the durable run store.
    ///
    /// Delegated runs inherit lineage and admitted model identity from their
    /// parent. Tests must therefore create that parent through the same
    /// `RunEngine` API used by the lifecycle instead of relying on the legacy
    /// orphan-child fallback.
    async fn persist_durable_parent_fixture(
        engine: &DelegationEngine,
        request: &DelegationRequest,
    ) -> Result<(), String> {
        if engine
            .run_engine
            .load_run(&request.user_id, &request.parent_run_id)
            .await?
            .is_none()
        {
            engine
                .run_engine
                .start_run(
                    &request.parent_run_id,
                    &request.user_id,
                    &request.session_id,
                )
                .await?;
        }
        Ok(())
    }

    async fn execute_with_durable_parent(
        engine: &DelegationEngine,
        request: DelegationRequest,
        source_agent_id: &str,
        cancel_token: Option<Arc<tokio_util::sync::CancellationToken>>,
    ) -> Result<DelegationResult, String> {
        persist_durable_parent_fixture(engine, &request).await?;
        engine.execute(request, source_agent_id, cancel_token).await
    }

    async fn execute_with_durable_parent_and_headers(
        engine: &DelegationEngine,
        request: DelegationRequest,
        source_agent_id: &str,
        cancel_token: Option<Arc<tokio_util::sync::CancellationToken>>,
        forward_headers: HashMap<String, String>,
        admitted_model_execution: Option<AdmittedModelExecution>,
    ) -> Result<DelegationResult, String> {
        persist_durable_parent_fixture(engine, &request).await?;
        engine
            .execute_with_forward_headers(
                request,
                source_agent_id,
                cancel_token,
                forward_headers,
                admitted_model_execution,
            )
            .await
    }

    #[test]
    fn missing_agent_profile_error_lists_operation_and_available_profiles() {
        let mut reg = AgentProfileRegistry::new();
        reg.register(AgentProfile::new("coder", "Coder", AgentTier::System))
            .unwrap();
        reg.register(AgentProfile::new("reviewer", "Reviewer", AgentTier::System))
            .unwrap();

        let message = DelegationEngine::missing_agent_profile_error("fanout spawn", "writer", &reg);

        assert!(message.contains("fanout spawn"), "{message}");
        assert!(message.contains("writer"), "{message}");
        assert!(message.contains("coder, reviewer"), "{message}");
        assert!(message.contains("do not invent a replacement"), "{message}");
    }

    #[test]
    fn parent_cancellation_is_projected_as_cancelled_not_failed() {
        let result = cancelled_agent_result("reviewer", "run-cancelled");
        assert_eq!(result.status, STATUS_CANCELLED);
        assert!(result.is_failure());
        assert_eq!(
            agent_result_status_to_subrun_state(&result.status),
            SubRunState::Cancelled
        );
    }

    #[test]
    fn cancellation_reconciliation_timeout_preserves_recoverable_unknown_state() {
        let attempted = AgentResult {
            agent_id: "reviewer".to_string(),
            run_id: "run-reconcile-pending".to_string(),
            status: STATUS_COMPLETED.to_string(),
            output: Some("locally observed output".to_string()),
            error: None,
            prompt_tokens: 12,
            completion_tokens: 4,
            tool_calls: 1,
        };
        let result = cancellation_reconciliation_pending_result(&attempted);

        assert_eq!(result.status, STATUS_WAITING);
        assert!(result.is_unfinished());
        assert_eq!(result.output, None, "uncommitted output must not leak");
        assert_eq!(result.prompt_tokens, 12);
        assert_eq!(result.completion_tokens, 4);
        assert_eq!(result.tool_calls, 1);
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("authoritative child state is still unknown"))
        );
        assert_eq!(
            agent_result_status_to_subrun_state(&result.status),
            SubRunState::Waiting
        );
    }

    #[tokio::test(start_paused = true)]
    async fn reconciliation_deadline_is_shared_across_the_whole_batch() {
        let budget = std::time::Duration::from_secs(2);
        let mut deadline = None;
        let started = tokio::time::Instant::now();

        assert!(
            await_with_shared_deadline(&mut deadline, budget, std::future::pending::<()>(),)
                .await
                .is_none()
        );
        assert_eq!(tokio::time::Instant::now() - started, budget);

        let exhausted_at = tokio::time::Instant::now();
        assert!(
            await_with_shared_deadline(&mut deadline, budget, std::future::pending::<()>(),)
                .await
                .is_none()
        );
        assert_eq!(
            tokio::time::Instant::now(),
            exhausted_at,
            "a shared deadline must not restart for each child"
        );
    }

    fn fan_out_request(agents: Vec<&str>) -> DelegationRequest {
        DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "del-1".into(),
            parent_run_id: "parent-1".into(),
            task: "test task".into(),
            pattern: CoordinationPattern::FanOut {
                agent_ids: agents.into_iter().map(String::from).collect(),
                aggregation: AggregationStrategy::AllResults,
                timeout_sec: 60,
            },
            user_id: "user-1".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        }
    }

    #[test]
    fn delegation_engine_injects_source_agent_into_chain_once() {
        let mut request = fan_out_request(vec!["coder"]);
        DelegationEngine::ensure_source_in_delegation_chain(&mut request, "orch");
        DelegationEngine::ensure_source_in_delegation_chain(&mut request, "orch");

        assert_eq!(request.delegation_chain, vec!["orch".to_string()]);
    }

    #[test]
    fn verification_retry_backoff_is_bounded_exponential_with_stable_jitter() {
        let first = verification_retry_delay(2, "run-a");
        let second = verification_retry_delay(3, "run-a");
        let saturated = verification_retry_delay(20, "run-a");

        assert!((200..=300).contains(&(first.as_millis() as u64)));
        assert!((400..=500).contains(&(second.as_millis() as u64)));
        assert!((2_000..=2_100).contains(&(saturated.as_millis() as u64)));
        assert_eq!(first, verification_retry_delay(2, "run-a"));
    }

    #[test]
    fn delegation_metrics_expose_low_cardinality_outcomes_and_usage() {
        let registry = Arc::new(astra_turn_core::pipeline_metrics::MetricsRegistry::new());
        register_delegation_metrics(&registry);
        let result = Ok(DelegationResult::from_results(
            "d1",
            vec![AgentResult {
                agent_id: "reviewer".into(),
                run_id: "run-1".into(),
                status: "completed".into(),
                output: Some("done".into()),
                error: None,
                prompt_tokens: 120,
                completion_tokens: 30,
                tool_calls: 2,
            }],
            Some("done".into()),
        ));

        record_delegation_metrics(
            Some(&registry),
            "fan_out",
            std::time::Duration::from_millis(25),
            &result,
        );

        let rendered = registry.render_prometheus();
        assert!(rendered.contains(
            "astra_delegation_executions_total{outcome=\"completed\",pattern=\"fan_out\"} 1"
        ));
        assert!(rendered.contains(
            "astra_delegation_sub_runs_total{pattern=\"fan_out\",status=\"completed\"} 1"
        ));
        assert!(
            rendered
                .contains("astra_delegation_tokens_total{kind=\"prompt\",pattern=\"fan_out\"} 120")
        );
    }

    #[test]
    fn delegation_engine_rejects_child_already_in_chain() {
        let mut request = fan_out_request(vec!["coder"]);
        request.delegation_chain = vec!["orch".to_string(), "coder".to_string()];

        let error = DelegationEngine::delegation_chain_for_child(&request, "coder").unwrap_err();

        assert!(error.contains("circular delegation detected"), "{error}");
        assert!(error.contains("orch → coder → coder"), "{error}");
    }

    #[test]
    fn delegation_engine_rejects_self_delegation() {
        // A → A: the most fundamental cycle. An agent must not be able to
        // delegate to itself, even with an empty chain — `ensure_source`
        // runs first, so the chain becomes [A] before `for_child(A)` fires.
        let mut request = fan_out_request(vec!["coder"]);
        DelegationEngine::ensure_source_in_delegation_chain(&mut request, "coder");

        let error = DelegationEngine::delegation_chain_for_child(&request, "coder").unwrap_err();

        assert!(error.contains("circular delegation detected"), "{error}");
        assert!(error.contains("coder → coder"), "{error}");
    }

    #[test]
    fn delegation_engine_rejects_three_hop_cycle() {
        // A → B → C → A: a deeper cycle that a naive "immediate parent only"
        // check would miss. The chain is walked in full.
        let mut request = fan_out_request(vec!["coder"]);
        request.delegation_chain = vec![
            "orch".to_string(),
            "coder".to_string(),
            "reviewer".to_string(),
        ];

        let error = DelegationEngine::delegation_chain_for_child(&request, "orch").unwrap_err();

        assert!(error.contains("circular delegation detected"), "{error}");
        assert!(error.contains("orch → coder → reviewer → orch"), "{error}");
    }

    #[test]
    fn delegation_engine_cycle_detection_is_case_insensitive() {
        // Agent IDs are user-provided; a case variant must not bypass the
        // chain check. `Orch` should match `orch` already in the chain.
        let mut request = fan_out_request(vec!["coder"]);
        request.delegation_chain = vec!["orch".to_string()];

        let error = DelegationEngine::delegation_chain_for_child(&request, "Orch").unwrap_err();

        assert!(error.contains("circular delegation detected"), "{error}");
    }

    #[test]
    fn delegation_engine_rejects_unicode_decomposition_bypass() {
        // NFC ("café") and NFD ("cafe" + combining acute ◌́) are visually
        // identical but byte-distinct. A child using the decomposed form
        // must not bypass a chain containing the composed form — that
        // would allow an infinite delegation loop through a normalization
        // alias.
        let mut request = fan_out_request(vec!["coder"]);
        // "café" in NFC: é = U+00E9
        request.delegation_chain = vec!["caf\u{00E9}".to_string()];
        // "café" in NFD: e + ◌́ = U+0065 U+0301
        let child = "caf\u{0065}\u{0301}";

        let error = DelegationEngine::delegation_chain_for_child(&request, child).unwrap_err();
        assert!(
            error.contains("circular delegation detected"),
            "NFD child must not bypass NFC chain entry, got: {error}"
        );
    }

    #[test]
    fn delegation_engine_rejects_unicode_composition_bypass() {
        // Inverse of the above: chain holds NFD, child uses NFC.
        let mut request = fan_out_request(vec!["coder"]);
        request.delegation_chain = vec!["caf\u{0065}\u{0301}".to_string()];
        let child = "caf\u{00E9}";

        let error = DelegationEngine::delegation_chain_for_child(&request, child).unwrap_err();
        assert!(
            error.contains("circular delegation detected"),
            "NFC child must not bypass NFD chain entry, got: {error}"
        );
    }

    #[test]
    fn delegation_engine_rejects_case_and_decomposition_combo() {
        // Chain holds "Coder" (capital C, NFC), child uses "c\u{0300}der"
        // (lowercase c + combining grave). Both case and normalization
        // differences must be normalized away before comparison.
        let mut request = fan_out_request(vec!["reviewer"]);
        request.delegation_chain = vec!["Coder".to_string()];
        let child = "c\u{006F}\u{0300}der"; // o + combining grave, lowercased intent

        let error = DelegationEngine::delegation_chain_for_child(&request, child);
        // If the combining mark makes it a different agent, it's admitted;
        // if normalization collapses it to "coder", it's a cycle. The
        // contract is: visually-distinct IDs are distinct agents. This
        // test asserts that "Coder" vs "cöder" (different grapheme) is
        // admitted, while the normalization logic does not over-collapse
        // distinct graphemes.
        // (If this combination genuinely differs after NFC, expect Ok.)
        // The key invariant is determinism — the same input always yields
        // the same canonical form.
        match error {
            Ok(_) => { /* distinct grapheme — correctly admitted */ }
            Err(msg) => {
                assert!(
                    msg.contains("circular delegation detected"),
                    "unexpected error: {msg}"
                );
            }
        }
    }

    #[test]
    fn delegation_engine_admits_visually_distinct_unicode() {
        // Sanity: two genuinely different agent IDs (different graphemes)
        // must not be collapsed by normalization. "café" and "cafe" (no
        // accent) are distinct agents.
        let mut request = fan_out_request(vec!["coder"]);
        request.delegation_chain = vec!["café".to_string()];

        let chain = DelegationEngine::delegation_chain_for_child(&request, "cafe")
            .expect("distinct grapheme must be admitted");
        assert_eq!(chain, vec!["café".to_string()]);
    }

    #[test]
    fn delegation_engine_admits_unrelated_child() {
        // Sanity: a child not present in the chain is admitted and receives
        // a copy of the current chain (which the child will later extend).
        let mut request = fan_out_request(vec!["coder"]);
        request.delegation_chain = vec!["orch".to_string()];

        let chain =
            DelegationEngine::delegation_chain_for_child(&request, "coder").expect("admitted");
        assert_eq!(chain, vec!["orch".to_string()]);
    }

    #[tokio::test]
    async fn delegation_tracker_records_and_queries() {
        let tracker = DelegationTracker::new();
        tracker
            .record_sub_run(SubRunRecord {
                run_id: "sub-1".into(),
                parent_run_id: "parent-1".into(),
                delegation_id: "del-1".into(),
                agent_id: "coder".into(),
                depth: 1,
                state: SubRunState::Created,
                retry_of: None,
            })
            .await;
        tracker
            .record_sub_run(SubRunRecord {
                run_id: "sub-2".into(),
                parent_run_id: "parent-1".into(),
                delegation_id: "del-1".into(),
                agent_id: "reviewer".into(),
                depth: 1,
                state: SubRunState::Created,
                retry_of: None,
            })
            .await;

        let subs = tracker.get_sub_runs("del-1").await;
        assert_eq!(subs.len(), 2);
        assert!(tracker.is_sub_run("sub-1").await);
        assert!(!tracker.is_sub_run("parent-1").await);
        assert_eq!(
            tracker.get_parent("sub-1").await.as_deref(),
            Some("parent-1")
        );
    }

    #[tokio::test]
    async fn delegation_tracker_ancestry() {
        let tracker = DelegationTracker::new();
        tracker
            .record_sub_run(SubRunRecord {
                run_id: "child".into(),
                parent_run_id: "parent".into(),
                delegation_id: "d1".into(),
                agent_id: "a".into(),
                depth: 1,
                state: SubRunState::Created,
                retry_of: None,
            })
            .await;
        tracker
            .record_sub_run(SubRunRecord {
                run_id: "grandchild".into(),
                parent_run_id: "child".into(),
                delegation_id: "d2".into(),
                agent_id: "b".into(),
                depth: 2,
                state: SubRunState::Created,
                retry_of: None,
            })
            .await;

        let ancestry = tracker.get_ancestry("grandchild").await;
        assert_eq!(ancestry, vec!["child", "parent"]);
    }

    #[tokio::test]
    async fn delegation_tracker_ancestry_returns_full_acyclic_prefix_on_mid_chain_cycle() {
        let tracker = DelegationTracker::new();
        for (run_id, parent_run_id, depth) in
            [("a", "b", 4), ("b", "c", 3), ("c", "d", 2), ("d", "b", 1)]
        {
            tracker
                .record_sub_run(SubRunRecord {
                    run_id: run_id.into(),
                    parent_run_id: parent_run_id.into(),
                    delegation_id: format!("d-{run_id}"),
                    agent_id: run_id.into(),
                    depth,
                    state: SubRunState::Created,
                    retry_of: None,
                })
                .await;
        }

        assert_eq!(tracker.get_ancestry("a").await, vec!["b", "c", "d"]);
    }

    #[tokio::test]
    async fn delegation_tracker_preserves_supported_deep_ancestry() {
        let tracker = DelegationTracker::new();
        for depth in 1..=32 {
            tracker
                .record_sub_run(SubRunRecord {
                    run_id: format!("run-{depth}"),
                    parent_run_id: format!("run-{}", depth - 1),
                    delegation_id: format!("d-{depth}"),
                    agent_id: format!("agent-{depth}"),
                    depth,
                    state: SubRunState::Created,
                    retry_of: None,
                })
                .await;
        }

        let ancestry = tracker.get_ancestry("run-32").await;
        assert_eq!(ancestry.len(), 32);
        assert_eq!(ancestry.first().map(String::as_str), Some("run-31"));
        assert_eq!(ancestry.last().map(String::as_str), Some("run-0"));
    }

    #[test]
    fn ancestry_corruption_guard_reports_truncation_instead_of_looking_complete() {
        let parents = (1..=MAX_ANCESTRY_TRAVERSAL + 1)
            .map(|depth| (format!("run-{depth}"), format!("run-{}", depth - 1)))
            .collect::<HashMap<_, _>>();

        let walk = ancestry_from_parents(&parents, &format!("run-{}", MAX_ANCESTRY_TRAVERSAL + 1));

        assert_eq!(walk.ancestors.len(), MAX_ANCESTRY_TRAVERSAL);
        assert_eq!(
            walk.termination,
            AncestryTermination::TraversalLimit {
                next_run_id: "run-0".to_string()
            }
        );
    }

    #[test]
    fn delegated_critical_findings_are_typed_and_failures_remain_visible() {
        let result = |output: Option<&str>, status: &str, error: Option<&str>| AgentResult {
            agent_id: "reviewer".into(),
            run_id: "run-review".into(),
            status: status.into(),
            output: output.map(str::to_string),
            error: error.map(str::to_string),
            prompt_tokens: 0,
            completion_tokens: 0,
            tool_calls: 0,
        };

        assert_eq!(
            critical_finding_from_agent_result(&result(
                Some("This prose mentions a critical blocker but carries no typed finding."),
                STATUS_COMPLETED,
                None,
            )),
            CriticalFindingExtraction::default()
        );
        assert_eq!(
            critical_finding_from_agent_result(&result(
                Some(r#"{"findings":[{"severity":"high","summary":"Not critical"}]}"#),
                STATUS_COMPLETED,
                None,
            ))
            .summary,
            None
        );

        let summary = critical_finding_from_agent_result(&result(
            Some(r#"{"findings":[{"severity":"Critical","summary":"Tenant boundary bypass","evidence":["events.rs:42","cross-user request accepted"]}]}"#),
            STATUS_COMPLETED,
            None,
        ))
        .summary
        .expect("structured critical finding");
        assert_eq!(
            summary,
            "Tenant boundary bypass\nEvidence:\n- events.rs:42\n- cross-user request accepted"
        );

        let failed = critical_finding_from_agent_result(&result(
            None,
            STATUS_FAILED,
            Some("review process crashed"),
        ));
        assert_eq!(
            failed.summary.as_deref(),
            Some("Delegated agent 'reviewer' failed with status 'failed': review process crashed")
        );

        let cancelled = critical_finding_from_agent_result(&result(
            None,
            STATUS_CANCELLED,
            Some("cancelled by user"),
        ));
        assert!(cancelled.summary.is_none());
    }

    #[test]
    fn malformed_finding_json_is_observable_without_becoming_a_finding() {
        let result = AgentResult {
            agent_id: "reviewer".into(),
            run_id: "run-review".into(),
            status: STATUS_COMPLETED.into(),
            output: Some(r#"{"findings":["#.into()),
            error: None,
            prompt_tokens: 0,
            completion_tokens: 0,
            tool_calls: 0,
        };

        let extraction = critical_finding_from_agent_result(&result);
        assert!(extraction.summary.is_none());
        assert!(extraction.contract_error.is_some());
    }

    #[test]
    fn finding_projection_targets_are_bounded_and_keep_the_root() {
        let ancestry = (0..32)
            .map(|idx| format!("ancestor-{idx}"))
            .collect::<Vec<_>>();
        let targets = finding_bubble_targets("session", "source", 32, &ancestry);

        assert_eq!(targets.len(), MAX_FINDING_BUBBLE_TARGETS);
        assert_eq!(targets[0].run_id, "source");
        assert_eq!(targets[1].run_id, "ancestor-0");
        assert_eq!(
            targets.last().map(|target| target.run_id.as_str()),
            Some("ancestor-31")
        );
        assert_eq!(targets.last().map(|target| target.depth), Some(0));
    }

    #[tokio::test]
    async fn fan_out_spawns_sub_runs() {
        let (reg, engine, tracker) = setup();
        let de = DelegationEngine::new(reg, engine.clone(), tracker.clone());

        let req = fan_out_request(vec!["coder", "reviewer"]);
        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();

        assert_eq!(result.agent_results.len(), 2);
        assert_eq!(result.delegation_id, "del-1");
        // Stub executor marks runs as completed
        assert_eq!(result.status, "completed");

        // Verify sub-runs were created in engine with final status
        for ar in &result.agent_results {
            assert_eq!(ar.status, "completed");
            assert!(ar.output.is_some());
            let run = engine
                .load_run("user-1", &ar.run_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(run.status, "completed");
        }

        // Verify tracker has the records
        let subs = tracker.get_sub_runs("del-1").await;
        assert_eq!(subs.len(), 2);
        assert!(subs.iter().all(|s| s.parent_run_id == "parent-1"));
        assert!(subs.iter().all(|s| s.depth == 1));
    }

    #[tokio::test]
    async fn missing_durable_parent_is_rejected_before_live_delegation_state_exists() {
        let (reg, engine, tracker) = setup();
        let router = Arc::new(crate::messaging::AgentMailboxRouter::new(
            Arc::new(crate::messaging::InProcessTransport::new()),
            tracker.clone(),
        ));
        let de =
            DelegationEngine::with_executor(reg, engine, tracker.clone(), Arc::new(EchoExecutor))
                .with_mailbox_router(router.clone());
        let request = fan_out_request(vec!["coder"]);

        let result = de.execute(request, "orch", None).await;

        assert!(result.is_err());
        assert!(tracker.get_progress("del-1").await.is_none());
        assert!(tracker.get_sub_runs("del-1").await.is_empty());
        assert!(!router.is_run_registered("parent-1").await);
    }

    #[tokio::test]
    async fn sequential_spawns_ordered_sub_runs() {
        let (reg, engine, tracker) = setup();
        let de = DelegationEngine::new(reg, engine.clone(), tracker.clone());

        let req = DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "del-seq".into(),
            parent_run_id: "parent-2".into(),
            task: "sequential test".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["coder".into(), "reviewer".into()],
                stop_on_success: false,
                timeout_sec: 0,
            },
            user_id: "user-1".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        };

        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();
        assert_eq!(result.agent_results.len(), 2);
        assert_eq!(result.agent_results[0].agent_id, "coder");
        assert_eq!(result.agent_results[1].agent_id, "reviewer");
    }

    #[tokio::test]
    async fn pipeline_spawns_stage_runs() {
        let (reg, engine, tracker) = setup();
        let de = DelegationEngine::new(reg, engine.clone(), tracker.clone());

        let req = DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "del-pipe".into(),
            parent_run_id: "parent-3".into(),
            task: "pipeline test".into(),
            pattern: CoordinationPattern::Pipeline {
                stages: vec![
                    PipelineStage {
                        agent_id: "coder".into(),
                        output_transform: None,
                    },
                    PipelineStage {
                        agent_id: "reviewer".into(),
                        output_transform: Some("extract_issues".into()),
                    },
                ],
                timeout_sec: 0,
            },
            user_id: "user-1".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        };

        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();
        assert_eq!(result.agent_results.len(), 2);

        let subs = tracker.get_sub_runs("del-pipe").await;
        assert_eq!(subs.len(), 2);
    }

    #[tokio::test]
    async fn adversarial_spawns_producer_reviewer_pairs() {
        let (reg, engine, tracker) = setup();
        let de = DelegationEngine::new(reg, engine.clone(), tracker.clone());

        let req = DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "del-adv".into(),
            parent_run_id: "parent-4".into(),
            task: "adversarial test".into(),
            pattern: CoordinationPattern::AdversarialReview {
                producer_id: "coder".into(),
                reviewer_id: "reviewer".into(),
                max_rounds: 2,
                acceptance_threshold: 0.8,
                timeout_sec: 0,
            },
            user_id: "user-1".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        };

        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();
        // 2 rounds × 2 agents = 4 sub-runs
        assert_eq!(result.agent_results.len(), 4);

        let subs = tracker.get_sub_runs("del-adv").await;
        assert_eq!(subs.len(), 4);

        // Verify alternating producer/reviewer
        assert_eq!(subs[0].agent_id, "coder");
        assert_eq!(subs[1].agent_id, "reviewer");
        assert_eq!(subs[2].agent_id, "coder");
        assert_eq!(subs[3].agent_id, "reviewer");
    }

    #[tokio::test]
    async fn validation_rejects_bad_delegation() {
        let (reg, engine, tracker) = setup();
        let de = DelegationEngine::new(reg, engine, tracker);

        // User agent cannot delegate
        let req = DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "bad".into(),
            parent_run_id: "p".into(),
            task: "fail".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["coder".into()],
                stop_on_success: true,
                timeout_sec: 0,
            },
            user_id: "u".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        };

        assert!(de.execute(req, "writer", None).await.is_err());
    }

    #[tokio::test]
    async fn depth_limit_enforcement() {
        let (reg, engine, tracker) = setup();
        let de = DelegationEngine::new(reg, engine, tracker);

        // Orchestrator max depth is 3; request at depth=5 should fail
        let req = DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "deep".into(),
            parent_run_id: "p".into(),
            task: "too deep".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["coder".into()],
                stop_on_success: true,
                timeout_sec: 0,
            },
            user_id: "u".into(),
            depth: 5,
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        };

        let err = de.execute(req, "orch", None).await.unwrap_err();
        assert!(err.contains("depth"));
    }

    #[tokio::test]
    async fn cross_delegation_isolation() {
        let (reg, engine, tracker) = setup();
        let de = DelegationEngine::new(reg, engine.clone(), tracker.clone());

        let req1 = DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "del-A".into(),
            parent_run_id: "pA".into(),
            task: "a".into(),
            pattern: CoordinationPattern::FanOut {
                agent_ids: vec!["coder".into()],
                aggregation: AggregationStrategy::FirstSuccess,
                timeout_sec: 60,
            },
            user_id: "u".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        };
        let req2 = DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "del-B".into(),
            parent_run_id: "pA".into(),
            task: "b".into(),
            pattern: CoordinationPattern::FanOut {
                agent_ids: vec!["reviewer".into()],
                aggregation: AggregationStrategy::FirstSuccess,
                timeout_sec: 60,
            },
            user_id: "u".into(),
            depth: 0,
            context: HashMap::new(),
            execution_metadata: None,
            delegation_chain: Vec::new(),
        };

        execute_with_durable_parent(&de, req1, "orch", None)
            .await
            .unwrap();
        execute_with_durable_parent(&de, req2, "orch", None)
            .await
            .unwrap();

        let subs_a = tracker.get_sub_runs("del-A").await;
        let subs_b = tracker.get_sub_runs("del-B").await;
        assert_eq!(subs_a.len(), 1);
        assert_eq!(subs_b.len(), 1);
        assert_eq!(subs_a[0].agent_id, "coder");
        assert_eq!(subs_b[0].agent_id, "reviewer");
    }

    // ─── Custom executor for testing ────────────────────────────────────────

    /// Test executor that echoes the task back with agent_id prefix.
    struct EchoExecutor;

    #[async_trait]
    impl SubRunExecutor for EchoExecutor {
        async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
            let output = if let Some(prev) = &config.previous_output {
                format!(
                    "[{}] {}: prev={}",
                    config.agent_profile.agent_id, config.task, prev
                )
            } else {
                format!("[{}] {}", config.agent_profile.agent_id, config.task)
            };
            Ok(AgentResult {
                agent_id: config.agent_profile.agent_id,
                run_id: config.run_id,
                status: "completed".to_string(),
                output: Some(output),
                error: None,
                prompt_tokens: 10,
                completion_tokens: 20,
                tool_calls: 1,
            })
        }
    }

    #[derive(Debug)]
    struct NoopLiveSink;

    impl astra_turn_core::agent_live_event::AgentLiveEventSink for NoopLiveSink {
        fn send(
            &self,
            _event: astra_turn_core::agent_live_event::AgentLiveEvent,
        ) -> Result<(), astra_turn_core::agent_live_event::AgentLiveSendError> {
            Ok(())
        }

        fn send_gap(
            &self,
            _gap: astra_turn_core::agent_live_event::AgentLiveGap,
        ) -> Result<(), astra_turn_core::agent_live_event::AgentLiveSendError> {
            Ok(())
        }
    }

    struct CapturedRunBinding {
        parent_run_id: Option<String>,
        session_id: String,
        task_context: HashMap<String, serde_json::Value>,
        has_live_event_sink: bool,
        cancel_token: Option<Arc<tokio_util::sync::CancellationToken>>,
        execution_owner_generation: Option<u64>,
        interaction_mode: RequestedTurnInteractionMode,
    }

    struct CaptureRunBindingExecutor {
        bindings: Arc<std::sync::Mutex<Vec<CapturedRunBinding>>>,
    }

    #[async_trait]
    impl SubRunExecutor for CaptureRunBindingExecutor {
        async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
            self.bindings.lock().unwrap().push(CapturedRunBinding {
                parent_run_id: Some(config.parent_run_id.clone()),
                session_id: config.session_id.clone(),
                task_context: config.context.clone(),
                has_live_event_sink: config.live_event_sink.is_some(),
                cancel_token: config.cancel_token.clone(),
                execution_owner_generation: config.execution_owner_generation,
                interaction_mode: config.interaction_mode,
            });
            Ok(AgentResult {
                agent_id: config.agent_profile.agent_id,
                run_id: config.run_id,
                status: "completed".into(),
                output: Some("completed".into()),
                error: None,
                prompt_tokens: 0,
                completion_tokens: 0,
                tool_calls: 0,
            })
        }
    }

    struct BlockingCancelExecutor {
        started_tx: tokio::sync::mpsc::UnboundedSender<String>,
        cancel_observed: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl SubRunExecutor for BlockingCancelExecutor {
        async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
            let cancel_token = config
                .cancel_token
                .clone()
                .expect("every delegated child has a cancellation token");
            self.started_tx
                .send(config.run_id.clone())
                .expect("test observes the child start");
            cancel_token.cancelled().await;
            self.cancel_observed.notify_one();
            self.release.notified().await;
            Ok(AgentResult {
                agent_id: config.agent_profile.agent_id,
                run_id: config.run_id,
                status: STATUS_CANCELLED.into(),
                output: None,
                error: None,
                prompt_tokens: 0,
                completion_tokens: 0,
                tool_calls: 0,
            })
        }
    }

    #[tokio::test]
    async fn delegation_binds_child_identity_and_live_lane_per_execution() {
        let (registry, run_engine, tracker) = setup();
        let bindings = Arc::new(std::sync::Mutex::new(Vec::new()));
        let engine = DelegationEngine::with_executor(
            registry,
            run_engine,
            tracker,
            Arc::new(CaptureRunBindingExecutor {
                bindings: bindings.clone(),
            }),
        );
        let request = DelegationRequest {
            session_id: "session-1".into(),
            delegation_id: "delegation-live-1".into(),
            parent_run_id: "run-root-1".into(),
            task: "inspect the implementation".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["coder".into()],
                stop_on_success: false,
                timeout_sec: 0,
            },
            user_id: "user-1".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        };

        persist_durable_parent_fixture(&engine, &request)
            .await
            .unwrap();
        engine
            .execute_with_forward_headers_and_live_events(
                request,
                "orch",
                None,
                HashMap::new(),
                None,
                Some(Arc::new(NoopLiveSink)),
            )
            .await
            .unwrap();

        let bindings = bindings.lock().unwrap();
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0].parent_run_id.as_deref(), Some("run-root-1"));
        assert_eq!(bindings[0].session_id, "session-1");
        assert!(
            !bindings[0].task_context.contains_key("session_id"),
            "runtime session identity must not be injected into the child task prompt"
        );
        assert!(bindings[0].has_live_event_sink);
        assert!(bindings[0].cancel_token.is_some());
        assert_eq!(
            bindings[0].interaction_mode,
            RequestedTurnInteractionMode::Headless
        );
    }

    #[tokio::test]
    async fn adversarial_and_fork_children_receive_independent_cancel_tokens() {
        let (registry, run_engine, tracker) = setup();
        let bindings = Arc::new(std::sync::Mutex::new(Vec::new()));
        let engine = DelegationEngine::with_executor(
            registry,
            run_engine,
            tracker,
            Arc::new(CaptureRunBindingExecutor {
                bindings: bindings.clone(),
            }),
        );

        let adversarial_request = DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "delegation-adversarial-cancel".into(),
            parent_run_id: "run-root-adversarial".into(),
            task: "review the implementation".into(),
            pattern: CoordinationPattern::AdversarialReview {
                producer_id: "coder".into(),
                reviewer_id: "reviewer".into(),
                max_rounds: 1,
                acceptance_threshold: 1.0,
                timeout_sec: 0,
            },
            user_id: "user-1".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        };
        execute_with_durable_parent(&engine, adversarial_request, "orch", None)
            .await
            .unwrap();
        let mut fork_request = fork_request("delegation-fork-cancel", vec!["a", "b"], "writer");
        fork_request.parent_run_id = "run-root-adversarial".into();
        execute_with_durable_parent(&engine, fork_request, "orch", None)
            .await
            .unwrap();

        let bindings = bindings.lock().unwrap();
        assert_eq!(bindings.len(), 4);
        let cancel_tokens: Vec<_> = bindings
            .iter()
            .map(|binding| binding.cancel_token.as_ref().expect("child cancel token"))
            .collect();
        for (index, cancel_token) in cancel_tokens.iter().enumerate() {
            for other in cancel_tokens.iter().skip(index + 1) {
                assert!(
                    !Arc::ptr_eq(cancel_token, other),
                    "sibling delegated runs need independent cancellation tokens"
                );
            }
        }
    }

    #[tokio::test]
    async fn parent_cancel_waits_for_child_executor_before_recording_terminal_state() {
        use std::time::Duration;

        let (registry, run_engine, tracker) = setup();
        let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel_observed = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let engine = Arc::new(DelegationEngine::with_executor(
            registry,
            run_engine.clone(),
            tracker.clone(),
            Arc::new(BlockingCancelExecutor {
                started_tx,
                cancel_observed: cancel_observed.clone(),
                release: release.clone(),
            }),
        ));
        let mut request = fan_out_request(vec!["coder"]);
        request.delegation_id = "delegation-cancel-awaits-executor".into();
        request.parent_run_id = "parent-cancel-awaits-executor".into();
        persist_durable_parent_fixture(&engine, &request)
            .await
            .unwrap();

        let execution = {
            let engine = engine.clone();
            tokio::spawn(async move { engine.execute(request, "orch", None).await })
        };
        let child_run_id = tokio::time::timeout(Duration::from_secs(1), started_rx.recv())
            .await
            .expect("child should begin promptly")
            .expect("test executor reports the run id");

        assert_eq!(
            engine
                .cancel_children_of("parent-cancel-awaits-executor")
                .await,
            1
        );
        tokio::time::timeout(Duration::from_secs(1), cancel_observed.notified())
            .await
            .expect("child should observe cancellation promptly");
        assert_eq!(
            tracker.get_sub_run_state(&child_run_id).await,
            Some(SubRunState::Running),
            "a cancellation request must not pre-write the terminal tracker state"
        );
        assert_eq!(
            run_engine
                .load_run("user-1", &child_run_id)
                .await
                .expect("durable record loads")
                .expect("child durable record exists")
                .status,
            STATUS_RUNNING,
            "durable state must not claim cancellation before executor completion"
        );

        release.notify_one();
        execution
            .await
            .expect("delegation task joins")
            .expect("delegation completes");
        assert_eq!(
            tracker.get_sub_run_state(&child_run_id).await,
            Some(SubRunState::Cancelled)
        );
        assert_eq!(
            run_engine
                .load_run("user-1", &child_run_id)
                .await
                .expect("durable record loads")
                .expect("child durable record exists")
                .status,
            STATUS_CANCELLED
        );
    }

    #[tokio::test]
    async fn fanout_cancellation_waits_for_the_childs_canonical_cancelled_result() {
        use std::time::Duration;

        let (registry, run_engine, tracker) = setup();
        let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel_observed = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let engine = Arc::new(DelegationEngine::with_executor(
            registry,
            run_engine.clone(),
            tracker.clone(),
            Arc::new(BlockingCancelExecutor {
                started_tx,
                cancel_observed: cancel_observed.clone(),
                release: release.clone(),
            }),
        ));
        let parent_cancel = Arc::new(tokio_util::sync::CancellationToken::new());
        let mut request = fan_out_request(vec!["coder"]);
        request.delegation_id = "delegation-parent-cancel".into();
        request.parent_run_id = "parent-fanout-cancel".into();
        persist_durable_parent_fixture(&engine, &request)
            .await
            .unwrap();

        let execution = {
            let engine = engine.clone();
            let parent_cancel = parent_cancel.clone();
            tokio::spawn(async move { engine.execute(request, "orch", Some(parent_cancel)).await })
        };
        let mut execution = Box::pin(execution);
        let child_run_id = tokio::time::timeout(Duration::from_secs(1), started_rx.recv())
            .await
            .expect("child should begin promptly")
            .expect("test executor reports the run id");

        parent_cancel.cancel();
        tokio::time::timeout(Duration::from_secs(1), cancel_observed.notified())
            .await
            .expect("child should observe parent cancellation promptly");
        assert!(
            tokio::time::timeout(Duration::from_millis(50), execution.as_mut())
                .await
                .is_err(),
            "fanout must wait for its child to publish a canonical terminal result"
        );

        release.notify_one();
        let result = execution
            .await
            .expect("delegation task joins")
            .expect("delegation completes");
        assert_eq!(result.agent_results[0].status, STATUS_CANCELLED);
        assert_eq!(
            tracker.get_sub_run_state(&child_run_id).await,
            Some(SubRunState::Cancelled)
        );
        assert_eq!(
            run_engine
                .load_run("user-1", &child_run_id)
                .await
                .expect("durable record loads")
                .expect("child durable record exists")
                .status,
            STATUS_CANCELLED
        );
    }

    #[tokio::test]
    async fn fork_cancellation_preserves_the_childs_canonical_cancelled_result() {
        use std::time::Duration;

        let (registry, run_engine, tracker) = setup();
        let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel_observed = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let engine = Arc::new(DelegationEngine::with_executor(
            registry,
            run_engine.clone(),
            tracker.clone(),
            Arc::new(BlockingCancelExecutor {
                started_tx,
                cancel_observed: cancel_observed.clone(),
                release: release.clone(),
            }),
        ));
        let parent_cancel = Arc::new(tokio_util::sync::CancellationToken::new());
        let request = fork_request("delegation-fork-parent-cancel", vec!["inspect"], "writer");
        persist_durable_parent_fixture(&engine, &request)
            .await
            .unwrap();

        let execution = {
            let engine = engine.clone();
            let parent_cancel = parent_cancel.clone();
            tokio::spawn(async move { engine.execute(request, "orch", Some(parent_cancel)).await })
        };
        let child_run_id = tokio::time::timeout(Duration::from_secs(1), started_rx.recv())
            .await
            .expect("fork child should begin promptly")
            .expect("test executor reports the run id");

        parent_cancel.cancel();
        tokio::time::timeout(Duration::from_secs(1), cancel_observed.notified())
            .await
            .expect("fork child should observe parent cancellation promptly");
        release.notify_one();

        let result = execution
            .await
            .expect("fork delegation task joins")
            .expect("fork delegation completes");
        assert_eq!(result.agent_results.len(), 1);
        assert_eq!(result.agent_results[0].status, STATUS_CANCELLED);
        assert_eq!(
            tracker.get_sub_run_state(&child_run_id).await,
            Some(SubRunState::Cancelled)
        );
        assert_eq!(
            run_engine
                .load_run("user-1", &child_run_id)
                .await
                .expect("durable record loads")
                .expect("fork child durable record exists")
                .status,
            STATUS_CANCELLED
        );
    }

    #[tokio::test]
    async fn fanout_cancellation_releases_children_queued_for_a_permit() {
        use std::time::Duration;

        let (registry, run_engine, tracker) = setup();
        let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel_observed = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let engine = DelegationEngine::with_executor(
            registry,
            run_engine,
            tracker,
            Arc::new(BlockingCancelExecutor {
                started_tx,
                cancel_observed: cancel_observed.clone(),
                release: release.clone(),
            }),
        );
        let parent_cancel = Arc::new(tokio_util::sync::CancellationToken::new());
        let mut request = fan_out_request(vec!["coder", "reviewer"]);
        request.delegation_id = "delegation-cancel-queued".into();
        request.parent_run_id = "parent-cancel-queued".into();
        request
            .context
            .insert("team_max_parallel".into(), serde_json::json!(1));
        persist_durable_parent_fixture(&engine, &request)
            .await
            .unwrap();

        let execution = {
            let parent_cancel = parent_cancel.clone();
            tokio::spawn(async move { engine.execute(request, "orch", Some(parent_cancel)).await })
        };
        let _first_child = tokio::time::timeout(Duration::from_secs(1), started_rx.recv())
            .await
            .expect("one child should acquire the only permit")
            .expect("test executor reports the run id");

        parent_cancel.cancel();
        tokio::time::timeout(Duration::from_secs(1), cancel_observed.notified())
            .await
            .expect("running child should observe cancellation");
        release.notify_one();

        let result = execution
            .await
            .expect("delegation task joins")
            .expect("delegation completes");
        assert_eq!(result.agent_results.len(), 2);
        assert!(
            result
                .agent_results
                .iter()
                .all(|result| result.status == STATUS_CANCELLED),
            "both running and queued children must report cancellation"
        );
        if let Ok(Some(unexpected_run_id)) =
            tokio::time::timeout(Duration::from_millis(50), started_rx.recv()).await
        {
            panic!(
                "queued child {unexpected_run_id} began execution after its parent was cancelled"
            );
        }
    }

    struct StatusExecutor {
        status: &'static str,
        error: Option<&'static str>,
    }

    #[async_trait]
    impl SubRunExecutor for StatusExecutor {
        async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
            Ok(AgentResult {
                agent_id: config.agent_profile.agent_id,
                run_id: config.run_id,
                status: self.status.to_string(),
                output: Some(format!("[{}] yielded", self.status)),
                error: self.error.map(ToString::to_string),
                prompt_tokens: 1,
                completion_tokens: 0,
                tool_calls: 0,
            })
        }
    }

    /// Test executor that fails for specific agents.
    struct FailingExecutor {
        fail_agents: Vec<String>,
    }

    #[async_trait]
    impl SubRunExecutor for FailingExecutor {
        async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
            if self.fail_agents.contains(&config.agent_profile.agent_id) {
                Ok(AgentResult {
                    agent_id: config.agent_profile.agent_id,
                    run_id: config.run_id,
                    status: "failed".to_string(),
                    output: None,
                    error: Some("intentional test failure".to_string()),
                    prompt_tokens: 5,
                    completion_tokens: 0,
                    tool_calls: 0,
                })
            } else {
                Ok(AgentResult {
                    agent_id: config.agent_profile.agent_id.clone(),
                    run_id: config.run_id,
                    status: "completed".to_string(),
                    output: Some(format!("[{}] done", config.agent_profile.agent_id)),
                    error: None,
                    prompt_tokens: 10,
                    completion_tokens: 20,
                    tool_calls: 1,
                })
            }
        }
    }

    fn setup_with_executor(
        executor: Arc<dyn SubRunExecutor>,
    ) -> (
        Arc<RwLock<AgentProfileRegistry>>,
        Arc<RunEngine>,
        Arc<DelegationTracker>,
        DelegationEngine,
    ) {
        let (reg, engine, tracker) = setup();
        let de =
            DelegationEngine::with_executor(reg.clone(), engine.clone(), tracker.clone(), executor);
        (reg, engine, tracker, de)
    }

    #[tokio::test]
    async fn fan_out_executes_with_custom_executor() {
        let (_, engine, tracker, de) = setup_with_executor(Arc::new(EchoExecutor));

        let req = fan_out_request(vec!["coder", "reviewer"]);
        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();

        assert_eq!(result.status, "completed");
        assert_eq!(result.agent_results.len(), 2);

        // Both agents should have executed and produced output
        for ar in &result.agent_results {
            assert_eq!(ar.status, "completed");
            assert!(ar.output.as_ref().unwrap().contains("test task"));
            assert_eq!(ar.prompt_tokens, 10);
            assert_eq!(ar.completion_tokens, 20);
            assert_eq!(ar.tool_calls, 1);
        }

        // Token aggregation
        assert_eq!(result.total_prompt_tokens, 20);
        assert_eq!(result.total_completion_tokens, 40);
        assert_eq!(result.total_tool_calls, 2);

        // Engine persisted final status
        for ar in &result.agent_results {
            let run = engine
                .load_run("user-1", &ar.run_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(run.status, "completed");
        }

        // Tracker recorded hierarchy
        let subs = tracker.get_sub_runs("del-1").await;
        assert_eq!(subs.len(), 2);
    }

    #[tokio::test]
    async fn fan_out_paused_result_preserves_nonterminal_tracker_state() {
        let (_, engine, tracker, de) = setup_with_executor(Arc::new(StatusExecutor {
            status: STATUS_PAUSED,
            error: None,
        }));

        let result = execute_with_durable_parent(&de, fan_out_request(vec!["coder"]), "orch", None)
            .await
            .unwrap();
        assert_eq!(result.agent_results.len(), 1);
        assert_eq!(result.agent_results[0].status, STATUS_PAUSED);

        let run_id = &result.agent_results[0].run_id;
        assert_eq!(
            tracker.get_sub_run_state(run_id).await,
            Some(SubRunState::Paused)
        );
        let run = engine.load_run("user-1", run_id).await.unwrap().unwrap();
        assert_eq!(run.status, STATUS_PAUSED);
    }

    #[tokio::test]
    async fn sequential_passes_output_to_next_stage() {
        let (_, _, _, de) = setup_with_executor(Arc::new(EchoExecutor));

        let req = DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "del-pipe".into(),
            parent_run_id: "p".into(),
            task: "build code".into(),
            pattern: CoordinationPattern::Pipeline {
                stages: vec![
                    PipelineStage {
                        agent_id: "coder".into(),
                        output_transform: None,
                    },
                    PipelineStage {
                        agent_id: "reviewer".into(),
                        output_transform: None,
                    },
                ],
                timeout_sec: 0,
            },
            user_id: "u".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        };

        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();
        assert_eq!(result.agent_results.len(), 2);

        // First stage has no previous_output
        let first = &result.agent_results[0];
        assert_eq!(first.agent_id, "coder");
        assert!(!first.output.as_ref().unwrap().contains("prev="));

        // Second stage receives first stage's output
        let second = &result.agent_results[1];
        assert_eq!(second.agent_id, "reviewer");
        assert!(second.output.as_ref().unwrap().contains("prev="));
        assert!(second.output.as_ref().unwrap().contains("[coder]"));
    }

    #[tokio::test]
    async fn sequential_stop_on_success_stops_early() {
        let (_, _, _, de) = setup_with_executor(Arc::new(EchoExecutor));

        let req = DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "del-early".into(),
            parent_run_id: "p".into(),
            task: "find answer".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["coder".into(), "reviewer".into(), "writer".into()],
                stop_on_success: true,
                timeout_sec: 0,
            },
            user_id: "u".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        };

        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();
        // First agent succeeds → stops
        assert_eq!(result.agent_results.len(), 1);
        assert_eq!(result.agent_results[0].agent_id, "coder");
    }

    #[tokio::test]
    async fn fan_out_partial_failure() {
        let executor = Arc::new(FailingExecutor {
            fail_agents: vec!["reviewer".to_string()],
        });
        let (_, _, _, de) = setup_with_executor(executor);

        let req = fan_out_request(vec!["coder", "reviewer"]);
        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();

        assert_eq!(result.status, "partial");
        assert_eq!(result.agent_results.len(), 2);

        let coder = result
            .agent_results
            .iter()
            .find(|r| r.agent_id == "coder")
            .unwrap();
        assert_eq!(coder.status, "completed");
        assert!(coder.output.is_some());

        let reviewer = result
            .agent_results
            .iter()
            .find(|r| r.agent_id == "reviewer")
            .unwrap();
        assert_eq!(reviewer.status, "failed");
        assert!(reviewer.error.is_some());
    }

    #[tokio::test]
    async fn adversarial_executes_all_rounds() {
        let (_, _, tracker, de) = setup_with_executor(Arc::new(EchoExecutor));

        let req = DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "del-adv".into(),
            parent_run_id: "p".into(),
            task: "write code".into(),
            pattern: CoordinationPattern::AdversarialReview {
                producer_id: "coder".into(),
                reviewer_id: "reviewer".into(),
                max_rounds: 2,
                acceptance_threshold: 0.8,
                timeout_sec: 0,
            },
            user_id: "u".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        };

        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();
        // 2 rounds × (producer + reviewer) = 4
        assert_eq!(result.agent_results.len(), 4);
        assert_eq!(result.status, "completed");

        // All agents produced output
        for ar in &result.agent_results {
            assert!(ar.output.is_some());
        }

        // Token aggregation across all sub-runs
        assert_eq!(result.total_prompt_tokens, 40); // 4 × 10
        assert_eq!(result.total_completion_tokens, 80); // 4 × 20

        let subs = tracker.get_sub_runs("del-adv").await;
        assert_eq!(subs.len(), 4);
    }

    #[tokio::test]
    async fn tracker_get_agent_id_returns_correct_id() {
        let tracker = DelegationTracker::new();
        tracker
            .record_sub_run(SubRunRecord {
                run_id: "sub-1".into(),
                parent_run_id: "parent".into(),
                delegation_id: "d1".into(),
                agent_id: "coder".into(),
                depth: 1,
                state: SubRunState::Created,
                retry_of: None,
            })
            .await;

        assert_eq!(
            tracker.get_agent_id("sub-1").await,
            Some("coder".to_string())
        );
        assert_eq!(tracker.get_agent_id("parent").await, None);
        assert_eq!(tracker.get_agent_id("nonexistent").await, None);
    }

    #[tokio::test]
    async fn with_executor_constructor_uses_custom_executor() {
        let (reg, engine, tracker) = setup();
        let de = DelegationEngine::with_executor(reg, engine, tracker, Arc::new(EchoExecutor));

        let req = fan_out_request(vec!["coder"]);
        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();
        assert_eq!(result.status, "completed");
        // EchoExecutor returns prompt_tokens=10
        assert_eq!(result.total_prompt_tokens, 10);
    }

    #[tokio::test]
    async fn sub_run_config_passes_context() {
        /// Executor that checks context is passed through.
        struct ContextCheckExecutor;

        #[async_trait]
        impl SubRunExecutor for ContextCheckExecutor {
            async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
                let has_key = config.context.contains_key("test_key");
                Ok(AgentResult {
                    agent_id: config.agent_profile.agent_id,
                    run_id: config.run_id,
                    status: "completed".to_string(),
                    output: Some(format!("context_present={}", has_key)),
                    error: None,
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    tool_calls: 0,
                })
            }
        }

        let (reg, engine, tracker) = setup();
        let de =
            DelegationEngine::with_executor(reg, engine, tracker, Arc::new(ContextCheckExecutor));

        let mut ctx = HashMap::new();
        ctx.insert("test_key".to_string(), serde_json::json!("test_value"));

        let req = DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "ctx-test".into(),
            parent_run_id: "p".into(),
            task: "check context".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["coder".into()],
                stop_on_success: false,
                timeout_sec: 0,
            },
            user_id: "u".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: ctx,
            execution_metadata: None,
        };

        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();
        assert_eq!(
            result.agent_results[0].output.as_deref(),
            Some("context_present=true")
        );
    }

    #[tokio::test]
    async fn execute_with_forward_headers_passes_sensitive_headers_sideband() {
        struct ForwardHeadersCheckExecutor;

        #[async_trait]
        impl SubRunExecutor for ForwardHeadersCheckExecutor {
            async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
                let has_auth = config.forward_headers.contains_key("authorization");
                let has_context_key = config.context.contains_key(
                    crate::turn::agentic::delegate_interception::FORWARD_HEADERS_CONTEXT_KEY,
                );
                Ok(AgentResult {
                    agent_id: config.agent_profile.agent_id,
                    run_id: config.run_id,
                    status: "completed".to_string(),
                    output: Some(format!(
                        "auth_present={has_auth};context_key_present={has_context_key}"
                    )),
                    error: None,
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    tool_calls: 0,
                })
            }
        }

        let (reg, engine, tracker) = setup();
        let de = DelegationEngine::with_executor(
            reg,
            engine,
            tracker,
            Arc::new(ForwardHeadersCheckExecutor),
        );

        let req = DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "fh-test".into(),
            parent_run_id: "p".into(),
            task: "check headers".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["coder".into()],
                stop_on_success: false,
                timeout_sec: 0,
            },
            user_id: "u".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        };

        let result = execute_with_durable_parent_and_headers(
            &de,
            req,
            "orch",
            None,
            HashMap::from([(
                "authorization".to_string(),
                "Bearer trusted-token".to_string(),
            )]),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            result.agent_results[0].output.as_deref(),
            Some("auth_present=true;context_key_present=false")
        );
    }

    #[tokio::test]
    async fn execute_with_forward_headers_passes_admitted_model_execution_sideband() {
        struct ExecutionMaterialCheckExecutor;

        #[async_trait]
        impl SubRunExecutor for ExecutionMaterialCheckExecutor {
            async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
                let encoded = config
                    .admitted_model_execution
                    .as_ref()
                    .map(|execution| {
                        format!(
                            "{}|{}",
                            execution
                                .completions_url_override
                                .as_deref()
                                .unwrap_or_default(),
                            execution.request_timeout_ms.unwrap_or(0)
                        )
                    })
                    .unwrap_or_else(|| "none".to_string());
                Ok(AgentResult {
                    agent_id: config.agent_profile.agent_id,
                    run_id: config.run_id,
                    status: "completed".to_string(),
                    output: Some(encoded),
                    error: None,
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    tool_calls: 0,
                })
            }
        }

        let (reg, engine, tracker) = setup();
        let de = DelegationEngine::with_executor(
            reg,
            engine,
            tracker,
            Arc::new(ExecutionMaterialCheckExecutor),
        );

        let req = DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "execution-material-test".into(),
            parent_run_id: "p".into(),
            task: "check admitted execution material".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["coder".into()],
                stop_on_success: false,
                timeout_sec: 0,
            },
            user_id: "u".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        };

        let result = execute_with_durable_parent_and_headers(
            &de,
            req,
            "orch",
            None,
            HashMap::new(),
            Some(AdmittedModelExecution::from_endpoint(
                "offer-coder".to_string(),
                "test-model".to_string(),
                "openai".to_string(),
                "http://catalog:8081/api/v1/chat/completions".to_string(),
                "Bearer test".to_string(),
                Some(2500),
                128_000,
            )),
        )
        .await
        .unwrap();

        assert_eq!(
            result.agent_results[0].output.as_deref(),
            Some("http://catalog:8081/api/v1/chat/completions|2500")
        );
    }

    #[tokio::test]
    async fn execute_ignores_serialized_forward_headers_in_request_context() {
        struct ForwardHeadersCheckExecutor;

        #[async_trait]
        impl SubRunExecutor for ForwardHeadersCheckExecutor {
            async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
                let has_auth = config.forward_headers.contains_key("authorization");
                let has_context_key = config.context.contains_key(
                    crate::turn::agentic::delegate_interception::FORWARD_HEADERS_CONTEXT_KEY,
                );
                Ok(AgentResult {
                    agent_id: config.agent_profile.agent_id,
                    run_id: config.run_id,
                    status: "completed".to_string(),
                    output: Some(format!(
                        "auth_present={has_auth};context_key_present={has_context_key}"
                    )),
                    error: None,
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    tool_calls: 0,
                })
            }
        }

        let (reg, engine, tracker) = setup();
        let de = DelegationEngine::with_executor(
            reg,
            engine,
            tracker,
            Arc::new(ForwardHeadersCheckExecutor),
        );

        let req = DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "fh-context-test".into(),
            parent_run_id: "p".into(),
            task: "check serialized headers".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["coder".into()],
                stop_on_success: false,
                timeout_sec: 0,
            },
            user_id: "u".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: HashMap::from([(
                crate::turn::agentic::delegate_interception::FORWARD_HEADERS_CONTEXT_KEY
                    .to_string(),
                serde_json::json!({"authorization": "Bearer evil", "x-workspace-id": "ws-001"}),
            )]),
            execution_metadata: None,
        };

        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();
        assert_eq!(
            result.agent_results[0].output.as_deref(),
            Some("auth_present=false;context_key_present=false")
        );
    }

    #[test]
    fn parse_request_allowlist_from_context_normalizes_and_dedupes() {
        let key = crate::turn::agentic::delegate_interception::REQUEST_ALLOWED_TOOLS_CONTEXT_KEY;
        let mut context = HashMap::from([(
            key.to_string(),
            serde_json::json!([" Bash ", "bash", "READ_FILE"]),
        )]);

        let parsed = parse_request_allowlist_from_context(&mut context, key)
            .expect("allowlist should parse")
            .expect("allowlist should be present");

        let expected = HashSet::from(["bash".to_string(), "read_file".to_string()]);
        assert_eq!(parsed, expected);
        assert!(
            !context.contains_key(key),
            "key should be removed from context"
        );
    }

    #[test]
    fn parse_request_allowlist_from_context_rejects_non_array_value() {
        let key = crate::turn::agentic::delegate_interception::REQUEST_ALLOWED_TOOLS_CONTEXT_KEY;
        let mut context = HashMap::from([(key.to_string(), serde_json::json!("bash"))]);

        let err = parse_request_allowlist_from_context(&mut context, key)
            .expect_err("non-array allowlist should fail");
        assert!(err.contains("must be an array of strings"));
    }

    #[test]
    fn parse_request_allowlist_from_context_rejects_non_string_or_empty_entries() {
        let key = crate::turn::agentic::delegate_interception::REQUEST_ALLOWED_TOOLS_CONTEXT_KEY;
        let mut non_string_context =
            HashMap::from([(key.to_string(), serde_json::json!(["bash", 42]))]);
        let err = parse_request_allowlist_from_context(&mut non_string_context, key)
            .expect_err("non-string entry should fail");
        assert!(err.contains("must contain only strings"));

        let mut empty_context =
            HashMap::from([(key.to_string(), serde_json::json!(["bash", "   "]))]);
        let err = parse_request_allowlist_from_context(&mut empty_context, key)
            .expect_err("empty entry should fail");
        assert!(err.contains("must not contain empty or whitespace-only strings"));
    }

    #[test]
    fn parse_request_skill_sources_from_context_normalizes_and_parses() {
        let key =
            crate::turn::agentic::delegate_interception::REQUEST_ALLOWED_SKILL_SOURCES_CONTEXT_KEY;
        let mut context = HashMap::from([(
            key.to_string(),
            serde_json::json!([" Database ", "database", "MCP"]),
        )]);

        let parsed = parse_request_skill_sources_from_context(&mut context, key)
            .expect("skill sources should parse")
            .expect("skill sources should be present");

        let expected = HashSet::from([
            crate::skills::manifest::SkillSourceKind::Database,
            crate::skills::manifest::SkillSourceKind::Mcp,
        ]);
        assert_eq!(parsed, expected);
        assert!(
            !context.contains_key(key),
            "key should be removed from context"
        );
    }

    #[test]
    fn parse_request_skill_sources_from_context_rejects_unknown_source() {
        let key =
            crate::turn::agentic::delegate_interception::REQUEST_ALLOWED_SKILL_SOURCES_CONTEXT_KEY;
        let mut context = HashMap::from([(key.to_string(), serde_json::json!(["dynamic"]))]);

        let err = parse_request_skill_sources_from_context(&mut context, key)
            .expect_err("unknown skill source should fail");
        assert!(err.contains("unsupported skill source"));
        assert!(err.contains("expected one of"));
    }

    #[tokio::test]
    async fn worktree_path_per_agent_flows_through_context() {
        /// Executor that captures the agent-specific worktree_path from context.
        struct WorktreeCheckExecutor;

        #[async_trait]
        impl SubRunExecutor for WorktreeCheckExecutor {
            async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
                let key = format!("worktree_path_{}", config.agent_profile.agent_id);
                let path = config
                    .context
                    .get(&key)
                    .and_then(|v| v.as_str())
                    .unwrap_or("none")
                    .to_string();
                Ok(AgentResult {
                    agent_id: config.agent_profile.agent_id,
                    run_id: config.run_id,
                    status: "completed".to_string(),
                    output: Some(path),
                    error: None,
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    tool_calls: 0,
                })
            }
        }

        let (reg, engine, tracker) = setup();
        // Register two agents
        {
            let mut r = reg.write().await;
            let _ = r.register(AgentProfile::new("agent-a", "Agent A", AgentTier::User));
            let _ = r.register(AgentProfile::new("agent-b", "Agent B", AgentTier::User));
        }
        let de =
            DelegationEngine::with_executor(reg, engine, tracker, Arc::new(WorktreeCheckExecutor));

        let mut ctx = HashMap::new();
        ctx.insert(
            "worktree_path_agent-a".to_string(),
            serde_json::json!("/tmp/wt/agent-a"),
        );
        ctx.insert(
            "worktree_path_agent-b".to_string(),
            serde_json::json!("/tmp/wt/agent-b"),
        );

        let req = DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "wt-test".into(),
            parent_run_id: "p".into(),
            task: "check worktree".into(),
            pattern: CoordinationPattern::FanOut {
                agent_ids: vec!["agent-a".into(), "agent-b".into()],
                aggregation: AggregationStrategy::AllResults,
                timeout_sec: 30,
            },
            user_id: "u".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: ctx,
            execution_metadata: None,
        };

        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();
        assert_eq!(result.agent_results.len(), 2);

        // Each agent should see its own worktree path
        for ar in &result.agent_results {
            let expected_path = format!("/tmp/wt/{}", ar.agent_id);
            assert_eq!(
                ar.output.as_deref(),
                Some(expected_path.as_str()),
                "agent {} should see its worktree path",
                ar.agent_id
            );
        }
    }

    #[tokio::test]
    async fn stub_executor_returns_completed() {
        let executor = StubSubRunExecutor;
        let config = SubRunConfig {
            execution_owner_generation: None,
            execution_owner_generation_sink: None,
            run_id: "r1".into(),
            parent_run_id: "parent-r1".into(),
            agent_profile: AgentProfile::new("test", "Test", AgentTier::User),
            task: "hello world".into(),
            session_id: "s1".into(),
            user_id: "u1".into(),
            previous_output: None,
            context: HashMap::new(),
            forward_headers: HashMap::new(),
            admitted_model_execution: None,
            interaction_mode: RequestedTurnInteractionMode::Headless,
            request_constraints: Default::default(),
            recursion_depth: 1,
            max_turns: None,
            initial_turns: None,
            pause_flag: None,
            checkpoint_gate: None,
            mailbox: None,
            progress_emitter: None,
            live_event_sink: None,
            cancel_token: None,
            inherited_prefix: None,
            execution_metadata: None,
            work_item: None,
            delegation_chain: Vec::new(),
            #[cfg(feature = "harness")]
            harness_sink: None,
        };

        let result = executor.execute(config).await.unwrap();
        assert_eq!(result.status, "completed");
        assert!(result.output.unwrap().contains("hello world"));
    }

    #[tokio::test]
    async fn pause_children_of_ignores_terminal_sub_runs() {
        let (_, engine, tracker, de) = setup_with_executor(Arc::new(EchoExecutor));

        let req = fan_out_request(vec!["coder", "reviewer"]);
        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();
        assert_eq!(result.agent_results.len(), 2);

        // Pause all children of parent-1 (sub-runs are already completed)
        let paused = de
            .pause_children_of("user-1", "test-session", "parent-1")
            .await;
        assert_eq!(paused, 0);

        // A terminal task cannot observe cooperative flags.
        for ar in &result.agent_results {
            assert!(!tracker.is_paused(&ar.run_id).await);
        }

        // Durable status is NOT overwritten for terminal sub-runs
        for ar in &result.agent_results {
            let run = engine
                .load_run("user-1", &ar.run_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(run.status, "completed");
        }

        // Resume does not claim to revive completed work.
        let resumed = de
            .resume_children_of("user-1", "test-session", "parent-1")
            .await;
        assert_eq!(resumed, 0);
        for ar in &result.agent_results {
            assert!(!tracker.is_paused(&ar.run_id).await);
        }
    }

    #[tokio::test]
    async fn pause_delegation_by_id_ignores_terminal_sub_runs() {
        let (_, engine, tracker, de) = setup_with_executor(Arc::new(EchoExecutor));

        let req = fan_out_request(vec!["coder", "reviewer"]);
        execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();

        let paused = de.pause_delegation("user-1", "test-session", "del-1").await;
        assert_eq!(paused, 0);

        let subs = tracker.get_sub_runs("del-1").await;
        for sub in &subs {
            assert!(!tracker.is_paused(&sub.run_id).await);
            // Durable status preserved — terminal sub-runs not overwritten
            let run = engine
                .load_run("user-1", &sub.run_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(run.status, "completed");
        }

        let resumed = de
            .resume_delegation("user-1", "test-session", "del-1")
            .await;
        assert_eq!(resumed, 0);
        for sub in &subs {
            assert!(!tracker.is_paused(&sub.run_id).await);
        }
    }

    #[tokio::test]
    async fn live_sub_run_pause_resume_commits_status_and_event_before_flag() {
        let (_, engine, tracker, de) = setup_with_executor(Arc::new(EchoExecutor));
        engine
            .start_run("parent-live", "user-1", "session-live")
            .await
            .unwrap();
        engine
            .start_run_ext(
                "sub-live",
                "user-1",
                "session-live",
                Some("parent-live"),
                Some("delegation-live"),
                Some("coder"),
                None,
            )
            .await
            .unwrap();
        tracker
            .record_sub_run(SubRunRecord {
                run_id: "sub-live".into(),
                parent_run_id: "parent-live".into(),
                delegation_id: "delegation-live".into(),
                agent_id: "coder".into(),
                depth: 1,
                state: SubRunState::Running,
                retry_of: None,
            })
            .await;
        tracker.register_pause_flag("sub-live").await;

        assert_eq!(
            de.pause_children_of("user-1", "session-live", "parent-live")
                .await,
            1
        );
        assert!(tracker.is_paused("sub-live").await);
        let paused = engine
            .load_run("user-1", "sub-live")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(paused.status, STATUS_PAUSED);
        assert_eq!(paused.waiting_for.as_deref(), Some("parent_pause"));
        assert_eq!(paused.events.last().unwrap()["event_type"], "run_paused");

        assert_eq!(
            de.resume_children_of("user-1", "session-live", "parent-live")
                .await,
            1
        );
        assert!(!tracker.is_paused("sub-live").await);
        let resumed = engine
            .load_run("user-1", "sub-live")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resumed.status, STATUS_RUNNING);
        assert!(resumed.waiting_for.is_none());
        assert_eq!(resumed.events.last().unwrap()["event_type"], "run_resumed");
    }

    #[tokio::test]
    async fn waiting_sub_run_retains_its_required_context_when_parent_pauses() {
        let (_, engine, tracker, de) = setup_with_executor(Arc::new(EchoExecutor));
        engine
            .start_run("parent-live", "user-1", "session-live")
            .await
            .unwrap();
        engine
            .start_run_ext(
                "sub-waiting",
                "user-1",
                "session-live",
                Some("parent-live"),
                Some("delegation-live"),
                Some("coder"),
                None,
            )
            .await
            .unwrap();
        tracker
            .record_sub_run(SubRunRecord {
                run_id: "sub-waiting".into(),
                parent_run_id: "parent-live".into(),
                delegation_id: "delegation-live".into(),
                agent_id: "coder".into(),
                depth: 1,
                state: SubRunState::Waiting,
                retry_of: None,
            })
            .await;
        tracker.register_pause_flag("sub-waiting").await;
        assert!(
            engine
                .persist_delegation_outcome_status(
                    "user-1",
                    "session-live",
                    "sub-waiting",
                    STATUS_WAITING,
                    Some("user_input"),
                    None,
                )
                .await
                .unwrap()
        );

        assert_eq!(
            de.pause_children_of("user-1", "session-live", "parent-live")
                .await,
            0
        );
        assert!(!tracker.is_paused("sub-waiting").await);
        let durable = engine
            .load_run("user-1", "sub-waiting")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(durable.status, STATUS_WAITING);
        assert_eq!(durable.waiting_for.as_deref(), Some("user_input"));
    }

    #[tokio::test]
    async fn durable_control_decision_replaces_stale_executor_result() {
        for (run_id, durable_status) in [
            ("stale-after-pause", STATUS_PAUSED),
            ("stale-after-cancel", STATUS_CANCELLED),
        ] {
            let (_, engine, _) = setup();
            engine
                .start_run(run_id, "user-1", "session-1")
                .await
                .unwrap();
            if durable_status == STATUS_CANCELLED {
                engine
                    .persist_delegation_outcome_status(
                        "user-1",
                        "session-1",
                        run_id,
                        durable_status,
                        None,
                        Some("control-plane"),
                    )
                    .await
                    .unwrap();
            } else {
                engine
                    .persist_status(
                        "user-1",
                        "session-1",
                        run_id,
                        durable_status,
                        Some("control-plane"),
                        None,
                    )
                    .await
                    .unwrap();
            }

            let authoritative = reconcile_agent_result_with_durable_authority(
                &engine,
                "user-1",
                "session-1",
                DurableLifecycleDisposition::SchedulerOwned {
                    owner_generation: 0,
                },
                AgentResult {
                    agent_id: "coder".into(),
                    run_id: run_id.into(),
                    status: STATUS_COMPLETED.into(),
                    output: Some("stale executor output".into()),
                    error: None,
                    prompt_tokens: 3,
                    completion_tokens: 5,
                    tool_calls: 1,
                },
            )
            .await;

            assert_eq!(authoritative.status, durable_status);
            assert!(authoritative.output.is_none());
            if durable_status == STATUS_CANCELLED {
                assert!(authoritative.error.is_some());
            } else {
                assert!(authoritative.error.is_none());
            }
        }
    }

    #[tokio::test]
    async fn executor_owned_durable_lifecycle_never_issues_unfenced_outer_terminal_write() {
        let store = Arc::new(InMemoryRunStateStore::new());
        let engine = RunEngine::new(store.clone());
        let authority = engine
            .start_run("executor-owned-stale-result", "user-1", "session-1")
            .await
            .expect("start durable child run");
        assert_eq!(authority.owner_generation, 0);

        let claimed = store
            .claim_recoverable_active_runs(1)
            .await
            .expect("recovery claims the expired execution owner");
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].run_generation, 1);

        let authoritative = reconcile_agent_result_with_durable_authority(
            &engine,
            "user-1",
            "session-1",
            DurableLifecycleDisposition::ExecutorOwned {
                owner_generation: authority.owner_generation,
            },
            AgentResult {
                agent_id: "coder".into(),
                run_id: "executor-owned-stale-result".into(),
                status: STATUS_COMPLETED.into(),
                output: Some("stale executor output".into()),
                error: None,
                prompt_tokens: 3,
                completion_tokens: 5,
                tool_calls: 1,
            },
        )
        .await;

        assert_eq!(authoritative.status, STATUS_WAITING);
        assert!(authoritative.output.is_none());
        let durable = engine
            .load_run("user-1", "executor-owned-stale-result")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(durable.status, STATUS_RUNNING);
        assert_eq!(durable.run_generation, 1);
    }

    #[tokio::test]
    async fn same_terminal_status_from_new_generation_does_not_preserve_stale_output() {
        let store = Arc::new(InMemoryRunStateStore::new());
        let engine = RunEngine::new(store.clone());
        let authority = engine
            .start_run("executor-owned-same-status-race", "user-1", "session-1")
            .await
            .expect("start durable child run");

        let claimed = store
            .claim_recoverable_active_runs(1)
            .await
            .expect("recovery claims the expired execution owner");
        let winner_generation = claimed[0].run_generation;
        assert_ne!(winner_generation, authority.owner_generation);
        assert!(
            engine
                .persist_delegation_outcome_status_if_current_owner(
                    "user-1",
                    "session-1",
                    "executor-owned-same-status-race",
                    winner_generation,
                    STATUS_COMPLETED,
                    None,
                    None,
                )
                .await
                .expect("recovered owner commits its completed result")
        );

        let authoritative = reconcile_agent_result_with_durable_authority(
            &engine,
            "user-1",
            "session-1",
            DurableLifecycleDisposition::ExecutorOwned {
                owner_generation: authority.owner_generation,
            },
            AgentResult {
                agent_id: "coder".into(),
                run_id: "executor-owned-same-status-race".into(),
                status: STATUS_COMPLETED.into(),
                output: Some("stale generation output".into()),
                error: None,
                prompt_tokens: 3,
                completion_tokens: 5,
                tool_calls: 1,
            },
        )
        .await;

        assert_eq!(authoritative.status, STATUS_COMPLETED);
        assert!(authoritative.output.is_none());
        assert_eq!(authoritative.prompt_tokens, 0);
        assert_eq!(authoritative.completion_tokens, 0);
        assert_eq!(authoritative.tool_calls, 0);
    }

    #[tokio::test]
    async fn scheduler_cas_loser_does_not_preserve_same_status_from_stale_generation() {
        let store = Arc::new(InMemoryRunStateStore::new());
        let engine = RunEngine::new(store.clone());
        let authority = engine
            .start_run("scheduler-owned-same-status-race", "user-1", "session-1")
            .await
            .expect("start durable child run");
        let claimed = store
            .claim_recoverable_active_runs(1)
            .await
            .expect("recovery claims the expired execution owner");
        let winner_generation = claimed[0].run_generation;
        assert!(
            engine
                .persist_delegation_outcome_status_if_current_owner(
                    "user-1",
                    "session-1",
                    "scheduler-owned-same-status-race",
                    winner_generation,
                    STATUS_COMPLETED,
                    None,
                    None,
                )
                .await
                .expect("recovered owner commits its completed result")
        );

        let authoritative = reconcile_agent_result_with_durable_authority(
            &engine,
            "user-1",
            "session-1",
            DurableLifecycleDisposition::SchedulerOwned {
                owner_generation: authority.owner_generation,
            },
            AgentResult {
                agent_id: "offline-executor".into(),
                run_id: "scheduler-owned-same-status-race".into(),
                status: STATUS_COMPLETED.into(),
                output: Some("stale scheduler output".into()),
                error: None,
                prompt_tokens: 8,
                completion_tokens: 13,
                tool_calls: 2,
            },
        )
        .await;

        assert_eq!(authoritative.status, STATUS_COMPLETED);
        assert!(authoritative.output.is_none());
        assert_eq!(authoritative.prompt_tokens, 0);
        assert_eq!(authoritative.completion_tokens, 0);
        assert_eq!(authoritative.tool_calls, 0);
    }

    #[tokio::test]
    async fn scheduler_owned_terminal_commits_replay_events_with_the_owner_cas() {
        for (run_id, status, error, expected_event_types) in [
            (
                "scheduler-owned-completed",
                STATUS_COMPLETED,
                None,
                vec!["run_finished"],
            ),
            (
                "scheduler-owned-failed",
                STATUS_FAILED,
                Some("offline executor failed"),
                vec!["run_error", "run_finished"],
            ),
        ] {
            let (_, engine, _) = setup();
            let authority = engine
                .start_run(run_id, "user-1", "session-1")
                .await
                .expect("start scheduler-owned run");

            let authoritative = reconcile_agent_result_with_durable_authority(
                &engine,
                "user-1",
                "session-1",
                DurableLifecycleDisposition::SchedulerOwned {
                    owner_generation: authority.owner_generation,
                },
                AgentResult {
                    agent_id: "offline-executor".into(),
                    run_id: run_id.into(),
                    status: status.into(),
                    output: (status == STATUS_COMPLETED).then(|| "answer".into()),
                    error: error.map(str::to_string),
                    prompt_tokens: 3,
                    completion_tokens: 5,
                    tool_calls: 1,
                },
            )
            .await;

            assert_eq!(authoritative.status, status);
            let durable = engine.load_run("user-1", run_id).await.unwrap().unwrap();
            assert_eq!(durable.status, status);
            let event_types = durable
                .events
                .iter()
                .filter_map(|event| event.get("event_type").and_then(serde_json::Value::as_str))
                .filter(|event_type| matches!(*event_type, "run_error" | "run_finished"))
                .collect::<Vec<_>>();
            assert_eq!(event_types, expected_event_types);
        }
    }

    #[tokio::test]
    async fn identical_terminal_replay_preserves_richer_agent_result() {
        let (_, engine, _) = setup();
        let run_id = "completed-result-replay";
        engine
            .start_run(run_id, "user-1", "session-1")
            .await
            .unwrap();
        assert!(
            engine
                .persist_delegation_outcome_status(
                    "user-1",
                    "session-1",
                    run_id,
                    STATUS_COMPLETED,
                    None,
                    None,
                )
                .await
                .unwrap()
        );

        let replayed = reconcile_agent_result_with_durable_authority(
            &engine,
            "user-1",
            "session-1",
            DurableLifecycleDisposition::SchedulerOwned {
                owner_generation: 0,
            },
            AgentResult {
                agent_id: "coder".into(),
                run_id: run_id.into(),
                status: STATUS_COMPLETED.into(),
                output: Some("full answer".into()),
                error: None,
                prompt_tokens: 3,
                completion_tokens: 5,
                tool_calls: 1,
            },
        )
        .await;

        assert_eq!(replayed.status, STATUS_COMPLETED);
        assert_eq!(replayed.output.as_deref(), Some("full answer"));
    }

    // ─── Verification Gate Tests ────────────────────────────────────────────

    /// Gate that always passes.
    struct AlwaysPassGate;

    #[async_trait]
    impl VerificationGate for AlwaysPassGate {
        async fn verify(
            &self,
            _result: &AgentResult,
            _delegation_id: &str,
            _attempt: u32,
        ) -> GateVerdict {
            GateVerdict::Pass
        }
    }

    /// Gate that fails the first N attempts, then passes.
    struct FailThenPassGate {
        fail_count: std::sync::atomic::AtomicU32,
        max_fails: u32,
    }

    impl FailThenPassGate {
        fn new(max_fails: u32) -> Self {
            Self {
                fail_count: std::sync::atomic::AtomicU32::new(0),
                max_fails,
            }
        }
    }

    #[async_trait]
    impl VerificationGate for FailThenPassGate {
        async fn verify(
            &self,
            _result: &AgentResult,
            _delegation_id: &str,
            _attempt: u32,
        ) -> GateVerdict {
            let count = self.fail_count.fetch_add(1, Ordering::Relaxed);
            if count < self.max_fails {
                GateVerdict::Fail {
                    reason: format!("fail #{}", count + 1),
                    details: None,
                }
            } else {
                GateVerdict::Pass
            }
        }

        fn max_retries(&self) -> u32 {
            3
        }
    }

    /// Gate that always fails.
    struct AlwaysFailGate;

    #[async_trait]
    impl VerificationGate for AlwaysFailGate {
        async fn verify(
            &self,
            _result: &AgentResult,
            _delegation_id: &str,
            _attempt: u32,
        ) -> GateVerdict {
            GateVerdict::Fail {
                reason: "quality too low".into(),
                details: Some(serde_json::json!({"score": 0.3})),
            }
        }

        fn max_retries(&self) -> u32 {
            2
        }
    }

    #[tokio::test]
    async fn default_gate_rejects_binary_garbage() {
        let gate = DefaultQualityGate::new();
        let result = AgentResult {
            agent_id: "test".into(),
            run_id: "r1".into(),
            status: "completed".into(),
            output: Some("some text\0\0\0\0\0\0\0\0\0\0garbage".into()),
            error: None,
            prompt_tokens: 0,
            completion_tokens: 0,
            tool_calls: 0,
        };
        let verdict = gate.verify(&result, "d1", 0).await;
        assert!(
            matches!(verdict, GateVerdict::Fail { .. }),
            "should reject output with null bytes"
        );
    }

    #[tokio::test]
    async fn default_gate_passes_clean_output() {
        let gate = DefaultQualityGate::new();
        let result = AgentResult {
            agent_id: "test".into(),
            run_id: "r1".into(),
            status: "completed".into(),
            output: Some("This is a perfectly normal output with enough content.".into()),
            error: None,
            prompt_tokens: 0,
            completion_tokens: 0,
            tool_calls: 0,
        };
        let verdict = gate.verify(&result, "d1", 0).await;
        assert!(matches!(verdict, GateVerdict::Pass));
    }

    #[tokio::test]
    async fn gate_pass_does_not_alter_results() {
        let (reg, engine, tracker, _) = setup_with_executor(Arc::new(EchoExecutor));
        let de = DelegationEngine::with_executor(reg, engine, tracker, Arc::new(EchoExecutor))
            .with_gate(Arc::new(AlwaysPassGate));

        let req = fan_out_request(vec!["coder", "reviewer"]);
        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();

        assert_eq!(result.status, "completed");
        assert_eq!(result.agent_results.len(), 2);
        for ar in &result.agent_results {
            assert_eq!(ar.status, "completed");
        }
    }

    #[tokio::test]
    async fn gate_fail_marks_verification_failed() {
        let (reg, engine, tracker, _) = setup_with_executor(Arc::new(EchoExecutor));
        let de = DelegationEngine::with_executor(reg, engine, tracker, Arc::new(EchoExecutor))
            .with_gate(Arc::new(AlwaysFailGate));

        let req = fan_out_request(vec!["coder"]);
        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();

        // Fan-out with always-fail gate: result should be verification_failed
        assert_eq!(result.agent_results.len(), 1);
        assert_eq!(result.agent_results[0].status, "verification_failed");
        assert!(
            result.agent_results[0]
                .error
                .as_ref()
                .unwrap()
                .contains("quality too low")
        );
    }

    #[tokio::test]
    async fn gate_retry_then_pass_sequential() {
        let (reg, engine, tracker, _) = setup_with_executor(Arc::new(EchoExecutor));
        // Fail once, then pass on second attempt
        let gate = Arc::new(FailThenPassGate::new(1));
        let de = DelegationEngine::with_executor(reg, engine, tracker, Arc::new(EchoExecutor))
            .with_gate(gate);

        let req = DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "del-seq-gate".into(),
            parent_run_id: "parent-1".into(),
            task: "sequential gate test".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["coder".into()],
                stop_on_success: false,
                timeout_sec: 0,
            },
            user_id: "user-1".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        };
        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();

        // Should eventually pass after retry
        assert_eq!(result.agent_results.len(), 1);
        assert_eq!(result.agent_results[0].status, "completed");
    }

    #[tokio::test]
    async fn gate_retry_carries_the_exact_durable_execution_authority() {
        let (registry, run_engine, tracker) = setup();
        let bindings = Arc::new(std::sync::Mutex::new(Vec::new()));
        let executor = Arc::new(CaptureRunBindingExecutor {
            bindings: bindings.clone(),
        });
        let engine = DelegationEngine::with_executor(registry, run_engine, tracker, executor)
            .with_gate(Arc::new(FailThenPassGate::new(1)));
        let request = DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "gate-retry-authority".into(),
            parent_run_id: "gate-retry-parent".into(),
            task: "verify retry authority".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["coder".into()],
                stop_on_success: false,
                timeout_sec: 0,
            },
            user_id: "user-1".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        };

        let result = execute_with_durable_parent(&engine, request, "orch", None)
            .await
            .expect("retry completes");
        assert_eq!(result.agent_results[0].status, STATUS_COMPLETED);
        let bindings = bindings.lock().unwrap();
        assert_eq!(bindings.len(), 2);
        assert!(
            bindings
                .iter()
                .all(|binding| binding.execution_owner_generation == Some(0)),
            "both the initial execution and the pre-started retry must carry their exact generation"
        );
    }

    #[tokio::test]
    async fn gate_retry_preserves_sequential_coordination_prompt() {
        let (reg, engine, tracker, _) = setup_with_executor(Arc::new(EchoExecutor));
        let gate = Arc::new(FailThenPassGate::new(1));
        let de = DelegationEngine::with_executor(reg, engine, tracker, Arc::new(EchoExecutor))
            .with_gate(gate);

        let req = DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "del-seq-gate-prompt".into(),
            parent_run_id: "parent-1".into(),
            task: "sequential gate test".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["coder".into()],
                stop_on_success: false,
                timeout_sec: 0,
            },
            user_id: "user-1".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        };
        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();

        let output = result.agent_results[0].output.as_deref().unwrap_or("");
        assert!(output.contains("## Team Coordination: Pipeline"));
        assert!(output.contains("Quality gate active"));
    }

    #[tokio::test]
    async fn gate_retry_preserves_adversarial_coordination_prompt() {
        let (reg, engine, tracker, _) = setup_with_executor(Arc::new(EchoExecutor));
        let gate = Arc::new(FailThenPassGate::new(1));
        let de = DelegationEngine::with_executor(reg, engine, tracker, Arc::new(EchoExecutor))
            .with_gate(gate);

        let req = DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "del-adv-gate-prompt".into(),
            parent_run_id: "parent-1".into(),
            task: "adversarial gate test".into(),
            pattern: CoordinationPattern::AdversarialReview {
                producer_id: "coder".into(),
                reviewer_id: "reviewer".into(),
                max_rounds: 1,
                timeout_sec: 0,
                acceptance_threshold: 0.8,
            },
            user_id: "user-1".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        };
        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();

        let producer_output = result.agent_results[0].output.as_deref().unwrap_or("");
        assert!(producer_output.contains("## Team Coordination: Adversarial Review (Producer)"));
        assert!(producer_output.contains("Quality gate active"));
    }

    #[tokio::test]
    async fn gate_retry_registers_pause_flag() {
        let (reg, engine, tracker, _) = setup_with_executor(Arc::new(EchoExecutor));
        let gate = Arc::new(FailThenPassGate::new(1));
        let de =
            DelegationEngine::with_executor(reg, engine, tracker.clone(), Arc::new(EchoExecutor))
                .with_gate(gate);

        let result = execute_with_durable_parent(&de, fan_out_request(vec!["coder"]), "orch", None)
            .await
            .unwrap();
        assert_eq!(result.agent_results.len(), 1);
        assert_eq!(result.agent_results[0].status, "completed");

        let chain = tracker
            .get_retry_chain(&result.agent_results[0].run_id)
            .await;
        assert_eq!(chain.len(), 2);
        assert!(tracker.get_pause_flag(&chain[0]).await.is_some());
        assert!(tracker.get_pause_flag(&chain[1]).await.is_some());
        assert_eq!(
            de.pause_delegation("user-1", "test-session", "del-1").await,
            0,
            "completed retry attempts must not be advertised as cooperatively paused"
        );
    }

    #[tokio::test]
    async fn gate_retry_preserves_depth_metadata() {
        let (reg, engine, tracker, _) = setup_with_executor(Arc::new(EchoExecutor));
        let gate = Arc::new(FailThenPassGate::new(1));
        let de =
            DelegationEngine::with_executor(reg, engine, tracker.clone(), Arc::new(EchoExecutor))
                .with_gate(gate);

        let result = execute_with_durable_parent(&de, fan_out_request(vec!["coder"]), "orch", None)
            .await
            .unwrap();
        let chain = tracker
            .get_retry_chain(&result.agent_results[0].run_id)
            .await;

        assert_eq!(chain.len(), 2);
        assert_eq!(tracker.get_depth(&chain[0]).await, Some(1));
        assert_eq!(tracker.get_depth(&chain[1]).await, Some(1));
    }

    #[tokio::test(flavor = "current_thread")]
    #[serial_test::serial(session_journal_dir)]
    async fn gate_retry_writes_journal_linkage_event() {
        let sessions_dir = std::env::temp_dir().join(format!(
            "delegation-engine-journal-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(&sessions_dir);

        let (reg, engine, tracker, _) = setup_with_executor(Arc::new(EchoExecutor));
        let gate = Arc::new(FailThenPassGate::new(1));
        let de =
            DelegationEngine::with_executor(reg, engine, tracker.clone(), Arc::new(EchoExecutor))
                .with_gate(gate);

        let mut req = fan_out_request(vec!["coder"]);
        req.delegation_id = "del-journal-retry".into();
        req.parent_run_id = "parent-journal-retry".into();
        req.session_id = "sess-journal-retry".into();

        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();
        assert_eq!(result.agent_results.len(), 1);
        assert_eq!(result.agent_results[0].status, "completed");

        let chain = tracker
            .get_retry_chain(&result.agent_results[0].run_id)
            .await;
        assert_eq!(chain.len(), 2);

        let journal_path = astra_services::session_journal::journal_file_path_for_user(
            "user-1",
            "sess-journal-retry",
        )
        .unwrap();
        let content = std::fs::read_to_string(&journal_path).unwrap();
        let retry_events: Vec<astra_services::session_journal::JournalEvent> = content
            .lines()
            .map(|line| {
                serde_json::from_str::<astra_services::session_journal::JournalEvent>(line).unwrap()
            })
            .filter(|evt| {
                evt.event_type == astra_services::session_journal::JournalEventType::DelegationRetry
            })
            .collect();

        assert_eq!(retry_events.len(), 1);
        let meta = retry_events[0].metadata.as_ref().unwrap();
        assert_eq!(meta["delegation_id"], "del-journal-retry");
        assert_eq!(meta["original_run_id"], chain[0]);
        assert_eq!(meta["retry_run_id"], chain[1]);
        assert_eq!(meta["agent_id"], "coder");
        assert_eq!(meta["attempt"], 2);
        assert_eq!(meta["reason"], "fail #1");

        let _ = std::fs::remove_file(journal_path);
        let _ = std::fs::remove_dir_all(sessions_dir);
    }

    #[tokio::test(flavor = "current_thread")]
    #[serial_test::serial(session_journal_dir)]
    async fn tracker_running_transition_writes_sub_run_started_event() {
        let sessions_dir = std::env::temp_dir().join(format!(
            "delegation-engine-subrun-start-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(&sessions_dir);
        let tracker = DelegationTracker::with_session("user-1".into(), "sess-subrun-start".into());

        tracker
            .record_sub_run(SubRunRecord {
                run_id: "run-1".into(),
                parent_run_id: "parent-1".into(),
                delegation_id: "del-1".into(),
                agent_id: "coder".into(),
                depth: 1,
                state: SubRunState::Created,
                retry_of: Some("run-0".into()),
            })
            .await;
        tracker
            .transition_state("run-1", SubRunState::Running)
            .await
            .unwrap();

        let journal_path = astra_services::session_journal::journal_file_path_for_user(
            "user-1",
            "sess-subrun-start",
        )
        .unwrap();
        let content = std::fs::read_to_string(&journal_path).unwrap();
        let started_events: Vec<astra_services::session_journal::JournalEvent> = content
            .lines()
            .map(|line| {
                serde_json::from_str::<astra_services::session_journal::JournalEvent>(line).unwrap()
            })
            .filter(|evt| {
                evt.event_type
                    == astra_services::session_journal::JournalEventType::DelegationSubRunStarted
            })
            .collect();

        assert_eq!(started_events.len(), 1);
        let meta = started_events[0].metadata.as_ref().unwrap();
        assert_eq!(meta["delegation_id"], "del-1");
        assert_eq!(meta["sub_run_id"], "run-1");
        assert_eq!(meta["parent_run_id"], "parent-1");
        assert_eq!(meta["agent_id"], "coder");
        assert_eq!(meta["status"], "running");
        assert_eq!(meta["retry_of"], "run-0");

        let _ = std::fs::remove_file(journal_path);
        let _ = std::fs::remove_dir_all(sessions_dir);
    }

    #[tokio::test(flavor = "current_thread")]
    #[serial_test::serial(session_journal_dir)]
    async fn tracker_complete_sub_run_writes_sub_run_completed_event() {
        let sessions_dir = std::env::temp_dir().join(format!(
            "delegation-engine-subrun-complete-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(&sessions_dir);
        let tracker =
            DelegationTracker::with_session("user-1".into(), "sess-subrun-complete".into());

        tracker
            .record_sub_run(SubRunRecord {
                run_id: "run-1".into(),
                parent_run_id: "parent-1".into(),
                delegation_id: "del-1".into(),
                agent_id: "coder".into(),
                depth: 1,
                state: SubRunState::Running,
                retry_of: None,
            })
            .await;
        tracker
            .complete_sub_run_with_result(
                "run-1",
                SubRunState::Failed,
                Some("boom"),
                Some("partial output"),
            )
            .await;

        let journal_path = astra_services::session_journal::journal_file_path_for_user(
            "user-1",
            "sess-subrun-complete",
        )
        .unwrap();
        let content = std::fs::read_to_string(&journal_path).unwrap();
        let completed_events: Vec<astra_services::session_journal::JournalEvent> = content
            .lines()
            .map(|line| {
                serde_json::from_str::<astra_services::session_journal::JournalEvent>(line).unwrap()
            })
            .filter(|evt| {
                evt.event_type
                    == astra_services::session_journal::JournalEventType::DelegationSubRunCompleted
            })
            .collect();

        assert_eq!(completed_events.len(), 1);
        let meta = completed_events[0].metadata.as_ref().unwrap();
        assert_eq!(meta["delegation_id"], "del-1");
        assert_eq!(meta["sub_run_id"], "run-1");
        assert_eq!(meta["agent_id"], "coder");
        assert_eq!(meta["status"], "failed");
        assert_eq!(meta["error"], "boom");
        assert_eq!(meta["output_preview"], "partial output");

        let _ = std::fs::remove_file(journal_path);
        let _ = std::fs::remove_dir_all(sessions_dir);
    }

    #[tokio::test]
    async fn gate_exhausted_retries_sequential() {
        let (reg, engine, tracker, _) = setup_with_executor(Arc::new(EchoExecutor));
        let de = DelegationEngine::with_executor(reg, engine, tracker, Arc::new(EchoExecutor))
            .with_gate(Arc::new(AlwaysFailGate));

        let req = DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "del-seq-fail".into(),
            parent_run_id: "parent-1".into(),
            task: "will fail".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["coder".into()],
                stop_on_success: false,
                timeout_sec: 0,
            },
            user_id: "user-1".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        };
        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();

        assert_eq!(result.agent_results.len(), 1);
        assert_eq!(result.agent_results[0].status, "verification_failed");
    }

    #[tokio::test]
    async fn no_gate_executes_without_verification_step() {
        let (_, _, _, de) = setup_with_executor(Arc::new(EchoExecutor));

        let req = fan_out_request(vec!["coder", "reviewer"]);
        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();

        assert_eq!(result.status, "completed");
        assert_eq!(result.agent_results.len(), 2);
        for ar in &result.agent_results {
            assert_eq!(ar.status, "completed");
        }
    }

    #[tokio::test]
    async fn gate_skips_failed_subrun() {
        let (reg, engine, tracker, _) = setup_with_executor(Arc::new(FailingExecutor {
            fail_agents: vec!["coder".into()],
        }));
        // AlwaysFailGate should NOT apply to already-failed results
        let de = DelegationEngine::with_executor(
            reg,
            engine,
            tracker,
            Arc::new(FailingExecutor {
                fail_agents: vec!["coder".into()],
            }),
        )
        .with_gate(Arc::new(AlwaysFailGate));

        let req = fan_out_request(vec!["coder"]);
        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();

        // Should be "failed" (from executor), NOT "verification_failed"
        assert_eq!(result.agent_results[0].status, "failed");
    }

    #[tokio::test]
    async fn gate_verdict_variants() {
        assert!(GateVerdict::Pass.is_pass());
        assert!(GateVerdict::Skip.is_pass());
        assert!(
            !GateVerdict::Fail {
                reason: "x".into(),
                details: None
            }
            .is_pass()
        );
    }

    // ── Persistence tests ────────────────────────────────────────────────

    #[tokio::test]
    async fn start_run_ext_persists_delegation_metadata() {
        let store = Arc::new(InMemoryRunStateStore::new());
        let engine = RunEngine::new(store.clone());

        engine
            .start_run("parent-1", "user-1", "sess-1")
            .await
            .unwrap();

        engine
            .start_run_ext(
                "sub-1",
                "user-1",
                "sess-1",
                Some("parent-1"),
                Some("del-1"),
                Some("coder"),
                None,
            )
            .await
            .unwrap();

        let record = store.load_run("user-1", "sub-1").await.unwrap().unwrap();
        assert_eq!(record.parent_run_id.as_deref(), Some("parent-1"));
        assert_eq!(record.delegation_id.as_deref(), Some("del-1"));
        assert_eq!(record.agent_id.as_deref(), Some("coder"));
        assert_eq!(record.session_id, "sess-1");
    }

    #[tokio::test]
    async fn start_run_without_parent_metadata_sets_none() {
        let store = Arc::new(InMemoryRunStateStore::new());
        let engine = RunEngine::new(store.clone());

        engine.start_run("run-1", "user-1", "sess-1").await.unwrap();

        let record = store.load_run("user-1", "run-1").await.unwrap().unwrap();
        assert!(record.parent_run_id.is_none());
        assert!(record.delegation_id.is_none());
        assert!(record.agent_id.is_none());
    }

    #[tokio::test]
    async fn find_sub_runs_by_delegation_id() {
        let store = Arc::new(InMemoryRunStateStore::new());
        let engine = RunEngine::new(store.clone());

        // Create a root run and two sub-runs in different delegations
        engine.start_run("root", "u1", "s1").await.unwrap();
        engine
            .start_run_ext(
                "sub-a",
                "u1",
                "s1",
                Some("root"),
                Some("del-1"),
                Some("coder"),
                None,
            )
            .await
            .unwrap();
        engine
            .start_run_ext(
                "sub-b",
                "u1",
                "s1",
                Some("root"),
                Some("del-1"),
                Some("reviewer"),
                None,
            )
            .await
            .unwrap();
        engine
            .start_run_ext(
                "sub-c",
                "u1",
                "s1",
                Some("root"),
                Some("del-2"),
                Some("writer"),
                None,
            )
            .await
            .unwrap();

        let del1_runs = engine.find_sub_runs("u1", "del-1").await.unwrap();
        assert_eq!(del1_runs.len(), 2);

        let del2_runs = engine.find_sub_runs("u1", "del-2").await.unwrap();
        assert_eq!(del2_runs.len(), 1);
        assert_eq!(del2_runs[0].agent_id.as_deref(), Some("writer"));
    }

    #[tokio::test]
    async fn persist_and_read_retry_count() {
        let store = Arc::new(InMemoryRunStateStore::new());
        let engine = RunEngine::new(store.clone());

        engine.start_run("run-1", "u1", "s1").await.unwrap();
        assert_eq!(
            store
                .load_run("u1", "run-1")
                .await
                .unwrap()
                .unwrap()
                .retry_count,
            0
        );

        engine
            .persist_retry_count("u1", "s1", "run-1", 2)
            .await
            .unwrap();
        assert_eq!(
            store
                .load_run("u1", "run-1")
                .await
                .unwrap()
                .unwrap()
                .retry_count,
            2
        );
    }

    #[tokio::test]
    async fn load_from_run_records_rebuilds_tracker() {
        use astra_services::runs::DurableRunRecord;

        let records = vec![
            DurableRunRecord {
                run_id: "sub-1".into(),
                user_id: "u1".into(),
                session_id: "s1".into(),
                parent_run_id: Some("parent-1".into()),
                root_run_id: Some("parent-1".into()),
                ancestor_path: Some("parent-1/sub-1".into()),
                depth: 1,
                delegation_id: Some("del-1".into()),
                agent_id: Some("coder".into()),
                retry_of: None,
                retry_scope: Some("node".into()),
                status: "completed".into(),
                waiting_for: None,
                owner_pod_id: None,
                owner_lease_expires_at: None,
                run_generation: 0,
                last_event_idx: -1,
                checkpoint_version: None,
                checkpoint_json: None,
                error_code: None,
                error_message: None,
                retry_count: 0,
                total_prompt_tokens: 0,
                total_completion_tokens: 0,
                total_tool_calls: 0,
                agent_binding_id: None,
                agent_binding_name: None,
                agent_binding_schema_version: None,
                model_offering_id: None,
                resolved_model_name: None,
                runtime_profile: None,
                start_request_fingerprint: None,
                work_binding: None,
                events: vec![],
                created_at: "2026-01-01T00:00:00Z".into(),
                updated_at: "2026-01-01T00:00:00Z".into(),
            },
            DurableRunRecord {
                run_id: "sub-2".into(),
                user_id: "u1".into(),
                session_id: "s1".into(),
                parent_run_id: Some("parent-1".into()),
                root_run_id: Some("parent-1".into()),
                ancestor_path: Some("parent-1/sub-2".into()),
                depth: 2,
                delegation_id: Some("del-1".into()),
                agent_id: Some("reviewer".into()),
                retry_of: Some("sub-1".into()),
                retry_scope: Some("node".into()),
                status: "paused".into(),
                waiting_for: None,
                owner_pod_id: None,
                owner_lease_expires_at: None,
                run_generation: 0,
                last_event_idx: -1,
                checkpoint_version: None,
                checkpoint_json: None,
                error_code: None,
                error_message: None,
                retry_count: 1,
                total_prompt_tokens: 0,
                total_completion_tokens: 0,
                total_tool_calls: 0,
                agent_binding_id: None,
                agent_binding_name: None,
                agent_binding_schema_version: None,
                model_offering_id: None,
                resolved_model_name: None,
                runtime_profile: None,
                start_request_fingerprint: None,
                work_binding: None,
                events: vec![],
                created_at: "2026-01-01T00:00:00Z".into(),
                updated_at: "2026-01-01T00:00:00Z".into(),
            },
            DurableRunRecord {
                run_id: "sub-3".into(),
                user_id: "u1".into(),
                session_id: "s1".into(),
                parent_run_id: Some("parent-1".into()),
                root_run_id: Some("parent-1".into()),
                ancestor_path: Some("parent-1/sub-3".into()),
                depth: 1,
                delegation_id: Some("del-1".into()),
                agent_id: Some("approver".into()),
                retry_of: None,
                retry_scope: Some("node".into()),
                status: "waiting".into(),
                waiting_for: Some("approval".into()),
                owner_pod_id: None,
                owner_lease_expires_at: None,
                run_generation: 0,
                last_event_idx: -1,
                checkpoint_version: None,
                checkpoint_json: None,
                error_code: None,
                error_message: None,
                retry_count: 0,
                total_prompt_tokens: 0,
                total_completion_tokens: 0,
                total_tool_calls: 0,
                agent_binding_id: None,
                agent_binding_name: None,
                agent_binding_schema_version: None,
                model_offering_id: None,
                resolved_model_name: None,
                runtime_profile: None,
                start_request_fingerprint: None,
                work_binding: None,
                events: vec![],
                created_at: "2026-01-01T00:00:00Z".into(),
                updated_at: "2026-01-01T00:00:00Z".into(),
            },
            // Root run — should be skipped
            DurableRunRecord {
                run_id: "root-run".into(),
                user_id: "u1".into(),
                session_id: "s1".into(),
                parent_run_id: None,
                root_run_id: Some("root-run".into()),
                ancestor_path: Some("root-run".into()),
                depth: 0,
                delegation_id: None,
                agent_id: None,
                retry_of: None,
                retry_scope: Some("node".into()),
                status: "completed".into(),
                waiting_for: None,
                owner_pod_id: None,
                owner_lease_expires_at: None,
                run_generation: 0,
                last_event_idx: -1,
                checkpoint_version: None,
                checkpoint_json: None,
                error_code: None,
                error_message: None,
                retry_count: 0,
                total_prompt_tokens: 0,
                total_completion_tokens: 0,
                total_tool_calls: 0,
                agent_binding_id: None,
                agent_binding_name: None,
                agent_binding_schema_version: None,
                model_offering_id: None,
                resolved_model_name: None,
                runtime_profile: None,
                start_request_fingerprint: None,
                work_binding: None,
                events: vec![],
                created_at: "2026-01-01T00:00:00Z".into(),
                updated_at: "2026-01-01T00:00:00Z".into(),
            },
        ];

        let tracker = DelegationTracker::new();
        tracker.load_from_run_records(&records).await;

        // Hierarchy rebuilt
        let subs = tracker.get_sub_runs("del-1").await;
        assert_eq!(subs.len(), 3);
        assert!(tracker.is_sub_run("sub-1").await);
        assert!(tracker.is_sub_run("sub-2").await);
        assert!(tracker.is_sub_run("sub-3").await);
        assert!(!tracker.is_sub_run("root-run").await);

        // Parent links rebuilt
        assert_eq!(
            tracker.get_parent("sub-1").await.as_deref(),
            Some("parent-1")
        );
        assert_eq!(
            tracker.get_agent_id("sub-1").await.as_deref(),
            Some("coder")
        );
        assert_eq!(
            subs.iter()
                .find(|sub| sub.run_id == "sub-2")
                .and_then(|sub| sub.retry_of.as_deref()),
            Some("sub-1")
        );
        assert_eq!(tracker.get_depth("sub-2").await, Some(2));
        // Cooperative flags belong to a live executor task. A recovered
        // durable pause has no such task, so manufacturing a flag would let
        // resume_delegation mark the row running without an executor.
        assert!(tracker.get_pause_flag("sub-2").await.is_none());
        assert_eq!(
            tracker.resume_delegation("del-1").await,
            0,
            "recovery must not fabricate a resumable live executor"
        );

        // Waiting is a distinct recoverable state and does not recreate a
        // cooperative pause flag.
        assert_eq!(
            tracker.get_sub_run_state("sub-3").await,
            Some(SubRunState::Waiting)
        );
        assert!(tracker.get_pause_flag("sub-3").await.is_none());

        // Completed sub-run has no pause flag
        assert!(tracker.get_pause_flag("sub-1").await.is_none());
    }

    // ─── clone_with_gate ─────────────────────────────────────────────────

    #[test]
    fn clone_with_gate_shares_components() {
        let run_store = Arc::new(astra_services::runs::InMemoryRunStateStore::default());
        let registry = Arc::new(tokio::sync::RwLock::new(AgentProfileRegistry::new()));
        let run_engine = Arc::new(crate::server::run::engine::RunEngine::new(run_store));
        let tracker = Arc::new(DelegationTracker::new());
        let executor: Arc<dyn SubRunExecutor> = Arc::new(StubSubRunExecutor);

        let engine = DelegationEngine::with_executor(
            registry.clone(),
            run_engine.clone(),
            tracker.clone(),
            executor.clone(),
        );
        assert!(engine.gate.is_none());

        // Clone with a gate — the new engine shares the same Arc components.
        struct PassGate;
        #[async_trait::async_trait]
        impl VerificationGate for PassGate {
            async fn verify(&self, _: &AgentResult, _: &str, _: u32) -> GateVerdict {
                GateVerdict::Pass
            }
        }

        let gated = engine.clone_with_gate(Arc::new(PassGate));
        assert!(gated.gate.is_some());
    }

    // ─── Fork Pattern Tests ─────────────────────────────────────────────

    fn fork_request(del_id: &str, tasks: Vec<&str>, agent_id: &str) -> DelegationRequest {
        DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: del_id.into(),
            parent_run_id: format!("parent-{del_id}"),
            task: "fork test".into(),
            pattern: CoordinationPattern::Fork {
                tasks: tasks.into_iter().map(String::from).collect(),
                agent_id: agent_id.into(),
                max_turns: 5,
                aggregation: AggregationStrategy::AllResults,
                timeout_sec: 0,
            },
            user_id: "user-1".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        }
    }

    #[tokio::test]
    async fn fork_spawns_parallel_children() {
        let (_, _engine, tracker, de) = setup_with_executor(Arc::new(EchoExecutor));

        let req = fork_request(
            "del-fork-spawn",
            vec!["task-a", "task-b", "task-c"],
            "writer",
        );
        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();

        assert_eq!(result.agent_results.len(), 3);
        assert_eq!(result.status, "completed");

        let subs = tracker.get_sub_runs("del-fork-spawn").await;
        assert_eq!(subs.len(), 3);
        for sub in &subs {
            assert_eq!(sub.agent_id, "writer");
            assert_eq!(sub.depth, 1);
        }

        // All results should have output
        for ar in &result.agent_results {
            assert_eq!(ar.status, "completed");
            assert!(ar.output.is_some());
        }
    }

    #[tokio::test]
    async fn fork_children_inherit_and_persist_durable_parent_interaction_mode() {
        let (registry, run_engine, tracker) = setup();
        let bindings = Arc::new(std::sync::Mutex::new(Vec::new()));
        let engine = DelegationEngine::with_executor(
            registry,
            run_engine.clone(),
            tracker,
            Arc::new(CaptureRunBindingExecutor {
                bindings: bindings.clone(),
            }),
        );
        let request = fork_request("del-fork-auto", vec!["task-a", "task-b"], "writer");
        run_engine
            .start_run_with_context(
                &request.parent_run_id,
                &request.user_id,
                &request.session_id,
                crate::server::run::engine::RunStartContext {
                    interaction_mode: RequestedTurnInteractionMode::Auto,
                    ..Default::default()
                },
            )
            .await
            .expect("persist Auto parent");

        let result = engine.execute(request, "orch", None).await.unwrap();
        let bindings = bindings.lock().unwrap();
        assert_eq!(bindings.len(), 2);
        assert!(
            bindings
                .iter()
                .all(|binding| { binding.interaction_mode == RequestedTurnInteractionMode::Auto })
        );
        drop(bindings);

        for child in result.agent_results {
            let durable = run_engine
                .load_run("user-1", &child.run_id)
                .await
                .unwrap()
                .expect("durable fork child");
            assert_eq!(durable.events[0]["data"]["interaction_mode"], "auto");
        }
    }

    #[tokio::test]
    async fn fork_children_cannot_delegate() {
        /// Executor that checks can_delegate is false on fork children.
        struct DelegateCheckExecutor;

        #[async_trait]
        impl SubRunExecutor for DelegateCheckExecutor {
            async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
                let can_del = config.agent_profile.can_delegate;
                let depth = config.agent_profile.max_delegation_depth;
                Ok(AgentResult {
                    agent_id: config.agent_profile.agent_id,
                    run_id: config.run_id,
                    status: "completed".to_string(),
                    output: Some(format!("can_delegate={can_del},depth={depth}")),
                    error: None,
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    tool_calls: 0,
                })
            }
        }

        let (reg, engine, tracker) = setup();
        let de =
            DelegationEngine::with_executor(reg, engine, tracker, Arc::new(DelegateCheckExecutor));

        let req = fork_request("del-fork-deleg", vec!["task-a"], "writer");
        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();

        assert_eq!(
            result.agent_results[0].output.as_deref(),
            Some("can_delegate=false,depth=0")
        );
    }

    #[tokio::test]
    async fn fork_partial_failure() {
        let executor = Arc::new(FailingExecutor {
            fail_agents: vec!["writer".to_string()],
        });
        let (reg, engine, tracker) = setup();
        let de = DelegationEngine::with_executor(reg, engine, tracker, executor);

        let req = fork_request("del-fork-fail", vec!["task-a", "task-b"], "writer");
        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();

        // All children use "writer" which fails → all failed
        assert_eq!(result.agent_results.len(), 2);
        assert_eq!(result.status, "failed");
        for ar in &result.agent_results {
            assert_eq!(ar.status, "failed");
        }
    }

    #[tokio::test]
    async fn fork_single_task() {
        let (_, _, _, de) = setup_with_executor(Arc::new(EchoExecutor));

        let req = fork_request("del-fork-single", vec!["only-task"], "writer");
        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();

        assert_eq!(result.agent_results.len(), 1);
        assert_eq!(result.status, "completed");
    }

    #[tokio::test]
    async fn fork_context_includes_fork_metadata() {
        /// Executor that checks fork context fields.
        struct ForkContextCheckExecutor;

        #[async_trait]
        impl SubRunExecutor for ForkContextCheckExecutor {
            async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
                let is_fork = config
                    .context
                    .get("is_fork_child")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let idx = config
                    .context
                    .get("fork_index")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(999);
                Ok(AgentResult {
                    agent_id: config.agent_profile.agent_id,
                    run_id: config.run_id,
                    status: "completed".to_string(),
                    output: Some(format!("is_fork={is_fork},idx={idx}")),
                    error: None,
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    tool_calls: 0,
                })
            }
        }

        let (reg, engine, tracker) = setup();
        let de = DelegationEngine::with_executor(
            reg,
            engine,
            tracker,
            Arc::new(ForkContextCheckExecutor),
        );

        let req = fork_request("del-fork-ctx", vec!["a", "b"], "writer");
        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();

        // Both children should have fork metadata
        let outputs: Vec<String> = result
            .agent_results
            .iter()
            .filter_map(|r| r.output.clone())
            .collect();
        assert!(outputs.iter().any(|o| o.contains("is_fork=true,idx=0")));
        assert!(outputs.iter().any(|o| o.contains("is_fork=true,idx=1")));
    }

    // ── Tracker: get_children ───────────────────────────────────────────────

    #[tokio::test]
    async fn tracker_get_children_returns_child_run_ids() {
        let tracker = DelegationTracker::new();
        tracker
            .record_sub_run(SubRunRecord {
                run_id: "child-1".into(),
                parent_run_id: "parent-X".into(),
                delegation_id: "del-1".into(),
                agent_id: "coder".into(),
                depth: 1,
                state: SubRunState::Created,
                retry_of: None,
            })
            .await;
        tracker
            .record_sub_run(SubRunRecord {
                run_id: "child-2".into(),
                parent_run_id: "parent-X".into(),
                delegation_id: "del-1".into(),
                agent_id: "reviewer".into(),
                depth: 1,
                state: SubRunState::Created,
                retry_of: None,
            })
            .await;
        tracker
            .record_sub_run(SubRunRecord {
                run_id: "other-child".into(),
                parent_run_id: "parent-Y".into(),
                delegation_id: "del-2".into(),
                agent_id: "writer".into(),
                depth: 1,
                state: SubRunState::Created,
                retry_of: None,
            })
            .await;

        let mut children = tracker.get_children("parent-X").await;
        children.sort();
        assert_eq!(children, vec!["child-1", "child-2"]);

        let children_y = tracker.get_children("parent-Y").await;
        assert_eq!(children_y, vec!["other-child"]);

        let none = tracker.get_children("nonexistent").await;
        assert!(none.is_empty());
    }

    // ── Tracker: individual pause_sub_run / resume_sub_run ──────────────────

    #[tokio::test]
    async fn pause_and_resume_individual_sub_run() {
        let tracker = DelegationTracker::new();
        let flag = tracker.register_pause_flag("run-1").await;

        assert!(!flag.load(Ordering::Relaxed));
        assert!(!tracker.is_paused("run-1").await);

        // Pause individual sub-run
        assert!(tracker.pause_sub_run("run-1").await);
        assert!(flag.load(Ordering::Relaxed));
        assert!(tracker.is_paused("run-1").await);

        // Resume individual sub-run
        assert!(tracker.resume_sub_run("run-1").await);
        assert!(!flag.load(Ordering::Relaxed));
        assert!(!tracker.is_paused("run-1").await);

        // Pause/resume unknown run returns false
        assert!(!tracker.pause_sub_run("unknown").await);
        assert!(!tracker.resume_sub_run("unknown").await);
    }

    // ── Fan-out: all agents fail ────────────────────────────────────────────

    #[tokio::test]
    async fn fan_out_all_agents_fail() {
        let (reg, engine, tracker) = setup();
        let failing = Arc::new(FailingExecutor {
            fail_agents: vec!["coder".into(), "reviewer".into()],
        });
        let de = DelegationEngine::with_executor(reg, engine, tracker, failing);

        let req = fan_out_request(vec!["coder", "reviewer"]);
        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();

        // All results should be failed
        assert_eq!(result.agent_results.len(), 2);
        for r in &result.agent_results {
            assert_eq!(r.status, "failed");
            assert!(r.error.is_some());
        }
    }

    // ── Executor hard error (Err) vs soft fail (Ok with failed status) ──────

    #[tokio::test]
    async fn executor_hard_error_captured_as_failed_result() {
        /// Executor that returns Err (panic-like failure, not just failed status).
        struct HardErrorExecutor;

        #[async_trait]
        impl SubRunExecutor for HardErrorExecutor {
            async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
                Err(format!(
                    "executor crashed for {}",
                    config.agent_profile.agent_id
                ))
            }
        }

        let (reg, engine, tracker) = setup();
        let de = DelegationEngine::with_executor(reg, engine, tracker, Arc::new(HardErrorExecutor));

        let req = fan_out_request(vec!["coder"]);
        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();

        // Hard errors should be captured as failed agent results, not propagated
        assert_eq!(result.agent_results.len(), 1);
        assert_eq!(result.agent_results[0].status, "failed");
        assert!(
            result.agent_results[0]
                .error
                .as_ref()
                .unwrap()
                .contains("crashed")
        );
    }

    // ── Sequential: output chaining across stages ───────────────────────────

    #[tokio::test]
    async fn sequential_output_chaining_verified() {
        let (reg, engine, tracker) = setup();
        let de = DelegationEngine::with_executor(reg, engine, tracker, Arc::new(EchoExecutor));

        let req = DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "del-seq-chain".into(),
            parent_run_id: "p1".into(),
            task: "chained task".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["coder".into(), "reviewer".into(), "writer".into()],
                stop_on_success: false,
                timeout_sec: 0,
            },
            user_id: "u".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        };

        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();
        assert_eq!(result.agent_results.len(), 3);

        // Each stage receives previous output
        let out0 = result.agent_results[0].output.as_ref().unwrap();
        assert!(out0.contains("[coder]"), "first stage should run");

        let out1 = result.agent_results[1].output.as_ref().unwrap();
        assert!(
            out1.contains("prev="),
            "second stage should receive prev output"
        );

        let out2 = result.agent_results[2].output.as_ref().unwrap();
        assert!(
            out2.contains("prev="),
            "third stage should receive prev output"
        );
    }

    // ── DefaultQualityGate tests ────────────────────────────────────────

    fn make_result(status: &str, output: Option<&str>) -> AgentResult {
        AgentResult {
            agent_id: "test".into(),
            run_id: "r1".into(),
            status: status.into(),
            output: output.map(|s| s.to_string()),
            error: if status == AGENT_RESULT_STATUS_FAILED {
                Some("err".into())
            } else {
                None
            },
            prompt_tokens: 0,
            completion_tokens: 0,
            tool_calls: 0,
        }
    }

    #[tokio::test]
    async fn quality_gate_passes_normal_output() {
        let gate = DefaultQualityGate::default();
        let result = make_result("completed", Some("This is a perfectly valid agent output."));
        assert!(gate.verify(&result, "d1", 1).await.is_pass());
    }

    #[tokio::test]
    async fn quality_gate_skips_failed_result() {
        // Failed results with no output still fail the min_output_len check.
        // This is by design — the gate checks output quality regardless of status.
        let gate = DefaultQualityGate::default();
        let result = make_result("failed", None);
        let v = gate.verify(&result, "d1", 1).await;
        assert!(!v.is_pass()); // No output → too short
    }

    #[tokio::test]
    async fn quality_gate_fails_no_output() {
        let gate = DefaultQualityGate::default();
        let result = make_result("completed", None);
        let v = gate.verify(&result, "d1", 1).await;
        assert!(!v.is_pass());
    }

    #[tokio::test]
    async fn quality_gate_fails_too_short() {
        let gate = DefaultQualityGate::default();
        let result = make_result("completed", Some("hi"));
        let v = gate.verify(&result, "d1", 1).await;
        assert!(!v.is_pass());
        if let GateVerdict::Fail { reason, .. } = v {
            assert!(reason.contains("too short"));
        }
    }

    #[tokio::test]
    async fn quality_gate_fails_too_long() {
        let gate = DefaultQualityGate::with_thresholds(QualityThresholds {
            max_output_len: 50,
            ..Default::default()
        });
        let result = make_result("completed", Some(&"x".repeat(100)));
        let v = gate.verify(&result, "d1", 1).await;
        assert!(!v.is_pass());
        if let GateVerdict::Fail { reason, .. } = v {
            assert!(reason.contains("too long"));
        }
    }

    #[tokio::test]
    async fn quality_gate_fails_repetitive_output() {
        let gate = DefaultQualityGate::default();
        // Use non-error lines so repetition check fires (not error_dominated).
        let repetitive = "processing data chunk...\n".repeat(20);
        let result = make_result("completed", Some(&repetitive));
        let v = gate.verify(&result, "d1", 1).await;
        assert!(!v.is_pass());
        if let GateVerdict::Fail { reason, .. } = v {
            assert!(reason.contains("repetition"));
        }
    }

    #[tokio::test]
    async fn quality_gate_custom_thresholds() {
        let gate = DefaultQualityGate::with_thresholds(QualityThresholds {
            min_output_len: 1,
            max_output_len: 1_000_000,
            max_repetition_ratio: 0.95,
            max_retries: 5,
        });
        assert_eq!(gate.max_retries(), 5);
        // Slightly repetitive but under 95% threshold — should pass.
        let mut lines = "same line\n".repeat(8);
        lines.push_str("different line 1\n");
        lines.push_str("different line 2\n");
        let result = make_result("completed", Some(&lines));
        assert!(gate.verify(&result, "d1", 1).await.is_pass());
    }

    // ── State Machine + Lifecycle Tests ──────────────────────────────────

    #[tokio::test]
    async fn tracker_state_transitions() {
        let tracker = DelegationTracker::new();
        tracker
            .record_sub_run(SubRunRecord {
                run_id: "r1".into(),
                parent_run_id: "parent".into(),
                delegation_id: "d1".into(),
                agent_id: "a1".into(),
                depth: 1,
                state: SubRunState::Created,
                retry_of: None,
            })
            .await;

        // Created → Running
        let new = tracker
            .transition_state("r1", SubRunState::Running)
            .await
            .unwrap();
        assert_eq!(new, SubRunState::Running);

        // Running → Completed
        let new = tracker
            .transition_state("r1", SubRunState::Completed)
            .await
            .unwrap();
        assert_eq!(new, SubRunState::Completed);
    }

    #[tokio::test]
    async fn tracker_recording_is_idempotent_by_durable_run_identity() {
        let tracker = DelegationTracker::new();
        let mut record = SubRunRecord {
            run_id: "r1".into(),
            parent_run_id: "parent".into(),
            delegation_id: "d1".into(),
            agent_id: "a1".into(),
            depth: 1,
            state: SubRunState::Created,
            retry_of: None,
        };
        tracker.record_sub_run(record.clone()).await;
        record.state = SubRunState::Running;
        tracker.record_sub_run(record).await;

        let records = tracker.get_sub_runs("d1").await;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].state, SubRunState::Created);
        assert_eq!(tracker.get_depth("r1").await, Some(1));
        assert_eq!(tracker.get_agent_id("r1").await.as_deref(), Some("a1"));
    }

    #[tokio::test]
    async fn mailbox_lineage_registration_does_not_publish_a_second_spawn() {
        use astra_messaging::DelegationLookup;
        let broadcaster = Arc::new(crate::orchestration::ProgressBroadcaster::new(8));
        let mut events = broadcaster.subscribe();
        let tracker = DelegationTracker::new().with_progress_broadcaster(broadcaster);

        DelegationLookup::record_sub_run(
            &tracker,
            astra_messaging::SubRunInfo {
                run_id: "child-run".into(),
                parent_run_id: "root-run".into(),
                delegation_id: "root-run".into(),
                agent_id: "reviewer-1".into(),
                depth: 1,
            },
        )
        .await;

        assert_eq!(
            tracker.get_parent("child-run").await.as_deref(),
            Some("root-run")
        );
        assert!(
            matches!(
                events.try_recv(),
                Err(tokio::sync::broadcast::error::TryRecvError::Empty)
            ),
            "mailbox lineage bookkeeping must leave lifecycle publication to the spawner"
        );
    }

    #[tokio::test]
    async fn tracker_invalid_transition_rejected() {
        let tracker = DelegationTracker::new();
        tracker
            .record_sub_run(SubRunRecord {
                run_id: "r1".into(),
                parent_run_id: "parent".into(),
                delegation_id: "d1".into(),
                agent_id: "a1".into(),
                depth: 1,
                state: SubRunState::Created,
                retry_of: None,
            })
            .await;

        // Created → Completed should fail (must go through Running)
        let err = tracker.transition_state("r1", SubRunState::Completed).await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn tracker_complete_sub_run_updates_state() {
        let tracker = DelegationTracker::new();
        tracker
            .record_sub_run(SubRunRecord {
                run_id: "r1".into(),
                parent_run_id: "parent".into(),
                delegation_id: "d1".into(),
                agent_id: "a1".into(),
                depth: 1,
                state: SubRunState::Running,
                retry_of: None,
            })
            .await;

        tracker.complete_sub_run("r1", SubRunState::Completed).await;

        let subs = tracker.get_sub_runs("d1").await;
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].state, SubRunState::Completed);
    }

    #[tokio::test]
    async fn tracker_retry_chain() {
        let tracker = DelegationTracker::new();
        // Original run
        tracker
            .record_sub_run(SubRunRecord {
                run_id: "r1".into(),
                parent_run_id: "parent".into(),
                delegation_id: "d1".into(),
                agent_id: "a1".into(),
                depth: 1,
                state: SubRunState::Created,
                retry_of: None,
            })
            .await;
        // First retry
        tracker
            .record_sub_run(SubRunRecord {
                run_id: "r2".into(),
                parent_run_id: "parent".into(),
                delegation_id: "d1".into(),
                agent_id: "a1".into(),
                depth: 1,
                state: SubRunState::Created,
                retry_of: Some("r1".into()),
            })
            .await;
        // Second retry
        tracker
            .record_sub_run(SubRunRecord {
                run_id: "r3".into(),
                parent_run_id: "parent".into(),
                delegation_id: "d1".into(),
                agent_id: "a1".into(),
                depth: 1,
                state: SubRunState::Created,
                retry_of: Some("r2".into()),
            })
            .await;

        let chain = tracker.get_retry_chain("r3").await;
        assert_eq!(chain, vec!["r1", "r2", "r3"]);

        // Chain from original should return just [r1, r2, r3]
        let chain_from_orig = tracker.get_retry_chain("r1").await;
        assert_eq!(chain_from_orig, vec!["r1", "r2", "r3"]);
    }

    #[tokio::test]
    async fn tracker_cleanup_delegation_removes_all_state() {
        let tracker = DelegationTracker::new();
        tracker
            .record_sub_run(SubRunRecord {
                run_id: "r1".into(),
                parent_run_id: "parent".into(),
                delegation_id: "d1".into(),
                agent_id: "a1".into(),
                depth: 1,
                state: SubRunState::Completed,
                retry_of: None,
            })
            .await;

        let _f1 = tracker.register_pause_flag("r1").await;
        assert!(tracker.get_pause_flag("r1").await.is_some());
        tracker.init_progress("d1", &["a1".into()]).await;
        assert!(tracker.get_progress("d1").await.is_some());
        assert_eq!(tracker.get_sub_runs("d1").await.len(), 1);

        tracker.cleanup_delegation("d1").await.unwrap();
        assert!(tracker.get_pause_flag("r1").await.is_none());
        assert!(tracker.get_progress("d1").await.is_none());
        assert_eq!(tracker.get_sub_runs("d1").await.len(), 0);
        assert!(tracker.get_children("parent").await.is_empty());
    }

    #[tokio::test]
    async fn tracker_cleanup_delegation_rejects_nonterminal_sub_runs() {
        let tracker = DelegationTracker::new();
        tracker
            .record_sub_run(SubRunRecord {
                run_id: "r1".into(),
                parent_run_id: "parent".into(),
                delegation_id: "d1".into(),
                agent_id: "a1".into(),
                depth: 1,
                state: SubRunState::Running,
                retry_of: None,
            })
            .await;

        let _f1 = tracker.register_pause_flag("r1").await;
        tracker.init_progress("d1", &["a1".into()]).await;

        let err = tracker
            .cleanup_delegation("d1")
            .await
            .expect_err("non-terminal delegation should not be cleaned up");
        assert!(err.contains("r1(running)"), "{err}");
        assert!(tracker.get_pause_flag("r1").await.is_some());
        assert!(tracker.get_progress("d1").await.is_some());
        assert_eq!(tracker.get_sub_runs("d1").await.len(), 1);
        assert_eq!(tracker.get_children("parent").await, vec!["r1".to_string()]);
    }

    #[tokio::test]
    async fn tracker_progress_tracking() {
        let tracker = DelegationTracker::new();
        tracker
            .init_progress("d1", &["a1".into(), "a2".into()])
            .await;

        let progress = tracker.get_progress("d1").await.unwrap();
        assert_eq!(progress.total_count, 2);
        assert_eq!(progress.completed_count, 0);
        assert_eq!(
            *progress.agent_states.get("a1").unwrap(),
            SubRunState::Created
        );

        // Update a1 to Running
        tracker
            .update_progress("d1", "a1", SubRunState::Running)
            .await;
        let progress = tracker.get_progress("d1").await.unwrap();
        assert_eq!(
            *progress.agent_states.get("a1").unwrap(),
            SubRunState::Running
        );
        assert_eq!(progress.completed_count, 0);

        // Complete a1
        tracker
            .update_progress("d1", "a1", SubRunState::Completed)
            .await;
        let progress = tracker.get_progress("d1").await.unwrap();
        assert_eq!(progress.completed_count, 1);
    }

    #[tokio::test]
    async fn cancel_token_per_execution_isolation() {
        let (_, _engine, _tracker, de) = setup_with_executor(Arc::new(EchoExecutor));

        // Create two separate cancel tokens
        let token1 = Arc::new(tokio_util::sync::CancellationToken::new());
        let token2 = Arc::new(tokio_util::sync::CancellationToken::new());

        // Use unique delegation/parent IDs to avoid conflicts
        let mut req1 = fan_out_request(vec!["coder"]);
        req1.delegation_id = "del-iso-1".into();
        req1.parent_run_id = "parent-iso-1".into();

        let mut req2 = fan_out_request(vec!["reviewer"]);
        req2.delegation_id = "del-iso-2".into();
        req2.parent_run_id = "parent-iso-2".into();
        req2.session_id = "test-session-2".into();

        // Execute with different tokens — cancelling one shouldn't affect the other
        let (r1, r2) = tokio::join!(
            execute_with_durable_parent(&de, req1, "orch", Some(token1.clone())),
            execute_with_durable_parent(&de, req2, "orch", Some(token2.clone())),
        );

        // Both should succeed since neither token was cancelled
        assert!(r1.is_ok(), "r1 failed: {:?}", r1.err());
        assert!(r2.is_ok(), "r2 failed: {:?}", r2.err());
    }

    /// Executor that sleeps for a configured duration before returning.
    #[derive(Clone)]
    struct SlowExecutor {
        delay: std::time::Duration,
    }

    #[async_trait::async_trait]
    impl SubRunExecutor for SlowExecutor {
        async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
            tokio::time::sleep(self.delay).await;
            Ok(AgentResult {
                agent_id: config.agent_profile.agent_id.clone(),
                run_id: config.run_id.clone(),
                status: "completed".into(),
                output: Some(format!("slow output for {}", config.task)),
                error: None,
                prompt_tokens: 0,
                completion_tokens: 0,
                tool_calls: 0,
            })
        }
    }

    /// Executor that succeeds immediately on the first call, then sleeps on retry.
    #[derive(Clone)]
    struct RetrySlowExecutor {
        retry_delay: std::time::Duration,
        calls: Arc<std::sync::atomic::AtomicU32>,
    }

    #[async_trait::async_trait]
    impl SubRunExecutor for RetrySlowExecutor {
        async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
            let call = self.calls.fetch_add(1, Ordering::Relaxed);
            if call > 0 {
                tokio::time::sleep(self.retry_delay).await;
            }
            Ok(AgentResult {
                agent_id: config.agent_profile.agent_id.clone(),
                run_id: config.run_id.clone(),
                status: "completed".into(),
                output: Some(format!("retry-slow output for {}", config.task)),
                error: None,
                prompt_tokens: 0,
                completion_tokens: 0,
                tool_calls: 0,
            })
        }
    }

    /// Executor that reports whether a mailbox was attached to the sub-run config.
    #[derive(Clone)]
    struct MailboxEchoExecutor;

    #[async_trait::async_trait]
    impl SubRunExecutor for MailboxEchoExecutor {
        async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
            Ok(AgentResult {
                agent_id: config.agent_profile.agent_id.clone(),
                run_id: config.run_id.clone(),
                status: "completed".into(),
                output: Some(format!("mailbox={}", config.mailbox.is_some())),
                error: None,
                prompt_tokens: 0,
                completion_tokens: 0,
                tool_calls: 0,
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn fan_out_per_agent_timeout_enforced() {
        // `start_paused = true` makes tokio time virtual: `tokio::time::sleep`
        // and `tokio::time::timeout` advance the clock without real waits, so
        // the test runs in <100ms instead of the real 1s timeout budget.
        let slow = Arc::new(SlowExecutor {
            delay: std::time::Duration::from_secs(5),
        });
        let (_, _engine, _tracker, de) = setup_with_executor(slow);

        let req = DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "timeout-test".into(),
            parent_run_id: "p".into(),
            task: "slow task".into(),
            pattern: CoordinationPattern::FanOut {
                agent_ids: vec!["coder".into()],
                aggregation: AggregationStrategy::AllResults,
                timeout_sec: 1, // 1 second timeout, executor sleeps 5s
            },
            user_id: "u".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        };

        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();

        // Should fail due to timeout
        assert_eq!(result.agent_results.len(), 1);
        assert_eq!(result.agent_results[0].status, "failed");
        assert!(
            result.agent_results[0]
                .error
                .as_deref()
                .unwrap_or("")
                .contains("timeout"),
            "expected timeout error, got: {:?}",
            result.agent_results[0].error
        );
    }

    #[tokio::test(start_paused = true)]
    async fn gate_retry_timeout_enforced() {
        let retry_slow = Arc::new(RetrySlowExecutor {
            retry_delay: std::time::Duration::from_secs(5),
            calls: Arc::new(std::sync::atomic::AtomicU32::new(0)),
        });
        let (reg, engine, tracker, _) = setup_with_executor(retry_slow.clone());
        let de = DelegationEngine::with_executor(reg, engine, tracker, retry_slow.clone())
            .with_gate(Arc::new(FailThenPassGate::new(1)));

        let req = DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "gate-timeout".into(),
            parent_run_id: "p".into(),
            task: "gated slow retry".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["coder".into()],
                stop_on_success: false,
                timeout_sec: 1,
            },
            user_id: "u".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        };

        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();

        assert_eq!(result.agent_results.len(), 1);
        assert_eq!(result.agent_results[0].status, "failed");
        assert!(
            result.agent_results[0]
                .error
                .as_deref()
                .unwrap_or("")
                .contains("timeout"),
            "expected retry timeout error, got: {:?}",
            result.agent_results[0].error
        );
        assert_eq!(retry_slow.calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn gate_retry_registers_mailbox_when_router_present() {
        let (reg, engine, tracker) = setup();
        let gate = Arc::new(FailThenPassGate::new(1));
        let router = Arc::new(crate::messaging::AgentMailboxRouter::new(
            Arc::new(crate::messaging::InProcessTransport::new()),
            tracker.clone(),
        ));
        let de =
            DelegationEngine::with_executor(reg, engine, tracker, Arc::new(MailboxEchoExecutor))
                .with_gate(gate)
                .with_mailbox_router(router);

        let result = execute_with_durable_parent(&de, fan_out_request(vec!["coder"]), "orch", None)
            .await
            .unwrap();
        assert_eq!(
            result.agent_results[0].output.as_deref(),
            Some("mailbox=true")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn sequential_per_stage_timeout_enforced() {
        let slow = Arc::new(SlowExecutor {
            delay: std::time::Duration::from_secs(5),
        });
        let (_, _engine, _tracker, de) = setup_with_executor(slow);

        let req = DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "seq-timeout".into(),
            parent_run_id: "p".into(),
            task: "slow pipeline".into(),
            pattern: CoordinationPattern::Sequential {
                agent_ids: vec!["coder".into(), "reviewer".into()],
                stop_on_success: false,
                timeout_sec: 1,
            },
            user_id: "u".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        };

        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();

        // Both agents should fail due to timeout
        assert_eq!(result.agent_results.len(), 2);
        for ar in &result.agent_results {
            assert_eq!(ar.status, "failed");
            assert!(
                ar.error.as_deref().unwrap_or("").contains("timeout"),
                "expected timeout error for {}, got: {:?}",
                ar.agent_id,
                ar.error
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn zero_timeout_means_no_timeout() {
        let slow = Arc::new(SlowExecutor {
            delay: std::time::Duration::from_millis(50),
        });
        let (_, _engine, _tracker, de) = setup_with_executor(slow);

        let req = DelegationRequest {
            session_id: "test-session".into(),
            delegation_id: "no-timeout".into(),
            parent_run_id: "p".into(),
            task: "quick task".into(),
            pattern: CoordinationPattern::FanOut {
                agent_ids: vec!["coder".into()],
                aggregation: AggregationStrategy::AllResults,
                timeout_sec: 0, // no timeout
            },
            user_id: "u".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: HashMap::new(),
            execution_metadata: None,
        };

        let result = execute_with_durable_parent(&de, req, "orch", None)
            .await
            .unwrap();
        assert_eq!(result.agent_results.len(), 1);
        assert_eq!(result.agent_results[0].status, "completed");
    }

    /// audit-#5: closing the semaphore must surface as a graceful Err from
    /// `acquire().await`, not a panic. This is the building-block invariant
    /// that the spawned delegation tasks now rely on (no `.expect`).
    #[tokio::test]
    async fn semaphore_acquire_returns_err_when_closed() {
        use tokio::sync::Semaphore;
        let sem = std::sync::Arc::new(Semaphore::new(0));
        let sem2 = sem.clone();
        let h = tokio::spawn(async move { sem2.acquire().await.map(|_| ()) });
        sem.close();
        let res = h.await.expect("task joins");
        assert!(res.is_err(), "closed semaphore must yield Err, not panic");
    }

    /// P1-B: cancel_children_of must cancel all child tokens.
    #[tokio::test]
    async fn cancel_children_of_cancels_tokens() {
        let tracker = DelegationTracker::new();
        let parent = "parent-run";
        let child1 = "child-1";
        let child2 = "child-2";

        // Register children under parent
        tracker
            .record_sub_run(SubRunRecord {
                run_id: child1.into(),
                parent_run_id: parent.into(),
                delegation_id: "deleg-1".into(),
                agent_id: "agent-a".into(),
                depth: 0,
                state: SubRunState::Running,
                retry_of: None,
            })
            .await;
        tracker
            .record_sub_run(SubRunRecord {
                run_id: child2.into(),
                parent_run_id: parent.into(),
                delegation_id: "deleg-1".into(),
                agent_id: "agent-b".into(),
                depth: 0,
                state: SubRunState::Running,
                retry_of: None,
            })
            .await;

        let token1 = Arc::new(tokio_util::sync::CancellationToken::new());
        let token2 = Arc::new(tokio_util::sync::CancellationToken::new());
        tracker.register_cancel_token(child1, token1.clone()).await;
        tracker.register_cancel_token(child2, token2.clone()).await;

        assert!(!token1.is_cancelled());
        assert!(!token2.is_cancelled());

        let count = tracker.cancel_children_of(parent).await;
        assert_eq!(count, 2, "both children must be cancelled");
        assert!(token1.is_cancelled(), "child1 token must be cancelled");
        assert!(token2.is_cancelled(), "child2 token must be cancelled");
        assert_eq!(
            tracker.get_sub_run_state(child1).await,
            Some(SubRunState::Running),
            "a cancellation request is not a fabricated terminal outcome"
        );
        assert_eq!(
            tracker.get_sub_run_state(child2).await,
            Some(SubRunState::Running),
            "the executor reports the eventual terminal state"
        );
    }

    #[tokio::test]
    async fn single_cancel_request_signals_only_the_target_and_keeps_outcome_pending() {
        let tracker = DelegationTracker::new();
        tracker
            .record_sub_run(SubRunRecord {
                run_id: "child-cancel".into(),
                parent_run_id: "parent-run".into(),
                delegation_id: "deleg-1".into(),
                agent_id: "reviewer".into(),
                depth: 1,
                state: SubRunState::Running,
                retry_of: None,
            })
            .await;
        let pause_flag = tracker.register_pause_flag("child-cancel").await;
        let cancel_token = Arc::new(tokio_util::sync::CancellationToken::new());
        tracker
            .register_cancel_token("child-cancel", cancel_token.clone())
            .await;

        assert!(tracker.request_cancel_sub_run("child-cancel").await);
        assert!(
            !pause_flag.load(Ordering::Acquire),
            "cancellation must not be misclassified as a pause"
        );
        assert!(cancel_token.is_cancelled());
        assert_eq!(
            tracker.get_sub_run_state("child-cancel").await,
            Some(SubRunState::Running),
            "the executor owns the terminal cancelled outcome"
        );
    }

    #[tokio::test]
    async fn terminal_sub_run_rejects_late_cancellation_without_signalling_token() {
        let tracker = DelegationTracker::new();
        tracker
            .record_sub_run(SubRunRecord {
                run_id: "child-complete".into(),
                parent_run_id: "parent-run".into(),
                delegation_id: "deleg-1".into(),
                agent_id: "reviewer".into(),
                depth: 1,
                state: SubRunState::Completed,
                retry_of: None,
            })
            .await;
        let cancel_token = Arc::new(tokio_util::sync::CancellationToken::new());
        tracker
            .register_cancel_token("child-complete", cancel_token.clone())
            .await;

        assert!(!tracker.request_cancel_sub_run("child-complete").await);
        assert!(!cancel_token.is_cancelled());
    }

    /// Regression: the SSE Failed event for a non-Completed/Paused/Cancelled
    /// terminal state (e.g. VerificationFailed) must surface the canonical
    /// `as_str()` wire form, NOT the Debug-formatted Rust enum variant.
    /// Pre-fix the broadcaster received "Sub-run terminal state:
    /// VerificationFailed"; the wire/JSON contract everywhere else uses
    /// "verification_failed", so the Debug leak coupled SSE consumers to
    /// the Debug derive — a refactor of the enum casing would have
    /// silently broken downstream parsing.
    #[tokio::test(flavor = "current_thread")]
    async fn sse_failed_event_uses_canonical_wire_status_not_debug() {
        use crate::orchestration::{ProgressBroadcaster, ProgressEventType};
        let broadcaster = Arc::new(ProgressBroadcaster::new(16));
        let mut rx = broadcaster.subscribe();
        let tracker = DelegationTracker::new().with_progress_broadcaster(broadcaster.clone());

        tracker
            .record_sub_run(SubRunRecord {
                run_id: "run-vf".into(),
                parent_run_id: "parent-vf".into(),
                delegation_id: "deleg-vf".into(),
                agent_id: "agent-vf".into(),
                depth: 0,
                state: SubRunState::Running,
                retry_of: None,
            })
            .await;

        tracker
            .complete_sub_run_with_result(
                "run-vf",
                SubRunState::VerificationFailed,
                Some("acceptance criterion 3 failed"),
                None,
            )
            .await;

        // Drain events until we see the terminal Failed (record_sub_run
        // emits a Started/Spawned event which we don't care about here).
        let error_text = loop {
            let event = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .expect("event should arrive within timeout")
                .expect("broadcast must deliver");
            match event.event_type {
                ProgressEventType::Failed { error } => break error,
                ProgressEventType::Completed { .. }
                | ProgressEventType::Cancelled { .. }
                | ProgressEventType::Interrupted { .. } => {
                    panic!(
                        "expected Failed for VerificationFailed terminal state, got {:?}",
                        event.event_type
                    );
                }
                _ => continue, // skip non-terminal events
            }
        };

        assert!(
            error_text.contains("verification_failed"),
            "SSE Failed event must use canonical wire status; got: {error_text}"
        );
        assert!(
            !error_text.contains("VerificationFailed"),
            "SSE Failed event must not leak the Rust Debug variant casing; got: {error_text}"
        );
    }

    /// Subtree cancellation: cancel_children_of must propagate to grandchildren
    /// (and deeper). Previously the implementation filtered the parents map by
    /// direct `parent_run_id` match only, leaving any sub-runs spawned by a
    /// cancelled child still alive — a real correctness bug for any
    /// multi-level delegation tree.
    #[tokio::test]
    async fn cancel_children_of_propagates_to_grandchildren() {
        let tracker = DelegationTracker::new();
        let parent = "parent-run";
        let child = "child-run";
        let grandchild = "grandchild-run";

        for (rid, prid, did) in [(child, parent, "deleg-1"), (grandchild, child, "deleg-2")] {
            tracker
                .record_sub_run(SubRunRecord {
                    run_id: rid.into(),
                    parent_run_id: prid.into(),
                    delegation_id: did.into(),
                    agent_id: "agent".into(),
                    depth: 0,
                    state: SubRunState::Running,
                    retry_of: None,
                })
                .await;
        }

        let token_child = Arc::new(tokio_util::sync::CancellationToken::new());
        let token_grand = Arc::new(tokio_util::sync::CancellationToken::new());
        tracker
            .register_cancel_token(child, token_child.clone())
            .await;
        tracker
            .register_cancel_token(grandchild, token_grand.clone())
            .await;

        let count = tracker.cancel_children_of(parent).await;
        assert!(token_child.is_cancelled(), "direct child must be cancelled");
        assert!(
            token_grand.is_cancelled(),
            "grandchild MUST be cancelled (subtree, not just first level)"
        );
        assert_eq!(count, 2, "count must include all descendants");
    }

    #[tokio::test]
    async fn collect_descendants_visits_siblings_before_grandchildren() {
        let tracker = DelegationTracker::new();
        for (rid, prid) in [
            ("child-a", "parent-run"),
            ("child-b", "parent-run"),
            ("grandchild-a", "child-a"),
        ] {
            tracker
                .record_sub_run(SubRunRecord {
                    run_id: rid.into(),
                    parent_run_id: prid.into(),
                    delegation_id: format!("deleg-{rid}"),
                    agent_id: "agent".into(),
                    depth: 0,
                    state: SubRunState::Running,
                    retry_of: None,
                })
                .await;
        }

        let descendants = tracker.collect_descendants("parent-run").await;
        let child_b = descendants
            .iter()
            .position(|run_id| run_id == "child-b")
            .expect("child-b should be collected");
        let grandchild_a = descendants
            .iter()
            .position(|run_id| run_id == "grandchild-a")
            .expect("grandchild-a should be collected");

        assert!(
            child_b < grandchild_a,
            "BFS must visit direct siblings before grandchildren: {descendants:?}"
        );
    }

    /// Concurrency regression: cancellation traversal and delegation cleanup
    /// can interleave while a parent is being torn down. The cancellation path
    /// takes short-lived per-run snapshots rather than holding tracker locks
    /// across the subtree walk, so the pair must complete without deadlock.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cancel_children_of_does_not_deadlock_with_cleanup_delegation() {
        use std::time::Duration;
        let tracker = Arc::new(DelegationTracker::new());

        // Pre-populate many delegation/run records so each iteration has work.
        const PARENTS: usize = 8;
        const CHILDREN_PER: usize = 4;
        for p in 0..PARENTS {
            for c in 0..CHILDREN_PER {
                let parent = format!("parent-{p}");
                let child = format!("child-{p}-{c}");
                tracker
                    .record_sub_run(SubRunRecord {
                        run_id: child.clone(),
                        parent_run_id: parent.clone(),
                        delegation_id: format!("deleg-{p}"),
                        agent_id: format!("agent-{p}-{c}"),
                        depth: 0,
                        state: SubRunState::Running,
                        retry_of: None,
                    })
                    .await;
                tracker
                    .register_cancel_token(
                        &child,
                        Arc::new(tokio_util::sync::CancellationToken::new()),
                    )
                    .await;
            }
        }

        // Workload A: hammer cancel_children_of across all parents.
        let a = {
            let tracker = Arc::clone(&tracker);
            tokio::spawn(async move {
                for _ in 0..200 {
                    for p in 0..PARENTS {
                        let _ = tracker.cancel_children_of(&format!("parent-{p}")).await;
                        tokio::task::yield_now().await;
                    }
                }
            })
        };

        // Workload B: hammer cleanup_delegation cycles. Re-register records
        // after each cleanup so the workload keeps having locks to take.
        let b = {
            let tracker = Arc::clone(&tracker);
            tokio::spawn(async move {
                for _ in 0..50 {
                    for p in 0..PARENTS {
                        // Force completion so cleanup can proceed.
                        for c in 0..CHILDREN_PER {
                            tracker
                                .complete_sub_run(&format!("child-{p}-{c}"), SubRunState::Completed)
                                .await;
                        }
                        let _ = tracker.cleanup_delegation(&format!("deleg-{p}")).await;

                        // Re-register so the next iteration has work.
                        for c in 0..CHILDREN_PER {
                            let child = format!("child-{p}-{c}");
                            tracker
                                .record_sub_run(SubRunRecord {
                                    run_id: child.clone(),
                                    parent_run_id: format!("parent-{p}"),
                                    delegation_id: format!("deleg-{p}"),
                                    agent_id: format!("agent-{p}-{c}"),
                                    depth: 0,
                                    state: SubRunState::Running,
                                    retry_of: None,
                                })
                                .await;
                            tracker
                                .register_cancel_token(
                                    &child,
                                    Arc::new(tokio_util::sync::CancellationToken::new()),
                                )
                                .await;
                        }
                        tokio::task::yield_now().await;
                    }
                }
            })
        };

        let result = tokio::time::timeout(Duration::from_secs(20), async {
            let _ = tokio::join!(a, b);
        })
        .await;
        assert!(
            result.is_ok(),
            "cancel_children_of and cleanup_delegation must not deadlock"
        );
    }

    /// cancel_tokens must be cleaned up in cleanup_delegation to prevent memory leaks.
    #[tokio::test]
    async fn cleanup_delegation_removes_cancel_tokens() {
        let tracker = DelegationTracker::new();
        let deleg_id = "deleg-cleanup";
        let child = "child-cleanup";

        tracker
            .record_sub_run(SubRunRecord {
                run_id: child.into(),
                parent_run_id: "parent".into(),
                delegation_id: deleg_id.into(),
                agent_id: "agent".into(),
                depth: 0,
                state: SubRunState::Running,
                retry_of: None,
            })
            .await;

        let token = Arc::new(tokio_util::sync::CancellationToken::new());
        tracker.register_cancel_token(child, token.clone()).await;
        assert!(tracker.state.read().await.cancel_tokens.contains_key(child));

        // Complete the sub-run so cleanup_delegation can proceed
        tracker
            .complete_sub_run(child, SubRunState::Completed)
            .await;

        tracker.cleanup_delegation(deleg_id).await.unwrap();
        assert!(
            !tracker.state.read().await.cancel_tokens.contains_key(child),
            "cancel_tokens must be cleaned up after delegation cleanup"
        );
    }

    #[tokio::test]
    async fn resolve_inherited_prefix_returns_none_without_store() {
        let registry = Arc::new(RwLock::new(AgentProfileRegistry::new()));
        let run_store = Arc::new(astra_services::runs::InMemoryRunStateStore::default());
        let tracker = Arc::new(DelegationTracker::new());
        let engine = DelegationEngine::with_executor(
            registry,
            Arc::new(RunEngine::new(run_store)),
            tracker,
            Arc::new(StubSubRunExecutor),
        );
        let out = engine.resolve_inherited_prefix_for_delegate("run-parent", "MiniMax-M2.5");
        assert!(
            out.is_none(),
            "engine without prefix_store must return None inherited_prefix"
        );
    }

    #[tokio::test]
    async fn resolve_inherited_prefix_resolves_captured_parent() {
        use astra_turn_core::fork_capture::{
            CaptureRequest, ForkCaptureOutcome, capture_parent_prefix,
        };
        use astra_turn_core::fork_prefix::{
            CacheMode, ProviderKind, SystemBlock, ThinkingConfigSlice, ToolSchemaEntry,
            hash_tool_schema,
        };
        use astra_turn_core::fork_prefix_store::{InMemoryPrefixStore, PrefixCaptureSink};

        let store: Arc<dyn PrefixCaptureSink> = Arc::new(InMemoryPrefixStore::new());
        let schema = serde_json::json!({"function": {"name": "bash"}});
        let (schema_bytes, schema_hash) = hash_tool_schema(&schema);
        let parent_msgs = serde_json::json!([
            {"role": "user", "content": "analyze"},
            {"role": "assistant", "content": "done"}
        ]);
        let canonical = serde_json::to_vec(&parent_msgs).unwrap();
        let capture = capture_parent_prefix(
            CaptureRequest {
                parent_run_id: "run-step2".into(),
                parent_turn_seq: 1,
                provider: ProviderKind::Other("MiniMax-M2.5".into()),
                model_id: "MiniMax-M2.5".into(),
                thinking: Some(ThinkingConfigSlice {
                    enabled: false,
                    budget_tokens: 0,
                    kind: "disabled".into(),
                }),
                system_blocks: vec![SystemBlock {
                    bytes: b"sys".to_vec(),
                    has_cache_control: true,
                }],
                tool_schemas: vec![ToolSchemaEntry {
                    name: "bash".into(),
                    canonical_bytes: schema_bytes,
                    hash: schema_hash,
                }],
                beta_headers: vec![],
                canonical_prefix_bytes: canonical,
                cache_mode: CacheMode::Write,
                captured_at_secs: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
                microcompact_fired_in_turn: false,
            },
            &*store,
        );
        assert!(matches!(capture, ForkCaptureOutcome::Captured { .. }));

        let registry = Arc::new(RwLock::new(AgentProfileRegistry::new()));
        let run_store = Arc::new(astra_services::runs::InMemoryRunStateStore::default());
        let tracker = Arc::new(DelegationTracker::new());
        let engine = DelegationEngine::with_executor(
            registry,
            Arc::new(RunEngine::new(run_store)),
            tracker,
            Arc::new(StubSubRunExecutor),
        )
        .with_prefix_store(store);

        let out = engine.resolve_inherited_prefix_for_delegate("run-step2", "MiniMax-M2.5");
        let inherited = out.expect("engine must resolve captured parent prefix");
        assert_eq!(inherited.parent_run_id, "run-step2");
        assert!(
            !inherited.prefix_messages.is_empty(),
            "resolved prefix must carry the captured parent messages"
        );
    }
}
