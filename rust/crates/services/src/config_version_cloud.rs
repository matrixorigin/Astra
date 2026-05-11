//! Server-side schema + query builders for the content-addressed
//! config version store.
//!
//! Shape:
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS config_versions (
//!     version_id          VARCHAR(24) NOT NULL,
//!     user_id             VARCHAR(64) NOT NULL,
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
    user_id             VARCHAR(64) NOT NULL,
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
     (version_id, user_id, toml_body, created_at, first_seen_session) \
     VALUES (?, ?, ?, FROM_UNIXTIME(? / 1000.0), ?)";

/// SELECT TOML body by (user_id, version_id) — the primary cloud
/// fetch used by `astra config sync pull`.
pub const CONFIG_VERSIONS_SELECT_TOML_SQL: &str =
    "SELECT toml_body FROM config_versions WHERE user_id = ? AND version_id = ?";

/// SELECT recent version metadata for list view.
pub const CONFIG_VERSIONS_LIST_SQL: &str = "SELECT version_id, user_id, toml_body, UNIX_TIMESTAMP(created_at) * 1000 AS created_at_ms, \
            first_seen_session \
     FROM config_versions \
     WHERE user_id = ? \
     ORDER BY created_at DESC \
     LIMIT ?";

/// Typed shape of a `config_versions` row. Used by both the push
/// path (Rust → INSERT binds) and the pull path (SELECT rows →
/// Rust). Keeping the struct small and flat avoids a drift between
/// INSERT columns and SELECT projections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigVersionRow {
    pub version_id: String,
    pub user_id: String,
    pub toml_body: String,
    /// Unix epoch milliseconds. We handle the conversion at the SQL
    /// boundary so the in-memory type stays plain integer.
    pub created_at_ms: i64,
    pub first_seen_session: Option<String>,
}

/// Bind-order placeholder for the INSERT statement. Thin wrapper
/// over `ConfigVersionRow` that makes the mapping explicit: if
/// someone adds a column, they must also add a bind position
/// here, which breaks the schema-shape test.
#[derive(Debug, Clone)]
pub struct ConfigVersionInsertParams<'a> {
    pub version_id: &'a str,
    pub user_id: &'a str,
    pub toml_body: &'a str,
    pub created_at_ms: i64,
    pub first_seen_session: Option<&'a str>,
}

/// Build the ordered bind list for `CONFIG_VERSIONS_INSERT_SQL`.
pub fn config_versions_insert_params(row: &ConfigVersionRow) -> ConfigVersionInsertParams<'_> {
    ConfigVersionInsertParams {
        version_id: &row.version_id,
        user_id: &row.user_id,
        toml_body: &row.toml_body,
        created_at_ms: row.created_at_ms,
        first_seen_session: row.first_seen_session.as_deref(),
    }
}

/// Identity parser — kept as a named function so row-shape drift
/// between the SELECT statement and `ConfigVersionRow` struct
/// surfaces as a single place to change. Call-sites that read
/// `MySqlRow` will first extract fields into a `ConfigVersionRow`
/// and then pass it here (mostly for future extension: redaction,
/// validation, or mapping alternative on-disk representations).
pub fn parse_config_version_row(row: &ConfigVersionRow) -> ConfigVersionRow {
    row.clone()
}

/// Canonical `event_type` for queued config-version pushes. The
/// ingestion worker classifies events by this tag; moving / renaming
/// it is a breaking change since the classifier string is embedded
/// in queued-but-not-yet-flushed events.
pub const CONFIG_VERSION_SAVED_EVENT_TYPE: &str = "config_version_saved";

/// Turn a queued `IngestionEvent` back into a typed row, iff it was
/// produced by `IngestionEvent::for_config_version`. Returns `None`
/// for any other event_type so the worker can cleanly decide whether
/// to also write to the `config_versions` table.
pub fn extract_config_version_row(
    event: &crate::event_ingestion::IngestionEvent,
) -> Option<ConfigVersionRow> {
    if event.event_type != CONFIG_VERSION_SAVED_EVENT_TYPE {
        return None;
    }
    let toml_body = event.content.clone().unwrap_or_default();
    // Treat empty session_id as "no session" so the roundtrip
    // matches what `for_config_version` put in (None → empty).
    let first_seen_session = if event.session_id.is_empty() {
        None
    } else {
        Some(event.session_id.clone())
    };
    // created_at on the IngestionEvent is an ISO-8601 string; we
    // don't carry that back into ConfigVersionRow on the hot path
    // because the SQL INSERT uses either FROM_UNIXTIME on a bound
    // epoch-ms value OR the server's CURRENT_TIMESTAMP default.
    // Scripts that need forensic timestamps read the DATETIME column
    // directly on the pull side.
    Some(ConfigVersionRow {
        version_id: event.event_id.clone(),
        user_id: event.user_id.clone(),
        toml_body,
        created_at_ms: 0,
        first_seen_session,
    })
}
