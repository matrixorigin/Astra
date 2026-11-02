//! A real MySQL response barrier shared by database cancellation tests.
use sqlx::mysql::{MySqlConnectOptions, MySqlSslMode};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio::task::{JoinHandle, JoinSet};

#[derive(Clone, Copy)]
pub enum Boundary {
    Begin,
    Commit,
    Write(&'static [u8]),
    Rows(&'static [u8]),
    RowTerminator(&'static [u8]),
}

// Only the first matching exchange is gated. Requests reach the real database
// unchanged; its response is retained until the test cancels the caller. This
// exercises cancellation after server execution, not a sleep before any SQL.
pub struct ResponseGate {
    armed: AtomicBool,
    pending: AtomicBool,
    pub received: Notify,
    pub release: Notify,
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
            pending: AtomicBool::new(false),
            received: Notify::new(),
            release: Notify::new(),
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
                            let (mut client_read, mut client_write) = client.into_split();
                            let (mut server_read, mut server_write) = server.into_split();
                            let request_gate = gate.clone();
                            let requests = async move {
                                let mut matched_prepared = false;
                                loop {
                                    let bytes = packet(&mut client_read).await?;
                                    let payload = &bytes[4..];
                                    // COM_STMT_PREPARE for a matching query is
                                    // immediately followed by its COM_STMT_EXECUTE on
                                    // this serial checkout. Gate execution, not prepare.
                                    if payload.first() == Some(&0x16) {
                                        matched_prepared = match boundary {
                                            Boundary::Write(prefix) | Boundary::Rows(prefix) | Boundary::RowTerminator(prefix) => payload[1..].starts_with(prefix),
                                            Boundary::Begin | Boundary::Commit => false,
                                        };
                                    }
                                    let matches = match boundary {
                                        Boundary::Begin => payload == b"\x03BEGIN",
                                        Boundary::Commit => payload == b"\x03COMMIT",
                                        Boundary::Write(_) | Boundary::Rows(_) | Boundary::RowTerminator(_) => payload.first() == Some(&0x17) && matched_prepared,
                                    };
                                    if matches && request_gate.armed.swap(false, Ordering::SeqCst) {
                                        request_gate.pending.store(true, Ordering::SeqCst);
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
                                    let is_terminator = matches!(boundary, Boundary::RowTerminator(_));
                                    let should_hold = gate.pending.load(Ordering::SeqCst)
                                        && (!is_terminator || (row_seen && matches!(bytes[4], 0xfe | 0x00)));
                                    if should_hold && gate.pending.swap(false, Ordering::SeqCst) {
                                        if matches!(boundary, Boundary::Begin | Boundary::Commit | Boundary::Write(_)) {
                                            assert_eq!(bytes[4], 0, "gated SQL must succeed at the server");
                                        }
                                        gate.received.notify_one();
                                        gate.release.notified().await;
                                    } else if is_terminator && gate.pending.load(Ordering::SeqCst) && bytes[4] == 0x00 {
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
    let mut direct = settings.clone();
    direct.db_pool_max_connections = 1;
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
            .ssl_mode(MySqlSslMode::Disabled),
    );
    initial.close().await.unwrap();
    pool
}
