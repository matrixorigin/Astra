use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use astra_services::session_journal::ToolCallRecord;
use astra_thin_client::ThinClient;
use serde_json::{Map, Value};

use super::agentic::headless_round::HeadlessRoundTerminal;
use crate::orchestration::PermissionSyncHandle;
use astra_pipeline::step_protocol::{IdempotencyKey, InMemoryIdempotencyCache};
use astra_pipeline::step_recorder::StepRecorder;
use astra_text_utils::semantic_dedup::SemanticDedup;
use astra_turn_core::edge_prompt_context::make_args_preview;
use astra_turn_core::guardrails::turn_guard::TurnGuard;
use astra_turn_core::headless_tool_assembly::{
    EdgeToolRoundRow, HeadlessResolvedToolSlot, HeadlessRoundToolIdx, READ_ONLY_TOOLS,
    resolve_headless_tool_slot, take_edge_output_for_tool_call_id_or_signature_with_duration,
};

mod execute;
mod policy;
mod record;

/// Compute the set of tool names the validator should admit.
///
/// `visible` is the set advertised in the current request's `tools[]`.
/// `extras` is an explicit execution grant from the caller, e.g. a runtime
/// or plugin transport that is installed out-of-band. Deferred-tool surface
/// should normally be consumed by the next surface assembly so the selected
/// tool becomes visible instead of lingering here as long-lived state.
pub fn admissible_tool_names(
    visible: &HashSet<String>,
    extras: &HashSet<String>,
) -> HashSet<String> {
    let mut out = HashSet::with_capacity(visible.len() + extras.len());
    out.extend(visible.iter().cloned());
    out.extend(extras.iter().cloned());
    out
}

/// Production-facing wrapper: the validator caller typically has the
/// turn's visible tool schemas (slice of JSON values) and needs the final
/// admitted name set. This intentionally admits only visible schemas unless
/// explicit extras are supplied.
pub fn admissible_tool_names_from_visible(
    visible_schemas: &[serde_json::Value],
) -> HashSet<String> {
    admissible_tool_names_from_visible_and_extras(visible_schemas, &[])
}

/// Like [`admissible_tool_names_from_visible`] but also admits names from
/// an `extras` list. Extras are names with an explicit execution grant:
/// runtime-injected schemas or plugin/MCP tools installed into the session.
pub fn admissible_tool_names_from_visible_and_extras(
    visible_schemas: &[serde_json::Value],
    extras: &[String],
) -> HashSet<String> {
    let visible = astra_turn_core::tool::schema::tool_names_from_schemas(visible_schemas);
    let extras: HashSet<String> = extras.iter().cloned().collect();
    admissible_tool_names(&visible, &extras)
}

/// Strict variant for externally-owned capability surfaces.
///
/// Agent Binding mode must not admit static Astra catalog tools unless they
/// are actually present in the loop's visible schemas. This keeps request
/// validation aligned with the binding-discovered tool surface.
pub fn admissible_tool_names_from_visible_and_extras_strict(
    visible_schemas: &[serde_json::Value],
    extras: &[String],
) -> HashSet<String> {
    let mut out = astra_turn_core::tool::schema::tool_names_from_schemas(visible_schemas);
    out.extend(extras.iter().cloned());
    out
}

pub(crate) struct HeadlessResolvedExecution {
    id: String,
    name: String,
    args: Value,
    result_str: String,
    tool_result_fields: Option<Map<String, Value>>,
    edge_duration_ms: u64,
    is_edge_tool: bool,
    early_exit_ms: u64,
}

const EDGE_RESULT_RUNTIME_ENVIRONMENT_ADVERTISEMENT_FIELD: &str =
    "runtime_environment_advertisement";
const EDGE_RESULT_RUNTIME_ENVIRONMENT_FIELD: &str = "runtime_environment";

fn edge_result_runtime_environment_denial(execution: &HeadlessResolvedExecution) -> Option<String> {
    if !execution.is_edge_tool {
        return None;
    }
    if execution
        .tool_result_fields
        .as_ref()
        .is_some_and(|fields| fields.get("blocked").and_then(Value::as_bool) == Some(true))
    {
        return None;
    }
    let Some(fields) = execution.tool_result_fields.as_ref() else {
        return Some(format!(
            "Error: edge runtime capability denied for tool '{}': runtime_environment_advertisement_required",
            execution.name
        ));
    };
    let Some(advertisement_value) = fields
        .get(EDGE_RESULT_RUNTIME_ENVIRONMENT_ADVERTISEMENT_FIELD)
        .or_else(|| fields.get(EDGE_RESULT_RUNTIME_ENVIRONMENT_FIELD))
    else {
        return Some(format!(
            "Error: edge runtime capability denied for tool '{}': runtime_environment_advertisement_required",
            execution.name
        ));
    };
    let advertisement = match serde_json::from_value::<
        astra_runtime_env::RuntimeEnvironmentAdvertisement,
    >(advertisement_value.clone())
    {
        Ok(advertisement) => advertisement,
        Err(_) => {
            return Some(format!(
                "Error: edge runtime capability denied for tool '{}': invalid_runtime_environment_advertisement",
                execution.name
            ));
        }
    };
    let registry = astra_runtime_env::ToolRegistry::builtins();
    if !advertisement.binding.policy.allows_tool(&execution.name) {
        return Some(format!(
            "Error: edge runtime capability denied for tool '{}': {}",
            execution.name,
            astra_runtime_env::PolicyIntent::disallowed_tool_reason(&execution.name)
        ));
    }
    astra_runtime_env::CapabilityResolver
        .check_tool_call(
            &registry,
            &execution.name,
            &execution.args,
            &advertisement.binding.capabilities,
        )
        .err()
        .map(|reason| {
            format!(
                "Error: edge runtime capability denied for tool '{}': {reason}",
                execution.name
            )
        })
}

struct HeadlessBlockedTool<'a> {
    id: &'a str,
    name: &'a str,
    args: &'a Value,
    reason_code: &'a str,
    journal_kind: HeadlessShortCircuitJournalKind,
    err_msg: String,
    journal_reason: String,
    early_exit_ms: u64,
    status_line: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HeadlessShortCircuitJournalKind {
    HardBlocked,
    SuppressedRetry,
}

enum HeadlessToolSlotControl {
    Continue,
    AbortRound,
}

enum HeadlessPipelineStage<T> {
    Continue(T),
    ShortCircuit,
    AbortRound,
}

pub(crate) struct ValidatedExecution {
    pub execution: HeadlessResolvedExecution,
    pub idem_key: IdempotencyKey,
}

pub(crate) struct PermittedExecution {
    pub execution: HeadlessResolvedExecution,
    pub idem_key: IdempotencyKey,
}

pub(crate) struct ExecutedExecution {
    pub execution: HeadlessResolvedExecution,
    pub idem_key: IdempotencyKey,
    pub is_err: bool,
    pub executed_ms: u64,
}

pub(crate) struct HeadlessToolExecutionCtx<'a, E: EdgeToolRoundRow> {
    /// Internal agentic step index (0-based) for cache and dedup accounting.
    pub turn_index: usize,
    /// User-visible session turn currently in progress (1-based).
    pub session_turn: u32,
    pub quiet: bool,
    pub api: &'a ThinClient,
    pub token: &'a str,
    pub current_user_id: Option<&'a str>,
    pub current_session_id: Option<&'a String>,
    pub tool_calls: &'a [Value],
    pub edge_tool_round: &'a [E],
    pub by_sig: &'a HashMap<String, String>,
    pub pre_resolved_ids: &'a HashSet<String>,
    pub messages: &'a mut Vec<Value>,
    pub tool_results: &'a mut Vec<Value>,
    pub valid_tool_names: &'a HashSet<String>,
    /// Names listed in this turn's rendered `<deferred-tools>` manifest.
    /// Used by the validator to differentiate "unknown" denials (truly
    /// hallucinated) from prompt-advertised names whose activation may still
    /// be blocked by runtime binding or fail-closed surface policy. When
    /// empty, every denial falls back to the generic unknown-tool message.
    pub deferred_tool_names: &'a HashSet<String>,
    pub restricted_tools: &'a mut HashSet<String>,
    pub turn_guard: &'a mut TurnGuard,
    pub step_recorder: &'a mut StepRecorder,
    pub idempotency_cache: &'a mut InMemoryIdempotencyCache,
    pub semantic_dedup: &'a mut SemanticDedup,
    pub call_counts: &'a mut HashMap<String, u32>,
    pub max_identical_calls: u32,
    pub max_tools_per_turn: u32,
    /// Consecutive cache-hit suppression cap (was `REPEATED_CACHE_HIT_SUPPRESSION_THRESHOLD`).
    pub repeated_cache_hit_suppression: u32,
    /// Headless-round abort cap for consecutive empty-name calls (was `MAX_CONSECUTIVE_EMPTY_NAME`).
    pub max_consecutive_empty_name: u32,
    pub tool_call_records: &'a mut Vec<ToolCallRecord>,
    pub tool_event_hooks: &'a crate::skills::hooks::ToolEventHookRegistry,
    pub term: &'a mut dyn HeadlessRoundTerminal,
    pub mailbox: Option<&'a mut astra_messaging::router::AgentMailbox>,
    pub permission_context: Option<&'a PermissionSyncHandle>,
    pub progress_emitter: Option<&'a crate::orchestration::AgentProgressEmitter>,
    pub effective_permission_timeout: Duration,
    /// Optional server-side tool executor for web agent sessions (no CLI edge agent).
    /// When present, tools that have no edge match are executed directly by the server.
    pub runtime_tool_executor:
        Option<&'a crate::server::runtime_tool_executor::RuntimeToolExecutor>,
    // ── Observability (Phase 1) ──
    /// Turn start instant for computing start_offset_ms on tool records.
    pub turn_start: Option<std::time::Instant>,
    /// Current LLM round index (0-based) within this turn.
    pub llm_round: u32,
    /// Plan mode active for this turn — set by the runtime when the
    /// session has a non-null `active_plan_id`. When true, the
    /// permission gate denies write/exec tools at evaluation time
    /// with a redirect to `exit_plan_mode`. See
    /// [`crate::turn::permission_gate::check_tool_permission_in_plan_mode`].
    pub plan_mode_active: bool,
}

pub(crate) struct HeadlessToolExecutionPipeline<'a, E: EdgeToolRoundRow> {
    ctx: HeadlessToolExecutionCtx<'a, E>,
    consumed_edge: Vec<bool>,
    consecutive_empty_name: u32,
    executed_this_turn: u32,
}

fn resolve_headless_tool_execution<E: EdgeToolRoundRow>(
    slot: HeadlessResolvedToolSlot,
    edge_tool_round: &[E],
    consumed_edge: &mut [bool],
    by_sig: &HashMap<String, String>,
) -> HeadlessResolvedExecution {
    let HeadlessResolvedToolSlot {
        id,
        name,
        args,
        synthetic_edge_index,
    } = slot;
    let consumed_before = consumed_edge.iter().filter(|&&c| c).count();

    let (result_str, edge_duration_ms, tool_result_fields) = if let Some(i) = synthetic_edge_index {
        (
            edge_tool_round[i].tool_output().to_string(),
            edge_tool_round[i].tool_duration_ms(),
            edge_tool_round[i].tool_result_fields().cloned(),
        )
    } else {
        let matched = take_edge_output_for_tool_call_id_or_signature_with_duration(
            &id,
            &name,
            &args,
            edge_tool_round,
            consumed_edge,
            by_sig,
        );
        (
            matched.output,
            matched.duration_ms,
            matched.tool_result_fields,
        )
    };

    let consumed_after = consumed_edge.iter().filter(|&&c| c).count();
    let is_edge_tool = synthetic_edge_index.is_some() || consumed_after > consumed_before;
    if !is_edge_tool
        && matches!(name.as_str(), "agent" | "agent_fanout")
        && !edge_tool_round.is_empty()
    {
        let edge_candidates = edge_tool_round
            .iter()
            .enumerate()
            .map(|(i, edge)| format!("{}:{}", edge.assistant_tool_call_id(i), edge.tool_name()))
            .collect::<Vec<_>>()
            .join(",");
        tracing::warn!(
            target: "astra_runtime::headless_tool_match",
            tool_name = %name,
            tool_call_id = %id,
            edge_round_len = edge_tool_round.len(),
            edge_candidates = %edge_candidates,
            "executor-gated tool had edge rows but no matching edge result; preserving runtime binding failure"
        );
    }
    let early_exit_ms = if is_edge_tool && edge_duration_ms > 0 {
        edge_duration_ms
    } else {
        0
    };

    HeadlessResolvedExecution {
        id,
        name,
        args,
        result_str,
        tool_result_fields,
        edge_duration_ms,
        is_edge_tool,
        early_exit_ms,
    }
}

impl<'a, E: EdgeToolRoundRow> HeadlessToolExecutionPipeline<'a, E> {
    pub(crate) fn new(ctx: HeadlessToolExecutionCtx<'a, E>, consumed_edge: Vec<bool>) -> Self {
        Self {
            ctx,
            consumed_edge,
            consecutive_empty_name: 0,
            executed_this_turn: 0,
        }
    }

    pub(crate) fn tool_results_len(&self) -> usize {
        self.ctx.tool_results.len()
    }

    pub(crate) fn tool_calls(&self) -> &[Value] {
        self.ctx.tool_calls
    }

    pub(crate) fn edge_tool_name(&self, i: usize) -> String {
        self.ctx.edge_tool_round[i].tool_name().to_string()
    }

    pub(crate) fn scheduling_timeout_ms(&self) -> u64 {
        self.ctx.step_recorder.scheduling().timeout_ms
    }

    pub(crate) fn record_step_abort(&mut self, aborted_tools: &[String]) {
        self.ctx.turn_guard.record_step_abort(aborted_tools);
    }

    fn begin_execution_trace(
        &mut self,
        execution: &HeadlessResolvedExecution,
        idem_key: &IdempotencyKey,
    ) {
        let tool_idem_key = if READ_ONLY_TOOLS.contains(&execution.name.as_str()) {
            Some(idem_key.cache_key())
        } else {
            None
        };
        let args_preview = make_args_preview(&execution.name, &execution.args);
        self.ctx.step_recorder.begin_tool_with_key_and_args_preview(
            &execution.name,
            &execution.id,
            tool_idem_key.as_deref(),
            args_preview.as_deref(),
        );

        if let Some(emitter) = self.ctx.progress_emitter {
            emitter.tool_executing(&execution.name, self.ctx.session_turn);
        }
    }

    fn resolve_slot(&self, item: HeadlessRoundToolIdx) -> HeadlessResolvedToolSlot {
        resolve_headless_tool_slot(item, self.ctx.tool_calls, |i| {
            let edge = &self.ctx.edge_tool_round[i];
            (
                edge.assistant_tool_call_id(i),
                edge.tool_name().to_string(),
                edge.tool_args().clone(),
            )
        })
    }

    pub(crate) async fn run_slot_with_control(&mut self, item: HeadlessRoundToolIdx) -> bool {
        let validated = match self.validate_slot(item) {
            HeadlessPipelineStage::Continue(validated) => validated,
            HeadlessPipelineStage::ShortCircuit => return true,
            HeadlessPipelineStage::AbortRound => return false,
        };

        let permitted = match self.permit_execution(validated).await {
            HeadlessPipelineStage::Continue(permitted) => permitted,
            HeadlessPipelineStage::ShortCircuit => return true,
            HeadlessPipelineStage::AbortRound => return false,
        };

        let executed = self.execute_execution(permitted).await;
        self.record_execution(executed).await;
        true
    }

    /// Execute a batch of read-only tools concurrently.
    /// Returns false if the round should be aborted.
    pub(crate) async fn run_batch_concurrent(&mut self, items: &[HeadlessRoundToolIdx]) -> bool {
        use super::headless_tool_pipeline::execute::{
            execute_tool_pure, execution_result_is_error,
        };

        // Phase 1: validate + permit serially (fast, needs &mut self).
        let mut permitted_batch: Vec<PermittedExecution> = Vec::with_capacity(items.len());
        for &item in items {
            let validated = match self.validate_slot(item) {
                HeadlessPipelineStage::Continue(v) => v,
                HeadlessPipelineStage::ShortCircuit => continue,
                HeadlessPipelineStage::AbortRound => return false,
            };
            match self.permit_execution(validated).await {
                HeadlessPipelineStage::Continue(p) => permitted_batch.push(p),
                HeadlessPipelineStage::ShortCircuit => continue,
                HeadlessPipelineStage::AbortRound => return false,
            };
        }

        if permitted_batch.is_empty() {
            return true;
        }

        // Phase 2: execute all concurrently (no &mut self needed).
        let mut executions: Vec<(HeadlessResolvedExecution, IdempotencyKey)> = permitted_batch
            .into_iter()
            .map(|p| (p.execution, p.idem_key))
            .collect();

        let server_executor = self.ctx.runtime_tool_executor;
        let api = self.ctx.api;
        let token = self.ctx.token;
        let session_id = self.ctx.current_session_id;
        let session_turn = self.ctx.session_turn;
        let edge_round_present = !self.ctx.edge_tool_round.is_empty();

        let started_at: Vec<Instant> = executions
            .iter()
            .map(|(execution, idem_key)| {
                self.begin_execution_trace(execution, idem_key);
                Instant::now()
            })
            .collect();

        let futs: Vec<_> = executions
            .iter_mut()
            .map(|(exec, _)| {
                execute_tool_pure(
                    exec,
                    server_executor,
                    api,
                    token,
                    session_id,
                    session_turn,
                    edge_round_present,
                )
            })
            .collect();
        futures_util::future::join_all(futs).await;

        // Phase 3: post-process + record serially (fast, needs &mut self).
        for ((execution, idem_key), started) in executions.into_iter().zip(started_at) {
            let is_err = execution_result_is_error(
                &execution.name,
                &execution.result_str,
                execution.tool_result_fields.as_ref(),
            );
            let executed_ms = if execution.is_edge_tool && execution.edge_duration_ms > 0 {
                execution.edge_duration_ms
            } else {
                started.elapsed().as_millis() as u64
            };
            let executed = ExecutedExecution {
                execution,
                idem_key,
                is_err,
                executed_ms,
            };
            self.record_execution(executed).await;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use serde_json::json;

    use crate::orchestration::{PermissionMode, PermissionSyncContext, PermissionSyncHandle};
    use crate::skills::hooks::{HookAction, ToolEventHook, ToolEventHookRegistry, ToolEventKind};
    use crate::turn::agentic::headless_round::NoopHeadlessTerminal;
    use astra_pipeline::step_protocol::{CachedToolResult, ContextSignature};
    use astra_turn_core::sse_stream_host::EdgeToolExecResult;

    fn init_git_repo(dir: &Path) {
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(dir)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(dir)
            .output()
            .unwrap();
    }

    async fn short_circuit_cached_read_file(
        harness: &mut PipelineHarness,
        turn_index: usize,
        call_id: &str,
        path: &str,
    ) {
        harness.valid_tool_names.insert("read_file".to_string());
        harness.tool_calls.clear();
        harness.tool_calls.push(json!({
            "id": call_id,
            "function": { "name": "read_file", "arguments": serde_json::to_string(&json!({ "path": path })).unwrap() }
        }));
        let idem_key = read_cache_key_at_epoch(path, 0);
        harness.idempotency_cache.record(
            &idem_key,
            CachedToolResult {
                tool_name: "read_file".to_string(),
                output: format!("cached {path}"),
                is_error: false,
                cached_at: 0,
                context_signature: idem_key.context_signature.clone(),
            },
        );

        harness.call_counts.clear();
        let mut pipeline = harness.pipeline_with_server_executor(turn_index, None);
        match pipeline.validate_slot(HeadlessRoundToolIdx::ServerToolCall(0)) {
            HeadlessPipelineStage::ShortCircuit => {}
            _ => panic!("expected cached read_file to short-circuit"),
        }
    }

    fn begin_recorded_turn(harness: &mut PipelineHarness, tool_count: usize) {
        harness.step_recorder.begin_turn(0);
        harness.step_recorder.begin_act(tool_count);
    }

    fn tool_trace_events(
        harness: &PipelineHarness,
    ) -> Vec<(astra_pipeline::step_protocol::StepEventType, Option<Value>)> {
        harness
            .step_recorder
            .events()
            .iter()
            .filter(|event| {
                matches!(
                    event.event_type,
                    astra_pipeline::step_protocol::StepEventType::ToolCallStarted
                        | astra_pipeline::step_protocol::StepEventType::ToolCallSkipped
                        | astra_pipeline::step_protocol::StepEventType::ToolCallCompleted
                        | astra_pipeline::step_protocol::StepEventType::ToolCallFailed
                )
            })
            .map(|event| (event.event_type.clone(), event.payload.clone()))
            .collect()
    }

    fn read_cache_key_at_epoch(path: &str, workspace_epoch: u64) -> IdempotencyKey {
        let args = json!({ "path": path });
        IdempotencyKey::semantic("read_file", &args).with_context(ContextSignature {
            workspace_version: Some(format!("workspace_epoch:{workspace_epoch}")),
            memory_snapshot_id: None,
        })
    }

    fn edge_runtime_environment_fields() -> Map<String, Value> {
        let registry = astra_runtime_env::ToolRegistry::builtins();
        let advertisement = astra_runtime_env::RuntimeEnvironmentAdvertisement::new(
            astra_runtime_env::RunBinding::edge_developer("/workspace/project", &registry),
        );
        Map::from_iter([(
            EDGE_RESULT_RUNTIME_ENVIRONMENT_ADVERTISEMENT_FIELD.to_string(),
            serde_json::to_value(advertisement).expect("serialize advertisement"),
        )])
    }

    fn server_executor_for_test_workspace(
        workspace: &std::path::Path,
        session_id: &str,
    ) -> crate::server::runtime_tool_executor::RuntimeToolExecutor {
        let mut executor = crate::server::runtime_tool_executor::RuntimeToolExecutor::new(
            workspace.to_path_buf(),
            "test-user".into(),
            session_id.to_string(),
            None,
            None,
        );
        executor.set_execution_bindings(
            crate::server::tool_execution_binding::WorkspaceBinding::server_sandbox(workspace),
            crate::server::tool_execution_binding::ExecutorBinding::server_local(),
        );
        executor
    }

    struct PipelineHarness {
        api: ThinClient,
        tool_calls: Vec<Value>,
        edge_tool_round: Vec<EdgeToolExecResult>,
        by_sig: HashMap<String, String>,
        pre_resolved_ids: HashSet<String>,
        messages: Vec<Value>,
        tool_results: Vec<Value>,
        valid_tool_names: HashSet<String>,
        deferred_tool_names: HashSet<String>,
        restricted_tools: HashSet<String>,
        turn_guard: TurnGuard,
        step_recorder: StepRecorder,
        idempotency_cache: InMemoryIdempotencyCache,
        semantic_dedup: SemanticDedup,
        call_counts: HashMap<String, u32>,
        tool_call_records: Vec<ToolCallRecord>,
        tool_event_hooks: ToolEventHookRegistry,
        permission_context: Option<PermissionSyncHandle>,
        term: NoopHeadlessTerminal,
        repeated_cache_hit_suppression: u32,
        max_consecutive_empty_name: u32,
    }

    impl PipelineHarness {
        fn new() -> Self {
            Self {
                api: ThinClient::new("http://127.0.0.1:1", None).unwrap(),
                tool_calls: Vec::new(),
                edge_tool_round: vec![EdgeToolExecResult {
                    request_id: String::new(),
                    tool: "grep".to_string(),
                    args: json!({ "pattern": "headless" }),
                    output: "found result".to_string(),
                    tool_result_fields: Some(edge_runtime_environment_fields()),
                    status: "completed".to_string(),
                    duration_ms: 12,
                }],
                by_sig: HashMap::new(),
                pre_resolved_ids: HashSet::new(),
                messages: Vec::new(),
                tool_results: Vec::new(),
                valid_tool_names: HashSet::from(["grep".to_string()]),
                deferred_tool_names: HashSet::new(),
                restricted_tools: HashSet::new(),
                turn_guard: TurnGuard::new(),
                step_recorder: StepRecorder::new("test-user", "test-session", "test-task"),
                idempotency_cache: InMemoryIdempotencyCache::new(),
                semantic_dedup: SemanticDedup::new(0.95),
                call_counts: HashMap::new(),
                tool_call_records: Vec::new(),
                tool_event_hooks: ToolEventHookRegistry::default(),
                permission_context: Some(PermissionSyncContext::shared_root(PermissionMode::Auto)),
                term: NoopHeadlessTerminal,
                // Tests assume the legacy threshold of 2 unless they override.
                // Production runs derive these from the per-model policy.
                repeated_cache_hit_suppression: 2,
                max_consecutive_empty_name: 3,
            }
        }

        fn pipeline(&mut self) -> HeadlessToolExecutionPipeline<'_, EdgeToolExecResult> {
            self.pipeline_with_server_executor(0, None)
        }

        fn pipeline_with_server_executor<'a>(
            &'a mut self,
            turn_index: usize,
            runtime_tool_executor: Option<
                &'a crate::server::runtime_tool_executor::RuntimeToolExecutor,
            >,
        ) -> HeadlessToolExecutionPipeline<'a, EdgeToolExecResult> {
            let session_turn = turn_index.saturating_add(1).min(u32::MAX as usize) as u32;
            self.pipeline_with_server_executor_for_session_turn(
                turn_index,
                session_turn.max(1),
                runtime_tool_executor,
            )
        }

        fn pipeline_with_server_executor_for_session_turn<'a>(
            &'a mut self,
            turn_index: usize,
            session_turn: u32,
            runtime_tool_executor: Option<
                &'a crate::server::runtime_tool_executor::RuntimeToolExecutor,
            >,
        ) -> HeadlessToolExecutionPipeline<'a, EdgeToolExecResult> {
            HeadlessToolExecutionPipeline::new(
                HeadlessToolExecutionCtx {
                    turn_index,
                    session_turn,
                    quiet: true,
                    api: &self.api,
                    token: "",
                    current_user_id: None,
                    current_session_id: None,
                    tool_calls: &self.tool_calls,
                    edge_tool_round: &self.edge_tool_round,
                    by_sig: &self.by_sig,
                    pre_resolved_ids: &self.pre_resolved_ids,
                    messages: &mut self.messages,
                    tool_results: &mut self.tool_results,
                    valid_tool_names: &self.valid_tool_names,
                    deferred_tool_names: &self.deferred_tool_names,
                    restricted_tools: &mut self.restricted_tools,
                    turn_guard: &mut self.turn_guard,
                    step_recorder: &mut self.step_recorder,
                    idempotency_cache: &mut self.idempotency_cache,
                    semantic_dedup: &mut self.semantic_dedup,
                    call_counts: &mut self.call_counts,
                    max_identical_calls: 2,
                    max_tools_per_turn: 15,
                    repeated_cache_hit_suppression: self.repeated_cache_hit_suppression,
                    max_consecutive_empty_name: self.max_consecutive_empty_name,
                    tool_call_records: &mut self.tool_call_records,
                    tool_event_hooks: &self.tool_event_hooks,
                    term: &mut self.term,
                    mailbox: None,
                    permission_context: self.permission_context.as_ref(),
                    progress_emitter: None,
                    effective_permission_timeout: Duration::from_secs(30),
                    runtime_tool_executor,
                    turn_start: None,
                    llm_round: 0,
                    plan_mode_active: false,
                },
                vec![false; self.edge_tool_round.len()],
            )
        }
    }

    #[tokio::test]
    async fn validate_slot_returns_validated_execution_for_synthetic_edge() {
        let mut harness = PipelineHarness::new();
        let mut pipeline = harness.pipeline();

        match pipeline.validate_slot(HeadlessRoundToolIdx::SyntheticEdge(0)) {
            HeadlessPipelineStage::Continue(validated) => {
                assert_eq!(validated.execution.id, "edge-0");
                assert_eq!(validated.execution.name, "grep");
                assert_eq!(validated.execution.args, json!({ "pattern": "headless" }));
                assert!(validated.execution.is_edge_tool);
                assert_eq!(validated.execution.edge_duration_ms, 12);
            }
            _ => panic!("expected validated execution"),
        }
    }

    #[tokio::test]
    async fn deferred_control_plane_tool_reports_activation_not_runtime_missing() {
        let mut harness = PipelineHarness::new();
        harness.tool_calls = vec![json!({
            "id": "call-session",
            "type": "function",
            "function": {
                "name": "session",
                "arguments": "{}",
            }
        })];
        harness.edge_tool_round.clear();
        harness.valid_tool_names = HashSet::from(["tool_search".to_string()]);
        harness.deferred_tool_names = HashSet::from(["session".to_string()]);
        begin_recorded_turn(&mut harness, 1);

        {
            let mut pipeline = harness.pipeline();
            match pipeline.validate_slot(HeadlessRoundToolIdx::ServerToolCall(0)) {
                HeadlessPipelineStage::ShortCircuit => {}
                _ => panic!("direct deferred session call must be rejected before execution"),
            }
        }

        let result = harness
            .tool_results
            .last()
            .expect("rejection must append a tool result")
            .to_string();
        assert!(
            result.contains("not available in this turn yet"),
            "deferred control-plane tool should produce an activation/admission hint, got: {result}"
        );
        assert!(
            !result.contains("required runtime capability is not connected"),
            "control-plane deferred tools are not executor-gated runtime tools: {result}"
        );
    }

    #[tokio::test]
    async fn deferred_agent_without_executor_still_reports_runtime_missing() {
        let mut harness = PipelineHarness::new();
        harness.tool_calls = vec![json!({
            "id": "call-agent",
            "type": "function",
            "function": {
                "name": "agent",
                "arguments": r#"{"action":"spawn","prompt":"review"}"#,
            }
        })];
        harness.edge_tool_round.clear();
        harness.valid_tool_names = HashSet::from(["tool_search".to_string()]);
        harness.deferred_tool_names = HashSet::from(["agent".to_string()]);
        begin_recorded_turn(&mut harness, 1);

        {
            let mut pipeline = harness.pipeline();
            match pipeline.validate_slot(HeadlessRoundToolIdx::ServerToolCall(0)) {
                HeadlessPipelineStage::ShortCircuit => {}
                _ => panic!("direct deferred agent call must be rejected without executor"),
            }
        }

        let result = harness
            .tool_results
            .last()
            .expect("rejection must append a tool result")
            .to_string();
        assert!(
            result.contains("multi-agent runtime is not connected"),
            "executor-gated agent must still fail closed, got: {result}"
        );
    }

    #[tokio::test]
    async fn permit_execution_returns_permitted_execution_for_allowed_tool() {
        let mut harness = PipelineHarness::new();
        let mut pipeline = harness.pipeline();
        let validated = match pipeline.validate_slot(HeadlessRoundToolIdx::SyntheticEdge(0)) {
            HeadlessPipelineStage::Continue(validated) => validated,
            _ => panic!("expected validated execution"),
        };

        match pipeline.permit_execution(validated).await {
            HeadlessPipelineStage::Continue(permitted) => {
                assert_eq!(permitted.execution.name, "grep");
                assert_eq!(
                    permitted.idem_key.cache_key(),
                    IdempotencyKey::semantic("grep", &json!({ "pattern": "headless" }))
                        .with_context(ContextSignature {
                            workspace_version: Some("workspace_epoch:0".into()),
                            memory_snapshot_id: None,
                        })
                        .cache_key()
                );
            }
            _ => panic!("expected permitted execution"),
        }
    }

    #[tokio::test]
    async fn permit_server_execution_without_permission_context_denies() {
        let mut harness = PipelineHarness::new();
        harness.permission_context = None;
        harness.tool_calls = vec![json!({
            "id": "call-grep",
            "type": "function",
            "function": {
                "name": "grep",
                "arguments": r#"{"pattern":"headless"}"#
            }
        })];
        harness.edge_tool_round.clear();
        let mut pipeline = harness.pipeline();
        let validated = match pipeline.validate_slot(HeadlessRoundToolIdx::ServerToolCall(0)) {
            HeadlessPipelineStage::Continue(validated) => validated,
            _ => panic!("expected validated execution"),
        };

        match pipeline.permit_execution(validated).await {
            HeadlessPipelineStage::ShortCircuit => {}
            _ => panic!("missing permission context must fail closed"),
        }

        let error = harness
            .tool_call_records
            .last()
            .and_then(|record| record.error.as_deref())
            .unwrap_or("");
        assert!(
            error.contains("no permission context configured"),
            "expected actionable missing-context denial, got {error:?}"
        );
    }

    #[tokio::test]
    async fn permit_edge_execution_without_local_permission_context_uses_edge_result() {
        let mut harness = PipelineHarness::new();
        harness.permission_context = None;
        let mut pipeline = harness.pipeline();
        let validated = match pipeline.validate_slot(HeadlessRoundToolIdx::SyntheticEdge(0)) {
            HeadlessPipelineStage::Continue(validated) => validated,
            _ => panic!("expected validated edge execution"),
        };

        match pipeline.permit_execution(validated).await {
            HeadlessPipelineStage::Continue(permitted) => {
                assert_eq!(permitted.execution.name, "grep");
                assert!(permitted.execution.is_edge_tool);
            }
            _ => panic!("edge results with runtime advertisement must not be denied locally"),
        }
    }

    #[tokio::test]
    async fn concurrent_batch_records_tool_starts_before_terminal_events() {
        let mut harness = PipelineHarness::new();
        harness.edge_tool_round.push(EdgeToolExecResult {
            request_id: String::new(),
            tool: "grep".to_string(),
            args: json!({ "pattern": "pipeline" }),
            output: "second result".to_string(),
            tool_result_fields: Some(edge_runtime_environment_fields()),
            status: "completed".to_string(),
            duration_ms: 7,
        });
        begin_recorded_turn(&mut harness, 2);

        {
            let mut pipeline = harness.pipeline();
            assert!(
                pipeline
                    .run_batch_concurrent(&[
                        HeadlessRoundToolIdx::SyntheticEdge(0),
                        HeadlessRoundToolIdx::SyntheticEdge(1),
                    ])
                    .await,
                "concurrent read-only batch should complete"
            );
        }

        let tool_events = tool_trace_events(&harness);
        assert_eq!(
            tool_events.len(),
            4,
            "each concurrently executed tool should emit started+terminal trace events"
        );
        assert!(matches!(
            tool_events[0].0,
            astra_pipeline::step_protocol::StepEventType::ToolCallStarted
        ));
        assert!(matches!(
            tool_events[1].0,
            astra_pipeline::step_protocol::StepEventType::ToolCallStarted
        ));
        assert!(matches!(
            tool_events[2].0,
            astra_pipeline::step_protocol::StepEventType::ToolCallCompleted
        ));
        assert!(matches!(
            tool_events[3].0,
            astra_pipeline::step_protocol::StepEventType::ToolCallCompleted
        ));
        assert_eq!(
            tool_events[2]
                .1
                .as_ref()
                .and_then(|payload| payload.get("elapsed_ms"))
                .and_then(Value::as_u64),
            Some(12)
        );
        assert_eq!(
            tool_events[3]
                .1
                .as_ref()
                .and_then(|payload| payload.get("elapsed_ms"))
                .and_then(Value::as_u64),
            Some(7)
        );
    }

    #[tokio::test]
    async fn permit_execution_blocks_edge_result_missing_runtime_environment_advertisement() {
        let mut harness = PipelineHarness::new();
        harness.edge_tool_round[0].tool_result_fields = None;
        begin_recorded_turn(&mut harness, 1);

        let mut pipeline = harness.pipeline();
        let validated = match pipeline.validate_slot(HeadlessRoundToolIdx::SyntheticEdge(0)) {
            HeadlessPipelineStage::Continue(validated) => validated,
            _ => panic!("expected validated execution"),
        };

        match pipeline.permit_execution(validated).await {
            HeadlessPipelineStage::ShortCircuit => {}
            _ => panic!("expected missing edge runtime advertisement denial"),
        }
        assert_eq!(pipeline.ctx.tool_results.len(), 1);
        assert!(
            pipeline.ctx.tool_results[0]
                .to_string()
                .contains("runtime_environment_advertisement_required"),
            "got: {}",
            pipeline.ctx.tool_results[0]
        );
    }

    #[tokio::test]
    async fn permit_execution_blocks_edge_result_with_invalid_runtime_environment_advertisement() {
        let mut harness = PipelineHarness::new();
        harness.edge_tool_round[0].tool_result_fields = Some(Map::from_iter([(
            EDGE_RESULT_RUNTIME_ENVIRONMENT_ADVERTISEMENT_FIELD.to_string(),
            json!({"schema_version": 1, "binding": {"invalid": true}}),
        )]));
        begin_recorded_turn(&mut harness, 1);

        let mut pipeline = harness.pipeline();
        let validated = match pipeline.validate_slot(HeadlessRoundToolIdx::SyntheticEdge(0)) {
            HeadlessPipelineStage::Continue(validated) => validated,
            _ => panic!("expected validated execution"),
        };

        match pipeline.permit_execution(validated).await {
            HeadlessPipelineStage::ShortCircuit => {}
            _ => panic!("expected edge runtime advertisement denial"),
        }
        assert_eq!(pipeline.ctx.tool_results.len(), 1);
        assert!(
            pipeline.ctx.tool_results[0]
                .to_string()
                .contains("invalid_runtime_environment_advertisement"),
            "got: {}",
            pipeline.ctx.tool_results[0]
        );
    }

    #[tokio::test]
    async fn permit_execution_blocks_edge_result_when_advertised_runtime_lacks_tool_capability() {
        let mut harness = PipelineHarness::new();
        let registry = astra_runtime_env::ToolRegistry::builtins();
        let advertisement = astra_runtime_env::RuntimeEnvironmentAdvertisement::new(
            astra_runtime_env::RunBinding::cloud_control_plane(&registry),
        );
        harness.edge_tool_round[0].tool_result_fields = Some(Map::from_iter([(
            EDGE_RESULT_RUNTIME_ENVIRONMENT_ADVERTISEMENT_FIELD.to_string(),
            serde_json::to_value(advertisement).expect("serialize advertisement"),
        )]));
        begin_recorded_turn(&mut harness, 1);

        let mut pipeline = harness.pipeline();
        let validated = match pipeline.validate_slot(HeadlessRoundToolIdx::SyntheticEdge(0)) {
            HeadlessPipelineStage::Continue(validated) => validated,
            _ => panic!("expected validated execution"),
        };

        match pipeline.permit_execution(validated).await {
            HeadlessPipelineStage::ShortCircuit => {}
            _ => panic!("expected edge runtime capability denial"),
        }
        assert_eq!(pipeline.ctx.tool_results.len(), 1);
        assert!(
            pipeline.ctx.tool_results[0]
                .to_string()
                .contains("edge runtime capability denied"),
            "got: {}",
            pipeline.ctx.tool_results[0]
        );
    }

    #[tokio::test]
    async fn permit_execution_blocks_edge_result_when_advertised_policy_disallows_tool() {
        let mut harness = PipelineHarness::new();
        let registry = astra_runtime_env::ToolRegistry::builtins();
        let advertisement = astra_runtime_env::RuntimeEnvironmentAdvertisement::new(
            astra_runtime_env::RunBinding::resolve(
                astra_runtime_env::WorkspaceBinding::edge_workspace(
                    "/workspace/project",
                    astra_runtime_env::WorkspaceAuthority::ReadWrite,
                ),
                astra_runtime_env::ExecutorBinding::edge_agent("edge-agent"),
                astra_runtime_env::RuntimeBinding::host_process("edge-host"),
                astra_runtime_env::PolicyIntent::local_developer()
                    .with_allowed_tools(["read_file"]),
                &registry,
            ),
        );
        harness.edge_tool_round[0].tool_result_fields = Some(Map::from_iter([(
            EDGE_RESULT_RUNTIME_ENVIRONMENT_ADVERTISEMENT_FIELD.to_string(),
            serde_json::to_value(advertisement).expect("serialize advertisement"),
        )]));
        begin_recorded_turn(&mut harness, 1);

        let mut pipeline = harness.pipeline();
        let validated = match pipeline.validate_slot(HeadlessRoundToolIdx::SyntheticEdge(0)) {
            HeadlessPipelineStage::Continue(validated) => validated,
            _ => panic!("expected validated execution"),
        };

        match pipeline.permit_execution(validated).await {
            HeadlessPipelineStage::ShortCircuit => {}
            _ => panic!("expected edge runtime policy denial"),
        }
        assert_eq!(pipeline.ctx.tool_results.len(), 1);
        let result = pipeline.ctx.tool_results[0].to_string();
        assert!(
            result.contains("tool 'grep' is not in allowed_tools"),
            "got: {result}"
        );
    }

    #[tokio::test]
    async fn permit_execution_short_circuits_restricted_tool() {
        let mut harness = PipelineHarness::new();
        harness.restricted_tools.insert("grep".to_string());
        begin_recorded_turn(&mut harness, 1);
        let mut pipeline = harness.pipeline();
        let validated = match pipeline.validate_slot(HeadlessRoundToolIdx::SyntheticEdge(0)) {
            HeadlessPipelineStage::Continue(validated) => validated,
            _ => panic!("expected validated execution"),
        };

        match pipeline.permit_execution(validated).await {
            HeadlessPipelineStage::ShortCircuit => {
                assert_eq!(pipeline.tool_results_len(), 1);
            }
            _ => panic!("expected restricted tool short circuit"),
        }
        drop(pipeline);

        let tool_events = tool_trace_events(&harness);
        assert_eq!(
            tool_events.len(),
            2,
            "restricted short-circuit should still be traced"
        );
        assert!(matches!(
            tool_events[0].0,
            astra_pipeline::step_protocol::StepEventType::ToolCallStarted
        ));
        assert!(matches!(
            tool_events[1].0,
            astra_pipeline::step_protocol::StepEventType::ToolCallSkipped
        ));
        assert_eq!(
            tool_events[1]
                .1
                .as_ref()
                .and_then(|payload| payload.get("reason"))
                .and_then(Value::as_str),
            Some("restricted_tool")
        );
    }

    #[tokio::test]
    async fn duplicate_within_turn_short_circuit_records_step_skip_trace() {
        let mut harness = PipelineHarness::new();
        begin_recorded_turn(&mut harness, 3);
        {
            let mut pipeline = harness.pipeline();
            assert!(matches!(
                pipeline.validate_slot(HeadlessRoundToolIdx::SyntheticEdge(0)),
                HeadlessPipelineStage::Continue(_)
            ));
            assert!(matches!(
                pipeline.validate_slot(HeadlessRoundToolIdx::SyntheticEdge(0)),
                HeadlessPipelineStage::Continue(_)
            ));
            assert!(matches!(
                pipeline.validate_slot(HeadlessRoundToolIdx::SyntheticEdge(0)),
                HeadlessPipelineStage::ShortCircuit
            ));
        }

        let tool_events = tool_trace_events(&harness);
        assert_eq!(
            tool_events.len(),
            2,
            "duplicate-within-turn short-circuit should emit started+skipped trace events"
        );
        assert!(matches!(
            tool_events[0].0,
            astra_pipeline::step_protocol::StepEventType::ToolCallStarted
        ));
        assert!(matches!(
            tool_events[1].0,
            astra_pipeline::step_protocol::StepEventType::ToolCallSkipped
        ));
        assert_eq!(
            tool_events[1]
                .1
                .as_ref()
                .and_then(|payload| payload.get("reason"))
                .and_then(Value::as_str),
            Some("duplicate_within_turn")
        );
    }

    #[tokio::test]
    async fn semantic_dedup_short_circuit_records_step_skip_trace() {
        let mut harness = PipelineHarness::new();
        harness.semantic_dedup.check_and_record(
            "grep",
            &json!({ "pattern": "headless" }),
            "previous grep output that should be reused for semantic dedup blocking",
            0,
        );
        begin_recorded_turn(&mut harness, 1);

        {
            let mut pipeline = harness.pipeline_with_server_executor(1, None);
            assert!(matches!(
                pipeline.validate_slot(HeadlessRoundToolIdx::SyntheticEdge(0)),
                HeadlessPipelineStage::ShortCircuit
            ));
        }

        let tool_events = tool_trace_events(&harness);
        assert_eq!(
            tool_events.len(),
            2,
            "semantic dedup short-circuit should emit started+skipped trace events"
        );
        assert!(matches!(
            tool_events[0].0,
            astra_pipeline::step_protocol::StepEventType::ToolCallStarted
        ));
        assert!(matches!(
            tool_events[1].0,
            astra_pipeline::step_protocol::StepEventType::ToolCallSkipped
        ));
        assert_eq!(
            tool_events[1]
                .1
                .as_ref()
                .and_then(|payload| payload.get("reason"))
                .and_then(Value::as_str),
            Some("semantic_dedup_pre_check")
        );
        assert!(
            tool_events[1]
                .1
                .as_ref()
                .and_then(|payload| payload.get("args_preview"))
                .and_then(Value::as_str)
                .is_some_and(|preview| preview.contains("headless")),
            "semantic dedup skip should carry structured args_preview: {:?}",
            tool_events[1].1
        );
        let skipped_output = tool_events[1]
            .1
            .as_ref()
            .and_then(|payload| payload.get("output"))
            .and_then(Value::as_str)
            .expect("semantic dedup skip output");
        assert!(
            skipped_output.contains("previous grep output"),
            "semantic dedup cache hit should replay the useful prior output: {skipped_output}"
        );
        let model_tool_output = harness.messages.last().and_then(|msg| {
            msg.get("content")
                .or_else(|| msg.get("output"))
                .and_then(Value::as_str)
        });
        assert!(
            model_tool_output.is_some_and(|output| output.contains("previous grep output")),
            "model-facing semantic cache hit must include usable evidence: {model_tool_output:?}"
        );
    }

    #[tokio::test]
    async fn turn_budget_short_circuit_records_step_skip_trace() {
        let mut harness = PipelineHarness::new();
        begin_recorded_turn(&mut harness, 1);
        let mut pipeline = harness.pipeline();
        pipeline.executed_this_turn = pipeline.ctx.max_tools_per_turn;

        assert!(matches!(
            pipeline.validate_slot(HeadlessRoundToolIdx::SyntheticEdge(0)),
            HeadlessPipelineStage::ShortCircuit
        ));
        drop(pipeline);

        let tool_events = tool_trace_events(&harness);
        assert_eq!(
            tool_events.len(),
            2,
            "turn-budget short-circuit should emit started+skipped trace events"
        );
        assert!(matches!(
            tool_events[0].0,
            astra_pipeline::step_protocol::StepEventType::ToolCallStarted
        ));
        assert!(matches!(
            tool_events[1].0,
            astra_pipeline::step_protocol::StepEventType::ToolCallSkipped
        ));
        assert_eq!(
            tool_events[1]
                .1
                .as_ref()
                .and_then(|payload| payload.get("reason"))
                .and_then(Value::as_str),
            Some("turn_budget_exhausted")
        );
    }

    #[tokio::test]
    async fn cached_cross_turn_short_circuit_records_explicit_trace_payload() {
        let mut harness = PipelineHarness::new();
        begin_recorded_turn(&mut harness, 1);

        short_circuit_cached_read_file(&mut harness, 1, "call-read-a-1", "a.txt").await;

        let tool_events = tool_trace_events(&harness);
        assert_eq!(
            tool_events.len(),
            2,
            "cached cross-turn short-circuit should emit started+completed trace events"
        );
        assert!(matches!(
            tool_events[0].0,
            astra_pipeline::step_protocol::StepEventType::ToolCallStarted
        ));
        let started_payload = tool_events[0].1.as_ref().expect("started payload");
        assert_eq!(
            started_payload.get("tool_name").and_then(Value::as_str),
            Some("read_file")
        );
        assert_eq!(
            started_payload.get("call_id").and_then(Value::as_str),
            Some("call-read-a-1")
        );
        assert!(
            started_payload
                .get("args_preview")
                .and_then(Value::as_str)
                .is_some_and(|preview| preview.contains("a.txt")),
            "started trace should include args preview, got: {started_payload:?}"
        );

        assert!(matches!(
            tool_events[1].0,
            astra_pipeline::step_protocol::StepEventType::ToolCallCompleted
        ));
        let completed_payload = tool_events[1].1.as_ref().expect("completed payload");
        assert_eq!(
            completed_payload.get("reason").and_then(Value::as_str),
            Some("cached_cross_turn")
        );
        assert_eq!(
            completed_payload.get("cached").and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            completed_payload.get("is_error").and_then(Value::as_bool),
            Some(false)
        );
        assert_eq!(
            completed_payload.get("call_id").and_then(Value::as_str),
            Some("call-read-a-1")
        );
        assert!(
            completed_payload
                .get("args_preview")
                .and_then(Value::as_str)
                .is_some_and(|preview| preview.contains("a.txt")),
            "completed trace should include args preview, got: {completed_payload:?}"
        );
        assert!(
            completed_payload
                .get("output")
                .and_then(Value::as_str)
                .is_some_and(|output| output.contains("cached a.txt")),
            "completed cache-hit trace should carry cached output, got: {completed_payload:?}"
        );
    }

    #[tokio::test]
    async fn successful_mutation_invalidates_cached_read_only_results() {
        let cases = [
            (
                "write_file",
                json!({ "path": "a.txt", "content": "new content" }),
                false,
                true,
            ),
            (
                "str_replace",
                json!({ "path": "a.txt", "old_str": "old", "new_str": "new" }),
                false,
                true,
            ),
            (
                "git",
                json!({ "action": "commit", "message": "save changes" }),
                false,
                true,
            ),
            (
                "bash",
                json!({ "command": "printf new > a.txt" }),
                false,
                true,
            ),
            (
                "write_file",
                json!({ "path": "a.txt", "content": "new content" }),
                true,
                false,
            ),
        ];

        for (tool_name, args, is_err, should_evict) in cases {
            let mut harness = PipelineHarness::new();
            begin_recorded_turn(&mut harness, 1);
            let read_key = read_cache_key_at_epoch("a.txt", 0);
            harness.idempotency_cache.record(
                &read_key,
                CachedToolResult {
                    tool_name: "read_file".into(),
                    output: "old content".into(),
                    is_error: false,
                    cached_at: 0,
                    context_signature: read_key.context_signature.clone(),
                },
            );

            let mut pipeline = harness.pipeline();
            pipeline
                .record_execution(ExecutedExecution {
                    execution: HeadlessResolvedExecution {
                        id: format!("call-{tool_name}"),
                        name: tool_name.into(),
                        args: args.clone(),
                        result_str: "mutation succeeded".into(),
                        tool_result_fields: None,
                        edge_duration_ms: 1,
                        is_edge_tool: true,
                        early_exit_ms: 0,
                    },
                    idem_key: IdempotencyKey::semantic(tool_name, &args),
                    is_err,
                    executed_ms: 1,
                })
                .await;
            drop(pipeline);

            assert_eq!(
                harness.idempotency_cache.check(&read_key).is_none(),
                should_evict,
                "{tool_name} with is_err={is_err} eviction mismatch"
            );
        }
    }

    #[tokio::test]
    async fn successful_mutation_clears_semantic_dedup_observations() {
        let mut harness = PipelineHarness::new();
        harness.edge_tool_round.clear();
        harness.valid_tool_names.insert("read_file".to_string());
        harness.semantic_dedup.check_and_record_with_generation(
            "read_file",
            &json!({"path": "models.rs", "start_line": 2900, "end_line": 2940}),
            "stale models.rs content that used to block fresh reads",
            0,
            0,
        );
        harness.tool_calls = vec![json!({
            "id": "call-read-stale",
            "type": "function",
            "function": {
                "name": "read_file",
                "arguments": r#"{"path":"models.rs","start_line":2900,"end_line":2940}"#
            }
        })];

        {
            let mut pipeline = harness.pipeline_with_server_executor(1, None);
            assert!(
                matches!(
                    pipeline.validate_slot(HeadlessRoundToolIdx::ServerToolCall(0)),
                    HeadlessPipelineStage::ShortCircuit
                ),
                "same epoch should use the semantic cache"
            );
        }

        {
            let mut pipeline = harness.pipeline();
            let args = json!({"path": "models.rs", "old_str": "before", "new_str": "after"});
            pipeline
                .record_execution(ExecutedExecution {
                    execution: HeadlessResolvedExecution {
                        id: "call-edit".into(),
                        name: "str_replace".into(),
                        args: args.clone(),
                        result_str: "mutation succeeded".into(),
                        tool_result_fields: None,
                        edge_duration_ms: 1,
                        is_edge_tool: true,
                        early_exit_ms: 0,
                    },
                    idem_key: IdempotencyKey::semantic("str_replace", &args),
                    is_err: false,
                    executed_ms: 1,
                })
                .await;
        }

        harness.tool_calls = vec![json!({
            "id": "call-read-fresh",
            "type": "function",
            "function": {
                "name": "read_file",
                "arguments": r#"{"path":"models.rs","start_line":2900,"end_line":2940}"#
            }
        })];
        let mut pipeline = harness.pipeline_with_server_executor(2, None);
        assert!(
            matches!(
                pipeline.validate_slot(HeadlessRoundToolIdx::ServerToolCall(0)),
                HeadlessPipelineStage::Continue(_)
            ),
            "mutation must force a fresh read instead of semantic duplicate blocking"
        );
    }

    #[tokio::test]
    async fn redundant_validation_is_suppressed_until_workspace_mutates() {
        let mut harness = PipelineHarness::new();
        harness.edge_tool_round.clear();
        harness.valid_tool_names.insert("bash".to_string());

        for i in 0..2 {
            harness.tool_calls = vec![json!({
                "id": format!("call-check-{i}"),
                "type": "function",
                "function": {
                    "name": "bash",
                    "arguments": r#"{"command":"cd rust && cargo check 2>&1 | head -50"}"#
                }
            })];
            let mut pipeline = harness.pipeline_with_server_executor(i, None);
            assert!(
                matches!(
                    pipeline.validate_slot(HeadlessRoundToolIdx::ServerToolCall(0)),
                    HeadlessPipelineStage::Continue(_)
                ),
                "first two validation attempts in an epoch are allowed"
            );
        }

        harness.tool_calls = vec![json!({
            "id": "call-check-blocked",
            "type": "function",
            "function": {
                "name": "bash",
                "arguments": r#"{"command":"cd rust && cargo check 2>&1 | tail -50"}"#
            }
        })];
        {
            let mut pipeline = harness.pipeline_with_server_executor(2, None);
            assert!(
                matches!(
                    pipeline.validate_slot(HeadlessRoundToolIdx::ServerToolCall(0)),
                    HeadlessPipelineStage::ShortCircuit
                ),
                "third same-prefix validation in one epoch should be policy-blocked"
            );
        }
        assert!(
            harness.tool_results.last().is_some_and(|result| result
                .to_string()
                .contains("Redundant validation suppressed")),
            "blocked validation should give an explicit model-facing reason"
        );

        {
            let mut pipeline = harness.pipeline();
            let args = json!({"path": "src/lib.rs", "old_str": "a", "new_str": "b"});
            pipeline
                .record_execution(ExecutedExecution {
                    execution: HeadlessResolvedExecution {
                        id: "call-edit".into(),
                        name: "str_replace".into(),
                        args: args.clone(),
                        result_str: "mutation succeeded".into(),
                        tool_result_fields: None,
                        edge_duration_ms: 1,
                        is_edge_tool: true,
                        early_exit_ms: 0,
                    },
                    idem_key: IdempotencyKey::semantic("str_replace", &args),
                    is_err: false,
                    executed_ms: 1,
                })
                .await;
        }

        harness.tool_calls = vec![json!({
            "id": "call-check-after-edit",
            "type": "function",
            "function": {
                "name": "bash",
                "arguments": r#"{"command":"cd rust && cargo check 2>&1 | head -50"}"#
            }
        })];
        let mut pipeline = harness.pipeline_with_server_executor(3, None);
        assert!(
            matches!(
                pipeline.validate_slot(HeadlessRoundToolIdx::ServerToolCall(0)),
                HeadlessPipelineStage::Continue(_)
            ),
            "workspace mutation must reset validation retry policy"
        );
    }

    #[tokio::test]
    async fn execute_and_record_pipeline_appends_one_tool_result() {
        let mut harness = PipelineHarness::new();
        let mut pipeline = harness.pipeline();
        let validated = match pipeline.validate_slot(HeadlessRoundToolIdx::SyntheticEdge(0)) {
            HeadlessPipelineStage::Continue(validated) => validated,
            _ => panic!("expected validated execution"),
        };
        let permitted = match pipeline.permit_execution(validated).await {
            HeadlessPipelineStage::Continue(permitted) => permitted,
            _ => panic!("expected permitted execution"),
        };

        let executed = pipeline.execute_execution(permitted).await;
        assert!(!executed.is_err);
        assert_eq!(executed.executed_ms, 12);

        pipeline.record_execution(executed).await;

        assert_eq!(pipeline.tool_results_len(), 1);
        assert_eq!(pipeline.executed_this_turn, 1);
        assert_eq!(pipeline.ctx.tool_call_records.len(), 1);
    }

    #[tokio::test]
    async fn post_tool_hooks_modify_cached_and_recorded_output() {
        let mut harness = PipelineHarness::new();
        harness.tool_event_hooks = ToolEventHookRegistry::new(vec![ToolEventHook {
            event: ToolEventKind::PostToolUse,
            matcher: "grep".into(),
            action: HookAction::Shell {
                command: r#"echo '{"output":"hooked result"}'"#.into(),
            },
            timeout_secs: 5,
            is_async: false,
            condition: None,
            once: false,
            priority: 0,
        }]);
        let idem_key = IdempotencyKey::semantic("grep", &json!({ "pattern": "headless" }))
            .with_context(ContextSignature {
                workspace_version: Some("workspace_epoch:0".into()),
                memory_snapshot_id: None,
            });
        let mut pipeline = harness.pipeline();
        let validated = match pipeline.validate_slot(HeadlessRoundToolIdx::SyntheticEdge(0)) {
            HeadlessPipelineStage::Continue(validated) => validated,
            _ => panic!("expected validated execution"),
        };
        let permitted = match pipeline.permit_execution(validated).await {
            HeadlessPipelineStage::Continue(permitted) => permitted,
            _ => panic!("expected permitted execution"),
        };

        let executed = pipeline.execute_execution(permitted).await;
        pipeline.record_execution(executed).await;

        let cached = pipeline
            .ctx
            .idempotency_cache
            .check(&idem_key)
            .expect("cache entry should be recorded");
        assert_eq!(cached.output, "hooked result");
        assert!(
            pipeline.ctx.tool_call_records[0]
                .result_preview
                .as_deref()
                .is_some_and(|preview| preview.contains("hooked result"))
        );
        assert!(
            pipeline.ctx.tool_results[0]
                .to_string()
                .contains("hooked result")
        );
    }

    #[tokio::test]
    async fn failed_edge_status_marks_execution_error_even_without_error_text() {
        let mut harness = PipelineHarness::new();
        harness.edge_tool_round[0].output = "permission denied".to_string();
        harness.edge_tool_round[0].status = "partial_failure".to_string();
        let mut fields = edge_runtime_environment_fields();
        fields.insert(
            "status".to_string(),
            Value::String("partial_failure".to_string()),
        );
        fields.insert(
            "output".to_string(),
            Value::String("permission denied".to_string()),
        );
        harness.edge_tool_round[0].tool_result_fields = Some(fields);

        let mut pipeline = harness.pipeline();
        let validated = match pipeline.validate_slot(HeadlessRoundToolIdx::SyntheticEdge(0)) {
            HeadlessPipelineStage::Continue(validated) => validated,
            _ => panic!("expected validated execution"),
        };
        let permitted = match pipeline.permit_execution(validated).await {
            HeadlessPipelineStage::Continue(permitted) => permitted,
            _ => panic!("expected permitted execution"),
        };

        let executed = pipeline.execute_execution(permitted).await;
        assert!(executed.is_err, "got: {}", executed.execution.result_str);
    }

    #[tokio::test]
    async fn explicit_success_output_overrides_stale_failed_edge_status() {
        let mut harness = PipelineHarness::new();
        let args = json!({
            "path": "src/lib.rs",
            "old_str": "before",
            "new_str": "after"
        });
        harness.edge_tool_round[0].tool = "str_replace".to_string();
        harness.edge_tool_round[0].args = args;
        harness.edge_tool_round[0].output = format!(
            "Replaced successfully\n<<<ASTRA_UNIFIED_DIFF>>>\n-old\n+new\n<<<END_ASTRA_UNIFIED_DIFF>>>\n{}",
            astra_turn_core::tool_result_semantics::TOOL_SUCCESS_SENTINEL
        );
        harness.edge_tool_round[0].status = "error".to_string();
        let mut fields = edge_runtime_environment_fields();
        fields.insert("status".to_string(), Value::String("failed".to_string()));
        harness.edge_tool_round[0].tool_result_fields = Some(fields);
        harness.valid_tool_names = HashSet::from(["str_replace".to_string()]);

        let mut pipeline = harness.pipeline();
        let validated = match pipeline.validate_slot(HeadlessRoundToolIdx::SyntheticEdge(0)) {
            HeadlessPipelineStage::Continue(validated) => validated,
            _ => panic!("expected validated execution"),
        };
        let permitted = match pipeline.permit_execution(validated).await {
            HeadlessPipelineStage::Continue(permitted) => permitted,
            _ => panic!("expected permitted execution"),
        };

        let executed = pipeline.execute_execution(permitted).await;
        assert!(!executed.is_err, "got: {}", executed.execution.result_str);
        assert!(
            !executed.execution.result_str.contains("returned an error"),
            "successful edit must not receive error feedback: {}",
            executed.execution.result_str
        );

        pipeline.record_execution(executed).await;
        assert_eq!(pipeline.ctx.tool_call_records.len(), 1);
        let record = &pipeline.ctx.tool_call_records[0];
        assert!(record.ok);
        assert!(record.error.is_none());
        assert!(
            record
                .result_preview
                .as_deref()
                .is_some_and(|preview| preview.starts_with("Replaced successfully"))
        );
    }

    #[tokio::test]
    async fn server_executor_sets_turn_index_for_current_turn_rollback() {
        let mut harness = PipelineHarness::new();
        harness.edge_tool_round.clear();
        let dir = tempfile::TempDir::new().unwrap();
        let server_exec = server_executor_for_test_workspace(dir.path(), "test-session");
        let mut pipeline =
            harness.pipeline_with_server_executor_for_session_turn(3, 7, Some(&server_exec));
        let args = json!({"path": "turn.txt", "content": "hello"});
        let permitted = PermittedExecution {
            execution: HeadlessResolvedExecution {
                id: "call-1".into(),
                name: "write_file".into(),
                args: args.clone(),
                result_str: "Error: headless edge protocol: no matching edge result".into(),
                tool_result_fields: None,
                edge_duration_ms: 0,
                is_edge_tool: false,
                early_exit_ms: 0,
            },
            idem_key: IdempotencyKey::semantic("write_file", &args),
        };

        let executed = pipeline.execute_execution(permitted).await;
        assert!(!executed.is_err);
        assert!(dir.path().join("turn.txt").exists());

        let rollback = server_exec
            .execute("rollback_file_edits", &json!({"scope": "current_turn"}))
            .await;
        let rollback_json: Value = serde_json::from_str(&rollback).unwrap();
        assert_eq!(
            rollback_json["success"].as_bool(),
            Some(true),
            "got: {rollback}"
        );
        assert_eq!(rollback_json["turn_index"].as_u64(), Some(7));
        assert_eq!(rollback_json["reverted"].as_array().map(Vec::len), Some(1));
        assert!(!dir.path().join("turn.txt").exists());
    }

    #[tokio::test]
    async fn server_executor_preserves_tool_result_fields() {
        let mut harness = PipelineHarness::new();
        harness.edge_tool_round.clear();
        let dir = tempfile::TempDir::new().unwrap();
        init_git_repo(dir.path());
        std::fs::write(dir.path().join("tracked.txt"), "hello\n").unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(dir.path())
            .output()
            .unwrap();
        let server_exec = server_executor_for_test_workspace(dir.path(), "test-session");
        let mut pipeline = harness.pipeline_with_server_executor(3, Some(&server_exec));
        let args = json!({"action": "commit", "message": "initial"});
        let permitted = PermittedExecution {
            execution: HeadlessResolvedExecution {
                id: "call-git".into(),
                name: "git".into(),
                args: args.clone(),
                result_str: "Error: headless edge protocol: no matching edge result".into(),
                tool_result_fields: None,
                edge_duration_ms: 0,
                is_edge_tool: false,
                early_exit_ms: 0,
            },
            idem_key: IdempotencyKey::semantic("git", &args),
        };

        let executed = pipeline.execute_execution(permitted).await;
        assert!(!executed.is_err, "got: {}", executed.execution.result_str);
        let result_fields = executed
            .execution
            .tool_result_fields
            .as_ref()
            .expect("alternate execution provider metadata");
        assert!(result_fields["commit_sha"].as_str().is_some());
        pipeline.record_execution(executed).await;
        assert!(
            pipeline.ctx.tool_results[0]
                .to_string()
                .contains("\"commit_sha\""),
            "got: {}",
            pipeline.ctx.tool_results[0]
        );
    }

    #[tokio::test]
    async fn server_executor_surfaces_read_file_large_file_preview() {
        let mut harness = PipelineHarness::new();
        harness.edge_tool_round.clear();
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("large.txt"),
            "0123456789abcdef\n".repeat(6_000),
        )
        .unwrap();
        let server_exec = server_executor_for_test_workspace(dir.path(), "test-session");
        let mut pipeline = harness.pipeline_with_server_executor(2, Some(&server_exec));
        let args = json!({"path": "large.txt"});
        let permitted = PermittedExecution {
            execution: HeadlessResolvedExecution {
                id: "call-read-large".into(),
                name: "read_file".into(),
                args: args.clone(),
                result_str: "Error: headless edge protocol: no matching edge result".into(),
                tool_result_fields: None,
                edge_duration_ms: 0,
                is_edge_tool: false,
                early_exit_ms: 0,
            },
            idem_key: IdempotencyKey::semantic("read_file", &args),
        };

        let executed = pipeline.execute_execution(permitted).await;
        assert!(!executed.is_err, "got: {}", executed.execution.result_str);
        assert!(
            executed.execution.result_str.contains("Large file preview"),
            "got: {}",
            executed.execution.result_str
        );
        assert!(
            executed.execution.result_str.contains("offset"),
            "got: {}",
            executed.execution.result_str
        );
    }

    #[tokio::test]
    async fn server_executor_surfaces_bash_timeout_partial_output() {
        let mut harness = PipelineHarness::new();
        harness.edge_tool_round.clear();
        let dir = tempfile::TempDir::new().unwrap();
        let server_exec = server_executor_for_test_workspace(dir.path(), "test-session");
        let mut pipeline = harness.pipeline_with_server_executor(4, Some(&server_exec));
        let args = json!({
            "command": "printf 'start\\n'; sleep 1; printf 'done\\n'",
            "timeout": 0.2
        });
        let permitted = PermittedExecution {
            execution: HeadlessResolvedExecution {
                id: "call-bash-timeout".into(),
                name: "bash".into(),
                args: args.clone(),
                result_str: "Error: headless edge protocol: no matching edge result".into(),
                tool_result_fields: None,
                edge_duration_ms: 0,
                is_edge_tool: false,
                early_exit_ms: 0,
            },
            idem_key: IdempotencyKey::semantic("bash", &args),
        };

        let executed = pipeline.execute_execution(permitted).await;
        assert!(executed.is_err, "got: {}", executed.execution.result_str);
        assert!(
            executed.execution.result_str.contains("start"),
            "got: {}",
            executed.execution.result_str
        );
        assert!(
            executed
                .execution
                .result_str
                .contains("timed out after 0.2s"),
            "got: {}",
            executed.execution.result_str
        );
        assert!(
            !executed.execution.result_str.contains("done"),
            "got: {}",
            executed.execution.result_str
        );
    }

    #[tokio::test]
    async fn edge_selected_runtime_tool_without_edge_result_does_not_fallback_to_server_executor() {
        let mut harness = PipelineHarness::new();
        let dir = tempfile::TempDir::new().unwrap();
        let server_exec = server_executor_for_test_workspace(dir.path(), "test-session");
        let mut pipeline = harness.pipeline_with_server_executor(3, Some(&server_exec));
        let args = json!({"path": "must_not_write.txt", "content": "wrong provider"});
        let permitted = PermittedExecution {
            execution: HeadlessResolvedExecution {
                id: "call-write".into(),
                name: "write_file".into(),
                args: args.clone(),
                result_str: "Error: headless edge protocol: no matching edge result".into(),
                tool_result_fields: None,
                edge_duration_ms: 0,
                is_edge_tool: false,
                early_exit_ms: 0,
            },
            idem_key: IdempotencyKey::semantic("write_file", &args),
        };

        let executed = pipeline.execute_execution(permitted).await;

        assert!(executed.is_err);
        assert!(
            executed
                .execution
                .result_str
                .contains("headless edge protocol"),
            "no matching selected edge result must remain a binding error, got: {}",
            executed.execution.result_str
        );
        assert!(
            !dir.path().join("must_not_write.txt").exists(),
            "server executor must not run a runtime tool when the selected edge provider failed to return a result"
        );
        assert_eq!(
            executed
                .execution
                .tool_result_fields
                .as_ref()
                .and_then(|fields| fields.get("error_kind"))
                .and_then(Value::as_str),
            Some(astra_core::ErrorKind::ToolBinding.as_str())
        );
    }

    // ── Unknown tool validation tests ────────────────────────────────

    /// Helper: push a server tool_call JSON for an unknown tool and run validate_slot.
    fn push_unknown_server_tool_call(harness: &mut PipelineHarness, tool_name: &str) {
        let idx = harness.tool_calls.len();
        harness.tool_calls.push(json!({
            "id": format!("call-{tool_name}-{idx}"),
            "function": {
                "name": tool_name,
                "arguments": "{}"
            }
        }));
    }

    #[tokio::test]
    async fn unknown_tool_records_journal_without_health_failure() {
        let mut harness = PipelineHarness::new();
        push_unknown_server_tool_call(&mut harness, "outline");
        begin_recorded_turn(&mut harness, 1);
        let mut pipeline = harness.pipeline();

        let result = pipeline.validate_slot(HeadlessRoundToolIdx::ServerToolCall(0));
        assert!(
            matches!(result, HeadlessPipelineStage::ShortCircuit),
            "unknown tool should short-circuit"
        );
        drop(pipeline);

        let tool_events = tool_trace_events(&harness);
        assert_eq!(
            tool_events.len(),
            2,
            "unknown tool should emit started+skipped trace events"
        );
        assert!(matches!(
            tool_events[0].0,
            astra_pipeline::step_protocol::StepEventType::ToolCallStarted
        ));
        assert!(matches!(
            tool_events[1].0,
            astra_pipeline::step_protocol::StepEventType::ToolCallSkipped
        ));
        assert_eq!(
            tool_events[1]
                .1
                .as_ref()
                .and_then(|payload| payload.get("reason"))
                .and_then(Value::as_str),
            Some("unknown_tool")
        );

        // Catalog misses are not runtime failures. Recording them in health
        // would make removed/hallucinated tools resurface as "failed often"
        // context in later turns.
        let health = harness.turn_guard.health.get("outline");
        assert!(
            health.is_none(),
            "unknown catalog tool should not pollute ToolHealth"
        );
    }

    /// Direct-call recovery contract: when the model calls a deferred tool
    /// that the current runtime can activate, the validator records the
    /// activation intent instead of emitting the bare "Unknown tool" message.
    #[tokio::test]
    async fn validator_direct_deferred_call_records_activation_hint() {
        let mut harness = PipelineHarness::new();
        push_unknown_server_tool_call(&mut harness, "memory");
        begin_recorded_turn(&mut harness, 1);

        let visible = vec![json!({"type": "function", "function": {"name": "grep"}})];
        harness.valid_tool_names = super::admissible_tool_names_from_visible(&visible);
        harness.deferred_tool_names = HashSet::from(["memory".to_string()]);
        let dir = tempfile::TempDir::new().unwrap();
        let server_exec = server_executor_for_test_workspace(dir.path(), "test-session");
        server_exec.set_current_activatable_tool_names(HashSet::from(["memory".to_string()]));

        let mut pipeline = harness.pipeline_with_server_executor(1, Some(&server_exec));
        let result = pipeline.validate_slot(HeadlessRoundToolIdx::ServerToolCall(0));
        assert!(matches!(result, HeadlessPipelineStage::ShortCircuit));
        drop(pipeline);

        let last_tr = harness
            .tool_results
            .last()
            .expect("denial should record a tool_result");
        let body = last_tr
            .get("result")
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert!(
            body.contains("requires `tool_search` activation first")
                && body.contains("select:memory")
                && body.contains("not executed"),
            "direct deferred call must become a non-executing activation hint; got: {body}"
        );
        assert!(
            !body.starts_with("Unknown tool"),
            "direct deferred call must not reuse the bare unknown-tool copy; got: {body}"
        );
        assert_eq!(
            server_exec.activated_deferred_tool_names(),
            vec!["memory".to_string()],
            "validator path must record activation for the next model request"
        );
        let record = harness
            .tool_call_records
            .last()
            .expect("direct deferred activation should record a journal placeholder");
        assert_eq!(record.name, "memory");
        assert!(record.ok);
        assert_eq!(record.error.as_deref(), Some("tool_not_admitted"));
        assert!(record.is_synthetic_placeholder());
        assert!(
            record
                .result_preview
                .as_deref()
                .is_some_and(|preview| preview.starts_with("Deferred:"))
        );
        let tool_events = tool_trace_events(&harness);
        assert_eq!(
            tool_events[1]
                .1
                .as_ref()
                .and_then(|payload| payload.get("reason"))
                .and_then(Value::as_str),
            Some("direct_deferred_call_activated")
        );

        // Hallucinated names still get the Unknown-tool body.
        let mut h2 = PipelineHarness::new();
        push_unknown_server_tool_call(&mut h2, "definitely_not_a_tool");
        begin_recorded_turn(&mut h2, 1);
        h2.valid_tool_names = super::admissible_tool_names_from_visible(&visible);
        let mut p2 = h2.pipeline();
        let _ = p2.validate_slot(HeadlessRoundToolIdx::ServerToolCall(0));
        drop(p2);
        let halluc_body = h2
            .tool_results
            .last()
            .and_then(|tr| tr.get("result"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        assert!(
            halluc_body.starts_with("Unknown tool"),
            "hallucinated names must still get the bare unknown-tool copy; got: {halluc_body}"
        );
    }

    #[tokio::test]
    async fn validator_ignores_stale_activatable_name_without_prompt_manifest() {
        let mut harness = PipelineHarness::new();
        push_unknown_server_tool_call(&mut harness, "github");
        begin_recorded_turn(&mut harness, 1);

        let visible = vec![json!({"type": "function", "function": {"name": "tool_search"}})];
        harness.valid_tool_names = super::admissible_tool_names_from_visible(&visible);
        harness.deferred_tool_names = HashSet::new();
        let dir = tempfile::TempDir::new().unwrap();
        let server_exec = server_executor_for_test_workspace(dir.path(), "test-session");
        server_exec.set_current_activatable_tool_names(HashSet::from(["github".to_string()]));

        let mut pipeline = harness.pipeline_with_server_executor(1, Some(&server_exec));
        let result = pipeline.validate_slot(HeadlessRoundToolIdx::ServerToolCall(0));
        assert!(matches!(result, HeadlessPipelineStage::ShortCircuit));
        drop(pipeline);

        let body = harness
            .tool_results
            .last()
            .and_then(|tr| tr.get("result"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert!(
            body.starts_with("Unknown tool"),
            "a name not shown in this turn's deferred manifest must not be treated as activatable: {body}"
        );
        assert!(
            !body.contains("select:github"),
            "validator must not invent activation guidance without a prompt manifest: {body}"
        );
        assert!(
            server_exec.activated_deferred_tool_names().is_empty(),
            "stale activatable state must not activate a tool that was not prompt-advertised"
        );
    }

    #[tokio::test]
    async fn validator_prompt_deferred_without_runtime_binding_reports_runtime_not_search() {
        let mut harness = PipelineHarness::new();
        push_unknown_server_tool_call(&mut harness, "agent_fanout");
        begin_recorded_turn(&mut harness, 1);

        let visible = vec![json!({"type": "function", "function": {"name": "grep"}})];
        harness.valid_tool_names = super::admissible_tool_names_from_visible(&visible);
        harness.deferred_tool_names = HashSet::from(["agent_fanout".to_string()]);

        let mut pipeline = harness.pipeline();
        let result = pipeline.validate_slot(HeadlessRoundToolIdx::ServerToolCall(0));
        assert!(matches!(result, HeadlessPipelineStage::ShortCircuit));
        drop(pipeline);

        let body = harness
            .tool_results
            .last()
            .and_then(|tr| tr.get("result"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert!(
            body.contains("multi-agent runtime is not connected"),
            "prompt-deferred but unbound tool must report the missing runtime: {body}"
        );
        assert!(
            body.contains("tool_search") && !body.contains("select:agent_fanout"),
            "runtime-binding denial must not claim select can make the tool executable: {body}"
        );
        assert!(
            !body.starts_with("Unknown tool"),
            "the name was prompt-advertised, so it is unavailable, not hallucinated: {body}"
        );
    }

    #[tokio::test]
    async fn validator_prompt_deferred_but_not_activatable_avoids_select_retry_loop() {
        let mut harness = PipelineHarness::new();
        push_unknown_server_tool_call(&mut harness, "github");
        begin_recorded_turn(&mut harness, 1);

        let visible = vec![json!({"type": "function", "function": {"name": "grep"}})];
        harness.valid_tool_names = super::admissible_tool_names_from_visible(&visible);
        harness.deferred_tool_names = HashSet::from(["github".to_string()]);
        let dir = tempfile::TempDir::new().unwrap();
        let server_exec = server_executor_for_test_workspace(dir.path(), "test-session");
        server_exec.set_current_activatable_tool_names(HashSet::new());

        let mut pipeline = harness.pipeline_with_server_executor(1, Some(&server_exec));
        let result = pipeline.validate_slot(HeadlessRoundToolIdx::ServerToolCall(0));
        assert!(matches!(result, HeadlessPipelineStage::ShortCircuit));
        drop(pipeline);

        let body = harness
            .tool_results
            .last()
            .and_then(|tr| tr.get("result"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert!(
            body.contains("not activatable") && body.contains("Do not retry"),
            "prompt-deferred but fail-closed activation must avoid a search retry loop: {body}"
        );
        assert!(
            !body.starts_with("Unknown tool"),
            "the name was prompt-advertised, so it is unavailable, not hallucinated: {body}"
        );
    }

    #[tokio::test]
    async fn validator_denial_empty_deferred_set_stays_unknown_with_tool_search_visible() {
        let mut harness = PipelineHarness::new();
        push_unknown_server_tool_call(&mut harness, "agent_fanout");
        begin_recorded_turn(&mut harness, 1);

        let visible = vec![json!({"type": "function", "function": {"name": "tool_search"}})];
        harness.valid_tool_names = super::admissible_tool_names_from_visible(&visible);
        harness.deferred_tool_names = HashSet::new();

        let mut pipeline = harness.pipeline();
        let result = pipeline.validate_slot(HeadlessRoundToolIdx::ServerToolCall(0));
        assert!(matches!(result, HeadlessPipelineStage::ShortCircuit));
        drop(pipeline);

        let body = harness
            .tool_results
            .last()
            .and_then(|tr| tr.get("result"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert!(
            body.starts_with("Unknown tool"),
            "empty deferred set means no deferred manifest was advertised; got: {body}"
        );
        assert!(
            !body.contains("select:agent_fanout"),
            "validator must not invent deferred activation guidance without a manifest; got: {body}"
        );
    }

    /// Symmetric: a truly hallucinated name must still short-circuit.
    #[tokio::test]
    async fn validator_rejects_hallucinated_tool_even_with_admissible_helper() {
        let mut harness = PipelineHarness::new();
        push_unknown_server_tool_call(&mut harness, "definitely_made_up");
        begin_recorded_turn(&mut harness, 1);

        let visible = vec![json!({"type": "function", "function": {"name": "grep"}})];
        harness.valid_tool_names = super::admissible_tool_names_from_visible(&visible);
        assert!(
            !harness.valid_tool_names.contains("definitely_made_up"),
            "precondition: hallucinated name must NOT be admissible"
        );

        let mut pipeline = harness.pipeline();
        let result = pipeline.validate_slot(HeadlessRoundToolIdx::ServerToolCall(0));
        assert!(
            matches!(result, HeadlessPipelineStage::ShortCircuit),
            "hallucinated tool must short-circuit; deferred-admission helper must not be a hole"
        );
    }

    /// Extras path: plugin/runtime-injected names reach admissible via
    /// `admissible_tool_names_from_visible_and_extras` and must be
    /// admitted by the real validator.
    #[tokio::test]
    async fn validator_admits_plugin_name_via_extras() {
        let mut harness = PipelineHarness::new();
        push_unknown_server_tool_call(&mut harness, "mcp__weather");
        begin_recorded_turn(&mut harness, 1);

        let visible = vec![json!({"type": "function", "function": {"name": "grep"}})];
        let extras = vec!["mcp__weather".to_string()];
        harness.valid_tool_names =
            super::admissible_tool_names_from_visible_and_extras(&visible, &extras);
        assert!(harness.valid_tool_names.contains("mcp__weather"));

        let mut pipeline = harness.pipeline();
        let result = pipeline.validate_slot(HeadlessRoundToolIdx::ServerToolCall(0));
        assert!(
            !matches!(result, HeadlessPipelineStage::ShortCircuit),
            "plugin-registered tool must be admitted via extras"
        );
    }

    #[tokio::test]
    async fn unknown_tool_retries_do_not_advise_avoidance_missing_catalog_entry() {
        let mut harness = PipelineHarness::new();
        // Push 3 calls with different args so dedup doesn't block them.
        for i in 0..3 {
            harness.tool_calls.push(json!({
                "id": format!("call-outline-{i}"),
                "function": {
                    "name": "outline",
                    "arguments": format!("{{\"path\": \"file{i}.rs\"}}")
                }
            }));
        }
        let mut pipeline = harness.pipeline();

        for i in 0..3 {
            let result = pipeline.validate_slot(HeadlessRoundToolIdx::ServerToolCall(i));
            assert!(matches!(result, HeadlessPipelineStage::ShortCircuit));
        }

        assert!(
            pipeline.ctx.turn_guard.health.get("outline").is_none(),
            "unknown catalog tool should not be tracked in ToolHealth"
        );
        assert!(
            !pipeline
                .ctx
                .turn_guard
                .health
                .health_avoidance_tools()
                .contains(&"outline"),
            "unknown catalog tool should not be avoidance_advised"
        );
    }

    #[tokio::test]
    async fn unknown_tool_journal_records_error_tag() {
        let mut harness = PipelineHarness::new();
        push_unknown_server_tool_call(&mut harness, "nonexistent");
        let mut pipeline = harness.pipeline();

        pipeline.validate_slot(HeadlessRoundToolIdx::ServerToolCall(0));

        assert_eq!(pipeline.ctx.tool_call_records.len(), 1);
        let record = &pipeline.ctx.tool_call_records[0];
        assert_eq!(record.name, "nonexistent");
        assert!(!record.ok);
        assert!(
            record
                .error
                .as_deref()
                .is_some_and(|e| e.contains("unknown_tool")),
        );
    }

    #[tokio::test]
    async fn unknown_tool_error_message_sent_to_llm() {
        let mut harness = PipelineHarness::new();
        push_unknown_server_tool_call(&mut harness, "outline");
        let mut pipeline = harness.pipeline();

        pipeline.validate_slot(HeadlessRoundToolIdx::ServerToolCall(0));

        // The tool result sent back to the LLM should mention "Unknown tool".
        assert_eq!(pipeline.ctx.tool_results.len(), 1);
        let result_str = pipeline.ctx.tool_results[0].to_string();
        assert!(
            result_str.contains("Unknown tool"),
            "LLM should see 'Unknown tool' in result, got: {result_str}"
        );
    }

    #[tokio::test]
    async fn empty_name_tool_does_not_pollute_health() {
        let mut harness = PipelineHarness::new();
        // Push a tool call with empty name.
        harness.tool_calls.push(json!({
            "id": "call-empty-0",
            "function": {
                "name": "",
                "arguments": "{}"
            }
        }));
        begin_recorded_turn(&mut harness, 1);
        let mut pipeline = harness.pipeline();

        let result = pipeline.validate_slot(HeadlessRoundToolIdx::ServerToolCall(0));
        assert!(matches!(result, HeadlessPipelineStage::ShortCircuit));
        drop(pipeline);

        let tool_events = tool_trace_events(&harness);
        assert_eq!(
            tool_events.len(),
            2,
            "empty-name short-circuit should emit started+skipped trace events"
        );
        assert!(matches!(
            tool_events[0].0,
            astra_pipeline::step_protocol::StepEventType::ToolCallStarted
        ));
        assert!(matches!(
            tool_events[1].0,
            astra_pipeline::step_protocol::StepEventType::ToolCallSkipped
        ));
        assert_eq!(
            tool_events[1]
                .1
                .as_ref()
                .and_then(|payload| payload.get("reason"))
                .and_then(Value::as_str),
            Some("unknown_tool")
        );

        // Empty-name calls use a separate consecutive-name guard; they should
        // not create a ToolHealth entry under the empty string.
        let health = harness.turn_guard.health.get("");
        assert!(
            health.is_none(),
            "empty-name catalog miss should not pollute ToolHealth"
        );
    }

    #[tokio::test]
    async fn unknown_tool_with_identical_args_blocked_by_dedup_after_limit() {
        let mut harness = PipelineHarness::new();
        // Push 3 calls with IDENTICAL args — dedup should block call #3.
        for i in 0..3 {
            harness.tool_calls.push(json!({
                "id": format!("call-outline-{i}"),
                "function": {
                    "name": "outline",
                    "arguments": "{}"
                }
            }));
        }
        let mut pipeline = harness.pipeline();

        // Call 1: unknown tool error
        let r1 = pipeline.validate_slot(HeadlessRoundToolIdx::ServerToolCall(0));
        assert!(matches!(r1, HeadlessPipelineStage::ShortCircuit));

        // Call 2: unknown tool error (count=2, at limit)
        let r2 = pipeline.validate_slot(HeadlessRoundToolIdx::ServerToolCall(1));
        assert!(matches!(r2, HeadlessPipelineStage::ShortCircuit));

        // Call 3: should be blocked by dedup (count=3 > limit=2)
        let r3 = pipeline.validate_slot(HeadlessRoundToolIdx::ServerToolCall(2));
        assert!(matches!(r3, HeadlessPipelineStage::ShortCircuit));

        // First 2 calls should have unknown_tool journal records,
        // 3rd should be a duplicate record.
        assert_eq!(pipeline.ctx.tool_call_records.len(), 3);
        assert!(
            pipeline.ctx.tool_call_records[0]
                .error
                .as_deref()
                .is_some_and(|e| e.contains("unknown_tool"))
        );
        assert!(
            pipeline.ctx.tool_call_records[1]
                .error
                .as_deref()
                .is_some_and(|e| e.contains("unknown_tool"))
        );
        // 3rd record is a dedup, not unknown_tool
        assert!(
            !pipeline.ctx.tool_call_records[2]
                .error
                .as_deref()
                .is_some_and(|e| e.contains("unknown_tool")),
            "3rd call should be caught by dedup, not unknown_tool path"
        );

        let health = pipeline.ctx.turn_guard.health.get("outline");
        if let Some(health) = health {
            assert_eq!(
                health.total_failures, 0,
                "deduped unknown catalog tools may record neutral cache stats, not failures"
            );
            assert!(
                !health.avoidance_advised,
                "deduped unknown catalog tools should not be avoidance_advised"
            );
        }
    }

    #[tokio::test]
    async fn multiple_different_unknown_tools_do_not_pollute_health() {
        let mut harness = PipelineHarness::new();
        harness.tool_calls.push(json!({
            "id": "call-outline-0",
            "function": { "name": "outline", "arguments": "{}" }
        }));
        harness.tool_calls.push(json!({
            "id": "call-foobar-0",
            "function": { "name": "foobar", "arguments": "{}" }
        }));
        harness.tool_calls.push(json!({
            "id": "call-outline-1",
            "function": { "name": "outline", "arguments": "{\"path\": \"a.rs\"}" }
        }));
        let mut pipeline = harness.pipeline();

        for i in 0..3 {
            pipeline.validate_slot(HeadlessRoundToolIdx::ServerToolCall(i));
        }

        assert!(
            pipeline.ctx.turn_guard.health.get("outline").is_none(),
            "outline is not in the catalog and should not be tracked"
        );
        assert!(
            pipeline.ctx.turn_guard.health.get("foobar").is_none(),
            "foobar is not in the catalog and should not be tracked"
        );
    }

    #[tokio::test]
    async fn unknown_tool_avoidance_warning_not_generated() {
        let mut harness = PipelineHarness::new();
        // 3 calls with different args to avoid dedup. They should remain
        // short-circuited catalog misses, not health failures.
        for i in 0..3 {
            harness.tool_calls.push(json!({
                "id": format!("call-outline-{i}"),
                "function": {
                    "name": "outline",
                    "arguments": format!("{{\"path\": \"file{i}.rs\"}}")
                }
            }));
        }
        let mut pipeline = harness.pipeline();

        for i in 0..3 {
            pipeline.validate_slot(HeadlessRoundToolIdx::ServerToolCall(i));
        }

        let warning = pipeline.ctx.turn_guard.health.health_avoidance_warning();
        assert!(
            warning.is_none(),
            "unknown catalog tool should not generate a advise_avoidance warning"
        );
    }

    #[tokio::test]
    async fn empty_name_abort_round_after_max_consecutive() {
        let mut harness = PipelineHarness::new();
        // MAX_CONSECUTIVE_EMPTY_NAME = 3; push 3 empty-name calls.
        for i in 0..3 {
            harness.tool_calls.push(json!({
                "id": format!("call-empty-{i}"),
                "function": { "name": "", "arguments": "{}" }
            }));
        }
        let mut pipeline = harness.pipeline();

        // First 2 should ShortCircuit (continue processing).
        let r1 = pipeline.validate_slot(HeadlessRoundToolIdx::ServerToolCall(0));
        assert!(matches!(r1, HeadlessPipelineStage::ShortCircuit));
        let r2 = pipeline.validate_slot(HeadlessRoundToolIdx::ServerToolCall(1));
        assert!(matches!(r2, HeadlessPipelineStage::ShortCircuit));

        // 3rd should AbortRound.
        let r3 = pipeline.validate_slot(HeadlessRoundToolIdx::ServerToolCall(2));
        assert!(
            matches!(r3, HeadlessPipelineStage::AbortRound),
            "3 consecutive empty-name calls should abort the round"
        );

        assert!(
            pipeline.ctx.turn_guard.health.get("").is_none(),
            "empty-name catalog misses use the consecutive-name guard only"
        );
    }

    #[tokio::test]
    async fn server_executor_unknown_tool_records_health_failure() {
        // Simulates the DefaultToolExecutor "not available" path:
        // tool passes valid_tool_names but executor returns error.
        let mut harness = PipelineHarness::new();
        let missing_tool = "definitely_missing_server_tool";
        // Add the missing tool to valid_tool_names so it passes validation.
        harness.valid_tool_names.insert(missing_tool.to_string());
        harness.tool_calls.push(json!({
            "id": "call-missing-0",
            "function": { "name": missing_tool, "arguments": "{}" }
        }));
        let dir = tempfile::TempDir::new().unwrap();
        let server_exec = server_executor_for_test_workspace(dir.path(), "test-session");
        let mut pipeline = harness.pipeline_with_server_executor(0, Some(&server_exec));

        // validate_slot should pass (outline is in valid_tool_names).
        let validated = match pipeline.validate_slot(HeadlessRoundToolIdx::ServerToolCall(0)) {
            HeadlessPipelineStage::Continue(v) => v,
            _ => panic!("expected Continue"),
        };

        // permit_execution should pass (no restrictions).
        let permitted = match pipeline.permit_execution(validated).await {
            HeadlessPipelineStage::Continue(p) => p,
            _ => panic!("expected Continue"),
        };

        // execute_execution: RuntimeToolExecutor doesn't know the tool,
        // returns an explicit not-available error.
        let executed = pipeline.execute_execution(permitted).await;
        assert!(
            executed.is_err,
            "server executor should return error for unknown tool"
        );

        // record_execution feeds through append_headless_result_quality_feedback
        // → turn_guard.record_tool_result → health.record_failure
        pipeline.record_execution(executed).await;

        let health = pipeline.ctx.turn_guard.health.get(missing_tool);
        assert!(
            health.is_some(),
            "missing tool should be tracked after alternate execution provider error"
        );
        let h = health.unwrap();
        assert_eq!(
            h.total_failures, 1,
            "alternate execution provider error should count as failure"
        );
        assert_eq!(h.consecutive_failures, 1);
    }

    #[tokio::test]
    async fn no_matching_edge_execution_is_failed_tool_binding_without_rollback_class() {
        let mut harness = PipelineHarness::new();
        harness.valid_tool_names.insert("github".to_string());
        harness.tool_calls.push(json!({
            "id": "call-github-0",
            "function": {
                "name": "github",
                "arguments": serde_json::to_string(&json!({
                    "action": "search",
                    "query": "astra"
                })).unwrap()
            }
        }));
        let mut pipeline = harness.pipeline();

        let validated = match pipeline.validate_slot(HeadlessRoundToolIdx::ServerToolCall(0)) {
            HeadlessPipelineStage::Continue(v) => v,
            _ => panic!("expected validation to pass"),
        };
        let permitted = match pipeline.permit_execution(validated).await {
            HeadlessPipelineStage::Continue(p) => p,
            _ => panic!("expected permission to pass"),
        };
        let executed = pipeline.execute_execution(permitted).await;

        assert!(
            executed.is_err,
            "executor-missing must be a failed tool call"
        );
        let fields = executed
            .execution
            .tool_result_fields
            .as_ref()
            .expect("headless protocol failure must carry structured metadata");
        assert_eq!(fields.get("status").and_then(Value::as_str), Some("failed"));
        assert_eq!(
            fields.get("error_kind").and_then(Value::as_str),
            Some(astra_core::ErrorKind::ToolBinding.as_str())
        );
        assert!(
            !astra_turn_core::tool_result_semantics::tool_error_triggers_rollback(
                "github",
                &executed.execution.result_str,
            ),
            "no executor means no tool implementation ran, so rollback is wrong"
        );

        pipeline.record_execution(executed).await;
        let record = pipeline
            .ctx
            .tool_call_records
            .last()
            .expect("recorded tool call");
        assert!(!record.ok, "journal must not mark executor-missing as ok");
        assert!(
            record
                .error
                .as_deref()
                .is_some_and(|error| error.contains("headless edge protocol")),
            "journal error should preserve the executor-missing body, got {record:?}"
        );
    }

    #[test]
    fn validate_slot_adopts_agent_fanout_edge_result_by_request_id_when_args_differ() {
        let mut harness = PipelineHarness::new();
        let server_args = json!({
            "action": "start",
            "target_count": 3,
            "slots": [{
                "id": "review",
                "description": "Review",
                "prompt": "Review this change."
            }]
        });
        let edge_args = json!({
            "action": "start",
            "target_count": 3,
            "slots": [{
                "id": "review",
                "description": "Review",
                "prompt": "Review this change."
            }],
            "title": "Review"
        });
        harness.valid_tool_names = HashSet::from(["agent_fanout".to_string()]);
        harness.tool_calls.push(json!({
            "id": "call-agent-fanout-1",
            "function": {
                "name": "agent_fanout",
                "arguments": serde_json::to_string(&server_args).unwrap()
            }
        }));
        harness.edge_tool_round = vec![EdgeToolExecResult {
            request_id: "call-agent-fanout-1".to_string(),
            tool: "agent_fanout".to_string(),
            args: edge_args,
            output: r#"{"completed":3,"group_id":"run-test-fanout-1"}"#.to_string(),
            tool_result_fields: Some(edge_runtime_environment_fields()),
            status: "completed".to_string(),
            duration_ms: 209_858,
        }];

        let mut pipeline = harness.pipeline();
        let validated = match pipeline.validate_slot(HeadlessRoundToolIdx::ServerToolCall(0)) {
            HeadlessPipelineStage::Continue(validated) => validated,
            HeadlessPipelineStage::ShortCircuit => {
                panic!("expected fanout edge result to validate, got short-circuit")
            }
            HeadlessPipelineStage::AbortRound => {
                panic!("expected fanout edge result to validate, got abort")
            }
        };

        assert_eq!(validated.execution.name, "agent_fanout");
        assert!(validated.execution.is_edge_tool);
        assert_eq!(validated.execution.edge_duration_ms, 209_858);
        assert!(validated.execution.result_str.contains(r#""completed":3"#));
        assert!(
            pipeline.ctx.tool_results.is_empty(),
            "matched edge result must not emit runtime-binding denial"
        );
    }

    #[tokio::test]
    async fn stale_executor_gated_tool_without_binding_short_circuits() {
        let cases = [
            (
                "agent",
                json!({
                    "action": "spawn",
                    "prompt": "Review this change.",
                    "description": "Review"
                }),
            ),
            (
                "agent_fanout",
                json!({
                    "action": "start",
                    "target_count": 1,
                    "slots": [{
                        "id": "review",
                        "description": "Review",
                        "prompt": "Review this change."
                    }]
                }),
            ),
        ];

        for (tool_name, args) in cases {
            let mut harness = PipelineHarness::new();
            // Simulate stale resume or cached tool-surface state that
            // incorrectly carried an executor-gated tool into the validator
            // allow-set.
            harness.valid_tool_names.insert(tool_name.to_string());
            harness.tool_calls.push(json!({
                "id": format!("call-{tool_name}-0"),
                "function": {
                    "name": tool_name,
                    "arguments": serde_json::to_string(&args).unwrap()
                }
            }));
            begin_recorded_turn(&mut harness, 1);
            let mut pipeline = harness.pipeline();

            assert!(matches!(
                pipeline.validate_slot(HeadlessRoundToolIdx::ServerToolCall(0)),
                HeadlessPipelineStage::ShortCircuit
            ));
            let body = pipeline
                .ctx
                .tool_results
                .last()
                .and_then(|tr| tr.get("result"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            assert!(
                body.contains("multi-agent runtime is not connected"),
                "executor-gated stale call should name the missing runtime for {tool_name}: {body}"
            );
            assert!(
                !body.contains("headless edge protocol"),
                "stale executor-gated calls must be denied before no-matching-edge binding failure for {tool_name}: {body}"
            );
            let record = pipeline
                .ctx
                .tool_call_records
                .last()
                .expect("blocked runtime call should be journaled");
            assert!(!record.ok);
            assert!(
                record
                    .error
                    .as_deref()
                    .is_some_and(|error| error.contains("multi-agent runtime is not connected")),
                "journal should preserve runtime-binding denial for {tool_name}, got {record:?}"
            );
        }
    }

    #[tokio::test]
    async fn semantic_dedup_does_not_block_git_action_diff_path_after_stat_only() {
        let mut harness = PipelineHarness::new();
        harness.valid_tool_names.insert("git".to_string());

        let dir = tempfile::TempDir::new().unwrap();
        init_git_repo(dir.path());
        std::fs::write(dir.path().join("tracked.txt"), "before\n").unwrap();
        std::process::Command::new("git")
            .args(["add", "tracked.txt"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::fs::write(dir.path().join("tracked.txt"), "before\nafter\n").unwrap();

        let server_exec = server_executor_for_test_workspace(dir.path(), "test-session");

        harness.tool_calls.push(json!({
            "id": "call-git-diff-stat",
            "function": { "name": "git", "arguments": "{\"action\":\"diff\",\"stat_only\":true}" }
        }));
        {
            let mut pipeline = harness.pipeline_with_server_executor(0, Some(&server_exec));
            let validated = match pipeline.validate_slot(HeadlessRoundToolIdx::ServerToolCall(0)) {
                HeadlessPipelineStage::Continue(v) => v,
                _ => panic!("expected stat_only git diff to validate"),
            };
            let permitted = match pipeline.permit_execution(validated).await {
                HeadlessPipelineStage::Continue(p) => p,
                _ => panic!("expected stat_only git diff to execute"),
            };
            let executed = pipeline.execute_execution(permitted).await;
            assert!(!executed.is_err, "got: {}", executed.execution.result_str);
            pipeline.record_execution(executed).await;
        }

        harness.tool_calls.clear();
        harness.tool_calls.push(json!({
            "id": "call-git-diff-path",
            "function": { "name": "git", "arguments": "{\"action\":\"diff\",\"path\":\"tracked.txt\"}" }
        }));
        let mut pipeline = harness.pipeline_with_server_executor(1, Some(&server_exec));
        let validated = match pipeline.validate_slot(HeadlessRoundToolIdx::ServerToolCall(0)) {
            HeadlessPipelineStage::Continue(v) => v,
            _ => panic!("expected path-scoped git diff to validate"),
        };
        let permitted = match pipeline.permit_execution(validated).await {
            HeadlessPipelineStage::Continue(p) => p,
            HeadlessPipelineStage::ShortCircuit => {
                panic!("path-scoped git diff must not be semantically blocked by earlier stat_only")
            }
            HeadlessPipelineStage::AbortRound => panic!("unexpected abort"),
        };
        let executed = pipeline.execute_execution(permitted).await;
        assert!(!executed.is_err, "got: {}", executed.execution.result_str);
        assert!(
            executed.execution.result_str.contains("@@"),
            "path-scoped git diff should execute and return patch hunks, got: {}",
            executed.execution.result_str
        );
    }

    #[tokio::test]
    async fn cache_hits_on_different_read_file_signatures_do_not_trigger_guard() {
        let mut harness = PipelineHarness::new();

        short_circuit_cached_read_file(&mut harness, 0, "call-read-a-1", "a.txt").await;
        short_circuit_cached_read_file(&mut harness, 1, "call-read-b-1", "b.txt").await;
        short_circuit_cached_read_file(&mut harness, 2, "call-read-c-1", "c.txt").await;

        let verdict = harness.turn_guard.evaluate();
        assert!(
            !verdict.avoid_tools.contains(&"read_file".to_string()),
            "distinct read_file signatures should not exhaust the whole tool"
        );
        assert!(
            verdict
                .injections
                .iter()
                .all(|msg| !msg.contains("Duplicate calls detected")),
            "distinct cached signatures should not look like duplicate waste"
        );
    }

    #[tokio::test]
    async fn execute_populates_outcome_cache_under_canonical_signature() {
        let mut harness = PipelineHarness::new();
        let missing_tool = "definitely_missing_server_tool";
        harness.valid_tool_names.insert(missing_tool.to_string());
        harness.tool_calls.push(json!({
            "id": "call-missing-0",
            "function": { "name": missing_tool, "arguments": "{}" }
        }));
        let dir = tempfile::TempDir::new().unwrap();
        let server_exec = server_executor_for_test_workspace(dir.path(), "test-session");
        let mut pipeline = harness.pipeline_with_server_executor(0, Some(&server_exec));

        let validated = match pipeline.validate_slot(HeadlessRoundToolIdx::ServerToolCall(0)) {
            HeadlessPipelineStage::Continue(v) => v,
            _ => panic!("expected Continue"),
        };
        let permitted = match pipeline.permit_execution(validated).await {
            HeadlessPipelineStage::Continue(p) => p,
            _ => panic!("expected Continue"),
        };
        let executed = pipeline.execute_execution(permitted).await;

        let sig = astra_turn_core::tool_result_semantics::tool_dedup_signature(
            &executed.execution.name,
            &executed.execution.args,
        );
        let outcome = pipeline
            .ctx
            .turn_guard
            .health
            .recent_outcome(&sig)
            .expect("outcome cache should have an entry for the executed signature");
        assert!(
            !outcome.success,
            "unknown-tool error path should record a failure outcome"
        );
        assert_eq!(
            pipeline
                .ctx
                .turn_guard
                .health
                .outcome_history(&sig)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn validate_slot_blocks_recent_identical_failures_from_outcome_memory() {
        let mut harness = PipelineHarness::new();
        harness.tool_calls.push(json!({
            "id": "call-grep-0",
            "function": { "name": "grep", "arguments": "{\"pattern\":\"headless\"}" }
        }));
        let sig = astra_turn_core::tool_result_semantics::tool_dedup_signature(
            "grep",
            &json!({"pattern":"headless"}),
        );
        let now_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        harness.turn_guard.health.record_outcome(
            &sig,
            astra_turn_core::tool_health::ToolOutcome {
                success: false,
                latency_ms: 10,
                result_hash: 1,
                at_epoch: now_epoch,
                failure_category: None,
            },
        );
        harness.turn_guard.health.record_outcome(
            &sig,
            astra_turn_core::tool_health::ToolOutcome {
                success: false,
                latency_ms: 12,
                result_hash: 2,
                at_epoch: now_epoch,
                failure_category: None,
            },
        );

        begin_recorded_turn(&mut harness, 1);
        let mut pipeline = harness.pipeline();
        let result = pipeline.validate_slot(HeadlessRoundToolIdx::ServerToolCall(0));
        assert!(matches!(result, HeadlessPipelineStage::ShortCircuit));
        assert_eq!(pipeline.ctx.tool_results.len(), 1);
        assert!(
            pipeline.ctx.tool_results[0]
                .to_string()
                .contains("identical_retry_suppressed"),
            "expected identical retry suppression advisory in tool result"
        );
        drop(pipeline);

        let tool_events = tool_trace_events(&harness);
        assert_eq!(
            tool_events.len(),
            2,
            "outcome-memory short-circuit should emit started+skipped trace events"
        );
        assert!(matches!(
            tool_events[0].0,
            astra_pipeline::step_protocol::StepEventType::ToolCallStarted
        ));
        assert!(matches!(
            tool_events[1].0,
            astra_pipeline::step_protocol::StepEventType::ToolCallSkipped
        ));
        assert_eq!(
            tool_events[1]
                .1
                .as_ref()
                .and_then(|payload| payload.get("reason"))
                .and_then(Value::as_str),
            Some("identical_failure_suppressed")
        );
        assert_eq!(harness.tool_call_records.len(), 1);
        assert!(harness.tool_call_records[0].ok);
        assert!(harness.tool_call_records[0].is_synthetic_placeholder());
        assert!(!harness.tool_call_records[0].was_blocked_by_policy());
    }

    #[test]
    fn validate_slot_blocks_repeated_str_replace_with_recovery_policy() {
        let mut harness = PipelineHarness::new();
        harness.valid_tool_names.insert("str_replace".to_string());
        let args = json!({
            "path": "src/lib.rs",
            "old_str": "fn missing() {}",
            "new_str": "fn present() {}"
        });
        harness.tool_calls.push(json!({
            "id": "call-edit-0",
            "function": { "name": "str_replace", "arguments": serde_json::to_string(&args).unwrap() }
        }));
        let sig =
            astra_turn_core::tool_result_semantics::tool_dedup_signature("str_replace", &args);
        let now_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        for result_hash in [11, 12] {
            harness.turn_guard.health.record_outcome(
                &sig,
                astra_turn_core::tool_health::ToolOutcome {
                    success: false,
                    latency_ms: 10,
                    result_hash,
                    at_epoch: now_epoch,
                    failure_category: None,
                },
            );
        }

        begin_recorded_turn(&mut harness, 1);
        let mut pipeline = harness.pipeline();
        let result = pipeline.validate_slot(HeadlessRoundToolIdx::ServerToolCall(0));
        assert!(matches!(result, HeadlessPipelineStage::ShortCircuit));
        assert!(
            pipeline.ctx.tool_results[0]
                .to_string()
                .contains("str_replace recovery required"),
            "expected str_replace-specific recovery advisory"
        );
        drop(pipeline);

        let tool_events = tool_trace_events(&harness);
        let skipped = tool_events
            .iter()
            .find(|(kind, _)| {
                matches!(
                    kind,
                    astra_pipeline::step_protocol::StepEventType::ToolCallSkipped
                )
            })
            .expect("expected skipped trace event");
        assert_eq!(
            skipped
                .1
                .as_ref()
                .and_then(|payload| payload.get("reason"))
                .and_then(Value::as_str),
            Some("str_replace_recovery_required")
        );
        assert!(
            skipped
                .1
                .as_ref()
                .and_then(|payload| payload.get("output"))
                .and_then(Value::as_str)
                .is_some_and(|output| output.contains("read_file the target range"))
        );
    }

    #[test]
    fn validate_slot_allows_retry_when_recent_success_exists() {
        let mut harness = PipelineHarness::new();
        harness.tool_calls.push(json!({
            "id": "call-grep-0",
            "function": { "name": "grep", "arguments": "{\"pattern\":\"headless\"}" }
        }));
        let sig = astra_turn_core::tool_result_semantics::tool_dedup_signature(
            "grep",
            &json!({"pattern":"headless"}),
        );
        let now_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        harness.turn_guard.health.record_outcome(
            &sig,
            astra_turn_core::tool_health::ToolOutcome {
                success: false,
                latency_ms: 10,
                result_hash: 1,
                at_epoch: now_epoch,
                failure_category: None,
            },
        );
        harness.turn_guard.health.record_outcome(
            &sig,
            astra_turn_core::tool_health::ToolOutcome {
                success: true,
                latency_ms: 8,
                result_hash: 2,
                at_epoch: now_epoch,
                failure_category: None,
            },
        );

        let mut pipeline = harness.pipeline();
        let result = pipeline.validate_slot(HeadlessRoundToolIdx::ServerToolCall(0));
        assert!(
            matches!(result, HeadlessPipelineStage::Continue(_)),
            "recent success should keep the tool callable"
        );
    }

    #[test]
    fn validate_slot_backs_off_repeated_identical_nonprogress_outcomes() {
        let mut harness = PipelineHarness::new();
        harness.valid_tool_names.insert("agent".to_string());
        let args = json!({"action":"get_result","agent_id":"general-purpose_demo@123"});
        harness.tool_calls.push(json!({
            "id": "call-agent-0",
            "function": { "name": "agent", "arguments": serde_json::to_string(&args).unwrap() }
        }));
        let sig = astra_turn_core::tool_result_semantics::tool_dedup_signature("agent", &args);
        let still_running = r#"{"status":"still_running","agent_id":"general-purpose_demo@123"}"#;
        harness.turn_guard.record_tool_outcome(
            &sig,
            astra_turn_core::result_quality::ResultQuality::Empty,
            10,
            still_running,
        );
        harness.turn_guard.record_tool_outcome(
            &sig,
            astra_turn_core::result_quality::ResultQuality::Empty,
            11,
            still_running,
        );

        let mut pipeline = harness.pipeline();
        let result = pipeline.validate_slot(HeadlessRoundToolIdx::ServerToolCall(0));
        assert!(matches!(result, HeadlessPipelineStage::ShortCircuit));
        assert!(
            pipeline.ctx.tool_results[0]
                .to_string()
                .contains("retry_deferred: Busy-poll backoff"),
            "repeated non-progress outcomes should trigger a short-term backoff"
        );
        drop(pipeline);
        assert_eq!(harness.tool_call_records.len(), 1);
        assert!(harness.tool_call_records[0].ok);
        assert!(harness.tool_call_records[0].is_synthetic_placeholder());
        assert!(!harness.tool_call_records[0].was_blocked_by_policy());
    }

    #[test]
    fn validate_slot_allows_poll_again_after_nonprogress_cooldown() {
        let mut harness = PipelineHarness::new();
        harness.valid_tool_names.insert("agent".to_string());
        let args = json!({"action":"get_result","agent_id":"general-purpose_demo@123"});
        harness.tool_calls.push(json!({
            "id": "call-agent-1",
            "function": { "name": "agent", "arguments": serde_json::to_string(&args).unwrap() }
        }));
        harness.edge_tool_round = vec![EdgeToolExecResult {
            request_id: "call-agent-1".to_string(),
            tool: "agent".to_string(),
            args: args.clone(),
            output: r#"{"status":"still_running","agent_id":"general-purpose_demo@123"}"#
                .to_string(),
            tool_result_fields: Some(edge_runtime_environment_fields()),
            status: "completed".to_string(),
            duration_ms: 4,
        }];
        let sig = astra_turn_core::tool_result_semantics::tool_dedup_signature("agent", &args);
        let now_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        for at_epoch in [now_epoch.saturating_sub(30), now_epoch.saturating_sub(29)] {
            harness.turn_guard.health.record_outcome(
                &sig,
                astra_turn_core::tool_health::ToolOutcome {
                    success: true,
                    latency_ms: 12,
                    result_hash: 7,
                    at_epoch,
                    failure_category: Some(
                        astra_turn_core::action_compensation::FailureCategory::NonProgress,
                    ),
                },
            );
        }

        let mut pipeline = harness.pipeline();
        let result = pipeline.validate_slot(HeadlessRoundToolIdx::ServerToolCall(0));
        assert!(
            matches!(result, HeadlessPipelineStage::Continue(_)),
            "non-progress backoff must expire so long-running tasks can be polled again later"
        );
    }

    #[tokio::test]
    async fn repeated_identical_cached_reads_are_suppressed_after_threshold() {
        let mut harness = PipelineHarness::new();

        short_circuit_cached_read_file(&mut harness, 0, "call-read-a-1", "a.txt").await;
        short_circuit_cached_read_file(&mut harness, 1, "call-read-a-2", "a.txt").await;
        short_circuit_cached_read_file(&mut harness, 2, "call-read-a-3", "a.txt").await;

        assert!(
            harness.tool_results[2]
                .to_string()
                .contains("Repeated cached read suppressed"),
            "third identical cached read should be a suppression advisory instead of replaying output, got: {:?}",
            harness.tool_results
        );
        let tool_events = tool_trace_events(&harness);
        let suppressed = tool_events
            .iter()
            .find(|(_, payload)| {
                payload.as_ref().is_some_and(|payload| {
                    payload.get("reason").and_then(Value::as_str)
                        == Some("repeated_cache_hit_suppressed")
                })
            })
            .expect("expected repeated cache-hit suppression trace");
        assert_eq!(
            suppressed
                .1
                .as_ref()
                .and_then(|payload| payload.get("cached"))
                .and_then(Value::as_bool),
            Some(true)
        );
    }

    #[test]
    fn cross_session_restored_outcome_memory_blocks_repeated_failure() {
        let sig = astra_turn_core::tool_result_semantics::tool_dedup_signature(
            "grep",
            &json!({"pattern":"headless"}),
        );
        let now_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let mut session_one_guard = TurnGuard::new();
        session_one_guard.health.record_failure("grep");
        session_one_guard.health.record_failure("grep");
        session_one_guard.health.record_outcome(
            &sig,
            astra_turn_core::tool_health::ToolOutcome {
                success: false,
                latency_ms: 10,
                result_hash: 1,
                at_epoch: now_epoch,
                failure_category: None,
            },
        );
        session_one_guard.health.record_outcome(
            &sig,
            astra_turn_core::tool_health::ToolOutcome {
                success: false,
                latency_ms: 12,
                result_hash: 2,
                at_epoch: now_epoch,
                failure_category: None,
            },
        );

        let exported = session_one_guard.health.export();
        assert_eq!(exported.len(), 1);
        assert_eq!(exported[0].recent_outcomes.len(), 1);

        let restored = astra_turn_core::tool_health::ToolHealthTracker::from_entries(&exported);
        let mut harness = PipelineHarness::new();
        harness.turn_guard = TurnGuard::with_health(restored);
        harness.tool_calls.push(json!({
            "id": "call-grep-0",
            "function": { "name": "grep", "arguments": "{\"pattern\":\"headless\"}" }
        }));

        let mut pipeline = harness.pipeline();
        let result = pipeline.validate_slot(HeadlessRoundToolIdx::ServerToolCall(0));
        assert!(matches!(result, HeadlessPipelineStage::ShortCircuit));
        assert!(
            pipeline.ctx.tool_results[0]
                .to_string()
                .contains("identical_retry_suppressed"),
            "restored identical failure history should suppress the next-session identical retry"
        );
    }

    #[tokio::test]
    async fn restored_outcome_memory_reduces_recovery_executions_vs_blind_retry() {
        async fn run_recovery_turn(
            restored: Option<astra_turn_core::tool_health::ToolHealthTracker>,
        ) -> (u32, usize, usize) {
            let mut harness = PipelineHarness::new();
            harness.valid_tool_names.insert("outline".to_string());
            harness.tool_calls.push(json!({
                "id": "call-outline-0",
                "function": { "name": "outline", "arguments": "{}" }
            }));
            if let Some(health) = restored {
                harness.turn_guard = TurnGuard::with_health(health);
            }
            let before_outline_calls = harness
                .turn_guard
                .health
                .get("outline")
                .map(|h| h.total_calls)
                .unwrap_or(0);
            let dir = tempfile::TempDir::new().unwrap();
            let server_exec = server_executor_for_test_workspace(dir.path(), "test-session");
            let mut pipeline = harness.pipeline_with_server_executor(0, Some(&server_exec));

            match pipeline.validate_slot(HeadlessRoundToolIdx::ServerToolCall(0)) {
                HeadlessPipelineStage::Continue(validated) => {
                    let permitted = match pipeline.permit_execution(validated).await {
                        HeadlessPipelineStage::Continue(p) => p,
                        _ => panic!("expected permitted outline execution"),
                    };
                    let executed = pipeline.execute_execution(permitted).await;
                    pipeline.record_execution(executed).await;
                }
                HeadlessPipelineStage::ShortCircuit => {}
                HeadlessPipelineStage::AbortRound => panic!("unexpected abort"),
            }

            let validated = match pipeline.validate_slot(HeadlessRoundToolIdx::SyntheticEdge(0)) {
                HeadlessPipelineStage::Continue(v) => v,
                _ => panic!("expected Continue for grep fallback"),
            };
            let permitted = match pipeline.permit_execution(validated).await {
                HeadlessPipelineStage::Continue(p) => p,
                _ => panic!("expected Continue for grep fallback"),
            };
            let executed = pipeline.execute_execution(permitted).await;
            assert!(!executed.is_err, "grep fallback should succeed");
            pipeline.record_execution(executed).await;

            let after_outline_calls = pipeline
                .ctx
                .turn_guard
                .health
                .get("outline")
                .map(|h| h.total_calls)
                .unwrap_or(0);
            let grep_calls = pipeline
                .ctx
                .turn_guard
                .health
                .get("grep")
                .map(|h| h.total_calls)
                .unwrap_or(0);
            (
                pipeline.executed_this_turn,
                after_outline_calls.saturating_sub(before_outline_calls),
                grep_calls,
            )
        }

        let blind_retry = run_recovery_turn(None).await;

        let mut prior_guard = TurnGuard::new();
        let outline_sig =
            astra_turn_core::tool_result_semantics::tool_dedup_signature("outline", &json!({}));
        let now_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        prior_guard.health.record_failure("outline");
        prior_guard.health.record_failure("outline");
        prior_guard.health.record_outcome(
            &outline_sig,
            astra_turn_core::tool_health::ToolOutcome {
                success: false,
                latency_ms: 10,
                result_hash: 1,
                at_epoch: now_epoch,
                failure_category: None,
            },
        );
        prior_guard.health.record_outcome(
            &outline_sig,
            astra_turn_core::tool_health::ToolOutcome {
                success: false,
                latency_ms: 11,
                result_hash: 2,
                at_epoch: now_epoch,
                failure_category: None,
            },
        );
        let restored = astra_turn_core::tool_health::ToolHealthTracker::from_entries(
            &prior_guard.health.export(),
        );
        let memory_guided = run_recovery_turn(Some(restored)).await;

        assert_eq!(blind_retry.2, 1, "blind retry still reaches grep success");
        assert_eq!(
            memory_guided.2, 1,
            "memory-guided path still reaches grep success"
        );
        assert_eq!(
            blind_retry.1, 1,
            "blind retry incurs one fresh outline failure"
        );
        assert_eq!(
            memory_guided.1, 0,
            "restored failure memory should prevent another outline execution"
        );
        assert!(
            memory_guided.0 < blind_retry.0,
            "memory-guided recovery should use fewer actual tool executions: blind={:?}, memory={:?}",
            blind_retry,
            memory_guided
        );
    }

    #[tokio::test]
    async fn unknown_tool_missing_catalog_does_not_affect_valid_tool_health() {
        let mut harness = PipelineHarness::new();
        // Call 1: schema-invalid tool "outline"
        harness.tool_calls.push(json!({
            "id": "call-outline-0",
            "function": { "name": "outline", "arguments": "{}" }
        }));
        // Call 2: valid tool "grep" (via synthetic edge, already in harness)
        // Call 3: schema-invalid tool "outline" with different args
        harness.tool_calls.push(json!({
            "id": "call-outline-1",
            "function": { "name": "outline", "arguments": "{\"path\": \"b.rs\"}" }
        }));
        let mut pipeline = harness.pipeline();

        // Unknown tool failure #1
        pipeline.validate_slot(HeadlessRoundToolIdx::ServerToolCall(0));

        // Valid tool success (grep via synthetic edge)
        let validated = match pipeline.validate_slot(HeadlessRoundToolIdx::SyntheticEdge(0)) {
            HeadlessPipelineStage::Continue(v) => v,
            _ => panic!("expected Continue for grep"),
        };
        let permitted = match pipeline.permit_execution(validated).await {
            HeadlessPipelineStage::Continue(p) => p,
            _ => panic!("expected Continue for grep"),
        };
        let executed = pipeline.execute_execution(permitted).await;
        assert!(!executed.is_err);
        pipeline.record_execution(executed).await;

        // Unknown tool failure #2
        pipeline.validate_slot(HeadlessRoundToolIdx::ServerToolCall(1));

        assert!(
            pipeline.ctx.turn_guard.health.get("outline").is_none(),
            "schema-invalid tools should not create health state"
        );

        // grep should show success.
        let grep_h = pipeline.ctx.turn_guard.health.get("grep").unwrap();
        assert_eq!(grep_h.consecutive_failures, 0);
        assert_eq!(grep_h.total_calls, 1);
    }

    #[tokio::test]
    async fn pre_tool_hook_block_short_circuit_records_step_skip_trace() {
        let mut harness = PipelineHarness::new();
        harness.tool_event_hooks = ToolEventHookRegistry::new(vec![ToolEventHook {
            event: ToolEventKind::PreToolUse,
            matcher: "grep".into(),
            action: HookAction::Shell {
                command: r#"echo '{"decision":"block","reason":"policy denied"}'"#.into(),
            },
            timeout_secs: 5,
            is_async: false,
            condition: None,
            once: false,
            priority: 0,
        }]);
        begin_recorded_turn(&mut harness, 1);
        let mut pipeline = harness.pipeline();
        let validated = match pipeline.validate_slot(HeadlessRoundToolIdx::SyntheticEdge(0)) {
            HeadlessPipelineStage::Continue(validated) => validated,
            _ => panic!("expected validated execution"),
        };

        assert!(matches!(
            pipeline.permit_execution(validated).await,
            HeadlessPipelineStage::ShortCircuit
        ));
        drop(pipeline);

        let tool_events = tool_trace_events(&harness);
        assert_eq!(
            tool_events.len(),
            2,
            "pre-tool hook block should emit started+skipped trace events"
        );
        assert!(matches!(
            tool_events[0].0,
            astra_pipeline::step_protocol::StepEventType::ToolCallStarted
        ));
        assert!(matches!(
            tool_events[1].0,
            astra_pipeline::step_protocol::StepEventType::ToolCallSkipped
        ));
        assert_eq!(
            tool_events[1]
                .1
                .as_ref()
                .and_then(|payload| payload.get("reason"))
                .and_then(Value::as_str),
            Some("pre_tool_hook_blocked")
        );
    }
}
