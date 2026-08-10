use crate::cli::cli_config::cli_utils::{
    CredentialsFile, Profile, TestCliProfileIdentityGuard, install_cli_profile_identity_for_test,
    save_credentials,
};
use crate::cli::cloud_sync::{
    self, CloudPullResult, append_cloud_pull_sync_journal,
    append_cloud_pull_sync_journal_for_immediate_drain, cloud_pull_warrants_sync_marker,
    should_append_cloud_pull_journal, try_cloud_pull, try_cloud_pull_preferences,
    try_cloud_push_preferences,
};
use crate::cli::plan::plan_monitor::{format_duration_short, format_plan_progress};
use crate::cli::session::session_side_effects::enqueue_ingestion_pub;
use crate::cli::session::session_state::SessionState;
use crate::cli::slash::{slash_health, slash_router::handle_slash_command};
use crate::tests::isolate_credentials;
use astra_services::SyncOutboxStore;
use astra_services::session_journal::{self, JournalDirGuard, ProcessJournalDirGuard};
use astra_sync_protocol::{SYNC_OUTBOX_SIGNATURE_HEADER, sync_outbox_request_signature};
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

struct EnvVarGuard {
    key: &'static str,
    previous: Option<String>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let previous = std::env::var(key).ok();
        unsafe {
            std::env::set_var(key, value);
        }
        Self { key, previous }
    }

    fn remove(key: &'static str) -> Self {
        let previous = std::env::var(key).ok();
        unsafe {
            std::env::remove_var(key);
        }
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        unsafe {
            match &self.previous {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

fn session_state_with_journal(session_id: String) -> SessionState {
    let journal =
        session_journal::JournalWriter::new(&session_id).expect("create owner-scoped journal");
    SessionState {
        session_id: Some(session_id),
        journal: Some(journal),
        ..Default::default()
    }
}

struct TestCloudAuth {
    _identity: TestCliProfileIdentityGuard,
    _credentials: crate::test_utils::CredentialsGuard,
}

fn install_test_cloud_auth(access_token: Option<&str>) -> TestCloudAuth {
    const PROFILE: &str = "cloud-sync-test";
    const ACCOUNT: &str = "cloud-sync-test-account";

    let credentials = isolate_credentials();
    let mut data = CredentialsFile {
        current_profile: Some(PROFILE.to_string()),
        ..Default::default()
    };
    data.profiles.insert(
        PROFILE.to_string(),
        Profile {
            account_id: Some(ACCOUNT.to_string()),
            access_token: access_token.map(str::to_string),
            ..Default::default()
        },
    );
    save_credentials(&data).expect("persist test profile credentials");
    let identity = install_cli_profile_identity_for_test(PROFILE, Some(ACCOUNT))
        .expect("install matching test profile identity");
    TestCloudAuth {
        _identity: identity,
        _credentials: credentials,
    }
}

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
        preferences_last_sync: Some(chrono::Utc::now().to_rfc3339()),
        pending_pushes: 2,
        last_error: Some("connection reset by peer".into()),
        ..Default::default()
    };
    slash_health::display_sync_status(&status);
}

#[serial_test::serial]
#[tokio::test]
async fn slash_health_offline_shows_cloud_section() {
    let api = astra_thin_client::ThinClient::new("http://unused", None).unwrap();
    let mut state = SessionState::default();
    let exit = handle_slash_command("/health", &api, None, &mut state, None)
        .await
        .unwrap();
    assert!(!exit);
}

// ── Cloud sync regression tests ─────────────────────────────────────
// These tests verify the async cloud sync functions don't panic when
// called from within a tokio runtime. We unset ASTRA_API_URL so they
// take the graceful-fallback path (CLI is now HTTP-only).

#[serial_test::serial]
#[tokio::test]
async fn try_cloud_pull_returns_unreachable_without_api_url() {
    let _api = EnvVarGuard::remove("ASTRA_API_URL");
    let result = cloud_sync::try_cloud_pull("default").await;
    assert!(
        !result.cloud_reachable,
        "Without ASTRA_API_URL, cloud should be unreachable"
    );
}

#[test]
fn cloud_pull_warrants_sync_marker_only_when_reachable_and_nonempty() {
    let dead = CloudPullResult {
        cloud_reachable: false,
    };
    assert!(!cloud_pull_warrants_sync_marker(&dead, &[]));
    let online_empty = CloudPullResult {
        cloud_reachable: true,
    };
    assert!(!cloud_pull_warrants_sync_marker(&online_empty, &[]));
    assert!(cloud_pull_warrants_sync_marker(
        &online_empty,
        &["explain_mode".into()]
    ));
}

#[test]
fn should_append_cloud_pull_journal_post_login_reachable_empty() {
    let pull = CloudPullResult {
        cloud_reachable: true,
    };
    assert!(should_append_cloud_pull_journal(&pull, &[], "post_login"));
}

#[serial_test::serial]
#[test]
fn should_append_cloud_pull_journal_session_startup_empty_without_env() {
    unsafe {
        std::env::remove_var(cloud_sync::ASTRA_JOURNAL_CLOUD_EMPTY_ACK);
    }
    let pull = CloudPullResult {
        cloud_reachable: true,
    };
    assert!(!should_append_cloud_pull_journal(
        &pull,
        &[],
        "session_startup"
    ));
}

#[serial_test::serial]
#[test]
fn should_append_session_startup_when_empty_ack_env_set() {
    let pull = CloudPullResult {
        cloud_reachable: true,
    };
    unsafe {
        std::env::remove_var(cloud_sync::ASTRA_JOURNAL_CLOUD_EMPTY_ACK);
    }
    assert!(!should_append_cloud_pull_journal(
        &pull,
        &[],
        "session_startup"
    ));
    unsafe {
        std::env::set_var(cloud_sync::ASTRA_JOURNAL_CLOUD_EMPTY_ACK, "1");
    }
    assert!(should_append_cloud_pull_journal(
        &pull,
        &[],
        "session_startup"
    ));
    unsafe {
        std::env::remove_var(cloud_sync::ASTRA_JOURNAL_CLOUD_EMPTY_ACK);
    }
}

#[test]
fn append_cloud_pull_sync_journal_skips_without_session_id() {
    let pull = CloudPullResult {
        cloud_reachable: true,
    };
    let state = SessionState::default();
    append_cloud_pull_sync_journal(&state, "default", "session_startup", &pull, &[]);
}

#[test]
fn append_cloud_pull_sync_journal_writes_sync_marker_jsonl() {
    let temp = tempfile::tempdir().expect("tempdir");
    let _guard = JournalDirGuard::new(temp.path());
    let sid = format!("test-cloud-pull-journal-{}", uuid::Uuid::new_v4());
    let state = session_state_with_journal(sid.clone());
    let owner_scope = state
        .journal
        .as_ref()
        .expect("live session journal")
        .owner_scope()
        .clone();
    let pull = CloudPullResult {
        cloud_reachable: true,
    };
    let prefs = vec!["explain_mode".to_string()];
    append_cloud_pull_sync_journal(&state, "work", "session_startup", &pull, &prefs);
    let events = session_journal::read_journal_for_owner(&owner_scope, &sid)
        .expect("read owner-scoped journal");
    assert_eq!(events.len(), 2);
    assert_eq!(
        events[0].event_type,
        session_journal::JournalEventType::SessionStart
    );
    let marker = events
        .iter()
        .find(|event| event.event_type == session_journal::JournalEventType::SyncMarker)
        .expect("sync marker");
    assert_eq!(
        marker.event_type,
        session_journal::JournalEventType::SyncMarker
    );
    let cp = marker
        .metadata
        .as_ref()
        .and_then(|m| m.get("cloud_pull"))
        .expect("cloud_pull");
    assert_eq!(cp.get("profile").and_then(|v| v.as_str()), Some("work"));
    assert_eq!(
        cp.get("reachable_empty_ack").and_then(|v| v.as_bool()),
        Some(false)
    );
    let outbox = SyncOutboxStore::local().status().expect("outbox status");
    assert_eq!(outbox.pending, 1);
    assert_eq!(outbox.ready, 1);
    assert_eq!(outbox.poisoned, 0);
}

#[test]
fn append_cloud_pull_post_login_reachable_empty_writes_marker() {
    let temp = tempfile::tempdir().expect("tempdir");
    let _guard = JournalDirGuard::new(temp.path());
    let sid = format!("test-cloud-pull-empty-{}", uuid::Uuid::new_v4());
    let state = session_state_with_journal(sid.clone());
    let owner_scope = state
        .journal
        .as_ref()
        .expect("live session journal")
        .owner_scope()
        .clone();
    let pull = CloudPullResult {
        cloud_reachable: true,
    };
    append_cloud_pull_sync_journal(&state, "default", "post_login", &pull, &[]);
    let events = session_journal::read_journal_for_owner(&owner_scope, &sid)
        .expect("read owner-scoped journal");
    assert_eq!(events.len(), 2);
    assert_eq!(
        events[0].event_type,
        session_journal::JournalEventType::SessionStart
    );
    let marker = events
        .iter()
        .find(|event| event.event_type == session_journal::JournalEventType::SyncMarker)
        .expect("sync marker");
    let cp = marker
        .metadata
        .as_ref()
        .and_then(|m| m.get("cloud_pull"))
        .expect("cloud_pull");
    assert_eq!(
        cp.get("reachable_empty_ack").and_then(|v| v.as_bool()),
        Some(true)
    );
    let outbox = SyncOutboxStore::local().status().expect("outbox status");
    assert_eq!(outbox.pending, 1);
    assert_eq!(outbox.ready, 1);
    assert_eq!(outbox.poisoned, 0);
}

#[test]
fn enqueue_ingestion_does_not_patch_missing_session_id_from_state() {
    let temp = tempfile::tempdir().expect("tempdir");
    let _guard = JournalDirGuard::new(temp.path());
    let state = SessionState {
        session_id: Some("state-session".to_string()),
        ..Default::default()
    };
    let mut event = session_journal::JournalEvent::config_change(None, "model", "gpt-5");
    event.ts = "2026-07-08T00:00:00Z".to_string();

    enqueue_ingestion_pub(&state, &event);

    let outbox = SyncOutboxStore::local().status().expect("outbox status");
    assert_eq!(outbox.total, 0);
    assert_eq!(outbox.pending, 0);
    assert_eq!(outbox.skipped, 1);
    assert_eq!(
        outbox.last_skipped_reason.as_deref(),
        Some("journal event has no session_id and cannot be delivered to /events")
    );
    assert!(outbox.degraded);
}

#[serial_test::serial]
#[tokio::test]
async fn drain_sync_outbox_acks_server_confirmed_payload_hash() {
    let temp = tempfile::tempdir().expect("tempdir");
    let _guard = ProcessJournalDirGuard::new(temp.path());
    let server = MockServer::start().await;
    let _api = EnvVarGuard::set("ASTRA_API_URL", &server.uri());
    let _auth = install_test_cloud_auth(Some("token"));

    let sid = format!("test-cloud-drain-ok-{}", uuid::Uuid::new_v4());
    let state = session_state_with_journal(sid.clone());
    let pull = CloudPullResult {
        cloud_reachable: true,
    };
    append_cloud_pull_sync_journal_for_immediate_drain(&state, "default", "post_login", &pull, &[]);

    let ready = SyncOutboxStore::local()
        .ready_records(10)
        .expect("ready records");
    assert_eq!(ready.len(), 1);
    let record = ready[0].clone();
    let mut metadata = record
        .payload
        .get("metadata")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    metadata.insert(
        "sync_outbox".to_string(),
        json!({
            "schema_version": record.schema_version,
            "record_id": record.record_id,
            "sequence": record.sequence,
            "payload_hash": record.payload_hash,
            "event_ts": record.event_ts,
        }),
    );
    let expected_body = json!({
        "event_id": record.record_id,
        "session_id": record.session_id,
        "event_type": record.event_type,
        "content": record.canonical_payload_json(),
        "agent_id": "edge_sync",
        "agent_version": env!("CARGO_PKG_VERSION"),
        "metadata": Value::Object(metadata),
    });
    let expected_signature = sync_outbox_request_signature("token", &expected_body);
    Mock::given(method("POST"))
        .and(path("/sync/outbox/events"))
        .and(header("authorization", "Bearer token"))
        .and(header(SYNC_OUTBOX_SIGNATURE_HEADER, expected_signature))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "schema_version": 1,
            "record_id": record.record_id,
            "payload_hash": record.payload_hash,
            "ingestion_status": "created"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let report = cloud_sync::try_drain_sync_outbox(10).await;
    assert_eq!(report.attempted, 1);
    assert_eq!(report.acked, 1);
    assert_eq!(report.failed, 0);
    let status = SyncOutboxStore::local().status().expect("status");
    assert_eq!(status.acked, 1);
    assert_eq!(status.pending, 0);
    assert_eq!(status.ack_watermark, 1);
}

#[serial_test::serial]
#[tokio::test]
async fn drain_sync_outbox_reconciles_delivered_event_after_unparseable_success_ack() {
    let temp = tempfile::tempdir().expect("tempdir");
    let _guard = ProcessJournalDirGuard::new(temp.path());
    let server = MockServer::start().await;
    let _api = EnvVarGuard::set("ASTRA_API_URL", &server.uri());
    let _auth = install_test_cloud_auth(Some("token"));

    let sid = format!("test-cloud-drain-reconcile-{}", uuid::Uuid::new_v4());
    let state = session_state_with_journal(sid.clone());
    append_cloud_pull_sync_journal_for_immediate_drain(
        &state,
        "default",
        "post_login",
        &CloudPullResult {
            cloud_reachable: true,
        },
        &[],
    );

    let record = SyncOutboxStore::local()
        .ready_records(1)
        .expect("ready records")
        .into_iter()
        .next()
        .expect("outbox record");
    let mut metadata = record
        .payload
        .get("metadata")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    metadata.insert(
        "sync_outbox".to_string(),
        json!({
            "schema_version": record.schema_version,
            "record_id": record.record_id,
            "sequence": record.sequence,
            "payload_hash": record.payload_hash,
            "event_ts": record.event_ts,
        }),
    );
    let expected_body = json!({
        "event_id": record.record_id,
        "session_id": record.session_id,
        "event_type": record.event_type,
        "content": record.canonical_payload_json(),
        "agent_id": "edge_sync",
        "agent_version": env!("CARGO_PKG_VERSION"),
        "metadata": Value::Object(metadata),
    });

    Mock::given(method("POST"))
        .and(path("/sync/outbox/events"))
        .and(header(
            SYNC_OUTBOX_SIGNATURE_HEADER,
            sync_outbox_request_signature("token", &expected_body),
        ))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "event_id": record.record_id
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/events/{}", record.record_id)))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "event_id": record.record_id,
            "user_id": "user-a",
            "session_id": sid,
            "event_type": record.event_type,
            "content": "{}",
            "agent_id": "edge_sync",
            "agent_version": "0.1.0",
            "parent_event_id": null,
            "causal_chain_id": null,
            "metadata": {
                "sync_outbox": {
                    "payload_hash": record.payload_hash
                }
            },
            "created_at": "2026-07-08T00:00:00.000000"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let report = cloud_sync::try_drain_sync_outbox(10).await;
    assert_eq!(report.acked, 1);
    assert_eq!(report.failed, 0);
    let status = SyncOutboxStore::local().status().expect("status");
    assert_eq!(status.acked, 1);
    assert_eq!(status.poisoned, 0);
    assert_eq!(status.ack_watermark, 1);
}

#[serial_test::serial]
#[tokio::test]
async fn post_auth_sync_foreground_drain_reports_its_own_marker_delivery() {
    let temp = tempfile::tempdir().expect("tempdir");
    let _guard = ProcessJournalDirGuard::new(temp.path());
    let server = MockServer::start().await;
    let _api = EnvVarGuard::set("ASTRA_API_URL", &server.uri());
    let _auth = install_test_cloud_auth(Some("token"));
    let mut state = session_state_with_journal(format!(
        "test-post-auth-foreground-drain-{}",
        uuid::Uuid::new_v4()
    ));

    Mock::given(method("GET"))
        .and(path("/preferences"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "preferences": [] })))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/sync/outbox/events"))
        .respond_with(|request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).expect("sync request JSON");
            ResponseTemplate::new(201).set_body_json(json!({
                "schema_version": 1,
                "record_id": body.get("event_id").and_then(Value::as_str),
                "payload_hash": body
                    .get("metadata")
                    .and_then(|metadata| metadata.get("sync_outbox"))
                    .and_then(|sync| sync.get("payload_hash"))
                    .and_then(Value::as_str),
                "ingestion_status": "created"
            }))
        })
        .expect(1)
        .mount(&server)
        .await;

    let report = cloud_sync::post_auth_cloud_resync(None, &mut state).await;
    assert_eq!(report.attempted, 1);
    assert_eq!(report.acked, 1);
    assert_eq!(report.failed, 0);
    assert_eq!(report.terminal, 0);
    assert_eq!(report.blocker, None);
    assert!(!report.is_incomplete());

    let status = SyncOutboxStore::local().status().expect("status");
    assert_eq!(status.acked, 1);
    assert_eq!(status.pending, 0);
    assert_eq!(status.in_flight, 0);
}

#[serial_test::serial]
#[tokio::test]
async fn drain_sync_outbox_http_failure_keeps_record_for_retry() {
    let temp = tempfile::tempdir().expect("tempdir");
    let _guard = ProcessJournalDirGuard::new(temp.path());
    let server = MockServer::start().await;
    let _api = EnvVarGuard::set("ASTRA_API_URL", &server.uri());
    let _auth = install_test_cloud_auth(Some("token"));

    let sid = format!("test-cloud-drain-fail-{}", uuid::Uuid::new_v4());
    let state = session_state_with_journal(sid);
    let pull = CloudPullResult {
        cloud_reachable: true,
    };
    append_cloud_pull_sync_journal_for_immediate_drain(&state, "default", "post_login", &pull, &[]);

    Mock::given(method("POST"))
        .and(path("/sync/outbox/events"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .expect(1)
        .mount(&server)
        .await;

    let report = cloud_sync::try_drain_sync_outbox(10).await;
    assert_eq!(report.attempted, 1);
    assert_eq!(report.acked, 0);
    assert_eq!(report.failed, 1);
    assert_eq!(report.blocker, None);
    assert!(report.is_incomplete());
    assert!(report.user_notice().is_some());
    let status = SyncOutboxStore::local().status().expect("status");
    assert_eq!(status.acked, 0);
    assert_eq!(status.pending, 1);
    assert_eq!(status.retry_deferred, 1);
    assert_eq!(status.ack_watermark, 0);
}

#[serial_test::serial]
#[tokio::test]
async fn post_auth_sync_reports_missing_token_and_preserves_ready_records() {
    let temp = tempfile::tempdir().expect("tempdir");
    let _guard = ProcessJournalDirGuard::new(temp.path());
    let server = MockServer::start().await;
    let _api = EnvVarGuard::set("ASTRA_API_URL", &server.uri());
    let _auth = install_test_cloud_auth(None);

    let mut state = session_state_with_journal(format!(
        "test-cloud-drain-no-token-{}",
        uuid::Uuid::new_v4()
    ));
    let pull = CloudPullResult {
        cloud_reachable: true,
    };
    append_cloud_pull_sync_journal_for_immediate_drain(&state, "default", "post_login", &pull, &[]);

    let report = cloud_sync::post_auth_cloud_resync(None, &mut state).await;
    assert!(report.cloud_configured);
    assert_eq!(report.attempted, 0);
    assert_eq!(report.acked, 0);
    assert_eq!(report.failed, 0);
    assert_eq!(
        report.blocker,
        Some(cloud_sync::SyncOutboxDrainBlocker::MissingAccessToken)
    );
    assert!(report.user_notice().is_some());

    let status = SyncOutboxStore::local().status().expect("status");
    assert_eq!(status.pending, 1);
    assert_eq!(status.ready, 1);
    assert_eq!(status.in_flight, 0);
    assert_eq!(status.retry_deferred, 0);
    assert_eq!(status.poisoned, 0);
    assert_eq!(status.ack_watermark, 0);
}

#[serial_test::serial]
#[tokio::test]
async fn drain_empty_sync_outbox_does_not_require_cloud_credentials() {
    let temp = tempfile::tempdir().expect("tempdir");
    let _guard = ProcessJournalDirGuard::new(temp.path());
    let server = MockServer::start().await;
    let _api = EnvVarGuard::set("ASTRA_API_URL", &server.uri());
    let _auth = install_test_cloud_auth(None);

    let report = cloud_sync::try_drain_sync_outbox(10).await;
    assert!(report.cloud_configured);
    assert_eq!(report.attempted, 0);
    assert_eq!(report.remaining_ready, 0);
    assert_eq!(report.blocker, None);
    assert!(!report.is_incomplete());
    assert_eq!(report.user_notice(), None);
}

#[serial_test::serial]
#[tokio::test]
async fn try_cloud_pull_returns_empty_without_matrixone() {
    unsafe {
        std::env::remove_var("MATRIXONE_HOST");
    }
    let result = try_cloud_pull("default").await;
    assert!(
        !result.cloud_reachable,
        "Without MatrixOne, cloud should be unreachable"
    );
}

#[serial_test::serial]
#[tokio::test]
async fn try_cloud_pull_preferences_is_noop_without_matrixone() {
    unsafe {
        std::env::remove_var("MATRIXONE_HOST");
    }
    let mut state = SessionState::default();
    // Should not panic (was the original bug)
    let keys = try_cloud_pull_preferences(&mut state).await;
    assert!(keys.is_empty());
}

#[serial_test::serial]
#[tokio::test]
async fn try_cloud_push_preferences_is_noop_without_matrixone() {
    unsafe {
        std::env::remove_var("MATRIXONE_HOST");
    }
    let state = SessionState::default();
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
