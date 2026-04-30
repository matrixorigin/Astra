//! Capture-site helpers for the fork-prefix primitive.
//!
//! This module defines the **pure function** that a turn-loop caller
//! invokes at the right moment in the parent-turn lifecycle to
//! snapshot the cacheable prefix into a [`PrefixCaptureSink`]. The
//! module itself does NOT know about turn state, SSE streams, tokio,
//! or the runtime crate — it only knows how to validate a
//! `CaptureRequest`, build a `ForkPrefix`, and emit a structured
//! `ForkCaptureOutcome` for telemetry.
//!
//! ## Why this lives in turn-core, not runtime
//!
//! - Callers in `astra-runtime` and `astra-cli` will each have their
//!   own turn loops. Both need the same invariants (feature flag
//!   check, microcompact abort, cacheable-state check, deterministic
//!   prefix construction). Putting the helper here forces a single
//!   source of truth.
//! - The helper is testable in isolation with a mock sink — no need
//!   to spin up a runtime to assert capture-site behavior.
//!
//! ## Role in the fork-prefix pipeline
//!
//! - PR 1: [`ForkPrefix`] type
//! - PR 2: [`PrefixCaptureSink`] trait + in-memory impl
//! - **PR 3 (this)**: capture-site helper + feature flag + outcome
//!   enum. Still NOT wired into any live turn loop.
//! - PR 3.5: runtime/cli call the helper from their turn-end path
//!   (tiny 1–3-line change per caller).
//! - PR 4+: spawn-time resolution, reconstructor, telemetry.
//!
//! ## Feature flag
//!
//! The helper reads [`is_fork_inherit_prefix_enabled`] on every call.
//! When disabled (default), it returns
//! [`ForkCaptureOutcome::FeatureDisabled`] without touching the sink
//! or constructing a `ForkPrefix`. This lets the feature ship dark
//! and be flipped on via env var without code changes.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use serde::{Deserialize, Serialize};

use crate::fork_prefix::{
    CacheMode, ForkPrefix, ProviderKind, SystemBlock, ThinkingConfigSlice, ToolSchemaEntry,
};
use crate::fork_prefix_store::PrefixCaptureSink;

// ---------------------------------------------------------------------
// Feature flag
// ---------------------------------------------------------------------

/// Environment variable that gates the fork-inherit-prefix feature.
/// When set to `1`, `true`, `on`, or `yes` (case-insensitive) the
/// capture site writes to the sink; any other value (or unset) keeps
/// it dark.
pub const FORK_INHERIT_PREFIX_ENV: &str = "ASTRA_FORK_INHERIT_PREFIX";

/// Tri-state cache of the feature-flag evaluation.
/// 0 = unread, 1 = disabled, 2 = enabled. Single `AtomicU8` + CAS
/// avoids a `OnceLock<bool>` and keeps the hot path to a relaxed
/// load on steady state.
static FORK_FLAG_CACHE: AtomicU8 = AtomicU8::new(0);

/// Returns whether the fork-inherit-prefix feature is enabled. Reads
/// the env var on first call and caches the result for the rest of
/// the process lifetime — the flag is intended as a deploy-time
/// switch, not a mid-session toggle.
///
/// Tests that need to override the flag without setting env vars
/// should use [`set_fork_flag_for_tests`].
///
/// **Test-mode quirk**: under `#[cfg(test)]` the env-var read is
/// skipped entirely — only the explicit value set via
/// `set_fork_flag_for_tests` is honored; an unread cache (state 0)
/// is treated as disabled. This prevents leaked env vars in CI from
/// flipping the default for tests that forgot to install a
/// `FlagGuard`.
pub fn is_fork_inherit_prefix_enabled() -> bool {
    match FORK_FLAG_CACHE.load(Ordering::Relaxed) {
        1 => false,
        2 => true,
        _ => {
            // State 0 (unread).
            #[cfg(test)]
            {
                // Tests MUST set an explicit value via FlagGuard.
                // An unread cache falls through as disabled without
                // consulting the environment — CI env pollution is
                // a real hazard and making the default explicit
                // keeps test behavior deterministic regardless of
                // test ordering.
                false
            }
            #[cfg(not(test))]
            {
                let enabled = std::env::var(FORK_INHERIT_PREFIX_ENV)
                    .map(|v| {
                        matches!(
                            v.trim().to_ascii_lowercase().as_str(),
                            "1" | "true" | "on" | "yes"
                        )
                    })
                    .unwrap_or(false);
                FORK_FLAG_CACHE.store(if enabled { 2 } else { 1 }, Ordering::Relaxed);
                enabled
            }
        }
    }
}

/// Test-only: force the feature flag to a specific value and bypass
/// env lookup. The helper is `#[doc(hidden)]` — production callers
/// should use the env var, not this function. Returns the previous
/// cached value so tests can restore state.
#[doc(hidden)]
pub fn set_fork_flag_for_tests(enabled: bool) -> u8 {
    FORK_FLAG_CACHE.swap(if enabled { 2 } else { 1 }, Ordering::Relaxed)
}

/// Test-only: restore the raw u8 state (including the "unread" 0)
/// previously returned from `set_fork_flag_for_tests`. Needed by
/// test `FlagGuard::Drop` implementations that must roll back to a
/// value the bool-only setter can't express.
#[doc(hidden)]
pub fn restore_fork_flag_raw_for_tests(raw: u8) {
    FORK_FLAG_CACHE.store(raw, Ordering::Relaxed);
}

/// Test-only: shared mutex that every test touching the feature
/// flag must acquire. Lives here (at the definition site) so that
/// tests in OTHER modules in the same crate — e.g. `fork_resolve` —
/// share the same lock and don't race each other for flag state.
#[cfg(test)]
pub(crate) static FORK_FLAG_TEST_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

// ---------------------------------------------------------------------
// Capture request + outcome types
// ---------------------------------------------------------------------

/// Everything the capture helper needs to know about the parent
/// turn. The runtime caller builds this at the turn-end slot.
///
/// Intentionally a plain struct (no builder, no Option chains): the
/// caller KNOWS all of these fields at capture time — they're what
/// it just sent to the provider. If any field is unknown, the caller
/// should NOT attempt to capture (pass `None` for the sink or skip
/// the call entirely).
#[derive(Debug, Clone)]
pub struct CaptureRequest {
    pub parent_run_id: String,
    pub parent_turn_seq: u32,
    pub provider: ProviderKind,
    pub model_id: String,
    pub thinking: Option<ThinkingConfigSlice>,
    pub system_blocks: Vec<SystemBlock>,
    pub tool_schemas: Vec<ToolSchemaEntry>,
    pub beta_headers: Vec<String>,
    pub canonical_prefix_bytes: Vec<u8>,
    pub cache_mode: CacheMode,
    /// Wall-clock seconds at capture; caller supplies so tests can
    /// inject deterministic timestamps.
    ///
    /// **Must match the sink's time source**. The default sink uses
    /// `SystemTime::now()`, so production callers should pass
    /// `SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()`.
    /// Passing a logical/session time (e.g. "session started at
    /// 1_700_000_000") will cause the entry to appear immediately
    /// stale under the default 10-minute TTL and be swept on first
    /// read.
    pub captured_at_secs: u64,
    /// Whether a microcompact fired within this parent turn. When
    /// true, the capture is aborted — the snapshot would reflect
    /// bytes that no longer match the parent's actual final state.
    pub microcompact_fired_in_turn: bool,
}

/// Why a capture was not written to the sink.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SkipReason {
    /// A microcompact fired during the parent turn; the prefix bytes
    /// would be stale relative to the parent's post-compact state.
    /// Next clean turn will re-capture.
    MicrocompactMidTurn,
    /// The turn produced no cacheable state (empty system, no tools,
    /// no messages). Capturing would be a no-op — we'd hash zero
    /// bytes and store a ForkPrefix that can never match anything.
    NoCacheableState,
    /// The canonical prefix exceeds the hard upper limit. We don't
    /// even construct the ForkPrefix; an oversized prefix is a
    /// data-shape fault the caller should surface in telemetry.
    OversizedInput { actual: usize, cap: usize },
}

/// Structured result of a capture attempt. Callers are expected to
/// emit this as a telemetry event; the helper does not log on
/// behalf of the caller to keep the module runtime-agnostic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ForkCaptureOutcome {
    /// Successfully recorded. `prefix_id` matches the one inside the
    /// `ForkPrefix` written to the sink. `evicted` lists any entries
    /// the store dropped to make room.
    Captured {
        prefix_id: String,
        evicted: Vec<String>,
    },
    /// The feature flag was off; no work was done.
    FeatureDisabled,
    /// The request was well-formed but the capture was deliberately
    /// skipped. See [`SkipReason`].
    Skipped { reason: SkipReason },
}

/// Hard upper limit on canonical prefix bytes accepted by the
/// capture helper. The soft cap in `fork_prefix` (2 MiB) triggers
/// `Oversized` inside `validate_spawn` but still allows construction
/// — that's so a child-spawn can CHOOSE to reject. At capture time
/// we're upstream of any child, so we fail earlier and simpler: if
/// the prefix is oversized, don't even build it.
///
/// Defined as an independent literal (not aliased to
/// `PREFIX_SOFT_CAP_BYTES`) because the two constants encode
/// DIFFERENT policies even when they happen to share a value:
/// PR 1's soft cap is "construct-then-flag"; this is "reject at
/// the door". A tripwire test asserts they're equal today so that
/// any future divergence is an explicit decision.
pub const CAPTURE_BYTE_CAP: usize = 2 * 1024 * 1024;

/// Generate a unique prefix_id. Deterministic across a process
/// (counter-based) so log-correlation across multiple captures in
/// one run is trivial; not cryptographically random.
///
/// `parent_run_id` is embedded as-is. Callers are expected to pass
/// a string safe for logs and URLs (typically a UUID or ULID); no
/// sanitization happens here because the id is also used as a
/// DashMap key and sanitizing would break key equality.
fn next_prefix_id(parent_run_id: &str, turn_seq: u32) -> String {
    use std::sync::atomic::AtomicU64;
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("pfx-{parent_run_id}-{turn_seq}-{n:08x}")
}

// ---------------------------------------------------------------------
// The capture helper
// ---------------------------------------------------------------------

/// Attempt to capture the parent turn's prefix into `sink`. Pure
/// function — validates the request, constructs a `ForkPrefix`,
/// writes it through the sink, returns a structured outcome. Never
/// panics, never logs, never I/O.
///
/// Call this once at the post-response, pre-side-effect slot of the
/// parent turn loop. See the module docstring for why.
pub fn capture_parent_prefix(
    request: CaptureRequest,
    sink: &dyn PrefixCaptureSink,
) -> ForkCaptureOutcome {
    if !is_fork_inherit_prefix_enabled() {
        return ForkCaptureOutcome::FeatureDisabled;
    }

    if request.microcompact_fired_in_turn {
        return ForkCaptureOutcome::Skipped {
            reason: SkipReason::MicrocompactMidTurn,
        };
    }

    // A turn with no system text and no tools and no message bytes
    // is indistinguishable from a corrupted capture. Writing it
    // would poison the sink with a prefix that can never match.
    let has_state = !request.system_blocks.is_empty()
        || !request.tool_schemas.is_empty()
        || !request.canonical_prefix_bytes.is_empty();
    if !has_state {
        return ForkCaptureOutcome::Skipped {
            reason: SkipReason::NoCacheableState,
        };
    }

    if request.canonical_prefix_bytes.len() > CAPTURE_BYTE_CAP {
        return ForkCaptureOutcome::Skipped {
            reason: SkipReason::OversizedInput {
                actual: request.canonical_prefix_bytes.len(),
                cap: CAPTURE_BYTE_CAP,
            },
        };
    }

    let prefix_id = next_prefix_id(&request.parent_run_id, request.parent_turn_seq);
    let prefix = Arc::new(ForkPrefix::build(
        prefix_id.clone(),
        request.parent_run_id.clone(),
        request.parent_turn_seq,
        request.captured_at_secs,
        request.provider,
        request.model_id,
        request.thinking,
        request.system_blocks,
        request.tool_schemas,
        request.beta_headers,
        request.canonical_prefix_bytes,
        request.cache_mode,
    ));

    let evicted = sink.record_prefix(&request.parent_run_id, prefix);

    ForkCaptureOutcome::Captured { prefix_id, evicted }
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fork_prefix::hash_tool_schema;
    use crate::fork_prefix_store::InMemoryPrefixStore;

    // All feature-flag-touching tests (here and in fork_resolve)
    // share the crate-global FORK_FLAG_TEST_MUTEX so they don't
    // race each other for flag state.

    /// RAII guard: set flag to `enabled` for the test's duration,
    /// restore on drop. Using drop-restore (not just "set true then
    /// set false") is safer when a test panics — Rust still runs
    /// destructors.
    struct FlagGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        prev_raw: u8,
    }

    impl FlagGuard {
        fn set(enabled: bool) -> Self {
            let lock = FORK_FLAG_TEST_MUTEX
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let prev_raw = set_fork_flag_for_tests(enabled);
            Self {
                _lock: lock,
                prev_raw,
            }
        }
    }

    impl Drop for FlagGuard {
        fn drop(&mut self) {
            // Restore the prior state exactly — including the
            // "unread" state (0), which we mimic by storing 0
            // directly rather than calling `set_fork_flag_for_tests`.
            FORK_FLAG_CACHE.store(self.prev_raw, Ordering::Relaxed);
        }
    }

    /// Current wall-clock seconds since epoch. Capture tests use
    /// this for `captured_at_secs` so the default-configured store
    /// (10-minute TTL, wall-clock time source) treats the entry as
    /// fresh during `get_prefix` assertions.
    fn wall_now_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    fn sample_tool_entry(name: &str) -> ToolSchemaEntry {
        let schema = serde_json::json!({"function": {"name": name}});
        let (bytes, hash) = hash_tool_schema(&schema);
        ToolSchemaEntry {
            name: name.into(),
            canonical_bytes: bytes,
            hash,
        }
    }

    fn sample_request() -> CaptureRequest {
        CaptureRequest {
            parent_run_id: "run-parent".into(),
            parent_turn_seq: 7,
            provider: ProviderKind::Anthropic,
            model_id: "claude-opus-4-6".into(),
            thinking: None,
            system_blocks: vec![SystemBlock {
                bytes: b"sys".to_vec(),
                has_cache_control: true,
            }],
            tool_schemas: vec![sample_tool_entry("bash")],
            beta_headers: vec![],
            canonical_prefix_bytes: b"canonical bytes".to_vec(),
            cache_mode: CacheMode::Write,
            captured_at_secs: wall_now_secs(),
            microcompact_fired_in_turn: false,
        }
    }

    #[test]
    fn feature_disabled_returns_early() {
        let _g = FlagGuard::set(false);
        let sink = InMemoryPrefixStore::new();
        let outcome = capture_parent_prefix(sample_request(), &sink);
        assert_eq!(outcome, ForkCaptureOutcome::FeatureDisabled);
        assert_eq!(
            sink.tracked_count(),
            0,
            "disabled feature must not touch sink"
        );
    }

    #[test]
    fn captured_writes_to_sink_and_returns_prefix_id() {
        let _g = FlagGuard::set(true);
        let sink = InMemoryPrefixStore::new();
        let outcome = capture_parent_prefix(sample_request(), &sink);
        match outcome {
            ForkCaptureOutcome::Captured { prefix_id, evicted } => {
                assert!(prefix_id.contains("run-parent"));
                assert!(prefix_id.contains("-7-"));
                assert!(evicted.is_empty());
            }
            other => panic!("expected Captured, got {other:?}"),
        }
        assert_eq!(sink.tracked_count(), 1);
        assert!(sink.get_prefix("run-parent").is_some());
    }

    #[test]
    fn microcompact_mid_turn_skips_capture() {
        let _g = FlagGuard::set(true);
        let sink = InMemoryPrefixStore::new();
        let mut req = sample_request();
        req.microcompact_fired_in_turn = true;
        let outcome = capture_parent_prefix(req, &sink);
        assert_eq!(
            outcome,
            ForkCaptureOutcome::Skipped {
                reason: SkipReason::MicrocompactMidTurn
            }
        );
        assert_eq!(
            sink.tracked_count(),
            0,
            "skipped capture must not touch sink"
        );
    }

    #[test]
    fn empty_state_skips_capture() {
        let _g = FlagGuard::set(true);
        let sink = InMemoryPrefixStore::new();
        let mut req = sample_request();
        req.system_blocks.clear();
        req.tool_schemas.clear();
        req.canonical_prefix_bytes.clear();
        let outcome = capture_parent_prefix(req, &sink);
        assert_eq!(
            outcome,
            ForkCaptureOutcome::Skipped {
                reason: SkipReason::NoCacheableState
            }
        );
        assert_eq!(sink.tracked_count(), 0);
    }

    #[test]
    fn partial_state_still_captures() {
        // System-only turn (tools+bytes empty) — some providers
        // genuinely send this for system-heavy prompts. Must not be
        // misclassified as NoCacheableState.
        let _g = FlagGuard::set(true);
        let sink = InMemoryPrefixStore::new();
        let mut req = sample_request();
        req.tool_schemas.clear();
        req.canonical_prefix_bytes = b"only-system".to_vec();
        let outcome = capture_parent_prefix(req, &sink);
        assert!(
            matches!(outcome, ForkCaptureOutcome::Captured { .. }),
            "system-blocks-only turn must capture, got {outcome:?}"
        );
    }

    #[test]
    fn oversized_input_is_rejected_before_construction() {
        let _g = FlagGuard::set(true);
        let sink = InMemoryPrefixStore::new();
        let mut req = sample_request();
        req.canonical_prefix_bytes = vec![b'x'; CAPTURE_BYTE_CAP + 1];
        let outcome = capture_parent_prefix(req, &sink);
        match outcome {
            ForkCaptureOutcome::Skipped {
                reason: SkipReason::OversizedInput { actual, cap },
            } => {
                assert_eq!(actual, CAPTURE_BYTE_CAP + 1);
                assert_eq!(cap, CAPTURE_BYTE_CAP);
            }
            other => panic!("expected OversizedInput skip, got {other:?}"),
        }
        assert_eq!(
            sink.tracked_count(),
            0,
            "oversized must not construct ForkPrefix"
        );
    }

    #[test]
    fn capture_overwrites_prior_capture_for_same_parent() {
        let _g = FlagGuard::set(true);
        let sink = InMemoryPrefixStore::new();
        let mut req = sample_request();
        let _ = capture_parent_prefix(req.clone(), &sink);

        // Precondition: guard must still be in effect. If a future
        // refactor drops `_g` early or moves the second capture to a
        // helper that forgets the guard, this assert catches it
        // before the real behavior assertion below.
        assert!(
            is_fork_inherit_prefix_enabled(),
            "test harness must keep flag enabled across both captures"
        );

        // Second capture on same run — different turn_seq, different
        // bytes. Must overwrite, not create a new entry.
        req.parent_turn_seq = 8;
        req.canonical_prefix_bytes = b"newer bytes".to_vec();
        let outcome = capture_parent_prefix(req, &sink);
        assert!(matches!(outcome, ForkCaptureOutcome::Captured { .. }));
        assert_eq!(sink.tracked_count(), 1, "same run_id must be single slot");
        let got = sink.get_prefix("run-parent").unwrap();
        assert_eq!(got.parent_turn_seq, 8);
    }

    #[test]
    fn prefix_ids_are_unique_across_captures() {
        // The prefix_id must distinguish captures made at the same
        // (run_id, turn_seq) in the same process — it's used to
        // correlate telemetry events to specific captures.
        let _g = FlagGuard::set(true);
        let sink = InMemoryPrefixStore::new();
        let mut ids = std::collections::HashSet::new();
        for _ in 0..10 {
            let outcome = capture_parent_prefix(sample_request(), &sink);
            if let ForkCaptureOutcome::Captured { prefix_id, .. } = outcome {
                assert!(ids.insert(prefix_id), "prefix_id must be unique");
            } else {
                panic!("expected Captured");
            }
        }
    }

    #[test]
    fn evicted_list_propagates_from_sink() {
        use crate::fork_prefix_store::PrefixStoreConfig;
        use std::time::Duration;

        let _g = FlagGuard::set(true);
        let sink = InMemoryPrefixStore::with_config(PrefixStoreConfig {
            ttl: Duration::from_secs(600),
            max_entries: 1,
        });

        // First capture fills the cap.
        let mut req = sample_request();
        req.parent_run_id = "run-A".into();
        let _ = capture_parent_prefix(req, &sink);

        // Second capture under a different run_id triggers eviction.
        let mut req = sample_request();
        req.parent_run_id = "run-B".into();
        let outcome = capture_parent_prefix(req, &sink);
        match outcome {
            ForkCaptureOutcome::Captured { evicted, .. } => {
                assert_eq!(evicted, vec!["run-A".to_string()]);
            }
            other => panic!("expected Captured with eviction, got {other:?}"),
        }
    }

    #[test]
    fn flag_env_var_name_is_stable() {
        // Tripwire: changing this is an observable operational
        // change (deployment scripts reference it).
        assert_eq!(FORK_INHERIT_PREFIX_ENV, "ASTRA_FORK_INHERIT_PREFIX");
    }

    #[test]
    fn capture_byte_cap_tracks_prefix_soft_cap() {
        // The two caps encode different policies (capture-time hard
        // rejection vs spawn-time soft flag) but currently share a
        // value. If they ever diverge, it MUST be an explicit
        // decision — not a surprise from one side being tuned while
        // the other wasn't.
        assert_eq!(
            CAPTURE_BYTE_CAP,
            crate::fork_prefix::PREFIX_SOFT_CAP_BYTES,
            "CAPTURE_BYTE_CAP and PREFIX_SOFT_CAP_BYTES are allowed to diverge, \
             but updating this assertion should force reviewers to confirm intent"
        );
    }

    #[test]
    fn unread_flag_defaults_to_disabled_under_test() {
        // Regression guard for the #[cfg(test)] branch in
        // `is_fork_inherit_prefix_enabled`. If a future change drops
        // that branch, a leaked ASTRA_FORK_INHERIT_PREFIX=1 env var
        // in CI would silently flip tests that forgot to install a
        // FlagGuard. We verify the branch's behavior: state 0 MUST
        // return false.
        let _lock = FORK_FLAG_TEST_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let prev = FORK_FLAG_CACHE.swap(0, Ordering::Relaxed);
        let observed = is_fork_inherit_prefix_enabled();
        // Restore before any panic-able assert, so later tests see
        // a predictable state.
        FORK_FLAG_CACHE.store(prev, Ordering::Relaxed);
        assert!(
            !observed,
            "unread flag state must default to disabled in test builds"
        );
    }
}
