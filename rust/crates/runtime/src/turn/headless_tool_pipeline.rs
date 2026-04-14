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
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use crate::skills::hooks::{HookAction, ToolEventHook, ToolEventHookRegistry, ToolEventKind};
    use crate::turn::agentic_headless_round::NoopHeadlessTerminal;
    use crate::turn::sse_stream_host::EdgeToolExecResult;

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
            HeadlessToolExecutionPipeline::new(
                HeadlessToolExecutionCtx {
                    turn_index: 0,
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
                    server_tool_executor: None,
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
}
