//! Shared scheduling for remote-child hints, without another execution ledger.
//! Only subscribed exact identities are read. Durable recovery owns results and
//! cancellation; neither absence nor an observation failure implies completion.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};

use astra_services::runs::{REMOTE_CHILD_WAKE_BATCH_SIZE, RemoteChildWakeKey, RunStateStore};
use futures_util::future::BoxFuture;
use tokio::sync::watch;

const POLL_INTERVAL: Duration = Duration::from_secs(1);
const QUERY_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_SUBSCRIPTIONS: usize = 8192;
const MAX_OWNER_SUBSCRIPTIONS: usize = MAX_SUBSCRIPTIONS / 2;
const MAX_KEYS: usize = 65_536;
const MAX_OWNER_KEYS: usize = 32_768;

type ReadWakes = dyn Fn(Vec<RemoteChildWakeKey>) -> BoxFuture<'static, Result<Vec<RemoteChildWakeKey>, String>>
    + Send
    + Sync;

pub(super) struct RemoteChildWakeHub {
    read: Arc<ReadWakes>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    subscriptions: Vec<Subscription>,
    running: bool,
    cursor: usize,
}

struct Subscription {
    user_id: String,
    pending: BTreeSet<RemoteChildWakeKey>,
    revision: watch::Sender<u64>,
}

impl RemoteChildWakeHub {
    pub(super) fn new(store: Arc<dyn RunStateStore>) -> Arc<Self> {
        Arc::new(Self {
            read: Arc::new(move |keys| {
                let store = store.clone();
                Box::pin(async move { store.load_remote_child_wakes(&keys).await })
            }),
            state: Mutex::new(State::default()),
        })
    }

    pub(super) fn subscribe(
        self: &Arc<Self>,
        user_id: &str,
        session_id: &str,
        parent_run_id: &str,
        child_run_ids: &[String],
    ) -> Result<Option<watch::Receiver<u64>>, String> {
        if child_run_ids.is_empty() {
            return Ok(None);
        }
        // Bound allocations even before deduplication. Caller overload remains
        // explicit; it must not silently fall back to per-parent SQL polling.
        if child_run_ids.len() > MAX_OWNER_KEYS {
            return Err("remote child wake subscription exceeds owner capacity".into());
        }
        let pending = child_run_ids
            .iter()
            .map(|run_id| RemoteChildWakeKey {
                user_id: user_id.into(),
                session_id: session_id.into(),
                parent_run_id: parent_run_id.into(),
                run_id: run_id.clone(),
            })
            .collect::<BTreeSet<_>>();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .subscriptions
            .retain(|sub| sub.revision.receiver_count() > 0);
        let total = state
            .subscriptions
            .iter()
            .map(|sub| sub.pending.len())
            .sum::<usize>();
        let owner_total = state
            .subscriptions
            .iter()
            .filter(|sub| sub.user_id == user_id)
            .map(|sub| sub.pending.len())
            .sum::<usize>();
        if state.subscriptions.len() >= MAX_SUBSCRIPTIONS
            || state
                .subscriptions
                .iter()
                .filter(|sub| sub.user_id == user_id)
                .count()
                >= MAX_OWNER_SUBSCRIPTIONS
            || total + pending.len() > MAX_KEYS
            || owner_total + pending.len() > MAX_OWNER_KEYS
        {
            return Err("remote child wake observer is at capacity".into());
        }
        let (revision, receiver) = watch::channel(0);
        state.subscriptions.push(Subscription {
            user_id: user_id.into(),
            pending,
            revision,
        });
        if !state.running {
            state.running = true;
            let weak = Arc::downgrade(self);
            tokio::spawn(async move {
                let mut delay = POLL_INTERVAL;
                let mut failures = 0u32;
                loop {
                    tokio::time::sleep(delay).await;
                    let Some(hub) = weak.upgrade() else { return };
                    let Some(failed) = hub.sweep().await else {
                        return;
                    };
                    failures = if failed {
                        failures.saturating_add(1)
                    } else {
                        0
                    };
                    delay = if failures == 0 {
                        POLL_INTERVAL
                    } else {
                        // One bounded backoff for this process, with jitter so
                        // reconnecting pods do not synchronize pool pressure.
                        let seconds = (1u64 << failures.min(5)).min(30);
                        Duration::from_millis(
                            seconds * 1000 + (uuid::Uuid::new_v4().as_u128() % 251) as u64,
                        )
                    };
                }
            });
        }
        Ok(Some(receiver))
    }

    /// None retires the worker. One SQL attempt at a time; owner interleaving
    /// and a rotating retry cursor keep a hot owner or failed batch from
    /// monopolizing the next sweep. No mutex is held across database I/O.
    async fn sweep(&self) -> Option<bool> {
        let (mut keys, senders) = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state
                .subscriptions
                .retain(|sub| sub.revision.receiver_count() > 0);
            let mut senders = BTreeMap::<RemoteChildWakeKey, Vec<watch::Sender<u64>>>::new();
            for sub in &state.subscriptions {
                for key in &sub.pending {
                    senders
                        .entry(key.clone())
                        .or_default()
                        .push(sub.revision.clone());
                }
            }
            if senders.is_empty() {
                state.running = false;
                return None;
            }
            let mut owners = BTreeMap::<String, VecDeque<RemoteChildWakeKey>>::new();
            for key in senders.keys() {
                owners
                    .entry(key.user_id.clone())
                    .or_default()
                    .push_back(key.clone());
            }
            let mut keys = Vec::with_capacity(senders.len());
            while !owners.is_empty() {
                owners.retain(|_, queue| {
                    if let Some(key) = queue.pop_front() {
                        keys.push(key);
                    }
                    !queue.is_empty()
                });
            }
            let offset = state.cursor % keys.len();
            keys.rotate_left(offset);
            (keys, senders)
        };
        for batch in keys.chunks_mut(REMOTE_CHILD_WAKE_BATCH_SIZE) {
            let active = batch
                .iter()
                .filter(|key| {
                    senders[*key]
                        .iter()
                        .any(|sender| sender.receiver_count() > 0)
                })
                .cloned()
                .collect::<Vec<_>>();
            if active.is_empty() {
                continue;
            }
            let result = tokio::time::timeout(QUERY_TIMEOUT, (self.read)(active.clone())).await;
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.cursor = state.cursor.wrapping_add(batch.len());
            let ready = match result {
                Ok(Ok(ready)) => ready
                    .into_iter()
                    .filter(|key| active.contains(key))
                    .collect::<BTreeSet<_>>(),
                _ => {
                    tracing::warn!(
                        "remote child wake batch unavailable; observation will back off"
                    );
                    return Some(true);
                }
            };
            if ready.is_empty() {
                continue;
            }
            for sub in &mut state.subscriptions {
                let before = sub.pending.len();
                sub.pending.retain(|key| !ready.contains(key));
                if before != sub.pending.len() {
                    sub.revision
                        .send_modify(|revision| *revision = revision.wrapping_add(1));
                }
            }
        }
        Some(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    fn hub(
        read: impl Fn(
            Vec<RemoteChildWakeKey>,
        ) -> BoxFuture<'static, Result<Vec<RemoteChildWakeKey>, String>>
        + Send
        + Sync
        + 'static,
    ) -> Arc<RemoteChildWakeHub> {
        Arc::new(RemoteChildWakeHub {
            read: Arc::new(read),
            state: Mutex::new(State::default()),
        })
    }

    #[tokio::test(start_paused = true)]
    async fn thousand_waiters_are_deduplicated_batched_and_owner_fair() {
        let reads = Arc::new(Mutex::new(Vec::<Vec<RemoteChildWakeKey>>::new()));
        let recorded = reads.clone();
        let observer = hub(move |keys| {
            recorded.lock().unwrap().push(keys);
            Box::pin(async { Ok(Vec::new()) })
        });
        let mut receivers = Vec::new();
        for i in 0..1000 {
            let user = if i < 900 {
                "hot".into()
            } else {
                format!("owner-{i}")
            };
            receivers.push(
                observer
                    .subscribe(
                        &user,
                        &format!("session-{i}"),
                        "parent",
                        &[format!("child-{i}")],
                    )
                    .unwrap()
                    .unwrap(),
            );
        }
        // Duplicate subscriptions must not duplicate database identities.
        receivers.push(
            observer
                .subscribe("hot", "session-0", "parent", &["child-0".into()])
                .unwrap()
                .unwrap(),
        );
        assert_eq!(observer.sweep().await, Some(false));
        {
            let batches = reads.lock().unwrap();
            assert_eq!(
                batches.len(),
                1000_usize.div_ceil(REMOTE_CHILD_WAKE_BATCH_SIZE)
            );
            assert_eq!(batches.iter().map(Vec::len).sum::<usize>(), 1000);
            assert!(batches.iter().all(|batch| batch.len() <= 128));
            assert_eq!(
                batches[0].iter().filter(|key| key.user_id != "hot").count(),
                100,
                "cold owners share the first batch with the hot owner"
            );
        }
        assert!(
            receivers.iter().all(|rx| !rx.has_changed().unwrap()),
            "running rows cannot wake parents"
        );
        drop(receivers);
        assert_eq!(observer.sweep().await, None);
        assert_eq!(
            reads.lock().unwrap().len(),
            8,
            "last unsubscribe stops reads"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn owner_capacity_is_bounded_and_reclaimed_after_unsubscribe() {
        let observer = hub(|_| Box::pin(async { Ok(Vec::new()) }));
        assert!(
            observer
                .subscribe("owner", "s", "p", &[])
                .unwrap()
                .is_none()
        );
        assert!(
            observer
                .subscribe("owner", "s", "p", &vec!["c".into(); MAX_OWNER_KEYS + 1])
                .is_err()
        );
        let keys = (0..MAX_OWNER_KEYS)
            .map(|i| format!("child-{i}"))
            .collect::<Vec<_>>();
        let full = observer
            .subscribe("owner", "s", "p", &keys)
            .unwrap()
            .unwrap();
        assert!(
            observer
                .subscribe("owner", "s2", "p", &["extra".into()])
                .is_err()
        );
        let other = observer
            .subscribe("other", "s", "p", &["other".into()])
            .unwrap()
            .unwrap();
        drop(full);
        assert!(
            observer
                .subscribe("owner", "s2", "p", &["extra".into()])
                .is_ok()
        );
        drop(other);
        assert_eq!(observer.sweep().await, None);
    }

    #[tokio::test(start_paused = true)]
    async fn hints_are_scoped_one_shot_and_resubscription_rechecks_durable_state() {
        let ready = RemoteChildWakeKey {
            user_id: "a".into(),
            session_id: "s".into(),
            parent_run_id: "p".into(),
            run_id: "c".into(),
        };
        let observer = hub(move |_keys| {
            let ready = ready.clone();
            Box::pin(async move { Ok(vec![ready]) })
        });
        let rx = observer
            .subscribe("a", "s", "p", &["c".into()])
            .unwrap()
            .unwrap();
        let other_user = observer
            .subscribe("b", "s", "p", &["c".into()])
            .unwrap()
            .unwrap();
        let other_session = observer
            .subscribe("a", "other", "p", &["c".into()])
            .unwrap()
            .unwrap();
        let other_parent = observer
            .subscribe("a", "s", "other", &["c".into()])
            .unwrap()
            .unwrap();
        observer.sweep().await;
        assert_eq!(*rx.borrow(), 1);
        for other in [&other_user, &other_session, &other_parent] {
            assert!(!other.has_changed().unwrap());
        }
        observer.sweep().await;
        assert_eq!(
            *rx.borrow(),
            1,
            "terminal hints cannot induce permanent per-parent polling"
        );
        // A consumer unable to recover the hinted result can resubscribe. The
        // new subscription is not silenced by a previous consumer's hint.
        let retry = observer
            .subscribe("a", "s", "p", &["c".into()])
            .unwrap()
            .unwrap();
        observer.sweep().await;
        assert_eq!(*retry.borrow(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn initial_delay_error_backoff_and_unsubscribe_are_bounded() {
        let reads = Arc::new(AtomicUsize::new(0));
        let count = reads.clone();
        let observer = hub(move |keys| {
            let attempt = count.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if attempt == 0 {
                    Err("database unavailable".into())
                } else {
                    Ok(keys)
                }
            })
        });
        let receiver = observer
            .subscribe("owner", "session", "parent", &["child".into()])
            .unwrap()
            .unwrap();
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(999)).await;
        tokio::task::yield_now().await;
        assert_eq!(reads.load(Ordering::SeqCst), 0);
        tokio::time::advance(Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(reads.load(Ordering::SeqCst), 1);
        assert!(!receiver.has_changed().unwrap());
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(reads.load(Ordering::SeqCst), 1, "failed reads back off");
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::task::yield_now().await;
        assert_eq!(reads.load(Ordering::SeqCst), 2);
        assert_eq!(*receiver.borrow(), 1);
        drop(receiver);
        tokio::time::advance(Duration::from_secs(60)).await;
        tokio::task::yield_now().await;
        assert_eq!(reads.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn failed_batch_rotates_and_slow_query_never_holds_subscription_lock() {
        let stalled = Arc::new(AtomicBool::new(true));
        let flag = stalled.clone();
        let reads = Arc::new(Mutex::new(Vec::new()));
        let recorded = reads.clone();
        let observer = hub(move |keys| {
            recorded.lock().unwrap().push(keys.clone());
            let stall = flag.load(Ordering::SeqCst);
            Box::pin(async move {
                if stall {
                    std::future::pending().await
                } else {
                    Ok(keys)
                }
            })
        });
        let receiver = observer
            .subscribe(
                "hot",
                "session",
                "parent",
                &(0..256)
                    .map(|i| format!("child-{i:03}"))
                    .collect::<Vec<_>>(),
            )
            .unwrap()
            .unwrap();
        tokio::task::yield_now().await;
        tokio::time::advance(POLL_INTERVAL).await;
        tokio::task::yield_now().await;
        // Registering another owner during a blocked read cannot wait on SQL.
        let cold = observer
            .subscribe("cold", "session", "parent", &["cold-child".into()])
            .unwrap()
            .unwrap();
        tokio::time::advance(QUERY_TIMEOUT).await;
        tokio::task::yield_now().await;
        assert!(!receiver.has_changed().unwrap());
        stalled.store(false, Ordering::SeqCst);
        tokio::time::advance(Duration::from_secs(3)).await;
        tokio::task::yield_now().await;
        assert!(cold.has_changed().unwrap());
        let batches = reads.lock().unwrap();
        assert_ne!(
            batches[0][0], batches[1][0],
            "failed front batch cannot starve later keys"
        );
    }
}
