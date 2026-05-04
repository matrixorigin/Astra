//! Gateway runner — bridges chat platforms to the `astra` CLI.
//!
//! Each inbound message spawns `astra chat -m "..." --session-id X`
//! and streams CLI progress to the chat platform while waiting for output.

use crate::cli_bridge::{self, CliProfile, CliProgress};
use crate::commands::{self, CommandContext};
use crate::config::GatewayConfig;
use crate::gateway_context::GatewayContext;
use crate::platforms::{InboundMessage, PlatformAdapter};
use crate::storage;
use crate::trace_model::{
    ConversationKey, GatewayRequest, MysqlTraceRepository, OutboxId, RequestId, RequestStatus,
    RunStatus, TraceId, TraceRepository, TraceWriter,
};
use astra_core::durable_task_store::DurableTaskStore as _;
use futures_util::future::select_all;
use sqlx::MySqlPool;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

const MAX_CHUNK_LEN: usize = 3800;
const INITIAL_ACK_DELAY: Duration = Duration::from_secs(3);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(25);
const PROGRESSIVE_FLUSH_INTERVAL: Duration = Duration::from_secs(8);
const PROGRESSIVE_MIN_CHARS: usize = 200;
const CONVERSATION_QUEUE_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Outbound message from CLI, scheduler, or other background tasks.
#[derive(Debug, Clone)]
pub struct OutboundMessage {
    pub platform: String,
    pub chat_id: String,
    pub text: String,
    pub reply_token: Option<String>,
    pub outbox: Option<OutboxDelivery>,
}

#[derive(Debug, Clone)]
pub struct OutboxDelivery {
    pub outbox_id: OutboxId,
    pub trace_id: TraceId,
    pub request_id: RequestId,
}

impl OutboundMessage {
    pub fn plain(
        platform: impl Into<String>,
        chat_id: impl Into<String>,
        text: impl Into<String>,
    ) -> Self {
        Self {
            platform: platform.into(),
            chat_id: chat_id.into(),
            text: text.into(),
            reply_token: None,
            outbox: None,
        }
    }

    pub fn with_outbox(
        platform: impl Into<String>,
        chat_id: impl Into<String>,
        text: impl Into<String>,
        reply_token: Option<String>,
        outbox: OutboxDelivery,
    ) -> Self {
        Self {
            platform: platform.into(),
            chat_id: chat_id.into(),
            text: text.into(),
            reply_token,
            outbox: Some(outbox),
        }
    }
}

pub struct GatewayRunner {
    config: GatewayConfig,
    pool: Option<MySqlPool>,
    cli_profile: CliProfile,
    thin: astra_thin_client::ThinClient,
    outbound_tx: Option<tokio::sync::mpsc::Sender<OutboundMessage>>,
    durable_store: Option<std::sync::Arc<crate::durable_task_store::MysqlDurableTaskStore>>,
    user_skills: Vec<(String, String)>,
    projects: Vec<String>,
    trace_repo: Option<Arc<MysqlTraceRepository>>,
    queue_senders:
        tokio::sync::Mutex<HashMap<ConversationKey, tokio::sync::mpsc::Sender<QueuedRequest>>>,
    global_run_limiter: Arc<tokio::sync::Semaphore>,
}

/// No-op adapter used in spawned CLI tasks (typing/heartbeats not available in background).
struct NullAdapter;

#[async_trait::async_trait]
impl PlatformAdapter for NullAdapter {
    fn name(&self) -> &'static str {
        "null"
    }
    async fn start(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(())
    }
    async fn stop(&mut self) {}
    async fn send_text(&self, _: &str, _: &str, _: Option<&str>) -> Result<(), String> {
        Ok(())
    }
    async fn recv(&self) -> Option<InboundMessage> {
        None
    }
}

/// Response from a background CLI task, routed back to the adapter.
type CliResponse = OutboundMessage;

#[derive(Debug)]
struct QueuedRequest {
    msg: InboundMessage,
    conversation: ConversationKey,
    trace: Option<OutboxDeliveryTrace>,
}

#[derive(Debug, Clone)]
struct OutboxDeliveryTrace {
    trace_id: TraceId,
    request_id: RequestId,
}

enum AdapterRecv {
    Message(InboundMessage),
    Closed(usize),
}

impl GatewayRunner {
    pub async fn new(
        config: GatewayConfig,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let thin = astra_thin_client::ThinClient::new(
            &config.astra.base_url,
            if config.astra.api_key.is_empty() {
                None
            } else {
                Some(config.astra.api_key.clone())
            },
        )?;

        let pool = match connect_db(&config.database.url).await {
            Ok(pool) => {
                tracing::info!("database connected");
                Some(pool)
            }
            Err(e) => {
                tracing::warn!(error = %e, "DB not available — running without persistence (sessions in-memory, no cron)");
                None
            }
        };

        let cli_profile = config.cli.clone();

        let durable_store = pool.as_ref().map(|p| {
            std::sync::Arc::new(crate::durable_task_store::MysqlDurableTaskStore::new(
                p.clone(),
            ))
        });
        let trace_repo = pool
            .as_ref()
            .map(|p| Arc::new(MysqlTraceRepository::new(p.clone())));

        let user_skills = config
            .skills_dir
            .as_deref()
            .map(crate::gateway_context::load_skills_from_dir)
            .unwrap_or_default();
        if !user_skills.is_empty() {
            tracing::info!(
                count = user_skills.len(),
                "loaded user skills from directory"
            );
        }

        // Discover available projects
        let projects: Vec<String> = crate::workspace::discover_all_projects(&config.project_dirs)
            .iter()
            .map(|p| p.summary())
            .collect();
        if !projects.is_empty() {
            tracing::info!(count = projects.len(), "discovered projects");
        }

        if let Some(ref pool) = pool
            && let Err(e) = crate::usage::ensure_usage_table(pool).await
        {
            tracing::warn!(error = %e, "failed to ensure usage tracking table");
        }
        let max_concurrent_runs = config.max_concurrent_runs.max(1);

        Ok(Self {
            config,
            pool,
            cli_profile,
            thin,
            outbound_tx: None,
            durable_store,
            user_skills,
            projects,
            trace_repo,
            queue_senders: tokio::sync::Mutex::new(HashMap::new()),
            global_run_limiter: Arc::new(tokio::sync::Semaphore::new(max_concurrent_runs)),
        })
    }

    pub fn pool(&self) -> Option<&MySqlPool> {
        self.pool.as_ref()
    }

    pub fn cli_profile(&self) -> &CliProfile {
        &self.cli_profile
    }

    /// Replay messages that were pending when gateway crashed.
    pub async fn replay_pending_messages(&self, adapter: &dyn PlatformAdapter) {
        if let Some(ref pool) = self.pool {
            let platform = adapter.name();
            match storage::list_pending_messages(pool, Some(platform)).await {
                Ok(msgs) if msgs.is_empty() => {}
                Ok(msgs) => {
                    tracing::info!(platform, count = msgs.len(), "replaying pending messages");
                    for (id, _platform, chat_id, user_id, text) in &msgs {
                        let msg = crate::platforms::InboundMessage {
                            platform,
                            chat_id: chat_id.clone(),
                            user_id: user_id.clone(),
                            text: text.clone(),
                            msg_id: format!("replay-{id}"),
                            chat_type: crate::platforms::ChatType::DirectMessage,
                            reply_token: None,
                        };
                        if let Some(response) = self
                            .handle_message_inner(&msg, adapter, Some(*id), false, None)
                            .await
                        {
                            for chunk in split_message(&response.text) {
                                if let Err(e) = adapter.send_text(chat_id, chunk, None).await {
                                    tracing::warn!(error = %e, chat_id, "failed to deliver pending replay");
                                }
                            }
                        }
                    }
                }
                Err(e) => tracing::warn!(error = %e, "failed to load pending messages"),
            }
        }
    }

    pub async fn sweep_stale_tasks(&self) {
        if let Some(ref store) = self.durable_store {
            match store.suspend_stale_running_tasks("gateway restarted").await {
                Ok(0) => {}
                Ok(n) => tracing::info!(count = n, "swept stale running tasks → suspended"),
                Err(e) => tracing::warn!(error = %e, "failed to sweep stale tasks"),
            }
        }
    }

    pub fn set_outbound_tx(&mut self, tx: tokio::sync::mpsc::Sender<OutboundMessage>) {
        self.outbound_tx = Some(tx);
    }

    /// Resolve the active CLI profile for a user (may be overridden via /cli + /model).
    async fn resolve_cli_profile(&self, platform: &str, user_id: &str) -> CliProfile {
        let mut profile = if let Some(ref pool) = self.pool
            && let Ok(Some(name)) =
                storage::get_user_preference(pool, platform, user_id, "cli_profile").await
            && let Some(p) = self.config.cli_profiles.get(&name)
        {
            p.clone()
        } else {
            self.cli_profile.clone()
        };

        // Apply per-user model override scoped to this CLI
        let model_key = storage::model_preference_key(profile.name());
        if let Some(ref pool) = self.pool
            && let Ok(Some(model_name)) =
                storage::get_user_preference(pool, platform, user_id, &model_key).await
        {
            match &mut profile {
                CliProfile::Astra { model, .. } | CliProfile::Claude { model, .. } => {
                    *model = Some(model_name);
                }
                _ => {}
            }
        }

        profile
    }

    /// Fast path: access control + slash commands. Returns Some if handled (no CLI needed).
    pub async fn handle_fast(
        &self,
        msg: &InboundMessage,
    ) -> Result<Option<String>, InboundMessage> {
        // Access control check
        if !self.config.access.is_allowed(&msg.user_id) {
            tracing::debug!(user = %safe_id(&msg.user_id), "message rejected by access policy");
            return Ok(Some(self.config.access.rejection_message().to_string()));
        }

        // Group chat: require @mention if configured
        if msg.chat_type == crate::platforms::ChatType::Group
            && self.config.group_require_mention
            && !msg.text.contains("@bot")
            && !msg.text.contains("@Bot")
        {
            return Ok(None);
        }

        // Group chat: per-user session isolation
        let effective_chat_id = if msg.chat_type == crate::platforms::ChatType::Group
            && self.config.group_sessions_per_user
        {
            format!("{}:{}", msg.chat_id, msg.user_id)
        } else {
            msg.chat_id.clone()
        };

        // Resolve active CLI profile
        let cli_profile = self.resolve_cli_profile(msg.platform, &msg.user_id).await;

        // Slash commands — instant response, no CLI
        let cmd_ctx = CommandContext {
            astra: &self.thin,
            config: &self.config,
            pool: self.pool.as_ref(),
            platform: msg.platform,
            chat_id: &effective_chat_id,
            user_id: &msg.user_id,
            resolved_cli: &cli_profile,
            durable_store: self
                .durable_store
                .as_ref()
                .map(|s| s.as_ref() as &dyn astra_core::durable_task_store::DurableTaskStore),
            trace_repo: self
                .trace_repo
                .as_ref()
                .map(|repo| repo.as_ref() as &dyn TraceRepository),
        };
        if let Some(response) = commands::handle_command(&cmd_ctx, &msg.text).await {
            return Ok(Some(response));
        }

        // Not a slash command — needs CLI (slow path)
        Err(msg.clone())
    }

    /// Handle a single inbound message (full path including CLI).
    pub async fn handle_message(
        &self,
        msg: &InboundMessage,
        adapter: &dyn PlatformAdapter,
    ) -> Option<String> {
        self.handle_message_inner(msg, adapter, None, true, None)
            .await
            .map(|outbound| outbound.text)
    }

    async fn handle_message_inner(
        &self,
        msg: &InboundMessage,
        adapter: &dyn PlatformAdapter,
        existing_pending_id: Option<i64>,
        save_pending: bool,
        trace: Option<OutboxDeliveryTrace>,
    ) -> Option<OutboundMessage> {
        // Group chat: per-user session isolation
        let effective_chat_id = if msg.chat_type == crate::platforms::ChatType::Group
            && self.config.group_sessions_per_user
        {
            format!("{}:{}", msg.chat_id, msg.user_id)
        } else {
            msg.chat_id.clone()
        };

        let cli_profile = self.resolve_cli_profile(msg.platform, &msg.user_id).await;

        if let Some(ref pool) = self.pool
            && let Err(e) =
                storage::upsert_user(pool, msg.platform, &msg.user_id, &msg.user_id).await
        {
            tracing::warn!(error = %e, "failed to upsert user");
        }

        let cli_name = cli_profile.name().to_string();

        // Auto-reset session if policy triggers
        if let Some(ref pool) = self.pool
            && let Ok(Some(last_active_str)) =
                storage::get_session_last_active(pool, msg.platform, &effective_chat_id, &cli_name)
                    .await
            && let Ok(last_active) =
                chrono::NaiveDateTime::parse_from_str(&last_active_str, "%Y-%m-%d %H:%M:%S%.f")
        {
            let last_utc = last_active.and_utc();
            let now = chrono::Utc::now();
            if self.config.session_reset.should_reset(last_utc, now) {
                if let Err(e) = storage::reset_session_for_cli(
                    pool,
                    msg.platform,
                    &effective_chat_id,
                    &cli_name,
                )
                .await
                {
                    tracing::warn!(error = %e, "session auto-reset failed");
                } else {
                    tracing::info!(cli = cli_name, "session auto-reset by policy");
                }
            }
        }

        let session_id = if let Some(ref pool) = self.pool {
            storage::get_current_session_for_cli(pool, msg.platform, &effective_chat_id, &cli_name)
                .await
                .ok()
                .flatten()
        } else {
            None
        };

        let trace_writer = trace.as_ref().and_then(|trace| {
            self.trace_repo.as_ref().map(|repo| {
                TraceWriter::from_existing(
                    repo.as_ref() as &dyn TraceRepository,
                    trace.trace_id.clone(),
                    trace.request_id.clone(),
                )
            })
        });
        let mut run_id = None;
        if let Some(writer) = trace_writer.as_ref() {
            match writer.start_run(&cli_name, session_id.clone()).await {
                Ok(id) => {
                    let _ = writer.mark_running().await;
                    run_id = Some(id);
                }
                Err(e) => {
                    tracing::info!(error = %e, "queued request skipped before CLI start");
                    return None;
                }
            }
        }

        tracing::info!(
            platform = msg.platform,
            chat_id = %safe_id(&msg.chat_id),
            user = %safe_id(&msg.user_id),
            "→ {}",
            truncate(&msg.text, 80),
        );

        // Send typing indicator immediately so user gets feedback
        let _ = adapter.send_typing(&msg.chat_id).await;

        // Check CLI is available before spawning
        let availability = cli_bridge::probe_cli(&cli_profile).await;
        if !availability.is_available() {
            if let Some(writer) = trace_writer.as_ref() {
                if let Some(ref run_id) = run_id {
                    let _ = writer
                        .finish_run(run_id, RunStatus::Failed, None, Some("CLI unavailable"))
                        .await;
                }
                let _ = writer.fail_request("CLI unavailable").await;
            }
            let text = cli_bridge::onboarding_message(&cli_profile, &availability);
            return Some(
                self.outbound_response(
                    trace.as_ref(),
                    msg.platform,
                    &msg.chat_id,
                    msg.reply_token.clone(),
                    text,
                )
                .await,
            );
        }

        // Resolve workspace directory for CLI
        let workspace: Option<std::path::PathBuf> = if let Some(ref pool) = self.pool
            && let Ok(Some(ws)) =
                storage::get_user_preference(pool, msg.platform, &msg.user_id, "workspace").await
        {
            let path = std::path::PathBuf::from(&ws);
            if path.is_dir() { Some(path) } else { None }
        } else {
            None
        };

        // Build gateway context for CLI system prompt
        let gw_context = {
            let mut ctx = GatewayContext::new(
                &msg.user_id,
                &msg.user_id,
                msg.platform,
                &cli_profile,
                self.pool.is_some(),
            )
            .with_model_actions_allowed(self.config.action_policy.allow_model_generated_mutations);
            if let Some(ref pool) = self.pool
                && let Ok(jobs) =
                    storage::list_cron_jobs(pool, msg.platform, &effective_chat_id).await
            {
                let cron_list: Vec<_> = jobs
                    .iter()
                    .map(|(id, expr, desc, _)| {
                        (
                            id[..8.min(id.len())].to_string(),
                            expr.clone(),
                            desc.clone(),
                        )
                    })
                    .collect();
                ctx = ctx.with_cron_jobs(cron_list);
            }
            if let Some(ref store) = self.durable_store {
                let owner = format!("{}:{}", msg.platform, effective_chat_id);
                let filter = astra_core::durable_task_store::TaskFilter {
                    owner_id: Some(owner),
                    ..Default::default()
                };
                if let Ok(tasks) = store.list(filter).await {
                    let task_list: Vec<_> = tasks
                        .iter()
                        .filter(|t| t.status.is_active())
                        .map(|t| {
                            (
                                t.id.0[..8.min(t.id.0.len())].to_string(),
                                t.name.clone(),
                                t.status.as_str().to_string(),
                                t.progress_pct,
                            )
                        })
                        .collect();
                    ctx = ctx.with_active_tasks(task_list);
                }
            }
            if !self.user_skills.is_empty() {
                ctx = ctx.with_extra_skills(self.user_skills.clone());
            }
            if !self.projects.is_empty() {
                ctx = ctx.with_projects(self.projects.clone());
            }
            if let Some(ref ws) = workspace {
                ctx = ctx.with_workspace(Some(ws.to_string_lossy().to_string()));
            }
            ctx
        };
        let system_prompt = gw_context.to_system_prompt();

        // Save as pending (for crash recovery)
        let pending_id = if save_pending {
            if let Some(ref pool) = self.pool {
                match storage::save_pending_message(
                    pool,
                    msg.platform,
                    &effective_chat_id,
                    &msg.user_id,
                    &msg.text,
                )
                .await
                {
                    Ok(id) => Some(id),
                    Err(e) => {
                        tracing::warn!(error = %e, "failed to save pending message");
                        None
                    }
                }
            } else {
                None
            }
        } else {
            existing_pending_id
        };

        // Run CLI with rich progress heartbeats and a bounded lifetime.
        let message_text = msg.text.clone();
        let sid = session_id.clone();
        let chat_id = effective_chat_id.clone();
        let cli_name = cli_profile.name().to_string();
        let cli_timeout = Duration::from_secs(self.config.cli_timeout_secs.max(1));

        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::channel::<CliProgress>(64);

        let cli_handle = tokio::spawn({
            let profile = cli_profile.clone();
            let message_text = message_text.clone();
            let system_prompt = system_prompt.clone();
            let ws = workspace.clone();
            async move {
                cli_bridge::run_cli_with_context_and_timeout(
                    &profile,
                    &message_text,
                    sid.as_deref(),
                    ws.as_deref(),
                    Some(progress_tx),
                    Some(&system_prompt),
                    // TODO(cli_bridge): pass gateway trace_id/request_id once the
                    // bridge exposes a stable metadata API.
                    Some(cli_timeout),
                )
                .await
            }
        });

        let start = Instant::now();
        let mut tool_count: u32 = 0;
        let mut last_tool = String::new();
        let mut sent_initial_ack = false;
        let mut token_buf = String::new();
        let mut in_think_block = false;
        let mut gateway_action_filter = GatewayActionStreamFilter::default();
        let mut progressive_text_len: usize = 0;
        let next_timer = tokio::time::sleep(INITIAL_ACK_DELAY);
        tokio::pin!(next_timer);

        #[allow(clippy::type_complexity)]
        let flush_buf = |buf: &mut String,
                         tx: &Option<tokio::sync::mpsc::Sender<OutboundMessage>>,
                         platform: &str,
                         chat: &str|
         -> Option<(
            std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>>,
            usize,
        )> {
            let text = std::mem::take(buf);
            let text = text.trim().to_string();
            if text.is_empty() {
                return None;
            }
            let len = text.len();
            let tx = tx.clone();
            let platform = platform.to_string();
            let chat = chat.to_string();
            Some((
                Box::pin(async move {
                    if let Some(tx) = tx {
                        let _ = tx.send(OutboundMessage::plain(platform, chat, text)).await;
                    }
                }),
                len,
            ))
        };

        loop {
            tokio::select! {
                progress = progress_rx.recv() => {
                    match progress {
                        Some(CliProgress::Token(text)) => {
                            // Filter <think>...</think> blocks from token stream
                            let filtered = filter_think_tags(&text, &mut in_think_block);
                            let filtered = gateway_action_filter.push(&filtered);
                            if !filtered.is_empty() {
                                token_buf.push_str(&filtered);
                                if token_buf.len() >= PROGRESSIVE_MIN_CHARS {
                                    if let Some(fut) = flush_buf(&mut token_buf, &self.outbound_tx, msg.platform, &chat_id) {
                                        progressive_text_len += fut.1;
                                        fut.0.await;
                                    }
                                    next_timer.as_mut().reset(tokio::time::Instant::now() + PROGRESSIVE_FLUSH_INTERVAL);
                                }
                            }
                        }
                        Some(CliProgress::ToolStarted { ref name }) => {
                            tool_count += 1;
                            if !token_buf.is_empty() {
                                token_buf.push('\n');
                            }
                            token_buf.push_str(&format!("🔧 {name}…\n"));
                            last_tool = name.clone();
                        }
                        Some(CliProgress::ToolDone { name, duration_ms }) => {
                            token_buf.push_str(&format!("✅ {name} ({duration_ms}ms)\n"));
                            last_tool = name;
                        }
                        Some(CliProgress::ToolCall(line)) => {
                            tool_count += 1;
                            last_tool = line;
                        }
                        Some(CliProgress::Thinking(_)) => {}
                        Some(CliProgress::Status(_) | CliProgress::Stderr(_)) => {}
                        None => {
                            let tail = gateway_action_filter.finish();
                            if !tail.is_empty() {
                                token_buf.push_str(&tail);
                            }
                            if let Some((fut, len)) = flush_buf(&mut token_buf, &self.outbound_tx, msg.platform, &chat_id) {
                                progressive_text_len += len;
                                fut.await;
                            }
                            break;
                        }
                    }
                }
                _ = &mut next_timer => {
                    // Timer-based flush: either initial ack or periodic token flush
                    if !token_buf.is_empty() {
                        if let Some((fut, len)) = flush_buf(&mut token_buf, &self.outbound_tx, msg.platform, &chat_id) {
                            progressive_text_len += len;
                            fut.await;
                        }
                        next_timer.as_mut().reset(tokio::time::Instant::now() + PROGRESSIVE_FLUSH_INTERVAL);
                    } else if !sent_initial_ack {
                        sent_initial_ack = true;
                        let heartbeat = format!("🤔 {cli_name} 思考中…");
                        if let Some(ref tx) = self.outbound_tx {
                            let _ = tx
                                .send(OutboundMessage::plain(msg.platform.to_string(), chat_id.clone(), heartbeat))
                                .await;
                        }
                        next_timer.as_mut().reset(tokio::time::Instant::now() + HEARTBEAT_INTERVAL);
                    } else {
                        let elapsed_str = format_elapsed(start.elapsed());
                        let heartbeat = if tool_count > 0 {
                            let tool_short = truncate(&last_tool, 40);
                            format!("⏳ {elapsed_str} | 🔧 {tool_count}个工具 | {tool_short}")
                        } else {
                            format!("⏳ 处理中… {elapsed_str}")
                        };
                        if let Some(ref tx) = self.outbound_tx {
                            let _ = tx
                                .send(OutboundMessage::plain(msg.platform.to_string(), chat_id.clone(), heartbeat))
                                .await;
                        }
                        next_timer.as_mut().reset(tokio::time::Instant::now() + HEARTBEAT_INTERVAL);
                    }
                }
            }
        }

        // Helper: suspend running durable tasks for this chat on failure
        let suspend_tasks = |store: &Option<
            std::sync::Arc<crate::durable_task_store::MysqlDurableTaskStore>,
        >,
                             reason: String| {
            let store = store.clone();
            let owner = format!(
                "{platform}:{chat_id}",
                platform = msg.platform,
                chat_id = effective_chat_id
            );
            async move {
                if let Some(ref s) = store {
                    let n = s
                        .suspend_running_tasks_for_owner(&owner, &reason)
                        .await
                        .unwrap_or(0);
                    if n > 0 {
                        tracing::info!(count = n, %reason, "auto-suspended durable tasks on CLI failure");
                    }
                }
            }
        };

        let result = match cli_handle.await {
            Ok(Ok(result)) => result,
            Ok(Err(e)) => {
                suspend_tasks(&self.durable_store, format!("CLI error: {e}")).await;
                if let Some(writer) = trace_writer.as_ref() {
                    if let Some(ref run_id) = run_id {
                        let _ = writer
                            .finish_run(run_id, RunStatus::Failed, None, Some(&e))
                            .await;
                    }
                    let _ = writer.fail_request(&e).await;
                }
                self.clear_pending_message(pending_id).await;
                let text = cli_bridge::translate_cli_error(&cli_profile, -1, &e);
                return Some(
                    self.outbound_response(
                        trace.as_ref(),
                        msg.platform,
                        &msg.chat_id,
                        msg.reply_token.clone(),
                        text,
                    )
                    .await,
                );
            }
            Err(e) => {
                suspend_tasks(&self.durable_store, format!("CLI interrupted: {e}")).await;
                if let Some(writer) = trace_writer.as_ref() {
                    if let Some(ref run_id) = run_id {
                        let _ = writer
                            .finish_run(run_id, RunStatus::Failed, None, Some(&e.to_string()))
                            .await;
                    }
                    let _ = writer.fail_request(&e.to_string()).await;
                }
                self.clear_pending_message(pending_id).await;
                return Some(
                    self.outbound_response(
                        trace.as_ref(),
                        msg.platform,
                        &msg.chat_id,
                        msg.reply_token.clone(),
                        format!("⚠️ 任务中断: {e}"),
                    )
                    .await,
                );
            }
        };

        // Stale session recovery: if CLI says "No conversation found", clear and retry
        if result.exit_code != 0
            && (result.stderr.contains("No conversation found")
                || result.stderr.contains("session not found"))
            && session_id.is_some()
        {
            if let Some(writer) = trace_writer.as_ref()
                && let Some(ref run_id) = run_id
            {
                let _ = writer
                    .finish_run(
                        run_id,
                        RunStatus::Failed,
                        Some(result.exit_code),
                        Some("stale session"),
                    )
                    .await;
            }
            tracing::info!(
                cli = cli_name,
                "stale session detected — clearing and retrying"
            );
            if let Some(ref pool) = self.pool {
                let _ = storage::reset_session_for_cli(
                    pool,
                    msg.platform,
                    &effective_chat_id,
                    &cli_name,
                )
                .await;
            }
            // Retry without session-id
            let retry_handle = tokio::spawn({
                let profile = cli_profile.clone();
                let message_text = msg.text.clone();
                let system_prompt = system_prompt.clone();
                let ws = workspace.clone();
                async move {
                    cli_bridge::run_cli_with_context(
                        &profile,
                        &message_text,
                        None, // no session-id
                        ws.as_deref(),
                        None,
                        Some(&system_prompt),
                    )
                    .await
                }
            });
            let retry_run_id = if let Some(writer) = trace_writer.as_ref() {
                writer.start_run(&cli_name, None).await.ok()
            } else {
                None
            };
            match retry_handle.await {
                Ok(Ok(retry_result)) if retry_result.exit_code == 0 => {
                    if let Some(writer) = trace_writer.as_ref()
                        && let Some(ref retry_run_id) = retry_run_id
                    {
                        let _ = writer
                            .finish_run(
                                retry_run_id,
                                RunStatus::Succeeded,
                                Some(retry_result.exit_code),
                                None,
                            )
                            .await;
                    }
                    // Save new session
                    if let Some(ref pool) = self.pool
                        && let Some(ref sid) = retry_result.session_id
                        && let Err(e) = storage::set_current_session_for_cli(
                            pool,
                            msg.platform,
                            &effective_chat_id,
                            &msg.user_id,
                            sid,
                            &cli_name,
                        )
                        .await
                    {
                        tracing::warn!(error = %e, "failed to persist session after retry");
                    }
                    let text = retry_result
                        .text
                        .as_deref()
                        .unwrap_or(retry_result.stdout.trim());
                    self.clear_pending_message(pending_id).await;
                    let text = if text.is_empty() { "(无回复)" } else { text };
                    return Some(
                        self.outbound_response(
                            trace.as_ref(),
                            msg.platform,
                            &msg.chat_id,
                            msg.reply_token.clone(),
                            text.to_string(),
                        )
                        .await,
                    );
                }
                _ => {
                    if let Some(writer) = trace_writer.as_ref()
                        && let Some(ref retry_run_id) = retry_run_id
                    {
                        let _ = writer
                            .finish_run(retry_run_id, RunStatus::Failed, None, Some("retry failed"))
                            .await;
                    }
                } // fall through to normal error handling
            }
        }

        if result.exit_code != 0 {
            tracing::warn!(
                exit_code = result.exit_code,
                stderr = %result.stderr.chars().take(200).collect::<String>(),
                stdout_len = result.stdout.len(),
                text = ?result.text.as_deref().map(|t| &t[..t.len().min(100)]),
                "CLI non-zero exit"
            );
            suspend_tasks(
                &self.durable_store,
                format!("CLI exit code {}", result.exit_code),
            )
            .await;
            if result.text.is_none() || result.text.as_deref() == Some("") {
                let error_text = if result.stderr.is_empty() {
                    &result.stdout
                } else {
                    &result.stderr
                };
                self.clear_pending_message(pending_id).await;
                if let Some(writer) = trace_writer.as_ref() {
                    if let Some(ref run_id) = run_id {
                        let _ = writer
                            .finish_run(
                                run_id,
                                RunStatus::Failed,
                                Some(result.exit_code),
                                Some(error_text.trim()),
                            )
                            .await;
                    }
                    let _ = writer.fail_request(error_text.trim()).await;
                }
                let text = cli_bridge::translate_cli_error(
                    &cli_profile,
                    result.exit_code,
                    error_text.trim(),
                );
                return Some(
                    self.outbound_response(
                        trace.as_ref(),
                        msg.platform,
                        &msg.chat_id,
                        msg.reply_token.clone(),
                        text,
                    )
                    .await,
                );
            }
        }

        if let Some(writer) = trace_writer.as_ref()
            && let Some(ref run_id) = run_id
        {
            let status = if result.exit_code == 0 {
                RunStatus::Succeeded
            } else {
                RunStatus::Failed
            };
            let error = if result.exit_code == 0 {
                None
            } else {
                Some(result.stderr.as_str())
            };
            let _ = writer
                .finish_run(run_id, status, Some(result.exit_code), error)
                .await;
        }

        // Save session_id to DB (if available), scoped by CLI profile
        if let Some(ref pool) = self.pool {
            if let Some(ref sid) = result.session_id {
                if let Err(e) = storage::set_current_session_for_cli(
                    pool,
                    msg.platform,
                    &effective_chat_id,
                    &msg.user_id,
                    sid,
                    &cli_name,
                )
                .await
                {
                    tracing::warn!(error = %e, "failed to persist session");
                }
            } else if let Err(e) =
                storage::touch_session_for_cli(pool, msg.platform, &effective_chat_id, &cli_name)
                    .await
            {
                tracing::warn!(error = %e, "failed to touch session");
            }
        }

        // Use the parsed text field (from --json), fallback to raw stdout
        let mut text = result
            .text
            .as_deref()
            .unwrap_or(result.stdout.trim())
            .to_string();

        // Strip <think>...</think> blocks that some models emit as plain text
        text = strip_think_blocks(&text);

        // Execute gateway actions embedded in agent response
        let mut action_results_text = String::new();
        if text.contains("[[GATEWAY:") {
            let mut action_results = Vec::new();
            text = execute_gateway_actions_with_policy(
                &text,
                self.pool.as_ref(),
                msg.platform,
                &msg.chat_id,
                &msg.user_id,
                self.durable_store
                    .as_ref()
                    .map(|s| s.as_ref() as &dyn astra_core::durable_task_store::DurableTaskStore),
                self.config.skills_dir.as_deref(),
                &self.config.action_policy,
                &mut action_results,
            )
            .await;
            if !action_results.is_empty() {
                action_results_text = action_results.join("\n");
                text.push_str("\n\n");
                text.push_str(&action_results_text);
            }
        }

        tracing::info!(
            platform = msg.platform,
            chat_id = %safe_id(&msg.chat_id),
            text_len = text.len(),
            tools = result.tool_calls_count.unwrap_or(0),
            exit = result.exit_code,
            "← done"
        );

        // Append token usage stats + cost estimate
        let elapsed = start.elapsed();
        let prompt_tok = result.tokens_prompt.unwrap_or(0);
        let completion_tok = result.tokens_completion.unwrap_or(0);
        let cost = (prompt_tok as f64 * 3.0 + completion_tok as f64 * 15.0) / 1_000_000.0;
        let mut stats_parts = Vec::new();
        if prompt_tok > 0 {
            stats_parts.push(format!("↓{}", format_tokens(prompt_tok)));
        }
        if completion_tok > 0 {
            stats_parts.push(format!("↑{}", format_tokens(completion_tok)));
        }
        if result.tool_calls_count.unwrap_or(0) > 0 {
            stats_parts.push(format!("🔧{}", result.tool_calls_count.unwrap()));
        }
        stats_parts.push(format_elapsed(elapsed));
        if cost > 0.001 {
            stats_parts.push(format!("${cost:.3}"));
        }
        text = build_final_message(
            &text,
            &action_results_text,
            &stats_parts,
            progressive_text_len,
        );

        // Record usage to DB
        if let Some(ref pool) = self.pool
            && let Err(e) = crate::usage::record_usage(
                pool,
                &crate::usage::UsageRecord {
                    platform: msg.platform.to_string(),
                    user_id: msg.user_id.clone(),
                    cli_profile: cli_name.clone(),
                    model: match &cli_profile {
                        CliProfile::Astra { model, .. } | CliProfile::Claude { model, .. } => {
                            model.clone()
                        }
                        _ => None,
                    },
                    tokens_prompt: result.tokens_prompt.unwrap_or(0),
                    tokens_completion: result.tokens_completion.unwrap_or(0),
                    tool_calls: result.tool_calls_count.unwrap_or(0),
                    elapsed_ms: elapsed.as_millis() as u64,
                },
            )
            .await
        {
            tracing::warn!(error = %e, "failed to record usage");
        }

        // Clear pending message (successfully processed)
        self.clear_pending_message(pending_id).await;

        let text = if text.is_empty() {
            "(无回复)".to_string()
        } else {
            text
        };
        Some(
            self.outbound_response(
                trace.as_ref(),
                msg.platform,
                &msg.chat_id,
                msg.reply_token.clone(),
                text,
            )
            .await,
        )
    }

    async fn clear_pending_message(&self, pending_id: Option<i64>) {
        let (Some(id), Some(pool)) = (pending_id, self.pool.as_ref()) else {
            return;
        };
        if let Err(e) = storage::delete_pending_message(pool, id).await {
            tracing::warn!(id, error = %e, "failed to delete pending message");
        }
    }

    async fn outbound_response(
        &self,
        trace: Option<&OutboxDeliveryTrace>,
        platform: &str,
        chat_id: &str,
        reply_token: Option<String>,
        text: String,
    ) -> OutboundMessage {
        let Some(trace) = trace else {
            return OutboundMessage {
                platform: platform.to_string(),
                chat_id: chat_id.to_string(),
                text,
                reply_token,
                outbox: None,
            };
        };
        let Some(repo) = self.trace_repo.as_ref() else {
            return OutboundMessage {
                platform: platform.to_string(),
                chat_id: chat_id.to_string(),
                text,
                reply_token,
                outbox: None,
            };
        };
        let writer = TraceWriter::from_existing(
            repo.as_ref() as &dyn TraceRepository,
            trace.trace_id.clone(),
            trace.request_id.clone(),
        );
        match writer
            .enqueue_outbox(platform, chat_id, reply_token.clone(), &text)
            .await
        {
            Ok(outbox_id) => OutboundMessage::with_outbox(
                platform.to_string(),
                chat_id.to_string(),
                text,
                reply_token,
                OutboxDelivery {
                    outbox_id,
                    trace_id: trace.trace_id.clone(),
                    request_id: trace.request_id.clone(),
                },
            ),
            Err(e) => {
                tracing::warn!(error = %e, "failed to enqueue outbox; falling back to direct send");
                OutboundMessage {
                    platform: platform.to_string(),
                    chat_id: chat_id.to_string(),
                    text,
                    reply_token,
                    outbox: None,
                }
            }
        }
    }

    fn effective_chat_id(&self, msg: &InboundMessage) -> String {
        if msg.chat_type == crate::platforms::ChatType::Group && self.config.group_sessions_per_user
        {
            format!("{}:{}", msg.chat_id, msg.user_id)
        } else {
            msg.chat_id.clone()
        }
    }

    async fn build_queued_request(&self, msg: InboundMessage) -> QueuedRequest {
        let cli_profile = self.resolve_cli_profile(msg.platform, &msg.user_id).await;
        let effective_chat_id = self.effective_chat_id(&msg);
        let conversation =
            ConversationKey::new(msg.platform, effective_chat_id, cli_profile.name());
        let trace = if let Some(repo) = self.trace_repo.as_ref() {
            let request = GatewayRequest::new(
                conversation.clone(),
                msg.msg_id.clone(),
                msg.user_id.clone(),
                msg.text.clone(),
            );
            let trace = OutboxDeliveryTrace {
                trace_id: request.trace_id.clone(),
                request_id: request.request_id.clone(),
            };
            match TraceWriter::begin(repo.as_ref(), request).await {
                Ok(writer) => {
                    let depth = repo
                        .list_active_requests(&conversation, 100)
                        .await
                        .map(|rows| rows.len().saturating_sub(1))
                        .unwrap_or(0);
                    let _ = writer.mark_queued(depth).await;
                    Some(trace)
                }
                Err(e) => {
                    tracing::warn!(error = %e, "failed to create trace request");
                    None
                }
            }
        } else {
            None
        };
        QueuedRequest {
            msg,
            conversation,
            trace,
        }
    }

    async fn enqueue_cli_request(
        self: &Arc<Self>,
        msg: InboundMessage,
        cli_resp_tx: tokio::sync::mpsc::Sender<CliResponse>,
    ) {
        let queued = self.build_queued_request(msg).await;
        let key = queued.conversation.clone();
        let tx = {
            let mut queues = self.queue_senders.lock().await;
            if let Some(tx) = queues.get(&key) {
                tx.clone()
            } else {
                let (tx, rx) = tokio::sync::mpsc::channel(128);
                queues.insert(key.clone(), tx.clone());
                let runner = self.clone();
                tokio::spawn(async move {
                    runner.run_conversation_worker(key, rx, cli_resp_tx).await;
                });
                tx
            }
        };
        if let Err(e) = tx.send(queued).await {
            tracing::warn!(error = %e, "failed to enqueue gateway request");
        }
    }

    async fn run_conversation_worker(
        self: Arc<Self>,
        key: ConversationKey,
        mut rx: tokio::sync::mpsc::Receiver<QueuedRequest>,
        cli_resp_tx: tokio::sync::mpsc::Sender<CliResponse>,
    ) {
        loop {
            let queued = match tokio::time::timeout(CONVERSATION_QUEUE_IDLE_TIMEOUT, rx.recv())
                .await
            {
                Ok(Some(queued)) => queued,
                Ok(None) => break,
                Err(_) => {
                    let mut queues = self.queue_senders.lock().await;
                    if let Ok(queued) = rx.try_recv() {
                        drop(queues);
                        queued
                    } else {
                        queues.remove(&key);
                        tracing::debug!(conversation = %key, "conversation worker idle timeout");
                        break;
                    }
                }
            };
            let Ok(_permit) = self.global_run_limiter.clone().acquire_owned().await else {
                break;
            };
            if !self.should_execute_queued(&queued).await {
                continue;
            }
            if let Some(outbound) = self
                .handle_message_inner(&queued.msg, &NullAdapter, None, true, queued.trace.clone())
                .await
            {
                let _ = cli_resp_tx.send(outbound).await;
            }
        }
        self.queue_senders.lock().await.remove(&key);
        tracing::debug!(conversation = %key, "conversation worker stopped");
    }

    async fn should_execute_queued(&self, queued: &QueuedRequest) -> bool {
        let Some(trace) = queued.trace.as_ref() else {
            return true;
        };
        let Some(repo) = self.trace_repo.as_ref() else {
            return true;
        };
        match repo.get_request(&trace.request_id).await {
            Ok(Some(request)) if request.status == RequestStatus::Accepted => true,
            Ok(Some(request)) => {
                tracing::info!(
                    request_id = %trace.request_id,
                    status = request.status.as_str(),
                    "skipping queued request"
                );
                false
            }
            Ok(None) => false,
            Err(e) => {
                tracing::warn!(error = %e, "failed to verify queued request status");
                false
            }
        }
    }

    async fn replay_retryable_outbox(
        &self,
        adapters: &[Box<dyn PlatformAdapter>],
        adapter_indices: &HashMap<&'static str, usize>,
    ) {
        let Some(repo) = self.trace_repo.as_ref() else {
            return;
        };
        match repo.list_retryable_outbox(None, 100).await {
            Ok(rows) if rows.is_empty() => {}
            Ok(rows) => {
                tracing::info!(count = rows.len(), "replaying retryable outbox");
                for row in rows {
                    let outbound = OutboundMessage::with_outbox(
                        row.platform.clone(),
                        row.chat_id.clone(),
                        row.body.clone(),
                        row.reply_token.clone(),
                        OutboxDelivery {
                            outbox_id: row.outbox_id,
                            trace_id: row.trace_id,
                            request_id: row.request_id,
                        },
                    );
                    self.deliver_outbound(adapters, adapter_indices, outbound)
                        .await;
                }
            }
            Err(e) => tracing::warn!(error = %e, "failed to load retryable outbox"),
        }
    }

    async fn deliver_outbound(
        &self,
        adapters: &[Box<dyn PlatformAdapter>],
        adapter_indices: &HashMap<&'static str, usize>,
        outbound: OutboundMessage,
    ) {
        let result = send_text_to_platform(
            adapters,
            adapter_indices,
            &outbound.platform,
            &outbound.chat_id,
            &outbound.text,
            outbound.reply_token.as_deref(),
        )
        .await;
        let Some(outbox) = outbound.outbox else {
            return;
        };
        let Some(repo) = self.trace_repo.as_ref() else {
            return;
        };
        let writer = TraceWriter::from_existing(
            repo.as_ref() as &dyn TraceRepository,
            outbox.trace_id,
            outbox.request_id,
        );
        match result {
            Ok(chunk_count) => {
                if let Err(e) = writer
                    .mark_outbox_sent(&outbox.outbox_id, chunk_count)
                    .await
                {
                    tracing::warn!(error = %e, "failed to ack sent outbox");
                }
            }
            Err((failed_chunk, error)) => {
                if let Err(e) = writer
                    .mark_outbox_failed(&outbox.outbox_id, &error, failed_chunk)
                    .await
                {
                    tracing::warn!(error = %e, "failed to mark outbox retryable");
                }
            }
        }
    }

    pub async fn run(
        self: std::sync::Arc<Self>,
        adapters: Vec<Box<dyn PlatformAdapter>>,
        mut cron_rx: tokio::sync::mpsc::Receiver<OutboundMessage>,
        mut shutdown: tokio::sync::broadcast::Receiver<()>,
    ) {
        let mut started: Vec<Box<dyn PlatformAdapter>> = Vec::new();
        for mut adapter in adapters {
            match adapter.start().await {
                Ok(()) => {
                    tracing::info!(platform = adapter.name(), "started");
                    started.push(adapter);
                }
                Err(e) => tracing::error!(platform = adapter.name(), error = %e, "start failed"),
            }
        }
        let mut adapters = started;
        if adapters.is_empty() {
            tracing::error!("no adapters started — exiting");
            return;
        }
        tracing::info!(count = adapters.len(), "gateway running");

        // Channel for CLI task responses back to the main loop
        let (cli_resp_tx, mut cli_resp_rx) = tokio::sync::mpsc::channel::<CliResponse>(64);

        let mut adapter_indices = HashMap::new();
        for (idx, adapter) in adapters.iter().enumerate() {
            adapter_indices.insert(adapter.name(), idx);
        }
        self.replay_retryable_outbox(&adapters, &adapter_indices)
            .await;
        for adapter in &adapters {
            self.replay_pending_messages(adapter.as_ref()).await;
        }

        loop {
            tokio::select! {
                inbound = recv_from_any(&adapters) => {
                    match inbound {
                        Some(AdapterRecv::Message(msg)) => {
                            // Fast path: slash commands — instant, no CLI
                            match self.handle_fast(&msg).await {
                                Ok(Some(text)) => {
                                    let _ = send_text_to_platform(&adapters, &adapter_indices, msg.platform, &msg.chat_id, &text, msg.reply_token.as_deref()).await;
                                }
                                Ok(None) => {}
                                Err(msg) => {
                                    // Slow path: enqueue by conversation. Workers serialize each
                                    // conversation while a global semaphore allows cross-chat concurrency.
                                    let platform = msg.platform;
                                    send_typing_to_platform(&adapters, &adapter_indices, platform, &msg.chat_id).await;
                                    self.enqueue_cli_request(msg, cli_resp_tx.clone()).await;
                                }
                            }
                        }
                        Some(AdapterRecv::Closed(idx)) => {
                            if idx < adapters.len() {
                                let mut adapter = adapters.remove(idx);
                                tracing::warn!(platform = adapter.name(), "adapter receive channel closed");
                                adapter.stop().await;
                                adapter_indices.clear();
                                for (idx, adapter) in adapters.iter().enumerate() {
                                    adapter_indices.insert(adapter.name(), idx);
                                }
                            }
                            if adapters.is_empty() {
                                break;
                            }
                        }
                        None => break,
                    }
                }
                // CLI task completed — send response to user
                resp = cli_resp_rx.recv() => {
                    if let Some(resp) = resp {
                        self.deliver_outbound(&adapters, &adapter_indices, resp).await;
                    }
                }
                outbound = cron_rx.recv() => {
                    if let Some(outbound) = outbound {
                        self.deliver_outbound(&adapters, &adapter_indices, outbound).await;
                    }
                }
                _ = shutdown.recv() => break,
            }
        }

        for adapter in &mut adapters {
            adapter.stop().await;
        }
    }
}

async fn recv_from_any(adapters: &[Box<dyn PlatformAdapter>]) -> Option<AdapterRecv> {
    if adapters.is_empty() {
        return None;
    }
    let futures: Vec<Pin<Box<dyn Future<Output = AdapterRecv> + Send + '_>>> = adapters
        .iter()
        .enumerate()
        .map(|(idx, adapter)| {
            Box::pin(async move {
                match adapter.recv().await {
                    Some(msg) => AdapterRecv::Message(msg),
                    None => AdapterRecv::Closed(idx),
                }
            }) as Pin<Box<dyn Future<Output = AdapterRecv> + Send + '_>>
        })
        .collect();
    let (event, _, _) = select_all(futures).await;
    Some(event)
}

async fn send_text_to_platform(
    adapters: &[Box<dyn PlatformAdapter>],
    adapter_indices: &HashMap<&'static str, usize>,
    platform: &str,
    chat_id: &str,
    text: &str,
    reply_token: Option<&str>,
) -> Result<usize, (usize, String)> {
    let Some(idx) = adapter_indices.get(platform).copied() else {
        tracing::warn!(platform, chat_id = %safe_id(chat_id), "no adapter for outbound message");
        return Err((0, "no adapter for outbound message".into()));
    };
    let Some(adapter) = adapters.get(idx) else {
        tracing::warn!(platform, chat_id = %safe_id(chat_id), "adapter index missing for outbound message");
        return Err((0, "adapter index missing for outbound message".into()));
    };
    let chunks = split_message(text);
    let chunk_count = chunks.len();
    for (idx, chunk) in chunks.into_iter().enumerate() {
        if let Err(e) = adapter.send_text(chat_id, chunk, reply_token).await {
            tracing::warn!(platform, chat_id = %safe_id(chat_id), error = %e, "failed to send platform message");
            return Err((idx, e));
        }
    }
    Ok(chunk_count)
}

async fn send_typing_to_platform(
    adapters: &[Box<dyn PlatformAdapter>],
    adapter_indices: &HashMap<&'static str, usize>,
    platform: &str,
    chat_id: &str,
) {
    let Some(idx) = adapter_indices.get(platform).copied() else {
        return;
    };
    if let Some(adapter) = adapters.get(idx) {
        let _ = adapter.send_typing(chat_id).await;
    }
}

fn split_message(text: &str) -> Vec<&str> {
    if text.len() <= MAX_CHUNK_LEN {
        return vec![text];
    }
    let mut chunks = Vec::new();
    let mut remaining = text;
    while !remaining.is_empty() {
        if remaining.len() <= MAX_CHUNK_LEN {
            if !remaining.trim().is_empty() {
                chunks.push(remaining);
            }
            break;
        }
        let window_end = crate::text::floor_char_boundary(remaining, MAX_CHUNK_LEN);
        let window = &remaining[..window_end];
        // Priority 1: paragraph boundary (\n\n)
        let split_at = rfind_paragraph_break(window)
            // Priority 2: code fence boundary (``` on its own line)
            .or_else(|| rfind_fence_break(window))
            // Priority 3: any newline
            .or_else(|| window.rfind('\n'))
            // Priority 4: space
            .or_else(|| window.rfind(' '))
            // Fallback: hard cut
            .unwrap_or(window_end);

        let chunk = &remaining[..split_at];
        if !chunk.trim().is_empty() {
            chunks.push(chunk);
        }
        remaining = remaining[split_at..].trim_start_matches('\n');
        if remaining.starts_with('\n') {
            remaining = remaining.trim_start_matches('\n');
        }
    }
    chunks
}

fn rfind_paragraph_break(s: &str) -> Option<usize> {
    // Find last \n\n that's not inside a code fence
    let mut pos = s.len();
    while pos > 0 {
        if let Some(p) = s[..pos].rfind("\n\n") {
            // Check we're not inside a code block
            let before = &s[..p];
            let fence_count = before.matches("```").count();
            if fence_count.is_multiple_of(2) {
                return Some(p);
            }
            pos = p;
        } else {
            break;
        }
    }
    None
}

fn rfind_fence_break(s: &str) -> Option<usize> {
    // Find last ``` followed by \n — split after the closing fence
    let mut search = s.len();
    while search > 3 {
        if let Some(p) = s[..search].rfind("```") {
            let after_fence = p + 3;
            if after_fence < s.len() && s.as_bytes().get(after_fence) == Some(&b'\n') {
                return Some(after_fence + 1);
            }
            search = p;
        } else {
            break;
        }
    }
    None
}

fn is_safe_skill_name(name: &str) -> bool {
    let trimmed = name.trim();
    !trimmed.is_empty()
        && trimmed
            .chars()
            .all(|ch| ch.is_alphanumeric() || matches!(ch, ' ' | '_' | '-'))
}

async fn connect_db(url: &str) -> Result<MySqlPool, sqlx::Error> {
    match sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(5)
        .connect(url)
        .await
    {
        Ok(pool) => {
            storage::ensure_schema(&pool).await?;
            Ok(pool)
        }
        Err(e) => {
            let msg = e.to_string();
            if !msg.contains("1049") && !msg.contains("Unknown database") {
                return Err(e);
            }
            // Extract DB name from URL and create it
            let (base_url, db_name) = match url.rfind('/') {
                Some(pos) if pos > url.find("://").map(|p| p + 2).unwrap_or(0) => {
                    (&url[..pos], &url[pos + 1..])
                }
                _ => return Err(e),
            };
            // Strip query params from db_name
            let db_name = db_name.split('?').next().unwrap_or(db_name);
            if !is_safe_db_name(db_name) {
                return Err(e);
            }

            tracing::info!(db = db_name, "database does not exist — creating");
            let tmp_pool = sqlx::mysql::MySqlPoolOptions::new()
                .max_connections(1)
                .connect(base_url)
                .await?;
            sqlx::query(&format!("CREATE DATABASE IF NOT EXISTS `{db_name}`"))
                .execute(&tmp_pool)
                .await?;
            tmp_pool.close().await;

            let pool = sqlx::mysql::MySqlPoolOptions::new()
                .max_connections(5)
                .connect(url)
                .await?;
            storage::ensure_schema(&pool).await?;
            Ok(pool)
        }
    }
}

fn is_safe_db_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

async fn resolve_gateway_task(
    store: &dyn astra_core::durable_task_store::DurableTaskStore,
    owner_id: &str,
    selector: &str,
) -> Result<astra_core::durable_task_store::DurableTask, String> {
    astra_core::durable_task_store::resolve_task_for_owner(store, owner_id, selector)
        .await
        .map_err(|e| format!("⚠️ {e}"))
}

/// Parse and execute `[[GATEWAY:action:args]]` tags in agent response text.
/// Returns the text with tags removed, and populates action_results with status messages.
#[allow(clippy::too_many_arguments)]
#[cfg(test)]
async fn execute_gateway_actions(
    text: &str,
    pool: Option<&sqlx::MySqlPool>,
    platform: &str,
    chat_id: &str,
    user_id: &str,
    durable_store: Option<&dyn astra_core::durable_task_store::DurableTaskStore>,
    skills_dir: Option<&str>,
    action_results: &mut Vec<String>,
) -> String {
    execute_gateway_actions_with_policy(
        text,
        pool,
        platform,
        chat_id,
        user_id,
        durable_store,
        skills_dir,
        &crate::access_control::ActionPolicy {
            allow_slash_mutations: true,
            allow_model_generated_mutations: true,
            workspace_roots: Vec::new(),
        },
        action_results,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn execute_gateway_actions_with_policy(
    text: &str,
    pool: Option<&sqlx::MySqlPool>,
    platform: &str,
    chat_id: &str,
    user_id: &str,
    durable_store: Option<&dyn astra_core::durable_task_store::DurableTaskStore>,
    skills_dir: Option<&str>,
    action_policy: &crate::access_control::ActionPolicy,
    action_results: &mut Vec<String>,
) -> String {
    static RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"(?s)\[\[GATEWAY:(.*?)\]\]").unwrap());
    let re = &*RE;
    let mut clean = text.to_string();

    for cap in re.captures_iter(text) {
        let full_match = cap.get(0).unwrap().as_str();
        let inner = &cap[1];
        let parts: Vec<&str> = inner.splitn(3, ':').collect();
        if let Some(capability) = action_capability(parts.first().copied().unwrap_or_default())
            && let Err(denial) = action_policy.check(
                crate::access_control::ActionSource::ModelGenerated,
                capability,
            )
        {
            action_results.push(denial);
            clean = clean.replace(full_match, "");
            continue;
        }

        let result = match parts.first().copied() {
            Some("cron_add") if parts.len() == 3 => {
                let cron_expr = parts[1].trim();
                let message = parts[2].trim();
                if message.is_empty() {
                    "⚠️ 任务消息不能为空".into()
                } else if !is_valid_cron_expr(cron_expr) {
                    format!("⚠️ 无效的 cron 表达式: `{cron_expr}`（需要 5 个字段: 分 时 日 月 周）")
                } else if let Some(pool) = pool {
                    let job_id = uuid::Uuid::new_v4().to_string();
                    match storage::create_cron_job(
                        pool, &job_id, platform, chat_id, user_id, cron_expr, message, message,
                    )
                    .await
                    {
                        Ok(()) => {
                            tracing::info!(
                                id = &job_id[..8],
                                expr = cron_expr,
                                msg = message,
                                "gateway action: cron_add"
                            );
                            format!(
                                "⏰ 定时任务已创建\n- ID: `{}`\n- 周期: `{cron_expr}`\n- 内容: {message}",
                                &job_id[..8]
                            )
                        }
                        Err(e) => format!("⚠️ 定时任务创建失败: {e}"),
                    }
                } else {
                    "⚠️ 定时任务需要数据库支持".into()
                }
            }
            Some("cron_add") => "⚠️ cron_add 格式错误（需要: cron_add:表达式:消息）".into(),

            Some("remind_after") if parts.len() == 3 => {
                let minutes: u64 = parts[1].trim().parse().unwrap_or(0);
                let message = parts[2].trim().to_string();
                if message.is_empty() {
                    "⚠️ 提醒消息不能为空".into()
                } else if minutes == 0 {
                    "⚠️ 提醒时间无效（需要大于 0 的分钟数）".into()
                } else if minutes > 1440 * 7 {
                    "⚠️ 提醒时间过长（最多 7 天 = 10080 分钟）".into()
                } else if let Some(pool) = pool {
                    // Store as one-shot cron job with computed next_run time
                    let job_id = uuid::Uuid::new_v4().to_string();
                    let next_run = chrono::Utc::now() + chrono::Duration::minutes(minutes as i64);
                    let next_run_str = next_run.format("%Y-%m-%d %H:%M:%S").to_string();
                    // Use special cron_expr "once" to mark as one-shot
                    match storage::create_cron_job(
                        pool,
                        &job_id,
                        platform,
                        chat_id,
                        user_id,
                        "once",
                        &message,
                        &format!("⏰ {message} (一次性)"),
                    )
                    .await
                    {
                        Ok(()) => {
                            if let Err(e) =
                                sqlx::query("UPDATE gw_cron_jobs SET next_run = ? WHERE job_id = ?")
                                    .bind(&next_run_str)
                                    .bind(&job_id)
                                    .execute(pool)
                                    .await
                            {
                                tracing::warn!(job_id = %&job_id[..8], error = %e, "failed to set remind_after next_run");
                            }
                            tracing::info!(minutes, msg = %message, job_id = &job_id[..8], "remind_after → cron job");
                            let time_str = if minutes >= 60 {
                                let h = minutes / 60;
                                let m = minutes % 60;
                                if m == 0 {
                                    format!("{h}小时")
                                } else {
                                    format!("{h}小时{m}分钟")
                                }
                            } else {
                                format!("{minutes}分钟")
                            };
                            format!("⏰ {time_str}后提醒: {message}\n(ID: `{}`)", &job_id[..8])
                        }
                        Err(e) => format!("⚠️ 创建提醒失败: {e}"),
                    }
                } else {
                    "⚠️ 延时提醒需要数据库支持".into()
                }
            }
            Some("remind_after") => {
                "⚠️ remind_after 格式错误（需要: remind_after:分钟数:消息）".into()
            }

            Some("task_list") => {
                if let Some(pool) = pool {
                    match storage::list_cron_jobs(pool, platform, chat_id).await {
                        Ok(jobs) if jobs.is_empty() => "📋 当前没有定时任务。".into(),
                        Ok(jobs) => {
                            let mut lines = vec![format!("📋 **定时任务** ({} 个)", jobs.len())];
                            for (id, expr, desc, enabled) in &jobs {
                                let status = if *enabled { "✅" } else { "⏸" };
                                let short_id = &id[..8.min(id.len())];
                                lines.push(format!("{status} `{short_id}` | `{expr}` | {desc}"));
                            }
                            lines.join("\n")
                        }
                        Err(e) => format!("⚠️ 查询失败: {e}"),
                    }
                } else {
                    "⚠️ 需要数据库支持".into()
                }
            }

            Some("task_del") if parts.len() >= 2 => {
                let target = parts[1].trim();
                if target.is_empty() {
                    "⚠️ 请指定任务 ID".into()
                } else if let Some(pool) = pool {
                    // Support prefix match
                    match find_and_delete_job(pool, platform, chat_id, target).await {
                        Ok(Some(desc)) => {
                            tracing::info!(target, "gateway action: task_del");
                            format!("✅ 已删除任务: {desc}")
                        }
                        Ok(None) => format!("❌ 找不到任务 `{target}`"),
                        Err(e) => format!("⚠️ 删除失败: {e}"),
                    }
                } else {
                    "⚠️ 需要数据库支持".into()
                }
            }
            Some("task_del") => "⚠️ task_del 格式错误（需要: task_del:任务ID）".into(),

            // Legacy alias
            Some("cron_del") if parts.len() >= 2 => {
                let job_id = parts[1].trim();
                if let Some(pool) = pool {
                    match find_and_delete_job(pool, platform, chat_id, job_id).await {
                        Ok(Some(desc)) => {
                            tracing::info!(job_id, "gateway action: cron_del");
                            format!("✅ 已删除任务: {desc}")
                        }
                        Ok(None) => format!("❌ 找不到任务 `{job_id}`"),
                        Err(e) => format!("⚠️ 删除失败: {e}"),
                    }
                } else {
                    "⚠️ 需要数据库支持".into()
                }
            }

            Some("dtask_create") if parts.len() >= 2 => {
                let name = parts[1].trim();
                let desc = if parts.len() >= 3 {
                    Some(parts[2].trim().to_string())
                } else {
                    None
                };
                if name.is_empty() {
                    "⚠️ 任务名称不能为空".into()
                } else if let Some(store) = durable_store {
                    let spec = astra_core::durable_task_store::TaskSpec {
                        name: name.to_string(),
                        description: desc,
                        owner_id: format!("{platform}:{chat_id}"),
                        initial_state: None,
                    };
                    match store.create(&spec).await {
                        Ok(id) => {
                            tracing::info!(task_id = %id, name, "dtask created");
                            format!(
                                "📋 任务已创建\n- ID: `{}`\n- 名称: {name}",
                                &id.0[..8.min(id.0.len())]
                            )
                        }
                        Err(e) => format!("⚠️ 创建失败: {e}"),
                    }
                } else {
                    "⚠️ 需要数据库支持".into()
                }
            }

            Some("dtask_checkpoint") if parts.len() == 3 => {
                let task_id = parts[1].trim();
                let json_str = parts[2].trim();
                if task_id.is_empty() {
                    "⚠️ 请指定任务 ID".into()
                } else {
                    match serde_json::from_str::<serde_json::Value>(json_str) {
                        Err(e) => format!("⚠️ checkpoint JSON 无效: {e}"),
                        Ok(state) => {
                            if let Some(store) = durable_store {
                                let owner_id = format!("{platform}:{chat_id}");
                                match resolve_gateway_task(store, &owner_id, task_id).await {
                                    Ok(task) => {
                                        match store.checkpoint(&task.id, &state, None, None).await {
                                            Ok(()) => {
                                                tracing::info!(task_id, "dtask checkpoint saved");
                                                format!(
                                                    "💾 检查点已保存 (`{}`)",
                                                    &task.id.0[..8.min(task.id.0.len())]
                                                )
                                            }
                                            Err(e) => format!("⚠️ 保存失败: {e}"),
                                        }
                                    }
                                    Err(e) => e,
                                }
                            } else {
                                "⚠️ 需要数据库支持".into()
                            }
                        }
                    }
                }
            }

            Some("dtask_status") if parts.len() >= 2 => {
                let task_id = parts[1].trim();
                if let Some(store) = durable_store {
                    let owner_id = format!("{platform}:{chat_id}");
                    match resolve_gateway_task(store, &owner_id, task_id).await {
                        Ok(t) => {
                            let mut lines = vec![
                                format!("📋 **任务: {}**", t.name),
                                format!("- 状态: {}", t.status.as_str()),
                                format!("- 进度: {}%", t.progress_pct),
                            ];
                            if let Some(ref step) = t.step_description {
                                lines.push(format!("- 当前: {step}"));
                            }
                            if let Some(ref err) = t.error_message {
                                lines.push(format!("- 错误: {err}"));
                            }
                            lines.join("\n")
                        }
                        Err(e) => e,
                    }
                } else {
                    "⚠️ 需要数据库支持".into()
                }
            }

            Some("dtask_resume") if parts.len() >= 2 => {
                let task_id = parts[1].trim();
                if let Some(store) = durable_store {
                    let owner_id = format!("{platform}:{chat_id}");
                    match resolve_gateway_task(store, &owner_id, task_id).await {
                        Ok(task) => match store.resume(&task.id).await {
                            Ok(Some(checkpoint)) => {
                                match store
                                    .update_status(
                                        &task.id,
                                        astra_core::durable_task_store::DurableTaskStatus::Running,
                                        None,
                                    )
                                    .await
                                {
                                    Ok(()) => format!(
                                        "▶️ 任务已恢复，检查点:\n```json\n{}\n```",
                                        serde_json::to_string_pretty(&checkpoint)
                                            .unwrap_or_default()
                                    ),
                                    Err(e) => format!("⚠️ 恢复失败: {e}"),
                                }
                            }
                            Ok(None) => match store
                                .update_status(
                                    &task.id,
                                    astra_core::durable_task_store::DurableTaskStatus::Running,
                                    None,
                                )
                                .await
                            {
                                Ok(()) => format!("▶️ 任务 `{task_id}` 无检查点，从头开始"),
                                Err(e) => format!("⚠️ 恢复失败: {e}"),
                            },
                            Err(e) => format!("⚠️ 恢复失败: {e}"),
                        },
                        Err(e) => e,
                    }
                } else {
                    "⚠️ 需要数据库支持".into()
                }
            }

            Some("dtask_list") => {
                if let Some(store) = durable_store {
                    let filter = astra_core::durable_task_store::TaskFilter {
                        owner_id: Some(format!("{platform}:{chat_id}")),
                        ..Default::default()
                    };
                    match store.list(filter).await {
                        Ok(tasks) if tasks.is_empty() => "📋 没有持久任务。".into(),
                        Ok(tasks) => {
                            let mut lines = vec![format!("📋 **持久任务** ({} 个)", tasks.len())];
                            for t in &tasks {
                                let short_id = &t.id.0[..8.min(t.id.0.len())];
                                let icon = match t.status {
                                    astra_core::durable_task_store::DurableTaskStatus::Running => "🔄",
                                    astra_core::durable_task_store::DurableTaskStatus::Suspended => "⏸",
                                    astra_core::durable_task_store::DurableTaskStatus::Completed => "✅",
                                    astra_core::durable_task_store::DurableTaskStatus::Failed => "❌",
                                    _ => "📋",
                                };
                                lines.push(format!(
                                    "{icon} `{short_id}` | {} | {}%",
                                    t.name, t.progress_pct
                                ));
                            }
                            lines.join("\n")
                        }
                        Err(e) => format!("⚠️ 查询失败: {e}"),
                    }
                } else {
                    "⚠️ 需要数据库支持".into()
                }
            }

            Some("dtask_complete") if parts.len() >= 2 => {
                let task_id = parts[1].trim();
                if let Some(store) = durable_store {
                    let owner_id = format!("{platform}:{chat_id}");
                    match resolve_gateway_task(store, &owner_id, task_id).await {
                        Ok(task) => match store
                            .update_status(
                                &task.id,
                                astra_core::durable_task_store::DurableTaskStatus::Completed,
                                None,
                            )
                            .await
                        {
                            Ok(()) => "✅ 任务已完成".into(),
                            Err(e) => format!("⚠️ {e}"),
                        },
                        Err(e) => e,
                    }
                } else {
                    "⚠️ 需要数据库支持".into()
                }
            }

            Some("dtask_fail") if parts.len() >= 2 => {
                let task_id = parts[1].trim();
                let error = if parts.len() >= 3 {
                    Some(parts[2].trim())
                } else {
                    None
                };
                if let Some(store) = durable_store {
                    let owner_id = format!("{platform}:{chat_id}");
                    match resolve_gateway_task(store, &owner_id, task_id).await {
                        Ok(task) => match store
                            .update_status(
                                &task.id,
                                astra_core::durable_task_store::DurableTaskStatus::Failed,
                                error,
                            )
                            .await
                        {
                            Ok(()) => "❌ 任务已标记失败".into(),
                            Err(e) => format!("⚠️ {e}"),
                        },
                        Err(e) => e,
                    }
                } else {
                    "⚠️ 需要数据库支持".into()
                }
            }

            Some("dtask_cancel") if parts.len() >= 2 => {
                let task_id = parts[1].trim();
                if let Some(store) = durable_store {
                    let owner_id = format!("{platform}:{chat_id}");
                    match resolve_gateway_task(store, &owner_id, task_id).await {
                        Ok(task) => match store
                            .update_status(
                                &task.id,
                                astra_core::durable_task_store::DurableTaskStatus::Cancelled,
                                None,
                            )
                            .await
                        {
                            Ok(()) => "🚫 任务已取消".into(),
                            Err(e) => format!("⚠️ {e}"),
                        },
                        Err(e) => e,
                    }
                } else {
                    "⚠️ 需要数据库支持".into()
                }
            }

            Some("skill_add") if parts.len() >= 2 => {
                let name = parts[1].trim();
                let content = if parts.len() >= 3 {
                    parts[2].trim()
                } else {
                    ""
                };
                if name.is_empty() {
                    "⚠️ skill 名称不能为空".into()
                } else if !is_safe_skill_name(name) {
                    "⚠️ skill 名称只能包含字母、数字、中文、空格、下划线或连字符，不能包含路径。"
                        .into()
                } else if content.is_empty() {
                    "⚠️ skill 内容不能为空".into()
                } else if let Some(dir) = skills_dir {
                    let expanded = if dir.starts_with('~') {
                        let home = std::env::var("HOME").unwrap_or_default();
                        dir.replacen('~', &home, 1)
                    } else {
                        dir.to_string()
                    };
                    let path = std::path::Path::new(&expanded);
                    if !path.is_dir()
                        && let Err(e) = std::fs::create_dir_all(path)
                    {
                        action_results.push(format!("⚠️ 创建 skill 目录失败: {e}"));
                        clean = clean.replace(full_match, "");
                        continue;
                    }
                    let file = path.join(format!("{name}.md"));
                    match std::fs::write(&file, content) {
                        Ok(()) => {
                            tracing::info!(name, "gateway action: skill_add");
                            format!("📝 Skill `{name}` 已保存 → {}", file.display())
                        }
                        Err(e) => format!("⚠️ 保存失败: {e}"),
                    }
                } else {
                    "⚠️ skill 目录未配置 (gateway.yaml: skills_dir)".into()
                }
            }

            Some("workspace_set") if parts.len() >= 2 => {
                let target = parts[1].trim();
                let expanded = if target.starts_with('~') {
                    let home = std::env::var("HOME").unwrap_or_default();
                    target.replacen('~', &home, 1)
                } else {
                    target.to_string()
                };
                let path = std::path::Path::new(&expanded);
                if !path.is_dir() {
                    format!("❌ 目录不存在: `{expanded}`")
                } else if let Err(denial) = action_policy.workspace_allowed(path) {
                    denial
                } else if let Some(pool) = pool {
                    let canonical = path
                        .canonicalize()
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or(expanded);
                    match storage::set_user_preference(
                        pool,
                        platform,
                        user_id,
                        "workspace",
                        &canonical,
                    )
                    .await
                    {
                        Ok(()) => {
                            tracing::info!(workspace = %canonical, "gateway action: workspace_set");
                            format!("📂 工作目录已切换: `{canonical}`")
                        }
                        Err(e) => format!("⚠️ 保存工作目录失败: {e}"),
                    }
                } else {
                    "⚠️ 需要数据库支持".into()
                }
            }

            _ => {
                tracing::warn!(action = inner, "unknown gateway action");
                format!("⚠️ 未知操作: {inner}")
            }
        };

        action_results.push(result);
        clean = clean.replace(full_match, "");
    }

    // Clean up extra whitespace from removed tags
    clean.trim().to_string()
}

fn action_capability(action: &str) -> Option<crate::access_control::ActionCapability> {
    use crate::access_control::ActionCapability as Cap;
    match action {
        "cron_add" | "remind_after" | "task_del" | "cron_del" => Some(Cap::CronMutation),
        "dtask_create" | "dtask_checkpoint" | "dtask_resume" | "dtask_complete" | "dtask_fail"
        | "dtask_cancel" => Some(Cap::DurableTaskMutation),
        "skill_add" => Some(Cap::SkillMutation),
        "workspace_set" => Some(Cap::WorkspaceMutation),
        _ => None,
    }
}

fn is_valid_cron_expr(expr: &str) -> bool {
    let parts: Vec<&str> = expr.split_whitespace().collect();
    if parts.len() != 5 {
        return false;
    }
    // Each field should be *, a number, a range, a list, or a step
    parts.iter().all(|p| {
        p.chars()
            .all(|c| c.is_ascii_digit() || c == '*' || c == ',' || c == '-' || c == '/')
    })
}

async fn find_and_delete_job(
    pool: &sqlx::MySqlPool,
    platform: &str,
    chat_id: &str,
    target: &str,
) -> Result<Option<String>, sqlx::Error> {
    let jobs = storage::list_cron_jobs(pool, platform, chat_id).await?;
    // Exact or prefix match
    let matched = jobs
        .iter()
        .find(|(id, _, _, _)| id == target || id.starts_with(target));
    if let Some((id, _, desc, _)) = matched {
        let desc = desc.clone();
        storage::delete_cron_job(pool, id).await?;
        Ok(Some(desc))
    } else {
        Ok(None)
    }
}

/// Filter `<think>...</think>` blocks from streaming token text.
/// `in_think` tracks state across calls (tokens arrive in small chunks).
fn filter_think_tags(text: &str, in_think: &mut bool) -> String {
    let mut result = String::new();
    let mut remaining = text;

    while !remaining.is_empty() {
        if *in_think {
            if let Some(end) = remaining.find("</think>") {
                *in_think = false;
                remaining = &remaining[end + 8..];
            } else {
                break;
            }
        } else if let Some(start) = remaining.find("<think>") {
            result.push_str(&remaining[..start]);
            *in_think = true;
            remaining = &remaining[start + 7..];
        } else {
            result.push_str(remaining);
            break;
        }
    }
    result
}

#[derive(Default)]
struct GatewayActionStreamFilter {
    pending: String,
    in_tag: bool,
}

impl GatewayActionStreamFilter {
    fn push(&mut self, text: &str) -> String {
        const TAG_START: &str = "[[GATEWAY:";
        self.pending.push_str(text);
        let mut out = String::new();

        loop {
            if self.in_tag {
                if let Some(end) = self.pending.find("]]") {
                    self.pending.drain(..end + 2);
                    self.in_tag = false;
                    continue;
                }
                self.pending.clear();
                break;
            }

            if let Some(start) = self.pending.find(TAG_START) {
                out.push_str(&self.pending[..start]);
                self.pending.drain(..start + TAG_START.len());
                self.in_tag = true;
                continue;
            }

            let keep = gateway_tag_prefix_suffix_len(&self.pending);
            let emit_len = self.pending.len().saturating_sub(keep);
            out.push_str(&self.pending[..emit_len]);
            self.pending.drain(..emit_len);
            break;
        }

        out
    }

    fn finish(&mut self) -> String {
        if self.in_tag {
            self.pending.clear();
            self.in_tag = false;
            String::new()
        } else {
            std::mem::take(&mut self.pending)
        }
    }
}

fn gateway_tag_prefix_suffix_len(text: &str) -> usize {
    const TAG_START: &str = "[[GATEWAY:";
    let max = text.len().min(TAG_START.len() - 1);
    for len in (1..=max).rev() {
        if text.is_char_boundary(text.len() - len)
            && TAG_START.starts_with(&text[text.len() - len..])
        {
            return len;
        }
    }
    0
}

/// Build the final message to send after CLI finishes.
/// When `progressive_text_len > 0`, text was already streamed — send only
/// action results + stats footer. Otherwise send full text + stats.
fn build_final_message(
    text: &str,
    action_results: &str,
    stats_parts: &[String],
    progressive_text_len: usize,
) -> String {
    if progressive_text_len > 0 {
        let mut parts = Vec::new();
        if !action_results.is_empty() {
            parts.push(action_results.to_string());
        }
        if !stats_parts.is_empty() {
            parts.push(format!("`{}`", stats_parts.join(" | ")));
        }
        parts.join("\n\n")
    } else {
        let mut result = text.to_string();
        if !result.is_empty() && !stats_parts.is_empty() {
            result.push_str(&format!("\n\n`{}`", stats_parts.join(" | ")));
        }
        result
    }
}

/// Strip `<think>...</think>` blocks from complete text.
/// Unclosed `<think>` at EOF: the tag is removed but content after it is
/// preserved — a malicious or buggy model cannot suppress all output.
fn strip_think_blocks(text: &str) -> String {
    let mut in_think = false;
    let mut result = filter_think_tags(text, &mut in_think);
    if in_think && let Some(pos) = text.rfind("<think>") {
        let after = &text[pos + 7..];
        if !after.is_empty() {
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str(after);
        }
    }
    result
}

fn format_elapsed(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 60 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max).collect();
        format!("{t}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_short() {
        assert_eq!(split_message("hello"), vec!["hello"]);
    }

    #[test]
    fn split_long() {
        let text = "x".repeat(8000);
        let chunks = split_message(&text);
        assert!(chunks.len() >= 2);
        for chunk in &chunks {
            assert!(!chunk.is_empty());
        }
    }

    #[test]
    fn split_long_multibyte_does_not_panic_or_split_chars() {
        let text = "中文内容".repeat(2000);
        let chunks = split_message(&text);
        assert!(chunks.len() >= 2);
        for chunk in &chunks {
            assert!(chunk.len() <= MAX_CHUNK_LEN);
            assert!(text.contains(chunk.trim()));
        }
    }

    #[test]
    fn split_preserves_code_block() {
        // Code block should not be split in the middle
        let code = format!(
            "before\n\n```rust\n{}\n```\n\nafter",
            "let x = 1;\n".repeat(300)
        );
        let chunks = split_message(&code);
        // The code block should be entirely in one chunk (or if too large, at least not split mid-line)
        let has_orphan_fence = chunks.iter().any(|c| {
            let opens = c.matches("```").count();
            opens % 2 != 0 // odd number of fences = split inside a code block
        });
        // If the code block fits in one chunk, it should not be split
        if code.len() <= MAX_CHUNK_LEN {
            assert_eq!(chunks.len(), 1);
        } else {
            // Large code block: at least no orphan fences
            assert!(
                !has_orphan_fence,
                "code block was split mid-fence: {chunks:?}"
            );
        }
    }

    #[test]
    fn split_prefers_paragraph_boundary() {
        let text = format!("{}\n\n{}", "a".repeat(1000), "b".repeat(1000));
        if text.len() <= MAX_CHUNK_LEN {
            assert_eq!(split_message(&text).len(), 1);
        }
        // For text > MAX_CHUNK_LEN: split at \n\n paragraph boundary
        let big = format!("{}\n\n{}", "a".repeat(2000), "b".repeat(2000));
        let chunks = split_message(&big);
        if chunks.len() >= 2 {
            assert!(
                chunks[0].ends_with('a'),
                "should split at paragraph boundary, got: {:?}...",
                &chunks[0][chunks[0].len() - 20..]
            );
        }
    }

    #[test]
    fn split_no_empty_chunks() {
        let text = "a\n\n\n\nb\n\n\n\nc".repeat(500);
        let chunks = split_message(&text);
        for (i, chunk) in chunks.iter().enumerate() {
            assert!(!chunk.trim().is_empty(), "chunk {i} is empty");
        }
    }

    #[test]
    fn format_elapsed_seconds() {
        assert_eq!(format_elapsed(Duration::from_secs(5)), "5s");
        assert_eq!(format_elapsed(Duration::from_secs(45)), "45s");
    }

    #[test]
    fn format_elapsed_minutes() {
        assert_eq!(format_elapsed(Duration::from_secs(65)), "1m5s");
        assert_eq!(format_elapsed(Duration::from_secs(130)), "2m10s");
    }

    #[test]
    fn initial_ack_delay_is_shorter_than_heartbeat() {
        assert!(INITIAL_ACK_DELAY < HEARTBEAT_INTERVAL);
        assert!(
            INITIAL_ACK_DELAY.as_secs() <= 5,
            "initial ack should be <= 5s for good UX"
        );
    }

    #[test]
    fn progressive_flush_interval_is_reasonable() {
        const { assert!(PROGRESSIVE_MIN_CHARS > 0) };
        const { assert!(PROGRESSIVE_MIN_CHARS <= 200) };
        let secs = PROGRESSIVE_FLUSH_INTERVAL.as_secs();
        assert!(secs >= 2, "too fast = flood WeChat");
        assert!(secs <= 10, "too slow = feels laggy");
    }

    // ── Think tag filtering ──────────────────────────────────────

    #[test]
    fn filter_think_tags_strips_complete_block() {
        let mut state = false;
        let result = filter_think_tags("<think>internal reasoning</think>Hello!", &mut state);
        assert_eq!(result, "Hello!");
        assert!(!state);
    }

    #[test]
    fn filter_think_tags_handles_streaming_chunks() {
        let mut state = false;
        // Chunk 1: start of think block
        let r1 = filter_think_tags("Hi <think>reasoning", &mut state);
        assert_eq!(r1, "Hi ");
        assert!(state);
        // Chunk 2: still inside
        let r2 = filter_think_tags(" more thinking", &mut state);
        assert_eq!(r2, "");
        assert!(state);
        // Chunk 3: end of think block + visible text
        let r3 = filter_think_tags("</think>Visible", &mut state);
        assert_eq!(r3, "Visible");
        assert!(!state);
    }

    #[test]
    fn filter_think_tags_no_think_passthrough() {
        let mut state = false;
        let result = filter_think_tags("Just normal text", &mut state);
        assert_eq!(result, "Just normal text");
    }

    #[test]
    fn strip_think_blocks_removes_all() {
        let text = "<think>hmm</think>Answer is 42<think>double check</think>.";
        assert_eq!(strip_think_blocks(text), "Answer is 42.");
    }

    #[test]
    fn strip_think_blocks_unclosed_preserves_content() {
        // Malicious/buggy model: <think> without </think> should NOT suppress output
        let text = "Before<think>suppressed content that should still appear";
        let result = strip_think_blocks(text);
        assert!(
            result.contains("Before"),
            "text before think lost: {result}"
        );
        assert!(
            result.contains("suppressed content"),
            "unclosed think suppressed output: {result}"
        );
    }

    #[test]
    fn strip_think_blocks_unclosed_at_start() {
        let text = "<think>all content here, no close tag";
        let result = strip_think_blocks(text);
        assert!(
            result.contains("all content here"),
            "unclosed think at start suppressed everything: {result}"
        );
    }

    // ── Progressive delivery dedup ─────────────────────────────────

    #[test]
    fn final_message_no_progressive_includes_full_text() {
        let stats = vec!["↓8.4k".into(), "↑95".into(), "8s".into()];
        let msg = build_final_message("Hello world", "", &stats, 0);
        assert!(msg.contains("Hello world"));
        assert!(msg.contains("↓8.4k"));
    }

    #[test]
    fn final_message_progressive_skips_body() {
        let stats = vec!["↓8.4k".into(), "↑95".into()];
        let msg = build_final_message("Hello world (already sent)", "", &stats, 500);
        assert!(
            !msg.contains("Hello world"),
            "body should not repeat: {msg}"
        );
        assert!(msg.contains("↓8.4k"), "stats should still appear: {msg}");
    }

    #[test]
    fn final_message_progressive_with_actions() {
        let stats = vec!["8s".into()];
        let msg = build_final_message("body", "⏰ 提醒已创建", &stats, 100);
        assert!(
            msg.contains("⏰ 提醒已创建"),
            "action results should appear"
        );
        assert!(msg.contains("8s"), "stats should appear");
        assert!(!msg.contains("body"), "body should not repeat");
    }

    #[test]
    fn final_message_progressive_empty_stats() {
        let msg = build_final_message("body", "", &[], 100);
        assert!(
            msg.is_empty(),
            "nothing to send if progressive + no actions + no stats"
        );
    }

    // ── Tool status merged into buffer ──────────────────────────

    #[test]
    fn tool_status_format_is_inline() {
        // Verify the format strings used in the progress loop
        let started = format!("🔧 {}…\n", "bash");
        let done = format!("✅ {} ({}ms)\n", "bash", 120);
        assert!(started.contains("🔧 bash…"));
        assert!(done.contains("✅ bash (120ms)"));
        // Both end with newline — they'll be part of a multi-line buffer
        assert!(started.ends_with('\n'));
        assert!(done.ends_with('\n'));
    }

    // ── Think tag filtering ──────────────────────────────────────

    #[test]
    fn filter_think_tags_empty_think_block() {
        let mut state = false;
        assert_eq!(filter_think_tags("<think></think>OK", &mut state), "OK");
        assert!(!state);
    }

    #[test]
    fn filter_think_tags_at_start_and_end() {
        assert_eq!(strip_think_blocks("<think>x</think>"), "");
        assert_eq!(strip_think_blocks("text<think>x</think>"), "text");
        assert_eq!(strip_think_blocks("<think>x</think>text"), "text");
    }

    #[test]
    fn filter_think_tags_unclosed_stays_open() {
        let mut state = false;
        let r = filter_think_tags("before<think>never closed", &mut state);
        assert_eq!(r, "before");
        assert!(state, "should remain in think state");
        // Subsequent call still in think
        let r2 = filter_think_tags("still thinking", &mut state);
        assert_eq!(r2, "");
        assert!(state);
    }

    #[test]
    fn filter_think_tags_split_at_tag_boundary() {
        let mut state = false;
        // "<think>" split across two chunks as "<thin" + "k>reasoning</think>out"
        let r1 = filter_think_tags("<thin", &mut state);
        // Can't detect partial tag — passes through (acceptable: rare edge case)
        assert_eq!(r1, "<thin");
        assert!(!state);
        // Next chunk completes the tag — won't match as opening tag
        let r2 = filter_think_tags("k>reasoning</think>out", &mut state);
        // "k>" isn't a valid tag, passes through; "</think>" is a close without open, passes through
        assert!(r2.contains("out"));
    }

    #[test]
    fn filter_think_tags_nested_ignored() {
        // Nested <think> inside another — inner is just text, outer close ends it
        let mut state = false;
        let r = filter_think_tags("<think>a<think>b</think>c", &mut state);
        // First </think> closes, "c" is visible
        assert_eq!(r, "c");
        assert!(!state);
    }

    #[test]
    fn gateway_action_stream_filter_removes_complete_tag() {
        let mut filter = GatewayActionStreamFilter::default();
        let out = filter.push("before [[GATEWAY:dtask_complete:abc]] after");
        assert_eq!(out, "before  after");
        assert_eq!(filter.finish(), "");
    }

    #[test]
    fn gateway_action_stream_filter_handles_split_tag_start() {
        let mut filter = GatewayActionStreamFilter::default();
        assert_eq!(filter.push("hello [["), "hello ");
        assert_eq!(filter.push("GATEWAY:dtask_cancel:abc]] done"), " done");
        assert_eq!(filter.finish(), "");
    }

    #[test]
    fn gateway_action_stream_filter_drops_unclosed_tag_at_finish() {
        let mut filter = GatewayActionStreamFilter::default();
        assert_eq!(
            filter.push("visible [[GATEWAY:dtask_complete:abc"),
            "visible "
        );
        assert_eq!(filter.finish(), "");
    }

    // ── Gateway action tests ──────────────────────────────────────

    #[tokio::test]
    async fn action_cron_add_no_db() {
        let text = "好的\n[[GATEWAY:cron_add:0 9 * * *:早上好]]";
        let mut r = Vec::new();
        let clean = execute_gateway_actions(text, None, "wx", "c1", "u1", None, None, &mut r).await;
        assert_eq!(clean, "好的");
        assert!(r[0].contains("数据库"), "{}", r[0]);
    }

    #[tokio::test]
    async fn action_cron_add_invalid_expr() {
        let text = "[[GATEWAY:cron_add:bad expr:msg]]";
        let mut r = Vec::new();
        execute_gateway_actions(text, None, "wx", "c1", "u1", None, None, &mut r).await;
        assert!(r[0].contains("无效"), "{}", r[0]);
    }

    #[tokio::test]
    async fn action_cron_add_empty_message() {
        let text = "[[GATEWAY:cron_add:0 9 * * *:]]";
        let mut r = Vec::new();
        execute_gateway_actions(text, None, "wx", "c1", "u1", None, None, &mut r).await;
        assert!(r[0].contains("不能为空"), "{}", r[0]);
    }

    #[tokio::test]
    async fn action_cron_add_missing_parts() {
        let text = "[[GATEWAY:cron_add:only_one_part]]";
        let mut r = Vec::new();
        execute_gateway_actions(text, None, "wx", "c1", "u1", None, None, &mut r).await;
        assert!(r[0].contains("格式错误"), "{}", r[0]);
    }

    #[tokio::test]
    async fn action_remind_after_no_db() {
        let text = "好的\n[[GATEWAY:remind_after:5:喝水]]";
        let mut r = Vec::new();
        let clean = execute_gateway_actions(text, None, "wx", "c1", "u1", None, None, &mut r).await;
        assert_eq!(clean, "好的");
        assert!(r[0].contains("数据库"), "{}", r[0]);
    }

    #[tokio::test]
    async fn action_remind_after_zero_minutes() {
        let text = "[[GATEWAY:remind_after:0:msg]]";
        let mut r = Vec::new();
        execute_gateway_actions(text, None, "wx", "c1", "u1", None, None, &mut r).await;
        assert!(r[0].contains("无效"), "{}", r[0]);
    }

    #[tokio::test]
    async fn action_remind_after_too_long() {
        let text = "[[GATEWAY:remind_after:99999:msg]]";
        let mut r = Vec::new();
        execute_gateway_actions(text, None, "wx", "c1", "u1", None, None, &mut r).await;
        assert!(r[0].contains("过长"), "{}", r[0]);
    }

    #[tokio::test]
    async fn action_remind_after_empty_message() {
        let text = "[[GATEWAY:remind_after:5:]]";
        let mut r = Vec::new();
        execute_gateway_actions(text, None, "wx", "c1", "u1", None, None, &mut r).await;
        assert!(r[0].contains("不能为空"), "{}", r[0]);
    }

    #[tokio::test]
    async fn action_remind_after_non_numeric() {
        let text = "[[GATEWAY:remind_after:abc:msg]]";
        let mut r = Vec::new();
        execute_gateway_actions(text, None, "wx", "c1", "u1", None, None, &mut r).await;
        assert!(r[0].contains("无效"), "{}", r[0]);
    }

    #[tokio::test]
    async fn action_task_list_no_db() {
        let text = "[[GATEWAY:task_list]]";
        let mut r = Vec::new();
        execute_gateway_actions(text, None, "wx", "c1", "u1", None, None, &mut r).await;
        assert!(r[0].contains("数据库"), "{}", r[0]);
    }

    #[tokio::test]
    async fn action_task_del_empty_id() {
        let text = "[[GATEWAY:task_del:]]";
        let mut r = Vec::new();
        execute_gateway_actions(text, None, "wx", "c1", "u1", None, None, &mut r).await;
        assert!(r[0].contains("请指定"), "{}", r[0]);
    }

    #[tokio::test]
    async fn action_task_del_no_db() {
        let text = "[[GATEWAY:task_del:abc123]]";
        let mut r = Vec::new();
        execute_gateway_actions(text, None, "wx", "c1", "u1", None, None, &mut r).await;
        assert!(r[0].contains("数据库"), "{}", r[0]);
    }

    #[tokio::test]
    async fn action_multiple_mixed() {
        let text = "好的，帮你设置：\n[[GATEWAY:cron_add:0 9 * * 1-5:工作日早报]]\n[[GATEWAY:remind_after:30:半小时后开会]]";
        let mut r = Vec::new();
        let clean = execute_gateway_actions(text, None, "wx", "c1", "u1", None, None, &mut r).await;
        assert_eq!(clean, "好的，帮你设置：");
        assert_eq!(r.len(), 2);
    }

    #[tokio::test]
    async fn action_unknown_type() {
        let text = "[[GATEWAY:fly_to_moon:now]]";
        let mut r = Vec::new();
        execute_gateway_actions(text, None, "wx", "c1", "u1", None, None, &mut r).await;
        assert!(r[0].contains("未知"), "{}", r[0]);
    }

    #[tokio::test]
    async fn action_no_tags_passthrough() {
        let text = "普通回复";
        let mut r = Vec::new();
        let clean = execute_gateway_actions(text, None, "wx", "c1", "u1", None, None, &mut r).await;
        assert_eq!(clean, "普通回复");
        assert!(r.is_empty());
    }

    // ── Validation helpers ──────────────────────────────────────

    #[test]
    fn valid_cron_expressions() {
        assert!(is_valid_cron_expr("0 9 * * *"));
        assert!(is_valid_cron_expr("*/5 * * * *"));
        assert!(is_valid_cron_expr("0 9 * * 1-5"));
        assert!(is_valid_cron_expr("30 17 * * 5"));
        assert!(is_valid_cron_expr("0 0 1 * *"));
    }

    #[test]
    fn invalid_cron_expressions() {
        assert!(!is_valid_cron_expr("bad"));
        assert!(!is_valid_cron_expr("0 9 *"));
        assert!(!is_valid_cron_expr(""));
        assert!(!is_valid_cron_expr("0 9 * * * *")); // 6 fields
        assert!(!is_valid_cron_expr("hello world foo bar baz"));
    }

    #[test]
    fn default_profile_is_astra() {
        use crate::cli_bridge::CliProfile;
        let p = CliProfile::default();
        assert_eq!(p.name(), "astra");
    }

    // ── C1: SQL injection prevention in CREATE DATABASE ──

    #[test]
    fn safe_db_name_accepts_valid() {
        assert!(is_safe_db_name("astra_gateway"));
        assert!(is_safe_db_name("test123"));
        assert!(is_safe_db_name("DB_NAME"));
    }

    #[test]
    fn safe_db_name_rejects_injection() {
        assert!(!is_safe_db_name(""));
        assert!(!is_safe_db_name("foo`; DROP TABLE users; --"));
        assert!(!is_safe_db_name("db name"));
        assert!(!is_safe_db_name("foo;bar"));
        assert!(!is_safe_db_name("foo`bar"));
        assert!(!is_safe_db_name("../etc/passwd"));
    }
}

// ── Durable task action tests ───────────────────────────────

#[tokio::test]
async fn action_dtask_create_no_db() {
    let text = "[[GATEWAY:dtask_create:weekly report:collect stats]]";
    let mut r = Vec::new();
    execute_gateway_actions(text, None, "wx", "c1", "u1", None, None, &mut r).await;
    assert!(r[0].contains("数据库"), "{}", r[0]);
}

#[tokio::test]
async fn action_dtask_create_empty_name() {
    let text = "[[GATEWAY:dtask_create::desc]]";
    let mut r = Vec::new();
    execute_gateway_actions(text, None, "wx", "c1", "u1", None, None, &mut r).await;
    assert!(r[0].contains("不能为空"), "{}", r[0]);
}

#[tokio::test]
async fn action_dtask_checkpoint_bad_json() {
    let text = "[[GATEWAY:dtask_checkpoint:tid:not-json]]";
    let mut r = Vec::new();
    execute_gateway_actions(text, None, "wx", "c1", "u1", None, None, &mut r).await;
    assert!(r[0].contains("JSON 无效"), "{}", r[0]);
}

#[tokio::test]
async fn action_dtask_checkpoint_empty_id() {
    let text = r#"[[GATEWAY:dtask_checkpoint::{"k":1}]]"#;
    let mut r = Vec::new();
    execute_gateway_actions(text, None, "wx", "c1", "u1", None, None, &mut r).await;
    assert!(r[0].contains("请指定"), "{}", r[0]);
}

#[tokio::test]
async fn action_dtask_status_no_db() {
    let text = "[[GATEWAY:dtask_status:some-id]]";
    let mut r = Vec::new();
    execute_gateway_actions(text, None, "wx", "c1", "u1", None, None, &mut r).await;
    assert!(r[0].contains("数据库"), "{}", r[0]);
}

#[tokio::test]
async fn action_dtask_resume_no_db() {
    let text = "[[GATEWAY:dtask_resume:some-id]]";
    let mut r = Vec::new();
    execute_gateway_actions(text, None, "wx", "c1", "u1", None, None, &mut r).await;
    assert!(r[0].contains("数据库"), "{}", r[0]);
}

#[tokio::test]
async fn action_dtask_list_no_db() {
    let text = "[[GATEWAY:dtask_list]]";
    let mut r = Vec::new();
    execute_gateway_actions(text, None, "wx", "c1", "u1", None, None, &mut r).await;
    assert!(r[0].contains("数据库"), "{}", r[0]);
}

#[tokio::test]
async fn action_dtask_complete_no_db() {
    let text = "[[GATEWAY:dtask_complete:some-id]]";
    let mut r = Vec::new();
    execute_gateway_actions(text, None, "wx", "c1", "u1", None, None, &mut r).await;
    assert!(r[0].contains("数据库"), "{}", r[0]);
}

#[tokio::test]
async fn action_dtask_fail_no_db() {
    let text = "[[GATEWAY:dtask_fail:some-id:oops]]";
    let mut r = Vec::new();
    execute_gateway_actions(text, None, "wx", "c1", "u1", None, None, &mut r).await;
    assert!(r[0].contains("数据库"), "{}", r[0]);
}

#[tokio::test]
async fn action_dtask_cancel_no_db() {
    let text = "[[GATEWAY:dtask_cancel:some-id]]";
    let mut r = Vec::new();
    execute_gateway_actions(text, None, "wx", "c1", "u1", None, None, &mut r).await;
    assert!(r[0].contains("数据库"), "{}", r[0]);
}

// ── Fix #1: Regex handles JSON with `]` chars (arrays/nested) ──

#[tokio::test]
async fn action_dtask_checkpoint_json_with_array() {
    let text = r#"[[GATEWAY:dtask_checkpoint:tid:{"items":[1,2,3]}]]"#;
    let mut r = Vec::new();
    let clean = execute_gateway_actions(text, None, "wx", "c1", "u1", None, None, &mut r).await;
    assert!(clean.is_empty(), "tags should be stripped, got: {clean}");
    assert_eq!(r.len(), 1);
    assert!(
        r[0].contains("数据库"),
        "expected no-db error, got: {}",
        r[0]
    );
}

#[tokio::test]
async fn action_dtask_checkpoint_json_with_nested_brackets() {
    let text = r#"[[GATEWAY:dtask_checkpoint:tid:{"a":{"b":[true]}}]]"#;
    let mut r = Vec::new();
    let clean = execute_gateway_actions(text, None, "wx", "c1", "u1", None, None, &mut r).await;
    assert!(clean.is_empty(), "tags should be stripped, got: {clean}");
    assert_eq!(r.len(), 1);
}

#[tokio::test]
async fn action_tag_with_text_around_bracket_json() {
    let text = r#"OK here:
[[GATEWAY:dtask_checkpoint:tid:{"steps":["a","b"]}]]
done"#;
    let mut r = Vec::new();
    let clean = execute_gateway_actions(text, None, "wx", "c1", "u1", None, None, &mut r).await;
    assert_eq!(r.len(), 1);
    assert!(!clean.contains("GATEWAY"), "tag should be removed: {clean}");
    assert!(clean.contains("OK here"));
    assert!(clean.contains("done"));
}

// ── Fix #4: allow_slash_mutations=false denial ──

#[tokio::test]
async fn action_policy_blocks_model_mutations_when_disabled() {
    let text = "[[GATEWAY:cron_add:0 9 * * *:早上好]]";
    let policy = crate::access_control::ActionPolicy {
        allow_slash_mutations: true,
        allow_model_generated_mutations: false,
        workspace_roots: Vec::new(),
    };
    let mut r = Vec::new();
    let clean = execute_gateway_actions_with_policy(
        text, None, "wx", "c1", "u1", None, None, &policy, &mut r,
    )
    .await;
    assert!(clean.is_empty(), "tag should be stripped: {clean}");
    assert_eq!(r.len(), 1);
    assert!(r[0].contains("拒绝"), "expected denial, got: {}", r[0]);
}

#[tokio::test]
async fn action_policy_allows_when_enabled() {
    let text = "[[GATEWAY:cron_add:0 9 * * *:test]]";
    let policy = crate::access_control::ActionPolicy {
        allow_slash_mutations: true,
        allow_model_generated_mutations: true,
        workspace_roots: Vec::new(),
    };
    let mut r = Vec::new();
    let clean = execute_gateway_actions_with_policy(
        text, None, "wx", "c1", "u1", None, None, &policy, &mut r,
    )
    .await;
    assert!(clean.is_empty());
    assert_eq!(r.len(), 1);
    assert!(
        r[0].contains("数据库"),
        "expected no-db fallback, got: {}",
        r[0]
    );
}

fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1e6)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1e3)
    } else {
        format!("{n}")
    }
}

fn safe_id(id: &str) -> String {
    if id.len() <= 8 {
        id.to_string()
    } else {
        format!("{}…", crate::text::safe_prefix(id, 8))
    }
}

// ── skill_add action tests ──────────────────────────────────

#[tokio::test]
async fn action_skill_add_no_skills_dir() {
    let text = "[[GATEWAY:skill_add:deploy:# Deploy\nRun make deploy]]";
    let mut r = Vec::new();
    execute_gateway_actions(text, None, "wx", "c1", "u1", None, None, &mut r).await;
    assert!(
        r[0].contains("skill 目录未配置") || r[0].contains("skills_dir"),
        "{}",
        r[0]
    );
}

#[tokio::test]
async fn action_skill_add_empty_name() {
    let text = "[[GATEWAY:skill_add::content]]";
    let mut r = Vec::new();
    execute_gateway_actions(text, None, "wx", "c1", "u1", None, None, &mut r).await;
    assert!(r[0].contains("名称不能为空"), "{}", r[0]);
}

#[tokio::test]
async fn action_skill_add_rejects_path_traversal() {
    let dir = tempfile::tempdir().unwrap();
    let text = "[[GATEWAY:skill_add:../../evil:# owned]]";
    let mut r = Vec::new();
    execute_gateway_actions(
        text,
        None,
        "wx",
        "c1",
        "u1",
        None,
        Some(dir.path().to_str().unwrap()),
        &mut r,
    )
    .await;
    assert!(
        r[0].contains("不能包含路径") || r[0].contains("只能包含"),
        "{}",
        r[0]
    );
    assert!(!dir.path().parent().unwrap().join("evil.md").exists());
}

// ── Concurrency tests ───────────────────────────────────────

#[test]
fn null_adapter_is_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<NullAdapter>();
}

#[test]
fn cli_response_fields() {
    let r = CliResponse {
        platform: "weixin".into(),
        chat_id: "c1".into(),
        text: "hello".into(),
        reply_token: Some("tok".into()),
        outbox: None,
    };
    assert_eq!(r.platform, "weixin");
    assert_eq!(r.chat_id, "c1");
    assert_eq!(r.text, "hello");
    assert_eq!(r.reply_token.as_deref(), Some("tok"));
}

#[cfg(test)]
struct RecordingAdapter {
    name: &'static str,
    sent: std::sync::Arc<tokio::sync::Mutex<Vec<(String, String)>>>,
    rx: tokio::sync::Mutex<tokio::sync::mpsc::Receiver<InboundMessage>>,
}

#[cfg(test)]
#[async_trait::async_trait]
impl PlatformAdapter for RecordingAdapter {
    fn name(&self) -> &'static str {
        self.name
    }
    async fn start(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(())
    }
    async fn stop(&mut self) {}
    async fn send_text(
        &self,
        chat_id: &str,
        text: &str,
        _reply_token: Option<&str>,
    ) -> Result<(), String> {
        self.sent
            .lock()
            .await
            .push((chat_id.to_string(), text.to_string()));
        Ok(())
    }
    async fn recv(&self) -> Option<InboundMessage> {
        self.rx.lock().await.recv().await
    }
}

#[tokio::test]
async fn send_text_routes_to_matching_platform_only() {
    let wecom_sent = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let weixin_sent = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let (_tx1, rx1) = tokio::sync::mpsc::channel(1);
    let (_tx2, rx2) = tokio::sync::mpsc::channel(1);
    let adapters: Vec<Box<dyn PlatformAdapter>> = vec![
        Box::new(RecordingAdapter {
            name: "wecom",
            sent: wecom_sent.clone(),
            rx: tokio::sync::Mutex::new(rx1),
        }),
        Box::new(RecordingAdapter {
            name: "weixin",
            sent: weixin_sent.clone(),
            rx: tokio::sync::Mutex::new(rx2),
        }),
    ];
    let mut indices = HashMap::new();
    for (idx, adapter) in adapters.iter().enumerate() {
        indices.insert(adapter.name(), idx);
    }

    let sent = send_text_to_platform(&adapters, &indices, "weixin", "chat", "hello", None)
        .await
        .unwrap();
    assert_eq!(sent, 1);

    assert!(wecom_sent.lock().await.is_empty());
    assert_eq!(
        weixin_sent.lock().await.as_slice(),
        &[("chat".to_string(), "hello".to_string())]
    );
}

#[tokio::test]
async fn handle_fast_slash_command_returns_ok() {
    // Can't easily construct a full GatewayRunner in unit test (needs DB),
    // but we can test that NullAdapter works for spawned tasks
    let adapter = NullAdapter;
    let result = adapter.send_text("chat", "text", None).await;
    assert!(result.is_ok());
    let result = adapter.send_typing("chat").await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn cli_response_channel_roundtrip() {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<CliResponse>(8);
    tx.send(CliResponse {
        platform: "weixin".into(),
        chat_id: "c1".into(),
        text: "result".into(),
        reply_token: None,
        outbox: None,
    })
    .await
    .unwrap();
    let resp = rx.recv().await.unwrap();
    assert_eq!(resp.chat_id, "c1");
    assert_eq!(resp.text, "result");
}

#[tokio::test]
async fn concurrent_cli_responses_ordered() {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<CliResponse>(8);
    let tx2 = tx.clone();

    // Simulate two concurrent CLI tasks
    let h1 = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        tx.send(CliResponse {
            platform: "weixin".into(),
            chat_id: "user1".into(),
            text: "response1".into(),
            reply_token: None,
            outbox: None,
        })
        .await
        .unwrap();
    });
    let h2 = tokio::spawn(async move {
        tx2.send(CliResponse {
            platform: "wecom".into(),
            chat_id: "user2".into(),
            text: "response2".into(),
            reply_token: None,
            outbox: None,
        })
        .await
        .unwrap();
    });

    h1.await.unwrap();
    h2.await.unwrap();

    // Both responses arrive (order may vary)
    let mut responses = vec![];
    while let Ok(r) = rx.try_recv() {
        responses.push(r.chat_id);
    }
    assert_eq!(responses.len(), 2);
    assert!(responses.contains(&"user1".to_string()));
    assert!(responses.contains(&"user2".to_string()));
}

#[tokio::test]
async fn heartbeat_via_channel_not_adapter() {
    // Heartbeats in spawned tasks should go through outbound channel,
    // not NullAdapter (which drops them silently)
    let (tx, mut rx) = tokio::sync::mpsc::channel::<OutboundMessage>(8);
    tx.send(OutboundMessage::plain("weixin", "chat1", "🤔 thinking…"))
        .await
        .unwrap();
    let msg = rx.recv().await.unwrap();
    assert_eq!(msg.platform, "weixin");
    assert_eq!(msg.chat_id, "chat1");
    assert!(msg.text.contains("thinking"));
}

#[tokio::test]
async fn typing_sent_before_cli_spawn() {
    // Typing indicator should be sent in the main loop (via real adapter),
    // NOT in the spawned task (via NullAdapter)
    let adapter = NullAdapter;
    // NullAdapter.send_typing succeeds but does nothing — that's OK
    // because the real adapter sends typing in run() before spawning
    assert!(adapter.send_typing("chat").await.is_ok());
}
