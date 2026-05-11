//! Content-addressed version store for RuntimeConfig.
//!
//! This is the "one config → one version" foundation: every distinct
//! RuntimeConfig the user ever runs lands in the store under a stable id
//! derived from its content hash. Sessions, checkpoints, and (later)
//! cloud mirrors reference the id, not the TOML blob.
//!
//! What this test file locks in before implementation:
//!
//! 1. **Identity is the hash**. Putting the same config twice yields
//!    the same id. Different content → different id. No clocks, no
//!    counters, no room for divergence between two machines that
//!    computed the same config independently.
//!
//! 2. **Put is idempotent**. Repeated puts of identical content don't
//!    multiply files on disk or rewrite the blob — the second put
//!    short-circuits and returns the existing id.
//!
//! 3. **The store persists to `~/.astra/config/versions/`** but must
//!    be redirectable to an arbitrary root, so tests don't clobber
//!    the user's real state and cloud mirrors can reuse the same type.
//!
//! 4. **Get by id returns the exact TOML bytes** that were put.
//!    Round-trip without reformatting — auditability demands "this is
//!    literally what the session saw".
//!
//! 5. **list() returns entries sorted newest-first** for the `astra
//!    config version list` CLI that will consume it.
//!
//! 6. **Metadata is carried alongside content**: creation timestamp,
//!    optional session id, optional parent id (the version this one
//!    was derived from, set when `/config` saves an edit). These live
//!    in an append-only index file so they survive rewrites of the
//!    blob directory.
//!
//! 7. **Missing id is a structured error**, not a panic.

use astra_config::config_versions::{ConfigVersionStore, LocalFileStore, PutMetadata, VersionId};
use astra_config::runtime_config::RuntimeConfig;

fn tmp_store() -> (tempfile::TempDir, LocalFileStore) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = LocalFileStore::new(dir.path().to_path_buf());
    (dir, store)
}

// ─── 1. Identity is the hash ────────────────────────────────────────────

#[test]
fn same_content_produces_same_id() {
    let (_dir, store) = tmp_store();
    let cfg = RuntimeConfig::default();
    let id_a = store.put(&cfg, PutMetadata::default()).expect("put ok");
    let id_b = store.put(&cfg, PutMetadata::default()).expect("put ok");
    assert_eq!(id_a, id_b, "identical content must hash to the same id");
}

#[test]
fn different_content_produces_different_id() {
    let (_dir, store) = tmp_store();
    let mut a = RuntimeConfig::default();
    let mut b = RuntimeConfig::default();
    b.token_budget.max_turn_input_tokens = a.token_budget.max_turn_input_tokens + 1;

    let id_a = store.put(&a, PutMetadata::default()).unwrap();
    let id_b = store.put(&b, PutMetadata::default()).unwrap();
    assert_ne!(id_a, id_b);
}

#[test]
fn version_id_starts_with_cfg_prefix() {
    // Human-readable discriminator so an id in logs/CLI is never
    // mistaken for a session id (`sess_...`) or skill id (`skill_...`).
    let (_dir, store) = tmp_store();
    let id = store
        .put(&RuntimeConfig::default(), PutMetadata::default())
        .unwrap();
    assert!(
        id.as_str().starts_with("cfg_"),
        "id must carry the `cfg_` prefix, got {}",
        id.as_str()
    );
}

#[test]
fn version_id_length_is_stable() {
    // Stable length (prefix + 16 hex chars) makes CLI columns align and
    // short-id collision probability sits around 2^-64.
    let (_dir, store) = tmp_store();
    let id = store
        .put(&RuntimeConfig::default(), PutMetadata::default())
        .unwrap();
    assert_eq!(id.as_str().len(), "cfg_".len() + 16);
}

// ─── 2. Put is idempotent on disk ──────────────────────────────────────

#[test]
fn duplicate_put_does_not_multiply_blobs() {
    let (dir, store) = tmp_store();
    let cfg = RuntimeConfig::default();
    store.put(&cfg, PutMetadata::default()).unwrap();
    store.put(&cfg, PutMetadata::default()).unwrap();
    store.put(&cfg, PutMetadata::default()).unwrap();

    let toml_count = std::fs::read_dir(dir.path())
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
    assert_eq!(toml_count, 1, "same content must not produce extra blobs");
}

// ─── 3. Store is redirectable ──────────────────────────────────────────

#[test]
fn custom_root_places_blobs_under_that_root() {
    let (dir, store) = tmp_store();
    store
        .put(&RuntimeConfig::default(), PutMetadata::default())
        .unwrap();
    let any_toml = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(Result::ok)
        .any(|e| {
            e.path()
                .extension()
                .and_then(|s| s.to_str())
                .map(|s| s == "toml")
                .unwrap_or(false)
        });
    assert!(any_toml, "blob must land under the custom root");
}

// ─── 4. Get returns exact content ──────────────────────────────────────

#[test]
fn get_by_id_returns_the_toml_bytes_that_were_put() {
    let (_dir, store) = tmp_store();
    let mut cfg = RuntimeConfig::default();
    cfg.token_budget.max_turn_input_tokens = 500_000;
    let id = store.put(&cfg, PutMetadata::default()).unwrap();
    let toml = store.get_toml(&id).expect("roundtrip").expect("found");
    assert!(
        toml.contains("max_turn_input_tokens"),
        "TOML must carry the field name: {toml}"
    );
    assert!(
        toml.contains("500000") || toml.contains("500_000"),
        "TOML must carry the edited value: {toml}"
    );
    // And reparses to the same config.
    let parsed: RuntimeConfig = toml::from_str(&toml).expect("reparse");
    assert_eq!(
        parsed.token_budget.max_turn_input_tokens,
        cfg.token_budget.max_turn_input_tokens
    );
}

#[test]
fn get_unknown_id_returns_none_not_error_not_panic() {
    let (_dir, store) = tmp_store();
    let unknown = VersionId::from_str_for_test("cfg_deadbeefdeadbeef");
    let got = store.get_toml(&unknown).expect("lookup must not fail");
    assert!(got.is_none(), "unknown id returns None");
}

// ─── 5. list() newest-first ────────────────────────────────────────────

#[test]
fn list_returns_versions_in_newest_first_order() {
    let (_dir, store) = tmp_store();
    let mut a = RuntimeConfig::default();
    let mut b = RuntimeConfig::default();
    let mut c = RuntimeConfig::default();
    a.token_budget.max_turn_input_tokens = 100_000;
    b.token_budget.max_turn_input_tokens = 200_000;
    c.token_budget.max_turn_input_tokens = 300_000;

    let id_a = store.put(&a, PutMetadata::default()).unwrap();
    // Sleep 2ms so timestamps truly differ; file mtime granularity on
    // some filesystems is 1ms.
    std::thread::sleep(std::time::Duration::from_millis(3));
    let id_b = store.put(&b, PutMetadata::default()).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(3));
    let id_c = store.put(&c, PutMetadata::default()).unwrap();

    let entries = store.list().expect("list ok");
    let ids: Vec<_> = entries.iter().map(|e| e.id.clone()).collect();
    assert_eq!(
        ids,
        vec![id_c, id_b, id_a],
        "list must order by created_at desc"
    );
}

// ─── 6. Metadata carried in index ──────────────────────────────────────

#[test]
fn put_records_session_id_and_parent_metadata() {
    let (_dir, store) = tmp_store();
    let cfg = RuntimeConfig::default();
    let mut meta = PutMetadata::default();
    meta.source_session = Some("sess_abc123".into());
    meta.parent = Some(VersionId::from_str_for_test("cfg_0000000000000000"));
    let id = store.put(&cfg, meta).unwrap();

    let entries = store.list().unwrap();
    let entry = entries
        .iter()
        .find(|e| e.id == id)
        .expect("put must land in the index");
    assert_eq!(entry.source_session.as_deref(), Some("sess_abc123"));
    assert_eq!(
        entry.parent.as_ref().map(|p| p.as_str()),
        Some("cfg_0000000000000000")
    );
    assert!(
        entry.created_at.is_some(),
        "every index row carries a timestamp"
    );
}

#[test]
fn duplicate_put_with_different_metadata_keeps_first_blob_records_second_index_row() {
    // Same content, different source session — the blob is one, but
    // the fact that two sessions arrived at the same config is
    // forensically interesting. Index carries both rows; blob isn't
    // rewritten.
    let (_dir, store) = tmp_store();
    let cfg = RuntimeConfig::default();
    let mut meta_a = PutMetadata::default();
    meta_a.source_session = Some("sess_A".into());
    let mut meta_b = PutMetadata::default();
    meta_b.source_session = Some("sess_B".into());
    let id_a = store.put(&cfg, meta_a).unwrap();
    let id_b = store.put(&cfg, meta_b).unwrap();
    assert_eq!(id_a, id_b);
    let entries = store.list().unwrap();
    let sessions: Vec<_> = entries
        .iter()
        .filter(|e| e.id == id_a)
        .filter_map(|e| e.source_session.clone())
        .collect();
    assert!(
        sessions.contains(&"sess_A".to_string()) && sessions.contains(&"sess_B".to_string()),
        "both index rows must be present, got {sessions:?}"
    );
}
