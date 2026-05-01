//! End-to-end spawn-with-prefix integration tests.
//!
//! The 9-PR turn-core suite (fork_prefix_e2e.rs) covers the data
//! flow across `fork_capture`, `fork_prefix_store`, `fork_resolve`,
//! `fork_reconstruct`, `fork_cache_event`. This suite covers the
//! two runtime-level PRs (5.5 + 5.6) that glue those primitives
//! into the spawner:
//!
//! - PR 5.5 populates `SpawnRunConfig.inherited_prefix`
//! - PR 5.6 has the executor consume it + emit a `ForkCacheEvent`
//!   via the probe helper
//!
//! We use a mock executor that:
//! 1. Captures the `SpawnRunConfig.inherited_prefix` it receives.
//! 2. Simulates the "first successful ingested turn" side-effect by
//!    explicitly invoking the probe helper — this is what
//!    `SubRunHost::on_turn_completed` does on the CLI side. Mocking
//!    at this boundary keeps the test HTTP-free while still
//!    exercising the full spawner → config → probe → sink chain.
//!
//! What the CLI-side executor does with `prefix_messages` (prepend
//! to initial `state.messages`) is covered by runtime lib tests
//! (`inherited_prefix_messages_round_trip_byte_identical` in
//! spawner.rs) and the CLI compilation itself — this suite focuses
//! on the runtime-owned glue.

use std::sync::{Arc, Mutex};

use astra_messaging::in_process::InProcessTransport;
use astra_messaging::router::AgentMailboxRouter;
use astra_runtime::orchestration::{
    DynamicAgentSpawner, ForkCacheProbeState, InheritedChildPrefix, SpawnAgentExecutor,
    SpawnAgentInput, SpawnAgentOutput, SpawnContext, SpawnRunConfig, SpawnRunResult,
    maybe_emit_fork_cache_probe,
};
use astra_runtime::server::delegation_engine::DelegationTracker;
use astra_turn_core::fork_cache_event::{
    ForkCacheEvent, ForkCacheEventSink, ForkCacheOutcome, ForkCacheThresholds,
};
use astra_turn_core::fork_capture::{
    CaptureRequest, FORK_FLAG_TEST_MUTEX, ForkCaptureOutcome, capture_parent_prefix,
    restore_fork_flag_raw_for_tests, set_fork_flag_for_tests,
};
use astra_turn_core::fork_prefix::{
    CacheMode, ProviderKind, SystemBlock, ThinkingConfigSlice, ToolSchemaEntry, hash_tool_schema,
};
use astra_turn_core::fork_prefix_store::{InMemoryPrefixStore, PrefixCaptureSink};
use astra_turn_core::orchestration_spawn_tool::InheritPrefixSpec;
use async_trait::async_trait;
use serde_json::json;

// ---------------------------------------------------------------------
// Test harness
// ---------------------------------------------------------------------

/// Cross-crate flag guard — shares the FORK_FLAG_TEST_MUTEX so we
/// serialize with every other fork-flag test (lib + this suite +
/// the turn-core fork_prefix_e2e suite).
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

/// Sink that captures every event for post-test assertions.
#[derive(Default)]
struct CollectSink(Mutex<Vec<ForkCacheEvent>>);

impl ForkCacheEventSink for CollectSink {
    fn emit(&self, event: ForkCacheEvent) {
        self.0.lock().unwrap().push(event);
    }
}

impl CollectSink {
    fn snapshot(&self) -> Vec<ForkCacheEvent> {
        self.0.lock().unwrap().clone()
    }
}

/// Mock executor that:
/// 1. Captures the inherited_prefix the spawner hands it (so the
///    test can assert on config.inherited_prefix).
/// 2. Simulates the "first successful ingested turn" by invoking
///    `maybe_emit_fork_cache_probe` with a caller-supplied observed
///    cache_read value. This is the exact call pattern
///    `SubRunHost::on_turn_completed` uses in production CLI code.
struct MockProbingExecutor {
    captured: Arc<Mutex<Option<Option<InheritedChildPrefix>>>>,
    observed_cache_read: u64,
    sink: Arc<dyn ForkCacheEventSink>,
}

impl MockProbingExecutor {
    fn new(
        observed: u64,
        sink: Arc<dyn ForkCacheEventSink>,
    ) -> (Self, Arc<Mutex<Option<Option<InheritedChildPrefix>>>>) {
        let captured = Arc::new(Mutex::new(None));
        (
            Self {
                captured: captured.clone(),
                observed_cache_read: observed,
                sink,
            },
            captured,
        )
    }
}

#[async_trait]
impl SpawnAgentExecutor for MockProbingExecutor {
    async fn execute(&self, config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
        *self.captured.lock().unwrap() = Some(config.inherited_prefix.clone());

        // Simulate the on_turn_completed hook: first-turn probe.
        let mut probe_state = ForkCacheProbeState::new();
        maybe_emit_fork_cache_probe(
            &mut probe_state,
            config.inherited_prefix.as_ref(),
            &config.run_id,
            self.observed_cache_read,
            ForkCacheThresholds::default(),
            self.sink.as_ref(),
        );

        Ok(SpawnRunResult {
            agent_id: config.agent_id,
            run_id: config.run_id,
            status: "completed".into(),
            output: Some("done".into()),
            error: None,
            prompt_tokens: 0,
            completion_tokens: 0,
            tool_calls: 0,
            permission_summary: None,
            permission_requests: 0,
            permission_requests_approved: 0,
            tools_blocked: 0,
        })
    }
}

fn mock_router() -> Arc<AgentMailboxRouter> {
    let transport = Arc::new(InProcessTransport::new());
    let dt = Arc::new(DelegationTracker::new());
    Arc::new(AgentMailboxRouter::new(transport, dt))
}

fn wall_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn capture_parent(store: &dyn PrefixCaptureSink, parent_run_id: &str, model: &str) -> String {
    let schema = json!({"function": {"name": "bash"}});
    let (schema_bytes, schema_hash) = hash_tool_schema(&schema);
    // Canonical bytes MUST be a JSON array of message objects —
    // reconstruct_messages enforces this. Using a real 2-message
    // transcript so the test mirrors production shape.
    let parent_msgs = json!([
        {"role": "user", "content": "analyze the file"},
        {"role": "assistant", "content": "I'll read it and summarize."}
    ]);
    let canonical = serde_json::to_vec(&parent_msgs).expect("static JSON encodes");

    let outcome = capture_parent_prefix(
        CaptureRequest {
            parent_run_id: parent_run_id.into(),
            parent_turn_seq: 1,
            provider: ProviderKind::Anthropic,
            model_id: model.into(),
            thinking: Some(ThinkingConfigSlice {
                enabled: false,
                budget_tokens: 0,
                kind: "disabled".into(),
            }),
            system_blocks: vec![SystemBlock {
                bytes: b"you are helpful".to_vec(),
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
            captured_at_secs: wall_now_secs(),
            microcompact_fired_in_turn: false,
        },
        store,
    );
    match outcome {
        ForkCaptureOutcome::Captured { prefix_id, .. } => prefix_id,
        other => panic!("capture setup failed: {other:?}"),
    }
}

fn child_input(required: bool) -> SpawnAgentInput {
    SpawnAgentInput {
        description: "child".into(),
        prompt: "subtask".into(),
        agent_type: "explore".into(),
        background: false, // sync so mock executor runs before spawn returns
        inherit_prefix: Some(InheritPrefixSpec {
            from_run_id: None, // use caller's run id
            required,
        }),
        ..Default::default()
    }
}

fn parent_context(parent_run_id: &str) -> SpawnContext {
    SpawnContext {
        parent_run_id: parent_run_id.into(),
        parent_agent_id: "parent-agent".into(),
        recursion_depth: 0,
        working_dir: std::path::PathBuf::from("/tmp"),
        inherited_permissions: None,
        inherited_skills: vec![],
    }
}

/// Pull the explore agent's default model from the spawner's
/// registry so capture_parent uses a model that will match the
/// child's resolve context.
fn explore_model(spawner: &DynamicAgentSpawner) -> String {
    spawner
        .agent_registry()
        .get("explore")
        .expect("explore agent type exists")
        .default_model
        .clone()
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[tokio::test]
async fn spawn_with_resolved_prefix_delivers_inherited_messages_and_emits_event() {
    let _g = FlagGuard::set(true);

    let store: Arc<dyn PrefixCaptureSink> = Arc::new(InMemoryPrefixStore::new());
    let sink = Arc::new(CollectSink::default());
    let typed_sink: Arc<dyn ForkCacheEventSink> = sink.clone();

    let (exec, captured_handle) = MockProbingExecutor::new(9_500, typed_sink.clone());
    let spawner = DynamicAgentSpawner::new(mock_router())
        .with_prefix_store(store.clone())
        .with_executor(Arc::new(exec) as Arc<dyn SpawnAgentExecutor>);

    let model = explore_model(&spawner);
    let parent_prefix_id = capture_parent(&*store, "run-parent-E2E", &model);

    let result = spawner
        .spawn(child_input(false), &parent_context("run-parent-E2E"))
        .await
        .expect("spawn should succeed");
    assert!(matches!(result, SpawnAgentOutput::Completed { .. }));

    // 1. Executor saw the resolved prefix on its config.
    let seen = captured_handle
        .lock()
        .unwrap()
        .take()
        .expect("mock executor ran once");
    let inherited = seen.expect("config.inherited_prefix must be Some on Resolved outcome");
    assert_eq!(
        inherited.prefix_id, parent_prefix_id,
        "prefix_id must equal the id capture emitted"
    );
    assert_eq!(inherited.parent_run_id, "run-parent-E2E");
    assert!(
        !inherited.prefix_messages.is_empty(),
        "inherited.prefix_messages must contain the captured parent messages"
    );
    assert!(matches!(inherited.provider, ProviderKind::Anthropic));

    // 2. Sink received exactly one event carrying prefix_id + parent_run_id
    //    through the full pipeline. With observed=9_500, expected=0 sentinel
    //    (PR 5.5 passes 0 until a real estimator is wired), the classifier
    //    hits the "zero_expected + observed > 0" branch → ExceededExpected.
    let events = sink.snapshot();
    assert_eq!(events.len(), 1, "exactly one ForkCacheEvent per spawn");
    assert_eq!(events[0].prefix_id, parent_prefix_id);
    assert_eq!(events[0].parent_run_id, "run-parent-E2E");
    assert_eq!(events[0].observed_cache_read_tokens, 9_500);
    assert_eq!(
        events[0].outcome,
        ForkCacheOutcome::ExceededExpected,
        "expected_cache_read=0 + observed>0 degenerate branch → ExceededExpected"
    );
}

#[tokio::test]
async fn spawn_without_inherit_spec_delivers_none_and_emits_nothing() {
    let _g = FlagGuard::set(true);
    let store: Arc<dyn PrefixCaptureSink> = Arc::new(InMemoryPrefixStore::new());
    let sink = Arc::new(CollectSink::default());
    let typed_sink: Arc<dyn ForkCacheEventSink> = sink.clone();

    let (exec, captured_handle) = MockProbingExecutor::new(0, typed_sink.clone());
    let spawner = DynamicAgentSpawner::new(mock_router())
        .with_prefix_store(store.clone())
        .with_executor(Arc::new(exec) as Arc<dyn SpawnAgentExecutor>);

    let model = explore_model(&spawner);
    capture_parent(&*store, "run-parent", &model);

    // Child WITHOUT inherit_prefix spec — executor should see None
    // and probe helper must not emit.
    let input = SpawnAgentInput {
        description: "fresh child".into(),
        prompt: "work".into(),
        agent_type: "explore".into(),
        background: false,
        inherit_prefix: None,
        ..Default::default()
    };

    let _ = spawner
        .spawn(input, &parent_context("run-parent"))
        .await
        .unwrap();

    let seen = captured_handle
        .lock()
        .unwrap()
        .take()
        .expect("mock executor ran");
    assert!(
        seen.is_none(),
        "no inherit spec must produce None inherited_prefix, got Some(...)"
    );
    assert!(
        sink.snapshot().is_empty(),
        "no probe must fire when inherited_prefix is None"
    );
}

#[tokio::test]
async fn spawn_with_optional_inherit_and_no_capture_falls_back_no_event() {
    // Fallback path: inherit_prefix requested, but no capture
    // exists for this run_id. Resolver returns Fallback; spawner
    // leaves inherited_prefix=None; probe helper is a no-op.
    let _g = FlagGuard::set(true);
    let store: Arc<dyn PrefixCaptureSink> = Arc::new(InMemoryPrefixStore::new());
    let sink = Arc::new(CollectSink::default());
    let typed_sink: Arc<dyn ForkCacheEventSink> = sink.clone();

    let (exec, captured_handle) = MockProbingExecutor::new(1_000, typed_sink.clone());
    let spawner = DynamicAgentSpawner::new(mock_router())
        .with_prefix_store(store)
        .with_executor(Arc::new(exec) as Arc<dyn SpawnAgentExecutor>);

    // No capture_parent call — store is empty.
    let _ = spawner
        .spawn(child_input(false), &parent_context("run-no-capture"))
        .await
        .unwrap();

    assert!(captured_handle.lock().unwrap().take().unwrap().is_none());
    assert!(
        sink.snapshot().is_empty(),
        "fallback must not emit ForkCacheEvent"
    );
}

#[tokio::test]
async fn captured_prefix_byte_identical_through_spawn_to_executor() {
    // Cache-reuse precondition at the runtime boundary: the bytes
    // the executor sees (serialize(prefix_messages)) must equal
    // the bytes the capture recorded. Without this, no prompt
    // cache hit is possible on the child's first API call.
    let _g = FlagGuard::set(true);
    let store: Arc<dyn PrefixCaptureSink> = Arc::new(InMemoryPrefixStore::new());
    let sink = Arc::new(CollectSink::default());
    let typed_sink: Arc<dyn ForkCacheEventSink> = sink.clone();

    let (exec, captured_handle) = MockProbingExecutor::new(9_000, typed_sink.clone());
    let spawner = DynamicAgentSpawner::new(mock_router())
        .with_prefix_store(store.clone())
        .with_executor(Arc::new(exec) as Arc<dyn SpawnAgentExecutor>);

    let model = explore_model(&spawner);
    let _ = capture_parent(&*store, "run-parent-bid", &model);
    let stored = store
        .get_prefix("run-parent-bid")
        .expect("capture persisted");
    let expected_bytes = stored.canonical_prefix_bytes().clone();

    let _ = spawner
        .spawn(child_input(false), &parent_context("run-parent-bid"))
        .await
        .unwrap();

    let seen = captured_handle
        .lock()
        .unwrap()
        .take()
        .unwrap()
        .expect("Resolved outcome");
    let reserialized = serde_json::to_vec(&seen.prefix_messages).unwrap();
    assert_eq!(
        reserialized.as_slice(),
        expected_bytes.as_slice(),
        "executor-visible prefix bytes must equal captured canonical bytes"
    );
}

#[tokio::test]
async fn event_prefix_id_chains_capture_to_emit() {
    // Cross-PR join-key contract: the `prefix_id` that capture
    // emitted must show up in the event as-is, with no mutation
    // through store → resolve → config → probe. This is the single
    // identifier dashboards use to correlate parent capture rows
    // with child event rows.
    let _g = FlagGuard::set(true);
    let store: Arc<dyn PrefixCaptureSink> = Arc::new(InMemoryPrefixStore::new());
    let sink = Arc::new(CollectSink::default());
    let typed_sink: Arc<dyn ForkCacheEventSink> = sink.clone();
    let (exec, _) = MockProbingExecutor::new(8_000, typed_sink.clone());
    let spawner = DynamicAgentSpawner::new(mock_router())
        .with_prefix_store(store.clone())
        .with_executor(Arc::new(exec) as Arc<dyn SpawnAgentExecutor>);
    let model = explore_model(&spawner);
    let prefix_id = capture_parent(&*store, "run-chain", &model);

    let _ = spawner
        .spawn(child_input(false), &parent_context("run-chain"))
        .await
        .unwrap();

    let events = sink.snapshot();
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].prefix_id, prefix_id,
        "prefix_id must chain from capture to event unchanged"
    );
}

#[tokio::test]
async fn feature_flag_off_kills_runtime_pipeline_end_to_end() {
    // Flag off at the runtime boundary: even with a sink installed
    // and a capture attempt, no Event fires and no inherited_prefix
    // reaches the executor. Pins the kill-switch contract across
    // all the glue modules (spawner, resolver, reconstruct, probe).
    let _g = FlagGuard::set(false);
    let store: Arc<dyn PrefixCaptureSink> = Arc::new(InMemoryPrefixStore::new());
    let sink = Arc::new(CollectSink::default());
    let typed_sink: Arc<dyn ForkCacheEventSink> = sink.clone();
    let (exec, captured_handle) = MockProbingExecutor::new(9_000, typed_sink.clone());
    let spawner = DynamicAgentSpawner::new(mock_router())
        .with_prefix_store(store.clone())
        .with_executor(Arc::new(exec) as Arc<dyn SpawnAgentExecutor>);

    // Capture attempt with flag off is a no-op — sink stays empty.
    let model = explore_model(&spawner);
    let cap = capture_parent_prefix(
        CaptureRequest {
            parent_run_id: "run-flag-off".into(),
            parent_turn_seq: 1,
            provider: ProviderKind::Anthropic,
            model_id: model.clone(),
            thinking: None,
            system_blocks: vec![SystemBlock {
                bytes: b"sys".to_vec(),
                has_cache_control: true,
            }],
            tool_schemas: vec![],
            beta_headers: vec![],
            canonical_prefix_bytes: b"[]".to_vec(),
            cache_mode: CacheMode::Write,
            captured_at_secs: wall_now_secs(),
            microcompact_fired_in_turn: false,
        },
        &*store,
    );
    assert_eq!(
        cap,
        ForkCaptureOutcome::FeatureDisabled,
        "capture must no-op with flag off"
    );

    let _ = spawner
        .spawn(child_input(false), &parent_context("run-flag-off"))
        .await
        .unwrap();

    assert!(captured_handle.lock().unwrap().take().unwrap().is_none());
    assert!(sink.snapshot().is_empty());
}
