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

/// Captured external-provider proxy policy. Debug and serialization deliberately
/// do not expose private proxy addresses or credentials.
#[derive(Clone)]
pub struct ResolvedProxyConfig {
    proxy: Option<reqwest::Proxy>,
    private_binding: Vec<u8>,
}

impl std::fmt::Debug for ResolvedProxyConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedProxyConfig")
            .field("configured", &self.proxy.is_some())
            .finish_non_exhaustive()
    }
}

impl ResolvedProxyConfig {
    /// Resolve the existing proxy precedence once. Malformed values are skipped.
    pub fn capture() -> Self {
        let no_proxy_raw = std::env::var("NO_PROXY")
            .or_else(|_| std::env::var("no_proxy"))
            .ok();
        for (var, scope) in [
            ("HTTPS_PROXY", "https"),
            ("https_proxy", "https"),
            ("ALL_PROXY", "all"),
            ("all_proxy", "all"),
        ] {
            let Ok(proxy_url) = std::env::var(var) else {
                continue;
            };
            if proxy_url.is_empty() {
                continue;
            }
            if let Some(config) = Self::resolve(&proxy_url, scope, no_proxy_raw.as_deref()) {
                tracing::info!(env_var = var, "captured provider proxy");
                return config;
            }
            tracing::warn!(env_var = var, "invalid provider proxy; ignoring");
        }
        // With no explicit Astra proxy, reqwest's environment matcher also
        // considers HTTP_PROXY. Its CGI guard disables the entire implicit
        // matcher. Native OS proxy discovery is not enabled by Astra's reqwest
        // feature configuration. Preserve this remaining environment behavior
        // before disabling reqwest's ambient matcher in apply().
        if std::env::var_os("REQUEST_METHOD").is_none()
            && let Ok(raw) = std::env::var("HTTP_PROXY").or_else(|_| std::env::var("http_proxy"))
        {
            // The implicit matcher uses http::Uri, unlike Proxy's more
            // permissive URL input. Invalid implicit values remain ignored.
            if raw
                .parse::<axum::http::Uri>()
                .ok()
                .filter(|uri| {
                    matches!(
                        uri.scheme_str(),
                        None | Some("http" | "https" | "socks4" | "socks4a" | "socks5" | "socks5h")
                    )
                })
                .and_then(|uri| uri.authority().cloned())
                .is_some()
                && let Some(config) = Self::resolve(&raw, "http", no_proxy_raw.as_deref())
            {
                return config;
            }
        }
        Self {
            proxy: None,
            private_binding: b"astra:resolved-provider-proxy:v1:none".to_vec(),
        }
    }

    fn resolve(raw: &str, scope: &str, no_proxy_raw: Option<&str>) -> Option<Self> {
        let proxy = match scope {
            "https" => reqwest::Proxy::https(raw),
            "http" => reqwest::Proxy::http(raw),
            _ => reqwest::Proxy::all(raw),
        }
        .ok()?;
        let no_proxy =
            no_proxy_raw.map(|raw| reqwest::NoProxy::from_string(raw).unwrap_or_default());
        // Proxy accepts schemeless addresses. Canonicalize the same address for
        // private binding, but omit pure authentication material.
        let mut route = reqwest::Url::parse(raw)
            .ok()
            .filter(|url| url.host_str().is_some())
            .or_else(|| reqwest::Url::parse(&format!("http://{raw}")).ok())?;
        route.set_username("").ok()?;
        route.set_password(None).ok()?;
        let mut private_binding = b"astra:resolved-provider-proxy:v1".to_vec();
        for value in [scope, route.as_str(), no_proxy_raw.unwrap_or("")] {
            private_binding.extend_from_slice(&(value.len() as u64).to_be_bytes());
            private_binding.extend_from_slice(value.as_bytes());
        }
        Some(Self {
            proxy: Some(proxy.no_proxy(no_proxy)),
            private_binding,
        })
    }

    /// Apply exactly the captured policy, without consulting process environment.
    pub fn apply(&self, builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
        let builder = builder.no_proxy();
        match &self.proxy {
            Some(proxy) => builder.proxy(proxy.clone()),
            None => builder,
        }
    }

    /// Only the caller's established private digest owner may project this material.
    pub fn private_binding_digest<E>(
        &self,
        digest: impl FnOnce(&[u8]) -> Result<String, E>,
    ) -> Result<String, E> {
        digest(&self.private_binding)
    }
}

/// Apply the canonical captured external-provider environment policy.
pub fn apply_env_proxy(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    ResolvedProxyConfig::capture().apply(builder)
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
    use super::apply_env_proxy;

    /// All four recognized env var names must be cleared for isolation, since
    /// `apply_env_proxy` reads them in precedence order.
    const PROXY_VARS: &[&str] = &[
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "NO_PROXY",
        "no_proxy",
        "REQUEST_METHOD",
    ];

    fn clear_all() -> Vec<(&'static str, Option<String>)> {
        PROXY_VARS.iter().map(|v| (*v, None)).collect()
    }

    #[test]
    fn captured_proxy_binding_excludes_credentials_and_tracks_route_and_bypass() {
        use super::ResolvedProxyConfig;
        let first = ResolvedProxyConfig::resolve(
            "http://alice:secret@proxy.example:8080",
            "https",
            Some("internal.example"),
        )
        .unwrap();
        let rotated = ResolvedProxyConfig::resolve(
            "http://bob:other@proxy.example:8080",
            "https",
            Some("internal.example"),
        )
        .unwrap();
        assert_eq!(first.private_binding, rotated.private_binding);
        for changed in [
            ResolvedProxyConfig::resolve(
                "http://proxy.example:8081",
                "https",
                Some("internal.example"),
            ),
            ResolvedProxyConfig::resolve(
                "http://proxy.example:8080",
                "all",
                Some("internal.example"),
            ),
            ResolvedProxyConfig::resolve(
                "http://proxy.example:8080",
                "https",
                Some("other.example"),
            ),
        ] {
            assert_ne!(first.private_binding, changed.unwrap().private_binding);
        }
        let debug = format!("{first:?}");
        for secret in ["alice", "secret", "proxy.example", "internal.example"] {
            assert!(!debug.contains(secret));
            if matches!(secret, "alice" | "secret") {
                assert!(!String::from_utf8_lossy(&first.private_binding).contains(secret));
            }
        }
    }

    #[tokio::test]
    async fn captured_http_proxy_survives_environment_change() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                axum::Router::new().fallback(|| async { "captured proxy" }),
            )
            .await
            .unwrap();
        });
        let mut vars = clear_all();
        vars.iter_mut()
            .find(|(name, _)| *name == "HTTP_PROXY")
            .unwrap()
            .1 = Some(proxy_url);
        let captured = temp_env::with_vars(vars, super::ResolvedProxyConfig::capture);
        let client = temp_env::with_vars(clear_all(), || {
            captured
                .apply(reqwest::Client::builder())
                .timeout(std::time::Duration::from_secs(2))
                .build()
                .unwrap()
        });
        let response = client
            .get("http://frozen-proxy.invalid/")
            .send()
            .await
            .unwrap();
        assert_eq!(response.text().await.unwrap(), "captured proxy");
        server.abort();
    }

    #[test]
    fn implicit_http_proxy_obeys_cgi_and_explicit_https_precedence() {
        let mut vars = clear_all();
        for (name, value) in &mut vars {
            *value = match *name {
                "HTTP_PROXY" => Some("http://proxy.example:8080".into()),
                "REQUEST_METHOD" => Some("GET".into()),
                _ => None,
            };
        }
        temp_env::with_vars(vars, || {
            assert!(super::ResolvedProxyConfig::capture().proxy.is_none());
            temp_env::with_var(
                "HTTPS_PROXY",
                Some("http://secure-proxy.example:8080"),
                || {
                    let captured = super::ResolvedProxyConfig::capture();
                    assert!(captured.proxy.is_some());
                    assert!(
                        String::from_utf8_lossy(&captured.private_binding)
                            .contains("secure-proxy.example")
                    );
                },
            );
        });
    }

    #[test]
    fn no_env_vars_builds_direct_client() {
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
}
