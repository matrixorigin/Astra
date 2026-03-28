use super::*;

fn copy_to_clipboard(text: &str) -> bool {
    let candidates: &[(&str, &[&str])] = &[
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
        ("pbcopy", &[]),
    ];
    for (cmd, args) in candidates {
        if let Ok(mut child) = SysCommand::new(cmd)
            .args(*args)
            .stdin(Stdio::piped())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            if child.wait().map(|s| s.success()).unwrap_or(false) {
                return true;
            }
        }
    }
    false
}

#[derive(Debug, PartialEq, Eq)]
enum SearchRequest {
    Content(String),
    Files(String),
    Review(String),
}

fn parse_search_request(arg: &str) -> Result<SearchRequest, &'static str> {
    let trimmed = arg.trim();
    if trimmed.is_empty() {
        return Err("Usage: /search <pattern> | /search files <glob> | /search review <pattern>");
    }
    if let Some(rest) = trimmed.strip_prefix("files ").map(str::trim) {
        if rest.is_empty() {
            return Err("Usage: /search files <glob>");
        }
        return Ok(SearchRequest::Files(rest.to_string()));
    }
    if let Some(rest) = trimmed.strip_prefix("review ").map(str::trim) {
        if rest.is_empty() {
            return Err("Usage: /search review <pattern>");
        }
        return Ok(SearchRequest::Review(rest.to_string()));
    }
    Ok(SearchRequest::Content(trimmed.to_string()))
}

fn collect_changed_files(staged: &str, unstaged: &str, untracked: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    staged
        .lines()
        .chain(unstaged.lines())
        .chain(untracked.lines())
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter_map(|line| {
            let normalized = line.to_string();
            if seen.insert(normalized.clone()) {
                Some(normalized)
            } else {
                None
            }
        })
        .collect()
}

fn run_git_lines(project_root: &std::path::Path, args: &[&str]) -> Vec<String> {
    match SysCommand::new("git")
        .args(args)
        .current_dir(project_root)
        .output()
    {
        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|line| line.trim().to_string())
            .filter(|line| !line.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

fn review_search(executor: &edge_tools::ToolExecutor, pattern: &str) -> String {
    let staged = run_git_lines(&executor.project_root, &["diff", "--name-only", "--cached"]);
    let unstaged = run_git_lines(&executor.project_root, &["diff", "--name-only"]);
    let untracked = run_git_lines(
        &executor.project_root,
        &["ls-files", "--others", "--exclude-standard"],
    );
    let files = collect_changed_files(&staged.join("\n"), &unstaged.join("\n"), &untracked.join("\n"));
    if files.is_empty() {
        return "No changed files found. Use /search <pattern> for workspace-wide search.".to_string();
    }

    let mut cmd = SysCommand::new("grep");
    cmd.arg("-n");
    cmd.arg("-i");
    cmd.arg("--binary-files=without-match");
    cmd.arg(pattern);
    for file in &files {
        cmd.arg(file);
    }
    cmd.current_dir(&executor.project_root);

    match cmd.output() {
        Ok(output) => match output.status.code() {
            Some(0) => {
                let text = String::from_utf8_lossy(&output.stdout);
                if text.len() > 20_000 {
                    format!("{}\n[truncated]", &text[..20_000])
                } else {
                    text.to_string()
                }
            }
            Some(1) => "No matches found in changed files".to_string(),
            _ => {
                let err = String::from_utf8_lossy(&output.stderr);
                let detail = err.trim();
                if detail.is_empty() {
                    "Error: review search failed".to_string()
                } else {
                    format!("Error: {detail}")
                }
            }
        },
        Err(e) => format!("Error: {e}"),
    }
}

pub(super) async fn handle_info_command(
    cmd: &str,
    arg: &str,
    client: &reqwest::Client,
    base: &str,
    state: &mut ReplState,
    token: Option<&str>,
) -> Result<(), String> {
    match cmd {
        "/history" => {
            if state.history.is_empty() {
                eprintln!("{}", "  No history yet".dim());
            } else if arg.starts_with("search ") || arg.starts_with("grep ") {
                // /history search <query>
                let query = arg
                    .split_once(' ')
                    .map(|x| x.1)
                    .unwrap_or("")
                    .to_lowercase();
                if query.is_empty() {
                    eprintln!("{}", "  Usage: /history search <query>".yellow());
                    return Ok(());
                }
                let mut found = 0;
                for (i, (user, asst)) in state.history.iter().enumerate() {
                    let turn_n = i + 1;
                    let matches_user = user.to_lowercase().contains(&query);
                    let matches_asst = asst.to_lowercase().contains(&query);
                    if matches_user || matches_asst {
                        found += 1;
                        eprintln!("  {}", format!("Turn {turn_n}").bold());
                        if matches_user {
                            let u = if user.len() > 120 {
                                format!("{}…", &user[..120])
                            } else {
                                user.clone()
                            };
                            eprintln!("  {} {}", "❯".cyan(), u);
                        }
                        if matches_asst {
                            let a = if asst.len() > 120 {
                                format!("{}…", &asst[..120])
                            } else {
                                asst.clone()
                            };
                            eprintln!("    {}", a.dim());
                        }
                        eprintln!();
                    }
                }
                if found == 0 {
                    eprintln!("{}", format!("  No matches for '{query}'").dim());
                } else {
                    eprintln!("{}", format!("  {found} turn(s) matched").dim());
                }
            } else {
                eprintln!(
                    "\n{}",
                    "─── Conversation History ─────────────────────────────────────".bold()
                );
                for (i, (user, asst)) in state.history.iter().enumerate() {
                    let turn_n = i + 1;
                    let u = if user.len() > 80 {
                        format!("{}…", &user[..80])
                    } else {
                        user.clone()
                    };
                    let a = if asst.len() > 80 {
                        format!("{}…", &asst[..80])
                    } else {
                        asst.clone()
                    };
                    eprintln!("  {}", format!("Turn {turn_n}").bold());
                    eprintln!("  {} {}", "❯".cyan(), u);
                    eprintln!("    {}", a.dim());
                    if i + 1 < state.history.len() {
                        eprintln!();
                    }
                }
                eprintln!();
            }
        }

        "/search" => {
            let request = match parse_search_request(arg) {
                Ok(request) => request,
                Err(usage) => {
                    eprintln!("{}", format!("  {usage}").yellow());
                    return Ok(());
                }
            };

            let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let executor = edge_tools::ToolExecutor::new(project_root);

            let (title, result) = match request {
                SearchRequest::Content(pattern) => (
                    format!("Workspace search · {pattern}"),
                    executor.grep(&serde_json::json!({"pattern": pattern, "path": "."})),
                ),
                SearchRequest::Files(pattern) => (
                    format!("File search · {pattern}"),
                    executor.glob(&serde_json::json!({"pattern": pattern, "path": "."})),
                ),
                SearchRequest::Review(pattern) => {
                    let title = format!("Review search · {pattern}");
                    (title, review_search(&executor, &pattern))
                }
            };

            eprintln!("\n{}", format!("─── {title} ─────────────────────────────────────────────").bold());
            for line in result.lines() {
                eprintln!("  {line}");
            }
            eprintln!();
        }

        "/copy" => match &state.last_response {
            Some(text) => {
                let text = text.clone();
                let n = text.chars().count();
                let preview: String = text.chars().take(60).collect();
                let preview_display = if text.chars().count() > 60 {
                    format!("{}…", preview)
                } else {
                    preview
                };
                if copy_to_clipboard(&text) {
                    eprintln!("{}", format!("  ✓ Copied ({n} chars)").green());
                    eprintln!("  {}", preview_display.dim());
                } else {
                    eprintln!(
                        "{}",
                        "  ✗ No clipboard tool found (install xclip or xsel)".yellow()
                    );
                }
            }
            None => eprintln!("{}", "  ✗ No response to copy yet".yellow()),
        },

        "/doctor" => {
            eprintln!(
                "\n{}",
                "─── Diagnostics ──────────────────────────────────────────────".bold()
            );

            // Accumulate rows: (ok: bool, label: &str, detail: String)
            let mut rows: Vec<(bool, &'static str, String)> = Vec::new();

            // Binary version
            rows.push((
                true,
                "binary",
                format!("mo-agent v{}", env!("CARGO_PKG_VERSION")),
            ));

            // API health
            match client.get(format!("{base}/health")).send().await {
                Ok(r) => {
                    let status = r.status();
                    rows.push((status.is_success(), "api health", format!("HTTP {status}")));
                }
                Err(e) => {
                    rows.push((false, "api health", e.to_string()));
                }
            }

            // Auth status
            if let Some(tok) = token {
                match client
                    .get(format!("{base}/auth/me"))
                    .headers(auth_headers(tok)?)
                    .send()
                    .await
                {
                    Ok(r) if r.status().is_success() => {
                        let b = r.text().await.unwrap_or_default();
                        let v: serde_json::Value = serde_json::from_str(&b).unwrap_or_default();
                        let un = v.get("username").and_then(|u| u.as_str()).unwrap_or("?");
                        rows.push((true, "auth", format!("logged in as {un}")));
                    }
                    Ok(r) if r.status().as_u16() == 401 => {
                        rows.push((false, "auth", "token expired — run /login".to_string()));
                    }
                    Ok(r) => {
                        rows.push((false, "auth", format!("HTTP {}", r.status())));
                    }
                    Err(e) => {
                        rows.push((false, "auth", e.to_string()));
                    }
                }
            } else {
                rows.push((false, "auth", "not logged in".to_string()));
            }

            // Git repo
            match std::process::Command::new("git")
                .args(["rev-parse", "--show-toplevel"])
                .output()
            {
                Ok(out) if out.status.success() => {
                    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
                    rows.push((true, "git repo", path));
                }
                _ => {
                    rows.push((false, "git repo", "not a git repo".to_string()));
                }
            }

            // Memoria
            let memoria_key_set = std::env::var("MEMORIA_API_KEY")
                .or_else(|_| std::env::var("MEMORIA_MASTER_KEY"))
                .is_ok();
            if memoria_key_set {
                let memoria_base = std::env::var("MEMORIA_BASE_URL")
                    .unwrap_or_else(|_| mo_agent_core::config::DEFAULT_MEMORIA_URL.to_string());
                match client.get(format!("{memoria_base}/health")).send().await {
                    Ok(r) if r.status().is_success() => {
                        rows.push((true, "memoria", format!("reachable at {memoria_base}")));
                    }
                    Ok(r) => {
                        rows.push((
                            false,
                            "memoria",
                            format!("HTTP {} at {memoria_base}", r.status()),
                        ));
                    }
                    Err(_) => {
                        rows.push((false, "memoria", format!("unreachable ({memoria_base})")));
                    }
                }
            } else {
                rows.push((false, "memoria", "MEMORIA_API_KEY not set".to_string()));
            }

            // Print table
            let label_w = rows.iter().map(|(_, l, _)| l.len()).max().unwrap_or(10);
            for (ok, label, detail) in &rows {
                let icon = if *ok {
                    "✓".green().to_string()
                } else {
                    "✗".red().to_string()
                };
                eprintln!("  {}  {:<label_w$}  {}", icon, label, detail.clone().dim());
            }

            let fail_count = rows.iter().filter(|(ok, _, _)| !ok).count();
            eprintln!();
            if fail_count == 0 {
                eprintln!("  {}", "All checks passed".green().bold());
            } else {
                eprintln!("  {}", format!("{fail_count} check(s) failed").red().bold());
            }
            eprintln!();
        }

        "/context" => {
            let sep = "─".repeat(38);
            eprintln!("\n  {}", format!("─── Context Window {sep}").cyan());
            let session_display = state
                .session_id
                .as_deref()
                .map(|s| {
                    if s.len() > 8 {
                        s[..8].to_string()
                    } else {
                        s.to_string()
                    }
                })
                .unwrap_or_else(|| "none".to_string());
            let model_display = state.model.clone().unwrap_or_else(|| "default".to_string());
            let run_display = state
                .run_id
                .as_deref()
                .map(|s| {
                    if s.len() > 8 {
                        s[..8].to_string()
                    } else {
                        s.to_string()
                    }
                })
                .unwrap_or_else(|| "none".to_string());
            let msg_count = state.history.len() * 2;
            // Estimate tokens from history
            let est_messages: Vec<serde_json::Value> = state
                .history
                .iter()
                .flat_map(|(u, a)| {
                    if u.is_empty() {
                        vec![serde_json::json!({"role":"assistant","content":a})]
                    } else {
                        vec![
                            serde_json::json!({"role":"user","content":u}),
                            serde_json::json!({"role":"assistant","content":a}),
                        ]
                    }
                })
                .collect();
            let est_tokens = prompts::estimate_tokens(&est_messages);
            let budget = &state.context_budget;
            let usage_pct = if budget.model_limit > 0 {
                (est_tokens as f64 / budget.model_limit as f64 * 100.0) as u32
            } else {
                0
            };
            let compact_trigger_k = budget.compact_trigger() / 1000;
            eprintln!("  {:<10}  {}", "session".cyan(), session_display.dim());
            eprintln!("  {:<10}  {}", "model".cyan(), model_display.dim());
            eprintln!("  {:<10}  {}", "turn".cyan(), state.turn.to_string().dim());
            eprintln!(
                "  {:<10}  {}",
                "history".cyan(),
                format!("{msg_count} messages").dim()
            );
            eprintln!(
                "  {:<10}  {}",
                "tokens".cyan(),
                format!(
                    "~{}k / {}k ({usage_pct}%)",
                    est_tokens / 1000,
                    budget.model_limit / 1000
                )
                .dim()
            );
            eprintln!(
                "  {:<10}  {}",
                "compact".cyan(),
                format!(
                    "auto at ~{compact_trigger_k}k tokens, keep {} turns",
                    budget.keep_recent_turns
                )
                .dim()
            );
            eprintln!(
                "  {:<10}  {}",
                "explain".cyan(),
                state.explain.to_string().dim()
            );
            eprintln!(
                "  {:<10}  {}",
                "verbose".cyan(),
                state.verbose_mode.to_string().dim()
            );
            eprintln!("  {:<10}  {}", "run_id".cyan(), run_display.dim());
            eprintln!("  {}", "─".repeat(56).cyan().dim());
            eprintln!();
        }

        "/version" => {
            eprintln!("{}", "  mo-agent version 0.1.0 (Rust)".bold());
        }

        "/rewind" => {
            if arg.is_empty() {
                // Show available turns
                if state.history.is_empty() {
                    eprintln!("{}", "  No history to rewind".dim());
                } else {
                    eprintln!("{}", "  Usage: /rewind <turn_number>".yellow());
                    eprintln!(
                        "{}",
                        format!(
                            "  Current: turn {} ({} exchanges)",
                            state.turn,
                            state.history.len()
                        )
                        .dim()
                    );
                    for (i, (user, _)) in state.history.iter().enumerate() {
                        let turn_n = i + 1;
                        let u = if user.len() > 60 {
                            format!("{}…", &user[..60])
                        } else {
                            user.clone()
                        };
                        eprintln!(
                            "  {} {} {}",
                            format!("{turn_n}").bold(),
                            "❯".cyan(),
                            u.dim()
                        );
                    }
                }
            } else {
                let target: usize = match arg.parse() {
                    Ok(n) => n,
                    Err(_) => {
                        eprintln!(
                            "{}",
                            "  Usage: /rewind <turn_number> (e.g. /rewind 3)".yellow()
                        );
                        return Ok(());
                    }
                };
                if target == 0 {
                    // Rewind to start = clear history
                    let old_len = state.history.len();
                    state.history.clear();
                    state.turn = 0;
                    state.last_response = None;
                    if let Some(ref j) = state.journal {
                        let _ = j.append(&session_journal::JournalEvent::config_change(
                            state.session_id.as_deref(),
                            "rewind",
                            &format!("rewound to start, removed {old_len} turn(s)"),
                        ));
                    }
                    eprintln!(
                        "{}",
                        format!("  ✓ Rewound to start. Removed {old_len} turn(s).").green()
                    );
                } else if target > state.history.len() {
                    eprintln!(
                        "{}",
                        format!(
                            "  ✗ Turn {target} does not exist (max: {})",
                            state.history.len()
                        )
                        .yellow()
                    );
                } else {
                    let old_len = state.history.len();
                    let removed = old_len - target;
                    state.history.truncate(target);
                    state.turn = target as u32;
                    state.last_response = state.history.last().map(|(_, a)| a.clone());
                    if let Some(ref j) = state.journal {
                        let _ = j.append(&session_journal::JournalEvent::config_change(
                            state.session_id.as_deref(),
                            "rewind",
                            &format!(
                                "rewound from turn {old_len} to {target}, removed {removed} turn(s)"
                            ),
                        ));
                    }
                    eprintln!(
                        "{}",
                        format!("  ✓ Rewound to turn {target}. Removed {removed} turn(s).").green()
                    );
                }
            }
        }

        _ => unreachable!("unexpected info command: {cmd}"),
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_search_request_defaults_to_content_search() {
        assert_eq!(
            parse_search_request("tool timeout").unwrap(),
            SearchRequest::Content("tool timeout".to_string())
        );
    }

    #[test]
    fn parse_search_request_supports_files_mode() {
        assert_eq!(
            parse_search_request("files Cargo.toml").unwrap(),
            SearchRequest::Files("Cargo.toml".to_string())
        );
    }

    #[test]
    fn parse_search_request_supports_review_mode() {
        assert_eq!(
            parse_search_request("review timeout").unwrap(),
            SearchRequest::Review("timeout".to_string())
        );
    }

    #[test]
    fn parse_search_request_rejects_empty_args() {
        assert!(parse_search_request("").is_err());
    }

    #[test]
    fn collect_changed_files_deduplicates_and_skips_blanks() {
        let files = collect_changed_files("src/main.rs\n", "src/main.rs\nsrc/lib.rs\n", "\nnew.rs\n");
        assert_eq!(files, vec!["src/main.rs", "src/lib.rs", "new.rs"]);
    }
}
