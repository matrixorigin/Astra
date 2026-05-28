//! Cloud preference sync via server REST.
//!
//! Edge-cloud contract: the CLI MUST NOT connect to MatrixOne
//! directly. This module previously built a `sqlx::Pool` and
//! invoked `MatrixOneSyncService` in-process; both moved to the
//! server (`/preferences` endpoints) so the only way the CLI
//! reaches the preference store is through HTTP.
//!
//! ## Sync Flow
//!
//! - **Preferences**: [`try_cloud_pull_preferences`] / [`try_cloud_push_preferences`].
//!   Both go through [`crate::preferences_client`] now.

use astra_services::session_journal;
use astra_services::state_sync::pref_keys;
use astra_turn_core::tool_health_persistence::ToolHealthEntry;

use super::chat_turn::enqueue_ingestion_pub;
use crate::{ExplainMode, SessionState};

/// Result from cloud pull attempt at session start.
pub(crate) struct CloudPullResult {
    /// True when the server's preferences endpoint responded
    /// successfully (regardless of whether it returned data).
    pub cloud_reachable: bool,
}

/// Parse a boolean preference value. Accepts "true"/"1"/"yes"/"on" as true,
/// "false"/"0"/"no"/"off" as false, anything else returns the default.
fn parse_bool_pref(value: &str, default: bool) -> bool {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => true,
        "false" | "0" | "no" | "off" => false,
        _ => default,
    }
}

/// Resolve the astra server base URL. Returns `None` when no server
/// is configured (offline mode). Reads `ASTRA_API_URL`.
fn resolve_cloud_base() -> Option<String> {
    std::env::var("ASTRA_API_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim_end_matches('/').to_string())
}

/// Check whether the cloud preference endpoint is reachable.
/// Best-effort: returns `cloud_reachable: false` when cloud is
/// unconfigured or unreachable.
pub(crate) async fn try_cloud_pull(profile_name: &str) -> CloudPullResult {
    let Some(cloud_base) = resolve_cloud_base() else {
        return CloudPullResult {
            cloud_reachable: false,
        };
    };
    let token = super::session_runtime::current_access_token(Some(profile_name));
    let cloud_reachable =
        crate::cli::preferences_client::probe_cloud_reachable(&cloud_base, token.as_deref()).await;
    CloudPullResult { cloud_reachable }
}

/// Pull user preferences from cloud at session start.
/// Merges cloud preferences into local state (cloud-wins). Returns
/// keys merged (for journal audit).
pub(crate) async fn try_cloud_pull_preferences(state: &mut SessionState) -> Vec<String> {
    let Some(cloud_base) = resolve_cloud_base() else {
        return Vec::new();
    };
    // Best-effort: pick token from whichever profile the session
    // currently holds. Empty token still works for local dev
    // servers without auth; the server's auth_service decides.
    let token = super::session_runtime::current_access_token(None);
    let prefs =
        match crate::cli::preferences_client::pull_all_preferences(&cloud_base, token.as_deref())
            .await
        {
            Ok(prefs) => prefs,
            Err(e) => {
                tracing::warn!(
                    target: "astra_cli::cloud_sync",
                    error = %e,
                    "preference pull skipped"
                );
                return Vec::new();
            }
        };
    if prefs.is_empty() {
        return Vec::new();
    }
    let keys: Vec<String> = prefs.iter().map(|(k, _)| k.clone()).collect();
    for (key, value) in &prefs {
        match key.as_str() {
            pref_keys::EXPLAIN_MODE => {
                state.explain = match value.as_str() {
                    "on" => ExplainMode::On,
                    "verbose" => ExplainMode::Verbose,
                    _ => ExplainMode::Off,
                };
            }
            pref_keys::BLOCKED_TOOLS => {
                if let Ok(tools) = serde_json::from_str::<Vec<String>>(value) {
                    let existing: std::collections::HashSet<String> = state
                        .tool_health_entries
                        .iter()
                        .map(|e| e.name.clone())
                        .collect();
                    for tool_name in tools {
                        if !existing.contains(&tool_name) {
                            state.tool_health_entries.push(ToolHealthEntry {
                                name: tool_name,
                                total_calls: 0,
                                total_failures: 0,
                                failure_rate: 0.0,
                                last_updated_epoch: 0,
                                recent_outcomes: vec![],
                            });
                        }
                    }
                }
            }
            pref_keys::AUTO_MEMORY_ENABLED => {
                state.auto_memory_enabled = parse_bool_pref(value, true);
            }
            pref_keys::NOTIFICATIONS_ENABLED => {
                state.notifications_enabled = parse_bool_pref(value, true);
            }
            pref_keys::NOTIFICATION_METHOD => match value.parse() {
                Ok(method) => state.notification_method = method,
                Err(err) => tracing::warn!(
                    value = %value,
                    error = %err,
                    "ignoring invalid notification_method preference"
                ),
            },
            pref_keys::NOTIFICATION_THRESHOLD_SECS => {
                if let Ok(n) = value.parse::<u64>() {
                    state.notification_threshold_secs = n;
                }
            }
            _ => {}
        }
    }
    // Status is recorded in the journal (`append_cloud_pull_sync_journal`)
    // and reflected in `/state` / `/account`. We intentionally do not
    // write to stderr here: this path runs after `/login` while the
    // TUI owns the terminal, and a stray "✓ Pulled N preferences"
    // line would scribble across the rendered viewport.
    keys
}

/// Push user preferences to cloud at session end.
pub(crate) async fn try_cloud_push_preferences(state: &SessionState) {
    let Some(cloud_base) = resolve_cloud_base() else {
        return;
    };
    let token = super::session_runtime::current_access_token(None);

    let blocked: Vec<String> = state
        .tool_health_entries
        .iter()
        .filter(|e| e.failure_rate >= 1.0)
        .map(|e| e.name.clone())
        .collect();
    let blocked_json = serde_json::to_string(&blocked).unwrap_or_else(|_| "[]".to_string());

    let prefs = [
        (pref_keys::EXPLAIN_MODE, state.explain.to_string()),
        (pref_keys::BLOCKED_TOOLS, blocked_json),
        (
            pref_keys::AUTO_MEMORY_ENABLED,
            state.auto_memory_enabled.to_string(),
        ),
        (
            pref_keys::NOTIFICATIONS_ENABLED,
            state.notifications_enabled.to_string(),
        ),
        (
            pref_keys::NOTIFICATION_METHOD,
            state.notification_method.to_string(),
        ),
        (
            pref_keys::NOTIFICATION_THRESHOLD_SECS,
            state.notification_threshold_secs.to_string(),
        ),
    ];
    for (key, value) in &prefs {
        if let Err(e) = crate::cli::preferences_client::push_preference(
            &cloud_base,
            token.as_deref(),
            key,
            value,
        )
        .await
        {
            tracing::warn!(
                target: "astra_cli::cloud_sync",
                error = %e,
                key = %key,
                "preference push failed"
            );
        }
    }
}

// ═══════════════════════════════════════════ Journal Helpers ═══════════════════════

/// When set to `1`, `session_startup` also journals a sync marker if MatrixOne was reachable but
/// returned no preferences (audit / connectivity proof).
pub(crate) const ASTRA_JOURNAL_CLOUD_EMPTY_ACK: &str = "ASTRA_JOURNAL_CLOUD_EMPTY_ACK";

pub(crate) fn cloud_pull_warrants_sync_marker(
    pull: &CloudPullResult,
    pref_keys: &[String],
) -> bool {
    pull.cloud_reachable && !pref_keys.is_empty()
}

fn cloud_pull_empty_ack_desired_for_source(source: &str) -> bool {
    if source == "post_login" {
        return true;
    }
    std::env::var(ASTRA_JOURNAL_CLOUD_EMPTY_ACK).ok().as_deref() == Some("1")
}

pub(crate) fn should_append_cloud_pull_journal(
    pull: &CloudPullResult,
    pref_keys: &[String],
    source: &str,
) -> bool {
    if !pull.cloud_reachable {
        return false;
    }
    if cloud_pull_warrants_sync_marker(pull, pref_keys) {
        return true;
    }
    cloud_pull_empty_ack_desired_for_source(source)
}

pub(crate) fn append_cloud_pull_sync_journal(
    state: &SessionState,
    profile: &str,
    source: &str,
    pull: &CloudPullResult,
    pref_keys: &[String],
) {
    if !should_append_cloud_pull_journal(pull, pref_keys, source) {
        return;
    }
    let Some(sid) = state.session_id.as_deref() else {
        return;
    };
    let reachable_empty_ack =
        pull.cloud_reachable && !cloud_pull_warrants_sync_marker(pull, pref_keys);
    let evt = session_journal::JournalEvent::cloud_pull_sync_marker(
        Some(sid),
        profile,
        source,
        pref_keys,
        reachable_empty_ack,
    );
    let Ok(writer) = session_journal::JournalWriter::new(sid) else {
        return;
    };
    if writer.append(&evt).is_ok() {
        enqueue_ingestion_pub(state, &evt);
    }
}

/// Re-sync preferences from cloud after authentication.
pub(crate) async fn post_auth_cloud_resync(profile: Option<&str>, state: &mut SessionState) {
    let profile_name = profile.unwrap_or("default");
    let pull = try_cloud_pull(profile_name).await;
    let pref_keys = try_cloud_pull_preferences(state).await;
    append_cloud_pull_sync_journal(state, profile_name, "post_login", &pull, &pref_keys);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloud_pull_result_default_not_reachable() {
        let result = CloudPullResult {
            cloud_reachable: false,
        };
        assert!(!result.cloud_reachable);
    }

    #[test]
    fn sync_marker_logic() {
        let empty_pull = CloudPullResult {
            cloud_reachable: true,
        };
        assert!(!cloud_pull_warrants_sync_marker(&empty_pull, &[]));

        // With pref keys
        assert!(cloud_pull_warrants_sync_marker(
            &empty_pull,
            &["explain_mode".to_string()]
        ));
    }

    #[test]
    fn journal_append_decision() {
        let unreachable = CloudPullResult {
            cloud_reachable: false,
        };
        assert!(!should_append_cloud_pull_journal(
            &unreachable,
            &[],
            "startup"
        ));

        // post_login always journals if reachable
        let reachable_empty = CloudPullResult {
            cloud_reachable: true,
        };
        assert!(should_append_cloud_pull_journal(
            &reachable_empty,
            &[],
            "post_login"
        ));
    }
}
