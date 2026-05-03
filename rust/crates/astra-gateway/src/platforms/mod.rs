pub mod wecom;
pub mod weixin;

use async_trait::async_trait;

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
    async fn start(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
    async fn stop(&mut self);
    async fn send_text(&self, chat_id: &str, text: &str, reply_token: Option<&str>) -> Result<(), String>;
    /// Receive the next inbound message (blocking).
    async fn recv(&mut self) -> Option<InboundMessage>;
}
