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
    pub has_durable_tasks: bool,
    pub model_actions_allowed: bool,
    pub cron_jobs_count: usize,
    pub cron_jobs: Vec<(String, String, String)>, // (short_id, expr, description)
    pub active_tasks: Vec<(String, String, String, u8)>, // (short_id, name, status, progress)
    pub db_tables: Vec<String>,
    pub extra_skills: Vec<(String, String)>,
    pub current_workspace: Option<String>,
    pub available_projects: Vec<String>, // project summaries for prompt
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
            has_durable_tasks: has_db,
            model_actions_allowed: true,
            cron_jobs_count: 0,
            cron_jobs: Vec::new(),
            active_tasks: Vec::new(),
            db_tables: Vec::new(),
            extra_skills: Vec::new(),
            current_workspace: None,
            available_projects: Vec::new(),
        }
    }

    pub fn with_cron_count(mut self, count: usize) -> Self {
        self.cron_jobs_count = count;
        self
    }

    pub fn with_cron_jobs(mut self, jobs: Vec<(String, String, String)>) -> Self {
        self.cron_jobs_count = jobs.len();
        self.cron_jobs = jobs;
        self
    }

    pub fn with_active_tasks(mut self, tasks: Vec<(String, String, String, u8)>) -> Self {
        self.active_tasks = tasks;
        self
    }

    pub fn with_db_tables(mut self, tables: Vec<String>) -> Self {
        self.db_tables = tables;
        self
    }

    pub fn with_workspace(mut self, ws: Option<String>) -> Self {
        self.current_workspace = ws;
        self
    }

    pub fn with_projects(mut self, projects: Vec<String>) -> Self {
        self.available_projects = projects;
        self
    }

    pub fn with_extra_skills(mut self, skills: Vec<(String, String)>) -> Self {
        self.extra_skills = skills;
        self
    }

    pub fn with_model_actions_allowed(mut self, allowed: bool) -> Self {
        self.model_actions_allowed = allowed;
        self
    }

    pub fn to_system_prompt(&self) -> String {
        let mut prompt = render_template(SKILL_TEMPLATE, self);
        for (name, content) in &self.extra_skills {
            prompt.push_str(&format!("\n\n### Skill: {name}\n\n{content}"));
        }
        prompt
    }
}

/// Load skill markdown files from a directory. Returns (name, content) pairs sorted by name.
pub fn load_skills_from_dir(dir: &str) -> Vec<(String, String)> {
    let expanded = if dir.starts_with('~') {
        let home = std::env::var("HOME").unwrap_or_default();
        dir.replacen('~', &home, 1)
    } else {
        dir.to_string()
    };
    let path = std::path::Path::new(&expanded);
    if !path.is_dir() {
        return Vec::new();
    }
    let mut skills = Vec::new();
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) == Some("md")
                && let Ok(content) = std::fs::read_to_string(&p)
            {
                let name = p
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("unknown")
                    .to_string();
                skills.push((name, content));
            }
        }
    }
    skills.sort_by(|a, b| a.0.cmp(&b.0));
    skills
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
        if let Some(var) = trimmed
            .strip_prefix("{{#if ")
            .and_then(|s| s.strip_suffix("}}"))
        {
            if skip_depth > 0 || !check_condition(var, ctx) {
                skip_depth += 1;
            }
            i += 1;
            continue;
        }
        if let Some(var) = trimmed
            .strip_prefix("{{#each ")
            .and_then(|s| s.strip_suffix("}}"))
        {
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
            let items: Vec<String> = match var {
                "db_tables" => ctx.db_tables.clone(),
                "cron_jobs" => ctx
                    .cron_jobs
                    .iter()
                    .map(|(id, expr, desc)| format!("`{id}` | `{expr}` | {desc}"))
                    .collect(),
                "active_tasks" => ctx
                    .active_tasks
                    .iter()
                    .map(|(id, name, status, pct)| format!("`{id}` | {name} | {status} | {pct}%"))
                    .collect(),
                "available_projects" => ctx.available_projects.clone(),
                _ => Vec::new(),
            };
            for item in &items {
                for bl in &body_lines {
                    out.push_str(&bl.replace("{{this}}", item));
                    out.push('\n');
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
                "{{current_workspace}}",
                ctx.current_workspace.as_deref().unwrap_or("(default)"),
            )
            .replace("{{cron_jobs_count}}", &ctx.cron_jobs_count.to_string());
        out.push_str(&rendered);
        out.push('\n');
        i += 1;
    }

    out.trim().to_string()
}

fn check_condition(var: &str, ctx: &GatewayContext) -> bool {
    match var {
        "has_session" => ctx.has_session,
        "has_cron" => ctx.has_cron && ctx.model_actions_allowed,
        "has_harness" => ctx.has_harness,
        "has_durable_tasks" => ctx.has_durable_tasks && ctx.model_actions_allowed,
        "active_tasks" => !ctx.active_tasks.is_empty(),
        "available_projects" => !ctx.available_projects.is_empty(),
        "current_workspace" => ctx.current_workspace.is_some(),
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
        let cli = CliProfile::Claude {
            bin: "claude".into(),
            model: None,
            stream_json: false,
            extra_args: vec![],
        };
        let ctx = GatewayContext::new("u1", "Test", "weixin", &cli, true);
        let prompt = ctx.to_system_prompt();
        assert!(!prompt.contains("Harness Monitoring"));
    }

    #[test]
    fn template_with_cron_jobs() {
        let ctx = GatewayContext::new("u1", "Test", "weixin", &CliProfile::default(), true)
            .with_cron_jobs(vec![("abc123".into(), "0 9 * * *".into(), "早报".into())]);
        let prompt = ctx.to_system_prompt();
        assert!(prompt.contains("abc123"), "should show job ID");
        assert!(prompt.contains("早报"), "should show description");
        assert!(prompt.contains("1)"), "should show count");
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
        assert!(prompt.contains("You CAN set reminders"));
    }

    #[test]
    fn template_session_only_when_supported() {
        let codex = CliProfile::Codex {
            bin: "codex".into(),
            approval_mode: "full-auto".into(),
        };
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

#[test]
fn load_skills_empty_dir() {
    let dir = tempfile::tempdir().unwrap();
    let skills = load_skills_from_dir(dir.path().to_str().unwrap());
    assert!(skills.is_empty());
}

#[test]
fn load_skills_nonexistent_dir() {
    let skills = load_skills_from_dir("/nonexistent/path/12345");
    assert!(skills.is_empty());
}

#[test]
fn load_skills_from_dir_basic() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("report.md"),
        "# Weekly Report\nCollect stats.",
    )
    .unwrap();
    std::fs::write(dir.path().join("alert.md"), "# Alert Rules").unwrap();
    std::fs::write(dir.path().join("ignore.txt"), "not a skill").unwrap();

    let skills = load_skills_from_dir(dir.path().to_str().unwrap());
    assert_eq!(skills.len(), 2);
    assert_eq!(skills[0].0, "alert"); // sorted
    assert_eq!(skills[1].0, "report");
    assert!(skills[1].1.contains("Weekly Report"));
}

#[test]
fn extra_skills_appended_to_prompt() {
    let ctx = GatewayContext::new("u1", "Test", "weixin", &CliProfile::default(), false)
        .with_extra_skills(vec![("myskill".into(), "Do X then Y.".into())]);
    let prompt = ctx.to_system_prompt();
    assert!(prompt.contains("### Skill: myskill"));
    assert!(prompt.contains("Do X then Y."));
}

#[test]
fn template_includes_durable_tasks_when_db() {
    let ctx = GatewayContext::new("u1", "Test", "weixin", &CliProfile::default(), true);
    let prompt = ctx.to_system_prompt();
    assert!(prompt.contains("Durable Tasks"));
    assert!(prompt.contains("dtask_create"));
}

#[test]
fn template_excludes_durable_tasks_without_db() {
    let ctx = GatewayContext::new("u1", "Test", "weixin", &CliProfile::default(), false);
    let prompt = ctx.to_system_prompt();
    assert!(!prompt.contains("Durable Tasks"));
}

#[test]
fn template_hides_model_generated_actions_when_policy_disallows() {
    let ctx = GatewayContext::new("u1", "Test", "weixin", &CliProfile::default(), true)
        .with_model_actions_allowed(false);
    let prompt = ctx.to_system_prompt();
    assert!(!prompt.contains("GATEWAY:cron_add"));
    assert!(!prompt.contains("dtask_create"));
}

#[test]
fn active_tasks_in_prompt() {
    let ctx = GatewayContext::new("u1", "Test", "weixin", &CliProfile::default(), true)
        .with_active_tasks(vec![(
            "abc12345".into(),
            "weekly report".into(),
            "running".into(),
            50,
        )]);
    let prompt = ctx.to_system_prompt();
    assert!(prompt.contains("abc12345"), "should show task ID");
    assert!(prompt.contains("weekly report"), "should show task name");
    assert!(prompt.contains("50%"), "should show progress");
}

#[test]
fn no_active_tasks_no_section() {
    let ctx = GatewayContext::new("u1", "Test", "weixin", &CliProfile::default(), true);
    let prompt = ctx.to_system_prompt();
    assert!(!prompt.contains("Current durable tasks"));
}
