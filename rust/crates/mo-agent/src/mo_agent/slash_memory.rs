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
            let subcmd = arg.split_whitespace().next().unwrap_or("show");
            let sub_arg = arg.strip_prefix(subcmd).unwrap_or("").trim();
            match subcmd {
                "show" | "" => {
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
                    let project_root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
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
                                            if let Ok(json) = serde_json::from_str::<serde_json::Value>(data) {
                                                // Check for text_delta type with content field
                                                if json.get("type").and_then(|v| v.as_str()) == Some("text_delta") {
                                                    if let Some(content) = json.get("content").and_then(|v| v.as_str()) {
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
                                    eprintln!("{}", format!("  ✗ Could not parse plan: {e}").yellow());
                                    eprintln!("  The response may still be useful — see above.");
                                }
                            }
                        }
                        Ok(resp) => {
                            eprintln!("{}", format!("  ✗ LLM call failed ({})", resp.status()).red());
                            // Fallback: show the prompt for manual execution
                            eprintln!();
                            eprintln!("{}  Generated decomposition prompt:", "📋".yellow());
                            let preview: String = prompt.chars().take(300).collect();
                            eprintln!("{}{}", preview.dim(), if prompt.len() > 300 { "..." } else { "" });
                            eprintln!();
                            eprintln!("{}  Type 'decompose: {}' to try again.", "💡".cyan(), sub_arg);
                        }
                        Err(e) => {
                            eprintln!("{}", format!("  ✗ Request failed: {e}").red());
                        }
                    }
                }
                "enter" if !sub_arg.is_empty() => {
                    // Enter interactive plan mode (Kiro-style)
                    use super::plan_decompose::{PlanModeState, analyze_project, decomposition_prompt, parse_plan_response, format_plan};
                    
                    let Some(tok) = token else {
                        eprintln!("  {} Not logged in. Run /login first.", "✗".red());
                        return Ok(());
                    };
                    
                    // Analyze project context
                    let project_root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
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
                                            if let Ok(json) = serde_json::from_str::<serde_json::Value>(data) {
                                                if json.get("type").and_then(|v| v.as_str()) == Some("text_delta") {
                                                    if let Some(content) = json.get("content").and_then(|v| v.as_str()) {
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
                            
                            eprintln!();
                            eprintln!("{}  Entered plan mode for: {}", "📋".yellow(), sub_arg.cyan());
                            eprintln!();
                            
                            // Display the plan
                            if let Ok(ref p) = plan_result {
                                let formatted = format_plan(p);
                                eprintln!("{formatted}");
                            }
                            
                            eprintln!();
                            eprintln!("  {} Commands: 'exit' to leave, 'execute' or 'go' to run the plan", "💡".cyan());
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
                "exit" => {
                    if state.plan_mode.is_some() {
                        state.plan_mode = None;
                        eprintln!("  {} Exited plan mode", "✓".green());
                    } else {
                        eprintln!("  ⚠️ Not in plan mode");
                    }
                }
                _ => {
                    eprintln!("  Usage: /plan [show | set <text> | clear | decompose <goal> | enter <goal> | exit]");
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
    use super::plan_decompose::{PlanModeState, TaskPlan, format_plan};
    
    let plan_state = match state.plan_mode.as_mut() {
        Some(ps) => ps,
        None => {
            eprintln!("  ⚠️ Not in plan mode");
            return Ok(());
        }
    };

    // Check for exit commands
    let input_lower = input.to_lowercase();
    if input_lower == "exit" || input_lower == "quit" || input_lower == "/plan exit" {
        eprintln!();
        eprintln!("{}  Exiting plan mode", "📋".yellow());
        state.plan_mode = None;
        return Ok(());
    }

    // Check for execute command
    if PlanModeState::is_execute_command(&input) {
        eprintln!();
        eprintln!("{}  Executing plan...", "🚀".green());
        // Store plan to memory before execution (TODO: actually persist this)
        let _memory_content = plan_state.to_memory_content();
        eprintln!("{}  Plan ready for execution", "💾".cyan());
        eprintln!();
        
        // Display the tasks to execute
        let plan = &plan_state.plan;
        if !plan.subtasks.is_empty() {
            eprintln!("{}  Tasks to execute:", "📋".yellow());
            for task in &plan.subtasks {
                let deps_str = if task.depends_on.is_empty() {
                    String::new()
                } else {
                    format!(" (depends on: {})", task.depends_on.join(", "))
                };
                eprintln!("    {} [{}] {}{}", "•".dim(), task.id, task.title, deps_str.dim());
            }
            eprintln!();
        }
        
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
                                let event_type = json.get("type").and_then(|v| v.as_str()).unwrap_or("unknown");
                                if !event_types.contains(&event_type.to_string()) {
                                    event_types.push(event_type.to_string());
                                }
                                if event_type == "text_delta" {
                                    if let Some(content) = json.get("content").and_then(|v| v.as_str()) {
                                        full_text.push_str(content);
                                    }
                                } else if event_type == "error" {
                                    // Show error messages from the server
                                    if let Some(msg) = json.get("message").or_else(|| json.get("error")).and_then(|v| v.as_str()) {
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
                    eprintln!("  {} {} events (types: {}) but no text", "⚠".yellow(), event_count, event_types.join(", "));
                }
            }
            
            // Check if response contains JSON plan update
            if let Some(json_start) = full_text.find("```json") {
                if let Some(json_end) = full_text[json_start..].find("```\n").or_else(|| full_text[json_start..].rfind("```")) {
                    let json_content = &full_text[json_start + 7..json_start + json_end];
                    if let Ok(plan) = serde_json::from_str::<TaskPlan>(json_content.trim()) {
                        plan_state.set_plan(plan.clone());
                        plan_state.modified = true;
                        eprintln!("{}  Plan updated!", "✓".green());
                        eprintln!();
                        // Display the updated plan
                        let formatted = format_plan(&plan);
                        eprintln!("{formatted}");
                    }
                }
            }
            
            // Show the LLM response (don't filter too aggressively)
            if !full_text.is_empty() {
                // Remove markdown code blocks but keep the rest
                let text_clean = full_text
                    .replace("```json", "")
                    .replace("```", "");
                // Only filter lines that look like pure JSON structure
                let text_filtered: String = text_clean
                    .lines()
                    .filter(|l| {
                        let trimmed = l.trim();
                        // Keep lines that have text content, not just JSON delimiters
                        !trimmed.is_empty() && 
                        !(trimmed == "{" || trimmed == "}" || trimmed == "[" || trimmed == "]" ||
                          (trimmed.starts_with('"') && trimmed.ends_with(',')) ||
                          (trimmed.starts_with('"') && trimmed.ends_with('"')))
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                
                if !text_filtered.trim().is_empty() {
                    eprintln!();
                    eprintln!("{}", text_filtered.trim());
                }
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
