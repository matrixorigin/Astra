//! Shared HTTP networking utilities.
//!
//! # Proxy policy (commit 3e3d6fa8)
//!
//! Only **external** HTTP traffic — the LLM client and provider connectivity
//! probes — is allowed to honour `HTTPS_PROXY` / `ALL_PROXY`. All other reqwest
//! clients in the workspace are considered internal (edge ↔ services, memoria,
//! durable task, app state, etc.) and MUST call `.no_proxy()` on their
//! builders. See `astra_runtime::turn::llm_client` module docs for the full
//! rationale.
//!
//! [`apply_env_proxy`] is the single authoritative implementation. Both the
//! LLM client and `validate_connectivity` in `astra-services` call it. Do not
//! duplicate this logic elsewhere — add a new caller instead.

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
                    proxy = %proxy_url,
                    "applying proxy from environment"
                );
                builder = builder.proxy(proxy);
                return builder;
            }
            Err(e) => {
                tracing::warn!(
                    target: "astra_core::net",
                    env_var = *var,
                    proxy = %proxy_url,
                    error = %e,
                    "failed to parse proxy URL; ignoring"
                );
            }
        }
    }
    builder
}

#[cfg(test)]
mod apply_env_proxy_tests {
    use super::apply_env_proxy;

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
}
