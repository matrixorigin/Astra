//! Tool-result deduplication cache (gap #4).
//!
//! For repeated, side-effect-free tool calls (e.g. `read_file`, `grep`,
//! `git_show` on the same arguments within a short window), returning a
//! previously-computed result avoids wasted latency and provider spend.
//!
//! This module is **pure** and **in-memory**. It defines a
//! [`CallSignature`] keyed by `(tool_name, input_hash, ctx_hash)` and a
//! simple LRU + TTL [`ResultCache`]. Downstream callers decide *which*
//! tools are eligible (pass a closure or guard before calling `record` /
//! `lookup`) — this crate does not encode a whitelist because
//! concurrency-safety metadata is the proper home for that (gap #3).
//!
//! ## Hashing
//!
//! `CallSignature::from_args` canonicalizes its JSON input (sorted keys)
//! before hashing so `{"a":1,"b":2}` and `{"b":2,"a":1}` collide
//! deterministically. A `ctx_hash` slot is reserved for callers that need
//! to invalidate based on outside context (current working directory, git
//! SHA, session id, ...). Pass `0` when not applicable.
//!
//! ## Cache discipline
//!
//! * Bounded capacity (`max_entries`) — oldest entry evicted on insert.
//! * Optional TTL — entries older than `ttl` are treated as misses and
//!   purged on lookup.
//! * Insertion refreshes position (MRU); lookup also refreshes.
//!
//! The cache is neither thread-safe nor shared across processes — wrap in
//! a `Mutex` / `RwLock` at the integration site.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

use serde_json::Value;

/// Signature identifying a tool call for dedup purposes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CallSignature {
    pub tool_name: String,
    pub input_hash: u64,
    pub ctx_hash: u64,
}

impl CallSignature {
    /// Build a signature from a tool name and its JSON arguments.
    ///
    /// `ctx_hash` defaults to `0`; pass [`Self::with_ctx_hash`] after
    /// constructing if a caller-supplied context hash applies.
    pub fn from_args(tool_name: &str, args: &Value) -> Self {
        let canon = canonicalize(args);
        let mut h = DefaultHasher::new();
        canon.hash(&mut h);
        Self {
            tool_name: tool_name.to_string(),
            input_hash: h.finish(),
            ctx_hash: 0,
        }
    }

    pub fn with_ctx_hash(mut self, ctx_hash: u64) -> Self {
        self.ctx_hash = ctx_hash;
        self
    }
}

/// A cached tool result and its insertion timestamp.
#[derive(Debug, Clone)]
struct CachedEntry {
    result: String,
    inserted: Instant,
}

/// Simple LRU + TTL result cache.
#[derive(Debug)]
pub struct ResultCache {
    entries: Vec<(CallSignature, CachedEntry)>,
    max_entries: usize,
    ttl: Option<Duration>,
}

impl ResultCache {
    /// Create a cache with the given bound and optional TTL. A TTL of
    /// `None` disables time-based expiry.
    pub fn new(max_entries: usize, ttl: Option<Duration>) -> Self {
        assert!(max_entries > 0, "max_entries must be >= 1");
        Self {
            entries: Vec::with_capacity(max_entries),
            max_entries,
            ttl,
        }
    }

    /// Number of entries currently in the cache (after lazy TTL purge is
    /// *not* applied — use [`Self::purge_expired`] for that side effect).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Insert or refresh an entry. If the key is already present, the
    /// stored result is replaced and moved to the MRU slot. If capacity
    /// would be exceeded, the LRU (front) entry is evicted first.
    pub fn record(&mut self, sig: CallSignature, result: String) {
        if let Some(pos) = self.entries.iter().position(|(k, _)| k == &sig) {
            self.entries.remove(pos);
        } else if self.entries.len() >= self.max_entries {
            self.entries.remove(0);
        }
        self.entries.push((
            sig,
            CachedEntry {
                result,
                inserted: Instant::now(),
            },
        ));
    }

    /// Fetch a cached result. A TTL miss evicts the stale entry and
    /// returns `None`. A hit moves the entry to the MRU slot.
    pub fn lookup(&mut self, sig: &CallSignature) -> Option<String> {
        let pos = self.entries.iter().position(|(k, _)| k == sig)?;
        let (key, entry) = self.entries.remove(pos);
        if let Some(ttl) = self.ttl {
            if entry.inserted.elapsed() > ttl {
                return None;
            }
        }
        let result = entry.result.clone();
        self.entries.push((
            key,
            CachedEntry {
                result: result.clone(),
                inserted: entry.inserted,
            },
        ));
        Some(result)
    }

    /// Drop all entries whose age exceeds TTL. No-op when TTL is `None`.
    pub fn purge_expired(&mut self) {
        let Some(ttl) = self.ttl else {
            return;
        };
        self.entries.retain(|(_, e)| e.inserted.elapsed() <= ttl);
    }

    /// Remove every entry whose tool name matches `tool_name`. Callers
    /// invalidate after a mutating operation that *could* affect cached
    /// read results (e.g. a `write_file` invalidates any prior
    /// `read_file` results — the caller-side policy decides the mapping).
    pub fn invalidate_tool(&mut self, tool_name: &str) {
        self.entries.retain(|(k, _)| k.tool_name != tool_name);
    }

    /// Clear the cache.
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

// ── Shared-cache convenience ──────────────────────────────────────────────
//
// Integration sites typically wrap a [`ResultCache`] in an `Arc<Mutex<...>>`
// so concurrent tool executors can share it. These free functions factor out
// the common "lookup-then-record" workflow so callers avoid hand-rolling
// lock dances. They are small and async-executor-agnostic.

use std::sync::Mutex;

/// Thread-safe alias callers can use to share one cache across tasks.
pub type SharedResultCache = std::sync::Arc<Mutex<ResultCache>>;

/// Construct a shared cache handle with the given capacity and optional TTL.
pub fn new_shared_cache(max_entries: usize, ttl: Option<Duration>) -> SharedResultCache {
    std::sync::Arc::new(Mutex::new(ResultCache::new(max_entries, ttl)))
}

/// Execute `f` only when `sig` is not already in `cache`. On cache hit,
/// returns the cached result directly. On miss, runs `f`, records the result,
/// and returns it.
///
/// Pass a closure that yields the fresh result as a `String` — callers
/// responsible for turning their tool invocation into that string (JSON
/// serialisation, `to_string`, etc).
///
/// Returns `(result, was_hit)` so callers can update metrics.
pub async fn lookup_or_compute<F, Fut>(
    cache: &SharedResultCache,
    sig: &CallSignature,
    f: F,
) -> (String, bool)
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = String>,
{
    {
        let mut guard = cache.lock().expect("result cache poisoned");
        if let Some(hit) = guard.lookup(sig) {
            return (hit, true);
        }
    }
    let fresh = f().await;
    {
        let mut guard = cache.lock().expect("result cache poisoned");
        guard.record(sig.clone(), fresh.clone());
    }
    (fresh, false)
}

/// Synchronous variant for callers that are already inside a blocking
/// context or can't await.
pub fn lookup_or_compute_sync<F>(
    cache: &SharedResultCache,
    sig: &CallSignature,
    f: F,
) -> (String, bool)
where
    F: FnOnce() -> String,
{
    {
        let mut guard = cache.lock().expect("result cache poisoned");
        if let Some(hit) = guard.lookup(sig) {
            return (hit, true);
        }
    }
    let fresh = f();
    {
        let mut guard = cache.lock().expect("result cache poisoned");
        guard.record(sig.clone(), fresh.clone());
    }
    (fresh, false)
}

/// Canonicalize a JSON value by sorting object keys recursively.
///
/// Returns a `String` (not a `Value`) because the caller only needs a
/// stable hashable representation.
fn canonicalize(v: &Value) -> String {
    match v {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys
                .into_iter()
                .map(|k| format!("{}:{}", k, canonicalize(&map[k])))
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        Value::Array(arr) => {
            let parts: Vec<String> = arr.iter().map(canonicalize).collect();
            format!("[{}]", parts.join(","))
        }
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::thread::sleep;

    #[test]
    fn signature_equality_stable_across_key_order() {
        let s1 = CallSignature::from_args("read_file", &json!({"a":1,"b":2}));
        let s2 = CallSignature::from_args("read_file", &json!({"b":2,"a":1}));
        assert_eq!(s1, s2);
    }

    #[test]
    fn signature_distinct_for_different_tool_names() {
        let args = json!({"path":"x"});
        let s1 = CallSignature::from_args("read_file", &args);
        let s2 = CallSignature::from_args("grep", &args);
        assert_ne!(s1, s2);
    }

    #[test]
    fn signature_distinct_for_different_inputs() {
        let s1 = CallSignature::from_args("read_file", &json!({"path":"a"}));
        let s2 = CallSignature::from_args("read_file", &json!({"path":"b"}));
        assert_ne!(s1, s2);
    }

    #[test]
    fn ctx_hash_alters_signature() {
        let a = CallSignature::from_args("read_file", &json!({"path":"a"}));
        let b = a.clone().with_ctx_hash(42);
        assert_ne!(a, b);
    }

    #[test]
    fn lookup_miss_on_empty_cache() {
        let mut cache = ResultCache::new(4, None);
        let sig = CallSignature::from_args("read_file", &json!({"path":"x"}));
        assert!(cache.lookup(&sig).is_none());
    }

    #[test]
    fn record_then_lookup_returns_same_result() {
        let mut cache = ResultCache::new(4, None);
        let sig = CallSignature::from_args("read_file", &json!({"path":"x"}));
        cache.record(sig.clone(), "contents".into());
        assert_eq!(cache.lookup(&sig), Some("contents".into()));
    }

    #[test]
    fn lru_eviction_when_capacity_exceeded() {
        let mut cache = ResultCache::new(2, None);
        let s1 = CallSignature::from_args("t", &json!({"i":1}));
        let s2 = CallSignature::from_args("t", &json!({"i":2}));
        let s3 = CallSignature::from_args("t", &json!({"i":3}));
        cache.record(s1.clone(), "r1".into());
        cache.record(s2.clone(), "r2".into());
        cache.record(s3.clone(), "r3".into());
        assert!(cache.lookup(&s1).is_none(), "oldest should be evicted");
        assert_eq!(cache.lookup(&s2), Some("r2".into()));
        assert_eq!(cache.lookup(&s3), Some("r3".into()));
    }

    #[test]
    fn lookup_refreshes_mru_position() {
        let mut cache = ResultCache::new(2, None);
        let s1 = CallSignature::from_args("t", &json!({"i":1}));
        let s2 = CallSignature::from_args("t", &json!({"i":2}));
        let s3 = CallSignature::from_args("t", &json!({"i":3}));
        cache.record(s1.clone(), "r1".into());
        cache.record(s2.clone(), "r2".into());
        // Touch s1 so s2 becomes LRU.
        let _ = cache.lookup(&s1);
        cache.record(s3.clone(), "r3".into());
        assert!(cache.lookup(&s2).is_none(), "s2 was LRU, should evict");
        assert_eq!(cache.lookup(&s1), Some("r1".into()));
        assert_eq!(cache.lookup(&s3), Some("r3".into()));
    }

    #[test]
    fn ttl_expires_entries() {
        let mut cache = ResultCache::new(4, Some(Duration::from_millis(20)));
        let sig = CallSignature::from_args("t", &json!({"i":1}));
        cache.record(sig.clone(), "r".into());
        assert_eq!(cache.lookup(&sig), Some("r".into()));
        sleep(Duration::from_millis(30));
        assert!(cache.lookup(&sig).is_none());
    }

    #[test]
    fn purge_expired_drops_stale_entries() {
        let mut cache = ResultCache::new(4, Some(Duration::from_millis(15)));
        let sig = CallSignature::from_args("t", &json!({"i":1}));
        cache.record(sig, "r".into());
        sleep(Duration::from_millis(25));
        cache.purge_expired();
        assert!(cache.is_empty());
    }

    #[test]
    fn invalidate_tool_drops_only_matching_entries() {
        let mut cache = ResultCache::new(4, None);
        let r1 = CallSignature::from_args("read_file", &json!({"path":"a"}));
        let r2 = CallSignature::from_args("read_file", &json!({"path":"b"}));
        let g = CallSignature::from_args("grep", &json!({"pattern":"x"}));
        cache.record(r1.clone(), "a".into());
        cache.record(r2.clone(), "b".into());
        cache.record(g.clone(), "gx".into());
        cache.invalidate_tool("read_file");
        assert!(cache.lookup(&r1).is_none());
        assert!(cache.lookup(&r2).is_none());
        assert_eq!(cache.lookup(&g), Some("gx".into()));
    }

    #[test]
    fn record_same_key_replaces_result() {
        let mut cache = ResultCache::new(4, None);
        let sig = CallSignature::from_args("t", &json!({"i":1}));
        cache.record(sig.clone(), "old".into());
        cache.record(sig.clone(), "new".into());
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.lookup(&sig), Some("new".into()));
    }

    #[test]
    fn clear_empties_cache() {
        let mut cache = ResultCache::new(4, None);
        let sig = CallSignature::from_args("t", &json!({"i":1}));
        cache.record(sig, "r".into());
        cache.clear();
        assert!(cache.is_empty());
    }

    #[test]
    fn canonicalize_handles_nested_structures() {
        let a = canonicalize(&json!({"outer":{"b":2,"a":1}}));
        let b = canonicalize(&json!({"outer":{"a":1,"b":2}}));
        assert_eq!(a, b);
    }

    #[test]
    fn canonicalize_preserves_array_order() {
        let a = canonicalize(&json!([1, 2, 3]));
        let b = canonicalize(&json!([3, 2, 1]));
        assert_ne!(a, b);
    }

    // ── Shared-cache helper tests ──

    #[tokio::test]
    async fn lookup_or_compute_miss_then_hit() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let cache = new_shared_cache(8, None);
        let sig = CallSignature::from_args("read_file", &json!({"path": "a"}));
        let calls = Arc::new(AtomicUsize::new(0));

        let calls_c = calls.clone();
        let (out1, hit1) = lookup_or_compute(&cache, &sig, move || {
            let c = calls_c.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                "content".to_string()
            }
        })
        .await;
        assert!(!hit1);
        assert_eq!(out1, "content");
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Second call: same signature → hit, closure must not run.
        let calls_c = calls.clone();
        let (out2, hit2) = lookup_or_compute(&cache, &sig, move || {
            let c = calls_c.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                "WRONG".to_string()
            }
        })
        .await;
        assert!(hit2);
        assert_eq!(out2, "content");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "closure ran on hit");
    }

    #[test]
    fn lookup_or_compute_sync_miss_then_hit() {
        let cache = new_shared_cache(8, None);
        let sig = CallSignature::from_args("grep", &json!({"q": "TODO"}));

        let mut ran = 0;
        let (out1, hit1) =
            lookup_or_compute_sync(&cache, &sig, || {
                ran += 1;
                "fresh".to_string()
            });
        assert!(!hit1);
        assert_eq!(out1, "fresh");
        assert_eq!(ran, 1);

        let (out2, hit2) =
            lookup_or_compute_sync(&cache, &sig, || {
                ran += 1;
                "SHOULD_NOT_RUN".to_string()
            });
        assert!(hit2);
        assert_eq!(out2, "fresh");
        assert_eq!(ran, 1, "closure ran on hit");
    }

    #[test]
    fn new_shared_cache_is_shareable_across_threads() {
        let cache = new_shared_cache(4, None);
        let c2 = cache.clone();
        let sig = CallSignature::from_args("t", &json!({}));
        let handle = std::thread::spawn(move || {
            let (out, _) = lookup_or_compute_sync(&c2, &sig, || "hello".to_string());
            out
        });
        let out = handle.join().unwrap();
        assert_eq!(out, "hello");
        // Parent sees the recorded entry.
        let sig2 = CallSignature::from_args("t", &json!({}));
        let (out2, hit) = lookup_or_compute_sync(&cache, &sig2, || "miss".into());
        assert!(hit);
        assert_eq!(out2, "hello");
    }
}
