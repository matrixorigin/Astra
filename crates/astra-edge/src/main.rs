//! `astra-edge` — lightweight remote tool execution agent.
//!
//! Connects to an Astra server via WebSocket, authenticates, and executes
//! tool calls locally on the user's machine. Results are sent back over the
//! same WebSocket connection.
//!
//! ## Usage
//! ```bash
//! astra-edge --server-url https://astra.example.com --workspace-dir ~/projects/my-app
//! ```

mod evaluation_allocation;
mod evaluation_provider;
mod invocation_journal;
mod runtime_process_authorization;
mod token_manager;
mod token_renewal;

use astra_credentials::{CredentialStore, CredentialsFile};
use astra_runtime_env::{
    ExecutorBinding, PolicyIntent, RunBinding, RuntimeBinding, RuntimeEnvironmentAdvertisement,
    ToolRegistry, WorkspaceAuthority, WorkspaceBinding, WorkspaceSourceIdentity,
};
use astra_server_types::edge_ws_protocol::{
    EDGE_AUTH_TIMEOUT_SECS, EDGE_HEARTBEAT_INTERVAL_SECS, EdgeClientMessage, EdgeServerMessage,
};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore, mpsc};
use tokio::task::JoinSet;
use tokio_tungstenite::{
    Connector, MaybeTlsStream, WebSocketStream, client_async_tls_with_config, connect_async,
    tungstenite::Message, tungstenite::client::IntoClientRequest,
};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use invocation_journal::{DurableEdgeResult, EdgeInvocationJournal, JournalError, PrepareOutcome};

const MAX_CONCURRENT_TOOL_EXECUTIONS: usize = 128;
const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const GIT_STDOUT_LIMIT: usize = 2 * 1024 * 1024;
const GIT_STDERR_LIMIT: usize = 64 * 1024;
const MAX_CLEAN_PROOF_ENTRIES: usize = 65_536;
const MAX_CLEAN_PROOF_BYTES: u64 = 64 * 1024 * 1024;
const MAX_CLEAN_PROOF_DEPTH: usize = 1_024;
const MAX_EVALUATION_PATCH_BYTES: usize = 144 * 1024;
const MAX_EVALUATION_VERIFIER_OUTPUT_BYTES: usize = 48 * 1024;
const MAX_EDGE_FINALIZATION_MESSAGE_BYTES: usize = 240 * 1024;

#[derive(Clone)]
struct EdgeExecutionBudget {
    permits: Arc<Semaphore>,
}

impl EdgeExecutionBudget {
    fn new() -> Self {
        Self {
            permits: Arc::new(Semaphore::new(MAX_CONCURRENT_TOOL_EXECUTIONS)),
        }
    }

    fn try_acquire(&self) -> Option<OwnedSemaphorePermit> {
        self.permits.clone().try_acquire_owned().ok()
    }
}

/// Astra remote edge agent — execute tool calls locally for web sessions.
#[derive(Parser, Debug)]
#[command(name = "astra-edge", version, about)]
struct Args {
    /// Astra API/WebSocket base URL. Accepts http(s)://host[:port] or ws(s)://host[:port]/edge/ws.
    ///
    /// Defaults to ASTRA_SERVER_URL, then ASTRA_API_URL, then http://127.0.0.1:17001.
    #[arg(long)]
    server_url: Option<String>,

    /// Authentication token (JWT). When omitted, astra-edge reads the selected Astra CLI profile.
    #[arg(long, env = "ASTRA_TOKEN")]
    token: Option<String>,

    /// Astra CLI credentials profile to read when --token is omitted.
    #[arg(long)]
    profile: Option<String>,

    /// Local workspace directory for file operations
    #[arg(long, env = "ASTRA_WORKSPACE_DIR", default_value = ".")]
    workspace_dir: PathBuf,

    /// Edge agent identifier. Defaults to a stable id derived from hostname + canonical workspace.
    #[arg(long, env = "ASTRA_EDGE_ID")]
    edge_id: Option<String>,

    /// Dedicated evaluation provider configuration; startup failure never falls back.
    #[arg(long)]
    evaluation_config: Option<PathBuf>,

    /// Auto-reconnect on disconnect
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    reconnect: bool,
}

#[derive(Debug)]
struct EdgeConfig {
    server_url: String,
    /// Single owner of the token state machine: memory value, token file,
    /// startup fallback and persistence debt (see token_manager.rs).
    token_manager: Arc<token_manager::TokenManager>,
    workspace_dir: PathBuf,
    edge_id: String,
    /// Stable identity of this local checkout. It is persisted in the Edge
    /// local state directory so reconnects and agent-label changes cannot
    /// make one materialization look like a new checkout or dirty the repo.
    materialization_id: String,
    reconnect: bool,
    invocation_journal_root: Option<PathBuf>,
    evaluation_config: Option<PathBuf>,
    evaluation: Option<Arc<Mutex<evaluation_allocation::Allocations>>>,
}

#[derive(Debug)]
struct PermanentEdgeConnectionError(String);

impl std::fmt::Display for PermanentEdgeConnectionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for PermanentEdgeConnectionError {}

fn validate_interaction_api_major(actual: &str) -> Result<(), PermanentEdgeConnectionError> {
    if actual == astra_server_types::AGENT_INTERACTION_API_MAJOR {
        return Ok(());
    }
    Err(PermanentEdgeConnectionError(format!(
        "Incompatible Server interaction contract: expected {}, received {}",
        astra_server_types::AGENT_INTERACTION_API_MAJOR,
        actual,
    )))
}

fn is_permanent_connection_error(error: &(dyn std::error::Error + 'static)) -> bool {
    if error
        .downcast_ref::<PermanentEdgeConnectionError>()
        .is_some()
        || error.downcast_ref::<ProxyConfigError>().is_some()
    {
        return true;
    }
    matches!(
        error.downcast_ref::<tokio_tungstenite::tungstenite::Error>(),
        Some(tokio_tungstenite::tungstenite::Error::Http(response))
            if matches!(response.status().as_u16(), 401 | 403)
    )
}

struct CompletedEdgeInvocation {
    request_id: String,
    generation: u64,
    result: astra_tools::ToolResult,
    duration_ms: u64,
}

fn rejected_tool_message(
    request_id: String,
    identity: astra_turn_types::ToolInvocationIdentity,
    delivery_generation: u64,
    message: impl Into<String>,
) -> EdgeClientMessage {
    DurableEdgeResult::from_tool_result(astra_tools::ToolResult::error(message.into()), 0)
        .client_message(request_id, identity, delivery_generation)
}

fn valid_runtime_process_authorization(
    tool: &str,
    required: bool,
    context: Option<&astra_server_types::edge_ws_protocol::RuntimeProcessAuthorizationContext>,
) -> bool {
    required == context.is_some()
        && context.is_none_or(|context| {
            astra_server_types::edge_ws_protocol::runtime_process_authorization_applies_to_tool(
                tool,
            ) && !context.authorization.trim().is_empty()
        })
}

struct InFlightEdgeInvocation {
    generation: u64,
    cancel: CancellationToken,
}

#[derive(Default)]
struct EdgeInvocationTracker {
    in_flight: HashMap<String, InFlightEdgeInvocation>,
}

struct FinalizationCancellationGuard(Arc<std::sync::Mutex<HashMap<String, CancellationToken>>>);

impl Drop for FinalizationCancellationGuard {
    fn drop(&mut self) {
        let finalizations = self.0.lock().unwrap_or_else(|error| error.into_inner());
        for cancel in finalizations.values() {
            cancel.cancel();
        }
    }
}

impl EdgeInvocationTracker {
    fn begin(&mut self, request_id: &str, generation: u64) -> Result<CancellationToken, u64> {
        if let Some(active) = self.in_flight.get(request_id) {
            return Err(active.generation);
        }
        let cancel = CancellationToken::new();
        self.in_flight.insert(
            request_id.to_string(),
            InFlightEdgeInvocation {
                generation,
                cancel: cancel.clone(),
            },
        );
        Ok(cancel)
    }

    fn cancel_if_current(&self, request_id: &str, generation: u64) -> bool {
        let Some(active) = self.in_flight.get(request_id) else {
            return false;
        };
        if active.generation != generation {
            return false;
        }
        active.cancel.cancel();
        true
    }

    fn finish_if_current(&mut self, request_id: &str, generation: u64) -> bool {
        if self
            .in_flight
            .get(request_id)
            .is_none_or(|active| active.generation != generation)
        {
            return false;
        }
        self.in_flight.remove(request_id);
        true
    }

    fn cancel_all(self) {
        for active in self.in_flight.into_values() {
            active.cancel.cancel();
        }
    }
}

fn normalized_hostname() -> String {
    let hostname = hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| "unknown".into());
    hostname
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '.' {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

fn default_edge_id(workspace_dir: &Path) -> String {
    let workspace = canonical_workspace_dir(workspace_dir).unwrap_or_else(|_| {
        // Fall back to the non-canonical path for edge ID stability
        workspace_dir.to_path_buf()
    });
    let mut hasher = Sha256::new();
    hasher.update(workspace.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    let suffix = digest[..4]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("edge-{}-{suffix}", normalized_hostname())
}

fn default_server_url() -> String {
    std::env::var("ASTRA_SERVER_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            std::env::var("ASTRA_API_URL")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .unwrap_or_else(|| "http://127.0.0.1:17001".to_string())
}

fn edge_ws_url(server_url: &str) -> Result<String, String> {
    let trimmed = server_url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err("server URL must not be empty".to_string());
    }
    let with_ws_scheme = if let Some(rest) = trimmed.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        format!("ws://{rest}")
    } else if trimmed.starts_with("ws://") || trimmed.starts_with("wss://") {
        trimmed.to_string()
    } else if trimmed.contains("://") {
        return Err(format!(
            "unsupported server URL scheme in '{trimmed}'; use http(s):// or ws(s)://"
        ));
    } else {
        format!("ws://{trimmed}")
    };

    if let Ok(mut url) = reqwest::Url::parse(&with_ws_scheme) {
        if !matches!(url.scheme(), "ws" | "wss") {
            return Err(format!(
                "unsupported edge WebSocket URL scheme '{}'; use ws:// or wss://",
                url.scheme()
            ));
        }
        url.set_path(&normalized_edge_ws_path(url.path()));
        url.set_query(None);
        url.set_fragment(None);
        Ok(url.to_string())
    } else {
        Err(format!("invalid server URL '{server_url}'"))
    }
}

fn normalized_edge_ws_path(path: &str) -> String {
    let segments = path
        .trim_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();

    if segments.is_empty() {
        return "/edge/ws".to_string();
    }

    if let Some(index) = segments
        .windows(2)
        .position(|window| window == ["edge", "ws"])
    {
        return format!("/{}", segments[..index + 2].join("/"));
    }

    format!("/{}/edge/ws", segments.join("/"))
}

fn token_from_credentials(
    creds: &CredentialsFile,
    profile_override: Option<&str>,
) -> Result<(String, String), String> {
    let profile_name =
        CredentialStore::resolve_profile_name(profile_override, creds.current_profile.as_deref());
    let profile = creds
        .profiles
        .get(&profile_name)
        .ok_or_else(|| format!("no profile '{profile_name}', run `astra login` first"))?;
    let token = profile
        .access_token
        .clone()
        .ok_or_else(|| format!("profile '{profile_name}' is not logged in; run `astra login`"))?;
    Ok((profile_name, token))
}

fn resolve_token(args: &Args) -> Result<String, String> {
    if let Some(token) = args.token.as_ref().filter(|token| !token.trim().is_empty()) {
        return Ok(token.clone());
    }
    let creds = CredentialStore::new()
        .load()
        .map_err(|error| format!("failed to read Astra credentials: {error}"))?;
    let (profile_name, token) = token_from_credentials(&creds, args.profile.as_deref())?;
    tracing::info!(profile = %profile_name, "using Astra CLI profile token");
    Ok(token)
}

fn resolve_config(args: Args) -> Result<EdgeConfig, String> {
    let raw_server_url = args.server_url.clone().unwrap_or_else(default_server_url);
    let workspace_dir = canonical_workspace_dir(&args.workspace_dir)?;
    // Prefer a valid persisted moi-user-token-v1 (written by a prior renewal)
    // over the env/flag token; astra JWT flows are untouched.
    let token_file = token_renewal::resolve_token_file_path(&workspace_dir);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    // Pick whichever unexpired token expires LATER. A recreated Runner that
    // reuses the workspace volume injects a fresh env token while the file
    // still holds the previous (revoked-but-unexpired) one; preferring the
    // file unconditionally would leave the edge permanently rejected.
    let env_token = resolve_token(&args);
    let file_token = token_renewal::read_valid_file_token(&token_file, now);
    let mut fallback_token = None;
    let token = match (file_token, &env_token) {
        (Some(file_token), Ok(env)) => {
            // Generation order is (iat, exp); expiry seconds alone cannot
            // order double-renew siblings. Exact ties keep the file (with the
            // env token retained as the one-shot auth-failure fallback), and
            // the AuthOk heal-write converges the file to whatever actually
            // authenticates.
            let file_claims = token_renewal::parse_moi_token_claims(&file_token);
            let env_claims = token_renewal::parse_moi_token_claims(env);
            match (file_claims, env_claims) {
                (_, None) => {
                    // The explicit credential is NOT a moi-user-token (plain
                    // Astra token / profile identity): it wins absolutely. A
                    // leftover sandbox token file must never replace an
                    // explicitly chosen identity, and the two are different
                    // identity domains — no fallback between them either.
                    tracing::info!(
                        "using explicit non-MOI credential (persisted MOI token file ignored)"
                    );
                    env.clone()
                }
                (None, Some(_)) => {
                    // File token is not a parseable moi-user-token but the
                    // explicit credential is: prefer the explicit token.
                    tracing::info!("using env/flag edge token (persisted token file unparseable)");
                    env.clone()
                }
                (Some(fc), Some(ec)) if !token_renewal::same_moi_identity(&fc, &ec) => {
                    // Different identity (e.g. the workspace volume was reused
                    // by another user/tenant). Generation order is meaningless
                    // across identities: the explicit env token wins and the
                    // stale file token is ignored — never used, never a fallback.
                    tracing::info!(
                        "using explicit edge token (persisted token file has a different identity — ignored)"
                    );
                    env.clone()
                }
                (Some(fc), Some(ec)) => {
                    // Same identity: order by generation (iat, exp). Expiry
                    // seconds alone cannot order double-renew siblings; exact
                    // ties keep the file (env retained as one-shot fallback) and
                    // the AuthOk heal-write converges the file to whatever
                    // actually authenticates.
                    let file_gen = (fc.iat, fc.exp);
                    let env_gen = (ec.iat, ec.exp);
                    if env_gen > file_gen {
                        tracing::info!(
                            "using env/flag edge token (newer than persisted token file)"
                        );
                        fallback_token = Some(file_token);
                        env.clone()
                    } else {
                        tracing::info!(
                            path = %token_file.display(),
                            "using persisted edge token from token file"
                        );
                        if env != &file_token {
                            fallback_token = Some(env.clone());
                        }
                        file_token
                    }
                }
            }
        }
        (Some(file_token), Err(_)) => {
            tracing::info!(
                path = %token_file.display(),
                "using persisted edge token from token file"
            );
            file_token
        }
        (None, _) => env_token?,
    };
    let edge_id = args
        .edge_id
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| default_edge_id(&workspace_dir));
    let materialization_id = load_or_create_materialization_id(&workspace_dir)?;
    Ok(EdgeConfig {
        server_url: edge_ws_url(&raw_server_url)?,
        token_manager: token_manager::TokenManager::new(token, fallback_token, token_file),
        workspace_dir,
        edge_id,
        materialization_id,
        reconnect: args.reconnect,
        invocation_journal_root: astra_runtime_env::local_state_root_override(),
        evaluation_config: args.evaluation_config,
        evaluation: None,
    })
}

fn canonical_workspace_dir(workspace_dir: &Path) -> Result<PathBuf, String> {
    workspace_dir.canonicalize().map_err(|error| {
        format!(
            "failed to canonicalize workspace directory '{}': {error}",
            workspace_dir.display()
        )
    })
}

use astra_runtime_env::load_or_create_materialization_id;
#[cfg(test)]
use astra_runtime_env::{
    load_or_create_materialization_id_in_roots, materialization_id_path_in_state,
};

fn edge_invocation_journal_path_in_root(
    edge_id: &str,
    workspace_dir: &Path,
    state_root: Option<PathBuf>,
) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(edge_id.as_bytes());
    hasher.update([0]);
    hasher.update(workspace_dir.to_string_lossy().as_bytes());
    let key = format!("{:x}", hasher.finalize());
    let base = state_root.unwrap_or_else(astra_core::local_state::local_state_root);
    base.join("edge-invocations").join(format!("{key}.json"))
}

/// Build the runtime-environment capability advertisement for this edge.
///
/// Callers must pass an already-canonical `workspace` path; `run_edge_agent`
/// enforces this via [`canonical_workspace_dir`] before calling here.
fn edge_runtime_environment_capabilities(edge_id: &str, workspace: &Path) -> Value {
    let registry = ToolRegistry::builtins();
    let workspace_path = workspace;
    let workspace = workspace_path.to_string_lossy().to_string();
    let binding = RunBinding::resolve(
        WorkspaceBinding::edge_workspace(workspace, WorkspaceAuthority::ReadWrite),
        ExecutorBinding::edge_agent(edge_id.to_string()),
        RuntimeBinding::host_process(format!("edge-host:{edge_id}")),
        PolicyIntent::local_developer(),
        &registry,
    );

    let mut runtime_advertisement = RuntimeEnvironmentAdvertisement::new(binding);
    runtime_advertisement.workspace_source = workspace_source_identity(workspace_path);
    let mut advertisement = serde_json::to_value(runtime_advertisement)
        .expect("runtime environment advertisement serializes");
    advertisement["protocol_capabilities"] = serde_json::json!({});
    advertisement["protocol_capabilities"]
        [astra_server_types::edge_ws_protocol::RUNTIME_PROCESS_AUTHORIZATION_V1_CAPABILITY] =
        Value::Bool(true);
    advertisement
}

const EVALUATION_TOOLS: &[&str] = &[
    "bash",
    "read_file",
    "write_file",
    "delete_file",
    "str_replace",
    "multi_edit",
];

fn dedicated_runtime_environment_capabilities(
    edge_id: &str,
    workspace: &Path,
    confinement: &astra_runtime_env::WorkspaceConfinementContract,
) -> Value {
    let mut value = edge_runtime_environment_capabilities(edge_id, workspace);
    value["workspace_confinement"] =
        serde_json::to_value(confinement).expect("workspace confinement serializes");
    value["protocol_capabilities"] = serde_json::json!({});
    if let Some(names) = value["binding"]["tool_surface"]["tool_names"].as_array_mut() {
        names.retain(|name| {
            name.as_str()
                .is_some_and(|name| EVALUATION_TOOLS.contains(&name))
        });
    }
    // Do not set workspace_confinement until runtime admission and durable
    // materialization/tool/finalization receipts consume the complete evidence.
    value
}

fn workspace_source_identity(workspace: &Path) -> Option<WorkspaceSourceIdentity> {
    let commit = git_object_id(workspace, "HEAD^{commit}")?;
    let tree = git_object_id(workspace, "HEAD^{tree}")?;
    let clean = workspace_matches_git_tree(workspace).unwrap_or(false);
    Some(WorkspaceSourceIdentity {
        commit,
        tree,
        clean,
    })
}

fn git_object_id(workspace: &Path, revision: &str) -> Option<String> {
    let output = git_command(workspace, &["rev-parse", "--verify", revision]).ok()?;
    if !output.status.success() {
        return None;
    }
    let object_id = String::from_utf8(output.stdout)
        .ok()?
        .trim()
        .to_ascii_lowercase();
    if matches!(object_id.len(), 40 | 64) && object_id.chars().all(|ch| ch.is_ascii_hexdigit()) {
        Some(object_id)
    } else {
        None
    }
}

fn valid_git_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.chars().all(|ch| ch.is_ascii_hexdigit())
}

#[derive(Debug, Clone)]
struct ExpectedGitTreeEntry {
    object_id: String,
    executable: bool,
    symlink: bool,
}

fn workspace_matches_git_tree(workspace: &Path) -> Option<bool> {
    workspace_matches_git_tree_with_cancel(workspace, None)
}

fn workspace_matches_git_tree_with_cancel(
    workspace: &Path,
    cancel: Option<&CancellationToken>,
) -> Option<bool> {
    let tree = git_command_with_cancel(
        workspace,
        &["ls-tree", "-r", "-z", "--full-tree", "HEAD"],
        cancel,
    )
    .ok()?;
    if !tree.status.success() {
        return Some(false);
    }
    let mut expected = HashMap::new();
    for entry in tree
        .stdout
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let tab = entry.iter().position(|byte| *byte == b'\t')?;
        let metadata = &entry[..tab];
        let path = &entry[tab + 1..];
        let mut fields = metadata.split(|byte| *byte == b' ');
        let mode = fields.next()?;
        let kind = fields.next()?;
        let object_id = std::str::from_utf8(fields.next()?)
            .ok()?
            .to_ascii_lowercase();
        if fields.next().is_some() || kind != b"blob" || !valid_git_object_id(&object_id) {
            return Some(false);
        }
        let path = std::str::from_utf8(path).ok()?;
        let path = PathBuf::from(path);
        if path.is_absolute()
            || path
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return Some(false);
        }
        let (executable, symlink) = match mode {
            b"100644" => (false, false),
            b"100755" => (true, false),
            b"120000" => (false, true),
            // Gitlinks and unusual modes require a provider-specific checkout
            // proof. Retain the workspace rather than guessing that it is safe
            // to delete.
            _ => return Some(false),
        };
        if expected
            .insert(
                path,
                ExpectedGitTreeEntry {
                    object_id,
                    executable,
                    symlink,
                },
            )
            .is_some()
        {
            return Some(false);
        }
        if expected.len() > MAX_CLEAN_PROOF_ENTRIES {
            return Some(false);
        }
    }
    if !git_index_matches_head_with_cancel(workspace, cancel)? {
        return Some(false);
    }
    let mut proof = WorkspaceTreeProof {
        expected: &expected,
        seen: HashSet::new(),
        content_bytes: 0,
        visited_entries: 0,
        cancel,
    };
    if !proof.walk(workspace, workspace, 0)? {
        return Some(false);
    }
    Some(proof.seen.len() == expected.len())
}

fn git_index_matches_head_with_cancel(
    workspace: &Path,
    cancel: Option<&CancellationToken>,
) -> Option<bool> {
    let flags = git_command_with_cancel(workspace, &["ls-files", "--debug"], cancel).ok()?;
    if !flags.status.success() {
        return Some(false);
    }
    let flags = String::from_utf8(flags.stdout).ok()?;
    if flags
        .lines()
        .filter_map(|line| line.trim().strip_prefix("flags:"))
        .any(|value| value.trim() != "0")
    {
        return Some(false);
    }
    let cached = git_command_with_cancel(
        workspace,
        &[
            "diff-index",
            "--cached",
            "--quiet",
            "--no-ext-diff",
            "HEAD",
            "--",
        ],
        cancel,
    )
    .ok()?;
    Some(cached.status.success())
}

struct WorkspaceTreeProof<'a> {
    expected: &'a HashMap<PathBuf, ExpectedGitTreeEntry>,
    seen: HashSet<PathBuf>,
    content_bytes: u64,
    visited_entries: usize,
    cancel: Option<&'a CancellationToken>,
}

impl WorkspaceTreeProof<'_> {
    fn walk(&mut self, root: &Path, directory: &Path, depth: usize) -> Option<bool> {
        if self.cancel.is_some_and(CancellationToken::is_cancelled) {
            return None;
        }
        if depth > MAX_CLEAN_PROOF_DEPTH {
            return Some(false);
        }
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(directory).ok()? {
            let entry = entry.ok()?;
            if entries.len() >= MAX_CLEAN_PROOF_ENTRIES {
                return Some(false);
            }
            entries.push(entry);
        }
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            self.visited_entries = self.visited_entries.checked_add(1)?;
            if self.visited_entries > MAX_CLEAN_PROOF_ENTRIES {
                return Some(false);
            }
            let path = entry.path();
            if directory == root && path.file_name() == Some(std::ffi::OsStr::new(".git")) {
                continue;
            }
            let relative = path.strip_prefix(root).ok()?.to_path_buf();
            let metadata = std::fs::symlink_metadata(&path).ok()?;
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                if !self.walk(root, &path, depth + 1)? {
                    return Some(false);
                }
                continue;
            }
            let Some(expected_entry) = self.expected.get(&relative) else {
                return Some(false);
            };
            if !git_tree_entry_matches(&path, &metadata, expected_entry, &mut self.content_bytes)? {
                return Some(false);
            }
            self.seen.insert(relative);
            if self.seen.len() > MAX_CLEAN_PROOF_ENTRIES {
                return Some(false);
            }
        }
        Some(true)
    }
}

fn git_tree_entry_matches(
    path: &Path,
    metadata: &std::fs::Metadata,
    expected: &ExpectedGitTreeEntry,
    content_bytes: &mut u64,
) -> Option<bool> {
    if expected.symlink != metadata.file_type().is_symlink() {
        return Some(false);
    }
    if expected.symlink {
        let target = std::fs::read_link(path).ok()?;
        let bytes = symlink_target_bytes(&target);
        *content_bytes = content_bytes.saturating_add(bytes.len() as u64);
        if *content_bytes > MAX_CLEAN_PROOF_BYTES {
            return Some(false);
        }
        return Some(git_blob_hash(&bytes, expected.object_id.len()) == expected.object_id);
    }
    if !metadata.is_file() {
        return Some(false);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 != 0 && !expected.executable
            || metadata.permissions().mode() & 0o111 == 0 && expected.executable
        {
            return Some(false);
        }
    }
    let expected_hash = expected.object_id.len();
    let file = std::fs::File::open(path).ok()?;
    let length = metadata.len();
    let remaining = MAX_CLEAN_PROOF_BYTES.saturating_sub(*content_bytes);
    if length > remaining {
        return Some(false);
    }
    let mut bytes = Vec::with_capacity(length.min(1024 * 1024) as usize);
    file.take(remaining.saturating_add(1))
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 != length {
        return Some(false);
    }
    *content_bytes = content_bytes.checked_add(bytes.len() as u64)?;
    let after = std::fs::symlink_metadata(path).ok()?;
    if after.len() != length {
        return Some(false);
    }
    Some(git_blob_hash(&bytes, expected_hash) == expected.object_id)
}

fn symlink_target_bytes(target: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        target.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    target.to_string_lossy().as_bytes().to_vec()
}

fn git_blob_hash(bytes: &[u8], object_id_len: usize) -> String {
    let header = format!("blob {}\0", bytes.len());
    if object_id_len == 40 {
        let mut hasher = sha1::Sha1::new();
        sha1::Digest::update(&mut hasher, header.as_bytes());
        sha1::Digest::update(&mut hasher, bytes);
        format!("{:x}", sha1::Digest::finalize(hasher))
    } else {
        let mut hasher = Sha256::new();
        hasher.update(header.as_bytes());
        hasher.update(bytes);
        format!("{:x}", hasher.finalize())
    }
}

fn safe_workspace_component(value: &str) -> Option<String> {
    let sanitized = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>();
    (!sanitized.is_empty() && sanitized != "." && sanitized != ".." && sanitized.len() <= 160)
        .then_some(sanitized)
}

fn evaluation_workspace_path(
    base_workspace: &Path,
    materialization_id: &str,
    workspace_key: &str,
) -> Result<PathBuf, String> {
    let materialization = safe_workspace_component(materialization_id)
        .ok_or_else(|| "edge materialization identity is invalid".to_string())?;
    let key = safe_workspace_component(workspace_key)
        .ok_or_else(|| "evaluation workspace key is invalid".to_string())?;
    let parent = base_workspace
        .parent()
        .ok_or_else(|| "edge workspace has no parent directory".to_string())?;
    Ok(parent.join(format!(".astra-evaluation-{materialization}-{key}")))
}

fn git_command(workspace: &Path, args: &[&str]) -> Result<std::process::Output, String> {
    git_command_with_cancel(workspace, args, None)
}

fn git_command_with_cancel(
    workspace: &Path,
    args: &[&str],
    cancel: Option<&CancellationToken>,
) -> Result<std::process::Output, String> {
    let mut command = astra_tools::workspace_observation::hardened_git_command(workspace)
        .ok_or_else(|| "trusted Git executable is unavailable".to_string())?;
    command.args(args);
    run_git_command_with_cancel(command, &format!("git {}", args.join(" ")), cancel)
}

fn run_git_command(command: Command, operation: &str) -> Result<std::process::Output, String> {
    run_git_command_with_cancel(command, operation, None)
}

fn run_git_command_with_cancel(
    mut command: Command,
    operation: &str,
    cancel: Option<&CancellationToken>,
) -> Result<std::process::Output, String> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| format!("failed to execute {operation}: {error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| format!("{operation} did not provide stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| format!("{operation} did not provide stderr"))?;
    let (stdout_tx, stdout_rx) = std::sync::mpsc::channel();
    let (stderr_tx, stderr_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::with_capacity(GIT_STDOUT_LIMIT.min(64 * 1024));
        let result = stdout
            .take(GIT_STDOUT_LIMIT.saturating_add(1) as u64)
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        let _ = stdout_tx.send(result);
    });
    std::thread::spawn(move || {
        let mut bytes = Vec::with_capacity(GIT_STDERR_LIMIT.min(16 * 1024));
        let result = stderr
            .take(GIT_STDERR_LIMIT.saturating_add(1) as u64)
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        let _ = stderr_tx.send(result);
    });
    let deadline = Instant::now() + GIT_COMMAND_TIMEOUT;
    let mut stdout_result = None;
    let mut stderr_result = None;
    let status = loop {
        if stdout_result.is_none() {
            match stdout_rx.try_recv() {
                Ok(Ok(bytes)) if bytes.len() > GIT_STDOUT_LIMIT => {
                    terminate_git_child(&mut child);
                    return Err(format!("{operation} produced too much stdout"));
                }
                Ok(Ok(bytes)) => stdout_result = Some(bytes),
                Ok(Err(error)) => {
                    terminate_git_child(&mut child);
                    return Err(format!("failed to read {operation} stdout: {error}"));
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    terminate_git_child(&mut child);
                    return Err(format!("{operation} stdout reader stopped"));
                }
            }
        }
        if stderr_result.is_none() {
            match stderr_rx.try_recv() {
                Ok(Ok(bytes)) if bytes.len() > GIT_STDERR_LIMIT => {
                    terminate_git_child(&mut child);
                    return Err(format!("{operation} produced too much stderr"));
                }
                Ok(Ok(bytes)) => stderr_result = Some(bytes),
                Ok(Err(error)) => {
                    terminate_git_child(&mut child);
                    return Err(format!("failed to read {operation} stderr: {error}"));
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    terminate_git_child(&mut child);
                    return Err(format!("{operation} stderr reader stopped"));
                }
            }
        }
        if cancel.is_some_and(CancellationToken::is_cancelled) {
            terminate_git_child(&mut child);
            return Err(format!("{operation} cancelled"));
        }
        match child.try_wait() {
            Ok(Some(exit)) => break exit,
            Ok(None) if Instant::now() >= deadline => {
                terminate_git_child(&mut child);
                return Err(format!(
                    "{operation} timed out after {}s",
                    GIT_COMMAND_TIMEOUT.as_secs()
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(error) => {
                terminate_git_child(&mut child);
                return Err(format!("failed to wait for {operation}: {error}"));
            }
        }
    };
    let stdout = match stdout_result {
        Some(bytes) => bytes,
        None => match stdout_rx.recv_timeout(Duration::from_millis(250)) {
            Ok(Ok(bytes)) if bytes.len() <= GIT_STDOUT_LIMIT => bytes,
            Ok(Ok(_)) => return Err(format!("{operation} produced too much stdout")),
            Ok(Err(error)) => return Err(format!("failed to read {operation} stdout: {error}")),
            Err(_) => {
                terminate_git_child(&mut child);
                return Err(format!(
                    "failed to collect {operation} stdout before the reader deadline"
                ));
            }
        },
    };
    let stderr = match stderr_result {
        Some(bytes) => bytes,
        None => match stderr_rx.recv_timeout(Duration::from_millis(250)) {
            Ok(Ok(bytes)) if bytes.len() <= GIT_STDERR_LIMIT => bytes,
            Ok(Ok(_)) => return Err(format!("{operation} produced too much stderr")),
            Ok(Err(error)) => return Err(format!("failed to read {operation} stderr: {error}")),
            Err(_) => {
                terminate_git_child(&mut child);
                return Err(format!(
                    "failed to collect {operation} stderr before the reader deadline"
                ));
            }
        },
    };
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

fn terminate_git_child(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let pid = child.id();
        if pid <= i32::MAX as u32 {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn git_failure(output: &std::process::Output, operation: &str) -> String {
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if detail.is_empty() {
        format!("git {operation} failed")
    } else {
        format!("git {operation} failed: {detail}")
    }
}

fn prepare_evaluation_workspace(
    base_workspace: &Path,
    materialization_id: &str,
    workspace_key: &str,
    source_commit: &str,
) -> Result<WorkspaceSourceIdentity, String> {
    if !valid_git_object_id(source_commit) {
        return Err("source_commit must be a full Git object id".to_string());
    }
    let path = evaluation_workspace_path(base_workspace, materialization_id, workspace_key)?;
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err("evaluation workspace path is an existing symlink".into());
        }
        Ok(metadata) if !metadata.file_type().is_dir() => {
            return Err("evaluation workspace path is an existing non-directory".into());
        }
        Ok(_) => return verify_evaluation_workspace(&path, source_commit),
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            return Err(format!("cannot inspect evaluation workspace path: {error}"));
        }
        Err(_) => {}
    }

    // Clone into a private sibling and atomically publish it. Two concurrent
    // starts for the same trial may both prepare, but only one can win the
    // rename; a losing attempt cleans only its own staging directory.
    let staging = path.with_file_name(format!(
        "{}.staging-{}-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("evaluation-workspace"),
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let mut clone_command =
        astra_tools::workspace_observation::hardened_git_command(base_workspace)
            .ok_or_else(|| "trusted Git executable is unavailable".to_string())?;
    clone_command
        .args(["clone", "--no-local", "--no-checkout"])
        .arg(base_workspace)
        .arg(&staging);
    let clone = match run_git_command(clone_command, "git clone") {
        Ok(output) => output,
        Err(error) => {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(error);
        }
    };
    if !clone.status.success() {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(git_failure(&clone, "clone"));
    }
    if let Err(error) = sanitize_evaluation_git_config(&staging) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error);
    }
    let checkout = match git_command(
        &staging,
        &["checkout", "--detach", "--force", source_commit],
    ) {
        Ok(output) => output,
        Err(error) => {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(error);
        }
    };
    if !checkout.status.success() {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(git_failure(&checkout, "checkout"));
    }
    let source = match verify_evaluation_workspace(&staging, source_commit) {
        Ok(source) => source,
        Err(error) => {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(error);
        }
    };
    match std::fs::rename(&staging, &path) {
        Ok(()) => Ok(source),
        Err(error) if path.exists() => {
            let _ = std::fs::remove_dir_all(&staging);
            if std::fs::symlink_metadata(&path)
                .map(|metadata| metadata.file_type().is_symlink())
                .unwrap_or(false)
            {
                return Err("evaluation workspace publish lost a race to a symlinked path".into());
            }
            verify_evaluation_workspace(&path, source_commit).map_err(|publish_error| {
                format!(
                    "evaluation workspace publish lost a concurrent race ({error}) and the winner failed verification: {publish_error}"
                )
            })
        }
        Err(error) => {
            let _ = std::fs::remove_dir_all(&staging);
            Err(format!("failed to publish evaluation workspace: {error}"))
        }
    }
}

fn sanitize_evaluation_git_config(workspace: &Path) -> Result<(), String> {
    let worktree_config = workspace.join(".git").join("config.worktree");
    match std::fs::symlink_metadata(&worktree_config) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err("evaluation workspace has an unsafe worktree config".into());
        }
        Ok(_) => std::fs::remove_file(&worktree_config)
            .map_err(|error| format!("failed to remove worktree Git config: {error}"))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!("cannot inspect worktree Git config: {error}"));
        }
    }

    // Remove include directives first, so values from an external file are
    // never mistaken for entries that this private clone can safely edit.
    for pattern in [r"^include", r"^filter\.", r"^core\.worktree$"] {
        let listed = git_command(
            workspace,
            &["config", "--local", "--name-only", "--get-regexp", pattern],
        )?;
        if !listed.status.success() {
            if listed.status.code() == Some(1) {
                continue;
            }
            return Err(git_failure(&listed, "config sanitization"));
        }
        for key in String::from_utf8(listed.stdout)
            .map_err(|error| format!("Git config key output is not UTF-8: {error}"))?
            .lines()
        {
            if key.is_empty() {
                return Err("Git config sanitization returned an empty key".into());
            }
            let output = git_command(workspace, &["config", "--local", "--unset-all", key])?;
            if !output.status.success() && output.status.code() != Some(5) {
                return Err(git_failure(&output, "config sanitization"));
            }
        }
    }
    Ok(())
}

fn verify_evaluation_workspace(
    workspace: &Path,
    frozen_source_commit: &str,
) -> Result<WorkspaceSourceIdentity, String> {
    if !independent_git_checkout(workspace) {
        return Err("evaluation workspace is not an independent Git checkout".into());
    }
    let source = workspace_source_identity(workspace)
        .ok_or_else(|| "evaluation workspace is not a Git checkout".to_string())?;
    if !source.clean || !source.commit.eq_ignore_ascii_case(frozen_source_commit) {
        return Err("evaluation workspace does not match the frozen source".into());
    }
    Ok(source)
}

fn release_evaluation_workspace(
    base_workspace: &Path,
    workspace: &Path,
    frozen_source_commit: &str,
) -> Result<(), String> {
    if std::fs::symlink_metadata(workspace)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err("refusing to release a symlinked evaluation workspace".into());
    }
    let base = base_workspace
        .canonicalize()
        .map_err(|error| format!("edge base workspace is unavailable: {error}"))?;
    let path = workspace
        .canonicalize()
        .map_err(|error| format!("evaluation workspace is unavailable: {error}"))?;
    let parent_matches = path.parent() == base.parent();
    let managed_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(".astra-evaluation-"));
    if !parent_matches || !managed_name || path == base {
        return Err("refusing to release a workspace outside the Edge evaluation area".into());
    }
    if !independent_git_checkout(&path) {
        return Err("evaluation workspace is not an independent Git checkout".into());
    }
    let source = workspace_source_identity(&path)
        .ok_or_else(|| "evaluation workspace is not a Git checkout".to_string())?;
    if !source.clean {
        return Err("evaluation workspace contains changes; preserving it for evidence".into());
    }
    if !source.commit.eq_ignore_ascii_case(frozen_source_commit) {
        return Err(
            "evaluation workspace has a committed trial result; preserving it for evidence".into(),
        );
    }
    std::fs::remove_dir_all(&path)
        .map_err(|error| format!("failed to remove evaluation workspace: {error}"))
}

#[derive(Debug)]
struct EvaluationWorkspaceFinalization {
    workspace_dir: String,
    source_commit: String,
    source_tree: String,
    base_revision: String,
    result_revision: String,
    patch: String,
    verifier_exit_code: Option<i32>,
    verifier_output: String,
    namespace_active: bool,
    scope_settled: bool,
    timed_out: bool,
    error: Option<String>,
}

fn copy_evaluation_snapshot(
    source: &Path,
    destination: &Path,
    cancel: &CancellationToken,
) -> Result<(), String> {
    fn copy_dir(
        root: &Path,
        directory: &Path,
        destination: &Path,
        entries: &mut usize,
        bytes: &mut u64,
        cancel: &CancellationToken,
        depth: usize,
    ) -> Result<(), String> {
        if cancel.is_cancelled() {
            return Err("evaluation finalization cancelled".into());
        }
        if depth > MAX_CLEAN_PROOF_DEPTH {
            return Err("evaluation workspace nesting is too deep".into());
        }
        let mut children = Vec::new();
        for child in std::fs::read_dir(directory)
            .map_err(|error| format!("cannot read evaluation workspace: {error}"))?
        {
            if cancel.is_cancelled() {
                return Err("evaluation finalization cancelled".into());
            }
            if children.len() >= MAX_CLEAN_PROOF_ENTRIES {
                return Err("evaluation workspace directory contains too many entries".into());
            }
            children.push(
                child.map_err(|error| format!("cannot enumerate evaluation workspace: {error}"))?,
            );
        }
        children.sort_by_key(std::fs::DirEntry::file_name);
        for child in children {
            if directory == root && child.file_name() == ".git" {
                continue;
            }
            *entries = entries.saturating_add(1);
            if *entries > MAX_CLEAN_PROOF_ENTRIES {
                return Err("evaluation workspace contains too many entries".into());
            }
            let source_path = child.path();
            let relative = source_path
                .strip_prefix(root)
                .map_err(|_| "evaluation workspace entry escaped its root")?;
            if relative
                .components()
                .any(|component| component.as_os_str() == ".git")
            {
                return Err("nested Git metadata is unsupported in evaluation workspaces".into());
            }
            let target = destination.join(relative);
            let metadata = std::fs::symlink_metadata(&source_path)
                .map_err(|error| format!("cannot inspect evaluation workspace entry: {error}"))?;
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                std::fs::create_dir_all(&target)
                    .map_err(|error| format!("cannot create replay directory: {error}"))?;
                copy_dir(
                    root,
                    &source_path,
                    destination,
                    entries,
                    bytes,
                    cancel,
                    depth + 1,
                )?;
            } else if metadata.file_type().is_symlink() {
                return Err("symlinks are unsupported in evaluation workspaces".into());
            } else if metadata.is_file() {
                *bytes = bytes.saturating_add(metadata.len());
                if *bytes > MAX_CLEAN_PROOF_BYTES {
                    return Err("evaluation workspace snapshot is too large".into());
                }
                let mut options = std::fs::OpenOptions::new();
                options.read(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.custom_flags(nix::libc::O_NOFOLLOW);
                }
                let input = options
                    .open(&source_path)
                    .map_err(|error| format!("cannot open evaluation workspace file: {error}"))?;
                let mut output = std::fs::File::create(&target)
                    .map_err(|error| format!("cannot create replay file: {error}"))?;
                let remaining =
                    MAX_CLEAN_PROOF_BYTES.saturating_sub(bytes.saturating_sub(metadata.len()));
                let copied =
                    std::io::copy(&mut input.take(remaining.saturating_add(1)), &mut output)
                        .map_err(|error| {
                            format!("cannot copy evaluation workspace file: {error}")
                        })?;
                if copied != metadata.len() || copied > remaining {
                    return Err(
                        "evaluation workspace changed or exceeded its snapshot limit while copying"
                            .into(),
                    );
                }
                std::fs::set_permissions(&target, metadata.permissions())
                    .map_err(|error| format!("cannot preserve replay permissions: {error}"))?;
            } else {
                return Err("evaluation workspace contains an unsupported entry type".into());
            }
        }
        Ok(())
    }

    for entry in std::fs::read_dir(destination)
        .map_err(|error| format!("cannot read replay workspace: {error}"))?
    {
        let path = entry
            .map_err(|error| format!("cannot enumerate replay workspace: {error}"))?
            .path();
        if path.file_name() == Some(std::ffi::OsStr::new(".git")) {
            continue;
        }
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|error| format!("cannot inspect replay entry: {error}"))?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        }
        .map_err(|error| format!("cannot clear replay workspace: {error}"))?;
    }
    copy_dir(source, source, destination, &mut 0, &mut 0, cancel, 0)
}

// Dedicated dispatch cannot reach runtime process authorization or an ordinary shell.
struct EvaluationToolCall<'a> {
    identity: &'a astra_turn_types::ToolInvocationIdentity,
    tool: &'a str,
    args: &'a Value,
    process_authorization: bool,
    allocation: &'a astra_runtime_env::EvaluationAllocationReceipt,
}

fn valid_evaluation_allocation_request(
    dedicated: bool,
    allocation: Option<&astra_runtime_env::EvaluationAllocationReceipt>,
    identity: &astra_turn_types::ToolInvocationIdentity,
    workspace: Option<&Path>,
) -> bool {
    match (dedicated, allocation) {
        (false, None) => true,
        (true, Some(allocation)) => {
            allocation.validate().is_ok()
                && allocation.owner_user_id == identity.user_id
                && allocation.session_id == identity.session_id
                && allocation.run_id == identity.run_id
                && workspace == Some(Path::new(&allocation.workspace_dir))
        }
        _ => false,
    }
}

async fn execute_evaluation_tool(
    allocations: Arc<Mutex<evaluation_allocation::Allocations>>,
    path: Option<&Path>,
    call: EvaluationToolCall<'_>,
    timeout_secs: u64,
    cancel: &CancellationToken,
) -> astra_tools::ToolResult {
    let EvaluationToolCall {
        identity,
        tool,
        args,
        process_authorization,
        allocation,
    } = call;
    let mut allocations = tokio::select! {
        guard = allocations.lock() => guard,
        _ = cancel.cancelled() => return astra_tools::ToolResult::error("allocation admission cancelled".into()),
    };
    let admission = (|| {
        let path = path.ok_or("dedicated tools require an allocated workspace")?;
        allocations.validate(allocation)?;
        let executor = allocations.executor(path, identity)?;
        if process_authorization || !EVALUATION_TOOLS.contains(&tool) {
            return Err(
                "tool or runtime process authorization is unsupported by dedicated evaluation"
                    .into(),
            );
        }
        Ok::<_, String>((path, executor, allocations.receipt(path)?))
    })();
    let (path, executor, allocation) = match admission {
        Ok(path) => path,
        Err(error) => return astra_tools::ToolResult::error(error),
    };
    // Pessimistic before awaiting: panic/drop leaves the allocation quarantined.
    allocations.mark_unsettled(path);
    let execution =
        astra_tools::ToolExecutor::execute_with_cancel(executor.as_ref(), tool, args, Some(cancel));
    tokio::pin!(execution);
    let mut result =
        match tokio::time::timeout(Duration::from_secs(timeout_secs), &mut execution).await {
            Ok(result) => result,
            Err(_) => {
                cancel.cancel();
                execution.await
            }
        };
    result
        .metadata
        .get_or_insert_with(serde_json::Map::new)
        .insert(
            "evaluation_allocation".into(),
            serde_json::to_value(&allocation).expect("allocation receipt serializes"),
        );
    let settled = tool != "bash"
        || result
            .metadata
            .as_ref()
            .and_then(|fields| fields.get("shell_confinement"))
            .and_then(|value| {
                serde_json::from_value::<astra_runtime_env::ShellExecutionEvidence>(value.clone())
                    .ok()
            })
            .is_some_and(|receipt| allocation_reusable_after_shell(&receipt));
    if settled {
        allocations.mark_settled(path);
    }
    result
}

fn allocation_reusable_after_shell(receipt: &astra_runtime_env::ShellExecutionEvidence) -> bool {
    receipt.schema_version == 1
        && receipt.profile == astra_runtime_env::WORKSPACE_CONFINEMENT_PROFILE
        && (!receipt.execution_started || authoritative_shell_settlement(receipt))
}

fn authoritative_shell_settlement(receipt: &astra_runtime_env::ShellExecutionEvidence) -> bool {
    receipt.settlement.scope_settled
        && matches!(
            receipt.settlement.ownership,
            Some(
                astra_runtime_env::ShellScopeOwnership::InvocationSupervisor
                    | astra_runtime_env::ShellScopeOwnership::InvocationCgroup
            )
        )
}

async fn execute_evaluation_verifier(
    boundary: &astra_sandbox::ShellProcessBoundary,
    command: &str,
    limits: &astra_sandbox::IsolationConfig,
    cancel: &CancellationToken,
) -> Result<(astra_sandbox::IsolatedOutput, Option<String>), String> {
    #[cfg(target_os = "linux")]
    {
        let plan = boundary
            .launch_plan_with_protected_paths(
                &boundary.workspace,
                "/usr/bin/bash",
                &[
                    "--noprofile".into(),
                    "--norc".into(),
                    "-c".into(),
                    command.into(),
                ],
                &[boundary.workspace.join(".git")],
            )
            .map_err(|error| format!("verifier confinement preparation failed: {error}"))?;
        let output = astra_sandbox::execute_confined_with_cancel(plan, limits, Some(cancel)).await;
        let receipt = output.execution_evidence();
        let error = if !matches!(
            receipt.setup,
            astra_runtime_env::ShellSetupEvidence::Verified { .. }
        ) || !authoritative_shell_settlement(&receipt)
        {
            Some(format!(
                "verifier confinement incomplete: {}",
                serde_json::to_string(&receipt).map_err(|e| e.to_string())?
            ))
        } else {
            None
        };
        let mut process = output.process;
        if error.is_some() {
            process.exit_code = None;
        }
        Ok((process, error))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (boundary, command, limits, cancel);
        Err("dedicated verifier requires Linux".into())
    }
}

async fn finalize_evaluation_workspace(
    base_workspace: &Path,
    requested_workspace: &Path,
    frozen_source_commit: &str,
    verifier_command: &str,
    verifier_timeout_secs: u64,
    cancel: CancellationToken,
    boundary: Option<astra_sandbox::ShellProcessBoundary>,
) -> Result<EvaluationWorkspaceFinalization, String> {
    if verifier_command.trim().is_empty()
        || verifier_command.len() > 4_096
        || !(1..=1_800).contains(&verifier_timeout_secs)
    {
        return Err("invalid frozen workspace verifier".into());
    }
    struct PreparedReplay {
        workspace_dir: String,
        verification: PathBuf,
        source: WorkspaceSourceIdentity,
        base_revision: String,
        result_revision: String,
        result_tree: String,
        patch: String,
    }

    let base_workspace = base_workspace.to_path_buf();
    let requested_workspace = requested_workspace.to_path_buf();
    let retained_allocation = boundary.is_some();
    let frozen_source_commit = frozen_source_commit.to_string();
    let prepare_cancel = cancel.clone();
    let prepared = tokio::task::spawn_blocking(move || {
        let workspace = if retained_allocation {
            requested_workspace
        } else {
            validate_workspace_override(&base_workspace, &requested_workspace)?
        };
        let source = workspace_source_identity(&workspace)
            .ok_or_else(|| "workspace source identity is unavailable".to_string())?;
        if source.commit != frozen_source_commit {
            return Err("workspace source commit differs from the frozen commit".into());
        }
        let nonce = uuid::Uuid::new_v4();
        let capture = workspace.with_file_name(format!(".astra-evaluation-capture-{nonce}"));
        let verification = workspace.with_file_name(format!(".astra-evaluation-verify-{nonce}"));
        let verification_cleanup = verification.clone();
        let operation = (|| {
            for (path, label) in [(&capture, "capture"), (&verification, "verification")] {
                let mut clone =
                    astra_tools::workspace_observation::hardened_git_command(&base_workspace)
                        .ok_or_else(|| "trusted Git executable is unavailable".to_string())?;
                clone
                    .args(["clone", "--no-local", "--no-checkout"])
                    .arg(&base_workspace)
                    .arg(path);
                let cloned = run_git_command_with_cancel(
                    clone,
                    &format!("git clone for evaluation {label}"),
                    Some(&prepare_cancel),
                )?;
                if !cloned.status.success() {
                    return Err(git_failure(
                        &cloned,
                        &format!("clone for evaluation {label}"),
                    ));
                }
                sanitize_evaluation_git_config(path)?;
                let checkout = git_command_with_cancel(
                    path,
                    &["checkout", "--detach", "--force", &frozen_source_commit],
                    Some(&prepare_cancel),
                )?;
                if !checkout.status.success() {
                    return Err(git_failure(
                        &checkout,
                        &format!("checkout evaluation {label}"),
                    ));
                }
            }
            let replay_source = verify_evaluation_workspace(&capture, &frozen_source_commit)?;
            let verification_source =
                verify_evaluation_workspace(&verification, &frozen_source_commit)?;
            if replay_source.tree != source.tree || verification_source != replay_source {
                return Err("workspace source identity differs from the trusted replay".into());
            }
            copy_evaluation_snapshot(&workspace, &capture, &prepare_cancel)?;
            let added = git_command_with_cancel(
                &capture,
                &["add", "--all", "--force", "--"],
                Some(&prepare_cancel),
            )?;
            if !added.status.success() {
                return Err(git_failure(&added, "stage evaluation capture"));
            }
            let diff = git_command_with_cancel(
                &capture,
                &[
                    "diff",
                    "--cached",
                    "--binary",
                    "--no-ext-diff",
                    "HEAD",
                    "--",
                ],
                Some(&prepare_cancel),
            )?;
            if !diff.status.success() {
                return Err(git_failure(&diff, "capture evaluation patch"));
            }
            if diff.stdout.len() > MAX_EVALUATION_PATCH_BYTES {
                return Err("evaluation patch exceeds the Edge evidence limit".into());
            }
            let patch = String::from_utf8(diff.stdout)
                .map_err(|_| "workspace patch is not valid UTF-8".to_string())?;
            if !patch.is_empty() {
                let patch_path = verification.join(".git").join("astra-evaluation.patch");
                std::fs::write(&patch_path, patch.as_bytes()).map_err(|error| {
                    format!("cannot stage evaluation patch for replay: {error}")
                })?;
                let mut apply =
                    astra_tools::workspace_observation::hardened_git_command(&verification)
                        .ok_or_else(|| "trusted Git executable is unavailable".to_string())?;
                apply
                    .args(["apply", "--index", "--binary", "--whitespace=nowarn", "--"])
                    .arg(&patch_path);
                let applied = run_git_command_with_cancel(
                    apply,
                    "git apply evaluation patch",
                    Some(&prepare_cancel),
                )?;
                let _ = std::fs::remove_file(&patch_path);
                if !applied.status.success() {
                    return Err(git_failure(&applied, "apply evaluation patch"));
                }
            }
            let tree =
                git_command_with_cancel(&verification, &["write-tree"], Some(&prepare_cancel))?;
            if !tree.status.success() {
                return Err(git_failure(&tree, "write evaluation result tree"));
            }
            let result_tree = String::from_utf8(tree.stdout)
                .map_err(|_| "evaluation result tree is not UTF-8".to_string())?
                .trim()
                .to_string();
            let base_revision = format!("git-tree:{}", replay_source.tree);
            let mut revision = Sha256::new();
            revision.update(replay_source.tree.as_bytes());
            revision.update([0]);
            revision.update(patch.as_bytes());
            Ok(PreparedReplay {
                workspace_dir: workspace.to_string_lossy().into_owned(),
                verification,
                source: replay_source,
                base_revision,
                result_revision: format!("sha256:{:x}", revision.finalize()),
                result_tree,
                patch,
            })
        })();
        let _ = std::fs::remove_dir_all(&capture);
        if operation.is_err() {
            let _ = std::fs::remove_dir_all(&verification_cleanup);
        }
        operation
    })
    .await
    .map_err(|error| format!("evaluation replay preparation task failed: {error}"))??;

    let dedicated = boundary.is_some();
    let mut preserve_unsettled_workspace = false;
    let result = async {
        let mut config = astra_sandbox::IsolationConfig::strict(prepared.verification.clone());
        config.timeout = Duration::from_secs(verifier_timeout_secs);
        config.max_output_bytes = MAX_EVALUATION_VERIFIER_OUTPUT_BYTES;
        config
            .read_only_paths
            .push(prepared.verification.join(".git"));
        let environment = std::collections::HashMap::from([
            ("HOME".to_string(), "/tmp/astra-evaluation-home".to_string()),
            ("LANG".to_string(), "C.UTF-8".to_string()),
            ("LC_ALL".to_string(), "C.UTF-8".to_string()),
            (
                "PATH".to_string(),
                "/usr/local/bin:/usr/bin:/bin".to_string(),
            ),
            ("TZ".to_string(), "UTC".to_string()),
        ]);
        let (output, evidence_error) = if let Some(mut boundary) = boundary {
            boundary.workspace = prepared.verification.clone();
            match execute_evaluation_verifier(&boundary, verifier_command, &config, &cancel).await {
                Ok(output) => output,
                Err(error) => {
                    preserve_unsettled_workspace = true;
                    return Ok(EvaluationWorkspaceFinalization {
                        workspace_dir: prepared.workspace_dir.clone(), source_commit: prepared.source.commit.clone(),
                        source_tree: prepared.source.tree.clone(), base_revision: prepared.base_revision.clone(),
                        result_revision: prepared.result_revision.clone(), patch: prepared.patch.clone(),
                        verifier_exit_code: None, verifier_output: String::new(),
                        namespace_active: false, scope_settled: false, timed_out: false, error: Some(error),
                    });
                }
            }
        } else {
            (astra_sandbox::execute_isolated_with_cancel_supervised(
                verifier_command, &environment, &config, Some(&cancel),
            ).await, None)
        };
        if let Some(error) = evidence_error {
            preserve_unsettled_workspace = output.execution_started;
            return Ok(EvaluationWorkspaceFinalization {
                workspace_dir: prepared.workspace_dir.clone(), source_commit: prepared.source.commit.clone(),
                source_tree: prepared.source.tree.clone(), base_revision: prepared.base_revision.clone(),
                result_revision: prepared.result_revision.clone(), patch: prepared.patch.clone(),
                verifier_exit_code: None, verifier_output: output.combined_output(),
                namespace_active: output.namespace_active, scope_settled: output.scope_settled,
                timed_out: output.timed_out, error: Some(error),
            });
        }
        if !output.scope_settled {
            preserve_unsettled_workspace = output.execution_started;
            return Err(format!(
                "workspace verifier process scope did not settle: ownership={:?}, namespace_active={}, cgroup_active={}, exit_code={:?}, timed_out={}, cancelled={}, stderr={}",
                output.scope_ownership,
                output.namespace_active,
                output.cgroup_active,
                output.exit_code,
                output.timed_out,
                output.cancelled,
                output.stderr.trim(),
            ));
        }
        let verification = prepared.verification.clone();
        let result_tree = prepared.result_tree.clone();
        let verify_cancel = cancel.clone();
        let verification_result = tokio::task::spawn_blocking(move || {
            let added = git_command_with_cancel(
                &verification,
                &["add", "--all", "--force", "--"],
                Some(&verify_cancel),
            )?;
            if !added.status.success() {
                return Err(git_failure(&added, "stage post-verifier workspace"));
            }
            let tree =
                git_command_with_cancel(&verification, &["write-tree"], Some(&verify_cancel))?;
            if !tree.status.success() {
                return Err(git_failure(&tree, "write post-verifier tree"));
            }
            let observed = String::from_utf8(tree.stdout)
                .map_err(|_| "post-verifier tree is not UTF-8".to_string())?;
            if observed.trim() != result_tree {
                return Err("workspace verifier modified the replayed result".into());
            }
            Ok::<_, String>(())
        })
        .await
        .map_err(|error| format!("post-verifier observation task failed: {error}"))
        .and_then(|result| result);
        if !dedicated { verification_result.as_ref().map_err(Clone::clone)?; }
        if verification_result.is_err() { preserve_unsettled_workspace = true; }
        Ok(EvaluationWorkspaceFinalization {
            workspace_dir: prepared.workspace_dir.clone(),
            source_commit: prepared.source.commit.clone(),
            source_tree: prepared.source.tree.clone(),
            base_revision: prepared.base_revision.clone(),
            result_revision: prepared.result_revision.clone(),
            patch: prepared.patch.clone(),
            verifier_exit_code: output.exit_code,
            verifier_output: output.combined_output(),
            namespace_active: output.namespace_active,
            scope_settled: output.scope_settled,
            timed_out: output.timed_out,
            error: verification_result.err(),
        })
    }
    .await;
    if !preserve_unsettled_workspace {
        let _ = std::fs::remove_dir_all(&prepared.verification);
    }
    result
}

fn independent_git_checkout(workspace: &Path) -> bool {
    independent_git_checkout_with_cancel(workspace, None)
}

fn independent_git_checkout_with_cancel(
    workspace: &Path,
    cancel: Option<&CancellationToken>,
) -> bool {
    let metadata = workspace.join(".git");
    let metadata_type = match std::fs::symlink_metadata(&metadata) {
        Ok(metadata) => metadata,
        Err(_) => return false,
    };
    if !metadata_type.file_type().is_dir() || metadata_type.file_type().is_symlink() {
        return false;
    }
    let Some(expected_git_dir) = metadata.canonicalize().ok() else {
        return false;
    };
    let Some(expected_worktree) = workspace.canonicalize().ok() else {
        return false;
    };
    let git_path = |argument: &str| {
        let output = git_command_with_cancel(workspace, &["rev-parse", argument], cancel).ok()?;
        if !output.status.success() {
            return None;
        }
        let path = PathBuf::from(String::from_utf8(output.stdout).ok()?.trim());
        let path = if path.is_absolute() {
            path
        } else {
            workspace.join(path)
        };
        path.canonicalize().ok()
    };
    let Some(actual_worktree) = git_path("--show-toplevel") else {
        return false;
    };
    if actual_worktree != expected_worktree {
        return false;
    }
    let Some(bare_output) =
        git_command_with_cancel(workspace, &["rev-parse", "--is-bare-repository"], cancel).ok()
    else {
        return false;
    };
    if !bare_output.status.success()
        || String::from_utf8_lossy(&bare_output.stdout).trim() != "false"
    {
        return false;
    }
    match (git_path("--git-dir"), git_path("--git-common-dir")) {
        (Some(git_dir), Some(common_dir)) => git_dir == common_dir && git_dir == expected_git_dir,
        _ => false,
    }
}

fn extract_workspace_override(args: &mut Value) -> Result<Option<PathBuf>, String> {
    let Some(object) = args.as_object_mut() else {
        return Ok(None);
    };
    let Some(value) = object.remove("__astra_workspace_dir") else {
        return Ok(None);
    };
    let path = value
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "Edge workspace override is invalid".to_string())?;
    Ok(Some(PathBuf::from(path)))
}

fn validate_workspace_override(base_workspace: &Path, requested: &Path) -> Result<PathBuf, String> {
    validate_workspace_override_with_cancel(base_workspace, requested, None)
}

fn validate_workspace_override_with_cancel(
    base_workspace: &Path,
    requested: &Path,
    cancel: Option<&CancellationToken>,
) -> Result<PathBuf, String> {
    if cancel.is_some_and(CancellationToken::is_cancelled) {
        return Err("Edge workspace validation was cancelled".into());
    }
    if std::fs::symlink_metadata(requested)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err("requested Edge workspace must not be a symlink".into());
    }
    let base = base_workspace
        .canonicalize()
        .map_err(|error| format!("edge workspace is unavailable: {error}"))?;
    let path = requested
        .canonicalize()
        .map_err(|error| format!("requested Edge workspace is unavailable: {error}"))?;
    if path == base {
        return Ok(path);
    }
    let managed_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(".astra-evaluation-"));
    if path.parent() == base.parent() && managed_name {
        if !independent_git_checkout_with_cancel(&path, cancel) {
            if cancel.is_some_and(CancellationToken::is_cancelled) {
                return Err("Edge workspace validation was cancelled".into());
            }
            return Err("requested Edge workspace is not an independent Git checkout".into());
        }
        return Ok(path);
    }
    Err("server requested an unmanaged Edge workspace".to_string())
}

// ─── Proxy helpers ───────────────────────────────────────────────────────────

fn first_nonempty(values: impl IntoIterator<Item = String>) -> Option<String> {
    values
        .into_iter()
        .map(|value| value.trim().to_string())
        .find(|value| !value.is_empty())
}

fn first_nonempty_env(names: &[&str]) -> Option<String> {
    first_nonempty(names.iter().filter_map(|name| std::env::var(name).ok()))
}

#[derive(Debug, PartialEq, Eq)]
enum ProxyConfigError {
    InvalidUrl { url: String, reason: String },
    UnsupportedScheme(String),
    MissingHost(String),
}

impl std::fmt::Display for ProxyConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidUrl { url, reason } => {
                write!(formatter, "invalid proxy URL '{url}': {reason}")
            }
            Self::UnsupportedScheme(scheme) => write!(
                formatter,
                "unsupported proxy URL scheme '{scheme}'; configure an http:// CONNECT proxy"
            ),
            Self::MissingHost(url) => write!(formatter, "proxy URL has no host: {url}"),
        }
    }
}

impl std::error::Error for ProxyConfigError {}

fn select_proxy_candidate(
    values: impl IntoIterator<Item = String>,
) -> Result<Option<String>, ProxyConfigError> {
    let Some(value) = first_nonempty(values) else {
        return Ok(None);
    };
    let parsed = reqwest::Url::parse(&value).map_err(|error| ProxyConfigError::InvalidUrl {
        url: redact_proxy_url(&value),
        reason: error.to_string(),
    })?;
    if parsed.scheme() != "http" {
        return Err(ProxyConfigError::UnsupportedScheme(
            parsed.scheme().to_string(),
        ));
    }
    if parsed.host_str().is_none() {
        return Err(ProxyConfigError::MissingHost(redact_proxy_url(&value)));
    }
    Ok(Some(value))
}

fn select_proxy_candidate_from_env(names: &[&str]) -> Result<Option<String>, ProxyConfigError> {
    select_proxy_candidate(names.iter().filter_map(|name| std::env::var(name).ok()))
}

/// Parse `host` and `port` from a WebSocket URL (`ws://` or `wss://`).
///
/// Handles IPv6 bracket notation (`[::1]:port`) and strips any path/query
/// component.  Does not support userinfo — WebSocket URLs with credentials
/// are not a use-case here.
fn parse_ws_target(ws_url: &str) -> Option<(String, u16)> {
    let (rest, default_port) = if let Some(r) = ws_url.strip_prefix("wss://") {
        (r, 443u16)
    } else {
        let r = ws_url.strip_prefix("ws://")?;
        (r, 80u16)
    };
    // Drop path/query/fragment — only the authority matters.
    let authority = rest.split('/').next()?;
    parse_host_port(authority, default_port)
}

fn ws_target_is_loopback(ws_url: &str) -> bool {
    let Some((host, _)) = parse_ws_target(ws_url) else {
        return false;
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .to_ascii_lowercase()
            .strip_suffix(".localhost")
            .is_some_and(|prefix| !prefix.is_empty())
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Parse `host`, `port`, and optional `userinfo` from an HTTP proxy URL.
///
/// Accepts the supported `http://` scheme, strips userinfo (e.g.
/// `user:pass@`), handles IPv6 bracket notation, and defaults to port 3128
/// when no explicit port is present.
///
/// Returns `(host, port, Option<userinfo>)`.
fn parse_proxy_addr(proxy_url: &str) -> Result<(String, u16, Option<String>), ProxyConfigError> {
    let parsed = reqwest::Url::parse(proxy_url).map_err(|error| ProxyConfigError::InvalidUrl {
        url: redact_proxy_url(proxy_url),
        reason: error.to_string(),
    })?;
    if parsed.scheme() != "http" {
        return Err(ProxyConfigError::UnsupportedScheme(
            parsed.scheme().to_string(),
        ));
    }
    let host = parsed
        .host_str()
        .filter(|host| !host.is_empty())
        .ok_or_else(|| ProxyConfigError::MissingHost(redact_proxy_url(proxy_url)))?
        .trim_matches(['[', ']'])
        .to_string();
    let port = parsed.port().unwrap_or(3128);
    let userinfo = if parsed.username().is_empty() && parsed.password().is_none() {
        None
    } else {
        Some(match parsed.password() {
            Some(password) => format!("{}:{password}", parsed.username()),
            None => parsed.username().to_string(),
        })
    };
    Ok((host, port, userinfo))
}

/// Returns `true` when `host` matches the NO_PROXY/no_proxy exclusion list.
///
/// Supports exact hostname matches and domain suffix matches (`.suffix` or
/// `suffix` both match `foo.suffix`). Port-specific entries (host:port) are
/// not supported and are matched on the host part only.
fn host_matches_no_proxy(host: &str, no_proxy: &str) -> bool {
    for entry in no_proxy.split(',') {
        let entry = entry.trim().trim_start_matches('.');
        // Ignore empty entries (stray/trailing commas, lone dots) — matching
        // curl/reqwest behavior. Only an explicit "*" is a catch-all wildcard.
        if entry.is_empty() {
            continue;
        }
        if entry == "*" {
            return true;
        }
        // Extract the host from the entry, tolerating bracketed/bare IPv6 and an
        // optional `:port` suffix. A bare IPv6 literal like `fd00::1` must NOT be
        // split on ':' — only strip a port when the form is unambiguous.
        let entry_host = if let Some(rest) = entry.strip_prefix('[') {
            // Bracketed IPv6: `[fd00::1]` or `[fd00::1]:port`.
            rest.split(']').next().unwrap_or(rest)
        } else if entry.matches(':').count() == 1 {
            // Exactly one colon → `host:port`.
            entry.split(':').next().unwrap_or(entry)
        } else {
            // No colon, or multiple colons (bare IPv6 such as `fd00::1`).
            entry
        };
        // DNS names are case-insensitive; normalize both sides for the exact and
        // domain-suffix comparisons.
        let host_lc = host.to_ascii_lowercase();
        let entry_lc = entry_host.to_ascii_lowercase();
        if host_lc == entry_lc
            || host_lc
                .strip_suffix(&entry_lc)
                .map(|prefix| prefix.ends_with('.'))
                .unwrap_or(false)
        {
            return true;
        }
    }
    false
}

/// Strip `user:pass@` credentials from a proxy URL so it is safe to log or
/// embed in error messages. Keeps the scheme and authority host:port.
fn redact_proxy_url(proxy_url: &str) -> String {
    match proxy_url.split_once("://") {
        Some((scheme, rest)) => {
            let host = rest.rsplit('@').next().unwrap_or(rest);
            format!("{scheme}://{host}")
        }
        None => proxy_url
            .rsplit('@')
            .next()
            .unwrap_or(proxy_url)
            .to_string(),
    }
}

/// Percent-decode a single URL component (`%XX` → byte). Invalid escapes are
/// left verbatim. Used to recover proxy credentials before Basic auth.
fn percent_decode(s: &str) -> String {
    fn hex_val(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push((h << 4) | l);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Build a `Proxy-Authorization: Basic ...` header value from URL userinfo.
///
/// Per RFC 3986 the userinfo components are percent-encoded, so decode the
/// username and password before base64 — otherwise credentials containing
/// reserved characters (`@`, `:`, `/`, …) authenticate with the literal `%XX`
/// text instead of the real value.
fn basic_proxy_auth(userinfo: &str) -> String {
    use base64::Engine as _;
    // Basic auth is always `username:password`; a userinfo with no ':' means an
    // empty password, which must still be encoded as `username:` (not bare
    // `username`).
    let decoded = match userinfo.split_once(':') {
        Some((user, pass)) => format!("{}:{}", percent_decode(user), percent_decode(pass)),
        None => format!("{}:", percent_decode(userinfo)),
    };
    let encoded = base64::engine::general_purpose::STANDARD.encode(decoded.as_bytes());
    format!("Basic {encoded}")
}

/// Parse `host:port` from an authority string, handling IPv6 bracket notation.
fn parse_host_port(authority: &str, default_port: u16) -> Option<(String, u16)> {
    if authority.starts_with('[') {
        // IPv6: `[::1]` or `[::1]:port`
        let bracket_end = authority.find(']')?;
        let host = authority.get(1..bracket_end)?.to_string();
        let port = if authority.as_bytes().get(bracket_end + 1) == Some(&b':') {
            authority.get(bracket_end + 2..)?.parse().ok()?
        } else {
            default_port
        };
        Some((host, port))
    } else if let Some(colon_pos) = authority.rfind(':') {
        let host = authority[..colon_pos].to_string();
        let port: u16 = authority[colon_pos + 1..].parse().ok()?;
        Some((host, port))
    } else {
        Some((authority.to_string(), default_port))
    }
}

/// Timeout for the HTTP CONNECT handshake and TLS setup inside the tunnel.
const PROXY_CONNECT_TIMEOUT_SECS: u64 = 30;

// Connect to the WebSocket server via HTTP CONNECT proxy.
// Returns the same type as `connect_async` so callers stay uniform.
async fn connect_via_proxy(
    ws_url: &str,
    proxy_url: &str,
    token: &str,
) -> Result<WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>, Box<dyn std::error::Error>> {
    let (ws_host, ws_port) =
        parse_ws_target(ws_url).ok_or_else(|| format!("Cannot parse WebSocket URL: {ws_url}"))?;
    let (proxy_host, proxy_port, userinfo) = parse_proxy_addr(proxy_url)?;

    tracing::info!(proxy_host = %proxy_host, proxy_port, target = %format!("{ws_host}:{ws_port}"), "CONNECT via proxy");

    let connect_timeout = Duration::from_secs(PROXY_CONNECT_TIMEOUT_SECS);

    let mut tcp = tokio::time::timeout(
        connect_timeout,
        tokio::net::TcpStream::connect(format!("{proxy_host}:{proxy_port}")),
    )
    .await
    .map_err(|_| format!("Proxy TCP connect timed out after {PROXY_CONNECT_TIMEOUT_SECS}s"))??;

    // HTTP CONNECT handshake — include Proxy-Authorization if userinfo present.
    let auth_header = userinfo
        .as_deref()
        .map(|ui| format!("Proxy-Authorization: {}\r\n", basic_proxy_auth(ui)))
        .unwrap_or_default();
    let req = format!(
        "CONNECT {ws_host}:{ws_port} HTTP/1.1\r\nHost: {ws_host}:{ws_port}\r\n{auth_header}\r\n"
    );
    tokio::time::timeout(connect_timeout, tcp.write_all(req.as_bytes()))
        .await
        .map_err(|_| "Proxy CONNECT write timed out")??;

    // Read proxy response headers (200 Connection Established). Read until
    // end-of-headers (\r\n\r\n) so we don't mis-parse when the status line is
    // split across TCP packets.
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 1024];
    let read_response = async {
        while buf.len() < 16 * 1024 {
            let n = tcp.read(&mut tmp).await?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        Ok::<(), std::io::Error>(())
    };
    tokio::time::timeout(connect_timeout, read_response)
        .await
        .map_err(|_| "Proxy CONNECT response timed out")??;

    let resp = String::from_utf8_lossy(&buf);
    if !resp.starts_with("HTTP/1.1 200") && !resp.starts_with("HTTP/1.0 200") {
        let first_line = resp.lines().next().unwrap_or("(empty)").to_string();
        return Err(format!("Proxy CONNECT rejected: {first_line}").into());
    }

    // For wss://, the CONNECT tunnel is plain TCP — we must perform a TLS
    // handshake inside the tunnel before sending the WebSocket upgrade.
    // For ws://, no TLS is needed; send the upgrade directly over the tunnel.
    if ws_url.starts_with("wss://") {
        let mut root_store = rustls::RootCertStore::empty();
        root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let tls_config = rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth();
        let connector = Connector::Rustls(std::sync::Arc::new(tls_config));
        let request = edge_ws_request(ws_url, token)?;
        let tls_and_ws = client_async_tls_with_config(request, tcp, None, Some(connector));
        let (ws_stream, _) = tokio::time::timeout(connect_timeout, tls_and_ws)
            .await
            .map_err(|_| "TLS+WebSocket upgrade timed out")??;
        Ok(ws_stream)
    } else {
        let request = edge_ws_request(ws_url, token)?;
        let ws_upgrade = tokio_tungstenite::client_async(request, MaybeTlsStream::Plain(tcp));
        let (ws_stream, _) = tokio::time::timeout(connect_timeout, ws_upgrade)
            .await
            .map_err(|_| "WebSocket upgrade timed out")??;
        Ok(ws_stream)
    }
}

fn edge_ws_request(
    ws_url: &str,
    token: &str,
) -> Result<tokio_tungstenite::tungstenite::http::Request<()>, Box<dyn std::error::Error>> {
    let mut request = ws_url.into_client_request()?;
    request.headers_mut().insert(
        tokio_tungstenite::tungstenite::http::header::AUTHORIZATION,
        tokio_tungstenite::tungstenite::http::HeaderValue::from_str(&format!("Bearer {token}"))?,
    );
    Ok(request)
}

// ─── Connection loop ─────────────────────────────────────────────────────────

async fn run_edge_connection(config: &EdgeConfig) -> Result<(), Box<dyn std::error::Error>> {
    let url = config.server_url.clone();

    tracing::info!(url = %url, edge_id = %config.edge_id, "Connecting to server...");

    // Prefer the scheme-appropriate proxy and select the first non-empty value;
    // an explicitly present but empty lowercase variable must not shadow a
    // populated uppercase fallback.
    let proxy_names: &[&str] = if url.starts_with("wss://") {
        &["https_proxy", "HTTPS_PROXY", "http_proxy", "HTTP_PROXY"]
    } else {
        &["http_proxy", "HTTP_PROXY"]
    };
    let proxy = if ws_target_is_loopback(&url) {
        None
    } else {
        select_proxy_candidate_from_env(proxy_names)?.and_then(|proxy_url| {
            // Extract the WS target host for NO_PROXY matching.
            let (ws_host, _) = parse_ws_target(&url)?;
            let no_proxy = first_nonempty_env(&["no_proxy", "NO_PROXY"]).unwrap_or_default();
            if !no_proxy.is_empty() && host_matches_no_proxy(&ws_host, &no_proxy) {
                tracing::debug!(
                    target: "astra.edge",
                    host = %ws_host,
                    "Skipping proxy: host matches NO_PROXY"
                );
                return None;
            }
            Some(proxy_url)
        })
    };

    // Snapshot the live token per connection attempt: the renewal task may
    // have replaced it since the previous (re)connect.
    let token_snapshot = config.token_manager.snapshot().await;
    let ws_stream = if let Some(ref proxy_url) = proxy {
        connect_via_proxy(&url, proxy_url, &token_snapshot)
            .await
            .map_err(|e| {
                tracing::error!(
                    target: "astra.edge",
                    edge_id = %config.edge_id,
                    url = %url,
                    proxy = %proxy_url.rsplit('@').next().unwrap_or(proxy_url),
                    error = %e,
                    "WebSocket connect via proxy failed"
                );
                e
            })?
    } else {
        let request = edge_ws_request(&url, &token_snapshot)?;
        let (ws, _) = connect_async(request).await.map_err(|e| {
            tracing::error!(
                target: "astra.edge",
                edge_id = %config.edge_id,
                url = %url,
                error = %e,
                "WebSocket connect failed"
            );
            e
        })?;
        ws
    };
    let (mut write, mut read) = ws_stream.split();

    tracing::info!("WebSocket connected, authenticating...");

    // Send auth
    let hostname = hostname::get().ok().and_then(|h| h.into_string().ok());
    let workspace = canonical_workspace_dir(&config.workspace_dir).map_err(|e| {
        Box::new(std::io::Error::new(std::io::ErrorKind::NotFound, e)) as Box<dyn std::error::Error>
    })?;
    let capabilities = if let Some(evaluation) = &config.evaluation {
        let evaluation = evaluation.lock().await;
        dedicated_runtime_environment_capabilities(
            &config.edge_id,
            &workspace,
            evaluation.provider.contract(),
        )
    } else {
        edge_runtime_environment_capabilities(&config.edge_id, &workspace)
    };
    let auth_msg = EdgeClientMessage::Auth {
        edge_agent_id: config.edge_id.clone(),
        materialization_id: config.materialization_id.clone(),
        interaction_api_major: astra_server_types::AGENT_INTERACTION_API_MAJOR.to_string(),
        hostname,
        workspace_dir: Some(workspace.to_string_lossy().to_string()),
        capabilities: Some(capabilities),
    };
    write
        .send(Message::Text(serde_json::to_string(&auth_msg)?.into()))
        .await?;

    // Wait for auth response
    let auth_timeout = Duration::from_secs(EDGE_AUTH_TIMEOUT_SECS);
    let auth_response = tokio::time::timeout(auth_timeout, read.next()).await;

    match auth_response {
        Ok(Some(Ok(Message::Text(text)))) => match serde_json::from_str::<EdgeServerMessage>(&text)
        {
            Ok(EdgeServerMessage::AuthOk {
                user_id,
                interaction_api_major,
            }) => {
                validate_interaction_api_major(&interaction_api_major)?;
                if let Some(evaluation) = &config.evaluation {
                    evaluation
                        .lock()
                        .await
                        .bind_owner(&user_id)
                        .map_err(PermanentEdgeConnectionError)?;
                }
                tracing::info!(user_id = %user_id, "Authenticated successfully");
                // The token that just proved itself is the SNAPSHOT this
                // connection authenticated with — not the current shared value
                // (a renewal during the handshake may have swapped in a newer,
                // unproven token). Persist exactly what was proven — but only
                // for MOI tokens, and never REGRESS the file over a token with
                // a later expiry (the renewal task owns forward progress).
                // The token that just proved itself is the SNAPSHOT this
                // connection authenticated with. The manager applies the
                // generation rule and owns any persistence retry.
                config.token_manager.mark_proven(&token_snapshot).await;
            }
            Ok(EdgeServerMessage::AuthError { message }) => {
                tracing::error!(
                    target: "astra.edge",
                    edge_id = %config.edge_id,
                    detail = %message,
                    "server rejected edge authentication"
                );
                return Err(PermanentEdgeConnectionError(format!(
                    "Authentication failed: {message}"
                ))
                .into());
            }
            _ => {
                tracing::error!(
                    target: "astra.edge",
                    edge_id = %config.edge_id,
                    "unexpected auth response payload"
                );
                return Err("Unexpected auth response".into());
            }
        },
        _ => {
            tracing::error!(
                target: "astra.edge",
                edge_id = %config.edge_id,
                "auth timeout or connection closed before auth_ok"
            );
            return Err("Auth timeout or connection closed".into());
        }
    }

    let session_id = format!("edge-{}", &uuid::Uuid::new_v4().to_string()[..8]);
    let executor = config.evaluation.is_none().then(|| {
        Arc::new(astra_tools::executor::DefaultToolExecutor::for_workspace(
            &workspace,
            config.edge_id.clone(),
            session_id.clone(),
            "astra-edge/0.1",
            Duration::from_secs(30),
        ))
    });
    let workspace_executors: Arc<
        Mutex<HashMap<PathBuf, Arc<astra_tools::executor::DefaultToolExecutor>>>,
    > = Arc::new(Mutex::new(HashMap::new()));
    let (completed_tx, mut completed_rx) = mpsc::channel::<CompletedEdgeInvocation>(1_024);
    let (workspace_operation_tx, mut workspace_operation_rx) =
        mpsc::channel::<EdgeClientMessage>(32);
    let execution_budget = EdgeExecutionBudget::new();
    let mut invocations = EdgeInvocationTracker::default();
    let mut tasks = JoinSet::new();
    let finalizations = Arc::new(std::sync::Mutex::new(
        HashMap::<String, CancellationToken>::new(),
    ));
    let _finalization_cancellation_guard = FinalizationCancellationGuard(finalizations.clone());
    let journal_path = edge_invocation_journal_path_in_root(
        &config.edge_id,
        &workspace,
        config.invocation_journal_root.clone(),
    );
    let mut journal = EdgeInvocationJournal::open(journal_path).await?;
    let journal_status = journal.status();
    tracing::info!(
        target: "astra.edge.invocation_journal",
        records = journal_status.records,
        running = journal_status.running,
        awaiting_ack = journal_status.awaiting_ack,
        state_bytes = journal_status.state_bytes,
        wal_entries = journal_status.wal_entries,
        wal_bytes = journal_status.wal_bytes,
        "edge invocation journal restored"
    );

    // Results remain in the durable outbox until the server acknowledges the
    // exact delivery generation. Reconnect therefore starts by replaying them.
    for pending in journal.pending_results()? {
        let message = pending.result.client_message(
            pending.request_id,
            pending.identity,
            pending.delivery_generation,
        );
        write
            .send(Message::Text(serde_json::to_string(&message)?.into()))
            .await?;
    }

    // Heartbeat ticker
    let mut heartbeat = tokio::time::interval(Duration::from_secs(EDGE_HEARTBEAT_INTERVAL_SECS));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    tracing::info!(
        workspace = %config.workspace_dir.display(),
        "Edge agent ready — waiting for tool calls"
    );

    let connection_result = async {
    loop {
        tokio::select! {
            joined = tasks.join_next(), if !tasks.is_empty() => {
                if let Some(Err(error)) = joined {
                    return Err(Box::new(error) as Box<dyn std::error::Error>);
                }
            }
            msg = read.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        tracing::debug!(
                            frame_len = text.len(),
                            "edge received server text frame"
                        );
                        match serde_json::from_str::<EdgeServerMessage>(&text) {
                            Ok(EdgeServerMessage::ToolRequest {
                                request_id,
                                identity,
                                delivery_generation,
                                tool,
                                args: mut tool_args,
                                runtime_process_authorization,
                                evaluation_allocation,
                                runtime_process_authorization_required,
                                timeout_secs,
                                }) => {
                                let journal_args = tool_args.clone();
                                let workspace_override = extract_workspace_override(&mut tool_args);
                                let workspace_override = match workspace_override {
                                    Ok(path) => path,
                                    Err(error) => {
                                        let message = rejected_tool_message(
                                            request_id,
                                            *identity,
                                            delivery_generation,
                                            error,
                                        );
                                        write
                                            .send(Message::Text(
                                                serde_json::to_string(&message)?.into(),
                                            ))
                                            .await?;
                                        continue;
                                    }
                                };
                                if !valid_runtime_process_authorization(
                                    &tool,
                                    runtime_process_authorization_required,
                                    runtime_process_authorization.as_deref(),
                                ) {
                                    let message = rejected_tool_message(
                                        request_id,
                                        *identity,
                                        delivery_generation,
                                        "Runtime process authorization is invalid",
                                    );
                                    write
                                        .send(Message::Text(
                                            serde_json::to_string(&message)?.into(),
                                        ))
                                        .await?;
                                    continue;
                                }
                                if !valid_evaluation_allocation_request(config.evaluation.is_some(), evaluation_allocation.as_deref(), &identity, workspace_override.as_deref()) {
                                    let message = rejected_tool_message(request_id, *identity, delivery_generation, "Evaluation allocation binding is missing or inconsistent");
                                    write.send(Message::Text(serde_json::to_string(&message)?.into())).await?;
                                    continue;
                                }
                                let execution_permit = execution_budget.try_acquire();
                                match journal
                                    .prepare(
                                        &request_id,
                                        &identity,
                                        delivery_generation,
                                        invocation_journal::InvocationPayload {
                                            tool: &tool, args: &journal_args, allocation: evaluation_allocation.as_deref(),
                                        },
                                        execution_permit.is_some(),
                                    )
                                    .await
                                {
                                    Ok(PrepareOutcome::Replay(result)) => {
                                        let message = result.client_message(
                                            request_id,
                                            *identity,
                                            delivery_generation,
                                        );
                                        write.send(Message::Text(serde_json::to_string(&message)?.into())).await?;
                                        continue;
                                    }
                                    Ok(PrepareOutcome::Active) => {
                                        tracing::warn!(
                                            request_id = %request_id,
                                            delivery_generation,
                                            "Duplicate edge delivery joined the existing invocation"
                                        );
                                        continue;
                                    }
                                    Ok(PrepareOutcome::Execute) => {}
                                    Err(error @ (JournalError::Full | JournalError::WalFull)) => {
                                        let journal_status = journal.status();
                                        tracing::warn!(
                                            target: "astra.edge.invocation_journal",
                                            %error,
                                            records = journal_status.records,
                                            running = journal_status.running,
                                            awaiting_ack = journal_status.awaiting_ack,
                                            state_bytes = journal_status.state_bytes,
                                            wal_entries = journal_status.wal_entries,
                                            wal_bytes = journal_status.wal_bytes,
                                            "edge invocation admission rejected by durable journal capacity"
                                        );
                                        let result = DurableEdgeResult::not_dispatched_rejection(
                                            format!("Edge invocation admission is temporarily saturated: {error}"),
                                        )
                                        .with_journal_status(&journal_status);
                                        let message = result.client_message(
                                            request_id,
                                            *identity,
                                            delivery_generation,
                                        );
                                        write.send(Message::Text(serde_json::to_string(&message)?.into())).await?;
                                        continue;
                                    }
                                    Err(error @ JournalError::IdentityConflict { .. }) => {
                                        let message = rejected_tool_message(
                                            request_id,
                                            *identity,
                                            delivery_generation,
                                            format!(
                                                "Edge invocation identity conflict before dispatch: {error}"
                                            ),
                                        );
                                        write.send(Message::Text(serde_json::to_string(&message)?.into())).await?;
                                        continue;
                                    }
                                    Err(error) => return Err(error.into()),
                                }
                                let execution_permit = execution_permit.ok_or_else(|| {
                                    format!(
                                        "edge invocation journal admitted {request_id} without execution capacity"
                                    )
                                })?;
                                let cancel = match invocations.begin(&request_id, delivery_generation) {
                                    Ok(cancel) => cancel,
                                    Err(active_generation) => {
                                        return Err(format!(
                                            "edge invocation tracker conflicts with durable journal for {request_id}: active generation {active_generation}, incoming {delivery_generation}"
                                        ).into());
                                    }
                                };
                                let workspace_executors = Arc::clone(&workspace_executors);
                                let base_workspace = workspace.clone();
                                let base_executor = executor.clone();
                                let evaluation = config.evaluation.clone();
                                let edge_id = config.edge_id.clone();
                                let session_id = session_id.clone();
                                let completed_tx = completed_tx.clone();
                                tracing::info!(tool = %tool, request_id = %request_id, generation = delivery_generation, "Executing tool");
                                tasks.spawn(async move {
                                    let _execution_permit = execution_permit;
                                    let start = Instant::now();
                                    if let Some(evaluation) = evaluation {
                                        let result = execute_evaluation_tool(
                                            evaluation, workspace_override.as_deref(), EvaluationToolCall {
                                                identity: &identity, tool: &tool, args: &tool_args,
                                                allocation: evaluation_allocation.as_deref().expect("validated dedicated allocation"),
                                                process_authorization: runtime_process_authorization.is_some() || runtime_process_authorization_required,
                                            }, timeout_secs, &cancel,
                                        ).await;
                                        let _ = completed_tx.send(CompletedEdgeInvocation {
                                            request_id, generation: delivery_generation, result,
                                            duration_ms: start.elapsed().as_millis() as u64,
                                        }).await;
                                        return;
                                    }
                                    // Git metadata validation is blocking
                                    // filesystem/process work. Own it inside
                                    // the invocation task so the receive loop
                                    // remains available for heartbeats and
                                    // cancellation; the same token also stops
                                    // the bounded Git probe.
                                    let workspace_override = match workspace_override {
                                        Some(path) => {
                                            let cancel_for_validation = cancel.clone();
                                            let validation_workspace = base_workspace.clone();
                                            match tokio::task::spawn_blocking(move || {
                                                validate_workspace_override_with_cancel(
                                                    &validation_workspace,
                                                    &path,
                                                    Some(&cancel_for_validation),
                                                )
                                            })
                                            .await
                                            {
                                                Ok(Ok(path)) => Some(path),
                                                Ok(Err(error)) => {
                                                    let completion = CompletedEdgeInvocation {
                                                        request_id,
                                                        generation: delivery_generation,
                                                        result: astra_tools::ToolResult::error(error),
                                                        duration_ms: start.elapsed().as_millis() as u64,
                                                    };
                                                    let _ = completed_tx.send(completion).await;
                                                    return;
                                                }
                                                Err(error) => {
                                                    let completion = CompletedEdgeInvocation {
                                                        request_id,
                                                        generation: delivery_generation,
                                                        result: astra_tools::ToolResult::error(
                                                            format!("Edge workspace validation task failed: {error}"),
                                                        ),
                                                        duration_ms: start.elapsed().as_millis() as u64,
                                                    };
                                                    let _ = completed_tx.send(completion).await;
                                                    return;
                                                }
                                            }
                                        }
                                        None => None,
                                    };
                                    // The ordinary Edge workspace already uses
                                    // the base executor. The mount boundary is
                                    // an evaluation allocation capability and
                                    // is enabled only for a distinct managed
                                    // clone.
                                    let workspace_override =
                                        workspace_override.filter(|path| path != &base_workspace);
                                    let executor = if let Some(workspace_override) = workspace_override
                                    {
                                        let mut executors = workspace_executors.lock().await;
                                        executors
                                            .entry(workspace_override.clone())
                                            .or_insert_with(|| {
                                                Arc::new(
                                                    astra_tools::executor::DefaultToolExecutor::for_workspace(
                                                        &workspace_override,
                                                        edge_id.clone(),
                                                        session_id.clone(),
                                                        "astra-edge/0.1",
                                                        Duration::from_secs(30),
                                                    )
                                                    .with_filesystem_write_boundary(vec![workspace_override.join(".git")])
                                                    .with_network_isolation(),
                                                )
                                            })
                                            .clone()
                                    } else {
                                        base_executor.expect("ordinary mode has a base executor")
                                    };
                                    let execution = async {
                                        if let Some(process_authorization) =
                                            runtime_process_authorization.as_deref()
                                        {
                                            runtime_process_authorization::execute_bash(
                                                executor.as_ref(),
                                                &tool_args,
                                                process_authorization,
                                                &cancel,
                                            )
                                            .await
                                        } else {
                                            astra_tools::ToolExecutor::execute_with_cancel(
                                                executor.as_ref(),
                                                &tool,
                                                &tool_args,
                                                Some(&cancel),
                                            ).await
                                        }
                                    };
                                    tokio::pin!(execution);
                                    // The executor receives the same token and
                                    // owns process settlement plus observation
                                    // evidence. Dropping its future at the
                                    // websocket boundary would lose that cleanup
                                    // window. A transport deadline therefore
                                    // cancels the same invocation token and
                                    // waits for the executor's terminal receipt.
                                    let result = match tokio::time::timeout(
                                        Duration::from_secs(timeout_secs),
                                        &mut execution,
                                    )
                                    .await {
                                        Ok(result) => result,
                                        Err(_) => {
                                            tracing::warn!(
                                                tool = %tool,
                                                request_id = %request_id,
                                                timeout_secs,
                                                "Tool transport deadline reached; cancelling and waiting for executor settlement"
                                            );
                                            cancel.cancel();
                                            execution.await
                                        }
                                    };
                                    let completion = CompletedEdgeInvocation {
                                        request_id,
                                        generation: delivery_generation,
                                        result,
                                        duration_ms: start.elapsed().as_millis() as u64,
                                    };
                                    let _ = completed_tx.send(completion).await;
                                });
                            }
                            Ok(EdgeServerMessage::WorkspacePrepare {
                                request_id, connection_generation, workspace_key, session_id, source_commit, confinement,
                            }) => {
                                let operation_tx = workspace_operation_tx.clone();
                                let base_workspace = workspace.clone();
                                let materialization_id = config.materialization_id.clone();
                                let evaluation = config.evaluation.clone();
                                tokio::spawn(async move {
                                    let authority = match evaluation { Some(value) => Some(value.lock_owned().await), None => None };
                                    let result = tokio::task::spawn_blocking(move || {
                                        let mut authority = authority.ok_or("dedicated allocation authority is required")?;
                                        authority.prepare(&base_workspace, &materialization_id, astra_server_types::edge_ws_protocol::EdgeWorkspacePreparationRequest {
                                            connection_generation, workspace_key: &workspace_key, session_id: &session_id,
                                            source_commit: &source_commit, confinement: &confinement,
                                        })
                                    }).await.map_err(|error| error.to_string()).and_then(|result| result);
                                    let (allocation, error) = match result {
                                        Ok(receipt) => (Some(receipt), None), Err(error) => (None, Some(error)),
                                    };
                                    let _ = operation_tx.send(EdgeClientMessage::WorkspacePrepared {
                                        request_id, connection_generation,
                                        workspace_dir: allocation.as_ref().map(|a| a.workspace_dir.clone()).unwrap_or_default(),
                                        source_commit: allocation.as_ref().map(|a| a.source_commit.clone()),
                                        source_tree: allocation.as_ref().map(|a| a.source_tree.clone()), allocation, error,
                                    }).await;
                                });
                            }
                            Ok(EdgeServerMessage::WorkspaceSnapshotRequest { request_id, connection_generation, allocation }) => {
                                let operation_tx = workspace_operation_tx.clone();
                                let workspace_dir = allocation.workspace_dir.clone();
                                let evaluation = config.evaluation.clone();
                                tokio::spawn(async move {
                                    let authority = match evaluation { Some(value) => Some(value.lock_owned().await), None => None };
                                    let result = tokio::task::spawn_blocking(move || {
                                        let authority = authority.ok_or("dedicated allocation authority is required")?;
                                        authority.validate(&allocation)?;
                                        let source = workspace_source_identity(Path::new(&allocation.workspace_dir)).ok_or("workspace source identity is unavailable")?;
                                        Ok::<_, String>((allocation, source))
                                    }).await.map_err(|error| error.to_string()).and_then(|result| result);
                                    let (allocation, source_commit, source_tree, clean, error) = match result {
                                        Ok((allocation, source)) => (Some(allocation), Some(source.commit), Some(source.tree), source.clean, None),
                                        Err(error) => (None, None, None, false, Some(error)),
                                    };
                                    let _ = operation_tx.send(EdgeClientMessage::WorkspaceSnapshot {
                                        request_id, connection_generation, workspace_dir, allocation, source_commit, source_tree, clean, error,
                                    }).await;
                                });
                            }
                            Ok(EdgeServerMessage::WorkspaceFinalize {
                                request_id,
                                connection_generation,
                                allocation,
                                verifier_command,
                                verifier_timeout_secs,
                                finalization_deadline_unix_ms,
                            }) => {
                                let workspace_dir = allocation.workspace_dir.clone();
                                let source_commit = allocation.source_commit.clone();
                                let operation_tx = workspace_operation_tx.clone();
                                let base_workspace = workspace.clone();
                                let requested_workspace = PathBuf::from(&workspace_dir);
                                let response_request_id = request_id.clone();
                                let response_workspace_dir = workspace_dir.clone();
                                let evaluation = config.evaluation.clone();
                                let cancel = CancellationToken::new();
                                finalizations
                                    .lock()
                                    .unwrap_or_else(|error| error.into_inner())
                                    .insert(request_id.clone(), cancel.clone());
                                let finalizations = finalizations.clone();
                                tokio::spawn(async move {
                                    let mut authority = match evaluation { Some(value) => Some(value.lock_owned().await), None => None };
                                    let now_unix_ms = std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .ok()
                                        .and_then(|value| u64::try_from(value.as_millis()).ok());
                                    let remaining_ms = now_unix_ms
                                        .and_then(|now| finalization_deadline_unix_ms.checked_sub(now))
                                        .filter(|remaining| *remaining > 0);
                                    let admission = authority.as_ref().ok_or_else(|| "dedicated allocation authority is required".to_string()).and_then(|authority| authority.validate(&allocation));
                                    let result = if let Err(error) = admission { Err(error) } else if let Some(remaining_ms) = remaining_ms {
                                        let boundary = authority.as_ref().map(|authority| authority.provider.boundary(&requested_workspace));
                                        if let Some(authority) = authority.as_mut() { authority.mark_unsettled(&requested_workspace); }
                                        let execution = finalize_evaluation_workspace(
                                            &base_workspace,
                                            &requested_workspace,
                                            &source_commit,
                                            &verifier_command,
                                            verifier_timeout_secs,
                                            cancel.clone(),
                                            boundary,
                                        );
                                        tokio::pin!(execution);
                                        match tokio::time::timeout(
                                            Duration::from_millis(remaining_ms),
                                            &mut execution,
                                        )
                                        .await
                                        {
                                            Ok(result) => result,
                                            Err(_) => {
                                                cancel.cancel();
                                                execution.await
                                            }
                                        }
                                    } else {
                                        Err("evaluation finalization deadline expired before Edge execution".into())
                                    };
                                    if let Some(authority) = authority.as_mut()
                                        && result.as_ref().is_ok_and(|result| result.error.is_none()) {
                                        authority.mark_settled(&requested_workspace);
                                    }
                                    finalizations
                                        .lock()
                                        .unwrap_or_else(|error| error.into_inner())
                                        .remove(&request_id);
                                    let mut message = match result {
                                        Ok(result) => EdgeClientMessage::WorkspaceFinalized {
                                            request_id,
                                            connection_generation,
                                            workspace_dir: result.workspace_dir,
                                            allocation: Some(allocation.clone()),
                                            source_commit: Some(result.source_commit),
                                            source_tree: Some(result.source_tree),
                                            base_revision: Some(result.base_revision),
                                            result_revision: Some(result.result_revision),
                                            patch: Some(result.patch),
                                            verifier_exit_code: result.verifier_exit_code,
                                            verifier_output: Some(result.verifier_output),
                                            namespace_active: result.namespace_active,
                                            scope_settled: result.scope_settled,
                                            timed_out: result.timed_out,
                                            error: result.error,
                                        },
                                        Err(error) => EdgeClientMessage::WorkspaceFinalized {
                                            request_id,
                                            connection_generation,
                                            workspace_dir,
                                            allocation: None,
                                            source_commit: None,
                                            source_tree: None,
                                            base_revision: None,
                                            result_revision: None,
                                            patch: None,
                                            verifier_exit_code: None,
                                            verifier_output: None,
                                            namespace_active: false,
                                            scope_settled: false,
                                            timed_out: false,
                                            error: Some(error),
                                        },
                                    };
                                    if serde_json::to_vec(&message)
                                        .is_ok_and(|bytes| bytes.len() > MAX_EDGE_FINALIZATION_MESSAGE_BYTES)
                                    {
                                        message = EdgeClientMessage::WorkspaceFinalized {
                                            request_id: response_request_id,
                                            connection_generation,
                                            workspace_dir: response_workspace_dir,
                                            allocation: None,
                                            source_commit: None,
                                            source_tree: None,
                                            base_revision: None,
                                            result_revision: None,
                                            patch: None,
                                            verifier_exit_code: None,
                                            verifier_output: None,
                                            namespace_active: false,
                                            scope_settled: false,
                                            timed_out: false,
                                            error: Some("evaluation evidence exceeds the Edge protocol limit".into()),
                                        };
                                    }
                                    let _ = operation_tx.send(message).await;
                                });
                            }
                            Ok(EdgeServerMessage::WorkspaceFinalizeCancel {
                                request_id,
                                connection_generation: _,
                            }) => {
                                if let Some(cancel) = finalizations
                                    .lock()
                                    .unwrap_or_else(|error| error.into_inner())
                                    .get(&request_id)
                                {
                                    cancel.cancel();
                                }
                            }
                            Ok(EdgeServerMessage::WorkspaceRelease { connection_generation: _, allocation }) => {
                                let base_workspace = workspace.clone();
                                let evaluation = config.evaluation.clone();
                                tokio::spawn(async move {
                                    let authority = match evaluation { Some(value) => Some(value.lock_owned().await), None => None };
                                    let result = tokio::task::spawn_blocking(move || {
                                        authority.ok_or("dedicated allocation authority is required")?.release(&base_workspace, &allocation)
                                    }).await;
                                    if !matches!(result, Ok(Ok(()))) {
                                        tracing::info!(?result, "Evaluation workspace retained after release");
                                    }
                                });
                            }
                            Ok(EdgeServerMessage::Pong {}) => {
                                // heartbeat ack
                            }
                            Ok(EdgeServerMessage::ToolCancel { request_id, delivery_generation }) => {
                                let execution_generation = journal
                                    .running_execution_generation(&request_id, delivery_generation);
                                if execution_generation.is_some_and(|generation| {
                                    invocations.cancel_if_current(&request_id, generation)
                                }) {
                                    tracing::info!(
                                        request_id = %request_id,
                                        delivery_generation,
                                        "Cancelled in-flight edge invocation"
                                    );
                                } else {
                                    tracing::debug!(request_id = %request_id, "Ignoring cancellation for non-active edge invocation");
                                }
                            }
                            Ok(EdgeServerMessage::ToolResultAck { request_id, delivery_generation }) => {
                                if !journal.acknowledge(&request_id, delivery_generation).await? {
                                    tracing::warn!(
                                        request_id = %request_id,
                                        delivery_generation,
                                        "Ignoring stale or unknown edge result acknowledgement"
                                    );
                                }
                            }
                            Ok(EdgeServerMessage::Closing { reason }) => {
                                tracing::info!(reason = %reason, "Server closing connection");
                                break;
                            }
                            Ok(EdgeServerMessage::AuthOk { .. } | EdgeServerMessage::AuthError { .. }) => {
                                // ignore duplicate auth
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "Failed to parse server message");
                            }
                        }
                    }
                    Some(Ok(Message::Ping(data))) => {
                        let _ = write.send(Message::Pong(data)).await;
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        tracing::info!("Connection closed");
                        break;
                    }
                    Some(Err(error)) => return Err(error.into()),
                    _ => {}
                }
            }
            Some(workspace_message) = workspace_operation_rx.recv() => {
                write
                    .send(Message::Text(serde_json::to_string(&workspace_message)?.into()))
                    .await?;
            }
            Some(completed) = completed_rx.recv() => {
                if !invocations.finish_if_current(&completed.request_id, completed.generation) {
                    tracing::warn!(
                        request_id = %completed.request_id,
                        generation = completed.generation,
                        "Discarding stale edge invocation completion"
                    );
                    continue;
                }
                let pending = persist_completion(&mut journal, completed).await?;
                let result_msg = pending.result.client_message(
                    pending.request_id,
                    pending.identity,
                    pending.delivery_generation,
                );
                write.send(Message::Text(serde_json::to_string(&result_msg)?.into())).await?;
            }
            _ = heartbeat.tick() => {
                let ping = EdgeClientMessage::Ping {};
                if write.send(Message::Text(serde_json::to_string(&ping)?.into())).await.is_err() {
                    tracing::warn!("Failed to send heartbeat");
                    break;
                }
            }
        }
    }

    Ok(())
    }.await;

    // Every exit (including journal and socket errors) settles this connection's
    // executions before reconnect can acquire the journal and dispatch again.
    invocations.cancel_all();
    // A failed append may have left a partial WAL record. Do not append again
    // until open() has validated/recovered it. Other connection failures do
    // not prevent preserving the results produced during cancellation.
    let journal_writable = !matches!(
        connection_result
            .as_ref()
            .err()
            .and_then(|error| error.downcast_ref::<JournalError>()),
        Some(JournalError::Io { .. } | JournalError::Corrupt { .. })
    );
    drop(completed_tx);
    let cleanup_result = settle_invocations(
        &mut tasks,
        &mut completed_rx,
        &mut journal,
        journal_writable,
    )
    .await;
    if let Err(error) = &cleanup_result {
        tracing::error!(component = "edge", operation = "settle_invocations", stage = "cleanup", error = %error, "Edge invocation cleanup failed");
    }
    connection_result.and(cleanup_result)
}

async fn persist_completion(
    journal: &mut EdgeInvocationJournal,
    completed: CompletedEdgeInvocation,
) -> Result<invocation_journal::PendingResult, JournalError> {
    let pending = journal
        .complete(
            &completed.request_id,
            completed.generation,
            DurableEdgeResult::from_tool_result(completed.result, completed.duration_ms),
        )
        .await?;
    tracing::info!(
        request_id = %completed.request_id,
        generation = completed.generation,
        duration_ms = completed.duration_ms,
        is_error = pending.result.is_error,
        output_len = pending.result.output.len(),
        "Tool execution complete"
    );
    Ok(pending)
}

async fn settle_invocations(
    tasks: &mut JoinSet<()>,
    completed_rx: &mut mpsc::Receiver<CompletedEdgeInvocation>,
    journal: &mut EdgeInvocationJournal,
    mut journal_writable: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut failure: Option<Box<dyn std::error::Error>> = None;
    // Spawned tasks continue running while we receive. Drain before joining:
    // queued completions have already released their execution permits, so
    // even a queue larger than the concurrency budget can fill up.
    // The connection must drop its sender before calling this function.
    while let Some(completed) = completed_rx.recv().await {
        if journal_writable {
            let request_id = completed.request_id.clone();
            if let Err(error) = persist_completion(journal, completed).await {
                tracing::error!(component = "edge", operation = "settle_invocations", stage = "persist_result", request_id = %request_id, error = %error, "Failed to persist completion during connection cleanup");
                journal_writable = false;
                failure = Some(Box::new(error));
            }
        }
    }
    while let Some(joined) = tasks.join_next().await {
        if let Err(error) = joined {
            tracing::error!(component = "edge", operation = "settle_invocations", stage = "join", error = %error, "Edge invocation task failed during cleanup");
            if failure.is_none() {
                failure = Some(Box::new(error));
            }
        }
    }
    failure.map_or(Ok(()), Err)
}

// ─── Main ────────────────────────────────────────────────────────────────────

fn main() {
    if let Some(exit_code) = astra_sandbox::run_invocation_supervisor_if_requested() {
        std::process::exit(exit_code);
    }
    astra_core::process_runtime::build_process_runtime()
        .expect("build Edge runtime")
        .block_on(run());
}

async fn run() {
    // The release builds CLI and Edge together, unifying ring and aws-lc
    // features. Select the Edge provider before constructing any TLS client.
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Edge installs its TLS provider before any TLS client");
    // Image-managed runners have no MOI installation marker and remain outside
    // the local update lifecycle. Local managed Edge holds a lease until exit.
    let executable = std::env::current_exe().expect("current executable");
    let args: Vec<String> = std::env::args().skip(1).collect();
    match astra_core::client_installation::early_command(&executable, &args, false) {
        Ok(Some(code)) => std::process::exit(code),
        Ok(None) => {}
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
    let _installation_lease = match astra_core::client_installation::acquire(&executable) {
        Ok(lease) => lease,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    };
    let process_capture =
        match astra_core::history_work_baseline::ProductionProcessCaptureGuard::from_env(
            astra_core::history_work_baseline::ProductionProcessRole::Edge,
        ) {
            Ok(process_capture) => process_capture,
            Err(error) => {
                eprintln!("Error: cannot start production baseline capture: {error}");
                std::process::exit(2);
            }
        };
    let _ = astra_logging::init_from_env(
        astra_logging::LogInitConfig::new("info").with_service_name("astra-edge"),
    );

    let args = Args::parse();
    let mut config = match resolve_config(args) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("Error: {error}");
            std::process::exit(2);
        }
    };

    if let Some(path) = &config.evaluation_config {
        let provider =
            match evaluation_allocation::Provider::start(path, &config.workspace_dir).await {
                Ok(provider) => provider,
                Err(error) => {
                    eprintln!("Dedicated evaluation startup rejected: {error}");
                    std::process::exit(2);
                }
            };
        config.evaluation = Some(Arc::new(Mutex::new(
            evaluation_allocation::Allocations::new(provider),
        )));
        tracing::info!("Dedicated evaluation provider capability is active");
    }

    eprintln!(
        "astra-edge v{} — remote tool execution agent",
        env!("CARGO_PKG_VERSION")
    );
    eprintln!("  server:    {}", config.server_url);
    eprintln!("  edge-id:   {}", config.edge_id);
    eprintln!("  workspace: {}", config.workspace_dir.display());
    eprintln!();

    // Background self-renewal of moi-user-token-v1 edge-registration tokens.
    token_renewal::spawn_renewal_task(config.token_manager.clone());

    let mut exit_with_error = false;
    let mut reconnect_delay_secs: u64 = 1;
    let max_reconnect_delay_secs: u64 = 60;
    loop {
        let edge_span = tracing::info_span!(
            "edge.agent",
            edge_id = %config.edge_id,
            server_url = %config.server_url,
        );
        match run_edge_connection(&config).instrument(edge_span).await {
            Ok(()) => {
                reconnect_delay_secs = 1; // reset on clean disconnect
                if !config.reconnect {
                    break;
                }
                tracing::info!(
                    delay = reconnect_delay_secs,
                    "Disconnected, reconnecting..."
                );
            }
            Err(e) => {
                if is_permanent_connection_error(e.as_ref()) {
                    // The chosen startup token may be revoked (e.g. a renewal
                    // rotated it away but the persist was lost). Before giving
                    // up, try the other startup candidate once.
                    if config.token_manager.swap_to_fallback().await {
                        tracing::warn!(
                            error = %e,
                            "Authentication failed with the selected token — retrying with the alternate startup token"
                        );
                        reconnect_delay_secs = 1;
                        continue;
                    }
                    tracing::error!(
                        error = %e,
                        "Permanent authentication failure — not retrying"
                    );
                    exit_with_error = true;
                    break;
                }
                tracing::error!(error = %e, "Connection error");
                if !config.reconnect {
                    exit_with_error = true;
                    break;
                }
                tracing::info!(delay = reconnect_delay_secs, "Reconnecting...");
            }
        }
        // Exponential backoff with jitter
        // jitter in [0.5*delay, 1.5*delay) — spreads out thundering herd
        let jitter = reconnect_delay_secs as f64 * (0.5 + fastrand::f64());
        tokio::time::sleep(Duration::from_secs_f64(jitter)).await;
        reconnect_delay_secs = (reconnect_delay_secs * 2).min(max_reconnect_delay_secs);
    }

    astra_logging::shutdown_otel();
    if let Some(process_capture) = process_capture
        && let Err(error) = process_capture.finish()
    {
        eprintln!("Error: cannot finish production baseline capture: {error}");
        exit_with_error = true;
    }
    if exit_with_error {
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cleanup_drains_a_full_completion_queue_and_preserves_results() {
        assert_cleanup_drains(false).await;
    }

    #[tokio::test]
    async fn cleanup_drains_senders_even_when_persistence_fails() {
        assert_cleanup_drains(true).await;
    }

    async fn assert_cleanup_drains(fail_first_completion: bool) {
        let state = tempfile::tempdir().unwrap();
        let path = state.path().join("journal.json");
        let mut journal = EdgeInvocationJournal::open(path.clone()).await.unwrap();
        let (tx, mut rx) = mpsc::channel(1);
        let mut tasks = JoinSet::new();
        for i in 0..4 {
            let identity = astra_server_types::edge_ws_protocol::ToolInvocationIdentity::new(
                "user",
                "session",
                "run",
                "turn",
                format!("completion-{i}"),
            )
            .unwrap();
            let request_id = identity.storage_key();
            journal
                .prepare(
                    &request_id,
                    &identity,
                    1,
                    invocation_journal::InvocationPayload {
                        tool: "bash",
                        args: &serde_json::json!({}),
                        allocation: None,
                    },
                    true,
                )
                .await
                .unwrap();
            let completion = CompletedEdgeInvocation {
                request_id: if fail_first_completion && i == 0 {
                    "missing-record".into()
                } else {
                    request_id
                },
                generation: 1,
                result: astra_tools::ToolResult::text(format!("finished-{i}")),
                duration_ms: i,
            };
            if i == 0 {
                tx.send(completion).await.unwrap();
            } else {
                let tx = tx.clone();
                tasks.spawn(async move {
                    tx.send(completion).await.unwrap();
                });
            }
        }
        assert_eq!(rx.len(), 1);
        drop(tx);
        let settled = tokio::time::timeout(
            Duration::from_secs(5),
            settle_invocations(&mut tasks, &mut rx, &mut journal, true),
        )
        .await
        .unwrap();
        assert!(tasks.is_empty());
        if fail_first_completion {
            let error = settled.unwrap_err();
            assert!(matches!(
                error.downcast_ref::<JournalError>(),
                Some(JournalError::Corrupt { .. })
            ));
            assert_eq!(
                journal.status().running,
                4,
                "stop appending after integrity failure"
            );
            drop(journal);
            let restored = EdgeInvocationJournal::open(path).await.unwrap();
            let pending = restored.pending_results().unwrap();
            assert_eq!(pending.len(), 4);
            assert!(pending.iter().all(
                |result| result.result.tool_result_fields.as_ref().unwrap()["outcome_certainty"]
                    == "unknown"
            ));
            return;
        }
        settled.unwrap();
        drop(journal);
        let restored = EdgeInvocationJournal::open(path).await.unwrap();
        let mut outputs = restored
            .pending_results()
            .unwrap()
            .into_iter()
            .map(|pending| {
                assert!(!pending.result.is_error);
                pending.result.output
            })
            .collect::<Vec<_>>();
        outputs.sort();
        assert_eq!(
            outputs,
            ["finished-0", "finished-1", "finished-2", "finished-3"]
        );
    }

    #[tokio::test]
    async fn connection_errors_cancel_and_reap_parallel_tools_before_returning() {
        assert_connection_cleanup(false).await;
    }

    #[tokio::test]
    async fn managed_connection_errors_cancel_and_reap_parallel_tools() {
        assert_connection_cleanup(true).await;
    }

    async fn assert_connection_cleanup(managed: bool) {
        let workspace = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = EdgeConfig {
            server_url: format!("ws://{}/edge/ws", listener.local_addr().unwrap()),
            token_manager: token_manager::TokenManager::new(
                "test-token".into(),
                None,
                state.path().join("token"),
            ),
            workspace_dir: workspace.path().to_owned(),
            edge_id: "cleanup-test".into(),
            materialization_id: "cleanup-materialization".into(),
            reconnect: false,
            invocation_journal_root: Some(state.path().to_owned()),
            evaluation_config: None,
            evaluation: None,
        };
        let server = async {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            assert!(ws.next().await.unwrap().unwrap().is_text());
            ws.send(Message::Text(
                serde_json::to_string(&EdgeServerMessage::AuthOk {
                    user_id: "user".into(),
                    interaction_api_major: astra_server_types::AGENT_INTERACTION_API_MAJOR.into(),
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
            for i in 0..2 {
                let identity = astra_server_types::edge_ws_protocol::ToolInvocationIdentity::new(
                    "user",
                    "session",
                    "run",
                    "turn",
                    format!("call-{i}"),
                )
                .unwrap();
                let request = EdgeServerMessage::ToolRequest {
                    request_id: identity.storage_key(), identity: Box::new(identity), delivery_generation: 1,
                    tool: "bash".into(), args: serde_json::json!({"command": format!("touch started-{i}; sleep 2; touch leaked-{i}")}),
                    evaluation_allocation: None,
                    runtime_process_authorization: managed.then(|| Box::new(astra_server_types::edge_ws_protocol::RuntimeProcessAuthorizationContext { authorization: "Bearer test-grant".into() })), runtime_process_authorization_required: managed, timeout_secs: 30,
                };
                ws.send(Message::Text(
                    serde_json::to_string(&request).unwrap().into(),
                ))
                .await
                .unwrap();
            }
            tokio::time::timeout(Duration::from_secs(10), async {
                while !(workspace.path().join("started-0").exists()
                    || workspace.path().join("started-1").exists())
                {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("one shell started; the other invocation may wait for its workspace lease");
            // An identity too large for even a terminal response forces an
            // error return from journal admission while both tools are active.
            let identity = astra_server_types::edge_ws_protocol::ToolInvocationIdentity::new(
                "user",
                "session",
                "run",
                "turn",
                "x".repeat(300_000),
            )
            .unwrap();
            ws.send(Message::Text(
                serde_json::to_string(&EdgeServerMessage::ToolRequest {
                    request_id: identity.storage_key(),
                    identity: Box::new(identity),
                    delivery_generation: 1,
                    tool: "bash".into(),
                    args: serde_json::json!({"command":"touch should-not-run"}),
                    evaluation_allocation: None,
                    runtime_process_authorization: None,
                    runtime_process_authorization_required: false,
                    timeout_secs: 30,
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
            while ws.next().await.is_some_and(|message| message.is_ok()) {}
        };
        let (_, result) = tokio::time::timeout(Duration::from_secs(15), async {
            tokio::join!(server, run_edge_connection(&config))
        })
        .await
        .unwrap();
        assert!(result.is_err());

        // The next connection must recover the abandoned journal entries,
        // replay their actual cancellation results, and never repeat effects.
        let recovery_server = async {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let auth = ws.next().await.unwrap().unwrap();
            let EdgeClientMessage::Auth {
                materialization_id,
                interaction_api_major,
                ..
            } = serde_json::from_str(auth.to_text().unwrap()).unwrap()
            else {
                panic!("expected registration on reconnect");
            };
            assert_eq!(materialization_id, config.materialization_id);
            assert_eq!(
                interaction_api_major,
                astra_server_types::AGENT_INTERACTION_API_MAJOR
            );
            ws.send(Message::Text(
                serde_json::to_string(&EdgeServerMessage::AuthOk {
                    user_id: "user".into(),
                    interaction_api_major: astra_server_types::AGENT_INTERACTION_API_MAJOR.into(),
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
            let mut replayed = std::collections::BTreeSet::new();
            while replayed.len() < 2 {
                let frame = ws.next().await.unwrap().unwrap();
                let message: EdgeClientMessage =
                    serde_json::from_str(frame.to_text().unwrap()).unwrap();
                if let EdgeClientMessage::ToolResult {
                    request_id,
                    identity,
                    delivery_generation,
                    is_error,
                    output,
                    ..
                } = message
                {
                    assert!(
                        replayed.insert(request_id.clone()),
                        "duplicate recovery result"
                    );
                    assert_eq!(request_id, identity.storage_key());
                    assert_eq!(delivery_generation, 1);
                    assert!(is_error);
                    // Started Bash returns partial-output cancellation text;
                    // a waiter cancelled before dispatch returns structured
                    // cancelled_tool_result. Preserve either owner's evidence.
                    assert!(
                        output.contains("cancelled"),
                        "lost cancellation evidence: {output}"
                    );
                    ws.send(Message::Text(
                        serde_json::to_string(&EdgeServerMessage::ToolResultAck {
                            request_id,
                            delivery_generation,
                        })
                        .unwrap()
                        .into(),
                    ))
                    .await
                    .unwrap();
                }
            }
            // Ordered after both ACKs, so connection completion also proves
            // that acknowledgement processing reached durable storage.
            ws.send(Message::Text(
                serde_json::to_string(&EdgeServerMessage::Closing {
                    reason: "recovery test complete".into(),
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
            while ws.next().await.is_some_and(|message| message.is_ok()) {}
        };
        let (_, recovered) = tokio::time::timeout(Duration::from_secs(15), async {
            tokio::join!(recovery_server, run_edge_connection(&config))
        })
        .await
        .unwrap();
        recovered.unwrap();
        let journal = EdgeInvocationJournal::open(edge_invocation_journal_path_in_root(
            &config.edge_id,
            &std::fs::canonicalize(workspace.path()).unwrap(),
            config.invocation_journal_root.clone(),
        ))
        .await
        .unwrap();
        assert_eq!(
            journal.status().records,
            0,
            "ACKs must remove recovered entries"
        );
        tokio::time::sleep(Duration::from_secs(3)).await;
        for name in ["leaked-0", "leaked-1", "should-not-run"] {
            assert!(
                !workspace.path().join(name).exists(),
                "orphan execution: {name}"
            );
        }
    }

    #[test]
    fn process_authorization_fails_closed_without_live_credential() {
        use astra_server_types::edge_ws_protocol::RuntimeProcessAuthorizationContext;

        let context = RuntimeProcessAuthorizationContext {
            authorization: "Bearer runtime-grant".to_string(),
        };
        assert!(valid_runtime_process_authorization(
            "bash",
            true,
            Some(&context)
        ));
        assert!(valid_runtime_process_authorization("bash", false, None));
        assert!(!valid_runtime_process_authorization("bash", true, None));
        assert!(!valid_runtime_process_authorization(
            "read_file",
            true,
            Some(&context)
        ));
    }

    #[test]
    fn reconnect_flag_accepts_an_explicit_false_for_bounded_process_runs() {
        let args = Args::try_parse_from(["astra-edge", "--reconnect=false"])
            .expect("explicit false must be a valid bounded-run configuration");

        assert!(!args.reconnect);
    }

    #[test]
    fn interaction_contract_mismatch_is_a_permanent_edge_connection_error() {
        assert!(
            validate_interaction_api_major(astra_server_types::AGENT_INTERACTION_API_MAJOR).is_ok()
        );
        let error = validate_interaction_api_major("1").unwrap_err();
        assert!(error.to_string().contains("expected 3"));
        assert!(is_permanent_connection_error(&error));
    }

    #[test]
    fn invocation_journal_honors_the_explicit_local_state_root() {
        let root = std::env::temp_dir().join("astra-edge-isolated-state");
        let workspace = std::env::temp_dir().join("astra-edge-workspace");
        let path =
            edge_invocation_journal_path_in_root("edge-test", &workspace, Some(root.clone()));

        assert_eq!(path.parent().and_then(Path::parent), Some(root.as_path()));
        assert_eq!(
            path.parent().and_then(Path::file_name),
            Some(std::ffi::OsStr::new("edge-invocations"))
        );
        assert_eq!(path.extension(), Some(std::ffi::OsStr::new("json")));
    }

    #[test]
    fn materialization_identity_is_stable_across_state_roots_and_distinct_per_checkout() {
        let state_a = tempfile::tempdir().expect("state A");
        let state_b = tempfile::tempdir().expect("state B");
        let device_state = tempfile::tempdir().expect("device state");
        let independent_device_state = tempfile::tempdir().expect("independent device state");
        let workspace = tempfile::tempdir().expect("workspace");
        let first = load_or_create_materialization_id_in_roots(
            workspace.path(),
            state_a.path(),
            device_state.path(),
        )
        .expect("create materialization identity");
        let reconnect = load_or_create_materialization_id_in_roots(
            workspace.path(),
            state_a.path(),
            device_state.path(),
        )
        .expect("reuse materialization identity");
        let other_state_same_workspace = load_or_create_materialization_id_in_roots(
            workspace.path(),
            state_b.path(),
            device_state.path(),
        )
        .expect("reuse materialization identity from another state root");
        assert_eq!(first, reconnect);
        assert_eq!(first, other_state_same_workspace);
        let independent_device_state_root = tempfile::tempdir().expect("independent device cache");
        let same_path_independent_device = load_or_create_materialization_id_in_roots(
            workspace.path(),
            independent_device_state_root.path(),
            independent_device_state.path(),
        )
        .expect("create independent device identity");
        assert_ne!(first, same_path_independent_device);
        let independent_checkout = tempfile::tempdir().expect("independent checkout");
        let independent_checkout_id = load_or_create_materialization_id_in_roots(
            independent_checkout.path(),
            state_b.path(),
            device_state.path(),
        )
        .expect("create independent checkout identity");
        assert_ne!(first, independent_checkout_id);
        std::fs::write(workspace.path().join("content-change"), b"changed")
            .expect("change workspace contents");
        let after_content_change = load_or_create_materialization_id_in_roots(
            workspace.path(),
            state_a.path(),
            device_state.path(),
        )
        .expect("identity after workspace content change");
        assert_eq!(first, after_content_change);
        assert!(
            materialization_id_path_in_state(workspace.path(), state_a.path())
                .starts_with(state_a.path())
        );
    }

    #[test]
    fn materialization_identity_publication_converges_under_concurrent_startup() {
        let state = tempfile::tempdir().expect("state");
        let device_state = tempfile::tempdir().expect("device state");
        let workspace = tempfile::tempdir().expect("workspace");
        let state_root = Arc::new(state.path().to_path_buf());
        let device_root = Arc::new(device_state.path().to_path_buf());
        let workspace_root = Arc::new(workspace.path().to_path_buf());
        let workers = (0..16)
            .map(|_| {
                let state_root = Arc::clone(&state_root);
                let device_root = Arc::clone(&device_root);
                let workspace_root = Arc::clone(&workspace_root);
                std::thread::spawn(move || {
                    load_or_create_materialization_id_in_roots(
                        &workspace_root,
                        &state_root,
                        &device_root,
                    )
                    .expect("concurrent materialization identity")
                })
            })
            .collect::<Vec<_>>();
        let identities = workers
            .into_iter()
            .map(|worker| worker.join().expect("identity worker must not panic"))
            .collect::<Vec<_>>();
        assert!(identities.windows(2).all(|pair| pair[0] == pair[1]));
        let identity_path = materialization_id_path_in_state(workspace.path(), state.path());
        let persisted = std::fs::read_to_string(identity_path).expect("published identity");
        assert_eq!(persisted, identities[0]);
    }

    #[test]
    fn evaluation_workspace_is_an_independent_clean_clone() {
        let root = tempfile::tempdir().expect("test root");
        let base = root.path().join("base");
        std::fs::create_dir(&base).expect("base directory");
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(&base)
                .args(args)
                .output()
                .expect("git command");
            assert!(
                output.status.success(),
                "git {:?}: {}",
                args,
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "--initial-branch=main"]);
        git(&["config", "user.email", "test@example.invalid"]);
        git(&["config", "user.name", "Astra Test"]);
        std::fs::write(base.join("README.md"), "source\n").expect("source file");
        git(&["add", "README.md"]);
        git(&["commit", "-m", "initial"]);

        let commit = git_object_id(&base, "HEAD^{commit}").expect("commit identity");
        let source = prepare_evaluation_workspace(&base, "materialization", "trial", &commit)
            .expect("prepare evaluation clone");
        let clone =
            evaluation_workspace_path(&base, "materialization", "trial").expect("clone path");
        assert_ne!(clone, base);
        assert!(
            clone.join(".git").is_dir(),
            "clone owns independent Git metadata"
        );
        assert_eq!(source.commit, commit);
        assert!(source.clean);
        std::fs::write(clone.join("trial-output"), "evidence\n").expect("trial output");
        assert!(!base.join("trial-output").exists());
        assert!(release_evaluation_workspace(&base, &clone, &commit).is_err());
        assert!(clone.exists(), "dirty clone is retained for evidence");
        std::fs::remove_file(clone.join("trial-output")).expect("remove test output");
        release_evaluation_workspace(&base, &clone, &commit).expect("release clean clone");
        assert!(!clone.exists());

        let source = prepare_evaluation_workspace(&base, "materialization", "trial", &commit)
            .expect("recreate evaluation clone");
        assert_eq!(source.commit, commit);
        let clone = evaluation_workspace_path(&base, "materialization", "trial")
            .expect("recreated clone path");
        let clone_git = |args: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(&clone)
                .args(args)
                .output()
                .expect("clone git command");
            assert!(
                output.status.success(),
                "clone git {:?}: {}",
                args,
                String::from_utf8_lossy(&output.stderr)
            );
        };
        clone_git(&["config", "user.email", "test@example.invalid"]);
        clone_git(&["config", "user.name", "Astra Test"]);
        clone_git(&["update-index", "--skip-worktree", "README.md"]);
        std::fs::write(clone.join("README.md"), "hidden trial output\n")
            .expect("hidden trial output");
        assert!(
            release_evaluation_workspace(&base, &clone, &commit).is_err(),
            "hidden index flags and content changes must retain the workspace"
        );
        std::fs::write(clone.join("README.md"), "source\n").expect("restore source file");
        clone_git(&["update-index", "--no-skip-worktree", "README.md"]);
        clone_git(&["config", "core.worktree", base.to_str().unwrap()]);
        assert!(
            release_evaluation_workspace(&base, &clone, &commit).is_err(),
            "a redirected Git worktree must not be eligible for cleanup"
        );
        clone_git(&["config", "--unset", "core.worktree"]);
        std::fs::write(clone.join("committed-output"), "committed evidence\n")
            .expect("committed trial output");
        clone_git(&["add", "committed-output"]);
        clone_git(&["commit", "-m", "trial result"]);
        assert!(release_evaluation_workspace(&base, &clone, &commit).is_err());
        assert!(
            clone.exists(),
            "committed trial output is retained for evidence"
        );
        std::fs::remove_dir_all(clone).expect("remove retained test clone");
    }

    #[cfg(unix)]
    #[test]
    fn git_runner_drains_both_pipes_before_waiting_for_exit() {
        let mut command = Command::new("sh");
        command.args([
            "-c",
            "head -c 262144 /dev/zero; head -c 49152 /dev/zero >&2",
        ]);
        let output = run_git_command(command, "test pipe drain")
            .expect("bounded Git runner must drain both pipes");
        assert_eq!(output.stdout.len(), 262_144);
        assert_eq!(output.stderr.len(), 49_152);
    }

    #[test]
    fn invocation_tracker_deduplicates_and_fences_stale_completions() {
        let mut tracker = EdgeInvocationTracker::default();
        let generation = 7;
        tracker.begin("request-1", generation).unwrap();
        assert_eq!(tracker.begin("request-1", 8).unwrap_err(), generation);
        assert!(!tracker.finish_if_current("request-1", generation + 1));
        assert_eq!(tracker.begin("request-1", 8).unwrap_err(), generation);
        assert!(tracker.finish_if_current("request-1", generation));

        let next_generation = generation + 1;
        tracker.begin("request-1", next_generation).unwrap();
        assert!(!tracker.finish_if_current("request-1", generation));
        assert!(tracker.finish_if_current("request-1", next_generation));
    }

    #[test]
    fn permanent_connection_errors_are_classified_by_type_and_http_status() {
        let authentication = PermanentEdgeConnectionError("denied".to_string());
        assert!(is_permanent_connection_error(&authentication));

        let proxy = ProxyConfigError::UnsupportedScheme("https".to_string());
        assert!(is_permanent_connection_error(&proxy));

        let unauthorized = tokio_tungstenite::tungstenite::Error::Http(
            tokio_tungstenite::tungstenite::http::Response::builder()
                .status(401)
                .body(None)
                .unwrap(),
        );
        assert!(is_permanent_connection_error(&unauthorized));

        let unavailable = tokio_tungstenite::tungstenite::Error::Http(
            tokio_tungstenite::tungstenite::http::Response::builder()
                .status(503)
                .body(None)
                .unwrap(),
        );
        assert!(!is_permanent_connection_error(&unavailable));
    }

    #[test]
    fn invocation_tracker_routes_cancellation_to_the_exact_active_request() {
        let mut tracker = EdgeInvocationTracker::default();
        let first_generation = 1;
        let first_cancel = tracker.begin("request-1", first_generation).unwrap();
        let second_cancel = tracker.begin("request-2", 2).unwrap();

        assert!(!tracker.cancel_if_current("request-1", first_generation + 1));
        assert!(!first_cancel.is_cancelled());
        assert!(tracker.cancel_if_current("request-1", first_generation));
        assert!(first_cancel.is_cancelled());
        assert!(!second_cancel.is_cancelled());
        assert!(!tracker.cancel_if_current("missing", first_generation));
    }

    #[test]
    fn execution_budget_admits_exactly_the_configured_concurrency() {
        let budget = EdgeExecutionBudget::new();
        let permits = (0..MAX_CONCURRENT_TOOL_EXECUTIONS)
            .map(|_| budget.try_acquire().expect("configured execution permit"))
            .collect::<Vec<_>>();
        assert!(
            budget.try_acquire().is_none(),
            "the first invocation beyond the execution budget must be rejected before dispatch"
        );
        drop(permits);
        assert!(
            budget.try_acquire().is_some(),
            "completed executions must release capacity"
        );
    }

    #[test]
    fn dedicated_advertisement_includes_the_verified_confinement_contract() {
        let root = tempfile::tempdir().unwrap();
        let confinement = astra_runtime_env::WorkspaceConfinementContract {
            profile_id: astra_runtime_env::WORKSPACE_CONFINEMENT_PROFILE.into(),
            toolchain_manifest: astra_runtime_env::ToolchainManifest {
                schema_version: 1,
                inputs: vec![astra_runtime_env::ToolchainInput {
                    guest_mount_path: "/usr/bin".into(),
                    content_digest: format!("sha256:{}", "a".repeat(64)),
                }],
                launcher_digest: format!("sha256:{}", "b".repeat(64)),
                supervisor_digest: format!("sha256:{}", "c".repeat(64)),
            },
        };
        let value = dedicated_runtime_environment_capabilities("test", root.path(), &confinement);
        assert_eq!(
            value["workspace_confinement"],
            serde_json::to_value(&confinement).unwrap()
        );
        assert_eq!(value["protocol_capabilities"], serde_json::json!({}));
        for name in value["binding"]["tool_surface"]["tool_names"]
            .as_array()
            .unwrap()
        {
            assert!(EVALUATION_TOOLS.contains(&name.as_str().unwrap()));
        }
    }

    #[test]
    fn dedicated_cli_mode_is_explicit() {
        let args = Args::try_parse_from([
            "astra-edge",
            "--evaluation-config",
            "/etc/astra/evaluation.json",
        ])
        .unwrap();
        assert_eq!(
            args.evaluation_config.as_deref(),
            Some(Path::new("/etc/astra/evaluation.json"))
        );
        assert!(
            Args::try_parse_from(["astra-edge"])
                .unwrap()
                .evaluation_config
                .is_none()
        );
    }

    #[test]
    fn foreground_settlement_cannot_release_dedicated_allocation() {
        use astra_runtime_env::{
            ShellExecutionEvidence, ShellScopeOwnership, ShellSettlementEvidence,
            ShellSetupEvidence,
        };
        let mut receipt = ShellExecutionEvidence {
            schema_version: 1,
            profile: astra_runtime_env::WORKSPACE_CONFINEMENT_PROFILE.into(),
            execution_started: true,
            setup: ShellSetupEvidence::Unverified {
                reason_code: "setup_or_exec_unverified".into(),
            },
            settlement: ShellSettlementEvidence {
                scope_settled: true,
                ownership: Some(ShellScopeOwnership::ForegroundProcessGroup),
                descendants_terminated: false,
            },
            timed_out: false,
            cancelled: false,
        };
        assert!(!authoritative_shell_settlement(&receipt));
        assert!(!allocation_reusable_after_shell(&receipt));
        receipt.execution_started = false;
        assert!(allocation_reusable_after_shell(&receipt));
        receipt.execution_started = true;
        receipt.settlement.ownership = Some(ShellScopeOwnership::InvocationSupervisor);
        assert!(authoritative_shell_settlement(&receipt));
        receipt.settlement.scope_settled = false;
        assert!(!authoritative_shell_settlement(&receipt));
    }

    #[test]
    fn edge_runtime_environment_capabilities_describe_local_edge_runtime() {
        let workspace = canonical_workspace_dir(Path::new(".")).expect("canonical test workspace");
        let value = edge_runtime_environment_capabilities("edge-test", &workspace);

        assert_eq!(
            value["schema_version"],
            RuntimeEnvironmentAdvertisement::SCHEMA_VERSION
        );
        assert_eq!(value["binding"]["workspace"]["kind"], "edge_workspace");
        assert_eq!(value["binding"]["workspace"]["authority"], "read_write");
        assert_eq!(
            value["binding"]["workspace"]["cwd"],
            workspace.to_string_lossy().as_ref()
        );
        assert_eq!(value["binding"]["executor"]["kind"], "edge_agent");
        assert_eq!(value["binding"]["executor"]["executor_id"], "edge-test");
        assert_eq!(
            value["binding"]["runtime"]["session_manager"],
            "host_process"
        );
        assert_eq!(
            value["binding"]["capabilities"]["runtime"]["runtime_has_shell"],
            true
        );
        assert_eq!(
            value["binding"]["capabilities"]["runtime"]["runtime_has_git"],
            true
        );
        assert_eq!(
            value["protocol_capabilities"]
                [astra_server_types::edge_ws_protocol::RUNTIME_PROCESS_AUTHORIZATION_V1_CAPABILITY],
            true
        );
        assert!(
            value["binding"]["tool_surface"]["tool_names"]
                .as_array()
                .unwrap()
                .iter()
                .any(|name| name.as_str() == Some("bash"))
        );
    }

    #[test]
    fn default_edge_id_is_stable_for_the_same_workspace() {
        let workspace = Path::new("/workspace/app");
        assert_eq!(default_edge_id(workspace), default_edge_id(workspace));
    }

    #[test]
    fn default_edge_id_is_workspace_scoped() {
        assert_ne!(
            default_edge_id(Path::new("/workspace/app-a")),
            default_edge_id(Path::new("/workspace/app-b"))
        );
    }

    #[test]
    fn edge_ws_url_accepts_api_or_ws_base_urls() {
        assert_eq!(
            edge_ws_url("http://127.0.0.1:17001").unwrap(),
            "ws://127.0.0.1:17001/edge/ws"
        );
        assert_eq!(
            edge_ws_url("https://astra.example.com").unwrap(),
            "wss://astra.example.com/edge/ws"
        );
        assert_eq!(
            edge_ws_url("wss://astra.example.com/edge/ws").unwrap(),
            "wss://astra.example.com/edge/ws"
        );
        assert_eq!(
            edge_ws_url("https://astra.example.com/edge/ws/extra-path").unwrap(),
            "wss://astra.example.com/edge/ws"
        );
        assert_eq!(
            edge_ws_url("https://astra.example.com/prefix").unwrap(),
            "wss://astra.example.com/prefix/edge/ws"
        );
        assert_eq!(
            edge_ws_url("https://astra.example.com/prefix/edge/ws/extra-path").unwrap(),
            "wss://astra.example.com/prefix/edge/ws"
        );
        assert_eq!(
            edge_ws_url("https://astra.example.com/not-edge/ws").unwrap(),
            "wss://astra.example.com/not-edge/ws/edge/ws"
        );
        assert_eq!(
            edge_ws_url("https://astra.example.com/edge/ws?debug=1#fragment").unwrap(),
            "wss://astra.example.com/edge/ws"
        );
        assert_eq!(
            edge_ws_url("127.0.0.1:17001").unwrap(),
            "ws://127.0.0.1:17001/edge/ws"
        );
        assert!(edge_ws_url("ftp://astra.example.com").is_err());
        assert!(edge_ws_url("").is_err());
    }

    #[test]
    fn websocket_proxy_policy_bypasses_only_process_local_targets() {
        for url in [
            "ws://localhost:17001/edge/ws",
            "ws://api.localhost:17001/edge/ws",
            "ws://127.0.0.1:17001/edge/ws",
            "ws://127.42.7.9:17001/edge/ws",
            "ws://[::1]:17001/edge/ws",
        ] {
            assert!(
                ws_target_is_loopback(url),
                "{url} must bypass inherited outbound proxies"
            );
        }
        for url in [
            "wss://astra.example.com/edge/ws",
            "ws://10.0.0.8:17001/edge/ws",
            "ws://host.docker.internal:17001/edge/ws",
            "ws://notlocalhost:17001/edge/ws",
        ] {
            assert!(
                !ws_target_is_loopback(url),
                "{url} must retain sandbox proxy routing"
            );
        }
    }

    #[test]
    fn proxy_value_selection_skips_present_but_empty_values() {
        assert_eq!(
            first_nonempty([
                "  ".to_string(),
                " https://proxy.example:8443 ".to_string(),
                "http://fallback.example:8080".to_string(),
            ]),
            Some("https://proxy.example:8443".to_string())
        );
        assert_eq!(first_nonempty(["".to_string(), "  ".to_string()]), None);
    }

    #[test]
    fn proxy_selection_rejects_the_first_configured_unsupported_scheme() {
        let error = select_proxy_candidate([
            "https://unsupported.example:8443".to_string(),
            "http://fallback.example:8080".to_string(),
        ])
        .expect_err("an unsupported configured proxy must not be bypassed");
        assert_eq!(
            error,
            ProxyConfigError::UnsupportedScheme("https".to_string())
        );
        assert_eq!(
            select_proxy_candidate(["  ".to_string(), "http://fallback.example:8080".to_string(),])
                .unwrap(),
            Some("http://fallback.example:8080".to_string())
        );
        assert_eq!(
            parse_proxy_addr("HTTP://user:pass@[::1]:8080/path").unwrap(),
            ("::1".to_string(), 8080, Some("user:pass".to_string()))
        );
    }

    #[test]
    fn token_from_credentials_uses_current_or_explicit_profile() {
        let mut creds = CredentialsFile {
            current_profile: Some("work".to_string()),
            profiles: Default::default(),
        };
        creds.profiles.insert(
            "work".to_string(),
            astra_credentials::Profile {
                access_token: Some("work-token".to_string()),
                ..Default::default()
            },
        );
        creds.profiles.insert(
            "other".to_string(),
            astra_credentials::Profile {
                access_token: Some("other-token".to_string()),
                ..Default::default()
            },
        );

        assert_eq!(
            token_from_credentials(&creds, None).unwrap(),
            ("work".to_string(), "work-token".to_string())
        );
        assert_eq!(
            token_from_credentials(&creds, Some("other")).unwrap(),
            ("other".to_string(), "other-token".to_string())
        );
    }
}
