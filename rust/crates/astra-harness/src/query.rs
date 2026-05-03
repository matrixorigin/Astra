use crate::{RuntimeSnapshot, SnapshotDiff, SnapshotSink};
use std::sync::Arc;

/// A query that can be sent to the harness from outside the loop.
#[derive(Debug)]
pub enum HarnessQuery {
    /// Get the latest snapshot.
    Latest,
    /// Get the last N snapshots (newest first).
    History(usize),
    /// Get diff between the last two snapshots.
    Diff,
}

/// Response to a HarnessQuery.
#[derive(Debug)]
pub enum HarnessQueryResponse {
    Snapshot(Option<RuntimeSnapshot>),
    History(Vec<RuntimeSnapshot>),
    Diff(Option<SnapshotDiff>),
}

/// Handle held by the loop (or the sink owner) to receive queries.
pub struct HarnessQueryReceiver {
    rx: std::sync::mpsc::Receiver<(HarnessQuery, std::sync::mpsc::SyncSender<HarnessQueryResponse>)>,
    sink: Arc<dyn SnapshotSink>,
}

/// Handle held by the CLI / external caller to send queries.
#[derive(Clone)]
pub struct HarnessQuerySender {
    tx: std::sync::mpsc::SyncSender<(HarnessQuery, std::sync::mpsc::SyncSender<HarnessQueryResponse>)>,
}

/// Create a query channel pair with bounded capacity.
pub fn query_channel(
    sink: Arc<dyn SnapshotSink>,
    bound: usize,
) -> (HarnessQuerySender, HarnessQueryReceiver) {
    let (tx, rx) = std::sync::mpsc::sync_channel(bound);
    (
        HarnessQuerySender { tx },
        HarnessQueryReceiver { rx, sink },
    )
}

impl HarnessQuerySender {
    /// Send a query and wait for the response.
    /// Returns None if the receiver is dropped (session ended).
    pub fn query(&self, q: HarnessQuery) -> Option<HarnessQueryResponse> {
        let (resp_tx, resp_rx) = std::sync::mpsc::sync_channel(1);
        self.tx.send((q, resp_tx)).ok()?;
        resp_rx.recv().ok()
    }

    /// Convenience: get latest snapshot.
    pub fn latest(&self) -> Option<RuntimeSnapshot> {
        match self.query(HarnessQuery::Latest)? {
            HarnessQueryResponse::Snapshot(s) => s,
            _ => None,
        }
    }

    /// Convenience: get history.
    pub fn history(&self, n: usize) -> Vec<RuntimeSnapshot> {
        match self.query(HarnessQuery::History(n)) {
            Some(HarnessQueryResponse::History(h)) => h,
            _ => vec![],
        }
    }

    /// Convenience: get diff.
    pub fn diff(&self) -> Option<SnapshotDiff> {
        match self.query(HarnessQuery::Diff)? {
            HarnessQueryResponse::Diff(d) => d,
            _ => None,
        }
    }
}

impl HarnessQueryReceiver {
    /// Process all pending queries (non-blocking). Call this periodically
    /// from the loop or from a dedicated thread.
    pub fn drain(&self) {
        while let Ok((query, resp_tx)) = self.rx.try_recv() {
            let response = self.handle(query);
            let _ = resp_tx.send(response);
        }
    }

    fn handle(&self, query: HarnessQuery) -> HarnessQueryResponse {
        match query {
            HarnessQuery::Latest => HarnessQueryResponse::Snapshot(self.sink.latest()),
            HarnessQuery::History(n) => HarnessQueryResponse::History(self.sink.history(n)),
            HarnessQuery::Diff => {
                let history = self.sink.history(2);
                let diff = if history.len() >= 2 {
                    Some(SnapshotDiff::between(&history[1], &history[0]))
                } else {
                    None
                };
                HarnessQueryResponse::Diff(diff)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DecisionRecord, HookPoint, InMemorySnapshotSink};

    fn make_sink_with_snapshots(n: u32) -> Arc<InMemorySnapshotSink> {
        let sink = InMemorySnapshotSink::arc();
        for i in 1..=n {
            let record = DecisionRecord {
                session_id: "test".into(),
                turn: i,
                point: HookPoint::PostTurn,
                wall_time_unix_millis: i as u64 * 1000,
                monotonic_millis_since_session: i as u64 * 1000,
                snapshot: RuntimeSnapshot {
                    turn_number: i,
                    turns_used: i,
                    tokens_used_session: i as u64 * 10_000,
                    tool_calls_this_session: i * 2,
                    unique_tools_used: vec!["bash".into()],
                    ..RuntimeSnapshot::empty()
                },
            };
            sink.update(&record);
        }
        sink
    }

    #[test]
    fn query_latest_returns_most_recent() {
        let sink = make_sink_with_snapshots(3);
        let (sender, receiver) = query_channel(sink, 4);

        // Spawn a thread to drain queries
        let handle = std::thread::spawn(move || {
            // Process one query
            if let Ok((query, resp_tx)) = receiver.rx.recv() {
                let _ = resp_tx.send(receiver.handle(query));
            }
        });

        let snap = sender.latest().unwrap();
        assert_eq!(snap.turn_number, 3);
        handle.join().unwrap();
    }

    #[test]
    fn query_history_returns_newest_first() {
        let sink = make_sink_with_snapshots(5);
        let (sender, receiver) = query_channel(sink, 4);

        let handle = std::thread::spawn(move || {
            if let Ok((query, resp_tx)) = receiver.rx.recv() {
                let _ = resp_tx.send(receiver.handle(query));
            }
        });

        let history = sender.history(3);
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].turn_number, 5);
        assert_eq!(history[2].turn_number, 3);
        handle.join().unwrap();
    }

    #[test]
    fn query_diff_computes_delta() {
        let sink = make_sink_with_snapshots(3);
        let (sender, receiver) = query_channel(sink, 4);

        let handle = std::thread::spawn(move || {
            if let Ok((query, resp_tx)) = receiver.rx.recv() {
                let _ = resp_tx.send(receiver.handle(query));
            }
        });

        let diff = sender.diff().unwrap();
        assert_eq!(diff.from_turn, 2);
        assert_eq!(diff.to_turn, 3);
        assert_eq!(diff.tokens_delta, 10_000);
        handle.join().unwrap();
    }

    #[test]
    fn query_diff_returns_none_with_single_snapshot() {
        let sink = make_sink_with_snapshots(1);
        let (sender, receiver) = query_channel(sink, 4);

        let handle = std::thread::spawn(move || {
            if let Ok((query, resp_tx)) = receiver.rx.recv() {
                let _ = resp_tx.send(receiver.handle(query));
            }
        });

        assert!(sender.diff().is_none());
        handle.join().unwrap();
    }

    #[test]
    fn drain_processes_multiple_pending_queries() {
        let sink = make_sink_with_snapshots(3);
        let (sender, receiver) = query_channel(sink, 8);

        // Send multiple queries without draining
        let (resp_tx1, resp_rx1) = std::sync::mpsc::sync_channel(1);
        let (resp_tx2, resp_rx2) = std::sync::mpsc::sync_channel(1);
        sender.tx.send((HarnessQuery::Latest, resp_tx1)).unwrap();
        sender.tx.send((HarnessQuery::History(2), resp_tx2)).unwrap();

        // Drain all at once
        receiver.drain();

        let r1 = resp_rx1.recv().unwrap();
        let r2 = resp_rx2.recv().unwrap();

        assert!(matches!(r1, HarnessQueryResponse::Snapshot(Some(_))));
        assert!(matches!(r2, HarnessQueryResponse::History(ref h) if h.len() == 2));
    }

    #[test]
    fn sender_returns_none_when_receiver_dropped() {
        let sink = make_sink_with_snapshots(1);
        let (sender, receiver) = query_channel(sink, 4);
        drop(receiver);

        assert!(sender.latest().is_none());
    }
}
