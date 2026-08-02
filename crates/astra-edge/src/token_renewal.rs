//! Edge-token self-renewal for `moi-user-token-v1` registration tokens.
//!
//! Long-lived sandboxes authenticate with a 30-day edge-registration token.
//! This module renews the token before expiry against the backend endpoint
//! configured via `ASTRA_TOKEN_RENEW_URL`, persists the renewed token to a
//! token file (`ASTRA_TOKEN_FILE`, default `<workspace>/.astra/token`), and
//! updates the shared in-process token so reconnects pick it up.
//!
//! No signature verification is performed — the edge does not hold the
//! signing key; claims are parsed only to learn the expiry.

use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Prefix identifying an edge-registration token: `moi-user-token-v1.<claims>.<sig>`.
pub const MOI_TOKEN_PREFIX: &str = "moi-user-token-v1.";

const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const STARTUP_DELAY: Duration = Duration::from_secs(30);
const DEFAULT_RENEW_THRESHOLD_SECS: i64 = 7 * 24 * 60 * 60;
/// Upper bound for the configurable renewal threshold. Caps pathological values
/// (e.g. i64::MAX) that would otherwise overflow the exp-based scheduling math.
const MAX_RENEW_THRESHOLD_SECS: i64 = 30 * 24 * 60 * 60;
const RENEW_HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// Fast-retry schedule after a transient renewal failure. Must fit inside the
/// backend's rotation grace window (5 minutes) so a lost rotation response can
/// recover via the previous-jti renewal path.
const TRANSIENT_RETRY_SECS: &[u64] = &[30, 60, 120];
/// The backend keeps a rotated jti renewable for this long after it hands out a
/// successor (dual-valid rotation). A lost rotation response can only be
/// recovered by a fast retry that both BEGINS and completes within this window,
/// measured from the first retry attempt; past it the old jti is swept and
/// every subsequent renewal is rejected. Fast retries are budgeted against this
/// deadline so the last attempt cannot start at or after the boundary.
const ROTATION_GRACE_WINDOW: Duration = Duration::from_secs(5 * 60);

/// Sleep to apply before the next fast retry so the attempt after it still
/// begins — and, budgeting one `request_timeout`, completes — within the
/// rotation grace window measured from the first attempt.
///
/// `elapsed` is the time since the first retry attempt began. Returns `None`
/// once no further attempt can complete before the deadline (stop fast
/// retrying); otherwise the requested `backoff`, clamped so the next attempt
/// leaves a full `request_timeout` of headroom before the deadline.
fn retry_delay_within_grace(
    elapsed: Duration,
    backoff: Duration,
    grace: Duration,
    request_timeout: Duration,
) -> Option<Duration> {
    let remaining = grace.checked_sub(elapsed)?;
    if remaining <= request_timeout {
        return None;
    }
    Some(backoff.min(remaining - request_timeout))
}

/// Claims embedded in the middle segment of a `moi-user-token-v1` token.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct TokenClaims {
    pub exp: i64,
    #[serde(default)]
    pub iat: i64,
    #[serde(default)]
    pub jti: Option<String>,
    #[serde(default)]
    pub sub: String,
    #[serde(default)]
    pub workspace_id: String,
    #[serde(default)]
    pub iss: String,
    #[serde(default)]
    pub edge_agent_id: String,
    #[serde(default)]
    pub purpose: String,
}

/// Parse the claims of a `moi-user-token-v1.<claims>.<sig>` token.
///
/// Returns `None` for non-moi tokens or malformed claims ("expiry unknown").
pub fn parse_moi_token_claims(token: &str) -> Option<TokenClaims> {
    use base64::Engine as _;
    let rest = token.strip_prefix(MOI_TOKEN_PREFIX)?;
    let (claims_b64, signature) = rest.split_once('.')?;
    if claims_b64.is_empty() || signature.is_empty() {
        return None;
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(claims_b64)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Decide whether an authentication-PROVEN token should overwrite the token
/// file. Skip only when the file already holds the proven token itself, or a
/// same-identity, STRICTLY newer generation (by iat, then exp). Anything else —
/// including a different jti with the SAME iat/exp, which double-renew / grace
/// recovery can legitimately produce, or a token of a DIFFERENT MOI identity —
/// is overwritten: expiry seconds do not order generations, and generations of
/// different identities are not comparable at all, but an AuthOk is hard proof
/// the proven token works for the current identity.
pub fn should_persist_proven(existing: Option<&str>, proven: &str) -> bool {
    let Some(proven_claims) = parse_moi_token_claims(proven) else {
        // Non-MOI tokens never touch the MOI token file.
        return false;
    };
    let Some(existing) = existing else {
        return true;
    };
    if existing == proven {
        return false;
    }
    let Some(existing_claims) = parse_moi_token_claims(existing) else {
        return true;
    };
    // Different MOI identity (e.g. a reused workspace volume still holding a
    // previous tenant's token): generation ordering via (iat, exp) is
    // meaningless across identities. Comparing them could leave a
    // later-generation previous-tenant token on disk, which a later env-less
    // startup would then select and be permanently rejected on. The proven
    // token is hard AuthOk proof for the CURRENT identity — replace the stale
    // foreign-identity file rather than generation-comparing it.
    if !same_moi_identity(&existing_claims, &proven_claims) {
        return true;
    }
    let existing_gen = (existing_claims.iat, existing_claims.exp);
    let proven_gen = (proven_claims.iat, proven_claims.exp);
    // Strictly newer file wins (renewal owns forward progress); ties go to
    // the PROVEN token — a same-generation sibling in the file may be the
    // superseded jti of a double renew and would brick the next restart.
    existing_gen <= proven_gen
}

/// Whether the token's remaining lifetime is below the renewal threshold.
pub fn should_renew(claims: &TokenClaims, now_unix: i64, threshold_secs: i64) -> bool {
    // saturating_sub: abnormal claims (huge/negative exp) must not overflow-panic
    // in debug or wrap in release.
    claims.exp.saturating_sub(now_unix) < threshold_secs
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Purpose claim of an edge-registration token; the server requires a non-empty
/// jti for tokens carrying it.
const EDGE_REGISTRATION_PURPOSE: &str = "edge_registration";

/// Issuer that mints edge-registration tokens exclusively. The server rejects a
/// token from this issuer that does not declare the edge-registration purpose
/// (a pre-binding historical runner token).
const ISSUER_MOI_BACKEND: &str = "moi-backend";

/// Mirror the server's `Verify` (crate `astraauth`) acceptance rules that the
/// edge CAN check without the signing key: exactly three segments, non-empty
/// `sub`/`workspace_id`, the moi-backend-issuer / edge-registration coupling,
/// and the edge-registration binding (`edge_agent_id` + `jti`). Signature and
/// expiry are checked by the caller. A token failing these is one the server
/// would reject, so the edge must neither select it from the token file nor
/// persist it from a renewal — with no env fallback, doing so bricks the edge.
fn matches_server_acceptance(token: &str, claims: &TokenClaims) -> bool {
    is_wellformed_moi_token(token)
        && !claims.sub.is_empty()
        && !claims.workspace_id.is_empty()
        // moi-backend issues edge-registration tokens exclusively; any other
        // purpose from that issuer is a pre-binding historical token the server
        // rejects.
        && !(claims.iss == ISSUER_MOI_BACKEND && claims.purpose != EDGE_REGISTRATION_PURPOSE)
        // Edge-registration tokens must bind an edge_agent_id and carry a jti.
        && (claims.purpose != EDGE_REGISTRATION_PURPOSE
            || (!claims.edge_agent_id.is_empty()
                && claims.jti.as_deref().is_some_and(|jti| !jti.is_empty())))
}

/// Structural check mirroring the server's `verify_user_token`: a moi-user token
/// must be exactly `moi-user-token-v1.<claims>.<sig>` — three dot-separated
/// segments, no more. The lenient `parse_moi_token_claims` accepts extra
/// trailing segments, so the renewal decision re-checks here to avoid persisting
/// a token the server would reject as malformed.
fn is_wellformed_moi_token(token: &str) -> bool {
    let expected_prefix = MOI_TOKEN_PREFIX.trim_end_matches('.');
    let mut parts = token.split('.');
    matches!(
        (parts.next(), parts.next(), parts.next(), parts.next()),
        (Some(prefix), Some(claims), Some(sig), None)
            if prefix == expected_prefix && !claims.is_empty() && !sig.is_empty()
    )
}

fn parse_usable_renewed_token(
    token: &str,
    current_token: &str,
    current: &TokenClaims,
    now_unix: i64,
) -> Option<TokenClaims> {
    parse_moi_token_claims(token).filter(|renewed| {
        token != current_token
            && renewed.exp > now_unix
            && (renewed.iat, renewed.exp) >= (current.iat, current.exp)
            && renewed.sub == current.sub
            && renewed.workspace_id == current.workspace_id
            && renewed.iss == current.iss
            && renewed.edge_agent_id == current.edge_agent_id
            && renewed.purpose == current.purpose
            // Match the server's acceptance rules before overwriting the
            // persisted identity. A token that passes the generation/identity
            // checks but the server would reject would brick the edge on restart.
            && matches_server_acceptance(token, renewed)
    })
}

/// True when two parsed moi-user-token claims share the SAME identity
/// (user + workspace + edge agent + issuer + purpose). Generation ordering via
/// (iat, exp) is only meaningful between same-identity tokens; a token of a
/// different identity must never override or become a fallback for another
/// (e.g. a reused workspace volume holding a previous tenant's token).
pub fn same_moi_identity(a: &TokenClaims, b: &TokenClaims) -> bool {
    a.sub == b.sub
        && a.workspace_id == b.workspace_id
        && a.iss == b.iss
        && a.edge_agent_id == b.edge_agent_id
        && a.purpose == b.purpose
}

fn renew_threshold_secs() -> i64 {
    std::env::var("ASTRA_TOKEN_RENEW_THRESHOLD_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|v| *v > 0)
        .map(|v| v.min(MAX_RENEW_THRESHOLD_SECS))
        .unwrap_or(DEFAULT_RENEW_THRESHOLD_SECS)
}

// ─── Token file persistence ──────────────────────────────────────────────────

/// Token file path: `ASTRA_TOKEN_FILE` env override, else `<workspace>/.astra/token`.
pub fn resolve_token_file_path(workspace_dir: &Path) -> PathBuf {
    match std::env::var("ASTRA_TOKEN_FILE") {
        Ok(value) if !value.trim().is_empty() => PathBuf::from(value),
        _ => workspace_dir.join(".astra").join("token"),
    }
}

/// Read the token file and return its token only if it is a `moi-user-token-v1`
/// token with parseable claims that have not expired AND that the server would
/// accept ([`matches_server_acceptance`]). The lenient claims parser tolerates
/// extra segments and defaulted identity fields the server rejects; without an
/// env fallback, selecting such a file would let the edge pick a token it is
/// then permanently rejected on, so the structural/claim checks are applied here.
pub fn read_valid_file_token(path: &Path, now_unix: i64) -> Option<String> {
    let contents = std::fs::read_to_string(path).ok()?;
    let token = contents.trim();
    if token.is_empty() {
        return None;
    }
    let claims = parse_moi_token_claims(token)?;
    if claims.exp <= now_unix {
        return None;
    }
    if !matches_server_acceptance(token, &claims) {
        return None;
    }
    Some(token.to_string())
}

/// Returns true when an existing token path should be atomically replaced to
/// enforce the Unix credential-file invariant: a regular file with mode 0600.
///
/// `symlink_metadata` deliberately does not follow symlinks. Replacing a
/// symlink through [`write_token_atomic`] publishes a private regular file at
/// the configured path instead of changing permissions on the symlink target.
#[cfg(unix)]
pub(crate) fn token_file_needs_permission_repair(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;

    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return false;
    };
    metadata.file_type().is_symlink() || metadata.permissions().mode() & 0o777 != 0o600
}

#[cfg(not(unix))]
pub(crate) fn token_file_needs_permission_repair(_path: &Path) -> bool {
    false
}

/// Atomically write the token: parent dir created, unique same-directory temp
/// file + rename, permissions 0600 on unix.
///
/// The temp file uses a collision-resistant random sibling name so two
/// edge/renew processes sharing a workspace (rolling restarts, double-run)
/// cannot delete or rename each other's in-flight file; the final replace is
/// atomic, so the published file is always one writer's complete token.
pub fn write_token_atomic(path: &Path, token: &str) -> std::io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("token");
    let mut staged = tempfile::Builder::new()
        .prefix(&format!(".{file_name}."))
        .suffix(".tmp")
        .tempfile_in(parent)?;
    {
        use std::io::Write as _;
        staged.write_all(token.as_bytes())?;
        staged.as_file_mut().sync_all()?;
    }
    // NamedTempFile::persist uses an overwrite-capable atomic replace on
    // Windows (MoveFileExW + MOVEFILE_REPLACE_EXISTING) and rename on Unix.
    // A failed persist retains and then removes the staged file on drop.
    staged
        .persist(path)
        .map(|_| ())
        .map_err(|error| error.error)
}

// ─── Renewal HTTP call ───────────────────────────────────────────────────────

enum RenewOutcome {
    Renewed {
        token: String,
        expires_at: Option<String>,
    },
    /// HTTP 401 — token rejected; keep the current token.
    Rejected,
    /// Transport error / 503 / unexpected response — retry next cycle.
    Transient(String),
}

async fn renew_once(client: &reqwest::Client, url: &str, current_token: &str) -> RenewOutcome {
    let response = match client
        .post(url)
        .json(&serde_json::json!({ "token": current_token }))
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => return RenewOutcome::Transient(format!("request failed: {error}")),
    };
    let status = response.status();
    if status.as_u16() == 401 {
        return RenewOutcome::Rejected;
    }
    if !status.is_success() {
        return RenewOutcome::Transient(format!("unexpected status {status}"));
    }
    let body: serde_json::Value = match response.json().await {
        Ok(body) => body,
        Err(error) => return RenewOutcome::Transient(format!("invalid JSON body: {error}")),
    };
    // Defensive parse: prefer data.token, fall back to top-level token.
    let token = body
        .pointer("/data/token")
        .and_then(|v| v.as_str())
        .or_else(|| body.get("token").and_then(|v| v.as_str()));
    match token {
        Some(token) if !token.trim().is_empty() => RenewOutcome::Renewed {
            token: token.trim().to_string(),
            expires_at: body
                .pointer("/data/expires_at")
                .and_then(|v| v.as_str())
                .map(str::to_string),
        },
        _ => RenewOutcome::Transient("response body contains no token".to_string()),
    }
}

/// True when the renewal URL is safe to POST the edge token to: HTTPS for any
/// remote host, HTTP allowed only for loopback (local dev). Unparseable URLs
/// are rejected.
fn renew_url_scheme_ok(url: &str) -> bool {
    match reqwest::Url::parse(url) {
        // HTTPS to anywhere, or plain HTTP only to a loopback host. Any other
        // scheme (ftp://localhost, ws://…) is rejected — url_is_loopback alone
        // ignores the scheme.
        Ok(parsed) => {
            parsed.scheme() == "https"
                || (parsed.scheme() == "http" && astra_core::net::url_is_loopback(url))
        }
        Err(_) => false,
    }
}

// ─── Renewal loop ────────────────────────────────────────────────────────────

/// Spawn the background renewal task. Call once at startup, at the same
/// supervision level as the main connection loop.
///
/// `manager` owns the token state machine: renewal reads its snapshot,
/// applies responses through it (stale responses are discarded) and relies on
/// it for persistence-debt bookkeeping.
pub fn spawn_renewal_task(manager: Arc<crate::token_manager::TokenManager>) {
    let renew_url = match std::env::var("ASTRA_TOKEN_RENEW_URL") {
        Ok(url) if !url.trim().is_empty() => url.trim().to_string(),
        _ => {
            tracing::info!(
                target: "astra.edge",
                "ASTRA_TOKEN_RENEW_URL not set — edge token renewal disabled"
            );
            // Renewal is off, but persistence debt (AuthOk heal-write
            // failures) still needs a consumer — run a minimal retry loop so
            // the dirty flag always has an owner.
            tokio::spawn(async move {
                loop {
                    manager.persist_debt_notified().await;
                    while !manager.retry_persist().await {
                        tokio::time::sleep(Duration::from_secs(30)).await;
                    }
                }
            });
            return;
        }
    };
    // Never send the long-lived edge token over plaintext to a remote host.
    // Require HTTPS for remote URLs (loopback may use HTTP for local dev).
    if !renew_url_scheme_ok(&renew_url) {
        tracing::error!(
            target: "astra.edge",
            url = %renew_url,
            "ASTRA_TOKEN_RENEW_URL must be HTTPS for remote hosts (loopback HTTP allowed) — \
             refusing to send the edge token over plaintext; renewal disabled"
        );
        tokio::spawn(async move {
            loop {
                manager.persist_debt_notified().await;
                while !manager.retry_persist().await {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                }
            }
        });
        return;
    }
    let threshold_secs = renew_threshold_secs();
    tokio::spawn(async move {
        let client = match astra_core::net::client_builder_for_target(&renew_url)
            .timeout(RENEW_HTTP_TIMEOUT)
            // Disallow redirects: a 307/308 must never forward the POST body
            // (which carries the token) to a different origin.
            .redirect(reqwest::redirect::Policy::none())
            .build()
        {
            Ok(client) => client,
            Err(error) => {
                tracing::warn!(
                    target: "astra.edge",
                    error = %error,
                    "failed to build HTTP client — edge token renewal disabled; \
                     keeping persistence-only retry loop"
                );
                // Renewal is off, but later mark_proven() writes can still fail
                // and set the dirty flag. Own the debt permanently like the
                // unconfigured-URL branch — waiting on each notification —
                // instead of draining once and exiting.
                loop {
                    manager.persist_debt_notified().await;
                    while !manager.retry_persist().await {
                        tokio::time::sleep(Duration::from_secs(30)).await;
                    }
                }
            }
        };
        let mut warned_malformed = false;
        let mut warned_expired = false;
        // Only delay startup for a healthy token. A token already inside the
        // renewal window (or with < STARTUP_DELAY remaining) must be processed
        // immediately or it could expire before the first pass. Unparseable
        // tokens also skip the delay (the first cycle just warns).
        let delay_startup = parse_moi_token_claims(&manager.snapshot().await)
            .map(|c| !should_renew(&c, now_unix(), threshold_secs))
            .unwrap_or(false);
        if delay_startup {
            tokio::time::sleep(STARTUP_DELAY).await;
        }
        loop {
            // A transient transport failure may mean the backend already
            // rotated the jti and the response was lost. The backend keeps the
            // old jti renewable for a short grace window (dual-valid rotation),
            // so retry quickly with backoff instead of waiting CHECK_INTERVAL —
            // the fast retries are what make the recovery path reachable.
            let mut transient_backoff = TRANSIENT_RETRY_SECS.iter();
            let mut rejected = false;
            // Anchor the grace deadline at the first attempt of this cycle. A
            // lost rotation response is only recoverable while the old jti is
            // still dual-valid, so retries are driven off this absolute deadline
            // rather than the raw backoff schedule (whose last sleep could push
            // an attempt to or past the boundary once request duration counts).
            let retry_start = tokio::time::Instant::now();
            loop {
                let transient = renewal_cycle(
                    &client,
                    &renew_url,
                    threshold_secs,
                    &manager,
                    &mut warned_malformed,
                    &mut warned_expired,
                    &mut rejected,
                )
                .await;
                if !transient {
                    break;
                }
                let Some(backoff_secs) = transient_backoff.next() else {
                    break;
                };
                let elapsed = tokio::time::Instant::now().duration_since(retry_start);
                let Some(delay) = retry_delay_within_grace(
                    elapsed,
                    Duration::from_secs(*backoff_secs),
                    ROTATION_GRACE_WINDOW,
                    RENEW_HTTP_TIMEOUT,
                ) else {
                    // No further attempt can complete inside the rotation grace
                    // window; stop fast-retrying and fall to the outer wait
                    // (which still polls at 30s while persistence debt remains).
                    tracing::info!(
                        target: "astra.edge",
                        "edge token renewal fast-retries exhausted the rotation grace window"
                    );
                    break;
                };
                tracing::info!(
                    target: "astra.edge",
                    retry_in_secs = delay.as_secs(),
                    "retrying edge token renewal after transient failure"
                );
                tokio::time::sleep(delay).await;
            }
            // Wake early when someone (renewal itself or the AuthOk heal
            // path) records persistence debt. Outstanding debt must NEVER
            // sleep the full check interval: the Notify permit may already be
            // consumed, and the token file has to catch up within the
            // backend's 5-minute rotation grace window — keep polling at 30s
            // until the write lands (H1, review round 12).
            let base = if manager.persist_pending().await {
                Duration::from_secs(30)
            } else {
                CHECK_INTERVAL
            };
            let wait = if rejected {
                // Token permanently rejected (401). Do NOT apply the exp-based
                // cap — that would pin an in-window rejected token to a 30s hot
                // loop forever. Wait the full interval (or 30s if we still owe a
                // persist); a token/fallback change wakes the connection path,
                // and persist_debt_notified() still interrupts the select below.
                base
            } else {
                // Never sleep past the renewal deadline: a token with less than
                // CHECK_INTERVAL remaining would otherwise expire before the next
                // pass. Cap the wait at exp - threshold, floored at 30s so we
                // don't busy-loop once inside the renew window (the next
                // renewal_cycle then renews). saturating_sub guards abnormal
                // claims / large thresholds from overflow.
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                let exp_bound = parse_moi_token_claims(&manager.snapshot().await)
                    .map(|c| {
                        let secs = c
                            .exp
                            .saturating_sub(threshold_secs)
                            .saturating_sub(now)
                            .max(0);
                        Duration::from_secs(secs as u64)
                    })
                    .unwrap_or(CHECK_INTERVAL);
                base.min(exp_bound.max(Duration::from_secs(30)))
            };
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = manager.persist_debt_notified() => {}
            }
        }
    });
}

/// One renewal pass. Returns true when a fast retry is warranted (transient
/// transport failure or outstanding persistence debt).
async fn renewal_cycle(
    client: &reqwest::Client,
    renew_url: &str,
    threshold_secs: i64,
    manager: &crate::token_manager::TokenManager,
    warned_malformed: &mut bool,
    warned_expired: &mut bool,
    rejected: &mut bool,
) -> bool {
    // Set on a 401: the token is permanently rejected, so the scheduler must
    // back off to the full interval instead of hammering every 30s.
    *rejected = false;
    // Persistence debt first: the manager re-reads its current token and
    // clears the flag inside one critical section, so no concurrent switch
    // can be skipped.
    if manager.persist_pending().await && !manager.retry_persist().await {
        return true;
    }
    let current_token = manager.snapshot().await;
    let Some(claims) = parse_moi_token_claims(&current_token) else {
        if !*warned_malformed {
            *warned_malformed = true;
            tracing::warn!(
                target: "astra.edge",
                "current token is not a parseable moi-user-token-v1 — expiry unknown, renewal not scheduled"
            );
        }
        return false;
    };
    *warned_malformed = false;
    let now = now_unix();
    if !should_renew(&claims, now, threshold_secs) {
        tracing::debug!(
            target: "astra.edge",
            remaining_secs = claims.exp.saturating_sub(now),
            threshold_secs,
            "edge token renewal not due yet"
        );
        return false;
    }
    if claims.exp <= now && !*warned_expired {
        *warned_expired = true;
        tracing::warn!(
            target: "astra.edge",
            expired_at = claims.exp,
            "edge token already expired at {}; renewal will be rejected — \
             re-provision the token (delete the token file / recreate the sandbox)",
            claims.exp
        );
    }
    // Still attempt — a server 401 confirms the diagnosis.
    match renew_once(client, renew_url, &current_token).await {
        RenewOutcome::Renewed { token, expires_at } => {
            // Validate before handing to the manager: must be a parseable,
            // unexpired moi-user-token-v1 token.
            let new_claims =
                parse_usable_renewed_token(&token, &current_token, &claims, now_unix());
            let Some(new_claims) = new_claims else {
                tracing::warn!(
                    target: "astra.edge",
                    "renewal endpoint returned an unusable token — keeping current token"
                );
                // A syntactically successful response can still be a transient
                // backend/proxy failure. Retry while the current token remains
                // usable instead of sleeping for the six-hour check interval.
                return true;
            };
            let new_jti = new_claims.jti;
            match manager.apply_renewed(&current_token, token).await {
                crate::token_manager::RenewApply::Discarded => {
                    tracing::warn!(
                        target: "astra.edge",
                        "shared token changed while renewal was in flight — discarding renewed token"
                    );
                    return false;
                }
                crate::token_manager::RenewApply::AppliedUnpersisted => {
                    *warned_expired = false;
                    tracing::info!(
                        target: "astra.edge",
                        old_jti = claims.jti.as_deref().unwrap_or("<none>"),
                        new_jti = new_jti.as_deref().unwrap_or("<none>"),
                        expires_at = expires_at.as_deref().unwrap_or("<unknown>"),
                        "edge token renewed (persistence pending)"
                    );
                    // File must catch up before the backend's grace sweeper
                    // revokes the old jti — fast retries.
                    return true;
                }
                crate::token_manager::RenewApply::Applied => {
                    *warned_expired = false;
                    tracing::info!(
                        target: "astra.edge",
                        old_jti = claims.jti.as_deref().unwrap_or("<none>"),
                        new_jti = new_jti.as_deref().unwrap_or("<none>"),
                        expires_at = expires_at.as_deref().unwrap_or("<unknown>"),
                        "edge token renewed"
                    );
                }
            }
        }
        RenewOutcome::Rejected => {
            *rejected = true;
            tracing::warn!(
                target: "astra.edge",
                "edge token renewal rejected (401) — keeping current token; \
                 backing off (token/fallback must change to recover)"
            );
        }
        RenewOutcome::Transient(detail) => {
            tracing::warn!(
                target: "astra.edge",
                detail = %detail,
                "edge token renewal failed transiently — fast retry scheduled"
            );
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renew_url_scheme_ok_requires_https_or_loopback_http() {
        assert!(renew_url_scheme_ok("https://catalog.example.com/renew"));
        assert!(renew_url_scheme_ok("http://localhost:8081/renew"));
        assert!(renew_url_scheme_ok("http://127.0.0.1:8081/renew"));
        assert!(renew_url_scheme_ok("http://[::1]:8081/renew"));
        // Remote plaintext HTTP: rejected.
        assert!(!renew_url_scheme_ok("http://catalog.example.com/renew"));
        // Non-http(s) schemes, even to loopback: rejected.
        assert!(!renew_url_scheme_ok("ftp://localhost/renew"));
        assert!(!renew_url_scheme_ok("ws://localhost/renew"));
        assert!(!renew_url_scheme_ok("not a url"));
    }

    fn make_token(claims: &serde_json::Value) -> String {
        use base64::Engine as _;
        let encoded =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims.to_string().as_bytes());
        format!("{MOI_TOKEN_PREFIX}{encoded}.sig")
    }

    #[test]
    fn renewed_token_must_preserve_identity_and_not_regress_generation() {
        let now = 1_000_000;
        let current_json = serde_json::json!({
            "iat": 100,
            "exp": now + 60,
            "jti": "old-jti",
            "sub": "user-1",
            "workspace_id": "workspace-1",
            "iss": "moi-backend",
            "edge_agent_id": "edge-1",
            "purpose": "edge_registration",
        });
        let current_token = make_token(&current_json);
        let current = parse_moi_token_claims(&current_token).expect("current claims parse");
        let mut renewed_json = current_json.clone();
        renewed_json["iat"] = serde_json::json!(101);
        renewed_json["exp"] = serde_json::json!(now + 3600);
        renewed_json["jti"] = serde_json::json!("new-jti");

        let renewed = make_token(&renewed_json);
        assert!(parse_usable_renewed_token(&renewed, &current_token, &current, now).is_some());
        assert!(
            parse_usable_renewed_token(&current_token, &current_token, &current, now).is_none()
        );

        let mut expired_json = renewed_json.clone();
        expired_json["exp"] = serde_json::json!(now);
        assert!(
            parse_usable_renewed_token(&make_token(&expired_json), &current_token, &current, now)
                .is_none()
        );

        let mut older_json = renewed_json.clone();
        older_json["iat"] = serde_json::json!(99);
        assert!(
            parse_usable_renewed_token(&make_token(&older_json), &current_token, &current, now)
                .is_none()
        );

        for field in ["sub", "workspace_id", "iss", "edge_agent_id", "purpose"] {
            let mut changed_identity = renewed_json.clone();
            changed_identity[field] = serde_json::json!("different");
            assert!(
                parse_usable_renewed_token(
                    &make_token(&changed_identity),
                    &current_token,
                    &current,
                    now
                )
                .is_none(),
                "renewal must reject changed identity field {field}"
            );
        }

        assert!(
            parse_usable_renewed_token("not-a-valid-moi-token", &current_token, &current, now)
                .is_none()
        );
    }

    // The lenient claims parser accepts extra trailing segments and a missing
    // jti, but the server rejects both. The renewal decision must not overwrite
    // the persisted identity with such a token or the edge bricks on restart.
    #[test]
    fn renewal_rejects_tokens_the_server_would_refuse() {
        let now = 1_000_000;
        let current_json = serde_json::json!({
            "iat": 100,
            "exp": now + 60,
            "jti": "old-jti",
            "sub": "user-1",
            "workspace_id": "workspace-1",
            "iss": "moi-backend",
            "edge_agent_id": "edge-1",
            "purpose": "edge_registration",
        });
        let current_token = make_token(&current_json);
        let current = parse_moi_token_claims(&current_token).expect("current claims parse");

        let mut valid_json = current_json.clone();
        valid_json["iat"] = serde_json::json!(101);
        valid_json["exp"] = serde_json::json!(now + 3600);
        valid_json["jti"] = serde_json::json!("new-jti");

        // Sanity: the otherwise-valid renewal is accepted.
        assert!(
            parse_usable_renewed_token(&make_token(&valid_json), &current_token, &current, now)
                .is_some()
        );

        // Four segments (extra dot in the signature) — server requires exactly 3.
        let four_segment = format!("{}.extra", make_token(&valid_json));
        assert!(
            parse_usable_renewed_token(&four_segment, &current_token, &current, now).is_none(),
            "renewal must reject a >3-segment token"
        );

        // Missing jti on an edge-registration token — server requires it.
        let mut no_jti_json = valid_json.clone();
        no_jti_json.as_object_mut().unwrap().remove("jti");
        assert!(
            parse_usable_renewed_token(&make_token(&no_jti_json), &current_token, &current, now)
                .is_none(),
            "renewal must reject an edge-registration token without jti"
        );

        // Empty jti string — also rejected.
        let mut empty_jti_json = valid_json.clone();
        empty_jti_json["jti"] = serde_json::json!("");
        assert!(
            parse_usable_renewed_token(&make_token(&empty_jti_json), &current_token, &current, now)
                .is_none(),
            "renewal must reject an edge-registration token with empty jti"
        );
    }

    #[test]
    fn write_token_atomic_concurrent_writers_publish_a_complete_token() {
        let dir = std::env::temp_dir().join(format!(
            "astra-edge-token-race-{}-{:x}",
            std::process::id(),
            fastrand::u64(..)
        ));
        let path = dir.join("token");
        let tokens: Vec<String> = (0..8)
            .map(|i| format!("token-value-{i}-{}", "x".repeat(64)))
            .collect();
        let mut handles = Vec::new();
        for token in tokens.clone() {
            let path = path.clone();
            handles.push(std::thread::spawn(move || {
                for _ in 0..50 {
                    write_token_atomic(&path, &token).expect("concurrent write must not fail");
                }
            }));
        }
        for handle in handles {
            handle.join().expect("writer thread panicked");
        }
        let published = std::fs::read_to_string(&path).expect("token file must exist");
        assert!(
            tokens.iter().any(|t| t == &published),
            "published file must be exactly one writer's complete token, got {} bytes",
            published.len()
        );
        // No leaked temp files.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "stale temp files left behind: {leftovers:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn should_persist_proven_generation_rules() {
        let tok = |iat: i64, exp: i64, jti: &str| {
            make_token(&serde_json::json!({ "iat": iat, "exp": exp, "jti": jti }))
        };
        let proven = tok(100, 200, "jti-A");
        // Empty / unparseable file → persist.
        assert!(should_persist_proven(None, &proven));
        assert!(should_persist_proven(Some("garbage"), &proven));
        // Identical token → no-op.
        assert!(!should_persist_proven(Some(&proven), &proven));
        // Different jti, SAME iat/exp (double renew sibling) → the PROVEN
        // token must win: expiry seconds do not order generations.
        let sibling = tok(100, 200, "jti-B");
        assert!(should_persist_proven(Some(&sibling), &proven));
        // Strictly newer file (renewal forward progress) → keep the file.
        let newer = tok(101, 201, "jti-C");
        assert!(!should_persist_proven(Some(&newer), &proven));
        // Strictly older file → persist the proven token.
        let older = tok(99, 199, "jti-D");
        assert!(should_persist_proven(Some(&older), &proven));
        // Non-MOI proven token never touches the file.
        assert!(!should_persist_proven(None, "plain-astra-token"));
    }

    // Reused-workspace regression: the file holds a previous tenant's token of a
    // LATER generation than the token that just authenticated for the current
    // tenant. Generation ordering is meaningless across identities — comparing
    // (iat, exp) would keep the stale foreign token, which a later env-less
    // startup would then select and be permanently rejected on. The proven
    // current-tenant token must overwrite it.
    #[test]
    fn should_persist_proven_replaces_stale_other_identity() {
        let identity_tok = |sub: &str, workspace: &str, iat: i64, exp: i64| {
            make_token(&serde_json::json!({
                "iat": iat,
                "exp": exp,
                "jti": format!("jti-{sub}"),
                "sub": sub,
                "workspace_id": workspace,
                "iss": "moi-backend",
                "edge_agent_id": format!("edge-{sub}"),
                "purpose": "edge_registration",
            }))
        };
        // Tenant A's leftover file token is a LATER generation than tenant B's
        // freshly-proven token.
        let tenant_a_file = identity_tok("tenant-a", "ws-a", 5_000, 9_000);
        let tenant_b_proven = identity_tok("tenant-b", "ws-b", 100, 200);
        assert!(
            should_persist_proven(Some(&tenant_a_file), &tenant_b_proven),
            "a proven current-identity token must overwrite a later-generation \
             previous-tenant token left in a reused workspace"
        );

        // Sanity: same identity still obeys generation ordering (a strictly
        // newer same-identity file is kept).
        let same_newer = identity_tok("tenant-b", "ws-b", 300, 400);
        assert!(!should_persist_proven(Some(&same_newer), &tenant_b_proven));
    }

    #[test]
    fn parse_valid_claims() {
        let token = make_token(&serde_json::json!({ "exp": 1234567890, "jti": "abc" }));
        let claims = parse_moi_token_claims(&token).expect("parse");
        assert_eq!(claims.exp, 1234567890);
        assert_eq!(claims.jti.as_deref(), Some("abc"));
    }

    #[test]
    fn parse_claims_without_jti() {
        let token = make_token(&serde_json::json!({ "exp": 42 }));
        let claims = parse_moi_token_claims(&token).expect("parse");
        assert_eq!(claims.exp, 42);
        assert!(claims.jti.is_none());
    }

    #[test]
    fn parse_rejects_malformed_tokens() {
        assert!(parse_moi_token_claims("").is_none());
        assert!(parse_moi_token_claims("not-a-token").is_none());
        // JWT-style token without the moi prefix
        assert!(parse_moi_token_claims("eyJhbGciOi.eyJleHAiOjB9.sig").is_none());
        // Missing signature segment
        assert!(parse_moi_token_claims("moi-user-token-v1.eyJleHAiOjB9").is_none());
        // Invalid base64 claims
        assert!(parse_moi_token_claims("moi-user-token-v1.!!!.sig").is_none());
        // Claims missing exp
        let no_exp = make_token(&serde_json::json!({ "jti": "x" }));
        assert!(parse_moi_token_claims(&no_exp).is_none());
    }

    #[test]
    fn should_renew_threshold_decision() {
        let claims = TokenClaims {
            iat: 0,
            exp: 1_000_000,
            jti: None,
            ..TokenClaims::default()
        };
        let week = 7 * 24 * 60 * 60;
        // 10 days remaining — not due.
        assert!(!should_renew(&claims, 1_000_000 - 10 * 86_400, week));
        // 3 days remaining — due.
        assert!(should_renew(&claims, 1_000_000 - 3 * 86_400, week));
        // Already expired — due.
        assert!(should_renew(&claims, 1_000_000 + 1, week));
    }

    #[test]
    fn file_token_preference_logic() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(".astra").join("token");
        let now = 1_000_000;

        // Missing file → none.
        assert!(read_valid_file_token(&path, now).is_none());

        // Valid unexpired moi token with the server-required identity claims →
        // preferred. (No iss/purpose ⇒ not an edge-registration token, so no
        // edge_agent_id/jti required.)
        let valid = make_token(&serde_json::json!({
            "exp": now + 86_400,
            "sub": "user-1",
            "workspace_id": "workspace-1",
        }));
        write_token_atomic(&path, &valid).expect("write");
        assert_eq!(
            read_valid_file_token(&path, now).as_deref(),
            Some(valid.as_str())
        );

        // A valid edge-registration token (iss=moi-backend + purpose) needs the
        // edge_agent_id and jti binding the server requires.
        let valid_edge = make_token(&serde_json::json!({
            "exp": now + 86_400,
            "sub": "user-1",
            "workspace_id": "workspace-1",
            "iss": "moi-backend",
            "purpose": "edge_registration",
            "edge_agent_id": "edge-1",
            "jti": "jti-1",
        }));
        write_token_atomic(&path, &valid_edge).expect("write");
        assert_eq!(
            read_valid_file_token(&path, now).as_deref(),
            Some(valid_edge.as_str())
        );

        // Expired token → rejected.
        let expired = make_token(&serde_json::json!({
            "exp": now - 1,
            "sub": "user-1",
            "workspace_id": "workspace-1",
        }));
        write_token_atomic(&path, &expired).expect("write");
        assert!(read_valid_file_token(&path, now).is_none());

        // The lenient parser accepts these, but the server would reject them, so
        // the file selector must too (no env fallback ⇒ selecting one bricks the
        // edge). Each mutates the otherwise-valid token in exactly one way.

        // Extra (4th) segment — server requires exactly three.
        write_token_atomic(&path, &format!("{valid}.extra")).expect("write");
        assert!(read_valid_file_token(&path, now).is_none());

        // Missing sub — required for every token type.
        let no_sub = make_token(&serde_json::json!({
            "exp": now + 86_400,
            "workspace_id": "workspace-1",
        }));
        write_token_atomic(&path, &no_sub).expect("write");
        assert!(read_valid_file_token(&path, now).is_none());

        // Missing workspace_id — required for every token type.
        let no_workspace = make_token(&serde_json::json!({
            "exp": now + 86_400,
            "sub": "user-1",
        }));
        write_token_atomic(&path, &no_workspace).expect("write");
        assert!(read_valid_file_token(&path, now).is_none());

        // moi-backend issuer without the edge_registration purpose — a
        // pre-binding historical token the server rejects.
        let backend_non_edge = make_token(&serde_json::json!({
            "exp": now + 86_400,
            "sub": "user-1",
            "workspace_id": "workspace-1",
            "iss": "moi-backend",
        }));
        write_token_atomic(&path, &backend_non_edge).expect("write");
        assert!(read_valid_file_token(&path, now).is_none());

        // edge_registration purpose without edge_agent_id / jti — rejected.
        let edge_no_binding = make_token(&serde_json::json!({
            "exp": now + 86_400,
            "sub": "user-1",
            "workspace_id": "workspace-1",
            "iss": "moi-backend",
            "purpose": "edge_registration",
        }));
        write_token_atomic(&path, &edge_no_binding).expect("write");
        assert!(read_valid_file_token(&path, now).is_none());

        // Non-moi content → rejected.
        write_token_atomic(&path, "some-jwt-token").expect("write");
        assert!(read_valid_file_token(&path, now).is_none());

        // Empty / whitespace file → rejected.
        write_token_atomic(&path, "  \n").expect("write");
        assert!(read_valid_file_token(&path, now).is_none());
    }

    #[test]
    fn retry_delay_within_grace_budgets_request_timeout() {
        let grace = Duration::from_secs(300);
        let timeout = Duration::from_secs(30);
        // Early in the window: the full backoff is granted.
        assert_eq!(
            retry_delay_within_grace(
                Duration::from_secs(30),
                Duration::from_secs(30),
                grace,
                timeout
            ),
            Some(Duration::from_secs(30))
        );
        // Late enough that the raw backoff would overrun: clamped so the next
        // attempt keeps a full request timeout of headroom before the deadline.
        assert_eq!(
            retry_delay_within_grace(
                Duration::from_secs(180),
                Duration::from_secs(120),
                grace,
                timeout
            ),
            Some(Duration::from_secs(90))
        );
        // Not enough room for another completing attempt → stop retrying.
        assert_eq!(
            retry_delay_within_grace(
                Duration::from_secs(280),
                Duration::from_secs(30),
                grace,
                timeout
            ),
            None
        );
        // Exactly one request timeout of room left (remaining == timeout) → stop.
        assert_eq!(
            retry_delay_within_grace(
                Duration::from_secs(270),
                Duration::from_secs(10),
                grace,
                timeout
            ),
            None
        );
        // Elapsed already past the grace window → None (saturating, no panic).
        assert_eq!(
            retry_delay_within_grace(
                Duration::from_secs(400),
                Duration::from_secs(30),
                grace,
                timeout
            ),
            None
        );
    }

    // Paused-time regression for the retry schedule: driving the real backoff
    // constants through the grace-deadline helper, every attempt — including the
    // last — must BEGIN early enough to complete within the rotation grace
    // window even when each attempt burns the full request timeout before it
    // fails. The pre-fix fixed schedule [30,60,120] pushed the 4th attempt to
    // t=300, at the boundary.
    #[tokio::test(start_paused = true)]
    async fn fast_retries_stay_within_rotation_grace_window() {
        let grace = ROTATION_GRACE_WINDOW;
        let timeout = RENEW_HTTP_TIMEOUT;
        let start = tokio::time::Instant::now();
        let mut backoff = TRANSIENT_RETRY_SECS.iter().copied();
        let mut attempt_offsets = vec![Duration::ZERO];
        loop {
            // Worst case: the attempt runs the full request timeout before it
            // reports the transient failure.
            tokio::time::sleep(timeout).await;
            let Some(backoff_secs) = backoff.next() else {
                break;
            };
            let elapsed = tokio::time::Instant::now().duration_since(start);
            let Some(delay) = retry_delay_within_grace(
                elapsed,
                Duration::from_secs(backoff_secs),
                grace,
                timeout,
            ) else {
                break;
            };
            tokio::time::sleep(delay).await;
            attempt_offsets.push(tokio::time::Instant::now().duration_since(start));
        }
        for offset in &attempt_offsets {
            assert!(
                *offset + timeout <= grace,
                "attempt starting at {offset:?} cannot complete within the {grace:?} grace window"
            );
        }
        // The window is genuinely used for several recovery attempts, not
        // collapsed to a single try.
        assert!(
            attempt_offsets.len() >= 2,
            "expected multiple in-window retries, got {}",
            attempt_offsets.len()
        );
    }

    #[test]
    fn atomic_write_creates_parent_and_sets_mode() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("dir").join("token");
        write_token_atomic(&path, "secret").expect("write");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "secret");
        // No leftover tmp file.
        assert!(!path.with_extension("tmp").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).expect("meta").permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        // A stale temp file from ANOTHER writer must not block or be touched:
        // temp names are per-writer unique, so foreign leftovers are ignored
        // and the write still lands atomically.
        std::fs::write(path.with_extension("tmp"), "stale").expect("stale tmp");
        write_token_atomic(&path, "secret2").expect("rewrite");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "secret2");
        assert_eq!(
            std::fs::read_to_string(path.with_extension("tmp")).expect("foreign tmp"),
            "stale",
            "another writer's temp file must be left untouched"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).expect("meta").permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
