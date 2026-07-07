//! Step 4b contract: the IngestionEvent path that pushes config
//! versions to the cloud.
//!
//! Wire flow:
//!   * `/config` save → `MatrixCloudRuntime::enqueue_config_version_push`
//!   * worker classifies the event (event_type = "config_version_saved")
//!     and dual-writes: agent_events (metadata trail) + config_versions
//!     (content blob + tenant scoping).
//!
//! This test file locks in the non-DB edge of that contract:
//!
//!   * `IngestionEvent::for_config_version(row)` builds the event
//!     with the canonical shape the worker recognises.
//!   * `extract_config_version_payload(&event)` is the inverse —
//!     turns a queued event back into the payload written to the
//!     `config_versions` table.
//!   * `event_type` discriminator is stable so a future schema
//!     change doesn't silently move events past the classifier.

use astra_services::config_version_cloud::{
    CONFIG_VERSION_SAVED_EVENT_TYPE, ConfigVersionPayload, extract_config_version_payload,
};
use astra_services::event_ingestion::IngestionEvent;

fn sample_payload() -> ConfigVersionPayload {
    ConfigVersionPayload {
        version_id: "cfg_abcdef0123456789".to_string(),
        user_id: "user_test".to_string(),
        toml_body: "[token_budget]\nmax_turn_input_tokens = 500000\n".to_string(),
        first_seen_session: Some("sess_xyz".to_string()),
    }
}

#[test]
fn for_config_version_sets_the_canonical_event_type() {
    let row = sample_payload();
    let evt = IngestionEvent::for_config_version(&row).expect("valid config event");
    assert_eq!(
        evt.event_type, CONFIG_VERSION_SAVED_EVENT_TYPE,
        "classifier relies on a stable event_type discriminator"
    );
}

#[test]
fn for_config_version_places_toml_body_in_content() {
    let row = sample_payload();
    let evt = IngestionEvent::for_config_version(&row).expect("valid config event");
    assert_eq!(
        evt.content.as_deref(),
        Some(row.toml_body.as_str()),
        "TOML must land in content — it's the cloud-side payload"
    );
    assert_eq!(evt.user_id, row.user_id);
    assert_eq!(evt.session_id, row.first_seen_session.as_deref().unwrap());
}

#[test]
fn for_config_version_requires_session_identity() {
    let mut row = sample_payload();
    row.first_seen_session = None;

    let error = IngestionEvent::for_config_version(&row).expect_err("session id is required");

    assert!(
        error.contains("first_seen_session") && error.contains(&row.version_id),
        "error should identify invalid config version event ownership: {error}"
    );
}

#[test]
fn for_config_version_uses_version_id_as_event_id() {
    // Content-addressed by construction: two puts of the same
    // config push the same event_id, so INSERT IGNORE on
    // agent_events (PK event_id) dedups them on the agent_events
    // side too. That's the right behaviour — the event is a fact
    // about "this version was saved", and the id IS that fact.
    let row = sample_payload();
    let evt = IngestionEvent::for_config_version(&row).expect("valid config event");
    assert_eq!(evt.event_id, row.version_id);
}

#[test]
fn extract_roundtrips_back_to_the_original_row() {
    let row = sample_payload();
    let evt = IngestionEvent::for_config_version(&row).expect("valid config event");
    let recovered = extract_config_version_payload(&evt)
        .expect("classifier should not fail")
        .expect("classifier must recover the row");
    assert_eq!(recovered.version_id, row.version_id);
    assert_eq!(recovered.user_id, row.user_id);
    assert_eq!(recovered.toml_body, row.toml_body);
    assert_eq!(recovered.first_seen_session, row.first_seen_session);
}

#[test]
fn extract_rejects_unrelated_event_types() {
    // The classifier must refuse to pull a "turn" event's payload
    // into the config_versions table. The check is the event_type
    // tag, not heuristics on the metadata body.
    let mut evt =
        IngestionEvent::for_config_version(&sample_payload()).expect("valid config event");
    evt.event_type = "turn".to_string();
    assert!(
        extract_config_version_payload(&evt)
            .expect("unrelated event types should not fail")
            .is_none(),
        "classifier must gate on event_type"
    );
}

#[test]
fn extract_rejects_config_event_missing_required_payload() {
    let mut evt =
        IngestionEvent::for_config_version(&sample_payload()).expect("valid config event");
    evt.session_id = String::new();
    let error = extract_config_version_payload(&evt).expect_err("missing session must fail");
    assert!(
        error.contains("missing session_id") && error.contains(&evt.event_id),
        "missing session should fail loudly: {error}"
    );

    let mut evt =
        IngestionEvent::for_config_version(&sample_payload()).expect("valid config event");
    evt.content = None;
    let error = extract_config_version_payload(&evt).expect_err("missing content must fail");
    assert!(
        error.contains("missing TOML content") && error.contains(&evt.event_id),
        "missing content should fail loudly: {error}"
    );
}
