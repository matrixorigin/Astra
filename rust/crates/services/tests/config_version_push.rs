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
//!   * `extract_config_version_row(&event)` is the inverse — turns
//!     a queued event back into a `ConfigVersionRow` ready for
//!     `config_versions_insert_params`. Used by the worker to
//!     decide whether to also write to the `config_versions` table.
//!   * `event_type` discriminator is stable so a future schema
//!     change doesn't silently move events past the classifier.

use astra_services::config_version_cloud::{
    CONFIG_VERSION_SAVED_EVENT_TYPE, ConfigVersionRow, extract_config_version_row,
};
use astra_services::event_ingestion::IngestionEvent;

fn sample_row() -> ConfigVersionRow {
    ConfigVersionRow {
        version_id: "cfg_abcdef0123456789".to_string(),
        user_id: "user_test".to_string(),
        toml_body: "[token_budget]\nmax_turn_input_tokens = 500000\n".to_string(),
        created_at_ms: 1_778_485_059_634,
        first_seen_session: Some("sess_xyz".to_string()),
    }
}

#[test]
fn for_config_version_sets_the_canonical_event_type() {
    let row = sample_row();
    let evt = IngestionEvent::for_config_version(&row);
    assert_eq!(
        evt.event_type, CONFIG_VERSION_SAVED_EVENT_TYPE,
        "classifier relies on a stable event_type discriminator"
    );
}

#[test]
fn for_config_version_places_toml_body_in_content() {
    let row = sample_row();
    let evt = IngestionEvent::for_config_version(&row);
    assert_eq!(
        evt.content.as_deref(),
        Some(row.toml_body.as_str()),
        "TOML must land in content — it's the cloud-side payload"
    );
    assert_eq!(evt.user_id, row.user_id);
    assert_eq!(
        evt.session_id,
        row.first_seen_session.clone().unwrap_or_default()
    );
}

#[test]
fn for_config_version_uses_version_id_as_event_id() {
    // Content-addressed by construction: two puts of the same
    // config push the same event_id, so INSERT IGNORE on
    // agent_events (PK event_id) dedups them on the agent_events
    // side too. That's the right behaviour — the event is a fact
    // about "this version was saved", and the id IS that fact.
    let row = sample_row();
    let evt = IngestionEvent::for_config_version(&row);
    assert_eq!(evt.event_id, row.version_id);
}

#[test]
fn extract_roundtrips_back_to_the_original_row() {
    let row = sample_row();
    let evt = IngestionEvent::for_config_version(&row);
    let recovered = extract_config_version_row(&evt).expect("classifier must recover the row");
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
    let mut evt = IngestionEvent::for_config_version(&sample_row());
    evt.event_type = "turn".to_string();
    assert!(
        extract_config_version_row(&evt).is_none(),
        "classifier must gate on event_type"
    );
}

#[test]
fn extract_tolerates_missing_session() {
    // first_seen_session can be None (line-mode saves or very-first
    // startup). Round-trip must preserve that shape without panic.
    let mut row = sample_row();
    row.first_seen_session = None;
    let evt = IngestionEvent::for_config_version(&row);
    let recovered = extract_config_version_row(&evt).expect("roundtrip ok");
    assert!(recovered.first_seen_session.is_none());
}
