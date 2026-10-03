//! Non-interactive workspace search and review command owners.

use crate::cli::{
    chat_stream::{ChatTurnParams, DEFAULT_TURN_INDEX, stream_chat_sse},
    permission_manager::PermissionManager,
    session::session_state::SessionState,
};
use crate::edge_tools;
use crossterm::style::Stylize;
use std::{collections::HashSet, path::PathBuf, process::Command as SysCommand};

#[derive(Debug, PartialEq, Eq)]
enum GrepRequest {
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

fn parse_grep_request(arg: &str) -> Result<GrepRequest, &'static str> {
    let trimmed = arg.trim();
    if trimmed.is_empty() {
        return Err("Usage: /grep <pattern> | /grep files <glob> | /grep review <pattern>");
    }
    if let Some(rest) = trimmed.strip_prefix("files ").map(str::trim) {
        if rest.is_empty() {
            return Err("Usage: /grep files <glob>");
        }
        return Ok(GrepRequest::Files(rest.to_string()));
    }
    if let Some(rest) = trimmed.strip_prefix("review ").map(str::trim) {
        if rest.is_empty() {
            return Err("Usage: /grep review <pattern>");
        }
        return Ok(GrepRequest::Review(rest.to_string()));
    }
    Ok(GrepRequest::Content(trimmed.to_string()))
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
#[derive(Clone, Debug, PartialEq, Eq)]
enum ReviewGitTarget<'a> {
    Head,
    WorkingTree,
    /// Most recent N commits (e.g. "latest 2 commits", "last 3", "HEAD~3").
    LastN(u32),
    /// Arbitrary two-rev range (e.g. "main..HEAD", "v1.0..v1.1").
    Range(&'a str),
    Rev(&'a str),
}

/// Match `latest N commits?` / `last N commits?` / `last-N` / `lastN`.
fn parse_last_n_phrase(lower: &str) -> Option<u32> {
    let s = lower.trim();
    if s.is_empty() {
        return None;
    }
    // "HEAD~N" (no range).
    if let Some(rest) = s.strip_prefix("head~") {
        if !rest.contains("..")
            && let Ok(n) = rest.parse::<u32>()
            && n >= 1
        {
            return Some(n);
        }
    }
    // "last-7" / "latest-3" style.
    for prefix in ["last-", "latest-"] {
        if let Some(rest) = s.strip_prefix(prefix)
            && let Ok(n) = rest.parse::<u32>()
            && n >= 1
        {
            return Some(n);
        }
    }
    let mut parts = s.split_whitespace();
    let kw = parts.next()?;
    if !matches!(kw, "last" | "latest") {
        return None;
    }
    let num = parts.next()?;
    let n: u32 = num.parse().ok()?;
    if n < 1 {
        return None;
    }
    match parts.next() {
        None => Some(n),
        Some(word) if matches!(word, "commit" | "commits") && parts.next().is_none() => Some(n),
        _ => None,
    }
}

fn parse_review_git_target(arg: &str) -> ReviewGitTarget<'_> {
    let a = arg.trim();
    if a.is_empty() {
        return ReviewGitTarget::Head;
    }
    let lower = a.to_ascii_lowercase();
    match lower.as_str() {
        "latest" | "latest commit" | "last" | "last commit" | "head" | "head commit" | "tip"
        | "current commit" => return ReviewGitTarget::Head,
        "working" | "working tree" | "worktree" | "working-tree" | "local" | "local changes"
        | "dirty" | "wt" => return ReviewGitTarget::WorkingTree,
        _ => {}
    }
    if let Some(n) = parse_last_n_phrase(&lower) {
        return ReviewGitTarget::LastN(n);
    }
    // Only treat as a range if both sides resolve to non-empty refs.
    if let Some((l, r)) = a.split_once("..")
        && !l.trim().is_empty()
        && !r.trim().is_empty()
        && !l.contains(char::is_whitespace)
        && !r.contains(char::is_whitespace)
    {
        return ReviewGitTarget::Range(a);
    }
    ReviewGitTarget::Rev(a)
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
            "Scope: {} changed files\nFiles: {}\n\nNo matches found in changed files\nTip: use /grep <pattern> for a workspace-wide scan.",
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
        out.truncate(out.floor_char_boundary(20_000));
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
        return "No changed files found. Use /grep <pattern> for workspace-wide search."
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
    let target_line = match parse_review_git_target(arg) {
        ReviewGitTarget::Head => "HEAD".to_string(),
        ReviewGitTarget::WorkingTree => "WORKING_TREE".to_string(),
        ReviewGitTarget::LastN(n) => format!("HEAD~{n}..HEAD (last {n} commits)"),
        ReviewGitTarget::Range(r) => r.to_string(),
        ReviewGitTarget::Rev(r) => r.to_string(),
    };
    format!(
        "Review target: {target_line}\n\
\n\
Step 1: Fetch the diff through admitted tools; use Bash for these Git commands when available.\n\
- HEAD → `git show HEAD` (use `git show HEAD~1` for the preceding commit)\n\
- WORKING_TREE → `git diff HEAD`\n\
- Range/rev → `git show <rev>` or `git diff <range>`\n\
\n\
Step 2: Review the diff. Write findings. Stop.\n\
\n\
Hard constraints:\n\
- Do NOT call `read_file` on any file. The diff is sufficient.\n\
- Exception: if a specific line is ambiguous, use `read_file` with `start_line`/`end_line` for ≤15 lines max. At most 2 such calls total.\n\
- Use Bash for the requested Git diff reads. Do NOT call `grep`, `glob`, or unrelated Bash commands unless the diff references an external file not shown.\n\
- Do NOT re-fetch the same commit twice.\n\
\n\
Output:\n\
- 0-3 bullet findings, only material issues (bugs, security, logic errors, API breakage).\n\
- Verdict: `LGTM` or `Needs changes` + one sentence.\n\
- No style/formatting comments. No markdown tables.\n"
    )
}

/// Search the explicitly selected CLI workspace.
pub(crate) async fn handle_grep_command(arg: &str) -> Result<(), String> {
    let request = match parse_grep_request(arg) {
        Ok(request) => request,
        Err(usage) => {
            eprintln!("{}", format!("  {usage}").yellow());
            return Ok(());
        }
    };

    let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let executor = edge_tools::ToolExecutor::new(project_root);

    let (title, result) = match request {
        GrepRequest::Content(pattern) => (
            format!("Workspace grep · {pattern}"),
            executor.grep(&serde_json::json!({"pattern": pattern, "path": "."})),
        ),
        GrepRequest::Files(pattern) => (
            format!("Workspace glob · {pattern}"),
            astra_tools::ToolExecutor::execute_with_metadata(
                &executor,
                "glob",
                &serde_json::json!({"pattern": pattern, "path": "."}),
            )
            .await
            .output,
        ),
        GrepRequest::Review(pattern) => {
            let title = format!("Review grep · {pattern}");
            (title, review_search(&executor, &pattern))
        }
    };

    eprintln!(
        "\n{}",
        format!("─── {title} ─────────────────────────────────────────────")
            .bold()
            .magenta()
    );
    for line in result.lines() {
        eprintln!("  {line}");
    }
    eprintln!();
    Ok(())
}

/// Execute the non-interactive CLI review through the shared chat runtime.
pub(crate) async fn handle_review_command(
    arg: &str,
    api: &astra_thin_client::ThinClient,
    state: &mut SessionState,
    profile: Option<&str>,
    token: Option<&str>,
) -> Result<(), String> {
    let Some(tok) = token else {
        eprintln!("{}", "  Not logged in. Use /login.".yellow());
        return Ok(());
    };
    let project_root = std::env::current_dir().unwrap_or_default();
    let prompt = build_review_prompt(arg);
    let review_label = if arg.trim().is_empty() {
        "HEAD".to_string()
    } else {
        arg.trim().to_string()
    };
    eprintln!(
        "\n{}",
        format!("─── Review · {review_label} ─────────────────────────────────────")
            .bold()
            .magenta()
    );
    let _pipeline_modules =
        crate::cli::session::session_runtime::create_pipeline_modules_quiet(api, None).await;
    let mut pm = PermissionManager::with_workspace_trust(false, &project_root);
    let turn_start = std::time::Instant::now();
    let sr = stream_chat_sse(ChatTurnParams {
        api,
        token: tok,
        auth_profile: profile,
        message: &prompt,
        user_intent: &prompt,
        input_runtime_required_texts: &[],
        input_active_system_skills: &[],
        input_runtime_volatile_texts: &[],
        input_work_unit_observations: &[],
        semantic_query_override: None,
        deferred_tool_activations: None,
        session_id: state.session_id.as_deref(),
        offering_id: None,
        model: state.model.as_deref(),
        provider: None,
        explain: state.explain,
        explain_report_format: state.runtime_config.explain.effective_report_format(),
        render_md: true,
        history: &state.history,
        perm_manager: &mut pm,
        verbose_mode: state.verbose_mode,
        render_policy: crate::cli::stream::stream_render::RenderPolicy::Stream,
        cli_context: Some(&state.cli_context),
        recent_tools: &state.recent_tools,
        tool_health_entries: &state.tool_health_entries,
        resume_restricted_tools: &state.resume_restricted_tools,
        session_lessons: &state.session_lessons,
        memory_selection_reports: &[],

        latest_turn_quality_feedback: state.latest_turn_quality_feedback.as_ref(),
        unified_skill_registry: astra_runtime::skills::default_unified_registry(),
        is_plan_subtask: false,
        plan_subtask_id: None,
        delegation_engine: None,
        cancel_token: None,
        execution_time_budget: None,
        run_control: None,
        incremental_state: None,
        request_session_execution_lease: None,
        plan_assemble_line_release: None,
        stream_event_tx: None,
        explain_analyze_terminal_degraded: None,
        stream_json_emitter: None,
        agent_live_event_sink: None,
        approval_request_tx: None,
        ask_user_request_tx: None,
        plan_review_request_tx: None,
        mcp_manager: Some(state.mcp_manager.clone()),
        skill_quality_tracker: &mut state.skill_quality_tracker,
        discovered_skills: None,
        messaging_metrics: state.messaging_metrics.clone(),
        agent_spawner: state.agent_spawner.clone(),
        root_agent_id: Some("main"),
        root_mailbox_slot: Some(&mut state.root_mailbox),
        observability_hub: state.observability_hub.clone(),
        observability_session: state.observability_session.clone(),
        file_journal: None,
        file_state: None,
        database_snapshot_journal: None,

        git_worktree_journal: None,
        session_state_journal: None,
        bg_task_commands: None,
        bg_task_list_cache: None,
        bash_detach_slot: None,
        turn_index: DEFAULT_TURN_INDEX,
        pipeline_state: None,
        compaction_state: None,
        consecutive_context_window_errors: 0,
        workspace_observation_quarantine: state.workspace_observation_quarantine.clone(),
        idempotency_cache: None,
        pre_loaded_messages: None,
        append_system_prompt: None,
        #[cfg(feature = "harness")]
        harness_sink: Some(state.harness_sink.clone()),
        #[cfg(feature = "harness")]
        harness_trace: Some(state.harness_trace.clone()),
        #[cfg(feature = "harness")]
        benchmark_profile: None,
    })
    .await
    .map_err(|f| f.error)?;
    if let Some(session_id) = sr.session_id.as_deref() {
        crate::cli::session::session_startup::initialize_journal_pub(state, session_id);
        state.set_session_id(session_id.to_string());
    }
    state.last_response = Some(sr.full_text.clone());
    let review_input = format!("/review {arg}").trim().to_string();
    state
        .history
        .push((review_input.clone(), sr.full_text.clone()));
    state.turn += 1;
    state.total_prompt_tokens += sr.prompt_tokens;
    state.total_completion_tokens += sr.completion_tokens;
    state.total_cache_read_tokens += sr.cache_read_tokens;
    state.total_cache_creation_tokens += sr.cache_creation_tokens;
    state.recent_tools = sr.tools_used.clone();

    // Write turn event to journal (same as normal chat turns).
    if let Some(journal) = state.journal.as_ref() {
        let tool_ms: u64 = sr
            .tool_call_records
            .iter()
            .filter(|r| !r.is_synthetic_placeholder())
            .map(|r| r.ms)
            .sum();
        let mut turn_event = astra_services::session_journal::JournalEvent::turn(
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
        .with_qualified_usage(sr.qualified_usage)
        .with_tool_surface(
            sr.visible_tools,
            sr.selected_skills,
            sr.tools_used.clone(),
            sr.budget_used,
        )
        .with_ttft(sr.ttft_ms)
        .with_context_time(sr.context_ms)
        .with_memoria_time(sr.memoria_ms);
        turn_event.llm_rounds = sr.llm_rounds;
        turn_event.total_tool_ms = Some(tool_ms);
        if let Some(dur) = turn_event.duration_ms {
            turn_event.total_llm_ms = Some(dur.saturating_sub(tool_ms));
        }
        state.last_turn_event = Some(turn_event.clone());
        crate::cli::cli_config::cli_utils::append_journal_event_or_warn(
            journal,
            state.session_id.as_deref(),
            &turn_event,
            "workspace_review:inject_turn_event",
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        GrepRequest, ReviewGitTarget, ReviewMatch, build_review_prompt, collect_changed_files,
        format_review_search_result, parse_grep_request, parse_review_git_target,
        parse_review_match,
    };

    #[test]
    fn parse_grep_request_defaults_to_content_search() {
        assert_eq!(
            parse_grep_request("tool timeout").unwrap(),
            GrepRequest::Content("tool timeout".to_string())
        );
    }

    #[test]
    fn parse_grep_request_supports_files_mode() {
        assert_eq!(
            parse_grep_request("files Cargo.toml").unwrap(),
            GrepRequest::Files("Cargo.toml".to_string())
        );
    }

    #[test]
    fn parse_grep_request_supports_review_mode() {
        assert_eq!(
            parse_grep_request("review timeout").unwrap(),
            GrepRequest::Review("timeout".to_string())
        );
    }

    #[test]
    fn parse_grep_request_rejects_empty_args() {
        assert!(parse_grep_request("").is_err());
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
        assert!(formatted.contains("Tip: use /grep <pattern>"));
    }

    #[test]
    fn build_review_prompt_defaults_to_head() {
        let prompt = build_review_prompt("");
        assert!(prompt.contains("Review target: HEAD"));
        assert!(prompt.contains("git show HEAD"));
        assert!(prompt.contains("Do NOT call `read_file`"));
    }

    #[test]
    fn build_review_prompt_supports_working_tree() {
        let prompt = build_review_prompt("working");
        assert!(prompt.contains("Review target: WORKING_TREE"));
        assert!(prompt.contains("git diff HEAD"));
        assert!(prompt.contains("Do NOT call `read_file`"));
    }

    #[test]
    fn build_review_prompt_local_changes_maps_to_working_tree() {
        let prompt = build_review_prompt("local changes");
        assert!(prompt.contains("Review target: WORKING_TREE"));
    }

    #[test]
    fn parse_review_git_target_accepts_common_aliases() {
        assert_eq!(parse_review_git_target(""), ReviewGitTarget::Head);
        assert_eq!(parse_review_git_target("latest"), ReviewGitTarget::Head);
        assert_eq!(
            parse_review_git_target("latest commit"),
            ReviewGitTarget::Head
        );
        assert_eq!(
            parse_review_git_target("last commit"),
            ReviewGitTarget::Head
        );
        assert_eq!(
            parse_review_git_target("local changes"),
            ReviewGitTarget::WorkingTree
        );
        assert_eq!(
            parse_review_git_target("LOCAL"),
            ReviewGitTarget::WorkingTree
        );
        assert_eq!(
            parse_review_git_target("abc123"),
            ReviewGitTarget::Rev("abc123")
        );
    }

    #[test]
    fn parse_review_git_target_recognizes_multi_commit() {
        assert_eq!(
            parse_review_git_target("latest 2 commits"),
            ReviewGitTarget::LastN(2)
        );
        assert_eq!(
            parse_review_git_target("last 3 commits"),
            ReviewGitTarget::LastN(3)
        );
        assert_eq!(parse_review_git_target("Last 5"), ReviewGitTarget::LastN(5));
        assert_eq!(parse_review_git_target("last-7"), ReviewGitTarget::LastN(7));
        assert_eq!(parse_review_git_target("HEAD~4"), ReviewGitTarget::LastN(4));
        assert_eq!(
            parse_review_git_target("main..HEAD"),
            ReviewGitTarget::Range("main..HEAD")
        );
        assert_eq!(
            parse_review_git_target("v1.0..v1.1"),
            ReviewGitTarget::Range("v1.0..v1.1")
        );
        // Bogus phrasing must still fall through to Rev, not silently match.
        assert_eq!(
            parse_review_git_target("latest wibble"),
            ReviewGitTarget::Rev("latest wibble")
        );
    }

    #[test]
    fn build_review_prompt_describes_multi_commit_range() {
        let prompt = build_review_prompt("latest 2 commits");
        assert!(prompt.contains("HEAD~2..HEAD"));
        assert!(prompt.contains("last 2 commits"));
    }
}
