use super::*;
use cloud_sync::{
    CloudPullResult, cloud_pull_warrants_sync_marker, should_append_cloud_pull_journal,
    try_connect_matrixone,
};

// ── slash_health::format_sync_age tests ────────────────────────────────────────────

#[test]
fn format_sync_age_rfc3339() {
    let now = chrono::Utc::now();
    let ts = now.to_rfc3339();
    let age = slash_health::format_sync_age(&ts);
    // Should be "just now" or "0s ago" or "1s ago"
    assert!(
        age.contains("s ago") || age == "just now",
        "unexpected age for just-now timestamp: {age}"
    );
}

#[test]
fn format_sync_age_minutes_ago() {
    let now = chrono::Utc::now();
    let five_min_ago = now - chrono::Duration::minutes(5);
    let ts = five_min_ago.to_rfc3339();
    let age = slash_health::format_sync_age(&ts);
    assert!(
        age.contains("m ago"),
        "expected minutes-ago format, got: {age}"
    );
}

#[test]
fn format_sync_age_hours_ago() {
    let now = chrono::Utc::now();
    let two_hours_ago = now - chrono::Duration::hours(2);
    let ts = two_hours_ago.to_rfc3339();
    let age = slash_health::format_sync_age(&ts);
    assert!(
        age.contains("h ago"),
        "expected hours-ago format, got: {age}"
    );
}

#[test]
fn format_sync_age_days_ago() {
    let now = chrono::Utc::now();
    let three_days_ago = now - chrono::Duration::days(3);
    let ts = three_days_ago.to_rfc3339();
    let age = slash_health::format_sync_age(&ts);
    assert!(
        age.contains("d ago"),
        "expected days-ago format, got: {age}"
    );
}

#[test]
fn format_sync_age_mysql_datetime() {
    // MySQL DATETIME without timezone — should parse as UTC
    let age = slash_health::format_sync_age("2020-01-01 00:00:00");
    assert!(
        age.contains("d ago"),
        "expected days-ago for old mysql datetime, got: {age}"
    );
}

#[test]
fn format_sync_age_unparseable_returns_raw() {
    let raw = "not-a-timestamp";
    let age = slash_health::format_sync_age(raw);
    assert_eq!(age, raw, "unparseable should return raw string");
}

#[test]
fn display_sync_status_no_crash_all_none() {
    let status = astra_services::SyncStatus::default();
    // Just verify no panic — output goes to stderr
    slash_health::display_sync_status(&status);
}

#[test]
fn display_sync_status_no_crash_full_data() {
    let status = astra_services::SyncStatus {
        learning_last_push: Some(chrono::Utc::now().to_rfc3339()),
        learning_last_pull: Some(chrono::Utc::now().to_rfc3339()),
        preferences_last_sync: Some(chrono::Utc::now().to_rfc3339()),
        pending_pushes: 2,
        last_error: Some("connection reset by peer".into()),
        cloud_version: None,
    };
    slash_health::display_sync_status(&status);
}

#[tokio::test]
async fn slash_health_offline_shows_cloud_section() {
    let api = astra_thin_client::ThinClient::new("http://unused", None).unwrap();
    let selector = tool_selector::TfIdfSelector::new(tool_registry::ToolRegistry::new(
        edge_tools::all_tool_schemas(),
    ));
    let mut state = ReplState::default();
    // No matrix runtime — should show "Offline" in cloud section
    assert!(state.matrix_runtime.is_none());
    let exit = handle_slash_command("/health", &api, None, &mut state, None, &selector)
        .await
        .unwrap();
    assert!(!exit);
}

// ── Cloud sync regression tests (block_on panic fix cc6d011) ────
// These tests verify the async cloud sync functions don't panic when
// called from within a tokio runtime (the original bug was block_on
// inside an existing runtime). We unset MATRIXONE_HOST so they take
// the graceful-fallback path.

#[tokio::test]
async fn try_connect_matrixone_returns_none_without_env_vars() {
    // Safety: test-only, single-threaded tokio runtime
    unsafe {
        std::env::remove_var("MATRIXONE_HOST");
    }
    let pool = try_connect_matrixone().await;
    assert!(
        pool.is_none(),
        "Without MATRIXONE_HOST, pool should be None"
    );
}

#[test]
fn cloud_pull_warrants_sync_marker_only_when_reachable_and_nonempty() {
    let dead = CloudPullResult {
        tool_health: Vec::new(),
        version: None,
        cloud_reachable: false,
    };
    assert!(!cloud_pull_warrants_sync_marker(&dead, &[]));
    let offline_version = CloudPullResult {
        tool_health: Vec::new(),
        version: Some(9),
        cloud_reachable: false,
    };
    assert!(!cloud_pull_warrants_sync_marker(&offline_version, &[]));
    let online_empty = CloudPullResult {
        tool_health: Vec::new(),
        version: None,
        cloud_reachable: true,
    };
    assert!(!cloud_pull_warrants_sync_marker(&online_empty, &[]));
    let online_version = CloudPullResult {
        tool_health: Vec::new(),
        version: Some(3),
        cloud_reachable: true,
    };
    assert!(cloud_pull_warrants_sync_marker(&online_version, &[]));
    assert!(cloud_pull_warrants_sync_marker(
        &online_empty,
        &["explain_mode".into()]
    ));
}

#[test]
fn should_append_cloud_pull_journal_post_login_reachable_empty() {
    let pull = CloudPullResult {
        tool_health: Vec::new(),
        version: None,
        cloud_reachable: true,
    };
    assert!(should_append_cloud_pull_journal(&pull, &[], "post_login"));
}

#[serial_test::serial]
#[test]
fn should_append_cloud_pull_journal_repl_startup_empty_without_env() {
    unsafe {
        std::env::remove_var(super::ASTRA_JOURNAL_CLOUD_EMPTY_ACK);
    }
    let pull = CloudPullResult {
        tool_health: Vec::new(),
        version: None,
        cloud_reachable: true,
    };
    assert!(!should_append_cloud_pull_journal(
        &pull,
        &[],
        "repl_startup"
    ));
}

#[serial_test::serial]
#[test]
fn should_append_repl_startup_when_empty_ack_env_set() {
    let pull = CloudPullResult {
        tool_health: Vec::new(),
        version: None,
        cloud_reachable: true,
    };
    unsafe {
        std::env::remove_var(super::ASTRA_JOURNAL_CLOUD_EMPTY_ACK);
    }
    assert!(!should_append_cloud_pull_journal(
        &pull,
        &[],
        "repl_startup"
    ));
    unsafe {
        std::env::set_var(super::ASTRA_JOURNAL_CLOUD_EMPTY_ACK, "1");
    }
    assert!(should_append_cloud_pull_journal(&pull, &[], "repl_startup"));
    unsafe {
        std::env::remove_var(super::ASTRA_JOURNAL_CLOUD_EMPTY_ACK);
    }
}

#[test]
fn append_cloud_pull_sync_journal_skips_without_session_id() {
    let pull = CloudPullResult {
        tool_health: Vec::new(),
        version: Some(1),
        cloud_reachable: true,
    };
    let state = ReplState::default();
    append_cloud_pull_sync_journal(&state, "default", "repl_startup", &pull, &[]);
}

#[test]
fn append_cloud_pull_sync_journal_writes_sync_marker_jsonl() {
    let sid = format!("test-cloud-pull-journal-{}", uuid::Uuid::new_v4());
    let state = ReplState {
        session_id: Some(sid.clone()),
        ..Default::default()
    };
    let pull = CloudPullResult {
        tool_health: Vec::new(),
        version: Some(99),
        cloud_reachable: true,
    };
    let prefs = vec!["explain_mode".to_string()];
    append_cloud_pull_sync_journal(&state, "work", "repl_startup", &pull, &prefs);
    let events = session_journal::read_journal(&sid).expect("read journal");
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].event_type,
        session_journal::JournalEventType::SyncMarker
    );
    let cp = events[0]
        .metadata
        .as_ref()
        .and_then(|m| m.get("cloud_pull"))
        .expect("cloud_pull");
    assert_eq!(cp.get("profile").and_then(|v| v.as_str()), Some("work"));
    assert_eq!(
        cp.get("learning_version").and_then(|v| v.as_i64()),
        Some(99)
    );
    assert_eq!(
        cp.get("reachable_empty_ack").and_then(|v| v.as_bool()),
        Some(false)
    );
    std::fs::remove_file(session_journal::journal_file_path(&sid)).ok();
}

#[test]
fn append_cloud_pull_post_login_reachable_empty_writes_marker() {
    let sid = format!("test-cloud-pull-empty-{}", uuid::Uuid::new_v4());
    let state = ReplState {
        session_id: Some(sid.clone()),
        ..Default::default()
    };
    let pull = CloudPullResult {
        tool_health: Vec::new(),
        version: None,
        cloud_reachable: true,
    };
    append_cloud_pull_sync_journal(&state, "default", "post_login", &pull, &[]);
    let events = session_journal::read_journal(&sid).expect("read journal");
    assert_eq!(events.len(), 1);
    let cp = events[0]
        .metadata
        .as_ref()
        .and_then(|m| m.get("cloud_pull"))
        .expect("cloud_pull");
    assert_eq!(
        cp.get("reachable_empty_ack").and_then(|v| v.as_bool()),
        Some(true)
    );
    std::fs::remove_file(session_journal::journal_file_path(&sid)).ok();
}

#[tokio::test]
async fn try_cloud_pull_returns_empty_without_matrixone() {
    unsafe {
        std::env::remove_var("MATRIXONE_HOST");
    }
    let eg = std::sync::Arc::new(std::sync::Mutex::new(
        astra_runtime::pipeline::entity::EntityGraph::new(),
    ));
    let pl = std::sync::Arc::new(std::sync::Mutex::new(
        astra_runtime::pipeline::pattern::PatternLibrary::new(),
    ));
    let cal = std::sync::Arc::new(std::sync::Mutex::new(
        astra_runtime::pipeline::calibration::ProgressiveCalibrator::new(0.15),
    ));
    let result = try_cloud_pull("default", &eg, &pl, &cal).await;
    assert!(
        result.tool_health.is_empty(),
        "Without MatrixOne, cloud pull should return empty tool health"
    );
    assert!(
        result.version.is_none(),
        "Without MatrixOne, cloud pull should return no version"
    );
    assert!(
        !result.cloud_reachable,
        "Without MatrixOne, cloud should be unreachable"
    );
}

#[tokio::test]
async fn try_cloud_push_is_noop_without_matrixone() {
    unsafe {
        std::env::remove_var("MATRIXONE_HOST");
    }
    let eg = std::sync::Arc::new(std::sync::Mutex::new(
        astra_runtime::pipeline::entity::EntityGraph::new(),
    ));
    let pl = std::sync::Arc::new(std::sync::Mutex::new(
        astra_runtime::pipeline::pattern::PatternLibrary::new(),
    ));
    let cal = std::sync::Arc::new(std::sync::Mutex::new(
        astra_runtime::pipeline::calibration::ProgressiveCalibrator::new(0.15),
    ));
    // Should not panic (was the original bug)
    // Use versioned API (None = new snapshot or unconditional push)
    let _result = try_cloud_push_versioned("default", &eg, &pl, &cal, &[], None).await;
}

#[tokio::test]
async fn try_cloud_push_delta_is_noop_without_matrixone() {
    unsafe {
        std::env::remove_var("MATRIXONE_HOST");
    }
    let eg = std::sync::Arc::new(std::sync::Mutex::new(
        astra_runtime::pipeline::entity::EntityGraph::new(),
    ));
    let pl = std::sync::Arc::new(std::sync::Mutex::new(
        astra_runtime::pipeline::pattern::PatternLibrary::new(),
    ));
    let cal = std::sync::Arc::new(std::sync::Mutex::new(
        astra_runtime::pipeline::calibration::ProgressiveCalibrator::new(0.15),
    ));
    let mut synced = Vec::new();
    eg.lock().unwrap().learn(
        "rust",
        astra_runtime::pipeline::routing::DomainHint::Code,
        &[],
        None,
    );
    let _result = try_cloud_push_delta("default", &eg, &pl, &cal, &[], &mut synced, None).await;
}

#[tokio::test]
async fn try_cloud_pull_preferences_is_noop_without_matrixone() {
    unsafe {
        std::env::remove_var("MATRIXONE_HOST");
    }
    let mut state = ReplState::default();
    // Should not panic (was the original bug)
    let keys = try_cloud_pull_preferences(&mut state).await;
    assert!(keys.is_empty());
}

#[tokio::test]
async fn try_cloud_push_preferences_is_noop_without_matrixone() {
    unsafe {
        std::env::remove_var("MATRIXONE_HOST");
    }
    let state = ReplState::default();
    // Should not panic (was the original bug)
    try_cloud_push_preferences(&state).await;
}

#[test]
fn format_duration_short_zero() {
    assert_eq!(
        format_duration_short(std::time::Duration::from_secs(0)),
        "0s"
    );
}

#[test]
fn format_duration_short_seconds() {
    assert_eq!(
        format_duration_short(std::time::Duration::from_secs(45)),
        "45s"
    );
}

#[test]
fn format_duration_short_minutes() {
    assert_eq!(
        format_duration_short(std::time::Duration::from_secs(92)),
        "1m32s"
    );
}

#[test]
fn format_duration_short_hours() {
    assert_eq!(
        format_duration_short(std::time::Duration::from_secs(7500)),
        "2h5m"
    );
}

#[test]
fn format_plan_progress_empty() {
    let s = format_plan_progress(0, 0, None, std::time::Duration::from_secs(0));
    assert!(s.contains("0/0 (0%)"));
    assert!(s.contains("0s elapsed"));
}

#[test]
fn format_plan_progress_first_subtask() {
    let s = format_plan_progress(0, 5, None, std::time::Duration::from_secs(10));
    assert!(s.contains("0/5 (0%)"));
    assert!(s.contains("10s elapsed"));
    // No ETA when done==0
    assert!(!s.contains("remaining"));
}

#[test]
fn format_plan_progress_midway_with_eta() {
    let avg = Some(std::time::Duration::from_secs(60));
    let s = format_plan_progress(3, 7, avg, std::time::Duration::from_secs(180));
    assert!(s.contains("3/7 (42%)"));
    assert!(s.contains("3m0s elapsed"));
    assert!(s.contains("~4m0s remaining")); // 4 remaining × 60s avg
}

#[test]
fn format_plan_progress_complete() {
    let avg = Some(std::time::Duration::from_secs(30));
    let s = format_plan_progress(5, 5, avg, std::time::Duration::from_secs(150));
    assert!(s.contains("5/5 (100%)"));
    // 0 remaining → "~0s remaining"
    assert!(s.contains("remaining"));
}

#[test]
fn format_plan_progress_bar_fills() {
    // At 50% with 16-width bar, should have 8 filled + 8 empty
    let s = format_plan_progress(3, 6, None, std::time::Duration::from_secs(0));
    assert!(s.contains("████████░░░░░░░░"));
}
