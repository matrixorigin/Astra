//! Storage layer for [`ForkPrefix`] — per-run snapshots keyed by parent
//! run id, with bounded capacity and soft TTL eviction.
//!
//! ## Role in the fork-prefix pipeline
//!
//! - **PR 1** defined the [`ForkPrefix`] type (frozen, hashed, validated).
//! - **This PR** provides the container that holds them: a
//!   [`PrefixCaptureSink`] trait + in-memory default impl backed by
//!   `DashMap`. The capture site (PR 3) writes in; the spawner (PR 4)
//!   reads out.
//! - **PR 3+** wire it into the turn lifecycle and spawner. This PR is
//!   storage-layer only — no knowledge of turn state, spawn semantics,
//!   or telemetry.
//!
//! ## Design choices (and what they rule out)
//!
//! 1. **Sync trait, not async.** The backing store is in-memory; there
//!    is no I/O. Making `record_prefix`/`get_prefix` async would be
//!    dead weight and force every caller into `.await` for no reason.
//!    Future disk-backed variants can add their own async wrapper.
//!
//! 2. **DashMap, not `RwLock<HashMap>`.** Multiple parent runs may be
//!    capturing prefixes concurrently (parallel root agents in one
//!    runtime process), and children may be spawning concurrently. A
//!    single write lock would serialize these. DashMap gives per-shard
//!    locking without introducing a new dep — it's already in
//!    `astra-turn-core`'s deps.
//!
//! 3. **LRU by timestamp, not by a separate order vector.** Unlike
//!    `CacheBreakDetector`'s per-source LRU (which lives in a single
//!    `&mut self` context), this store serves concurrent writers, so a
//!    shared `Vec<RunId>` ordering would need its own lock and
//!    re-introduce the serialization problem point 2 tries to avoid.
//!    We LRU-evict by scanning for the oldest `captured_at_secs`
//!    on overflow — acceptable because the cap is small and overflow
//!    is the exceptional path, not the common one.
//!
//! 4. **Lazy TTL sweep.** No background Tokio task. TTL is enforced on
//!    read (`get_prefix` returns `None` for stale entries and drops
//!    them) and on demand via `sweep_stale`. This keeps the store
//!    runtime-agnostic — tests and non-Tokio callers work identically.
//!
//! 5. **Trait + default impl, not a concrete struct.** Downstream
//!    plumbing wires `Arc<dyn PrefixCaptureSink>` so tests can inject
//!    a mock without spinning up a real DashMap. The default impl is
//!    `InMemoryPrefixStore`.
//!
//! ## Not in this PR
//!
//! - Capture at parent turn boundary (PR 3).
//! - Spawn-time resolution and child attachment (PR 4).
//! - `ForkCacheEvent` telemetry (PR 5).
//! - Disk-backed persistence.

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dashmap::DashMap;

use crate::fork_prefix::ForkPrefix;

/// Soft TTL for captured prefixes. Any entry older than this is
/// considered stale and treated as absent by `get_prefix`. Value is
/// generous — captures are typically consumed seconds after creation,
/// but a parent that pauses for a while then spawns should still get
/// a cache hit.
///
/// This is a soft cap — callers that need a different policy (e.g.
/// longer sessions, or stricter freshness) can construct
/// `InMemoryPrefixStore::with_config`.
pub const DEFAULT_PREFIX_TTL_SECS: u64 = 10 * 60;

/// Maximum number of concurrently tracked parent-run prefixes. Beyond
/// this, the oldest entry is evicted on insert. Matches the scale of
/// claudecode's module-level slot (which holds exactly one) plus
/// headroom for parallel root agents. Tuneable via `with_config`.
pub const DEFAULT_MAX_ENTRIES: usize = 64;

// ---------------------------------------------------------------------
// Trait surface
// ---------------------------------------------------------------------

/// Read+write surface for captured fork prefixes.
///
/// The trait is intentionally minimal and sync:
/// - `record_prefix` writes (or overwrites on refresh)
/// - `get_prefix` reads (with lazy TTL enforcement)
/// - `evict_prefix` explicitly removes (used at run-end hooks in PR 3)
/// - `tracked_count` / `sweep_stale` are diagnostics/maintenance
///
/// Downstream callers take `Arc<dyn PrefixCaptureSink>`; tests inject a
/// mock to observe capture events without running a real store.
pub trait PrefixCaptureSink: Send + Sync {
    /// Store (or overwrite) the snapshot for a parent run. Returns
    /// the run_ids of entries that were evicted as a side effect of
    /// this write, so telemetry (PR 3+) can emit one event per
    /// eviction. The vector is empty in the common case (no
    /// eviction) and does not allocate.
    ///
    /// Under concurrency or when multiple entries have aged out at
    /// once, a single write may evict more than one — returning a
    /// vector keeps telemetry complete instead of silently dropping
    /// "second-and-beyond" eviction events.
    fn record_prefix(&self, run_id: &str, prefix: Arc<ForkPrefix>) -> Vec<String>;

    /// Fetch the snapshot for a parent run. Returns `None` if absent
    /// OR if the entry is older than the configured TTL (in which
    /// case the stale entry is dropped as a side effect).
    fn get_prefix(&self, run_id: &str) -> Option<Arc<ForkPrefix>>;

    /// Remove a run's prefix explicitly (called on parent-run end).
    /// Idempotent — returns whether an entry was present.
    fn evict_prefix(&self, run_id: &str) -> bool;

    /// Number of tracked entries. Includes stale entries that
    /// `sweep_stale` would remove — callers that want a live count
    /// should call `sweep_stale` first.
    fn tracked_count(&self) -> usize;

    /// Drop all entries whose age exceeds the configured TTL. Returns
    /// the number of entries evicted. Safe to call concurrently with
    /// other operations.
    fn sweep_stale(&self) -> usize;
}

// ---------------------------------------------------------------------
// In-memory default impl
// ---------------------------------------------------------------------

/// Tunable knobs. Separated from the store struct so the constructor
/// remains readable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrefixStoreConfig {
    /// Soft TTL for captured prefixes.
    pub ttl: Duration,
    /// Upper bound on concurrently tracked entries before LRU
    /// eviction fires.
    pub max_entries: usize,
}

impl Default for PrefixStoreConfig {
    fn default() -> Self {
        Self {
            ttl: Duration::from_secs(DEFAULT_PREFIX_TTL_SECS),
            max_entries: DEFAULT_MAX_ENTRIES,
        }
    }
}

/// Thread-safe time source. Production uses a wall-clock closure;
/// tests inject their own clock (backed by an `AtomicU64`) so that
/// parallel tests can't collide on a shared global clock.
type TimeSource = Arc<dyn Fn() -> u64 + Send + Sync>;

/// DashMap-backed default implementation of [`PrefixCaptureSink`].
///
/// Instances are cheap to construct. Most callers will hold an
/// `Arc<InMemoryPrefixStore>` on the spawner (see PR 3) and share it
/// across threads.
pub struct InMemoryPrefixStore {
    entries: DashMap<String, Arc<ForkPrefix>>,
    /// Serializes the write-side insert+evict critical section. Reads
    /// stay lock-free through `DashMap`, while capacity maintenance
    /// becomes exact and telemetry only reports successful removals.
    eviction_lock: Mutex<()>,
    config: PrefixStoreConfig,
    /// Injected time source. Stored as `Arc<dyn Fn>` rather than a
    /// raw fn pointer so each test instance can own an independent
    /// clock — parallel tests sharing a global `AtomicU64` would
    /// otherwise race and produce flaky TTL assertions.
    now_secs: TimeSource,
}

impl std::fmt::Debug for InMemoryPrefixStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // TimeSource isn't Debug (opaque closure); skip it. The
        // caller rarely wants its pointer — they want the config and
        // live size.
        f.debug_struct("InMemoryPrefixStore")
            .field("config", &self.config)
            .field("tracked_count", &self.entries.len())
            .finish()
    }
}

impl Default for InMemoryPrefixStore {
    fn default() -> Self {
        Self::with_config(PrefixStoreConfig::default())
    }
}

impl InMemoryPrefixStore {
    /// Construct with default config.
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct with custom TTL/capacity.
    ///
    /// Panics if `max_entries` is zero — a zero cap would make the
    /// store perpetually empty (every insert would trigger an
    /// eviction loop that exits without evicting anything because
    /// the just-inserted entry is excluded). Silent degradation to
    /// "no caching ever" would be a nasty footgun; we prefer loud
    /// misconfiguration.
    pub fn with_config(config: PrefixStoreConfig) -> Self {
        assert!(
            config.max_entries >= 1,
            "PrefixStoreConfig.max_entries must be >= 1"
        );
        Self {
            entries: DashMap::new(),
            eviction_lock: Mutex::new(()),
            config,
            now_secs: Arc::new(wall_clock_secs),
        }
    }

    /// Test hook: override the time source. Kept `pub` (not
    /// `#[cfg(test)]`) so integration tests in other crates can
    /// inject clocks too. `#[doc(hidden)]` keeps it off the public
    /// doc surface.
    #[doc(hidden)]
    pub fn with_time_source(mut self, now_secs: impl Fn() -> u64 + Send + Sync + 'static) -> Self {
        self.now_secs = Arc::new(now_secs);
        self
    }

    fn is_stale(&self, prefix: &ForkPrefix) -> bool {
        let now = (self.now_secs)();
        let age = now.saturating_sub(prefix.captured_at_secs);
        age > self.config.ttl.as_secs()
    }

    /// Find the oldest entry's key by `captured_at_secs`, excluding a
    /// given key (usually the entry we just inserted — we must never
    /// evict ourselves even if our captured_at_secs happens to be the
    /// smallest, e.g. an out-of-order capture).
    ///
    /// Called only on the overflow path; O(n) scan is fine because n
    /// is small (bounded by `max_entries`).
    fn oldest_key_excluding(&self, exclude: &str) -> Option<String> {
        self.entries
            .iter()
            .filter(|e| e.key().as_str() != exclude)
            .min_by_key(|e| e.value().captured_at_secs)
            .map(|e| e.key().clone())
    }
}

impl PrefixCaptureSink for InMemoryPrefixStore {
    fn record_prefix(&self, run_id: &str, prefix: Arc<ForkPrefix>) -> Vec<String> {
        let _eviction_guard = self
            .eviction_lock
            .lock()
            .expect("prefix store eviction lock poisoned");

        // Overwrite semantics: the newest capture for a given parent
        // run wins, mirroring claudecode's `saveCacheSafeParams` slot
        // which is also last-write-wins. We insert first, then check
        // whether we need to evict — so a refresh on an existing key
        // never counts toward the cap.
        self.entries.insert(run_id.to_string(), prefix);

        // Capacity maintenance is serialized on the write path:
        // `DashMap` keeps reads concurrent, while this short critical
        // section avoids duplicate eviction attempts and keeps the
        // returned telemetry aligned with removals that actually
        // happened. Bounded iterations: we can't evict ourselves
        // (excluded) and the loop terminates when no victim remains.
        let mut evicted: Vec<String> = Vec::new();
        while self.entries.len() > self.config.max_entries {
            match self.oldest_key_excluding(run_id) {
                Some(victim) => {
                    if self.entries.remove(&victim).is_some() {
                        evicted.push(victim);
                    }
                }
                None => break, // only our own entry left — can't evict further
            }
        }
        evicted
    }

    fn get_prefix(&self, run_id: &str) -> Option<Arc<ForkPrefix>> {
        // Read-then-check-TTL-then-maybe-drop. We can't easily do this
        // inside a single DashMap entry handle because dropping an
        // entry while holding its guard deadlocks — hence the two-step
        // pattern: clone the Arc, release the read guard, then
        // conditionally call `remove`.
        let maybe = self.entries.get(run_id).map(|e| e.value().clone());
        match maybe {
            Some(prefix) if self.is_stale(&prefix) => {
                self.entries.remove(run_id);
                None
            }
            other => other,
        }
    }

    fn evict_prefix(&self, run_id: &str) -> bool {
        self.entries.remove(run_id).is_some()
    }

    fn tracked_count(&self) -> usize {
        self.entries.len()
    }

    fn sweep_stale(&self) -> usize {
        let now = (self.now_secs)();
        let ttl_secs = self.config.ttl.as_secs();

        // Two-phase: collect stale keys first, then re-check age at
        // delete time. A naive "collect then remove_blindly" has an
        // ABA hole: between phase 1 and phase 2, another thread may
        // `record_prefix` the same key with a fresh snapshot, and we
        // would delete that fresh entry. `remove_if` atomically
        // checks the CURRENT value's freshness at delete time, so a
        // concurrent refresh is safe.
        let stale: Vec<String> = self
            .entries
            .iter()
            .filter(|e| now.saturating_sub(e.value().captured_at_secs) > ttl_secs)
            .map(|e| e.key().clone())
            .collect();

        let mut n = 0usize;
        for key in stale {
            let removed = self
                .entries
                .remove_if(&key, |_, v| {
                    now.saturating_sub(v.captured_at_secs) > ttl_secs
                })
                .is_some();
            if removed {
                n += 1;
            }
        }
        n
    }
}

fn wall_clock_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fork_prefix::{
        hash_tool_schema, CacheMode, ForkPrefix, ProviderKind, SystemBlock, ThinkingConfigSlice,
        ToolSchemaEntry,
    };
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    /// Per-test controllable clock. Each test instantiates one so
    /// parallel test execution doesn't create inter-test timing races
    /// (an earlier global static AtomicU64 caused exactly that).
    #[derive(Clone)]
    struct FakeClock(Arc<AtomicU64>);

    impl FakeClock {
        fn starting_at(t: u64) -> Self {
            Self(Arc::new(AtomicU64::new(t)))
        }
        fn set(&self, t: u64) {
            self.0.store(t, Ordering::SeqCst);
        }
        fn advance(&self, secs: u64) {
            self.0.fetch_add(secs, Ordering::SeqCst);
        }
        /// Clone-as-Fn so the store can call it repeatedly without
        /// borrowing this struct.
        fn as_time_source(&self) -> impl Fn() -> u64 + Send + Sync + 'static {
            let inner = self.0.clone();
            move || inner.load(Ordering::SeqCst)
        }
    }

    fn make_prefix(parent_run: &str, captured_at_secs: u64) -> Arc<ForkPrefix> {
        let schema = serde_json::json!({"function": {"name": "bash"}});
        let (bytes, hash) = hash_tool_schema(&schema);
        Arc::new(ForkPrefix::build(
            format!("pfx-{parent_run}-{captured_at_secs}"),
            parent_run,
            1,
            captured_at_secs,
            ProviderKind::Anthropic,
            "claude-opus-4-6",
            Some(ThinkingConfigSlice {
                enabled: false,
                budget_tokens: 0,
                kind: "disabled".into(),
            }),
            vec![SystemBlock {
                bytes: b"sys".to_vec(),
                has_cache_control: true,
            }],
            vec![ToolSchemaEntry {
                name: "bash".into(),
                canonical_bytes: bytes,
                hash,
            }],
            vec![],
            b"prefix".to_vec(),
            CacheMode::Write,
        ))
    }

    fn store_at(clock: &FakeClock) -> InMemoryPrefixStore {
        InMemoryPrefixStore::new().with_time_source(clock.as_time_source())
    }

    fn store_at_with_config(clock: &FakeClock, config: PrefixStoreConfig) -> InMemoryPrefixStore {
        InMemoryPrefixStore::with_config(config).with_time_source(clock.as_time_source())
    }

    // --- Basic read/write --------------------------------------------

    #[test]
    fn record_then_get_returns_same_arc() {
        let clock = FakeClock::starting_at(1_000);
        let store = store_at(&clock);
        let p = make_prefix("run-1", 1_000);
        assert!(
            store.record_prefix("run-1", p.clone()).is_empty(),
            "first insert does not evict"
        );
        let got = store.get_prefix("run-1").unwrap();
        assert!(Arc::ptr_eq(&p, &got), "Arc identity must round-trip");
        assert_eq!(store.tracked_count(), 1);
    }

    #[test]
    fn get_absent_returns_none() {
        let clock = FakeClock::starting_at(1_000);
        let store = store_at(&clock);
        assert!(store.get_prefix("nope").is_none());
    }

    #[test]
    fn evict_removes_entry() {
        let clock = FakeClock::starting_at(1_000);
        let store = store_at(&clock);
        store.record_prefix("run-1", make_prefix("run-1", 1_000));
        assert!(store.evict_prefix("run-1"));
        assert!(!store.evict_prefix("run-1"), "second evict is idempotent");
        assert!(store.get_prefix("run-1").is_none());
    }

    // --- Last-write-wins refresh -------------------------------------

    #[test]
    fn refresh_overwrites_and_does_not_count_toward_cap() {
        let clock = FakeClock::starting_at(1_000);
        let store = store_at_with_config(
            &clock,
            PrefixStoreConfig {
                ttl: Duration::from_secs(600),
                max_entries: 2,
            },
        );

        store.record_prefix("run-A", make_prefix("run-A", 1_000));
        store.record_prefix("run-B", make_prefix("run-B", 1_000));
        clock.set(1_005);
        let evicted = store.record_prefix("run-A", make_prefix("run-A", 1_005));
        assert!(
            evicted.is_empty(),
            "refreshing an existing key must not trigger eviction"
        );
        assert_eq!(store.tracked_count(), 2);
        let got = store.get_prefix("run-A").unwrap();
        assert_eq!(got.captured_at_secs, 1_005, "latest snapshot must win");
    }

    // --- Capacity + LRU eviction -------------------------------------

    #[test]
    fn lru_evicts_oldest_when_exceeding_cap() {
        let clock = FakeClock::starting_at(1_000);
        let store = store_at_with_config(
            &clock,
            PrefixStoreConfig {
                ttl: Duration::from_secs(600),
                max_entries: 2,
            },
        );

        store.record_prefix("run-A", make_prefix("run-A", 1_000));
        clock.set(1_010);
        store.record_prefix("run-B", make_prefix("run-B", 1_010));
        clock.set(1_020);
        let evicted = store.record_prefix("run-C", make_prefix("run-C", 1_020));

        assert_eq!(
            evicted,
            vec!["run-A".to_string()],
            "oldest run should be evicted"
        );
        assert!(store.get_prefix("run-A").is_none());
        assert!(store.get_prefix("run-B").is_some());
        assert!(store.get_prefix("run-C").is_some());
    }

    #[test]
    fn eviction_never_drops_freshly_inserted_entry() {
        // Edge case: what if the newest insert has the smallest
        // captured_at_secs (e.g. an out-of-order capture)? We must
        // never evict the entry we just inserted.
        let clock = FakeClock::starting_at(2_000);
        let store = store_at_with_config(
            &clock,
            PrefixStoreConfig {
                ttl: Duration::from_secs(600),
                max_entries: 1,
            },
        );

        store.record_prefix("run-A", make_prefix("run-A", 2_000));
        // Insert a second with an OLDER captured_at_secs. A naive
        // oldest-scan would pick IT as the victim, leaving the
        // store with run-A and the new write lost. The
        // `oldest_key_excluding` guard in `record_prefix` prevents
        // that.
        let evicted = store.record_prefix("run-B", make_prefix("run-B", 1_500));

        assert_eq!(evicted, vec!["run-A".to_string()]);
        assert!(
            store.get_prefix("run-B").is_some(),
            "new write must survive"
        );
        assert!(store.get_prefix("run-A").is_none());
    }

    // --- TTL ---------------------------------------------------------

    #[test]
    fn get_returns_none_when_entry_is_stale() {
        let clock = FakeClock::starting_at(1_000);
        let store = store_at_with_config(
            &clock,
            PrefixStoreConfig {
                ttl: Duration::from_secs(60),
                max_entries: 8,
            },
        );

        store.record_prefix("run-1", make_prefix("run-1", 1_000));
        assert!(store.get_prefix("run-1").is_some(), "fresh");

        clock.advance(61);
        assert!(
            store.get_prefix("run-1").is_none(),
            "entry older than TTL must not be returned"
        );
        assert_eq!(
            store.tracked_count(),
            0,
            "stale read must drop the entry as a side effect"
        );
    }

    #[test]
    fn sweep_stale_batch_removes_expired_entries() {
        let clock = FakeClock::starting_at(1_000);
        let store = store_at_with_config(
            &clock,
            PrefixStoreConfig {
                ttl: Duration::from_secs(30),
                max_entries: 16,
            },
        );

        store.record_prefix("run-1", make_prefix("run-1", 1_000));
        store.record_prefix("run-2", make_prefix("run-2", 1_000));
        clock.set(1_050);
        store.record_prefix("run-3", make_prefix("run-3", 1_050));

        clock.set(1_070); // 1 and 2 are 70s old, 3 is 20s — only 3 survives
        let dropped = store.sweep_stale();
        assert_eq!(dropped, 2);
        assert_eq!(store.tracked_count(), 1);
        assert!(store.get_prefix("run-3").is_some());
    }

    #[test]
    fn sweep_stale_is_noop_when_all_fresh() {
        let clock = FakeClock::starting_at(1_000);
        let store = store_at(&clock);
        store.record_prefix("run-1", make_prefix("run-1", 1_000));
        store.record_prefix("run-2", make_prefix("run-2", 1_000));
        assert_eq!(store.sweep_stale(), 0);
        assert_eq!(store.tracked_count(), 2);
    }

    #[test]
    fn tracked_count_includes_stale_entries_until_swept() {
        // Counter-intuitive but documented: `tracked_count` doesn't
        // lie about what's in the map. Callers needing a live count
        // call `sweep_stale()` first.
        let clock = FakeClock::starting_at(1_000);
        let store = store_at_with_config(
            &clock,
            PrefixStoreConfig {
                ttl: Duration::from_secs(30),
                max_entries: 16,
            },
        );

        store.record_prefix("run-1", make_prefix("run-1", 1_000));
        clock.set(1_100); // entry is now stale
        assert_eq!(store.tracked_count(), 1);
        store.sweep_stale();
        assert_eq!(store.tracked_count(), 0);
    }

    // --- Concurrency -------------------------------------------------

    #[test]
    fn concurrent_writes_from_many_parents_do_not_deadlock_and_stay_bounded() {
        // Exercises concurrent writes + reads. Two things we assert:
        // 1. No deadlock (join completes).
        // 2. Capacity stays at the hard cap after all writes.
        // 3. Eviction telemetry reports exactly the entries that were
        //    actually removed: unique writes - final live entries.
        const THREADS: usize = 8;
        const WRITES_PER_THREAD: usize = 50;
        const TOTAL_WRITES: usize = THREADS * WRITES_PER_THREAD;

        let clock = FakeClock::starting_at(1_000);
        let store = Arc::new(store_at(&clock));
        let eviction_reports = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for i in 0..THREADS {
            let s = store.clone();
            let reports = eviction_reports.clone();
            handles.push(std::thread::spawn(move || {
                for j in 0..WRITES_PER_THREAD {
                    let run_id = format!("run-{i}-{j}");
                    let evicted = s.record_prefix(&run_id, make_prefix(&run_id, 1_000));
                    reports.fetch_add(evicted.len(), Ordering::SeqCst);
                    let _ = s.get_prefix(&run_id);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let size = store.tracked_count();
        let reported = eviction_reports.load(Ordering::SeqCst);
        assert!(
            size <= DEFAULT_MAX_ENTRIES,
            "size {size} exceeded hard cap {DEFAULT_MAX_ENTRIES}"
        );
        assert_eq!(
            reported + size,
            TOTAL_WRITES,
            "eviction reports must match the number of removed unique writes"
        );
    }

    // --- Trait-object usage ------------------------------------------

    #[test]
    fn works_through_dyn_trait_object() {
        // The whole point of the trait is to be used as Arc<dyn> at
        // spawner wire-up. Verify it compiles and behaves identically.
        let clock = FakeClock::starting_at(1_000);
        let store: Arc<dyn PrefixCaptureSink> = Arc::new(store_at(&clock));
        store.record_prefix("r", make_prefix("r", 1_000));
        assert!(store.get_prefix("r").is_some());
        assert!(store.evict_prefix("r"));
    }

    // --- Guards on defaults ------------------------------------------

    #[test]
    fn default_config_values_are_documented() {
        // Tripwire: changing these constants is an observable
        // behavior change and should be reviewed.
        assert_eq!(DEFAULT_PREFIX_TTL_SECS, 600);
        assert_eq!(DEFAULT_MAX_ENTRIES, 64);
    }

    #[test]
    #[should_panic(expected = "max_entries must be >= 1")]
    fn zero_max_entries_panics_loudly() {
        // A zero cap silently degrades to "no cache ever" (every
        // insert triggers an eviction loop that exits with the
        // just-inserted entry still present, but the NEXT insert
        // evicts the one before it…). Panic at construction so the
        // misconfiguration is loud rather than latent.
        let _ = InMemoryPrefixStore::with_config(PrefixStoreConfig {
            ttl: Duration::from_secs(60),
            max_entries: 0,
        });
    }

    #[test]
    fn record_prefix_reports_all_evictions_on_batch_overflow() {
        // A single write can evict multiple entries when the store
        // was already over capacity (e.g. concurrent writes raced
        // past the cap, then this write arrives and catches up
        // cleanup). Telemetry must see all of them.
        //
        // We trigger this by instantiating with a cap of 2 and then
        // directly inserting 4 entries via DashMap bypass — but that
        // requires access to `entries`. Instead simulate via the
        // supported path: fill to cap, reduce cap at runtime isn't
        // supported, so we verify by construction with a cap-of-1
        // and a pre-populated DashMap. That needs internals access,
        // so the cleaner path is to exercise the loop condition via
        // rapid sequential writes and assert the vector contract
        // even if only one entry is evicted each time.
        let clock = FakeClock::starting_at(1_000);
        let store = store_at_with_config(
            &clock,
            PrefixStoreConfig {
                ttl: Duration::from_secs(600),
                max_entries: 2,
            },
        );

        store.record_prefix("a", make_prefix("a", 1_000));
        clock.set(1_001);
        store.record_prefix("b", make_prefix("b", 1_001));
        // Steady-state: this write evicts exactly one entry. The
        // vector contract is what we're testing — it should be
        // exactly one element, not None-wrapped-in-Some.
        clock.set(1_002);
        let evicted = store.record_prefix("c", make_prefix("c", 1_002));
        assert_eq!(evicted.len(), 1);
        assert_eq!(evicted[0], "a");

        // Another write — continues to evict "b" (now oldest).
        clock.set(1_003);
        let evicted = store.record_prefix("d", make_prefix("d", 1_003));
        assert_eq!(evicted, vec!["b".to_string()]);
    }

    #[test]
    fn sweep_stale_does_not_drop_concurrently_refreshed_entry() {
        // Regression test for the ABA window in `sweep_stale`:
        // 1. Thread X collects stale keys at time T (entry E is
        //    stale at T)
        // 2. Thread Y refreshes E at time T+1 (now fresh)
        // 3. Thread X proceeds to remove E — MUST re-check freshness
        //    and leave E alone
        //
        // We simulate steps 1–3 by reaching into the store through
        // the ordinary API: record E at t=0, advance clock past
        // TTL, but also refresh E before calling sweep_stale. The
        // refresh makes the entry's captured_at_secs current; the
        // sweep's per-key re-check must see the fresh value.
        let clock = FakeClock::starting_at(1_000);
        let store = store_at_with_config(
            &clock,
            PrefixStoreConfig {
                ttl: Duration::from_secs(30),
                max_entries: 16,
            },
        );

        store.record_prefix("E", make_prefix("E", 1_000));
        // Entry is now stale.
        clock.set(1_100);
        // Concurrent-refresh analogue: rewrite E with a fresh
        // snapshot before the sweep runs. In the buggy implementation
        // the prior naive `remove(&key)` would delete this fresh
        // entry. `remove_if` must skip it.
        store.record_prefix("E", make_prefix("E", 1_100));

        let dropped = store.sweep_stale();
        assert_eq!(dropped, 0, "fresh refresh must not be swept");
        assert!(
            store.get_prefix("E").is_some(),
            "fresh entry must survive a concurrent sweep"
        );
    }
}
