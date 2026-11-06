use super::*;
use tokio::sync::{mpsc, oneshot};

struct TestTls {
    server: Arc<rustls::ServerConfig>,
    client: Arc<rustls::ClientConfig>,
    certificate: reqwest::Certificate,
}
fn test_tls() -> TestTls {
    let rcgen::CertifiedKey { cert, key_pair } = rcgen::generate_simple_self_signed(vec![
        "byok.test".into(),
        "localhost".into(),
        "127.0.0.1".into(),
    ])
    .unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let key = rustls::pki_types::PrivatePkcs8KeyDer::from(key_pair.serialize_der()).into();
    let server = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert.der().clone()], key)
        .unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert.der().clone()).unwrap();
    let client = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    TestTls {
        server: Arc::new(server),
        client: Arc::new(client),
        certificate: reqwest::Certificate::from_der(cert.der()).unwrap(),
    }
}

async fn origin(
    tls: &TestTls,
    reply: &'static [u8],
) -> (
    SocketAddr,
    oneshot::Receiver<(String, String)>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let tls = tls.server.clone();
    let (tx, rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let Ok(mut stream) = tokio_rustls::TlsAcceptor::from(tls).accept(tcp).await else {
            return;
        };
        let sni = stream
            .get_ref()
            .1
            .server_name()
            .unwrap_or_default()
            .to_owned();
        let Ok(head) = read_head(&mut stream).await else {
            return;
        };
        let _ = tx.send((sni, head));
        let _ = stream.write_all(reply).await;
        let _ = stream.shutdown().await;
    });
    (address, rx, task)
}

// A controllable test proxy maps an explicitly asserted public target to a local
// TLS origin. Production code still performs its real public-address checks.
async fn mock_proxy(
    scheme: &str,
    tls: &TestTls,
    origin: SocketAddr,
    refuse: bool,
) -> (
    EgressProxy,
    mpsc::Receiver<String>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut proxy = EgressProxy::parse(Some(&format!(
        "{scheme}://test-user:test-password@{address}"
    )))
    .unwrap()
    .unwrap();
    proxy.tls = tls.client.clone();
    let server_tls = tls.server.clone();
    let scheme = scheme.to_owned();
    let (tx, rx) = mpsc::channel(8);
    let task = tokio::spawn(async move {
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (tcp, _) = match accepted {
                        Ok(connection) => connection,
                        #[cfg(unix)]
                        Err(error) if matches!(error.raw_os_error(), Some(libc::EMFILE) | Some(libc::ENFILE)) => {
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            continue;
                        }
                        Err(error) => panic!("test proxy failed to accept a connection: {error}"),
                    };
                    let (tls, tx, scheme) = (server_tls.clone(), tx.clone(), scheme.clone());
                    connections.spawn(async move {
                        let mut stream: Stream = if scheme == "https" {
                            Box::new(tokio_rustls::TlsAcceptor::from(tls).accept(tcp).await.unwrap())
                        } else { Box::new(tcp) };
                        if scheme.starts_with("socks") {
                            let mut greeting = [0; 3];
                            stream.read_exact(&mut greeting).await.unwrap();
                            assert_eq!(greeting, [5, 1, 2]);
                            stream.write_all(&[5, 2]).await.unwrap();
                            assert_eq!(stream.read_u8().await.unwrap(), 1);
                            let size = stream.read_u8().await.unwrap();
                            let mut user = vec![0; usize::from(size)];
                            stream.read_exact(&mut user).await.unwrap();
                            assert_eq!(user, b"test-user");
                            let size = stream.read_u8().await.unwrap();
                            let mut password = vec![0; usize::from(size)];
                            stream.read_exact(&mut password).await.unwrap();
                            assert_eq!(password, b"test-password");
                            stream.write_all(&[1, 0]).await.unwrap();
                            let mut connect = [0; 10];
                            stream.read_exact(&mut connect).await.unwrap();
                            assert_eq!(connect, [5, 1, 0, 1, 8, 8, 8, 8, 1, 187], "SOCKS must receive a pinned IP, not a hostname");
                            tx.send("8.8.8.8:443".into()).await.unwrap();
                            stream.write_all(&[5, if refuse { 2 } else { 0 }, 0, 1, 0, 0, 0, 0, 0, 0]).await.unwrap();
                        } else {
                            let head = read_head(&mut stream).await.unwrap();
                            assert!(head.starts_with("CONNECT 8.8.8.8:443 HTTP/1.1\r\n"), "proxy destination must be pinned");
                            assert!(head.contains(&STANDARD.encode("test-user:test-password")));
                            assert!(!head.contains("provider-key"));
                            tx.send(head).await.unwrap();
                            stream.write_all(if refuse { b"HTTP/1.1 407 Authentication Required\r\n\r\n" } else { b"HTTP/1.1 200 Connection Established\r\n\r\n" }).await.unwrap();
                        }
                        if refuse { return; }
                        let mut upstream = TcpStream::connect(origin).await.unwrap();
                        let _ = tokio::io::copy_bidirectional(&mut stream, &mut upstream).await;
                    });
                }
                result = connections.join_next(), if !connections.is_empty() => { result.unwrap().unwrap(); }
            }
        }
    });
    (proxy, rx, task)
}

fn test_builder(tls: &TestTls) -> reqwest::ClientBuilder {
    super::super::transport_builder()
        .add_root_certificate(tls.certificate.clone())
        .timeout(Duration::from_secs(3))
}
fn endpoint() -> reqwest::Url {
    super::super::parse_endpoint("https://byok.test/v1").unwrap()
}
fn target() -> SocketAddr {
    "8.8.8.8:443".parse().unwrap()
}

#[cfg(unix)]
#[tokio::test]
async fn proxy_registrations_share_one_listener_under_descriptor_pressure() {
    const CHILD: &str = "ASTRA_TEST_PROXY_SCALE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "byok_endpoint::proxy::tests::proxy_registrations_share_one_listener_under_descriptor_pressure"])
            .env(CHILD, "1")
            .status()
            .unwrap();
        assert!(status.success());
        return;
    }
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // The limit belongs only to this subprocess, never to the test runner.
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) },
        0
    );
    limit.rlim_cur = limit.rlim_cur.min(256);
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
    let proxy = EgressProxy::parse(Some("http://127.0.0.1:1"))
        .unwrap()
        .unwrap();
    // This test measures listener descriptors, independently of native
    // trust-store loading, which the other TLS tests exercise separately.
    let tls = test_tls();
    let mut clients = Vec::new();
    for _ in 0..1000 {
        clients.push(
            client(
                &endpoint(),
                &[target()],
                proxy.clone(),
                super::super::transport_builder().use_preconfigured_tls((*tls.client).clone()),
            )
            .await
            .unwrap(),
        );
    }
    let gateway = clients[0]._tunnel.as_ref().unwrap().gateway.clone();
    assert_eq!(gateway.routes.lock().unwrap().len(), 1000);
    assert!(
        clients
            .iter()
            .all(|client| Arc::ptr_eq(&gateway, &client._tunnel.as_ref().unwrap().gateway))
    );
    drop(clients);
    assert!(gateway.routes.lock().unwrap().is_empty());
}

#[tokio::test]
async fn proxy_capability_cannot_be_used_for_another_origin() {
    let proxy = EgressProxy::parse(Some("http://127.0.0.1:1"))
        .unwrap()
        .unwrap();
    let first = client(
        &endpoint(),
        &[target()],
        proxy.clone(),
        super::super::transport_builder(),
    )
    .await
    .unwrap();
    let other_url = super::super::parse_endpoint("https://other.test/v1").unwrap();
    let other = client(
        &other_url,
        &[target()],
        proxy,
        super::super::transport_builder(),
    )
    .await
    .unwrap();
    let first_route = first._tunnel.as_ref().unwrap();
    let other_route = other._tunnel.as_ref().unwrap();
    assert!(Arc::ptr_eq(&first_route.gateway, &other_route.gateway));
    let mut socket = TcpStream::connect(first_route.gateway.address)
        .await
        .unwrap();
    socket
        .write_all(
            format!(
                "CONNECT byok.test:443 HTTP/1.1\r\nProxy-Authorization: {}\r\n\r\n",
                other_route.authorization
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    assert!(
        read_head(&mut socket)
            .await
            .unwrap()
            .starts_with("HTTP/1.1 502")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn proxy_listener_recovers_from_descriptor_exhaustion_without_dropping_streams() {
    const CHILD: &str = "ASTRA_TEST_PROXY_OVERLOAD_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "byok_endpoint::proxy::tests::proxy_listener_recovers_from_descriptor_exhaustion_without_dropping_streams"])
            .env(CHILD, "1").status().unwrap();
        assert!(status.success());
        return;
    }
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) },
        0
    );
    limit.rlim_cur = limit.rlim_cur.min(256);
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);

    let origin = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin_address = origin.local_addr().unwrap();
    let origin_task = tokio::spawn(async move {
        loop {
            match origin.accept().await {
                Ok((mut stream, _)) => {
                    tokio::spawn(async move {
                        let mut bytes = [0; 32];
                        while let Ok(count) = stream.read(&mut bytes).await {
                            if count == 0 || stream.write_all(&bytes[..count]).await.is_err() {
                                break;
                            }
                        }
                    });
                }
                Err(error)
                    if matches!(
                        error.raw_os_error(),
                        Some(libc::EMFILE) | Some(libc::ENFILE)
                    ) =>
                {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(error) => panic!("test origin failed to accept a connection: {error}"),
            }
        }
    });
    let tls = test_tls();
    let (proxy, mut forwarded, proxy_task) = mock_proxy("http", &tls, origin_address, false).await;
    let client = client(&endpoint(), &[target()], proxy, test_builder(&tls))
        .await
        .unwrap();
    let route = client._tunnel.as_ref().unwrap();
    let address = route.gateway.address;
    let request = format!(
        "CONNECT byok.test:443 HTTP/1.1\r\nProxy-Authorization: {}\r\n\r\n",
        route.authorization
    );
    let mut established = TcpStream::connect(address).await.unwrap();
    established.write_all(request.as_bytes()).await.unwrap();
    assert!(
        read_head(&mut established)
            .await
            .unwrap()
            .starts_with("HTTP/1.1 200")
    );
    forwarded.recv().await.unwrap();

    let mut slow = Vec::new();
    for _ in 0..MAX_PENDING_HANDSHAKES {
        slow.push(TcpStream::connect(address).await.unwrap());
    }
    let mut queued = TcpStream::connect(address).await.unwrap();
    queued.write_all(request.as_bytes()).await.unwrap();
    established.write_all(b"before overload").await.unwrap();
    let mut reply = [0; 15];
    tokio::time::timeout(Duration::from_secs(2), established.read_exact(&mut reply))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&reply, b"before overload");
    drop(slow);
    let head = tokio::time::timeout(Duration::from_secs(3), read_head(&mut queued))
        .await
        .unwrap()
        .unwrap();
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    forwarded.recv().await.unwrap();
    drop(queued);

    let reserve = std::fs::File::open("/dev/null").unwrap();
    let mut files = Vec::new();
    while let Ok(file) = std::fs::File::open("/dev/null") {
        files.push(file);
    }
    drop(reserve);
    let waiting = std::net::TcpStream::connect(address).unwrap();
    waiting.set_nonblocking(true).unwrap();
    let waiting = TcpStream::from_std(waiting).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    established.write_all(b"still alive").await.unwrap();
    let mut reply = [0; 11];
    tokio::time::timeout(Duration::from_secs(2), established.read_exact(&mut reply))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&reply, b"still alive");
    assert!(!route.gateway.task.is_finished());

    drop(files);
    // The OS may discard a connection whose accept hit EMFILE. Recovery
    // means a fresh request can connect; an established stream stayed alive.
    drop(waiting);
    let mut after = TcpStream::connect(address).await.unwrap();
    after.write_all(request.as_bytes()).await.unwrap();
    let head = tokio::time::timeout(Duration::from_secs(3), read_head(&mut after))
        .await
        .unwrap()
        .unwrap();
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    forwarded.recv().await.unwrap();
    origin_task.abort();
    proxy_task.abort();
}

#[tokio::test]
async fn proxy_stream_has_no_hidden_total_deadline() {
    let tls = test_tls();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (frames, mut incoming_frames) = mpsc::channel::<String>(1);
    let server_tls = tls.server.clone();
    let origin_task = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut stream = tokio_rustls::TlsAcceptor::from(server_tls)
            .accept(socket)
            .await
            .unwrap();
        read_head(&mut stream).await.unwrap();
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").await.unwrap();
        while let Some(frame) = incoming_frames.recv().await {
            stream
                .write_all(format!("{:x}\r\n{frame}\r\n", frame.len()).as_bytes())
                .await
                .unwrap();
            stream.flush().await.unwrap();
        }
        stream.write_all(b"0\r\n\r\n").await.unwrap();
        stream.shutdown().await.unwrap();
    });
    let (proxy, _forwarded, proxy_task) = mock_proxy("http", &tls, address, false).await;
    let client = client(
        &endpoint(),
        &[target()],
        proxy,
        super::super::transport_builder().add_root_certificate(tls.certificate.clone()),
    )
    .await
    .unwrap();
    frames.send("data: start\n\n".into()).await.unwrap();
    let mut response = client.get("https://byok.test/v1").send().await.unwrap();
    assert_eq!(response.chunk().await.unwrap().unwrap(), "data: start\n\n");
    let clock_guard = tokio::spawn(async {
        loop {
            tokio::task::yield_now().await;
        }
    });
    tokio::time::pause();
    for index in 1..=40 {
        tokio::time::advance(Duration::from_secs(10)).await;
        let frame = format!("data: part-{index}\n\n");
        frames.send(frame.clone()).await.unwrap();
        assert_eq!(
            response
                .chunk()
                .await
                .expect("active proxy stream")
                .unwrap(),
            frame
        );
    }
    drop(frames);
    assert!(response.chunk().await.unwrap().is_none());
    clock_guard.abort();
    tokio::time::resume();
    drop(client);
    origin_task.await.unwrap();
    proxy_task.abort();
}

#[tokio::test]
async fn http_https_and_socks_proxies_preserve_tls_host_auth_and_streaming() {
    for scheme in ["http", "https", "socks5", "socks5h"] {
        let tls = test_tls();
        let (origin, observed, origin_task) = origin(&tls, b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n9\r\ndata: a\n\n\r\n9\r\ndata: b\n\n\r\n0\r\n\r\n").await;
        let (proxy, mut forwarded, proxy_task) = mock_proxy(scheme, &tls, origin, false).await;
        let client = client(&endpoint(), &[target()], proxy, test_builder(&tls))
            .await
            .unwrap();
        let response = client
            .get("https://byok.test/v1")
            .bearer_auth("provider-key")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{scheme}");
        assert_eq!(response.text().await.unwrap(), "data: a\n\ndata: b\n\n");
        let (sni, headers) = observed.await.unwrap();
        assert_eq!(sni, "byok.test");
        assert!(headers.to_lowercase().contains("host: byok.test"));
        assert!(headers.contains("provider-key"));
        assert!(!headers.to_lowercase().contains("proxy-authorization"));
        assert!(!headers.contains("test-password"));
        assert!(forwarded.recv().await.is_some());
        drop(client);
        origin_task.await.unwrap();
        proxy_task.abort();
    }
}

#[tokio::test]
async fn proxy_cannot_expand_destination_or_follow_redirects() {
    let tls = test_tls();
    let (origin, observed, origin_task) = origin(
        &tls,
        b"HTTP/1.1 302 Found\r\nLocation: https://private.test/\r\nContent-Length: 0\r\n\r\n",
    )
    .await;
    let (proxy, mut forwarded, proxy_task) = mock_proxy("http", &tls, origin, false).await;
    let client = client(&endpoint(), &[target()], proxy, test_builder(&tls))
        .await
        .unwrap();
    assert!(client.get("https://unadmitted.test/").send().await.is_err());
    assert!(
        forwarded.try_recv().is_err(),
        "wrong destinations must not reach the proxy"
    );
    let response = client.get("https://byok.test/v1").send().await.unwrap();
    assert_eq!(response.status(), 302);
    observed.await.unwrap();
    forwarded.recv().await.unwrap();
    assert!(forwarded.try_recv().is_err());
    drop(client);
    origin_task.await.unwrap();
    proxy_task.abort();
}

#[tokio::test]
async fn proxy_failure_does_not_fall_back_to_direct_or_leak_credentials() {
    for scheme in ["http", "socks5"] {
        let tls = test_tls();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (proxy, mut forwarded, task) =
            mock_proxy(scheme, &tls, listener.local_addr().unwrap(), true).await;
        let client = client(&endpoint(), &[target()], proxy, test_builder(&tls))
            .await
            .unwrap();
        let error = client
            .get("https://byok.test/v1")
            .bearer_auth("provider-key")
            .send()
            .await
            .unwrap_err()
            .to_string();
        assert!(!error.contains("provider-key"));
        assert!(!error.contains("test-password"));
        forwarded.recv().await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(30), listener.accept())
                .await
                .is_err()
        );
        drop(client);
        task.abort();
    }
}

#[tokio::test]
async fn proxy_never_disables_origin_certificate_validation() {
    let tls = test_tls();
    let (origin, observed, origin_task) =
        origin(&tls, b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").await;
    let (proxy, mut forwarded, proxy_task) = mock_proxy("http", &tls, origin, false).await;
    // Do not add the fixture's certificate to the origin trust store.
    let client = client(
        &endpoint(),
        &[target()],
        proxy,
        super::super::transport_builder().timeout(Duration::from_secs(3)),
    )
    .await
    .unwrap();
    assert!(
        client
            .get("https://byok.test/v1")
            .bearer_auth("provider-key")
            .send()
            .await
            .is_err()
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(3), observed)
            .await
            .unwrap()
            .is_err(),
        "untrusted TLS must stop before HTTP/credentials"
    );
    forwarded.recv().await.unwrap();
    drop(client);
    origin_task.await.unwrap();
    proxy_task.abort();
}

#[tokio::test]
async fn https_proxy_certificate_is_checked_before_proxy_credentials_are_sent() {
    let tls = test_tls();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = EgressProxy::parse(Some(&format!(
        "https://user:proxy-secret@{}",
        listener.local_addr().unwrap()
    )))
    .unwrap()
    .unwrap();
    let server_tls = tls.server.clone();
    let proxy_task = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        assert!(
            tokio_rustls::TlsAcceptor::from(server_tls)
                .accept(tcp)
                .await
                .is_err()
        );
    });
    // Trust the origin fixture but not the HTTPS proxy certificate.
    let guard = client(&endpoint(), &[target()], proxy, test_builder(&tls))
        .await
        .unwrap();
    let error = guard
        .get("https://byok.test/v1")
        .bearer_auth("provider-key")
        .send()
        .await
        .unwrap_err()
        .to_string();
    assert!(!error.contains("proxy-secret"));
    assert!(!error.contains("provider-key"));
    tokio::time::timeout(Duration::from_secs(3), proxy_task)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn private_addresses_are_rejected_before_a_proxy_tunnel_is_created() {
    let tls = test_tls();
    let proxy = EgressProxy::parse(Some("http://127.0.0.1:1"))
        .unwrap()
        .unwrap();
    for blocked in [
        "127.0.0.1:443",
        "169.254.169.254:443",
        "198.18.4.197:443",
        "[::ffff:127.0.0.1]:443",
    ] {
        assert!(
            client(
                &endpoint(),
                &[target(), blocked.parse().unwrap()],
                proxy.clone(),
                test_builder(&tls)
            )
            .await
            .is_err()
        );
    }
}

#[test]
fn proxy_configuration_errors_do_not_echo_secrets_or_allow_ambiguous_urls() {
    for raw in [
        "ftp://user:secret@proxy",
        "http://user:secret@proxy/path",
        "http://proxy?key=secret",
        "http://user:secret@proxy:0",
        "http://u%0d:p@proxy",
    ] {
        let error = EgressProxy::parse(Some(raw)).err().unwrap();
        assert!(!error.contains("secret"));
    }
    assert!(EgressProxy::parse(None).unwrap().is_none());
    assert!(EgressProxy::parse(Some("")).unwrap().is_none());
    for raw in [
        "proxy:8080",
        "http://localhost:8080",
        "https://proxy.example",
        "socks5://[::1]:1080",
        "socks5h://proxy.example:1080",
    ] {
        assert!(EgressProxy::parse(Some(raw)).unwrap().is_some());
    }
}

#[tokio::test]
async fn dropping_client_cancels_a_stalled_proxy_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = EgressProxy::parse(Some(&format!("http://{}", listener.local_addr().unwrap())))
        .unwrap()
        .unwrap();
    let guard = client(
        &endpoint(),
        &[target()],
        proxy,
        super::super::transport_builder(),
    )
    .await
    .unwrap();
    let requester = guard.client.clone();
    let request = tokio::spawn(async move { requester.get("https://byok.test/v1").send().await });
    let (mut stream, _) = tokio::time::timeout(Duration::from_secs(3), listener.accept())
        .await
        .unwrap()
        .unwrap();
    read_head(&mut stream).await.unwrap();
    drop(guard);
    let mut byte = [0];
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(3), stream.read(&mut byte))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(3), request)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
}

#[tokio::test]
async fn stalled_proxy_obeys_request_timeout() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = EgressProxy::parse(Some(&format!("http://{}", listener.local_addr().unwrap())))
        .unwrap()
        .unwrap();
    let guard = client(
        &endpoint(),
        &[target()],
        proxy,
        super::super::transport_builder().timeout(Duration::from_millis(100)),
    )
    .await
    .unwrap();
    let error = tokio::time::timeout(
        Duration::from_secs(3),
        guard.get("https://byok.test/v1").send(),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(error.is_timeout());
}
