use super::*;
use async_trait::async_trait;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

use super::super::tool_transport_metadata::{
    TOOL_ERROR_KIND_ROUTE_MISMATCH, TOOL_ERROR_KIND_TRANSPORT_UNAVAILABLE,
};
use super::super::tool_transport_plan::{EdgeBoundExecutionPlan, edge_executor_id};
use astra_services::multi_agent::{EdgeDispatchIdentity, EdgeDispatchRow};

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

struct StaticEdgeDispatch {
    inserted_edge_agent_ids: Mutex<Vec<String>>,
    inserted_identities: Mutex<Vec<EdgeDispatchIdentity>>,
    failed_dispatches: Mutex<Vec<(String, String)>>,
    return_result: bool,
    result_status: &'static str,
    terminal_admission_result: Option<String>,
    admission_error: Option<astra_services::multi_agent::EdgeDispatchAdmissionError>,
    direct_claimed: AtomicBool,
}

impl Default for StaticEdgeDispatch {
    fn default() -> Self {
        Self {
            inserted_edge_agent_ids: Mutex::new(Vec::new()),
            inserted_identities: Mutex::new(Vec::new()),
            failed_dispatches: Mutex::new(Vec::new()),
            return_result: true,
            result_status: "completed",
            terminal_admission_result: None,
            admission_error: None,
            direct_claimed: AtomicBool::new(false),
        }
    }
}

impl StaticEdgeDispatch {
    fn no_result() -> Self {
        Self {
            inserted_edge_agent_ids: Mutex::new(Vec::new()),
            inserted_identities: Mutex::new(Vec::new()),
            failed_dispatches: Mutex::new(Vec::new()),
            return_result: false,
            result_status: "completed",
            terminal_admission_result: None,
            admission_error: None,
            direct_claimed: AtomicBool::new(false),
        }
    }

    fn failed_result() -> Self {
        Self {
            inserted_edge_agent_ids: Mutex::new(Vec::new()),
            inserted_identities: Mutex::new(Vec::new()),
            failed_dispatches: Mutex::new(Vec::new()),
            return_result: true,
            result_status: "failed",
            terminal_admission_result: None,
            admission_error: None,
            direct_claimed: AtomicBool::new(false),
        }
    }

    fn terminal_admission(output: &str) -> Self {
        let result = astra_thin_client::ToolResultRequest::new_with_hash(
            astra_thin_client::ToolResultRequestParts {
                session_id: "session-1".to_string(),
                run_id: "run-1".to_string(),
                turn_chain_id: "turn-chain-1".to_string(),
                request_id: "call-1".to_string(),
                edge_agent_id: "edge-selected".to_string(),
                status: "completed".to_string(),
                output: output.to_string(),
                duration_ms: 0,
                tool_result_fields: None,
            },
        );
        Self {
            inserted_edge_agent_ids: Mutex::new(Vec::new()),
            inserted_identities: Mutex::new(Vec::new()),
            failed_dispatches: Mutex::new(Vec::new()),
            return_result: false,
            result_status: "completed",
            terminal_admission_result: Some(
                serde_json::to_string(&result).expect("terminal result fixture must serialize"),
            ),
            admission_error: None,
            direct_claimed: AtomicBool::new(false),
        }
    }

    fn admission_rejected(message: &str) -> Self {
        Self {
            admission_error: Some(
                astra_services::multi_agent::EdgeDispatchAdmissionError::Rejected(
                    message.to_string(),
                ),
            ),
            ..Self::default()
        }
    }

    fn admission_outcome_unknown(message: &str) -> Self {
        Self {
            admission_error: Some(
                astra_services::multi_agent::EdgeDispatchAdmissionError::OutcomeUnknown(
                    message.to_string(),
                ),
            ),
            ..Self::default()
        }
    }
}

#[async_trait]
impl astra_services::multi_agent::EdgeDispatchService for StaticEdgeDispatch {
    async fn insert_dispatch(
        &self,
        identity: &EdgeDispatchIdentity,
        edge_agent_id: &str,
        _payload_json: &str,
    ) -> Result<(), String> {
        self.inserted_edge_agent_ids
            .lock()
            .expect("inserted edge agent ids lock")
            .push(edge_agent_id.to_string());
        self.inserted_identities
            .lock()
            .expect("inserted identities lock")
            .push(identity.clone());
        Ok(())
    }

    async fn admit_dispatch(
        &self,
        identity: &EdgeDispatchIdentity,
        edge_agent_id: &str,
        payload_json: &str,
    ) -> Result<
        astra_services::multi_agent::EdgeDispatchAdmission,
        astra_services::multi_agent::EdgeDispatchAdmissionError,
    > {
        if let Some(error @ astra_services::multi_agent::EdgeDispatchAdmissionError::Rejected(_)) =
            &self.admission_error
        {
            return Err(error.clone());
        }
        self.insert_dispatch(identity, edge_agent_id, payload_json)
            .await
            .map_err(astra_services::multi_agent::EdgeDispatchAdmissionError::OutcomeUnknown)?;
        if let Some(error) = &self.admission_error {
            return Err(error.clone());
        }
        Ok(match &self.terminal_admission_result {
            Some(result) => {
                astra_services::multi_agent::EdgeDispatchAdmission::Terminal(result.clone())
            }
            None => astra_services::multi_agent::EdgeDispatchAdmission::Pending,
        })
    }

    async fn poll_pending(
        &self,
        _user_id: &str,
        _edge_agent_id: &str,
    ) -> Result<Vec<astra_services::multi_agent::EdgeDispatchRow>, String> {
        Ok(Vec::new())
    }

    async fn claim_direct_dispatch(
        &self,
        _identity: &EdgeDispatchIdentity,
        _edge_agent_id: &str,
    ) -> Result<bool, String> {
        Ok(self
            .direct_claimed
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok())
    }

    async fn deliver_result(
        &self,
        _identity: &EdgeDispatchIdentity,
        _edge_agent_id: &str,
        _result_json: &str,
    ) -> Result<bool, String> {
        Ok(true)
    }

    async fn fail_dispatch(
        &self,
        identity: &EdgeDispatchIdentity,
        _edge_agent_id: &str,
        reason: &str,
    ) -> Result<bool, String> {
        self.failed_dispatches
            .lock()
            .expect("failed dispatches lock")
            .push((identity.request_id.clone(), reason.to_string()));
        Ok(true)
    }

    async fn wait_result(
        &self,
        identity: &EdgeDispatchIdentity,
        _timeout: std::time::Duration,
    ) -> Result<Option<String>, String> {
        if !self.return_result {
            return Ok(None);
        }
        let result = astra_thin_client::ToolResultRequest::new_with_hash(
            astra_thin_client::ToolResultRequestParts {
                session_id: identity.session_id.clone(),
                run_id: identity.run_id.clone(),
                turn_chain_id: identity.turn_chain_id.clone(),
                request_id: identity.request_id.clone(),
                edge_agent_id: "edge-selected".to_string(),
                status: self.result_status.to_string(),
                output: if self.result_status == "completed" {
                    "ledger-result".to_string()
                } else {
                    "edge dispatch expired".to_string()
                },
                duration_ms: 12,
                tool_result_fields: None,
            },
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
        _identity: &EdgeDispatchIdentity,
        edge_agent_id: &str,
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
        _identity: &EdgeDispatchIdentity,
        _edge_agent_id: &str,
        _result_json: &str,
    ) -> Result<bool, String> {
        Ok(true)
    }

    async fn fail_dispatch(
        &self,
        identity: &EdgeDispatchIdentity,
        _edge_agent_id: &str,
        reason: &str,
    ) -> Result<bool, String> {
        self.failed_dispatches
            .lock()
            .expect("failed dispatches lock")
            .push((identity.request_id.clone(), reason.to_string()));
        Ok(true)
    }

    async fn wait_result(
        &self,
        _identity: &EdgeDispatchIdentity,
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
    identity: EdgeDispatchIdentity,
    edge_agent_id: String,
    payload_json: String,
    result_json: Option<String>,
    status: String,
}

#[derive(Default)]
struct SharedNoStickyEdgeDispatch {
    rows: Mutex<HashMap<EdgeDispatchIdentity, SharedNoStickyDispatchRow>>,
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
            .values()
            .find(|row| row.identity.user_id == user_id && row.identity.request_id == request_id)
            .map(|row| row.status.clone())
    }
}

#[async_trait]
impl astra_services::multi_agent::EdgeDispatchService for SharedNoStickyEdgeDispatch {
    async fn insert_dispatch(
        &self,
        identity: &EdgeDispatchIdentity,
        edge_agent_id: &str,
        payload_json: &str,
    ) -> Result<(), String> {
        let mut rows = self.rows.lock().expect("shared dispatch rows");
        rows.entry(identity.clone())
            .or_insert_with(|| SharedNoStickyDispatchRow {
                identity: identity.clone(),
                edge_agent_id: edge_agent_id.to_string(),
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
    ) -> Result<Vec<EdgeDispatchRow>, String> {
        let mut rows = self.rows.lock().expect("shared dispatch rows");
        let mut claimed = Vec::new();
        for row in rows.values_mut() {
            if row.identity.user_id == user_id
                && row.edge_agent_id == edge_agent_id
                && row.status == "pending"
            {
                row.status = "dispatched".to_string();
                claimed.push(EdgeDispatchRow {
                    user_id: row.identity.user_id.clone(),
                    session_id: row.identity.session_id.clone(),
                    run_id: row.identity.run_id.clone(),
                    turn_chain_id: row.identity.turn_chain_id.clone(),
                    edge_agent_id: row.edge_agent_id.clone(),
                    request_id: row.identity.request_id.clone(),
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
        identity: &EdgeDispatchIdentity,
        edge_agent_id: &str,
        result_json: &str,
    ) -> Result<bool, String> {
        let mut rows = self.rows.lock().expect("shared dispatch rows");
        let Some(row) = rows.get_mut(identity) else {
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
        identity: &EdgeDispatchIdentity,
        edge_agent_id: &str,
        reason: &str,
    ) -> Result<bool, String> {
        let mut rows = self.rows.lock().expect("shared dispatch rows");
        let Some(row) = rows.get_mut(identity) else {
            return Ok(false);
        };
        if row.edge_agent_id != edge_agent_id
            || !matches!(row.status.as_str(), "pending" | "dispatched")
        {
            return Ok(false);
        }
        row.status = "failed".to_string();
        row.result_json = Some(
            serde_json::json!({
                "session_id": identity.session_id,
                "run_id": identity.run_id,
                "turn_chain_id": identity.turn_chain_id,
                "request_id": identity.request_id,
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
        identity: &EdgeDispatchIdentity,
        timeout: std::time::Duration,
    ) -> Result<Option<String>, String> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            {
                let rows = self.rows.lock().expect("shared dispatch rows");
                let Some(row) = rows.get(identity) else {
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
        _workspace_id: Option<&str>,
    ) -> Result<astra_services::multi_agent::EdgeAgentRecord, String> {
        Err("not needed for this test".to_string())
    }

    async fn heartbeat(
        &self,
        _user_id: &str,
        _edge_agent_id: &str,
        _edge_id_header: &str,
        _registration_claim_id: Option<&str>,
    ) -> Result<(), astra_services::multi_agent::HeartbeatError> {
        Ok(())
    }

    async fn find_by_agent_id_and_workspace(
        &self,
        edge_agent_id: &str,
        workspace_id: Option<&str>,
    ) -> Result<Option<astra_services::multi_agent::EdgeAgentRecord>, String> {
        let record = self
            .agents
            .iter()
            .find(|a| {
                a.edge_agent_id == edge_agent_id
                    && match (workspace_id, a.workspace_id.as_deref()) {
                        (Some(req), Some(edge)) => req == edge,
                        (None, None) => true,
                        _ => false,
                    }
            })
            .cloned();
        Ok(record)
    }

    async fn list_by_user(
        &self,
        user_id: &str,
    ) -> Result<Vec<astra_services::multi_agent::EdgeAgentRecord>, String> {
        Ok(self
            .agents
            .iter()
            .filter(|agent| agent.user_id == user_id)
            .cloned()
            .collect())
    }

    async fn unregister_generation(
        &self,
        _user_id: &str,
        _edge_agent_id: &str,
        _edge_id_header: &str,
    ) -> Result<bool, String> {
        Ok(true)
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
        workspace_id: None,
        materialization_id: Some(format!("materialization-{edge_agent_id}")),
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
    let mut advertisement = serde_json::to_value(
        astra_runtime_env::RuntimeEnvironmentAdvertisement::new(binding),
    )
    .expect("serialize edge runtime environment advertisement");
    advertisement["protocol_capabilities"] = serde_json::json!({
        "runtime_process_authorization_v1": true,
    });
    advertisement
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
        turn_chain_id: "chain-1".to_string(),
        tool_call_id: "call-1".to_string(),
        tool_name: tool_name.to_string(),
        args: serde_json::json!({}),
        workspace,
        workspace_record: None,
        executor,
        runtime: None,
        runtime_process_authorization: None,
        runtime_process_authorization_required: false,
        runtime_edge_dispatch_authorization: None,
        runtime_edge_dispatch_authorization_required: false,
        selected_offer: None,
        policy: ToolPolicySnapshot::default(),
    }
}

fn request_scoped_mcp_request(tool_name: &str) -> ToolExecutionRequest {
    request(
        tool_name,
        WorkspaceBinding::none(),
        ExecutorBinding::request_scoped_mcp(),
    )
    .with_selected_offer(SelectedToolOfferSnapshot::new(
        tool_name,
        "request-scoped-mcp",
    ))
}

#[test]
fn edge_provider_selection_requires_the_current_frozen_descriptor() {
    use super::super::tool_execution_binding::{
        ToolPermissionGrantSnapshot, ToolPermissionGrantSource,
    };
    use astra_turn_core::provider_resolution::{
        ProviderClaimTrustPolicy, ResolvedProviderPolicyIndex, resolve_provider_snapshot,
    };
    use astra_turn_types::*;
    let tool = ProviderToolDeclaration {
        native_tool_id: NativeToolId::new("structured_worker").unwrap(),
        native_tool_name: "structured_worker".into(),
        stable_tool_alias: None,
        title: None,
        description: None,
        input_schema: serde_json::json!({"type": "object"}),
        output_schema: None,
        claims: ProviderToolClaims::default(),
        task_support: ProviderTaskSupport::Unspecified,
        extension_fields: serde_json::Map::from_iter([(
            PROVIDER_RUNTIME_REQUIREMENTS_KEY.into(),
            serde_json::json!({"executable": "/usr/bin/worker", "read_paths": ["/usr/bin/worker", "/usr/lib"]}),
        )]),
    };
    let discovery = ProviderDiscoverySnapshot::new(
        ProviderIdentity::new("selected-runtime").unwrap(),
        ProviderBindingRef::new(
            astra_services::SessionExecutionBindingV1::edge_materialization_physical_identity(
                "materialization-edge-1",
                "/Users/test/project",
            ),
        )
        .unwrap(),
        ProviderProtocolId::new("cli-local").unwrap(),
        vec![tool],
    )
    .unwrap();
    let aliases = std::collections::BTreeMap::from([(
        NativeToolId::new("structured_worker").unwrap(),
        PublicToolAlias::new("structured_worker").unwrap(),
    )]);
    let resolved =
        resolve_provider_snapshot(&discovery, &ProviderClaimTrustPolicy::default(), &aliases)
            .unwrap();
    let index = ResolvedProviderPolicyIndex::from_snapshots(&[resolved]).unwrap();
    let mut agent = edge_agent_record("edge-1");
    let mut advert: astra_runtime_env::RuntimeEnvironmentAdvertisement =
        serde_json::from_value(agent.capabilities.clone().unwrap()).unwrap();
    advert.provider_discovery = vec![discovery.clone()];
    agent.capabilities = Some(serde_json::to_value(&advert).unwrap());
    let mut invocation = request(
        "structured_worker",
        WorkspaceBinding::edge_workspace(
            "project",
            "/Users/test/project",
            WorkspaceAuthority::ReadWrite,
        ),
        ExecutorBinding::edge_agent(
            "edge-1",
            "Edge",
            ToolTransportKind::EdgeLedger,
            ExecutorStatus::Online,
        ),
    );
    invocation.policy.resolved_provider_policy = index.resolve("structured_worker").cloned();
    assert!(
        EdgeBoundExecutionPlan::try_from_request_with_binding(&invocation, &advert.binding)
            .is_err()
    );
    invocation.policy.permission_grant = Some(ToolPermissionGrantSnapshot {
        source: ToolPermissionGrantSource::Policy,
        reason: None,
        updates_hash: None,
    });
    assert!(
        EdgeBoundExecutionPlan::try_from_request_with_binding(&invocation, &advert.binding)
            .is_err()
    );
    invocation.policy.execution_binding_generation = Some(7);
    let plan = EdgeBoundExecutionPlan::try_from_request_with_binding(&invocation, &advert.binding)
        .unwrap();
    assert!(!plan.requires_live_provider_interaction());
    assert!(plan.dispatch_payload_json().is_err());

    let read_only_binding = astra_runtime_env::RunBinding::resolve(
        astra_runtime_env::WorkspaceBinding::edge_workspace(
            "/Users/test/project",
            astra_runtime_env::WorkspaceAuthority::ReadOnly,
        ),
        astra_runtime_env::ExecutorBinding::edge_agent("edge-1"),
        astra_runtime_env::RuntimeBinding::host_process("edge-host:edge-1"),
        astra_runtime_env::PolicyIntent::read_only_review(),
        &astra_runtime_env::ToolRegistry::builtins(),
    );
    let mut read_only_invocation = invocation.clone();
    read_only_invocation.workspace.authority = WorkspaceAuthority::ReadOnly;
    let read_only_plan = EdgeBoundExecutionPlan::try_from_request_with_binding(
        &read_only_invocation,
        &read_only_binding,
    )
    .unwrap();
    assert_eq!(
        read_only_binding.policy.isolation,
        astra_runtime_env::IsolationIntent::ProviderEnforced
    );
    assert!(read_only_plan.execution_ceiling().is_some());

    for filesystem in [
        astra_runtime_env::FilesystemPolicy::NoAccess,
        astra_runtime_env::FilesystemPolicy::ExplicitAllowList,
    ] {
        let mut restricted = advert.binding.clone();
        restricted.policy.filesystem = filesystem;
        assert!(
            EdgeBoundExecutionPlan::try_from_request_with_binding(&invocation, &restricted)
                .is_err()
        );
    }
    assert!(
        plan.bind_execution_ceiling(
            "user-1",
            "edge-1",
            Some("/Users/test/project"),
            None,
            Some("replacement-checkout")
        )
        .is_err()
    );
    for (owner, executor, root, materialization) in [
        (
            "other-user",
            "edge-1",
            "/Users/test/project",
            Some("materialization-edge-1"),
        ),
        (
            "user-1",
            "other-edge",
            "/Users/test/project",
            Some("materialization-edge-1"),
        ),
        (
            "user-1",
            "edge-1",
            "/different",
            Some("materialization-edge-1"),
        ),
        ("user-1", "edge-1", "/Users/test/project", None),
    ] {
        assert!(
            plan.bind_execution_ceiling(owner, executor, Some(root), None, materialization)
                .is_err()
        );
    }
    let bound = plan
        .bind_execution_ceiling(
            "user-1",
            "edge-1",
            Some("/Users/test/project"),
            None,
            Some("materialization-edge-1"),
        )
        .unwrap();
    let ceiling = bound.execution_ceiling().unwrap();
    assert_eq!(ceiling.execution_binding_generation, 7);
    assert_eq!(
        ceiling.materialization_id.as_deref(),
        Some("materialization-edge-1")
    );
    assert_eq!(ceiling.runtime_read_paths, ["/usr/bin/worker", "/usr/lib"]);
    let payload: astra_server_types::EdgeServerMessage =
        serde_json::from_str(&bound.dispatch_payload_json().unwrap()).unwrap();
    let astra_server_types::EdgeServerMessage::ToolRequest {
        execution_ceiling, ..
    } = payload
    else {
        panic!("tool request required");
    };
    assert_eq!(execution_ceiling.as_deref(), Some(ceiling));
    let registry = astra_runtime_env::ToolRegistry::builtins();
    let select = |agent: &astra_services::multi_agent::EdgeAgentRecord,
                  invocation: &ToolExecutionRequest| {
        super::super::tool_edge_selection::select_capable_edge_agent(
            std::slice::from_ref(agent),
            Some("edge-1"),
            invocation,
            &registry,
        )
        .map(|selected| selected.is_some())
    };
    assert!(select(&agent, &invocation).unwrap());
    let mut unadmitted = invocation.clone();
    unadmitted.policy.resolved_provider_policy = None;
    assert!(select(&agent, &unadmitted).is_err());

    let mut collaborator_tool = discovery.tool_declarations[0].clone();
    collaborator_tool.task_support = ProviderTaskSupport::Required;
    collaborator_tool
        .extension_fields
        .insert(PROVIDER_COLLABORATOR_STAGE_KEY.into(), Value::Bool(true));
    collaborator_tool.extension_fields.insert(
        astra_turn_core::provider_resolution::NativeCollaboratorProtocol::EXTENSION_KEY.into(),
        Value::String(
            astra_turn_core::provider_resolution::NativeCollaboratorProtocol::CodexAppServer
                .extension_value()
                .into(),
        ),
    );
    let collaborator_discovery = ProviderDiscoverySnapshot::new(
        ProviderIdentity::new("selected-runtime").unwrap(),
        ProviderBindingRef::new(
            astra_services::SessionExecutionBindingV1::edge_materialization_physical_identity(
                "materialization-edge-1",
                "/Users/test/project",
            ),
        )
        .unwrap(),
        ProviderProtocolId::new("cli-local").unwrap(),
        vec![collaborator_tool],
    )
    .unwrap();
    let collaborator_resolved = resolve_provider_snapshot(
        &collaborator_discovery,
        &ProviderClaimTrustPolicy::default(),
        &aliases,
    )
    .unwrap();
    let collaborator_index =
        ResolvedProviderPolicyIndex::from_snapshots(&[collaborator_resolved]).unwrap();
    let mut collaborator_invocation = invocation.clone();
    collaborator_invocation.policy.resolved_provider_policy =
        collaborator_index.resolve("structured_worker").cloned();
    let collaborator_plan = EdgeBoundExecutionPlan::try_from_request(&collaborator_invocation)
        .expect("collaborator policy should produce an edge plan");
    assert!(collaborator_plan.requires_live_provider_interaction());
    let mut command_binding = advert.binding.clone();
    command_binding.policy.resources.max_execution_secs = Some(7.2);
    let stage_plan = EdgeBoundExecutionPlan::try_from_request_with_binding(
        &collaborator_invocation,
        &command_binding,
    )
    .unwrap();
    assert_eq!(stage_plan.execution_timeout_secs(), 300);
    assert_eq!(stage_plan.command_timeout_cap_ms(), Some(8_000));
    collaborator_invocation.policy.admission_deadline =
        Some(std::time::Instant::now() + std::time::Duration::from_secs(3));
    collaborator_invocation.policy.execution_deadline_unix_ms = Some(4_102_444_800_000);
    let bounded_stage = EdgeBoundExecutionPlan::try_from_request_with_binding(
        &collaborator_invocation,
        &command_binding,
    )
    .unwrap();
    assert!(bounded_stage.execution_timeout_secs() <= 3);
    assert_eq!(bounded_stage.command_timeout_cap_ms(), Some(8_000));
    for change in [
        "schema",
        "root",
        "executor",
        "process",
        "readable",
        "reachable",
    ] {
        let mut changed = advert.clone();
        match change {
            "schema" => {
                let old = &changed.provider_discovery[0];
                let mut tools = old.tool_declarations.clone();
                tools[0].input_schema = serde_json::json!({"type": "object", "required": ["new"]});
                changed.provider_discovery[0] = ProviderDiscoverySnapshot::new(
                    old.provider_identity.clone(),
                    old.binding_ref.clone(),
                    old.protocol.clone(),
                    tools,
                )
                .unwrap();
            }
            "root" => changed.binding.workspace.cwd = Some("/different".into()),
            "process" => changed.binding.capabilities.runtime.runtime_has_process = false,
            "readable" => changed.binding.capabilities.workspace.readable = false,
            "reachable" => changed.binding.capabilities.executor.reachable = false,
            _ => changed.binding.executor.executor_id = "other-edge".into(),
        }
        let mut stale = agent.clone();
        stale.capabilities = Some(serde_json::to_value(changed).unwrap());
        assert!(select(&stale, &invocation).is_err(), "{change}");
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
fn route_boundary_tool_call_end_promotes_structured_artifacts() {
    let service = ToolExecutionService::new_for_test();
    let mut request = request_scoped_mcp_request("mcp__moi__write_file");
    request.args = serde_json::json!({
        "_tool_call_id": "call-1",
        "_run_id": "run-1",
        "path": "main.go"
    });
    let boundary = service.route_boundary(request);

    let mut result = astra_tools::ToolResult::text("created main.go".to_string());
    result.metadata = Some(serde_json::Map::from_iter([(
        "structuredContent".to_string(),
        serde_json::json!({
            "artifacts": [{
                "artifact_id": "artifact_file_1",
                "name": "x".repeat(161),
                "type": "file",
                "data": {
                    "file_id": "file_1",
                    "content_type": "text/html; charset=utf-8",
                    "mime_type": "IMAGE/PNG"
                }
            }]
        }),
    )]));

    let transport_event = boundary
        .transport_finished_event(&result, 17)
        .expect("tool transport completed event");
    assert_eq!(transport_event["type"], "tool_transport_completed");
    assert!(transport_event.get("structuredContent").is_none());
    assert!(transport_event.get("artifacts").is_none());

    let end_event = boundary
        .tool_call_end_event(&result, 17)
        .expect("tool call end event");
    assert_eq!(end_event["type"], "tool_call_end");
    assert_eq!(end_event["result"], "created main.go");
    assert_eq!(
        end_event["structuredContent"]["artifacts"][0]["artifact_id"],
        "artifact_file_1"
    );
    assert_eq!(end_event["artifacts"][0]["artifact_id"], "artifact_file_1");
    assert!(end_event["artifacts"][0].get("name").is_none());
    assert!(
        end_event["artifacts"][0]["data"]
            .get("content_type")
            .is_none()
    );
    assert_eq!(end_event["artifacts"][0]["data"]["mime_type"], "image/png");
    assert!(end_event.get("output").is_none());
    assert!(end_event.get("artifact").is_none());
}

#[test]
fn route_boundary_tool_call_end_includes_error_for_failed_result() {
    let service = ToolExecutionService::new_for_test();
    let mut request = request_scoped_mcp_request("mcp__moi__write_file");
    request.args = serde_json::json!({
        "_tool_call_id": "call-err",
        "_run_id": "run-1",
        "path": "main.go"
    });
    let boundary = service.route_boundary(request);

    let result = astra_tools::ToolResult::error("permission denied".to_string());

    let end_event = boundary
        .tool_call_end_event(&result, 17)
        .expect("tool call end event");
    assert_eq!(end_event["type"], "tool_call_end");
    assert_eq!(end_event["result"], "permission denied");
    assert_eq!(end_event["success"], false);
    assert_eq!(end_event["error"], "permission denied");
}

#[test]
fn route_boundary_preserves_skipped_terminal_status_from_tool_metadata() {
    let service = ToolExecutionService::new_for_test();
    let mut request = request(
        "read_file",
        WorkspaceBinding::server_sandbox("/tmp/astra-workspace"),
        ExecutorBinding::server_local(),
    );
    request.tool_call_id = "call-skip".to_string();
    request.args = serde_json::json!({
        "_tool_call_id": "stale-legacy-call-id",
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
    let mut request = request(
        "bash",
        WorkspaceBinding::server_sandbox("/tmp/astra-workspace"),
        ExecutorBinding::server_local(),
    );
    request.tool_call_id.clear();
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
    let metadata = result
        .metadata
        .expect("unknown tool must retain typed admission metadata");
    assert_eq!(
        metadata["error_kind"],
        astra_core::ErrorKind::ToolNotFound.as_str()
    );
    assert_eq!(metadata["disposition"], "rejected");
    assert_eq!(metadata["execution_started"], false);
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
    for allowed in ["read_file", "glob"] {
        let service = ToolExecutionService::new_for_test();
        let local = CountingLocalTransport::new();
        let mut request = request(
            "bash",
            WorkspaceBinding::server_sandbox("/tmp/astra-workspace"),
            ExecutorBinding::server_local(),
        );
        request.policy.allowed_tools = vec![allowed.to_string()];

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
        "worktree",
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
async fn mcp_prefixed_tool_is_not_request_scoped_mcp_without_explicit_executor() {
    let service = ToolExecutionService::new_for_test();
    let local = CountingLocalTransport::new();
    let request = request(
        "mcp__rag__retrieve",
        WorkspaceBinding::none(),
        ExecutorBinding::server_local(),
    );
    let registry = astra_runtime_env::ToolRegistry::builtins();
    let binding = request.runtime_environment_binding(&registry);

    assert_eq!(
        service.routing_decision(&request),
        ToolExecutionRouteKind::Unsupported
    );
    assert_ne!(
        binding.executor.kind,
        astra_runtime_env::ExecutorBindingKind::Mcp,
        "mcp__ prefix alone must not synthesize an MCP executor"
    );
    assert_eq!(
        binding.runtime.session_manager,
        astra_runtime_env::RuntimeSessionManager::None
    );
    assert_eq!(
        binding.runtime.isolation_backend,
        astra_runtime_env::RuntimeIsolationBackend::None
    );
    assert!(
        !binding.tool_surface.contains("mcp__rag__retrieve"),
        "mcp__ prefix alone must not expose a provider offer"
    );
    assert!(
        astra_runtime_env::CapabilityResolver
            .check_tool_call_for_surface(
                &registry,
                "mcp__rag__retrieve",
                &serde_json::json!({"query": "what is astra?"}),
                &binding.capabilities,
                &binding.tool_surface,
            )
            .is_err(),
        "tool surface must reject mcp__ names without an explicit MCP provider"
    );

    let result = service.execute(request, &local).await;

    assert!(result.is_error, "{result:?}");
    assert_eq!(local.calls(), 0);
    let metadata = result.metadata.expect("unsupported mcp metadata");
    assert_eq!(metadata["error_kind"], TOOL_ERROR_KIND_CAPABILITY_DENIED);
    assert_eq!(metadata["workspace"]["kind"], "none");
    assert_ne!(metadata["executor"]["kind"], "mcp");
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
    assert!(binding.tool_surface.contains("glob"));
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

    let plan = EdgeBoundExecutionPlan::try_from_request(&request).unwrap();

    assert_eq!(plan.selected_executor_id(), Some("edge-1"));
    assert_eq!(plan.dispatch_request_id(), plan.identity().storage_key());
    assert_eq!(plan.wait_timeout(), std::time::Duration::from_secs(310));

    let payload: Value =
        serde_json::from_str(&plan.dispatch_payload_json().expect("dispatch payload"))
            .expect("payload json");
    assert_eq!(payload["type"], "edge_tool_request");
    assert_eq!(payload["request_id"], plan.identity().storage_key());
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
fn durable_edge_payload_never_contains_runtime_process_authorization() {
    let mut request = request(
        "bash",
        WorkspaceBinding::edge_workspace(
            "Ephemeral sandbox",
            "/sandbox/.moi",
            WorkspaceAuthority::ReadWrite,
        ),
        ExecutorBinding::edge_agent(
            "edge-1",
            "Ephemeral sandbox",
            ToolTransportKind::EdgeWs,
            ExecutorStatus::Online,
        ),
    );
    request.runtime_process_authorization = Some(std::sync::Arc::new(
        astra_services::runs::RuntimeProcessAuthorizationContext {
            authorization: "Bearer durable-secret-must-not-appear".to_string(),
        },
    ));
    request.runtime_process_authorization_required = true;

    let plan = EdgeBoundExecutionPlan::try_from_request(&request).unwrap();
    let payload = plan.dispatch_payload_json().expect("dispatch payload");
    let parsed: Value = serde_json::from_str(&payload).expect("payload json");

    assert!(!payload.contains("durable-secret-must-not-appear"));
    assert_eq!(parsed["runtime_process_authorization"], Value::Null);
    assert_eq!(parsed["runtime_process_authorization_required"], true);
    assert!(plan.runtime_process_authorization().is_some());
}

#[test]
fn edge_dispatch_preserves_admitted_work_budget_without_rewriting_arguments() {
    let mut request = request(
        "native_codex",
        WorkspaceBinding::edge_workspace(
            "selected CLI",
            "/selected",
            WorkspaceAuthority::ReadWrite,
        ),
        ExecutorBinding::edge_agent(
            "selected-cli",
            "selected CLI",
            ToolTransportKind::EdgeLedger,
            ExecutorStatus::Online,
        ),
    );
    request.args = serde_json::json!({"task":"review", "model":"luna"});
    request.policy.admission_deadline =
        Some(std::time::Instant::now() + std::time::Duration::from_secs(600));
    request.policy.execution_deadline_unix_ms = Some(4_102_444_800_000);
    let plan = EdgeBoundExecutionPlan::try_from_request(&request).unwrap();
    let payload: Value = serde_json::from_str(&plan.dispatch_payload_json().unwrap()).unwrap();
    assert_eq!(
        payload["identity"],
        serde_json::to_value(plan.identity()).unwrap()
    );
    assert_eq!(payload["request_id"], plan.identity().storage_key());
    assert_eq!(payload["args"], request.args);
    assert_eq!(payload["execution_deadline_unix_ms"], 4_102_444_800_000u64);
    assert!(payload["execution_timeout_ms"].as_u64().unwrap() <= 600_000);
    assert!(payload["execution_timeout_ms"].as_u64().unwrap() > 590_000);
    assert!(payload["command_timeout_cap_ms"].is_null());
    assert!(plan.wait_timeout() > std::time::Duration::from_secs(590));
    request.policy.max_execution_secs = Some(7.2);
    let binding = request.runtime_environment_binding(&astra_runtime_env::ToolRegistry::builtins());
    let governed =
        EdgeBoundExecutionPlan::try_from_request_with_binding(&request, &binding).unwrap();
    let governed: Value = serde_json::from_str(&governed.dispatch_payload_json().unwrap()).unwrap();
    assert_eq!(governed["command_timeout_cap_ms"], 8_000);
    assert!(
        governed["execution_timeout_ms"].as_u64().unwrap() > 590_000,
        "command policy must not replace native whole-stage budget"
    );
    let decoded: astra_server_types::EdgeServerMessage = serde_json::from_value(payload).unwrap();
    let replay = serde_json::to_value(decoded).unwrap();
    assert_eq!(replay["execution_deadline_unix_ms"], 4_102_444_800_000u64);
    request.policy.admission_deadline = Some(std::time::Instant::now());
    assert!(matches!(
        EdgeBoundExecutionPlan::try_from_request(&request),
        Err(astra_turn_types::ToolInvocationContractError::InvalidExecutionBudget)
    ));
}

#[tokio::test(start_paused = true)]
async fn admitted_edge_plan_remaining_never_renews_and_rejects_partial_pair() {
    let mut request = request(
        "native_codex",
        WorkspaceBinding::edge_workspace("CLI", "/selected", WorkspaceAuthority::ReadWrite),
        ExecutorBinding::edge_agent(
            "cli",
            "CLI",
            ToolTransportKind::EdgeWs,
            ExecutorStatus::Online,
        ),
    );
    request.policy.admission_deadline =
        Some(std::time::Instant::now() + std::time::Duration::from_secs(86_400));
    request.policy.execution_deadline_unix_ms = Some(4_102_444_800_000);
    let plan = EdgeBoundExecutionPlan::try_from_request(&request).unwrap();
    let original = plan.execution_timeout_ms().unwrap();
    tokio::time::advance(std::time::Duration::from_secs(5430)).await;
    let remaining = plan.execution_timeout_ms().unwrap();
    assert_eq!(original - remaining, 5_430_000);
    assert!(plan.wait_timeout() > std::time::Duration::from_secs(80_000));
    let payload: Value = serde_json::from_str(&plan.dispatch_payload_json().unwrap()).unwrap();
    assert_eq!(payload["execution_timeout_ms"], remaining);
    tokio::time::advance(std::time::Duration::from_secs(86_400)).await;
    assert_eq!(plan.execution_timeout_ms(), Some(0));
    assert_eq!(plan.wait_timeout(), std::time::Duration::from_secs(10));
    request.policy.execution_deadline_unix_ms = None;
    assert!(EdgeBoundExecutionPlan::try_from_request(&request).is_err());
    request.policy.admission_deadline = None;
    request.policy.execution_deadline_unix_ms = Some(4_102_444_800_000);
    assert!(EdgeBoundExecutionPlan::try_from_request(&request).is_err());
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

    let plan = EdgeBoundExecutionPlan::try_from_request_with_binding(&request, &binding).unwrap();

    assert_eq!(plan.wait_timeout(), std::time::Duration::from_secs(40));
    let payload: Value =
        serde_json::from_str(&plan.dispatch_payload_json().expect("dispatch payload"))
            .expect("payload json");
    assert_eq!(payload["request_id"], plan.identity().storage_key());
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

    let plan = EdgeBoundExecutionPlan::try_from_request_with_binding(&request, &binding).unwrap();

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
fn edge_bound_execution_plan_clamps_invalid_or_excessive_policy_timeout() {
    let registry = astra_runtime_env::ToolRegistry::builtins();
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

    request.policy.max_execution_secs = Some(0.0);
    let binding = request.runtime_environment_binding(&registry);
    let plan = EdgeBoundExecutionPlan::try_from_request_with_binding(&request, &binding).unwrap();
    assert_eq!(plan.execution_timeout_secs(), 1);
    assert_eq!(plan.wait_timeout(), std::time::Duration::from_secs(11));

    request.policy.max_execution_secs = Some(9_999.0);
    let binding = request.runtime_environment_binding(&registry);
    let plan = EdgeBoundExecutionPlan::try_from_request_with_binding(&request, &binding).unwrap();
    assert_eq!(
        plan.execution_timeout_secs(),
        astra_server_types::MAX_EDGE_TOOL_TIMEOUT_SECS
    );
    assert_eq!(
        plan.wait_timeout(),
        std::time::Duration::from_secs(astra_server_types::MAX_EDGE_TOOL_TIMEOUT_SECS + 10)
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
    assert_eq!(metadata["failure_scope"], "executor_transport");
    assert_eq!(metadata["disposition"], "rejected");
    assert_eq!(
        metadata["route_failure"]["executor_id"],
        "orchestrator:snapshot"
    );
    assert_eq!(metadata["route_failure"]["shared_across_bound_tools"], true);
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
    assert!(binding.tool_surface.contains("glob"));
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
async fn gateway_relay_route_is_explicitly_unavailable() {
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
async fn sandbox_resident_agent_route_is_explicitly_unavailable() {
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
async fn cloud_workspace_blocks_without_server_reroute() {
    let service = ToolExecutionService::new_for_test();
    let local = CountingLocalTransport::new();
    let mut request = request(
        "read_file",
        WorkspaceBinding {
            kind: WorkspaceBindingKind::CloudWorkspace,
            display_name: "Cloud workspace".to_string(),
            cwd: Some("/checkout/repo".to_string()),
            authority: WorkspaceAuthority::ReadOnly,
        },
        ExecutorBinding::server_local(),
    );
    request.args = serde_json::json!({"path": "README.md"});

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
        result.output.contains("changing the tool name"),
        "{}",
        result.output
    );
    let metadata = result.metadata.expect("offline metadata");
    assert_eq!(metadata["error_kind"], TOOL_ERROR_KIND_EXECUTOR_OFFLINE);
    assert_eq!(metadata["blocked"], true);
    assert_eq!(metadata["executor"]["status"], "offline");
    assert_eq!(metadata["failure_scope"], "executor_transport");
    assert_eq!(metadata["disposition"], "rejected");
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
    let dispatch = Arc::new(StaticEdgeDispatch::default());
    let service = ToolExecutionService::builder()
        .edge_connection_pool(pool)
        .edge_dispatch_service(dispatch.clone())
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
            .contains("unavailable before tool 'bash' was dispatched"),
        "{}",
        result.output
    );
    let metadata = result.metadata.expect("transport metadata");
    assert_eq!(
        metadata["error_kind"],
        TOOL_ERROR_KIND_TRANSPORT_UNAVAILABLE
    );
    assert_eq!(metadata["execution_started"], false);
    assert!(
        dispatch
            .inserted_edge_agent_ids
            .lock()
            .expect("inserted edge agent ids lock")
            .is_empty(),
        "missing selected executor must not admit a dispatch for another connected edge"
    );
    assert_eq!(local.calls(), 0);
    assert!(
        rx.try_recv().is_err(),
        "selected edge binding must not dispatch to a different connected edge"
    );
}

#[tokio::test]
async fn unscoped_edge_binding_does_not_route_to_another_users_connected_edge() {
    let pool = astra_server_types::edge_connection_pool::EdgeConnectionPool::new();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<astra_server_types::EdgeServerMessage>(1);
    pool.register_with_capabilities(
        "user-2",
        "edge-selected",
        Some("Other user's laptop".to_string()),
        Some("/Users/other/project".to_string()),
        Some(edge_runtime_environment_advertisement("edge-selected")),
        None,
        tx,
    );
    let dispatch = Arc::new(StaticEdgeDispatch::default());
    let service = ToolExecutionService::builder()
        .edge_connection_pool(pool)
        .edge_dispatch_service(dispatch.clone())
        .build();

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
            &CountingLocalTransport::new(),
        )
        .await;

    assert!(result.is_error, "{result:?}");
    assert!(
        rx.try_recv().is_err(),
        "an unscoped edge_agent_id must not authorize cross-user socket delivery"
    );
    assert!(
        dispatch
            .inserted_edge_agent_ids
            .lock()
            .expect("inserted edge agent ids lock")
            .is_empty(),
        "an unscoped cross-user connection must not be durably admitted"
    );
}

#[tokio::test]
async fn edge_websocket_without_durable_dispatch_authority_does_not_send() {
    let pool = astra_server_types::edge_connection_pool::EdgeConnectionPool::new();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<astra_server_types::EdgeServerMessage>(1);
    pool.register_with_capabilities(
        "user-1",
        "edge-selected",
        Some("MacBook Pro".to_string()),
        Some("/Users/test/project".to_string()),
        Some(edge_runtime_environment_advertisement("edge-selected")),
        None,
        tx,
    );
    let service = ToolExecutionService::builder()
        .edge_connection_pool(pool)
        .build();

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
            &CountingLocalTransport::new(),
        )
        .await;

    assert!(result.is_error, "{result:?}");
    assert!(
        rx.try_recv().is_err(),
        "socket delivery without a durable ACK authority must be rejected before send"
    );
    let metadata = result.metadata.expect("transport metadata");
    assert_eq!(
        metadata["side_effects_maybe"], false,
        "an invocation rejected before delivery has a certain not-dispatched outcome"
    );
    assert_eq!(metadata["execution_started"], false);
    assert_eq!(metadata["retryable"], true);
}

#[tokio::test]
async fn current_provider_executor_authorization_allows_one_durable_socket_dispatch() {
    let authorization_calls = Arc::new(AtomicUsize::new(0));
    let observed_request = Arc::new(Mutex::new(None));
    let calls = Arc::clone(&authorization_calls);
    let observed = Arc::clone(&observed_request);
    let app = axum::Router::new().route(
        "/api/v1/runtime-executors/authorize",
        axum::routing::post(
            move |headers: axum::http::HeaderMap, axum::Json(body): axum::Json<Value>| {
                let calls = Arc::clone(&calls);
                let observed = Arc::clone(&observed);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    *observed.lock().expect("authorization request lock") = Some((headers, body));
                    axum::http::StatusCode::NO_CONTENT
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind authorization server");
    let endpoint_url = format!(
        "http://{}/api/v1/runtime-executors/authorize",
        listener.local_addr().expect("authorization server address")
    );
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve authorization response");
    });

    let pool = astra_server_types::edge_connection_pool::EdgeConnectionPool::new();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<astra_server_types::EdgeServerMessage>(1);
    pool.register_with_capabilities(
        "user-1",
        "runner-selected",
        Some("Runner".to_string()),
        Some("/workspace".to_string()),
        Some(edge_runtime_environment_advertisement("runner-selected")),
        None,
        tx,
    );
    let dispatch = Arc::new(StaticEdgeDispatch::default());
    let service = ToolExecutionService::builder()
        .edge_connection_pool(pool.clone())
        .edge_dispatch_service(dispatch.clone())
        .build();
    let mut tool_request = request(
        "bash",
        WorkspaceBinding::edge_workspace("Runner", "/workspace", WorkspaceAuthority::ReadWrite),
        ExecutorBinding::edge_agent(
            "runner-selected",
            "Runner",
            ToolTransportKind::EdgeWs,
            ExecutorStatus::Online,
        ),
    );
    tool_request.runtime_edge_dispatch_authorization = Some(Arc::new(
        astra_services::runs::RuntimeEdgeDispatchAuthorizationContext {
            endpoint_url,
            authorization: "Bearer runtime-grant".to_string(),
            task_id: "task-1".to_string(),
            executor_id: "runner-selected".to_string(),
        },
    ));
    tool_request.runtime_edge_dispatch_authorization_required = true;

    let handle = tokio::spawn(async move {
        service
            .execute(tool_request, &CountingLocalTransport::new())
            .await
    });
    let message = rx.recv().await.expect("authorized edge tool request");
    let (request_id, delivery_generation) = match message {
        astra_server_types::EdgeServerMessage::ToolRequest {
            request_id,
            delivery_generation,
            ..
        } => (request_id, delivery_generation),
        other => panic!("expected tool request, got {other:?}"),
    };
    assert!(pool.deliver_tool_result(
        "user-1",
        "runner-selected",
        &request_id,
        delivery_generation,
        astra_server_types::edge_connection_pool::EdgeToolResult {
            output: "authorized-result".to_string(),
            is_error: false,
            duration_ms: Some(3),
            tool_result_fields: None,
        },
    ));
    let result = handle.await.expect("authorized edge execution join");

    assert!(!result.is_error, "{result:?}");
    assert_eq!(result.output, "authorized-result");
    assert_eq!(authorization_calls.load(Ordering::SeqCst), 1);
    let observed = observed_request
        .lock()
        .expect("authorization request lock")
        .take()
        .expect("authorization request");
    assert_eq!(
        observed.0.get(axum::http::header::AUTHORIZATION),
        Some(&axum::http::HeaderValue::from_static(
            "Bearer runtime-grant"
        ))
    );
    assert_eq!(observed.1["task_id"], "task-1");
    assert_eq!(observed.1["executor_id"], "runner-selected");
    assert_eq!(
        *dispatch
            .inserted_edge_agent_ids
            .lock()
            .expect("inserted edge agent ids lock"),
        vec!["runner-selected".to_string()]
    );
    server.abort();
}

#[tokio::test]
async fn concurrent_authorized_retries_only_reauthorize_the_durable_claimant() {
    let authorization_calls = Arc::new(AtomicUsize::new(0));
    let calls = Arc::clone(&authorization_calls);
    let app = axum::Router::new().route(
        "/api/v1/runtime-executors/authorize",
        axum::routing::post(move || {
            let calls = Arc::clone(&calls);
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                axum::http::StatusCode::NO_CONTENT
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind authorization server");
    let endpoint_url = format!(
        "http://{}/api/v1/runtime-executors/authorize",
        listener.local_addr().expect("authorization server address")
    );
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve authorization response");
    });

    let pool = astra_server_types::edge_connection_pool::EdgeConnectionPool::new();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<astra_server_types::EdgeServerMessage>(2);
    pool.register_with_capabilities(
        "user-1",
        "runner-selected",
        Some("Runner".to_string()),
        Some("/workspace".to_string()),
        Some(edge_runtime_environment_advertisement("runner-selected")),
        None,
        tx,
    );
    let dispatch = Arc::new(StaticEdgeDispatch::default());
    let service = Arc::new(
        ToolExecutionService::builder()
            .edge_connection_pool(pool.clone())
            .edge_dispatch_service(dispatch.clone())
            .build(),
    );
    let mut tool_request = request(
        "bash",
        WorkspaceBinding::edge_workspace("Runner", "/workspace", WorkspaceAuthority::ReadWrite),
        ExecutorBinding::edge_agent(
            "runner-selected",
            "Runner",
            ToolTransportKind::EdgeWs,
            ExecutorStatus::Online,
        ),
    );
    tool_request.runtime_edge_dispatch_authorization = Some(Arc::new(
        astra_services::runs::RuntimeEdgeDispatchAuthorizationContext {
            endpoint_url,
            authorization: "Bearer runtime-grant".to_string(),
            task_id: "task-1".to_string(),
            executor_id: "runner-selected".to_string(),
        },
    ));
    tool_request.runtime_edge_dispatch_authorization_required = true;

    let start = Arc::new(tokio::sync::Barrier::new(3));
    let mut handles = Vec::new();
    for _ in 0..2 {
        let service = Arc::clone(&service);
        let request = tool_request.clone();
        let start = Arc::clone(&start);
        handles.push(tokio::spawn(async move {
            start.wait().await;
            service
                .execute(request, &CountingLocalTransport::new())
                .await
        }));
    }
    start.wait().await;

    let message = rx.recv().await.expect("claimed edge tool request");
    let (request_id, delivery_generation) = match message {
        astra_server_types::EdgeServerMessage::ToolRequest {
            request_id,
            delivery_generation,
            ..
        } => (request_id, delivery_generation),
        other => panic!("expected tool request, got {other:?}"),
    };
    assert!(pool.deliver_tool_result(
        "user-1",
        "runner-selected",
        &request_id,
        delivery_generation,
        astra_server_types::edge_connection_pool::EdgeToolResult {
            output: "authorized-result".to_string(),
            is_error: false,
            duration_ms: Some(3),
            tool_result_fields: None,
        },
    ));

    let mut outputs = Vec::new();
    for handle in handles {
        let result = handle.await.expect("authorized retry join");
        assert!(!result.is_error, "{result:?}");
        outputs.push(result.output);
    }
    outputs.sort();
    assert_eq!(
        outputs,
        ["authorized-result".to_string(), "ledger-result".to_string()]
    );
    assert_eq!(
        authorization_calls.load(Ordering::SeqCst),
        1,
        "only the caller holding the durable dispatch claim may reauthorize"
    );
    assert!(
        rx.try_recv().is_err(),
        "the observing retry must not emit a second socket dispatch"
    );
    server.abort();
}

#[tokio::test]
async fn revoked_provider_executor_authorization_closes_claim_before_socket_dispatch() {
    let authorization_calls = Arc::new(AtomicUsize::new(0));
    let calls = Arc::clone(&authorization_calls);
    let app = axum::Router::new().route(
        "/api/v1/runtime-executors/authorize",
        axum::routing::post(move || {
            let calls = Arc::clone(&calls);
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                axum::http::StatusCode::FORBIDDEN
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind authorization server");
    let endpoint_url = format!(
        "http://{}/api/v1/runtime-executors/authorize",
        listener.local_addr().expect("authorization server address")
    );
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve authorization response");
    });

    let pool = astra_server_types::edge_connection_pool::EdgeConnectionPool::new();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<astra_server_types::EdgeServerMessage>(1);
    pool.register_with_capabilities(
        "user-1",
        "runner-selected",
        Some("Runner".to_string()),
        Some("/workspace".to_string()),
        Some(edge_runtime_environment_advertisement("runner-selected")),
        None,
        tx,
    );
    let dispatch = Arc::new(StaticEdgeDispatch::default());
    let service = ToolExecutionService::builder()
        .edge_connection_pool(pool)
        .edge_dispatch_service(dispatch.clone())
        .build();
    let mut tool_request = request(
        "bash",
        WorkspaceBinding::edge_workspace("Runner", "/workspace", WorkspaceAuthority::ReadWrite),
        ExecutorBinding::edge_agent(
            "runner-selected",
            "Runner",
            ToolTransportKind::EdgeWs,
            ExecutorStatus::Online,
        ),
    );
    tool_request.runtime_edge_dispatch_authorization = Some(Arc::new(
        astra_services::runs::RuntimeEdgeDispatchAuthorizationContext {
            endpoint_url,
            authorization: "Bearer runtime-grant".to_string(),
            task_id: "task-1".to_string(),
            executor_id: "runner-selected".to_string(),
        },
    ));
    tool_request.runtime_edge_dispatch_authorization_required = true;

    let result = service
        .execute(tool_request, &CountingLocalTransport::new())
        .await;

    assert!(result.is_error, "{result:?}");
    assert_eq!(authorization_calls.load(Ordering::SeqCst), 1);
    assert!(
        rx.try_recv().is_err(),
        "revoked use must not reach the Edge"
    );
    assert_eq!(
        dispatch
            .inserted_edge_agent_ids
            .lock()
            .expect("inserted edge agent ids lock")
            .as_slice(),
        ["runner-selected"],
        "authorization must run only after one caller owns the durable claim"
    );
    assert_eq!(
        dispatch
            .failed_dispatches
            .lock()
            .expect("failed dispatches lock")
            .len(),
        1,
        "a denied claim must become terminal before it can reach the Edge"
    );
    server.abort();
}

#[tokio::test]
async fn completed_authorized_dispatch_replays_without_provider_reauthorization() {
    let pool = astra_server_types::edge_connection_pool::EdgeConnectionPool::new();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<astra_server_types::EdgeServerMessage>(1);
    pool.register_with_capabilities(
        "user-1",
        "runner-selected",
        Some("Runner".to_string()),
        Some("/workspace".to_string()),
        Some(edge_runtime_environment_advertisement("runner-selected")),
        None,
        tx,
    );
    let dispatch = Arc::new(StaticEdgeDispatch::terminal_admission("durable-result"));
    let service = ToolExecutionService::builder()
        .edge_connection_pool(pool)
        .edge_dispatch_service(dispatch)
        .build();
    let mut tool_request = request(
        "bash",
        WorkspaceBinding::edge_workspace("Runner", "/workspace", WorkspaceAuthority::ReadWrite),
        ExecutorBinding::edge_agent(
            "runner-selected",
            "Runner",
            ToolTransportKind::EdgeWs,
            ExecutorStatus::Online,
        ),
    );
    tool_request.runtime_edge_dispatch_authorization = Some(Arc::new(
        astra_services::runs::RuntimeEdgeDispatchAuthorizationContext {
            endpoint_url: "http://127.0.0.1:9/must-not-be-called".to_string(),
            authorization: "Bearer expired-runtime-grant".to_string(),
            task_id: "task-1".to_string(),
            executor_id: "runner-selected".to_string(),
        },
    ));
    tool_request.runtime_edge_dispatch_authorization_required = true;

    let result = service
        .execute(tool_request, &CountingLocalTransport::new())
        .await;

    assert!(!result.is_error, "{result:?}");
    assert_eq!(result.output, "durable-result");
    assert!(
        rx.try_recv().is_err(),
        "terminal replay must not use the socket"
    );
}

#[tokio::test]
async fn authorized_provider_executor_cannot_fall_back_to_server_sandbox_local_execution() {
    let service = ToolExecutionService::builder().build();
    let local = CountingLocalTransport::new();
    let mut tool_request = request(
        "bash",
        WorkspaceBinding::server_sandbox("/tmp/astra-workspace"),
        ExecutorBinding::edge_agent(
            "runner-selected",
            "Runner",
            ToolTransportKind::EdgeWs,
            ExecutorStatus::Online,
        ),
    );
    tool_request.runtime_edge_dispatch_authorization = Some(Arc::new(
        astra_services::runs::RuntimeEdgeDispatchAuthorizationContext {
            endpoint_url: "http://127.0.0.1/api/v1/runtime-executors/authorize".to_string(),
            authorization: "Bearer runtime-grant".to_string(),
            task_id: "task-1".to_string(),
            executor_id: "runner-selected".to_string(),
        },
    ));
    tool_request.runtime_edge_dispatch_authorization_required = true;

    let result = service.execute(tool_request, &local).await;

    assert!(result.is_error, "{result:?}");
    assert_eq!(
        local.calls(),
        0,
        "authorized edge execution must fail closed before local transport"
    );
    let metadata = result.metadata.expect("route rejection metadata");
    assert_eq!(metadata["execution_started"], false);
    assert_eq!(metadata["side_effects_maybe"], false);
}

#[tokio::test]
async fn replayed_authorized_provider_executor_without_context_fails_before_dispatch() {
    let pool = astra_server_types::edge_connection_pool::EdgeConnectionPool::new();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<astra_server_types::EdgeServerMessage>(1);
    pool.register_with_capabilities(
        "user-1",
        "runner-selected",
        Some("Runner".to_string()),
        Some("/workspace".to_string()),
        Some(edge_runtime_environment_advertisement("runner-selected")),
        None,
        tx,
    );
    let dispatch = Arc::new(StaticEdgeDispatch::default());
    let service = ToolExecutionService::builder()
        .edge_connection_pool(pool)
        .edge_dispatch_service(dispatch.clone())
        .build();
    let local = CountingLocalTransport::new();
    let mut original = request(
        "bash",
        WorkspaceBinding::edge_workspace("Runner", "/workspace", WorkspaceAuthority::ReadWrite),
        ExecutorBinding::edge_agent(
            "runner-selected",
            "Runner",
            ToolTransportKind::EdgeWs,
            ExecutorStatus::Online,
        ),
    );
    original.runtime_edge_dispatch_authorization = Some(Arc::new(
        astra_services::runs::RuntimeEdgeDispatchAuthorizationContext {
            endpoint_url: "http://127.0.0.1/api/v1/runtime-executors/authorize".to_string(),
            authorization: "Bearer runtime-grant".to_string(),
            task_id: "task-1".to_string(),
            executor_id: "runner-selected".to_string(),
        },
    ));
    original.runtime_edge_dispatch_authorization_required = true;

    let snapshot = serde_json::to_value(&original).expect("serialize tool snapshot");
    assert!(
        snapshot
            .get("runtime_edge_dispatch_authorization")
            .is_none(),
        "provider authorization context must never enter a durable snapshot"
    );
    assert_eq!(
        snapshot["runtime_edge_dispatch_authorization_required"],
        true
    );
    assert!(
        !snapshot.to_string().contains("runtime-grant"),
        "provider bearer must never enter a durable snapshot"
    );
    let replayed: ToolExecutionRequest =
        serde_json::from_value(snapshot).expect("deserialize tool snapshot");
    assert!(replayed.runtime_edge_dispatch_authorization.is_none());
    assert!(replayed.runtime_edge_dispatch_authorization_required);

    let result = service.execute(replayed, &local).await;

    assert!(result.is_error, "{result:?}");
    assert_eq!(local.calls(), 0);
    assert!(rx.try_recv().is_err(), "replay must not reach the Edge");
    assert!(
        dispatch
            .inserted_edge_agent_ids
            .lock()
            .expect("inserted edge agent ids lock")
            .is_empty(),
        "replay without authorization context must not create a durable dispatch row"
    );
    let metadata = result.metadata.expect("authorization rejection metadata");
    assert_eq!(metadata["execution_started"], false);
    assert_eq!(metadata["side_effects_maybe"], false);
}

#[tokio::test]
async fn terminal_durable_dispatch_replays_without_socket_redispatch() {
    let pool = astra_server_types::edge_connection_pool::EdgeConnectionPool::new();
    let dispatch = Arc::new(StaticEdgeDispatch::terminal_admission("durable-result"));
    let (tx, mut rx) = tokio::sync::mpsc::channel::<astra_server_types::EdgeServerMessage>(1);
    pool.register_with_capabilities(
        "user-1",
        "edge-selected",
        Some("MacBook Pro".to_string()),
        Some("/Users/test/project".to_string()),
        Some(edge_runtime_environment_advertisement("edge-selected")),
        None,
        tx,
    );
    let service = ToolExecutionService::builder()
        .edge_connection_pool(pool)
        .edge_dispatch_service(dispatch)
        .build();

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
            &CountingLocalTransport::new(),
        )
        .await;

    assert!(!result.is_error, "{result:?}");
    assert_eq!(result.output, "durable-result");
    assert!(
        rx.try_recv().is_err(),
        "terminal durable evidence must replay without crossing the socket boundary again"
    );
    assert_eq!(result.metadata.unwrap()["transport"], "edge_ledger");
}

#[tokio::test]
async fn workspace_service_account_edge_separates_route_owner_from_invocation_owner() {
    let pool = astra_server_types::edge_connection_pool::EdgeConnectionPool::new();
    let dispatch = Arc::new(StaticEdgeDispatch::default());
    let (tx, mut rx) = tokio::sync::mpsc::channel::<astra_server_types::EdgeServerMessage>(1);
    pool.register_with_capabilities(
        "sandbox-service-account",
        "edge-selected",
        Some("sandbox".to_string()),
        Some("/workspace".to_string()),
        Some(edge_runtime_environment_advertisement("edge-selected")),
        Some("workspace-1".to_string()),
        tx,
    );
    let service = ToolExecutionService::builder()
        .edge_connection_pool(pool.clone())
        .edge_dispatch_service(dispatch.clone())
        .build();
    let mut tool_request = request(
        "bash",
        WorkspaceBinding::edge_workspace("sandbox", "/workspace", WorkspaceAuthority::ReadWrite),
        ExecutorBinding::edge_agent(
            "edge-selected",
            "sandbox",
            ToolTransportKind::EdgeWs,
            ExecutorStatus::Online,
        ),
    );
    tool_request.workspace_record = Some(astra_runtime_env::WorkspaceRecord {
        workspace_id: "workspace-1".to_string(),
        owner_scope: astra_runtime_env::WorkspaceOwnerScope::Executor,
        kind: astra_runtime_env::WorkspaceBindingKind::EdgeWorkspace,
        authority: astra_runtime_env::WorkspaceAuthority::ReadWrite,
        root_or_volume_ref: "/workspace".to_string(),
        source: astra_runtime_env::WorkspaceSource::EdgePath {
            executor_id: "edge-selected".to_string(),
            path: "/workspace".to_string(),
        },
        persistence: astra_runtime_env::WorkspacePersistence::Persistent,
        revision: "rev-1".to_string(),
        display_name: "sandbox".to_string(),
    });

    let handle = tokio::spawn(async move {
        service
            .execute(tool_request, &CountingLocalTransport::new())
            .await
    });

    let message = rx.recv().await.expect("service-account edge tool request");
    let (request_id, delivery_generation, identity) = match message {
        astra_server_types::EdgeServerMessage::ToolRequest {
            request_id,
            delivery_generation,
            identity,
            ..
        } => (request_id, delivery_generation, identity),
        other => panic!("expected tool request, got {other:?}"),
    };
    assert_eq!(
        identity.user_id, "user-1",
        "the protocol must preserve the logical invocation owner"
    );
    let admitted = dispatch
        .inserted_identities
        .lock()
        .expect("inserted identities lock");
    assert_eq!(admitted.len(), 1);
    assert_eq!(
        admitted[0].user_id, "sandbox-service-account",
        "the durable dispatch row must use the authenticated route owner"
    );
    drop(admitted);

    assert!(pool.deliver_tool_result(
        "sandbox-service-account",
        "edge-selected",
        &request_id,
        delivery_generation,
        astra_server_types::edge_connection_pool::EdgeToolResult {
            output: "sandbox-result".to_string(),
            is_error: false,
            duration_ms: Some(3),
            tool_result_fields: None,
        },
    ));
    let result = handle.await.expect("edge execution join");
    assert!(!result.is_error, "{result:?}");
    assert_eq!(result.output, "sandbox-result");
}

#[tokio::test]
async fn edge_ws_result_preserves_tool_result_fields() {
    let pool = astra_server_types::edge_connection_pool::EdgeConnectionPool::new();
    let dispatch = Arc::new(StaticEdgeDispatch::default());
    let (tx, mut rx) = tokio::sync::mpsc::channel::<astra_server_types::EdgeServerMessage>(1);
    pool.register_with_capabilities(
        "user-1",
        "edge-selected",
        Some("MacBook Pro".to_string()),
        Some("/Users/test/project".to_string()),
        Some(edge_runtime_environment_advertisement("edge-selected")),
        None,
        tx,
    );
    let service = ToolExecutionService::builder()
        .edge_connection_pool(pool.clone())
        .edge_dispatch_service(dispatch.clone())
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
    assert_eq!(
        *dispatch
            .inserted_edge_agent_ids
            .lock()
            .expect("inserted edge agent ids lock"),
        vec!["edge-selected".to_string()],
        "the durable dispatch row must exist before direct socket delivery"
    );
    let (request_id, delivery_generation) = match message {
        astra_server_types::EdgeServerMessage::ToolRequest {
            request_id,
            delivery_generation,
            ..
        } => (request_id, delivery_generation),
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
        delivery_generation,
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
async fn unscoped_edge_dispatch_does_not_route_to_another_users_registry_record() {
    let dispatch = Arc::new(StaticEdgeDispatch::default());
    let mut other_users_agent = edge_agent_record("edge-selected");
    other_users_agent.user_id = "user-2".to_string();
    let service = ToolExecutionService::builder()
        .edge_dispatch_service(dispatch.clone())
        .edge_registry_service(Arc::new(StaticEdgeRegistry {
            agents: vec![other_users_agent],
        }))
        .build();

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
            &CountingLocalTransport::new(),
        )
        .await;

    assert!(result.is_error, "{result:?}");
    assert!(
        dispatch
            .inserted_edge_agent_ids
            .lock()
            .expect("inserted edge agent ids lock")
            .is_empty(),
        "an unscoped durable lookup must remain within the requesting user"
    );
}

#[tokio::test]
async fn edge_dispatch_failed_result_reports_tool_error() {
    let dispatch = Arc::new(StaticEdgeDispatch::failed_result());
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
    let claimed_identity = tokio::time::timeout(std::time::Duration::from_secs(1), async move {
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
        let identity = row.identity();
        let message: astra_server_types::edge_ws_protocol::EdgeServerMessage =
            serde_json::from_str(&row.payload_json).expect("dispatch payload should be WS message");
        match message {
            astra_server_types::edge_ws_protocol::EdgeServerMessage::ToolRequest {
                request_id,
                tool,
                args,
                timeout_secs,
                ..
            } => {
                assert_eq!(request_id, row.request_id);
                assert_eq!(tool, "bash");
                assert_eq!(args, serde_json::json!({}));
                assert!(timeout_secs > 0);
            }
            other => panic!("expected tool request payload, got {other:?}"),
        }
        identity
    })
    .await
    .expect("edge WS pod should poll pending dispatch before timeout");
    assert_eq!(
        dispatch
            .status_for("user-1", &claimed_identity.request_id)
            .as_deref(),
        Some("dispatched")
    );

    let tool_result = astra_thin_client::ToolResultRequest::new_with_hash(
        astra_thin_client::ToolResultRequestParts {
            session_id: claimed_identity.session_id.clone(),
            run_id: claimed_identity.run_id.clone(),
            turn_chain_id: claimed_identity.turn_chain_id.clone(),
            request_id: claimed_identity.request_id.clone(),
            edge_agent_id: "edge-selected".to_string(),
            status: "completed".to_string(),
            output: "no-sticky-result".to_string(),
            duration_ms: 9,
            tool_result_fields: None,
        },
    );
    let result_json =
        serde_json::to_string(&tool_result).expect("tool result should serialize for callback pod");
    let delivered = astra_services::multi_agent::EdgeDispatchService::deliver_result(
        dispatch.as_ref(),
        &claimed_identity,
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
            .status_for("user-1", &claimed_identity.request_id)
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
async fn edge_admission_rejection_is_reported_as_certain_and_is_not_retried() {
    let dispatch = Arc::new(StaticEdgeDispatch::admission_rejected(
        "durable identity conflicts with its owner",
    ));
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
    let metadata = result.metadata.expect("admission rejection metadata");
    assert_eq!(metadata["error_kind"], TOOL_ERROR_KIND_ROUTE_MISMATCH);
    assert_eq!(metadata["reason"], TOOL_ERROR_KIND_ROUTE_MISMATCH);
    assert!(
        metadata.get("outcome_certainty").is_none(),
        "a proven admission rejection must not claim the tool may have executed"
    );
    assert!(
        result.output.contains("The tool was not dispatched"),
        "{}",
        result.output
    );
    assert_eq!(
        local.calls(),
        0,
        "an edge-bound tool must never fall back locally"
    );
    assert!(
        dispatch
            .inserted_edge_agent_ids
            .lock()
            .expect("inserted edge agent ids lock")
            .is_empty(),
        "the rejected admission fixture proves no durable dispatch was created"
    );
}

#[tokio::test]
async fn ambiguous_admission_commit_is_reported_as_outcome_unknown() {
    let dispatch = Arc::new(StaticEdgeDispatch::admission_outcome_unknown(
        "database response was lost after insert",
    ));
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
    let metadata = result.metadata.expect("ambiguous admission metadata");
    assert_eq!(
        metadata["error_kind"],
        TOOL_ERROR_KIND_TRANSPORT_DISCONNECTED
    );
    assert_eq!(metadata["outcome_certainty"], "unknown");
    assert_eq!(metadata["side_effects_maybe"], true);
    assert!(result.output.contains("reconcile its durable invocation"));
    assert_eq!(
        metadata["diagnostics"][0],
        "edge-dispatch: admission outcome is unknown: database response was lost after insert"
    );
    assert_eq!(
        local.calls(),
        0,
        "unknown outcome must never be retried locally"
    );
    assert_eq!(
        *dispatch
            .inserted_edge_agent_ids
            .lock()
            .expect("inserted edge agent ids lock"),
        vec!["edge-selected".to_string()],
        "the fixture models an insert that may have committed before the response was lost"
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
    assert_eq!(metadata["side_effects_maybe"], true);
    assert_eq!(metadata["outcome_certainty"], "unknown");
    assert!(result.output.contains("reconcile its durable invocation"));
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
    let server_runtime_tools = ["memory", "mo_query", "rollback_database_snapshots"];

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

    let control_plane_request = request(
        "tool_search",
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
        service.routing_decision(&control_plane_request),
        ToolExecutionRouteKind::ServerControlPlane,
        "tool_search is control-plane backbone and must not depend on edge transport"
    );
    let result = service.execute(control_plane_request, &local).await;
    assert!(!result.is_error, "tool_search: {result:?}");
    assert_eq!(result.output, "local:tool_search");
    assert_eq!(local.calls(), server_runtime_tools.len() + 1);
}

#[tokio::test]
async fn shared_network_tools_use_server_without_runtime_and_edge_with_edge_binding() {
    let dispatch = Arc::new(StaticEdgeDispatch::default());
    let service = ToolExecutionService::builder()
        .edge_dispatch_service(dispatch.clone())
        .edge_registry_service(Arc::new(StaticEdgeRegistry {
            agents: vec![edge_agent_record("edge-selected")],
        }))
        .initial_provider_capabilities(HashMap::from([(
            crate::server::tool_execution_service::SERVER_OPTIONAL_TOOL_PROVIDER_ID.to_string(),
            HashSet::from([astra_core::PROVIDER_CAPABILITY_PUBLIC_NETWORK.to_string()]),
        )]))
        .build();
    let local = CountingLocalTransport::new();

    for tool in ["web_fetch", "web_search"] {
        let server_request = request(
            tool,
            WorkspaceBinding::none(),
            ExecutorBinding::server_local(),
        )
        .with_selected_offer(SelectedToolOfferSnapshot::new_with_route(
            tool,
            crate::server::tool_execution_service::SERVER_OPTIONAL_TOOL_PROVIDER_ID,
            ToolExecutionRouteKind::ServerRuntime,
        ));
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
async fn disabled_shared_network_server_offer_does_not_block_explicit_edge_offer() {
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
            )
            .with_selected_offer(SelectedToolOfferSnapshot::new_with_route(
                "web_fetch",
                crate::server::tool_execution_service::SERVER_OPTIONAL_TOOL_PROVIDER_ID,
                ToolExecutionRouteKind::ServerRuntime,
            )),
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
            )
            .with_selected_offer(SelectedToolOfferSnapshot::new_with_route(
                "web_fetch",
                "edge-selected",
                ToolExecutionRouteKind::EdgeBound,
            )),
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
async fn selected_offer_route_mismatch_blocks_execution_without_provider_fallback() {
    let service = ToolExecutionService::new_for_test();
    let local = CountingLocalTransport::new();
    let request = request(
        "web_fetch",
        WorkspaceBinding::none(),
        ExecutorBinding::server_local(),
    )
    .with_selected_offer(SelectedToolOfferSnapshot::new_with_route(
        "web_fetch",
        "edge-selected",
        ToolExecutionRouteKind::EdgeBound,
    ));

    assert_eq!(
        service.routing_decision(&request),
        ToolExecutionRouteKind::EdgeBound,
        "selected offer route is the execution source of truth"
    );
    let boundary = crate::server::tool_route_boundary::ToolRouteBoundary::new(
        request,
        ToolExecutionRouteKind::ServerRuntime,
    );
    let result = service
        .execute_boundary_with_cancel(&boundary, &local, None)
        .await;

    assert!(result.is_error, "{result:?}");
    assert_eq!(
        local.calls(),
        0,
        "selected offer route mismatch must block before server fallback execution"
    );
    assert!(
        result.output.contains("Refusing to run"),
        "{}",
        result.output
    );
    let metadata = result.metadata.expect("route mismatch metadata");
    assert_eq!(metadata["error_kind"], TOOL_ERROR_KIND_ROUTE_MISMATCH);
    assert_eq!(metadata["runtime_error"]["kind"], "route_mismatch");
    assert_eq!(
        metadata["selected_tool_offer"]["offer_id"],
        "web_fetch@edge-selected"
    );
    assert_eq!(metadata["selected_tool_offer"]["route"], "edge_bound");
    assert_eq!(metadata["actual_route"], "server_runtime");
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
    let local_code_tools = ["bash", "read_file", "list_dir", "grep", "glob", "worktree"];

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
    let edge_request = request_scoped_mcp_request("mcp__demo__search");

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
    assert_eq!(metadata["workspace"]["kind"], "none");
    assert_eq!(metadata["executor"]["kind"], "mcp");
    assert_eq!(metadata["executor"]["executor_id"], "request-scoped-mcp");
    assert_eq!(metadata["executor"]["display_name"], "Request-scoped MCP");
    assert_eq!(metadata["executor"]["transport"], "mcp_http");
    assert_eq!(metadata["transport"], "mcp_http");
}

#[tokio::test]
async fn request_scoped_mcp_execution_requires_selected_offer_snapshot() {
    let service = ToolExecutionService::new_for_test();
    let local = CountingLocalTransport::new();
    let result = service
        .execute(
            request(
                "mcp__demo__search",
                WorkspaceBinding::none(),
                ExecutorBinding::request_scoped_mcp(),
            ),
            &local,
        )
        .await;

    assert!(result.is_error, "{result:?}");
    assert_eq!(local.calls(), 0);
    let metadata = result.metadata.expect("missing selected offer metadata");
    assert_eq!(metadata["error_kind"], TOOL_ERROR_KIND_CAPABILITY_DENIED);
    assert_eq!(metadata["executor"]["kind"], "mcp");
    assert_eq!(metadata["transport"], "mcp_http");
}

#[tokio::test]
async fn disabled_request_scoped_mcp_offer_blocks_selected_offer_without_schema_inventory() {
    let service = ToolExecutionService::builder()
        .initial_disabled_tool_offers(&["mcp__demo__search@request-scoped-mcp".to_string()])
        .build();
    let local = CountingLocalTransport::new();
    let result = service
        .execute(request_scoped_mcp_request("mcp__demo__search"), &local)
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
async fn request_scoped_mcp_provider_allowlist_blocks_selected_offer_without_schema_inventory() {
    let service = ToolExecutionService::builder()
        .initial_provider_allowed_tools(HashMap::from([(
            "request-scoped-mcp".to_string(),
            HashSet::from(["mcp__demo__allowed".to_string()]),
        )]))
        .build();
    let local = CountingLocalTransport::new();
    let result = service
        .execute(request_scoped_mcp_request("mcp__demo__search"), &local)
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
        WorkspaceBinding::none(),
        ExecutorBinding::request_scoped_mcp(),
    )
    .with_selected_offer(SelectedToolOfferSnapshot::new(
        "mcp__rag__retrieve",
        "request-scoped-mcp",
    ));

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
async fn online_executor_without_durable_authority_is_unavailable_before_dispatch() {
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
        result
            .output
            .contains("unavailable before tool 'bash' was dispatched")
    );
    let metadata = result.metadata.expect("diagnostics metadata");
    assert_eq!(
        metadata["error_kind"],
        TOOL_ERROR_KIND_TRANSPORT_UNAVAILABLE
    );
    assert_eq!(metadata["reason"], TOOL_ERROR_KIND_TRANSPORT_UNAVAILABLE);
    assert_eq!(metadata["executor"]["status"], "degraded");
    assert_eq!(metadata["workspace"]["kind"], "edge_workspace");
    assert_eq!(metadata["execution_started"], false);
    assert_eq!(metadata["side_effects_maybe"], false);
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
        runtime_process_authorization: None,
        runtime_process_authorization_required: false,
        runtime_edge_dispatch_authorization: None,
        runtime_edge_dispatch_authorization_required: false,
        tool_name: "bash".to_string(),
        args: serde_json::json!({"cmd": "ls"}),
        user_id: "test-user".to_string(),
        run_id: "run-1".to_string(),
        session_id: "session-1".to_string(),
        turn_chain_id: "chain-1".to_string(),
        tool_call_id: "tc-1".to_string(),
        selected_offer: None,
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
        runtime_process_authorization: None,
        runtime_process_authorization_required: false,
        runtime_edge_dispatch_authorization: None,
        runtime_edge_dispatch_authorization_required: false,
        tool_name: "bash".to_string(),
        args: serde_json::json!({"cmd": "ls"}),
        user_id: "test-user".to_string(),
        run_id: "run-1".to_string(),
        session_id: "session-1".to_string(),
        turn_chain_id: "chain-1".to_string(),
        tool_call_id: "tc-1".to_string(),
        selected_offer: None,
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
        runtime_process_authorization: None,
        runtime_process_authorization_required: false,
        runtime_edge_dispatch_authorization: None,
        runtime_edge_dispatch_authorization_required: false,
        tool_name: "bash".to_string(),
        args: serde_json::json!({"cmd": "ls"}),
        user_id: "test-user".to_string(),
        run_id: "run-1".to_string(),
        session_id: "session-1".to_string(),
        turn_chain_id: "chain-1".to_string(),
        tool_call_id: "tc-1".to_string(),
        selected_offer: None,
        policy: ToolPolicySnapshot::default(),
    };
    assert_eq!(edge_executor_id(&request), Some("valid-edge-123"));
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

#[tokio::test]
async fn unavailable_provider_routes_preserve_pre_dispatch_cancellation() {
    for request in [
        openshell_gateway_request("bash"),
        cloud_snapshot_request("read_file"),
    ] {
        let service = ToolExecutionService::new_for_test();
        let local = CountingLocalTransport::new();
        let cancel = Arc::new(CancellationToken::new());
        cancel.cancel();
        let result = service
            .execute_with_cancel(request, &local, Some(cancel))
            .await;
        assert!(result.is_error);
        assert_eq!(local.calls(), 0);
        assert_eq!(
            result.exit_semantics,
            Some(astra_tools::exit_semantics::ExitSemantics::ExecutionError)
        );
        let metadata = result.metadata.expect("cancel metadata");
        assert_eq!(metadata["error_kind"], TOOL_ERROR_KIND_CANCELLED);
        assert_eq!(metadata["execution_started"], false);
        assert_eq!(metadata["side_effects_maybe"], false);
    }
}
