use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use astra_runtime_env::{
    runtime_result_fields_with_policy_evidence, validate_runtime_session_spec, CapabilityResolver,
    CompiledRuntimePolicy, EffectiveCapabilitySet, ExecutorBinding, RunnerAckResponse,
    RunnerCapacity, RunnerDenial, RunnerDenialReason, RunnerDestroySessionRequest,
    RunnerDestroySessionResponse, RunnerExecuteToolRequest, RunnerExecuteToolResponse,
    RunnerHeartbeat, RunnerIdentity, RunnerPrepareSessionRequest, RunnerPrepareSessionResponse,
    RunnerProtocol, RunnerRegisterRequest, RunnerRegisterResponse, RunnerRpcEndpoint, RunnerStatus,
    RuntimeBinding, RuntimeEnvironment, RuntimeEnvironmentAdvertisement, RuntimeError,
    RuntimeErrorKind, RuntimeIsolationBackend, RuntimeSessionHandle, RuntimeSessionManager,
    RuntimeSessionSpec, RuntimeSessionStatus, RuntimeToolInvocation, RuntimeToolOutcome,
    ToolRegistry, WorkspaceAuthority, WorkspaceBinding,
};
use astra_tools::{executor::DefaultToolExecutor, ToolExecutor};
use async_trait::async_trait;
use axum::{
    extract::State,
    routing::{get, post},
    Json, Router,
};

#[derive(Debug, Clone)]
pub struct LocalRunnerConfig {
    pub runner_id: String,
    pub owner_id: Option<String>,
    pub workspace_dir: PathBuf,
    pub authority: WorkspaceAuthority,
    pub max_sessions: u32,
    pub rpc_base_url: Option<String>,
}

impl LocalRunnerConfig {
    pub fn new(runner_id: impl Into<String>, workspace_dir: impl Into<PathBuf>) -> Self {
        Self {
            runner_id: runner_id.into(),
            owner_id: None,
            workspace_dir: workspace_dir.into(),
            authority: WorkspaceAuthority::ReadWrite,
            max_sessions: 1,
            rpc_base_url: None,
        }
    }

    pub fn with_owner_id(mut self, owner_id: impl Into<String>) -> Self {
        self.owner_id = Some(owner_id.into());
        self
    }

    pub fn with_authority(mut self, authority: WorkspaceAuthority) -> Self {
        self.authority = authority;
        self
    }

    pub fn with_max_sessions(mut self, max_sessions: u32) -> Self {
        self.max_sessions = max_sessions;
        self
    }

    pub fn with_rpc_base_url(mut self, rpc_base_url: impl Into<String>) -> Self {
        self.rpc_base_url = Some(rpc_base_url.into());
        self
    }

    pub fn canonical_workspace_dir(&self) -> PathBuf {
        canonical_workspace_dir(&self.workspace_dir)
    }
}

pub fn canonical_workspace_dir(workspace_dir: &Path) -> PathBuf {
    workspace_dir
        .canonicalize()
        .unwrap_or_else(|_| workspace_dir.to_path_buf())
}

pub fn local_runner_binding(config: &LocalRunnerConfig) -> astra_runtime_env::RunBinding {
    let registry = ToolRegistry::builtins();
    let workspace = config
        .canonical_workspace_dir()
        .to_string_lossy()
        .to_string();
    let policy = match config.authority {
        WorkspaceAuthority::ReadOnly => {
            let mut policy = astra_runtime_env::PolicyIntent::local_developer();
            policy.filesystem = astra_runtime_env::FilesystemPolicy::ReadOnlyWorkspace;
            policy.credentials = astra_runtime_env::CredentialPolicy::Disabled;
            policy
        }
        WorkspaceAuthority::ReadWrite => astra_runtime_env::PolicyIntent::local_developer(),
        WorkspaceAuthority::None | WorkspaceAuthority::Unknown | _ => {
            astra_runtime_env::PolicyIntent::cloud_control_plane()
        }
    };

    astra_runtime_env::RunBinding::resolve(
        WorkspaceBinding::local_filesystem(workspace, config.authority),
        ExecutorBinding::personal_runner(config.runner_id.clone()),
        RuntimeBinding::host_process(format!("runner-host:{}", config.runner_id)),
        policy,
        &registry,
    )
}

pub fn local_runner_advertisement(config: &LocalRunnerConfig) -> RuntimeEnvironmentAdvertisement {
    RuntimeEnvironmentAdvertisement::new(local_runner_binding(config))
}

pub fn local_runner_register_request(config: &LocalRunnerConfig) -> RunnerRegisterRequest {
    let identity = RunnerIdentity::personal(
        config.runner_id.clone(),
        config
            .owner_id
            .clone()
            .unwrap_or_else(|| "local-user".to_string()),
    );
    let mut request = RunnerRegisterRequest::new(
        identity,
        RunnerCapacity {
            max_sessions: config.max_sessions,
            active_sessions: 0,
        },
        local_runner_advertisement(config),
    );
    if let Some(base_url) = config.rpc_base_url.as_deref() {
        request = request.with_rpc_endpoint(RunnerRpcEndpoint::new(base_url));
    }
    request
}

pub struct LocalRunnerEnvironment {
    config: LocalRunnerConfig,
    registry: ToolRegistry,
    sessions: tokio::sync::Mutex<HashMap<String, RuntimeSessionHandle>>,
}

impl LocalRunnerEnvironment {
    pub fn new(config: LocalRunnerConfig) -> Self {
        Self {
            config,
            registry: ToolRegistry::builtins(),
            sessions: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    pub fn binding(&self) -> astra_runtime_env::RunBinding {
        local_runner_binding(&self.config)
    }

    pub fn advertisement(&self) -> RuntimeEnvironmentAdvertisement {
        RuntimeEnvironmentAdvertisement::new(self.binding())
    }

    pub fn register_request(&self) -> RunnerRegisterRequest {
        local_runner_register_request(&self.config)
    }

    pub async fn active_session_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.sessions.lock().await.keys().cloned().collect();
        ids.sort();
        ids
    }

    pub async fn capacity(&self) -> RunnerCapacity {
        RunnerCapacity {
            max_sessions: self.config.max_sessions,
            active_sessions: self.sessions.lock().await.len() as u32,
        }
    }

    pub async fn heartbeat_snapshot(&self) -> RunnerHeartbeat {
        let active_session_ids = self.active_session_ids().await;
        let status = if active_session_ids.is_empty() {
            RunnerStatus::Idle
        } else {
            RunnerStatus::Busy
        };
        RunnerHeartbeat {
            runner_id: self.config.runner_id.clone(),
            status,
            capacity: self.capacity().await,
            active_session_ids,
            advertisement: self.advertisement(),
        }
    }

    fn workspace_dir(&self) -> PathBuf {
        self.config.canonical_workspace_dir()
    }

    async fn session_exists(&self, session_id: &str) -> bool {
        self.sessions.lock().await.contains_key(session_id)
    }

    async fn stored_session(&self, session_id: &str) -> Option<RuntimeSessionHandle> {
        self.sessions.lock().await.get(session_id).cloned()
    }

    async fn remove_session(&self, session_id: &str) -> bool {
        self.sessions.lock().await.remove(session_id).is_some()
    }

    fn validate_session_target(&self, spec: &RuntimeSessionSpec) -> Result<(), RuntimeError> {
        let expected = self.binding();
        if spec.binding.executor.executor_id != expected.executor.executor_id {
            return Err(RuntimeError::runner_protocol(format!(
                "session targets executor '{}' but this runner is '{}'",
                spec.binding.executor.executor_id, expected.executor.executor_id
            )));
        }
        if spec.binding.runtime.runtime_id != expected.runtime.runtime_id {
            return Err(RuntimeError::runner_protocol(format!(
                "session targets runtime '{}' but this runner is '{}'",
                spec.binding.runtime.runtime_id, expected.runtime.runtime_id
            )));
        }
        if spec.binding.workspace.cwd != expected.workspace.cwd {
            return Err(RuntimeError::new(
                RuntimeErrorKind::WorkspaceUnavailable,
                "session workspace does not match runner workspace binding",
            ));
        }
        Ok(())
    }
}

#[async_trait]
impl RuntimeEnvironment for LocalRunnerEnvironment {
    fn runtime_binding(&self) -> RuntimeBinding {
        self.binding().runtime
    }

    fn capabilities(&self, binding: &astra_runtime_env::RunBinding) -> EffectiveCapabilitySet {
        binding.capabilities
    }

    async fn prepare_session(
        &self,
        spec: RuntimeSessionSpec,
    ) -> Result<RuntimeSessionHandle, RuntimeError> {
        self.validate_session_target(&spec)?;
        validate_runtime_session_spec(&self.registry, &spec)?;
        let mut sessions = self.sessions.lock().await;
        if !sessions.contains_key(&spec.session_id)
            && sessions.len() as u32 >= self.config.max_sessions
        {
            return Err(RuntimeError::capacity_exhausted(
                "runner session capacity exhausted",
            ));
        }
        let handle = RuntimeSessionHandle::from_spec(&spec);
        sessions.insert(handle.session_id.clone(), handle.clone());
        Ok(handle)
    }

    async fn execute_tool(
        &self,
        session: &RuntimeSessionHandle,
        invocation: RuntimeToolInvocation,
    ) -> Result<RuntimeToolOutcome, RuntimeError> {
        let Some(stored_session) = self.stored_session(&session.session_id).await else {
            return Err(RuntimeError::runtime_unavailable(
                "runtime session is not live on this runner",
            ));
        };
        if session.status != RuntimeSessionStatus::Ready {
            return Err(RuntimeError::runtime_unavailable(
                "runtime session is not ready",
            ));
        }
        if stored_session != *session {
            return Err(RuntimeError::runner_protocol(
                "tool invocation session handle does not match runner state",
            ));
        }
        if invocation.policy_revision != session.policy.revision {
            return Err(RuntimeError::sandbox_recreate_required(
                "tool invocation policy revision does not match runtime session",
            ));
        }
        if invocation.binding.executor.executor_id != self.config.runner_id {
            return Err(RuntimeError::runner_protocol(
                "tool invocation targets a different executor",
            ));
        }

        CapabilityResolver
            .check_tool_call(
                &self.registry,
                &invocation.tool_name,
                &invocation.arguments,
                &invocation.binding.capabilities,
            )
            .map_err(|reason| RuntimeError::tool_unavailable(&invocation.tool_name, reason))?;

        let executor = DefaultToolExecutor::for_workspace(
            &self.workspace_dir(),
            "local-runner",
            &session.session_id,
            "astra-runner",
            Duration::from_secs(60),
        );
        let result = executor
            .execute_with_metadata(&invocation.tool_name, &invocation.arguments)
            .await;
        let mut metadata = result.metadata.unwrap_or_default();
        let side_effects_maybe = self
            .registry
            .get(&invocation.tool_name)
            .map(|spec| spec.effect.writes_workspace || spec.effect.mutates_external_state)
            .unwrap_or(true);
        let policy_evidence = astra_runtime_env::RuntimePolicyEvidence::from_session(
            session,
            true,
            result.is_error && side_effects_maybe,
        );
        metadata.extend(runtime_result_fields_with_policy_evidence(
            &invocation.binding,
            session,
            &policy_evidence,
        ));

        Ok(RuntimeToolOutcome {
            call_id: invocation.call_id,
            tool_name: invocation.tool_name,
            output: result.output,
            is_error: result.is_error,
            metadata,
            execution_started: true,
            side_effects_maybe: result.is_error && side_effects_maybe,
            policy_evidence,
        })
    }

    async fn update_policy(
        &self,
        session: &RuntimeSessionHandle,
        binding: astra_runtime_env::RunBinding,
        policy: CompiledRuntimePolicy,
    ) -> Result<RuntimeSessionHandle, RuntimeError> {
        if !self.session_exists(&session.session_id).await {
            return Err(RuntimeError::runtime_unavailable(
                "runtime session is not live on this runner",
            ));
        }
        policy.require_runtime(&binding.runtime)?;
        if policy.requires_session_recreate_from(&session.policy) {
            return Err(RuntimeError::sandbox_recreate_required(
                "policy change requires a fresh runner session",
            ));
        }
        let updated = session.clone().with_policy(policy, &binding);
        self.sessions
            .lock()
            .await
            .insert(updated.session_id.clone(), updated.clone());
        Ok(updated)
    }

    async fn destroy_session(&self, session: RuntimeSessionHandle) -> Result<(), RuntimeError> {
        self.remove_session(&session.session_id).await;
        Ok(())
    }
}

#[async_trait]
impl RunnerProtocol for LocalRunnerEnvironment {
    async fn register(
        &self,
        request: RunnerRegisterRequest,
    ) -> Result<RunnerRegisterResponse, RuntimeError> {
        let runner_id = request.identity.runner_id.clone();
        if let Err(denial) = request.validate() {
            return Ok(RunnerRegisterResponse::denied(runner_id, denial));
        }
        if runner_id != self.config.runner_id {
            return Ok(RunnerRegisterResponse::denied(
                runner_id,
                RunnerDenial::new(
                    RunnerDenialReason::AuthenticationFailed,
                    "runner identity does not match this runtime environment",
                ),
            ));
        }
        Ok(RunnerRegisterResponse::accepted(
            self.config.runner_id.clone(),
            30.0,
        ))
    }

    async fn heartbeat(&self, heartbeat: RunnerHeartbeat) -> Result<(), RuntimeError> {
        if heartbeat.runner_id != self.config.runner_id {
            return Err(RuntimeError::runner_protocol(
                "heartbeat runner id does not match this runtime environment",
            ));
        }
        if heartbeat.advertisement.binding.executor.executor_id != self.config.runner_id {
            return Err(RuntimeError::runner_protocol(
                "heartbeat advertisement targets a different executor",
            ));
        }
        Ok(())
    }

    async fn prepare_session(
        &self,
        request: RunnerPrepareSessionRequest,
    ) -> Result<RunnerPrepareSessionResponse, RuntimeError> {
        Ok(
            match RuntimeEnvironment::prepare_session(self, request.spec).await {
                Ok(handle) => RunnerPrepareSessionResponse::Prepared {
                    handle: Box::new(handle),
                },
                Err(error) => RunnerPrepareSessionResponse::Rejected { error },
            },
        )
    }

    async fn execute_tool(
        &self,
        request: RunnerExecuteToolRequest,
    ) -> Result<RunnerExecuteToolResponse, RuntimeError> {
        Ok(
            match RuntimeEnvironment::execute_tool(self, &request.session, request.invocation).await
            {
                Ok(outcome) => RunnerExecuteToolResponse::Completed { outcome },
                Err(error) => RunnerExecuteToolResponse::Rejected { error },
            },
        )
    }

    async fn destroy_session(
        &self,
        request: RunnerDestroySessionRequest,
    ) -> Result<(), RuntimeError> {
        self.remove_session(&request.session_id).await;
        Ok(())
    }
}

#[derive(Clone)]
pub struct RunnerRpcState {
    env: Arc<LocalRunnerEnvironment>,
}

impl RunnerRpcState {
    pub fn new(env: Arc<LocalRunnerEnvironment>) -> Self {
        Self { env }
    }
}

pub fn runner_rpc_router(env: Arc<LocalRunnerEnvironment>) -> Router {
    Router::new()
        .route("/v1/runner/advertisement", get(get_advertisement))
        .route("/v1/runner/register-request", get(get_register_request))
        .route("/v1/runner/register", post(post_register))
        .route("/v1/runner/heartbeat", get(get_heartbeat))
        .route("/v1/runner/heartbeat", post(post_heartbeat))
        .route("/v1/sessions/prepare", post(post_prepare_session))
        .route("/v1/tools/execute", post(post_execute_tool))
        .route("/v1/sessions/destroy", post(post_destroy_session))
        .with_state(RunnerRpcState::new(env))
}

async fn get_advertisement(
    State(state): State<RunnerRpcState>,
) -> Json<RuntimeEnvironmentAdvertisement> {
    Json(state.env.advertisement())
}

async fn get_register_request(State(state): State<RunnerRpcState>) -> Json<RunnerRegisterRequest> {
    Json(state.env.register_request())
}

async fn post_register(
    State(state): State<RunnerRpcState>,
    Json(request): Json<RunnerRegisterRequest>,
) -> Json<RunnerRegisterResponse> {
    let response = RunnerProtocol::register(state.env.as_ref(), request)
        .await
        .unwrap_or_else(|error| {
            RunnerRegisterResponse::denied(
                state.env.config.runner_id.clone(),
                RunnerDenial::new(RunnerDenialReason::CapabilityTooWeak, error.to_string()),
            )
        });
    Json(response)
}

async fn get_heartbeat(State(state): State<RunnerRpcState>) -> Json<RunnerHeartbeat> {
    Json(state.env.heartbeat_snapshot().await)
}

async fn post_heartbeat(
    State(state): State<RunnerRpcState>,
    Json(heartbeat): Json<RunnerHeartbeat>,
) -> Json<RunnerAckResponse> {
    match RunnerProtocol::heartbeat(state.env.as_ref(), heartbeat).await {
        Ok(()) => Json(RunnerAckResponse::Accepted),
        Err(error) => Json(RunnerAckResponse::Rejected { error }),
    }
}

async fn post_prepare_session(
    State(state): State<RunnerRpcState>,
    Json(request): Json<RunnerPrepareSessionRequest>,
) -> Json<RunnerPrepareSessionResponse> {
    let response = RunnerProtocol::prepare_session(state.env.as_ref(), request)
        .await
        .unwrap_or_else(|error| RunnerPrepareSessionResponse::Rejected { error });
    Json(response)
}

async fn post_execute_tool(
    State(state): State<RunnerRpcState>,
    Json(request): Json<RunnerExecuteToolRequest>,
) -> Json<RunnerExecuteToolResponse> {
    let response = RunnerProtocol::execute_tool(state.env.as_ref(), request)
        .await
        .unwrap_or_else(|error| RunnerExecuteToolResponse::Rejected { error });
    Json(response)
}

async fn post_destroy_session(
    State(state): State<RunnerRpcState>,
    Json(request): Json<RunnerDestroySessionRequest>,
) -> Json<RunnerDestroySessionResponse> {
    let session_id = request.session_id.clone();
    match RunnerProtocol::destroy_session(state.env.as_ref(), request).await {
        Ok(()) => Json(RunnerDestroySessionResponse::Destroyed { session_id }),
        Err(error) => Json(RunnerDestroySessionResponse::Rejected { error }),
    }
}

pub fn unsupported_runtime_topology_error(
    session_manager: RuntimeSessionManager,
    isolation_backend: RuntimeIsolationBackend,
) -> RuntimeError {
    RuntimeError::policy_unenforceable(format!(
        "astra-runner local runtime does not implement runtime topology {session_manager:?}/{isolation_backend:?}"
    ))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use astra_runtime_env::{
        RunnerExecuteToolRequest, RunnerExecuteToolResponse, RunnerPrepareSessionRequest,
        RunnerPrepareSessionResponse, RuntimeEnvironmentAdvertisement, RuntimeToolInvocation,
        ToolUnavailableReason, TOOL_RESULT_RUNTIME_ENVIRONMENT_ADVERTISEMENT,
        TOOL_RESULT_RUNTIME_POLICY_EVIDENCE,
    };
    use axum::{
        body::{to_bytes, Body},
        http::{header, Request},
    };
    use serde_json::json;
    use tower::ServiceExt;

    use super::*;

    #[test]
    fn local_runner_advertisement_exposes_runtime_bound_project_tools() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = LocalRunnerConfig::new("runner-1", dir.path());
        let advert = local_runner_advertisement(&config);

        assert_eq!(
            advert.schema_version,
            RuntimeEnvironmentAdvertisement::SCHEMA_VERSION
        );
        assert_eq!(
            advert.binding.workspace.authority,
            WorkspaceAuthority::ReadWrite
        );
        assert_eq!(
            advert.binding.executor.kind,
            astra_runtime_env::ExecutorBindingKind::PersonalRunner
        );
        assert_eq!(advert.binding.executor.executor_id, "runner-1");
        assert_eq!(
            advert.binding.runtime.session_manager,
            RuntimeSessionManager::HostProcess
        );
        assert_eq!(
            advert.binding.runtime.isolation_backend,
            RuntimeIsolationBackend::HostProcess
        );
        assert!(advert.binding.tool_surface.contains("bash"));
        assert!(advert.binding.tool_surface.contains("read_file"));
    }

    #[test]
    fn local_runner_register_request_validates_same_advertisement_contract() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = LocalRunnerConfig::new("runner-1", dir.path()).with_owner_id("user-1");
        let request = local_runner_register_request(&config);

        request.validate().expect("local runner registration");
    }

    #[tokio::test]
    async fn local_runner_execute_read_file_attaches_runtime_evidence() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("note.txt"), "hello runner\n").expect("write fixture");
        let env = LocalRunnerEnvironment::new(LocalRunnerConfig::new("runner-1", dir.path()));
        let binding = env.binding();
        let session = RuntimeEnvironment::prepare_session(
            &env,
            RuntimeSessionSpec::new("session-1", "run-1", binding.clone())
                .with_requested_tools(["read_file"]),
        )
        .await
        .expect("prepare session");
        let invocation = RuntimeToolInvocation::new(
            "call-1",
            "read_file",
            json!({"path": "note.txt"}),
            binding,
            session.policy.revision,
        );

        let outcome = RuntimeEnvironment::execute_tool(&env, &session, invocation)
            .await
            .expect("execute read_file");

        assert!(!outcome.is_error, "{}", outcome.output);
        assert!(outcome.output.contains("hello runner"));
        assert_eq!(
            outcome.metadata[TOOL_RESULT_RUNTIME_ENVIRONMENT_ADVERTISEMENT]["binding"]["executor"]
                ["executor_id"],
            "runner-1"
        );
        assert_eq!(
            outcome.metadata[TOOL_RESULT_RUNTIME_ENVIRONMENT_ADVERTISEMENT]["binding"]["runtime"]
                ["session_manager"],
            "host_process"
        );
        assert_eq!(
            outcome.metadata[TOOL_RESULT_RUNTIME_ENVIRONMENT_ADVERTISEMENT]["binding"]["runtime"]
                ["launch_driver"],
            "in_process"
        );
        assert_eq!(
            outcome.policy_evidence.launch_driver,
            astra_runtime_env::RuntimeLaunchDriver::InProcess
        );
        assert_eq!(
            outcome.metadata[TOOL_RESULT_RUNTIME_POLICY_EVIDENCE]["enforcement_status"],
            "enforced"
        );
        assert_eq!(
            outcome.metadata[TOOL_RESULT_RUNTIME_POLICY_EVIDENCE]["side_effects_maybe"],
            false
        );
    }

    #[tokio::test]
    async fn local_runner_read_only_binding_blocks_write_before_execution() {
        let dir = tempfile::tempdir().expect("tempdir");
        let env = LocalRunnerEnvironment::new(
            LocalRunnerConfig::new("runner-1", dir.path())
                .with_authority(WorkspaceAuthority::ReadOnly),
        );
        let binding = env.binding();
        let session = RuntimeEnvironment::prepare_session(
            &env,
            RuntimeSessionSpec::new("session-1", "run-1", binding.clone()),
        )
        .await
        .expect("prepare session");
        let invocation = RuntimeToolInvocation::new(
            "call-1",
            "write_file",
            json!({"path": "note.txt", "content": "nope"}),
            binding,
            session.policy.revision,
        );

        let err = RuntimeEnvironment::execute_tool(&env, &session, invocation)
            .await
            .expect_err("read-only runner must deny writes");

        assert_eq!(err.kind, RuntimeErrorKind::ToolUnavailable);
        assert!(!err.execution_started);
        assert!(matches!(
            err.tool_reason,
            Some(ToolUnavailableReason::WorkspaceUnavailable(_))
                | Some(ToolUnavailableReason::RuntimeCapabilityMissing(_))
        ));
        assert!(!dir.path().join("note.txt").exists());
    }

    #[tokio::test]
    async fn local_runner_rejects_session_for_different_runtime() {
        let dir = tempfile::tempdir().expect("tempdir");
        let env = LocalRunnerEnvironment::new(LocalRunnerConfig::new("runner-1", dir.path()));
        let mut binding = env.binding();
        binding.runtime = RuntimeBinding::gvisor("gvisor-1");

        let err = RuntimeEnvironment::prepare_session(
            &env,
            RuntimeSessionSpec::new("session-1", "run-1", binding),
        )
        .await
        .expect_err("runner must reject sessions for another runtime");

        assert_eq!(err.kind, RuntimeErrorKind::RunnerProtocolError);
        assert!(!err.execution_started);
    }

    async fn json_request(app: Router, uri: &str, payload: serde_json::Value) -> serde_json::Value {
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(payload.to_string()))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert!(
            response.status().is_success(),
            "status={}",
            response.status()
        );
        let bytes = to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("body");
        serde_json::from_slice(&bytes).expect("json response")
    }

    async fn get_json(app: Router, uri: &str) -> serde_json::Value {
        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(uri)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert!(
            response.status().is_success(),
            "status={}",
            response.status()
        );
        let bytes = to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("body");
        serde_json::from_slice(&bytes).expect("json response")
    }

    #[tokio::test]
    async fn runner_rpc_advertisement_endpoint_uses_personal_runner_binding() {
        let dir = tempfile::tempdir().expect("tempdir");
        let env = Arc::new(LocalRunnerEnvironment::new(LocalRunnerConfig::new(
            "runner-1",
            dir.path(),
        )));
        let app = runner_rpc_router(env);

        let value = get_json(app, "/v1/runner/advertisement").await;

        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["binding"]["executor"]["kind"], "personal_runner");
        assert_eq!(
            value["binding"]["capabilities"]["runtime"]["runtime_has_shell"],
            true
        );
    }

    #[tokio::test]
    async fn runner_rpc_prepare_and_execute_read_file_preserves_runtime_evidence() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("note.txt"), "hello rpc\n").expect("write fixture");
        let env = Arc::new(LocalRunnerEnvironment::new(LocalRunnerConfig::new(
            "runner-1",
            dir.path(),
        )));
        let binding = env.binding();
        let app = runner_rpc_router(env);
        let prepare_request = RunnerPrepareSessionRequest {
            request_id: "prepare-1".to_string(),
            spec: RuntimeSessionSpec::new("session-1", "run-1", binding.clone())
                .with_requested_tools(["read_file"]),
        };

        let prepared = json_request(
            app.clone(),
            "/v1/sessions/prepare",
            serde_json::to_value(prepare_request).expect("prepare request"),
        )
        .await;
        let prepared: RunnerPrepareSessionResponse =
            serde_json::from_value(prepared).expect("prepare response");
        let session = match prepared {
            RunnerPrepareSessionResponse::Prepared { handle } => handle,
            RunnerPrepareSessionResponse::Rejected { error } => {
                panic!("prepare rejected: {error:?}")
            }
        };
        let execute_request = RunnerExecuteToolRequest {
            request_id: "execute-1".to_string(),
            session: (*session).clone(),
            invocation: RuntimeToolInvocation::new(
                "call-1",
                "read_file",
                json!({"path": "note.txt"}),
                binding,
                session.policy.revision,
            ),
            idempotency_key: "idem-1".to_string(),
        };

        let executed = json_request(
            app,
            "/v1/tools/execute",
            serde_json::to_value(execute_request).expect("execute request"),
        )
        .await;
        let executed: RunnerExecuteToolResponse =
            serde_json::from_value(executed).expect("execute response");

        let outcome = match executed {
            RunnerExecuteToolResponse::Completed { outcome } => outcome,
            RunnerExecuteToolResponse::Rejected { error } => {
                panic!("execute rejected: {error:?}")
            }
        };
        assert!(outcome.output.contains("hello rpc"));
        assert_eq!(
            outcome.metadata[TOOL_RESULT_RUNTIME_ENVIRONMENT_ADVERTISEMENT]["binding"]["executor"]
                ["kind"],
            "personal_runner"
        );
    }

    #[tokio::test]
    async fn runner_rpc_rejects_forged_session_handle_before_execution() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("note.txt"), "hello rpc\n").expect("write fixture");
        let env = Arc::new(LocalRunnerEnvironment::new(LocalRunnerConfig::new(
            "runner-1",
            dir.path(),
        )));
        let binding = env.binding();
        let app = runner_rpc_router(env);
        let session = match serde_json::from_value::<RunnerPrepareSessionResponse>(
            json_request(
                app.clone(),
                "/v1/sessions/prepare",
                serde_json::to_value(RunnerPrepareSessionRequest {
                    request_id: "prepare-1".to_string(),
                    spec: RuntimeSessionSpec::new("session-1", "run-1", binding.clone()),
                })
                .expect("prepare request"),
            )
            .await,
        )
        .expect("prepare response")
        {
            RunnerPrepareSessionResponse::Prepared { handle } => handle,
            RunnerPrepareSessionResponse::Rejected { error } => {
                panic!("prepare rejected: {error:?}")
            }
        };
        let mut forged_session = (*session).clone();
        forged_session.executor_id = "other-runner".to_string();
        let request = RunnerExecuteToolRequest {
            request_id: "execute-1".to_string(),
            session: forged_session,
            invocation: RuntimeToolInvocation::new(
                "call-1",
                "read_file",
                json!({"path": "note.txt"}),
                binding,
                session.policy.revision,
            ),
            idempotency_key: "idem-1".to_string(),
        };

        let rejected = json_request(
            app,
            "/v1/tools/execute",
            serde_json::to_value(request).expect("execute request"),
        )
        .await;
        let rejected: RunnerExecuteToolResponse =
            serde_json::from_value(rejected).expect("execute response");

        let error = match rejected {
            RunnerExecuteToolResponse::Rejected { error } => error,
            RunnerExecuteToolResponse::Completed { outcome } => {
                panic!("forged session executed: {outcome:?}")
            }
        };
        assert_eq!(error.kind, RuntimeErrorKind::RunnerProtocolError);
        assert!(!error.execution_started);
    }

    #[tokio::test]
    async fn runner_rpc_capacity_exhaustion_is_reported_as_rejected_prepare() {
        let dir = tempfile::tempdir().expect("tempdir");
        let env = Arc::new(LocalRunnerEnvironment::new(
            LocalRunnerConfig::new("runner-1", dir.path()).with_max_sessions(1),
        ));
        let binding = env.binding();
        let app = runner_rpc_router(env);
        let first = RunnerPrepareSessionRequest {
            request_id: "prepare-1".to_string(),
            spec: RuntimeSessionSpec::new("session-1", "run-1", binding.clone()),
        };
        let second = RunnerPrepareSessionRequest {
            request_id: "prepare-2".to_string(),
            spec: RuntimeSessionSpec::new("session-2", "run-2", binding),
        };
        let _ = json_request(
            app.clone(),
            "/v1/sessions/prepare",
            serde_json::to_value(first).expect("first request"),
        )
        .await;

        let rejected = json_request(
            app,
            "/v1/sessions/prepare",
            serde_json::to_value(second).expect("second request"),
        )
        .await;
        let rejected: RunnerPrepareSessionResponse =
            serde_json::from_value(rejected).expect("prepare response");

        let error = match rejected {
            RunnerPrepareSessionResponse::Rejected { error } => error,
            RunnerPrepareSessionResponse::Prepared { handle } => {
                panic!("capacity exhausted but prepared: {handle:?}")
            }
        };
        assert_eq!(error.kind, RuntimeErrorKind::RuntimeCapacityExhausted);
        assert!(error.retryable);
        assert!(!error.execution_started);
    }
}
