//! WeCom (企业微信) AI Bot WebSocket adapter.
//!
//! Protocol: connect → aibot_subscribe → heartbeat loop + message receive + outbound send.
//! Inbound: aibot_msg_callback. Outbound: aibot_send_msg / aibot_respond_msg.

use super::{
    AdapterCapability, AdapterHealthEvent, AdapterHealthEventType, ChatType, InboundMessage,
    PlatformAdapter, emit_adapter_health,
};
use crate::config::WeComConfig;
use crate::dedup::MessageDeduplicator;
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc};
use tokio_tungstenite::tungstenite::Message;

const HEARTBEAT_INTERVAL_SECS: u64 = 30;
const MAX_TEXT_LENGTH: usize = 4000;
const RECONNECT_DELAYS: &[u64] = &[2, 5, 10, 30, 60];
const WECOM_CAPABILITIES: &[AdapterCapability] = &[
    AdapterCapability::ReceiveText,
    AdapterCapability::SendText,
    AdapterCapability::GroupReply,
    AdapterCapability::WebSocket,
];

/// Outbound message to send via WebSocket.
struct OutboundMessage {
    chat_id: String,
    text: String,
    /// For group chats: the inbound req_id to use aibot_respond_msg.
    reply_token: Option<String>,
}

pub struct WeComAdapter {
    config: WeComConfig,
    msg_tx: mpsc::Sender<InboundMessage>,
    msg_rx: Mutex<mpsc::Receiver<InboundMessage>>,
    out_tx: mpsc::Sender<OutboundMessage>,
    shutdown: Option<tokio::sync::broadcast::Sender<()>>,
}

impl WeComAdapter {
    pub fn new(config: WeComConfig) -> Self {
        let (msg_tx, msg_rx) = mpsc::channel(256);
        let (out_tx, _out_rx) = mpsc::channel(256);
        Self {
            config: config.resolve(),
            msg_tx,
            msg_rx: Mutex::new(msg_rx),
            out_tx,
            shutdown: None,
        }
    }
}

#[async_trait]
impl PlatformAdapter for WeComAdapter {
    fn name(&self) -> &'static str {
        "wecom"
    }

    fn capabilities(&self) -> &'static [AdapterCapability] {
        WECOM_CAPABILITIES
    }

    async fn start(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if self.config.bot_id.is_empty() || self.config.secret.is_empty() {
            return Err("wecom: bot_id and secret required".into());
        }
        for capability in self.capabilities() {
            emit_adapter_health(AdapterHealthEvent::capability("wecom", *capability));
        }

        let (shutdown_tx, _) = tokio::sync::broadcast::channel(1);
        self.shutdown = Some(shutdown_tx.clone());

        let config = self.config.clone();
        let msg_tx = self.msg_tx.clone();

        // Create the real outbound channel and replace the placeholder
        let (out_tx, out_rx) = mpsc::channel(256);
        self.out_tx = out_tx;

        tokio::spawn(async move {
            let mut attempt = 0usize;
            let out_rx = std::sync::Arc::new(tokio::sync::Mutex::new(out_rx));
            loop {
                let mut shutdown_rx = shutdown_tx.subscribe();
                let out_rx_clone = out_rx.clone();
                match run_wecom_connection(&config, &msg_tx, out_rx_clone, &mut shutdown_rx).await {
                    Ok(()) => break,
                    Err(e) => {
                        let delay = RECONNECT_DELAYS[attempt.min(RECONNECT_DELAYS.len() - 1)];
                        emit_adapter_health(AdapterHealthEvent::new(
                            "wecom",
                            AdapterHealthEventType::Reconnecting,
                            Some(format!("{e}; retrying in {delay}s")),
                        ));
                        tracing::warn!(
                            error = %e,
                            delay_s = delay,
                            attempt = attempt + 1,
                            "wecom connection failed, reconnecting"
                        );
                        attempt += 1;
                        if wait_reconnect_delay(
                            std::time::Duration::from_secs(delay),
                            &mut shutdown_rx,
                        )
                        .await
                        {
                            break;
                        }
                    }
                }
            }
        });

        tracing::info!(bot_id = %self.config.bot_id, "wecom adapter started");
        Ok(())
    }

    async fn stop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            emit_adapter_health(AdapterHealthEvent::new(
                "wecom",
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
        reply_token: Option<&str>,
    ) -> Result<(), String> {
        let text = if text.len() > MAX_TEXT_LENGTH {
            crate::text::truncate_with_suffix(text, MAX_TEXT_LENGTH, "…\n\n(truncated)")
        } else {
            text.to_string()
        };

        self.out_tx
            .send(OutboundMessage {
                chat_id: chat_id.to_string(),
                text,
                reply_token: reply_token.map(String::from),
            })
            .await
            .map_err(|e| format!("outbound channel send failed: {e}"))
    }

    async fn recv(&self) -> Option<InboundMessage> {
        self.msg_rx.lock().await.recv().await
    }
}

#[cfg(test)]
mod truncation_tests {
    use super::*;

    #[test]
    fn outbound_truncation_preserves_utf8() {
        let long = "企业微信".repeat(1200);
        let truncated =
            crate::text::truncate_with_suffix(&long, MAX_TEXT_LENGTH, "…\n\n(truncated)");
        assert!(truncated.len() <= MAX_TEXT_LENGTH);
        assert!(truncated.ends_with("(truncated)"));
    }
}

async fn run_wecom_connection(
    config: &WeComConfig,
    msg_tx: &mpsc::Sender<InboundMessage>,
    out_rx: std::sync::Arc<tokio::sync::Mutex<mpsc::Receiver<OutboundMessage>>>,
    shutdown: &mut tokio::sync::broadcast::Receiver<()>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (ws_stream, _) = tokio_tungstenite::connect_async(&config.websocket_url).await?;
    let (mut ws_write, mut ws_read) = ws_stream.split();
    emit_adapter_health(AdapterHealthEvent::new(
        "wecom",
        AdapterHealthEventType::Connected,
        None,
    ));

    // Subscribe (WeCom AI Bot uses bot_id + secret in body, no signature)
    let subscribe_msg = json!({
        "cmd": "aibot_subscribe",
        "headers": {"req_id": format!("subscribe-{}", uuid::Uuid::new_v4())},
        "body": {
            "bot_id": &config.bot_id,
            "secret": &config.secret,
            "device_id": uuid::Uuid::new_v4().to_string().replace("-", ""),
        }
    });
    ws_write
        .send(Message::Text(subscribe_msg.to_string().into()))
        .await?;
    tracing::info!("wecom subscribe sent");

    let mut dedup = MessageDeduplicator::new();
    let mut heartbeat =
        tokio::time::interval(std::time::Duration::from_secs(HEARTBEAT_INTERVAL_SECS));
    heartbeat.tick().await;
    let bot_id = config.bot_id.clone();

    loop {
        let mut out_guard = out_rx.lock().await;

        tokio::select! {
            out = out_guard.recv() => {
                let Some(out) = out else {
                    emit_adapter_health(AdapterHealthEvent::new(
                        "wecom",
                        AdapterHealthEventType::Disconnected,
                        Some("outbound channel closed".to_string()),
                    ));
                    return Err("wecom outbound channel closed".into());
                };
                let frame = build_send_frame(&bot_id, &out);
                match ws_write.send(Message::Text(frame.to_string().into())).await {
                    Ok(()) => {
                        emit_adapter_health(AdapterHealthEvent::new(
                            "wecom",
                            AdapterHealthEventType::SendAck,
                            Some(out.chat_id),
                        ));
                    }
                    Err(e) => {
                        emit_adapter_health(AdapterHealthEvent::new(
                            "wecom",
                            AdapterHealthEventType::SendError,
                            Some(e.to_string()),
                        ));
                        tracing::error!(error = %e, "wecom outbound send failed");
                        return Err(format!("wecom outbound send failed: {e}").into());
                    }
                }
            }
            _ = heartbeat.tick() => {
                let ping = json!({
                    "cmd": "ping",
                    "headers": {"req_id": format!("ping-{}", uuid::Uuid::new_v4())},
                    "body": {}
                });
                ws_write.send(Message::Text(ping.to_string().into())).await?;
            }
            msg = ws_read.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        if let Ok(data) = serde_json::from_str::<Value>(&text) {
                            handle_wecom_message(&data, msg_tx, &mut dedup).await;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        emit_adapter_health(AdapterHealthEvent::new(
                            "wecom",
                            AdapterHealthEventType::Disconnected,
                            Some("websocket closed".to_string()),
                        ));
                        return Err("wecom websocket closed".into());
                    }
                    Some(Err(e)) => {
                        emit_adapter_health(AdapterHealthEvent::new(
                            "wecom",
                            AdapterHealthEventType::Disconnected,
                            Some(e.to_string()),
                        ));
                        return Err(format!("wecom ws error: {e}").into());
                    }
                    _ => {}
                }
            }
            _ = shutdown.recv() => {
                emit_adapter_health(AdapterHealthEvent::new(
                    "wecom",
                    AdapterHealthEventType::Shutdown,
                    None,
                ));
                let _ = ws_write.close().await;
                return Ok(());
            }
        }
    }
}

async fn wait_reconnect_delay(
    delay: std::time::Duration,
    shutdown: &mut tokio::sync::broadcast::Receiver<()>,
) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(delay) => false,
        _ = shutdown.recv() => true,
    }
}

fn build_send_frame(bot_id: &str, out: &OutboundMessage) -> Value {
    if let Some(ref req_id) = out.reply_token {
        // Group chat: respond to the inbound request
        json!({
            "cmd": "aibot_respond_msg",
            "headers": {"req_id": req_id},
            "body": {
                "msgtype": "markdown",
                "markdown": {"content": &out.text}
            }
        })
    } else {
        // DM: proactive send
        json!({
            "cmd": "aibot_send_msg",
            "headers": {"req_id": format!("send-{}", uuid::Uuid::new_v4())},
            "body": {
                "bot_id": bot_id,
                "chatid": &out.chat_id,
                "msgtype": "markdown",
                "markdown": {"content": &out.text}
            }
        })
    }
}

async fn handle_wecom_message(
    data: &Value,
    msg_tx: &mpsc::Sender<InboundMessage>,
    dedup: &mut MessageDeduplicator,
) {
    let cmd = data["cmd"].as_str().unwrap_or("");
    if cmd != "aibot_msg_callback" && cmd != "aibot_callback" {
        if let Some((event_type, detail)) = classify_wecom_control_message(data) {
            emit_adapter_health(AdapterHealthEvent::new("wecom", event_type, detail));
        }
        return;
    }

    let body = &data["body"];
    let msg_id = body["msgid"].as_str().unwrap_or("").to_string();
    if msg_id.is_empty() || !dedup.check(&msg_id) {
        return;
    }

    let text = body["text"]["content"]
        .as_str()
        .or_else(|| body["voice"]["content"].as_str())
        .unwrap_or("")
        .trim()
        .to_string();

    if text.is_empty() {
        return;
    }

    let chat_id = body["chatid"].as_str().unwrap_or("").to_string();
    let user_id = body["from"]["userid"]
        .as_str()
        .unwrap_or("unknown")
        .to_string();
    let chat_type = if body["chattype"].as_str() == Some("group") {
        ChatType::Group
    } else {
        ChatType::DirectMessage
    };
    let reply_token = data["headers"]["req_id"].as_str().map(String::from);

    let msg = InboundMessage {
        platform: "wecom",
        chat_id,
        user_id,
        text,
        msg_id,
        chat_type,
        reply_token,
        route_override: None,
    };

    if msg_tx.send(msg).await.is_err() {
        tracing::warn!("wecom message channel full, dropping message");
    }
}

fn classify_wecom_control_message(
    data: &Value,
) -> Option<(AdapterHealthEventType, Option<String>)> {
    let cmd = data["cmd"].as_str().unwrap_or("");
    match cmd {
        "aibot_subscribe" => {
            let errcode = data["body"]["errcode"].as_i64().unwrap_or(-1);
            if errcode == 0 {
                tracing::info!("wecom subscription confirmed");
                Some((AdapterHealthEventType::SubscribeAck, None))
            } else {
                tracing::error!(errcode, "wecom subscription failed");
                Some((
                    AdapterHealthEventType::SubscribeError,
                    Some(format!("errcode={errcode}")),
                ))
            }
        }
        "aibot_send_msg" | "aibot_respond_msg" => {
            let errcode = data["body"]["errcode"]
                .as_i64()
                .or_else(|| data["errcode"].as_i64())
                .unwrap_or(0);
            if errcode == 0 {
                Some((AdapterHealthEventType::SendAck, None))
            } else {
                let errmsg = data["body"]["errmsg"]
                    .as_str()
                    .or_else(|| data["errmsg"].as_str())
                    .unwrap_or("unknown");
                Some((
                    AdapterHealthEventType::SendError,
                    Some(format!("{errcode}: {errmsg}")),
                ))
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use tokio::net::TcpListener;
    use tokio::sync::{broadcast, oneshot};
    use tokio::time::{Duration, timeout};

    #[test]
    fn parse_wecom_callback() {
        let data: Value = serde_json::from_str(
            r#"{
            "cmd": "aibot_msg_callback",
            "headers": {"req_id": "req-123"},
            "body": {
                "msgid": "msg-001",
                "msgtype": "text",
                "from": {"userid": "user-1"},
                "chatid": "chat-1",
                "chattype": "single",
                "text": {"content": "hello world"}
            }
        }"#,
        )
        .unwrap();

        let (tx, mut rx) = mpsc::channel(10);
        let mut dedup = MessageDeduplicator::new();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            handle_wecom_message(&data, &tx, &mut dedup).await;
            let msg = rx.recv().await.unwrap();
            assert_eq!(msg.platform, "wecom");
            assert_eq!(msg.chat_id, "chat-1");
            assert_eq!(msg.user_id, "user-1");
            assert_eq!(msg.text, "hello world");
            assert_eq!(msg.chat_type, ChatType::DirectMessage);
            assert_eq!(msg.reply_token, Some("req-123".to_string()));
        });
    }

    #[test]
    fn dedup_skips_duplicate_callback() {
        let data: Value = serde_json::from_str(
            r#"{
            "cmd": "aibot_msg_callback",
            "headers": {"req_id": "req-1"},
            "body": {
                "msgid": "msg-dup",
                "from": {"userid": "u"},
                "chatid": "c",
                "chattype": "single",
                "text": {"content": "hi"}
            }
        }"#,
        )
        .unwrap();

        let (tx, mut rx) = mpsc::channel(10);
        let mut dedup = MessageDeduplicator::new();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            handle_wecom_message(&data, &tx, &mut dedup).await;
            handle_wecom_message(&data, &tx, &mut dedup).await;
            assert!(rx.recv().await.is_some());
            assert!(rx.try_recv().is_err());
        });
    }

    #[test]
    fn empty_text_ignored() {
        let data: Value = serde_json::from_str(
            r#"{
            "cmd": "aibot_msg_callback",
            "headers": {"req_id": "r"},
            "body": {
                "msgid": "m",
                "from": {"userid": "u"},
                "chatid": "c",
                "chattype": "single",
                "text": {"content": "  "}
            }
        }"#,
        )
        .unwrap();

        let (tx, mut rx) = mpsc::channel(10);
        let mut dedup = MessageDeduplicator::new();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            handle_wecom_message(&data, &tx, &mut dedup).await;
            assert!(rx.try_recv().is_err());
        });
    }

    #[test]
    fn build_send_frame_dm() {
        let out = OutboundMessage {
            chat_id: "chat-123".into(),
            text: "hello".into(),
            reply_token: None,
        };
        let frame = build_send_frame("bot-1", &out);
        assert_eq!(frame["cmd"], "aibot_send_msg");
        assert_eq!(frame["body"]["chatid"], "chat-123");
        assert_eq!(frame["body"]["markdown"]["content"], "hello");
    }

    #[test]
    fn build_send_frame_group_respond() {
        let out = OutboundMessage {
            chat_id: "group-456".into(),
            text: "response".into(),
            reply_token: Some("req-original".into()),
        };
        let frame = build_send_frame("bot-1", &out);
        assert_eq!(frame["cmd"], "aibot_respond_msg");
        assert_eq!(frame["headers"]["req_id"], "req-original");
        assert_eq!(frame["body"]["markdown"]["content"], "response");
    }

    #[test]
    fn classify_send_ack_and_error_health() {
        let ack: Value =
            serde_json::from_str(r#"{"cmd":"aibot_send_msg","body":{"errcode":0}}"#).unwrap();
        let err: Value = serde_json::from_str(
            r#"{"cmd":"aibot_respond_msg","body":{"errcode":45009,"errmsg":"rate limited"}}"#,
        )
        .unwrap();

        let (event, detail) = classify_wecom_control_message(&ack).unwrap();
        assert_eq!(event, AdapterHealthEventType::SendAck);
        assert!(detail.is_none());

        let (event, detail) = classify_wecom_control_message(&err).unwrap();
        assert_eq!(event, AdapterHealthEventType::SendError);
        assert!(detail.unwrap().contains("rate limited"));
    }

    #[tokio::test]
    async fn outbound_send_wakes_without_waiting_for_heartbeat() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (seen_tx, seen_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();

            let subscribe = timeout(Duration::from_millis(500), ws.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let subscribe = subscribe.into_text().unwrap();
            assert!(subscribe.contains("aibot_subscribe"));

            let outbound = timeout(Duration::from_millis(500), ws.next())
                .await
                .expect("outbound send should wake immediately")
                .unwrap()
                .unwrap();
            let outbound = outbound.into_text().unwrap();
            assert!(outbound.contains("aibot_send_msg"));
            assert!(outbound.contains("wake now"));
            let _ = seen_tx.send(());
            let _ = timeout(Duration::from_secs(1), ws.next()).await;
        });

        let config = WeComConfig {
            enabled: true,
            bot_id: "bot".into(),
            secret: "secret".into(),
            websocket_url: format!("ws://{addr}"),
        };
        let (msg_tx, _msg_rx) = mpsc::channel(4);
        let (out_tx, out_rx) = mpsc::channel(4);
        let (shutdown_tx, mut shutdown_rx) = broadcast::channel(1);
        let out_rx = std::sync::Arc::new(tokio::sync::Mutex::new(out_rx));

        let client = tokio::spawn(async move {
            run_wecom_connection(&config, &msg_tx, out_rx, &mut shutdown_rx).await
        });

        out_tx
            .send(OutboundMessage {
                chat_id: "chat-1".into(),
                text: "wake now".into(),
                reply_token: None,
            })
            .await
            .unwrap();

        timeout(Duration::from_millis(500), seen_rx)
            .await
            .expect("server should see outbound frame")
            .unwrap();
        let _ = shutdown_tx.send(());
        server.await.unwrap();
        let result = timeout(Duration::from_secs(1), client)
            .await
            .unwrap()
            .unwrap();
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn reconnect_delay_is_shutdown_interruptible() {
        let (tx, mut rx) = broadcast::channel(1);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let _ = tx.send(());
        });

        let interrupted = timeout(
            Duration::from_millis(200),
            wait_reconnect_delay(Duration::from_secs(60), &mut rx),
        )
        .await
        .unwrap();

        assert!(interrupted);
    }

    #[test]
    fn parse_voice_message() {
        let data: Value = serde_json::from_str(
            r#"{
            "cmd": "aibot_msg_callback",
            "headers": {"req_id": "r1"},
            "body": {
                "msgid": "voice-1",
                "from": {"userid": "u1"},
                "chatid": "c1",
                "chattype": "single",
                "voice": {"content": "transcribed text from voice"}
            }
        }"#,
        )
        .unwrap();

        let (tx, mut rx) = mpsc::channel(10);
        let mut dedup = MessageDeduplicator::new();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            handle_wecom_message(&data, &tx, &mut dedup).await;
            let msg = rx.recv().await.unwrap();
            assert_eq!(msg.text, "transcribed text from voice");
        });
    }

    #[test]
    fn parse_group_message() {
        let data: Value = serde_json::from_str(
            r#"{
            "cmd": "aibot_msg_callback",
            "headers": {"req_id": "req-group"},
            "body": {
                "msgid": "g1",
                "from": {"userid": "u1"},
                "chatid": "group-123",
                "chattype": "group",
                "text": {"content": "group message"}
            }
        }"#,
        )
        .unwrap();

        let (tx, mut rx) = mpsc::channel(10);
        let mut dedup = MessageDeduplicator::new();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            handle_wecom_message(&data, &tx, &mut dedup).await;
            let msg = rx.recv().await.unwrap();
            assert_eq!(msg.chat_type, ChatType::Group);
            assert_eq!(msg.chat_id, "group-123");
            assert_eq!(msg.reply_token, Some("req-group".into()));
        });
    }

    #[test]
    fn parse_subscribe_success() {
        let data: Value = serde_json::from_str(
            r#"{
            "cmd": "aibot_subscribe",
            "headers": {},
            "body": {"errcode": 0}
        }"#,
        )
        .unwrap();

        let (tx, mut rx) = mpsc::channel(10);
        let mut dedup = MessageDeduplicator::new();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            handle_wecom_message(&data, &tx, &mut dedup).await;
            // Subscribe responses don't produce InboundMessages
            assert!(rx.try_recv().is_err());
        });
    }

    #[test]
    fn unknown_cmd_ignored() {
        let data: Value =
            serde_json::from_str(r#"{"cmd": "pong", "headers": {}, "body": {}}"#).unwrap();

        let (tx, mut rx) = mpsc::channel(10);
        let mut dedup = MessageDeduplicator::new();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            handle_wecom_message(&data, &tx, &mut dedup).await;
            assert!(rx.try_recv().is_err());
        });
    }

    #[test]
    fn missing_msgid_ignored() {
        let data: Value = serde_json::from_str(
            r#"{
            "cmd": "aibot_msg_callback",
            "headers": {"req_id": "r"},
            "body": {
                "from": {"userid": "u"},
                "chatid": "c",
                "text": {"content": "no msgid"}
            }
        }"#,
        )
        .unwrap();

        let (tx, mut rx) = mpsc::channel(10);
        let mut dedup = MessageDeduplicator::new();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            handle_wecom_message(&data, &tx, &mut dedup).await;
            assert!(rx.try_recv().is_err());
        });
    }
}
