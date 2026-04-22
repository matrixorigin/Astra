//! Phase F — Track #3: nested sub-run cancellation propagation (2+ levels deep).
//!
//! The audit flagged that while single-level cancel was covered by
//! `team_execution_respects_cancellation` (via `JoinHandle::abort`), the
//! `cancel_token` propagation path — which is how real sub-runs inside
//! `DelegationEngine` are asked to wind down gracefully — had no test at
//! nesting depth > 1.
//!
//! These tests use `DelegationEngine` plus a mock `SubRunExecutor` that
//! spawns two additional task levels internally, both holding a clone of the
//! same `Arc<CancellationToken>` received via `SubRunConfig::cancel_token`.
//! Cancelling the root token must flip the signal for every descendant, and
//! already-completed work must not be rolled back retroactively.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use astra_runtime::server::delegation_engine::{
    DelegationEngine, DelegationTracker, SubRunConfig, SubRunExecutor,
};
use astra_runtime::server::run_engine::RunEngine;
use astra_services::coordination::{
    AgentProfile, AgentProfileRegistry, AgentResult, AgentTier, AggregationStrategy,
    CoordinationPattern, DelegationRequest,
};
use astra_services::runs::InMemoryRunStateStore;

// ─── Test harness ───────────────────────────────────────────────────────────

fn setup() -> (
    Arc<RwLock<AgentProfileRegistry>>,
    Arc<RunEngine>,
    Arc<DelegationTracker>,
) {
    let mut reg = AgentProfileRegistry::new();
    reg.register(AgentProfile::new("orch", "Orch", AgentTier::Orchestrator))
        .unwrap();
    for name in ["w1", "w2", "w3", "w4", "w5"] {
        reg.register(AgentProfile::new(name, name, AgentTier::System))
            .unwrap();
    }
    let run_store = Arc::new(InMemoryRunStateStore::new());
    let run_engine = Arc::new(RunEngine::new(run_store));
    let tracker = Arc::new(DelegationTracker::new());
    (Arc::new(RwLock::new(reg)), run_engine, tracker)
}

fn fan_out(delegation_id: &str, agents: Vec<&str>) -> DelegationRequest {
    DelegationRequest {
        delegation_id: delegation_id.into(),
        parent_run_id: format!("parent-{delegation_id}"),
        task: "nested cancel probe".into(),
        pattern: CoordinationPattern::FanOut {
            agent_ids: agents.into_iter().map(String::from).collect(),
            aggregation: AggregationStrategy::AllResults,
            // Keep timeout noticeably longer than expected cancel latency so
            // a hang is distinguishable from natural completion.
            timeout_sec: 30,
        },
        user_id: "user-1".into(),
        depth: 0,
        context: HashMap::new(),
    }
}

/// Mock executor whose `execute()` itself opens two additional nesting levels
/// on top of the `SubRunConfig::cancel_token` it receives, emulating real
/// production paths where a sub-run internally spawns deeper work.
///
/// At every depth level, each task awaits either the shared cancel signal or
/// a long natural timeout. Every observed cancellation bumps `seen_depth_N`.
#[derive(Clone)]
struct NestedMockExecutor {
    seen_depth_0: Arc<AtomicUsize>,
    seen_depth_1: Arc<AtomicUsize>,
    seen_depth_2: Arc<AtomicUsize>,
    started_at_root: Arc<AtomicUsize>,
    children_per_level: usize,
}

impl NestedMockExecutor {
    fn new(children_per_level: usize) -> Self {
        Self {
            seen_depth_0: Arc::new(AtomicUsize::new(0)),
            seen_depth_1: Arc::new(AtomicUsize::new(0)),
            seen_depth_2: Arc::new(AtomicUsize::new(0)),
            started_at_root: Arc::new(AtomicUsize::new(0)),
            children_per_level,
        }
    }
}

#[async_trait]
impl SubRunExecutor for NestedMockExecutor {
    async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
        self.started_at_root.fetch_add(1, Ordering::SeqCst);
        let token = config.cancel_token.clone().expect("cancel_token required");

        // Spawn `children_per_level` tasks at depth-1, each of which spawns
        // the same count at depth-2. All share the same `Arc<CancellationToken>`.
        let mut depth_1_handles = Vec::new();
        for _ in 0..self.children_per_level {
            let t1 = token.clone();
            let seen1 = self.seen_depth_1.clone();
            let seen2 = self.seen_depth_2.clone();
            let n_grand = self.children_per_level;

            depth_1_handles.push(tokio::spawn(async move {
                let mut grand = Vec::new();
                for _ in 0..n_grand {
                    let t2 = t1.clone();
                    let seen2 = seen2.clone();
                    grand.push(tokio::spawn(async move {
                        tokio::select! {
                            _ = t2.cancelled() => {
                                seen2.fetch_add(1, Ordering::SeqCst);
                            }
                            _ = tokio::time::sleep(Duration::from_secs(30)) => {}
                        }
                    }));
                }
                tokio::select! {
                    _ = t1.cancelled() => {
                        seen1.fetch_add(1, Ordering::SeqCst);
                    }
                    _ = tokio::time::sleep(Duration::from_secs(30)) => {}
                }
                for h in grand {
                    let _ = h.await;
                }
            }));
        }

        // Depth-0 wait (the executor itself).
        tokio::select! {
            _ = token.cancelled() => {
                self.seen_depth_0.fetch_add(1, Ordering::SeqCst);
            }
            _ = tokio::time::sleep(Duration::from_secs(30)) => {}
        }
        for h in depth_1_handles {
            let _ = h.await;
        }

        Ok(AgentResult {
            agent_id: config.agent_profile.agent_id,
            run_id: config.run_id,
            status: "cancelled".to_string(),
            output: None,
            error: None,
            prompt_tokens: 0,
            completion_tokens: 0,
            tool_calls: 0,
        })
    }
}

// ─── 1. Root cancel propagates through 3 nesting levels ─────────────────────
// Fan-out of 2 sub-runs at depth-0; each opens 3 depth-1 and 9 depth-2 tasks
// all sharing the same `Arc<CancellationToken>`. Root cancel must be observed
// at every depth, and the top-level `execute` must return well before the
// natural 30-s task timeout.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nested_cancel_propagates_through_three_levels() {
    let (reg, engine, tracker) = setup();
    let exec = Arc::new(NestedMockExecutor::new(3));
    let de = DelegationEngine::with_executor(reg, engine, tracker, exec.clone());

    let token = Arc::new(CancellationToken::new());
    let req = fan_out("del-nested-1", vec!["w1", "w2"]);

    // Cancel after depth-2 tasks have had time to reach the select! boundary.
    // Use a generous delay so concurrent test execution does not starve the
    // root tasks before they reach their cancel-await point.
    let cancel_after = {
        let t = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            t.cancel();
        })
    };

    let started = Instant::now();
    let result = de.execute(req, "orch", Some(token.clone())).await.unwrap();
    let elapsed = started.elapsed();
    let _ = cancel_after.await;

    // Sanity: the top-level execute returned quickly, not after the 30-s
    // natural timeout.
    assert!(
        elapsed < Duration::from_secs(5),
        "execute blocked after cancel; took {elapsed:?}"
    );

    // At least one root sub-run reached the executor (guards against the
    // regression where the engine never spawns tasks at all).
    assert!(
        exec.started_at_root.load(Ordering::SeqCst) >= 1,
        "at least one root sub-run must have started; got {}",
        exec.started_at_root.load(Ordering::SeqCst)
    );

    // Every depth must observe the cancel signal for at least one task.
    // (With join_set.abort_all() the depth-0 awaits may be dropped mid-flight
    //  so we assert a lower-bound rather than exact counts — the important
    //  invariant is that the signal propagated into the deeper levels.)
    assert!(
        exec.seen_depth_1.load(Ordering::SeqCst) >= 1,
        "depth-1 tasks must observe cancel (shared Arc propagation)"
    );
    assert!(
        exec.seen_depth_2.load(Ordering::SeqCst) >= 1,
        "depth-2 tasks must observe cancel (shared Arc propagation)"
    );

    // Every returned agent_result is marked cancelled (produced by leaf
    // executors) OR the sub-runs were outright aborted by join_set. Either
    // way no result may falsely report "completed" when cancel fired first.
    for r in &result.agent_results {
        assert_ne!(
            r.status, "completed",
            "sub-run {} claimed completed after cancel: {:?}",
            r.agent_id, r
        );
    }
}

// ─── 2. Cancel before execute short-circuits ────────────────────────────────
// If the token is already cancelled when `execute` is called, the engine
// should not burn seconds on sub-runs that can never make progress.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pre_cancelled_token_short_circuits_nested_execute() {
    let (reg, engine, tracker) = setup();
    let exec = Arc::new(NestedMockExecutor::new(2));
    let de = DelegationEngine::with_executor(reg, engine, tracker, exec.clone());

    let token = Arc::new(CancellationToken::new());
    token.cancel(); // pre-cancel

    let req = fan_out("del-nested-2", vec!["w1", "w2", "w3"]);

    let started = Instant::now();
    let _ = de.execute(req, "orch", Some(token.clone())).await;
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_secs(3),
        "pre-cancelled execute should return fast; took {elapsed:?}"
    );
}

// ─── 3. Cancel of one delegation does not cancel an independent one ─────────
// A detached token for a sibling delegation must not be flipped when an
// unrelated token fires. This guards against a regression where someone
// unifies tokens into a shared global.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sibling_delegations_have_isolated_cancel_tokens() {
    let (reg, engine, tracker) = setup();
    let exec = Arc::new(NestedMockExecutor::new(2));
    let de = Arc::new(DelegationEngine::with_executor(
        reg,
        engine,
        tracker,
        exec.clone(),
    ));

    let token_a = Arc::new(CancellationToken::new());
    let token_b = Arc::new(CancellationToken::new());

    // Two independent in-flight delegations on the same engine.
    let de_a = de.clone();
    let ta = token_a.clone();
    let handle_a = tokio::spawn(async move {
        let req = fan_out("del-iso-a", vec!["w1"]);
        de_a.execute(req, "orch", Some(ta)).await
    });

    let de_b = de.clone();
    let tb = token_b.clone();
    let handle_b = tokio::spawn(async move {
        let req = fan_out("del-iso-b", vec!["w2"]);
        de_b.execute(req, "orch", Some(tb)).await
    });

    // Give both a chance to reach the cancel-await boundary.
    tokio::time::sleep(Duration::from_millis(80)).await;

    // Cancel only A.
    token_a.cancel();

    // A completes quickly; B is still pending.
    let a = tokio::time::timeout(Duration::from_secs(5), handle_a)
        .await
        .expect("A must unwind after its own token fires")
        .unwrap();
    assert!(a.is_ok(), "delegation A should return Ok, got {a:?}");

    // B must still be running — poll its handle (don't await, would block).
    // Use a short timeout to prove it hasn't finished.
    let b_timeout = tokio::time::timeout(Duration::from_millis(200), handle_b).await;
    assert!(
        b_timeout.is_err(),
        "sibling delegation B must NOT be affected by A's cancel"
    );

    // Clean up B so the test actually exits.
    token_b.cancel();
}

// ─── 4. Shared Arc<CancellationToken> — one cancel flips every clone ───────
// Pure primitive invariant that the `DelegationEngine` relies on.
// Codifying this guards against a refactor that accidentally introduces
// `child_token()` (which would break the nested propagation contract).
#[tokio::test]
async fn shared_arc_cancellation_token_fires_every_clone_once() {
    let root = Arc::new(CancellationToken::new());

    let clones: Vec<Arc<CancellationToken>> = (0..8).map(|_| root.clone()).collect();

    // All clones start un-cancelled.
    for c in &clones {
        assert!(!c.is_cancelled());
    }

    // Spawn a watcher per clone.
    let mut handles = Vec::new();
    let counter = Arc::new(AtomicUsize::new(0));
    for c in clones {
        let counter = counter.clone();
        handles.push(tokio::spawn(async move {
            c.cancelled().await;
            counter.fetch_add(1, Ordering::SeqCst);
        }));
    }

    // Fire once at the root.
    tokio::time::sleep(Duration::from_millis(20)).await;
    root.cancel();

    for h in handles {
        let _ = tokio::time::timeout(Duration::from_secs(2), h).await;
    }
    assert_eq!(
        counter.load(Ordering::SeqCst),
        8,
        "single root cancel must fire every Arc clone exactly once"
    );
    assert!(root.is_cancelled());
}
