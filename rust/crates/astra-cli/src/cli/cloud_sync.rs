//! Cloud preference sync with MatrixOne.
//!
//! Tool-health and pattern sync has been removed. What remains is
//! user-preference sync.
//!
//! ## Sync Flow
//!
//! - **Preferences**: [`try_cloud_pull_preferences`] / [`try_cloud_push_preferences`].

use astra_core::resolve_database_name_or;
use astra_services::session_journal;
use astra_services::state_sync::{MatrixOneSyncService, StateSyncService, pref_keys};
use astra_turn_core::tool_health_persistence::ToolHealthEntry;

use super::chat_turn::enqueue_ingestion_pub;
use super::{ExplainMode, SessionState};

/// Result from cloud pull attempt at session start.
pub(super) struct CloudPullResult {
    /// True when MatrixOne was reachable.
    pub cloud_reachable: bool,
}

/// Best-effort MatrixOne pool creation for sync operations.
pub(super) async fn try_connect_matrixone() -> Option<sqlx::Pool<sqlx::MySql>> {
    let host = std::env::var("MATRIXONE_HOST").ok()?;
    let port: u16 = std::env::var("MATRIXONE_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(6001);
    let user = std::env::var("MATRIXONE_USER").unwrap_or_else(|_| "root".to_string());
    let password = std::env::var("MATRIXONE_PASSWORD").unwrap_or_default();
    let database = resolve_database_name_or(&|k| std::env::var(k).ok(), "astra");
    let url = format!("mysql://{user}:{password}@{host}:{port}/{database}");
    sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(std::time::Duration::from_secs(3))
        .idle_timeout(std::time::Duration::from_secs(60))
        .test_before_acquire(true)
        .connect(&url)
        .await
        .ok()
}

/// Check whether MatrixOne is reachable for downstream preference sync.
/// Best-effort: silently returns unreachable when cloud is unavailable.
pub(super) async fn try_cloud_pull(_profile_name: &str) -> CloudPullResult {
    let cloud_reachable = try_connect_matrixone().await.is_some();
    CloudPullResult { cloud_reachable }
}

/// Shut down an ephemeral audit flusher: drop all senders, cancel the token,
/// and await the flusher task so the final batch is flushed to DB.
async fn drain_ephemeral_audit(
    svc: MatrixOneSyncService,
    flusher: astra_services::state_sync::AuditFlusherHandle,
) {
    drop(svc);
    drop(flusher.writer);
    flusher.shutdown.cancel();
    match tokio::time::timeout(std::time::Duration::from_secs(5), flusher.join_handle).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::error!(target: "cloud_sync", "audit flusher panicked: {e}"),
        Err(_) => {
            tracing::warn!(target: "cloud_sync", "audit flusher drain timed out (5s), some entries may be lost")
        }
    }
}

/// Pull user preferences from cloud at session start.
/// Merges cloud preferences into local state (cloud-wins). Returns keys merged (for journal audit).
pub(super) async fn try_cloud_pull_preferences(state: &mut SessionState) -> Vec<String> {
    let pool = match try_connect_matrixone().await {
        Some(p) => p,
        None => return Vec::new(),
    };
    let flusher = astra_services::state_sync::spawn_audit_flusher(pool.clone());
    let svc = MatrixOneSyncService::new(pool, flusher.writer.clone());
    let user_id = astra_core::cli_user_id();
    let out = StateSyncService::pull_all_preferences(&svc, &user_id).await;
    drain_ephemeral_audit(svc, flusher).await;
    match out {
        Ok(prefs) if !prefs.is_empty() => {
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
        Ok(_) => Vec::new(),
        Err(e) => {
            tracing::warn!(
                target: "astra_cli::cloud_sync",
                error = %e,
                "preference pull skipped"
            );
            Vec::new()
        }
    }
}

/// Push user preferences to cloud at session end.
pub(super) async fn try_cloud_push_preferences(state: &SessionState) {
    let pool = match try_connect_matrixone().await {
        Some(p) => p,
        None => return,
    };
    let flusher = astra_services::state_sync::spawn_audit_flusher(pool.clone());
    let svc = MatrixOneSyncService::new(pool, flusher.writer.clone());
    let user_id = astra_core::cli_user_id();

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
    ];
    for (key, value) in &prefs {
        let _ = svc.push_preference(&user_id, key, value).await;
    }
    drain_ephemeral_audit(svc, flusher).await;
}

// ═══════════════════════════════════════════ Journal Helpers ═══════════════════════

/// When set to `1`, `session_startup` also journals a sync marker if MatrixOne was reachable but
/// returned no preferences (audit / connectivity proof).
pub(super) const ASTRA_JOURNAL_CLOUD_EMPTY_ACK: &str = "ASTRA_JOURNAL_CLOUD_EMPTY_ACK";

pub(super) fn cloud_pull_warrants_sync_marker(
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

pub(super) fn should_append_cloud_pull_journal(
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

pub(super) fn append_cloud_pull_sync_journal(
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
