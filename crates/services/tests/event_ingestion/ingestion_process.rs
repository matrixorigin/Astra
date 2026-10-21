//! Real subprocess coverage for durable ingestion identity and deletion fences.
use super::*;
use astra_services::event_ingestion::measurement::{
    IngestionDeliveryKey, IngestionDeliveryTerminal, IngestionMeasurementSink,
    IngestionRejectionReason,
};
use futures_util::FutureExt;
use std::{panic::AssertUnwindSafe, process::Stdio, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const CHILD: &str = "ingestion_process::child";
const PREFIX: &str = "INGESTION_IPC ";

async fn command(reader: &mut BufReader<tokio::io::Stdin>, expected: &str) {
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(15), reader.read_line(&mut line))
        .await
        .expect("parent command deadline")
        .expect("parent command");
    assert_eq!(line.trim(), expected);
}

fn signal(value: &str) {
    use std::io::Write;
    println!("{PREFIX}{value}");
    std::io::stdout().flush().unwrap();
}

#[tokio::test]
#[ignore = "subprocess entrypoint; invoked only by cross_process_identity_and_delete_fence"]
async fn child() {
    let Ok(owner) = std::env::var("ASTRA_INGESTION_CHILD_OWNER") else {
        return;
    };
    assert!(owner.starts_with("ingestion-process-"));
    let role = std::env::var("ASTRA_INGESTION_CHILD_ROLE").unwrap();
    let settings = common::require_db_it_env();
    // Parent bootstrapped once. Children must not repeat DDL behind its fence.
    let shared = astra_core::SharedPool::new(&settings).await.unwrap();
    let pool = shared.get().clone();
    let (sender, shutdown, stats, worker) = EventIngestionWorker::spawn(
        pool,
        IngestionConfig {
            batch_size: 1,
            flush_interval_secs: 300,
            max_concurrent_session_flushes: 2,
            db_attempt_timeout_secs: 10,
            ..Default::default()
        },
    );
    let (sink, mut reports) = IngestionMeasurementSink::bounded(3);
    let mut input = BufReader::new(tokio::io::stdin());
    signal("ready");
    command(&mut input, "start").await;
    let mut event = test_event_for_user(&owner, "duplicate", "shared", "user_query");
    event.content = Some("identical frozen cross-process envelope".into());
    event.parent_event_id = Some("parent".into());
    let (token, probe) = sink.try_start(IngestionDeliveryKey(0)).unwrap();
    sender.enqueue_observed(event, token);
    tokio::time::timeout(Duration::from_secs(2), async {
        while probe.snapshot().first_dispatched_at.is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    signal("dispatched");
    if role == "0" {
        let (token, _) = sink.try_start(IngestionDeliveryKey(1)).unwrap();
        sender.enqueue_observed(
            test_event_for_user(&owner, "healthy", "healthy", "user_query"),
            token,
        );
        let report = tokio::time::timeout(Duration::from_secs(3), reports.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(report.key, IngestionDeliveryKey(1));
        assert_eq!(
            report.terminal,
            IngestionDeliveryTerminal::CommittedInserted
        );
        signal("healthy");
    }
    let report = tokio::time::timeout(Duration::from_secs(12), reports.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(report.key, IngestionDeliveryKey(0));
    match report.terminal {
        IngestionDeliveryTerminal::CommittedInserted => signal("inserted"),
        IngestionDeliveryTerminal::CommittedReplayed => signal("replayed"),
        other => panic!("unexpected duplicate outcome: {other:?}"),
    }
    if role == "1" {
        command(&mut input, "deleted").await;
        let (token, _) = sink.try_start(IngestionDeliveryKey(2)).unwrap();
        sender.enqueue_observed(
            test_event_for_user(&owner, "late", "shared", "user_query"),
            token,
        );
        let report = tokio::time::timeout(Duration::from_secs(3), reports.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            report.terminal,
            IngestionDeliveryTerminal::Rejected(IngestionRejectionReason::SessionAdmission)
        );
        signal("rejected");
    }
    shutdown.signal();
    sender.shutdown();
    tokio::time::timeout(Duration::from_secs(3), worker)
        .await
        .unwrap()
        .unwrap();
    assert!(reports.try_recv().is_err());
    assert_eq!(
        astra_core::sync_poison::recover_mutex_lock(&stats).resident_events_current,
        0
    );
}

struct Child {
    process: tokio::process::Child,
    output: BufReader<tokio::process::ChildStdout>,
}

impl Child {
    fn spawn(owner: &str, role: &str) -> Self {
        let mut process = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([CHILD, "--exact", "--ignored", "--nocapture"])
            .env("ASTRA_INGESTION_CHILD_OWNER", owner)
            .env("ASTRA_INGESTION_CHILD_ROLE", role)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let output = BufReader::new(process.stdout.take().unwrap());
        Self { process, output }
    }

    async fn send(&mut self, value: &str) {
        let input = self.process.stdin.as_mut().unwrap();
        input
            .write_all(format!("{value}\n").as_bytes())
            .await
            .unwrap();
        input.flush().await.unwrap();
    }

    async fn receive(&mut self) -> String {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let mut line = String::new();
                assert_ne!(
                    self.output.read_line(&mut line).await.unwrap(),
                    0,
                    "child exited before IPC result"
                );
                if let Some((_, value)) = line.trim().split_once(PREFIX) {
                    return value.to_string();
                }
                // Do not forward raw logs, endpoints, or credentials.
            }
        })
        .await
        .expect("bounded child IPC response")
    }
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne; two real child processes"]
async fn cross_process_identity_and_delete_fence() {
    let shared = common::setup_pool().await;
    let pool = shared.get().clone();
    let owner = format!("ingestion-process-{}", Uuid::new_v4());
    for session in ["shared", "healthy"] {
        insert_session_root(&pool, &owner, session).await;
    }
    let mut fence = pool.begin().await.unwrap();
    admit_session_event_write(&mut fence, "shared", &owner, true)
        .await
        .unwrap();
    let mut children = Vec::with_capacity(2);
    // Catch assertion failures so every started child is killed and reaped.
    let outcome = AssertUnwindSafe(async {
        children.push(Child::spawn(&owner, "0"));
        children.push(Child::spawn(&owner, "1"));
        assert_ne!(children[0].process.id(), children[1].process.id());
        for child in &mut children { assert_eq!(child.receive().await, "ready"); }
        for child in &mut children { child.send("start").await; }
        for child in &mut children { assert_eq!(child.receive().await, "dispatched"); }
        assert_eq!(children[0].receive().await, "healthy");
        assert_session_event_count(&pool, &owner, "healthy", 1).await;
        assert_session_event_count(&pool, &owner, "shared", 0).await;
        fence.rollback().await.unwrap();
        let first = children[0].receive().await;
        let second = children[1].receive().await;
        assert_eq!(std::collections::BTreeSet::from([first.as_str(), second.as_str()]),
            std::collections::BTreeSet::from(["inserted", "replayed"]));
        assert_session_event_count(&pool, &owner, "shared", 1).await;
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_events WHERE user_id = ? AND session_id = ? AND event_id = ?")
            .bind(&owner).bind("shared").bind("duplicate").fetch_one(&pool).await.unwrap();
        assert_eq!(rows, 1);
        let parents = load_agent_event_parent_ids(&pool, &owner, &["duplicate".to_string()]).await.unwrap();
        assert_eq!(parents.get("duplicate").unwrap(), &vec!["parent".to_string()]);
        DatabaseSessionService::new(astra_core::MatrixOneSettings::from_env())
            .with_pool(shared.clone()).delete_session("shared".into(), owner.clone()).await.unwrap();
        children[1].send("deleted").await;
        assert_eq!(children[1].receive().await, "rejected");
        for table in ["agent_sessions", "agent_events"] {
            let rows: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT COUNT(*) FROM {table} WHERE user_id = ? AND session_id = ?")))
                .bind(&owner).bind("shared").fetch_one(&pool).await.unwrap();
            assert_eq!(rows, 0, "late write cannot resurrect deleted data");
        }
        for child in &mut children {
            assert!(tokio::time::timeout(Duration::from_secs(3), child.process.wait()).await.unwrap().unwrap().success());
        }
    }).catch_unwind().await;
    for child in &mut children {
        let _ = child.process.start_kill();
        child.process.wait().await.expect("reap owned child");
    }
    for session in ["shared", "healthy"] {
        cleanup_session(&pool, &owner, session).await;
    }
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}
