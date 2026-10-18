//! Offline coverage of the workspace reqwest TLS trust policy. Environment
//! overrides live only in child processes, never in the parallel test runner.
use std::{net::SocketAddr, process::Command, sync::Arc, time::Duration};

use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair, KeyUsagePurpose};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::{TlsAcceptor, rustls};

const CHILD: &str = "ASTRA_TLS_TRUST_TEST_CHILD";

#[test]
fn workspace_http_clients_verify_native_ca_and_hostname() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    if let Ok(case) = std::env::var(CHILD) {
        let address: SocketAddr = std::env::var("ASTRA_TLS_TRUST_TEST_ADDRESS")
            .unwrap()
            .parse()
            .unwrap();
        runtime.block_on(async {
            let host = if case == "wrong-host" {
                "wrong-host.invalid"
            } else {
                "native-ca.invalid"
            };
            let url = format!("https://{host}:{}/", address.port());
            // Cover the default dependency policy and all three shared owners.
            for builder in [
                reqwest::Client::builder().no_proxy(),
                astra_core::net::client_builder_for_target(&url),
                astra_core::net::apply_env_proxy(reqwest::Client::builder()),
            ] {
                let client = builder
                    .resolve(host, address)
                    .timeout(Duration::from_secs(3))
                    .build()
                    .unwrap();
                check_response(client, &url, &case).await;
            }
            let client = astra_core::net::build_internal_http_client(
                reqwest::Client::builder()
                    .resolve(host, address)
                    .timeout(Duration::from_secs(3)),
                "TLS trust regression",
            );
            check_response(client, &url, &case).await;
        });
        return;
    }

    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
    let ca_key = KeyPair::generate().unwrap();
    let ca = ca_params.self_signed(&ca_key).unwrap();
    let leaf_key = KeyPair::generate().unwrap();
    let leaf = CertificateParams::new(vec!["native-ca.invalid".into()])
        .unwrap()
        .signed_by(&leaf_key, &ca, &ca_key)
        .unwrap();
    let tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![leaf.der().clone()],
        rustls::pki_types::PrivatePkcs8KeyDer::from(leaf_key.serialize_der()).into(),
    )
    .unwrap();
    let address = runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(tls));
        tokio::spawn(async move {
            loop {
                let (tcp, _) = listener.accept().await.unwrap();
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    // Rejected certificates intentionally fail the handshake.
                    let Ok(mut stream) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let mut request = [0; 4096];
                    if stream.read(&mut request).await.is_ok_and(|n| n > 0) {
                        stream
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\ntrusted")
                            .await
                            .unwrap();
                        let _ = stream.shutdown().await;
                    }
                });
            }
        });
        address
    });

    let fixtures = tempfile::tempdir().unwrap();
    let ca_file = fixtures.path().join("ca.pem");
    let ca_dir = fixtures.path().join("roots");
    let empty_file = fixtures.path().join("empty.pem");
    let empty_dir = fixtures.path().join("empty");
    std::fs::create_dir(&ca_dir).unwrap();
    std::fs::create_dir(&empty_dir).unwrap();
    std::fs::write(&ca_file, ca.pem()).unwrap();
    std::fs::write(ca_dir.join("ca.pem"), ca.pem()).unwrap();
    std::fs::write(&empty_file, "").unwrap();

    for case in ["ca-file", "ca-dir", "untrusted", "wrong-host"] {
        let mut child = Command::new(std::env::current_exe().unwrap());
        child
            .args([
                "--exact",
                "workspace_http_clients_verify_native_ca_and_hostname",
                "--nocapture",
            ])
            .env(CHILD, case)
            .env("ASTRA_TLS_TRUST_TEST_ADDRESS", address.to_string())
            .env(
                "SSL_CERT_FILE",
                if case == "ca-file" || case == "wrong-host" {
                    &ca_file
                } else {
                    &empty_file
                },
            )
            .env(
                "SSL_CERT_DIR",
                if case == "ca-dir" {
                    &ca_dir
                } else {
                    &empty_dir
                },
            );
        for variable in [
            "HTTP_PROXY",
            "http_proxy",
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
            "NO_PROXY",
            "no_proxy",
        ] {
            child.env_remove(variable);
        }
        let output = child.output().unwrap();
        assert!(
            output.status.success(),
            "{case}:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

async fn check_response(client: reqwest::Client, url: &str, case: &str) {
    let result = client.get(url).send().await;
    if matches!(case, "ca-file" | "ca-dir") {
        let response = result.unwrap().error_for_status().unwrap();
        assert_eq!(response.text().await.unwrap(), "trusted");
    } else {
        let error = result.unwrap_err();
        assert!(error.is_connect() && !error.is_timeout(), "{error:?}");
        let diagnostic = format!("{error:?}");
        let expected = if case == "wrong-host" {
            "NotValidForName"
        } else {
            "UnknownIssuer"
        };
        assert!(diagnostic.contains(expected), "{diagnostic}");
    }
}
