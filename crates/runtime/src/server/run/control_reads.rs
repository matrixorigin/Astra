//! On-demand coalescing of fresh control reads. No observations are retained
//! between batches, and this queue never polls or grants execution authority.

use std::{
    collections::{HashMap, LinkedList, VecDeque},
    panic::AssertUnwindSafe,
    sync::{Arc, Mutex},
    time::Duration,
};

use astra_services::runs::{
    RUN_CONTROL_READ_BATCH_SIZE, RunControlReadKey, RunControlReadResult, RunStateStore,
};
use futures_util::{FutureExt, future::BoxFuture};
use tokio::{sync::oneshot, time::Instant};

const COLLECTION_WINDOW: Duration = Duration::from_millis(25);
const QUERY_TIMEOUT: Duration = Duration::from_secs(3);
const READ_TIMEOUT: Duration = Duration::from_secs(4);
const MAX_READS: usize = 8192;
const MAX_OWNER_READS: usize = 4096;
const MAX_WORKERS: usize = 4;

type Read = dyn Fn(Vec<RunControlReadKey>) -> BoxFuture<'static, Result<Vec<RunControlReadResult>, String>>
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
    key: RunControlReadKey,
    collect_at: Instant,
    reply: oneshot::Sender<RunControlReadResult>,
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
        batch
    }
}

impl ControlReads {
    pub(super) fn new(store: Arc<dyn RunStateStore>) -> Arc<Self> {
        Arc::new(Self {
            read: Arc::new(move |keys| {
                let store = store.clone();
                Box::pin(async move { store.load_run_control_batch(&keys).await })
            }),
            state: Mutex::new(State::default()),
        })
    }

    pub(super) async fn get(self: &Arc<Self>, user_id: &str, run_id: &str) -> RunControlReadResult {
        // Bound retained key bytes as well as callers. These limits exceed
        // the canonical VARCHAR identity capacities, including UTF-8 bytes.
        if user_id.len() > 512 || run_id.len() > 512 {
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
                key: RunControlReadKey {
                    user_id: user_id.into(),
                    run_id: run_id.into(),
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
                    state.release(&request.key.user_id);
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
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    fn reads(
        read: impl Fn(
            Vec<RunControlReadKey>,
        ) -> BoxFuture<'static, Result<Vec<RunControlReadResult>, String>>
        + Send
        + Sync
        + 'static,
    ) -> Arc<ControlReads> {
        Arc::new(ControlReads {
            read: Arc::new(read),
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
