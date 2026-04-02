use super::*;

fn format_bytes(bytes: u32) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{}B", bytes)
    }
}

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

#[derive(Debug, PartialEq, Eq)]
struct ReviewMatch<'a> {
    path: &'a str,
    line: &'a str,
    text: &'a str,
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

fn parse_review_match(line: &str) -> Option<ReviewMatch<'_>> {
    let mut parts = line.splitn(3, ':');
    let path = parts.next()?.trim();
    let line = parts.next()?.trim();
    let text = parts.next()?.trim_end();
    if path.is_empty() || line.is_empty() {
        return None;
    }
    Some(ReviewMatch { path, line, text })
}

fn summarize_file_list(files: &[String], limit: usize) -> String {
    let shown: Vec<&str> = files.iter().take(limit).map(String::as_str).collect();
    let mut summary = shown.join(", ");
    if files.len() > limit {
        if !summary.is_empty() {
            summary.push_str(", ");
        }
        summary.push_str(&format!("+{} more", files.len() - limit));
    }
    summary
}

fn format_review_search_result(files: &[String], raw: &str) -> String {
    if raw.trim().is_empty() {
        return format!(
            "Scope: {} changed files\nFiles: {}\n\nNo matches found in changed files\nTip: use /search <pattern> for a workspace-wide scan.",
            files.len(),
            summarize_file_list(files, 6)
        );
    }

    let parsed: Vec<ReviewMatch<'_>> = raw.lines().filter_map(parse_review_match).collect();
    if parsed.is_empty() {
        return raw.trim().to_string();
    }

    let mut out = String::new();
    let matched_files: HashSet<&str> = parsed.iter().map(|m| m.path).collect();
    out.push_str(&format!(
        "Scope: {} changed files\nFiles: {}\n\nMatches: {} hit(s) across {} file(s)\n",
        files.len(),
        summarize_file_list(files, 6),
        parsed.len(),
        matched_files.len()
    ));

    let mut current_path: Option<&str> = None;
    for m in parsed {
        if current_path != Some(m.path) {
            if current_path.is_some() {
                out.push('\n');
            }
            out.push_str(&format!("\n{}\n", m.path));
            current_path = Some(m.path);
        }
        out.push_str(&format!("  {}: {}\n", m.line, m.text));
    }

    if out.len() > 20_000 {
        out.truncate(20_000);
        out.push_str("\n[truncated]");
    }
    out
}

fn review_search(executor: &edge_tools::ToolExecutor, pattern: &str) -> String {
    let staged = run_git_lines(&executor.project_root, &["diff", "--name-only", "--cached"]);
    let unstaged = run_git_lines(&executor.project_root, &["diff", "--name-only"]);
    let untracked = run_git_lines(
        &executor.project_root,
        &["ls-files", "--others", "--exclude-standard"],
    );
    let files = collect_changed_files(
        &staged.join("\n"),
        &unstaged.join("\n"),
        &untracked.join("\n"),
    );
    if files.is_empty() {
        return "No changed files found. Use /search <pattern> for workspace-wide search."
            .to_string();
    }

    let mut cmd = SysCommand::new("grep");
    cmd.arg("-n");
    cmd.arg("-i");
    cmd.arg("--binary-files=without-match");
    cmd.arg("--");
    cmd.arg(pattern);
    for file in &files {
        cmd.arg(file);
    }
    cmd.current_dir(&executor.project_root);

    match cmd.output() {
        Ok(output) => match output.status.code() {
            Some(0) => {
                let text = String::from_utf8_lossy(&output.stdout);
                format_review_search_result(&files, &text)
            }
            Some(1) => format_review_search_result(&files, ""),
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

fn build_review_prompt(arg: &str) -> String {
    let target = match arg.trim() {
        "" | "latest" => "HEAD",
        "working" => "WORKING_TREE",
        other => other,
    };
    format!(
        "You are an expert code reviewer working in the current local git repository.\n\
\n\
Review target: {target}\n\
\n\
Process:\n\
1. Get the diff:\n\
   - HEAD -> `git_show` (gives you the full diff already)\n\
   - WORKING_TREE -> `git_diff`\n\
   - Other -> `git_show <rev>`\n\
2. Review the diff directly. Do NOT read entire files.\n\
   Only use `read_file` with `start_line`/`end_line` if you need \
   ~10 lines of surrounding context to verify a specific finding.\n\
3. If you need to understand a function signature or type, use \
   `read_file` with `outline=true` instead of reading the whole file.\n\
4. Prefer `read_file`/`grep`/`glob` over `bash` unless a shell command is truly necessary.\n\
5. Ignore pure formatting churn and environment-only failures unrelated to the reviewed change.\n\
6. Do not narrate your process, do not repeat the diff, and do not output XML-like tags such as `<reflect>`.\n\
\n\
Output format:\n\
- Findings: 0-3 bullets, only material issues.\n\
- Verdict: `LGTM` or `Needs changes`, with one short sentence.\n\
- If nothing material is wrong, say `LGTM` and mention residual risk only if it is real.\n"
    )
}

/// Prefer journal `turn == n` (latest match), else the *n*th `Turn` event (1-based order).
fn resolve_turn_from_journal(
    events: Vec<session_journal::JournalEvent>,
    n: u32,
) -> Option<session_journal::JournalEvent> {
    let turns: Vec<_> = events
        .into_iter()
        .filter(|e| e.event_type == session_journal::JournalEventType::Turn)
        .collect();
    turns
        .iter()
        .rev()
        .find(|e| e.turn == Some(n))
        .cloned()
        .or_else(|| turns.get((n as usize).saturating_sub(1)).cloned())
}

fn print_turn_trace(ev: &session_journal::JournalEvent) {
    let total_ms = ev.duration_ms.unwrap_or(1) as f64;
    let sep = "─".repeat(42);
    eprintln!(
        "\n  {}",
        format!("─── Turn {} Trace {sep}", ev.turn.unwrap_or(0)).cyan()
    );

    // Calculate tool time
    let tool_time_ms: u64 = ev
        .tool_calls
        .as_ref()
        .map(|calls| calls.iter().map(|tc| tc.ms).sum())
        .unwrap_or(0);
    let llm_time_ms = ev.duration_ms.unwrap_or(0).saturating_sub(tool_time_ms);

    // Summary line
    if let Some(ms) = ev.duration_ms {
        eprintln!(
            "  {} {}",
            "Total:".bold(),
            format!("{:.2}s", ms as f64 / 1000.0).bold()
        );
    }

    // TTFT and context time if available
    if let Some(ttft) = ev.ttft_ms {
        eprintln!(
            "  {} {}ms {}",
            "TTFT:".cyan(),
            ttft,
            "(time to first token)".dim()
        );
    }
    if let Some(ctx) = ev.context_ms {
        let mut parts = Vec::new();
        if let Some(sel) = ev.selector_ms {
            let strat = ev.selector_strategy.as_deref().unwrap_or("?");
            parts.push(format!("selector: {}ms [{}]", sel, strat));
        }
        if let Some(m) = ev.memoria_ms {
            parts.push(format!("memoria: {}ms", m));
        }
        let detail = if parts.is_empty() {
            String::new()
        } else {
            format!(" ({})", parts.join(", "))
        };
        eprintln!(
            "  {} {}ms{}  {}",
            "Context:".cyan(),
            ctx,
            detail,
            "(prompt assembly)".dim()
        );
    }
    if let Some(ref skills) = ev.selected_skills
        && !skills.is_empty()
    {
        eprintln!("  {} {}", "Skills:".cyan(), skills.join(", ").cyan());
    }
    eprintln!();

    // Timeline visualization
    eprintln!("  {}", "Timeline".bold());
    let bar_width = 40;

    // LLM portion
    let llm_pct = (llm_time_ms as f64 / total_ms * 100.0) as u32;
    let llm_bar_len = (llm_pct as usize * bar_width / 100).max(1);
    let llm_bar = "█".repeat(llm_bar_len);
    eprintln!(
        "    {:<12} {:>6}ms {:>3}%  {}",
        "LLM".cyan(),
        llm_time_ms,
        llm_pct,
        llm_bar.blue()
    );

    // Per-tool bars with I/O sizes
    if let Some(ref calls) = ev.tool_calls {
        for tc in calls {
            let pct = (tc.ms as f64 / total_ms * 100.0) as u32;
            let bar_len = (pct as usize * bar_width / 100).max(1);
            let bar = if tc.ok {
                "█".repeat(bar_len).green()
            } else {
                "█".repeat(bar_len).red()
            };
            let status = if tc.ok { " " } else { "!" };
            let io_info = match (tc.input_bytes, tc.output_bytes) {
                (Some(i), Some(o)) => {
                    format!(" [{}/{}B]", format_bytes(i), format_bytes(o))
                }
                _ => String::new(),
            };
            eprintln!(
                "    {:<12} {:>6}ms {:>3}%  {}{}{}",
                tc.name.as_str().cyan(),
                tc.ms,
                pct,
                bar,
                status,
                io_info.dim()
            );
        }
    }

    eprintln!();

    // Detailed trace view (OpenTrace style)
    eprintln!("  {}", "Trace".bold());
    let mut offset = 0u64;

    // Context assembly (if available)
    if let Some(ctx) = ev.context_ms {
        eprintln!(
            "    {} {} Context assembly",
            format!("[{:>5}ms]", offset).dim(),
            "├─".dim()
        );
        if let Some(mem) = ev.memoria_ms {
            eprintln!(
                "    {} {}   memoria search ({}ms)",
                format!("[{:>5}ms]", offset).dim(),
                "│ ".dim(),
                mem
            );
        }
        if let Some(sel) = ev.selector_ms {
            let strat = ev.selector_strategy.as_deref().unwrap_or("unknown");
            eprintln!(
                "    {} {}   tool selection ({}ms, {}){}",
                format!("[{:>5}ms]", offset).dim(),
                "│ ".dim(),
                sel,
                strat,
                if sel > 3000 { "  ← slow" } else { "" }
            );
            if let Some(ref skills) = ev.selected_skills
                && !skills.is_empty()
            {
                eprintln!(
                    "    {} {}   selected skills: {}",
                    format!("[{:>5}ms]", offset).dim(),
                    "│ ".dim(),
                    skills.join(", ").cyan()
                );
            }
        }
        offset = ctx;
        eprintln!(
            "    {} {} complete ({}ms)",
            format!("[{:>5}ms]", offset).dim(),
            "│".dim(),
            ctx.to_string().dim()
        );
    }

    // LLM call
    eprintln!(
        "    {} {} LLM request",
        format!("[{:>5}ms]", offset).dim(),
        "├─".dim()
    );
    if let Some(ref m) = ev.model {
        eprintln!(
            "    {}    {} model: {}",
            " ".repeat(8),
            "│".dim(),
            m.as_str().dim()
        );
    }
    if let Some(t_in) = ev.tokens_in {
        let sel_note = match (ev.selector_tokens_in, ev.selector_tokens_out) {
            (Some(si), Some(so)) if si > 0 || so > 0 => {
                format!(" (+selector: {}→{})", si, so)
            }
            _ => String::new(),
        };
        eprintln!(
            "    {}    {} input: {} tokens{}",
            " ".repeat(8),
            "│".dim(),
            t_in.to_string().dim(),
            sel_note.dim()
        );
    }
    // Show TTFT inline
    if let Some(ttft) = ev.ttft_ms {
        let ttft_offset = offset + ttft;
        eprintln!(
            "    {} {} first token (TTFT: {}ms)",
            format!("[{:>5}ms]", ttft_offset).dim(),
            "│".dim(),
            ttft.to_string().yellow()
        );
    }
    if let Some(t_out) = ev.tokens_out {
        eprintln!(
            "    {}    {} output: {} tokens",
            " ".repeat(8),
            "│".dim(),
            t_out.to_string().dim()
        );
    }
    offset += llm_time_ms;
    eprintln!(
        "    {} {} LLM complete ({}ms)",
        format!("[{:>5}ms]", offset).dim(),
        "│".dim(),
        llm_time_ms.to_string().yellow()
    );

    // Tool calls
    if let Some(ref calls) = ev.tool_calls {
        for (i, tc) in calls.iter().enumerate() {
            let is_last = i == calls.len() - 1;
            let branch = if is_last { "└─" } else { "├─" };
            let status = if tc.ok { "✓".green() } else { "✗".red() };

            // Build I/O size annotation
            let io_info = match (tc.input_bytes, tc.output_bytes) {
                (Some(i), Some(o)) => {
                    format!(" (in:{} out:{})", format_bytes(i), format_bytes(o))
                }
                (Some(i), None) => format!(" (in:{})", format_bytes(i)),
                (None, Some(o)) => format!(" (out:{})", format_bytes(o)),
                (None, None) => String::new(),
            };

            eprintln!(
                "    {} {} {} {}{}",
                format!("[{:>5}ms]", offset).dim(),
                branch.dim(),
                status,
                tc.name.as_str().cyan(),
                io_info.dim()
            );

            // Show args preview if available
            if let Some(ref args) = tc.args_preview {
                let sub_branch = if is_last { "   " } else { "│  " };
                let args_truncated = if args.len() > 60 {
                    format!("{}…", &args[..59])
                } else {
                    args.clone()
                };
                eprintln!(
                    "    {}    {} {}",
                    " ".repeat(8),
                    sub_branch.dim(),
                    args_truncated.dim()
                );
            }

            if let Some(ref err) = tc.error {
                let err_preview = if err.len() > 50 {
                    format!("{}…", &err[..50])
                } else {
                    err.clone()
                };
                let sub_branch = if is_last { "   " } else { "│  " };
                eprintln!(
                    "    {}    {} {}",
                    " ".repeat(8),
                    sub_branch.dim(),
                    err_preview.red()
                );
            }
            offset += tc.ms;
            let sub_branch = if is_last { "   " } else { "│  " };
            eprintln!(
                "    {}    {} complete ({}ms)",
                format!("[{:>5}ms]", offset).dim(),
                sub_branch.dim(),
                tc.ms.to_string().dim()
            );
        }
    }

    eprintln!();

    // Breakdown summary
    eprintln!("  {}", "Breakdown".bold());
    let llm_note = if llm_pct > 80 {
        "← bottleneck".yellow().to_string()
    } else {
        String::new()
    };
    eprintln!(
        "    {:<12} {:>6}ms  {:>3}%  {}",
        "LLM".cyan(),
        llm_time_ms,
        llm_pct,
        llm_note
    );
    let tool_pct = 100u32.saturating_sub(llm_pct);
    let tool_note = if tool_pct > 80 {
        "← bottleneck".yellow().to_string()
    } else {
        String::new()
    };
    eprintln!(
        "    {:<12} {:>6}ms  {:>3}%  {}",
        "Tools".cyan(),
        tool_time_ms,
        tool_pct,
        tool_note
    );

    // Tokens per second
    if let (Some(t_out), Some(ms)) = (ev.tokens_out, ev.duration_ms)
        && ms > 0
    {
        let tps = t_out as f64 / (ms as f64 / 1000.0);
        eprintln!("    {:<12} {:>6.1} tokens/s", "Throughput".cyan(), tps);
    }

    eprintln!("  {}", "─".repeat(56).cyan().dim());
    eprintln!();
}

pub(super) async fn handle_info_command(
    cmd: &str,
    arg: &str,
    api: &mo_thin_client::ThinClient,
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

            eprintln!(
                "\n{}",
                format!("─── {title} ─────────────────────────────────────────────").bold()
            );
            for line in result.lines() {
                eprintln!("  {line}");
            }
            eprintln!();
        }

        "/review" => {
            let Some(tok) = token else {
                eprintln!("{}", "  Not logged in. Use /login.".yellow());
                return Ok(());
            };
            let prompt = build_review_prompt(arg);
            let selector = crate::repl_runtime::create_tool_selector_quiet(api, None);
            let mut pm = PermissionManager::with_project(
                false,
                &std::env::current_dir().unwrap_or_default(),
            );
            let turn_start = std::time::Instant::now();
            let sr = stream_chat_sse(ChatTurnParams {
                api,
                token: tok,
                message: &prompt,
                session_id: state.session_id.as_deref(),
                model: state.model.as_deref(),
                explain: state.explain,
                render_md: false,
                history: &state.history,
                perm_manager: &mut pm,
                verbose_mode: state.verbose_mode,
                quiet: false,
                suppress_intermediate_output: true,
                selector: &*selector.0,
                recent_tools: &state.recent_tools,
                tool_health_entries: &state.tool_health_entries,
                skill_registry: crate::skill_instructions::empty_registry(),
                plan_only_chat: false,
                is_plan_subtask: false,
                plan_subtask_id: None,
                delegation_engine: None,
            })
            .await?;
            if let Some(session_id) = sr.session_id.as_deref() {
                crate::repl_turn::initialize_journal_pub(state, session_id);
                state.session_id = Some(session_id.to_string());
            }
            if !sr.full_text.trim().is_empty() {
                crate::cli_utils::print_markdown_width(
                    &sr.full_text,
                    Some(crate::cli_utils::terminal_width_usize()),
                );
                if !sr.full_text.ends_with('\n') {
                    println!();
                }
            }
            state.last_response = Some(sr.full_text.clone());
            let review_input = format!("/review {arg}").trim().to_string();
            state
                .history
                .push((review_input.clone(), sr.full_text.clone()));
            state.turn += 1;
            state.total_prompt_tokens += sr.prompt_tokens;
            state.total_completion_tokens += sr.completion_tokens;
            state.recent_tools = sr.tools_used.clone();

            // Write turn event to journal (same as normal chat turns).
            if let Some(journal) = state.journal.as_ref() {
                let turn_event = mo_agent_services::session_journal::JournalEvent::turn(
                    state.session_id.as_deref(),
                    state.turn,
                    state.model.as_deref(),
                    &review_input,
                    &sr.full_text,
                    sr.tool_calls_count,
                    sr.prompt_tokens,
                    sr.completion_tokens,
                    turn_start.elapsed().as_millis() as u64,
                )
                .with_tool_calls(sr.tool_call_records)
                .with_budget_pressure(sr.budget_pressure)
                .with_tool_selection(
                    sr.tools_selected,
                    sr.selected_skills,
                    sr.tools_used.clone(),
                    sr.budget_used,
                )
                .with_ttft(sr.ttft_ms)
                .with_context_time(sr.context_ms)
                .with_selector_strategy(sr.selector_strategy)
                .with_selector_time(sr.selector_ms)
                .with_selector_tokens(sr.selector_tokens_in, sr.selector_tokens_out)
                .with_memoria_time(sr.memoria_ms);
                state.last_turn_event = Some(turn_event.clone());
                let _ = journal.append(&turn_event);
            }
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
            match api.get_health_text().await {
                Ok(_) => {
                    rows.push((true, "api health", "OK".to_string()));
                }
                Err(e) => {
                    rows.push((false, "api health", e.to_string()));
                }
            }

            // Auth status
            if let Some(tok) = token {
                match api.get_auth_me_text(tok).await {
                    Ok(b) => {
                        let v: serde_json::Value = serde_json::from_str(&b).unwrap_or_default();
                        let un = v.get("username").and_then(|u| u.as_str()).unwrap_or("?");
                        rows.push((true, "auth", format!("logged in as {un}")));
                    }
                    Err(mo_thin_client::ThinClientError::Api { status, .. })
                        if status.as_u16() == 401 =>
                    {
                        rows.push((false, "auth", "token expired — run /login".to_string()));
                    }
                    Err(mo_thin_client::ThinClientError::Api { status, .. }) => {
                        rows.push((false, "auth", format!("HTTP {status}")));
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
                let memoria_health = format!("{}/health", memoria_base.trim_end_matches('/'));
                match api.get_url(&memoria_health).await {
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
                        // When https fails, probe http to give an actionable hint
                        let hint = if memoria_base.starts_with("https://") {
                            let http_url = memoria_base.replacen("https://", "http://", 1);
                            let http_health = format!("{}/health", http_url.trim_end_matches('/'));
                            if api
                                .get_url(&http_health)
                                .await
                                .is_ok_and(|r| r.status().is_success())
                            {
                                format!(
                                    "reachable over http, not https — set MEMORIA_BASE_URL={http_url}"
                                )
                            } else {
                                format!("unreachable ({memoria_base})")
                            }
                        } else {
                            format!("unreachable ({memoria_base})")
                        };
                        rows.push((false, "memoria", hint));
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
                    let mut pair = Vec::with_capacity(2);
                    if !u.is_empty() {
                        pair.push(serde_json::json!({"role":"user","content":u}));
                    }
                    if !a.is_empty() {
                        pair.push(serde_json::json!({"role":"assistant","content":a}));
                    }
                    pair
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

        "/turn" => {
            let trimmed = arg.trim();
            let from_journal = if trimmed.is_empty() {
                None
            } else {
                let n = match trimmed.parse::<u32>() {
                    Ok(v) if v > 0 => v,
                    _ => {
                        eprintln!(
                            "{}",
                            "  Usage: /turn [n] — show timing trace for the last turn, or journal turn #n / nth completed turn."
                                .yellow()
                        );
                        return Ok(());
                    }
                };
                let Some(sid) = state.session_id.as_deref() else {
                    eprintln!(
                        "{}",
                        "  No active session; cannot load /turn from journal.".yellow()
                    );
                    return Ok(());
                };
                match session_journal::read_journal(sid) {
                    Ok(events) => resolve_turn_from_journal(events, n),
                    Err(e) => {
                        eprintln!(
                            "{}",
                            format!("  Failed to read session journal: {e}").yellow()
                        );
                        return Ok(());
                    }
                }
            };
            let ev_ref = if trimmed.is_empty() {
                state.last_turn_event.as_ref()
            } else {
                from_journal.as_ref()
            };

            if let Some(ev) = ev_ref {
                print_turn_trace(ev);
            } else if trimmed.is_empty() {
                eprintln!(
                    "{}",
                    "  No turn data yet. Complete a conversation turn first.".dim()
                );
            } else {
                eprintln!(
                    "{}",
                    format!("  No journal turn matches '{trimmed}'.").yellow()
                );
            }
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
        let files =
            collect_changed_files("src/main.rs\n", "src/main.rs\nsrc/lib.rs\n", "\nnew.rs\n");
        assert_eq!(files, vec!["src/main.rs", "src/lib.rs", "new.rs"]);
    }

    #[test]
    fn parse_review_match_extracts_file_line_and_text() {
        assert_eq!(
            parse_review_match("src/main.rs:42:timeout exceeded"),
            Some(ReviewMatch {
                path: "src/main.rs",
                line: "42",
                text: "timeout exceeded",
            })
        );
    }

    #[test]
    fn format_review_search_result_summarizes_grouped_hits() {
        let files = vec![
            "src/main.rs".to_string(),
            "src/lib.rs".to_string(),
            "tests/review.rs".to_string(),
        ];
        let formatted = format_review_search_result(
            &files,
            "src/main.rs:12:tool timeout\nsrc/main.rs:18:retry timeout\nsrc/lib.rs:7:timeout budget",
        );
        assert!(formatted.contains("Scope: 3 changed files"));
        assert!(formatted.contains("Matches: 3 hit(s) across 2 file(s)"));
        assert!(formatted.contains("\nsrc/main.rs\n"));
        assert!(formatted.contains("  12: tool timeout"));
        assert!(formatted.contains("\nsrc/lib.rs\n"));
    }

    #[test]
    fn format_review_search_result_guides_when_no_matches_found() {
        let files = vec!["src/main.rs".to_string(), "tests/review.rs".to_string()];
        let formatted = format_review_search_result(&files, "");
        assert!(formatted.contains("Scope: 2 changed files"));
        assert!(formatted.contains("No matches found in changed files"));
        assert!(formatted.contains("Tip: use /search <pattern>"));
    }

    #[test]
    fn build_review_prompt_defaults_to_head() {
        let prompt = build_review_prompt("");
        assert!(prompt.contains("Review target: HEAD"));
        assert!(prompt.contains("git_show"));
        assert!(prompt.contains("Do NOT read entire files"));
        assert!(prompt.contains("Do not narrate your process"));
    }

    #[test]
    fn build_review_prompt_supports_working_tree() {
        let prompt = build_review_prompt("working");
        assert!(prompt.contains("Review target: WORKING_TREE"));
        assert!(prompt.contains("git_diff"));
        assert!(prompt.contains("Prefer `read_file`/`grep`/`glob` over `bash`"));
    }
}
