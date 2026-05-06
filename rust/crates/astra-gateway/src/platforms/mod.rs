pub mod wecom;
pub mod weixin;

use async_trait::async_trait;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdapterCapability {
    ReceiveText,
    SendText,
    SendTyping,
    GroupReply,
    #[serde(rename = "websocket")]
    WebSocket,
    LongPoll,
    PersistentState,
}

impl AdapterCapability {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReceiveText => "receive_text",
            Self::SendText => "send_text",
            Self::SendTyping => "send_typing",
            Self::GroupReply => "group_reply",
            Self::WebSocket => "websocket",
            Self::LongPoll => "long_poll",
            Self::PersistentState => "persistent_state",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdapterHealthEventType {
    Capability,
    Connected,
    Disconnected,
    Reconnecting,
    Shutdown,
    SubscribeAck,
    SubscribeError,
    SendAck,
    SendError,
    CredentialRestored,
    CredentialInvalid,
    StateRestored,
    StateInvalid,
    PollError,
    PollBackoff,
    InboundDropped,
}

impl AdapterHealthEventType {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Capability => "capability",
            Self::Connected => "connected",
            Self::Disconnected => "disconnected",
            Self::Reconnecting => "reconnecting",
            Self::Shutdown => "shutdown",
            Self::SubscribeAck => "subscribe_ack",
            Self::SubscribeError => "subscribe_error",
            Self::SendAck => "send_ack",
            Self::SendError => "send_error",
            Self::CredentialRestored => "credential_restored",
            Self::CredentialInvalid => "credential_invalid",
            Self::StateRestored => "state_restored",
            Self::StateInvalid => "state_invalid",
            Self::PollError => "poll_error",
            Self::PollBackoff => "poll_backoff",
            Self::InboundDropped => "inbound_dropped",
        }
    }

    const fn is_error(self) -> bool {
        matches!(
            self,
            Self::Disconnected
                | Self::SubscribeError
                | Self::SendError
                | Self::CredentialInvalid
                | Self::StateInvalid
                | Self::InboundDropped
        )
    }

    const fn is_warn(self) -> bool {
        matches!(
            self,
            Self::Reconnecting | Self::PollError | Self::PollBackoff
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AdapterHealthEvent {
    pub platform: &'static str,
    pub event_type: AdapterHealthEventType,
    pub capability: Option<AdapterCapability>,
    pub detail: Option<String>,
}

impl AdapterHealthEvent {
    pub fn new(
        platform: &'static str,
        event_type: AdapterHealthEventType,
        detail: Option<String>,
    ) -> Self {
        Self {
            platform,
            event_type,
            capability: None,
            detail,
        }
    }

    pub fn capability(platform: &'static str, capability: AdapterCapability) -> Self {
        Self {
            platform,
            event_type: AdapterHealthEventType::Capability,
            capability: Some(capability),
            detail: None,
        }
    }
}

pub fn emit_adapter_health(event: AdapterHealthEvent) -> AdapterHealthEvent {
    let event_type = event.event_type.as_str();
    let capability = event.capability.map(AdapterCapability::as_str);
    let detail = event.detail.as_deref();
    if event.event_type.is_error() {
        tracing::error!(
            platform = event.platform,
            event_type,
            capability,
            detail,
            "platform adapter health"
        );
    } else if event.event_type.is_warn() {
        tracing::warn!(
            platform = event.platform,
            event_type,
            capability,
            detail,
            "platform adapter health"
        );
    } else {
        tracing::info!(
            platform = event.platform,
            event_type,
            capability,
            detail,
            "platform adapter health"
        );
    }
    event
}

/// Normalized inbound message from any chat platform.
#[derive(Debug, Clone)]
pub struct InboundMessage {
    pub platform: &'static str,
    pub chat_id: String,
    pub user_id: String,
    pub text: String,
    pub msg_id: String,
    pub chat_type: ChatType,
    /// WeCom: the inbound req_id, needed for group responds.
    pub reply_token: Option<String>,
    /// When the router wants to enqueue this message under a
    /// non-default cli_profile (e.g. `/manage` routes through the
    /// `_manage` virtual profile so it does NOT queue behind the user's
    /// stuck tasks). None = use resolve_cli_profile's normal result.
    pub route_override: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatType {
    DirectMessage,
    Group,
}

impl InboundMessage {
    pub fn session_key(&self) -> String {
        format!("{}:{}", self.platform, self.chat_id)
    }
}

/// Platform adapter trait — implemented by each chat platform.
#[async_trait]
pub trait PlatformAdapter: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    fn capabilities(&self) -> &'static [AdapterCapability] {
        &[]
    }
    async fn start(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
    async fn stop(&mut self);
    async fn send_text(
        &self,
        chat_id: &str,
        text: &str,
        reply_token: Option<&str>,
    ) -> Result<(), String>;
    /// Send typing indicator (start). No-op for platforms that don't support it.
    async fn send_typing(&self, _chat_id: &str) -> Result<(), String> {
        Ok(())
    }
    /// Receive the next inbound message (blocking).
    async fn recv(&self) -> Option<InboundMessage>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_event_serializes() {
        let event = AdapterHealthEvent::capability("wecom", AdapterCapability::WebSocket);
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("capability"));
        assert!(json.contains("websocket"));
    }

    #[test]
    fn emit_helper_returns_event() {
        let event = emit_adapter_health(AdapterHealthEvent::new(
            "weixin",
            AdapterHealthEventType::StateInvalid,
            Some("bad sync cursor".to_string()),
        ));
        assert_eq!(event.platform, "weixin");
        assert_eq!(event.event_type, AdapterHealthEventType::StateInvalid);
    }
}
