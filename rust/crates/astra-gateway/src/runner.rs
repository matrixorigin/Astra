//! Gateway runner — bridges chat platforms to the `astra` CLI.
//!
//! Each inbound message spawns `astra chat -m "..." --session-id X`
//! and streams CLI progress to the chat platform while waiting for output.

use astra_core::durable_task_store::DurableTaskStore as _;
use crate::cli_bridge::{self, CliProfile, CliProgress};
use crate::commands::{self, CommandContext};
use crate::config::GatewayConfig;
use crate::gateway_context::GatewayContext;
use crate::platforms::{InboundMessage, PlatformAdapter};
use crate::storage;
use sqlx::MySqlPool;
use std::time::{Duration, Instant};

const MAX_CHUNK_LEN: usize = 3800;
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(25);
/// Kill CLI only if no stderr activity for this long (process likely dead).
const CLI_STALL_TIMEOUT: Duration = Duration::from_secs(300);

/// Outbound message from cron scheduler or other background tasks.
pub type OutboundMessage = (String, String, String); // (platform, chat_id, text)

pub struct GatewayRunner {
    config: GatewayConfig,
    pool: Option<MySqlPool>,
    cli_profile: CliProfile,
    thin: astra_thin_client::ThinClient,
    outbound_tx: Option<tokio::sync::mpsc::Sender<OutboundMessage>>,
    durable_store: Option<std::sync::Arc<crate::durable_task_store::MysqlDurableTaskStore>>,
    user_skills: Vec<(String, String)>,
}

impl GatewayRunner {
    pub async fn new(config: GatewayConfig) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
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
            std::sync::Arc::new(crate::durable_task_store::MysqlDurableTaskStore::new(p.clone()))
        });

        let user_skills = config.skills_dir.as_deref()
            .map(crate::gateway_context::load_skills_from_dir)
            .unwrap_or_default();
        if !user_skills.is_empty() {
            tracing::info!(count = user_skills.len(), "loaded user skills from directory");
        }

        Ok(Self {
            config,
            pool,
            cli_profile,
            thin,
            outbound_tx: None,
            durable_store,
            user_skills,
        })
    }

    pub fn pool(&self) -> Option<&MySqlPool> {
        self.pool.as_ref()
    }

    pub fn cli_profile(&self) -> &CliProfile {
        &self.cli_profile
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
        let model_key = format!("model_override:{}", profile.name());
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


    /// Handle a single inbound message.
    pub async fn handle_message(
        &self,
        msg: &InboundMessage,
        adapter: &dyn PlatformAdapter,
    ) -> Option<String> {
        // Resolve active CLI profile (user may have switched via /cli)
        let cli_profile = self.resolve_cli_profile(msg.platform, &msg.user_id).await;

        // Slash commands
        let cmd_ctx = CommandContext {
            astra: &self.thin,
            config: &self.config,
            pool: self.pool.as_ref(),
            platform: msg.platform,
            chat_id: &msg.chat_id,
            user_id: &msg.user_id,
            resolved_cli: &cli_profile,
            durable_store: self.durable_store.as_ref().map(|s| s.as_ref() as &dyn astra_core::durable_task_store::DurableTaskStore),
        };
        if let Some(response) = commands::handle_command(&cmd_ctx, &msg.text).await {
            return Some(response);
        }

        // Ensure user exists (if DB available)
        if let Some(ref pool) = self.pool {
            let _ = storage::upsert_user(pool, msg.platform, &msg.user_id, &msg.user_id).await;
        }

        let cli_name = cli_profile.name().to_string();
        let session_id = if let Some(ref pool) = self.pool {
            storage::get_current_session_for_cli(pool, msg.platform, &msg.chat_id, &cli_name)
                .await
                .ok()
                .flatten()
        } else {
            None
        };

        tracing::info!(
            platform = msg.platform,
            chat_id = %msg.chat_id,
            user = %msg.user_id,
            "→ {}",
            truncate(&msg.text, 80),
        );

        // Send typing indicator immediately so user gets feedback
        let _ = adapter.send_typing(&msg.chat_id).await;

        // Check CLI is available before spawning
        let availability = cli_bridge::probe_cli(&cli_profile).await;
        if !availability.is_available() {
            return Some(cli_bridge::onboarding_message(&cli_profile, &availability));
        }

        // Build gateway context for CLI system prompt
        let gw_context = {
            let mut ctx = GatewayContext::new(
                &msg.user_id,
                &msg.user_id,
                msg.platform,
                &cli_profile,
                self.pool.is_some(),
            );
            if let Some(ref pool) = self.pool
                && let Ok(jobs) = storage::list_cron_jobs(pool, msg.platform, &msg.chat_id).await
            {
                let cron_list: Vec<_> = jobs.iter().map(|(id, expr, desc, _)| {
                    (id[..8.min(id.len())].to_string(), expr.clone(), desc.clone())
                }).collect();
                ctx = ctx.with_cron_jobs(cron_list);
            }
            if let Some(ref store) = self.durable_store {
                let owner = format!("{}:{}", msg.platform, msg.chat_id);
                let filter = astra_core::durable_task_store::TaskFilter {
                    owner_id: Some(owner),
                    ..Default::default()
                };
                if let Ok(tasks) = store.list(filter).await {
                    let task_list: Vec<_> = tasks.iter()
                        .filter(|t| t.status.is_active())
                        .map(|t| (
                            t.id.0[..8.min(t.id.0.len())].to_string(),
                            t.name.clone(),
                            t.status.as_str().to_string(),
                            t.progress_pct,
                        ))
                        .collect();
                    ctx = ctx.with_active_tasks(task_list);
                }
            }
            if !self.user_skills.is_empty() {
                ctx = ctx.with_extra_skills(self.user_skills.clone());
            }
            ctx
        };
        let system_prompt = gw_context.to_system_prompt();

        // Run CLI with rich progress heartbeats (no hard timeout — only stall detection).
        let message_text = msg.text.clone();
        let sid = session_id.clone();
        let chat_id = msg.chat_id.clone();
        let cli_name = cli_profile.name().to_string();

        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::channel::<CliProgress>(64);

        let cli_handle = tokio::spawn({
            let profile = cli_profile.clone();
            let message_text = message_text.clone();
            let system_prompt = system_prompt.clone();
            async move {
                cli_bridge::run_cli_with_context(
                    &profile,
                    &message_text,
                    sid.as_deref(),
                    None,
                    Some(progress_tx),
                    Some(&system_prompt),
                ).await
            }
        });

        let start = Instant::now();
        let mut last_activity = Instant::now();
        let mut tool_count: u32 = 0;
        let mut last_tool = String::new();
        let mut heartbeat_count: u32 = 0;
        let mut stalled = false;

        loop {
            tokio::select! {
                progress = progress_rx.recv() => {
                    match progress {
                        Some(CliProgress::ToolCall(line)) => {
                            tool_count += 1;
                            last_tool = line;
                            last_activity = Instant::now();
                        }
                        Some(CliProgress::Status(_) | CliProgress::Stderr(_)) => {
                            last_activity = Instant::now();
                        }
                        None => break, // CLI finished
                    }
                }
                _ = tokio::time::sleep(HEARTBEAT_INTERVAL) => {
                    let elapsed = start.elapsed();
                    let idle = last_activity.elapsed();
                    heartbeat_count += 1;

                    // Stall detection: no activity for CLI_STALL_TIMEOUT → kill
                    if idle > CLI_STALL_TIMEOUT {
                        tracing::error!(
                            idle_secs = idle.as_secs(),
                            "CLI stalled — no activity for {}s, aborting",
                            CLI_STALL_TIMEOUT.as_secs()
                        );
                        let _ = adapter.send_text(
                            &chat_id,
                            &format!("⚠️ `{cli_name}` 无响应 ({}s)，已终止", idle.as_secs()),
                            None,
                        ).await;
                        cli_handle.abort();
                        stalled = true;
                        break;
                    }

                    // Rich heartbeat
                    let elapsed_str = format_elapsed(elapsed);
                    let heartbeat = if tool_count > 0 {
                        let tool_short = truncate(&last_tool, 40);
                        format!("⏳ {elapsed_str} | 🔧 {tool_count}个工具 | {tool_short}")
                    } else if heartbeat_count == 1 {
                        format!("🤔 {cli_name} 思考中… ({elapsed_str})")
                    } else {
                        format!("⏳ 处理中… {elapsed_str}")
                    };
                    let _ = adapter.send_text(&chat_id, &heartbeat, None).await;
                }
            }
        }

        // Helper: suspend running durable tasks for this chat on failure
        let suspend_tasks = |store: &Option<std::sync::Arc<crate::durable_task_store::MysqlDurableTaskStore>>, reason: String| {
            let store = store.clone();
            let owner = format!("{platform}:{chat_id}", platform = msg.platform, chat_id = msg.chat_id);
            async move {
                if let Some(ref s) = store {
                    let n = s.suspend_running_tasks_for_owner(&owner, &reason).await.unwrap_or(0);
                    if n > 0 {
                        tracing::info!(count = n, %reason, "auto-suspended durable tasks on CLI failure");
                    }
                }
            }
        };

        if stalled {
            suspend_tasks(&self.durable_store, "CLI stalled — no activity".into()).await;
            return Some(format!("⚠️ `{cli_name}` 执行超时，请重试或 `/new` 新建会话"));
        }

        let result = match cli_handle.await {
            Ok(Ok(result)) => result,
            Ok(Err(e)) => {
                suspend_tasks(&self.durable_store, format!("CLI error: {e}")).await;
                return Some(cli_bridge::translate_cli_error(&cli_profile, -1, &e));
            }
            Err(e) => {
                suspend_tasks(&self.durable_store, format!("CLI interrupted: {e}")).await;
                return Some(format!("⚠️ 任务中断: {e}"));
            }
        };

        if result.exit_code != 0 {
            tracing::warn!(exit_code = result.exit_code, "CLI non-zero exit");
            suspend_tasks(&self.durable_store, format!("CLI exit code {}", result.exit_code)).await;
            if result.text.is_none() || result.text.as_deref() == Some("") {
                let error_text = if result.stderr.is_empty() {
                    &result.stdout
                } else {
                    &result.stderr
                };
                return Some(cli_bridge::translate_cli_error(
                    &cli_profile,
                    result.exit_code,
                    error_text.trim(),
                ));
            }
        }

        // Save session_id to DB (if available), scoped by CLI profile
        if let Some(ref pool) = self.pool {
            if let Some(ref sid) = result.session_id {
                let _ = storage::set_current_session_for_cli(
                    pool, msg.platform, &msg.chat_id, &msg.user_id, sid, &cli_name,
                ).await;
            } else {
                let _ = storage::touch_session_for_cli(pool, msg.platform, &msg.chat_id, &cli_name).await;
            }
        }

        // Use the parsed text field (from --json), fallback to raw stdout
        let mut text = result.text.as_deref().unwrap_or(result.stdout.trim()).to_string();

        // Execute gateway actions embedded in agent response
        if text.contains("[[GATEWAY:") {
            let mut action_results = Vec::new();
            text = execute_gateway_actions(
                &text,
                self.pool.as_ref(),
                msg.platform,
                &msg.chat_id,
                &msg.user_id,
                self.outbound_tx.as_ref(),
                self.durable_store.as_ref().map(|s| s.as_ref() as &dyn astra_core::durable_task_store::DurableTaskStore),
                &mut action_results,
            ).await;
            if !action_results.is_empty() {
                text.push_str("\n\n");
                text.push_str(&action_results.join("\n"));
            }
        }

        tracing::info!(
            platform = msg.platform,
            chat_id = %msg.chat_id,
            text_len = text.len(),
            tools = result.tool_calls_count.unwrap_or(0),
            exit = result.exit_code,
            "← done"
        );

        if text.is_empty() {
            Some("(无回复)".into())
        } else {
            Some(text)
        }
    }

    pub async fn run(
        &self,
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

        if let Some(adapter) = adapters.first_mut() {
            loop {
                tokio::select! {
                    msg = adapter.recv() => {
                        if let Some(msg) = msg {
                            let response = self.handle_message(&msg, adapter.as_ref()).await;
                            if let Some(text) = response {
                                for chunk in split_message(&text) {
                                    let _ = adapter.send_text(
                                        &msg.chat_id,
                                        chunk,
                                        msg.reply_token.as_deref(),
                                    ).await;
                                }
                            }
                        } else {
                            break;
                        }
                    }
                    outbound = cron_rx.recv() => {
                        if let Some((_platform, chat_id, text)) = outbound {
                            for chunk in split_message(&text) {
                                let _ = adapter.send_text(&chat_id, chunk, None).await;
                            }
                        }
                    }
                    _ = shutdown.recv() => break,
                }
            }
        }

        for adapter in &mut adapters {
            adapter.stop().await;
        }
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
        let window = &remaining[..MAX_CHUNK_LEN];
        // Priority 1: paragraph boundary (\n\n)
        let split_at = rfind_paragraph_break(window)
            // Priority 2: code fence boundary (``` on its own line)
            .or_else(|| rfind_fence_break(window))
            // Priority 3: any newline
            .or_else(|| window.rfind('\n'))
            // Priority 4: space
            .or_else(|| window.rfind(' '))
            // Fallback: hard cut
            .unwrap_or(MAX_CHUNK_LEN);

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
            if db_name.is_empty() {
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

/// Parse and execute `[[GATEWAY:action:args]]` tags in agent response text.
/// Returns the text with tags removed, and populates action_results with status messages.
#[allow(clippy::too_many_arguments)]
async fn execute_gateway_actions(
    text: &str,
    pool: Option<&sqlx::MySqlPool>,
    platform: &str,
    chat_id: &str,
    user_id: &str,
    outbound_tx: Option<&tokio::sync::mpsc::Sender<OutboundMessage>>,
    durable_store: Option<&dyn astra_core::durable_task_store::DurableTaskStore>,
    action_results: &mut Vec<String>,
) -> String {
    let re = regex::Regex::new(r"\[\[GATEWAY:([^\]]+)\]\]").unwrap();
    let mut clean = text.to_string();

    for cap in re.captures_iter(text) {
        let full_match = cap.get(0).unwrap().as_str();
        let inner = &cap[1];
        let parts: Vec<&str> = inner.splitn(3, ':').collect();

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
                        pool, &job_id, platform, chat_id, user_id,
                        cron_expr, message, message,
                    ).await {
                        Ok(()) => {
                            tracing::info!(id = &job_id[..8], expr = cron_expr, msg = message, "gateway action: cron_add");
                            format!("⏰ 定时任务已创建\n- ID: `{}`\n- 周期: `{cron_expr}`\n- 内容: {message}", &job_id[..8])
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
                } else if let Some(tx) = outbound_tx {
                    let tx = tx.clone();
                    let plat = platform.to_string();
                    let cid = chat_id.to_string();
                    let msg_clone = message.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_secs(minutes * 60)).await;
                        let text = format!("⏰ 提醒: {msg_clone}");
                        let _ = tx.send((plat, cid, text)).await;
                    });
                    tracing::info!(minutes, msg = %message, "gateway action: remind_after");
                    let time_str = if minutes >= 60 {
                        let h = minutes / 60;
                        let m = minutes % 60;
                        if m == 0 { format!("{h}小时") } else { format!("{h}小时{m}分钟") }
                    } else {
                        format!("{minutes}分钟")
                    };
                    format!("⏰ {time_str}后提醒: {message}")
                } else {
                    "⚠️ 延时提醒功能不可用".into()
                }
            }
            Some("remind_after") => "⚠️ remind_after 格式错误（需要: remind_after:分钟数:消息）".into(),

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
                let desc = if parts.len() >= 3 { Some(parts[2].trim().to_string()) } else { None };
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
                            format!("📋 任务已创建\n- ID: `{}`\n- 名称: {name}", &id.0[..8.min(id.0.len())])
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
                                let tid = astra_core::durable_task_store::TaskId(task_id.to_string());
                                match store.checkpoint(&tid, &state, None, None).await {
                                    Ok(()) => {
                                        tracing::info!(task_id, "dtask checkpoint saved");
                                        format!("💾 检查点已保存 (`{}`)", &task_id[..8.min(task_id.len())])
                                    }
                                    Err(e) => format!("⚠️ 保存失败: {e}"),
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
                    let tid = astra_core::durable_task_store::TaskId(task_id.to_string());
                    match store.get(&tid).await {
                        Ok(Some(t)) => {
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
                        Ok(None) => format!("❌ 任务 `{task_id}` 不存在"),
                        Err(e) => format!("⚠️ 查询失败: {e}"),
                    }
                } else {
                    "⚠️ 需要数据库支持".into()
                }
            }

            Some("dtask_resume") if parts.len() >= 2 => {
                let task_id = parts[1].trim();
                if let Some(store) = durable_store {
                    let tid = astra_core::durable_task_store::TaskId(task_id.to_string());
                    match store.resume(&tid).await {
                        Ok(Some(checkpoint)) => {
                            let _ = store.update_status(
                                &tid, astra_core::durable_task_store::DurableTaskStatus::Running, None,
                            ).await;
                            format!("▶️ 任务已恢复，检查点:\n```json\n{}\n```", serde_json::to_string_pretty(&checkpoint).unwrap_or_default())
                        }
                        Ok(None) => format!("▶️ 任务 `{task_id}` 无检查点，从头开始"),
                        Err(e) => format!("⚠️ 恢复失败: {e}"),
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
                                lines.push(format!("{icon} `{short_id}` | {} | {}%", t.name, t.progress_pct));
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
                    let tid = astra_core::durable_task_store::TaskId(task_id.to_string());
                    match store.update_status(&tid, astra_core::durable_task_store::DurableTaskStatus::Completed, None).await {
                        Ok(()) => "✅ 任务已完成".into(),
                        Err(e) => format!("⚠️ {e}"),
                    }
                } else { "⚠️ 需要数据库支持".into() }
            }

            Some("dtask_fail") if parts.len() >= 2 => {
                let task_id = parts[1].trim();
                let error = if parts.len() >= 3 { Some(parts[2].trim()) } else { None };
                if let Some(store) = durable_store {
                    let tid = astra_core::durable_task_store::TaskId(task_id.to_string());
                    match store.update_status(&tid, astra_core::durable_task_store::DurableTaskStatus::Failed, error).await {
                        Ok(()) => "❌ 任务已标记失败".into(),
                        Err(e) => format!("⚠️ {e}"),
                    }
                } else { "⚠️ 需要数据库支持".into() }
            }

            Some("dtask_cancel") if parts.len() >= 2 => {
                let task_id = parts[1].trim();
                if let Some(store) = durable_store {
                    let tid = astra_core::durable_task_store::TaskId(task_id.to_string());
                    match store.update_status(&tid, astra_core::durable_task_store::DurableTaskStatus::Cancelled, None).await {
                        Ok(()) => "🚫 任务已取消".into(),
                        Err(e) => format!("⚠️ {e}"),
                    }
                } else { "⚠️ 需要数据库支持".into() }
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

fn is_valid_cron_expr(expr: &str) -> bool {
    let parts: Vec<&str> = expr.split_whitespace().collect();
    if parts.len() != 5 {
        return false;
    }
    // Each field should be *, a number, a range, a list, or a step
    parts.iter().all(|p| {
        p.chars().all(|c| c.is_ascii_digit() || c == '*' || c == ',' || c == '-' || c == '/')
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
    let matched = jobs.iter().find(|(id, _, _, _)| {
        id == target || id.starts_with(target)
    });
    if let Some((id, _, desc, _)) = matched {
        let desc = desc.clone();
        storage::delete_cron_job(pool, id).await?;
        Ok(Some(desc))
    } else {
        Ok(None)
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
    fn split_preserves_code_block() {
        // Code block should not be split in the middle
        let code = format!("before\n\n```rust\n{}\n```\n\nafter", "let x = 1;\n".repeat(300));
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
            assert!(!has_orphan_fence, "code block was split mid-fence: {chunks:?}");
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
            assert!(chunks[0].ends_with('a'), "should split at paragraph boundary, got: {:?}...", &chunks[0][chunks[0].len()-20..]);
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
    async fn action_remind_after_valid() {
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let text = "好的\n[[GATEWAY:remind_after:5:喝水]]";
        let mut r = Vec::new();
        let clean = execute_gateway_actions(text, None, "wx", "c1", "u1", Some(&tx), None, &mut r).await;
        assert_eq!(clean, "好的");
        assert!(r[0].contains("5分钟"), "{}", r[0]);
        assert!(r[0].contains("喝水"), "{}", r[0]);
    }

    #[tokio::test]
    async fn action_remind_after_hours() {
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let text = "[[GATEWAY:remind_after:120:吃药]]";
        let mut r = Vec::new();
        execute_gateway_actions(text, None, "wx", "c1", "u1", Some(&tx), None, &mut r).await;
        assert!(r[0].contains("2小时"), "{}", r[0]);
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
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        execute_gateway_actions(text, None, "wx", "c1", "u1", Some(&tx), None, &mut r).await;
        assert!(r[0].contains("过长"), "{}", r[0]);
    }

    #[tokio::test]
    async fn action_remind_after_empty_message() {
        let text = "[[GATEWAY:remind_after:5:]]";
        let mut r = Vec::new();
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        execute_gateway_actions(text, None, "wx", "c1", "u1", Some(&tx), None, &mut r).await;
        assert!(r[0].contains("不能为空"), "{}", r[0]);
    }

    #[tokio::test]
    async fn action_remind_after_no_outbound() {
        let text = "[[GATEWAY:remind_after:5:test]]";
        let mut r = Vec::new();
        execute_gateway_actions(text, None, "wx", "c1", "u1", None, None, &mut r).await;
        assert!(r[0].contains("不可用"), "{}", r[0]);
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
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let text = "好的，帮你设置：\n[[GATEWAY:cron_add:0 9 * * 1-5:工作日早报]]\n[[GATEWAY:remind_after:30:半小时后开会]]";
        let mut r = Vec::new();
        let clean = execute_gateway_actions(text, None, "wx", "c1", "u1", Some(&tx), None, &mut r).await;
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
