//! Gateway identity, trace, and reliability state model.
//!
//! The model keeps mutable request/run/outbox rows for current state, plus an
//! append-only event stream keyed by typed identifiers for audits and `/trace`.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::MySqlPool;
use std::fmt;

pub type TraceResult<T> = Result<T, String>;

/// Maximum number of delivery attempts for an outbox entry before it is
/// considered expired and excluded from future retries.
pub const OUTBOX_MAX_RETRIES: u32 = 3;

macro_rules! id_type {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new() -> Self {
                Self(uuid::Uuid::new_v4().to_string())
            }

            pub fn from_string(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_string())
            }
        }
    };
}

id_type!(RequestId);
id_type!(TraceId);
id_type!(RunId);
id_type!(OutboxId);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ConversationKey {
    platform: String,
    chat_id: String,
    cli_profile: String,
}

impl ConversationKey {
    pub fn new(
        platform: impl Into<String>,
        chat_id: impl Into<String>,
        cli_profile: impl Into<String>,
    ) -> Self {
        Self {
            platform: platform.into(),
            chat_id: chat_id.into(),
            cli_profile: cli_profile.into(),
        }
    }

    pub fn platform(&self) -> &str {
        &self.platform
    }

    pub fn chat_id(&self) -> &str {
        &self.chat_id
    }

    pub fn cli_profile(&self) -> &str {
        &self.cli_profile
    }
}

impl fmt::Display for ConversationKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}", self.platform, self.chat_id, self.cli_profile)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestStatus {
    Accepted,
    Running,
    Completed,
    Failed,
}

impl RequestStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "accepted" => Some(Self::Accepted),
            "running" => Some(Self::Running),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed)
    }

    pub fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Accepted, Self::Running)
                | (Self::Accepted, Self::Completed)
                | (Self::Accepted, Self::Failed)
                | (Self::Running, Self::Completed)
                | (Self::Running, Self::Failed)
        ) || self == next
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Started,
    Succeeded,
    Failed,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "started" => Some(Self::Started),
            "succeeded" => Some(Self::Succeeded),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }

    pub fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Started, Self::Succeeded) | (Self::Started, Self::Failed)
        ) || self == next
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutboxStatus {
    Pending,
    Sent,
    Failed,
}

impl OutboxStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Sent => "sent",
            Self::Failed => "failed",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "sent" => Some(Self::Sent),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }

    pub fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Pending, Self::Sent)
                | (Self::Pending, Self::Failed)
                | (Self::Failed, Self::Sent)
                | (Self::Failed, Self::Failed)
        ) || self == next
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GatewayEventKind {
    RequestReceived,
    RequestCompleted,
    RequestFailed,
    RunStarted,
    RunCompleted,
    RunFailed,
    RequestQueued,
    RequestRunning,
    RequestCancelled,
    RequestShutdown,
    PolicyDenied,
    CliProgress,
    OutboxQueued,
    OutboxSent,
    OutboxFailed,
    Unknown,
}

impl GatewayEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RequestReceived => "request_received",
            Self::RequestCompleted => "request_completed",
            Self::RequestFailed => "request_failed",
            Self::RunStarted => "run_started",
            Self::RunCompleted => "run_completed",
            Self::RunFailed => "run_failed",
            Self::RequestQueued => "request_queued",
            Self::RequestRunning => "request_running",
            Self::RequestCancelled => "request_cancelled",
            Self::RequestShutdown => "request_shutdown",
            Self::PolicyDenied => "policy_denied",
            Self::CliProgress => "cli_progress",
            Self::OutboxQueued => "outbox_queued",
            Self::OutboxSent => "outbox_sent",
            Self::OutboxFailed => "outbox_failed",
            Self::Unknown => "unknown",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "request_received" => Self::RequestReceived,
            "request_completed" => Self::RequestCompleted,
            "request_failed" => Self::RequestFailed,
            "run_started" => Self::RunStarted,
            "run_completed" => Self::RunCompleted,
            "run_failed" => Self::RunFailed,
            "request_queued" => Self::RequestQueued,
            "request_running" => Self::RequestRunning,
            "request_cancelled" => Self::RequestCancelled,
            "request_shutdown" => Self::RequestShutdown,
            "policy_denied" => Self::PolicyDenied,
            "cli_progress" => Self::CliProgress,
            "outbox_queued" => Self::OutboxQueued,
            "outbox_sent" => Self::OutboxSent,
            "outbox_failed" => Self::OutboxFailed,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone)]
pub struct GatewayRequest {
    pub request_id: RequestId,
    pub trace_id: TraceId,
    pub conversation: ConversationKey,
    pub platform_msg_id: String,
    pub user_id: String,
    pub text: String,
    pub status: RequestStatus,
    pub error_message: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl GatewayRequest {
    pub fn new(
        conversation: ConversationKey,
        platform_msg_id: impl Into<String>,
        user_id: impl Into<String>,
        text: impl Into<String>,
    ) -> Self {
        Self {
            request_id: RequestId::new(),
            trace_id: TraceId::new(),
            conversation,
            platform_msg_id: platform_msg_id.into(),
            user_id: user_id.into(),
            text: text.into(),
            status: RequestStatus::Accepted,
            error_message: None,
            created_at: Utc::now(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct GatewayRun {
    pub run_id: RunId,
    pub request_id: RequestId,
    pub trace_id: TraceId,
    pub cli_profile: String,
    pub session_id: Option<String>,
    pub status: RunStatus,
    pub exit_code: Option<i32>,
    pub error_message: Option<String>,
}

impl GatewayRun {
    pub fn start(
        request_id: RequestId,
        trace_id: TraceId,
        cli_profile: impl Into<String>,
        session_id: Option<String>,
    ) -> Self {
        Self {
            run_id: RunId::new(),
            request_id,
            trace_id,
            cli_profile: cli_profile.into(),
            session_id,
            status: RunStatus::Started,
            exit_code: None,
            error_message: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct OutboxRecord {
    pub outbox_id: OutboxId,
    pub request_id: RequestId,
    pub trace_id: TraceId,
    pub platform: String,
    pub chat_id: String,
    pub reply_token: Option<String>,
    pub body: String,
    pub status: OutboxStatus,
    pub error_message: Option<String>,
    pub retry_count: u32,
}

impl OutboxRecord {
    pub fn pending(
        request_id: RequestId,
        trace_id: TraceId,
        platform: impl Into<String>,
        chat_id: impl Into<String>,
        reply_token: Option<String>,
        body: impl Into<String>,
    ) -> Self {
        Self {
            outbox_id: OutboxId::new(),
            request_id,
            trace_id,
            platform: platform.into(),
            chat_id: chat_id.into(),
            reply_token,
            body: body.into(),
            status: OutboxStatus::Pending,
            error_message: None,
            retry_count: 0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct NewGatewayEvent {
    pub trace_id: TraceId,
    pub request_id: RequestId,
    pub run_id: Option<RunId>,
    pub kind: GatewayEventKind,
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct GatewayEvent {
    pub sequence: i64,
    pub trace_id: TraceId,
    pub request_id: RequestId,
    pub run_id: Option<RunId>,
    pub kind: GatewayEventKind,
    pub payload: serde_json::Value,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct TraceSummary {
    pub trace_id: TraceId,
    pub request_id: RequestId,
    pub status: RequestStatus,
    pub text_preview: String,
    pub created_at: String,
    pub event_count: u64,
}

#[derive(Debug, Clone)]
pub struct ActiveRequestSummary {
    pub trace_id: TraceId,
    pub request_id: RequestId,
    pub user_id: String,
    pub status: RequestStatus,
    pub text_preview: String,
    pub created_at: String,
    pub error_message: Option<String>,
    pub run_status: Option<RunStatus>,
    pub outbox_status: Option<OutboxStatus>,
    pub outbox_error_message: Option<String>,
    pub event_count: u64,
    pub last_event_kind: Option<GatewayEventKind>,
}

impl ActiveRequestSummary {
    pub fn display_status(&self) -> &'static str {
        match (self.status, self.outbox_status) {
            (_, Some(OutboxStatus::Failed)) => "reply_retrying",
            (_, Some(OutboxStatus::Pending)) if self.status.is_terminal() => "reply_pending",
            (RequestStatus::Accepted, _) => "queued",
            (RequestStatus::Running, _) => "running",
            (RequestStatus::Completed, _) => "completed",
            (RequestStatus::Failed, _) => "failed",
        }
    }

    pub fn is_cancellable(&self) -> bool {
        self.status == RequestStatus::Accepted
    }
}

#[derive(Debug, Clone)]
pub struct GatewayStatusSummary {
    pub active_count: usize,
    pub queued_count: usize,
    pub running_count: usize,
    pub retrying_outbox_count: usize,
    pub pending_outbox_count: usize,
    pub recent_trace_count: usize,
    pub last_trace: Option<TraceSummary>,
}

#[derive(Debug, Clone)]
pub enum CancelRequestOutcome {
    Cancelled(ActiveRequestSummary),
    AlreadyRunning(ActiveRequestSummary),
    NotFound,
}

#[async_trait]
pub trait TraceRepository: Send + Sync {
    async fn create_request(&self, request: &GatewayRequest) -> TraceResult<()>;
    async fn get_request(&self, request_id: &RequestId) -> TraceResult<Option<GatewayRequest>>;
    async fn update_request_status(
        &self,
        request_id: &RequestId,
        status: RequestStatus,
        error_message: Option<&str>,
    ) -> TraceResult<()>;
    async fn create_run(&self, run: &GatewayRun) -> TraceResult<()>;
    async fn finish_run(
        &self,
        run_id: &RunId,
        status: RunStatus,
        exit_code: Option<i32>,
        error_message: Option<&str>,
    ) -> TraceResult<()>;
    async fn enqueue_outbox(&self, outbox: &OutboxRecord) -> TraceResult<()>;
    async fn get_outbox(&self, outbox_id: &OutboxId) -> TraceResult<Option<OutboxRecord>>;
    async fn list_retryable_outbox(
        &self,
        platform: Option<&str>,
        limit: u32,
    ) -> TraceResult<Vec<OutboxRecord>>;
    async fn update_outbox_status(
        &self,
        outbox_id: &OutboxId,
        status: OutboxStatus,
        error_message: Option<&str>,
    ) -> TraceResult<()>;
    async fn append_event(&self, event: &NewGatewayEvent) -> TraceResult<()>;
    async fn list_events_for_trace(
        &self,
        trace_id: &TraceId,
        limit: u32,
    ) -> TraceResult<Vec<GatewayEvent>>;
    async fn list_recent_traces(
        &self,
        conversation: &ConversationKey,
        limit: u32,
    ) -> TraceResult<Vec<TraceSummary>>;
    async fn list_active_requests(
        &self,
        conversation: &ConversationKey,
        limit: u32,
    ) -> TraceResult<Vec<ActiveRequestSummary>>;

    /// Fail all requests stuck in Accepted or Running (orphaned by restart).
    /// Returns the count of swept requests.
    async fn sweep_stale_requests(&self, reason: &str) -> TraceResult<u64>;

    /// Fail all Accepted/Running requests for a specific conversation.
    /// Used when a conversation worker exits to clean up orphaned traces.
    async fn sweep_conversation_stale_requests(
        &self,
        conversation: &ConversationKey,
        reason: &str,
    ) -> TraceResult<u64>;

    /// Force-fail a request regardless of current status (unless already terminal).
    /// Returns `Ok(true)` if the request was transitioned, `Ok(false)` if already terminal.
    async fn force_fail_request(&self, trace_id: &TraceId, reason: &str) -> TraceResult<bool>;

    /// Dismiss all failed outbox entries for a request by marking them as `sent`.
    /// Used by `/retry dismiss` to clear stuck outbox entries without re-sending.
    async fn dismiss_failed_outbox(&self, request_id: &RequestId) -> TraceResult<()>;

    async fn gateway_status(
        &self,
        conversation: &ConversationKey,
    ) -> TraceResult<GatewayStatusSummary> {
        let active = self.list_active_requests(conversation, 20).await?;
        let recent = self.list_recent_traces(conversation, 10).await?;
        Ok(GatewayStatusSummary {
            active_count: active.len(),
            queued_count: active
                .iter()
                .filter(|request| request.status == RequestStatus::Accepted)
                .count(),
            running_count: active
                .iter()
                .filter(|request| request.status == RequestStatus::Running)
                .count(),
            retrying_outbox_count: active
                .iter()
                .filter(|request| request.outbox_status == Some(OutboxStatus::Failed))
                .count(),
            pending_outbox_count: active
                .iter()
                .filter(|request| request.outbox_status == Some(OutboxStatus::Pending))
                .count(),
            recent_trace_count: recent.len(),
            last_trace: recent.into_iter().next(),
        })
    }

    async fn cancel_accepted_request(
        &self,
        conversation: &ConversationKey,
        selector: &str,
        reason: &str,
    ) -> TraceResult<CancelRequestOutcome> {
        let active = self.list_active_requests(conversation, 50).await?;
        let Some(request) = active.into_iter().find(|request| {
            request.trace_id.as_str() == selector
                || request.request_id.as_str() == selector
                || request.trace_id.as_str().starts_with(selector)
                || request.request_id.as_str().starts_with(selector)
        }) else {
            return Ok(CancelRequestOutcome::NotFound);
        };

        if request.status == RequestStatus::Running {
            return Ok(CancelRequestOutcome::AlreadyRunning(request));
        }
        if request.status != RequestStatus::Accepted {
            return Ok(CancelRequestOutcome::NotFound);
        }

        self.update_request_status(&request.request_id, RequestStatus::Failed, Some(reason))
            .await?;
        self.append_event(&NewGatewayEvent {
            trace_id: request.trace_id.clone(),
            request_id: request.request_id.clone(),
            run_id: None,
            kind: GatewayEventKind::RequestCancelled,
            payload: serde_json::json!({ "reason": reason }),
        })
        .await?;

        let mut cancelled = request;
        cancelled.status = RequestStatus::Failed;
        cancelled.error_message = Some(reason.to_string());
        cancelled.last_event_kind = Some(GatewayEventKind::RequestCancelled);
        cancelled.event_count += 1;
        Ok(CancelRequestOutcome::Cancelled(cancelled))
    }
}

pub struct TraceWriter<'a> {
    repo: &'a dyn TraceRepository,
    trace_id: TraceId,
    request_id: RequestId,
}

impl<'a> TraceWriter<'a> {
    pub fn from_existing(
        repo: &'a (dyn TraceRepository + 'a),
        trace_id: TraceId,
        request_id: RequestId,
    ) -> Self {
        Self {
            repo,
            trace_id,
            request_id,
        }
    }

    pub async fn begin<R>(repo: &'a R, request: GatewayRequest) -> TraceResult<Self>
    where
        R: TraceRepository + 'a,
    {
        repo.create_request(&request).await?;
        let writer = Self {
            repo,
            trace_id: request.trace_id.clone(),
            request_id: request.request_id.clone(),
        };
        writer
            .append(
                GatewayEventKind::RequestReceived,
                serde_json::json!({
                    "conversation": request.conversation.to_string(),
                    "platform": request.conversation.platform(),
                    "chat_id": request.conversation.chat_id(),
                    "cli_profile": request.conversation.cli_profile(),
                    "platform_msg_id": request.platform_msg_id,
                    "user_id": request.user_id,
                }),
            )
            .await?;
        Ok(writer)
    }

    pub fn trace_id(&self) -> &TraceId {
        &self.trace_id
    }

    pub fn request_id(&self) -> &RequestId {
        &self.request_id
    }

    pub async fn append(
        &self,
        kind: GatewayEventKind,
        payload: serde_json::Value,
    ) -> TraceResult<()> {
        self.repo
            .append_event(&NewGatewayEvent {
                trace_id: self.trace_id.clone(),
                request_id: self.request_id.clone(),
                run_id: None,
                kind,
                payload,
            })
            .await
    }

    pub async fn mark_queued(&self, depth: usize) -> TraceResult<()> {
        self.append(
            GatewayEventKind::RequestQueued,
            serde_json::json!({ "queue_depth": depth }),
        )
        .await
    }

    pub async fn mark_running(&self) -> TraceResult<()> {
        self.append(GatewayEventKind::RequestRunning, serde_json::json!({}))
            .await
    }

    pub async fn mark_cancelled(&self, reason: &str) -> TraceResult<()> {
        self.append(
            GatewayEventKind::RequestCancelled,
            serde_json::json!({ "reason": reason }),
        )
        .await
    }

    pub async fn mark_shutdown(&self, reason: &str) -> TraceResult<()> {
        self.append(
            GatewayEventKind::RequestShutdown,
            serde_json::json!({ "reason": reason }),
        )
        .await
    }

    pub async fn start_run(
        &self,
        cli_profile: &str,
        session_id: Option<String>,
    ) -> TraceResult<RunId> {
        self.repo
            .update_request_status(&self.request_id, RequestStatus::Running, None)
            .await?;
        let run = GatewayRun::start(
            self.request_id.clone(),
            self.trace_id.clone(),
            cli_profile,
            session_id,
        );
        self.repo.create_run(&run).await?;
        self.repo
            .append_event(&NewGatewayEvent {
                trace_id: self.trace_id.clone(),
                request_id: self.request_id.clone(),
                run_id: Some(run.run_id.clone()),
                kind: GatewayEventKind::RunStarted,
                payload: serde_json::json!({
                    "cli_profile": run.cli_profile,
                    "session_id": run.session_id,
                }),
            })
            .await?;
        Ok(run.run_id)
    }

    pub async fn finish_run(
        &self,
        run_id: &RunId,
        status: RunStatus,
        exit_code: Option<i32>,
        error_message: Option<&str>,
    ) -> TraceResult<()> {
        self.repo
            .finish_run(run_id, status, exit_code, error_message)
            .await?;
        let kind = match status {
            RunStatus::Succeeded => GatewayEventKind::RunCompleted,
            RunStatus::Failed => GatewayEventKind::RunFailed,
            RunStatus::Started => GatewayEventKind::RunStarted,
        };
        self.repo
            .append_event(&NewGatewayEvent {
                trace_id: self.trace_id.clone(),
                request_id: self.request_id.clone(),
                run_id: Some(run_id.clone()),
                kind,
                payload: serde_json::json!({
                    "status": status.as_str(),
                    "exit_code": exit_code,
                    "error": error_message,
                }),
            })
            .await
    }

    pub async fn complete_request(&self) -> TraceResult<()> {
        self.repo
            .update_request_status(&self.request_id, RequestStatus::Completed, None)
            .await?;
        self.append(GatewayEventKind::RequestCompleted, serde_json::json!({}))
            .await
    }

    pub async fn fail_request(&self, error_message: &str) -> TraceResult<()> {
        self.repo
            .update_request_status(&self.request_id, RequestStatus::Failed, Some(error_message))
            .await?;
        self.append(
            GatewayEventKind::RequestFailed,
            serde_json::json!({"error": error_message}),
        )
        .await
    }

    pub async fn enqueue_outbox(
        &self,
        platform: &str,
        chat_id: &str,
        reply_token: Option<String>,
        body: &str,
    ) -> TraceResult<OutboxId> {
        let outbox = OutboxRecord::pending(
            self.request_id.clone(),
            self.trace_id.clone(),
            platform,
            chat_id,
            reply_token,
            body,
        );
        self.repo.enqueue_outbox(&outbox).await?;
        self.append(
            GatewayEventKind::OutboxQueued,
            serde_json::json!({
                "outbox_id": outbox.outbox_id.as_str(),
                "platform": outbox.platform,
                "chat_id": outbox.chat_id,
                "body_len": outbox.body.len(),
            }),
        )
        .await?;
        Ok(outbox.outbox_id)
    }

    pub async fn mark_outbox_sent(
        &self,
        outbox_id: &OutboxId,
        chunk_count: usize,
    ) -> TraceResult<()> {
        self.repo
            .update_outbox_status(outbox_id, OutboxStatus::Sent, None)
            .await?;
        self.append(
            GatewayEventKind::OutboxSent,
            serde_json::json!({
                "outbox_id": outbox_id.as_str(),
                "chunk_count": chunk_count,
            }),
        )
        .await?;
        // Try to mark request completed, but ignore failures — the request
        // may already be in a terminal state (e.g. failed by startup sweep)
        // and the outbox delivery still succeeded.
        let _ = self.complete_request().await;
        Ok(())
    }

    pub async fn mark_outbox_failed(
        &self,
        outbox_id: &OutboxId,
        error_message: &str,
        failed_chunk: usize,
    ) -> TraceResult<()> {
        self.repo
            .update_outbox_status(outbox_id, OutboxStatus::Failed, Some(error_message))
            .await?;
        self.append(
            GatewayEventKind::OutboxFailed,
            serde_json::json!({
                "outbox_id": outbox_id.as_str(),
                "error": error_message,
                "failed_chunk": failed_chunk,
            }),
        )
        .await
    }
}

#[derive(Clone)]
pub struct MysqlTraceRepository {
    pool: MySqlPool,
}

impl MysqlTraceRepository {
    pub fn new(pool: MySqlPool) -> Self {
        Self { pool }
    }
}

pub async fn ensure_schema(pool: &MySqlPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS gw_trace_requests (
            request_id VARCHAR(64) PRIMARY KEY,
            trace_id VARCHAR(64) NOT NULL,
            platform VARCHAR(32) NOT NULL,
            chat_id VARCHAR(128) NOT NULL,
            cli_profile VARCHAR(32) NOT NULL,
            platform_msg_id VARCHAR(128) NOT NULL,
            user_id VARCHAR(128) NOT NULL,
            text LONGTEXT NOT NULL,
            status VARCHAR(20) NOT NULL,
            error_message TEXT,
            created_at DATETIME(6) DEFAULT NOW(6),
            updated_at DATETIME(6) DEFAULT NOW(6),
            INDEX idx_trace_requests_trace (trace_id),
            INDEX idx_trace_requests_conversation (platform, chat_id, cli_profile, created_at)
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS gw_trace_runs (
            run_id VARCHAR(64) PRIMARY KEY,
            request_id VARCHAR(64) NOT NULL,
            trace_id VARCHAR(64) NOT NULL,
            cli_profile VARCHAR(32) NOT NULL,
            session_id VARCHAR(128),
            status VARCHAR(20) NOT NULL,
            exit_code INT,
            error_message TEXT,
            started_at DATETIME(6) DEFAULT NOW(6),
            finished_at DATETIME(6),
            INDEX idx_trace_runs_request (request_id),
            INDEX idx_trace_runs_trace (trace_id)
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS gw_trace_events (
            event_id BIGINT AUTO_INCREMENT PRIMARY KEY,
            trace_id VARCHAR(64) NOT NULL,
            request_id VARCHAR(64) NOT NULL,
            run_id VARCHAR(64),
            kind VARCHAR(64) NOT NULL,
            payload LONGTEXT NOT NULL,
            created_at DATETIME(6) DEFAULT NOW(6),
            INDEX idx_trace_events_trace (trace_id, event_id),
            INDEX idx_trace_events_request (request_id, event_id)
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS gw_trace_outbox (
            outbox_id VARCHAR(64) PRIMARY KEY,
            trace_id VARCHAR(64) NOT NULL,
            request_id VARCHAR(64) NOT NULL,
            platform VARCHAR(32) NOT NULL,
            chat_id VARCHAR(128) NOT NULL,
            reply_token VARCHAR(256),
            body LONGTEXT NOT NULL,
            status VARCHAR(20) NOT NULL,
            error_message TEXT,
            retry_count INT NOT NULL DEFAULT 0,
            created_at DATETIME(6) DEFAULT NOW(6),
            sent_at DATETIME(6),
            INDEX idx_trace_outbox_pending (status, created_at),
            INDEX idx_trace_outbox_trace (trace_id)
        )",
    )
    .execute(pool)
    .await?;

    // Migration: add retry_count column if missing (existing deployments)
    let _ =
        sqlx::query("ALTER TABLE gw_trace_outbox ADD COLUMN retry_count INT NOT NULL DEFAULT 0")
            .execute(pool)
            .await;

    Ok(())
}

#[async_trait]
impl TraceRepository for MysqlTraceRepository {
    async fn create_request(&self, request: &GatewayRequest) -> TraceResult<()> {
        sqlx::query(
            "INSERT INTO gw_trace_requests
             (request_id, trace_id, platform, chat_id, cli_profile, platform_msg_id, user_id, text, status, error_message)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(request.request_id.as_str())
        .bind(request.trace_id.as_str())
        .bind(request.conversation.platform())
        .bind(request.conversation.chat_id())
        .bind(request.conversation.cli_profile())
        .bind(&request.platform_msg_id)
        .bind(&request.user_id)
        .bind(&request.text)
        .bind(request.status.as_str())
        .bind(&request.error_message)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("create trace request failed: {e}"))?;
        Ok(())
    }

    async fn get_request(&self, request_id: &RequestId) -> TraceResult<Option<GatewayRequest>> {
        let row: Option<(
            String,
            String,
            String,
            String,
            String,
            String,
            String,
            String,
            String,
            Option<String>,
        )> = sqlx::query_as(
            "SELECT request_id, trace_id, platform, chat_id, cli_profile, platform_msg_id, user_id, text, status, error_message
             FROM gw_trace_requests WHERE request_id = ?",
        )
        .bind(request_id.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| format!("get trace request failed: {e}"))?;

        Ok(row.map(
            |(
                request_id,
                trace_id,
                platform,
                chat_id,
                cli_profile,
                platform_msg_id,
                user_id,
                text,
                status,
                error_message,
            )| GatewayRequest {
                request_id: RequestId::from(request_id),
                trace_id: TraceId::from(trace_id),
                conversation: ConversationKey::new(platform, chat_id, cli_profile),
                platform_msg_id,
                user_id,
                text,
                status: RequestStatus::parse(&status).unwrap_or(RequestStatus::Failed),
                error_message,
                created_at: Utc::now(),
            },
        ))
    }

    async fn update_request_status(
        &self,
        request_id: &RequestId,
        status: RequestStatus,
        error_message: Option<&str>,
    ) -> TraceResult<()> {
        let current: Option<(String,)> =
            sqlx::query_as("SELECT status FROM gw_trace_requests WHERE request_id = ?")
                .bind(request_id.as_str())
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| format!("load request status failed: {e}"))?;
        let Some((current,)) = current else {
            return Err(format!("trace request {request_id} not found"));
        };
        let current = RequestStatus::parse(&current).unwrap_or(RequestStatus::Failed);
        if !current.can_transition_to(status) {
            return Err(format!(
                "invalid request transition {} -> {}",
                current.as_str(),
                status.as_str()
            ));
        }

        let result = sqlx::query(
            "UPDATE gw_trace_requests SET status = ?, error_message = ?, updated_at = NOW(6) WHERE request_id = ? AND status = ?",
        )
        .bind(status.as_str())
        .bind(error_message)
        .bind(request_id.as_str())
        .bind(current.as_str())
        .execute(&self.pool)
        .await
        .map_err(|e| format!("update request status failed: {e}"))?;
        if result.rows_affected() != 1 {
            return Err(format!(
                "concurrent request status update for {request_id}; expected {}",
                current.as_str()
            ));
        }
        Ok(())
    }

    async fn create_run(&self, run: &GatewayRun) -> TraceResult<()> {
        sqlx::query(
            "INSERT INTO gw_trace_runs
             (run_id, request_id, trace_id, cli_profile, session_id, status, exit_code, error_message)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(run.run_id.as_str())
        .bind(run.request_id.as_str())
        .bind(run.trace_id.as_str())
        .bind(&run.cli_profile)
        .bind(&run.session_id)
        .bind(run.status.as_str())
        .bind(run.exit_code)
        .bind(&run.error_message)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("create trace run failed: {e}"))?;
        Ok(())
    }

    async fn finish_run(
        &self,
        run_id: &RunId,
        status: RunStatus,
        exit_code: Option<i32>,
        error_message: Option<&str>,
    ) -> TraceResult<()> {
        let current: Option<(String,)> =
            sqlx::query_as("SELECT status FROM gw_trace_runs WHERE run_id = ?")
                .bind(run_id.as_str())
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| format!("load run status failed: {e}"))?;
        let Some((current,)) = current else {
            return Err(format!("trace run {run_id} not found"));
        };
        let current = RunStatus::parse(&current).unwrap_or(RunStatus::Failed);
        if !current.can_transition_to(status) {
            return Err(format!(
                "invalid run transition {} -> {}",
                current.as_str(),
                status.as_str()
            ));
        }

        let result = sqlx::query(
            "UPDATE gw_trace_runs SET status = ?, exit_code = ?, error_message = ?, finished_at = NOW(6) WHERE run_id = ? AND status = ?",
        )
        .bind(status.as_str())
        .bind(exit_code)
        .bind(error_message)
        .bind(run_id.as_str())
        .bind(current.as_str())
        .execute(&self.pool)
        .await
        .map_err(|e| format!("finish trace run failed: {e}"))?;
        if result.rows_affected() != 1 {
            return Err(format!(
                "concurrent run status update for {run_id}; expected {}",
                current.as_str()
            ));
        }
        Ok(())
    }

    async fn enqueue_outbox(&self, outbox: &OutboxRecord) -> TraceResult<()> {
        sqlx::query(
            "INSERT INTO gw_trace_outbox
             (outbox_id, trace_id, request_id, platform, chat_id, reply_token, body, status, error_message)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(outbox.outbox_id.as_str())
        .bind(outbox.trace_id.as_str())
        .bind(outbox.request_id.as_str())
        .bind(&outbox.platform)
        .bind(&outbox.chat_id)
        .bind(&outbox.reply_token)
        .bind(&outbox.body)
        .bind(outbox.status.as_str())
        .bind(&outbox.error_message)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("enqueue outbox failed: {e}"))?;
        Ok(())
    }

    async fn get_outbox(&self, outbox_id: &OutboxId) -> TraceResult<Option<OutboxRecord>> {
        let row: Option<(
            String,
            String,
            String,
            String,
            String,
            Option<String>,
            String,
            String,
            Option<String>,
            i32,
        )> = sqlx::query_as(
            "SELECT outbox_id, request_id, trace_id, platform, chat_id, reply_token, body, status, error_message, retry_count
             FROM gw_trace_outbox WHERE outbox_id = ?",
        )
        .bind(outbox_id.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| format!("get outbox failed: {e}"))?;

        Ok(row.map(
            |(
                outbox_id,
                request_id,
                trace_id,
                platform,
                chat_id,
                reply_token,
                body,
                status,
                error_message,
                retry_count,
            )| OutboxRecord {
                outbox_id: OutboxId::from(outbox_id),
                request_id: RequestId::from(request_id),
                trace_id: TraceId::from(trace_id),
                platform,
                chat_id,
                reply_token,
                body,
                status: OutboxStatus::parse(&status).unwrap_or(OutboxStatus::Failed),
                error_message,
                retry_count: retry_count.max(0) as u32,
            },
        ))
    }

    async fn list_retryable_outbox(
        &self,
        platform: Option<&str>,
        limit: u32,
    ) -> TraceResult<Vec<OutboxRecord>> {
        let limit = limit.min(200);
        let max_retries = OUTBOX_MAX_RETRIES as i32;
        let rows: Vec<(
            String,
            String,
            String,
            String,
            String,
            Option<String>,
            String,
            String,
            Option<String>,
            i32,
        )> = if let Some(platform) = platform {
            sqlx::query_as(
                "SELECT outbox_id, request_id, trace_id, platform, chat_id, reply_token, body, status, error_message, retry_count
                 FROM gw_trace_outbox
                 WHERE platform = ? AND status IN ('pending', 'failed')
                   AND retry_count < ?
                   AND created_at > DATE_SUB(NOW(6), INTERVAL 1 HOUR)
                 ORDER BY created_at ASC
                 LIMIT ?",
            )
            .bind(platform)
            .bind(max_retries)
            .bind(limit)
            .fetch_all(&self.pool)
            .await
        } else {
            sqlx::query_as(
                "SELECT outbox_id, request_id, trace_id, platform, chat_id, reply_token, body, status, error_message, retry_count
                 FROM gw_trace_outbox
                 WHERE status IN ('pending', 'failed')
                   AND retry_count < ?
                   AND created_at > DATE_SUB(NOW(6), INTERVAL 1 HOUR)
                 ORDER BY created_at ASC
                 LIMIT ?",
            )
            .bind(max_retries)
            .bind(limit)
            .fetch_all(&self.pool)
            .await
        }
        .map_err(|e| format!("list retryable outbox failed: {e}"))?;

        Ok(rows
            .into_iter()
            .map(
                |(
                    outbox_id,
                    request_id,
                    trace_id,
                    platform,
                    chat_id,
                    reply_token,
                    body,
                    status,
                    error_message,
                    retry_count,
                )| OutboxRecord {
                    outbox_id: OutboxId::from(outbox_id),
                    request_id: RequestId::from(request_id),
                    trace_id: TraceId::from(trace_id),
                    platform,
                    chat_id,
                    reply_token,
                    body,
                    status: OutboxStatus::parse(&status).unwrap_or(OutboxStatus::Failed),
                    error_message,
                    retry_count: retry_count.max(0) as u32,
                },
            )
            .collect())
    }

    async fn update_outbox_status(
        &self,
        outbox_id: &OutboxId,
        status: OutboxStatus,
        error_message: Option<&str>,
    ) -> TraceResult<()> {
        let current: Option<(String,)> =
            sqlx::query_as("SELECT status FROM gw_trace_outbox WHERE outbox_id = ?")
                .bind(outbox_id.as_str())
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| format!("load outbox status failed: {e}"))?;
        let Some((current,)) = current else {
            return Err(format!("outbox {outbox_id} not found"));
        };
        let current = OutboxStatus::parse(&current).unwrap_or(OutboxStatus::Failed);
        if !current.can_transition_to(status) {
            return Err(format!(
                "invalid outbox transition {} -> {}",
                current.as_str(),
                status.as_str()
            ));
        }
        if status == OutboxStatus::Sent {
            let result = sqlx::query(
                "UPDATE gw_trace_outbox SET status = ?, error_message = ?, sent_at = NOW(6) WHERE outbox_id = ? AND status = ?",
            )
            .bind(status.as_str())
            .bind(error_message)
            .bind(outbox_id.as_str())
            .bind(current.as_str())
            .execute(&self.pool)
            .await
            .map_err(|e| format!("update outbox status failed: {e}"))?;
            if result.rows_affected() != 1 {
                return Err(format!(
                    "concurrent outbox status update for {outbox_id}; expected {}",
                    current.as_str()
                ));
            }
        } else if status == OutboxStatus::Failed {
            // Increment retry_count on failure; auto-expire if exhausted
            let result = sqlx::query(
                "UPDATE gw_trace_outbox SET status = ?, error_message = ?, retry_count = retry_count + 1
                 WHERE outbox_id = ? AND status = ?",
            )
            .bind(status.as_str())
            .bind(error_message)
            .bind(outbox_id.as_str())
            .bind(current.as_str())
            .execute(&self.pool)
            .await
            .map_err(|e| format!("update outbox status failed: {e}"))?;
            if result.rows_affected() != 1 {
                return Err(format!(
                    "concurrent outbox status update for {outbox_id}; expected {}",
                    current.as_str()
                ));
            }
        } else {
            let result = sqlx::query(
                "UPDATE gw_trace_outbox SET status = ?, error_message = ? WHERE outbox_id = ? AND status = ?",
            )
            .bind(status.as_str())
            .bind(error_message)
            .bind(outbox_id.as_str())
            .bind(current.as_str())
            .execute(&self.pool)
            .await
            .map_err(|e| format!("update outbox status failed: {e}"))?;
            if result.rows_affected() != 1 {
                return Err(format!(
                    "concurrent outbox status update for {outbox_id}; expected {}",
                    current.as_str()
                ));
            }
        }
        Ok(())
    }

    async fn append_event(&self, event: &NewGatewayEvent) -> TraceResult<()> {
        let payload = serde_json::to_string(&event.payload)
            .map_err(|e| format!("serialize trace event payload failed: {e}"))?;
        sqlx::query(
            "INSERT INTO gw_trace_events (trace_id, request_id, run_id, kind, payload)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(event.trace_id.as_str())
        .bind(event.request_id.as_str())
        .bind(event.run_id.as_ref().map(|id| id.as_str()))
        .bind(event.kind.as_str())
        .bind(payload)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("append trace event failed: {e}"))?;
        Ok(())
    }

    async fn list_events_for_trace(
        &self,
        trace_id: &TraceId,
        limit: u32,
    ) -> TraceResult<Vec<GatewayEvent>> {
        let rows: Vec<(i64, String, String, Option<String>, String, String, String)> =
            sqlx::query_as(
                "SELECT event_id, trace_id, request_id, run_id, kind, payload, CAST(created_at AS CHAR)
                 FROM gw_trace_events WHERE trace_id = ? ORDER BY event_id ASC LIMIT ?",
            )
            .bind(trace_id.as_str())
            .bind(limit.min(200))
            .fetch_all(&self.pool)
            .await
            .map_err(|e| format!("list trace events failed: {e}"))?;
        Ok(rows
            .into_iter()
            .map(
                |(sequence, trace_id, request_id, run_id, kind, payload, created_at)| {
                    GatewayEvent {
                        sequence,
                        trace_id: TraceId::from(trace_id),
                        request_id: RequestId::from(request_id),
                        run_id: run_id.map(RunId::from),
                        kind: GatewayEventKind::parse(&kind),
                        payload: serde_json::from_str(&payload).unwrap_or(serde_json::Value::Null),
                        created_at,
                    }
                },
            )
            .collect())
    }

    async fn list_recent_traces(
        &self,
        conversation: &ConversationKey,
        limit: u32,
    ) -> TraceResult<Vec<TraceSummary>> {
        let rows: Vec<(String, String, String, String, String, i64)> = sqlx::query_as(
            "SELECT r.trace_id, r.request_id, r.status,
                    CASE WHEN CHAR_LENGTH(r.text) > 120 THEN CONCAT(SUBSTRING(r.text, 1, 120), '…') ELSE r.text END,
                    CAST(r.created_at AS CHAR), COUNT(e.event_id)
             FROM gw_trace_requests r
             LEFT JOIN gw_trace_events e ON e.trace_id = r.trace_id
             WHERE r.platform = ? AND r.chat_id = ? AND r.cli_profile = ?
             GROUP BY r.trace_id, r.request_id, r.status, r.text, r.created_at
             ORDER BY r.created_at DESC
             LIMIT ?",
        )
        .bind(conversation.platform())
        .bind(conversation.chat_id())
        .bind(conversation.cli_profile())
        .bind(limit.min(50))
        .fetch_all(&self.pool)
        .await
        .map_err(|e| format!("list recent traces failed: {e}"))?;

        Ok(rows
            .into_iter()
            .map(
                |(trace_id, request_id, status, text_preview, created_at, event_count)| {
                    TraceSummary {
                        trace_id: TraceId::from(trace_id),
                        request_id: RequestId::from(request_id),
                        status: RequestStatus::parse(&status).unwrap_or(RequestStatus::Failed),
                        text_preview,
                        created_at,
                        event_count: event_count.max(0) as u64,
                    }
                },
            )
            .collect())
    }

    async fn list_active_requests(
        &self,
        conversation: &ConversationKey,
        limit: u32,
    ) -> TraceResult<Vec<ActiveRequestSummary>> {
        let rows: Vec<(
            String,
            String,
            String,
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            i64,
            Option<String>,
        )> = sqlx::query_as(
            "SELECT r.trace_id, r.request_id, r.user_id, r.status,
                    CASE WHEN CHAR_LENGTH(r.text) > 120 THEN CONCAT(SUBSTRING(r.text, 1, 120), '…') ELSE r.text END,
                    CAST(r.created_at AS CHAR), r.error_message,
                    (SELECT rr.status FROM gw_trace_runs rr WHERE rr.request_id = r.request_id ORDER BY rr.started_at DESC LIMIT 1),
                    (SELECT o.status FROM gw_trace_outbox o WHERE o.request_id = r.request_id AND o.status IN ('pending', 'failed') AND o.retry_count < ? ORDER BY o.created_at DESC LIMIT 1),
                    (SELECT o.error_message FROM gw_trace_outbox o WHERE o.request_id = r.request_id AND o.status IN ('pending', 'failed') AND o.retry_count < ? ORDER BY o.created_at DESC LIMIT 1),
                    (SELECT COUNT(*) FROM gw_trace_events e WHERE e.trace_id = r.trace_id),
                    (SELECT e.kind FROM gw_trace_events e WHERE e.trace_id = r.trace_id ORDER BY e.event_id DESC LIMIT 1)
             FROM gw_trace_requests r
             WHERE r.platform = ? AND r.chat_id = ? AND r.cli_profile = ?
               AND (r.status IN ('accepted', 'running')
                    OR EXISTS (SELECT 1 FROM gw_trace_outbox o WHERE o.request_id = r.request_id AND o.status IN ('pending', 'failed') AND o.retry_count < ?))
             ORDER BY CASE r.status WHEN 'running' THEN 0 WHEN 'accepted' THEN 1 ELSE 2 END, r.created_at ASC
             LIMIT ?",
        )
        .bind(OUTBOX_MAX_RETRIES as i32)
        .bind(OUTBOX_MAX_RETRIES as i32)
        .bind(conversation.platform())
        .bind(conversation.chat_id())
        .bind(conversation.cli_profile())
        .bind(OUTBOX_MAX_RETRIES as i32)
        .bind(limit.min(100))
        .fetch_all(&self.pool)
        .await
        .map_err(|e| format!("list active requests failed: {e}"))?;

        Ok(rows
            .into_iter()
            .map(
                |(
                    trace_id,
                    request_id,
                    user_id,
                    status,
                    text_preview,
                    created_at,
                    error_message,
                    run_status,
                    outbox_status,
                    outbox_error_message,
                    event_count,
                    last_event_kind,
                )| ActiveRequestSummary {
                    trace_id: TraceId::from(trace_id),
                    request_id: RequestId::from(request_id),
                    user_id,
                    status: RequestStatus::parse(&status).unwrap_or(RequestStatus::Failed),
                    text_preview,
                    created_at,
                    error_message,
                    run_status: run_status.as_deref().and_then(RunStatus::parse),
                    outbox_status: outbox_status.as_deref().and_then(OutboxStatus::parse),
                    outbox_error_message,
                    event_count: event_count.max(0) as u64,
                    last_event_kind: last_event_kind.as_deref().map(GatewayEventKind::parse),
                },
            )
            .collect())
    }

    async fn sweep_stale_requests(&self, reason: &str) -> TraceResult<u64> {
        let result = sqlx::query(
            "UPDATE gw_trace_requests SET status = 'failed', error_message = ?, updated_at = NOW(6)
             WHERE status IN ('accepted', 'running')",
        )
        .bind(reason)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("sweep stale requests failed: {e}"))?;
        Ok(result.rows_affected())
    }

    async fn sweep_conversation_stale_requests(
        &self,
        conversation: &ConversationKey,
        reason: &str,
    ) -> TraceResult<u64> {
        let result = sqlx::query(
            "UPDATE gw_trace_requests SET status = 'failed', error_message = ?, updated_at = NOW(6)
             WHERE platform = ? AND chat_id = ? AND cli_profile = ? AND status IN ('accepted', 'running')",
        )
        .bind(reason)
        .bind(conversation.platform())
        .bind(conversation.chat_id())
        .bind(conversation.cli_profile())
        .execute(&self.pool)
        .await
        .map_err(|e| format!("sweep conversation stale requests failed: {e}"))?;
        Ok(result.rows_affected())
    }

    async fn force_fail_request(&self, trace_id: &TraceId, reason: &str) -> TraceResult<bool> {
        let result = sqlx::query(
            "UPDATE gw_trace_requests SET status = 'failed', error_message = ?, updated_at = NOW(6)
             WHERE trace_id = ? AND status IN ('accepted', 'running')",
        )
        .bind(reason)
        .bind(trace_id.as_str())
        .execute(&self.pool)
        .await
        .map_err(|e| format!("force fail request failed: {e}"))?;
        Ok(result.rows_affected() > 0)
    }

    async fn dismiss_failed_outbox(&self, request_id: &RequestId) -> TraceResult<()> {
        sqlx::query(
            "UPDATE gw_trace_outbox SET status = 'sent', sent_at = NOW(6)
             WHERE request_id = ? AND status = 'failed'",
        )
        .bind(request_id.as_str())
        .execute(&self.pool)
        .await
        .map_err(|e| format!("dismiss failed outbox failed: {e}"))?;
        Ok(())
    }
}

#[cfg(test)]
#[derive(Default)]
pub struct InMemoryTraceRepository {
    state: std::sync::Mutex<MemoryState>,
}

#[cfg(test)]
#[derive(Default)]
struct MemoryState {
    next_event_id: i64,
    next_request_order: u64,
    requests: std::collections::HashMap<RequestId, (GatewayRequest, u64)>,
    runs: std::collections::HashMap<RunId, GatewayRun>,
    outbox: std::collections::HashMap<OutboxId, OutboxRecord>,
    events: Vec<GatewayEvent>,
}

#[cfg(test)]
#[async_trait]
impl TraceRepository for InMemoryTraceRepository {
    async fn create_request(&self, request: &GatewayRequest) -> TraceResult<()> {
        let mut state = self.state.lock().unwrap();
        state.next_request_order += 1;
        let order = state.next_request_order;
        state
            .requests
            .insert(request.request_id.clone(), (request.clone(), order));
        Ok(())
    }

    async fn get_request(&self, request_id: &RequestId) -> TraceResult<Option<GatewayRequest>> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .requests
            .get(request_id)
            .map(|(request, _)| request.clone()))
    }

    async fn update_request_status(
        &self,
        request_id: &RequestId,
        status: RequestStatus,
        error_message: Option<&str>,
    ) -> TraceResult<()> {
        let mut state = self.state.lock().unwrap();
        let Some((request, _)) = state.requests.get_mut(request_id) else {
            return Err(format!("trace request {request_id} not found"));
        };
        if !request.status.can_transition_to(status) {
            return Err(format!(
                "invalid request transition {} -> {}",
                request.status.as_str(),
                status.as_str()
            ));
        }
        request.status = status;
        request.error_message = error_message.map(str::to_string);
        Ok(())
    }

    async fn create_run(&self, run: &GatewayRun) -> TraceResult<()> {
        self.state
            .lock()
            .unwrap()
            .runs
            .insert(run.run_id.clone(), run.clone());
        Ok(())
    }

    async fn finish_run(
        &self,
        run_id: &RunId,
        status: RunStatus,
        exit_code: Option<i32>,
        error_message: Option<&str>,
    ) -> TraceResult<()> {
        let mut state = self.state.lock().unwrap();
        let Some(run) = state.runs.get_mut(run_id) else {
            return Err(format!("trace run {run_id} not found"));
        };
        if !run.status.can_transition_to(status) {
            return Err(format!(
                "invalid run transition {} -> {}",
                run.status.as_str(),
                status.as_str()
            ));
        }
        run.status = status;
        run.exit_code = exit_code;
        run.error_message = error_message.map(str::to_string);
        Ok(())
    }

    async fn enqueue_outbox(&self, outbox: &OutboxRecord) -> TraceResult<()> {
        self.state
            .lock()
            .unwrap()
            .outbox
            .insert(outbox.outbox_id.clone(), outbox.clone());
        Ok(())
    }

    async fn get_outbox(&self, outbox_id: &OutboxId) -> TraceResult<Option<OutboxRecord>> {
        Ok(self.state.lock().unwrap().outbox.get(outbox_id).cloned())
    }

    async fn list_retryable_outbox(
        &self,
        platform: Option<&str>,
        limit: u32,
    ) -> TraceResult<Vec<OutboxRecord>> {
        let mut rows: Vec<_> = self
            .state
            .lock()
            .unwrap()
            .outbox
            .values()
            .filter(|outbox| {
                matches!(outbox.status, OutboxStatus::Pending | OutboxStatus::Failed)
                    && outbox.retry_count < OUTBOX_MAX_RETRIES
                    && platform
                        .map(|platform| outbox.platform == platform)
                        .unwrap_or(true)
            })
            .cloned()
            .collect();
        rows.sort_by(|a, b| a.outbox_id.as_str().cmp(b.outbox_id.as_str()));
        rows.truncate(limit as usize);
        Ok(rows)
    }

    async fn update_outbox_status(
        &self,
        outbox_id: &OutboxId,
        status: OutboxStatus,
        error_message: Option<&str>,
    ) -> TraceResult<()> {
        let mut state = self.state.lock().unwrap();
        let Some(outbox) = state.outbox.get_mut(outbox_id) else {
            return Err(format!("outbox {outbox_id} not found"));
        };
        if !outbox.status.can_transition_to(status) {
            return Err(format!(
                "invalid outbox transition {} -> {}",
                outbox.status.as_str(),
                status.as_str()
            ));
        }
        outbox.status = status;
        outbox.error_message = error_message.map(str::to_string);
        if status == OutboxStatus::Failed {
            outbox.retry_count += 1;
        }
        Ok(())
    }

    async fn append_event(&self, event: &NewGatewayEvent) -> TraceResult<()> {
        let mut state = self.state.lock().unwrap();
        state.next_event_id += 1;
        let sequence = state.next_event_id;
        state.events.push(GatewayEvent {
            sequence,
            trace_id: event.trace_id.clone(),
            request_id: event.request_id.clone(),
            run_id: event.run_id.clone(),
            kind: event.kind,
            payload: event.payload.clone(),
            created_at: sequence.to_string(),
        });
        Ok(())
    }

    async fn list_events_for_trace(
        &self,
        trace_id: &TraceId,
        limit: u32,
    ) -> TraceResult<Vec<GatewayEvent>> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .events
            .iter()
            .filter(|event| &event.trace_id == trace_id)
            .take(limit as usize)
            .cloned()
            .collect())
    }

    async fn list_recent_traces(
        &self,
        conversation: &ConversationKey,
        limit: u32,
    ) -> TraceResult<Vec<TraceSummary>> {
        let state = self.state.lock().unwrap();
        let mut requests: Vec<_> = state
            .requests
            .values()
            .filter(|(request, _)| &request.conversation == conversation)
            .collect();
        requests.sort_by_key(|(_, order)| std::cmp::Reverse(*order));
        Ok(requests
            .into_iter()
            .take(limit as usize)
            .map(|(request, order)| TraceSummary {
                trace_id: request.trace_id.clone(),
                request_id: request.request_id.clone(),
                status: request.status,
                text_preview: request.text.chars().take(120).collect(),
                created_at: order.to_string(),
                event_count: state
                    .events
                    .iter()
                    .filter(|event| event.trace_id == request.trace_id)
                    .count() as u64,
            })
            .collect())
    }

    async fn list_active_requests(
        &self,
        conversation: &ConversationKey,
        limit: u32,
    ) -> TraceResult<Vec<ActiveRequestSummary>> {
        let state = self.state.lock().unwrap();
        let mut requests: Vec<_> = state
            .requests
            .values()
            .filter(|(request, _)| &request.conversation == conversation)
            .filter(|(request, _)| {
                matches!(
                    request.status,
                    RequestStatus::Accepted | RequestStatus::Running
                ) || state.outbox.values().any(|outbox| {
                    outbox.request_id == request.request_id
                        && matches!(outbox.status, OutboxStatus::Pending | OutboxStatus::Failed)
                        && outbox.retry_count < OUTBOX_MAX_RETRIES
                })
            })
            .collect();
        requests.sort_by_key(|(request, order)| {
            let status_order = match request.status {
                RequestStatus::Running => 0_u8,
                RequestStatus::Accepted => 1,
                _ => 2,
            };
            (status_order, *order)
        });

        Ok(requests
            .into_iter()
            .take(limit as usize)
            .map(|(request, order)| {
                let run_status = state
                    .runs
                    .values()
                    .find(|run| run.request_id == request.request_id)
                    .map(|run| run.status);
                let outbox = state
                    .outbox
                    .values()
                    .find(|outbox| {
                        outbox.request_id == request.request_id
                            && matches!(outbox.status, OutboxStatus::Pending | OutboxStatus::Failed)
                            && outbox.retry_count < OUTBOX_MAX_RETRIES
                    })
                    .cloned();
                let request_events: Vec<_> = state
                    .events
                    .iter()
                    .filter(|event| event.trace_id == request.trace_id)
                    .collect();
                ActiveRequestSummary {
                    trace_id: request.trace_id.clone(),
                    request_id: request.request_id.clone(),
                    user_id: request.user_id.clone(),
                    status: request.status,
                    text_preview: request.text.chars().take(120).collect(),
                    created_at: order.to_string(),
                    error_message: request.error_message.clone(),
                    run_status,
                    outbox_status: outbox.as_ref().map(|outbox| outbox.status),
                    outbox_error_message: outbox.and_then(|outbox| outbox.error_message),
                    event_count: request_events.len() as u64,
                    last_event_kind: request_events.last().map(|event| event.kind),
                }
            })
            .collect())
    }

    async fn sweep_stale_requests(&self, reason: &str) -> TraceResult<u64> {
        let mut state = self.state.lock().unwrap();
        let mut count = 0u64;
        for (request, _) in state.requests.values_mut() {
            if request.status == RequestStatus::Accepted || request.status == RequestStatus::Running
            {
                request.status = RequestStatus::Failed;
                request.error_message = Some(reason.to_string());
                count += 1;
            }
        }
        Ok(count)
    }

    async fn sweep_conversation_stale_requests(
        &self,
        conversation: &ConversationKey,
        reason: &str,
    ) -> TraceResult<u64> {
        let mut state = self.state.lock().unwrap();
        let mut count = 0u64;
        for (request, _) in state.requests.values_mut() {
            if &request.conversation == conversation
                && (request.status == RequestStatus::Accepted
                    || request.status == RequestStatus::Running)
            {
                request.status = RequestStatus::Failed;
                request.error_message = Some(reason.to_string());
                count += 1;
            }
        }
        Ok(count)
    }

    async fn force_fail_request(&self, trace_id: &TraceId, reason: &str) -> TraceResult<bool> {
        let mut state = self.state.lock().unwrap();
        for (request, _) in state.requests.values_mut() {
            if request.trace_id == *trace_id
                && (request.status == RequestStatus::Accepted
                    || request.status == RequestStatus::Running)
            {
                request.status = RequestStatus::Failed;
                request.error_message = Some(reason.to_string());
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn dismiss_failed_outbox(&self, request_id: &RequestId) -> TraceResult<()> {
        let mut state = self.state.lock().unwrap();
        for outbox in state.outbox.values_mut() {
            if outbox.request_id == *request_id && outbox.status == OutboxStatus::Failed {
                outbox.status = OutboxStatus::Sent;
                outbox.error_message = None;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_ids_are_structured_and_serializable() {
        let request_id = RequestId::new();
        let trace_id = TraceId::new();
        let run_id = RunId::new();
        let conversation = ConversationKey::new("wecom", "chat-42", "astra");

        assert_ne!(request_id.as_str(), trace_id.as_str());
        assert_eq!(conversation.platform(), "wecom");
        assert_eq!(conversation.chat_id(), "chat-42");
        assert_eq!(conversation.cli_profile(), "astra");

        let encoded = serde_json::to_string(&run_id).expect("run id serializes");
        let decoded: RunId = serde_json::from_str(&encoded).expect("run id deserializes");
        assert_eq!(decoded, run_id);
    }

    #[test]
    fn request_state_machine_allows_only_forward_terminal_transitions() {
        assert!(RequestStatus::Accepted.can_transition_to(RequestStatus::Running));
        assert!(RequestStatus::Accepted.can_transition_to(RequestStatus::Failed));
        assert!(RequestStatus::Running.can_transition_to(RequestStatus::Completed));
        assert!(RequestStatus::Running.can_transition_to(RequestStatus::Failed));
        assert!(RequestStatus::Accepted.can_transition_to(RequestStatus::Completed));
        assert!(!RequestStatus::Completed.can_transition_to(RequestStatus::Running));
        assert!(!RequestStatus::Failed.can_transition_to(RequestStatus::Completed));
    }

    #[tokio::test]
    async fn trace_writer_records_append_only_events_and_updates_request() {
        let repo = InMemoryTraceRepository::default();
        let request = GatewayRequest::new(
            ConversationKey::new("wecom", "chat-42", "astra"),
            "msg-1",
            "user-1",
            "hello",
        );
        let request_id = request.request_id.clone();
        let trace_id = request.trace_id.clone();

        let writer = TraceWriter::begin(&repo, request).await.unwrap();
        writer.mark_queued(0).await.unwrap();
        writer.mark_running().await.unwrap();
        let run_id = writer
            .start_run("astra", Some("session-before".into()))
            .await
            .unwrap();
        writer
            .append(
                GatewayEventKind::CliProgress,
                serde_json::json!({"tool_count": 1}),
            )
            .await
            .unwrap();
        writer
            .finish_run(&run_id, RunStatus::Succeeded, Some(0), None)
            .await
            .unwrap();
        writer.complete_request().await.unwrap();

        let stored = repo.get_request(&request_id).await.unwrap().unwrap();
        assert_eq!(stored.status, RequestStatus::Completed);

        let events = repo.list_events_for_trace(&trace_id, 20).await.unwrap();
        let kinds: Vec<_> = events.iter().map(|event| event.kind).collect();
        assert_eq!(
            kinds,
            vec![
                GatewayEventKind::RequestReceived,
                GatewayEventKind::RequestQueued,
                GatewayEventKind::RequestRunning,
                GatewayEventKind::RunStarted,
                GatewayEventKind::CliProgress,
                GatewayEventKind::RunCompleted,
                GatewayEventKind::RequestCompleted,
            ]
        );
        assert!(events.iter().all(|event| event.trace_id == trace_id));
        assert!(events.iter().all(|event| event.request_id == request_id));
    }

    #[tokio::test]
    async fn trace_writer_records_stale_session_retry_as_new_run() {
        let repo = InMemoryTraceRepository::default();
        let request = GatewayRequest::new(
            ConversationKey::new("wecom", "chat-42", "astra"),
            "msg-1",
            "user-1",
            "hello",
        );
        let trace_id = request.trace_id.clone();

        let writer = TraceWriter::begin(&repo, request).await.unwrap();
        let first_run = writer
            .start_run("astra", Some("stale-session".into()))
            .await
            .unwrap();
        writer
            .finish_run(
                &first_run,
                RunStatus::Failed,
                Some(1),
                Some("session not found"),
            )
            .await
            .unwrap();
        let retry_run = writer.start_run("astra", None).await.unwrap();
        writer
            .finish_run(&retry_run, RunStatus::Succeeded, Some(0), None)
            .await
            .unwrap();
        writer.complete_request().await.unwrap();

        assert_ne!(first_run, retry_run);
        let state = repo.state.lock().unwrap();
        assert_eq!(state.runs[&first_run].status, RunStatus::Failed);
        assert_eq!(state.runs[&retry_run].status, RunStatus::Succeeded);
        let run_events = state
            .events
            .iter()
            .filter(|event| event.trace_id == trace_id && event.run_id.is_some())
            .count();
        assert_eq!(run_events, 4);
    }

    #[tokio::test]
    async fn repository_lists_recent_traces_by_conversation() {
        let repo = InMemoryTraceRepository::default();
        let conversation = ConversationKey::new("wecom", "chat-42", "astra");
        let other_conversation = ConversationKey::new("wecom", "chat-99", "astra");

        let first = GatewayRequest::new(conversation.clone(), "msg-1", "user-1", "hello");
        let second = GatewayRequest::new(conversation.clone(), "msg-2", "user-1", "again");
        let third = GatewayRequest::new(other_conversation, "msg-3", "user-1", "elsewhere");
        let first_trace = first.trace_id.clone();
        let second_trace = second.trace_id.clone();

        TraceWriter::begin(&repo, first).await.unwrap();
        TraceWriter::begin(&repo, second).await.unwrap();
        TraceWriter::begin(&repo, third).await.unwrap();

        let summaries = repo.list_recent_traces(&conversation, 10).await.unwrap();
        let traces: Vec<_> = summaries
            .iter()
            .map(|summary| summary.trace_id.clone())
            .collect();

        assert_eq!(traces, vec![second_trace, first_trace]);
        assert_eq!(summaries[0].event_count, 1);
    }

    #[tokio::test]
    async fn outbox_transitions_ack_only_after_send_and_records_events() {
        let repo = InMemoryTraceRepository::default();
        let request = GatewayRequest::new(
            ConversationKey::new("wecom", "chat-42", "astra"),
            "msg-1",
            "user-1",
            "hello",
        );
        let request_id = request.request_id.clone();
        let trace_id = request.trace_id.clone();
        let writer = TraceWriter::begin(&repo, request).await.unwrap();
        let run_id = writer
            .start_run("astra", Some("session-1".into()))
            .await
            .unwrap();
        writer
            .finish_run(&run_id, RunStatus::Succeeded, Some(0), None)
            .await
            .unwrap();

        let outbox_id = writer
            .enqueue_outbox("wecom", "chat-42", Some("reply-token".into()), "body")
            .await
            .unwrap();

        let queued_request = repo.get_request(&request_id).await.unwrap().unwrap();
        assert_eq!(queued_request.status, RequestStatus::Running);
        assert_eq!(
            repo.get_outbox(&outbox_id).await.unwrap().unwrap().status,
            OutboxStatus::Pending
        );

        writer.mark_outbox_sent(&outbox_id, 1).await.unwrap();

        let sent_request = repo.get_request(&request_id).await.unwrap().unwrap();
        assert_eq!(sent_request.status, RequestStatus::Completed);
        let outbox = repo.get_outbox(&outbox_id).await.unwrap().unwrap();
        assert_eq!(outbox.status, OutboxStatus::Sent);
        assert_eq!(outbox.platform, "wecom");
        assert_eq!(outbox.chat_id, "chat-42");
        assert_eq!(outbox.reply_token.as_deref(), Some("reply-token"));
        assert_eq!(outbox.body, "body");

        let events = repo.list_events_for_trace(&trace_id, 20).await.unwrap();
        let kinds: Vec<_> = events.iter().map(|event| event.kind).collect();
        assert!(kinds.contains(&GatewayEventKind::OutboxQueued));
        assert!(kinds.contains(&GatewayEventKind::OutboxSent));
        assert!(kinds.contains(&GatewayEventKind::RequestCompleted));
    }

    #[tokio::test]
    async fn failed_outbox_send_remains_retryable_and_can_later_ack() {
        let repo = InMemoryTraceRepository::default();
        let request = GatewayRequest::new(
            ConversationKey::new("wecom", "chat-42", "astra"),
            "msg-1",
            "user-1",
            "hello",
        );
        let request_id = request.request_id.clone();
        let writer = TraceWriter::begin(&repo, request).await.unwrap();
        let run_id = writer.start_run("astra", None).await.unwrap();
        writer
            .finish_run(&run_id, RunStatus::Succeeded, Some(0), None)
            .await
            .unwrap();
        let outbox_id = writer
            .enqueue_outbox("wecom", "chat-42", None, "retry me")
            .await
            .unwrap();

        writer
            .mark_outbox_failed(&outbox_id, "writer closed", 0)
            .await
            .unwrap();

        let failed = repo.get_outbox(&outbox_id).await.unwrap().unwrap();
        assert_eq!(failed.status, OutboxStatus::Failed);
        assert_eq!(failed.error_message.as_deref(), Some("writer closed"));
        assert_eq!(
            repo.get_request(&request_id).await.unwrap().unwrap().status,
            RequestStatus::Running
        );
        let retryable = repo.list_retryable_outbox(Some("wecom"), 10).await.unwrap();
        assert_eq!(
            retryable
                .iter()
                .map(|row| row.outbox_id.clone())
                .collect::<Vec<_>>(),
            vec![outbox_id.clone()]
        );

        writer.mark_outbox_sent(&outbox_id, 1).await.unwrap();
        assert_eq!(
            repo.get_outbox(&outbox_id).await.unwrap().unwrap().status,
            OutboxStatus::Sent
        );
        assert!(
            repo.list_retryable_outbox(Some("wecom"), 10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn sweep_stale_requests_fails_accepted_and_running() {
        let repo = InMemoryTraceRepository::default();
        let conv = ConversationKey::new("wecom", "chat-1", "astra");

        // Create one Accepted, one Running
        let req1 = GatewayRequest::new(conv.clone(), "msg-1", "u1", "hello");
        let req1_id = req1.request_id.clone();
        TraceWriter::begin(&repo, req1).await.unwrap();

        let req2 = GatewayRequest::new(conv.clone(), "msg-2", "u1", "world");
        let req2_id = req2.request_id.clone();
        let writer2 = TraceWriter::begin(&repo, req2).await.unwrap();
        writer2.mark_running().await.unwrap();

        // Sweep
        let swept = repo
            .sweep_stale_requests("gateway restarted")
            .await
            .unwrap();
        assert_eq!(swept, 2);

        // Both should be Failed
        let r1 = repo.get_request(&req1_id).await.unwrap().unwrap();
        assert_eq!(r1.status, RequestStatus::Failed);
        assert_eq!(r1.error_message.as_deref(), Some("gateway restarted"));

        let r2 = repo.get_request(&req2_id).await.unwrap().unwrap();
        assert_eq!(r2.status, RequestStatus::Failed);

        // Second sweep should return 0
        let swept2 = repo.sweep_stale_requests("again").await.unwrap();
        assert_eq!(swept2, 0);
    }

    #[tokio::test]
    async fn sweep_conversation_stale_requests_only_affects_matching_conversation() {
        let repo = InMemoryTraceRepository::default();
        let conv_a = ConversationKey::new("wecom", "chat-1", "astra");
        let conv_b = ConversationKey::new("wecom", "chat-2", "astra");

        // Create requests in both conversations
        let req_a = GatewayRequest::new(conv_a.clone(), "msg-1", "u1", "hello");
        let req_a_id = req_a.request_id.clone();
        TraceWriter::begin(&repo, req_a).await.unwrap();

        let req_b = GatewayRequest::new(conv_b.clone(), "msg-2", "u1", "world");
        let req_b_id = req_b.request_id.clone();
        TraceWriter::begin(&repo, req_b).await.unwrap();

        // Sweep only conv_a
        let swept = repo
            .sweep_conversation_stale_requests(&conv_a, "worker exited")
            .await
            .unwrap();
        assert_eq!(swept, 1);

        // conv_a request should be Failed
        let r_a = repo.get_request(&req_a_id).await.unwrap().unwrap();
        assert_eq!(r_a.status, RequestStatus::Failed);
        assert_eq!(r_a.error_message.as_deref(), Some("worker exited"));

        // conv_b request should still be Accepted
        let r_b = repo.get_request(&req_b_id).await.unwrap().unwrap();
        assert_eq!(r_b.status, RequestStatus::Accepted);
    }

    #[tokio::test]
    async fn force_fail_request_transitions_running_to_failed() {
        let repo = InMemoryTraceRepository::default();
        let conv = ConversationKey::new("wecom", "chat-1", "astra");

        let req = GatewayRequest::new(conv.clone(), "msg-1", "u1", "hello");
        let trace_id = req.trace_id.clone();
        let req_id = req.request_id.clone();
        let writer = TraceWriter::begin(&repo, req).await.unwrap();
        let _ = writer.start_run("astra", None).await.unwrap();

        // Force-fail while Running
        let result = repo
            .force_fail_request(&trace_id, "killed by user")
            .await
            .unwrap();
        assert!(result, "should return true for Running -> Failed");

        let r = repo.get_request(&req_id).await.unwrap().unwrap();
        assert_eq!(r.status, RequestStatus::Failed);
        assert_eq!(r.error_message.as_deref(), Some("killed by user"));
    }

    #[tokio::test]
    async fn force_fail_request_returns_false_for_terminal() {
        let repo = InMemoryTraceRepository::default();
        let conv = ConversationKey::new("wecom", "chat-1", "astra");

        let req = GatewayRequest::new(conv, "msg-1", "u1", "hello");
        let trace_id = req.trace_id.clone();
        let writer = TraceWriter::begin(&repo, req).await.unwrap();
        writer.complete_request().await.unwrap();

        // Already Completed — force_fail should return false
        let result = repo
            .force_fail_request(&trace_id, "too late")
            .await
            .unwrap();
        assert!(!result, "should return false for already-terminal request");
    }

    #[tokio::test]
    async fn dismiss_failed_outbox_marks_as_sent() {
        let repo = InMemoryTraceRepository::default();
        let conv = ConversationKey::new("wecom", "chat-1", "astra");
        let req = GatewayRequest::new(conv, "msg-1", "u1", "hello");
        let request_id = req.request_id.clone();
        let writer = TraceWriter::begin(&repo, req).await.unwrap();
        let run_id = writer.start_run("astra", None).await.unwrap();
        writer
            .finish_run(&run_id, RunStatus::Succeeded, Some(0), None)
            .await
            .unwrap();
        let outbox_id = writer
            .enqueue_outbox("wecom", "chat-1", None, "reply body")
            .await
            .unwrap();

        // Mark outbox as failed
        writer
            .mark_outbox_failed(&outbox_id, "send error", 0)
            .await
            .unwrap();
        assert_eq!(
            repo.get_outbox(&outbox_id).await.unwrap().unwrap().status,
            OutboxStatus::Failed
        );

        // Dismiss it
        repo.dismiss_failed_outbox(&request_id).await.unwrap();
        let outbox = repo.get_outbox(&outbox_id).await.unwrap().unwrap();
        assert_eq!(outbox.status, OutboxStatus::Sent);
        assert!(outbox.error_message.is_none());

        // Should no longer appear in retryable list
        assert!(
            repo.list_retryable_outbox(Some("wecom"), 10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn dismiss_failed_outbox_ignores_non_failed() {
        let repo = InMemoryTraceRepository::default();
        let conv = ConversationKey::new("wecom", "chat-1", "astra");
        let req = GatewayRequest::new(conv, "msg-1", "u1", "hello");
        let request_id = req.request_id.clone();
        let writer = TraceWriter::begin(&repo, req).await.unwrap();
        let run_id = writer.start_run("astra", None).await.unwrap();
        writer
            .finish_run(&run_id, RunStatus::Succeeded, Some(0), None)
            .await
            .unwrap();
        let outbox_id = writer
            .enqueue_outbox("wecom", "chat-1", None, "pending body")
            .await
            .unwrap();

        // Outbox is Pending, not Failed — dismiss should not change it
        repo.dismiss_failed_outbox(&request_id).await.unwrap();
        let outbox = repo.get_outbox(&outbox_id).await.unwrap().unwrap();
        assert_eq!(outbox.status, OutboxStatus::Pending);
    }

    #[tokio::test]
    async fn force_fail_request_works_on_accepted() {
        let repo = InMemoryTraceRepository::default();
        let conv = ConversationKey::new("wecom", "chat-1", "astra");

        let req = GatewayRequest::new(conv, "msg-1", "u1", "hello");
        let trace_id = req.trace_id.clone();
        let req_id = req.request_id.clone();
        TraceWriter::begin(&repo, req).await.unwrap();

        // Force-fail while Accepted
        let result = repo.force_fail_request(&trace_id, "killed").await.unwrap();
        assert!(result);

        let r = repo.get_request(&req_id).await.unwrap().unwrap();
        assert_eq!(r.status, RequestStatus::Failed);
    }

    #[tokio::test]
    async fn outbox_retry_count_increments_and_stops_retrying() {
        let repo = InMemoryTraceRepository::default();
        let conv = ConversationKey::new("wecom", "chat-1", "astra");
        let req = GatewayRequest::new(conv.clone(), "msg-1", "u1", "hello");
        let request_id = req.request_id.clone();
        let writer = TraceWriter::begin(&repo, req).await.unwrap();
        let run_id = writer.start_run("astra", None).await.unwrap();
        writer
            .finish_run(&run_id, RunStatus::Succeeded, Some(0), None)
            .await
            .unwrap();
        let outbox_id = writer
            .enqueue_outbox("wecom", "chat-1", None, "retry body")
            .await
            .unwrap();

        // Fail it OUTBOX_MAX_RETRIES times
        for i in 0..OUTBOX_MAX_RETRIES {
            // Before exhaustion, it should still be retryable
            let retryable = repo.list_retryable_outbox(Some("wecom"), 10).await.unwrap();
            assert_eq!(
                retryable.len(),
                1,
                "should be retryable before exhaustion (attempt {i})"
            );

            writer
                .mark_outbox_failed(&outbox_id, &format!("error attempt {i}"), 0)
                .await
                .unwrap();
        }

        // After max retries, retry_count == OUTBOX_MAX_RETRIES
        let outbox = repo.get_outbox(&outbox_id).await.unwrap().unwrap();
        assert_eq!(outbox.retry_count, OUTBOX_MAX_RETRIES);
        assert_eq!(outbox.status, OutboxStatus::Failed);

        // Should no longer appear in retryable list
        let retryable = repo.list_retryable_outbox(Some("wecom"), 10).await.unwrap();
        assert!(
            retryable.is_empty(),
            "should stop retrying after max retries"
        );

        // Should also not appear in active requests (request is completed via run)
        let active = repo.list_active_requests(&conv, 10).await.unwrap();
        let has_outbox_retrying = active
            .iter()
            .any(|r| r.request_id == request_id && r.outbox_status == Some(OutboxStatus::Failed));
        assert!(
            !has_outbox_retrying,
            "exhausted outbox should not show as retrying in active requests"
        );
    }

    // ── GAP 1: gateway_status aggregation ─────────────────────────

    #[tokio::test]
    async fn gateway_status_aggregates_correctly() {
        let repo = InMemoryTraceRepository::default();
        let conv = ConversationKey::new("wx", "c1", "astra");

        // Empty status
        let status = repo.gateway_status(&conv).await.unwrap();
        assert_eq!(status.queued_count, 0);
        assert_eq!(status.running_count, 0);
        assert_eq!(status.active_count, 0);
        assert_eq!(status.recent_trace_count, 0);
        assert!(status.last_trace.is_none());

        // Add an Accepted request (counts as "queued")
        let req1 = GatewayRequest::new(conv.clone(), "m1", "u1", "hello");
        let writer1 = TraceWriter::begin(&repo, req1).await.unwrap();

        let status = repo.gateway_status(&conv).await.unwrap();
        assert_eq!(status.queued_count, 1);
        assert_eq!(status.running_count, 0);
        assert_eq!(status.active_count, 1);
        assert_eq!(status.recent_trace_count, 1);
        assert!(status.last_trace.is_some());

        // Move to Running via start_run (which updates status)
        let _run_id = writer1.start_run("astra", None).await.unwrap();
        let status = repo.gateway_status(&conv).await.unwrap();
        assert_eq!(status.queued_count, 0);
        assert_eq!(status.running_count, 1);

        // Add another Accepted request
        let req2 = GatewayRequest::new(conv.clone(), "m2", "u1", "world");
        TraceWriter::begin(&repo, req2).await.unwrap();

        let status = repo.gateway_status(&conv).await.unwrap();
        assert_eq!(status.queued_count, 1);
        assert_eq!(status.running_count, 1);
        assert_eq!(status.active_count, 2);
        assert_eq!(status.recent_trace_count, 2);
    }

    #[tokio::test]
    async fn gateway_status_counts_outbox_states() {
        let repo = InMemoryTraceRepository::default();
        let conv = ConversationKey::new("wx", "c1", "astra");

        let req = GatewayRequest::new(conv.clone(), "m1", "u1", "outbox test");
        let writer = TraceWriter::begin(&repo, req).await.unwrap();
        let run_id = writer.start_run("astra", None).await.unwrap();
        writer
            .finish_run(&run_id, RunStatus::Succeeded, Some(0), None)
            .await
            .unwrap();

        // Enqueue outbox -> pending
        let outbox_id = writer
            .enqueue_outbox("wx", "c1", None, "reply body")
            .await
            .unwrap();

        let status = repo.gateway_status(&conv).await.unwrap();
        assert_eq!(status.pending_outbox_count, 1);
        assert_eq!(status.retrying_outbox_count, 0);

        // Fail outbox -> retrying
        writer
            .mark_outbox_failed(&outbox_id, "send error", 0)
            .await
            .unwrap();
        let status = repo.gateway_status(&conv).await.unwrap();
        assert_eq!(status.retrying_outbox_count, 1);
        assert_eq!(status.pending_outbox_count, 0);
    }

    // ── GAP 2: full request lifecycle chain ──────────────────────

    #[tokio::test]
    async fn full_request_lifecycle_begin_to_complete() {
        let repo = InMemoryTraceRepository::default();
        let conv = ConversationKey::new("wx", "c1", "astra");
        let req = GatewayRequest::new(conv.clone(), "m1", "u1", "do something");
        let trace_id = req.trace_id.clone();
        let req_id = req.request_id.clone();

        let writer = TraceWriter::begin(&repo, req).await.unwrap();

        // Initially Accepted
        let r = repo.get_request(&req_id).await.unwrap().unwrap();
        assert_eq!(r.status, RequestStatus::Accepted);

        // Start and finish a run (transitions to Running, then we complete)
        let run_id = writer
            .start_run("astra", Some("session-1".into()))
            .await
            .unwrap();
        let r = repo.get_request(&req_id).await.unwrap().unwrap();
        assert_eq!(r.status, RequestStatus::Running);

        writer
            .finish_run(&run_id, RunStatus::Succeeded, Some(0), None)
            .await
            .unwrap();

        // Complete
        writer.complete_request().await.unwrap();
        let r = repo.get_request(&req_id).await.unwrap().unwrap();
        assert_eq!(r.status, RequestStatus::Completed);

        // Verify events recorded
        let events = repo.list_events_for_trace(&trace_id, 50).await.unwrap();
        assert!(
            events.len() >= 4,
            "should have received, run_started, run_completed, request_completed; got {}",
            events.len()
        );
        let kinds: Vec<_> = events.iter().map(|e| e.kind).collect();
        assert!(kinds.contains(&GatewayEventKind::RequestReceived));
        assert!(kinds.contains(&GatewayEventKind::RunStarted));
        assert!(kinds.contains(&GatewayEventKind::RunCompleted));
        assert!(kinds.contains(&GatewayEventKind::RequestCompleted));
    }

    #[tokio::test]
    async fn full_request_lifecycle_with_failure() {
        let repo = InMemoryTraceRepository::default();
        let conv = ConversationKey::new("wx", "c1", "astra");
        let req = GatewayRequest::new(conv.clone(), "m1", "u1", "fail me");
        let req_id = req.request_id.clone();
        let trace_id = req.trace_id.clone();

        let writer = TraceWriter::begin(&repo, req).await.unwrap();
        let run_id = writer.start_run("astra", None).await.unwrap();
        writer
            .finish_run(&run_id, RunStatus::Failed, Some(1), Some("CLI error"))
            .await
            .unwrap();
        writer.fail_request("something broke").await.unwrap();

        let r = repo.get_request(&req_id).await.unwrap().unwrap();
        assert_eq!(r.status, RequestStatus::Failed);
        assert_eq!(r.error_message.as_deref(), Some("something broke"));

        let events = repo.list_events_for_trace(&trace_id, 50).await.unwrap();
        let kinds: Vec<_> = events.iter().map(|e| e.kind).collect();
        assert!(kinds.contains(&GatewayEventKind::RunFailed));
        assert!(kinds.contains(&GatewayEventKind::RequestFailed));
    }

    // ── GAP 5: cancel_accepted_request coverage ─────────────────

    #[tokio::test]
    async fn cancel_accepted_request_transitions_to_cancelled() {
        let repo = InMemoryTraceRepository::default();
        let conv = ConversationKey::new("wx", "c1", "astra");
        let req = GatewayRequest::new(conv.clone(), "m1", "u1", "cancel me");
        let req_id = req.request_id.clone();
        TraceWriter::begin(&repo, req).await.unwrap();

        match repo
            .cancel_accepted_request(&conv, req_id.as_str(), "user cancelled")
            .await
            .unwrap()
        {
            CancelRequestOutcome::Cancelled(row) => {
                assert_eq!(row.request_id, req_id);
                assert_eq!(row.status, RequestStatus::Failed);
                assert_eq!(row.error_message.as_deref(), Some("user cancelled"));
            }
            other => panic!("expected Cancelled, got {:?}", other),
        }

        // Verify underlying request was updated
        let r = repo.get_request(&req_id).await.unwrap().unwrap();
        assert_eq!(r.status, RequestStatus::Failed);
    }

    #[tokio::test]
    async fn cancel_running_request_returns_already_running() {
        let repo = InMemoryTraceRepository::default();
        let conv = ConversationKey::new("wx", "c1", "astra");
        let req = GatewayRequest::new(conv.clone(), "m1", "u1", "running");
        let req_id = req.request_id.clone();
        let writer = TraceWriter::begin(&repo, req).await.unwrap();
        // Use start_run to actually transition status to Running
        let _run_id = writer.start_run("astra", None).await.unwrap();

        match repo
            .cancel_accepted_request(&conv, req_id.as_str(), "user cancelled")
            .await
            .unwrap()
        {
            CancelRequestOutcome::AlreadyRunning(row) => {
                assert_eq!(row.request_id, req_id);
                assert_eq!(row.status, RequestStatus::Running);
            }
            other => panic!("expected AlreadyRunning, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn cancel_nonexistent_returns_not_found() {
        let repo = InMemoryTraceRepository::default();
        let conv = ConversationKey::new("wx", "c1", "astra");
        match repo
            .cancel_accepted_request(&conv, "nonexistent", "reason")
            .await
            .unwrap()
        {
            CancelRequestOutcome::NotFound => {}
            other => panic!("expected NotFound, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn cancel_completed_request_returns_not_found() {
        let repo = InMemoryTraceRepository::default();
        let conv = ConversationKey::new("wx", "c1", "astra");
        let req = GatewayRequest::new(conv.clone(), "m1", "u1", "done");
        let req_id = req.request_id.clone();
        let writer = TraceWriter::begin(&repo, req).await.unwrap();
        writer.complete_request().await.unwrap();

        // Completed requests are not in active list, so cancel returns NotFound
        match repo
            .cancel_accepted_request(&conv, req_id.as_str(), "too late")
            .await
            .unwrap()
        {
            CancelRequestOutcome::NotFound => {}
            other => panic!("expected NotFound for completed request, got {:?}", other),
        }
    }

    // ── GAP 6: list_recent_traces ────────────────────────────────

    #[tokio::test]
    async fn list_recent_traces_returns_newest_first() {
        let repo = InMemoryTraceRepository::default();
        let conv = ConversationKey::new("wx", "c1", "astra");

        let req1 = GatewayRequest::new(conv.clone(), "m1", "u1", "first");
        let trace1 = req1.trace_id.clone();
        TraceWriter::begin(&repo, req1).await.unwrap();

        let req2 = GatewayRequest::new(conv.clone(), "m2", "u1", "second");
        let trace2 = req2.trace_id.clone();
        TraceWriter::begin(&repo, req2).await.unwrap();

        let traces = repo.list_recent_traces(&conv, 10).await.unwrap();
        assert_eq!(traces.len(), 2);
        // Newest first
        assert_eq!(traces[0].trace_id, trace2);
        assert_eq!(traces[1].trace_id, trace1);
    }

    #[tokio::test]
    async fn list_recent_traces_respects_limit() {
        let repo = InMemoryTraceRepository::default();
        let conv = ConversationKey::new("wx", "c1", "astra");

        for i in 0..5 {
            let req = GatewayRequest::new(conv.clone(), format!("m{i}"), "u1", format!("msg {i}"));
            TraceWriter::begin(&repo, req).await.unwrap();
        }

        let traces = repo.list_recent_traces(&conv, 3).await.unwrap();
        assert_eq!(traces.len(), 3);
    }

    #[tokio::test]
    async fn list_recent_traces_filters_by_conversation() {
        let repo = InMemoryTraceRepository::default();
        let conv_a = ConversationKey::new("wx", "c1", "astra");
        let conv_b = ConversationKey::new("wx", "c2", "astra");

        TraceWriter::begin(
            &repo,
            GatewayRequest::new(conv_a.clone(), "m1", "u1", "in conv_a"),
        )
        .await
        .unwrap();
        TraceWriter::begin(
            &repo,
            GatewayRequest::new(conv_b.clone(), "m2", "u1", "in conv_b"),
        )
        .await
        .unwrap();

        let traces_a = repo.list_recent_traces(&conv_a, 10).await.unwrap();
        assert_eq!(traces_a.len(), 1);
        assert_eq!(traces_a[0].text_preview, "in conv_a");

        let traces_b = repo.list_recent_traces(&conv_b, 10).await.unwrap();
        assert_eq!(traces_b.len(), 1);
        assert_eq!(traces_b[0].text_preview, "in conv_b");
    }

    // ── GAP 7: outbox enqueue → send → complete chain ────────────

    #[tokio::test]
    async fn outbox_enqueue_send_completes_request() {
        let repo = InMemoryTraceRepository::default();
        let conv = ConversationKey::new("wx", "c1", "astra");
        let req = GatewayRequest::new(conv.clone(), "m1", "u1", "test");
        let req_id = req.request_id.clone();
        let writer = TraceWriter::begin(&repo, req).await.unwrap();
        let run_id = writer.start_run("astra", None).await.unwrap();
        writer
            .finish_run(&run_id, RunStatus::Succeeded, Some(0), None)
            .await
            .unwrap();

        // Enqueue outbox
        let outbox_id = writer
            .enqueue_outbox("wx", "c1", None, "hello world")
            .await
            .unwrap();

        // Verify it appears in retryable list (status: pending)
        let retryable = repo.list_retryable_outbox(Some("wx"), 10).await.unwrap();
        assert_eq!(retryable.len(), 1);
        assert_eq!(retryable[0].body, "hello world");

        // Send successfully (mark_outbox_sent also calls complete_request)
        writer.mark_outbox_sent(&outbox_id, 1).await.unwrap();

        // No longer retryable
        let retryable = repo.list_retryable_outbox(Some("wx"), 10).await.unwrap();
        assert!(retryable.is_empty());

        // Request should be completed
        let r = repo.get_request(&req_id).await.unwrap().unwrap();
        assert_eq!(r.status, RequestStatus::Completed);
    }

    #[tokio::test]
    async fn outbox_platform_filter_works() {
        let repo = InMemoryTraceRepository::default();
        let conv = ConversationKey::new("wx", "c1", "astra");
        let req = GatewayRequest::new(conv.clone(), "m1", "u1", "test");
        let writer = TraceWriter::begin(&repo, req).await.unwrap();
        let run_id = writer.start_run("astra", None).await.unwrap();
        writer
            .finish_run(&run_id, RunStatus::Succeeded, Some(0), None)
            .await
            .unwrap();
        writer
            .enqueue_outbox("wx", "c1", None, "wx msg")
            .await
            .unwrap();

        // Filter by platform
        let wx = repo.list_retryable_outbox(Some("wx"), 10).await.unwrap();
        assert_eq!(wx.len(), 1);

        let telegram = repo
            .list_retryable_outbox(Some("telegram"), 10)
            .await
            .unwrap();
        assert!(telegram.is_empty());

        // No filter returns all
        let all = repo.list_retryable_outbox(None, 10).await.unwrap();
        assert_eq!(all.len(), 1);
    }

    #[tokio::test]
    async fn outbox_retry_count_resets_on_successful_send() {
        let repo = InMemoryTraceRepository::default();
        let conv = ConversationKey::new("wecom", "chat-1", "astra");
        let req = GatewayRequest::new(conv, "msg-1", "u1", "hello");
        let writer = TraceWriter::begin(&repo, req).await.unwrap();
        let run_id = writer.start_run("astra", None).await.unwrap();
        writer
            .finish_run(&run_id, RunStatus::Succeeded, Some(0), None)
            .await
            .unwrap();
        let outbox_id = writer
            .enqueue_outbox("wecom", "chat-1", None, "body")
            .await
            .unwrap();

        // Fail twice (below max)
        writer
            .mark_outbox_failed(&outbox_id, "error 1", 0)
            .await
            .unwrap();
        writer
            .mark_outbox_failed(&outbox_id, "error 2", 0)
            .await
            .unwrap();

        // Should still be retryable
        let retryable = repo.list_retryable_outbox(Some("wecom"), 10).await.unwrap();
        assert_eq!(retryable.len(), 1);

        // Successfully send
        writer.mark_outbox_sent(&outbox_id, 1).await.unwrap();

        // Should no longer be retryable (status is Sent)
        let retryable = repo.list_retryable_outbox(Some("wecom"), 10).await.unwrap();
        assert!(retryable.is_empty());
    }

    #[tokio::test]
    async fn outbox_new_entry_has_zero_retry_count() {
        let repo = InMemoryTraceRepository::default();
        let conv = ConversationKey::new("wecom", "chat-1", "astra");
        let req = GatewayRequest::new(conv, "msg-1", "u1", "hello");
        let writer = TraceWriter::begin(&repo, req).await.unwrap();
        let run_id = writer.start_run("astra", None).await.unwrap();
        writer
            .finish_run(&run_id, RunStatus::Succeeded, Some(0), None)
            .await
            .unwrap();
        let outbox_id = writer
            .enqueue_outbox("wecom", "chat-1", None, "body")
            .await
            .unwrap();

        let outbox = repo.get_outbox(&outbox_id).await.unwrap().unwrap();
        assert_eq!(outbox.retry_count, 0);
    }

    #[tokio::test]
    async fn mark_outbox_sent_succeeds_when_request_already_failed() {
        let repo = InMemoryTraceRepository::default();
        let conv = ConversationKey::new("wx", "c1", "astra");
        let req = GatewayRequest::new(conv.clone(), "m1", "u1", "test");
        let req_id = req.request_id.clone();
        let writer = TraceWriter::begin(&repo, req).await.unwrap();
        let run_id = writer.start_run("astra", None).await.unwrap();
        writer
            .finish_run(&run_id, RunStatus::Succeeded, Some(0), None)
            .await
            .unwrap();
        let outbox_id = writer
            .enqueue_outbox("wx", "c1", None, "hello")
            .await
            .unwrap();

        // Simulate startup sweep: force request to failed (as sweep_stale_requests does)
        repo.update_request_status(&req_id, RequestStatus::Failed, Some("gateway restarted"))
            .await
            .unwrap();
        let r = repo.get_request(&req_id).await.unwrap().unwrap();
        assert_eq!(r.status, RequestStatus::Failed);

        // Now outbox replay delivers successfully — mark_outbox_sent must NOT fail
        writer.mark_outbox_sent(&outbox_id, 1).await.unwrap();

        // Outbox is marked sent
        let outbox = repo.get_outbox(&outbox_id).await.unwrap().unwrap();
        assert_eq!(outbox.status, OutboxStatus::Sent);

        // Request stays failed (terminal state unchanged — that's correct)
        let r = repo.get_request(&req_id).await.unwrap().unwrap();
        assert_eq!(r.status, RequestStatus::Failed);
    }
}
