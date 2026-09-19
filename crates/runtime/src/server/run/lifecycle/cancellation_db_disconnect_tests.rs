//! Withhold a real MatrixOne BEGIN acknowledgement, then drop the same future
//! that a disconnected HTTP request would drop. Never log protocol payloads.
use super::*;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

async fn packet(reader: &mut (impl AsyncRead + Unpin)) -> std::io::Result<Vec<u8>> {
    let mut header = [0; 4];
    reader.read_exact(&mut header).await?;
    let length =
        usize::from(header[0]) | (usize::from(header[1]) << 8) | (usize::from(header[2]) << 16);
    let mut bytes = vec![0; length + 4];
    bytes[..4].copy_from_slice(&header);
    reader.read_exact(&mut bytes[4..]).await?;
    Ok(bytes)
}

struct BeginAckProxy {
    port: u16,
    armed: Arc<AtomicBool>,
    entered: Arc<Notify>,
    release: Arc<Notify>,
    task: tokio::task::JoinHandle<()>,
}

impl BeginAckProxy {
    async fn new(settings: &MatrixOneSettings) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let upstream = (settings.host.clone(), settings.port);
        let armed = Arc::new(AtomicBool::new(false));
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let (armed_task, entered_task, release_task) =
            (armed.clone(), entered.clone(), release.clone());
        let task = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                let (mut client, _) = listener.accept().await.unwrap();
                let mut server = TcpStream::connect((upstream.0.as_str(), upstream.1))
                    .await
                    .unwrap();
                let (armed, entered, release) = (
                    armed_task.clone(),
                    entered_task.clone(),
                    release_task.clone(),
                );
                connections.spawn(async move {
                    let (mut client_read, mut client_write) = client.split();
                    let (mut server_read, mut server_write) = server.split();
                    let begin_pending = AtomicBool::new(false);
                    let requests = async {
                        loop {
                            let bytes = packet(&mut client_read).await?;
                            let is_begin = bytes.get(4) == Some(&3)
                                && std::str::from_utf8(&bytes[5..]).is_ok_and(|sql| {
                                    let sql = sql.trim().trim_end_matches(';');
                                    sql.eq_ignore_ascii_case("BEGIN")
                                        || sql.eq_ignore_ascii_case("START TRANSACTION")
                                });
                            if is_begin && armed.swap(false, Ordering::SeqCst) {
                                begin_pending.store(true, Ordering::SeqCst);
                            }
                            server_write.write_all(&bytes).await?;
                        }
                        #[allow(unreachable_code)]
                        Ok::<(), std::io::Error>(())
                    };
                    let responses = async {
                        loop {
                            let bytes = packet(&mut server_read).await?;
                            if begin_pending.swap(false, Ordering::SeqCst) {
                                entered.notify_one();
                                release.notified().await;
                            }
                            client_write.write_all(&bytes).await?;
                        }
                        #[allow(unreachable_code)]
                        Ok::<(), std::io::Error>(())
                    };
                    tokio::select! {
                        _ = requests => {},
                        _ = responses => {},
                    }
                });
            }
        });
        Self {
            port,
            armed,
            entered,
            release,
            task,
        }
    }
}

impl Drop for BeginAckProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[tokio::test]
#[ignore = "requires disposable MatrixOne: ASTRA_TEST_DB_IT=1"]
async fn db_cancel_session_disconnect_during_begin_discards_connection() {
    let fixture = CancellationFixture::new(false, true).await;
    let proxy = BeginAckProxy::new(fixture.pool.settings()).await;
    let mut settings = fixture.pool.settings().clone();
    settings.host = "127.0.0.1".into();
    settings.port = proxy.port;
    settings.db_pool_min_connections = 1;
    settings.db_pool_max_connections = 1;
    let pool = SharedPool::new(&settings).await.unwrap();
    for operation in [
        "idle_proof",
        "terminal_writer",
        "active_writer",
        "tool_reconcile",
        "session_update",
    ] {
        let before: u64 = sqlx::query_scalar("SELECT CONNECTION_ID()")
            .fetch_one(pool.get())
            .await
            .unwrap();
        proxy.armed.store(true, Ordering::SeqCst);
        let (task_pool, key, writer, run_id) = (
            pool.clone(),
            fixture.key.clone(),
            fixture.writer.clone(),
            fixture.run_id.clone(),
        );
        let task = tokio::spawn(async move {
            let coordinator = DatabaseSessionContextCoordinator::new(task_pool.clone());
            match operation {
                "idle_proof" => {
                    let _ = coordinator.execution_reuse_blocker(&key).await;
                }
                "terminal_writer" => {
                    let _ = coordinator
                        .release_terminal_execution_writer(&writer, &run_id, 0)
                        .await;
                }
                "active_writer" => {
                    let _ = coordinator.load_active_writer(&key).await;
                }
                "tool_reconcile" => {
                    let ledger =
                        astra_services::tool_invocation_ledger::DatabaseToolInvocationLedger::new(
                            task_pool,
                        );
                    let _ = ledger
                        .reconcile_terminal_run(&key.owner_user_id, &key.session_id, &run_id)
                        .await;
                }
                _ => {
                    use astra_services::SessionService;
                    let sessions =
                        astra_services::DatabaseSessionService::new(task_pool.settings().clone())
                            .with_pool(task_pool);
                    let _ = sessions
                        .update_session(
                            key.session_id,
                            key.owner_user_id,
                            astra_services::SessionUpdateRequestData {
                                title: None,
                                metadata: None,
                                metadata_patch: None,
                                status: Some(STATUS_CANCELLED.into()),
                            },
                        )
                        .await;
                }
            }
        });
        tokio::time::timeout(Duration::from_secs(5), proxy.entered.notified())
            .await
            .expect("fixture must intercept the real BEGIN response");
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        // Do not leave a permit for the next operation if closing this
        // connection already cancelled the response-forwarding future.
        proxy.release.notify_waiters();
        let after: u64 = tokio::time::timeout(
            Duration::from_secs(5),
            sqlx::query_scalar("SELECT CONNECTION_ID()").fetch_one(pool.get()),
        )
        .await
        .expect("pool must remain usable after disconnect")
        .unwrap();
        assert_ne!(
            before, after,
            "{operation} returned an untracked transaction to the pool"
        );
        eprintln!("disconnect-safe BEGIN/pool replacement passed: {operation}");
    }
    pool.get().close().await;
    fixture.cleanup().await;
}
