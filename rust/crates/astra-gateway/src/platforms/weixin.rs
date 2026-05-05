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

use super::{
    AdapterCapability, AdapterHealthEvent, AdapterHealthEventType, ChatType, InboundMessage,
    PlatformAdapter, emit_adapter_health,
};
use crate::dedup::MessageDeduplicator;
use crate::store::GatewayStore;
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
const MAX_RESTORED_TOKEN_LEN: usize = 8192;
const MAX_RESTORED_ID_LEN: usize = 512;
const MAX_RESTORED_SYNC_BUF_LEN: usize = 64 * 1024;
const MAX_RESTORED_CONTEXT_TOKENS: usize = 4096;
const WEIXIN_CAPABILITIES: &[AdapterCapability] = &[
    AdapterCapability::ReceiveText,
    AdapterCapability::SendText,
    AdapterCapability::SendTyping,
    AdapterCapability::LongPoll,
    AdapterCapability::PersistentState,
];

#[derive(Clone, serde::Deserialize)]
pub struct WeixinConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub account_id: String,
}

impl std::fmt::Debug for WeixinConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WeixinConfig")
            .field("enabled", &self.enabled)
            .field(
                "token",
                &if self.token.is_empty() {
                    "(empty)"
                } else {
                    "[REDACTED]"
                },
            )
            .field("account_id", &self.account_id)
            .finish()
    }
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
    store: Option<Arc<dyn GatewayStore>>,
    msg_tx: mpsc::Sender<InboundMessage>,
    msg_rx: Mutex<mpsc::Receiver<InboundMessage>>,
    context_tokens: ContextTokens,
    typing_tickets: TypingTickets,
    shutdown: Option<tokio::sync::broadcast::Sender<()>>,
}

impl WeixinAdapter {
    pub fn new(config: WeixinConfig) -> Self {
        let (tx, rx) = mpsc::channel(256);
        Self {
            config: config.resolve(),
            store: None,
            msg_tx: tx,
            msg_rx: Mutex::new(rx),
            context_tokens: Arc::new(Mutex::new(HashMap::new())),
            typing_tickets: Arc::new(Mutex::new(HashMap::new())),
            shutdown: None,
        }
    }

    pub fn with_store(mut self, store: Arc<dyn GatewayStore>) -> Self {
        self.store = Some(store);
        self
    }

    async fn resolve_credentials(
        &mut self,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if !self.config.token.is_empty() {
            validate_weixin_credentials(&self.config.token, &self.config.account_id)?;
            return Ok(());
        }
        if let Some(ref store) = self.store
            && let Ok(Some(cred)) = store.get_credential("weixin", "default", "bot_token").await
        {
            if let Some(token) = cred.credentials["token"].as_str() {
                if validate_restored_token(token) {
                    self.config.token = token.to_string();
                } else {
                    emit_adapter_health(AdapterHealthEvent::new(
                        "weixin",
                        AdapterHealthEventType::CredentialInvalid,
                        Some("stored bot_token token is invalid".to_string()),
                    ));
                }
            }
            if let Some(aid) = cred.credentials["account_id"].as_str()
                && self.config.account_id.is_empty()
            {
                if validate_restored_id(aid) {
                    self.config.account_id = aid.to_string();
                } else {
                    emit_adapter_health(AdapterHealthEvent::new(
                        "weixin",
                        AdapterHealthEventType::CredentialInvalid,
                        Some("stored account_id is invalid".to_string()),
                    ));
                }
            }
            if !self.config.token.is_empty() {
                validate_weixin_credentials(&self.config.token, &self.config.account_id)?;
                emit_adapter_health(AdapterHealthEvent::new(
                    "weixin",
                    AdapterHealthEventType::CredentialRestored,
                    Some("bot_token".to_string()),
                ));
                tracing::info!("weixin credentials loaded from database");
                return Ok(());
            }
        }
        Err("weixin: token required — run `astra-gateway login-weixin` to scan QR code".into())
    }
}

fn validate_weixin_credentials(
    token: &str,
    account_id: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if !validate_restored_token(token) {
        emit_adapter_health(AdapterHealthEvent::new(
            "weixin",
            AdapterHealthEventType::CredentialInvalid,
            Some("token is empty or cannot be used in an authorization header".to_string()),
        ));
        return Err("weixin: invalid token".into());
    }
    if !account_id.is_empty() && !validate_restored_id(account_id) {
        emit_adapter_health(AdapterHealthEvent::new(
            "weixin",
            AdapterHealthEventType::CredentialInvalid,
            Some("account_id contains invalid characters".to_string()),
        ));
        return Err("weixin: invalid account_id".into());
    }
    Ok(())
}

fn validate_restored_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= MAX_RESTORED_TOKEN_LEN
        && token.trim() == token
        && !token.chars().any(char::is_control)
        && reqwest::header::HeaderValue::from_str(&format!("Bearer {token}")).is_ok()
}

fn validate_restored_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_RESTORED_ID_LEN
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

fn restore_sync_buf_value(value: &Value) -> Option<String> {
    let Some(sync_buf) = value.as_str() else {
        emit_adapter_health(AdapterHealthEvent::new(
            "weixin",
            AdapterHealthEventType::StateInvalid,
            Some("sync_buf is not a string".to_string()),
        ));
        return None;
    };
    if sync_buf.len() > MAX_RESTORED_SYNC_BUF_LEN || sync_buf.chars().any(char::is_control) {
        emit_adapter_health(AdapterHealthEvent::new(
            "weixin",
            AdapterHealthEventType::StateInvalid,
            Some("sync_buf failed validation".to_string()),
        ));
        return None;
    }
    emit_adapter_health(AdapterHealthEvent::new(
        "weixin",
        AdapterHealthEventType::StateRestored,
        Some("sync_buf".to_string()),
    ));
    Some(sync_buf.to_string())
}

fn restore_context_tokens_value(value: &Value) -> HashMap<String, String> {
    let Some(map) = value.as_object() else {
        emit_adapter_health(AdapterHealthEvent::new(
            "weixin",
            AdapterHealthEventType::StateInvalid,
            Some("context_tokens is not an object".to_string()),
        ));
        return HashMap::new();
    };

    let mut restored = HashMap::new();
    let mut invalid = 0usize;
    for (key, value) in map.iter().take(MAX_RESTORED_CONTEXT_TOKENS) {
        let Some(token) = value.as_str() else {
            invalid += 1;
            continue;
        };
        if validate_restored_id(key) && validate_restored_token(token) {
            restored.insert(key.clone(), token.to_string());
        } else {
            invalid += 1;
        }
    }
    invalid += map.len().saturating_sub(MAX_RESTORED_CONTEXT_TOKENS);

    if !restored.is_empty() {
        emit_adapter_health(AdapterHealthEvent::new(
            "weixin",
            AdapterHealthEventType::StateRestored,
            Some(format!("context_tokens={}", restored.len())),
        ));
    }
    if invalid > 0 {
        emit_adapter_health(AdapterHealthEvent::new(
            "weixin",
            AdapterHealthEventType::StateInvalid,
            Some(format!("context_tokens_invalid={invalid}")),
        ));
    }
    restored
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

    fn capabilities(&self) -> &'static [AdapterCapability] {
        WEIXIN_CAPABILITIES
    }

    async fn start(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.resolve_credentials().await?;
        for capability in self.capabilities() {
            emit_adapter_health(AdapterHealthEvent::capability("weixin", *capability));
        }

        let (shutdown_tx, _) = tokio::sync::broadcast::channel(1);
        self.shutdown = Some(shutdown_tx.clone());

        let config = self.config.clone();
        let msg_tx = self.msg_tx.clone();
        let tokens = self.context_tokens.clone();
        let store = self.store.clone();

        // Restore persisted context_tokens from DB
        if let Some(ref store) = store
            && let Ok(Some(cred)) = store
                .get_credential("weixin", "default", "context_tokens")
                .await
        {
            let restored = restore_context_tokens_value(&cred.credentials);
            let mut t = tokens.lock().await;
            t.extend(restored);
            tracing::info!(count = t.len(), "restored context_tokens from DB");
        }

        tokio::spawn(async move {
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(POLL_TIMEOUT_SECS + 10))
                .build()
                .unwrap();
            let mut dedup = MessageDeduplicator::new();
            let mut shutdown_rx = shutdown_tx.subscribe();
            // Restore sync cursor from DB
            let mut sync_buf = if let Some(ref store) = store
                && let Ok(Some(cred)) = store.get_credential("weixin", "default", "sync_buf").await
                && let Some(s) = restore_sync_buf_value(&cred.credentials)
            {
                tracing::info!("restored sync cursor from DB");
                s
            } else {
                String::new()
            };
            let mut consecutive_errors = 0u32;

            loop {
                tokio::select! {
                    result = poll_updates(&client, &config, &mut sync_buf, &msg_tx, &mut dedup, &tokens, &store) => {
                        match result {
                            Ok(()) => { consecutive_errors = 0; }
                            Err(e) => {
                                consecutive_errors += 1;
                                let msg = e.to_string();
                                let (delay, event_type, health_msg) = if msg.contains("-14") || msg.contains("session timeout") {
                                    tracing::error!(error = %e, "weixin session expired — token may need refresh");
                                    (
                                        std::time::Duration::from_secs(60),
                                        AdapterHealthEventType::PollError,
                                        format!("session expired: {e}"),
                                    )
                                } else if consecutive_errors > 3 {
                                    tracing::warn!(error = %e, failures = consecutive_errors, "weixin poll backoff");
                                    (
                                        std::time::Duration::from_secs(10),
                                        AdapterHealthEventType::PollBackoff,
                                        format!("failures={consecutive_errors}: {e}"),
                                    )
                                } else {
                                    tracing::warn!(error = %e, "weixin poll error, retrying in 2s");
                                    (
                                        std::time::Duration::from_secs(2),
                                        AdapterHealthEventType::PollError,
                                        e.to_string(),
                                    )
                                };
                                emit_adapter_health(AdapterHealthEvent::new(
                                    "weixin",
                                    event_type,
                                    Some(health_msg),
                                ));
                                if poll_backoff_or_shutdown(delay, &mut shutdown_rx).await {
                                    break;
                                };
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
            emit_adapter_health(AdapterHealthEvent::new(
                "weixin",
                AdapterHealthEventType::Shutdown,
                None,
            ));
            let _ = tx.send(());
        }
    }

    async fn send_text(
        &self,
        chat_id: &str,
        text: &str,
        _reply_token: Option<&str>,
    ) -> Result<(), String> {
        let text = crate::markdown::rewrite_for_weixin(text);
        let text = if text.len() > MAX_MESSAGE_LENGTH {
            crate::text::truncate_with_suffix(&text, MAX_MESSAGE_LENGTH, "…")
        } else {
            text
        };

        let context_token = {
            let tokens = self.context_tokens.lock().await;
            tokens.get(chat_id).cloned().unwrap_or_default()
        };

        match send_text_with_retry(&self.config.token, chat_id, &text, &context_token).await {
            Ok(new_ct) => {
                if let Some(ct) = new_ct {
                    let mut tokens = self.context_tokens.lock().await;
                    tokens.insert(chat_id.to_string(), ct);
                }
                Ok(())
            }
            Err(e) => {
                if e.starts_with(FATAL_SEND_ERROR_PREFIX) {
                    // Fatal send (stale session even after tokenless retry) —
                    // evict the dead token so the NEXT inbound message's
                    // context_token takes over. Without this the cache
                    // keeps serving the dead value forever.
                    let mut tokens = self.context_tokens.lock().await;
                    if tokens.remove(chat_id).is_some() {
                        tracing::warn!(
                            chat_id = %crate::runner::truncate_chars(chat_id, 12),
                            "evicted dead context_token after fatal send"
                        );
                    }
                }
                Err(e)
            }
        }
    }

    async fn send_typing(&self, chat_id: &str) -> Result<(), String> {
        // Get or fetch typing ticket
        let ticket = {
            let cache = self.typing_tickets.lock().await;
            cache
                .get(chat_id)
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
                match fetch_typing_ticket(&self.config.token, chat_id, context_token.as_deref())
                    .await
                {
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

    async fn recv(&self) -> Option<InboundMessage> {
        self.msg_rx.lock().await.recv().await
    }
}

async fn poll_backoff_or_shutdown(
    delay: std::time::Duration,
    shutdown: &mut tokio::sync::broadcast::Receiver<()>,
) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(delay) => false,
        _ = shutdown.recv() => true,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InboundDelivery {
    Delivered,
    DroppedFull,
    Closed,
}

fn deliver_weixin_inbound(
    msg_tx: &mpsc::Sender<InboundMessage>,
    inbound: InboundMessage,
) -> InboundDelivery {
    match msg_tx.try_send(inbound) {
        Ok(()) => InboundDelivery::Delivered,
        Err(mpsc::error::TrySendError::Full(_)) => {
            emit_adapter_health(AdapterHealthEvent::new(
                "weixin",
                AdapterHealthEventType::InboundDropped,
                Some("inbound channel full".to_string()),
            ));
            InboundDelivery::DroppedFull
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            emit_adapter_health(AdapterHealthEvent::new(
                "weixin",
                AdapterHealthEventType::InboundDropped,
                Some("inbound channel closed".to_string()),
            ));
            InboundDelivery::Closed
        }
    }
}

async fn poll_updates(
    client: &reqwest::Client,
    config: &WeixinConfig,
    sync_buf: &mut String,
    msg_tx: &mpsc::Sender<InboundMessage>,
    dedup: &mut MessageDeduplicator,
    context_tokens: &ContextTokens,
    store: &Option<Arc<dyn GatewayStore>>,
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

    // Update sync cursor and persist to DB
    if let Some(buf) = data["get_updates_buf"].as_str() {
        *sync_buf = buf.to_string();
        if let Some(store) = store {
            let _ = store
                .save_credential(
                    "weixin",
                    "default",
                    "sync_buf",
                    &serde_json::Value::String(buf.to_string()),
                    None,
                )
                .await;
        }
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
                if let Some(store) = store {
                    let map: serde_json::Value = tokens
                        .iter()
                        .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                        .collect();
                    let _ = store
                        .save_credential("weixin", "default", "context_tokens", &map, None)
                        .await;
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
                safe_truncate(&text, 60)
            );

            let inbound = InboundMessage {
                platform: "weixin",
                chat_id,
                user_id: from_id,
                text,
                msg_id,
                chat_type,
                reply_token: None,
                route_override: None,
            };

            match deliver_weixin_inbound(msg_tx, inbound) {
                InboundDelivery::Delivered | InboundDelivery::DroppedFull => {}
                InboundDelivery::Closed => break,
            }
        }
    }

    Ok(())
}

fn safe_truncate(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn extract_text(msg: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(items) = msg["item_list"].as_array() {
        for item in items {
            match item["type"].as_i64() {
                Some(1) => {
                    if let Some(t) = item["text_item"]["text"].as_str() {
                        parts.push(t.to_string());
                    }
                }
                Some(3) => {
                    // Voice — use transcription if available
                    if let Some(t) = item["voice_item"]["text"].as_str()
                        && !t.trim().is_empty()
                    {
                        parts.push(format!("🎤 {t}"));
                    } else {
                        parts.push("🎤 [语音消息]".to_string());
                    }
                }
                Some(2) => {
                    if let Some(url) = item["image_item"]["media"]["full_url"].as_str() {
                        parts.push(format!("🖼 [图片: {url}]"));
                    } else {
                        parts.push("🖼 [图片]".to_string());
                    }
                }
                Some(4) => {
                    let name = item["file_item"]["file_name"].as_str().unwrap_or("unknown");
                    parts.push(format!("📎 [文件: {name}]"));
                }
                Some(5) => {
                    if let Some(url) = item["video_item"]["media"]["full_url"].as_str() {
                        parts.push(format!("🎬 [视频: {url}]"));
                    } else {
                        parts.push("🎬 [视频]".to_string());
                    }
                }
                _ => {}
            }
        }
    }
    parts.join("\n").trim().to_string()
}

// ─── QR Login ──────────────────────────────────────────────────────────────

const SEND_MAX_RETRIES: usize = 3;
const SEND_RETRY_DELAY_MS: u64 = 1500;

/// What to do when iLink returns a non-zero error code.
///
/// Semantics (observed from production traffic):
/// - iLink returns bare `{"ret":-2}` with NO `errmsg` field for stale
///   sessions (same condition as `errcode=-14`, despite the different code).
/// - iLink returns `-2` with an explicit `freq` errmsg for rate limiting.
/// - Any other `-2` with a specific errmsg is genuinely "something else
///   went wrong, give up for this call" — retrying won't change the
///   outcome and clogs the outbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SendRetryAction {
    /// Retry the same request without context_token (session expired).
    DropContextToken,
    /// Back off longer — server indicated rate limiting.
    RateLimitBackoff,
    /// Normal retry with delay (keep context_token).
    NormalRetry,
    /// Do NOT retry — the error is terminal for this message. Caller
    /// should give up and surface the failure immediately instead of
    /// blocking the outbox on repeated attempts that will all fail.
    /// Triggered when we've already tried tokenless and still hit
    /// stale-session-shape errors — the problem is outside our control
    /// (e.g. peer logged out, token revoked).
    Fatal,
}

/// Return true when the errmsg "looks like" iLink's stale-session signal:
/// missing, empty, or the default "unknown" / "unknown error" we see when
/// the response body is just `{"ret":-2}`. Same semantic as errcode=-14.
fn is_stale_session_errmsg(errmsg: &str) -> bool {
    let trimmed = errmsg.trim().to_lowercase();
    trimmed.is_empty() || trimmed == "unknown" || trimmed == "unknown error"
}

/// Decide how to handle an iLink send error. Pure function — no I/O.
fn classify_send_error(
    errcode: i64,
    errmsg: &str,
    already_tried_tokenless: bool,
) -> SendRetryAction {
    // -14 = explicit session expiry.
    if errcode == -14 {
        return if already_tried_tokenless {
            // Tokenless retry still session-expired → nothing left to try.
            // The peer's session is gone; our outbound is blocked until
            // they send us something new.
            SendRetryAction::Fatal
        } else {
            SendRetryAction::DropContextToken
        };
    }
    // -2 + explicit rate-limit errmsg (`freq`) → backoff.
    if errcode == -2 && errmsg.contains("freq") {
        return SendRetryAction::RateLimitBackoff;
    }
    // -2 with no/empty/"unknown" errmsg = stale session.
    // iLink returns bare `{"ret":-2}` in this case; we parse errmsg as
    // the default placeholder "unknown". Treat as session-expired.
    if errcode == -2 && is_stale_session_errmsg(errmsg) {
        return if already_tried_tokenless {
            // We already tried without context_token and still got -2 stale.
            // The peer's session is dead. Do NOT NormalRetry — that would
            // waste retry budget on the same hopeless attempt.
            SendRetryAction::Fatal
        } else {
            SendRetryAction::DropContextToken
        };
    }
    // -2 with some other specific errmsg: genuinely an iLink error we
    // don't understand. Retry once in case it's transient — but only
    // while we still have a token. Tokenless + unknown -2 = give up.
    if errcode == -2 && already_tried_tokenless {
        return SendRetryAction::Fatal;
    }
    // Everything else: normal retry with original context_token
    SendRetryAction::NormalRetry
}

/// Send a text message via iLink API with retries.
/// Returns the updated context_token from the response (if any).
async fn send_text_with_retry(
    token: &str,
    chat_id: &str,
    text: &str,
    context_token: &str,
) -> Result<Option<String>, String> {
    let client = reqwest::Client::new();
    let url = format!("{ILINK_BASE_URL}/ilink/bot/sendmessage");
    let mut last_error = String::new();
    let mut tried_tokenless = false;

    if text.is_empty() {
        return Err("empty message text".into());
    }
    tracing::debug!(
        text_len = text.len(),
        has_context_token = !context_token.is_empty(),
        "iLink send attempt"
    );

    for attempt in 0..=SEND_MAX_RETRIES {
        let ct = if tried_tokenless { "" } else { context_token };
        let client_id = format!("astra-gw-{}", uuid::Uuid::new_v4());

        let body = json!({
            "msg": {
                "from_user_id": "",
                "to_user_id": chat_id,
                "client_id": client_id,
                "message_type": 2,
                "message_state": 2,
                "context_token": ct,
                "item_list": [{"type": 1, "text_item": {"text": text}}]
            },
            "base_info": {"channel_version": CHANNEL_VERSION}
        });

        let resp = match client
            .post(&url)
            .headers(build_headers(token))
            .json(&body)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                last_error = format!("HTTP error: {e}");
                if attempt < SEND_MAX_RETRIES {
                    tokio::time::sleep(std::time::Duration::from_millis(
                        SEND_RETRY_DELAY_MS * (attempt as u64 + 1),
                    ))
                    .await;
                }
                continue;
            }
        };

        let data: Value = match resp.json().await {
            Ok(d) => d,
            Err(e) => {
                last_error = format!("parse error: {e}");
                continue;
            }
        };

        let errcode = data["errcode"]
            .as_i64()
            .or_else(|| data["ret"].as_i64())
            .unwrap_or(0);
        if errcode == 0 {
            let new_ct = data["context_token"]
                .as_str()
                .or_else(|| data["data"]["context_token"].as_str())
                .filter(|s| !s.is_empty())
                .map(String::from);
            return Ok(new_ct);
        }

        let errmsg = data["errmsg"].as_str().unwrap_or("unknown");
        tracing::debug!(errcode, errmsg, body = %data, "iLink send response (non-zero)");
        last_error = format!("{errcode}: {errmsg}");

        match classify_send_error(errcode, errmsg, tried_tokenless) {
            SendRetryAction::DropContextToken => {
                tracing::debug!(errcode, "retrying without context_token");
                tried_tokenless = true;
                continue;
            }
            SendRetryAction::RateLimitBackoff => {
                tracing::warn!("send rate limited, backing off");
                tokio::time::sleep(std::time::Duration::from_millis(SEND_RETRY_DELAY_MS * 3)).await;
                continue;
            }
            SendRetryAction::Fatal => {
                // Tokenless retry still stale (or some other unrecoverable
                // -14/-2). Do NOT keep hammering the same failing request.
                tracing::warn!(
                    errcode,
                    tried_tokenless,
                    "iLink send unrecoverable — giving up this attempt \
                     so the outbox isn't blocked on a dead session"
                );
                return Err(FATAL_SEND_ERROR_PREFIX.to_string() + &format!("{errcode}: {errmsg}"));
            }
            SendRetryAction::NormalRetry => {}
        }

        // Other error — retry with delay
        if attempt < SEND_MAX_RETRIES {
            tokio::time::sleep(std::time::Duration::from_millis(SEND_RETRY_DELAY_MS)).await;
        }
    }

    Err(format!("weixin send failed after retries: {last_error}"))
}

/// Prefix on Err(...) from send_text_with_retry that tells the caller this
/// is a fatal/unrecoverable send failure (stale session that even tokenless
/// retry couldn't resolve). The PlatformAdapter impl recognizes this prefix
/// and evicts the cached context_token so the next inbound message
/// refreshes it rather than reusing the dead one.
const FATAL_SEND_ERROR_PREFIX: &str = "weixin fatal send: ";

async fn fetch_typing_ticket(
    token: &str,
    user_id: &str,
    context_token: Option<&str>,
) -> Result<String, String> {
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
    let data: Value = resp
        .json()
        .await
        .map_err(|e| format!("getconfig parse: {e}"))?;
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
    let qrcode = data["qrcode"].as_str().ok_or("no qrcode in response")?;
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
                let token = status["bot_token"].as_str().unwrap_or("").to_string();
                let account_id = status["ilink_bot_id"].as_str().unwrap_or("").to_string();
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
    use tokio::sync::broadcast;
    use tokio::time::{Duration, timeout};

    #[test]
    fn config_debug_redacts_token() {
        let cfg = WeixinConfig {
            enabled: true,
            token: "secret-bot-token-12345".into(),
            account_id: "wxid_abc".into(),
        };
        let dbg = format!("{cfg:?}");
        assert!(
            !dbg.contains("secret-bot-token"),
            "token leaked in Debug: {dbg}"
        );
        assert!(dbg.contains("[REDACTED]"), "token not redacted: {dbg}");
        assert!(dbg.contains("wxid_abc"), "account_id should be visible");
    }

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
        let msg: Value = serde_json::from_str(
            r#"{
            "message_id": "msg-1",
            "from_user_id": "wxid_abc",
            "item_list": [
                {"type": 1, "text_item": {"text": "hello from wechat"}}
            ]
        }"#,
        )
        .unwrap();
        assert_eq!(extract_text(&msg), "hello from wechat");
    }

    #[test]
    fn extract_text_multi_items() {
        let msg: Value = serde_json::from_str(
            r#"{
            "item_list": [
                {"type": 1, "text_item": {"text": "line1"}},
                {"type": 2, "image_item": {}},
                {"type": 1, "text_item": {"text": "line2"}}
            ]
        }"#,
        )
        .unwrap();
        assert_eq!(extract_text(&msg), "line1\n🖼 [图片]\nline2");
    }

    #[test]
    fn extract_text_empty() {
        let msg: Value = serde_json::from_str(r#"{"item_list": []}"#).unwrap();
        assert_eq!(extract_text(&msg), "");
    }

    #[test]
    fn extract_text_voice_transcription() {
        let msg: Value = serde_json::from_str(
            r#"{
            "item_list": [
                {"type": 3, "voice_item": {"text": "你好", "playtime": 2764}}
            ]
        }"#,
        )
        .unwrap();
        assert_eq!(extract_text(&msg), "🎤 你好");
    }

    #[test]
    fn extract_text_voice_no_transcription() {
        let msg: Value = serde_json::from_str(
            r#"{
            "item_list": [
                {"type": 3, "voice_item": {"playtime": 2764}}
            ]
        }"#,
        )
        .unwrap();
        assert_eq!(extract_text(&msg), "🎤 [语音消息]");
    }

    #[test]
    fn extract_text_image_no_url() {
        let msg: Value = serde_json::from_str(
            r#"{
            "item_list": [
                {"type": 2, "image_item": {"media": {}}}
            ]
        }"#,
        )
        .unwrap();
        assert_eq!(extract_text(&msg), "🖼 [图片]");
    }

    #[test]
    fn extract_text_image_with_url() {
        let msg: Value = serde_json::from_str(r#"{
            "item_list": [
                {"type": 2, "image_item": {"media": {"full_url": "https://cdn.example.com/img.jpg"}}}
            ]
        }"#).unwrap();
        let text = extract_text(&msg);
        assert!(text.contains("🖼"));
        assert!(text.contains("https://cdn.example.com/img.jpg"));
    }

    #[test]
    fn extract_text_file() {
        let msg: Value = serde_json::from_str(
            r#"{
            "item_list": [
                {"type": 4, "file_item": {"file_name": "report.pdf"}}
            ]
        }"#,
        )
        .unwrap();
        assert_eq!(extract_text(&msg), "📎 [文件: report.pdf]");
    }

    #[test]
    fn extract_text_mixed_voice_and_text() {
        let msg: Value = serde_json::from_str(
            r#"{
            "item_list": [
                {"type": 3, "voice_item": {"text": "说的话"}},
                {"type": 1, "text_item": {"text": "打的字"}}
            ]
        }"#,
        )
        .unwrap();
        let text = extract_text(&msg);
        assert!(text.contains("说的话"));
        assert!(text.contains("打的字"));
    }

    #[test]
    fn max_message_truncation() {
        let long = "x".repeat(3000);
        let truncated = crate::text::truncate_with_suffix(&long, MAX_MESSAGE_LENGTH, "…");
        assert!(truncated.len() < 3000);
        assert!(truncated.len() <= MAX_MESSAGE_LENGTH);
    }

    #[test]
    fn max_message_truncation_preserves_utf8() {
        let long = "中文".repeat(1200);
        let truncated = crate::text::truncate_with_suffix(&long, MAX_MESSAGE_LENGTH, "…");
        assert!(truncated.len() <= MAX_MESSAGE_LENGTH);
        assert!(truncated.ends_with('…'));
    }

    #[tokio::test]
    async fn typing_ticket_cache_hit() {
        let cache: TypingTickets = Arc::new(Mutex::new(HashMap::new()));
        let user = "user1";
        // Insert a fresh ticket
        {
            let mut c = cache.lock().await;
            c.insert(
                user.to_string(),
                ("ticket_abc".into(), std::time::Instant::now()),
            );
        }
        // Should hit cache
        let c = cache.lock().await;
        let entry = c
            .get(user)
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
            let old = std::time::Instant::now()
                - std::time::Duration::from_secs(TYPING_TICKET_TTL_SECS + 10);
            c.insert(user.to_string(), ("old_ticket".into(), old));
        }
        let c = cache.lock().await;
        let entry = c
            .get(user)
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

    #[test]
    fn validates_restored_credentials() {
        assert!(validate_restored_token("token_abc"));
        assert!(!validate_restored_token(""));
        assert!(!validate_restored_token(" token"));
        assert!(!validate_restored_token("bad\nvalue"));
        assert!(validate_restored_id("wxid_abc"));
        assert!(!validate_restored_id("wxid\nabc"));
    }

    #[test]
    fn restore_state_filters_invalid_values() {
        let sync = Value::String("cursor-1".into());
        assert_eq!(restore_sync_buf_value(&sync).unwrap(), "cursor-1");
        assert!(restore_sync_buf_value(&json!({"bad": true})).is_none());

        let restored = restore_context_tokens_value(&json!({
            "user_ok": "ctx_ok",
            "bad\nuser": "ctx_bad",
            "user_bad": "ctx\nbad",
            "not_string": 42
        }));
        assert_eq!(restored.len(), 1);
        assert_eq!(restored.get("user_ok").unwrap(), "ctx_ok");
    }

    #[tokio::test]
    async fn poll_backoff_is_shutdown_interruptible() {
        let (tx, mut rx) = broadcast::channel(1);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let _ = tx.send(());
        });

        let interrupted = timeout(
            Duration::from_millis(200),
            poll_backoff_or_shutdown(Duration::from_secs(60), &mut rx),
        )
        .await
        .unwrap();

        assert!(interrupted);
    }

    #[tokio::test]
    async fn inbound_backpressure_drops_when_channel_full() {
        let (tx, mut rx) = mpsc::channel(1);
        let first = InboundMessage {
            platform: "weixin",
            chat_id: "chat".into(),
            user_id: "user".into(),
            text: "first".into(),
            msg_id: "msg-1".into(),
            chat_type: ChatType::DirectMessage,
            reply_token: None,
            route_override: None,
        };
        tx.send(first).await.unwrap();

        let second = InboundMessage {
            platform: "weixin",
            chat_id: "chat".into(),
            user_id: "user".into(),
            text: "second".into(),
            msg_id: "msg-2".into(),
            chat_type: ChatType::DirectMessage,
            reply_token: None,
            route_override: None,
        };

        assert_eq!(
            deliver_weixin_inbound(&tx, second),
            InboundDelivery::DroppedFull
        );
        assert_eq!(rx.recv().await.unwrap().text, "first");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn send_retry_constants() {
        const { assert!(SEND_MAX_RETRIES >= 2) };
        const { assert!(SEND_RETRY_DELAY_MS >= 1000) };
    }

    // ── classify_send_error regression tests ───────────────────────

    // Stale-session semantics:
    //   ret=-2 with errmsg missing / empty / "unknown"/"unknown error"
    //   = stale session (same bucket as errcode=-14).
    // Our iLink responses are literally `{"ret":-2}` with no errmsg field,
    // which we parse via .unwrap_or("unknown"). Both forms must map to
    // DropContextToken (first try) or Fatal (second try) — never
    // NormalRetry, which would waste the retry budget on a dead session.

    #[test]
    fn error_minus2_unknown_drops_context_token_once() {
        // First attempt with "unknown" → drop token, retry once.
        assert_eq!(
            classify_send_error(-2, "unknown", false),
            SendRetryAction::DropContextToken,
        );
    }

    #[test]
    fn error_minus2_empty_errmsg_treated_as_stale_session() {
        // iLink returns `{"ret":-2}` with no errmsg field → parser default
        // can be empty too. Same behavior as "unknown".
        assert_eq!(
            classify_send_error(-2, "", false),
            SendRetryAction::DropContextToken,
        );
    }

    #[test]
    fn error_minus2_unknown_error_with_space_treated_as_stale_session() {
        // Normalize: "unknown" (our default placeholder) and "unknown error"
        // (with space, the form some iLink responses carry) both mean stale.
        assert_eq!(
            classify_send_error(-2, "unknown error", false),
            SendRetryAction::DropContextToken,
        );
    }

    #[test]
    fn error_minus2_unknown_after_tokenless_is_fatal() {
        // BUG FIX: we used to loop on NormalRetry here, burning the entire
        // retry budget on doomed requests and blocking the outbox for
        // seconds. Tokenless retry still getting -2 means the session is
        // really dead — give up this call, let the next inbound refresh it.
        assert_eq!(
            classify_send_error(-2, "unknown", true),
            SendRetryAction::Fatal,
        );
    }

    #[test]
    fn error_minus2_empty_after_tokenless_is_fatal() {
        assert_eq!(classify_send_error(-2, "", true), SendRetryAction::Fatal,);
    }

    #[test]
    fn error_minus2_freq_triggers_rate_limit_backoff() {
        assert_eq!(
            classify_send_error(-2, "freq limit exceeded", false),
            SendRetryAction::RateLimitBackoff,
        );
    }

    #[test]
    fn error_minus2_freq_still_backoff_after_tokenless() {
        // Rate-limit is a server-load signal, not a session signal.
        // Whether or not we tried tokenless, backoff is the right response.
        assert_eq!(
            classify_send_error(-2, "freq exceeded", true),
            SendRetryAction::RateLimitBackoff,
        );
    }

    #[test]
    fn error_minus14_drops_context_token_once() {
        assert_eq!(
            classify_send_error(-14, "session expired", false),
            SendRetryAction::DropContextToken,
        );
    }

    #[test]
    fn error_minus14_is_fatal_after_tokenless() {
        // BUG FIX: was NormalRetry, which is hopeless for -14.
        assert_eq!(
            classify_send_error(-14, "session expired", true),
            SendRetryAction::Fatal,
        );
    }

    #[test]
    fn error_other_codes_normal_retry() {
        // Unknown negative codes without context_token-affinity: maybe
        // transient server blip, worth one more try.
        assert_eq!(
            classify_send_error(-1, "", false),
            SendRetryAction::NormalRetry,
        );
        assert_eq!(
            classify_send_error(-99, "server error", false),
            SendRetryAction::NormalRetry,
        );
    }

    #[test]
    fn error_zero_would_not_reach_classify() {
        // errcode 0 is success — classify is never called.
        // But if it were, it should be normal retry (harmless).
        assert_eq!(
            classify_send_error(0, "", false),
            SendRetryAction::NormalRetry,
        );
    }

    #[test]
    fn is_stale_session_errmsg_matches_known_shapes() {
        assert!(is_stale_session_errmsg(""));
        assert!(is_stale_session_errmsg("   "));
        assert!(is_stale_session_errmsg("unknown"));
        assert!(is_stale_session_errmsg("Unknown"));
        assert!(is_stale_session_errmsg("unknown error"));
        assert!(is_stale_session_errmsg("UNKNOWN ERROR"));
    }

    #[test]
    fn is_stale_session_errmsg_rejects_specific_errors() {
        assert!(!is_stale_session_errmsg("freq"));
        assert!(!is_stale_session_errmsg("freq limit"));
        assert!(!is_stale_session_errmsg("invalid token"));
        assert!(!is_stale_session_errmsg("peer offline"));
    }

    #[test]
    fn typing_ticket_ttl_reasonable() {
        const { assert!(TYPING_TICKET_TTL_SECS >= 300) };
        const { assert!(TYPING_TICKET_TTL_SECS <= 1800) };
    }

    #[test]
    fn send_diagnostics_do_not_log_message_preview() {
        let source = include_str!("weixin.rs");
        let needle = concat!("text_", "preview");
        assert!(
            !source.contains(needle),
            "send diagnostics must not log outbound message content previews"
        );
    }

    #[test]
    fn truncate_exact_crash_repro() {
        // This is the exact string that caused the panic:
        // byte 60 falls inside '量' (bytes 58..61)
        let text = "多角度review当前分支的修改 ，设计，工程质量，测试强度，产品行为等";
        // Must NOT panic
        let safe = safe_truncate(text, 60);
        assert!(safe.len() <= 60);
        assert!(
            text.is_char_boundary(safe.len()),
            "must end on char boundary"
        );
    }

    #[test]
    fn truncate_chinese_various_boundaries() {
        let text = "你好世界测试中文截断"; // 10 chars × 3 bytes = 30 bytes
        for max in 0..35 {
            let safe = safe_truncate(text, max);
            assert!(safe.len() <= max, "max={max}, got len={}", safe.len());
            assert!(
                text.is_char_boundary(safe.len()),
                "max={max}: not a char boundary"
            );
        }
    }

    #[test]
    fn truncate_ascii_exact() {
        let text = "hello world this is a test";
        let safe = safe_truncate(text, 10);
        assert_eq!(safe, "hello worl");
    }

    #[test]
    fn truncate_short_passthrough() {
        let text = "短";
        let safe = safe_truncate(text, 60);
        assert_eq!(safe, "短");
    }

    #[test]
    fn truncate_empty() {
        assert_eq!(safe_truncate("", 10), "");
    }

    #[test]
    fn truncate_zero_max() {
        assert_eq!(safe_truncate("hello", 0), "");
    }
}
