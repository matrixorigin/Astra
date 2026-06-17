use super::*;
use async_trait::async_trait;
use serde_json::Value;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

use super::super::tool_transport_plan::{
    EdgeBoundExecutionPlan, RunnerRpcExecutionPlan, edge_executor_id,
};

struct CountingLocalTransport {
    calls: AtomicUsize,
}

impl CountingLocalTransport {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ServerLocalToolTransport for CountingLocalTransport {
    async fn execute_server_local_tool(
        &self,
        request: &ToolExecutionRequest,
    ) -> astra_tools::ToolResult {
        self.calls.fetch_add(1, Ordering::SeqCst);
        astra_tools::ToolResult::text(format!("local:{}", request.tool_name))
    }
}

struct PendingLocalTransport {
    calls: AtomicUsize,
    execute_started: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

impl PendingLocalTransport {
    fn new(execute_started: tokio::sync::oneshot::Sender<()>) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            execute_started: Mutex::new(Some(execute_started)),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ServerLocalToolTransport for PendingLocalTransport {
    async fn execute_server_local_tool(
        &self,
        _request: &ToolExecutionRequest,
    ) -> astra_tools::ToolResult {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let sender = self
            .execute_started
            .lock()
            .expect("local execute started lock")
            .take();
        if let Some(sender) = sender {
            let _ = sender.send(());
        }
        std::future::pending::<()>().await;
        unreachable!("pending local execute never completes")
    }
}

struct StaticRunnerRpcTransport {
    prepare_calls: Mutex<Vec<astra_runtime_env::RunnerPrepareSessionRequest>>,
    execute_calls: Mutex<Vec<astra_runtime_env::RunnerExecuteToolRequest>>,
    prepare_error: Option<astra_runtime_env::RuntimeError>,
    execute_error: Option<astra_runtime_env::RuntimeError>,
    output: String,
}

impl StaticRunnerRpcTransport {
    fn new() -> Self {
        Self {
            prepare_calls: Mutex::new(Vec::new()),
            execute_calls: Mutex::new(Vec::new()),
            prepare_error: None,
            execute_error: None,
            output: "runner-result".to_string(),
        }
    }

    fn with_prepare_error(error: astra_runtime_env::RuntimeError) -> Self {
        Self {
            prepare_error: Some(error),
            ..Self::new()
        }
    }

    fn with_output(output: impl Into<String>) -> Self {
        Self {
            output: output.into(),
            ..Self::new()
        }
    }

    fn prepare_calls(&self) -> usize {
        self.prepare_calls.lock().expect("prepare calls lock").len()
    }

    fn first_prepare_request(&self) -> Option<astra_runtime_env::RunnerPrepareSessionRequest> {
        self.prepare_calls
            .lock()
            .expect("prepare calls lock")
            .first()
            .cloned()
    }

    fn execute_calls(&self) -> usize {
        self.execute_calls.lock().expect("execute calls lock").len()
    }
}

#[async_trait]
impl RunnerRpcTransport for StaticRunnerRpcTransport {
    async fn prepare_session(
        &self,
        _executor_id: &str,
        request: astra_runtime_env::RunnerPrepareSessionRequest,
    ) -> Result<astra_runtime_env::RunnerPrepareSessionResponse, astra_runtime_env::RuntimeError>
    {
        self.prepare_calls
            .lock()
            .expect("prepare calls lock")
            .push(request.clone());
        if let Some(error) = self.prepare_error.clone() {
            return Ok(astra_runtime_env::RunnerPrepareSessionResponse::Rejected { error });
        }
        Ok(astra_runtime_env::RunnerPrepareSessionResponse::Prepared {
            handle: Box::new(astra_runtime_env::RuntimeSessionHandle::from_spec(
                &request.spec,
            )),
        })
    }

    async fn execute_tool(
        &self,
        _executor_id: &str,
        request: astra_runtime_env::RunnerExecuteToolRequest,
    ) -> Result<astra_runtime_env::RunnerExecuteToolResponse, astra_runtime_env::RuntimeError> {
        self.execute_calls
            .lock()
            .expect("execute calls lock")
            .push(request.clone());
        if let Some(error) = self.execute_error.clone() {
            return Ok(astra_runtime_env::RunnerExecuteToolResponse::Rejected { error });
        }
        Ok(astra_runtime_env::RunnerExecuteToolResponse::Completed {
            outcome: astra_runtime_env::RuntimeToolOutcome::completed(
                &request.invocation,
                &self.output,
                &request.session,
            ),
        })
    }
}

struct PendingRunnerRpcTransport {
    prepare_calls: AtomicUsize,
    execute_calls: AtomicUsize,
    execute_started: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

impl PendingRunnerRpcTransport {
    fn new(execute_started: tokio::sync::oneshot::Sender<()>) -> Self {
        Self {
            prepare_calls: AtomicUsize::new(0),
            execute_calls: AtomicUsize::new(0),
            execute_started: Mutex::new(Some(execute_started)),
        }
    }

    fn prepare_calls(&self) -> usize {
        self.prepare_calls.load(Ordering::SeqCst)
    }

    fn execute_calls(&self) -> usize {
        self.execute_calls.load(Ordering::SeqCst)
    }
}

struct PendingPrepareRunnerRpcTransport {
    prepare_calls: AtomicUsize,
    execute_calls: AtomicUsize,
    prepare_started: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

impl PendingPrepareRunnerRpcTransport {
    fn new(prepare_started: tokio::sync::oneshot::Sender<()>) -> Self {
        Self {
            prepare_calls: AtomicUsize::new(0),
            execute_calls: AtomicUsize::new(0),
            prepare_started: Mutex::new(Some(prepare_started)),
        }
    }

    fn prepare_calls(&self) -> usize {
        self.prepare_calls.load(Ordering::SeqCst)
    }

    fn execute_calls(&self) -> usize {
        self.execute_calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl RunnerRpcTransport for PendingPrepareRunnerRpcTransport {
    async fn prepare_session(
        &self,
        _executor_id: &str,
        _request: astra_runtime_env::RunnerPrepareSessionRequest,
    ) -> Result<astra_runtime_env::RunnerPrepareSessionResponse, astra_runtime_env::RuntimeError>
    {
        self.prepare_calls.fetch_add(1, Ordering::SeqCst);
        let sender = self
            .prepare_started
            .lock()
            .expect("prepare started lock")
            .take();
        if let Some(sender) = sender {
            let _ = sender.send(());
        }
        std::future::pending::<()>().await;
        unreachable!("pending runner prepare never completes")
    }

    async fn execute_tool(
        &self,
        _executor_id: &str,
        _request: astra_runtime_env::RunnerExecuteToolRequest,
    ) -> Result<astra_runtime_env::RunnerExecuteToolResponse, astra_runtime_env::RuntimeError> {
        self.execute_calls.fetch_add(1, Ordering::SeqCst);
        unreachable!("execute should not run when prepare is pending")
    }
}

#[async_trait]
impl RunnerRpcTransport for PendingRunnerRpcTransport {
    async fn prepare_session(
        &self,
        _executor_id: &str,
        request: astra_runtime_env::RunnerPrepareSessionRequest,
    ) -> Result<astra_runtime_env::RunnerPrepareSessionResponse, astra_runtime_env::RuntimeError>
    {
        self.prepare_calls.fetch_add(1, Ordering::SeqCst);
        Ok(astra_runtime_env::RunnerPrepareSessionResponse::Prepared {
            handle: Box::new(astra_runtime_env::RuntimeSessionHandle::from_spec(
                &request.spec,
            )),
        })
    }

    async fn execute_tool(
        &self,
        _executor_id: &str,
        _request: astra_runtime_env::RunnerExecuteToolRequest,
    ) -> Result<astra_runtime_env::RunnerExecuteToolResponse, astra_runtime_env::RuntimeError> {
        self.execute_calls.fetch_add(1, Ordering::SeqCst);
        let sender = self
            .execute_started
            .lock()
            .expect("execute started lock")
            .take();
        if let Some(sender) = sender {
            let _ = sender.send(());
        }
        std::future::pending::<()>().await;
        unreachable!("pending runner execute never completes")
    }
}

struct StaticGatewayRelayTransport {
    calls: AtomicUsize,
    output: String,
}

impl StaticGatewayRelayTransport {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            output: "gateway-result".to_string(),
        }
    }

    fn with_output(output: impl Into<String>) -> Self {
        Self {
            output: output.into(),
            ..Self::new()
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl GatewayRelayTransport for StaticGatewayRelayTransport {
    async fn execute_tool(
        &self,
        request: ToolExecutionRequest,
        binding: astra_runtime_env::RunBinding,
    ) -> Result<astra_runtime_env::RuntimeToolOutcome, astra_runtime_env::RuntimeError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(runtime_outcome_for_request(
            &request,
            &binding,
            &self.output,
        ))
    }
}

struct PendingGatewayRelayTransport {
    calls: AtomicUsize,
    execute_started: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

impl PendingGatewayRelayTransport {
    fn new(execute_started: tokio::sync::oneshot::Sender<()>) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            execute_started: Mutex::new(Some(execute_started)),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl GatewayRelayTransport for PendingGatewayRelayTransport {
    async fn execute_tool(
        &self,
        _request: ToolExecutionRequest,
        _binding: astra_runtime_env::RunBinding,
    ) -> Result<astra_runtime_env::RuntimeToolOutcome, astra_runtime_env::RuntimeError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let sender = self
            .execute_started
            .lock()
            .expect("gateway execute started lock")
            .take();
        if let Some(sender) = sender {
            let _ = sender.send(());
        }
        std::future::pending::<()>().await;
        unreachable!("pending gateway execute never completes")
    }
}

struct StaticSandboxResidentAgentTransport {
    calls: AtomicUsize,
}

impl StaticSandboxResidentAgentTransport {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl SandboxResidentAgentTransport for StaticSandboxResidentAgentTransport {
    async fn execute_tool(
        &self,
        request: ToolExecutionRequest,
        binding: astra_runtime_env::RunBinding,
    ) -> Result<astra_runtime_env::RuntimeToolOutcome, astra_runtime_env::RuntimeError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(runtime_outcome_for_request(
            &request,
            &binding,
            "resident-agent-result",
        ))
    }
}

fn runtime_outcome_for_request(
    request: &ToolExecutionRequest,
    binding: &astra_runtime_env::RunBinding,
    output: &str,
) -> astra_runtime_env::RuntimeToolOutcome {
    let spec = astra_runtime_env::RuntimeSessionSpec::new(
        &request.session_id,
        &request.run_id,
        binding.clone(),
    )
    .with_requested_tools([request.tool_name.clone()]);
    let session = astra_runtime_env::RuntimeSessionHandle::from_spec(&spec);
    let invocation = astra_runtime_env::RuntimeToolInvocation::new(
        &request.tool_call_id,
        &request.tool_name,
        request.args.clone(),
        binding.clone(),
        session.policy.revision,
    )
    .with_idempotency_key(format!(
        "{}:{}:{}",
        request.user_id, request.session_id, request.tool_call_id
    ));
    astra_runtime_env::RuntimeToolOutcome::completed(&invocation, output, &session)
}

struct StaticEdgeDispatch {
    inserted_edge_agent_ids: Mutex<Vec<String>>,
    failed_dispatches: Mutex<Vec<(String, String)>>,
    return_result: bool,
}

impl Default for StaticEdgeDispatch {
    fn default() -> Self {
        Self {
            inserted_edge_agent_ids: Mutex::new(Vec::new()),
            failed_dispatches: Mutex::new(Vec::new()),
            return_result: true,
        }
    }
}

impl StaticEdgeDispatch {
    fn no_result() -> Self {
        Self {
            inserted_edge_agent_ids: Mutex::new(Vec::new()),
            failed_dispatches: Mutex::new(Vec::new()),
            return_result: false,
        }
    }
}

#[async_trait]
impl astra_services::multi_agent::EdgeDispatchService for StaticEdgeDispatch {
    async fn insert_dispatch(
        &self,
        _user_id: &str,
        edge_agent_id: &str,
        _request_id: &str,
        _payload_json: &str,
    ) -> Result<i64, String> {
        self.inserted_edge_agent_ids
            .lock()
            .expect("inserted edge agent ids lock")
            .push(edge_agent_id.to_string());
        Ok(1)
    }

    async fn poll_pending(
        &self,
        _user_id: &str,
        _edge_agent_id: &str,
    ) -> Result<Vec<astra_services::multi_agent::EdgeDispatchRow>, String> {
        Ok(Vec::new())
    }

    async fn mark_dispatched(&self, _dispatch_ids: &[i64]) -> Result<(), String> {
        Ok(())
    }

    async fn deliver_result(
        &self,
        _request_id: &str,
        _edge_agent_id: &str,
        _result_json: &str,
    ) -> Result<bool, String> {
        Ok(true)
    }

    async fn fail_dispatch(&self, request_id: &str, reason: &str) -> Result<bool, String> {
        self.failed_dispatches
            .lock()
            .expect("failed dispatches lock")
            .push((request_id.to_string(), reason.to_string()));
        Ok(true)
    }

    async fn wait_result(
        &self,
        request_id: &str,
        _timeout: std::time::Duration,
    ) -> Result<Option<String>, String> {
        if !self.return_result {
            return Ok(None);
        }
        let result = astra_thin_client::ToolResultRequest::new_with_hash(
            request_id.to_string(),
            Some("edge-selected".to_string()),
            "success".to_string(),
            "ledger-result".to_string(),
            12,
        );
        serde_json::to_string(&result)
            .map(Some)
            .map_err(|error| error.to_string())
    }

    async fn cleanup_stale(&self, _older_than: std::time::Duration) -> Result<u64, String> {
        Ok(0)
    }
}

struct PendingEdgeDispatch {
    inserted_edge_agent_ids: Mutex<Vec<String>>,
    failed_dispatches: Mutex<Vec<(String, String)>>,
    wait_started: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

impl PendingEdgeDispatch {
    fn new(wait_started: tokio::sync::oneshot::Sender<()>) -> Self {
        Self {
            inserted_edge_agent_ids: Mutex::new(Vec::new()),
            failed_dispatches: Mutex::new(Vec::new()),
            wait_started: Mutex::new(Some(wait_started)),
        }
    }
}

#[async_trait]
impl astra_services::multi_agent::EdgeDispatchService for PendingEdgeDispatch {
    async fn insert_dispatch(
        &self,
        _user_id: &str,
        edge_agent_id: &str,
        _request_id: &str,
        _payload_json: &str,
    ) -> Result<i64, String> {
        self.inserted_edge_agent_ids
            .lock()
            .expect("inserted edge agent ids lock")
            .push(edge_agent_id.to_string());
        Ok(1)
    }

    async fn poll_pending(
        &self,
        _user_id: &str,
        _edge_agent_id: &str,
    ) -> Result<Vec<astra_services::multi_agent::EdgeDispatchRow>, String> {
        Ok(Vec::new())
    }

    async fn mark_dispatched(&self, _dispatch_ids: &[i64]) -> Result<(), String> {
        Ok(())
    }

    async fn deliver_result(
        &self,
        _request_id: &str,
        _edge_agent_id: &str,
        _result_json: &str,
    ) -> Result<bool, String> {
        Ok(true)
    }

    async fn fail_dispatch(&self, request_id: &str, reason: &str) -> Result<bool, String> {
        self.failed_dispatches
            .lock()
            .expect("failed dispatches lock")
            .push((request_id.to_string(), reason.to_string()));
        Ok(true)
    }

    async fn wait_result(
        &self,
        _request_id: &str,
        _timeout: std::time::Duration,
    ) -> Result<Option<String>, String> {
        let sender = self.wait_started.lock().expect("wait started lock").take();
        if let Some(sender) = sender {
            let _ = sender.send(());
        }
        std::future::pending::<()>().await;
        unreachable!("pending edge dispatch wait never completes")
    }

    async fn cleanup_stale(&self, _older_than: std::time::Duration) -> Result<u64, String> {
        Ok(0)
    }
}

struct StaticEdgeRegistry {
    agents: Vec<astra_services::multi_agent::EdgeAgentRecord>,
}

#[async_trait]
impl astra_services::multi_agent::EdgeRegistryService for StaticEdgeRegistry {
    async fn register_or_update(
        &self,
        _user_id: &str,
        _edge_agent_id: &str,
        _edge_id_header: &str,
        _hostname: Option<&str>,
        _worktree_path: Option<&str>,
        _capabilities: Option<serde_json::Value>,
    ) -> Result<astra_services::multi_agent::EdgeAgentRecord, String> {
        Err("not needed for this test".to_string())
    }

    async fn heartbeat(
        &self,
        _user_id: &str,
        _edge_agent_id: &str,
        _edge_id_header: &str,
    ) -> Result<(), String> {
        Ok(())
    }

    async fn list_by_user(
        &self,
        _user_id: &str,
    ) -> Result<Vec<astra_services::multi_agent::EdgeAgentRecord>, String> {
        Ok(self.agents.clone())
    }

    async fn unregister(&self, _user_id: &str, _edge_agent_id: &str) -> Result<(), String> {
        Ok(())
    }
}

fn edge_agent_record(edge_agent_id: &str) -> astra_services::multi_agent::EdgeAgentRecord {
    astra_services::multi_agent::EdgeAgentRecord {
        registry_id: format!("registry-{edge_agent_id}"),
        user_id: "user-1".to_string(),
        edge_agent_id: edge_agent_id.to_string(),
        edge_id: format!("edge-id-{edge_agent_id}"),
        hostname: Some("MacBook Pro".to_string()),
        worktree_path: Some("/Users/test/project".to_string()),
        capabilities: Some(edge_runtime_environment_advertisement(edge_agent_id)),
        registered_at: "2026-06-11T00:00:00Z".to_string(),
        last_heartbeat_at: "2026-06-11T00:00:00Z".to_string(),
    }
}

fn edge_runtime_environment_advertisement(edge_agent_id: &str) -> Value {
    let registry = astra_runtime_env::ToolRegistry::builtins();
    let binding = astra_runtime_env::RunBinding::resolve(
        astra_runtime_env::WorkspaceBinding::edge_workspace(
            "/Users/test/project",
            astra_runtime_env::WorkspaceAuthority::ReadWrite,
        ),
        astra_runtime_env::ExecutorBinding::edge_agent(edge_agent_id.to_string()),
        astra_runtime_env::RuntimeBinding::host_process(format!("edge-host:{edge_agent_id}")),
        astra_runtime_env::PolicyIntent::local_developer(),
        &registry,
    );
    serde_json::to_value(astra_runtime_env::RuntimeEnvironmentAdvertisement::new(
        binding,
    ))
    .expect("serialize edge runtime environment advertisement")
}

fn request(
    tool_name: &str,
    workspace: WorkspaceBinding,
    executor: ExecutorBinding,
) -> ToolExecutionRequest {
    ToolExecutionRequest {
        user_id: "user-1".to_string(),
        run_id: "run-1".to_string(),
        session_id: "session-1".to_string(),
        tool_call_id: "call-1".to_string(),
        tool_name: tool_name.to_string(),
        args: serde_json::json!({}),
        workspace,
        workspace_record: None,
        executor,
        runtime: None,
        policy: ToolPolicySnapshot::default(),
    }
}

#[test]
fn route_boundary_builds_events_and_attaches_binding_metadata() {
    let service = ToolExecutionService::new_for_test();
    let mut request = request(
        "bash",
        WorkspaceBinding::server_sandbox("/tmp/astra-workspace"),
        ExecutorBinding::server_local(),
    );
    request.args = serde_json::json!({
        "_tool_call_id": "call-1",
        "_run_id": "run-1",
        "command": "pwd"
    });

    let boundary = service.route_boundary(request);

    let routing_event = boundary
        .routing_decision_event()
        .expect("routing decision event");
    assert_eq!(routing_event["type"], "tool_routing_decision");
    assert_eq!(routing_event["call_id"], "call-1");
    assert_eq!(routing_event["run_id"], "run-1");
    assert_eq!(routing_event["route"], "server_local");

    let started_event = boundary
        .transport_started_event()
        .expect("transport started event");
    assert_eq!(started_event["type"], "tool_transport_started");
    assert_eq!(
        started_event["arguments"],
        serde_json::json!({"command": "pwd"})
    );

    let mut result = astra_tools::ToolResult::text("ok".to_string());
    boundary.attach_binding_metadata(&mut result, service.tool_registry());
    let metadata = result.metadata.as_ref().expect("result metadata");
    assert_eq!(metadata["workspace"]["kind"], "server_sandbox");
    assert_eq!(metadata["executor"]["kind"], "server_local");
    assert_eq!(metadata["transport"], "server_local");
    assert!(metadata.get("runtime").is_some());
    assert!(metadata.get("policy").is_some());
    assert!(metadata.get("runtime_environment").is_some());

    let finished_event = boundary
        .transport_finished_event(&result, 17)
        .expect("transport finished event");
    assert_eq!(finished_event["type"], "tool_transport_completed");
    assert_eq!(finished_event["success"], true);
    assert_eq!(finished_event["workspace"]["kind"], "server_sandbox");

    let end_event = boundary
        .tool_call_end_event(&result, 17)
        .expect("tool call end event");
    assert_eq!(end_event["type"], "tool_call_end");
    assert_eq!(end_event["result"], "ok");
    assert_eq!(end_event["executor"]["kind"], "server_local");
}

#[test]
fn route_boundary_events_require_call_id_without_mutating_result_metadata() {
    let service = ToolExecutionService::new_for_test();
    let request = request(
        "bash",
        WorkspaceBinding::server_sandbox("/tmp/astra-workspace"),
        ExecutorBinding::server_local(),
    );
    let boundary = service.route_boundary(request);
    let result = astra_tools::ToolResult::text("ok".to_string());

    assert!(boundary.routing_decision_event().is_none());
    assert!(boundary.transport_started_event().is_none());
    assert!(boundary.transport_finished_event(&result, 1).is_none());
    assert!(boundary.tool_call_end_event(&result, 1).is_none());
    assert!(result.metadata.is_none());
}

#[tokio::test]
async fn boundary_execution_uses_frozen_route_without_recomputing() {
    let service = ToolExecutionService::new_for_test();
    let local = CountingLocalTransport::new();
    let request = request(
        "bash",
        WorkspaceBinding::server_sandbox("/tmp/astra-workspace"),
        ExecutorBinding::server_local(),
    );
    assert_eq!(
        service.routing_decision(&request),
        ToolExecutionRouteKind::ServerLocal
    );
    let boundary = ToolRouteBoundary::new(request, ToolExecutionRouteKind::Unsupported);

    let result = service
        .execute_boundary_with_cancel(&boundary, &local, None)
        .await;

    assert!(result.is_error, "{result:?}");
    assert_eq!(local.calls(), 0);
    let metadata = result.metadata.expect("route mismatch metadata");
    assert_eq!(metadata["error_kind"], TOOL_ERROR_KIND_ROUTE_MISMATCH);
    assert_eq!(metadata["runtime_error"]["kind"], "route_mismatch");
}

#[tokio::test]
async fn server_sandbox_routes_to_server_local_transport() {
    let service = ToolExecutionService::new_for_test();
    let local = CountingLocalTransport::new();
    let result = service
        .execute(
            request(
                "bash",
                WorkspaceBinding::server_sandbox("/tmp/astra-workspace"),
                ExecutorBinding::server_local(),
            ),
            &local,
        )
        .await;

    assert!(!result.is_error, "{result:?}");
    assert_eq!(result.output, "local:bash");
    assert_eq!(local.calls(), 1);
}

#[tokio::test]
async fn no_workspace_local_code_blocks_without_server_fallback() {
    let service = ToolExecutionService::new_for_test();
    let local = CountingLocalTransport::new();
    let result = service
        .execute(
            request(
                "bash",
                WorkspaceBinding {
                    kind: WorkspaceBindingKind::None,
                    display_name: "No workspace".to_string(),
                    cwd: None,
                    authority: WorkspaceAuthority::None,
                    fallback_policy: FallbackPolicy::Disabled,
                },
                ExecutorBinding::server_local(),
            ),
            &local,
        )
        .await;

    assert!(result.is_error, "{result:?}");
    assert!(
        result.output.contains("no fallback was attempted"),
        "{}",
        result.output
    );
    let metadata = result.metadata.expect("capability metadata");
    assert_eq!(metadata["error_kind"], TOOL_ERROR_KIND_CAPABILITY_DENIED);
    assert_eq!(metadata["reason"], TOOL_ERROR_KIND_CAPABILITY_DENIED);
    assert_eq!(metadata["blocked"], true);
    assert_eq!(metadata["retryable"], false);
    assert_eq!(metadata["execution_started"], false);
    assert_eq!(metadata["side_effects_maybe"], false);
    assert_eq!(
        metadata["next_action"],
        "change_workspace_executor_runtime_or_policy"
    );
    assert_eq!(metadata["runtime_error"]["kind"], "capability_denied");
    assert_eq!(metadata["workspace"]["kind"], "none");
    assert_eq!(
        metadata["capability_denial"],
        serde_json::json!({"ExecutorUnavailable": "runtime_executor_required"})
    );
    assert_eq!(metadata["runtime"]["session_manager"], "none");
    assert_eq!(metadata["runtime"]["isolation_backend"], "none");
    assert_eq!(metadata["policy"]["revision"], 1);
    assert_eq!(metadata["policy"]["intent"]["filesystem"], "no_access");
    assert_eq!(local.calls(), 0);
}

#[tokio::test]
async fn unknown_tool_is_denied_before_local_transport() {
    let service = ToolExecutionService::new_for_test();
    let local = CountingLocalTransport::new();
    let result = service
        .execute(
            request(
                "not_a_tool",
                WorkspaceBinding::server_sandbox("/tmp/astra-workspace"),
                ExecutorBinding::server_local(),
            ),
            &local,
        )
        .await;

    assert!(result.is_error, "{result:?}");
    let metadata = result.metadata.expect("capability metadata");
    assert_eq!(metadata["error_kind"], TOOL_ERROR_KIND_CAPABILITY_DENIED);
    assert_eq!(
        metadata["capability_denial"],
        serde_json::json!("UnknownTool")
    );
    assert_eq!(local.calls(), 0);
}

#[tokio::test]
async fn policy_allowed_tools_blocks_disallowed_tool_before_local_transport() {
    let service = ToolExecutionService::new_for_test();
    let local = CountingLocalTransport::new();
    let mut request = request(
        "bash",
        WorkspaceBinding::server_sandbox("/tmp/astra-workspace"),
        ExecutorBinding::server_local(),
    );
    request.policy.allowed_tools = vec!["read_file".to_string()];

    let binding = request.runtime_environment_binding(service.tool_registry());
    assert!(!binding.tool_surface.contains("bash"));
    assert_eq!(
        binding.tool_surface.denial_for("bash"),
        Some(&astra_runtime_env::ToolUnavailableReason::PolicyDenied(
            astra_runtime_env::PolicyIntent::disallowed_tool_reason("bash")
        ))
    );

    let result = service.execute(request, &local).await;

    assert!(result.is_error, "{result:?}");
    let metadata = result.metadata.expect("policy denial metadata");
    assert_eq!(metadata["error_kind"], TOOL_ERROR_KIND_CAPABILITY_DENIED);
    assert_eq!(
        metadata["capability_denial"],
        serde_json::json!({"PolicyDenied": "tool 'bash' is not in allowed_tools"})
    );
    assert_eq!(metadata["execution_started"], false);
    assert_eq!(metadata["side_effects_maybe"], false);
    assert_eq!(local.calls(), 0);
}

#[test]
fn no_workspace_binding_resolves_to_control_plane_tool_surface_only() {
    let registry = astra_runtime_env::ToolRegistry::builtins();
    let request = request(
        "bash",
        WorkspaceBinding {
            kind: WorkspaceBindingKind::None,
            display_name: "No workspace".to_string(),
            cwd: None,
            authority: WorkspaceAuthority::None,
            fallback_policy: FallbackPolicy::Disabled,
        },
        ExecutorBinding::server_local(),
    );

    let binding = request.runtime_environment_binding(&registry);

    assert!(binding.tool_surface.contains("ask_user"));
    assert!(binding.tool_surface.contains("tool_search"));
    for tool in [
        "bash",
        "read_file",
        "write_file",
        "git",
        "git_clone",
        "find_definition",
    ] {
        assert!(
            !binding.tool_surface.contains(tool),
            "{tool} should be hidden"
        );
    }
}

#[test]
fn server_sandbox_binding_reports_host_process_runtime_not_provider_runtime() {
    let registry = astra_runtime_env::ToolRegistry::builtins();
    let request = request(
        "bash",
        WorkspaceBinding::server_sandbox("/tmp/astra-workspace"),
        ExecutorBinding::server_local(),
    );

    let binding = request.runtime_environment_binding(&registry);

    assert_eq!(
        binding.runtime.session_manager,
        astra_runtime_env::RuntimeSessionManager::HostProcess
    );
    assert_eq!(
        binding.runtime.isolation_backend,
        astra_runtime_env::RuntimeIsolationBackend::HostProcess
    );
    assert_eq!(
        binding.runtime.launch_driver,
        astra_runtime_env::RuntimeLaunchDriver::InProcess
    );
    assert_ne!(
        binding.runtime.isolation_backend,
        astra_runtime_env::RuntimeIsolationBackend::GVisorRunsc
    );
    assert_ne!(
        binding.runtime.launch_driver,
        astra_runtime_env::RuntimeLaunchDriver::Kubernetes
    );
    assert!(binding.tool_surface.contains("bash"));
    assert!(binding.tool_surface.contains("read_file"));
}

#[tokio::test]
async fn no_workspace_mcp_retrieve_runs_as_request_scoped_mcp_without_runtime() {
    let service = ToolExecutionService::new_for_test();
    let local = CountingLocalTransport::new();
    let request = request(
        "mcp__rag__retrieve",
        WorkspaceBinding {
            kind: WorkspaceBindingKind::None,
            display_name: "No workspace".to_string(),
            cwd: None,
            authority: WorkspaceAuthority::None,
            fallback_policy: FallbackPolicy::Disabled,
        },
        ExecutorBinding::server_local(),
    );
    let registry = astra_runtime_env::ToolRegistry::builtins();
    let binding = request.runtime_environment_binding(&registry);

    assert_eq!(
        service.routing_decision(&request),
        ToolExecutionRouteKind::RequestScopedMcp
    );
    assert_eq!(
        binding.executor.kind,
        astra_runtime_env::ExecutorBindingKind::RequestScopedMcp
    );
    assert_eq!(
        binding.runtime.session_manager,
        astra_runtime_env::RuntimeSessionManager::None
    );
    assert_eq!(
        binding.runtime.isolation_backend,
        astra_runtime_env::RuntimeIsolationBackend::None
    );
    assert_eq!(
        astra_runtime_env::CapabilityResolver.check_tool_call(
            &registry,
            "mcp__rag__retrieve",
            &serde_json::json!({"query": "what is astra?"}),
            &binding.capabilities,
        ),
        Ok(())
    );

    let result = service.execute(request, &local).await;

    assert!(!result.is_error, "{result:?}");
    assert_eq!(result.output, "local:mcp__rag__retrieve");
    assert_eq!(local.calls(), 1);
    let metadata = result.metadata.expect("mcp metadata");
    assert_eq!(metadata["workspace"]["kind"], "none");
    assert_eq!(metadata["executor"]["kind"], "mcp");
    assert_eq!(metadata["transport"], "mcp_http");
}

#[test]
fn edge_workspace_binding_resolves_project_tools_to_edge_runtime() {
    let registry = astra_runtime_env::ToolRegistry::builtins();
    let request = request(
        "bash",
        WorkspaceBinding::edge_workspace(
            "MacBook Pro",
            "/Users/test/project",
            WorkspaceAuthority::ReadWrite,
        ),
        ExecutorBinding::edge_agent(
            "edge-1",
            "MacBook Pro",
            ToolTransportKind::EdgeWs,
            ExecutorStatus::Online,
        ),
    );

    let binding = request.runtime_environment_binding(&registry);

    assert!(binding.tool_surface.contains("bash"));
    assert!(binding.tool_surface.contains("read_file"));
    assert!(binding.tool_surface.contains("write_file"));
    assert!(binding.tool_surface.contains("git"));
}

#[test]
fn edge_bound_execution_plan_builds_dispatch_payload_and_delivery_metadata() {
    let mut request = request(
        "bash",
        WorkspaceBinding::edge_workspace(
            "MacBook Pro",
            "/Users/test/project",
            WorkspaceAuthority::ReadWrite,
        ),
        ExecutorBinding::edge_agent(
            "edge-1",
            "MacBook Pro",
            ToolTransportKind::EdgeWs,
            ExecutorStatus::Online,
        ),
    );
    request.args = serde_json::json!({
        "_tool_call_id": "call-edge",
        "command": "pwd"
    });

    let plan = EdgeBoundExecutionPlan::from_request_with_dispatch_id(&request, "edge-dispatch-1");

    assert_eq!(plan.selected_executor_id(), Some("edge-1"));
    assert_eq!(plan.dispatch_request_id(), "edge-dispatch-1");
    assert_eq!(plan.wait_timeout(), std::time::Duration::from_secs(310));

    let payload: Value =
        serde_json::from_str(&plan.dispatch_payload_json().expect("dispatch payload"))
            .expect("payload json");
    assert_eq!(payload["type"], "edge_tool_request");
    assert_eq!(payload["request_id"], "edge-dispatch-1");
    assert_eq!(payload["tool"], "bash");
    assert_eq!(payload["timeout_secs"], 300);
    assert_eq!(payload["args"]["command"], "pwd");

    let result = plan.delivered_result("ok".to_string(), false, ToolTransportKind::EdgeLedger);
    assert!(!result.is_error);
    assert_eq!(result.output, "ok");
    let metadata = result.metadata.expect("delivery metadata");
    assert_eq!(metadata["workspace"]["kind"], "edge_workspace");
    assert_eq!(metadata["executor"]["kind"], "edge_agent");
    assert_eq!(metadata["executor"]["transport"], "edge_ledger");
    assert_eq!(metadata["transport"], "edge_ledger");
}

#[test]
fn edge_bound_execution_plan_uses_policy_timeout_from_binding() {
    let registry = astra_runtime_env::ToolRegistry::builtins();
    let mut request = request(
        "read_file",
        WorkspaceBinding::edge_workspace(
            "MacBook Pro",
            "/Users/test/project",
            WorkspaceAuthority::ReadOnly,
        ),
        ExecutorBinding::edge_agent(
            "edge-1",
            "MacBook Pro",
            ToolTransportKind::EdgeWs,
            ExecutorStatus::Online,
        ),
    );
    request.args = serde_json::json!({
        "_tool_call_id": "call-edge-read",
        "path": "README.md"
    });
    let binding = request.runtime_environment_binding(&registry);

    let plan = EdgeBoundExecutionPlan::from_request_with_binding_and_dispatch_id(
        &request,
        &binding,
        "edge-dispatch-read",
    );

    assert_eq!(plan.wait_timeout(), std::time::Duration::from_secs(40));
    let payload: Value =
        serde_json::from_str(&plan.dispatch_payload_json().expect("dispatch payload"))
            .expect("payload json");
    assert_eq!(payload["request_id"], "edge-dispatch-read");
    assert_eq!(payload["tool"], "read_file");
    assert_eq!(payload["timeout_secs"], 30);
}

#[test]
fn edge_bound_execution_plan_uses_policy_snapshot_timeout_override() {
    let registry = astra_runtime_env::ToolRegistry::builtins();
    let mut request = request(
        "read_file",
        WorkspaceBinding::edge_workspace(
            "MacBook Pro",
            "/Users/test/project",
            WorkspaceAuthority::ReadOnly,
        ),
        ExecutorBinding::edge_agent(
            "edge-1",
            "MacBook Pro",
            ToolTransportKind::EdgeWs,
            ExecutorStatus::Online,
        ),
    );
    request.policy.max_execution_secs = Some(7.2);
    let binding = request.runtime_environment_binding(&registry);

    let plan = EdgeBoundExecutionPlan::from_request_with_binding_and_dispatch_id(
        &request,
        &binding,
        "edge-dispatch-short",
    );

    assert_eq!(plan.wait_timeout(), std::time::Duration::from_secs(18));
    let payload: Value =
        serde_json::from_str(&plan.dispatch_payload_json().expect("dispatch payload"))
            .expect("payload json");
    assert_eq!(payload["timeout_secs"], 8);
    assert_eq!(
        binding.policy.resources.max_execution_secs,
        Some(7.2),
        "policy snapshot should override default read-only timeout"
    );
}

#[test]
fn offline_edge_binding_hides_project_tools_even_with_workspace_metadata() {
    let registry = astra_runtime_env::ToolRegistry::builtins();
    let request = request(
        "bash",
        WorkspaceBinding::edge_workspace(
            "MacBook Pro",
            "/Users/test/project",
            WorkspaceAuthority::ReadWrite,
        ),
        ExecutorBinding::edge_agent(
            "edge-1",
            "MacBook Pro",
            ToolTransportKind::EdgeWs,
            ExecutorStatus::Offline,
        ),
    );

    let binding = request.runtime_environment_binding(&registry);

    assert!(!binding.tool_surface.contains("bash"));
    assert_eq!(
        binding.tool_surface.denial_for("bash"),
        Some(
            &astra_runtime_env::ToolUnavailableReason::ExecutorUnavailable(
                "runtime_executor_required".to_string()
            )
        )
    );
}

#[test]
fn hosted_runner_unknown_status_hides_project_tools_until_runtime_ready() {
    let registry = astra_runtime_env::ToolRegistry::builtins();
    let request = request(
        "read_file",
        WorkspaceBinding {
            kind: WorkspaceBindingKind::CloudWorkspace,
            display_name: "Snapshot".to_string(),
            cwd: Some("/snapshot".to_string()),
            authority: WorkspaceAuthority::ReadOnly,
            fallback_policy: FallbackPolicy::Disabled,
        },
        ExecutorBinding {
            kind: ExecutorBindingKind::HostedRunner,
            executor_id: "snapshot-runner".to_string(),
            display_name: "Snapshot runner".to_string(),
            transport: ToolTransportKind::RunnerRpc,
            status: ExecutorStatus::Unknown,
        },
    );

    let binding = request.runtime_environment_binding(&registry);

    assert_eq!(
        binding.runtime.session_manager,
        astra_runtime_env::RuntimeSessionManager::None
    );
    assert_eq!(
        binding.runtime.isolation_backend,
        astra_runtime_env::RuntimeIsolationBackend::None
    );
    assert!(!binding.tool_surface.contains("read_file"));
    assert_eq!(
        binding.tool_surface.denial_for("read_file"),
        Some(
            &astra_runtime_env::ToolUnavailableReason::ExecutorUnavailable(
                "runtime_executor_required".to_string()
            )
        )
    );
}

#[test]
fn hosted_runner_online_without_runtime_hides_project_tools() {
    let registry = astra_runtime_env::ToolRegistry::builtins();
    let request = request(
        "read_file",
        WorkspaceBinding::cloud_workspace("/workspace/project", WorkspaceAuthority::ReadWrite),
        ExecutorBinding {
            kind: ExecutorBindingKind::HostedRunner,
            executor_id: "snapshot-runner".to_string(),
            display_name: "Snapshot runner".to_string(),
            transport: ToolTransportKind::RunnerRpc,
            status: ExecutorStatus::Online,
        },
    );

    let binding = request.runtime_environment_binding(&registry);

    assert_eq!(
        binding.runtime.session_manager,
        astra_runtime_env::RuntimeSessionManager::None
    );
    assert_eq!(
        binding.runtime.isolation_backend,
        astra_runtime_env::RuntimeIsolationBackend::None
    );
    for tool in [
        "read_file",
        "list_dir",
        "grep",
        "glob",
        "write_file",
        "str_replace",
        "bash",
        "run_script",
        "background_shell",
        "git",
        "git_clone",
        "lsp",
    ] {
        assert!(
            !binding.tool_surface.contains(tool),
            "{tool} must stay hidden without a ready runtime"
        );
        assert!(
            binding.tool_surface.denial_for(tool).is_some(),
            "{tool} must carry an explicit denial reason"
        );
    }
    assert_eq!(
        binding.tool_surface.denial_for("read_file"),
        Some(
            &astra_runtime_env::ToolUnavailableReason::RuntimeCapabilityMissing(
                "process".to_string()
            )
        )
    );
    assert_eq!(
        binding.tool_surface.denial_for("bash"),
        Some(
            &astra_runtime_env::ToolUnavailableReason::RuntimeCapabilityMissing(
                "process".to_string()
            )
        )
    );
}

#[tokio::test]
async fn hosted_runner_without_runtime_blocks_stale_project_tool_call() {
    let service = ToolExecutionService::new_for_test();
    let local = CountingLocalTransport::new();
    let result = service
        .execute(
            request(
                "bash",
                WorkspaceBinding::cloud_workspace(
                    "/workspace/project",
                    WorkspaceAuthority::ReadWrite,
                ),
                ExecutorBinding {
                    kind: ExecutorBindingKind::HostedRunner,
                    executor_id: "snapshot-runner".to_string(),
                    display_name: "Snapshot runner".to_string(),
                    transport: ToolTransportKind::RunnerRpc,
                    status: ExecutorStatus::Online,
                },
            ),
            &local,
        )
        .await;

    assert!(result.is_error, "{result:?}");
    assert_eq!(
        local.calls(),
        0,
        "stale project calls must not fall back locally"
    );
    let metadata = result.metadata.expect("capability metadata");
    assert_eq!(metadata["error_kind"], TOOL_ERROR_KIND_CAPABILITY_DENIED);
    assert_eq!(metadata["reason"], TOOL_ERROR_KIND_CAPABILITY_DENIED);
    assert_eq!(metadata["blocked"], true);
    assert_eq!(metadata["execution_started"], false);
    assert_eq!(metadata["runtime_error"]["kind"], "capability_denied");
    assert_eq!(
        metadata["runtime_error"]["message"],
        "tool 'bash' is denied by this run binding: runtime capability is missing: process"
    );
}

#[test]
fn personal_runner_with_ready_runtime_routes_through_runner_rpc() {
    let registry = astra_runtime_env::ToolRegistry::builtins();
    let mut request = request(
        "read_file",
        WorkspaceBinding {
            kind: WorkspaceBindingKind::CloudWorkspace,
            display_name: "Personal workspace".to_string(),
            cwd: Some("/workspace/personal".to_string()),
            authority: WorkspaceAuthority::ReadOnly,
            fallback_policy: FallbackPolicy::Disabled,
        },
        ExecutorBinding {
            kind: ExecutorBindingKind::PersonalRunner,
            executor_id: "personal-runner-1".to_string(),
            display_name: "Personal runner".to_string(),
            transport: ToolTransportKind::RunnerRpc,
            status: ExecutorStatus::Online,
        },
    );
    request.runtime = Some(astra_runtime_env::RuntimeBinding::gvisor(
        "personal-runtime",
    ));

    let binding = request.runtime_environment_binding(&registry);

    assert_eq!(
        binding.executor.kind,
        astra_runtime_env::ExecutorBindingKind::PersonalRunner
    );
    assert!(binding.tool_surface.contains("read_file"));
    assert_eq!(
        ToolExecutionService::new_for_test().routing_decision(&request),
        ToolExecutionRouteKind::RunnerRpc
    );
}

#[test]
fn enterprise_runner_with_ready_runtime_preserves_executor_kind() {
    let registry = astra_runtime_env::ToolRegistry::builtins();
    let mut request = request(
        "bash",
        WorkspaceBinding {
            kind: WorkspaceBindingKind::CloudWorkspace,
            display_name: "Team workspace".to_string(),
            cwd: Some("/workspace/team".to_string()),
            authority: WorkspaceAuthority::ReadWrite,
            fallback_policy: FallbackPolicy::Disabled,
        },
        ExecutorBinding {
            kind: ExecutorBindingKind::EnterpriseRunner,
            executor_id: "enterprise-runner-1".to_string(),
            display_name: "Enterprise runner".to_string(),
            transport: ToolTransportKind::RunnerRpc,
            status: ExecutorStatus::Online,
        },
    );
    request.runtime = Some(astra_runtime_env::RuntimeBinding::gvisor(
        "enterprise-runtime",
    ));

    let binding = request.runtime_environment_binding(&registry);

    assert_eq!(
        binding.executor.kind,
        astra_runtime_env::ExecutorBindingKind::EnterpriseRunner
    );
    assert!(binding.tool_surface.contains("bash"));
    assert_eq!(
        ToolExecutionService::new_for_test().routing_decision(&request),
        ToolExecutionRouteKind::RunnerRpc
    );
}

#[test]
fn cloud_workspace_with_runtime_bound_hosted_runner_exposes_read_write_project_tools() {
    let registry = astra_runtime_env::ToolRegistry::builtins();
    let mut request = request(
        "bash",
        WorkspaceBinding {
            kind: WorkspaceBindingKind::CloudWorkspace,
            display_name: "Team workspace".to_string(),
            cwd: Some("/cloud/volumes/team-volume-1".to_string()),
            authority: WorkspaceAuthority::ReadWrite,
            fallback_policy: FallbackPolicy::Disabled,
        },
        ExecutorBinding {
            kind: ExecutorBindingKind::HostedRunner,
            executor_id: "runner-1".to_string(),
            display_name: "Hosted runner".to_string(),
            transport: ToolTransportKind::RunnerRpc,
            status: ExecutorStatus::Online,
        },
    );
    request.runtime = Some(astra_runtime_env::RuntimeBinding::oci_container(
        "runner-runtime",
    ));

    let binding = request.runtime_environment_binding(&registry);

    assert_eq!(
        binding.workspace.kind,
        astra_runtime_env::WorkspaceBindingKind::CloudWorkspace
    );
    assert_eq!(
        binding.runtime.session_manager,
        astra_runtime_env::RuntimeSessionManager::AstraManaged
    );
    assert_eq!(
        binding.runtime.isolation_backend,
        astra_runtime_env::RuntimeIsolationBackend::OciRuntime
    );
    assert!(binding.tool_surface.contains("bash"));
    assert!(binding.tool_surface.contains("read_file"));
    assert!(binding.tool_surface.contains("write_file"));
    assert!(binding.tool_surface.contains("git"));
}

#[test]
fn explicit_runtime_binding_overrides_executor_inference() {
    let registry = astra_runtime_env::ToolRegistry::builtins();
    let mut request = request(
        "bash",
        WorkspaceBinding {
            kind: WorkspaceBindingKind::CloudWorkspace,
            display_name: "OpenShell workspace".to_string(),
            cwd: Some("/sandbox".to_string()),
            authority: WorkspaceAuthority::ReadWrite,
            fallback_policy: FallbackPolicy::Disabled,
        },
        ExecutorBinding {
            kind: ExecutorBindingKind::HostedRunner,
            executor_id: "openshell-gateway".to_string(),
            display_name: "OpenShell Gateway".to_string(),
            transport: ToolTransportKind::GatewayRelay,
            status: ExecutorStatus::Online,
        },
    );
    request.runtime = Some(astra_runtime_env::RuntimeBinding::nvidia_openshell(
        "openshell-runtime",
    ));

    let binding = request.runtime_environment_binding(&registry);

    assert_eq!(
        binding.runtime.session_manager,
        astra_runtime_env::RuntimeSessionManager::NvidiaOpenShell
    );
    assert_eq!(
        binding.runtime.launch_driver,
        astra_runtime_env::RuntimeLaunchDriver::OpenShellGateway
    );
    assert_eq!(
        binding.executor.transport,
        astra_runtime_env::ToolTransportKind::GatewayRelay
    );
    assert!(binding.tool_surface.contains("bash"));
}

#[tokio::test]
async fn gateway_relay_transport_fails_closed_until_adapter_is_configured() {
    let service = ToolExecutionService::new_for_test();
    let local = CountingLocalTransport::new();
    let mut request = request(
        "bash",
        WorkspaceBinding {
            kind: WorkspaceBindingKind::CloudWorkspace,
            display_name: "OpenShell workspace".to_string(),
            cwd: Some("/sandbox".to_string()),
            authority: WorkspaceAuthority::ReadWrite,
            fallback_policy: FallbackPolicy::Disabled,
        },
        ExecutorBinding {
            kind: ExecutorBindingKind::HostedRunner,
            executor_id: "openshell-gateway".to_string(),
            display_name: "OpenShell Gateway".to_string(),
            transport: ToolTransportKind::GatewayRelay,
            status: ExecutorStatus::Online,
        },
    );
    request.runtime = Some(astra_runtime_env::RuntimeBinding::nvidia_openshell(
        "openshell-runtime",
    ));
    request.args = serde_json::json!({"_tool_call_id": "call-gateway"});

    assert_eq!(
        service.routing_decision(&request),
        ToolExecutionRouteKind::GatewayRelay
    );
    let route_event = service
        .route_boundary(request.clone())
        .routing_decision_event()
        .expect("routing decision event");
    assert_eq!(route_event["route"], "gateway_relay");
    assert_eq!(route_event["transport"], "gateway_relay");
    assert_eq!(route_event["executor"]["transport"], "gateway_relay");

    let result = service.execute(request, &local).await;

    assert!(result.is_error, "{result:?}");
    assert_eq!(local.calls(), 0);
    assert!(result.output.contains("gateway relay transport adapter"));
    let metadata = result.metadata.expect("transport metadata");
    assert_eq!(metadata["error_kind"], "transport_unavailable");
    assert_eq!(metadata["runtime_error"]["kind"], "transport_unavailable");
    assert_eq!(metadata["executor"]["transport"], "gateway_relay");
    assert_eq!(metadata["transport"], "gateway_relay");
    assert_eq!(metadata["runtime"]["session_manager"], "nvidia_open_shell");
    assert_eq!(metadata["runtime"]["launch_driver"], "open_shell_gateway");
    assert_eq!(metadata["policy"]["revision"], 1);
    assert_eq!(metadata["next_action"], "reconnect_runner");
    assert_eq!(
        metadata["runtime_environment"]["runtime"]["runtime_id"],
        "openshell-runtime"
    );
}

#[tokio::test]
async fn gateway_relay_executes_through_configured_transport() {
    let gateway = Arc::new(StaticGatewayRelayTransport::new());
    let service = ToolExecutionService::builder()
        .gateway_relay_transport(gateway.clone())
        .build();
    let local = CountingLocalTransport::new();
    let mut request = request(
        "bash",
        WorkspaceBinding {
            kind: WorkspaceBindingKind::CloudWorkspace,
            display_name: "OpenShell workspace".to_string(),
            cwd: Some("/sandbox".to_string()),
            authority: WorkspaceAuthority::ReadWrite,
            fallback_policy: FallbackPolicy::Disabled,
        },
        ExecutorBinding {
            kind: ExecutorBindingKind::HostedRunner,
            executor_id: "openshell-gateway".to_string(),
            display_name: "OpenShell Gateway".to_string(),
            transport: ToolTransportKind::GatewayRelay,
            status: ExecutorStatus::Online,
        },
    );
    request.runtime = Some(astra_runtime_env::RuntimeBinding::nvidia_openshell(
        "openshell-runtime",
    ));

    let result = service.execute(request, &local).await;

    assert!(!result.is_error, "{result:?}");
    assert_eq!(result.output, "gateway-result");
    assert_eq!(local.calls(), 0);
    assert_eq!(gateway.calls(), 1);
    let metadata = result.metadata.expect("gateway metadata");
    assert_eq!(metadata["transport"], "gateway_relay");
    assert_eq!(metadata["executor"]["transport"], "gateway_relay");
    assert_eq!(metadata["runtime"]["session_manager"], "nvidia_open_shell");
    assert_eq!(metadata["runtime"]["launch_driver"], "open_shell_gateway");
}

#[tokio::test]
async fn gateway_relay_oversized_output_is_blocked_at_transport_boundary() {
    let oversized = "x".repeat(1_048_577);
    let gateway = Arc::new(StaticGatewayRelayTransport::with_output(oversized));
    let service = ToolExecutionService::builder()
        .gateway_relay_transport(gateway.clone())
        .build();
    let local = CountingLocalTransport::new();
    let mut request = request(
        "read_file",
        WorkspaceBinding::cloud_workspace("/snapshot", WorkspaceAuthority::ReadOnly),
        ExecutorBinding {
            kind: ExecutorBindingKind::HostedRunner,
            executor_id: "openshell-gateway-1".to_string(),
            display_name: "OpenShell gateway".to_string(),
            transport: ToolTransportKind::GatewayRelay,
            status: ExecutorStatus::Online,
        },
    );
    request.runtime = Some(astra_runtime_env::RuntimeBinding::nvidia_openshell(
        "openshell-runtime",
    ));

    let result = service.execute(request, &local).await;

    assert!(result.is_error, "{result:?}");
    assert!(result.output.contains("output limit exceeded"));
    assert!(!result.output.contains(&"x".repeat(128)));
    assert_eq!(local.calls(), 0);
    assert_eq!(gateway.calls(), 1);
    let metadata = result.metadata.expect("output limit metadata");
    assert_eq!(metadata["error_kind"], "output_limit_exceeded");
    assert_eq!(metadata["execution_started"], true);
    assert_eq!(metadata["side_effects_maybe"], true);
    assert_eq!(metadata["output_bytes"], 1_048_577);
    assert_eq!(metadata["max_output_bytes"], 1_048_576);
    assert_eq!(metadata["transport"], "gateway_relay");
}

#[tokio::test(start_paused = true)]
async fn gateway_relay_timeout_reports_side_effect_uncertainty() {
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let gateway = Arc::new(PendingGatewayRelayTransport::new(started_tx));
    let service = ToolExecutionService::builder()
        .gateway_relay_transport(gateway.clone())
        .build();
    let local = CountingLocalTransport::new();
    let mut request = request(
        "read_file",
        WorkspaceBinding::cloud_workspace("/snapshot", WorkspaceAuthority::ReadOnly),
        ExecutorBinding {
            kind: ExecutorBindingKind::HostedRunner,
            executor_id: "openshell-gateway-1".to_string(),
            display_name: "OpenShell gateway".to_string(),
            transport: ToolTransportKind::GatewayRelay,
            status: ExecutorStatus::Online,
        },
    );
    request.runtime = Some(astra_runtime_env::RuntimeBinding::nvidia_openshell(
        "openshell-runtime",
    ));

    let handle = tokio::spawn(async move { service.execute(request, &local).await });

    started_rx
        .await
        .expect("gateway relay execute should start");
    tokio::time::advance(std::time::Duration::from_secs(31)).await;
    let result = handle
        .await
        .expect("gateway relay timeout task should not panic");

    assert!(result.is_error, "{result:?}");
    assert!(
        result.output.contains("max_execution_secs"),
        "{}",
        result.output
    );
    assert_eq!(gateway.calls(), 1);
    let metadata = result.metadata.expect("gateway timeout metadata");
    assert_eq!(metadata["error_kind"], TOOL_ERROR_KIND_TOOL_TIMEOUT);
    assert_eq!(metadata["reason"], TOOL_ERROR_KIND_TOOL_TIMEOUT);
    assert_eq!(metadata["blocked"], true);
    assert_eq!(metadata["execution_started"], true);
    assert_eq!(metadata["side_effects_maybe"], true);
    assert_eq!(metadata["next_action"], "inspect_effects_before_retry");
    assert_eq!(metadata["max_execution_secs"], 30.0);
    assert_eq!(metadata["transport"], "gateway_relay");
    assert_eq!(metadata["runtime"]["session_manager"], "nvidia_open_shell");
    assert_eq!(metadata["runtime_error"]["kind"], "tool_timeout");
}

#[tokio::test]
async fn sandbox_resident_agent_transport_fails_closed_until_adapter_is_configured() {
    let service = ToolExecutionService::new_for_test();
    let local = CountingLocalTransport::new();
    let mut request = request(
        "bash",
        WorkspaceBinding {
            kind: WorkspaceBindingKind::CloudWorkspace,
            display_name: "OpenShell workspace".to_string(),
            cwd: Some("/sandbox".to_string()),
            authority: WorkspaceAuthority::ReadWrite,
            fallback_policy: FallbackPolicy::Disabled,
        },
        ExecutorBinding {
            kind: ExecutorBindingKind::HostedRunner,
            executor_id: "openshell-agent".to_string(),
            display_name: "OpenShell resident agent".to_string(),
            transport: ToolTransportKind::SandboxResidentAgent,
            status: ExecutorStatus::Online,
        },
    );
    request.runtime = Some(astra_runtime_env::RuntimeBinding::nvidia_openshell(
        "openshell-runtime",
    ));
    request.args = serde_json::json!({"_tool_call_id": "call-resident-agent"});

    assert_eq!(
        service.routing_decision(&request),
        ToolExecutionRouteKind::SandboxResidentAgent
    );
    let route_event = service
        .route_boundary(request.clone())
        .routing_decision_event()
        .expect("routing decision event");
    assert_eq!(route_event["route"], "sandbox_resident_agent");
    assert_eq!(route_event["transport"], "sandbox_resident_agent");
    assert_eq!(
        route_event["executor"]["transport"],
        "sandbox_resident_agent"
    );

    let result = service.execute(request, &local).await;

    assert!(result.is_error, "{result:?}");
    assert_eq!(local.calls(), 0);
    assert!(
        result
            .output
            .contains("sandbox resident agent transport adapter")
    );
    let metadata = result.metadata.expect("transport metadata");
    assert_eq!(metadata["error_kind"], "transport_unavailable");
    assert_eq!(metadata["runtime_error"]["kind"], "transport_unavailable");
    assert_eq!(metadata["executor"]["transport"], "sandbox_resident_agent");
    assert_eq!(metadata["transport"], "sandbox_resident_agent");
    assert_eq!(metadata["runtime"]["session_manager"], "nvidia_open_shell");
    assert_eq!(metadata["runtime"]["launch_driver"], "open_shell_gateway");
    assert_eq!(metadata["policy"]["revision"], 1);
    assert_eq!(metadata["next_action"], "reconnect_runner");
}

#[tokio::test]
async fn sandbox_resident_agent_executes_through_configured_transport() {
    let resident = Arc::new(StaticSandboxResidentAgentTransport::new());
    let service = ToolExecutionService::builder()
        .sandbox_resident_agent_transport(resident.clone())
        .build();
    let local = CountingLocalTransport::new();
    let mut request = request(
        "bash",
        WorkspaceBinding {
            kind: WorkspaceBindingKind::CloudWorkspace,
            display_name: "OpenShell workspace".to_string(),
            cwd: Some("/sandbox".to_string()),
            authority: WorkspaceAuthority::ReadWrite,
            fallback_policy: FallbackPolicy::Disabled,
        },
        ExecutorBinding {
            kind: ExecutorBindingKind::HostedRunner,
            executor_id: "openshell-agent".to_string(),
            display_name: "OpenShell resident agent".to_string(),
            transport: ToolTransportKind::SandboxResidentAgent,
            status: ExecutorStatus::Online,
        },
    );
    request.runtime = Some(astra_runtime_env::RuntimeBinding::nvidia_openshell(
        "openshell-runtime",
    ));

    let result = service.execute(request, &local).await;

    assert!(!result.is_error, "{result:?}");
    assert_eq!(result.output, "resident-agent-result");
    assert_eq!(local.calls(), 0);
    assert_eq!(resident.calls(), 1);
    let metadata = result.metadata.expect("resident agent metadata");
    assert_eq!(metadata["transport"], "sandbox_resident_agent");
    assert_eq!(metadata["executor"]["transport"], "sandbox_resident_agent");
    assert_eq!(metadata["runtime"]["session_manager"], "nvidia_open_shell");
    assert_eq!(metadata["runtime"]["launch_driver"], "open_shell_gateway");
}

fn hosted_snapshot_request(tool_name: &str) -> ToolExecutionRequest {
    let mut request = request(
        tool_name,
        WorkspaceBinding {
            kind: WorkspaceBindingKind::CloudWorkspace,
            display_name: "Snapshot".to_string(),
            cwd: Some("/snapshot".to_string()),
            authority: WorkspaceAuthority::ReadOnly,
            fallback_policy: FallbackPolicy::Disabled,
        },
        ExecutorBinding {
            kind: ExecutorBindingKind::HostedRunner,
            executor_id: "snapshot-runner".to_string(),
            display_name: "Snapshot runner".to_string(),
            transport: ToolTransportKind::RunnerRpc,
            status: ExecutorStatus::Online,
        },
    );
    request.workspace_record = Some(hosted_snapshot_workspace_record());
    request.runtime = Some(astra_runtime_env::RuntimeBinding::oci_container(
        "snapshot-runtime",
    ));
    request
}

fn openshell_gateway_request(tool_name: &str) -> ToolExecutionRequest {
    let mut request = request(
        tool_name,
        WorkspaceBinding {
            kind: WorkspaceBindingKind::CloudWorkspace,
            display_name: "OpenShell workspace".to_string(),
            cwd: Some("/sandbox".to_string()),
            authority: WorkspaceAuthority::ReadWrite,
            fallback_policy: FallbackPolicy::Disabled,
        },
        ExecutorBinding {
            kind: ExecutorBindingKind::HostedRunner,
            executor_id: "openshell-gateway".to_string(),
            display_name: "OpenShell Gateway".to_string(),
            transport: ToolTransportKind::GatewayRelay,
            status: ExecutorStatus::Online,
        },
    );
    request.runtime = Some(astra_runtime_env::RuntimeBinding::nvidia_openshell(
        "openshell-runtime",
    ));
    request.workspace_record = Some(astra_runtime_env::WorkspaceRecord {
        workspace_id: "openshell-workspace-1".to_string(),
        owner_scope: astra_runtime_env::WorkspaceOwnerScope::Tenant,
        kind: astra_runtime_env::WorkspaceBindingKind::CloudWorkspace,
        authority: astra_runtime_env::WorkspaceAuthority::ReadWrite,
        root_or_volume_ref: "/sandbox".to_string(),
        source: astra_runtime_env::WorkspaceSource::ProviderManaged {
            provider: "nvidia_openshell".to_string(),
            reference: "openshell-workspace-1".to_string(),
        },
        persistence: astra_runtime_env::WorkspacePersistence::Persistent,
        revision: "rev-1".to_string(),
        display_name: "OpenShell workspace".to_string(),
    });
    request
}

fn hosted_snapshot_workspace_record() -> astra_runtime_env::WorkspaceRecord {
    astra_runtime_env::WorkspaceRecord {
        workspace_id: "snapshot-1".to_string(),
        owner_scope: astra_runtime_env::WorkspaceOwnerScope::Tenant,
        kind: astra_runtime_env::WorkspaceBindingKind::CloudWorkspace,
        authority: astra_runtime_env::WorkspaceAuthority::ReadOnly,
        root_or_volume_ref: "/snapshot".to_string(),
        source: astra_runtime_env::WorkspaceSource::UploadedSnapshot {
            artifact_id: "artifact-1".to_string(),
        },
        persistence: astra_runtime_env::WorkspacePersistence::ImmutableSnapshot,
        revision: "rev-1".to_string(),
        display_name: "Snapshot".to_string(),
    }
}

#[test]
fn runner_rpc_execution_plan_builds_prepare_and_execute_requests() {
    let registry = astra_runtime_env::ToolRegistry::builtins();
    let request = hosted_snapshot_request("read_file");
    let binding = request.runtime_environment_binding(&registry);

    let plan = RunnerRpcExecutionPlan::from_request(&request, &binding).expect("runner plan");
    assert_eq!(plan.executor_id(), "snapshot-runner");

    let prepare = plan.prepare_request();
    assert_eq!(prepare.request_id, "prepare:call-1");
    assert_eq!(prepare.spec.session_id, "session-1");
    assert_eq!(prepare.spec.run_id, "run-1");
    assert_eq!(prepare.spec.requested_tools, vec!["read_file"]);
    assert_eq!(
        prepare
            .spec
            .workspace_record
            .as_ref()
            .expect("workspace record")
            .workspace_id,
        "snapshot-1"
    );

    let policy = astra_runtime_env::CompiledRuntimePolicy::dynamic(
        astra_runtime_env::PolicyRevision(7),
        binding.policy.clone(),
    );
    let handle = astra_runtime_env::RuntimeSessionHandle::from_spec(&prepare.spec)
        .with_policy(policy, &binding);
    let execute = plan.execute_request(handle);

    assert_eq!(execute.request_id, "execute:call-1");
    assert_eq!(execute.idempotency_key, "user-1:session-1:call-1");
    assert_eq!(
        execute.invocation.idempotency_key.as_deref(),
        Some("user-1:session-1:call-1")
    );
    assert_eq!(execute.invocation.call_id, "call-1");
    assert_eq!(execute.invocation.tool_name, "read_file");
    assert_eq!(
        execute.invocation.policy_revision,
        astra_runtime_env::PolicyRevision(7)
    );
    assert_eq!(
        execute.session.policy.revision,
        astra_runtime_env::PolicyRevision(7)
    );
}

#[test]
fn runner_rpc_execution_plan_rejects_cloud_workspace_without_record() {
    let registry = astra_runtime_env::ToolRegistry::builtins();
    let mut request = hosted_snapshot_request("read_file");
    request.workspace_record = None;
    let binding = request.runtime_environment_binding(&registry);

    let error = RunnerRpcExecutionPlan::from_request(&request, &binding)
        .expect_err("missing cloud workspace record must fail before transport");

    assert_eq!(
        error.kind,
        astra_runtime_env::RuntimeErrorKind::RuntimeUnavailable
    );
    assert!(error.retryable);
    assert!(!error.execution_started);
    assert!(!error.side_effects_maybe);
    assert!(error.message.contains("durable WorkspaceRecord"));
}

#[tokio::test]
async fn hosted_runner_executes_through_runner_rpc_transport() {
    let runner = Arc::new(StaticRunnerRpcTransport::new());
    let _local = CountingLocalTransport::new();

    let service = ToolExecutionService::builder()
        .runner_rpc_transport(runner.clone())
        .build();
    let local = CountingLocalTransport::new();

    let result = service
        .execute(hosted_snapshot_request("read_file"), &local)
        .await;

    assert!(!result.is_error, "{result:?}");
    assert_eq!(result.output, "runner-result");
    assert_eq!(local.calls(), 0);
    assert_eq!(runner.prepare_calls(), 1);
    assert_eq!(runner.execute_calls(), 1);
    let metadata = result.metadata.expect("runner metadata");
    assert_eq!(metadata["transport"], "runner_rpc");
    assert_eq!(metadata["executor"]["kind"], "hosted_runner");
    assert_eq!(
        metadata[astra_runtime_env::TOOL_RESULT_RUNTIME_SESSION]["executor_id"],
        "snapshot-runner"
    );
}

#[tokio::test]
async fn hosted_runner_oversized_output_is_blocked_at_transport_boundary() {
    let oversized = "x".repeat(1_048_577);
    let runner = Arc::new(StaticRunnerRpcTransport::with_output(oversized));
    let _local = CountingLocalTransport::new();

    let service = ToolExecutionService::builder()
        .runner_rpc_transport(runner.clone())
        .build();
    let local = CountingLocalTransport::new();

    let result = service
        .execute(hosted_snapshot_request("read_file"), &local)
        .await;

    assert!(result.is_error, "{result:?}");
    assert!(result.output.contains("output limit exceeded"));
    assert!(!result.output.contains(&"x".repeat(128)));
    assert_eq!(local.calls(), 0);
    assert_eq!(runner.prepare_calls(), 1);
    assert_eq!(runner.execute_calls(), 1);
    let metadata = result.metadata.expect("output limit metadata");
    assert_eq!(metadata["error_kind"], "output_limit_exceeded");
    assert_eq!(metadata["execution_started"], true);
    assert_eq!(metadata["side_effects_maybe"], true);
    assert_eq!(metadata["output_bytes"], 1_048_577);
    assert_eq!(metadata["max_output_bytes"], 1_048_576);
    assert_eq!(metadata["transport"], "runner_rpc");
}

#[tokio::test]
async fn hosted_runner_uses_policy_snapshot_output_limit_override() {
    let runner = Arc::new(StaticRunnerRpcTransport::with_output("abcd"));
    let _local = CountingLocalTransport::new();
    let _request = hosted_snapshot_request("read_file");
    let service = ToolExecutionService::builder()
        .runner_rpc_transport(runner.clone())
        .build();
    let local = CountingLocalTransport::new();
    let mut request = hosted_snapshot_request("read_file");
    request.policy.max_output_bytes = Some(3);

    let result = service.execute(request, &local).await;

    assert!(result.is_error, "{result:?}");
    assert!(result.output.contains("output limit exceeded"));
    assert_eq!(local.calls(), 0);
    assert_eq!(runner.prepare_calls(), 1);
    assert_eq!(runner.execute_calls(), 1);
    let metadata = result.metadata.expect("custom output limit metadata");
    assert_eq!(metadata["error_kind"], "output_limit_exceeded");
    assert_eq!(metadata["output_bytes"], 4);
    assert_eq!(metadata["max_output_bytes"], 3);
    assert_eq!(
        metadata["policy"]["intent"]["resources"]["max_output_bytes"],
        3
    );
    assert_eq!(
        metadata[astra_runtime_env::TOOL_RESULT_RUNTIME_SESSION]["resources"]["max_output_bytes"],
        3
    );
}

#[tokio::test(start_paused = true)]
async fn hosted_runner_prepare_timeout_reports_no_side_effects() {
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let runner = Arc::new(PendingPrepareRunnerRpcTransport::new(started_tx));
    let _request = hosted_snapshot_request("read_file");

    let service = ToolExecutionService::builder()
        .runner_rpc_transport(runner.clone())
        .build();
    let request = hosted_snapshot_request("read_file");

    let handle = tokio::spawn(async move {
        let local = CountingLocalTransport::new();
        let result = service.execute(request, &local).await;
        (result, local.calls())
    });

    started_rx.await.expect("runner prepare should start");
    tokio::time::advance(std::time::Duration::from_secs(31)).await;
    let (result, local_calls) = handle.await.expect("runner timeout task should not panic");

    assert!(result.is_error, "{result:?}");
    assert!(
        result.output.contains("max_execution_secs"),
        "{}",
        result.output
    );
    assert_eq!(local_calls, 0);
    assert_eq!(runner.prepare_calls(), 1);
    assert_eq!(runner.execute_calls(), 0);
    let metadata = result.metadata.expect("runner prepare timeout metadata");
    assert_eq!(metadata["error_kind"], TOOL_ERROR_KIND_TOOL_TIMEOUT);
    assert_eq!(metadata["reason"], TOOL_ERROR_KIND_TOOL_TIMEOUT);
    assert_eq!(metadata["blocked"], true);
    assert_eq!(metadata["execution_started"], false);
    assert_eq!(metadata["side_effects_maybe"], false);
    assert_eq!(metadata["next_action"], "none");
    assert_eq!(metadata["max_execution_secs"], 30.0);
    assert_eq!(metadata["transport"], "runner_rpc");
    assert_eq!(metadata["runtime_error"]["kind"], "tool_timeout");
}

#[tokio::test(start_paused = true)]
async fn hosted_runner_execute_timeout_reports_side_effect_uncertainty() {
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let runner = Arc::new(PendingRunnerRpcTransport::new(started_tx));
    let _request = hosted_snapshot_request("read_file");

    let service = ToolExecutionService::builder()
        .runner_rpc_transport(runner.clone())
        .build();
    let request = hosted_snapshot_request("read_file");

    let handle = tokio::spawn(async move {
        let local = CountingLocalTransport::new();
        let result = service.execute(request, &local).await;
        (result, local.calls())
    });

    started_rx.await.expect("runner execute should start");
    tokio::time::advance(std::time::Duration::from_secs(31)).await;
    let (result, local_calls) = handle.await.expect("runner timeout task should not panic");

    assert!(result.is_error, "{result:?}");
    assert!(
        result.output.contains("max_execution_secs"),
        "{}",
        result.output
    );
    assert_eq!(local_calls, 0);
    assert_eq!(runner.prepare_calls(), 1);
    assert_eq!(runner.execute_calls(), 1);
    let metadata = result.metadata.expect("runner execute timeout metadata");
    assert_eq!(metadata["error_kind"], TOOL_ERROR_KIND_TOOL_TIMEOUT);
    assert_eq!(metadata["reason"], TOOL_ERROR_KIND_TOOL_TIMEOUT);
    assert_eq!(metadata["blocked"], true);
    assert_eq!(metadata["execution_started"], true);
    assert_eq!(metadata["side_effects_maybe"], true);
    assert_eq!(metadata["next_action"], "inspect_effects_before_retry");
    assert_eq!(metadata["max_execution_secs"], 30.0);
    assert_eq!(metadata["transport"], "runner_rpc");
    assert_eq!(metadata["runtime_error"]["kind"], "tool_timeout");
}

#[tokio::test]
async fn hosted_runner_prepare_carries_workspace_record() {
    let runner = Arc::new(StaticRunnerRpcTransport::new());
    let service = ToolExecutionService::builder()
        .runner_rpc_transport(runner.clone())
        .build();
    let local = CountingLocalTransport::new();
    let request = hosted_snapshot_request("read_file");

    let result = service.execute(request, &local).await;

    assert!(!result.is_error, "{result:?}");
    let prepare = runner
        .first_prepare_request()
        .expect("prepare request should be recorded");
    let workspace = prepare
        .spec
        .workspace_record
        .expect("workspace record should be carried to runner prepare");
    assert_eq!(workspace.workspace_id, "snapshot-1");
    assert_eq!(
        workspace.kind,
        astra_runtime_env::WorkspaceBindingKind::CloudWorkspace
    );
    assert_eq!(
        workspace.authority,
        astra_runtime_env::WorkspaceAuthority::ReadOnly
    );
}

#[tokio::test]
async fn hosted_runner_missing_workspace_record_fails_before_runner_rpc() {
    let runner = Arc::new(StaticRunnerRpcTransport::new());
    let _local = CountingLocalTransport::new();
    let _request = hosted_snapshot_request("read_file");
    let service = ToolExecutionService::builder()
        .runner_rpc_transport(runner.clone())
        .build();
    let local = CountingLocalTransport::new();
    let mut request = hosted_snapshot_request("read_file");
    request.workspace_record = None;

    let result = service.execute(request, &local).await;

    assert!(result.is_error, "{result:?}");
    assert_eq!(local.calls(), 0);
    assert_eq!(runner.prepare_calls(), 0);
    assert_eq!(runner.execute_calls(), 0);
    let metadata = result.metadata.expect("runner error metadata");
    assert_eq!(metadata["error_kind"], "runtime_unavailable");
    assert_eq!(metadata["blocked"], true);
    assert!(
        result.output.contains("durable WorkspaceRecord"),
        "{}",
        result.output
    );
}

#[tokio::test]
async fn hosted_runner_without_runner_rpc_transport_does_not_fallback_to_local() {
    let service = ToolExecutionService::new_for_test();
    let local = CountingLocalTransport::new();

    let result = service
        .execute(hosted_snapshot_request("read_file"), &local)
        .await;

    assert!(result.is_error, "{result:?}");
    assert_eq!(local.calls(), 0);
    let metadata = result.metadata.expect("runner transport metadata");
    assert_eq!(
        metadata["error_kind"],
        astra_runtime_env::RuntimeErrorKind::TransportUnavailable.to_string()
    );
    assert_eq!(metadata["blocked"], true);
    assert_eq!(metadata["retryable"], true);
    assert_eq!(metadata["execution_started"], false);
    assert_eq!(metadata["side_effects_maybe"], false);
    assert_eq!(metadata["next_action"], "reconnect_runner");
    assert_eq!(metadata["runtime_error"]["kind"], "transport_unavailable");
    assert_eq!(metadata["transport"], "runner_rpc");
}

#[tokio::test]
async fn hosted_runner_prepare_rejection_skips_execute_and_local_fallback() {
    let runner = Arc::new(StaticRunnerRpcTransport::with_prepare_error(
        astra_runtime_env::RuntimeError::runtime_unavailable("runner pool drained"),
    ));
    let service = ToolExecutionService::builder()
        .runner_rpc_transport(runner.clone())
        .build();
    let local = CountingLocalTransport::new();

    let result = service
        .execute(hosted_snapshot_request("read_file"), &local)
        .await;

    assert!(result.is_error, "{result:?}");
    assert_eq!(local.calls(), 0);
    assert_eq!(runner.prepare_calls(), 1);
    assert_eq!(runner.execute_calls(), 0);
    let metadata = result.metadata.expect("runner error metadata");
    assert_eq!(metadata["error_kind"], "runtime_unavailable");
    assert_eq!(metadata["transport"], "runner_rpc");
    assert_eq!(metadata["blocked"], true);
    assert_eq!(metadata["runtime_error"]["message"], "runner pool drained");
}

#[tokio::test]
async fn cloud_workspace_blocks_without_server_fallback() {
    let service = ToolExecutionService::new_for_test();
    let local = CountingLocalTransport::new();
    let mut request = request(
        "git",
        WorkspaceBinding {
            kind: WorkspaceBindingKind::CloudWorkspace,
            display_name: "Hosted workspace".to_string(),
            cwd: Some("/checkout/repo".to_string()),
            authority: WorkspaceAuthority::ReadOnly,
            fallback_policy: FallbackPolicy::Disabled,
        },
        ExecutorBinding::server_local(),
    );
    request.args = serde_json::json!({"action": "status"});

    let result = service.execute(request, &local).await;

    assert!(result.is_error, "{result:?}");
    assert!(
        result.output.contains("No server fallback was attempted"),
        "{}",
        result.output
    );
    let metadata = result.metadata.expect("unsupported metadata");
    assert_eq!(metadata["error_kind"], TOOL_ERROR_KIND_ROUTE_MISMATCH);
    assert_eq!(metadata["reason"], RUN_BLOCKED_REASON_ROUTE_MISMATCH);
    assert_eq!(metadata["blocked"], true);
    assert_eq!(
        metadata["next_action"],
        "change_workspace_executor_runtime_or_policy"
    );
    assert_eq!(metadata["runtime_error"]["kind"], "route_mismatch");
    assert_eq!(metadata["workspace"]["kind"], "cloud_workspace");
    assert_eq!(metadata["executor"]["status"], "degraded");
    assert_eq!(local.calls(), 0);
}

#[tokio::test]
async fn edge_offline_with_fallback_disabled_does_not_call_server_local() {
    let service = ToolExecutionService::new_for_test();
    let local = CountingLocalTransport::new();
    let result = service
        .execute(
            request(
                "bash",
                WorkspaceBinding::edge_workspace(
                    "MacBook Pro",
                    "/Users/test/project",
                    WorkspaceAuthority::ReadWrite,
                ),
                ExecutorBinding::edge_agent(
                    "edge-macbook-1",
                    "MacBook Pro",
                    ToolTransportKind::EdgeWs,
                    ExecutorStatus::Offline,
                ),
            ),
            &local,
        )
        .await;

    assert!(result.is_error, "{result:?}");
    assert!(
        result.output.contains("fallback is disabled"),
        "{}",
        result.output
    );
    let metadata = result.metadata.expect("offline metadata");
    assert_eq!(metadata["error_kind"], TOOL_ERROR_KIND_EXECUTOR_OFFLINE);
    assert_eq!(metadata["blocked"], true);
    assert_eq!(metadata["executor"]["status"], "offline");
    assert_eq!(local.calls(), 0);
}

#[tokio::test]
async fn edge_bound_selected_executor_does_not_route_to_other_connected_edge() {
    let pool = astra_server_types::edge_connection_pool::EdgeConnectionPool::new();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<astra_server_types::EdgeServerMessage>(1);
    pool.register(
        "user-1",
        "edge-other",
        Some("Other laptop".to_string()),
        Some("/Users/test/other".to_string()),
        tx,
    );
    let service = ToolExecutionService::builder()
        .edge_connection_pool(pool)
        .build();
    let local = CountingLocalTransport::new();

    let result = service
        .execute(
            request(
                "bash",
                WorkspaceBinding::edge_workspace(
                    "MacBook Pro",
                    "/Users/test/project",
                    WorkspaceAuthority::ReadWrite,
                ),
                ExecutorBinding::edge_agent(
                    "edge-selected",
                    "MacBook Pro",
                    ToolTransportKind::EdgeWs,
                    ExecutorStatus::Online,
                ),
            ),
            &local,
        )
        .await;

    assert!(result.is_error, "{result:?}");
    assert!(
        result.output.contains("transport 'edge_ws' disconnected")
            || result.output.contains("transport disconnected"),
        "{}",
        result.output
    );
    assert_eq!(local.calls(), 0);
    assert!(
        rx.try_recv().is_err(),
        "selected edge binding must not dispatch to a different connected edge"
    );
}

#[tokio::test]
async fn edge_dispatch_result_reports_edge_ledger_transport() {
    let dispatch = Arc::new(StaticEdgeDispatch::default());
    let _local = CountingLocalTransport::new();

    let service = ToolExecutionService::builder()
        .edge_dispatch_service(dispatch.clone())
        .edge_registry_service(Arc::new(StaticEdgeRegistry {
            agents: vec![edge_agent_record("edge-selected")],
        }))
        .build();
    let local = CountingLocalTransport::new();

    let result = service
        .execute(
            request(
                "bash",
                WorkspaceBinding::edge_workspace(
                    "MacBook Pro",
                    "/Users/test/project",
                    WorkspaceAuthority::ReadWrite,
                ),
                ExecutorBinding::edge_agent(
                    "edge-selected",
                    "MacBook Pro",
                    ToolTransportKind::EdgeWs,
                    ExecutorStatus::Online,
                ),
            ),
            &local,
        )
        .await;

    assert!(!result.is_error, "{result:?}");
    assert_eq!(result.output, "ledger-result");
    let metadata = result.metadata.expect("ledger metadata");
    assert_eq!(metadata["transport"], "edge_ledger");
    assert_eq!(metadata["executor"]["transport"], "edge_ledger");
    assert_eq!(metadata["executor"]["status"], "online");
    assert_eq!(metadata["executor"]["executor_id"], "edge-selected");
    assert_eq!(metadata["workspace"]["kind"], "edge_workspace");
    assert_eq!(local.calls(), 0);
    assert_eq!(
        *dispatch
            .inserted_edge_agent_ids
            .lock()
            .expect("inserted edge agent ids lock"),
        vec!["edge-selected".to_string()]
    );
}

#[tokio::test]
async fn edge_bound_explicit_offline_status_blocks_without_dispatch() {
    let dispatch = Arc::new(StaticEdgeDispatch::default());
    let _local = CountingLocalTransport::new();

    let service = ToolExecutionService::builder()
        .edge_dispatch_service(dispatch.clone())
        .edge_registry_service(Arc::new(StaticEdgeRegistry {
            agents: vec![edge_agent_record("edge-selected")],
        }))
        .build();
    let local = CountingLocalTransport::new();

    let result = service
        .execute(
            request(
                "bash",
                WorkspaceBinding::edge_workspace(
                    "MacBook Pro",
                    "/Users/test/project",
                    WorkspaceAuthority::ReadWrite,
                ),
                ExecutorBinding::edge_agent(
                    "edge-selected",
                    "MacBook Pro",
                    ToolTransportKind::EdgeWs,
                    ExecutorStatus::Offline,
                ),
            ),
            &local,
        )
        .await;

    assert!(result.is_error, "{result:?}");
    let metadata = result.metadata.expect("offline metadata");
    assert_eq!(metadata["error_kind"], TOOL_ERROR_KIND_EXECUTOR_OFFLINE);
    assert_eq!(metadata["executor"]["status"], "offline");
    assert_eq!(local.calls(), 0);
    assert!(
        dispatch
            .inserted_edge_agent_ids
            .lock()
            .expect("inserted edge agent ids lock")
            .is_empty(),
        "explicit offline executor status must block before edge ledger dispatch"
    );
}

#[tokio::test]
async fn edge_dispatch_without_result_reports_transport_disconnected() {
    let dispatch = Arc::new(StaticEdgeDispatch::no_result());
    let _local = CountingLocalTransport::new();

    let service = ToolExecutionService::builder()
        .edge_dispatch_service(dispatch.clone())
        .edge_registry_service(Arc::new(StaticEdgeRegistry {
            agents: vec![edge_agent_record("edge-selected")],
        }))
        .build();
    let local = CountingLocalTransport::new();

    let result = service
        .execute(
            request(
                "bash",
                WorkspaceBinding::edge_workspace(
                    "MacBook Pro",
                    "/Users/test/project",
                    WorkspaceAuthority::ReadWrite,
                ),
                ExecutorBinding::edge_agent(
                    "edge-selected",
                    "MacBook Pro",
                    ToolTransportKind::EdgeWs,
                    ExecutorStatus::Online,
                ),
            ),
            &local,
        )
        .await;

    assert!(result.is_error, "{result:?}");
    assert!(
        result.output.contains("transport 'edge_ws' disconnected"),
        "{}",
        result.output
    );
    let metadata = result.metadata.expect("transport disconnected metadata");
    assert_eq!(
        metadata["error_kind"],
        TOOL_ERROR_KIND_TRANSPORT_DISCONNECTED
    );
    assert_eq!(
        metadata["reason"],
        RUN_BLOCKED_REASON_TRANSPORT_DISCONNECTED
    );
    assert_eq!(metadata["blocked"], true);
    assert_eq!(metadata["executor"]["status"], "degraded");
    assert_eq!(metadata["workspace"]["kind"], "edge_workspace");
    assert_eq!(local.calls(), 0);
    assert_eq!(
        *dispatch
            .inserted_edge_agent_ids
            .lock()
            .expect("inserted edge agent ids lock"),
        vec!["edge-selected".to_string()]
    );
    let failed_dispatches = dispatch
        .failed_dispatches
        .lock()
        .expect("failed dispatches lock");
    assert_eq!(failed_dispatches.len(), 1);
    assert_eq!(failed_dispatches[0].1, "expired");
}

#[tokio::test]
async fn edge_dispatch_requires_runtime_environment_advertisement() {
    let dispatch = Arc::new(StaticEdgeDispatch::default());
    let mut agent = edge_agent_record("edge-selected");
    agent.capabilities = None;
    let service = ToolExecutionService::builder()
        .edge_dispatch_service(dispatch.clone())
        .edge_registry_service(Arc::new(StaticEdgeRegistry {
            agents: vec![agent],
        }))
        .build();
    let local = CountingLocalTransport::new();

    let result = service
        .execute(
            request(
                "bash",
                WorkspaceBinding::edge_workspace(
                    "MacBook Pro",
                    "/Users/test/project",
                    WorkspaceAuthority::ReadWrite,
                ),
                ExecutorBinding::edge_agent(
                    "edge-selected",
                    "MacBook Pro",
                    ToolTransportKind::EdgeWs,
                    ExecutorStatus::Online,
                ),
            ),
            &local,
        )
        .await;

    assert!(result.is_error, "{result:?}");
    assert!(
        result
            .output
            .contains("runtime_environment_advertisement_required"),
        "{}",
        result.output
    );
    let metadata = result.metadata.expect("capability denied metadata");
    assert_eq!(metadata["error_kind"], TOOL_ERROR_KIND_CAPABILITY_DENIED);
    assert_eq!(metadata["blocked"], true);
    assert_eq!(local.calls(), 0);
    assert!(
        dispatch
            .inserted_edge_agent_ids
            .lock()
            .expect("inserted edge agent ids lock")
            .is_empty(),
        "missing edge capabilities must block before edge ledger dispatch"
    );
}

#[tokio::test]
async fn control_plane_tool_bypasses_edge_transport() {
    let service = ToolExecutionService::new_for_test();
    let local = CountingLocalTransport::new();
    let edge_request = request(
        "agent",
        WorkspaceBinding::edge_workspace(
            "MacBook Pro",
            "/Users/test/project",
            WorkspaceAuthority::ReadWrite,
        ),
        ExecutorBinding::edge_agent(
            "edge-macbook-1",
            "MacBook Pro",
            ToolTransportKind::EdgeWs,
            ExecutorStatus::Offline,
        ),
    );

    assert_eq!(
        service.routing_decision(&edge_request),
        ToolExecutionRouteKind::ServerControlPlane
    );
    let result = service.execute(edge_request, &local).await;

    assert!(!result.is_error, "{result:?}");
    assert_eq!(result.output, "local:agent");
    assert_eq!(local.calls(), 1);
}

#[tokio::test]
async fn server_runtime_tools_bypass_edge_transport() {
    let service = ToolExecutionService::new_for_test();
    let local = CountingLocalTransport::new();
    let server_runtime_tools = [
        "tool_search",
        "web_search",
        "web_fetch",
        "memory",
        "mo",
        "mo_query",
        "rollback_database_snapshots",
        "github",
    ];

    for tool in server_runtime_tools {
        let edge_request = request(
            tool,
            WorkspaceBinding::edge_workspace(
                "MacBook Pro",
                "/Users/test/project",
                WorkspaceAuthority::ReadWrite,
            ),
            ExecutorBinding::edge_agent(
                "edge-macbook-1",
                "MacBook Pro",
                ToolTransportKind::EdgeWs,
                ExecutorStatus::Offline,
            ),
        );

        assert_eq!(
            service.routing_decision(&edge_request),
            ToolExecutionRouteKind::ServerRuntime,
            "{tool} must not depend on edge transport"
        );
        let result = service.execute(edge_request, &local).await;
        assert!(!result.is_error, "{tool}: {result:?}");
        assert_eq!(result.output, format!("local:{tool}"));
        let metadata = result.metadata.expect("server runtime metadata");
        assert_eq!(metadata["workspace"]["kind"], "none", "{tool}");
        assert_eq!(metadata["executor"]["kind"], "server_local", "{tool}");
        assert_eq!(
            metadata["executor"]["display_name"], "Server runtime",
            "{tool}"
        );
        assert_eq!(metadata["transport"], "server_local", "{tool}");
    }
    assert_eq!(local.calls(), server_runtime_tools.len());
}

#[tokio::test]
async fn local_code_tool_remains_edge_bound_with_edge_binding() {
    let service = ToolExecutionService::new_for_test();
    let local_code_tools = ["bash", "read_file", "list_dir", "grep", "glob", "git"];

    for tool in local_code_tools {
        let edge_request = request(
            tool,
            WorkspaceBinding::edge_workspace(
                "MacBook Pro",
                "/Users/test/project",
                WorkspaceAuthority::ReadWrite,
            ),
            ExecutorBinding::edge_agent(
                "edge-macbook-1",
                "MacBook Pro",
                ToolTransportKind::EdgeWs,
                ExecutorStatus::Offline,
            ),
        );

        assert_eq!(
            service.routing_decision(&edge_request),
            ToolExecutionRouteKind::EdgeBound,
            "{tool} must stay bound to the selected edge workspace"
        );
    }
}

#[tokio::test]
async fn request_scoped_mcp_tools_bypass_edge_transport() {
    let dispatch = Arc::new(StaticEdgeDispatch::default());
    let service = ToolExecutionService::builder()
        .edge_dispatch_service(dispatch.clone())
        .edge_registry_service(Arc::new(StaticEdgeRegistry {
            agents: vec![edge_agent_record("edge-macbook-1")],
        }))
        .build();
    let local = CountingLocalTransport::new();
    let edge_request = request(
        "mcp__demo__search",
        WorkspaceBinding::edge_workspace(
            "MacBook Pro",
            "/Users/test/project",
            WorkspaceAuthority::ReadWrite,
        ),
        ExecutorBinding::edge_agent(
            "edge-macbook-1",
            "MacBook Pro",
            ToolTransportKind::EdgeWs,
            ExecutorStatus::Offline,
        ),
    );

    assert_eq!(
        service.routing_decision(&edge_request),
        ToolExecutionRouteKind::RequestScopedMcp
    );
    let result = service.execute(edge_request, &local).await;

    assert!(!result.is_error, "{result:?}");
    assert_eq!(result.output, "local:mcp__demo__search");
    assert_eq!(local.calls(), 1);
    assert!(
        dispatch
            .inserted_edge_agent_ids
            .lock()
            .expect("inserted edge agent ids lock")
            .is_empty(),
        "request-scoped MCP tools must not dispatch to edge"
    );
    let metadata = result.metadata.expect("request-scoped MCP metadata");
    assert_eq!(metadata["workspace"]["kind"], "edge_workspace");
    assert_eq!(metadata["workspace"]["cwd"], "/Users/test/project");
    assert_eq!(metadata["executor"]["kind"], "mcp");
    assert_eq!(metadata["executor"]["executor_id"], "request-scoped-mcp");
    assert_eq!(metadata["executor"]["display_name"], "MCP server");
    assert_eq!(metadata["executor"]["transport"], "mcp_http");
    assert_eq!(metadata["transport"], "mcp_http");
}

// ── Cancel token propagation ──────────────────────────────────────────

#[tokio::test]
async fn cancel_already_triggered_skips_all_transports() {
    let dispatch = Arc::new(StaticEdgeDispatch::default());
    let _local = CountingLocalTransport::new();
    let _cancel = Arc::new(CancellationToken::new());
    let service = ToolExecutionService::builder()
        .edge_dispatch_service(dispatch.clone())
        .edge_registry_service(Arc::new(StaticEdgeRegistry {
            agents: vec![edge_agent_record("edge-online")],
        }))
        .build();
    let local = CountingLocalTransport::new();
    let cancel = Arc::new(CancellationToken::new());
    cancel.cancel();

    let result = service
        .execute_with_cancel(
            request(
                "bash",
                WorkspaceBinding::edge_workspace(
                    "MacBook Pro",
                    "/Users/test/project",
                    WorkspaceAuthority::ReadWrite,
                ),
                ExecutorBinding::edge_agent(
                    "edge-online",
                    "MacBook Pro",
                    ToolTransportKind::EdgeWs,
                    ExecutorStatus::Online,
                ),
            ),
            &local,
            Some(cancel),
        )
        .await;

    assert!(result.is_error, "{result:?}");
    assert!(result.output.contains("cancelled"), "{}", result.output);
    assert_eq!(local.calls(), 0);
    assert!(
        dispatch
            .inserted_edge_agent_ids
            .lock()
            .expect("lock")
            .is_empty(),
        "cancel must block dispatch insertion"
    );
    let metadata = result.metadata.expect("edge cancel metadata");
    assert_eq!(metadata["error_kind"], TOOL_ERROR_KIND_CANCELLED);
    assert_eq!(metadata["reason"], TOOL_ERROR_KIND_CANCELLED);
    assert_eq!(metadata["cancelled"], true);
    assert_eq!(metadata["execution_started"], false);
    assert_eq!(metadata["side_effects_maybe"], false);
    assert_eq!(metadata["transport"], "edge_ws");
    assert_eq!(metadata["executor"]["kind"], "edge_agent");
    assert_eq!(metadata["runtime"]["session_manager"], "host_process");
}

#[tokio::test]
async fn server_local_cancel_during_execute_reports_side_effect_uncertainty() {
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let service = ToolExecutionService::new_for_test();
    let cancel = Arc::new(CancellationToken::new());
    let cancel_for_task = cancel.clone();
    let request = request(
        "bash",
        WorkspaceBinding::server_sandbox("/tmp/astra-workspace"),
        ExecutorBinding::server_local(),
    );

    let handle = tokio::spawn(async move {
        let local = PendingLocalTransport::new(started_tx);
        let result = service
            .execute_with_cancel(request, &local, Some(cancel_for_task))
            .await;
        (result, local.calls())
    });

    tokio::time::timeout(std::time::Duration::from_secs(1), started_rx)
        .await
        .expect("local execute should start")
        .expect("local execute start signal");
    cancel.cancel();
    let (result, local_calls) = tokio::time::timeout(std::time::Duration::from_secs(1), handle)
        .await
        .expect("local cancel should resolve")
        .expect("local cancel task should not panic");

    assert!(result.is_error, "{result:?}");
    assert!(result.output.contains("cancelled"), "{}", result.output);
    assert_eq!(local_calls, 1);
    let metadata = result.metadata.expect("local cancel metadata");
    assert_eq!(metadata["error_kind"], TOOL_ERROR_KIND_CANCELLED);
    assert_eq!(metadata["reason"], TOOL_ERROR_KIND_CANCELLED);
    assert_eq!(metadata["cancelled"], true);
    assert_eq!(metadata["blocked"], true);
    assert_eq!(metadata["execution_started"], true);
    assert_eq!(metadata["side_effects_maybe"], true);
    assert_eq!(metadata["transport"], "server_local");
    assert_eq!(metadata["executor"]["kind"], "server_local");
    assert_eq!(metadata["runtime"]["session_manager"], "host_process");
}

#[tokio::test]
async fn request_scoped_mcp_cancel_reports_mcp_binding() {
    let service = ToolExecutionService::new_for_test();
    let local = CountingLocalTransport::new();
    let cancel = Arc::new(CancellationToken::new());
    cancel.cancel();
    let request = request(
        "mcp__rag__retrieve",
        WorkspaceBinding {
            kind: WorkspaceBindingKind::None,
            display_name: "No workspace".to_string(),
            cwd: None,
            authority: WorkspaceAuthority::None,
            fallback_policy: FallbackPolicy::Disabled,
        },
        ExecutorBinding::server_local(),
    );

    let result = service
        .execute_with_cancel(request, &local, Some(cancel))
        .await;

    assert!(result.is_error, "{result:?}");
    assert_eq!(local.calls(), 0);
    let metadata = result.metadata.expect("mcp cancel metadata");
    assert_eq!(metadata["error_kind"], TOOL_ERROR_KIND_CANCELLED);
    assert_eq!(metadata["cancelled"], true);
    assert_eq!(metadata["execution_started"], false);
    assert_eq!(metadata["side_effects_maybe"], false);
    assert_eq!(metadata["workspace"]["kind"], "none");
    assert_eq!(metadata["executor"]["kind"], "mcp");
    assert_eq!(metadata["executor"]["transport"], "mcp_http");
    assert_eq!(metadata["transport"], "mcp_http");
    assert_eq!(metadata["runtime"]["session_manager"], "none");
}

#[tokio::test]
async fn hosted_runner_cancel_during_execute_reports_side_effect_uncertainty() {
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let runner = Arc::new(PendingRunnerRpcTransport::new(started_tx));
    let cancel = Arc::new(CancellationToken::new());
    let _cancel_for_task = cancel.clone();
    let _request = hosted_snapshot_request("read_file");

    let service = ToolExecutionService::builder()
        .runner_rpc_transport(runner.clone())
        .build();
    let cancel = Arc::new(CancellationToken::new());
    let cancel_for_task = cancel.clone();
    let request = hosted_snapshot_request("read_file");

    let handle = tokio::spawn(async move {
        let local = CountingLocalTransport::new();
        let result = service
            .execute_with_cancel(request, &local, Some(cancel_for_task))
            .await;
        (result, local.calls())
    });

    tokio::time::timeout(std::time::Duration::from_secs(1), started_rx)
        .await
        .expect("runner execute should start")
        .expect("runner execute start signal");
    cancel.cancel();
    let (result, local_calls) = tokio::time::timeout(std::time::Duration::from_secs(1), handle)
        .await
        .expect("runner cancel should resolve")
        .expect("runner cancel task should not panic");

    assert!(result.is_error, "{result:?}");
    assert!(result.output.contains("cancelled"), "{}", result.output);
    assert_eq!(local_calls, 0);
    assert_eq!(runner.prepare_calls(), 1);
    assert_eq!(runner.execute_calls(), 1);
    let metadata = result.metadata.expect("runner cancel metadata");
    assert_eq!(metadata["error_kind"], TOOL_ERROR_KIND_CANCELLED);
    assert_eq!(metadata["reason"], TOOL_ERROR_KIND_CANCELLED);
    assert_eq!(metadata["cancelled"], true);
    assert_eq!(metadata["blocked"], true);
    assert_eq!(metadata["execution_started"], true);
    assert_eq!(metadata["side_effects_maybe"], true);
    assert_eq!(metadata["next_action"], "inspect_effects_before_retry");
    assert_eq!(metadata["transport"], "runner_rpc");
    assert_eq!(metadata["executor"]["kind"], "hosted_runner");
    assert_eq!(metadata["runtime"]["isolation_backend"], "oci_runtime");
    assert_eq!(
        metadata["runtime_environment"]["runtime"]["runtime_id"],
        "snapshot-runtime"
    );
}

#[tokio::test]
async fn gateway_relay_cancel_during_execute_reports_side_effect_uncertainty() {
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let gateway = Arc::new(PendingGatewayRelayTransport::new(started_tx));
    let cancel = Arc::new(CancellationToken::new());
    let _cancel_for_task = cancel.clone();
    let _request = openshell_gateway_request("bash");

    let service = ToolExecutionService::builder()
        .gateway_relay_transport(gateway.clone())
        .build();
    let cancel = Arc::new(CancellationToken::new());
    let cancel_for_task = cancel.clone();
    let request = openshell_gateway_request("bash");

    let handle = tokio::spawn(async move {
        let local = CountingLocalTransport::new();
        let result = service
            .execute_with_cancel(request, &local, Some(cancel_for_task))
            .await;
        (result, local.calls())
    });

    tokio::time::timeout(std::time::Duration::from_secs(1), started_rx)
        .await
        .expect("gateway relay execute should start")
        .expect("gateway relay execute start signal");
    cancel.cancel();
    let (result, local_calls) = tokio::time::timeout(std::time::Duration::from_secs(1), handle)
        .await
        .expect("gateway relay cancel should resolve")
        .expect("gateway relay cancel task should not panic");

    assert!(result.is_error, "{result:?}");
    assert!(result.output.contains("cancelled"), "{}", result.output);
    assert_eq!(local_calls, 0);
    assert_eq!(gateway.calls(), 1);
    let metadata = result.metadata.expect("gateway cancel metadata");
    assert_eq!(metadata["error_kind"], TOOL_ERROR_KIND_CANCELLED);
    assert_eq!(metadata["reason"], TOOL_ERROR_KIND_CANCELLED);
    assert_eq!(metadata["cancelled"], true);
    assert_eq!(metadata["blocked"], true);
    assert_eq!(metadata["execution_started"], true);
    assert_eq!(metadata["side_effects_maybe"], true);
    assert_eq!(metadata["next_action"], "inspect_effects_before_retry");
    assert_eq!(metadata["transport"], "gateway_relay");
    assert_eq!(metadata["executor"]["transport"], "gateway_relay");
    assert_eq!(metadata["runtime"]["session_manager"], "nvidia_open_shell");
    assert_eq!(
        metadata["runtime_environment"]["runtime"]["runtime_id"],
        "openshell-runtime"
    );
}

#[tokio::test]
async fn edge_dispatch_cancel_during_wait_reports_side_effect_uncertainty() {
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let dispatch = Arc::new(PendingEdgeDispatch::new(started_tx));
    let service = ToolExecutionService::builder()
        .edge_dispatch_service(dispatch.clone())
        .edge_registry_service(Arc::new(StaticEdgeRegistry {
            agents: vec![edge_agent_record("edge-online")],
        }))
        .build();
    let cancel = Arc::new(CancellationToken::new());
    let cancel_for_task = cancel.clone();
    let request = request(
        "bash",
        WorkspaceBinding::edge_workspace(
            "MacBook Pro",
            "/Users/test/project",
            WorkspaceAuthority::ReadWrite,
        ),
        ExecutorBinding::edge_agent(
            "edge-online",
            "MacBook Pro",
            ToolTransportKind::EdgeWs,
            ExecutorStatus::Online,
        ),
    );

    let handle = tokio::spawn(async move {
        let local = CountingLocalTransport::new();
        let result = service
            .execute_with_cancel(request, &local, Some(cancel_for_task))
            .await;
        (result, local.calls())
    });

    tokio::time::timeout(std::time::Duration::from_secs(1), started_rx)
        .await
        .expect("edge dispatch wait should start")
        .expect("edge dispatch wait start signal");
    cancel.cancel();
    let (result, local_calls) = tokio::time::timeout(std::time::Duration::from_secs(1), handle)
        .await
        .expect("edge dispatch cancel should resolve")
        .expect("edge dispatch cancel task should not panic");

    assert!(result.is_error, "{result:?}");
    assert!(result.output.contains("cancelled"), "{}", result.output);
    assert_eq!(local_calls, 0);
    assert_eq!(
        dispatch
            .inserted_edge_agent_ids
            .lock()
            .expect("inserted edge agent ids lock")
            .as_slice(),
        ["edge-online"]
    );
    let failed = dispatch
        .failed_dispatches
        .lock()
        .expect("failed dispatches lock");
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].1, TOOL_ERROR_KIND_CANCELLED);
    let metadata = result.metadata.expect("edge dispatch cancel metadata");
    assert_eq!(metadata["error_kind"], TOOL_ERROR_KIND_CANCELLED);
    assert_eq!(metadata["reason"], TOOL_ERROR_KIND_CANCELLED);
    assert_eq!(metadata["cancelled"], true);
    assert_eq!(metadata["blocked"], true);
    assert_eq!(metadata["execution_started"], true);
    assert_eq!(metadata["side_effects_maybe"], true);
    assert_eq!(metadata["next_action"], "inspect_effects_before_retry");
    assert_eq!(metadata["transport"], "edge_ledger");
    assert_eq!(metadata["executor"]["kind"], "edge_agent");
    assert_eq!(metadata["runtime"]["session_manager"], "host_process");
}

// ── Both transports unavailable with Online executor ──────────────────

#[tokio::test]
async fn online_executor_with_no_transports_reports_disconnected_with_diagnostics() {
    let dispatch = Arc::new(StaticEdgeDispatch::no_result());
    let _local = CountingLocalTransport::new();

    let service = ToolExecutionService::builder()
        .edge_dispatch_service(dispatch.clone())
        .edge_registry_service(Arc::new(StaticEdgeRegistry {
            agents: vec![edge_agent_record("edge-online")],
        }))
        .build();
    let local = CountingLocalTransport::new();

    let result = service
        .execute(
            request(
                "bash",
                WorkspaceBinding::edge_workspace(
                    "MacBook Pro",
                    "/Users/test/project",
                    WorkspaceAuthority::ReadWrite,
                ),
                ExecutorBinding::edge_agent(
                    "edge-selected",
                    "MacBook Pro",
                    ToolTransportKind::EdgeWs,
                    ExecutorStatus::Online,
                ),
            ),
            &local,
        )
        .await;

    assert!(result.is_error, "{result:?}");
    assert!(
        result.output.contains("transport 'edge_ws' disconnected")
            || result.output.contains("transport disconnected"),
        "{}",
        result.output
    );
    let metadata = result.metadata.expect("diagnostics metadata");
    assert_eq!(
        metadata["error_kind"],
        TOOL_ERROR_KIND_TRANSPORT_DISCONNECTED
    );
    assert_eq!(metadata["executor"]["status"], "degraded");
    assert_eq!(metadata["workspace"]["kind"], "edge_workspace");
    assert_eq!(local.calls(), 0);
}

/// Verify edge_executor_id never returns Some("") — the pattern
/// `is_some() + unwrap_or_default()` was previously exploitable.
#[test]
fn edge_executor_id_returns_none_for_empty_id() {
    let request = ToolExecutionRequest {
        executor: ExecutorBinding {
            kind: ExecutorBindingKind::EdgeAgent,
            executor_id: String::new(),
            display_name: "test-edge".to_string(),
            transport: ToolTransportKind::EdgeWs,
            status: ExecutorStatus::Online,
        },
        workspace: WorkspaceBinding {
            kind: WorkspaceBindingKind::EdgeWorkspace,
            display_name: "test-ws".to_string(),
            cwd: None,
            authority: WorkspaceAuthority::ReadWrite,
            fallback_policy: FallbackPolicy::Disabled,
        },
        workspace_record: None,
        runtime: None,
        tool_name: "bash".to_string(),
        args: serde_json::json!({"cmd": "ls"}),
        user_id: "test-user".to_string(),
        run_id: "run-1".to_string(),
        session_id: "session-1".to_string(),
        tool_call_id: "tc-1".to_string(),
        policy: ToolPolicySnapshot::default(),
    };
    assert_eq!(
        edge_executor_id(&request),
        None,
        "empty executor_id on EdgeAgent must return None, not silently route with empty string"
    );
}

/// When executor_id is whitespace-only, edge_executor_id returns None.
#[test]
fn edge_executor_id_rejects_whitespace_only_id() {
    let request = ToolExecutionRequest {
        executor: ExecutorBinding {
            kind: ExecutorBindingKind::EdgeAgent,
            executor_id: "   ".to_string(),
            display_name: "test-edge".to_string(),
            transport: ToolTransportKind::EdgeWs,
            status: ExecutorStatus::Online,
        },
        workspace: WorkspaceBinding {
            kind: WorkspaceBindingKind::EdgeWorkspace,
            display_name: "test-ws".to_string(),
            cwd: None,
            authority: WorkspaceAuthority::ReadWrite,
            fallback_policy: FallbackPolicy::Disabled,
        },
        workspace_record: None,
        runtime: None,
        tool_name: "bash".to_string(),
        args: serde_json::json!({"cmd": "ls"}),
        user_id: "test-user".to_string(),
        run_id: "run-1".to_string(),
        session_id: "session-1".to_string(),
        tool_call_id: "tc-1".to_string(),
        policy: ToolPolicySnapshot::default(),
    };
    assert_eq!(
        edge_executor_id(&request),
        None,
        "whitespace-only executor_id must be treated as unset"
    );
}

/// verify match-based routing: None → execute_tool_any_edge_with_cancel
#[test]
fn edge_executor_id_returns_some_for_valid_id() {
    let request = ToolExecutionRequest {
        executor: ExecutorBinding {
            kind: ExecutorBindingKind::EdgeAgent,
            executor_id: "  valid-edge-123  ".to_string(),
            display_name: "test-edge".to_string(),
            transport: ToolTransportKind::EdgeWs,
            status: ExecutorStatus::Online,
        },
        workspace: WorkspaceBinding {
            kind: WorkspaceBindingKind::EdgeWorkspace,
            display_name: "test-ws".to_string(),
            cwd: None,
            authority: WorkspaceAuthority::ReadWrite,
            fallback_policy: FallbackPolicy::Disabled,
        },
        workspace_record: None,
        runtime: None,
        tool_name: "bash".to_string(),
        args: serde_json::json!({"cmd": "ls"}),
        user_id: "test-user".to_string(),
        run_id: "run-1".to_string(),
        session_id: "session-1".to_string(),
        tool_call_id: "tc-1".to_string(),
        policy: ToolPolicySnapshot::default(),
    };
    assert_eq!(edge_executor_id(&request), Some("valid-edge-123"));
}
