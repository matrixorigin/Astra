//! Shared HTTP networking utilities.
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
/// `builder`; this helper enforces `.no_proxy()` and, if the customized build
/// fails, retries once with a minimal no-proxy client so internal service calls
/// never silently fall back to `reqwest::Client::new()` (which would re-enable
/// env proxy handling).
pub fn build_internal_http_client(
    builder: reqwest::ClientBuilder,
    client_name: &'static str,
) -> reqwest::Client {
    match builder.no_proxy().build() {
        Ok(client) => client,
        Err(error) => {
            tracing::warn!(
                target: "astra_core::net",
                client_name,
                error = %error,
                "failed to build configured internal HTTP client; retrying with minimal no-proxy client"
            );
            reqwest::Client::builder()
                .no_proxy()
                .build()
                .unwrap_or_else(|fallback_error| {
                    panic!(
                        "failed to build minimal no-proxy client for {client_name}: {fallback_error}"
                    )
                })
        }
    }
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
pub fn apply_env_proxy(mut builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    let no_proxy = reqwest::NoProxy::from_env();
    for var in &["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"] {
        let Ok(proxy_url) = std::env::var(var) else {
            continue;
        };
        if proxy_url.is_empty() {
            continue;
        }
        if let Some(proxy) = parse_environment_proxy(var, &proxy_url, no_proxy.clone()) {
            builder = builder.proxy(proxy);
            return builder;
        }
    }
    builder
}

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
    let bytes =
        std::fs::read(path).map_err(|_| RunnerProviderNetworkConfigError::CaBundleUnreadable)?;
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

fn parse_environment_proxy(
    var: &str,
    proxy_url: &str,
    no_proxy: Option<reqwest::NoProxy>,
) -> Option<reqwest::Proxy> {
    let parsed = if matches!(var, "ALL_PROXY" | "all_proxy") {
        reqwest::Proxy::all(proxy_url)
    } else {
        reqwest::Proxy::https(proxy_url)
    };
    match parsed {
        Ok(proxy) => {
            tracing::info!(
                target: "astra_core::net",
                env_var = var,
                "applying proxy from environment"
            );
            Some(proxy.no_proxy(no_proxy))
        }
        Err(_) => {
            // URLs and parser errors can contain credentials, private hosts,
            // and local paths. The variable name is enough to locate repair.
            tracing::warn!(
                target: "astra_core::net",
                env_var = var,
                "failed to parse proxy URL; ignoring"
            );
            None
        }
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
    use super::{apply_env_proxy, parse_environment_proxy, runner_provider_client_builder};

    #[test]
    fn proxy_configuration_logs_never_include_private_material() {
        use std::io::Write;
        use std::sync::{Arc, Mutex};

        #[derive(Clone)]
        struct Capture(Arc<Mutex<Vec<u8>>>);

        impl Write for Capture {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0
                    .lock()
                    .expect("capture lock")
                    .extend_from_slice(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let captured = Capture(Arc::new(Mutex::new(Vec::new())));
        let writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            assert!(
                parse_environment_proxy(
                    "HTTPS_PROXY",
                    "http://canary-user:canary-secret@canary-proxy.invalid/private-path",
                    None,
                )
                .is_some()
            );
            assert!(
                parse_environment_proxy(
                    "ALL_PROXY",
                    "http://canary-user:canary-secret@[invalid/private-path",
                    None,
                )
                .is_none()
            );
        });
        let output = String::from_utf8(captured.0.lock().expect("capture lock").clone())
            .expect("UTF-8 logs");
        assert!(output.contains("applying proxy from environment"));
        assert!(output.contains("failed to parse proxy URL; ignoring"));
        assert!(output.contains("HTTPS_PROXY"));
        assert!(output.contains("ALL_PROXY"));
        for private_value in [
            "canary-user",
            "canary-secret",
            "canary-proxy",
            "private-path",
            "[invalid",
        ] {
            assert!(
                !output.contains(private_value),
                "private configuration in logs"
            );
        }
    }

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
    fn runner_ca_failures_do_not_disclose_private_paths() {
        let private_path = "/private/customer/canary-ca.pem";
        temp_env::with_var("ASTRA_RUNNER_CA_BUNDLE", Some(private_path), || {
            let diagnostic = runner_provider_client_builder()
                .expect_err("missing CA bundle must fail closed")
                .to_string();
            assert!(diagnostic.contains("cannot be read"));
            assert!(!diagnostic.contains(private_path));
            assert!(!diagnostic.contains("customer"));
        });
    }
}
