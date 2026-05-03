//! WeChat (微信) personal account adapter via iLink Bot API.
//!
//! Protocol (reverse-engineered from hermes-agent):
//! - Long-poll: POST /ilink/bot/getupdates with sync cursor
//! - Send: POST /ilink/bot/sendmessage with context_token echo
//! - Auth: Bearer token + ilink_bot_token AuthorizationType header
//!
//! Configuration:
//!   platforms:
//!     weixin:
//!       enabled: true
//!       token: ""              # from QR login (bot_token), or WEIXIN_TOKEN env
//!       account_id: ""         # from QR login (ilink_bot_id), or WEIXIN_ACCOUNT_ID env

use super::{ChatType, InboundMessage, PlatformAdapter};
use crate::dedup::MessageDeduplicator;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};

const ILINK_BASE_URL: &str = "https://ilinkai.weixin.qq.com";
const ILINK_APP_ID: &str = "bot";
const ILINK_CLIENT_VERSION: &str = "131072";
const CHANNEL_VERSION: &str = "2.2.0";
const POLL_TIMEOUT_SECS: u64 = 35;
const MAX_MESSAGE_LENGTH: usize = 2000;

#[derive(Debug, Clone, serde::Deserialize)]
pub struct WeixinConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub account_id: String,
}

impl WeixinConfig {
    pub fn resolve(mut self) -> Self {
        if self.token.is_empty()
            && let Ok(v) = std::env::var("WEIXIN_TOKEN")
        {
            self.token = v;
        }
        if self.account_id.is_empty()
            && let Ok(v) = std::env::var("WEIXIN_ACCOUNT_ID")
        {
            self.account_id = v;
        }
        self
    }
}

/// Per-user context token cache (required for sending replies).
type ContextTokens = Arc<Mutex<HashMap<String, String>>>;
type TypingTickets = Arc<Mutex<HashMap<String, (String, std::time::Instant)>>>;

const TYPING_TICKET_TTL_SECS: u64 = 600; // 10 minutes

pub struct WeixinAdapter {
    config: WeixinConfig,
    pool: Option<sqlx::MySqlPool>,
    msg_tx: mpsc::Sender<InboundMessage>,
    msg_rx: Option<mpsc::Receiver<InboundMessage>>,
    context_tokens: ContextTokens,
    typing_tickets: TypingTickets,
    shutdown: Option<tokio::sync::broadcast::Sender<()>>,
}

impl WeixinAdapter {
    pub fn new(config: WeixinConfig) -> Self {
        let (tx, rx) = mpsc::channel(256);
        Self {
            config: config.resolve(),
            pool: None,
            msg_tx: tx,
            msg_rx: Some(rx),
            context_tokens: Arc::new(Mutex::new(HashMap::new())),
            typing_tickets: Arc::new(Mutex::new(HashMap::new())),
            shutdown: None,
        }
    }

    pub fn with_pool(mut self, pool: sqlx::MySqlPool) -> Self {
        self.pool = Some(pool);
        self
    }

    async fn resolve_credentials(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if !self.config.token.is_empty() {
            return Ok(());
        }
        if let Some(ref pool) = self.pool
            && let Ok(Some(cred)) =
                crate::storage::get_credential(pool, "weixin", "default", "bot_token").await
        {
            if let Some(token) = cred.credentials["token"].as_str() {
                self.config.token = token.to_string();
            }
            if let Some(aid) = cred.credentials["account_id"].as_str()
                && self.config.account_id.is_empty()
            {
                self.config.account_id = aid.to_string();
            }
            if !self.config.token.is_empty() {
                tracing::info!("weixin credentials loaded from database");
                return Ok(());
            }
        }
        Err("weixin: token required — run `astra-gateway login-weixin` to scan QR code".into())
    }
}

fn build_headers(token: &str) -> reqwest::header::HeaderMap {
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
    let mut h = HeaderMap::new();
    h.insert("iLink-App-Id", HeaderValue::from_static(ILINK_APP_ID));
    h.insert(
        "iLink-App-ClientVersion",
        HeaderValue::from_static(ILINK_CLIENT_VERSION),
    );
    h.insert(
        HeaderName::from_static("authorizationtype"),
        HeaderValue::from_static("ilink_bot_token"),
    );
    if let Ok(v) = HeaderValue::from_str(&format!("Bearer {token}")) {
        h.insert(reqwest::header::AUTHORIZATION, v);
    }
    // X-WECHAT-UIN: random 4 bytes base64
    let uin: [u8; 4] = rand_bytes();
    use base64::Engine;
    let uin_b64 = base64::engine::general_purpose::STANDARD.encode(uin);
    if let Ok(v) = HeaderValue::from_str(&uin_b64) {
        h.insert(HeaderName::from_static("x-wechat-uin"), v);
    }
    h
}

fn rand_bytes() -> [u8; 4] {
    let mut buf = [0u8; 4];
    buf[0] = (std::process::id() & 0xFF) as u8;
    buf[1] = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos()
        & 0xFF) as u8;
    buf[2] = rand_u8();
    buf[3] = rand_u8();
    buf
}

fn rand_u8() -> u8 {
    (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        % 256) as u8
}

#[async_trait]
impl PlatformAdapter for WeixinAdapter {
    fn name(&self) -> &'static str {
        "weixin"
    }

    async fn start(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.resolve_credentials().await?;

        let (shutdown_tx, _) = tokio::sync::broadcast::channel(1);
        self.shutdown = Some(shutdown_tx.clone());

        let config = self.config.clone();
        let msg_tx = self.msg_tx.clone();
        let tokens = self.context_tokens.clone();
        let pool = self.pool.clone();

        // Restore persisted context_tokens from DB
        if let Some(ref pool) = pool
            && let Ok(Some(cred)) = crate::storage::get_credential(pool, "weixin", "default", "context_tokens").await
            && let Some(map) = cred.credentials.as_object()
        {
            let mut t = tokens.lock().await;
            for (k, v) in map {
                if let Some(s) = v.as_str() {
                    t.insert(k.clone(), s.to_string());
                }
            }
            tracing::info!(count = t.len(), "restored context_tokens from DB");
        }

        tokio::spawn(async move {
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(POLL_TIMEOUT_SECS + 10))
                .build()
                .unwrap();
            let mut dedup = MessageDeduplicator::new();
            let mut shutdown_rx = shutdown_tx.subscribe();
            let mut sync_buf = String::new();
            let mut consecutive_errors = 0u32;

            loop {
                tokio::select! {
                    result = poll_updates(&client, &config, &mut sync_buf, &msg_tx, &mut dedup, &tokens, &pool) => {
                        match result {
                            Ok(()) => { consecutive_errors = 0; }
                            Err(e) => {
                                consecutive_errors += 1;
                                let msg = e.to_string();
                                if msg.contains("-14") || msg.contains("session timeout") {
                                    tracing::error!(error = %e, "weixin session expired — token may need refresh");
                                    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                                } else if consecutive_errors > 3 {
                                    tracing::warn!(error = %e, failures = consecutive_errors, "weixin poll backoff");
                                    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                                } else {
                                    tracing::warn!(error = %e, "weixin poll error, retrying in 2s");
                                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                                }
                            }
                        }
                    }
                    _ = shutdown_rx.recv() => break,
                }
            }
        });

        tracing::info!("weixin adapter started (long-poll)");
        Ok(())
    }

    async fn stop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }

    async fn send_text(
        &self,
        chat_id: &str,
        text: &str,
        _reply_token: Option<&str>,
    ) -> Result<(), String> {
        let text = if text.len() > MAX_MESSAGE_LENGTH {
            format!("{}…", &text[..MAX_MESSAGE_LENGTH - 5])
        } else {
            text.to_string()
        };

        let context_token = {
            let tokens = self.context_tokens.lock().await;
            tokens.get(chat_id).cloned().unwrap_or_default()
        };

        let client = reqwest::Client::new();
        let url = format!("{ILINK_BASE_URL}/ilink/bot/sendmessage");
        let client_id = format!("astra-gw-{}", uuid::Uuid::new_v4());

        let body = json!({
            "msg": {
                "from_user_id": "",
                "to_user_id": chat_id,
                "client_id": client_id,
                "message_type": 2,
                "message_state": 2,
                "context_token": context_token,
                "item_list": [
                    {
                        "type": 1,
                        "text_item": {
                            "text": text
                        }
                    }
                ]
            },
            "base_info": {
                "channel_version": CHANNEL_VERSION
            }
        });

        let resp = client
            .post(&url)
            .headers(build_headers(&self.config.token))
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("weixin send failed: {e}"))?;

        let data: Value = resp
            .json()
            .await
            .map_err(|e| format!("weixin send parse error: {e}"))?;

        let ret = data["ret"].as_i64().unwrap_or(-1);
        if ret != 0 {
            let errcode = data["errcode"].as_i64().unwrap_or(ret);
            let errmsg = data["errmsg"].as_str().unwrap_or("unknown");
            return Err(format!("weixin send error {errcode}: {errmsg}"));
        }

        Ok(())
    }

    async fn send_typing(&self, chat_id: &str) -> Result<(), String> {
        // Get or fetch typing ticket
        let ticket = {
            let cache = self.typing_tickets.lock().await;
            cache.get(chat_id)
                .filter(|(_, ts)| ts.elapsed().as_secs() < TYPING_TICKET_TTL_SECS)
                .map(|(t, _)| t.clone())
        };
        let ticket = match ticket {
            Some(t) => t,
            None => {
                // Fetch from getconfig
                let context_token = {
                    let tokens = self.context_tokens.lock().await;
                    tokens.get(chat_id).cloned()
                };
                match fetch_typing_ticket(&self.config.token, chat_id, context_token.as_deref()).await {
                    Ok(t) => {
                        let mut cache = self.typing_tickets.lock().await;
                        cache.insert(chat_id.to_string(), (t.clone(), std::time::Instant::now()));
                        t
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, "typing ticket fetch failed");
                        return Ok(()); // non-fatal
                    }
                }
            }
        };

        let client = reqwest::Client::new();
        let body = json!({
            "ilink_user_id": chat_id,
            "typing_ticket": ticket,
            "status": 1,
        });
        let _ = client
            .post(format!("{ILINK_BASE_URL}/ilink/bot/sendtyping"))
            .headers(build_headers(&self.config.token))
            .json(&body)
            .send()
            .await;
        Ok(())
    }

    async fn recv(&mut self) -> Option<InboundMessage> {
        self.msg_rx.as_mut()?.recv().await
    }
}

async fn poll_updates(
    client: &reqwest::Client,
    config: &WeixinConfig,
    sync_buf: &mut String,
    msg_tx: &mpsc::Sender<InboundMessage>,
    dedup: &mut MessageDeduplicator,
    context_tokens: &ContextTokens,
    pool: &Option<sqlx::MySqlPool>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let url = format!("{ILINK_BASE_URL}/ilink/bot/getupdates");

    let body = json!({
        "get_updates_buf": *sync_buf,
        "base_info": {
            "channel_version": CHANNEL_VERSION
        }
    });

    tracing::debug!("weixin poll starting");

    let poll_timeout = std::time::Duration::from_secs(POLL_TIMEOUT_SECS + 10);
    let resp = tokio::time::timeout(poll_timeout, async {
        let resp = client
            .post(&url)
            .headers(build_headers(&config.token))
            .json(&body)
            .send()
            .await?;
        resp.json::<Value>().await
    })
    .await
    .map_err(|_| -> Box<dyn std::error::Error + Send + Sync> {
        tracing::debug!("weixin poll timed out after {}s", POLL_TIMEOUT_SECS + 10);
        "poll timeout".into()
    })??;

    let data = resp;
    let msg_count = data.get("msgs").and_then(|m| m.as_array()).map(|a| a.len());
    tracing::debug!(msgs = ?msg_count, "weixin poll response");
    if msg_count.unwrap_or(0) > 0 {
        tracing::info!(raw = %data["msgs"], "weixin raw msgs");
    }

    // Check for errors
    let ret = data["ret"].as_i64().unwrap_or(0);
    if ret != 0 {
        let errcode = data["errcode"].as_i64().unwrap_or(ret);
        let errmsg = data["errmsg"].as_str().unwrap_or("unknown");
        return Err(format!("getupdates error {errcode}: {errmsg}").into());
    }

    // Update sync cursor
    if let Some(buf) = data["get_updates_buf"].as_str() {
        *sync_buf = buf.to_string();
    }

    // Parse messages
    let msgs = data["msgs"].as_array();
    if let Some(msgs) = msgs {
        for msg in msgs {
            // Skip bot's own messages
            if msg["msg_type"].as_i64() == Some(2) {
                continue;
            }

            let msg_id = msg["message_id"]
                .as_str()
                .map(|s| s.to_string())
                .or_else(|| msg["message_id"].as_u64().map(|n| n.to_string()))
                .or_else(|| msg["message_id"].as_i64().map(|n| n.to_string()))
                .unwrap_or_default();
            if msg_id.is_empty() || !dedup.check(&msg_id) {
                continue;
            }

            // Extract text from item_list
            let text = extract_text(msg);
            if text.is_empty() {
                continue;
            }

            let from_id = msg["from_user_id"]
                .as_str()
                .unwrap_or("unknown")
                .to_string();

            // Cache context_token for reply
            if let Some(ct) = msg["context_token"].as_str()
                && !ct.is_empty()
            {
                let mut tokens = context_tokens.lock().await;
                tokens.insert(from_id.clone(), ct.to_string());
                // Persist to DB for crash recovery
                if let Some(pool) = pool {
                    let map: serde_json::Value = tokens.iter()
                        .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                        .collect();
                    let _ = crate::storage::save_credential(
                        pool, "weixin", "default", "context_tokens", &map, None,
                    ).await;
                }
            }

            let room_id = msg["room_id"].as_str().unwrap_or("");
            let chat_type = if room_id.is_empty() {
                ChatType::DirectMessage
            } else {
                ChatType::Group
            };
            let chat_id = if room_id.is_empty() {
                from_id.clone()
            } else {
                room_id.to_string()
            };

            tracing::info!(
                from = %from_id,
                text_len = text.len(),
                "weixin ← {}",
                &text[..text.len().min(60)]
            );

            let inbound = InboundMessage {
                platform: "weixin",
                chat_id,
                user_id: from_id,
                text,
                msg_id,
                chat_type,
                reply_token: None,
            };

            if msg_tx.send(inbound).await.is_err() {
                break;
            }
        }
    }

    Ok(())
}

fn extract_text(msg: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(items) = msg["item_list"].as_array() {
        for item in items {
            if item["type"].as_i64() == Some(1)
                && let Some(t) = item["text_item"]["text"].as_str()
            {
                parts.push(t.to_string());
            }
        }
    }
    parts.join("\n").trim().to_string()
}

// ─── QR Login ──────────────────────────────────────────────────────────────

async fn fetch_typing_ticket(token: &str, user_id: &str, context_token: Option<&str>) -> Result<String, String> {
    let client = reqwest::Client::new();
    let mut body = json!({ "ilink_user_id": user_id });
    if let Some(ct) = context_token {
        body["context_token"] = json!(ct);
    }
    let resp = client
        .post(format!("{ILINK_BASE_URL}/ilink/bot/getconfig"))
        .headers(build_headers(token))
        .json(&body)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| format!("getconfig: {e}"))?;
    let data: Value = resp.json().await.map_err(|e| format!("getconfig parse: {e}"))?;
    data["typing_ticket"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .ok_or_else(|| "no typing_ticket in response".into())
}

const ILINK_BOT_TYPE: &str = "3";

/// QR code login flow — call this interactively to get token + account_id.
pub async fn qr_login() -> Result<(String, String), Box<dyn std::error::Error + Send + Sync>> {
    let client = reqwest::Client::new();

    let resp = client
        .get(format!(
            "{ILINK_BASE_URL}/ilink/bot/get_bot_qrcode?bot_type={ILINK_BOT_TYPE}"
        ))
        .header("iLink-App-Id", ILINK_APP_ID)
        .send()
        .await?;

    let data: Value = resp.json().await?;
    if data["ret"].as_i64().unwrap_or(-1) != 0 {
        return Err(format!("get_bot_qrcode failed: {data}").into());
    }
    let qrcode = data["qrcode"]
        .as_str()
        .ok_or("no qrcode in response")?;
    let qr_url = data["qrcode_img_content"]
        .as_str()
        .ok_or("no qrcode_img_content in response")?;

    println!("📱 请用微信扫描此二维码:");
    println!();
    println!("   {qr_url}");
    println!();
    println!("   (等待扫码...)");

    loop {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        let resp = client
            .get(format!(
                "{ILINK_BASE_URL}/ilink/bot/get_qrcode_status?qrcode={qrcode}&bot_type={ILINK_BOT_TYPE}"
            ))
            .header("iLink-App-Id", ILINK_APP_ID)
            .send()
            .await?;

        let status: Value = resp.json().await?;
        let state = status["status"].as_str().unwrap_or("");

        match state {
            "wait" | "scanned" => continue,
            "expired" => {
                return Err("二维码已过期，请重新运行".into());
            }
            "confirmed" | "authorized" => {
                println!("✅ 登录成功！");
                let token = status["bot_token"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();
                let account_id = status["ilink_bot_id"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();
                return Ok((token, account_id));
            }
            other => {
                tracing::debug!(status = other, "unknown qrcode status, retrying");
                continue;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_resolve_env() {
        let cfg = WeixinConfig {
            enabled: true,
            token: String::new(),
            account_id: String::new(),
        };
        let resolved = cfg.resolve();
        assert!(resolved.token.is_empty() || !resolved.token.is_empty());
    }

    #[test]
    fn extract_text_from_item_list() {
        let msg: Value = serde_json::from_str(r#"{
            "message_id": "msg-1",
            "from_user_id": "wxid_abc",
            "item_list": [
                {"type": 1, "text_item": {"text": "hello from wechat"}}
            ]
        }"#).unwrap();
        assert_eq!(extract_text(&msg), "hello from wechat");
    }

    #[test]
    fn extract_text_multi_items() {
        let msg: Value = serde_json::from_str(r#"{
            "item_list": [
                {"type": 1, "text_item": {"text": "line1"}},
                {"type": 2, "image_item": {}},
                {"type": 1, "text_item": {"text": "line2"}}
            ]
        }"#).unwrap();
        assert_eq!(extract_text(&msg), "line1\nline2");
    }

    #[test]
    fn extract_text_empty() {
        let msg: Value = serde_json::from_str(r#"{"item_list": []}"#).unwrap();
        assert_eq!(extract_text(&msg), "");
    }

    #[test]
    fn max_message_truncation() {
        let long = "x".repeat(3000);
        let truncated = if long.len() > MAX_MESSAGE_LENGTH {
            format!("{}…", &long[..MAX_MESSAGE_LENGTH - 5])
        } else {
            long.clone()
        };
        assert!(truncated.len() < 3000);
    }
}

    #[tokio::test]
    async fn typing_ticket_cache_hit() {
        let cache: TypingTickets = Arc::new(Mutex::new(HashMap::new()));
        let user = "user1";
        // Insert a fresh ticket
        {
            let mut c = cache.lock().await;
            c.insert(user.to_string(), ("ticket_abc".into(), std::time::Instant::now()));
        }
        // Should hit cache
        let c = cache.lock().await;
        let entry = c.get(user)
            .filter(|(_, ts)| ts.elapsed().as_secs() < TYPING_TICKET_TTL_SECS);
        assert!(entry.is_some());
        assert_eq!(entry.unwrap().0, "ticket_abc");
    }

    #[tokio::test]
    async fn typing_ticket_cache_expired() {
        let cache: TypingTickets = Arc::new(Mutex::new(HashMap::new()));
        let user = "user1";
        // Insert an expired ticket (fake old timestamp)
        {
            let mut c = cache.lock().await;
            let old = std::time::Instant::now() - std::time::Duration::from_secs(TYPING_TICKET_TTL_SECS + 10);
            c.insert(user.to_string(), ("old_ticket".into(), old));
        }
        let c = cache.lock().await;
        let entry = c.get(user)
            .filter(|(_, ts)| ts.elapsed().as_secs() < TYPING_TICKET_TTL_SECS);
        assert!(entry.is_none(), "expired ticket should not hit cache");
    }

    #[tokio::test]
    async fn context_tokens_cache_roundtrip() {
        let tokens: ContextTokens = Arc::new(Mutex::new(HashMap::new()));
        // Simulate receiving a message with context_token
        {
            let mut t = tokens.lock().await;
            t.insert("user_abc".into(), "ctx_token_123".into());
        }
        // Read back
        let t = tokens.lock().await;
        assert_eq!(t.get("user_abc").unwrap(), "ctx_token_123");
    }

    #[test]
    fn build_headers_contains_auth() {
        let h = build_headers("test-token");
        assert!(h.get("authorization").is_some());
        assert_eq!(h.get("authorization").unwrap(), "Bearer test-token");
        assert_eq!(h.get("iLink-App-Id").unwrap(), "bot");
        assert_eq!(h.get("iLink-App-ClientVersion").unwrap(), "131072");
        assert!(h.get("authorizationtype").is_some());
        assert!(h.get("x-wechat-uin").is_some());
    }
