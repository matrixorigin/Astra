use super::*;

const SECTION_NAMES: &[&str] = &[
    "Active Goals",
    "Pending Todos",
    "Completed",
    "L0 Critical",
    "L1 Important",
    "L2 Contextual",
    "Learnings",
];

const SECTION_DISPLAY_NAMES: &[(&str, &str)] = &[
    ("Active Goals", "🎯 Active Goals"),
    ("Pending Todos", "📌 Pending Todos"),
    ("Completed", "✅ Completed"),
    ("L0 Critical", "🔒 Critical (L0)"),
    ("L1 Important", "📝 Important (L1)"),
    ("Learnings", "💡 Learnings"),
    ("L2 Contextual", "📋 Context (L2)"),
];

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionMemoryRecord {
    memory_id: String,
    body: String,
}

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
                                        let _ = super::edge_tools::memoria::memoria_feedback(
                                            mid,
                                            "irrelevant",
                                            Some("user /memory dismiss"),
                                        )
                                        .await;
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
                    match super::edge_tools::memoria::memoria_show(memory_id).await {
                        Ok(body) => print_json_or_raw(&body),
                        Err(e) => eprintln!("{}", format!("  ✗ Show failed: {e}").red()),
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
                    let body =
                        serde_json::json!({"memory_ids": [memory_id.clone()], "reason": reason});
                    match super::edge_tools::memoria::memoria_purge(&body).await {
                        Ok(_) => {
                            eprintln!(
                                "  {} Forgot memory {}",
                                theme::icon_ok(),
                                memory_id.magenta()
                            );
                        }
                        Err(e) => eprintln!("{}", format!("  ✗ Purge failed: {e}").red()),
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
                    let Some(session_id) = state.session_id.as_deref() else {
                        eprintln!("  {}", "No active session yet.".yellow());
                        return Ok(());
                    };
                    match load_current_session_memory(api, tok, session_id).await {
                        Ok(record) => {
                            let body = record.map(|memory| memory.body).unwrap_or_default();
                            eprintln!("{}", format_session_memory_display(&body));
                        }
                        Err(e) => eprintln!("{}", format!("  ✗ Session memory failed: {e}").red()),
                    }
                }
                // ─── Edit a session memory section ───────────────
                "edit" => {
                    if sub_arg.is_empty() || !SECTION_NAMES.contains(&sub_arg) {
                        eprintln!("  {} /memory edit <section>", "Usage:".dim());
                        eprintln!("  Sections: {}", SECTION_NAMES.join(" | "));
                        return Ok(());
                    }
                    let section = sub_arg;
                    let Some(session_id) = state.session_id.as_deref() else {
                        eprintln!("  {}", "No active session yet.".yellow());
                        return Ok(());
                    };
                    // 1. Retrieve current session memory
                    let current_record = match load_current_session_memory(api, tok, session_id).await
                    {
                        Ok(record) => record,
                        Err(e) => {
                            eprintln!("  {} {e}", theme::icon_err());
                            return Ok(());
                        }
                    };
                    let (current_memory_id, current_body) = current_record
                        .map(|record| (Some(record.memory_id), record.body))
                        .unwrap_or((None, String::new()));
                    // 2. Open $EDITOR with current section content
                    let current_section =
                        extract_md_section(&current_body, section).unwrap_or_default();
                    let mut tmp = match tempfile::Builder::new()
                        .prefix(&format!(
                            "astra_memory_edit_{}_",
                            section.replace(' ', "_").to_lowercase()
                        ))
                        .suffix(".md")
                        .rand_bytes(6)
                        .tempfile_in(std::env::temp_dir())
                    {
                        Ok(tmp) => tmp,
                        Err(e) => {
                            eprintln!("  {} Failed to create temp file: {e}", theme::icon_err());
                            return Ok(());
                        }
                    };
                    if let Err(e) =
                        std::io::Write::write_all(tmp.as_file_mut(), current_section.as_bytes())
                    {
                        eprintln!("  {} Failed to seed temp file: {e}", theme::icon_err());
                        return Ok(());
                    }
                    if let Err(e) = tmp.as_file().sync_all() {
                        eprintln!("  {} Failed to flush temp file: {e}", theme::icon_err());
                        return Ok(());
                    }
                    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
                    match std::process::Command::new(&editor).arg(tmp.path()).status() {
                        Ok(s) if s.success() => {}
                        _ => {
                            eprintln!("  {} Editor exited with error", theme::icon_err());
                            return Ok(());
                        }
                    }
                    // 3. Read edited content and store back
                    let new_content = match std::fs::read_to_string(tmp.path()) {
                        Ok(content) => content,
                        Err(e) => {
                            eprintln!("  {} Failed to read edited content: {e}", theme::icon_err());
                            return Ok(());
                        }
                    };
                    if normalize_section_content(&new_content)
                        == normalize_section_content(&current_section)
                    {
                        eprintln!("  {} No changes for section {section:?}.", "⋯".dim());
                        return Ok(());
                    }
                    let updated = replace_md_section(&current_body, section, &new_content);
                    match store_current_session_memory(
                        api,
                        tok,
                        session_id,
                        current_memory_id.as_deref(),
                        &updated,
                    )
                    .await
                    {
                        Ok(()) => {
                            eprintln!("  {} Section {section:?} updated.", theme::icon_ok())
                        }
                        Err(e) => eprintln!("  {} {e}", theme::icon_err()),
                    }
                }

                _ => {
                    eprintln!("  {} /memory <subcommand>", "Usage:".dim());
                    eprintln!("  {}", "  list                    List all memories".dim());
                    eprintln!("  {}", "  search <query>          Search memories".dim());
                    eprintln!(
                        "  {}",
                        "  session                 Show session memory".dim()
                    );
                    eprintln!(
                        "  {}",
                        "  edit <section>          Edit one session section".dim()
                    );
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
    for (section_name, label) in SECTION_DISPLAY_NAMES {
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

fn select_session_memory_record(
    payload: &serde_json::Value,
    session_id: &str,
) -> Option<SessionMemoryRecord> {
    payload
        .get("memories")
        .and_then(serde_json::Value::as_array)
        .or_else(|| payload.as_array())
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            serde_json::from_value::<astra_runtime::turn::cloud::memoria_compact::MemoriaMemory>(
                entry.clone(),
            )
            .ok()
        })
        .find_map(|memory| {
            astra_runtime::session_memory::runner::decode_session_memory_entry(
                &memory.content,
                session_id,
            )
            .map(|body| SessionMemoryRecord {
                memory_id: memory.memory_id,
                body,
            })
        })
}

async fn load_current_session_memory(
    api: &astra_thin_client::ThinClient,
    token: &str,
    session_id: &str,
) -> Result<Option<SessionMemoryRecord>, String> {
    let payload = serde_json::json!({
        "query": astra_runtime::session_memory::runner::SESSION_MEMORY_PREFIX,
        "top_k": 8,
        "session_id": session_id,
        "session_scope": "only",
    });
    let response = api
        .post_memory_retrieve_json(token, &payload)
        .await
        .map_err(|error| format!("memory retrieve failed: {error}"))?;
    let status = response.status();
    let payload: serde_json::Value = response
        .json()
        .await
        .map_err(|error| format!("memory retrieve parse failed: {error}"))?;
    if !status.is_success() {
        return Err(format!("memory retrieve failed ({status})"));
    }
    Ok(select_session_memory_record(&payload, session_id))
}

pub(super) async fn load_current_session_memory_body_with_profile(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    session_id: &str,
) -> Option<String> {
    let token = session_runtime::fresh_access_token(api, profile).await?;
    load_current_session_memory(api, &token, session_id)
        .await
        .ok()
        .flatten()
        .map(|record| record.body)
}

async fn store_current_session_memory(
    api: &astra_thin_client::ThinClient,
    token: &str,
    session_id: &str,
    memory_id: Option<&str>,
    body: &str,
) -> Result<(), String> {
    let encoded =
        astra_runtime::session_memory::runner::encode_session_memory_entry(session_id, body);
    if let Some(memory_id) = memory_id {
        let path = format!("/memory/{memory_id}/correct");
        let payload = serde_json::json!({
            "new_content": encoded,
            "reason": "manual /memory edit",
        });
        api
            .put_bearer_path_json_text(token, &path, &payload)
            .await
            .map_err(|error| format!("memory update failed: {error}"))?;
        return Ok(());
    }

    let payload = serde_json::json!({
        "content": encoded,
        "memory_type": "working",
        "session_id": session_id,
    });
    let response = api
        .post_memory_store_json(token, &payload)
        .await
        .map_err(|error| format!("memory store failed: {error}"))?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(format!("memory store failed ({})", response.status()))
    }
}

/// Extract a `## SectionName` block from a markdown string.
/// Returns content between the header and the next `##` header (exclusive).
fn extract_md_section(md: &str, section_name: &str) -> Option<String> {
    let (_, content_start, section_end) = find_md_section_bounds(md, section_name)?;
    Some(md[content_start..section_end].to_string())
}

/// Replace (or append) a `## SectionName` block in a markdown string.
/// If the section exists its content is replaced; otherwise the section is appended.
/// Pure function — no I/O.
pub(crate) fn replace_md_section(md: &str, section_name: &str, new_content: &str) -> String {
    let normalized = normalize_section_content(new_content);
    if let Some((_, content_start, section_end)) = find_md_section_bounds(md, section_name) {
        format!(
            "{}{}{}",
            &md[..content_start],
            normalized,
            &md[section_end..]
        )
    } else {
        // Section absent — append at end
        let sep = if md.is_empty() || md.ends_with('\n') {
            ""
        } else {
            "\n"
        };
        format!("{}{}## {section_name}\n{}", md, sep, normalized)
    }
}

fn normalize_section_content(content: &str) -> String {
    if content.is_empty() || content.ends_with('\n') {
        content.to_string()
    } else {
        format!("{content}\n")
    }
}

fn find_md_section_bounds(md: &str, section_name: &str) -> Option<(usize, usize, usize)> {
    let mut offset = 0usize;
    let mut section_start = None;
    let mut content_start = None;
    let mut active_fence: Option<&'static str> = None;

    for line in md.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        let heading = if active_fence.is_none() {
            markdown_h2_heading(trimmed)
        } else {
            None
        };

        if let Some(start) = section_start {
            if heading.is_some() {
                return Some((start, content_start.unwrap_or(offset), offset));
            }
        } else if heading == Some(section_name) {
            section_start = Some(offset);
            content_start = Some(offset + line.len());
        }

        update_fenced_code_block_state(trimmed, &mut active_fence);
        offset += line.len();
    }

    section_start.map(|start| (start, content_start.unwrap_or(md.len()), md.len()))
}

fn markdown_h2_heading(line: &str) -> Option<&str> {
    line.strip_prefix("## ").map(str::trim)
}

fn update_fenced_code_block_state(line: &str, active_fence: &mut Option<&'static str>) {
    let trimmed = line.trim_start();
    for fence in ["```", "~~~"] {
        if trimmed.starts_with(fence) {
            match *active_fence {
                Some(active) if active == fence => *active_fence = None,
                None => *active_fence = Some(fence),
                _ => {}
            }
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regex::Regex;

    fn strip_ansi(input: &str) -> String {
        Regex::new(r"\x1b\[[0-9;]*m")
            .unwrap()
            .replace_all(input, "")
            .into_owned()
    }

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
        let result = strip_ansi(&format_session_memory_display(body));
        // Missing sections must not produce empty labelled blocks
        assert!(
            !result.contains("📝 Important (L1)"),
            "spurious L1 block, got: {result:?}"
        );
        assert!(
            !result.contains("📋 Context (L2)"),
            "spurious L2 block, got: {result:?}"
        );
    }

    #[test]
    fn select_session_memory_record_decodes_protocol_entry() {
        let payload = serde_json::json!({
            "memories": [
                {
                    "memory_id": "mem-1",
                    "content": astra_runtime::session_memory::runner::encode_session_memory_entry(
                        "sess-1",
                        "## Active Goals\n- Fix memory\n"
                    ),
                    "memory_type": "working",
                    "session_id": "sess-1"
                },
                {
                    "memory_id": "mem-2",
                    "content": "unrelated",
                    "memory_type": "working",
                    "session_id": "sess-1"
                }
            ]
        });
        let record = select_session_memory_record(&payload, "sess-1").expect("session memory");
        assert_eq!(record.memory_id, "mem-1");
        assert!(record.body.contains("Fix memory"));
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
            "edit",
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
            "session",
        ] {
            assert!(
                src.contains(&format!("  {cmd}")),
                "/memory usage text missing {cmd}"
            );
        }
        assert!(
            src.contains("  edit <section>"),
            "/memory usage text missing edit <section>"
        );
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
    fn extract_md_section_returns_exact_named_section() {
        let body = "## Active Goals\n- old goal\n\n## Pending Todos\n- do stuff\n";
        let result = extract_md_section(body, "Active Goals").expect("section exists");
        assert_eq!(result, "- old goal\n\n");
    }

    #[test]
    fn extract_md_section_ignores_fenced_code_block_headers() {
        let body = "## Active Goals\n```md\n## Pending Todos\n- fake header\n```\n- keep this\n\n## Pending Todos\n- real todo\n";
        let result = extract_md_section(body, "Active Goals").expect("section exists");
        assert!(
            result.contains("## Pending Todos\n- fake header"),
            "fenced header should stay inside section: {result:?}"
        );
        assert!(
            result.contains("- keep this"),
            "real content missing: {result:?}"
        );
        assert!(
            !result.ends_with("## Pending Todos\n- real todo\n"),
            "next real section should not leak into extracted section: {result:?}"
        );
    }

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
        for name in SECTION_NAMES {
            let body = format!("## {name}\n- content\n");
            let result = replace_md_section(&body, name, "- replaced\n");
            assert!(
                result.contains("- replaced"),
                "replace failed for section {name:?}"
            );
        }
    }

    #[test]
    fn replace_md_section_ignores_fenced_headers_inside_target_section() {
        let body = "## Active Goals\n```md\n## Pending Todos\n- fake header\n```\n- keep goal\n\n## Pending Todos\n- real todo\n";
        let result = replace_md_section(body, "Active Goals", "- rewritten goal\n");
        assert!(
            result.contains("## Active Goals\n- rewritten goal\n"),
            "target section should be rewritten cleanly: {result:?}"
        );
        assert!(
            !result.contains("```md\n## Pending Todos\n- fake header\n```"),
            "old fenced content should be removed with the rewritten section: {result:?}"
        );
        assert!(
            result.contains("## Pending Todos\n- real todo\n"),
            "next real section should still be present: {result:?}"
        );
    }

    #[test]
    fn replace_md_section_ignores_fenced_headers_when_matching_target() {
        let body = "## Active Goals\n```md\n## Pending Todos\n- fake header\n```\n- keep goal\n\n## Pending Todos\n- real todo\n";
        let result = replace_md_section(body, "Pending Todos", "- updated todo\n");
        assert!(
            result.contains("```md\n## Pending Todos\n- fake header\n```"),
            "code block content should stay untouched: {result:?}"
        );
        assert!(
            result.contains("- keep goal"),
            "preceding section content should stay untouched: {result:?}"
        );
        assert!(
            result.contains("## Pending Todos\n- updated todo\n"),
            "real section should be replaced: {result:?}"
        );
        assert!(
            !result.contains("## Pending Todos\n- real todo\n"),
            "old real section content should be replaced: {result:?}"
        );
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
