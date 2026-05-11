//! Step 2b contract for the `ConfigChange` journal event under the
//! content-addressed version scheme.
//!
//! Background: `JournalEvent::config_change(key, value)` already exists
//! for the legacy "change a single setting like model or verbose" flow.
//! Under the new scheme every edit lands as a new VersionId, and the
//! journal must say which id took over from which. Keeping the old
//! constructor intact avoids churning unrelated call sites; this test
//! locks in a new constructor.
//!
//! The contract:
//!
//!   * `JournalEvent::config_version_change(session_id, turn, from, to, source)`
//!     produces a `ConfigChange`-typed event whose metadata carries
//!     `from`, `to`, and `source` as a small JSON object.
//!   * `source` is a short human-readable tag: "slash_config_edit",
//!     "settings_overlay", "startup". Kept as a free string so adding
//!     a new source later doesn't need a schema migration.
//!   * Missing `from` (first ever load in a session) is allowed;
//!     `to` is always set.
//!   * Serde round-trip preserves every field.

use astra_services::session_journal::{JournalEvent, JournalEventType};

#[test]
fn config_version_change_produces_config_change_event_type() {
    let evt = JournalEvent::config_version_change(
        Some("sess_test"),
        3,
        Some("cfg_aaa1111222233333"),
        "cfg_bbb4444555566666",
        "slash_config_edit",
    );
    assert_eq!(evt.event_type, JournalEventType::ConfigChange);
    assert_eq!(evt.turn, Some(3));
}

#[test]
fn config_version_change_metadata_carries_from_to_source() {
    let evt = JournalEvent::config_version_change(
        Some("sess_test"),
        42,
        Some("cfg_from01234567"),
        "cfg_to0123456789",
        "settings_overlay",
    );
    let meta = evt.metadata.expect("must have metadata");
    let cfg = meta
        .get("config_version")
        .expect("metadata.config_version object");
    assert_eq!(cfg.get("from").and_then(|v| v.as_str()), Some("cfg_from01234567"));
    assert_eq!(cfg.get("to").and_then(|v| v.as_str()), Some("cfg_to0123456789"));
    assert_eq!(
        cfg.get("source").and_then(|v| v.as_str()),
        Some("settings_overlay")
    );
}

#[test]
fn config_version_change_accepts_none_from_for_initial_load() {
    // First load of a session has no predecessor — `from` may be
    // absent. The event must still serialize cleanly.
    let evt =
        JournalEvent::config_version_change(Some("sess_test"), 0, None, "cfg_initial000000", "startup");
    let meta = evt.metadata.expect("must have metadata");
    let cfg = meta.get("config_version").expect("object");
    assert!(
        cfg.get("from").map(|v| v.is_null() || v.is_string()).unwrap_or(true),
        "from should be null or string, got {:?}",
        cfg.get("from")
    );
    assert_eq!(cfg.get("to").and_then(|v| v.as_str()), Some("cfg_initial000000"));
    assert_eq!(cfg.get("source").and_then(|v| v.as_str()), Some("startup"));
}

#[test]
fn config_version_change_round_trips_through_serde() {
    let evt = JournalEvent::config_version_change(
        Some("sess_roundtrip"),
        7,
        Some("cfg_from01234567"),
        "cfg_to0123456789",
        "slash_config_edit",
    );
    let json = serde_json::to_string(&evt).expect("serialize");
    let de: JournalEvent = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(de.event_type, JournalEventType::ConfigChange);
    assert_eq!(de.turn, Some(7));
    let meta = de.metadata.expect("metadata survives");
    let cfg = meta.get("config_version").expect("object survives");
    assert_eq!(cfg.get("to").and_then(|v| v.as_str()), Some("cfg_to0123456789"));
    assert_eq!(cfg.get("from").and_then(|v| v.as_str()), Some("cfg_from01234567"));
}
