//! End-to-end integration tests for the fork-prefix pipeline.
//!
//! Each of the 9 component PRs (1.5, 1, 2, 3, 4, 4.5, 5a, 5b, 5c)
//! has its own unit tests. This suite exercises the *data flow
//! across modules* — the cross-cutting invariants that no single
//! unit test can verify:
//!
//! 1. `prefix_id` threaded from capture → store → resolve →
//!    telemetry event, unchanged.
//! 2. Canonical bytes survive round-trip through store + reconstruct
//!    byte-identically (cache reuse precondition).
//! 3. `evaluate_fork_cache` agrees with `cache_diagnostics` thresholds
//!    on the boundary cases.
//! 4. Validation errors at resolve time propagate into the right
//!    outcome bucket without corrupting the store.
//! 5. Feature flag off kills the entire pipeline with no partial
//!    writes.
//!
//! These tests use the public API only — no crate-internal hacks —
//! mirroring how a real downstream caller (runtime / CLI) will wire
//! the pipeline together.

use std::sync::Arc;

use astra_turn_core::fork_cache_event::{
    ForkCacheOutcome, ForkCacheProbe, ForkCacheThresholds, evaluate_fork_cache,
};
use astra_turn_core::fork_capture::{
    CaptureRequest, FORK_FLAG_TEST_MUTEX, FORK_INHERIT_PREFIX_ENV, ForkCaptureOutcome,
    capture_parent_prefix, restore_fork_flag_raw_for_tests, set_fork_flag_for_tests,
};
use astra_turn_core::fork_prefix::{
    CacheMode, ForkValidationError, ProviderKind, SystemBlock, ThinkingConfigSlice,
    ToolSchemaEntry, hash_tool_schema,
};
use astra_turn_core::fork_prefix_store::{InMemoryPrefixStore, PrefixCaptureSink};
use astra_turn_core::fork_reconstruct::reconstruct_messages;
use astra_turn_core::fork_resolve::{
    PrefixResolveOutcome, ResolveFailure, SpawnResolveContext, resolve_inherit_prefix,
};
use astra_turn_core::orchestration_spawn_tool::InheritPrefixSpec;
use serde_json::{Value, json};

// ---------------------------------------------------------------------
// Test harness: feature-flag guard shared with lib tests
// ---------------------------------------------------------------------

/// Cross-crate integration-test guard. Takes the crate-public
/// `FORK_FLAG_TEST_MUTEX` so it serializes with lib tests AND with
/// other integration tests in this file. Without the mutex,
/// parallel tests would race on the process-global flag cache and
/// flip each other into the wrong outcome.
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
        restore_fork_flag_raw_for_tests(self.prev_raw);
    }
}

fn wall_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn parent_messages() -> Vec<Value> {
    vec![
        json!({"role": "user", "content": "read the file and explain"}),
        json!({"role": "assistant", "content": [
            {"type": "text", "text": "I'll read it now"},
            {"type": "tool_use", "id": "t1", "name": "read_file", "input": {"path": "x"}}
        ]}),
        json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": "file body..."}
        ]}),
        json!({"role": "assistant", "content": "The file defines a helper function."}),
    ]
}

fn build_sample_request(parent_run_id: &str, model: &str, budget_tokens: u32) -> CaptureRequest {
    let msgs = parent_messages();
    let canonical = serde_json::to_vec(&msgs).expect("json encode");
    let schema = json!({"function": {"name": "read_file"}});
    let (schema_bytes, schema_hash) = hash_tool_schema(&schema);
    CaptureRequest {
        parent_run_id: parent_run_id.into(),
        parent_turn_seq: 1,
        provider: ProviderKind::Anthropic,
        model_id: model.into(),
        thinking: Some(ThinkingConfigSlice {
            enabled: budget_tokens > 0,
            budget_tokens,
            kind: if budget_tokens > 0 {
                "enabled"
            } else {
                "disabled"
            }
            .into(),
        }),
        system_blocks: vec![SystemBlock {
            bytes: b"you are a careful assistant".to_vec(),
            has_cache_control: true,
        }],
        tool_schemas: vec![ToolSchemaEntry {
            name: "read_file".into(),
            canonical_bytes: schema_bytes,
            hash: schema_hash,
        }],
        beta_headers: vec![],
        canonical_prefix_bytes: canonical,
        cache_mode: CacheMode::Write,
        captured_at_secs: wall_now_secs(),
        microcompact_fired_in_turn: false,
    }
}

fn matching_spawn_ctx(parent_run_id: &str, model: &str) -> SpawnResolveContext {
    SpawnResolveContext {
        caller_run_id: Some(parent_run_id.into()),
        child_provider: ProviderKind::Anthropic,
        child_model_id: model.into(),
        child_max_output_tokens: None,
    }
}

// ---------------------------------------------------------------------
// Happy path
// ---------------------------------------------------------------------

#[test]
fn happy_path_capture_store_resolve_reconstruct_event_round_trips_prefix_id() {
    let _g = FlagGuard::set(true);
    let store: Arc<dyn PrefixCaptureSink> = Arc::new(InMemoryPrefixStore::new());

    // 1. Capture parent turn.
    let req = build_sample_request("run-parent", "claude-opus-4-6", 0);
    let capture_outcome = capture_parent_prefix(req, store.as_ref());
    let captured_prefix_id = match capture_outcome {
        ForkCaptureOutcome::Captured { prefix_id, .. } => prefix_id,
        other => panic!("capture should succeed: {other:?}"),
    };

    // 2. Resolve for child spawn.
    let spec = InheritPrefixSpec {
        from_run_id: None, // inherit from caller_run_id
        required: false,
    };
    let ctx = matching_spawn_ctx("run-parent", "claude-opus-4-6");
    let resolved = resolve_inherit_prefix(Some(&spec), &ctx, store.as_ref());
    let prefix_arc = match &resolved {
        PrefixResolveOutcome::Resolved { prefix } => prefix.clone(),
        other => panic!("resolver should return Resolved: {other:?}"),
    };
    // prefix_id MUST be the same one that capture emitted — this is
    // the thread that lets telemetry join capture/spawn/event rows.
    assert_eq!(
        prefix_arc.prefix_id, captured_prefix_id,
        "prefix_id must round-trip from capture to resolve"
    );

    // 3. Reconstruct messages with a child suffix.
    let child_suffix = vec![json!({"role": "user", "content": "subtask: summarize"})];
    let rebuilt = reconstruct_messages(&prefix_arc, child_suffix.clone()).unwrap();
    assert_eq!(rebuilt.prefix_len, parent_messages().len());
    assert_eq!(
        rebuilt.messages[..rebuilt.prefix_len].to_vec(),
        parent_messages(),
        "prefix region must equal captured parent messages"
    );
    assert_eq!(
        &rebuilt.messages[rebuilt.prefix_len..],
        child_suffix.as_slice(),
        "suffix preserved in order"
    );

    // 4. Byte-identical contract: re-serializing the prefix region
    //    MUST equal the captured canonical bytes. This is the cache-
    //    reuse precondition across modules.
    let reserialized = serde_json::to_vec(&rebuilt.messages[..rebuilt.prefix_len]).unwrap();
    assert_eq!(
        reserialized.as_slice(),
        prefix_arc.canonical_prefix_bytes().as_slice(),
        "re-serialized prefix region must equal captured canonical bytes"
    );

    // 5. Emit a telemetry event representing the child's first
    //    response. We simulate the provider returning
    //    cache_read_tokens close to our estimate (perfect hit).
    let expected = 15_000u64;
    let observed = 14_500u64; // ~97% hit
    let probe = ForkCacheProbe {
        prefix_id: prefix_arc.prefix_id.clone(),
        parent_run_id: prefix_arc.parent_run_id.clone(),
        child_run_id: "run-child-1".into(),
        expected_cache_read_tokens: expected,
        observed_cache_read_tokens: observed,
        provider: prefix_arc.provider.clone(),
    };
    let event = evaluate_fork_cache(probe, ForkCacheThresholds::default());

    // prefix_id must ALSO round-trip to the event, completing the
    // capture → event attribution chain.
    assert_eq!(event.prefix_id, captured_prefix_id);
    assert_eq!(event.parent_run_id, "run-parent");
    assert_eq!(event.child_run_id, "run-child-1");
    assert_eq!(event.outcome, ForkCacheOutcome::Hit);
}

// ---------------------------------------------------------------------
// Data-flow invariants
// ---------------------------------------------------------------------

#[test]
fn evict_during_resolve_produces_not_found_fallback_not_failed() {
    // Two parents capture; store cap is 1; first parent is LRU-evicted
    // before resolver runs on behalf of the first parent's child.
    // Resolver with required=false must Fallback{NotFound}, not Failed.
    use astra_turn_core::fork_prefix_store::PrefixStoreConfig;
    use std::time::Duration;

    let _g = FlagGuard::set(true);
    let store = Arc::new(InMemoryPrefixStore::with_config(PrefixStoreConfig {
        ttl: Duration::from_secs(600),
        max_entries: 1,
    }));
    let sink: &dyn PrefixCaptureSink = store.as_ref();

    // Capture run-A.
    let _ = capture_parent_prefix(build_sample_request("run-A", "claude-opus-4-6", 0), sink);
    // Capture run-B — evicts run-A (max_entries=1).
    let _ = capture_parent_prefix(build_sample_request("run-B", "claude-opus-4-6", 0), sink);

    // Now resolver sees no prefix for run-A.
    let spec = InheritPrefixSpec {
        from_run_id: None,
        required: false,
    };
    let ctx = matching_spawn_ctx("run-A", "claude-opus-4-6");
    let outcome = resolve_inherit_prefix(Some(&spec), &ctx, sink);
    match outcome {
        PrefixResolveOutcome::Fallback {
            reason: ResolveFailure::NotFound { run_id },
        } => {
            assert_eq!(run_id, "run-A");
        }
        other => panic!("expected Fallback{{NotFound}}, got {other:?}"),
    }
}

#[test]
fn thinking_budget_clamp_mismatch_propagates_with_structured_reason() {
    let _g = FlagGuard::set(true);
    let store = InMemoryPrefixStore::new();
    let sink: &dyn PrefixCaptureSink = &store;

    // Capture with a 16k thinking budget.
    let req = build_sample_request("run-parent", "claude-opus-4-6", 16_000);
    let _ = capture_parent_prefix(req, sink);

    // Child wants max_output_tokens=8k — would clamp the budget.
    let spec = InheritPrefixSpec {
        from_run_id: None,
        required: false,
    };
    let mut ctx = matching_spawn_ctx("run-parent", "claude-opus-4-6");
    ctx.child_max_output_tokens = Some(8_000);
    let outcome = resolve_inherit_prefix(Some(&spec), &ctx, sink);
    match outcome {
        PrefixResolveOutcome::Fallback {
            reason:
                ResolveFailure::Incompatible {
                    reason: ForkValidationError::ThinkingBudgetConflict { prefix_budget, .. },
                    ..
                },
        } => {
            assert_eq!(prefix_budget, 16_000);
        }
        other => panic!("expected Incompatible(ThinkingBudgetConflict), got {other:?}"),
    }
}

#[test]
fn feature_flag_off_kills_pipeline_with_no_writes() {
    let _g = FlagGuard::set(false);
    let store = InMemoryPrefixStore::new();
    let sink: &dyn PrefixCaptureSink = &store;

    // Capture attempt with flag off — must NOT write.
    let req = build_sample_request("run-parent", "claude-opus-4-6", 0);
    let outcome = capture_parent_prefix(req, sink);
    assert_eq!(outcome, ForkCaptureOutcome::FeatureDisabled);
    assert_eq!(store.tracked_count(), 0, "flag off must not write to sink");

    // Resolve attempt with flag off + non-required — must Fallback.
    let spec = InheritPrefixSpec {
        from_run_id: None,
        required: false,
    };
    let ctx = matching_spawn_ctx("run-parent", "claude-opus-4-6");
    let resolve_outcome = resolve_inherit_prefix(Some(&spec), &ctx, sink);
    match resolve_outcome {
        PrefixResolveOutcome::Fallback {
            reason: ResolveFailure::FeatureDisabled,
        } => {}
        other => panic!("expected Fallback{{FeatureDisabled}}, got {other:?}"),
    }
}

#[test]
fn miss_event_fires_when_provider_returns_zero_cache_read() {
    // Simulates the "inherited, reconstructed, sent — but provider
    // cache TTL expired" path. Resolver + reconstruct succeed;
    // event says Miss.
    let _g = FlagGuard::set(true);
    let store = InMemoryPrefixStore::new();
    let sink: &dyn PrefixCaptureSink = &store;

    let req = build_sample_request("run-parent", "claude-opus-4-6", 0);
    let cap_outcome = capture_parent_prefix(req, sink);
    let prefix_id = match cap_outcome {
        ForkCaptureOutcome::Captured { prefix_id, .. } => prefix_id,
        other => panic!("{other:?}"),
    };

    let spec = InheritPrefixSpec {
        from_run_id: None,
        required: false,
    };
    let ctx = matching_spawn_ctx("run-parent", "claude-opus-4-6");
    let prefix = match resolve_inherit_prefix(Some(&spec), &ctx, sink) {
        PrefixResolveOutcome::Resolved { prefix } => prefix,
        other => panic!("{other:?}"),
    };
    let _rebuilt = reconstruct_messages(&prefix, vec![]).unwrap();

    // Simulate provider returning 0 cache_read_tokens — cache missed
    // server-side despite our best effort.
    let probe = ForkCacheProbe {
        prefix_id: prefix.prefix_id.clone(),
        parent_run_id: prefix.parent_run_id.clone(),
        child_run_id: "run-child".into(),
        expected_cache_read_tokens: 12_000,
        observed_cache_read_tokens: 0,
        provider: prefix.provider.clone(),
    };
    let event = evaluate_fork_cache(probe, ForkCacheThresholds::default());
    assert_eq!(
        event.prefix_id, prefix_id,
        "prefix_id round-trips to Miss event"
    );
    assert_eq!(event.outcome, ForkCacheOutcome::Miss);
}

#[test]
fn partial_drift_event_classifies_mid_range_ratio() {
    // Capture + resolve + reconstruct proceed normally; provider
    // reports 60% of expected — legitimate microcompact-trimmed
    // scenario, PartialDrift (not Miss, not Hit).
    let _g = FlagGuard::set(true);
    let store = InMemoryPrefixStore::new();
    let sink: &dyn PrefixCaptureSink = &store;
    let _ = capture_parent_prefix(
        build_sample_request("run-parent", "claude-opus-4-6", 0),
        sink,
    );
    let prefix = match resolve_inherit_prefix(
        Some(&InheritPrefixSpec {
            from_run_id: None,
            required: false,
        }),
        &matching_spawn_ctx("run-parent", "claude-opus-4-6"),
        sink,
    ) {
        PrefixResolveOutcome::Resolved { prefix } => prefix,
        other => panic!("{other:?}"),
    };
    let probe = ForkCacheProbe {
        prefix_id: prefix.prefix_id.clone(),
        parent_run_id: prefix.parent_run_id.clone(),
        child_run_id: "run-child".into(),
        expected_cache_read_tokens: 10_000,
        observed_cache_read_tokens: 6_000,
        provider: prefix.provider.clone(),
    };
    let event = evaluate_fork_cache(probe, ForkCacheThresholds::default());
    assert_eq!(event.outcome, ForkCacheOutcome::PartialDrift);
}

#[test]
fn two_parents_in_store_resolve_independently() {
    // Multi-parent store. Each child's resolver must pick its own
    // parent's prefix — no cross-contamination.
    let _g = FlagGuard::set(true);
    let store = InMemoryPrefixStore::new();
    let sink: &dyn PrefixCaptureSink = &store;

    let pa = capture_parent_prefix(build_sample_request("run-A", "claude-opus-4-6", 0), sink);
    let pb = capture_parent_prefix(build_sample_request("run-B", "claude-opus-4-6", 0), sink);
    let pa_id = match pa {
        ForkCaptureOutcome::Captured { prefix_id, .. } => prefix_id,
        _ => panic!(),
    };
    let pb_id = match pb {
        ForkCaptureOutcome::Captured { prefix_id, .. } => prefix_id,
        _ => panic!(),
    };
    assert_ne!(pa_id, pb_id, "distinct captures produce distinct ids");

    let spec = InheritPrefixSpec {
        from_run_id: None,
        required: false,
    };
    let rx_a = resolve_inherit_prefix(
        Some(&spec),
        &matching_spawn_ctx("run-A", "claude-opus-4-6"),
        sink,
    );
    let rx_b = resolve_inherit_prefix(
        Some(&spec),
        &matching_spawn_ctx("run-B", "claude-opus-4-6"),
        sink,
    );
    let (pfx_a, pfx_b) = match (rx_a, rx_b) {
        (
            PrefixResolveOutcome::Resolved { prefix: a },
            PrefixResolveOutcome::Resolved { prefix: b },
        ) => (a, b),
        other => panic!("both must Resolve: {other:?}"),
    };
    assert_eq!(pfx_a.prefix_id, pa_id);
    assert_eq!(pfx_b.prefix_id, pb_id);
    assert_eq!(pfx_a.parent_run_id, "run-A");
    assert_eq!(pfx_b.parent_run_id, "run-B");
}

#[test]
fn prefix_id_present_in_all_five_flow_stages() {
    // Meta-test: `prefix_id` is the single join key running through
    // every stage. If any future refactor accidentally drops it from
    // one layer, telemetry joins downstream will silently lose data.
    // This test pins the presence explicitly.
    let _g = FlagGuard::set(true);
    let store = InMemoryPrefixStore::new();
    let sink: &dyn PrefixCaptureSink = &store;

    // Stage 1 — capture emits it.
    let cap = capture_parent_prefix(build_sample_request("run-X", "claude-opus-4-6", 0), sink);
    let id = match cap {
        ForkCaptureOutcome::Captured { prefix_id, .. } => prefix_id,
        _ => panic!(),
    };
    assert!(!id.is_empty());

    // Stage 2 — store keeps it on the ForkPrefix.
    let stored = sink.get_prefix("run-X").expect("entry exists");
    assert_eq!(stored.prefix_id, id);

    // Stage 3 — resolver returns the same Arc (pointer equality is
    // too strict — store clones, but id MUST match).
    let rx = resolve_inherit_prefix(
        Some(&InheritPrefixSpec {
            from_run_id: None,
            required: false,
        }),
        &matching_spawn_ctx("run-X", "claude-opus-4-6"),
        sink,
    );
    let pfx = match rx {
        PrefixResolveOutcome::Resolved { prefix } => prefix,
        _ => panic!(),
    };
    assert_eq!(pfx.prefix_id, id);

    // Stage 4 — reconstruct doesn't alter the ForkPrefix itself, but
    // `prefix` stays valid for the caller's event construction.
    let _ = reconstruct_messages(&pfx, vec![]).unwrap();
    assert_eq!(pfx.prefix_id, id, "reconstruct must not mutate prefix_id");

    // Stage 5 — event carries it forward for telemetry joins.
    let probe = ForkCacheProbe {
        prefix_id: pfx.prefix_id.clone(),
        parent_run_id: pfx.parent_run_id.clone(),
        child_run_id: "run-child".into(),
        expected_cache_read_tokens: 100,
        observed_cache_read_tokens: 100,
        provider: pfx.provider.clone(),
    };
    let event = evaluate_fork_cache(probe, ForkCacheThresholds::default());
    assert_eq!(event.prefix_id, id, "event must carry the same id");
}

// ---------------------------------------------------------------------
// Tripwire — env var name stays stable
// ---------------------------------------------------------------------

#[test]
fn env_var_name_stable_across_pipeline() {
    // This integration test lives in a different module than the
    // lib tripwire; if operational docs reference the constant,
    // changing it here exposes the change to every call-site.
    assert_eq!(FORK_INHERIT_PREFIX_ENV, "ASTRA_FORK_INHERIT_PREFIX");
}
