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

mod capacity_provider;
mod managed_background;
mod runtime_process_authorization;
mod token_manager;
mod token_renewal;

use astra_credentials::{CredentialStore, CredentialsFile};
use astra_runtime_env::{
    ExecutorBinding, PolicyIntent, RunBinding, RuntimeBinding, RuntimeEnvironmentAdvertisement,
    ToolRegistry, WorkspaceAuthority, WorkspaceBinding,
};
use astra_server_types::edge_ws_protocol::EdgeClientMessage;
#[cfg(test)]
use astra_server_types::edge_ws_protocol::EdgeServerMessage;
use clap::Parser;
#[cfg(test)]
use futures_util::SinkExt;
#[cfg(test)]
use futures_util::StreamExt;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
#[cfg(test)]
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

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
    /// Private local configuration for explicitly enabled sealed capacity providers.
    #[arg(long)]
    capacity_providers: Option<PathBuf>,
}

#[derive(Debug)]
struct EdgeConfig {
    capacity_providers: Vec<capacity_provider::LocalCapacityProvider>,
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
}

fn is_permanent_connection_error(error: &(dyn std::error::Error + 'static)) -> bool {
    if let Some(error) = error.downcast_ref::<astra_edge::EdgeAuthenticationError>() {
        return error.is_permanent();
    }
    if error.downcast_ref::<ProxyConfigError>().is_some() {
        return true;
    }
    matches!(
        error.downcast_ref::<tokio_tungstenite::tungstenite::Error>(),
        Some(tokio_tungstenite::tungstenite::Error::Http(response))
            if matches!(response.status().as_u16(), 401 | 403)
    )
}

async fn observe_shutdown(
    shutdown: CancellationToken,
    first_signal: impl Future<Output = std::io::Result<()>>,
    second_signal: impl Future<Output = std::io::Result<()>>,
    on_second_interrupt: impl FnOnce(),
) {
    match first_signal.await {
        Err(error) => {
            tracing::warn!(%error, "Cannot install Ctrl+C handler");
        }
        Ok(()) => {
            tracing::info!("Ctrl+C received; shutting down");
            shutdown.cancel();
            // Tokio keeps a process-wide SIGINT handler installed after the
            // first wait returns. Keep consuming later signals so a second
            // Ctrl+C can escape leftover blocking work during runtime shutdown.
            match second_signal.await {
                Ok(()) => {
                    tracing::warn!(
                        "Second Ctrl+C received; exiting without waiting for leftover blocking work"
                    );
                    on_second_interrupt();
                }
                Err(error) => {
                    tracing::warn!(%error, "Cannot install a second Ctrl+C handler");
                }
            }
        }
    }
}

/// Drive one connection until it finishes or the process-lifetime shutdown
/// token is cancelled. After cancel, the connection is awaited to completion
/// with no wall-clock timeout so EdgeInvocationOwner can finish cancellation,
/// join, and persist settlement.
async fn await_connection_result<F>(
    connection: &mut std::pin::Pin<&mut F>,
    shutdown: &CancellationToken,
) -> Option<Result<(), Box<dyn std::error::Error>>>
where
    F: Future<Output = Result<(), Box<dyn std::error::Error>>>,
{
    tokio::select! {
        biased;
        _ = shutdown.cancelled() => match connection.await {
            Ok(()) => None,
            Err(error) => Some(Err(error)),
        },
        result = &mut *connection => Some(result),
    }
}

/// Race pre-owner transport establishment (proxy CONNECT, TLS and the
/// WebSocket upgrade) against shutdown. Once a connection is admitted, its
/// settlement is awaited by [`await_connection_result`] instead.
async fn establish_unless_shutdown<T>(
    establish: impl Future<Output = T>,
    shutdown: &CancellationToken,
) -> Option<T> {
    tokio::select! {
        biased;
        _ = shutdown.cancelled() => None,
        result = establish => Some(result),
    }
}

async fn wait_for_reconnect(delay: Duration, shutdown: &CancellationToken) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(delay) => true,
        _ = shutdown.cancelled() => false,
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
    let token_file_override = token_renewal::explicit_token_file_override();
    let managed_dir = managed_background::credential_handoff_dir();
    let token_file = token_renewal::runner_token_file(
        &workspace_dir,
        token_file_override.as_deref(),
        managed_dir.as_deref(),
    );
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
        capacity_providers: capacity_provider::load(args.capacity_providers.as_deref())?,
        server_url: astra_edge::edge_ws_url(&raw_server_url)?,
        token_manager: token_manager::TokenManager::new(token, fallback_token, token_file),
        workspace_dir,
        edge_id,
        materialization_id,
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

use astra_runtime_env::load_or_create_materialization_id;
#[cfg(test)]
use astra_runtime_env::{
    load_or_create_materialization_id_in_roots, materialization_id_path_in_state,
};

fn edge_invocation_journal_path_in_root(
    edge_id: &str,
    server_url: &str,
    workspace_dir: &Path,
    state_root: Option<PathBuf>,
) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(edge_id.as_bytes());
    hasher.update([0]);
    hasher.update(server_url.as_bytes());
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
    advertisement["protocol_capabilities"]
        [astra_server_types::edge_ws_protocol::RUNTIME_PROCESS_AUTHORIZATION_V1_CAPABILITY] =
        Value::Bool(true);
    advertisement["protocol_capabilities"]
        [astra_server_types::edge_ws_protocol::PROTECTED_BASH_WRITE_V1_CAPABILITY] =
        Value::Bool(true);
    advertisement["protocol_capabilities"]
        [astra_server_types::edge_ws_protocol::W3C_TRACE_CONTEXT_V1_CAPABILITY] = Value::Bool(true);
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

// Open the same monitored WebSocket through an HTTP CONNECT tunnel.
async fn connect_via_proxy(
    ws_url: &str,
    proxy_url: &str,
    token: &str,
) -> Result<astra_edge::EdgeSocket, Box<dyn std::error::Error>> {
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

    let request = edge_ws_request(ws_url, token)?;
    Ok(tokio::time::timeout(
        connect_timeout,
        astra_edge::upgrade_edge_socket(request, tcp),
    )
    .await
    .map_err(|_| "WebSocket upgrade timed out")??)
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

fn managed_ready_sender() -> Option<tokio::sync::oneshot::Sender<()>> {
    if !managed_background::is_managed_child() {
        return None;
    }
    // Capture the phase generation before this connection can disconnect.
    // The callback may run after the loop has already recorded reconnecting.
    let epoch = managed_background::current_phase_epoch();
    let (sender, receiver) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        if receiver.await.is_ok() {
            // The acknowledgement read waits for the starting command. Keep
            // that wait off the runtime so a suspended parent cannot stall
            // connection timers, and so shutdown can end it.
            let _ = tokio::task::spawn_blocking(move || {
                managed_background::signal_ready_for_epoch(epoch);
            })
            .await;
        }
    });
    Some(sender)
}

#[cfg(test)]
static SHUTDOWN_BRIDGES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Foreground Ctrl+C uses `process_shutdown` directly. A managed child also
/// passes its process-lifetime managed token. The connection loop calls this
/// once: each reconnect reuses the returned token, and the process token
/// itself stays uncancelled so an in-flight owner can settle.
fn bridge_shutdown(
    process_shutdown: CancellationToken,
    managed_shutdown: Option<CancellationToken>,
) -> CancellationToken {
    let Some(managed_shutdown) = managed_shutdown else {
        return process_shutdown;
    };
    #[cfg(test)]
    SHUTDOWN_BRIDGES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let connection = process_shutdown.child_token();
    let notify = connection.clone();
    tokio::spawn(async move {
        tokio::select! {
            biased;
            _ = process_shutdown.cancelled() => {}
            _ = managed_shutdown.cancelled() => notify.cancel(),
        }
    });
    connection
}

async fn run_edge_connection(
    config: &EdgeConfig,
    shutdown: CancellationToken,
) -> Result<(), Box<dyn std::error::Error>> {
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
    let establish = async {
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
            astra_edge::connect_edge_socket(request)
                .await
                .map_err(|e| {
                    tracing::error!(
                        target: "astra.edge",
                        edge_id = %config.edge_id,
                        url = %url,
                        error = %e,
                        "WebSocket connect failed"
                    );
                    e
                })?
        };
        Ok::<_, Box<dyn std::error::Error>>(ws_stream)
    };
    // No EdgeInvocationOwner exists yet, so nothing needs settling: a stalled
    // proxy/TLS/WebSocket handshake must yield to the first Ctrl+C.
    let Some(ws_stream) = establish_unless_shutdown(establish, &shutdown).await else {
        tracing::info!("Shutdown requested before the Edge connection was established");
        return Ok(());
    };
    let socket = ws_stream?;

    tracing::info!("WebSocket connected, authenticating...");

    // Send auth
    let hostname = hostname::get().ok().and_then(|h| h.into_string().ok());
    let workspace = canonical_workspace_dir(&config.workspace_dir).map_err(|e| {
        Box::new(std::io::Error::new(std::io::ErrorKind::NotFound, e)) as Box<dyn std::error::Error>
    })?;
    let auth_msg = EdgeClientMessage::Auth {
        edge_agent_id: config.edge_id.clone(),
        materialization_id: config.materialization_id.clone(),
        interaction_api_major: astra_server_types::AGENT_INTERACTION_API_MAJOR.to_string(),
        hostname,
        workspace_dir: Some(workspace.to_string_lossy().to_string()),
        capabilities: Some({
            let mut value = edge_runtime_environment_capabilities(&config.edge_id, &workspace);
            if !config.capacity_providers.is_empty() {
                value["capacity_rpc_v1"] = serde_json::to_value(
                    config
                        .capacity_providers
                        .iter()
                        .map(|provider| provider.advertisement())
                        .collect::<Vec<_>>(),
                )?;
            }
            value
        }),
    };
    let (socket, authenticated_account, _authenticated_edge_id) =
        astra_edge::authenticate_connection(socket, auth_msg, None, &shutdown).await?;
    // Only the token snapshot proven by this connection may advance persistence.
    config.token_manager.mark_proven(&token_snapshot).await;

    let session_id = format!("edge-{}", &uuid::Uuid::new_v4().to_string()[..8]);
    astra_edge::serve_connection(
        socket,
        astra_edge::EdgeConnectionContext {
            account_id: authenticated_account,
            edge_agent_id: config.edge_id.clone(),
            workspace_dir: workspace.clone(),
            journal_path: edge_invocation_journal_path_in_root(
                &config.edge_id,
                &config.server_url,
                &workspace,
                config.invocation_journal_root.clone(),
            ),
            ready: managed_ready_sender(),
            on_detach: managed_background::is_managed_child()
                .then_some(managed_background::withdraw_connection_readiness as fn()),
        },
        Arc::new(DefaultInvocationExecutor::new(
            &workspace,
            &config.edge_id,
            &config.materialization_id,
            session_id,
            config.capacity_providers.clone(),
        )),
        shutdown,
    )
    .await
    .map_err(|error| error as Box<dyn std::error::Error>)
}

struct DefaultInvocationExecutor {
    tools: astra_tools::executor::DefaultToolExecutor,
    providers: Vec<capacity_provider::LocalCapacityProvider>,
    edge_id: String,
    materialization_id: String,
}

impl DefaultInvocationExecutor {
    fn new(
        workspace: &Path,
        edge_id: &str,
        materialization_id: &str,
        session_id: String,
        providers: Vec<capacity_provider::LocalCapacityProvider>,
    ) -> Self {
        let mut executor = astra_tools::executor::DefaultToolExecutor::for_workspace(
            workspace, edge_id, session_id,
        )
        .with_local_network(astra_tools::web_fetch::configured_fake_ip_policy())
        .with_bash_preflight(astra_turn_core::permission::path_sensitivity::bash_path_preflight);
        if let Ok(root) = std::fs::File::open(workspace) {
            executor = executor.with_protected_write_workspace(std::sync::Arc::new(root));
        }
        Self {
            tools: executor,
            providers,
            edge_id: edge_id.to_owned(),
            materialization_id: materialization_id.to_owned(),
        }
    }
}

impl astra_edge::EdgeInvocationExecutor for DefaultInvocationExecutor {
    fn execute_capacity(
        &self,
        provider_id: String,
        payload: astra_server_types::edge_ws_protocol::SealedCapacityPayload,
        timeout_secs: u64,
        cancel: CancellationToken,
    ) -> futures_util::future::BoxFuture<
        '_,
        Result<
            astra_server_types::edge_ws_protocol::SealedCapacityPayload,
            astra_server_types::edge_ws_protocol::CapacityRpcFailure,
        >,
    > {
        Box::pin(async move {
            let Some(provider) = self
                .providers
                .iter()
                .find(|provider| provider.advertisement().id == provider_id)
            else {
                return Err(astra_server_types::edge_ws_protocol::CapacityRpcFailure::Unsupported);
            };
            provider.call(payload, timeout_secs, cancel).await
        })
    }

    fn execute(
        &self,
        invocation: astra_edge::EdgeInvocation,
        cancel: CancellationToken,
    ) -> futures_util::future::BoxFuture<'_, astra_tools::ToolResult> {
        Box::pin(async move {
            if invocation.execution_ceiling.is_some() {
                return astra_tools::ToolResult::error(
                    "This executor cannot enforce the admitted execution grant".into(),
                );
            }
            let approval = invocation.approved_bash_write.as_deref();
            if approval.is_some_and(|scope| {
                invocation.tool != "bash"
                    || scope.identity != invocation.identity
                    || scope.executor_id != self.edge_id
                    || scope.materialization_id.as_deref() != Some(self.materialization_id.as_str())
                    || scope.arguments_hash
                        != astra_turn_types::canonical_public_arguments_hash(&invocation.args)
            }) {
                return astra_tools::ToolResult::error(
                    "Protected-write approval does not match this Runner invocation; the command was not run.".into(),
                )
                .with_failure_evidence(astra_core::ToolFailureEvidence::from_error_kind(astra_core::ErrorKind::PolicyDenied))
                .with_execution_not_started();
            }
            if let Some(authorization) = invocation.runtime_process_authorization.as_deref() {
                runtime_process_authorization::execute_bash(
                    &self.tools,
                    &invocation.args,
                    authorization,
                    &runtime_process_authorization::CommandTraceContext {
                        traceparent: invocation.traceparent.clone(),
                        tracestate: invocation.tracestate.clone(),
                    },
                    &cancel,
                    approval,
                )
                .await
            } else if let Some(approval) = approval {
                self.tools
                    .execute_bash_with_approval(&invocation.args, approval, &cancel)
                    .await
            } else {
                astra_tools::ToolExecutor::execute_with_cancel(
                    &self.tools,
                    &invocation.tool,
                    &invocation.args,
                    Some(&cancel),
                )
                .await
            }
        })
    }
}

// ─── Main ────────────────────────────────────────────────────────────────────

/// Reject argv that the canonical parser rejects, before a managed connect
/// command can hand a credential to a running runner or start one.
pub(crate) fn validate_managed_command(args: &[String]) -> Result<(), i32> {
    let mut full = Vec::with_capacity(args.len() + 1);
    full.push(env!("CARGO_PKG_NAME").to_string());
    full.extend(args.iter().cloned());
    match Args::try_parse_from(full) {
        Ok(_) => Ok(()),
        Err(error) => {
            let _ = error.print();
            Err(error.exit_code())
        }
    }
}

fn main() {
    // Protocol probes stay ahead of the runtime and of background startup.
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
    let state_root = astra_core::local_state::local_state_root();
    if let Some(code) = managed_background::management_exit_code(&args, &state_root) {
        std::process::exit(code);
    }
    if managed_background::is_managed_child() {
        if let Err(error) = managed_background::prepare_child() {
            eprintln!("Error: {error}");
            std::process::exit(1);
        }
    } else if managed_background::should_detach(
        &args,
        std::io::IsTerminal::is_terminal(&std::io::stdin()),
    ) {
        std::process::exit(managed_background::supervise(
            &executable,
            &args,
            &state_root,
        ));
    }
    runtime();
}

#[tokio::main]
async fn runtime() {
    // The release builds CLI and Edge together, unifying ring and aws-lc
    // features. Select the Edge provider before constructing any TLS client.
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Edge installs its TLS provider before any TLS client");
    // Image-managed runners have no MOI installation marker and remain outside
    // the local update lifecycle. Local managed Edge holds a lease until exit.
    let executable = std::env::current_exe().expect("current executable");
    if managed_background::is_managed_child() {
        managed_background::install_shutdown_listener();
        managed_background::spawn_log_rotator();
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
            astra_build_info::current_attestation(),
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

    // Register exactly one process-lifetime signal observer. Recreating or
    // dropping ctrl_c futures between connection phases can lose SIGINT.
    let shutdown = CancellationToken::new();
    let _shutdown_listener = tokio::spawn(observe_shutdown(
        shutdown.clone(),
        tokio::signal::ctrl_c(),
        tokio::signal::ctrl_c(),
        || std::process::exit(130),
    ));

    let handoff = managed_background::credential_handoff_dir();
    let mut exit_with_error = run_connection_loop(&config, handoff.as_deref(), shutdown).await;

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

/// Stage a credential that a later connect command handed to this runner.
/// The open connection and its admitted work are untouched; the credential is
/// used by the next attempt. One that cannot authenticate falls back to the
/// working credential through the token manager's one-shot swap.
async fn adopt_handed_off_credential(config: &EdgeConfig, handoff: Option<&Path>) -> Handoff {
    let Some(dir) = handoff else {
        return Handoff::None;
    };
    match managed_background::claim_pending_credential(dir) {
        Ok(Some(token)) => {
            let changed = config.token_manager.stage_unproven_candidate(token).await;
            managed_background::finish_claim(dir);
            if changed {
                Handoff::Adopted
            } else {
                Handoff::None
            }
        }
        Ok(None) => Handoff::None,
        Err(error) => {
            tracing::warn!(
                target: "astra.edge",
                error = %error,
                "cannot read the credential handed to this runner; it is kept for the next attempt"
            );
            Handoff::Unreadable
        }
    }
}

#[derive(PartialEq, Eq)]
enum Handoff {
    None,
    Adopted,
    /// A credential was handed over but could not be read yet.
    Unreadable,
}

/// Returns true when the runner must exit with an error.
async fn run_connection_loop(
    config: &EdgeConfig,
    handoff: Option<&Path>,
    shutdown: CancellationToken,
) -> bool {
    let managed =
        managed_background::is_managed_child().then(managed_background::connection_shutdown);
    connection_loop(config, handoff, shutdown, managed, 1, None).await
}

async fn connection_loop(
    config: &EdgeConfig,
    handoff: Option<&Path>,
    shutdown: CancellationToken,
    managed: Option<CancellationToken>,
    mut reconnect_delay_secs: u64,
    max_attempts: Option<usize>,
) -> bool {
    let connection_shutdown = bridge_shutdown(shutdown.clone(), managed.clone());
    let mut attempts = 0usize;
    let max_reconnect_delay_secs: u64 = 60;
    loop {
        attempts += 1;
        if max_attempts.is_some_and(|limit| attempts > limit) {
            return false;
        }
        if managed_background::shutdown_requested() {
            return false;
        }
        adopt_handed_off_credential(config, handoff).await;
        let edge_span = tracing::info_span!(
            "edge.agent",
            edge_id = %config.edge_id,
            server_url = %config.server_url,
        );
        let connection =
            run_edge_connection(config, connection_shutdown.clone()).instrument(edge_span);
        tokio::pin!(connection);
        let mut startup_lost = false;
        let connect_result = loop {
            tokio::select! {
                biased;
                // Keep polling the connection after this fires. Dropping it
                // abandons admitted work instead of letting its owner settle.
                _ = managed_background::until_starting_command_lost(), if !startup_lost => {
                    startup_lost = true;
                    managed_background::shutdown_token().cancel();
                }
                result = await_connection_result(&mut connection, &shutdown) => break result,
            }
        };
        if startup_lost {
            eprintln!("Error: the starting command is gone; runner startup was not accepted.");
            return true;
        }
        let Some(connect_result) = connect_result else {
            return false;
        };
        match connect_result {
            Ok(()) => {
                reconnect_delay_secs = 1; // reset on clean disconnect
                if shutdown.is_cancelled()
                    || !config.reconnect
                    || managed_background::shutdown_requested()
                {
                    return false;
                }
                if abandon_unaccepted("reconnecting") {
                    return true;
                }
                tracing::info!(
                    delay = reconnect_delay_secs,
                    "Disconnected, reconnecting..."
                );
            }
            Err(e) => {
                if shutdown.is_cancelled() || managed_background::shutdown_requested() {
                    return false;
                }
                if is_permanent_connection_error(e.as_ref()) {
                    // A connect command either hands its credential over
                    // before this decision or, once the lock is released by
                    // exit, starts a new runner itself.
                    let handoff_lock = managed_background::lock_credential_handoff(handoff);
                    match adopt_handed_off_credential(config, handoff).await {
                        Handoff::Adopted => {
                            tracing::warn!(
                                error = %e,
                                "Authentication failed — retrying with the credential handed to this runner"
                            );
                            reconnect_delay_secs = 1;
                            continue;
                        }
                        // Never exit while an accepted handoff is untried.
                        Handoff::Unreadable => {
                            drop(handoff_lock);
                            tracing::warn!(
                                error = %e,
                                delay = reconnect_delay_secs,
                                "Authentication failed — retrying once the handed-off credential is readable"
                            );
                            if abandon_unaccepted("reconnecting") {
                                return true;
                            }
                        }
                        Handoff::None => {
                            // The previous credential may be revoked (e.g. a
                            // renewal rotated it away but the persist was
                            // lost, or a handed-off candidate was rejected).
                            // Before giving up, try the credential that last
                            // authenticated, or the other startup candidate.
                            if config.token_manager.swap_to_fallback().await {
                                tracing::warn!(
                                    error = %e,
                                    "Authentication failed — retrying with the fallback credential"
                                );
                                reconnect_delay_secs = 1;
                                continue;
                            }
                            tracing::error!(
                                error = %e,
                                "Permanent authentication failure — not retrying"
                            );
                            managed_background::note_phase("authentication-failure");
                            if let Some(lock) = handoff_lock {
                                lock.keep_until_exit();
                            }
                            return true;
                        }
                    }
                } else {
                    tracing::error!(error = %e, "Connection error");
                    if !config.reconnect {
                        return true;
                    }
                    if abandon_unaccepted("reconnecting") {
                        return true;
                    }
                    tracing::info!(delay = reconnect_delay_secs, "Reconnecting...");
                }
            }
        }
        // Exponential backoff with jitter
        // jitter in [0.5*delay, 1.5*delay) — spreads out thundering herd
        let jitter = reconnect_delay_secs as f64 * (0.5 + fastrand::f64());
        let managed_shutdown = managed
            .clone()
            .unwrap_or_else(managed_background::connection_shutdown);
        tokio::select! {
            biased;
            _ = managed_background::until_starting_command_lost() => {
                // Same signal as a loss during the connection. Cancel first
                // so a readiness wait still in progress can leave, then return
                // to the process runtime instead of exiting in this select.
                managed_background::shutdown_token().cancel();
                eprintln!(
                    "Error: the starting command is gone; runner startup was not accepted."
                );
                return true;
            }
            _ = managed_shutdown.cancelled() => return false,
            ready = wait_for_reconnect(Duration::from_secs_f64(jitter), &shutdown) => {
                if !ready {
                    return false;
                }
            }
        }
        reconnect_delay_secs = (reconnect_delay_secs * 2).min(max_reconnect_delay_secs);
    }
}

fn abandon_unaccepted(phase: &str) -> bool {
    managed_background::note_phase(phase);
    if managed_background::readiness_abandoned() {
        eprintln!("Error: the connection ended before startup was accepted.");
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shutdown_signal_cancels_the_process_lifetime_token() {
        let shutdown = CancellationToken::new();
        let listener = tokio::spawn(observe_shutdown(
            shutdown.clone(),
            std::future::ready(Ok(())),
            std::future::pending(),
            || panic!("second interrupt should not run"),
        ));
        tokio::time::timeout(Duration::from_secs(1), shutdown.cancelled())
            .await
            .expect("first signal should cancel the token");
        listener.abort();
    }

    #[tokio::test]
    async fn signal_registration_failure_does_not_cancel_or_skip_backoff() {
        let shutdown = CancellationToken::new();
        observe_shutdown(
            shutdown.clone(),
            std::future::ready(Err(std::io::Error::other("signal registration failed"))),
            std::future::pending(),
            || panic!("second interrupt should not run"),
        )
        .await;
        assert!(!shutdown.is_cancelled());
        assert!(wait_for_reconnect(Duration::ZERO, &shutdown).await);
    }

    #[tokio::test]
    async fn second_ctrl_c_escalates_while_blocking_work_is_still_running() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let shutdown = CancellationToken::new();
        let escalated = Arc::new(AtomicBool::new(false));
        let (second_tx, second_rx) = tokio::sync::oneshot::channel::<()>();
        let flag = escalated.clone();
        let listener = tokio::spawn(observe_shutdown(
            shutdown.clone(),
            std::future::ready(Ok(())),
            async move {
                second_rx
                    .await
                    .map_err(|_| std::io::Error::other("second signal dropped"))
            },
            move || {
                flag.store(true, Ordering::SeqCst);
            },
        ));
        tokio::time::timeout(Duration::from_secs(1), shutdown.cancelled())
            .await
            .expect("first signal should cancel the token");
        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let blocking = tokio::task::spawn_blocking(move || {
            let _ = started_tx.send(());
            // Dropping the sender also releases this task on an assertion failure.
            let _ = release_rx.blocking_recv();
        });
        tokio::time::timeout(Duration::from_secs(1), started_rx)
            .await
            .expect("blocking work should start")
            .expect("blocking task should report its start");
        second_tx
            .send(())
            .expect("second signal receiver is still live");
        tokio::time::timeout(Duration::from_secs(1), listener)
            .await
            .expect("second Ctrl+C should escalate without waiting for blocking work")
            .expect("listener task should finish");
        assert!(escalated.load(Ordering::SeqCst));
        assert!(
            !blocking.is_finished(),
            "blocking work must still be running"
        );
        drop(release_tx);
        tokio::time::timeout(Duration::from_secs(1), blocking)
            .await
            .expect("blocking work should exit after release")
            .expect("blocking task should finish");
    }

    #[tokio::test]
    async fn shutdown_waits_for_slow_settlement_without_a_wall_clock_drop() {
        let shutdown = CancellationToken::new();
        let connection = async {
            shutdown.cancelled().await;
            tokio::time::sleep(Duration::from_millis(250)).await;
            Ok::<(), Box<dyn std::error::Error>>(())
        };
        tokio::pin!(connection);
        let started = std::time::Instant::now();
        shutdown.cancel();
        let result = await_connection_result(&mut connection, &shutdown).await;
        assert!(result.is_none());
        assert!(started.elapsed() >= Duration::from_millis(250));
    }

    #[tokio::test]
    async fn shutdown_settlement_errors_are_returned_instead_of_discarded() {
        let shutdown = CancellationToken::new();
        let connection = async {
            shutdown.cancelled().await;
            Err::<(), Box<dyn std::error::Error>>("journal sync failed".into())
        };
        tokio::pin!(connection);
        shutdown.cancel();
        let result = await_connection_result(&mut connection, &shutdown).await;
        let error = result.expect("settlement error should be observed");
        assert!(error.is_err());
        assert!(format!("{}", error.unwrap_err()).contains("journal sync failed"));
    }

    #[tokio::test]
    async fn repeated_connection_failures_share_one_shutdown_bridge() {
        let workspace = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let refused = listener.local_addr().unwrap();
        drop(listener);
        let mut config = edge_config(
            format!("ws://{refused}/edge/ws"),
            "runner-bridge",
            workspace.path(),
            state.path(),
            "bridge-token",
        );
        config.reconnect = true;
        let process = CancellationToken::new();
        let managed = CancellationToken::new();
        let before = SHUTDOWN_BRIDGES.load(std::sync::atomic::Ordering::Relaxed);
        let exit_with_error = connection_loop(
            &config,
            None,
            process.clone(),
            Some(managed.clone()),
            0,
            Some(8),
        )
        .await;
        assert!(!exit_with_error);
        assert!(!process.is_cancelled());
        assert!(!managed.is_cancelled());
        assert_eq!(
            SHUTDOWN_BRIDGES.load(std::sync::atomic::Ordering::Relaxed),
            before + 1
        );

        let waiting = bridge_shutdown(process.clone(), Some(managed.clone()));
        let observed = tokio::spawn(async move { waiting.cancelled().await });
        managed.cancel();
        tokio::time::timeout(Duration::from_secs(1), observed)
            .await
            .expect("managed shutdown should reach the shared connection token")
            .unwrap();
        assert!(!process.is_cancelled());

        let process = CancellationToken::new();
        let managed = CancellationToken::new();
        let connection = bridge_shutdown(process.clone(), Some(managed.clone()));
        process.cancel();
        tokio::time::timeout(Duration::from_secs(1), connection.cancelled())
            .await
            .expect("process shutdown should reach the connection token");
        assert!(!managed.is_cancelled());
    }

    #[tokio::test]
    async fn first_shutdown_cancels_a_stalled_websocket_handshake() {
        let workspace = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        // The peer accepts TCP but never answers the WebSocket upgrade.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = EdgeConfig {
            capacity_providers: Vec::new(),
            server_url: format!("ws://{}/edge/ws", listener.local_addr().unwrap()),
            token_manager: token_manager::TokenManager::new(
                "test-token".into(),
                None,
                state.path().join("token"),
            ),
            workspace_dir: workspace.path().to_owned(),
            edge_id: "handshake-test".into(),
            materialization_id: "handshake-materialization".into(),
            reconnect: false,
            invocation_journal_root: Some(state.path().to_owned()),
        };
        let held = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(30)).await;
            drop(stream);
        });
        let shutdown = CancellationToken::new();
        let connection = run_edge_connection(&config, shutdown.clone());
        tokio::pin!(connection);
        // Let the attempt reach the pending upgrade before signalling.
        assert!(
            tokio::time::timeout(Duration::from_millis(200), &mut connection)
                .await
                .is_err(),
            "handshake should still be pending"
        );
        shutdown.cancel();
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            await_connection_result(&mut connection, &shutdown),
        )
        .await
        .expect("first shutdown must end a stalled handshake");
        assert!(result.is_none());
        held.abort();
    }

    #[tokio::test]
    async fn reconnect_backoff_stops_when_shutdown_arrives() {
        let shutdown = CancellationToken::new();
        shutdown.cancel();
        assert!(!wait_for_reconnect(Duration::from_secs(60), &shutdown).await);
    }

    #[tokio::test]
    async fn connection_errors_cancel_and_reap_parallel_tools_before_returning() {
        assert_connection_cleanup(false).await;
    }

    #[tokio::test]
    async fn standalone_runner_applies_bash_path_preflight_with_or_without_process_grant() {
        use astra_edge::EdgeInvocationExecutor;
        use astra_server_types::edge_ws_protocol::{
            RuntimeProcessAuthorizationContext, ToolInvocationIdentity,
        };
        use astra_services::SessionArtifactStore;

        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path();
        let _sessions =
            astra_services::session_journal::JournalDirGuard::new(root.join(".astra/sessions"));
        let artifact = astra_services::local_session_artifact_store()
            .session_path("session-1", "tool-results/call_abc.txt")
            .unwrap();
        std::fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        std::fs::write(&artifact, "synthetic result").unwrap();
        let print_artifact = format!("printf '%s' '{}'", artifact.display());
        let read_artifact = format!("cat '{}'", artifact.display());
        let artifact_label = artifact.to_str().unwrap();
        std::fs::create_dir(root.join(".claude")).unwrap();
        std::fs::write(root.join(".claude/settings.json"), "fixture").unwrap();
        std::fs::write(root.join(".env"), "synthetic source fixture").unwrap();
        std::fs::write(root.join(".ENV"), "synthetic uppercase fixture").unwrap();
        std::fs::write(root.join("README.md"), "mentions .env\n").unwrap();
        std::fs::create_dir(root.join("docs")).unwrap();
        std::fs::write(root.join("docs/guide.txt"), "mentions .env\n").unwrap();
        std::fs::write(root.join("docs/.env"), "synthetic source fixture").unwrap();
        for args in [
            &["init", "-q"][..],
            &["add", "-f", "--", ".claude"],
            &[
                "-c",
                "user.name=Astra Test",
                "-c",
                "user.email=astra@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-qm",
                "fixture .env",
            ],
        ] {
            let result = std::process::Command::new("git")
                .args(args)
                .current_dir(root)
                .output()
                .unwrap();
            assert!(result.status.success(), "{args:?}: {result:?}");
        }
        let executor = DefaultInvocationExecutor::new(
            root,
            "edge",
            "materialization",
            "session".into(),
            Vec::new(),
        );
        for authorized in [false, true] {
            let mut cases = vec![
                ("git log --format=%s -- .claude", Ok("fixture .env")),
                (
                    "for rev in HEAD; do git log --format=%s \"$rev\" -- .claude; done",
                    Ok("fixture .env"),
                ),
                ("grep -F .env README.md", Ok("mentions .env")),
                ("grep -r -F .env docs", Ok("docs/guide.txt:mentions .env")),
                ("echo .env | head -n 1", Ok(".env")),
                ("printf '%s\\n' '>' .env | head -n 2", Ok(">\n.env")),
                (
                    "printf '%s\\n' .env > output.txt",
                    Ok("(command completed with no output)"),
                ),
                ("echo /etc/example | head -n 1", Ok("/etc/example")),
                ("awk 'BEGIN { print \"/etc/example\" }'", Ok("/etc/example")),
                ("awk 'BEGIN { print \".env\" }'", Ok(".env")),
                (print_artifact.as_str(), Ok(artifact_label)),
                (read_artifact.as_str(), Err("owned tool-result artifact")),
                ("f=README.md; cat \"$f\"", Ok("mentions .env")),
                ("sort .env", Err("blocked the read operand")),
                ("sort --files0-from=.env", Err("blocked the read operand")),
                ("sort --files=.env", Err("blocked the read operand")),
                ("sort -o.env README.md", Err("blocked the write operand")),
                (
                    "sort --output=.env README.md",
                    Err("blocked the write operand"),
                ),
                ("tree -H . --hintro=.env", Err("blocked the read operand")),
                ("tree -o .env .", Err("blocked the write operand")),
                ("find -files0-from .env", Err("blocked the read operand")),
                (
                    "find README.md -maxdepth 0 -exec cat {} \\;",
                    Ok("mentions .env"),
                ),
                (
                    "find . -fprint \"$destination\"",
                    Err("could not establish"),
                ),
                (
                    "find README.md -files0-from paths -exec cat {} \\;",
                    Err("could not establish"),
                ),
                ("find . -name .env -delete", Err("could not establish")),
                (
                    "find . -name .env -exec cat {} \\;",
                    Err("could not establish"),
                ),
                ("find . -name .env -exec {} \\;", Err("could not establish")),
                ("find . -exec cat {} \\;", Err("could not establish")),
                (
                    "git config --file=.env --list",
                    Err("blocked the read operand"),
                ),
                (
                    "git -C . config --fil=.env --list",
                    Err("blocked the read operand"),
                ),
                (
                    "git config --file=.claude/settings.json --add synthetic.key marker",
                    Err("blocked the write operand"),
                ),
                ("uniq .env", Err("blocked the read operand")),
                ("cut -c 1- .env", Err("blocked the read operand")),
                ("paste .env", Err("blocked the read operand")),
                ("wc --files0-from=.env", Err("blocked the read operand")),
                ("file -f.env", Err("blocked the read operand")),
                ("date --file=.env", Err("blocked the read operand")),
                ("nl .env", Err("blocked the read operand")),
                (
                    "sort -T .claude README.md",
                    Err("blocked the write operand"),
                ),
                ("echo .env; cat .ENV", Err("blocked the read operand")),
                ("awk '{print $1}' .env", Err("could not establish")),
                ("printf x > .env", Err("blocked the write operand")),
                ("cp .env output.txt", Err("blocked the read operand")),
                (
                    "grep -r --include .env needle docs",
                    Err("could not establish"),
                ),
                (
                    "rsync -r --include .env --exclude '*' docs/ out/",
                    Err("could not establish"),
                ),
                (
                    "rsync -r --include=.env --exclude='*' docs/ out/",
                    Err("could not establish"),
                ),
                (
                    "printf changed > .claude/settings.json; touch marker",
                    Err("blocked the write operand"),
                ),
                (
                    "grep needle .env; touch marker",
                    Err("blocked the read operand"),
                ),
                (
                    "grep -f .env README.md; touch marker",
                    Err("blocked the read operand"),
                ),
                (
                    "git log --grep .env -- .env; touch marker",
                    Err("blocked the read operand"),
                ),
            ];
            if std::process::Command::new("rg")
                .arg("--version")
                .output()
                .is_ok()
            {
                cases.push((
                    "rg -g '!*.env' mentions docs",
                    Ok("docs/guide.txt:mentions .env"),
                ));
                cases.push((
                    "rg -g '!*.env' -g '*.env' mentions docs",
                    Err("could not establish"),
                ));
            }
            for (command, expected) in cases {
                let (_, input_rx) = tokio::sync::mpsc::channel(1);
                let result = executor
                    .execute(
                        astra_edge::EdgeInvocation {
                            approved_bash_write: None,
                            identity: ToolInvocationIdentity::new(
                                "user", "session", "run", "chain", "call",
                            )
                            .unwrap(),
                            delivery_generation: 1,
                            tool: "bash".into(),
                            args: serde_json::json!({"command":command}),
                            execution_deadline: None,
                            command_timeout_cap_ms: None,
                            execution_ceiling: None,
                            runtime_process_authorization: authorized.then(|| {
                                Box::new(RuntimeProcessAuthorizationContext {
                                    authorization: "Bearer synthetic-grant".into(),
                                })
                            }),
                            traceparent: None,
                            tracestate: None,
                            input_rx,
                        },
                        CancellationToken::new(),
                    )
                    .await;
                if let Err(expected) = expected {
                    assert!(result.is_error, "{command}: {result:?}");
                    assert!(result.output.contains(expected), "{command}: {result:?}");
                    assert!(!result.output.contains("synthetic-grant"), "{result:?}");
                    let facts = result.metadata.unwrap();
                    assert_eq!(facts["error_kind"], "policy_denied");
                    assert_eq!(facts["disposition"], "rejected");
                    assert_eq!(facts["execution_started"], false);
                    assert_eq!(facts["side_effects_maybe"], false);
                } else {
                    assert!(!result.is_error, "{command}: {result:?}");
                    assert_eq!(result.output.trim(), expected.unwrap(), "{command}");
                    if command == "printf '%s\\n' .env > output.txt" {
                        assert_eq!(
                            std::fs::read_to_string(root.join("output.txt")).unwrap(),
                            ".env\n"
                        );
                    }
                    assert!(
                        result
                            .metadata
                            .as_ref()
                            .is_none_or(|metadata| { !metadata.contains_key("source_preimage") }),
                        "{command}: {result:?}"
                    );
                }
                assert!(!root.join("marker").exists());
                assert_eq!(
                    std::fs::read_to_string(root.join(".claude/settings.json")).unwrap(),
                    "fixture"
                );
            }
        }
    }

    #[tokio::test]
    async fn runner_reads_ordinary_files_in_a_path_named_like_a_credential() {
        use astra_edge::EdgeInvocationExecutor;

        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("service.envoy");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(root.join("service.envoy")).unwrap();
        std::fs::create_dir(root.join("output.env")).unwrap();
        std::fs::create_dir(root.join("cache.pem")).unwrap();
        std::fs::create_dir_all(root.join("fixtures/etc")).unwrap();
        std::fs::create_dir(root.join("service.aws")).unwrap();
        std::fs::create_dir(root.join(".aws")).unwrap();
        for path in [
            "fixtures/etc/passwd",
            "service.aws/credentials",
            ".aws/credentials",
        ] {
            std::fs::write(root.join(path), "fixture\n").unwrap();
        }
        std::fs::write(root.join("README.md"), "ordinary fixture\n").unwrap();
        std::fs::write(root.join(".env"), "private fixture\n").unwrap();
        let executor = DefaultInvocationExecutor::new(
            &root,
            "edge",
            "materialization",
            "session".into(),
            Vec::new(),
        );
        for (command, expected) in [
            ("cat README.md", Some("ordinary fixture\n")),
            (
                "ls service.envoy",
                Some("(command completed with no output)"),
            ),
            ("ls output.env", Some("(command completed with no output)")),
            ("ls cache.pem", Some("(command completed with no output)")),
            ("cat fixtures/etc/passwd", Some("fixture\n")),
            ("cat service.aws/credentials", Some("fixture\n")),
            ("test .env && printf yes", Some("yes")),
            ("basename .env", Some(".env\n")),
            ("dirname .env", Some(".\n")),
            ("cat .env", None),
            ("cat .aws/credentials", None),
        ] {
            let (_, input_rx) = tokio::sync::mpsc::channel(1);
            let result = executor
                .execute(
                    astra_edge::EdgeInvocation {
                        identity: astra_turn_types::ToolInvocationIdentity::new(
                            "user", "session", "run", "turn", command,
                        )
                        .unwrap(),
                        delivery_generation: 1,
                        tool: "bash".into(),
                        args: serde_json::json!({"command":command}),
                        execution_deadline: None,
                        command_timeout_cap_ms: None,
                        execution_ceiling: None,
                        approved_bash_write: None,
                        runtime_process_authorization: None,
                        traceparent: None,
                        tracestate: None,
                        input_rx,
                    },
                    CancellationToken::new(),
                )
                .await;
            assert_eq!(
                !result.is_error,
                expected.is_some(),
                "{command}: {result:?}"
            );
            if let Some(expected) = expected {
                assert_eq!(result.output, expected);
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn protected_write_approval_is_bound_to_the_invocation_and_pinned_workspace() {
        use astra_edge::EdgeInvocationExecutor;
        use sha2::{Digest, Sha256};

        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("project");
        std::fs::create_dir_all(root.join(".claude")).unwrap();
        let root = root.canonicalize().unwrap();
        let executor = DefaultInvocationExecutor::new(
            &root,
            "edge",
            "materialization",
            "session".into(),
            Vec::new(),
        );
        let identity =
            astra_turn_types::ToolInvocationIdentity::new("user", "session", "run", "turn", "call")
                .unwrap();
        let args = serde_json::json!({"command":"printf approved > .claude/settings.json"});
        let mut physical = b"materialization\0".to_vec();
        physical.extend_from_slice(root.to_string_lossy().as_bytes());
        let scope = astra_turn_types::ApprovedBashWrite {
            identity: identity.clone(),
            arguments_hash: astra_turn_types::canonical_public_arguments_hash(&args),
            executor_id: "edge".into(),
            workspace_root: root.to_string_lossy().into_owned(),
            physical_workspace_id: format!(
                "edge-materialization-v1:{:x}",
                Sha256::digest(physical)
            ),
            authority: astra_turn_types::ProtectedBashWriteAuthority::NativeSession {
                execution_binding_generation: 1,
            },
            permission_revision: -1,
            working_directory: root.to_string_lossy().into_owned(),
            materialization_id: Some("materialization".into()),
        };
        let invoke = |args: serde_json::Value,
                      approval: Option<astra_turn_types::ApprovedBashWrite>| {
            let identity = identity.clone();
            let executor = &executor;
            async move {
                let (_, input_rx) = tokio::sync::mpsc::channel(1);
                executor
                    .execute(
                        astra_edge::EdgeInvocation {
                            identity,
                            delivery_generation: 1,
                            tool: "bash".into(),
                            args,
                            execution_deadline: None,
                            command_timeout_cap_ms: None,
                            execution_ceiling: None,
                            approved_bash_write: approval.map(Box::new),
                            runtime_process_authorization: None,
                            traceparent: None,
                            tracestate: None,
                            input_rx,
                        },
                        CancellationToken::new(),
                    )
                    .await
            }
        };
        assert!(invoke(args.clone(), None).await.is_error);
        let result = invoke(args.clone(), Some(scope.clone())).await;
        assert!(!result.is_error, "{result:?}");
        assert_eq!(
            std::fs::read_to_string(root.join(".claude/settings.json")).unwrap(),
            "approved"
        );
        for changed in [
            serde_json::json!({"command":"printf changed > .claude/settings.json"}),
            serde_json::json!({"command":"printf approved > .claude/other.json"}),
            serde_json::json!({"command":"printf approved > .claude/settings.json", "workdir":".."}),
        ] {
            assert!(invoke(changed, Some(scope.clone())).await.is_error);
        }
        let mut wrong_runner = scope.clone();
        wrong_runner.materialization_id = Some("other".into());
        assert!(invoke(args.clone(), Some(wrong_runner)).await.is_error);

        std::fs::write(root.join("credentials.db"), "s/approved/changed/").unwrap();
        for command in [
            "sed -f credentials.db -i .claude/settings.json",
            "touch --reference=credentials.db .claude/settings.json",
            "install -s --strip-program=credentials.db source .claude/settings.json",
            "rsync --files-from=credentials.db source .claude/settings.json",
            "sed -e 's/x/y/' -e '1r credentials.db' ordinary",
            "rsync --filter 'merge credentials.db' source ordinary",
            "sed -i.env 's/approved/changed/' .claude/settings.json",
            "sed --in-place=.env 's/approved/changed/' .claude/settings.json",
            "cp -S.env source .claude/settings.json",
            "printf changed > .claude/settings.json; cat credentials.db",
        ] {
            let sensitive_args = serde_json::json!({"command":command});
            let mut sensitive_scope = scope.clone();
            sensitive_scope.arguments_hash =
                astra_turn_types::canonical_public_arguments_hash(&sensitive_args);
            for approval in [None, Some(sensitive_scope)] {
                let result = invoke(sensitive_args.clone(), approval).await;
                assert!(result.is_error, "{command}: {result:?}");
                assert_eq!(
                    std::fs::read_to_string(root.join(".claude/settings.json")).unwrap(),
                    "approved"
                );
                assert!(!root.join(".claude/settings.json.env").exists());
            }
        }

        let backup_args = serde_json::json!({"command":format!(
            "printf '%s' .env; sed -i'{}/backup-*' 's/approved/changed/' .claude/settings.json",
            parent.path().display()
        )});
        let mut backup_scope = scope.clone();
        backup_scope.arguments_hash =
            astra_turn_types::canonical_public_arguments_hash(&backup_args);
        for approval in [None, Some(backup_scope)] {
            let result = invoke(backup_args.clone(), approval).await;
            assert!(result.is_error, "{result:?}");
            assert_eq!(
                std::fs::read_to_string(root.join(".claude/settings.json")).unwrap(),
                "approved"
            );
            assert!(!parent.path().join("backup-.claude/settings.json").exists());
        }

        std::fs::write(root.join("notes.txt"), "a").unwrap();
        let result = invoke(
            serde_json::json!({"command":"sed -i.bak 's/a/b/' notes.txt"}),
            None,
        )
        .await;
        assert!(!result.is_error, "{result:?}");
        assert_eq!(
            std::fs::read_to_string(root.join("notes.txt")).unwrap(),
            "b"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("notes.txt.bak")).unwrap(),
            "a"
        );

        #[cfg(target_os = "macos")]
        {
            let protected_args = serde_json::json!({"command":"sed -i bak 's/approved/changed/' .claude/settings.json"});
            let mut protected_scope = scope.clone();
            protected_scope.arguments_hash =
                astra_turn_types::canonical_public_arguments_hash(&protected_args);
            assert!(invoke(protected_args.clone(), None).await.is_error);
            let result = invoke(protected_args, Some(protected_scope)).await;
            assert!(!result.is_error, "{result:?}");
            assert_eq!(
                std::fs::read_to_string(root.join(".claude/settings.json")).unwrap(),
                "changed"
            );
            assert_eq!(
                std::fs::read_to_string(root.join(".claude/settings.jsonbak")).unwrap(),
                "approved"
            );
            std::fs::write(root.join(".claude/settings.json"), "approved").unwrap();

            for (name, extension) in [
                ("separate.txt", ".bak"),
                ("nondot.txt", "bak"),
                ("empty.txt", "''"),
            ] {
                std::fs::write(root.join(name), "x").unwrap();
                let result = invoke(
                    serde_json::json!({"command":format!(
                        "sed -i {extension} 's#x#../../../../outside#' {name}"
                    )}),
                    None,
                )
                .await;
                assert!(!result.is_error, "{result:?}");
                assert_eq!(
                    std::fs::read_to_string(root.join(name)).unwrap(),
                    "../../../../outside"
                );
            }
            assert_eq!(
                std::fs::read_to_string(root.join("separate.txt.bak")).unwrap(),
                "x"
            );
            assert_eq!(
                std::fs::read_to_string(root.join("nondot.txtbak")).unwrap(),
                "x"
            );

            std::fs::write(root.join("-env"), "a").unwrap();
            let result = invoke(
                serde_json::json!({"command":"sed -i .claude -- 's/a/b/' -env"}),
                None,
            )
            .await;
            assert!(!result.is_error, "{result:?}");
            assert_eq!(std::fs::read_to_string(root.join("-env")).unwrap(), "b");
            assert_eq!(
                std::fs::read_to_string(root.join("-env.claude")).unwrap(),
                "a"
            );

            let outside = parent.path().join("outside-sed.txt");
            std::fs::write(&outside, "unchanged").unwrap();
            let result = invoke(
                serde_json::json!({"command":format!(
                    "sed -i .bak 's/a/b/' {}",
                    outside.display()
                )}),
                None,
            )
            .await;
            assert!(result.is_error, "{result:?}");
            assert_eq!(std::fs::read_to_string(&outside).unwrap(), "unchanged");
        }

        let data_args = serde_json::json!({"command":"printf '%s' .env > .claude/settings.json"});
        let mut data_scope = scope.clone();
        data_scope.arguments_hash = astra_turn_types::canonical_public_arguments_hash(&data_args);
        assert!(invoke(data_args.clone(), None).await.is_error);
        let result = invoke(data_args, Some(data_scope)).await;
        assert!(!result.is_error, "{result:?}");
        assert_eq!(
            std::fs::read_to_string(root.join(".claude/settings.json")).unwrap(),
            ".env"
        );
        let outside = parent.path().join("outside.txt");
        std::fs::write(&outside, "unchanged").unwrap();
        let outside_args = serde_json::json!({"command":format!("cp --suffix -tfoo source {}", outside.display())});
        let mut outside_scope = scope.clone();
        outside_scope.arguments_hash =
            astra_turn_types::canonical_public_arguments_hash(&outside_args);
        for approval in [None, Some(outside_scope)] {
            assert!(invoke(outside_args.clone(), approval).await.is_error);
            assert_eq!(std::fs::read_to_string(&outside).unwrap(), "unchanged");
        }

        std::fs::create_dir_all(root.join("source/.claude")).unwrap();
        std::os::unix::fs::symlink("source", root.join("source-link")).unwrap();
        let alias_args = serde_json::json!({"command":"printf alias > .claude/settings.json", "workdir":"source-link"});
        let mut alias_scope = scope.clone();
        alias_scope.arguments_hash = astra_turn_types::canonical_public_arguments_hash(&alias_args);
        alias_scope.working_directory = root.join("source-link").to_string_lossy().into_owned();
        let result = invoke(alias_args, Some(alias_scope)).await;
        assert!(!result.is_error, "{result:?}");
        assert_eq!(
            std::fs::read_to_string(root.join("source/.claude/settings.json")).unwrap(),
            "alias"
        );

        std::fs::rename(&root, parent.path().join("old-project")).unwrap();
        std::fs::create_dir_all(root.join(".claude")).unwrap();
        assert!(invoke(args, Some(scope)).await.is_error);
        assert!(!root.join(".claude/settings.json").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn standalone_runner_checks_aliases_from_the_invocation_workdir() {
        use astra_edge::EdgeInvocationExecutor;
        use astra_server_types::edge_ws_protocol::ToolInvocationIdentity;
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().unwrap();
        let nested = workspace.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(workspace.path().join(".env"), "synthetic secret").unwrap();
        std::fs::write(nested.join("ordinary"), "ordinary").unwrap();
        symlink("../.env", nested.join("alias")).unwrap();
        let executor = DefaultInvocationExecutor::new(
            workspace.path(),
            "edge",
            "materialization",
            "session".into(),
            Vec::new(),
        );

        for (command, allowed) in [("cat ordinary", true), ("cat alias", false)] {
            let (_, input_rx) = tokio::sync::mpsc::channel(1);
            let result = executor
                .execute(
                    astra_edge::EdgeInvocation {
                        approved_bash_write: None,
                        identity: ToolInvocationIdentity::new(
                            "user", "session", "run", "chain", "call",
                        )
                        .unwrap(),
                        delivery_generation: 1,
                        tool: "bash".into(),
                        args: serde_json::json!({"command":command, "workdir":"nested"}),
                        execution_deadline: None,
                        command_timeout_cap_ms: None,
                        execution_ceiling: None,
                        runtime_process_authorization: None,
                        traceparent: None,
                        tracestate: None,
                        input_rx,
                    },
                    CancellationToken::new(),
                )
                .await;
            if allowed {
                assert!(!result.is_error, "{result:?}");
                assert_eq!(result.output.trim(), "ordinary");
            } else {
                assert!(result.is_error, "{result:?}");
                assert!(result.output.contains("sensitive credential"), "{result:?}");
                assert_eq!(result.metadata.unwrap()["execution_started"], false);
            }
        }
    }

    #[tokio::test]
    async fn standalone_runner_checks_source_preimages_with_path_policy() {
        use astra_edge::EdgeInvocationExecutor;
        use astra_server_types::edge_ws_protocol::{
            RuntimeProcessAuthorizationContext, ToolInvocationIdentity,
        };

        // Give the capture store a private directory without changing the
        // environment of other concurrently running tests.
        const CHILD: &str = "ASTRA_TEST_DECLARED_SOURCES_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let store = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "tests::standalone_runner_checks_source_preimages_with_path_policy",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env("_ASTRA_SOURCE_PREIMAGE_ROOT", store.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let mut pending = vec![store.path().to_path_buf()];
            let mut files = 0;
            while let Some(directory) = pending.pop() {
                for entry in std::fs::read_dir(directory).unwrap() {
                    let entry = entry.unwrap();
                    if entry.file_type().unwrap().is_dir() {
                        pending.push(entry.path());
                    } else {
                        files += 1;
                        let bytes = std::fs::read(entry.path()).unwrap();
                        assert!(
                            !bytes
                                .windows(b"synthetic fixture".len())
                                .any(|part| part == b"synthetic fixture")
                        );
                    }
                }
            }
            assert!(files > 0, "the safe source capture did not run");
            return;
        }

        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("README.md"), "mentions .env\n").unwrap();
        std::fs::write(workspace.path().join("id_dsa"), "synthetic fixture").unwrap();
        std::fs::write(workspace.path().join(".ENV"), "synthetic fixture").unwrap();
        std::fs::write(workspace.path().join("notes"), "ordinary").unwrap();
        std::fs::write(workspace.path().join("notes.key"), "synthetic fixture").unwrap();
        std::fs::write(workspace.path().join("secrets.yaml"), "synthetic fixture").unwrap();
        std::fs::create_dir_all(workspace.path().join("src/root")).unwrap();
        std::fs::create_dir_all(workspace.path().join("docs/etc")).unwrap();
        std::fs::write(workspace.path().join("src/root/report.txt"), "ordinary").unwrap();
        std::fs::write(workspace.path().join("docs/etc/guide.txt"), "ordinary").unwrap();
        let executor = DefaultInvocationExecutor::new(
            workspace.path(),
            "edge",
            "materialization",
            "session".into(),
            Vec::new(),
        );
        for authorized in [false, true] {
            for (args, expected) in [
                (
                    serde_json::json!({"command":"grep -F .env README.md", "source_artifacts":[" README.md ", "src/root/report.txt", "docs/etc/guide.txt"]}),
                    Ok(("mentions .env", true)),
                ),
                (
                    serde_json::json!({"command":"uniq README.md"}),
                    Ok(("mentions .env", true)),
                ),
                (
                    serde_json::json!({"command":"true", "source_artifacts":[" id_dsa "]}),
                    Err("Source-artifact preflight"),
                ),
                (
                    serde_json::json!({"command":"true", "source_artifacts":[".ENV"]}),
                    Err("Source-artifact preflight"),
                ),
                (
                    serde_json::json!({"command":"true", "source_artifacts":["../outside"]}),
                    Err("relative to the workspace root"),
                ),
                (
                    serde_json::json!({"command":"cp README.md safe-copy"}),
                    Ok(("(command completed with no output)", true)),
                ),
                (
                    serde_json::json!({"command":"cp notes output"}),
                    Ok(("(command completed with no output)", false)),
                ),
                (
                    serde_json::json!({"command":"echo secrets.yaml"}),
                    Ok(("secrets.yaml", false)),
                ),
            ] {
                let (_, input_rx) = tokio::sync::mpsc::channel(1);
                let result = executor
                    .execute(
                        astra_edge::EdgeInvocation {
                            approved_bash_write: None,
                            identity: ToolInvocationIdentity::new(
                                "user", "session", "run", "chain", "call",
                            )
                            .unwrap(),
                            delivery_generation: 1,
                            tool: "bash".into(),
                            args,
                            execution_deadline: None,
                            command_timeout_cap_ms: None,
                            execution_ceiling: None,
                            runtime_process_authorization: authorized.then(|| {
                                Box::new(RuntimeProcessAuthorizationContext {
                                    authorization: "Bearer synthetic-grant".into(),
                                })
                            }),
                            traceparent: None,
                            tracestate: None,
                            input_rx,
                        },
                        CancellationToken::new(),
                    )
                    .await;
                match expected {
                    Err(message) => {
                        assert!(
                            result.is_error && result.output.contains(message),
                            "{result:?}"
                        );
                        assert_eq!(result.metadata.unwrap()["execution_started"], false);
                    }
                    Ok((output, receipt)) => {
                        assert!(!result.is_error, "{result:?}");
                        assert_eq!(result.output.trim(), output);
                        assert_eq!(
                            result
                                .metadata
                                .as_ref()
                                .is_some_and(|fields| fields.contains_key("source_preimage")),
                            receipt,
                            "{result:?}"
                        );
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn websocket_expired_work_budget_never_dispatches_and_replays_durable_rejection() {
        let workspace = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = EdgeConfig {
            capacity_providers: Vec::new(),
            server_url: format!("ws://{}/edge/ws", listener.local_addr().unwrap()),
            token_manager: token_manager::TokenManager::new(
                "test-token".into(),
                None,
                state.path().join("token"),
            ),
            workspace_dir: workspace.path().to_owned(),
            edge_id: "budget-test".into(),
            materialization_id: "budget-materialization".into(),
            reconnect: false,
            invocation_journal_root: Some(state.path().to_owned()),
        };
        let server = async {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            assert!(ws.next().await.unwrap().unwrap().is_text());
            ws.send(Message::Text(
                serde_json::to_string(&EdgeServerMessage::AuthOk {
                    user_id: "user".into(),
                    edge_id: "ws-budget-test".into(),
                    interaction_api_major: astra_server_types::AGENT_INTERACTION_API_MAJOR.into(),
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
            let identity = astra_server_types::edge_ws_protocol::ToolInvocationIdentity::new(
                "user",
                "session",
                "run",
                "chain",
                "expired-call",
            )
            .unwrap();
            let request = EdgeServerMessage::ToolRequest {
                approved_bash_write: None,
                request_id: identity.storage_key(),
                identity: Box::new(identity.clone()),
                delivery_generation: 1,
                tool: "bash".into(),
                args: serde_json::json!({"command":"touch must-not-exist"}),
                runtime_process_authorization: None,
                runtime_process_authorization_required: false,
                timeout_secs: Some(30),
                execution_deadline_unix_ms: Some(1),
                execution_timeout_ms: Some(86_400_000),
                command_timeout_cap_ms: None,
                execution_ceiling: None,

                traceparent: None,
                tracestate: None,
            };
            let mut original = None;
            for _ in 0..2 {
                ws.send(Message::Text(
                    serde_json::to_string(&request).unwrap().into(),
                ))
                .await
                .unwrap();
                loop {
                    let frame = ws.next().await.unwrap().unwrap();
                    if !frame.is_text() {
                        continue;
                    }
                    let message: EdgeClientMessage =
                        serde_json::from_str(frame.to_text().unwrap()).unwrap();
                    if let EdgeClientMessage::ToolResult {
                        identity: returned,
                        is_error,
                        tool_result_fields,
                        output,
                        ..
                    } = message
                    {
                        assert_eq!(returned, identity);
                        assert!(is_error);
                        let fields = tool_result_fields.unwrap();
                        assert_eq!(fields["execution_started"], false);
                        assert_eq!(fields["execution_fact"], "not_executed");
                        assert_eq!(fields["error_kind"], "tool_timeout");
                        let rejection = (output, fields);
                        if let Some(previous) = &original {
                            assert_eq!(previous, &rejection);
                        } else {
                            original = Some(rejection);
                        }
                        break;
                    }
                }
                assert!(!workspace.path().join("must-not-exist").exists());
            }
            ws.send(Message::Text(
                serde_json::to_string(&EdgeServerMessage::Closing {
                    reason: "fixture complete".into(),
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        };
        let (_, result) = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(
                server,
                run_edge_connection(&config, CancellationToken::new())
            )
        })
        .await
        .unwrap();
        result.unwrap();
        assert!(!workspace.path().join("must-not-exist").exists());
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
            capacity_providers: Vec::new(),
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
        };
        let server = async {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            assert!(ws.next().await.unwrap().unwrap().is_text());
            ws.send(Message::Text(
                serde_json::to_string(&EdgeServerMessage::AuthOk {
                    user_id: "user".into(),
                    edge_id: "ws-cleanup-test".into(),
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
            approved_bash_write: None,
                    request_id: identity.storage_key(), identity: Box::new(identity), delivery_generation: 1,
                    // Keep both admitted commands active until the malformed
                    // request forces the connection down.  A short sleep made
                    // this test depend on CI scheduling: one command could
                    // complete successfully before the failure was observed,
                    // making the recovery assertion nondeterministic.
                    tool: "bash".into(), args: serde_json::json!({"command": format!("touch started-{i}; sleep 30; touch leaked-{i}")}),
                    runtime_process_authorization: managed.then(|| Box::new(astra_server_types::edge_ws_protocol::RuntimeProcessAuthorizationContext { authorization: "Bearer test-grant".into() })), runtime_process_authorization_required: managed, timeout_secs: Some(30),
                    execution_deadline_unix_ms: None,
                    execution_timeout_ms: None,
                    command_timeout_cap_ms: None,
                    execution_ceiling: None,
                    traceparent: None,
                    tracestate: None,
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
                    approved_bash_write: None,
                    request_id: identity.storage_key(),
                    identity: Box::new(identity),
                    delivery_generation: 1,
                    tool: "bash".into(),
                    args: serde_json::json!({"command":"touch should-not-run"}),
                    runtime_process_authorization: None,
                    runtime_process_authorization_required: false,
                    timeout_secs: Some(30),
                    execution_deadline_unix_ms: None,
                    execution_timeout_ms: None,
                    command_timeout_cap_ms: None,
                    execution_ceiling: None,

                    traceparent: None,
                    tracestate: None,
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
            while ws.next().await.is_some_and(|message| message.is_ok()) {}
        };
        let (_, result) = tokio::time::timeout(Duration::from_secs(15), async {
            tokio::join!(
                server,
                run_edge_connection(&config, CancellationToken::new())
            )
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
                    edge_id: "ws-recovery-test".into(),
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
            tokio::join!(
                recovery_server,
                run_edge_connection(&config, CancellationToken::new())
            )
        })
        .await
        .unwrap();
        recovered.unwrap();
        tokio::time::sleep(Duration::from_secs(3)).await;
        for name in ["leaked-0", "leaked-1", "should-not-run"] {
            assert!(
                !workspace.path().join(name).exists(),
                "orphan execution: {name}"
            );
        }
    }

    #[test]
    fn reconnect_flag_accepts_an_explicit_false_for_bounded_process_runs() {
        let args = Args::try_parse_from(["astra-edge", "--reconnect=false"])
            .expect("explicit false must be a valid bounded-run configuration");

        assert!(!args.reconnect);
    }

    #[test]
    fn interaction_contract_mismatch_is_a_permanent_edge_connection_error() {
        let error = astra_edge::EdgeAuthenticationError::IncompatibleContract;
        assert!(is_permanent_connection_error(&error));
    }

    #[test]
    fn invocation_journal_honors_the_explicit_local_state_root() {
        let root = std::env::temp_dir().join("astra-edge-isolated-state");
        let workspace = std::env::temp_dir().join("astra-edge-workspace");
        let path = edge_invocation_journal_path_in_root(
            "edge-test",
            "wss://example.test",
            &workspace,
            Some(root.clone()),
        );

        assert_eq!(path.parent().and_then(Path::parent), Some(root.as_path()));
        assert_eq!(
            path.parent().and_then(Path::file_name),
            Some(std::ffi::OsStr::new("edge-invocations"))
        );
        assert_eq!(path.extension(), Some(std::ffi::OsStr::new("json")));
        let other_server = edge_invocation_journal_path_in_root(
            "edge-test",
            "wss://other.example.test",
            &workspace,
            Some(root),
        );
        assert_ne!(path, other_server);
    }

    fn edge_config(
        server_url: String,
        edge_id: &str,
        workspace: &Path,
        state: &Path,
        token_name: &str,
    ) -> EdgeConfig {
        EdgeConfig {
            capacity_providers: Vec::new(),
            server_url,
            token_manager: token_manager::TokenManager::new(
                "test-token".into(),
                None,
                state.join(token_name),
            ),
            workspace_dir: workspace.to_path_buf(),
            edge_id: edge_id.to_string(),
            materialization_id: format!("{edge_id}-materialization"),
            reconnect: false,
            invocation_journal_root: Some(state.to_path_buf()),
        }
    }

    async fn accept_plain_socket(
        listener: &tokio::net::TcpListener,
    ) -> WebSocketStream<tokio::net::TcpStream> {
        let (stream, _) = listener.accept().await.unwrap();
        tokio_tungstenite::accept_async(stream).await.unwrap()
    }

    #[tokio::test]
    async fn two_servers_with_one_edge_id_open_distinct_journals() {
        let workspace_dir = tempfile::tempdir().unwrap();
        let workspace = workspace_dir.path().canonicalize().unwrap();
        let state_dir = tempfile::tempdir().unwrap();
        let state = state_dir.path();
        let listener_a = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let listener_b = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url_a = format!("ws://{}/edge/ws", listener_a.local_addr().unwrap());
        let url_b = format!("ws://{}/edge/ws", listener_b.local_addr().unwrap());
        let journal = |url| {
            edge_invocation_journal_path_in_root(
                "runner-abc",
                url,
                &workspace,
                Some(state.to_path_buf()),
            )
        };
        let journals = [journal(&url_a), journal(&url_b)];
        assert_ne!(journals[0], journals[1]);
        let both_open = &tokio::sync::Barrier::new(2);
        let hold = |listener: tokio::net::TcpListener| async move {
            let mut socket = accept_plain_socket(&listener).await;
            assert!(socket.next().await.unwrap().unwrap().is_text());
            socket
                .send(Message::Text(
                    serde_json::to_string(&EdgeServerMessage::AuthOk {
                        user_id: "user".into(),
                        edge_id: "runner-abc".into(),
                        interaction_api_major: astra_server_types::AGENT_INTERACTION_API_MAJOR
                            .into(),
                    })
                    .unwrap()
                    .into(),
                ))
                .await
                .unwrap();
            let heartbeat = socket.next().await.unwrap().unwrap();
            let heartbeat: EdgeClientMessage =
                serde_json::from_str(heartbeat.to_text().unwrap()).unwrap();
            assert!(matches!(heartbeat, EdgeClientMessage::Ping {}));
            both_open.wait().await;
            socket
                .send(Message::Text(
                    serde_json::to_string(&EdgeServerMessage::Closing {
                        reason: "both journals opened successfully".into(),
                    })
                    .unwrap()
                    .into(),
                ))
                .await
                .unwrap();
        };
        let config_a = edge_config(url_a, "runner-abc", &workspace, state, "server-a.token");
        let config_b = edge_config(url_b, "runner-abc", &workspace, state, "server-b.token");
        let ((), (), first, second) = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(
                hold(listener_a),
                hold(listener_b),
                run_edge_connection(&config_a, CancellationToken::new()),
                run_edge_connection(&config_b, CancellationToken::new()),
            )
        })
        .await
        .expect("the two servers did not both open their journals");
        first.unwrap();
        second.unwrap();
        for journal in journals {
            assert!(journal.with_extension("json.wal").is_file());
        }
    }

    #[tokio::test]
    async fn parent_loss_after_tool_admission_replays_cancellation() {
        if std::env::var("ASTRA_EDGE_TEST_PARENT_LOSS").ok().as_deref() == Some("1") {
            let workspace = PathBuf::from(std::env::var("ASTRA_EDGE_TEST_WORKSPACE").unwrap());
            let state = PathBuf::from(std::env::var("ASTRA_EDGE_TEST_STATE").unwrap());
            let config = edge_config(
                std::env::var("ASTRA_EDGE_TEST_SERVER").unwrap(),
                "runner-parent-loss",
                &workspace,
                &state,
                "parent-loss.token",
            );
            let handoff = managed_background::credential_handoff_dir();
            let _ =
                run_connection_loop(&config, handoff.as_deref(), CancellationToken::new()).await;
            return;
        }

        let workspace = tempfile::tempdir().unwrap().keep();
        let workspace = workspace.canonicalize().unwrap();
        let state = tempfile::tempdir().unwrap().keep();
        let state = state.canonicalize().unwrap();
        let runner_dir = state.join("runner");
        std::fs::create_dir_all(&runner_dir).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server_url = format!("ws://{}/edge/ws", listener.local_addr().unwrap());
        let started_marker = workspace.join(".parent-loss-started");
        let (mut parent_ready, child_ready) = std::os::unix::net::UnixStream::pair().unwrap();
        parent_ready
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let child_fd = std::os::fd::AsRawFd::as_raw_fd(&child_ready);
        nix::fcntl::fcntl(
            unsafe { std::os::fd::BorrowedFd::borrow_raw(child_fd) },
            nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::empty()),
        )
        .unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "tests::parent_loss_after_tool_admission_replays_cancellation",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("ASTRA_EDGE_TEST_PARENT_LOSS", "1")
            .env("ASTRA_EDGE_MANAGED", "1")
            .env("ASTRA_EDGE_STATE_DIR", &runner_dir)
            .env("ASTRA_EDGE_READY_FD", child_fd.to_string())
            .env("ASTRA_EDGE_TEST_SERVER", &server_url)
            .env("ASTRA_EDGE_TEST_WORKSPACE", &workspace)
            .env("ASTRA_EDGE_TEST_STATE", &state)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .unwrap();
        drop(child_ready);

        let mut socket = accept_plain_socket(&listener).await;
        assert!(socket.next().await.unwrap().unwrap().is_text());
        socket
            .send(Message::Text(
                serde_json::to_string(&EdgeServerMessage::AuthOk {
                    user_id: "user".into(),
                    edge_id: "runner-parent-loss".into(),
                    interaction_api_major: astra_server_types::AGENT_INTERACTION_API_MAJOR.into(),
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        let identity = astra_server_types::edge_ws_protocol::ToolInvocationIdentity::new(
            "user",
            "session",
            "run",
            "chain",
            "parent-loss-call",
        )
        .unwrap();
        let request = EdgeServerMessage::ToolRequest {
            approved_bash_write: None,
            request_id: identity.storage_key(),
            identity: Box::new(identity),
            delivery_generation: 1,
            tool: "bash".into(),
            args: serde_json::json!({"command": "touch .parent-loss-started && exec sleep 30"}),
            runtime_process_authorization: None,
            runtime_process_authorization_required: false,
            timeout_secs: Some(30),
            execution_deadline_unix_ms: Some(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64
                    + 120_000,
            ),
            execution_timeout_ms: Some(120_000),
            command_timeout_cap_ms: None,
            execution_ceiling: None,
            traceparent: None,
            tracestate: None,
        };
        socket
            .send(Message::Text(
                serde_json::to_string(&request).unwrap().into(),
            ))
            .await
            .unwrap();
        tokio::task::spawn_blocking(move || {
            use std::io::Read;
            let mut ready = [0u8; 1];
            parent_ready.read_exact(&mut ready).unwrap();
            assert_eq!(ready, *b"1");
            let started = std::time::Instant::now();
            while started.elapsed() < Duration::from_secs(10) {
                if started_marker.is_file() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            panic!("bash did not start before the starting command disappeared");
        })
        .await
        .unwrap();
        let status = tokio::time::timeout(
            Duration::from_secs(20),
            tokio::task::spawn_blocking(move || child.wait()),
        )
        .await
        .expect("admitted work did not settle after the starting command disappeared")
        .unwrap()
        .unwrap();
        assert!(
            status.success(),
            "runner exited before settlement: {status}"
        );
        while let Some(frame) = socket.next().await {
            if frame.is_err() {
                break;
            }
        }

        let replay_config = edge_config(
            server_url,
            "runner-parent-loss",
            &workspace,
            &state,
            "replay.token",
        );
        let replay = run_edge_connection(&replay_config, CancellationToken::new());
        let replay_server = async {
            let mut socket = accept_plain_socket(&listener).await;
            assert!(socket.next().await.unwrap().unwrap().is_text());
            socket
                .send(Message::Text(
                    serde_json::to_string(&EdgeServerMessage::AuthOk {
                        user_id: "user".into(),
                        edge_id: "runner-parent-loss".into(),
                        interaction_api_major: astra_server_types::AGENT_INTERACTION_API_MAJOR
                            .into(),
                    })
                    .unwrap()
                    .into(),
                ))
                .await
                .unwrap();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            loop {
                let frame = tokio::time::timeout_at(deadline, socket.next())
                    .await
                    .expect("replay did not deliver the settled result")
                    .unwrap()
                    .unwrap();
                if !frame.is_text() {
                    continue;
                }
                let message: EdgeClientMessage =
                    serde_json::from_str(frame.to_text().unwrap()).unwrap();
                if let EdgeClientMessage::ToolResult {
                    is_error,
                    output,
                    tool_result_fields,
                    ..
                } = message
                {
                    socket
                        .send(Message::Text(
                            serde_json::to_string(&EdgeServerMessage::Closing {
                                reason: "replayed".into(),
                            })
                            .unwrap()
                            .into(),
                        ))
                        .await
                        .unwrap();
                    while let Some(frame) = socket.next().await {
                        if frame.is_err() {
                            break;
                        }
                    }
                    return (is_error, output, tool_result_fields);
                }
            }
        };
        let ((is_error, output, fields), replayed) =
            tokio::time::timeout(Duration::from_secs(10), async {
                tokio::join!(replay_server, replay)
            })
            .await
            .expect("replaying the settled invocation timed out");
        replayed.expect("replay connection failed");
        let fields = fields.expect("settled result had no tool fields");
        assert!(is_error);
        assert_ne!(
            fields.get("outcome_certainty").and_then(Value::as_str),
            Some("unknown"),
            "parent loss was replayed as an unknown outcome: {output}"
        );
        assert!(
            !output.contains("dispatched without a durable completion"),
            "{output}"
        );
        let cancelled = fields.get("cancelled").and_then(Value::as_bool) == Some(true)
            || fields.get("exit_semantics").and_then(Value::as_str) == Some("cancelled");
        assert!(
            cancelled,
            "settled result was not cancellation evidence: {fields:?} {output}"
        );
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
    fn permanent_connection_errors_are_classified_by_type_and_http_status() {
        let authentication = astra_edge::EdgeAuthenticationError::Rejected;
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
        assert_eq!(
            value["protocol_capabilities"]
                [astra_server_types::edge_ws_protocol::W3C_TRACE_CONTEXT_V1_CAPABILITY],
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
            astra_edge::edge_ws_url("http://127.0.0.1:17001").unwrap(),
            "ws://127.0.0.1:17001/edge/ws"
        );
        assert_eq!(
            astra_edge::edge_ws_url("https://astra.example.com").unwrap(),
            "wss://astra.example.com/edge/ws"
        );
        assert_eq!(
            astra_edge::edge_ws_url("wss://astra.example.com/edge/ws").unwrap(),
            "wss://astra.example.com/edge/ws"
        );
        assert_eq!(
            astra_edge::edge_ws_url("https://astra.example.com/edge/ws/extra-path").unwrap(),
            "wss://astra.example.com/edge/ws"
        );
        assert_eq!(
            astra_edge::edge_ws_url("https://astra.example.com/prefix").unwrap(),
            "wss://astra.example.com/prefix/edge/ws"
        );
        assert_eq!(
            astra_edge::edge_ws_url("https://astra.example.com/prefix/edge/ws/extra-path").unwrap(),
            "wss://astra.example.com/prefix/edge/ws"
        );
        assert_eq!(
            astra_edge::edge_ws_url("https://astra.example.com/not-edge/ws").unwrap(),
            "wss://astra.example.com/not-edge/ws/edge/ws"
        );
        assert_eq!(
            astra_edge::edge_ws_url("https://astra.example.com/edge/ws?debug=1#fragment").unwrap(),
            "wss://astra.example.com/edge/ws"
        );
        assert_eq!(
            astra_edge::edge_ws_url("127.0.0.1:17001").unwrap(),
            "ws://127.0.0.1:17001/edge/ws"
        );
        assert!(astra_edge::edge_ws_url("ftp://astra.example.com").is_err());
        assert!(astra_edge::edge_ws_url("").is_err());
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

    fn handoff_config(
        listener: &tokio::net::TcpListener,
        state: &Path,
        workspace: &Path,
        reconnect: bool,
    ) -> EdgeConfig {
        EdgeConfig {
            capacity_providers: Vec::new(),
            server_url: format!("ws://{}/edge/ws", listener.local_addr().unwrap()),
            token_manager: token_manager::TokenManager::new(
                "old-token".into(),
                None,
                state.join("token"),
            ),
            workspace_dir: workspace.to_owned(),
            edge_id: "handoff-test".into(),
            materialization_id: "handoff-materialization".into(),
            reconnect,
            invocation_journal_root: Some(state.to_owned()),
        }
    }

    // The handshake callback's error type is fixed by tokio-tungstenite.
    #[allow(clippy::result_large_err)]
    async fn accept_edge(
        listener: &tokio::net::TcpListener,
    ) -> (WebSocketStream<tokio::net::TcpStream>, String) {
        use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
        let (stream, _) = listener.accept().await.unwrap();
        let mut authorization = String::new();
        let mut ws =
            tokio_tungstenite::accept_hdr_async(stream, |request: &Request, response: Response| {
                authorization = request
                    .headers()
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_string();
                Ok(response)
            })
            .await
            .unwrap();
        assert!(ws.next().await.unwrap().unwrap().is_text());
        (ws, authorization)
    }

    async fn send_server(
        ws: &mut WebSocketStream<tokio::net::TcpStream>,
        message: &EdgeServerMessage,
    ) {
        ws.send(Message::Text(
            serde_json::to_string(message).unwrap().into(),
        ))
        .await
        .unwrap();
    }

    fn auth_ok() -> EdgeServerMessage {
        EdgeServerMessage::AuthOk {
            user_id: "user".into(),
            edge_id: "ws-handoff-test".into(),
            interaction_api_major: astra_server_types::AGENT_INTERACTION_API_MAJOR.into(),
        }
    }

    #[tokio::test]
    async fn credential_handed_off_during_authentication_is_tried_before_exiting() {
        let workspace = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let handoff = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = handoff_config(&listener, state.path(), workspace.path(), false);
        let (in_flight, queued) = (
            tokio::sync::oneshot::channel::<()>(),
            tokio::sync::oneshot::channel::<()>(),
        );
        let (in_flight_sender, in_flight_receiver) = in_flight;
        let (queued_sender, queued_receiver) = queued;
        let server = async {
            let (mut ws, first) = accept_edge(&listener).await;
            in_flight_sender.send(()).unwrap();
            queued_receiver.await.unwrap();
            send_server(
                &mut ws,
                &EdgeServerMessage::AuthError {
                    message: "revoked".into(),
                },
            )
            .await;
            drop(ws);
            let (mut ws, second) = accept_edge(&listener).await;
            send_server(&mut ws, &auth_ok()).await;
            send_server(
                &mut ws,
                &EdgeServerMessage::Closing {
                    reason: "handoff test complete".into(),
                },
            )
            .await;
            while ws.next().await.is_some_and(|message| message.is_ok()) {}
            (first, second)
        };
        let connect_command = async {
            in_flight_receiver.await.unwrap();
            managed_background::queue_pending_credential(handoff.path(), "fresh-token").unwrap();
            queued_sender.send(()).unwrap();
        };
        let ((first, second), exit_with_error, ()) =
            tokio::time::timeout(Duration::from_secs(15), async {
                tokio::join!(
                    server,
                    run_connection_loop(&config, Some(handoff.path()), CancellationToken::new()),
                    connect_command
                )
            })
            .await
            .unwrap();
        assert_eq!(first, "Bearer old-token");
        assert_eq!(second, "Bearer fresh-token");
        assert!(!exit_with_error);
        assert_eq!(
            managed_background::claim_pending_credential(handoff.path()).unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn superseded_handoffs_still_fall_back_to_the_proven_credential() {
        let workspace = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let handoff = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = handoff_config(&listener, state.path(), workspace.path(), true);
        let rejected = EdgeServerMessage::AuthError {
            message: "rejected".into(),
        };
        let server = async {
            let mut seen = Vec::new();
            let (mut ws, bearer) = accept_edge(&listener).await;
            seen.push(bearer);
            send_server(&mut ws, &auth_ok()).await;
            managed_background::queue_pending_credential(handoff.path(), "bad-one").unwrap();
            send_server(
                &mut ws,
                &EdgeServerMessage::Closing {
                    reason: "server restart".into(),
                },
            )
            .await;
            while ws.next().await.is_some_and(|message| message.is_ok()) {}

            let (mut ws, bearer) = accept_edge(&listener).await;
            seen.push(bearer);
            // A second handoff arrives while the first is authenticating.
            managed_background::queue_pending_credential(handoff.path(), "bad-two").unwrap();
            send_server(&mut ws, &rejected).await;
            drop(ws);

            let (mut ws, bearer) = accept_edge(&listener).await;
            seen.push(bearer);
            send_server(&mut ws, &rejected).await;
            drop(ws);

            let (_ws, bearer) = accept_edge(&listener).await;
            seen.push(bearer);
            seen
        };
        let seen = tokio::time::timeout(Duration::from_secs(20), async {
            tokio::select! {
                exit_with_error = run_connection_loop(&config, Some(handoff.path()), CancellationToken::new()) => {
                    panic!("runner exited before trying the proven credential (error: {exit_with_error})")
                }
                seen = server => seen,
            }
        })
        .await
        .unwrap();
        assert_eq!(
            seen,
            [
                "Bearer old-token",
                "Bearer bad-one",
                "Bearer bad-two",
                "Bearer old-token"
            ]
        );
    }

    #[tokio::test]
    async fn handed_off_credential_reconnects_without_interrupting_admitted_work() {
        let workspace = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let handoff = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = handoff_config(&listener, state.path(), workspace.path(), true);
        let server = async {
            let (mut ws, first) = accept_edge(&listener).await;
            assert_eq!(first, "Bearer old-token");
            send_server(&mut ws, &auth_ok()).await;
            let identity = astra_server_types::edge_ws_protocol::ToolInvocationIdentity::new(
                "user", "session", "run", "turn", "call",
            )
            .unwrap();
            send_server(
                &mut ws,
                &EdgeServerMessage::ToolRequest {
                    approved_bash_write: None,
                    request_id: identity.storage_key(),
                    identity: Box::new(identity),
                    delivery_generation: 1,
                    tool: "bash".into(),
                    args: serde_json::json!({
                        "command": "touch started; sleep 2; echo finished"
                    }),
                    runtime_process_authorization: None,
                    runtime_process_authorization_required: false,
                    timeout_secs: Some(30),
                    execution_deadline_unix_ms: None,
                    execution_timeout_ms: None,
                    command_timeout_cap_ms: None,
                    execution_ceiling: None,
                    traceparent: None,
                    tracestate: None,
                },
            )
            .await;
            tokio::time::timeout(Duration::from_secs(10), async {
                while !workspace.path().join("started").exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("admitted work did not start");

            managed_background::queue_pending_credential(handoff.path(), "fresh-token").unwrap();
            let (request_id, delivery_generation, is_error, output) =
                tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        let frame = ws.next().await.unwrap().unwrap();
                        if let Ok(EdgeClientMessage::ToolResult {
                            request_id,
                            delivery_generation,
                            is_error,
                            output,
                            ..
                        }) = serde_json::from_str(frame.to_text().unwrap())
                        {
                            return (request_id, delivery_generation, is_error, output);
                        }
                    }
                })
                .await
                .expect("admitted work did not report a result");
            assert!(!is_error, "admitted work was interrupted: {output}");
            assert!(output.contains("finished"), "unexpected output: {output}");
            send_server(
                &mut ws,
                &EdgeServerMessage::ToolResultAck {
                    request_id,
                    delivery_generation,
                },
            )
            .await;
            send_server(
                &mut ws,
                &EdgeServerMessage::Closing {
                    reason: "server restart".into(),
                },
            )
            .await;
            while ws.next().await.is_some_and(|message| message.is_ok()) {}

            let (_ws, second) =
                tokio::time::timeout(Duration::from_secs(10), accept_edge(&listener))
                    .await
                    .expect("runner did not reconnect");
            second
        };
        let second = tokio::select! {
            exit_with_error = run_connection_loop(&config, Some(handoff.path()), CancellationToken::new()) => {
                panic!("runner exited before reconnecting (error: {exit_with_error})")
            }
            second = server => second,
        };
        assert_eq!(second, "Bearer fresh-token");
        assert_eq!(
            managed_background::claim_pending_credential(handoff.path()).unwrap(),
            None
        );
    }
}
