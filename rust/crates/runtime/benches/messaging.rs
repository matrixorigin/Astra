//! Criterion benchmarks for the messaging subsystem.
//!
//! Run: `cargo bench -p astra-runtime --bench messaging`
//!
//! Measures:
//! - Message creation & serialization
//! - InProcessTransport throughput (direct + broadcast)
//! - Router registration & lookup
//! - AckTracker track/ack/sweep cycle
//! - DeadLetterQueue store/list/purge
//! - LatencyTracker record throughput

use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use std::sync::Arc;
use std::time::Duration;

use astra_runtime::messaging::metrics::LatencyTracker;
use astra_runtime::messaging::{
    AckConfig, AgentAddress, AgentMailboxRouter, AgentMessage, DeadLetterQueue, DeadLetterReason,
    InProcessTransport, MessagePayload, MessageTarget, PendingAckTracker,
};
use astra_runtime::server::delegation_engine::DelegationTracker;

// ─── Helpers ────────────────────────────────────────────────────────────────

fn addr(run: &str, agent: &str) -> AgentAddress {
    AgentAddress::new(run, agent)
}

fn text_msg(from: (&str, &str), to: (&str, &str)) -> AgentMessage {
    AgentMessage::new(
        addr(from.0, from.1),
        MessageTarget::Direct {
            address: addr(to.0, to.1),
        },
        MessagePayload::Text {
            content: "benchmark payload".into(),
            summary: None,
        },
    )
}

fn text_msg_with_body(from: (&str, &str), to: (&str, &str), body: &str) -> AgentMessage {
    AgentMessage::new(
        addr(from.0, from.1),
        MessageTarget::Direct {
            address: addr(to.0, to.1),
        },
        MessagePayload::Text {
            content: body.into(),
            summary: None,
        },
    )
}

// ─── Message Creation ───────────────────────────────────────────────────────

fn bench_message_creation(c: &mut Criterion) {
    let mut group = c.benchmark_group("message_creation");

    group.bench_function("new_text_message", |b| {
        b.iter(|| {
            black_box(text_msg(("r1", "a1"), ("r2", "a2")));
        })
    });

    group.bench_function("new_text_with_ack", |b| {
        b.iter(|| {
            black_box(text_msg(("r1", "a1"), ("r2", "a2")).with_ack_required());
        })
    });

    // Varying payload sizes
    for size in [64, 256, 1024, 4096, 16384] {
        let body: String = "x".repeat(size);
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::new("text_payload", size), &body, |b, body| {
            b.iter(|| {
                black_box(text_msg_with_body(("r1", "a1"), ("r2", "a2"), body));
            })
        });
    }

    group.bench_function("make_ack", |b| {
        let msg = text_msg(("r1", "a1"), ("r2", "a2"));
        let responder = addr("r2", "a2");
        b.iter(|| {
            black_box(msg.make_ack(responder.clone()));
        })
    });

    group.finish();
}

// ─── InProcessTransport Throughput ──────────────────────────────────────────

fn bench_inprocess_transport(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let mut group = c.benchmark_group("inprocess_transport");

    // Direct message send throughput
    group.bench_function("direct_send_1k", |b| {
        b.to_async(&rt).iter(|| async {
            let transport = Arc::new(InProcessTransport::new());
            let dt = Arc::new(DelegationTracker::new());
            let router = Arc::new(AgentMailboxRouter::new(transport, dt));
            let a1 = addr("r1", "a1");
            let a2 = addr("r1", "a2");
            let mb1 = router.register(a1, None).await.unwrap();
            let mut mb2 = router.register(a2.clone(), None).await.unwrap();
            for _ in 0..1000 {
                let msg = AgentMessage::new(
                    mb1.address.clone(),
                    MessageTarget::Direct {
                        address: a2.clone(),
                    },
                    MessagePayload::Text {
                        content: "bench".into(),
                        summary: None,
                    },
                );
                let _ = mb1.send(msg).await;
            }
            let _ = mb2.drain();
        })
    });

    // Broadcast throughput (10 subscribers)
    group.bench_function("broadcast_10_subscribers", |b| {
        b.to_async(&rt).iter(|| async {
            let transport = Arc::new(InProcessTransport::new());
            let dt = Arc::new(DelegationTracker::new());
            let router = Arc::new(AgentMailboxRouter::new(transport, dt.clone()));
            let parent = addr("r1", "orchestrator");
            let parent_mb = router.register(parent.clone(), None).await.unwrap();
            for i in 0..10 {
                let child_addr = addr(&format!("r-child-{i}"), &format!("agent-{i}"));
                let delegation_id = "del-bench";
                dt.record_sub_run(astra_runtime::server::delegation_engine::SubRunRecord {
                    run_id: format!("r-child-{i}"),
                    parent_run_id: "r1".into(),
                    delegation_id: delegation_id.into(),
                    agent_id: format!("agent-{i}"),
                    depth: 1,
                })
                .await;
                let _ = router
                    .register(child_addr, Some(delegation_id.into()))
                    .await
                    .unwrap();
            }
            for _ in 0..100 {
                let msg = AgentMessage::new(
                    parent_mb.address.clone(),
                    MessageTarget::Broadcast {
                        delegation_id: "del-bench".into(),
                    },
                    MessagePayload::Text {
                        content: "broadcast".into(),
                        summary: None,
                    },
                );
                let _ = parent_mb.send(msg).await;
            }
        })
    });

    group.finish();
}

// ─── AckTracker Performance ─────────────────────────────────────────────────

fn bench_ack_tracker(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let mut group = c.benchmark_group("ack_tracker");

    // Track + immediate acknowledge
    group.bench_function("track_and_ack_1k", |b| {
        b.to_async(&rt).iter(|| async {
            let tracker = PendingAckTracker::new();
            let mut ids = Vec::with_capacity(1000);
            for i in 0..1000u32 {
                let msg = Arc::new(
                    text_msg(("r1", "sender"), ("r2", &format!("recv-{i}"))).with_ack_required(),
                );
                ids.push(msg.id.clone());
                tracker.track(msg).await;
            }
            for id in &ids {
                tracker.acknowledge(id).await;
            }
            assert_eq!(tracker.pending_count().await, 0);
        })
    });

    // Sweep with all expired (worst case)
    group.bench_function("sweep_1k_expired", |b| {
        b.to_async(&rt).iter(|| async {
            let config = AckConfig {
                ack_timeout: Duration::from_nanos(1),
                max_retries: 1,
                sweep_interval: Duration::from_secs(1),
            };
            let tracker = PendingAckTracker::with_config(config);
            for i in 0..1000u32 {
                let msg =
                    Arc::new(text_msg(("r1", "s"), ("r2", &format!("r-{i}"))).with_ack_required());
                tracker.track(msg).await;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
            let outcomes = tracker.sweep().await;
            assert_eq!(outcomes.len(), 1000);
        })
    });

    group.finish();
}

// ─── Dead Letter Queue Performance ──────────────────────────────────────────

fn bench_dead_letter_queue(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let mut group = c.benchmark_group("dead_letter_queue");

    // Store throughput
    group.bench_function("store_1k", |b| {
        b.to_async(&rt).iter(|| async {
            let dlq = DeadLetterQueue::new();
            for i in 0..1000u32 {
                let msg = Arc::new(text_msg(("r1", "s"), ("r2", &format!("r-{i}"))));
                dlq.store(msg, DeadLetterReason::AckTimeout { attempts: 3 }, 3)
                    .await;
            }
            assert_eq!(dlq.count().await, 1000);
        })
    });

    // Store with eviction (capacity 100, insert 1000)
    group.bench_function("store_1k_evict_to_100", |b| {
        b.to_async(&rt).iter(|| async {
            let dlq = DeadLetterQueue::with_capacity(100);
            for i in 0..1000u32 {
                let msg = Arc::new(text_msg(("r1", "s"), ("r2", &format!("r-{i}"))));
                dlq.store(msg, DeadLetterReason::AckTimeout { attempts: 3 }, 3)
                    .await;
            }
            assert_eq!(dlq.count().await, 100);
        })
    });

    // List + reason_summary
    group.bench_function("reason_summary_1k", |b| {
        b.to_async(&rt).iter(|| async {
            let dlq = DeadLetterQueue::new();
            for i in 0..1000u32 {
                let msg = Arc::new(text_msg(("r1", "s"), ("r2", &format!("r-{i}"))));
                let reason = if i % 3 == 0 {
                    DeadLetterReason::AckTimeout { attempts: 3 }
                } else if i % 3 == 1 {
                    DeadLetterReason::Rejected {
                        reason: Some("bad".into()),
                    }
                } else {
                    DeadLetterReason::Expired
                };
                dlq.store(msg, reason, 1).await;
            }
            let summary = dlq.reason_summary().await;
            assert!(summary.total > 0);
        })
    });

    group.finish();
}

// ─── LatencyTracker Throughput ──────────────────────────────────────────────

fn bench_latency_tracker(c: &mut Criterion) {
    let mut group = c.benchmark_group("latency_tracker");

    group.bench_function("record_1M", |b| {
        let tracker = LatencyTracker::new();
        b.iter(|| {
            for _ in 0..1000 {
                tracker.record(black_box(Duration::from_micros(42)));
            }
        })
    });

    group.bench_function("snapshot", |b| {
        let tracker = LatencyTracker::new();
        for _ in 0..10000 {
            tracker.record(Duration::from_micros(42));
        }
        b.iter(|| {
            black_box(tracker.snapshot());
        })
    });

    group.finish();
}

// ─── Router Registration ────────────────────────────────────────────────────

fn bench_router(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let mut group = c.benchmark_group("router");

    // Register N agents
    for n in [10, 100, 1000] {
        group.bench_with_input(BenchmarkId::new("register", n), &n, |b, &n| {
            b.to_async(&rt).iter(|| async move {
                let transport = Arc::new(InProcessTransport::new());
                let dt = Arc::new(DelegationTracker::new());
                let router = Arc::new(AgentMailboxRouter::new(transport, dt));
                for i in 0..n {
                    let a = addr(&format!("run-{i}"), &format!("agent-{i}"));
                    let _ = router.register(a, None).await;
                }
            });
        });
    }

    // Send via router (includes lookup)
    group.bench_function("send_via_router_1k", |b| {
        b.to_async(&rt).iter(|| async {
            let transport = Arc::new(InProcessTransport::new());
            let dt = Arc::new(DelegationTracker::new());
            let router = Arc::new(AgentMailboxRouter::new(transport, dt));
            let a1 = addr("r1", "sender");
            let a2 = addr("r2", "receiver");
            let mb1 = router.register(a1, None).await.unwrap();
            let mut mb2 = router.register(a2.clone(), None).await.unwrap();
            for _ in 0..1000 {
                let msg = AgentMessage::new(
                    mb1.address.clone(),
                    MessageTarget::Direct {
                        address: a2.clone(),
                    },
                    MessagePayload::Text {
                        content: "x".into(),
                        summary: None,
                    },
                );
                let _ = mb1.send(msg).await;
            }
            let _ = mb2.drain();
        })
    });

    group.finish();
}

// ─── Entry Point ────────────────────────────────────────────────────────────

criterion_group!(
    benches,
    bench_message_creation,
    bench_inprocess_transport,
    bench_ack_tracker,
    bench_dead_letter_queue,
    bench_latency_tracker,
    bench_router,
);
criterion_main!(benches);
