//! Step 4c helper: LocalFileStore must accept raw TOML bytes under
//! a specific VersionId so the `astra config sync pull` path can
//! write cloud-fetched blobs into the local store without
//! re-serializing a `RuntimeConfig` (which would produce a different
//! canonical form and therefore a different id).
//!
//! Contract:
//!   * `put_raw_toml(id, body, meta)` writes the exact bytes under
//!     `<root>/<id>.toml` and records an index row. If `id` doesn't
//!     match the hash of `body`, it's rejected — otherwise the
//!     content-addressed invariant is silently broken.
//!   * Duplicate puts (same id + same body) are no-ops on the blob
//!     but still record a metadata row, same as `put`.

use astra_config::config_versions::{
    ConfigVersionStore, LocalFileStore, PutMetadata, VersionId,
};

#[test]
fn put_raw_toml_preserves_cloud_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalFileStore::new(dir.path().to_path_buf());
    // Cloud gave us this exact byte sequence; the pull path must NOT
    // reformat (a new toml::to_string_pretty round-trip can reorder
    // tables and change the hash).
    let body = "version = \"1.0\"\n\n[token_budget]\nmax_turn_input_tokens = 500000\n";
    let id = VersionId::from_toml_bytes(body.as_bytes());
    store
        .put_raw_toml(&id, body, PutMetadata::default())
        .expect("put_raw_toml ok");
    let roundtrip = store.get_toml(&id).expect("get ok").expect("present");
    assert_eq!(roundtrip, body, "pull path must preserve byte-identity");
}

#[test]
fn put_raw_toml_rejects_id_that_doesnt_match_body_hash() {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalFileStore::new(dir.path().to_path_buf());
    let body = "version = \"1.0\"\n";
    let bogus = VersionId::from_str_for_test("cfg_0000000000000000");
    let err = store
        .put_raw_toml(&bogus, body, PutMetadata::default())
        .expect_err("id/body mismatch must fail");
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("mismatch") || msg.contains("hash"),
        "error must call out the mismatch: {err}"
    );
}

#[test]
fn put_raw_toml_duplicate_is_a_noop_on_blob() {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalFileStore::new(dir.path().to_path_buf());
    let body = "version = \"1.0\"\n";
    let id = VersionId::from_toml_bytes(body.as_bytes());
    store
        .put_raw_toml(&id, body, PutMetadata::default())
        .unwrap();
    store
        .put_raw_toml(&id, body, PutMetadata::default())
        .unwrap();
    let files = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|s| s.to_str())
                .map(|s| s == "toml")
                .unwrap_or(false)
        })
        .count();
    assert_eq!(files, 1, "same body must not multiply blobs");
}
