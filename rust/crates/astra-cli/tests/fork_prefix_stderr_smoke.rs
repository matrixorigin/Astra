//! Stderr smoke test for the fork-prefix pipeline at the CLI layer.
//!
//! This test builds a real `DynamicAgentSpawner` with the stderr
//! sink wired (as `agent_runtime::build_one_shot_spawner` does in
//! production), captures a parent prefix, then triggers a
//! spawn+probe sequence via the `fork_cache_probe` helper the same
//! way `SubRunHost::on_turn_completed` does. Any regression that
//! stops the stderr line from being printable would fail here.
//!
//! We don't actually capture stderr (the `println!`-equivalent
//! goes to the real fd); the test confirms the helper doesn't
//! panic and that with a sink wired we reach the emit path.

use astra_turn_core::fork_cache_event::{
    ForkCacheEventSink, ForkCacheThresholds, StderrForkCacheSink,
};
use astra_turn_core::fork_capture::{
    CaptureRequest, ForkCaptureOutcome, capture_parent_prefix,
    restore_fork_flag_raw_for_tests, set_fork_flag_for_tests,
};
use astra_turn_core::fork_prefix::{
    CacheMode, ProviderKind, SystemBlock, ThinkingConfigSlice, ToolSchemaEntry, hash_tool_schema,
};
use astra_turn_core::fork_prefix_store::{InMemoryPrefixStore, PrefixCaptureSink};
use std::sync::Arc;

#[test]
fn stderr_sink_end_to_end_smoke() {
    // Manually wire the fork flag (the helper checks process global
    // state just like the production startup path does).
    let prev = set_fork_flag_for_tests(true);

    let store: Arc<dyn PrefixCaptureSink> = Arc::new(InMemoryPrefixStore::new());
    let sink: Arc<dyn ForkCacheEventSink> = Arc::new(StderrForkCacheSink);

    // Capture a parent prefix.
    let schema = serde_json::json!({"function": {"name": "bash"}});
    let (schema_bytes, schema_hash) = hash_tool_schema(&schema);
    let parent_messages = serde_json::json!([
        {"role": "user", "content": "hello"},
        {"role": "assistant", "content": "hi"}
    ]);
    let canonical = serde_json::to_vec(&parent_messages).unwrap();
    let out = capture_parent_prefix(
        CaptureRequest {
            parent_run_id: "run-smoke".into(),
            parent_turn_seq: 1,
            provider: ProviderKind::OpenAi, // MiniMax-style
            model_id: "MiniMax-M2.5".into(),
            thinking: Some(ThinkingConfigSlice {
                enabled: false,
                budget_tokens: 0,
                kind: "disabled".into(),
            }),
            system_blocks: vec![SystemBlock {
                bytes: b"system prompt".to_vec(),
                has_cache_control: true,
            }],
            tool_schemas: vec![ToolSchemaEntry {
                name: "bash".into(),
                canonical_bytes: schema_bytes,
                hash: schema_hash,
            }],
            beta_headers: vec![],
            canonical_prefix_bytes: canonical,
            cache_mode: CacheMode::Write,
            captured_at_secs: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            microcompact_fired_in_turn: false,
        },
        &*store,
    );
    let prefix_id = match out {
        ForkCaptureOutcome::Captured { prefix_id, .. } => prefix_id,
        other => panic!("capture should succeed, got {other:?}"),
    };

    // Retrieve + verify the stored ForkPrefix — simulates what a
    // spawn would hand to the executor.
    let stored = store.get_prefix("run-smoke").expect("captured prefix present");
    assert_eq!(stored.prefix_id, prefix_id);

    // Simulate executor.on_turn_completed: build an inherited and
    // call the probe helper with a plausible observed value. This
    // is the exact path SubRunHost takes in CLI production code.
    let inherited = astra_runtime::orchestration::InheritedChildPrefix {
        prefix_id: stored.prefix_id.clone(),
        parent_run_id: stored.parent_run_id.clone(),
        provider: stored.provider.clone(),
        prefix_messages: vec![],
        expected_cache_read_tokens: 1_000,
    };
    let mut probe_state = astra_runtime::orchestration::ForkCacheProbeState::new();
    astra_runtime::orchestration::maybe_emit_fork_cache_probe(
        &mut probe_state,
        Some(&inherited),
        "run-child-smoke",
        950, // 95% — Hit
        ForkCacheThresholds::default(),
        sink.as_ref(),
    );

    assert!(
        probe_state.fired(),
        "probe state must have fired after first call"
    );

    // Restore flag state so later tests in this binary aren't poisoned.
    restore_fork_flag_raw_for_tests(prev);

    // If we got here without panicking, stderr has received exactly one
    // [fork-cache] line with outcome=hit. Manual verification:
    //   cargo test -p astra-cli --test fork_prefix_stderr_smoke -- --nocapture
    // should print `[fork-cache] {"prefix_id":"...","outcome":"hit",...}`.
}
