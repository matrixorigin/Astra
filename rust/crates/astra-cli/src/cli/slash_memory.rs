use super::*;

fn parse_memory_forget_args(input: &str) -> Result<(String, String), String> {
    let mut parts = input.splitn(2, "--reason");
    let memory_id = parts.next().unwrap_or("").trim().to_string();
    if memory_id.is_empty() {
        return Err("usage: /memory forget <memory_id> --reason TEXT".to_string());
    }
    let reason = parts
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "Error: /memory forget requires --reason for audit trail".to_string())?;
    Ok((memory_id, reason))
}

pub(super) async fn handle_memory_domain_command(
    cmd: &str,
    arg: &str,
    api: &astra_thin_client::ThinClient,
    state: &mut SessionState,
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
                    let top_k = state.runtime_config.memory.retrieval_top_k;
                    let payload = serde_json::json!({
                        "query": sub_arg,
                        "top_k": top_k,
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
                                            (i + 1).to_string().magenta(),
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
                                        eprintln!(
                                            "  {}. {}",
                                            (i + 1).to_string().magenta(),
                                            display
                                        );
                                    }
                                    eprintln!(
                                        "  {}",
                                        "────────────────────────────────────────────────".dim()
                                    );
                                    eprintln!("  {} memories", arr.len().to_string().magenta());
                                }
                            } else {
                                print_json_or_raw(&body);
                            }
                        }
                        Ok(r) => eprintln!("{}", format!("  ✗ Failed ({})", r.status()).red()),
                        Err(e) => eprintln!("{}", format!("  ✗ Unreachable: {e}").red()),
                    }
                }
                "dismiss" if !sub_arg.is_empty() => {
                    // Search for matching memories, then send "irrelevant" feedback.
                    let payload = serde_json::json!({
                        "query": sub_arg,
                        "top_k": 3,
                    });
                    match api.post_memory_search_json(tok, &payload).await {
                        Ok(r) if r.status().is_success() => {
                            let body = r.text().await.unwrap_or_default();
                            if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(&body) {
                                if arr.is_empty() {
                                    eprintln!("  {}", "No matching memories to dismiss.".dim());
                                } else {
                                    let mut dismissed = 0u32;
                                    for m in &arr {
                                        let Some(mid) = m.get("memory_id").and_then(|v| v.as_str())
                                        else {
                                            continue;
                                        };
                                        let content = m
                                            .get("content")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("?");
                                        let preview: String = content.chars().take(60).collect();
                                        // Send "irrelevant" feedback to lower retrieval score
                                        let mem = astra_core::MemoriaSettings::from_env();
                                        let fb_url =
                                            format!("{}/v1/memories/{mid}/feedback", mem.base_url);
                                        if let (Ok(client), Some(token)) = (
                                            reqwest::Client::builder()
                                                .timeout(std::time::Duration::from_secs(3))
                                                .no_proxy()
                                                .build(),
                                            mem.bearer_token(),
                                        ) {
                                            let _ = client
                                                .post(&fb_url)
                                                .header("Authorization", token)
                                                .json(&serde_json::json!({"signal": "irrelevant", "context": "user /memory dismiss"}))
                                                .send()
                                                .await;
                                        }
                                        eprintln!("  {} dismissed: {preview}", theme::icon_err());
                                        dismissed += 1;
                                    }
                                    eprintln!(
                                        "  {} memories dismissed (retrieval score lowered)",
                                        dismissed.to_string().magenta()
                                    );
                                }
                            }
                        }
                        Ok(r) => {
                            eprintln!("{}", format!("  ✗ Search failed ({})", r.status()).red())
                        }
                        Err(e) => eprintln!("{}", format!("  ✗ Unreachable: {e}").red()),
                    }
                }
                // ─── Inspect one memory by id ──────────────────────
                "show" if !sub_arg.is_empty() => {
                    let memory_id = sub_arg.trim();
                    let mem = astra_core::MemoriaSettings::from_env();
                    let Some(bearer) = mem.bearer_token() else {
                        eprintln!(
                            "  {}",
                            "Memoria not configured (MEMORIA_MASTER_KEY missing).".red()
                        );
                        return Ok(());
                    };
                    match reqwest::Client::builder()
                        .timeout(std::time::Duration::from_secs(5))
                        .no_proxy()
                        .build()
                    {
                        Ok(client) => {
                            let url = format!("{}/v1/memories/{memory_id}", mem.base_url);
                            match client
                                .get(&url)
                                .header("Authorization", bearer)
                                .send()
                                .await
                            {
                                Ok(r) if r.status().is_success() => {
                                    let body = r.text().await.unwrap_or_default();
                                    print_json_or_raw(&body);
                                }
                                Ok(r) => eprintln!(
                                    "{}",
                                    format!("  ✗ Not found ({}): {memory_id}", r.status()).red()
                                ),
                                Err(e) => {
                                    eprintln!("{}", format!("  ✗ Memoria unreachable: {e}").red())
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!("{}", format!("  ✗ Client build failed: {e}").red())
                        }
                    }
                }
                // ─── Hard-delete one memory by id ──────────────────
                "forget" if !sub_arg.is_empty() => {
                    // Grammar: "forget <id> --reason TEXT"
                    let (memory_id, reason) = match parse_memory_forget_args(sub_arg) {
                        Ok(parsed) => parsed,
                        Err(msg) => {
                            eprintln!("  {}", msg.yellow());
                            return Ok(());
                        }
                    };
                    let mem = astra_core::MemoriaSettings::from_env();
                    let Some(bearer) = mem.bearer_token() else {
                        eprintln!(
                            "  {}",
                            "Memoria not configured (MEMORIA_MASTER_KEY missing).".red()
                        );
                        return Ok(());
                    };
                    // v1 /v1/memories/purge with memory_ids=[id], reason.
                    let url = format!("{}/v1/memories/purge", mem.base_url);
                    let body =
                        serde_json::json!({"memory_ids": [memory_id.clone()], "reason": reason});
                    match reqwest::Client::builder()
                        .timeout(std::time::Duration::from_secs(5))
                        .no_proxy()
                        .build()
                    {
                        Ok(client) => {
                            match client
                                .post(&url)
                                .header("Authorization", bearer)
                                .json(&body)
                                .send()
                                .await
                            {
                                Ok(r) if r.status().is_success() => {
                                    eprintln!(
                                        "  {} Forgot memory {}",
                                        theme::icon_ok(),
                                        memory_id.magenta()
                                    );
                                }
                                Ok(r) => eprintln!(
                                    "{}",
                                    format!("  ✗ Purge failed ({}): {memory_id}", r.status()).red()
                                ),
                                Err(e) => {
                                    eprintln!("{}", format!("  ✗ Memoria unreachable: {e}").red())
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!("{}", format!("  ✗ Client build failed: {e}").red())
                        }
                    }
                }
                // ─── Cloud: Snapshots ──────────────────────────────
                "snapshot" => {
                    let name = if sub_arg.is_empty() {
                        format!("snap_{}", chrono::Utc::now().format("%Y%m%d_%H%M%S"))
                    } else {
                        sub_arg.to_string()
                    };
                    match super::edge_tools::memoria::memoria_snapshot_create(&name).await {
                        Ok(_) => {
                            eprintln!(
                                "  {} Snapshot '{}' created",
                                theme::icon_ok(),
                                name.magenta()
                            )
                        }
                        Err(e) => eprintln!("  {} Snapshot failed: {e}", theme::icon_err()),
                    }
                }
                "rollback" if !sub_arg.is_empty() => {
                    match super::edge_tools::memoria::memoria_snapshot_rollback(sub_arg).await {
                        Ok(_) => {
                            eprintln!(
                                "  {} Rolled back to '{}'",
                                theme::icon_ok(),
                                sub_arg.magenta()
                            )
                        }
                        Err(e) => eprintln!("  {} Rollback failed: {e}", theme::icon_err()),
                    }
                }
                "snapshots" => match super::edge_tools::memoria::memoria_snapshots_list().await {
                    Ok(body) => print_json_or_raw(&body),
                    Err(e) => eprintln!("  {} {e}", theme::icon_err()),
                },
                // ─── Cloud: Branches ──────────────────────────────
                "branch" if !sub_arg.is_empty() => {
                    match super::edge_tools::memoria::memoria_branch_create(sub_arg).await {
                        Ok(_) => {
                            eprintln!(
                                "  {} Branch '{}' created",
                                theme::icon_ok(),
                                sub_arg.magenta()
                            )
                        }
                        Err(e) => eprintln!("  {} Branch failed: {e}", theme::icon_err()),
                    }
                }
                "checkout" if !sub_arg.is_empty() => {
                    match super::edge_tools::memoria::memoria_branch_checkout(sub_arg).await {
                        Ok(_) => eprintln!(
                            "  {} Switched to branch '{}'",
                            theme::icon_ok(),
                            sub_arg.magenta()
                        ),
                        Err(e) => eprintln!("  {} Checkout failed: {e}", theme::icon_err()),
                    }
                }
                "merge" if !sub_arg.is_empty() => {
                    match super::edge_tools::memoria::memoria_branch_merge(sub_arg).await {
                        Ok(_) => {
                            eprintln!(
                                "  {} Branch '{}' merged",
                                theme::icon_ok(),
                                sub_arg.magenta()
                            )
                        }
                        Err(e) => eprintln!("  {} Merge failed: {e}", theme::icon_err()),
                    }
                }
                "diff" if !sub_arg.is_empty() => {
                    // Try branch diff first; fall back to snapshot diff on 404.
                    match super::edge_tools::memoria::memoria_branch_diff(sub_arg).await {
                        Ok(body) => print_json_or_raw(&body),
                        Err(branch_err) => {
                            match super::edge_tools::memoria::memoria_snapshot_diff(sub_arg).await {
                                Ok(body) => print_json_or_raw(&body),
                                Err(_) => eprintln!(
                                    "  {} diff failed (branch: {branch_err})",
                                    theme::icon_err()
                                ),
                            }
                        }
                    }
                }
                "branches" => match super::edge_tools::memoria::memoria_branches_list().await {
                    Ok(body) => print_json_or_raw(&body),
                    Err(e) => eprintln!("  {} {e}", theme::icon_err()),
                },
                // ─── Cloud: Analysis ─────────────────────────────
                "reflect" => {
                    eprintln!("  {} Analyzing memory patterns...", "⋯".dim());
                    match super::edge_tools::memoria::memoria_reflect().await {
                        Ok(body) => print_json_or_raw(&body),
                        Err(e) => eprintln!("  {} {e}", theme::icon_err()),
                    }
                }
                "health" => match super::edge_tools::memoria::memoria_health().await {
                    Ok(body) => print_json_or_raw(&body),
                    Err(e) => eprintln!("  {} {e}", theme::icon_err()),
                },
                // ─── Current session memory ───────────────────────
                "session" => {
                    let payload = serde_json::json!({});
                    match api.post_memory_retrieve_json(tok, &payload).await {
                        Ok(r) if r.status().is_success() => {
                            let text = r.text().await.unwrap_or_default();
                            // Response may be JSON {"content":"..."} or plain markdown
                            let body =
                                if let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) {
                                    val.get("content")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or(&text)
                                        .to_string()
                                } else {
                                    text
                                };
                            eprintln!("{}", format_session_memory_display(&body));
                        }
                        Ok(r) => eprintln!(
                            "{}",
                            format!("  ✗ Memory retrieve failed ({})", r.status()).red()
                        ),
                        Err(e) => {
                            eprintln!("{}", format!("  ✗ Memoria unreachable: {e}").red())
                        }
                    }
                }
                // ─── Edit a session memory section ───────────────
                "edit" => {
                    let valid_sections = [
                        "Active Goals",
                        "Pending Todos",
                        "Completed",
                        "L0 Critical",
                        "L1 Important",
                        "L2 Contextual",
                        "Learnings",
                    ];
                    if sub_arg.is_empty() || !valid_sections.contains(&sub_arg) {
                        eprintln!("  {} /memory edit <section>", "Usage:".dim());
                        eprintln!("  Sections: {}", valid_sections.join(" | "));
                        return Ok(());
                    }
                    let section = sub_arg;
                    // 1. Retrieve current session memory
                    let current_body = match api
                        .post_memory_retrieve_json(tok, &serde_json::json!({}))
                        .await
                    {
                        Ok(r) if r.status().is_success() => {
                            let text = r.text().await.unwrap_or_default();
                            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) {
                                val.get("content")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or(&text)
                                    .to_string()
                            } else {
                                text
                            }
                        }
                        _ => String::new(),
                    };
                    // 2. Open $EDITOR with current section content
                    let current_section =
                        extract_md_section(&current_body, section).unwrap_or_default();
                    let tmp = std::env::temp_dir().join(format!(
                        "astra_memory_edit_{}.md",
                        section.replace(' ', "_").to_lowercase()
                    ));
                    std::fs::write(&tmp, current_section.trim_start_matches('\n'))
                        .unwrap_or_default();
                    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
                    match std::process::Command::new(&editor).arg(&tmp).status() {
                        Ok(s) if s.success() => {}
                        _ => {
                            eprintln!("  {} Editor exited with error", theme::icon_err());
                            return Ok(());
                        }
                    }
                    // 3. Read edited content and store back
                    let new_content = std::fs::read_to_string(&tmp).unwrap_or_default();
                    let _ = std::fs::remove_file(&tmp);
                    let updated = replace_md_section(&current_body, section, &new_content);
                    match api
                        .post_memory_store_json(tok, &serde_json::json!({ "content": updated }))
                        .await
                    {
                        Ok(r) if r.status().is_success() => {
                            eprintln!("  {} Section {section:?} updated.", theme::icon_ok())
                        }
                        Ok(r) => eprintln!("  {} Store failed ({})", theme::icon_err(), r.status()),
                        Err(e) => eprintln!("  {} Memoria unreachable: {e}", theme::icon_err()),
                    }
                }

                _ => {
                    eprintln!("  {} /memory <subcommand>", "Usage:".dim());
                    eprintln!("  {}", "  list                    List all memories".dim());
                    eprintln!("  {}", "  search <query>          Search memories".dim());
                    eprintln!(
                        "  {}",
                        "  dismiss <query>         Mark memories as irrelevant".dim()
                    );
                    eprintln!(
                        "  {}",
                        "  snapshot [name]         Create memory checkpoint".dim()
                    );
                    eprintln!(
                        "  {}",
                        "  rollback <name>         Restore to checkpoint".dim()
                    );
                    eprintln!("  {}", "  snapshots               List checkpoints".dim());
                    eprintln!(
                        "  {}",
                        "  branch <name>           Create experiment branch".dim()
                    );
                    eprintln!("  {}", "  checkout <name>         Switch branch".dim());
                    eprintln!("  {}", "  merge <name>            Merge branch back".dim());
                    eprintln!(
                        "  {}",
                        "  diff <name>             Preview branch changes".dim()
                    );
                    eprintln!("  {}", "  branches                List branches".dim());
                    eprintln!(
                        "  {}",
                        "  reflect                 Analyze memory patterns".dim()
                    );
                    eprintln!(
                        "  {}",
                        "  health                  Memory hygiene status".dim()
                    );
                }
            }
        }

        _ => unreachable!("unexpected memory-domain command: {cmd}"),
    }

    Ok(())
}

/// Format the session memory markdown body for human-readable terminal display.
/// Pure function — no I/O, no API calls. Testable in isolation.
pub(crate) fn format_session_memory_display(body: &str) -> String {
    if body.trim().is_empty() {
        return format!(
            "  {}\n  {}\n",
            "No session memory extracted yet.".dim(),
            "Tip: /save triggers early extraction.".dim()
        );
    }

    let mut out = String::new();
    out.push_str(&format!(
        "\n  {}\n",
        "── Session Memory ──────────────────────────────────────".dim()
    ));

    // Priority-ordered sections: actionable state first, then background
    let sections: &[(&str, &str)] = &[
        ("Active Goals", "🎯 Active Goals"),
        ("Pending Todos", "📌 Pending Todos"),
        ("Completed", "✅ Completed"),
        ("L0 Critical", "🔒 Critical (L0)"),
        ("L1 Important", "📝 Important (L1)"),
        ("Learnings", "💡 Learnings"),
        ("L2 Contextual", "📋 Context (L2)"),
    ];

    for (section_name, label) in sections {
        if let Some(content) = extract_md_section(body, section_name) {
            let trimmed = content.trim();
            if !trimmed.is_empty() {
                out.push_str(&format!("\n  {}\n", label.bold()));
                for line in trimmed.lines().take(15) {
                    out.push_str(&format!("    {line}\n"));
                }
            }
        }
    }

    out.push_str(&format!(
        "\n  {}\n",
        "───────────────────────────────────────────────────────".dim()
    ));
    out
}

/// Extract a `## SectionName` block from a markdown string.
/// Returns content between the header and the next `##` header (exclusive).
fn extract_md_section(md: &str, section_name: &str) -> Option<String> {
    let needle = format!("## {section_name}");
    let start = md.find(&needle)?;
    let after_header = start + needle.len();
    let rest = &md[after_header..];
    // Find start of next ## header (the newline before it)
    let end = rest.find("\n## ").map(|i| i + 1).unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

/// Replace (or append) a `## SectionName` block in a markdown string.
/// If the section exists its content is replaced; otherwise the section is appended.
/// Pure function — no I/O.
pub(crate) fn replace_md_section(md: &str, section_name: &str, new_content: &str) -> String {
    let needle = format!("## {section_name}");
    if let Some(start) = md.find(&needle) {
        let after_header = start + needle.len();
        let rest = &md[after_header..];
        let end = rest.find("\n## ").map(|i| i + 1).unwrap_or(rest.len());
        let before = &md[..after_header];
        let after = &rest[end..];
        format!("{}\n{}{}", before, new_content, after)
    } else {
        // Section absent — append at end
        let sep = if md.is_empty() || md.ends_with('\n') {
            ""
        } else {
            "\n"
        };
        format!("{}{}\n## {section_name}\n{}", md, sep, new_content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── /memory session display ──────────────────────────────────────────

    #[test]
    fn format_session_memory_display_empty_is_graceful() {
        let result = format_session_memory_display("");
        assert!(
            result.contains("No session memory") || result.contains("not yet extracted"),
            "empty body should show a helpful message, got: {result:?}"
        );
    }

    #[test]
    fn format_session_memory_display_shows_l0_content() {
        let body = "## L0 Critical\n- Goal: fix auth module\n";
        let result = format_session_memory_display(body);
        assert!(
            result.contains("fix auth module"),
            "should show L0 content, got: {result:?}"
        );
    }

    #[test]
    fn format_session_memory_display_shows_goals_todos_completed() {
        let body = "## Active Goals\n- Refactor memory\n\n## Pending Todos\n- Write tests\n\n## Completed\n- Scaffold done\n";
        let result = format_session_memory_display(body);
        assert!(
            result.contains("Refactor memory"),
            "missing goals, got: {result:?}"
        );
        assert!(
            result.contains("Write tests"),
            "missing todos, got: {result:?}"
        );
        assert!(
            result.contains("Scaffold done"),
            "missing completed, got: {result:?}"
        );
    }

    #[test]
    fn format_session_memory_display_omits_missing_sections() {
        let body = "## L0 Critical\n- only this section\n";
        let result = format_session_memory_display(body);
        // Missing sections must not produce empty labelled blocks
        assert!(
            !result.contains("L1 Important:\n\n"),
            "spurious L1 block, got: {result:?}"
        );
        assert!(
            !result.contains("L2 Contextual:\n\n"),
            "spurious L2 block, got: {result:?}"
        );
    }

    // ── /memory subcommand contracts ──

    #[test]
    fn memory_help_lists_all_cloud_commands() {
        let src = include_str!("slash_memory.rs");
        let test_start = src.find("#[cfg(test)]").unwrap_or(src.len());
        let prod = &src[..test_start];
        for cmd in &[
            "snapshot",
            "rollback",
            "snapshots",
            "branch",
            "checkout",
            "merge",
            "diff",
            "branches",
            "reflect",
            "health",
            "search",
            "dismiss",
            "list",
            "session",
        ] {
            assert!(
                prod.contains(&format!("\"{cmd}\"")),
                "/memory {cmd} subcommand missing from match block"
            );
        }
    }

    #[test]
    fn memory_usage_text_covers_all_subcommands() {
        let src = include_str!("slash_memory.rs");
        for cmd in &[
            "snapshot",
            "rollback",
            "snapshots",
            "branch",
            "checkout",
            "merge",
            "diff",
            "branches",
            "reflect",
            "health",
        ] {
            assert!(
                src.contains(&format!("  {cmd}")),
                "/memory usage text missing {cmd}"
            );
        }
    }

    #[test]
    fn memory_forget_requires_non_empty_reason() {
        assert!(parse_memory_forget_args("mem-1").is_err());
        assert!(parse_memory_forget_args("mem-1 --reason   ").is_err());
        let parsed = parse_memory_forget_args("mem-1 --reason duplicate stale memory").unwrap();
        assert_eq!(parsed.0, "mem-1");
        assert_eq!(parsed.1, "duplicate stale memory");
    }

    // ── /memory edit — replace_md_section ───────────────────────────────

    #[test]
    fn replace_md_section_replaces_existing_section() {
        let body = "## Active Goals\n- old goal\n\n## Pending Todos\n- do stuff\n";
        let result = replace_md_section(body, "Active Goals", "- new goal\n");
        assert!(
            result.contains("- new goal"),
            "new content missing, got: {result:?}"
        );
        assert!(
            !result.contains("- old goal"),
            "old content still present, got: {result:?}"
        );
        // Other sections must be preserved
        assert!(
            result.contains("## Pending Todos"),
            "other section lost, got: {result:?}"
        );
        assert!(
            result.contains("- do stuff"),
            "other section content lost, got: {result:?}"
        );
    }

    #[test]
    fn replace_md_section_adds_section_if_missing() {
        let body = "## L0 Critical\n- keep this\n";
        let result = replace_md_section(body, "Active Goals", "- brand new goal\n");
        assert!(
            result.contains("## Active Goals"),
            "section not added, got: {result:?}"
        );
        assert!(
            result.contains("- brand new goal"),
            "content not added, got: {result:?}"
        );
        assert!(
            result.contains("## L0 Critical"),
            "existing section lost, got: {result:?}"
        );
    }

    #[test]
    fn replace_md_section_preserves_all_other_sections() {
        let body = "## Active Goals\n- goal\n\n## Pending Todos\n- todo\n\n## Completed\n- done\n\n## L0 Critical\n- critical\n";
        let result = replace_md_section(body, "Pending Todos", "- new todo\n");
        assert!(result.contains("## Active Goals"), "Active Goals lost");
        assert!(result.contains("- goal"), "Active Goals content lost");
        assert!(result.contains("## Completed"), "Completed lost");
        assert!(result.contains("- done"), "Completed content lost");
        assert!(result.contains("## L0 Critical"), "L0 Critical lost");
        assert!(result.contains("- critical"), "L0 Critical content lost");
        assert!(result.contains("- new todo"), "new content missing");
        assert!(
            !result.contains("- todo\n"),
            "old todo content still present"
        );
    }

    #[test]
    fn replace_md_section_valid_section_names() {
        // All recognised section names must work without error
        for name in &[
            "Active Goals",
            "Pending Todos",
            "Completed",
            "L0 Critical",
            "L1 Important",
            "L2 Contextual",
            "Learnings",
        ] {
            let body = format!("## {name}\n- content\n");
            let result = replace_md_section(&body, name, "- replaced\n");
            assert!(
                result.contains("- replaced"),
                "replace failed for section {name:?}"
            );
        }
    }

    #[test]
    fn memory_edit_subcommand_exists_in_router() {
        let src = include_str!("slash_memory.rs");
        let test_start = src.find("#[cfg(test)]").unwrap_or(src.len());
        let prod = &src[..test_start];
        assert!(
            prod.contains("\"edit\""),
            "/memory edit subcommand missing from match block"
        );
    }
}
