use super::*;

fn default_skill_category(category: Option<&str>) -> String {
    category
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("general")
        .to_string()
}

// ── Catalog surfacing (SkillSearchSettings → agent context; was /skill-search) ──

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SkillSurfacingCmd {
    Show,
    Reset,
    SetDynamic(bool),
    SetMinCatalog(usize),
    SetSurfaceCap(usize),
}

fn format_skill_surfacing_line(settings: &astra_core::SkillSearchSettings) -> String {
    format!(
        "dynamic={}, min_catalog_size={}, surface_cap={}",
        settings.dynamic_surface, settings.min_catalog_size, settings.surface_cap
    )
}

fn parse_skill_surfacing(arg: &str) -> Result<SkillSurfacingCmd, String> {
    let arg = arg.trim();
    if arg.is_empty() || matches!(arg, "show" | "status") {
        return Ok(SkillSurfacingCmd::Show);
    }
    if arg == "reset" {
        return Ok(SkillSurfacingCmd::Reset);
    }

    let mut parts = arg.split_whitespace();
    let key = parts.next().unwrap_or_default();
    let value = parts.next().unwrap_or_default();
    if parts.next().is_some() {
        return Err(
            "Usage: /skill surfacing [show|status|reset|dynamic <on|off>|min <n>|cap <n>]"
                .to_string(),
        );
    }

    match key {
        "dynamic" => match value.to_ascii_lowercase().as_str() {
            "on" | "true" | "1" => Ok(SkillSurfacingCmd::SetDynamic(true)),
            "off" | "false" | "0" => Ok(SkillSurfacingCmd::SetDynamic(false)),
            _ => Err("Usage: /skill surfacing dynamic <on|off>".to_string()),
        },
        "min" => value
            .parse::<usize>()
            .ok()
            .filter(|n| *n > 0)
            .map(SkillSurfacingCmd::SetMinCatalog)
            .ok_or_else(|| "Usage: /skill surfacing min <positive-integer>".to_string()),
        "cap" => value
            .parse::<usize>()
            .ok()
            .filter(|n| *n > 0)
            .map(SkillSurfacingCmd::SetSurfaceCap)
            .ok_or_else(|| "Usage: /skill surfacing cap <positive-integer>".to_string()),
        _ => Err(
            "Usage: /skill surfacing [show|status|reset|dynamic <on|off>|min <n>|cap <n>]"
                .to_string(),
        ),
    }
}

fn apply_skill_surfacing(state: &mut ReplState, command: SkillSurfacingCmd) -> (String, bool) {
    match command {
        SkillSurfacingCmd::Show => (
            format!(
                "Catalog surfacing: {}",
                format_skill_surfacing_line(&state.skill_search)
            ),
            false,
        ),
        SkillSurfacingCmd::Reset => {
            state.skill_search = astra_core::SkillSearchSettings::default();
            (
                format!(
                    "Catalog surfacing reset: {}",
                    format_skill_surfacing_line(&state.skill_search)
                ),
                true,
            )
        }
        SkillSurfacingCmd::SetDynamic(dynamic) => {
            state.skill_search.dynamic_surface = dynamic;
            (
                format!(
                    "Catalog surfacing updated: {}",
                    format_skill_surfacing_line(&state.skill_search)
                ),
                true,
            )
        }
        SkillSurfacingCmd::SetMinCatalog(min_catalog_size) => {
            state.skill_search.min_catalog_size = min_catalog_size;
            (
                format!(
                    "Catalog surfacing updated: {}",
                    format_skill_surfacing_line(&state.skill_search)
                ),
                true,
            )
        }
        SkillSurfacingCmd::SetSurfaceCap(surface_cap) => {
            state.skill_search.surface_cap = surface_cap;
            (
                format!(
                    "Catalog surfacing updated: {}",
                    format_skill_surfacing_line(&state.skill_search)
                ),
                true,
            )
        }
    }
}

pub(super) async fn handle_skill_command(
    arg: &str,
    api: &astra_thin_client::ThinClient,
    state: &mut ReplState,
    profile: Option<&str>,
    token: Option<&str>,
) -> Result<(), String> {
    // Parse subcommand and remaining args from `arg`
    let mut sub_parts = arg.splitn(2, ' ');
    let sub = sub_parts.next().unwrap_or("").trim();
    let sub_arg = sub_parts.next().unwrap_or("").trim();

    // Route based on subcommand
    match sub {
        "" => {
            // Overview + subcommand navigation (like /session)
            let registry = &state.unified_skill_registry;
            let all = registry.all_manifests();
            let local = all
                .iter()
                .filter(|m| m.source == astra_skills::SkillSourceKind::Local)
                .count();
            let bundled = all
                .iter()
                .filter(|m| m.source == astra_skills::SkillSourceKind::Bundled)
                .count();
            let mcp = all
                .iter()
                .filter(|m| m.source == astra_skills::SkillSourceKind::Mcp)
                .count();

            eprintln!(
                "\n{}",
                "─── Skills ──────────────────────────────────────"
                    .bold()
                    .cyan()
            );
            eprintln!("  {:<16} {}", "total:".dim(), all.len().to_string().cyan());
            eprintln!("  {:<16} {}", "local:".dim(), local.to_string().cyan());
            eprintln!("  {:<16} {}", "bundled:".dim(), bundled.to_string().cyan());
            if mcp > 0 {
                eprintln!("  {:<16} {}", "mcp:".dim(), mcp.to_string().cyan());
            }
            if let Some(ref dev) = state.skill_dev {
                eprintln!(
                    "  {:<16} {} {}",
                    "dev mode:".dim(),
                    dev.name.as_str().cyan(),
                    "(use /skill dev off to exit)".dim()
                );
            }
            eprintln!();
            eprintln!("  {}", "Subcommands:".dim());
            eprintln!(
                "    {}  {}",
                "/skill list [filter]".cyan(),
                "List all skills".dim()
            );
            eprintln!(
                "    {}  {}",
                "/skill info <name>".cyan(),
                "Show skill details".dim()
            );
            eprintln!(
                "    {}  {}",
                "/skill search <query>".cyan(),
                "Keyword match (name, tags, description, …)".dim()
            );
            eprintln!(
                "    {}  {}",
                "/skill surfacing …".cyan(),
                "Agent catalog: dynamic/min/cap (discover_skills path)".dim()
            );
            eprintln!(
                "    {}  {}",
                "/skill new <name>".cyan(),
                "Scaffold a new skill".dim()
            );
            eprintln!(
                "    {}  {}",
                "/skill create".cyan(),
                "Auto-generate from session".dim()
            );
            eprintln!(
                "    {}  {}",
                "/skill dev <name|off>".cyan(),
                "Enter/exit dev mode".dim()
            );
            eprintln!(
                "    {}  {}",
                "/skill test <name>".cyan(),
                "API test or local manifest check (+ hooks)".dim()
            );
            eprintln!(
                "    {}  {}",
                "/skill health".cyan(),
                "Skill catalog health (registry + disk)".dim()
            );
            eprintln!(
                "    {}  {}",
                "/skill stats".cyan(),
                "Quality tracker stats".dim()
            );
            eprintln!(
                "    {}  {}",
                "/skill feedback <name> +/-".cyan(),
                "Record user feedback".dim()
            );
            eprintln!(
                "    {}  {}",
                "/skill pin/unpin <name>".cyan(),
                "Pin/unpin for priority".dim()
            );
            eprintln!(
                "    {}  {}",
                "/skill info <name> --raw".cyan(),
                "Print YAML frontmatter (on-disk)".dim()
            );
            eprintln!();
            eprintln!("  {}", "Marketplace:".dim());
            eprintln!(
                "    {}  {}",
                "/skill browse [query]".cyan(),
                "Browse marketplace".dim()
            );
            eprintln!(
                "    {}  {}",
                "/skill install <name>".cyan(),
                "Install from marketplace".dim()
            );
            eprintln!(
                "    {}  {}",
                "/skill publish <name>".cyan(),
                "Publish to marketplace".dim()
            );
            eprintln!(
                "    {}  {}",
                "/skill upgrade <name>".cyan(),
                "Upgrade installed skill".dim()
            );
            eprintln!();
            eprintln!("  {}", "Evolution:".dim());
            eprintln!(
                "    {}  {}",
                "/skill evolve".cyan(),
                "Show evolution status (signals, pending proposals, canaries)".dim()
            );
            eprintln!(
                "    {}  {}",
                "/skill evolve approve <id>".cyan(),
                "Approve a pending proposal or promote an active canary".dim()
            );
            eprintln!(
                "    {}  {}",
                "/skill evolve reject <id>".cyan(),
                "Reject a pending proposal or roll back an active canary".dim()
            );
            eprintln!();
        }

        "list" => {
            // Show skills from the unified registry (local + bundled + MCP)
            let registry = &state.unified_skill_registry;
            let all_manifests = registry.all_manifests();

            // Parse filter flags from sub_arg: free text search and --source=X, --category=X
            let (search_query, source_filter, category_filter) = parse_list_filters(sub_arg);

            let manifests: Vec<_> = all_manifests
                .into_iter()
                .filter(|m| {
                    matches_skill_filter(m, &search_query, &source_filter, &category_filter)
                })
                .collect();

            // Show active filter if any
            if !sub_arg.is_empty() {
                eprintln!("\n  {} {}", "Filter:".dim(), sub_arg.yellow());
            }

            eprintln!(
                "\n{}",
                format!(
                    "{:<28}  {:<10}  {:<8}  {}",
                    "Name", "Version", "Source", "Description"
                )
                .bold()
            );
            eprintln!("{}", "\u{2500}".repeat(78).dim());

            if manifests.is_empty() {
                if sub_arg.is_empty() {
                    eprintln!("  {}", "(no skills discovered)".dim());
                } else {
                    eprintln!("  {}", format!("No skills matching '{sub_arg}'").dim());
                }
            } else {
                for m in &manifests {
                    let source = source_label(&m.source);
                    let desc = truncate_desc(&m.description, 36);
                    eprintln!(
                        "  {:<26}  {:<10}  {:<8}  {}",
                        m.name.as_str().cyan(),
                        m.version.to_string().dim(),
                        source.dim(),
                        desc
                    );
                }
            }
            let local_count = manifests
                .iter()
                .filter(|m| m.source == astra_skills::SkillSourceKind::Local)
                .count();
            let bundled_count = manifests
                .iter()
                .filter(|m| m.source == astra_skills::SkillSourceKind::Bundled)
                .count();
            let mcp_count = manifests
                .iter()
                .filter(|m| m.source == astra_skills::SkillSourceKind::Mcp)
                .count();
            let mut parts = vec![
                format!("{} local", local_count),
                format!("{} bundled", bundled_count),
            ];
            if mcp_count > 0 {
                parts.push(format!("{} mcp", mcp_count));
            }
            parts.push(format!("{} total", manifests.len()));
            eprintln!(
                "\n  {}",
                parts
                    .iter()
                    .enumerate()
                    .map(|(i, p)| {
                        if i == parts.len() - 1 {
                            format!("{}", p.as_str().bold())
                        } else {
                            p.as_str().dim().to_string()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            eprintln!();
        }

        "search" => {
            let query = sub_arg.trim();
            if query.is_empty() {
                eprintln!("{}", "  Usage: /skill search <query>".yellow());
                eprintln!("{}", "  Example: /skill search code review".dim());
                return Ok(());
            }
            let registry = &state.unified_skill_registry;
            let all = registry.all_manifests();
            let query_lower = query.to_lowercase();

            let mut scored: Vec<_> = all
                .iter()
                .filter_map(|m| {
                    let score = skill_relevance_score(m, &query_lower);
                    if score > 0 { Some((m, score)) } else { None }
                })
                .collect();
            scored.sort_by_key(|x| std::cmp::Reverse(x.1));

            eprintln!(
                "\n  {} '{}' {}",
                "Keyword matches for".dim(),
                query.cyan(),
                "(not vector search)".dim()
            );
            eprintln!("{}", "\u{2500}".repeat(78).dim());

            if scored.is_empty() {
                eprintln!("  {}", "No matching skills found.".dim());
            } else {
                for (m, score) in scored.iter().take(10) {
                    let source = source_label(&m.source);
                    let desc = truncate_desc(&m.description, 50);
                    let relevance = match score {
                        s if *s >= 10 => "★★★",
                        s if *s >= 5 => "★★ ",
                        _ => "★  ",
                    };
                    eprintln!(
                        "  {} {:<24}  {:<8}  {}",
                        relevance.yellow(),
                        m.name.as_str().cyan(),
                        source.dim(),
                        desc
                    );
                    // Show matched fields
                    let mut matched = Vec::new();
                    if m.name.to_lowercase().contains(&query_lower) {
                        matched.push("name");
                    }
                    if m.description.to_lowercase().contains(&query_lower) {
                        matched.push("description");
                    }
                    if m.tags
                        .iter()
                        .any(|t| t.to_lowercase().contains(&query_lower))
                    {
                        matched.push("tags");
                    }
                    if m.triggers
                        .iter()
                        .any(|t| t.to_lowercase().contains(&query_lower))
                    {
                        matched.push("triggers");
                    }
                    if m.when_to_use
                        .as_ref()
                        .map(|w| w.to_lowercase().contains(&query_lower))
                        .unwrap_or(false)
                    {
                        matched.push("when_to_use");
                    }
                    if !matched.is_empty() {
                        eprintln!(
                            "        {}",
                            format!("matched: {}", matched.join(", ")).dim()
                        );
                    }
                }
                eprintln!(
                    "\n  {} results {}",
                    scored.len().to_string().cyan(),
                    "(showing top 10)".dim()
                );
            }
            eprintln!();
        }

        "surfacing" => {
            let command = match parse_skill_surfacing(sub_arg) {
                Ok(command) => command,
                Err(message) => {
                    eprintln!("  {}", message.yellow());
                    return Ok(());
                }
            };
            let (message, changed) = apply_skill_surfacing(state, command);
            eprintln!("  {}", message.green());
            if changed && let Some(ref j) = state.journal {
                let _ = j.append(
                    &astra_services::session_journal::JournalEvent::config_change(
                        state.session_id.as_deref(),
                        "skill_search",
                        &format_skill_surfacing_line(&state.skill_search),
                    ),
                );
            }
        }

        "info" => {
            let (name, raw_config) = parse_skill_info_args(sub_arg);
            if name.is_empty() {
                eprintln!("{}", "  Usage: /skill info <name> [--raw]".yellow());
                return Ok(());
            }
            let registry = &state.unified_skill_registry;
            if raw_config {
                match registry.get_manifest(name.as_str()) {
                    None => {
                        eprintln!(
                            "  {}",
                            format!("✗ Skill '{name}' not found in catalog").yellow()
                        );
                    }
                    Some(_) => match resolve_skill_dir_on_disk(name.as_str()) {
                        Some(ref dir) => {
                            if let Err(e) = print_skill_directory_raw(name.as_str(), dir) {
                                eprintln!("  {}", e.red());
                            }
                        }
                        None => {
                            eprintln!(
                                "  {}",
                                format!("No on-disk SKILL.md for '{name}' (e.g. MCP-only).")
                                    .yellow()
                            );
                        }
                    },
                }
                eprintln!();
                return Ok(());
            }
            match registry.get_manifest(name.as_str()) {
                None => {
                    // Suggest similar skill names using fuzzy matching
                    let all = registry.skill_names();
                    let suggestions = cli_output::suggest_skills(&name, &all);
                    let refs: Vec<&str> = suggestions.iter().map(|s| s.as_str()).collect();
                    cli_output::format_not_found_error(
                        "Skill",
                        &name,
                        &refs,
                        Some("/skill list to see available skills"),
                    );
                }
                Some(m) => {
                    eprintln!("\n{}", format!("── {} ──", m.name).bold().cyan());
                    eprintln!("  {:<16} {}", "Description:".dim(), m.description);
                    eprintln!("  {:<16} {}", "Version:".dim(), m.version);
                    eprintln!("  {:<16} {}", "Source:".dim(), source_label(&m.source));
                    eprintln!(
                        "  {:<16} {}",
                        "Context:".dim(),
                        format!("{:?}", m.execution_context).to_lowercase()
                    );
                    if let Some(ref author) = m.author {
                        eprintln!("  {:<16} {}", "Author:".dim(), author);
                    }
                    if let Some(ref model) = m.model {
                        eprintln!("  {:<16} {}", "Model:".dim(), model);
                    }
                    if let Some(max_tok) = m.max_tokens {
                        eprintln!("  {:<16} {}", "Max tokens:".dim(), max_tok);
                    }
                    if let Some(ref cat) = m.category {
                        eprintln!("  {:<16} {}", "Category:".dim(), cat);
                    }
                    if !m.tags.is_empty() {
                        eprintln!("  {:<16} {}", "Tags:".dim(), m.tags.join(", "));
                    }
                    if !m.triggers.is_empty() {
                        eprintln!("  {:<16} {}", "Triggers:".dim(), m.triggers.join(", "));
                    }
                    if !m.allowed_tools.is_empty() {
                        eprintln!(
                            "  {:<16} {}",
                            "Allowed tools:".dim(),
                            m.allowed_tools.join(", ")
                        );
                    }
                    if !m.paths.is_empty() {
                        eprintln!("  {:<16} {}", "Path patterns:".dim(), m.paths.join(", "));
                    }
                    if !m.arguments.is_empty() {
                        eprintln!("  {:<16}", "Arguments:".dim());
                        for arg in &m.arguments {
                            let required = if arg.required { " (required)" } else { "" };
                            eprintln!(
                                "    {} {}{}",
                                arg.name.as_str().cyan(),
                                arg.description.as_str().dim(),
                                required.yellow()
                            );
                        }
                    }
                    if let Some(ref wtu) = m.when_to_use {
                        eprintln!("  {:<16} {}", "When to use:".dim(), wtu);
                    }
                    if !m.user_invocable {
                        eprintln!("  {:<16} {}", "Invocable:".dim(), "no (auto-only)".yellow());
                    }

                    // Show instruction preview if loaded
                    if let Some(loaded) = registry.get_loaded_skill(name.as_str()) {
                        eprintln!(
                            "\n  {} ({} tokens)",
                            "Instructions:".dim(),
                            loaded.instruction_tokens
                        );
                        let preview: String = loaded.instructions.chars().take(500).collect();
                        for line in preview.lines().take(15) {
                            eprintln!("    {}", line.dim());
                        }
                        if loaded.instructions.len() > 500 {
                            eprintln!("    {}", "… (truncated)".dim());
                        }
                        if let Some(ref dir) = loaded.skill_dir {
                            eprintln!("\n  {:<16} {}", "Directory:".dim(), dir.display());
                        }
                    }
                    eprintln!();
                }
            }
        }

        "new" => {
            let name = sub_arg;
            if name.is_empty() {
                eprintln!("{}", "  Usage: /skill new <name>".yellow());
                return Ok(());
            }
            let skills_base = std::env::current_dir()
                .map_err(|e| e.to_string())?
                .join(".astra/skills");
            let skill_dir = skills_base.join(name);
            if skill_dir.exists() {
                eprintln!(
                    "{}",
                    format!(
                        "  \u{2717} Skill directory already exists: {}",
                        skill_dir.display()
                    )
                    .yellow()
                );
                return Ok(());
            }
            std::fs::create_dir_all(&skill_dir).map_err(|e| e.to_string())?;

            let skill_md = format!(
                r#"---
name: {name}
description: ""
version: "0.1.0"
user_invocable: true
triggers:
  - {name}
allowed_tools: []
when_to_use: ""
# model: "claude-sonnet-4-20250514"
# max_tokens: 8192
# execution_context: inline
# paths:
#   - "src/**/*.rs"
# hooks:
#   pre_invoke:
#     - type: shell
#       command: "echo starting {name}"
#   post_invoke:
#     - type: shell
#       command: "echo done"
# arguments:
#   - name: TARGET
#     description: "Target file or directory"
#     required: false
---

# {name}

Follow these steps:

1. Understand the user's request
2. $ARGUMENTS
3. Report results
"#
            );
            std::fs::write(skill_dir.join("SKILL.md"), skill_md).map_err(|e| e.to_string())?;

            eprintln!(
                "  {} Skill scaffolded: {}",
                "\u{2713}".green(),
                skill_dir.display().to_string().cyan()
            );
            match state.unified_skill_registry.discover_all().await {
                Ok(_) => eprintln!("  {}", "Skill registry refreshed.".dim()),
                Err(err) => eprintln!(
                    "  {} {}",
                    "Warning:".yellow(),
                    format!("Skill registry refresh failed: {err}").dim()
                ),
            }
            eprintln!("  {} SKILL.md", "Files created:".dim());
            eprintln!("  {}", format!("Dev mode: /skill dev {name}").dim());
        }

        "test" => {
            let name = sub_arg.split(' ').next().unwrap_or("").trim();
            let json_args = sub_arg.split_once(' ').map(|x| x.1).unwrap_or("").trim();
            if name.is_empty() {
                eprintln!("{}", "  Usage: /skill test <name> [json_args]".yellow());
                return Ok(());
            }
            eprintln!(
                "\n{}",
                format!("─── Skill test: {name} ───────────────────────────────────────")
                    .bold()
                    .cyan()
            );
            if !json_args.is_empty() {
                eprintln!("  Input: {}", json_args.cyan());
            }

            // Try API first
            let api_ok = if let Some(tok) = token {
                let payload = serde_json::json!({
                    "skill_id": name,
                    "args": if json_args.is_empty() {
                        serde_json::Value::Object(serde_json::Map::new())
                    } else {
                        serde_json::from_str(json_args).unwrap_or(serde_json::Value::String(json_args.to_string()))
                    }
                });
                match api.post_skills_test_json(tok, &payload).await {
                    Ok(body) => {
                        eprintln!("  {}", "\u{2713} API test result:".green());
                        eprintln!("  {}", body.dim());
                        true
                    }
                    Err(_) => false,
                }
            } else {
                false
            };

            if !api_ok {
                let skill_dir = match resolve_skill_dir_on_disk(name) {
                    Some(p) => p,
                    None => {
                        eprintln!(
                            "{}",
                            format!(
                                "  \u{2717} Skill directory not found for '{name}' (searched .astra/skills, skills/, ~/.astra/skills)"
                            )
                            .yellow()
                        );
                        eprintln!();
                        return Ok(());
                    }
                };
                let skill_md = skill_dir.join("SKILL.md");
                let test_file = skill_dir.join("test_skill.py");

                if skill_md.exists() {
                    eprintln!(
                        "  {} SKILL.md ({})…",
                        "Validating".dim(),
                        skill_dir.display().to_string().dim()
                    );
                    let src = std::fs::read_to_string(&skill_md).map_err(|e| e.to_string())?;
                    let issues = collect_skill_md_issues(name, &src);
                    let mut ok = issues.is_empty();
                    if !issues.is_empty() {
                        for issue in &issues {
                            eprintln!("  {}", format!("\u{2717} {issue}").red());
                        }
                    } else if let Some(end) = src[3..].find("\n---") {
                        let yaml = &src[3..3 + end];
                        if let Ok(val) = serde_yaml_ng::from_str::<serde_json::Value>(yaml) {
                            let sname = val.get("name").and_then(|v| v.as_str()).unwrap_or("");
                            eprintln!("  {} {}", "Manifest name:".dim(), sname.cyan());
                        }
                        let body = &src[3 + end + 4..];
                        eprintln!("  {} {} chars", "Instruction body:".dim(), body.len());
                    }

                    if ok && let Ok((manifest, _body)) = astra_skills::loader::parse_skill_md(&src)
                    {
                        if let Some(ref hooks) = manifest.hooks {
                            if !hooks.pre_invoke.is_empty() {
                                eprintln!("  {} pre_invoke hooks…", "Running".dim());
                                for action in &hooks.pre_invoke {
                                    if let astra_skills::hooks::HookAction::Shell { command } =
                                        action
                                    {
                                        eprintln!("  {} {command}", "$".dim());
                                        match std::process::Command::new("sh")
                                            .arg("-c")
                                            .arg(command)
                                            .current_dir(&skill_dir)
                                            .output()
                                        {
                                            Ok(o) if o.status.success() => {
                                                eprintln!("    {}", "\u{2713} ok".green());
                                            }
                                            Ok(o) => {
                                                eprintln!(
                                                    "    {}",
                                                    format!("\u{2717} exit {}", o.status).red()
                                                );
                                                ok = false;
                                            }
                                            Err(e) => {
                                                eprintln!("    {}", format!("\u{2717} {e}").red());
                                                ok = false;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    if ok {
                        eprintln!("  {}", "\u{2713} SKILL.md validation passed".green());
                    }
                } else if test_file.exists() {
                    eprintln!("  Running legacy Python tests...");
                    let out = std::process::Command::new("python3")
                        .args([
                            "-m",
                            "unittest",
                            "discover",
                            "-s",
                            ".",
                            "-p",
                            "test_*.py",
                            "-q",
                        ])
                        .current_dir(&skill_dir)
                        .output();
                    match out {
                        Ok(o) => {
                            let stdout = String::from_utf8_lossy(&o.stdout);
                            let stderr = String::from_utf8_lossy(&o.stderr);
                            if o.status.success() {
                                eprintln!("  {}", "\u{2713} Local skill tests passed".green());
                            } else {
                                eprintln!("  {}", "\u{2717} Local skill tests failed".red());
                            }
                            if !stdout.is_empty() {
                                eprintln!("{stdout}");
                            }
                            if !stderr.is_empty() {
                                eprintln!("{stderr}");
                            }
                        }
                        Err(e) => {
                            eprintln!(
                                "{}",
                                format!("  \u{2717} Failed to run local tests: {e}").red()
                            );
                        }
                    }
                } else {
                    eprintln!(
                        "  {}",
                        "No SKILL.md or test_skill.py found. Use /skill new to scaffold.".yellow()
                    );
                }
            }
            eprintln!();
        }

        "dev" => {
            if sub_arg == "off" {
                state.skill_dev = None;
                eprintln!("  {}", "Exited skill dev mode".green());
                return Ok(());
            }
            let name = sub_arg;
            if name.is_empty() {
                if let Some(ref dev) = state.skill_dev {
                    eprintln!(
                        "  \u{1f527} Currently in skill dev mode: {}",
                        dev.name.as_str().cyan()
                    );
                    eprintln!("  Use /skill dev off to exit.");
                } else {
                    eprintln!(
                        "{}",
                        "  Usage: /skill dev <name>  (or /skill dev off)".yellow()
                    );
                }
                return Ok(());
            }
            // Search all skill paths (project .astra/skills/, skills/, ~/.astra/skills/)
            let search_paths = crate::skill_instructions::skill_search_paths();
            let found = search_paths.iter().find_map(|base| {
                let dir = base.join(name);
                if dir.join("SKILL.md").exists() {
                    Some((dir, "SKILL.md"))
                } else if dir.join("skill.py").exists() {
                    Some((dir, "skill.py (legacy)"))
                } else {
                    None
                }
            });
            let (skill_dir, src_label) = match found {
                Some(pair) => pair,
                None => {
                    eprintln!(
                        "{}",
                        format!(
                            "  \u{2717} SKILL.md not found for '{name}'. Use /skill new {name} to scaffold."
                        )
                        .yellow()
                    );
                    return Ok(());
                }
            };
            state.skill_dev = Some(super::SkillDevState {
                name: name.to_string(),
                dir: skill_dir.clone(),
            });
            eprintln!(
                "\n  \u{1f527} {} {}",
                "Skill dev mode:".bold(),
                name.cyan().bold()
            );
            eprintln!("  {}", format!("Dir: {}", skill_dir.display()).dim());
            eprintln!("  {}", format!("Source: {src_label}").dim());
            eprintln!(
                "  {}",
                "SKILL.md is re-read from disk each turn — external edits are picked up automatically.".dim()
            );
            eprintln!("  {}", "Exit: /skill dev off".dim());
            eprintln!();
        }

        "health" => {
            eprintln!(
                "\n{}",
                "─── Skill catalog health ─────────────────────────────────────"
                    .bold()
                    .cyan()
            );
            // Try API first
            let api_ok = if let Some(tok) = token {
                match api.get_skills_status_query_text(tok, &[]).await {
                    Ok(body) => {
                        let value: serde_json::Value =
                            serde_json::from_str(&body).unwrap_or_default();
                        let skills = value
                            .as_array()
                            .cloned()
                            .or_else(|| value.get("skills").and_then(|v| v.as_array()).cloned())
                            .unwrap_or_default();
                        eprintln!(
                            "{}",
                            format!(
                                "{:<28}  {:<10}  {:<8}  {}",
                                "Name", "Registered", "Healthy", "Issues"
                            )
                            .bold()
                        );
                        eprintln!("{}", "\u{2500}".repeat(70).dim());
                        for s in &skills {
                            let name = s.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                            let registered = s
                                .get("registered")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false);
                            let healthy =
                                s.get("healthy").and_then(|v| v.as_bool()).unwrap_or(false);
                            let issues = s.get("issues").and_then(|v| v.as_str()).unwrap_or("");
                            eprintln!(
                                "  {:<26}  {:<10}  {:<8}  {}",
                                name.cyan(),
                                if registered {
                                    "\u{2713}".green().to_string()
                                } else {
                                    "\u{2717}".red().to_string()
                                },
                                if healthy {
                                    "\u{2713}".green().to_string()
                                } else {
                                    "\u{2717}".red().to_string()
                                },
                                issues
                            );
                        }
                        eprintln!();
                        true
                    }
                    _ => false,
                }
            } else {
                false
            };

            if !api_ok {
                eprintln!(
                    "  {}",
                    "Local view: unified catalog + on-disk paths (.astra/skills, skills/, ~/.astra/skills)."
                        .dim()
                );
                let registry = &state.unified_skill_registry;
                let mut manifests = registry.all_manifests();
                if manifests.is_empty() {
                    eprintln!("  {}", "No skills discovered in catalog.".dim());
                    eprintln!("  {}", "Use /skill new <name> to create one, or add SKILL.md files to .astra/skills/.".dim());
                    eprintln!();
                    return Ok(());
                }
                manifests.sort_by(|a, b| a.name.cmp(&b.name));
                eprintln!(
                    "{}",
                    format!(
                        "{:<24}  {:<10}  {:<8}  {}",
                        "Name", "Source", "On disk", "Check"
                    )
                    .bold()
                );
                eprintln!("{}", "\u{2500}".repeat(72).dim());
                for m in &manifests {
                    use astra_skills::SkillSourceKind::*;
                    let (disk_col, check_col): (String, String) = match m.source {
                        Mcp | Database | Plugin => {
                            ("—".dim().to_string(), "(remote)".dim().to_string())
                        }
                        Local | Bundled => {
                            let disk = resolve_skill_dir_on_disk(m.name.as_str());
                            let disk_mark = if disk.is_some() {
                                "\u{2713}".green().to_string()
                            } else {
                                "\u{2717}".red().to_string()
                            };
                            let check = if let Some(ref d) = disk {
                                let md = d.join("SKILL.md");
                                if md.exists() {
                                    match std::fs::read_to_string(md) {
                                        Ok(src) => {
                                            let issues =
                                                collect_skill_md_issues(m.name.as_str(), &src);
                                            if issues.is_empty() {
                                                "ok".green().to_string()
                                            } else {
                                                format!("{} issue(s)", issues.len())
                                                    .yellow()
                                                    .to_string()
                                            }
                                        }
                                        Err(e) => format!("read err: {e}").red().to_string(),
                                    }
                                } else if d.join("skill.py").exists() {
                                    "legacy skill.py".dim().to_string()
                                } else {
                                    "no SKILL.md".yellow().to_string()
                                }
                            } else {
                                "not found".yellow().to_string()
                            };
                            (disk_mark, check)
                        }
                    };
                    eprintln!(
                        "  {:<22}  {:<10}  {:<8}  {}",
                        m.name.as_str().cyan(),
                        source_label(&m.source).dim(),
                        disk_col,
                        check_col
                    );
                }
                eprintln!();
            }
        }

        "system" => {
            let available = prompts::builtin_system_skills();
            if sub_arg.is_empty() || sub_arg == "list" {
                eprintln!("\n  {}", "System Skills".bold());
                for skill in &available {
                    let active = state
                        .active_system_skills
                        .iter()
                        .any(|s| s.name == skill.name);
                    let marker = if active {
                        "●".green().to_string()
                    } else {
                        "○".dim().to_string()
                    };
                    eprintln!(
                        "  {} {:<12} {}",
                        marker,
                        skill.name.as_str().cyan(),
                        skill.description.as_str().dim()
                    );
                }
                if state.active_system_skills.is_empty() {
                    eprintln!(
                        "\n  {}",
                        "No active system skills. Use /skill system <name> to toggle.".dim()
                    );
                } else {
                    let names: Vec<&str> = state
                        .active_system_skills
                        .iter()
                        .map(|s| s.name.as_str())
                        .collect();
                    eprintln!("\n  Active: {}", names.join(", ").green());
                }
                eprintln!();
            } else {
                let name = sub_arg;
                if let Some(pos) = state
                    .active_system_skills
                    .iter()
                    .position(|s| s.name == name)
                {
                    state.active_system_skills.remove(pos);
                    eprintln!(
                        "  {} System skill {} {}",
                        "○".dim(),
                        name.cyan(),
                        "deactivated".dim()
                    );
                } else if let Some(skill) = available.iter().find(|s| s.name == name) {
                    state.active_system_skills.push(skill.clone());
                    eprintln!(
                        "  {} System skill {} {}",
                        "●".green(),
                        name.cyan(),
                        "activated".green()
                    );
                } else {
                    let names: Vec<&str> = available.iter().map(|s| s.name.as_str()).collect();
                    eprintln!(
                        "{}",
                        format!(
                            "  Unknown skill: '{}'. Available: {}",
                            name,
                            names.join(", ")
                        )
                        .yellow()
                    );
                }
            }
        }

        "stats" => {
            let tracker = &state.skill_quality_tracker;
            let entries = tracker.all_entries();

            if sub_arg.is_empty() {
                // Show all tracked skills
                if entries.is_empty() {
                    eprintln!(
                        "  {}",
                        "No skill execution data yet. Run some skills first.".dim()
                    );
                } else {
                    eprintln!(
                        "\n  {:<24}  {:>6}  {:>6}  {:>6}  {:>7}  {:>5}",
                        "Skill", "Runs", "Pass", "Fail", "Quality", "Boost"
                    );
                    eprintln!("  {}", "─".repeat(72));
                    let mut sorted: Vec<_> = entries.iter().collect();
                    sorted.sort_by(|a, b| {
                        b.1.quality_score()
                            .partial_cmp(&a.1.quality_score())
                            .unwrap_or(std::cmp::Ordering::Equal)
                    });
                    for (name, entry) in &sorted {
                        let score = entry.quality_score();
                        let boost = entry.selection_boost();
                        let score_color = if score >= 0.7 {
                            format!("{:.0}%", score * 100.0).green().to_string()
                        } else if score >= 0.4 {
                            format!("{:.0}%", score * 100.0).yellow().to_string()
                        } else {
                            format!("{:.0}%", score * 100.0).red().to_string()
                        };
                        eprintln!(
                            "  {:<24}  {:>6}  {:>6}  {:>6}  {:>7}  {:>5.2}x",
                            name,
                            entry.invocations,
                            entry.successes,
                            entry.failures,
                            score_color,
                            boost
                        );
                    }
                    eprintln!();
                }
            } else {
                // Show detailed stats for one skill
                let name = sub_arg;
                match tracker.get(name) {
                    Some(entry) => {
                        eprintln!("\n  Skill: {}", name.cyan());
                        eprintln!("  ─────────────────────────");
                        eprintln!("  Invocations:      {}", entry.invocations);
                        eprintln!("  Successes:        {}", entry.successes);
                        eprintln!("  Failures:         {}", entry.failures);
                        eprintln!("  Partial:          {}", entry.partial);
                        eprintln!("  Success rate:     {:.0}%", entry.success_rate() * 100.0);
                        eprintln!(
                            "  User satisfaction:{:.0}%",
                            entry.user_satisfaction() * 100.0
                        );
                        eprintln!("  Quality score:    {:.0}%", entry.quality_score() * 100.0);
                        eprintln!("  Selection boost:  {:.2}x", entry.selection_boost());
                        if entry.invocations > 0 {
                            eprintln!("  Avg tokens:       {:.0}", entry.avg_tokens());
                            eprintln!("  Avg duration:     {:.0}ms", entry.avg_duration_ms());
                        }
                        eprintln!();
                    }
                    None => {
                        eprintln!(
                            "  {}",
                            format!("No execution data for skill '{name}'").yellow()
                        );
                    }
                }
            }
        }

        "search-remote" | "marketplace" => {
            let query_str = sub_arg.trim();
            if query_str.is_empty() {
                eprintln!("{}", "  Usage: /skill search-remote <query>".yellow());
                eprintln!(
                    "{}",
                    "  Searches the marketplace for skills with ranking.".dim()
                );
                return Ok(());
            }

            let tok = token.unwrap_or("");
            match api
                .get_bearer_path_query_text(
                    tok,
                    "/marketplace/search",
                    &[("query", query_str.to_string())],
                )
                .await
            {
                Ok(text) => {
                    match serde_json::from_str::<
                        astra_services::marketplace_stats::SkillSearchResponse,
                    >(&text)
                    {
                        Ok(resp) => {
                            eprintln!(
                                "\n  {} '{}' ({} results)",
                                "Marketplace search:".dim(),
                                query_str.cyan(),
                                resp.total
                            );
                            eprintln!("{}", "\u{2500}".repeat(78).dim());

                            if resp.results.is_empty() {
                                eprintln!("  {}", "No matching skills in marketplace.".dim());
                            } else {
                                eprintln!(
                                    "  {:<24}  {:<8}  {:<10}  {:<6}  {}",
                                    "Name".bold(),
                                    "Version".bold(),
                                    "Trust".bold(),
                                    "Score".bold(),
                                    "Description".bold()
                                );
                                for r in &resp.results {
                                    let tier = r.trust_tier.as_deref().unwrap_or("?");
                                    let desc =
                                        truncate_desc(r.description.as_deref().unwrap_or(""), 30);
                                    eprintln!(
                                        "  {:<24}  {:<8}  {:<10}  {:<6.2}  {}",
                                        r.skill_name.as_str().cyan(),
                                        r.version.as_str().dim(),
                                        tier.dim(),
                                        r.ranking_score,
                                        desc
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!("  {} {}", "✗ Parse error:".yellow(), format!("{e}").dim())
                        }
                    }
                }
                Err(e) => {
                    eprintln!(
                        "  {} {}",
                        "✗ Marketplace unavailable:".yellow(),
                        format!("{e}").dim()
                    );
                    eprintln!(
                        "  {}",
                        "Tip: use '/skill search' for local keyword match (not vector search)."
                            .dim()
                    );
                }
            }
            eprintln!();
        }

        "upload-quality" => {
            upload_quality_report(api, &state.skill_quality_tracker, token).await;
        }

        "compose-info" => {
            let skill_name = sub_arg.trim();
            if skill_name.is_empty() {
                eprintln!("{}", "  Usage: /skill compose-info <name>".yellow());
                return Ok(());
            }
            let registry = &state.unified_skill_registry;
            match registry.get_loaded_skill(skill_name) {
                Some(skill) => {
                    eprintln!("  {} {}", "Skill:".bold(), skill.manifest.name);
                    if let Some(ref comp) = skill.manifest.composition {
                        eprintln!("  {} {}", "composable:".dim(), comp.composable);
                        eprintln!("  {} {}", "idempotent:".dim(), comp.idempotent);
                        if !comp.side_effects.is_empty() {
                            eprintln!(
                                "  {} {}",
                                "side_effects:".dim(),
                                comp.side_effects.join(", ")
                            );
                        }
                        if let Some(t) = comp.max_duration_sec {
                            eprintln!("  {} {}s", "max_duration:".dim(), t);
                        }
                    } else {
                        eprintln!("  {}", "(no composition metadata declared)".dim());
                    }
                    if skill.manifest.input_schema.is_some() {
                        eprintln!("  {} defined", "input_schema:".dim());
                    }
                    if skill.manifest.output_schema.is_some() {
                        eprintln!("  {} defined", "output_schema:".dim());
                    }
                    if !skill.manifest.required_capabilities.is_empty() {
                        eprintln!(
                            "  {} {}",
                            "required_capabilities:".dim(),
                            skill.manifest.required_capabilities.join(", ")
                        );
                    }
                }
                None => {
                    eprintln!("{}", format!("  Skill '{skill_name}' not found.").yellow());
                }
            }
        }

        "pin" => {
            let skill_name = sub_arg.trim();
            if skill_name.is_empty() {
                if state.pinned_skills.is_empty() {
                    eprintln!("  {}", "No pinned skills.".dim());
                } else {
                    eprintln!("  {}", "Pinned skills:".bold());
                    for name in &state.pinned_skills {
                        eprintln!("    📌 {name}");
                    }
                }
                return Ok(());
            }
            // Verify the skill exists
            let registry = &state.unified_skill_registry;
            if registry.get_loaded_skill(skill_name).is_none() {
                eprintln!("{}", format!("  Skill '{skill_name}' not found.").yellow());
                return Ok(());
            }
            if state.pinned_skills.insert(skill_name.to_string()) {
                eprintln!("  📌 Pinned skill '{skill_name}' — always included in budget.");
            } else {
                eprintln!("  Already pinned: '{skill_name}'");
            }
        }

        "unpin" => {
            let skill_name = sub_arg.trim();
            if skill_name.is_empty() {
                eprintln!("{}", "  Usage: /skill unpin <name>".yellow());
                return Ok(());
            }
            if state.pinned_skills.remove(skill_name) {
                eprintln!("  Unpinned skill '{skill_name}'.");
            } else {
                eprintln!(
                    "{}",
                    format!("  Skill '{skill_name}' was not pinned.").yellow()
                );
            }
        }

        "install" => {
            install_skill_from_marketplace(sub_arg.trim(), api, token, state).await;
        }

        "publish" => {
            publish_skill_to_marketplace(sub_arg.trim(), api, token, state).await;
        }

        "uninstall" | "remove" => {
            uninstall_local_skill(sub_arg.trim(), state).await;
        }

        "pack" => {
            pack_skill_bundle(sub_arg.trim());
        }

        "unpack" => {
            unpack_skill_bundle(sub_arg.trim(), state).await;
        }

        "inspect" => {
            inspect_skill_bundle(sub_arg.trim());
        }

        "browse" => {
            browse_marketplace(sub_arg.trim(), api, token).await;
        }

        "trending" => {
            trending_marketplace(api, token).await;
        }

        "installed" => {
            list_installed_marketplace(api, token).await;
        }

        "check-update" | "check-updates" => {
            check_skill_updates(sub_arg.trim(), api, token).await;
        }

        "upgrade" | "update" => {
            upgrade_skill(sub_arg.trim(), api, token, state).await;
        }

        "rollback" | "downgrade" => {
            rollback_skill(sub_arg.trim(), api, token, state).await;
        }

        "create" => {
            // Auto-generate a skill from the current session transcript
            create_skill_from_session(sub_arg, state).await?;
        }

        "feedback" => {
            // Parse: <skill_name> +/- or <skill_name> up/down or <skill_name> thumbs_up/thumbs_down
            let parts: Vec<&str> = sub_arg.split_whitespace().collect();
            if parts.len() < 2 {
                eprintln!("  {} Usage: /skill feedback <skill_name> +/-", "⚠".yellow());
                eprintln!("  Examples:");
                eprintln!("    /skill feedback pdf +     {} thumbs up", "—".dim());
                eprintln!("    /skill feedback pdf -     {} thumbs down", "—".dim());
                return Ok(());
            }
            let skill_name = parts[0];
            let positive = match parts[1] {
                "+" | "+1" | "up" | "thumbs_up" | "good" | "yes" => true,
                "-" | "-1" | "down" | "thumbs_down" | "bad" | "no" => false,
                other => {
                    eprintln!("  {} Unknown feedback type: {}", "✗".red(), other.yellow());
                    eprintln!("  Use + (positive) or - (negative)");
                    return Ok(());
                }
            };
            // Record feedback via the quality tracker
            state
                .skill_quality_tracker
                .record_feedback(skill_name, positive);
            let emoji = if positive { "👍" } else { "👎" };
            let word = if positive { "positive" } else { "negative" };
            eprintln!(
                "  {} Recorded {} feedback for skill '{}'",
                emoji,
                word.cyan(),
                skill_name.green()
            );
        }

        "evolve" => {
            let evo = match &state.evolution_service {
                Some(e) => e,
                None => {
                    eprintln!("  {} Evolution service not initialized", "✗".red());
                    return Ok(());
                }
            };
            if sub_arg.is_empty() {
                // Show evolution status
                let signal_count = evo.signal_count().await;
                let pending = evo.pending().await;
                let canaries = evo.active_canaries().await;
                let applied = evo.applied().await;
                eprintln!(
                    "\n{}",
                    "─── Evolution Status ─────────────────────────────"
                        .bold()
                        .cyan()
                );
                eprintln!(
                    "  {:<20} {}",
                    "buffered signals:".dim(),
                    signal_count.to_string().cyan()
                );
                eprintln!(
                    "  {:<20} {}",
                    "pending proposals:".dim(),
                    pending.len().to_string().cyan()
                );
                eprintln!(
                    "  {:<20} {}",
                    "active canaries:".dim(),
                    canaries.len().to_string().cyan()
                );
                eprintln!(
                    "  {:<20} {}",
                    "applied proposals:".dim(),
                    applied.len().to_string().cyan()
                );
                let axis_label = |p: &astra_runtime::evolution::types::EvolutionProposal| match &p
                    .axis
                {
                    astra_runtime::evolution::types::EvolutionAxis::Skill {
                        skill_name,
                        section,
                        ..
                    } => format!("skill:{}/{}", skill_name, section.heading()),
                    astra_runtime::evolution::types::EvolutionAxis::Pattern {
                        signature, ..
                    } => {
                        format!("pattern:{signature}")
                    }
                    astra_runtime::evolution::types::EvolutionAxis::Calibration { .. } => {
                        "calibration".into()
                    }
                    astra_runtime::evolution::types::EvolutionAxis::Entity { entity, .. } => {
                        format!("entity:{entity}")
                    }
                };
                if !pending.is_empty() {
                    eprintln!("\n  {}", "Pending proposals:".yellow());
                    for p in &pending {
                        eprintln!(
                            "    {} {} (confidence: {:.0}%)",
                            p.id.as_str().dim(),
                            axis_label(p).cyan(),
                            p.confidence * 100.0
                        );
                        eprintln!("      {}", p.reasoning.as_str().dim());
                    }
                }
                if !canaries.is_empty() {
                    eprintln!("\n  {}", "Active canaries:".magenta());
                    for p in &canaries {
                        eprintln!(
                            "    {} {} (confidence: {:.0}%)",
                            p.id.as_str().dim(),
                            axis_label(p).magenta(),
                            p.confidence * 100.0
                        );
                        eprintln!("      {}", p.reasoning.as_str().dim());
                    }
                }
                if !pending.is_empty() || !canaries.is_empty() {
                    eprintln!(
                        "\n  {}",
                        "Use /skill evolve approve <id> or /skill evolve reject <id> to resolve pending proposals or active canaries".dim()
                    );
                }
                if !applied.is_empty() {
                    let recent: Vec<_> = applied.iter().rev().take(5).collect();
                    eprintln!("\n  {}", "Recent applied (last 5):".green());
                    for p in recent {
                        eprintln!("    {} {}", p.id.as_str().dim(), p.reasoning.as_str().dim());
                    }
                }
            } else if let Some(id) = sub_arg.strip_prefix("approve ") {
                let id = id.trim();
                match evo.approve(id).await {
                    Ok(Some(p)) => {
                        let message = match p.status {
                            astra_runtime::evolution::types::ApprovalStatus::CanaryPromoted => {
                                "Promoted canary"
                            }
                            _ => "Approved and applied",
                        };
                        eprintln!("  {} {}: {}", "✓".green(), message, p.id);
                    }
                    Ok(None) => {
                        eprintln!(
                            "  {} No pending proposal or active canary with id '{}'",
                            "✗".red(),
                            id
                        );
                    }
                    Err(e) => eprintln!("  {} Approval failed: {}", "✗".red(), e),
                }
            } else if let Some(id) = sub_arg.strip_prefix("reject ") {
                let id = id.trim();
                match evo.reject(id).await {
                    Ok(Some(p)) => {
                        let message = match p.status {
                            astra_runtime::evolution::types::ApprovalStatus::CanaryRolledBack => {
                                "Rolled back canary"
                            }
                            _ => "Rejected",
                        };
                        eprintln!("  {} {}: {}", "✓".green(), message, id);
                    }
                    Ok(None) => {
                        eprintln!(
                            "  {} No pending proposal or active canary with id '{}'",
                            "✗".red(),
                            id
                        )
                    }
                    Err(e) => eprintln!("  {} Reject failed: {}", "✗".red(), e),
                }
            } else {
                eprintln!("  {}", "Usage:".yellow());
                eprintln!("    {}       Show evolution status", "/skill evolve".cyan());
                eprintln!(
                    "    {}  Approve a proposal or promote a canary",
                    "/skill evolve approve <id>".cyan()
                );
                eprintln!(
                    "    {}   Reject a proposal or roll back a canary",
                    "/skill evolve reject <id>".cyan()
                );
            }
        }

        "reflect" => {
            let evo = match &state.evolution_service {
                Some(e) => e,
                None => {
                    eprintln!("  {} Evolution service not initialized", "✗".red());
                    return Ok(());
                }
            };

            let session_id = state.session_id.as_deref().unwrap_or("unknown");
            let turns_completed = state.turn;
            let scenario = state.observability_session.as_ref().and_then(|sess| {
                let s = sess.read().ok()?;
                s.current_scenario().map(|sc| format!("{:?}", sc))
            });
            let total_tokens = state.total_prompt_tokens + state.total_completion_tokens;
            let token_util = if turns_completed > 0 && total_tokens > 0 {
                // Rough utilization: average tokens per turn / 200k reference budget
                (total_tokens as f64 / turns_completed as f64) / 200_000.0
            } else {
                0.0
            };

            // Build tool stats from persisted tool health entries
            let tool_stats: Vec<astra_runtime::liquid::reflection::ToolStat> = state
                .tool_health_entries
                .iter()
                .map(|e| astra_runtime::liquid::reflection::ToolStat {
                    tool_name: e.name.clone(),
                    calls: e.total_calls as u32,
                    failures: e.total_failures as u32,
                    avg_latency_ms: 0, // latency not tracked in ToolHealthEntry
                })
                .collect();

            // Flush to get LLM signals
            let (_fast, llm_signals) = evo.flush().await;

            if llm_signals.is_empty() && sub_arg != "force" {
                eprintln!(
                    "  {} No signals requiring LLM reflection. Use {} to force.",
                    "ℹ".cyan(),
                    "/skill reflect force".cyan()
                );
                return Ok(());
            }

            let ctx = evo.build_reflection_context(
                session_id,
                turns_completed,
                scenario.as_deref(),
                token_util,
                &llm_signals,
                tool_stats,
                vec![],
                None,
            );

            let (system_prompt, user_prompt) = evo.build_reflection_prompt(&ctx);

            eprintln!(
                "\n{}",
                "─── Reflection ───────────────────────────────────"
                    .bold()
                    .cyan()
            );
            eprintln!(
                "  🔍 Building reflection from {} signals, {} turns",
                ctx.signals.len(),
                ctx.turns_completed
            );

            eprintln!("  {}", "Context:".yellow());
            for sig in &ctx.signals {
                eprintln!(
                    "    [{}] {}",
                    sig.kind.as_str().cyan(),
                    sig.detail.as_str().dim()
                );
            }

            if sub_arg == "prompt" {
                eprintln!("\n  {}", "System prompt:".yellow());
                eprintln!("{}", system_prompt.dim());
                eprintln!("\n  {}", "User prompt:".yellow());
                eprintln!("{}", user_prompt.dim());
                return Ok(());
            }

            let Some(tok) = token else {
                eprintln!(
                    "\n  {} Reflection prompt ready ({} chars).",
                    "✓".green(),
                    system_prompt.len() + user_prompt.len()
                );
                eprintln!(
                    "  {}",
                    "Not logged in, so live reflection was skipped.".yellow()
                );
                eprintln!(
                    "  💡 To see full prompt: {}",
                    "/skill reflect prompt".cyan()
                );
                return Ok(());
            };

            let reflection_message = format!(
                "Follow these reflection instructions exactly and respond with JSON only.\n\nSystem instructions:\n{}\n\nReflection context:\n{}",
                system_prompt, user_prompt
            );
            let reflection_history: Vec<(String, String)> = Vec::new();
            let (selector, _) = create_tool_selector_quiet(api, None);
            let mut auto_pm =
                PermissionManager::with_project(true, &std::env::current_dir().unwrap_or_default());
            let result = stream_chat_sse(ChatTurnParams {
                api,
                token: tok,
                auth_profile: profile,
                message: &reflection_message,
                session_id: state.session_id.as_deref(),
                model: state.model.as_deref(),
                provider: None,
                explain: ExplainMode::Off,
                render_md: false,
                history: &reflection_history,
                perm_manager: &mut auto_pm,
                verbose_mode: false,
                render_policy: crate::stream_render::RenderPolicy::Silent,
                selector: selector.as_ref(),
                recent_tools: &[],
                tool_health_entries: &[],
                session_lessons: &state.session_lessons,
                latest_skill_diagnosis: state.latest_skill_diagnosis.as_ref(),
                unified_skill_registry: &state.unified_skill_registry,
                plan_only_chat: false,
                is_plan_subtask: false,
                plan_subtask_id: None,
                delegation_engine: None,
                cancel_token: None,
                plan_assemble_line_release: None,
                stream_event_tx: None,
                approval_request_tx: None,
                mcp_manager: Some(state.mcp_manager.clone()),
                skill_search: &state.skill_search,
                skill_quality_tracker: &mut state.skill_quality_tracker,
                discovered_skills: None,
                messaging_metrics: state.messaging_metrics.clone(),
                agent_spawner: state.agent_spawner.clone(),
                root_agent_id: Some("main"),
                root_mailbox_slot: Some(&mut state.root_mailbox),
                observability_hub: state.observability_hub.clone(),
                observability_session: state.observability_session.clone(),
                file_journal: Some(state.file_journal.clone()),
                file_state: Some(state.file_state.clone()),
                database_snapshot_journal: Some(state.database_snapshot_journal.clone()),
                git_stash_journal: Some(state.git_stash_journal.clone()),
                git_commit_journal: Some(state.git_commit_journal.clone()),
                git_worktree_journal: Some(state.git_worktree_journal.clone()),
                session_state_journal: Some(state.session_state_journal.clone()),
                task_manager: Some(state.task_manager.clone()),
                runtime_continuity: state.runtime_continuity.as_ref(),
                turn_index: state.turn,
                evolution_service: None,
                pipeline_state: None,
                pre_loaded_messages: None,
                append_system_prompt: None,
                #[cfg(feature = "harness")]
                harness_sink: Some(state.harness_sink.clone()),
                #[cfg(feature = "harness")]
                harness_trace: Some(state.harness_trace.clone()),
            })
            .await
            .map_err(|e| e.to_string())?;

            let response = result.full_text.trim();
            let outcome = evo
                .ingest_reflection_response_detailed(response, &ctx)
                .await;
            match outcome {
                Ok(outcome) => {
                    eprintln!(
                        "\n  {} Reflection executed live; processed {} proposal(s): {} auto-applied, {} canary-started, {} queued.",
                        "✓".green(),
                        outcome.processed.to_string().cyan(),
                        outcome.auto_applied.to_string().cyan(),
                        outcome.canary_started.to_string().cyan(),
                        outcome.queued.to_string().cyan()
                    );
                    if outcome.queued > 0 {
                        eprintln!("  💡 Review proposals in: {}", "/skill evolve".cyan());
                    }
                }
                Err(err) => {
                    let preview = response.lines().take(12).collect::<Vec<_>>().join("\n");
                    eprintln!("  {} Reflection response parse failed: {}", "✗".red(), err);
                    if !preview.is_empty() {
                        eprintln!("\n  {}", "Response preview:".yellow());
                        eprintln!("{}", preview.dim());
                    }
                }
            }
        }

        _ => {
            eprintln!(
                "{}",
                format!("  Unknown /skill subcommand: '{sub}'").yellow()
            );
            eprintln!(
                "  {}",
                "Common: list · info · search · new · test · dev · health · surfacing".dim()
            );
            eprintln!(
                "  {}",
                "Marketplace: browse · install · publish · trending · installed".dim()
            );
        }
    }
    Ok(())
}

// ── Skill path + SKILL.md checks (info --raw, test, doctor) ──

fn parse_skill_info_args(sub_arg: &str) -> (String, bool) {
    const SUFFIX: &str = " --raw";
    let s = sub_arg.trim();
    if s.len() > SUFFIX.len() && s.ends_with(SUFFIX) {
        return (s[..s.len() - SUFFIX.len()].trim().to_string(), true);
    }
    (s.to_string(), false)
}

/// Same resolution order as `/skill dev`: `.astra/skills`, `skills/`, `~/.astra/skills`.
fn resolve_skill_dir_on_disk(name: &str) -> Option<std::path::PathBuf> {
    crate::skill_instructions::skill_search_paths()
        .into_iter()
        .find_map(|base| {
            let dir = base.join(name);
            if dir.join("SKILL.md").exists() || dir.join("skill.py").exists() {
                Some(dir)
            } else {
                None
            }
        })
}

fn collect_skill_md_issues(_skill_name: &str, src: &str) -> Vec<String> {
    let mut issues: Vec<String> = Vec::new();
    if !src.starts_with("---") {
        issues.push("missing YAML frontmatter (must start with ---)".to_string());
    } else if let Some(end) = src[3..].find("\n---") {
        let yaml_block = &src[3..3 + end];
        match serde_yaml_ng::from_str::<serde_json::Value>(yaml_block) {
            Ok(val) => {
                if val
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .is_empty()
                {
                    issues.push("frontmatter `name` is missing or empty".to_string());
                }
            }
            Err(e) => {
                issues.push(format!("invalid YAML frontmatter: {e}"));
            }
        }
        let body = &src[3 + end + 4..];
        if body.trim().is_empty() {
            issues.push("instruction body is empty (content after frontmatter)".to_string());
        }
    } else {
        issues.push("unclosed frontmatter (missing closing ---)".to_string());
    }
    issues
}

fn print_skill_directory_raw(name: &str, skill_dir: &std::path::Path) -> Result<(), String> {
    let skill_md_path = skill_dir.join("SKILL.md");
    let json_path = skill_dir.join("skill.json");
    if skill_md_path.exists() {
        let raw = std::fs::read_to_string(&skill_md_path).map_err(|e| e.to_string())?;
        if raw.starts_with("---") {
            if let Some(end) = raw[3..].find("\n---") {
                let yaml_block = &raw[3..3 + end];
                eprintln!(
                    "\n{}",
                    format!("─── {name}/SKILL.md frontmatter ────────────────────────────")
                        .bold()
                        .cyan()
                );
                for line in yaml_block.lines() {
                    eprintln!("  {line}");
                }
                eprintln!();
            } else {
                eprintln!("  {}", "Unclosed frontmatter in SKILL.md".yellow());
            }
        } else {
            eprintln!("  {}", "SKILL.md has no frontmatter".yellow());
        }
        Ok(())
    } else if json_path.exists() {
        let raw = std::fs::read_to_string(&json_path).map_err(|e| e.to_string())?;
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
        let pretty = serde_json::to_string_pretty(&value).unwrap_or(raw);
        eprintln!(
            "\n{}",
            format!("─── {name}/skill.json (legacy) ─────────────────────────────")
                .bold()
                .cyan()
        );
        for line in pretty.lines() {
            eprintln!("  {line}");
        }
        eprintln!();
        Ok(())
    } else {
        Err(format!(
            "\u{2717} No SKILL.md or skill.json in {}",
            skill_dir.display()
        ))
    }
}

// ── List filtering helpers ──────────────────────────────────────────────

fn source_label(source: &astra_skills::SkillSourceKind) -> &'static str {
    match source {
        astra_skills::SkillSourceKind::Local => "local",
        astra_skills::SkillSourceKind::Bundled => "bundled",
        astra_skills::SkillSourceKind::Mcp => "mcp",
        _ => "other",
    }
}

fn truncate_desc(desc: &str, max: usize) -> String {
    if desc.len() > max {
        let end = desc.floor_char_boundary(max);
        format!("{}\u{2026}", &desc[..end])
    } else {
        desc.to_string()
    }
}

/// Parse `/skill list` arguments into (free-text search, source filter, category filter).
/// Supports: `/skill list review`, `/skill list --source=local`, `/skill list --category=code-review`.
fn parse_list_filters(arg: &str) -> (Option<String>, Option<String>, Option<String>) {
    let mut search = Vec::new();
    let mut source = None;
    let mut category = None;

    for token in arg.split_whitespace() {
        if let Some(val) = token.strip_prefix("--source=") {
            source = Some(val.to_lowercase());
        } else if let Some(val) = token.strip_prefix("--category=") {
            category = Some(val.to_lowercase());
        } else {
            search.push(token);
        }
    }

    let query = if search.is_empty() {
        None
    } else {
        Some(search.join(" ").to_lowercase())
    };
    (query, source, category)
}

/// Check if a skill manifest matches the given filters.
fn matches_skill_filter(
    m: &astra_skills::SkillManifest,
    search: &Option<String>,
    source_filter: &Option<String>,
    category_filter: &Option<String>,
) -> bool {
    // Source filter
    if let Some(src) = source_filter {
        if source_label(&m.source) != src.as_str() {
            return false;
        }
    }

    // Category filter
    if let Some(cat) = category_filter {
        match &m.category {
            Some(c) if c.to_lowercase() == *cat => {}
            _ => return false,
        }
    }

    // Free-text search: match name, description, tags, or category
    if let Some(q) = search {
        let name_match = m.name.to_lowercase().contains(q.as_str());
        let desc_match = m.description.to_lowercase().contains(q.as_str());
        let tag_match = m.tags.iter().any(|t| t.to_lowercase().contains(q.as_str()));
        let cat_match = m
            .category
            .as_ref()
            .map(|c| c.to_lowercase().contains(q.as_str()))
            .unwrap_or(false);
        if !(name_match || desc_match || tag_match || cat_match) {
            return false;
        }
    }

    true
}

/// Score a skill's relevance to a search query.
/// Higher score = more relevant. Returns 0 if no match.
fn skill_relevance_score(m: &astra_skills::SkillManifest, query: &str) -> u32 {
    let mut score = 0u32;
    let words: Vec<&str> = query.split_whitespace().collect();

    // Exact name match (highest signal)
    if m.name.to_lowercase() == query {
        score += 20;
    } else if m.name.to_lowercase().contains(query) {
        score += 10;
    }

    // Word-level name matching
    for word in &words {
        if m.name.to_lowercase().contains(word) {
            score += 5;
        }
    }

    // Description matching
    let desc_lower = m.description.to_lowercase();
    if desc_lower.contains(query) {
        score += 6;
    } else {
        for word in &words {
            if desc_lower.contains(word) {
                score += 2;
            }
        }
    }

    // Tag matching (high signal — tags are curated)
    for tag in &m.tags {
        let tag_lower = tag.to_lowercase();
        if tag_lower == query {
            score += 8;
        } else if tag_lower.contains(query) || words.iter().any(|w| tag_lower.contains(w)) {
            score += 4;
        }
    }

    // Trigger matching
    for trigger in &m.triggers {
        let trig_lower = trigger.to_lowercase();
        if trig_lower.contains(query) || words.iter().any(|w| trig_lower.contains(w)) {
            score += 3;
        }
    }

    // when_to_use matching
    if let Some(ref wtu) = m.when_to_use {
        let wtu_lower = wtu.to_lowercase();
        if wtu_lower.contains(query) {
            score += 4;
        } else {
            for word in &words {
                if wtu_lower.contains(word) {
                    score += 1;
                }
            }
        }
    }

    // Category matching
    if let Some(ref cat) = m.category {
        if cat.to_lowercase().contains(query) {
            score += 5;
        }
    }

    score
}

// ═══════════════════════════════════════════════ Skill Auto-Generation ════

/// Analyze the current session and generate a SKILL.md from observed patterns.
async fn create_skill_from_session(arg: &str, state: &mut super::ReplState) -> Result<(), String> {
    use astra_services::session_journal;
    use std::collections::HashMap;

    let name = arg.split_whitespace().next().unwrap_or("").trim();
    if name.is_empty() {
        eprintln!("{}", "  Usage: /skill create <name>".yellow());
        eprintln!(
            "{}",
            "  Analyzes the current session and generates a skill from it.".dim()
        );
        return Ok(());
    }

    // Validate name (kebab-case)
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        eprintln!(
            "  {} Skill name must be alphanumeric, hyphens, or underscores.",
            theme::icon_err()
        );
        return Ok(());
    }

    // Check not duplicate
    let skills_base = std::env::current_dir()
        .map_err(|e| e.to_string())?
        .join(".astra/skills");
    let skill_dir = skills_base.join(name);
    if skill_dir.exists() {
        eprintln!(
            "  {} Skill '{}' already exists at {}",
            theme::icon_err(),
            name,
            skill_dir.display()
        );
        return Ok(());
    }

    // Read session journal
    let session_id = match &state.session_id {
        Some(s) => s.clone(),
        None => {
            eprintln!("  {} No active session to analyze.", theme::icon_err());
            return Ok(());
        }
    };

    let events = session_journal::read_journal(&session_id).map_err(|e| e.to_string())?;
    let turns: Vec<_> = events
        .iter()
        .filter(|e| matches!(e.event_type, session_journal::JournalEventType::Turn))
        .collect();

    if turns.is_empty() {
        eprintln!(
            "  {} No turns in current session to analyze.",
            theme::icon_warn()
        );
        return Ok(());
    }

    eprintln!(
        "  {} Analyzing {} turns from session {}...",
        theme::icon_info(),
        turns.len(),
        &session_id[..8.min(session_id.len())]
    );

    // ── Extract patterns ────────────────────────────────────────────────

    // 1. Tool frequency
    let mut tool_freq: HashMap<String, u32> = HashMap::new();
    let mut total_tool_calls = 0u32;
    for t in &turns {
        if let Some(ref tools) = t.tools_used {
            for tool in tools {
                *tool_freq.entry(tool.clone()).or_insert(0) += 1;
                total_tool_calls += 1;
            }
        }
    }

    // Sort by frequency, take top tools
    let mut tool_ranked: Vec<_> = tool_freq.into_iter().collect();
    tool_ranked.sort_by_key(|x| std::cmp::Reverse(x.1));
    let top_tools: Vec<String> = tool_ranked.iter().take(10).map(|t| t.0.clone()).collect();

    // 2. Collect user intents (first line of each user input)
    let mut user_intents: Vec<String> = Vec::new();
    for t in &turns {
        if let Some(ref input) = t.user_input {
            let first_line = input.lines().next().unwrap_or("").trim();
            if !first_line.is_empty() && first_line.len() < 200 {
                user_intents.push(first_line.to_string());
            }
        }
    }

    // 3. Skills already used
    let mut skills_used: Vec<String> = Vec::new();
    for t in &turns {
        if let Some(ref skills) = t.selected_skills {
            for s in skills {
                if !skills_used.contains(s) {
                    skills_used.push(s.clone());
                }
            }
        }
    }

    // 4. Estimate description from first user message
    let description = user_intents.first().cloned().unwrap_or_else(|| {
        format!(
            "Auto-generated skill from session {}",
            prefix_chars(&session_id, 8)
        )
    });

    // 5. Derive triggers from common words
    let triggers = derive_triggers(name, &user_intents);

    // ── Build steps from turn transcript ────────────────────────────────

    let mut steps = Vec::new();
    for (i, t) in turns.iter().enumerate() {
        let mut step = String::new();
        if let Some(ref input) = t.user_input {
            let preview = truncate_str(input, 120);
            step.push_str(&format!("User asked: {preview}"));
        }
        if let Some(ref tools) = t.tools_used {
            if !tools.is_empty() {
                step.push_str(&format!(" → Tools: {}", tools.join(", ")));
            }
        }
        if !step.is_empty() {
            steps.push(format!("{}. {step}", i + 1));
        }
    }

    // ── Generate SKILL.md ───────────────────────────────────────────────

    let allowed_tools_yaml = if top_tools.is_empty() {
        "allowed_tools: []".to_string()
    } else {
        let items: Vec<String> = top_tools.iter().map(|t| format!("  - {t}")).collect();
        format!("allowed_tools:\n{}", items.join("\n"))
    };

    let triggers_yaml = if triggers.is_empty() {
        "triggers: []".to_string()
    } else {
        let items: Vec<String> = triggers.iter().map(|t| format!("  - {t}")).collect();
        format!("triggers:\n{}", items.join("\n"))
    };

    let session_steps = if steps.is_empty() {
        "1. Understand the user's request\n2. Execute the task\n3. Report results".to_string()
    } else {
        steps.join("\n")
    };

    let skill_md = format!(
        r#"---
name: {name}
description: "{description}"
version: "0.1.0"
user_invocable: true
{triggers_yaml}
{allowed_tools_yaml}
when_to_use: "{description}"
# arguments:
#   - name: TARGET
#     description: "Target file or directory"
#     required: false
---

# {name}

Skill auto-generated from session {session_short}.
{total_tool_calls} tool calls across {turn_count} turns.

## Objective

{description}

## Steps

{session_steps}

## Tools Available

{tool_summary}

## Guidelines

- Follow the step sequence above, adapting to the specific request
- Use the allowed tools listed in the frontmatter
- Report progress and results clearly
"#,
        session_short = &session_id[..8.min(session_id.len())],
        turn_count = turns.len(),
        tool_summary = if top_tools.is_empty() {
            "All tools available.".to_string()
        } else {
            format!(
                "Primary tools (by frequency): {}",
                tool_ranked
                    .iter()
                    .take(5)
                    .map(|(n, c)| format!("{n} ({c}x)"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        },
    );

    // Write to disk
    std::fs::create_dir_all(&skill_dir).map_err(|e| e.to_string())?;
    std::fs::write(skill_dir.join("SKILL.md"), &skill_md).map_err(|e| e.to_string())?;

    // Summary output
    eprintln!(
        "\n  {} Skill '{}' created from session analysis",
        theme::icon_ok(),
        name.to_string().cyan()
    );
    eprintln!("  {}", format!("  Path: {}", skill_dir.display()).dim());
    eprintln!(
        "  {}",
        format!(
            "  Derived from: {} turns, {} tool calls",
            turns.len(),
            total_tool_calls
        )
        .dim()
    );
    if !top_tools.is_empty() {
        eprintln!(
            "  {}",
            format!(
                "  Top tools: {}",
                top_tools[..top_tools.len().min(5)].join(", ")
            )
            .dim()
        );
    }
    eprintln!(
        "\n  {}",
        format!("  Edit: {}/SKILL.md", skill_dir.display()).dim()
    );
    match state.unified_skill_registry.discover_all().await {
        Ok(_) => eprintln!("  {}", "  Skill registry refreshed.".dim()),
        Err(err) => eprintln!(
            "  {} {}",
            "Warning:".yellow(),
            format!("Skill registry refresh failed: {err}").dim()
        ),
    }
    eprintln!("  {}", format!("  Dev mode: /skill dev {name}").dim());
    eprintln!("  {}", format!("  Test: /skill test {name}").dim());
    eprintln!();

    Ok(())
}

/// Derive trigger words from user intents and the skill name.
pub(crate) fn derive_triggers(name: &str, intents: &[String]) -> Vec<String> {
    use std::collections::HashMap;

    let mut triggers = vec![name.to_string()];

    // Count words across intents (skip very common words)
    let stop_words: std::collections::HashSet<&str> = [
        "the", "a", "an", "to", "in", "for", "of", "and", "or", "is", "it", "on", "at", "by",
        "with", "this", "that", "from", "can", "do", "how", "what", "i", "me", "my", "we", "you",
        "your", "please", "let", "make", "use", "get", "set", "put", "all", "not", "no", "so",
        "if", "be", "as", "but", "are", "was", "were",
    ]
    .into_iter()
    .collect();

    let mut word_freq: HashMap<String, u32> = HashMap::new();
    for intent in intents {
        for word in intent.split_whitespace() {
            let w = word.to_lowercase();
            let w = w.trim_matches(|c: char| !c.is_alphanumeric());
            if w.len() >= 3 && !stop_words.contains(w) {
                *word_freq.entry(w.to_string()).or_insert(0) += 1;
            }
        }
    }

    // Take top 3 frequent words as triggers
    let mut ranked: Vec<_> = word_freq.into_iter().collect();
    ranked.sort_by_key(|x| std::cmp::Reverse(x.1));
    for (word, _) in ranked.iter().take(3) {
        if !triggers.contains(word) {
            triggers.push(word.clone());
        }
    }

    triggers
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── truncate_desc tests ────────────────────────────────────────────

    #[test]
    fn truncate_desc_ascii_short() {
        assert_eq!(truncate_desc("hello", 10), "hello");
    }

    #[test]
    fn truncate_desc_ascii_truncated() {
        let result = truncate_desc("hello world", 5);
        assert_eq!(result, "hello\u{2026}");
    }

    #[test]
    fn truncate_desc_cjk_boundary() {
        // Each CJK char is 3 bytes. Cutting at byte 36 would land inside '协'.
        let desc = "数据查询技能：通过 MySQL 协议连接";
        let result = truncate_desc(desc, 36);
        // Should not panic, and should end at a valid char boundary
        assert!(result.ends_with('\u{2026}'));
        assert!(result.len() <= 36 + "\u{2026}".len());
    }

    #[test]
    fn truncate_desc_exact_len() {
        let desc = "exact";
        assert_eq!(truncate_desc(desc, 5), "exact");
    }

    #[test]
    fn truncate_desc_empty() {
        assert_eq!(truncate_desc("", 10), "");
    }

    #[test]
    fn parse_list_filters_empty() {
        let (q, s, c) = parse_list_filters("");
        assert!(q.is_none());
        assert!(s.is_none());
        assert!(c.is_none());
    }

    #[test]
    fn parse_list_filters_text_only() {
        let (q, s, c) = parse_list_filters("review code");
        assert_eq!(q.as_deref(), Some("review code"));
        assert!(s.is_none());
        assert!(c.is_none());
    }

    #[test]
    fn parse_list_filters_flags() {
        let (q, s, c) = parse_list_filters("--source=local --category=testing");
        assert!(q.is_none());
        assert_eq!(s.as_deref(), Some("local"));
        assert_eq!(c.as_deref(), Some("testing"));
    }

    #[test]
    fn parse_list_filters_mixed() {
        let (q, s, c) = parse_list_filters("debug --source=bundled");
        assert_eq!(q.as_deref(), Some("debug"));
        assert_eq!(s.as_deref(), Some("bundled"));
        assert!(c.is_none());
    }

    #[test]
    fn parse_skill_surfacing_supports_status_and_updates() {
        assert_eq!(parse_skill_surfacing("").unwrap(), SkillSurfacingCmd::Show);
        assert_eq!(
            parse_skill_surfacing("status").unwrap(),
            SkillSurfacingCmd::Show
        );
        assert_eq!(
            parse_skill_surfacing("dynamic off").unwrap(),
            SkillSurfacingCmd::SetDynamic(false)
        );
        assert_eq!(
            parse_skill_surfacing("min 12").unwrap(),
            SkillSurfacingCmd::SetMinCatalog(12)
        );
        assert_eq!(
            parse_skill_surfacing("cap 20").unwrap(),
            SkillSurfacingCmd::SetSurfaceCap(20)
        );
    }

    #[test]
    fn apply_skill_surfacing_mutates_repl_state() {
        let mut state = ReplState::default();

        let (_, changed) = apply_skill_surfacing(&mut state, SkillSurfacingCmd::SetDynamic(false));
        assert!(changed);
        assert!(!state.skill_search.dynamic_surface);

        let (_, changed) = apply_skill_surfacing(&mut state, SkillSurfacingCmd::SetMinCatalog(11));
        assert!(changed);
        assert_eq!(state.skill_search.min_catalog_size, 11);

        let (_, changed) = apply_skill_surfacing(&mut state, SkillSurfacingCmd::SetSurfaceCap(19));
        assert!(changed);
        assert_eq!(state.skill_search.surface_cap, 19);

        let (_, changed) = apply_skill_surfacing(&mut state, SkillSurfacingCmd::Show);
        assert!(!changed);
    }

    #[test]
    fn matches_filter_no_filters() {
        let m = astra_skills::SkillManifest {
            name: "test-skill".into(),
            description: "A test skill".into(),
            ..Default::default()
        };
        assert!(matches_skill_filter(&m, &None, &None, &None));
    }

    #[test]
    fn matches_filter_by_name() {
        let m = astra_skills::SkillManifest {
            name: "pr-review".into(),
            description: "Review pull requests".into(),
            ..Default::default()
        };
        let q = Some("review".to_string());
        assert!(matches_skill_filter(&m, &q, &None, &None));

        let q2 = Some("deploy".to_string());
        assert!(!matches_skill_filter(&m, &q2, &None, &None));
    }

    #[test]
    fn matches_filter_by_source() {
        let m = astra_skills::SkillManifest {
            name: "debug".into(),
            source: astra_skills::SkillSourceKind::Bundled,
            ..Default::default()
        };
        let src = Some("bundled".to_string());
        assert!(matches_skill_filter(&m, &None, &src, &None));

        let src2 = Some("local".to_string());
        assert!(!matches_skill_filter(&m, &None, &src2, &None));
    }

    #[test]
    fn matches_filter_by_tag() {
        let m = astra_skills::SkillManifest {
            name: "security-scan".into(),
            tags: vec!["security".into(), "audit".into()],
            ..Default::default()
        };
        let q = Some("audit".to_string());
        assert!(matches_skill_filter(&m, &q, &None, &None));
    }

    #[test]
    fn relevance_score_exact_name_highest() {
        let m = astra_skills::SkillManifest {
            name: "debug".into(),
            description: "Debug issues".into(),
            ..Default::default()
        };
        let exact = skill_relevance_score(&m, "debug");
        let partial = skill_relevance_score(&m, "deb");
        assert!(exact > partial, "exact={exact} should > partial={partial}");
    }

    #[test]
    fn relevance_score_zero_for_no_match() {
        let m = astra_skills::SkillManifest {
            name: "debug".into(),
            description: "Debug issues".into(),
            ..Default::default()
        };
        assert_eq!(skill_relevance_score(&m, "deploy"), 0);
    }

    #[test]
    fn relevance_score_tag_match() {
        let m = astra_skills::SkillManifest {
            name: "security-scan".into(),
            tags: vec!["security".into(), "vulnerability".into()],
            ..Default::default()
        };
        assert!(skill_relevance_score(&m, "vulnerability") > 0);
    }

    #[test]
    fn relevance_score_multi_word_query() {
        let m = astra_skills::SkillManifest {
            name: "pr-review".into(),
            description: "Review pull requests for code quality".into(),
            ..Default::default()
        };
        let score = skill_relevance_score(&m, "code review");
        assert!(score > 0, "multi-word query should match description");
    }

    // ── derive_triggers tests ───────────────────────────────────────────

    #[test]
    fn derive_triggers_name_always_first() {
        let triggers = super::derive_triggers("my-skill", &[]);
        assert_eq!(triggers[0], "my-skill");
    }

    #[test]
    fn derive_triggers_empty_intents() {
        let triggers = super::derive_triggers("test", &[]);
        assert_eq!(triggers, vec!["test"]);
    }

    #[test]
    fn derive_triggers_extracts_frequent_words() {
        let intents = vec![
            "deploy the frontend app".to_string(),
            "deploy the backend api".to_string(),
            "deploy to production".to_string(),
        ];
        let triggers = super::derive_triggers("cleanup", &intents);
        assert_eq!(triggers[0], "cleanup");
        assert!(triggers.contains(&"deploy".to_string()));
    }

    #[test]
    fn derive_triggers_skips_stop_words() {
        let intents = vec![
            "the quick brown fox".to_string(),
            "the quick lazy dog".to_string(),
        ];
        let triggers = super::derive_triggers("x", &intents);
        // "the" is a stop word, should not appear
        assert!(!triggers.contains(&"the".to_string()));
    }

    #[test]
    fn derive_triggers_skips_short_words() {
        let intents = vec![
            "do it as we go on by".to_string(),
            "do it as we go on by".to_string(),
        ];
        let triggers = super::derive_triggers("x", &intents);
        // all words < 3 chars, no extra triggers beyond the name
        assert_eq!(triggers.len(), 1);
    }

    #[test]
    fn derive_triggers_max_three_extras() {
        let intents = vec![
            "deploy frontend backend infrastructure monitoring alerting".to_string(),
            "deploy frontend backend infrastructure monitoring alerting".to_string(),
        ];
        let triggers = super::derive_triggers("name", &intents);
        // name + at most 3 extras = 4 max
        assert!(
            triggers.len() <= 4,
            "got {}: {:?}",
            triggers.len(),
            triggers
        );
    }

    #[test]
    fn derive_triggers_no_duplicate_with_name() {
        let intents = vec!["deploy deploy deploy".to_string()];
        let triggers = super::derive_triggers("deploy", &intents);
        let count = triggers.iter().filter(|t| *t == "deploy").count();
        assert_eq!(count, 1, "name should not be duplicated");
    }

    // ── Skill name validation tests ─────────────────────────────────────

    #[test]
    fn skill_name_validation_accepts_valid() {
        for name in &["my-skill", "test_123", "ABC", "a"] {
            assert!(
                name.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
                "should accept: {name}"
            );
        }
    }

    #[test]
    fn skill_name_validation_rejects_invalid() {
        for name in &["my skill", "test/bad", "a@b", "skill;rm"] {
            assert!(
                !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
                "should reject: {name}"
            );
        }
    }

    // ── Marketplace integration tests (wiremock) ────────────────────────

    mod marketplace_tests {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        fn make_client(server_uri: &str) -> astra_thin_client::ThinClient {
            astra_thin_client::ThinClient::new(server_uri, None).unwrap()
        }

        #[tokio::test]
        async fn browse_marketplace_calls_search_endpoint() {
            let srv = MockServer::start().await;
            let resp = serde_json::json!({
                "results": [{
                    "skill_name": "code-review",
                    "version": "1.0.0",
                    "description": "Review code quality",
                    "publisher_id": null,
                    "trust_tier": "verified",
                    "category": "development",
                    "ranking_score": 0.8,
                    "avg_quality": 0.9,
                    "total_installs": 42,
                    "active_users_7d": 10,
                }],
                "total": 1,
                "limit": 20,
                "offset": 0,
            });

            Mock::given(method("GET"))
                .and(path("/marketplace/search"))
                .and(query_param("limit", "20"))
                .respond_with(ResponseTemplate::new(200).set_body_json(&resp))
                .expect(1)
                .mount(&srv)
                .await;

            let client = make_client(&srv.uri());
            super::browse_marketplace("", &client, Some("tok")).await;
            // Mock expectation validates the endpoint was called exactly once.
        }

        #[tokio::test]
        async fn browse_marketplace_with_category_filter() {
            let srv = MockServer::start().await;
            let resp = serde_json::json!({
                "results": [],
                "total": 0,
                "limit": 10,
                "offset": 0,
            });

            Mock::given(method("GET"))
                .and(path("/marketplace/search"))
                .and(query_param("category", "security"))
                .and(query_param("limit", "10"))
                .respond_with(ResponseTemplate::new(200).set_body_json(&resp))
                .expect(1)
                .mount(&srv)
                .await;

            let client = make_client(&srv.uri());
            super::browse_marketplace("security --limit=10", &client, Some("tok")).await;
        }

        #[tokio::test]
        async fn browse_marketplace_with_trust_filter() {
            let srv = MockServer::start().await;
            let resp = serde_json::json!({
                "results": [],
                "total": 0,
                "limit": 20,
                "offset": 0,
            });

            Mock::given(method("GET"))
                .and(path("/marketplace/search"))
                .and(query_param("trust_tier", "verified"))
                .respond_with(ResponseTemplate::new(200).set_body_json(&resp))
                .expect(1)
                .mount(&srv)
                .await;

            let client = make_client(&srv.uri());
            super::browse_marketplace("--trust=verified", &client, Some("tok")).await;
        }

        #[tokio::test]
        async fn browse_marketplace_handles_server_error() {
            let srv = MockServer::start().await;

            Mock::given(method("GET"))
                .and(path("/marketplace/search"))
                .respond_with(ResponseTemplate::new(500))
                .mount(&srv)
                .await;

            let client = make_client(&srv.uri());
            // Should not panic — gracefully prints error.
            super::browse_marketplace("", &client, Some("tok")).await;
        }

        #[tokio::test]
        async fn trending_marketplace_calls_search_endpoint() {
            let srv = MockServer::start().await;
            let resp = serde_json::json!({
                "results": [{
                    "skill_name": "hot-skill",
                    "version": "2.0.0",
                    "description": "Trending now",
                    "publisher_id": null,
                    "trust_tier": "bundled",
                    "category": "general",
                    "ranking_score": 0.95,
                    "avg_quality": 0.88,
                    "total_installs": 100,
                    "active_users_7d": 50,
                }],
                "total": 1,
                "limit": 15,
                "offset": 0,
            });

            Mock::given(method("GET"))
                .and(path("/marketplace/search"))
                .and(query_param("limit", "15"))
                .respond_with(ResponseTemplate::new(200).set_body_json(&resp))
                .expect(1)
                .mount(&srv)
                .await;

            let client = make_client(&srv.uri());
            super::trending_marketplace(&client, Some("tok")).await;
        }

        #[tokio::test]
        async fn list_installed_marketplace_calls_installed_endpoint() {
            let srv = MockServer::start().await;
            let resp = serde_json::json!({
                "installations": [{
                    "installation_id": "inst-001",
                    "skill_name": "debug-pro",
                    "skill_version": "1.2.0",
                    "status": "installed",
                    "installed_at": "2025-01-15T10:00:00Z",
                }],
                "total": 1,
                "limit": 50,
                "offset": 0,
            });

            Mock::given(method("GET"))
                .and(path("/marketplace/installed"))
                .and(query_param("limit", "50"))
                .respond_with(ResponseTemplate::new(200).set_body_json(&resp))
                .expect(1)
                .mount(&srv)
                .await;

            let client = make_client(&srv.uri());
            super::list_installed_marketplace(&client, Some("tok")).await;
        }

        #[tokio::test]
        async fn list_installed_marketplace_handles_empty() {
            let srv = MockServer::start().await;
            let resp = serde_json::json!({
                "installations": [],
                "total": 0,
                "limit": 50,
                "offset": 0,
            });

            Mock::given(method("GET"))
                .and(path("/marketplace/installed"))
                .respond_with(ResponseTemplate::new(200).set_body_json(&resp))
                .mount(&srv)
                .await;

            let client = make_client(&srv.uri());
            super::list_installed_marketplace(&client, Some("tok")).await;
        }

        #[tokio::test]
        async fn install_legacy_writes_skill_md() {
            let srv = MockServer::start().await;
            let record = serde_json::json!({
                "skill_id": "sk-001",
                "skill_name": "test-install-skill",
                "version": "1.0.0",
                "description": "A test skill",
                "metadata": {
                    "manifest": "---\nname: test-install-skill\nversion: 1.0.0\n---",
                    "instructions": "Do the thing."
                },
                "created_at": "2025-01-01T00:00:00Z",
            });

            Mock::given(method("GET"))
                .and(path("/skills/test-install-skill"))
                .respond_with(ResponseTemplate::new(200).set_body_json(&record))
                .expect(1)
                .mount(&srv)
                .await;

            let client = make_client(&srv.uri());
            let ok = super::install_single_skill_legacy("test-install-skill", None, &client, "tok")
                .await;

            assert!(ok, "install should succeed");

            // install_single_skill_legacy writes to cwd/.astra/skills/<name>
            let skill_dir = std::env::current_dir()
                .unwrap()
                .join(".astra/skills/test-install-skill");
            let skill_md = skill_dir.join("SKILL.md");
            assert!(skill_md.exists(), "SKILL.md should be written");
            let content = std::fs::read_to_string(&skill_md).unwrap();
            assert!(content.contains("test-install-skill"));
            assert!(content.contains("Do the thing."));

            // Clean up
            let _ = std::fs::remove_dir_all(&skill_dir);
        }

        #[tokio::test]
        async fn install_legacy_with_version_query() {
            let srv = MockServer::start().await;
            let record = serde_json::json!({
                "skill_id": "sk-002",
                "skill_name": "versioned-skill",
                "version": "2.0.0",
                "description": null,
                "metadata": {
                    "instructions": "Version 2 instructions."
                },
                "created_at": null,
            });

            Mock::given(method("GET"))
                .and(path("/skills/versioned-skill"))
                .and(query_param("version", "2.0.0"))
                .respond_with(ResponseTemplate::new(200).set_body_json(&record))
                .expect(1)
                .mount(&srv)
                .await;

            let client = make_client(&srv.uri());
            let ok = super::install_single_skill_legacy(
                "versioned-skill",
                Some("2.0.0"),
                &client,
                "tok",
            )
            .await;

            assert!(ok);

            // Clean up
            let skill_dir = std::env::current_dir()
                .unwrap()
                .join(".astra/skills/versioned-skill");
            let _ = std::fs::remove_dir_all(&skill_dir);
        }

        #[tokio::test]
        async fn install_legacy_returns_false_on_404() {
            let srv = MockServer::start().await;

            Mock::given(method("GET"))
                .and(path("/skills/missing-skill"))
                .respond_with(ResponseTemplate::new(404))
                .mount(&srv)
                .await;

            let client = make_client(&srv.uri());
            let ok =
                super::install_single_skill_legacy("missing-skill", None, &client, "tok").await;

            assert!(!ok, "install should fail on 404");
        }

        #[tokio::test]
        async fn uninstall_removes_skill_directory() {
            let tmp = tempfile::tempdir().unwrap();
            let skill_dir = tmp.path().join(".astra/skills/removable-skill");
            std::fs::create_dir_all(&skill_dir).unwrap();
            std::fs::write(skill_dir.join("SKILL.md"), "---\nname: removable\n---").unwrap();
            assert!(skill_dir.exists());

            let prev_dir = std::env::current_dir().unwrap();
            std::env::set_current_dir(tmp.path()).unwrap();

            // uninstall_skill_from_marketplace needs ReplState; test the core logic directly
            let target = std::env::current_dir()
                .unwrap()
                .join(".astra/skills/removable-skill");
            assert!(target.exists());
            std::fs::remove_dir_all(&target).unwrap();

            std::env::set_current_dir(&prev_dir).unwrap();
            assert!(!skill_dir.exists(), "skill dir should be removed");
        }

        // ── Versioning / upgrade tests ─────────────────────────────────

        #[test]
        fn read_local_skill_version_parses_frontmatter() {
            let tmp = tempfile::tempdir().unwrap();
            let skill_dir = tmp.path().join("my-skill");
            std::fs::create_dir_all(&skill_dir).unwrap();
            std::fs::write(
                skill_dir.join("SKILL.md"),
                "---\nname: my-skill\nversion: \"1.2.3\"\ndescription: test\n---\nInstructions",
            )
            .unwrap();

            // read_local_skill_version searches skill_search_paths() which won't find
            // our tempdir, so test the underlying parse_skill_md directly
            let content = std::fs::read_to_string(skill_dir.join("SKILL.md")).unwrap();
            let (manifest, _body) = astra_skills::loader::parse_skill_md(&content).unwrap();
            assert_eq!(manifest.version.to_string(), "1.2.3");
        }

        #[tokio::test]
        async fn fetch_marketplace_version_returns_latest() {
            let srv = MockServer::start().await;
            let resp = serde_json::json!({
                "results": [{
                    "skill_name": "my-skill",
                    "version": "1.0.0",
                    "description": null,
                    "publisher_id": null,
                    "trust_tier": null,
                    "category": null,
                    "ranking_score": 0.5,
                    "avg_quality": 0.0,
                    "total_installs": 0,
                    "active_users_7d": 0,
                }, {
                    "skill_name": "my-skill",
                    "version": "2.0.0",
                    "description": null,
                    "publisher_id": null,
                    "trust_tier": null,
                    "category": null,
                    "ranking_score": 0.4,
                    "avg_quality": 0.0,
                    "total_installs": 0,
                    "active_users_7d": 0,
                }, {
                    "skill_name": "my-skill",
                    "version": "1.5.0",
                    "description": null,
                    "publisher_id": null,
                    "trust_tier": null,
                    "category": null,
                    "ranking_score": 0.6,
                    "avg_quality": 0.0,
                    "total_installs": 0,
                    "active_users_7d": 0,
                }],
                "total": 3,
                "limit": 50,
                "offset": 0,
            });

            Mock::given(method("GET"))
                .and(path("/marketplace/search"))
                .and(query_param("name", "my-skill"))
                .and(query_param("limit", "50"))
                .respond_with(ResponseTemplate::new(200).set_body_json(&resp))
                .expect(1)
                .mount(&srv)
                .await;

            let client = make_client(&srv.uri());
            let ver = super::fetch_marketplace_version("my-skill", &client, "tok").await;
            assert_eq!(ver, Some("2.0.0".to_string()));
        }

        #[tokio::test]
        async fn fetch_marketplace_version_returns_none_on_empty() {
            let srv = MockServer::start().await;
            let resp = serde_json::json!({
                "results": [],
                "total": 0,
                "limit": 1,
                "offset": 0,
            });

            Mock::given(method("GET"))
                .and(path("/marketplace/search"))
                .respond_with(ResponseTemplate::new(200).set_body_json(&resp))
                .mount(&srv)
                .await;

            let client = make_client(&srv.uri());
            let ver = super::fetch_marketplace_version("nonexistent", &client, "tok").await;
            assert_eq!(ver, None);
        }

        #[tokio::test]
        async fn fetch_marketplace_version_returns_none_on_server_error() {
            let srv = MockServer::start().await;

            Mock::given(method("GET"))
                .and(path("/marketplace/search"))
                .respond_with(ResponseTemplate::new(500))
                .mount(&srv)
                .await;

            let client = make_client(&srv.uri());
            let ver = super::fetch_marketplace_version("some-skill", &client, "tok").await;
            assert_eq!(ver, None);
        }

        #[test]
        fn version_comparison_detects_upgrade_needed() {
            use astra_skills::version::Version;
            let local: Version = "1.0.0".parse().unwrap();
            let remote: Version = "1.1.0".parse().unwrap();
            assert!(remote > local);

            let same: Version = "1.0.0".parse().unwrap();
            assert!(same <= local);

            let older: Version = "0.9.0".parse().unwrap();
            assert!(local >= older);
        }

        #[test]
        fn version_comparison_handles_prerelease() {
            use astra_skills::version::Version;
            let release: Version = "2.0.0".parse().unwrap();
            let pre: Version = "2.0.0-beta".parse().unwrap();
            assert!(release > pre, "release should be greater than pre-release");
        }

        #[test]
        fn default_skill_category_falls_back_to_general() {
            assert_eq!(super::default_skill_category(None), "general");
            assert_eq!(super::default_skill_category(Some("")), "general");
            assert_eq!(super::default_skill_category(Some("   ")), "general");
            assert_eq!(
                super::default_skill_category(Some("automation")),
                "automation"
            );
        }
    }
}

/// Upload local quality metrics to the marketplace API (opt-in).
async fn upload_quality_report(
    api: &astra_thin_client::ThinClient,
    tracker: &astra_skills::quality::SkillQualityTracker,
    token: Option<&str>,
) {
    let entries = tracker.all_entries();
    if entries.is_empty() {
        eprintln!("  {}", "No quality data to upload.".dim());
        return;
    }

    let tok = token.unwrap_or("");
    let runtime_version = env!("CARGO_PKG_VERSION");
    let mut uploaded = 0u32;
    let mut failed = 0u32;

    for (name, entry) in entries {
        if entry.invocations < 2 {
            continue;
        }

        let report = serde_json::json!({
            "skill_name": name,
            "skill_version": "unknown",
            "runtime_version": runtime_version,
            "success_rate": entry.success_rate(),
            "avg_tokens": entry.avg_tokens(),
            "invocation_count": entry.invocations,
        });

        match api
            .post_bearer_path_json_text(tok, "/marketplace/quality-report", &report)
            .await
        {
            Ok(_) => uploaded += 1,
            Err(_) => failed += 1,
        }
    }

    if failed > 0 {
        eprintln!(
            "  {} {uploaded} uploaded, {failed} failed (marketplace may be offline)",
            "Quality upload:".dim()
        );
    } else if uploaded > 0 {
        eprintln!("  {} {uploaded} skill reports uploaded.", "✓".green());
    } else {
        eprintln!(
            "  {}",
            "No skills had enough data (min 2 invocations).".dim()
        );
    }
}

/// Upload quality on REPL exit — disabled (was opt-in via ASTRA_QUALITY_UPLOAD).
pub(super) async fn maybe_upload_quality_on_exit(
    _api: &astra_thin_client::ThinClient,
    _tracker: &astra_skills::quality::SkillQualityTracker,
    _token: Option<&str>,
) {
}

// ── Marketplace install/publish/uninstall ─────────────────────────────────

/// Install a skill from the marketplace into `.astra/skills/<name>/`.
async fn install_skill_from_marketplace(
    name: &str,
    api: &astra_thin_client::ThinClient,
    token: Option<&str>,
    state: &mut ReplState,
) {
    if name.is_empty() {
        eprintln!("{}", "  Usage: /skill install <name>[@version]".yellow());
        eprintln!(
            "{}",
            "  Downloads a skill from the marketplace to .astra/skills/.".dim()
        );
        return;
    }

    let tok = token.unwrap_or("");
    let mut installed_names: Vec<String> = Vec::new();
    let constraint = astra_skills::version::VersionConstraint::default(); // Any

    install_skill_recursive(name, &constraint, api, tok, state, &mut installed_names, 0).await;

    if installed_names.len() > 1 {
        eprintln!(
            "  {} Installed {} skills total: {}",
            "✓".green(),
            installed_names.len(),
            installed_names.join(", ").dim()
        );
    }
    eprintln!();
}

const MAX_DEP_INSTALL_DEPTH: u32 = 5;

/// Recursively install a skill and its dependencies, checking version constraints.
fn install_skill_recursive<'a>(
    name: &'a str,
    constraint: &'a astra_skills::version::VersionConstraint,
    api: &'a astra_thin_client::ThinClient,
    tok: &'a str,
    state: &'a mut ReplState,
    installed: &'a mut Vec<String>,
    depth: u32,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + 'a>> {
    Box::pin(async move {
        if depth > MAX_DEP_INSTALL_DEPTH {
            eprintln!(
                "  {} Dependency depth limit ({}) reached for '{}'",
                theme::icon_warn(),
                MAX_DEP_INSTALL_DEPTH,
                name.cyan()
            );
            return;
        }

        // Parse name@version (explicit version override takes precedence over constraint)
        let (skill_name, explicit_version) = if let Some(idx) = name.find('@') {
            (&name[..idx], Some(&name[idx + 1..]))
        } else {
            (name, None)
        };

        // Skip if already installed in this session (avoid cycles)
        if installed.iter().any(|n| n == skill_name) {
            return;
        }

        // Check if skill is already available locally and satisfies the constraint
        if depth > 0 {
            let all = state.unified_skill_registry.all_manifests();
            if let Some(existing) = all.iter().find(|m| m.name == skill_name) {
                if constraint.matches(&existing.version) {
                    return; // Already available and satisfies constraint
                }
                // Version constraint not satisfied — will re-install
                eprintln!(
                    "  {} '{}' v{} does not satisfy {}, upgrading…",
                    theme::icon_warn(),
                    skill_name.cyan(),
                    existing.version.to_string().dim(),
                    constraint.to_string().yellow()
                );
            }
        }

        let constraint_label = if constraint.is_any() {
            String::new()
        } else {
            format!(" ({})", constraint)
        };

        if depth == 0 {
            eprintln!(
                "  {} {}{}",
                "Installing".cyan(),
                skill_name.cyan().bold(),
                explicit_version
                    .map(|v| format!("@{v}"))
                    .unwrap_or(constraint_label)
                    .dim()
            );
        } else {
            eprintln!(
                "  {} {}{} (dependency)",
                "Installing".cyan(),
                skill_name.cyan(),
                constraint_label.dim()
            );
        }

        // Try bundle endpoint first, fall back to legacy JSON
        let success = install_single_skill(skill_name, explicit_version, api, tok, state).await;

        if success {
            installed.push(skill_name.to_string());

            // Refresh registry to pick up newly installed skill
            let _ = state.unified_skill_registry.discover_all().await;

            // Validate the installed version satisfies the constraint
            if !constraint.is_any() {
                let all = state.unified_skill_registry.all_manifests();
                if let Some(m) = all.iter().find(|m| m.name == skill_name) {
                    if !constraint.matches(&m.version) {
                        eprintln!(
                            "  {} Installed '{}' v{} does not satisfy constraint {}",
                            theme::icon_warn(),
                            skill_name.cyan(),
                            m.version.to_string().dim(),
                            constraint.to_string().yellow()
                        );
                    }
                }
            }

            // Check dependencies of the newly installed skill
            let deps = {
                let all = state.unified_skill_registry.all_manifests();
                all.iter()
                    .find(|m| m.name == skill_name)
                    .map(|m| m.dependencies.clone())
                    .unwrap_or_default()
            };

            let skill_deps: Vec<_> = deps
                .into_iter()
                .filter(|d| d.dep_type == astra_skills::version::DependencyType::Skill)
                .collect();

            if !skill_deps.is_empty() {
                eprintln!(
                    "  {} {} has {} dependencies",
                    "→".dim(),
                    skill_name.cyan(),
                    skill_deps.len()
                );

                for dep in &skill_deps {
                    install_skill_recursive(
                        &dep.name,
                        &dep.version,
                        api,
                        tok,
                        state,
                        installed,
                        depth + 1,
                    )
                    .await;
                }
            }
        }
    }) // close Box::pin(async move { ... })
}

/// Install a single skill (no dependency resolution). Returns true on success.
async fn install_single_skill(
    skill_name: &str,
    version: Option<&str>,
    api: &astra_thin_client::ThinClient,
    tok: &str,
    _state: &mut ReplState,
) -> bool {
    let bundle_path = format!("/skills/{}/bundle", skill_name);
    let query_pairs: Vec<(&str, String)> = if let Some(v) = version {
        vec![("version", v.to_string())]
    } else {
        vec![]
    };

    // Attempt bundle download (binary, base64-encoded)
    match api
        .get_bearer_path_query_text(tok, &bundle_path, &query_pairs)
        .await
    {
        Ok(text) => {
            if let Ok(bytes) =
                base64::Engine::decode(&base64::engine::general_purpose::STANDARD, text.trim())
            {
                let install_dir = std::env::current_dir()
                    .unwrap_or_default()
                    .join(".astra")
                    .join("skills");

                match astra_skills::pack::unpack_skill_from_bytes(&bytes, &install_dir) {
                    Ok((installed, manifest)) => {
                        eprintln!(
                            "  {} Installed {} v{} to {}",
                            "✓".green(),
                            manifest.name.cyan(),
                            manifest.version.dim(),
                            installed.display().to_string().dim()
                        );
                        return true;
                    }
                    Err(e) => {
                        eprintln!(
                            "  {} {}",
                            "Bundle unpack failed, trying legacy format...".yellow(),
                            format!("{e}").dim()
                        );
                    }
                }
            }
            // Fall through to legacy install
            install_single_skill_legacy(skill_name, version, api, tok).await
        }
        Err(_) => {
            // Bundle endpoint not available, use legacy
            install_single_skill_legacy(skill_name, version, api, tok).await
        }
    }
}

/// Legacy install: fetches SkillRecord JSON and writes SKILL.md directly. Returns true on success.
async fn install_single_skill_legacy(
    skill_name: &str,
    version: Option<&str>,
    api: &astra_thin_client::ThinClient,
    tok: &str,
) -> bool {
    let path = format!("/skills/{}", skill_name);
    let query_pairs: Vec<(&str, String)> = if let Some(v) = version {
        vec![("version", v.to_string())]
    } else {
        vec![]
    };

    match api
        .get_bearer_path_query_text(tok, &path, &query_pairs)
        .await
    {
        Ok(text) => match serde_json::from_str::<astra_services::skills::SkillRecord>(&text) {
            Ok(record) => {
                let instructions = record
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("instructions"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");

                let manifest_str = record
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("manifest"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");

                let install_dir = std::env::current_dir()
                    .unwrap_or_default()
                    .join(".astra")
                    .join("skills")
                    .join(skill_name);

                if let Err(e) = std::fs::create_dir_all(&install_dir) {
                    eprintln!("  {} {}", "✗ Failed to create directory:".red(), e);
                    return false;
                }

                let skill_md = if !manifest_str.is_empty() {
                    format!("{manifest_str}\n\n{instructions}")
                } else {
                    let header = format!(
                        "---\nname: {}\nversion: {}\ndescription: {}\n---\n\n",
                        record.skill_name,
                        record.version,
                        record.description.as_deref().unwrap_or(""),
                    );
                    format!("{header}{instructions}")
                };

                if let Err(e) = std::fs::write(install_dir.join("SKILL.md"), &skill_md) {
                    eprintln!("  {} {}", "✗ Failed to write SKILL.md:".red(), e);
                    return false;
                }

                eprintln!(
                    "  {} Installed {} v{} to {}",
                    "✓".green(),
                    record.skill_name.cyan(),
                    record.version.dim(),
                    install_dir.display().to_string().dim()
                );
                true
            }
            Err(e) => {
                eprintln!("  {} {}", "✗ Parse error:".yellow(), format!("{e}").dim());
                false
            }
        },
        Err(e) => {
            eprintln!(
                "  {} {}",
                "✗ Failed to fetch skill:".yellow(),
                format!("{e}").dim()
            );
            false
        }
    }
}

/// Publish a local skill to the marketplace (as a bundle).
async fn publish_skill_to_marketplace(
    name: &str,
    api: &astra_thin_client::ThinClient,
    token: Option<&str>,
    state: &ReplState,
) {
    if name.is_empty() {
        eprintln!("{}", "  Usage: /skill publish <name>".yellow());
        eprintln!(
            "{}",
            "  Publishes a local skill to the marketplace as a bundle.".dim()
        );
        return;
    }

    let tok = token.unwrap_or("");

    // Find the skill in local registry
    let registry = &state.unified_skill_registry;
    let all = registry.all_manifests();
    let manifest = all.iter().find(|m| m.name == name);
    let manifest = match manifest {
        Some(m) => m.clone(),
        None => {
            eprintln!(
                "  {} {}",
                "✗ Skill not found locally:".yellow(),
                name.cyan()
            );
            eprintln!(
                "  {}",
                "Tip: use '/skill list' to see available skills.".dim()
            );
            return;
        }
    };

    // Find skill directory to pack
    let search_paths = crate::skill_instructions::skill_search_paths();
    let mut skill_dir: Option<std::path::PathBuf> = None;
    for base in &search_paths {
        let candidate = base.join(name);
        if candidate.join("SKILL.md").exists() {
            skill_dir = Some(candidate);
            break;
        }
    }

    eprintln!(
        "  {} {} v{}...",
        "Publishing".cyan(),
        name.cyan().bold(),
        manifest.version.to_string().dim()
    );
    let category = default_skill_category(manifest.category.as_deref());

    // Try bundle publish if we have a local directory
    if let Some(ref dir) = skill_dir {
        match astra_skills::pack::pack_skill_to_bytes(dir) {
            Ok((bundle_bytes, bundle_manifest)) => {
                let encoded = base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    &bundle_bytes,
                );
                let request = serde_json::json!({
                    "name": bundle_manifest.name,
                    "version": bundle_manifest.version,
                    "description": bundle_manifest.description,
                    "category": category,
                    "tags": manifest.tags,
                    "bundle": encoded,
                    "bundle_sha256": bundle_manifest.skill_md_sha256,
                });

                match api
                    .post_bearer_path_json_text(tok, "/skills/publish", &request)
                    .await
                {
                    Ok(_) => {
                        eprintln!(
                            "  {} Published {} v{} ({} bundle)",
                            "✓".green(),
                            name.cyan(),
                            manifest.version.to_string().dim(),
                            format_bytes(bundle_bytes.len() as u64).dim()
                        );
                        eprintln!();
                        return;
                    }
                    Err(e) => {
                        eprintln!(
                            "  {} {}",
                            "✗ Bundle publish failed:".yellow(),
                            format!("{e}").dim()
                        );
                    }
                }
            }
            Err(e) => {
                eprintln!(
                    "  {} {}",
                    "✗ Bundle creation failed:".yellow(),
                    format!("{e}").dim()
                );
            }
        }
    }

    // Fallback: publish raw manifest + instructions
    let loaded = match registry.load(name).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("  {} {}", "✗ Failed to load skill:".red(), e);
            return;
        }
    };

    let request = serde_json::json!({
        "name": manifest.name,
        "version": manifest.version.to_string(),
        "description": manifest.description,
        "triggers": manifest.triggers,
        "dependencies": manifest.dependencies,
        "manifest": loaded.instructions,
        "category": category,
    });

    match api
        .post_bearer_path_json_text(tok, "/skills/publish", &request)
        .await
    {
        Ok(_) => {
            eprintln!(
                "  {} Published {} v{} to marketplace.",
                "✓".green(),
                name.cyan(),
                manifest.version.to_string().dim()
            );
        }
        Err(e) => {
            eprintln!(
                "  {} {}",
                "✗ Publish failed:".yellow(),
                format!("{e}").dim()
            );
        }
    }
    eprintln!();
}

/// Remove a locally installed skill.
async fn uninstall_local_skill(name: &str, state: &mut ReplState) {
    if name.is_empty() {
        eprintln!("{}", "  Usage: /skill uninstall <name>".yellow());
        eprintln!("{}", "  Removes a locally installed skill.".dim());
        return;
    }

    // Search for the skill in local paths
    let search_paths = crate::skill_instructions::skill_search_paths();
    let mut found_dir: Option<std::path::PathBuf> = None;

    for base in &search_paths {
        let candidate = base.join(name);
        if candidate.join("SKILL.md").exists() {
            found_dir = Some(candidate);
            break;
        }
    }

    match found_dir {
        Some(dir) => {
            use std::io::IsTerminal;
            if !std::io::stdin().is_terminal() {
                eprintln!(
                    "  {} {}",
                    theme::icon_warn(),
                    "Cannot confirm in non-interactive mode.".yellow()
                );
                return;
            }
            eprintln!(
                "  {} Remove skill '{}' from {}?",
                theme::icon_warn(),
                name.cyan(),
                dir.display().to_string().dim()
            );
            eprint!("  Confirm [y/N]: ");
            let _ = std::io::stderr().flush();
            let mut answer = String::new();
            if std::io::stdin().read_line(&mut answer).is_err()
                || !answer.trim().eq_ignore_ascii_case("y")
            {
                eprintln!("  {}", "Cancelled.".dim());
                return;
            }
            match std::fs::remove_dir_all(&dir) {
                Ok(()) => {
                    eprintln!(
                        "  {} Removed skill '{}' from {}",
                        "✓".green(),
                        name.cyan(),
                        dir.display().to_string().dim()
                    );
                    // Refresh registry
                    let _ = state.unified_skill_registry.discover_all().await;
                    eprintln!("  {}", "Skill registry refreshed.".dim());
                }
                Err(e) => {
                    eprintln!("  {} {}", "✗ Failed to remove:".red(), e);
                }
            }
        }
        None => {
            eprintln!(
                "  {} Skill '{}' not found in local paths.",
                "✗".yellow(),
                name.cyan()
            );
            eprintln!("  {}", "Searched:".dim());
            for p in &search_paths {
                eprintln!("    {}", p.display().to_string().dim());
            }
        }
    }
    eprintln!();
}

// ── Pack / Unpack / Inspect commands ────────────────────────────────────

/// Pack a local skill into a `.astra-skill` bundle.
fn pack_skill_bundle(name: &str) {
    if name.is_empty() {
        eprintln!("{}", "  Usage: /skill pack <name>".yellow());
        eprintln!(
            "{}",
            "  Bundles a local skill directory into a .astra-skill file.".dim()
        );
        return;
    }

    // Find the skill directory
    let search_paths = crate::skill_instructions::skill_search_paths();
    let mut skill_dir: Option<std::path::PathBuf> = None;

    for base in &search_paths {
        let candidate = base.join(name);
        if candidate.join("SKILL.md").exists() {
            skill_dir = Some(candidate);
            break;
        }
    }

    let skill_dir = match skill_dir {
        Some(d) => d,
        None => {
            eprintln!(
                "  {} Skill '{}' not found in local paths.",
                "✗".yellow(),
                name.cyan()
            );
            return;
        }
    };

    // Output to current directory
    let output_dir = std::env::current_dir().unwrap_or_default();

    match astra_skills::pack::pack_skill(&skill_dir, &output_dir) {
        Ok((path, manifest)) => {
            let size = std::fs::metadata(&path)
                .map(|m| format_bytes(m.len()))
                .unwrap_or_else(|_| "?".to_string());
            eprintln!(
                "  {} Packed {} v{} → {} ({})",
                "✓".green(),
                manifest.name.cyan(),
                manifest.version.dim(),
                path.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .cyan()
                    .bold(),
                size.dim()
            );
            eprintln!(
                "  {}",
                format!("SHA-256: {}", manifest.skill_md_sha256).dim()
            );
        }
        Err(e) => {
            eprintln!("  {} {}", "✗ Pack failed:".red(), e);
        }
    }
    eprintln!();
}

/// Unpack a `.astra-skill` bundle to local skills directory.
async fn unpack_skill_bundle(path_str: &str, state: &mut ReplState) {
    if path_str.is_empty() {
        eprintln!("{}", "  Usage: /skill unpack <file.astra-skill>".yellow());
        eprintln!("{}", "  Extracts a skill bundle to .astra/skills/.".dim());
        return;
    }

    let bundle_path = std::path::Path::new(path_str);
    if !bundle_path.exists() {
        eprintln!("  {} File not found: {}", "✗".yellow(), path_str.cyan());
        return;
    }

    let install_dir = std::env::current_dir()
        .unwrap_or_default()
        .join(".astra")
        .join("skills");

    match astra_skills::pack::unpack_skill(bundle_path, &install_dir) {
        Ok((installed, manifest)) => {
            eprintln!(
                "  {} Unpacked {} v{} to {}",
                "✓".green(),
                manifest.name.cyan(),
                manifest.version.dim(),
                installed.display().to_string().dim()
            );

            // Refresh registry
            let _ = state.unified_skill_registry.discover_all().await;
            eprintln!("  {}", "Skill registry refreshed.".dim());
        }
        Err(e) => {
            eprintln!("  {} {}", "✗ Unpack failed:".red(), e);
        }
    }
    eprintln!();
}

/// Inspect a `.astra-skill` bundle without extracting.
fn inspect_skill_bundle(path_str: &str) {
    if path_str.is_empty() {
        eprintln!("{}", "  Usage: /skill inspect <file.astra-skill>".yellow());
        eprintln!("{}", "  Shows bundle metadata without extracting.".dim());
        return;
    }

    let bundle_path = std::path::Path::new(path_str);
    if !bundle_path.exists() {
        eprintln!("  {} File not found: {}", "✗".yellow(), path_str.cyan());
        return;
    }

    match astra_skills::pack::inspect_bundle(bundle_path) {
        Ok(manifest) => {
            eprintln!("  {}", "Bundle contents:".bold());
            eprintln!("    Name:        {}", manifest.name.cyan());
            eprintln!("    Version:     {}", manifest.version.dim());
            eprintln!("    Description: {}", manifest.description);
            if let Some(ref author) = manifest.author {
                eprintln!("    Author:      {}", author);
            }
            if let Some(ref category) = manifest.category {
                eprintln!("    Category:    {}", category);
            }
            if !manifest.tags.is_empty() {
                eprintln!("    Tags:        {}", manifest.tags.join(", "));
            }
            eprintln!("    SHA-256:     {}", manifest.skill_md_sha256.dim());
        }
        Err(e) => {
            eprintln!("  {} {}", "✗ Inspect failed:".red(), e);
        }
    }
    eprintln!();
}

fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

// ── Upgrade / Rollback / Check-update commands ─────────────────────────

/// Read the local installed version of a skill from its SKILL.md frontmatter.
fn read_local_skill_version(
    skill_name: &str,
) -> Option<(std::path::PathBuf, astra_skills::version::Version)> {
    let search_paths = crate::skill_instructions::skill_search_paths();
    for base in &search_paths {
        let skill_md = base.join(skill_name).join("SKILL.md");
        if skill_md.exists() {
            if let Ok(content) = std::fs::read_to_string(&skill_md) {
                if let Ok((manifest, _)) = astra_skills::loader::parse_skill_md(&content) {
                    return Some((base.join(skill_name), manifest.version));
                }
            }
        }
    }
    None
}

/// Fetch the latest version string for a skill from the marketplace.
async fn fetch_marketplace_version(
    skill_name: &str,
    api: &astra_thin_client::ThinClient,
    tok: &str,
) -> Option<String> {
    let query_pairs = vec![
        ("name", skill_name.to_string()),
        ("limit", "50".to_string()),
    ];
    match api
        .get_bearer_path_query_text(tok, "/marketplace/search", &query_pairs)
        .await
    {
        Ok(text) => {
            if let Ok(resp) = serde_json::from_str::<
                astra_services::marketplace_stats::SkillSearchResponse,
            >(&text)
            {
                resp.results
                    .iter()
                    .filter_map(|result| {
                        result
                            .version
                            .parse::<astra_skills::version::Version>()
                            .ok()
                            .map(|version| (version, result.version.clone()))
                    })
                    .max_by(|(left, _), (right, _)| left.cmp(right))
                    .map(|(_, version)| version)
            } else {
                None
            }
        }
        Err(_) => None,
    }
}

/// `/skill check-update [name]` — compare installed vs marketplace latest.
async fn check_skill_updates(name: &str, api: &astra_thin_client::ThinClient, token: Option<&str>) {
    let tok = token.unwrap_or("");

    eprintln!("\n  {}", "Checking for updates…".bold());
    eprintln!("{}", "─".repeat(78).dim());

    // Collect skills to check
    let skills_to_check: Vec<String> = if name.is_empty() {
        // Check all locally installed skills
        let search_paths = crate::skill_instructions::skill_search_paths();
        let mut names = Vec::new();
        for base in &search_paths {
            if let Ok(entries) = std::fs::read_dir(base) {
                for entry in entries.flatten() {
                    if entry.path().join("SKILL.md").exists() {
                        if let Some(n) = entry.file_name().to_str() {
                            if !names.contains(&n.to_string()) {
                                names.push(n.to_string());
                            }
                        }
                    }
                }
            }
        }
        names.sort();
        names
    } else {
        vec![name.to_string()]
    };

    if skills_to_check.is_empty() {
        eprintln!("  {}", "No locally installed skills found.".dim());
        eprintln!();
        return;
    }

    let mut updates_available = 0u32;

    for skill_name in &skills_to_check {
        let skill_name = skill_name.as_str();
        let local = read_local_skill_version(skill_name);
        let remote = fetch_marketplace_version(skill_name, api, tok).await;

        match (local, remote) {
            (Some((_path, local_ver)), Some(remote_str)) => {
                match remote_str.parse::<astra_skills::version::Version>() {
                    Ok(remote_ver) => {
                        if remote_ver > local_ver {
                            eprintln!(
                                "  {} {} → {} {}",
                                skill_name.cyan(),
                                local_ver.to_string().dim(),
                                remote_ver.to_string().green(),
                                "(update available)".green()
                            );
                            updates_available += 1;
                        } else {
                            eprintln!(
                                "  {} {} {}",
                                skill_name.cyan(),
                                local_ver.to_string().dim(),
                                "(up to date)".dim()
                            );
                        }
                    }
                    Err(_) => {
                        eprintln!(
                            "  {} {} {}",
                            skill_name.cyan(),
                            local_ver.to_string().dim(),
                            format!("(marketplace version '{remote_str}' unparseable)").yellow()
                        );
                    }
                }
            }
            (Some((_path, local_ver)), None) => {
                eprintln!(
                    "  {} {} {}",
                    skill_name.cyan(),
                    local_ver.to_string().dim(),
                    "(not found in marketplace)".dim()
                );
            }
            (None, _) => {
                if !name.is_empty() {
                    eprintln!(
                        "  {} Skill '{}' not found locally.",
                        "✗".yellow(),
                        skill_name.cyan()
                    );
                }
            }
        }
    }

    if updates_available == 0 {
        eprintln!("\n  {}", "All skills are up to date.".dim());
    } else {
        eprintln!(
            "\n  {} {} update(s) available. Use '/skill upgrade <name>' to upgrade.",
            "ℹ".cyan(),
            updates_available
        );
    }
    eprintln!();
}

/// `/skill upgrade <name>` or `/skill upgrade --all` — upgrade to latest version.
async fn upgrade_skill(
    arg: &str,
    api: &astra_thin_client::ThinClient,
    token: Option<&str>,
    state: &mut ReplState,
) {
    let tok = token.unwrap_or("");

    if arg == "--all" {
        upgrade_all_skills(api, tok, state).await;
        return;
    }

    let skill_name = arg;
    if skill_name.is_empty() {
        eprintln!(
            "{}",
            "  Usage: /skill upgrade <name>  or  /skill upgrade --all".yellow()
        );
        return;
    }

    // Check local version
    let local = read_local_skill_version(skill_name);
    let (local_path, local_ver) = match local {
        Some((p, v)) => (p, v),
        None => {
            eprintln!(
                "  {} Skill '{}' not found locally. Use '/skill install {}' first.",
                "✗".yellow(),
                skill_name.cyan(),
                skill_name
            );
            eprintln!();
            return;
        }
    };

    // Fetch latest from marketplace
    let remote_str = match fetch_marketplace_version(skill_name, api, tok).await {
        Some(v) => v,
        None => {
            eprintln!(
                "  {} Skill '{}' not found in marketplace.",
                "✗".yellow(),
                skill_name.cyan()
            );
            eprintln!();
            return;
        }
    };

    let remote_ver = match remote_str.parse::<astra_skills::version::Version>() {
        Ok(v) => v,
        Err(_) => {
            eprintln!(
                "  {} Cannot parse marketplace version '{}'.",
                "✗".yellow(),
                remote_str
            );
            eprintln!();
            return;
        }
    };

    if remote_ver <= local_ver {
        eprintln!(
            "  {} '{}' is already at v{} (latest).",
            "✓".green(),
            skill_name.cyan(),
            local_ver
        );
        eprintln!();
        return;
    }

    eprintln!(
        "  {} Upgrading '{}': {} → {}",
        "⬆".cyan(),
        skill_name.cyan(),
        local_ver.to_string().dim(),
        remote_ver.to_string().green()
    );

    // Remove old and re-install latest
    let _ = std::fs::remove_dir_all(&local_path);
    let ok = install_single_skill(skill_name, None, api, tok, state).await;

    if ok {
        // Notify server
        let body = serde_json::json!({ "skill_name": skill_name });
        let _ = api
            .post_bearer_path_json_text(tok, "/marketplace/upgrade", &body)
            .await;

        // Refresh registry
        let _ = state.unified_skill_registry.discover_all().await;
        eprintln!("  {}", "Skill registry refreshed.".dim());
    }
    eprintln!();
}

/// Upgrade all installed marketplace skills.
async fn upgrade_all_skills(api: &astra_thin_client::ThinClient, tok: &str, state: &mut ReplState) {
    eprintln!(
        "\n  {}",
        "Checking all installed skills for updates…".bold()
    );
    eprintln!("{}", "─".repeat(78).dim());

    // Get installed list from server
    let installed = match api
        .get_bearer_path_query_text(
            tok,
            "/marketplace/installed",
            &[("limit", "200".to_string())],
        )
        .await
    {
        Ok(text) => {
            match serde_json::from_str::<astra_services::marketplace::InstalledListResponse>(&text)
            {
                Ok(resp) => resp.installations,
                Err(e) => {
                    eprintln!("  {} {}", "✗ Parse error:".yellow(), format!("{e}").dim());
                    return;
                }
            }
        }
        Err(e) => {
            eprintln!(
                "  {} {}",
                "✗ Server unavailable:".yellow(),
                format!("{e}").dim()
            );
            return;
        }
    };

    if installed.is_empty() {
        eprintln!("  {}", "No skills installed from marketplace.".dim());
        eprintln!();
        return;
    }

    let mut upgraded = 0u32;
    let mut up_to_date = 0u32;

    for inst in &installed {
        let skill_name = inst.skill_name.as_str();
        let local = read_local_skill_version(skill_name);

        let remote_str = match fetch_marketplace_version(skill_name, api, tok).await {
            Some(v) => v,
            None => continue,
        };

        let remote_ver = match remote_str.parse::<astra_skills::version::Version>() {
            Ok(v) => v,
            Err(_) => continue,
        };

        match local {
            Some((local_path, local_ver)) if remote_ver > local_ver => {
                eprintln!(
                    "  {} Upgrading '{}': {} → {}",
                    "⬆".cyan(),
                    skill_name.cyan(),
                    local_ver.to_string().dim(),
                    remote_ver.to_string().green()
                );
                let _ = std::fs::remove_dir_all(&local_path);
                let ok = install_single_skill(skill_name, None, api, tok, state).await;
                if ok {
                    let body = serde_json::json!({ "skill_name": skill_name });
                    let _ = api
                        .post_bearer_path_json_text(tok, "/marketplace/upgrade", &body)
                        .await;
                    upgraded += 1;
                }
            }
            Some(_) => {
                up_to_date += 1;
            }
            None => {
                // Installed on server but not locally — skip
                up_to_date += 1;
            }
        }
    }

    if upgraded > 0 {
        // Refresh registry once after all upgrades
        let _ = state.unified_skill_registry.discover_all().await;
        eprintln!("  {}", "Skill registry refreshed.".dim());
    }

    eprintln!(
        "\n  {} {} upgraded, {} up to date",
        "Done:".bold(),
        upgraded.to_string().green(),
        up_to_date.to_string().dim()
    );
    eprintln!();
}

/// `/skill rollback <name>` — revert to previous version.
async fn rollback_skill(
    name: &str,
    api: &astra_thin_client::ThinClient,
    token: Option<&str>,
    state: &mut ReplState,
) {
    if name.is_empty() {
        eprintln!("{}", "  Usage: /skill rollback <name>".yellow());
        eprintln!("{}", "  Reverts a skill to its previous version.".dim());
        return;
    }

    let tok = token.unwrap_or("");

    // Call server rollback to get previous version
    let body = serde_json::json!({ "skill_name": name });
    let resp = match api
        .post_bearer_path_json_text(tok, "/marketplace/rollback", &body)
        .await
    {
        Ok(text) => {
            match serde_json::from_str::<astra_services::marketplace::InstallationResponse>(&text) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("  {} {}", "✗ Parse error:".yellow(), format!("{e}").dim());
                    eprintln!();
                    return;
                }
            }
        }
        Err(e) => {
            let msg = format!("{e}");
            if msg.contains("404") || msg.contains("not installed") || msg.contains("Not Found") {
                eprintln!(
                    "  {} Skill '{}' is not installed on the server.",
                    "✗".yellow(),
                    name.cyan()
                );
            } else if msg.contains("400") || msg.contains("No previous version") {
                eprintln!(
                    "  {} No previous version to rollback to for '{}'.",
                    "✗".yellow(),
                    name.cyan()
                );
            } else {
                eprintln!("  {} {}", "✗ Rollback failed:".yellow(), msg.dim());
            }
            eprintln!();
            return;
        }
    };

    let target_version = &resp.skill_version;
    eprintln!(
        "  {} Rolling back '{}' to v{}",
        "⬇".cyan(),
        name.cyan(),
        target_version.as_str().dim()
    );

    // Remove local copy and re-install the specific version
    let search_paths = crate::skill_instructions::skill_search_paths();
    for base in &search_paths {
        let skill_dir = base.join(name);
        if skill_dir.exists() {
            let _ = std::fs::remove_dir_all(&skill_dir);
            break;
        }
    }

    let ok = install_single_skill(name, Some(target_version.as_str()), api, tok, state).await;

    if ok {
        let _ = state.unified_skill_registry.discover_all().await;
        eprintln!("  {}", "Skill registry refreshed.".dim());
    }
    eprintln!();
}

// ── Browse / Trending / Installed commands ──────────────────────────────

/// Browse marketplace by category.
async fn browse_marketplace(
    category: &str,
    api: &astra_thin_client::ThinClient,
    token: Option<&str>,
) {
    let tok = token.unwrap_or("");

    // Parse args: optional category + flags
    let mut cat_filter = None;
    let mut trust_filter = None;
    let mut limit = 20u32;

    for part in category.split_whitespace() {
        if let Some(val) = part.strip_prefix("--trust=") {
            trust_filter = Some(val.to_string());
        } else if let Some(val) = part.strip_prefix("--limit=") {
            limit = val.parse().unwrap_or(20).min(100);
        } else if cat_filter.is_none() {
            cat_filter = Some(part.to_string());
        }
    }

    let title = cat_filter
        .as_deref()
        .map(|c| format!("Browse: {c}"))
        .unwrap_or_else(|| "Browse marketplace".to_string());

    eprintln!("\n  {}", title.bold());
    eprintln!("{}", "─".repeat(78).dim());

    let mut query_pairs: Vec<(&str, String)> = vec![("limit", limit.to_string())];
    if let Some(ref c) = cat_filter {
        query_pairs.push(("category", c.clone()));
    }
    if let Some(ref t) = trust_filter {
        query_pairs.push(("trust_tier", t.clone()));
    }

    match api
        .get_bearer_path_query_text(tok, "/marketplace/search", &query_pairs)
        .await
    {
        Ok(text) => {
            match serde_json::from_str::<astra_services::marketplace_stats::SkillSearchResponse>(
                &text,
            ) {
                Ok(resp) => {
                    if resp.results.is_empty() {
                        eprintln!("  {}", "No skills found.".dim());
                    } else {
                        eprintln!(
                            "  {:<24}  {:<8}  {:<12}  {:<10}  {:<6}  {}",
                            "Name".bold(),
                            "Version".bold(),
                            "Category".bold(),
                            "Trust".bold(),
                            "Installs".bold(),
                            "Description".bold()
                        );
                        for r in &resp.results {
                            let cat = r.category.as_deref().unwrap_or("-");
                            let tier = r.trust_tier.as_deref().unwrap_or("?");
                            let desc = truncate_desc(r.description.as_deref().unwrap_or(""), 28);
                            eprintln!(
                                "  {:<24}  {:<8}  {:<12}  {:<10}  {:<6}  {}",
                                r.skill_name.as_str().cyan(),
                                r.version.as_str().dim(),
                                cat.dim(),
                                tier.dim(),
                                r.total_installs,
                                desc
                            );
                        }
                        eprintln!(
                            "\n  {} {} total",
                            "Showing".dim(),
                            resp.total.to_string().dim()
                        );
                    }
                }
                Err(e) => {
                    eprintln!("  {} {}", "✗ Parse error:".yellow(), format!("{e}").dim());
                }
            }
        }
        Err(e) => {
            eprintln!(
                "  {} {}",
                "✗ Marketplace unavailable:".yellow(),
                format!("{e}").dim()
            );
        }
    }

    if cat_filter.is_none() {
        eprintln!();
        eprintln!(
            "  {}",
            "Tip: /skill browse <category> to filter (e.g. code-review, deployment, analysis)."
                .dim()
        );
    }
    eprintln!();
}

/// Show trending skills from the marketplace (sorted by ranking score).
async fn trending_marketplace(api: &astra_thin_client::ThinClient, token: Option<&str>) {
    let tok = token.unwrap_or("");

    eprintln!("\n  {}", "🔥 Trending skills".bold());
    eprintln!("{}", "─".repeat(78).dim());

    match api
        .get_bearer_path_query_text(tok, "/marketplace/search", &[("limit", "15".to_string())])
        .await
    {
        Ok(text) => {
            match serde_json::from_str::<astra_services::marketplace_stats::SkillSearchResponse>(
                &text,
            ) {
                Ok(resp) => {
                    if resp.results.is_empty() {
                        eprintln!("  {}", "No skills in marketplace yet.".dim());
                    } else {
                        eprintln!(
                            "  {:<3}  {:<22}  {:<8}  {:<6}  {:<8}  {:<7}  {}",
                            "#".bold(),
                            "Name".bold(),
                            "Version".bold(),
                            "Score".bold(),
                            "Installs".bold(),
                            "Active".bold(),
                            "Description".bold()
                        );
                        for (i, r) in resp.results.iter().enumerate() {
                            let desc = truncate_desc(r.description.as_deref().unwrap_or(""), 26);
                            let rank_color = if i < 3 {
                                format!("{:<3}", i + 1).yellow().bold().to_string()
                            } else {
                                format!("{:<3}", i + 1)
                            };
                            eprintln!(
                                "  {}  {:<22}  {:<8}  {:<6.2}  {:<8}  {:<7}  {}",
                                rank_color,
                                r.skill_name.as_str().cyan(),
                                r.version.as_str().dim(),
                                r.ranking_score,
                                r.total_installs,
                                r.active_users_7d,
                                desc
                            );
                        }
                    }
                }
                Err(e) => {
                    eprintln!("  {} {}", "✗ Parse error:".yellow(), format!("{e}").dim());
                }
            }
        }
        Err(e) => {
            eprintln!(
                "  {} {}",
                "✗ Marketplace unavailable:".yellow(),
                format!("{e}").dim()
            );
        }
    }
    eprintln!();
}

/// Show skills installed on the server for the current user.
async fn list_installed_marketplace(api: &astra_thin_client::ThinClient, token: Option<&str>) {
    let tok = token.unwrap_or("");

    eprintln!("\n  {}", "Installed skills (server)".bold());
    eprintln!("{}", "─".repeat(78).dim());

    match api
        .get_bearer_path_query_text(
            tok,
            "/marketplace/installed",
            &[("limit", "50".to_string())],
        )
        .await
    {
        Ok(text) => {
            match serde_json::from_str::<astra_services::marketplace::InstalledListResponse>(&text)
            {
                Ok(resp) => {
                    if resp.installations.is_empty() {
                        eprintln!("  {}", "No skills installed from marketplace.".dim());
                        eprintln!(
                            "  {}",
                            "Tip: use '/skill install <name>' to install from marketplace.".dim()
                        );
                    } else {
                        eprintln!(
                            "  {:<24}  {:<10}  {:<12}  {}",
                            "Name".bold(),
                            "Version".bold(),
                            "Status".bold(),
                            "Installed".bold()
                        );
                        for inst in &resp.installations {
                            let status_colored = match inst.status.as_str() {
                                "installed" => inst.status.as_str().green().to_string(),
                                "upgraded" => inst.status.as_str().cyan().to_string(),
                                "rolled_back" => inst.status.as_str().yellow().to_string(),
                                _ => inst.status.clone(),
                            };
                            eprintln!(
                                "  {:<24}  {:<10}  {:<12}  {}",
                                inst.skill_name.as_str().cyan(),
                                inst.skill_version.as_str().dim(),
                                status_colored,
                                inst.installed_at.as_str().dim()
                            );
                        }
                        eprintln!(
                            "\n  {} {} total",
                            "Installed:".dim(),
                            resp.total.to_string().dim()
                        );
                    }
                }
                Err(e) => {
                    eprintln!("  {} {}", "✗ Parse error:".yellow(), format!("{e}").dim());
                }
            }
        }
        Err(e) => {
            eprintln!(
                "  {} {}",
                "✗ Server unavailable:".yellow(),
                format!("{e}").dim()
            );
            eprintln!(
                "  {}",
                "Tip: use '/skill list' to see locally available skills.".dim()
            );
        }
    }
    eprintln!();
}
