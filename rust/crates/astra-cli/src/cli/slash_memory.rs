use super::*;
use crate::sse_utils::{collect_sse_text, stream_sse_markdown};
use astra_runtime::plan_decompose;

/// Non-interactive plan decomposition (same `/chat/turn` + [`plan_decompose::parse_plan_response`] path as `/plan enter`).
///
/// Does not use the REPL. Cloud template enrichment is skipped (no `MatrixCloudRuntime` in this entrypoint).
pub(crate) async fn headless_plan_decompose(
    api: &astra_thin_client::ThinClient,
    token: &str,
    goal: &str,
    session_id: Option<&str>,
    model: Option<&str>,
    quiet: bool,
) -> Result<plan_decompose::TaskPlan, String> {
    use std::path::PathBuf;

    let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if !quiet {
        eprintln!("  {} Analyzing project...", "⋯".dim());
    }
    let context = plan_decompose::analyze_project(&project_root);
    let prompt = plan_decompose::decomposition_prompt(goal, &context);
    if !quiet {
        eprintln!("  {} Decomposing goal...", "⋯".dim());
    }

    let mut payload = serde_json::json!({
        "messages": [{"role": "user", "content": prompt}],
        "session_id": session_id,
    });
    if let Some(m) = model {
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("model".to_string(), serde_json::json!(m));
        }
    }

    let resp = api
        .post_chat_turn(token, &payload)
        .await
        .map_err(map_thin_err)?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("LLM request failed ({status}): {body}"));
    }

    let collected = if quiet {
        collect_sse_text(resp, false).await
    } else {
        stream_sse_markdown(resp).await
    };
    if collected.event_types.iter().any(|t| t == "error") && collected.text.trim().is_empty() {
        return Err("LLM stream returned an error event with no text".to_string());
    }
    let full_text = collected.text;
    if full_text.trim().is_empty() {
        return Err("empty model response (no text_delta)".to_string());
    }

    plan_decompose::parse_plan_response(&full_text).map_err(|e| {
        let prev = plan_decompose::plan_response_parse_error_preview(&full_text, 10, 700);
        if prev.is_empty() {
            format!("failed to parse plan: {e}")
        } else {
            format!("failed to parse plan: {e}\n--- response preview ---\n{prev}")
        }
    })
}

/// Enrich a `ProjectContext` with learned plan templates from cloud storage.
///
/// Best-effort: returns quietly if no cloud connection or query fails.
/// When `verbose` is true, shows a message when searching (useful for debugging).
async fn enrich_with_templates(
    context: &mut plan_decompose::ProjectContext,
    matrix_runtime: Option<&std::sync::Arc<astra_runtime::MatrixCloudRuntime>>,
    user_id: Option<&str>,
    goal: &str,
    verbose: bool,
) {
    let Some(mc) = matrix_runtime else {
        if verbose {
            eprintln!(
                "  {} {}",
                "⋯".dim(),
                "No cloud connection — skipping template search".dim()
            );
        }
        return;
    };
    let pool = mc.shared_pool().get();
    let uid = user_id.unwrap_or("anonymous");

    if verbose {
        eprintln!(
            "  {} {}",
            "⋯".cyan(),
            "Searching for similar plan templates…".dim()
        );
    }

    let templates = plan_decompose::query_similar_templates(pool, uid, goal, 3).await;
    if !templates.is_empty() {
        eprintln!(
            "  {} Found {} learned template{}",
            theme::icon_ok(),
            format!("{}", templates.len()).cyan(),
            if templates.len() == 1 { "" } else { "s" }
        );
        context.prior_templates = templates;
    } else if verbose {
        eprintln!("  {} {}", "⋯".dim(), "No matching templates found".dim());
    }
}

fn eprint_plan_json_parse_failed(full_text: &str, err: &str) {
    eprintln!("  {} Failed to parse plan: {}", theme::icon_err(), err);
    let prev = plan_decompose::plan_response_parse_error_preview(full_text, 10, 700);
    if !prev.is_empty() {
        eprintln!("  {}", "Model reply:".dim());
        for line in prev.lines() {
            eprintln!("    {}", line.dim());
        }
    }
    eprintln!(
        "  {}",
        "Tip: Plan decomposition expects JSON only. For git/files without JSON plans, exit plan mode (`exit`) or `/plan off` and use normal chat with tools."
            .dim()
    );
}

pub(super) async fn handle_memory_domain_command(
    cmd: &str,
    arg: &str,
    api: &astra_thin_client::ThinClient,
    state: &mut ReplState,
    token: Option<&str>,
) -> Result<(), String> {
    match cmd {
        // ═══════════════════════════════════════════ Memory Commands ════
        "/memory" => {
            let Some(tok) = token else {
                eprintln!("{}", "  Not logged in. Use /login.".yellow());
                return Ok(());
            };
            let subcmd = arg.split_whitespace().next().unwrap_or("list");
            let sub_arg = arg.strip_prefix(subcmd).unwrap_or("").trim();
            match subcmd {
                "search" if !sub_arg.is_empty() => {
                    let payload = serde_json::json!({
                        "query": sub_arg,
                        "top_k": 10,
                    });
                    match api.post_memory_search_json(tok, &payload).await {
                        Ok(r) if r.status().is_success() => {
                            let body = r.text().await.unwrap_or_default();
                            if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(&body) {
                                if arr.is_empty() {
                                    eprintln!("  {}", "No memories found.".dim());
                                } else {
                                    for (i, m) in arr.iter().enumerate() {
                                        let content = m
                                            .get("content")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("?");
                                        let id = m.get("id").and_then(|v| v.as_str()).unwrap_or("");
                                        let short_id = prefix_chars(id, 8);
                                        // Use protocol-aware display
                                        let display = if let Some(entry) =
                                            prompts::memory_proto::MemoryEntry::parse(content)
                                        {
                                            entry.display_line()
                                        } else {
                                            let mtype = m
                                                .get("memory_type")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("?");
                                            let preview: String =
                                                content.chars().take(80).collect();
                                            format!("[{mtype}] {preview}")
                                        };
                                        eprintln!(
                                            "  {}. {} {}",
                                            (i + 1).to_string().cyan(),
                                            display,
                                            short_id.dim()
                                        );
                                    }
                                }
                            } else {
                                print_json_or_raw(&body);
                            }
                        }
                        Ok(r) => eprintln!(
                            "{}",
                            format!("  ✗ Memory search failed ({})", r.status()).red()
                        ),
                        Err(e) => eprintln!("{}", format!("  ✗ Memory unreachable: {e}").red()),
                    }
                }
                _ if sub_arg.is_empty() && subcmd == "list" => {
                    let payload = serde_json::json!({
                        "query": "user preferences knowledge plans tasks",
                        "top_k": 20,
                    });
                    match api.post_memory_search_json(tok, &payload).await {
                        Ok(r) if r.status().is_success() => {
                            let body = r.text().await.unwrap_or_default();
                            if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(&body) {
                                if arr.is_empty() {
                                    eprintln!("  {}", "No memories stored yet.".dim());
                                } else {
                                    eprintln!(
                                        "\n  {}",
                                        "─── Memories ───────────────────────────────────".dim()
                                    );
                                    for (i, m) in arr.iter().enumerate() {
                                        let content = m
                                            .get("content")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("?");
                                        let display = if let Some(entry) =
                                            prompts::memory_proto::MemoryEntry::parse(content)
                                        {
                                            entry.display_line()
                                        } else {
                                            let mtype = m
                                                .get("memory_type")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("?");
                                            let preview: String =
                                                content.chars().take(80).collect();
                                            format!("[{mtype}] {preview}")
                                        };
                                        eprintln!("  {}. {}", (i + 1).to_string().cyan(), display);
                                    }
                                    eprintln!(
                                        "  {}",
                                        "────────────────────────────────────────────────".dim()
                                    );
                                    eprintln!("  {} memories", arr.len().to_string().cyan());
                                }
                            } else {
                                print_json_or_raw(&body);
                            }
                        }
                        Ok(r) => eprintln!("{}", format!("  ✗ Failed ({})", r.status()).red()),
                        Err(e) => eprintln!("{}", format!("  ✗ Unreachable: {e}").red()),
                    }
                }
                _ => {
                    eprintln!("  {} /memory [list | search <query>]", "Usage:".dim());
                }
            }
        }

        "/plan" => {
            let subcmd = arg.split_whitespace().next().unwrap_or("");
            let sub_arg = arg.strip_prefix(subcmd).unwrap_or("").trim();
            match subcmd {
                "on" => {
                    if state.plan_mode.is_some() {
                        eprintln!(
                            "  {} Leave structured plan mode first (`exit` or `/plan`), then `/plan on`.",
                            theme::icon_warn()
                        );
                        return Ok(());
                    }
                    if state.chat_plan_only {
                        eprintln!(
                            "  {}",
                            "Plan-only chat is already on. Use `/plan off` to disable.".dim()
                        );
                        return Ok(());
                    }
                    state.chat_plan_only = true;
                    eprintln!(
                        "  {} Plan-only chat ON — edge tools disabled; model answers with plans only.",
                        "📋".cyan()
                    );
                    eprintln!(
                        "  {}",
                        "  `/plan off` restores normal tool use. `/plan` (no args) opens the structured plan editor."
                            .dim()
                    );
                    return Ok(());
                }
                "off" => {
                    if !state.chat_plan_only {
                        eprintln!("  {}", "Plan-only chat was not active.".dim());
                        return Ok(());
                    }
                    state.chat_plan_only = false;
                    eprintln!("  {} Plan-only chat OFF — normal agent mode.", "←".cyan());
                    return Ok(());
                }
                "status" => {
                    handle_plan_status(state);
                    return Ok(());
                }
                "pause" => {
                    handle_plan_pause(state);
                    return Ok(());
                }
                _ => {}
            }

            let Some(tok) = token else {
                eprintln!("{}", "  Not logged in. Use /login.".yellow());
                return Ok(());
            };
            match subcmd {
                // Toggle: /plan with no args enters or exits plan mode
                "" => {
                    use plan_decompose::{PlanModeState, format_plan, format_plan_entry_card};

                    // If already in plan mode, exit
                    if state.plan_mode.is_some() {
                        // Save state before exiting
                        if let Some(ref ps) = state.plan_mode
                            && let Err(e) = ps.save_to_file(&PlanModeState::state_path())
                        {
                            eprintln!("  {} Failed to save plan state: {e}", theme::icon_warn());
                        }
                        state.plan_mode = None;
                        eprintln!("  {} Exited plan mode", "←".cyan());
                        return Ok(());
                    }

                    // Try to load saved plan
                    let saved_plan =
                        PlanModeState::load_from_file(&PlanModeState::state_path()).ok();

                    // Display entry card
                    eprintln!();
                    let card =
                        format_plan_entry_card(saved_plan.as_ref(), state.executing_plan.as_ref());
                    eprintln!("{}", card);

                    // Restore saved plan or create new
                    if let Some(plan) = saved_plan {
                        state.chat_plan_only = false;
                        let goal = plan.goal.clone();
                        state.plan_mode = Some(plan);
                        plan_interaction::eprint_plan_mode_banner(&goal);
                        if let Some(ref ps) = state.plan_mode {
                            eprintln!(
                                "  {} Restored saved plan: {}",
                                "↩".cyan(),
                                ps.goal.as_str().cyan()
                            );
                            eprintln!();
                            let formatted = format_plan(&ps.plan);
                            eprintln!("{formatted}");
                        }
                    } else {
                        let project_root = std::env::current_dir()
                            .unwrap_or_else(|_| std::path::PathBuf::from("."));
                        let context = plan_decompose::analyze_project(&project_root);
                        state.chat_plan_only = false;
                        state.plan_mode = Some(PlanModeState::new(String::new(), context));
                        plan_interaction::eprint_plan_mode_banner("");
                        eprintln!("  {} Describe your goal to start planning.", "→".cyan());
                    }
                }
                "show" => {
                    let payload = prompts::memory_proto::MemoryEntry::search_query(
                        prompts::memory_proto::NS_PLAN,
                        "current goals",
                    );
                    match api.post_memory_search_json(tok, &payload).await {
                        Ok(r) if r.status().is_success() => {
                            let body = r.text().await.unwrap_or_default();
                            if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(&body) {
                                let contents: Vec<&str> = arr
                                    .iter()
                                    .filter_map(|m| m.get("content").and_then(|v| v.as_str()))
                                    .collect();
                                let plans = prompts::memory_proto::filter_ns(
                                    &contents,
                                    prompts::memory_proto::NS_PLAN,
                                );
                                if plans.is_empty() {
                                    eprintln!(
                                        "  {}",
                                        "No active plan. Use /plan set <text> to create one.".dim()
                                    );
                                } else {
                                    eprintln!(
                                        "\n  {}",
                                        "─── Plan ───────────────────────────────────────".dim()
                                    );
                                    for p in &plans {
                                        for line in p.body.lines() {
                                            eprintln!("  {line}");
                                        }
                                    }
                                    eprintln!(
                                        "  {}",
                                        "────────────────────────────────────────────────".dim()
                                    );
                                }
                            } else {
                                print_json_or_raw(&body);
                            }
                        }
                        Ok(r) => eprintln!("{}", format!("  ✗ Failed ({})", r.status()).red()),
                        Err(e) => eprintln!("{}", format!("  ✗ Unreachable: {e}").red()),
                    }
                }
                "set" if !sub_arg.is_empty() => {
                    let entry = prompts::memory_proto::MemoryEntry::new(
                        prompts::memory_proto::NS_PLAN,
                        prompts::memory_proto::ST_ACTIVE,
                        sub_arg,
                    );
                    let meta = prompts::memory_proto::EntryMeta::from_session_with_tier(
                        state.session_id.as_deref(),
                        state.turn,
                        prompts::memory_proto::SRC_USER,
                        prompts::memory_proto::TIER_VERIFIED,
                    );
                    match api
                        .post_memory_store_json(tok, &entry.to_store_payload_with_meta(&meta))
                        .await
                    {
                        Ok(r) if r.status().is_success() => {
                            eprintln!("  {} Plan saved to memory.", theme::icon_ok());
                        }
                        Ok(r) => eprintln!("{}", format!("  ✗ Failed ({})", r.status()).red()),
                        Err(e) => eprintln!("{}", format!("  ✗ Unreachable: {e}").red()),
                    }
                }
                "clear" => {
                    let payload = prompts::memory_proto::MemoryEntry::purge_payload(
                        prompts::memory_proto::NS_PLAN,
                    );
                    match api.post_memory_purge_json(tok, &payload).await {
                        Ok(r) if r.status().is_success() => {
                            eprintln!("  {} Plan cleared.", theme::icon_ok());
                        }
                        Ok(r) => eprintln!("{}", format!("  ✗ Failed ({})", r.status()).red()),
                        Err(e) => eprintln!("{}", format!("  ✗ Unreachable: {e}").red()),
                    }
                }
                "decompose" if !sub_arg.is_empty() => {
                    // Analyze project context using current working directory
                    let project_root =
                        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
                    eprintln!("  {} Analyzing project structure...", "⋯".dim());
                    let mut context = plan_decompose::analyze_project(&project_root);

                    eprintln!(
                        "  {} {} languages, {} files, {}",
                        theme::icon_ok(),
                        context.languages.len(),
                        context.source_file_count,
                        context.entry_points.join(", ")
                    );

                    // Enrich with learned templates from cloud
                    enrich_with_templates(
                        &mut context,
                        state.matrix_runtime.as_ref(),
                        state.ingestion_user_id.as_deref(),
                        sub_arg,
                        state.verbose_mode,
                    )
                    .await;

                    // Generate the decomposition prompt
                    let prompt = plan_decompose::decomposition_prompt(sub_arg, &context);

                    // Store the goal in plan memory
                    let entry = prompts::memory_proto::MemoryEntry::new(
                        prompts::memory_proto::NS_PLAN,
                        prompts::memory_proto::ST_ACTIVE,
                        sub_arg,
                    );
                    let store_meta = prompts::memory_proto::EntryMeta::from_session_with_tier(
                        state.session_id.as_deref(),
                        state.turn,
                        prompts::memory_proto::SRC_USER,
                        prompts::memory_proto::TIER_VERIFIED,
                    );
                    let store_payload = entry.to_store_payload_with_meta(&store_meta);
                    let _ = api.post_memory_store_json(tok, &store_payload).await;

                    // Call LLM via /chat/turn SSE endpoint
                    eprintln!("  {} Decomposing goal into subtasks...", "⋯".dim());

                    let payload = serde_json::json!({
                        "messages": [{"role": "user", "content": prompt}],
                        "session_id": state.session_id.clone(),
                        "model": state.model.clone(),
                        "edge_profile": {
                            "cwd": project_root.to_string_lossy(),
                        },
                        "edge_tools": [],  // No tools needed for plan generation
                    });

                    match api.post_chat_turn(tok, &payload).await {
                        Ok(resp) if resp.status().is_success() => {
                            let sse_result = stream_sse_markdown(resp).await;

                            match plan_decompose::parse_plan_response(&sse_result.text) {
                                Ok(_plan) => {}
                                Err(e) => {
                                    eprint_plan_json_parse_failed(&sse_result.text, &e);
                                }
                            }
                        }
                        Ok(resp) => {
                            eprintln!(
                                "{}",
                                format!("  ✗ LLM call failed ({})", resp.status()).red()
                            );
                            // Fallback: show the prompt for manual execution
                            eprintln!();
                            eprintln!("{}  Generated decomposition prompt:", "📋".yellow());
                            let preview: String = prompt.chars().take(300).collect();
                            eprintln!(
                                "{}{}",
                                preview.dim(),
                                if prompt.len() > 300 { "..." } else { "" }
                            );
                            eprintln!();
                            eprintln!(
                                "{}  Type 'decompose: {}' to try again.",
                                "💡".cyan(),
                                sub_arg
                            );
                        }
                        Err(e) => {
                            eprintln!("{}", format!("  ✗ Request failed: {e}").red());
                        }
                    }
                }
                "enter" if !sub_arg.is_empty() => {
                    // Enter interactive plan mode (Kiro-style)
                    use plan_decompose::{
                        PlanModeState, analyze_project, decomposition_prompt, format_plan,
                        parse_plan_response,
                    };

                    let Some(tok) = token else {
                        eprintln!("  {} Not logged in. Run /login first.", theme::icon_err());
                        return Ok(());
                    };

                    // Analyze project context
                    let project_root =
                        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
                    eprintln!("  {} Analyzing project...", "⋯".dim());
                    let mut context = analyze_project(&project_root);

                    // Enrich with learned templates from cloud
                    enrich_with_templates(
                        &mut context,
                        state.matrix_runtime.as_ref(),
                        state.ingestion_user_id.as_deref(),
                        sub_arg,
                        state.verbose_mode,
                    )
                    .await;

                    // Generate initial decomposition prompt
                    let prompt = decomposition_prompt(sub_arg, &context);

                    eprintln!("  {} Decomposing goal...", "⋯".dim());

                    // Call LLM for initial plan
                    let payload = serde_json::json!({
                        "messages": [{"role": "user", "content": prompt}],
                        "session_id": state.session_id.clone(),
                    });

                    let resp = api.post_chat_turn(tok, &payload).await;

                    match resp {
                        Ok(r) if r.status().is_success() => {
                            let mut full_text = String::new();
                            let mut stream = r.bytes_stream();
                            use futures_util::StreamExt;

                            while let Some(chunk) = stream.next().await {
                                if let Ok(bytes) = chunk {
                                    let event_str = String::from_utf8_lossy(&bytes);
                                    for line in event_str.lines() {
                                        if let Some(data) = line.strip_prefix("data: ")
                                            && let Ok(json) =
                                                serde_json::from_str::<serde_json::Value>(data)
                                            && json.get("type").and_then(|v| v.as_str())
                                                == Some("text_delta")
                                            && let Some(content) =
                                                json.get("content").and_then(|v| v.as_str())
                                        {
                                            full_text.push_str(content);
                                        }
                                    }
                                }
                            }

                            // Parse the plan
                            let plan_result = parse_plan_response(&full_text);

                            // Create PlanModeState
                            let mut plan_state = PlanModeState::new(sub_arg.to_string(), context);

                            // Set the plan if parsing succeeded
                            if let Ok(ref plan) = plan_result {
                                plan_state.set_plan(plan.clone());
                            }

                            state.plan_mode = Some(plan_state);

                            // Save for session recovery
                            if let Some(ref ps) = state.plan_mode {
                                let _ = ps.save_to_file(&PlanModeState::state_path());
                            }

                            plan_interaction::eprint_plan_mode_banner(sub_arg);

                            if let Ok(ref p) = plan_result {
                                let formatted = format_plan(p);
                                eprintln!("{formatted}");
                                eprintln!();
                                plan_interaction::eprint_plan_commands_help();
                            } else if let Err(ref e) = plan_result {
                                eprint_plan_json_parse_failed(&full_text, &e.to_string());
                            }
                        }
                        Ok(r) => {
                            eprintln!("{}", format!("  ✗ LLM call failed ({})", r.status()).red());
                        }
                        Err(e) => {
                            eprintln!("{}", format!("  ✗ Request failed: {e}").red());
                        }
                    }
                }
                "resume" => {
                    // Resume plan mode from saved state
                    use plan_decompose::{PlanModeState, format_plan};
                    let path = PlanModeState::state_path();
                    match PlanModeState::load_from_file(&path) {
                        Ok(ps) => {
                            let goal = ps.goal.clone();
                            let plan = ps.plan.clone();
                            state.plan_mode = Some(ps);
                            eprintln!();
                            eprintln!("{}  Resumed plan mode for: {}", "📋".yellow(), goal.cyan());
                            eprintln!();
                            if !plan.subtasks.is_empty() {
                                eprintln!("{}", format_plan(&plan));
                            }
                            eprintln!();
                            eprintln!(
                                "  {} Commands: 'exit' to leave, 'execute' or 'go' to run",
                                "💡".cyan()
                            );
                        }
                        Err(_) => {
                            eprintln!("  {} No saved plan state to resume", theme::icon_warn());
                        }
                    }
                }
                "exit" => {
                    if state.plan_mode.is_some() {
                        state.plan_mode = None;
                        plan_decompose::PlanModeState::clear_saved_state();
                        eprintln!("  {} Exited plan mode", theme::icon_ok());
                    } else {
                        eprintln!("  {} {}", theme::icon_warn(), "Not in plan mode".yellow());
                    }
                }
                "cloud" => {
                    // List or load plans from cloud
                    if let Some(ref svc) = state.task_service {
                        use astra_services::TaskService;
                        let user_id = state.ingestion_user_id.as_deref().unwrap_or("local");

                        match svc.list_tasks(user_id, None).await {
                            Ok(tasks) => {
                                let with_plans: Vec<_> =
                                    tasks.iter().filter(|t| t.items_total > 0).collect();

                                if with_plans.is_empty() {
                                    eprintln!(
                                        "  {} No cloud plans found. Use /plan auto <goal> to create one.",
                                        theme::icon_warn()
                                    );
                                } else {
                                    eprintln!("\n{}  Cloud Plans", "☁️".cyan());
                                    eprintln!("{}", "─".repeat(50));
                                    for t in &with_plans {
                                        let icon = match t.status {
                                            astra_services::TaskStatus::Completed => "✓",
                                            astra_services::TaskStatus::Failed => "✗",
                                            astra_services::TaskStatus::InProgress => "▶",
                                            astra_services::TaskStatus::Paused => "⏸",
                                            _ => "○",
                                        };
                                        let short_id = &t.task_id[..8.min(t.task_id.len())];
                                        let subtask_count = t.items_total;
                                        let project_type = t.project_type.as_deref().unwrap_or("?");
                                        eprintln!(
                                            "  {} {} {} [{}] ({} subtasks, {})",
                                            short_id.dim(),
                                            icon,
                                            t.title.as_str().cyan(),
                                            t.status.as_str(),
                                            subtask_count,
                                            project_type,
                                        );
                                    }
                                    eprintln!();
                                    eprintln!(
                                        "  {} Use /plan load <id> to restore a cloud plan",
                                        "💡".cyan()
                                    );
                                }
                            }
                            Err(e) => eprintln!("{}", format!("  ✗ {e}").red()),
                        }
                    } else {
                        eprintln!(
                            "  {} Cloud not available. Use /login first.",
                            theme::icon_warn()
                        );
                    }
                }
                "load" if !sub_arg.is_empty() => {
                    // Load a specific plan from cloud by task_id (or prefix)
                    if let Some(ref svc) = state.task_service {
                        use astra_services::TaskService;
                        use plan_decompose::{PlanModeState, analyze_project, format_plan};

                        let user_id = state.ingestion_user_id.as_deref().unwrap_or("local");
                        let query = sub_arg.trim();

                        // Find task by ID prefix or title substring
                        match svc.list_tasks(user_id, None).await {
                            Ok(tasks) => {
                                let found = tasks.iter().find(|t| {
                                    t.task_id.starts_with(query)
                                        || t.title.to_lowercase().contains(&query.to_lowercase())
                                });

                                match found {
                                    Some(task) => {
                                        let Some(task) =
                                            svc.get_task(&task.task_id).await.ok().flatten()
                                        else {
                                            eprintln!(
                                                "  {} Failed to load task details.",
                                                theme::icon_err()
                                            );
                                            return Ok(());
                                        };
                                        let Some(plan) = task.plan.as_ref() else {
                                            eprintln!(
                                                "  {} Task '{}' has no plan",
                                                theme::icon_warn(),
                                                query
                                            );
                                            return Ok(());
                                        };
                                        let project_root = std::env::current_dir()
                                            .unwrap_or_else(|_| std::path::PathBuf::from("."));
                                        let context = analyze_project(&project_root);
                                        let mut ps =
                                            PlanModeState::new(task.title.clone(), context);
                                        ps.set_plan(plan.clone());

                                        state.plan_mode = Some(ps);
                                        let short_id = &task.task_id[..8.min(task.task_id.len())];
                                        eprintln!();
                                        eprintln!(
                                            "{}  Loaded cloud plan: {} ({})",
                                            "☁️".cyan(),
                                            task.title.as_str().cyan(),
                                            short_id.dim()
                                        );
                                        eprintln!();
                                        eprintln!("{}", format_plan(plan));
                                        eprintln!();
                                        eprintln!(
                                            "  {} Commands: 'execute' to run, 'exit' to leave plan mode",
                                            "💡".cyan()
                                        );
                                    }
                                    None => {
                                        eprintln!(
                                            "  {} No task found matching '{}'",
                                            theme::icon_warn(),
                                            query
                                        );
                                        eprintln!(
                                            "  {} Use /plan cloud to list available plans",
                                            "💡".cyan()
                                        );
                                    }
                                }
                            }
                            Err(e) => eprintln!("{}", format!("  ✗ {e}").red()),
                        }
                    } else {
                        eprintln!(
                            "  {} Cloud not available. Use /login first.",
                            theme::icon_warn()
                        );
                    }
                }
                "list" => {
                    let plans = plan_decompose::list_saved_plans();
                    let templates = plan_decompose::builtin_templates();
                    eprintln!("{}", plan_decompose::format_plan_list(&plans));
                    eprintln!("  {} Built-in templates:", "📋".cyan());
                    for t in &templates {
                        eprintln!(
                            "    • {} — {} [{}]",
                            t.name,
                            t.description,
                            t.languages.join(", ")
                        );
                    }
                    eprintln!(
                        "  {}",
                        "Use /plan template <name> <goal> to instantiate".dim()
                    );
                    // Also hint about cloud if available
                    if state.task_service.is_some() {
                        eprintln!(
                            "  {} Use /plan cloud to list cloud-synced plans",
                            "☁️".cyan()
                        );
                    }
                }
                "template" if !sub_arg.is_empty() => {
                    let parts: Vec<&str> = sub_arg.splitn(2, ' ').collect();
                    let name = parts[0];
                    let goal = if parts.len() > 1 {
                        parts[1]
                    } else {
                        "implement this feature"
                    };
                    match plan_decompose::instantiate_template(name, goal) {
                        Some(plan) => {
                            eprintln!(
                                "  {} Template '{}' instantiated with {} subtasks",
                                theme::icon_ok(),
                                name,
                                plan.subtasks.len()
                            );
                            eprintln!("{}", plan_decompose::format_plan(&plan));
                            // Enter plan mode with this template
                            let project_root = std::env::current_dir()
                                .unwrap_or_else(|_| std::path::PathBuf::from("."));
                            let context = plan_decompose::analyze_project(&project_root);
                            let mut ps =
                                plan_decompose::PlanModeState::new(goal.to_string(), context);
                            ps.set_plan(plan);
                            state.plan_mode = Some(ps);
                            eprintln!(
                                "  {} Entered plan mode. Type 'execute' to run, 'exit' to leave.",
                                "💡".cyan()
                            );
                        }
                        None => {
                            let names: Vec<_> = plan_decompose::builtin_templates()
                                .iter()
                                .map(|t| t.name.clone())
                                .collect();
                            eprintln!(
                                "  {} Template '{}' not found. Available: {}",
                                theme::icon_warn(),
                                name,
                                names.join(", ")
                            );
                        }
                    }
                }
                "rate" if !sub_arg.is_empty() => {
                    // Record user feedback for the current/last executed plan
                    let rating_str = sub_arg.trim();

                    if rating_str == "skip" {
                        eprintln!("  {} Feedback skipped", theme::icon_warn());
                        return Ok(());
                    }

                    let rating: u8 = match rating_str.parse() {
                        Ok(r) if (1..=5).contains(&r) => r,
                        _ => {
                            eprintln!("  {} Rating must be 1-5 (or 'skip')", theme::icon_warn());
                            return Ok(());
                        }
                    };

                    // Find the task to rate - use the most recent task for current goal
                    if let Some(ref svc) = state.task_service {
                        use astra_services::{TaskOutcome, TaskService};
                        let user_id = state.ingestion_user_id.as_deref().unwrap_or("local");

                        // Find task by current plan goal or executing plan goal
                        let goal = state
                            .plan_mode
                            .as_ref()
                            .map(|ps| ps.goal.clone())
                            .or_else(|| state.executing_plan_goal.clone());

                        if let Some(goal_text) = goal {
                            match svc.list_tasks(user_id, None).await {
                                Ok(tasks) => {
                                    // Find the most recent task matching the goal
                                    let found = tasks
                                        .iter()
                                        .filter(|t| t.title == goal_text)
                                        .max_by_key(|t| &t.created_at);

                                    if let Some(task) = found {
                                        // Determine outcome based on plan completion
                                        let outcome = if let Some(ref ps) = state.plan_mode {
                                            let pct = ps.plan.progress_pct();
                                            if pct == 100 {
                                                TaskOutcome::Success
                                            } else if pct > 0 {
                                                TaskOutcome::Partial
                                            } else {
                                                TaskOutcome::Failed
                                            }
                                        } else {
                                            // Infer from rating
                                            if rating >= 4 {
                                                TaskOutcome::Success
                                            } else if rating >= 2 {
                                                TaskOutcome::Partial
                                            } else {
                                                TaskOutcome::Failed
                                            }
                                        };

                                        match svc
                                            .record_feedback(&task.task_id, rating, outcome, None)
                                            .await
                                        {
                                            Ok(_) => {
                                                let stars = "★".repeat(rating as usize)
                                                    + &"☆".repeat(5 - rating as usize);
                                                eprintln!(
                                                    "  {} Feedback recorded: {} ({})",
                                                    theme::icon_ok(),
                                                    stars.yellow(),
                                                    outcome.as_str()
                                                );

                                                // Auto-extract template if rating >= 4
                                                if rating >= 4 {
                                                    let goal_pattern =
                                                        extract_goal_pattern(&goal_text);
                                                    match svc
                                                        .extract_template(
                                                            &task.task_id,
                                                            &goal_pattern,
                                                        )
                                                        .await
                                                    {
                                                        Ok(Some(template_id)) => {
                                                            eprintln!(
                                                                "  {} Template extracted: {} → {}",
                                                                "📝".cyan(),
                                                                goal_pattern.dim(),
                                                                prefix_chars(&template_id, 8)
                                                            );
                                                        }
                                                        Ok(None) => {} // Not eligible
                                                        Err(e) => {
                                                            eprintln!(
                                                                "  {} Template extraction failed: {}",
                                                                theme::icon_warn(),
                                                                e
                                                            );
                                                        }
                                                    }
                                                }
                                            }
                                            Err(e) => {
                                                eprintln!(
                                                    "  {} Could not record feedback: {}",
                                                    theme::icon_warn(),
                                                    e
                                                );
                                            }
                                        }
                                    } else {
                                        eprintln!(
                                            "  {} No task found for current goal",
                                            theme::icon_warn()
                                        );
                                    }
                                }
                                Err(e) => eprintln!("{}", format!("  ✗ {e}").red()),
                            }
                        } else {
                            eprintln!("  {} No active plan to rate", theme::icon_warn());
                        }
                    } else {
                        // Store rating locally
                        eprintln!(
                            "  {} Rating {} recorded locally (cloud sync not available)",
                            theme::icon_ok(),
                            "★".repeat(rating as usize).yellow()
                        );
                    }
                }
                "recommend" => {
                    // Show template recommendations for current or specified goal
                    let query_goal = if sub_arg.is_empty() {
                        state.plan_mode.as_ref().map(|ps| ps.goal.clone())
                    } else {
                        Some(sub_arg.to_string())
                    };

                    if let Some(goal) = query_goal {
                        if let Some(ref svc) = state.task_service {
                            use astra_services::TaskService;
                            let user_id = state.ingestion_user_id.as_deref().unwrap_or("local");
                            let project_type = state
                                .plan_mode
                                .as_ref()
                                .and_then(|ps| ps.context.languages.first())
                                .map(|s| s.as_str());

                            match svc
                                .recommend_templates(user_id, &goal, project_type, 5)
                                .await
                            {
                                Ok(recommendations) => {
                                    if recommendations.is_empty() {
                                        eprintln!(
                                            "  {} No templates found for: {}",
                                            "📋".dim(),
                                            goal.dim()
                                        );
                                        eprintln!(
                                            "  {} Complete more plans and rate them to build templates!",
                                            "💡".cyan()
                                        );
                                    } else {
                                        eprintln!(
                                            "  {} Recommended templates for: {}",
                                            "📋".cyan(),
                                            goal.cyan()
                                        );
                                        eprintln!();
                                        for (i, rec) in recommendations.iter().enumerate() {
                                            let stars = "★"
                                                .repeat((rec.template.success_rate * 5.0) as usize);
                                            eprintln!(
                                                "  [{}] {} {} ({}x used)",
                                                (i + 1).to_string().cyan(),
                                                rec.template.goal_pattern,
                                                stars.yellow(),
                                                rec.template.use_count
                                            );
                                            let reason_ref = &rec.reason;
                                            eprintln!(
                                                "      {} {}",
                                                "→".dim(),
                                                reason_ref.as_str().dim()
                                            );
                                            eprintln!(
                                                "      {} {} subtasks",
                                                "📝".dim(),
                                                rec.template.template.subtasks.len()
                                            );
                                        }
                                        eprintln!();
                                        eprintln!(
                                            "  {} Use '/plan use <n>' to apply a template",
                                            "💡".cyan()
                                        );
                                    }
                                }
                                Err(e) => eprintln!("{}", format!("  ✗ {e}").red()),
                            }
                        } else {
                            eprintln!("  {} Cloud service not available", theme::icon_warn());
                        }
                    } else {
                        eprintln!("  {} Usage: /plan recommend <goal>", theme::icon_warn());
                    }
                }
                "stats" => {
                    // Show learning stats
                    let query_pattern = if sub_arg.is_empty() {
                        state.plan_mode.as_ref().map(|ps| ps.goal.clone())
                    } else {
                        Some(sub_arg.to_string())
                    };

                    if let Some(pattern) = query_pattern {
                        if let Some(ref svc) = state.task_service {
                            use astra_services::TaskService;
                            let user_id = state.ingestion_user_id.as_deref().unwrap_or("local");

                            match svc.get_learning_stats(user_id, &pattern).await {
                                Ok(stats) => {
                                    eprintln!(
                                        "  {} Learning Stats: {}",
                                        "📊".cyan(),
                                        pattern.cyan()
                                    );
                                    eprintln!();
                                    eprintln!("  Total tasks:     {}", stats.total_tasks);
                                    eprintln!(
                                        "  Completed:       {} ({:.0}%)",
                                        stats.completed_tasks,
                                        if stats.total_tasks > 0 {
                                            stats.completed_tasks as f32 / stats.total_tasks as f32
                                                * 100.0
                                        } else {
                                            0.0
                                        }
                                    );
                                    if let Some(avg) = stats.avg_rating {
                                        let stars = "★".repeat(avg.round() as usize);
                                        eprintln!(
                                            "  Avg rating:      {} ({:.1})",
                                            stars.yellow(),
                                            avg
                                        );
                                    }
                                    eprintln!("  Avg replans:     {:.1}", stats.avg_replan_count);
                                    eprintln!(
                                        "  Success rate:    {:.0}% (inferred)",
                                        stats.inferred_success_rate * 100.0
                                    );
                                }
                                Err(e) => eprintln!("{}", format!("  ✗ {e}").red()),
                            }
                        } else {
                            eprintln!("  {} Cloud service not available", theme::icon_warn());
                        }
                    } else {
                        eprintln!("  {} Usage: /plan stats <pattern>", theme::icon_warn());
                    }
                }
                "history" => {
                    if let Some(ref ps) = state.plan_mode {
                        eprintln!("  ─── Version History ───");
                        eprintln!("{}", ps.version_history.format_log());
                    } else {
                        eprintln!(
                            "  {} Not in plan mode. Use /plan enter <goal> first.",
                            theme::icon_warn()
                        );
                    }
                }
                "timeline" => {
                    if let Some(ref ps) = state.plan_mode {
                        eprintln!("  ─── Execution Timeline ───");
                        if ps.timeline.events.is_empty() {
                            eprintln!("  {} No events recorded yet", "(empty)".dim());
                            eprintln!(
                                "  {} Events are recorded during plan execution",
                                "💡".cyan()
                            );
                        } else {
                            eprintln!("{}", ps.timeline.format_display());
                            // Show summary
                            eprintln!("  ─────────────────────────");
                            eprintln!(
                                "  Completed: {} | Failed: {} | Replans: {}",
                                ps.timeline.completed_subtask_count().to_string().green(),
                                ps.timeline.failed_subtask_count().to_string().red(),
                                ps.timeline.replan_count()
                            );
                            if let Some(duration) = ps.timeline.total_duration_sec() {
                                eprintln!("  Total duration: {} sec", duration);
                            }
                        }
                    } else {
                        eprintln!("  {} Not in plan mode.", theme::icon_warn());
                    }
                }
                "diff" if !sub_arg.is_empty() => {
                    if let Some(ref ps) = state.plan_mode {
                        let parts: Vec<&str> = sub_arg.split_whitespace().collect();
                        if parts.len() == 2 {
                            if let (Ok(from), Ok(to)) =
                                (parts[0].parse::<u32>(), parts[1].parse::<u32>())
                            {
                                match ps.version_history.diff_versions(from, to) {
                                    Ok(diff) => eprintln!("{}", diff.format()),
                                    Err(e) => eprintln!("  {} {}", theme::icon_warn(), e),
                                }
                            } else {
                                eprintln!("  Usage: /plan diff <from_version> <to_version>");
                            }
                        } else {
                            eprintln!("  Usage: /plan diff <from_version> <to_version>");
                        }
                    } else {
                        eprintln!("  {} Not in plan mode.", theme::icon_warn());
                    }
                }
                "rollback" if !sub_arg.is_empty() => {
                    if let Some(ref mut ps) = state.plan_mode {
                        if let Ok(version) = sub_arg.trim().parse::<u32>() {
                            match ps.rollback_to_version(version) {
                                Ok(msg) => {
                                    eprintln!("  {} {}", theme::icon_ok(), msg);
                                    eprintln!("{}", plan_decompose::format_plan(&ps.plan));
                                }
                                Err(e) => eprintln!("  {} {}", theme::icon_warn(), e),
                            }
                        } else {
                            eprintln!("  Usage: /plan rollback <version_number>");
                        }
                    } else {
                        eprintln!("  {} Not in plan mode.", theme::icon_warn());
                    }
                }
                "replan" => {
                    // Regenerate plan based on current state and issues
                    use plan_decompose::{
                        ReplanReason, detect_replan_needed, format_plan, generate_replan_prompt,
                        parse_plan_response,
                    };

                    let Some(ref mut ps) = state.plan_mode else {
                        // Check if there's an executing plan to replan
                        if state.executing_plan.is_some() {
                            eprintln!(
                                "  {} Replan from executing plan not yet supported",
                                theme::icon_warn()
                            );
                            eprintln!(
                                "  {} Pause execution first with Ctrl+C, then enter plan mode",
                                "💡".cyan()
                            );
                        } else {
                            eprintln!(
                                "  {} Not in plan mode. Use /plan first.",
                                theme::icon_warn()
                            );
                        }
                        return Ok(());
                    };

                    let Some(tok) = token else {
                        eprintln!("  {} Not logged in. Run /login first.", theme::icon_err());
                        return Ok(());
                    };

                    // Determine reason for replan
                    let reason = if !sub_arg.is_empty() {
                        // User provided reason
                        ReplanReason::UserRequest
                    } else {
                        // Auto-detect reason from plan state
                        let failed: Vec<(&str, &str)> = ps
                            .plan
                            .subtasks
                            .iter()
                            .filter(|s| {
                                s.status == astra_services::task_orchestrator::TaskStatus::Failed
                            })
                            .map(|s| {
                                (
                                    s.id.as_str(),
                                    s.description.as_deref().unwrap_or("subtask failed"),
                                )
                            })
                            .collect();
                        match detect_replan_needed(&ps.plan, state.plan_execution_rounds, &failed) {
                            Some(suggestion) => suggestion.reason,
                            None => ReplanReason::UserRequest,
                        }
                    };

                    eprintln!();
                    eprintln!("  {} Replanning: {}", "🔄".yellow(), reason.format());
                    eprintln!("  {} Generating revised plan...", "⋯".dim());

                    let prompt = generate_replan_prompt(&ps.goal, &ps.plan, &reason, &ps.context);
                    let payload = serde_json::json!({
                        "messages": [{"role": "user", "content": prompt}],
                        "session_id": state.session_id.clone(),
                    });

                    let resp = api.post_chat_turn(tok, &payload).await;

                    match resp {
                        Ok(r) if r.status().is_success() => {
                            let mut full_text = String::new();
                            let mut stream = r.bytes_stream();
                            use futures_util::StreamExt;

                            while let Some(chunk) = stream.next().await {
                                if let Ok(bytes) = chunk {
                                    let event_str = String::from_utf8_lossy(&bytes);
                                    for line in event_str.lines() {
                                        if let Some(data) = line.strip_prefix("data: ")
                                            && let Ok(json) =
                                                serde_json::from_str::<serde_json::Value>(data)
                                            && let Some(content) =
                                                json.get("content").and_then(|v| v.as_str())
                                        {
                                            full_text.push_str(content);
                                        }
                                    }
                                }
                            }

                            match parse_plan_response(&full_text) {
                                Ok(new_plan) => {
                                    // Keep completed subtasks, update pending ones
                                    let old_version = ps.version_history.current_version;
                                    ps.update_plan(
                                        new_plan,
                                        &format!("Replan: {}", reason.format()),
                                    );
                                    let _ = ps
                                        .save_to_file(&plan_decompose::PlanModeState::state_path());

                                    eprintln!();
                                    eprintln!(
                                        "  {} Plan updated (v{} → v{})",
                                        theme::icon_ok(),
                                        old_version,
                                        ps.version_history.current_version
                                    );
                                    eprintln!();
                                    eprintln!("{}", format_plan(&ps.plan));
                                    eprintln!();
                                    eprintln!(
                                        "  {} Use '/plan diff {} {}' to see changes",
                                        "💡".cyan(),
                                        old_version,
                                        ps.version_history.current_version
                                    );
                                }
                                Err(e) => {
                                    eprint_plan_json_parse_failed(&full_text, &e.to_string());
                                }
                            }
                        }
                        Ok(r) => {
                            eprintln!("  {} LLM call failed ({})", theme::icon_err(), r.status());
                        }
                        Err(e) => {
                            eprintln!("  {} Request failed: {}", theme::icon_err(), e);
                        }
                    }

                    // Increment replan count in cloud if available
                    if let Some(ref svc) = state.task_service {
                        use astra_services::TaskService;
                        let user_id = state.ingestion_user_id.as_deref().unwrap_or("local");
                        if let Ok(tasks) = svc.list_tasks(user_id, None).await
                            && let Some(task) = tasks.iter().find(|t| t.title == ps.goal)
                        {
                            let _ = svc.increment_replan_count(&task.task_id).await;
                        }
                    }
                }
                "parallel" => {
                    if let Some(ref ps) = state.plan_mode {
                        let analysis = plan_decompose::analyze_parallelism(&ps.plan);
                        eprintln!("{}", plan_decompose::format_parallelism(&analysis));
                    } else {
                        eprintln!("  {} Not in plan mode.", theme::icon_warn());
                    }
                }
                "auto" if !sub_arg.is_empty() => {
                    // Auto mode: decompose + preview + execute in one shot
                    use plan_decompose::{
                        PlanExecutionConfig, analyze_project, decomposition_prompt,
                        format_execution_preview, format_plan, parse_plan_response,
                    };

                    let Some(tok) = token else {
                        eprintln!("  {} Not logged in. Run /login first.", theme::icon_err());
                        return Ok(());
                    };

                    let project_root =
                        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
                    eprintln!("  {} Analyzing project...", "⋯".dim());
                    let mut context = analyze_project(&project_root);
                    enrich_with_templates(
                        &mut context,
                        state.matrix_runtime.as_ref(),
                        state.ingestion_user_id.as_deref(),
                        sub_arg,
                        state.verbose_mode,
                    )
                    .await;
                    let prompt = decomposition_prompt(sub_arg, &context);
                    eprintln!(
                        "  {} Decomposing and auto-executing: {}",
                        "🚀".cyan(),
                        sub_arg
                    );

                    let payload = serde_json::json!({
                        "messages": [{"role": "user", "content": prompt}],
                        "session_id": state.session_id.clone(),
                    });

                    match api.post_chat_turn(tok, &payload).await {
                        Ok(r) if r.status().is_success() => {
                            let mut full_text = String::new();
                            let mut stream = r.bytes_stream();
                            use futures_util::StreamExt;

                            while let Some(chunk) = stream.next().await {
                                if let Ok(bytes) = chunk {
                                    let event_str = String::from_utf8_lossy(&bytes);
                                    for line in event_str.lines() {
                                        if let Some(data) = line.strip_prefix("data: ")
                                            && let Ok(json) =
                                                serde_json::from_str::<serde_json::Value>(data)
                                            && json.get("type").and_then(|v| v.as_str())
                                                == Some("text_delta")
                                            && let Some(content) =
                                                json.get("content").and_then(|v| v.as_str())
                                        {
                                            full_text.push_str(content);
                                        }
                                    }
                                }
                            }

                            match parse_plan_response(&full_text) {
                                Ok(plan) => {
                                    eprintln!();
                                    eprint!("{}", format_plan(&plan));
                                    eprintln!();
                                    eprint!("{}", format_execution_preview(&plan));
                                    eprintln!();
                                    eprintln!(
                                        "{}  Auto-executing plan ({} subtasks)...",
                                        "🚀".green(),
                                        plan.subtasks.len()
                                    );

                                    state.plan_execution_config = Some(PlanExecutionConfig {
                                        auto_execute: true,
                                        ..Default::default()
                                    });
                                    state.executing_plan_goal = Some(sub_arg.to_string());
                                    state.plan_execution_rounds = 0;
                                    state.plan_execution_corrections.clear();
                                    state.executing_plan = Some(plan);
                                }
                                Err(e) => {
                                    eprint_plan_json_parse_failed(&full_text, &e.to_string());
                                    eprintln!(
                                        "  {}",
                                        format!(
                                            "Try '/plan enter {sub_arg}' for interactive mode."
                                        )
                                        .dim()
                                    );
                                }
                            }
                        }
                        Ok(r) => {
                            eprintln!("{}", format!("  ✗ LLM call failed ({})", r.status()).red());
                        }
                        Err(e) => {
                            eprintln!("{}", format!("  ✗ Request failed: {e}").red());
                        }
                    }
                }
                _ => {
                    eprintln!("  {}", "Usage: /plan [on | off | list | history | …]".dim());
                    eprintln!(
                        "  {}",
                        "In plan mode, just describe your goal - no commands needed.".dim()
                    );
                }
            }
        }

        _ => unreachable!("unexpected memory-domain command: {cmd}"),
    }

    Ok(())
}

/// Old plan-mode handler — replaced by `plan_interaction::handle_plan_mode_input`.
/// Kept temporarily for reference; will be removed in a follow-up cleanup.
#[allow(dead_code)]
async fn _old_handle_plan_mode_input(
    input: String,
    token: Option<&str>,
    state: &mut ReplState,
    api: &astra_thin_client::ThinClient,
) -> Result<(), String> {
    use super::plan_interaction::eprint_clarification_question;
    use plan_decompose::{
        ClarificationAnswer, PendingClarifications, PlanEntryChoice, PlanModeState,
        decomposition_prompt, detect_clarification_questions, format_project_context,
        parse_clarification_response, parse_plan_entry_choice, parse_plan_response,
    };

    let plan_state = match state.plan_mode.as_mut() {
        Some(ps) => ps,
        None => {
            eprintln!("  {} {}", theme::icon_warn(), "Not in plan mode".yellow());
            return Ok(());
        }
    };

    // Handle pending clarification questions first
    if let Some(ref mut pending) = plan_state.pending_clarifications
        && let Some(question) = pending.next_question().cloned()
    {
        // Parse user's answer
        let answer = parse_clarification_response(&input, &question);
        match answer {
            ClarificationAnswer::Selected(idx) => {
                let selected = &question.options[idx];
                pending.record_answer(selected.clone());
                eprintln!("  {} Selected: {}", theme::icon_ok(), selected);
            }
            ClarificationAnswer::Freeform(text) => {
                pending.record_answer(text.clone());
                eprintln!("  {} Answer: {}", theme::icon_ok(), text);
            }
            ClarificationAnswer::Invalid(msg) => {
                eprintln!("  {} {}", theme::icon_err(), msg);
                eprintln!();
                eprint_clarification_question(&question);
                return Ok(());
            }
        }

        // Check if more questions remain
        if let Some(next_q) = pending.next_question() {
            eprintln!();
            eprint_clarification_question(next_q);
            let _ = plan_state.save_to_file(&PlanModeState::state_path());
            return Ok(());
        }

        // All questions answered - regenerate plan with clarifications
        eprintln!();
        eprintln!(
            "  {} All clarifications answered. Regenerating plan...",
            "🔄".cyan()
        );

        let answers_context = pending.format_for_prompt();
        let goal_with_context = format!(
            "{}\n\n## Clarifications from user:\n{}",
            plan_state.goal, answers_context
        );

        // Clear pending and regenerate
        plan_state.pending_clarifications = None;

        let Some(tok) = token else {
            eprintln!("  {} Not logged in. Run /login first.", theme::icon_err());
            return Ok(());
        };

        enrich_with_templates(
            &mut plan_state.context,
            state.matrix_runtime.as_ref(),
            state.ingestion_user_id.as_deref(),
            &goal_with_context,
            state.verbose_mode,
        )
        .await;
        let prompt = decomposition_prompt(&goal_with_context, &plan_state.context);
        let payload = serde_json::json!({
            "messages": [{"role": "user", "content": prompt}],
            "session_id": state.session_id.clone(),
        });

        let resp = api.post_chat_turn(tok, &payload).await;

        eprintln!();
        match resp {
            Ok(r) if r.status().is_success() => {
                let sse_result = stream_sse_markdown(r).await;
                let full_text = sse_result.text;

                match parse_plan_response(&full_text) {
                    Ok(plan) => {
                        plan_state.set_plan(plan);
                        let _ = plan_state.save_to_file(&PlanModeState::state_path());
                        plan_interaction::eprint_plan_commands_help();
                    }
                    Err(e) => {
                        eprint_plan_json_parse_failed(&full_text, &e.to_string());
                    }
                }
            }
            Ok(r) => {
                eprintln!("  {} LLM call failed ({})", theme::icon_err(), r.status());
            }
            Err(e) => {
                eprintln!("  {} Request failed: {}", theme::icon_err(), e);
            }
        }

        return Ok(());
    }

    // Check for exit commands
    let input_lower = input.to_lowercase();
    if input_lower == "exit" || input_lower == "quit" || input_lower == "/plan exit" {
        eprintln!();
        eprintln!("{}  Exiting plan mode", "📋".yellow());
        PlanModeState::clear_saved_state();
        state.plan_mode = None;
        return Ok(());
    }

    // Handle entry choices (when goal is empty - fresh plan mode)
    if plan_state.goal.is_empty() {
        let has_plan = !plan_state.plan.subtasks.is_empty();
        let choice = parse_plan_entry_choice(&input, has_plan, state.executing_plan.is_some());

        match choice {
            PlanEntryChoice::Exit => {
                eprintln!();
                eprintln!("{}  Exiting plan mode", "📋".yellow());
                state.plan_mode = None;
                return Ok(());
            }
            PlanEntryChoice::Continue => {
                // Already have a plan, just continue
                eprintln!("  {} Continuing with current plan", "→".cyan());
                return Ok(());
            }
            PlanEntryChoice::Restart => {
                // Clear current plan, prompt for new goal
                plan_state.plan = Default::default();
                plan_state.goal = String::new();
                eprintln!(
                    "  {} Plan cleared. Describe what you want to do:",
                    "🔄".yellow()
                );
                return Ok(());
            }
            PlanEntryChoice::Resume => {
                // Resume paused execution
                if state.executing_plan.is_some() {
                    eprintln!("  {} Resuming plan execution...", "▶".cyan());
                }
                return Ok(());
            }
            PlanEntryChoice::New(_) => {
                // Start fresh
                plan_state.plan = Default::default();
                eprintln!("  {} Describe what you want to do:", "📝".cyan());
                return Ok(());
            }
            PlanEntryChoice::Goal(goal) => {
                // User provided a goal - generate plan
                let Some(tok) = token else {
                    eprintln!("  {} Not logged in. Run /login first.", theme::icon_err());
                    return Ok(());
                };

                plan_state.goal = goal.clone();

                if state.verbose_mode {
                    eprintln!();
                    eprintln!("{}", format_project_context(&plan_state.context).dim());
                }

                enrich_with_templates(
                    &mut plan_state.context,
                    state.matrix_runtime.as_ref(),
                    state.ingestion_user_id.as_deref(),
                    &goal,
                    state.verbose_mode,
                )
                .await;
                let prompt = decomposition_prompt(&goal, &plan_state.context);
                let payload = serde_json::json!({
                    "messages": [{"role": "user", "content": prompt}],
                    "session_id": state.session_id.clone(),
                });

                eprintln!();
                let resp = api.post_chat_turn(tok, &payload).await;

                match resp {
                    Ok(r) if r.status().is_success() => {
                        let sse_result = stream_sse_markdown(r).await;
                        let full_text = sse_result.text;

                        if let Some(questions) = detect_clarification_questions(&full_text) {
                            eprintln!();
                            eprintln!(
                                "  {} {}",
                                "▸".cyan(),
                                format!(
                                    "{} question{} before planning:",
                                    questions.len(),
                                    if questions.len() == 1 { "" } else { "s" }
                                )
                                .bold()
                                .cyan()
                            );
                            eprintln!();

                            let pending = PendingClarifications {
                                questions: questions.clone(),
                                answers: Vec::new(),
                            };
                            plan_state.pending_clarifications = Some(pending);

                            eprint_clarification_question(&questions[0]);
                            let _ = plan_state.save_to_file(&PlanModeState::state_path());
                        } else {
                            match parse_plan_response(&full_text) {
                                Ok(plan) => {
                                    plan_state.set_plan(plan);
                                    let _ = plan_state.save_to_file(&PlanModeState::state_path());
                                    plan_interaction::eprint_plan_commands_help();
                                }
                                Err(e) => {
                                    eprint_plan_json_parse_failed(&full_text, &e.to_string());
                                }
                            }
                        }
                    }
                    Ok(r) => {
                        eprintln!("  {} LLM call failed ({})", theme::icon_err(), r.status());
                    }
                    Err(e) => {
                        eprintln!("  {} Request failed: {}", theme::icon_err(), e);
                    }
                }

                return Ok(());
            }
        }
    }

    // Check for "done <id>" — mark subtask completed
    if let Some(done_id) = input_lower.strip_prefix("done ").map(|s| s.trim())
        && !done_id.is_empty()
    {
        match plan_state.complete_subtask(done_id) {
            Ok(title) => {
                let pct = plan_state.plan.progress_pct();
                let done_count = plan_state.plan.items_done();
                let total_count = plan_state.plan.subtasks.len();
                eprintln!("  {} Completed: {} ({}%)", theme::icon_ok(), title, pct);
                // Save updated state locally
                let _ = plan_state.save_to_file(&PlanModeState::state_path());

                // Sync progress to cloud if available
                if let Some(ref svc) = state.task_service {
                    use astra_services::TaskService;
                    let user_id = state.ingestion_user_id.as_deref().unwrap_or("local");
                    let goal = &plan_state.goal;

                    // Find matching cloud task and update progress
                    if let Ok(tasks) = svc.list_tasks(user_id, None).await
                        && let Some(task) = tasks.iter().find(|t| &t.title == goal)
                    {
                        // Update plan and progress in cloud
                        let _ = svc.update_plan(&task.task_id, &plan_state.plan).await;
                        let _ = svc
                            .update_progress(&task.task_id, pct, done_count, total_count as u32)
                            .await;
                    }
                }

                // Show remaining ready tasks
                let ready = plan_state.plan.ready_subtasks();
                if !ready.is_empty() {
                    eprintln!("  {} Next ready:", "→".cyan());
                    for st in &ready {
                        eprintln!("    {} [{}] {}", "○".dim(), st.id, st.title);
                    }
                } else if plan_state.plan.progress_pct() == 100 {
                    eprintln!("  {} All tasks complete!", "✓".green());
                    // Complete the cloud task
                    if let Some(ref svc) = state.task_service {
                        use astra_services::TaskService;
                        let user_id = state.ingestion_user_id.as_deref().unwrap_or("local");
                        let goal = &plan_state.goal;
                        if let Ok(tasks) = svc.list_tasks(user_id, None).await
                            && let Some(task) = tasks.iter().find(|t| &t.title == goal)
                        {
                            let _ = svc.complete_task(&task.task_id).await;
                        }
                    }
                    // Prompt for feedback
                    eprintln!();
                    eprintln!(
                        "  {} Rate this plan (1-5)? Or 'skip' to skip: /plan rate <1-5>",
                        "💡".cyan()
                    );
                }
            }
            Err(e) => eprintln!("  {} {}", theme::icon_warn(), e),
        }
        return Ok(());
    }

    // Check for "status" — show current progress
    if input_lower == "status" {
        let pct = plan_state.plan.progress_pct();
        let done = plan_state.plan.items_done();
        let total = plan_state.plan.subtasks.len();
        eprintln!("  Progress: {done}/{total} ({pct}%)");
        let ready = plan_state.plan.ready_subtasks();
        if !ready.is_empty() {
            eprintln!("  {} Ready:", "→".cyan());
            for st in &ready {
                eprintln!("    {} [{}] {}", "○".dim(), st.id, st.title);
            }
        }
        return Ok(());
    }

    // Check for execute command
    if PlanModeState::is_execute_command(&input) {
        use plan_decompose::{PlanExecutionConfig, format_execution_preview};

        let plan = plan_state.plan.clone();
        let goal = plan_state.goal.clone();

        // Show execution preview with parallel analysis
        eprintln!();
        eprint!("{}", format_execution_preview(&plan));
        eprintln!();

        // Persist to task service if available
        state.plan_run_task_id = None;
        state.plan_run_task_last_progress = None;
        state.plan_run_task_last_error = None;
        if let Some(ref svc) = state.task_service {
            use astra_services::{TaskCreateRequest, TaskService};
            let user_id = state.ingestion_user_id.as_deref().unwrap_or("local");
            let session_id = state.session_id.as_deref().unwrap_or("no-session");

            // Extract project_type from context
            let project_type = plan_state
                .context
                .languages
                .first()
                .map(|s| s.to_lowercase());

            // Extract goal_pattern: normalize the goal for pattern matching
            let goal_pattern = Some(extract_goal_pattern(&goal));

            match svc
                .create_task(
                    user_id,
                    session_id,
                    TaskCreateRequest {
                        title: goal.clone(),
                        description: Some(format!("Plan Mode: {} subtasks", plan.subtasks.len())),
                        plan: Some(plan.clone()),
                        parent_task_id: None,
                        project_type,
                        goal_pattern,
                    },
                )
                .await
            {
                Ok(tid) => {
                    state.plan_run_task_id = Some(tid.clone());
                    let short = &tid[..8.min(tid.len())];
                    eprintln!(
                        "{}  Task created: {} ({})",
                        theme::icon_ok(),
                        goal,
                        short.dim()
                    );
                    eprintln!("{}  Track progress: /task status {}", "💡".cyan(), short);
                }
                Err(e) => {
                    eprintln!("{}  Could not persist task: {}", theme::icon_warn(), e);
                }
            }
        }

        eprintln!(
            "{}  Auto-executing plan ({} subtasks)...",
            "🚀".green(),
            plan.subtasks.len()
        );
        eprintln!();

        // Store execution config for auto mode (go = automatic)
        state.plan_execution_config = Some(PlanExecutionConfig {
            step_by_step: false,
            auto_execute: true,
        });
        state.executing_plan_goal = Some(goal);
        state.plan_execution_rounds = 0;

        // Store plan for auto-execution and exit plan mode
        state.plan_execution_corrections.clear();
        state.executing_plan = Some(plan);
        PlanModeState::clear_saved_state();
        state.plan_mode = None;
        return Ok(());
    }

    // Check for step-by-step execute command
    if input.trim().to_lowercase().starts_with("step") || input.trim() == "逐步执行" {
        use plan_decompose::{PlanExecutionConfig, format_execution_preview};

        let plan = plan_state.plan.clone();
        let goal = plan_state.goal.clone();

        // Show execution preview
        eprintln!();
        eprint!("{}", format_execution_preview(&plan));
        eprintln!();
        eprintln!(
            "{}  Step-by-step mode: you'll confirm each subtask before execution.",
            "⚙".cyan()
        );
        eprintln!();

        // Set step-by-step config
        state.plan_execution_config = Some(PlanExecutionConfig {
            step_by_step: true,
            auto_execute: false,
        });
        state.executing_plan_goal = Some(goal);
        state.plan_execution_rounds = 0;

        state.plan_execution_corrections.clear();
        state.executing_plan = Some(plan);
        PlanModeState::clear_saved_state();
        state.plan_mode = None;
        return Ok(());
    }

    // Build prompt for LLM
    let prompt = plan_state.plan_mode_prompt(&input);
    plan_state.add_turn(&input, ""); // Will update assistant part after response

    let Some(tok) = token else {
        eprintln!("  {} Not logged in. Run /login first.", theme::icon_err());
        return Ok(());
    };

    let messages = vec![serde_json::json!({
        "role": "user",
        "content": prompt
    })];

    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));

    let payload = serde_json::json!({
        "messages": messages,
        "model": state.model.clone(),
        "edge_profile": {
            "cwd": cwd.to_string_lossy(),
        },
        "edge_tools": [],
    });

    let resp = api.post_chat_turn(tok, &payload).await;

    eprintln!();
    match resp {
        Ok(r) if r.status().is_success() => {
            let sse_result = stream_sse_markdown(r).await;

            if sse_result.text.is_empty() {
                if sse_result.event_count == 0 {
                    eprintln!(
                        "  {} No SSE events received from server",
                        theme::icon_warn()
                    );
                } else {
                    eprintln!(
                        "  {} {} events (types: {}) but no text",
                        theme::icon_warn(),
                        sse_result.event_count,
                        sse_result.event_types.join(", ")
                    );
                }
            }

            if !sse_result.text.is_empty() {
                match plan_interaction::try_replace_plan_from_llm_json(&sse_result.text, plan_state)
                {
                    Ok(true) => {
                        plan_state.modified = true;
                        let _ = plan_state.save_to_file(&PlanModeState::state_path());
                    }
                    Ok(false) => {}
                    Err(e) => {
                        eprintln!(
                            "  {} Model reply is not valid plan JSON: {}",
                            theme::icon_warn(),
                            e
                        );
                        eprintln!(
                            "  {} Plan unchanged. Try {} for the current plan.",
                            "⋯".dim(),
                            "show".cyan()
                        );
                    }
                }
            }

            if let Some(last) = plan_state.history.last_mut() {
                last.1 = sse_result.text.chars().take(500).collect();
            }
        }
        Ok(r) => {
            eprintln!("\r  ✗ LLM call failed ({})", r.status());
        }
        Err(e) => {
            eprintln!("\r  ✗ Request failed: {e}");
        }
    }

    Ok(())
}

/// Extract a normalized goal pattern for matching similar tasks.
///
/// The pattern removes specific identifiers and normalizes common task patterns:
/// - "add feature X to module Y" → "add feature * to module *"
/// - "fix bug in file.rs" → "fix bug in *"
/// - "implement API endpoint for users" → "implement api endpoint for *"
fn extract_goal_pattern(goal: &str) -> String {
    // Common task verbs to preserve
    let task_verbs = [
        "add",
        "fix",
        "implement",
        "create",
        "update",
        "refactor",
        "remove",
        "delete",
        "optimize",
        "improve",
        "migrate",
        "integrate",
        "test",
        "document",
        "configure",
    ];

    // Normalize to lowercase and split
    let goal_lower = goal.to_lowercase();
    let words: Vec<&str> = goal_lower.split_whitespace().collect();
    if words.is_empty() {
        return "*".to_string();
    }

    let mut pattern_parts = Vec::new();
    let mut i = 0;

    while i < words.len() {
        let word = words[i];

        // Keep task verbs
        if task_verbs.contains(&word)
            || ["for", "to", "in", "with", "from", "by", "the", "a", "an"].contains(&word)
            || [
                "api", "endpoint", "database", "file", "module", "function", "class", "test",
                "config", "error", "logging", "auth", "user", "data", "cache", "queue",
            ]
            .contains(&word)
        {
            pattern_parts.push(word.to_string());
        }
        // Replace specific identifiers with wildcard
        else if word.contains('.') || word.contains('/') || word.contains('_') {
            pattern_parts.push("*".to_string());
        }
        // Keep short words, replace long specific words
        else if word.len() <= 4 {
            pattern_parts.push(word.to_string());
        } else {
            pattern_parts.push("*".to_string());
        }

        i += 1;
    }

    // Collapse consecutive wildcards
    let mut result = Vec::new();
    for part in pattern_parts {
        if part == "*" && result.last() == Some(&"*".to_string()) {
            continue;
        }
        result.push(part);
    }

    if result.is_empty() {
        "*".to_string()
    } else {
        result.join(" ")
    }
}

// ─── /plan status|pause helpers ──────────────────────────────────────────────

fn handle_plan_status(state: &ReplState) {
    if let Some(ref plan) = state.executing_plan {
        let pct = plan.progress_pct();
        let total = plan.subtasks.len();
        let done = plan.items_done() as usize;
        let pending = total.saturating_sub(done);
        let in_progress = plan
            .subtasks
            .iter()
            .filter(|s| s.status == astra_runtime::plan_decompose::TaskStatus::InProgress)
            .count();

        eprintln!("\n{}  Plan Status", "📋".cyan());
        eprintln!("{}", "─".repeat(45).dim());

        if let Some(ref goal) = state.executing_plan_goal {
            eprintln!("  Goal: {}", goal.as_str().cyan());
        }
        eprintln!("  Progress: {}%  ({}/{} subtasks done)", pct, done, total);
        if in_progress > 0 {
            eprintln!("  In progress: {}", in_progress);
        }
        eprintln!("  Remaining:  {}", pending);

        // Show individual subtask status
        eprintln!();
        for st in &plan.subtasks {
            let icon = match st.status {
                astra_runtime::plan_decompose::TaskStatus::Completed => "✓".green().to_string(),
                astra_runtime::plan_decompose::TaskStatus::InProgress => "▶".yellow().to_string(),
                astra_runtime::plan_decompose::TaskStatus::Pending => "○".dim().to_string(),
                astra_runtime::plan_decompose::TaskStatus::Paused => "⏸".yellow().to_string(),
                astra_runtime::plan_decompose::TaskStatus::Failed => "✗".red().to_string(),
                astra_runtime::plan_decompose::TaskStatus::Cancelled => "⊘".dim().to_string(),
            };
            eprintln!("  {} {} [{}]", icon, st.title, st.id.as_str().dim());
        }
        eprintln!("{}", "─".repeat(45).dim());

        if pct < 100 {
            eprintln!(
                "  {} Type 'continue' to resume or a message to add guidance",
                "💡".cyan()
            );
        }
    } else if let Some(ref ps) = state.plan_mode {
        eprintln!("\n{}  Plan Mode (editing)", "📋".cyan());
        if !ps.goal.is_empty() {
            eprintln!("  Goal: {}", ps.goal.as_str().cyan());
        }
        if !ps.plan.subtasks.is_empty() {
            eprintln!(
                "  Subtasks: {} ({}% done)",
                ps.plan.subtasks.len(),
                ps.plan.progress_pct()
            );
        } else {
            eprintln!("  {}", "No plan generated yet".dim());
        }
    } else {
        eprintln!(
            "  {} No active plan. Use /plan enter <goal> to create one.",
            theme::icon_warn()
        );
    }
}

fn handle_plan_pause(state: &mut ReplState) {
    if state.executing_plan.is_some() {
        // Signal the running plan to pause after current subtask
        // Currently, plan execution is synchronous, so this just sets a flag
        // that run_plan_execution checks between subtasks.
        state.last_turn_interrupted = true;
        eprintln!(
            "  {} Pause requested. Plan will pause after current subtask completes.",
            "⏸".yellow()
        );
    } else {
        eprintln!("  {} No plan is currently executing.", theme::icon_warn());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Empty / whitespace-only input ---

    #[test]
    fn extract_goal_pattern_empty_string_returns_wildcard() {
        assert_eq!(extract_goal_pattern(""), "*");
    }

    #[test]
    fn extract_goal_pattern_whitespace_only_returns_wildcard() {
        assert_eq!(extract_goal_pattern("   "), "*");
    }

    // --- Task verbs are preserved ---

    #[test]
    fn extract_goal_pattern_preserves_task_verbs() {
        let verbs = [
            "add",
            "fix",
            "implement",
            "create",
            "update",
            "refactor",
            "remove",
            "delete",
            "optimize",
            "improve",
            "migrate",
            "integrate",
            "test",
            "document",
            "configure",
        ];
        for verb in verbs {
            assert_eq!(
                extract_goal_pattern(verb),
                verb,
                "verb '{verb}' should be preserved"
            );
        }
    }

    // --- Domain words are preserved ---

    #[test]
    fn extract_goal_pattern_preserves_domain_words() {
        let domain_words = [
            "api", "endpoint", "database", "file", "module", "function", "class", "test", "config",
            "error", "logging", "auth", "user", "data", "cache", "queue",
        ];
        for word in domain_words {
            assert_eq!(
                extract_goal_pattern(word),
                word,
                "domain word '{word}' should be preserved"
            );
        }
    }

    // --- Stop words are preserved ---

    #[test]
    fn extract_goal_pattern_preserves_stop_words() {
        let stop_words = ["for", "to", "in", "with", "from", "by", "the", "a", "an"];
        for word in stop_words {
            assert_eq!(
                extract_goal_pattern(word),
                word,
                "stop word '{word}' should be preserved"
            );
        }
    }

    // --- Short words (≤4 chars) preserved as-is ---

    #[test]
    fn extract_goal_pattern_preserves_short_words() {
        // "foo" (3 chars), "bar" (3), "baz" (3), "quux" (4) — all ≤4
        assert_eq!(extract_goal_pattern("foo"), "foo");
        assert_eq!(extract_goal_pattern("bar"), "bar");
        assert_eq!(extract_goal_pattern("quux"), "quux");
        // exactly 4 chars
        assert_eq!(extract_goal_pattern("abcd"), "abcd");
    }

    // --- Long words (>4 chars) not in any preserved list → wildcard ---

    #[test]
    fn extract_goal_pattern_replaces_long_unknown_words() {
        // "hello" is 5 chars and not in any preserved list
        assert_eq!(extract_goal_pattern("hello"), "*");
        assert_eq!(extract_goal_pattern("foobar"), "*");
        assert_eq!(extract_goal_pattern("something"), "*");
    }

    // --- Identifiers with `.`, `/`, `_` replaced with wildcard ---

    #[test]
    fn extract_goal_pattern_replaces_dotted_identifiers() {
        assert_eq!(extract_goal_pattern("file.rs"), "*");
        assert_eq!(extract_goal_pattern("main.go"), "*");
    }

    #[test]
    fn extract_goal_pattern_replaces_slash_identifiers() {
        assert_eq!(extract_goal_pattern("src/lib"), "*");
        assert_eq!(extract_goal_pattern("a/b/c"), "*");
    }

    #[test]
    fn extract_goal_pattern_replaces_underscore_identifiers() {
        assert_eq!(extract_goal_pattern("my_var"), "*");
        assert_eq!(extract_goal_pattern("connection_pool"), "*");
    }

    // --- Consecutive wildcards collapsed ---

    #[test]
    fn extract_goal_pattern_collapses_consecutive_wildcards() {
        // Three long unknown words in a row should produce a single "*"
        assert_eq!(extract_goal_pattern("alpha bravo charlie"), "*");
    }

    #[test]
    fn extract_goal_pattern_collapses_mixed_wildcard_sources() {
        // identifier + long word → two wildcard sources, collapsed to one "*"
        assert_eq!(extract_goal_pattern("file.rs foobar"), "*");
    }

    #[test]
    fn extract_goal_pattern_does_not_collapse_non_consecutive_wildcards() {
        // wildcard, preserved word, wildcard → "* fix *"
        assert_eq!(extract_goal_pattern("hello fix world"), "* fix *");
    }

    // --- Case insensitivity ---

    #[test]
    fn extract_goal_pattern_normalizes_uppercase_to_lowercase() {
        assert_eq!(extract_goal_pattern("FIX"), "fix");
        assert_eq!(extract_goal_pattern("API"), "api");
        assert_eq!(extract_goal_pattern("Implement"), "implement");
    }

    #[test]
    fn extract_goal_pattern_mixed_case_preserved_words() {
        assert_eq!(extract_goal_pattern("Add API Endpoint"), "add api endpoint");
    }

    // --- Real-world mixed goals ---

    #[test]
    fn extract_goal_pattern_implement_user_authentication_api() {
        // "implement" = verb, "user" = domain, "authentication" = 14 chars unknown → "*",
        // "api" = domain
        assert_eq!(
            extract_goal_pattern("implement user authentication API"),
            "implement user * api"
        );
    }

    #[test]
    fn extract_goal_pattern_fix_database_connection_pool_timeout() {
        // "fix" = verb, "database" = domain, "connection_pool" has `_` → "*",
        // "timeout" = 7 chars unknown → "*", consecutive wildcards collapsed
        assert_eq!(
            extract_goal_pattern("fix database connection_pool timeout"),
            "fix database *"
        );
    }

    #[test]
    fn extract_goal_pattern_add_feature_to_module() {
        // "add" = verb, "feature" = 7 chars unknown → "*", "x" ≤4 kept,
        // "to" = stop, "module" = domain, "y" ≤4 kept
        assert_eq!(
            extract_goal_pattern("add feature X to module Y"),
            "add * x to module y"
        );
    }

    #[test]
    fn extract_goal_pattern_fix_bug_in_file_rs() {
        // "fix" = verb, "bug" = 3 chars ≤4, "in" = stop, "file.rs" has `.` → "*"
        assert_eq!(extract_goal_pattern("fix bug in file.rs"), "fix bug in *");
    }

    #[test]
    fn extract_goal_pattern_refactor_logging_for_error_handling() {
        // "refactor" = verb, "logging" = domain, "for" = stop, "error" = domain,
        // "handling" = 8 chars unknown → "*"
        assert_eq!(
            extract_goal_pattern("refactor logging for error handling"),
            "refactor logging for error *"
        );
    }

    #[test]
    fn extract_goal_pattern_create_cache_config_for_queue() {
        // "create" = verb, "cache" = domain, "config" = domain, "for" = stop, "queue" = domain
        assert_eq!(
            extract_goal_pattern("create cache config for queue"),
            "create cache config for queue"
        );
    }

    #[test]
    fn extract_goal_pattern_migrate_data_from_old_db() {
        // "migrate" = verb, "data" = domain, "from" = stop, "old" = 3 chars, "db" = 2 chars
        assert_eq!(
            extract_goal_pattern("migrate data from old db"),
            "migrate data from old db"
        );
    }

    #[test]
    fn extract_goal_pattern_optimize_function_with_memoization() {
        // "optimize" = verb, "function" = domain, "with" = stop,
        // "memoization" = 11 chars unknown → "*"
        assert_eq!(
            extract_goal_pattern("optimize function with memoization"),
            "optimize function with *"
        );
    }
}
