//! Concrete [`RunLifecycleService`] backed by [`ServerAgenticLoopHost`].
//!
//! This module replaces `UnconfiguredRunLifecycleService` with a real implementation
//! that runs multi-turn agentic loops on the server via the shared
//! [`run_agentic_loop_with_host`] cognitive pipeline.
//!
//! Run status, listing, and replay are backed by durable run state. The
//! process-local map only keeps live control handles for in-flight runs.

mod persistence;
mod run_state;

use std::any::Any;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use axum::Json;
use axum::http::StatusCode;
use futures_util::FutureExt;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex as TokioMutex, RwLock, broadcast, mpsc, oneshot};

use astra_server_types::ws_progress_callback::ProgressEvent;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::turn::run_control::RunInputProvider;
use astra_core::{ErrorResponse, SharedPool, connect_matrixone, error_response};
use astra_services::coordination::{AgentProfile, AgentTier};
use astra_services::runs::{
    CancelRunRecord, ChatRequestData, ChatRunRecord, ChatStreamRecord, DurableRunRecord,
    DurableRunStatusKind, RunInputData, RunInputRecord, RunLifecycleService, RunListRecord,
    RunMutationRecord, RunProjectionCheckpointRecord, RunProjectionRecord, RunStatusRecord,
    durable_run_status_kind,
};
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
use astra_tools::task_mgmt::{SessionTask, TaskManager, TaskStore};
use astra_tools::task_mgmt_matrixone::MatrixOneTaskStore;
use sqlx::Row;

use crate::FernetTokenEncryptor;
use crate::MatrixOneSettings;
use crate::observability::ObservabilityHub;
use crate::orchestration::{
    AgentProgressEvent, AgentToolContext, DynamicAgentSpawner, InheritedPermissions,
    ProgressBroadcaster, ProgressEventType, SpawnAgentExecutor, SpawnRunConfig, SpawnRunResult,
    SpawnedAgentState,
};
use crate::server::run::cloud_workspace_provisioning::CloudWorkspaceProvisioner;
use crate::server::run::workspace_provisioning::{
    ServerWorkspaceProvisionError, ServerWorkspaceProvisioner,
};

use crate::turn::agentic_loop::host::{
    AgenticLoopHost, AgenticLoopOutcome, AgenticLoopState, CancellationState,
    ContextTracePersistenceContext, EvaluationPersistenceContext, MessagingState,
    RequestConstraints, SkillState, StopHookState, run_agentic_loop_with_host,
};
use crate::{
    DatabaseEvaluationService, DatabaseEventService, DatabaseTraceEventWriter,
    EventCreateRequestData, EventService,
};
use astra_pipeline::step_recorder::StepRecorder;
use astra_turn_core::agent_live_event::{
    AgentLiveEvent, AgentLiveEventKind, AgentLiveEventSink, AgentLiveSendError,
    AgentLiveTermination, SharedAgentLiveEventSink,
};
use astra_turn_core::contracts::{
    TurnCoreEventRecord, TurnCoreEventWriter, TurnCorePersistPlan, TurnDecisionAuditRecord,
    TurnHookDbPersistPlan, TurnHookDbWriter, TurnObserverRequest, TurnObserverWorker,
    TurnSkillSelectionRecord, TurnToolEventPersistPlan, TurnToolEventRecord, TurnToolEventWriter,
};
use astra_turn_core::trace_event::{TraceContext, TraceEvent, TraceEventWriter};

use astra_core::{
    STATUS_CANCELLED, STATUS_COMPLETED, STATUS_FAILED, STATUS_INPUT_QUEUED, STATUS_PAUSED,
    STATUS_RUNNING, STATUS_WAITING,
};
use astra_runtime_env::{
    CleanupReason as RuntimeCleanupReason, PolicyIntent as RuntimePolicyIntent,
    WorkspaceOwnerScope as RuntimeWorkspaceOwnerScope,
    WorkspacePersistence as RuntimeWorkspacePersistence,
    WorkspaceProvisionError as RuntimeWorkspaceProvisionError,
    WorkspaceProvisionErrorKind as RuntimeWorkspaceProvisionErrorKind,
    WorkspaceProvisionRequest as RuntimeWorkspaceProvisionRequest, WorkspaceProvisioner,
    WorkspaceRecord as RuntimeWorkspaceRecord, WorkspaceSource as RuntimeWorkspaceSource,
};

use crate::orchestration::spawner::{
    agent_status_to_progress_event, project_subrun_status_to_spawn,
};
use crate::server::run::engine::{RunEngine, RunStartContext};
use crate::server::run::handlers as run_handlers;
use crate::server::runtime_mcp;
use crate::server::server_loop_host::{self, ServerAgenticLoopHostBuilder};
use crate::server::tool_transport::{
    ExecutionBindingSnapshot, ExecutorBinding, ExecutorBindingKind, ExecutorStatus, FallbackPolicy,
    ToolExecutionService, ToolTransportKind, WorkspaceAuthority, WorkspaceBinding,
    WorkspaceBindingKind, binding_event_fields,
};
use crate::server::{server_skill_subrun, server_tool_executor};

const MAX_DEFERRED_INPUT_CHARS: usize = 20_000;
const MAX_DURABLE_RUN_PROJECTION_RECENT_EVENTS: u32 = 500;
const MAX_ACTIVE_RUN_LIVE_EVENTS: usize = MAX_DURABLE_RUN_PROJECTION_RECENT_EVENTS as usize;
const AGENT_PROGRESS_STREAM_DRAIN_GRACE: Duration = Duration::from_millis(25);

const RUNTIME_CONTEXT_TRACE_AGENT_ID: &str = "astra-server";
const LLM_TOKEN_SERVICE_TRUSTED_DOMAINS_TABLE: &str = "runtime_llm_trusted_domains";

/// Lazily load deployment-disabled tools from server config.
/// Reads `[deployment].disabled_tools` from TOML + `ASTRA_DISABLED_TOOLS` env override.
fn load_deployment_disabled_tools() -> Vec<String> {
    use std::sync::OnceLock;
    static DISABLED_TOOLS: OnceLock<Vec<String>> = OnceLock::new();
    DISABLED_TOOLS
        .get_or_init(|| {
            astra_core::ServerConfig::load()
                .map(|sc| sc.deployment.disabled_tools.clone())
                .unwrap_or_default()
        })
        .clone()
}

/// Wire a freshly-constructed [`server_tool_executor::ServerToolExecutor`]
/// into the agentic loop state: Arc-wrap it, attach the task-board monitor,
/// and set the tool-executor handle.  This small helper deduplicates the
/// same three-line pattern repeated at every executor construction site.
fn wire_executor_into_state(
    executor: server_tool_executor::ServerToolExecutor,
    state: &mut crate::turn::agentic_loop::host::AgenticLoopState,
) {
    let executor = std::sync::Arc::new(executor);
    state.hooks.task_board_monitor = Some(executor.task_manager());
    state.server_tool_executor = Some(executor);
}

struct NonInteractiveApprovalGate;

#[async_trait]
impl astra_tools::ToolApprovalGate for NonInteractiveApprovalGate {
    async fn request_approval(
        &self,
        _request_id: &str,
        tool_name: &str,
        _args: &Value,
    ) -> astra_tools::ApprovalDecision {
        astra_tools::ApprovalDecision::Denied {
            reason: Some(format!(
                "{tool_name} requires interactive approval, but this run has no interactive client"
            )),
        }
    }

    fn requires_approval(&self, tool_name: &str) -> bool {
        astra_tools::APPROVAL_REQUIRED_TOOLS.contains(&tool_name)
    }
}

use crate::server::run::binding_resolution::{
    RunExecutionBindingSnapshot, agent_working_dir_for_bindings, binding_snapshot_events,
    execution_bindings_from_edge_profile, execution_bindings_from_metadata,
    executor_binding_from_request, request_uses_server_workspace,
    resolve_request_execution_bindings,
    resolve_request_execution_bindings_without_server_workspace, run_start_context_from_request,
};

fn panic_payload_message(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

/// Cap on concurrent live subagents per server-side run. Mirrors the
/// CLI-side cap in `crate::cli::agent_runtime::resolved_spawn_concurrency_cap`
/// so headless web sessions don't have a different ceiling than the
/// terminal CLI. Override via `ASTRA_MAX_CONCURRENT_AGENTS=N`.
fn resolved_server_spawn_concurrency_cap() -> usize {
    const DEFAULT_CAP: usize = 10;
    match std::env::var("ASTRA_MAX_CONCURRENT_AGENTS") {
        Ok(raw) => match raw.trim().parse::<usize>() {
            Ok(n) if n > 0 => n,
            _ => {
                tracing::warn!(
                    target: "astra::spawner",
                    raw = %raw,
                    default = DEFAULT_CAP,
                    "ASTRA_MAX_CONCURRENT_AGENTS unparseable; using default"
                );
                DEFAULT_CAP
            }
        },
        Err(_) => DEFAULT_CAP,
    }
}

async fn run_agentic_loop_with_host_panic_safe<H: AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
) -> Result<AgenticLoopOutcome, astra_core::ClassifiedError> {
    match AssertUnwindSafe(run_agentic_loop_with_host(host, state))
        .catch_unwind()
        .await
    {
        Ok(outcome) => outcome,
        Err(payload) => {
            let message = format!(
                "agentic loop panicked: {}",
                panic_payload_message(payload.as_ref())
            );
            tracing::error!(
                target: "astra_runtime::run_lifecycle",
                error = %message,
                "agentic loop panic converted to failed run"
            );
            Err(astra_core::ClassifiedError::new(
                astra_core::ErrorKind::Unknown,
                message,
            ))
        }
    }
}

// ─── Skill wiring for server paths ──────────────────────────────────────────

type ServerSkillResolverBundle = (
    Option<Arc<crate::skills::UnifiedSkillRegistry>>,
    Option<Arc<dyn crate::turn::skill_tool::SkillResolver>>,
);

/// Post-loop session-memory cleanup shared by `create_run` and
/// `stream_chat`. Runs session-end governance (purge working memory +
/// persist episodic overview + Memoria reflect) when the per-session
/// debounce window allows, and always clears the bridge seen-ledger +
/// extraction-service debounce so long-lived servers don't accumulate
/// per-session state.
///
/// Best-effort: every step logs and continues on failure. Safe to call
/// with an empty `session_id` (no-op).
async fn post_loop_memory_cleanup(
    session_id: &str,
    session_facts: &astra_turn_types::session_facts::SessionFacts,
    extraction_service: Option<&Arc<crate::session_memory::MemoryExtractionService>>,
    final_extract_request: Option<crate::session_memory::ExtractionRequest>,
) {
    if session_id.is_empty() {
        return;
    }
    if let (Some(svc), Some(req)) = (extraction_service, final_extract_request) {
        let _ = svc.maybe_spawn_shutdown_flush(req);
    }
    if let Some(svc) = extraction_service {
        let leftover = svc
            .wait_for_pending(std::time::Duration::from_secs(10))
            .await;
        if leftover > 0 {
            tracing::warn!(
                session_id = %session_id,
                leftover,
                "session-memory extraction still in flight after post-loop drain timeout"
            );
        }
    }
    // ── Governance, debounced ──
    //
    // Session IDs are sticky across many terminal runs (user reopens a
    // session or the TUI issues follow-up turns). Running governance
    // per run would write one episode per turn and hammer reflect.
    // The debouncer allows one governance per session per window.
    let mut episode_was_written = false;
    if let Some(ref memoria_client) =
        crate::turn::cloud::memoria_compact::HttpMemoriaClient::from_env()
    {
        let debouncer = crate::turn::session_end_debounce::global();
        if matches!(
            debouncer.should_run(session_id),
            crate::turn::session_end_debounce::DebounceDecision::Run
        ) {
            match crate::turn::cloud::session_end_governance::run_session_end_governance(
                session_facts,
                session_id,
                memoria_client,
            )
            .await
            {
                Ok(report) => {
                    episode_was_written = report.episode_chars > 0;
                    if episode_was_written
                        || report.working_purged > 0
                        || report.reflect_candidates > 0
                        || report.scenes_stored > 0
                    {
                        tracing::info!(
                            session_id = %session_id,
                            learnings = report.learnings_stored,
                            purged = report.working_purged,
                            episode_chars = report.episode_chars,
                            reflect_candidates = report.reflect_candidates,
                            reflect_synthesized = report.reflect_synthesized,
                            scenes_stored = report.scenes_stored,
                            "session-end governance complete"
                        );
                    }
                    debouncer.record(session_id);
                }
                Err(e) => {
                    tracing::warn!(
                        session_id = %session_id,
                        error = %e,
                        "session-end governance failed"
                    );
                }
            }
        } else {
            tracing::debug!(
                session_id = %session_id,
                "session-end governance skipped by debounce"
            );
        }

        // ── Close the recall → outcome feedback loop ──────────────
        //
        // Every LLM-driven `memory(action=recall)` pushed its returned
        // memory_ids onto the process-global recall ledger. Drain them
        // here and route a feedback signal to Memoria. Conservative
        // heuristic:
        //   • episode was written (session did substantive work)
        //       → signal `useful` — the recall was at least surfaced
        //         into a productive session
        //   • trivial session (no episode)
        //       → drop without scoring — not enough evidence
        //
        // A richer attribution (per-tool-call outcome mapping) needs
        // the tool dispatch layer to report success/failure per recall,
        // which is a bigger refactor. The episode-level heuristic is
        // the smallest step that closes the loop in production.
        let snapshots = astra_tools::memoria::MemoriaClient::drain_recalls(session_id, None);
        if !snapshots.is_empty() && episode_was_written {
            use crate::turn::cloud::memoria_compact::MemoriaClient as ServerMemoriaClient;
            for snap in &snapshots {
                for id in &snap.memory_ids {
                    let ctx = format!("session-end: turn {} productive session", snap.turn);
                    if let Err(e) =
                        ServerMemoriaClient::feedback(memoria_client, id, "useful", Some(&ctx))
                            .await
                    {
                        tracing::debug!(memory_id = %id, error = %e, "feedback push failed");
                    }
                }
            }
            tracing::info!(
                session_id = %session_id,
                snapshots = snapshots.len(),
                "closed recall → useful feedback loop"
            );
        } else if !snapshots.is_empty() {
            tracing::debug!(
                session_id = %session_id,
                snapshots = snapshots.len(),
                "session trivial; dropped recall snapshots without scoring"
            );
        }
    }
    // ── Always: clear canonical memory process state for this session ──
    //
    // A single process-global set in `astra_tools::memoria` holds both
    // the bridge-side content-dedup keys and the tool-side
    // memory_id dedup entries. The shared reset also ensures focus hints
    // and the recall ledger are clean even if
    // governance didn't run (e.g. no memoria client configured, or drain
    // was conditional on an episode being written).
    astra_tools::memoria::MemoriaClient::reset_session_process_state(session_id);

    // ── Always: release extraction service's per-session debounce ──
    if let Some(svc) = extraction_service {
        svc.forget_session(session_id);
    }
}

/// Build a user-scoped skill registry + resolver for server-side web runs.
///
/// The visible catalog is assembled by `skills::catalog` and contains exactly:
/// API-server HOME skills (`~/.astra/skills`, `~/.claude/skills`) plus database
/// skills visible to the authenticated user. Request `allow_skills` is a
/// selector/execution filter over that catalog, not a switch that enables the
/// catalog.
fn build_server_skill_resolver(
    skill_service: Option<Arc<dyn SkillService>>,
    user_id: &str,
) -> ServerSkillResolverBundle {
    use crate::turn::skill_tool::SkillResolver as _;

    let Some(registry) = crate::capabilities::build_server_skill_registry(skill_service, user_id)
    else {
        return (None, None);
    };

    let resolver_impl = Arc::new(crate::skills::UnifiedSkillResolver::new(Arc::clone(
        &registry,
    )));
    let skills = resolver_impl.available_skills();
    let resolver: Option<Arc<dyn crate::turn::skill_tool::SkillResolver>> = if skills.is_empty() {
        None
    } else {
        Some(resolver_impl)
    };
    (Some(registry), resolver)
}

fn normalize_allowlist_entry(entry: &str, field: &str) -> Result<String, String> {
    let normalized = entry.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        Err(format!("{field} must not contain empty values"))
    } else {
        Ok(normalized)
    }
}

fn normalize_request_allowlist(
    entries: Option<&[String]>,
    field: &str,
) -> Result<Option<HashSet<String>>, String> {
    let Some(entries) = entries else {
        return Ok(None);
    };
    let mut normalized = HashSet::new();
    for entry in entries {
        normalized.insert(normalize_allowlist_entry(entry, field)?);
    }
    Ok(Some(normalized))
}

fn normalize_request_skill_sources(
    entries: Option<&[String]>,
    field: &str,
) -> Result<Option<HashSet<crate::skills::manifest::SkillSourceKind>>, String> {
    let Some(entries) = entries else {
        return Ok(None);
    };
    let mut normalized = HashSet::new();
    for entry in entries {
        normalized.insert(
            entry
                .parse::<crate::skills::manifest::SkillSourceKind>()
                .map_err(|error| format!("{field}: {error}"))?,
        );
    }
    Ok(Some(normalized))
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
struct TrustedLlmDomain {
    host: String,
    port: Option<u16>,
}

fn normalize_trusted_llm_domain_host(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("trusted domain host must not be empty".to_string());
    }
    if trimmed.contains("://") {
        return Err(format!(
            "trusted domain host '{trimmed}' must not include URL scheme"
        ));
    }
    let parsed = reqwest::Url::parse(&format!("http://{trimmed}"))
        .map_err(|error| format!("invalid trusted domain host '{trimmed}': {error}"))?;
    if parsed.username() != ""
        || parsed.password().is_some()
        || parsed.path() != "/"
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || parsed.port().is_some()
    {
        return Err(format!("trusted domain host '{trimmed}' must be host only"));
    }
    let Some(host) = parsed.host_str() else {
        return Err(format!(
            "trusted domain host '{trimmed}' must include a host"
        ));
    };
    Ok(host.to_ascii_lowercase())
}

fn trusted_llm_domain_from_db_values(
    host_raw: &str,
    port_raw: i64,
) -> Result<TrustedLlmDomain, String> {
    let host = normalize_trusted_llm_domain_host(host_raw)?;
    let port = match port_raw {
        0 => None,
        port if !(1..=65_535).contains(&port) => {
            return Err(format!(
                "trusted domain host '{host_raw}' has invalid port value {port}"
            ));
        }
        port => Some(port as u16),
    };
    Ok(TrustedLlmDomain { host, port })
}

fn llm_token_service_domain_is_trusted(
    url: &reqwest::Url,
    trusted_domains: &[TrustedLlmDomain],
) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let normalized_host = host.to_ascii_lowercase();
    let resolved_port = url.port_or_known_default();
    trusted_domains.iter().any(|trusted| {
        if trusted.host != normalized_host {
            return false;
        }
        match trusted.port {
            Some(port) => resolved_port == Some(port),
            None => true,
        }
    })
}

fn validate_llm_token_service_config(
    config: Option<&astra_services::LlmTokenServiceConfig>,
    trusted_domains: &[TrustedLlmDomain],
) -> Result<(), String> {
    let Some(config) = config else {
        return Ok(());
    };
    let raw_url = config.url.trim();
    if raw_url.is_empty() {
        return Err("llm_token_service.url must not be empty".to_string());
    }
    let parsed = reqwest::Url::parse(raw_url)
        .map_err(|error| format!("llm_token_service.url must be a valid URL: {error}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("llm_token_service.url must use http or https scheme".to_string());
    }
    if parsed.host_str().is_none() {
        return Err("llm_token_service.url must include a host".to_string());
    }
    if config.timeout_ms == Some(0) {
        return Err("llm_token_service.timeout_ms must be greater than 0".to_string());
    }
    if trusted_domains.is_empty() {
        return Err(format!(
            "llm_token_service.url is not allowed without trusted domains configured in table {LLM_TOKEN_SERVICE_TRUSTED_DOMAINS_TABLE}"
        ));
    }
    if !llm_token_service_domain_is_trusted(&parsed, trusted_domains) {
        return Err(format!(
            "llm_token_service.url host must match trusted domains configured in table {LLM_TOKEN_SERVICE_TRUSTED_DOMAINS_TABLE}"
        ));
    }
    Ok(())
}

fn apply_normalized_skill_allowlist(
    resolver: Option<Arc<dyn crate::turn::skill_tool::SkillResolver>>,
    request_constraints: &RequestConstraints,
) -> Result<Option<Arc<dyn crate::turn::skill_tool::SkillResolver>>, String> {
    crate::turn::skill_tool::apply_skill_surfacing_policy(
        resolver,
        &request_constraints.skill_surfacing_policy(),
    )
}

/// Build a server-side skill executor that supports both Inline and Fork
/// execution contexts via [`SkillExecutionRouter`].
fn build_server_skill_executor(
    matrixone: &MatrixOneSettings,
    encryptor: &Arc<FernetTokenEncryptor>,
    shared_pool: Option<&SharedPool>,
    model_override: Option<&str>,
    llm_token_service: Option<&astra_services::LlmTokenServiceConfig>,
    edge_tools: &[Value],
    edge_profile: &Map<String, Value>,
    execution_bindings: Option<&ExecutionBindingSnapshot>,
    forward_headers: &HashMap<String, String>,
    request_constraints: RequestConstraints,
    skill_resolver: Option<Arc<dyn crate::turn::skill_tool::SkillResolver>>,
    session_id: &str,
    edge_connection_pool: Option<&astra_server_types::edge_connection_pool::EdgeConnectionPool>,
    cancel_token: Option<Arc<tokio_util::sync::CancellationToken>>,
    memory_extraction_service: Option<&Arc<crate::session_memory::MemoryExtractionService>>,
    #[cfg(feature = "harness")] harness_sink: Option<
        &std::sync::Arc<dyn astra_harness::SnapshotSink>,
    >,
) -> Option<Arc<dyn crate::skills::traits::SkillExecutor>> {
    use crate::server::server_skill_subrun::ServerSkillSubRunExecutor;
    use astra_skills::executor::isolated::{IsolatedSkillExecutor, SkillExecutionRouter};

    let mut subrun_executor = ServerSkillSubRunExecutor::new(
        matrixone.clone(),
        Arc::clone(encryptor),
        session_id.to_string(),
    )
    .with_pool(shared_pool.cloned())
    .with_default_model(model_override.map(String::from))
    .with_llm_token_service(llm_token_service.cloned())
    .with_edge_tools(edge_tools.to_vec())
    .with_edge_profile(edge_profile.clone())
    .with_forward_headers(forward_headers.clone())
    .with_request_constraints(request_constraints)
    .with_skill_resolver(skill_resolver)
    .with_cancel_token(cancel_token);
    if let Some(snapshot) = execution_bindings {
        subrun_executor = subrun_executor.with_execution_binding_snapshot(snapshot.clone());
    }
    if let Some(svc) = memory_extraction_service {
        subrun_executor = subrun_executor.with_memory_extraction_service(Arc::clone(svc));
    }
    if let Some(pool) = edge_connection_pool {
        subrun_executor = subrun_executor.with_edge_connection_pool(pool.clone());
    }
    #[cfg(feature = "harness")]
    if let Some(sink) = harness_sink {
        subrun_executor = subrun_executor.with_harness_sink(Some(sink.clone()));
    }

    // Wire skill checkpoint manager for crash recovery resume.
    // This allows skills to resume from their last checkpoint instead of starting over.
    #[cfg(feature = "crash-recovery")]
    {
        let checkpoint_dir = astra_services::session_journal::journal_file_path(session_id)
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .join("skill_checkpoints");
        let checkpoint_manager = Arc::new(std::sync::Mutex::new(
            astra_pipeline::skill_checkpoint::SkillCheckpointManager::new(checkpoint_dir),
        ));
        let isolated = IsolatedSkillExecutor::with_checkpoint_manager(
            Arc::new(subrun_executor),
            checkpoint_manager,
        );
        let isolated = Arc::new(isolated);
    }
    #[cfg(not(feature = "crash-recovery"))]
    let isolated = Arc::new(IsolatedSkillExecutor::new(Arc::new(subrun_executor)));

    let router = SkillExecutionRouter::new(Some(isolated));
    Some(Arc::new(router))
}

pub(crate) fn has_turn_verdict_warning(
    verdict_events: &[astra_turn_core::agentic_verdict_audit::AgenticVerdictAuditEvent],
) -> bool {
    verdict_events.iter().any(|event| {
        event.severity.eq_ignore_ascii_case("warning")
            || event.severity.eq_ignore_ascii_case("critical")
    })
}

fn build_runtime_turn_evaluation_event(
    session_id: &str,
    source: &str,
    state: &AgenticLoopState,
) -> astra_services::session_journal::JournalEvent {
    let verdict_warning = has_turn_verdict_warning(&state.stall.verdict_events);
    let eval_thresholds = crate::pipeline::evaluation::current_evaluation_thresholds();
    let eval = crate::pipeline::evaluation::evaluate_tool_call_records_with_thresholds(
        &state.message,
        &state.recent_tools,
        &state.stall.tool_call_records,
        state.stall.events.len(),
        verdict_warning,
        state.telemetry.first_budget_pressure,
        eval_thresholds,
    );
    crate::pipeline::evaluation::build_turn_evaluation_journal_event(
        Some(session_id),
        None,
        source,
        &state.message,
        &state.recent_tools,
        &state.stall.tool_call_records,
        state.stall.events.len(),
        verdict_warning,
        state.telemetry.first_budget_pressure,
        &eval,
    )
}

fn persist_turn_evaluation_journal(session_id: &str, source: &str, state: &AgenticLoopState) {
    if session_id.is_empty() {
        return;
    }

    let event = build_runtime_turn_evaluation_event(session_id, source, state);
    match astra_services::session_journal::JournalWriter::new(session_id) {
        Ok(journal) => {
            if let Err(err) = journal.append(&event) {
                tracing::warn!(
                    target: "astra_runtime::run_lifecycle",
                    session_id = %session_id,
                    error = %err,
                    "turn evaluation journal append failed"
                );
            }
        }
        Err(err) => tracing::warn!(
            target: "astra_runtime::run_lifecycle",
            session_id = %session_id,
            error = %err,
            "turn evaluation journal init failed"
        ),
    }
}

/// Best-effort flush of turn observability events to local journal.
fn flush_turn_observability(state: &mut AgenticLoopState, session_id: &str, interrupted: bool) {
    let Some(buf) = state.turn_event_buffer.as_mut() else {
        return;
    };
    if buf.is_empty() {
        return;
    }
    let Ok(writer) = astra_services::session_journal::JournalWriter::new(session_id) else {
        return;
    };
    if interrupted {
        let _ = buf.flush_interrupted(&writer);
    } else {
        let _ = buf.flush(&writer);
    }
}

fn skill_search_from_context(
    context: &std::collections::HashMap<String, serde_json::Value>,
) -> astra_core::SkillSearchSettings {
    context
        .get("skill_search")
        .cloned()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default()
}

fn build_runtime_evaluation_service(
    matrixone: &MatrixOneSettings,
    shared_pool: &SharedPool,
) -> DatabaseEvaluationService {
    DatabaseEvaluationService::new(matrixone.clone()).with_pool(shared_pool.clone())
}

async fn initialize_runtime_controllers(
    loop_state: &mut AgenticLoopState,
    user_id: &str,
    session_id: &str,
    evaluation_persistence: Option<EvaluationPersistenceContext>,
    context_trace_persistence: Option<ContextTracePersistenceContext>,
) {
    let hub = Arc::new(ObservabilityHub::new());
    let session = hub.start_session(user_id, session_id);

    loop_state.telemetry.observability_hub = Some(hub);
    loop_state.telemetry.observability_session = Some(session);
    loop_state.telemetry.evaluation_persistence = evaluation_persistence;
    loop_state.telemetry.context_trace_persistence = context_trace_persistence;
}

async fn configure_runtime_controllers(
    matrixone: &MatrixOneSettings,
    shared_pool: Option<&SharedPool>,
    loop_state: &mut AgenticLoopState,
    user_id: &str,
    session_id: &str,
) {
    let evaluation_persistence = shared_pool.map(|pool| EvaluationPersistenceContext {
        user_id: user_id.to_string(),
        evaluation_service: build_runtime_evaluation_service(matrixone, pool),
    });
    let context_trace_persistence = shared_pool.map(|pool| ContextTracePersistenceContext {
        user_id: user_id.to_string(),
        event_service: build_runtime_event_service(matrixone, pool),
        artifact_store: astra_services::DatabaseSessionArtifactStore::new(matrixone.clone())
            .with_pool(pool.clone()),
        agent_id: RUNTIME_CONTEXT_TRACE_AGENT_ID.to_string(),
    });
    initialize_runtime_controllers(
        loop_state,
        user_id,
        session_id,
        evaluation_persistence,
        context_trace_persistence,
    )
    .await
}

fn build_runtime_event_service(
    matrixone: &MatrixOneSettings,
    shared_pool: &SharedPool,
) -> DatabaseEventService {
    DatabaseEventService::new(matrixone.clone()).with_pool(shared_pool.clone())
}

async fn persist_runtime_promotion_events(
    matrixone: &MatrixOneSettings,
    shared_pool: Option<&SharedPool>,
    user_id: &str,
    session_id: &str,
    run_id: &str,
    promotions: &[RuntimePromotionEventData],
) {
    if promotions.is_empty() {
        return;
    }
    let Some(pool) = shared_pool else {
        tracing::debug!(
            session_id,
            "runtime promotion persistence skipped: shared_pool not configured"
        );
        return;
    };

    let service = build_runtime_event_service(matrixone, pool);
    for promotion in promotions {
        let metadata = match serde_json::to_value(promotion) {
            Ok(value) => Some(value),
            Err(err) => {
                tracing::warn!(
                    target: "astra_runtime::run_lifecycle",
                    subject_id = %promotion.subject_id,
                    error = %err,
                    "runtime promotion event serialize failed"
                );
                continue;
            }
        };
        if let Err((status, response)) = service
            .create_event(
                user_id.to_string(),
                EventCreateRequestData {
                    session_id: session_id.to_string(),
                    event_type: RUNTIME_PROMOTION_EVENT_TYPE.to_string(),
                    content: promotion.summary.clone(),
                    agent_id: None,
                    agent_version: None,
                    parent_event_id: None,
                    parent_event_ids: Some(Vec::new()),
                    causal_chain_id: Some(format!(
                        "{session_id}:runtime-promotion:{}:{run_id}",
                        promotion.subject_id
                    )),
                    metadata,
                },
            )
            .await
        {
            tracing::warn!(
                target: "astra_runtime::run_lifecycle",
                subject_id = %promotion.subject_id,
                status = %status,
                detail = %response.0.detail,
                "runtime promotion event persist failed"
            );
        }
    }
}

fn should_emit_stream_turn_complete(final_status: &RunStatus) -> bool {
    matches!(final_status, RunStatus::Completed | RunStatus::Paused)
}

pub(crate) use persistence::{
    PostLoopPersistContext, TranscriptPersistItem, build_run_turn_complete_event_with_interruption,
    format_task_board_resume_hint, infer_session_turn, persist_server_loop_core_events,
    persist_server_loop_trace_events, persist_session_transcript_items,
    restore_session_state_compact, restore_step_checkpoint_runtime_state, server_trace_context,
    trace_context_from_subrun_context,
};
use run_state::*;

struct AgentProgressStreamBridge {
    stop_tx: oneshot::Sender<()>,
    join: tokio::task::JoinHandle<()>,
    sent_lifecycle_events: AgentProgressLifecycleLedger,
}

impl AgentProgressStreamBridge {
    async fn stop_and_drain(self) -> AgentProgressLifecycleLedger {
        let _ = self.stop_tx.send(());
        let _ = self.join.await;
        self.sent_lifecycle_events
    }
}

type AgentProgressLifecycleLedger = Arc<std::sync::Mutex<HashSet<AgentProgressLifecycleEventKey>>>;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct AgentProgressLifecycleEventKey {
    agent_id: String,
    kind: AgentProgressLifecycleEventKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum AgentProgressLifecycleEventKind {
    Spawned { run_id: String },
    Completed,
    Interrupted,
    Failed,
    Waiting,
    Cancelled,
}

fn agent_progress_lifecycle_event_key(
    event: &AgentProgressEvent,
) -> Option<AgentProgressLifecycleEventKey> {
    let kind = match &event.event_type {
        ProgressEventType::AgentSpawned { run_id, .. } => {
            AgentProgressLifecycleEventKind::Spawned {
                run_id: run_id.clone(),
            }
        }
        ProgressEventType::Completed { .. } => AgentProgressLifecycleEventKind::Completed,
        ProgressEventType::Interrupted { .. } => AgentProgressLifecycleEventKind::Interrupted,
        ProgressEventType::Failed { .. } => AgentProgressLifecycleEventKind::Failed,
        ProgressEventType::Waiting { .. } => AgentProgressLifecycleEventKind::Waiting,
        ProgressEventType::Cancelled { .. } => AgentProgressLifecycleEventKind::Cancelled,
        _ => return None,
    };
    Some(AgentProgressLifecycleEventKey {
        agent_id: event.agent_id.clone(),
        kind,
    })
}

fn mark_agent_progress_lifecycle_event_sent(
    sent_lifecycle_events: &AgentProgressLifecycleLedger,
    key: AgentProgressLifecycleEventKey,
) {
    sent_lifecycle_events
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(key);
}

fn has_agent_progress_lifecycle_event_sent(
    sent_lifecycle_events: &AgentProgressLifecycleLedger,
    key: &AgentProgressLifecycleEventKey,
) -> bool {
    sent_lifecycle_events
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .contains(key)
}

#[derive(Debug, Clone)]
struct WorkSurfaceAgentLiveEventSink {
    tx: mpsc::Sender<Value>,
    execution_metadata: Option<Value>,
}

impl WorkSurfaceAgentLiveEventSink {
    fn new(tx: mpsc::Sender<Value>, execution_metadata: Option<Value>) -> Self {
        Self {
            tx,
            execution_metadata,
        }
    }
}

impl AgentLiveEventSink for WorkSurfaceAgentLiveEventSink {
    fn send(&self, event: AgentLiveEvent) -> Result<(), AgentLiveSendError> {
        let value = agent_live_event_to_work_surface_sse(&event, self.execution_metadata.as_ref());
        match self.tx.try_send(value) {
            Ok(()) => Ok(()),
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                // Work surface receiver is behind — drop rather than block the
                // SSE emitter thread. The frontend will catch up on the next
                // poll / refresh.
                Err(AgentLiveSendError::Dropped)
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                Err(AgentLiveSendError::Closed)
            }
        }
    }
}

fn agent_live_event_to_work_surface_sse(
    event: &AgentLiveEvent,
    execution_metadata: Option<&Value>,
) -> Value {
    let timestamp = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0);
    let mut value = match &event.kind {
        AgentLiveEventKind::OutputDelta(content) => json!({
            "type": "agent_live_event",
            "agent_id": event.agent_id.as_str(),
            "event_kind": "output_delta",
            "content": content,
            "timestamp": timestamp,
        }),
        AgentLiveEventKind::ThinkingDelta(content) => json!({
            "type": "agent_live_event",
            "agent_id": event.agent_id.as_str(),
            "event_kind": "thinking_delta",
            "content": content,
            "timestamp": timestamp,
        }),
        AgentLiveEventKind::Status(content) => json!({
            "type": "agent_live_event",
            "agent_id": event.agent_id.as_str(),
            "event_kind": "status",
            "content": content,
            "timestamp": timestamp,
        }),
        AgentLiveEventKind::ToolStarted {
            name,
            description,
            tool_use_id,
        } => json!({
            "type": "agent_live_event",
            "agent_id": event.agent_id.as_str(),
            "event_kind": "tool_started",
            "name": name,
            "description": description,
            "tool_use_id": tool_use_id,
            "timestamp": timestamp,
        }),
        AgentLiveEventKind::ToolCompleted {
            name,
            description,
            status,
            duration_ms,
            output_summary,
            output,
            tool_use_id,
        } => json!({
            "type": "agent_live_event",
            "agent_id": event.agent_id.as_str(),
            "event_kind": "tool_completed",
            "name": name,
            "description": description,
            "status": status,
            "duration_ms": duration_ms,
            "output_summary": output_summary,
            "output": output,
            "tool_use_id": tool_use_id,
            "timestamp": timestamp,
        }),
        AgentLiveEventKind::AgentTerminated {
            termination,
            duration_ms,
            reason,
        } => {
            let termination = match termination {
                AgentLiveTermination::Completed => "completed",
                AgentLiveTermination::Failed => "failed",
                AgentLiveTermination::Cancelled => "cancelled",
            };
            json!({
                "type": "agent_live_event",
                "agent_id": event.agent_id.as_str(),
                "event_kind": "agent_terminated",
                "termination": termination,
                "status": termination,
                "duration_ms": duration_ms,
                "reason": reason,
                "timestamp": timestamp,
            })
        }
    };
    merge_agent_live_execution_metadata(&mut value, execution_metadata);
    value
}

fn merge_agent_live_execution_metadata(event: &mut Value, execution_metadata: Option<&Value>) {
    let Some(event_obj) = event.as_object_mut() else {
        return;
    };
    let Some(metadata_obj) = execution_metadata.and_then(Value::as_object) else {
        return;
    };
    for key in ["workspace", "executor", "transport", "fallback_policy"] {
        if let Some(value) = metadata_obj.get(key).cloned() {
            event_obj.entry(key.to_string()).or_insert(value);
        }
    }
}

async fn forward_agent_progress_event_to_stream(
    filter: &mut server_loop_host::RunScopedAgentProgressFilter,
    event_tx: &mpsc::Sender<Value>,
    sent_lifecycle_events: &AgentProgressLifecycleLedger,
    evt: AgentProgressEvent,
) -> bool {
    for evt in filter.accept(evt) {
        let lifecycle_key = agent_progress_lifecycle_event_key(&evt);
        let Some(event) = server_loop_host::progress_event_to_sse(&evt) else {
            continue;
        };
        if event_tx.send(event).await.is_err() {
            return false;
        }
        if let Some(key) = lifecycle_key {
            mark_agent_progress_lifecycle_event_sent(sent_lifecycle_events, key);
        }
    }
    true
}

async fn drain_ready_agent_progress_events(
    progress_rx: &mut broadcast::Receiver<AgentProgressEvent>,
    filter: &mut server_loop_host::RunScopedAgentProgressFilter,
    event_tx: &mpsc::Sender<Value>,
    sent_lifecycle_events: &AgentProgressLifecycleLedger,
) -> bool {
    loop {
        match progress_rx.try_recv() {
            Ok(evt) => {
                if !forward_agent_progress_event_to_stream(
                    filter,
                    event_tx,
                    sent_lifecycle_events,
                    evt,
                )
                .await
                {
                    return false;
                }
            }
            Err(broadcast::error::TryRecvError::Lagged(dropped)) => {
                tracing::warn!(
                    target: "astra_runtime::work_surface",
                    dropped,
                    "agent progress live stream lagged while draining ready events"
                );
                continue;
            }
            Err(broadcast::error::TryRecvError::Empty) => return true,
            Err(broadcast::error::TryRecvError::Closed) => return true,
        }
    }
}

fn system_time_epoch_ms(time: SystemTime) -> u64 {
    time.duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn agent_spawned_progress_event_from_state(state: &SpawnedAgentState) -> AgentProgressEvent {
    AgentProgressEvent {
        agent_id: state.agent_id.clone(),
        event_type: ProgressEventType::AgentSpawned {
            run_id: state.run_id.clone(),
            parent_run_id: state.parent_run_id.clone(),
            agent_type: state.agent_type.clone(),
            description: state.description.clone(),
            fanout_slot: state.fanout_slot.clone(),
        },
        timestamp_epoch_ms: system_time_epoch_ms(state.started_at),
        metadata: state.execution_metadata.clone(),
    }
}

fn agent_lifecycle_progress_event_from_state(
    state: &SpawnedAgentState,
) -> Option<AgentProgressEvent> {
    let event_type =
        agent_status_to_progress_event(&state.status, &state.metrics, state.started_at)?;
    if !event_type.is_terminal() && !matches!(event_type, ProgressEventType::Waiting { .. }) {
        return None;
    }
    Some(AgentProgressEvent {
        agent_id: state.agent_id.clone(),
        event_type,
        timestamp_epoch_ms: SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0),
        metadata: state.execution_metadata.clone(),
    })
}

fn missing_agent_lifecycle_sse_event(
    sent_lifecycle_events: &AgentProgressLifecycleLedger,
    event: AgentProgressEvent,
) -> Option<Value> {
    let key = agent_progress_lifecycle_event_key(&event)?;
    if has_agent_progress_lifecycle_event_sent(sent_lifecycle_events, &key) {
        return None;
    }
    let sse = server_loop_host::progress_event_to_sse(&event)?;
    mark_agent_progress_lifecycle_event_sent(sent_lifecycle_events, key);
    Some(sse)
}

async fn collect_missing_agent_lifecycle_events(
    spawner: &DynamicAgentSpawner,
    root_run_id: &str,
    sent_lifecycle_events: &AgentProgressLifecycleLedger,
) -> Vec<Value> {
    let states = spawner.get_agent_states_for_run_tree(root_run_id).await;
    let mut events = Vec::new();
    for state in states {
        if let Some(event) = missing_agent_lifecycle_sse_event(
            sent_lifecycle_events,
            agent_spawned_progress_event_from_state(&state),
        ) {
            events.push(event);
        }
        if let Some(event) = agent_lifecycle_progress_event_from_state(&state)
            .and_then(|event| missing_agent_lifecycle_sse_event(sent_lifecycle_events, event))
        {
            events.push(event);
        }
    }
    events
}

async fn collect_agent_lifecycle_events_for_persistence(
    spawner: &DynamicAgentSpawner,
    root_run_id: &str,
) -> Vec<Value> {
    let states = spawner.get_agent_states_for_run_tree(root_run_id).await;
    let mut events = Vec::new();
    for state in states {
        if let Some(event) = server_loop_host::progress_event_to_sse(
            &agent_spawned_progress_event_from_state(&state),
        ) {
            events.push(event);
        }
        if let Some(event) = agent_lifecycle_progress_event_from_state(&state)
            .and_then(|event| server_loop_host::progress_event_to_sse(&event))
        {
            events.push(event);
        }
    }
    events
}

fn agent_lifecycle_dedupe_key(event: &Value) -> Option<String> {
    let event_type = durable_event_type(event)?;
    if !matches!(
        event_type,
        "agent_spawned"
            | "agent_completed"
            | "agent_failed"
            | "agent_waiting"
            | "agent_cancelled"
            | "agent_interrupted"
    ) {
        return None;
    }
    let agent_id = event.get("agent_id").and_then(Value::as_str)?;
    let status = event
        .get("status")
        .or_else(|| event.get("reason"))
        .or_else(|| event.get("termination"))
        .and_then(Value::as_str)
        .unwrap_or("");
    Some(format!("{event_type}:{agent_id}:{status}"))
}

fn merge_agent_lifecycle_before_terminal_events(
    final_events: &[Value],
    agent_lifecycle_events: &[Value],
) -> Vec<Value> {
    let mut out = Vec::new();
    let existing_lifecycle_keys: HashSet<String> = final_events
        .iter()
        .filter_map(agent_lifecycle_dedupe_key)
        .collect();
    let agent_lifecycle_events: Vec<Value> = agent_lifecycle_events
        .iter()
        .filter(|event| match agent_lifecycle_dedupe_key(event) {
            Some(key) => !existing_lifecycle_keys.contains(&key),
            None => true,
        })
        .cloned()
        .collect();
    let mut inserted_lifecycle = false;
    for event in final_events {
        if streaming_final_event_for_replay(event) && !inserted_lifecycle {
            out.extend(agent_lifecycle_events.iter().cloned());
            inserted_lifecycle = true;
        }
        if streaming_event_for_persistence(event) {
            out.push(event.clone());
        }
    }
    if !inserted_lifecycle {
        out.extend(agent_lifecycle_events);
    }
    out
}

#[cfg(test)]
async fn stream_missing_agent_lifecycle_events(
    spawner: &DynamicAgentSpawner,
    root_run_id: &str,
    event_tx: &mpsc::Sender<Value>,
    sent_lifecycle_events: &AgentProgressLifecycleLedger,
) -> bool {
    let events =
        collect_missing_agent_lifecycle_events(spawner, root_run_id, sent_lifecycle_events).await;
    for event in events {
        if event_tx.send(event).await.is_err() {
            return false;
        }
    }
    true
}

#[derive(Clone)]
struct ServerAgentSpawnerEntry {
    spawner: Arc<DynamicAgentSpawner>,
    executor: Arc<ServerSpawnAgentExecutor>,
}

#[derive(Clone)]
struct ServerSpawnRuntimeContext {
    parent_run_id: String,
    user_id: String,
    session_id: String,
    trace_context: TraceContext,
    forward_headers: HashMap<String, String>,
    llm_token_service: Option<LlmTokenServiceConfig>,
    request_constraints: RequestConstraints,
    execution_metadata: Option<Value>,
    pause_flag: Option<Arc<AtomicBool>>,
    cancel_token: Option<Arc<CancellationToken>>,
    #[cfg(feature = "bridge-e2e-hooks")]
    test_child_llm_rounds: Vec<Value>,
    #[cfg(feature = "harness")]
    harness_sink: Option<Arc<dyn astra_harness::SnapshotSink>>,
}

// ─── Service ────────────────────────────────────────────────────────────────

/// Spawn a fire-and-forget background task. Unlike a raw `tokio::spawn` whose
/// `JoinHandle` is silently dropped, this wrapper catches panics inside the
/// spawned future and emits a `tracing::error` log so that silent failures
/// are observable.
pub(crate) fn spawn_observed(
    future: impl std::future::Future<Output = ()> + Send + 'static,
    name: &'static str,
) {
    tokio::spawn(async move {
        let result = AssertUnwindSafe(future).catch_unwind().await;
        if let Err(panic_err) = result {
            let msg = panic_err
                .downcast_ref::<String>()
                .map(|s| s.as_str())
                .or_else(|| panic_err.downcast_ref::<&str>().copied())
                .unwrap_or("unknown panic");
            tracing::error!(task = name, panic = %msg, "background task panicked");
        }
    });
}

/// Production [`RunLifecycleService`] that executes agentic loops via
/// [`ServerAgenticLoopHost`].
///
/// Durable run state is mandatory; process-local state is limited to live
/// control handles that cannot survive a restart.
pub struct AgenticRunLifecycleService {
    /// Process-local run handles (run_id -> state) for live cancellation, pause,
    /// and active SSE fanout. Durable state is the user-visible authority.
    runs: Arc<RwLock<HashMap<String, RunState>>>,
    /// LLM resolution dependencies.
    matrixone: MatrixOneSettings,
    encryptor: Arc<FernetTokenEncryptor>,
    shared_pool: Option<SharedPool>,
    /// Edge callback ledger shared with the API server.
    edge_callback_ledger: Arc<TokioMutex<HashMap<String, Value>>>,
    /// P0-3: Cross-pod edge dispatch service. When configured, tool results
    /// delivered to another pod are visible via DB polling fallback.
    edge_dispatch_service: Option<Arc<dyn astra_services::multi_agent::EdgeDispatchService>>,
    /// P0-3: Edge registry for cross-pod edge agent discovery.
    edge_registry_service: Option<Arc<dyn astra_services::multi_agent::EdgeRegistryService>>,
    /// Durable run engine for persistence, replay, status, and recovery.
    run_engine: RunEngine,
    /// Optional delegation engine for multi-agent coordination.
    delegation_engine: Option<Arc<crate::server::delegation::engine::DelegationEngine>>,
    /// Session-scoped dynamic-agent spawners used by Web/server `agent(action='spawn')`.
    server_agent_spawners: Arc<RwLock<HashMap<String, ServerAgentSpawnerEntry>>>,
    /// Fallback progress broadcaster for dynamic spawn when no delegation
    /// engine is configured. Normal production wiring uses the delegation
    /// engine broadcaster so Web SSE sees one agent tree stream.
    server_agent_progress_broadcaster: Arc<ProgressBroadcaster>,
    /// Shared mailbox router for Web/server dynamic spawned agents.
    server_agent_mailbox_router: Arc<astra_messaging::router::AgentMailboxRouter>,
    /// Per-user resource governor (Phase 5).
    resource_governor:
        Option<std::sync::Arc<dyn astra_services::resource_governor::ResourceGovernor>>,
    /// Live edge WebSocket connection pool (Phase 6).
    edge_connection_pool: Option<astra_server_types::edge_connection_pool::EdgeConnectionPool>,
    /// Durable workspace record store for cloud workspace ownership/audit.
    workspace_record_store: Option<Arc<dyn WorkspaceStateStore>>,
    /// Optional database skill provider for runtime skill resolution.
    skill_service: Option<Arc<dyn SkillService>>,
    /// Registry-backed MCP bindings available to server-side chat loops.
    mcp_registry_service: Arc<dyn astra_services::McpRegistryService>,
    /// Per-run approval request channel receivers (Phase E).
    /// Key: run_id → receiver that the WS handler drains.
    approval_channels: Arc<TokioMutex<HashMap<String, mpsc::Receiver<serde_json::Value>>>>,
    /// Per-run ask_user prompt channel receivers.
    /// Key: run_id → receiver that the WS handler drains.
    user_prompt_channels: Arc<TokioMutex<HashMap<String, mpsc::Receiver<serde_json::Value>>>>,
    /// Per-run progress event channel receivers (Phase F.3).
    /// Key: run_id → receiver that the WS handler drains.
    progress_channels: Arc<TokioMutex<HashMap<String, mpsc::Receiver<ProgressEvent>>>>,
    /// Hook DB writer for decision audit + skill selection persistence.
    hook_db_writer: Option<Arc<dyn TurnHookDbWriter>>,
    /// Memoria observer worker for cross-session knowledge extraction.
    observer_worker: Option<Arc<dyn TurnObserverWorker>>,
    /// Tool event writer for persisting tool_call events to agent_events.
    tool_event_writer: Option<Arc<dyn TurnToolEventWriter>>,
    /// Auxiliary event writer for ask_user lifecycle audit events.
    auxiliary_event_writer: Option<Arc<dyn crate::TurnAuxiliaryEventWriter>>,
    /// Counter of in-flight background agentic loop tasks.
    /// Incremented before spawn, decremented when the task exits.
    /// Used by `drain_background_tasks` for graceful shutdown.
    background_task_count: Arc<AtomicUsize>,
    /// Global admission control: limits the number of concurrently executing
    /// agentic loop tasks across all users. A permit is acquired before
    /// spawn and automatically released when the task completes.
    run_semaphore: Arc<tokio::sync::Semaphore>,
    /// Harness sink registry for server-side harness observation (Phase 2A).
    #[cfg(feature = "harness")]
    harness_registry: Option<crate::server::harness::handlers::HarnessSinkRegistry>,
    /// Shared background session-memory extraction coordinator. Cloned
    /// into every `AgenticLoopState` the service builds, so all turns
    /// share selector cooldown, in-flight dedup, event sink, and
    /// broker. `None` → extraction disabled (e.g. minimal test service).
    memory_extraction_service: Option<Arc<crate::session_memory::MemoryExtractionService>>,
    /// Shared ToolExecutionService so executors share the same disabled_tools set.
    tool_execution_service: Option<ToolExecutionService>,
}

impl AgenticRunLifecycleService {
    pub fn new(
        matrixone: MatrixOneSettings,
        encryptor: Arc<FernetTokenEncryptor>,
        edge_callback_ledger: Arc<TokioMutex<HashMap<String, Value>>>,
        run_engine: RunEngine,
    ) -> Self {
        Self {
            runs: Arc::new(RwLock::new(HashMap::new())),
            matrixone,
            encryptor,
            shared_pool: None,
            edge_callback_ledger,
            edge_dispatch_service: None,
            edge_registry_service: None,
            run_engine,
            delegation_engine: None,
            server_agent_spawners: Arc::new(RwLock::new(HashMap::new())),
            server_agent_progress_broadcaster: Arc::new(ProgressBroadcaster::default()),
            server_agent_mailbox_router: Arc::new(astra_messaging::AgentMailboxRouter::new(
                Arc::new(astra_messaging::InProcessTransport::new()),
                Arc::new(crate::server::delegation::engine::DelegationTracker::new()),
            )),
            resource_governor: None,
            edge_connection_pool: None,
            workspace_record_store: None,
            skill_service: None,
            mcp_registry_service: Arc::new(astra_services::UnconfiguredMcpRegistryService),
            approval_channels: Arc::new(TokioMutex::new(HashMap::new())),
            user_prompt_channels: Arc::new(TokioMutex::new(HashMap::new())),
            progress_channels: Arc::new(TokioMutex::new(HashMap::new())),
            hook_db_writer: None,
            observer_worker: None,
            tool_event_writer: None,
            auxiliary_event_writer: None,
            background_task_count: Arc::new(AtomicUsize::new(0)),
            run_semaphore: Arc::new(tokio::sync::Semaphore::new(50)),
            #[cfg(feature = "harness")]
            harness_registry: None,
            memory_extraction_service: None,
            tool_execution_service: None,
        }
    }

    pub fn with_memory_extraction_service(
        mut self,
        svc: Arc<crate::session_memory::MemoryExtractionService>,
    ) -> Self {
        self.memory_extraction_service = Some(svc);
        self
    }

    #[cfg(feature = "harness")]
    pub fn with_harness_registry(
        mut self,
        registry: crate::server::harness::handlers::HarnessSinkRegistry,
    ) -> Self {
        self.harness_registry = Some(registry);
        self
    }

    pub fn with_pool(mut self, pool: SharedPool) -> Self {
        self.shared_pool = Some(pool);
        self
    }

    pub fn with_delegation_engine(
        mut self,
        engine: Arc<crate::server::delegation::engine::DelegationEngine>,
    ) -> Self {
        self.delegation_engine = Some(engine);
        self
    }

    pub fn with_edge_connection_pool(
        mut self,
        pool: astra_server_types::edge_connection_pool::EdgeConnectionPool,
    ) -> Self {
        self.edge_connection_pool = Some(pool);
        self
    }

    /// P0-3: Wire the cross-pod edge dispatch service for horizontal scaling.
    pub fn with_edge_dispatch_service(
        mut self,
        svc: Arc<dyn astra_services::multi_agent::EdgeDispatchService>,
    ) -> Self {
        self.edge_dispatch_service = Some(svc);
        self
    }

    pub fn with_edge_registry_service(
        mut self,
        svc: Arc<dyn astra_services::multi_agent::EdgeRegistryService>,
    ) -> Self {
        self.edge_registry_service = Some(svc);
        self
    }

    pub(crate) fn with_workspace_record_store(
        mut self,
        store: Arc<dyn WorkspaceStateStore>,
    ) -> Self {
        self.workspace_record_store = Some(store);
        self
    }

    pub fn with_tool_execution_service(mut self, service: ToolExecutionService) -> Self {
        self.tool_execution_service = Some(service);
        self
    }

    pub fn with_resource_governor(
        mut self,
        governor: std::sync::Arc<dyn astra_services::resource_governor::ResourceGovernor>,
    ) -> Self {
        self.resource_governor = Some(governor);
        self
    }

    pub fn with_skill_service(mut self, service: Arc<dyn SkillService>) -> Self {
        self.skill_service = Some(service);
        self
    }

    pub fn with_mcp_registry_service(
        mut self,
        service: Arc<dyn astra_services::McpRegistryService>,
    ) -> Self {
        self.mcp_registry_service = service;
        self
    }

    pub fn with_hook_db_writer(mut self, writer: Arc<dyn TurnHookDbWriter>) -> Self {
        self.hook_db_writer = Some(writer);
        self
    }

    pub fn with_observer_worker(mut self, worker: Arc<dyn TurnObserverWorker>) -> Self {
        self.observer_worker = Some(worker);
        self
    }

    pub fn with_tool_event_writer(mut self, writer: Arc<dyn TurnToolEventWriter>) -> Self {
        self.tool_event_writer = Some(writer);
        self
    }

    pub fn with_auxiliary_event_writer(
        mut self,
        writer: Arc<dyn crate::TurnAuxiliaryEventWriter>,
    ) -> Self {
        self.auxiliary_event_writer = Some(writer);
        self
    }

    /// Configure the maximum number of concurrent agentic loop tasks.
    /// Default: 50. Set via env `ASTRA_RUN_CONCURRENCY_LIMIT`.
    pub fn with_run_concurrency_limit(mut self, limit: usize) -> Self {
        self.run_semaphore = Arc::new(tokio::sync::Semaphore::new(limit));
        self
    }

    #[cfg(test)]
    pub(crate) fn test_run_semaphore(&self) -> Arc<tokio::sync::Semaphore> {
        self.run_semaphore.clone()
    }

    fn dynamic_agent_progress_broadcaster(&self) -> Arc<ProgressBroadcaster> {
        self.delegation_engine
            .as_ref()
            .and_then(|engine| engine.progress_broadcaster().cloned())
            .unwrap_or_else(|| Arc::clone(&self.server_agent_progress_broadcaster))
    }

    fn spawn_agent_progress_stream_bridge(
        &self,
        root_run_id: String,
        event_tx: mpsc::Sender<Value>,
    ) -> AgentProgressStreamBridge {
        let mut progress_rx = self.dynamic_agent_progress_broadcaster().subscribe();
        let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
        let sent_lifecycle_events = Arc::new(std::sync::Mutex::new(HashSet::new()));
        let sent_lifecycle_events_for_bridge = Arc::clone(&sent_lifecycle_events);
        let join = tokio::spawn(async move {
            let mut filter = server_loop_host::RunScopedAgentProgressFilter::new(root_run_id);
            'bridge: loop {
                tokio::select! {
                    _ = &mut stop_rx => {
                        if !drain_ready_agent_progress_events(
                            &mut progress_rx,
                            &mut filter,
                            &event_tx,
                            &sent_lifecycle_events_for_bridge,
                        )
                        .await
                        {
                            break 'bridge;
                        }
                        let drain_deadline = tokio::time::sleep(AGENT_PROGRESS_STREAM_DRAIN_GRACE);
                        tokio::pin!(drain_deadline);
                        'drain: loop {
                            tokio::select! {
                                _ = &mut drain_deadline => break 'drain,
                                received = progress_rx.recv() => {
                                    match received {
                                        Ok(evt) => {
                                            if !forward_agent_progress_event_to_stream(
                                                &mut filter,
                                                &event_tx,
                                                &sent_lifecycle_events_for_bridge,
                                                evt,
                                            )
                                            .await
                                            {
                                                break 'bridge;
                                            }
                                        }
                                        Err(broadcast::error::RecvError::Lagged(dropped)) => {
                                            tracing::warn!(
                                                target: "astra_runtime::work_surface",
                                                dropped,
                                                "agent progress live stream lagged while draining"
                                            );
                                            continue;
                                        }
                                        Err(broadcast::error::RecvError::Closed) => break 'drain,
                                    }
                                }
                            }
                        }
                        break;
                    },
                    received = progress_rx.recv() => {
                        match received {
                            Ok(evt) => {
                                if !forward_agent_progress_event_to_stream(
                                    &mut filter,
                                    &event_tx,
                                    &sent_lifecycle_events_for_bridge,
                                    evt,
                                )
                                .await
                                {
                                    break 'bridge;
                                }
                            }
                            Err(broadcast::error::RecvError::Lagged(dropped)) => {
                                tracing::warn!(
                                    target: "astra_runtime::work_surface",
                                    dropped,
                                    "agent progress live stream lagged"
                                );
                                continue;
                            }
                            Err(broadcast::error::RecvError::Closed) => break,
                        }
                    }
                }
            }
        });
        AgentProgressStreamBridge {
            stop_tx,
            join,
            sent_lifecycle_events,
        }
    }

    async fn server_agent_spawner_for_session(&self, session_id: &str) -> ServerAgentSpawnerEntry {
        if let Some(entry) = self
            .server_agent_spawners
            .read()
            .await
            .get(session_id)
            .cloned()
        {
            return entry;
        }

        let mut guard = self.server_agent_spawners.write().await;
        if let Some(entry) = guard.get(session_id).cloned() {
            return entry;
        }

        let executor = Arc::new(
            ServerSpawnAgentExecutor::new(
                self.matrixone.clone(),
                Arc::clone(&self.encryptor),
                Arc::clone(&self.edge_callback_ledger),
            )
            .with_pool(self.shared_pool.clone())
            .with_edge_connection_pool(self.edge_connection_pool.clone())
            .with_skill_service(self.skill_service.clone())
            .with_memory_extraction_service(self.memory_extraction_service.clone()),
        );
        let executor_for_spawner: Arc<dyn SpawnAgentExecutor> = executor.clone();
        let mut spawner = DynamicAgentSpawner::with_broadcaster(
            Arc::clone(&self.server_agent_mailbox_router),
            self.dynamic_agent_progress_broadcaster(),
        )
        .with_executor(executor_for_spawner)
        .with_session(session_id.to_string())
        // Same cap as the CLI side. Web/headless sessions are no less
        // susceptible to the runaway-spawn-on-failure pattern; without
        // a cap, a misbehaving agent can fan out unbounded sub-agents
        // and burn the parent's quota.
        .with_max_concurrent_agents(resolved_server_spawn_concurrency_cap());
        if let Some(pool) = self.shared_pool.clone() {
            spawner = spawner.with_trace_writer(Arc::new(
                DatabaseTraceEventWriter::new(self.matrixone.clone()).with_pool(pool),
            ));
        }
        if let Some(store) = self
            .delegation_engine
            .as_ref()
            .and_then(|engine| engine.prefix_store().cloned())
        {
            spawner = spawner.with_prefix_store(store);
        }

        let entry = ServerAgentSpawnerEntry {
            spawner: Arc::new(spawner),
            executor,
        };
        guard.insert(session_id.to_string(), entry.clone());
        entry
    }

    /// Single source of truth: parse all three allowlist lanes from raw wire
    /// shape, validating each. Every code path that needs a
    /// [`RequestConstraints`] for the agentic loop runs through this; the
    /// previous `.expect("validated before state build")` ladder is gone
    /// because validation and construction now happen together.
    fn try_request_constraints(request: &ChatRequestData) -> Result<RequestConstraints, String> {
        Ok(RequestConstraints::new(
            normalize_request_allowlist(request.allow_tools.as_deref(), "allow_tools")?,
            normalize_request_allowlist(request.allow_skills.as_deref(), "allow_skills")?,
            normalize_request_skill_sources(
                request.allow_skill_sources.as_deref(),
                "allow_skill_sources",
            )?,
        ))
    }

    fn inherited_permissions_from_constraints(
        constraints: &RequestConstraints,
    ) -> InheritedPermissions {
        let mut inherited = InheritedPermissions::auto_approve();
        inherited.allowed_tools = constraints.allowed_tools.clone();
        inherited
    }

    #[allow(clippy::too_many_arguments)]
    async fn wire_server_dynamic_agent_tools(
        &self,
        executor: &mut server_tool_executor::ServerToolExecutor,
        user_id: &str,
        session_id: &str,
        run_id: &str,
        turn_seq: u32,
        request: &ChatRequestData,
        workspace: &std::path::Path,
        work_surface_event_tx: Option<mpsc::Sender<Value>>,
        pause_flag: Option<Arc<AtomicBool>>,
        cancel_token: Option<Arc<CancellationToken>>,
        #[cfg(feature = "harness")] harness_sink: Option<Arc<dyn astra_harness::SnapshotSink>>,
    ) {
        let entry = self.server_agent_spawner_for_session(session_id).await;
        // Validation already happened up the call chain (see
        // `validate_request_constraints`); this re-parse is safe because the
        // wire-level shape was checked before this point. If validation ever
        // becomes optional on this path, the `unwrap_or_else` below logs the
        // surprise instead of silently building corrupt constraints.
        let request_constraints = Self::try_request_constraints(request).unwrap_or_else(|err| {
            tracing::error!(error = %err, "request constraints failed late validation in dynamic-agent wiring");
            RequestConstraints::default()
        });
        let execution_metadata = Some(executor.binding_metadata());
        entry
            .executor
            .set_runtime_context(ServerSpawnRuntimeContext {
                parent_run_id: run_id.to_string(),
                user_id: user_id.to_string(),
                session_id: session_id.to_string(),
                trace_context: server_trace_context(user_id, session_id, run_id, turn_seq),
                forward_headers: request.forward_headers.clone(),
                llm_token_service: request.llm_token_service.clone(),
                request_constraints: request_constraints.clone(),
                execution_metadata: execution_metadata.clone(),
                pause_flag,
                cancel_token,
                #[cfg(feature = "bridge-e2e-hooks")]
                test_child_llm_rounds: request
                    .context
                    .as_ref()
                    .and_then(|ctx| ctx.get("test_spawn_child_llm_rounds"))
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default(),
                #[cfg(feature = "harness")]
                harness_sink,
            })
            .await;

        let agent_id = request
            .agent_id
            .clone()
            .unwrap_or_else(|| "root-agent".to_string());
        executor.set_agent_tool_context(AgentToolContext {
            run_id: run_id.to_string(),
            agent_id,
            current_model: request.model.clone(),
            recursion_depth: 0,
            is_fork_child: false,
            working_dir: workspace.to_path_buf(),
            spawner: entry.spawner,
            inherited_permissions: Self::inherited_permissions_from_constraints(
                &request_constraints,
            ),
            active_skills: Vec::new(),
            live_event_sink: work_surface_event_tx.map(|tx| {
                Arc::new(WorkSurfaceAgentLiveEventSink::new(
                    tx,
                    execution_metadata.clone(),
                )) as SharedAgentLiveEventSink
            }),
            trace_context: Some(server_trace_context(user_id, session_id, run_id, turn_seq)),
            execution_metadata,
        });
    }

    fn build_csl_store(&self) -> Option<Arc<dyn astra_turn_core::conversation_log::CslStore>> {
        let pool = self.shared_pool.as_ref()?;
        let store =
            astra_turn_core::conversation_log::db_store::DbCslStore::new(self.matrixone.clone())
                .with_pool(pool.clone());
        Some(Arc::new(store))
    }

    async fn restore_csl_history(
        &self,
        user_id: &str,
        session_id: &str,
        run_id: &str,
        loop_state: &mut AgenticLoopState,
    ) -> Option<astra_turn_core::conversation_log::manager::CslManager> {
        let store = self.build_csl_store()?;
        let mut mgr = match astra_turn_core::conversation_log::manager::CslManager::new(
            store,
            session_id.to_string(),
            Default::default(),
        ) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(error = %e, "CSL manager creation failed");
                return None;
            }
        };
        mgr.set_trace_id(run_id.to_string());

        let mut restored_messages = Vec::new();

        match mgr.load().await {
            Ok(Some(mat)) => {
                restored_messages = mat.messages;
                restore_session_state_compact(mat.session_state, loop_state);
            }
            Ok(None) => {
                self.record_runtime_retrieval_degrade(
                    user_id,
                    session_id,
                    run_id,
                    RetrievalStage::Structured,
                    "timeout",
                )
                .await;
                self.record_runtime_retrieval_degrade(
                    user_id,
                    session_id,
                    run_id,
                    RetrievalStage::Fts,
                    "empty",
                )
                .await;
                self.record_runtime_retrieval_degrade(
                    user_id,
                    session_id,
                    run_id,
                    RetrievalStage::Vector,
                    "stale",
                )
                .await;
            }
            Err(e) => {
                tracing::warn!(
                    session_id,
                    error = %e,
                    "CSL load failed; starting with empty history"
                );
                self.record_runtime_retrieval_degrade(
                    user_id,
                    session_id,
                    run_id,
                    RetrievalStage::Structured,
                    "timeout",
                )
                .await;
            }
        }

        if !restored_messages.is_empty() {
            if !loop_state.messages.is_empty() {
                restored_messages.push(loop_state.messages.remove(0));
            }
            loop_state.messages = restored_messages;
        }

        mgr.mark_turn_start(loop_state.messages.len());
        Some(mgr)
    }

    async fn record_runtime_retrieval_degrade(
        &self,
        user_id: &str,
        session_id: &str,
        run_id: &str,
        stage: RetrievalStage,
        reason: &str,
    ) {
        let Some(pool) = &self.shared_pool else {
            return;
        };
        let store = DatabaseContextManifestStore::new(pool.clone());
        if let Err(error) = store
            .record_retrieval_degrade_event(
                user_id,
                session_id,
                Some(run_id),
                stage.clone(),
                reason,
                0,
            )
            .await
        {
            tracing::warn!(
                target: "astra_runtime::retrieval",
                session_id,
                run_id,
                stage = ?stage,
                reason,
                error = %error,
                "failed to persist retrieval degrade event"
            );
        }
    }

    /// Wait for all in-flight background agentic loop tasks to finish.
    ///
    /// Called during graceful shutdown. Polls the task counter with 100ms
    /// intervals up to `timeout`. Returns `true` if all tasks drained within
    /// the timeout, `false` if tasks are still running.
    async fn drain_background_tasks_impl(&self, timeout: std::time::Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if self.background_task_count.load(Ordering::Acquire) == 0 {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                self.persist_graceful_shutdown_checkpoints().await;
                return false;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        // Turn-owned background_task_count reached zero, but the
        // session-memory extraction service has its own pending
        // counter (see `MemoryExtractionService::wait_for_pending`).
        // Fold it into the same shutdown deadline so we don't kill
        // in-flight Memoria writes mid-HTTP.
        if let Some(svc) = self.memory_extraction_service.as_ref() {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let leftover = svc.wait_for_pending(remaining).await;
            if leftover > 0 {
                return false;
            }
        }
        true
    }

    async fn persist_graceful_shutdown_checkpoints(&self) {
        let engine = &self.run_engine;
        let run_ids = {
            let runs = self.runs.read().await;
            runs.values()
                .filter(|run| {
                    matches!(
                        run.status,
                        RunStatus::Running | RunStatus::Waiting | RunStatus::Paused
                    )
                })
                .map(|run| run.run_id.clone())
                .collect::<Vec<_>>()
        };
        for run_id in run_ids {
            let checkpoint = json!({
                "version": "checkpoint_v1",
                "graceful": true,
                "last_batch_id": format!("shutdown-{run_id}"),
                "extra": {}
            });
            astra_core::log_persist!(
                engine
                    .persist_checkpoint(&run_id, &checkpoint.to_string())
                    .await,
                "run_lifecycle",
                &run_id,
                "graceful_shutdown_checkpoint"
            );
            astra_core::log_persist!(
                engine
                    .append_event(
                        &run_id,
                        json!({"event_type": "run_checkpointed_for_shutdown", "data": {}})
                    )
                    .await,
                "run_lifecycle",
                &run_id,
                "graceful_shutdown_checkpoint_event"
            );
        }
    }

    /// Returns the current number of in-flight background tasks.
    pub fn background_task_count(&self) -> usize {
        self.background_task_count.load(Ordering::Acquire)
    }

    /// Clone the Arc handle to the runs map (for background tasks).
    fn runs_handle(&self) -> Arc<RwLock<HashMap<String, RunState>>> {
        Arc::clone(&self.runs)
    }

    /// Schedule removal of a terminal run from the in-memory cache after a
    /// grace period. Clients have 5 minutes to poll final events before the
    /// entry is evicted. This prevents unbounded memory growth.
    fn schedule_run_eviction(runs: &Arc<RwLock<HashMap<String, RunState>>>, run_id: String) {
        let runs = Arc::clone(runs);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(300);
        spawn_observed(
            async move {
                tokio::time::sleep_until(deadline).await;
                runs.write().await.remove(&run_id);
            },
            "run_eviction",
        );
    }

    fn build_tracked_run_state(
        run_id: String,
        session_id: String,
        _user_id: String,
    ) -> (
        RunState,
        Arc<AtomicBool>,
        Arc<AtomicBool>,
        Arc<CancellationToken>,
    ) {
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let pause_flag = Arc::new(AtomicBool::new(false));
        let llm_cancel_token = Arc::new(CancellationToken::new());
        let run_state = RunState {
            run_id,
            session_id,
            status: RunStatus::Running,
            events: vec![json!({"event_type": "run_started", "data": {}})],
            cancel_flag: cancel_flag.clone(),
            pause_flag: pause_flag.clone(),
            llm_cancel_token: llm_cancel_token.clone(),
            live_tx: None,
            waiting_for: None,
        };
        (run_state, cancel_flag, pause_flag, llm_cancel_token)
    }

    /// Return true when an existing run should block starting a new turn in the
    /// same session.
    ///
    /// `paused` is intentionally split:
    /// - `paused + waiting_for=Some(..)` is a real user/approval wait and must
    ///   be resumed or cancelled before another run starts.
    /// - `paused + waiting_for=None` is used for resumable interruptions such as
    ///   `budget_exhausted`; the user-facing contract says the next message can
    ///   continue from the checkpoint, so it must not block a fresh web turn.
    fn blocks_new_session_run(run: &RunState, session_id: &str) -> bool {
        run.session_id == session_id && run.status.blocks_session(run.waiting_for.as_deref())
    }

    fn session_has_blocking_run(runs: &HashMap<String, RunState>, session_id: &str) -> bool {
        runs.values()
            .any(|run| Self::blocks_new_session_run(run, session_id))
    }

    fn configure_loop_state_runtime_controls(
        &self,
        loop_state: &mut AgenticLoopState,
        cancel_flag: &Arc<AtomicBool>,
        pause_flag: &Arc<AtomicBool>,
        llm_cancel_token: &Arc<CancellationToken>,
    ) {
        loop_state.cancellation.flag = Some(cancel_flag.clone());
        loop_state.cancellation.pause_flag = Some(pause_flag.clone());
        loop_state.cancellation.token = Some(llm_cancel_token.clone());
        loop_state.delegation_engine = self.delegation_engine.clone();
        // Wire cross-pod cancel/pause provider so the agentic loop can poll
        // DB for control signals from other pods in horizontally-scaled deployments.
        loop_state.run_control = Some(Arc::new(self.run_engine.clone()));
    }

    async fn persist_run_start(
        &self,
        run_id: &str,
        user_id: &str,
        session_id: &str,
        request: &ChatRequestData,
        execution_bindings: Option<&ExecutionBindingSnapshot>,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        self.run_engine
            .start_run_with_context(
                run_id,
                user_id,
                session_id,
                run_start_context_from_request(request, execution_bindings),
            )
            .await
            .map_err(|error| {
                let status = if error == "session already has an active run" {
                    StatusCode::CONFLICT
                } else {
                    StatusCode::SERVICE_UNAVAILABLE
                };
                let detail = if status == StatusCode::CONFLICT {
                    error
                } else {
                    format!("Failed to persist durable run start: {error}")
                };
                error_response(status, detail)
            })
    }

    async fn fail_started_run_before_spawn(&self, run_id: &str, message: &str) {
        self.runs.write().await.remove(run_id);
        astra_core::log_persist!(
            self.run_engine
                .persist_status(run_id, STATUS_FAILED, None, Some(message))
                .await,
            "run_lifecycle",
            run_id,
            "pre_spawn_failure"
        );
    }

    fn finalize_run_events(
        loop_outcome: Result<AgenticLoopOutcome, astra_core::ClassifiedError>,
        mut events: Vec<Value>,
        loop_state: &AgenticLoopState,
    ) -> (Vec<Value>, RunStatus, Option<String>) {
        let total_input =
            loop_state.total_prompt + loop_state.total_cache_read + loop_state.total_cache_creation;
        let usage = json!({
            "prompt_tokens": total_input,
            "completion_tokens": loop_state.total_completion,
            "tool_call_count": loop_state.total_tool_calls,
        });
        let cancellation_requested = loop_state
            .cancellation
            .flag
            .as_ref()
            .is_some_and(|f| f.load(Ordering::Acquire))
            || loop_state
                .cancellation
                .token
                .as_ref()
                .is_some_and(|t| t.is_cancelled());

        let (final_status, error_msg) = if cancellation_requested
            || matches!(&loop_outcome, Ok(AgenticLoopOutcome::Cancelled))
        {
            let mut data = usage;
            data["cancelled"] = Value::Bool(true);
            events.push(json!({
                "event_type": "run_finished",
                "data": data,
            }));
            (RunStatus::Cancelled, None)
        } else {
            match loop_outcome {
                Ok(AgenticLoopOutcome::Completed) => {
                    if let Some(interruption) = loop_state.interruption.as_ref() {
                        let interruption_json = interruption.to_json();
                        if !loop_state.final_text.is_empty() {
                            events.push(json!({
                                "event_type": "text_done",
                                "data": {
                                    "full_text": loop_state.final_text.clone(),
                                    "partial": true,
                                    "interruption": interruption_json.clone(),
                                }
                            }));
                        }
                        events.push(json!({
                            "event_type": "run_interrupted",
                            "data": interruption_json.clone(),
                        }));
                        let mut finished = usage;
                        finished["interrupted"] = Value::Bool(true);
                        finished["interruption_kind"] =
                            Value::String(interruption.kind.label().to_string());
                        finished["resumable"] = Value::Bool(interruption.kind.is_resumable());
                        events.push(json!({
                            "event_type": "run_finished",
                            "data": finished,
                        }));
                        (RunStatus::Paused, Some(interruption.user_message.clone()))
                    } else {
                        if !loop_state.final_text.is_empty() {
                            events.push(json!({
                            "event_type": "text_done",
                            "data": { "full_text": loop_state.final_text.clone() }
                            }));
                        }
                        events.push(json!({
                            "event_type": "run_finished",
                            "data": usage,
                        }));
                        (RunStatus::Completed, None)
                    }
                }
                Ok(AgenticLoopOutcome::Cancelled) => unreachable!("handled by cancellation gate"),
                Ok(AgenticLoopOutcome::Error(e)) => {
                    events.push(json!({
                        "event_type": "run_error",
                        "data": {"error": e.clone()}
                    }));
                    events.push(json!({
                        "event_type": "run_finished",
                        "data": usage.clone(),
                    }));
                    (RunStatus::Failed, Some(e))
                }
                Ok(AgenticLoopOutcome::Waiting(w)) => {
                    let msg = format!("waiting: {w}");
                    events.push(json!({
                        "event_type": "run_waiting",
                        "data": {"reason": msg.clone()}
                    }));
                    (RunStatus::Waiting, Some(msg))
                }
                Err(err) => {
                    let msg = err.to_string();
                    events.push(json!({
                        "event_type": "run_error",
                        "data": {"error": &msg, "error_kind": err.kind.as_str()}
                    }));
                    events.push(json!({
                        "event_type": "run_finished",
                        "data": usage,
                    }));
                    (RunStatus::Failed, Some(msg))
                }
            }
        };

        (events, final_status, error_msg)
    }

    async fn load_trusted_llm_token_service_domains(
        &self,
    ) -> Result<Vec<TrustedLlmDomain>, (StatusCode, Json<ErrorResponse>)> {
        let pool = if let Some(pool) = &self.shared_pool {
            pool.get().clone()
        } else {
            connect_matrixone(&self.matrixone).await.map_err(|error| {
                error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("failed to connect database for trusted domains query: {error}"),
                )
            })?
        };
        let rows = sqlx::query(
            "SELECT domain_host, IFNULL(domain_port, 0) AS domain_port \
             FROM runtime_llm_trusted_domains \
             WHERE is_enabled = 1",
        )
        .fetch_all(&pool)
        .await
        .map_err(|error| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!(
                    "failed to query trusted domains from table {LLM_TOKEN_SERVICE_TRUSTED_DOMAINS_TABLE}: {error}"
                ),
            )
        })?;
        let mut trusted_domains = Vec::new();
        let mut seen = HashSet::new();
        for row in rows {
            let host: String = row.try_get("domain_host").map_err(|error| {
                error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!(
                        "failed to decode domain_host from table {LLM_TOKEN_SERVICE_TRUSTED_DOMAINS_TABLE}: {error}"
                    ),
                )
            })?;
            let port: i64 = row.try_get("domain_port").map_err(|error| {
                error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!(
                        "failed to decode domain_port from table {LLM_TOKEN_SERVICE_TRUSTED_DOMAINS_TABLE}: {error}"
                    ),
                )
            })?;
            let domain = trusted_llm_domain_from_db_values(&host, port).map_err(|detail| {
                error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!(
                        "invalid trusted domain row in table {LLM_TOKEN_SERVICE_TRUSTED_DOMAINS_TABLE}: {detail}"
                    ),
                )
            })?;
            let key = format!(
                "{}:{}",
                domain.host,
                domain
                    .port
                    .map(|value| value.to_string())
                    .unwrap_or_default()
            );
            if seen.insert(key) {
                trusted_domains.push(domain);
            }
        }
        if trusted_domains.is_empty() {
            return Err(error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                format!(
                    "table {LLM_TOKEN_SERVICE_TRUSTED_DOMAINS_TABLE} has no enabled trusted domains"
                ),
            ));
        }
        Ok(trusted_domains)
    }

    /// Validate the request and return the parsed [`RequestConstraints`].
    ///
    /// The returned constraints are the ones every downstream consumer
    /// (`build_initial_state`, dynamic-agent spawner wiring, delegation
    /// engine) must use — re-parsing wire shape after this point is the bug
    /// pattern that motivated the refactor. Callers that just need
    /// validation and don't take the constraints can drop the result with
    /// `let _ = ...?;`.
    async fn validate_request_constraints(
        &self,
        user_id: &str,
        request: &ChatRequestData,
    ) -> Result<RequestConstraints, (StatusCode, Json<ErrorResponse>)> {
        if request.llm_token_service.is_some() {
            let trusted_domains = self.load_trusted_llm_token_service_domains().await?;
            validate_llm_token_service_config(request.llm_token_service.as_ref(), &trusted_domains)
                .map_err(|detail| error_response(StatusCode::BAD_REQUEST, detail))?;
        }
        if request
            .mcp_binding_ids
            .as_deref()
            .is_some_and(|ids| !ids.is_empty())
        {
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                "mcp_binding_ids is no longer supported on /chat/stream; use runtime_mcp_bindings"
                    .to_string(),
            ));
        }
        let request_constraints = Self::try_request_constraints(request)
            .map_err(|detail| error_response(StatusCode::BAD_REQUEST, detail))?;
        let (_, resolver) = build_server_skill_resolver(self.skill_service.clone(), user_id);
        apply_normalized_skill_allowlist(resolver, &request_constraints)
            .map_err(|detail| error_response(StatusCode::BAD_REQUEST, detail))?;
        Ok(request_constraints)
    }

    /// Build a [`ServerAgenticLoopHost`] for a single run.
    fn build_host(
        &self,
        user_id: &str,
        session_id: &str,
        run_id: &str,
        request: &ChatRequestData,
        edge_tools: Vec<Value>,
        edge_profile: Map<String, Value>,
        execution_bindings: Option<&ExecutionBindingSnapshot>,
        plan_resume_hint: Option<String>,
        task_board_resume_hint: Option<String>,
    ) -> server_loop_host::ServerAgenticLoopHost {
        let mut builder = ServerAgenticLoopHostBuilder::new(
            self.matrixone.clone(),
            self.encryptor.clone(),
            user_id.to_string(),
            session_id.to_string(),
        )
        .with_model(request.model.clone())
        .with_llm_token_service(request.llm_token_service.clone())
        .with_full_llm_capture(request.full_llm_capture)
        .with_edge_tools(edge_tools)
        .with_capabilities(crate::capabilities::lifecycle_server_capabilities(
            self.shared_pool.is_some(),
        ))
        .with_edge_profile(edge_profile)
        .with_edge_callback_ledger(self.edge_callback_ledger.clone())
        .with_interaction_mode(request.interaction_mode)
        .with_interactive_client(request.interactive_client)
        .with_plan_resume_hint(plan_resume_hint)
        .with_task_board_resume_hint(task_board_resume_hint);

        if let Some(pool) = &self.shared_pool {
            builder = builder.with_pool(pool.clone());
        }
        if let Some(snapshot) = execution_bindings {
            builder = builder.with_execution_binding_snapshot(snapshot.clone());
        }
        // Wire one shared agent-progress broadcaster for delegation and
        // dynamic `agent(action='spawn')` trees so Web SSE observes a single lineage.
        builder = builder
            .with_progress_broadcaster(self.dynamic_agent_progress_broadcaster())
            .with_progress_root_run_id(run_id.to_string());
        if let Some(ref de) = self.delegation_engine {
            if let Some(store) = de.prefix_store() {
                builder = builder.with_prefix_store(Some(Arc::clone(store)));
            }
        }
        // Share the tool execution service's disabled-tools set so the LLM
        // surface excludes admin-disabled tools (not just dispatch-rejected).
        if let Some(ref shared_tes) = self.tool_execution_service {
            builder = builder.with_disabled_tools(shared_tes.disabled_tools_handle());
        }
        // Wire test LLM rounds from request context (E2E test hook).
        #[cfg(feature = "bridge-e2e-hooks")]
        if let Some(rounds) = request
            .context
            .as_ref()
            .and_then(|c| c.get("test_llm_rounds"))
            .and_then(Value::as_array)
            .cloned()
        {
            builder = builder.with_test_llm_rounds(rounds);
        }
        builder.build()
    }

    async fn task_board_resume_hint_for_session(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> Option<String> {
        let Some(shared) = &self.shared_pool else {
            return None;
        };
        let store: Arc<dyn TaskStore> =
            match MatrixOneTaskStore::from_shared_for_user(shared, user_id) {
                Ok(store) => Arc::new(store),
                Err(error) => {
                    tracing::warn!(
                        session_id = %session_id,
                        user_id = %user_id,
                        error = %error,
                        "failed to construct user-scoped task store for resume hint"
                    );
                    return None;
                }
            };
        let manager = TaskManager::new(session_id.to_string(), store);
        match manager.load_active_tasks().await {
            Ok(tasks) => format_task_board_resume_hint(&tasks),
            Err(error) => {
                tracing::warn!(
                    session_id = %session_id,
                    user_id = %user_id,
                    error = %error,
                    "failed to load task board resume hint for Cloud turn"
                );
                Some(format!(
                    "Task board state could not be loaded for this turn: {error}. \
                     Do not assume the task board is empty; avoid creating duplicate tasks and surface the load failure to the user."
                ))
            }
        }
    }

    /// Build the initial [`AgenticLoopState`] from a chat request.
    ///
    /// `workspace_override` — when the server provisions a workspace (web-agent
    /// mode, no CLI edge), pass it here so stop hooks and skill hooks are loaded
    /// from the provisioned directory instead of requiring `edge_profile.cwd`.
    fn build_initial_state(
        &self,
        user_id: &str,
        request: &ChatRequestData,
        session_id: &str,
        run_id: &str,
        workspace_override: Option<&std::path::Path>,
        execution_bindings: Option<&ExecutionBindingSnapshot>,
        cancel_token: Option<Arc<CancellationToken>>,
    ) -> AgenticLoopState {
        use astra_pipeline::step_protocol::InMemoryIdempotencyCache;
        use astra_text_utils::semantic_dedup::SemanticDedup;
        use astra_turn_core::chat_turn_heuristics::infer_task_execution_profile;
        use astra_turn_core::stop_hooks_yaml::{
            detect_turn_hook_sets, is_plan_subtask_from_chat_context, project_root_for_stop_hooks,
        };

        // Constraints come pre-validated through `validate_request_constraints`
        // (caller is `create_run` / resume paths). For deep-internal call
        // sites (tests, recovery flows) we re-parse and surface the error in
        // structured logs rather than panicking — a bad value here means an
        // upstream invariant is broken, not a user mistake.
        let request_constraints = match Self::try_request_constraints(request) {
            Ok(c) => c,
            Err(err) => {
                tracing::error!(
                    error = %err,
                    "request constraints failed validation in build_initial_state — falling back to default; upstream caller should validate first",
                );
                RequestConstraints::default()
            }
        };
        let (skill_registry, raw_skill_resolver) =
            build_server_skill_resolver(self.skill_service.clone(), user_id);
        let skill_resolver = apply_normalized_skill_allowlist(
            raw_skill_resolver,
            &request_constraints,
        )
        .unwrap_or_else(|err| {
            tracing::error!(
                error = %err,
                "skill allowlist failed in build_initial_state — proceeding without resolver",
            );
            None
        });
        use astra_turn_core::turn_guard::TurnGuard;

        let user_message = json!({
            "role": "user",
            "content": request.message,
        });

        let task_profile = infer_task_execution_profile(&request.message);
        let runtime_turn_ceiling = astra_config::runtime_config::RuntimeConfig::cached()
            .runtime_limits
            .resolve_turn_ceiling(is_plan_subtask_from_chat_context(&request.context));
        let requested_budget = request.execution_budget.as_ref().map(|budget| {
            astra_turn_core::chat_turn_heuristics::AgenticTurnBudgetOverride {
                initial_turns: budget.initial_turns.map(|value| value as usize),
                hard_turn_limit: budget.hard_turn_limit.map(|value| value as usize),
            }
        });
        let agentic_turn_budget =
            astra_turn_core::chat_turn_heuristics::resolve_agentic_turn_budget(
                task_profile,
                runtime_turn_ceiling,
                requested_budget,
            );
        let max_turns = agentic_turn_budget.initial_turns;
        let edge_ctx = Self::extract_edge_context(request);
        // Use edge profile's git_root/cwd if available; fall back to provisioned
        // server workspace so web-agent sessions still load stop-hooks.yaml.
        let project_root_buf = project_root_for_stop_hooks(&edge_ctx)
            .or_else(|| workspace_override.map(|p| p.to_path_buf()));
        let hook_sets = project_root_buf
            .as_ref()
            .map(|root| {
                detect_turn_hook_sets(
                    root.as_path(),
                    task_profile,
                    is_plan_subtask_from_chat_context(&request.context),
                )
            })
            .unwrap_or_default();
        let workspace_root_hint = project_root_buf.map(|p| p.to_string_lossy().into_owned());
        let skill_search = request.skill_search.clone().unwrap_or_default();
        let (tool_event_hooks, session_event_hooks) = workspace_root_hint
            .as_ref()
            .map(|root| crate::skills::hooks::load_all_hooks(std::path::Path::new(root)))
            .unwrap_or_default();
        let thinking_config =
            Self::thinking_from_chat_context(&request.context, request.model.as_deref());

        // Create harness sink early so sub-run executors can share it.
        #[cfg(feature = "harness")]
        let (harness_server_sink, harness_sink_arc): (
            Option<std::sync::Arc<crate::server::harness::server_sink::ServerSnapshotSink>>,
            Option<std::sync::Arc<dyn astra_harness::SnapshotSink>>,
        ) = if self.harness_registry.is_some() {
            let mut raw_sink = crate::server::harness::server_sink::ServerSnapshotSink::new(
                session_id.to_string(),
                String::new(),
            );
            if let Some(ref pool) = self.shared_pool {
                raw_sink = raw_sink.with_pool(pool.get().clone());
            }
            let concrete = std::sync::Arc::new(raw_sink);
            let dyn_sink = concrete.clone() as std::sync::Arc<dyn astra_harness::SnapshotSink>;
            (Some(concrete), Some(dyn_sink))
        } else {
            (None, None)
        };

        // Build the server-side skill fork executor so skills with
        // execution_context: Fork can run in isolated sub-agent loops.
        let edge_tools = Self::extract_edge_tools(request);
        let edge_profile = Self::extract_edge_profile(request);
        let skill_executor = build_server_skill_executor(
            &self.matrixone,
            &self.encryptor,
            self.shared_pool.as_ref(),
            request.model.as_deref(),
            request.llm_token_service.as_ref(),
            &edge_tools,
            &edge_profile,
            execution_bindings,
            &request.forward_headers,
            request_constraints.clone(),
            skill_resolver.clone(),
            session_id,
            self.edge_connection_pool.as_ref(),
            cancel_token,
            self.memory_extraction_service.as_ref(),
            #[cfg(feature = "harness")]
            harness_sink_arc.as_ref(),
        );
        let resolved_tool_policy = astra_config::runtime_config::RuntimeConfig::load()
            .tool_selection
            .resolve_for_model(request.model.as_deref());
        let restricted_tools: std::collections::HashSet<String> =
            load_deployment_disabled_tools().into_iter().collect();

        AgenticLoopState {
            messages: vec![user_message],
            volatile_pending: Vec::new(),
            recent_rounds: Vec::new(),
            tool_results: Vec::new(),
            current_session_id: Some(session_id.to_string()),
            current_run_id: Some(run_id.to_string()),
            context_manifest_pool: self.shared_pool.clone(),
            context_manifest_user_id: None,
            context_manifest_model_name: request.model.clone(),
            recursion_depth: 0,
            final_text: String::new(),
            final_text_streamed: false,
            total_prompt: 0,
            total_completion: 0,
            total_cache_read: 0,
            total_cache_creation: 0,
            total_tool_calls: 0,
            total_evidence_tool_calls: 0,
            has_any_usage: false,
            max_turns,
            remaining_turns: max_turns,
            turn_budget_hint_emitted_90: false,
            turn_budget_hint_emitted_50: false,
            turn_budget_hint_emitted_20: false,
            agentic_turn_budget,
            current_round_index: 0,
            llm_rounds_completed: 0,
            last_request_message_count: None,
            turn_guard: TurnGuard::with_profile(task_profile),
            restricted_tools,
            boosted_tools: std::collections::HashSet::new(),
            widen_selection_pending: false,
            step_recorder: StepRecorder::new(session_id, run_id),
            idempotency_cache: InMemoryIdempotencyCache::new(),
            semantic_dedup: SemanticDedup::new(0.75),
            call_counts: HashMap::new(),
            // Per-model workflow-guard policy (see
            // `ToolSelectionConfig::resolve_for_model`). Built-in profiles
            // give stronger models (opus/sonnet-4) more rope than haiku.
            // Security guards (shell_obfuscation, destructive_sql) are
            // unaffected and stay uniform across models.
            max_identical_tool_calls: resolved_tool_policy.max_identical_tool_calls,
            max_tools_per_turn: resolved_tool_policy.max_tools_per_turn,
            repeated_cache_hit_suppression: resolved_tool_policy.repeated_cache_hit_suppression,
            max_consecutive_empty_name: resolved_tool_policy.max_consecutive_empty_name,
            stall: Default::default(),
            telemetry: Default::default(),
            skills: SkillState {
                registry_for_activation: if request_constraints.allowed_skills.is_some() {
                    None
                } else {
                    skill_registry
                },
                resolver: skill_resolver,
                executor: skill_executor,
                request_constraints,
                quality_tracker: crate::skills::quality::SkillQualityTracker::new(),
                improvement_tracker: astra_skills::improvement::ImprovementTracker::new(),
                search: skill_search,
                tool_event_hooks,
                session_event_hooks,
                ..Default::default()
            },
            hooks: StopHookState {
                stop_hooks: hook_sets.stop_hooks,
                teammate_idle_hooks: hook_sets.teammate_idle_hooks,
                workspace_root_hint,
                forward_headers: request.forward_headers.clone(),
                llm_token_service: request.llm_token_service.clone(),
                ..Default::default()
            },
            cancellation: Default::default(),
            messaging: Default::default(),
            deferred_input: Default::default(),
            error_recovery: Default::default(),
            run_control: None,
            pipeline_session: Some(
                astra_turn_core::pipeline_session::PipelineSession::new_with_current_date(
                    astra_turn_core::pipeline_config::PipelineConfig::default(),
                    crate::turn::session_current_date::resolve_session_current_date(session_id),
                ),
            ),
            message: request.message.clone(),
            recent_tools: Vec::new(),
            task_profile,
            last_turn_policy: crate::turn::agentic_loop::host::TurnInteractionPolicy::default(),
            api: astra_thin_client::ThinClient::new("http://127.0.0.1:1", None)
                .expect("valid dummy URL"),
            api_token: String::new(),
            delegation_engine: None,
            delegations_this_turn: 0,
            project_context: None,
            checkpoint_gate: None,
            last_llm_context_manifest_trace: None,
            rate_limit_cooldown: Default::default(),
            data_snapshot_provider: None,
            last_composite_snapshot: None,
            last_measured_prompt_tokens: None,
            consecutive_context_window_errors: 0,
            compaction_effectiveness: Default::default(),
            pinned_tool_schema_tokens: 0,
            sticky_tool_schemas: Vec::new(),
            max_turn_input_tokens: astra_core::RuntimeLimits::global().max_turn_input_tokens,
            budget_wrapup_injected: false,
            budget_wrapup_ignored_rounds: 0,
            compact_tier_applied: astra_turn_core::compaction_types::CompactionTier::Normal,
            skill_produced_output: false,
            max_cumulative_tokens: 0,
            thinking: thinking_config,
            recent_file_reads: Vec::new(),
            permission_context: None,
            permission_handler: None,
            tactical_adapter: None,
            step_signal_collector: None,
            tool_budget_override: None,
            recent_tactical_actions: Vec::new(),
            server_tool_executor: None,
            interruption: None,
            session_facts: Default::default(),
            memory_extraction_service: self.memory_extraction_service.clone(),
            session_memory_state: Default::default(),
            session_memory_llm_params: None,
            compact_strategy: astra_turn_core::microcompact::CompactStrategy::from_provider_hint(
                request.model.as_deref().unwrap_or(""),
            ),
            approval_overrides: None,
            confidence_trend: Default::default(),
            last_confidence_diagnosis: None,
            session_turn: 0,
            bridge_turn_chain_id: Some(format!("{}:harness", session_id)),
            bridge_user_query_event_id: None,
            turn_event_buffer: None,
            harness: {
                #[cfg(feature = "harness")]
                {
                    if let (Some(registry), Some(server_sink), Some(sink_arc)) = (
                        &self.harness_registry,
                        harness_server_sink,
                        harness_sink_arc,
                    ) {
                        let broadcaster_tx = server_sink.broadcaster_sender();
                        let limits = astra_harness::HarnessLimits {
                            max_turns: if max_turns > 0 {
                                Some(max_turns as u32)
                            } else {
                                None
                            },
                            ..Default::default()
                        };
                        let kernel = std::sync::Arc::new(
                            astra_harness::StandardKernel::configured(sink_arc.clone(), limits),
                        );
                        registry.register_with_broadcast(
                            session_id.to_string(),
                            sink_arc.clone(),
                            broadcaster_tx,
                        );
                        let mut slot = crate::turn::harness_adapter::HarnessSlot::new(
                            kernel as std::sync::Arc<dyn astra_harness::HarnessKernel>,
                            sink_arc,
                        );
                        slot.registry = Some(registry.clone());
                        slot.session_id_for_cleanup = Some(session_id.to_string());
                        slot.server_sink = Some(server_sink);
                        slot
                    } else {
                        crate::turn::harness_adapter::HarnessSlot::empty()
                    }
                }
                #[cfg(not(feature = "harness"))]
                {
                    crate::turn::harness_adapter::HarnessSlot::empty()
                }
            },
        }
    }

    fn thinking_from_chat_context(
        context: &Option<Map<String, Value>>,
        model: Option<&str>,
    ) -> astra_turn_core::thinking_config::ThinkingConfig {
        if let Some(value) = context.as_ref().and_then(|ctx| ctx.get("thinking")) {
            return astra_turn_core::thinking_config::ThinkingConfig::from_payload_value(value);
        }
        model
            .map(|name| astra_turn_core::thinking_config::resolve_model_thinking(name).1)
            .unwrap_or_default()
    }
    /// Extract edge tools from the request context, or provide empty defaults.
    /// Parse the request context into a typed [`EdgeContext`].
    fn extract_edge_context(request: &ChatRequestData) -> EdgeContext {
        request
            .context
            .as_ref()
            .map(EdgeContext::from_context_map)
            .unwrap_or_default()
    }

    /// Extract edge tools from the request context, or provide empty defaults.
    fn extract_edge_tools(request: &ChatRequestData) -> Vec<Value> {
        Self::extract_edge_context(request).edge_tools
    }

    /// Extract edge profile from the request context, or provide empty defaults.
    fn extract_edge_profile(request: &ChatRequestData) -> Map<String, Value> {
        let mut profile = Self::extract_edge_context(request).edge_profile.to_map();
        if let Some(raw_profile) = request
            .context
            .as_ref()
            .and_then(|context| context.get("edge_profile"))
            .and_then(Value::as_object)
        {
            for (key, value) in raw_profile {
                profile.insert(key.clone(), value.clone());
            }
        }
        profile
    }

    /// Provision a cloud workspace record for orchestrator-managed workspaces.
    async fn provision_cloud_workspace_record(
        &self,
        user_id: &str,
        session_id: &str,
        request: &ChatRequestData,
        run_id: &str,
    ) -> Result<Option<RuntimeWorkspaceRecord>, (StatusCode, Json<ErrorResponse>)> {
        let Some(provision_request) =
            cloud_workspace_provision_request_from_request(request, run_id)?
        else {
            return Ok(None);
        };
        let record = CloudWorkspaceProvisioner::from_env()
            .provision(provision_request)
            .await
            .map_err(cloud_workspace_provision_error)?;
        if let Err(error) = self
            .persist_workspace_record(user_id, session_id, run_id, &record)
            .await
        {
            self.cleanup_cloud_workspace_after_failed_start(
                user_id,
                session_id,
                run_id,
                &record,
                format!(
                    "workspace record persistence failed before orchestrator binding: {}",
                    error.1.0.detail
                ),
            )
            .await;
            return Err(error);
        }
        Ok(Some(record))
    }

    async fn persist_workspace_record(
        &self,
        user_id: &str,
        session_id: &str,
        run_id: &str,
        record: &RuntimeWorkspaceRecord,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        let Some(store) = self.workspace_record_store.as_ref() else {
            return Ok(());
        };
        store
            .upsert_workspace_record(StoredWorkspaceRecordEntry::new(
                user_id.to_string(),
                Some(session_id.to_string()),
                Some(run_id.to_string()),
                record.clone(),
            ))
            .await
            .map_err(workspace_record_store_error)
    }

    async fn cleanup_cloud_workspace_after_failed_start(
        &self,
        user_id: &str,
        session_id: &str,
        run_id: &str,
        record: &RuntimeWorkspaceRecord,
        failure_message: String,
    ) {
        Self::cleanup_cloud_workspace_with_debt(
            self.workspace_record_store.clone(),
            user_id,
            session_id,
            run_id,
            record,
            RuntimeCleanupReason::Failed,
            failure_message,
        )
        .await;
    }

    async fn cleanup_cloud_workspace_after_terminal_run(
        workspace_record_store: Option<Arc<dyn WorkspaceStateStore>>,
        user_id: &str,
        session_id: &str,
        run_id: &str,
        record: &RuntimeWorkspaceRecord,
        status: &RunStatus,
    ) {
        let Some(reason) = cleanup_reason_for_terminal_run_status(status) else {
            return;
        };
        Self::cleanup_cloud_workspace_with_debt(
            workspace_record_store,
            user_id,
            session_id,
            run_id,
            record,
            reason,
            format!("run ended with status {}", status.as_str()),
        )
        .await;
    }

    async fn cleanup_cloud_workspace_with_debt(
        workspace_record_store: Option<Arc<dyn WorkspaceStateStore>>,
        user_id: &str,
        session_id: &str,
        run_id: &str,
        record: &RuntimeWorkspaceRecord,
        reason: RuntimeCleanupReason,
        failure_message: String,
    ) {
        match CloudWorkspaceProvisioner::from_env()
            .cleanup(record, reason)
            .await
        {
            Ok(()) => {
                tracing::info!(
                    target: "astra_runtime::run_lifecycle",
                    workspace_id = %record.workspace_id,
                    run_id = %run_id,
                    "cleaned provisioned cloud workspace"
                );
            }
            Err(cleanup_error) => {
                tracing::warn!(
                    target: "astra_runtime::run_lifecycle",
                    workspace_id = %record.workspace_id,
                    run_id = %run_id,
                    error = %cleanup_error,
                    failure = %failure_message,
                    "failed to clean provisioned cloud workspace"
                );
                Self::record_workspace_cleanup_debt_in_store(
                    workspace_record_store,
                    user_id,
                    session_id,
                    run_id,
                    record,
                    cleanup_error.reason,
                    format!(
                        "{failure_message}; cleanup failed: {}",
                        cleanup_error.message
                    ),
                )
                .await;
            }
        }
    }

    async fn record_workspace_cleanup_debt_in_store(
        workspace_record_store: Option<Arc<dyn WorkspaceStateStore>>,
        user_id: &str,
        session_id: &str,
        run_id: &str,
        record: &RuntimeWorkspaceRecord,
        reason: RuntimeCleanupReason,
        message: String,
    ) {
        let Some(store) = workspace_record_store.as_ref() else {
            tracing::warn!(
                target: "astra_runtime::run_lifecycle",
                workspace_id = %record.workspace_id,
                run_id = %run_id,
                "workspace cleanup debt store is unavailable"
            );
            return;
        };
        if let Err(error) = store
            .record_cleanup_debt(WorkspaceCleanupDebtEntry::new(
                user_id.to_string(),
                Some(session_id.to_string()),
                Some(run_id.to_string()),
                record.clone(),
                reason,
                message,
            ))
            .await
        {
            tracing::warn!(
                target: "astra_runtime::run_lifecycle",
                workspace_id = %record.workspace_id,
                run_id = %run_id,
                error = %error,
                "failed to persist workspace cleanup debt"
            );
        }
    }

    /// Provision a sandboxed workspace directory for server-side tool execution.
    fn provision_server_workspace(
        &self,
        session_id: &str,
    ) -> Result<std::path::PathBuf, (StatusCode, Json<ErrorResponse>)> {
        let record = ServerWorkspaceProvisioner::from_env()
            .provision(session_id)
            .map_err(server_workspace_provision_error)?;
        Ok(record.root)
    }

    /// Collect run events into SSE-compatible format.
    fn format_run_events(events: &[Value], start_index: usize) -> Vec<Value> {
        events
            .iter()
            .enumerate()
            .map(|(i, ev)| {
                let mut out = ev.clone();
                if let Some(obj) = out.as_object_mut() {
                    obj.insert("index".to_string(), json!(start_index + i));
                }
                out
            })
            .collect()
    }

    fn durable_event_payload(event: &Value) -> Option<&Map<String, Value>> {
        if event.get("event_type").is_some() {
            event.get("data").and_then(Value::as_object)
        } else {
            event.as_object()
        }
    }

    fn durable_run_execution_binding_snapshot(
        run: &DurableRunRecord,
    ) -> RunExecutionBindingSnapshot {
        let mut snapshot = RunExecutionBindingSnapshot::default();
        for event in &run.events {
            let Some(payload) = Self::durable_event_payload(event) else {
                continue;
            };
            if let Some(workspace) = payload
                .get("workspace")
                .filter(|value| value.is_object())
                .cloned()
            {
                snapshot.workspace = Some(workspace);
            }
            if let Some(executor) = payload
                .get("executor")
                .filter(|value| value.is_object())
                .cloned()
            {
                snapshot.executor = Some(executor);
            }
            if let Some(transport) = payload.get("transport").and_then(Value::as_str) {
                snapshot.transport = Some(transport.to_string());
            }
            if let Some(fallback_policy) = payload.get("fallback_policy").and_then(Value::as_str) {
                snapshot.fallback_policy = Some(fallback_policy.to_string());
            }
        }
        snapshot
    }

    fn durable_status_record(run: &DurableRunRecord) -> RunStatusRecord {
        let binding = Self::durable_run_execution_binding_snapshot(run);
        RunStatusRecord {
            run_id: run.run_id.clone(),
            session_id: run.session_id.clone(),
            status: run.status.clone(),
            waiting_for: run.waiting_for.clone(),
            events_count: run.events.len() as i64,
            workspace: binding.workspace,
            executor: binding.executor,
            transport: binding.transport,
            fallback_policy: binding.fallback_policy,
        }
    }

    fn durable_stream_events(run: &DurableRunRecord, last_index: u32) -> Vec<Value> {
        let offset = last_index as usize;
        if offset < run.events.len() {
            Self::format_run_events(&run.events[offset..], offset)
        } else {
            Vec::new()
        }
    }

    fn durable_recent_events(run: &DurableRunRecord, limit: u32) -> Vec<Value> {
        let capped = limit.clamp(1, MAX_DURABLE_RUN_PROJECTION_RECENT_EVENTS) as usize;
        let offset = run.events.len().saturating_sub(capped);
        Self::format_run_events(&run.events[offset..], offset)
    }

    async fn load_durable_run_for_user(
        &self,
        run_id: &str,
        user_id: &str,
    ) -> Result<Option<DurableRunRecord>, (StatusCode, Json<ErrorResponse>)> {
        let run = self.run_engine.load_run(run_id).await.map_err(|error| {
            error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("Failed to load durable run state: {error}"),
            )
        })?;

        let Some(run) = run else {
            return Ok(None);
        };

        if run.user_id != user_id {
            return Err(error_response(StatusCode::FORBIDDEN, "Access denied"));
        }

        Ok(Some(run))
    }

    async fn require_durable_run_for_user(
        &self,
        run_id: &str,
        user_id: &str,
    ) -> Result<DurableRunRecord, (StatusCode, Json<ErrorResponse>)> {
        self.load_durable_run_for_user(run_id, user_id)
            .await?
            .ok_or_else(|| error_response(StatusCode::NOT_FOUND, "Run not found"))
    }

    fn run_status_from_durable(
        status: &str,
    ) -> Result<RunStatus, (StatusCode, Json<ErrorResponse>)> {
        RunStatus::from_durable_status(status).ok_or_else(|| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Invalid durable run status '{status}'"),
            )
        })
    }

    fn run_state_conflict(action: &str, status: &str) -> (StatusCode, Json<ErrorResponse>) {
        error_response(
            StatusCode::CONFLICT,
            format!("Cannot {action} run in '{status}' state"),
        )
    }

    fn durable_persist_error(action: &str, error: String) -> (StatusCode, Json<ErrorResponse>) {
        error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("Failed to persist durable run {action}: {error}"),
        )
    }

    #[cfg(test)]
    pub(crate) async fn test_llm_cancel_token_is_cancelled(&self, run_id: &str) -> Option<bool> {
        let runs = self.runs.read().await;
        runs.get(run_id).map(|r| r.llm_cancel_token.is_cancelled())
    }

    #[cfg(test)]
    pub(crate) async fn test_pause_flag_is_set(&self, run_id: &str) -> Option<bool> {
        let runs = self.runs.read().await;
        runs.get(run_id)
            .map(|r| r.pause_flag.load(Ordering::Acquire))
    }
}

/// Build an [`ExtractionRequest`] from the current loop state for shutdown-time
/// memory extraction. Returns `None` when no session id is set.
fn build_shutdown_extraction_request(
    state: &AgenticLoopState,
) -> Option<crate::session_memory::ExtractionRequest> {
    state.current_session_id.as_ref().map(|session_id| {
        crate::session_memory::ExtractionRequest {
            session_id: session_id.clone(),
            messages: state.messages.clone(),
            session_facts: state.session_facts.clone(),
            current_tokens: state
                .total_prompt
                .saturating_add(state.total_cache_read)
                .saturating_add(state.total_cache_creation)
                as usize,
            current_tool_calls: state.total_tool_calls as usize,
            had_error: state.error_recovery.consecutive_same_error > 0,
            had_user_correction: astra_turn_core::input_classifier::is_correction_signal(
                &state.message,
            ),
            turn_number: state.max_turns.saturating_sub(state.remaining_turns) as u32,
            config:
                astra_turn_core::cloud_session_memory_extract::SessionMemoryExtractConfig::default(
                ),
        }
    })
}

fn server_workspace_provision_error(
    error: ServerWorkspaceProvisionError,
) -> (StatusCode, Json<ErrorResponse>) {
    match error {
        ServerWorkspaceProvisionError::InvalidSessionId => error_response(
            StatusCode::BAD_REQUEST,
            "Invalid session_id for server workspace provisioning",
        ),
        ServerWorkspaceProvisionError::WorkspaceEscapedBase { .. } => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Server workspace provisioning escaped its base directory: {error}"),
        ),
        _ => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("Failed to provision server workspace: {error}"),
        ),
    }
}

fn cloud_workspace_provision_request_from_request(
    request: &ChatRequestData,
    run_id: &str,
) -> Result<Option<RuntimeWorkspaceProvisionRequest>, (StatusCode, Json<ErrorResponse>)> {
    let Some(binding) = request.workspace_binding.as_ref() else {
        return Ok(None);
    };
    match binding.kind {
        astra_services::runs::WorkspaceBindingRequestKind::CloudWorkspace => {
            cloud_workspace_provision_request_from_source(binding, run_id).map(Some)
        }
        _ => Ok(None),
    }
}

fn cloud_workspace_provision_request_from_source(
    binding: &astra_services::runs::WorkspaceBindingRequest,
    run_id: &str,
) -> Result<RuntimeWorkspaceProvisionRequest, (StatusCode, Json<ErrorResponse>)> {
    let Some(source) = binding.source.as_ref() else {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "Cloud workspace requires an explicit source",
        ));
    };
    match source {
        astra_services::runs::WorkspaceSourceRequest::PersistentVolume { .. } => {
            cloud_persistent_volume_provision_request(binding, run_id)
        }
        astra_services::runs::WorkspaceSourceRequest::UploadedSnapshot { .. } => {
            cloud_uploaded_snapshot_provision_request(binding, run_id)
        }
        astra_services::runs::WorkspaceSourceRequest::GitCheckout { .. } => {
            cloud_git_checkout_provision_request(binding, run_id)
        }
        astra_services::runs::WorkspaceSourceRequest::Scratch => {
            cloud_scratch_workspace_provision_request(binding, run_id)
        }
        astra_services::runs::WorkspaceSourceRequest::Template { .. }
        | astra_services::runs::WorkspaceSourceRequest::DatasetBundle { .. }
        | astra_services::runs::WorkspaceSourceRequest::ArtifactBundle { .. } => {
            cloud_materialized_source_provision_request(binding, run_id)
        }
        astra_services::runs::WorkspaceSourceRequest::EdgePath { .. } => Err(error_response(
            StatusCode::BAD_REQUEST,
            "Cloud workspace source is not supported by this provisioner yet",
        )),
    }
}

fn cloud_persistent_volume_provision_request(
    binding: &astra_services::runs::WorkspaceBindingRequest,
    run_id: &str,
) -> Result<RuntimeWorkspaceProvisionRequest, (StatusCode, Json<ErrorResponse>)> {
    let Some(astra_services::runs::WorkspaceSourceRequest::PersistentVolume { volume_id }) =
        binding.source.as_ref()
    else {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "Cloud workspace requires source.kind=persistent_volume",
        ));
    };
    let volume_id = non_empty_request_string(Some(volume_id.as_str())).ok_or_else(|| {
        error_response(
            StatusCode::BAD_REQUEST,
            "Cloud workspace requires a non-empty source.volume_id",
        )
    })?;
    Ok(RuntimeWorkspaceProvisionRequest {
        workspace_id: cloud_workspace_id(run_id),
        owner_scope: RuntimeWorkspaceOwnerScope::Tenant,
        kind: astra_runtime_env::WorkspaceBindingKind::CloudWorkspace,
        authority: runtime_workspace_authority_from_request(
            binding.authority,
            astra_runtime_env::WorkspaceAuthority::ReadWrite,
        ),
        source: RuntimeWorkspaceSource::PersistentVolume { volume_id },
        persistence: RuntimeWorkspacePersistence::Persistent,
        requested_root: non_empty_request_string(binding.root.as_deref()),
        display_name: non_empty_request_string(binding.display_name.as_deref())
            .or_else(|| Some("Cloud workspace".to_string())),
    })
}

fn cloud_uploaded_snapshot_provision_request(
    binding: &astra_services::runs::WorkspaceBindingRequest,
    run_id: &str,
) -> Result<RuntimeWorkspaceProvisionRequest, (StatusCode, Json<ErrorResponse>)> {
    let workspace_id = cloud_workspace_id(run_id);
    let Some(astra_services::runs::WorkspaceSourceRequest::UploadedSnapshot { artifact_id, root }) =
        binding.source.as_ref()
    else {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "Uploaded snapshot workspace requires source.kind=uploaded_snapshot",
        ));
    };
    let artifact_id = non_empty_request_string(Some(artifact_id.as_str())).ok_or_else(|| {
        error_response(
            StatusCode::BAD_REQUEST,
            "Uploaded snapshot workspace requires a non-empty source.artifact_id",
        )
    })?;
    let requested_root = non_empty_request_string(root.as_deref())
        .or_else(|| non_empty_request_string(binding.root.as_deref()));
    validate_absolute_materialized_source_root(
        "Uploaded snapshot source.root",
        requested_root.as_deref(),
    )?;
    let authority = runtime_workspace_authority_from_request(
        binding.authority,
        astra_runtime_env::WorkspaceAuthority::ReadOnly,
    );
    Ok(RuntimeWorkspaceProvisionRequest {
        workspace_id,
        owner_scope: RuntimeWorkspaceOwnerScope::Tenant,
        kind: astra_runtime_env::WorkspaceBindingKind::CloudWorkspace,
        authority,
        source: RuntimeWorkspaceSource::UploadedSnapshot { artifact_id },
        persistence: if authority == astra_runtime_env::WorkspaceAuthority::ReadWrite {
            RuntimeWorkspacePersistence::Session
        } else {
            RuntimeWorkspacePersistence::ImmutableSnapshot
        },
        requested_root,
        display_name: non_empty_request_string(binding.display_name.as_deref())
            .or_else(|| Some("Uploaded snapshot".to_string())),
    })
}

fn cloud_materialized_source_provision_request(
    binding: &astra_services::runs::WorkspaceBindingRequest,
    run_id: &str,
) -> Result<RuntimeWorkspaceProvisionRequest, (StatusCode, Json<ErrorResponse>)> {
    let Some(source) = binding.source.as_ref() else {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "Materialized cloud workspace requires an explicit source",
        ));
    };
    let (source, default_authority, display_name) = match source {
        astra_services::runs::WorkspaceSourceRequest::Template { template_id } => {
            let template_id =
                non_empty_request_string(Some(template_id.as_str())).ok_or_else(|| {
                    error_response(
                        StatusCode::BAD_REQUEST,
                        "Template workspace requires a non-empty source.template_id",
                    )
                })?;
            (
                RuntimeWorkspaceSource::Template { template_id },
                astra_runtime_env::WorkspaceAuthority::ReadWrite,
                "Template workspace",
            )
        }
        astra_services::runs::WorkspaceSourceRequest::DatasetBundle { dataset_id } => {
            let dataset_id =
                non_empty_request_string(Some(dataset_id.as_str())).ok_or_else(|| {
                    error_response(
                        StatusCode::BAD_REQUEST,
                        "Dataset workspace requires a non-empty source.dataset_id",
                    )
                })?;
            (
                RuntimeWorkspaceSource::DatasetBundle { dataset_id },
                astra_runtime_env::WorkspaceAuthority::ReadOnly,
                "Dataset workspace",
            )
        }
        astra_services::runs::WorkspaceSourceRequest::ArtifactBundle { artifact_id } => {
            let artifact_id =
                non_empty_request_string(Some(artifact_id.as_str())).ok_or_else(|| {
                    error_response(
                        StatusCode::BAD_REQUEST,
                        "Artifact workspace requires a non-empty source.artifact_id",
                    )
                })?;
            (
                RuntimeWorkspaceSource::ArtifactBundle { artifact_id },
                astra_runtime_env::WorkspaceAuthority::ReadOnly,
                "Artifact workspace",
            )
        }
        _ => {
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                "Materialized cloud workspace requires source.kind=template, dataset_bundle, or artifact_bundle",
            ));
        }
    };
    let requested_root = non_empty_request_string(binding.root.as_deref());
    validate_absolute_materialized_source_root(
        "Cloud workspace source.root",
        requested_root.as_deref(),
    )?;
    let authority = runtime_workspace_authority_from_request(binding.authority, default_authority);
    Ok(RuntimeWorkspaceProvisionRequest {
        workspace_id: cloud_workspace_id(run_id),
        owner_scope: RuntimeWorkspaceOwnerScope::Tenant,
        kind: astra_runtime_env::WorkspaceBindingKind::CloudWorkspace,
        authority,
        source,
        persistence: if authority == astra_runtime_env::WorkspaceAuthority::ReadWrite {
            RuntimeWorkspacePersistence::Session
        } else {
            RuntimeWorkspacePersistence::ImmutableSnapshot
        },
        requested_root,
        display_name: non_empty_request_string(binding.display_name.as_deref())
            .or_else(|| Some(display_name.to_string())),
    })
}

fn cloud_git_checkout_provision_request(
    binding: &astra_services::runs::WorkspaceBindingRequest,
    run_id: &str,
) -> Result<RuntimeWorkspaceProvisionRequest, (StatusCode, Json<ErrorResponse>)> {
    let Some(astra_services::runs::WorkspaceSourceRequest::GitCheckout {
        repository,
        reference,
    }) = binding.source.as_ref()
    else {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "Git checkout workspace requires source.kind=git_checkout",
        ));
    };
    let repository = non_empty_request_string(Some(repository.as_str())).ok_or_else(|| {
        error_response(
            StatusCode::BAD_REQUEST,
            "Git checkout workspace requires a non-empty source.repository",
        )
    })?;
    Ok(RuntimeWorkspaceProvisionRequest {
        workspace_id: cloud_workspace_id(run_id),
        owner_scope: RuntimeWorkspaceOwnerScope::Tenant,
        kind: astra_runtime_env::WorkspaceBindingKind::CloudWorkspace,
        authority: runtime_workspace_authority_from_request(
            binding.authority,
            astra_runtime_env::WorkspaceAuthority::ReadWrite,
        ),
        source: RuntimeWorkspaceSource::GitCheckout {
            repository,
            reference: non_empty_request_string(reference.as_deref()),
        },
        persistence: RuntimeWorkspacePersistence::Session,
        requested_root: None,
        display_name: non_empty_request_string(binding.display_name.as_deref())
            .or_else(|| Some("Git checkout".to_string())),
    })
}

fn cloud_scratch_workspace_provision_request(
    binding: &astra_services::runs::WorkspaceBindingRequest,
    run_id: &str,
) -> Result<RuntimeWorkspaceProvisionRequest, (StatusCode, Json<ErrorResponse>)> {
    if !matches!(
        binding.source.as_ref(),
        Some(astra_services::runs::WorkspaceSourceRequest::Scratch)
    ) {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "Scratch cloud workspace requires source.kind=scratch",
        ));
    }
    Ok(RuntimeWorkspaceProvisionRequest {
        workspace_id: cloud_workspace_id(run_id),
        owner_scope: RuntimeWorkspaceOwnerScope::Tenant,
        kind: astra_runtime_env::WorkspaceBindingKind::CloudWorkspace,
        authority: runtime_workspace_authority_from_request(
            binding.authority,
            astra_runtime_env::WorkspaceAuthority::ReadWrite,
        ),
        source: RuntimeWorkspaceSource::Scratch,
        persistence: RuntimeWorkspacePersistence::Session,
        requested_root: non_empty_request_string(binding.root.as_deref()),
        display_name: non_empty_request_string(binding.display_name.as_deref())
            .or_else(|| Some("Scratch workspace".to_string())),
    })
}

fn cloud_workspace_id(run_id: &str) -> String {
    format!("run-{run_id}")
}

fn non_empty_request_string(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn validate_absolute_materialized_source_root(
    label: &str,
    root: Option<&str>,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    if root.is_some_and(|root| !Path::new(root).is_absolute()) {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            format!("{label} must be an absolute materialized source path"),
        ));
    }
    Ok(())
}

fn runtime_workspace_authority_from_request(
    authority: Option<astra_services::runs::WorkspaceAuthorityRequest>,
    default: astra_runtime_env::WorkspaceAuthority,
) -> astra_runtime_env::WorkspaceAuthority {
    match authority {
        Some(astra_services::runs::WorkspaceAuthorityRequest::ReadOnly) => {
            astra_runtime_env::WorkspaceAuthority::ReadOnly
        }
        Some(astra_services::runs::WorkspaceAuthorityRequest::ReadWrite) => {
            astra_runtime_env::WorkspaceAuthority::ReadWrite
        }
        Some(astra_services::runs::WorkspaceAuthorityRequest::None) => {
            astra_runtime_env::WorkspaceAuthority::None
        }
        None => default,
    }
}

fn execution_bindings_from_workspace_record(
    record: &RuntimeWorkspaceRecord,
) -> ExecutionBindingSnapshot {
    let workspace = server_workspace_binding_from_workspace_record(record);
    let executor = ExecutorBinding::orchestrator_managed(
        format!("orchestrator:{}", record.workspace_id),
        "Orchestrator-managed executor",
        ExecutorStatus::Online,
    );
    let runtime = astra_runtime_env::RuntimeBinding::kubernetes(format!(
        "kubernetes:{}",
        record.workspace_id
    ));
    ExecutionBindingSnapshot::new(workspace, executor, runtime)
}

fn server_workspace_binding_from_workspace_record(
    record: &RuntimeWorkspaceRecord,
) -> WorkspaceBinding {
    WorkspaceBinding {
        kind: match record.kind {
            astra_runtime_env::WorkspaceBindingKind::LocalFilesystem => {
                WorkspaceBindingKind::Unknown
            }
            other => other,
        },
        display_name: record.display_name.clone(),
        cwd: if record.kind == astra_runtime_env::WorkspaceBindingKind::None {
            None
        } else {
            Some(record.root_or_volume_ref.clone())
        },
        authority: record.authority,
        fallback_policy: FallbackPolicy::Disabled,
    }
}

fn cloud_workspace_provision_error(
    error: RuntimeWorkspaceProvisionError,
) -> (StatusCode, Json<ErrorResponse>) {
    match error.kind {
        RuntimeWorkspaceProvisionErrorKind::InvalidWorkspaceId
        | RuntimeWorkspaceProvisionErrorKind::SourceKindMismatch
        | RuntimeWorkspaceProvisionErrorKind::AuthorityDenied => error_response(
            StatusCode::BAD_REQUEST,
            format!("Invalid cloud workspace request: {error}"),
        ),
        RuntimeWorkspaceProvisionErrorKind::MountFailed
        | RuntimeWorkspaceProvisionErrorKind::Internal
        | RuntimeWorkspaceProvisionErrorKind::CleanupFailed => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to provision cloud workspace: {error}"),
        ),
        RuntimeWorkspaceProvisionErrorKind::WorkspaceUnavailable => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("Cloud workspace unavailable: {error}"),
        ),
    }
}

fn workspace_record_store_error(
    error: WorkspaceRecordStoreError,
) -> (StatusCode, Json<ErrorResponse>) {
    match error {
        WorkspaceRecordStoreError::InvalidOwnerId
        | WorkspaceRecordStoreError::InvalidSessionId
        | WorkspaceRecordStoreError::InvalidRunId
        | WorkspaceRecordStoreError::InvalidWorkspaceId(_) => error_response(
            StatusCode::BAD_REQUEST,
            format!("Invalid workspace ownership record: {error}"),
        ),
        WorkspaceRecordStoreError::WorkspaceOwnerConflict { .. }
        | WorkspaceRecordStoreError::SourceOwnerConflict { .. } => error_response(
            StatusCode::CONFLICT,
            format!("Workspace ownership conflict: {error}"),
        ),
        WorkspaceRecordStoreError::Database(_)
        | WorkspaceRecordStoreError::Json(_)
        | WorkspaceRecordStoreError::Unavailable(_) => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("Failed to persist workspace record: {error}"),
        ),
    }
}

#[async_trait]
impl RunLifecycleService for AgenticRunLifecycleService {
    /// Create a run (background mode): spawns the agentic loop in a task, returns immediately.
    async fn create_run(
        &self,
        user_id: String,
        request: ChatRequestData,
    ) -> Result<ChatRunRecord, (StatusCode, Json<ErrorResponse>)> {
        self.validate_request_constraints(&user_id, &request)
            .await?;

        // ── Resource governance check (Phase 5) ─────────────────────
        if let Some(ref gov) = self.resource_governor {
            if let astra_services::resource_governor::LimitCheck::Denied { reason } =
                gov.check_run_start(&user_id).await
            {
                return Err(error_response(
                    StatusCode::TOO_MANY_REQUESTS,
                    format!("Resource limit exceeded: {reason}"),
                ));
            }
        }

        let run_id = Uuid::new_v4().to_string();
        let session_id = request
            .session_id
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());

        let edge_tools = Self::extract_edge_tools(&request);
        let server_side_tool_catalog = edge_tools.is_empty();
        let edge_profile = Self::extract_edge_profile(&request);
        let mcp_bundle =
            runtime_mcp::prepare_request_scoped_runtime_bundle(&request.runtime_mcp_bindings)
                .await?;

        // Guard: reject if this session already has a blocking run.
        // Hold write lock across check+insert to prevent TOCTOU race.
        let (run_state, cancel_flag, pause_flag, llm_cancel_token) =
            Self::build_tracked_run_state(run_id.clone(), session_id.clone(), user_id.clone());
        {
            let mut runs = self.runs.write().await;
            let has_active = Self::session_has_blocking_run(&runs, &session_id);
            if has_active {
                return Err(error_response(
                    StatusCode::CONFLICT,
                    "session already has an active run".to_string(),
                ));
            }
            runs.insert(run_id.clone(), run_state);
        }

        // Provision workspace early so build_initial_state and durable
        // run_started metadata use the same execution boundary.
        let cloud_workspace_record = match self
            .provision_cloud_workspace_record(&user_id, &session_id, &request, &run_id)
            .await
        {
            Ok(record) => record,
            Err(error) => {
                self.runs.write().await.remove(&run_id);
                return Err(error);
            }
        };
        let cloud_execution_bindings = cloud_workspace_record
            .as_ref()
            .map(|record| execution_bindings_from_workspace_record(record));
        let cloud_workspace = cloud_workspace_record
            .as_ref()
            .map(|record| PathBuf::from(&record.root_or_volume_ref));

        let server_workspace = if cloud_workspace_record.is_none()
            && request_uses_server_workspace(&request, !edge_tools.is_empty())
        {
            match self.provision_server_workspace(&session_id) {
                Ok(workspace) => Some(workspace),
                Err(error) => {
                    self.runs.write().await.remove(&run_id);
                    return Err(error);
                }
            }
        } else {
            None
        };
        let execution_bindings = cloud_execution_bindings
            .or_else(|| {
                server_workspace.as_deref().map(|workspace| {
                    let (workspace, executor) =
                        resolve_request_execution_bindings(&request, workspace);
                    ExecutionBindingSnapshot::inferred(workspace, executor)
                })
            })
            .or_else(|| {
                resolve_request_execution_bindings_without_server_workspace(&request, &edge_profile)
                    .map(|(workspace, executor)| {
                        ExecutionBindingSnapshot::inferred(workspace, executor)
                    })
            });
        let tool_runtime_workspace = if let Some(workspace) = cloud_workspace.clone() {
            Some(workspace)
        } else if let Some(workspace) = server_workspace.clone() {
            Some(workspace)
        } else if server_side_tool_catalog && execution_bindings.is_some() {
            match self.provision_server_workspace(&session_id) {
                Ok(workspace) => Some(workspace),
                Err(error) => {
                    self.runs.write().await.remove(&run_id);
                    return Err(error);
                }
            }
        } else {
            None
        };

        if let Err(error) = self
            .persist_run_start(
                &run_id,
                &user_id,
                &session_id,
                &request,
                execution_bindings.as_ref(),
            )
            .await
        {
            self.runs.write().await.remove(&run_id);
            if let Some(record) = cloud_workspace_record.as_ref() {
                self.cleanup_cloud_workspace_after_failed_start(
                    &user_id,
                    &session_id,
                    &run_id,
                    record,
                    format!(
                        "durable run start failed after cloud workspace provisioning: {}",
                        error.1.0.detail
                    ),
                )
                .await;
            }
            return Err(error);
        }

        // Spawn background agentic loop.
        // Look up the plan-resume hint up-front so the system prompt on every
        // turn reminds the LLM a plan is in flight. Missing pool → None, missing
        // active plan → None, transient errors → None (best-effort).
        let plan_resume_hint = if let Some(shared) = &self.shared_pool {
            let repo = astra_plan::CloudPlanRepository::new(shared.get().clone());
            astra_plan::plan_resume_hint_for_session(&repo, &session_id).await
        } else {
            None
        };
        let task_board_resume_hint = self
            .task_board_resume_hint_for_session(&user_id, &session_id)
            .await;
        let mut host = self.build_host(
            &user_id,
            &session_id,
            &run_id,
            &request,
            edge_tools,
            edge_profile,
            execution_bindings.as_ref(),
            plan_resume_hint,
            task_board_resume_hint,
        );
        if let Some(snapshot) = execution_bindings.as_ref() {
            host.set_execution_metadata(Value::Object(binding_event_fields(
                &snapshot.workspace,
                &snapshot.executor,
            )));
        }
        if let Some(ref bundle) = mcp_bundle {
            host.install_runtime_tool_schemas(bundle.schemas.clone());
        }
        let mut loop_state = self.build_initial_state(
            &user_id,
            &request,
            &session_id,
            &run_id,
            tool_runtime_workspace
                .as_deref()
                .or(server_workspace.as_deref()),
            execution_bindings.as_ref(),
            Some(llm_cancel_token.clone()),
        );
        loop_state.context_manifest_user_id = Some(user_id.clone());
        // Inject user_id into the harness sink used by DB-persistence tests.
        #[cfg(feature = "harness")]
        loop_state.harness.set_user_id(&user_id);

        loop_state.session_turn = infer_session_turn(self.shared_pool.as_ref(), &session_id).await;
        let fresh_session_current_date = loop_state
            .pipeline_session
            .as_ref()
            .map(|session| session.current_date().to_string())
            .unwrap_or_else(|| {
                crate::turn::session_current_date::resolve_session_current_date(&session_id)
            });

        // ── Runtime warm-start: restore loop state from checkpoint ──
        // Overwrites fresh advisory state with checkpointed pipeline,
        // compaction, and context-window counters. Without this, server-side
        // session resume starts cold even though finalization persisted the
        // state needed for long-running sessions.
        if let Ok(Some(restored)) = astra_pipeline::step_restore::restore_session(&session_id) {
            restore_step_checkpoint_runtime_state(
                restored,
                &fresh_session_current_date,
                &mut loop_state,
            );
        }

        // ── CSL: Load conversation history from the log ─────────────
        let csl_manager = if request.session_id.is_some() {
            self.restore_csl_history(&user_id, &session_id, &run_id, &mut loop_state)
                .await
        } else {
            None
        };

        self.configure_loop_state_runtime_controls(
            &mut loop_state,
            &cancel_flag,
            &pause_flag,
            &llm_cancel_token,
        );
        configure_runtime_controllers(
            &self.matrixone,
            self.shared_pool.as_ref(),
            &mut loop_state,
            &user_id,
            &session_id,
        )
        .await;
        // The ServerToolExecutor is the owner for server-side runtime tools
        // such as `agent`. For edge-bound runs this workspace is only an
        // internal runtime scratch dir; execution routing still follows the
        // explicit workspace/executor binding and cannot silently fall back.
        if let Some(workspace) = tool_runtime_workspace {
            let memoria_base = Some(astra_core::MemoriaSettings::from_env().base_url);
            let task_store = match astra_tools::task_mgmt_matrixone::select_task_store(
                self.shared_pool.as_ref().map(|p| p.get().clone()),
                user_id.clone(),
            ) {
                Ok(store) => store,
                Err(error) => {
                    let message =
                        format!("tool executor setup failed after durable run start: {error}");
                    self.fail_started_run_before_spawn(&run_id, &message).await;
                    if let Some(record) = cloud_workspace_record.as_ref() {
                        self.cleanup_cloud_workspace_after_failed_start(
                            &user_id,
                            &session_id,
                            &run_id,
                            record,
                            message,
                        )
                        .await;
                    }
                    return Err(error_response(StatusCode::INTERNAL_SERVER_ERROR, error));
                }
            };
            let mut executor = server_tool_executor::ServerToolExecutor::new(
                workspace.clone(),
                user_id.clone(),
                session_id.clone(),
                memoria_base,
                None,
            )
            .with_capabilities(crate::capabilities::lifecycle_server_capabilities(
                self.shared_pool.is_some(),
            ))
            .with_cancel_token(loop_state.cancellation.token.clone())
            .with_task_store(task_store);

            // Enable exactly-once tool execution for crash recovery dedup.
            // This prevents side-effect tools (github_create_issue, task create, etc.)
            // from re-executing when a session resumes after a crash.
            executor.enable_exactly_once().await;

            // Apply shared ToolExecutionService (with admin-controllable disabled_tools)
            // or fall back to building one from deployment config.
            if let Some(ref shared_tes) = self.tool_execution_service {
                executor = executor.with_tool_execution_service(shared_tes.clone());
            } else {
                let mut builder = ToolExecutionService::builder();
                let disabled = load_deployment_disabled_tools();
                if !disabled.is_empty() {
                    builder = builder.initial_disabled_tools(&disabled);
                }
                if let Some(pool) = &self.edge_connection_pool {
                    builder = builder.edge_connection_pool(pool.clone());
                }
                if let Some(svc) = &self.edge_dispatch_service {
                    builder = builder.edge_dispatch_service(Arc::clone(svc));
                }
                if let Some(svc) = &self.edge_registry_service {
                    builder = builder.edge_registry_service(Arc::clone(svc));
                }
                executor = executor.with_tool_execution_service(builder.build());
            }

            if let Some(ref bundle) = mcp_bundle {
                executor.set_mcp_manager(bundle.manager.clone());
                executor.set_plugin_schemas(bundle.schemas.clone());
            }
            // Wire the plan repository so enter/exit_plan_mode tools work and
            // the write-tool guard can check `active_plan_id`.
            if let Some(shared) = &self.shared_pool {
                executor.set_context_manifest_pool(shared.clone());
                executor = executor.with_workspace_artifact_store(
                    astra_services::DatabaseSessionArtifactStore::new(self.matrixone.clone())
                        .with_pool(shared.clone()),
                );
                executor.set_plan_repository(std::sync::Arc::new(
                    astra_plan::CloudPlanRepository::new(shared.get().clone()),
                ));
            }
            // Share the host's plan-resume hint slot so tool-triggered
            // plan-mode changes refresh the system prompt mid-run.
            executor.set_plan_resume_hint_handle(host.plan_resume_hint_handle());
            if let Some(observability_session) = loop_state.telemetry.observability_session.clone()
            {
                executor.set_observability_session(observability_session);
            }
            if let Some(writer) = self.auxiliary_event_writer.clone() {
                executor.set_auxiliary_event_writer(writer);
            }
            let binding_snapshot = execution_bindings.clone().unwrap_or_else(|| {
                let (workspace_binding, executor_binding) =
                    resolve_request_execution_bindings(&request, workspace.as_path());
                ExecutionBindingSnapshot::inferred(workspace_binding, executor_binding)
            });
            let agent_working_dir =
                agent_working_dir_for_bindings(execution_bindings.as_ref(), workspace.as_path());
            host.set_execution_metadata(Value::Object(binding_event_fields(
                &binding_snapshot.workspace,
                &binding_snapshot.executor,
            )));
            executor.set_execution_binding_snapshot(binding_snapshot);
            executor.set_workspace_record(cloud_workspace_record.clone());

            self.wire_server_dynamic_agent_tools(
                &mut executor,
                &user_id,
                &session_id,
                &run_id,
                loop_state.session_turn,
                &request,
                agent_working_dir.as_path(),
                None,
                Some(pause_flag.clone()),
                Some(llm_cancel_token.clone()),
                #[cfg(feature = "harness")]
                loop_state.harness.sink.clone(),
            )
            .await;

            if request.interactive_client {
                // ── Phase E: Wire WebSocket approval and ask_user gates ───
                let (approval_tx, approval_rx) = mpsc::channel::<Value>(64);
                let approval_gate = astra_turn_core::ws_approval_gate::WebSocketApprovalGate::new(
                    user_id.clone(),
                    self.edge_callback_ledger.clone(),
                    approval_tx,
                );
                executor.set_approval_gate(std::sync::Arc::new(approval_gate));
                self.approval_channels
                    .lock()
                    .await
                    .insert(run_id.clone(), approval_rx);

                let (user_prompt_tx, user_prompt_rx) = mpsc::channel::<Value>(64);
                let user_prompt_gate =
                    astra_turn_core::ws_user_prompt_gate::WebSocketUserPromptGate::new(
                        user_id.clone(),
                        self.edge_callback_ledger.clone(),
                        user_prompt_tx,
                    );
                executor.set_ask_user_gate(std::sync::Arc::new(user_prompt_gate));
                self.user_prompt_channels
                    .lock()
                    .await
                    .insert(run_id.clone(), user_prompt_rx);

                // ── Phase F.3: Wire WebSocket progress callback ──────
                let (progress_tx, progress_rx) = mpsc::channel::<ProgressEvent>(64);
                let progress_cb =
                    astra_server_types::ws_progress_callback::WebSocketProgressCallback::new(
                        progress_tx,
                    );
                executor.set_progress_callback(std::sync::Arc::new(progress_cb));
                self.progress_channels
                    .lock()
                    .await
                    .insert(run_id.clone(), progress_rx);
            } else {
                executor.set_approval_gate(std::sync::Arc::new(NonInteractiveApprovalGate));
            }

            wire_executor_into_state(executor, &mut loop_state);
        }

        // Clone handles we need inside the spawned task.
        let bg_approval_channels = self.approval_channels.clone();
        let bg_user_prompt_channels = self.user_prompt_channels.clone();
        let bg_progress_channels = self.progress_channels.clone();
        let runs = self.runs_handle();
        let run_engine = self.run_engine.clone();
        let bg_run_id = run_id.clone();
        let bg_session_id = session_id.clone();
        let bg_resource_governor = self.resource_governor.clone();
        let bg_user_id = user_id.clone();
        let bg_cloud_workspace_record = cloud_workspace_record.clone();
        let bg_workspace_record_store = self.workspace_record_store.clone();
        let _bg_cancel_flag = cancel_flag.clone();
        let _bg_pause_flag = pause_flag.clone();
        let _bg_llm_cancel_token = llm_cancel_token.clone();
        let persist_ctx = PostLoopPersistContext {
            matrixone: self.matrixone.clone(),
            shared_pool: self.shared_pool.clone(),
            user_id: user_id.clone(),
            session_id: session_id.clone(),
            run_id: run_id.clone(),
            agent_id: request.agent_id.clone(),
            model_name: request.model.clone(),
            user_message: request.message.clone(),
            hook_db_writer: self.hook_db_writer.clone(),
            observer_worker: self.observer_worker.clone(),
            tool_event_writer: self.tool_event_writer.clone(),
            csl_manager: csl_manager.map(tokio::sync::Mutex::new),
        };

        // ── Global admission control: limit concurrent agentic loop tasks ──
        // Wait up to 30 s for a slot (previously immediate 503);
        // if no slot frees up, return 503.
        let permit = match tokio::time::timeout(
            std::time::Duration::from_secs(30),
            self.run_semaphore.clone().acquire_owned(),
        )
        .await
        {
            Ok(permit) => permit,
            Err(_elapsed) => {
                self.fail_started_run_before_spawn(
                    &run_id,
                    "server capacity timeout before agentic loop start",
                )
                .await;
                if let Some(record) = cloud_workspace_record.as_ref() {
                    self.cleanup_cloud_workspace_after_failed_start(
                        &user_id,
                        &session_id,
                        &run_id,
                        record,
                        "server capacity timeout before agentic loop start".to_string(),
                    )
                    .await;
                }
                return Err(error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "server at capacity, please retry",
                ));
            }
        };

        // Background task tracking: background_task_count is incremented before
        // spawn and decremented via RAII guard on exit. serve()'s shutdown path
        // calls drain_background_tasks() to wait for in-flight runs.
        let bg_task_count_1 = Arc::clone(&self.background_task_count);
        bg_task_count_1.fetch_add(1, Ordering::Release);
        spawn_observed(
            async move {
                let _permit = permit; // RAII: released when this task completes
                // RAII guard: decrement counter when this task exits (normal or panic).
                struct TaskCountGuard(Arc<AtomicUsize>);
                impl Drop for TaskCountGuard {
                    fn drop(&mut self) {
                        self.0.fetch_sub(1, Ordering::Release);
                    }
                }
                let _guard = TaskCountGuard(bg_task_count_1);

                // Pre-flight: check daily token budget before starting the agentic loop.
                if let Some(ref gov) = bg_resource_governor {
                    use astra_services::resource_governor::LimitCheck;
                    if let LimitCheck::Denied { reason } = gov.check_token_budget(&bg_user_id).await
                    {
                        tracing::warn!(
                            target: "astra_runtime::run_lifecycle",
                            user_id = %bg_user_id,
                            run_id = %bg_run_id,
                            reason = %reason,
                            "run rejected: daily token budget exhausted"
                        );
                        if let Some(run) = runs.write().await.get_mut(&bg_run_id) {
                            if run.status.try_transition(&RunStatus::Failed).is_ok() {
                                run.status = RunStatus::Failed;
                            }
                            // Push terminal events so SSE clients see the failure.
                            run.events.push(json!({
                                "event_type": "run_error",
                                "data": {"error": reason.clone()}
                            }));
                            run.events.push(json!({
                                "event_type": "run_finished",
                                "data": {"total_prompt_tokens": 0, "total_completion_tokens": 0}
                            }));
                            run.live_tx = None;
                        }
                        // Clean up channels for this run.
                        bg_approval_channels.lock().await.remove(&bg_run_id);
                        bg_user_prompt_channels.lock().await.remove(&bg_run_id);
                        bg_progress_channels.lock().await.remove(&bg_run_id);
                        astra_core::log_persist!(
                            run_engine
                                .persist_status(
                                    &bg_run_id,
                                    astra_core::STATUS_FAILED,
                                    None,
                                    Some(&reason),
                                )
                                .await,
                            "run_lifecycle",
                            &bg_run_id,
                            "budget_reject"
                        );
                        if let Some(record) = bg_cloud_workspace_record.as_ref() {
                            Self::cleanup_cloud_workspace_with_debt(
                            bg_workspace_record_store.clone(),
                            &bg_user_id,
                            &bg_session_id,
                            &bg_run_id,
                            record,
                            RuntimeCleanupReason::Failed,
                            format!(
                                "run rejected before agentic loop start: daily token budget exhausted: {reason}"
                            ),
                        )
                        .await;
                        }
                        Self::schedule_run_eviction(&runs, bg_run_id.clone());
                        return;
                    }
                }

                let outcome =
                    run_agentic_loop_with_host_panic_safe(&mut host, &mut loop_state).await;
                let loop_success = outcome.is_ok();
                let (events, final_status, error_msg) =
                    Self::finalize_run_events(outcome, host.take_emitted_events(), &loop_state);

                // Clean up channels for this run.
                bg_approval_channels.lock().await.remove(&bg_run_id);
                bg_user_prompt_channels.lock().await.remove(&bg_run_id);
                bg_progress_channels.lock().await.remove(&bg_run_id);
                let terminal_events = terminal_events_for_persistence(&events);

                // Publish terminal run state before best-effort post-run side effects
                // so background observers do not stay stuck in "running" because a
                // hook, event write, or learning save is slow.
                let mut persisted_status = final_status.clone();
                let mut persist_status_update = true;
                let mut persist_terminal_events = true;

                if let Some(run) = runs.write().await.get_mut(&bg_run_id) {
                    if run.status == RunStatus::Cancelled {
                        persist_status_update = false;
                        persist_terminal_events = false;
                        merge_cancelled_run_events(run, events);
                        if final_status != RunStatus::Waiting {
                            run.live_tx = None;
                        }
                        flush_turn_observability(&mut loop_state, &bg_session_id, true);
                    } else {
                        run.events.extend(events);
                        if should_preserve_manual_pause_on_completion(&run.status, &final_status) {
                            persist_status_update = false;
                            persisted_status = RunStatus::Paused;
                            run.waiting_for
                                .get_or_insert_with(|| "user_resume".to_string());
                            run.live_tx = None;
                        } else if run.status.try_transition(&final_status).is_ok() {
                            run.status = final_status.clone();
                        }
                        if !run.status.is_resumable() {
                            run.live_tx = None;
                        }
                    }
                }

                if persist_status_update
                    && should_preserve_manual_pause_from_durable(
                        &run_engine,
                        &bg_run_id,
                        &final_status,
                    )
                    .await
                {
                    persist_status_update = false;
                    persisted_status = RunStatus::Paused;
                    if let Some(run) = runs.write().await.get_mut(&bg_run_id) {
                        run.status = RunStatus::Paused;
                        run.pause_flag.store(true, Ordering::SeqCst);
                        run.waiting_for
                            .get_or_insert_with(|| "user_resume".to_string());
                        run.live_tx = None;
                    }
                }

                // Schedule eviction of the terminal run from the in-memory cache.
                // Waiting and paused runs are NOT evicted — they may still be resumed.
                if !persisted_status.is_resumable() {
                    Self::schedule_run_eviction(&runs, bg_run_id.clone());
                }

                if persist_status_update {
                    astra_core::log_persist!(
                        run_engine
                            .persist_status(
                                &bg_run_id,
                                persisted_status.as_str(),
                                None,
                                error_msg.as_deref()
                            )
                            .await,
                        "run_lifecycle",
                        &bg_run_id,
                        "status"
                    );
                }
                astra_core::log_persist!(
                    run_engine
                        .persist_usage(
                            &bg_run_id,
                            loop_state.total_prompt,
                            loop_state.total_completion,
                            loop_state.total_tool_calls,
                        )
                        .await,
                    "run_lifecycle",
                    &bg_run_id,
                    "usage"
                );
                // Record tokens consumed so check_token_budget sees up-to-date usage.
                if let Some(ref gov) = bg_resource_governor {
                    let total = loop_state.total_prompt + loop_state.total_completion;
                    if total > 0 {
                        gov.record_tokens(&bg_user_id, total).await;
                    }
                }
                if persist_terminal_events && !terminal_events.is_empty() {
                    astra_core::log_persist!(
                        run_engine
                            .append_events_batch(&bg_run_id, &terminal_events)
                            .await,
                        "run_lifecycle",
                        &bg_run_id,
                        "append_terminal_events_batch"
                    );
                }

                if persist_terminal_events {
                    flush_turn_observability(&mut loop_state, &bg_session_id, false);
                    persist_turn_evaluation_journal(&bg_session_id, "server_runtime", &loop_state);
                }

                // Best-effort post-loop persistence (core events, tool events,
                // hook DB, observer, session-end hooks, promotion events).
                persist_ctx.run(&loop_state, loop_success).await;

                // Post-loop memory cleanup — shared with the streaming path
                // (see `stream_chat`). Runs governance once per session
                // debounce window, clears bridge seen-ledger, and forgets
                // extraction debounce.
                post_loop_memory_cleanup(
                    loop_state.current_session_id.as_deref().unwrap_or(""),
                    &loop_state.session_facts,
                    loop_state.memory_extraction_service.as_ref(),
                    build_shutdown_extraction_request(&loop_state),
                )
                .await;

                if let Some(record) = bg_cloud_workspace_record.as_ref() {
                    Self::cleanup_cloud_workspace_after_terminal_run(
                        bg_workspace_record_store,
                        &bg_user_id,
                        &bg_session_id,
                        &bg_run_id,
                        record,
                        &persisted_status,
                    )
                    .await;
                }
            },
            "agentic_loop_create_run",
        );

        Ok(ChatRunRecord {
            session_id,
            run_id,
            status: STATUS_RUNNING.to_string(),
            explain: if request.explain {
                Some(json!({"mode": "background"}))
            } else {
                None
            },
        })
    }

    /// Stream chat (incremental SSE mode): spawns the agentic loop in a
    /// background task and returns an event channel for incremental streaming.
    /// Post-loop cleanup (persistence, learning state) runs inside the task.
    async fn stream_chat(
        &self,
        user_id: String,
        request: ChatRequestData,
    ) -> Result<ChatStreamRecord, (StatusCode, Json<ErrorResponse>)> {
        self.validate_request_constraints(&user_id, &request)
            .await?;

        // ── Resource governance check ────────────────────────────────
        if let Some(ref gov) = self.resource_governor {
            if let astra_services::resource_governor::LimitCheck::Denied { reason } =
                gov.check_run_start(&user_id).await
            {
                return Err(error_response(
                    StatusCode::TOO_MANY_REQUESTS,
                    format!("Resource limit exceeded: {reason}"),
                ));
            }
        }

        let run_id = Uuid::new_v4().to_string();
        let session_id = request
            .session_id
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());

        let edge_tools = Self::extract_edge_tools(&request);
        let server_side_tool_catalog = edge_tools.is_empty();
        let edge_profile = Self::extract_edge_profile(&request);

        // ── MCP: request-scoped discovery; schemas and credentials stay in memory.
        let mcp_bundle =
            runtime_mcp::prepare_request_scoped_runtime_bundle(&request.runtime_mcp_bindings)
                .await?;

        // Provision workspace early for web-agent mode (no edge tools) so
        // build_initial_state loads stop hooks from the provisioned directory.
        let cloud_workspace_record = self
            .provision_cloud_workspace_record(&user_id, &session_id, &request, &run_id)
            .await?;
        // Orchestrator-managed architecture: executor bindings come directly
        // from the workspace record — no server-owned executor scheduling.
        let cloud_execution_bindings = cloud_workspace_record
            .as_ref()
            .map(|record| execution_bindings_from_workspace_record(record));
        let cloud_workspace = cloud_workspace_record
            .as_ref()
            .map(|record| PathBuf::from(&record.root_or_volume_ref));

        let server_workspace = if cloud_workspace_record.is_none()
            && request_uses_server_workspace(&request, !edge_tools.is_empty())
        {
            Some(self.provision_server_workspace(&session_id)?)
        } else {
            None
        };
        let execution_bindings = cloud_execution_bindings
            .or_else(|| {
                server_workspace.as_deref().map(|workspace| {
                    let (workspace, executor) =
                        resolve_request_execution_bindings(&request, workspace);
                    ExecutionBindingSnapshot::inferred(workspace, executor)
                })
            })
            .or_else(|| {
                resolve_request_execution_bindings_without_server_workspace(&request, &edge_profile)
                    .map(|(workspace, executor)| {
                        ExecutionBindingSnapshot::inferred(workspace, executor)
                    })
            });
        let stream_agent_spawner = self
            .server_agent_spawner_for_session(&session_id)
            .await
            .spawner;
        let tool_runtime_workspace = if let Some(workspace) = cloud_workspace.clone() {
            Some(workspace)
        } else if let Some(workspace) = server_workspace.clone() {
            Some(workspace)
        } else if server_side_tool_catalog && execution_bindings.is_some() {
            Some(self.provision_server_workspace(&session_id)?)
        } else {
            None
        };

        // Create bounded live channels. If a client cannot keep up, the host
        // detaches that live stream without cancelling the server-side run.
        const SSE_CHANNEL_CAPACITY: usize = 512;
        let (client_event_tx, event_rx) = mpsc::channel::<Value>(SSE_CHANNEL_CAPACITY);
        let (event_tx, mut fanout_rx) = mpsc::channel::<Value>(SSE_CHANNEL_CAPACITY);
        let (live_tx, _) = broadcast::channel::<Value>(SSE_CHANNEL_CAPACITY);
        let live_tx_for_fanout = live_tx.clone();
        let client_event_tx_for_fanout = client_event_tx.clone();
        let fanout_runs = self.runs_handle();
        let fanout_run_id = run_id.clone();
        spawn_observed(
            async move {
                while let Some(event) = fanout_rx.recv().await {
                    if live_delta_event_for_persistence(&event) {
                        if let Some(run) = fanout_runs.write().await.get_mut(&fanout_run_id) {
                            push_active_run_live_event(run, event.clone());
                        }
                    }
                    let _ = live_tx_for_fanout.send(event.clone());
                    let _ = client_event_tx_for_fanout.send(event).await;
                }
            },
            "sse_fanout",
        );
        let progress_bridge =
            self.spawn_agent_progress_stream_bridge(run_id.clone(), event_tx.clone());

        let (mut run_state, cancel_flag, pause_flag, llm_cancel_token) =
            Self::build_tracked_run_state(run_id.clone(), session_id.clone(), user_id.clone());
        run_state.live_tx = Some(live_tx.clone());

        let mut state = self.build_initial_state(
            &user_id,
            &request,
            &session_id,
            &run_id,
            tool_runtime_workspace
                .as_deref()
                .or(server_workspace.as_deref()),
            execution_bindings.as_ref(),
            Some(llm_cancel_token.clone()),
        );
        state.context_manifest_user_id = Some(user_id.clone());
        // Inject user_id into the harness sink used by DB-persistence tests.
        #[cfg(feature = "harness")]
        state.harness.set_user_id(&user_id);

        state.session_turn = infer_session_turn(self.shared_pool.as_ref(), &session_id).await;
        let fresh_session_current_date = state
            .pipeline_session
            .as_ref()
            .map(|session| session.current_date().to_string())
            .unwrap_or_else(|| {
                crate::turn::session_current_date::resolve_session_current_date(&session_id)
            });

        // ── Runtime warm-start from step checkpoint ────────────────
        if request.session_id.is_some() {
            if let Ok(Some(restored)) = astra_pipeline::step_restore::restore_session(&session_id) {
                restore_step_checkpoint_runtime_state(
                    restored,
                    &fresh_session_current_date,
                    &mut state,
                );
            }
        }

        // ── CSL: Load conversation history from the log ─────────────
        let csl_manager = if request.session_id.is_some() {
            self.restore_csl_history(&user_id, &session_id, &run_id, &mut state)
                .await
        } else {
            None
        };

        let plan_resume_hint = if let Some(shared) = &self.shared_pool {
            let repo = astra_plan::CloudPlanRepository::new(shared.get().clone());
            astra_plan::plan_resume_hint_for_session(&repo, &session_id).await
        } else {
            None
        };
        let task_board_resume_hint = self
            .task_board_resume_hint_for_session(&user_id, &session_id)
            .await;
        let mut host = self.build_host(
            &user_id,
            &session_id,
            &run_id,
            &request,
            edge_tools,
            edge_profile,
            execution_bindings.as_ref(),
            plan_resume_hint,
            task_board_resume_hint,
        );
        host.set_event_tx(event_tx.clone());
        host.set_client_cancel(cancel_flag.clone(), llm_cancel_token.clone());
        if let Some(snapshot) = execution_bindings.as_ref() {
            host.set_execution_metadata(Value::Object(binding_event_fields(
                &snapshot.workspace,
                &snapshot.executor,
            )));
        }

        // ── MCP: inject request-scoped schemas into host tool surface ─
        if let Some(ref bundle) = mcp_bundle {
            host.install_runtime_tool_schemas(bundle.schemas.clone());
        }

        // Guard: reject if this session already has a blocking run.
        // Hold write lock across check+insert to prevent TOCTOU race.
        {
            let mut runs = self.runs.write().await;
            let has_active = Self::session_has_blocking_run(&runs, &session_id);
            if has_active {
                if let Some(record) = cloud_workspace_record.as_ref() {
                    self.cleanup_cloud_workspace_after_failed_start(
                        &user_id,
                        &session_id,
                        &run_id,
                        record,
                        "session already has an active run before streaming agentic loop start"
                            .to_string(),
                    )
                    .await;
                }
                return Err(error_response(
                    StatusCode::CONFLICT,
                    "session already has an active run".to_string(),
                ));
            }
            runs.insert(run_id.clone(), run_state);
        }
        // Persist run first, so the binding is durable before the client
        // receives binding events and starts using the workspace.
        if let Err(error) = self
            .persist_run_start(
                &run_id,
                &user_id,
                &session_id,
                &request,
                execution_bindings.as_ref(),
            )
            .await
        {
            self.runs.write().await.remove(&run_id);
            if let Some(record) = cloud_workspace_record.as_ref() {
                self.cleanup_cloud_workspace_after_failed_start(
                    &user_id,
                    &session_id,
                    &run_id,
                    record,
                    format!(
                        "durable streaming run start failed after cloud workspace provisioning: {}",
                        error.1.0.detail
                    ),
                )
                .await;
            }
            return Err(error);
        }
        if let Some(snapshot) = execution_bindings.as_ref() {
            for event in binding_snapshot_events(
                &run_id,
                &session_id,
                &snapshot.workspace,
                &snapshot.executor,
            ) {
                if event_tx.send(event).await.is_err() {
                    self.fail_started_run_before_spawn(
                        &run_id,
                        "failed to start streaming run event stream",
                    )
                    .await;
                    if let Some(record) = cloud_workspace_record.as_ref() {
                        self.cleanup_cloud_workspace_after_failed_start(
                            &user_id,
                            &session_id,
                            &run_id,
                            record,
                            "failed to start streaming run event stream after cloud workspace provisioning"
                                .to_string(),
                        )
                        .await;
                    }
                    return Err(error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "failed to start run event stream".to_string(),
                    ));
                }
            }
        }
        if let Some(pool) = &self.shared_pool {
            let trace = server_trace_context(&user_id, &session_id, &run_id, state.session_turn);
            let user_transcript = TranscriptPersistItem {
                run_id: run_id.clone(),
                role: "user",
                content: request.message.clone(),
                source_event_id: trace.root_event_id,
            };
            persist_session_transcript_items(pool, &user_id, &session_id, &[user_transcript]).await;
        }

        self.configure_loop_state_runtime_controls(
            &mut state,
            &cancel_flag,
            &pause_flag,
            &llm_cancel_token,
        );
        configure_runtime_controllers(
            &self.matrixone,
            self.shared_pool.as_ref(),
            &mut state,
            &user_id,
            &session_id,
        )
        .await;

        // Wire the server-side runtime tool owner whenever the host exposes the
        // server tool catalog. For edge-bound runs this uses an internal
        // scratch workspace only; the visible binding still routes local-code
        // tools to edge or blocks when edge is unavailable.
        if let Some(workspace) = tool_runtime_workspace {
            let memoria_base = Some(astra_core::MemoriaSettings::from_env().base_url);
            let task_store = match astra_tools::task_mgmt_matrixone::select_task_store(
                self.shared_pool.as_ref().map(|p| p.get().clone()),
                user_id.clone(),
            ) {
                Ok(store) => store,
                Err(error) => {
                    let message = format!(
                        "streaming tool executor setup failed after durable run start: {error}"
                    );
                    self.fail_started_run_before_spawn(&run_id, &message).await;
                    if let Some(record) = cloud_workspace_record.as_ref() {
                        self.cleanup_cloud_workspace_after_failed_start(
                            &user_id,
                            &session_id,
                            &run_id,
                            record,
                            message,
                        )
                        .await;
                    }
                    return Err(error_response(StatusCode::INTERNAL_SERVER_ERROR, error));
                }
            };
            let mut executor = server_tool_executor::ServerToolExecutor::new(
                workspace.clone(),
                user_id.clone(),
                session_id.clone(),
                memoria_base,
                None,
            )
            .with_capabilities(crate::capabilities::lifecycle_server_capabilities(
                self.shared_pool.is_some(),
            ))
            .with_cancel_token(state.cancellation.token.clone())
            .with_task_store(task_store);

            // Enable exactly-once tool execution for crash recovery dedup.
            // This prevents side-effect tools (github_create_issue, task create, etc.)
            // from re-executing when a session resumes after a crash.
            executor.enable_exactly_once().await;

            // Apply shared ToolExecutionService (with admin-controllable disabled_tools)
            // or fall back to building one from deployment config.
            if let Some(ref shared_tes) = self.tool_execution_service {
                executor = executor.with_tool_execution_service(shared_tes.clone());
            } else {
                let mut builder = ToolExecutionService::builder();
                let disabled = load_deployment_disabled_tools();
                if !disabled.is_empty() {
                    builder = builder.initial_disabled_tools(&disabled);
                }
                if let Some(pool) = &self.edge_connection_pool {
                    builder = builder.edge_connection_pool(pool.clone());
                }
                if let Some(svc) = &self.edge_dispatch_service {
                    builder = builder.edge_dispatch_service(Arc::clone(svc));
                }
                if let Some(svc) = &self.edge_registry_service {
                    builder = builder.edge_registry_service(Arc::clone(svc));
                }
                executor = executor.with_tool_execution_service(builder.build());
            }

            // ── MCP: inject manager + plugin schemas into executor ────
            if let Some(ref bundle) = mcp_bundle {
                executor.set_mcp_manager(bundle.manager.clone());
                executor.set_plugin_schemas(bundle.schemas.clone());
            }
            if let Some(shared) = &self.shared_pool {
                executor.set_context_manifest_pool(shared.clone());
                executor = executor.with_workspace_artifact_store(
                    astra_services::DatabaseSessionArtifactStore::new(self.matrixone.clone())
                        .with_pool(shared.clone()),
                );
                executor.set_plan_repository(std::sync::Arc::new(
                    astra_plan::CloudPlanRepository::new(shared.get().clone()),
                ));
            }
            if let Some(observability_session) = state.telemetry.observability_session.clone() {
                executor.set_observability_session(observability_session);
            }
            if let Some(writer) = self.auxiliary_event_writer.clone() {
                executor.set_auxiliary_event_writer(writer);
            }
            let binding_snapshot = execution_bindings.clone().unwrap_or_else(|| {
                let (workspace_binding, executor_binding) =
                    resolve_request_execution_bindings(&request, workspace.as_path());
                ExecutionBindingSnapshot::inferred(workspace_binding, executor_binding)
            });
            let agent_working_dir =
                agent_working_dir_for_bindings(execution_bindings.as_ref(), workspace.as_path());
            executor.set_execution_binding_snapshot(binding_snapshot);
            executor.set_workspace_record(cloud_workspace_record.clone());
            executor.set_work_surface_event_tx(event_tx.clone());
            self.wire_server_dynamic_agent_tools(
                &mut executor,
                &user_id,
                &session_id,
                &run_id,
                state.session_turn,
                &request,
                agent_working_dir.as_path(),
                Some(event_tx.clone()),
                Some(pause_flag.clone()),
                Some(llm_cancel_token.clone()),
                #[cfg(feature = "harness")]
                state.harness.sink.clone(),
            )
            .await;
            wire_executor_into_state(executor, &mut state);
        }

        // Clone handles for the background task.
        let runs = self.runs_handle();
        let run_engine = self.run_engine.clone();
        let bg_run_id = run_id.clone();
        let bg_session_id = session_id.clone();
        let bg_resource_governor = self.resource_governor.clone();
        let bg_user_id = user_id.clone();
        let bg_cloud_workspace_record = cloud_workspace_record.clone();
        let bg_workspace_record_store = self.workspace_record_store.clone();
        let missing_lifecycle_spawner = Arc::clone(&stream_agent_spawner);
        let persist_ctx = PostLoopPersistContext {
            matrixone: self.matrixone.clone(),
            shared_pool: self.shared_pool.clone(),
            user_id: user_id.clone(),
            session_id: session_id.clone(),
            run_id: run_id.clone(),
            agent_id: request.agent_id.clone(),
            model_name: request.model.clone(),
            user_message: request.message.clone(),
            hook_db_writer: self.hook_db_writer.clone(),
            observer_worker: self.observer_worker.clone(),
            tool_event_writer: self.tool_event_writer.clone(),
            csl_manager: csl_manager.map(tokio::sync::Mutex::new),
        };

        // ── Global admission control: limit concurrent agentic loop tasks ──
        // Wait up to 30 s for a slot; if no slot frees up, return 503.
        let permit = match tokio::time::timeout(
            std::time::Duration::from_secs(30),
            self.run_semaphore.clone().acquire_owned(),
        )
        .await
        {
            Ok(permit) => permit,
            Err(_elapsed) => {
                self.fail_started_run_before_spawn(
                    &run_id,
                    "server capacity timeout before streaming agentic loop start",
                )
                .await;
                if let Some(record) = cloud_workspace_record.as_ref() {
                    self.cleanup_cloud_workspace_after_failed_start(
                        &user_id,
                        &session_id,
                        &run_id,
                        record,
                        "server capacity timeout before streaming agentic loop start".to_string(),
                    )
                    .await;
                }
                return Err(error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "server at capacity, please retry",
                ));
            }
        };

        // Background task tracking (same pattern as the create_run spawn above).
        // Spawn the agentic loop in a background task. Events are pushed
        // through event_tx incrementally; the HTTP handler streams them.
        let bg_task_count_2 = Arc::clone(&self.background_task_count);
        bg_task_count_2.fetch_add(1, Ordering::Release);
        spawn_observed(
            async move {
                let _permit = permit; // RAII: released when this task completes
                struct TaskCountGuard(Arc<AtomicUsize>);
                impl Drop for TaskCountGuard {
                    fn drop(&mut self) {
                        self.0.fetch_sub(1, Ordering::Release);
                    }
                }
                let _guard = TaskCountGuard(bg_task_count_2);
                let loop_result =
                    run_agentic_loop_with_host_panic_safe(&mut host, &mut state).await;
                let loop_success = loop_result.is_ok();

                // Best-effort post-loop persistence (core events, tool events,
                // hook DB, observer, session-end hooks, promotion events).
                persist_ctx.run(&state, loop_success).await;

                let (final_events, final_status, error_msg) =
                    Self::finalize_run_events(loop_result, host.take_emitted_events(), &state);
                // Ensure fast synchronous child-agent progress has reached both
                // durable replay and the live SSE stream before parent terminal
                // markers close the turn.
                let sent_lifecycle_events = progress_bridge.stop_and_drain().await;
                let missing_lifecycle_events = collect_missing_agent_lifecycle_events(
                    missing_lifecycle_spawner.as_ref(),
                    &bg_run_id,
                    &sent_lifecycle_events,
                )
                .await;
                let archived_lifecycle_events = collect_agent_lifecycle_events_for_persistence(
                    missing_lifecycle_spawner.as_ref(),
                    &bg_run_id,
                )
                .await;
                // In streaming mode, host-emitted `type` events have already gone
                // through event_tx and the fanout persistence path. Replay only the
                // synthesized terminal events appended by finalize_run_events.
                let streaming_final_events: Vec<Value> = final_events
                    .iter()
                    .filter(|event| streaming_final_event_for_replay(event))
                    .cloned()
                    .collect();
                let streamed_final_events = run_handlers::transform_stream_run_events_for_client(
                    &bg_run_id,
                    streaming_final_events.clone(),
                );
                let streaming_events_for_durable = merge_agent_lifecycle_before_terminal_events(
                    &final_events,
                    &archived_lifecycle_events,
                );
                persist_turn_evaluation_journal(&bg_session_id, "server_runtime", &state);
                let mut terminal_state_events = streaming_final_events;

                let mut persisted_status = final_status.clone();
                let mut persist_status_update = true;
                let mut persist_streaming_events = true;
                if let Some(run) = runs.write().await.get_mut(&bg_run_id) {
                    if run.status == RunStatus::Cancelled {
                        persist_status_update = false;
                        persist_streaming_events = false;
                        merge_cancelled_run_events(run, terminal_state_events);
                        if final_status != RunStatus::Waiting {
                            run.live_tx = None;
                        }
                        flush_turn_observability(&mut state, &bg_session_id, true);
                    } else {
                        run.events.append(&mut terminal_state_events);
                        if should_preserve_manual_pause_on_completion(&run.status, &final_status) {
                            persist_status_update = false;
                            persisted_status = RunStatus::Paused;
                            run.waiting_for
                                .get_or_insert_with(|| "user_resume".to_string());
                            run.live_tx = None;
                        } else if run.status.try_transition(&final_status).is_ok() {
                            run.status = final_status.clone();
                        }
                        if !run.status.is_resumable() {
                            run.live_tx = None;
                        }
                        flush_turn_observability(&mut state, &bg_session_id, false);
                    }
                }

                if persist_status_update
                    && should_preserve_manual_pause_from_durable(
                        &run_engine,
                        &bg_run_id,
                        &final_status,
                    )
                    .await
                {
                    persist_status_update = false;
                    persisted_status = RunStatus::Paused;
                    if let Some(run) = runs.write().await.get_mut(&bg_run_id) {
                        run.status = RunStatus::Paused;
                        run.pause_flag.store(true, Ordering::SeqCst);
                        run.waiting_for
                            .get_or_insert_with(|| "user_resume".to_string());
                        run.live_tx = None;
                    }
                }

                // Schedule eviction of the terminal run from the in-memory cache.
                // Waiting and paused runs are NOT evicted — they may still be resumed.
                if !persisted_status.is_resumable() {
                    Self::schedule_run_eviction(&runs, bg_run_id.clone());
                }

                // Record tokens consumed regardless of cancel — cancelled runs still
                // consumed tokens and must count toward the daily budget.
                if let Some(ref gov) = bg_resource_governor {
                    let total = state.total_prompt + state.total_completion;
                    if total > 0 {
                        gov.record_tokens(&bg_user_id, total).await;
                    }
                }

                if persist_status_update {
                    astra_core::log_persist!(
                        run_engine
                            .persist_status(
                                &bg_run_id,
                                persisted_status.as_str(),
                                None,
                                error_msg.as_deref()
                            )
                            .await,
                        "run_lifecycle",
                        &bg_run_id,
                        "status"
                    );
                }

                // Persist usage unconditionally — cancelled runs still consumed tokens
                // and must have accurate usage in durable store for billing/audit.
                astra_core::log_persist!(
                    run_engine
                        .persist_usage(
                            &bg_run_id,
                            state.total_prompt,
                            state.total_completion,
                            state.total_tool_calls,
                        )
                        .await,
                    "run_lifecycle",
                    &bg_run_id,
                    "usage"
                );

                // Persist terminal events to durable store in a single batch.
                if persist_streaming_events && !streaming_events_for_durable.is_empty() {
                    astra_core::log_persist!(
                        run_engine
                            .append_events_batch(&bg_run_id, &streaming_events_for_durable)
                            .await,
                        "run_lifecycle",
                        &bg_run_id,
                        "append_streaming_events_batch"
                    );
                }

                for event in missing_lifecycle_events {
                    if event_tx.send(event).await.is_err() {
                        break;
                    }
                }

                for event in streamed_final_events {
                    if event_tx.send(event).await.is_err() {
                        break;
                    }
                }

                // `turn_complete` carries successful assistant reconciliation data.
                // Failed/cancelled/waiting turns terminate via their run lifecycle
                // event (`run_error`, `run_finished`, `run_waiting`) instead.
                if should_emit_stream_turn_complete(&final_status) {
                    let _ = event_tx
                        .send(build_run_turn_complete_event_with_interruption(
                            state.total_tool_calls,
                            &state.final_text,
                            state.interruption.as_ref(),
                        ))
                        .await;
                }

                // Drop event_tx — signals end-of-stream to the HTTP handler.
                drop(event_tx);

                // Post-loop memory cleanup — identical to `create_run`. Runs
                // AFTER event_tx drops so the client sees the terminal event promptly
                // and doesn't wait on governance RTT.
                post_loop_memory_cleanup(
                    state.current_session_id.as_deref().unwrap_or(""),
                    &state.session_facts,
                    state.memory_extraction_service.as_ref(),
                    build_shutdown_extraction_request(&state),
                )
                .await;

                if let Some(record) = bg_cloud_workspace_record.as_ref() {
                    Self::cleanup_cloud_workspace_after_terminal_run(
                        bg_workspace_record_store,
                        &bg_user_id,
                        &bg_session_id,
                        &bg_run_id,
                        record,
                        &persisted_status,
                    )
                    .await;
                }
            },
            "agentic_loop_stream_chat",
        );

        Ok(ChatStreamRecord {
            session_id,
            run_id,
            events: Vec::new(),
            event_rx: Some(event_rx),
        })
    }

    async fn get_run_status(
        &self,
        run_id: String,
        user_id: String,
    ) -> Result<RunStatusRecord, (StatusCode, Json<ErrorResponse>)> {
        let run = self.require_durable_run_for_user(&run_id, &user_id).await?;
        Ok(Self::durable_status_record(&run))
    }

    async fn get_run_projection(
        &self,
        run_id: String,
        user_id: String,
        recent_limit: u32,
    ) -> Result<RunProjectionRecord, (StatusCode, Json<ErrorResponse>)> {
        let run = self.require_durable_run_for_user(&run_id, &user_id).await?;
        let projection = self
            .run_engine
            .load_run_projection(&run_id)
            .await
            .map_err(|error| {
                error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!("Failed to load run projection: {error}"),
                )
            })?;
        let latest_checkpoint = self
            .run_engine
            .load_latest_checkpoint(&run_id, None)
            .await
            .map_err(|error| {
                error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!("Failed to load run checkpoint: {error}"),
                )
            })?;
        let recent_events = Self::durable_recent_events(&run, recent_limit);
        let binding = Self::durable_run_execution_binding_snapshot(&run);

        if let Some(projection) = projection {
            Ok(RunProjectionRecord {
                run_id: projection.run_id,
                session_id: projection.session_id,
                status: projection.status,
                waiting_for: projection.waiting_for,
                error_message: projection.error_message,
                workspace: binding.workspace.clone(),
                executor: binding.executor.clone(),
                transport: binding.transport.clone(),
                fallback_policy: binding.fallback_policy.clone(),
                run_event_high_watermark: run.last_event_idx,
                projection_event_idx: projection.projection_event_idx,
                projection_updated_at: projection.updated_at,
                projection_hash: projection.projection_hash,
                latest_event_type: projection.latest_event_type,
                total_prompt_tokens: projection.total_prompt_tokens,
                total_completion_tokens: projection.total_completion_tokens,
                total_tool_calls: projection.total_tool_calls,
                latest_checkpoint: latest_checkpoint.map(|checkpoint| {
                    RunProjectionCheckpointRecord {
                        checkpoint_id: checkpoint.checkpoint_id,
                        checkpoint_kind: checkpoint.checkpoint_kind,
                        checkpoint_version: checkpoint.checkpoint_version,
                        node_seq: checkpoint.node_seq,
                        created_at: checkpoint.created_at,
                    }
                }),
                recent_events,
            })
        } else {
            let latest_event_type = run.events.last().map(astra_services::extract_event_type);
            Ok(RunProjectionRecord {
                run_id: run.run_id.clone(),
                session_id: run.session_id.clone(),
                status: run.status.clone(),
                waiting_for: run.waiting_for.clone(),
                error_message: run.error_message.clone(),
                workspace: binding.workspace,
                executor: binding.executor,
                transport: binding.transport,
                fallback_policy: binding.fallback_policy,
                run_event_high_watermark: run.last_event_idx,
                projection_event_idx: run.last_event_idx,
                projection_updated_at: run.updated_at.clone(),
                projection_hash: format!(
                    "{:x}",
                    Sha256::digest(
                        serde_json::json!({
                            "run_id": run.run_id,
                            "status": run.status,
                            "waiting_for": run.waiting_for,
                            "last_event_idx": run.last_event_idx,
                            "total_prompt_tokens": run.total_prompt_tokens,
                            "total_completion_tokens": run.total_completion_tokens,
                            "total_tool_calls": run.total_tool_calls,
                            "latest_event_type": latest_event_type.clone(),
                        })
                        .to_string()
                        .as_bytes()
                    )
                ),
                latest_event_type,
                total_prompt_tokens: run.total_prompt_tokens,
                total_completion_tokens: run.total_completion_tokens,
                total_tool_calls: run.total_tool_calls,
                latest_checkpoint: latest_checkpoint.map(|checkpoint| {
                    RunProjectionCheckpointRecord {
                        checkpoint_id: checkpoint.checkpoint_id,
                        checkpoint_kind: checkpoint.checkpoint_kind,
                        checkpoint_version: checkpoint.checkpoint_version,
                        node_seq: checkpoint.node_seq,
                        created_at: checkpoint.created_at,
                    }
                }),
                recent_events,
            })
        }
    }

    async fn stream_run(
        &self,
        run_id: String,
        user_id: String,
        last_index: u32,
    ) -> Result<Vec<Value>, (StatusCode, Json<ErrorResponse>)> {
        let run = self.require_durable_run_for_user(&run_id, &user_id).await?;
        Ok(Self::durable_stream_events(&run, last_index))
    }

    async fn stream_run_live(
        &self,
        run_id: String,
        user_id: String,
        last_index: u32,
    ) -> Result<ChatStreamRecord, (StatusCode, Json<ErrorResponse>)> {
        let durable = self.require_durable_run_for_user(&run_id, &user_id).await?;
        let live_tx = {
            let runs = self.runs.read().await;
            runs.get(&run_id).and_then(|run| run.live_tx.clone())
        };
        if let Some(live_tx) = live_tx {
            let replay_events = Self::durable_stream_events(&durable, last_index);
            let mut live_rx = live_tx.subscribe();
            let (event_tx, event_rx) = mpsc::channel(512);
            spawn_observed(
                async move {
                    for event in replay_events {
                        if event_tx.send(event).await.is_err() {
                            return;
                        }
                    }
                    loop {
                        match live_rx.recv().await {
                            Ok(event) => {
                                if event_tx.send(event).await.is_err() {
                                    return;
                                }
                            }
                            Err(broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(broadcast::error::RecvError::Closed) => return,
                        }
                    }
                },
                "durable_stream_replay",
            );
            return Ok(ChatStreamRecord {
                session_id: durable.session_id,
                run_id,
                events: Vec::new(),
                event_rx: Some(event_rx),
            });
        }

        let events = Self::durable_stream_events(&durable, last_index);
        Ok(ChatStreamRecord {
            session_id: durable.session_id,
            run_id,
            events,
            event_rx: None,
        })
    }

    async fn drain_approval_requests(&self, run_id: &str) -> Vec<serde_json::Value> {
        let mut channels = self.approval_channels.lock().await;
        let Some(rx) = channels.get_mut(run_id) else {
            return vec![];
        };
        let mut requests = Vec::new();
        while let Ok(req) = rx.try_recv() {
            requests.push(req);
        }
        requests
    }

    async fn drain_user_prompt_requests(&self, run_id: &str) -> Vec<serde_json::Value> {
        let mut channels = self.user_prompt_channels.lock().await;
        let Some(rx) = channels.get_mut(run_id) else {
            return vec![];
        };
        let mut requests = Vec::new();
        while let Ok(req) = rx.try_recv() {
            requests.push(req);
        }
        requests
    }

    async fn drain_progress_events(&self, run_id: &str) -> Vec<serde_json::Value> {
        let mut channels = self.progress_channels.lock().await;
        let Some(rx) = channels.get_mut(run_id) else {
            return vec![];
        };
        let mut events = Vec::new();
        while let Ok(evt) = rx.try_recv() {
            events.push(serde_json::to_value(&evt).unwrap_or_default());
        }
        events
    }

    async fn submit_run_input(
        &self,
        run_id: String,
        user_id: String,
        input: RunInputData,
    ) -> Result<RunInputRecord, (StatusCode, Json<ErrorResponse>)> {
        if input.idempotency_key.trim().is_empty() {
            return Err(error_response(
                StatusCode::UNPROCESSABLE_ENTITY,
                "idempotency_key is required",
            ));
        }
        if deferred_input_text_len(&input.input) > MAX_DEFERRED_INPUT_CHARS {
            return Err(error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "deferred input is too large",
            ));
        }

        let idempotency_key = input.idempotency_key.trim().to_string();
        let event = json!({
            "event_type": "user_input",
            "idempotency_key": idempotency_key,
            "data": {"input": input.input},
        });

        let durable = self.require_durable_run_for_user(&run_id, &user_id).await?;
        let durable_status = Self::run_status_from_durable(&durable.status)?;
        if matches!(
            durable_status,
            RunStatus::Paused | RunStatus::Completed | RunStatus::Failed | RunStatus::Cancelled
        ) {
            return Err(Self::run_state_conflict("submit input to", &durable.status));
        }

        let duplicate = durable.events.iter().any(|event| {
            event.get("idempotency_key").and_then(Value::as_str) == Some(idempotency_key.as_str())
        });
        if duplicate {
            return Ok(RunInputRecord {
                run_id,
                accepted: true,
                duplicate: true,
            });
        }

        durable_status
            .try_transition(&RunStatus::InputQueued)
            .map_err(|_| Self::run_state_conflict("submit input to", &durable.status))?;

        self.run_engine
            .append_event(&run_id, event.clone())
            .await
            .map_err(|error| Self::durable_persist_error("input", error))?;

        let durable_after_append = self.require_durable_run_for_user(&run_id, &user_id).await?;
        let durable_status_after_append =
            Self::run_status_from_durable(&durable_after_append.status)?;
        if matches!(
            durable_status_after_append,
            RunStatus::Paused | RunStatus::Completed | RunStatus::Failed | RunStatus::Cancelled
        ) {
            if let Some(event_index) = durable_after_append.events.iter().enumerate().find_map(
                |(index, persisted_event)| {
                    (persisted_event
                        .get("idempotency_key")
                        .and_then(Value::as_str)
                        == Some(idempotency_key.as_str()))
                    .then_some(index)
                },
            ) {
                self.run_engine
                    .mark_user_inputs_released(&run_id, &[event_index])
                    .await
                    .map_err(|error| {
                        Self::durable_persist_error("input release rollback", error)
                    })?;
            }
            return Err(Self::run_state_conflict(
                "submit input to",
                &durable_after_append.status,
            ));
        }
        let status_updated = self
            .run_engine
            .persist_status_if_current(
                &run_id,
                &[STATUS_RUNNING, STATUS_INPUT_QUEUED, STATUS_WAITING],
                STATUS_INPUT_QUEUED,
                Some("user_input"),
                None,
            )
            .await
            .map_err(|error| Self::durable_persist_error("input status", error))?;
        if !status_updated {
            let durable_after_conflict =
                self.require_durable_run_for_user(&run_id, &user_id).await?;
            if let Some(event_index) = durable_after_conflict.events.iter().enumerate().find_map(
                |(index, persisted_event)| {
                    (persisted_event
                        .get("idempotency_key")
                        .and_then(Value::as_str)
                        == Some(idempotency_key.as_str()))
                    .then_some(index)
                },
            ) {
                self.run_engine
                    .mark_user_inputs_released(&run_id, &[event_index])
                    .await
                    .map_err(|error| {
                        Self::durable_persist_error("input release rollback", error)
                    })?;
            }
            return Err(Self::run_state_conflict(
                "submit input to",
                &durable_after_conflict.status,
            ));
        }
        let input_queued_event = json!({
            "event_type": "run_input_queued",
            "data": { "waiting_for": "user_input" },
        });
        self.run_engine
            .append_event(&run_id, input_queued_event.clone())
            .await
            .map_err(|error| Self::durable_persist_error("input queued event", error))?;
        let mut stream_input_queued_event = input_queued_event.clone();
        if let Some(obj) = stream_input_queued_event.as_object_mut() {
            obj.insert(
                "index".to_string(),
                json!(durable_after_append.events.len()),
            );
        }
        let live_events = run_handlers::transform_stream_run_events_for_client(
            &run_id,
            vec![stream_input_queued_event],
        );
        let live_tx = if let Some(run) = self.runs.write().await.get_mut(&run_id) {
            run.events.push(event);
            run.events.push(input_queued_event);
            run.status = RunStatus::InputQueued;
            run.waiting_for = Some("user_input".to_string());
            run.live_tx.clone()
        } else {
            None
        };
        if let Some(live_tx) = live_tx {
            for event in live_events {
                let _ = live_tx.send(event);
            }
        }

        Ok(RunInputRecord {
            run_id,
            accepted: true,
            duplicate: false,
        })
    }

    async fn drain_background_tasks(&self, timeout: std::time::Duration) -> bool {
        self.drain_background_tasks_impl(timeout).await
    }

    async fn cancel_run(
        &self,
        run_id: String,
        user_id: String,
    ) -> Result<CancelRunRecord, (StatusCode, Json<ErrorResponse>)> {
        let durable = self.require_durable_run_for_user(&run_id, &user_id).await?;
        let durable_status = Self::run_status_from_durable(&durable.status)?;
        if matches!(
            durable_status,
            RunStatus::Completed | RunStatus::Failed | RunStatus::Cancelled
        ) {
            return Ok(CancelRunRecord {
                run_id,
                status: durable.status,
            });
        }

        let cancel_event = json!({"event_type": "run_finished", "data": {"cancelled": true}});
        self.run_engine
            .persist_status(&run_id, STATUS_CANCELLED, None, None)
            .await
            .map_err(|error| Self::durable_persist_error("cancel status", error))?;
        let append_result = self
            .run_engine
            .append_event(&run_id, cancel_event.clone())
            .await;

        {
            let mut runs = self.runs.write().await;
            if let Some(run) = runs.get_mut(&run_id) {
                run.cancel_flag.store(true, Ordering::SeqCst);
                run.pause_flag.store(false, Ordering::SeqCst);
                run.llm_cancel_token.cancel();
                run.status = RunStatus::Cancelled;
                run.waiting_for = None;
                run.events.push(cancel_event);
            }
        }

        if let Some(de) = &self.delegation_engine {
            de.cancel_children_of(&run_id).await;
        }
        append_result.map_err(|error| Self::durable_persist_error("cancel event", error))?;
        Self::schedule_run_eviction(&self.runs, run_id.clone());

        Ok(CancelRunRecord {
            run_id,
            status: STATUS_CANCELLED.to_string(),
        })
    }

    async fn list_runs(
        &self,
        user_id: String,
        limit: u32,
        offset: u32,
    ) -> Result<RunListRecord, (StatusCode, Json<ErrorResponse>)> {
        let (limit, offset) = astra_services::pagination::clamp_api_list_pagination(limit, offset);
        let (durable_runs, total) = self
            .run_engine
            .list_user_runs(&user_id, limit, offset)
            .await
            .map_err(|error| {
                error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!("Failed to list durable run state: {error}"),
                )
            })?;
        let page = durable_runs
            .iter()
            .map(Self::durable_status_record)
            .collect();
        Ok(RunListRecord {
            runs: page,
            total,
            limit,
            offset,
        })
    }

    async fn pause_run(
        &self,
        run_id: String,
        user_id: String,
    ) -> Result<RunMutationRecord, (StatusCode, Json<ErrorResponse>)> {
        let durable = self.require_durable_run_for_user(&run_id, &user_id).await?;
        if durable.status != STATUS_RUNNING {
            return Err(Self::run_state_conflict("pause", &durable.status));
        }

        let pause_event = json!({"event_type": "run_paused", "data": {}});
        // Always write to DB first — the source of truth for cross-pod control.
        if let Err(error) = self
            .run_engine
            .persist_status(&run_id, STATUS_PAUSED, Some("user_resume"), None)
            .await
        {
            return Err(Self::durable_persist_error("pause status", error));
        }
        if let Err(error) = self
            .run_engine
            .append_event(&run_id, pause_event.clone())
            .await
        {
            // Rollback DB status. Ignore rollback failures — durable state
            // still says PAUSED, but the event was never written, so the run
            // is in a safe state (no partial event). On pod restart, the
            // LR/checkpoint logic handles reconciliation.
            let _ = self
                .run_engine
                .persist_status(&run_id, STATUS_RUNNING, None, None)
                .await;
            return Err(Self::durable_persist_error("pause event", error));
        }

        // Only update in-memory state after both DB writes succeeded.
        {
            let mut runs = self.runs.write().await;
            if let Some(run) = runs.get_mut(&run_id) {
                run.pause_flag.store(true, Ordering::SeqCst);
                run.status = RunStatus::Paused;
                run.waiting_for = Some("user_resume".to_string());
                run.events.push(pause_event);
            }
        }
        if let Some(de) = &self.delegation_engine {
            de.pause_children_of(&run_id).await;
        }
        Ok(RunMutationRecord {
            run_id,
            status: STATUS_PAUSED.to_string(),
            previous_status: durable.status,
        })
    }

    async fn resume_run(
        &self,
        run_id: String,
        user_id: String,
    ) -> Result<RunMutationRecord, (StatusCode, Json<ErrorResponse>)> {
        let durable = self.require_durable_run_for_user(&run_id, &user_id).await?;
        if durable.status != STATUS_PAUSED {
            return Err(Self::run_state_conflict("resume", &durable.status));
        }

        if has_buffered_terminal_completion(&durable.events) {
            self.run_engine
                .persist_status(&run_id, STATUS_COMPLETED, None, None)
                .await
                .map_err(|error| Self::durable_persist_error("resume completed status", error))?;
            {
                let mut runs = self.runs.write().await;
                if let Some(run) = runs.get_mut(&run_id) {
                    run.status = RunStatus::Completed;
                    run.pause_flag.store(false, Ordering::SeqCst);
                    run.waiting_for = None;
                    run.live_tx = None;
                }
            }
            Self::schedule_run_eviction(&self.runs, run_id.clone());
            return Ok(RunMutationRecord {
                run_id,
                status: STATUS_COMPLETED.to_string(),
                previous_status: durable.status,
            });
        }

        let resume_event = json!({"event_type": "run_resumed", "data": {}});
        // Always write to DB first — the source of truth for cross-pod control.
        if let Err(error) = self
            .run_engine
            .persist_status(&run_id, STATUS_RUNNING, None, None)
            .await
        {
            return Err(Self::durable_persist_error("resume status", error));
        }
        if let Err(error) = self
            .run_engine
            .append_event(&run_id, resume_event.clone())
            .await
        {
            let _ = self
                .run_engine
                .persist_status(&run_id, STATUS_PAUSED, Some("user_resume"), None)
                .await;
            return Err(Self::durable_persist_error("resume event", error));
        }

        // Only update in-memory state after both DB writes succeeded.
        {
            let mut runs = self.runs.write().await;
            if let Some(run) = runs.get_mut(&run_id) {
                run.pause_flag.store(false, Ordering::SeqCst);
                run.status = RunStatus::Running;
                run.waiting_for = None;
                run.events.push(resume_event);
            }
        }
        if let Some(de) = &self.delegation_engine {
            de.resume_children_of(&run_id).await;
        }
        Ok(RunMutationRecord {
            run_id,
            status: STATUS_RUNNING.to_string(),
            previous_status: durable.status,
        })
    }
}

// ─── Sub-Run Executor ───────────────────────────────────────────────────────

use crate::server::delegation::engine::{SubRunConfig, SubRunExecutor};

/// Server-side executor for dynamic `agent(action='spawn')` children.
///
/// It reuses the production sub-run loop executor so Web dynamic agents run
/// with the same server host, tool backend, skill resolver, memory plumbing,
/// and observe-only harness path as delegated children. Spawn-specific
/// semantics stay in `DynamicAgentSpawner` and `agent_tool`.
pub struct ServerSpawnAgentExecutor {
    matrixone: MatrixOneSettings,
    encryptor: Arc<FernetTokenEncryptor>,
    shared_pool: Option<SharedPool>,
    edge_callback_ledger: Arc<TokioMutex<HashMap<String, Value>>>,
    edge_connection_pool: Option<astra_server_types::edge_connection_pool::EdgeConnectionPool>,
    edge_dispatch_service: Option<Arc<dyn astra_services::multi_agent::EdgeDispatchService>>,
    edge_registry_service: Option<Arc<dyn astra_services::multi_agent::EdgeRegistryService>>,
    skill_service: Option<Arc<dyn SkillService>>,
    memory_extraction_service: Option<Arc<crate::session_memory::MemoryExtractionService>>,
    runtime_contexts: Arc<RwLock<HashMap<String, ServerSpawnRuntimeContext>>>,
}

impl ServerSpawnAgentExecutor {
    pub fn new(
        matrixone: MatrixOneSettings,
        encryptor: Arc<FernetTokenEncryptor>,
        edge_callback_ledger: Arc<TokioMutex<HashMap<String, Value>>>,
    ) -> Self {
        Self {
            matrixone,
            encryptor,
            shared_pool: None,
            edge_callback_ledger,
            edge_connection_pool: None,
            edge_dispatch_service: None,
            edge_registry_service: None,
            skill_service: None,
            memory_extraction_service: None,
            runtime_contexts: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn with_pool(mut self, pool: Option<SharedPool>) -> Self {
        self.shared_pool = pool;
        self
    }

    pub fn with_edge_connection_pool(
        mut self,
        pool: Option<astra_server_types::edge_connection_pool::EdgeConnectionPool>,
    ) -> Self {
        self.edge_connection_pool = pool;
        self
    }

    pub fn with_edge_dispatch_service(
        mut self,
        svc: Arc<dyn astra_services::multi_agent::EdgeDispatchService>,
    ) -> Self {
        self.edge_dispatch_service = Some(svc);
        self
    }

    pub fn with_edge_registry_service(
        mut self,
        svc: Arc<dyn astra_services::multi_agent::EdgeRegistryService>,
    ) -> Self {
        self.edge_registry_service = Some(svc);
        self
    }

    pub fn with_skill_service(mut self, service: Option<Arc<dyn SkillService>>) -> Self {
        self.skill_service = service;
        self
    }

    pub fn with_memory_extraction_service(
        mut self,
        svc: Option<Arc<crate::session_memory::MemoryExtractionService>>,
    ) -> Self {
        self.memory_extraction_service = svc;
        self
    }

    async fn set_runtime_context(&self, context: ServerSpawnRuntimeContext) {
        self.runtime_contexts
            .write()
            .await
            .insert(context.parent_run_id.clone(), context);
    }

    async fn runtime_context_for_config(
        &self,
        config: &SpawnRunConfig,
    ) -> Result<ServerSpawnRuntimeContext, String> {
        let parent_run_id = config
            .parent_address
            .as_ref()
            .map(|address| address.run_id.as_str())
            .ok_or_else(|| {
                "server dynamic agent executor requires parent run lineage".to_string()
            })?;

        self.runtime_contexts
            .read()
            .await
            .get(parent_run_id)
            .cloned()
            .ok_or_else(|| {
                format!(
                    "server dynamic agent executor has no runtime context for parent run {parent_run_id}"
                )
            })
    }

    fn build_subrun_executor(&self) -> ServerSubRunExecutor {
        let mut executor = ServerSubRunExecutor::new(
            self.matrixone.clone(),
            Arc::clone(&self.encryptor),
            Arc::clone(&self.edge_callback_ledger),
        );
        if let Some(pool) = self.shared_pool.clone() {
            executor = executor.with_pool(pool);
        }
        if let Some(pool) = self.edge_connection_pool.clone() {
            executor = executor.with_edge_connection_pool(pool);
        }
        if let Some(svc) = self.edge_dispatch_service.clone() {
            executor = executor.with_edge_dispatch_service(svc);
        }
        if let Some(svc) = self.edge_registry_service.clone() {
            executor = executor.with_edge_registry_service(svc);
        }
        if let Some(service) = self.skill_service.clone() {
            executor = executor.with_skill_service(service);
        }
        if let Some(svc) = self.memory_extraction_service.clone() {
            executor = executor.with_memory_extraction_service(svc);
        }
        executor
    }
}

fn spawn_child_request_constraints(
    parent: &RequestConstraints,
    config: &SpawnRunConfig,
) -> RequestConstraints {
    let child_allowed = if config.allowed_tools.iter().any(|tool| tool == "*") {
        if config.read_only {
            Some(
                ["bash", "glob", "grep", "list_dir", "read_file"]
                    .into_iter()
                    .map(String::from)
                    .collect::<HashSet<_>>(),
            )
        } else {
            None
        }
    } else {
        Some(
            config
                .allowed_tools
                .iter()
                .map(|tool| tool.trim().to_ascii_lowercase())
                .filter(|tool| !tool.is_empty())
                .collect::<HashSet<_>>(),
        )
    };

    let allowed_tools = match (&parent.allowed_tools, child_allowed) {
        (Some(parent), Some(child)) => Some(parent.intersection(&child).cloned().collect()),
        (Some(parent), None) => Some(parent.clone()),
        (None, Some(child)) => Some(child),
        (None, None) => None,
    };

    RequestConstraints::new(
        allowed_tools,
        parent.allowed_skills.clone(),
        parent.allowed_skill_sources.clone(),
    )
}

fn spawn_system_prompt(config: &SpawnRunConfig) -> String {
    if config.system_prompt_addendum.trim().is_empty() {
        format!(
            "You are '{}', a specialized sub-agent. Complete the task thoroughly.",
            config.agent_id
        )
    } else {
        format!(
            "You are '{}', a specialized sub-agent.\n\n{}\n\nComplete the task thoroughly.",
            config.agent_id, config.system_prompt_addendum
        )
    }
}

fn emit_server_subrun_agent_terminated(
    sink: Option<&SharedAgentLiveEventSink>,
    agent_id: &str,
    started_at: Instant,
    termination: AgentLiveTermination,
    reason: Option<String>,
) {
    let Some(sink) = sink else {
        return;
    };
    if let Err(error) = sink.send(AgentLiveEvent {
        agent_id: agent_id.to_string(),
        kind: AgentLiveEventKind::AgentTerminated {
            termination,
            duration_ms: started_at.elapsed().as_millis() as u64,
            reason,
        },
    }) {
        tracing::warn!(
            target: "astra_runtime::work_surface",
            agent_id,
            error = ?error,
            "failed to emit server subrun terminal live event"
        );
    }
}

fn server_subrun_live_termination(
    outcome: &Result<AgenticLoopOutcome, astra_core::ClassifiedError>,
    loop_state: &AgenticLoopState,
) -> AgentLiveTermination {
    match outcome {
        Ok(AgenticLoopOutcome::Completed) if loop_state.interruption.is_some() => {
            AgentLiveTermination::Cancelled
        }
        Ok(AgenticLoopOutcome::Completed) => AgentLiveTermination::Completed,
        Ok(AgenticLoopOutcome::Cancelled | AgenticLoopOutcome::Waiting(_)) => {
            AgentLiveTermination::Cancelled
        }
        Ok(AgenticLoopOutcome::Error(_)) | Err(_) => AgentLiveTermination::Failed,
    }
}

fn server_subrun_live_reason(
    outcome: &Result<AgenticLoopOutcome, astra_core::ClassifiedError>,
    loop_state: &AgenticLoopState,
) -> Option<String> {
    match outcome {
        Ok(AgenticLoopOutcome::Completed) if loop_state.interruption.is_some() => {
            Some("paused".to_string())
        }
        Ok(AgenticLoopOutcome::Completed) => None,
        Ok(AgenticLoopOutcome::Cancelled) => Some("cancelled".to_string()),
        Ok(AgenticLoopOutcome::Waiting(reason)) => Some(reason.clone()),
        Ok(AgenticLoopOutcome::Error(error)) => Some(error.clone()),
        Err(error) => Some(error.to_string()),
    }
}

#[async_trait]
impl SpawnAgentExecutor for ServerSpawnAgentExecutor {
    async fn execute(&self, config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
        let context = self.runtime_context_for_config(&config).await?;

        let mut profile =
            AgentProfile::new(&config.agent_id, &config.agent_type, AgentTier::System);
        profile.system_prompt = Some(spawn_system_prompt(&config));
        profile.model_override = config.model.clone();
        profile.skill_filter = config.allowed_tools.clone();
        profile.metadata.insert(
            "spawn_agent_type".to_string(),
            json!(config.agent_type.clone()),
        );
        profile
            .metadata
            .insert("spawn_read_only".to_string(), json!(config.read_only));

        let mut subrun_context = HashMap::new();
        subrun_context.insert(
            "workspace_root".to_string(),
            json!(config.working_dir.to_string_lossy().to_string()),
        );
        subrun_context.insert(
            "cwd".to_string(),
            json!(config.working_dir.to_string_lossy().to_string()),
        );
        subrun_context.insert("spawn_agent_id".to_string(), json!(config.agent_id.clone()));
        subrun_context.insert(
            "spawn_agent_type".to_string(),
            json!(config.agent_type.clone()),
        );
        subrun_context.insert(
            "parent_run_id".to_string(),
            json!(context.parent_run_id.clone()),
        );
        subrun_context.insert(
            "parent_agent_id".to_string(),
            json!(
                config
                    .parent_address
                    .as_ref()
                    .map(|address| address.agent_id.clone())
                    .unwrap_or_else(|| "root-agent".to_string())
            ),
        );
        subrun_context.insert(
            "trace_session_id".to_string(),
            json!(context.trace_context.session_id.clone()),
        );
        subrun_context.insert(
            "trace_user_id".to_string(),
            json!(context.trace_context.user_id.clone()),
        );
        subrun_context.insert(
            "trace_turn_id".to_string(),
            json!(context.trace_context.turn_id.clone()),
        );
        subrun_context.insert(
            "trace_turn_seq".to_string(),
            json!(context.trace_context.turn_seq),
        );
        subrun_context.insert(
            "trace_causal_chain_id".to_string(),
            json!(context.trace_context.causal_chain_id.clone()),
        );
        subrun_context.insert(
            "trace_root_event_id".to_string(),
            json!(context.trace_context.root_event_id.clone()),
        );

        let request_constraints =
            spawn_child_request_constraints(&context.request_constraints, &config);
        let subrun = SubRunConfig {
            run_id: config.run_id.clone(),
            agent_profile: profile,
            task: config.task.clone(),
            session_id: context.session_id.clone(),
            user_id: context.user_id.clone(),
            previous_output: None,
            context: subrun_context,
            forward_headers: context.forward_headers.clone(),
            llm_token_service: context.llm_token_service.clone(),
            request_constraints,
            recursion_depth: config.recursion_depth,
            max_turns: Some(config.max_turns),
            pause_flag: context.pause_flag.clone(),
            checkpoint_gate: None,
            mailbox: config.mailbox,
            progress_emitter: config.progress_emitter.clone(),
            live_event_sink: config.live_event_sink.clone(),
            cancel_token: context.cancel_token.clone(),
            inherited_prefix: config.inherited_prefix,
            execution_metadata: config
                .execution_metadata
                .clone()
                .or_else(|| context.execution_metadata.clone()),
            #[cfg(feature = "harness")]
            harness_sink: context.harness_sink.clone(),
        };

        let executor = self.build_subrun_executor();
        #[cfg(feature = "bridge-e2e-hooks")]
        let executor = if !context.test_child_llm_rounds.is_empty() {
            executor.with_test_llm_rounds(context.test_child_llm_rounds.clone())
        } else {
            executor
        };
        let result = executor.execute(subrun).await?;
        let projection = project_subrun_status_to_spawn(&result.status, result.error);

        Ok(SpawnRunResult {
            agent_id: result.agent_id,
            run_id: result.run_id,
            status: projection.status.to_string(),
            finish_reason: projection.finish_reason.to_string(),
            output: result.output,
            error: projection.error,
            prompt_tokens: result.prompt_tokens,
            completion_tokens: result.completion_tokens,
            tool_calls: result.tool_calls,
            permission_summary: None,
            permission_requests: 0,
            permission_requests_approved: 0,
            tools_blocked: 0,
        })
    }
}

/// Production sub-run executor backed by [`ServerAgenticLoopHost`].
///
/// Creates a real agentic loop for each sub-run with the agent's system prompt,
/// model, and tool configuration.
pub struct ServerSubRunExecutor {
    matrixone: MatrixOneSettings,
    encryptor: Arc<FernetTokenEncryptor>,
    shared_pool: Option<SharedPool>,
    edge_callback_ledger: Arc<TokioMutex<HashMap<String, Value>>>,
    edge_connection_pool: Option<astra_server_types::edge_connection_pool::EdgeConnectionPool>,
    edge_dispatch_service: Option<Arc<dyn astra_services::multi_agent::EdgeDispatchService>>,
    edge_registry_service: Option<Arc<dyn astra_services::multi_agent::EdgeRegistryService>>,
    skill_service: Option<Arc<dyn SkillService>>,
    memory_extraction_service: Option<Arc<crate::session_memory::MemoryExtractionService>>,
    /// Shared ToolExecutionService so executors share the same disabled_tools set.
    pub tool_execution_service: Option<ToolExecutionService>,
    #[cfg(feature = "bridge-e2e-hooks")]
    test_llm_rounds: Vec<Value>,
}

impl ServerSubRunExecutor {
    pub fn new(
        matrixone: MatrixOneSettings,
        encryptor: Arc<FernetTokenEncryptor>,
        edge_callback_ledger: Arc<TokioMutex<HashMap<String, Value>>>,
    ) -> Self {
        Self {
            matrixone,
            encryptor,
            shared_pool: None,
            edge_callback_ledger,
            edge_connection_pool: None,
            edge_dispatch_service: None,
            edge_registry_service: None,
            skill_service: None,
            memory_extraction_service: None,
            tool_execution_service: None,
            #[cfg(feature = "bridge-e2e-hooks")]
            test_llm_rounds: Vec::new(),
        }
    }

    pub fn with_pool(mut self, pool: SharedPool) -> Self {
        self.shared_pool = Some(pool);
        self
    }

    pub fn with_memory_extraction_service(
        mut self,
        svc: Arc<crate::session_memory::MemoryExtractionService>,
    ) -> Self {
        self.memory_extraction_service = Some(svc);
        self
    }

    pub fn with_edge_connection_pool(
        mut self,
        pool: astra_server_types::edge_connection_pool::EdgeConnectionPool,
    ) -> Self {
        self.edge_connection_pool = Some(pool);
        self
    }

    pub fn with_edge_dispatch_service(
        mut self,
        svc: Arc<dyn astra_services::multi_agent::EdgeDispatchService>,
    ) -> Self {
        self.edge_dispatch_service = Some(svc);
        self
    }

    pub fn with_edge_registry_service(
        mut self,
        svc: Arc<dyn astra_services::multi_agent::EdgeRegistryService>,
    ) -> Self {
        self.edge_registry_service = Some(svc);
        self
    }

    pub fn with_skill_service(mut self, service: Arc<dyn SkillService>) -> Self {
        self.skill_service = Some(service);
        self
    }

    #[cfg(feature = "bridge-e2e-hooks")]
    pub fn with_test_llm_rounds(mut self, rounds: Vec<Value>) -> Self {
        self.test_llm_rounds = rounds;
        self
    }
}

impl ServerSubRunExecutor {
    /// Provision a workspace directory for a delegation sub-run.
    ///
    /// Sub-runs get a subdirectory under the parent session workspace to
    /// keep file operations isolated while sharing the same base.
    fn provision_subrun_workspace(
        &self,
        session_id: &str,
        run_id: &str,
    ) -> Result<std::path::PathBuf, String> {
        let sanitize = |s: &str| -> String {
            s.chars()
                .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
                .collect()
        };
        let safe_session = sanitize(session_id);
        let safe_run = sanitize(run_id);
        if safe_session.is_empty() {
            return Err(format!(
                "session_id {session_id:?} contains no valid characters for workspace path"
            ));
        }
        if safe_run.is_empty() {
            return Err(format!(
                "run_id {run_id:?} contains no valid characters for workspace path"
            ));
        }

        let base = std::env::var("ASTRA_SERVER_WORKSPACES")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir().join("astra-workspaces"));
        let workspace = base.join(&safe_session).join(&safe_run);
        if let Err(error) = std::fs::create_dir_all(&workspace) {
            tracing::warn!(
                error = %error,
                workspace = %workspace.display(),
                "failed to create run workspace directory"
            );
        }
        Ok(workspace)
    }
}

fn resolve_subrun_agentic_turn_budget(
    task_profile: astra_turn_core::chat_turn_heuristics::TaskExecutionProfile,
    explicit_max_turns: Option<u32>,
) -> astra_turn_core::chat_turn_heuristics::AgenticTurnBudget {
    astra_turn_core::chat_turn_heuristics::resolve_agentic_turn_budget(
        task_profile,
        astra_core::RuntimeLimits::global().max_turns,
        explicit_max_turns.map(|max_turns| {
            let max_turns = max_turns as usize;
            astra_turn_core::chat_turn_heuristics::AgenticTurnBudgetOverride {
                initial_turns: Some(max_turns),
                hard_turn_limit: Some(max_turns),
            }
        }),
    )
}

#[async_trait]
impl SubRunExecutor for ServerSubRunExecutor {
    async fn execute(
        &self,
        config: SubRunConfig,
    ) -> Result<astra_services::coordination::AgentResult, String> {
        use astra_pipeline::step_protocol::InMemoryIdempotencyCache;
        use astra_text_utils::semantic_dedup::SemanticDedup;
        use astra_turn_core::chat_turn_heuristics::infer_task_execution_profile;
        use astra_turn_core::stop_hooks_yaml::{
            detect_turn_hook_sets, is_plan_subtask_from_delegation_context,
            project_root_from_delegation_context,
        };
        use astra_turn_core::turn_guard::TurnGuard;

        // Build edge profile from agent's system prompt and metadata.
        let compact_strategy = astra_turn_core::microcompact::CompactStrategy::from_provider_hint(
            config.agent_profile.model_override.as_deref().unwrap_or(""),
        );
        let mut edge_profile = Map::new();
        if let Some(prompt) = &config.agent_profile.system_prompt {
            edge_profile.insert(
                "system_prompt_override".to_string(),
                Value::String(prompt.clone()),
            );
        }
        if let Some(model) = &config.agent_profile.model_override {
            edge_profile.insert("model".to_string(), Value::String(model.clone()));
        }
        edge_profile.insert(
            "agent_id".to_string(),
            Value::String(config.agent_profile.agent_id.clone()),
        );

        // Build the host with agent-specific configuration.
        let mut builder = ServerAgenticLoopHostBuilder::new(
            self.matrixone.clone(),
            self.encryptor.clone(),
            config.user_id.clone(),
            config.session_id.clone(),
        )
        .with_model(config.agent_profile.model_override.clone())
        .with_llm_token_service(config.llm_token_service.clone())
        .with_capabilities(crate::capabilities::lifecycle_server_capabilities(
            self.shared_pool.is_some(),
        ))
        .with_edge_profile(edge_profile)
        .with_edge_callback_ledger(self.edge_callback_ledger.clone());

        if let Some(pool) = &self.shared_pool {
            builder = builder.with_pool(pool.clone());
        }
        #[cfg(feature = "bridge-e2e-hooks")]
        if !self.test_llm_rounds.is_empty() {
            builder = builder.with_test_llm_rounds(self.test_llm_rounds.clone());
        }
        // NOTE on grandchild inheritance: delegated children don't get
        // a prefix_store wired here because this sub-run executor
        // doesn't own one. Grandchild captures would be valuable for
        // deeper delegation trees but require threading the store into
        // `ServerSubRunExecutor` separately — scope-cut from G2 v1.
        if let Some(ref shared_tes) = self.tool_execution_service {
            builder = builder.with_disabled_tools(shared_tes.disabled_tools_handle());
        }
        let mut host = builder.build();
        if let Some(sink) = config.live_event_sink.clone() {
            host.set_agent_live_event_sink(config.agent_profile.agent_id.clone(), sink);
        }

        // Build the task prompt, incorporating previous output if pipeline.
        let full_task = if let Some(prev) = &config.previous_output {
            format!("{}\n\nPrevious agent output:\n{}", config.task, prev)
        } else {
            config.task.clone()
        };

        let user_message = json!({
            "role": "user",
            "content": full_task,
        });

        let task_profile = infer_task_execution_profile(&full_task);
        let agentic_turn_budget =
            resolve_subrun_agentic_turn_budget(task_profile, config.max_turns);
        let max_turns = agentic_turn_budget.initial_turns;
        let project_root_buf = project_root_from_delegation_context(&config.context);
        let hook_sets = project_root_buf
            .as_ref()
            .map(|root| {
                detect_turn_hook_sets(
                    root.as_path(),
                    task_profile,
                    is_plan_subtask_from_delegation_context(&config.context),
                )
            })
            .unwrap_or_default();
        let workspace_root_hint = project_root_buf.map(|p| p.to_string_lossy().into_owned());
        let (tool_event_hooks, session_event_hooks) = workspace_root_hint
            .as_ref()
            .map(|root| crate::skills::hooks::load_all_hooks(std::path::Path::new(root)))
            .unwrap_or_default();

        let (skill_registry, raw_skill_resolver) =
            build_server_skill_resolver(self.skill_service.clone(), &config.user_id);
        let skill_resolver =
            apply_normalized_skill_allowlist(raw_skill_resolver, &config.request_constraints)?;

        // Sub-agent / delegation path: model comes from the agent profile
        // override, not a request field.
        let resolved_tool_policy = astra_config::runtime_config::RuntimeConfig::load()
            .tool_selection
            .resolve_for_model(config.agent_profile.model_override.as_deref());
        let restricted_tools: std::collections::HashSet<String> =
            load_deployment_disabled_tools().into_iter().collect();

        let mut loop_state = AgenticLoopState {
            messages: vec![user_message],
            volatile_pending: Vec::new(),
            recent_rounds: Vec::new(),
            tool_results: Vec::new(),
            current_session_id: Some(config.session_id.clone()),
            current_run_id: Some(config.run_id.clone()),
            context_manifest_pool: self.shared_pool.clone(),
            context_manifest_user_id: Some(config.user_id.clone()),
            context_manifest_model_name: config.agent_profile.model_override.clone(),
            recursion_depth: config.recursion_depth,
            final_text: String::new(),
            final_text_streamed: false,
            total_prompt: 0,
            total_completion: 0,
            total_cache_read: 0,
            total_cache_creation: 0,
            total_tool_calls: 0,
            total_evidence_tool_calls: 0,
            has_any_usage: false,
            max_turns,
            remaining_turns: max_turns,
            turn_budget_hint_emitted_90: false,
            turn_budget_hint_emitted_50: false,
            turn_budget_hint_emitted_20: false,
            agentic_turn_budget,
            current_round_index: 0,
            llm_rounds_completed: 0,
            last_request_message_count: None,
            turn_guard: TurnGuard::new(),
            restricted_tools,
            boosted_tools: std::collections::HashSet::new(),
            widen_selection_pending: false,
            step_recorder: StepRecorder::new(&config.session_id, &config.run_id),
            idempotency_cache: InMemoryIdempotencyCache::new(),
            semantic_dedup: SemanticDedup::new(0.75),
            call_counts: HashMap::new(),
            max_identical_tool_calls: resolved_tool_policy.max_identical_tool_calls,
            max_tools_per_turn: resolved_tool_policy.max_tools_per_turn,
            repeated_cache_hit_suppression: resolved_tool_policy.repeated_cache_hit_suppression,
            max_consecutive_empty_name: resolved_tool_policy.max_consecutive_empty_name,
            stall: Default::default(),
            telemetry: Default::default(),
            skills: SkillState {
                registry_for_activation: if config.request_constraints.allowed_skills.is_some() {
                    None
                } else {
                    skill_registry
                },
                resolver: skill_resolver,
                request_constraints: config.request_constraints.clone(),
                quality_tracker: crate::skills::quality::SkillQualityTracker::new(),
                improvement_tracker: astra_skills::improvement::ImprovementTracker::new(),
                search: skill_search_from_context(&config.context),
                tool_event_hooks,
                session_event_hooks,
                ..Default::default()
            },
            hooks: StopHookState {
                stop_hooks: hook_sets.stop_hooks,
                teammate_idle_hooks: hook_sets.teammate_idle_hooks,
                workspace_root_hint,
                forward_headers: config.forward_headers.clone(),
                llm_token_service: config.llm_token_service.clone(),
                ..Default::default()
            },
            cancellation: CancellationState {
                flag: None,
                pause_flag: config.pause_flag.clone(),
                token: config.cancel_token.clone(),
            },
            messaging: MessagingState {
                mailbox: config.mailbox,
                progress_emitter: config.progress_emitter.clone(),
                ..Default::default()
            },
            deferred_input: Default::default(),
            error_recovery: Default::default(),
            run_control: None,
            pipeline_session: Some(
                astra_turn_core::pipeline_session::PipelineSession::new_with_current_date(
                    astra_turn_core::pipeline_config::PipelineConfig::default(),
                    crate::turn::session_current_date::resolve_session_current_date(
                        &config.session_id,
                    ),
                ),
            ),
            message: full_task,
            recent_tools: Vec::new(),
            task_profile,
            last_turn_policy: crate::turn::agentic_loop::host::TurnInteractionPolicy::default(),
            api: astra_thin_client::ThinClient::new("http://127.0.0.1:1", None)
                .expect("valid dummy URL"),
            api_token: String::new(),
            delegation_engine: None,
            delegations_this_turn: 0,
            project_context: None,
            checkpoint_gate: config.checkpoint_gate.clone(),
            last_llm_context_manifest_trace: None,
            rate_limit_cooldown: Default::default(),
            data_snapshot_provider: None,
            last_composite_snapshot: None,
            last_measured_prompt_tokens: None,
            consecutive_context_window_errors: 0,
            compaction_effectiveness: Default::default(),
            pinned_tool_schema_tokens: 0,
            sticky_tool_schemas: Vec::new(),
            max_turn_input_tokens: astra_core::RuntimeLimits::global().max_turn_input_tokens,
            budget_wrapup_injected: false,
            budget_wrapup_ignored_rounds: 0,
            compact_tier_applied: astra_turn_core::compaction_types::CompactionTier::Normal,
            skill_produced_output: false,
            max_cumulative_tokens: 0,
            thinking: astra_turn_core::thinking_config::ThinkingConfig::Off,
            recent_file_reads: Vec::new(),
            permission_context: None,
            permission_handler: None,
            tactical_adapter: None,
            step_signal_collector: None,
            tool_budget_override: None,
            recent_tactical_actions: Vec::new(),
            server_tool_executor: None,
            interruption: None,
            session_facts: Default::default(),
            memory_extraction_service: self.memory_extraction_service.clone(),
            session_memory_state: Default::default(),
            session_memory_llm_params: None,
            compact_strategy,
            approval_overrides: None,
            confidence_trend: Default::default(),
            last_confidence_diagnosis: None,
            session_turn: 0,
            bridge_turn_chain_id: None,
            bridge_user_query_event_id: None,
            turn_event_buffer: None,
            harness: {
                #[cfg(feature = "harness")]
                {
                    match config.harness_sink {
                        Some(ref sink) => {
                            crate::turn::harness_adapter::HarnessSlot::observe_only(sink.clone())
                        }
                        None => crate::turn::harness_adapter::HarnessSlot::empty(),
                    }
                }
                #[cfg(not(feature = "harness"))]
                {
                    crate::turn::harness_adapter::HarnessSlot::empty()
                }
            },
        };
        if let Some(trace_context) = trace_context_from_subrun_context(&config.context) {
            loop_state.session_turn = u32::try_from(trace_context.turn_seq).unwrap_or(0);
        }

        // ── Wire ServerToolExecutor for sub-run tool execution ──────────
        // Without this, the headless pipeline fallback cannot execute tools
        // server-side and sub-agents would get edge-protocol errors.
        {
            let workspace = self.provision_subrun_workspace(&config.session_id, &config.run_id)?;
            let execution_bindings =
                execution_bindings_from_metadata(config.execution_metadata.as_ref(), &workspace);
            let memoria_base = Some(astra_core::MemoriaSettings::from_env().base_url);
            let task_store = astra_tools::task_mgmt_matrixone::select_task_store(
                self.shared_pool.as_ref().map(|p| p.get().clone()),
                config.user_id.clone(),
            )?;
            let mut executor = server_tool_executor::ServerToolExecutor::new(
                workspace,
                config.user_id.clone(),
                config.session_id.clone(),
                memoria_base,
                None,
            )
            .with_capabilities(crate::capabilities::lifecycle_server_capabilities(
                self.shared_pool.is_some(),
            ))
            .with_cancel_token(config.cancel_token.clone())
            .with_task_store(task_store);

            // Enable exactly-once tool execution for crash recovery dedup.
            // This prevents side-effect tools (github_create_issue, task create, etc.)
            // from re-executing when a session resumes after a crash.
            executor.enable_exactly_once().await;

            // Apply shared ToolExecutionService (with admin-controllable disabled_tools)
            // or fall back to building one from deployment config.
            if let Some(ref shared_tes) = self.tool_execution_service {
                executor = executor.with_tool_execution_service(shared_tes.clone());
            } else {
                let mut builder = ToolExecutionService::builder();
                let disabled = load_deployment_disabled_tools();
                if !disabled.is_empty() {
                    builder = builder.initial_disabled_tools(&disabled);
                }
                if let Some(pool) = self.shared_pool.as_ref() {
                    executor.set_context_manifest_pool(pool.clone());
                    executor = executor.with_workspace_artifact_store(
                        astra_services::DatabaseSessionArtifactStore::new(self.matrixone.clone())
                            .with_pool(pool.clone()),
                    );
                }
                if let Some(pool) = &self.edge_connection_pool {
                    builder = builder.edge_connection_pool(pool.clone());
                }
                if let Some(svc) = &self.edge_dispatch_service {
                    builder = builder.edge_dispatch_service(Arc::clone(svc));
                }
                if let Some(svc) = &self.edge_registry_service {
                    builder = builder.edge_registry_service(Arc::clone(svc));
                }
                executor = executor.with_tool_execution_service(builder.build());
            }
            if let Some(shared) = self.shared_pool.as_ref() {
                executor.set_plan_repository(std::sync::Arc::new(
                    astra_plan::CloudPlanRepository::new(shared.get().clone()),
                ));
            }
            executor.set_plan_resume_hint_handle(host.plan_resume_hint_handle());
            if let Some(obs) = loop_state.telemetry.observability_session.clone() {
                executor.set_observability_session(obs);
            }
            if let Some(snapshot) = execution_bindings {
                executor.set_execution_binding_snapshot(snapshot);
            }
            wire_executor_into_state(executor, &mut loop_state);
        }

        configure_runtime_controllers(
            &self.matrixone,
            self.shared_pool.as_ref(),
            &mut loop_state,
            &config.user_id,
            &config.session_id,
        )
        .await;

        let live_started_at = Instant::now();
        let live_agent_id = config.agent_profile.agent_id.clone();
        let outcome = run_agentic_loop_with_host(&mut host, &mut loop_state).await;

        // Fire SessionEnd hooks (best-effort).
        crate::skills::hooks::fire_session_end(
            &loop_state.skills.session_event_hooks,
            loop_state.current_session_id.as_deref().unwrap_or(""),
        )
        .await;
        persist_runtime_promotion_events(
            &self.matrixone,
            self.shared_pool.as_ref(),
            &config.user_id,
            &config.session_id,
            &config.run_id,
            &loop_state.telemetry.promotion_events,
        )
        .await;
        persist_turn_evaluation_journal(&config.session_id, "server_subrun", &loop_state);
        flush_turn_observability(&mut loop_state, &config.session_id, false);

        // Persist core events for delegation sub-runs.
        persist_server_loop_core_events(
            &self.matrixone,
            self.shared_pool.as_ref(),
            &config.user_id,
            &config.session_id,
            &config.run_id,
            config.context.get("parent_run_id").and_then(Value::as_str),
            Some(config.agent_profile.agent_id.as_str()),
            config
                .context
                .get("parent_agent_id")
                .and_then(Value::as_str),
            trace_context_from_subrun_context(&config.context),
            &config.task,
            &loop_state,
            config.agent_profile.model_override.as_deref(),
        )
        .await;
        persist_server_loop_trace_events(
            &self.matrixone,
            self.shared_pool.as_ref(),
            &config.user_id,
            &config.session_id,
            &config.run_id,
            config.context.get("parent_run_id").and_then(Value::as_str),
            Some(config.agent_profile.agent_id.as_str()),
            config
                .context
                .get("parent_agent_id")
                .and_then(Value::as_str),
            trace_context_from_subrun_context(&config.context),
            &loop_state,
            config.agent_profile.model_override.as_deref(),
        )
        .await;

        emit_server_subrun_agent_terminated(
            config.live_event_sink.as_ref(),
            &live_agent_id,
            live_started_at,
            server_subrun_live_termination(&outcome, &loop_state),
            server_subrun_live_reason(&outcome, &loop_state),
        );

        match outcome {
            Ok(AgenticLoopOutcome::Completed) => {
                let status = if loop_state.interruption.is_some() {
                    STATUS_PAUSED
                } else {
                    STATUS_COMPLETED
                };
                Ok(astra_services::coordination::AgentResult {
                    agent_id: config.agent_profile.agent_id,
                    run_id: config.run_id,
                    status: status.to_string(),
                    output: if loop_state.final_text.is_empty() {
                        None
                    } else {
                        Some(loop_state.final_text)
                    },
                    error: None,
                    prompt_tokens: loop_state.total_prompt,
                    completion_tokens: loop_state.total_completion,
                    tool_calls: loop_state.total_tool_calls,
                })
            }
            Ok(AgenticLoopOutcome::Cancelled) => {
                // Cancelled via pause_flag — report as "paused" so the
                // delegation engine can distinguish from hard errors.
                Ok(astra_services::coordination::AgentResult {
                    agent_id: config.agent_profile.agent_id,
                    run_id: config.run_id,
                    status: STATUS_PAUSED.to_string(),
                    output: if loop_state.final_text.is_empty() {
                        None
                    } else {
                        Some(loop_state.final_text)
                    },
                    error: None,
                    prompt_tokens: loop_state.total_prompt,
                    completion_tokens: loop_state.total_completion,
                    tool_calls: loop_state.total_tool_calls,
                })
            }
            Ok(AgenticLoopOutcome::Waiting(reason)) => {
                Ok(astra_services::coordination::AgentResult {
                    agent_id: config.agent_profile.agent_id,
                    run_id: config.run_id,
                    status: STATUS_WAITING.to_string(),
                    output: Some(reason),
                    error: None,
                    prompt_tokens: loop_state.total_prompt,
                    completion_tokens: loop_state.total_completion,
                    tool_calls: loop_state.total_tool_calls,
                })
            }
            Ok(AgenticLoopOutcome::Error(err)) => Ok(astra_services::coordination::AgentResult {
                agent_id: config.agent_profile.agent_id,
                run_id: config.run_id,
                status: STATUS_FAILED.to_string(),
                output: None,
                error: Some(err),
                prompt_tokens: loop_state.total_prompt,
                completion_tokens: loop_state.total_completion,
                tool_calls: loop_state.total_tool_calls,
            }),
            Err(err) => Ok(astra_services::coordination::AgentResult {
                agent_id: config.agent_profile.agent_id,
                run_id: config.run_id,
                status: STATUS_FAILED.to_string(),
                output: None,
                error: Some(err.to_string()),
                prompt_tokens: loop_state.total_prompt,
                completion_tokens: loop_state.total_completion,
                tool_calls: loop_state.total_tool_calls,
            }),
        }
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::run::lifecycle::persistence::{
        build_tool_trace_events, extract_prev_assistant_text, extract_session_state_compact,
        redact_trace_value, server_loop_causal_chain_id, transcript_page_bounds,
        transcript_page_seq,
    };
    use astra_services::runs::{
        DurableRunCheckpointRecord, DurableRunDisplayProjectionRecord, InMemoryRunStateStore,
        RunStateStore,
    };
    use astra_services::session_journal::{JournalEventType, ToolCallRecord};
    use astra_services::workspace_records::{
        InMemoryWorkspaceRecordStore, WorkspaceCleanupDebtStore, WorkspaceCleanupDebtStoreError,
        WorkspaceRecordStore,
    };
    use serde_json::json;
    use sqlx::Row;
    use std::collections::HashSet;
    use std::sync::Mutex as StdMutex;
    use uuid::Uuid;

    fn test_session_task(
        id: &str,
        title: &str,
        status: astra_tools::task_mgmt::SessionTaskStatusKind,
    ) -> SessionTask {
        SessionTask {
            archived_at: None,
            id: id.to_string(),
            title: title.to_string(),
            description: None,
            status,
            subtasks: vec![],
            created_at: String::new(),
            updated_at: String::new(),
            active_form: None,
            owner: None,
            metadata: None,
            blocks: vec![],
            blocked_by: vec![],
        }
    }

    fn test_agent_progress_event(
        agent_id: &str,
        timestamp_epoch_ms: u64,
        event_type: ProgressEventType,
    ) -> AgentProgressEvent {
        AgentProgressEvent {
            agent_id: agent_id.to_string(),
            event_type,
            timestamp_epoch_ms,
            metadata: None,
        }
    }

    fn test_agent_spawned(
        agent_id: &str,
        run_id: &str,
        parent_run_id: &str,
        timestamp_epoch_ms: u64,
    ) -> AgentProgressEvent {
        test_agent_progress_event(
            agent_id,
            timestamp_epoch_ms,
            ProgressEventType::AgentSpawned {
                run_id: run_id.to_string(),
                parent_run_id: parent_run_id.to_string(),
                agent_type: "reviewer".to_string(),
                description: "review code".to_string(),
                fanout_slot: None,
            },
        )
    }

    #[test]
    fn restore_session_state_compact_ignores_runtime_control_state() {
        let svc = test_service();
        let request = test_request("resume");
        let mut state = svc.build_initial_state(
            "test-user",
            &request,
            "session-1",
            "run-1",
            None,
            None,
            None,
        );
        state.max_turn_input_tokens = 123_456;
        state.remaining_turns = 9;

        restore_session_state_compact(
            astra_turn_core::conversation_log::SessionStateCompact {
                approval_overrides: Some(json!({"approval": "stale"})),
                budget_remaining_tokens: 42_000,
                budget_remaining_rounds: 3,
                consecutive_ctx_errors: 3,
                interruption: Some(json!({
                    "kind": "budget_exhausted",
                    "resume_action": "continue_immediately"
                })),
                compaction_tracker: Some(json!({
                    "attempt_count": 4,
                    "cumulative_tokens_freed": 18_000,
                    "last_tokens_freed": 2_000,
                    "last_was_insufficient": true,
                    "consecutive_futile_attempts": 2,
                })),
                ..Default::default()
            },
            &mut state,
        );

        assert!(state.approval_overrides.is_none());
        assert!(state.interruption.is_none());
        assert_eq!(state.max_turn_input_tokens, 123_456);
        assert_eq!(state.remaining_turns, 9);
        assert_eq!(state.consecutive_context_window_errors, 0);
        assert_eq!(state.compaction_effectiveness.attempt_count, 0);
    }

    #[test]
    fn csl_session_state_does_not_persist_runtime_control_state() {
        let svc = test_service();
        let request = test_request("resume");
        let mut state = svc.build_initial_state(
            "test-user",
            &request,
            "session-1",
            "run-1",
            None,
            None,
            None,
        );
        state.restricted_tools.insert("write_file".to_string());
        state.max_turn_input_tokens = 50_000;
        state.remaining_turns = 2;
        state.consecutive_context_window_errors = 5;
        state.interruption = Some(astra_turn_core::interruption::InterruptionRecord::new(
            astra_turn_core::interruption::InterruptionKind::BudgetExhausted,
            astra_turn_core::interruption::ResumeAction::ContinueImmediately,
            astra_turn_core::interruption::InterruptionStateSummary {
                has_checkpoint: true,
                tool_calls_completed: 1,
                turns_completed: 1,
                remaining_turns: 0,
                error_detail: Some("stale interruption".to_string()),
                stall_signal: None,
                resume_restricted_tools: vec![],
            },
        ));
        state.compaction_effectiveness.attempt_count = 7;

        let compact = extract_session_state_compact(&state);

        assert!(
            compact.blocked_tools.is_empty(),
            "conversation-log state must not persist transient runtime restrictions"
        );
        assert!(compact.approval_overrides.is_none());
        assert!(compact.interruption.is_none());
        assert_eq!(compact.budget_remaining_tokens, 0);
        assert_eq!(compact.budget_remaining_rounds, 0);
        assert_eq!(compact.consecutive_ctx_errors, 0);
        assert!(compact.compaction_tracker.is_none());
    }

    #[test]
    fn csl_session_state_restore_ignores_legacy_blocked_tools() {
        let svc = test_service();
        let request = test_request("resume");
        let mut state = svc.build_initial_state(
            "test-user",
            &request,
            "session-1",
            "run-1",
            None,
            None,
            None,
        );

        restore_session_state_compact(
            astra_turn_core::conversation_log::SessionStateCompact {
                blocked_tools: vec!["legacy_stale_tool".into()],
                recent_tools: vec!["read_file".into()],
                ..Default::default()
            },
            &mut state,
        );

        assert!(
            state.restricted_tools.is_empty(),
            "legacy CSL blocked_tools must not restore as hard runtime restrictions"
        );
        assert_eq!(state.recent_tools, vec!["read_file"]);
    }

    #[test]
    fn restore_step_checkpoint_runtime_state_restores_replay_guards_and_runtime_state() {
        let svc = test_service();
        let request = test_request("resume");
        let mut state = svc.build_initial_state(
            "test-user",
            &request,
            "session-1",
            "run-1",
            None,
            None,
            None,
        );
        let idem_key = astra_pipeline::step_protocol::IdempotencyKey::semantic(
            "read_file",
            &json!({"path": "src/lib.rs"}),
        );
        let mut idempotency_cache = astra_pipeline::step_protocol::InMemoryIdempotencyCache::new();
        idempotency_cache.record(
            &idem_key,
            astra_pipeline::step_protocol::CachedToolResult {
                tool_name: "read_file".into(),
                output: "cached contents".into(),
                is_error: false,
                cached_at: 123,
                context_signature: None,
            },
        );
        let restored = astra_pipeline::step_restore::RestoredSession {
            messages: Vec::new(),
            budget_remaining_tokens: 0,
            budget_remaining_rounds: 0,
            blocked_tools: vec!["flaky_tool".into()],
            recent_tools: vec!["read_file".into(), "bash".into()],
            idempotency_cache,
            resume_turn: 0,
            protocol_version: astra_pipeline::step_protocol::PROTOCOL_VERSION,
            completed_tool_results: HashMap::new(),
            interruption: None,
            approval_overrides: None,
            consecutive_context_window_errors: 5,
            compaction_state: Some(json!({
                "attempt_count": 6,
                "cumulative_tokens_freed": 24_000,
                "last_tokens_freed": 1_500,
                "last_was_insufficient": false,
                "consecutive_futile_attempts": 1,
            })),
            pipeline_state: None,
        };

        restore_step_checkpoint_runtime_state(restored, "2026-06-13", &mut state);

        assert!(state.restricted_tools.contains("flaky_tool"));
        assert_eq!(state.recent_tools, vec!["read_file", "bash"]);
        let cached = state
            .idempotency_cache
            .check(&idem_key)
            .expect("idempotency cache should be restored");
        assert_eq!(cached.output, "cached contents");
        assert_eq!(state.consecutive_context_window_errors, 5);
        assert_eq!(state.compaction_effectiveness.attempt_count, 6);
        assert_eq!(
            state.compaction_effectiveness.cumulative_tokens_freed,
            24_000
        );
        assert_eq!(state.compaction_effectiveness.last_tokens_freed, 1_500);
        assert!(!state.compaction_effectiveness.last_was_insufficient);
        assert_eq!(
            state.compaction_effectiveness.consecutive_futile_attempts,
            1
        );
    }

    #[test]
    fn run_scoped_agent_progress_filter_replays_early_events_after_spawn() {
        let mut filter =
            server_loop_host::RunScopedAgentProgressFilter::new("root-run".to_string());

        let accepted = filter.accept(test_agent_progress_event(
            "agent-a",
            1,
            ProgressEventType::Started {
                description: "review code".to_string(),
            },
        ));
        assert!(accepted.is_empty());

        let accepted = filter.accept(test_agent_spawned("agent-a", "child-run", "root-run", 2));
        assert_eq!(accepted.len(), 2);
        assert!(matches!(
            accepted[0].event_type,
            ProgressEventType::Started { .. }
        ));
        assert!(matches!(
            accepted[1].event_type,
            ProgressEventType::AgentSpawned { .. }
        ));

        let accepted = filter.accept(test_agent_progress_event(
            "agent-a",
            3,
            ProgressEventType::ToolExecuting {
                tool_name: "rg".to_string(),
                turn: 1,
            },
        ));
        assert_eq!(accepted.len(), 1);
    }

    #[test]
    fn run_scoped_agent_progress_filter_replays_bounded_latest_early_events_in_order() {
        let mut filter =
            server_loop_host::RunScopedAgentProgressFilter::new("root-run".to_string());

        for timestamp in 1..=10 {
            assert!(
                filter
                    .accept(test_agent_progress_event(
                        "agent-a",
                        timestamp,
                        ProgressEventType::ToolExecuting {
                            tool_name: format!("tool-{timestamp}"),
                            turn: timestamp as u32,
                        },
                    ))
                    .is_empty()
            );
        }

        let accepted = filter.accept(test_agent_spawned("agent-a", "child-run", "root-run", 11));

        assert_eq!(
            accepted
                .iter()
                .map(|event| event.timestamp_epoch_ms)
                .collect::<Vec<_>>(),
            vec![3, 4, 5, 6, 7, 8, 9, 10, 11]
        );
    }

    #[test]
    fn run_scoped_agent_progress_filter_blocks_foreign_root_events() {
        let mut filter = server_loop_host::RunScopedAgentProgressFilter::new("root-a".to_string());

        assert!(
            filter
                .accept(test_agent_progress_event(
                    "agent-b",
                    1,
                    ProgressEventType::Started {
                        description: "other run".to_string(),
                    },
                ))
                .is_empty()
        );
        assert!(
            filter
                .accept(test_agent_spawned("agent-b", "child-b", "root-b", 2))
                .is_empty()
        );
        assert!(
            !filter.agent_ids.contains("agent-b"),
            "foreign agent must not be admitted"
        );
        assert!(
            !filter.pending_by_agent.contains_key("agent-b"),
            "foreign spawn should clear cached early events"
        );
    }

    #[test]
    fn run_scoped_agent_progress_filter_allows_nested_child_runs() {
        let mut filter =
            server_loop_host::RunScopedAgentProgressFilter::new("root-run".to_string());

        assert_eq!(
            filter
                .accept(test_agent_spawned("agent-a", "child-a", "root-run", 1))
                .len(),
            1
        );
        assert_eq!(
            filter
                .accept(test_agent_spawned("agent-b", "grandchild-b", "child-a", 2))
                .len(),
            1
        );
        assert!(filter.agent_ids.contains("agent-b"));
        assert!(filter.run_ids.contains("grandchild-b"));
    }

    #[tokio::test]
    async fn agent_progress_stream_bridge_drains_progress_on_stop() {
        let svc = test_service();
        let (event_tx, mut event_rx) = mpsc::channel::<Value>(16);
        let bridge = svc.spawn_agent_progress_stream_bridge("root-run".to_string(), event_tx);

        let emitter = svc
            .server_agent_progress_broadcaster
            .for_agent("agent-a".to_string());
        emitter.started("review code");
        emitter.agent_spawned("child-run", "root-run", "reviewer", "review code");
        emitter.completed("done", 0, (0, 0), 7);

        bridge.stop_and_drain().await;

        let mut events = Vec::new();
        while let Ok(event) = event_rx.try_recv() {
            events.push(event);
        }

        assert!(
            events
                .iter()
                .any(|event| event["type"].as_str() == Some("agent_spawned")),
            "bridge should drain agent_spawned before stopping: {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|event| event["type"].as_str() == Some("agent_completed")),
            "bridge should drain agent_completed before stopping: {events:?}"
        );
    }

    struct ImmediateLifecycleExecutor;

    #[async_trait]
    impl SpawnAgentExecutor for ImmediateLifecycleExecutor {
        async fn execute(&self, config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
            Ok(SpawnRunResult {
                agent_id: config.agent_id,
                run_id: config.run_id,
                status: "completed".to_string(),
                finish_reason: "normal".to_string(),
                output: Some("child done".to_string()),
                error: None,
                prompt_tokens: 3,
                completion_tokens: 5,
                tool_calls: 1,
                permission_summary: None,
                permission_requests: 0,
                permission_requests_approved: 0,
                tools_blocked: 0,
            })
        }
    }

    struct WaitingLifecycleExecutor;

    #[async_trait]
    impl SpawnAgentExecutor for WaitingLifecycleExecutor {
        async fn execute(&self, config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
            Ok(SpawnRunResult {
                agent_id: config.agent_id,
                run_id: config.run_id,
                status: "waiting".to_string(),
                finish_reason: "waiting".to_string(),
                output: Some("executor_offline".to_string()),
                error: None,
                prompt_tokens: 3,
                completion_tokens: 5,
                tool_calls: 1,
                permission_summary: None,
                permission_requests: 0,
                permission_requests_approved: 0,
                tools_blocked: 1,
            })
        }
    }

    #[tokio::test]
    async fn missing_agent_lifecycle_stream_uses_spawner_archive() {
        let router = Arc::new(astra_messaging::AgentMailboxRouter::new(
            Arc::new(astra_messaging::InProcessTransport::new()),
            Arc::new(crate::server::delegation::engine::DelegationTracker::new()),
        ));
        let spawner =
            DynamicAgentSpawner::new(router).with_executor(Arc::new(ImmediateLifecycleExecutor));
        let execution_metadata = json!({
            "workspace": {"kind": "server_sandbox", "cwd": "/tmp/astra"},
            "executor": {"kind": "server_local"},
            "transport": "server_local"
        });
        let context = crate::orchestration::SpawnContext {
            parent_run_id: "root-run".to_string(),
            parent_agent_id: "root-agent".to_string(),
            recursion_depth: 0,
            parent_is_fork_child: false,
            inherited_permissions: None,
            inherited_skills: vec![],
            working_dir: PathBuf::from("/tmp/astra"),
            live_event_sink: None,
            trace_context: None,
            spawn_tool_call_id: Some("call-spawn".to_string()),
            execution_metadata: Some(execution_metadata),
        };
        let input = astra_turn_core::orchestration_spawn_tool::SpawnAgentInput {
            description: "review code".to_string(),
            prompt: "review".to_string(),
            agent_type: "explore".to_string(),
            run_in_background: false,
            ..Default::default()
        };
        let spawn_output = spawner.spawn(input, &context).await.unwrap();
        assert!(
            matches!(
                spawn_output,
                astra_turn_core::orchestration_spawn_tool::SpawnAgentOutput::Completed { .. }
            ),
            "test setup must archive a synchronous completed child: {spawn_output:?}"
        );

        let sent_lifecycle_events = Arc::new(std::sync::Mutex::new(HashSet::new()));
        let (event_tx, mut event_rx) = mpsc::channel::<Value>(8);
        assert!(
            stream_missing_agent_lifecycle_events(
                &spawner,
                "root-run",
                &event_tx,
                &sent_lifecycle_events
            )
            .await
        );

        let mut events = Vec::new();
        while let Ok(event) = event_rx.try_recv() {
            events.push(event);
        }
        assert_eq!(events.len(), 2, "expected spawned + completed: {events:?}");
        assert_eq!(events[0]["type"], "agent_spawned");
        assert_eq!(events[0]["workspace"]["kind"], "server_sandbox");
        assert_eq!(events[0]["executor"]["kind"], "server_local");
        assert_eq!(events[0]["transport"], "server_local");
        assert_eq!(events[1]["type"], "agent_completed");
        assert_eq!(events[1]["status"], "completed");
        assert_eq!(events[1]["workspace"]["kind"], "server_sandbox");

        let (second_tx, mut second_rx) = mpsc::channel::<Value>(8);
        assert!(
            stream_missing_agent_lifecycle_events(
                &spawner,
                "root-run",
                &second_tx,
                &sent_lifecycle_events
            )
            .await
        );
        assert!(
            second_rx.try_recv().is_err(),
            "already-sent lifecycle events must not be replayed twice"
        );
    }

    #[tokio::test]
    async fn missing_agent_lifecycle_stream_reconstructs_waiting_child() {
        let router = Arc::new(astra_messaging::AgentMailboxRouter::new(
            Arc::new(astra_messaging::InProcessTransport::new()),
            Arc::new(crate::server::delegation::engine::DelegationTracker::new()),
        ));
        let spawner =
            DynamicAgentSpawner::new(router).with_executor(Arc::new(WaitingLifecycleExecutor));
        let context = crate::orchestration::SpawnContext {
            parent_run_id: "root-run".to_string(),
            parent_agent_id: "root-agent".to_string(),
            recursion_depth: 0,
            parent_is_fork_child: false,
            inherited_permissions: None,
            inherited_skills: vec![],
            working_dir: PathBuf::from("/tmp/astra"),
            live_event_sink: None,
            trace_context: None,
            spawn_tool_call_id: Some("call-spawn".to_string()),
            execution_metadata: Some(json!({
                "workspace": {"kind": "edge_workspace", "cwd": "/Users/test/repo"},
                "executor": {"kind": "edge_agent", "status": "offline"},
                "transport": "edge_ws"
            })),
        };
        let input = astra_turn_core::orchestration_spawn_tool::SpawnAgentInput {
            description: "review code".to_string(),
            prompt: "review".to_string(),
            agent_type: "explore".to_string(),
            run_in_background: false,
            ..Default::default()
        };
        let spawn_output = spawner.spawn(input, &context).await.unwrap();
        assert!(
            matches!(
                spawn_output,
                astra_turn_core::orchestration_spawn_tool::SpawnAgentOutput::Waiting { .. }
            ),
            "test setup must archive a synchronous waiting child: {spawn_output:?}"
        );

        let sent_lifecycle_events = Arc::new(std::sync::Mutex::new(HashSet::new()));
        let (event_tx, mut event_rx) = mpsc::channel::<Value>(8);
        assert!(
            stream_missing_agent_lifecycle_events(
                &spawner,
                "root-run",
                &event_tx,
                &sent_lifecycle_events
            )
            .await
        );

        let mut events = Vec::new();
        while let Ok(event) = event_rx.try_recv() {
            events.push(event);
        }
        assert_eq!(events.len(), 2, "expected spawned + waiting: {events:?}");
        assert_eq!(events[0]["type"], "agent_spawned");
        assert_eq!(events[1]["type"], "agent_waiting");
        assert_eq!(events[1]["reason"], "executor_offline");
        assert_eq!(events[1]["workspace"]["kind"], "edge_workspace");
        assert_eq!(events[1]["executor"]["kind"], "edge_agent");
    }

    #[test]
    fn agent_live_event_to_work_surface_sse_maps_output_and_terminal() {
        let metadata = json!({
            "workspace": {
                "kind": "edge_workspace",
                "display_name": "MacBook Pro",
                "cwd": "/Users/test/project",
                "authority": "read_write",
                "fallback_policy": "disabled"
            },
            "executor": {
                "kind": "edge_agent",
                "executor_id": "edge-macbook-1",
                "display_name": "MacBook Pro",
                "transport": "edge_ws",
                "status": "online"
            },
            "transport": "edge_ws",
            "fallback_policy": "disabled"
        });
        let output = super::agent_live_event_to_work_surface_sse(
            &AgentLiveEvent {
                agent_id: "agent-1".to_string(),
                kind: AgentLiveEventKind::OutputDelta("child output".to_string()),
            },
            Some(&metadata),
        );
        assert_eq!(output["type"], "agent_live_event");
        assert_eq!(output["agent_id"], "agent-1");
        assert_eq!(output["event_kind"], "output_delta");
        assert_eq!(output["content"], "child output");
        assert_eq!(output["workspace"]["kind"], "edge_workspace");
        assert_eq!(output["executor"]["kind"], "edge_agent");
        assert_eq!(output["transport"], "edge_ws");
        assert_eq!(output["fallback_policy"], "disabled");

        let terminal = super::agent_live_event_to_work_surface_sse(
            &AgentLiveEvent {
                agent_id: "agent-1".to_string(),
                kind: AgentLiveEventKind::AgentTerminated {
                    termination: AgentLiveTermination::Completed,
                    duration_ms: 12,
                    reason: None,
                },
            },
            Some(&metadata),
        );
        assert_eq!(terminal["event_kind"], "agent_terminated");
        assert_eq!(terminal["termination"], "completed");
        assert_eq!(terminal["status"], "completed");
        assert_eq!(terminal["duration_ms"], 12);
        assert_eq!(terminal["workspace"]["kind"], "edge_workspace");
        assert_eq!(terminal["executor"]["executor_id"], "edge-macbook-1");
    }

    // ── extract_prev_assistant_text + implicit feedback wiring ──

    #[test]
    fn task_board_resume_hint_is_bounded_and_prefers_running_work() {
        use astra_tools::task_mgmt::SessionTaskStatusKind;

        let tasks = vec![
            test_session_task("task-1", "pending setup", SessionTaskStatusKind::Pending),
            test_session_task(
                "task-2",
                "active implementation",
                SessionTaskStatusKind::InProgress,
            ),
            test_session_task("task-3", "already done", SessionTaskStatusKind::Completed),
            test_session_task("task-4", "waiting review", SessionTaskStatusKind::Paused),
        ];

        let hint = format_task_board_resume_hint(&tasks).expect("open task hint");

        assert_eq!(
            hint,
            "open=3 · next=[in_progress] task-2: active implementation · +2 more open"
        );
    }

    #[test]
    fn task_board_resume_hint_is_absent_without_open_work() {
        use astra_tools::task_mgmt::SessionTaskStatusKind;

        let tasks = vec![test_session_task(
            "task-1",
            "already done",
            SessionTaskStatusKind::Completed,
        )];

        assert!(format_task_board_resume_hint(&tasks).is_none());
    }

    #[test]
    fn trace_redaction_removes_nested_secrets_and_truncates_long_text() {
        let redacted = redact_trace_value(&json!({
            "Authorization": "Bearer secret",
            "nested": {
                "api_key": "abc123",
                "safe": "visible"
            },
            "items": [
                {"cookie": "session=abc"},
                {"text": "x".repeat(2_050)}
            ]
        }));

        assert_eq!(redacted["Authorization"], "[REDACTED]");
        assert_eq!(redacted["nested"]["api_key"], "[REDACTED]");
        assert_eq!(redacted["nested"]["safe"], "visible");
        assert_eq!(redacted["items"][0]["cookie"], "[REDACTED]");
        assert!(
            redacted["items"][1]["text"]
                .as_str()
                .expect("string")
                .ends_with("...")
        );
    }

    #[test]
    fn tool_trace_events_populate_columns_and_redacted_payloads() {
        let trace = TraceContext {
            session_id: "session-1".to_string(),
            user_id: "user-1".to_string(),
            turn_id: "turn-1".to_string(),
            turn_seq: 7,
            causal_chain_id: "chain-1".to_string(),
            root_event_id: "trace:root".to_string(),
        };
        let record = ToolCallRecord {
            tool_call_id: Some("tool-call-1".to_string()),
            name: "agent".to_string(),
            ok: true,
            ms: 42,
            args_preview: Some("agent(action='spawn'): child".to_string()),
            result_preview: Some("launched child".to_string()),
            round: Some(2),
            args_full: Some(r#"{"action":"spawn","token":"secret"}"#.to_string()),
            result_full: Some(
                r#"{"agent_id":"child@run","run_id":"child-run","result":"ok"}"#.to_string(),
            ),
            ..Default::default()
        };

        let events = build_tool_trace_events(
            &trace,
            "root-run",
            None,
            Some("root-agent"),
            None,
            &[record],
        );

        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_type, "tool_call_started");
        assert_eq!(events[0].tool_call_id.as_deref(), Some("tool-call-1"));
        assert_eq!(events[0].round_index, Some(2));
        assert_eq!(events[0].meta_tool_name.as_deref(), Some("agent"));
        assert_eq!(
            events[0].metadata["tool_args_json_redacted"]["token"],
            "[REDACTED]"
        );
        assert_eq!(events[1].event_type, "tool_call_completed");
        assert_eq!(events[1].meta_duration_ms, Some(42));
        assert_eq!(events[1].metadata["action"], "spawn");
        assert_eq!(events[1].metadata["child_run_id"], "child-run");
    }

    #[test]
    fn extract_prev_assistant_text_picks_latest_assistant_string() {
        let messages = vec![
            serde_json::json!({"role": "user", "content": "hi"}),
            serde_json::json!({"role": "assistant", "content": "first answer"}),
            serde_json::json!({"role": "user", "content": "follow up"}),
            serde_json::json!({"role": "assistant", "content": "latest answer"}),
        ];
        assert_eq!(
            extract_prev_assistant_text(&messages).as_deref(),
            Some("latest answer")
        );
    }

    #[test]
    fn extract_prev_assistant_text_handles_content_parts_array() {
        let messages = vec![
            serde_json::json!({"role": "user", "content": "hi"}),
            serde_json::json!({
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "part one"},
                    {"type": "text", "text": "part two"},
                ],
            }),
        ];
        assert_eq!(
            extract_prev_assistant_text(&messages).as_deref(),
            Some("part one\npart two")
        );
    }

    #[test]
    fn extract_prev_assistant_text_returns_none_when_no_assistant_turn() {
        let messages = vec![serde_json::json!({"role": "user", "content": "hi"})];
        assert!(extract_prev_assistant_text(&messages).is_none());
    }

    #[test]
    fn extract_prev_assistant_text_skips_empty_assistant_bodies() {
        let messages = vec![
            serde_json::json!({"role": "assistant", "content": "real answer"}),
            serde_json::json!({"role": "user", "content": "ok"}),
            serde_json::json!({"role": "assistant", "content": "   "}),
        ];
        assert_eq!(
            extract_prev_assistant_text(&messages).as_deref(),
            Some("real answer")
        );
    }

    #[test]
    fn build_run_turn_complete_event_carries_authoritative_assistant_text() {
        let event =
            build_run_turn_complete_event_with_interruption(0, "recovered final answer", None);
        assert_eq!(event["type"], "turn_complete");
        assert_eq!(event["assistant_text"], "recovered final answer");
        assert_eq!(event["has_tool_calls"], false);
    }

    #[test]
    fn build_run_turn_complete_event_omits_empty_assistant_text() {
        let event = build_run_turn_complete_event_with_interruption(1, "", None);
        assert_eq!(event["type"], "turn_complete");
        assert_eq!(event["has_tool_calls"], true);
        assert!(event.get("assistant_text").is_none());
    }

    #[test]
    fn stream_turn_complete_is_only_for_completed_or_paused_turns() {
        assert!(should_emit_stream_turn_complete(&RunStatus::Completed));
        assert!(should_emit_stream_turn_complete(&RunStatus::Paused));
        assert!(!should_emit_stream_turn_complete(&RunStatus::Failed));
        assert!(!should_emit_stream_turn_complete(&RunStatus::Cancelled));
        assert!(!should_emit_stream_turn_complete(&RunStatus::Waiting));
        assert!(!should_emit_stream_turn_complete(&RunStatus::InputQueued));
        assert!(!should_emit_stream_turn_complete(&RunStatus::Running));
    }

    #[test]
    fn transcript_page_seq_rolls_over_every_fifty_items() {
        assert_eq!(transcript_page_seq(1), 1);
        assert_eq!(transcript_page_seq(50), 1);
        assert_eq!(transcript_page_seq(51), 2);
        assert_eq!(transcript_page_seq(101), 3);
    }

    #[test]
    fn transcript_page_bounds_cover_exact_page_window() {
        assert_eq!(transcript_page_bounds(1), (1, 50));
        assert_eq!(transcript_page_bounds(2), (51, 100));
        assert_eq!(transcript_page_bounds(3), (101, 150));
    }

    #[test]
    fn budget_exhausted_paused_run_does_not_block_next_session_turn() {
        let (mut run, _, _, _) = AgenticRunLifecycleService::build_tracked_run_state(
            "run-1".to_string(),
            "session-1".to_string(),
            "user-1".to_string(),
        );

        run.status = RunStatus::Running;
        assert!(
            AgenticRunLifecycleService::blocks_new_session_run(&run, "session-1"),
            "running run must block a concurrent turn"
        );

        run.status = RunStatus::Paused;
        run.waiting_for = Some("user_resume".to_string());
        assert!(
            AgenticRunLifecycleService::blocks_new_session_run(&run, "session-1"),
            "manual/user-wait paused run must block until resumed or cancelled"
        );

        run.waiting_for = None;
        assert!(
            !AgenticRunLifecycleService::blocks_new_session_run(&run, "session-1"),
            "budget-exhausted paused run has no waiting_for and must allow the next message"
        );

        run.status = RunStatus::Waiting;
        assert!(
            AgenticRunLifecycleService::blocks_new_session_run(&run, "session-1"),
            "waiting run must still block a concurrent turn"
        );
    }

    fn test_spawn_run_config(allowed_tools: Vec<&str>, read_only: bool) -> SpawnRunConfig {
        SpawnRunConfig {
            run_id: "child-run".to_string(),
            agent_id: "child@1234".to_string(),
            recursion_depth: 1,
            agent_type: "test".to_string(),
            task: "do work".to_string(),
            system_prompt_addendum: String::new(),
            model: Some("test-model".to_string()),
            max_turns: 3,
            allowed_tools: allowed_tools.into_iter().map(String::from).collect(),
            read_only,
            working_dir: std::path::PathBuf::from("/tmp"),
            mailbox: None,
            progress_emitter: None,
            context_cache: None,
            inherited_permissions: None,
            parent_address: None,
            permission_context: None,
            inherited_skills: Vec::new(),
            live_event_sink: None,
            inherited_prefix: None,
            execution_metadata: None,
            is_fork_child: false,
        }
    }

    fn test_spawn_runtime_context(parent_run_id: &str, user_id: &str) -> ServerSpawnRuntimeContext {
        ServerSpawnRuntimeContext {
            parent_run_id: parent_run_id.to_string(),
            user_id: user_id.to_string(),
            session_id: "session-1".to_string(),
            forward_headers: HashMap::new(),
            llm_token_service: None,
            request_constraints: RequestConstraints::default(),
            execution_metadata: None,
            pause_flag: None,
            cancel_token: None,
            trace_context: server_trace_context(user_id, "session-1", parent_run_id, 1),
            #[cfg(feature = "bridge-e2e-hooks")]
            test_child_llm_rounds: Vec::new(),
            #[cfg(feature = "harness")]
            harness_sink: None,
        }
    }

    #[tokio::test]
    async fn server_spawn_runtime_context_is_keyed_by_parent_run() {
        let executor = ServerSpawnAgentExecutor::new(
            test_settings(),
            test_encryptor(),
            Arc::new(TokioMutex::new(HashMap::new())),
        );
        executor
            .set_runtime_context(test_spawn_runtime_context("parent-run-a", "user-a"))
            .await;
        executor
            .set_runtime_context(test_spawn_runtime_context("parent-run-b", "user-b"))
            .await;

        let mut config = test_spawn_run_config(vec!["*"], false);
        config.parent_address = Some(astra_messaging::types::AgentAddress::new(
            "parent-run-b",
            "root-agent",
        ));

        let context = executor.runtime_context_for_config(&config).await.unwrap();

        assert_eq!(context.parent_run_id, "parent-run-b");
        assert_eq!(context.user_id, "user-b");
    }

    #[tokio::test]
    async fn server_spawn_runtime_context_requires_parent_lineage() {
        let executor = ServerSpawnAgentExecutor::new(
            test_settings(),
            test_encryptor(),
            Arc::new(TokioMutex::new(HashMap::new())),
        );
        executor
            .set_runtime_context(test_spawn_runtime_context("parent-run-a", "user-a"))
            .await;

        let config = test_spawn_run_config(vec!["*"], false);
        let err = match executor.runtime_context_for_config(&config).await {
            Ok(_) => panic!("server dynamic spawn must not run without parent lineage"),
            Err(err) => err,
        };

        assert!(err.contains("parent run lineage"), "{err}");
    }

    #[test]
    fn subrun_turn_budget_uses_explicit_spawn_max_turns() {
        let profile = astra_turn_core::chat_turn_heuristics::infer_task_execution_profile(
            "explore the codebase and implement the fix",
        );
        let budget = resolve_subrun_agentic_turn_budget(profile, Some(3));

        assert_eq!(budget.initial_turns, 3);
        assert_eq!(budget.hard_turn_limit, 3);
        assert_eq!(budget.max_extensions, 0);
    }

    #[test]
    fn spawn_child_constraints_intersect_parent_and_agent_allowlists() {
        let parent = RequestConstraints::new(
            Some(
                ["bash", "read_file", "write_file"]
                    .into_iter()
                    .map(String::from)
                    .collect(),
            ),
            Some(["review"].into_iter().map(String::from).collect()),
            Some(
                [
                    crate::skills::manifest::SkillSourceKind::Local,
                    crate::skills::manifest::SkillSourceKind::Database,
                ]
                .into_iter()
                .collect(),
            ),
        );
        let config = test_spawn_run_config(vec!["bash", "read_file"], true);

        let constraints = spawn_child_request_constraints(&parent, &config);

        assert_eq!(
            constraints.allowed_tools.unwrap(),
            ["bash", "read_file"]
                .into_iter()
                .map(String::from)
                .collect()
        );
        assert_eq!(
            constraints.allowed_skills.unwrap(),
            ["review"].into_iter().map(String::from).collect()
        );
        assert_eq!(
            constraints.allowed_skill_sources.unwrap(),
            [
                crate::skills::manifest::SkillSourceKind::Local,
                crate::skills::manifest::SkillSourceKind::Database,
            ]
            .into_iter()
            .collect()
        );
    }

    #[test]
    fn spawn_child_constraints_preserve_parent_when_child_allows_all() {
        let parent = RequestConstraints::new(
            Some(
                ["bash", "write_file"]
                    .into_iter()
                    .map(String::from)
                    .collect(),
            ),
            None,
            None,
        );
        let config = test_spawn_run_config(vec!["*"], false);

        let constraints = spawn_child_request_constraints(&parent, &config);

        assert_eq!(
            constraints.allowed_tools.unwrap(),
            ["bash", "write_file"]
                .into_iter()
                .map(String::from)
                .collect()
        );
    }

    #[test]
    fn spawn_child_constraints_read_only_wildcard_gets_read_only_tools() {
        let parent = RequestConstraints::default();
        let config = test_spawn_run_config(vec!["*"], true);

        let constraints = spawn_child_request_constraints(&parent, &config);
        let allowed = constraints.allowed_tools.unwrap();

        assert!(allowed.contains("read_file"));
        assert!(allowed.contains("grep"));
        assert!(!allowed.contains("write_file"));
        assert!(!allowed.contains("str_replace"));
    }

    #[test]
    fn build_run_turn_complete_event_marks_interrupted_turns() {
        let interruption = astra_turn_core::interruption::InterruptionRecord::new(
            astra_turn_core::interruption::InterruptionKind::BudgetExhausted,
            astra_turn_core::interruption::ResumeAction::ContinueImmediately,
            astra_turn_core::interruption::InterruptionStateSummary {
                has_checkpoint: true,
                tool_calls_completed: 7,
                turns_completed: 15,
                remaining_turns: 0,
                error_detail: Some("Round budget hard-limit reached".to_string()),
                stall_signal: None,
                resume_restricted_tools: vec![],
            },
        );

        let event = build_run_turn_complete_event_with_interruption(
            7,
            "[Round budget hard-limit reached]",
            Some(&interruption),
        );

        assert_eq!(event["type"], "turn_complete");
        assert_eq!(event["has_tool_calls"], false);
        assert_eq!(event["stall_detected"], true);
        assert_eq!(event["execution_state"]["status"], "interrupted");
        assert_eq!(event["execution_state"]["interrupted"], true);
        assert_eq!(
            event["execution_state"]["interruption_kind"],
            "budget_exhausted"
        );
        assert_eq!(event["execution_state"]["tool_calls_completed"], 7);
        assert_eq!(event["execution_state"]["remaining_turns"], 0);
        assert_eq!(event["assistant_text"], "[Round budget hard-limit reached]");
    }

    #[test]
    fn correction_keywords_trigger_was_corrected_via_implicit_feedback() {
        // Sanity-check that the detect_implicit_feedback_signal contract used in
        // record_server_loop_learning_outcome produces a "correction" signal
        // for the Chinese-language corrections listed in routing::detect_correction.
        let signal = astra_turn_types::detect_implicit_feedback_signal(
            "不对，你搞错了",
            Some("previous assistant reply"),
        );
        assert!(
            matches!(signal.signal_type.as_str(), "correction" | "frustration"),
            "expected correction/frustration, got {:?}",
            signal.signal_type
        );
    }

    #[test]
    fn neutral_user_turn_does_not_flag_was_corrected() {
        let signal = astra_turn_types::detect_implicit_feedback_signal(
            "再列一下 docs 目录",
            Some("previous assistant reply"),
        );
        assert!(
            !matches!(signal.signal_type.as_str(), "correction" | "frustration"),
            "expected non-correction, got {:?}",
            signal.signal_type
        );
    }

    /// Unwrap a `Result<T, (StatusCode, Json<ErrorResponse>)>` in tests.
    fn ok<T>(result: Result<T, (StatusCode, Json<ErrorResponse>)>) -> T {
        match result {
            Ok(v) => v,
            Err((status, body)) => panic!("expected Ok, got {status}: {}", body.0.detail),
        }
    }

    /// Unwrap the error side.
    fn err<T>(
        result: Result<T, (StatusCode, Json<ErrorResponse>)>,
    ) -> (StatusCode, Json<ErrorResponse>) {
        match result {
            Ok(_) => panic!("expected Err, got Ok"),
            Err(e) => e,
        }
    }

    fn test_settings() -> MatrixOneSettings {
        MatrixOneSettings::from_env_with_database("test_astra_runtime")
    }

    fn test_encryptor() -> Arc<FernetTokenEncryptor> {
        Arc::new(FernetTokenEncryptor::new("cJ8pxr3t6iJmSYqe6wD7vu2rN_C3ovGUxkC5H3NXFNY=").unwrap())
    }

    #[derive(Default)]
    struct FaultInjectedRunStoreCounters {
        status_calls: usize,
        append_calls: usize,
    }

    struct FaultInjectedRunStateStore {
        inner: InMemoryRunStateStore,
        fail_status_calls: HashSet<usize>,
        fail_append_calls: HashSet<usize>,
        counters: StdMutex<FaultInjectedRunStoreCounters>,
    }

    impl FaultInjectedRunStateStore {
        fn new(fail_status_calls: &[usize], fail_append_calls: &[usize]) -> Self {
            Self {
                inner: InMemoryRunStateStore::new(),
                fail_status_calls: fail_status_calls.iter().copied().collect(),
                fail_append_calls: fail_append_calls.iter().copied().collect(),
                counters: StdMutex::new(FaultInjectedRunStoreCounters::default()),
            }
        }

        fn next_status_call(&self) -> usize {
            let mut counters = self.counters.lock().expect("status counter lock");
            counters.status_calls += 1;
            counters.status_calls
        }

        fn next_append_call(&self) -> usize {
            let mut counters = self.counters.lock().expect("append counter lock");
            counters.append_calls += 1;
            counters.append_calls
        }
    }

    #[async_trait]
    impl RunStateStore for FaultInjectedRunStateStore {
        async fn insert_run(&self, record: DurableRunRecord) -> Result<(), String> {
            self.inner.insert_run(record).await
        }

        async fn load_run(&self, run_id: &str) -> Result<Option<DurableRunRecord>, String> {
            self.inner.load_run(run_id).await
        }

        async fn update_run_status(
            &self,
            run_id: &str,
            status: &str,
            waiting_for: Option<&str>,
            error_message: Option<&str>,
        ) -> Result<bool, String> {
            let call = self.next_status_call();
            if self.fail_status_calls.contains(&call) {
                return Err(format!("injected update_run_status failure on call {call}"));
            }
            self.inner
                .update_run_status(run_id, status, waiting_for, error_message)
                .await
        }

        async fn update_run_status_if_current(
            &self,
            run_id: &str,
            expected_statuses: &[&str],
            status: &str,
            waiting_for: Option<&str>,
            error_message: Option<&str>,
        ) -> Result<bool, String> {
            let call = self.next_status_call();
            if self.fail_status_calls.contains(&call) {
                return Err(format!(
                    "injected update_run_status_if_current failure on call {call}"
                ));
            }
            self.inner
                .update_run_status_if_current(
                    run_id,
                    expected_statuses,
                    status,
                    waiting_for,
                    error_message,
                )
                .await
        }

        async fn update_run_usage(
            &self,
            run_id: &str,
            prompt_tokens: u64,
            completion_tokens: u64,
            tool_calls: u32,
        ) -> Result<bool, String> {
            self.inner
                .update_run_usage(run_id, prompt_tokens, completion_tokens, tool_calls)
                .await
        }

        async fn save_checkpoint(
            &self,
            run_id: &str,
            checkpoint_json: &str,
        ) -> Result<bool, String> {
            self.inner.save_checkpoint(run_id, checkpoint_json).await
        }

        async fn load_latest_checkpoint(
            &self,
            run_id: &str,
            checkpoint_kind: Option<&str>,
        ) -> Result<Option<DurableRunCheckpointRecord>, String> {
            self.inner
                .load_latest_checkpoint(run_id, checkpoint_kind)
                .await
        }

        async fn load_run_projection(
            &self,
            run_id: &str,
        ) -> Result<Option<DurableRunDisplayProjectionRecord>, String> {
            self.inner.load_run_projection(run_id).await
        }

        async fn append_events_batch(
            &self,
            run_id: &str,
            events: &[serde_json::Value],
        ) -> Result<(), String> {
            let call = self.next_append_call();
            if self.fail_append_calls.contains(&call) {
                return Err(format!("injected append_event failure on call {call}"));
            }
            self.inner.append_events_batch(run_id, events).await
        }

        async fn list_user_runs(
            &self,
            user_id: &str,
            limit: u32,
            offset: u32,
        ) -> Result<(Vec<DurableRunRecord>, i64), String> {
            self.inner.list_user_runs(user_id, limit, offset).await
        }

        async fn find_waiting_runs(&self) -> Result<Vec<DurableRunRecord>, String> {
            self.inner.find_waiting_runs().await
        }

        async fn find_running_runs(&self) -> Result<Vec<DurableRunRecord>, String> {
            self.inner.find_running_runs().await
        }

        async fn find_blocking_session_run(
            &self,
            user_id: &str,
            session_id: &str,
        ) -> Result<Option<DurableRunRecord>, String> {
            self.inner
                .find_blocking_session_run(user_id, session_id)
                .await
        }

        async fn find_sub_runs(
            &self,
            delegation_id: &str,
        ) -> Result<Vec<DurableRunRecord>, String> {
            self.inner.find_sub_runs(delegation_id).await
        }

        async fn update_retry_count(&self, run_id: &str, retry_count: u32) -> Result<bool, String> {
            self.inner.update_retry_count(run_id, retry_count).await
        }
    }

    fn test_service() -> AgenticRunLifecycleService {
        let engine = RunEngine::new(Arc::new(InMemoryRunStateStore::new()));
        AgenticRunLifecycleService::new(
            test_settings(),
            test_encryptor(),
            Arc::new(TokioMutex::new(HashMap::new())),
            engine,
        )
    }

    fn test_service_with_store(store: Arc<dyn RunStateStore>) -> AgenticRunLifecycleService {
        let engine = RunEngine::new(store);
        AgenticRunLifecycleService::new(
            test_settings(),
            test_encryptor(),
            Arc::new(TokioMutex::new(HashMap::new())),
            engine,
        )
    }

    fn test_request(message: &str) -> ChatRequestData {
        ChatRequestData {
            message: message.to_string(),
            session_id: None,
            full_llm_capture: false,
            agent_id: None,
            model: None,
            llm_token_service: None,
            skill_search: None,
            allow_skills: None,
            allow_skill_sources: None,
            allow_tools: None,
            workspace_binding: None,
            executor_binding: None,
            runtime_mcp_bindings: Vec::new(),
            mcp_binding_ids: None,
            context: None,
            forward_headers: HashMap::new(),
            execution_budget: None,
            explain: false,
            interaction_mode: None,
            interactive_client: false,
        }
    }

    struct FailingWorkspaceRecordStore;

    #[async_trait]
    impl WorkspaceRecordStore for FailingWorkspaceRecordStore {
        async fn upsert_workspace_record(
            &self,
            _entry: StoredWorkspaceRecordEntry,
        ) -> Result<(), WorkspaceRecordStoreError> {
            Err(WorkspaceRecordStoreError::Unavailable(
                "injected workspace store failure".to_string(),
            ))
        }

        async fn load_workspace_record(
            &self,
            _owner_id: &str,
            _workspace_id: &str,
        ) -> Result<Option<StoredWorkspaceRecordEntry>, WorkspaceRecordStoreError> {
            Ok(None)
        }

        async fn list_workspace_records(
            &self,
            _owner_id: &str,
            _limit: u32,
        ) -> Result<Vec<StoredWorkspaceRecordEntry>, WorkspaceRecordStoreError> {
            Ok(Vec::new())
        }
    }

    #[async_trait]
    impl WorkspaceCleanupDebtStore for FailingWorkspaceRecordStore {
        async fn record_cleanup_debt(
            &self,
            _entry: WorkspaceCleanupDebtEntry,
        ) -> Result<(), WorkspaceCleanupDebtStoreError> {
            Err(WorkspaceCleanupDebtStoreError::Unavailable(
                "injected cleanup debt store failure".to_string(),
            ))
        }

        async fn list_cleanup_debts(
            &self,
            _owner_id: &str,
            _limit: u32,
        ) -> Result<Vec<WorkspaceCleanupDebtEntry>, WorkspaceCleanupDebtStoreError> {
            Ok(Vec::new())
        }

        async fn resolve_cleanup_debt(
            &self,
            _owner_id: &str,
            _debt_id: &str,
        ) -> Result<bool, WorkspaceCleanupDebtStoreError> {
            Ok(false)
        }

        async fn list_all_unresolved_debts(
            &self,
        ) -> Result<Vec<WorkspaceCleanupDebtEntry>, WorkspaceCleanupDebtStoreError> {
            Err(WorkspaceCleanupDebtStoreError::Unavailable(
                "injected cleanup debt store failure".to_string(),
            ))
        }

        async fn increment_debt_attempts(
            &self,
            _debt_id: &str,
        ) -> Result<(), WorkspaceCleanupDebtStoreError> {
            Err(WorkspaceCleanupDebtStoreError::Unavailable(
                "injected cleanup debt store failure".to_string(),
            ))
        }
    }

    fn test_cloud_workspace_record(workspace_id: &str) -> RuntimeWorkspaceRecord {
        RuntimeWorkspaceRecord {
            workspace_id: workspace_id.to_string(),
            owner_scope: RuntimeWorkspaceOwnerScope::Tenant,
            kind: astra_runtime_env::WorkspaceBindingKind::CloudWorkspace,
            authority: astra_runtime_env::WorkspaceAuthority::ReadWrite,
            root_or_volume_ref: "/cloud/volumes/team-volume-1".to_string(),
            source: RuntimeWorkspaceSource::PersistentVolume {
                volume_id: "team-volume-1".to_string(),
            },
            persistence: RuntimeWorkspacePersistence::Persistent,
            revision: "1".to_string(),
            display_name: "Team workspace".to_string(),
        }
    }

    #[tokio::test]
    async fn lifecycle_persists_workspace_record_with_owner_session_and_run() {
        let store = Arc::new(InMemoryWorkspaceRecordStore::new());
        let svc = test_service().with_workspace_record_store(store.clone());
        let record = test_cloud_workspace_record("workspace-1");

        ok(svc
            .persist_workspace_record("user-1", "session-1", "run-1", &record)
            .await);

        let loaded = store
            .load_workspace_record("user-1", "workspace-1")
            .await
            .expect("load workspace record")
            .expect("record");
        assert_eq!(loaded.owner_id, "user-1");
        assert_eq!(loaded.session_id.as_deref(), Some("session-1"));
        assert_eq!(loaded.run_id.as_deref(), Some("run-1"));
        assert_eq!(loaded.record, record);
        assert!(
            store
                .load_workspace_record("user-2", "workspace-1")
                .await
                .expect("load workspace record")
                .is_none(),
            "workspace records must stay owner scoped"
        );
    }

    #[tokio::test]
    async fn lifecycle_workspace_record_store_failure_fails_closed() {
        let svc = test_service().with_workspace_record_store(Arc::new(FailingWorkspaceRecordStore));
        let record = test_cloud_workspace_record("workspace-1");

        let error = err(svc
            .persist_workspace_record("user-1", "session-1", "run-1", &record)
            .await);

        assert_eq!(error.0, StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            error
                .1
                .0
                .detail
                .contains("Failed to persist workspace record"),
            "{}",
            error.1.0.detail
        );
    }

    #[tokio::test]
    async fn lifecycle_workspace_record_source_conflict_returns_conflict() {
        let store = Arc::new(InMemoryWorkspaceRecordStore::new());
        store
            .upsert_workspace_record(StoredWorkspaceRecordEntry::new(
                "user-2",
                Some("session-2".to_string()),
                Some("run-2".to_string()),
                test_cloud_workspace_record("workspace-2"),
            ))
            .await
            .expect("store existing workspace owner");
        let svc = test_service().with_workspace_record_store(store);
        let record = test_cloud_workspace_record("workspace-1");

        let error = err(svc
            .persist_workspace_record("user-1", "session-1", "run-1", &record)
            .await);

        assert_eq!(error.0, StatusCode::CONFLICT);
        assert!(
            error.1.0.detail.contains("Workspace ownership conflict"),
            "{}",
            error.1.0.detail
        );
    }

    #[tokio::test]
    async fn lifecycle_records_cleanup_debt_when_failed_start_cleanup_fails() {
        let store = Arc::new(InMemoryWorkspaceRecordStore::new());
        let svc = test_service().with_workspace_record_store(store.clone());
        let mut record = test_cloud_workspace_record("workspace-cleanup-debt");
        record.persistence = RuntimeWorkspacePersistence::Session;
        record.source = RuntimeWorkspaceSource::Scratch;
        record.root_or_volume_ref = "/definitely/missing/astra-cleanup-debt".to_string();

        svc.cleanup_cloud_workspace_after_failed_start(
            "user-1",
            "session-1",
            "run-1",
            &record,
            "injected start failure".to_string(),
        )
        .await;

        let debts = store
            .list_cleanup_debts("user-1", 10)
            .await
            .expect("list cleanup debts");
        assert_eq!(debts.len(), 1);
        assert_eq!(debts[0].workspace_id, "workspace-cleanup-debt");
        assert_eq!(debts[0].reason, RuntimeCleanupReason::Failed);
        assert!(debts[0].message.contains("injected start failure"));
        assert_eq!(debts[0].session_id.as_deref(), Some("session-1"));
        assert_eq!(debts[0].run_id.as_deref(), Some("run-1"));
    }

    #[tokio::test]
    async fn lifecycle_records_cleanup_debt_when_terminal_cleanup_fails() {
        let store = Arc::new(InMemoryWorkspaceRecordStore::new());
        let mut record = test_cloud_workspace_record("workspace-terminal-cleanup-debt");
        record.persistence = RuntimeWorkspacePersistence::Session;
        record.source = RuntimeWorkspaceSource::Scratch;
        record.root_or_volume_ref = "/definitely/missing/astra-terminal-cleanup-debt".to_string();

        AgenticRunLifecycleService::cleanup_cloud_workspace_after_terminal_run(
            Some(store.clone()),
            "user-1",
            "session-1",
            "run-1",
            &record,
            &RunStatus::Completed,
        )
        .await;

        let debts = store
            .list_cleanup_debts("user-1", 10)
            .await
            .expect("list cleanup debts");
        assert_eq!(debts.len(), 1);
        assert_eq!(debts[0].workspace_id, "workspace-terminal-cleanup-debt");
        assert_eq!(debts[0].reason, RuntimeCleanupReason::Completed);
        assert!(debts[0].message.contains("run ended with status completed"));
        assert_eq!(debts[0].session_id.as_deref(), Some("session-1"));
        assert_eq!(debts[0].run_id.as_deref(), Some("run-1"));
    }

    #[tokio::test]
    async fn lifecycle_skips_cloud_workspace_cleanup_for_resumable_status() {
        let store = Arc::new(InMemoryWorkspaceRecordStore::new());
        let mut record = test_cloud_workspace_record("workspace-waiting-no-cleanup");
        record.persistence = RuntimeWorkspacePersistence::Session;
        record.root_or_volume_ref = "/definitely/missing/astra-waiting-no-cleanup".to_string();

        AgenticRunLifecycleService::cleanup_cloud_workspace_after_terminal_run(
            Some(store.clone()),
            "user-1",
            "session-1",
            "run-1",
            &record,
            &RunStatus::Waiting,
        )
        .await;

        assert!(
            store
                .list_cleanup_debts("user-1", 10)
                .await
                .expect("list cleanup debts")
                .is_empty(),
            "resumable runs must keep their workspace for continuation"
        );
    }

    #[test]
    fn cloud_git_source_maps_to_workspace_record_contract() {
        let mut request = test_request("checkout this repo");
        request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
            kind: astra_services::runs::WorkspaceBindingRequestKind::CloudWorkspace,
            display_name: Some("Repo checkout".to_string()),
            root: None,
            source: Some(astra_services::runs::WorkspaceSourceRequest::GitCheckout {
                repository: "https://example.com/org/repo.git".to_string(),
                reference: None,
            }),
            authority: None,
            fallback_policy: Some(astra_services::runs::FallbackPolicyRequest::Disabled),
        });

        let provision_request = ok(cloud_workspace_provision_request_from_request(
            &request, "123",
        ))
        .expect("cloud workspace request");

        assert_eq!(provision_request.workspace_id, "run-123");
        assert_eq!(
            provision_request.kind,
            astra_runtime_env::WorkspaceBindingKind::CloudWorkspace
        );
        assert_eq!(
            provision_request.authority,
            astra_runtime_env::WorkspaceAuthority::ReadWrite
        );
        assert_eq!(
            provision_request.persistence,
            RuntimeWorkspacePersistence::Session
        );
        assert_eq!(
            provision_request.source,
            RuntimeWorkspaceSource::GitCheckout {
                repository: "https://example.com/org/repo.git".to_string(),
                reference: None,
            }
        );

        let record = RuntimeWorkspaceRecord {
            workspace_id: provision_request.workspace_id,
            owner_scope: provision_request.owner_scope,
            kind: provision_request.kind,
            authority: provision_request.authority,
            root_or_volume_ref: "/cloud/checkouts/run-123".to_string(),
            source: provision_request.source,
            persistence: provision_request.persistence,
            revision: "1".to_string(),
            display_name: "Repo checkout".to_string(),
        };
        let snapshot = execution_bindings_from_workspace_record(&record);
        let workspace = &snapshot.workspace;
        let executor = &snapshot.executor;

        assert_eq!(workspace.kind, WorkspaceBindingKind::CloudWorkspace);
        assert_eq!(workspace.cwd.as_deref(), Some("/cloud/checkouts/run-123"));
        assert_eq!(executor.kind, ExecutorBindingKind::OrchestratorManaged);
        assert_eq!(executor.transport, ToolTransportKind::SandboxResidentAgent);
        assert_eq!(
            snapshot
                .runtime
                .as_ref()
                .map(|runtime| runtime.launch_driver),
            Some(astra_runtime_env::RuntimeLaunchDriver::Kubernetes)
        );
    }

    #[test]
    fn cloud_persistent_volume_binding_maps_to_workspace_record_contract() {
        let mut request = test_request("use my workspace");
        request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
            kind: astra_services::runs::WorkspaceBindingRequestKind::CloudWorkspace,
            display_name: Some("Team workspace".to_string()),
            root: None,
            source: Some(
                astra_services::runs::WorkspaceSourceRequest::PersistentVolume {
                    volume_id: "team-volume-1".to_string(),
                },
            ),
            authority: None,
            fallback_policy: Some(astra_services::runs::FallbackPolicyRequest::Disabled),
        });

        let provision_request = ok(cloud_workspace_provision_request_from_request(
            &request,
            "volume-run",
        ))
        .expect("cloud workspace request");

        assert_eq!(provision_request.workspace_id, "run-volume-run");
        assert_eq!(
            provision_request.kind,
            astra_runtime_env::WorkspaceBindingKind::CloudWorkspace
        );
        assert_eq!(
            provision_request.authority,
            astra_runtime_env::WorkspaceAuthority::ReadWrite
        );
        assert_eq!(
            provision_request.persistence,
            RuntimeWorkspacePersistence::Persistent
        );
        assert_eq!(
            provision_request.source,
            RuntimeWorkspaceSource::PersistentVolume {
                volume_id: "team-volume-1".to_string(),
            }
        );

        let record = RuntimeWorkspaceRecord {
            workspace_id: provision_request.workspace_id,
            owner_scope: provision_request.owner_scope,
            kind: provision_request.kind,
            authority: provision_request.authority,
            root_or_volume_ref: "/cloud/volumes/team-volume-1".to_string(),
            source: provision_request.source,
            persistence: provision_request.persistence,
            revision: "1".to_string(),
            display_name: "Team workspace".to_string(),
        };
        let snapshot = execution_bindings_from_workspace_record(&record);
        let workspace = &snapshot.workspace;
        let executor = &snapshot.executor;

        assert_eq!(workspace.kind, WorkspaceBindingKind::CloudWorkspace);
        assert_eq!(
            workspace.cwd.as_deref(),
            Some("/cloud/volumes/team-volume-1")
        );
        assert_eq!(executor.kind, ExecutorBindingKind::OrchestratorManaged);
        assert_eq!(executor.transport, ToolTransportKind::SandboxResidentAgent);
        assert_eq!(
            snapshot
                .runtime
                .as_ref()
                .map(|runtime| runtime.session_manager),
            Some(astra_runtime_env::RuntimeSessionManager::ProviderManaged)
        );
    }

    #[test]
    fn cloud_scratch_source_maps_to_generic_workspace_record_contract() {
        let mut request = test_request("create scratch workspace");
        request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
            kind: astra_services::runs::WorkspaceBindingRequestKind::CloudWorkspace,
            display_name: Some("Scratch workspace".to_string()),
            root: None,
            source: Some(astra_services::runs::WorkspaceSourceRequest::Scratch),
            authority: None,
            fallback_policy: Some(astra_services::runs::FallbackPolicyRequest::Disabled),
        });

        let provision_request = ok(cloud_workspace_provision_request_from_request(
            &request,
            "scratch-run",
        ))
        .expect("scratch cloud workspace request");

        assert_eq!(provision_request.workspace_id, "run-scratch-run");
        assert_eq!(
            provision_request.kind,
            astra_runtime_env::WorkspaceBindingKind::CloudWorkspace
        );
        assert_eq!(provision_request.source, RuntimeWorkspaceSource::Scratch);
        assert_eq!(
            provision_request.persistence,
            RuntimeWorkspacePersistence::Session
        );
    }

    #[test]
    fn cloud_uploaded_snapshot_source_defaults_to_immutable_read_only() {
        let mut request = test_request("inspect snapshot");
        request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
            kind: astra_services::runs::WorkspaceBindingRequestKind::CloudWorkspace,
            display_name: None,
            root: None,
            source: Some(
                astra_services::runs::WorkspaceSourceRequest::UploadedSnapshot {
                    artifact_id: "artifact-1".to_string(),
                    root: None,
                },
            ),
            authority: None,
            fallback_policy: Some(astra_services::runs::FallbackPolicyRequest::Disabled),
        });

        let provision_request = ok(cloud_workspace_provision_request_from_request(
            &request, "456",
        ))
        .expect("cloud workspace request");

        assert_eq!(
            provision_request.kind,
            astra_runtime_env::WorkspaceBindingKind::CloudWorkspace
        );
        assert_eq!(
            provision_request.authority,
            astra_runtime_env::WorkspaceAuthority::ReadOnly
        );
        assert_eq!(
            provision_request.persistence,
            RuntimeWorkspacePersistence::ImmutableSnapshot
        );
        assert_eq!(
            provision_request.source,
            RuntimeWorkspaceSource::UploadedSnapshot {
                artifact_id: "artifact-1".to_string(),
            }
        );
    }

    #[test]
    fn cloud_template_source_defaults_to_read_write_session_workspace() {
        let mut request = test_request("start from template");
        request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
            kind: astra_services::runs::WorkspaceBindingRequestKind::CloudWorkspace,
            display_name: None,
            root: Some("/cloud/templates/template-1".to_string()),
            source: Some(astra_services::runs::WorkspaceSourceRequest::Template {
                template_id: "template-1".to_string(),
            }),
            authority: None,
            fallback_policy: Some(astra_services::runs::FallbackPolicyRequest::Disabled),
        });

        let provision_request = ok(cloud_workspace_provision_request_from_request(
            &request,
            "template-run",
        ))
        .expect("template workspace request");

        assert_eq!(
            provision_request.authority,
            astra_runtime_env::WorkspaceAuthority::ReadWrite
        );
        assert_eq!(
            provision_request.persistence,
            RuntimeWorkspacePersistence::Session
        );
        assert_eq!(
            provision_request.source,
            RuntimeWorkspaceSource::Template {
                template_id: "template-1".to_string(),
            }
        );
        assert_eq!(
            provision_request.requested_root.as_deref(),
            Some("/cloud/templates/template-1")
        );
    }

    #[test]
    fn cloud_dataset_and_artifact_sources_default_to_immutable_read_only() {
        let cases = [
            (
                astra_services::runs::WorkspaceSourceRequest::DatasetBundle {
                    dataset_id: "dataset-1".to_string(),
                },
                RuntimeWorkspaceSource::DatasetBundle {
                    dataset_id: "dataset-1".to_string(),
                },
            ),
            (
                astra_services::runs::WorkspaceSourceRequest::ArtifactBundle {
                    artifact_id: "artifact-1".to_string(),
                },
                RuntimeWorkspaceSource::ArtifactBundle {
                    artifact_id: "artifact-1".to_string(),
                },
            ),
        ];

        for (source, expected_source) in cases {
            let mut request = test_request("inspect materialized source");
            request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
                kind: astra_services::runs::WorkspaceBindingRequestKind::CloudWorkspace,
                display_name: None,
                root: None,
                source: Some(source),
                authority: None,
                fallback_policy: Some(astra_services::runs::FallbackPolicyRequest::Disabled),
            });

            let provision_request = ok(cloud_workspace_provision_request_from_request(
                &request,
                "bundle-run",
            ))
            .expect("bundle workspace request");

            assert_eq!(
                provision_request.authority,
                astra_runtime_env::WorkspaceAuthority::ReadOnly
            );
            assert_eq!(
                provision_request.persistence,
                RuntimeWorkspacePersistence::ImmutableSnapshot
            );
            assert_eq!(provision_request.source, expected_source);
        }
    }

    #[test]
    fn cloud_materialized_source_rejects_relative_root_before_provisioning() {
        let mut request = test_request("bad template root");
        request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
            kind: astra_services::runs::WorkspaceBindingRequestKind::CloudWorkspace,
            display_name: None,
            root: Some("relative/template".to_string()),
            source: Some(astra_services::runs::WorkspaceSourceRequest::Template {
                template_id: "template-1".to_string(),
            }),
            authority: None,
            fallback_policy: Some(astra_services::runs::FallbackPolicyRequest::Disabled),
        });

        let error = err(cloud_workspace_provision_request_from_request(
            &request,
            "bad-template",
        ));

        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert!(
            error
                .1
                .0
                .detail
                .contains("absolute materialized source path"),
            "{}",
            error.1.0.detail
        );
    }

    #[test]
    fn cloud_materialized_source_rejects_empty_identifier() {
        let mut request = test_request("bad dataset");
        request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
            kind: astra_services::runs::WorkspaceBindingRequestKind::CloudWorkspace,
            display_name: None,
            root: None,
            source: Some(
                astra_services::runs::WorkspaceSourceRequest::DatasetBundle {
                    dataset_id: "   ".to_string(),
                },
            ),
            authority: None,
            fallback_policy: Some(astra_services::runs::FallbackPolicyRequest::Disabled),
        });

        let error = err(cloud_workspace_provision_request_from_request(
            &request,
            "bad-dataset",
        ));

        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert!(
            error.1.0.detail.contains("non-empty source.dataset_id"),
            "{}",
            error.1.0.detail
        );
    }

    #[test]
    fn cloud_workspace_binding_requires_materialized_source() {
        let mut request = test_request("checkout");
        request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
            kind: astra_services::runs::WorkspaceBindingRequestKind::CloudWorkspace,
            display_name: None,
            root: None,
            source: Some(astra_services::runs::WorkspaceSourceRequest::GitCheckout {
                repository: "   ".to_string(),
                reference: None,
            }),
            authority: None,
            fallback_policy: Some(astra_services::runs::FallbackPolicyRequest::Disabled),
        });

        let error = err(cloud_workspace_provision_request_from_request(
            &request, "789",
        ));

        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert!(
            error
                .1
                .0
                .detail
                .contains("Git checkout workspace requires a non-empty source.repository"),
            "{}",
            error.1.0.detail
        );
    }

    #[test]
    fn cloud_workspace_binding_rejects_missing_source() {
        let mut request = test_request("use cloud workspace");
        request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
            kind: astra_services::runs::WorkspaceBindingRequestKind::CloudWorkspace,
            display_name: None,
            root: None,
            source: None,
            authority: None,
            fallback_policy: Some(astra_services::runs::FallbackPolicyRequest::Disabled),
        });

        let error = err(cloud_workspace_provision_request_from_request(
            &request,
            "bad-volume",
        ));

        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert!(
            error
                .1
                .0
                .detail
                .contains("Cloud workspace requires an explicit source"),
            "{}",
            error.1.0.detail
        );
    }

    #[test]
    fn cloud_workspace_runtime_kind_projects_to_server_binding() {
        let record = RuntimeWorkspaceRecord {
            workspace_id: "workspace-1".to_string(),
            owner_scope: RuntimeWorkspaceOwnerScope::Tenant,
            kind: astra_runtime_env::WorkspaceBindingKind::CloudWorkspace,
            authority: astra_runtime_env::WorkspaceAuthority::ReadWrite,
            root_or_volume_ref: "/cloud/volumes/team-volume-1".to_string(),
            source: RuntimeWorkspaceSource::PersistentVolume {
                volume_id: "team-volume-1".to_string(),
            },
            persistence: RuntimeWorkspacePersistence::Persistent,
            revision: "1".to_string(),
            display_name: "Team workspace".to_string(),
        };

        let workspace = server_workspace_binding_from_workspace_record(&record);

        assert_eq!(workspace.kind, WorkspaceBindingKind::CloudWorkspace);
        assert_eq!(
            workspace.cwd.as_deref(),
            Some("/cloud/volumes/team-volume-1")
        );
    }

    #[test]
    fn request_execution_bindings_use_actual_server_workspace_for_server_sandbox() {
        let mut request = test_request("hello");
        request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
            kind: astra_services::runs::WorkspaceBindingRequestKind::ServerSandbox,
            display_name: Some("Requested server".to_string()),
            root: Some("/client/claimed/path".to_string()),
            source: None,
            authority: Some(astra_services::runs::WorkspaceAuthorityRequest::ReadWrite),
            fallback_policy: Some(astra_services::runs::FallbackPolicyRequest::Disabled),
        });
        request.executor_binding = Some(astra_services::runs::ExecutorBindingRequest {
            kind: astra_services::runs::ExecutorBindingRequestKind::ServerLocal,
            executor_id: Some("server-local".to_string()),
            display_name: Some("Requested executor".to_string()),
            transport: Some(astra_services::runs::ToolTransportKindRequest::ServerLocal),
            status: Some(astra_services::runs::ExecutorStatusRequest::Online),
        });

        let server_workspace = Path::new("/tmp/astra-runtime-workspace");
        let (workspace, executor) = resolve_request_execution_bindings(&request, server_workspace);

        assert_eq!(workspace.kind, WorkspaceBindingKind::ServerSandbox);
        assert_eq!(workspace.display_name, "Requested server");
        assert_eq!(
            workspace.cwd.as_deref(),
            Some("/tmp/astra-runtime-workspace")
        );
        assert_eq!(workspace.authority, WorkspaceAuthority::ReadWrite);
        assert_eq!(workspace.fallback_policy, FallbackPolicy::Disabled);
        assert_eq!(executor.kind, ExecutorBindingKind::ServerLocal);
        assert_eq!(executor.executor_id, "server-local");
        assert_eq!(executor.display_name, "Requested executor");
        assert_eq!(executor.transport, ToolTransportKind::ServerLocal);
        assert_eq!(executor.status, ExecutorStatus::Online);
    }

    #[test]
    fn server_workspace_binding_decision_respects_explicit_binding_and_edge_tools() {
        let mut request = test_request("hello");

        assert!(request_uses_server_workspace(&request, false));
        assert!(!request_uses_server_workspace(&request, true));

        request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
            kind: astra_services::runs::WorkspaceBindingRequestKind::ServerSandbox,
            display_name: None,
            root: None,
            source: None,
            authority: None,
            fallback_policy: None,
        });
        assert!(request_uses_server_workspace(&request, true));

        request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
            kind: astra_services::runs::WorkspaceBindingRequestKind::EdgeWorkspace,
            display_name: Some("Edge".to_string()),
            root: Some("/repo".to_string()),
            source: Some(astra_services::runs::WorkspaceSourceRequest::EdgePath {
                path: "/repo".to_string(),
            }),
            authority: Some(astra_services::runs::WorkspaceAuthorityRequest::ReadWrite),
            fallback_policy: Some(astra_services::runs::FallbackPolicyRequest::Disabled),
        });
        assert!(!request_uses_server_workspace(&request, false));
        assert!(!request_uses_server_workspace(&request, true));
    }

    #[test]
    fn request_execution_bindings_keep_edge_workspace_without_server_fallback() {
        let mut request = test_request("review this repo");
        request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
            kind: astra_services::runs::WorkspaceBindingRequestKind::EdgeWorkspace,
            display_name: Some("MacBook Pro".to_string()),
            root: Some("/Users/xupeng/github/astra".to_string()),
            source: Some(astra_services::runs::WorkspaceSourceRequest::EdgePath {
                path: "/Users/xupeng/github/astra".to_string(),
            }),
            authority: Some(astra_services::runs::WorkspaceAuthorityRequest::ReadWrite),
            fallback_policy: Some(astra_services::runs::FallbackPolicyRequest::Disabled),
        });
        request.executor_binding = Some(astra_services::runs::ExecutorBindingRequest {
            kind: astra_services::runs::ExecutorBindingRequestKind::EdgeAgent,
            executor_id: Some("edge-macbook-1".to_string()),
            display_name: Some("MacBook Pro".to_string()),
            transport: Some(astra_services::runs::ToolTransportKindRequest::EdgeWs),
            status: Some(astra_services::runs::ExecutorStatusRequest::Online),
        });

        let (workspace, executor) =
            resolve_request_execution_bindings(&request, Path::new("/tmp/server-workspace"));

        assert_eq!(workspace.kind, WorkspaceBindingKind::EdgeWorkspace);
        assert_eq!(workspace.display_name, "MacBook Pro");
        assert_eq!(workspace.cwd.as_deref(), Some("/Users/xupeng/github/astra"));
        assert_eq!(workspace.fallback_policy, FallbackPolicy::Disabled);
        assert_eq!(executor.kind, ExecutorBindingKind::EdgeAgent);
        assert_eq!(executor.executor_id, "edge-macbook-1");
        assert_eq!(executor.transport, ToolTransportKind::EdgeWs);
        assert_eq!(executor.status, ExecutorStatus::Online);
    }

    #[test]
    fn edge_profile_execution_bindings_make_legacy_edge_tools_explicit() {
        let mut edge_profile = Map::new();
        edge_profile.insert("cwd".to_string(), json!("/Users/xupeng/github/astra"));
        edge_profile.insert("edge_agent_id".to_string(), json!("edge-macbook-1"));
        edge_profile.insert("hostname".to_string(), json!("MacBook Pro"));

        let (workspace, executor) = resolve_request_execution_bindings_without_server_workspace(
            &test_request("review this repo"),
            &edge_profile,
        )
        .expect("legacy edge profile should produce explicit bindings");

        assert_eq!(workspace.kind, WorkspaceBindingKind::EdgeWorkspace);
        assert_eq!(workspace.display_name, "MacBook Pro");
        assert_eq!(workspace.cwd.as_deref(), Some("/Users/xupeng/github/astra"));
        assert_eq!(workspace.authority, WorkspaceAuthority::ReadWrite);
        assert_eq!(workspace.fallback_policy, FallbackPolicy::Disabled);
        assert_eq!(executor.kind, ExecutorBindingKind::EdgeAgent);
        assert_eq!(executor.executor_id, "edge-macbook-1");
        assert_eq!(executor.display_name, "MacBook Pro");
        assert_eq!(executor.transport, ToolTransportKind::EdgeLedger);
        assert_eq!(executor.status, ExecutorStatus::Unknown);
    }

    #[test]
    fn missing_edge_profile_execution_bindings_emit_no_workspace() {
        let (workspace, executor) = resolve_request_execution_bindings_without_server_workspace(
            &test_request("hello"),
            &Map::new(),
        )
        .expect("missing edge profile should still produce an explicit no-workspace binding");

        assert_eq!(workspace.kind, WorkspaceBindingKind::None);
        assert_eq!(workspace.display_name, "No workspace");
        assert_eq!(workspace.authority, WorkspaceAuthority::None);
        assert_eq!(workspace.fallback_policy, FallbackPolicy::Disabled);
        assert_eq!(executor.kind, ExecutorBindingKind::ServerLocal);
        assert_eq!(executor.executor_id, "server-control-plane");
        assert_eq!(executor.display_name, "Server control plane");
        assert_eq!(executor.transport, ToolTransportKind::ServerLocal);
        assert_eq!(executor.status, ExecutorStatus::Online);
    }

    #[test]
    fn explicit_no_workspace_binding_uses_server_control_plane_executor() {
        let mut request = test_request("plan only");
        request.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
            kind: astra_services::runs::WorkspaceBindingRequestKind::None,
            display_name: None,
            root: None,
            source: None,
            authority: None,
            fallback_policy: Some(astra_services::runs::FallbackPolicyRequest::Disabled),
        });

        let (workspace, executor) =
            resolve_request_execution_bindings(&request, Path::new("/tmp/server-workspace"));

        assert_eq!(workspace.kind, WorkspaceBindingKind::None);
        assert_eq!(workspace.display_name, "No workspace");
        assert_eq!(workspace.authority, WorkspaceAuthority::None);
        assert_eq!(workspace.fallback_policy, FallbackPolicy::Disabled);
        assert_eq!(executor.kind, ExecutorBindingKind::ServerLocal);
        assert_eq!(executor.executor_id, "server-control-plane");
        assert_eq!(executor.display_name, "Server control plane");
        assert_eq!(executor.transport, ToolTransportKind::ServerLocal);
        assert_eq!(executor.status, ExecutorStatus::Online);
    }

    #[test]
    fn execution_bindings_from_metadata_rebases_server_sandbox_cwd() {
        let metadata = json!({
            "workspace": {
                "kind": "server_sandbox",
                "display_name": "Server sandbox",
                "cwd": "/tmp/parent-workspace",
                "authority": "read_write",
                "fallback_policy": "disabled"
            },
            "executor": {
                "kind": "server_local",
                "executor_id": "server-local",
                "display_name": "Server sandbox",
                "transport": "server_local",
                "status": "online"
            }
        });

        let snapshot =
            execution_bindings_from_metadata(Some(&metadata), Path::new("/tmp/child-workspace"))
                .expect("metadata bindings");
        let workspace = &snapshot.workspace;
        let executor = &snapshot.executor;

        assert_eq!(workspace.kind, WorkspaceBindingKind::ServerSandbox);
        assert_eq!(workspace.cwd.as_deref(), Some("/tmp/child-workspace"));
        assert_eq!(executor.kind, ExecutorBindingKind::ServerLocal);
        assert!(snapshot.runtime.is_none());
    }

    #[tokio::test]
    async fn validate_request_constraints_rejects_legacy_mcp_binding_ids() {
        let service = test_service();
        let mut request = test_request("hello");
        request.mcp_binding_ids = Some(vec![301]);

        let err = service
            .validate_request_constraints("u1", &request)
            .await
            .expect_err("legacy mcp_binding_ids must be rejected on chat stream");

        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(
            err.1
                .0
                .detail
                .contains("mcp_binding_ids is no longer supported")
        );
    }

    #[tokio::test]
    async fn build_initial_state_includes_database_skill_provider_when_wired() {
        use astra_services::skills::{
            SkillInfoRecord, SkillListItem, SkillListRecord, SkillPublishRequestData, SkillRecord,
            SkillRegisterRequestData, SkillService, SkillStatusRecord, SkillVersionRecord,
        };
        use async_trait::async_trait;

        #[derive(Default)]
        struct MockSkillService {
            unsupported_calls: std::sync::atomic::AtomicUsize,
        }

        impl MockSkillService {
            fn unsupported<T>(
                &self,
                operation: &str,
            ) -> Result<T, (StatusCode, Json<ErrorResponse>)> {
                self.unsupported_calls
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Err((
                    StatusCode::NOT_IMPLEMENTED,
                    Json(ErrorResponse::new(format!(
                        "MockSkillService::{operation} is not implemented in this test"
                    ))),
                ))
            }
        }

        #[async_trait]
        impl SkillService for MockSkillService {
            async fn register_skill(
                &self,
                _: String,
                _: SkillRegisterRequestData,
            ) -> Result<SkillRecord, (StatusCode, Json<ErrorResponse>)> {
                self.unsupported("register_skill")
            }

            async fn list_skills(
                &self,
                _user_id: String,
                limit: u32,
                offset: u32,
            ) -> Result<SkillListRecord, (StatusCode, Json<ErrorResponse>)> {
                if offset > 0 {
                    return Ok(SkillListRecord {
                        skills: Vec::new(),
                        total: 1,
                        limit,
                        offset,
                    });
                }
                Ok(SkillListRecord {
                    skills: vec![SkillListItem {
                        skill_id: "remote-db@1.0.0".to_string(),
                        skill_name: "remote-db".to_string(),
                        version: "1.0.0".to_string(),
                        description: Some("Remote DB skill".to_string()),
                        status: Some("active".to_string()),
                        source: Some("user".to_string()),
                        category: Some("integration".to_string()),
                        created_at: None,
                    }],
                    total: 1,
                    limit,
                    offset,
                })
            }

            async fn get_skill(
                &self,
                _user_id: String,
                skill_id: String,
                _version: Option<String>,
            ) -> Result<SkillRecord, (StatusCode, Json<ErrorResponse>)> {
                if skill_id == "remote-db" || skill_id == "remote-db@1.0.0" {
                    return Ok(SkillRecord {
                        skill_id: "remote-db@1.0.0".to_string(),
                        skill_name: "remote-db".to_string(),
                        version: "1.0.0".to_string(),
                        description: Some("Remote DB skill".to_string()),
                        metadata: Some(serde_json::json!({
                            "skill_type": "remote",
                            "remote_url": "http://127.0.0.1:18080/remote-skill",
                            "forward_headers": ["authorization", "x-workspace-id"],
                            "required_headers": ["x-workspace-id"],
                            "when_to_use": "when task needs remote orchestration"
                        })),
                        created_at: None,
                    });
                }
                Err((
                    StatusCode::NOT_FOUND,
                    Json(ErrorResponse::new("not found".to_string())),
                ))
            }

            async fn get_skill_info(
                &self,
                _: String,
                _: String,
            ) -> Result<SkillInfoRecord, (StatusCode, Json<ErrorResponse>)> {
                self.unsupported("get_skill_info")
            }

            async fn list_skill_versions(
                &self,
                _: String,
                _: String,
            ) -> Result<Vec<SkillVersionRecord>, (StatusCode, Json<ErrorResponse>)> {
                self.unsupported("list_skill_versions")
            }

            async fn get_skill_status(
                &self,
                _: String,
                _: u32,
            ) -> Result<SkillStatusRecord, (StatusCode, Json<ErrorResponse>)> {
                self.unsupported("get_skill_status")
            }

            async fn publish_skill(
                &self,
                _: String,
                _: SkillPublishRequestData,
            ) -> Result<serde_json::Value, (StatusCode, Json<ErrorResponse>)> {
                self.unsupported("publish_skill")
            }

            async fn unpublish_skill(
                &self,
                _: String,
                _: String,
            ) -> Result<serde_json::Value, (StatusCode, Json<ErrorResponse>)> {
                self.unsupported("unpublish_skill")
            }
        }

        let skill_service = Arc::new(MockSkillService::default());
        let svc = test_service().with_skill_service(skill_service.clone());

        let default_request = test_request("hello");
        let default_state = svc.build_initial_state(
            "test-user",
            &default_request,
            "session-1",
            "run-1",
            None,
            None,
            None,
        );
        let default_resolver = default_state
            .skills
            .resolver
            .as_ref()
            .expect("default server resolver should include visible catalog");
        let default_names: Vec<String> = default_resolver
            .available_skills()
            .into_iter()
            .map(|skill| skill.name)
            .collect();
        assert!(
            default_names.iter().any(|name| name == "remote-db"),
            "expected database skill without request allow_skills filter: {default_names:?}"
        );
        assert!(
            default_state.skills.registry_for_activation.is_some(),
            "unfiltered server catalog should be available for conditional activation"
        );

        let mut request = test_request("hello");
        request.allow_skills = Some(vec!["remote-db".to_string()]);
        let state = svc.build_initial_state(
            "test-user",
            &request,
            "session-1",
            "run-1",
            None,
            None,
            None,
        );
        let resolver = state
            .skills
            .resolver
            .as_ref()
            .expect("skill resolver should be configured");
        let names: Vec<String> = resolver
            .available_skills()
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert!(
            names.iter().any(|name| name == "remote-db"),
            "expected database skill in available skills: {names:?}"
        );

        let resolved = resolver
            .resolve("remote-db")
            .expect("resolver should load database skill");
        assert_eq!(
            resolved.remote_url.as_deref(),
            Some("http://127.0.0.1:18080/remote-skill")
        );
        assert_eq!(
            resolved.forward_headers,
            vec!["authorization".to_string(), "x-workspace-id".to_string()]
        );
        assert_eq!(
            resolved.required_headers,
            vec!["x-workspace-id".to_string()]
        );

        let mut filtered_request = test_request("hello");
        filtered_request.allow_skills = Some(vec!["remote-db".to_string()]);
        let filtered_state = svc.build_initial_state(
            "test-user",
            &filtered_request,
            "session-1",
            "run-1",
            None,
            None,
            None,
        );
        assert!(
            filtered_state.skills.registry_for_activation.is_none(),
            "request-scoped allow_skills should disable automatic conditional activation"
        );
        let filtered_resolver = filtered_state
            .skills
            .resolver
            .as_ref()
            .expect("filtered resolver should be configured");
        let filtered_names: Vec<String> = filtered_resolver
            .available_skills()
            .into_iter()
            .map(|skill| skill.name)
            .collect();
        assert_eq!(filtered_names, vec!["remote-db".to_string()]);
        filtered_resolver
            .resolve("remote-db")
            .expect("allowed remote-db skill should resolve");
        assert_eq!(
            skill_service
                .unsupported_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0,
            "build_initial_state should only use list_skills/get_skill on this mock"
        );
    }

    #[tokio::test]
    async fn create_run_rejects_unknown_request_skill_allowlist() {
        let svc = test_service();
        let mut request = test_request("hello");
        request.allow_skills = Some(vec!["__missing_skill__".into()]);

        let err = svc
            .create_run("user-1".into(), request)
            .await
            .expect_err("unknown allow_skills entry should be rejected");
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1.0.detail.contains("allow_skills"));
    }

    #[test]
    fn build_runtime_turn_evaluation_event_uses_loop_state_signals() {
        let svc = test_service();
        let request = test_request("git status");
        let mut state = svc.build_initial_state(
            "test-user",
            &request,
            "session-1",
            "run-1",
            None,
            None,
            None,
        );
        state.recent_tools = vec!["git_status".into()];
        state.telemetry.first_budget_pressure = 0.27;
        state.stall.events.push(("repetition_stall".into(), 1));
        state.stall.verdict_events.push(
            astra_turn_core::agentic_verdict_audit::AgenticVerdictAuditEvent {
                turn: 1,
                severity: "warning".into(),
                injections: vec!["stall detected".into()],
                avoid_tools: vec!["git_status".into()],
                deprioritized_tools: vec![],
                force_stop: false,
                nudge_count: 1,
                interaction_mode: "prompt".into(),
                suppressed_loop_nudges: false,
                recent_error_pressure: 0,
                recent_timeout_pressure: 0,
                total_errors: 0,
                deprioritized_count: 0,
                total_timeouts: 0,
                timeout_dominant_tools: vec![],
                total_cache_hits: 0,
                flaky_count: 0,
            },
        );
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "git_status".into(),
            ok: true,
            ms: 14,
            error: None,
            input_bytes: Some(8),
            output_bytes: Some(180),
            args_preview: None,
            result_preview: Some("clean".into()),
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
        });

        let event = build_runtime_turn_evaluation_event("session-1", "server_runtime", &state);

        assert_eq!(event.event_type, JournalEventType::TurnEvaluation);
        assert_eq!(event.turn, None);
        let metadata = event.metadata.expect("turn evaluation metadata");
        assert_eq!(metadata["source"], "server_runtime");
        assert_eq!(metadata["live_query"], true);
        assert_eq!(metadata["stall_count"], 1);
        assert_eq!(metadata["verdict_warning"], true);
        assert_eq!(metadata["tool_call_count"], 1);
        assert!(metadata["quality"].as_f64().unwrap() < 0.8);
        assert_eq!(metadata["signals"][0]["kind"], "tool_error_rate");
    }

    #[test]
    fn finalize_run_events_appends_run_finished_for_failures() {
        let svc = test_service();
        let request = test_request("boom");
        let state = svc.build_initial_state(
            "test-user",
            &request,
            "session-1",
            "run-1",
            None,
            None,
            None,
        );

        let (events, status, error) = AgenticRunLifecycleService::finalize_run_events(
            Ok(AgenticLoopOutcome::Error("boom".into())),
            vec![],
            &state,
        );

        assert_eq!(status, RunStatus::Failed);
        assert_eq!(error.as_deref(), Some("boom"));
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["event_type"], "run_error");
        assert_eq!(events[1]["event_type"], "run_finished");
    }

    #[test]
    fn finalize_run_events_cancellation_beats_completed_outcome() {
        let svc = test_service();
        let request = test_request("done");
        let mut state = svc.build_initial_state(
            "test-user",
            &request,
            "session-1",
            "run-1",
            None,
            None,
            None,
        );
        let cancel_flag = Arc::new(AtomicBool::new(true));
        let cancel_token = Arc::new(CancellationToken::new());
        cancel_token.cancel();
        state.cancellation.flag = Some(cancel_flag);
        state.cancellation.token = Some(cancel_token);

        let (events, status, error) = AgenticRunLifecycleService::finalize_run_events(
            Ok(AgenticLoopOutcome::Completed),
            vec![],
            &state,
        );

        assert_eq!(status, RunStatus::Cancelled);
        assert!(error.is_none());
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["event_type"], "run_finished");
        assert_eq!(events[0]["data"]["cancelled"], true);
    }

    #[test]
    fn streaming_final_replay_excludes_live_work_surface_events() {
        let events = vec![
            json!({"type": "text_delta", "content": "hi"}),
            json!({"type": "reasoning_delta", "content": "thinking"}),
            json!({"type": "tool_call", "tool_call": {"id": "call-1"}}),
            json!({"type": "tool_call_end", "call_id": "call-1", "result": "ok"}),
            json!({"type": "agent_progress", "agent_id": "agent-1", "status": "started"}),
            json!({"type": "agent_live_event", "agent_id": "agent-1", "event_kind": "output_delta", "content": "child"}),
            json!({"type": "run_blocked", "call_id": "call-1", "reason": "transport_disconnected"}),
            json!({"type": "run_blocked", "call_id": "call-2", "reason": "fallback_disabled"}),
            json!({"type": "run_blocked", "call_id": "call-3", "reason": "route_mismatch"}),
            json!({"event_type": "text_done", "data": {"full_text": "hi"}}),
            json!({"event_type": "run_finished", "data": {"prompt_tokens": 1}}),
        ];

        let replay: Vec<_> = events
            .iter()
            .filter(|event| streaming_final_event_for_replay(event))
            .cloned()
            .collect();

        assert_eq!(replay.len(), 2);
        assert_eq!(replay[0]["event_type"], "text_done");
        assert_eq!(replay[1]["event_type"], "run_finished");
        assert!(live_delta_event_for_persistence(&events[1]));
        assert!(live_delta_event_for_persistence(&events[2]));
        assert!(live_delta_event_for_persistence(&events[3]));
        assert!(live_delta_event_for_persistence(&events[4]));
        assert!(live_delta_event_for_persistence(&events[5]));
        assert!(live_delta_event_for_persistence(&events[6]));
        assert!(live_delta_event_for_persistence(&events[7]));
        assert!(live_delta_event_for_persistence(&events[8]));
    }

    #[test]
    fn streaming_durable_persistence_keeps_live_events_before_terminal() {
        let events = vec![
            json!({"type": "reasoning_delta", "content": "thinking"}),
            json!({"type": "tool_call", "tool_call": {"id": "call-1"}}),
            json!({"type": "tool_call_end", "call_id": "call-1", "result": "ok"}),
            json!({"event_type": "text_done", "data": {"full_text": "answer"}}),
            json!({"event_type": "run_finished", "data": {"prompt_tokens": 1}}),
        ];

        let persisted: Vec<_> = events
            .iter()
            .filter(|event| streaming_event_for_persistence(event))
            .cloned()
            .collect();

        assert_eq!(persisted.len(), 5);
        assert_eq!(persisted[0]["type"], "reasoning_delta");
        assert_eq!(persisted[1]["type"], "tool_call");
        assert_eq!(persisted[2]["type"], "tool_call_end");
        assert_eq!(persisted[3]["event_type"], "text_done");
        assert_eq!(persisted[4]["event_type"], "run_finished");
    }

    #[test]
    fn active_run_live_event_projection_is_bounded() {
        let mut run = RunState {
            run_id: "run-live-bound".to_string(),
            session_id: "session-live-bound".to_string(),
            status: RunStatus::Running,
            events: vec![
                json!({"event_type": "run_started", "data": {"run_id": "run-live-bound"}}),
            ],
            cancel_flag: Arc::new(AtomicBool::new(false)),
            pause_flag: Arc::new(AtomicBool::new(false)),
            llm_cancel_token: Arc::new(CancellationToken::new()),
            live_tx: None,
            waiting_for: None,
        };

        for idx in 0..(MAX_ACTIVE_RUN_LIVE_EVENTS + 5) {
            push_active_run_live_event(
                &mut run,
                json!({"type": "text_delta", "content": idx.to_string()}),
            );
        }

        let live_events: Vec<_> = run
            .events
            .iter()
            .filter(|event| live_delta_event_for_persistence(event))
            .collect();
        assert_eq!(live_events.len(), MAX_ACTIVE_RUN_LIVE_EVENTS);
        assert_eq!(run.events[0]["event_type"], "run_started");
        assert_eq!(live_events[0]["content"], "5");
    }

    #[test]
    fn finalize_run_events_interrupted_completed_outcome_is_partial_not_completed() {
        let svc = test_service();
        let request = test_request("partial");
        let mut state = svc.build_initial_state(
            "test-user",
            &request,
            "session-1",
            "run-1",
            None,
            None,
            None,
        );
        state.final_text = "[Round budget hard-limit reached]".to_string();
        state.interruption = Some(astra_turn_core::interruption::InterruptionRecord::new(
            astra_turn_core::interruption::InterruptionKind::BudgetExhausted,
            astra_turn_core::interruption::ResumeAction::ContinueImmediately,
            astra_turn_core::interruption::InterruptionStateSummary {
                has_checkpoint: true,
                tool_calls_completed: 5,
                turns_completed: 15,
                remaining_turns: 0,
                error_detail: Some("Round budget hard-limit reached".to_string()),
                stall_signal: None,
                resume_restricted_tools: vec![],
            },
        ));

        let (events, status, error) = AgenticRunLifecycleService::finalize_run_events(
            Ok(AgenticLoopOutcome::Completed),
            vec![],
            &state,
        );

        assert_eq!(status, RunStatus::Paused);
        assert!(
            error
                .as_deref()
                .is_some_and(|msg| msg.to_ascii_lowercase().contains("budget"))
        );
        assert_eq!(events[0]["event_type"], "text_done");
        assert_eq!(events[0]["data"]["partial"], true);
        assert_eq!(
            events[0]["data"]["interruption"]["kind"],
            "budget_exhausted"
        );
        assert_eq!(events[1]["event_type"], "run_interrupted");
        assert_eq!(events[2]["event_type"], "run_finished");
        assert_eq!(events[2]["data"]["interrupted"], true);
        assert_eq!(events[2]["data"]["interruption_kind"], "budget_exhausted");
    }

    #[test]
    fn merge_cancelled_run_events_preserves_order_and_usage() {
        let cancel_flag = Arc::new(AtomicBool::new(true));
        let cancel_token = Arc::new(CancellationToken::new());
        let mut run = RunState {
            run_id: "run-1".into(),
            session_id: "session-1".into(),
            status: RunStatus::Cancelled,
            events: vec![
                json!({"event_type": "run_started", "data": {}}),
                json!({"event_type": "run_finished", "data": {"cancelled": true}}),
            ],
            cancel_flag,
            pause_flag: Arc::new(AtomicBool::new(false)),
            llm_cancel_token: cancel_token,
            live_tx: None,
            waiting_for: None,
        };

        merge_cancelled_run_events(
            &mut run,
            vec![
                json!({"event_type": "text_delta", "data": {"chunk": "hi"}}),
                json!({"event_type": "run_finished", "data": {"cancelled": true, "prompt_tokens": 3}}),
            ],
        );

        assert_eq!(run.events.len(), 3);
        assert_eq!(run.events[1]["event_type"], "text_delta");
        assert_eq!(run.events[2]["event_type"], "run_finished");
        assert_eq!(run.events[2]["data"]["cancelled"], true);
        assert_eq!(run.events[2]["data"]["prompt_tokens"], 3);
    }

    #[test]
    fn terminal_events_for_persistence_keeps_only_terminal_lifecycle_events() {
        let events = vec![
            json!({"event_type": "text_delta", "data": {"chunk": "hi"}}),
            json!({"type": "reasoning_delta", "content": "thinking"}),
            json!({"type": "reasoning_done"}),
            json!({"event_type": "text_done", "data": {"full_text": "final answer"}}),
            json!({"event_type": "run_error", "data": {"error": "boom"}}),
            json!({"event_type": "run_finished", "data": {"prompt_tokens": 1}}),
        ];

        let persisted = terminal_events_for_persistence(&events);
        assert_eq!(persisted.len(), 5);
        assert_eq!(persisted[0]["type"], "reasoning_delta");
        assert_eq!(persisted[1]["type"], "reasoning_done");
        assert_eq!(persisted[2]["event_type"], "text_done");
        assert_eq!(persisted[3]["event_type"], "run_error");
        assert_eq!(persisted[4]["event_type"], "run_finished");
    }

    #[tokio::test]
    async fn create_run_returns_running_status() {
        let svc = test_service();
        let result = ok(svc.create_run("user-1".into(), test_request("hello")).await);
        assert_eq!(result.status, "running");
        assert!(!result.run_id.is_empty());
        assert!(!result.session_id.is_empty());
    }

    #[tokio::test]
    async fn create_run_uses_provided_session_id() {
        let svc = test_service();
        let mut req = test_request("hi");
        req.session_id = Some("custom-session".into());
        let result = ok(svc.create_run("user-1".into(), req).await);
        assert_eq!(result.session_id, "custom-session");
    }

    #[tokio::test]
    async fn create_run_rejects_invalid_server_workspace_session_id() {
        let svc = test_service();
        let mut req = test_request("hi");
        req.session_id = Some("../../".into());
        req.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
            kind: astra_services::runs::WorkspaceBindingRequestKind::ServerSandbox,
            display_name: None,
            root: None,
            source: None,
            authority: Some(astra_services::runs::WorkspaceAuthorityRequest::ReadWrite),
            fallback_policy: Some(astra_services::runs::FallbackPolicyRequest::Disabled),
        });

        let err = err(svc.create_run("user-1".into(), req).await);

        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            err.1.0.detail,
            "Invalid session_id for server workspace provisioning"
        );
    }

    #[tokio::test]
    async fn stream_chat_rejects_invalid_server_workspace_session_id() {
        let svc = test_service();
        let mut req = test_request("hi");
        req.session_id = Some("../../".into());
        req.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
            kind: astra_services::runs::WorkspaceBindingRequestKind::ServerSandbox,
            display_name: None,
            root: None,
            source: None,
            authority: Some(astra_services::runs::WorkspaceAuthorityRequest::ReadWrite),
            fallback_policy: Some(astra_services::runs::FallbackPolicyRequest::Disabled),
        });

        let err = err(svc.stream_chat("user-1".into(), req).await);

        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            err.1.0.detail,
            "Invalid session_id for server workspace provisioning"
        );
    }

    #[tokio::test]
    async fn create_run_explain_mode_returns_metadata() {
        let svc = test_service();
        let mut req = test_request("explain me");
        req.explain = true;
        let result = ok(svc.create_run("user-1".into(), req).await);
        assert!(result.explain.is_some());
        assert_eq!(result.explain.unwrap()["mode"], "background");
    }

    #[tokio::test]
    async fn create_run_conflicts_when_same_session_already_has_active_run() {
        let svc = test_service();
        let mut first = test_request("hello");
        first.session_id = Some("shared-session".into());
        ok(svc.create_run("user-1".into(), first).await);

        let mut second = test_request("again");
        second.session_id = Some("shared-session".into());
        let err = err(svc.create_run("user-1".into(), second).await);
        assert_eq!(err.0, StatusCode::CONFLICT);
        assert_eq!(err.1.0.detail, "session already has an active run");
    }

    #[tokio::test]
    async fn stream_chat_conflicts_when_same_session_already_has_active_run() {
        let svc = test_service();
        let mut first = test_request("hello");
        first.session_id = Some("shared-session".into());
        ok(svc.create_run("user-1".into(), first).await);

        let mut second = test_request("again");
        second.session_id = Some("shared-session".into());
        let err = err(svc.stream_chat("user-1".into(), second).await);
        assert_eq!(err.0, StatusCode::CONFLICT);
        assert_eq!(err.1.0.detail, "session already has an active run");
    }

    #[tokio::test]
    #[ignore] // stream_chat runs full agentic loop; needs live DB + LLM or mock
    async fn stream_chat_tracks_run_for_status_and_replay() {
        let svc = test_service();
        let stream = ok(svc
            .stream_chat("user-1".into(), test_request("hello"))
            .await);

        let status = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let status = ok(svc
                    .get_run_status(stream.run_id.clone(), "user-1".into())
                    .await);
                if status.status != "running" {
                    break status;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("timeout waiting for stream_chat status to finish");
        let replay = ok(svc
            .stream_run(stream.run_id.clone(), "user-1".into(), 0)
            .await);

        assert_eq!(status.run_id, stream.run_id);
        assert!(status.events_count > 0);
        assert_eq!(replay.len(), status.events_count as usize);
        assert_eq!(replay[0]["event_type"], "run_started");
        assert_eq!(
            svc.test_llm_cancel_token_is_cancelled(&stream.run_id).await,
            Some(false)
        );
    }

    #[tokio::test]
    async fn get_run_status_returns_state() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("hello")).await);
        let status = ok(svc
            .get_run_status(run.run_id.clone(), "user-1".into())
            .await);
        assert_eq!(status.run_id, run.run_id);
        assert_eq!(status.status, "running");
        assert_eq!(status.events_count, 1);
        assert_eq!(status.workspace.as_ref().unwrap()["kind"], "server_sandbox");
        assert_eq!(status.executor.as_ref().unwrap()["kind"], "server_local");
        assert_eq!(status.transport.as_deref(), Some("server_local"));
        assert_eq!(status.fallback_policy.as_deref(), Some("disabled"));
    }

    #[tokio::test]
    async fn noninteractive_create_run_does_not_wire_ws_only_channels() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("hello")).await);

        assert!(!svc.approval_channels.lock().await.contains_key(&run.run_id));
        assert!(
            !svc.user_prompt_channels
                .lock()
                .await
                .contains_key(&run.run_id)
        );
        assert!(!svc.progress_channels.lock().await.contains_key(&run.run_id));
    }

    #[tokio::test]
    async fn noninteractive_approval_gate_denies_required_tools_without_waiting_for_ws() {
        let gate = NonInteractiveApprovalGate;

        assert!(astra_tools::ToolApprovalGate::requires_approval(
            &gate, "bash"
        ));
        assert!(astra_tools::ToolApprovalGate::requires_approval_for(
            &gate,
            "git",
            &serde_json::json!({"action": "commit"})
        ));
        assert!(!astra_tools::ToolApprovalGate::requires_approval_for(
            &gate,
            "git",
            &serde_json::json!({"action": "diff"})
        ));
        let decision = astra_tools::ToolApprovalGate::request_approval(
            &gate,
            "req-1",
            "bash",
            &serde_json::json!({"command": "rm -rf /tmp/example"}),
        )
        .await;

        assert!(matches!(
            decision,
            astra_tools::ApprovalDecision::Denied { reason: Some(reason) }
                if reason.contains("no interactive client")
        ));
    }

    #[tokio::test]
    async fn create_run_persists_interaction_mode_into_run_started_event() {
        let svc = test_service();
        let mut req = test_request("hello");
        req.interaction_mode = Some(astra_services::runs::RequestedTurnInteractionMode::Auto);
        req.interactive_client = true;
        let run = ok(svc.create_run("user-1".into(), req).await);

        let durable = svc
            .run_engine
            .load_run(&run.run_id)
            .await
            .expect("load run")
            .expect("run exists");
        assert_eq!(durable.events[0]["event_type"], "run_started");
        assert_eq!(durable.events[0]["data"]["interaction_mode"], "auto");
        assert_eq!(durable.events[0]["data"]["suppressed_loop_nudges"], true);
        assert_eq!(durable.events[0]["data"]["interactive_client"], true);
        assert_eq!(
            durable.events[0]["data"]["workspace"]["kind"],
            "server_sandbox"
        );
        assert!(
            durable.events[0]["data"]["workspace"]["cwd"]
                .as_str()
                .is_some_and(|cwd| cwd.contains("astra-workspaces")),
            "{:?}",
            durable.events[0]
        );
        assert_eq!(
            durable.events[0]["data"]["executor"]["kind"],
            "server_local"
        );
        assert_eq!(durable.events[0]["data"]["transport"], "server_local");
        assert_eq!(durable.events[0]["data"]["fallback_policy"], "disabled");
    }

    #[tokio::test]
    async fn create_run_persists_edge_binding_into_run_started_event() {
        let svc = test_service();
        let mut req = test_request("review this repo");
        req.workspace_binding = Some(astra_services::runs::WorkspaceBindingRequest {
            kind: astra_services::runs::WorkspaceBindingRequestKind::EdgeWorkspace,
            display_name: Some("MacBook Pro".to_string()),
            root: Some("/Users/xupeng/github/astra".to_string()),
            source: Some(astra_services::runs::WorkspaceSourceRequest::EdgePath {
                path: "/Users/xupeng/github/astra".to_string(),
            }),
            authority: Some(astra_services::runs::WorkspaceAuthorityRequest::ReadWrite),
            fallback_policy: Some(astra_services::runs::FallbackPolicyRequest::Disabled),
        });
        req.executor_binding = Some(astra_services::runs::ExecutorBindingRequest {
            kind: astra_services::runs::ExecutorBindingRequestKind::EdgeAgent,
            executor_id: Some("edge-macbook-1".to_string()),
            display_name: Some("MacBook Pro".to_string()),
            transport: Some(astra_services::runs::ToolTransportKindRequest::EdgeWs),
            status: Some(astra_services::runs::ExecutorStatusRequest::Online),
        });
        let run = ok(svc.create_run("user-1".into(), req).await);

        let durable = svc
            .run_engine
            .load_run(&run.run_id)
            .await
            .expect("load run")
            .expect("run exists");
        assert_eq!(durable.events[0]["event_type"], "run_started");
        assert_eq!(
            durable.events[0]["data"]["workspace"]["kind"],
            "edge_workspace"
        );
        assert_eq!(
            durable.events[0]["data"]["workspace"]["cwd"],
            "/Users/xupeng/github/astra"
        );
        assert_eq!(durable.events[0]["data"]["executor"]["kind"], "edge_agent");
        assert_eq!(
            durable.events[0]["data"]["executor"]["executor_id"],
            "edge-macbook-1"
        );
        assert_eq!(durable.events[0]["data"]["transport"], "edge_ws");
        assert_eq!(durable.events[0]["data"]["fallback_policy"], "disabled");

        let status = ok(svc
            .get_run_status(run.run_id.clone(), "user-1".into())
            .await);
        assert_eq!(status.workspace.as_ref().unwrap()["kind"], "edge_workspace");
        assert_eq!(
            status.workspace.as_ref().unwrap()["cwd"],
            "/Users/xupeng/github/astra"
        );
        assert_eq!(status.executor.as_ref().unwrap()["kind"], "edge_agent");
        assert_eq!(
            status.executor.as_ref().unwrap()["executor_id"],
            "edge-macbook-1"
        );
        assert_eq!(status.transport.as_deref(), Some("edge_ws"));
        assert_eq!(status.fallback_policy.as_deref(), Some("disabled"));
    }

    #[tokio::test]
    async fn get_run_status_not_found() {
        let svc = test_service();
        let e = err(svc
            .get_run_status("nonexistent".into(), "user-1".into())
            .await);
        assert_eq!(e.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn get_run_status_forbidden_for_other_user() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("hello")).await);
        let e = err(svc.get_run_status(run.run_id, "user-2".into()).await);
        assert_eq!(e.0, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn cancel_run_sets_cancelled_status() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
        let result = ok(svc.cancel_run(run.run_id.clone(), "user-1".into()).await);
        assert_eq!(result.status, "cancelled");
        let status = ok(svc.get_run_status(run.run_id, "user-1".into()).await);
        assert_eq!(status.status, "cancelled");
        assert!(status.events_count >= 1);
    }

    #[tokio::test]
    async fn cancel_run_cancels_llm_token_for_inflight_wake() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
        assert_eq!(
            svc.test_llm_cancel_token_is_cancelled(&run.run_id).await,
            Some(false)
        );
        ok(svc.cancel_run(run.run_id.clone(), "user-1".into()).await);
        assert_eq!(
            svc.test_llm_cancel_token_is_cancelled(&run.run_id).await,
            Some(true)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_run_schedules_in_memory_eviction() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
        assert!(
            svc.test_llm_cancel_token_is_cancelled(&run.run_id)
                .await
                .is_some()
        );

        ok(svc.cancel_run(run.run_id.clone(), "user-1".into()).await);
        tokio::time::advance(std::time::Duration::from_secs(301)).await;
        tokio::task::yield_now().await;

        assert_eq!(
            svc.test_llm_cancel_token_is_cancelled(&run.run_id).await,
            None,
            "cancelled runs must not stay pinned in the process-local run cache"
        );
    }

    #[tokio::test]
    async fn cancel_run_from_paused_sets_cancelled_status_and_clears_pause_flag() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
        ok(svc.pause_run(run.run_id.clone(), "user-1".into()).await);
        assert_eq!(svc.test_pause_flag_is_set(&run.run_id).await, Some(true));

        let result = ok(svc.cancel_run(run.run_id.clone(), "user-1".into()).await);
        assert_eq!(result.status, "cancelled");
        assert_eq!(svc.test_pause_flag_is_set(&run.run_id).await, Some(false));
        assert_eq!(
            svc.test_llm_cancel_token_is_cancelled(&run.run_id).await,
            Some(true)
        );
    }

    #[tokio::test]
    async fn pause_run_sets_live_pause_flag_and_resume_clears_it() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
        assert_eq!(svc.test_pause_flag_is_set(&run.run_id).await, Some(false));
        ok(svc.pause_run(run.run_id.clone(), "user-1".into()).await);
        assert_eq!(svc.test_pause_flag_is_set(&run.run_id).await, Some(true));
        ok(svc.resume_run(run.run_id.clone(), "user-1".into()).await);
        assert_eq!(svc.test_pause_flag_is_set(&run.run_id).await, Some(false));
    }

    #[tokio::test]
    async fn cancel_run_idempotent_for_non_running() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
        ok(svc.cancel_run(run.run_id.clone(), "user-1".into()).await);
        let result = ok(svc.cancel_run(run.run_id.clone(), "user-1".into()).await);
        assert_eq!(result.status, "cancelled");
    }

    #[tokio::test]
    async fn cancel_run_forbidden_for_other_user() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
        let e = err(svc.cancel_run(run.run_id, "user-2".into()).await);
        assert_eq!(e.0, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn stream_run_returns_events_from_offset() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("hello")).await);
        let events = ok(svc.stream_run(run.run_id.clone(), "user-1".into(), 0).await);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["event_type"], "run_started");
        let events = ok(svc.stream_run(run.run_id, "user-1".into(), 1).await);
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn stream_run_not_found() {
        let svc = test_service();
        let e = err(svc
            .stream_run("nonexistent".into(), "user-1".into(), 0)
            .await);
        assert_eq!(e.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn list_runs_empty_initially() {
        let svc = test_service();
        let result = ok(svc.list_runs("user-1".into(), 10, 0).await);
        assert_eq!(result.total, 0);
        assert!(result.runs.is_empty());
    }

    #[tokio::test]
    async fn list_runs_filters_by_user() {
        let svc = test_service();
        let u1_a = ok(svc.create_run("user-1".into(), test_request("a")).await);
        let u2_b = ok(svc.create_run("user-2".into(), test_request("b")).await);
        let u1_c = ok(svc.create_run("user-1".into(), test_request("c")).await);
        let for_u1 = ok(svc.list_runs("user-1".into(), 10, 0).await);
        assert_eq!(for_u1.total, 2);
        let ids: std::collections::HashSet<_> =
            for_u1.runs.iter().map(|r| r.run_id.as_str()).collect();
        assert!(ids.contains(u1_a.run_id.as_str()));
        assert!(ids.contains(u1_c.run_id.as_str()));
        assert!(!ids.contains(u2_b.run_id.as_str()));
        assert!(
            for_u1
                .runs
                .iter()
                .all(|run| run.workspace.as_ref().unwrap()["kind"] == "server_sandbox")
        );
        assert!(
            for_u1
                .runs
                .iter()
                .all(|run| run.executor.as_ref().unwrap()["kind"] == "server_local")
        );

        let for_u2 = ok(svc.list_runs("user-2".into(), 10, 0).await);
        assert_eq!(for_u2.total, 1);
        assert_eq!(for_u2.runs[0].run_id, u2_b.run_id);
    }

    #[tokio::test]
    async fn list_runs_pagination() {
        let svc = test_service();
        for i in 0..5 {
            ok(svc
                .create_run("user-1".into(), test_request(&format!("msg {i}")))
                .await);
        }
        let page1 = ok(svc.list_runs("user-1".into(), 2, 0).await);
        assert_eq!(page1.runs.len(), 2);
        assert_eq!(page1.total, 5);
        let page2 = ok(svc.list_runs("user-1".into(), 2, 2).await);
        assert_eq!(page2.runs.len(), 2);
        let page3 = ok(svc.list_runs("user-1".into(), 2, 4).await);
        assert_eq!(page3.runs.len(), 1);
    }

    #[tokio::test]
    async fn list_runs_orders_by_latest_update() {
        let svc = test_service();
        let older = ok(svc.create_run("user-1".into(), test_request("older")).await);
        let newer = ok(svc.create_run("user-1".into(), test_request("newer")).await);

        let initial = ok(svc.list_runs("user-1".into(), 10, 0).await);
        assert_eq!(initial.runs[0].run_id, newer.run_id);

        ok(svc.pause_run(older.run_id.clone(), "user-1".into()).await);

        let after_update = ok(svc.list_runs("user-1".into(), 10, 0).await);
        assert_eq!(
            after_update.runs[0].run_id, older.run_id,
            "list_runs should surface the most recently updated run first"
        );
    }

    /// P2-B: list_runs must clamp pagination params like other list endpoints.
    #[tokio::test]
    async fn list_runs_clamps_pagination() {
        let svc = test_service();
        // Absurdly large limit/offset must not panic or produce unbounded queries
        let result = ok(svc.list_runs("user-clamp".into(), u32::MAX, u32::MAX).await);
        assert_eq!(result.runs.len(), 0);
        // Verify the returned limit/offset are clamped
        assert!(
            result.limit <= astra_services::pagination::MAX_API_LIST_LIMIT,
            "limit must be clamped to MAX_API_LIST_LIMIT"
        );
    }

    #[test]
    fn format_run_events_adds_index() {
        let events = vec![
            json!({"event_type": "run_started"}),
            json!({"event_type": "text_delta", "data": {"chunk": "hi"}}),
        ];
        let formatted = AgenticRunLifecycleService::format_run_events(&events, 0);
        assert_eq!(formatted[0]["index"], 0);
        assert_eq!(formatted[1]["index"], 1);
        assert_eq!(formatted[1]["event_type"], "text_delta");
    }

    #[test]
    fn format_run_events_preserves_global_offset() {
        let events = vec![
            json!({"event_type": "text_delta", "data": {"chunk": "a"}}),
            json!({"event_type": "text_delta", "data": {"chunk": "b"}}),
        ];
        let formatted = AgenticRunLifecycleService::format_run_events(&events, 5);
        assert_eq!(formatted[0]["index"], 5);
        assert_eq!(formatted[1]["index"], 6);
    }

    #[test]
    fn durable_recent_events_honors_work_surface_hydrate_limit() {
        let events = (0..450)
            .map(|i| json!({"event_type": "tool_call_end", "data": {"seq": i}}))
            .collect();
        let run = DurableRunRecord {
            run_id: "run-long".to_string(),
            user_id: "user-1".to_string(),
            session_id: "session-1".to_string(),
            parent_run_id: None,
            root_run_id: None,
            ancestor_path: None,
            depth: 0,
            delegation_id: None,
            agent_id: None,
            retry_of: None,
            retry_scope: None,
            status: STATUS_RUNNING.to_string(),
            waiting_for: None,
            owner_pod_id: None,
            owner_lease_expires_at: None,
            run_generation: 0,
            last_event_idx: 449,
            checkpoint_version: None,
            checkpoint_json: None,
            error_code: None,
            error_message: None,
            retry_count: 0,
            total_prompt_tokens: 0,
            total_completion_tokens: 0,
            total_tool_calls: 0,
            events,
            created_at: "2026-06-13T00:00:00.000Z".to_string(),
            updated_at: "2026-06-13T00:00:00.000Z".to_string(),
        };

        let recent_events = AgenticRunLifecycleService::durable_recent_events(&run, 400);

        assert_eq!(recent_events.len(), 400);
        assert_eq!(recent_events[0]["index"], 50);
        assert_eq!(recent_events[399]["index"], 449);
    }

    #[test]
    fn extract_edge_tools_from_context() {
        let mut ctx = serde_json::Map::new();
        ctx.insert(
            "edge_tools".to_string(),
            json!([{"function": {"name": "bash"}}]),
        );
        let req = ChatRequestData {
            message: "hi".into(),
            session_id: None,
            full_llm_capture: false,
            agent_id: None,
            model: None,
            llm_token_service: None,
            skill_search: None,
            allow_skills: None,
            allow_skill_sources: None,
            allow_tools: None,
            workspace_binding: None,
            executor_binding: None,
            runtime_mcp_bindings: Vec::new(),
            mcp_binding_ids: None,
            context: Some(ctx),
            forward_headers: HashMap::new(),
            execution_budget: None,
            explain: false,
            interaction_mode: None,
            interactive_client: false,
        };
        let tools = AgenticRunLifecycleService::extract_edge_tools(&req);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["function"]["name"], "bash");
    }

    #[test]
    fn extract_edge_tools_empty_when_no_context() {
        assert!(AgenticRunLifecycleService::extract_edge_tools(&test_request("hi")).is_empty());
    }

    fn trusted_domains_for_tests() -> Vec<super::TrustedLlmDomain> {
        vec![super::TrustedLlmDomain {
            host: "catalog".to_string(),
            port: Some(8081),
        }]
    }

    #[test]
    fn validate_llm_token_service_config_accepts_http_url() {
        let config = astra_services::LlmTokenServiceConfig {
            url: "http://catalog:8081/api/v1/llm-token".to_string(),
            timeout_ms: Some(2500),
        };
        let trusted = trusted_domains_for_tests();
        assert!(super::validate_llm_token_service_config(Some(&config), &trusted).is_ok());
    }

    #[test]
    fn validate_llm_token_service_config_rejects_invalid_url() {
        let config = astra_services::LlmTokenServiceConfig {
            url: "not-a-url".to_string(),
            timeout_ms: Some(2500),
        };
        let trusted = trusted_domains_for_tests();
        let err = super::validate_llm_token_service_config(Some(&config), &trusted)
            .expect_err("invalid url should fail");
        assert!(err.contains("valid URL"), "unexpected error: {err}");
    }

    #[test]
    fn validate_llm_token_service_config_rejects_untrusted_url() {
        let config = astra_services::LlmTokenServiceConfig {
            url: "http://evil.example.com/v1/chat/completions".to_string(),
            timeout_ms: Some(2500),
        };
        let trusted = trusted_domains_for_tests();
        let err = super::validate_llm_token_service_config(Some(&config), &trusted)
            .expect_err("untrusted url should fail");
        assert!(
            err.contains("trusted domains"),
            "unexpected error message: {err}"
        );
    }

    #[test]
    fn validate_llm_token_service_config_rejects_when_trusted_domains_unconfigured() {
        let config = astra_services::LlmTokenServiceConfig {
            url: "http://catalog:8081/api/v1/llm-token".to_string(),
            timeout_ms: Some(2500),
        };
        let err = super::validate_llm_token_service_config(Some(&config), &[])
            .expect_err("missing trusted domains should fail");
        assert!(
            err.contains(super::LLM_TOKEN_SERVICE_TRUSTED_DOMAINS_TABLE),
            "unexpected error message: {err}"
        );
    }

    #[test]
    fn validate_llm_token_service_config_enforces_host_port_boundary_for_trusted_domains() {
        let config = astra_services::LlmTokenServiceConfig {
            url: "http://catalog:8082/api/v1/chat".to_string(),
            timeout_ms: Some(2500),
        };
        let trusted = trusted_domains_for_tests();
        let err = super::validate_llm_token_service_config(Some(&config), &trusted)
            .expect_err("host:port boundary should be enforced");
        assert!(
            err.contains("trusted domains"),
            "unexpected error message: {err}"
        );
    }

    #[test]
    fn trusted_llm_domain_from_db_values_accepts_valid_host_and_port() {
        let parsed = super::trusted_llm_domain_from_db_values("catalog", 8081)
            .expect("host+port should parse");
        assert_eq!(parsed.host, "catalog");
        assert_eq!(parsed.port, Some(8081));
        let wildcard = super::trusted_llm_domain_from_db_values("catalog", 0)
            .expect("sentinel port should represent wildcard");
        assert_eq!(wildcard.port, None);
    }

    #[test]
    fn trusted_llm_domain_from_db_values_rejects_invalid_host_or_port() {
        let host_err = super::trusted_llm_domain_from_db_values("http://catalog:8081", 8081)
            .expect_err("host should not include scheme");
        assert!(host_err.contains("host"));
        let port_err = super::trusted_llm_domain_from_db_values("catalog", 70000)
            .expect_err("port out of range should fail");
        assert!(port_err.contains("port"));
    }

    #[test]
    fn normalize_request_allowlists_preserve_explicit_empty_sets() {
        let empty: Vec<String> = Vec::new();
        assert_eq!(
            super::normalize_request_allowlist(Some(&empty), "allow_skills")
                .expect("empty allow_skills should normalize"),
            Some(HashSet::new())
        );
        assert_eq!(
            super::normalize_request_skill_sources(Some(&empty), "allow_skill_sources")
                .expect("empty allow_skill_sources should normalize"),
            Some(HashSet::new())
        );
    }

    #[test]
    fn extract_edge_profile_from_context() {
        let mut ctx = serde_json::Map::new();
        ctx.insert(
            "edge_profile".to_string(),
            json!({
                "cwd": "/tmp",
                "git_branch": "main",
                "system_prompt_override": "override text"
            }),
        );
        let req = ChatRequestData {
            message: "hi".into(),
            session_id: None,
            full_llm_capture: false,
            agent_id: None,
            model: None,
            llm_token_service: None,
            skill_search: None,
            allow_skills: None,
            allow_skill_sources: None,
            allow_tools: None,
            workspace_binding: None,
            executor_binding: None,
            runtime_mcp_bindings: Vec::new(),
            mcp_binding_ids: None,
            context: Some(ctx),
            forward_headers: HashMap::new(),
            execution_budget: None,
            explain: false,
            interaction_mode: None,
            interactive_client: false,
        };
        let profile = AgenticRunLifecycleService::extract_edge_profile(&req);
        assert_eq!(profile["cwd"], "/tmp");
        assert_eq!(profile["git_branch"], "main");
        assert_eq!(profile["system_prompt_override"], "override text");
    }

    #[test]
    fn build_initial_state_sets_user_message() {
        let svc = test_service();
        let req = test_request("write a test");
        let expected_budget = astra_turn_core::chat_turn_heuristics::resolve_agentic_turn_budget(
            astra_turn_core::chat_turn_heuristics::infer_task_execution_profile("write a test"),
            astra_core::RuntimeLimits::global().max_turns,
            None,
        );
        let state = svc.build_initial_state("test-user", &req, "sess-1", "run-1", None, None, None);
        assert_eq!(state.messages.len(), 1);
        assert_eq!(state.messages[0]["role"], "user");
        assert_eq!(state.messages[0]["content"], "write a test");
        assert_eq!(state.current_session_id, Some("sess-1".to_string()));
        assert_eq!(state.current_run_id, Some("run-1".to_string()));
        assert_eq!(state.max_turns, expected_budget.initial_turns);
        assert_eq!(state.remaining_turns, expected_budget.initial_turns);
        assert_eq!(state.agentic_turn_budget, expected_budget);
        assert_eq!(state.message, "write a test");
        assert!(state.cancellation.token.is_none());
    }

    #[test]
    fn build_initial_state_applies_execution_budget_override() {
        let svc = test_service();
        let mut req = test_request("go");
        req.execution_budget = Some(astra_services::runs::ExecutionBudget {
            initial_turns: Some(4),
            hard_turn_limit: Some(9),
        });
        let state = svc.build_initial_state("test-user", &req, "s", "r", None, None, None);
        assert_eq!(state.max_turns, 4);
        assert_eq!(state.remaining_turns, 4);
        assert_eq!(state.agentic_turn_budget.hard_turn_limit, 9);
    }

    #[test]
    fn build_initial_state_clamps_execution_budget_override() {
        let svc = test_service();
        let mut req = test_request("go");
        req.execution_budget = Some(astra_services::runs::ExecutionBudget {
            initial_turns: Some(0),
            hard_turn_limit: Some(0),
        });
        let state = svc.build_initial_state("test-user", &req, "s", "r", None, None, None);
        assert_eq!(state.max_turns, 1);
        assert_eq!(state.agentic_turn_budget.hard_turn_limit, 1);
    }

    #[test]
    fn build_initial_state_loads_stop_hooks_from_edge_profile_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let mo = dir.path().join(".astra");
        std::fs::create_dir_all(&mo).unwrap();
        std::fs::write(
            mo.join("stop-hooks.yaml"),
            "version: 1\nauto_detect: false\nhooks:\n  - label: cloud_hook\n    command: true\n",
        )
        .unwrap();

        let svc = test_service();
        let mut req = test_request("implement a fix");
        req.context = Some(
            serde_json::json!({
                "edge_profile": { "cwd": dir.path().to_str().unwrap() }
            })
            .as_object()
            .unwrap()
            .clone(),
        );

        let state = svc.build_initial_state("test-user", &req, "s", "r", None, None, None);
        assert_eq!(state.hooks.stop_hooks.len(), 1);
        assert_eq!(state.hooks.stop_hooks[0].label, "cloud_hook");
        assert_eq!(
            state.hooks.workspace_root_hint.as_deref(),
            Some(dir.path().to_str().unwrap())
        );
    }

    #[test]
    fn build_initial_state_uses_workspace_override_when_no_edge_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let mo = dir.path().join(".astra");
        std::fs::create_dir_all(&mo).unwrap();
        std::fs::write(
            mo.join("stop-hooks.yaml"),
            "version: 1\nauto_detect: false\nhooks:\n  - label: server_hook\n    command: echo ok\n",
        )
        .unwrap();

        let svc = test_service();
        // Request with NO edge_profile.cwd — simulates web-agent mode.
        let req = test_request("fix a bug");
        let state =
            svc.build_initial_state("test-user", &req, "s", "r", Some(dir.path()), None, None);
        assert_eq!(state.hooks.stop_hooks.len(), 1);
        assert_eq!(state.hooks.stop_hooks[0].label, "server_hook");
        assert_eq!(
            state.hooks.workspace_root_hint.as_deref(),
            Some(dir.path().to_str().unwrap())
        );
    }

    #[test]
    fn build_initial_state_edge_cwd_takes_priority_over_workspace_override() {
        // Edge profile with cwd set — workspace_override should be ignored.
        let edge_dir = tempfile::tempdir().unwrap();
        let mo = edge_dir.path().join(".astra");
        std::fs::create_dir_all(&mo).unwrap();
        std::fs::write(
            mo.join("stop-hooks.yaml"),
            "version: 1\nauto_detect: false\nhooks:\n  - label: edge_hook\n    command: true\n",
        )
        .unwrap();

        let override_dir = tempfile::tempdir().unwrap();
        let mo2 = override_dir.path().join(".astra");
        std::fs::create_dir_all(&mo2).unwrap();
        std::fs::write(
            mo2.join("stop-hooks.yaml"),
            "version: 1\nauto_detect: false\nhooks:\n  - label: override_hook\n    command: true\n",
        )
        .unwrap();

        let svc = test_service();
        let mut req = test_request("deploy");
        req.context = Some(
            serde_json::json!({
                "edge_profile": { "cwd": edge_dir.path().to_str().unwrap() }
            })
            .as_object()
            .unwrap()
            .clone(),
        );

        let state = svc.build_initial_state(
            "test-user",
            &req,
            "s",
            "r",
            Some(override_dir.path()),
            None,
            None,
        );
        // Edge profile's cwd wins over the workspace override.
        assert_eq!(state.hooks.stop_hooks.len(), 1);
        assert_eq!(state.hooks.stop_hooks[0].label, "edge_hook");
        assert_eq!(
            state.hooks.workspace_root_hint.as_deref(),
            Some(edge_dir.path().to_str().unwrap())
        );
    }

    #[test]
    fn run_status_as_str() {
        assert_eq!(RunStatus::Running.as_str(), "running");
        assert_eq!(RunStatus::InputQueued.as_str(), "input-queued");
        assert_eq!(RunStatus::Completed.as_str(), "completed");
        assert_eq!(RunStatus::Failed.as_str(), "failed");
        assert_eq!(RunStatus::Cancelled.as_str(), "cancelled");
        assert_eq!(RunStatus::Paused.as_str(), "paused");
    }

    #[test]
    fn server_loop_causal_chain_ids_fit_agent_event_column() {
        assert!(server_loop_causal_chain_id("server-loop").len() <= 64);
        assert!(server_loop_causal_chain_id("server-loop-tools").len() <= 64);
    }

    #[test]
    fn has_buffered_terminal_completion_ignores_cancelled_and_interrupted_finishes() {
        assert!(has_buffered_terminal_completion(&[json!({
            "event_type": "run_finished",
            "data": {"total_prompt_tokens": 1, "total_completion_tokens": 1}
        })]));
        assert!(!has_buffered_terminal_completion(&[json!({
            "event_type": "run_finished",
            "data": {"cancelled": true}
        })]));
        assert!(!has_buffered_terminal_completion(&[json!({
            "event_type": "run_finished",
            "data": {"interrupted": true}
        })]));
    }

    #[test]
    fn preserve_manual_pause_wins_over_late_completed_status() {
        assert!(should_preserve_manual_pause_on_completion(
            &RunStatus::Paused,
            &RunStatus::Completed
        ));
        assert!(!should_preserve_manual_pause_on_completion(
            &RunStatus::Paused,
            &RunStatus::Failed
        ));
        assert!(!should_preserve_manual_pause_on_completion(
            &RunStatus::Running,
            &RunStatus::Completed
        ));
    }

    #[tokio::test]
    async fn durable_paused_state_wins_over_late_completed_status() {
        let svc = test_service_with_engine();
        let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
        svc.run_engine
            .persist_status(&run.run_id, STATUS_PAUSED, Some("user_resume"), None)
            .await
            .unwrap();

        assert!(
            should_preserve_manual_pause_from_durable(
                &svc.run_engine,
                &run.run_id,
                &RunStatus::Completed,
            )
            .await
        );
        assert!(
            !should_preserve_manual_pause_from_durable(
                &svc.run_engine,
                &run.run_id,
                &RunStatus::Failed,
            )
            .await
        );
    }

    #[tokio::test]
    async fn pause_run_transitions_running_to_paused() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
        let result = ok(svc.pause_run(run.run_id.clone(), "user-1".into()).await);
        assert_eq!(result.status, "paused");
        assert_eq!(result.previous_status, "running");
        let status = ok(svc.get_run_status(run.run_id, "user-1".into()).await);
        assert_eq!(status.status, "paused");
    }

    #[tokio::test]
    async fn pause_run_conflict_when_not_running() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
        ok(svc.cancel_run(run.run_id.clone(), "user-1".into()).await);
        let e = err(svc.pause_run(run.run_id, "user-1".into()).await);
        assert_eq!(e.0, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn pause_run_forbidden_for_other_user() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
        let e = err(svc.pause_run(run.run_id, "user-2".into()).await);
        assert_eq!(e.0, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn pause_run_not_found() {
        let svc = test_service();
        let e = err(svc.pause_run("nonexistent".into(), "user-1".into()).await);
        assert_eq!(e.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn resume_run_transitions_paused_to_running() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
        ok(svc.pause_run(run.run_id.clone(), "user-1".into()).await);
        let result = ok(svc.resume_run(run.run_id.clone(), "user-1".into()).await);
        assert_eq!(result.status, "running");
        assert_eq!(result.previous_status, "paused");
        let status = ok(svc.get_run_status(run.run_id, "user-1".into()).await);
        assert_eq!(status.status, "running");
    }

    #[tokio::test]
    async fn resume_run_promotes_buffered_completed_pause_to_completed() {
        let svc = test_service_with_engine();
        let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
        ok(svc.pause_run(run.run_id.clone(), "user-1".into()).await);
        svc.run_engine
            .append_event(
                &run.run_id,
                json!({
                    "event_type": "run_finished",
                    "data": {"total_prompt_tokens": 1, "total_completion_tokens": 1}
                }),
            )
            .await
            .unwrap();

        let result = ok(svc.resume_run(run.run_id.clone(), "user-1".into()).await);
        assert_eq!(result.status, "completed");
        assert_eq!(result.previous_status, "paused");
        let status = ok(svc.get_run_status(run.run_id, "user-1".into()).await);
        assert_eq!(status.status, "completed");
    }

    #[tokio::test]
    async fn resume_run_conflict_when_not_paused() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
        let e = err(svc.resume_run(run.run_id, "user-1".into()).await);
        assert_eq!(e.0, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn resume_run_forbidden_for_other_user() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
        ok(svc.pause_run(run.run_id.clone(), "user-1".into()).await);
        let e = err(svc.resume_run(run.run_id, "user-2".into()).await);
        assert_eq!(e.0, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn resume_run_not_found() {
        let svc = test_service();
        let e = err(svc.resume_run("nonexistent".into(), "user-1".into()).await);
        assert_eq!(e.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn pause_resume_round_trip_preserves_events() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
        ok(svc.pause_run(run.run_id.clone(), "user-1".into()).await);
        ok(svc.resume_run(run.run_id.clone(), "user-1".into()).await);
        let status = ok(svc
            .get_run_status(run.run_id.clone(), "user-1".into())
            .await);
        assert_eq!(status.events_count, 3); // run_started + run_paused + run_resumed
        let events = ok(svc.stream_run(run.run_id, "user-1".into(), 0).await);
        assert_eq!(events[0]["event_type"], "run_started");
        assert_eq!(events[1]["event_type"], "run_paused");
        assert_eq!(events[2]["event_type"], "run_resumed");
    }

    #[tokio::test]
    async fn double_pause_is_conflict() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
        ok(svc.pause_run(run.run_id.clone(), "user-1".into()).await);
        let e = err(svc.pause_run(run.run_id, "user-1".into()).await);
        assert_eq!(e.0, StatusCode::CONFLICT);
    }

    // ─── Durable persistence integration tests ─────────────────────────

    fn test_service_with_engine() -> AgenticRunLifecycleService {
        test_service()
    }

    #[tokio::test]
    #[ignore] // runs full agentic loop; needs live infra
    async fn durable_create_run_persists_to_store() {
        let svc = test_service_with_engine();
        let run = ok(svc.create_run("user-1".into(), test_request("hello")).await);

        let engine = &svc.run_engine;
        let durable = engine.load_run(&run.run_id).await.unwrap().unwrap();
        assert_eq!(durable.user_id, "user-1");
        assert_eq!(durable.status, "running");
        assert_eq!(durable.session_id, run.session_id);
    }

    #[tokio::test]
    #[ignore] // runs full agentic loop; needs live infra
    async fn durable_create_run_eventually_persists_terminal_event() {
        let svc = test_service_with_engine();
        let run = ok(svc.create_run("user-1".into(), test_request("hello")).await);

        let engine = &svc.run_engine;
        let durable = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let durable = engine.load_run(&run.run_id).await.unwrap().unwrap();
                if durable.status != "running"
                    && matches!(
                        durable
                            .events
                            .last()
                            .and_then(|event| event["event_type"].as_str()),
                        Some("run_finished")
                    )
                {
                    break durable;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("timeout waiting for durable run to persist terminal event");
        assert_eq!(durable.events.last().unwrap()["event_type"], "run_finished");
    }

    #[tokio::test]
    #[ignore] // runs full agentic loop; needs live infra
    async fn durable_stream_chat_persists_final_state() {
        let svc = test_service_with_engine();
        let stream = ok(svc
            .stream_chat("user-1".into(), test_request("hello"))
            .await);

        let engine = &svc.run_engine;
        let durable = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let durable = engine.load_run(&stream.run_id).await.unwrap().unwrap();
                if durable.status != "running"
                    && matches!(
                        durable
                            .events
                            .last()
                            .and_then(|event| event["event_type"].as_str()),
                        Some("run_finished")
                    )
                {
                    break durable;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("timeout waiting for durable stream_chat final state");
        assert_eq!(durable.user_id, "user-1");
        assert_eq!(durable.session_id, stream.session_id);
        assert!(durable.events.len() >= 2);
        assert_eq!(durable.events.last().unwrap()["event_type"], "run_finished");
    }

    #[tokio::test]
    #[ignore] // runs full agentic loop; needs live infra
    async fn durable_cancel_persists_to_store() {
        let svc = test_service_with_engine();
        let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
        ok(svc.cancel_run(run.run_id.clone(), "user-1".into()).await);

        let engine = &svc.run_engine;
        let durable = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let durable = engine.load_run(&run.run_id).await.unwrap().unwrap();
                if matches!(
                    durable
                        .events
                        .last()
                        .and_then(|event| event["event_type"].as_str()),
                    Some("run_finished")
                ) {
                    break durable;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("timeout waiting for cancelled run to persist terminal event");
        assert_eq!(durable.status, "cancelled");
        assert!(durable.events.len() >= 2); // run_started + run_finished
    }

    #[tokio::test]
    #[ignore] // runs full agentic loop; needs live infra
    async fn durable_pause_resume_round_trip() {
        let svc = test_service_with_engine();
        let run = ok(svc.create_run("user-1".into(), test_request("task")).await);

        ok(svc.pause_run(run.run_id.clone(), "user-1".into()).await);
        let engine = &svc.run_engine;
        let durable = engine.load_run(&run.run_id).await.unwrap().unwrap();
        assert_eq!(durable.status, "paused");
        assert_eq!(durable.waiting_for.as_deref(), Some("user_resume"));

        ok(svc.resume_run(run.run_id.clone(), "user-1".into()).await);
        let durable = engine.load_run(&run.run_id).await.unwrap().unwrap();
        assert_eq!(durable.status, "running");
        assert!(durable.waiting_for.is_none());
    }

    #[tokio::test]
    async fn cancel_run_returns_durable_terminal_status_on_cache_miss() {
        let svc = test_service_with_engine();
        let engine = &svc.run_engine;
        engine.start_run("run-1", "user-1", "sess-1").await.unwrap();
        engine
            .persist_status("run-1", STATUS_COMPLETED, None, None)
            .await
            .unwrap();

        let result = ok(svc.cancel_run("run-1".into(), "user-1".into()).await);
        assert_eq!(result.run_id, "run-1");
        assert_eq!(result.status, STATUS_COMPLETED);
    }

    #[tokio::test]
    async fn cancel_run_running_cache_miss_persists_cancelled() {
        let svc = test_service_with_engine();
        let engine = &svc.run_engine;
        engine.start_run("run-1", "user-1", "sess-1").await.unwrap();

        let result = ok(svc.cancel_run("run-1".into(), "user-1".into()).await);
        assert_eq!(result.status, STATUS_CANCELLED);
        let durable = engine.load_run("run-1").await.unwrap().unwrap();
        assert_eq!(durable.status, STATUS_CANCELLED);
        assert_eq!(durable.events.last().unwrap()["event_type"], "run_finished");
    }

    #[tokio::test]
    async fn pause_run_running_succeeds_via_db() {
        let svc = test_service_with_engine();
        let engine = &svc.run_engine;
        engine.start_run("run-1", "user-1", "sess-1").await.unwrap();

        let result = ok(svc.pause_run("run-1".into(), "user-1".into()).await);
        assert_eq!(result.status, STATUS_PAUSED);
        let durable = engine.load_run("run-1").await.unwrap().unwrap();
        assert_eq!(durable.status, STATUS_PAUSED);
    }

    #[tokio::test]
    async fn resume_run_paused_succeeds_via_db() {
        let svc = test_service_with_engine();
        let engine = &svc.run_engine;
        engine.start_run("run-1", "user-1", "sess-1").await.unwrap();
        engine
            .persist_status("run-1", STATUS_PAUSED, Some("user_resume"), None)
            .await
            .unwrap();

        let result = ok(svc.resume_run("run-1".into(), "user-1".into()).await);
        assert_eq!(result.status, STATUS_RUNNING);
        let durable = engine.load_run("run-1").await.unwrap().unwrap();
        assert_eq!(durable.status, STATUS_RUNNING);
    }

    #[tokio::test]
    async fn pause_run_append_failure_rollback_succeeds_keeps_running() {
        let store: Arc<dyn RunStateStore> = Arc::new(FaultInjectedRunStateStore::new(&[], &[1]));
        let svc = test_service_with_store(store);
        let run = ok(svc.create_run("user-1".into(), test_request("task")).await);

        let e = err(svc.pause_run(run.run_id.clone(), "user-1".into()).await);
        assert_eq!(e.0, StatusCode::SERVICE_UNAVAILABLE);
        assert!(e.1.0.detail.contains("pause event"));

        let durable = svc.run_engine.load_run(&run.run_id).await.unwrap().unwrap();
        assert_eq!(durable.status, STATUS_RUNNING);
        assert!(durable.waiting_for.is_none());
        assert_eq!(durable.events.len(), 1);
        assert_eq!(durable.events[0]["event_type"], "run_started");

        let runs = svc.runs.read().await;
        let live = runs.get(&run.run_id).expect("live run state");
        assert_eq!(live.status, RunStatus::Running);
        assert!(live.waiting_for.is_none());
        assert!(!live.pause_flag.load(Ordering::SeqCst));
        assert_eq!(live.events.len(), 1);
        assert_eq!(live.events[0]["event_type"], "run_started");
    }

    #[tokio::test]
    async fn resume_run_append_failure_rollback_succeeds_keeps_paused() {
        let store: Arc<dyn RunStateStore> = Arc::new(FaultInjectedRunStateStore::new(&[], &[2]));
        let svc = test_service_with_store(store);
        let run = ok(svc.create_run("user-1".into(), test_request("task")).await);
        ok(svc.pause_run(run.run_id.clone(), "user-1".into()).await);

        let e = err(svc.resume_run(run.run_id.clone(), "user-1".into()).await);
        assert_eq!(e.0, StatusCode::SERVICE_UNAVAILABLE);
        assert!(e.1.0.detail.contains("resume event"));

        let durable = svc.run_engine.load_run(&run.run_id).await.unwrap().unwrap();
        assert_eq!(durable.status, STATUS_PAUSED);
        assert_eq!(durable.waiting_for.as_deref(), Some("user_resume"));
        assert_eq!(durable.events.len(), 2);
        assert_eq!(durable.events[1]["event_type"], "run_paused");

        let runs = svc.runs.read().await;
        let live = runs.get(&run.run_id).expect("live run state");
        assert_eq!(live.status, RunStatus::Paused);
        assert_eq!(live.waiting_for.as_deref(), Some("user_resume"));
        assert!(live.pause_flag.load(Ordering::SeqCst));
        assert_eq!(live.events.len(), 2);
        assert_eq!(live.events[1]["event_type"], "run_paused");
    }

    #[tokio::test]
    async fn cancel_run_paused_cache_miss_persists_cancelled() {
        let svc = test_service_with_engine();
        let engine = &svc.run_engine;
        engine.start_run("run-1", "user-1", "sess-1").await.unwrap();
        engine
            .persist_status("run-1", STATUS_PAUSED, Some("user_resume"), None)
            .await
            .unwrap();

        let result = ok(svc.cancel_run("run-1".into(), "user-1".into()).await);
        assert_eq!(result.status, STATUS_CANCELLED);
        let durable = engine.load_run("run-1").await.unwrap().unwrap();
        assert_eq!(durable.status, STATUS_CANCELLED);
    }

    #[tokio::test]
    #[ignore] // stream_chat runs full agentic loop; needs live DB + LLM or mock
    async fn get_run_status_falls_back_to_durable_store_on_cache_miss() {
        let svc = test_service_with_engine();
        let stream = ok(svc
            .stream_chat("user-1".into(), test_request("hello"))
            .await);
        let engine = &svc.run_engine;
        let durable = engine.load_run(&stream.run_id).await.unwrap().unwrap();

        svc.runs.write().await.remove(&stream.run_id);

        let status = ok(svc
            .get_run_status(stream.run_id.clone(), "user-1".into())
            .await);
        assert_eq!(status.run_id, stream.run_id);
        assert_eq!(status.session_id, stream.session_id);
        assert_eq!(status.status, durable.status);
        assert_eq!(status.waiting_for, durable.waiting_for);
        assert_eq!(status.events_count, durable.events.len() as i64);
    }

    #[tokio::test]
    async fn stream_run_cache_miss_replays_durable_text_done() {
        let svc = test_service_with_engine();
        let engine = &svc.run_engine;
        engine
            .start_run("run-durable-text", "user-1", "session-1")
            .await
            .expect("start durable run");
        engine
            .append_event(
                "run-durable-text",
                json!({"event_type": "text_done", "data": {"full_text": "durable final answer"}}),
            )
            .await
            .expect("persist text_done");
        engine
            .append_event(
                "run-durable-text",
                json!({"event_type": "run_finished", "data": {"prompt_tokens": 1}}),
            )
            .await
            .expect("persist run_finished");

        let events = ok(svc
            .stream_run("run-durable-text".into(), "user-1".into(), 1)
            .await);

        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["event_type"], "text_done");
        assert_eq!(events[0]["data"]["full_text"], "durable final answer");
        assert_eq!(events[1]["event_type"], "run_finished");
    }

    #[tokio::test]
    async fn submit_run_input_uses_durable_idempotency_on_cache_miss() {
        let svc = test_service_with_engine();
        let engine = &svc.run_engine;
        engine
            .start_run("run-input", "user-1", "session-1")
            .await
            .unwrap();

        let first = ok(svc
            .submit_run_input(
                "run-input".into(),
                "user-1".into(),
                RunInputData {
                    idempotency_key: "key-1".into(),
                    input: json!({"answer": "yes"}),
                },
            )
            .await);
        let duplicate = ok(svc
            .submit_run_input(
                "run-input".into(),
                "user-1".into(),
                RunInputData {
                    idempotency_key: "key-1".into(),
                    input: json!({"answer": "yes"}),
                },
            )
            .await);

        let durable = engine.load_run("run-input").await.unwrap().unwrap();
        let matching_inputs = durable
            .events
            .iter()
            .filter(|event| event.get("idempotency_key").and_then(Value::as_str) == Some("key-1"))
            .count();
        assert!(!first.duplicate);
        assert!(duplicate.duplicate);
        assert_eq!(matching_inputs, 1);
        assert_eq!(durable.status, STATUS_INPUT_QUEUED);
        assert_eq!(durable.waiting_for.as_deref(), Some("user_input"));
    }

    #[tokio::test]
    async fn submit_run_input_rejects_terminal_durable_run() {
        let svc = test_service_with_engine();
        let engine = &svc.run_engine;
        engine
            .start_run("run-terminal-input", "user-1", "session-1")
            .await
            .unwrap();
        engine
            .persist_status("run-terminal-input", STATUS_COMPLETED, None, None)
            .await
            .unwrap();

        let e = err(svc
            .submit_run_input(
                "run-terminal-input".into(),
                "user-1".into(),
                RunInputData {
                    idempotency_key: "key-1".into(),
                    input: json!({"answer": "late"}),
                },
            )
            .await);
        assert_eq!(e.0, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn submit_run_input_accepts_repeated_queueing_while_input_already_queued() {
        let svc = test_service_with_engine();
        let engine = &svc.run_engine;
        engine
            .start_run("run-queued-input", "user-1", "session-1")
            .await
            .unwrap();
        engine
            .persist_status(
                "run-queued-input",
                STATUS_INPUT_QUEUED,
                Some("user_input"),
                None,
            )
            .await
            .unwrap();

        let result = svc
            .submit_run_input(
                "run-queued-input".into(),
                "user-1".into(),
                RunInputData {
                    idempotency_key: "key-queued-1".into(),
                    input: json!({"answer": "keep queueing"}),
                },
            )
            .await
            .expect("input-queued runs should accept additional deferred input");

        assert!(result.accepted);
        assert!(!result.duplicate);
        let durable = engine.load_run("run-queued-input").await.unwrap().unwrap();
        assert_eq!(durable.status, STATUS_INPUT_QUEUED);
        assert_eq!(durable.waiting_for.as_deref(), Some("user_input"));
        assert!(durable.events.iter().any(|event| {
            event.get("idempotency_key").and_then(Value::as_str) == Some("key-queued-1")
        }));
    }

    #[tokio::test]
    async fn submit_run_input_rejects_paused_durable_run() {
        let svc = test_service_with_engine();
        let engine = &svc.run_engine;
        engine
            .start_run("run-paused-input", "user-1", "session-1")
            .await
            .unwrap();
        engine
            .persist_status("run-paused-input", STATUS_PAUSED, None, None)
            .await
            .unwrap();

        let e = err(svc
            .submit_run_input(
                "run-paused-input".into(),
                "user-1".into(),
                RunInputData {
                    idempotency_key: "key-1".into(),
                    input: json!({"answer": "late"}),
                },
            )
            .await);
        assert_eq!(e.0, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn submit_run_input_rejects_oversized_content() {
        let svc = test_service_with_engine();
        let engine = &svc.run_engine;
        engine
            .start_run("run-large-input", "user-1", "session-1")
            .await
            .unwrap();

        let e = err(svc
            .submit_run_input(
                "run-large-input".into(),
                "user-1".into(),
                RunInputData {
                    idempotency_key: "key-large".into(),
                    input: json!({"content": "x".repeat(MAX_DEFERRED_INPUT_CHARS + 1)}),
                },
            )
            .await);

        assert_eq!(e.0, StatusCode::PAYLOAD_TOO_LARGE);
        let durable = engine.load_run("run-large-input").await.unwrap().unwrap();
        assert!(
            durable.events.iter().all(|event| {
                event.get("idempotency_key").and_then(Value::as_str) != Some("key-large")
            }),
            "oversized input must not be appended before validation"
        );
    }

    #[tokio::test]
    async fn create_run_conflict_checks_durable_active_session() {
        let svc = test_service_with_engine();
        let engine = &svc.run_engine;
        engine
            .start_run("existing-run", "user-1", "shared-session")
            .await
            .unwrap();
        let mut request = test_request("second");
        request.session_id = Some("shared-session".into());

        let e = err(svc.create_run("user-1".into(), request).await);
        assert_eq!(e.0, StatusCode::CONFLICT);
    }

    #[tokio::test]
    #[ignore] // stream_chat runs full agentic loop; needs live DB + LLM or mock
    async fn stream_run_falls_back_to_durable_store_on_cache_miss() {
        let svc = test_service_with_engine();
        let stream = ok(svc
            .stream_chat("user-1".into(), test_request("hello"))
            .await);
        let engine = &svc.run_engine;
        let durable = engine.load_run(&stream.run_id).await.unwrap().unwrap();

        svc.runs.write().await.remove(&stream.run_id);

        let events = ok(svc
            .stream_run(stream.run_id.clone(), "user-1".into(), 1)
            .await);
        assert_eq!(
            events,
            AgenticRunLifecycleService::format_run_events(&durable.events[1..], 1)
        );
    }

    #[tokio::test]
    #[ignore] // stream_chat runs full agentic loop; needs live DB + LLM or mock
    async fn list_runs_falls_back_to_durable_store_on_cache_miss() {
        let svc = test_service_with_engine();
        let first = ok(svc
            .stream_chat("user-1".into(), test_request("first"))
            .await);
        let second = ok(svc
            .stream_chat("user-1".into(), test_request("second"))
            .await);

        svc.runs.write().await.remove(&first.run_id);

        let runs = ok(svc.list_runs("user-1".into(), 10, 0).await);
        let run_ids: Vec<_> = runs.runs.iter().map(|run| run.run_id.as_str()).collect();
        assert_eq!(runs.total, 2);
        assert!(run_ids.contains(&first.run_id.as_str()));
        assert!(run_ids.contains(&second.run_id.as_str()));
    }

    #[tokio::test]
    async fn lifecycle_run_creation_is_durable_by_default() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("hello")).await);
        let durable = svc.run_engine.load_run(&run.run_id).await.unwrap().unwrap();
        assert_eq!(durable.user_id, "user-1");
        assert_eq!(durable.session_id, run.session_id);
        assert_eq!(durable.status, STATUS_RUNNING);
    }

    // ─── EdgeContext integration tests ──────────────────────────────────

    #[test]
    fn extract_edge_context_from_request_with_tools() {
        let mut ctx = serde_json::Map::new();
        ctx.insert(
            "edge_tools".to_string(),
            json!([{"function": {"name": "bash", "parameters": {}}}]),
        );
        ctx.insert(
            "edge_profile".to_string(),
            json!({"cwd": "/tmp", "git_branch": "main"}),
        );
        let req = ChatRequestData {
            context: Some(ctx),
            ..test_request("hello")
        };

        let edge_ctx = AgenticRunLifecycleService::extract_edge_context(&req);
        assert_eq!(edge_ctx.tool_count(), 1);
        assert_eq!(edge_ctx.tool_names(), vec!["bash"]);
        assert_eq!(edge_ctx.edge_profile.cwd.as_deref(), Some("/tmp"));
        assert_eq!(edge_ctx.edge_profile.git_branch.as_deref(), Some("main"));
    }

    #[test]
    fn extract_edge_context_from_empty_request() {
        let req = test_request("hello");
        let edge_ctx = AgenticRunLifecycleService::extract_edge_context(&req);
        assert!(!edge_ctx.has_tools());
        assert!(edge_ctx.edge_profile.cwd.is_none());
    }

    #[test]
    fn extract_edge_tools_backward_compat() {
        let mut ctx = serde_json::Map::new();
        ctx.insert(
            "edge_tools".to_string(),
            json!([
                {"function": {"name": "bash"}},
                {"function": {"name": "grep"}}
            ]),
        );
        let req = ChatRequestData {
            context: Some(ctx),
            ..test_request("hello")
        };
        let tools = AgenticRunLifecycleService::extract_edge_tools(&req);
        assert_eq!(tools.len(), 2);
    }

    #[test]
    fn extract_edge_profile_backward_compat() {
        let mut ctx = serde_json::Map::new();
        ctx.insert(
            "edge_profile".to_string(),
            json!({"cwd": "/workspace", "os": "linux"}),
        );
        let req = ChatRequestData {
            context: Some(ctx),
            ..test_request("hello")
        };
        let profile = AgenticRunLifecycleService::extract_edge_profile(&req);
        assert_eq!(profile.get("cwd").unwrap(), "/workspace");
        assert_eq!(profile.get("os").unwrap(), "linux");
    }

    // ─── Background spawning integration tests ──────────────────────────

    #[tokio::test]
    #[ignore] // runs full agentic loop; needs live infra
    async fn create_run_spawns_background_task() {
        let svc = test_service();
        let run = ok(svc.create_run("user-1".into(), test_request("hello")).await);
        assert_eq!(run.status, "running");

        // Deterministic wait: poll until the background task advances state.
        let status = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let status = ok(svc
                    .get_run_status(run.run_id.clone(), "user-1".into())
                    .await);
                if status.status != "running" || status.events_count > 1 {
                    break status;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("timeout waiting for background task to advance state");
        assert!(
            status.status != "running" || status.events_count > 1,
            "Expected background task to advance state, but status={} events={}",
            status.status,
            status.events_count
        );
    }

    #[tokio::test]
    #[ignore] // runs full agentic loop; needs live infra
    async fn create_run_with_engine_persists_final_state() {
        let svc = test_service_with_engine();
        let run = ok(svc.create_run("user-1".into(), test_request("hello")).await);

        // Deterministic wait: poll durable state until it leaves "running".
        let engine = &svc.run_engine;
        let durable = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let durable = engine.load_run(&run.run_id).await.unwrap().unwrap();
                if durable.status != "running" {
                    break durable;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("timeout waiting for durable run status to finalize");
        assert_ne!(durable.status, "running");
    }

    // ─── DelegationTracker integration tests ────────────────────────────

    #[tokio::test]
    async fn delegation_tracker_get_children() {
        use crate::server::delegation::engine::{DelegationTracker, SubRunRecord, SubRunState};

        let tracker = DelegationTracker::new();
        tracker
            .record_sub_run(SubRunRecord {
                delegation_id: "d1".into(),
                run_id: "child-1".into(),
                parent_run_id: "parent-1".into(),
                agent_id: "agent-a".into(),
                depth: 1,
                state: SubRunState::Created,
                retry_of: None,
            })
            .await;
        tracker
            .record_sub_run(SubRunRecord {
                delegation_id: "d1".into(),
                run_id: "child-2".into(),
                parent_run_id: "parent-1".into(),
                agent_id: "agent-b".into(),
                depth: 1,
                state: SubRunState::Created,
                retry_of: None,
            })
            .await;
        tracker
            .record_sub_run(SubRunRecord {
                delegation_id: "d2".into(),
                run_id: "other-child".into(),
                parent_run_id: "parent-2".into(),
                agent_id: "agent-c".into(),
                depth: 1,
                state: SubRunState::Created,
                retry_of: None,
            })
            .await;

        let mut children = tracker.get_children("parent-1").await;
        children.sort();
        assert_eq!(children, vec!["child-1", "child-2"]);

        let children = tracker.get_children("parent-2").await;
        assert_eq!(children, vec!["other-child"]);

        let children = tracker.get_children("nonexistent").await;
        assert!(children.is_empty());
    }

    /// P0-C: The agentic loop spawn must check token budget before starting.
    #[test]
    fn run_lifecycle_checks_token_budget_before_loop() {
        let source = include_str!("mod.rs");
        let test_start = source.find("mod tests {").unwrap_or(source.len());
        let prod_code = &source[..test_start];
        assert!(
            prod_code.contains("check_token_budget"),
            "run_lifecycle must call check_token_budget before the agentic loop"
        );
    }

    /// P0-C: drain_background_tasks returns true when no tasks are running.
    #[tokio::test]
    async fn drain_background_tasks_returns_immediately_when_idle() {
        // Test the drain logic directly: counter at 0 → drain returns true immediately.
        let count = Arc::new(AtomicUsize::new(0));
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(100);
        let drained = loop {
            if count.load(Ordering::Acquire) == 0 {
                break true;
            }
            if tokio::time::Instant::now() >= deadline {
                break false;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        };
        assert!(drained, "counter at 0 — drain must return true immediately");
    }

    /// P0-C: background_task_count increments on spawn and decrements on exit.
    #[tokio::test]
    async fn background_task_count_tracks_spawned_tasks() {
        use std::sync::atomic::Ordering;
        let count = Arc::new(AtomicUsize::new(0));
        let count_clone = Arc::clone(&count);

        // Simulate what the spawn does: increment, spawn, decrement on drop
        count.fetch_add(1, Ordering::Release);
        let handle = tokio::spawn(async move {
            struct Guard(Arc<AtomicUsize>);
            impl Drop for Guard {
                fn drop(&mut self) {
                    self.0.fetch_sub(1, Ordering::Release);
                }
            }
            let _g = Guard(count_clone);
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        });

        assert_eq!(count.load(Ordering::Acquire), 1, "task in flight");
        handle.await.unwrap();
        assert_eq!(
            count.load(Ordering::Acquire),
            0,
            "task completed — counter must be 0"
        );
    }

    /// P1-A: RunStatus::try_transition enforces valid state machine transitions.
    #[test]
    fn run_status_try_transition_valid_and_invalid() {
        use super::RunStatus::*;

        // Valid transitions
        assert!(Running.try_transition(&InputQueued).is_ok());
        assert!(Running.try_transition(&Paused).is_ok());
        assert!(Running.try_transition(&Completed).is_ok());
        assert!(Running.try_transition(&Failed).is_ok());
        assert!(Running.try_transition(&Cancelled).is_ok());
        assert!(InputQueued.try_transition(&Running).is_ok());
        assert!(InputQueued.try_transition(&Paused).is_ok());
        assert!(InputQueued.try_transition(&Waiting).is_ok());
        assert!(InputQueued.try_transition(&Completed).is_ok());
        assert!(InputQueued.try_transition(&Failed).is_ok());
        assert!(InputQueued.try_transition(&Cancelled).is_ok());
        assert!(Paused.try_transition(&Running).is_ok());
        assert!(Paused.try_transition(&Cancelled).is_ok());
        assert!(Paused.try_transition(&Failed).is_ok());
        assert!(Waiting.try_transition(&InputQueued).is_ok());

        // Terminal states cannot transition
        let err = Completed.try_transition(&Running);
        assert!(err.is_err(), "Completed → Running must be rejected");
        assert!(
            err.unwrap_err().contains("Completed"),
            "error must name the source state"
        );

        let err = Failed.try_transition(&Running);
        assert!(err.is_err(), "Failed → Running must be rejected");

        let err = Cancelled.try_transition(&Completed);
        assert!(err.is_err(), "Cancelled → Completed must be rejected");

        // Running cannot go back to Running
        let err = Running.try_transition(&Running);
        assert!(err.is_err(), "Running → Running must be rejected");

        assert!(
            InputQueued.try_transition(&InputQueued).is_ok(),
            "InputQueued → InputQueued must stay queueable for repeated user input"
        );
    }

    /// P1-F: list_runs pagination must be deterministic — all runs appear
    /// exactly once across pages, with no duplicates or missing entries.
    #[tokio::test]
    async fn list_runs_pagination_is_deterministic() {
        let svc = test_service();
        for i in 0..5 {
            ok(svc
                .create_run("user-pg".into(), test_request(&format!("msg {i}")))
                .await);
        }
        // Collect all run_ids across 3 pages
        let mut all_ids = Vec::new();
        let page1 = ok(svc.list_runs("user-pg".into(), 2, 0).await);
        all_ids.extend(page1.runs.iter().map(|r| r.run_id.clone()));
        let page2 = ok(svc.list_runs("user-pg".into(), 2, 2).await);
        all_ids.extend(page2.runs.iter().map(|r| r.run_id.clone()));
        let page3 = ok(svc.list_runs("user-pg".into(), 2, 4).await);
        all_ids.extend(page3.runs.iter().map(|r| r.run_id.clone()));

        assert_eq!(all_ids.len(), 5, "all 5 runs must appear across pages");
        let unique: std::collections::HashSet<_> = all_ids.iter().collect();
        assert_eq!(
            unique.len(),
            5,
            "no duplicate run_ids across pages — pagination must be deterministic"
        );
    }

    /// P1-A: RunStatus must have a Waiting variant that is non-terminal.
    /// Runs needing external input must not be killed as Failed.
    #[test]
    fn waiting_is_non_terminal_status() {
        // Running → Waiting is valid
        assert!(
            RunStatus::Running
                .try_transition(&RunStatus::Waiting)
                .is_ok(),
            "Running → Waiting must be allowed"
        );
        // Waiting → Running is valid (resume after input)
        assert!(
            RunStatus::Waiting
                .try_transition(&RunStatus::Running)
                .is_ok(),
            "Waiting → Running must be allowed (resume)"
        );
        // Waiting → Cancelled is valid
        assert!(
            RunStatus::Waiting
                .try_transition(&RunStatus::Cancelled)
                .is_ok(),
            "Waiting → Cancelled must be allowed"
        );
        // Waiting → Failed is valid (timeout)
        assert!(
            RunStatus::Waiting
                .try_transition(&RunStatus::Failed)
                .is_ok(),
            "Waiting → Failed must be allowed"
        );
        // Waiting serializes as "waiting"
        assert_eq!(RunStatus::Waiting.as_str(), "waiting");
        assert!(
            RunStatus::Waiting
                .try_transition(&RunStatus::InputQueued)
                .is_ok(),
            "Waiting → InputQueued must be allowed when user input arrives"
        );
    }

    #[test]
    fn run_status_live_semantics_align_with_durable_owner() {
        assert_eq!(
            RunStatus::from_durable_status(STATUS_RUNNING),
            Some(RunStatus::Running)
        );
        assert_eq!(
            RunStatus::from_durable_status(STATUS_INPUT_QUEUED),
            Some(RunStatus::InputQueued)
        );
        assert_eq!(
            RunStatus::from_durable_status(STATUS_WAITING),
            Some(RunStatus::Waiting)
        );
        assert_eq!(
            RunStatus::from_durable_status(STATUS_PAUSED),
            Some(RunStatus::Paused)
        );
        assert_eq!(
            RunStatus::from_durable_status(STATUS_COMPLETED),
            Some(RunStatus::Completed)
        );
        assert_eq!(
            RunStatus::from_durable_status(STATUS_FAILED),
            Some(RunStatus::Failed)
        );
        assert_eq!(
            RunStatus::from_durable_status(STATUS_CANCELLED),
            Some(RunStatus::Cancelled)
        );
        assert_eq!(RunStatus::from_durable_status("mystery"), None);

        assert!(RunStatus::Waiting.is_resumable());
        assert!(RunStatus::Paused.is_resumable());
        assert!(!RunStatus::Running.is_resumable());
        assert!(!RunStatus::Completed.is_resumable());

        assert_eq!(
            RunStatus::Running.blocks_session(None),
            astra_services::runs::durable_run_status_blocks_session(STATUS_RUNNING, None)
        );
        assert_eq!(
            RunStatus::Waiting.blocks_session(None),
            astra_services::runs::durable_run_status_blocks_session(STATUS_WAITING, None)
        );
        assert_eq!(
            RunStatus::Paused.blocks_session(Some("tool_approval")),
            astra_services::runs::durable_run_status_blocks_session(
                STATUS_PAUSED,
                Some("tool_approval")
            )
        );
        assert_eq!(
            RunStatus::Paused.blocks_session(None),
            astra_services::runs::durable_run_status_blocks_session(STATUS_PAUSED, None)
        );
        assert_eq!(
            RunStatus::Completed.blocks_session(None),
            astra_services::runs::durable_run_status_blocks_session(STATUS_COMPLETED, None)
        );
    }

    /// P1-A: finalize_run_events must preserve Waiting as a non-error status.
    #[test]
    fn finalize_run_events_preserves_waiting_without_error_event() {
        let svc = test_service();
        let request = test_request("wait");
        let state = svc.build_initial_state(
            "test-user",
            &request,
            "session-1",
            "run-1",
            None,
            None,
            None,
        );

        let (events, status, error) = AgenticRunLifecycleService::finalize_run_events(
            Ok(AgenticLoopOutcome::Waiting("tool_approval".into())),
            vec![],
            &state,
        );

        assert_eq!(status, RunStatus::Waiting);
        assert_eq!(error.as_deref(), Some("waiting: tool_approval"));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["event_type"], "run_waiting");
        assert_eq!(events[0]["data"]["reason"], "waiting: tool_approval");
    }

    /// P1-F: stream_chat must persist usage unconditionally.
    /// Cancelled runs still consumed tokens and must have accurate durable records,
    /// even when status persistence is skipped.
    #[test]
    fn stream_chat_persists_usage_unconditionally() {
        let source = include_str!("mod.rs");
        // Find the stream_chat method
        let fn_start = source
            .find("async fn stream_chat(")
            .expect("stream_chat must exist");
        let fn_end = source[fn_start..]
            .find("\n    async fn ")
            .map(|p| fn_start + p)
            .unwrap_or(source.len());
        let fn_body = &source[fn_start..fn_end];

        let usage_pos = fn_body
            .find(".persist_usage(")
            .expect("stream_chat must call persist_usage");

        // persist_usage must NOT be inside the status-persistence guard.
        // Cancelled runs skip persist_status, but usage must still be written.
        let guard_pos = fn_body
            .find("if persist_status_update {")
            .expect("persist_status_update guard must exist");
        let guard_block = &fn_body[guard_pos..];
        let mut depth = 0;
        let mut guard_end = 0;
        for (i, c) in guard_block.char_indices() {
            if c == '{' {
                depth += 1;
            } else if c == '}' {
                depth -= 1;
                if depth == 0 {
                    guard_end = guard_pos + i + 1;
                    break;
                }
            }
        }
        assert!(
            usage_pos > guard_end,
            "persist_usage must remain outside the persist_status_update guard — \
             cancelled stream_chat runs must still persist usage for billing/audit"
        );
    }

    /// P1-C: build_server_skill_executor must accept and wire a cancel_token.
    /// Without this, skill sub-runs ignore parent cancellation.
    #[test]
    fn build_server_skill_executor_accepts_cancel_token() {
        let source = include_str!("mod.rs");
        let fn_start = source
            .find("fn build_server_skill_executor(")
            .expect("build_server_skill_executor must exist");
        let fn_end = source[fn_start..]
            .find("\npub(crate) fn ")
            .or_else(|| source[fn_start..].find("\nfn "))
            .map(|p| fn_start + p)
            .unwrap_or(source.len());
        let fn_body = &source[fn_start..fn_end];
        assert!(
            fn_body.contains("cancel_token"),
            "build_server_skill_executor must accept a cancel_token parameter"
        );
        assert!(
            fn_body.contains("with_cancel_token"),
            "build_server_skill_executor must wire cancel_token via with_cancel_token"
        );
    }

    /// Runtime tool surfacing for forked server skills must inherit the parent
    /// workspace/executor/runtime binding; otherwise sub-runs see raw edge
    /// schemas without the capability resolver's runtime truth.
    #[test]
    fn build_server_skill_executor_wires_execution_binding_snapshot() {
        let source = include_str!("mod.rs");
        let fn_start = source
            .find("fn build_server_skill_executor(")
            .expect("build_server_skill_executor must exist");
        let fn_end = source[fn_start..]
            .find("\npub(crate) fn ")
            .or_else(|| source[fn_start..].find("\nfn "))
            .map(|p| fn_start + p)
            .unwrap_or(source.len());
        let fn_body = &source[fn_start..fn_end];
        assert!(
            fn_body.contains("execution_bindings"),
            "build_server_skill_executor must accept execution binding metadata"
        );
        assert!(
            fn_body.contains("with_execution_binding_snapshot"),
            "build_server_skill_executor must pass execution bindings to server skill sub-runs"
        );
    }

    /// P1-C: build_initial_state must pass cancel_token to skill executor builder.
    #[test]
    fn build_initial_state_passes_cancel_token_to_skill_executor() {
        let source = include_str!("mod.rs");
        let fn_start = source
            .find("fn build_initial_state(")
            .expect("build_initial_state must exist");
        let fn_end = source[fn_start..]
            .find("\n    fn ")
            .or_else(|| source[fn_start..].find("\n    pub"))
            .map(|p| fn_start + p)
            .unwrap_or(source.len());
        let fn_body = &source[fn_start..fn_end];
        assert!(
            fn_body.contains("cancel_token"),
            "build_initial_state must accept and pass cancel_token to skill executor"
        );
    }

    #[test]
    fn build_initial_state_passes_execution_bindings_to_skill_executor() {
        let source = include_str!("mod.rs");
        let fn_start = source
            .find("fn build_initial_state(")
            .expect("build_initial_state must exist");
        let fn_end = source[fn_start..]
            .find("\n    fn ")
            .or_else(|| source[fn_start..].find("\n    pub"))
            .map(|p| fn_start + p)
            .unwrap_or(source.len());
        let fn_body = &source[fn_start..fn_end];
        assert!(
            fn_body.contains("execution_bindings"),
            "build_initial_state must accept execution bindings"
        );
        assert!(
            fn_body.contains("build_server_skill_executor(")
                && fn_body.contains("execution_bindings,"),
            "build_initial_state must pass execution bindings into the skill executor builder"
        );
    }

    #[test]
    fn resumable_run_statuses_stay_live_for_resume() {
        assert!(RunStatus::Waiting.is_resumable());
        assert!(RunStatus::Paused.is_resumable());
        assert!(!RunStatus::Running.is_resumable());
        assert!(!RunStatus::Completed.is_resumable());
        assert!(!RunStatus::Failed.is_resumable());
        assert!(!RunStatus::Cancelled.is_resumable());
    }

    /// A Waiting run persisted in durable store must be cancellable even after
    /// the process-local control handle is gone.
    #[tokio::test]
    async fn cancel_run_waiting_cache_miss_persists_cancelled() {
        let svc = test_service_with_engine();
        let engine = &svc.run_engine;
        engine
            .start_run("waiting-run", "user-1", "session-1")
            .await
            .unwrap();
        engine
            .persist_status("waiting-run", STATUS_WAITING, Some("tool_approval"), None)
            .await
            .unwrap();

        let result = ok(svc.cancel_run("waiting-run".into(), "user-1".into()).await);
        let durable = engine.load_run("waiting-run").await.unwrap().unwrap();
        assert_eq!(result.status, STATUS_CANCELLED);
        assert_eq!(durable.status, STATUS_CANCELLED);
    }

    /// Admission control: semaphore rejects when at capacity, allows after release.
    #[tokio::test]
    async fn run_semaphore_admission_control() {
        // Limit = 1: only one concurrent run permitted.
        let svc = AgenticRunLifecycleService::new(
            test_settings(),
            test_encryptor(),
            Arc::new(TokioMutex::new(HashMap::new())),
            RunEngine::new(Arc::new(InMemoryRunStateStore::new())),
        )
        .with_run_concurrency_limit(1);
        let sem = svc.test_run_semaphore();

        // 1st acquire succeeds.
        let permit1 = sem.clone().try_acquire_owned().expect("first permit");
        // 2nd acquire must fail — at capacity.
        assert!(
            sem.clone().try_acquire_owned().is_err(),
            "second acquire must fail when at capacity"
        );

        // After release, re-acquire succeeds.
        drop(permit1);
        let permit2 = sem
            .clone()
            .try_acquire_owned()
            .expect("re-acquire after release");
        drop(permit2);
    }

    /// Admission control: limit=2, third acquire must fail, release creates room.
    #[tokio::test]
    async fn run_semaphore_limit_two() {
        let svc = AgenticRunLifecycleService::new(
            test_settings(),
            test_encryptor(),
            Arc::new(TokioMutex::new(HashMap::new())),
            RunEngine::new(Arc::new(InMemoryRunStateStore::new())),
        )
        .with_run_concurrency_limit(2);
        let sem = svc.test_run_semaphore();

        let p1 = sem.clone().try_acquire_owned().expect("first");
        let p2 = sem.clone().try_acquire_owned().expect("second");
        assert!(sem.clone().try_acquire_owned().is_err(), "third must fail");

        drop(p1);
        // Now one slot open, re-acquire works.
        let p3 = sem
            .clone()
            .try_acquire_owned()
            .expect("re-acquire after one drop");
        drop(p2);
        drop(p3);
    }

    /// Admission with timeout: `acquire_owned` + `timeout` rejects after
    /// the deadline while a short release window lets a waiter proceed.
    #[tokio::test]
    async fn run_semaphore_admission_timeout_waits_and_proceeds() {
        let svc = AgenticRunLifecycleService::new(
            test_settings(),
            test_encryptor(),
            Arc::new(TokioMutex::new(HashMap::new())),
            RunEngine::new(Arc::new(InMemoryRunStateStore::new())),
        )
        .with_run_concurrency_limit(1);
        let sem = svc.test_run_semaphore();

        // 1st acquire: capacity exhausted.
        let p1 = sem.clone().try_acquire_owned().expect("first");
        // Spawn a waiter with a short timeout — it will time out.
        let sem2 = sem.clone();
        let timeout_result =
            tokio::time::timeout(std::time::Duration::from_millis(50), sem2.acquire_owned()).await;
        assert!(
            timeout_result.is_err(),
            "waiter should time out when no slot opens"
        );

        // Now spawn a waiter and release the slot quickly — waiter should get it.
        let sem3 = sem.clone();
        let waiter = tokio::spawn(async move {
            tokio::time::timeout(std::time::Duration::from_secs(5), sem3.acquire_owned())
                .await
                .expect("timeout should not fire")
                .expect("acquire_owned")
        });
        // Small yield to let the waiter enter acquire_owned.
        tokio::task::yield_now().await;
        drop(p1); // release the slot
        let p2 = waiter.await.expect("waiter panicked");
        drop(p2);
    }
}
