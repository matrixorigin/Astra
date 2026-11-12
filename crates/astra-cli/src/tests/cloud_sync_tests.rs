use crate::cli::cli_config::cli_utils::{
    CredentialsFile, Profile, TestCliProfileIdentityGuard, install_cli_profile_identity_for_test,
    save_credentials,
};
use crate::cli::cloud_sync::{
    self, CloudPullResult, append_cloud_pull_sync_journal,
    append_cloud_pull_sync_journal_for_immediate_drain, cloud_pull_warrants_sync_marker,
    should_append_cloud_pull_journal, try_cloud_pull, try_cloud_pull_preferences,
};
use crate::cli::session::session_side_effects::schedule_journal_ingestion;
use crate::cli::session::session_state::SessionState;
use crate::tests::isolate_credentials;
use astra_services::SyncOutboxStore;
use astra_services::session_journal::{self, JournalDirGuard, ProcessJournalDirGuard};
use astra_sync_protocol::{SYNC_OUTBOX_SIGNATURE_HEADER, sync_outbox_request_signature};
use serde_json::{Value, json};
use wiremock::matchers::{body_json, header, method, path};
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

fn expected_event_batch(records: &[astra_services::SyncOutboxRecord]) -> Value {
    json!({
        "schema_version": 1,
        "events": records.iter().map(|record| json!({
            "event_id": record.record_id,
            "session_id": record.session_id,
            "event_type": record.event_type,
            "content": record.canonical_payload_json(),
            "agent_id": "edge_sync",
            "agent_version": env!("CARGO_PKG_VERSION"),
            "metadata": {"sync_outbox": {
                "schema_version": record.schema_version,
                "record_id": record.record_id,
                "sequence": record.sequence,
                "payload_hash": record.payload_hash,
                "event_ts": record.event_ts,
            }},
        })).collect::<Vec<_>>()
    })
}

fn event_acks(records: &[astra_services::SyncOutboxRecord], status: &str) -> Vec<Value> {
    records
        .iter()
        .map(|record| {
            json!({
                "record_id": record.record_id,
                "payload_hash": record.payload_hash,
                "ingestion_status": status,
            })
        })
        .collect()
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
    let outbox = SyncOutboxStore::for_owner(&owner_scope)
        .expect("owner-scoped outbox")
        .status()
        .expect("outbox status");
    assert_eq!(outbox.pending, 2);
    assert_eq!(outbox.ready, 2);
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
    let outbox = SyncOutboxStore::for_owner(&owner_scope)
        .expect("owner-scoped outbox")
        .status()
        .expect("outbox status");
    assert_eq!(outbox.pending, 2);
    assert_eq!(outbox.ready, 2);
    assert_eq!(outbox.poisoned, 0);
}

#[test]
fn enqueue_ingestion_does_not_patch_missing_session_id_from_state() {
    let temp = tempfile::tempdir().expect("tempdir");
    let _guard = JournalDirGuard::new(temp.path());
    let state = session_state_with_journal("state-session".to_string());
    let owner_scope = state
        .journal
        .as_ref()
        .expect("live session journal")
        .owner_scope()
        .clone();
    let mut event = session_journal::JournalEvent::config_change(None, "model", "gpt-5");
    event.ts = "2026-07-08T00:00:00Z".to_string();

    let journal = state.journal.as_ref().unwrap();
    journal
        .append(&session_journal::JournalEvent::session_start(
            Some("state-session"),
            None,
        ))
        .unwrap();
    journal.append(&event).unwrap();
    schedule_journal_ingestion(journal);

    let outbox = SyncOutboxStore::for_owner(&owner_scope)
        .expect("owner-scoped outbox")
        .status()
        .expect("outbox status");
    assert_eq!(outbox.skipped, 1);
    assert_eq!(
        outbox.last_skipped_reason.as_deref(),
        Some("journal event has no session_id and cannot be delivered to /events")
    );
    assert!(outbox.degraded);
}

#[serial_test::serial]
#[tokio::test]
async fn drain_reports_existing_rejections_and_skipped_events_without_ready_work() {
    let temp = tempfile::tempdir().expect("tempdir");
    let _guard = ProcessJournalDirGuard::new(temp.path());
    let _api = EnvVarGuard::set("ASTRA_API_URL", "http://127.0.0.1:9");
    let _auth = install_test_cloud_auth(Some("token"));
    let state = session_state_with_journal("rejected-session".to_owned());
    append_cloud_pull_sync_journal_for_immediate_drain(
        &state,
        "default",
        "post_login",
        &CloudPullResult {
            cloud_reachable: true,
        },
        &[],
    )
    .await
    .unwrap();
    let store = SyncOutboxStore::local();
    let records = store.ready_records(10).unwrap();
    assert_eq!(records.len(), 2);
    store
        .settle_delivery_batch(
            &[
                astra_services::SyncOutboxDeliverySettlement::Ack {
                    record_id: records[0].record_id.clone(),
                    payload_hash: records[0].payload_hash.clone(),
                },
                astra_services::SyncOutboxDeliverySettlement::Rejected {
                    record_id: records[1].record_id.clone(),
                    error: "invalid event".into(),
                },
            ],
            None,
        )
        .unwrap();
    store
        .record_skipped_journal_event(
            &session_journal::JournalEvent::config_change(None, "model", "gpt-5"),
            astra_services::SyncOutboxSkipKind::MissingSessionId,
            "missing session",
        )
        .unwrap();

    let report = cloud_sync::try_drain_sync_outbox(10).await;
    assert_eq!(report.attempted, 0);
    assert_eq!(report.terminal, 2);
    assert!(report.user_notice().unwrap().contains("astra doctor"));
    let status = cloud_sync::read_sync_outbox_status().await.unwrap();
    assert_eq!(status.poisoned, 1);
    assert_eq!(status.skipped, 1);
}

#[serial_test::serial]
#[tokio::test]
async fn newly_rejected_sync_notifies_the_running_session_after_persistence() {
    let temp = tempfile::tempdir().unwrap();
    let _guard = ProcessJournalDirGuard::new(temp.path());
    let server = MockServer::start().await;
    let _api = EnvVarGuard::set("ASTRA_API_URL", &server.uri());
    let _auth = install_test_cloud_auth(Some("token"));
    let mut notices = cloud_sync::subscribe_sync_issues();
    let store = SyncOutboxStore::local();
    store
        .enqueue_journal_event(&session_journal::JournalEvent::config_change(
            Some("sync-warning-session"),
            "model",
            "test",
        ))
        .unwrap();
    let record = store.ready_records(1).unwrap().pop().unwrap();
    Mock::given(method("POST"))
        .and(path("/sync/outbox/events"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "schema_version": 1,
            "acks": [],
            "rejections": [{
                "record_id": record.record_id,
                "error_code": "invalid_record",
                "detail": "invalid event"
            }]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let report = cloud_sync::try_drain_sync_outbox(10).await;
    assert_eq!(report.terminal, 1);
    assert_eq!(store.status().unwrap().poisoned, 1);
    assert_eq!(
        notices.try_recv().unwrap(),
        astra_services::local_owner_scope()
    );
}

#[serial_test::serial]
#[tokio::test]
async fn rate_limit_pauses_newly_queued_sessions_until_the_server_deadline() {
    let temp = tempfile::tempdir().unwrap();
    let _guard = ProcessJournalDirGuard::new(temp.path());
    let server = MockServer::start().await;
    let _api = EnvVarGuard::set("ASTRA_API_URL", &server.uri());
    let _auth = install_test_cloud_auth(Some("token"));
    Mock::given(method("POST"))
        .and(path("/sync/outbox/events"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "60"))
        .mount(&server)
        .await;
    let store = SyncOutboxStore::local();
    for index in 0..20 {
        store
            .enqueue_journal_event(&session_journal::JournalEvent::config_change(
                Some(&format!("rate-limit-{index}")),
                "model",
                "test",
            ))
            .unwrap();
    }

    let report = cloud_sync::try_drain_sync_outbox(64).await;
    assert_eq!(report.failed, 20);
    let first_requests = server.received_requests().await.unwrap().len();
    assert!((1..=8).contains(&first_requests));
    store
        .enqueue_journal_event(&session_journal::JournalEvent::config_change(
            Some("rate-limit-new"),
            "model",
            "test",
        ))
        .unwrap();
    assert_eq!(SyncOutboxStore::local().status().unwrap().claimable, 0);
    assert_eq!(cloud_sync::try_drain_sync_outbox(64).await.attempted, 0);
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        first_requests
    );
}

#[serial_test::serial]
#[tokio::test]
async fn background_sync_uses_a_full_protocol_batch() {
    let temp = tempfile::tempdir().unwrap();
    let _guard = ProcessJournalDirGuard::new(temp.path());
    let server = MockServer::start().await;
    let _api = EnvVarGuard::set("ASTRA_API_URL", &server.uri());
    let _auth = install_test_cloud_auth(Some("token"));
    let store = SyncOutboxStore::local();
    let events = (0..astra_sync_protocol::MAX_SYNC_OUTBOX_BATCH_EVENTS)
        .map(|index| {
            session_journal::JournalEvent::config_change(
                Some("full-sync-batch"),
                "model",
                &format!("model-{index}"),
            )
        })
        .collect::<Vec<_>>();
    store.enqueue_journal_events(&events).unwrap();
    Mock::given(method("POST"))
        .and(path("/sync/outbox/events"))
        .respond_with(|request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            let acks = body["events"]
                .as_array()
                .unwrap()
                .iter()
                .map(|event| {
                    json!({
                        "record_id": event["event_id"],
                        "payload_hash": event["metadata"]["sync_outbox"]["payload_hash"],
                        "ingestion_status": "created"
                    })
                })
                .collect::<Vec<_>>();
            ResponseTemplate::new(200)
                .set_body_json(json!({"schema_version":1,"acks":acks,"rejections":[]}))
        })
        .expect(1)
        .mount(&server)
        .await;

    cloud_sync::schedule_sync_outbox_drain();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if store.status().unwrap().acked == events.len() as u64 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("background drain should settle its full batch");
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["events"].as_array().unwrap().len(), events.len());
    server.verify().await;
}

#[serial_test::serial]
#[tokio::test]
async fn temporary_local_outbox_failure_warns_and_retries_automatically() {
    use fs2::FileExt;

    let temp = tempfile::tempdir().unwrap();
    let _guard = ProcessJournalDirGuard::new(temp.path());
    let server = MockServer::start().await;
    let _api = EnvVarGuard::set("ASTRA_API_URL", &server.uri());
    let _auth = install_test_cloud_auth(Some("token"));
    let store = SyncOutboxStore::local();
    store
        .enqueue_journal_event(&session_journal::JournalEvent::config_change(
            Some("lock-recovery"),
            "model",
            "test",
        ))
        .unwrap();
    let record = store.ready_records(1).unwrap().remove(0);
    Mock::given(method("POST"))
        .and(path("/sync/outbox/events"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "schema_version": 1,
            "acks": [{"record_id": record.record_id, "payload_hash": record.payload_hash, "ingestion_status": "created"}],
            "rejections": []
        })))
        .expect(1).mount(&server).await;
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(store.path().parent().unwrap().join("outbox.lock"))
        .unwrap();
    lock.lock_exclusive().unwrap();
    let mut notices = cloud_sync::subscribe_sync_issues();
    cloud_sync::schedule_sync_outbox_drain();
    let owner = tokio::time::timeout(std::time::Duration::from_secs(5), notices.recv())
        .await
        .expect("lock failure should be reported")
        .unwrap();
    assert_eq!(owner, astra_services::local_owner_scope());
    lock.unlock().unwrap();

    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(17)).await;
    tokio::time::resume();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if store.status().unwrap().acked == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("pending record should sync after the local lock is released");
    server.verify().await;
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
    append_cloud_pull_sync_journal_for_immediate_drain(&state, "default", "post_login", &pull, &[])
        .await
        .unwrap();

    let ready = SyncOutboxStore::local()
        .ready_records(10)
        .expect("ready records");
    assert_eq!(ready.len(), 2);
    let expected_body = expected_event_batch(&ready);
    let expected_signature = sync_outbox_request_signature("token", &expected_body);
    Mock::given(method("POST"))
        .and(path("/sync/outbox/events"))
        .and(header("authorization", "Bearer token"))
        .and(header(SYNC_OUTBOX_SIGNATURE_HEADER, expected_signature))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "schema_version": 1,
            "acks": event_acks(&ready, "created"),
            "rejections": []
        })))
        .expect(1)
        .mount(&server)
        .await;

    let report = cloud_sync::try_drain_sync_outbox(10).await;
    assert_eq!(report.attempted, 2);
    assert_eq!(report.acked, 2);
    assert_eq!(report.failed, 0);
    let status = SyncOutboxStore::local().status().expect("status");
    assert_eq!(status.acked, 2);
    assert_eq!(status.pending, 0);
    assert_eq!(status.ack_watermark, 2);
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
    )
    .await
    .unwrap();

    let records = SyncOutboxStore::local().ready_records(10).unwrap();
    assert_eq!(records.len(), 2);
    let expected_body = expected_event_batch(&records);

    Mock::given(method("POST"))
        .and(path("/sync/outbox/events"))
        .and(header(
            SYNC_OUTBOX_SIGNATURE_HEADER,
            sync_outbox_request_signature("token", &expected_body),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "schema_version": 1,
            "acks": []
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/sync/outbox/events/reconcile"))
        .and(body_json(json!({
            "schema_version": 1,
            "record_ids": records.iter().map(|record| &record.record_id).collect::<Vec<_>>()
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "schema_version": 1,
            "acks": event_acks(&records, "idempotent_replay"),
            "rejections": []
        })))
        .expect(1)
        .mount(&server)
        .await;

    let report = cloud_sync::try_drain_sync_outbox(10).await;
    assert_eq!(report.acked, 2);
    assert_eq!(report.failed, 0);
    let status = SyncOutboxStore::local().status().expect("status");
    assert_eq!(status.acked, 2);
    assert_eq!(status.poisoned, 0);
    assert_eq!(status.ack_watermark, 2);
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
            let acks = body["events"]
                .as_array()
                .unwrap()
                .iter()
                .map(|event| {
                    json!({
                        "record_id": event["event_id"],
                        "payload_hash": event["metadata"]["sync_outbox"]["payload_hash"],
                        "ingestion_status": "created"
                    })
                })
                .collect::<Vec<_>>();
            ResponseTemplate::new(200).set_body_json(json!({
                "schema_version": 1,
                "acks": acks,
                "rejections": []
            }))
        })
        .expect(1)
        .mount(&server)
        .await;

    let report = cloud_sync::post_auth_cloud_resync(None, &mut state).await;
    assert_eq!(report.attempted, 2);
    assert_eq!(report.acked, 2);
    assert_eq!(report.failed, 0);
    assert_eq!(report.terminal, 0);
    assert_eq!(report.blocker, None);
    assert!(!report.is_incomplete());

    let status = SyncOutboxStore::local().status().expect("status");
    assert_eq!(status.acked, 2);
    assert_eq!(status.pending, 0);
    assert_eq!(status.in_flight, 0);
}

#[serial_test::serial]
#[tokio::test]
async fn post_auth_reports_projection_failure_and_drains_other_sessions() {
    let temp = tempfile::tempdir().expect("tempdir");
    let _guard = ProcessJournalDirGuard::new(temp.path());
    let server = MockServer::start().await;
    let _api = EnvVarGuard::set("ASTRA_API_URL", &server.uri());
    let _auth = install_test_cloud_auth(Some("token"));
    let mut state = session_state_with_journal("broken-journal".into());
    std::fs::create_dir_all(state.journal.as_ref().unwrap().path()).unwrap();
    let store = SyncOutboxStore::local();
    store
        .enqueue_journal_event(&session_journal::JournalEvent::config_change(
            Some("other-session"),
            "model",
            "test-model",
        ))
        .unwrap();

    Mock::given(method("GET"))
        .and(path("/preferences"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "preferences": [] })))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/sync/outbox/events"))
        .respond_with(|request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            let event = &body["events"][0];
            ResponseTemplate::new(200).set_body_json(json!({
                "schema_version": 1,
                "acks": [{
                    "record_id": event["event_id"],
                    "payload_hash": event["metadata"]["sync_outbox"]["payload_hash"],
                    "ingestion_status": "created"
                }],
                "rejections": []
            }))
        })
        .expect(1)
        .mount(&server)
        .await;

    let report = cloud_sync::post_auth_cloud_resync(None, &mut state).await;
    assert_eq!(report.acked, 1);
    assert_eq!(
        report.blocker,
        Some(cloud_sync::SyncOutboxDrainBlocker::LocalJournalSync)
    );
    assert!(report.is_incomplete());
    assert!(report.user_notice().unwrap().contains("local journal"));
    assert_eq!(store.status().unwrap().pending, 0);
    server.verify().await;
}

#[serial_test::serial]
#[tokio::test]
async fn timed_out_login_projection_finishes_and_wakes_the_drain() {
    use fs2::FileExt;

    let temp = tempfile::tempdir().unwrap();
    let _guard = ProcessJournalDirGuard::new(temp.path());
    let server = MockServer::start().await;
    let _api = EnvVarGuard::set("ASTRA_API_URL", &server.uri());
    let _auth = install_test_cloud_auth(Some("token"));
    let sid = "slow-login-projection";
    let mut state = session_state_with_journal(sid.into());
    state
        .journal
        .as_ref()
        .unwrap()
        .append(&session_journal::JournalEvent::config_change(
            Some(sid),
            "model",
            "test-model",
        ))
        .unwrap();
    let store = SyncOutboxStore::local();
    std::fs::create_dir_all(store.path().parent().unwrap()).unwrap();
    let outbox_lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(store.path().parent().unwrap().join("outbox.lock"))
        .unwrap();
    outbox_lock.lock_exclusive().unwrap();
    Mock::given(method("GET"))
        .and(path("/preferences"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "preferences": [] })))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/sync/outbox/events"))
        .respond_with(|request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            let acks = body["events"]
                .as_array()
                .unwrap()
                .iter()
                .map(|event| {
                    json!({
                        "record_id": event["event_id"],
                        "payload_hash": event["metadata"]["sync_outbox"]["payload_hash"],
                        "ingestion_status": "created"
                    })
                })
                .collect::<Vec<_>>();
            ResponseTemplate::new(200).set_body_json(json!({
                "schema_version": 1, "acks": acks, "rejections": []
            }))
        })
        .expect(1)
        .mount(&server)
        .await;

    let report = cloud_sync::post_auth_cloud_resync_with_wait(
        None,
        &mut state,
        std::time::Duration::from_millis(50),
    )
    .await;
    assert_eq!(
        report.blocker,
        Some(cloud_sync::SyncOutboxDrainBlocker::LocalSyncPending)
    );
    assert!(report.user_notice().unwrap().contains("unconfirmed"));
    outbox_lock.unlock().unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if SyncOutboxStore::local().status().unwrap().acked == 3 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the completed projection must wake cloud delivery");
    server.verify().await;
}

#[serial_test::serial]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_post_auth_wait_keeps_projection_and_delivery_alive() {
    use fs2::FileExt;

    let temp = tempfile::tempdir().unwrap();
    let _guard = ProcessJournalDirGuard::new(temp.path());
    let server = MockServer::start().await;
    let _api = EnvVarGuard::set("ASTRA_API_URL", &server.uri());
    let _auth = install_test_cloud_auth(Some("token"));
    let sid = "cancelled-login-wait";
    let mut state = session_state_with_journal(sid.into());
    let owner = state.journal.as_ref().unwrap().owner_scope().clone();
    let store = SyncOutboxStore::for_owner(&owner).unwrap();
    std::fs::create_dir_all(store.path().parent().unwrap()).unwrap();
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(store.path().parent().unwrap().join("outbox.lock"))
        .unwrap();
    lock.lock_exclusive().unwrap();
    Mock::given(method("GET"))
        .and(path("/preferences"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"preferences": []})))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/sync/outbox/events"))
        .respond_with(|request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            let acks = body["events"]
                .as_array()
                .unwrap()
                .iter()
                .map(|event| {
                    json!({
                        "record_id": event["event_id"],
                        "payload_hash": event["metadata"]["sync_outbox"]["payload_hash"],
                        "ingestion_status": "created"
                    })
                })
                .collect::<Vec<_>>();
            ResponseTemplate::new(200)
                .set_body_json(json!({"schema_version": 1, "acks": acks, "rejections": []}))
        })
        .expect(1)
        .mount(&server)
        .await;

    let waiter = tokio::spawn(async move {
        cloud_sync::post_auth_cloud_resync_with_wait(
            None,
            &mut state,
            std::time::Duration::from_secs(10),
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if session_journal::read_journal_for_owner(&owner, sid)
                .unwrap()
                .iter()
                .any(|event| event.event_type == session_journal::JournalEventType::SyncMarker)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("post-login marker was appended");
    waiter.abort();
    lock.unlock().unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if store.status().unwrap().acked == 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("cancelling the wait must not cancel the owned sync task");
    server.verify().await;
}

#[serial_test::serial]
#[tokio::test]
async fn post_auth_job_never_sends_old_owner_events_with_new_owner_credentials() {
    use fs2::FileExt;

    let temp = tempfile::tempdir().unwrap();
    let _guard = ProcessJournalDirGuard::new(temp.path());
    let server = MockServer::start().await;
    let _api = EnvVarGuard::set("ASTRA_API_URL", &server.uri());
    let _auth = install_test_cloud_auth(Some("owner-a-token"));
    let state = session_state_with_journal("slow-owner-switch".into());
    let owner_a = state.journal.as_ref().unwrap().owner_scope().clone();
    let store_a = SyncOutboxStore::for_owner(&owner_a).unwrap();
    std::fs::create_dir_all(store_a.path().parent().unwrap()).unwrap();
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(store_a.path().parent().unwrap().join("outbox.lock"))
        .unwrap();
    lock.lock_exclusive().unwrap();
    Mock::given(method("POST"))
        .and(path("/sync/outbox/events"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;

    let projection = append_cloud_pull_sync_journal_for_immediate_drain(
        &state,
        "default",
        "post_login",
        &CloudPullResult {
            cloud_reachable: true,
        },
        &[],
    );
    let snapshot = crate::cli::cli_config::cli_utils::cli_owner_auth_snapshot();
    let binding = Some((owner_a.clone(), "slow-owner-switch".to_string()));
    let job = tokio::spawn(cloud_sync::run_post_auth_sync(
        projection, binding, snapshot,
    ));
    let _owner_b = install_cli_profile_identity_for_test("owner-b", Some("account-b")).unwrap();
    let owner_b = astra_services::local_owner_scope();
    lock.unlock().unwrap();

    let report = tokio::time::timeout(std::time::Duration::from_secs(5), job)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        report.blocker,
        Some(cloud_sync::SyncOutboxDrainBlocker::LocalSyncPending)
    );
    assert_eq!(store_a.status().unwrap().pending, 2);
    assert_eq!(
        SyncOutboxStore::for_owner(&owner_b)
            .unwrap()
            .status()
            .unwrap()
            .pending,
        0
    );
    server.verify().await;
}

#[serial_test::serial]
#[tokio::test]
async fn post_auth_reports_in_flight_records_as_unconfirmed() {
    let temp = tempfile::tempdir().unwrap();
    let _guard = ProcessJournalDirGuard::new(temp.path());
    let server = MockServer::start().await;
    let _api = EnvVarGuard::set("ASTRA_API_URL", &server.uri());
    let _auth = install_test_cloud_auth(Some("token"));
    let store = SyncOutboxStore::local();
    store
        .enqueue_journal_event(&session_journal::JournalEvent::config_change(
            Some("in-flight-session"),
            "model",
            "test-model",
        ))
        .unwrap();
    assert_eq!(store.claim_ready_records(1, 1024 * 1024).unwrap().len(), 1);
    Mock::given(method("POST"))
        .and(path("/sync/outbox/events"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;

    let snapshot = crate::cli::cli_config::cli_utils::cli_owner_auth_snapshot();
    let report = cloud_sync::run_post_auth_sync(async { Ok(()) }, None, snapshot).await;
    assert_eq!(report.attempted, 0);
    assert_eq!(report.remaining_unconfirmed, 1);
    assert_eq!(
        report.blocker,
        Some(cloud_sync::SyncOutboxDrainBlocker::LocalSyncPending)
    );
    assert!(report.user_notice().unwrap().contains("unconfirmed"));
    server.verify().await;
}

#[serial_test::serial]
#[tokio::test]
async fn post_auth_keeps_terminal_errors_visible_with_in_flight_work() {
    let temp = tempfile::tempdir().unwrap();
    let _guard = ProcessJournalDirGuard::new(temp.path());
    let server = MockServer::start().await;
    let _api = EnvVarGuard::set("ASTRA_API_URL", &server.uri());
    let _auth = install_test_cloud_auth(Some("token"));
    let store = SyncOutboxStore::local();
    for sid in ["rejected-session", "in-flight-session"] {
        store
            .enqueue_journal_event(&session_journal::JournalEvent::config_change(
                Some(sid),
                "model",
                "test-model",
            ))
            .unwrap();
    }
    let records = store.ready_records(2).unwrap();
    store
        .settle_delivery_batch(
            &[astra_services::SyncOutboxDeliverySettlement::Rejected {
                record_id: records[0].record_id.clone(),
                error: "invalid event".into(),
            }],
            None,
        )
        .unwrap();
    assert_eq!(store.claim_ready_records(1, 1024 * 1024).unwrap().len(), 1);
    let snapshot = crate::cli::cli_config::cli_utils::cli_owner_auth_snapshot();
    let report = cloud_sync::run_post_auth_sync(async { Ok(()) }, None, snapshot).await;
    assert_eq!(report.terminal, 1);
    assert_eq!(report.remaining_unconfirmed, 1);
    assert!(report.blocker.is_none());
    assert!(report.user_notice().unwrap().contains("astra doctor"));
}

#[serial_test::serial]
#[tokio::test]
async fn authentication_rejection_waits_for_new_login_without_losing_records() {
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
    append_cloud_pull_sync_journal_for_immediate_drain(&state, "default", "post_login", &pull, &[])
        .await
        .unwrap();

    Mock::given(method("POST"))
        .and(path("/sync/outbox/events"))
        .and(header("authorization", "Bearer token"))
        .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
        .expect(1)
        .mount(&server)
        .await;

    let report = cloud_sync::try_drain_sync_outbox(10).await;
    assert_eq!(report.attempted, 2);
    assert_eq!(report.acked, 0);
    assert_eq!(report.failed, 2);
    assert_eq!(
        report.blocker,
        Some(cloud_sync::SyncOutboxDrainBlocker::CloudAccessRequired)
    );
    assert!(report.is_incomplete());
    assert!(report.user_notice().is_some());
    let status = SyncOutboxStore::local().status().expect("status");
    assert_eq!(status.acked, 0);
    assert_eq!(status.pending, 2);
    assert_eq!(status.retry_deferred, 0);
    assert_eq!(status.claimable, 2);
    assert_eq!(status.ack_watermark, 0);
    assert!(
        status
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("HTTP 401"))
    );
    let blocked = cloud_sync::try_drain_sync_outbox(10).await;
    assert_eq!(blocked.blocker, report.blocker);
    assert_eq!(blocked.attempted, 0);
    cloud_sync::schedule_sync_outbox_drain();
    assert_eq!(server.received_requests().await.unwrap().len(), 1);

    let records = SyncOutboxStore::local().ready_records(10).unwrap();
    assert_eq!(records.len(), 2);
    Mock::given(method("POST"))
        .and(path("/sync/outbox/events"))
        .and(header("authorization", "Bearer new-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "schema_version": 1,
            "acks": event_acks(&records, "created"),
            "rejections": []
        })))
        .expect(1)
        .mount(&server)
        .await;
    crate::cli::auth_flow::save_profile_auth_tokens(
        Some("cloud-sync-test"),
        "user",
        &crate::cli::auth_flow::AuthTokenPayload {
            user_id: "cloud-sync-test-account".into(),
            access_token: "new-token".into(),
            refresh_token: "new-refresh".into(),
        },
    )
    .unwrap();
    let recovered = cloud_sync::try_drain_sync_outbox(10).await;
    assert_eq!(recovered.acked, 2);
    assert_eq!(recovered.blocker, None);
    assert_eq!(SyncOutboxStore::local().status().unwrap().pending, 0);
}

async fn exercise_relogin_handoff(direct_post_auth: bool) {
    use axum::{http::StatusCode, response::IntoResponse};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    let temp = tempfile::tempdir().unwrap();
    let _guard = ProcessJournalDirGuard::new(temp.path());
    let _auth = install_test_cloud_auth(Some("token"));
    let state = session_state_with_journal("login-race".into());
    append_cloud_pull_sync_journal_for_immediate_drain(
        &state,
        "default",
        "post_login",
        &CloudPullResult {
            cloud_reachable: true,
        },
        &[],
    )
    .await
    .unwrap();
    let store = SyncOutboxStore::local();
    assert_eq!(store.ready_records(10).unwrap().len(), 2);
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let started_tx = Arc::new(std::sync::Mutex::new(Some(started_tx)));
    let release_rx = Arc::new(tokio::sync::Mutex::new(Some(release_rx)));
    let old_calls = Arc::new(AtomicUsize::new(0));
    let new_calls = Arc::new(AtomicUsize::new(0));
    let app = axum::Router::new().route(
        "/sync/outbox/events",
        axum::routing::post({
            let old_calls = old_calls.clone();
            let new_calls = new_calls.clone();
            move |headers: axum::http::HeaderMap, axum::Json(body): axum::Json<Value>| {
                let (started_tx, release_rx, old_calls, new_calls) = (
                    started_tx.clone(),
                    release_rx.clone(),
                    old_calls.clone(),
                    new_calls.clone(),
                );
                async move {
                    if headers.get("authorization").and_then(|v| v.to_str().ok())
                        == Some("Bearer token")
                    {
                        old_calls.fetch_add(1, Ordering::SeqCst);
                        if let Some(sender) = started_tx.lock().unwrap().take() {
                            let _ = sender.send(());
                        }
                        if let Some(receiver) = release_rx.lock().await.take() {
                            let _ = receiver.await;
                        }
                        StatusCode::UNAUTHORIZED.into_response()
                    } else {
                        assert_eq!(
                            headers.get("authorization").and_then(|v| v.to_str().ok()),
                            Some("Bearer new-token")
                        );
                        new_calls.fetch_add(1, Ordering::SeqCst);
                        let acks = body["events"].as_array().unwrap().iter().map(|event| json!({
                            "record_id": event["event_id"],
                            "payload_hash": event["metadata"]["sync_outbox"]["payload_hash"],
                            "ingestion_status": "created"
                        })).collect::<Vec<_>>();
                        (
                            StatusCode::OK,
                            axum::Json(
                                json!({"schema_version": 1, "acks": acks, "rejections": []}),
                            ),
                        )
                            .into_response()
                    }
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let _api = EnvVarGuard::set("ASTRA_API_URL", &address);
    let old_snapshot = crate::cli::cli_config::cli_utils::cli_owner_auth_snapshot();
    let direct_job = if direct_post_auth {
        Some(tokio::spawn(cloud_sync::run_post_auth_sync(
            async { Ok(()) },
            None,
            old_snapshot.clone(),
        )))
    } else {
        cloud_sync::schedule_sync_outbox_drain();
        None
    };
    tokio::time::timeout(std::time::Duration::from_secs(3), started_rx)
        .await
        .unwrap()
        .unwrap();
    let old_binding = old_snapshot.legacy_binding.as_ref().unwrap().clone();
    let old_pair = old_binding.pair.lock().await;
    crate::cli::auth_flow::save_profile_auth_tokens(
        Some("cloud-sync-test"),
        "user",
        &crate::cli::auth_flow::AuthTokenPayload {
            user_id: "cloud-sync-test-account".into(),
            access_token: "new-token".into(),
            refresh_token: "new-refresh".into(),
        },
    )
    .unwrap();
    assert_eq!(cloud_sync::try_drain_sync_outbox(10).await.attempted, 0);
    // A successful login wakes the old worker without waiting for its response.
    cloud_sync::clear_sync_cloud_access();
    if !direct_post_auth {
        cloud_sync::schedule_sync_outbox_drain();
    }
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if store.status().unwrap().acked == 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("new login resumes the settled record");
    let _ = release_tx.send(());
    drop(old_pair);
    if let Some(job) = direct_job {
        let stale_report = job.await.unwrap();
        assert_eq!(
            stale_report.blocker,
            Some(cloud_sync::SyncOutboxDrainBlocker::LocalSyncPending)
        );
    }
    assert_eq!(old_calls.load(Ordering::SeqCst), 1);
    assert_eq!(new_calls.load(Ordering::SeqCst), 1);
    let status = store.status().unwrap();
    assert_eq!(status.pending, 0);
    assert_eq!(status.in_flight, 0);
    server.abort();
}

#[serial_test::serial]
#[tokio::test]
async fn relogin_hands_background_claims_to_the_new_authentication() {
    exercise_relogin_handoff(false).await;
}

#[serial_test::serial]
#[tokio::test]
async fn relogin_hands_post_auth_claims_to_the_new_authentication() {
    exercise_relogin_handoff(true).await;
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
    append_cloud_pull_sync_journal_for_immediate_drain(&state, "default", "post_login", &pull, &[])
        .await
        .unwrap();

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
    assert_eq!(status.pending, 2);
    assert_eq!(status.ready, 2);
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

#[derive(Clone)]
struct SyncDbTestAuth(String);

type SyncDbAuthResult<T> = Result<
    T,
    (
        axum::http::StatusCode,
        axum::Json<astra_core::ErrorResponse>,
    ),
>;

#[async_trait::async_trait]
impl astra_services::AuthService for SyncDbTestAuth {
    async fn register(
        &self,
        _: astra_services::AuthRegisterRequestData,
    ) -> SyncDbAuthResult<astra_services::AuthUserRecord> {
        Err(astra_core::error_response(
            axum::http::StatusCode::NOT_IMPLEMENTED,
            "test",
        ))
    }

    async fn login(
        &self,
        _: astra_services::AuthLoginRequestData,
    ) -> SyncDbAuthResult<astra_services::AuthTokenRecord> {
        Err(astra_core::error_response(
            axum::http::StatusCode::NOT_IMPLEMENTED,
            "test",
        ))
    }

    async fn refresh(
        &self,
        _: astra_services::AuthRefreshRequestData,
    ) -> SyncDbAuthResult<astra_services::AuthTokenRecord> {
        Err(astra_core::error_response(
            axum::http::StatusCode::NOT_IMPLEMENTED,
            "test",
        ))
    }

    async fn logout(&self, _: astra_services::AuthRefreshRequestData) -> SyncDbAuthResult<()> {
        Err(astra_core::error_response(
            axum::http::StatusCode::NOT_IMPLEMENTED,
            "test",
        ))
    }

    async fn current_user(
        &self,
        headers: &axum::http::HeaderMap,
    ) -> SyncDbAuthResult<astra_services::AuthUserRecord> {
        if headers.get("authorization").and_then(|v| v.to_str().ok()) != Some("Bearer token") {
            return Err(astra_core::error_response(
                axum::http::StatusCode::UNAUTHORIZED,
                "test",
            ));
        }
        Ok(astra_services::AuthUserRecord {
            user_id: self.0.clone(),
            username: self.0.clone(),
            email: "sync-test@example.test".into(),
            display_name: None,
        })
    }
}

struct SyncDbTestHealth;

#[async_trait::async_trait]
impl astra_runtime::HealthChecker for SyncDbTestHealth {
    async fn database_healthy(&self) -> bool {
        true
    }
}

#[derive(Clone)]
struct SyncDbTestProxy {
    upstream: String,
    client: reqwest::Client,
    writes: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    reads: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    active: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    peak_active: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

async fn forward_sync_test_request(
    axum::extract::State(proxy): axum::extract::State<SyncDbTestProxy>,
    uri: axum::http::Uri,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> (axum::http::StatusCode, String) {
    use std::sync::atomic::Ordering;
    let active = proxy.active.fetch_add(1, Ordering::Relaxed) + 1;
    proxy.peak_active.fetch_max(active, Ordering::Relaxed);
    let response = proxy
        .client
        .post(format!("{}{}", proxy.upstream, uri.path()))
        .header("authorization", headers["authorization"].clone())
        .header(
            SYNC_OUTBOX_SIGNATURE_HEADER,
            headers[SYNC_OUTBOX_SIGNATURE_HEADER].clone(),
        )
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .expect("forward sync request");
    let status = response.status();
    let body = response.text().await.expect("read sync response");
    proxy.active.fetch_sub(1, Ordering::Relaxed);
    if uri.path() == "/sync/outbox/events" {
        assert_eq!(
            status,
            axum::http::StatusCode::OK,
            "upstream response: {body}"
        );
        proxy.writes.fetch_add(1, Ordering::Relaxed);
        // The server has committed; emulate a gateway losing that response.
        (
            axum::http::StatusCode::BAD_GATEWAY,
            "lost acknowledgement".into(),
        )
    } else {
        proxy.reads.fetch_add(1, Ordering::Relaxed);
        (status, body)
    }
}

#[serial_test::serial]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires an explicitly isolated MatrixOne test database"]
async fn real_sync_batch_recovers_lost_ack_through_http_and_matrixone() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    assert_eq!(std::env::var("ASTRA_TEST_DB_IT").as_deref(), Ok("1"));
    let settings = astra_core::MatrixOneSettings::from_env();
    let test_database = std::env::var("ASTRA_TEST_DATABASE").expect("isolated test database");
    assert_eq!(settings.database, test_database);
    assert!(test_database.starts_with("astra_test_"));
    astra_services::storage::ensure_core_schema(&settings, "mysql")
        .await
        .unwrap();
    let pool = astra_core::SharedPool::new(&settings).await.unwrap();
    let owner = uuid::Uuid::new_v4().to_string();
    let events = astra_services::DatabaseEventService::new(settings).with_pool(pool.clone());
    let app = astra_runtime::build_app(
        astra_runtime::AppState::new(
            astra_runtime::ServiceInfo::default(),
            Arc::new(SyncDbTestHealth),
        )
        .with_auth_service(Arc::new(SyncDbTestAuth(owner.clone())))
        .with_event_service(Arc::new(events)),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let runtime = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let proxy = SyncDbTestProxy {
        upstream,
        client: reqwest::Client::builder().no_proxy().build().unwrap(),
        writes: Arc::new(AtomicUsize::new(0)),
        reads: Arc::new(AtomicUsize::new(0)),
        active: Arc::new(AtomicUsize::new(0)),
        peak_active: Arc::new(AtomicUsize::new(0)),
    };
    let proxy_app = axum::Router::new()
        .route(
            "/sync/outbox/events",
            axum::routing::post(forward_sync_test_request),
        )
        .route(
            "/sync/outbox/events/reconcile",
            axum::routing::post(forward_sync_test_request),
        )
        .with_state(proxy.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_url = format!("http://{}", listener.local_addr().unwrap());
    let proxy_task = tokio::spawn(async move { axum::serve(listener, proxy_app).await.unwrap() });

    let temp = tempfile::tempdir().unwrap();
    let _journal = ProcessJournalDirGuard::new(temp.path());
    let _api = EnvVarGuard::set("ASTRA_API_URL", &proxy_url);
    let _no_proxy = EnvVarGuard::set("NO_PROXY", "localhost,127.0.0.1");
    let _auth = install_test_cloud_auth(Some("token"));
    let store = SyncOutboxStore::local();
    let scale = std::env::var_os("ASTRA_TEST_SYNC_1000_SESSIONS").is_some();
    let sessions = if scale { 1_000 } else { 1 };
    let turns: u32 = std::env::var("ASTRA_TEST_SYNC_TURNS")
        .ok()
        .map(|value| value.parse().expect("valid turn count"))
        .unwrap_or(if scale { 1 } else { 2 });
    let drain_limit: usize = std::env::var("ASTRA_TEST_SYNC_DRAIN_LIMIT")
        .ok()
        .map(|value| value.parse().expect("valid drain limit"))
        .unwrap_or(64);
    assert!(turns > 0 && drain_limit > 0);
    let mut journal_events = Vec::new();
    for _ in 0..sessions {
        let session = uuid::Uuid::new_v4().to_string();
        for turn in 1..=turns {
            journal_events.push(session_journal::JournalEvent::turn(
                Some(&session),
                turn,
                Some("sync-test"),
                "question",
                "answer",
                0,
                10,
                5,
                1,
            ));
        }
    }
    store.enqueue_journal_events(&journal_events).unwrap();
    if !scale && turns == 2 && drain_limit == 64 {
        let record = store.ready_records(1).unwrap().remove(0);
        let valid = json!({
            "event_id": record.record_id,
            "session_id": record.session_id,
            "event_type": record.event_type,
            "content": record.canonical_payload_json(),
            "agent_id": "edge_sync",
            "agent_version": env!("CARGO_PKG_VERSION"),
            "metadata": {"sync_outbox": {"payload_hash": record.payload_hash}},
        });
        let invalid = json!({
            "event_id": "malformed-event",
            "session_id": record.session_id,
            "event_type": 42,
            "content": "{}",
        });
        let client = astra_thin_client::ThinClient::new(&proxy.upstream, None).unwrap();
        let response = client
            .post_sync_outbox_events_json(
                Some("token"),
                &astra_sync_protocol::SyncOutboxEventBatch::new(vec![valid, invalid]),
            )
            .await
            .unwrap();
        assert_eq!(response.acks.len(), 1);
        assert_eq!(response.acks[0].record_id, record.record_id);
        assert_eq!(response.rejections.len(), 1);
        assert_eq!(response.rejections[0].record_id, "malformed-event");
    }
    let start = std::time::Instant::now();
    let mut acked = 0;
    while acked < journal_events.len() {
        let report = cloud_sync::try_drain_sync_outbox(drain_limit).await;
        assert!(
            report.attempted > 0,
            "sync drain stopped making progress: {report:?}; outbox: {:?}",
            store.status()
        );
        if report.failed > 0 {
            eprintln!(
                "sync report: {report:?}; writes={}, reads={}, elapsed={:?}",
                proxy.writes.load(Ordering::Relaxed),
                proxy.reads.load(Ordering::Relaxed),
                start.elapsed()
            );
            eprintln!("sync outbox: {:?}", store.status());
            let state = std::fs::read(store.path()).and_then(|bytes| {
                serde_json::from_slice::<Value>(&bytes).map_err(std::io::Error::other)
            });
            match state {
                Ok(state) => {
                    let mut errors = std::collections::BTreeMap::<
                        (String, String),
                        (usize, String, String),
                    >::new();
                    for record in state["records"].as_array().into_iter().flatten() {
                        if let Some(error) = record["last_error"].as_str() {
                            let key = (
                                record["state"].as_str().unwrap_or("unknown").to_string(),
                                error.to_string(),
                            );
                            let entry = errors.entry(key).or_insert_with(|| {
                                (
                                    0,
                                    record["record_id"].as_str().unwrap_or("").into(),
                                    record["payload_hash"].as_str().unwrap_or("").into(),
                                )
                            });
                            entry.0 += 1;
                        }
                    }
                    for ((state, error), (count, id, _)) in &errors {
                        eprintln!(
                            "sync failure: state={state}, count={count}, record={id}, error={error:?}"
                        );
                    }
                    for (_, id, hash) in errors.values().take(3) {
                        let stored = tokio::time::timeout(
                            std::time::Duration::from_secs(2),
                            sqlx::query_scalar::<_, String>("SELECT payload_hash FROM agent_events WHERE user_id = ? AND event_id = ?")
                                .bind(&owner).bind(id).fetch_optional(pool.get()),
                        ).await;
                        eprintln!("sync failure: record={id}, stored_hash_matches={:?}", stored.map(|result| result.map(|value| value.map(|value| value == *hash))));
                    }
                }
                Err(error) => eprintln!("could not inspect sync outbox: {error}"),
            }
            panic!("sync drain did not settle all records");
        }
        acked += report.acked as usize;
    }
    assert_eq!(acked, journal_events.len());
    assert_eq!(
        store.status().unwrap().ack_watermark,
        journal_events.len() as u64
    );
    if sessions == 1 || turns == 1 {
        let batch_size = drain_limit.min(astra_sync_protocol::MAX_SYNC_OUTBOX_BATCH_EVENTS);
        let expected_requests = sessions * (turns as usize).div_ceil(batch_size);
        assert_eq!(proxy.writes.load(Ordering::Relaxed), expected_requests);
        assert_eq!(proxy.reads.load(Ordering::Relaxed), expected_requests);
    }
    let peak = proxy.peak_active.load(Ordering::Relaxed);
    assert!(peak <= 8, "session concurrency must stay bounded");
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_events WHERE user_id = ?")
        .bind(&owner)
        .fetch_one(pool.get())
        .await
        .unwrap();
    assert_eq!(count, journal_events.len() as i64);
    if scale || turns != 2 || drain_limit != 64 {
        eprintln!(
            "lost-ACK workload: {sessions} sessions, {turns} turns, drain limit {drain_limit}, {}ms, {} writes, {} reconciliations, peak {} concurrent",
            start.elapsed().as_millis(),
            proxy.writes.load(Ordering::Relaxed),
            proxy.reads.load(Ordering::Relaxed),
            peak
        );
    }
    for sql in [
        "DELETE FROM agent_event_edges WHERE user_id = ?",
        "DELETE FROM agent_events WHERE user_id = ?",
        "DELETE FROM agent_sessions WHERE user_id = ?",
        "DELETE FROM agent_session_lifecycle_fences WHERE user_id = ?",
    ] {
        sqlx::query(sql)
            .bind(&owner)
            .execute(pool.get())
            .await
            .unwrap();
    }
    proxy_task.abort();
    runtime.abort();
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
