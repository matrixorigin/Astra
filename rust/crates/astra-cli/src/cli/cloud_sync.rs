//! Cloud learning and preference sync with MatrixOne.
//!
//! This module handles bidirectional sync of learning data (entity graphs, patterns,
//! calibration) and user preferences between local REPL state and cloud storage.
//!
//! ## Sync Flow
//!
//! - **Pull at startup**: `try_cloud_pull` merges cloud learning state into local modules
//! - **Delta push on exit**: `try_cloud_push_delta` sends only changed data (~90% bandwidth reduction)
//! - **Preferences**: `try_cloud_pull_preferences` and `try_cloud_push_preferences` for user settings
//! - **Conflict resolution**: Optimistic locking with version numbers; conflicts trigger re-pull

use astra_core::resolve_database_name_or;
use astra_runtime::pipeline::persistence::{
    DeltaSnapshot, LearningSnapshot, ToolHealthEntry, clear_dirty_learning_in_modules,
    export_dirty_learning_from_modules, export_from_modules_with_health, export_tool_health_delta,
    has_dirty_learning_data, merge_into_modules, merge_tool_health, save_synced_tool_health,
};
use astra_services::session_journal;
use astra_services::state_sync::{MatrixOneSyncService, StateSyncService, pref_keys};
use crossterm::style::Stylize;
use std::sync::{Arc, Mutex};

use super::repl_turn::enqueue_ingestion_pub;
use super::theme;
use super::{ExplainMode, ReplState};

/// Result from cloud pull including tool health and version for optimistic locking.
pub(super) struct CloudPullResult {
    pub tool_health: Vec<ToolHealthEntry>,
    pub version: Option<i64>,
    /// True when MatrixOne was reachable and versioned pull was attempted (may return no row).
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
        .connect(&url)
        .await
        .ok()
}

/// Merge a JSON learning snapshot into live pipeline modules.
pub(super) fn merge_learning_snapshot(
    json: &str,
    entity_graph: &Arc<Mutex<astra_runtime::pipeline::entity::EntityGraph>>,
    pattern_library: &Arc<Mutex<astra_runtime::pipeline::pattern::PatternLibrary>>,
    calibrator: &Arc<Mutex<astra_runtime::pipeline::calibration::ProgressiveCalibrator>>,
) {
    if json.trim().is_empty() {
        return;
    }
    match serde_json::from_str::<LearningSnapshot>(json) {
        Ok(snapshot) => {
            merge_into_modules(&snapshot, entity_graph, pattern_library, calibrator);
            let n = snapshot.entities.len() + snapshot.patterns.len();
            if n > 0 {
                eprintln!(
                    "  {} Merged learning: {} entities, {} patterns",
                    theme::icon_ok(),
                    snapshot.entities.len(),
                    snapshot.patterns.len(),
                );
            }
        }
        Err(e) => {
            eprintln!(
                "{}",
                format!("  ⚠ Learning snapshot format changed (starting fresh): {e}").yellow()
            );
        }
    }
}

/// Try to pull learning state from MatrixOne and merge into live modules.
/// Best-effort: silently skips if cloud is unavailable.
/// Returns tool health entries and cloud version for optimistic locking.
pub(super) async fn try_cloud_pull(
    profile_name: &str,
    entity_graph: &Arc<Mutex<astra_runtime::pipeline::entity::EntityGraph>>,
    pattern_library: &Arc<Mutex<astra_runtime::pipeline::pattern::PatternLibrary>>,
    calibrator: &Arc<Mutex<astra_runtime::pipeline::calibration::ProgressiveCalibrator>>,
) -> CloudPullResult {
    let pool = match try_connect_matrixone().await {
        Some(p) => p,
        None => {
            return CloudPullResult {
                tool_health: Vec::new(),
                version: None,
                cloud_reachable: false,
            };
        }
    };
    let svc = MatrixOneSyncService::new(pool);
    let user_id = std::env::var("MO_USER_ID").unwrap_or_else(|_| "local".to_string());
    match StateSyncService::pull_learning_versioned(&svc, &user_id, profile_name).await {
        Ok(Some(versioned)) => {
            // Parse snapshot to extract tool health before merging entities/patterns
            let cloud_health = serde_json::from_str::<LearningSnapshot>(&versioned.json)
                .map(|s| s.tool_health)
                .unwrap_or_default();
            merge_learning_snapshot(&versioned.json, entity_graph, pattern_library, calibrator);
            eprintln!(
                "{}",
                format!("  ✓ Cloud learning merged (v{})", versioned.version).dim()
            );
            CloudPullResult {
                tool_health: cloud_health,
                version: Some(versioned.version),
                cloud_reachable: true,
            }
        }
        Ok(None) => CloudPullResult {
            tool_health: Vec::new(),
            version: None,
            cloud_reachable: true,
        },
        Err(e) => {
            eprintln!("{}", format!("  ⚠ Cloud pull skipped: {e}").dim());
            CloudPullResult {
                tool_health: Vec::new(),
                version: None,
                cloud_reachable: true,
            }
        }
    }
}

/// Push learning state to cloud with optimistic locking.
/// Returns the new cloud version if successful, or None on conflict/failure.
/// On conflict, the caller should pull fresh data and retry.
pub(super) async fn try_cloud_push_versioned(
    profile_name: &str,
    entity_graph: &Arc<Mutex<astra_runtime::pipeline::entity::EntityGraph>>,
    pattern_library: &Arc<Mutex<astra_runtime::pipeline::pattern::PatternLibrary>>,
    calibrator: &Arc<Mutex<astra_runtime::pipeline::calibration::ProgressiveCalibrator>>,
    tool_health: &[ToolHealthEntry],
    expected_version: Option<i64>,
) -> Option<i64> {
    let pool = match try_connect_matrixone().await {
        Some(p) => p,
        None => return None,
    };
    let snapshot =
        export_from_modules_with_health(entity_graph, pattern_library, calibrator, tool_health);
    let json = match serde_json::to_string(&snapshot) {
        Ok(j) => j,
        Err(_) => return None,
    };
    let svc = MatrixOneSyncService::new(pool);
    let user_id = std::env::var("MO_USER_ID").unwrap_or_else(|_| "local".to_string());
    let result = StateSyncService::push_learning_versioned(
        &svc,
        &user_id,
        profile_name,
        &json,
        snapshot.entities.len() as u32,
        snapshot.patterns.len() as u32,
        snapshot.calibration.is_some(),
        expected_version,
    )
    .await;

    if result.is_conflict {
        eprintln!(
            "{}",
            "  ⚠ Cloud sync conflict (another session updated)".yellow()
        );
        return None;
    }

    if result.success {
        if let Err(e) = save_synced_tool_health(profile_name, tool_health) {
            eprintln!(
                "{}",
                format!("  ⚠ Tool-health sync metadata not saved: {e}").dim()
            );
        }
        if let Some(v) = result.new_version {
            return Some(v);
        }
    } else if !result.message.is_empty() {
        eprintln!(
            "{}",
            format!("  ⚠ Cloud push skipped: {}", result.message).dim()
        );
    }
    result.new_version
}

/// Push only changed learning data to cloud using delta sync.
///
/// Delta sync reduces bandwidth by ~90%: full snapshot ~40KB, delta ~2-5KB.
/// Falls back to full push if delta export fails or is empty.
///
/// Returns the new cloud version if successful, None otherwise.
pub(super) async fn try_cloud_push_delta(
    profile_name: &str,
    entity_graph: &Arc<Mutex<astra_runtime::pipeline::entity::EntityGraph>>,
    pattern_library: &Arc<Mutex<astra_runtime::pipeline::pattern::PatternLibrary>>,
    calibrator: &Arc<Mutex<astra_runtime::pipeline::calibration::ProgressiveCalibrator>>,
    tool_health_entries: &[ToolHealthEntry],
    synced_tool_health_entries: &mut Vec<ToolHealthEntry>,
    expected_version: Option<i64>,
) -> Option<i64> {
    let learning_dirty = has_dirty_learning_data(entity_graph, pattern_library, calibrator);
    let tool_health_deltas =
        export_tool_health_delta(tool_health_entries, synced_tool_health_entries);

    if !learning_dirty && tool_health_deltas.is_empty() {
        return expected_version;
    }

    let mut delta = export_dirty_learning_from_modules(entity_graph, pattern_library, calibrator)
        .unwrap_or(DeltaSnapshot {
            baseline_epoch: 0,
            entity_deltas: Vec::new(),
            pattern_deltas: Vec::new(),
            calibration: None,
            tool_health_deltas: Vec::new(),
            delta_count: 0,
        });

    delta.delta_count += tool_health_deltas.len() as u32;
    delta.tool_health_deltas = tool_health_deltas;

    let delta_json = match serde_json::to_string(&delta) {
        Ok(j) => j,
        Err(_) => return None,
    };

    let pool = match try_connect_matrixone().await {
        Some(p) => p,
        None => return None,
    };

    let svc = MatrixOneSyncService::new(pool);
    let user_id = std::env::var("MO_USER_ID").unwrap_or_else(|_| "local".to_string());

    let result =
        StateSyncService::push_delta(&svc, &user_id, profile_name, &delta_json, expected_version)
            .await;

    if result.is_conflict {
        eprintln!(
            "{}",
            "  ⚠ Delta sync conflict (another session updated)".yellow()
        );
        return None;
    }

    if result.success {
        let synced_tool_health_snapshot = tool_health_entries.to_vec();
        *synced_tool_health_entries = synced_tool_health_snapshot.clone();
        clear_dirty_learning_in_modules(entity_graph, pattern_library, calibrator);
        if let Err(e) = save_synced_tool_health(profile_name, &synced_tool_health_snapshot) {
            eprintln!(
                "{}",
                format!("  ⚠ Tool-health sync metadata not saved: {e}").dim()
            );
        }

        if let Some(v) = result.new_version {
            eprintln!(
                "{}",
                format!(
                    "  ✓ Delta synced to cloud (v{}, {} items, {}B)",
                    v,
                    delta.delta_count,
                    delta_json.len()
                )
                .dim()
            );
            return Some(v);
        }
        eprintln!(
            "{}",
            format!("  ✓ Delta synced ({} items)", delta.delta_count).dim()
        );
    } else {
        eprintln!(
            "{}",
            format!("  ⚠ Delta push skipped: {}", result.message).dim()
        );
    }
    result.new_version
}

/// Pull user preferences from cloud at session start.
/// Merges cloud preferences into local state (cloud-wins). Returns keys merged (for journal audit).
pub(super) async fn try_cloud_pull_preferences(state: &mut ReplState) -> Vec<String> {
    let pool = match try_connect_matrixone().await {
        Some(p) => p,
        None => return Vec::new(),
    };
    let svc = MatrixOneSyncService::new(pool);
    let user_id = std::env::var("MO_USER_ID").unwrap_or_else(|_| "local".to_string());
    match StateSyncService::pull_all_preferences(&svc, &user_id).await {
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
                        // Merge cloud-persisted blocked tools into tool_health_entries
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
                                        total_calls: 3,
                                        total_failures: 3,
                                        failure_rate: 1.0,
                                        last_updated_epoch: 0,
                                    });
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            eprintln!(
                "{}",
                format!("  ✓ Pulled {} preferences from cloud", prefs.len()).dim()
            );
            keys
        }
        Ok(_) => Vec::new(), // no cloud prefs yet
        Err(e) => {
            eprintln!("{}", format!("  ⚠ Preference pull skipped: {e}").dim());
            Vec::new()
        }
    }
}

/// Push user preferences to cloud at session end.
pub(super) async fn try_cloud_push_preferences(state: &ReplState) {
    let pool = match try_connect_matrixone().await {
        Some(p) => p,
        None => return,
    };
    let svc = MatrixOneSyncService::new(pool);
    let user_id = std::env::var("MO_USER_ID").unwrap_or_else(|_| "local".to_string());

    // Collect blocked/deprioritized tools from health entries
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
    // Silently succeed — only warn on failure
}

// ═══════════════════════════════════════════ Journal Helpers ═══════════════════════

/// When set to `1`, `repl_startup` also journals a sync marker if MatrixOne was reachable but
/// returned no learning rows, tool health, or preferences (audit / connectivity proof).
pub(super) const ASTRA_JOURNAL_CLOUD_EMPTY_ACK: &str = "ASTRA_JOURNAL_CLOUD_EMPTY_ACK";

pub(super) fn cloud_pull_warrants_sync_marker(
    pull: &CloudPullResult,
    pref_keys: &[String],
) -> bool {
    pull.cloud_reachable
        && (pull.version.is_some() || !pull.tool_health.is_empty() || !pref_keys.is_empty())
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
    state: &ReplState,
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
        pull.version,
        pull.version.is_some(),
        pull.tool_health.len(),
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

/// Re-sync learning and preferences from cloud after authentication.
pub(crate) async fn post_auth_cloud_resync(profile: Option<&str>, state: &mut ReplState) {
    let profile_name = profile.unwrap_or("default");
    let (Some(eg), Some(pl), Some(cal)) = (
        state.entity_graph.as_ref(),
        state.pattern_library.as_ref(),
        state.calibrator.as_ref(),
    ) else {
        return;
    };
    let pull = try_cloud_pull(profile_name, eg, pl, cal).await;
    state.cloud_learning_version = pull.version.or(state.cloud_learning_version);
    if !pull.tool_health.is_empty() {
        let (merged, _, _) = merge_tool_health(&state.tool_health_entries, &pull.tool_health);
        state.tool_health_entries = merged;
    }
    let pref_keys = try_cloud_pull_preferences(state).await;
    append_cloud_pull_sync_journal(state, profile_name, "post_login", &pull, &pref_keys);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloud_pull_result_default_not_reachable() {
        let result = CloudPullResult {
            tool_health: Vec::new(),
            version: None,
            cloud_reachable: false,
        };
        assert!(!result.cloud_reachable);
        assert!(result.version.is_none());
    }

    #[test]
    fn sync_marker_logic() {
        let empty_pull = CloudPullResult {
            tool_health: Vec::new(),
            version: None,
            cloud_reachable: true,
        };
        assert!(!cloud_pull_warrants_sync_marker(&empty_pull, &[]));

        let versioned_pull = CloudPullResult {
            tool_health: Vec::new(),
            version: Some(42),
            cloud_reachable: true,
        };
        assert!(cloud_pull_warrants_sync_marker(&versioned_pull, &[]));

        // With pref keys
        assert!(cloud_pull_warrants_sync_marker(
            &empty_pull,
            &["explain_mode".to_string()]
        ));
    }

    #[test]
    fn journal_append_decision() {
        let unreachable = CloudPullResult {
            tool_health: Vec::new(),
            version: None,
            cloud_reachable: false,
        };
        assert!(!should_append_cloud_pull_journal(
            &unreachable,
            &[],
            "startup"
        ));

        // post_login always journals if reachable
        let reachable_empty = CloudPullResult {
            tool_health: Vec::new(),
            version: None,
            cloud_reachable: true,
        };
        assert!(should_append_cloud_pull_journal(
            &reachable_empty,
            &[],
            "post_login"
        ));
    }
}
