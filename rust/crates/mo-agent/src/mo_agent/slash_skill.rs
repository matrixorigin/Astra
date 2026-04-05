use super::*;

pub(super) async fn handle_skill_command(
    arg: &str,
    api: &astra_thin_client::ThinClient,
    state: &mut ReplState,
    token: Option<&str>,
) -> Result<(), String> {
    // Parse subcommand and remaining args from `arg`
    let mut sub_parts = arg.splitn(2, ' ');
    let sub = sub_parts.next().unwrap_or("").trim();
    let sub_arg = sub_parts.next().unwrap_or("").trim();

    // Route based on subcommand
    match sub {
        "" | "list" => {
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
                .filter(|m| m.source == astra_runtime::skills::SkillSourceKind::Local)
                .count();
            let bundled_count = manifests
                .iter()
                .filter(|m| m.source == astra_runtime::skills::SkillSourceKind::Bundled)
                .count();
            let mcp_count = manifests
                .iter()
                .filter(|m| m.source == astra_runtime::skills::SkillSourceKind::Mcp)
                .count();
            let mut parts = vec![
                format!("{} local", local_count),
                format!("{} bundled", bundled_count),
            ];
            if mcp_count > 0 {
                parts.push(format!("{} mcp", mcp_count));
            }
            parts.push(format!("{} total", manifests.len()));
            eprintln!("\n  {}", parts.join(", "));
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
            scored.sort_by(|a, b| b.1.cmp(&a.1));

            eprintln!("\n  {} '{}'", "Search results for".dim(), query.cyan());
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
                eprintln!("\n  {} results (showing top 10)", scored.len());
            }
            eprintln!();
        }

        "info" => {
            let name = sub_arg.trim();
            if name.is_empty() {
                eprintln!("{}", "  Usage: /skill info <name>".yellow());
                return Ok(());
            }
            let registry = &state.unified_skill_registry;
            match registry.get_manifest(name) {
                None => {
                    eprintln!("  {}", format!("✗ Skill '{name}' not found").yellow());
                    // Suggest similar names
                    let all = registry.skill_names();
                    let suggestions: Vec<_> = all
                        .iter()
                        .filter(|n| n.contains(name) || name.contains(n.as_str()))
                        .take(5)
                        .collect();
                    if !suggestions.is_empty() {
                        eprintln!(
                            "  {}",
                            format!(
                                "Did you mean: {}?",
                                suggestions
                                    .iter()
                                    .map(|s| s.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            )
                            .dim()
                        );
                    }
                }
                Some(m) => {
                    eprintln!("\n{}", format!("── {} ──", m.name).bold());
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
                    if let Some(loaded) = registry.get_loaded_skill(name) {
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
            eprintln!("  Files created: SKILL.md");
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
                format!("─── Skill test: {name} ───────────────────────────────────────").bold()
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
                        eprintln!("  {body}");
                        true
                    }
                    Err(_) => false,
                }
            } else {
                false
            };

            if !api_ok {
                let skill_dir = std::env::current_dir()
                    .map_err(|e| e.to_string())?
                    .join(".astra/skills")
                    .join(name);
                let skill_md = skill_dir.join("SKILL.md");
                let test_file = skill_dir.join("test_skill.py");

                if skill_md.exists() {
                    eprintln!("  Validating SKILL.md...");
                    let src = std::fs::read_to_string(&skill_md).map_err(|e| e.to_string())?;
                    let mut ok = true;
                    if !src.starts_with("---") {
                        eprintln!("  {}", "\u{2717} Missing frontmatter".red());
                        ok = false;
                    } else if let Some(end) = src[3..].find("\n---") {
                        let yaml = &src[3..3 + end];
                        match serde_yaml::from_str::<serde_json::Value>(yaml) {
                            Ok(val) => {
                                let sname = val.get("name").and_then(|v| v.as_str()).unwrap_or("");
                                if sname.is_empty() {
                                    eprintln!("  {}", "\u{2717} Frontmatter `name` is empty".red());
                                    ok = false;
                                }
                                eprintln!("  Manifest name: {}", sname.cyan());
                            }
                            Err(e) => {
                                eprintln!("  {}", format!("\u{2717} Invalid YAML: {e}").red());
                                ok = false;
                            }
                        }
                        let body = &src[3 + end + 4..];
                        if body.trim().is_empty() {
                            eprintln!("  {}", "\u{2717} Empty instruction body".red());
                            ok = false;
                        } else {
                            eprintln!("  Instruction body: {} chars", body.len());
                        }
                    } else {
                        eprintln!("  {}", "\u{2717} Unclosed frontmatter".red());
                        ok = false;
                    }

                    if let Ok((manifest, _body)) =
                        astra_runtime::skills::loader::parse_skill_md(&src)
                    {
                        if let Some(ref hooks) = manifest.hooks {
                            if !hooks.pre_invoke.is_empty() {
                                eprintln!("  Running pre_invoke hooks...");
                                for action in &hooks.pre_invoke {
                                    if let astra_runtime::skills::hooks::HookAction::Shell {
                                        command,
                                    } = action
                                    {
                                        eprintln!("  $ {command}");
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
                state.skill_dev_name = None;
                state.skill_dev_dir = None;
                state.skill_dev_context = None;
                eprintln!("  {}", "Exited skill dev mode".green());
                return Ok(());
            }
            let name = sub_arg;
            if name.is_empty() {
                if let Some(ref current) = state.skill_dev_name.clone() {
                    eprintln!(
                        "  \u{1f527} Currently in skill dev mode: {}",
                        current.as_str().cyan()
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
            let skill_dir = std::env::current_dir()
                .map_err(|e| e.to_string())?
                .join(".astra/skills")
                .join(name);
            let skill_md_path = skill_dir.join("SKILL.md");
            // Fall back to legacy skill.py for backward compat
            let skill_py_path = skill_dir.join("skill.py");
            let (src_path, src_label) = if skill_md_path.exists() {
                (skill_md_path, "SKILL.md")
            } else if skill_py_path.exists() {
                (skill_py_path, "skill.py (legacy)")
            } else {
                eprintln!(
                    "{}",
                    format!(
                        "  \u{2717} SKILL.md not found in {}. Use /skill new {name} to scaffold.",
                        skill_dir.display()
                    )
                    .yellow()
                );
                return Ok(());
            };
            let skill_src = std::fs::read_to_string(&src_path).map_err(|e| e.to_string())?;
            state.skill_dev_name = Some(name.to_string());
            state.skill_dev_dir = Some(skill_dir.display().to_string());
            state.skill_dev_context = Some(skill_src);
            eprintln!(
                "\n  \u{1f527} {} {}",
                "Skill dev mode:".bold(),
                name.cyan().bold()
            );
            eprintln!("  {}", format!("Dir: {}", skill_dir.display()).dim());
            eprintln!("  {}", format!("Source: {src_label}").dim());
            eprintln!(
                "  {}",
                "Skill source is injected into each turn. Ask me to improve it.".dim()
            );
            eprintln!("  {}", "Exit: /skill dev off".dim());
            eprintln!();
        }

        "doctor" => {
            eprintln!(
                "\n{}",
                "─── Skill Health ──────────────────────────────────────────────".bold()
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
                let skills_base = std::env::current_dir()
                    .map_err(|e| e.to_string())?
                    .join(".astra/skills");
                if !skills_base.exists() {
                    eprintln!(
                        "  {}",
                        "No local skills found (.astra/skills/ does not exist).".dim()
                    );
                    return Ok(());
                }
                eprintln!(
                    "{}",
                    format!("{:<28}  {:<12}  {}", "Name", "SKILL.md", "Format").bold()
                );
                eprintln!("{}", "\u{2500}".repeat(60).dim());
                let entries = std::fs::read_dir(&skills_base).map_err(|e| e.to_string())?;
                let mut found = false;
                for entry in entries.flatten() {
                    if entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
                        let name = entry.file_name().to_string_lossy().to_string();
                        let has_skill_md = entry.path().join("SKILL.md").exists();
                        let has_legacy = entry.path().join("skill.py").exists();
                        let md_s = if has_skill_md {
                            "\u{2713}".green().to_string()
                        } else {
                            "\u{2717} missing".red().to_string()
                        };
                        let format_s = if has_skill_md {
                            "unified"
                        } else if has_legacy {
                            "legacy (skill.py)"
                        } else {
                            "unknown"
                        };
                        eprintln!("  {:<26}  {:<12}  {}", name.cyan(), md_s, format_s.dim());
                        found = true;
                    }
                }
                if !found {
                    eprintln!("  {}", "No skill directories found.".dim());
                }
                eprintln!();
            }
        }

        "validate" => {
            let name = sub_arg;
            if name.is_empty() {
                eprintln!("{}", "  Usage: /skill validate <name>".yellow());
                return Ok(());
            }
            let skill_dir = std::env::current_dir()
                .map_err(|e| e.to_string())?
                .join(".astra/skills")
                .join(name);
            let skill_md_path = skill_dir.join("SKILL.md");
            if !skill_md_path.exists() {
                eprintln!(
                    "{}",
                    format!("  \u{2717} SKILL.md not found in {}", skill_dir.display()).red()
                );
                return Ok(());
            }
            let src = std::fs::read_to_string(&skill_md_path).map_err(|e| e.to_string())?;
            let mut issues: Vec<String> = Vec::new();

            // Validate YAML frontmatter
            if !src.starts_with("---") {
                issues.push("missing YAML frontmatter (must start with ---)".to_string());
            } else if let Some(end) = src[3..].find("\n---") {
                let yaml_block = &src[3..3 + end];
                match serde_yaml::from_str::<serde_json::Value>(yaml_block) {
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
                    issues
                        .push("instruction body is empty (content after frontmatter)".to_string());
                }
            } else {
                issues.push("unclosed frontmatter (missing closing ---)".to_string());
            }

            if issues.is_empty() {
                eprintln!(
                    "  {} {}",
                    "\u{2713}".green(),
                    format!("{name} looks valid").green()
                );
            } else {
                eprintln!("  {} {} issue(s):", "\u{2717}".red(), issues.len());
                for issue in &issues {
                    eprintln!("    - {}", issue.as_str().yellow());
                }
            }
        }

        "config" => {
            let name = sub_arg;
            if name.is_empty() {
                eprintln!("{}", "  Usage: /skill config <name>".yellow());
                return Ok(());
            }
            let skill_dir = std::env::current_dir()
                .map_err(|e| e.to_string())?
                .join(".astra/skills")
                .join(name);
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
            } else if json_path.exists() {
                let raw = std::fs::read_to_string(&json_path).map_err(|e| e.to_string())?;
                let value: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
                let pretty = serde_json::to_string_pretty(&value).unwrap_or(raw);
                eprintln!(
                    "\n{}",
                    format!("─── {name}/skill.json (legacy) ─────────────────────────────").bold()
                );
                for line in pretty.lines() {
                    eprintln!("  {line}");
                }
                eprintln!();
            } else {
                eprintln!(
                    "{}",
                    format!(
                        "  \u{2717} No SKILL.md or skill.json found in {}",
                        skill_dir.display()
                    )
                    .red()
                );
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
                        "Tip: use '/skill search' for local-only search.".dim()
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

        _ => {
            eprintln!(
                        "{}",
                        format!("  Unknown /skill subcommand: '{sub}'. Try /skill list, /skill search, /skill search-remote, /skill info, /skill new, /skill test, /skill dev, /skill doctor, /skill stats, /skill pin, /skill unpin, /skill compose-info, /skill upload-quality").yellow()
                    );
        }
    }
    Ok(())
}

// ── List filtering helpers ──────────────────────────────────────────────

fn source_label(source: &astra_runtime::skills::SkillSourceKind) -> &'static str {
    match source {
        astra_runtime::skills::SkillSourceKind::Local => "local",
        astra_runtime::skills::SkillSourceKind::Bundled => "bundled",
        astra_runtime::skills::SkillSourceKind::Mcp => "mcp",
        _ => "other",
    }
}

fn truncate_desc(desc: &str, max: usize) -> String {
    if desc.len() > max {
        format!("{}\u{2026}", &desc[..max])
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
    m: &astra_runtime::skills::SkillManifest,
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
fn skill_relevance_score(m: &astra_runtime::skills::SkillManifest, query: &str) -> u32 {
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn matches_filter_no_filters() {
        let m = astra_runtime::skills::SkillManifest {
            name: "test-skill".into(),
            description: "A test skill".into(),
            ..Default::default()
        };
        assert!(matches_skill_filter(&m, &None, &None, &None));
    }

    #[test]
    fn matches_filter_by_name() {
        let m = astra_runtime::skills::SkillManifest {
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
        let m = astra_runtime::skills::SkillManifest {
            name: "debug".into(),
            source: astra_runtime::skills::SkillSourceKind::Bundled,
            ..Default::default()
        };
        let src = Some("bundled".to_string());
        assert!(matches_skill_filter(&m, &None, &src, &None));

        let src2 = Some("local".to_string());
        assert!(!matches_skill_filter(&m, &None, &src2, &None));
    }

    #[test]
    fn matches_filter_by_tag() {
        let m = astra_runtime::skills::SkillManifest {
            name: "security-scan".into(),
            tags: vec!["security".into(), "audit".into()],
            ..Default::default()
        };
        let q = Some("audit".to_string());
        assert!(matches_skill_filter(&m, &q, &None, &None));
    }

    #[test]
    fn relevance_score_exact_name_highest() {
        let m = astra_runtime::skills::SkillManifest {
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
        let m = astra_runtime::skills::SkillManifest {
            name: "debug".into(),
            description: "Debug issues".into(),
            ..Default::default()
        };
        assert_eq!(skill_relevance_score(&m, "deploy"), 0);
    }

    #[test]
    fn relevance_score_tag_match() {
        let m = astra_runtime::skills::SkillManifest {
            name: "security-scan".into(),
            tags: vec!["security".into(), "vulnerability".into()],
            ..Default::default()
        };
        assert!(skill_relevance_score(&m, "vulnerability") > 0);
    }

    #[test]
    fn relevance_score_multi_word_query() {
        let m = astra_runtime::skills::SkillManifest {
            name: "pr-review".into(),
            description: "Review pull requests for code quality".into(),
            ..Default::default()
        };
        let score = skill_relevance_score(&m, "code review");
        assert!(score > 0, "multi-word query should match description");
    }
}

// ── Quality upload ──────────────────────────────────────────────────────

/// Upload local quality metrics to the marketplace API (opt-in).
async fn upload_quality_report(
    api: &astra_thin_client::ThinClient,
    tracker: &astra_runtime::skills::SkillQualityTracker,
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

/// Upload quality on REPL exit if opt-in enabled via ASTRA_QUALITY_UPLOAD=true.
pub(super) async fn maybe_upload_quality_on_exit(
    api: &astra_thin_client::ThinClient,
    tracker: &astra_runtime::skills::SkillQualityTracker,
    token: Option<&str>,
) {
    if std::env::var("ASTRA_QUALITY_UPLOAD")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false)
    {
        upload_quality_report(api, tracker, token).await;
    }
}
