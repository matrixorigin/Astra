//! Server-side schema + query builders for the content-addressed
//! config version store.
//!
//! Shape:
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS config_versions (
//!     version_id          VARCHAR(24) NOT NULL,
//!     user_id             VARCHAR(128) NOT NULL,
//!     toml_body           MEDIUMTEXT NOT NULL,
//!     created_at          DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
//!     first_seen_session  VARCHAR(64) NULL,
//!     PRIMARY KEY (user_id, version_id),
//!     INDEX idx_cv_user_created (user_id, created_at DESC)
//! )
//! ```
//!
//! Primary key is `(user_id, version_id)` so two tenants can
//! independently arrive at the same content-addressed id without
//! collision; the TOML body is stored once per tenant. Not a single-
//! global-id model (one tenant per row) because config may carry
//! tenant-specific credentials or preferences we must not leak across
//! accounts.

/// Idempotent DDL for the cloud version store. Run by
/// `ensure_core_schema` on every server boot.
pub const CONFIG_VERSIONS_CREATE_SQL: &str = "CREATE TABLE IF NOT EXISTS config_versions (
    version_id          VARCHAR(24) NOT NULL,
    user_id             VARCHAR(128) NOT NULL,
    toml_body           MEDIUMTEXT NOT NULL,
    created_at          DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    first_seen_session  VARCHAR(64) NULL,
    PRIMARY KEY (user_id, version_id),
    INDEX idx_cv_user_created (user_id, created_at DESC)
)";

/// Idempotent INSERT for a fresh version row. Uses INSERT IGNORE
/// on the composite PK so cloud push is naturally at-least-once
/// safe — a duplicate put from the same user returns 0 rows
/// affected rather than an error.
pub const CONFIG_VERSIONS_INSERT_SQL: &str = "INSERT IGNORE INTO config_versions \
     (version_id, user_id, toml_body, first_seen_session) \
     VALUES (?, ?, ?, ?)";

/// SELECT TOML body by (user_id, version_id) — the primary cloud
/// fetch for owner-scoped config-version inspection.
pub const CONFIG_VERSIONS_SELECT_TOML_SQL: &str =
    "SELECT toml_body FROM config_versions WHERE user_id = ? AND version_id = ?";

/// SELECT recent version metadata for list view.
pub const CONFIG_VERSIONS_LIST_SQL: &str = "SELECT version_id, user_id, toml_body, first_seen_session \
     FROM config_versions \
     WHERE user_id = ? \
     ORDER BY created_at DESC \
     LIMIT ?";

/// Payload for saving a config version to cloud storage. `created_at`
/// is intentionally absent: cloud persistence timestamps are assigned
/// by the database default at insert time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigVersionPayload {
    pub version_id: String,
    pub user_id: String,
    pub toml_body: String,
    pub first_seen_session: Option<String>,
}

/// Canonical `event_type` for queued config-version pushes. The
/// ingestion worker classifies events by this tag; moving / renaming
/// it is a breaking change since the classifier string is embedded
/// in queued-but-not-yet-flushed events.
pub const CONFIG_VERSION_SAVED_EVENT_TYPE: &str = "config_version_saved";

/// Turn a queued `IngestionEvent` back into a typed row, iff it was
/// produced by `IngestionEvent::for_config_version`. Returns `Ok(None)`
/// for any other event_type so the worker can cleanly decide whether
/// to also write to the `config_versions` table. Config-version events
/// with malformed required payload fail loudly so the batch rolls back.
pub fn extract_config_version_payload(
    event: &crate::event_ingestion::IngestionEvent,
) -> Result<Option<ConfigVersionPayload>, String> {
    if event.event_type != CONFIG_VERSION_SAVED_EVENT_TYPE {
        return Ok(None);
    }
    let toml_body = event.content.clone().ok_or_else(|| {
        format!(
            "config version event {} is missing TOML content",
            event.event_id
        )
    })?;
    let first_seen_session = event.session_id.trim();
    if first_seen_session.is_empty() {
        return Err(format!(
            "config version event {} is missing session_id",
            event.event_id
        ));
    }
    // created_at on the IngestionEvent is an ISO-8601 string. We do
    // not carry it into `config_versions`; the table's database-side
    // default is the durable forensic timestamp for cloud persistence.
    Ok(Some(ConfigVersionPayload {
        version_id: event.event_id.clone(),
        user_id: event.user_id.clone(),
        toml_body,
        first_seen_session: Some(first_seen_session.to_string()),
    }))
}
