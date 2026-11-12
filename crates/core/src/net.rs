//! Shared HTTP networking utilities.
//!
//! # TLS trust
//!
//! The workspace reqwest features combine bundled public roots with native
//! platform roots, including `SSL_CERT_FILE` / `SSL_CERT_DIR` overrides. Keep
//! this policy in the shared dependency rather than loading roots per client.
//! Certificate-chain and hostname verification remain enabled.
//!
//! WebSocket and proxy TLS use bundled Mozilla public roots through
//! [`public_root_tls_config`]. WSS callers must pass this explicit configuration;
//! the WebSocket dependency does not select or load a trust store.
//!
//! # Proxy policy
//!
//! Three tiers, by traffic destination — pick the matching helper instead of
//! hand-rolling proxy handling:
//!
//! 1. **Server-internal, always-local traffic** (runtime ↔ services on the
//!    same host: memoria, durable task, app state, ...): MUST bypass env
//!    proxies — use [`build_internal_http_client`].
//! 2. **Target-dependent traffic** (astra-cli / edge clients that may talk to
//!    either a local or a REMOTE astra server): use
//!    [`client_builder_for_target`] — loopback targets bypass proxies, remote
//!    targets honour the environment. Mandatory-egress-proxy sandboxes
//!    (OpenShell) depend on this: an unconditional `.no_proxy()` there makes
//!    remote calls hang.
//! 3. **External provider traffic** (the LLM client, provider connectivity
//!    probes): honours `HTTPS_PROXY`/`ALL_PROXY` via [`apply_env_proxy`] — the
//!    single authoritative env-proxy implementation; add callers rather than
//!    duplicating it.
//!
//! The historical rule "everything except the LLM client must .no_proxy()"
//! (commit 3e3d6fa8) applies ONLY to tier 1; tier 2 superseded it for client
//! code that can target remote servers.

/// Build an internal `reqwest` client that must never honor env proxy vars.
///
/// Callers should pass any desired timeouts / redirect policy / TLS settings in
/// `builder`; this helper enforces `.no_proxy()`. Construction failure stops
/// initialization rather than discarding the caller's transport policy.
pub fn build_internal_http_client(
    builder: reqwest::ClientBuilder,
    client_name: &'static str,
) -> reqwest::Client {
    builder.no_proxy().build().unwrap_or_else(|error| {
        panic!("failed to build configured internal HTTP client for {client_name}: {error}")
    })
}

/// Apply proxy settings from the environment to a `reqwest::ClientBuilder`.
///
/// Precedence (first match wins): `HTTPS_PROXY`, `https_proxy`, `ALL_PROXY`,
/// `all_proxy`. For `HTTPS_PROXY`/`https_proxy` we register an HTTPS-scheme
/// proxy; for `ALL_PROXY`/`all_proxy` we register an all-scheme proxy so that
/// `socks5://` URLs (which only make sense as all-scheme) are honoured.
///
/// `NO_PROXY` / `no_proxy` is always respected via `reqwest::NoProxy::from_env`.
///
/// Malformed or empty proxy URLs are logged at warn level and skipped; the
/// returned builder is always usable.
fn configured_env_proxy() -> Option<(reqwest::Proxy, String)> {
    let no_proxy = reqwest::NoProxy::from_env();
    for var in &["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"] {
        let Ok(proxy_url) = std::env::var(var) else {
            continue;
        };
        if proxy_url.is_empty() {
            continue;
        }
        let is_all = matches!(*var, "ALL_PROXY" | "all_proxy");
        let parsed = if is_all {
            reqwest::Proxy::all(&proxy_url)
        } else {
            reqwest::Proxy::https(&proxy_url)
        };
        match parsed {
            Ok(mut proxy) => {
                if let Some(np) = no_proxy.clone() {
                    proxy = proxy.no_proxy(Some(np));
                }
                tracing::info!(
                    target: "astra_core::net",
                    env_var = *var,
                    "proxy configured from environment"
                );
                return Some((proxy, proxy_url));
            }
            Err(_) => {
                tracing::warn!(
                    target: "astra_core::net",
                    env_var = *var,
                    "failed to parse proxy URL; ignoring"
                );
            }
        }
    }
    None
}

pub fn apply_env_proxy(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    match configured_env_proxy() {
        Some((proxy, _)) => builder.proxy(proxy),
        None => builder,
    }
}

/// The same configured proxy used for HTTPS provider traffic. The returned
/// URL may contain credentials and must never be logged or returned to users.
pub fn configured_https_proxy_url_for(url: &reqwest::Url) -> Result<Option<String>, &'static str> {
    let (_, proxy_url) = match configured_env_proxy() {
        Some(proxy) => proxy,
        None if ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]
            .iter()
            .any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty())) =>
        {
            return Err("Invalid Server HTTPS proxy configuration");
        }
        None => return Ok(None),
    };
    let no_proxy = std::env::var("NO_PROXY")
        .or_else(|_| std::env::var("no_proxy"))
        .unwrap_or_default();
    let matcher = hyper_util::client::proxy::matcher::Matcher::builder()
        .https("http://127.0.0.1")
        .no(no_proxy)
        .build();
    if url
        .as_str()
        .parse::<axum::http::Uri>()
        .ok()
        .is_some_and(|uri| matcher.intercept(&uri).is_none())
    {
        Ok(None)
    } else {
        Ok(Some(proxy_url))
    }
}

/// Returns `true` when `url` targets the local host (`localhost`,
/// `*.localhost`, or a loopback IP literal).
pub fn url_is_loopback(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .strip_suffix(".localhost")
            .is_some_and(|prefix| !prefix.is_empty())
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Start a `reqwest` client builder whose proxy policy is decided by the
/// target URL: loopback targets are process-local control-plane calls and
/// always bypass env proxies; remote targets keep reqwest's
/// environment-aware proxy behavior (`HTTP(S)_PROXY` / `NO_PROXY`).
///
/// Mandatory-egress-proxy environments (e.g. OpenShell sandboxes) block
/// direct remote connections, so clients that may talk to a remote service
/// must never force `.no_proxy()` unconditionally — use this helper instead.
pub fn client_builder_for_target(url: &str) -> reqwest::ClientBuilder {
    let builder = reqwest::Client::builder();
    if url_is_loopback(url) {
        builder.no_proxy()
    } else {
        builder
    }
}

#[cfg(test)]
mod internal_http_client_tests {
    #[test]
    #[should_panic(
        expected = "failed to build configured internal HTTP client for test policy client"
    )]
    fn invalid_builder_cannot_discard_transport_policy() {
        super::build_internal_http_client(
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(std::time::Duration::from_secs(1))
                .user_agent("invalid\nheader"),
            "test policy client",
        );
    }
}

#[cfg(test)]
mod client_builder_for_target_tests {
    use super::url_is_loopback;

    #[test]
    fn loopback_targets_are_detected() {
        assert!(url_is_loopback("http://localhost:8080/x"));
        assert!(url_is_loopback("http://api.localhost/x"));
        assert!(url_is_loopback("http://127.0.0.1:17001/x"));
        assert!(url_is_loopback("http://[::1]:17001/x"));
    }

    #[test]
    fn remote_and_invalid_targets_are_not_loopback() {
        assert!(!url_is_loopback("http://astra.example.com/x"));
        assert!(!url_is_loopback("http://10.0.0.8:17001/x"));
        assert!(!url_is_loopback("not a url"));
        assert!(!url_is_loopback("http://.localhost/x"));
    }
}

#[cfg(test)]
mod apply_env_proxy_tests {
    use super::{apply_env_proxy, configured_https_proxy_url_for};

    /// All four recognized env var names must be cleared for isolation, since
    /// `apply_env_proxy` reads them in precedence order.
    const PROXY_VARS: &[&str] = &["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"];

    fn clear_all() -> Vec<(&'static str, Option<String>)> {
        PROXY_VARS.iter().map(|v| (*v, None)).collect()
    }

    #[test]
    fn no_env_vars_leaves_builder_unmodified() {
        temp_env::with_vars(clear_all(), || {
            let builder = reqwest::Client::builder();
            let builder = apply_env_proxy(builder);
            assert!(
                builder.build().is_ok(),
                "builder should produce client with no proxy"
            );
        });
    }

    #[test]
    fn empty_proxy_url_is_ignored() {
        temp_env::with_vars([("HTTPS_PROXY", Some(""))], || {
            let builder = apply_env_proxy(reqwest::Client::builder());
            assert!(
                builder.build().is_ok(),
                "empty proxy URL should be ignored, not error"
            );
        });
    }

    #[test]
    fn malformed_proxy_url_is_ignored_not_panic() {
        temp_env::with_vars([("HTTPS_PROXY", Some("not a url ::::"))], || {
            let builder = apply_env_proxy(reqwest::Client::builder());
            assert!(
                builder.build().is_ok(),
                "malformed proxy must not break the builder"
            );
        });
    }

    #[test]
    fn valid_https_proxy_applied_without_panic() {
        temp_env::with_vars([("HTTPS_PROXY", Some("http://127.0.0.1:9999"))], || {
            let builder = apply_env_proxy(reqwest::Client::builder());
            assert!(builder.build().is_ok());
        });
    }

    #[test]
    fn socks5_via_all_proxy_is_accepted() {
        // Regression: ALL_PROXY must use Proxy::all() so socks5:// schemes work.
        temp_env::with_vars([("ALL_PROXY", Some("socks5://127.0.0.1:1080"))], || {
            let builder = apply_env_proxy(reqwest::Client::builder());
            assert!(builder.build().is_ok(), "socks5 via ALL_PROXY must build");
        });
    }

    #[test]
    fn https_proxy_takes_precedence_over_all_proxy() {
        temp_env::with_vars(
            [
                ("HTTPS_PROXY", Some("http://127.0.0.1:1")),
                ("ALL_PROXY", Some("socks5://127.0.0.1:2")),
            ],
            || {
                let builder = apply_env_proxy(reqwest::Client::builder());
                assert!(builder.build().is_ok());
            },
        );
    }

    #[test]
    fn provider_proxy_selection_uses_the_same_environment_and_no_proxy_rules() {
        temp_env::with_vars(
            [
                (
                    "HTTPS_PROXY",
                    Some("http://test-user:test-password@127.0.0.1:9999"),
                ),
                ("https_proxy", None),
                ("ALL_PROXY", Some("http://127.0.0.1:8888")),
                ("all_proxy", None),
                ("NO_PROXY", Some(".example.org,10.0.0.0/8")),
                ("no_proxy", None),
            ],
            || {
                let selected = |url| {
                    configured_https_proxy_url_for(&reqwest::Url::parse(url).unwrap()).unwrap()
                };
                assert_eq!(
                    selected("https://provider.test/v1").as_deref(),
                    Some("http://test-user:test-password@127.0.0.1:9999")
                );
                assert!(selected("https://api.example.org/v1").is_none());
                assert!(selected("https://10.4.3.2/v1").is_none());
                assert!(selected("https://other-example.org/v1").is_some());
            },
        );
    }

    #[test]
    fn invalid_proxy_configuration_does_not_silently_route_direct() {
        temp_env::with_vars(
            [
                ("HTTPS_PROXY", Some("http://invalid proxy")),
                ("https_proxy", None),
                ("ALL_PROXY", None),
                ("all_proxy", None),
            ],
            || {
                assert!(
                    configured_https_proxy_url_for(
                        &reqwest::Url::parse("https://provider.test").unwrap()
                    )
                    .is_err()
                );
            },
        );
    }
}

/// Mozilla public roots for WebSocket and proxy TLS. Keep this trust policy
/// explicit: HTTP clients separately include native platform roots.
pub fn public_root_tls_config() -> std::sync::Arc<rustls::ClientConfig> {
    let roots = rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("ring supports the default TLS protocol versions")
    .with_root_certificates(roots)
    .with_no_client_auth();
    std::sync::Arc::new(config)
}

/// The local host startup/attachment boundary uses this complete set of
/// supported network settings. It is not a general environment allowlist.
pub const RUNNER_NETWORK_ENV_VARS: &[&str] = &[
    "HTTPS_PROXY",
    "https_proxy",
    "HTTP_PROXY",
    "http_proxy",
    "ALL_PROXY",
    "all_proxy",
    "NO_PROXY",
    "no_proxy",
    "ASTRA_RUNNER_CA_BUNDLE",
];

/// Build the client used by Runner-local model traffic.
///
/// Proxy settings use the same explicit external-egress policy as Server
/// providers. Enterprises may additionally point `ASTRA_RUNNER_CA_BUNDLE` at
/// a PEM bundle. The path and certificate contents are deliberately excluded
/// from errors because this function is also used by user-facing diagnostics.
pub fn runner_provider_client_builder()
-> Result<reqwest::ClientBuilder, RunnerProviderNetworkConfigError> {
    const MAX_CA_BUNDLE_BYTES: u64 = 4 * 1024 * 1024;

    // A provider credential is authorized for exactly the configured origin.
    // Following even a seemingly harmless redirect would let an untrusted or
    // compromised endpoint choose the next Authorization recipient. Provider
    // migrations must therefore be an explicit binding revision, not HTTP
    // control flow.
    let mut builder = apply_env_proxy(
        reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none()),
    );
    let Some(path) = std::env::var_os("ASTRA_RUNNER_CA_BUNDLE") else {
        return Ok(builder);
    };
    let metadata = std::fs::metadata(&path)
        .map_err(|_| RunnerProviderNetworkConfigError::CaBundleUnreadable)?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_CA_BUNDLE_BYTES {
        return Err(RunnerProviderNetworkConfigError::CaBundleInvalid);
    }
    use std::io::Read;
    let file = std::fs::File::open(path)
        .map_err(|_| RunnerProviderNetworkConfigError::CaBundleUnreadable)?;
    let metadata = file
        .metadata()
        .map_err(|_| RunnerProviderNetworkConfigError::CaBundleUnreadable)?;
    if !metadata.is_file() {
        return Err(RunnerProviderNetworkConfigError::CaBundleInvalid);
    }
    let mut bytes = Vec::new();
    file.take(MAX_CA_BUNDLE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| RunnerProviderNetworkConfigError::CaBundleUnreadable)?;
    if bytes.is_empty() || bytes.len() as u64 > MAX_CA_BUNDLE_BYTES {
        return Err(RunnerProviderNetworkConfigError::CaBundleInvalid);
    }
    let certificates = reqwest::Certificate::from_pem_bundle(&bytes)
        .map_err(|_| RunnerProviderNetworkConfigError::CaBundleInvalid)?;
    if certificates.is_empty() {
        return Err(RunnerProviderNetworkConfigError::CaBundleInvalid);
    }
    for certificate in certificates {
        builder = builder.add_root_certificate(certificate);
    }
    Ok(builder)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunnerProviderNetworkConfigError {
    CaBundleUnreadable,
    CaBundleInvalid,
}

impl std::fmt::Display for RunnerProviderNetworkConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::CaBundleUnreadable => "Runner private CA bundle cannot be read",
            Self::CaBundleInvalid => {
                "Runner private CA bundle must be a non-empty PEM certificate bundle of at most 4 MiB"
            }
        })
    }
}

impl std::error::Error for RunnerProviderNetworkConfigError {}
