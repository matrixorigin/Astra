//! Query configured DNS servers directly; never reuse OS synthetic-address caches.
use hickory_resolver::{
    Resolver, TokioResolver,
    config::{
        LookupIpStrategy, NameServerConfig, ResolveHosts, ResolverConfig, ServerOrderingStrategy,
    },
    net::runtime::TokioRuntimeProvider,
};
use std::{
    net::{IpAddr, SocketAddr},
    time::Duration,
};

pub(super) const DNS_SERVERS_ENV: &str = "ASTRA_BYOK_DNS_SERVERS";

fn resolver(servers: Option<&str>) -> Result<TokioResolver, String> {
    let config = if let Some(servers) = servers {
        let mut config = ResolverConfig::from_parts(None, vec![], vec![]);
        if servers.len() > 2048 {
            return Err("Invalid ASTRA_BYOK_DNS_SERVERS".into());
        }
        for value in servers.split(',') {
            let value = value.trim();
            // A TCP-only route must not send a UDP query first: transparent
            // proxies may return a syntactically valid synthetic UDP answer.
            let (tcp_only, value) = match value.strip_prefix("tcp://") {
                Some(address) => (true, address),
                None => (false, value),
            };
            let address: SocketAddr = value.parse().or_else(|_| value.parse::<IpAddr>().map(|ip| SocketAddr::new(ip, 53)))
                .map_err(|_| "ASTRA_BYOK_DNS_SERVERS must contain comma-separated DNS IP addresses, optionally with ports and a tcp:// prefix")?;
            if address.port() == 0 || address.ip().is_unspecified() || address.ip().is_multicast() {
                return Err("Invalid ASTRA_BYOK_DNS_SERVERS address".into());
            }
            let mut ns = if tcp_only {
                NameServerConfig::tcp(address.ip())
            } else {
                NameServerConfig::udp_and_tcp(address.ip())
            };
            for connection in &mut ns.connections {
                connection.port = address.port();
            }
            config.add_name_server(ns);
        }
        config
    } else {
        hickory_resolver::system_conf::read_system_conf()
            .map_err(
                |_| "Astra Server could not read DNS servers; configure ASTRA_BYOK_DNS_SERVERS",
            )?
            .0
    };
    if config.name_servers().is_empty() {
        return Err("Astra Server has no DNS servers; configure ASTRA_BYOK_DNS_SERVERS".into());
    }
    let mut builder = Resolver::builder_with_config(config, TokioRuntimeProvider::default());
    let options = builder.options_mut();
    options.use_hosts_file = ResolveHosts::Never;
    // Respect DNS priority. Racing a VPN/system resolver against a secondary
    // resolver can otherwise choose a fast synthetic answer nondeterministically.
    options.server_ordering_strategy = ServerOrderingStrategy::UserProvidedOrder;
    options.num_concurrent_reqs = 1;
    options.ip_strategy = LookupIpStrategy::Ipv4AndIpv6;
    options.timeout = Duration::from_secs(2);
    options.attempts = 1;
    builder
        .build()
        .map_err(|_| "Unable to initialize Astra Server DNS resolver".into())
}

pub(super) async fn resolve(
    url: &reqwest::Url,
    servers: Option<&str>,
) -> Result<Vec<SocketAddr>, String> {
    // Validate operator configuration even for literal addresses.
    let resolver = resolver(servers)?;
    let host = url.host_str().unwrap().trim_matches(['[', ']']);
    let port = url.port_or_known_default().unwrap();
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    let fqdn = format!("{}.", host.trim_end_matches('.'));
    let lookup = tokio::time::timeout(Duration::from_secs(5), resolver.lookup_ip(fqdn)).await
        .map_err(|_| "Astra Server DNS lookup timed out; check its DNS/network configuration, not the model API key")?
        .map_err(|error| {
            // Log resolver evidence server-side only. Never include the URL path,
            // proxy credentials or model API key in the public error.
            tracing::warn!(host, error = ?error, "BYOK endpoint DNS lookup failed");
            "Astra Server could not resolve the model endpoint; check its DNS configuration and the provider hostname"
        })?;
    Ok(lookup.iter().map(|ip| SocketAddr::new(ip, port)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hickory_resolver::proto::{
        op::Message,
        rr::{
            RData, Record, RecordType,
            rdata::{A, AAAA},
        },
    };
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn tcp_only_avoids_fake_udp_and_still_rejects_private_answers() {
        let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = tcp.local_addr().unwrap();
        let udp = tokio::net::UdpSocket::bind(address).await.unwrap();
        let udp_seen = Arc::new(AtomicBool::new(false));
        let seen = udp_seen.clone();
        let udp_task = tokio::spawn(async move {
            let mut packet = [0; 4096];
            loop {
                let (size, peer) = udp.recv_from(&mut packet).await.unwrap();
                seen.store(true, Ordering::SeqCst);
                let mut reply = Message::from_vec(&packet[..size]).unwrap().into_response();
                let query = reply.queries[0].clone();
                if query.query_type() == RecordType::A {
                    reply.add_answer(Record::from_rdata(
                        query.name().clone(),
                        60,
                        RData::A(A("198.18.4.197".parse().unwrap())),
                    ));
                }
                udp.send_to(&reply.to_vec().unwrap(), peer).await.unwrap();
            }
        });
        let private = Arc::new(AtomicBool::new(false));
        let state = private.clone();
        let tcp_task = tokio::spawn(async move {
            loop {
                let (mut stream, _) = tcp.accept().await.unwrap();
                let state = state.clone();
                tokio::spawn(async move {
                    while let Ok(size) = stream.read_u16().await {
                        let mut packet = vec![0; usize::from(size)];
                        stream.read_exact(&mut packet).await.unwrap();
                        let mut reply = Message::from_vec(&packet).unwrap().into_response();
                        let query = reply.queries[0].clone();
                        let data = match query.query_type() {
                            RecordType::A => RData::A(A("8.8.8.8".parse().unwrap())),
                            RecordType::AAAA => RData::AAAA(AAAA(
                                if state.load(Ordering::SeqCst) {
                                    "::1"
                                } else {
                                    "2606:4700:4700::1111"
                                }
                                .parse()
                                .unwrap(),
                            )),
                            other => panic!("unexpected query {other}"),
                        };
                        reply.add_answer(Record::from_rdata(query.name().clone(), 60, data));
                        let packet = reply.to_vec().unwrap();
                        stream.write_u16(packet.len() as u16).await.unwrap();
                        stream.write_all(&packet).await.unwrap();
                    }
                });
            }
        });
        let url = reqwest::Url::parse("https://byok.test/v1").unwrap();
        let fake = resolve(&url, Some(&address.to_string())).await.unwrap();
        assert!(super::super::validate_addresses(&fake).is_err());
        assert!(udp_seen.swap(false, Ordering::SeqCst));
        let servers = format!("tcp://{address}");
        let real = resolve(&url, Some(&servers)).await.unwrap();
        assert_eq!(real.len(), 2);
        assert!(super::super::validate_addresses(&real).is_ok());
        private.store(true, Ordering::SeqCst);
        let rebound = resolve(&url, Some(&servers)).await.unwrap();
        assert!(super::super::validate_addresses(&rebound).is_err());
        assert!(!udp_seen.load(Ordering::SeqCst), "TCP-only mode used UDP");
        tcp_task.abort();
        tcp_task.await.unwrap_err();
        // UDP is still available, but a failed TCP route must fail closed.
        assert!(resolve(&url, Some(&servers)).await.is_err());
        assert!(
            !udp_seen.load(Ordering::SeqCst),
            "TCP failure fell back to UDP"
        );
        udp_task.abort();
    }

    #[tokio::test]
    async fn configured_dns_returns_all_addresses_and_rechecks_changed_answers() {
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let servers = socket.local_addr().unwrap().to_string();
        let private = Arc::new(AtomicBool::new(false));
        let state = private.clone();
        let task = tokio::spawn(async move {
            let mut packet = [0; 4096];
            loop {
                let (size, peer) = socket.recv_from(&mut packet).await.unwrap();
                let mut reply = Message::from_vec(&packet[..size]).unwrap().into_response();
                let query = reply.queries[0].clone();
                assert_eq!(query.name().to_string(), "byok.test.");
                let data = match query.query_type() {
                    RecordType::A => RData::A(A("8.8.8.8".parse().unwrap())),
                    RecordType::AAAA => RData::AAAA(AAAA(
                        if state.load(Ordering::SeqCst) {
                            "::1"
                        } else {
                            "2606:4700:4700::1111"
                        }
                        .parse()
                        .unwrap(),
                    )),
                    other => panic!("unexpected query {other}"),
                };
                reply.add_answer(Record::from_rdata(query.name().clone(), 60, data));
                socket
                    .send_to(&reply.to_vec().unwrap(), peer)
                    .await
                    .unwrap();
            }
        });
        let url = reqwest::Url::parse("https://byok.test:8443/v1").unwrap();
        let public = resolve(&url, Some(&servers)).await.unwrap();
        assert_eq!(public.len(), 2);
        assert!(public.iter().all(|address| address.port() == 8443));
        assert!(super::super::validate_addresses(&public).is_ok());
        private.store(true, Ordering::SeqCst);
        let rebound = resolve(&url, Some(&servers)).await.unwrap();
        assert_eq!(rebound.len(), 2);
        assert!(
            super::super::validate_addresses(&rebound).is_err(),
            "a cached public answer must not hide a new private AAAA record"
        );
        task.abort();
    }

    #[tokio::test]
    async fn unavailable_dns_fails_without_system_resolver_fallback() {
        let listener = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let url = reqwest::Url::parse("https://byok.test/v1").unwrap();
        let error = tokio::time::timeout(
            Duration::from_secs(6),
            resolve(&url, Some(&listener.local_addr().unwrap().to_string())),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert!(error.contains("DNS"));
    }

    #[tokio::test]
    async fn dns_priority_is_preserved_instead_of_racing_secondary_servers() {
        let primary = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let secondary = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let servers = format!(
            "{},{}",
            primary.local_addr().unwrap(),
            secondary.local_addr().unwrap()
        );
        let task = tokio::spawn(async move {
            let mut packet = [0; 4096];
            loop {
                let (size, peer) = primary.recv_from(&mut packet).await.unwrap();
                let mut reply = Message::from_vec(&packet[..size]).unwrap().into_response();
                let query = reply.queries[0].clone();
                let data = match query.query_type() {
                    RecordType::A => RData::A(A("8.8.8.8".parse().unwrap())),
                    RecordType::AAAA => RData::AAAA(AAAA("2606:4700:4700::1111".parse().unwrap())),
                    other => panic!("unexpected query {other}"),
                };
                reply.add_answer(Record::from_rdata(query.name().clone(), 60, data));
                tokio::time::sleep(Duration::from_millis(25)).await;
                primary
                    .send_to(&reply.to_vec().unwrap(), peer)
                    .await
                    .unwrap();
            }
        });
        let addresses = resolve(
            &reqwest::Url::parse("https://byok.test/v1").unwrap(),
            Some(&servers),
        )
        .await
        .unwrap();
        assert_eq!(addresses.len(), 2);
        assert!(super::super::validate_addresses(&addresses).is_ok());
        let mut packet = [0; 4096];
        assert!(
            tokio::time::timeout(Duration::from_millis(30), secondary.recv_from(&mut packet))
                .await
                .is_err(),
            "a successful primary DNS must not race secondary resolvers"
        );
        task.abort();
    }

    #[tokio::test]
    async fn explicit_dns_configuration_is_validated() {
        for invalid in [
            "",
            "dns.example",
            "https://dns.example",
            "127.0.0.1:0",
            "0.0.0.0",
            "1.1.1.1,",
            "tcp://",
            "tcp://dns.example",
            "tcp://127.0.0.1:0",
            "tcp://127.0.0.1:53/path",
            "udp://127.0.0.1",
            "tcp://user:password@127.0.0.1",
        ] {
            assert!(resolver(Some(invalid)).is_err(), "accepted {invalid}");
        }
        assert!(resolver(Some("127.0.0.1:5353,[::1]:5353")).is_ok());
        assert!(resolver(Some("tcp://127.0.0.1:5353,tcp://[::1]:5353")).is_ok());
    }
}
