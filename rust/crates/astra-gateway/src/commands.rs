//! Slash command handlers for the gateway.

use crate::access_control::{ActionCapability, ActionSource};
use crate::config::GatewayConfig;
use crate::store::{self, GatewayStore};
use crate::trace_model::{
    ActiveRequestSummary, CancelRequestOutcome, ConversationKey, GatewayEvent, GatewayEventKind,
    OutboxStatus, RequestStatus, TraceId, TraceRepository,
};
use astra_core::durable_task_store::{
    DurableTask, DurableTaskStatus, DurableTaskStore, TaskFilter, resolve_task_for_owner,
};

pub struct CommandContext<'a> {
    pub astra: &'a astra_thin_client::ThinClient,
    pub config: &'a GatewayConfig,
    pub store: Option<&'a dyn GatewayStore>,
    pub platform: &'a str,
    pub chat_id: &'a str,
    pub user_id: &'a str,
    pub resolved_cli: &'a crate::cli_bridge::CliProfile,
    pub durable_store: Option<&'a dyn astra_core::durable_task_store::DurableTaskStore>,
    pub trace_repo: Option<&'a dyn TraceRepository>,
    pub project_dirs: &'a [String],
    pub cli_availability: &'a [(String, crate::cli_bridge::CliAvailability)],
    pub auth_status: Option<String>,
    /// Active task registry — allows /kill to actually terminate CLI processes.
    pub active_tasks: Option<&'a dashmap::DashMap<String, tokio_util::sync::CancellationToken>>,
}

/// Helper: get store or return error message for storage-dependent commands.
macro_rules! require_store {
    ($ctx:expr) => {
        match $ctx.store {
            Some(s) => s,
            None => return Some("⚠️ 此命令需要存储。当前以无持久化模式运行。".into()),
        }
    };
}

macro_rules! require_trace_repo {
    ($ctx:expr) => {
        match $ctx.trace_repo {
            Some(repo) => repo,
            None => return Some("⚠️ 此命令需要 MySQL 存储。当前没有启用 trace/durable 能力。".into()),
        }
    };
}

async fn resolve_owned_task(
    store: &dyn DurableTaskStore,
    owner_id: &str,
    selector: &str,
) -> Result<DurableTask, String> {
    resolve_task_for_owner(store, owner_id, selector)
        .await
        .map_err(|e| format!("⚠️ {e}"))
}

pub async fn handle_command(ctx: &CommandContext<'_>, text: &str) -> Option<String> {
    let text = text.trim();
    if !text.starts_with('/') {
        return None;
    }

    let (cmd, arg) = text.split_once(' ').unwrap_or((text, ""));
    let arg = arg.trim();

    match cmd {
        "/new" | "/reset" => {
            if let Some(denial) = slash_denial(ctx, ActionCapability::SessionMutation) {
                return Some(denial);
            }
            let store = require_store!(ctx);
            let cli_name = ctx.resolved_cli.name();
            match store
                .reset_session(ctx.platform, ctx.chat_id, cli_name)
                .await
            {
                Ok(()) => Some(format!(
                    "🔄 `{cli_name}` 会话已重置。发送新消息开始新对话。"
                )),
                Err(e) => Some(format!("⚠️ 会话重置失败: {e}")),
            }
        }

        "/status" => {
            let cli_name = ctx.resolved_cli.name();
            let session = if let Some(store) = ctx.store {
                store
                    .get_current_session(ctx.platform, ctx.chat_id, cli_name)
                    .await
                    .ok()
                    .flatten()
            } else {
                None
            };
            let cli_model = match ctx.resolved_cli {
                crate::cli_bridge::CliProfile::Astra { model, .. }
                | crate::cli_bridge::CliProfile::Claude { model, .. } => model.as_deref(),
                _ => None,
            };
            let (model_display, model_source) = if let Some(m) = cli_model {
                (m.to_string(), "user override")
            } else if let Some(m) = ctx.config.astra.default_model.as_deref() {
                (m.to_string(), "config default")
            } else {
                ("(CLI default)".to_string(), "profile default")
            };
            let mut lines = vec![
                "📊 **状态**".to_string(),
                format!("- CLI: `{cli_name}`"),
                format!("- 模型: `{model_display}` ({model_source})"),
                format!("- 用户: `{}`", ctx.user_id),
                format!("- 会话: `{}`", session.as_deref().unwrap_or("(无)")),
                format!(
                    "- 存储: `{}`",
                    if ctx.store.is_some() { "on" } else { "off" }
                ),
            ];
            if let Some(auth_status) = ctx.auth_status.as_deref() {
                lines.push(format!("- 认证: {auth_status}"));
            }

            if let Some(repo) = ctx.trace_repo {
                let conversation = ConversationKey::new(ctx.platform, ctx.chat_id, cli_name);
                if let Ok(status) = repo.gateway_status(&conversation).await {
                    lines.push(format!(
                        "- 队列: queued={} running={} outbox_pending={} retrying={}",
                        status.queued_count,
                        status.running_count,
                        status.pending_outbox_count,
                        status.retrying_outbox_count
                    ));
                    if let Some(last) = status.last_trace {
                        lines.push(format!(
                            "- 最近 trace: `{}` ({})",
                            short_id(last.trace_id.as_str()),
                            last.status.as_str()
                        ));
                    }
                }
            }

            if let Some(ref sid) = session
                && let Some(snap) =
                    fetch_harness_snapshot(ctx.astra, sid, &ctx.config.astra.api_key).await
            {
                lines.push(String::new());
                lines.push("**🔭 Harness**".into());
                lines.push(format!(
                    "- 轮次: {}/{}",
                    snap.turns_used,
                    snap.turns_limit_str()
                ));
                lines.push(format!(
                    "- Token: ↓{} ↑{}",
                    format_tokens(snap.tokens_prompt),
                    format_tokens(snap.tokens_completion)
                ));
                lines.push(format!(
                    "- 工具: {} ({})",
                    snap.tool_calls,
                    snap.tool_summary()
                ));
                lines.push(format!("- 成本: ~${:.4}", snap.cost_estimate_usd()));
                for w in snap.warnings() {
                    lines.push(format!("- {w}"));
                }
            }
            Some(lines.join("\n"))
        }

        "/inspect" => {
            let cli_name = ctx.resolved_cli.name();
            let sid = match require_store!(ctx)
                .get_current_session(ctx.platform, ctx.chat_id, cli_name)
                .await
            {
                Ok(Some(s)) => s,
                _ => return Some("❌ 当前无活跃会话。".into()),
            };
            match fetch_harness_snapshot(ctx.astra, &sid, &ctx.config.astra.api_key).await {
                Some(snap) => Some(snap.format_full()),
                None => Some("⚠️ 暂无 harness 数据。".into()),
            }
        }

        "/session" => {
            let cli_name = ctx.resolved_cli.name();
            if arg.is_empty() || arg == "current" {
                let store = require_store!(ctx);
                let sid = store
                    .get_current_session(ctx.platform, ctx.chat_id, cli_name)
                    .await
                    .ok()
                    .flatten();
                if sid.is_some() {
                    return Some(format!(
                        "📋 **当前会话** (CLI: `{cli_name}`)\n- ID: `{}`",
                        sid.as_deref().unwrap_or("(无)")
                    ));
                }
                // Current CLI has no session — check all CLIs and show which ones do.
                let mut found = Vec::new();
                for (name, _) in ctx.config.cli_profiles.iter() {
                    if let Ok(Some(other_sid)) = store
                        .get_current_session(ctx.platform, ctx.chat_id, name)
                        .await
                    {
                        let short = &other_sid[..8.min(other_sid.len())];
                        found.push(format!("  `{name}`: `{short}…`"));
                    }
                }
                if found.is_empty() {
                    return Some(format!(
                        "📋 **当前会话** (CLI: `{cli_name}`)\n- ID: (无)\n\n所有 CLI 均无活跃会话。发送消息开始新对话。"
                    ));
                }
                let mut lines = vec![
                    format!("📋 **当前会话** (CLI: `{cli_name}`)"),
                    "- ID: (无)".into(),
                    String::new(),
                    "其他 CLI 有活跃会话:".into(),
                ];
                lines.extend(found);
                lines.push(format!("\n使用 `/cli <name>` 切换，或发送消息创建新 `{cli_name}` 会话。"));
                return Some(lines.join("\n"));
            }

            if arg == "list" {
                let sessions = require_store!(ctx)
                    .list_sessions(ctx.platform, ctx.chat_id, cli_name)
                    .await
                    .unwrap_or_default();
                if sessions.is_empty() {
                    return Some(format!("📋 `{cli_name}` 没有历史会话。"));
                }
                let mut lines = vec![format!("📋 **`{cli_name}` 会话列表**")];
                for s in &sessions {
                    let marker = if s.is_current { "→ " } else { "  " };
                    let short = &s.session_id[..8.min(s.session_id.len())];
                    lines.push(format!("{marker}`{short}…` ({})", s.created_at));
                }
                lines.push("\n使用 `/session switch <id>` 切换".into());
                return Some(lines.join("\n"));
            }

            if let Some(target) = arg
                .strip_prefix("switch ")
                .or_else(|| arg.strip_prefix("sw "))
            {
                let target = target.trim();
                match require_store!(ctx)
                    .switch_session(ctx.platform, ctx.chat_id, target)
                    .await
                {
                    Ok(true) => Some(format!(
                        "✅ 已切换到会话 `{}`",
                        &target[..8.min(target.len())]
                    )),
                    Ok(false) => Some(format!("❌ 找不到会话 `{target}`")),
                    Err(e) => Some(format!("⚠️ 切换失败: {e}")),
                }
            } else {
                Some("用法: `/session [list|switch <id>|current]`".into())
            }
        }

        "/cron" => {
            if arg.is_empty() || arg == "list" {
                let jobs = require_store!(ctx)
                    .list_cron_jobs(ctx.platform, ctx.chat_id)
                    .await
                    .unwrap_or_default();
                if jobs.is_empty() {
                    return Some("⏰ 没有定时任务。用 `/cron add` 创建。".into());
                }
                let mut lines = vec!["⏰ **定时任务**".to_string()];
                for j in &jobs {
                    let status = if j.enabled { "✅" } else { "⏸" };
                    let short_id = &j.job_id[..8.min(j.job_id.len())];
                    lines.push(format!(
                        "{status} `{short_id}` | `{}` | {}",
                        j.cron_expr, j.description
                    ));
                }
                lines.push(
                    "\n`/cron add <cron_expr> <消息>` — 创建\n`/cron del <id>` — 删除".into(),
                );
                return Some(lines.join("\n"));
            }

            if let Some(rest) = arg.strip_prefix("add ") {
                if let Some(denial) = slash_denial(ctx, ActionCapability::CronMutation) {
                    return Some(denial);
                }
                // Parse: /cron add "0 9 * * 1-5" 每天早上9点汇报
                let (cron_expr, message) = match parse_cron_add(rest) {
                    Some(parsed) => parsed,
                    None => {
                        return Some(
                            "⚠️ 格式错误。用法:\n\
                             `/cron add \"0 9 * * 1-5\" 每天早上9点汇报`\n\
                             `/cron add 0 9 * * * 每天早上9点汇报`"
                                .into(),
                        );
                    }
                };
                let job_id = uuid::Uuid::new_v4().to_string();
                let store = require_store!(ctx);
                match store
                    .create_cron_job(&store::CronJobSpec {
                        job_id: job_id.clone(),
                        platform: ctx.platform.to_string(),
                        chat_id: ctx.chat_id.to_string(),
                        user_id: ctx.user_id.to_string(),
                        cron_expr: cron_expr.clone(),
                        message: message.clone(),
                        description: message.clone(),
                    })
                    .await
                {
                    Ok(()) => Some(format!(
                        "✅ 定时任务已创建\n- ID: `{}`\n- 表达式: `{cron_expr}`\n- 任务: {message}",
                        &job_id[..8]
                    )),
                    Err(e) => Some(format!("⚠️ 创建失败: {e}")),
                }
            } else if let Some(id) = arg.strip_prefix("del ").or_else(|| arg.strip_prefix("rm ")) {
                if let Some(denial) = slash_denial(ctx, ActionCapability::CronMutation) {
                    return Some(denial);
                }
                match require_store!(ctx).delete_cron_job(id.trim()).await {
                    Ok(true) => Some("✅ 任务已删除".into()),
                    Ok(false) => Some("❌ 找不到该任务".into()),
                    Err(e) => Some(format!("⚠️ 删除失败: {e}")),
                }
            } else {
                Some("用法: `/cron [list|add <expr> <msg>|del <id>]`".into())
            }
        }

        "/model" => {
            let cli_name = ctx.resolved_cli.name();
            let current_model = match ctx.resolved_cli {
                crate::cli_bridge::CliProfile::Astra { model, .. }
                | crate::cli_bridge::CliProfile::Claude { model, .. } => model.as_deref(),
                _ => None,
            }
            .or(ctx.config.astra.default_model.as_deref())
            .unwrap_or("(server default)");

            if arg.is_empty() {
                let shortcuts = model_shortcuts();
                let mut lines = vec![
                    format!("🤖 当前模型: `{current_model}` (CLI: `{cli_name}`)"),
                    String::new(),
                    "**快捷切换:**".into(),
                ];
                for (shortcut, full_name, desc) in &shortcuts {
                    lines.push(format!("  `/model {shortcut}` → `{full_name}` ({desc})"));
                }
                lines.push(String::new());
                lines.push("或指定完整名: `/model <model-name>`".into());
                return Some(lines.join("\n"));
            }

            // Resolve shortcut or use as-is
            if let Some(denial) = slash_denial(ctx, ActionCapability::ModelMutation) {
                return Some(denial);
            }
            let target = resolve_model_shortcut(arg);
            if let Some(store) = ctx.store {
                let model_key = store::model_preference_key(ctx.resolved_cli.name());
                if let Err(e) = store
                    .set_user_preference(ctx.platform, ctx.user_id, &model_key, &target)
                    .await
                {
                    return Some(format!("⚠️ 模型设置失败: {e}"));
                }
            }
            Some(format!("🤖 模型已切换: `{target}`\n(下次消息生效)"))
        }

        "/cli" => {
            if arg.is_empty() {
                // Show current CLI + available profiles + workspace
                let current = ctx.config.cli.name();
                let caps = ctx.config.cli.capabilities();
                let workspace = if let Some(s) = ctx.store {
                    s.get_user_preference(ctx.platform, ctx.user_id, "workspace")
                        .await
                        .ok()
                        .flatten()
                } else {
                    None
                };
                let ws_display = workspace.as_deref().unwrap_or("(默认)");
                let mut lines = vec![
                    format!("🔧 **当前 CLI: `{current}`**"),
                    format!("📂 工作目录: `{ws_display}`"),
                    format!(
                        "  能力: {}session {}model {}harness {}tools",
                        if caps.supports_session { "✅" } else { "❌" },
                        if caps.supports_model_switch {
                            "✅"
                        } else {
                            "❌"
                        },
                        if caps.supports_harness { "✅" } else { "❌" },
                        if caps.supports_tools { "✅" } else { "❌" },
                    ),
                ];
                if !ctx.config.cli_profiles.is_empty() {
                    lines.push("\n**可用 CLI:**".into());
                    for (name, profile) in &ctx.config.cli_profiles {
                        let c = profile.capabilities();
                        // Look up availability from pre-probed list
                        let (status_icon, version_info) = ctx
                            .cli_availability
                            .iter()
                            .find(|(n, _)| n == name)
                            .map(|(_, avail)| {
                                let icon = if avail.is_available() { "✅" } else { "❌" };
                                let ver = match avail {
                                    crate::cli_bridge::CliAvailability::Available { version } => {
                                        version
                                            .as_deref()
                                            .map(|v| format!(" {v}"))
                                            .unwrap_or_default()
                                    }
                                    crate::cli_bridge::CliAvailability::NotInstalled => {
                                        " — 未安装".into()
                                    }
                                    crate::cli_bridge::CliAvailability::NotExecutable(e) => {
                                        format!(" — {e}")
                                    }
                                };
                                (icon, ver)
                            })
                            .unwrap_or(("  ", String::new()));
                        lines.push(format!(
                            "  {status_icon} `{name}` ({}{}{}){version_info}",
                            profile.name(),
                            if c.supports_harness { " +harness" } else { "" },
                            if c.supports_session { " +session" } else { "" },
                        ));
                    }
                    lines.push("\n用 `/cli <name>` 切换".into());
                }
                return Some(lines.join("\n"));
            }

            // Switch to a named profile
            if let Some(profile) = ctx.config.cli_profiles.get(arg) {
                if let Some(denial) = slash_denial(ctx, ActionCapability::CliMutation) {
                    return Some(denial);
                }
                let caps = profile.capabilities();
                let cap_str = format!(
                    "session={} model={} harness={} tools={}",
                    if caps.supports_session { "✅" } else { "❌" },
                    if caps.supports_model_switch {
                        "✅"
                    } else {
                        "❌"
                    },
                    if caps.supports_harness { "✅" } else { "❌" },
                    if caps.supports_tools { "✅" } else { "❌" },
                );
                if let Some(s) = ctx.store
                    && let Err(e) = s
                        .set_user_preference(ctx.platform, ctx.user_id, "cli_profile", arg)
                        .await
                {
                    return Some(format!("⚠️ CLI 切换失败: {e}"));
                }
                Some(format!(
                    "✅ 已切换到 `{arg}` ({name})\n{cap_str}",
                    name = profile.name()
                ))
            } else {
                let available: Vec<&str> =
                    ctx.config.cli_profiles.keys().map(|s| s.as_str()).collect();
                Some(format!(
                    "❌ 未找到 CLI `{arg}`\n可用: {}",
                    if available.is_empty() {
                        "(无配置)".into()
                    } else {
                        available.join(", ")
                    }
                ))
            }
        }

        "/approve" => Some(
            "🔐 **工具权限说明**\n\n\
             Gateway 模式下工具自动执行（`--auto-approve`）。\n\
             安全由 Harness 保障：\n\
             - 🛡 BudgetVerifier 限制轮次/token\n\
             - 🛡 TurnGuard 检测工具循环\n\
             - 🛡 CostVerifier 限制成本"
                .into(),
        ),

        "/help" => Some(
            "💡 **命令列表**\n\n\
             **对话**\n\
             `/new` (`/reset`) — 新建会话\n\
             `/session list` — 历史会话\n\
             `/session switch <id>` — 切换会话\n\n\
             **模型**\n\
             `/model` — 当前模型 + 快捷列表\n\
             `/model <name>` — 切换 (haiku/sonnet/opus/minimax/deepseek/qwen/glm)\n\n\
             **CLI & 工作区**\n\
             `/cli` — 查看当前 CLI + 能力 + 工作目录\n\
             `/cli <name>` — 切换 CLI (astra/claude)\n\
             `/ws` — 当前工作目录\n\
             `/ws ls` — 列出可用项目\n\
             `/ws <name|path>` — 切换工作目录\n\n\
             **任务管理**\n\
             `/running` — 查看正在执行的任务 (带编号)\n\
             `/cancel [N|text]` — 取消排队请求 (序号/文本/ID)\n\
             `/kill [N|text]` — 终止运行中请求 (序号/文本/ID)\n\
             `/retry` — 查看发送失败的消息\n\
             `/retry dismiss` — 清除所有失败消息\n\
             `/manage [指令]` — AI 辅助任务管理\n\
             `/usage` — 用量统计\n\n\
             **监控**\n\
             `/status` — 状态 + harness\n\
             `/inspect` — harness 详情 (token/cost/tools/warnings)\n\
             `/audit` — 审计记录 (最近 N 轮决策链)\n\
             `/trace [id]` — 查看 trace 详情\n\n\
             **持久任务**\n\
             `/task list` — 查看任务\n\
             `/task status <id>` — 任务状态\n\
             `/task cancel <id>` — 取消任务\n\
             `/task resume <id>` — 恢复任务\n\n\
             **定时任务**\n\
             `/cron list` — 查看任务\n\
             `/cron add <expr> <msg>` — 创建\n\
             `/cron del <id>` — 删除\n\n\
             **其他**\n\
             `/auth` — 认证状态 + 重置 + 自动重登录\n\
             `/gateway` — 完整能力概览\n\
             `/approve` — 权限说明"
                .into(),
        ),

        "/task" => {
            let store = match ctx.durable_store {
                Some(s) => s,
                None => return Some("⚠️ 持久任务需要 MySQL 存储支持".into()),
            };
            let owner_id = format!("{}:{}", ctx.platform, ctx.chat_id);

            if arg.is_empty() || arg == "list" {
                let filter = TaskFilter {
                    owner_id: Some(owner_id.clone()),
                    ..Default::default()
                };
                match store.list(filter).await {
                    Ok(tasks) if tasks.is_empty() => Some("📋 没有持久任务。".into()),
                    Ok(tasks) => {
                        let mut lines = vec![format!("📋 **持久任务** ({} 个)", tasks.len())];
                        for t in &tasks {
                            let short_id = &t.id.0[..8.min(t.id.0.len())];
                            let icon = match t.status {
                                astra_core::durable_task_store::DurableTaskStatus::Running => "🔄",
                                astra_core::durable_task_store::DurableTaskStatus::Suspended => "⏸",
                                astra_core::durable_task_store::DurableTaskStatus::Completed => {
                                    "✅"
                                }
                                astra_core::durable_task_store::DurableTaskStatus::Failed => "❌",
                                astra_core::durable_task_store::DurableTaskStatus::Cancelled => {
                                    "🚫"
                                }
                                _ => "📋",
                            };
                            lines.push(format!(
                                "{icon} `{short_id}` | {} | {}%",
                                t.name, t.progress_pct
                            ));
                        }
                        Some(lines.join("\n"))
                    }
                    Err(e) => Some(format!("⚠️ 查询失败: {e}")),
                }
            } else if let Some(id) = arg
                .strip_prefix("cancel ")
                .or_else(|| arg.strip_prefix("rm "))
            {
                if let Some(denial) = slash_denial(ctx, ActionCapability::DurableTaskMutation) {
                    return Some(denial);
                }
                let task = match resolve_owned_task(store, &owner_id, id).await {
                    Ok(task) => task,
                    Err(e) => return Some(e),
                };
                match store
                    .update_status(&task.id, DurableTaskStatus::Cancelled, None)
                    .await
                {
                    Ok(()) => Some("🚫 任务已取消".into()),
                    Err(e) => Some(format!("⚠️ {e}")),
                }
            } else if let Some(id) = arg.strip_prefix("resume ") {
                if let Some(denial) = slash_denial(ctx, ActionCapability::DurableTaskMutation) {
                    return Some(denial);
                }
                let task = match resolve_owned_task(store, &owner_id, id).await {
                    Ok(task) => task,
                    Err(e) => return Some(e),
                };
                match store.resume(&task.id).await {
                    Ok(Some(cp)) => {
                        if let Err(e) = store
                            .update_status(&task.id, DurableTaskStatus::Running, None)
                            .await
                        {
                            return Some(format!("⚠️ 恢复失败: {e}"));
                        }
                        Some(format!(
                            "▶️ 任务已恢复\n检查点:\n```\n{}\n```",
                            serde_json::to_string_pretty(&cp).unwrap_or_default()
                        ))
                    }
                    Ok(None) => {
                        if let Err(e) = store
                            .update_status(&task.id, DurableTaskStatus::Running, None)
                            .await
                        {
                            return Some(format!("⚠️ 恢复失败: {e}"));
                        }
                        Some("▶️ 任务无检查点，将从头开始".into())
                    }
                    Err(e) => Some(format!("⚠️ {e}")),
                }
            } else if let Some(id) = arg.strip_prefix("status ") {
                match resolve_owned_task(store, &owner_id, id).await {
                    Ok(t) => {
                        let mut lines = vec![
                            format!("📋 **{}**", t.name),
                            format!("- 状态: {}", t.status.as_str()),
                            format!("- 进度: {}%", t.progress_pct),
                        ];
                        if let Some(ref step) = t.step_description {
                            lines.push(format!("- 当前: {step}"));
                        }
                        if let Some(ref err) = t.error_message {
                            lines.push(format!("- 信息: {err}"));
                        }
                        Some(lines.join("\n"))
                    }
                    Err(e) => Some(e),
                }
            } else {
                Some("用法: `/task [list|cancel <id>|resume <id>|status <id>]`".into())
            }
        }

        "/audit" => {
            let cli_name = ctx.resolved_cli.name();
            let store = require_store!(ctx);
            let sid = match store
                .get_current_session(ctx.platform, ctx.chat_id, cli_name)
                .await
            {
                Ok(Some(s)) => s,
                _ => return Some("❌ 当前无活跃会话。".into()),
            };
            let history =
                fetch_harness_history(ctx.astra, &sid, &ctx.config.astra.api_key, 50).await;
            if history.is_empty() {
                return Some("📋 暂无审计记录。".into());
            }
            Some(format_audit_history(history))
        }

        "/running" => {
            let repo = require_trace_repo!(ctx);
            let conversation =
                ConversationKey::new(ctx.platform, ctx.chat_id, ctx.resolved_cli.name());
            let rows = repo
                .list_active_requests(&conversation, 20)
                .await
                .unwrap_or_default();
            if rows.is_empty() {
                Some("✅ 当前没有正在执行的任务。".into())
            } else {
                let mut lines = vec![format!("🔄 **正在执行** ({} 个)", rows.len())];
                let mut stuck_outbox_count = 0usize;
                for (i, row) in rows.iter().enumerate() {
                    let icon = status_icon(row.display_status());
                    let tag = crate::runner::short_request_tag(row.trace_id.as_str());
                    let short_text = truncate_text(&row.text_preview, 40);
                    let ts = short_timestamp(&row.created_at);
                    lines.push(format!(
                        "[{}] {} {} | {} | {} | {}",
                        i + 1,
                        icon,
                        row.display_status(),
                        tag,
                        short_text,
                        ts,
                    ));
                    if row.status.is_terminal() && row.outbox_status == Some(OutboxStatus::Failed) {
                        stuck_outbox_count += 1;
                    }
                }
                if stuck_outbox_count > 0 {
                    lines.push(format!(
                        "\n📬 有 {stuck_outbox_count} 个消息发送失败。用 `/retry` 查看，`/retry dismiss` 清除。"
                    ));
                }
                lines.push("\n💡 `/kill 1` 终止 | `/cancel 2` 取消 | `/manage` AI 辅助管理".into());
                Some(lines.join("\n"))
            }
        }

        "/trace" => {
            let repo = require_trace_repo!(ctx);
            let conversation =
                ConversationKey::new(ctx.platform, ctx.chat_id, ctx.resolved_cli.name());
            if arg.is_empty() || arg == "list" {
                let traces = repo
                    .list_recent_traces(&conversation, 10)
                    .await
                    .unwrap_or_default();
                if traces.is_empty() {
                    return Some("📋 暂无 trace。".into());
                }
                let mut lines = vec![format!("🧭 **最近 Trace** ({} 个)", traces.len())];
                for trace in traces {
                    lines.push(format!(
                        "- `{}` {} | {} | {} events | {}",
                        short_id(trace.trace_id.as_str()),
                        trace.status.as_str(),
                        trace.text_preview,
                        trace.event_count,
                        trace.created_at
                    ));
                }
                lines.push("\n用 `/trace <trace_id>` 查看详情".into());
                return Some(lines.join("\n"));
            }

            let trace_id = resolve_trace_selector(repo, &conversation, arg).await;
            let Some(trace_id) = trace_id else {
                return Some(format!("❌ 找不到 trace `{arg}`"));
            };
            let events = repo
                .list_events_for_trace(&trace_id, 80)
                .await
                .unwrap_or_default();
            if events.is_empty() {
                return Some(format!(
                    "📋 Trace `{}` 暂无事件。",
                    short_id(trace_id.as_str())
                ));
            }
            Some(format_trace_events(&trace_id, events))
        }

        "/cancel" => {
            let repo = require_trace_repo!(ctx);
            let conversation =
                ConversationKey::new(ctx.platform, ctx.chat_id, ctx.resolved_cli.name());
            if arg.is_empty() {
                // Auto-pick first cancellable
                let row = repo
                    .list_active_requests(&conversation, 20)
                    .await
                    .ok()
                    .and_then(|rows| rows.into_iter().find(|r| r.is_cancellable()));
                let Some(row) = row else {
                    return Some("✅ 当前没有可取消的排队请求。运行中的请求请用 `/kill`。".into());
                };
                match repo
                    .cancel_accepted_request(
                        &conversation,
                        row.trace_id.as_str(),
                        "cancelled by user",
                    )
                    .await
                {
                    Ok(CancelRequestOutcome::Cancelled(r)) => {
                        Some(format!("🚫 已取消: {}", truncate_text(&r.text_preview, 40)))
                    }
                    Ok(CancelRequestOutcome::AlreadyRunning(_)) => {
                        Some("⚠️ 请求已在运行，用 `/kill` 终止。".into())
                    }
                    Ok(CancelRequestOutcome::NotFound) => Some("❌ 找不到可取消请求".into()),
                    Err(e) => Some(format!("⚠️ 取消失败: {e}")),
                }
            } else {
                // Resolve by number, text, or ID
                let Some(row) = resolve_active_request(repo, &conversation, arg).await else {
                    return Some(format!("❌ 找不到匹配 `{arg}` 的请求"));
                };
                if !row.is_cancellable() {
                    return Some(format!(
                        "⚠️ 请求 [{}] 已在运行，用 `/kill` 终止。",
                        truncate_text(&row.text_preview, 30)
                    ));
                }
                match repo
                    .cancel_accepted_request(
                        &conversation,
                        row.trace_id.as_str(),
                        "cancelled by user",
                    )
                    .await
                {
                    Ok(CancelRequestOutcome::Cancelled(r)) => {
                        Some(format!("🚫 已取消: {}", truncate_text(&r.text_preview, 40)))
                    }
                    Ok(CancelRequestOutcome::AlreadyRunning(_)) => {
                        Some("⚠️ 请求已在运行，用 `/kill` 终止。".into())
                    }
                    Ok(CancelRequestOutcome::NotFound) => {
                        Some(format!("❌ 找不到可取消请求 `{arg}`"))
                    }
                    Err(e) => Some(format!("⚠️ 取消失败: {e}")),
                }
            }
        }

        "/kill" => {
            let repo = require_trace_repo!(ctx);
            let conversation =
                ConversationKey::new(ctx.platform, ctx.chat_id, ctx.resolved_cli.name());

            let row = if arg.is_empty() {
                // Auto-pick the most recent running request
                repo.list_active_requests(&conversation, 20)
                    .await
                    .ok()
                    .and_then(|rows| {
                        rows.into_iter()
                            .find(|r| r.status == RequestStatus::Running)
                    })
            } else {
                // Resolve by number, text, or ID
                resolve_active_request(repo, &conversation, arg).await
            };

            let Some(row) = row else {
                if arg.is_empty() {
                    return Some("✅ 当前没有运行中的请求。".into());
                }
                return Some(format!("❌ 找不到匹配 `{arg}` 的请求"));
            };
            match repo
                .force_fail_request(&row.trace_id, "killed by user via /kill")
                .await
            {
                Ok(true) => {
                    // Actually kill the CLI process via cancellation token.
                    let process_killed = ctx
                        .active_tasks
                        .map(|tasks| {
                            if let Some((_, token)) = tasks.remove(row.trace_id.as_str()) {
                                token.cancel();
                                true
                            } else {
                                false
                            }
                        })
                        .unwrap_or(false);
                    let suffix = if process_killed {
                        " (进程已杀死)"
                    } else {
                        " (等待自然退出)"
                    };
                    Some(format!(
                        "💀 已终止: {}{}",
                        truncate_text(&row.text_preview, 40),
                        suffix
                    ))
                }
                Ok(false) => Some(format!(
                    "⚠️ 请求已是终态: {}",
                    truncate_text(&row.text_preview, 30)
                )),
                Err(e) => Some(format!("⚠️ 终止失败: {e}")),
            }
        }

        "/retry" => {
            let repo = require_trace_repo!(ctx);
            let conversation =
                ConversationKey::new(ctx.platform, ctx.chat_id, ctx.resolved_cli.name());

            // Find requests with failed outbox
            let rows = repo
                .list_active_requests(&conversation, 20)
                .await
                .unwrap_or_default();
            let stuck: Vec<_> = rows
                .iter()
                .filter(|r| r.status.is_terminal() && r.outbox_status == Some(OutboxStatus::Failed))
                .collect();

            if stuck.is_empty() {
                return Some("✅ 没有需要重试的消息。".into());
            }

            if arg == "dismiss" || arg == "clear" {
                let mut dismissed = 0usize;
                for row in &stuck {
                    if repo.dismiss_failed_outbox(&row.request_id).await.is_ok() {
                        dismissed += 1;
                    }
                }
                return Some(format!("🧹 已清除 {dismissed} 个失败消息。"));
            }

            // Support `/retry 1` to dismiss a specific item by index
            if let Ok(idx) = arg.parse::<usize>() {
                if idx >= 1 && idx <= stuck.len() {
                    let row = stuck[idx - 1];
                    match repo.dismiss_failed_outbox(&row.request_id).await {
                        Ok(()) => {
                            return Some(format!(
                                "🧹 已清除: {}",
                                truncate_text(&row.text_preview, 40)
                            ));
                        }
                        Err(e) => return Some(format!("⚠️ 清除失败: {e}")),
                    }
                } else {
                    return Some(format!("❌ 序号 {idx} 超出范围 (1-{})", stuck.len()));
                }
            }

            let mut lines = vec![format!("📬 **待重试消息** ({} 个)", stuck.len())];
            for (i, row) in stuck.iter().enumerate() {
                lines.push(format!(
                    "[{}] 📬 {} | {}",
                    i + 1,
                    truncate_text(&row.text_preview, 40),
                    row.outbox_error_message
                        .as_deref()
                        .unwrap_or("unknown error"),
                ));
            }
            lines.push("\n`/retry dismiss` 清除全部 | `/retry 1` 清除指定".into());
            Some(lines.join("\n"))
        }

        "/usage" => {
            let store = require_store!(ctx);
            let today = store
                .get_usage_today(ctx.platform, ctx.user_id)
                .await
                .unwrap_or_default();
            let total = store
                .get_usage_total(ctx.platform, ctx.user_id)
                .await
                .unwrap_or_default();
            Some(format!(
                "📊 **用量统计**\n\n\
                 **今日**\n\
                 - 消息: {}\n\
                 - Token: ↓{} ↑{}\n\
                 - 工具: {}\n\n\
                 **累计**\n\
                 - 消息: {}\n\
                 - Token: ↓{} ↑{}\n\
                 - 工具: {}",
                today.messages,
                format_tokens(today.tokens_prompt),
                format_tokens(today.tokens_completion),
                today.tool_calls,
                total.messages,
                format_tokens(total.tokens_prompt),
                format_tokens(total.tokens_completion),
                total.tool_calls,
            ))
        }

        "/workspace" | "/ws" => {
            // /ws ls | /ws list — list discovered projects (no store needed)
            if arg == "ls" || arg == "list" {
                let projects = crate::workspace::discover_all_projects(ctx.project_dirs);
                if projects.is_empty() {
                    return Some("📂 没有发现项目。配置 `project_dirs` 后重试。".into());
                }
                let mut lines = vec![format!("📂 **可用项目** ({} 个)", projects.len())];
                for p in &projects {
                    lines.push(format!("  {}", p.summary()));
                }
                lines.push("\n用 `/ws <项目名>` 切换".into());
                return Some(lines.join("\n"));
            }

            let store = require_store!(ctx);
            if arg.is_empty() {
                let ws = store
                    .get_user_preference(ctx.platform, ctx.user_id, "workspace")
                    .await
                    .ok()
                    .flatten();
                return Some(format!(
                    "📂 当前工作目录: `{}`",
                    ws.as_deref().unwrap_or("(默认)")
                ));
            }

            // Try name-based fuzzy match against discovered projects
            let projects = crate::workspace::discover_all_projects(ctx.project_dirs);
            let arg_lower = arg.to_lowercase();
            let matches: Vec<_> = projects
                .iter()
                .filter(|p| {
                    p.name.eq_ignore_ascii_case(arg) || p.name.to_lowercase().contains(&arg_lower)
                })
                .collect();
            let target = match matches.len() {
                1 => matches[0].path.clone(),
                0 => {
                    // No project match — fall through to path-based logic
                    if arg.starts_with('~') {
                        let home = std::env::var("HOME").unwrap_or_default();
                        arg.replacen('~', &home, 1)
                    } else {
                        arg.to_string()
                    }
                }
                _ => {
                    let names: Vec<_> = matches
                        .iter()
                        .map(|p| format!("  {}", p.summary()))
                        .collect();
                    return Some(format!(
                        "⚠️ 多个匹配:\n{}\n请更精确指定。",
                        names.join("\n")
                    ));
                }
            };

            let path = std::path::Path::new(&target);
            if !path.is_dir() {
                return Some(format!("❌ 目录不存在: `{target}`"));
            }
            if let Some(denial) = slash_denial(ctx, ActionCapability::WorkspaceMutation) {
                return Some(denial);
            }
            if let Err(denial) = ctx.config.action_policy.workspace_allowed(path) {
                return Some(denial);
            }
            let canonical = path
                .canonicalize()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or(target);
            match store
                .set_user_preference(ctx.platform, ctx.user_id, "workspace", &canonical)
                .await
            {
                Ok(()) => Some(format!("📂 工作目录已切换: `{canonical}`")),
                Err(e) => Some(format!("⚠️ 工作目录设置失败: {e}")),
            }
        }

        "/gateway" => {
            let cli_name = ctx.resolved_cli.name();
            let caps = ctx.resolved_cli.capabilities();
            let has_store = ctx.store.is_some();

            let model = match ctx.resolved_cli {
                crate::cli_bridge::CliProfile::Astra { model, .. }
                | crate::cli_bridge::CliProfile::Claude { model, .. } => model.as_deref(),
                _ => None,
            }
            .or(ctx.config.astra.default_model.as_deref())
            .unwrap_or("default");

            let workspace = if let Some(store) = ctx.store {
                store
                    .get_user_preference(ctx.platform, ctx.user_id, "workspace")
                    .await
                    .ok()
                    .flatten()
            } else {
                None
            };

            let mut lines = vec![
                "🌐 **Gateway Context**".to_string(),
                String::new(),
                "**Identity**".to_string(),
                format!("- Platform: `{}`", ctx.platform),
                format!("- User: `{}`", ctx.user_id),
                format!("- CLI: `{cli_name}`"),
                format!("- Model: `{model}`"),
                format!(
                    "- Storage: {}",
                    if has_store { "✅ active" } else { "❌ none" }
                ),
                format!(
                    "- Workspace: `{}`",
                    workspace.as_deref().unwrap_or("(default)")
                ),
                String::new(),
                "**Capabilities**".to_string(),
                format!(
                    "- Session management: {}",
                    if caps.supports_session { "✅" } else { "❌" }
                ),
                format!(
                    "- Model switching: {}",
                    if caps.supports_model_switch {
                        "✅"
                    } else {
                        "❌"
                    }
                ),
                format!(
                    "- Harness monitoring: {}",
                    if caps.supports_harness { "✅" } else { "❌" }
                ),
                format!("- Cron/scheduling: {}", if has_store { "✅" } else { "❌" }),
                format!("- Durable tasks: {}", if has_store { "✅" } else { "❌" }),
                format!(
                    "- Tool execution: {}",
                    if caps.supports_tools { "✅" } else { "❌" }
                ),
            ];

            lines.push(String::new());
            lines.push("**Commands**".to_string());
            lines.push("| Command | Description |".to_string());
            lines.push("|---------|-------------|".to_string());
            lines.push("| `/new` | Reset conversation |".to_string());
            lines.push("| `/status` | Status + harness |".to_string());
            lines.push("| `/model <name>` | Switch model |".to_string());
            lines.push("| `/cli <name>` | Switch CLI backend |".to_string());
            lines.push("| `/ws ls` | List projects |".to_string());
            lines.push("| `/ws <name>` | Switch workspace |".to_string());
            lines.push("| `/session list` | Session history |".to_string());
            lines.push("| `/cron list` | Scheduled tasks |".to_string());
            lines.push("| `/task list` | Durable tasks |".to_string());
            lines.push("| `/running` | Active requests (numbered) |".to_string());
            lines.push("| `/cancel [N\\|text]` | Cancel queued request |".to_string());
            lines.push("| `/kill [N\\|text]` | Force-kill running request |".to_string());
            lines.push("| `/retry` | View/dismiss failed outbox |".to_string());
            lines.push("| `/manage [hint]` | AI-assisted task management |".to_string());
            lines.push("| `/trace [id]` | Request trace |".to_string());
            lines.push("| `/usage` | Token/cost stats |".to_string());
            lines.push("| `/inspect` | Harness details |".to_string());
            lines.push("| `/audit` | Decision chain |".to_string());
            lines.push("| `/auth` | Auth status + reset + auto-relogin |".to_string());
            lines.push("| `/gateway` | This context dump |".to_string());

            if ctx.config.action_policy.allow_model_generated_mutations && has_store {
                lines.push(String::new());
                lines.push(
                    "**Gateway Actions** (embed `[[GATEWAY:...]]` tags in response)".to_string(),
                );
                lines.push("| Tag | Effect |".to_string());
                lines.push("|-----|--------|".to_string());
                lines.push("| `cron_add:<expr>:<msg>` | Create recurring task |".to_string());
                lines.push("| `remind_after:<min>:<msg>` | One-time reminder |".to_string());
                lines.push("| `task_list` | List scheduled tasks |".to_string());
                lines.push("| `task_del:<id>` | Delete task |".to_string());
                lines.push("| `dtask_create:<name>:<desc>` | Create durable task |".to_string());
                lines.push("| `dtask_checkpoint:<id>:<json>` | Save checkpoint |".to_string());
                lines.push("| `dtask_complete:<id>` | Mark complete |".to_string());
                lines.push("| `workspace_set:<path>` | Switch workspace |".to_string());
                lines.push("| `skill_add:<name>:<md>` | Save reusable skill |".to_string());
                lines.push("| `trace_kill:<trace_id>` | Force-kill request |".to_string());
                lines.push("| `outbox_dismiss:<request_id>` | Dismiss failed outbox |".to_string());
            }

            if let Some(store) = ctx.store {
                let cron_jobs = store
                    .list_cron_jobs(ctx.platform, ctx.chat_id)
                    .await
                    .unwrap_or_default();
                if !cron_jobs.is_empty() {
                    lines.push(String::new());
                    lines.push(format!("**Scheduled Tasks** ({} 个)", cron_jobs.len()));
                    for j in &cron_jobs {
                        let short = &j.job_id[..8.min(j.job_id.len())];
                        lines.push(format!(
                            "- `{short}` | `{}` | {}",
                            j.cron_expr, j.description
                        ));
                    }
                }
            }

            Some(lines.join("\n"))
        }

        _ => None,
    }
}

fn parse_cron_add(input: &str) -> Option<(String, String)> {
    let input = input.trim();
    // Try quoted: /cron add "0 9 * * *" message
    if let Some(after_quote) = input.strip_prefix('"')
        && let Some(end) = after_quote.find('"')
    {
        let expr = after_quote[..end].to_string();
        let msg = after_quote[end + 1..].trim().to_string();
        if !expr.is_empty() && !msg.is_empty() && store::is_valid_cron_expr(&expr) {
            return Some((expr, msg));
        }
    }
    // Try unquoted: first 5 space-separated tokens are cron, rest is message
    let parts: Vec<&str> = input.splitn(6, ' ').collect();
    if parts.len() >= 6 {
        let expr = parts[..5].join(" ");
        let msg = parts[5].to_string();
        if !msg.is_empty() && store::is_valid_cron_expr(&expr) {
            return Some((expr, msg));
        }
    }
    None
}

fn slash_denial(ctx: &CommandContext<'_>, capability: ActionCapability) -> Option<String> {
    ctx.config
        .action_policy
        .check(ActionSource::SlashCommand, capability)
        .err()
}

async fn resolve_trace_selector(
    repo: &dyn TraceRepository,
    conversation: &ConversationKey,
    selector: &str,
) -> Option<TraceId> {
    let traces = repo.list_recent_traces(conversation, 50).await.ok()?;
    traces
        .into_iter()
        .find(|trace| {
            trace.trace_id.as_str() == selector
                || trace.request_id.as_str() == selector
                || trace.trace_id.as_str().starts_with(selector)
                || trace.request_id.as_str().starts_with(selector)
        })
        .map(|trace| trace.trace_id)
        .or_else(|| Some(TraceId::from_string(selector.to_string())))
}

/// Resolve a selector to an active request. Supports:
/// - Numeric index "1", "2" (from `/running` output order)
/// - Trace ID or prefix match
/// - Text substring match against message content
async fn resolve_active_request(
    repo: &dyn TraceRepository,
    conversation: &ConversationKey,
    selector: &str,
) -> Option<ActiveRequestSummary> {
    let rows = repo.list_active_requests(conversation, 50).await.ok()?;

    let is_numeric = selector.chars().all(|c| c.is_ascii_digit()) && !selector.is_empty();

    // Try numeric index first (1-based)
    if is_numeric {
        if let Ok(idx) = selector.parse::<usize>()
            && idx >= 1
            && idx <= rows.len()
        {
            return Some(rows[idx - 1].clone());
        }
        // Short pure numbers are task indices only; longer numeric strings may
        // be valid UUID prefixes.
        if selector.len() < 4 {
            return None;
        }
    }

    // Try trace ID or request ID prefix match (only for non-numeric selectors)
    if let Some(row) = rows.iter().find(|r| {
        r.trace_id.as_str() == selector
            || r.trace_id.as_str().starts_with(selector)
            || r.request_id.as_str() == selector
            || r.request_id.as_str().starts_with(selector)
    }) {
        return Some(row.clone());
    }

    // Try text fuzzy match
    let lower = selector.to_lowercase();
    rows.into_iter()
        .find(|r| r.text_preview.to_lowercase().contains(&lower))
}

fn status_icon(status: &str) -> &'static str {
    match status {
        "running" => "\u{1f504}",
        "queued" => "\u{231b}",
        "completed" => "\u{2705}",
        "failed" => "\u{274c}",
        "reply_retrying" => "\u{1f4ec}",
        "reply_pending" => "\u{1f4e4}",
        _ => "\u{2753}",
    }
}

/// Format timestamp: strip date prefix if it's today.
fn short_timestamp(ts: &str) -> &str {
    // created_at is typically "YYYY-MM-DD HH:MM:SS.ffffff" or similar.
    // If it starts with today's date, strip to just time.
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    if let Some(rest) = ts.strip_prefix(&today) {
        rest.trim_start_matches([' ', 'T'])
    } else {
        ts
    }
}

fn truncate_text(text: &str, max_chars: usize) -> String {
    if text.chars().count() > max_chars {
        format!("{}…", text.chars().take(max_chars).collect::<String>())
    } else {
        text.to_string()
    }
}

fn format_trace_events(trace_id: &TraceId, events: Vec<GatewayEvent>) -> String {
    let mut lines = vec![format!("🧭 **Trace `{}`**", short_id(trace_id.as_str()))];
    for event in events {
        let payload = compact_event_payload(event.kind, &event.payload);
        lines.push(format!(
            "- #{} `{}` {} {}",
            event.sequence,
            event.kind.as_str(),
            event.created_at,
            payload
        ));
    }
    lines.join("\n")
}

fn compact_event_payload(kind: GatewayEventKind, payload: &serde_json::Value) -> String {
    match kind {
        GatewayEventKind::RequestQueued => payload["queue_depth"]
            .as_u64()
            .map(|depth| format!("depth={depth}"))
            .unwrap_or_default(),
        GatewayEventKind::RunStarted => payload["session_id"]
            .as_str()
            .map(|sid| format!("session={}", short_id(sid)))
            .unwrap_or_default(),
        GatewayEventKind::RunFailed
        | GatewayEventKind::RequestFailed
        | GatewayEventKind::OutboxFailed => payload["error"]
            .as_str()
            .unwrap_or("")
            .chars()
            .take(80)
            .collect(),
        GatewayEventKind::OutboxQueued => payload["outbox_id"]
            .as_str()
            .map(|id| format!("outbox={}", short_id(id)))
            .unwrap_or_default(),
        GatewayEventKind::OutboxSent => "sent".into(),
        _ => String::new(),
    }
}

fn short_id(id: &str) -> String {
    if id.len() <= 8 {
        id.to_string()
    } else {
        format!("{}…", &id[..8])
    }
}

// ─── Harness snapshot ───────────────────────────────────────────────────────

struct HarnessSnapshot {
    // Identity
    session_id: String,
    turn_number: u32,
    model: Option<String>,
    // Context
    context_utilization: Option<f32>,
    context_message_count: u32,
    context_total_tokens: Option<u32>,
    // Budget
    turns_used: u32,
    turns_limit: Option<u32>,
    tokens_used: u64,
    tokens_prompt: u64,
    tokens_completion: u64,
    tokens_cache_read: u64,
    elapsed_ms: u64,
    // Tools
    tool_calls: u32,
    unique_tools: Vec<String>,
    last_tool: Option<String>,
    consecutive_same_tool: u32,
    // Delegation
    #[allow(dead_code)]
    delegations: u32,
    // Errors
    consecutive_errors: u32,
}

impl HarnessSnapshot {
    fn turns_limit_str(&self) -> String {
        self.turns_limit
            .map(|l| l.to_string())
            .unwrap_or_else(|| "∞".into())
    }
    fn utilization_pct(&self) -> String {
        self.context_utilization
            .map(|u| format!("{:.0}%", u * 100.0))
            .unwrap_or_else(|| "—".into())
    }
    fn cost_estimate_usd(&self) -> f64 {
        // Rough estimate: $3/M input, $15/M output (Sonnet-class pricing)
        (self.tokens_prompt as f64 * 3.0 + self.tokens_completion as f64 * 15.0) / 1_000_000.0
    }
    fn tool_summary(&self) -> String {
        if self.unique_tools.is_empty() {
            return if self.tool_calls > 0 {
                "详情已脱敏".into()
            } else {
                "—".into()
            };
        }
        self.unique_tools
            .iter()
            .take(5)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ")
    }
    fn warnings(&self) -> Vec<String> {
        let mut w = Vec::new();
        if self.consecutive_same_tool > 2 {
            w.push(format!(
                "⚠️ 重复工具 {}次: {}",
                self.consecutive_same_tool,
                self.last_tool.as_deref().unwrap_or("详情已脱敏")
            ));
        }
        if let Some(u) = self.context_utilization
            && u > 0.85
        {
            w.push(format!("⚠️ Context 使用率 {:.0}%，接近上限", u * 100.0));
        }
        if self.consecutive_errors > 1 {
            w.push(format!("⚠️ 连续 {} 次错误", self.consecutive_errors));
        }
        w
    }

    fn format_full(&self) -> String {
        let mut lines = vec![
            format!(
                "🔭 **Harness — Session `{}`**",
                &self.session_id[..8.min(self.session_id.len())]
            ),
            String::new(),
            format!(
                "**状态** Turn {}/{} | {} | 🔧 {}",
                self.turns_used,
                self.turns_limit_str(),
                format_duration(self.elapsed_ms),
                self.tool_calls
            ),
            String::new(),
            format!(
                "**Token** ↓{} ↑{} 缓存↩{} | 总{}",
                format_tokens(self.tokens_prompt),
                format_tokens(self.tokens_completion),
                format_tokens(self.tokens_cache_read),
                format_tokens(self.tokens_used)
            ),
            format!(
                "**Context** {} msgs | {} | {}",
                self.context_message_count,
                self.utilization_pct(),
                self.context_total_tokens
                    .map(|t| format_tokens(t as u64))
                    .unwrap_or_else(|| "—".into())
            ),
            format!("**成本** ~${:.4}", self.cost_estimate_usd()),
        ];
        if let Some(ref model) = self.model {
            lines.push(format!("**模型** `{model}`"));
        }
        if self.tool_calls > 0 {
            lines.push(format!(
                "**工具** {} ({})",
                self.tool_calls,
                self.tool_summary()
            ));
        }
        let warnings = self.warnings();
        if !warnings.is_empty() {
            lines.push(String::new());
            for w in &warnings {
                lines.push(w.clone());
            }
        }
        lines.join("\n")
    }
}

fn format_audit_history(history: Vec<HarnessSnapshot>) -> String {
    let mut latest_by_turn = std::collections::BTreeMap::new();
    for snap in history {
        // The harness history endpoint returns newest-first snapshots.
        latest_by_turn.entry(snap.turn_number).or_insert(snap);
    }

    let turns: Vec<_> = latest_by_turn.into_values().rev().take(10).collect();
    let mut turns: Vec<_> = turns.into_iter().rev().collect();
    let mut lines = vec![format!("📋 **审计记录** (最近 {} 轮)", turns.len())];
    for snap in turns.drain(..) {
        lines.push(format!(
            "**Turn {}** ↓{} ↑{} | 🔧 {} ({}) | ctx:{} | ${:.4}",
            snap.turn_number,
            format_tokens(snap.tokens_prompt),
            format_tokens(snap.tokens_completion),
            snap.tool_calls,
            snap.tool_summary(),
            snap.utilization_pct(),
            snap.cost_estimate_usd(),
        ));
        for w in snap.warnings() {
            lines.push(format!("  {w}"));
        }
    }
    lines.join("\n")
}

async fn fetch_harness_snapshot(
    astra: &astra_thin_client::ThinClient,
    session_id: &str,
    api_key: &str,
) -> Option<HarnessSnapshot> {
    let path = format!("/sessions/{session_id}/harness/snapshot");
    let text = astra
        .get_bearer_path_query_text(api_key, &path, &[])
        .await
        .ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    Some(parse_harness_snapshot(&v, session_id))
}

async fn fetch_harness_history(
    astra: &astra_thin_client::ThinClient,
    session_id: &str,
    api_key: &str,
    n: usize,
) -> Vec<HarnessSnapshot> {
    let path = format!("/sessions/{session_id}/harness/history?n={n}");
    let text = match astra.get_bearer_path_query_text(api_key, &path, &[]).await {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    let v: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    v.as_array()
        .map(|arr| {
            arr.iter()
                .map(|s| parse_harness_snapshot(s, session_id))
                .collect()
        })
        .unwrap_or_default()
}

fn parse_harness_snapshot(v: &serde_json::Value, session_id: &str) -> HarnessSnapshot {
    HarnessSnapshot {
        session_id: v["session_id"].as_str().unwrap_or(session_id).to_string(),
        turn_number: v["turn_number"].as_u64().unwrap_or(0) as u32,
        model: v["model"].as_str().map(String::from),
        context_utilization: v["context_utilization"].as_f64().map(|u| u as f32),
        context_message_count: v["context_message_count"].as_u64().unwrap_or(0) as u32,
        context_total_tokens: v["context_total_tokens"].as_u64().map(|t| t as u32),
        turns_used: v["turns_used"].as_u64().unwrap_or(0) as u32,
        turns_limit: v["turns_limit"].as_u64().map(|l| l as u32),
        tokens_used: v["tokens_used_session"].as_u64().unwrap_or(0),
        tokens_prompt: v["tokens_prompt"].as_u64().unwrap_or(0),
        tokens_completion: v["tokens_completion"].as_u64().unwrap_or(0),
        tokens_cache_read: v["tokens_cache_read"].as_u64().unwrap_or(0),
        elapsed_ms: v["elapsed_millis"].as_u64().unwrap_or(0),
        tool_calls: v["tool_calls_this_session"].as_u64().unwrap_or(0) as u32,
        unique_tools: v["unique_tools_used"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|t| t.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default(),
        last_tool: v["last_tool_called"].as_str().map(String::from),
        consecutive_same_tool: v["consecutive_same_tool"].as_u64().unwrap_or(0) as u32,
        delegations: v["delegations_this_turn"].as_u64().unwrap_or(0) as u32,
        consecutive_errors: v["consecutive_errors"].as_u64().unwrap_or(0) as u32,
    }
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

fn format_duration(ms: u64) -> String {
    if ms >= 60_000 {
        format!("{}m {}s", ms / 60_000, (ms % 60_000) / 1000)
    } else if ms >= 1_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{ms}ms")
    }
}

fn model_shortcuts() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        (
            "haiku",
            "us.anthropic.claude-haiku-4-5-20251001-v1:0",
            "快/便宜",
        ),
        ("sonnet", "us.anthropic.claude-sonnet-4-6", "均衡"),
        ("opus", "us.anthropic.claude-opus-4-6-v1", "最强"),
        ("minimax", "MiniMax-M2.7", "MiniMax"),
        ("deepseek", "deepseek-v4-pro", "DeepSeek"),
        ("qwen", "qwen3.6-plus", "通义千问"),
        ("glm", "glm-5.1", "智谱 GLM"),
    ]
}

fn resolve_model_shortcut(input: &str) -> String {
    let lower = input.to_ascii_lowercase();
    for (shortcut, full, _) in model_shortcuts() {
        if lower == shortcut {
            return full.to_string();
        }
    }
    input.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cron_quoted() {
        let (expr, msg) = parse_cron_add("\"0 9 * * *\" 每天早上汇报").unwrap();
        assert_eq!(expr, "0 9 * * *");
        assert_eq!(msg, "每天早上汇报");
    }

    #[test]
    fn parse_cron_unquoted() {
        let (expr, msg) = parse_cron_add("0 9 * * 1-5 每个工作日早上汇报").unwrap();
        assert_eq!(expr, "0 9 * * 1-5");
        assert_eq!(msg, "每个工作日早上汇报");
    }

    #[test]
    fn format_tokens_values() {
        assert_eq!(format_tokens(500), "500");
        assert_eq!(format_tokens(1500), "1.5k");
        assert_eq!(format_tokens(1_500_000), "1.5M");
    }

    #[test]
    fn format_duration_values() {
        assert_eq!(format_duration(500), "500ms");
        assert_eq!(format_duration(3500), "3.5s");
        assert_eq!(format_duration(125_000), "2m 5s");
    }

    #[test]
    fn model_shortcut_resolves() {
        assert_eq!(
            resolve_model_shortcut("haiku"),
            "us.anthropic.claude-haiku-4-5-20251001-v1:0"
        );
        assert_eq!(
            resolve_model_shortcut("opus"),
            "us.anthropic.claude-opus-4-6-v1"
        );
        assert_eq!(resolve_model_shortcut("minimax"), "MiniMax-M2.7");
        assert_eq!(resolve_model_shortcut("deepseek"), "deepseek-v4-pro");
    }

    #[test]
    fn model_shortcut_passthrough() {
        assert_eq!(
            resolve_model_shortcut("some-custom-model"),
            "some-custom-model"
        );
    }

    #[test]
    fn model_shortcut_case_insensitive() {
        assert_eq!(
            resolve_model_shortcut("Haiku"),
            "us.anthropic.claude-haiku-4-5-20251001-v1:0"
        );
        assert_eq!(
            resolve_model_shortcut("OPUS"),
            "us.anthropic.claude-opus-4-6-v1"
        );
    }

    // ── Harness snapshot tests ──────────────────────────────────

    fn test_snapshot() -> HarnessSnapshot {
        HarnessSnapshot {
            session_id: "abc12345-def6-7890".into(),
            turn_number: 5,
            model: Some("claude-opus-4-6".into()),
            context_utilization: Some(0.42),
            context_message_count: 12,
            context_total_tokens: Some(50000),
            turns_used: 5,
            turns_limit: Some(20),
            tokens_used: 25000,
            tokens_prompt: 20000,
            tokens_completion: 5000,
            tokens_cache_read: 10000,
            elapsed_ms: 35000,
            tool_calls: 8,
            unique_tools: vec!["bash".into(), "read_file".into(), "edit_file".into()],
            last_tool: Some("edit_file".into()),
            consecutive_same_tool: 1,
            delegations: 0,
            consecutive_errors: 0,
        }
    }

    #[test]
    fn snapshot_format_full_contains_key_fields() {
        let s = test_snapshot();
        let full = s.format_full();
        assert!(full.contains("abc12345"), "session id");
        assert!(full.contains("5/20"), "turns");
        assert!(full.contains("🔧 8"), "tool calls");
        assert!(full.contains("42%"), "utilization");
        assert!(full.contains("claude-opus-4-6"), "model");
        assert!(full.contains("bash"), "tool name");
        assert!(full.contains("$"), "cost");
    }

    #[test]
    fn snapshot_warnings_consecutive_tool() {
        let mut s = test_snapshot();
        s.consecutive_same_tool = 5;
        s.last_tool = Some("bash".into());
        let w = s.warnings();
        assert!(!w.is_empty());
        assert!(w[0].contains("重复工具"));
        assert!(w[0].contains("bash"));
    }

    #[test]
    fn snapshot_warnings_high_utilization() {
        let mut s = test_snapshot();
        s.context_utilization = Some(0.92);
        let w = s.warnings();
        assert!(!w.is_empty());
        assert!(w[0].contains("接近上限"));
    }

    #[test]
    fn snapshot_warnings_consecutive_errors() {
        let mut s = test_snapshot();
        s.consecutive_errors = 3;
        let w = s.warnings();
        assert!(!w.is_empty());
        assert!(w[0].contains("连续"));
    }

    #[test]
    fn snapshot_no_warnings_when_healthy() {
        let s = test_snapshot();
        assert!(s.warnings().is_empty());
    }

    #[test]
    fn snapshot_cost_estimate() {
        let s = test_snapshot();
        let cost = s.cost_estimate_usd();
        // 20k * $3/M + 5k * $15/M = $0.06 + $0.075 = $0.135
        assert!(cost > 0.1 && cost < 0.2, "cost={cost}");
    }

    #[test]
    fn parse_snapshot_from_json() {
        let v = serde_json::json!({
            "session_id": "test-123",
            "turn_number": 3,
            "model": "opus",
            "turns_used": 3,
            "turns_limit": 10,
            "tokens_used_session": 15000,
            "tokens_prompt": 12000,
            "tokens_completion": 3000,
            "tokens_cache_read": 5000,
            "elapsed_millis": 8000,
            "tool_calls_this_session": 4,
            "unique_tools_used": ["bash", "read_file"],
            "last_tool_called": "read_file",
            "consecutive_same_tool": 0,
            "context_utilization": 0.35,
            "context_message_count": 8,
        });
        let snap = parse_harness_snapshot(&v, "fallback");
        assert_eq!(snap.session_id, "test-123");
        assert_eq!(snap.turn_number, 3);
        assert_eq!(snap.tokens_prompt, 12000);
        assert_eq!(snap.unique_tools.len(), 2);
    }

    #[test]
    fn sanitized_snapshot_tool_summary_does_not_look_empty() {
        let mut s = test_snapshot();
        s.tool_calls = 4;
        s.unique_tools.clear();
        s.last_tool = None;

        assert_eq!(s.tool_summary(), "详情已脱敏");
        let full = s.format_full();
        assert!(
            full.contains("🔧 4"),
            "tool call count should remain visible"
        );
        assert!(
            full.contains("详情已脱敏"),
            "sanitized details should be explicit"
        );
    }

    #[test]
    fn audit_history_dedupes_same_turn_and_sorts_chronologically() {
        let mut newest_turn_2 = test_snapshot();
        newest_turn_2.turn_number = 2;
        newest_turn_2.tokens_prompt = 2_000;
        newest_turn_2.tool_calls = 3;

        let mut older_turn_2 = test_snapshot();
        older_turn_2.turn_number = 2;
        older_turn_2.tokens_prompt = 1_000;
        older_turn_2.tool_calls = 1;

        let mut turn_1 = test_snapshot();
        turn_1.turn_number = 1;
        turn_1.tokens_prompt = 500;

        let audit = format_audit_history(vec![newest_turn_2, older_turn_2, turn_1]);

        assert_eq!(audit.matches("**Turn 2**").count(), 1);
        assert!(audit.find("**Turn 1**").unwrap() < audit.find("**Turn 2**").unwrap());
        assert!(
            audit.contains("↓2.0k"),
            "keeps latest snapshot for duplicate turn"
        );
        assert!(!audit.contains("↓1.0k"), "drops older duplicate snapshot");
    }

    // ── handle_command dispatch tests ──────────────────────────────

    fn test_config() -> GatewayConfig {
        GatewayConfig {
            astra: crate::config::AstraServerConfig {
                base_url: "http://localhost:8080".into(),
                api_key: String::new(),
                default_model: None,
                username: None,
                password: None,
            },
            storage: Default::default(),
            database: None,
            cli: Default::default(),
            cli_profiles: Default::default(),
            cli_timeout_secs: 3600,
            platforms: Default::default(),
            skills_dir: None,
            session_reset: Default::default(),
            access: Default::default(),
            action_policy: Default::default(),
            max_concurrent_runs: 4,
            group_sessions_per_user: true,
            group_require_mention: false,
            bot_name: String::new(),
            project_dirs: vec![],
        }
    }

    macro_rules! cmd_test {
        ($name:ident, $input:expr, $check:expr) => {
            #[tokio::test]
            async fn $name() {
                let config = test_config();
                let cli = crate::cli_bridge::CliProfile::default();
                let astra =
                    astra_thin_client::ThinClient::new("http://localhost:8080", None).unwrap();
                let ctx = CommandContext {
                    astra: &astra,
                    config: &config,
                    store: None,
                    platform: "test",
                    chat_id: "chat_1",
                    user_id: "user_1",
                    resolved_cli: &cli,
                    durable_store: None,
                    trace_repo: None,
                    project_dirs: &config.project_dirs,
                    cli_availability: &[],
                    auth_status: None,
                    active_tasks: None,
                };
                let result = handle_command(&ctx, $input).await;
                let check: fn(Option<String>) = $check;
                check(result);
            }
        };
    }

    cmd_test!(cmd_non_slash_returns_none, "hello world", |r| assert!(
        r.is_none()
    ));
    cmd_test!(cmd_unknown_returns_none, "/nonexistent", |r| assert!(
        r.is_none()
    ));
    cmd_test!(cmd_help_returns_command_list, "/help", |r| {
        let s = r.unwrap();
        assert!(s.contains("命令列表"));
        assert!(s.contains("/new"), "missing /new");
        assert!(s.contains("/reset"), "missing /reset alias");
        assert!(s.contains("/model"), "missing /model");
        assert!(s.contains("/session"), "missing /session");
        assert!(s.contains("/task"), "missing /task");
        assert!(s.contains("/trace"), "missing /trace");
        assert!(s.contains("/cancel"), "missing /cancel");
        assert!(s.contains("/ws"), "missing /ws alias");
    });
    cmd_test!(cmd_approve_returns_info, "/approve", |r| {
        assert!(r.unwrap().contains("工具权限"));
    });
    cmd_test!(cmd_model_no_arg_shows_current, "/model", |r| {
        let s = r.unwrap();
        assert!(s.contains("当前模型"));
        assert!(s.contains("快捷切换"));
        assert!(s.contains("haiku"));
        assert!(s.contains("opus"));
    });
    cmd_test!(
        cmd_model_set_without_db_still_succeeds,
        "/model opus",
        |r| {
            assert!(r.unwrap().contains("模型已切换"));
        }
    );
    cmd_test!(cmd_cli_no_arg_shows_current, "/cli", |r| {
        assert!(r.unwrap().contains("astra"));
    });
    cmd_test!(cmd_new_requires_db, "/new", |r| {
        {
            let msg = r.unwrap();
            assert!(msg.contains("存储"), "expected storage error, got: {msg}");
        }
    });
    cmd_test!(cmd_session_requires_db, "/session list", |r| {
        {
            let msg = r.unwrap();
            assert!(msg.contains("存储"), "expected storage error, got: {msg}");
        }
    });
    cmd_test!(cmd_cron_requires_db, "/cron list", |r| {
        {
            let msg = r.unwrap();
            assert!(msg.contains("存储"), "expected storage error, got: {msg}");
        }
    });
    cmd_test!(cmd_usage_requires_db, "/usage", |r| {
        {
            let msg = r.unwrap();
            assert!(msg.contains("存储"), "expected storage error, got: {msg}");
        }
    });
    cmd_test!(cmd_workspace_requires_db, "/workspace", |r| {
        {
            let msg = r.unwrap();
            assert!(msg.contains("存储"), "expected storage error, got: {msg}");
        }
    });
    cmd_test!(cmd_running_requires_trace_repo, "/running", |r| {
        {
            let msg = r.unwrap();
            assert!(
                msg.contains("存储") || msg.contains("MySQL"),
                "expected storage error, got: {msg}"
            );
        }
    });
    cmd_test!(cmd_task_requires_durable_store, "/task list", |r| {
        assert!(r.is_some());
    });
    cmd_test!(cmd_status_works_without_db, "/status", |r| {
        assert!(r.unwrap().contains("astra"));
    });

    #[tokio::test]
    async fn cmd_status_includes_auth_circuit_status_when_available() {
        let config = test_config();
        let cli = crate::cli_bridge::CliProfile::default();
        let astra = astra_thin_client::ThinClient::new("http://localhost:8080", None).unwrap();
        let ctx = CommandContext {
            astra: &astra,
            config: &config,
            store: None,
            platform: "test",
            chat_id: "chat_1",
            user_id: "user_1",
            resolved_cli: &cli,
            durable_store: None,
            trace_repo: None,
            project_dirs: &config.project_dirs,
            cli_availability: &[],
            auth_status: Some("⚠️ 暂停 (剩余 3m 42s)".to_string()),
            active_tasks: None,
        };

        let result = handle_command(&ctx, "/status").await.unwrap();
        assert!(result.contains("- 认证: ⚠️ 暂停 (剩余 3m 42s)"), "{result}");
    }
    cmd_test!(
        cmd_cron_add_malformed_gives_error,
        "/cron add badformat",
        |r| {
            let s = r.unwrap();
            assert!(s.contains("格式错误"), "should show format error, got: {s}");
        }
    );
    cmd_test!(cmd_inspect_requires_db, "/inspect", |r| {
        {
            let msg = r.unwrap();
            assert!(
                msg.contains("存储") || msg.contains("MySQL"),
                "expected storage error, got: {msg}"
            );
        }
    });
    cmd_test!(cmd_audit_requires_db, "/audit", |r| {
        {
            let msg = r.unwrap();
            assert!(
                msg.contains("存储") || msg.contains("MySQL"),
                "expected storage error, got: {msg}"
            );
        }
    });
    cmd_test!(cmd_trace_requires_trace_repo, "/trace", |r| {
        {
            let msg = r.unwrap();
            assert!(
                msg.contains("存储") || msg.contains("MySQL"),
                "expected storage error, got: {msg}"
            );
        }
    });
    cmd_test!(cmd_cancel_requires_trace_repo, "/cancel", |r| {
        {
            let msg = r.unwrap();
            assert!(
                msg.contains("存储") || msg.contains("MySQL"),
                "expected storage error, got: {msg}"
            );
        }
    });
    cmd_test!(cmd_kill_requires_trace_repo, "/kill abc123", |r| {
        {
            let msg = r.unwrap();
            assert!(
                msg.contains("存储") || msg.contains("MySQL"),
                "expected storage error, got: {msg}"
            );
        }
    });
    cmd_test!(cmd_kill_no_arg_auto_pick_or_no_db, "/kill", |r| {
        {
            let msg = r.unwrap();
            assert!(
                msg.contains("存储") || msg.contains("没有运行中"),
                "expected db error or no running message, got: {msg}"
            );
        }
    });
    cmd_test!(cmd_help_includes_kill, "/help", |r| {
        let s = r.unwrap();
        assert!(s.contains("/kill"), "help should include /kill");
    });
    cmd_test!(cmd_help_includes_gateway, "/help", |r| {
        let s = r.unwrap();
        assert!(s.contains("/gateway"), "help should include /gateway");
    });
    cmd_test!(cmd_help_includes_ws_ls, "/help", |r| {
        let s = r.unwrap();
        assert!(s.contains("/ws ls"), "help should include /ws ls");
    });
    cmd_test!(cmd_gateway_shows_context, "/gateway", |r| {
        let s = r.unwrap();
        assert!(s.contains("Gateway Context"), "missing header");
        assert!(s.contains("Identity"), "missing identity section");
        assert!(s.contains("Capabilities"), "missing capabilities section");
        assert!(s.contains("Commands"), "missing commands section");
        assert!(s.contains("astra"), "missing CLI name");
        assert!(s.contains("test"), "missing platform");
    });
    cmd_test!(cmd_ws_ls_empty_projects, "/ws ls", |r| {
        let s = r.unwrap();
        assert!(s.contains("没有发现项目"), "expected empty project message");
    });

    cmd_test!(cmd_retry_requires_trace_repo, "/retry", |r| {
        {
            let msg = r.unwrap();
            assert!(msg.contains("存储"), "expected storage error, got: {msg}");
        }
    });
    cmd_test!(
        cmd_retry_dismiss_requires_trace_repo,
        "/retry dismiss",
        |r| {
            {
                let msg = r.unwrap();
                assert!(msg.contains("存储"), "expected storage error, got: {msg}");
            }
        }
    );
    cmd_test!(cmd_help_includes_retry, "/help", |r| {
        let s = r.unwrap();
        assert!(s.contains("/retry"), "help should include /retry");
    });
    cmd_test!(cmd_gateway_includes_retry, "/gateway", |r| {
        let s = r.unwrap();
        assert!(s.contains("/retry"), "gateway should include /retry");
    });

    cmd_test!(cmd_ws_no_arg_requires_store, "/ws", |r| {
        let s = r.unwrap();
        assert!(s.contains("存储"), "expected storage error, got: {s}");
    });

    cmd_test!(
        cmd_ws_nonexistent_path,
        "/ws /nonexistent/path/12345",
        |r| {
            let s = r.unwrap();
            // Without store, /ws <arg> requires store first
            assert!(
                s.contains("存储") || s.contains("不存在"),
                "expected storage error or not-found, got: {s}"
            );
        }
    );

    // ── /manage is NOT a fast-path command (goes to slow path) ──

    cmd_test!(cmd_manage_not_handled_as_command, "/manage", |r| {
        assert!(r.is_none(), "/manage should not be a fast-path command");
    });
    cmd_test!(
        cmd_manage_with_arg_not_handled,
        "/manage 清理所有卡住的任务",
        |r| {
            assert!(
                r.is_none(),
                "/manage with arg should not be a fast-path command"
            );
        }
    );

    // ── /help and /gateway include /manage ──

    cmd_test!(cmd_help_includes_manage, "/help", |r| {
        let s = r.unwrap();
        assert!(s.contains("/manage"), "help should include /manage");
    });
    cmd_test!(cmd_gateway_includes_manage, "/gateway", |r| {
        let s = r.unwrap();
        assert!(s.contains("/manage"), "gateway should include /manage");
    });
    // Note: trace_kill and outbox_dismiss GATEWAY action tags are only visible
    // when model_generated_mutations is allowed AND store is available (tested below).

    // ── GAP 4: /gateway content completeness ────────────────────

    cmd_test!(cmd_gateway_content_completeness, "/gateway", |r| {
        let s = r.unwrap();
        assert!(s.contains("Gateway Context"), "missing header");
        assert!(s.contains("Identity"), "missing identity section");
        assert!(s.contains("Capabilities"), "missing capabilities section");
        assert!(s.contains("Commands"), "missing commands section");
        assert!(s.contains("astra"), "missing CLI name");
        assert!(s.contains("test"), "missing platform");
        // Verify capabilities matrix
        assert!(
            s.contains("Session management"),
            "missing session capability"
        );
        assert!(s.contains("Model switching"), "missing model capability");
        assert!(s.contains("Tool execution"), "missing tool capability");
        // Verify commands table
        assert!(s.contains("/new"), "missing /new in commands table");
        assert!(s.contains("/model"), "missing /model in commands table");
        assert!(s.contains("/kill"), "missing /kill in commands table");
        assert!(s.contains("/manage"), "missing /manage in commands table");
        assert!(s.contains("/retry"), "missing /retry in commands table");
        assert!(s.contains("/trace"), "missing /trace in commands table");
        assert!(s.contains("/usage"), "missing /usage in commands table");
        assert!(s.contains("/inspect"), "missing /inspect in commands table");
        assert!(s.contains("/audit"), "missing /audit in commands table");
        // Verify storage status (no store in test)
        assert!(
            s.contains("none"),
            "should show no storage when store is None"
        );
    });

    // ── GAP 8: resolve_active_request edge cases ────────────────

    #[tokio::test]
    async fn resolve_active_request_returns_none_for_empty_list() {
        use crate::trace_model::InMemoryTraceRepository;
        let repo = InMemoryTraceRepository::default();
        let conv = ConversationKey::new("wx", "c1", "astra");
        let result = resolve_active_request(&repo, &conv, "1").await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn resolve_active_request_numeric_zero_returns_none() {
        use crate::trace_model::InMemoryTraceRepository;
        let repo = InMemoryTraceRepository::default();
        let conv = ConversationKey::new("wx", "c1", "astra");
        let req = crate::trace_model::GatewayRequest::new(conv.clone(), "m1", "u1", "test");
        crate::trace_model::TraceWriter::begin(&repo, req)
            .await
            .unwrap();
        let result = resolve_active_request(&repo, &conv, "0").await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn resolve_active_request_out_of_range_returns_none() {
        use crate::trace_model::InMemoryTraceRepository;
        let repo = InMemoryTraceRepository::default();
        let conv = ConversationKey::new("wx", "c1", "astra");
        let req = crate::trace_model::GatewayRequest::new(conv.clone(), "m1", "u1", "test");
        crate::trace_model::TraceWriter::begin(&repo, req)
            .await
            .unwrap();
        // Only 1 request, asking for index 5
        let result = resolve_active_request(&repo, &conv, "5").await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn resolve_active_request_by_trace_id_prefix() {
        use crate::trace_model::InMemoryTraceRepository;
        let repo = InMemoryTraceRepository::default();
        let conv = ConversationKey::new("wx", "c1", "astra");
        let req = crate::trace_model::GatewayRequest::new(conv.clone(), "m1", "u1", "test");
        let trace_id = req.trace_id.clone();
        crate::trace_model::TraceWriter::begin(&repo, req)
            .await
            .unwrap();

        // Full trace ID
        let found = resolve_active_request(&repo, &conv, trace_id.as_str())
            .await
            .unwrap();
        assert_eq!(found.trace_id, trace_id);

        // Prefix match (first 8 chars)
        let prefix = &trace_id.as_str()[..8];
        let found = resolve_active_request(&repo, &conv, prefix).await.unwrap();
        assert_eq!(found.trace_id, trace_id);
    }

    #[tokio::test]
    async fn resolve_active_request_no_match_returns_none() {
        use crate::trace_model::InMemoryTraceRepository;
        let repo = InMemoryTraceRepository::default();
        let conv = ConversationKey::new("wx", "c1", "astra");
        let req =
            crate::trace_model::GatewayRequest::new(conv.clone(), "m1", "u1", "something specific");
        crate::trace_model::TraceWriter::begin(&repo, req)
            .await
            .unwrap();

        let result = resolve_active_request(&repo, &conv, "nonexistent_text").await;
        assert!(result.is_none());
    }

    // ── status_icon ──

    #[test]
    fn status_icon_maps_known_statuses() {
        assert_eq!(status_icon("running"), "\u{1f504}");
        assert_eq!(status_icon("queued"), "\u{231b}");
        assert_eq!(status_icon("completed"), "\u{2705}");
        assert_eq!(status_icon("failed"), "\u{274c}");
        assert_eq!(status_icon("reply_retrying"), "\u{1f4ec}");
        assert_eq!(status_icon("reply_pending"), "\u{1f4e4}");
        assert_eq!(status_icon("unknown"), "\u{2753}");
    }

    // ── short_timestamp ──

    #[test]
    fn short_timestamp_strips_today() {
        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let ts = format!("{today} 12:34:56.789");
        let short = short_timestamp(&ts);
        assert_eq!(short, "12:34:56.789");
    }

    #[test]
    fn short_timestamp_keeps_other_dates() {
        let ts = "2020-01-01 12:34:56";
        assert_eq!(short_timestamp(ts), ts);
    }

    // ── resolve_active_request ──

    #[tokio::test]
    async fn resolve_active_request_by_index() {
        use crate::trace_model::InMemoryTraceRepository;
        let repo = InMemoryTraceRepository::default();
        let conv = ConversationKey::new("test", "chat", "astra");

        // Create two requests
        let req1 = crate::trace_model::GatewayRequest::new(
            conv.clone(),
            "msg-1",
            "user-1",
            "first request",
        );
        let trace1 = req1.trace_id.clone();
        crate::trace_model::TraceWriter::begin(&repo, req1)
            .await
            .unwrap();

        let req2 = crate::trace_model::GatewayRequest::new(
            conv.clone(),
            "msg-2",
            "user-1",
            "second request",
        );
        let trace2 = req2.trace_id.clone();
        crate::trace_model::TraceWriter::begin(&repo, req2)
            .await
            .unwrap();

        // Index 1 → first (both are Accepted, sorted by status then order)
        let r1 = resolve_active_request(&repo, &conv, "1").await.unwrap();
        assert_eq!(r1.trace_id, trace1);

        // Index 2 → second
        let r2 = resolve_active_request(&repo, &conv, "2").await.unwrap();
        assert_eq!(r2.trace_id, trace2);

        // Index 0 → None (1-based)
        assert!(resolve_active_request(&repo, &conv, "0").await.is_none());

        // Index 3 → None (out of range; pure numbers don't fall through to ID match)
        assert!(resolve_active_request(&repo, &conv, "3").await.is_none());
    }

    #[tokio::test]
    async fn resolve_active_request_by_text() {
        use crate::trace_model::InMemoryTraceRepository;
        let repo = InMemoryTraceRepository::default();
        let conv = ConversationKey::new("test", "chat", "astra");

        let req = crate::trace_model::GatewayRequest::new(
            conv.clone(),
            "msg-1",
            "user-1",
            "帮我写个周报",
        );
        let trace = req.trace_id.clone();
        crate::trace_model::TraceWriter::begin(&repo, req)
            .await
            .unwrap();

        let found = resolve_active_request(&repo, &conv, "周报").await.unwrap();
        assert_eq!(found.trace_id, trace);

        // No match
        assert!(
            resolve_active_request(&repo, &conv, "不存在的内容")
                .await
                .is_none()
        );
    }

    #[test]
    fn parse_cron_add_rejects_invalid_cron_expression() {
        assert!(parse_cron_add("99 99 99 99 99 impossible").is_none());
        assert!(parse_cron_add("\"bad expr\" impossible").is_none());
    }
}
