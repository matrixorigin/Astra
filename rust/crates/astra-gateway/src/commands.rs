//! Slash command handlers for the gateway.

use crate::config::GatewayConfig;
use crate::storage;
use sqlx::MySqlPool;

pub struct CommandContext<'a> {
    pub astra: &'a astra_thin_client::ThinClient,
    pub config: &'a GatewayConfig,
    pub pool: Option<&'a MySqlPool>,
    pub platform: &'a str,
    pub chat_id: &'a str,
    pub user_id: &'a str,
    pub resolved_cli: &'a crate::cli_bridge::CliProfile,
}

/// Helper: get pool or return error message for DB-dependent commands.
macro_rules! require_db {
    ($ctx:expr) => {
        match $ctx.pool {
            Some(p) => p,
            None => return Some("⚠️ 此命令需要数据库。当前以无数据库模式运行。".into()),
        }
    };
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
            let pool = require_db!(ctx);
            let cli_name = ctx.resolved_cli.name();
            let _ = storage::reset_session_for_cli(pool, ctx.platform, ctx.chat_id, cli_name).await;
            Some(format!("🔄 `{cli_name}` 会话已重置。发送新消息开始新对话。"))
        }

        "/status" => {
            let cli_name = ctx.resolved_cli.name();
            let session = storage::get_current_session_for_cli(require_db!(ctx), ctx.platform, ctx.chat_id, cli_name)
                .await
                .ok()
                .flatten();
            let cli_model = match ctx.resolved_cli {
                crate::cli_bridge::CliProfile::Astra { model, .. }
                | crate::cli_bridge::CliProfile::Claude { model, .. } => model.as_deref(),
                _ => None,
            };
            let model = cli_model
                .or(ctx.config.astra.default_model.as_deref())
                .unwrap_or("default");
            let mut lines = vec![
                "📊 **状态**".to_string(),
                format!("- CLI: `{}`", ctx.resolved_cli.name()),
                format!("- 模型: `{model}`"),
                format!("- 用户: `{}`", ctx.user_id),
                format!("- 会话: `{}`", session.as_deref().unwrap_or("(无)")),
            ];

            if let Some(ref sid) = session
                && let Some(snap) = fetch_harness_snapshot(ctx.astra, sid, &ctx.config.astra.api_key).await
            {
                    lines.push(String::new());
                    lines.push("**🔭 Harness**".into());
                    lines.push(format!("- 轮次: {}/{}", snap.turns_used, snap.turns_limit_str()));
                    lines.push(format!("- Token: {}", format_tokens(snap.tokens)));
                    lines.push(format!("- 工具: {}", snap.tool_calls));
                if snap.consecutive_same_tool > 1 {
                    lines.push(format!("- ⚠️ 重复工具: {}次", snap.consecutive_same_tool));
                }
            }
            Some(lines.join("\n"))
        }

        "/inspect" => {
            let cli_name = ctx.resolved_cli.name();
            let sid = match storage::get_current_session_for_cli(require_db!(ctx), ctx.platform, ctx.chat_id, cli_name).await {
                Ok(Some(s)) => s,
                _ => return Some("❌ 当前无活跃会话。".into()),
            };
            match fetch_harness_snapshot(ctx.astra, &sid, &ctx.config.astra.api_key).await {
                Some(snap) => Some(format!(
                    "🔭 **Harness 快照**\n\
                     - 轮次: {}/{}\n\
                     - Token: {}\n\
                     - 工具: {}\n\
                     - 利用率: {}\n\
                     - 消息: {}\n\
                     - 耗时: {}",
                    snap.turns_used, snap.turns_limit_str(),
                    format_tokens(snap.tokens), snap.tool_calls,
                    snap.utilization_str(), snap.message_count,
                    format_duration(snap.elapsed_ms),
                )),
                None => Some("⚠️ 暂无 harness 数据。".into()),
            }
        }

        "/session" => {
            let cli_name = ctx.resolved_cli.name();
            if arg.is_empty() || arg == "current" {
                let sid = storage::get_current_session_for_cli(require_db!(ctx), ctx.platform, ctx.chat_id, cli_name)
                    .await.ok().flatten();
                return Some(format!(
                    "📋 **当前会话** (CLI: `{cli_name}`)\n- ID: `{}`",
                    sid.as_deref().unwrap_or("(无)")
                ));
            }

            if arg == "list" {
                let sessions = storage::list_sessions_for_cli(require_db!(ctx), ctx.platform, ctx.chat_id, cli_name)
                    .await
                    .unwrap_or_default();
                if sessions.is_empty() {
                    return Some(format!("📋 `{cli_name}` 没有历史会话。"));
                }
                let mut lines = vec![format!("📋 **`{cli_name}` 会话列表**")];
                for (sid, current, created) in &sessions {
                    let marker = if *current { "→ " } else { "  " };
                    let short = &sid[..8.min(sid.len())];
                    lines.push(format!("{marker}`{short}…` ({created})"));
                }
                lines.push("\n使用 `/session switch <id>` 切换".into());
                return Some(lines.join("\n"));
            }

            if let Some(target) = arg.strip_prefix("switch ").or_else(|| arg.strip_prefix("sw ")) {
                let target = target.trim();
                match storage::switch_session(require_db!(ctx), ctx.platform, ctx.chat_id, target).await {
                    Ok(true) => Some(format!("✅ 已切换到会话 `{}`", &target[..8.min(target.len())])),
                    Ok(false) => Some(format!("❌ 找不到会话 `{target}`")),
                    Err(e) => Some(format!("⚠️ 切换失败: {e}")),
                }
            } else {
                Some("用法: `/session [list|switch <id>|current]`".into())
            }
        }

        "/cron" => {
            if arg.is_empty() || arg == "list" {
                let jobs = storage::list_cron_jobs(require_db!(ctx), ctx.platform, ctx.chat_id)
                    .await
                    .unwrap_or_default();
                if jobs.is_empty() {
                    return Some("⏰ 没有定时任务。用 `/cron add` 创建。".into());
                }
                let mut lines = vec!["⏰ **定时任务**".to_string()];
                for (id, expr, desc, enabled) in &jobs {
                    let status = if *enabled { "✅" } else { "⏸" };
                    let short_id = &id[..8.min(id.len())];
                    lines.push(format!("{status} `{short_id}` | `{expr}` | {desc}"));
                }
                lines.push("\n`/cron add <cron_expr> <消息>` — 创建\n`/cron del <id>` — 删除".into());
                return Some(lines.join("\n"));
            }

            if let Some(rest) = arg.strip_prefix("add ") {
                // Parse: /cron add "0 9 * * 1-5" 每天早上9点汇报
                let (cron_expr, message) = parse_cron_add(rest)?;
                let job_id = uuid::Uuid::new_v4().to_string();
                let pool = require_db!(ctx);
                match storage::create_cron_job(
                    pool, &job_id, ctx.platform, ctx.chat_id, ctx.user_id,
                    &cron_expr, &message, &message,
                ).await {
                    Ok(()) => Some(format!(
                        "✅ 定时任务已创建\n- ID: `{}`\n- 表达式: `{cron_expr}`\n- 任务: {message}",
                        &job_id[..8]
                    )),
                    Err(e) => Some(format!("⚠️ 创建失败: {e}")),
                }
            } else if let Some(id) = arg.strip_prefix("del ").or_else(|| arg.strip_prefix("rm ")) {
                match storage::delete_cron_job(require_db!(ctx), id.trim()).await {
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
            let target = resolve_model_shortcut(arg);
            if let Some(pool) = ctx.pool {
                let model_key = format!("model_override:{}", ctx.resolved_cli.name());
                let _ = storage::set_user_preference(pool, ctx.platform, ctx.user_id, &model_key, &target).await;
            }
            Some(format!("🤖 模型已切换: `{target}`\n(下次消息生效)"))
        }

        "/cli" => {
            if arg.is_empty() {
                // Show current CLI + available profiles
                let current = ctx.config.cli.name();
                let caps = ctx.config.cli.capabilities();
                let mut lines = vec![
                    format!("🔧 **当前 CLI: `{current}`**"),
                    format!(
                        "  能力: {}session {}model {}harness {}tools",
                        if caps.supports_session { "✅" } else { "❌" },
                        if caps.supports_model_switch { "✅" } else { "❌" },
                        if caps.supports_harness { "✅" } else { "❌" },
                        if caps.supports_tools { "✅" } else { "❌" },
                    ),
                ];
                if !ctx.config.cli_profiles.is_empty() {
                    lines.push("\n**可用 CLI:**".into());
                    for (name, profile) in &ctx.config.cli_profiles {
                        let c = profile.capabilities();
                        lines.push(format!(
                            "  `{name}` ({}{}{})",
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
                let caps = profile.capabilities();
                let cap_str = format!(
                    "session={} model={} harness={} tools={}",
                    if caps.supports_session { "✅" } else { "❌" },
                    if caps.supports_model_switch { "✅" } else { "❌" },
                    if caps.supports_harness { "✅" } else { "❌" },
                    if caps.supports_tools { "✅" } else { "❌" },
                );
                // Save preference to DB
                if let Some(pool) = ctx.pool {
                    let _ = storage::set_user_preference(
                        pool,
                        ctx.platform,
                    ctx.user_id,
                        "cli_profile",
                        arg,
                    )
                    .await;
                }
                Some(format!(
                    "✅ 已切换到 `{arg}` ({name})\n{cap_str}",
                    name = profile.name()
                ))
            } else {
                let available: Vec<&str> = ctx.config.cli_profiles.keys().map(|s| s.as_str()).collect();
                Some(format!(
                    "❌ 未找到 CLI `{arg}`\n可用: {}",
                    if available.is_empty() { "(无配置)".into() } else { available.join(", ") }
                ))
            }
        }

        "/approve" => Some(
            "🔐 **工具权限说明**\n\n\
             Gateway 模式下工具自动执行（`--auto-approve`）。\n\
             安全由 Harness 保障：\n\
             - 🛡 BudgetVerifier 限制轮次/token\n\
             - 🛡 TurnGuard 检测工具循环\n\
             - 🛡 CostVerifier 限制成本".into(),
        ),

        "/help" => Some(
            "💡 **命令列表**\n\n\
             **对话**\n\
             `/new` — 新建会话\n\
             `/session list` — 历史会话\n\
             `/session switch <id>` — 切换会话\n\n\
             **模型**\n\
             `/model` — 当前模型 + 快捷列表\n\
             `/model <name>` — 切换 (haiku/sonnet/opus/minimax/deepseek/qwen/glm)\n\n\
             **CLI**\n\
             `/cli` — 查看当前 CLI + 能力\n\
             `/cli <name>` — 切换 CLI (astra/claude)\n\n\
             **监控**\n\
             `/status` — 状态 + harness\n\
             `/inspect` — harness 详情\n\n\
             **定时任务**\n\
             `/cron list` — 查看任务\n\
             `/cron add <expr> <msg>` — 创建\n\
             `/cron del <id>` — 删除\n\n\
             **安全**\n\
             `/approve` — 权限说明".into(),
        ),

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
        if !expr.is_empty() && !msg.is_empty() {
            return Some((expr, msg));
        }
    }
    // Try unquoted: first 5 space-separated tokens are cron, rest is message
    let parts: Vec<&str> = input.splitn(6, ' ').collect();
    if parts.len() >= 6 {
        let expr = parts[..5].join(" ");
        let msg = parts[5].to_string();
        return Some((expr, msg));
    }
    None
}

// ─── Harness snapshot ───────────────────────────────────────────────────────

struct SnapshotSummary {
    turns_used: u32,
    turns_limit: Option<u32>,
    tokens: u64,
    tool_calls: u32,
    consecutive_same_tool: u32,
    utilization: Option<f32>,
    message_count: u32,
    elapsed_ms: u64,
}

impl SnapshotSummary {
    fn turns_limit_str(&self) -> String {
        self.turns_limit.map(|l| l.to_string()).unwrap_or_else(|| "∞".into())
    }
    fn utilization_str(&self) -> String {
        self.utilization.map(|u| format!("{:.0}%", u * 100.0)).unwrap_or_else(|| "—".into())
    }
}

async fn fetch_harness_snapshot(
    astra: &astra_thin_client::ThinClient,
    session_id: &str,
    api_key: &str,
) -> Option<SnapshotSummary> {
    let path = format!("/sessions/{session_id}/harness/snapshot");
    let text = astra.get_bearer_path_query_text(api_key, &path, &[]).await.ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    Some(SnapshotSummary {
        turns_used: v["turns_used"].as_u64().unwrap_or(0) as u32,
        turns_limit: v["turns_limit"].as_u64().map(|l| l as u32),
        tokens: v["tokens_used_session"].as_u64().unwrap_or(0),
        tool_calls: v["tool_calls_this_session"].as_u64().unwrap_or(0) as u32,
        consecutive_same_tool: v["consecutive_same_tool"].as_u64().unwrap_or(0) as u32,
        utilization: v["context_utilization"].as_f64().map(|u| u as f32),
        message_count: v["context_message_count"].as_u64().unwrap_or(0) as u32,
        elapsed_ms: v["elapsed_millis"].as_u64().unwrap_or(0),
    })
}

fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 { format!("{:.1}M", n as f64 / 1e6) }
    else if n >= 1_000 { format!("{:.1}k", n as f64 / 1e3) }
    else { format!("{n}") }
}

fn format_duration(ms: u64) -> String {
    if ms >= 60_000 { format!("{}m {}s", ms / 60_000, (ms % 60_000) / 1000) }
    else if ms >= 1_000 { format!("{:.1}s", ms as f64 / 1000.0) }
    else { format!("{ms}ms") }
}

fn model_shortcuts() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        ("haiku", "us.anthropic.claude-haiku-4-5-20251001-v1:0", "快/便宜"),
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
        assert_eq!(resolve_model_shortcut("haiku"), "us.anthropic.claude-haiku-4-5-20251001-v1:0");
        assert_eq!(resolve_model_shortcut("opus"), "us.anthropic.claude-opus-4-6-v1");
        assert_eq!(resolve_model_shortcut("minimax"), "MiniMax-M2.7");
        assert_eq!(resolve_model_shortcut("deepseek"), "deepseek-v4-pro");
    }

    #[test]
    fn model_shortcut_passthrough() {
        assert_eq!(resolve_model_shortcut("some-custom-model"), "some-custom-model");
    }

    #[test]
    fn model_shortcut_case_insensitive() {
        assert_eq!(resolve_model_shortcut("Haiku"), "us.anthropic.claude-haiku-4-5-20251001-v1:0");
        assert_eq!(resolve_model_shortcut("OPUS"), "us.anthropic.claude-opus-4-6-v1");
    }
}
