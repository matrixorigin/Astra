//! Gateway skill — template-based context injected into CLI agent prompts.
//!
//! The skill content lives in `skills/gateway.md` (compiled into the binary).
//! Dynamic variables are rendered at runtime per message.

use crate::cli_bridge::CliProfile;

const SKILL_TEMPLATE: &str = include_str!("../skills/gateway.md");

pub struct GatewayContext {
    pub user_id: String,
    pub user_display_name: String,
    pub platform: String,
    pub cli_name: String,
    pub model: Option<String>,
    pub has_db: bool,
    pub has_cron: bool,
    pub has_session: bool,
    pub has_harness: bool,
    pub cron_jobs_count: usize,
    pub db_tables: Vec<String>,
}

impl GatewayContext {
    pub fn new(
        user_id: &str,
        display_name: &str,
        platform: &str,
        cli: &CliProfile,
        has_db: bool,
    ) -> Self {
        let caps = cli.capabilities();
        let model = match cli {
            CliProfile::Astra { model, .. } | CliProfile::Claude { model, .. } => model.clone(),
            _ => None,
        };
        Self {
            user_id: user_id.to_string(),
            user_display_name: display_name.to_string(),
            platform: platform.to_string(),
            cli_name: cli.name().to_string(),
            model,
            has_db,
            has_cron: has_db,
            has_session: caps.supports_session,
            has_harness: caps.supports_harness,
            cron_jobs_count: 0,
            db_tables: Vec::new(),
        }
    }

    pub fn with_cron_count(mut self, count: usize) -> Self {
        self.cron_jobs_count = count;
        self
    }

    pub fn with_db_tables(mut self, tables: Vec<String>) -> Self {
        self.db_tables = tables;
        self
    }

    pub fn to_system_prompt(&self) -> String {
        render_template(SKILL_TEMPLATE, self)
    }
}

fn render_template(template: &str, ctx: &GatewayContext) -> String {
    let mut out = String::new();
    let lines: Vec<&str> = template.lines().collect();
    let mut i = 0;
    let mut skip_depth = 0;

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();

        // Handle {{#if var}} / {{#each var}} / {{/if}} / {{/each}}
        if let Some(var) = trimmed.strip_prefix("{{#if ").and_then(|s| s.strip_suffix("}}")) {
            if skip_depth > 0 || !check_condition(var, ctx) {
                skip_depth += 1;
            }
            i += 1;
            continue;
        }
        if let Some(var) = trimmed.strip_prefix("{{#each ").and_then(|s| s.strip_suffix("}}")) {
            if skip_depth > 0 {
                skip_depth += 1;
                i += 1;
                continue;
            }
            // Collect body until {{/each}}
            let mut body_lines = Vec::new();
            i += 1;
            while i < lines.len() {
                if lines[i].trim() == "{{/each}}" {
                    break;
                }
                body_lines.push(lines[i]);
                i += 1;
            }
            // Render for each item
            if var == "db_tables" {
                for table in &ctx.db_tables {
                    for bl in &body_lines {
                        out.push_str(&bl.replace("{{this}}", table));
                        out.push('\n');
                    }
                }
            }
            i += 1; // skip {{/each}}
            continue;
        }
        if trimmed == "{{/if}}" || trimmed == "{{/each}}" {
            if skip_depth > 0 {
                skip_depth -= 1;
            }
            i += 1;
            continue;
        }
        if skip_depth > 0 {
            i += 1;
            continue;
        }

        // Variable substitution
        let rendered = line
            .replace("{{platform}}", &ctx.platform)
            .replace("{{user_display_name}}", &ctx.user_display_name)
            .replace("{{user_id}}", &ctx.user_id)
            .replace("{{cli_name}}", &ctx.cli_name)
            .replace("{{model}}", ctx.model.as_deref().unwrap_or("auto"))
            .replace(
                "{{cron_jobs_count}}",
                &ctx.cron_jobs_count.to_string(),
            );
        out.push_str(&rendered);
        out.push('\n');
        i += 1;
    }

    out.trim().to_string()
}

fn check_condition(var: &str, ctx: &GatewayContext) -> bool {
    match var {
        "has_session" => ctx.has_session,
        "has_cron" => ctx.has_cron,
        "has_harness" => ctx.has_harness,
        "db_tables" => !ctx.db_tables.is_empty(),
        "cron_jobs_count" => ctx.cron_jobs_count > 0,
        "model" => ctx.model.is_some(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_renders_user_info() {
        let ctx = GatewayContext::new("wx_abc", "张三", "weixin", &CliProfile::default(), true);
        let prompt = ctx.to_system_prompt();
        assert!(prompt.contains("张三"), "missing display name");
        assert!(prompt.contains("wx_abc"), "missing user_id");
        assert!(prompt.contains("weixin"), "missing platform");
    }

    #[test]
    fn template_includes_cron_when_db() {
        let ctx = GatewayContext::new("u1", "Test", "weixin", &CliProfile::default(), true);
        let prompt = ctx.to_system_prompt();
        assert!(prompt.contains("GATEWAY:cron_add"), "missing cron action");
        assert!(prompt.contains("Gateway Actions"), "missing section header");
    }

    #[test]
    fn template_excludes_cron_without_db() {
        let ctx = GatewayContext::new("u1", "Test", "weixin", &CliProfile::default(), false);
        let prompt = ctx.to_system_prompt();
        assert!(!prompt.contains("Gateway Actions"));
        assert!(!prompt.contains("GATEWAY:cron_add"));
    }

    #[test]
    fn template_includes_harness_for_astra() {
        let ctx = GatewayContext::new("u1", "Test", "weixin", &CliProfile::default(), true);
        let prompt = ctx.to_system_prompt();
        assert!(prompt.contains("Harness Monitoring"));
    }

    #[test]
    fn template_excludes_harness_for_claude() {
        let cli = CliProfile::Claude { bin: "claude".into(), model: None };
        let ctx = GatewayContext::new("u1", "Test", "weixin", &cli, true);
        let prompt = ctx.to_system_prompt();
        assert!(!prompt.contains("Harness Monitoring"));
    }

    #[test]
    fn template_with_cron_count() {
        let ctx = GatewayContext::new("u1", "Test", "weixin", &CliProfile::default(), true)
            .with_cron_count(3);
        let prompt = ctx.to_system_prompt();
        assert!(prompt.contains("3 scheduled task(s) active"));
    }

    #[test]
    fn template_with_db_tables() {
        let ctx = GatewayContext::new("u1", "Test", "weixin", &CliProfile::default(), true)
            .with_db_tables(vec!["gw_users".into(), "gw_sessions".into()]);
        let prompt = ctx.to_system_prompt();
        assert!(prompt.contains("gw_users"));
        assert!(prompt.contains("gw_sessions"));
    }

    #[test]
    fn template_response_guidelines() {
        let ctx = GatewayContext::new("u1", "Test", "weixin", &CliProfile::default(), true);
        let prompt = ctx.to_system_prompt();
        assert!(prompt.contains("Do NOT say you can't set reminders"));
    }

    #[test]
    fn template_session_only_when_supported() {
        let codex = CliProfile::Codex { bin: "codex".into(), approval_mode: "full-auto".into() };
        let ctx = GatewayContext::new("u1", "Test", "weixin", &codex, true);
        let prompt = ctx.to_system_prompt();
        assert!(!prompt.contains("/session list"));
    }

    #[test]
    fn template_shows_model() {
        let cli = CliProfile::Astra {
            bin: "astra".into(),
            model: Some("MiniMax-M2.7".into()),
            permission_mode: "auto".into(),
        };
        let ctx = GatewayContext::new("u1", "Test", "weixin", &cli, true);
        let prompt = ctx.to_system_prompt();
        assert!(prompt.contains("MiniMax-M2.7"));
    }

    #[test]
    fn render_simple_template() {
        let ctx = GatewayContext::new("u1", "U", "wx", &CliProfile::default(), false);
        let tpl = "Hello {{user_display_name}} on {{platform}}";
        let out = render_template(tpl, &ctx);
        assert_eq!(out, "Hello U on wx");
    }

    #[test]
    fn render_conditional_block() {
        let ctx = GatewayContext::new("u1", "U", "wx", &CliProfile::default(), true);
        let tpl = "before\n{{#if has_cron}}\ncron section\n{{/if}}\nafter";
        let out = render_template(tpl, &ctx);
        assert!(out.contains("cron section"));
        assert!(out.contains("before"));
        assert!(out.contains("after"));
    }

    #[test]
    fn render_false_conditional_excluded() {
        let ctx = GatewayContext::new("u1", "U", "wx", &CliProfile::default(), false);
        let tpl = "before\n{{#if has_cron}}\nhidden\n{{/if}}\nafter";
        let out = render_template(tpl, &ctx);
        assert!(!out.contains("hidden"));
    }
}
