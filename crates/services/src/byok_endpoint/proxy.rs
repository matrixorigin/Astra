//! Shared CONNECT adapter. Upstream proxies receive validated IPs, never
//! provider hostnames; reqwest retains end-to-end origin TLS/SNI.
use super::EndpointClient;
use base64::{Engine, engine::general_purpose::STANDARD};
use std::{
    collections::HashMap,
    io,
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex, OnceLock, Weak},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinSet,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_HEAD: usize = 8192;
const MAX_ROUTES: usize = 4096;
const MAX_CONNECTIONS: usize = 4096;
const MAX_PENDING_HANDSHAKES: usize = 64;

trait TunnelIo: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> TunnelIo for T {}
type Stream = Box<dyn TunnelIo>;

#[derive(Clone)]
pub(super) struct EgressProxy {
    url: reqwest::Url,
    username: String,
    password: String,
    tls: Arc<rustls::ClientConfig>,
}

fn invalid() -> String {
    "Invalid Server proxy URL; use an http://, https:// or socks5:// proxy".into()
}
fn failed(reason: &'static str) -> io::Error {
    io::Error::other(reason)
}

impl EgressProxy {
    pub(super) fn parse(raw: Option<&str>) -> Result<Option<Self>, String> {
        let Some(raw) = raw.filter(|value| !value.is_empty()) else {
            return Ok(None);
        };
        if raw.len() > 2048 || raw.chars().any(char::is_control) {
            return Err(invalid());
        }
        let normalized = if raw.contains("://") {
            std::borrow::Cow::Borrowed(raw)
        } else {
            std::borrow::Cow::Owned(format!("http://{raw}"))
        };
        let url = reqwest::Url::parse(&normalized).map_err(|_| invalid())?;
        if !matches!(url.scheme(), "http" | "https" | "socks5" | "socks5h")
            || url.host_str().is_none()
            || url.port() == Some(0)
            || !matches!(url.path(), "" | "/")
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(invalid());
        }
        let decode = |value: &str| {
            percent_encoding::percent_decode_str(value)
                .decode_utf8()
                .map(|value| value.into_owned())
                .map_err(|_| invalid())
        };
        let username = decode(url.username())?;
        let password = decode(url.password().unwrap_or_default())?;
        if username.len() > 255
            || password.len() > 255
            || username.contains(':')
            || username
                .chars()
                .chain(password.chars())
                .any(char::is_control)
        {
            return Err(invalid());
        }
        Ok(Some(Self {
            url,
            username,
            password,
            tls: astra_core::net::public_root_tls_config(),
        }))
    }

    async fn connect(&self, target: SocketAddr) -> io::Result<Stream> {
        let host = self.url.host_str().unwrap().trim_matches(['[', ']']);
        let port = self.url.port_or_known_default().unwrap_or(1080);
        // The proxy is operator-owned configuration, not a user-selected endpoint.
        let addresses = super::resolve_host(host, port).await?;
        let tcp = TcpStream::connect(addresses.as_slice()).await?;
        tcp.set_nodelay(true)?;
        let mut stream: Stream = if self.url.scheme() == "https" {
            let name = rustls::pki_types::ServerName::try_from(host.to_owned())
                .map_err(|_| failed("invalid proxy TLS name"))?;
            Box::new(
                tokio_rustls::TlsConnector::from(self.tls.clone())
                    .connect(name, tcp)
                    .await?,
            )
        } else {
            Box::new(tcp)
        };
        if matches!(self.url.scheme(), "socks5" | "socks5h") {
            socks_connect(&mut stream, target, &self.username, &self.password).await?;
        } else {
            let auth = if !self.username.is_empty() || !self.password.is_empty() {
                format!(
                    "Proxy-Authorization: Basic {}\r\n",
                    STANDARD.encode(format!("{}:{}", self.username, self.password))
                )
            } else {
                String::new()
            };
            stream
                .write_all(
                    format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n{auth}\r\n").as_bytes(),
                )
                .await?;
            let head = read_head(&mut stream).await?;
            let first = head.lines().next().unwrap_or_default();
            let fields: Vec<_> = first.split_whitespace().collect();
            if fields.len() < 2
                || !matches!(fields[0], "HTTP/1.0" | "HTTP/1.1")
                || fields[1] != "200"
            {
                return Err(failed("configured proxy refused CONNECT"));
            }
        }
        Ok(stream)
    }
}

async fn read_head(stream: &mut (impl AsyncRead + Unpin + ?Sized)) -> io::Result<String> {
    let mut head = Vec::new();
    loop {
        if head.len() >= MAX_HEAD {
            return Err(failed("proxy header exceeds limit"));
        }
        head.push(stream.read_u8().await?);
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    String::from_utf8(head).map_err(|_| failed("invalid proxy header"))
}

async fn socks_connect(
    stream: &mut Stream,
    target: SocketAddr,
    username: &str,
    password: &str,
) -> io::Result<()> {
    let auth = !username.is_empty() || !password.is_empty();
    stream
        .write_all(if auth { &[5, 1, 2] } else { &[5, 1, 0] })
        .await?;
    let mut selection = [0; 2];
    stream.read_exact(&mut selection).await?;
    if selection != [5, if auth { 2 } else { 0 }] {
        return Err(failed("SOCKS proxy authentication method rejected"));
    }
    if auth {
        let mut credentials = vec![1, username.len() as u8];
        credentials.extend_from_slice(username.as_bytes());
        credentials.push(password.len() as u8);
        credentials.extend_from_slice(password.as_bytes());
        stream.write_all(&credentials).await?;
        stream.read_exact(&mut selection).await?;
        if selection != [1, 0] {
            return Err(failed("SOCKS proxy authentication failed"));
        }
    }
    let mut request = vec![5, 1, 0];
    match target.ip() {
        IpAddr::V4(ip) => {
            request.push(1);
            request.extend_from_slice(&ip.octets());
        }
        IpAddr::V6(ip) => {
            request.push(4);
            request.extend_from_slice(&ip.octets());
        }
    }
    request.extend_from_slice(&target.port().to_be_bytes());
    stream.write_all(&request).await?;
    let mut reply = [0; 4];
    stream.read_exact(&mut reply).await?;
    if reply[..3] != [5, 0, 0] {
        return Err(failed("SOCKS proxy refused CONNECT"));
    }
    let length = match reply[3] {
        1 => 4,
        4 => 16,
        3 => usize::from(stream.read_u8().await?),
        _ => return Err(failed("invalid SOCKS reply")),
    };
    let mut bound = vec![0; length + 2];
    stream.read_exact(&mut bound).await?;
    Ok(())
}

struct Route {
    authority: String,
    targets: Vec<SocketAddr>,
    proxy: EgressProxy,
    cancel: tokio_util::sync::CancellationToken,
}

type Routes = Arc<Mutex<HashMap<String, Arc<Route>>>>;

struct Gateway {
    address: SocketAddr,
    routes: Routes,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(super) struct RouteLease {
    gateway: Arc<Gateway>,
    authorization: String,
    cancel: tokio_util::sync::CancellationToken,
}

impl Drop for RouteLease {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.gateway
            .routes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.authorization);
    }
}

impl Gateway {
    fn current() -> Result<Arc<Self>, String> {
        // Each Tokio runtime owns its listener. Weak entries do not keep an
        // idle runtime, listener, or registered request alive.
        static GATEWAYS: OnceLock<Mutex<HashMap<tokio::runtime::Id, Weak<Gateway>>>> =
            OnceLock::new();
        let runtime = tokio::runtime::Handle::current().id();
        let mut gateways = GATEWAYS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(gateway) = gateways.get(&runtime).and_then(Weak::upgrade)
            && !gateway.task.is_finished()
        {
            return Ok(gateway);
        }
        gateways.retain(|_, gateway| gateway.strong_count() != 0);
        let listener = std::net::TcpListener::bind("127.0.0.1:0")
            .map_err(|_| "Unable to initialize model proxy tunnel")?;
        listener
            .set_nonblocking(true)
            .map_err(|_| "Unable to initialize model proxy tunnel")?;
        let address = listener
            .local_addr()
            .map_err(|_| "Unable to initialize model proxy tunnel")?;
        let listener = TcpListener::from_std(listener)
            .map_err(|_| "Unable to initialize model proxy tunnel")?;
        let routes: Routes = Arc::new(Mutex::new(HashMap::new()));
        let active_routes = routes.clone();
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            let handshakes = Arc::new(tokio::sync::Semaphore::new(MAX_PENDING_HANDSHAKES));
            let mut permit = None;
            loop {
                tokio::select! {
                    acquired = handshakes.clone().acquire_owned(), if permit.is_none() && connections.len() < MAX_CONNECTIONS => {
                        permit = acquired.ok();
                    }
                    accepted = listener.accept(), if permit.is_some() => {
                        let handshake = permit.take().expect("handshake admission reserved");
                        match accepted {
                            Ok((incoming, _)) => {
                                let routes = active_routes.clone();
                                connections.spawn(async move { let _ = bridge(incoming, routes, handshake).await; });
                            }
                            Err(error) => {
                                // Resource pressure must not cancel unrelated active streams.
                                drop(handshake);
                                tracing::debug!(kind = ?error.kind(), "model proxy listener could not accept a connection");
                                tokio::time::sleep(Duration::from_millis(100)).await;
                            }
                        }
                    }
                    _ = connections.join_next(), if !connections.is_empty() => {}
                }
            }
        });
        let gateway = Arc::new(Self {
            address,
            routes,
            task,
        });
        gateways.insert(runtime, Arc::downgrade(&gateway));
        Ok(gateway)
    }
}

async fn bridge(
    mut incoming: TcpStream,
    routes: Routes,
    handshake: tokio::sync::OwnedSemaphorePermit,
) -> io::Result<()> {
    let setup = tokio::time::timeout(CONNECT_TIMEOUT, async {
        let head = read_head(&mut incoming).await?;
        let mut lines = head.lines();
        let request = lines
            .next()
            .ok_or_else(|| failed("invalid tunnel request"))?;
        let mut authorizations = lines
            .filter_map(|line| line.split_once(':'))
            .filter(|(name, _)| name.eq_ignore_ascii_case("proxy-authorization"))
            .map(|(_, value)| value.trim());
        let authorization = authorizations
            .next()
            .ok_or_else(|| failed("unauthorized tunnel"))?;
        if authorizations.next().is_some() {
            return Err(failed("ambiguous tunnel authorization"));
        }
        let route = routes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(authorization)
            .cloned()
            .ok_or_else(|| failed("unauthorized tunnel"))?;
        if request != format!("CONNECT {} HTTP/1.1", route.authority) {
            return Err(failed("unadmitted tunnel destination"));
        }
        drop(handshake);
        for &target in &route.targets {
            let stream = tokio::select! {
                biased;
                _ = route.cancel.cancelled() => return Err(failed("model request cancelled")),
                result = route.proxy.connect(target) => result,
            };
            if let Ok(stream) = stream {
                return Ok((stream, route));
            }
        }
        Err(failed(
            "configured proxy could not connect to a validated public address",
        ))
    })
    .await;
    let (mut upstream, route) = match setup {
        Ok(Ok(stream)) => stream,
        _ => {
            incoming
                .write_all(
                    b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await?;
            return Err(failed("model proxy tunnel unavailable"));
        }
    };
    incoming
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await?;
    // Only opaque, end-to-end TLS travels through the adapter. Neither proxy
    // authentication header is forwarded to the model endpoint. The request
    // owner enforces idle/cancellation/explicit deadlines; dropping its
    // EndpointClient cancels every connection belonging to its route.
    tokio::select! {
        biased;
        _ = route.cancel.cancelled() => {},
        result = tokio::io::copy_bidirectional(&mut incoming, &mut upstream) => { result?; }
    }
    Ok(())
}

pub(super) async fn client(
    url: &reqwest::Url,
    targets: &[SocketAddr],
    proxy: EgressProxy,
    builder: reqwest::ClientBuilder,
) -> Result<EndpointClient, String> {
    super::validate_addresses(targets)?;
    let authority = format!(
        "{}:{}",
        url.host_str().unwrap(),
        url.port_or_known_default().unwrap()
    );
    let gateway = Gateway::current()?;
    let token = uuid::Uuid::new_v4().to_string();
    let authorization = format!("Basic {}", STANDARD.encode(format!("astra:{token}")));
    let local_proxy = reqwest::Proxy::https(format!("http://{}", gateway.address))
        .map_err(|_| "Unable to initialize model proxy tunnel")?
        .basic_auth("astra", &token);
    let client = builder
        .no_proxy()
        .https_only(true)
        .proxy(local_proxy)
        .build()
        .map_err(|_| "Unable to initialize model proxy client")?;
    let cancel = tokio_util::sync::CancellationToken::new();
    {
        let mut routes = gateway
            .routes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if routes.len() >= MAX_ROUTES {
            return Err("Model proxy is busy; try again".into());
        }
        routes.insert(
            authorization.clone(),
            Arc::new(Route {
                authority,
                targets: targets.to_vec(),
                proxy,
                cancel: cancel.clone(),
            }),
        );
    }
    Ok(EndpointClient {
        client,
        _tunnel: Some(RouteLease {
            gateway,
            authorization,
            cancel,
        }),
    })
}

#[cfg(test)]
mod tests;
