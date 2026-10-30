//! Deterministic workload primitives for the opt-in sustained-ingestion probe.
//! Database execution is added at the existing ingestion sender/worker boundary;
//! these offline checks keep workload generation independent of database speed.

use std::num::NonZeroU64;
use std::time::Duration;

use astra_services::event_ingestion::measurement::{
    IngestionDeliveryKey, IngestionDeliveryProbe, IngestionDeliveryReport,
    IngestionDeliveryTerminal, IngestionMeasurementSink,
};
use astra_services::event_ingestion::{
    EventIngestionWorker, IngestionConfig, IngestionEvent, IngestionEventPriority,
};
use serde_json::{Value, json};
use sqlx::Row;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

mod common;

const OWNERS: usize = 100;
const SESSIONS_PER_OWNER: usize = 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OwnerDistribution {
    Uniform,
    HalfHotOwner,
}

fn owner_session(index: u64, distribution: OwnerDistribution) -> (usize, usize) {
    let (owner, session) = match distribution {
        OwnerDistribution::Uniform => (
            (index / SESSIONS_PER_OWNER as u64) % OWNERS as u64,
            index % SESSIONS_PER_OWNER as u64,
        ),
        OwnerDistribution::HalfHotOwner => {
            let ordinal = index / 2;
            let owner = if index.is_multiple_of(2) {
                0
            } else {
                1 + (ordinal / SESSIONS_PER_OWNER as u64) % (OWNERS - 1) as u64
            };
            (owner, ordinal % SESSIONS_PER_OWNER as u64)
        }
    };
    (owner as usize, session as usize)
}

/// Absolute deadlines avoid completion-paced load and cumulative timer drift.
fn arrival_offset(index: u64, rate: NonZeroU64) -> Duration {
    let nanos = (u128::from(index) * 1_000_000_000).div_ceil(u128::from(rate.get()));
    Duration::new(
        (nanos / 1_000_000_000) as u64,
        (nanos % 1_000_000_000) as u32,
    )
}

/// A late generator skips and reports older arrivals instead of bursting them.
fn latest_due_index(elapsed: Duration, rate: NonZeroU64) -> u64 {
    (elapsed.as_nanos().saturating_mul(u128::from(rate.get())) / 1_000_000_000)
        .min(u128::from(u64::MAX)) as u64
}

/// Seeded sizes describe target serialized envelopes, not just content bytes.
fn target_envelope_bytes(index: u64, seed: u64) -> usize {
    let mut rng = fastrand::Rng::with_seed(seed.wrapping_add(index));
    match rng.u8(0..100) {
        0..80 => 1_024,
        80..95 => 16 * 1_024,
        _ => 128 * 1_024,
    }
}

#[derive(Default)]
struct Histogram {
    buckets: BTreeMap<u64, u64>,
    count: u64,
}

impl Histogram {
    fn record(&mut self, duration: Duration) {
        let micros = duration.as_micros().min(u128::from(u64::MAX / 2)) as u64;
        // Sixteen subdivisions per power of two: bounded cardinality and
        // <=6.25% quantization. Report bucket bounds, never exact percentiles.
        let exponent = 63_u32.saturating_sub(micros.leading_zeros());
        let quantum = 1_u64 << exponent.saturating_sub(4);
        let upper = micros.div_ceil(quantum) * quantum;
        *self.buckets.entry(upper).or_default() += 1;
        self.count += 1;
    }

    fn percentile(&self, percentile: u64) -> Option<u64> {
        let rank = (self.count * percentile).div_ceil(100).max(1);
        let mut count = 0;
        for (upper, samples) in &self.buckets {
            count += samples;
            if count >= rank {
                return Some(*upper);
            }
        }
        None
    }

    fn summary(&self) -> Value {
        json!({"samples": self.count, "unit": "microseconds_upper_bucket_bound",
            "p50": self.percentile(50), "p95": self.percentile(95), "p99": self.percentile(99)})
    }
}

#[derive(Clone)]
struct ProbeConfig {
    rate: NonZeroU64,
    warmup: u64,
    seconds: u64,
    drain: u64,
    pool_size: u32,
    foreground_rate: NonZeroU64,
    baseline_seconds: u64,
    distribution: OwnerDistribution,
    seed: u64,
}

fn bounded_env(name: &str, default: u64, max: u64) -> u64 {
    let value = std::env::var(name)
        .map(|value| value.parse::<u64>().expect(name))
        .unwrap_or(default);
    assert!((1..=max).contains(&value), "{name} must be in 1..={max}");
    value
}

impl ProbeConfig {
    fn from_env() -> Self {
        Self {
            rate: NonZeroU64::new(bounded_env("ASTRA_INGESTION_PROBE_RATE", 200, 10_000)).unwrap(),
            warmup: bounded_env("ASTRA_INGESTION_PROBE_WARMUP_SECS", 60, 600),
            seconds: bounded_env("ASTRA_INGESTION_PROBE_SECS", 600, 3_600),
            drain: bounded_env("ASTRA_INGESTION_PROBE_DRAIN_SECS", 60, 300),
            pool_size: bounded_env("ASTRA_INGESTION_PROBE_POOL_SIZE", 32, 128) as u32,
            foreground_rate: NonZeroU64::new(bounded_env(
                "ASTRA_INGESTION_PROBE_FOREGROUND_RATE",
                20,
                1_000,
            ))
            .unwrap(),
            baseline_seconds: bounded_env("ASTRA_INGESTION_PROBE_BASELINE_SECS", 10, 120),
            distribution: match std::env::var("ASTRA_INGESTION_PROBE_DISTRIBUTION").as_deref() {
                Ok("hot") => OwnerDistribution::HalfHotOwner,
                Ok("uniform") | Err(_) => OwnerDistribution::Uniform,
                _ => panic!("distribution must be uniform or hot"),
            },
            seed: 42,
        }
    }

    fn count(&self) -> u64 {
        let total = self.rate.get() * (self.warmup + self.seconds);
        assert!(
            total <= 2_000_000,
            "probe accounting is bounded to two million offered arrivals"
        );
        total
    }
}

fn fixture_identity(run: &str, owner: usize, session: usize) -> (String, String) {
    (
        format!("ingestion-probe-{run}-{owner:03}"),
        format!("{run}-{owner:03}-{session:02}"),
    )
}

fn probe_event(run: &str, index: u64, config: &ProbeConfig, epoch_micros: i64) -> IngestionEvent {
    let (owner, session) = owner_session(index, config.distribution);
    let (user_id, session_id) = fixture_identity(run, owner, session);
    let previous_distance = match config.distribution {
        OwnerDistribution::Uniform => (OWNERS * SESSIONS_PER_OWNER) as u64,
        OwnerDistribution::HalfHotOwner if index.is_multiple_of(2) => 2 * SESSIONS_PER_OWNER as u64,
        OwnerDistribution::HalfHotOwner => (2 * (OWNERS - 1) * SESSIONS_PER_OWNER) as u64,
    };
    let parent_event_id = index
        .checked_sub(previous_distance)
        .filter(|_| index.is_multiple_of(10))
        .map(|previous| format!("probe-{run}-{previous}"));
    let timestamp = epoch_micros + arrival_offset(index, config.rate).as_micros() as i64;
    let target_bytes = target_envelope_bytes(index, config.seed);
    let mut event = IngestionEvent {
        event_id: format!("probe-{run}-{index}"),
        session_id,
        user_id,
        event_type: if index.is_multiple_of(5) {
            "user_query"
        } else {
            "llm_round"
        }
        .to_string(),
        content: Some(String::new()),
        token_usage: None,
        llm_model_used: None,
        skill_name: None,
        metadata: Some(json!({"probe_index": index, "target_bytes": target_bytes})),
        created_at: chrono::DateTime::from_timestamp_micros(timestamp)
            .unwrap()
            .to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
        parent_event_ids: parent_event_id.iter().cloned().collect(),
        parent_event_id,
        causal_chain_id: None,
        history_work_queue_reservation: None,
        ingestion_enqueued_at: None,
    };
    let base_bytes = serde_json::to_vec(&event).unwrap().len();
    assert!(base_bytes <= target_bytes);
    event.content = Some("x".repeat(target_bytes - base_bytes));
    event
}

struct Measurements {
    inserted: u64,
    replayed: u64,
    rejected_before: u64,
    rejected_after: u64,
    unknown_before: u64,
    unknown_after: u64,
    uncertain_commit: u64,
    schedule_to_terminal: Histogram,
    accepted_to_commit: Histogram,
    pool_wait: Histogram,
    limiter_wait: Histogram,
    transaction_time: Histogram,
    owner_committed: [u64; OWNERS],
}

impl Default for Measurements {
    fn default() -> Self {
        Self {
            inserted: 0,
            replayed: 0,
            rejected_before: 0,
            rejected_after: 0,
            unknown_before: 0,
            unknown_after: 0,
            uncertain_commit: 0,
            schedule_to_terminal: Histogram::default(),
            accepted_to_commit: Histogram::default(),
            pool_wait: Histogram::default(),
            limiter_wait: Histogram::default(),
            transaction_time: Histogram::default(),
            owner_committed: [0; OWNERS],
        }
    }
}

impl Measurements {
    fn record(&mut self, report: &IngestionDeliveryReport, scheduled: Instant, owner: usize) -> u8 {
        let accepted = report.progress.channel_accepted_at;
        let status = match report.terminal {
            IngestionDeliveryTerminal::CommittedInserted => {
                self.inserted += 1;
                2
            }
            IngestionDeliveryTerminal::CommittedReplayed => {
                self.replayed += 1;
                3
            }
            IngestionDeliveryTerminal::Rejected(_) => {
                if accepted.is_some() {
                    self.rejected_after += 1;
                } else {
                    self.rejected_before += 1;
                }
                4
            }
            IngestionDeliveryTerminal::Unknown(_) => {
                if accepted.is_some() {
                    self.unknown_after += 1;
                } else {
                    self.unknown_before += 1;
                }
                5
            }
        };
        if status == 2 || status == 3 {
            self.owner_committed[owner] += 1;
            self.accepted_to_commit.record(
                report
                    .terminal_at
                    .duration_since(accepted.expect("commit requires acceptance")),
            );
        }
        self.uncertain_commit += u64::from(report.progress.commit_was_uncertain);
        self.schedule_to_terminal
            .record(report.terminal_at.saturating_duration_since(scheduled));
        if report.progress.attempt_count > 0 {
            self.pool_wait.record(report.progress.pool_wait);
            self.limiter_wait.record(report.progress.limiter_wait);
            self.transaction_time
                .record(report.progress.transaction_time);
        }
        status
    }

    fn summary(&self) -> Value {
        json!({"committed_inserted":self.inserted,"committed_replayed":self.replayed,
            "rejected_before_acceptance":self.rejected_before,"rejected_after_acceptance":self.rejected_after,
            "unknown_before_acceptance":self.unknown_before,"unknown_after_acceptance":self.unknown_after,
            "commit_was_uncertain":self.uncertain_commit,"owner_committed":self.owner_committed.as_slice(),
            "scheduled_to_terminal":self.schedule_to_terminal.summary(),"accepted_to_commit_ack":self.accepted_to_commit.summary(),
            "delivery_experienced_pool_wait":self.pool_wait.summary(),"delivery_experienced_limiter_wait":self.limiter_wait.summary(),
            "delivery_experienced_transaction_time":self.transaction_time.summary()})
    }
}

async fn foreground_workload(
    pool: sqlx::MySqlPool,
    run: String,
    rate: NonZeroU64,
    seconds: u64,
) -> Value {
    let started = Instant::now();
    let mut tasks = tokio::task::JoinSet::<(Duration, Duration, bool)>::new();
    let mut completion = Histogram::default();
    let mut acquire = Histogram::default();
    let mut skipped = 0;
    let mut failed = 0;
    let mut index = 0;
    let count = rate.get() * seconds;
    while index < count || !tasks.is_empty() {
        tokio::select! {
            biased;
            Some(result) = tasks.join_next(), if !tasks.is_empty() => {
                let (elapsed, wait, success) = result.expect("foreground task panicked");
                completion.record(elapsed); acquire.record(wait); failed += u64::from(!success);
            }
            _ = tokio::time::sleep_until((started + arrival_offset(index, rate)).into()), if index < count => {
                let due = latest_due_index(started.elapsed(), rate).min(count - 1);
                skipped += due.saturating_sub(index); index = due;
                if tasks.len() >= 8 { skipped += 1; index += 1; continue; }
                let pool = pool.clone();
                let (owner, session) = fixture_identity(&run, index as usize % OWNERS, 0);
                let write = index.is_multiple_of(5);
                tasks.spawn(async move {
                    let started = Instant::now();
                    let mut wait = Duration::ZERO;
                    let outcome = tokio::time::timeout(Duration::from_secs(5), async {
                        let mut connection = pool.acquire().await?;
                        wait = started.elapsed();
                        if write {
                            sqlx::query("UPDATE agent_sessions SET title = 'capacity foreground' WHERE user_id = ? AND session_id = ?")
                                .bind(owner).bind(session).execute(&mut *connection).await?;
                        } else {
                            sqlx::query("SELECT status, event_count FROM agent_sessions WHERE user_id = ? AND session_id = ?")
                                .bind(owner).bind(session).fetch_one(&mut *connection).await?;
                        }
                        Ok::<(),sqlx::Error>(())
                    }).await;
                    if wait.is_zero() { wait = started.elapsed(); }
                    (started.elapsed(), wait, matches!(outcome, Ok(Ok(()))))
                });
                index += 1;
            }
        }
    }
    json!({"offered":count,"skipped":skipped,"failed":failed,"max_outstanding":8,
        "rate":rate.get(),"seconds":seconds,"completion":completion.summary(),"pool_acquire":acquire.summary()})
}

struct PendingDelivery {
    probe: IngestionDeliveryProbe,
    scheduled: Instant,
    owner: usize,
}

struct ProducerResult {
    statuses: Vec<u8>,
    generator_lag: Histogram,
    serialized_bytes: u64,
    priority_mix: [u64; 2],
    missed: u64,
    observer_unavailable: u64,
}

fn produce_arrivals(
    sender: astra_services::event_ingestion::IngestionSender,
    sink: IngestionMeasurementSink,
    metadata: tokio::sync::mpsc::Sender<(u64, PendingDelivery)>,
    config: ProbeConfig,
    run: String,
    clock: (Instant, i64),
    submitted: std::sync::Arc<std::sync::atomic::AtomicU64>,
) -> ProducerResult {
    use std::sync::atomic::Ordering;
    let (started, epoch_micros) = clock;
    let total = config.count();
    let offer_end = started + Duration::from_secs(config.warmup + config.seconds);
    let mut result = ProducerResult {
        statuses: vec![0; total as usize],
        generator_lag: Histogram::default(),
        serialized_bytes: 0,
        priority_mix: [0; 2],
        missed: 0,
        observer_unavailable: 0,
    };
    let mut index = 0;
    while index < total {
        if metadata.is_closed() {
            break;
        }
        let scheduled = started + arrival_offset(index, config.rate);
        std::thread::sleep(scheduled.saturating_duration_since(Instant::now()));
        if Instant::now() >= offer_end {
            result.statuses[index as usize..].fill(6);
            result.missed += total - index;
            break;
        }
        let due = latest_due_index(started.elapsed(), config.rate).min(total - 1);
        if due > index {
            result.statuses[index as usize..due as usize].fill(6);
            result.missed += due - index;
            index = due;
        }
        let scheduled = started + arrival_offset(index, config.rate);
        let event = probe_event(&run, index, &config, epoch_micros);
        result.serialized_bytes += target_envelope_bytes(index, config.seed) as u64;
        result.priority_mix[match event.priority() {
            IngestionEventPriority::Critical => 0,
            IngestionEventPriority::Telemetry => 1,
        }] += 1;
        result
            .generator_lag
            .record(Instant::now().saturating_duration_since(scheduled));
        // Reserve metadata before creating an observed token. If measurement
        // saturates, still offer the event without claiming complete evidence.
        match metadata.try_reserve() {
            Ok(permit) => match sink.try_start(IngestionDeliveryKey(index)) {
                Ok((token, probe)) => {
                    result.statuses[index as usize] = 1;
                    permit.send((
                        index,
                        PendingDelivery {
                            probe,
                            scheduled,
                            owner: owner_session(index, config.distribution).0,
                        },
                    ));
                    sender.enqueue_observed(event, token);
                }
                Err(_) => {
                    result.statuses[index as usize] = 7;
                    result.observer_unavailable += 1;
                    sender.enqueue(event);
                }
            },
            Err(_) => {
                result.statuses[index as usize] = 7;
                result.observer_unavailable += 1;
                sender.enqueue(event);
            }
        }
        submitted.fetch_add(1, Ordering::Relaxed);
        index += 1;
    }
    result
}

fn accept_metadata(
    key: u64,
    delivery: PendingDelivery,
    pending: &mut BTreeMap<u64, PendingDelivery>,
    early: &mut BTreeMap<u64, IngestionDeliveryReport>,
) -> Option<IngestionDeliveryReport> {
    assert!(
        pending.insert(key, delivery).is_none(),
        "unique delivery metadata"
    );
    early.remove(&key)
}

struct DrainCutoff {
    deadline: Instant,
    terminal: Measurements,
    late: u64,
}

fn consume_report(
    report: IngestionDeliveryReport,
    pending: &mut BTreeMap<u64, PendingDelivery>,
    statuses: &mut [u8],
    all: &mut Measurements,
    measured: &mut Measurements,
    warmup_count: u64,
    cutoff: &mut DrainCutoff,
) {
    let delivery = pending
        .remove(&report.key.0)
        .expect("exactly one report per observed delivery");
    statuses[report.key.0 as usize] = all.record(&report, delivery.scheduled, delivery.owner);
    if report.key.0 >= warmup_count {
        measured.record(&report, delivery.scheduled, delivery.owner);
    }
    if report.terminal_at <= cutoff.deadline {
        cutoff
            .terminal
            .record(&report, delivery.scheduled, delivery.owner);
    } else {
        cutoff.late += 1;
    }
}

fn rss_kib() -> Option<u64> {
    std::fs::read_to_string("/proc/self/status")
        .ok()?
        .lines()
        .find_map(|line| {
            line.strip_prefix("VmRSS:")
                .and_then(|value| value.split_whitespace().next()?.parse().ok())
        })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "opt-in sustained MatrixOne load; ASTRA_TEST_DB_IT=1"]
async fn sustained_ingestion_capacity() {
    use astra_services::observation_capture::canonical_agent_event_payload_hash;
    use futures_util::TryStreamExt;

    assert!(
        std::env::var("ASTRA_DATABASE").is_ok_and(|name| name.starts_with("astra_test_probe_")),
        "capacity probes require an explicit dedicated astra_test_probe_* database"
    );
    let config = ProbeConfig::from_env();
    let total = config.count();
    let (_, mut settings) = common::setup_pool_and_settings().await;
    settings.db_pool_max_connections = config.pool_size;
    settings.db_pool_min_connections = 0;
    let shared = astra_core::SharedPool::new(&settings)
        .await
        .expect("probe pool");
    let pool = shared.get().clone();
    let run = uuid::Uuid::new_v4().simple().to_string();
    let owner_like = format!("ingestion-probe-{run}-%");
    for start in (0..OWNERS * SESSIONS_PER_OWNER).step_by(100) {
        let mut query = sqlx::QueryBuilder::<sqlx::MySql>::new(
            "INSERT INTO agent_sessions (user_id, session_id, title, status, event_count) ",
        );
        query.push_values(
            start..(start + 100).min(OWNERS * SESSIONS_PER_OWNER),
            |mut row, index| {
                let (owner, session) =
                    fixture_identity(&run, index / SESSIONS_PER_OWNER, index % SESSIONS_PER_OWNER);
                row.push_bind(owner)
                    .push_bind(session)
                    .push_bind("capacity probe")
                    .push_bind("active")
                    .push_bind(0_i64);
            },
        );
        query
            .build()
            .execute(&pool)
            .await
            .expect("probe session roots");
    }
    let foreground_baseline = foreground_workload(
        pool.clone(),
        run.clone(),
        config.foreground_rate,
        config.baseline_seconds,
    )
    .await;
    let ingestion = IngestionConfig::default();
    let ingestion_config = format!("{ingestion:?}");
    let attempt_timeout = ingestion.db_attempt_timeout_secs;
    let observer_capacity = ingestion.channel_capacity * 2;
    let (sender, shutdown, stats, mut worker) =
        EventIngestionWorker::spawn(pool.clone(), ingestion);
    let (sink, mut reports) = IngestionMeasurementSink::bounded(observer_capacity);
    let foreground = tokio::spawn(foreground_workload(
        pool.clone(),
        run.clone(),
        config.foreground_rate,
        config.warmup + config.seconds,
    ));
    let started = Instant::now();
    let epoch_micros = chrono::Utc::now().timestamp_micros();
    let warmup_count = config.warmup * config.rate.get();
    let mut pending = BTreeMap::<u64, PendingDelivery>::new();
    // 0=not offered,1=pending,2=insert,3=replay,4=reject,5=unknown,
    // 6=generator missed,7=measurement unavailable. Bounded by config.count().
    let mut statuses = vec![0_u8; total as usize];
    let mut all = Measurements::default();
    let mut measured = Measurements::default();
    let (metadata_tx, mut metadata_rx) = tokio::sync::mpsc::channel(observer_capacity);
    let submitted = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let producer = {
        let sender = sender.clone();
        let config = config.clone();
        let run = run.clone();
        let submitted = submitted.clone();
        tokio::task::spawn_blocking(move || {
            produce_arrivals(
                sender,
                sink,
                metadata_tx,
                config,
                run,
                (started, epoch_micros),
                submitted,
            )
        })
    };
    let mut producer_finished = false;
    let mut early_reports = BTreeMap::new();
    let mut next_sample = started + Duration::from_secs(1);
    let mut samples = Vec::new();
    let offer_end = started + Duration::from_secs(config.warmup + config.seconds);
    let drain_end = offer_end + Duration::from_secs(config.drain);
    let mut cutoff = DrainCutoff {
        deadline: drain_end,
        terminal: Measurements::default(),
        late: 0,
    };

    while Instant::now() < drain_end
        && (!producer_finished || !pending.is_empty() || !early_reports.is_empty())
    {
        tokio::select! {
            biased;
            metadata=metadata_rx.recv(), if !producer_finished => {
                if let Some((key,delivery)) = metadata {
                    if let Some(report)=accept_metadata(key,delivery,&mut pending,&mut early_reports) {
                        consume_report(report,&mut pending,&mut statuses,&mut all,&mut measured,warmup_count,&mut cutoff);
                    }
                } else {producer_finished=true;}
            }
            Some(report) = reports.recv() => {
                if pending.contains_key(&report.key.0) {
                    consume_report(report, &mut pending, &mut statuses, &mut all, &mut measured, warmup_count, &mut cutoff);
                } else {
                    assert!(early_reports.insert(report.key.0,report).is_none());
                    assert!(early_reports.len()<=observer_capacity,"bounded report/metadata publication race");
                }
            }
            _ = tokio::time::sleep_until(next_sample.into()) => {
                let now = Instant::now();
                let oldest = pending.values().filter_map(|delivery| delivery.probe.snapshot().channel_accepted_at)
                    .map(|accepted| now.saturating_duration_since(accepted)).max().unwrap_or_default();
                let snapshot = astra_core::sync_poison::recover_mutex_lock(&stats).clone();
                samples.push(json!({"elapsed_ms":started.elapsed().as_millis(),"resident_events":snapshot.resident_events_current,
                    "resident_bytes":snapshot.resident_bytes_current,"db_attempts":snapshot.db_attempts_current,
                    "pool_size":pool.size(),"pool_idle":pool.num_idle(),"oldest_accepted_ms":oldest.as_millis(),
                    "outstanding_observations":pending.len(),"rss_kib":rss_kib(),"owner_committed":all.owner_committed.as_slice()}));
                if samples.len().is_multiple_of(60) {
                    println!("INGESTION_CAPACITY_PROGRESS elapsed_s={} submitted={} committed={} outstanding={}",
                        started.elapsed().as_secs(),submitted.load(std::sync::atomic::Ordering::Relaxed),all.inserted+all.replayed,pending.len());
                }
                next_sample = now + Duration::from_secs(1);
            }
            _ = tokio::time::sleep_until(drain_end.into()) => break,
        }
    }
    let ProducerResult {
        statuses: offered_statuses,
        generator_lag,
        serialized_bytes,
        priority_mix,
        missed,
        observer_unavailable,
    } = producer.await.expect("arrival generator");
    for (status, offered) in statuses.iter_mut().zip(offered_statuses) {
        if *status == 0 {
            *status = offered;
        }
    }
    // Metadata is published before enqueue, but the two receiver wakeups can
    // race. Drain any remaining bounded metadata before final reports.
    while let Ok((key, delivery)) = metadata_rx.try_recv() {
        if let Some(report) = accept_metadata(key, delivery, &mut pending, &mut early_reports) {
            consume_report(
                report,
                &mut pending,
                &mut statuses,
                &mut all,
                &mut measured,
                warmup_count,
                &mut cutoff,
            );
        }
    }
    assert!(
        early_reports.is_empty(),
        "all terminal reports have delivery metadata"
    );
    let shutdown_started = Instant::now();
    let outstanding_at_shutdown = pending.len();
    shutdown.signal();
    sender.shutdown();
    let forced_abort =
        match tokio::time::timeout(Duration::from_secs(attempt_timeout + 10), &mut worker).await {
            Err(_) => {
                worker.abort();
                let _ = worker.await;
                true
            }
            Ok(result) => {
                result.expect("ingestion worker must not panic");
                false
            }
        };
    while let Ok(report) = reports.try_recv() {
        consume_report(
            report,
            &mut pending,
            &mut statuses,
            &mut all,
            &mut measured,
            warmup_count,
            &mut cutoff,
        );
    }
    let shutdown_grace_elapsed = shutdown_started.elapsed();
    let foreground = foreground.await.expect("foreground workload");
    let final_stats = astra_core::sync_poison::recover_mutex_lock(&stats).clone();
    let mut durable_count = 0_u64;
    let mut reconciliation_unknown = 0_u64;
    let mut expected_sessions = BTreeMap::new();
    for owner in 0..OWNERS {
        for session in 0..SESSIONS_PER_OWNER {
            expected_sessions.insert(fixture_identity(&run, owner, session), 0_i64);
        }
    }
    let mut expected_edges = BTreeSet::new();
    let mut rows = sqlx::query(
        "SELECT event_id, user_id, session_id, payload_hash FROM agent_events WHERE user_id LIKE ?",
    )
    .bind(&owner_like)
    .fetch(&pool);
    while let Some(row) = rows.try_next().await.expect("reconcile durable events") {
        let event_id: String = row.get("event_id");
        let index: u64 = event_id
            .rsplit('-')
            .next()
            .unwrap()
            .parse()
            .expect("probe event index");
        let expected = probe_event(&run, index, &config, epoch_micros);
        assert_eq!(event_id, expected.event_id);
        assert_eq!(row.get::<String, _>("user_id"), expected.user_id);
        assert_eq!(row.get::<String, _>("session_id"), expected.session_id);
        assert_eq!(
            row.get::<String, _>("payload_hash"),
            canonical_agent_event_payload_hash(&serde_json::to_value(&expected).unwrap())
        );
        assert!(
            matches!(statuses[index as usize], 1 | 2 | 3 | 5 | 7),
            "rejected or unoffered delivery appeared durably"
        );
        if !matches!(statuses[index as usize], 2 | 3) {
            reconciliation_unknown += 1;
        }
        *expected_sessions
            .get_mut(&(expected.user_id.clone(), expected.session_id.clone()))
            .unwrap() += 1;
        if let Some(parent) = expected.parent_event_id {
            expected_edges.insert((
                expected.user_id,
                expected.session_id,
                expected.event_id,
                parent,
            ));
        }
        durable_count += 1;
    }
    drop(rows);
    assert_eq!(
        durable_count - reconciliation_unknown,
        all.inserted + all.replayed
    );
    let session_counts = sqlx::query(
        "SELECT user_id, session_id, event_count FROM agent_sessions WHERE user_id LIKE ?",
    )
    .bind(&owner_like)
    .fetch_all(&pool)
    .await
    .expect("reconcile session counters");
    assert_eq!(session_counts.len(), OWNERS * SESSIONS_PER_OWNER);
    let actual_sessions = session_counts
        .iter()
        .map(|row| {
            (
                (
                    row.get::<String, _>("user_id"),
                    row.get::<String, _>("session_id"),
                ),
                row.get::<i64, _>("event_count"),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let edge_rows = sqlx::query("SELECT user_id, session_id, child_event_id, parent_event_id FROM agent_event_edges WHERE user_id LIKE ?")
            .bind(&owner_like)
            .fetch_all(&pool)
            .await
            .expect("reconcile lineage edges");
    let actual_edges = edge_rows
        .iter()
        .map(|row| {
            (
                row.get::<String, _>("user_id"),
                row.get::<String, _>("session_id"),
                row.get::<String, _>("child_event_id"),
                row.get::<String, _>("parent_event_id"),
            )
        })
        .collect::<BTreeSet<_>>();
    assert!(
        projection_matches(
            &expected_sessions,
            &actual_sessions,
            &expected_edges,
            &actual_edges
        ),
        "per-session counters or owner/session/child/parent edge identities differ"
    );
    assert_eq!(actual_edges.len(), edge_rows.len());
    let accounted = statuses.iter().filter(|status| **status != 0).count() as u64;
    assert_eq!(accounted, total);
    let mut output = json!({"schema_version":1,"scope":"single_process_ingestion_shared_pool",
        "users":OWNERS,"sessions":OWNERS*SESSIONS_PER_OWNER,"rate":config.rate.get(),
        "warmup_seconds":config.warmup,"measurement_seconds":config.seconds,"drain_seconds":config.drain,
        "distribution":format!("{:?}",config.distribution),"seed":config.seed,"pool_max":config.pool_size,
        "producer":"one dedicated absolute-deadline blocking thread; bounded metadata channel",
        "ingestion_config":ingestion_config,"observation_capacity":observer_capacity,
        "scheduled":total,"submitted":total-missed,"generator_missed":missed,"generator_lag":generator_lag.summary(),
        "serialized_bytes_submitted":serialized_bytes,"measurement_unavailable":observer_unavailable,
        "priority_mix_submitted":{"critical":priority_mix[0],"telemetry":priority_mix[1]},
        "unresolved_observations":pending.len(),"forced_worker_abort":forced_abort});
    let metrics = json!({"all":all.summary(),"measured_arrivals":measured.summary(),"samples":samples,
        "foreground_baseline":foreground_baseline,"foreground_contended":foreground,
        "resident_bytes_peak":final_stats.resident_bytes_peak,"resident_events_peak":final_stats.resident_events_peak,
        "db_attempts_peak":final_stats.db_attempts_peak,"resident_events_after_shutdown":final_stats.resident_events_current,
        "reconciled_durable":durable_count,"reconciled_unknown_durable":reconciliation_unknown,"reconciled_edges":actual_edges.len(),
        "outcomes_by_declared_drain_deadline":cutoff.terminal.summary(),"late_terminal_resolutions":cutoff.late,
        "unresolved_at_declared_drain_deadline":total-missed-observer_unavailable-cutoff.terminal.schedule_to_terminal.count,
        "shutdown_started_ms":shutdown_started.duration_since(started).as_millis(),"outstanding_at_shutdown":outstanding_at_shutdown,
        "shutdown_grace_ms":shutdown_grace_elapsed.as_millis(),"shutdown_grace_limit_seconds":attempt_timeout+10,
        "measurement_complete":observer_unavailable==0 && pending.is_empty() && !forced_abort,
        "offered_rate_within_0_1_percent":missed*1_000<=total,
        "duration_eligible_for_sustained_claim":config.seconds>=600 && config.warmup>=60});
    let Value::Object(metrics) = metrics else {
        unreachable!()
    };
    output.as_object_mut().unwrap().extend(metrics);
    for query in [
        "DELETE FROM observation_identity_collisions WHERE user_id LIKE ?",
        "DELETE FROM agent_event_edges WHERE user_id LIKE ?",
        "DELETE FROM agent_events WHERE user_id LIKE ?",
        "DELETE FROM agent_session_lifecycle_fences WHERE user_id LIKE ?",
        "DELETE FROM agent_sessions WHERE user_id LIKE ?",
    ] {
        sqlx::query(query)
            .bind(&owner_like)
            .execute(&pool)
            .await
            .expect("cleanup this probe's UUID-scoped fixtures");
    }
    println!("INGESTION_CAPACITY_RESULT {output}");
}

type SessionCounts = BTreeMap<(String, String), i64>;
type EdgeIdentities = BTreeSet<(String, String, String, String)>;

fn projection_matches(
    expected_sessions: &SessionCounts,
    actual_sessions: &SessionCounts,
    expected_edges: &EdgeIdentities,
    actual_edges: &EdgeIdentities,
) -> bool {
    expected_sessions == actual_sessions && expected_edges == actual_edges
}

#[test]
fn uniform_workload_visits_every_owner_session_once_per_cycle() {
    let mut counts = [[0; SESSIONS_PER_OWNER]; OWNERS];
    for index in 0..(OWNERS * SESSIONS_PER_OWNER) as u64 {
        let (owner, session) = owner_session(index, OwnerDistribution::Uniform);
        counts[owner][session] += 1;
    }
    assert!(counts.iter().flatten().all(|count| *count == 1));
}

#[test]
fn generated_priority_mix_and_serialized_envelope_sizes_are_real() {
    let config = ProbeConfig {
        rate: NonZeroU64::new(500).unwrap(),
        warmup: 60,
        seconds: 600,
        drain: 60,
        pool_size: 32,
        foreground_rate: NonZeroU64::new(20).unwrap(),
        baseline_seconds: 10,
        distribution: OwnerDistribution::Uniform,
        seed: 42,
    };
    let mut counts = [0; 2];
    for index in 0..2000 {
        let event = probe_event(
            "0123456789abcdef0123456789abcdef",
            index,
            &config,
            1_700_000_000_000_000,
        );
        counts[match event.priority() {
            IngestionEventPriority::Critical => 0,
            IngestionEventPriority::Telemetry => 1,
        }] += 1;
        assert_eq!(
            serde_json::to_vec(&event).unwrap().len(),
            target_envelope_bytes(index, config.seed)
        );
    }
    assert_eq!(counts, [400, 1600]);
}

#[test]
fn reconciliation_rejects_offsetting_counters_and_substituted_parents() {
    let expected_sessions = BTreeMap::from([
        (("owner".to_string(), "a".to_string()), 1),
        (("owner".to_string(), "b".to_string()), 1),
    ]);
    let expected_edges =
        BTreeSet::from([("owner".into(), "a".into(), "child".into(), "parent".into())]);
    assert!(projection_matches(
        &expected_sessions,
        &expected_sessions,
        &expected_edges,
        &expected_edges
    ));
    let mut wrong_sessions = expected_sessions.clone();
    *wrong_sessions
        .get_mut(&("owner".into(), "a".into()))
        .unwrap() = 2;
    *wrong_sessions
        .get_mut(&("owner".into(), "b".into()))
        .unwrap() = 0;
    assert!(!projection_matches(
        &expected_sessions,
        &wrong_sessions,
        &expected_edges,
        &expected_edges
    ));
    let wrong_edges = BTreeSet::from([(
        "owner".into(),
        "a".into(),
        "child".into(),
        "wrong-parent".into(),
    )]);
    assert!(!projection_matches(
        &expected_sessions,
        &expected_sessions,
        &expected_edges,
        &wrong_edges
    ));
}

#[test]
fn shutdown_grace_cannot_rewrite_drain_deadline_accounting() {
    let started = Instant::now();
    let deadline = started + Duration::from_millis(10);
    let (sink, _receiver) = IngestionMeasurementSink::bounded(2);
    let mut pending = BTreeMap::new();
    let mut statuses = [1, 1];
    let mut all = Measurements::default();
    let mut measured = Measurements::default();
    let mut cutoff = DrainCutoff {
        deadline,
        terminal: Measurements::default(),
        late: 0,
    };
    for index in 0..2 {
        let (_token, probe) = sink.try_start(IngestionDeliveryKey(index)).unwrap();
        let mut progress = probe.snapshot();
        progress.channel_accepted_at = Some(started);
        pending.insert(
            index,
            PendingDelivery {
                probe,
                scheduled: started,
                owner: 0,
            },
        );
        let terminal_at = if index == 0 {
            deadline - Duration::from_millis(1)
        } else {
            deadline + Duration::from_millis(1)
        };
        consume_report(
            IngestionDeliveryReport {
                key: IngestionDeliveryKey(index),
                terminal: IngestionDeliveryTerminal::CommittedInserted,
                terminal_at,
                progress,
            },
            &mut pending,
            &mut statuses,
            &mut all,
            &mut measured,
            0,
            &mut cutoff,
        );
    }
    assert_eq!(all.inserted, 2);
    assert_eq!(cutoff.terminal.inserted, 1);
    assert_eq!(cutoff.late, 1);
    assert_eq!(statuses, [2, 2]);
}

#[test]
fn latency_histogram_bounds_and_sample_count_are_explicit() {
    let mut histogram = Histogram::default();
    assert_eq!(histogram.percentile(95), None);
    for micros in [0, 1, 10, 31, 32, 33, 1_001, 1_000_000] {
        histogram.record(Duration::from_micros(micros));
        let upper = histogram.percentile(100).unwrap();
        assert!(upper >= micros);
        assert!(upper <= micros.saturating_add(micros.div_ceil(16)));
    }
    assert_eq!(histogram.count, 8);
    assert_eq!(histogram.buckets.values().sum::<u64>(), 8);
}

#[test]
fn hot_owner_does_not_starve_the_generated_cold_owner_workload() {
    let mut counts = [[0; SESSIONS_PER_OWNER]; OWNERS];
    for index in 0..(2 * (OWNERS - 1) * SESSIONS_PER_OWNER) as u64 {
        let (owner, session) = owner_session(index, OwnerDistribution::HalfHotOwner);
        counts[owner][session] += 1;
    }
    assert!(counts[0].iter().all(|count| *count == OWNERS - 1));
    assert!(counts[1..].iter().flatten().all(|count| *count == 1));
}

#[test]
fn open_loop_deadlines_do_not_drift_or_create_catch_up_bursts() {
    let rate = NonZeroU64::new(3).unwrap();
    assert_eq!(arrival_offset(3, rate), Duration::from_secs(1));
    for index in 1..1_000 {
        let deadline = arrival_offset(index, rate);
        assert_eq!(latest_due_index(deadline, rate), index);
        assert_eq!(
            latest_due_index(deadline - Duration::from_nanos(1), rate),
            index - 1
        );
    }
    // If submission stalls for two seconds, the generator must report six
    // missed arrivals rather than send them in a completion-driven burst.
    assert_eq!(latest_due_index(Duration::from_secs(2), rate), 6);
}

#[test]
fn mixed_sizes_are_reproducible_and_include_large_envelopes() {
    let sample = (0..10_000)
        .map(|index| target_envelope_bytes(index, 42))
        .collect::<Vec<_>>();
    assert_eq!(
        sample,
        (0..10_000)
            .map(|index| target_envelope_bytes(index, 42))
            .collect::<Vec<_>>()
    );
    for (size, low, high) in [
        (1_024, 7_700, 8_300),
        (16 * 1_024, 1_200, 1_800),
        (128 * 1_024, 300, 700),
    ] {
        let count = sample.iter().filter(|value| **value == size).count();
        assert!((low..=high).contains(&count), "size={size}, count={count}");
    }
}
