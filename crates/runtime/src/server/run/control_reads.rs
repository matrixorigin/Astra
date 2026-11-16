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
    RunControlBatchError, RunControlReadKey, RunControlReadKeyWithParent, RunControlReadResult,
    RunControlReadWithParent, RunControlReadWithParentResult, RunStateStore,
};
use futures_util::{FutureExt, future::BoxFuture};
use tokio::{sync::oneshot, time::Instant};

const COLLECTION_WINDOW: Duration = Duration::from_millis(25);
const QUERY_TIMEOUT: Duration = Duration::from_secs(3);
const READ_TIMEOUT: Duration = Duration::from_secs(4);
const MAX_READS: usize = 8192;
const MAX_OWNER_READS: usize = 4096;
const MAX_WORKERS: usize = 4;
const MAX_JOINED_WORKERS: usize = MAX_WORKERS - 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReadMode {
    Plain,
    Joined,
    Fallback,
}

type Read = dyn Fn(
        Vec<RunControlReadKeyWithParent>,
        ReadMode,
    ) -> BoxFuture<'static, Result<Vec<RunControlReadWithParentResult>, RunControlBatchError>>
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
    joined_workers: usize,
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

    fn collection_deadline(&self) -> Option<Instant> {
        if self.joined_workers < MAX_JOINED_WORKERS {
            let queued = self
                .owners
                .values()
                .map(|owner| owner.pending.len())
                .sum::<usize>();
            if queued >= RUN_CONTROL_READ_BATCH_SIZE {
                return None;
            }
            return self
                .owners
                .values()
                .filter_map(|owner| owner.pending.front())
                .map(|request| request.collect_at)
                .min();
        }

        // Hinted work cannot use the reserved worker. Count only eligible
        // requests so a large hinted backlog does not defeat root batching.
        let mut queued = 0;
        let mut earliest: Option<Instant> = None;
        for request in self.owners.values().flat_map(|owner| owner.pending.iter()) {
            if request.key.parent_hint.is_none() && !request.reply.is_closed() {
                queued += 1;
                if queued >= RUN_CONTROL_READ_BATCH_SIZE {
                    return None;
                }
                earliest =
                    Some(earliest.map_or(request.collect_at, |at| at.min(request.collect_at)));
            }
        }
        earliest
    }

    fn has_unhinted(&self) -> bool {
        self.owners.values().any(|owner| {
            owner
                .pending
                .iter()
                .any(|request| request.key.parent_hint.is_none() && !request.reply.is_closed())
        })
    }

    fn take_batch(&mut self, unhinted_only: bool) -> (Vec<Request>, bool) {
        let mut batch = Vec::with_capacity(RUN_CONTROL_READ_BATCH_SIZE);
        let mut skipped = VecDeque::new();
        while batch.len() < RUN_CONTROL_READ_BATCH_SIZE {
            let Some(user_id) = self.ready.pop_front() else {
                break;
            };
            let owner = self.owners.get_mut(&user_id).expect("queued control owner");
            let request = if unhinted_only {
                // An unhinted request may be behind this owner's hinted work.
                owner
                    .pending
                    .extract_if(|request| {
                        request.key.parent_hint.is_none() || request.reply.is_closed()
                    })
                    .next()
            } else {
                owner.pending.pop_front()
            };
            let pending = !owner.pending.is_empty();
            if let Some(request) = request {
                if pending {
                    self.ready.push_back(user_id.clone());
                }
                if request.reply.is_closed() {
                    self.release(&user_id);
                } else {
                    batch.push(request);
                }
            } else if unhinted_only && pending {
                // Do not revisit hint-only owners until this batch is built.
                skipped.push_back(user_id);
            } else {
                unreachable!("queued control request");
            }
        }
        if unhinted_only {
            skipped.append(&mut self.ready);
            self.ready = skipped;
            return (batch, false);
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
            return (batch, hinted >= RUN_CONTROL_READ_MIN_PARENT_HINTS);
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
        (selected, take_hinted)
    }
}

impl ControlReads {
    pub(super) fn new(store: Arc<dyn RunStateStore>) -> Arc<Self> {
        Arc::new(Self {
            read: Arc::new(move |keys, mode| {
                let store = store.clone();
                Box::pin(async move {
                    match mode {
                        ReadMode::Joined => store.load_run_control_batch_with_parents(&keys).await,
                        ReadMode::Fallback => {
                            store
                                .load_run_control_batch_after_parent_failure(&keys)
                                .await
                        }
                        ReadMode::Plain => {
                            // A custom store may prefetch even one hint. Plain
                            // batches must never share that failure domain.
                            let keys = keys.into_iter().map(|key| key.key).collect::<Vec<_>>();
                            Ok(store
                                .load_run_control_batch(&keys)
                                .await
                                .map_err(RunControlBatchError::Invalid)?
                                .into_iter()
                                .map(|result| {
                                    result.map(|own| RunControlReadWithParent { own, parent: None })
                                })
                                .collect())
                        }
                    }
                })
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
        let parent_hint = parent_hint.filter(|hint| hint.len() <= RUN_CONTROL_READ_MAX_KEY_BYTES);
        // Keep one batch of admission capacity for unhinted observations,
        // including when joined reads have already filled the hinted backlog.
        let reserve = if parent_hint.is_some() {
            RUN_CONTROL_READ_BATCH_SIZE
        } else {
            0
        };
        let queued_at = Instant::now();
        let (reply, receiver) = oneshot::channel();
        let start_worker = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.outstanding >= MAX_READS - reserve
                || state
                    .owners
                    .get(user_id)
                    .is_some_and(|owner| owner.outstanding >= MAX_OWNER_READS - reserve)
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
                    parent_hint: parent_hint.map(str::to_owned),
                },
                collect_at: queued_at + COLLECTION_WINDOW,
                reply,
            });
            if was_empty {
                state.ready.push_back(user_id.to_owned());
            }
            state.outstanding += 1;
            let start = state.workers < MAX_WORKERS
                && (state.joined_workers < MAX_JOINED_WORKERS || parent_hint.is_none());
            if start {
                state.workers += 1;
            }
            start
        };
        if start_worker {
            self.spawn_worker();
        }
        tokio::time::timeout_at(queued_at + READ_TIMEOUT, receiver)
            .await
            .map_err(|_| "run control read timed out".to_string())?
            .map_err(|_| "run control read worker closed".to_string())?
    }

    fn spawn_worker(self: &Arc<Self>) {
        let reads = self.clone();
        tokio::spawn(async move { reads.run().await });
    }

    async fn run(self: Arc<Self>) {
        loop {
            let selection = {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                // Check the current eligible class and its original collection
                // deadline under the same lock used to select the batch.
                if let Some(deadline) = state
                    .collection_deadline()
                    .filter(|at| *at > Instant::now())
                {
                    Err(deadline)
                } else {
                    let unhinted_only = state.joined_workers >= MAX_JOINED_WORKERS;
                    let (batch, joined) = state.take_batch(unhinted_only);
                    if batch.is_empty() {
                        state.workers -= 1;
                        return;
                    }
                    if joined {
                        state.joined_workers += 1;
                    }
                    // A mixed selection can restore a previously queued root
                    // as the third joined worker starts.
                    let start = joined
                        && state.joined_workers == MAX_JOINED_WORKERS
                        && state.workers < MAX_WORKERS
                        && state.has_unhinted();
                    if start {
                        state.workers += 1;
                    }
                    Ok((batch, joined, start))
                }
            };
            let (batch, joined, start_reserved) = match selection {
                Ok(selection) => selection,
                Err(deadline) => {
                    tokio::time::sleep_until(deadline).await;
                    continue;
                }
            };
            if start_reserved {
                self.spawn_worker();
            }
            // Freeze identities before SQL; later callers always get a fresh
            // observation, never one captured before their request.
            let keys = batch.iter().map(|request| request.key.clone()).collect();
            let mode = if joined {
                ReadMode::Joined
            } else {
                ReadMode::Plain
            };
            let outcome = tokio::time::timeout(
                QUERY_TIMEOUT,
                AssertUnwindSafe(async { (self.read)(keys, mode).await }).catch_unwind(),
            )
            .await;
            let (mut results, recover) = match outcome {
                Ok(Ok(Ok(results))) if results.len() == batch.len() => (Ok(results), false),
                Ok(Ok(Ok(_))) => (
                    Err("run control batch returned an invalid result count".to_string()),
                    false,
                ),
                Ok(Ok(Err(RunControlBatchError::Unavailable(error)))) => (Err(error), joined),
                Ok(Ok(Err(error))) => (Err(error.to_string()), false),
                Ok(Err(_)) => (Err("run control batch panicked".to_string()), false),
                Err(_) => (Err("run control batch query timed out".to_string()), joined),
            };
            if recover {
                let now = Instant::now();
                let live = batch
                    .iter()
                    .enumerate()
                    .filter_map(|(index, request)| {
                        let deadline = request.collect_at + (READ_TIMEOUT - COLLECTION_WINDOW);
                        (!request.reply.is_closed() && deadline > now).then_some((index, deadline))
                    })
                    .collect::<Vec<_>>();
                if let Some(deadline) = live.iter().map(|(_, deadline)| *deadline).max() {
                    let keys = live
                        .iter()
                        .map(|(index, _)| batch[*index].key.clone())
                        .collect();
                    // Retain this batch's admission and joined-worker slot.
                    // Each caller keeps its original deadline, and no second
                    // joined query is issued for the uncertain first read.
                    let deadline = deadline.min(now + QUERY_TIMEOUT);
                    let fallback = tokio::time::timeout_at(
                        deadline,
                        AssertUnwindSafe(async { (self.read)(keys, ReadMode::Fallback).await })
                            .catch_unwind(),
                    )
                    .await;
                    let original = results.as_ref().unwrap_err().clone();
                    let fallback_error = match fallback {
                        Ok(Ok(Ok(recovered))) if recovered.len() == live.len() => {
                            tracing::warn!(
                                outcome = "recovered",
                                requests = live.len(),
                                joined_error = %original,
                                "joined run control batch required recovery"
                            );
                            let mut merged = vec![Err(original.clone()); batch.len()];
                            for ((index, _), result) in live.into_iter().zip(recovered) {
                                merged[index] = result;
                            }
                            results = Ok(merged);
                            None
                        }
                        Ok(Ok(Err(RunControlBatchError::Unsupported))) => None,
                        Ok(Ok(Err(error))) => Some(error.to_string()),
                        Ok(Ok(Ok(_))) => Some("invalid fallback result count".to_string()),
                        Ok(Err(_)) => Some("fallback panicked".to_string()),
                        Err(_) => Some("fallback timed out".to_string()),
                    };
                    if let Some(error) = fallback_error {
                        tracing::warn!(
                            outcome = "failed",
                            joined_error = %original,
                            fallback_error = %error,
                            "joined run control batch recovery failed"
                        );
                        results = Err(format!("{original}; fallback failed: {error}"));
                    }
                }
            }
            {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if joined {
                    state.joined_workers -= 1;
                }
                for request in &batch {
                    state.release(&request.key.key.user_id);
                }
            }
            let respond = |request: Request, result: RunControlReadWithParentResult| {
                let deadline = request.collect_at + (READ_TIMEOUT - COLLECTION_WINDOW);
                let result = if Instant::now() >= deadline {
                    Err("run control read timed out".to_string())
                } else {
                    result
                };
                let _ = request.reply.send(result);
            };
            match results {
                Ok(results) => {
                    for (request, result) in batch.into_iter().zip(results) {
                        respond(request, result);
                    }
                }
                Err(error) => {
                    for request in batch {
                        respond(request, Err(error.clone()));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
            read: Arc::new(move |keys, _mode| {
                let read = read.clone();
                Box::pin(async move {
                    let original = keys.into_iter().map(|key| key.key).collect();
                    Ok(read(original)
                        .await
                        .map_err(RunControlBatchError::Invalid)?
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
            read: Arc::new(move |keys, _mode| {
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
            read: Arc::new(move |keys, mode| {
                counter.fetch_add(1, Ordering::SeqCst);
                let joined = mode == ReadMode::Joined;
                let (start, gate) = (start.clone(), gate.clone());
                Box::pin(async move {
                    if joined {
                        assert!(keys.iter().all(|key| key.parent_hint.is_some()));
                        start.notify_one();
                        gate.notified().await;
                        Err(RunControlBatchError::Unavailable(
                            "injected joined failure".into(),
                        ))
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
                .all(|result| result.unwrap().own.is_none())
        );
        assert_eq!(reader.state.lock().unwrap().outstanding, 0);
        assert!(reader.get("root-owner", "root").await.unwrap().is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn joined_timeout_recovers_live_requests_with_original_deadlines() {
        let joined_started = Arc::new(tokio::sync::Notify::new());
        let fallback_started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (joined, fallback, gate, calls) = (
            joined_started.clone(),
            fallback_started.clone(),
            release.clone(),
            seen.clone(),
        );
        let reader = Arc::new(ControlReads {
            read: Arc::new(move |keys, mode| {
                calls.lock().unwrap().push((
                    mode,
                    keys.iter()
                        .map(|key| key.key.run_id.clone())
                        .collect::<Vec<_>>(),
                ));
                let (joined, fallback, gate) = (joined.clone(), fallback.clone(), gate.clone());
                Box::pin(async move {
                    match mode {
                        ReadMode::Joined => {
                            joined.notify_one();
                            std::future::pending().await
                        }
                        ReadMode::Fallback => {
                            fallback.notify_one();
                            gate.notified().await;
                            Ok(empty_results(keys.len()))
                        }
                        ReadMode::Plain => panic!("unexpected plain batch"),
                    }
                })
            }),
            state: Mutex::new(State::default()),
        });
        let mut early = Box::pin(reader.get_with_parent("owner-0", "early", Some("parent")));
        assert!(futures_util::poll!(early.as_mut()).is_pending());
        tokio::time::advance(Duration::from_millis(10)).await;
        let mut a = Box::pin(reader.get_with_parent("owner-1", "a", Some("parent")));
        let mut b = Box::pin(reader.get_with_parent("owner-2", "b", Some("parent")));
        let mut c = Box::pin(reader.get_with_parent("owner-3", "c", Some("parent")));
        let mut dropped = Box::pin(reader.get_with_parent("owner-4", "dropped", Some("parent")));
        for request in [&mut a, &mut b, &mut c, &mut dropped] {
            assert!(futures_util::poll!(request.as_mut()).is_pending());
        }
        tokio::time::advance(Duration::from_millis(15)).await;
        joined_started.notified().await;
        drop(dropped);
        tokio::time::advance(QUERY_TIMEOUT).await;
        fallback_started.notified().await;
        {
            let calls = seen.lock().unwrap();
            assert_eq!(calls.len(), 2);
            assert_eq!(calls[0].0, ReadMode::Joined);
            assert_eq!(calls[1].0, ReadMode::Fallback);
            assert_eq!(calls[1].1.len(), 4);
            assert!(!calls[1].1.iter().any(|run| run == "dropped"));
        }
        // The earliest caller still expires at its original deadline; the
        // slightly later callers can use the same in-flight recovery result.
        tokio::time::advance(Duration::from_millis(975)).await;
        tokio::time::advance(Duration::from_millis(5)).await;
        release.notify_one();
        // Let recovery publish before polling the expired caller. A ready
        // oneshot must not turn its elapsed deadline into a late success.
        tokio::time::timeout(Duration::from_millis(1), async {
            while reader.state.lock().unwrap().outstanding != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let (early, a, b, c) = tokio::join!(early, a, b, c);
        assert!(early.unwrap_err().contains("timed out"));
        assert!(a.unwrap().own.is_none());
        assert!(b.unwrap().own.is_none());
        assert!(c.unwrap().own.is_none());
        assert_eq!(reader.state.lock().unwrap().outstanding, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn fallback_waiting_for_a_pool_slot_stops_at_the_original_deadline() {
        let joined_started = Arc::new(tokio::sync::Notify::new());
        let fallback_started = Arc::new(tokio::sync::Notify::new());
        let pool = Arc::new(tokio::sync::Semaphore::new(0));
        let (joined, fallback, pool_slot) = (
            joined_started.clone(),
            fallback_started.clone(),
            pool.clone(),
        );
        let reader = Arc::new(ControlReads {
            read: Arc::new(move |_keys, mode| {
                let (joined, fallback, pool_slot) =
                    (joined.clone(), fallback.clone(), pool_slot.clone());
                Box::pin(async move {
                    match mode {
                        ReadMode::Joined => {
                            joined.notify_one();
                            std::future::pending().await
                        }
                        ReadMode::Fallback => {
                            fallback.notify_one();
                            let _permit = pool_slot.acquire().await.unwrap();
                            unreachable!("the pool stays exhausted")
                        }
                        ReadMode::Plain => unreachable!(),
                    }
                })
            }),
            state: Mutex::new(State::default()),
        });
        let mut requests = (0..4)
            .map(|_| Box::pin(reader.get_with_parent("owner", "child", Some("parent"))))
            .collect::<Vec<_>>();
        for request in &mut requests {
            assert!(futures_util::poll!(request.as_mut()).is_pending());
        }
        tokio::time::advance(COLLECTION_WINDOW).await;
        joined_started.notified().await;
        tokio::time::advance(QUERY_TIMEOUT).await;
        fallback_started.notified().await;
        tokio::time::advance(READ_TIMEOUT - COLLECTION_WINDOW - QUERY_TIMEOUT).await;
        let results = futures_util::future::join_all(requests).await;
        assert!(
            results
                .into_iter()
                .all(|result| result.unwrap_err().contains("timed out"))
        );
        tokio::task::yield_now().await;
        assert_eq!(reader.state.lock().unwrap().outstanding, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn failed_fallback_preserves_both_errors_and_releases_capacity() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let seen = calls.clone();
        let reader = Arc::new(ControlReads {
            read: Arc::new(move |keys, mode| {
                seen.lock().unwrap().push(mode);
                Box::pin(async move {
                    match mode {
                        ReadMode::Joined => Err(RunControlBatchError::Unavailable(
                            "joined unavailable".into(),
                        )),
                        ReadMode::Fallback => Err(RunControlBatchError::Unavailable(
                            "fallback unavailable".into(),
                        )),
                        ReadMode::Plain => Ok(empty_results(keys.len())),
                    }
                })
            }),
            state: Mutex::new(State::default()),
        });
        let results = futures_util::future::join_all((0..4).map(|i| {
            let reader = reader.clone();
            async move {
                reader
                    .get_with_parent(&format!("owner-{i}"), "child", Some("parent"))
                    .await
            }
        }))
        .await;
        assert!(results.into_iter().all(|result| result.unwrap_err()
            == "joined unavailable; fallback failed: fallback unavailable"));
        assert_eq!(
            *calls.lock().unwrap(),
            vec![ReadMode::Joined, ReadMode::Fallback]
        );
        assert_eq!(reader.state.lock().unwrap().outstanding, 0);
        assert!(reader.get("owner", "root").await.unwrap().is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn invalid_results_and_unsupported_recovery_preserve_original_errors() {
        for invalid in [true, false] {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let seen = calls.clone();
            let reader = Arc::new(ControlReads {
                read: Arc::new(move |_keys, mode| {
                    seen.lock().unwrap().push(mode);
                    Box::pin(async move {
                        match mode {
                            ReadMode::Joined if invalid => {
                                Err(RunControlBatchError::Invalid("bad ordinal".into()))
                            }
                            ReadMode::Joined => Err(RunControlBatchError::Unavailable(
                                "joined unavailable".into(),
                            )),
                            ReadMode::Fallback => Err(RunControlBatchError::Unsupported),
                            ReadMode::Plain => panic!("unexpected plain batch"),
                        }
                    })
                }),
                state: Mutex::new(State::default()),
            });
            let results = futures_util::future::join_all((0..4).map(|i| {
                let reader = reader.clone();
                async move {
                    reader
                        .get_with_parent(&format!("owner-{i}"), "child", Some("parent"))
                        .await
                }
            }))
            .await;
            let expected = if invalid {
                "bad ordinal"
            } else {
                "joined unavailable"
            };
            assert!(
                results
                    .into_iter()
                    .all(|result| result.unwrap_err() == expected)
            );
            let expected_calls = if invalid {
                vec![ReadMode::Joined]
            } else {
                vec![ReadMode::Joined, ReadMode::Fallback]
            };
            assert_eq!(*calls.lock().unwrap(), expected_calls);
            assert_eq!(reader.state.lock().unwrap().outstanding, 0);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn abandoned_joined_batch_does_not_start_a_fallback() {
        let joined_started = Arc::new(tokio::sync::Notify::new());
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (start, seen) = (joined_started.clone(), calls.clone());
        let reader = Arc::new(ControlReads {
            read: Arc::new(move |_keys, mode| {
                seen.lock().unwrap().push(mode);
                let start = start.clone();
                Box::pin(async move {
                    assert_eq!(mode, ReadMode::Joined);
                    start.notify_one();
                    std::future::pending().await
                })
            }),
            state: Mutex::new(State::default()),
        });
        let mut requests = (0..4)
            .map(|_| Box::pin(reader.get_with_parent("owner", "child", Some("parent"))))
            .collect::<Vec<_>>();
        for request in &mut requests {
            assert!(futures_util::poll!(request.as_mut()).is_pending());
        }
        tokio::time::advance(COLLECTION_WINDOW).await;
        joined_started.notified().await;
        drop(requests);
        tokio::time::advance(QUERY_TIMEOUT).await;
        tokio::task::yield_now().await;
        assert_eq!(*calls.lock().unwrap(), vec![ReadMode::Joined]);
        assert_eq!(reader.state.lock().unwrap().outstanding, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn sparse_hints_remain_batched_and_mixed_groups_keep_owner_order() {
        for hint_count in [3, 4] {
            let batches = Arc::new(Mutex::new(Vec::new()));
            let seen = batches.clone();
            let reader = Arc::new(ControlReads {
                read: Arc::new(move |keys, _mode| {
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
            read: Arc::new(move |keys, _mode| {
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
    async fn reserved_worker_batches_roots_behind_hints_and_recovers() {
        let joined_started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let open = Arc::new(AtomicBool::new(false));
        let joined_calls = Arc::new(AtomicUsize::new(0));
        let plain_calls = Arc::new(AtomicUsize::new(0));
        let (started, gate, opened, joined_count, plain_count) = (
            joined_started.clone(),
            release.clone(),
            open.clone(),
            joined_calls.clone(),
            plain_calls.clone(),
        );
        let reader = Arc::new(ControlReads {
            read: Arc::new(move |keys, mode| {
                let joined = mode == ReadMode::Joined;
                let (started, gate, opened, joined_count, plain_count) = (
                    started.clone(),
                    gate.clone(),
                    opened.clone(),
                    joined_count.clone(),
                    plain_count.clone(),
                );
                Box::pin(async move {
                    if joined {
                        joined_count.fetch_add(1, Ordering::SeqCst);
                        started.notify_one();
                        if !opened.load(Ordering::SeqCst) {
                            gate.notified().await;
                        }
                    } else {
                        plain_count.fetch_add(1, Ordering::SeqCst);
                    }
                    Ok(empty_results(keys.len()))
                })
            }),
            state: Mutex::new(State::default()),
        });

        let mut held: Vec<BoxFuture<'static, ()>> = Vec::new();
        for wave in 0..MAX_JOINED_WORKERS {
            for i in 0..RUN_CONTROL_READ_MIN_PARENT_HINTS {
                let reader = reader.clone();
                let mut request: BoxFuture<'static, ()> = Box::pin(async move {
                    reader
                        .get_with_parent("held", &format!("child-{wave}-{i}"), Some("parent"))
                        .await
                        .unwrap();
                });
                assert!(futures_util::poll!(request.as_mut()).is_pending());
                held.push(request);
            }
            if wave + 1 == MAX_JOINED_WORKERS {
                // Both free workers can be waiting on the older hinted
                // deadline when a root arrives just before the third join.
                tokio::task::yield_now().await;
                tokio::time::advance(COLLECTION_WINDOW - Duration::from_millis(5)).await;
                let mut first = Box::pin(reader.get("transition", "root-1"));
                assert!(futures_util::poll!(first.as_mut()).is_pending());
                tokio::time::advance(Duration::from_millis(5)).await;
                tokio::time::timeout(Duration::from_millis(100), joined_started.notified())
                    .await
                    .expect("third joined worker did not start");
                tokio::task::yield_now().await;
                assert_eq!(
                    plain_calls.load(Ordering::SeqCst),
                    0,
                    "a hinted deadline must not prematurely dispatch the restored root"
                );
                let mut second = Box::pin(reader.get("transition", "root-2"));
                assert!(futures_util::poll!(second.as_mut()).is_pending());
                tokio::time::advance(COLLECTION_WINDOW - Duration::from_millis(5)).await;
                let (first, second) = tokio::join!(first, second);
                assert!(first.unwrap().is_none() && second.unwrap().is_none());
                assert_eq!(plain_calls.load(Ordering::SeqCst), 1);
            } else {
                tokio::time::advance(COLLECTION_WINDOW).await;
                tokio::time::timeout(Duration::from_millis(100), joined_started.notified())
                    .await
                    .expect("joined worker did not start");
            }
        }

        let mut backlog: Vec<BoxFuture<'static, ()>> = Vec::new();
        let backlog_size = MAX_READS - RUN_CONTROL_READ_BATCH_SIZE - held.len();
        for i in 0..backlog_size {
            let reader = reader.clone();
            let owner = if i < backlog_size - 160 {
                format!("owner-{i}")
            } else {
                "target".into()
            };
            let mut request: BoxFuture<'static, ()> = Box::pin(async move {
                let _ = reader
                    .get_with_parent(&owner, &format!("backlog-{i}"), Some("parent"))
                    .await;
            });
            assert!(futures_util::poll!(request.as_mut()).is_pending());
            backlog.push(request);
        }
        tokio::task::yield_now().await;
        assert_eq!(
            reader.state.lock().unwrap().joined_workers,
            MAX_JOINED_WORKERS
        );

        let mut roots: Vec<BoxFuture<'static, ()>> = Vec::new();
        for i in 0..RUN_CONTROL_READ_BATCH_SIZE {
            let reader = reader.clone();
            let mut request: BoxFuture<'static, ()> = Box::pin(async move {
                assert!(
                    reader
                        .get("target", &format!("root-{i}"))
                        .await
                        .unwrap()
                        .is_none()
                );
            });
            assert!(futures_util::poll!(request.as_mut()).is_pending());
            roots.push(request);
            if i == 0 {
                tokio::task::yield_now().await;
                assert_eq!(
                    plain_calls.load(Ordering::SeqCst),
                    1,
                    "hinted backlog must not bypass the root collection window"
                );
            }
        }
        assert!(reader.get("another-owner", "overflow").await.is_err());
        tokio::time::advance(COLLECTION_WINDOW).await;
        tokio::time::timeout(
            Duration::from_millis(100),
            futures_util::future::join_all(roots),
        )
        .await
        .expect("roots behind hinted backlog must be observed");
        assert_eq!(plain_calls.load(Ordering::SeqCst), 2);
        assert_eq!(joined_calls.load(Ordering::SeqCst), MAX_JOINED_WORKERS);

        // The reserved worker may retire while joins are still held; a later
        // root must start it again without waiting for a joined query.
        for _ in 0..10 {
            tokio::task::yield_now().await;
            if reader.state.lock().unwrap().workers == MAX_JOINED_WORKERS {
                break;
            }
        }
        assert_eq!(reader.state.lock().unwrap().workers, MAX_JOINED_WORKERS);
        assert!(reader.get("target", "late-root").await.unwrap().is_none());
        assert_eq!(plain_calls.load(Ordering::SeqCst), 3);

        drop(backlog);
        open.store(true, Ordering::SeqCst);
        release.notify_waiters();
        futures_util::future::join_all(held).await;
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                {
                    let state = reader.state.lock().unwrap();
                    if state.outstanding == 0 && state.workers == 0 {
                        assert_eq!(state.joined_workers, 0);
                        assert!(state.owners.is_empty() && state.ready.is_empty());
                        break;
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("closed and completed work must release all capacity");
        let calls = joined_calls.load(Ordering::SeqCst) + plain_calls.load(Ordering::SeqCst);
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(
            joined_calls.load(Ordering::SeqCst) + plain_calls.load(Ordering::SeqCst),
            calls
        );
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
    async fn hinted_admission_reserves_bounded_unhinted_capacity_and_recovers() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let reader = reads(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { panic!("closed observations must not reach SQL") })
        });

        let long_hint = "x".repeat(RUN_CONTROL_READ_MAX_KEY_BYTES + 1);
        for (same_owner, limit) in [
            (true, MAX_OWNER_READS - RUN_CONTROL_READ_BATCH_SIZE),
            (false, MAX_READS - RUN_CONTROL_READ_BATCH_SIZE),
        ] {
            let mut hinted: Vec<BoxFuture<'static, RunControlReadWithParentResult>> = Vec::new();
            for index in 0..limit {
                let reader = reader.clone();
                let owner = if same_owner {
                    "owner".to_string()
                } else {
                    format!("owner-{index}")
                };
                let mut request: BoxFuture<'static, RunControlReadWithParentResult> =
                    Box::pin(async move {
                        reader
                            .get_with_parent(&owner, "child", Some("parent"))
                            .await
                    });
                assert!(futures_util::poll!(request.as_mut()).is_pending());
                hinted.push(request);
            }
            let root_owner = if same_owner { "owner" } else { "root-owner" };
            assert!(
                reader
                    .get_with_parent(root_owner, "overflow", Some("parent"))
                    .await
                    .unwrap_err()
                    .contains("capacity")
            );
            let mut unhinted = Vec::new();
            for index in 0..RUN_CONTROL_READ_BATCH_SIZE {
                // An oversized hint is filtered before the admission check.
                let hint = (index == 0).then_some(long_hint.as_str());
                let mut request = Box::pin(reader.get_with_parent(root_owner, "root", hint));
                assert!(futures_util::poll!(request.as_mut()).is_pending());
                unhinted.push(request);
            }
            assert!(reader.get(root_owner, "overflow").await.is_err());
            assert_eq!(
                reader.state.lock().unwrap().outstanding,
                limit + RUN_CONTROL_READ_BATCH_SIZE
            );
            drop(unhinted);
            drop(hinted);
            tokio::time::sleep(COLLECTION_WINDOW * 2).await;
            let state = reader.state.lock().unwrap();
            assert_eq!((state.outstanding, state.workers), (0, 0));
            assert!(state.owners.is_empty() && state.ready.is_empty());
        }
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
