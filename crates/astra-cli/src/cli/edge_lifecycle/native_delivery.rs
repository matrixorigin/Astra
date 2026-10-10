//! CLI hosting of the shared Edge delivery owner. Bootstrap permission is
//! resolved on actual invocation against a frozen declaration, never at
//! installation or inferred from discovery.

use super::provider_interaction::NativeInvocationInteractionGate;
use crate::cli::{
    chat_stream,
    permission_manager::{GateOutcome, PermissionPolicySubscription},
};
use crate::edge_tools::{ApprovedNativeRuntime, ToolExecutor, native_codex};
use astra_edge::{EdgeConnectionContext, EdgeInvocation, EdgeInvocationExecutor};
use astra_server_types::edge_ws_protocol::EdgeClientMessage;
use astra_tools::{
    ToolResult,
    tool_engine::{ToolInvocationAdmissionSource, ToolInvocationMetadata},
};
use astra_turn_core::provider_resolution::NativeCollaboratorProtocol;
use astra_turn_types::{
    ProviderBindingRef, ProviderDiscoverySnapshot, ProviderIdentity, ProviderProtocolId,
};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const NATIVE_DELIVERY_STARTUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const NATIVE_DELIVERY_STARTUP_WAIT: std::time::Duration = std::time::Duration::from_secs(6);

#[derive(Debug)]
enum NativeDeliveryError {
    Authentication(String),
    Other(String),
}

/// Resolve credentials from the already captured CLI owner at dispatch time.
/// This covers both native login rotation and legacy profile refresh without
/// letting a reconnect reselect the process's current account.
#[derive(Clone, Debug)]
struct SessionOwnerBearerProvider {
    api: astra_thin_client::ThinClient,
    owner: crate::cli::cli_config::cli_utils::CliOwnerAuthSnapshot,
}

impl astra_thin_client::client::BearerProvider for SessionOwnerBearerProvider {
    fn token(
        &self,
    ) -> futures_util::future::BoxFuture<'_, Result<String, astra_thin_client::ThinClientError>>
    {
        Box::pin(async {
            crate::cli::session::session_runtime::owner_access_token(&self.api, &self.owner, None)
                .await
                .ok_or_else(|| {
                    astra_thin_client::ThinClientError::InvalidInput(
                        "CLI owner authentication is unavailable".into(),
                    )
                })
        })
    }
}

/// Stable UI-facing request path for the session-owned delivery supervisor.
/// The sender behind this handle changes when a transport owner is replaced;
/// a TUI action must therefore retain the indirection, not a sender tied to a
/// previous owner.
#[derive(Clone, Default)]
pub(crate) struct NativeDeliveryRefreshHandle {
    sender: Arc<std::sync::Mutex<Option<mpsc::Sender<()>>>>,
}

impl NativeDeliveryRefreshHandle {
    pub(crate) fn request(&self) {
        let sender = self.sender.lock().ok().and_then(|sender| sender.clone());
        if let Some(sender) = sender {
            let _ = sender.try_send(());
        }
    }

    fn bind(&self, sender: mpsc::Sender<()>) {
        if let Ok(mut current) = self.sender.lock() {
            *current = Some(sender);
        }
    }

    pub(crate) fn clear(&self) {
        if let Ok(mut current) = self.sender.lock() {
            *current = None;
        }
    }
}

impl NativeDeliveryError {
    fn other(message: impl Into<String>) -> Self {
        Self::Other(message.into())
    }

    fn is_authentication(&self) -> bool {
        matches!(self, Self::Authentication(_))
    }
}

impl std::fmt::Display for NativeDeliveryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Authentication(message) | Self::Other(message) => formatter.write_str(message),
        }
    }
}

async fn delivery_auth_token(
    config: &NativeDeliveryConfig,
    admission_deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<String, NativeDeliveryError> {
    let token = async {
        if let Some(provider) = &config.auth_provider {
            provider.token().await.map_err(|_| {
                NativeDeliveryError::other("native delivery authentication unavailable")
            })
        } else {
            Ok(config.auth.read().await.clone())
        }
    };
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => {
            Err(NativeDeliveryError::other("native delivery authentication cancelled"))
        }
        result = tokio::time::timeout_at(
            tokio::time::Instant::from_std(admission_deadline),
            token,
        ) => match result {
            Ok(result) => result,
            Err(_) => Err(NativeDeliveryError::other(
                "native delivery authentication deadline expired",
            )),
        }
    }
}

fn native_delivery_authentication_error(
    error: astra_edge::EdgeAuthenticationError,
) -> NativeDeliveryError {
    use astra_edge::EdgeAuthenticationError;
    match error {
        EdgeAuthenticationError::Rejected
        | EdgeAuthenticationError::InvalidAccount
        | EdgeAuthenticationError::AccountMismatch => {
            NativeDeliveryError::Authentication("native delivery credentials were rejected".into())
        }
        EdgeAuthenticationError::IncompatibleContract
        | EdgeAuthenticationError::Protocol
        | EdgeAuthenticationError::Envelope(_) => {
            NativeDeliveryError::other("native delivery authentication protocol is incompatible")
        }
        EdgeAuthenticationError::ClosedBeforeAuthentication
        | EdgeAuthenticationError::Timeout
        | EdgeAuthenticationError::Cancelled
        | EdgeAuthenticationError::Transport(_) => {
            NativeDeliveryError::other("native delivery authentication transport failed")
        }
    }
}

fn native_delivery_connection_error(
    error: tokio_tungstenite::tungstenite::Error,
) -> NativeDeliveryError {
    if let tokio_tungstenite::tungstenite::Error::Http(response) = &error
        && matches!(response.status().as_u16(), 401 | 403)
    {
        return NativeDeliveryError::Authentication(
            "native delivery server rejected credentials".into(),
        );
    }
    NativeDeliveryError::other("native delivery connection failed")
}

/// Held by the existing CLI lifecycle, never by a root-turn future. Dropping
/// the handle requests cancellation; the task still owns settlement/join.
type SharedEdgeInvocationOwner = Arc<tokio::sync::Mutex<astra_edge::EdgeInvocationOwner>>;

pub(crate) struct NativeDeliveryHandle {
    cancellation: CancellationToken,
    withdrawal: CancellationToken,
    auth: Arc<tokio::sync::RwLock<String>>,
    account_id: String,
    auth_owner: Option<crate::cli::cli_config::cli_utils::CliOwnerAuthSnapshot>,
    refresh_tx: mpsc::Sender<()>,
    discovered_executables: Arc<std::sync::Mutex<Vec<native_codex::NativeExecutableIdentity>>>,
    invocation_owner: Option<SharedEdgeInvocationOwner>,
    task: Option<tokio::task::JoinHandle<()>>,
    /// Set after the first discovery/handshake/publication attempt completes.
    /// `true` means the optional capability is settled for this turn; it does
    /// not claim that a provider was available.
    ready: tokio::sync::watch::Receiver<bool>,
    /// Session-scoped availability, shared by the supervisor and its current
    /// transport attachment. A failed probe is recoverable on a later turn.
    provider_available: Arc<AtomicBool>,
}

impl Drop for NativeDeliveryHandle {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

impl NativeDeliveryHandle {
    pub(crate) fn cancel(&self) {
        self.cancellation.cancel();
    }

    fn withdraw(&self) {
        self.withdrawal.cancel();
    }

    fn request_refresh_if_environment_changed(&self) {
        // Capacity one coalesces repeated environment changes. The existing
        // supervisor remains the single owner of discovery and reconnection.
        let current = native_codex::native_provider_executable_snapshot();
        let changed = self
            .discovered_executables
            .lock()
            .ok()
            .is_some_and(|selected| *selected != current);
        if changed {
            let _ = self.refresh_tx.try_send(());
        }
    }

    fn executable_changed(&self) -> bool {
        let current = native_codex::native_provider_executable_snapshot();
        self.discovered_executables
            .lock()
            .ok()
            .is_none_or(|selected| *selected != current)
    }

    fn request_refresh(&self) {
        // The channel is bounded to one item, so repeated turns cannot create
        // an unbounded discovery queue or a second supervisor.
        let _ = self.refresh_tx.try_send(());
    }

    async fn update_auth(&self, token: &str) {
        *self.auth.write().await = token.to_owned();
    }

    fn credential_generation_changed(&self) -> bool {
        self.auth_owner
            .as_ref()
            .is_some_and(|owner| !owner.is_current())
    }

    fn account_id(&self) -> &str {
        &self.account_id
    }

    fn provider_available(&self) -> bool {
        self.provider_available.load(Ordering::Acquire)
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.task
            .as_ref()
            .is_none_or(tokio::task::JoinHandle::is_finished)
    }

    pub(crate) async fn shutdown(mut self) {
        self.cancel();
        self.wait().await;
        if let Some(invocation_owner) = &self.invocation_owner {
            settle_native_invocation_owner(invocation_owner).await;
        }
    }

    async fn wait(&mut self) {
        if let Some(task) = self.task.as_mut() {
            let _ = task.await;
        }
        self.task.take();
    }
}

async fn settle_native_invocation_owner(owner: &SharedEdgeInvocationOwner) {
    let mut owner = owner.lock().await;
    let shutdown = CancellationToken::new();
    shutdown.cancel();
    if let Err(error) = owner.settle(&shutdown).await {
        tracing::error!(
            component = "native_delivery",
            operation = "settle_invocation_owner",
            error = %error,
            "Native collaborator invocation settlement failed"
        );
    }
}

async fn capability_withdrawal_requested(
    external: &CancellationToken,
    internal: &CancellationToken,
) {
    tokio::select! {
        _ = external.cancelled() => {}
        _ = internal.cancelled() => {}
    }
}

async fn wait_for_native_delivery_ready(handle: &NativeDeliveryHandle) {
    if *handle.ready.borrow() {
        return;
    }
    let mut ready = handle.ready.clone();
    let _ = tokio::time::timeout(NATIVE_DELIVERY_STARTUP_WAIT, async move {
        loop {
            if *ready.borrow() {
                break;
            }
            if ready.changed().await.is_err() {
                break;
            }
        }
    })
    .await;
}

/// All identities come from the selected authenticated CLI boundary. The
/// journal path is allocated by the existing local state owner, not the model.
#[derive(Clone)]
pub(crate) struct NativeDeliveryConfig {
    pub(crate) websocket_url: String,
    pub(crate) api: astra_thin_client::ThinClient,
    pub(crate) auth: Arc<tokio::sync::RwLock<String>>,
    /// The selected native login owns rotation and generation checks. Keep
    /// this provider alongside the session config so a background reconnect
    /// can obtain the current credential without waiting for another turn.
    pub(crate) auth_provider: Option<Arc<dyn astra_thin_client::client::BearerProvider>>,
    pub(crate) auth_owner: Option<crate::cli::cli_config::cli_utils::CliOwnerAuthSnapshot>,
    pub(crate) account_id: String,
    pub(crate) edge_agent_id: String,
    /// The server-issued connection identity is transport-scoped. Keep the
    /// current value behind the session owner so an invocation that survives
    /// a reconnect uses the replacement connection for later callbacks.
    pub(crate) edge_transport_id: Arc<tokio::sync::RwLock<String>>,
    pub(crate) workspace_id: Option<String>,
    pub(crate) materialization_id: String,
    pub(crate) journal_path: PathBuf,
    pub(crate) executor: Arc<ToolExecutor>,
    pub(crate) ask_user_request_tx: Option<chat_stream::AskUserRequestTx>,
    /// Read-only publication from the selected SessionState permission writer.
    pub(crate) permission_policy: PermissionPolicySubscription,
    pub(crate) approval_request_tx: Option<chat_stream::ApprovalRequestTx>,
}

fn bootstrap_args(snapshot: &ProviderDiscoverySnapshot, path: &str) -> Value {
    json!({"directory": path, "provider_snapshot_hash": snapshot.content_hash,
        "provider_binding": snapshot.binding_ref, "access": "native_runtime_read"})
}

fn discovery_snapshot(
    edge_agent_id: &str,
    materialization_id: &str,
    canonical_root: &str,
    declaration: astra_turn_types::ProviderToolDeclaration,
) -> Result<ProviderDiscoverySnapshot, String> {
    ProviderDiscoverySnapshot::new(
        ProviderIdentity::new(edge_agent_id).map_err(|_| "invalid provider identity")?,
        ProviderBindingRef::new(
            astra_services::SessionExecutionBindingV1::edge_materialization_physical_identity(
                materialization_id,
                canonical_root,
            ),
        )
        .map_err(|_| "invalid provider binding")?,
        ProviderProtocolId::new("cli-local").map_err(|_| "invalid provider protocol")?,
        vec![declaration],
    )
    .map_err(|_| "invalid native declaration".into())
}

/// Verify current attachment authority, not merely a retained policy Arc.
fn current_policy(
    policy: &PermissionPolicySubscription,
    session_id: &str,
    attachment_epoch: u64,
) -> Result<Arc<crate::cli::permission_manager::PermissionPolicySnapshot>, String> {
    let current = policy
        .current()
        .ok_or("native permission attachment is unbound")?;
    if current.session_id() != session_id || current.attachment_epoch() != attachment_epoch {
        return Err("native permission attachment changed".into());
    }
    Ok(current)
}

fn dependencies_need_approval(
    policy: &PermissionPolicySubscription,
    session_id: &str,
    attachment_epoch: u64,
    snapshot: &ProviderDiscoverySnapshot,
    requirements: &astra_turn_types::ProviderRuntimeRequirements,
) -> Result<bool, String> {
    let current = current_policy(policy, session_id, attachment_epoch)?;
    let declaration = snapshot
        .tool_declarations
        .first()
        .ok_or_else(|| "missing native collaborator declaration".to_string())?;
    let protocol = NativeCollaboratorProtocol::from_extension_fields(&declaration.extension_fields)
        .ok_or_else(|| "native collaborator protocol is not declared".to_string())?;
    let mut needed = false;
    for path in &requirements.read_paths {
        if astra_sandbox::is_never_readable_path(std::path::Path::new(path)) {
            return Err("native bootstrap contains a forbidden path".into());
        }
        match current
            .check_sandbox_expansion(protocol.permission_scope(), &bootstrap_args(snapshot, path))
        {
            GateOutcome::Allow => {}
            GateOutcome::Deny(_) => return Err("native bootstrap denied by local policy".into()),
            GateOutcome::NeedApproval { .. } => needed = true,
        }
    }
    Ok(needed)
}

/// One complete-set prompt, with no permission writes in the consumer.
impl CliNativeExecutor {
    async fn approve_bootstrap(
        &self,
        invocation: &EdgeInvocation,
        cancellation: &CancellationToken,
    ) -> Result<ApprovedNativeRuntime, String> {
        let executor = &self.config.executor;
        let snapshot = self.snapshot.clone();
        let permission_policy = &self.config.permission_policy;
        let expected_session_id = self.expected_session_id.as_str();
        let expected_attachment_epoch = self.expected_attachment_epoch;
        let approval_tx = self.config.approval_request_tx.as_ref();
        let deadline = invocation.execution_deadline;
        if cancellation.is_cancelled() || Instant::now() >= deadline {
            return Err("native bootstrap admission cancelled or expired".into());
        }
        if invocation.identity.session_id != expected_session_id {
            return Err("native invocation does not match permission attachment".into());
        }
        let binding_generation = invocation
            .execution_ceiling
            .as_ref()
            .ok_or("native bootstrap requires an execution ceiling")?
            .execution_binding_generation;
        if binding_generation == 0 {
            return Err("native bootstrap requires a binding generation".into());
        }
        let declaration = snapshot
            .tool_declarations
            .first()
            .ok_or("missing native declaration")?;
        let protocol =
            NativeCollaboratorProtocol::from_extension_fields(&declaration.extension_fields)
                .ok_or("native collaborator protocol is not declared")?;
        let requirements = astra_turn_types::ProviderRuntimeRequirements::from_extension_fields(
            &declaration.extension_fields,
        )
        .map_err(|_| "invalid native runtime requirements")?
        .ok_or("missing native runtime requirements")?;
        let root = executor
            .effective_project_root()
            .canonicalize()
            .map_err(|_| "native workspace is unavailable")?;
        let mut policy = permission_policy.clone();
        let source = if dependencies_need_approval(
            &policy,
            expected_session_id,
            expected_attachment_epoch,
            &snapshot,
            &requirements,
        )? {
            let tx = approval_tx.ok_or(
                "native collaborator needs permission approval; run it in the interactive TUI or use --auto-approve",
            )?;
            let (response_tx, mut response_rx) = tokio::sync::oneshot::channel();
            let args = json!({"provider_snapshot_hash": snapshot.content_hash,
            "provider_binding": snapshot.binding_ref, "executable": requirements.executable,
            "read_paths": requirements.read_paths, "access": "native_runtime_read"});
            let mut request = chat_stream::ApprovalRequest::bare(
            protocol.permission_scope().into(), format!("Allow native {} runtime reads?", protocol.display_name()),
            Some(requirements.read_paths.join("\n")),
            "These exact executable/platform paths become readable by the native collaborator; workspace, network and sensitive-path restrictions remain in force.".into(),
            args, response_tx,
        );
            request.metadata = Some(Box::new(crate::tui::approval::queue::ApprovalMetadata {
                runtime_dependencies: Some(
                    crate::tui::approval::queue::RuntimeDependencyApprovalContext {
                        invocation: invocation.identity.clone(),
                        attachment_epoch: expected_attachment_epoch,
                        execution_binding_generation: binding_generation,
                        deadline,
                        cancel: cancellation.clone(),
                    },
                ),
                ..Default::default()
            }));
            chat_stream::enqueue_interactive_request(tx, request).map_err(|_| {
                "native collaborator approval UI is unavailable; retry in the interactive TUI or use --auto-approve"
            })?;
            let cutoff = tokio::time::Instant::from_std(deadline);
            loop {
                if cancellation.is_cancelled() || tokio::time::Instant::now() >= cutoff {
                    return Err("native bootstrap admission cancelled or expired".into());
                }
                let needs_approval = dependencies_need_approval(
                    &policy,
                    expected_session_id,
                    expected_attachment_epoch,
                    &snapshot,
                    &requirements,
                )?;
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() =>
                        return Err("native bootstrap admission cancelled".into()),
                    _ = tokio::time::sleep_until(cutoff) =>
                        return Err("native bootstrap admission expired".into()),
                    response = &mut response_rx => {
                        // The latest whole-set decision always wins over a stale answer.
                        let needed = dependencies_need_approval(
                            &policy, expected_session_id, expected_attachment_epoch, &snapshot, &requirements,
                        )?;
                        match response.map_err(|_| "native bootstrap approval consumer closed")? {
                            chat_stream::ApprovalResponse::AllowOnce => break if needed {
                                ToolInvocationAdmissionSource::ParentApproval
                            } else {
                                ToolInvocationAdmissionSource::Policy
                            },
                            chat_stream::ApprovalResponse::AlwaysAllow =>
                                return Err("native dependency persistent approval is not supported".into()),
                            chat_stream::ApprovalResponse::Deny =>
                                return Err("native bootstrap approval denied".into()),
                        }
                    }
                    changed = policy.changed() => {
                        changed.map_err(|_| "native permission writer closed")?;
                    }
                    _ = std::future::ready(()), if !needs_approval =>
                        break ToolInvocationAdmissionSource::Policy,
                }
            }
        } else {
            ToolInvocationAdmissionSource::Policy
        };
        if cancellation.is_cancelled() || Instant::now() >= deadline {
            return Err("native bootstrap admission cancelled or expired".into());
        }
        let still_needs_approval = dependencies_need_approval(
            &policy,
            expected_session_id,
            expected_attachment_epoch,
            &snapshot,
            &requirements,
        )?;
        if still_needs_approval && source == ToolInvocationAdmissionSource::Policy {
            return Err("native bootstrap policy approval was revoked".into());
        }
        let source = if still_needs_approval {
            source
        } else {
            ToolInvocationAdmissionSource::Policy
        };
        Ok(ApprovedNativeRuntime {
            protocol,
            snapshot: (*snapshot).clone(),
            requirements,
            workspace_root: root,
            admission_source: source,
        })
    }
}

struct CliNativeExecutor {
    config: Arc<NativeDeliveryConfig>,
    snapshot: Arc<ProviderDiscoverySnapshot>,
    workspace_root: PathBuf,
    requirements: astra_turn_types::ProviderRuntimeRequirements,
    tool_name: String,
    protocol: NativeCollaboratorProtocol,
    expected_session_id: String,
    expected_attachment_epoch: u64,
    // Production installation always supplies this identity. The optional
    // test-only value keeps admission tests focused on permission facts
    // without manufacturing an executable artifact.
    expected_executable_identity: Option<native_codex::NativeExecutableIdentity>,
    invalidation: CancellationToken,
}

/// One verified provider binding. Multiple protocol adapters may be available
/// in the same CLI process; they share the Edge invocation owner and differ
/// only in this immutable binding plus their protocol translation.
struct NativeProviderCandidate {
    snapshot: Arc<ProviderDiscoverySnapshot>,
    executable_identity: native_codex::NativeExecutableIdentity,
}

struct NativeExecutorRouter {
    providers: Vec<Arc<CliNativeExecutor>>,
}

fn rejected(reason: &str) -> ToolResult {
    let mut metadata = serde_json::Map::new();
    astra_tools::execution_outcome::insert_not_executed_fact(&mut metadata);
    metadata.insert("workspace_effect_settled".into(), json!(true));
    ToolResult {
        output: reason.into(),
        is_error: true,
        metadata: Some(metadata),
        exit_semantics: None,
    }
}

struct UnavailableNativeExecutor;

impl EdgeInvocationExecutor for UnavailableNativeExecutor {
    fn execute(&self, _: EdgeInvocation, _: CancellationToken) -> BoxFuture<'_, ToolResult> {
        Box::pin(async {
            rejected("native provider capability is unavailable; no work was dispatched")
        })
    }
}

impl EdgeInvocationExecutor for NativeExecutorRouter {
    fn execute(
        &self,
        invocation: EdgeInvocation,
        cancel: CancellationToken,
    ) -> BoxFuture<'_, ToolResult> {
        if let Some(provider) = self
            .providers
            .iter()
            .find(|provider| provider.tool_name == invocation.tool)
        {
            return provider.execute(invocation, cancel);
        }
        Box::pin(async {
            rejected("native provider selection is unavailable; no work was dispatched")
        })
    }
}

impl EdgeInvocationExecutor for CliNativeExecutor {
    fn execute(
        &self,
        invocation: EdgeInvocation,
        cancel: CancellationToken,
    ) -> BoxFuture<'_, ToolResult> {
        Box::pin(async move {
            let config = &self.config;
            let Some(ceiling) = invocation.execution_ceiling.as_deref() else {
                return rejected("CLI native delivery requires a frozen execution grant");
            };
            if invocation.tool != self.tool_name
                || invocation.identity.user_id != config.account_id
                || ceiling.workspace_root != self.workspace_root.to_string_lossy()
                || ceiling.workspace_id != config.workspace_id
                || ceiling.materialization_id.as_deref() != Some(config.materialization_id.as_str())
                || ceiling.execution_binding_generation == 0
                || invocation.runtime_process_authorization.is_some()
                || ceiling.runtime_read_paths != self.requirements.read_paths
            {
                return rejected("CLI native delivery does not match the selected boundary");
            }
            // Discovery is capability only. Local approval is acquired on a
            // real, fenced invocation using its original work deadline. The
            // UI wait and native execution share that cutoff.
            if config
                .executor
                .effective_project_root()
                .canonicalize()
                .ok()
                .as_ref()
                != Some(&self.workspace_root)
            {
                return rejected("native bootstrap workspace no longer matches discovery");
            }
            // The tool executor owns workspace serialization through process
            // settlement. Delivery must not acquire the same non-reentrant
            // lease before calling it, or hold the workspace during approval.
            let approval = match self.approve_bootstrap(&invocation, &cancel).await {
                Ok(approval) => approval,
                Err(reason) => return rejected(&reason),
            };
            // A worktree change while the approval was pending must not turn
            // the frozen grant into authority over the newly selected root.
            if approval.workspace_root != self.workspace_root {
                return rejected("native bootstrap workspace changed during approval");
            }
            let gate = NativeInvocationInteractionGate {
                api: config.api.clone(),
                auth: config.auth.clone(),
                auth_provider: config.auth_provider.clone(),
                edge_transport_id: config.edge_transport_id.clone(),
                edge_agent_id: config.edge_agent_id.clone(),
                physical_workspace_id:
                    astra_services::SessionExecutionBindingV1::edge_materialization_physical_identity(
                        &config.materialization_id,
                        &self.workspace_root.to_string_lossy(),
                    ),
                identity: invocation.identity.clone(),
                provider: self.protocol,
                deadline: invocation.execution_deadline,
                ask_user_request_tx: config.ask_user_request_tx.clone(),
            };
            let metadata = ToolInvocationMetadata {
                admission_deadline: Some(invocation.execution_deadline),
                run_id: Some(&invocation.identity.run_id),
                turn_chain_id: Some(&invocation.identity.turn_chain_id),
                tool_call_id: Some(&invocation.identity.invocation_id),
                admission_source: Some(approval.admission_source),
                command_timeout_cap_ms: invocation.command_timeout_cap_ms,
                ..ToolInvocationMetadata::default()
            };
            let deadline = invocation.execution_deadline;
            let validate_dispatch = || -> Result<(), String> {
                if config
                    .executor
                    .effective_project_root()
                    .canonicalize()
                    .ok()
                    .as_ref()
                    != Some(&self.workspace_root)
                {
                    return Err("native bootstrap workspace changed before dispatch".into());
                }
                let needed = dependencies_need_approval(
                    &config.permission_policy,
                    &self.expected_session_id,
                    self.expected_attachment_epoch,
                    &self.snapshot,
                    &self.requirements,
                )?;
                if needed && approval.admission_source == ToolInvocationAdmissionSource::Policy {
                    return Err(
                        "native bootstrap policy approval was revoked before dispatch".into(),
                    );
                }
                if cancel.is_cancelled() || Instant::now() >= deadline {
                    return Err("native invocation cancelled or expired before dispatch".into());
                }
                // Revalidate immediately before dispatch, after any user approval
                // wait. A capability snapshot is valid only while the exact
                // installed executable it described remains usable. A changed or
                // removed client withdraws this owner; the session supervisor will
                // probe the environment again instead of repeatedly rejecting a
                // stale published snapshot.
                let current_requirements =
                    crate::edge_tools::native_codex::runtime_requirements_for_executable(
                        std::path::Path::new(&self.requirements.executable),
                    );
                let identity_matches =
                    self.expected_executable_identity
                        .as_ref()
                        .is_none_or(|expected| {
                            native_codex::native_executable_identity(std::path::Path::new(
                                &self.requirements.executable,
                            ))
                            .is_ok_and(|current| current == *expected)
                        });
                if current_requirements.as_ref().ok() != Some(&self.requirements)
                    || !identity_matches
                {
                    self.invalidation.cancel();
                    return Err(
                        "native provider capability changed; rediscovery is required".into(),
                    );
                }
                Ok(())
            };
            let native_input_rx = invocation.input_rx;
            let outcome = config
                .executor
                .execute_native_provider_invocation(
                    self.protocol,
                    &self.tool_name,
                    &invocation.args,
                    metadata,
                    Some(&cancel),
                    &gate,
                    ceiling,
                    Some(&approval),
                    native_input_rx,
                    &|| validate_dispatch().map_err(|reason| rejected(&reason)),
                )
                .await;
            if outcome
                .tool_result_fields
                .as_ref()
                .and_then(|fields| fields.get("native_capability_unavailable"))
                .and_then(Value::as_bool)
                == Some(true)
            {
                // The result is returned through the normal Edge completion
                // path first. The shared owner observes this token only as a
                // drain signal, so already-admitted sibling invocations are
                // not cancelled or lost.
                self.invalidation.cancel();
            }
            ToolResult {
                output: outcome.output,
                is_error: outcome.is_error,
                metadata: outcome.tool_result_fields,
                exit_semantics: None,
            }
        })
    }
}

fn capabilities(
    config: &NativeDeliveryConfig,
    snapshots: &[Arc<ProviderDiscoverySnapshot>],
) -> Value {
    let mut value = astra_thin_client::edge_runtime_environment_capabilities(
        &config.edge_agent_id,
        config.executor.effective_project_root().to_string_lossy(),
    );
    // This socket is a provider-stage executor, not the ordinary CLI tool
    // boundary. Advertising the builtin surface would let server admission
    // route `bash`/file tools here even though this executor intentionally
    // accepts only the native provider invocation.
    if let Some(surface) = value
        .get_mut("binding")
        .and_then(Value::as_object_mut)
        .and_then(|binding| binding.get_mut("tool_surface"))
        .and_then(Value::as_object_mut)
    {
        surface.insert("tool_names".into(), json!([]));
        surface.insert("admissions".into(), json!([]));
        surface.insert("denials".into(), json!([]));
    }
    value["provider_discovery"] = json!(snapshots);
    value
}

async fn publish(
    config: &NativeDeliveryConfig,
    snapshots: &[Arc<ProviderDiscoverySnapshot>],
) -> Result<(), String> {
    let mut body = astra_thin_client::EdgeRegisterRequest::new(&config.edge_agent_id);
    body.worktree_path = Some(
        config
            .executor
            .effective_project_root()
            .to_string_lossy()
            .into_owned(),
    );
    body.materialization_id = Some(config.materialization_id.clone());
    body.capabilities = Some(capabilities(config, snapshots));
    let edge_transport_id = config.edge_transport_id.read().await.clone();
    let auth = if let Some(provider) = &config.auth_provider {
        provider
            .token()
            .await
            .map_err(|_| "native capacity authentication is unavailable".to_string())?
    } else {
        config.auth.read().await.clone()
    };
    config
        .api
        .post_agents_edge_register(Some(&auth), Some(&edge_transport_id), &body)
        .await
        .map_err(|_| "native capacity registration failed".to_string())?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn build_native_delivery_config(
    api: &astra_thin_client::ThinClient,
    token: &str,
    account_id: String,
    edge_agent_id: String,
    session_id: &str,
    materialization_id: String,
    executor: Arc<ToolExecutor>,
    permission_policy: PermissionPolicySubscription,
    ask_user_request_tx: Option<chat_stream::AskUserRequestTx>,
    approval_request_tx: Option<chat_stream::ApprovalRequestTx>,
) -> Result<NativeDeliveryConfig, String> {
    let owner = crate::cli::cli_config::cli_utils::cli_owner_auth_snapshot();
    let auth_owner = (!crate::cli::session::session_runtime::uses_environment_access_token(token)
        && owner.is_current()
        && owner.profile_name.is_some()
        && owner.server_account_id.is_some())
    .then_some(owner.clone());
    Ok(NativeDeliveryConfig {
        websocket_url: astra_edge::edge_ws_url(&api.api_origin())
            .map_err(|error| format!("invalid WebSocket endpoint: {error}"))?,
        api: api.clone().without_bearer_provider(),
        auth: Arc::new(tokio::sync::RwLock::new(token.to_owned())),
        auth_provider: auth_owner.clone().map(|owner| {
            Arc::new(SessionOwnerBearerProvider {
                api: api.clone().without_bearer_provider(),
                owner,
            }) as Arc<dyn astra_thin_client::client::BearerProvider>
        }),
        auth_owner,
        account_id,
        edge_transport_id: Arc::new(tokio::sync::RwLock::new(edge_agent_id.clone())),
        edge_agent_id,
        workspace_id: None,
        materialization_id,
        journal_path: native_journal_path(session_id),
        executor,
        ask_user_request_tx,
        permission_policy,
        approval_request_tx,
    })
}

/// Called by the process-scoped CLI lifecycle after constructing the real
/// executor and UI channels. No standalone model-tool projection is installed.
/// Installation neither evaluates local permission nor asks for bootstrap
/// approval: discovery becomes a grant only inside a fenced invocation.
#[allow(clippy::too_many_arguments)]
async fn install_native_delivery(
    config: NativeDeliveryConfig,
    admission_deadline: Instant,
    cancellation: &CancellationToken,
    withdrawal: CancellationToken,
    refresh_tx: mpsc::Sender<()>,
    discovered_executables: Arc<std::sync::Mutex<Vec<native_codex::NativeExecutableIdentity>>>,
    invocation_owner: Option<SharedEdgeInvocationOwner>,
    provider_available: Arc<AtomicBool>,
) -> Result<
    (
        NativeDeliveryHandle,
        bool,
        Option<Vec<native_codex::NativeExecutableIdentity>>,
        SharedEdgeInvocationOwner,
    ),
    NativeDeliveryError,
> {
    if config.account_id.is_empty()
        || config.edge_agent_id.is_empty()
        || config
            .edge_transport_id
            .try_read()
            .map(|id| id.is_empty())
            .unwrap_or(true)
    {
        return Err(NativeDeliveryError::other(
            "native delivery requires authenticated CLI identities",
        ));
    }
    let root = config
        .executor
        .effective_project_root()
        .canonicalize()
        .map_err(|_| NativeDeliveryError::other("native workspace is unavailable"))?;
    let root_text = root
        .to_str()
        .ok_or_else(|| NativeDeliveryError::other("native workspace is not UTF-8"))?;
    let owner_needs_recovery = if let Some(owner) = invocation_owner.as_ref() {
        owner.lock().await.has_unsettled_work()
    } else {
        false
    };
    let (providers, verified_executables) = match config
        .executor
        .native_collaborator_declarations_if_available(Some(cancellation), admission_deadline)
        .await
    {
        Some((declarations, verified_executables)) => {
            let providers = declarations
                .into_iter()
                .map(|(declaration, executable_identity)| {
                    discovery_snapshot(
                        &config.edge_agent_id,
                        &config.materialization_id,
                        root_text,
                        declaration,
                    )
                    .map(|snapshot| NativeProviderCandidate {
                        snapshot: Arc::new(snapshot),
                        executable_identity,
                    })
                    .map_err(NativeDeliveryError::other)
                })
                .collect::<Result<Vec<_>, _>>()?;
            (Some(providers), Some(verified_executables))
        }
        None if owner_needs_recovery
            || astra_edge::has_pending_invocation_results(config.journal_path.clone()).await =>
        {
            // Provider capability is optional, but an authenticated Edge owner
            // must still be able to steer active work and replay durable
            // results. It runs control-only until discovery succeeds, so this
            // recovery path cannot admit new provider work.
            (None, None)
        }
        None => {
            return Err(NativeDeliveryError::other(
                "installed native provider is unavailable",
            ));
        }
    };
    let provider_is_available = providers.is_some();
    let delivery = connect_native_delivery(
        config,
        providers,
        admission_deadline,
        cancellation,
        withdrawal,
        refresh_tx,
        discovered_executables,
        invocation_owner,
        provider_available,
    )
    .await?;
    let owner = delivery.1;
    Ok((
        delivery.0,
        provider_is_available,
        verified_executables,
        owner,
    ))
}

/// Install the same native Edge owner and reconnect supervisor for a one-shot
/// CLI turn. The one-shot command owns the returned supervisor handle and
/// shuts it down after the canonical turn settles.
pub(crate) async fn start_headless_native_delivery(
    api: &astra_thin_client::ThinClient,
    token: &str,
    account_id: String,
    session_id: &str,
    executor: Arc<ToolExecutor>,
    permission_policy: PermissionPolicySubscription,
    terminal_deadline: Option<tokio::time::Instant>,
) -> Option<NativeDeliveryHandle> {
    let root = match executor.effective_project_root().canonicalize() {
        Ok(root) => root,
        Err(error) => {
            tracing::debug!(%error, "headless native collaborator workspace unavailable");
            return None;
        }
    };
    let materialization_id = match astra_runtime_env::load_or_create_materialization_id(&root) {
        Ok(id) => id,
        Err(error) => {
            tracing::debug!(%error, "headless native collaborator materialization unavailable");
            return None;
        }
    };
    let edge_agent_id = match crate::cli::chat_stream::try_edge_executor_instance_id() {
        Ok(id) => id.to_owned(),
        Err(error) => {
            tracing::debug!(%error, "headless native collaborator Edge identity unavailable");
            return None;
        }
    };
    let config = match build_native_delivery_config(
        api,
        token,
        account_id,
        edge_agent_id,
        session_id,
        materialization_id,
        executor,
        permission_policy,
        None,
        None,
    ) {
        Ok(config) => config,
        Err(error) => {
            tracing::debug!(%error, "headless native collaborator delivery unavailable");
            return None;
        }
    };
    let cancellation = CancellationToken::new();
    let deadline = terminal_deadline
        .map(|deadline| {
            Instant::now()
                + deadline
                    .saturating_duration_since(tokio::time::Instant::now())
                    .min(NATIVE_DELIVERY_STARTUP_TIMEOUT)
        })
        .unwrap_or_else(|| Instant::now() + NATIVE_DELIVERY_STARTUP_TIMEOUT);
    let (refresh_tx, refresh_rx) = mpsc::channel(1);
    let discovered_executables = Arc::new(std::sync::Mutex::new(
        native_codex::native_provider_executable_snapshot(),
    ));
    let (ready_tx, ready_rx) = tokio::sync::watch::channel(false);
    let handle = spawn_native_delivery_supervisor(
        config,
        cancellation,
        refresh_tx,
        refresh_rx,
        discovered_executables,
        ready_tx,
        session_id.to_owned(),
        Some(deadline),
    );
    // The supervisor reports optional capacity as ready even when the
    // provider is unavailable, so a headless turn never waits on this
    // capability. It still retains the owner and reconnect path for work
    // admitted after the first attachment.
    let mut ready = ready_rx;
    let _ = tokio::time::timeout(NATIVE_DELIVERY_STARTUP_WAIT, async {
        while !*ready.borrow() {
            if ready.changed().await.is_err() {
                break;
            }
        }
    })
    .await;
    Some(handle)
}

// The capability-only installation path is shared by production and real transport
// tests; tests replace only the external peer, not Astra auth/custody/dispatch.
#[allow(clippy::too_many_arguments)]
async fn connect_native_delivery(
    config: NativeDeliveryConfig,
    providers: Option<Vec<NativeProviderCandidate>>,
    admission_deadline: Instant,
    cancellation: &CancellationToken,
    withdrawal: CancellationToken,
    refresh_tx: mpsc::Sender<()>,
    discovered_executables: Arc<std::sync::Mutex<Vec<native_codex::NativeExecutableIdentity>>>,
    invocation_owner: Option<SharedEdgeInvocationOwner>,
    provider_available: Arc<AtomicBool>,
) -> Result<(NativeDeliveryHandle, SharedEdgeInvocationOwner), NativeDeliveryError> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let workspace_root = config
        .executor
        .effective_project_root()
        .canonicalize()
        .map_err(|_| NativeDeliveryError::other("native workspace is unavailable"))?;
    if withdrawal.is_cancelled() {
        return Err(NativeDeliveryError::other(
            "native delivery refresh requested before connection",
        ));
    }
    let providers = providers.unwrap_or_default();
    let snapshots = providers
        .iter()
        .map(|provider| provider.snapshot.clone())
        .collect::<Vec<_>>();
    let attachment = config.permission_policy.current();
    let bindings = providers
        .iter()
        .map(|provider| {
            let declaration = provider
                .snapshot
                .tool_declarations
                .first()
                .ok_or_else(|| NativeDeliveryError::other("missing native declaration"))?;
            let protocol =
                NativeCollaboratorProtocol::from_extension_fields(&declaration.extension_fields)
                    .ok_or_else(|| {
                        NativeDeliveryError::other("native collaborator protocol is not declared")
                    })?;
            let requirements =
                astra_turn_types::ProviderRuntimeRequirements::from_extension_fields(
                    &declaration.extension_fields,
                )
                .map_err(|_| NativeDeliveryError::other("invalid native runtime requirements"))?
                .ok_or_else(|| NativeDeliveryError::other("missing native runtime requirements"))?;
            let executable_identity = native_codex::native_executable_identity(
                std::path::Path::new(&requirements.executable),
            )
            .map_err(NativeDeliveryError::other)?;
            if executable_identity != provider.executable_identity {
                return Err(NativeDeliveryError::other(
                    "native executable changed during discovery",
                ));
            }
            let attachment = attachment.as_ref().ok_or_else(|| {
                NativeDeliveryError::other("native delivery requires a bound permission attachment")
            })?;
            Ok::<_, NativeDeliveryError>((
                provider.snapshot.clone(),
                declaration.native_tool_name.clone(),
                protocol,
                requirements,
                executable_identity,
                attachment.session_id().to_owned(),
                attachment.attachment_epoch(),
            ))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut request = config
        .websocket_url
        .as_str()
        .into_client_request()
        .map_err(|_| NativeDeliveryError::other("invalid Edge WebSocket endpoint"))?;
    let auth = delivery_auth_token(&config, admission_deadline, cancellation).await?;
    request.headers_mut().insert(
        "authorization",
        format!("Bearer {auth}")
            .parse()
            .map_err(|_| NativeDeliveryError::other("invalid Edge authentication header"))?,
    );
    let connect = tokio_tungstenite::connect_async(request);
    let (socket, _) = tokio::select! {
        biased;
        _ = cancellation.cancelled() => return Err(NativeDeliveryError::other("native delivery cancelled")),
        result = tokio::time::timeout_at(tokio::time::Instant::from_std(admission_deadline), connect) =>
            result.map_err(|_| NativeDeliveryError::other("native delivery connection deadline expired"))?
                .map_err(native_delivery_connection_error)?,
    };
    // Auth does not claim capacity: only recovery/installed consumer readiness
    // below permits the later authenticated REST advertisement.
    let auth = EdgeClientMessage::Auth {
        edge_agent_id: config.edge_agent_id.clone(),
        materialization_id: config.materialization_id.clone(),
        interaction_api_major: astra_server_types::AGENT_INTERACTION_API_MAJOR.into(),
        hostname: None,
        workspace_dir: Some(workspace_root.to_string_lossy().into_owned()),
        // Provider-only capacity must be present in the authenticated
        // handshake. REST publication happens after the socket is ready, so
        // sending the snapshot only there makes the server reject the socket
        // before it can ever publish or route the capability.
        capabilities: Some(capabilities(&config, &snapshots)),
    };
    let authenticated = tokio::time::timeout_at(
        tokio::time::Instant::from_std(admission_deadline),
        astra_edge::authenticate_connection(socket, auth, Some(&config.account_id), cancellation),
    )
    .await
    .map_err(|_| NativeDeliveryError::other("native delivery authentication deadline expired"))?
    .map_err(native_delivery_authentication_error)?;
    let (socket, account_id, edge_transport_id) = authenticated;
    // The server owns the transport identity. The local agent label is only
    // an authenticated capability selector and must never be reused as the
    // REST callback identity. Keep the replacement identity in the shared
    // session owner so already-admitted invocations use the current
    // connection for later interaction callbacks.
    *config.edge_transport_id.write().await = edge_transport_id;
    let config = Arc::new(config);
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let context = EdgeConnectionContext {
        account_id,
        edge_agent_id: config.edge_agent_id.clone(),
        workspace_dir: workspace_root.clone(),
        journal_path: config.journal_path.clone(),
        ready: Some(ready_tx),
    };
    let owner_cancel = cancellation.child_token();
    let capability_withdrawal = CancellationToken::new();
    let callback: Arc<dyn EdgeInvocationExecutor> = if !bindings.is_empty() {
        let providers = bindings
            .into_iter()
            .map(
                |(
                    snapshot,
                    tool_name,
                    protocol,
                    requirements,
                    executable_identity,
                    expected_session_id,
                    expected_attachment_epoch,
                )| {
                    Arc::new(CliNativeExecutor {
                        config: config.clone(),
                        snapshot,
                        workspace_root: workspace_root.clone(),
                        requirements,
                        tool_name,
                        protocol,
                        expected_session_id,
                        expected_attachment_epoch,
                        expected_executable_identity: Some(executable_identity),
                        invalidation: capability_withdrawal.clone(),
                    })
                },
            )
            .collect();
        Arc::new(NativeExecutorRouter { providers })
    } else {
        // Recovery can connect without a currently usable provider. The Edge
        // owner replays durable receipts, while every new request is denied
        // and the capability remains withdrawn.
        Arc::new(UnavailableNativeExecutor)
    };
    let invocation_owner = match invocation_owner {
        Some(owner) => {
            owner.lock().await.replace_executor(callback);
            owner
        }
        None => Arc::new(tokio::sync::Mutex::new(
            astra_edge::EdgeInvocationOwner::open(&context, callback)
                .await
                .map_err(|error| NativeDeliveryError::other(error.to_string()))?,
        )),
    };
    let task_cancel = owner_cancel.clone();
    let task_withdrawal = capability_withdrawal.clone();
    let task_external_withdrawal = withdrawal.clone();
    let task_config = config.clone();
    let task_snapshots = snapshots.clone();
    let task_invocation_owner = invocation_owner.clone();
    let (installed_tx, installed_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let owner = async {
            let mut invocation_owner = task_invocation_owner.lock().await;
            invocation_owner
                .serve_connection(
                    socket,
                    context,
                    task_cancel.clone(),
                    Some(task_withdrawal.clone()),
                )
                .await
        };
        tokio::pin!(owner);
        let mut ended_before_ready = false;
        let ready = tokio::select! {
            biased;
            _ = &mut owner => { ended_before_ready = true; false },
            _ = capability_withdrawal_requested(&task_external_withdrawal, &task_withdrawal) => {
                task_withdrawal.cancel();
                false
            }
            result = ready_rx => result.is_ok(),
        };
        let mut withdrawal_published = false;
        if ready {
            // The same task owns publication and withdrawal ordering. An
            // initializer cannot publish after an already-settled owner has
            // withdrawn its capacity.
            let result = tokio::time::timeout_at(
                tokio::time::Instant::from_std(admission_deadline),
                publish(&task_config, &task_snapshots),
            );
            tokio::pin!(result);
            let mut owner_ended = false;
            let published = tokio::select! {
                biased;
                _ = &mut owner => {
                    owner_ended = true;
                    // If publication was dispatched, settle it before the
                    // withdrawal; never issue these writes concurrently.
                    let _ = result.await;
                    false
                },
                _ = capability_withdrawal_requested(&task_external_withdrawal, &task_withdrawal) => {
                    task_withdrawal.cancel();
                    // Keep polling the Edge owner while the REST withdrawal
                    // settles. It owns the WebSocket input lane for already
                    // admitted work; awaiting the HTTP request by itself
                    // would make steer/cancel traffic look hung.
                    tokio::select! {
                        biased;
                        _ = &mut owner => {
                            owner_ended = true;
                            let _ = result.await;
                        }
                        _ = &mut result => {}
                    }
                    let withdrawal = tokio::time::timeout(
                        std::time::Duration::from_secs(10),
                        publish(&task_config, &[]),
                    );
                    tokio::pin!(withdrawal);
                    if owner_ended {
                        let _ = withdrawal.await;
                    } else {
                        tokio::select! {
                            biased;
                            _ = &mut owner => {
                                owner_ended = true;
                                let _ = withdrawal.await;
                            }
                            _ = &mut withdrawal => {}
                        }
                    }
                    withdrawal_published = true;
                    false
                }
                result = &mut result => result.is_ok_and(|result| result.is_ok()),
            };
            let _ = installed_tx.send(published);
            if !owner_ended {
                if !published {
                    if !withdrawal_published {
                        task_cancel.cancel();
                    }
                }
                if withdrawal_published {
                    let _ = owner.await;
                } else {
                    tokio::select! {
                        biased;
                        _ = &mut owner => {}
                        _ = capability_withdrawal_requested(&task_external_withdrawal, &task_withdrawal) => {
                            task_withdrawal.cancel();
                            let withdrawal = tokio::time::timeout(
                                std::time::Duration::from_secs(10),
                                publish(&task_config, &[]),
                            );
                            tokio::pin!(withdrawal);
                            tokio::select! {
                                biased;
                                _ = &mut owner => {
                                    owner_ended = true;
                                    let _ = withdrawal.await;
                                }
                                _ = &mut withdrawal => {}
                            }
                            withdrawal_published = true;
                            if !owner_ended {
                                let _ = owner.await;
                            }
                        }
                    }
                }
            }
        } else {
            let _ = installed_tx.send(false);
            if !ended_before_ready {
                task_cancel.cancel();
                let _ = owner.await;
            }
        }
        // Withdraw capacity when admission/transport ends. No native work can
        // be dispatched through a disconnected consumer.
        // The external-withdrawal branches publish before waiting for a long
        // admitted child to settle. Natural owner termination reaches this
        // single publication point after settlement.
        if !ready || !withdrawal_published {
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                publish(&task_config, &[]),
            )
            .await;
        }
    });
    let mut handle = NativeDeliveryHandle {
        cancellation: owner_cancel,
        withdrawal,
        auth: config.auth.clone(),
        account_id: config.account_id.clone(),
        auth_owner: config.auth_owner.clone(),
        refresh_tx,
        discovered_executables,
        invocation_owner: Some(invocation_owner.clone()),
        task: Some(task),
        ready: tokio::sync::watch::channel(true).1,
        provider_available,
    };
    let installed = tokio::select! {
        biased;
        _ = cancellation.cancelled() => false,
        result = tokio::time::timeout_at(tokio::time::Instant::from_std(admission_deadline), installed_rx) =>
            result.ok().and_then(Result::ok).unwrap_or(false),
    };
    if !installed {
        if capability_withdrawal.is_cancelled() {
            // Capability withdrawal is a drain, not session teardown. Keep
            // the owner alive long enough to settle admitted work and its
            // durable receipts; only a publication/transport failure uses
            // the cancelling shutdown path.
            handle.wait().await;
        } else {
            handle.shutdown().await;
        }
        return Err(NativeDeliveryError::other(
            "native delivery recovery or capacity publication failed",
        ));
    }
    Ok((handle, invocation_owner))
}

/// Keep one reconnect supervisor for both TUI and headless turns. The
/// supervisor owns the session-scoped invocation owner; a connection handle
/// is only the current delivery attachment. This is deliberately the only
/// place that decides whether a transport is replaced, withdrawn, retried, or
/// finally settled.
#[allow(clippy::too_many_arguments)]
fn spawn_native_delivery_supervisor(
    config: NativeDeliveryConfig,
    cancellation: CancellationToken,
    refresh_tx: mpsc::Sender<()>,
    mut refresh_rx: mpsc::Receiver<()>,
    discovered_executables: Arc<std::sync::Mutex<Vec<native_codex::NativeExecutableIdentity>>>,
    ready_tx: tokio::sync::watch::Sender<bool>,
    session_id: String,
    first_admission_deadline: Option<Instant>,
) -> NativeDeliveryHandle {
    let supervisor_cancel = cancellation.clone();
    let ready_rx = ready_tx.subscribe();
    let task_config = config.clone();
    let handle_auth = config.auth.clone();
    let handle_account_id = config.account_id.clone();
    let handle_auth_owner = config.auth_owner.clone();
    let task_refresh_tx = refresh_tx.clone();
    let task_discovered_executables = discovered_executables.clone();
    let provider_available = Arc::new(AtomicBool::new(false));
    let task_provider_available = provider_available.clone();
    let task = tokio::spawn(async move {
        let mut startup_tx = Some(ready_tx);
        let mut reconnect_failures = 0_u32;
        let mut invocation_owner: Option<SharedEdgeInvocationOwner> = None;
        let mut first_admission_deadline = first_admission_deadline;
        loop {
            if supervisor_cancel.is_cancelled() {
                break;
            }
            let admission_deadline = first_admission_deadline
                .take()
                .unwrap_or_else(|| Instant::now() + NATIVE_DELIVERY_STARTUP_TIMEOUT);
            let result = install_native_delivery(
                config.clone(),
                admission_deadline,
                &supervisor_cancel,
                CancellationToken::new(),
                task_refresh_tx.clone(),
                task_discovered_executables.clone(),
                invocation_owner.clone(),
                task_provider_available.clone(),
            )
            .await;
            match result {
                Ok((mut delivery, provider_available, verified_executables, owner)) => {
                    task_provider_available.store(provider_available, Ordering::Release);
                    invocation_owner = Some(owner);
                    if let Some(verified_executables) = verified_executables {
                        if let Ok(mut selected) = task_discovered_executables.lock() {
                            *selected = verified_executables;
                        }
                    }
                    let _ = startup_tx.take().map(|tx| tx.send(true));
                    let mut refresh_requested = false;
                    tokio::select! {
                        _ = supervisor_cancel.cancelled() => {
                            delivery.cancel();
                            delivery.wait().await;
                            break;
                        }
                        refresh = refresh_rx.recv() => {
                            if refresh.is_none() {
                                delivery.cancel();
                                delivery.wait().await;
                                break;
                            }
                            refresh_requested = true;
                            delivery.withdraw();
                            delivery.wait().await;
                        }
                        _ = delivery.wait() => {}
                    }
                    if supervisor_cancel.is_cancelled() {
                        break;
                    }
                    if refresh_requested {
                        reconnect_failures = 0;
                        continue;
                    }
                    reconnect_failures = 1;
                    tracing::warn!(
                        session_id = %session_id,
                        "native collaborator transport ended; retrying capability discovery"
                    );
                }
                Err(error) => {
                    task_provider_available.store(false, Ordering::Release);
                    let _ = startup_tx.take().map(|tx| tx.send(true));
                    tracing::warn!(
                        session_id = %session_id,
                        %error,
                        "native collaborator reconnect unavailable"
                    );
                    let recovery_required = if let Some(owner) = invocation_owner.as_ref() {
                        owner.lock().await.has_unsettled_work()
                    } else {
                        astra_edge::has_pending_invocation_results(task_config.journal_path.clone())
                            .await
                    };
                    // Once a transport has been established, a failed
                    // reconnect is a transport failure, not an optional
                    // capability probe. Keep the existing backoff loop alive
                    // even when there is no currently unsettled invocation;
                    // otherwise one transient outage permanently strands the
                    // session until an unrelated refresh request arrives.
                    if invocation_owner.is_none() && !recovery_required {
                        tokio::select! {
                            _ = supervisor_cancel.cancelled() => break,
                            refresh = refresh_rx.recv() => {
                                if refresh.is_none() {
                                    break;
                                }
                                reconnect_failures = 0;
                            }
                        }
                        continue;
                    }
                    reconnect_failures = reconnect_failures.saturating_add(1);
                }
            }

            let delay_secs = match reconnect_failures.min(5) {
                0 => 1,
                1 => 1,
                2 => 2,
                3 => 4,
                4 => 8,
                _ => 16,
            };
            tokio::select! {
                _ = supervisor_cancel.cancelled() => break,
                refresh = refresh_rx.recv() => {
                    if refresh.is_none() {
                        break;
                    }
                    reconnect_failures = 0;
                }
                _ = tokio::time::sleep(std::time::Duration::from_secs(delay_secs)) => {}
            }
        }
        if let Some(invocation_owner) = invocation_owner.as_ref() {
            settle_native_invocation_owner(invocation_owner).await;
        }
    });
    NativeDeliveryHandle {
        cancellation,
        withdrawal: CancellationToken::new(),
        auth: handle_auth,
        account_id: handle_account_id,
        auth_owner: handle_auth_owner,
        refresh_tx,
        discovered_executables,
        invocation_owner: None,
        task: Some(task),
        ready: ready_rx,
        provider_available,
    }
}

fn native_journal_path(session_id: &str) -> PathBuf {
    astra_services::session_journal::journal_file_path(session_id)
        .with_extension("native-edge.jsonl")
}

/// Install the selected CLI capacity only after the canonical interactive
/// session identity exists. The turn boundary is the existing owner for this
/// one-time preparation; the handle itself remains owned by SessionState until
/// the session attachment changes or the TUI shuts down.
pub(crate) async fn ensure_session_native_delivery(
    state: &mut crate::cli::session::session_state::SessionState,
    api: &astra_thin_client::ThinClient,
    token: &str,
    session_id: &str,
) {
    let attachment_epoch = state.session_attachment_epoch;
    let account_id = state
        .ingestion_user_id
        .clone()
        .or_else(crate::cli::cli_config::cli_utils::cli_account_id)
        .filter(|value| !value.trim().is_empty());
    if let Some(handle) = state.native_delivery.as_ref()
        && state.native_delivery_session_id.as_deref() == Some(session_id)
        && state.native_delivery_attachment_epoch == Some(attachment_epoch)
        && account_id.as_deref() == Some(handle.account_id())
        && !handle.credential_generation_changed()
        && !handle.is_finished()
    {
        handle.update_auth(token).await;
        if handle.executable_changed() {
            // A PATH/launcher change is an explicit capability invalidation,
            // not a reason to create a second owner. The current owner drains
            // admitted work; its supervisor coalesces this request and probes
            // the current environment before advertising again.
            handle.request_refresh_if_environment_changed();
        }
        if !handle.provider_available() {
            // A settled but unavailable optional capability must be retried on
            // a later normal turn. Authentication/network failure is not a
            // permanent session state, and the bounded refresh channel
            // coalesces concurrent requests.
            handle.request_refresh();
        }
        wait_for_native_delivery_ready(handle).await;
        // Capacity is optional. A slow or unavailable provider must not block
        // the ordinary turn; the next turn reuses the same readiness watch.
        return;
    }

    if let Some(handle) = state.native_delivery.take() {
        state.native_delivery_refresh.clear();
        handle.shutdown().await;
    } else {
        state.native_delivery_refresh.clear();
    }
    state.native_delivery_session_id = None;
    state.native_delivery_attachment_epoch = None;

    let Some(shutdown) = state.native_delivery_shutdown.clone() else {
        // Headless/line callers have no interactive delivery owner. They
        // continue through the ordinary canonical turn path unchanged.
        return;
    };
    if shutdown.is_cancelled() {
        return;
    }

    let Some(account_id) = account_id.filter(|value| !value.trim().is_empty()) else {
        tracing::debug!("native collaborator delivery skipped: account identity unavailable");
        return;
    };
    let root = match std::env::current_dir()
        .ok()
        .and_then(|path| std::fs::canonicalize(path).ok())
    {
        Some(root) => root,
        None => {
            tracing::warn!("native collaborator delivery skipped: workspace is unavailable");
            return;
        }
    };
    let materialization_id = match astra_runtime_env::load_or_create_materialization_id(&root) {
        Ok(id) => id,
        Err(error) => {
            tracing::warn!(%error, "native collaborator delivery skipped: materialization is unavailable");
            return;
        }
    };
    let edge_agent_id = match crate::cli::chat_stream::try_edge_executor_instance_id() {
        Ok(id) => id.to_owned(),
        Err(error) => {
            tracing::warn!(%error, "native collaborator delivery skipped: Edge identity unavailable");
            return;
        }
    };

    let mut executor = ToolExecutor::new(root.clone())
        .with_active_session_id(session_id.to_owned())
        .with_cloud(api.api_origin(), token.to_owned())
        .with_shared_file_journal(state.file_journal.clone())
        .with_shared_file_state(state.file_state.clone())
        .with_shared_database_snapshot_journal(state.database_snapshot_journal.clone())
        .with_shared_git_worktree_journal(state.git_worktree_journal.clone())
        .with_shared_session_state_journal(state.session_state_journal.clone())
        .with_bg_task_commands(state.bg_task_commands.clone())
        .with_bg_task_list_cache(state.bg_task_list_cache.clone())
        .with_bash_detach_slot(state.bash_detach_slot.clone());
    if let Some(observability) = state.observability_session.clone() {
        executor = executor.with_observability_session(observability);
    }

    let config = match build_native_delivery_config(
        api,
        token,
        account_id,
        edge_agent_id,
        session_id,
        materialization_id,
        Arc::new(executor),
        state.perm_manager.subscribe_permission_policy(),
        state.tui_ask_user_request_tx.clone(),
        state.tui_approval_request_tx.clone(),
    ) {
        Ok(config) => config,
        Err(error) => {
            tracing::warn!(%error, "native collaborator delivery skipped: invalid WebSocket endpoint");
            return;
        }
    };

    let task_cancel = shutdown.child_token();
    let (refresh_tx, refresh_rx) = mpsc::channel(1);
    state.native_delivery_refresh.bind(refresh_tx.clone());
    let discovered_executables = Arc::new(std::sync::Mutex::new(
        native_codex::native_provider_executable_snapshot(),
    ));
    let (ready_tx, _) = tokio::sync::watch::channel(false);
    state.native_delivery = Some(spawn_native_delivery_supervisor(
        config,
        task_cancel,
        refresh_tx,
        refresh_rx,
        discovered_executables,
        ready_tx,
        session_id.to_owned(),
        None,
    ));
    state.native_delivery_session_id = Some(session_id.to_owned());
    state.native_delivery_attachment_epoch = Some(attachment_epoch);
    if let Some(handle) = state.native_delivery.as_ref() {
        wait_for_native_delivery_ready(handle).await;
    }
    tracing::info!(
        session_id,
        attachment_epoch,
        "native collaborator delivery startup scheduled"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edge_tools::{native_claude, native_codex};

    #[test]
    fn native_journal_is_a_session_sibling_file() {
        let session_id = "session-test";
        let journal = native_journal_path(session_id);
        assert_eq!(
            journal.parent(),
            astra_services::session_journal::journal_file_path(session_id).parent()
        );
        assert!(
            journal
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".native-edge.jsonl"))
        );
    }

    #[test]
    fn authentication_failure_is_typed_for_recovery() {
        assert!(NativeDeliveryError::Authentication("rejected".into()).is_authentication());
        assert!(!NativeDeliveryError::other("deadline").is_authentication());
        assert!(!NativeDeliveryError::other("connection failed").is_authentication());
        assert!(
            !native_delivery_authentication_error(astra_edge::EdgeAuthenticationError::Timeout)
                .is_authentication()
        );
        assert!(
            native_delivery_authentication_error(astra_edge::EdgeAuthenticationError::Rejected)
                .is_authentication()
        );
    }

    #[tokio::test]
    async fn refresh_handle_targets_the_current_session_owner() {
        let refresh = NativeDeliveryRefreshHandle::default();
        let (first_tx, mut first_rx) = mpsc::channel(1);
        refresh.bind(first_tx);
        refresh.request();
        assert!(first_rx.recv().await.is_some());

        let (second_tx, mut second_rx) = mpsc::channel(1);
        refresh.bind(second_tx);
        refresh.request();
        assert!(second_rx.recv().await.is_some());
        assert!(first_rx.try_recv().is_err());

        refresh.clear();
        refresh.request();
        assert!(second_rx.try_recv().is_err());
    }

    #[test]
    fn native_registration_advertises_only_the_provider_stage_surface() {
        let workspace = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let (consumer, _owner) = consumer(workspace.path(), runtime.path(), None);
        let value = capabilities(&consumer.config, std::slice::from_ref(&consumer.snapshot));
        let surface = &value["binding"]["tool_surface"];
        assert_eq!(surface["tool_names"], json!([]));
        assert_eq!(surface["admissions"], json!([]));
        assert_eq!(surface["denials"], json!([]));
        assert_eq!(
            value["provider_discovery"][0]["tool_declarations"][0]["native_tool_name"],
            native_codex::TOOL_NAME
        );
    }

    #[test]
    fn native_registration_keeps_each_verified_protocol_selectable() {
        let workspace = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let (consumer, _owner) = consumer(workspace.path(), runtime.path(), None);
        let claude_declaration = native_claude::provider_declaration(requirements(runtime.path()))
            .expect("Claude declaration must use the shared provider contract");
        let claude_snapshot = Arc::new(
            discovery_snapshot(
                "edge-test",
                "materialization-test",
                workspace.path().to_str().unwrap(),
                claude_declaration,
            )
            .unwrap(),
        );
        let value = capabilities(
            &consumer.config,
            &[consumer.snapshot.clone(), claude_snapshot],
        );
        let declarations = value["provider_discovery"]
            .as_array()
            .expect("provider discovery must be an array");
        let names = declarations
            .iter()
            .filter_map(|snapshot| {
                snapshot["tool_declarations"][0]["native_tool_name"]
                    .as_str()
                    .map(str::to_owned)
            })
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            names,
            [
                native_codex::TOOL_NAME.to_owned(),
                native_claude::TOOL_NAME.to_owned(),
            ]
            .into_iter()
            .collect()
        );
    }

    use crate::cli::permission_manager::{PermissionManager, PermissionMode};
    use crate::cli::session::session_state::SessionState;
    use astra_turn_types::{NativeToolId, ProviderRuntimeRequirements, ProviderToolDeclaration};
    use std::time::Duration;

    fn snapshot(
        requirements: &ProviderRuntimeRequirements,
        root: &std::path::Path,
    ) -> ProviderDiscoverySnapshot {
        let mut extension_fields = serde_json::Map::new();
        extension_fields.insert(
            astra_turn_types::PROVIDER_RUNTIME_REQUIREMENTS_KEY.into(),
            json!(requirements),
        );
        extension_fields.insert(
            NativeCollaboratorProtocol::EXTENSION_KEY.into(),
            json!(NativeCollaboratorProtocol::CodexAppServer.extension_value()),
        );
        let declaration = ProviderToolDeclaration {
            native_tool_id: NativeToolId::new(native_codex::TOOL_NAME).unwrap(),
            native_tool_name: native_codex::TOOL_NAME.into(),
            stable_tool_alias: None,
            title: None,
            description: None,
            input_schema: native_codex::schema()["function"]["parameters"].clone(),
            output_schema: None,
            claims: Default::default(),
            task_support: Default::default(),
            extension_fields,
        };
        discovery_snapshot(
            "edge-test",
            "materialization-test",
            root.to_str().unwrap(),
            declaration,
        )
        .unwrap()
    }

    fn requirements(root: &std::path::Path) -> ProviderRuntimeRequirements {
        ProviderRuntimeRequirements {
            executable: root.join("codex").to_str().unwrap().into(),
            read_paths: vec![
                root.join("codex").to_str().unwrap().into(),
                root.join("platform").to_str().unwrap().into(),
            ],
        }
    }

    fn consumer(
        workspace: &std::path::Path,
        runtime: &std::path::Path,
        approval_tx: Option<chat_stream::ApprovalRequestTx>,
    ) -> (CliNativeExecutor, SessionState) {
        let mut owner = SessionState {
            perm_manager: PermissionManager::with_project_mode(PermissionMode::Prompt, workspace),
            ..SessionState::default()
        };
        owner.set_session_id("session-test");
        let requirements = requirements(runtime);
        let consumer = CliNativeExecutor {
            snapshot: Arc::new(snapshot(&requirements, workspace)),
            workspace_root: workspace.canonicalize().unwrap(),
            requirements,
            tool_name: native_codex::TOOL_NAME.into(),
            protocol: NativeCollaboratorProtocol::CodexAppServer,
            expected_session_id: "session-test".into(),
            expected_attachment_epoch: owner.session_attachment_epoch,
            expected_executable_identity: None,
            invalidation: CancellationToken::new(),
            config: Arc::new(NativeDeliveryConfig {
                websocket_url: "ws://127.0.0.1:1/edge/ws".into(),
                api: astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap(),
                auth: Arc::new(tokio::sync::RwLock::new("test-only-token".into())),
                auth_provider: None,
                auth_owner: None,
                account_id: "account-test".into(),
                edge_agent_id: "edge-test".into(),
                edge_transport_id: Arc::new(tokio::sync::RwLock::new("transport-test".into())),
                workspace_id: None,
                materialization_id: "materialization-test".into(),
                journal_path: workspace.join("journal.json"),
                executor: Arc::new(ToolExecutor::new(workspace)),
                ask_user_request_tx: None,
                permission_policy: owner.perm_manager.subscribe_permission_policy(),
                approval_request_tx: approval_tx,
            }),
        };
        (consumer, owner)
    }

    async fn admission(
        consumer: &CliNativeExecutor,
        invocation: &EdgeInvocation,
        cancel: &CancellationToken,
    ) -> Result<ApprovedNativeRuntime, String> {
        consumer.approve_bootstrap(invocation, cancel).await
    }

    fn invocation(consumer: &CliNativeExecutor) -> EdgeInvocation {
        EdgeInvocation {
            identity: astra_turn_types::ToolInvocationIdentity::new(
                "account-test",
                "session-test",
                "run-test",
                "chain-test",
                "call-test",
            )
            .unwrap(),
            delivery_generation: 1,
            tool: native_codex::TOOL_NAME.into(),
            args: json!({"task": "No paid invocation", "anchor_run_id": "anchor-test"}),
            execution_deadline: Instant::now() + Duration::from_secs(5),
            command_timeout_cap_ms: Some(120_000),
            runtime_process_authorization: None,
            input_rx: tokio::sync::mpsc::channel(1).1,
            execution_ceiling: Some(Box::new(
                astra_server_types::edge_ws_protocol::EdgeExecutionCeiling {
                    workspace_root: consumer.workspace_root.to_str().unwrap().into(),
                    workspace_id: None,
                    materialization_id: Some("materialization-test".into()),
                    execution_binding_generation: 1,
                    runtime_read_paths: consumer.requirements.read_paths.clone(),
                    workspace_write_allowed: false,
                    network_allowed: false,
                },
            )),
        }
    }

    #[tokio::test]
    async fn actual_invocation_allow_once_does_not_write_permission_owner() {
        let workspace = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<chat_stream::ApprovalRequest>(1);
        let (consumer, owner) = consumer(workspace.path(), runtime.path(), Some(tx));
        assert!(rx.try_recv().is_err(), "discovery has no approval effect");
        for _ in 0..2 {
            let call = invocation(&consumer);
            let identity = call.identity.clone();
            let deadline = call.execution_deadline;
            let ui = async {
                let prompt = rx.recv().await.unwrap();
                let context = prompt
                    .metadata
                    .as_ref()
                    .unwrap()
                    .runtime_dependencies
                    .as_ref()
                    .unwrap();
                assert_eq!(context.invocation, identity);
                assert_eq!(context.attachment_epoch, owner.session_attachment_epoch);
                assert_eq!(context.execution_binding_generation, 1);
                assert_eq!(
                    prompt.args["provider_snapshot_hash"].as_str().unwrap(),
                    consumer.snapshot.content_hash
                );
                assert_eq!(context.deadline, deadline);
                prompt
                    .response_tx
                    .send(chat_stream::ApprovalResponse::AllowOnce)
                    .unwrap();
            };
            let (result, ()) = tokio::join!(consumer.execute(call, CancellationToken::new()), ui);
            assert_eq!(result.metadata.unwrap()["execution_fact"], "not_executed");
            assert!(
                matches!(
                    owner
                        .perm_manager
                        .subscribe_permission_policy()
                        .current()
                        .unwrap()
                        .check_sandbox_expansion(
                            "sandbox_expand:native_codex",
                            &bootstrap_args(
                                &consumer.snapshot,
                                &consumer.requirements.read_paths[0]
                            )
                        ),
                    GateOutcome::NeedApproval { .. }
                ),
                "allow once must not persist an override"
            );
        }
    }

    #[tokio::test]
    async fn missing_approval_ui_fails_closed_with_a_recovery_action() {
        let workspace = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let (consumer, _owner) = consumer(workspace.path(), runtime.path(), None);

        let result = consumer
            .execute(invocation(&consumer), CancellationToken::new())
            .await;

        assert!(result.is_error);
        assert_eq!(
            result.output,
            "native collaborator needs permission approval; run it in the interactive TUI or use --auto-approve"
        );
        assert_eq!(result.metadata.unwrap()["execution_fact"], "not_executed");
    }

    #[tokio::test]
    async fn pending_approval_observes_hard_revocation_without_ui_response() {
        let workspace = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<chat_stream::ApprovalRequest>(1);
        let (consumer, mut owner) = consumer(workspace.path(), runtime.path(), Some(tx));
        let ui = async {
            let prompt = rx.recv().await.unwrap();
            owner.perm_manager.set_mode(PermissionMode::Deny);
            prompt
        };
        let (result, prompt) = tokio::join!(
            consumer.execute(invocation(&consumer), CancellationToken::new()),
            ui
        );
        assert_eq!(result.metadata.unwrap()["execution_fact"], "not_executed");
        assert!(prompt.response_tx.is_closed());
    }

    #[tokio::test]
    async fn queued_user_deny_wins_simultaneously_ready_policy_allow() {
        let workspace = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<chat_stream::ApprovalRequest>(1);
        let (consumer, mut owner) = consumer(workspace.path(), runtime.path(), Some(tx));
        let ui = async {
            let prompt = rx.recv().await.unwrap();
            // No await between these operations: both watch and response are
            // ready before the admission future can be polled again.
            owner.perm_manager.set_mode(PermissionMode::Bypass);
            assert!(
                !dependencies_need_approval(
                    &consumer.config.permission_policy,
                    &consumer.expected_session_id,
                    consumer.expected_attachment_epoch,
                    &consumer.snapshot,
                    &consumer.requirements,
                )
                .unwrap()
            );
            prompt
                .response_tx
                .send(chat_stream::ApprovalResponse::Deny)
                .unwrap();
        };
        let (result, ()) = tokio::join!(
            biased;
            consumer.execute(invocation(&consumer), CancellationToken::new()),
            ui
        );
        assert!(result.is_error);
        assert_eq!(result.output, "native bootstrap approval denied");
        let metadata = result.metadata.unwrap();
        assert_eq!(metadata["execution_fact"], "not_executed");
        assert!(
            metadata.get("native_collaborator").is_none(),
            "native leaf must not dispatch"
        );
    }

    #[tokio::test]
    async fn policy_change_rechecks_all_dependencies_and_returns_policy_not_user_approval() {
        let workspace = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<chat_stream::ApprovalRequest>(1);
        let (consumer, mut owner) = consumer(workspace.path(), runtime.path(), Some(tx));
        let call = invocation(&consumer);
        let cancel = CancellationToken::new();
        let ui = async {
            let prompt = rx.recv().await.unwrap();
            owner.perm_manager.record_approval(
                "sandbox_expand:native_codex",
                Some(&bootstrap_args(
                    &consumer.snapshot,
                    &consumer.requirements.read_paths[0],
                )),
                true,
            );
            tokio::task::yield_now().await;
            assert!(
                !prompt.response_tx.is_closed(),
                "one allowed path is not the complete set"
            );
            owner.perm_manager.set_mode(PermissionMode::Bypass);
            prompt
        };
        let (result, prompt) = tokio::join!(admission(&consumer, &call, &cancel), ui);
        assert_eq!(
            result.unwrap().admission_source,
            ToolInvocationAdmissionSource::Policy
        );
        assert!(prompt.response_tx.is_closed());
    }

    #[tokio::test]
    async fn coalesced_same_session_rebind_revokes_pending_and_future_dispatch() {
        let workspace = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<chat_stream::ApprovalRequest>(1);
        let (consumer, mut owner) = consumer(workspace.path(), runtime.path(), Some(tx));
        let ui = async {
            let prompt = rx.recv().await.unwrap();
            owner.clear_session_id();
            owner.set_session_id("session-test");
            prompt
                .response_tx
                .send(chat_stream::ApprovalResponse::AllowOnce)
                .unwrap();
        };
        let (result, ()) = tokio::join!(
            consumer.execute(invocation(&consumer), CancellationToken::new()),
            ui
        );
        assert_eq!(result.metadata.unwrap()["execution_fact"], "not_executed");
        owner.perm_manager.set_mode(PermissionMode::Bypass);
        let result = consumer
            .execute(invocation(&consumer), CancellationToken::new())
            .await;
        assert_eq!(result.metadata.unwrap()["execution_fact"], "not_executed");
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn always_allow_is_rejected_without_permission_writeback() {
        let workspace = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<chat_stream::ApprovalRequest>(1);
        let (consumer, owner) = consumer(workspace.path(), runtime.path(), Some(tx));
        let call = invocation(&consumer);
        let cancel = CancellationToken::new();
        let ui = async {
            rx.recv()
                .await
                .unwrap()
                .response_tx
                .send(chat_stream::ApprovalResponse::AlwaysAllow)
                .unwrap();
        };
        let (result, ()) = tokio::join!(admission(&consumer, &call, &cancel), ui);
        assert!(result.is_err());
        assert!(matches!(
            owner
                .perm_manager
                .subscribe_permission_policy()
                .current()
                .unwrap()
                .check_sandbox_expansion(
                    "sandbox_expand:native_codex",
                    &bootstrap_args(&consumer.snapshot, &consumer.requirements.read_paths[0])
                ),
            GateOutcome::NeedApproval { .. }
        ));
    }

    #[tokio::test]
    async fn same_id_reset_and_closed_writer_reject_without_prompt() {
        let workspace = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<chat_stream::ApprovalRequest>(1);
        let (consumer, mut owner) = consumer(workspace.path(), runtime.path(), Some(tx));
        owner.perm_manager.set_mode(PermissionMode::Bypass);
        assert!(
            current_policy(
                &consumer.config.permission_policy,
                &consumer.expected_session_id,
                consumer.expected_attachment_epoch,
            )
            .is_ok()
        );
        owner.reset_for_new_session();
        let result = consumer
            .execute(invocation(&consumer), CancellationToken::new())
            .await;
        assert_eq!(result.metadata.unwrap()["execution_fact"], "not_executed");
        drop(owner);
        let result = consumer
            .execute(invocation(&consumer), CancellationToken::new())
            .await;
        assert_eq!(result.metadata.unwrap()["execution_fact"], "not_executed");
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn mismatched_ceiling_cancel_unbound_and_forbidden_paths_never_prompt() {
        let workspace = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<chat_stream::ApprovalRequest>(1);
        let (consumer, mut owner) = consumer(workspace.path(), runtime.path(), Some(tx));
        let mut call = invocation(&consumer);
        call.execution_ceiling
            .as_mut()
            .unwrap()
            .runtime_read_paths
            .push("/unapproved".into());
        assert_eq!(
            consumer
                .execute(call, CancellationToken::new())
                .await
                .metadata
                .unwrap()["execution_fact"],
            "not_executed"
        );
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(
            admission(&consumer, &invocation(&consumer), &cancel)
                .await
                .is_err()
        );
        let mut forbidden = consumer.requirements.clone();
        forbidden
            .read_paths
            .push(workspace.path().join(".env.local").to_str().unwrap().into());
        assert!(
            dependencies_need_approval(
                &consumer.config.permission_policy,
                &consumer.expected_session_id,
                consumer.expected_attachment_epoch,
                &snapshot(&forbidden, workspace.path()),
                &forbidden,
            )
            .is_err()
        );
        owner.clear_session_id();
        owner.perm_manager.set_active_session_id("session-test");
        owner.perm_manager.set_mode(PermissionMode::Bypass);
        assert!(
            admission(&consumer, &invocation(&consumer), &CancellationToken::new())
                .await
                .is_err()
        );
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn pending_approval_cancellation_and_deadline_close_response_without_dispatch() {
        let workspace = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<chat_stream::ApprovalRequest>(1);
        let (consumer, _owner) = consumer(workspace.path(), runtime.path(), Some(tx));
        let cancel = CancellationToken::new();
        let ui = async {
            let prompt = rx.recv().await.unwrap();
            cancel.cancel();
            prompt
        };
        let (result, prompt) =
            tokio::join!(consumer.execute(invocation(&consumer), cancel.clone()), ui);
        assert_eq!(result.metadata.unwrap()["execution_fact"], "not_executed");
        assert!(prompt.response_tx.is_closed());
        let ui = async {
            let prompt = rx.recv().await.unwrap();
            tokio::time::sleep(Duration::from_secs(6)).await;
            prompt
        };
        let (result, prompt) = tokio::join!(
            consumer.execute(invocation(&consumer), CancellationToken::new()),
            ui
        );
        assert_eq!(result.metadata.unwrap()["execution_fact"], "not_executed");
        assert!(prompt.response_tx.is_closed());
    }

    #[tokio::test]
    async fn native_authentication_has_a_bounded_startup_deadline() {
        use futures_util::StreamExt;

        let workspace = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let executable = runtime.path().join("codex");
        std::fs::write(&executable, b"provider").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&executable, permissions).unwrap();
        }
        let (consumer, _owner) = consumer(workspace.path(), runtime.path(), None);
        let discovery = Arc::new(snapshot(&consumer.requirements, workspace.path()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("ws://{}/edge/ws", listener.local_addr().unwrap());
        let peer = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let _ = ws.next().await;
            tokio::time::sleep(Duration::from_secs(5)).await;
        });

        let mut config = (*consumer.config).clone();
        config.websocket_url = endpoint;
        let error = tokio::time::timeout(
            Duration::from_secs(2),
            connect_native_delivery(
                config,
                Some(vec![NativeProviderCandidate {
                    snapshot: discovery,
                    executable_identity: native_codex::native_executable_identity(&executable)
                        .unwrap(),
                }]),
                Instant::now() + Duration::from_millis(100),
                &CancellationToken::new(),
                CancellationToken::new(),
                mpsc::channel(1).0,
                Arc::new(std::sync::Mutex::new(Vec::new())),
                None,
                Arc::new(AtomicBool::new(false)),
            ),
        )
        .await
        .expect("authentication must not hang past the test guard");
        let error = match error {
            Ok(_) => panic!("an unauthenticated peer cannot publish native capacity"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("deadline"));
        peer.abort();
        let _ = peer.await;
    }

    #[tokio::test]
    async fn credential_refresh_cannot_block_delivery_shutdown() {
        #[derive(Debug)]
        struct NeverBearer;

        impl astra_thin_client::client::BearerProvider for NeverBearer {
            fn token(
                &self,
            ) -> futures_util::future::BoxFuture<
                '_,
                Result<String, astra_thin_client::ThinClientError>,
            > {
                Box::pin(std::future::pending())
            }
        }

        let workspace = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let (consumer, _owner) = consumer(workspace.path(), runtime.path(), None);
        let mut config = (*consumer.config).clone();
        config.auth_provider = Some(Arc::new(NeverBearer));
        let cancellation = CancellationToken::new();
        let pending = delivery_auth_token(
            &config,
            Instant::now() + Duration::from_secs(60),
            &cancellation,
        );
        tokio::pin!(pending);
        let guard = tokio::time::timeout(Duration::from_millis(100), async {
            cancellation.cancel();
            pending.await
        })
        .await
        .expect("credential acquisition must observe cancellation");
        assert!(guard.is_err());
        assert!(guard.unwrap_err().to_string().contains("cancelled"));
    }

    #[tokio::test]
    async fn unavailable_provider_can_keep_the_edge_owner_for_receipt_recovery() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

        let http = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wiremock::matchers::path("/agents/edge"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
            .mount(&http)
            .await;
        let workspace = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("ws://{}/edge/ws", listener.local_addr().unwrap());
        let peer = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let _auth = ws.next().await.unwrap().unwrap();
            ws.send(Message::Text(
                serde_json::to_string(
                    &astra_server_types::edge_ws_protocol::EdgeServerMessage::AuthOk {
                        user_id: "account-test".into(),
                        edge_id: "edge-test".into(),
                        interaction_api_major: astra_server_types::AGENT_INTERACTION_API_MAJOR
                            .into(),
                    },
                )
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
            while ws.next().await.is_some() {}
        });
        let permission_owner = SessionState {
            perm_manager: PermissionManager::with_project_mode(
                PermissionMode::Prompt,
                workspace.path(),
            ),
            ..SessionState::default()
        };
        let config = NativeDeliveryConfig {
            websocket_url: endpoint,
            api: astra_thin_client::ThinClient::new(&http.uri(), None).unwrap(),
            auth: Arc::new(tokio::sync::RwLock::new("test-only-token".into())),
            auth_provider: None,
            auth_owner: None,
            account_id: "account-test".into(),
            edge_agent_id: "edge-test".into(),
            edge_transport_id: Arc::new(tokio::sync::RwLock::new("transport-test".into())),
            workspace_id: None,
            materialization_id: "materialization-test".into(),
            journal_path: workspace.path().join("journal.json"),
            executor: Arc::new(ToolExecutor::new(workspace.path())),
            ask_user_request_tx: None,
            permission_policy: permission_owner.perm_manager.subscribe_permission_policy(),
            approval_request_tx: None,
        };
        let (handle, _) = connect_native_delivery(
            config,
            None,
            Instant::now() + Duration::from_secs(5),
            &CancellationToken::new(),
            CancellationToken::new(),
            mpsc::channel(1).0,
            Arc::new(std::sync::Mutex::new(Vec::new())),
            None,
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .expect("transport recovery must not require a currently installed provider");
        let requests = http.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let registration: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(
            registration["capabilities"]["provider_discovery"],
            json!([])
        );
        handle.shutdown().await;
        peer.abort();
        let _ = peer.await;
    }

    #[tokio::test]
    async fn authenticated_shared_ws_reaches_cli_native_entrypoint_and_withdraws_capacity() {
        use astra_server_types::edge_ws_protocol::{
            EdgeExecutionCeiling, EdgeServerMessage, ToolInvocationIdentity,
        };
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{header, method, path},
        };
        let http = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/agents/edge"))
            .and(header("X-Astra-Edge-Id", "ws-native-test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok":true})))
            .mount(&http)
            .await;
        let workspace = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let executable = runtime.path().join("codex");
        std::fs::write(&executable, b"provider").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&executable, permissions).unwrap();
        }
        let executor = Arc::new(ToolExecutor::new(workspace.path()));
        let (approval_tx, mut approval_rx) =
            tokio::sync::mpsc::channel::<chat_stream::ApprovalRequest>(1);
        let req = requirements(runtime.path());
        let discovery = Arc::new(snapshot(&req, workspace.path()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("ws://{}/edge/ws", listener.local_addr().unwrap());
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let root = workspace
            .path()
            .canonicalize()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let (dispatch_tx, dispatch_rx) = tokio::sync::oneshot::channel();
        let peer = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let auth: EdgeClientMessage =
                serde_json::from_slice(&ws.next().await.unwrap().unwrap().into_data()).unwrap();
            let EdgeClientMessage::Auth {
                capabilities: Some(capabilities),
                ..
            } = auth
            else {
                panic!("Auth required")
            };
            let provider_discovery = capabilities["provider_discovery"]
                .as_array()
                .expect("provider discovery must be an array");
            assert_eq!(provider_discovery.len(), 1);
            assert_eq!(
                provider_discovery[0]["tool_declarations"][0]["native_tool_name"],
                native_codex::TOOL_NAME
            );
            ws.send(Message::Text(
                serde_json::to_string(&EdgeServerMessage::AuthOk {
                    user_id: "account-test".into(),
                    edge_id: "ws-native-test".into(),
                    interaction_api_major: astra_server_types::AGENT_INTERACTION_API_MAJOR.into(),
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
            dispatch_rx.await.unwrap();
            let identity = ToolInvocationIdentity::new(
                "account-test",
                "session-test",
                "run-test",
                "chain-test",
                "call-test",
            )
            .unwrap();
            let unix = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;
            let request = EdgeServerMessage::ToolRequest {
                request_id: identity.storage_key(),
                identity: Box::new(identity.clone()),
                delivery_generation: 1,
                tool: native_codex::TOOL_NAME.into(),
                args: json!({"task":"No paid invocation", "anchor_run_id":"anchor-test"}),
                execution_ceiling: Some(Box::new(EdgeExecutionCeiling {
                    workspace_root: root,
                    workspace_id: None,
                    materialization_id: Some("materialization-test".into()),
                    execution_binding_generation: 1,
                    runtime_read_paths: req.read_paths,
                    workspace_write_allowed: false,
                    network_allowed: false,
                })),
                runtime_process_authorization: None,
                runtime_process_authorization_required: false,
                timeout_secs: 120,
                execution_deadline_unix_ms: Some(unix + 5000),
                execution_timeout_ms: Some(5000),
                command_timeout_cap_ms: Some(120_000),
            };
            ws.send(Message::Text(
                serde_json::to_string(&request).unwrap().into(),
            ))
            .await
            .unwrap();
            // The callback detects that the executable captured by the
            // published capability has been replaced. It must not dispatch
            // through a stale snapshot. The rejected result is delivered
            // before the owner withdraws, so the server has a truthful
            // not-executed fact and can rediscover on the next turn.
            loop {
                match ws.next().await {
                    Some(Ok(frame)) if frame.is_text() => {
                        let message: EdgeClientMessage =
                            serde_json::from_slice(&frame.into_data()).unwrap();
                        if matches!(message, EdgeClientMessage::Ping {}) {
                            continue;
                        }
                        let EdgeClientMessage::ToolResult {
                            identity: actual,
                            is_error,
                            tool_result_fields,
                            ..
                        } = message.clone()
                        else {
                            panic!(
                                "stale native capability returned an unexpected message: {message:?}"
                            )
                        };
                        assert_eq!(actual, identity);
                        assert!(is_error);
                        let fields = tool_result_fields.unwrap();
                        assert_eq!(fields["execution_fact"], "not_executed");
                        assert_eq!(fields["workspace_effect_settled"], true);
                        break;
                    }
                    Some(Ok(frame)) if frame.is_close() => {
                        panic!("stale native capability closed before returning ToolResult")
                    }
                    Some(Ok(_)) => {}
                    Some(Err(error)) => panic!(
                        "stale native capability transport failed before returning ToolResult: {error}"
                    ),
                    None => {
                        panic!("stale native capability peer ended before returning ToolResult")
                    }
                }
            }
            result_tx
                .send(())
                .expect("test observer must still be waiting for ToolResult");
            while let Some(frame) = ws.next().await {
                if frame.is_err() || frame.is_ok_and(|frame| frame.is_close()) {
                    break;
                }
            }
        });
        let mut permission_owner = SessionState {
            perm_manager: PermissionManager::with_project_mode(
                PermissionMode::Prompt,
                workspace.path(),
            ),
            ..SessionState::default()
        };
        permission_owner.set_session_id("session-test");
        let config = NativeDeliveryConfig {
            websocket_url: endpoint,
            api: astra_thin_client::ThinClient::new(&http.uri(), None).unwrap(),
            auth: Arc::new(tokio::sync::RwLock::new("test-only-token".into())),
            auth_provider: None,
            auth_owner: None,
            account_id: "account-test".into(),
            edge_agent_id: "edge-test".into(),
            edge_transport_id: Arc::new(tokio::sync::RwLock::new("transport-test".into())),
            workspace_id: None,
            materialization_id: "materialization-test".into(),
            journal_path: workspace.path().join("journal.json"),
            executor,
            ask_user_request_tx: None,
            permission_policy: permission_owner.perm_manager.subscribe_permission_policy(),
            approval_request_tx: Some(approval_tx),
        };
        let (handle, _) = connect_native_delivery(
            config,
            Some(vec![NativeProviderCandidate {
                snapshot: discovery,
                executable_identity: native_codex::native_executable_identity(&executable).unwrap(),
            }]),
            Instant::now() + Duration::from_secs(5),
            &CancellationToken::new(),
            CancellationToken::new(),
            mpsc::channel(1).0,
            Arc::new(std::sync::Mutex::new(Vec::new())),
            None,
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .unwrap();
        assert!(
            approval_rx.try_recv().is_err(),
            "installation/discovery cannot prompt"
        );
        // Replace the discovered artifact before dispatch. The provider path
        // is still present, but its capability identity is no longer the one
        // that was probed and published.
        std::fs::write(&executable, b"replacement-provider").unwrap();
        dispatch_tx.send(()).unwrap();
        let prompt = tokio::time::timeout(Duration::from_secs(5), approval_rx.recv())
            .await
            .unwrap()
            .unwrap();
        prompt
            .response_tx
            .send(chat_stream::ApprovalResponse::AllowOnce)
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), result_rx)
            .await
            .unwrap()
            .unwrap();
        handle.shutdown().await;
        tokio::time::timeout(Duration::from_secs(5), peer)
            .await
            .unwrap()
            .unwrap();
        let requests = http.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2);
        let first: Value = serde_json::from_slice(&requests[0].body).unwrap();
        let last: Value = serde_json::from_slice(&requests[1].body).unwrap();
        assert_eq!(
            first["capabilities"]["provider_discovery"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(last["capabilities"]["provider_discovery"], json!([]));
    }

    /// Keep one composition test between the production Edge owner and an
    /// external protocol process. Adapter-only tests cannot catch a routing
    /// or settlement break between those two owners.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "requires an explicit freshly built Astra invocation supervisor binary"]
    async fn authenticated_shared_ws_executes_native_provider_through_cli_entrypoint() {
        for scenario in ["released", "cancelled", "expired", "revoked", "replaced"] {
            native_entrypoint_with_workspace_contention(scenario).await;
        }
    }

    #[cfg(target_os = "linux")]
    async fn native_entrypoint_with_workspace_contention(scenario: &'static str) {
        use astra_server_types::edge_ws_protocol::{
            EdgeExecutionCeiling, EdgeServerMessage, ToolInvocationIdentity,
        };
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

        let http = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wiremock::matchers::path("/agents/edge"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
            .mount(&http)
            .await;

        let workspace = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let executable = runtime.path().join("codex");
        std::fs::write(
            &executable,
            r##"#!/usr/bin/env python3
import json, sys

def recv():
    line = sys.stdin.readline()
    if not line:
        raise SystemExit(0)
    return json.loads(line)

def emit(value):
    print(json.dumps(value), flush=True)

request = recv()
assert request["method"] == "initialize"
emit({"id": 1, "result": {"userAgent": "codex-cli harness", "codexHome": "/tmp/codex", "platformFamily": "unix", "platformOs": "linux"}})
assert recv()["method"] == "initialized"
request = recv()
assert request["id"] == 7 and request["method"] == "account/read"
emit({"id": 7, "result": {"account": None, "requiresOpenaiAuth": False}})
request = recv()
assert request["id"] == 8 and request["method"] == "config/read"
emit({"id": 8, "result": {"config": {"additional": {"mcp_servers": {}}}}})
request = recv()
assert request["method"] == "thread/start"
profile = request["params"]["permissions"]
emit({"id": 2, "result": {"thread": {"id": "edge-thread", "status": {"type": "idle"}}, "cwd": request["params"]["cwd"], "approvalPolicy": "never", "approvalsReviewer": "user", "activePermissionProfile": {"id": profile}, "sandbox": {"type": "readOnly", "networkAccess": False}}})
request = recv()
assert request["method"] == "turn/start"
emit({"id": 3, "result": {"turn": {"id": "edge-turn", "status": "inProgress"}}})
emit({"method": "item/agentMessage/delta", "params": {"threadId": "edge-thread", "turnId": "edge-turn", "delta": "NATIVE_EDGE_SUCCESS"}})
emit({"method": "turn/completed", "params": {"threadId": "edge-thread", "turn": {"id": "edge-turn", "status": "completed"}}})
for line in sys.stdin:
    pass
"##,
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&executable, permissions).unwrap();
        }

        let (mut consumer, mut permission_owner) = consumer(workspace.path(), runtime.path(), None);
        permission_owner
            .perm_manager
            .set_mode(PermissionMode::Bypass);
        // Use the same bounded discovery owner as production. The Linux
        // runtime declaration includes the system paths needed by a shebang;
        // a hand-written subset would make the revalidation test dishonest.
        let requirements = native_codex::runtime_requirements_for_executable(&executable)
            .expect("harness provider requirements");
        consumer.requirements = requirements.clone();
        consumer.snapshot = Arc::new(snapshot(&requirements, workspace.path()));
        let snapshot = consumer.snapshot.clone();
        consumer.expected_executable_identity =
            Some(native_codex::native_executable_identity(&executable).unwrap());
        let mut config = (*consumer.config).clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        config.websocket_url = format!("ws://{}/edge/ws", listener.local_addr().unwrap());
        config.api = astra_thin_client::ThinClient::new(&http.uri(), None).unwrap();

        let (dispatch_tx, dispatch_rx) = tokio::sync::oneshot::channel();
        let (result_tx, mut result_rx) = tokio::sync::oneshot::channel();
        let lease =
            astra_tools::workspace_observation::acquire_workspace_mutation_lease_with_options(
                workspace.path(),
                None,
                Duration::from_secs(1),
            )
            .await
            .expect("hold a competing workspace operation");
        let root = workspace
            .path()
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let peer = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let auth: EdgeClientMessage =
                serde_json::from_slice(&ws.next().await.unwrap().unwrap().into_data()).unwrap();
            assert!(matches!(auth, EdgeClientMessage::Auth { .. }));
            ws.send(Message::Text(
                serde_json::to_string(&EdgeServerMessage::AuthOk {
                    user_id: "account-test".into(),
                    edge_id: "ws-native-success".into(),
                    interaction_api_major: astra_server_types::AGENT_INTERACTION_API_MAJOR.into(),
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
            dispatch_rx.await.unwrap();

            let identity = ToolInvocationIdentity::new(
                "account-test",
                "session-test",
                "run-success",
                "chain-success",
                "call-success",
            )
            .unwrap();
            let request = EdgeServerMessage::ToolRequest {
                request_id: identity.storage_key(),
                identity: Box::new(identity.clone()),
                delivery_generation: 1,
                tool: native_codex::TOOL_NAME.into(),
                args: json!({"task": "Return the harness result", "anchor_run_id": "run-success"}),
                execution_ceiling: Some(Box::new(EdgeExecutionCeiling {
                    workspace_root: root,
                    workspace_id: None,
                    materialization_id: Some("materialization-test".into()),
                    execution_binding_generation: 1,
                    runtime_read_paths: requirements.read_paths.clone(),
                    workspace_write_allowed: false,
                    network_allowed: false,
                })),
                runtime_process_authorization: None,
                runtime_process_authorization_required: false,
                timeout_secs: 120,
                execution_deadline_unix_ms: Some(
                    (std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_millis() as u64)
                        + if scenario == "expired" { 500 } else { 15_000 },
                ),
                execution_timeout_ms: Some(15_000),
                command_timeout_cap_ms: Some(120_000),
            };
            ws.send(Message::Text(
                serde_json::to_string(&request).unwrap().into(),
            ))
            .await
            .unwrap();

            if scenario == "cancelled" {
                tokio::time::sleep(Duration::from_millis(100)).await;
                ws.send(Message::Text(
                    serde_json::to_string(&EdgeServerMessage::ToolCancel {
                        request_id: identity.storage_key(),
                        delivery_generation: 1,
                    })
                    .unwrap()
                    .into(),
                ))
                .await
                .unwrap();
            }

            loop {
                match ws.next().await {
                    Some(Ok(frame)) if frame.is_text() => {
                        let message: EdgeClientMessage =
                            serde_json::from_slice(&frame.into_data()).unwrap();
                        if matches!(message, EdgeClientMessage::Ping {}) {
                            continue;
                        }
                        let EdgeClientMessage::ToolResult {
                            identity: actual,
                            output,
                            is_error,
                            tool_result_fields,
                            ..
                        } = message
                        else {
                            panic!("expected native tool result");
                        };
                        assert_eq!(actual, identity);
                        if scenario != "released" {
                            assert!(is_error, "blocked native call must fail: {output}");
                            assert!(!output.contains("NATIVE_EDGE_SUCCESS"));
                            if scenario == "revoked" {
                                assert!(
                                    output.contains("denied") || output.contains("revoked"),
                                    "{output}"
                                );
                            }
                            if scenario == "replaced" {
                                assert!(output.contains("capability changed"), "{output}");
                            }
                            result_tx.send(()).unwrap();
                            break;
                        }
                        assert!(!is_error, "native entrypoint failed: {output}");
                        assert_eq!(output.trim(), "NATIVE_EDGE_SUCCESS");
                        let fields = tool_result_fields.expect("native evidence");
                        assert_eq!(
                            fields["native_collaborator"]["native_terminal"],
                            "completed"
                        );
                        assert_eq!(
                            fields["native_collaborator"]["transport_settled_after_terminal"],
                            true
                        );
                        result_tx.send(()).unwrap();
                        break;
                    }
                    Some(Ok(frame)) if frame.is_close() => {
                        panic!("Edge owner closed before result")
                    }
                    Some(Ok(_)) => {}
                    Some(Err(error)) => panic!("Edge peer failed: {error}"),
                    None => panic!("Edge peer ended before result"),
                }
            }
        });

        let provider = NativeProviderCandidate {
            snapshot,
            executable_identity: native_codex::native_executable_identity(&executable).unwrap(),
        };
        let (handle, _) = connect_native_delivery(
            config,
            Some(vec![provider]),
            Instant::now() + Duration::from_secs(15),
            &CancellationToken::new(),
            CancellationToken::new(),
            mpsc::channel(1).0,
            Arc::new(std::sync::Mutex::new(Vec::new())),
            None,
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .expect("native delivery should publish the tested provider");
        dispatch_tx.send(()).unwrap();
        if matches!(scenario, "released" | "revoked" | "replaced") {
            assert!(
                tokio::time::timeout(Duration::from_millis(150), &mut result_rx)
                    .await
                    .is_err(),
                "native execution must respect the competing workspace lease"
            );
            if scenario == "revoked" {
                permission_owner.perm_manager.set_mode(PermissionMode::Deny);
            }
            if scenario == "replaced" {
                std::fs::write(&executable, "#!/usr/bin/env python3\nraise RuntimeError('replaced executable must not run')\n").unwrap();
            }
            drop(lease);
        } else {
            // Keep the competing owner alive while cancellation/deadline is
            // settled; neither condition should require workspace release.
            tokio::time::timeout(Duration::from_secs(3), &mut result_rx)
                .await
                .expect("blocked invocation must settle promptly")
                .unwrap();
            drop(lease);
            let next =
                astra_tools::workspace_observation::acquire_workspace_mutation_lease_with_options(
                    workspace.path(),
                    None,
                    Duration::from_secs(1),
                )
                .await
                .expect("cancelled waiter must not leak workspace ownership");
            drop(next);
            handle.shutdown().await;
            peer.await.unwrap();
            return;
        }
        tokio::time::timeout(Duration::from_secs(10), &mut result_rx)
            .await
            .unwrap()
            .unwrap();
        handle.shutdown().await;
        tokio::time::timeout(Duration::from_secs(5), peer)
            .await
            .unwrap()
            .unwrap();
    }
}
