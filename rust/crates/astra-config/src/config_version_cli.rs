//! Presentation helpers for `astra config version ...` subcommands.
//!
//! Kept as pure `fn(&store, ...) -> Result<String, _>` so the CLI
//! dispatcher is a thin println wrapper and the contract is
//! test-friendly without capturing stdout.
//!
//! All four helpers (`list`, `show`, `diff`, `current`) share the
//! same prefix-resolution logic so typing `astra config version show
//! cfg_a7b2` works the same way everywhere.

use crate::config_versions::{ConfigVersionStore, IndexEntry, StoreError, VersionId};
use crate::runtime_config::RuntimeConfig;

// ─── Prefix resolution ─────────────────────────────────────────────────

/// Error surfaced by any helper that looks up a version by id.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("no such config version: `{0}` not found in the local store")]
    NotFound(String),
    #[error("ambiguous config-version prefix `{prefix}`, matches {count} versions")]
    Ambiguous { prefix: String, count: usize },
    #[error("store error: {0}")]
    Store(#[from] StoreError),
}

/// Return the unique full id that starts with `needle`, or an error
/// if zero or more than one matches. Exact matches are accepted too —
/// a full id is just a prefix that happens to equal itself.
///
/// Walks the index to collect distinct ids (the index records every
/// put, including dupes, so we dedup here before deciding
/// uniqueness).
pub fn resolve_prefix(
    store: &dyn ConfigVersionStore,
    needle: &str,
) -> Result<VersionId, ResolveError> {
    let entries = store.list()?;
    let mut uniq: Vec<VersionId> = Vec::new();
    for e in entries {
        if e.id.as_str().starts_with(needle) && !uniq.iter().any(|x| x == &e.id) {
            uniq.push(e.id);
        }
    }
    match uniq.len() {
        0 => Err(ResolveError::NotFound(needle.to_string())),
        1 => Ok(uniq.into_iter().next().unwrap()),
        n => Err(ResolveError::Ambiguous {
            prefix: needle.to_string(),
            count: n,
        }),
    }
}

// ─── list ──────────────────────────────────────────────────────────────

/// Render a newest-first table of every version in the index.
///
/// `limit = None` renders everything; `Some(n)` caps to n rows.
/// Columns: id · created_at (ISO 8601) · source session · parent.
/// The formatter picks fixed padding rather than a full table crate
/// dep — the row count is small and human-scan friendliness wins
/// over perfect alignment on edge cases.
pub fn format_version_list(
    store: &dyn ConfigVersionStore,
    limit: Option<usize>,
) -> Result<String, StoreError> {
    let entries = store.list()?;
    let take = limit.unwrap_or(entries.len());
    let rows: Vec<&IndexEntry> = entries.iter().take(take).collect();

    if rows.is_empty() {
        return Ok("(no versions recorded)".to_string());
    }

    let mut out = String::new();
    out.push_str(&format!(
        "{:<24}  {:<25}  {:<16}  {}\n",
        "id", "created_at", "source", "parent"
    ));
    out.push_str(&"─".repeat(80));
    out.push('\n');
    for e in rows {
        let created = e
            .created_at
            .map(|t| t.to_rfc3339())
            .unwrap_or_else(|| "-".to_string());
        let session = e.source_session.as_deref().unwrap_or("-");
        let parent = e.parent.as_ref().map(|p| p.as_str()).unwrap_or("-");
        out.push_str(&format!(
            "{:<24}  {:<25}  {:<16}  {}\n",
            e.id.as_str(),
            created,
            session,
            parent
        ));
    }
    Ok(out)
}

// ─── show ──────────────────────────────────────────────────────────────

/// Render the TOML bytes of the version referenced by `id_or_prefix`.
///
/// Returns the exact content that was put (no reformat) so an audit
/// can compare byte-for-byte with the blob on disk.
pub fn format_version_show(
    store: &dyn ConfigVersionStore,
    id_or_prefix: &str,
) -> Result<String, ResolveError> {
    let id = resolve_prefix(store, id_or_prefix)?;
    match store.get_toml(&id)? {
        Some(body) => Ok(body),
        None => Err(ResolveError::NotFound(id_or_prefix.to_string())),
    }
}

// ─── diff ──────────────────────────────────────────────────────────────

/// Render a field-level diff between two versions.
///
/// Format is a minimal `key = a → b` list — one line per changed
/// leaf field. The approach reuses `serde_json::Value` as the
/// lingua franca: parse both TOMLs into a generic Value tree,
/// walk them recursively, emit any leaf whose values disagree.
/// Additions and deletions (field in one side only) are rendered
/// with a `(none)` placeholder on the absent side.
pub fn format_version_diff(
    store: &dyn ConfigVersionStore,
    a: &str,
    b: &str,
) -> Result<String, ResolveError> {
    let id_a = resolve_prefix(store, a)?;
    let id_b = resolve_prefix(store, b)?;
    let toml_a = store
        .get_toml(&id_a)?
        .ok_or_else(|| ResolveError::NotFound(a.to_string()))?;
    let toml_b = store
        .get_toml(&id_b)?
        .ok_or_else(|| ResolveError::NotFound(b.to_string()))?;

    // TOML → JSON-shaped Value via toml's built-in de+ser cycle.
    let va: toml::Value = toml::from_str(&toml_a)
        .map_err(|e| ResolveError::Store(StoreError::CorruptIndex(format!("parse a: {e}"))))?;
    let vb: toml::Value = toml::from_str(&toml_b)
        .map_err(|e| ResolveError::Store(StoreError::CorruptIndex(format!("parse b: {e}"))))?;

    let mut changes: Vec<(String, Option<String>, Option<String>)> = Vec::new();
    walk_diff("", &va, &vb, &mut changes);

    let mut out = String::new();
    out.push_str(&format!("--- {}\n+++ {}\n", id_a, id_b));
    if changes.is_empty() {
        out.push_str("(no changes — identical content)\n");
        return Ok(out);
    }
    for (path, av, bv) in changes {
        let left = av.as_deref().unwrap_or("(none)");
        let right = bv.as_deref().unwrap_or("(none)");
        out.push_str(&format!("{path}: {left} → {right}\n"));
    }
    Ok(out)
}

/// Recurse two `toml::Value` trees in lockstep. Any leaf mismatch is
/// pushed as (dotted_path, a_str, b_str). Missing keys on either
/// side become (Some, None) or (None, Some).
fn walk_diff(
    path: &str,
    a: &toml::Value,
    b: &toml::Value,
    out: &mut Vec<(String, Option<String>, Option<String>)>,
) {
    match (a, b) {
        (toml::Value::Table(ta), toml::Value::Table(tb)) => {
            let mut keys: Vec<&String> = ta.keys().chain(tb.keys()).collect();
            keys.sort();
            keys.dedup();
            for k in keys {
                let sub = if path.is_empty() {
                    k.to_string()
                } else {
                    format!("{path}.{k}")
                };
                match (ta.get(k), tb.get(k)) {
                    (Some(av), Some(bv)) => walk_diff(&sub, av, bv, out),
                    (Some(av), None) => out.push((sub, Some(render_leaf(av)), None)),
                    (None, Some(bv)) => out.push((sub, None, Some(render_leaf(bv)))),
                    (None, None) => {}
                }
            }
        }
        (av, bv) => {
            if av != bv {
                out.push((
                    path.to_string(),
                    Some(render_leaf(av)),
                    Some(render_leaf(bv)),
                ));
            }
        }
    }
}

fn render_leaf(v: &toml::Value) -> String {
    match v {
        toml::Value::String(s) => s.clone(),
        toml::Value::Integer(i) => i.to_string(),
        toml::Value::Float(f) => f.to_string(),
        toml::Value::Boolean(b) => b.to_string(),
        // Arrays / tables at a "leaf" position get Debug-rendered;
        // rare for our config shape but we don't want to panic.
        other => format!("{other:?}"),
    }
}

// ─── current ───────────────────────────────────────────────────────────

/// Hash the process's effective `RuntimeConfig::load()` without
/// touching any store. Used by `astra config version current` so
/// scripts can ask "what would this session start with" without a
/// filesystem side effect.
pub fn format_current() -> Result<String, StoreError> {
    let cfg = RuntimeConfig::load();
    let toml_bytes = toml::to_string_pretty(&cfg)?;
    Ok(VersionId::from_toml_bytes(toml_bytes.as_bytes()).to_string())
}
