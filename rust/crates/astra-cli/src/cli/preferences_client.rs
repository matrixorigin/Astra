//! Cloud REST client for user preferences.
//!
//! The CLI MUST NOT connect to MatrixOne directly for preference
//! sync — these helpers proxy `pull_all_preferences` and
//! `push_preference` through the server's `/preferences` endpoints
//! so the user is resolved from the auth header (no client-side
//! user_id forging).
//!
//! Used by `cli::cloud_sync` to replace the prior direct
//! `MatrixOneSyncService` calls.

use serde::Deserialize;
use serde_json::json;

const PREFS_HTTP_TIMEOUT_SECS: u64 = 10;

#[derive(Deserialize)]
struct PreferencesResponse {
    preferences: Vec<PreferenceEntry>,
}

#[derive(Deserialize)]
struct PreferenceEntry {
    key: String,
    value: String,
}

fn build_request(
    method: reqwest::Method,
    url: &str,
    token: Option<&str>,
) -> Result<reqwest::RequestBuilder, String> {
    let client = reqwest::Client::builder()
        .no_proxy() // astra server is local/intranet; bypass http_proxy env
        .timeout(std::time::Duration::from_secs(PREFS_HTTP_TIMEOUT_SECS))
        .build()
        .map_err(|e| format!("http client init: {e}"))?;
    let mut req = client.request(method, url);
    if let Some(tok) = token {
        req = req.bearer_auth(tok);
    }
    Ok(req)
}

/// `GET /preferences` — pull every preference for the authed user.
/// Empty `Vec` means cloud reachable but no preferences set; `Err`
/// means the cloud was unreachable or returned a server error.
pub async fn pull_all_preferences(
    cloud_base: &str,
    token: Option<&str>,
) -> Result<Vec<(String, String)>, String> {
    let url = format!(
        "{}/preferences",
        cloud_base.trim_end_matches('/')
    );
    let resp = build_request(reqwest::Method::GET, &url, token)?
        .send()
        .await
        .map_err(|e| format!("network: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("cloud {status}: {body}"));
    }
    let parsed: PreferencesResponse = resp
        .json()
        .await
        .map_err(|e| format!("decode response: {e}"))?;
    Ok(parsed
        .preferences
        .into_iter()
        .map(|p| (p.key, p.value))
        .collect())
}

/// `PUT /preferences/{key}` — push a single preference value.
pub async fn push_preference(
    cloud_base: &str,
    token: Option<&str>,
    key: &str,
    value: &str,
) -> Result<(), String> {
    // URL-encode the key segment minimally — keys today are short
    // ASCII (`explain_mode`, `blocked_tools`) but we don't trust
    // that forever.
    let encoded_key: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.' {
                c.to_string()
            } else {
                format!("%{:02X}", c as u32)
            }
        })
        .collect();
    let url = format!(
        "{}/preferences/{}",
        cloud_base.trim_end_matches('/'),
        encoded_key
    );
    let resp = build_request(reqwest::Method::PUT, &url, token)?
        .json(&json!({ "value": value }))
        .send()
        .await
        .map_err(|e| format!("network: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("cloud {status}: {body}"));
    }
    Ok(())
}

/// Probe whether the cloud preference endpoint is reachable. Used
/// by `try_cloud_pull` to set `cloud_reachable` in the journal
/// marker — same intent as the legacy `try_connect_matrixone()`
/// reachability check, just over HTTP.
pub async fn probe_cloud_reachable(cloud_base: &str, token: Option<&str>) -> bool {
    pull_all_preferences(cloud_base, token).await.is_ok()
}
