//! Real MySQL transport coverage for returned-connection shutdown ownership.
use astra_core::{MatrixOneSettings, SharedPool};
use sqlx::Connection;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires ASTRA_TEST_DB_IT=1 and the configured MatrixOne test instance"]
async fn closing_pool_discards_a_connection_with_a_pending_return_ping() {
    check_return_shutdown(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires ASTRA_TEST_DB_IT=1 and the configured MatrixOne test instance"]
async fn closing_pool_during_queued_rollback_releases_transaction_and_key() {
    check_return_shutdown(true).await;
}

async fn check_return_shutdown(block_rollback: bool) {
    assert_eq!(std::env::var("ASTRA_TEST_DB_IT").as_deref(), Ok("1"));
    let mut settings = MatrixOneSettings::from_env();
    let mut admin = if block_rollback {
        Some(
            sqlx::MySqlConnection::connect(&settings.database_url_with_password())
                .await
                .unwrap(),
        )
    } else {
        None
    };
    // The only interpolated SQL identifier is generated locally from a UUID.
    let table = format!("astra_pool_shutdown_{}", uuid::Uuid::new_v4().simple());
    if let Some(admin) = admin.as_mut() {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "CREATE TABLE {table} (id INT PRIMARY KEY)"
        )))
        .execute(&mut *admin)
        .await
        .unwrap();
    }
    let database_host = settings.host.clone();
    let database_port = settings.port;
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    settings.host = "127.0.0.1".into();
    settings.port = listener.local_addr().unwrap().port();
    settings.db_pool_max_connections = 1;
    settings.db_pool_min_connections = 0;
    let armed = Arc::new(AtomicBool::new(false));
    let proxy_armed = armed.clone();
    let (entered_tx, entered_rx) = oneshot::channel();
    let (closed_tx, closed_rx) = oneshot::channel();
    let proxy = tokio::spawn(async move {
        let (client, _) = listener.accept().await.unwrap();
        let server = TcpStream::connect((database_host.as_str(), database_port))
            .await
            .unwrap();
        let (mut client_read, mut client_write) = tokio::io::split(client);
        let (mut server_read, mut server_write) = tokio::io::split(server);
        let upload = async move {
            loop {
                let mut header = [0u8; 4];
                client_read.read_exact(&mut header).await.unwrap();
                let length = usize::from(header[0])
                    | (usize::from(header[1]) << 8)
                    | (usize::from(header[2]) << 16);
                let mut body = vec![0u8; length];
                client_read.read_exact(&mut body).await.unwrap();
                let blocked_command = if block_rollback {
                    body.first() == Some(&0x03) && body[1..].eq_ignore_ascii_case(b"ROLLBACK")
                } else {
                    body == [0x0e]
                };
                if header[3] == 0 && blocked_command && proxy_armed.swap(false, Ordering::SeqCst) {
                    entered_tx.send(()).unwrap();
                    // Keep the return health check unanswered. Pool shutdown must
                    // discard the client socket without needing a server reply.
                    let mut byte = [0u8; 1];
                    assert_eq!(client_read.read(&mut byte).await.unwrap(), 0);
                    closed_tx.send(()).unwrap();
                    break;
                }
                server_write.write_all(&header).await.unwrap();
                server_write.write_all(&body).await.unwrap();
            }
        };
        tokio::select! {
            () = upload => {},
            result = tokio::io::copy(&mut server_read, &mut client_write) => { result.unwrap(); },
        }
    });
    let pool = SharedPool::new(&settings).await.unwrap();
    let mut connection = pool.get().acquire().await.unwrap();
    if block_rollback {
        let mut transaction = connection.begin().await.unwrap();
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "INSERT INTO {table} VALUES (1)"
        )))
        .execute(&mut *transaction)
        .await
        .unwrap();
        armed.store(true, Ordering::SeqCst);
        drop(transaction); // Queue ROLLBACK; ping must drain it before COM_PING.
    } else {
        let value: i64 = sqlx::query_scalar("SELECT 1")
            .fetch_one(&mut *connection)
            .await
            .unwrap();
        assert_eq!(value, 1);
        armed.store(true, Ordering::SeqCst);
    }
    drop(connection);
    tokio::time::timeout(Duration::from_secs(5), entered_rx)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((pool.stats().size, pool.stats().num_idle), (1, 0));
    tokio::time::timeout(Duration::from_secs(2), pool.close())
        .await
        .expect("shutdown must not wait for the return ping response");
    tokio::time::timeout(Duration::from_secs(2), closed_rx)
        .await
        .expect("discarded connection must close its socket")
        .unwrap();
    proxy.await.unwrap();
    assert_eq!((pool.stats().size, pool.stats().num_idle), (0, 0));
    if let Some(admin) = admin.as_mut() {
        // Same-key insertion must succeed: the discarded transaction neither
        // committed its row nor retained its key lock after socket destruction.
        tokio::time::timeout(
            Duration::from_secs(5),
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "INSERT INTO {table} VALUES (1)"
            )))
            .execute(&mut *admin),
        )
        .await
        .expect("discarded transaction must release its key lock")
        .unwrap();
        let count: i64 =
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT COUNT(*) FROM {table}")))
                .fetch_one(&mut *admin)
                .await
                .unwrap();
        assert_eq!(count, 1);
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP TABLE {table}")))
            .execute(&mut *admin)
            .await
            .unwrap();
    }
    // Repeated close is valid; quota ownership is asserted in astra-core tests.
    tokio::join!(pool.close(), pool.close());
}
