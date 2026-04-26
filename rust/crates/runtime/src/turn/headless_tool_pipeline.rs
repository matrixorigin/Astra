use std::collections::{HashMap, HashSet};
use std::time::Duration;

use astra_services::session_journal::ToolCallRecord;
use astra_thin_client::ThinClient;
use serde_json::{Map, Value};

use super::agentic_headless_round::{HeadlessRoundTerminal, PermissionSyncHandle};
use super::headless_tool_assembly::{
    EdgeToolRoundRow, HeadlessResolvedToolSlot, HeadlessRoundToolIdx, resolve_headless_tool_slot,
    take_edge_output_for_tool_call_with_duration,
};
use crate::pipeline::step_protocol::{IdempotencyKey, InMemoryIdempotencyCache};
use crate::pipeline::step_recorder::StepRecorder;
use crate::semantic_dedup::SemanticDedup;
use crate::turn::turn_guard::TurnGuard;

mod execute;
mod policy;
mod record;

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

struct HeadlessBlockedTool<'a> {
    id: &'a str,
    name: &'a str,
    args: &'a Value,
    reason_code: &'a str,
    err_msg: String,
    journal_reason: String,
    early_exit_ms: u64,
    status_line: Option<String>,
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
    pub turn_index: usize,
    pub quiet: bool,
    pub api: &'a ThinClient,
    pub token: &'a str,
    pub current_session_id: Option<&'a String>,
    pub tool_calls: &'a [Value],
    pub edge_tool_round: &'a [E],
    pub by_sig: &'a HashMap<String, String>,
    pub pre_resolved_ids: &'a HashSet<String>,
    pub messages: &'a mut Vec<Value>,
    pub tool_results: &'a mut Vec<Value>,
    pub valid_tool_names: &'a HashSet<String>,
    pub restricted_tools: &'a mut HashSet<String>,
    pub turn_guard: &'a mut TurnGuard,
    pub step_recorder: &'a mut StepRecorder,
    pub idempotency_cache: &'a mut InMemoryIdempotencyCache,
    pub semantic_dedup: &'a mut SemanticDedup,
    pub call_counts: &'a mut HashMap<String, u32>,
    pub max_identical_calls: u32,
    pub max_tools_per_turn: u32,
    pub tool_call_records: &'a mut Vec<ToolCallRecord>,
    pub tool_event_hooks: &'a crate::skills::hooks::ToolEventHookRegistry,
    pub term: &'a mut dyn HeadlessRoundTerminal,
    pub mailbox: Option<&'a mut crate::messaging::router::AgentMailbox>,
    pub permission_context: Option<&'a PermissionSyncHandle>,
    pub progress_emitter: Option<&'a crate::orchestration::AgentProgressEmitter>,
    pub effective_permission_timeout: Duration,
    /// Optional server-side tool executor for web agent sessions (no CLI edge agent).
    /// When present, tools that have no edge match are executed directly by the server.
    pub server_tool_executor: Option<&'a crate::server::server_tool_executor::ServerToolExecutor>,
    // ── Observability (Phase 1) ──
    /// Turn start instant for computing start_offset_ms on tool records.
    pub turn_start: Option<std::time::Instant>,
    /// Current LLM round index (0-based) within this turn.
    pub llm_round: u32,
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
        let matched = take_edge_output_for_tool_call_with_duration(
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
    /// After this many consecutive empty-name tool calls in one headless round,
    /// stop processing — the model is stuck emitting malformed calls.
    const MAX_CONSECUTIVE_EMPTY_NAME: u32 = 3;

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

    fn resolve_slot(&self, item: HeadlessRoundToolIdx) -> HeadlessResolvedToolSlot {
        resolve_headless_tool_slot(item, self.ctx.tool_calls, |i| {
            let edge = &self.ctx.edge_tool_round[i];
            (edge.tool_name().to_string(), edge.tool_args().clone())
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
        use super::headless_tool_pipeline::execute::execute_tool_pure;

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

        let server_executor = self.ctx.server_tool_executor;
        let api = self.ctx.api;
        let token = self.ctx.token;
        let session_id = self.ctx.current_session_id;
        let turn_index = self.ctx.turn_index;

        let futs: Vec<_> = executions
            .iter_mut()
            .map(|(exec, _)| {
                execute_tool_pure(exec, server_executor, api, token, session_id, turn_index)
            })
            .collect();
        futures_util::future::join_all(futs).await;

        // Phase 3: post-process + record serially (fast, needs &mut self).
        for (execution, idem_key) in executions {
            let is_err = crate::turn::tool_result_semantics::is_tool_error(&execution.result_str);
            let executed_ms = if execution.is_edge_tool && execution.edge_duration_ms > 0 {
                execution.edge_duration_ms
            } else {
                0
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

    use crate::pipeline::step_protocol::CachedToolResult;
    use crate::skills::hooks::{HookAction, ToolEventHook, ToolEventHookRegistry, ToolEventKind};
    use crate::turn::agentic_headless_round::NoopHeadlessTerminal;
    use crate::turn::sse_stream_host::EdgeToolExecResult;

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
        let args = json!({ "path": path });
        let idem_key = IdempotencyKey::semantic("read_file", &args);
        harness.idempotency_cache.record(
            &idem_key,
            CachedToolResult {
                tool_name: "read_file".to_string(),
                output: format!("cached {path}"),
                is_error: false,
                cached_at: 0,
            },
        );

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
    ) -> Vec<(crate::pipeline::step_protocol::StepEventType, Option<Value>)> {
        harness
            .step_recorder
            .events()
            .iter()
            .filter(|event| {
                matches!(
                    event.event_type,
                    crate::pipeline::step_protocol::StepEventType::ToolCallStarted
                        | crate::pipeline::step_protocol::StepEventType::ToolCallSkipped
                        | crate::pipeline::step_protocol::StepEventType::ToolCallCompleted
                        | crate::pipeline::step_protocol::StepEventType::ToolCallFailed
                )
            })
            .map(|event| (event.event_type.clone(), event.payload.clone()))
            .collect()
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
        restricted_tools: HashSet<String>,
        turn_guard: TurnGuard,
        step_recorder: StepRecorder,
        idempotency_cache: InMemoryIdempotencyCache,
        semantic_dedup: SemanticDedup,
        call_counts: HashMap<String, u32>,
        tool_call_records: Vec<ToolCallRecord>,
        tool_event_hooks: ToolEventHookRegistry,
        term: NoopHeadlessTerminal,
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
                    tool_result_fields: None,
                    status: "ok".to_string(),
                    duration_ms: 12,
                }],
                by_sig: HashMap::new(),
                pre_resolved_ids: HashSet::new(),
                messages: Vec::new(),
                tool_results: Vec::new(),
                valid_tool_names: HashSet::from(["grep".to_string()]),
                restricted_tools: HashSet::new(),
                turn_guard: TurnGuard::new(),
                step_recorder: StepRecorder::new("test-session", "test-task"),
                idempotency_cache: InMemoryIdempotencyCache::new(),
                semantic_dedup: SemanticDedup::new(0.95),
                call_counts: HashMap::new(),
                tool_call_records: Vec::new(),
                tool_event_hooks: ToolEventHookRegistry::default(),
                term: NoopHeadlessTerminal,
            }
        }

        fn pipeline(&mut self) -> HeadlessToolExecutionPipeline<'_, EdgeToolExecResult> {
            self.pipeline_with_server_executor(0, None)
        }

        fn pipeline_with_server_executor<'a>(
            &'a mut self,
            turn_index: usize,
            server_tool_executor: Option<
                &'a crate::server::server_tool_executor::ServerToolExecutor,
            >,
        ) -> HeadlessToolExecutionPipeline<'a, EdgeToolExecResult> {
            HeadlessToolExecutionPipeline::new(
                HeadlessToolExecutionCtx {
                    turn_index,
                    quiet: true,
                    api: &self.api,
                    token: "",
                    current_session_id: None,
                    tool_calls: &self.tool_calls,
                    edge_tool_round: &self.edge_tool_round,
                    by_sig: &self.by_sig,
                    pre_resolved_ids: &self.pre_resolved_ids,
                    messages: &mut self.messages,
                    tool_results: &mut self.tool_results,
                    valid_tool_names: &self.valid_tool_names,
                    restricted_tools: &mut self.restricted_tools,
                    turn_guard: &mut self.turn_guard,
                    step_recorder: &mut self.step_recorder,
                    idempotency_cache: &mut self.idempotency_cache,
                    semantic_dedup: &mut self.semantic_dedup,
                    call_counts: &mut self.call_counts,
                    max_identical_calls: 2,
                    max_tools_per_turn: 15,
                    tool_call_records: &mut self.tool_call_records,
                    tool_event_hooks: &self.tool_event_hooks,
                    term: &mut self.term,
                    mailbox: None,
                    permission_context: None,
                    progress_emitter: None,
                    effective_permission_timeout: Duration::from_secs(30),
                    server_tool_executor,
                    turn_start: None,
                    llm_round: 0,
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
                    IdempotencyKey::semantic("grep", &json!({ "pattern": "headless" })).cache_key()
                );
            }
            _ => panic!("expected permitted execution"),
        }
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
            crate::pipeline::step_protocol::StepEventType::ToolCallStarted
        ));
        assert!(matches!(
            tool_events[1].0,
            crate::pipeline::step_protocol::StepEventType::ToolCallSkipped
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
            crate::pipeline::step_protocol::StepEventType::ToolCallStarted
        ));
        assert!(matches!(
            tool_events[1].0,
            crate::pipeline::step_protocol::StepEventType::ToolCallSkipped
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
            crate::pipeline::step_protocol::StepEventType::ToolCallStarted
        ));
        assert!(matches!(
            tool_events[1].0,
            crate::pipeline::step_protocol::StepEventType::ToolCallSkipped
        ));
        assert_eq!(
            tool_events[1]
                .1
                .as_ref()
                .and_then(|payload| payload.get("reason"))
                .and_then(Value::as_str),
            Some("semantic_dedup_pre_check")
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
            crate::pipeline::step_protocol::StepEventType::ToolCallStarted
        ));
        assert!(matches!(
            tool_events[1].0,
            crate::pipeline::step_protocol::StepEventType::ToolCallSkipped
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
            "cached cross-turn short-circuit should emit started+skipped trace events"
        );
        assert!(matches!(
            tool_events[0].0,
            crate::pipeline::step_protocol::StepEventType::ToolCallStarted
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
            crate::pipeline::step_protocol::StepEventType::ToolCallSkipped
        ));
        let skipped_payload = tool_events[1].1.as_ref().expect("skipped payload");
        assert_eq!(
            skipped_payload.get("reason").and_then(Value::as_str),
            Some("cached_cross_turn")
        );
        assert_eq!(
            skipped_payload.get("cached").and_then(Value::as_bool),
            Some(true)
        );
        assert!(
            skipped_payload
                .get("args_preview")
                .and_then(Value::as_str)
                .is_some_and(|preview| preview.contains("a.txt")),
            "skipped trace should include args preview, got: {skipped_payload:?}"
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
        let idem_key = IdempotencyKey::semantic("grep", &json!({ "pattern": "headless" }));
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
        harness.edge_tool_round[0].tool_result_fields = Some(Map::from_iter([
            (
                "status".to_string(),
                Value::String("partial_failure".to_string()),
            ),
            (
                "output".to_string(),
                Value::String("permission denied".to_string()),
            ),
        ]));

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
    async fn server_fallback_sets_turn_index_for_current_turn_rollback() {
        let mut harness = PipelineHarness::new();
        let dir = tempfile::TempDir::new().unwrap();
        let server_exec = crate::server::server_tool_executor::ServerToolExecutor::new(
            dir.path().to_path_buf(),
            "test-user".into(),
            "test-session".into(),
            None,
            None,
        );
        let mut pipeline = harness.pipeline_with_server_executor(7, Some(&server_exec));
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
    async fn server_fallback_preserves_tool_result_fields() {
        let mut harness = PipelineHarness::new();
        let dir = tempfile::TempDir::new().unwrap();
        init_git_repo(dir.path());
        std::fs::write(dir.path().join("tracked.txt"), "hello\n").unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(dir.path())
            .output()
            .unwrap();
        let server_exec = crate::server::server_tool_executor::ServerToolExecutor::new(
            dir.path().to_path_buf(),
            "test-user".into(),
            "test-session".into(),
            None,
            None,
        );
        let mut pipeline = harness.pipeline_with_server_executor(3, Some(&server_exec));
        let args = json!({"message": "initial"});
        let permitted = PermittedExecution {
            execution: HeadlessResolvedExecution {
                id: "call-git-commit".into(),
                name: "git_commit".into(),
                args: args.clone(),
                result_str: "Error: headless edge protocol: no matching edge result".into(),
                tool_result_fields: None,
                edge_duration_ms: 0,
                is_edge_tool: false,
                early_exit_ms: 0,
            },
            idem_key: IdempotencyKey::semantic("git_commit", &args),
        };

        let executed = pipeline.execute_execution(permitted).await;
        assert!(!executed.is_err, "got: {}", executed.execution.result_str);
        let result_fields = executed
            .execution
            .tool_result_fields
            .as_ref()
            .expect("server fallback metadata");
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
    async fn server_fallback_surfaces_read_file_large_file_preview() {
        let mut harness = PipelineHarness::new();
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("large.txt"),
            "0123456789abcdef\n".repeat(6_000),
        )
        .unwrap();
        let server_exec = crate::server::server_tool_executor::ServerToolExecutor::new(
            dir.path().to_path_buf(),
            "test-user".into(),
            "test-session".into(),
            None,
            None,
        );
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
            executed.execution.result_str.contains("start_line"),
            "got: {}",
            executed.execution.result_str
        );
    }

    #[tokio::test]
    async fn server_fallback_surfaces_bash_timeout_partial_output() {
        let mut harness = PipelineHarness::new();
        let dir = tempfile::TempDir::new().unwrap();
        let server_exec = crate::server::server_tool_executor::ServerToolExecutor::new(
            dir.path().to_path_buf(),
            "test-user".into(),
            "test-session".into(),
            None,
            None,
        );
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

    // ── Unknown tool health tracking tests ───────────────────────────

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
    async fn unknown_tool_records_health_failure() {
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
            crate::pipeline::step_protocol::StepEventType::ToolCallStarted
        ));
        assert!(matches!(
            tool_events[1].0,
            crate::pipeline::step_protocol::StepEventType::ToolCallSkipped
        ));
        assert_eq!(
            tool_events[1]
                .1
                .as_ref()
                .and_then(|payload| payload.get("reason"))
                .and_then(Value::as_str),
            Some("unknown_tool")
        );

        // The health tracker must have recorded a failure for "outline".
        let health = harness.turn_guard.health.get("outline");
        assert!(health.is_some(), "outline should be tracked");
        let h = health.unwrap();
        assert_eq!(h.total_calls, 1);
        assert_eq!(h.total_failures, 1);
        assert_eq!(h.consecutive_failures, 1);
        assert!(!h.deprioritized, "1 failure should not deprioritize yet");
    }

    #[tokio::test]
    async fn unknown_tool_deprioritized_after_consecutive_failures() {
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

        // After 3 consecutive failures, the tool must be deprioritized.
        let health = pipeline.ctx.turn_guard.health.get("outline").unwrap();
        assert_eq!(health.consecutive_failures, 3);
        assert!(
            health.deprioritized,
            "outline should be deprioritized after 3 consecutive failures"
        );
        assert!(
            pipeline
                .ctx
                .turn_guard
                .health
                .deprioritized_tools()
                .contains(&"outline"),
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
    async fn empty_name_tool_records_health_failure() {
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
            crate::pipeline::step_protocol::StepEventType::ToolCallStarted
        ));
        assert!(matches!(
            tool_events[1].0,
            crate::pipeline::step_protocol::StepEventType::ToolCallSkipped
        ));
        assert_eq!(
            tool_events[1]
                .1
                .as_ref()
                .and_then(|payload| payload.get("reason"))
                .and_then(Value::as_str),
            Some("unknown_tool")
        );

        // Empty-name tool should also be tracked in health.
        let health = harness.turn_guard.health.get("");
        assert!(health.is_some(), "empty-name tool should be tracked");
        assert_eq!(health.unwrap().total_failures, 1);
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

        // Health should show 2 failures (dedup doesn't add a 3rd failure).
        let health = pipeline.ctx.turn_guard.health.get("outline").unwrap();
        assert_eq!(health.total_failures, 2);
    }

    #[tokio::test]
    async fn multiple_different_unknown_tools_each_tracked_independently() {
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

        let outline_h = pipeline.ctx.turn_guard.health.get("outline").unwrap();
        assert_eq!(outline_h.consecutive_failures, 2);
        assert!(!outline_h.deprioritized);

        let foobar_h = pipeline.ctx.turn_guard.health.get("foobar").unwrap();
        assert_eq!(foobar_h.consecutive_failures, 1);
        assert!(!foobar_h.deprioritized);
    }

    #[tokio::test]
    async fn unknown_tool_deprioritize_warning_generated() {
        let mut harness = PipelineHarness::new();
        // 3 calls with different args to avoid dedup, trigger deprioritization.
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

        let warning = pipeline.ctx.turn_guard.health.deprioritize_warning();
        assert!(warning.is_some(), "should generate deprioritize warning");
        assert!(
            warning.unwrap().contains("outline"),
            "warning should mention the deprioritized tool"
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

        // All 3 should have recorded health failures.
        let health = pipeline.ctx.turn_guard.health.get("").unwrap();
        assert_eq!(health.total_failures, 3);
        assert_eq!(health.consecutive_failures, 3);
    }

    #[tokio::test]
    async fn deprioritized_unknown_tool_merges_into_restricted() {
        let mut harness = PipelineHarness::new();
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

        // Simulate what the server loop does between turns.
        assert!(!pipeline.ctx.restricted_tools.contains("outline"));
        crate::turn::turn_guard::merge_deprioritized_tools_into_restricted(
            pipeline.ctx.turn_guard,
            pipeline.ctx.restricted_tools,
        );
        assert!(
            pipeline.ctx.restricted_tools.contains("outline"),
            "deprioritized unknown tool should be added to restricted_tools"
        );
    }

    #[tokio::test]
    async fn server_fallback_unknown_tool_records_health_failure() {
        // Simulates the DefaultToolExecutor "not available" path:
        // tool passes valid_tool_names but executor returns error.
        let mut harness = PipelineHarness::new();
        // Add "outline" to valid_tool_names so it passes validation.
        harness.valid_tool_names.insert("outline".to_string());
        harness.tool_calls.push(json!({
            "id": "call-outline-0",
            "function": { "name": "outline", "arguments": "{}" }
        }));
        let dir = tempfile::TempDir::new().unwrap();
        let server_exec = crate::server::server_tool_executor::ServerToolExecutor::new(
            dir.path().to_path_buf(),
            "test-user".into(),
            "test-session".into(),
            None,
            None,
        );
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

        // execute_execution: ServerToolExecutor doesn't know "outline",
        // returns "Error: Tool 'outline' not available..."
        let executed = pipeline.execute_execution(permitted).await;
        assert!(
            executed.is_err,
            "server executor should return error for unknown tool"
        );

        // record_execution feeds through append_headless_result_quality_feedback
        // → turn_guard.record_tool_result → health.record_failure
        pipeline.record_execution(executed).await;

        let health = pipeline.ctx.turn_guard.health.get("outline");
        assert!(
            health.is_some(),
            "outline should be tracked after server fallback error"
        );
        let h = health.unwrap();
        assert_eq!(
            h.total_failures, 1,
            "server fallback error should count as failure"
        );
        assert_eq!(h.consecutive_failures, 1);
    }

    #[tokio::test]
    async fn semantic_dedup_does_not_block_git_diff_path_after_stat_only() {
        let mut harness = PipelineHarness::new();
        harness.valid_tool_names.insert("git_diff".to_string());

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

        let server_exec = crate::server::server_tool_executor::ServerToolExecutor::new(
            dir.path().to_path_buf(),
            "test-user".into(),
            "test-session".into(),
            None,
            None,
        );

        harness.tool_calls.push(json!({
            "id": "call-git-diff-stat",
            "function": { "name": "git_diff", "arguments": "{\"stat_only\":true}" }
        }));
        {
            let mut pipeline = harness.pipeline_with_server_executor(0, Some(&server_exec));
            let validated = match pipeline.validate_slot(HeadlessRoundToolIdx::ServerToolCall(0)) {
                HeadlessPipelineStage::Continue(v) => v,
                _ => panic!("expected stat_only git_diff to validate"),
            };
            let permitted = match pipeline.permit_execution(validated).await {
                HeadlessPipelineStage::Continue(p) => p,
                _ => panic!("expected stat_only git_diff to execute"),
            };
            let executed = pipeline.execute_execution(permitted).await;
            assert!(!executed.is_err, "got: {}", executed.execution.result_str);
            pipeline.record_execution(executed).await;
        }

        harness.tool_calls.clear();
        harness.tool_calls.push(json!({
            "id": "call-git-diff-path",
            "function": { "name": "git_diff", "arguments": "{\"path\":\"tracked.txt\"}" }
        }));
        let mut pipeline = harness.pipeline_with_server_executor(1, Some(&server_exec));
        let validated = match pipeline.validate_slot(HeadlessRoundToolIdx::ServerToolCall(0)) {
            HeadlessPipelineStage::Continue(v) => v,
            _ => panic!("expected path-scoped git_diff to validate"),
        };
        let permitted = match pipeline.permit_execution(validated).await {
            HeadlessPipelineStage::Continue(p) => p,
            HeadlessPipelineStage::ShortCircuit => {
                panic!("path-scoped git_diff must not be semantically blocked by earlier stat_only")
            }
            HeadlessPipelineStage::AbortRound => panic!("unexpected abort"),
        };
        let executed = pipeline.execute_execution(permitted).await;
        assert!(!executed.is_err, "got: {}", executed.execution.result_str);
        assert!(
            executed.execution.result_str.contains("@@"),
            "path-scoped git_diff should execute and return patch hunks, got: {}",
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
        harness.valid_tool_names.insert("outline".to_string());
        harness.tool_calls.push(json!({
            "id": "call-outline-0",
            "function": { "name": "outline", "arguments": "{}" }
        }));
        let dir = tempfile::TempDir::new().unwrap();
        let server_exec = crate::server::server_tool_executor::ServerToolExecutor::new(
            dir.path().to_path_buf(),
            "test-user".into(),
            "test-session".into(),
            None,
            None,
        );
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

        let sig = crate::turn::tool_result_semantics::tool_dedup_signature(
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
        let sig = crate::turn::tool_result_semantics::tool_dedup_signature(
            "grep",
            &json!({"pattern":"headless"}),
        );
        let now_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        harness.turn_guard.health.record_outcome(
            &sig,
            crate::turn::tool_health::ToolOutcome {
                success: false,
                latency_ms: 10,
                result_hash: 1,
                at_epoch: now_epoch,
                failure_category: None,
            },
        );
        harness.turn_guard.health.record_outcome(
            &sig,
            crate::turn::tool_health::ToolOutcome {
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
                .contains("Outcome memory blocked"),
            "expected blocked outcome-memory advisory in tool result"
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
            crate::pipeline::step_protocol::StepEventType::ToolCallStarted
        ));
        assert!(matches!(
            tool_events[1].0,
            crate::pipeline::step_protocol::StepEventType::ToolCallSkipped
        ));
        assert_eq!(
            tool_events[1]
                .1
                .as_ref()
                .and_then(|payload| payload.get("reason"))
                .and_then(Value::as_str),
            Some("outcome_memory_blocked")
        );
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
        let sig = crate::turn::tool_result_semantics::tool_dedup_signature("str_replace", &args);
        let now_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        for result_hash in [11, 12] {
            harness.turn_guard.health.record_outcome(
                &sig,
                crate::turn::tool_health::ToolOutcome {
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
                    crate::pipeline::step_protocol::StepEventType::ToolCallSkipped
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
        let sig = crate::turn::tool_result_semantics::tool_dedup_signature(
            "grep",
            &json!({"pattern":"headless"}),
        );
        let now_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        harness.turn_guard.health.record_outcome(
            &sig,
            crate::turn::tool_health::ToolOutcome {
                success: false,
                latency_ms: 10,
                result_hash: 1,
                at_epoch: now_epoch,
                failure_category: None,
            },
        );
        harness.turn_guard.health.record_outcome(
            &sig,
            crate::turn::tool_health::ToolOutcome {
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
        let sig = crate::turn::tool_result_semantics::tool_dedup_signature(
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
            crate::turn::tool_health::ToolOutcome {
                success: false,
                latency_ms: 10,
                result_hash: 1,
                at_epoch: now_epoch,
                failure_category: None,
            },
        );
        session_one_guard.health.record_outcome(
            &sig,
            crate::turn::tool_health::ToolOutcome {
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

        let restored = crate::turn::tool_health::ToolHealthTracker::from_entries(&exported);
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
                .contains("Outcome memory blocked"),
            "restored identical failure history should block the next-session retry"
        );
    }

    #[tokio::test]
    async fn restored_outcome_memory_reduces_recovery_executions_vs_blind_retry() {
        async fn run_recovery_turn(
            restored: Option<crate::turn::tool_health::ToolHealthTracker>,
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
            let server_exec = crate::server::server_tool_executor::ServerToolExecutor::new(
                dir.path().to_path_buf(),
                "test-user".into(),
                "test-session".into(),
                None,
                None,
            );
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
            crate::turn::tool_result_semantics::tool_dedup_signature("outline", &json!({}));
        let now_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        prior_guard.health.record_failure("outline");
        prior_guard.health.record_failure("outline");
        prior_guard.health.record_outcome(
            &outline_sig,
            crate::turn::tool_health::ToolOutcome {
                success: false,
                latency_ms: 10,
                result_hash: 1,
                at_epoch: now_epoch,
                failure_category: None,
            },
        );
        prior_guard.health.record_outcome(
            &outline_sig,
            crate::turn::tool_health::ToolOutcome {
                success: false,
                latency_ms: 11,
                result_hash: 2,
                at_epoch: now_epoch,
                failure_category: None,
            },
        );
        let restored =
            crate::turn::tool_health::ToolHealthTracker::from_entries(&prior_guard.health.export());
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
    async fn unknown_tool_failure_not_reset_by_valid_tool_success() {
        let mut harness = PipelineHarness::new();
        // Call 1: unknown tool "outline"
        harness.tool_calls.push(json!({
            "id": "call-outline-0",
            "function": { "name": "outline", "arguments": "{}" }
        }));
        // Call 2: valid tool "grep" (via synthetic edge, already in harness)
        // Call 3: unknown tool "outline" with different args
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

        // grep success should NOT reset outline's consecutive failures.
        let outline_h = pipeline.ctx.turn_guard.health.get("outline").unwrap();
        assert_eq!(
            outline_h.consecutive_failures, 2,
            "valid tool success should not reset unknown tool's consecutive failures"
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
            crate::pipeline::step_protocol::StepEventType::ToolCallStarted
        ));
        assert!(matches!(
            tool_events[1].0,
            crate::pipeline::step_protocol::StepEventType::ToolCallSkipped
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
