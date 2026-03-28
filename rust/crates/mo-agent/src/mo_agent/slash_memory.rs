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
                    
                    // Call LLM to decompose
                    eprintln!("  {} Decomposing goal into subtasks...", "⋯".dim());
                    let payload = serde_json::json!({
                        "model": state.model.as_deref().unwrap_or("ep-glm-5-fc5e97"),
                        "messages": [
                            {"role": "user", "content": prompt}
                        ],
                        "temperature": 0.3,
                        "max_tokens": 2000,
                    });
                    
                    match client
                        .post(format!("{base}/chat"))
                        .headers(auth_headers(tok)?)
                        .json(&payload)
                        .send()
                        .await
                    {
                        Ok(r) if r.status().is_success() => {
                            let body = r.text().await.unwrap_or_default();
                            // Extract content from response
                            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&body) {
                                let content = json
                                    .get("choices")
                                    .and_then(|c| c.get(0))
                                    .and_then(|c| c.get("message"))
                                    .and_then(|m| m.get("content"))
                                    .and_then(|c| c.as_str())
                                    .unwrap_or(&body);
                                
                                match crate::plan_decompose::parse_plan_response(content) {
                                    Ok(plan) => {
                                        // Store the plan in memory using MemoryEntry helper
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
                                        
                                        // Display the plan
                                        eprintln!();
                                        eprint!("{}", crate::plan_decompose::format_plan(&plan));
                                    }
                                    Err(e) => {
                                        eprintln!("{}", format!("  ✗ Failed to parse plan: {e}").red());
                                        eprintln!("  Raw response:\n{}", content);
                                    }
                                }
                            } else {
                                eprintln!("{}", format!("  ✗ Invalid JSON response").red());
                            }
                        }
                        Ok(r) => eprintln!("{}", format!("  ✗ LLM call failed ({})", r.status()).red()),
                        Err(e) => eprintln!("{}", format!("  ✗ Unreachable: {e}").red()),
                    }
                }
                _ => {
                    eprintln!("  Usage: /plan [show | set <text> | clear | decompose <goal>]");
                }
            }
        }

        _ => unreachable!("unexpected memory-domain command: {cmd}"),
    }

    Ok(())
}
