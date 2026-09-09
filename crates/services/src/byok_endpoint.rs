//! Network policy for user-configured OpenAI-compatible endpoints.
//! Public HTTPS is the default; optional strict mode uses the administrator registry.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

pub const COMPATIBLE_PROVIDER: &str = "openai-compatible";
pub const ENDPOINT_POLICY_ENV: &str = "ASTRA_BYOK_ENDPOINT_POLICY";
mod dns;
mod proxy;

/// Keeps any private CONNECT tunnel alive for the entire response/stream lifetime.
pub struct EndpointClient<T = reqwest::Client> {
    client: T,
    tunnel: Option<tokio::task::JoinHandle<()>>,
}
impl From<reqwest::Client> for EndpointClient {
    fn from(client: reqwest::Client) -> Self {
        Self {
            client,
            tunnel: None,
        }
    }
}
impl<T> std::ops::Deref for EndpointClient<T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        &self.client
    }
}
impl<T> Drop for EndpointClient<T> {
    fn drop(&mut self) {
        if let Some(task) = &self.tunnel {
            task.abort();
        }
    }
}

fn configured_value(name: &str) -> Result<Option<String>, String> {
    match std::env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(_) => Err(format!("Invalid {name}")),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndpointPolicy {
    PublicHttps,
    TrustedDomains,
}

impl EndpointPolicy {
    pub fn parse(value: Option<&str>) -> Result<Self, String> {
        match value {
            None | Some("public-https") => Ok(Self::PublicHttps),
            Some("trusted-domains") => Ok(Self::TrustedDomains),
            _ => Err(format!(
                "Invalid {ENDPOINT_POLICY_ENV}; expected public-https or trusted-domains"
            )),
        }
    }
}

pub fn parse_endpoint(raw: &str) -> Result<reqwest::Url, String> {
    if raw.len() > 2048 || raw.chars().any(char::is_control) || raw.contains('\\') {
        return Err("Invalid model base URL".into());
    }
    let url = reqwest::Url::parse(raw).map_err(|_| "Invalid model base URL")?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.host_str().is_none()
        || url.port_or_known_default() == Some(0)
    {
        return Err("Model base URL must use HTTPS without credentials, query or fragment".into());
    }
    let host = url.host_str().unwrap().trim_end_matches('.');
    if host == "localhost" || host.ends_with(".localhost") || !host.contains('.') {
        return Err("Model endpoint must be a public host".into());
    }
    if let Ok(ip) = host.trim_matches(['[', ']']).parse::<IpAddr>()
        && !public_ip(ip)
    {
        return Err("Model endpoint must use public IP addresses".into());
    }
    // Reject encoded path separators/dot segments rather than relying on
    // potentially different normalization in upstream gateways.
    if url.path().contains('%') {
        return Err("Model base URL cannot contain an encoded path".into());
    }
    Ok(url)
}

fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, _, _] = ip.octets();
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_broadcast()
                || ip.is_documentation()
                || ip.is_unspecified()
                || a == 0
                || a >= 224
                || (a == 100 && (64..=127).contains(&b))
                || (a == 198 && (b == 18 || b == 19))
                || (a == 192 && b == 0))
                && ip.octets() != [168, 63, 129, 16]
        }
        IpAddr::V6(ip) => {
            let s = ip.segments();
            // Global unicast only; exclude special-purpose and transition
            // ranges, including IPv4 mapped/NAT64 addresses.
            s[0] & 0xe000 == 0x2000
                && !(s[0] == 0x2001 && (s[1] < 0x200 || s[1] == 0xdb8))
                && s[0] != 0x2002
                && !(s[0] == 0x3fff && s[1] < 0x1000)
        }
    }
}

pub async fn require_endpoint_policy(
    pool: &sqlx::Pool<sqlx::MySql>,
    raw: &str,
) -> Result<(), String> {
    let policy = match std::env::var(ENDPOINT_POLICY_ENV) {
        Ok(value) => EndpointPolicy::parse(Some(&value))?,
        Err(std::env::VarError::NotPresent) => EndpointPolicy::parse(None)?,
        Err(_) => return Err(format!("Invalid {ENDPOINT_POLICY_ENV}")),
    };
    require_endpoint_policy_with_mode(pool, raw, policy).await
}

/// Applies the configured policy without relaxing URL or outbound DNS checks.
pub async fn require_endpoint_policy_with_mode(
    pool: &sqlx::Pool<sqlx::MySql>,
    raw: &str,
    policy: EndpointPolicy,
) -> Result<(), String> {
    let url = parse_endpoint(raw)?;
    if policy == EndpointPolicy::PublicHttps {
        return Ok(());
    }
    let permitted: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM runtime_llm_trusted_domains WHERE domain_host = ? \
         AND is_enabled = 1 AND (domain_port = ? OR \
         (IFNULL(domain_port, 0) = 0 AND ? = 443))",
    )
    .bind(url.host_str().unwrap())
    .bind(i32::from(url.port_or_known_default().unwrap()))
    .bind(i32::from(url.port_or_known_default().unwrap()))
    .fetch_one(pool)
    .await
    .map_err(|_| "Unable to check the model endpoint policy")?;
    if permitted == 0 {
        return Err(format!(
            "This deployment requires an approved model endpoint; {}:{} is not enabled. Contact the administrator.",
            url.host_str().unwrap(),
            url.port_or_known_default().unwrap()
        ));
    }
    Ok(())
}

fn transport_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .no_proxy()
        .retry(reqwest::retry::never())
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(360))
}

fn validate_addresses(addresses: &[SocketAddr]) -> Result<(), String> {
    if addresses.is_empty() || addresses.iter().any(|addr| !public_ip(addr.ip())) {
        return Err("Astra Server DNS returned non-public model addresses (possibly proxy Fake-IP). Configure ASTRA_BYOK_DNS_SERVERS to return real public addresses; the model API key is not the cause.".into());
    }
    Ok(())
}

#[cfg(test)]
fn pinned_client(url: &reqwest::Url, addresses: &[SocketAddr]) -> Result<reqwest::Client, String> {
    validate_addresses(addresses)?;
    transport_builder()
        // User-selected destinations require DNS pinning. An ambient proxy
        // would resolve the host again, outside this validation boundary.
        .resolve_to_addrs(url.host_str().unwrap(), addresses)
        .build()
        .map_err(|_| "Unable to create model HTTP client".into())
}

/// Resolve and pin a fresh public address set for each outbound attempt.
/// TLS still validates the original hostname. Redirects never forward keys.
pub async fn endpoint_client(raw: &str) -> Result<EndpointClient, String> {
    endpoint_client_with(raw, |builder| {
        builder
            .build()
            .map_err(|_| "Unable to create model HTTP client".into())
    })
    .await
}

/// Same DNS/egress owner as credential probes, with exact-wire inference
/// transport. The returned guard owns the tunnel for the entire response.
pub async fn endpoint_transport(
    raw: &str,
) -> Result<EndpointClient<astra_inference_adapter::transport::ProviderTransport>, String> {
    endpoint_client_with(raw, |builder| {
        astra_inference_adapter::transport::ProviderTransport::build(builder)
            .map_err(|_| "Unable to create model inference transport".into())
    })
    .await
}

async fn endpoint_client_with<T>(
    raw: &str,
    build: impl FnOnce(reqwest::ClientBuilder) -> Result<T, String>,
) -> Result<EndpointClient<T>, String> {
    let url = parse_endpoint(raw)?;
    let proxy = proxy::EgressProxy::parse(configured_value(proxy::PROXY_ENV)?.as_deref())?;
    let addresses = dns::resolve(&url, configured_value(dns::DNS_SERVERS_ENV)?.as_deref()).await?;
    validate_addresses(&addresses)?;
    match proxy {
        Some(proxy) => {
            proxy::client_with(&url, &addresses, proxy, transport_builder(), build).await
        }
        None => Ok(EndpointClient {
            client: build(
                transport_builder().resolve_to_addrs(url.host_str().unwrap(), &addresses),
            )?,
            tunnel: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_policy_defaults_to_public_https_and_invalid_config_fails_closed() {
        assert_eq!(
            EndpointPolicy::parse(None).unwrap(),
            EndpointPolicy::PublicHttps
        );
        assert_eq!(
            EndpointPolicy::parse(Some("trusted-domains")).unwrap(),
            EndpointPolicy::TrustedDomains
        );
        for invalid in ["", "public", "allow-all", "trusted-domain"] {
            assert!(EndpointPolicy::parse(Some(invalid)).is_err());
        }
    }

    #[tokio::test]
    async fn public_policy_does_not_require_a_registry_or_database_connection() {
        let pool = sqlx::mysql::MySqlPoolOptions::new()
            .connect_lazy("mysql://unused:unused@127.0.0.1:1/unused")
            .unwrap();
        for raw in [
            "https://api.moonshot.cn/v1",
            "https://another-provider.example:8443/v1",
        ] {
            require_endpoint_policy_with_mode(&pool, raw, EndpointPolicy::PublicHttps)
                .await
                .unwrap();
        }
        for policy in [EndpointPolicy::PublicHttps, EndpointPolicy::TrustedDomains] {
            assert!(
                require_endpoint_policy_with_mode(&pool, "https://127.0.0.1/v1", policy)
                    .await
                    .is_err()
            );
        }
    }

    #[test]
    fn endpoint_rejects_private_and_ambiguous_urls() {
        for raw in [
            "http://api.example.com/v1",
            "https://127.0.0.1/v1",
            "https://169.254.169.254",
            "https://10.1.1.1",
            "https://[::1]",
            "https://localhost",
            "https://x.localhost",
            "https://user:secret@api.example.com",
            "https://api.example.com?key=secret",
            "https://api.example.com/#x",
            "https://api.example.com/%2fadmin",
            "https://100.64.0.1",
            "https://2130706433",
            "https://0x7f000001",
            "https://168.63.129.16",
            "https://[::ffff:127.0.0.1]",
        ] {
            assert!(parse_endpoint(raw).is_err(), "accepted {raw}");
        }
        assert!(parse_endpoint("https://api.example.com:8443/compatible/v1").is_ok());
    }

    #[test]
    fn dns_pinning_rejects_mixed_and_rebound_answers() {
        let url = parse_endpoint("https://api.example.com/v1").unwrap();
        let public = "8.8.8.8:443".parse().unwrap();
        assert!(pinned_client(&url, &[public]).is_ok());
        for blocked in [
            "127.0.0.1:443",
            "10.1.1.1:443",
            "169.254.169.254:443",
            "[::ffff:127.0.0.1]:443",
            "[64:ff9b::a00:1]:443",
            "[fc00::1]:443",
            "168.63.129.16:443",
            "100.100.100.200:443",
            "[2001:db8::1]:443",
        ] {
            assert!(pinned_client(&url, &[public, blocked.parse().unwrap()]).is_err());
        }
        assert!(pinned_client(&url, &[]).is_err());
    }

    #[tokio::test]
    async fn transport_never_follows_provider_redirects() {
        use axum::{Router, routing::get};
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let hits = Arc::new(AtomicUsize::new(0));
        let observed = hits.clone();
        let app = Router::new()
            .route(
                "/redirect",
                get(|| async { axum::response::Redirect::temporary("/destination") }),
            )
            .route(
                "/destination",
                get(move || async move {
                    observed.fetch_add(1, Ordering::SeqCst);
                    "unexpected"
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let response = transport_builder()
            .build()
            .unwrap()
            .get(format!("http://{addr}/redirect"))
            .bearer_auth("test-secret")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(hits.load(Ordering::SeqCst), 0);
        server.abort();
    }
}
