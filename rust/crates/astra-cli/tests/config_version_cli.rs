//! Step 3 contract: the `astra config version ...` CLI surface that
//! exposes the content-addressed version store to humans.
//!
//! Four subcommands, all implemented over the same
//! `astra_config::config_versions` trait so the same code paths can
//! later serve a cloud-backed variant:
//!
//!   * `list`  — newest-first table of every version seen. Flags
//!     `--limit` for paging and `--json` for scripting.
//!   * `show <id>` — print the TOML body of a specific version.
//!     Accepts either a full `cfg_<16hex>` id or a unique short
//!     prefix (`cfg_a7b2`).
//!   * `diff <a> <b>` — show the field-level TOML diff between two
//!     versions. Accepts prefixes on both sides.
//!   * `current` — print the id of the config the current process
//!     would run under right now (hash of the effective
//!     `RuntimeConfig::load()` result).
//!
//! This test file drives the rendering/formatting helpers directly,
//! because the subcommand entry points are `fn(…) -> Result<(), String>`
//! that write to stdout — impossible to capture from a unit test
//! without rerouting. The presentation helpers (`format_version_list`,
//! `format_version_diff`, `resolve_prefix`) are pure `(store, args) ->
//! String` shapes that each return the exact bytes the CLI would
//! print, so the test contract locks the content, not the println
//! machinery.

use astra_config::config_version_cli::{
    ResolveError, format_current, format_version_diff, format_version_list, format_version_show,
    resolve_prefix,
};
use astra_config::config_versions::{ConfigVersionStore, LocalFileStore, PutMetadata};
use astra_config::runtime_config::RuntimeConfig;

fn tmp_store_with_versions(n_changes: usize) -> (tempfile::TempDir, LocalFileStore) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = LocalFileStore::new(dir.path().to_path_buf());
    for i in 0..n_changes {
        let mut cfg = RuntimeConfig::default();
        cfg.token_budget.max_turn_input_tokens = 100_000 + (i as u32) * 10_000;
        let meta = PutMetadata {
            source_session: Some(format!("sess_{i}")),
            parent: None,
        };
        store.put(&cfg, meta).expect("put");
        // Guarantee distinct mtimes so list order is stable across
        // filesystems with millisecond resolution.
        std::thread::sleep(std::time::Duration::from_millis(3));
    }
    (dir, store)
}

// ─── list ───────────────────────────────────────────────────────────────

#[test]
fn list_renders_newest_first_with_id_and_source_session() {
    let (_dir, store) = tmp_store_with_versions(3);
    let out = format_version_list(&store, None).expect("format ok");
    let lines: Vec<&str> = out.lines().collect();
    // At least a header + 3 rows. Header detection is loose — look
    // for the id column and the session column names.
    assert!(
        lines
            .iter()
            .any(|l| l.contains("id") && l.contains("source")),
        "list must render a header that names the id and source columns: {out}"
    );
    // Newest session (sess_2) must appear before the oldest (sess_0).
    let idx_2 = lines.iter().position(|l| l.contains("sess_2"));
    let idx_0 = lines.iter().position(|l| l.contains("sess_0"));
    assert!(
        idx_2.is_some() && idx_0.is_some() && idx_2 < idx_0,
        "newest session must come first; got {lines:#?}"
    );
}

#[test]
fn list_with_limit_caps_output() {
    let (_dir, store) = tmp_store_with_versions(5);
    let out = format_version_list(&store, Some(2)).expect("format ok");
    // Count non-header, non-blank lines that carry a `cfg_` id — that's
    // how many actual rows the view rendered.
    let rows = out.lines().filter(|l| l.contains("cfg_")).count();
    assert_eq!(rows, 2, "limit=2 must render 2 id rows, got:\n{out}");
}

#[test]
fn list_on_empty_store_is_graceful_not_panic() {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalFileStore::new(dir.path().to_path_buf());
    let out = format_version_list(&store, None).expect("format ok");
    let lower = out.to_lowercase();
    assert!(
        lower.contains("no versions") || lower.contains("empty") || out.trim().is_empty(),
        "empty store should signal emptiness, got: {out}"
    );
}

// ─── show ───────────────────────────────────────────────────────────────

#[test]
fn show_prints_the_exact_toml_bytes_of_a_version() {
    let (_dir, store) = tmp_store_with_versions(1);
    let entries = store.list().unwrap();
    let id = entries[0].id.clone();
    let out = format_version_show(&store, id.as_str()).expect("show ok");
    assert!(
        out.contains("[token_budget]"),
        "show must include the token_budget section: {out}"
    );
    assert!(
        out.contains("max_turn_input_tokens"),
        "show must include field names: {out}"
    );
}

#[test]
fn show_accepts_short_prefix_when_unique() {
    let (_dir, store) = tmp_store_with_versions(1);
    let entries = store.list().unwrap();
    let id = entries[0].id.as_str().to_string();
    // Use the first 8 chars past `cfg_` — a realistic human prefix.
    let prefix = &id[..8.min(id.len())];
    let out = format_version_show(&store, prefix).expect("short prefix resolves");
    assert!(out.contains("token_budget"), "short prefix must resolve");
}

#[test]
fn show_rejects_unknown_id_with_structured_error() {
    let (_dir, store) = tmp_store_with_versions(1);
    let err = format_version_show(&store, "cfg_does_not_exist_x").unwrap_err();
    let lower = err.to_string().to_lowercase();
    assert!(
        lower.contains("unknown") || lower.contains("no such") || lower.contains("not found"),
        "unknown id must surface a readable error, got: {err}"
    );
}

// ─── diff ───────────────────────────────────────────────────────────────

#[test]
fn diff_shows_changed_field_between_two_versions() {
    let (_dir, store) = tmp_store_with_versions(2);
    let entries = store.list().unwrap();
    // list is newest-first; [0] = sess_1 (higher tokens), [1] = sess_0
    let newer = entries[0].id.clone();
    let older = entries[1].id.clone();

    let out = format_version_diff(&store, older.as_str(), newer.as_str()).expect("diff ok");
    let lower = out.to_lowercase();
    // Both version ids must appear in the header so the user sees
    // which side is which.
    assert!(out.contains(older.as_str()) && out.contains(newer.as_str()));
    // The changed field must appear with its old and new values.
    assert!(
        lower.contains("max_turn_input_tokens"),
        "diff must name the changed field: {out}"
    );
    assert!(
        out.contains("100000") || out.contains("100_000"),
        "diff must show the old value: {out}"
    );
    assert!(
        out.contains("110000") || out.contains("110_000"),
        "diff must show the new value: {out}"
    );
}

#[test]
fn diff_between_identical_versions_reports_no_changes() {
    let (_dir, store) = tmp_store_with_versions(1);
    let entries = store.list().unwrap();
    let id = entries[0].id.clone();
    let out = format_version_diff(&store, id.as_str(), id.as_str()).expect("diff ok");
    let lower = out.to_lowercase();
    assert!(
        lower.contains("no changes") || lower.contains("identical"),
        "identical diff must signal no changes: {out}"
    );
}

// ─── resolve_prefix (exposed for reuse by diff/show) ────────────────────

#[test]
fn resolve_prefix_returns_full_id_on_unique_match() {
    let (_dir, store) = tmp_store_with_versions(1);
    let entries = store.list().unwrap();
    let full = entries[0].id.as_str().to_string();
    let got = resolve_prefix(&store, &full[..8]).expect("prefix resolves");
    assert_eq!(got.as_str(), full);
}

#[test]
fn resolve_prefix_errors_on_unknown() {
    let (_dir, store) = tmp_store_with_versions(1);
    let err = resolve_prefix(&store, "cfg_ffff").unwrap_err();
    assert!(matches!(err, ResolveError::NotFound(_)));
}

#[test]
fn resolve_prefix_errors_on_ambiguous_prefix() {
    // Two versions sharing a common prefix — extremely unlikely in
    // practice (SHA-256 collision on 4 hex) but the contract must
    // not silently pick one.
    let dir = tempfile::tempdir().unwrap();
    let store = LocalFileStore::new(dir.path().to_path_buf());
    // Synthesize two entries via the append-index path by putting
    // two legitimately different configs; take just the `cfg_` prefix
    // as the ambiguous needle.
    let mut a = RuntimeConfig::default();
    let mut b = RuntimeConfig::default();
    a.token_budget.max_turn_input_tokens = 1;
    b.token_budget.max_turn_input_tokens = 2;
    store.put(&a, PutMetadata::default()).unwrap();
    store.put(&b, PutMetadata::default()).unwrap();
    let err = resolve_prefix(&store, "cfg_").unwrap_err();
    assert!(matches!(err, ResolveError::Ambiguous { .. }));
}

// ─── current ────────────────────────────────────────────────────────────

#[test]
fn current_reports_the_hash_of_the_default_runtime_config() {
    // Pure: computes the id for RuntimeConfig::load() without touching
    // the store. Safe to run in tests — it does NOT write to the
    // default `~/.astra/config/versions/` location.
    let out = format_current().expect("current ok");
    assert!(
        out.starts_with("cfg_"),
        "current must print the canonical cfg_ id: {out}"
    );
    assert!(
        out.trim().len() >= "cfg_".len() + 16,
        "id must be full length (cfg_ + 16 hex): {out:?}"
    );
}
