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

mod invocation_journal;
mod runtime_file_transfer;
mod token_manager;
mod token_renewal;

use astra_credentials::{CredentialStore, CredentialsFile};
use astra_runtime_env::{
    ExecutorBinding, PolicyIntent, RunBinding, RuntimeBinding, RuntimeEnvironmentAdvertisement,
    ToolRegistry, WorkspaceAuthority, WorkspaceBinding,
};
use astra_server_types::edge_ws_protocol::{
    EDGE_AUTH_TIMEOUT_SECS, EDGE_HEARTBEAT_INTERVAL_SECS, EdgeClientMessage, EdgeServerMessage,
};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tokio_tungstenite::{
    Connector, MaybeTlsStream, WebSocketStream, client_async_tls_with_config, connect_async,
    tungstenite::Message, tungstenite::client::IntoClientRequest,
};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use invocation_journal::{DurableEdgeResult, EdgeInvocationJournal, JournalError, PrepareOutcome};

const MAX_CONCURRENT_TOOL_EXECUTIONS: usize = 128;

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
    reconnect: bool,
    invocation_journal_root: Option<PathBuf>,
}

#[derive(Debug)]
struct PermanentEdgeConnectionError(String);

impl std::fmt::Display for PermanentEdgeConnectionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for PermanentEdgeConnectionError {}

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

struct InFlightEdgeInvocation {
    generation: u64,
    cancel: CancellationToken,
}

#[derive(Default)]
struct EdgeInvocationTracker {
    in_flight: HashMap<String, InFlightEdgeInvocation>,
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
    Ok(EdgeConfig {
        server_url: edge_ws_url(&raw_server_url)?,
        token_manager: token_manager::TokenManager::new(token, fallback_token, token_file),
        workspace_dir,
        edge_id,
        reconnect: args.reconnect,
        invocation_journal_root: astra_runtime_env::local_state_root_override(),
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
    #[cfg(unix)]
    let managed_file_transfer_supported = workspace.starts_with(Path::new("/sandbox"));
    let workspace = workspace.to_string_lossy().to_string();
    let binding = RunBinding::resolve(
        WorkspaceBinding::edge_workspace(workspace, WorkspaceAuthority::ReadWrite),
        ExecutorBinding::edge_agent(edge_id.to_string()),
        RuntimeBinding::host_process(format!("edge-host:{edge_id}")),
        PolicyIntent::local_developer(),
        &registry,
    );

    let mut advertisement = serde_json::to_value(RuntimeEnvironmentAdvertisement::new(binding))
        .expect("runtime environment advertisement serializes");
    advertisement["protocol_capabilities"] = serde_json::json!({});
    #[cfg(unix)]
    if managed_file_transfer_supported {
        advertisement["protocol_capabilities"]
            [astra_server_types::edge_ws_protocol::MANAGED_FILE_TRANSFER_V1_CAPABILITY] =
            Value::Bool(true);
    }
    advertisement
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
    let auth_msg = EdgeClientMessage::Auth {
        edge_agent_id: config.edge_id.clone(),
        hostname,
        workspace_dir: Some(workspace.to_string_lossy().to_string()),
        capabilities: Some(edge_runtime_environment_capabilities(
            &config.edge_id,
            &workspace,
        )),
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
            Ok(EdgeServerMessage::AuthOk { user_id }) => {
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
    let executor = Arc::new(astra_tools::executor::DefaultToolExecutor::for_workspace(
        &workspace,
        config.edge_id.clone(),
        session_id,
        "astra-edge/0.1",
        Duration::from_secs(30),
    ));
    let (completed_tx, mut completed_rx) = mpsc::channel::<CompletedEdgeInvocation>(1_024);
    let execution_budget = EdgeExecutionBudget::new();
    let mut invocations = EdgeInvocationTracker::default();
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

    loop {
        tokio::select! {
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
                                args: tool_args,
                                runtime_file_transfer,
                                runtime_filesystem_boundary,
                                timeout_secs,
                            }) => {
                                let execution_permit = execution_budget.try_acquire();
                                match journal
                                    .prepare(
                                        &request_id,
                                        &identity,
                                        delivery_generation,
                                        &tool,
                                        &tool_args,
                                        execution_permit.is_some(),
                                    )
                                    .await
                                {
                                    Ok(PrepareOutcome::Replay(result)) => {
                                        let message = result.client_message(
                                            request_id,
                                            identity,
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
                                            identity,
                                            delivery_generation,
                                        );
                                        write.send(Message::Text(serde_json::to_string(&message)?.into())).await?;
                                        continue;
                                    }
                                    Err(error @ JournalError::IdentityConflict { .. }) => {
                                        let result = DurableEdgeResult::from_tool_result(
                                            astra_tools::ToolResult::error(format!(
                                                "Edge invocation identity conflict before dispatch: {error}"
                                            )),
                                            0,
                                        );
                                        let message = result.client_message(
                                            request_id,
                                            identity,
                                            delivery_generation,
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
                                let executor = executor.clone();
                                let completed_tx = completed_tx.clone();
                                let transfer_call_id = request_id.clone();
                                tracing::info!(tool = %tool, request_id = %request_id, generation = delivery_generation, "Executing tool");
                                tokio::spawn(async move {
                                    let _execution_permit = execution_permit;
                                    let start = Instant::now();
                                    let execution = async {
                                        if let Some(result) = runtime_file_transfer::execute(
                                            &tool,
                                            &tool_args,
                                            runtime_file_transfer.as_deref(),
                                            &transfer_call_id,
                                        )
                                        .await
                                        {
                                            result
                                        } else {
                                            runtime_file_transfer::execute_default_tool(
                                                executor.as_ref(),
                                                &tool,
                                                &tool_args,
                                                runtime_file_transfer.as_deref(),
                                                runtime_filesystem_boundary.as_deref(),
                                                &cancel,
                                            )
                                            .await
                                        }
                                    };
                                    let result = tokio::select! {
                                        _ = cancel.cancelled() => astra_tools::ToolResult::error(
                                            format!("Tool '{tool}' cancelled before completion")
                                        ),
                                        result = tokio::time::timeout(Duration::from_secs(timeout_secs), execution) => {
                                            match result {
                                                Ok(result) => result,
                                                Err(_) => astra_tools::ToolResult::error(
                                                    format!("Tool '{tool}' timed out after {timeout_secs}s")
                                                ),
                                            }
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
                            Ok(EdgeServerMessage::Pong) => {
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
                    _ => {}
                }
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
                let result = journal
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
                    is_error = result.is_error,
                    output_len = result.output.len(),
                    "Tool execution complete"
                );
                let record = journal.pending_results()?.into_iter().find(|pending| {
                    pending.request_id == completed.request_id
                }).ok_or_else(|| format!("durable edge result {} disappeared before delivery", completed.request_id))?;
                let result_msg = record.result.client_message(
                    record.request_id,
                    record.identity,
                    record.delivery_generation,
                );
                write.send(Message::Text(serde_json::to_string(&result_msg)?.into())).await?;
            }
            _ = heartbeat.tick() => {
                let ping = EdgeClientMessage::Ping;
                if write.send(Message::Text(serde_json::to_string(&ping)?.into())).await.is_err() {
                    tracing::warn!("Failed to send heartbeat");
                    break;
                }
            }
        }
    }

    invocations.cancel_all();

    Ok(())
}

// ─── Main ────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
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
    let config = match resolve_config(args) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("Error: {error}");
            std::process::exit(2);
        }
    };

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
    use astra_server_types::edge_ws_protocol::{
        RuntimeFileTransferAttachment, RuntimeFileTransferContext,
    };
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn reconnect_flag_accepts_an_explicit_false_for_bounded_process_runs() {
        let args = Args::try_parse_from(["astra-edge", "--reconnect=false"])
            .expect("explicit false must be a valid bounded-run configuration");

        assert!(!args.reconnect);
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

    #[tokio::test]
    async fn websocket_receive_loop_executes_managed_attachment_transfer() {
        let runtime_files = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/runtime-files/file-1"))
            .and(header("authorization", "Bearer runtime-grant"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"hello"))
            .mount(&runtime_files)
            .await;
        // write_file normalizes text files to a trailing newline; the upload
        // response must attest to the exact bytes produced by that real tool.
        let published_content = b"protocol report\n";
        let published_sha256 = format!("sha256:{:x}", sha2::Sha256::digest(published_content));
        let published_md5 = format!("{:x}", md5::Md5::digest(published_content));
        Mock::given(method("POST"))
            .and(path("/api/v1/runtime-files"))
            .and(header("authorization", "Bearer runtime-grant"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "file_id": "published-1",
                "filename": "report.txt",
                "size": published_content.len(),
                "md5": published_md5,
                "sha256": published_sha256,
                "content_type": "text/plain",
                "download_url": "https://example.invalid/published-1"
            })))
            .mount(&runtime_files)
            .await;

        let temp = tempfile::tempdir().expect("temporary edge workspace");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(&workspace).expect("create edge workspace");
        let workspace = workspace
            .canonicalize()
            .expect("canonicalize edge workspace");
        let transfer_root = workspace.join(".moi/runtime/task-1");
        std::fs::create_dir_all(&transfer_root).expect("create trusted transfer root");
        let identity = astra_turn_types::ToolInvocationIdentity::new(
            "user-1",
            "session-1",
            "run-1",
            "turn-1",
            "call-1",
        )
        .expect("complete invocation identity");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind websocket test server");
        let address = listener.local_addr().expect("websocket server address");
        let expected_catalog_file = transfer_root.join("catalog/000000-input.txt");
        let transfer_context = RuntimeFileTransferContext {
            endpoint_url: format!("{}/api/v1/runtime-files", runtime_files.uri()),
            authorization: "Bearer runtime-grant".to_string(),
            workspace_root: workspace.display().to_string(),
            layout: astra_server_types::edge_ws_protocol::RuntimeFileTransferLayout::Legacy {
                task_id: "task-1".to_string(),
                root: transfer_root.display().to_string(),
                catalog_dir: transfer_root.join("catalog").display().to_string(),
                session_dir: workspace
                    .join(".moi/sessions/session-1")
                    .display()
                    .to_string(),
                scratch_dir: transfer_root.join("scratch").display().to_string(),
            },
            max_file_bytes: 1024,
            attachments: vec![RuntimeFileTransferAttachment {
                file_id: "file-1".to_string(),
                name: "input.txt".to_string(),
                size: 5,
                md5: "5d41402abc4b2a76b9719d911017c592".to_string(),
            }],
        };
        let filesystem_boundary =
            astra_server_types::edge_ws_protocol::RuntimeFilesystemBoundaryContext {
                workspace_root: workspace.display().to_string(),
                read_only_paths: vec![
                    transfer_root.display().to_string(),
                    workspace
                        .join(".moi/sessions/session-1")
                        .display()
                        .to_string(),
                ],
            };
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept edge connection");
            let mut websocket = tokio_tungstenite::accept_async(stream)
                .await
                .expect("upgrade edge websocket");

            let auth = websocket
                .next()
                .await
                .expect("edge auth frame")
                .expect("edge auth");
            let Message::Text(auth) = auth else {
                panic!("expected edge auth text frame");
            };
            assert!(matches!(
                serde_json::from_str::<EdgeClientMessage>(&auth).expect("decode edge auth"),
                EdgeClientMessage::Auth { edge_agent_id, .. } if edge_agent_id == "edge-test"
            ));
            websocket
                .send(Message::Text(
                    serde_json::to_string(&EdgeServerMessage::AuthOk {
                        user_id: "user-1".to_string(),
                    })
                    .expect("encode auth ok")
                    .into(),
                ))
                .await
                .expect("send auth ok");

            let request = EdgeServerMessage::ToolRequest {
                request_id: identity.storage_key(),
                identity: identity.clone(),
                delivery_generation: 1,
                tool: "materialize_attachment".to_string(),
                args: json!({"file_id": "file-1"}),
                runtime_file_transfer: Some(Box::new(transfer_context.clone())),
                runtime_filesystem_boundary: Some(Box::new(filesystem_boundary.clone())),
                timeout_secs: 10,
            };
            websocket
                .send(Message::Text(
                    serde_json::to_string(&request)
                        .expect("encode transfer request")
                        .into(),
                ))
                .await
                .expect("send transfer request");

            loop {
                let frame = websocket
                    .next()
                    .await
                    .expect("edge result frame")
                    .expect("edge result");
                let Message::Text(text) = frame else {
                    continue;
                };
                match serde_json::from_str::<EdgeClientMessage>(&text).expect("decode edge frame") {
                    EdgeClientMessage::ToolResult {
                        request_id,
                        is_error,
                        tool_result_fields,
                        ..
                    } => {
                        assert_eq!(request_id, identity.storage_key());
                        assert!(!is_error);
                        assert_eq!(
                            tool_result_fields
                                .as_ref()
                                .and_then(|fields| fields.get("file_id"))
                                .and_then(Value::as_str),
                            Some("file-1")
                        );
                        break;
                    }
                    EdgeClientMessage::Ping | EdgeClientMessage::Auth { .. } => {}
                }
            }
            assert_eq!(
                std::fs::read(expected_catalog_file).expect("materialized attachment"),
                b"hello"
            );

            let write_identity = astra_turn_types::ToolInvocationIdentity::new(
                "user-1",
                "session-1",
                "run-1",
                "turn-1",
                "call-write",
            )
            .expect("complete write identity");
            websocket
                .send(Message::Text(
                    serde_json::to_string(&EdgeServerMessage::ToolRequest {
                        request_id: write_identity.storage_key(),
                        identity: write_identity.clone(),
                        delivery_generation: 1,
                        tool: "write_file".to_string(),
                        args: json!({"path": "report.txt", "content": "protocol report"}),
                        runtime_file_transfer: Some(Box::new(transfer_context.clone())),
                        runtime_filesystem_boundary: Some(Box::new(filesystem_boundary.clone())),
                        timeout_secs: 10,
                    })
                    .expect("encode write request")
                    .into(),
                ))
                .await
                .expect("send write request");
            loop {
                let Message::Text(text) = websocket.next().await.unwrap().unwrap() else {
                    continue;
                };
                match serde_json::from_str::<EdgeClientMessage>(&text).unwrap() {
                    EdgeClientMessage::ToolResult {
                        request_id,
                        is_error,
                        ..
                    } if request_id == write_identity.storage_key() => {
                        assert!(!is_error, "write_file failed: {text}");
                        break;
                    }
                    EdgeClientMessage::Ping | EdgeClientMessage::Auth { .. } => {}
                    EdgeClientMessage::ToolResult { .. } => {}
                }
            }

            let publish_identity = astra_turn_types::ToolInvocationIdentity::new(
                "user-1",
                "session-1",
                "run-1",
                "turn-1",
                "call-publish",
            )
            .expect("complete publish identity");
            websocket
                .send(Message::Text(
                    serde_json::to_string(&EdgeServerMessage::ToolRequest {
                        request_id: publish_identity.storage_key(),
                        identity: publish_identity.clone(),
                        delivery_generation: 1,
                        tool: "publish_artifact".to_string(),
                        args: json!({"path": "report.txt"}),
                        runtime_file_transfer: Some(Box::new(transfer_context.clone())),
                        runtime_filesystem_boundary: Some(Box::new(filesystem_boundary.clone())),
                        timeout_secs: 10,
                    })
                    .expect("encode publish request")
                    .into(),
                ))
                .await
                .expect("send publish request");
            loop {
                let Message::Text(text) = websocket.next().await.unwrap().unwrap() else {
                    continue;
                };
                match serde_json::from_str::<EdgeClientMessage>(&text).unwrap() {
                    EdgeClientMessage::ToolResult {
                        request_id,
                        is_error,
                        tool_result_fields,
                        ..
                    } if request_id == publish_identity.storage_key() => {
                        assert!(!is_error, "publish_artifact failed: {text}");
                        assert_eq!(
                            tool_result_fields
                                .as_ref()
                                .and_then(|fields| fields.get("file_id"))
                                .and_then(Value::as_str),
                            Some("published-1")
                        );
                        assert_eq!(
                            tool_result_fields
                                .as_ref()
                                .and_then(|fields| fields.get("artifacts"))
                                .and_then(Value::as_array)
                                .and_then(|artifacts| artifacts.first())
                                .and_then(|artifact| artifact.get("artifact_id"))
                                .and_then(Value::as_str),
                            Some("published-1")
                        );
                        break;
                    }
                    EdgeClientMessage::Ping | EdgeClientMessage::Auth { .. } => {}
                    EdgeClientMessage::ToolResult { .. } => {}
                }
            }

            let denied_identity = astra_turn_types::ToolInvocationIdentity::new(
                "user-1",
                "session-1",
                "run-1",
                "turn-1",
                "call-denied",
            )
            .expect("complete denied invocation identity");
            websocket
                .send(Message::Text(
                    serde_json::to_string(&EdgeServerMessage::ToolRequest {
                        request_id: denied_identity.storage_key(),
                        identity: denied_identity.clone(),
                        delivery_generation: 1,
                        tool: "materialize_attachment".to_string(),
                        args: json!({"file_id": "file-1"}),
                        runtime_file_transfer: None,
                        runtime_filesystem_boundary: None,
                        timeout_secs: 10,
                    })
                    .expect("encode denied transfer request")
                    .into(),
                ))
                .await
                .expect("send denied transfer request");
            loop {
                let frame = websocket
                    .next()
                    .await
                    .expect("denied edge result frame")
                    .expect("denied edge result");
                let Message::Text(text) = frame else {
                    continue;
                };
                match serde_json::from_str::<EdgeClientMessage>(&text).expect("decode edge frame") {
                    EdgeClientMessage::ToolResult {
                        request_id,
                        output,
                        is_error,
                        ..
                    } => {
                        assert_eq!(request_id, denied_identity.storage_key());
                        assert!(is_error);
                        assert!(output.contains("file transfer is unavailable"));
                        break;
                    }
                    EdgeClientMessage::Ping | EdgeClientMessage::Auth { .. } => {}
                }
            }
            websocket
                .send(Message::Text(
                    serde_json::to_string(&EdgeServerMessage::Closing {
                        reason: "test complete".to_string(),
                    })
                    .expect("encode closing")
                    .into(),
                ))
                .await
                .expect("close edge connection");
        });

        let config = EdgeConfig {
            server_url: format!("ws://{address}/edge/ws"),
            token_manager: token_manager::TokenManager::new(
                "test-token".to_string(),
                None,
                temp.path().join("edge-token"),
            ),
            workspace_dir: workspace,
            edge_id: "edge-test".to_string(),
            reconnect: false,
            invocation_journal_root: Some(temp.path().join("state")),
        };

        run_edge_connection(&config)
            .await
            .expect("edge websocket transfer loop");
        server.await.expect("websocket test server");
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
        assert!(
            value["protocol_capabilities"]
                .get(astra_server_types::edge_ws_protocol::MANAGED_FILE_TRANSFER_V1_CAPABILITY)
                .is_none()
        );
        assert!(
            value["binding"]["tool_surface"]["tool_names"]
                .as_array()
                .unwrap()
                .iter()
                .any(|name| name.as_str() == Some("bash"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn managed_sandbox_edge_advertises_file_transfer_capability() {
        let value = edge_runtime_environment_capabilities("edge-managed", Path::new("/sandbox"));

        assert_eq!(
            value["protocol_capabilities"]
                [astra_server_types::edge_ws_protocol::MANAGED_FILE_TRANSFER_V1_CAPABILITY],
            true
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
