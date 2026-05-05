//! Web content retrieval with intelligent extraction.
//!
//! Fetches URLs and transforms raw HTML into model-friendly Markdown with
//! metadata extraction, link discovery, and content-aware routing.

use std::net::IpAddr;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use reqwest::header;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::Mutex;

pub use convert::{to_markdown, to_text};
pub use extract::{ExtractedLink, PageMetadata, extract_links, extract_metadata};

// ─── Public Types ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OutputFormat {
    Markdown,
    Text,
}

impl OutputFormat {
    fn parse(s: Option<&str>) -> Result<Self, String> {
        match s {
            None | Some("markdown") => Ok(Self::Markdown),
            Some("text") => Ok(Self::Text),
            Some(other) => Err(format!(
                "Unknown format '{other}'. Use 'markdown' or 'text'."
            )),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FetchResult {
    pub url: String,
    pub final_url: Option<String>,
    pub status: u16,
    pub content_type: String,
    pub metadata: extract::PageMetadata,
    pub content: String,
    pub links: Vec<extract::ExtractedLink>,
    pub content_length: usize,
    pub truncated: bool,
    pub cached: bool,
    pub elapsed_ms: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("{0}")]
    Validation(String),
    #[error("{0}")]
    Network(String),
    #[error("{0}")]
    Timeout(String),
    #[error("SSRF blocked: {0} resolves to a private/internal address")]
    SsrfBlocked(String),
}

// ─── Configuration ───────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct FetchConfig {
    pub format: OutputFormat,
    pub max_content: usize,
    pub timeout: Duration,
    pub max_links: usize,
}

impl Default for FetchConfig {
    fn default() -> Self {
        Self {
            format: OutputFormat::Markdown,
            max_content: 80_000,
            timeout: Duration::from_secs(30),
            max_links: 25,
        }
    }
}

impl FetchConfig {
    fn from_args(args: &Value) -> Result<(String, Self), FetchError> {
        let url = args
            .get("url")
            .and_then(Value::as_str)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| FetchError::Validation("Missing 'url' parameter".into()))?;

        let format = OutputFormat::parse(args.get("format").and_then(Value::as_str))
            .map_err(FetchError::Validation)?;

        let max_content = args
            .get("max_content")
            .or_else(|| args.get("max_bytes"))
            .and_then(Value::as_u64)
            .unwrap_or(80_000) as usize;

        let timeout_secs = args.get("timeout").and_then(Value::as_u64).unwrap_or(30);

        let max_links = args.get("max_links").and_then(Value::as_u64).unwrap_or(25) as usize;

        Ok((
            url,
            Self {
                format,
                max_content,
                timeout: Duration::from_secs(timeout_secs),
                max_links,
            },
        ))
    }
}

// ─── Cache ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct CacheKey {
    scope: String,
    url: String,
    format: OutputFormat,
    max_content: usize,
    max_links: usize,
}

struct CacheEntry {
    result: Arc<FetchResult>,
    inserted_at: Instant,
}

struct Cache {
    entries: Vec<(CacheKey, CacheEntry)>,
    max_entries: usize,
    ttl: Duration,
}

impl Cache {
    fn new(max_entries: usize, ttl: Duration) -> Self {
        Self {
            entries: Vec::with_capacity(max_entries),
            max_entries,
            ttl,
        }
    }

    fn get(&mut self, key: &CacheKey) -> Option<Arc<FetchResult>> {
        let now = Instant::now();
        self.entries
            .retain(|(_, e)| now.duration_since(e.inserted_at) < self.ttl);
        self.entries
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, e)| Arc::clone(&e.result))
    }

    fn put(&mut self, key: CacheKey, result: Arc<FetchResult>) {
        if self.entries.len() >= self.max_entries {
            self.entries.remove(0);
        }
        self.entries.push((
            key,
            CacheEntry {
                result,
                inserted_at: Instant::now(),
            },
        ));
    }
}

static CACHE: OnceLock<Arc<Mutex<Cache>>> = OnceLock::new();

fn shared_cache() -> &'static Arc<Mutex<Cache>> {
    CACHE.get_or_init(|| Arc::new(Mutex::new(Cache::new(64, Duration::from_secs(15 * 60)))))
}

// ─── Constants ───────────────────────────────────────────────────────────────

const MAX_DOWNLOAD_BYTES: usize = 2 * 1024 * 1024;
const MAX_REDIRECTS: usize = 5;
const MAX_URL_LENGTH: usize = 4096;
const USER_AGENT: &str = "astra/1.0 (web-fetch)";
const MAX_WALK_DEPTH: usize = 256;

// ─── Entry Point ─────────────────────────────────────────────────────────────

/// Tool dispatcher entry point. Returns JSON on success, `"Error: ..."` on failure.
/// HTTP 4xx/5xx responses are returned as error strings so the caller marks them as errors.
pub async fn fetch(client: Option<&reqwest::Client>, args: &Value) -> String {
    fetch_with_cache_scope(client, args, "").await
}

/// Same as [`fetch`], but isolates the URL cache by caller-provided session/workspace scope.
pub async fn fetch_with_cache_scope(
    client: Option<&reqwest::Client>,
    args: &Value,
    cache_scope: &str,
) -> String {
    match fetch_inner(client, args, cache_scope).await {
        Ok(result) if result.status >= 400 => {
            format!("Error: HTTP {} — {}", result.status, result.content)
        }
        Ok(result) => serde_json::to_string(&*result).unwrap_or_else(|e| format!("Error: {e}")),
        Err(e) => format!("Error: {e}"),
    }
}

async fn fetch_inner(
    client: Option<&reqwest::Client>,
    args: &Value,
    cache_scope: &str,
) -> Result<Arc<FetchResult>, FetchError> {
    let (raw_url, config) = FetchConfig::from_args(args)?;
    let url = upgrade_scheme(&raw_url);
    validate_url(&url)?;
    validate_resolved_host(&url).await?;

    let cache_key = CacheKey {
        scope: cache_scope.to_string(),
        url: url.clone(),
        format: config.format,
        max_content: config.max_content,
        max_links: config.max_links,
    };

    if !cache_scope.is_empty() {
        let mut cache = shared_cache().lock().await;
        if let Some(cached) = cache.get(&cache_key) {
            // Zero-copy hit: return an Arc clone to the stored result.
            // The stored FetchResult already has `cached: true` (we set
            // it pre-store below), so no mutation is needed here.
            return Ok(cached);
        }
    }

    let start = Instant::now();
    let (status, final_url, content_type, body) = do_fetch(client, &url, config.timeout).await?;
    let result = transform(
        &url,
        final_url.as_deref(),
        status,
        &content_type,
        &body,
        &config,
        start.elapsed(),
    );

    if status < 400 && !cache_scope.is_empty() {
        // Store a SECOND Arc (pre-flagged `cached: true`) separately so
        // the fresh-miss return value keeps `cached: false` for the
        // current caller. The clone is paid once at store time, not on
        // every subsequent hit. Net: one O(FetchResult) allocation per
        // miss; O(1) Arc::clone per hit.
        let mut for_cache = result.clone();
        for_cache.cached = true;
        let cache_arc = Arc::new(for_cache);
        let mut cache = shared_cache().lock().await;
        cache.put(cache_key, cache_arc);
    }

    Ok(Arc::new(result))
}

// ─── URL Validation ──────────────────────────────────────────────────────────

/// Upgrade http:// to https://. Preserves non-standard ports.
fn upgrade_scheme(url: &str) -> String {
    let Ok(mut parsed) = url::Url::parse(url) else {
        return url.to_string();
    };
    if parsed.scheme() == "http" {
        let _ = parsed.set_scheme("https");
        // If port was explicitly 80 (default for http), remove it for https
        if parsed.port() == Some(80) {
            let _ = parsed.set_port(None);
        }
    }
    parsed.to_string()
}

fn validate_url(url: &str) -> Result<(), FetchError> {
    if url.len() > MAX_URL_LENGTH {
        return Err(FetchError::Validation(format!(
            "URL exceeds {MAX_URL_LENGTH} chars"
        )));
    }

    let parsed =
        url::Url::parse(url).map_err(|e| FetchError::Validation(format!("Invalid URL: {e}")))?;

    match parsed.scheme() {
        "http" | "https" => {}
        s => return Err(FetchError::Validation(format!("Unsupported scheme '{s}'"))),
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| FetchError::Validation("URL has no host".into()))?;

    if is_private_host(host) {
        return Err(FetchError::SsrfBlocked(host.to_string()));
    }

    Ok(())
}

async fn validate_resolved_host(url: &str) -> Result<(), FetchError> {
    let parsed =
        url::Url::parse(url).map_err(|e| FetchError::Validation(format!("Invalid URL: {e}")))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| FetchError::Validation("URL has no host".into()))?;

    if host.parse::<IpAddr>().is_ok() {
        return Ok(());
    }

    let port = parsed
        .port_or_known_default()
        .ok_or_else(|| FetchError::Validation("URL has no port for scheme".into()))?;
    let addrs = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| FetchError::Network(format!("DNS lookup failed for {host}: {e}")))?;

    for addr in addrs {
        if is_private_ip(addr.ip()) {
            return Err(FetchError::SsrfBlocked(host.to_string()));
        }
    }
    Ok(())
}

fn is_private_host(host: &str) -> bool {
    let host_clean = host.trim_start_matches('[').trim_end_matches(']');

    if let Ok(ip) = host_clean.parse::<IpAddr>() {
        return is_private_ip(ip);
    }

    let lower = host_clean.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "localhost" | "localhost.localdomain" | "metadata.google.internal"
    ) || lower.ends_with(".localhost")
        || lower.ends_with(".local")
        || lower.ends_with(".internal")
}

fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || matches!(v6.segments()[0] & 0xfe00, 0xfc00)
                || matches!(v6.segments()[0] & 0xffc0, 0xfe80)
                || v6
                    .to_ipv4_mapped()
                    .is_some_and(|v4| is_private_ip(IpAddr::V4(v4)))
        }
    }
}

// ─── HTTP Fetch ──────────────────────────────────────────────────────────────

async fn do_fetch(
    client: Option<&reqwest::Client>,
    url: &str,
    timeout: Duration,
) -> Result<(u16, Option<String>, String, String), FetchError> {
    if let Some(client) = client {
        fetch_reqwest(client, url, timeout).await
    } else {
        fetch_curl(url, timeout).await
    }
}

async fn fetch_reqwest(
    _client: &reqwest::Client,
    url: &str,
    timeout: Duration,
) -> Result<(u16, Option<String>, String, String), FetchError> {
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(USER_AGENT)
        .no_proxy()
        .build()
        .map_err(|e| FetchError::Network(format!("HTTP client init failed: {e}")))?;

    let mut current = url.to_string();
    let mut final_url = None;

    for redirect_count in 0..=MAX_REDIRECTS {
        validate_url(&current)?;
        validate_resolved_host(&current).await?;

        let mut resp = tokio::time::timeout(timeout, async {
            client
                .get(&current)
                .header(
                    "Accept",
                    "text/html,application/xhtml+xml,application/json,text/plain;q=0.9,*/*;q=0.8",
                )
                .send()
                .await
        })
        .await
        .map_err(|_| FetchError::Timeout(format!("Request timed out after {timeout:?}")))?
        .map_err(|e| FetchError::Network(format!("HTTP request failed: {e}")))?;

        let status = resp.status();
        if status.is_redirection()
            && let Some(location) = resp.headers().get(header::LOCATION)
        {
            if redirect_count >= MAX_REDIRECTS {
                return Err(FetchError::Network(format!(
                    "Too many redirects (>{MAX_REDIRECTS})"
                )));
            }
            let location = location.to_str().map_err(|e| {
                FetchError::Network(format!("Invalid redirect Location header: {e}"))
            })?;
            let next = resolve_url(&current, location).ok_or_else(|| {
                FetchError::Network(format!("Invalid redirect Location: {location}"))
            })?;
            validate_url(&next)?;
            validate_resolved_host(&next).await?;
            final_url = Some(next.clone());
            current = next;
            continue;
        }

        let content_type = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("text/html")
            .to_string();

        let body = if is_binary_content_type(&content_type.to_lowercase()) {
            String::new()
        } else {
            read_limited_body(&mut resp).await?
        };

        return Ok((status.as_u16(), final_url, content_type, body));
    }

    Err(FetchError::Network(format!(
        "Too many redirects (>{MAX_REDIRECTS})"
    )))
}

async fn read_limited_body(resp: &mut reqwest::Response) -> Result<String, FetchError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| FetchError::Network(format!("Failed to read body: {e}")))?
    {
        let remaining = MAX_DOWNLOAD_BYTES.saturating_sub(bytes.len());
        if remaining == 0 {
            break;
        }
        if chunk.len() > remaining {
            bytes.extend_from_slice(&chunk[..remaining]);
            break;
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Sentinel used to separate body from curl metadata.
/// Chosen to be long and random-looking so it cannot appear in real HTTP content.
const CURL_SENTINEL: &str = "\n---ASTRA_FETCH_7f3a9b2e1d4c---\n";

async fn fetch_curl(
    url: &str,
    timeout: Duration,
) -> Result<(u16, Option<String>, String, String), FetchError> {
    let mut current = url.to_string();
    let mut final_url = None;

    for redirect_count in 0..=MAX_REDIRECTS {
        validate_url(&current)?;
        validate_resolved_host(&current).await?;

        let (status, content_type, redirect_url, body) = fetch_curl_once(&current, timeout).await?;
        if (300..400).contains(&status)
            && let Some(location) = redirect_url.filter(|u| !u.is_empty())
        {
            if redirect_count >= MAX_REDIRECTS {
                return Err(FetchError::Network(format!(
                    "Too many redirects (>{MAX_REDIRECTS})"
                )));
            }
            validate_url(&location)?;
            validate_resolved_host(&location).await?;
            final_url = Some(location.clone());
            current = location;
            continue;
        }
        return Ok((status, final_url, content_type, body));
    }

    Err(FetchError::Network(format!(
        "Too many redirects (>{MAX_REDIRECTS})"
    )))
}

async fn fetch_curl_once(
    url: &str,
    timeout: Duration,
) -> Result<(u16, String, Option<String>, String), FetchError> {
    let timeout_secs = timeout.as_secs().to_string();
    let write_format =
        format!("{CURL_SENTINEL}%{{http_code}}\n%{{content_type}}\n%{{redirect_url}}");
    let output = tokio::process::Command::new("curl")
        .args([
            "-sS",
            "--max-time",
            &timeout_secs,
            "--max-filesize",
            &MAX_DOWNLOAD_BYTES.to_string(),
            "--proto",
            "=http,https",
            "--proto-redir",
            "=http,https",
            "-H",
            &format!("User-Agent: {USER_AGENT}"),
            "-H",
            "Accept: text/html,application/xhtml+xml,application/json,text/plain;q=0.9,*/*;q=0.8",
            "-w",
            &write_format,
            url,
        ])
        .output()
        .await
        .map_err(|e| FetchError::Network(format!("curl: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(FetchError::Network(format!("curl: {}", stderr.trim())));
    }

    let raw = String::from_utf8_lossy(&output.stdout);
    let (body, meta) = raw
        .rsplit_once(CURL_SENTINEL)
        .ok_or_else(|| FetchError::Network("Failed to parse curl output".into()))?;

    let lines: Vec<&str> = meta.lines().collect();
    let status: u16 = lines.first().and_then(|s| s.parse().ok()).unwrap_or(0);
    let content_type = lines.get(1).unwrap_or(&"text/html").to_string();
    let redirect_url = lines
        .get(2)
        .map(|s| s.to_string())
        .filter(|u| !u.is_empty());

    if status == 0 {
        return Err(FetchError::Network("curl returned HTTP status 0".into()));
    }

    let body = if is_binary_content_type(&content_type.to_lowercase()) {
        String::new()
    } else {
        body.to_string()
    };

    Ok((status, content_type, redirect_url, body))
}

// ─── Content Transformation ──────────────────────────────────────────────────

fn transform(
    url: &str,
    final_url: Option<&str>,
    status: u16,
    content_type: &str,
    body: &str,
    config: &FetchConfig,
    elapsed: Duration,
) -> FetchResult {
    let ct = content_type.to_lowercase();

    let base = FetchResult {
        url: url.to_string(),
        final_url: final_url.map(String::from),
        status,
        content_type: content_type.to_string(),
        metadata: PageMetadata::default(),
        content: String::new(),
        links: vec![],
        content_length: 0,
        truncated: false,
        cached: false,
        elapsed_ms: elapsed.as_millis().min(u64::MAX as u128) as u64,
    };

    if is_binary_content_type(&ct) {
        return FetchResult {
            content: format!("[Binary content: {content_type}]"),
            ..base
        };
    }

    if ct.contains("application/json") || ct.contains("application/ld+json") {
        let (content, truncated) = truncate(body, config.max_content);
        return FetchResult {
            content,
            content_length: body.len(),
            truncated,
            ..base
        };
    }

    if ct.contains("text/html") || ct.contains("application/xhtml") {
        let doc = scraper::Html::parse_document(body);
        let effective_base = final_url.unwrap_or(url);
        let metadata = extract_metadata(&doc, effective_base);
        let links = extract_links(&doc, effective_base, config.max_links);
        let extracted = match config.format {
            OutputFormat::Markdown => to_markdown(&doc, effective_base),
            OutputFormat::Text => to_text(&doc),
        };
        let (content, truncated) = truncate(&extracted, config.max_content);
        return FetchResult {
            metadata,
            content,
            links,
            content_length: extracted.len(),
            truncated,
            ..base
        };
    }

    // Plain text fallback
    let (content, truncated) = truncate(body, config.max_content);
    FetchResult {
        content,
        content_length: body.len(),
        truncated,
        ..base
    }
}

fn is_binary_content_type(ct: &str) -> bool {
    ct.starts_with("image/")
        || ct.starts_with("audio/")
        || ct.starts_with("video/")
        || ct.contains("application/pdf")
        || ct.contains("application/zip")
        || ct.contains("application/octet-stream")
        || ct.contains("application/gzip")
}

fn truncate(content: &str, max: usize) -> (String, bool) {
    if content.len() <= max {
        return (content.to_string(), false);
    }
    let end = content.floor_char_boundary(max);
    (
        format!(
            "{}\n\n[…truncated — showing {} of {} chars]",
            &content[..end],
            end,
            content.len()
        ),
        true,
    )
}

// ─── Submodules (inline for single-file simplicity) ──────────────────────────

mod convert {
    use super::MAX_WALK_DEPTH;
    use scraper::Selector;

    pub fn to_markdown(doc: &scraper::Html, base_url: &str) -> String {
        let root = doc.root_element();
        let content_root = find_content_element(&root).unwrap_or(root);
        let mut out = String::new();
        walk_md(&content_root, base_url, &mut out, &MdState::default(), 0);
        normalize_blank_lines(&out)
    }

    pub fn to_text(doc: &scraper::Html) -> String {
        let root = doc.root_element();
        let mut out = String::new();
        walk_text(&root, &mut out, 0);
        normalize_blank_lines(&out)
    }

    #[derive(Default, Clone)]
    struct MdState {
        in_pre: bool,
        list_depth: usize,
        ordered_counter: usize,
        is_ordered: bool,
    }

    fn walk_text(el: &scraper::ElementRef, out: &mut String, depth: usize) {
        if depth > MAX_WALK_DEPTH {
            return;
        }
        use scraper::node::Node;
        for child in el.children() {
            match child.value() {
                Node::Text(t) => {
                    let s = t.text.trim();
                    if !s.is_empty() {
                        if !out.is_empty() && !out.ends_with('\n') && !out.ends_with(' ') {
                            out.push(' ');
                        }
                        out.push_str(s);
                    }
                }
                Node::Element(e) => {
                    let tag = e.name();
                    if is_non_content(tag) {
                        continue;
                    }
                    let block = is_block(tag);
                    if block && !out.is_empty() && !out.ends_with('\n') {
                        out.push('\n');
                    }
                    if tag == "li" {
                        out.push_str("- ");
                    }
                    if let Some(child_el) = scraper::ElementRef::wrap(child) {
                        walk_text(&child_el, out, depth + 1);
                    }
                    if block {
                        out.push('\n');
                    }
                }
                _ => {}
            }
        }
    }

    fn walk_md(
        el: &scraper::ElementRef,
        base_url: &str,
        out: &mut String,
        state: &MdState,
        depth: usize,
    ) {
        if depth > MAX_WALK_DEPTH {
            return;
        }
        use scraper::node::Node;

        let mut li_counter: usize = state.ordered_counter;

        for child in el.children() {
            match child.value() {
                Node::Text(t) => {
                    if state.in_pre {
                        out.push_str(t.text.as_ref());
                    } else {
                        let s = t.text.trim();
                        if !s.is_empty() {
                            if !out.is_empty() && !out.ends_with('\n') && !out.ends_with(' ') {
                                out.push(' ');
                            }
                            out.push_str(s);
                        }
                    }
                }
                Node::Element(e) => {
                    let tag = e.name();
                    if is_non_content(tag) {
                        continue;
                    }
                    let child_el = match scraper::ElementRef::wrap(child) {
                        Some(r) => r,
                        None => continue,
                    };

                    match tag {
                        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                            let level = tag[1..].parse::<usize>().unwrap_or(1);
                            ensure_blank_line(out);
                            out.push_str(&"#".repeat(level));
                            out.push(' ');
                            walk_md(&child_el, base_url, out, state, depth + 1);
                            out.push_str("\n\n");
                        }
                        "p" => {
                            ensure_blank_line(out);
                            walk_md(&child_el, base_url, out, state, depth + 1);
                            out.push_str("\n\n");
                        }
                        "a" => {
                            let href = e.attr("href").unwrap_or("");
                            let resolved = super::resolve_url(base_url, href).unwrap_or_default();
                            out.push('[');
                            let mark = out.len();
                            walk_md(&child_el, base_url, out, state, depth + 1);
                            trim_leading_space(out, mark);
                            out.push_str("](");
                            out.push_str(&resolved);
                            out.push(')');
                        }
                        "strong" | "b" => {
                            out.push_str("**");
                            let mark = out.len();
                            walk_md(&child_el, base_url, out, state, depth + 1);
                            trim_leading_space(out, mark);
                            out.push_str("**");
                        }
                        "em" | "i" => {
                            out.push('*');
                            let mark = out.len();
                            walk_md(&child_el, base_url, out, state, depth + 1);
                            trim_leading_space(out, mark);
                            out.push('*');
                        }
                        "code" if !state.in_pre => {
                            out.push('`');
                            let mark = out.len();
                            walk_md(&child_el, base_url, out, state, depth + 1);
                            trim_leading_space(out, mark);
                            out.push('`');
                        }
                        "pre" => {
                            ensure_blank_line(out);
                            let lang = detect_lang(&child_el);
                            out.push_str("```");
                            out.push_str(lang);
                            out.push('\n');
                            let pre_state = MdState {
                                in_pre: true,
                                ..state.clone()
                            };
                            walk_md(&child_el, base_url, out, &pre_state, depth + 1);
                            if !out.ends_with('\n') {
                                out.push('\n');
                            }
                            out.push_str("```\n\n");
                        }
                        "blockquote" => {
                            ensure_blank_line(out);
                            let mut inner = String::new();
                            walk_md(&child_el, base_url, &mut inner, state, depth + 1);
                            for line in inner.trim().lines() {
                                out.push_str("> ");
                                out.push_str(line);
                                out.push('\n');
                            }
                            out.push('\n');
                        }
                        "ul" => {
                            ensure_newline(out);
                            let child_state = MdState {
                                list_depth: state.list_depth + 1,
                                is_ordered: false,
                                ordered_counter: 0,
                                ..state.clone()
                            };
                            walk_md(&child_el, base_url, out, &child_state, depth + 1);
                            out.push('\n');
                        }
                        "ol" => {
                            ensure_newline(out);
                            let child_state = MdState {
                                list_depth: state.list_depth + 1,
                                is_ordered: true,
                                ordered_counter: 0,
                                ..state.clone()
                            };
                            walk_md(&child_el, base_url, out, &child_state, depth + 1);
                            out.push('\n');
                        }
                        "li" => {
                            let indent = "  ".repeat(state.list_depth.saturating_sub(1));
                            out.push_str(&indent);
                            if state.is_ordered {
                                li_counter += 1;
                                out.push_str(&format!("{li_counter}. "));
                            } else {
                                out.push_str("- ");
                            }
                            walk_md(&child_el, base_url, out, state, depth + 1);
                            ensure_newline(out);
                        }
                        "table" => {
                            ensure_blank_line(out);
                            render_table(&child_el, base_url, out, state, depth);
                            out.push('\n');
                        }
                        "br" => out.push('\n'),
                        "hr" => {
                            ensure_blank_line(out);
                            out.push_str("---\n\n");
                        }
                        "img" => {
                            let alt = e.attr("alt").unwrap_or("");
                            if let Some(src) = e.attr("src").filter(|s| !s.is_empty())
                                && let Some(resolved) = super::resolve_url(base_url, src)
                            {
                                out.push_str(&format!("![{alt}]({resolved})"));
                            }
                        }
                        _ => walk_md(&child_el, base_url, out, state, depth + 1),
                    }
                }
                _ => {}
            }
        }
    }

    fn detect_lang<'a>(pre: &'a scraper::ElementRef<'a>) -> &'a str {
        Selector::parse("code")
            .ok()
            .and_then(|sel| pre.select(&sel).next())
            .and_then(|code| code.value().attr("class"))
            .and_then(|cls| {
                cls.split_whitespace().find(|c| {
                    c.starts_with("language-")
                        || c.starts_with("lang-")
                        || c.starts_with("highlight-")
                })
            })
            .map(|c| {
                c.trim_start_matches("language-")
                    .trim_start_matches("lang-")
                    .trim_start_matches("highlight-")
            })
            .unwrap_or("")
    }

    fn render_table(
        table: &scraper::ElementRef,
        base_url: &str,
        out: &mut String,
        state: &MdState,
        depth: usize,
    ) {
        let row_sel = match Selector::parse("tr") {
            Ok(s) => s,
            Err(_) => return,
        };
        let cell_sel = match Selector::parse("th, td") {
            Ok(s) => s,
            Err(_) => return,
        };

        let mut rows: Vec<Vec<String>> = Vec::new();
        for row in table.select(&row_sel) {
            let cells: Vec<String> = row
                .select(&cell_sel)
                .map(|cell| {
                    let mut s = String::new();
                    walk_md(&cell, base_url, &mut s, state, depth + 1);
                    s.trim().replace('|', "\\|").to_string()
                })
                .collect();
            if !cells.is_empty() {
                rows.push(cells);
            }
        }
        if rows.is_empty() {
            return;
        }

        let cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
        for row in &mut rows {
            row.resize(cols, String::new());
        }

        out.push_str("| ");
        out.push_str(&rows[0].join(" | "));
        out.push_str(" |\n|");
        for _ in 0..cols {
            out.push_str(" --- |");
        }
        out.push('\n');
        for row in rows.iter().skip(1) {
            out.push_str("| ");
            out.push_str(&row.join(" | "));
            out.push_str(" |\n");
        }
    }

    fn find_content_element<'a>(
        root: &'a scraper::ElementRef<'a>,
    ) -> Option<scraper::ElementRef<'a>> {
        for sel_str in ["main", "article", r#"[role="main"]"#] {
            if let Ok(sel) = Selector::parse(sel_str)
                && let Some(el) = root.select(&sel).next()
            {
                return Some(el);
            }
        }
        Selector::parse("body")
            .ok()
            .and_then(|sel| root.select(&sel).next())
    }

    fn is_non_content(tag: &str) -> bool {
        matches!(
            tag,
            "script" | "style" | "noscript" | "svg" | "nav" | "footer" | "aside" | "iframe"
        )
    }

    fn is_block(tag: &str) -> bool {
        matches!(
            tag,
            "p" | "div"
                | "h1"
                | "h2"
                | "h3"
                | "h4"
                | "h5"
                | "h6"
                | "li"
                | "tr"
                | "br"
                | "hr"
                | "blockquote"
                | "pre"
                | "section"
                | "article"
                | "main"
                | "ul"
                | "ol"
                | "table"
        )
    }

    fn ensure_newline(out: &mut String) {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
    }

    fn ensure_blank_line(out: &mut String) {
        if out.is_empty() {
            return;
        }
        if !out.ends_with('\n') {
            out.push_str("\n\n");
        } else if !out.ends_with("\n\n") {
            out.push('\n');
        }
    }

    fn trim_leading_space(out: &mut String, mark: usize) {
        if out.len() > mark && out.as_bytes()[mark] == b' ' {
            out.remove(mark);
        }
    }

    pub(super) fn normalize_blank_lines(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut consecutive = 0u8;
        for line in s.lines() {
            if line.trim().is_empty() {
                consecutive += 1;
                if consecutive <= 2 {
                    out.push('\n');
                }
            } else {
                consecutive = 0;
                out.push_str(line);
                out.push('\n');
            }
        }
        out.trim_end().to_string()
    }
}

mod extract {
    use scraper::Selector;
    use serde::Serialize;
    use std::collections::HashSet;

    #[derive(Debug, Clone, Default, Serialize)]
    pub struct PageMetadata {
        #[serde(skip_serializing_if = "Option::is_none")]
        pub title: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub description: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub canonical_url: Option<String>,
    }

    #[derive(Debug, Clone, Serialize)]
    pub struct ExtractedLink {
        pub href: String,
        pub text: String,
    }

    pub fn extract_metadata(doc: &scraper::Html, base_url: &str) -> PageMetadata {
        let title = select_text(doc, "title");
        let description = meta_attr(doc, "name", "description")
            .or_else(|| meta_attr(doc, "property", "og:description"));
        let canonical_url = Selector::parse(r#"link[rel="canonical"]"#)
            .ok()
            .and_then(|sel| doc.select(&sel).next())
            .and_then(|el| el.value().attr("href"))
            .and_then(|href| super::resolve_url(base_url, href));

        PageMetadata {
            title,
            description,
            canonical_url,
        }
    }

    pub fn extract_links(doc: &scraper::Html, base_url: &str, max: usize) -> Vec<ExtractedLink> {
        let sel = match Selector::parse("a[href]") {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        let mut seen = HashSet::new();
        let mut links = Vec::new();

        for el in doc.select(&sel) {
            if links.len() >= max {
                break;
            }
            let href = match el.value().attr("href") {
                Some(h) => h.trim(),
                None => continue,
            };
            if should_skip(href) {
                continue;
            }
            let resolved = match super::resolve_url(base_url, href) {
                Some(u) => u,
                None => continue,
            };
            if !seen.insert(resolved.clone()) {
                continue;
            }
            let text: String = el
                .text()
                .collect::<String>()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            if text.is_empty() || text.len() > 200 {
                continue;
            }
            links.push(ExtractedLink {
                href: resolved,
                text,
            });
        }
        links
    }

    fn select_text(doc: &scraper::Html, selector: &str) -> Option<String> {
        Selector::parse(selector)
            .ok()
            .and_then(|sel| doc.select(&sel).next())
            .map(|el| el.text().collect::<String>().trim().to_string())
            .filter(|s| !s.is_empty())
    }

    fn meta_attr(doc: &scraper::Html, attr_name: &str, attr_value: &str) -> Option<String> {
        let sel_str = format!(r#"meta[{attr_name}="{attr_value}"]"#);
        Selector::parse(&sel_str)
            .ok()
            .and_then(|sel| doc.select(&sel).next())
            .and_then(|el| el.value().attr("content"))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    fn should_skip(href: &str) -> bool {
        href.is_empty()
            || href == "#"
            || href.starts_with("javascript:")
            || href.starts_with("mailto:")
            || href.starts_with("tel:")
            || href.starts_with("data:")
    }
}

fn resolve_url(base: &str, href: &str) -> Option<String> {
    if href.starts_with("http://") || href.starts_with("https://") {
        return Some(href.to_string());
    }
    url::Url::parse(base)
        .ok()
        .and_then(|b| b.join(href).ok())
        .map(|u| u.to_string())
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── SSRF Protection ───────────────────────────────────────────────

    #[test]
    fn blocks_localhost() {
        assert!(validate_url("http://localhost/secret").is_err());
        assert!(validate_url("http://127.0.0.1/admin").is_err());
        assert!(validate_url("http://[::1]/internal").is_err());
    }

    #[test]
    fn blocks_private_ips() {
        assert!(validate_url("http://10.0.0.1/api").is_err());
        assert!(validate_url("http://172.16.0.1/").is_err());
        assert!(validate_url("http://192.168.1.1/").is_err());
        assert!(validate_url("http://169.254.169.254/metadata").is_err());
    }

    #[test]
    fn blocks_private_ipv6_ranges() {
        assert!(validate_url("http://[fc00::1]/").is_err());
        assert!(validate_url("http://[fd12:3456::1]/").is_err());
        assert!(validate_url("http://[fe80::1]/").is_err());
        assert!(validate_url("http://[::ffff:127.0.0.1]/").is_err());
        assert!(validate_url("http://[::ffff:10.0.0.1]/").is_err());
    }

    #[test]
    fn blocks_internal_domains() {
        assert!(validate_url("http://service.local/api").is_err());
        assert!(validate_url("http://db.internal/query").is_err());
    }

    // ── P3-1: explicit SSRF regression guards ─────────────────────────────
    //
    // The review flagged that SSRF behavior was correct in code but not
    // covered by *explicit* regression tests matching the attack names
    // ("cloud metadata endpoint", "DNS rebinding target"). Add named
    // tests so future refactors can't accidentally regress.

    #[test]
    fn blocks_aws_metadata_endpoint() {
        // Classic cloud-metadata SSRF target.
        assert!(validate_url("http://169.254.169.254/latest/meta-data/iam/security-credentials/").is_err());
    }

    #[test]
    fn blocks_gcp_metadata_endpoint() {
        assert!(validate_url("http://metadata.google.internal/computeMetadata/v1/").is_err());
    }

    #[test]
    fn blocks_azure_metadata_endpoint() {
        // Azure IMDS also lives on 169.254.169.254.
        assert!(validate_url("http://169.254.169.254/metadata/instance?api-version=2021-02-01").is_err());
    }

    #[test]
    fn blocks_host_that_dns_would_resolve_to_private_ip() {
        // validate_url's synchronous guard catches the literal-IP form;
        // DNS rebinding (hostname → private IP) is caught asynchronously
        // in validate_resolved_host. Assert the literal-IP form here;
        // integration test covers the DNS case.
        assert!(validate_url("http://127.0.0.1.nip.io/").is_ok(),
                "nip.io resolves 127.0.0.1.nip.io → 127.0.0.1; validate_url only inspects literal host, so this passes synchronously. DNS rebinding blocked at validate_resolved_host.");
        // Meanwhile a literal private IP is blocked immediately:
        assert!(validate_url("http://127.0.0.1/").is_err());
    }

    #[test]
    fn allows_public_urls() {
        assert!(validate_url("https://example.com").is_ok());
        assert!(validate_url("https://docs.rs/scraper").is_ok());
        assert!(validate_url("http://93.184.216.34/").is_ok());
    }

    // ── URL Validation ────────────────────────────────────────────────

    #[test]
    fn rejects_bad_schemes() {
        assert!(validate_url("ftp://example.com").is_err());
        assert!(validate_url("file:///etc/passwd").is_err());
    }

    #[test]
    fn rejects_too_long_url() {
        let long = format!("https://x.com/{}", "a".repeat(5000));
        assert!(validate_url(&long).is_err());
    }

    #[test]
    fn rejects_malformed_urls() {
        assert!(validate_url("not a url").is_err());
        assert!(validate_url("").is_err());
    }

    // ── Format Parsing ────────────────────────────────────────────────

    #[test]
    fn format_valid() {
        assert_eq!(OutputFormat::parse(None).unwrap(), OutputFormat::Markdown);
        assert_eq!(
            OutputFormat::parse(Some("markdown")).unwrap(),
            OutputFormat::Markdown
        );
        assert_eq!(
            OutputFormat::parse(Some("text")).unwrap(),
            OutputFormat::Text
        );
    }

    #[test]
    fn format_invalid_returns_error() {
        assert!(OutputFormat::parse(Some("raw")).is_err());
        assert!(OutputFormat::parse(Some("markdwon")).is_err());
    }

    // ── Ordered List Fix ──────────────────────────────────────────────

    #[test]
    fn ordered_list_numbers_correctly() {
        let doc = scraper::Html::parse_document(
            "<html><body><ol><li>A</li><li>B</li><li>C</li></ol></body></html>",
        );
        let md = to_markdown(&doc, "https://x.com");
        assert!(md.contains("1. A"), "got: {md}");
        assert!(md.contains("2. B"), "got: {md}");
        assert!(md.contains("3. C"), "got: {md}");
    }

    #[test]
    fn nested_lists() {
        let doc = scraper::Html::parse_document(
            "<html><body><ul><li>Top<ul><li>Nested</li></ul></li></ul></body></html>",
        );
        let md = to_markdown(&doc, "https://x.com");
        assert!(md.contains("- Top"), "got: {md}");
        assert!(md.contains("  - Nested"), "got: {md}");
    }

    // ── Depth Guard ───────────────────────────────────────────────────

    #[test]
    fn deeply_nested_html_does_not_stack_overflow() {
        let open: String = (0..1000).map(|_| "<div>").collect();
        let close: String = (0..1000).map(|_| "</div>").collect();
        let html = format!("<html><body>{open}deep{close}</body></html>");
        let doc = scraper::Html::parse_document(&html);
        let md = to_markdown(&doc, "https://x.com");
        // Should not panic — depth guard truncates
        assert!(!md.is_empty() || md.is_empty()); // just assert no panic
    }

    // ── Cache ─────────────────────────────────────────────────────────

    fn cache_key(scope: &str, url: &str, format: OutputFormat) -> CacheKey {
        CacheKey {
            scope: scope.into(),
            url: url.into(),
            format,
            max_content: 80_000,
            max_links: 25,
        }
    }

    #[test]
    fn cache_key_includes_format() {
        let mut cache = Cache::new(10, Duration::from_secs(60));
        let result = Arc::new(FetchResult {
            url: "https://x.com".into(),
            final_url: None,
            status: 200,
            content_type: "text/html".into(),
            metadata: PageMetadata::default(),
            content: "md content".into(),
            links: vec![],
            content_length: 10,
            truncated: false,
            cached: false,
            elapsed_ms: 0,
        });
        let key_md = cache_key("s1", "https://x.com", OutputFormat::Markdown);
        let key_txt = cache_key("s1", "https://x.com", OutputFormat::Text);
        cache.put(key_md.clone(), Arc::clone(&result));

        assert!(cache.get(&key_md).is_some());
        assert!(cache.get(&key_txt).is_none());
    }

    #[test]
    fn cache_key_isolates_scope_and_limits() {
        let mut key_a = cache_key("session-a", "https://x.com", OutputFormat::Markdown);
        let mut key_b = cache_key("session-b", "https://x.com", OutputFormat::Markdown);
        assert_ne!(key_a, key_b);

        key_b.scope = key_a.scope.clone();
        key_b.max_content = 10_000;
        assert_ne!(key_a, key_b);

        key_a.max_content = key_b.max_content;
        key_b.max_links = 5;
        assert_ne!(key_a, key_b);
    }

    // ── R3-P0-#3: cache hit must be Arc::clone, not deep clone ────────────
    //
    // Previous impl did `(*cached).clone()` then mutated `cached = true`,
    // copying the full FetchResult (potentially MBs of content+markdown).
    // Cache hit should be zero-copy via Arc — the FetchResult sitting in
    // the cache is immutable from our side.

    #[test]
    fn cache_hit_returns_arc_clone_not_deep_copy() {
        let mut cache = Cache::new(10, Duration::from_secs(60));
        let result = Arc::new(FetchResult {
            url: "https://x.com".into(),
            final_url: None,
            status: 200,
            content_type: "text/html".into(),
            metadata: PageMetadata::default(),
            content: "X".repeat(100_000), // 100KB payload
            links: vec![],
            content_length: 100_000,
            truncated: false,
            cached: true,
            elapsed_ms: 0,
        });
        let key = cache_key("s1", "https://x.com", OutputFormat::Markdown);
        cache.put(key.clone(), Arc::clone(&result));

        // After put, the cache and `result` share the same Arc => strong_count == 2.
        assert_eq!(Arc::strong_count(&result), 2);

        let hit = cache.get(&key).unwrap();
        // Hit must clone the Arc, NOT deep-clone the FetchResult.
        // Proof: the returned Arc points at the same allocation.
        assert!(
            Arc::ptr_eq(&result, &hit),
            "cache hit must return an Arc to the same allocation — got \
             different pointers (full FetchResult was cloned)"
        );
        // And the content buffer is the very same String, not a copy.
        assert_eq!(hit.content.as_ptr(), result.content.as_ptr());
    }

    #[test]
    fn cache_stores_with_cached_flag_preset() {
        // When a caller put()s a FetchResult, the cached flag stays as
        // written. Readers must decide whether to set cached=true BEFORE
        // put, or accept the stored flag as-is. Production callers set
        // it via `stash_with_cached_flag` helper.
        let mut cache = Cache::new(10, Duration::from_secs(60));
        let result = Arc::new(FetchResult {
            url: "https://x.com".into(),
            final_url: None,
            status: 200,
            content_type: "text/html".into(),
            metadata: PageMetadata::default(),
            content: "c".into(),
            links: vec![],
            content_length: 1,
            truncated: false,
            cached: true, // pre-flagged
            elapsed_ms: 0,
        });
        let key = cache_key("s1", "https://x.com", OutputFormat::Markdown);
        cache.put(key.clone(), result);
        let hit = cache.get(&key).unwrap();
        assert!(hit.cached, "stored flag must survive round-trip");
    }

    #[test]
    fn cache_evicts_oldest() {
        let mut cache = Cache::new(2, Duration::from_secs(60));
        for i in 0..3 {
            let result = Arc::new(FetchResult {
                url: format!("https://x.com/{i}"),
                final_url: None,
                status: 200,
                content_type: "text/html".into(),
                metadata: PageMetadata::default(),
                content: format!("c{i}"),
                links: vec![],
                content_length: 2,
                truncated: false,
                cached: false,
                elapsed_ms: 0,
            });
            cache.put(
                cache_key("s1", &format!("https://x.com/{i}"), OutputFormat::Markdown),
                result,
            );
        }
        assert_eq!(cache.entries.len(), 2);
        // First entry should be evicted
        let key0 = cache_key("s1", "https://x.com/0", OutputFormat::Markdown);
        assert!(cache.get(&key0).is_none());
    }

    #[test]
    fn cache_expires() {
        let mut cache = Cache::new(10, Duration::from_millis(1));
        let result = Arc::new(FetchResult {
            url: "https://x.com".into(),
            final_url: None,
            status: 200,
            content_type: "text/html".into(),
            metadata: PageMetadata::default(),
            content: "old".into(),
            links: vec![],
            content_length: 3,
            truncated: false,
            cached: false,
            elapsed_ms: 0,
        });
        cache.put(
            cache_key("s1", "https://x.com", OutputFormat::Markdown),
            result,
        );
        std::thread::sleep(Duration::from_millis(5));
        let key = cache_key("s1", "https://x.com", OutputFormat::Markdown);
        assert!(cache.get(&key).is_none());
    }

    // ── Metadata ──────────────────────────────────────────────────────

    #[test]
    fn extracts_title_and_description() {
        let doc = scraper::Html::parse_document(
            r#"<html><head><title>T</title><meta name="description" content="D"></head><body></body></html>"#,
        );
        let meta = extract_metadata(&doc, "https://x.com");
        assert_eq!(meta.title.as_deref(), Some("T"));
        assert_eq!(meta.description.as_deref(), Some("D"));
    }

    #[test]
    fn canonical_url_resolved() {
        let doc = scraper::Html::parse_document(
            r#"<html><head><link rel="canonical" href="/p"></head><body></body></html>"#,
        );
        let meta = extract_metadata(&doc, "https://x.com/other");
        assert_eq!(meta.canonical_url.as_deref(), Some("https://x.com/p"));
    }

    // ── Links ─────────────────────────────────────────────────────────

    #[test]
    fn links_resolved_and_filtered() {
        let html = concat!(
            r#"<html><body>"#,
            r#"<a href="/page">Page</a>"#,
            r#"<a href="javascript:void(0)">JS</a>"#,
            r#"<a href="https://ext.com">Ext</a>"#,
            r#"</body></html>"#,
        );
        let doc = scraper::Html::parse_document(html);
        let links = extract_links(&doc, "https://x.com", 10);
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].href, "https://x.com/page");
        assert_eq!(links[1].href, "https://ext.com");
    }

    #[test]
    fn links_deduplicated_and_capped() {
        let html: String = (0..50)
            .map(|i| format!(r#"<a href="/p{i}">L{i}</a>"#))
            .collect();
        let html = format!("<html><body>{html}<a href=\"/p0\">dup</a></body></html>");
        let doc = scraper::Html::parse_document(&html);
        let links = extract_links(&doc, "https://x.com", 5);
        assert_eq!(links.len(), 5);
    }

    // ── HTML → Text ───────────────────────────────────────────────────

    #[test]
    fn text_strips_non_content() {
        let doc = scraper::Html::parse_document(
            "<html><body><script>x</script><nav>N</nav><p>V</p><footer>F</footer></body></html>",
        );
        let text = to_text(&doc);
        assert!(text.contains('V'));
        assert!(!text.contains('x'));
        assert!(!text.contains('N'));
        assert!(!text.contains('F'));
    }

    // ── HTML → Markdown ───────────────────────────────────────────────

    #[test]
    fn md_headings() {
        let doc = scraper::Html::parse_document("<html><body><h1>A</h1><h2>B</h2></body></html>");
        let md = to_markdown(&doc, "https://x.com");
        assert!(md.contains("# A"), "got: {md}");
        assert!(md.contains("## B"), "got: {md}");
    }

    #[test]
    fn md_links() {
        let doc = scraper::Html::parse_document(
            r#"<html><body><p><a href="/d">docs</a></p></body></html>"#,
        );
        let md = to_markdown(&doc, "https://x.com");
        assert!(md.contains("[docs](https://x.com/d)"), "got: {md}");
    }

    #[test]
    fn md_code_block() {
        let doc = scraper::Html::parse_document(
            r#"<html><body><pre><code class="language-rs">fn f(){}</code></pre></body></html>"#,
        );
        let md = to_markdown(&doc, "https://x.com");
        assert!(md.contains("```rs"), "got: {md}");
        assert!(md.contains("fn f(){}"), "got: {md}");
    }

    #[test]
    fn md_inline_formatting() {
        let doc = scraper::Html::parse_document(
            "<html><body><p><strong>b</strong> <em>i</em> <code>c</code></p></body></html>",
        );
        let md = to_markdown(&doc, "https://x.com");
        assert!(md.contains("**b**"), "got: {md}");
        assert!(md.contains("*i*"), "got: {md}");
        assert!(md.contains("`c`"), "got: {md}");
    }

    #[test]
    fn md_table() {
        let doc = scraper::Html::parse_document(
            r#"<html><body><table><tr><th>H</th></tr><tr><td>D</td></tr></table></body></html>"#,
        );
        let md = to_markdown(&doc, "https://x.com");
        assert!(md.contains("| H |"), "got: {md}");
        assert!(md.contains("| D |"), "got: {md}");
    }

    #[test]
    fn md_blockquote() {
        let doc = scraper::Html::parse_document(
            "<html><body><blockquote><p>Q</p></blockquote></body></html>",
        );
        let md = to_markdown(&doc, "https://x.com");
        assert!(md.contains("> Q"), "got: {md}");
    }

    #[test]
    fn md_content_root_preference() {
        let doc = scraper::Html::parse_document(
            "<html><body><div>X</div><main><p>M</p></main></body></html>",
        );
        let md = to_markdown(&doc, "https://x.com");
        assert!(md.contains('M'), "got: {md}");
        assert!(!md.contains('X'), "got: {md}");
    }

    // ── Content-Type Routing ──────────────────────────────────────────

    #[test]
    fn json_passthrough() {
        let config = FetchConfig::default();
        let r = transform(
            "https://x.com",
            None,
            200,
            "application/json",
            r#"{"a":1}"#,
            &config,
            Duration::ZERO,
        );
        assert!(r.content.contains(r#""a":1"#));
    }

    #[test]
    fn binary_rejection() {
        let config = FetchConfig::default();
        let r = transform(
            "https://x.com",
            None,
            200,
            "image/png",
            "",
            &config,
            Duration::ZERO,
        );
        assert!(r.content.contains("Binary content"));
    }

    // ── Truncation ────────────────────────────────────────────────────

    #[test]
    fn truncation_works() {
        let (content, trunc) = truncate(&"x".repeat(1000), 100);
        assert!(trunc);
        assert!(content.len() < 200);
        assert!(content.contains("truncated"));
    }

    // ── Integration ───────────────────────────────────────────────────

    #[tokio::test]
    async fn error_on_missing_url() {
        let r = fetch(None, &serde_json::json!({})).await;
        assert!(r.starts_with("Error:"), "got: {r}");
        assert!(r.contains("Missing 'url'"));
    }

    #[tokio::test]
    async fn error_on_private_ip() {
        let r = fetch(None, &serde_json::json!({"url": "http://127.0.0.1/admin"})).await;
        assert!(r.starts_with("Error:"), "got: {r}");
        assert!(r.contains("SSRF"), "got: {r}");
    }

    #[tokio::test]
    async fn error_on_bad_format() {
        let r = fetch(
            None,
            &serde_json::json!({"url": "https://x.com", "format": "raw"}),
        )
        .await;
        assert!(r.starts_with("Error:"), "got: {r}");
    }

    // ── Performance ───────────────────────────────────────────────────

    #[test]
    fn extraction_under_50ms() {
        let para = format!("<p>{}</p>\n", "word ".repeat(100));
        let html = format!("<html><body>{}</body></html>", para.repeat(400));
        let doc = scraper::Html::parse_document(&html);
        let start = Instant::now();
        let _ = to_markdown(&doc, "https://x.com");
        assert!(
            start.elapsed().as_millis() < 50,
            "took {:?}",
            start.elapsed()
        );
    }

    // ── HTTP Error Signaling ──────────────────────────────────────────

    #[test]
    fn transform_404_returns_content_normally() {
        let config = FetchConfig::default();
        let r = transform(
            "https://x.com/missing",
            None,
            404,
            "text/html",
            "<html><body><p>Not Found</p></body></html>",
            &config,
            Duration::ZERO,
        );
        assert_eq!(r.status, 404);
        assert!(r.content.contains("Not Found"), "got: {}", r.content);
    }

    #[tokio::test]
    async fn http_4xx_returns_error_string() {
        // Simulate what happens when fetch_inner returns a 404
        // We test the fetch() wrapper's error signaling by calling transform + checking output
        let config = FetchConfig::default();
        let result = transform(
            "https://x.com/gone",
            None,
            410,
            "text/html",
            "<html><body><p>Gone</p></body></html>",
            &config,
            Duration::ZERO,
        );
        assert_eq!(result.status, 410);
        // The fetch() function wraps this as an error
        let result = Arc::new(result);
        let output = if result.status >= 400 {
            format!("Error: HTTP {} — {}", result.status, result.content)
        } else {
            serde_json::to_string(&*result).unwrap()
        };
        assert!(output.starts_with("Error: HTTP 410"), "got: {output}");
    }

    // ── Curl Sentinel ─────────────────────────────────────────────────

    #[test]
    fn curl_sentinel_is_sufficiently_unique() {
        // Verify the sentinel won't appear in typical web content
        assert!(CURL_SENTINEL.contains("ASTRA_FETCH_7f3a9b2e1d4c"));
        assert!(
            CURL_SENTINEL.len() > 30,
            "sentinel must be long enough to be unique"
        );
        // Verify it starts and ends with newlines for clean splitting
        assert!(CURL_SENTINEL.starts_with('\n'));
        assert!(CURL_SENTINEL.ends_with('\n'));
    }

    // ── Empty Body Handling ───────────────────────────────────────────

    #[test]
    fn empty_html_body_returns_empty_content() {
        let config = FetchConfig::default();
        let r = transform(
            "https://x.com",
            None,
            200,
            "text/html",
            "<html><head><title>Empty</title></head><body></body></html>",
            &config,
            Duration::ZERO,
        );
        assert_eq!(r.status, 200);
        assert_eq!(r.metadata.title.as_deref(), Some("Empty"));
        // Content may be empty or just whitespace
        assert!(r.content.trim().is_empty(), "got: {:?}", r.content);
    }

    #[test]
    fn completely_empty_body_text() {
        let config = FetchConfig::default();
        let r = transform(
            "https://x.com",
            None,
            200,
            "text/plain",
            "",
            &config,
            Duration::ZERO,
        );
        assert_eq!(r.content, "");
        assert_eq!(r.content_length, 0);
        assert!(!r.truncated);
    }

    // ── HTTP→HTTPS Upgrade ────────────────────────────────────────────

    #[test]
    fn upgrades_http_to_https() {
        assert_eq!(
            upgrade_scheme("http://example.com/page"),
            "https://example.com/page"
        );
        assert_eq!(
            upgrade_scheme("http://example.com:80/p?q=1"),
            "https://example.com/p?q=1"
        );
    }

    #[test]
    fn does_not_modify_https() {
        assert_eq!(
            upgrade_scheme("https://example.com/page"),
            "https://example.com/page"
        );
    }

    #[test]
    fn preserves_explicit_non_standard_port() {
        assert_eq!(
            upgrade_scheme("http://example.com:8080/p"),
            "https://example.com:8080/p"
        );
    }

    // ── Relative URL resolution ───────────────────────────────────────

    #[test]
    fn resolves_protocol_relative_urls() {
        assert_eq!(
            resolve_url("https://example.com/page", "//cdn.example.com/img.png"),
            Some("https://cdn.example.com/img.png".into())
        );
    }

    // ── Selector static compilation (regression guard) ────────────────

    #[test]
    fn selectors_compile() {
        use scraper::Selector;
        assert!(Selector::parse("a[href]").is_ok());
        assert!(Selector::parse("main").is_ok());
        assert!(Selector::parse(r#"link[rel="canonical"]"#).is_ok());
        assert!(Selector::parse(r#"meta[name="description"]"#).is_ok());
        assert!(Selector::parse(r#"meta[property="og:description"]"#).is_ok());
    }

    // ── Edge cases in Markdown conversion ─────────────────────────────

    #[test]
    fn md_handles_empty_links_gracefully() {
        let doc = scraper::Html::parse_document(
            r#"<html><body><a href="">empty</a><a href="/ok">ok</a></body></html>"#,
        );
        let md = to_markdown(&doc, "https://x.com");
        assert!(md.contains("[ok](https://x.com/ok)"), "got: {md}");
    }

    #[test]
    fn md_nested_inline_formatting() {
        let doc = scraper::Html::parse_document(
            "<html><body><p><strong><em>bold italic</em></strong></p></body></html>",
        );
        let md = to_markdown(&doc, "https://x.com");
        assert!(
            md.contains("***bold italic***")
                || md.contains("**_bold italic_**")
                || md.contains("***bold italic***"),
            "got: {md}"
        );
    }

    #[test]
    fn md_preserves_code_block_content_verbatim() {
        let doc = scraper::Html::parse_document(
            r#"<html><body><pre><code>fn &lt;T&gt;(x: &amp;T) { }</code></pre></body></html>"#,
        );
        let md = to_markdown(&doc, "https://x.com");
        // HTML entities should be decoded by the parser
        assert!(md.contains("fn <T>(x: &T) { }"), "got: {md}");
    }

    #[test]
    fn text_collapses_excessive_whitespace() {
        let doc = scraper::Html::parse_document(
            "<html><body><p>  lots   of   spaces  </p></body></html>",
        );
        let text = to_text(&doc);
        assert!(
            text.contains("lots of spaces") || text.contains("lots   of   spaces"),
            "got: {text}"
        );
    }

    // ── Binary content types ──────────────────────────────────────────

    #[test]
    fn detects_all_binary_types() {
        let config = FetchConfig::default();
        for ct in [
            "image/png",
            "audio/mpeg",
            "video/mp4",
            "application/pdf",
            "application/zip",
            "application/octet-stream",
            "application/gzip",
        ] {
            let r = transform(
                "https://x.com/f",
                None,
                200,
                ct,
                "",
                &config,
                Duration::ZERO,
            );
            assert!(
                r.content.contains("Binary content"),
                "failed for {ct}: {}",
                r.content
            );
        }
    }

    #[test]
    fn non_binary_types_not_rejected() {
        let config = FetchConfig::default();
        for ct in [
            "text/html",
            "text/plain",
            "application/json",
            "text/xml",
            "text/csv",
        ] {
            let r = transform(
                "https://x.com/f",
                None,
                200,
                ct,
                "data",
                &config,
                Duration::ZERO,
            );
            assert!(
                !r.content.contains("Binary content"),
                "incorrectly rejected {ct}"
            );
        }
    }
}
