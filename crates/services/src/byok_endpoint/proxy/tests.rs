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
                    let Ok((tcp, _)) = accepted else { break; };
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

#[tokio::test]
async fn exact_inference_transport_preserves_pinned_proxy_and_redirect_contract() {
    use astra_inference_adapter::transport::{ProviderTransport, provider_headers};
    use astra_inference_adapter::{ExactProviderRequest, ProviderProtocol};
    for scheme in ["http", "https", "socks5", "socks5h"] {
        for (status, response_bytes) in [
            (200, &b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK"[..]),
            (302, &b"HTTP/1.1 302 Found\r\nLocation: https://private.test/\r\nContent-Length: 0\r\n\r\n"[..]),
        ] {
            let tls = test_tls();
            let (origin, observed, origin_task) = origin(&tls, response_bytes).await;
            let (proxy, mut forwarded, proxy_task) = mock_proxy(scheme, &tls, origin, false).await;
            let guard = client_with(&endpoint(), &[target()], proxy, test_builder(&tls), |builder| {
                ProviderTransport::build(builder).map_err(|error| error.to_string())
            }).await.unwrap();
            let body = ExactProviderRequest::compile(
                &serde_json::json!({"model":"o3", "messages":[], "max_completion_tokens":4}),
                ProviderProtocol::OpenAiCompatible, 1024,
            ).unwrap();
            let headers = provider_headers(ProviderProtocol::OpenAiCompatible, "fixture-key", []).unwrap();
            let request = guard.prepare("https://byok.test/v1", headers, &body, None).unwrap();
            let response = guard.send_once(request).await.unwrap();
            assert_eq!(response.status(), status, "{scheme}");
            assert_eq!(response.text().await.unwrap(), if status == 200 { "OK" } else { "" });
            let (sni, headers) = observed.await.unwrap();
            assert_eq!(sni, "byok.test");
            assert!(headers.starts_with("POST /v1 "));
            assert!(headers.contains("fixture-key"));
            assert!(!headers.to_lowercase().contains("proxy-authorization"));
            forwarded.recv().await.unwrap();
            assert!(forwarded.try_recv().is_err(), "no retry or redirect: {scheme}");
            drop(guard);
            origin_task.await.unwrap();
            proxy_task.abort();
        }
    }
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
        "proxy:8080",
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
