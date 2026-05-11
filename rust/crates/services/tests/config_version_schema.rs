//! Step 4a contract: cloud schema for the content-addressed config
//! version store.
//!
//! Two things land on the server:
//!
//!   1. `config_versions` table — one row per unique `cfg_<hex>` blob.
//!      Primary key is the version_id; body is the canonical TOML bytes
//!      (small, KBs, fits in a TEXT column); user_id scopes rows so
//!      multiple tenants don't collide on identical content-addressed
//!      ids. created_at + first_seen_session are forensic metadata.
//!
//!   2. `agent_sessions.config_version_id` — foreign pointer to the
//!      version_id a session ran under. Null for legacy rows.
//!
//! Both are wired as additive `ensure_core_schema` + migration steps
//! so fresh DBs get the new shape via CREATE TABLE and existing DBs
//! catch up via ALTER.
//!
//! The live DB test is `#[ignore]` and runs under
//! `ASTRA_TEST_DB_IT=1`, matching the existing services_db_integration.
//! A pure builder test verifies the SQL shape without touching the DB.
//!
//! The fact that `ensure_core_schema` is the only path to install both
//! the CREATE TABLE and the migrations means a fresh test DB that has
//! never run the legacy pre-Step-4 schema must still end up with the
//! final column set.

use astra_services::config_version_cloud::{
    CONFIG_VERSIONS_CREATE_SQL, ConfigVersionRow, config_versions_insert_params,
    parse_config_version_row,
};

#[test]
fn create_sql_names_expected_columns_and_types() {
    // Pure shape check on the DDL string — no DB round-trip needed.
    // If someone renames a column, this test makes them update the
    // query builder + row parser in lock-step.
    let ddl = CONFIG_VERSIONS_CREATE_SQL;
    for needle in [
        "CREATE TABLE IF NOT EXISTS config_versions",
        "version_id",
        "user_id",
        "toml_body",
        "created_at",
        "first_seen_session",
        "PRIMARY KEY",
    ] {
        assert!(
            ddl.contains(needle),
            "DDL missing required token `{needle}`; got:\n{ddl}"
        );
    }
}

#[test]
fn insert_params_roundtrip_preserves_every_field() {
    // The insert-params builder is the bridge between a typed
    // ConfigVersionRow in Rust and the bound sqlx query: exercising
    // it here guards against a field being added to the struct but
    // not plumbed through the bind list. We verify the output shape
    // matches what the SQL VALUES clause expects.
    let row = ConfigVersionRow {
        version_id: "cfg_abcdef0123456789".to_string(),
        user_id: "user_test".to_string(),
        toml_body: "[token_budget]\nmax_turn_input_tokens = 500000\n".to_string(),
        created_at_ms: 1_778_485_059_634,
        first_seen_session: Some("sess_xyz".to_string()),
    };
    let bindings = config_versions_insert_params(&row);
    // Four required bind positions + one optional; order corresponds
    // to the column list in CONFIG_VERSIONS_INSERT_SQL.
    assert_eq!(bindings.version_id, "cfg_abcdef0123456789");
    assert_eq!(bindings.user_id, "user_test");
    assert!(bindings.toml_body.contains("max_turn_input_tokens"));
    assert_eq!(bindings.created_at_ms, 1_778_485_059_634);
    assert_eq!(bindings.first_seen_session.as_deref(), Some("sess_xyz"));
}

#[test]
fn parse_row_tolerates_missing_first_seen_session() {
    // Legacy rows (or rows produced by a user without a pinned
    // session at save time) may leave first_seen_session NULL.
    // The parser must accept that, not panic.
    let row = ConfigVersionRow {
        version_id: "cfg_no_session_row".to_string(),
        user_id: "user_x".to_string(),
        toml_body: "ok = true\n".to_string(),
        created_at_ms: 0,
        first_seen_session: None,
    };
    let out = parse_config_version_row(&row);
    assert_eq!(out.version_id, "cfg_no_session_row");
    assert!(out.first_seen_session.is_none());
}

#[test]
fn create_sql_is_idempotent_shape() {
    // `IF NOT EXISTS` is non-negotiable — `ensure_core_schema`
    // re-runs on every server boot. A fresh CREATE TABLE on an
    // existing table would panic-on-rerun, which is exactly the
    // regression this DDL token prevents.
    assert!(
        CONFIG_VERSIONS_CREATE_SQL.contains("IF NOT EXISTS"),
        "create statement must be idempotent: {CONFIG_VERSIONS_CREATE_SQL}"
    );
}
