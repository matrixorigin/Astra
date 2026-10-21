//! Exercise the explicit connector used by Edge, MCP and native delivery.
use std::sync::Arc;

use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
use tokio_rustls::{TlsAcceptor, rustls};
use tokio_tungstenite::{
    Connector, client_async_tls_with_config, tungstenite::client::IntoClientRequest,
};

struct TestTls {
    server: Arc<rustls::ServerConfig>,
    certificate: rustls::pki_types::CertificateDer<'static>,
    ca_pem: Vec<u8>,
}

fn test_tls(hostname: &str) -> TestTls {
    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca_key = KeyPair::generate().unwrap();
    let ca = params.self_signed(&ca_key).unwrap();
    let key = KeyPair::generate().unwrap();
    let leaf = CertificateParams::new(vec![hostname.into()])
        .unwrap()
        .signed_by(&key, &ca, &ca_key)
        .unwrap();
    let server = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![leaf.der().clone()],
        rustls::pki_types::PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
    )
    .unwrap();
    TestTls {
        server: Arc::new(server),
        certificate: ca.der().clone(),
        ca_pem: ca.pem().into_bytes(),
    }
}

fn assert_certificate_rejection(error: &(dyn std::error::Error + 'static), wrong_host: bool) {
    let mut cause = Some(error);
    while let Some(error) = cause {
        if let Some(rustls::Error::InvalidCertificate(reason)) =
            error.downcast_ref::<rustls::Error>()
        {
            if wrong_host {
                assert!(
                    matches!(
                        reason,
                        rustls::CertificateError::NotValidForName
                            | rustls::CertificateError::NotValidForNameContext { .. }
                    ),
                    "{reason:?}"
                );
            } else {
                assert!(
                    matches!(reason, rustls::CertificateError::UnknownIssuer),
                    "{reason:?}"
                );
            }
            return;
        }
        cause = if let Some(error) = error.downcast_ref::<std::io::Error>() {
            error
                .get_ref()
                .map(|inner| inner as &(dyn std::error::Error + 'static))
        } else {
            error.source()
        };
    }
    panic!("expected a certificate rejection: {error:?}");
}

#[tokio::test]
async fn websocket_tls_preserves_chain_and_hostname_verification() {
    let tls = test_tls("websocket.invalid");
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = rustls::RootCertStore::empty();
    roots.add(tls.certificate.clone()).unwrap();
    // A trusted local CA is the positive control. It uses the same standard
    // verifier and protocol defaults, with no certificate or hostname bypass.
    let trusted = Arc::new(
        rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    );
    let public = astra_core::net::public_root_tls_config();
    for (host, config, expected_error) in [
        ("websocket.invalid", trusted.clone(), None),
        ("wrong-host.invalid", trusted, Some(true)),
        ("websocket.invalid", public, Some(false)),
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let acceptor = TlsAcceptor::from(tls.server.clone());
        let peer = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let Ok(socket) = acceptor.accept(socket).await else {
                return false;
            };
            // Authorization cannot reach the HTTP upgrade before TLS succeeds.
            tokio_tungstenite::accept_async(socket).await.is_ok()
        });
        let mut request = format!("wss://{host}/").into_client_request().unwrap();
        request.headers_mut().insert(
            "authorization",
            "Bearer public-test-fixture".parse().unwrap(),
        );
        let socket = tokio::net::TcpStream::connect(address).await.unwrap();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            client_async_tls_with_config(request, socket, None, Some(Connector::Rustls(config))),
        )
        .await
        .unwrap();
        match expected_error {
            None => assert!(result.is_ok(), "trusted TLS: {result:?}"),
            Some(wrong_host) => assert_certificate_rejection(&result.unwrap_err(), wrong_host),
        }
        assert_eq!(peer.await.unwrap(), expected_error.is_none());
    }
}

/// A minimal protocol peer stops at authentication: the test exercises SQLx's
/// real TLS negotiation without a database, SQL execution or plaintext fallback.
#[tokio::test]
async fn mysql_verify_identity_checks_certificates_before_authentication() {
    use sqlx::{
        ConnectOptions,
        mysql::{MySqlConnectOptions, MySqlSslMode},
    };
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

    async fn read_packet(socket: &mut (impl AsyncRead + Unpin)) -> Vec<u8> {
        let mut header = [0u8; 4];
        socket.read_exact(&mut header).await.unwrap();
        let length =
            usize::from(header[0]) | (usize::from(header[1]) << 8) | (usize::from(header[2]) << 16);
        assert!(length <= 4096, "bounded handshake packet");
        let mut packet = vec![0; length];
        socket.read_exact(&mut packet).await.unwrap();
        packet
    }
    for (hostname, trust_ca, authenticates) in [
        ("127.0.0.1", true, true),
        ("wrong-host.invalid", true, false),
        ("127.0.0.1", false, false),
    ] {
        let tls = test_tls(hostname);
        let server = tls.server;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let peer = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            // Protocol 4.1, SSL, secure connection and native-password plugin.
            let capabilities = 0x0008_8a01u32;
            let greeting = [
                vec![10],
                b"8.0.0\0".to_vec(),
                1u32.to_le_bytes().to_vec(),
                b"12345678\0".to_vec(),
                (capabilities as u16).to_le_bytes().to_vec(),
                vec![45],
                2u16.to_le_bytes().to_vec(),
                ((capabilities >> 16) as u16).to_le_bytes().to_vec(),
                vec![21],
                vec![0; 10],
                b"123456789012\0mysql_native_password\0".to_vec(),
            ]
            .concat();
            let header = [greeting.len() as u8, 0, 0, 0];
            socket.write_all(&header).await.unwrap();
            socket.write_all(&greeting).await.unwrap();
            let request = read_packet(&mut socket).await;
            assert_eq!(request.len(), 32, "only SSLRequest may precede TLS");
            assert_ne!(
                u32::from_le_bytes(request[..4].try_into().unwrap()) & 0x0800,
                0
            );
            let Ok(mut socket) = TlsAcceptor::from(server).accept(socket).await else {
                return false;
            };
            let login = read_packet(&mut socket).await;
            assert!(
                login
                    .windows(b"tls-fixture".len())
                    .any(|part| part == b"tls-fixture")
            );
            // A recognizable authentication error proves TLS completed without
            // implementing a parallel database/query protocol in the fixture.
            let error = b"\xff\x15\x04#28000TLS fixture authentication ended";
            socket
                .write_all(&[error.len() as u8, 0, 0, 3])
                .await
                .unwrap();
            socket.write_all(error).await.unwrap();
            true
        });
        let mut options = MySqlConnectOptions::new()
            .host("127.0.0.1")
            .port(port)
            .username("tls-fixture")
            .password("public-test-fixture")
            .ssl_mode(MySqlSslMode::VerifyIdentity);
        if trust_ca {
            options = options.ssl_ca_from_pem(tls.ca_pem);
        }
        let error = tokio::time::timeout(std::time::Duration::from_secs(3), options.connect())
            .await
            .unwrap()
            .unwrap_err();
        if authenticates {
            assert_eq!(
                error.as_database_error().unwrap().message(),
                "TLS fixture authentication ended"
            );
        } else {
            assert_certificate_rejection(&error, hostname != "127.0.0.1");
        }
        assert_eq!(peer.await.unwrap(), authenticates);
    }
}
