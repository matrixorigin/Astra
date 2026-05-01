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

    // Wait for the executor to actually start at least one sub-run before
    // cancelling — and only then give depth-1/2 tasks a small grace window
    // to reach their `select!`. The previous version slept a fixed 500ms
    // before cancelling, which flaked under load: if the 4-worker pool was
    // busy, the root task wouldn't be scheduled before the sleep elapsed,
    // we'd cancel a not-yet-started execution, and `started_at_root` stayed
    // at 0. Polling `started_at_root` makes the wait load-invariant.
    let cancel_after = {
        let t = token.clone();
        let started = exec.started_at_root.clone();
        tokio::spawn(async move {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            while tokio::time::Instant::now() < deadline
                && started.load(Ordering::SeqCst) == 0
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            // Small grace window so depth-1/2 tasks reach `select!`.
            tokio::time::sleep(Duration::from_millis(50)).await;
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

// ─── 4. REMOVED: `shared_arc_cancellation_token_fires_every_clone_once`
//     per review: that test was exercising the `tokio_util::sync::CancellationToken`
//     primitive itself, not any business logic in `DelegationEngine`. Property
//     is already covered by `tokio_util`'s own test suite.

// ─── 5. Precise-count propagation (no DelegationEngine abort_all in the way) ──
//
// The `nested_cancel_propagates_through_three_levels` test above must use
// a lower-bound (>= 1) assertion because the production `DelegationEngine`
// calls `join_set.abort_all()` on token fire, which MAY unwind depth-0
// tasks before they reach their `select!` cancel arm. That's a real
// (and desirable) fast-path optimization.
//
// This test bypasses `DelegationEngine` entirely and builds the same
// 3-level tree directly from `tokio::spawn`, so every spawned task HAS
// to reach its cancel-await boundary before we fire the root. With nothing
// aborting tasks mid-flight, we can assert *exact* counts at every depth.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nested_cancel_primitive_exact_counts_per_depth() {
    let token = Arc::new(CancellationToken::new());
    let seen_0 = Arc::new(AtomicUsize::new(0));
    let seen_1 = Arc::new(AtomicUsize::new(0));
    let seen_2 = Arc::new(AtomicUsize::new(0));

    const D0: usize = 2; // number of depth-0 branches
    const D1: usize = 3; // per depth-0: depth-1 children
    const D2: usize = 4; // per depth-1: depth-2 children
    const TOTAL_0: usize = D0;
    const TOTAL_1: usize = D0 * D1;
    const TOTAL_2: usize = D0 * D1 * D2;

    // Readiness signal: every leaf reports when it's PARKED on the select.
    let ready_0 = Arc::new(AtomicUsize::new(0));
    let ready_1 = Arc::new(AtomicUsize::new(0));
    let ready_2 = Arc::new(AtomicUsize::new(0));

    let mut depth_0_handles = Vec::new();
    for _ in 0..D0 {
        let t0 = token.clone();
        let s0 = seen_0.clone();
        let s1 = seen_1.clone();
        let s2 = seen_2.clone();
        let r0 = ready_0.clone();
        let r1 = ready_1.clone();
        let r2 = ready_2.clone();

        depth_0_handles.push(tokio::spawn(async move {
            // Spawn depth-1 children.
            let mut d1_handles = Vec::new();
            for _ in 0..D1 {
                let t1 = t0.clone();
                let s1 = s1.clone();
                let s2 = s2.clone();
                let r1 = r1.clone();
                let r2 = r2.clone();
                d1_handles.push(tokio::spawn(async move {
                    // Spawn depth-2 children.
                    let mut d2_handles = Vec::new();
                    for _ in 0..D2 {
                        let t2 = t1.clone();
                        let s2 = s2.clone();
                        let r2 = r2.clone();
                        d2_handles.push(tokio::spawn(async move {
                            r2.fetch_add(1, Ordering::SeqCst);
                            t2.cancelled().await;
                            s2.fetch_add(1, Ordering::SeqCst);
                        }));
                    }
                    r1.fetch_add(1, Ordering::SeqCst);
                    t1.cancelled().await;
                    s1.fetch_add(1, Ordering::SeqCst);
                    for h in d2_handles {
                        let _ = h.await;
                    }
                }));
            }
            r0.fetch_add(1, Ordering::SeqCst);
            t0.cancelled().await;
            s0.fetch_add(1, Ordering::SeqCst);
            for h in d1_handles {
                let _ = h.await;
            }
        }));
    }

    // Wait until EVERY task is parked on its cancel-await (ready counts == totals).
    let start = Instant::now();
    loop {
        if ready_0.load(Ordering::SeqCst) == TOTAL_0
            && ready_1.load(Ordering::SeqCst) == TOTAL_1
            && ready_2.load(Ordering::SeqCst) == TOTAL_2
        {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "tasks failed to park: ready_0={}/{} ready_1={}/{} ready_2={}/{}",
            ready_0.load(Ordering::SeqCst),
            TOTAL_0,
            ready_1.load(Ordering::SeqCst),
            TOTAL_1,
            ready_2.load(Ordering::SeqCst),
            TOTAL_2
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // All parked — fire root exactly once.
    token.cancel();

    for h in depth_0_handles {
        let _ = tokio::time::timeout(Duration::from_secs(3), h).await;
    }

    // EXACT counts — every spawned task observed exactly one cancel.
    assert_eq!(
        seen_0.load(Ordering::SeqCst),
        TOTAL_0,
        "depth-0 exact propagation"
    );
    assert_eq!(
        seen_1.load(Ordering::SeqCst),
        TOTAL_1,
        "depth-1 exact propagation"
    );
    assert_eq!(
        seen_2.load(Ordering::SeqCst),
        TOTAL_2,
        "depth-2 exact propagation"
    );
}
