//! Step 2a contract: `RuntimeConfig::load_with_version` is the front
//! door that every session should walk through at startup. It resolves
//! the config (defaults → user → project → env → --settings overlay),
//! hashes the result, records a put in the version store, and returns
//! both the live config and the id.
//!
//! Downstream, `HeavyCheckpoint` carries a `config_version_id: Option<String>`;
//! resume uses the pointer to re-hydrate the exact config the session
//! ran under, not whatever the disk happens to say today.
//!
//! Tests here only exercise the in-crate path (load_with_version +
//! store roundtrip). The HeavyCheckpoint field is tested in
//! astra-pipeline's own suite.

use astra_config::config_versions::{ConfigVersionStore, LocalFileStore, PutMetadata};
use astra_config::runtime_config::RuntimeConfig;

fn tmp_store() -> (tempfile::TempDir, LocalFileStore) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = LocalFileStore::new(dir.path().to_path_buf());
    (dir, store)
}

#[test]
fn load_with_version_returns_config_and_matching_id() {
    let (_dir, store) = tmp_store();
    let (config, id) = RuntimeConfig::load_with_version(
        &store,
        PutMetadata {
            source_session: Some("sess_test".into()),
            parent: None,
        },
    )
    .expect("load_with_version ok");

    // The id must be resolvable back to the exact config via the store.
    let roundtrip_toml = store
        .get_toml(&id)
        .expect("lookup")
        .expect("id must resolve");
    let reparsed: RuntimeConfig = toml::from_str(&roundtrip_toml).expect("reparse");
    assert_eq!(
        reparsed.token_budget.max_turn_input_tokens,
        config.token_budget.max_turn_input_tokens,
        "pointer must resolve to the same content that was loaded"
    );
}

#[test]
fn load_with_version_is_stable_across_calls_when_nothing_changes() {
    // Two startups with the same resolved config must produce the same
    // id so audit correctly answers "these two sessions ran the same
    // config".
    let (_dir, store) = tmp_store();
    let (_c1, id1) = RuntimeConfig::load_with_version(&store, PutMetadata::default()).unwrap();
    let (_c2, id2) = RuntimeConfig::load_with_version(&store, PutMetadata::default()).unwrap();
    assert_eq!(id1, id2);
}

#[test]
fn load_with_version_records_source_session_in_index() {
    let (_dir, store) = tmp_store();
    let meta = PutMetadata {
        source_session: Some("sess_abc".into()),
        parent: None,
    };
    let (_c, id) = RuntimeConfig::load_with_version(&store, meta).unwrap();

    let entries = store.list().unwrap();
    assert!(
        entries
            .iter()
            .any(|e| e.id == id && e.source_session.as_deref() == Some("sess_abc")),
        "session id must land in the index: {entries:?}"
    );
}

#[test]
fn id_for_same_config_has_from_toml_bytes_parity() {
    // Public VersionId::from_toml_bytes must agree with what the store
    // arrives at. This lets callers that hold a config (e.g. after an
    // overlay merge) ask "what id would this be?" without side effects.
    use astra_config::config_versions::VersionId;
    let (_dir, store) = tmp_store();
    let cfg = RuntimeConfig::default();
    let id_via_put = store.put(&cfg, PutMetadata::default()).unwrap();
    let toml = toml::to_string_pretty(&cfg).unwrap();
    let id_via_pure = VersionId::from_toml_bytes(toml.as_bytes());
    assert_eq!(id_via_put, id_via_pure);
}
