//! On-demand coalescing of fresh control reads. No observations are retained
//! between batches, and this queue never polls or grants execution authority.

use std::{
    collections::{HashMap, HashSet, LinkedList, VecDeque},
    panic::AssertUnwindSafe,
    sync::{Arc, Mutex},
    time::Duration,
};

use astra_services::runs::{
    RUN_CONTROL_READ_BATCH_SIZE, RUN_CONTROL_READ_MAX_KEY_BYTES, RUN_CONTROL_READ_MIN_PARENT_HINTS,
    RunControlReadKey, RunControlReadKeyWithParent, RunControlReadResult,
    RunControlReadWithParentResult, RunStateStore,
};
use futures_util::{FutureExt, future::BoxFuture};
use tokio::{sync::oneshot, time::Instant};

const COLLECTION_WINDOW: Duration = Duration::from_millis(25);
const QUERY_TIMEOUT: Duration = Duration::from_secs(3);
const READ_TIMEOUT: Duration = Duration::from_secs(4);
const MAX_READS: usize = 8192;
const MAX_OWNER_READS: usize = 4096;
const MAX_WORKERS: usize = 4;

type Read = dyn Fn(
        Vec<RunControlReadKeyWithParent>,
    ) -> BoxFuture<'static, Result<Vec<RunControlReadWithParentResult>, String>>
    + Send
    + Sync;

pub(super) struct ControlReads {
    read: Arc<Read>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    owners: HashMap<String, Owner>,
    ready: VecDeque<String>,
    outstanding: usize,
    workers: usize,
}

#[derive(Default)]
struct Owner {
    // Release each queue node rather than retaining a user's historic peak.
    pending: LinkedList<Request>,
    outstanding: usize,
}

struct Request {
    key: RunControlReadKeyWithParent,
    collect_at: Instant,
    reply: oneshot::Sender<RunControlReadWithParentResult>,
}

impl State {
    fn release(&mut self, user_id: &str) {
        self.outstanding -= 1;
        let owner = self
            .owners
            .get_mut(user_id)
            .expect("admitted control owner");
        owner.outstanding -= 1;
        if owner.outstanding == 0 {
            self.owners.remove(user_id);
        }
    }

    fn take_batch(&mut self) -> Vec<Request> {
        let mut batch = Vec::with_capacity(RUN_CONTROL_READ_BATCH_SIZE);
        while batch.len() < RUN_CONTROL_READ_BATCH_SIZE {
            let Some(user_id) = self.ready.pop_front() else {
                break;
            };
            let owner = self.owners.get_mut(&user_id).expect("queued control owner");
            let request = owner.pending.pop_front().expect("queued control request");
            if !owner.pending.is_empty() {
                self.ready.push_back(user_id.clone());
            }
            if request.reply.is_closed() {
                self.release(&user_id);
            } else {
                batch.push(request);
            }
        }
        let hinted = batch
            .iter()
            .filter(|request| request.key.parent_hint.is_some())
            .take(RUN_CONTROL_READ_MIN_PARENT_HINTS)
            .count();
        if hinted < RUN_CONTROL_READ_MIN_PARENT_HINTS
            || batch
                .iter()
                .all(|request| request.key.parent_hint.is_some())
        {
            return batch;
        }

        // Keep the original worker and queue bounds. A joined read must not
        // make an unrelated unhinted observation share its failure domain.
        // Restore the other class to each owner's existing queue, retaining
        // its admission, deadline, and relative order.
        let take_hinted = batch[0].key.parent_hint.is_some();
        let (selected, deferred): (Vec<_>, Vec<_>) = batch
            .into_iter()
            .partition(|request| request.key.parent_hint.is_some() == take_hinted);
        let deferred = deferred
            .into_iter()
            .filter(|request| {
                if request.reply.is_closed() {
                    self.release(&request.key.key.user_id);
                    false
                } else {
                    true
                }
            })
            .collect::<Vec<_>>();
        let mut deferred_owners = Vec::new();
        let mut seen = HashSet::new();
        for request in &deferred {
            let user_id = request.key.key.user_id.as_str();
            if seen.insert(user_id) {
                deferred_owners.push(user_id);
            }
        }
        // Give the restored class the next turn even under a sustained
        // backlog, while keeping at most one ready entry for each owner.
        self.ready
            .retain(|user_id| !seen.contains(user_id.as_str()));
        for user_id in deferred_owners.into_iter().rev() {
            self.ready.push_front(user_id.to_owned());
        }
        drop(seen);
        for request in deferred.into_iter().rev() {
            self.owners
                .get_mut(&request.key.key.user_id)
                .expect("admitted control owner")
                .pending
                .push_front(request);
        }
        selected
    }
}

impl ControlReads {
    pub(super) fn new(store: Arc<dyn RunStateStore>) -> Arc<Self> {
        Arc::new(Self {
            read: Arc::new(move |keys| {
                let store = store.clone();
                Box::pin(async move { store.load_run_control_batch_with_parents(&keys).await })
            }),
            state: Mutex::new(State::default()),
        })
    }

    pub(super) async fn get(self: &Arc<Self>, user_id: &str, run_id: &str) -> RunControlReadResult {
        self.get_with_parent(user_id, run_id, None)
            .await
            .map(|result| result.own)
    }

    pub(super) async fn get_with_parent(
        self: &Arc<Self>,
        user_id: &str,
        run_id: &str,
        parent_hint: Option<&str>,
    ) -> RunControlReadWithParentResult {
        // Bound retained key bytes as well as callers. These limits exceed
        // the canonical VARCHAR identity capacities, including UTF-8 bytes.
        if user_id.len() > RUN_CONTROL_READ_MAX_KEY_BYTES
            || run_id.len() > RUN_CONTROL_READ_MAX_KEY_BYTES
        {
            return Err("run control identity exceeds capacity".into());
        }
        let queued_at = Instant::now();
        let (reply, receiver) = oneshot::channel();
        let start_worker = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.outstanding >= MAX_READS
                || state
                    .owners
                    .get(user_id)
                    .is_some_and(|owner| owner.outstanding >= MAX_OWNER_READS)
            {
                return Err("run control read queue is at capacity".into());
            }
            let owner = state.owners.entry(user_id.to_owned()).or_default();
            let was_empty = owner.pending.is_empty();
            owner.outstanding += 1;
            owner.pending.push_back(Request {
                key: RunControlReadKeyWithParent {
                    key: RunControlReadKey {
                        user_id: user_id.into(),
                        run_id: run_id.into(),
                    },
                    parent_hint: parent_hint
                        .filter(|hint| hint.len() <= RUN_CONTROL_READ_MAX_KEY_BYTES)
                        .map(str::to_owned),
                },
                collect_at: queued_at + COLLECTION_WINDOW,
                reply,
            });
            if was_empty {
                state.ready.push_back(user_id.to_owned());
            }
            state.outstanding += 1;
            let start = state.workers < MAX_WORKERS;
            if start {
                state.workers += 1;
            }
            start
        };
        if start_worker {
            let reads = self.clone();
            tokio::spawn(async move { reads.run().await });
        }
        tokio::time::timeout_at(queued_at + READ_TIMEOUT, receiver)
            .await
            .map_err(|_| "run control read timed out".to_string())?
            .map_err(|_| "run control read worker closed".to_string())?
    }

    async fn run(self: Arc<Self>) {
        loop {
            let collect_at = {
                let state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let queued = state
                    .owners
                    .values()
                    .map(|owner| owner.pending.len())
                    .sum::<usize>();
                if queued >= RUN_CONTROL_READ_BATCH_SIZE {
                    None
                } else {
                    state
                        .owners
                        .values()
                        .filter_map(|owner| owner.pending.front())
                        .map(|request| request.collect_at)
                        .min()
                }
            };
            if let Some(deadline) = collect_at {
                // Collection is bounded from admission, not renewed every
                // time a worker reaches an already-waiting partial batch.
                tokio::time::sleep_until(deadline).await;
            }
            let batch = {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let batch = state.take_batch();
                if batch.is_empty() {
                    state.workers -= 1;
                    return;
                }
                batch
            };
            // Freeze identities before SQL; later callers always get a fresh
            // observation, never one captured before their request.
            let keys = batch.iter().map(|request| request.key.clone()).collect();
            let outcome = tokio::time::timeout(
                QUERY_TIMEOUT,
                AssertUnwindSafe(async { (self.read)(keys).await }).catch_unwind(),
            )
            .await;
            let results = match outcome {
                Ok(Ok(Ok(results))) if results.len() == batch.len() => Ok(results),
                Ok(Ok(Ok(_))) => {
                    Err("run control batch returned an invalid result count".to_string())
                }
                Ok(Ok(Err(error))) => Err(error),
                Ok(Err(_)) => Err("run control batch panicked".to_string()),
                Err(_) => Err("run control batch query timed out".to_string()),
            };
            {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                for request in &batch {
                    state.release(&request.key.key.user_id);
                }
            }
            match results {
                Ok(results) => {
                    for (request, result) in batch.into_iter().zip(results) {
                        let _ = request.reply.send(result);
                    }
                }
                Err(error) => {
                    for request in batch {
                        let _ = request.reply.send(Err(error.clone()));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_services::runs::RunControlReadWithParent;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    fn empty_results(count: usize) -> Vec<RunControlReadWithParentResult> {
        vec![
            Ok(RunControlReadWithParent {
                own: None,
                parent: None
            });
            count
        ]
    }

    fn reads(
        read: impl Fn(
            Vec<RunControlReadKey>,
        ) -> BoxFuture<'static, Result<Vec<RunControlReadResult>, String>>
        + Send
        + Sync
        + 'static,
    ) -> Arc<ControlReads> {
        let read = Arc::new(read);
        Arc::new(ControlReads {
            read: Arc::new(move |keys| {
                let read = read.clone();
                Box::pin(async move {
                    let original = keys.into_iter().map(|key| key.key).collect();
                    Ok(read(original)
                        .await?
                        .into_iter()
                        .map(|result| {
                            result.map(|own| RunControlReadWithParent { own, parent: None })
                        })
                        .collect())
                })
            }),
            state: Mutex::new(State::default()),
        })
    }

    #[tokio::test(start_paused = true)]
    async fn thousand_owners_share_eight_bounded_reads_without_idle_polling() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let reader = reads(move |keys| {
            counter.fetch_add(1, Ordering::SeqCst);
            assert!(keys.len() <= RUN_CONTROL_READ_BATCH_SIZE);
            Box::pin(async move { Ok(vec![Ok(None); keys.len()]) })
        });
        let requests = (0..1000).map(|i| {
            let reader = reader.clone();
            async move {
                reader
                    .get(&format!("owner-{i}"), &format!("run-{i}"))
                    .await
                    .unwrap()
            }
        });
        let result = futures_util::future::join_all(requests).await;
        assert!(result.into_iter().all(|result| result.is_none()));
        assert_eq!(calls.load(Ordering::SeqCst), 8);
        let staggered = (0..1000).map(|i| {
            let reader = reader.clone();
            async move {
                tokio::time::sleep(Duration::from_millis(i)).await;
                reader
                    .get(&format!("owner-{i}"), &format!("run-{i}"))
                    .await
                    .unwrap()
            }
        });
        assert!(
            futures_util::future::join_all(staggered)
                .await
                .into_iter()
                .all(|result| result.is_none())
        );
        let after_stagger = calls.load(Ordering::SeqCst);
        assert!(
            after_stagger - 8 < 250,
            "staggered arrivals must also coalesce"
        );
        tokio::time::sleep(Duration::from_secs(10)).await;
        assert_eq!(calls.load(Ordering::SeqCst), after_stagger);
        let state = reader.state.lock().unwrap();
        assert_eq!((state.outstanding, state.workers), (0, 0));
        assert!(state.owners.is_empty() && state.ready.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn parent_hints_are_scoped_to_each_request_and_bounded() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let reader = Arc::new(ControlReads {
            read: Arc::new(move |keys| {
                counter.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move {
                    Ok(keys
                        .into_iter()
                        .map(|key| {
                            Ok(RunControlReadWithParent {
                                own: None,
                                parent: key.parent_hint.map(Err),
                            })
                        })
                        .collect())
                })
            }),
            state: Mutex::new(State::default()),
        });
        let oversized = "x".repeat(RUN_CONTROL_READ_MAX_KEY_BYTES + 1);
        let (first, second) = tokio::join!(
            reader.get_with_parent("owner-a", "run-a", Some("parent-a")),
            reader.get_with_parent("owner-b", "run-b", Some(&oversized)),
        );
        assert_eq!(first.unwrap().parent.unwrap().unwrap_err(), "parent-a");
        assert!(second.unwrap().parent.is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn joined_failure_does_not_delay_unhinted_observation() {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let (start, gate, counter) = (entered.clone(), release.clone(), calls.clone());
        let reader = Arc::new(ControlReads {
            read: Arc::new(move |keys| {
                counter.fetch_add(1, Ordering::SeqCst);
                let joined = keys.iter().any(|key| key.parent_hint.is_some());
                let (start, gate) = (start.clone(), gate.clone());
                Box::pin(async move {
                    if joined {
                        assert!(keys.iter().all(|key| key.parent_hint.is_some()));
                        start.notify_one();
                        gate.notified().await;
                        Err("injected joined failure".into())
                    } else {
                        Ok(empty_results(keys.len()))
                    }
                })
            }),
            state: Mutex::new(State::default()),
        });
        let mut children = (0..4)
            .map(|_| Box::pin(reader.get_with_parent("child-owner", "child", Some("parent"))))
            .collect::<Vec<_>>();
        for child in &mut children {
            assert!(futures_util::poll!(child.as_mut()).is_pending());
        }
        let mut root = Box::pin(reader.get("root-owner", "root"));
        assert!(futures_util::poll!(root.as_mut()).is_pending());

        tokio::time::advance(COLLECTION_WINDOW).await;
        tokio::time::timeout(Duration::from_secs(1), entered.notified())
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), root)
                .await
                .unwrap()
                .unwrap()
                .is_none()
        );
        assert_eq!(reader.state.lock().unwrap().outstanding, 4);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        release.notify_one();
        assert!(
            futures_util::future::join_all(children)
                .await
                .into_iter()
                .all(|result| result.unwrap_err() == "injected joined failure")
        );
        assert_eq!(reader.state.lock().unwrap().outstanding, 0);
        assert!(reader.get("root-owner", "root").await.unwrap().is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn sparse_hints_remain_batched_and_mixed_groups_keep_owner_order() {
        for hint_count in [3, 4] {
            let batches = Arc::new(Mutex::new(Vec::new()));
            let seen = batches.clone();
            let reader = Arc::new(ControlReads {
                read: Arc::new(move |keys| {
                    seen.lock().unwrap().push(
                        keys.iter()
                            .map(|key| (key.key.run_id.clone(), key.parent_hint.is_some()))
                            .collect::<Vec<_>>(),
                    );
                    Box::pin(async move { Ok(empty_results(keys.len())) })
                }),
                state: Mutex::new(State::default()),
            });
            let ids = (0..8).map(|i| format!("run-{i}")).collect::<Vec<_>>();
            let mut pending = ids
                .iter()
                .enumerate()
                .map(|(i, id)| {
                    let hint = (i % 2 == 0 && i / 2 < hint_count).then_some("parent");
                    Box::pin(reader.get_with_parent("owner", id, hint))
                })
                .collect::<Vec<_>>();
            for request in &mut pending {
                assert!(futures_util::poll!(request.as_mut()).is_pending());
            }
            assert!(
                futures_util::future::join_all(pending)
                    .await
                    .into_iter()
                    .all(|result| result.unwrap().own.is_none())
            );
            let batches = batches.lock().unwrap();
            if hint_count == 3 {
                assert_eq!(batches.len(), 1, "sparse hints must not add a query");
                assert_eq!(batches[0].len(), 8);
            } else {
                assert_eq!(batches.len(), 2);
                assert_eq!(
                    batches[0]
                        .iter()
                        .map(|(id, _)| id.as_str())
                        .collect::<Vec<_>>(),
                    vec!["run-0", "run-2", "run-4", "run-6"]
                );
                assert_eq!(
                    batches[1]
                        .iter()
                        .map(|(id, _)| id.as_str())
                        .collect::<Vec<_>>(),
                    vec!["run-1", "run-3", "run-5", "run-7"]
                );
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn deferred_hints_get_a_turn_under_other_owners_backlogs() {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let open = Arc::new(AtomicBool::new(false));
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let (start, gate, ready, in_flight, max_seen) = (
            entered.clone(),
            release.clone(),
            open.clone(),
            active.clone(),
            peak.clone(),
        );
        let reader = Arc::new(ControlReads {
            read: Arc::new(move |keys| {
                let (start, gate, ready, in_flight, max_seen) = (
                    start.clone(),
                    gate.clone(),
                    ready.clone(),
                    in_flight.clone(),
                    max_seen.clone(),
                );
                Box::pin(async move {
                    let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    max_seen.fetch_max(now, Ordering::SeqCst);
                    if keys.iter().any(|key| key.parent_hint.is_some()) {
                        start.notify_one();
                    }
                    if !ready.load(Ordering::SeqCst) {
                        gate.notified().await;
                    }
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                    Ok(empty_results(keys.len()))
                })
            }),
            state: Mutex::new(State::default()),
        });
        let mut pending: Vec<BoxFuture<'static, ()>> = Vec::new();
        for i in 0..124 {
            for _ in 0..4 {
                let (reader, owner) = (reader.clone(), format!("root-{i}"));
                let mut request: BoxFuture<'static, ()> = Box::pin(async move {
                    reader.get(&owner, "root").await.unwrap();
                });
                assert!(futures_util::poll!(request.as_mut()).is_pending());
                pending.push(request);
            }
        }
        for i in 0..4 {
            let (reader, owner) = (reader.clone(), format!("child-{i}"));
            let mut request: BoxFuture<'static, ()> = Box::pin(async move {
                reader
                    .get_with_parent(&owner, "child", Some("parent"))
                    .await
                    .unwrap();
            });
            assert!(futures_util::poll!(request.as_mut()).is_pending());
            pending.push(request);
        }
        tokio::time::advance(COLLECTION_WINDOW).await;
        tokio::time::timeout(Duration::from_millis(100), entered.notified())
            .await
            .expect("deferred hints must not be starved by ready owners");
        assert!(peak.load(Ordering::SeqCst) <= MAX_WORKERS);
        open.store(true, Ordering::SeqCst);
        release.notify_waiters();
        futures_util::future::join_all(pending).await;
        assert!(peak.load(Ordering::SeqCst) <= MAX_WORKERS);
        assert_eq!(reader.state.lock().unwrap().outstanding, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn late_request_does_not_share_an_already_started_observation() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let (start, gate) = (entered.clone(), release.clone());
        let reader = reads(move |_| {
            let ordinal = counter.fetch_add(1, Ordering::SeqCst);
            let (start, gate) = (start.clone(), gate.clone());
            Box::pin(async move {
                if ordinal == 0 {
                    start.notify_one();
                    gate.notified().await;
                }
                Ok(vec![Err(format!("observation-{ordinal}"))])
            })
        });
        let first = {
            let reader = reader.clone();
            tokio::spawn(async move { reader.get("owner", "run").await })
        };
        entered.notified().await;
        assert_eq!(
            reader.get("owner", "run").await.unwrap_err(),
            "observation-1"
        );
        release.notify_one();
        assert_eq!(first.await.unwrap().unwrap_err(), "observation-0");
        assert_eq!(
            reader.get("owner", "run").await.unwrap_err(),
            "observation-2"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn failures_release_capacity_and_do_not_poison_neighboring_results() {
        let reader = reads(|keys| {
            if keys[0].run_id == "factory-panic" {
                panic!("injected factory panic");
            }
            Box::pin(async move {
                match keys[0].run_id.as_str() {
                    "sql" => Err("unavailable".into()),
                    "hang" => std::future::pending().await,
                    "panic" => panic!("injected control read panic"),
                    "short" => Ok(Vec::new()),
                    _ => Ok(keys
                        .iter()
                        .map(|key| {
                            if key.run_id == "bad" {
                                Err("malformed".into())
                            } else {
                                Ok(None)
                            }
                        })
                        .collect()),
                }
            })
        });
        for failure in ["sql", "hang", "panic", "factory-panic", "short"] {
            assert!(reader.get("owner", failure).await.is_err());
            assert_eq!(reader.state.lock().unwrap().outstanding, 0);
            assert!(reader.get("owner", "good").await.unwrap().is_none());
        }
        let (bad, good) = tokio::join!(reader.get("owner", "bad"), reader.get("other", "good"));
        assert_eq!(bad.unwrap_err(), "malformed");
        assert!(good.unwrap().is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_and_owner_global_limits_release_all_retained_requests() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let reader = reads(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { panic!("cancelled requests must not reach SQL") })
        });
        let mut pending = Vec::new();
        for owner in ["hot", "cold"] {
            for _ in 0..MAX_OWNER_READS {
                let mut request = Box::pin(reader.get(owner, "run"));
                assert!(futures_util::poll!(request.as_mut()).is_pending());
                pending.push(request);
            }
            assert!(
                reader
                    .get(owner, "overflow")
                    .await
                    .unwrap_err()
                    .contains("capacity")
            );
        }
        assert!(reader.get("third", "overflow").await.is_err());
        assert_eq!(reader.state.lock().unwrap().outstanding, MAX_READS);
        drop(pending);
        tokio::time::sleep(COLLECTION_WINDOW * 2).await;
        let state = reader.state.lock().unwrap();
        assert_eq!((state.outstanding, state.workers), (0, 0));
        assert!(state.owners.is_empty() && state.ready.is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn slow_owner_cannot_exceed_workers_or_starve_a_later_owner() {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let ready = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let (start, gate, open, counter) = (
            entered.clone(),
            release.clone(),
            ready.clone(),
            calls.clone(),
        );
        let reader = reads(move |keys| {
            let (start, gate, open, counter) =
                (start.clone(), gate.clone(), open.clone(), counter.clone());
            Box::pin(async move {
                let ordinal = counter.fetch_add(1, Ordering::SeqCst);
                if keys.iter().any(|key| key.user_id == "cold") {
                    assert_eq!(
                        ordinal, MAX_WORKERS,
                        "the next batch must include the later owner"
                    );
                }
                if ordinal + 1 == MAX_WORKERS {
                    start.notify_one();
                }
                if !open.load(Ordering::SeqCst) {
                    gate.notified().await;
                }
                Ok(vec![Ok(None); keys.len()])
            })
        });
        let mut pending = Vec::new();
        for _ in 0..1000 {
            let mut request = Box::pin(reader.get("hot", "run"));
            assert!(futures_util::poll!(request.as_mut()).is_pending());
            pending.push(request);
        }
        entered.notified().await;
        assert_eq!(calls.load(Ordering::SeqCst), MAX_WORKERS);
        tokio::time::advance(COLLECTION_WINDOW).await;
        let mut cold = Box::pin(reader.get("cold", "run"));
        assert!(futures_util::poll!(cold.as_mut()).is_pending());
        let released_at = Instant::now();
        ready.store(true, Ordering::SeqCst);
        release.notify_waiters();
        assert!(cold.await.unwrap().is_none());
        assert!(
            futures_util::future::join_all(pending)
                .await
                .into_iter()
                .all(|result| result.unwrap().is_none())
        );
        assert_eq!(calls.load(Ordering::SeqCst), 8);
        assert_eq!(
            Instant::now(),
            released_at,
            "aged backlog must not acquire another collection window"
        );
    }
}
