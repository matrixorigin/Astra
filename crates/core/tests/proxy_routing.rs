use std::{net::SocketAddr, process::Command, time::Duration};

const CHILD: &str = "ASTRA_PROXY_ROUTING_TEST_CHILD";
const PROXY_VARS: [&str; 8] = [
    "HTTP_PROXY",
    "http_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "ALL_PROXY",
    "all_proxy",
    "NO_PROXY",
    "no_proxy",
];

#[test]
fn target_proxy_routes_to_the_expected_peer() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    if let Ok(expected_remote) = std::env::var(CHILD) {
        let direct: SocketAddr = std::env::var("ASTRA_PROXY_ROUTING_TEST_DIRECT")
            .unwrap()
            .parse()
            .unwrap();
        runtime.block_on(async {
            for (url, expected) in [
                (format!("http://{direct}/"), "direct"),
                (
                    format!("http://proxy-target.invalid:{}/", direct.port()),
                    expected_remote.as_str(),
                ),
            ] {
                let result = astra_core::net::client_builder_for_target(&url)
                    .resolve("proxy-target.invalid", direct)
                    .timeout(Duration::from_secs(3))
                    .build()
                    .unwrap()
                    .get(&url)
                    .send()
                    .await
                    .and_then(reqwest::Response::error_for_status);
                if expected == "unreachable" {
                    assert!(
                        result.unwrap_err().is_timeout(),
                        "remote proxy failure must not fall back to direct"
                    );
                } else {
                    assert_eq!(result.unwrap().text().await.unwrap(), expected);
                }
            }
        });
        return;
    }

    let (direct, proxy) = runtime.block_on(async {
        async fn serve(peer: &'static str) -> SocketAddr {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let app = axum::Router::new().fallback(move || async move { peer });
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            address
        }
        (serve("direct").await, serve("proxy").await)
    });
    let proxy_url = format!("http://{proxy}");
    // Own the port but never accept: the remote request must time out, not retry direct.
    let unavailable_proxy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let unavailable_proxy_url = format!("http://{}", unavailable_proxy.local_addr().unwrap());
    let cases = [
        (None, None, "direct"),
        (Some("HTTP_PROXY"), None, "proxy"),
        (Some("http_proxy"), None, "proxy"),
        (Some("ALL_PROXY"), None, "proxy"),
        (Some("all_proxy"), None, "proxy"),
        (Some("HTTP_PROXY"), Some("NO_PROXY"), "direct"),
        (Some("http_proxy"), Some("no_proxy"), "direct"),
        (Some("HTTP_PROXY"), None, "unreachable"),
    ];
    for (proxy_var, exclusion_var, expected) in cases {
        let mut child = Command::new(std::env::current_exe().unwrap());
        child
            .args([
                "--exact",
                "target_proxy_routes_to_the_expected_peer",
                "--nocapture",
            ])
            .env(CHILD, expected)
            .env("ASTRA_PROXY_ROUTING_TEST_DIRECT", direct.to_string())
            .env_remove("REQUEST_METHOD");
        for variable in PROXY_VARS {
            child.env_remove(variable);
        }
        if let Some(variable) = proxy_var {
            child.env(
                variable,
                if expected == "unreachable" {
                    &unavailable_proxy_url
                } else {
                    &proxy_url
                },
            );
        }
        if let Some(variable) = exclusion_var {
            child.env(variable, "proxy-target.invalid");
        }
        let output = child.output().unwrap();
        assert!(
            output.status.success(),
            "proxy={proxy_var:?}, exclusion={exclusion_var:?}, expected={expected}:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
