use super::*;
use async_trait::async_trait;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

use super::super::tool_transport_plan::{EdgeBoundExecutionPlan, edge_executor_id};

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
        _cancel_token: Option<&CancellationToken>,
    ) -> astra_tools::ToolResult {
        self.calls.fetch_add(1, Ordering::SeqCst);
        astra_tools::ToolResult::text(format!("local:{}", request.tool_name))
    }
}

struct CapturingLocalTransport {
    args: Mutex<Option<Value>>,
}

impl CapturingLocalTransport {
    fn new() -> Self {
        Self {
            args: Mutex::new(None),
        }
    }

    fn args(&self) -> Value {
        self.args
            .lock()
            .expect("captured local args lock")
            .clone()
            .expect("captured local args")
    }
}

#[async_trait]
impl ServerLocalToolTransport for CapturingLocalTransport {
    async fn execute_server_local_tool(
        &self,
        request: &ToolExecutionRequest,
        _cancel_token: Option<&CancellationToken>,
    ) -> astra_tools::ToolResult {
        *self.args.lock().expect("captured local args lock") = Some(request.args.clone());
        astra_tools::ToolResult::text("captured-local".to_string())
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
        _cancel_token: Option<&CancellationToken>,
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

struct StaticSandboxResidentAgentTransport {
    calls: AtomicUsize,
    error: Option<astra_runtime_env::RuntimeError>,
    output: String,
}

impl StaticSandboxResidentAgentTransport {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            error: None,
            output: "resident-agent-result".to_string(),
        }
    }

    fn with_error(error: astra_runtime_env::RuntimeError) -> Self {
        Self {
            error: Some(error),
            ..Self::new()
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
impl ExternalTransport for StaticSandboxResidentAgentTransport {
    async fn execute_tool(
        &self,
        request: ToolExecutionRequest,
        binding: astra_runtime_env::RunBinding,
    ) -> Result<astra_runtime_env::RuntimeToolOutcome, astra_runtime_env::RuntimeError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(error) = self.error.clone() {
            return Err(error);
        }
        Ok(runtime_outcome_for_request(
            &request,
            &binding,
            &self.output,
        ))
    }
}

struct PendingSandboxResidentAgentTransport {
    calls: AtomicUsize,
    execute_started: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

impl PendingSandboxResidentAgentTransport {
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
impl ExternalTransport for PendingSandboxResidentAgentTransport {
    async fn execute_tool(
        &self,
        _request: ToolExecutionRequest,
        _binding: astra_runtime_env::RunBinding,
    ) -> Result<astra_runtime_env::RuntimeToolOutcome, astra_runtime_env::RuntimeError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let sender = self
            .execute_started
            .lock()
            .expect("resident agent execute started lock")
            .take();
        if let Some(sender) = sender {
            let _ = sender.send(());
        }
        std::future::pending::<()>().await;
        unreachable!("pending resident agent execute never completes")
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
impl ExternalTransport for StaticGatewayRelayTransport {
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

struct CapturingGatewayRelayTransport {
    args: Mutex<Option<Value>>,
}

impl CapturingGatewayRelayTransport {
    fn new() -> Self {
        Self {
            args: Mutex::new(None),
        }
    }

    fn args(&self) -> Value {
        self.args
            .lock()
            .expect("captured gateway args lock")
            .clone()
            .expect("captured gateway args")
    }
}

#[async_trait]
impl ExternalTransport for CapturingGatewayRelayTransport {
    async fn execute_tool(
        &self,
        request: ToolExecutionRequest,
        binding: astra_runtime_env::RunBinding,
    ) -> Result<astra_runtime_env::RuntimeToolOutcome, astra_runtime_env::RuntimeError> {
        *self.args.lock().expect("captured gateway args lock") = Some(request.args.clone());
        Ok(runtime_outcome_for_request(
            &request,
            &binding,
            "captured-gateway",
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
impl ExternalTransport for PendingGatewayRelayTransport {
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
    result_status: &'static str,
}

impl Default for StaticEdgeDispatch {
    fn default() -> Self {
        Self {
            inserted_edge_agent_ids: Mutex::new(Vec::new()),
            failed_dispatches: Mutex::new(Vec::new()),
            return_result: true,
            result_status: "completed",
        }
    }
}

impl StaticEdgeDispatch {
    fn no_result() -> Self {
        Self {
            inserted_edge_agent_ids: Mutex::new(Vec::new()),
            failed_dispatches: Mutex::new(Vec::new()),
            return_result: false,
            result_status: "completed",
        }
    }

    fn legacy_failed_result() -> Self {
        Self {
            inserted_edge_agent_ids: Mutex::new(Vec::new()),
            failed_dispatches: Mutex::new(Vec::new()),
            return_result: true,
            result_status: "failed",
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
    ) -> Result<(), String> {
        self.inserted_edge_agent_ids
            .lock()
            .expect("inserted edge agent ids lock")
            .push(edge_agent_id.to_string());
        Ok(())
    }

    async fn poll_pending(
        &self,
        _user_id: &str,
        _edge_agent_id: &str,
    ) -> Result<Vec<astra_services::multi_agent::EdgeDispatchRow>, String> {
        Ok(Vec::new())
    }

    async fn deliver_result(
        &self,
        _user_id: &str,
        _request_id: &str,
        _edge_agent_id: &str,
        _result_json: &str,
    ) -> Result<bool, String> {
        Ok(true)
    }

    async fn fail_dispatch(
        &self,
        _user_id: &str,
        request_id: &str,
        reason: &str,
    ) -> Result<bool, String> {
        self.failed_dispatches
            .lock()
            .expect("failed dispatches lock")
            .push((request_id.to_string(), reason.to_string()));
        Ok(true)
    }

    async fn wait_result(
        &self,
        _user_id: &str,
        request_id: &str,
        _timeout: std::time::Duration,
    ) -> Result<Option<String>, String> {
        if !self.return_result {
            return Ok(None);
        }
        let result = astra_thin_client::ToolResultRequest::new_with_hash(
            request_id.to_string(),
            Some("edge-selected".to_string()),
            self.result_status.to_string(),
            if self.result_status == "completed" {
                "ledger-result".to_string()
            } else {
                "edge dispatch expired".to_string()
            },
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
    ) -> Result<(), String> {
        self.inserted_edge_agent_ids
            .lock()
            .expect("inserted edge agent ids lock")
            .push(edge_agent_id.to_string());
        Ok(())
    }

    async fn poll_pending(
        &self,
        _user_id: &str,
        _edge_agent_id: &str,
    ) -> Result<Vec<astra_services::multi_agent::EdgeDispatchRow>, String> {
        Ok(Vec::new())
    }

    async fn deliver_result(
        &self,
        _user_id: &str,
        _request_id: &str,
        _edge_agent_id: &str,
        _result_json: &str,
    ) -> Result<bool, String> {
        Ok(true)
    }

    async fn fail_dispatch(
        &self,
        _user_id: &str,
        request_id: &str,
        reason: &str,
    ) -> Result<bool, String> {
        self.failed_dispatches
            .lock()
            .expect("failed dispatches lock")
            .push((request_id.to_string(), reason.to_string()));
        Ok(true)
    }

    async fn wait_result(
        &self,
        _user_id: &str,
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

#[derive(Clone, Debug)]
struct SharedNoStickyDispatchRow {
    user_id: String,
    edge_agent_id: String,
    request_id: String,
    payload_json: String,
    result_json: Option<String>,
    status: String,
}

#[derive(Default)]
struct SharedNoStickyEdgeDispatch {
    rows: Mutex<HashMap<(String, String), SharedNoStickyDispatchRow>>,
    inserted: tokio::sync::Notify,
    terminal: tokio::sync::Notify,
}

impl SharedNoStickyEdgeDispatch {
    async fn wait_for_insert(&self) {
        loop {
            if !self.rows.lock().expect("shared dispatch rows").is_empty() {
                return;
            }
            self.inserted.notified().await;
        }
    }

    fn status_for(&self, user_id: &str, request_id: &str) -> Option<String> {
        self.rows
            .lock()
            .expect("shared dispatch rows")
            .get(&(user_id.to_string(), request_id.to_string()))
            .map(|row| row.status.clone())
    }
}

#[async_trait]
impl astra_services::multi_agent::EdgeDispatchService for SharedNoStickyEdgeDispatch {
    async fn insert_dispatch(
        &self,
        user_id: &str,
        edge_agent_id: &str,
        request_id: &str,
        payload_json: &str,
    ) -> Result<(), String> {
        let mut rows = self.rows.lock().expect("shared dispatch rows");
        rows.entry((user_id.to_string(), request_id.to_string()))
            .or_insert_with(|| SharedNoStickyDispatchRow {
                user_id: user_id.to_string(),
                edge_agent_id: edge_agent_id.to_string(),
                request_id: request_id.to_string(),
                payload_json: payload_json.to_string(),
                result_json: None,
                status: "pending".to_string(),
            });
        drop(rows);
        self.inserted.notify_waiters();
        Ok(())
    }

    async fn poll_pending(
        &self,
        user_id: &str,
        edge_agent_id: &str,
    ) -> Result<Vec<astra_services::multi_agent::EdgeDispatchRow>, String> {
        let mut rows = self.rows.lock().expect("shared dispatch rows");
        let mut claimed = Vec::new();
        for row in rows.values_mut() {
            if row.user_id == user_id
                && row.edge_agent_id == edge_agent_id
                && row.status == "pending"
            {
                row.status = "dispatched".to_string();
                claimed.push(astra_services::multi_agent::EdgeDispatchRow {
                    user_id: row.user_id.clone(),
                    edge_agent_id: row.edge_agent_id.clone(),
                    request_id: row.request_id.clone(),
                    payload_json: row.payload_json.clone(),
                    result_json: row.result_json.clone(),
                    status: row.status.clone(),
                    pending_wait_us: 0,
                });
            }
        }
        Ok(claimed)
    }

    async fn deliver_result(
        &self,
        user_id: &str,
        request_id: &str,
        edge_agent_id: &str,
        result_json: &str,
    ) -> Result<bool, String> {
        let mut rows = self.rows.lock().expect("shared dispatch rows");
        let Some(row) = rows.get_mut(&(user_id.to_string(), request_id.to_string())) else {
            return Ok(false);
        };
        if row.edge_agent_id != edge_agent_id
            || !matches!(row.status.as_str(), "pending" | "dispatched")
        {
            return Ok(false);
        }
        row.status = "completed".to_string();
        row.result_json = Some(result_json.to_string());
        drop(rows);
        self.terminal.notify_waiters();
        Ok(true)
    }

    async fn fail_dispatch(
        &self,
        user_id: &str,
        request_id: &str,
        reason: &str,
    ) -> Result<bool, String> {
        let mut rows = self.rows.lock().expect("shared dispatch rows");
        let Some(row) = rows.get_mut(&(user_id.to_string(), request_id.to_string())) else {
            return Ok(false);
        };
        if !matches!(row.status.as_str(), "pending" | "dispatched") {
            return Ok(false);
        }
        row.status = "failed".to_string();
        row.result_json = Some(
            serde_json::json!({
                "request_id": request_id,
                "status": "failed",
                "output": format!("edge dispatch {reason}"),
                "duration_ms": 0,
            })
            .to_string(),
        );
        drop(rows);
        self.terminal.notify_waiters();
        Ok(true)
    }

    async fn wait_result(
        &self,
        user_id: &str,
        request_id: &str,
        timeout: std::time::Duration,
    ) -> Result<Option<String>, String> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            {
                let rows = self.rows.lock().expect("shared dispatch rows");
                let Some(row) = rows.get(&(user_id.to_string(), request_id.to_string())) else {
                    return Ok(None);
                };
                if matches!(row.status.as_str(), "completed" | "failed") {
                    return Ok(row.result_json.clone());
                }
            }
            tokio::select! {
                _ = self.terminal.notified() => {}
                _ = tokio::time::sleep_until(deadline) => return Ok(None),
            }
        }
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
fn route_boundary_preserves_skipped_terminal_status_from_tool_metadata() {
    let service = ToolExecutionService::new_for_test();
    let mut request = request(
        "read_file",
        WorkspaceBinding::server_sandbox("/tmp/astra-workspace"),
        ExecutorBinding::server_local(),
    );
    request.args = serde_json::json!({
        "_tool_call_id": "call-skip",
        "_run_id": "run-1",
        "path": "README.md"
    });
    let boundary = service.route_boundary(request);

    let mut result = astra_tools::ToolResult::text("Duplicate read skipped.".to_string());
    result.metadata = Some(serde_json::Map::from_iter([
        ("status".to_string(), Value::String("skipped".to_string())),
        ("skipped".to_string(), Value::Bool(true)),
    ]));
    boundary.attach_binding_metadata(&mut result, service.tool_registry());

    let transport_event = boundary
        .transport_finished_event(&result, 0)
        .expect("transport completed event");
    assert_eq!(transport_event["type"], "tool_transport_completed");
    assert_eq!(transport_event["status"], "skipped");
    assert_eq!(transport_event["skipped"], true);
    assert_eq!(transport_event["success"], true);

    let end_event = boundary
        .tool_call_end_event(&result, 0)
        .expect("tool call end event");
    assert_eq!(end_event["type"], "tool_call_end");
    assert_eq!(end_event["call_id"], "call-skip");
    assert_eq!(end_event["status"], "skipped");
    assert_eq!(end_event["skipped"], true);
    assert_eq!(end_event["success"], true);
    assert_eq!(end_event["result"], "Duplicate read skipped.");
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
async fn local_transport_receives_args_without_internal_tool_metadata() {
    let service = ToolExecutionService::new_for_test();
    let local = CapturingLocalTransport::new();
    let mut request = request(
        "bash",
        WorkspaceBinding::server_sandbox("/tmp/astra-workspace"),
        ExecutorBinding::server_local(),
    );
    request.args = serde_json::json!({
        "command": "pwd",
        "_tool_call_id": "call-1",
        "_run_id": "run-1",
    });

    let result = service.execute(request, &local).await;

    assert!(!result.is_error, "{result:?}");
    assert_eq!(local.args(), serde_json::json!({"command": "pwd"}));
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
async fn no_file_environment_local_code_blocks_without_server_reroute() {
    let service = ToolExecutionService::new_for_test();
    let local = CountingLocalTransport::new();
    let result = service
        .execute(
            request(
                "bash",
                WorkspaceBinding {
                    kind: WorkspaceBindingKind::None,
                    display_name: "No file environment".to_string(),
                    cwd: None,
                    authority: WorkspaceAuthority::None,
                },
                ExecutorBinding::server_local(),
            ),
            &local,
        )
        .await;

    assert!(result.is_error, "{result:?}");
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
    assert!(
        result.metadata.is_none(),
        "unknown tool is a schema/admission failure, not a runtime capability denial"
    );
    let body: Value = serde_json::from_str(&result.output).expect("json error body");
    assert_eq!(
        body["error_kind"],
        serde_json::json!(astra_core::ErrorKind::ToolNotFound.as_str())
    );
    assert_eq!(body["retryable"], serde_json::json!(false));
    assert_eq!(local.calls(), 0);
}

#[tokio::test]
async fn client_only_and_intercepted_tools_do_not_leak_to_server_local_transport() {
    let service = ToolExecutionService::new_for_test();
    let local = CountingLocalTransport::new();

    for tool in ["lsp", "powershell", "skill"] {
        let result = service
            .execute(
                request(
                    tool,
                    WorkspaceBinding::server_sandbox("/tmp/astra-workspace"),
                    ExecutorBinding::server_local(),
                ),
                &local,
            )
            .await;

        assert!(result.is_error, "{tool}: {result:?}");
        assert_eq!(local.calls(), 0, "{tool} must not call local transport");
    }
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
fn no_file_environment_binding_resolves_to_control_plane_tool_surface_only() {
    let registry = astra_runtime_env::ToolRegistry::builtins();
    let request = request(
        "bash",
        WorkspaceBinding {
            kind: WorkspaceBindingKind::None,
            display_name: "No file environment".to_string(),
            cwd: None,
            authority: WorkspaceAuthority::None,
        },
        ExecutorBinding::server_local(),
    );

    let binding = request.runtime_environment_binding(&registry);

    assert!(binding.tool_surface.contains("ask_user"));
    assert!(binding.tool_surface.contains("tool_search"));
    assert!(binding.tool_surface.contains("enter_plan_mode"));
    assert!(binding.tool_surface.contains("exit_plan_mode"));
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
async fn no_file_environment_mcp_retrieve_runs_as_request_scoped_mcp_without_runtime() {
    let service = ToolExecutionService::new_for_test();
    let local = CountingLocalTransport::new();
    let request = request(
        "mcp__rag__retrieve",
        WorkspaceBinding {
            kind: WorkspaceBindingKind::None,
            display_name: "No file environment".to_string(),
            cwd: None,
            authority: WorkspaceAuthority::None,
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

    let result = plan.delivered_result_with_fields(
        "ok".to_string(),
        false,
        ToolTransportKind::EdgeLedger,
        None,
    );
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
fn orchestrator_managed_unknown_status_hides_project_tools_until_runtime_ready() {
    let registry = astra_runtime_env::ToolRegistry::builtins();
    let request = request(
        "read_file",
        WorkspaceBinding {
            kind: WorkspaceBindingKind::CloudWorkspace,
            display_name: "Snapshot".to_string(),
            cwd: Some("/snapshot".to_string()),
            authority: WorkspaceAuthority::ReadOnly,
        },
        ExecutorBinding {
            kind: ExecutorBindingKind::OrchestratorManaged,
            executor_id: "orchestrator:snapshot".to_string(),
            display_name: "Orchestrator-managed executor".to_string(),
            transport: ToolTransportKind::SandboxResidentAgent,
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
fn orchestrator_managed_online_derives_provider_runtime_capabilities() {
    let registry = astra_runtime_env::ToolRegistry::builtins();
    let request = request(
        "read_file",
        WorkspaceBinding::cloud_workspace("/workspace/project", WorkspaceAuthority::ReadWrite),
        ExecutorBinding {
            kind: ExecutorBindingKind::OrchestratorManaged,
            executor_id: "orchestrator:snapshot".to_string(),
            display_name: "Orchestrator-managed executor".to_string(),
            transport: ToolTransportKind::SandboxResidentAgent,
            status: ExecutorStatus::Online,
        },
    );

    let binding = request.runtime_environment_binding(&registry);

    assert_eq!(
        binding.runtime.session_manager,
        astra_runtime_env::RuntimeSessionManager::ProviderManaged,
        "online orchestrator-managed executor derives ProviderManaged session"
    );
    assert_eq!(
        binding.runtime.isolation_backend,
        astra_runtime_env::RuntimeIsolationBackend::ProviderManaged,
        "online orchestrator-managed executor derives ProviderManaged isolation"
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
            binding.tool_surface.contains(tool),
            "{tool} must be available with ProviderManaged runtime"
        );
    }
}

#[tokio::test]
async fn orchestrator_managed_without_transport_returns_transport_unavailable() {
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
                    kind: ExecutorBindingKind::OrchestratorManaged,
                    executor_id: "orchestrator:snapshot".to_string(),
                    display_name: "Orchestrator-managed executor".to_string(),
                    transport: ToolTransportKind::SandboxResidentAgent,
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
        "orchestrator-managed calls must not fall back locally"
    );
    let metadata = result.metadata.expect("transport metadata");
    assert_eq!(metadata["error_kind"], "transport_unavailable");
    assert_eq!(metadata["reason"], "transport_unavailable");
    assert_eq!(metadata["blocked"], true);
    assert_eq!(metadata["execution_started"], false);
    assert_eq!(metadata["runtime_error"]["kind"], "transport_unavailable");
    assert!(
        metadata["runtime_error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("sandbox resident agent transport adapter unavailable")
    );
}

#[test]
fn orchestrator_managed_with_ready_runtime_routes_through_resident_agent() {
    let registry = astra_runtime_env::ToolRegistry::builtins();
    let mut request = request(
        "read_file",
        WorkspaceBinding {
            kind: WorkspaceBindingKind::CloudWorkspace,
            display_name: "Personal workspace".to_string(),
            cwd: Some("/workspace/personal".to_string()),
            authority: WorkspaceAuthority::ReadOnly,
        },
        ExecutorBinding {
            kind: ExecutorBindingKind::OrchestratorManaged,
            executor_id: "orchestrator:personal-1".to_string(),
            display_name: "Orchestrator-managed executor".to_string(),
            transport: ToolTransportKind::SandboxResidentAgent,
            status: ExecutorStatus::Online,
        },
    );
    request.runtime = Some(astra_runtime_env::RuntimeBinding::gvisor(
        "personal-runtime",
    ));

    let binding = request.runtime_environment_binding(&registry);

    assert_eq!(
        binding.executor.kind,
        astra_runtime_env::ExecutorBindingKind::OrchestratorManaged
    );
    assert!(binding.tool_surface.contains("read_file"));
    assert_eq!(
        ToolExecutionService::new_for_test().routing_decision(&request),
        ToolExecutionRouteKind::SandboxResidentAgent
    );
}

#[test]
fn orchestrator_managed_enterprise_binding_preserves_executor_kind() {
    let registry = astra_runtime_env::ToolRegistry::builtins();
    let mut request = request(
        "bash",
        WorkspaceBinding {
            kind: WorkspaceBindingKind::CloudWorkspace,
            display_name: "Team workspace".to_string(),
            cwd: Some("/workspace/team".to_string()),
            authority: WorkspaceAuthority::ReadWrite,
        },
        ExecutorBinding {
            kind: ExecutorBindingKind::OrchestratorManaged,
            executor_id: "orchestrator:enterprise-1".to_string(),
            display_name: "Orchestrator-managed executor".to_string(),
            transport: ToolTransportKind::SandboxResidentAgent,
            status: ExecutorStatus::Online,
        },
    );
    request.runtime = Some(astra_runtime_env::RuntimeBinding::gvisor(
        "enterprise-runtime",
    ));

    let binding = request.runtime_environment_binding(&registry);

    assert_eq!(
        binding.executor.kind,
        astra_runtime_env::ExecutorBindingKind::OrchestratorManaged
    );
    assert!(binding.tool_surface.contains("bash"));
    assert_eq!(
        ToolExecutionService::new_for_test().routing_decision(&request),
        ToolExecutionRouteKind::SandboxResidentAgent
    );
}

#[test]
fn cloud_workspace_with_runtime_bound_orchestrator_exposes_read_write_project_tools() {
    let registry = astra_runtime_env::ToolRegistry::builtins();
    let mut request = request(
        "bash",
        WorkspaceBinding {
            kind: WorkspaceBindingKind::CloudWorkspace,
            display_name: "Team workspace".to_string(),
            cwd: Some("/cloud/volumes/team-volume-1".to_string()),
            authority: WorkspaceAuthority::ReadWrite,
        },
        ExecutorBinding {
            kind: ExecutorBindingKind::OrchestratorManaged,
            executor_id: "orchestrator:workspace-1".to_string(),
            display_name: "Orchestrator-managed executor".to_string(),
            transport: ToolTransportKind::SandboxResidentAgent,
            status: ExecutorStatus::Online,
        },
    );
    request.runtime = Some(astra_runtime_env::RuntimeBinding::oci_container(
        "orchestrator-runtime",
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
        },
        ExecutorBinding {
            kind: ExecutorBindingKind::OrchestratorManaged,
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
        },
        ExecutorBinding {
            kind: ExecutorBindingKind::OrchestratorManaged,
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
    assert_eq!(
        metadata["next_action"],
        "change_workspace_executor_runtime_or_policy"
    );
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
        },
        ExecutorBinding {
            kind: ExecutorBindingKind::OrchestratorManaged,
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
async fn gateway_relay_receives_args_without_internal_tool_metadata() {
    let gateway = Arc::new(CapturingGatewayRelayTransport::new());
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
        },
        ExecutorBinding {
            kind: ExecutorBindingKind::OrchestratorManaged,
            executor_id: "openshell-gateway".to_string(),
            display_name: "OpenShell Gateway".to_string(),
            transport: ToolTransportKind::GatewayRelay,
            status: ExecutorStatus::Online,
        },
    );
    request.runtime = Some(astra_runtime_env::RuntimeBinding::nvidia_openshell(
        "openshell-runtime",
    ));
    request.args = serde_json::json!({
        "command": "pwd",
        "_tool_call_id": "call-gateway",
        "_run_id": "run-gateway",
    });

    let result = service.execute(request, &local).await;

    assert!(!result.is_error, "{result:?}");
    assert_eq!(local.calls(), 0);
    assert_eq!(gateway.args(), serde_json::json!({"command": "pwd"}));
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
            kind: ExecutorBindingKind::OrchestratorManaged,
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
            kind: ExecutorBindingKind::OrchestratorManaged,
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
        },
        ExecutorBinding {
            kind: ExecutorBindingKind::OrchestratorManaged,
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
    assert_eq!(
        metadata["next_action"],
        "change_workspace_executor_runtime_or_policy"
    );
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
        },
        ExecutorBinding {
            kind: ExecutorBindingKind::OrchestratorManaged,
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

fn cloud_snapshot_request(tool_name: &str) -> ToolExecutionRequest {
    let mut request = request(
        tool_name,
        WorkspaceBinding {
            kind: WorkspaceBindingKind::CloudWorkspace,
            display_name: "Snapshot".to_string(),
            cwd: Some("/snapshot".to_string()),
            authority: WorkspaceAuthority::ReadOnly,
        },
        ExecutorBinding {
            kind: ExecutorBindingKind::OrchestratorManaged,
            executor_id: "orchestrator:snapshot-1".to_string(),
            display_name: "Orchestrator-managed executor".to_string(),
            transport: ToolTransportKind::SandboxResidentAgent,
            status: ExecutorStatus::Online,
        },
    );
    request.workspace_record = Some(cloud_snapshot_workspace_record());
    request.runtime = Some(astra_runtime_env::RuntimeBinding::kubernetes(
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
        },
        ExecutorBinding {
            kind: ExecutorBindingKind::OrchestratorManaged,
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

fn cloud_snapshot_workspace_record() -> astra_runtime_env::WorkspaceRecord {
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

#[tokio::test]
async fn orchestrator_managed_executes_through_sandbox_resident_agent_transport() {
    let resident = Arc::new(StaticSandboxResidentAgentTransport::new());
    let _local = CountingLocalTransport::new();

    let service = ToolExecutionService::builder()
        .sandbox_resident_agent_transport(resident.clone())
        .build();
    let local = CountingLocalTransport::new();

    let result = service
        .execute(cloud_snapshot_request("read_file"), &local)
        .await;

    assert!(!result.is_error, "{result:?}");
    assert_eq!(result.output, "resident-agent-result");
    assert_eq!(local.calls(), 0);
    assert_eq!(resident.calls(), 1);
    let metadata = result.metadata.expect("resident metadata");
    assert_eq!(metadata["transport"], "sandbox_resident_agent");
    assert_eq!(metadata["executor"]["kind"], "orchestrator_managed");
    assert_eq!(
        metadata[astra_runtime_env::TOOL_RESULT_RUNTIME_SESSION]["executor_id"],
        "orchestrator:snapshot-1"
    );
    assert_eq!(metadata["runtime"]["launch_driver"], "kubernetes");
}

#[tokio::test]
async fn orchestrator_managed_oversized_output_is_blocked_at_transport_boundary() {
    let oversized = "x".repeat(1_048_577);
    let resident = Arc::new(StaticSandboxResidentAgentTransport::with_output(oversized));
    let _local = CountingLocalTransport::new();

    let service = ToolExecutionService::builder()
        .sandbox_resident_agent_transport(resident.clone())
        .build();
    let local = CountingLocalTransport::new();

    let result = service
        .execute(cloud_snapshot_request("read_file"), &local)
        .await;

    assert!(result.is_error, "{result:?}");
    assert!(result.output.contains("output limit exceeded"));
    assert!(!result.output.contains(&"x".repeat(128)));
    assert_eq!(local.calls(), 0);
    assert_eq!(resident.calls(), 1);
    let metadata = result.metadata.expect("output limit metadata");
    assert_eq!(metadata["error_kind"], "output_limit_exceeded");
    assert_eq!(metadata["execution_started"], true);
    assert_eq!(metadata["side_effects_maybe"], true);
    assert_eq!(metadata["output_bytes"], 1_048_577);
    assert_eq!(metadata["max_output_bytes"], 1_048_576);
    assert_eq!(metadata["transport"], "sandbox_resident_agent");
}

#[tokio::test]
async fn orchestrator_managed_uses_policy_snapshot_output_limit_override() {
    let resident = Arc::new(StaticSandboxResidentAgentTransport::with_output("abcd"));
    let _local = CountingLocalTransport::new();
    let _request = cloud_snapshot_request("read_file");
    let service = ToolExecutionService::builder()
        .sandbox_resident_agent_transport(resident.clone())
        .build();
    let local = CountingLocalTransport::new();
    let mut request = cloud_snapshot_request("read_file");
    request.policy.max_output_bytes = Some(3);

    let result = service.execute(request, &local).await;

    assert!(result.is_error, "{result:?}");
    assert!(result.output.contains("output limit exceeded"));
    assert_eq!(local.calls(), 0);
    assert_eq!(resident.calls(), 1);
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
async fn orchestrator_managed_execute_timeout_reports_side_effect_uncertainty() {
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let resident = Arc::new(PendingSandboxResidentAgentTransport::new(started_tx));
    let _request = cloud_snapshot_request("read_file");

    let service = ToolExecutionService::builder()
        .sandbox_resident_agent_transport(resident.clone())
        .build();
    let request = cloud_snapshot_request("read_file");

    let handle = tokio::spawn(async move {
        let local = CountingLocalTransport::new();
        let result = service.execute(request, &local).await;
        (result, local.calls())
    });

    started_rx.await.expect("resident execute should start");
    tokio::time::advance(std::time::Duration::from_secs(31)).await;
    let (result, local_calls) = handle
        .await
        .expect("resident timeout task should not panic");

    assert!(result.is_error, "{result:?}");
    assert!(
        result.output.contains("max_execution_secs"),
        "{}",
        result.output
    );
    assert_eq!(local_calls, 0);
    assert_eq!(resident.calls(), 1);
    let metadata = result.metadata.expect("resident timeout metadata");
    assert_eq!(metadata["error_kind"], TOOL_ERROR_KIND_TOOL_TIMEOUT);
    assert_eq!(metadata["reason"], TOOL_ERROR_KIND_TOOL_TIMEOUT);
    assert_eq!(metadata["blocked"], true);
    assert_eq!(metadata["execution_started"], true);
    assert_eq!(metadata["side_effects_maybe"], true);
    assert_eq!(metadata["next_action"], "inspect_effects_before_retry");
    assert_eq!(metadata["max_execution_secs"], 30.0);
    assert_eq!(metadata["transport"], "sandbox_resident_agent");
    assert_eq!(metadata["runtime_error"]["kind"], "tool_timeout");
}

#[tokio::test]
async fn orchestrator_managed_without_sandbox_resident_agent_transport_does_not_reroute_to_local() {
    let service = ToolExecutionService::new_for_test();
    let local = CountingLocalTransport::new();

    let result = service
        .execute(cloud_snapshot_request("read_file"), &local)
        .await;

    assert!(result.is_error, "{result:?}");
    assert_eq!(local.calls(), 0);
    let metadata = result.metadata.expect("resident agent transport metadata");
    assert_eq!(
        metadata["error_kind"],
        astra_runtime_env::RuntimeErrorKind::TransportUnavailable.to_string()
    );
    assert_eq!(metadata["blocked"], true);
    assert_eq!(metadata["retryable"], true);
    assert_eq!(metadata["execution_started"], false);
    assert_eq!(metadata["side_effects_maybe"], false);
    assert_eq!(
        metadata["next_action"],
        "change_workspace_executor_runtime_or_policy"
    );
    assert_eq!(metadata["runtime_error"]["kind"], "transport_unavailable");
    assert_eq!(metadata["transport"], "sandbox_resident_agent");
}

#[tokio::test]
async fn orchestrator_managed_transport_error_skips_local_reroute() {
    let resident = Arc::new(StaticSandboxResidentAgentTransport::with_error(
        astra_runtime_env::RuntimeError::runtime_unavailable("orchestrator denied execution"),
    ));
    let service = ToolExecutionService::builder()
        .sandbox_resident_agent_transport(resident.clone())
        .build();
    let local = CountingLocalTransport::new();

    let result = service
        .execute(cloud_snapshot_request("read_file"), &local)
        .await;

    assert!(result.is_error, "{result:?}");
    assert_eq!(local.calls(), 0);
    assert_eq!(resident.calls(), 4); // 1 initial + 3 retries (runtime_unavailable is retryable)
    let metadata = result.metadata.expect("resident error metadata");
    assert_eq!(metadata["error_kind"], "runtime_unavailable");
    assert_eq!(metadata["transport"], "sandbox_resident_agent");
    assert_eq!(metadata["blocked"], true);
    assert!(
        metadata["runtime_error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("orchestrator denied execution"),
        "unexpected message: {:?}",
        metadata["runtime_error"]["message"]
    );
}

#[tokio::test]
async fn cloud_workspace_blocks_without_server_reroute() {
    let service = ToolExecutionService::new_for_test();
    let local = CountingLocalTransport::new();
    let mut request = request(
        "git",
        WorkspaceBinding {
            kind: WorkspaceBindingKind::CloudWorkspace,
            display_name: "Cloud workspace".to_string(),
            cwd: Some("/checkout/repo".to_string()),
            authority: WorkspaceAuthority::ReadOnly,
        },
        ExecutorBinding::server_local(),
    );
    request.args = serde_json::json!({"action": "status"});

    let result = service.execute(request, &local).await;

    assert!(result.is_error, "{result:?}");
    assert!(
        result
            .output
            .contains("No alternate execution provider was attempted"),
        "{}",
        result.output
    );
    assert!(
        result
            .output
            .contains("workspace provider with an available executor"),
        "{}",
        result.output
    );
    assert!(
        !result.output.contains("Select Server sandbox"),
        "{}",
        result.output
    );
    assert!(
        !result.output.contains("connected edge workspace"),
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
async fn edge_offline_does_not_call_server_local() {
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
        result.output.contains("No alternate execution provider"),
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
async fn edge_ws_result_preserves_tool_result_fields() {
    let pool = astra_server_types::edge_connection_pool::EdgeConnectionPool::new();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<astra_server_types::EdgeServerMessage>(1);
    pool.register_with_capabilities(
        "user-1",
        "edge-selected",
        Some("MacBook Pro".to_string()),
        Some("/Users/test/project".to_string()),
        Some(edge_runtime_environment_advertisement("edge-selected")),
        tx,
    );
    let service = ToolExecutionService::builder()
        .edge_connection_pool(pool.clone())
        .build();
    let request = request(
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
    );
    let handle = tokio::spawn(async move {
        let local = CountingLocalTransport::new();
        service.execute(request, &local).await
    });

    let message = rx.recv().await.expect("edge tool request");
    let request_id = match message {
        astra_server_types::EdgeServerMessage::ToolRequest { request_id, .. } => request_id,
        other => panic!("expected tool request, got {other:?}"),
    };
    let mut fields = serde_json::Map::new();
    fields.insert("exit_code".to_string(), serde_json::json!(7));
    fields.insert(
        "result_class".to_string(),
        serde_json::json!("execution_error"),
    );
    assert!(pool.deliver_tool_result(
        "user-1",
        "edge-selected",
        &request_id,
        astra_server_types::edge_connection_pool::EdgeToolResult {
            output: "failed".to_string(),
            is_error: true,
            duration_ms: Some(5),
            tool_result_fields: Some(fields),
        },
    ));

    let result = handle.await.expect("edge execution join");
    assert!(result.is_error, "{result:?}");
    assert_eq!(result.output, "failed");
    let metadata = result.metadata.expect("edge ws metadata");
    assert_eq!(metadata["transport"], "edge_ws");
    assert_eq!(metadata["exit_code"], 7);
    assert_eq!(metadata["result_class"], "execution_error");
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
async fn edge_dispatch_legacy_failed_result_reports_tool_error() {
    let dispatch = Arc::new(StaticEdgeDispatch::legacy_failed_result());
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
    assert_eq!(result.output, "edge dispatch expired");
    let metadata = result.metadata.expect("ledger metadata");
    assert_eq!(metadata["transport"], "edge_ledger");
    assert_eq!(metadata["executor"]["transport"], "edge_ledger");
    assert_eq!(metadata["executor"]["status"], "online");
    assert_eq!(metadata["workspace"]["kind"], "edge_workspace");
    assert_eq!(local.calls(), 0);
}

#[tokio::test]
async fn edge_dispatch_waiter_poller_and_callback_do_not_require_sticky_pod() {
    let dispatch = Arc::new(SharedNoStickyEdgeDispatch::default());
    let service = ToolExecutionService::builder()
        .edge_dispatch_service(dispatch.clone())
        .edge_registry_service(Arc::new(StaticEdgeRegistry {
            agents: vec![edge_agent_record("edge-selected")],
        }))
        .build();
    let request = request(
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
    );

    let waiter = tokio::spawn(async move {
        let local = CountingLocalTransport::new();
        let result = service.execute(request, &local).await;
        (result, local.calls())
    });

    let edge_ws_pod = dispatch.clone();
    let claimed_request_id = tokio::time::timeout(std::time::Duration::from_secs(1), async move {
        edge_ws_pod.wait_for_insert().await;
        let rows = astra_services::multi_agent::EdgeDispatchService::poll_pending(
            edge_ws_pod.as_ref(),
            "user-1",
            "edge-selected",
        )
        .await
        .expect("edge WS pod should claim pending dispatch");
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.user_id, "user-1");
        assert_eq!(row.edge_agent_id, "edge-selected");
        assert_eq!(row.status, "dispatched");
        let message: astra_server_types::edge_ws_protocol::EdgeServerMessage =
            serde_json::from_str(&row.payload_json).expect("dispatch payload should be WS message");
        match message {
            astra_server_types::edge_ws_protocol::EdgeServerMessage::ToolRequest {
                request_id,
                tool,
                args,
                timeout_secs,
            } => {
                assert_eq!(request_id, row.request_id);
                assert_eq!(tool, "bash");
                assert_eq!(args, serde_json::json!({}));
                assert!(timeout_secs > 0);
            }
            other => panic!("expected tool request payload, got {other:?}"),
        }
        row.request_id.clone()
    })
    .await
    .expect("edge WS pod should poll pending dispatch before timeout");
    assert_eq!(
        dispatch
            .status_for("user-1", &claimed_request_id)
            .as_deref(),
        Some("dispatched")
    );

    let tool_result = astra_thin_client::ToolResultRequest::new_with_hash(
        claimed_request_id.clone(),
        Some("edge-selected".to_string()),
        "completed".to_string(),
        "no-sticky-result".to_string(),
        9,
    );
    let result_json =
        serde_json::to_string(&tool_result).expect("tool result should serialize for callback pod");
    let delivered = astra_services::multi_agent::EdgeDispatchService::deliver_result(
        dispatch.as_ref(),
        "user-1",
        &claimed_request_id,
        "edge-selected",
        &result_json,
    )
    .await
    .expect("callback pod should deliver result");
    assert!(delivered);

    let (result, local_calls) = tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
        .await
        .expect("waiter pod should observe delivered dispatch result")
        .expect("waiter task should not panic");
    assert!(!result.is_error, "{result:?}");
    assert_eq!(result.output, "no-sticky-result");
    assert_eq!(local_calls, 0);
    let metadata = result.metadata.expect("edge ledger metadata");
    assert_eq!(metadata["transport"], "edge_ledger");
    assert_eq!(metadata["executor"]["transport"], "edge_ledger");
    assert_eq!(metadata["executor"]["executor_id"], "edge-selected");
    assert_eq!(
        dispatch
            .status_for("user-1", &claimed_request_id)
            .as_deref(),
        Some("completed")
    );
}

#[tokio::test]
async fn edge_bound_offline_or_unknown_status_blocks_without_dispatch() {
    for status in [ExecutorStatus::Offline, ExecutorStatus::Unknown] {
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
                        status,
                    ),
                ),
                &local,
            )
            .await;

        assert!(result.is_error, "{status:?}: {result:?}");
        let metadata = result.metadata.expect("offline metadata");
        assert_eq!(metadata["error_kind"], TOOL_ERROR_KIND_EXECUTOR_OFFLINE);
        assert_eq!(metadata["executor"]["status"], serde_json::json!(status));
        assert_eq!(local.calls(), 0);
        assert!(
            dispatch
                .inserted_edge_agent_ids
                .lock()
                .expect("inserted edge agent ids lock")
                .is_empty(),
            "explicit {status:?} executor status must block before edge ledger dispatch"
        );
    }
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
        "memory",
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
async fn shared_network_tools_use_server_without_runtime_and_edge_with_edge_binding() {
    let dispatch = Arc::new(StaticEdgeDispatch::default());
    let service = ToolExecutionService::builder()
        .edge_dispatch_service(dispatch.clone())
        .edge_registry_service(Arc::new(StaticEdgeRegistry {
            agents: vec![edge_agent_record("edge-selected")],
        }))
        .build();
    let local = CountingLocalTransport::new();

    for tool in ["web_fetch", "web_search"] {
        let server_request = request(
            tool,
            WorkspaceBinding::none(),
            ExecutorBinding::server_local(),
        );
        assert_eq!(
            service.routing_decision(&server_request),
            ToolExecutionRouteKind::ServerRuntime,
            "{tool} must be service-backed when no runtime executor is selected"
        );
        let server_result = service.execute(server_request, &local).await;
        assert!(!server_result.is_error, "{tool}: {server_result:?}");
        assert_eq!(server_result.output, format!("local:{tool}"));

        let edge_request = request(
            tool,
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
        );
        assert_eq!(
            service.routing_decision(&edge_request),
            ToolExecutionRouteKind::EdgeBound,
            "{tool} must prefer the selected edge executor"
        );
        let edge_result = service.execute(edge_request, &local).await;
        assert!(!edge_result.is_error, "{tool}: {edge_result:?}");
        assert_eq!(edge_result.output, "ledger-result");
    }

    assert_eq!(local.calls(), 2);
    assert_eq!(
        dispatch
            .inserted_edge_agent_ids
            .lock()
            .expect("inserted edge agent ids lock")
            .as_slice(),
        ["edge-selected", "edge-selected"]
    );
}

#[tokio::test]
async fn disabled_shared_network_tool_blocks_server_route_not_edge_route() {
    let dispatch = Arc::new(StaticEdgeDispatch::default());
    let service = ToolExecutionService::builder()
        .edge_dispatch_service(dispatch.clone())
        .edge_registry_service(Arc::new(StaticEdgeRegistry {
            agents: vec![edge_agent_record("edge-selected")],
        }))
        .initial_disabled_tool_offers(&["web_fetch@server-builtin".to_string()])
        .build();
    let local = CountingLocalTransport::new();

    let server_result = service
        .execute(
            request(
                "web_fetch",
                WorkspaceBinding::none(),
                ExecutorBinding::server_local(),
            ),
            &local,
        )
        .await;
    assert!(server_result.is_error, "{server_result:?}");
    let metadata = server_result.metadata.expect("disabled metadata");
    assert_eq!(metadata["tool_disabled"], true);
    assert_eq!(metadata["tool_offer_id"], "web_fetch@server-builtin");

    let edge_result = service
        .execute(
            request(
                "web_fetch",
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
    assert!(!edge_result.is_error, "{edge_result:?}");
    assert_eq!(edge_result.output, "ledger-result");
    assert_eq!(local.calls(), 0);
    assert_eq!(
        dispatch
            .inserted_edge_agent_ids
            .lock()
            .expect("inserted edge agent ids lock")
            .as_slice(),
        ["edge-selected"]
    );
}

#[tokio::test]
async fn disabled_tool_name_blocks_every_selected_offer_before_transport() {
    let service = ToolExecutionService::builder()
        .initial_disabled_tool_names(&["web_fetch".to_string()])
        .build();
    let local = CountingLocalTransport::new();

    let result = service
        .execute(
            request(
                "web_fetch",
                WorkspaceBinding::none(),
                ExecutorBinding::server_local(),
            ),
            &local,
        )
        .await;

    assert!(result.is_error, "{result:?}");
    let metadata = result.metadata.expect("disabled metadata");
    assert_eq!(metadata["tool_disabled"], true);
    assert_eq!(metadata["tool_offer_id"], "web_fetch@server-builtin");
    assert_eq!(local.calls(), 0);
}

#[tokio::test]
async fn provider_allowlist_blocks_selected_edge_offer_without_server_reroute() {
    let dispatch = Arc::new(StaticEdgeDispatch::default());
    let service = ToolExecutionService::builder()
        .edge_dispatch_service(dispatch.clone())
        .edge_registry_service(Arc::new(StaticEdgeRegistry {
            agents: vec![edge_agent_record("edge-selected")],
        }))
        .initial_provider_allowed_tools(HashMap::from([(
            "edge-selected".to_string(),
            HashSet::from(["bash".to_string()]),
        )]))
        .build();
    let local = CountingLocalTransport::new();

    let result = service
        .execute(
            request(
                "web_fetch",
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
    let metadata = result.metadata.expect("provider disallowed metadata");
    assert_eq!(metadata["tool_provider_disallowed"], true);
    assert_eq!(metadata["tool_offer_id"], "web_fetch@edge-selected");
    assert_eq!(metadata["provider_id"], "edge-selected");
    assert_eq!(local.calls(), 0);
    assert!(
        dispatch
            .inserted_edge_agent_ids
            .lock()
            .expect("inserted edge agent ids lock")
            .is_empty(),
        "disallowed selected offer must be blocked before edge dispatch"
    );
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

#[tokio::test]
async fn disabled_request_scoped_mcp_offer_blocks_execution_without_schema_inventory() {
    let service = ToolExecutionService::builder()
        .initial_disabled_tool_offers(&["mcp__demo__search@request-scoped-mcp".to_string()])
        .build();
    let local = CountingLocalTransport::new();
    let result = service
        .execute(
            request(
                "mcp__demo__search",
                WorkspaceBinding::none(),
                ExecutorBinding {
                    kind: ExecutorBindingKind::Mcp,
                    executor_id: "request-scoped-mcp".to_string(),
                    display_name: "MCP server".to_string(),
                    transport: ToolTransportKind::McpHttp,
                    status: ExecutorStatus::Online,
                },
            ),
            &local,
        )
        .await;

    assert!(result.is_error, "{result:?}");
    let metadata = result.metadata.expect("disabled metadata");
    assert_eq!(metadata["tool_disabled"], true);
    assert_eq!(
        metadata["tool_offer_id"],
        "mcp__demo__search@request-scoped-mcp"
    );
    assert_eq!(local.calls(), 0);
}

#[tokio::test]
async fn request_scoped_mcp_provider_allowlist_blocks_unlisted_tool_without_schema_inventory() {
    let service = ToolExecutionService::builder()
        .initial_provider_allowed_tools(HashMap::from([(
            "request-scoped-mcp".to_string(),
            HashSet::from(["mcp__demo__allowed".to_string()]),
        )]))
        .build();
    let local = CountingLocalTransport::new();
    let result = service
        .execute(
            request(
                "mcp__demo__search",
                WorkspaceBinding::none(),
                ExecutorBinding {
                    kind: ExecutorBindingKind::Mcp,
                    executor_id: "request-scoped-mcp".to_string(),
                    display_name: "MCP server".to_string(),
                    transport: ToolTransportKind::McpHttp,
                    status: ExecutorStatus::Online,
                },
            ),
            &local,
        )
        .await;

    assert!(result.is_error, "{result:?}");
    let metadata = result.metadata.expect("provider disallowed metadata");
    assert_eq!(metadata["tool_provider_disallowed"], true);
    assert_eq!(
        metadata["tool_offer_id"],
        "mcp__demo__search@request-scoped-mcp"
    );
    assert_eq!(metadata["provider_id"], "request-scoped-mcp");
    assert_eq!(local.calls(), 0);
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
            display_name: "No file environment".to_string(),
            cwd: None,
            authority: WorkspaceAuthority::None,
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
async fn orchestrator_managed_cancel_during_execute_reports_side_effect_uncertainty() {
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let resident = Arc::new(PendingSandboxResidentAgentTransport::new(started_tx));
    let cancel = Arc::new(CancellationToken::new());
    let _cancel_for_task = cancel.clone();
    let _request = cloud_snapshot_request("read_file");

    let service = ToolExecutionService::builder()
        .sandbox_resident_agent_transport(resident.clone())
        .build();
    let cancel = Arc::new(CancellationToken::new());
    let cancel_for_task = cancel.clone();
    let request = cloud_snapshot_request("read_file");

    let handle = tokio::spawn(async move {
        let local = CountingLocalTransport::new();
        let result = service
            .execute_with_cancel(request, &local, Some(cancel_for_task))
            .await;
        (result, local.calls())
    });

    tokio::time::timeout(std::time::Duration::from_secs(1), started_rx)
        .await
        .expect("resident agent execute should start")
        .expect("resident agent execute start signal");
    cancel.cancel();
    let (result, local_calls) = tokio::time::timeout(std::time::Duration::from_secs(1), handle)
        .await
        .expect("resident agent cancel should resolve")
        .expect("resident agent cancel task should not panic");

    assert!(result.is_error, "{result:?}");
    assert!(result.output.contains("cancelled"), "{}", result.output);
    assert_eq!(local_calls, 0);
    assert_eq!(resident.calls(), 1);
    let metadata = result.metadata.expect("resident agent cancel metadata");
    assert_eq!(metadata["error_kind"], TOOL_ERROR_KIND_CANCELLED);
    assert_eq!(metadata["reason"], TOOL_ERROR_KIND_CANCELLED);
    assert_eq!(metadata["cancelled"], true);
    assert_eq!(metadata["blocked"], true);
    assert_eq!(metadata["execution_started"], true);
    assert_eq!(metadata["side_effects_maybe"], true);
    assert_eq!(metadata["next_action"], "inspect_effects_before_retry");
    assert_eq!(metadata["transport"], "sandbox_resident_agent");
    assert_eq!(metadata["executor"]["kind"], "orchestrator_managed");
    assert_eq!(metadata["runtime"]["isolation_backend"], "provider_managed");
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

// ─── ExternalTransport health-check boundary tests ──────────────────────────

/// Transport health states for boundary tests.
#[derive(Clone)]
enum HealthState {
    Healthy,
    Unhealthy(&'static str),
    /// First health_check fails, reconnect succeeds, then healthy.
    Reconnectable(&'static str),
    /// First health_check fails, reconnect also fails.
    Unrecoverable(&'static str, &'static str),
}

struct HealthStateTransport {
    state: Mutex<HealthState>,
    calls: AtomicUsize,
    output: String,
}

impl HealthStateTransport {
    fn new(state: HealthState) -> Self {
        Self {
            state: Mutex::new(state),
            calls: AtomicUsize::new(0),
            output: "health-state-result".to_string(),
        }
    }
}

#[async_trait]
impl ExternalTransport for HealthStateTransport {
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

    async fn health_check(&self) -> Result<(), String> {
        let mut state = self.state.lock().unwrap();
        match *state {
            HealthState::Healthy => Ok(()),
            HealthState::Unhealthy(reason) => Err(reason.to_string()),
            HealthState::Reconnectable(reason) => {
                // After reconnect succeeds, transition to Healthy
                *state = HealthState::Healthy;
                Err(reason.to_string())
            }
            HealthState::Unrecoverable(reason, _) => Err(reason.to_string()),
        }
    }

    async fn reconnect(&self) -> Result<(), String> {
        let state = self.state.lock().unwrap();
        match *state {
            HealthState::Reconnectable(_) => Ok(()),
            HealthState::Unrecoverable(_, reconnect_err) => Err(reconnect_err.to_string()),
            _ => Ok(()),
        }
    }
}

#[tokio::test]
async fn external_transport_health_check_failure_returns_transport_unavailable() {
    let transport = Arc::new(HealthStateTransport::new(HealthState::Unhealthy(
        "connection refused",
    )));
    let service = ToolExecutionService::builder()
        .sandbox_resident_agent_transport(transport)
        .build();
    let local = CountingLocalTransport::new();

    let result = service
        .execute(cloud_snapshot_request("read_file"), &local)
        .await;

    assert!(result.is_error, "{result:?}");
    assert_eq!(local.calls(), 0, "must not fall back locally");
    let metadata = result.metadata.expect("transport metadata");
    assert_eq!(metadata["error_kind"], "transport_unavailable");
    assert!(
        metadata["runtime_error"]["message"]
            .as_str()
            .unwrap()
            .contains("transport is unhealthy")
    );
    assert!(
        metadata["runtime_error"]["message"]
            .as_str()
            .unwrap()
            .contains("connection refused")
    );
}

#[tokio::test]
async fn external_transport_reconnect_after_health_failure_succeeds() {
    let transport = Arc::new(HealthStateTransport::new(HealthState::Reconnectable(
        "stale connection",
    )));
    let service = ToolExecutionService::builder()
        .sandbox_resident_agent_transport(transport.clone())
        .build();
    let local = CountingLocalTransport::new();

    let result = service
        .execute(cloud_snapshot_request("read_file"), &local)
        .await;

    // After reconnect, the transport should be healthy and execute normally.
    assert!(!result.is_error, "{result:?}");
    assert_eq!(result.output, "health-state-result");
    assert_eq!(local.calls(), 0, "must not fall back locally");
}

#[tokio::test]
async fn external_transport_reconnect_failure_returns_transport_unavailable() {
    let transport = Arc::new(HealthStateTransport::new(HealthState::Unrecoverable(
        "connection refused",
        "reconnect failed: timeout",
    )));
    let service = ToolExecutionService::builder()
        .sandbox_resident_agent_transport(transport)
        .build();
    let local = CountingLocalTransport::new();

    let result = service
        .execute(cloud_snapshot_request("read_file"), &local)
        .await;

    assert!(result.is_error, "{result:?}");
    assert_eq!(local.calls(), 0, "must not fall back locally");
    let metadata = result.metadata.expect("transport metadata");
    assert_eq!(metadata["error_kind"], "transport_unavailable");
    assert!(
        metadata["runtime_error"]["message"]
            .as_str()
            .unwrap()
            .contains("transport is unhealthy")
    );
}

// ─── ExternalTransport exit_semantics boundary tests ────────────────────────

#[tokio::test]
async fn external_transport_cancelled_result_carries_execution_error_semantics() {
    let local = CountingLocalTransport::new();
    let cancel_token = CancellationToken::new();
    cancel_token.cancel();

    // Use gateway relay path — it exercises the external route code.
    let transport = Arc::new(StaticGatewayRelayTransport::new());
    let service = ToolExecutionService::builder()
        .gateway_relay_transport(transport)
        .build();

    let result = service
        .execute_with_cancel(
            openshell_gateway_request("bash"),
            &local,
            Some(Arc::new(cancel_token)),
        )
        .await;

    assert!(result.is_error, "{result:?}");
    assert_eq!(
        result.exit_semantics,
        Some(astra_tools::exit_semantics::ExitSemantics::ExecutionError),
        "cancelled transport result must carry ExecutionError exit semantics"
    );
}

#[tokio::test]
async fn external_transport_timeout_result_carries_execution_error_semantics() {
    // Verify that output_limit_exceeded also carries ExecutionError.
    let oversized = "x".repeat(1_048_577);
    let transport = Arc::new(StaticGatewayRelayTransport::with_output(oversized));
    let service = ToolExecutionService::builder()
        .gateway_relay_transport(transport)
        .build();
    let local = CountingLocalTransport::new();

    let mut request = openshell_gateway_request("bash");
    request.policy.max_output_bytes = Some(1024);

    let result = service.execute(request, &local).await;

    assert!(result.is_error, "{result:?}");
    assert_eq!(
        result.exit_semantics,
        Some(astra_tools::exit_semantics::ExitSemantics::ExecutionError),
        "output-limit-exceeded result must carry ExecutionError exit semantics"
    );
}

#[tokio::test]
async fn external_transport_not_configured_result_carries_execution_error_semantics() {
    let service = ToolExecutionService::new_for_test();
    let local = CountingLocalTransport::new();

    // No gateway relay transport configured — must return transport_unavailable.
    let result = service
        .execute(openshell_gateway_request("bash"), &local)
        .await;

    assert!(result.is_error, "{result:?}");
    assert_eq!(
        result.exit_semantics,
        Some(astra_tools::exit_semantics::ExitSemantics::ExecutionError),
        "transport-unavailable result must carry ExecutionError exit semantics"
    );
}

// ─── ExternalTransport retry & recovery boundary tests ──────────────────────

/// A transport that replays a pre-configured sequence of responses.
/// Each call to `execute_tool` consumes the next response in the queue.
struct ReplayTransport {
    responses:
        Mutex<Vec<Result<astra_runtime_env::RuntimeToolOutcome, astra_runtime_env::RuntimeError>>>,
    calls: AtomicUsize,
    healthy: AtomicBool,
}

impl ReplayTransport {
    fn new(
        responses: Vec<
            Result<astra_runtime_env::RuntimeToolOutcome, astra_runtime_env::RuntimeError>,
        >,
    ) -> Self {
        Self {
            responses: Mutex::new(responses),
            calls: AtomicUsize::new(0),
            healthy: AtomicBool::new(true),
        }
    }

    fn with_health(healthy: bool) -> Self {
        Self {
            healthy: AtomicBool::new(healthy),
            ..Self::new(vec![])
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ExternalTransport for ReplayTransport {
    async fn execute_tool(
        &self,
        _request: ToolExecutionRequest,
        _binding: astra_runtime_env::RunBinding,
    ) -> Result<astra_runtime_env::RuntimeToolOutcome, astra_runtime_env::RuntimeError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut responses = self.responses.lock().unwrap();
        if responses.is_empty() {
            return Err(astra_runtime_env::RuntimeError::new(
                astra_runtime_env::RuntimeErrorKind::RuntimeUnavailable,
                "no more responses in replay queue",
            ));
        }
        responses.remove(0)
    }

    async fn health_check(&self) -> Result<(), String> {
        if self.healthy.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err("transport is unhealthy".to_string())
        }
    }
}

/// A transport that blocks forever (simulates a hung connection / timeout).
struct PendingExternalTransport {
    execute_started: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    calls: AtomicUsize,
}

impl PendingExternalTransport {
    fn new(execute_started: tokio::sync::oneshot::Sender<()>) -> Self {
        Self {
            execute_started: Mutex::new(Some(execute_started)),
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl ExternalTransport for PendingExternalTransport {
    async fn execute_tool(
        &self,
        _request: ToolExecutionRequest,
        _binding: astra_runtime_env::RunBinding,
    ) -> Result<astra_runtime_env::RuntimeToolOutcome, astra_runtime_env::RuntimeError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let sender = self.execute_started.lock().unwrap().take();
        if let Some(sender) = sender {
            let _ = sender.send(());
        }
        std::future::pending::<()>().await;
        unreachable!("pending external transport never completes")
    }
}

#[tokio::test]
async fn external_transport_retryable_error_retries_then_succeeds() {
    // Transport returns retryable errors on first 2 calls, success on 3rd.
    let request = cloud_snapshot_request("read_file");
    let binding = astra_runtime_env::RunBinding::resolve(
        astra_runtime_env::WorkspaceBinding::server_sandbox("session-r1"),
        astra_runtime_env::ExecutorBinding::local_cli(),
        astra_runtime_env::RuntimeBinding::host_process("test-host"),
        astra_runtime_env::PolicyIntent::local_developer(),
        &astra_runtime_env::ToolRegistry::default(),
    );
    let success_outcome = runtime_outcome_for_request(&request, &binding, "retry-succeeded");
    let retryable_err = astra_runtime_env::RuntimeError::runtime_unavailable("temporary outage");

    let transport = Arc::new(ReplayTransport::new(vec![
        Err(retryable_err.clone()),
        Err(retryable_err.clone()),
        Ok(success_outcome),
    ]));
    let service = ToolExecutionService::builder()
        .sandbox_resident_agent_transport(transport.clone())
        .build();
    let local = CountingLocalTransport::new();

    let result = service.execute(request, &local).await;

    assert!(
        !result.is_error,
        "retry should eventually succeed: {result:?}"
    );
    assert_eq!(result.output, "retry-succeeded");
    assert_eq!(
        transport.calls(),
        3,
        "should have retried 2 times, total 3 calls"
    );
    assert_eq!(local.calls(), 0, "must not fall back locally");
}

#[tokio::test]
async fn external_transport_retryable_error_exhausts_retries() {
    // Transport always returns retryable errors — all retries exhausted.
    let request = cloud_snapshot_request("read_file");
    let retryable_err = astra_runtime_env::RuntimeError::runtime_unavailable("persistent outage");

    let transport = Arc::new(ReplayTransport::new(vec![
        Err(retryable_err.clone()),
        Err(retryable_err.clone()),
        Err(retryable_err.clone()),
        Err(retryable_err.clone()),
        // 4 calls: initial + retry1 + retry2 + retry3 → exhausted
    ]));
    let service = ToolExecutionService::builder()
        .sandbox_resident_agent_transport(transport.clone())
        .build();
    let local = CountingLocalTransport::new();

    let result = service.execute(request, &local).await;

    assert!(
        result.is_error,
        "all retries exhausted, should error: {result:?}"
    );
    assert_eq!(transport.calls(), 4, "initial + 3 retries = 4 total calls");
    assert!(
        result.output.contains("retried 3 time(s)"),
        "error should mention retry count: {result:?}"
    );
    let metadata = result.metadata.as_ref().expect("retry metadata");
    assert_eq!(metadata["retryable"], true);
    assert_eq!(metadata["execution_started"], false);
    assert_eq!(metadata["side_effects_maybe"], false);
    assert_eq!(
        metadata["next_action"],
        "change_workspace_executor_runtime_or_policy"
    );
    assert_eq!(local.calls(), 0, "must not fall back locally");
    assert_eq!(
        result.exit_semantics,
        Some(astra_tools::exit_semantics::ExitSemantics::ExecutionError),
        "exhausted retries must carry ExecutionError exit semantics"
    );
}

#[tokio::test]
async fn external_transport_retryable_error_after_start_does_not_retry() {
    // A disconnected transport after execution started is retryable at the
    // run/session level, but replaying the same tool call can duplicate
    // side effects. The transport dispatch layer must fail closed and report
    // uncertainty instead of issuing another execute_tool call.
    let request = cloud_snapshot_request("read_file");
    let error = astra_runtime_env::RuntimeError::transport_disconnected("lost after tool start");

    let transport = Arc::new(ReplayTransport::new(vec![
        Err(error),
        Ok(runtime_outcome_for_request(
            &request,
            &astra_runtime_env::RunBinding::resolve(
                astra_runtime_env::WorkspaceBinding::server_sandbox("session-r-side-effect"),
                astra_runtime_env::ExecutorBinding::local_cli(),
                astra_runtime_env::RuntimeBinding::host_process("test-host"),
                astra_runtime_env::PolicyIntent::local_developer(),
                &astra_runtime_env::ToolRegistry::default(),
            ),
            "must-not-run",
        )),
    ]));
    let service = ToolExecutionService::builder()
        .sandbox_resident_agent_transport(transport.clone())
        .build();
    let local = CountingLocalTransport::new();

    let result = service.execute(request, &local).await;

    assert!(
        result.is_error,
        "side-effect-uncertain transport error should fail closed: {result:?}"
    );
    assert_eq!(transport.calls(), 1, "must not replay after start");
    assert_eq!(local.calls(), 0, "must not fall back locally");
    let metadata = result.metadata.as_ref().expect("transport metadata");
    assert_eq!(metadata["error_kind"], "transport_disconnected");
    assert_eq!(metadata["retryable"], true);
    assert_eq!(metadata["execution_started"], true);
    assert_eq!(metadata["side_effects_maybe"], true);
    assert_eq!(metadata["next_action"], "inspect_effects_before_retry");
}

#[tokio::test(start_paused = true)]
async fn external_transport_timeout_then_second_call_transport_unavailable() {
    // Scenario: a transport hangs and times out.  After the timeout the
    // connection is considered lost.  A subsequent call hits the health
    // gate and receives transport_unavailable with ExecutionError semantics.
    let request = cloud_snapshot_request("read_file");
    let local = CountingLocalTransport::new();

    // ── First call: transport hangs → timeout ──
    let (tx1, rx1) = tokio::sync::oneshot::channel();
    let pending = Arc::new(PendingExternalTransport::new(tx1));
    let service = ToolExecutionService::builder()
        .sandbox_resident_agent_transport(pending.clone())
        .build();

    // Spawn the first call; it will hang inside the transport.
    let handle = tokio::spawn({
        let request = request.clone();
        async move {
            let local = CountingLocalTransport::new();
            service.execute(request, &local).await
        }
    });

    // Wait for the transport to start, then advance past the 30 s timeout.
    rx1.await.expect("transport execute should start");
    tokio::time::advance(std::time::Duration::from_secs(31)).await;
    let first_result = handle.await.expect("timeout task should not panic");
    assert!(first_result.is_error, "{first_result:?}");
    let first_meta = first_result.metadata.as_ref().expect("timeout metadata");
    assert_eq!(first_meta["error_kind"], TOOL_ERROR_KIND_TOOL_TIMEOUT);

    // ── Second call: transport is now unhealthy ──
    let unhealthy = Arc::new(ReplayTransport::with_health(false));
    let service2 = ToolExecutionService::builder()
        .sandbox_resident_agent_transport(unhealthy)
        .build();

    let result = service2.execute(request, &local).await;

    assert!(result.is_error, "{result:?}");
    assert_eq!(local.calls(), 0, "must not fall back locally");
    let metadata = result.metadata.expect("transport metadata");
    assert_eq!(metadata["error_kind"], "transport_unavailable");
    assert_eq!(
        result.exit_semantics,
        Some(astra_tools::exit_semantics::ExitSemantics::ExecutionError),
        "transport-unavailable must carry ExecutionError exit semantics"
    );
}

#[tokio::test]
async fn external_transport_concurrent_cancel_two_transport_paths() {
    // Cancel two different transport paths (gateway relay + sandbox resident
    // agent) concurrently and verify both report ExecutionError.
    let cancel_token = Arc::new(CancellationToken::new());

    let gateway_transport = Arc::new(StaticGatewayRelayTransport::new());
    let sandbox_transport = Arc::new(StaticSandboxResidentAgentTransport::new());
    let service = ToolExecutionService::builder()
        .gateway_relay_transport(gateway_transport)
        .sandbox_resident_agent_transport(sandbox_transport)
        .build();
    let local = CountingLocalTransport::new();

    cancel_token.cancel();

    let gateway_req = openshell_gateway_request("bash");
    let sandbox_req = cloud_snapshot_request("read_file");

    let (r1, r2) = tokio::join!(
        service.execute_with_cancel(gateway_req, &local, Some(cancel_token.clone())),
        service.execute_with_cancel(sandbox_req, &local, Some(cancel_token.clone())),
    );

    for (label, result) in [("gateway relay", &r1), ("sandbox resident agent", &r2)] {
        assert!(result.is_error, "{label}: {result:?}");
        assert_eq!(
            result.exit_semantics,
            Some(astra_tools::exit_semantics::ExitSemantics::ExecutionError),
            "{label}: cancelled transport result must carry ExecutionError exit semantics"
        );
    }
}
