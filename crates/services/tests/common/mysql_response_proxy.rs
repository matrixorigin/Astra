//! A real MySQL response barrier shared by database cancellation tests.
use sqlx::mysql::{MySqlConnectOptions, MySqlSslMode};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio::task::{JoinHandle, JoinSet};

#[derive(Clone, Copy)]
pub enum Boundary {
    Begin,
    Commit,
    Ping,
    Write(&'static [u8]),
    Rows(&'static [u8]),
    RowsMatching(&'static [u8], &'static [u8]),
    RowTerminator(&'static [u8]),
    RowsAfter(&'static [u8], std::time::Duration),
}

// By default only the first matching exchange is gated. Requests reach the real database
// unchanged; its response is retained until the test cancels the caller. This
// exercises cancellation after server execution, not a sleep before any SQL.
pub struct ResponseGate {
    pub armed: AtomicBool,
    pub continuous: AtomicBool,
    pub received: Notify,
    pub release: Notify,
    pub disconnect_on_release: AtomicBool,
    pub disconnects: AtomicUsize,
    pub matched: AtomicUsize,
    pub disconnected: Notify,
}

pub struct ResponseProxy {
    pub port: u16,
    pub gate: Arc<ResponseGate>,
    task: JoinHandle<()>,
}

impl Drop for ResponseProxy {
    fn drop(&mut self) {
        // Dropping the accept task drops its JoinSet and closes every relay,
        // including on assertion failure. No fault-injection socket survives.
        self.task.abort();
    }
}

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

impl ResponseProxy {
    pub async fn start(host: String, port: u16, boundary: Boundary) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_port = listener.local_addr().unwrap().port();
        let gate = Arc::new(ResponseGate {
            armed: AtomicBool::new(true),
            continuous: AtomicBool::new(false),
            received: Notify::new(),
            release: Notify::new(),
            disconnect_on_release: AtomicBool::new(false),
            disconnects: AtomicUsize::new(0),
            matched: AtomicUsize::new(0),
            disconnected: Notify::new(),
        });
        let relay_gate = gate.clone();
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (client, _) = accepted.unwrap();
                        let host = host.clone();
                        let gate = relay_gate.clone();
                        connections.spawn(async move {
                            let server = TcpStream::connect((host.as_str(), port)).await.unwrap();
                            assert!(server.peer_addr().unwrap().ip().is_loopback(), "plaintext response proxy requires a loopback database");
                            let pending = Arc::new(AtomicBool::new(false));
                            let request_pending = pending.clone();
                            let (mut client_read, mut client_write) = client.into_split();
                            let (mut server_read, mut server_write) = server.into_split();
                            let request_gate = gate.clone();
                            let requests = async move {
                                let mut matched_prepared = false;
                                loop {
                                    let bytes = packet(&mut client_read).await?;
                                    let payload = &bytes[4..];
                                    // The fixture caches only the last prepared statement.
                                    // Gate its execution, including cache hits.
                                    if payload.first() == Some(&0x16) {
                                        matched_prepared = match boundary {
                                            Boundary::Write(prefix) | Boundary::Rows(prefix) | Boundary::RowsMatching(prefix, _) | Boundary::RowTerminator(prefix) | Boundary::RowsAfter(prefix, _) => payload[1..].starts_with(prefix),
                                            Boundary::Begin | Boundary::Commit | Boundary::Ping => false,
                                        };
                                    }
                                    let matches = match boundary {
                                        Boundary::Begin => payload == b"\x03BEGIN",
                                        Boundary::Commit => payload == b"\x03COMMIT",
                                        Boundary::Ping => payload == b"\x0e",
                                        Boundary::RowsMatching(_, argument) => {
                                            payload.first() == Some(&0x17) && matched_prepared
                                                && payload.windows(argument.len()).any(|value| value == argument)
                                        }
                                        Boundary::Write(_) | Boundary::Rows(_) | Boundary::RowTerminator(_) | Boundary::RowsAfter(_, _) => payload.first() == Some(&0x17) && matched_prepared,
                                    };
                                    if matches && (request_gate.continuous.load(Ordering::SeqCst)
                                        || request_gate.armed.swap(false, Ordering::SeqCst)) {
                                        request_gate.matched.fetch_add(1, Ordering::SeqCst);
                                        request_pending.store(true, Ordering::SeqCst);
                                    }
                                    server_write.write_all(&bytes).await?;
                                }
                                #[allow(unreachable_code)]
                                Ok::<(), std::io::Error>(())
                            };
                            let responses = async move {
                                let mut row_seen = false;
                                loop {
                                    let bytes = packet(&mut server_read).await?;
                                    if let Boundary::RowsAfter(_, delay) = boundary
                                        && pending.load(Ordering::SeqCst) {
                                        if bytes[4] == 0x00 {
                                            if !row_seen {
                                                gate.received.notify_one();
                                                row_seen = true;
                                            }
                                            tokio::time::sleep(delay).await;
                                        } else if bytes[4] == 0xfe && row_seen {
                                            row_seen = false;
                                            pending.store(false, Ordering::SeqCst);
                                        }
                                        client_write.write_all(&bytes).await?;
                                        continue;
                                    }
                                    let is_terminator = matches!(boundary, Boundary::RowTerminator(_));
                                    let should_hold = pending.load(Ordering::SeqCst)
                                        // A second binary row also starts with 0x00.
                                        && (!is_terminator || (row_seen && bytes[4] == 0xfe));
                                    if should_hold && pending.swap(false, Ordering::SeqCst) {
                                        assert_ne!(bytes[4], 0xff, "gated SQL must succeed at the server");
                                        if matches!(boundary, Boundary::Begin | Boundary::Commit | Boundary::Write(_)) {
                                            assert_eq!(bytes[4], 0, "gated SQL must succeed at the server");
                                        }
                                        gate.received.notify_one();
                                        if !gate.disconnect_on_release.load(Ordering::SeqCst) {
                                            gate.release.notified().await;
                                        }
                                        if gate.disconnect_on_release.load(Ordering::SeqCst) {
                                            gate.disconnects.fetch_add(1, Ordering::SeqCst);
                                            gate.disconnected.notify_one();
                                            return Ok(());
                                        }
                                    } else if is_terminator && pending.load(Ordering::SeqCst) && bytes[4] == 0x00 {
                                        row_seen = true;
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
                    Some(result) = connections.join_next() => { result.unwrap(); }
                }
            }
        });
        Self {
            port: proxy_port,
            gate,
            task,
        }
    }
}

// Keep the protocol relay plaintext, even when the database advertises TLS.
pub async fn proxy_pool(
    settings: &astra_core::MatrixOneSettings,
    port: u16,
) -> astra_core::SharedPool {
    proxy_pool_with_limit(settings, port, 1).await
}

pub async fn proxy_pool_with_limit(
    settings: &astra_core::MatrixOneSettings,
    port: u16,
    max_connections: u32,
) -> astra_core::SharedPool {
    let mut direct = settings.clone();
    direct.db_pool_max_connections = max_connections;
    direct.db_pool_min_connections = 0;
    let pool = astra_core::SharedPool::new(&direct).await.unwrap();
    let initial = pool.get().acquire().await.unwrap();
    pool.get().set_connect_options(
        MySqlConnectOptions::new()
            .host("127.0.0.1")
            .port(port)
            .username(&settings.user)
            .password(&settings.password)
            .database(&settings.database)
            // Keep only the latest prepared statement for prefix matching. With
            // zero entries SQLx sends COM_STMT_CLOSE before reading the result,
            // which can desynchronize MatrixOne responses.
            .statement_cache_capacity(1)
            .ssl_mode(MySqlSslMode::Disabled),
    );
    initial.close().await.unwrap();
    pool
}
