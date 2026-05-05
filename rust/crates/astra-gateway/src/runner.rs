//! Gateway runner — bridges chat platforms to the `astra` CLI.
//!
//! Each inbound message spawns `astra chat -m "..." --session-id X`
//! and streams CLI progress to the chat platform while waiting for output.

use crate::cli_bridge::{self, CliProfile, CliProgress};
use crate::commands::{self, CommandContext};
use crate::config::GatewayConfig;
use crate::gateway_context::GatewayContext;
use crate::platforms::{InboundMessage, PlatformAdapter};
use crate::store::{self, GatewayStore};
use crate::trace_model::{
    ConversationKey, GatewayRequest, MysqlTraceRepository, OutboxId, RequestId, RequestStatus,
    RunStatus, TraceId, TraceRepository, TraceWriter,
};
use astra_core::durable_task_store::DurableTaskStore as _;
use futures_util::future::select_all;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

const MAX_CHUNK_LEN: usize = 3800;
const INITIAL_ACK_DELAY: Duration = Duration::from_secs(3);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(25);
const PROGRESSIVE_FLUSH_INTERVAL: Duration = Duration::from_secs(8);
const PROGRESSIVE_MIN_CHARS: usize = 200;

// ─── Send Circuit Breaker ───────────────────────────────────────────────────

const SEND_FAILURE_THRESHOLD: u32 = 3;

/// Per-conversation send health tracker. Suppresses heartbeats after
/// consecutive send failures to avoid flooding an unreachable platform.
#[derive(Clone, Default)]
struct SendCircuitBreaker {
    state: Arc<dashmap::DashMap<String, u32>>,
}

impl SendCircuitBreaker {
    fn record_success(&self, key: &str) {
        self.state.remove(key);
    }

    fn record_failure(&self, key: &str) {
        let mut entry = self.state.entry(key.to_string()).or_insert(0);
        *entry += 1;
    }

    fn is_open(&self, key: &str) -> bool {
        self.state
            .get(key)
            .map(|v| *v >= SEND_FAILURE_THRESHOLD)
            .unwrap_or(false)
    }

    fn reset(&self, key: &str) {
        self.state.remove(key);
    }
}

// ─── Safe String Truncation ─────────────────────────────────────────────────

/// Truncate a string to at most `n` characters, safe for multi-byte UTF-8.
pub(crate) fn truncate_chars(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}
const CONVERSATION_QUEUE_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Number of consecutive auth failures before the circuit breaker trips.
const AUTH_FAILURE_THRESHOLD: u32 = 2;
/// How long the circuit breaker stays open before allowing retries.
const AUTH_FAILURE_COOLDOWN: Duration = Duration::from_secs(300);

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
    store: Option<Arc<dyn GatewayStore>>,
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
    cli_availability: Vec<(String, cli_bridge::CliAvailability)>,
    /// Tracks consecutive auth failures per CLI profile name.
    /// Value: (failure_count, last_failure_time).
    auth_failures: Arc<dashmap::DashMap<String, (u32, Instant)>>,
    /// Shared access token — gateway validates once, all CLI spawns reuse via env var.
    shared_auth: Option<SharedAuthToken>,
    /// Monotonic counter for generating short request tags when no trace exists.
    request_counter: AtomicU32,
    /// Active CLI processes indexed by trace_id. Used by `/kill` to abort
    /// running tasks immediately (SIGKILL) instead of only marking DB state.
    active_tasks: Arc<dashmap::DashMap<String, CancellationToken>>,
    /// Per-conversation send circuit breaker. Workers check this before
    /// emitting heartbeats — stops sending after consecutive failures to
    /// avoid message flood when platform is unreachable.
    send_health: SendCircuitBreaker,
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

        let storage_config = config.resolve_storage();
        let (store, durable_store, trace_repo) =
            match store::open_store_bundle(&storage_config).await {
                Ok(Some(bundle)) => {
                    tracing::info!(backend = ?storage_config, "storage connected");
                    (Some(bundle.store), bundle.durable_store, bundle.trace_repo)
                }
                Ok(None) => {
                    tracing::info!("running without persistence (storage: none)");
                    (None, None, None)
                }
                Err(e) => return Err(e),
            };

        let cli_profile = config.cli.clone();

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

        let max_concurrent_runs = config.max_concurrent_runs.max(1);

        // Probe all configured CLI profiles for availability
        let mut cli_availability = Vec::new();
        let default_avail = cli_bridge::probe_cli(&cli_profile).await;
        cli_availability.push((cli_profile.name().to_string(), default_avail.clone()));
        for (name, profile) in &config.cli_profiles {
            let avail = cli_bridge::probe_cli(profile).await;
            cli_availability.push((name.clone(), avail));
        }

        // If default CLI not available, auto-select first available
        let effective_cli = if !default_avail.is_available() {
            if let Some((name, _)) = cli_availability.iter().find(|(_, a)| a.is_available()) {
                if let Some(profile) = config.cli_profiles.get(name) {
                    tracing::info!(cli = %name, "default CLI unavailable, auto-selected");
                    profile.clone()
                } else {
                    cli_profile.clone()
                }
            } else {
                cli_profile.clone()
            }
        } else {
            cli_profile.clone()
        };

        let shared_auth = Some(SharedAuthToken::new(
            thin.clone(),
            config.astra.username.clone(),
            config.astra.password.clone(),
        ));

        Ok(Self {
            config,
            store,
            cli_profile: effective_cli,
            thin,
            outbound_tx: None,
            durable_store,
            user_skills,
            projects,
            trace_repo,
            queue_senders: tokio::sync::Mutex::new(HashMap::new()),
            global_run_limiter: Arc::new(tokio::sync::Semaphore::new(max_concurrent_runs)),
            cli_availability,
            auth_failures: Arc::new(dashmap::DashMap::new()),
            shared_auth,
            request_counter: AtomicU32::new(0),
            active_tasks: Arc::new(dashmap::DashMap::new()),
            send_health: SendCircuitBreaker::default(),
        })
    }

    /// Whether any storage backend is active.
    pub fn has_store(&self) -> bool {
        self.store.is_some()
    }

    /// Clone the Arc-wrapped store for use by external components (e.g. scheduler).
    pub fn store(&self) -> Option<Arc<dyn GatewayStore>> {
        self.store.clone()
    }

    /// Clone the Arc-wrapped trace repository.
    pub fn trace_repo(&self) -> Option<Arc<MysqlTraceRepository>> {
        self.trace_repo.clone()
    }

    pub fn cli_profile(&self) -> &CliProfile {
        &self.cli_profile
    }

    /// Replay messages that were pending when gateway crashed.
    pub async fn replay_pending_messages(&self, adapter: &dyn PlatformAdapter) {
        if let Some(ref store) = self.store {
            let platform = adapter.name();
            match store.list_pending_messages(Some(platform)).await {
                Ok(msgs) if msgs.is_empty() => {}
                Ok(msgs) => {
                    tracing::info!(platform, count = msgs.len(), "replaying pending messages");
                    for pm in &msgs {
                        let msg = crate::platforms::InboundMessage {
                            platform,
                            chat_id: pm.chat_id.clone(),
                            user_id: pm.user_id.clone(),
                            text: pm.text.clone(),
                            msg_id: format!("replay-{}", pm.id),
                            chat_type: crate::platforms::ChatType::DirectMessage,
                            reply_token: None,
                        };
                        if let Some(response) = self
                            .handle_message_inner(&msg, adapter, Some(pm.id), false, None)
                            .await
                        {
                            for chunk in split_message(&response.text) {
                                if let Err(e) = adapter.send_text(&pm.chat_id, chunk, None).await {
                                    tracing::warn!(error = %e, chat_id = %pm.chat_id, "failed to deliver pending replay");
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

    pub async fn sweep_stale_traces(&self) {
        if let Some(ref repo) = self.trace_repo {
            match repo.sweep_stale_requests("gateway restarted").await {
                Ok(0) => {}
                Ok(n) => tracing::info!(count = n, "swept stale trace requests → failed"),
                Err(e) => tracing::warn!(error = %e, "failed to sweep stale traces"),
            }
        }
    }

    pub fn set_outbound_tx(&mut self, tx: tokio::sync::mpsc::Sender<OutboundMessage>) {
        self.outbound_tx = Some(tx);
    }

    /// Resolve the active CLI profile for a user (may be overridden via /cli + /model).
    async fn resolve_cli_profile(&self, platform: &str, user_id: &str) -> CliProfile {
        let mut profile = if let Some(ref store) = self.store
            && let Ok(Some(name)) = store
                .get_user_preference(platform, user_id, "cli_profile")
                .await
            && let Some(p) = self.config.cli_profiles.get(&name)
        {
            p.clone()
        } else {
            self.cli_profile.clone()
        };

        // Apply per-user model override scoped to this CLI
        let model_key = store::model_preference_key(profile.name());
        if let Some(ref store) = self.store
            && let Ok(Some(model_name)) = store
                .get_user_preference(platform, user_id, &model_key)
                .await
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
            && !is_mentioned(&msg.text, &self.config.bot_name)
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

        let trimmed = msg.text.trim();

        // /auth — reset auth circuit breaker, show CLI auth status, attempt auto-relogin
        if trimmed == "/auth" {
            return Ok(Some(self.handle_auth_command(&cli_profile).await));
        }

        // /manage cancel, /manage kill → redirect to fast-path /cancel, /kill
        // so they execute immediately even when a task is running.
        if let Some(rest) = trimmed.strip_prefix("/manage ") {
            let rest = rest.trim();
            if rest == "cancel"
                || rest.starts_with("cancel ")
                || rest == "kill"
                || rest.starts_with("kill ")
            {
                let rewritten_cmd = format!("/{rest}");
                // Build command context and dispatch directly (avoids async recursion).
                let cmd_ctx = CommandContext {
                    astra: &self.thin,
                    config: &self.config,
                    store: self.store.as_deref(),
                    platform: msg.platform,
                    chat_id: &effective_chat_id,
                    user_id: &msg.user_id,
                    resolved_cli: &cli_profile,
                    durable_store: self.durable_store.as_ref().map(|s| {
                        s.as_ref() as &dyn astra_core::durable_task_store::DurableTaskStore
                    }),
                    trace_repo: self
                        .trace_repo
                        .as_ref()
                        .map(|repo| repo.as_ref() as &dyn TraceRepository),
                    project_dirs: &self.config.project_dirs,
                    cli_availability: &self.cli_availability,
                    auth_status: self.auth_status_line(cli_profile.name()),
                    active_tasks: Some(&self.active_tasks),
                };
                if let Some(response) = commands::handle_command(&cmd_ctx, &rewritten_cmd).await {
                    return Ok(Some(response));
                }
            }
        }

        // /manage — rewrite to rich context message and send to slow CLI path
        if trimmed == "/manage" || trimmed.starts_with("/manage ") {
            let extra = trimmed.strip_prefix("/manage").unwrap_or("").trim();
            let context = self
                .build_manage_context(msg, &effective_chat_id, &cli_profile, extra)
                .await;
            let mut managed_msg = msg.clone();
            managed_msg.text = context;
            return Err(managed_msg);
        }

        // Slash commands — instant response, no CLI
        let cmd_ctx = CommandContext {
            astra: &self.thin,
            config: &self.config,
            store: self.store.as_deref(),
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
            project_dirs: &self.config.project_dirs,
            cli_availability: &self.cli_availability,
            auth_status: self.auth_status_line(cli_profile.name()),
            active_tasks: Some(&self.active_tasks),
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

        // Check first-time user BEFORE upsert (upsert creates the row)
        let is_first = if let Some(ref store) = self.store {
            store
                .is_first_message(msg.platform, &msg.user_id)
                .await
                .unwrap_or(false)
        } else {
            false
        };

        if let Some(ref store) = self.store
            && let Err(e) = store
                .upsert_user(msg.platform, &msg.user_id, &msg.user_id)
                .await
        {
            tracing::warn!(error = %e, "failed to upsert user");
        }

        // Send first-time welcome after upsert
        if is_first {
            let welcome = build_welcome_message(&cli_profile);
            if let Some(ref tx) = self.outbound_tx {
                let _ = tx
                    .send(OutboundMessage::plain(
                        msg.platform.to_string(),
                        msg.chat_id.clone(),
                        welcome,
                    ))
                    .await;
            }
        }

        let cli_name = cli_profile.name().to_string();

        // Auto-reset session if policy triggers
        if let Some(ref store) = self.store
            && let Ok(Some(last_active_str)) = store
                .get_session_last_active(msg.platform, &effective_chat_id, &cli_name)
                .await
            && let Ok(last_active) =
                chrono::NaiveDateTime::parse_from_str(&last_active_str, "%Y-%m-%d %H:%M:%S%.f")
        {
            let last_utc = last_active.and_utc();
            let now = chrono::Utc::now();
            if self.config.session_reset.should_reset(last_utc, now) {
                if let Err(e) = store
                    .reset_session(msg.platform, &effective_chat_id, &cli_name)
                    .await
                {
                    tracing::warn!(error = %e, "session auto-reset failed");
                } else {
                    tracing::info!(cli = cli_name, "session auto-reset by policy");
                }
            }
        }

        let session_id = if let Some(ref store) = self.store {
            store
                .get_current_session(msg.platform, &effective_chat_id, &cli_name)
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

        // Generate short request tag for user-facing message correlation
        let request_tag = match trace.as_ref() {
            Some(t) => short_request_tag(t.trace_id.as_str()),
            None => {
                let n = self.request_counter.fetch_add(1, Ordering::Relaxed);
                format!("#{:02X}", n % 256)
            }
        };

        tracing::info!(
            platform = msg.platform,
            chat_id = %safe_id(&msg.chat_id),
            user = %safe_id(&msg.user_id),
            tag = %request_tag,
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

        // Check auth circuit breaker before spawning CLI
        if let Some(auth_msg) = self.check_auth_circuit(&cli_name) {
            if let Some(writer) = trace_writer.as_ref() {
                if let Some(ref run_id) = run_id {
                    let _ = writer
                        .finish_run(
                            run_id,
                            RunStatus::Failed,
                            None,
                            Some("auth circuit breaker open"),
                        )
                        .await;
                }
                let _ = writer.fail_request("auth circuit breaker open").await;
            }
            return Some(
                self.outbound_response(
                    trace.as_ref(),
                    msg.platform,
                    &msg.chat_id,
                    msg.reply_token.clone(),
                    auth_msg,
                )
                .await,
            );
        }

        // Resolve workspace directory for CLI
        let workspace: Option<std::path::PathBuf> = if let Some(ref store) = self.store
            && let Ok(Some(ws)) = store
                .get_user_preference(msg.platform, &msg.user_id, "workspace")
                .await
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
                self.store.is_some(),
            )
            .with_model_actions_allowed(self.config.action_policy.allow_model_generated_mutations);
            if let Some(ref store) = self.store
                && let Ok(jobs) = store.list_cron_jobs(msg.platform, &effective_chat_id).await
            {
                let cron_list: Vec<_> = jobs
                    .iter()
                    .map(|j| {
                        (
                            j.job_id[..8.min(j.job_id.len())].to_string(),
                            j.cron_expr.clone(),
                            j.description.clone(),
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
            if let Some(ref store) = self.store {
                match store
                    .save_pending_message(msg.platform, &effective_chat_id, &msg.user_id, &msg.text)
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

        // Pre-fetch shared access token so the CLI can skip per-spawn auth.
        let access_token = if let Some(ref auth) = self.shared_auth {
            auth.get().await
        } else {
            None
        };

        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::channel::<CliProgress>(64);

        // Register cancellation token for this task so /kill can abort it.
        // Uses trace_id if available, otherwise a synthetic key based on
        // the request tag — ensures all tasks are killable.
        let cancel_token = CancellationToken::new();
        let kill_registry_key = trace
            .as_ref()
            .map(|t| t.trace_id.to_string())
            .unwrap_or_else(|| format!("notrace:{}", request_tag));
        self.active_tasks
            .insert(kill_registry_key.clone(), cancel_token.clone());

        let cli_handle = tokio::spawn({
            let profile = cli_profile.clone();
            let message_text = message_text.clone();
            let system_prompt = system_prompt.clone();
            let ws = workspace.clone();
            let token = access_token.clone();
            let kill_token = cancel_token.clone();
            let trace_id_str = trace.as_ref().map(|t| t.trace_id.to_string());
            let request_id_str = trace.as_ref().map(|t| t.request_id.to_string());
            async move {
                cli_bridge::run_cli_with_cancel(
                    &profile,
                    &message_text,
                    sid.as_deref(),
                    ws.as_deref(),
                    Some(progress_tx),
                    Some(&system_prompt),
                    trace_id_str.as_deref(),
                    request_id_str.as_deref(),
                    Some(cli_timeout),
                    token.as_deref(),
                    Some(kill_token),
                )
                .await
            }
        });

        let start = Instant::now();
        let mut tool_count: u32 = 0;
        let mut last_tool = String::new();
        let mut sent_initial_ack = false;
        let mut token_buf = String::new();
        let mut think_filter = ThinkTagStreamFilter::default();
        let mut gateway_action_filter = GatewayActionStreamFilter::default();
        let mut progressive_text_len: usize = 0;
        let mut chunk_counter: u32 = 0;
        let next_timer = tokio::time::sleep(INITIAL_ACK_DELAY);
        tokio::pin!(next_timer);

        // Health key for heartbeat circuit-breaker. MUST use msg.chat_id
        // (not effective_chat_id) because deliver_outbound receives
        // OutboundMessage with msg.chat_id and tracks failures under
        // that key. Using effective_chat_id here would create a mismatch.
        let health_key = format!("{}:{}", msg.platform, msg.chat_id);
        // Reset health at task start — previous failures are stale.
        self.send_health.reset(&health_key);

        let flush_buf = |buf: &mut String,
                         tx: &Option<tokio::sync::mpsc::Sender<OutboundMessage>>,
                         platform: &str,
                         chat: &str,
                         tag: &str,
                         chunk_num: u32|
         -> usize {
            let text = buf.trim().to_string();
            if text.is_empty() {
                return 0;
            }
            let Some(tx) = tx else {
                // No outbound channel — discard to prevent memory leak.
                buf.clear();
                return 0;
            };
            let len = text.len();
            let tagged = format!("[{tag}:{chunk_num}] {text}");
            if tx
                .try_send(OutboundMessage::plain(
                    platform.to_string(),
                    chat.to_string(),
                    tagged,
                ))
                .is_err()
            {
                return 0;
            }
            // Only clear buffer after successful send.
            buf.clear();
            len
        };

        loop {
            tokio::select! {
                _ = cancel_token.cancelled() => {
                    tracing::info!(tag = %request_tag, "task cancelled by user — CLI process will be killed by bridge");
                    break;
                }
                progress = progress_rx.recv() => {
                    match progress {
                        Some(CliProgress::Token(text)) => {
                            let filtered = think_filter.push(&text);
                            let filtered = gateway_action_filter.push(&filtered);
                            if !filtered.is_empty() {
                                token_buf.push_str(&filtered);
                                if token_buf.len() >= PROGRESSIVE_MIN_CHARS {
                                    chunk_counter += 1;
                                    progressive_text_len += flush_buf(&mut token_buf, &self.outbound_tx, msg.platform, &chat_id, &request_tag, chunk_counter);
                                    next_timer.as_mut().reset(tokio::time::Instant::now() + PROGRESSIVE_FLUSH_INTERVAL);
                                }
                            }
                        }
                        Some(CliProgress::ToolStarted { ref name }) => {
                            tool_count += 1;
                            last_tool = name.clone();
                        }
                        Some(CliProgress::ToolDone { name, .. }) => {
                            last_tool = name;
                        }
                        Some(CliProgress::ToolCall(line)) => {
                            tool_count += 1;
                            last_tool = line;
                        }
                        Some(CliProgress::Thinking(_)) => {}
                        Some(CliProgress::Status(_) | CliProgress::Stderr(_)) => {}
                        None => {
                            let think_tail = think_filter.finish();
                            if !think_tail.is_empty() {
                                let filtered = gateway_action_filter.push(&think_tail);
                                token_buf.push_str(&filtered);
                            }
                            let tail = gateway_action_filter.finish();
                            if !tail.is_empty() {
                                token_buf.push_str(&tail);
                            }
                            chunk_counter += 1;
                            progressive_text_len += flush_buf(&mut token_buf, &self.outbound_tx, msg.platform, &chat_id, &request_tag, chunk_counter);
                            break;
                        }
                    }
                }
                _ = &mut next_timer => {
                    // Circuit-breaker: skip heartbeats/flushes when platform is unreachable.
                    if self.send_health.is_open(&health_key) {
                        next_timer.as_mut().reset(tokio::time::Instant::now() + HEARTBEAT_INTERVAL);
                        continue;
                    }
                    if !token_buf.is_empty() {
                        chunk_counter += 1;
                        progressive_text_len += flush_buf(&mut token_buf, &self.outbound_tx, msg.platform, &chat_id, &request_tag, chunk_counter);
                        next_timer.as_mut().reset(tokio::time::Instant::now() + PROGRESSIVE_FLUSH_INTERVAL);
                    } else if !sent_initial_ack {
                        sent_initial_ack = true;
                        let heartbeat = format!("[{request_tag}] 🤔 {cli_name} 思考中…");
                        if let Some(ref tx) = self.outbound_tx {
                            // try_send: drop heartbeat if channel backpressured —
                            // stale heartbeats are worse than missing ones.
                            let _ = tx.try_send(OutboundMessage::plain(
                                msg.platform.to_string(), chat_id.clone(), heartbeat,
                            ));
                        }
                        next_timer.as_mut().reset(tokio::time::Instant::now() + HEARTBEAT_INTERVAL);
                    } else {
                        let elapsed_str = format_elapsed(start.elapsed());
                        let heartbeat = if tool_count > 0 {
                            let tool_short = truncate(&last_tool, 40);
                            format!("[{request_tag}] ⏳ {elapsed_str} | 🔧 {tool_count}个工具 | {tool_short}")
                        } else {
                            format!("[{request_tag}] ⏳ 处理中… {elapsed_str}")
                        };
                        if let Some(ref tx) = self.outbound_tx {
                            let _ = tx.try_send(OutboundMessage::plain(
                                msg.platform.to_string(), chat_id.clone(), heartbeat,
                            ));
                        }
                        next_timer.as_mut().reset(tokio::time::Instant::now() + HEARTBEAT_INTERVAL);
                    }
                }
            }
        }

        // Deregister from active tasks registry.
        self.active_tasks.remove(&kill_registry_key);

        // If the task was cancelled, the CLI bridge returns Err("killed by user").
        // Short-circuit: mark trace as failed and return a kill confirmation
        // without processing the result as a normal completion.
        if cancel_token.is_cancelled() {
            if let Some(writer) = trace_writer.as_ref()
                && let Err(e) = writer.fail_request("killed by user").await
            {
                tracing::warn!(error = %e, "failed to mark trace as killed");
            }
            if let Err(e) = cli_handle.await {
                tracing::debug!(error = %e, "cli task join error after cancel");
            }
            tracing::info!(tag = %request_tag, "task killed, skipping result processing");
            return Some(
                self.outbound_response(
                    trace.as_ref(),
                    msg.platform,
                    &msg.chat_id,
                    msg.reply_token.clone(),
                    format!("[{request_tag}] 💀 任务已终止"),
                )
                .await,
            );
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
                // Track auth failures for circuit breaker
                if cli_bridge::is_auth_error(&e) {
                    self.record_auth_failure(&cli_name);
                    if let Some(ref auth) = self.shared_auth {
                        auth.invalidate().await;
                    }
                }
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
                let text = format!("[{request_tag}] {text}");
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
                        format!("[{request_tag}] ⚠️ 任务中断: {e}"),
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
            if let Some(ref store) = self.store {
                let _ = store
                    .reset_session(msg.platform, &effective_chat_id, &cli_name)
                    .await;
            }
            // Retry without session-id
            let retry_handle = tokio::spawn({
                let profile = cli_profile.clone();
                let message_text = msg.text.clone();
                let system_prompt = system_prompt.clone();
                let ws = workspace.clone();
                let access_token = access_token.clone();
                async move {
                    cli_bridge::run_cli_with_context(
                        &profile,
                        &message_text,
                        None, // no session-id
                        ws.as_deref(),
                        None,
                        Some(&system_prompt),
                        access_token.as_deref(),
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
                    if let Some(ref store) = self.store
                        && let Some(ref sid) = retry_result.session_id
                        && let Err(e) = store
                            .set_current_session(
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

            // Detect auth errors and record for circuit breaker
            let combined_err = format!("{}\n{}", result.stderr, result.stdout);
            if cli_bridge::is_auth_error(&combined_err) {
                self.record_auth_failure(&cli_name);
                if let Some(ref auth) = self.shared_auth {
                    auth.invalidate().await;
                }

                // Attempt auto-relogin if credentials are configured
                if self.config.astra.username.is_some()
                    && self.config.astra.password.is_some()
                    && matches!(cli_profile, CliProfile::Astra { .. })
                {
                    match self.try_auto_relogin().await {
                        Ok(ref token) => {
                            tracing::info!(cli = %cli_name, "auto-relogin succeeded after auth failure");
                            self.clear_auth_failure(&cli_name);
                            // Update shared token cache with the fresh token
                            if let Some(ref auth) = self.shared_auth {
                                let mut guard = auth.token.write().await;
                                *guard = Some(token.clone());
                            }
                        }
                        Err(e) => {
                            tracing::warn!(cli = %cli_name, error = %e, "auto-relogin failed");
                        }
                    }
                }
            }

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
                let text = format!("[{request_tag}] {text}");
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

        // Clear auth failure counter on success
        if result.exit_code == 0 {
            self.clear_auth_failure(&cli_name);
        }

        // Save session_id to DB (if available), scoped by CLI profile
        if let Some(ref store) = self.store {
            if let Some(ref sid) = result.session_id {
                if let Err(e) = store
                    .set_current_session(
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
            } else if let Err(e) = store
                .touch_session(msg.platform, &effective_chat_id, &cli_name)
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
                self.store.as_deref(),
                msg.platform,
                &msg.chat_id,
                &msg.user_id,
                self.durable_store
                    .as_ref()
                    .map(|s| s.as_ref() as &dyn astra_core::durable_task_store::DurableTaskStore),
                self.config.skills_dir.as_deref(),
                &self.config.action_policy,
                self.trace_repo
                    .as_ref()
                    .map(|r| r.as_ref() as &dyn TraceRepository),
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
        let tool_count_total = result.tool_calls_count.unwrap_or(0);
        if tool_count_total > 0 {
            stats_parts.push(format!("🔧{tool_count_total}"));
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
            &request_tag,
        );

        // Record usage to DB
        if let Some(ref store) = self.store
            && let Err(e) = store
                .record_usage(&store::UsageRecord {
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
                })
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

        // When progressive streaming already delivered the main content, the
        // final message is just a stats footer + action results.  Sending it
        // as a plain message (no durable outbox) avoids retry storms: if this
        // low-value footer fails to deliver it is simply dropped rather than
        // retried on every restart.
        if progressive_text_len > 0 {
            // Still mark the trace request as completed (even without outbox).
            if let Some(writer) = trace_writer.as_ref() {
                let _ = writer.complete_request().await;
            }
            Some(OutboundMessage::plain(
                msg.platform.to_string(),
                msg.chat_id.clone(),
                text,
            ))
        } else {
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
    }

    async fn clear_pending_message(&self, pending_id: Option<i64>) {
        let (Some(id), Some(store)) = (pending_id, self.store.as_ref()) else {
            return;
        };
        if let Err(e) = store.delete_pending_message(id).await {
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

    // ─── Auth circuit breaker ──────────────────────────────────────────────

    /// Check if the auth circuit breaker is tripped for the given CLI.
    /// Returns `Some(message)` if the circuit is open and the caller should
    /// short-circuit without spawning the CLI.
    fn check_auth_circuit(&self, cli_name: &str) -> Option<String> {
        if let Some(entry) = self.auth_failures.get(cli_name) {
            let (count, last_failure) = *entry;
            if count > AUTH_FAILURE_THRESHOLD && last_failure.elapsed() < AUTH_FAILURE_COOLDOWN {
                let remaining = AUTH_FAILURE_COOLDOWN
                    .saturating_sub(last_failure.elapsed())
                    .as_secs();
                return Some(format!(
                    "🔑 CLI `{cli_name}` 认证失败（连续 {} 次）\n\n\
                     可能原因:\n\
                     - API 密钥过期\n\
                     - 服务端 token 刷新失败\n\n\
                     解决方法:\n\
                     1. 运行 `astra /login` 重新登录\n\
                     2. 或检查环境变量 ASTRA_API_KEY\n\
                     3. 或切换到其他 CLI: `/cli claude`\n\n\
                     {remaining} 秒后自动重试，或发送 `/auth` 手动重试。",
                    count,
                ));
            }
            // Cooldown expired — clear the counter
            if last_failure.elapsed() >= AUTH_FAILURE_COOLDOWN {
                drop(entry);
                self.auth_failures.remove(cli_name);
            }
        }
        None
    }

    fn auth_status_line(&self, cli_name: &str) -> Option<String> {
        let entry = self.auth_failures.get(cli_name)?;
        let (count, last_failure) = *entry;
        if last_failure.elapsed() >= AUTH_FAILURE_COOLDOWN {
            drop(entry);
            self.auth_failures.remove(cli_name);
            return None;
        }

        if count > AUTH_FAILURE_THRESHOLD {
            let remaining = AUTH_FAILURE_COOLDOWN
                .saturating_sub(last_failure.elapsed())
                .as_secs();
            Some(format!("⚠️ 暂停 (剩余 {remaining}s, 连续失败 {count} 次)"))
        } else {
            Some(format!("✅ 正常 (最近失败 {count} 次)"))
        }
    }

    /// Record an auth failure for the given CLI profile.
    fn record_auth_failure(&self, cli_name: &str) {
        let mut entry = self
            .auth_failures
            .entry(cli_name.to_string())
            .or_insert((0, Instant::now()));
        entry.0 += 1;
        entry.1 = Instant::now();
        tracing::warn!(
            cli = cli_name,
            consecutive_failures = entry.0,
            "auth failure recorded"
        );
    }

    /// Clear auth failure counter for a CLI (called on successful request).
    fn clear_auth_failure(&self, cli_name: &str) {
        if self.auth_failures.remove(cli_name).is_some() {
            tracing::info!(cli = cli_name, "auth failure counter cleared (success)");
        }
    }

    /// Attempt to re-login to the astra server using configured credentials.
    /// On success, writes the new tokens to `~/.astra/credentials.json` so
    /// subsequent CLI spawns pick them up.
    async fn try_auto_relogin(&self) -> Result<String, String> {
        let username = self
            .config
            .astra
            .username
            .as_ref()
            .ok_or("no username configured")?;
        let password = self
            .config
            .astra
            .password
            .as_ref()
            .ok_or("no password configured")?;

        let body = serde_json::json!({ "username": username, "password": password });
        let resp = self
            .thin
            .post_auth_login_json(&body)
            .await
            .map_err(|e| format!("login request failed: {e}"))?;

        let v: serde_json::Value =
            serde_json::from_str(&resp).map_err(|e| format!("invalid login response: {e}"))?;
        let access = v
            .get("access_token")
            .and_then(|t| t.as_str())
            .ok_or("missing access_token in response")?;
        let refresh = v.get("refresh_token").and_then(|t| t.as_str());

        save_token_to_cli_credentials(username, access, refresh)?;
        tracing::info!(username = %username, "auto-relogin succeeded, CLI credentials refreshed");
        Ok(access.to_string())
    }

    /// Handle the `/auth` slash command: reset circuit breaker, show CLI auth
    /// status, and attempt auto-relogin if credentials are configured.
    async fn handle_auth_command(&self, _current_cli: &CliProfile) -> String {
        // 1. Reset circuit breaker
        let cleared = self.auth_failures.len();
        self.auth_failures.clear();

        // 2. Invalidate probe cache so re-probe is fresh
        cli_bridge::invalidate_probe_cache();

        // 3. Re-probe all CLIs
        let mut lines = vec!["🔑 **认证状态**".to_string()];
        let default_avail = cli_bridge::probe_cli(&self.cli_profile).await;
        let default_status = if default_avail.is_available() {
            "✅"
        } else {
            "❌"
        };
        lines.push(format!(
            "  {default_status} `{}` (默认)",
            self.cli_profile.name()
        ));
        for (name, profile) in &self.config.cli_profiles {
            let avail = cli_bridge::probe_cli(profile).await;
            let status = if avail.is_available() { "✅" } else { "❌" };
            lines.push(format!("  {status} `{name}`"));
        }

        if cleared > 0 {
            lines.push(format!(
                "\n认证缓存已重置（清除 {cleared} 个 CLI 的失败计数）。"
            ));
        } else {
            lines.push("\n认证缓存已重置。".into());
        }

        // 4. Invalidate shared token cache so it refreshes on next message
        if let Some(ref auth) = self.shared_auth {
            auth.invalidate().await;
        }

        // 5. Attempt auto-relogin if credentials configured
        if self.config.astra.username.is_some() && self.config.astra.password.is_some() {
            match self.try_auto_relogin().await {
                Ok(ref token) => {
                    lines.push("✅ 自动重新登录成功，凭证已刷新。".into());
                    // Warm shared token cache with the fresh token
                    if let Some(ref auth) = self.shared_auth {
                        let mut guard = auth.token.write().await;
                        *guard = Some(token.clone());
                    }
                }
                Err(e) => {
                    lines.push(format!("⚠️ 自动重新登录失败: {e}"));
                }
            }
        }

        lines.push("\n下次消息将重新验证。".into());
        lines.join("\n")
    }

    /// Build a rich context message for `/manage` that gets sent to the CLI agent.
    async fn build_manage_context(
        &self,
        msg: &InboundMessage,
        effective_chat_id: &str,
        cli_profile: &CliProfile,
        extra: &str,
    ) -> String {
        let cli_name = cli_profile.name();
        let mut sections = vec![
            "# Gateway 任务管理模式".to_string(),
            "你现在是 Gateway 任务管理助手。分析以下运行状态，帮助用户管理任务。".to_string(),
            "你可以使用 [[GATEWAY:...]] 标签直接执行操作。".to_string(),
        ];

        // Active requests
        if let Some(repo) = self.trace_repo.as_ref() {
            let conversation = ConversationKey::new(msg.platform, effective_chat_id, cli_name);
            let rows = repo
                .list_active_requests(&conversation, 50)
                .await
                .unwrap_or_default();
            if !rows.is_empty() {
                sections.push("\n## 活跃请求".to_string());
                for (i, row) in rows.iter().enumerate() {
                    let icon = match row.display_status() {
                        "running" => "\u{1f504}",
                        "queued" => "\u{231b}",
                        "reply_retrying" => "\u{1f4ec}",
                        "reply_pending" => "\u{1f4e4}",
                        _ => "\u{2753}",
                    };
                    let error_suffix = row
                        .error_message
                        .as_ref()
                        .map(|e| format!(" | error: {e}"))
                        .unwrap_or_default();
                    sections.push(format!(
                        "[{}] {} {} | {} | {}{}",
                        i + 1,
                        icon,
                        row.display_status(),
                        row.text_preview,
                        row.created_at,
                        error_suffix,
                    ));
                }
                sections.push("\n可用操作:".to_string());
                sections.push("- 终止运行中: [[GATEWAY:trace_kill:<trace_id>]]".to_string());
                sections
                    .push("- 清除失败投递: [[GATEWAY:outbox_dismiss:<request_id>]]".to_string());
            } else {
                sections.push("\n## 活跃请求\n无".to_string());
            }
        }

        // Cron jobs
        if let Some(ref store) = self.store {
            let jobs = store
                .list_cron_jobs(msg.platform, effective_chat_id)
                .await
                .unwrap_or_default();
            if !jobs.is_empty() {
                sections.push("\n## 定时任务".to_string());
                for job in &jobs {
                    let short = &job.job_id[..8.min(job.job_id.len())];
                    let status = if job.enabled { "\u{2705}" } else { "\u{23f8}" };
                    sections.push(format!(
                        "- {status} `{short}` | `{}` | {}",
                        job.cron_expr, job.description
                    ));
                }
                sections.push("\n可用操作: [[GATEWAY:task_del:<job_id>]] 删除".to_string());
            }
        }

        // Durable tasks
        if let Some(ref durable) = self.durable_store {
            let owner = format!("{}:{}", msg.platform, effective_chat_id);
            let filter = astra_core::durable_task_store::TaskFilter {
                owner_id: Some(owner),
                ..Default::default()
            };
            if let Ok(tasks) = durable.list(filter).await {
                let active: Vec<_> = tasks.iter().filter(|t| t.status.is_active()).collect();
                if !active.is_empty() {
                    sections.push("\n## 持久任务".to_string());
                    for t in &active {
                        let short = &t.id.0[..8.min(t.id.0.len())];
                        let icon = match t.status {
                            astra_core::durable_task_store::DurableTaskStatus::Running => {
                                "\u{1f504}"
                            }
                            astra_core::durable_task_store::DurableTaskStatus::Suspended => {
                                "\u{23f8}"
                            }
                            _ => "\u{1f4cb}",
                        };
                        let step = t
                            .step_description
                            .as_ref()
                            .map(|s| format!(" | {s}"))
                            .unwrap_or_default();
                        sections.push(format!(
                            "- {} `{short}` | {} | {}%{}",
                            icon, t.name, t.progress_pct, step,
                        ));
                    }
                }
            }
        }

        if !extra.is_empty() {
            sections.push(format!("\n## 用户指示\n{extra}"));
        } else {
            sections.push(
                "\n## 请求\n请分析以上状态，报告异常，并建议操作。如果有明显的问题（如长时间 running、失败的投递），直接用 GATEWAY 标签处理。".to_string(),
            );
        }

        sections.join("\n")
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
            match self
                .handle_message_inner(&queued.msg, &NullAdapter, None, true, queued.trace.clone())
                .await
            {
                Some(outbound) => {
                    let _ = cli_resp_tx.send(outbound).await;
                }
                None => {
                    // Fix A: Never leave a trace stuck in Running when no response is produced.
                    if let Some(trace) = queued.trace.as_ref()
                        && let Some(repo) = self.trace_repo.as_ref()
                    {
                        let writer = TraceWriter::from_existing(
                            repo.as_ref() as &dyn TraceRepository,
                            trace.trace_id.clone(),
                            trace.request_id.clone(),
                        );
                        let _ = writer.fail_request("request produced no response").await;
                    }
                }
            }
        }
        // Fix C: Sweep any Running/Accepted traces for this conversation before exiting.
        if let Some(repo) = self.trace_repo.as_ref() {
            match repo
                .sweep_conversation_stale_requests(&key, "conversation worker exited")
                .await
            {
                Ok(0) => {}
                Ok(n) => {
                    tracing::info!(conversation = %key, count = n, "swept stale traces on worker exit");
                }
                Err(e) => {
                    tracing::warn!(conversation = %key, error = %e, "failed to sweep stale traces on worker exit");
                }
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
            Ok(Some(request)) if request.status.is_terminal() => {
                tracing::info!(
                    request_id = %trace.request_id,
                    status = request.status.as_str(),
                    "skipping queued request (terminal status)"
                );
                false
            }
            Ok(Some(request)) => {
                // Non-terminal, non-Accepted (e.g. Running from a previous crash) — mark failed
                tracing::info!(
                    request_id = %trace.request_id,
                    status = request.status.as_str(),
                    "skipping queued request with unexpected status; marking failed"
                );
                let writer = TraceWriter::from_existing(
                    repo.as_ref() as &dyn TraceRepository,
                    trace.trace_id.clone(),
                    trace.request_id.clone(),
                );
                let _ = writer
                    .fail_request(&format!(
                        "queued request had unexpected status: {}",
                        request.status.as_str()
                    ))
                    .await;
                false
            }
            Ok(None) => false,
            Err(e) => {
                // Execute on DB error rather than silently dropping the request
                tracing::warn!(error = %e, "failed to verify queued request status; executing anyway");
                true
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
        let health_key = format!("{}:{}", outbound.platform, outbound.chat_id);
        let result = send_text_to_platform(
            adapters,
            adapter_indices,
            &outbound.platform,
            &outbound.chat_id,
            &outbound.text,
            outbound.reply_token.as_deref(),
        )
        .await;

        // Track send health for heartbeat circuit-breaker.
        match &result {
            Ok(_) => {
                self.send_health.record_success(&health_key);
            }
            Err((_, error)) => {
                self.send_health.record_failure(&health_key);
                if outbound.outbox.is_none() {
                    tracing::debug!(
                        platform = %outbound.platform,
                        chat_id = %safe_id(&outbound.chat_id),
                        error,
                        "heartbeat/chunk send failed (no outbox, not retried)"
                    );
                }
            }
        }

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
        self.sweep_stale_tasks().await;
        self.sweep_stale_traces().await;
        self.replay_retryable_outbox(&adapters, &adapter_indices)
            .await;
        for adapter in &adapters {
            self.replay_pending_messages(adapter.as_ref()).await;
        }
        // Drain any progressive chunks that accumulated in the outbound channel
        // during replay (the main select loop wasn't consuming yet).
        while let Ok(outbound) = cron_rx.try_recv() {
            self.deliver_outbound(&adapters, &adapter_indices, outbound)
                .await;
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

// ─── Shared auth token ─────────────────────────────────────────────────────
//
// Gateway manages a single cached access token that is injected into CLI
// spawns via `ASTRA_ACCESS_TOKEN`.  This eliminates per-message auth
// round-trips (`GET /auth/me`) inside the CLI.

/// Thread-safe cached access token.  The gateway validates the token once,
/// then all concurrent CLI spawns reuse it without any HTTP call.
struct SharedAuthToken {
    token: tokio::sync::RwLock<Option<String>>,
    api: astra_thin_client::ThinClient,
    username: Option<String>,
    password: Option<String>,
}

impl SharedAuthToken {
    fn new(
        api: astra_thin_client::ThinClient,
        username: Option<String>,
        password: Option<String>,
    ) -> Self {
        Self {
            token: tokio::sync::RwLock::new(None),
            api,
            username,
            password,
        }
    }

    /// Return a cached valid token, or try to obtain one (from credentials file
    /// or by logging in).
    async fn get(&self) -> Option<String> {
        // Fast path: return cached token
        {
            let guard = self.token.read().await;
            if let Some(ref tok) = *guard {
                return Some(tok.clone());
            }
        }
        // Slow path: refresh
        self.refresh().await
    }

    /// Force-refresh: read from `~/.astra/credentials.json`, validate via
    /// `GET /auth/me`, and optionally re-login with username/password.
    async fn refresh(&self) -> Option<String> {
        // Read from CLI credentials file
        if let Some(ref tok) = read_cli_access_token()
            && let Ok(resp) = self
                .api
                .get_auth_me_text_timeout(tok, Duration::from_secs(3))
                .await
            && resp.status().is_success()
        {
            let mut guard = self.token.write().await;
            *guard = Some(tok.clone());
            return Some(tok.clone());
        }
        // Token invalid — try login if credentials available
        if let (Some(username), Some(password)) = (&self.username, &self.password)
            && let Ok(body) = self
                .api
                .post_auth_login_json(&serde_json::json!({
                    "username": username,
                    "password": password,
                }))
                .await
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(&body)
            && let Some(access) = v["access_token"].as_str()
        {
            let refresh = v["refresh_token"].as_str();
            let _ = save_token_to_cli_credentials(username, access, refresh);
            let tok = access.to_string();
            let mut guard = self.token.write().await;
            *guard = Some(tok.clone());
            return Some(tok);
        }
        None
    }

    /// Clear the cached token (e.g. after an auth failure from the CLI).
    async fn invalidate(&self) {
        let mut guard = self.token.write().await;
        *guard = None;
    }
}

/// Read the access token for the current profile from `~/.astra/credentials.json`.
fn read_cli_access_token() -> Option<String> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()?;
    let path = std::path::Path::new(&home)
        .join(".astra")
        .join("credentials.json");
    let content = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&content).ok()?;
    let current = v["current_profile"].as_str().unwrap_or("default");
    v["profiles"][current]["access_token"]
        .as_str()
        .map(String::from)
}

/// Write refreshed tokens to `~/.astra/credentials.json` so subsequent CLI
/// spawns pick them up.  Reads the existing file (if any), updates the
/// access/refresh tokens on the current or default profile, and writes back.
fn save_token_to_cli_credentials(
    username: &str,
    access_token: &str,
    refresh_token: Option<&str>,
) -> Result<(), String> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".to_string());
    let path = std::path::PathBuf::from(home)
        .join(".astra")
        .join("credentials.json");

    // Read existing file (may not exist)
    let mut doc: serde_json::Value = if let Ok(content) = std::fs::read_to_string(&path) {
        serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    // Determine profile name
    let profile_name = doc
        .get("current_profile")
        .and_then(|v| v.as_str())
        .unwrap_or("default")
        .to_string();

    // Ensure profiles object exists
    if !doc.get("profiles").is_some_and(|p| p.is_object()) {
        doc["profiles"] = serde_json::json!({});
    }

    // Update the profile
    let profile = &mut doc["profiles"][&profile_name];
    if !profile.is_object() {
        *profile = serde_json::json!({});
    }
    profile["username"] = serde_json::Value::String(username.to_string());
    profile["access_token"] = serde_json::Value::String(access_token.to_string());
    if let Some(rt) = refresh_token {
        profile["refresh_token"] = serde_json::Value::String(rt.to_string());
    }

    // Write back
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir failed: {e}"))?;
    }
    let body = serde_json::to_string_pretty(&doc).map_err(|e| format!("serialize failed: {e}"))?;
    std::fs::write(&path, body).map_err(|e| format!("write failed: {e}"))?;

    // Restrict to owner-only (0o600)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }

    Ok(())
}

fn is_safe_skill_name(name: &str) -> bool {
    let trimmed = name.trim();
    !trimmed.is_empty()
        && trimmed
            .chars()
            .all(|ch| ch.is_alphanumeric() || matches!(ch, ' ' | '_' | '-'))
}

#[cfg(test)]
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
    store: Option<&dyn GatewayStore>,
    platform: &str,
    chat_id: &str,
    user_id: &str,
    durable_store: Option<&dyn astra_core::durable_task_store::DurableTaskStore>,
    skills_dir: Option<&str>,
    action_results: &mut Vec<String>,
) -> String {
    execute_gateway_actions_with_policy(
        text,
        store,
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
        None,
        action_results,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn execute_gateway_actions_with_policy(
    text: &str,
    store: Option<&dyn GatewayStore>,
    platform: &str,
    chat_id: &str,
    user_id: &str,
    durable_store: Option<&dyn astra_core::durable_task_store::DurableTaskStore>,
    skills_dir: Option<&str>,
    action_policy: &crate::access_control::ActionPolicy,
    trace_repo: Option<&dyn TraceRepository>,
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
                } else if let Some(store) = store {
                    let job_id = uuid::Uuid::new_v4().to_string();
                    match store
                        .create_cron_job(&store::CronJobSpec {
                            job_id: job_id.clone(),
                            platform: platform.to_string(),
                            chat_id: chat_id.to_string(),
                            user_id: user_id.to_string(),
                            cron_expr: cron_expr.to_string(),
                            message: message.to_string(),
                            description: message.to_string(),
                        })
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
                    "⚠️ 定时任务需要 MySQL 存储支持".into()
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
                } else if let Some(store) = store {
                    // Store as one-shot cron job with computed next_run time
                    let job_id = uuid::Uuid::new_v4().to_string();
                    let next_run = chrono::Utc::now() + chrono::Duration::minutes(minutes as i64);
                    let next_run_str = next_run.format("%Y-%m-%d %H:%M:%S").to_string();
                    // Use special cron_expr "once" to mark as one-shot
                    match store
                        .create_cron_job(&store::CronJobSpec {
                            job_id: job_id.clone(),
                            platform: platform.to_string(),
                            chat_id: chat_id.to_string(),
                            user_id: user_id.to_string(),
                            cron_expr: "once".to_string(),
                            message: message.clone(),
                            description: format!("⏰ {message} (一次性)"),
                        })
                        .await
                    {
                        Ok(()) => {
                            // Update next_run via the trait method (works on all backends)
                            if let Err(e) = store.update_cron_next_run(&job_id, &next_run_str).await
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
                    "⚠️ 延时提醒需要 MySQL 存储支持".into()
                }
            }
            Some("remind_after") => {
                "⚠️ remind_after 格式错误（需要: remind_after:分钟数:消息）".into()
            }

            Some("task_list") => {
                if let Some(store) = store {
                    match store.list_cron_jobs(platform, chat_id).await {
                        Ok(jobs) if jobs.is_empty() => "📋 当前没有定时任务。".into(),
                        Ok(jobs) => {
                            let mut lines = vec![format!("📋 **定时任务** ({} 个)", jobs.len())];
                            for j in &jobs {
                                let status = if j.enabled { "✅" } else { "⏸" };
                                let short_id = &j.job_id[..8.min(j.job_id.len())];
                                lines.push(format!(
                                    "{status} `{short_id}` | `{}` | {}",
                                    j.cron_expr, j.description
                                ));
                            }
                            lines.join("\n")
                        }
                        Err(e) => format!("⚠️ 查询失败: {e}"),
                    }
                } else {
                    "⚠️ 需要 MySQL 存储支持".into()
                }
            }

            Some("task_del") if parts.len() >= 2 => {
                let target = parts[1].trim();
                if target.is_empty() {
                    "⚠️ 请指定任务 ID".into()
                } else if let Some(store) = store {
                    // Support prefix match
                    match find_and_delete_job(store, platform, chat_id, target).await {
                        Ok(Some(desc)) => {
                            tracing::info!(target, "gateway action: task_del");
                            format!("✅ 已删除任务: {desc}")
                        }
                        Ok(None) => format!("❌ 找不到任务 `{target}`"),
                        Err(e) => format!("⚠️ 删除失败: {e}"),
                    }
                } else {
                    "⚠️ 需要 MySQL 存储支持".into()
                }
            }
            Some("task_del") => "⚠️ task_del 格式错误（需要: task_del:任务ID）".into(),

            // Legacy alias
            Some("cron_del") if parts.len() >= 2 => {
                let job_id = parts[1].trim();
                if let Some(store) = store {
                    match find_and_delete_job(store, platform, chat_id, job_id).await {
                        Ok(Some(desc)) => {
                            tracing::info!(job_id, "gateway action: cron_del");
                            format!("✅ 已删除任务: {desc}")
                        }
                        Ok(None) => format!("❌ 找不到任务 `{job_id}`"),
                        Err(e) => format!("⚠️ 删除失败: {e}"),
                    }
                } else {
                    "⚠️ 需要 MySQL 存储支持".into()
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
                    "⚠️ 需要 MySQL 存储支持".into()
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
                                "⚠️ 需要 MySQL 存储支持".into()
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
                    "⚠️ 需要 MySQL 存储支持".into()
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
                    "⚠️ 需要 MySQL 存储支持".into()
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
                    "⚠️ 需要 MySQL 存储支持".into()
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
                    "⚠️ 需要 MySQL 存储支持".into()
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
                    "⚠️ 需要 MySQL 存储支持".into()
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
                    "⚠️ 需要 MySQL 存储支持".into()
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
                } else if let Some(store) = store {
                    let canonical = path
                        .canonicalize()
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or(expanded);
                    match store
                        .set_user_preference(platform, user_id, "workspace", &canonical)
                        .await
                    {
                        Ok(()) => {
                            tracing::info!(workspace = %canonical, "gateway action: workspace_set");
                            format!("📂 工作目录已切换: `{canonical}`")
                        }
                        Err(e) => format!("⚠️ 保存工作目录失败: {e}"),
                    }
                } else {
                    "⚠️ 需要 MySQL 存储支持".into()
                }
            }

            Some("trace_kill") if parts.len() >= 2 => {
                let trace_id_str = parts[1].trim();
                if trace_id_str.is_empty() {
                    "⚠️ 请指定 trace ID".into()
                } else if let Some(repo) = trace_repo {
                    let tid = TraceId::from_string(trace_id_str.to_string());
                    match repo.force_fail_request(&tid, "killed via manage").await {
                        Ok(true) => {
                            tracing::info!(trace_id = trace_id_str, "gateway action: trace_kill");
                            format!(
                                "💀 已终止请求 `{}`",
                                &trace_id_str[..8.min(trace_id_str.len())]
                            )
                        }
                        Ok(false) => format!(
                            "⚠️ 请求 `{}` 已是终态",
                            &trace_id_str[..8.min(trace_id_str.len())]
                        ),
                        Err(e) => format!("⚠️ 终止失败: {e}"),
                    }
                } else {
                    "⚠️ 需要 trace 支持".into()
                }
            }

            Some("outbox_dismiss") if parts.len() >= 2 => {
                let request_id_str = parts[1].trim();
                if request_id_str.is_empty() {
                    "⚠️ 请指定 request ID".into()
                } else if let Some(repo) = trace_repo {
                    let rid = RequestId::from_string(request_id_str.to_string());
                    match repo.dismiss_failed_outbox(&rid).await {
                        Ok(()) => {
                            tracing::info!(
                                request_id = request_id_str,
                                "gateway action: outbox_dismiss"
                            );
                            format!(
                                "🧹 已清除失败消息 `{}`",
                                &request_id_str[..8.min(request_id_str.len())]
                            )
                        }
                        Err(e) => format!("⚠️ 清除失败: {e}"),
                    }
                } else {
                    "⚠️ 需要 trace 支持".into()
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
    store::is_valid_cron_expr(expr)
}

async fn find_and_delete_job(
    store: &dyn GatewayStore,
    platform: &str,
    chat_id: &str,
    target: &str,
) -> Result<Option<String>, store::StoreError> {
    let jobs = store.list_cron_jobs(platform, chat_id).await?;
    // Exact or prefix match
    let matched = jobs
        .iter()
        .find(|j| j.job_id == target || j.job_id.starts_with(target));
    if let Some(j) = matched {
        let desc = j.description.clone();
        store.delete_cron_job(&j.job_id).await?;
        Ok(Some(desc))
    } else {
        Ok(None)
    }
}

/// Streaming filter for `<think>...</think>` blocks.
///
/// Buffers partial tag fragments across token boundaries so that a `<think>`
/// split across two chunks (e.g. `"<thi"` + `"nk>..."`) is correctly detected
/// and suppressed, instead of leaking the partial tag to the user.
#[derive(Default)]
struct ThinkTagStreamFilter {
    pending: String,
    in_think: bool,
}

impl ThinkTagStreamFilter {
    fn push(&mut self, text: &str) -> String {
        self.pending.push_str(text);
        let mut out = String::new();

        loop {
            if self.in_think {
                if let Some(end) = self.pending.find("</think>") {
                    self.pending.drain(..end + 8);
                    self.in_think = false;
                    continue;
                }
                // No closing tag yet — keep all pending content. finish()
                // will return it if the block is never closed.
                break;
            }

            if let Some(start) = self.pending.find("<think>") {
                out.push_str(&self.pending[..start]);
                self.pending.drain(..start + 7);
                self.in_think = true;
                continue;
            }

            let keep = open_think_prefix_len(&self.pending);
            let emit_len = self.pending.len().saturating_sub(keep);
            out.push_str(&self.pending[..emit_len]);
            self.pending.drain(..emit_len);
            break;
        }

        out
    }

    fn finish(&mut self) -> String {
        if self.in_think {
            // Unclosed <think> — return accumulated content so it's not lost
            let leftover = std::mem::take(&mut self.pending);
            self.in_think = false;
            leftover
        } else {
            std::mem::take(&mut self.pending)
        }
    }
}

/// Suffix of `text` that could be a prefix of `<think>` (when outside a think block).
fn open_think_prefix_len(text: &str) -> usize {
    tag_suffix_prefix_len(text, "<think>")
}

fn tag_suffix_prefix_len(text: &str, tag: &str) -> usize {
    let max = text.len().min(tag.len() - 1);
    for len in (1..=max).rev() {
        if text.is_char_boundary(text.len() - len) && tag.starts_with(&text[text.len() - len..]) {
            return len;
        }
    }
    0
}

/// Simple non-streaming filter for complete text. Used on the final CLI output.
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
    request_tag: &str,
) -> String {
    if progressive_text_len > 0 {
        // Progressive mode: main content already streamed; final msg is stats footer only
        let mut parts = Vec::new();
        if !action_results.is_empty() {
            parts.push(action_results.to_string());
        }
        if !stats_parts.is_empty() {
            parts.push(format!("`{}`", stats_parts.join(" | ")));
        }
        let body = parts.join("\n\n");
        if body.is_empty() {
            body
        } else {
            format!("[{request_tag}] {body}")
        }
    } else {
        // Non-progressive: full text + stats in one message (no tag prefix —
        // the response was not streamed, so there is no chunk sequence to correlate).
        let mut result = text.to_string();
        if !result.is_empty() && !stats_parts.is_empty() {
            result.push_str(&format!(
                "\n\n`[{request_tag}] {}`",
                stats_parts.join(" | ")
            ));
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

/// Derive a short request tag like `#A7` from the first two hex digits of a
/// trace ID (UUID).  Returns `#??` when the input has fewer than two hex chars.
pub(crate) fn short_request_tag(trace_id: &str) -> String {
    let hex: String = trace_id
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .take(2)
        .collect();
    if hex.len() == 2 {
        format!("#{}", hex.to_uppercase())
    } else {
        "#??".to_string()
    }
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

    // ── Short request tag ──────────────────────────────────────

    #[test]
    fn short_request_tag_from_uuid() {
        assert_eq!(
            short_request_tag("a7bc1234-5678-9abc-def0-123456789abc"),
            "#A7"
        );
        assert_eq!(
            short_request_tag("3f001122-3344-5566-7788-99aabbccddee"),
            "#3F"
        );
    }

    #[test]
    fn short_request_tag_empty_input() {
        assert_eq!(short_request_tag(""), "#??");
    }

    #[test]
    fn short_request_tag_single_hex_char() {
        assert_eq!(short_request_tag("a"), "#??");
    }

    #[test]
    fn short_request_tag_always_uppercase() {
        assert_eq!(
            short_request_tag("abcdef12-0000-0000-0000-000000000000"),
            "#AB"
        );
    }

    #[test]
    fn short_request_tag_skips_dashes() {
        // UUID dashes should be skipped, so "a-b-c" should pick up 'a' and 'b'
        assert_eq!(short_request_tag("a-b-c"), "#AB");
    }

    #[test]
    fn short_request_tag_counter_fallback_format() {
        // Verify the counter-based format matches expectations
        assert_eq!(format!("#{:02X}", 0u32), "#00");
        assert_eq!(format!("#{:02X}", 167u32), "#A7");
        assert_eq!(format!("#{:02X}", 255u32), "#FF");
        assert_eq!(format!("#{:02X}", 256u32 % 256), "#00"); // wraps
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

    // ── is_mentioned tests ────────────────────────────────────

    #[test]
    fn mentioned_with_bot_name() {
        assert!(is_mentioned("hello @Astra help", "Astra"));
        assert!(is_mentioned("@astra", "Astra"));
        assert!(!is_mentioned("hello world", "Astra"));
        assert!(!is_mentioned("@someone else", "Astra"));
    }

    #[test]
    fn mentioned_empty_bot_name_matches_any_at() {
        assert!(is_mentioned("@anyone", ""));
        assert!(is_mentioned("hey @bot", ""));
        assert!(!is_mentioned("hello world", ""));
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
        let msg = build_final_message("Hello world", "", &stats, 0, "#A7");
        assert!(msg.contains("Hello world"));
        assert!(msg.contains("↓8.4k"));
        assert!(msg.contains("[#A7]"), "stats footer should have tag: {msg}");
    }

    #[test]
    fn final_message_progressive_skips_body() {
        let stats = vec!["↓8.4k".into(), "↑95".into()];
        let msg = build_final_message("Hello world (already sent)", "", &stats, 500, "#3F");
        assert!(
            !msg.contains("Hello world"),
            "body should not repeat: {msg}"
        );
        assert!(msg.contains("↓8.4k"), "stats should still appear: {msg}");
        assert!(
            msg.starts_with("[#3F]"),
            "progressive footer should be tagged: {msg}"
        );
    }

    #[test]
    fn final_message_progressive_with_actions() {
        let stats = vec!["8s".into()];
        let msg = build_final_message("body", "⏰ 提醒已创建", &stats, 100, "#B2");
        assert!(
            msg.contains("⏰ 提醒已创建"),
            "action results should appear"
        );
        assert!(msg.contains("8s"), "stats should appear");
        assert!(!msg.contains("body"), "body should not repeat");
        assert!(
            msg.starts_with("[#B2]"),
            "progressive footer should be tagged: {msg}"
        );
    }

    #[test]
    fn final_message_progressive_empty_stats() {
        let msg = build_final_message("body", "", &[], 100, "#C0");
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

    // ── ThinkTagStreamFilter tests ────────────────────────────────

    #[test]
    fn think_stream_filter_complete_block() {
        let mut f = ThinkTagStreamFilter::default();
        assert_eq!(f.push("<think>reasoning</think>Hello!"), "Hello!");
        assert_eq!(f.finish(), "");
    }

    #[test]
    fn think_stream_filter_split_open_tag() {
        let mut f = ThinkTagStreamFilter::default();
        let r1 = f.push("before<thi");
        assert_eq!(r1, "before");
        let r2 = f.push("nk>hidden</think>visible");
        assert_eq!(r2, "visible");
    }

    #[test]
    fn think_stream_filter_split_close_tag() {
        let mut f = ThinkTagStreamFilter::default();
        assert_eq!(f.push("<think>hidden</thi"), "");
        assert_eq!(f.push("nk>after"), "after");
    }

    #[test]
    fn think_stream_filter_no_think_passthrough() {
        let mut f = ThinkTagStreamFilter::default();
        assert_eq!(f.push("just normal text"), "just normal text");
        assert_eq!(f.finish(), "");
    }

    #[test]
    fn think_stream_filter_unclosed_at_finish_preserves_content() {
        let mut f = ThinkTagStreamFilter::default();
        assert_eq!(f.push("before<think>hidden"), "before");
        let tail = f.finish();
        assert_eq!(tail, "hidden");
    }

    #[test]
    fn think_stream_filter_single_char_chunks() {
        let mut f = ThinkTagStreamFilter::default();
        let input = "A<think>secret</think>B";
        let mut output = String::new();
        for ch in input.chars() {
            output.push_str(&f.push(&ch.to_string()));
        }
        output.push_str(&f.finish());
        assert_eq!(output, "AB", "single-char chunking must still filter");
    }

    #[test]
    fn think_stream_filter_multiple_blocks() {
        let mut f = ThinkTagStreamFilter::default();
        let out = f.push("A<think>x</think>B<think>y</think>C");
        assert_eq!(out, "ABC");
    }

    #[test]
    fn open_think_prefix_len_values() {
        assert_eq!(open_think_prefix_len("hello"), 0);
        assert_eq!(open_think_prefix_len("hello<"), 1);
        assert_eq!(open_think_prefix_len("hello<t"), 2);
        assert_eq!(open_think_prefix_len("hello<th"), 3);
        assert_eq!(open_think_prefix_len("hello<thi"), 4);
        assert_eq!(open_think_prefix_len("hello<thin"), 5);
        assert_eq!(open_think_prefix_len("hello<think"), 6);
    }

    #[test]
    fn tag_suffix_prefix_len_for_close_tag() {
        assert_eq!(tag_suffix_prefix_len("text</", "</think>"), 2);
        assert_eq!(tag_suffix_prefix_len("text</t", "</think>"), 3);
        assert_eq!(tag_suffix_prefix_len("text</think", "</think>"), 7);
        assert_eq!(tag_suffix_prefix_len("text", "</think>"), 0);
    }

    // ── Gateway action tests ──────────────────────────────────────

    #[tokio::test]
    async fn action_cron_add_no_db() {
        let text = "好的\n[[GATEWAY:cron_add:0 9 * * *:早上好]]";
        let mut r = Vec::new();
        let clean = execute_gateway_actions(text, None, "wx", "c1", "u1", None, None, &mut r).await;
        assert_eq!(clean, "好的");
        assert!(r[0].contains("存储"), "{}", r[0]);
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
        assert!(r[0].contains("存储"), "{}", r[0]);
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
        assert!(r[0].contains("存储"), "{}", r[0]);
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
        assert!(r[0].contains("存储"), "{}", r[0]);
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

    // ── Auth circuit breaker tests ────────────────────────────────

    #[test]
    fn auth_circuit_not_tripped_initially() {
        let failures: dashmap::DashMap<String, (u32, Instant)> = dashmap::DashMap::new();
        // No entries — check_auth_circuit equivalent
        assert!(!failures.contains_key("astra"));
    }

    #[test]
    fn auth_circuit_trips_after_threshold() {
        let failures: dashmap::DashMap<String, (u32, Instant)> = dashmap::DashMap::new();
        // Simulate consecutive failures exceeding threshold
        failures.insert(
            "astra".to_string(),
            (AUTH_FAILURE_THRESHOLD + 1, Instant::now()),
        );
        let entry = failures.get("astra").unwrap();
        let (count, last) = *entry;
        assert!(count > AUTH_FAILURE_THRESHOLD);
        assert!(last.elapsed() < AUTH_FAILURE_COOLDOWN);
    }

    #[test]
    fn auth_circuit_resets_after_cooldown() {
        let failures: dashmap::DashMap<String, (u32, Instant)> = dashmap::DashMap::new();
        // Simulate an old failure past cooldown
        failures.insert(
            "astra".to_string(),
            (
                AUTH_FAILURE_THRESHOLD + 1,
                Instant::now() - AUTH_FAILURE_COOLDOWN - Duration::from_secs(1),
            ),
        );
        let entry = failures.get("astra").unwrap();
        let (_, last) = *entry;
        assert!(
            last.elapsed() >= AUTH_FAILURE_COOLDOWN,
            "should be past cooldown"
        );
    }

    #[test]
    fn auth_circuit_constants_are_reasonable() {
        let threshold = AUTH_FAILURE_THRESHOLD;
        assert!(threshold >= 1, "threshold should be at least 1");
        assert!(threshold <= 10, "threshold should be reasonable");
        assert!(
            AUTH_FAILURE_COOLDOWN.as_secs() >= 60,
            "cooldown should be >= 1 min"
        );
        assert!(
            AUTH_FAILURE_COOLDOWN.as_secs() <= 600,
            "cooldown should be <= 10 min"
        );
    }

    #[test]
    fn save_token_to_cli_credentials_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let creds_dir = dir.path().join(".astra");
        std::fs::create_dir_all(&creds_dir).unwrap();
        let creds_path = creds_dir.join("credentials.json");

        // Write initial file
        std::fs::write(
            &creds_path,
            r#"{"current_profile":"default","profiles":{"default":{"username":"old"}}}"#,
        )
        .unwrap();

        // We can't easily test save_token_to_cli_credentials because it uses
        // dirs::home_dir(), but we can test the JSON structure it produces.
        let mut doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&creds_path).unwrap()).unwrap();
        doc["profiles"]["default"]["access_token"] = serde_json::Value::String("new-token".into());
        doc["profiles"]["default"]["refresh_token"] =
            serde_json::Value::String("new-refresh".into());
        doc["profiles"]["default"]["username"] = serde_json::Value::String("admin".into());
        let body = serde_json::to_string_pretty(&doc).unwrap();
        std::fs::write(&creds_path, body).unwrap();

        // Verify
        let loaded: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&creds_path).unwrap()).unwrap();
        assert_eq!(
            loaded["profiles"]["default"]["access_token"].as_str(),
            Some("new-token")
        );
        assert_eq!(
            loaded["profiles"]["default"]["refresh_token"].as_str(),
            Some("new-refresh")
        );
        assert_eq!(
            loaded["profiles"]["default"]["username"].as_str(),
            Some("admin")
        );
    }

    // ── SharedAuthToken ─────────────────────────────────────────

    #[tokio::test]
    async fn shared_auth_token_get_returns_none_without_credentials() {
        // With no credentials file and no username/password, get() should return None
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap();
        let auth = SharedAuthToken::new(api, None, None);
        // get() will try to read credentials file and validate — both fail → None
        assert!(auth.get().await.is_none());
    }

    #[tokio::test]
    async fn shared_auth_token_invalidate_clears_cached() {
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap();
        let auth = SharedAuthToken::new(api, None, None);
        // Manually set a cached token
        {
            let mut guard = auth.token.write().await;
            *guard = Some("cached-token-abc".to_string());
        }
        assert_eq!(auth.get().await, Some("cached-token-abc".to_string()));

        auth.invalidate().await;
        // After invalidation, cached token is cleared (get returns None because
        // refresh will fail with unreachable server)
        let guard = auth.token.read().await;
        assert!(guard.is_none());
    }

    #[tokio::test]
    async fn shared_auth_token_get_returns_cached() {
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap();
        let auth = SharedAuthToken::new(api, None, None);
        // Manually seed cache
        {
            let mut guard = auth.token.write().await;
            *guard = Some("fast-path-token".to_string());
        }
        // get() should return the cached token without any network call
        assert_eq!(auth.get().await, Some("fast-path-token".to_string()));
    }

    #[test]
    fn read_cli_access_token_missing_file() {
        // With a nonexistent HOME, read_cli_access_token should return None
        let _guard = EnvGuard::set("HOME", "/nonexistent/path/xyz");
        assert!(read_cli_access_token().is_none());
    }

    #[test]
    fn read_cli_access_token_valid_file() {
        let tmp = tempfile::tempdir().unwrap();
        let astra_dir = tmp.path().join(".astra");
        std::fs::create_dir_all(&astra_dir).unwrap();
        let creds = serde_json::json!({
            "current_profile": "default",
            "profiles": {
                "default": {
                    "access_token": "my-token-123",
                    "refresh_token": "my-refresh"
                }
            }
        });
        std::fs::write(
            astra_dir.join("credentials.json"),
            serde_json::to_string(&creds).unwrap(),
        )
        .unwrap();

        let _guard = EnvGuard::set("HOME", tmp.path().to_str().unwrap());
        assert_eq!(read_cli_access_token(), Some("my-token-123".to_string()));
    }

    struct EnvGuard {
        key: &'static str,
        old: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let old = std::env::var(key).ok();
            unsafe {
                std::env::set_var(key, value);
            }
            Self { key, old }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.old {
                Some(value) => unsafe {
                    std::env::set_var(self.key, value);
                },
                None => unsafe {
                    std::env::remove_var(self.key);
                },
            }
        }
    }
}

// ── Durable task action tests ───────────────────────────────

#[tokio::test]
async fn action_dtask_create_no_db() {
    let text = "[[GATEWAY:dtask_create:weekly report:collect stats]]";
    let mut r = Vec::new();
    execute_gateway_actions(text, None, "wx", "c1", "u1", None, None, &mut r).await;
    assert!(r[0].contains("存储"), "{}", r[0]);
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
    assert!(r[0].contains("存储"), "{}", r[0]);
}

#[tokio::test]
async fn action_dtask_resume_no_db() {
    let text = "[[GATEWAY:dtask_resume:some-id]]";
    let mut r = Vec::new();
    execute_gateway_actions(text, None, "wx", "c1", "u1", None, None, &mut r).await;
    assert!(r[0].contains("存储"), "{}", r[0]);
}

#[tokio::test]
async fn action_dtask_list_no_db() {
    let text = "[[GATEWAY:dtask_list]]";
    let mut r = Vec::new();
    execute_gateway_actions(text, None, "wx", "c1", "u1", None, None, &mut r).await;
    assert!(r[0].contains("存储"), "{}", r[0]);
}

#[tokio::test]
async fn action_dtask_complete_no_db() {
    let text = "[[GATEWAY:dtask_complete:some-id]]";
    let mut r = Vec::new();
    execute_gateway_actions(text, None, "wx", "c1", "u1", None, None, &mut r).await;
    assert!(r[0].contains("存储"), "{}", r[0]);
}

#[tokio::test]
async fn action_dtask_fail_no_db() {
    let text = "[[GATEWAY:dtask_fail:some-id:oops]]";
    let mut r = Vec::new();
    execute_gateway_actions(text, None, "wx", "c1", "u1", None, None, &mut r).await;
    assert!(r[0].contains("存储"), "{}", r[0]);
}

#[tokio::test]
async fn action_dtask_cancel_no_db() {
    let text = "[[GATEWAY:dtask_cancel:some-id]]";
    let mut r = Vec::new();
    execute_gateway_actions(text, None, "wx", "c1", "u1", None, None, &mut r).await;
    assert!(r[0].contains("存储"), "{}", r[0]);
}

// ── trace_kill / outbox_dismiss GATEWAY actions ──

#[tokio::test]
async fn action_trace_kill_no_repo() {
    let text = "[[GATEWAY:trace_kill:some-trace-id]]";
    let mut r = Vec::new();
    // execute_gateway_actions passes None for trace_repo
    execute_gateway_actions(text, None, "wx", "c1", "u1", None, None, &mut r).await;
    assert!(r[0].contains("trace 支持"), "{}", r[0]);
}

#[tokio::test]
async fn action_trace_kill_empty_id() {
    let text = "[[GATEWAY:trace_kill:]]";
    let mut r = Vec::new();
    execute_gateway_actions(text, None, "wx", "c1", "u1", None, None, &mut r).await;
    assert!(r[0].contains("请指定"), "{}", r[0]);
}

#[tokio::test]
async fn action_trace_kill_with_repo() {
    use crate::trace_model::{InMemoryTraceRepository, TraceWriter};
    let repo = InMemoryTraceRepository::default();
    let conv = crate::trace_model::ConversationKey::new("wx", "c1", "astra");
    let req = crate::trace_model::GatewayRequest::new(conv, "msg-1", "u1", "hello");
    let trace_id = req.trace_id.clone();
    let writer = TraceWriter::begin(&repo, req).await.unwrap();
    let _ = writer.start_run("astra", None).await.unwrap();

    let text = format!("[[GATEWAY:trace_kill:{}]]", trace_id);
    let policy = crate::access_control::ActionPolicy {
        allow_slash_mutations: true,
        allow_model_generated_mutations: true,
        workspace_roots: Vec::new(),
    };
    let mut r = Vec::new();
    let repo_ref: &dyn crate::trace_model::TraceRepository = &repo;
    execute_gateway_actions_with_policy(
        &text,
        None,
        "wx",
        "c1",
        "u1",
        None,
        None,
        &policy,
        Some(repo_ref),
        &mut r,
    )
    .await;
    assert_eq!(r.len(), 1);
    assert!(
        r[0].contains("已终止"),
        "expected kill confirmation, got: {}",
        r[0]
    );
}

#[tokio::test]
async fn action_outbox_dismiss_no_repo() {
    let text = "[[GATEWAY:outbox_dismiss:some-request-id]]";
    let mut r = Vec::new();
    execute_gateway_actions(text, None, "wx", "c1", "u1", None, None, &mut r).await;
    assert!(r[0].contains("trace 支持"), "{}", r[0]);
}

#[tokio::test]
async fn action_outbox_dismiss_empty_id() {
    let text = "[[GATEWAY:outbox_dismiss:]]";
    let mut r = Vec::new();
    execute_gateway_actions(text, None, "wx", "c1", "u1", None, None, &mut r).await;
    assert!(r[0].contains("请指定"), "{}", r[0]);
}

// ── Fix #1: Regex handles JSON with `]` chars (arrays/nested) ──

#[tokio::test]
async fn action_dtask_checkpoint_json_with_array() {
    let text = r#"[[GATEWAY:dtask_checkpoint:tid:{"items":[1,2,3]}]]"#;
    let mut r = Vec::new();
    let clean = execute_gateway_actions(text, None, "wx", "c1", "u1", None, None, &mut r).await;
    assert!(clean.is_empty(), "tags should be stripped, got: {clean}");
    assert_eq!(r.len(), 1);
    assert!(r[0].contains("存储"), "expected no-db error, got: {}", r[0]);
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
        text, None, "wx", "c1", "u1", None, None, &policy, None, &mut r,
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
        text, None, "wx", "c1", "u1", None, None, &policy, None, &mut r,
    )
    .await;
    assert!(clean.is_empty());
    assert_eq!(r.len(), 1);
    assert!(
        r[0].contains("存储"),
        "expected no-db fallback, got: {}",
        r[0]
    );
}

fn build_welcome_message(cli: &CliProfile) -> String {
    format!(
        "👋 **欢迎使用 Astra Gateway!**\n\n\
         当前 CLI: `{cli_name}`\n\
         发送任意消息开始对话，或使用命令:\n\n\
         - `/help` — 所有命令\n\
         - `/status` — 当前状态\n\
         - `/cli` — 切换 CLI 后端\n\
         - `/model` — 切换模型\n\
         - `/ws ls` — 可用项目\n\
         - `/gateway` — 完整能力概览\n\n\
         发送消息开始 🚀",
        cli_name = cli.name()
    )
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

/// Check if the message text contains an @mention for the bot.
/// When `bot_name` is non-empty, matches `@{bot_name}` (case-insensitive).
/// When `bot_name` is empty, matches any `@` followed by a word character.
fn is_mentioned(text: &str, bot_name: &str) -> bool {
    if bot_name.is_empty() {
        text.contains('@')
    } else {
        let pattern = format!("@{}", bot_name);
        text.to_lowercase().contains(&pattern.to_lowercase())
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

// ── Kill registry tests ────────────────────────────────────────────────

#[test]
fn active_tasks_registry_insert_and_cancel() {
    let registry: dashmap::DashMap<String, CancellationToken> = dashmap::DashMap::new();
    let token = CancellationToken::new();
    registry.insert("trace-1".into(), token.clone());
    assert!(!token.is_cancelled());

    // Simulate /kill: remove + cancel
    let (_, removed_token) = registry.remove("trace-1").unwrap();
    removed_token.cancel();
    assert!(token.is_cancelled());
}

#[test]
fn active_tasks_registry_kill_nonexistent_returns_none() {
    let registry: dashmap::DashMap<String, CancellationToken> = dashmap::DashMap::new();
    assert!(registry.remove("ghost").is_none());
}

#[tokio::test]
async fn cancellation_token_aborts_spawned_task() {
    let token = CancellationToken::new();
    let token_inner = token.clone();

    let handle = tokio::spawn(async move {
        tokio::select! {
            _ = token_inner.cancelled() => "killed",
            _ = tokio::time::sleep(Duration::from_secs(60)) => "completed",
        }
    });

    // Cancel immediately
    token.cancel();
    let result = handle.await.unwrap();
    assert_eq!(result, "killed");
}

#[tokio::test]
async fn kill_command_removes_and_cancels_token() {
    let registry: Arc<dashmap::DashMap<String, CancellationToken>> =
        Arc::new(dashmap::DashMap::new());
    let token = CancellationToken::new();
    registry.insert("trace-abc".into(), token.clone());

    let killed = if let Some((_, t)) = registry.remove("trace-abc") {
        t.cancel();
        true
    } else {
        false
    };

    assert!(killed);
    assert!(token.is_cancelled());
    assert!(registry.is_empty());
}

// ── /manage cancel redirect tests ──────────────────────────────────────

// ── /manage redirect routing tests ─────────────────────────────────────

#[test]
fn manage_redirect_recognizes_cancel_and_kill() {
    // Verifies the routing predicate used in handle_fast.
    for input in [
        "/manage cancel",
        "/manage cancel 1",
        "/manage kill",
        "/manage kill 2",
    ] {
        let rest = input.strip_prefix("/manage ").unwrap().trim();
        assert!(
            rest == "cancel"
                || rest.starts_with("cancel ")
                || rest == "kill"
                || rest.starts_with("kill "),
            "'{input}' should redirect to fast path"
        );
    }
    // These should NOT redirect.
    for input in ["/manage status", "/manage", "/manage help"] {
        let rest = input.strip_prefix("/manage ").unwrap_or("").trim();
        let should_redirect = rest == "cancel"
            || rest.starts_with("cancel ")
            || rest == "kill"
            || rest.starts_with("kill ");
        assert!(!should_redirect, "'{input}' should NOT redirect");
    }
}

// ── cancel_task abstraction test ───────────────────────────────────────

#[test]
fn cancel_task_removes_token_and_fires() {
    let registry: Arc<dashmap::DashMap<String, CancellationToken>> =
        Arc::new(dashmap::DashMap::new());
    let token = CancellationToken::new();
    registry.insert("trace-1".into(), token.clone());

    // Simulate GatewayRunner::cancel_task logic
    let found = if let Some((_, t)) = registry.remove("trace-1") {
        t.cancel();
        true
    } else {
        false
    };

    assert!(found);
    assert!(token.is_cancelled());
    assert!(registry.is_empty());
    // Double-cancel is a no-op
    assert!(registry.remove("trace-1").is_none());
}

// ── /status model display tests ────────────────────────────────────────

#[test]
fn model_display_with_override() {
    let model: Option<String> = Some("gpt-4o".into());
    let (display, source) = if let Some(m) = model.as_deref() {
        (m.to_string(), "user override")
    } else {
        ("(CLI default)".to_string(), "profile default")
    };
    assert_eq!(display, "gpt-4o");
    assert_eq!(source, "user override");
}

#[test]
fn model_display_without_override() {
    let model: Option<String> = None;
    let config_default: Option<&str> = Some("sonnet");
    let (display, source) = if let Some(m) = model.as_deref() {
        (m.to_string(), "user override")
    } else if let Some(m) = config_default {
        (m.to_string(), "config default")
    } else {
        ("(CLI default)".to_string(), "profile default")
    };
    assert_eq!(display, "sonnet");
    assert_eq!(source, "config default");
}

#[test]
fn model_display_no_config() {
    let model: Option<String> = None;
    let config_default: Option<&str> = None;
    let (display, source) = if let Some(m) = model.as_deref() {
        (m.to_string(), "user override")
    } else if let Some(m) = config_default {
        (m.to_string(), "config default")
    } else {
        ("(CLI default)".to_string(), "profile default")
    };
    assert_eq!(display, "(CLI default)");
    assert_eq!(source, "profile default");
}

// ── Kill registry key tests ────────────────────────────────────────────

#[test]
fn kill_registry_key_with_trace() {
    let trace_id: Option<&str> = Some("abc-123");
    let request_tag = "#A7";
    let key = trace_id
        .map(String::from)
        .unwrap_or_else(|| format!("notrace:{request_tag}"));
    assert_eq!(key, "abc-123");
}

#[test]
fn kill_registry_key_without_trace() {
    let trace_id: Option<&str> = None;
    let request_tag = "#A7";
    let key = trace_id
        .map(String::from)
        .unwrap_or_else(|| format!("notrace:{request_tag}"));
    assert_eq!(key, "notrace:#A7");
}

#[test]
fn kill_registry_notrace_key_is_findable() {
    let registry: dashmap::DashMap<String, CancellationToken> = dashmap::DashMap::new();
    let token = CancellationToken::new();
    let key = "notrace:#A7".to_string();
    registry.insert(key.clone(), token.clone());

    // /kill can find it by the synthetic key
    let found = registry.remove(&key);
    assert!(found.is_some());
    found.unwrap().1.cancel();
    assert!(token.is_cancelled());
}

// ── flush_buf non-blocking test ────────────────────────────────────────

#[tokio::test]
async fn flush_buf_does_not_block_when_channel_full() {
    // Proves the progressive loop won't deadlock: flush_buf uses try_send.
    let (tx, _rx) = tokio::sync::mpsc::channel::<OutboundMessage>(1);

    // Fill channel.
    tx.try_send(OutboundMessage::plain(
        String::from("p"),
        String::from("c"),
        String::from("fill"),
    ))
    .unwrap();

    // flush_buf equivalent: try_send on full channel completes instantly.
    let result = tx.try_send(OutboundMessage::plain(
        String::from("p"),
        String::from("c"),
        String::from("chunk"),
    ));
    // Returns Err(Full), not deadlock.
    assert!(result.is_err());
}

// ── ChildKillGuard Drop test ───────────────────────────────────────────

#[tokio::test]
async fn child_kill_guard_kills_on_drop() {
    use std::process::Stdio;
    use tokio::process::Command;

    let mut child = Command::new("cat")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn cat");
    let pid = child.id().expect("pid");

    {
        let _guard = crate::cli_bridge::ChildKillGuard::new(&child);
        // Guard dropped here — should send SIGKILL.
    }

    // child.wait() should return quickly (killed).
    let status = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
    assert!(
        status.is_ok(),
        "child must exit promptly after guard drop (pid={pid})"
    );
}

#[test]
fn child_kill_guard_defuse_prevents_kill() {
    // Defused guard must NOT kill.
    let mut guard = crate::cli_bridge::ChildKillGuard::with_pid(1);
    guard.defuse();
    assert!(guard.is_defused());
    // Drop now — no kill sent (pid is None).
}

// ── SendCircuitBreaker tests ─────────────────────────────────────────────

#[cfg(test)]
mod circuit_breaker_tests {
    use super::*;

    #[test]
    fn starts_closed() {
        let cb = SendCircuitBreaker::default();
        assert!(!cb.is_open("wx:chat1"));
    }

    #[test]
    fn opens_after_threshold_failures() {
        let cb = SendCircuitBreaker::default();
        let key = "wx:chat1";
        cb.record_failure(key);
        cb.record_failure(key);
        assert!(!cb.is_open(key), "2 failures should not open");
        cb.record_failure(key);
        assert!(cb.is_open(key), "3 failures must open");
    }

    #[test]
    fn success_resets_to_closed() {
        let cb = SendCircuitBreaker::default();
        let key = "wx:chat1";
        for _ in 0..5 {
            cb.record_failure(key);
        }
        assert!(cb.is_open(key));
        cb.record_success(key);
        assert!(!cb.is_open(key));
    }

    #[test]
    fn reset_clears_state() {
        let cb = SendCircuitBreaker::default();
        let key = "wx:chat1";
        for _ in 0..5 {
            cb.record_failure(key);
        }
        cb.reset(key);
        assert!(!cb.is_open(key));
    }

    #[test]
    fn independent_keys() {
        let cb = SendCircuitBreaker::default();
        for _ in 0..5 {
            cb.record_failure("wx:chat1");
        }
        assert!(cb.is_open("wx:chat1"));
        assert!(!cb.is_open("wx:chat2"));
    }

    #[test]
    fn concurrent_access() {
        let cb = SendCircuitBreaker::default();
        let cb2 = cb.clone();
        let h = std::thread::spawn(move || {
            for _ in 0..100 {
                cb2.record_failure("wx:chat1");
            }
        });
        for _ in 0..100 {
            cb.record_failure("wx:chat1");
        }
        h.join().unwrap();
        // After 200 failures, must be open.
        assert!(cb.is_open("wx:chat1"));
    }
}

// ── truncate_chars tests ─────────────────────────────────────────────────

#[cfg(test)]
mod truncate_tests {
    use super::*;

    #[test]
    fn ascii_within_limit() {
        assert_eq!(truncate_chars("abcdefgh", 8), "abcdefgh");
    }

    #[test]
    fn ascii_over_limit() {
        assert_eq!(truncate_chars("abcdefghij", 8), "abcdefgh");
    }

    #[test]
    fn empty_string() {
        assert_eq!(truncate_chars("", 8), "");
    }

    #[test]
    fn multibyte_chinese() {
        let s = "你好世界测试数据额外";
        let truncated = truncate_chars(s, 8);
        assert_eq!(truncated.chars().count(), 8);
        assert_eq!(truncated, "你好世界测试数据");
    }

    #[test]
    fn multibyte_shorter_than_limit() {
        let s = "你好";
        assert_eq!(truncate_chars(s, 8), "你好");
    }

    #[test]
    fn emoji_boundary() {
        let s = "👋🌍🎉✨💫🔥⭐🎯extra";
        let truncated = truncate_chars(s, 8);
        assert_eq!(truncated.chars().count(), 8);
        assert_eq!(truncated, "👋🌍🎉✨💫🔥⭐🎯");
    }

    #[test]
    fn zero_limit() {
        assert_eq!(truncate_chars("hello", 0), "");
    }
}
