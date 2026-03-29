use super::*;

pub(super) async fn handle_memory_domain_command(
    cmd: &str,
    arg: &str,
    client: &reqwest::Client,
    base: &str,
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
                    match client
                        .post(format!("{base}/memory/search"))
                        .headers(auth_headers(tok)?)
                        .json(&payload)
                        .send()
                        .await
                    {
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
                                        let short_id = if id.len() > 8 { &id[..8] } else { id };
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
                    match client
                        .post(format!("{base}/memory/search"))
                        .headers(auth_headers(tok)?)
                        .json(&payload)
                        .send()
                        .await
                    {
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
                                    eprintln!("  {} memories", arr.len());
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
                    eprintln!("  Usage: /memory [list | search <query>]");
                }
            }
        }

        "/plan" => {
            let Some(tok) = token else {
                eprintln!("{}", "  Not logged in. Use /login.".yellow());
                return Ok(());
            };
            let subcmd = arg.split_whitespace().next().unwrap_or("");
            let sub_arg = arg.strip_prefix(subcmd).unwrap_or("").trim();
            match subcmd {
                // Smart entry: /plan with no args shows entry card and enters plan mode
                "" => {
                    use super::plan_decompose::{
                        PlanModeState, format_plan_entry_card, format_plan,
                    };
                    
                    // Check for active plan in state or saved state
                    let has_active = state.plan_mode.is_some();
                    let _has_paused = state.executing_plan.is_some();
                    
                    // Try to load saved plan if not in memory
                    let saved_plan = if !has_active {
                        PlanModeState::load_from_file(&PlanModeState::state_path()).ok()
                    } else {
                        None
                    };
                    
                    // Display entry card
                    eprintln!();
                    let card = format_plan_entry_card(
                        state.plan_mode.as_ref().or(saved_plan.as_ref()),
                        state.executing_plan.as_ref(),
                    );
                    eprintln!("{}", card);
                    
                    // If we loaded a saved plan, restore it
                    if saved_plan.is_some() && state.plan_mode.is_none() {
                        state.plan_mode = saved_plan;
                        if let Some(ref ps) = state.plan_mode {
                            eprintln!("  {} Restored saved plan: {}", "↩".cyan(), ps.goal.as_str().cyan());
                            eprintln!();
                            let formatted = format_plan(&ps.plan);
                            eprintln!("{formatted}");
                        }
                    }
                    
                    // Enter plan mode (plan> prompt will be shown by main loop)
                    if state.plan_mode.is_none() {
                        // Create empty plan mode state - user will provide goal
                        let project_root = std::env::current_dir()
                            .unwrap_or_else(|_| std::path::PathBuf::from("."));
                        let context = super::plan_decompose::analyze_project(&project_root);
                        state.plan_mode = Some(PlanModeState::new(String::new(), context));
                    }
                }
                "show" => {
                    let payload = prompts::memory_proto::MemoryEntry::search_query(
                        prompts::memory_proto::NS_PLAN,
                        "current goals",
                    );
                    match client
                        .post(format!("{base}/memory/search"))
                        .headers(auth_headers(tok)?)
                        .json(&payload)
                        .send()
                        .await
                    {
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
                    let meta = prompts::memory_proto::EntryMeta::from_session(
                        state.session_id.as_deref(),
                        state.turn,
                        prompts::memory_proto::SRC_USER,
                    );
                    match client
                        .post(format!("{base}/memory/store"))
                        .headers(auth_headers(tok)?)
                        .json(&entry.to_store_payload_with_meta(&meta))
                        .send()
                        .await
                    {
                        Ok(r) if r.status().is_success() => {
                            eprintln!("  {} Plan saved to memory.", "✓".green());
                        }
                        Ok(r) => eprintln!("{}", format!("  ✗ Failed ({})", r.status()).red()),
                        Err(e) => eprintln!("{}", format!("  ✗ Unreachable: {e}").red()),
                    }
                }
                "clear" => {
                    let payload = prompts::memory_proto::MemoryEntry::purge_payload(
                        prompts::memory_proto::NS_PLAN,
                    );
                    match client
                        .post(format!("{base}/memory/purge"))
                        .headers(auth_headers(tok)?)
                        .json(&payload)
                        .send()
                        .await
                    {
                        Ok(r) if r.status().is_success() => {
                            eprintln!("  {} Plan cleared.", "✓".green());
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
                    let context = crate::plan_decompose::analyze_project(&project_root);

                    eprintln!(
                        "  {} {} languages, {} files, {}",
                        "✓".green(),
                        context.languages.len(),
                        context.source_file_count,
                        context.entry_points.join(", ")
                    );

                    // Generate the decomposition prompt
                    let prompt = crate::plan_decompose::decomposition_prompt(sub_arg, &context);

                    // Store the goal in plan memory
                    let entry = prompts::memory_proto::MemoryEntry::new(
                        prompts::memory_proto::NS_PLAN,
                        prompts::memory_proto::ST_ACTIVE,
                        sub_arg,
                    );
                    let store_payload = entry.to_store_payload();
                    let _ = client
                        .post(format!("{base}/memory/store"))
                        .headers(auth_headers(tok)?)
                        .json(&store_payload)
                        .send()
                        .await;

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

                    match client
                        .post(format!("{base}/chat/turn"))
                        .headers(auth_headers(tok)?)
                        .header("Accept", "text/event-stream")
                        .json(&payload)
                        .send()
                        .await
                    {
                        Ok(resp) if resp.status().is_success() => {
                            // Collect text from SSE stream
                            let mut full_text = String::new();
                            let mut stream = resp.bytes_stream();
                            let mut buffer = String::new();

                            use futures_util::StreamExt;
                            while let Some(chunk) = stream.next().await {
                                let Ok(chunk) = chunk else { break };
                                buffer.push_str(&String::from_utf8_lossy(&chunk));

                                // Parse SSE events
                                while let Some(event_end) = buffer.find("\n\n") {
                                    let event_str = buffer[..event_end].to_string();
                                    buffer = buffer[event_end + 2..].to_string();

                                    // Extract text_delta content from SSE data
                                    for line in event_str.lines() {
                                        if let Some(data) = line.strip_prefix("data: ") {
                                            if let Ok(json) =
                                                serde_json::from_str::<serde_json::Value>(data)
                                            {
                                                // Check for text_delta type with content field
                                                if json.get("type").and_then(|v| v.as_str())
                                                    == Some("text_delta")
                                                {
                                                    if let Some(content) =
                                                        json.get("content").and_then(|v| v.as_str())
                                                    {
                                                        full_text.push_str(content);
                                                        eprint!("{}", content); // Stream output
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            eprintln!(); // End streaming output

                            // Parse the plan from the response
                            match crate::plan_decompose::parse_plan_response(&full_text) {
                                Ok(plan) => {
                                    eprintln!();
                                    eprint!("{}", crate::plan_decompose::format_plan(&plan));
                                }
                                Err(e) => {
                                    eprintln!(
                                        "{}",
                                        format!("  ✗ Could not parse plan: {e}").yellow()
                                    );
                                    eprintln!("  The response may still be useful — see above.");
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
                    use super::plan_decompose::{
                        PlanModeState, analyze_project, decomposition_prompt, format_plan,
                        parse_plan_response,
                    };

                    let Some(tok) = token else {
                        eprintln!("  {} Not logged in. Run /login first.", "✗".red());
                        return Ok(());
                    };

                    // Analyze project context
                    let project_root =
                        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
                    eprintln!("  {} Analyzing project...", "⋯".dim());
                    let context = analyze_project(&project_root);

                    // Generate initial decomposition prompt
                    let prompt = decomposition_prompt(sub_arg, &context);

                    eprintln!("  {} Decomposing goal...", "⋯".dim());

                    // Call LLM for initial plan
                    let payload = serde_json::json!({
                        "messages": [{"role": "user", "content": prompt}],
                        "session_id": state.session_id.clone(),
                    });

                    let resp = client
                        .post(format!("{base}/chat/turn"))
                        .bearer_auth(tok)
                        .json(&payload)
                        .send()
                        .await;

                    match resp {
                        Ok(r) if r.status().is_success() => {
                            let mut full_text = String::new();
                            let mut stream = r.bytes_stream();
                            use futures_util::StreamExt;

                            while let Some(chunk) = stream.next().await {
                                if let Ok(bytes) = chunk {
                                    let event_str = String::from_utf8_lossy(&bytes);
                                    for line in event_str.lines() {
                                        if let Some(data) = line.strip_prefix("data: ") {
                                            if let Ok(json) =
                                                serde_json::from_str::<serde_json::Value>(data)
                                            {
                                                if json.get("type").and_then(|v| v.as_str())
                                                    == Some("text_delta")
                                                {
                                                    if let Some(content) =
                                                        json.get("content").and_then(|v| v.as_str())
                                                    {
                                                        full_text.push_str(content);
                                                    }
                                                }
                                            }
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

                            eprintln!();
                            eprintln!(
                                "{}  Entered plan mode for: {}",
                                "📋".yellow(),
                                sub_arg.cyan()
                            );
                            eprintln!();

                            // Display the plan
                            if let Ok(ref p) = plan_result {
                                let formatted = format_plan(p);
                                eprintln!("{formatted}");
                            }

                            eprintln!();
                            eprintln!(
                                "  {} Commands: 'exit' to leave, 'execute' or 'go' to run the plan",
                                "💡".cyan()
                            );
                            eprintln!("  {} Or ask questions to modify the plan", "💬".cyan());
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
                    use super::plan_decompose::{PlanModeState, format_plan};
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
                            eprintln!("  {} No saved plan state to resume", "⚠".yellow());
                        }
                    }
                }
                "exit" => {
                    if state.plan_mode.is_some() {
                        state.plan_mode = None;
                        super::plan_decompose::PlanModeState::clear_saved_state();
                        eprintln!("  {} Exited plan mode", "✓".green());
                    } else {
                        eprintln!("  ⚠️ Not in plan mode");
                    }
                }
                "cloud" => {
                    // List or load plans from cloud
                    if let Some(ref svc) = state.task_service {
                        use mo_agent_services::TaskService;
                        let user_id = state.ingestion_user_id.as_deref().unwrap_or("local");
                        
                        match svc.list_tasks(user_id, None).await {
                            Ok(tasks) => {
                                let with_plans: Vec<_> = tasks.iter()
                                    .filter(|t| t.plan.is_some())
                                    .collect();
                                
                                if with_plans.is_empty() {
                                    eprintln!("  {} No cloud plans found. Use /plan auto <goal> to create one.", "⚠".yellow());
                                } else {
                                    eprintln!("\n{}  Cloud Plans", "☁️".cyan());
                                    eprintln!("{}", "─".repeat(50));
                                    for t in &with_plans {
                                        let icon = match t.status {
                                            mo_agent_services::TaskStatus::Completed => "✓",
                                            mo_agent_services::TaskStatus::Failed => "✗",
                                            mo_agent_services::TaskStatus::InProgress => "▶",
                                            mo_agent_services::TaskStatus::Paused => "⏸",
                                            _ => "○",
                                        };
                                        let short_id = &t.task_id[..8.min(t.task_id.len())];
                                        let subtask_count = t.plan.as_ref().map(|p| p.subtasks.len()).unwrap_or(0);
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
                                    eprintln!("  {} Use /plan load <id> to restore a cloud plan", "💡".cyan());
                                }
                            }
                            Err(e) => eprintln!("{}", format!("  ✗ {e}").red()),
                        }
                    } else {
                        eprintln!("  {} Cloud not available. Use /login first.", "⚠".yellow());
                    }
                }
                "load" if !sub_arg.is_empty() => {
                    // Load a specific plan from cloud by task_id (or prefix)
                    if let Some(ref svc) = state.task_service {
                        use mo_agent_services::TaskService;
                        use super::plan_decompose::{PlanModeState, format_plan, analyze_project};
                        
                        let user_id = state.ingestion_user_id.as_deref().unwrap_or("local");
                        let query = sub_arg.trim();
                        
                        // Find task by ID prefix or title substring
                        match svc.list_tasks(user_id, None).await {
                            Ok(tasks) => {
                                let found = tasks.iter().find(|t| {
                                    t.task_id.starts_with(query) || 
                                    t.title.to_lowercase().contains(&query.to_lowercase())
                                });
                                
                                match found {
                                    Some(task) if task.plan.is_some() => {
                                        let plan = task.plan.as_ref().unwrap().clone();
                                        let project_root = std::env::current_dir()
                                            .unwrap_or_else(|_| std::path::PathBuf::from("."));
                                        let context = analyze_project(&project_root);
                                        let mut ps = PlanModeState::new(task.title.clone(), context);
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
                                        eprintln!("{}", format_plan(&plan));
                                        eprintln!();
                                        eprintln!(
                                            "  {} Commands: 'execute' to run, 'exit' to leave plan mode",
                                            "💡".cyan()
                                        );
                                    }
                                    Some(_) => {
                                        eprintln!("  {} Task '{}' has no plan", "⚠".yellow(), query);
                                    }
                                    None => {
                                        eprintln!("  {} No task found matching '{}'", "⚠".yellow(), query);
                                        eprintln!("  {} Use /plan cloud to list available plans", "💡".cyan());
                                    }
                                }
                            }
                            Err(e) => eprintln!("{}", format!("  ✗ {e}").red()),
                        }
                    } else {
                        eprintln!("  {} Cloud not available. Use /login first.", "⚠".yellow());
                    }
                }
                "list" => {
                    let plans = crate::plan_decompose::list_saved_plans();
                    let templates = crate::plan_decompose::builtin_templates();
                    eprintln!("{}", crate::plan_decompose::format_plan_list(&plans));
                    eprintln!("  {} Built-in templates:", "📋".cyan());
                    for t in &templates {
                        eprintln!("    • {} — {} [{}]", t.name, t.description,
                            t.languages.join(", "));
                    }
                    eprintln!("  Use /plan template <name> <goal> to instantiate");
                    // Also hint about cloud if available
                    if state.task_service.is_some() {
                        eprintln!("  {} Use /plan cloud to list cloud-synced plans", "☁️".cyan());
                    }
                }
                "template" if !sub_arg.is_empty() => {
                    let parts: Vec<&str> = sub_arg.splitn(2, ' ').collect();
                    let name = parts[0];
                    let goal = if parts.len() > 1 { parts[1] } else { "implement this feature" };
                    match crate::plan_decompose::instantiate_template(name, goal) {
                        Some(plan) => {
                            eprintln!("  {} Template '{}' instantiated with {} subtasks",
                                "✓".green(), name, plan.subtasks.len());
                            eprintln!("{}", crate::plan_decompose::format_plan(&plan));
                            // Enter plan mode with this template
                            let project_root = std::env::current_dir()
                                .unwrap_or_else(|_| std::path::PathBuf::from("."));
                            let context = crate::plan_decompose::analyze_project(&project_root);
                            let mut ps = crate::plan_decompose::PlanModeState::new(
                                goal.to_string(), context);
                            ps.set_plan(plan);
                            state.plan_mode = Some(ps);
                            eprintln!("  {} Entered plan mode. Type 'execute' to run, 'exit' to leave.",
                                "💡".cyan());
                        }
                        None => {
                            let names: Vec<_> = crate::plan_decompose::builtin_templates()
                                .iter().map(|t| t.name.clone()).collect();
                            eprintln!("  {} Template '{}' not found. Available: {}",
                                "⚠".yellow(), name, names.join(", "));
                        }
                    }
                }
                "rate" if !sub_arg.is_empty() => {
                    // Record user feedback for the current/last executed plan
                    let rating_str = sub_arg.trim();
                    
                    if rating_str == "skip" {
                        eprintln!("  {} Feedback skipped", "⚠".yellow());
                        return Ok(());
                    }
                    
                    let rating: u8 = match rating_str.parse() {
                        Ok(r) if (1..=5).contains(&r) => r,
                        _ => {
                            eprintln!("  {} Rating must be 1-5 (or 'skip')", "⚠".yellow());
                            return Ok(());
                        }
                    };
                    
                    // Find the task to rate - use the most recent task for current goal
                    if let Some(ref svc) = state.task_service {
                        use mo_agent_services::{TaskService, TaskOutcome};
                        let user_id = state.ingestion_user_id.as_deref().unwrap_or("local");
                        
                        // Find task by current plan goal or executing plan goal
                        let goal = state.plan_mode.as_ref().map(|ps| ps.goal.clone())
                            .or_else(|| state.executing_plan_goal.clone());
                        
                        if let Some(goal_text) = goal {
                            match svc.list_tasks(user_id, None).await {
                                Ok(tasks) => {
                                    // Find the most recent task matching the goal
                                    let found = tasks.iter()
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
                                        
                                        match svc.record_feedback(&task.task_id, rating, outcome, None).await {
                                            Ok(_) => {
                                                let stars = "★".repeat(rating as usize) + &"☆".repeat(5 - rating as usize);
                                                eprintln!(
                                                    "  {} Feedback recorded: {} ({})",
                                                    "✓".green(),
                                                    stars.yellow(),
                                                    outcome.as_str()
                                                );
                                                
                                                // Auto-extract template if rating >= 4
                                                if rating >= 4 {
                                                    let goal_pattern = extract_goal_pattern(&goal_text);
                                                    match svc.extract_template(&task.task_id, &goal_pattern).await {
                                                        Ok(Some(template_id)) => {
                                                            eprintln!(
                                                                "  {} Template extracted: {} → {}",
                                                                "📝".cyan(),
                                                                goal_pattern.dim(),
                                                                &template_id[..8]
                                                            );
                                                        }
                                                        Ok(None) => {} // Not eligible
                                                        Err(e) => {
                                                            eprintln!("  {} Template extraction failed: {}", "⚠".yellow(), e);
                                                        }
                                                    }
                                                }
                                            }
                                            Err(e) => {
                                                eprintln!("  {} Could not record feedback: {}", "⚠".yellow(), e);
                                            }
                                        }
                                    } else {
                                        eprintln!("  {} No task found for current goal", "⚠".yellow());
                                    }
                                }
                                Err(e) => eprintln!("{}", format!("  ✗ {e}").red()),
                            }
                        } else {
                            eprintln!("  {} No active plan to rate", "⚠".yellow());
                        }
                    } else {
                        // Store rating locally
                        eprintln!(
                            "  {} Rating {} recorded locally (cloud sync not available)",
                            "✓".green(),
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
                            use mo_agent_services::TaskService;
                            let user_id = state.ingestion_user_id.as_deref().unwrap_or("local");
                            let project_type = state.plan_mode.as_ref()
                                .and_then(|ps| ps.context.languages.first())
                                .map(|s| s.as_str());
                            
                            match svc.recommend_templates(user_id, &goal, project_type, 5).await {
                                Ok(recommendations) => {
                                    if recommendations.is_empty() {
                                        eprintln!("  {} No templates found for: {}", "📋".dim(), goal.dim());
                                        eprintln!("  {} Complete more plans and rate them to build templates!", "💡".cyan());
                                    } else {
                                        eprintln!("  {} Recommended templates for: {}", "📋".cyan(), goal.cyan());
                                        eprintln!();
                                        for (i, rec) in recommendations.iter().enumerate() {
                                            let stars = "★".repeat((rec.template.success_rate * 5.0) as usize);
                                            eprintln!(
                                                "  [{}] {} {} ({}x used)",
                                                (i + 1).to_string().cyan(),
                                                rec.template.goal_pattern,
                                                stars.yellow(),
                                                rec.template.use_count
                                            );
                                            let reason_ref = &rec.reason;
                                            eprintln!("      {} {}", "→".dim(), reason_ref.as_str().dim());
                                            eprintln!("      {} {} subtasks", "📝".dim(), rec.template.template.subtasks.len());
                                        }
                                        eprintln!();
                                        eprintln!("  {} Use '/plan use <n>' to apply a template", "💡".cyan());
                                    }
                                }
                                Err(e) => eprintln!("{}", format!("  ✗ {e}").red()),
                            }
                        } else {
                            eprintln!("  {} Cloud service not available", "⚠".yellow());
                        }
                    } else {
                        eprintln!("  {} Usage: /plan recommend <goal>", "⚠".yellow());
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
                            use mo_agent_services::TaskService;
                            let user_id = state.ingestion_user_id.as_deref().unwrap_or("local");
                            
                            match svc.get_learning_stats(user_id, &pattern).await {
                                Ok(stats) => {
                                    eprintln!("  {} Learning Stats: {}", "📊".cyan(), pattern.cyan());
                                    eprintln!();
                                    eprintln!("  Total tasks:     {}", stats.total_tasks);
                                    eprintln!("  Completed:       {} ({:.0}%)", 
                                        stats.completed_tasks,
                                        if stats.total_tasks > 0 { stats.completed_tasks as f32 / stats.total_tasks as f32 * 100.0 } else { 0.0 }
                                    );
                                    if let Some(avg) = stats.avg_rating {
                                        let stars = "★".repeat(avg.round() as usize);
                                        eprintln!("  Avg rating:      {} ({:.1})", stars.yellow(), avg);
                                    }
                                    eprintln!("  Avg replans:     {:.1}", stats.avg_replan_count);
                                    eprintln!("  Success rate:    {:.0}% (inferred)", stats.inferred_success_rate * 100.0);
                                }
                                Err(e) => eprintln!("{}", format!("  ✗ {e}").red()),
                            }
                        } else {
                            eprintln!("  {} Cloud service not available", "⚠".yellow());
                        }
                    } else {
                        eprintln!("  {} Usage: /plan stats <pattern>", "⚠".yellow());
                    }
                }
                "history" => {
                    if let Some(ref ps) = state.plan_mode {
                        eprintln!("  ─── Version History ───");
                        eprintln!("{}", ps.version_history.format_log());
                    } else {
                        eprintln!("  {} Not in plan mode. Use /plan enter <goal> first.",
                            "⚠".yellow());
                    }
                }
                "timeline" => {
                    if let Some(ref ps) = state.plan_mode {
                        eprintln!("  ─── Execution Timeline ───");
                        if ps.timeline.events.is_empty() {
                            eprintln!("  {} No events recorded yet", "(empty)".dim());
                            eprintln!("  {} Events are recorded during plan execution", "💡".cyan());
                        } else {
                            eprintln!("{}", ps.timeline.format_display());
                            // Show summary
                            eprintln!("  ─────────────────────────");
                            eprintln!("  Completed: {} | Failed: {} | Replans: {}",
                                ps.timeline.completed_subtask_count().to_string().green(),
                                ps.timeline.failed_subtask_count().to_string().red(),
                                ps.timeline.replan_count()
                            );
                            if let Some(duration) = ps.timeline.total_duration_sec() {
                                eprintln!("  Total duration: {} sec", duration);
                            }
                        }
                    } else {
                        eprintln!("  {} Not in plan mode.", "⚠".yellow());
                    }
                }
                "diff" if !sub_arg.is_empty() => {
                    if let Some(ref ps) = state.plan_mode {
                        let parts: Vec<&str> = sub_arg.split_whitespace().collect();
                        if parts.len() == 2 {
                            if let (Ok(from), Ok(to)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>()) {
                                match ps.version_history.diff_versions(from, to) {
                                    Ok(diff) => eprintln!("{}", diff.format()),
                                    Err(e) => eprintln!("  {} {}", "⚠".yellow(), e),
                                }
                            } else {
                                eprintln!("  Usage: /plan diff <from_version> <to_version>");
                            }
                        } else {
                            eprintln!("  Usage: /plan diff <from_version> <to_version>");
                        }
                    } else {
                        eprintln!("  {} Not in plan mode.", "⚠".yellow());
                    }
                }
                "rollback" if !sub_arg.is_empty() => {
                    if let Some(ref mut ps) = state.plan_mode {
                        if let Ok(version) = sub_arg.trim().parse::<u32>() {
                            match ps.rollback_to_version(version) {
                                Ok(msg) => {
                                    eprintln!("  {} {}", "✓".green(), msg);
                                    eprintln!("{}", crate::plan_decompose::format_plan(&ps.plan));
                                }
                                Err(e) => eprintln!("  {} {}", "⚠".yellow(), e),
                            }
                        } else {
                            eprintln!("  Usage: /plan rollback <version_number>");
                        }
                    } else {
                        eprintln!("  {} Not in plan mode.", "⚠".yellow());
                    }
                }
                "replan" => {
                    // Regenerate plan based on current state and issues
                    use super::plan_decompose::{
                        detect_replan_needed, generate_replan_prompt, format_plan,
                        parse_plan_response, ReplanReason,
                    };
                    
                    let Some(ref mut ps) = state.plan_mode else {
                        // Check if there's an executing plan to replan
                        if let Some(ref exec_plan) = state.executing_plan {
                            eprintln!("  {} Replan from executing plan not yet supported", "⚠".yellow());
                            eprintln!("  {} Pause execution first with Ctrl+C, then enter plan mode", "💡".cyan());
                        } else {
                            eprintln!("  {} Not in plan mode. Use /plan first.", "⚠".yellow());
                        }
                        return Ok(());
                    };
                    
                    let Some(tok) = token else {
                        eprintln!("  {} Not logged in. Run /login first.", "✗".red());
                        return Ok(());
                    };
                    
                    // Determine reason for replan
                    let reason = if !sub_arg.is_empty() {
                        // User provided reason
                        ReplanReason::UserRequest
                    } else {
                        // Auto-detect reason
                        let failed: Vec<(&str, &str)> = vec![];  // TODO: track failed subtasks
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
                    
                    let resp = client
                        .post(format!("{base}/chat/turn"))
                        .bearer_auth(tok)
                        .json(&payload)
                        .send()
                        .await;
                    
                    match resp {
                        Ok(r) if r.status().is_success() => {
                            let mut full_text = String::new();
                            let mut stream = r.bytes_stream();
                            use futures_util::StreamExt;
                            
                            while let Some(chunk) = stream.next().await {
                                if let Ok(bytes) = chunk {
                                    let event_str = String::from_utf8_lossy(&bytes);
                                    for line in event_str.lines() {
                                        if let Some(data) = line.strip_prefix("data: ") {
                                            if let Ok(json) = serde_json::from_str::<serde_json::Value>(data) {
                                                if let Some(content) = json.get("content").and_then(|v| v.as_str()) {
                                                    full_text.push_str(content);
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            
                            match parse_plan_response(&full_text) {
                                Ok(new_plan) => {
                                    // Keep completed subtasks, update pending ones
                                    let old_version = ps.version_history.current_version;
                                    ps.update_plan(new_plan, &format!("Replan: {}", reason.format()));
                                    let _ = ps.save_to_file(&super::plan_decompose::PlanModeState::state_path());
                                    
                                    eprintln!();
                                    eprintln!("  {} Plan updated (v{} → v{})", 
                                        "✓".green(), old_version, ps.version_history.current_version);
                                    eprintln!();
                                    eprintln!("{}", format_plan(&ps.plan));
                                    eprintln!();
                                    eprintln!("  {} Use '/plan diff {} {}' to see changes", 
                                        "💡".cyan(), old_version, ps.version_history.current_version);
                                }
                                Err(e) => {
                                    eprintln!("  {} Failed to parse replan: {}", "✗".red(), e);
                                }
                            }
                        }
                        Ok(r) => {
                            eprintln!("  {} LLM call failed ({})", "✗".red(), r.status());
                        }
                        Err(e) => {
                            eprintln!("  {} Request failed: {}", "✗".red(), e);
                        }
                    }
                    
                    // Increment replan count in cloud if available
                    if let Some(ref svc) = state.task_service {
                        use mo_agent_services::TaskService;
                        let user_id = state.ingestion_user_id.as_deref().unwrap_or("local");
                        if let Ok(tasks) = svc.list_tasks(user_id, None).await {
                            if let Some(task) = tasks.iter().find(|t| t.title == ps.goal) {
                                let _ = svc.increment_replan_count(&task.task_id).await;
                            }
                        }
                    }
                }
                "parallel" => {
                    if let Some(ref ps) = state.plan_mode {
                        let analysis = crate::plan_decompose::analyze_parallelism(&ps.plan);
                        eprintln!("{}", crate::plan_decompose::format_parallelism(&analysis));
                    } else {
                        eprintln!("  {} Not in plan mode.", "⚠".yellow());
                    }
                }
                "auto" if !sub_arg.is_empty() => {
                    // Auto mode: decompose + preview + execute in one shot
                    use super::plan_decompose::{
                        analyze_project, decomposition_prompt,
                        format_execution_preview, format_plan, parse_plan_response,
                        PlanExecutionConfig,
                    };

                    let Some(tok) = token else {
                        eprintln!("  {} Not logged in. Run /login first.", "✗".red());
                        return Ok(());
                    };

                    let project_root =
                        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
                    eprintln!("  {} Analyzing project...", "⋯".dim());
                    let context = analyze_project(&project_root);
                    let prompt = decomposition_prompt(sub_arg, &context);
                    eprintln!("  {} Decomposing and auto-executing: {}", "🚀".cyan(), sub_arg);

                    let payload = serde_json::json!({
                        "messages": [{"role": "user", "content": prompt}],
                        "session_id": state.session_id.clone(),
                    });

                    match client
                        .post(format!("{base}/chat/turn"))
                        .bearer_auth(tok)
                        .json(&payload)
                        .send()
                        .await
                    {
                        Ok(r) if r.status().is_success() => {
                            let mut full_text = String::new();
                            let mut stream = r.bytes_stream();
                            use futures_util::StreamExt;

                            while let Some(chunk) = stream.next().await {
                                if let Ok(bytes) = chunk {
                                    let event_str = String::from_utf8_lossy(&bytes);
                                    for line in event_str.lines() {
                                        if let Some(data) = line.strip_prefix("data: ") {
                                            if let Ok(json) =
                                                serde_json::from_str::<serde_json::Value>(data)
                                            {
                                                if json.get("type").and_then(|v| v.as_str())
                                                    == Some("text_delta")
                                                {
                                                    if let Some(content) =
                                                        json.get("content").and_then(|v| v.as_str())
                                                    {
                                                        full_text.push_str(content);
                                                    }
                                                }
                                            }
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

                                    state.plan_execution_config =
                                        Some(PlanExecutionConfig { auto_execute: true, ..Default::default() });
                                    state.executing_plan_goal = Some(sub_arg.to_string());
                                    state.plan_execution_rounds = 0;
                                    state.executing_plan = Some(plan);
                                }
                                Err(e) => {
                                    eprintln!(
                                        "{}",
                                        format!("  ✗ Could not parse plan: {e}").yellow()
                                    );
                                    eprintln!(
                                        "  Try '/plan enter {}' for interactive mode.",
                                        sub_arg
                                    );
                                }
                            }
                        }
                        Ok(r) => {
                            eprintln!(
                                "{}",
                                format!("  ✗ LLM call failed ({})", r.status()).red()
                            );
                        }
                        Err(e) => {
                            eprintln!("{}", format!("  ✗ Request failed: {e}").red());
                        }
                    }
                }
                _ => {
                    eprintln!(
                        "  Usage: /plan [show | set <text> | clear | decompose <goal> | enter <goal> | auto <goal> | resume | exit | list | cloud | load <id> | rate <1-5> | template <name> <goal> | history | diff <v1> <v2> | rollback <v> | parallel]"
                    );
                }
            }
        }

        _ => unreachable!("unexpected memory-domain command: {cmd}"),
    }

    Ok(())
}

/// Handle user input in plan mode - sends to LLM for plan editing
pub async fn handle_plan_mode_input(
    input: String,
    token: Option<&str>,
    state: &mut ReplState,
    client: &reqwest::Client,
    base: &str,
) -> Result<(), String> {
    use super::plan_decompose::{
        PlanModeState, format_plan, parse_plan_response, decomposition_prompt,
        format_project_context, parse_plan_entry_choice, PlanEntryChoice,
        format_clarification_question, parse_clarification_response, 
        detect_clarification_questions, PendingClarifications, ClarificationAnswer,
    };

    let plan_state = match state.plan_mode.as_mut() {
        Some(ps) => ps,
        None => {
            eprintln!("  ⚠️ Not in plan mode");
            return Ok(());
        }
    };
    
    // Handle pending clarification questions first
    if let Some(ref mut pending) = plan_state.pending_clarifications {
        if let Some(question) = pending.next_question().cloned() {
            // Parse user's answer
            let answer = parse_clarification_response(&input, &question);
            match answer {
                ClarificationAnswer::Selected(idx) => {
                    let selected = &question.options[idx];
                    pending.record_answer(selected.clone());
                    eprintln!("  {} Selected: {}", "✓".green(), selected);
                }
                ClarificationAnswer::Freeform(text) => {
                    pending.record_answer(text.clone());
                    eprintln!("  {} Answer: {}", "✓".green(), text);
                }
                ClarificationAnswer::Invalid(msg) => {
                    eprintln!("  {} {}", "✗".red(), msg);
                    eprintln!();
                    eprint!("{}", format_clarification_question(&question));
                    return Ok(());
                }
            }
            
            // Check if more questions remain
            if let Some(next_q) = pending.next_question() {
                eprintln!();
                eprint!("{}", format_clarification_question(next_q));
                let _ = plan_state.save_to_file(&PlanModeState::state_path());
                return Ok(());
            }
            
            // All questions answered - regenerate plan with clarifications
            eprintln!();
            eprintln!("  {} All clarifications answered. Regenerating plan...", "🔄".cyan());
            
            let answers_context = pending.format_for_prompt();
            let goal_with_context = format!(
                "{}\n\n## Clarifications from user:\n{}",
                plan_state.goal, answers_context
            );
            
            // Clear pending and regenerate
            plan_state.pending_clarifications = None;
            
            let Some(tok) = token else {
                eprintln!("  {} Not logged in. Run /login first.", "✗".red());
                return Ok(());
            };
            
            let prompt = decomposition_prompt(&goal_with_context, &plan_state.context);
            let payload = serde_json::json!({
                "messages": [{"role": "user", "content": prompt}],
                "session_id": state.session_id.clone(),
            });
            
            let resp = client
                .post(format!("{base}/chat/turn"))
                .bearer_auth(tok)
                .json(&payload)
                .send()
                .await;
            
            match resp {
                Ok(r) if r.status().is_success() => {
                    let mut full_text = String::new();
                    let mut stream = r.bytes_stream();
                    use futures_util::StreamExt;
                    
                    eprintln!("  {} Thinking...", "🧠".cyan());
                    
                    while let Some(chunk) = stream.next().await {
                        if let Ok(bytes) = chunk {
                            let event_str = String::from_utf8_lossy(&bytes);
                            for line in event_str.lines() {
                                if let Some(data) = line.strip_prefix("data: ") {
                                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(data) {
                                        if let Some(content) = json.get("content").and_then(|v| v.as_str()) {
                                            full_text.push_str(content);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    
                    // Parse and set plan (no clarification check in regeneration)
                    match parse_plan_response(&full_text) {
                        Ok(plan) => {
                            plan_state.set_plan(plan);
                            let _ = plan_state.save_to_file(&PlanModeState::state_path());
                            
                            eprintln!();
                            eprintln!("{}", format_plan(&plan_state.plan));
                            eprintln!();
                            eprintln!("  {} Commands:", "💡".cyan());
                            eprintln!("    'go' or 'execute' → Run the plan");
                            eprintln!("    'step' → Run step-by-step with confirmation");
                            eprintln!("    Or describe changes to modify the plan");
                        }
                        Err(e) => {
                            eprintln!("  {} Failed to parse plan: {}", "✗".red(), e);
                        }
                    }
                }
                Ok(r) => {
                    eprintln!("  {} LLM call failed ({})", "✗".red(), r.status());
                }
                Err(e) => {
                    eprintln!("  {} Request failed: {}", "✗".red(), e);
                }
            }
            
            return Ok(());
        }
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
                eprintln!("  {} Plan cleared. Describe what you want to do:", "🔄".yellow());
                return Ok(());
            }
            PlanEntryChoice::Resume => {
                // Resume paused execution
                if let Some(ref paused) = state.executing_plan {
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
                    eprintln!("  {} Not logged in. Run /login first.", "✗".red());
                    return Ok(());
                };
                
                plan_state.goal = goal.clone();
                
                // Show project context
                eprintln!();
                eprintln!("{}", format_project_context(&plan_state.context));
                eprintln!();
                
                // Streaming plan generation with real-time output
                eprintln!("  {} Thinking...", "🧠".cyan());
                eprintln!();
                
                let prompt = decomposition_prompt(&goal, &plan_state.context);
                let payload = serde_json::json!({
                    "messages": [{"role": "user", "content": prompt}],
                    "session_id": state.session_id.clone(),
                });
                
                let resp = client
                    .post(format!("{base}/chat/turn"))
                    .bearer_auth(tok)
                    .json(&payload)
                    .send()
                    .await;
                
                match resp {
                    Ok(r) if r.status().is_success() => {
                        let mut full_text = String::new();
                        let mut stream = r.bytes_stream();
                        use futures_util::StreamExt;
                        
                        // Track streaming state
                        let mut in_thinking = false;
                        let mut in_plan_json = false;
                        let mut chars_since_nl = 0;
                        
                        while let Some(chunk) = stream.next().await {
                            if let Ok(bytes) = chunk {
                                let event_str = String::from_utf8_lossy(&bytes);
                                for line in event_str.lines() {
                                    if let Some(data) = line.strip_prefix("data: ") {
                                        if let Ok(json) = serde_json::from_str::<serde_json::Value>(data) {
                                            if json.get("type").and_then(|v| v.as_str()) == Some("text_delta") {
                                                if let Some(content) = json.get("content").and_then(|v| v.as_str()) {
                                                    full_text.push_str(content);
                                                    
                                                    // Stream thinking process to user (before JSON)
                                                    for ch in content.chars() {
                                                        // Detect start of JSON plan
                                                        if ch == '{' && !in_thinking && !in_plan_json {
                                                            in_plan_json = true;
                                                            eprintln!();
                                                            eprintln!();
                                                            eprint!("  {} Parsing plan", "⚙".dim());
                                                            continue;
                                                        }
                                                        
                                                        if in_plan_json {
                                                            // Show progress dots during JSON parsing
                                                            if ch == ',' || ch == '}' {
                                                                eprint!(".");
                                                            }
                                                            continue;
                                                        }
                                                        
                                                        // Stream thinking text
                                                        if !in_thinking && chars_since_nl == 0 {
                                                            in_thinking = true;
                                                            eprint!("  ");
                                                        }
                                                        
                                                        eprint!("{}", ch);
                                                        
                                                        if ch == '\n' {
                                                            chars_since_nl = 0;
                                                            in_thinking = false;
                                                        } else {
                                                            chars_since_nl += 1;
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        eprintln!();
                        
                        // Check for clarification questions first
                        if let Some(questions) = detect_clarification_questions(&full_text) {
                            // LLM is asking for clarification instead of producing plan
                            eprintln!();
                            eprintln!("  {} Need clarification before generating plan:", "❓".yellow());
                            eprintln!();
                            
                            let pending = PendingClarifications {
                                questions: questions.clone(),
                                answers: Vec::new(),
                            };
                            plan_state.pending_clarifications = Some(pending);
                            
                            // Show first question
                            eprint!("{}", format_clarification_question(&questions[0]));
                            let _ = plan_state.save_to_file(&PlanModeState::state_path());
                        } else {
                            // Parse and set plan
                            match parse_plan_response(&full_text) {
                                Ok(plan) => {
                                    plan_state.set_plan(plan);
                                    let _ = plan_state.save_to_file(&PlanModeState::state_path());
                                    
                                    eprintln!();
                                    eprintln!("{}", format_plan(&plan_state.plan));
                                    eprintln!();
                                    eprintln!("  {} Commands:", "💡".cyan());
                                    eprintln!("    'go' or 'execute' → Run the plan");
                                    eprintln!("    'step' → Run step-by-step with confirmation");
                                    eprintln!("    Or describe changes to modify the plan");
                                }
                                Err(e) => {
                                    eprintln!("  {} Failed to parse plan: {}", "✗".red(), e);
                                }
                            }
                        }
                    }
                    Ok(r) => {
                        eprintln!("  {} LLM call failed ({})", "✗".red(), r.status());
                    }
                    Err(e) => {
                        eprintln!("  {} Request failed: {}", "✗".red(), e);
                    }
                }
                
                return Ok(());
            }
        }
    }

    // Check for "done <id>" — mark subtask completed
    if let Some(done_id) = input_lower.strip_prefix("done ").map(|s| s.trim()) {
        if !done_id.is_empty() {
            match plan_state.complete_subtask(done_id) {
                Ok(title) => {
                    let pct = plan_state.plan.progress_pct();
                    let done_count = plan_state.plan.items_done();
                    let total_count = plan_state.plan.subtasks.len();
                    eprintln!("  {} Completed: {} ({}%)", "✓".green(), title, pct);
                    // Save updated state locally
                    let _ = plan_state.save_to_file(&PlanModeState::state_path());
                    
                    // Sync progress to cloud if available
                    if let Some(ref svc) = state.task_service {
                        use mo_agent_services::TaskService;
                        let user_id = state.ingestion_user_id.as_deref().unwrap_or("local");
                        let goal = &plan_state.goal;
                        
                        // Find matching cloud task and update progress
                        if let Ok(tasks) = svc.list_tasks(user_id, None).await {
                            if let Some(task) = tasks.iter().find(|t| &t.title == goal) {
                                // Update plan and progress in cloud
                                let _ = svc.update_plan(&task.task_id, &plan_state.plan).await;
                                let _ = svc.update_progress(
                                    &task.task_id,
                                    pct,
                                    done_count,
                                    total_count as u32,
                                ).await;
                            }
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
                        eprintln!("  {} All tasks complete!", "🎉".green());
                        // Complete the cloud task
                        if let Some(ref svc) = state.task_service {
                            use mo_agent_services::TaskService;
                            let user_id = state.ingestion_user_id.as_deref().unwrap_or("local");
                            let goal = &plan_state.goal;
                            if let Ok(tasks) = svc.list_tasks(user_id, None).await {
                                if let Some(task) = tasks.iter().find(|t| &t.title == goal) {
                                    let _ = svc.complete_task(&task.task_id).await;
                                }
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
                Err(e) => eprintln!("  {} {}", "⚠".yellow(), e),
            }
            return Ok(());
        }
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
        use super::plan_decompose::{format_execution_preview, PlanExecutionConfig};

        let plan = plan_state.plan.clone();
        let goal = plan_state.goal.clone();

        // Show execution preview with parallel analysis
        eprintln!();
        eprint!("{}", format_execution_preview(&plan));
        eprintln!();

        // Persist to task service if available
        if let Some(ref svc) = state.task_service {
            use mo_agent_services::{TaskCreateRequest, TaskService};
            let user_id = state.ingestion_user_id.as_deref().unwrap_or("local");
            let session_id = state.session_id.as_deref().unwrap_or("no-session");

            // Extract project_type from context
            let project_type = plan_state.context.languages.first().map(|s| s.to_lowercase());

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
                    let short = &tid[..8.min(tid.len())];
                    eprintln!("{}  Task created: {} ({})", "✓".green(), goal, short.dim());
                    eprintln!("{}  Track progress: /task status {}", "💡".cyan(), short);
                }
                Err(e) => {
                    eprintln!("{}  Could not persist task: {}", "⚠".yellow(), e);
                }
            }
        }

        eprintln!("{}  Auto-executing plan ({} subtasks)...", "🚀".green(), plan.subtasks.len());
        eprintln!();

        // Store execution config for auto mode (go = automatic)
        state.plan_execution_config = Some(PlanExecutionConfig {
            step_by_step: false,
            auto_execute: true,
        });
        state.executing_plan_goal = Some(goal);
        state.plan_execution_rounds = 0;

        // Store plan for auto-execution and exit plan mode
        state.executing_plan = Some(plan);
        PlanModeState::clear_saved_state();
        state.plan_mode = None;
        return Ok(());
    }

    // Check for step-by-step execute command
    if input.trim().to_lowercase().starts_with("step") || input.trim() == "逐步执行" {
        use super::plan_decompose::{format_execution_preview, PlanExecutionConfig};

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

        state.executing_plan = Some(plan);
        PlanModeState::clear_saved_state();
        state.plan_mode = None;
        return Ok(());
    }

    // Build prompt for LLM
    let prompt = plan_state.plan_mode_prompt(&input);
    plan_state.add_turn(&input, ""); // Will update assistant part after response

    // Show thinking indicator
    eprint!("  ● Thinking...");

    let Some(tok) = token else {
        eprintln!("\r  ✗ Not logged in. Run /login first.");
        return Ok(());
    };

    // Call LLM via SSE (match the format used by /plan decompose)
    let turn_url = format!("{base}/chat/turn");
    let messages = vec![serde_json::json!({
        "role": "user",
        "content": prompt
    })];

    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));

    // Don't pass session_id for plan mode - let server create ephemeral session
    // This avoids "Session not found" errors since plan mode is self-contained
    let payload = serde_json::json!({
        "messages": messages,
        "model": state.model.clone(),
        "edge_profile": {
            "cwd": cwd.to_string_lossy(),
        },
        "edge_tools": [],  // No tools needed for plan editing
    });

    let resp = client
        .post(&turn_url)
        .headers(auth_headers(tok)?)
        .header("Accept", "text/event-stream")
        .json(&payload)
        .send()
        .await;

    match resp {
        Ok(r) if r.status().is_success() => {
            let mut full_text = String::new();
            let mut stream = r.bytes_stream();
            let mut event_count = 0;
            let mut event_types: Vec<String> = Vec::new();
            use futures_util::StreamExt;

            while let Some(chunk) = stream.next().await {
                if let Ok(bytes) = chunk {
                    let event_str = String::from_utf8_lossy(&bytes);
                    for line in event_str.lines() {
                        if let Some(data) = line.strip_prefix("data: ") {
                            event_count += 1;
                            if let Ok(json) = serde_json::from_str::<serde_json::Value>(data) {
                                let event_type = json
                                    .get("type")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("unknown");
                                if !event_types.contains(&event_type.to_string()) {
                                    event_types.push(event_type.to_string());
                                }
                                if event_type == "text_delta" {
                                    if let Some(content) =
                                        json.get("content").and_then(|v| v.as_str())
                                    {
                                        full_text.push_str(content);
                                    }
                                } else if event_type == "error" {
                                    // Show error messages from the server
                                    if let Some(msg) = json
                                        .get("message")
                                        .or_else(|| json.get("error"))
                                        .and_then(|v| v.as_str())
                                    {
                                        eprintln!("\r  {} Server error: {}", "✗".red(), msg);
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Clear thinking indicator
            eprint!("\r                    \r");

            // Debug: show response info
            if full_text.is_empty() {
                if event_count == 0 {
                    eprintln!("  {} No SSE events received from server", "⚠".yellow());
                } else {
                    eprintln!(
                        "  {} {} events (types: {}) but no text",
                        "⚠".yellow(),
                        event_count,
                        event_types.join(", ")
                    );
                }
            }

            // Try to parse plan update from LLM response
            let plan_updated = if !full_text.is_empty() {
                match parse_plan_response(&full_text) {
                    Ok(plan) => {
                        plan_state.set_plan(plan.clone());
                        plan_state.modified = true;
                        // Save updated state for recovery
                        let _ = plan_state.save_to_file(&PlanModeState::state_path());
                        eprintln!("{}  Plan updated!", "✓".green());
                        eprintln!();
                        let formatted = format_plan(&plan);
                        eprintln!("{formatted}");
                        true
                    }
                    Err(_) => false, // No valid plan JSON — treat as conversational response
                }
            } else {
                false
            };

            // Show the LLM text response (skip if we already displayed the plan)
            if !full_text.is_empty() && !plan_updated {
                eprintln!();
                eprintln!("{}", full_text.trim());
            }

            // Update history with assistant response
            if let Some(last) = plan_state.history.last_mut() {
                last.1 = full_text.chars().take(500).collect();
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
        "add", "fix", "implement", "create", "update", "refactor", "remove", "delete",
        "optimize", "improve", "migrate", "integrate", "test", "document", "configure",
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
        if task_verbs.contains(&word) {
            pattern_parts.push(word.to_string());
        }
        // Keep common structural words
        else if ["for", "to", "in", "with", "from", "by", "the", "a", "an"].contains(&word) {
            pattern_parts.push(word.to_string());
        }
        // Keep technology/domain keywords
        else if [
            "api", "endpoint", "database", "file", "module", "function", "class", "test",
            "config", "error", "logging", "auth", "user", "data", "cache", "queue",
        ].contains(&word) {
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
