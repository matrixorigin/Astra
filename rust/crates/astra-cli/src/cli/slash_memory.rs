use super::*;

pub(crate) const MEMORY_BROWSE_QUERY: &str = "memory knowledge fact preference plan task note";
pub(crate) const MEMORY_BROWSE_TOP_K: usize = 50;
pub(crate) const MEMORY_STATS_TOP_K: usize = 200;
const MEMORY_DISMISS_TOP_K: usize = 3;

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

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionMemoryStatusHint {
    summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DismissCandidate {
    memory_id: String,
    preview: String,
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
                                    render_memory_search_results(&arr, sub_arg);
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
                _ if sub_arg.is_empty() && (subcmd == "list" || subcmd == "ls") => {
                    let payload = serde_json::json!({
                        "query": MEMORY_BROWSE_QUERY,
                        "top_k": MEMORY_BROWSE_TOP_K,
                    });
                    match api.post_memory_search_json(tok, &payload).await {
                        Ok(r) if r.status().is_success() => {
                            let body = r.text().await.unwrap_or_default();
                            if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(&body) {
                                if arr.is_empty() {
                                    eprintln!("  {}", "No memories stored yet.".dim());
                                } else {
                                    render_memory_list(&arr);
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
                    let payload = serde_json::json!({
                        "query": sub_arg,
                        "top_k": MEMORY_DISMISS_TOP_K,
                    });
                    match api.post_memory_search_json(tok, &payload).await {
                        Ok(r) if r.status().is_success() => {
                            let body = r.text().await.unwrap_or_default();
                            if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(&body) {
                                let candidates = collect_dismiss_candidates(&arr);
                                if candidates.is_empty() {
                                    eprintln!(
                                        "  {}",
                                        "No matching non-session memories to dismiss.".dim()
                                    );
                                } else {
                                    use std::io::{IsTerminal, Write};

                                    // Bail out *before* rendering candidates so
                                    // scripted/CI runs don't see a misleading
                                    // "we're about to dismiss these" preview
                                    // followed by a refusal at the bottom.
                                    if !std::io::stdin().is_terminal() {
                                        eprintln!(
                                            "  {} {}",
                                            theme::icon_warn(),
                                            "Cannot confirm /memory dismiss in non-interactive mode."
                                                .yellow()
                                        );
                                        return Ok(());
                                    }

                                    eprintln!();
                                    eprintln!(
                                        "  {} Dismiss the following memor{}?",
                                        theme::icon_warn(),
                                        if candidates.len() == 1 { "y" } else { "ies" }
                                    );
                                    for candidate in &candidates {
                                        eprintln!(
                                            "    • {}  {}",
                                            candidate.preview,
                                            prefix_chars(&candidate.memory_id, 8).dim()
                                        );
                                    }
                                    eprint!("  Confirm [y/N]: ");
                                    let _ = std::io::stderr().flush();
                                    let mut answer = String::new();
                                    if std::io::stdin().read_line(&mut answer).is_err()
                                        || !answer.trim().eq_ignore_ascii_case("y")
                                    {
                                        eprintln!("  {}", "Cancelled.".dim());
                                        return Ok(());
                                    }
                                    let mut dismissed = 0u32;
                                    let mut failed = 0u32;
                                    for candidate in candidates {
                                        match super::edge_tools::memoria::memoria_feedback(
                                            &candidate.memory_id,
                                            "irrelevant",
                                            Some("user /memory dismiss"),
                                        )
                                        .await
                                        {
                                            Ok(_) => {
                                                eprintln!(
                                                    "  {} dismissed: {}",
                                                    theme::icon_ok(),
                                                    candidate.preview
                                                );
                                                dismissed += 1;
                                            }
                                            Err(error) => {
                                                eprintln!(
                                                    "  {} failed to dismiss {}: {error}",
                                                    theme::icon_err(),
                                                    candidate.preview
                                                );
                                                failed += 1;
                                            }
                                        }
                                    }
                                    if dismissed > 0 {
                                        eprintln!(
                                            "  {} memories dismissed (retrieval score lowered)",
                                            dismissed.to_string().magenta()
                                        );
                                    }
                                    if failed > 0 {
                                        eprintln!(
                                            "  {} dismiss operation{} failed",
                                            failed.to_string().yellow(),
                                            if failed == 1 { "" } else { "s" }
                                        );
                                    }
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
                "show" | "inspect" if !sub_arg.is_empty() => {
                    let memory_id = sub_arg.trim();
                    match super::edge_tools::memoria::memoria_show(memory_id).await {
                        Ok(body) => print_memory_detail(&body),
                        Err(e) => eprintln!("{}", format!("  ✗ Show failed: {e}").red()),
                    }
                }
                // ─── Hard-delete one memory by id ──────────────────
                "forget" if !sub_arg.is_empty() => {
                    // Grammar: "forget <id> --reason TEXT"
                    let (memory_id, reason) = match parse_memory_forget_args(sub_arg) {
                        Ok(parsed) => parsed,
                        Err(msg) => {
                            eprintln!(
                                "  {}",
                                format!("Could not parse {sub_arg:?}. {msg}").yellow()
                            );
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
                    Ok(body) => print_snapshots_list(&body),
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
                        Ok(body) => print_memory_diff(&body, sub_arg),
                        Err(branch_err) => {
                            match super::edge_tools::memoria::memoria_snapshot_diff(sub_arg).await {
                                Ok(body) => print_memory_diff(&body, sub_arg),
                                Err(_) => eprintln!(
                                    "  {} diff failed (branch: {branch_err})",
                                    theme::icon_err()
                                ),
                            }
                        }
                    }
                }
                "branches" => match super::edge_tools::memoria::memoria_branches_list().await {
                    Ok(body) => print_branches_list(&body),
                    Err(e) => eprintln!("  {} {e}", theme::icon_err()),
                },
                // ─── Cloud: Analysis ─────────────────────────────
                "reflect" => {
                    eprintln!("  {} Analyzing memory patterns...", "⋯".dim());
                    match super::edge_tools::memoria::memoria_reflect().await {
                        Ok(body) => print_reflect_result(&body),
                        Err(e) => eprintln!("  {} {e}", theme::icon_err()),
                    }
                }
                "health" => match super::edge_tools::memoria::memoria_health().await {
                    Ok(body) => print_health_status(&body),
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
                            let hint = if body.trim().is_empty() {
                                latest_session_memory_status_hint(session_id)
                            } else {
                                None
                            };
                            eprintln!(
                                "{}",
                                format_session_memory_display(
                                    &body,
                                    Some(session_id),
                                    hint.as_ref().map(|hint| hint.summary.as_str())
                                )
                            );
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
                    let current_record =
                        match load_current_session_memory(api, tok, session_id).await {
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

                "stats" | "count" => {
                    let payload = serde_json::json!({
                        "query": MEMORY_BROWSE_QUERY,
                        "top_k": MEMORY_STATS_TOP_K,
                    });
                    match api.post_memory_search_json(tok, &payload).await {
                        Ok(r) if r.status().is_success() => {
                            let body = r.text().await.unwrap_or_default();
                            if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(&body) {
                                render_memory_stats(&arr);
                            } else {
                                print_json_or_raw(&body);
                            }
                        }
                        Ok(r) => {
                            eprintln!("{}", format!("  ✗ Stats failed ({})", r.status()).red())
                        }
                        Err(e) => eprintln!("{}", format!("  ✗ Unreachable: {e}").red()),
                    }
                }

                "help" if sub_arg.is_empty() => {
                    print_memory_usage();
                }

                _ => {
                    print_memory_usage();
                }
            }
        }

        _ => unreachable!("unexpected memory-domain command: {cmd}"),
    }

    Ok(())
}

fn print_memory_usage() {
    eprintln!("  {} /memory <subcommand>", "Usage:".dim());
    eprintln!();
    eprintln!("  {}", "Browse".dim());
    eprintln!(
        "  {}",
        "    list                  List memories grouped by type".dim()
    );
    eprintln!("  {}", "    search <query>        Search by content".dim());
    eprintln!(
        "  {}",
        "    show <id>             Inspect one memory in detail".dim()
    );
    eprintln!("  {}", "    inspect <id>          Alias for show".dim());
    eprintln!(
        "  {}",
        "    stats                 Count memories by type".dim()
    );
    eprintln!(
        "  {}",
        "    dismiss <query>       Mark memories as irrelevant".dim()
    );
    eprintln!("  {}", "    help                  Show this help".dim());
    eprintln!();
    eprintln!("  {}", "Session".dim());
    eprintln!(
        "  {}",
        "    session               Show current session memory".dim()
    );
    eprintln!(
        "  {}",
        "    edit <section>        Edit a session memory section".dim()
    );
    eprintln!();
    eprintln!("  {}", "Manage".dim());
    eprintln!(
        "  {}",
        "    forget <id> --reason  Permanently delete a memory".dim()
    );
    eprintln!(
        "  {}",
        "    snapshot [name]       Create a memory checkpoint".dim()
    );
    eprintln!(
        "  {}",
        "    rollback <name>       Restore to a checkpoint".dim()
    );
    eprintln!(
        "  {}",
        "    snapshots             List all checkpoints".dim()
    );
    eprintln!();
    eprintln!("  {}", "Branches".dim());
    eprintln!(
        "  {}",
        "    branch <name>         Create experiment branch".dim()
    );
    eprintln!("  {}", "    checkout <name>       Switch to a branch".dim());
    eprintln!(
        "  {}",
        "    merge <name>          Merge branch back into main".dim()
    );
    eprintln!(
        "  {}",
        "    diff <name>           Preview branch changes".dim()
    );
    eprintln!("  {}", "    branches              List all branches".dim());
    eprintln!();
    eprintln!("  {}", "Analysis".dim());
    eprintln!(
        "  {}",
        "    reflect               Analyze memory patterns".dim()
    );
    eprintln!(
        "  {}",
        "    health                Memory hygiene status".dim()
    );
}

/// Format the session memory markdown body for human-readable terminal display.
/// Pure function — no I/O, no API calls. Testable in isolation.
pub(crate) fn format_session_memory_display(
    body: &str,
    session_id: Option<&str>,
    status_hint: Option<&str>,
) -> String {
    if body.trim().is_empty() {
        let mut out = String::new();
        out.push_str(&format!("  {}\n", "No session memory yet.".dim()));
        if let Some(sid) = session_id {
            out.push_str(&format!("  {} {}\n", "session:".dim(), sid.dim()));
        }
        if let Some(hint) = status_hint.filter(|hint| !hint.trim().is_empty()) {
            out.push_str(&format!("  {}\n", hint.yellow()));
        }
        out.push_str(&format!(
            "  {}\n",
            "Memory is captured automatically during the conversation. Use /save to capture the current context immediately.".dim()
        ));
        return out;
    }

    let mut out = String::new();
    out.push_str(&format!("\n  {}", "── Session Memory".dim()));
    if let Some(sid) = session_id {
        out.push_str(&format!("    {} {}", "session:".dim(), sid.dim()));
    }
    out.push('\n');

    // Priority-ordered sections: actionable state first, then background
    const PER_SECTION_LIMIT: usize = 12;
    let mut sections_shown = 0usize;
    for (section_name, label) in SECTION_DISPLAY_NAMES {
        if let Some(content) = extract_md_section(body, section_name) {
            let trimmed = content.trim();
            if trimmed.is_empty() {
                continue;
            }
            sections_shown += 1;
            out.push_str(&format!("\n  {}\n", label.bold()));
            let lines: Vec<&str> = trimmed.lines().collect();
            for line in lines.iter().take(PER_SECTION_LIMIT) {
                out.push_str(&format!("    {line}\n"));
            }
            if lines.len() > PER_SECTION_LIMIT {
                out.push_str(&format!(
                    "    {}\n",
                    format!("… {} more lines", lines.len() - PER_SECTION_LIMIT).dim()
                ));
            }
        }
    }

    if sections_shown == 0 {
        out.push_str(&format!("  {}\n", "No sections populated yet.".dim()));
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

fn parse_session_memory_status_hint_from_journal_text(
    journal_text: &str,
) -> Option<SessionMemoryStatusHint> {
    let mut latest_error: Option<SessionMemoryStatusHint> = None;
    let mut latest_skip: Option<SessionMemoryStatusHint> = None;

    for line in journal_text.lines() {
        let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if event.get("type").and_then(serde_json::Value::as_str)
            != Some("session_memory_extraction")
        {
            continue;
        }
        let Some(metadata) = event.get("metadata").and_then(serde_json::Value::as_object) else {
            continue;
        };
        let Some(outcome) = metadata.get("outcome").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let turn = event
            .get("turn")
            .and_then(serde_json::Value::as_u64)
            .map(|turn| format!("turn {turn}"))
            .unwrap_or_else(|| "recent turn".to_string());
        match outcome {
            "errored" => {
                let reason = metadata
                    .get("reason")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown_error");
                let detail = metadata
                    .get("persist_detail")
                    .or_else(|| metadata.get("llm_detail"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let summary = if detail.is_empty() {
                    format!("Latest extraction failed on {turn}: {reason}.")
                } else {
                    format!("Latest extraction failed on {turn}: {reason} — {detail}.")
                };
                latest_error = Some(SessionMemoryStatusHint { summary });
            }
            "skipped" => {
                let reason = metadata
                    .get("reason")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown_skip");
                latest_skip = Some(SessionMemoryStatusHint {
                    summary: format!("Latest extraction was skipped on {turn}: {reason}."),
                });
            }
            _ => {}
        }
    }

    latest_error.or(latest_skip)
}

fn latest_session_memory_status_hint(session_id: &str) -> Option<SessionMemoryStatusHint> {
    let writer = astra_services::session_journal::JournalWriter::new(session_id).ok()?;
    let path = writer.path().clone();
    let journal = std::fs::read_to_string(path).ok()?;
    parse_session_memory_status_hint_from_journal_text(&journal)
}

async fn load_current_session_memory(
    api: &astra_thin_client::ThinClient,
    token: &str,
    session_id: &str,
) -> Result<Option<SessionMemoryRecord>, String> {
    const SESSION_MEMORY_TOP_K: usize = 64;
    let typed_payload = serde_json::json!({
        "query": astra_runtime::session_memory::runner::SESSION_MEMORY_PREFIX,
        "top_k": SESSION_MEMORY_TOP_K,
        "session_id": session_id,
        "session_scope": "only",
        "memory_types": [astra_runtime::session_memory::runner::SESSION_MEMORY_MEMORIA_TYPE],
    });
    if let Ok(response) = api.post_memory_retrieve_json(token, &typed_payload).await {
        let status = response.status();
        if status.is_success() {
            let payload: serde_json::Value = response
                .json()
                .await
                .map_err(|error| format!("memory retrieve parse failed: {error}"))?;
            if let Some(record) = select_session_memory_record(&payload, session_id) {
                return Ok(Some(record));
            }
        }
    }
    let fallback_payload = serde_json::json!({
        "query": astra_runtime::session_memory::runner::SESSION_MEMORY_PREFIX,
        "top_k": SESSION_MEMORY_TOP_K,
        "session_id": session_id,
        "session_scope": "only",
    });
    let response = api
        .post_memory_retrieve_json(token, &fallback_payload)
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
        api.put_bearer_path_json_text(token, &path, &payload)
            .await
            .map_err(|error| format!("memory update failed: {error}"))?;
        return Ok(());
    }

    let payload = serde_json::json!({
        "content": encoded,
        "memory_type": astra_runtime::session_memory::runner::SESSION_MEMORY_MEMORIA_TYPE,
        "session_id": session_id,
    });
    let response = api
        .post_memory_store_json(token, &payload)
        .await
        .map_err(|error| format!("memory store failed: {error}"))?;
    if response.status().is_success() {
        Ok(())
    } else {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        Err(format!("memory store failed ({status}): {}", body.trim()))
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

// ──────────────────────────────────────────────────────────────────────────────
// Memory display helpers
// ──────────────────────────────────────────────────────────────────────────────

/// Returns true if the memory content is an internal session-snapshot entry
/// that should be hidden from general list/search output.
pub(crate) fn is_session_proto(content: &str) -> bool {
    let trimmed = content.trim_start();
    trimmed.starts_with("[@session/memory]") || trimmed.starts_with("[@session/active]")
}

pub(crate) fn memory_result_id(m: &serde_json::Value) -> Option<&str> {
    m.get("id")
        .or_else(|| m.get("memory_id"))
        .and_then(|v| v.as_str())
        .filter(|id| !id.trim().is_empty())
}

/// Build a short one-line display string for a raw memory JSON object.
pub(crate) fn format_memory_entry_line(m: &serde_json::Value) -> String {
    let content = m.get("content").and_then(|v| v.as_str()).unwrap_or("?");
    if let Some(entry) = prompts::memory_proto::MemoryEntry::parse(content) {
        entry.display_line()
    } else {
        let mtype = m.get("memory_type").and_then(|v| v.as_str()).unwrap_or("?");
        let preview: String = content
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(80)
            .collect();
        format!("[{mtype}] {preview}")
    }
}

/// Render `/memory list`: group visible entries by memory_type, collapse session protos.
fn render_memory_list(arr: &[serde_json::Value]) {
    const TYPE_ORDER: &[(&str, &str)] = &[
        ("semantic", "Semantic"),
        ("profile", "Profile"),
        ("procedural", "Procedural"),
        ("episodic", "Episodic"),
        ("working", "Working"),
    ];

    let mut session_count = 0usize;
    let mut buckets: Vec<Vec<&serde_json::Value>> = vec![Vec::new(); TYPE_ORDER.len()];

    for m in arr {
        let content = m.get("content").and_then(|v| v.as_str()).unwrap_or("");
        if is_session_proto(content) {
            session_count += 1;
            continue;
        }
        let mtype = m
            .get("memory_type")
            .and_then(|v| v.as_str())
            .unwrap_or("semantic");
        let idx = TYPE_ORDER
            .iter()
            .position(|(k, _)| *k == mtype)
            .unwrap_or(0);
        buckets[idx].push(m);
    }

    let total_visible: usize = buckets.iter().map(|b| b.len()).sum();
    if total_visible == 0 && session_count == 0 {
        eprintln!("  {}", "No memories stored yet.".dim());
        return;
    }

    let mut counter = 1usize;
    eprintln!();
    for ((_, label), bucket) in TYPE_ORDER.iter().zip(buckets.iter()) {
        if bucket.is_empty() {
            continue;
        }
        eprintln!(
            "  {}",
            format!(
                "── {label} ({}) ──────────────────────────────────────",
                bucket.len()
            )
            .dim()
        );
        for m in bucket {
            let id = memory_result_id(m).unwrap_or("");
            let short_id = prefix_chars(id, 8);
            eprintln!(
                "  {}. {}  {}",
                counter.to_string().magenta(),
                format_memory_entry_line(m),
                short_id.dim()
            );
            counter += 1;
        }
        eprintln!();
    }
    if session_count > 0 {
        eprintln!(
            "  {}",
            format!("{session_count} session entries hidden — /memory session to view").dim()
        );
    }
    eprintln!("  {} memories", total_visible.to_string().magenta());
}

/// Render `/memory search` results: numbered list with session protos filtered out.
fn render_memory_search_results(arr: &[serde_json::Value], query: &str) {
    let mut session_count = 0usize;
    let mut visible: Vec<&serde_json::Value> = Vec::new();
    for m in arr {
        let content = m.get("content").and_then(|v| v.as_str()).unwrap_or("");
        if is_session_proto(content) {
            session_count += 1;
        } else {
            visible.push(m);
        }
    }
    if visible.is_empty() {
        eprintln!("  {}", format!("No results for {query:?}").dim());
    } else {
        for (i, m) in visible.iter().enumerate() {
            let id = memory_result_id(m).unwrap_or("");
            let short_id = prefix_chars(id, 8);
            eprintln!(
                "  {}. {}  {}",
                (i + 1).to_string().magenta(),
                format_memory_entry_line(m),
                short_id.dim()
            );
        }
    }
    if session_count > 0 {
        eprintln!(
            "  {}",
            format!("({session_count} session entries hidden)").dim()
        );
    }
}

/// Pretty-print a single memory for `/memory show`.
fn print_memory_detail(body: &str) {
    if let Ok(m) = serde_json::from_str::<serde_json::Value>(body) {
        let id = memory_result_id(&m).unwrap_or("?");
        let mtype = m.get("memory_type").and_then(|v| v.as_str()).unwrap_or("?");
        let content = m.get("content").and_then(|v| v.as_str()).unwrap_or("?");
        let created = m.get("created_at").and_then(|v| v.as_str()).unwrap_or("");
        let tags: Vec<&str> = m
            .get("tags")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|t| t.as_str()).collect())
            .unwrap_or_default();

        eprintln!("\n  Memory  {}", id.magenta());
        eprintln!("  {}", "─".repeat(50).dim());
        eprintln!("  {:<12}  {mtype}", "type".dim());
        if !created.is_empty() {
            eprintln!("  {:<12}  {created}", "created".dim());
        }
        if !tags.is_empty() {
            eprintln!("  {:<12}  {}", "tags".dim(), tags.join(", "));
        }
        eprintln!("  {}", "─".repeat(50).dim());
        for line in content.lines().take(20) {
            eprintln!("  {line}");
        }
        let line_count = content.lines().count();
        if line_count > 20 {
            eprintln!("  {} ({} more lines)", "…".dim(), line_count - 20);
        }
        eprintln!();
    } else {
        print_json_or_raw(body);
    }
}

/// Pretty-print a snapshots list.
fn print_snapshots_list(body: &str) {
    match serde_json::from_str::<serde_json::Value>(body) {
        Ok(v) => {
            let arr = v
                .as_array()
                .or_else(|| v.get("snapshots").and_then(|s| s.as_array()));
            match arr {
                Some(snaps) if snaps.is_empty() => {
                    eprintln!(
                        "  {}",
                        "No snapshots yet.  Create one: /memory snapshot [name]".dim()
                    );
                }
                Some(snaps) => {
                    eprintln!("\n  Snapshots  {}", format!("({})", snaps.len()).magenta());
                    eprintln!("  {}", "─".repeat(50).dim());
                    for snap in snaps {
                        let name = snap.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                        let ts = snap
                            .get("created_at")
                            .or_else(|| snap.get("timestamp"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        if ts.is_empty() {
                            eprintln!("  {name}");
                        } else {
                            eprintln!("  {:<34}  {}", name, ts.dim());
                        }
                    }
                    eprintln!();
                }
                None => print_json_or_raw(body),
            }
        }
        Err(_) => eprintln!("  {}", body.trim()),
    }
}

/// Pretty-print a branches list.
fn print_branches_list(body: &str) {
    match serde_json::from_str::<serde_json::Value>(body) {
        Ok(v) => {
            let arr = v
                .as_array()
                .or_else(|| v.get("branches").and_then(|b| b.as_array()));
            match arr {
                Some(branches) if branches.is_empty() => {
                    eprintln!(
                        "  {}",
                        "No branches yet.  Create one: /memory branch <name>".dim()
                    );
                }
                Some(branches) => {
                    eprintln!(
                        "\n  Branches  {}",
                        format!("({})", branches.len()).magenta()
                    );
                    eprintln!("  {}", "─".repeat(50).dim());
                    for branch in branches {
                        let name = branch.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                        let is_current = branch
                            .get("current")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        let marker = if is_current { "*" } else { " " };
                        eprintln!("  {marker} {name}");
                    }
                    eprintln!();
                }
                None => print_json_or_raw(body),
            }
        }
        Err(_) => eprintln!("  {}", body.trim()),
    }
}

/// Pretty-print a memory reflect result.
fn print_reflect_result(body: &str) {
    match serde_json::from_str::<serde_json::Value>(body) {
        Ok(v) => {
            eprintln!("\n  Memory Reflection");
            eprintln!("  {}", "─".repeat(50).dim());
            let summary = v
                .get("summary")
                .or_else(|| v.get("reflection"))
                .or_else(|| v.get("result"))
                .and_then(|v| v.as_str());
            if let Some(txt) = summary {
                for line in txt.lines() {
                    eprintln!("  {line}");
                }
                eprintln!();
            }
            if let Some(insights) = v.get("insights").and_then(|v| v.as_array()) {
                if !insights.is_empty() {
                    eprintln!("  {}", "Insights:".dim());
                    for item in insights {
                        let s = item
                            .as_str()
                            .or_else(|| item.get("text").and_then(|v| v.as_str()))
                            .unwrap_or("?");
                        eprintln!("  • {s}");
                    }
                    eprintln!();
                }
            }
            if summary.is_none()
                && v.get("insights")
                    .and_then(|v| v.as_array())
                    .map(|a| a.is_empty())
                    .unwrap_or(true)
            {
                print_json_or_raw(body);
            }
        }
        Err(_) => eprintln!("  {}", body.trim()),
    }
}

/// Pretty-print a memory health status.
fn print_health_status(body: &str) {
    for line in memory_health_lines(body) {
        // Style labels dim, keep values/icon as-is.
        if line.contains("total memories")
            || line.contains("last consolidation")
            || line.contains("quarantined")
        {
            if let Some((label, val)) = line.split_once(':') {
                eprintln!("  {}{}:{}", label.trim().dim(), ":".dim(), val);
                continue;
            }
        }
        if line.starts_with("  ─") || line == "  Memory Health" {
            eprintln!("{}", line.dim());
        } else {
            eprintln!("{line}");
        }
    }
}

/// Pretty-print a branch/snapshot diff.
fn print_memory_diff(body: &str, name: &str) {
    match serde_json::from_str::<serde_json::Value>(body) {
        Ok(v) => {
            let added = v.get("added").and_then(|v| v.as_array());
            let removed = v.get("removed").and_then(|v| v.as_array());
            let modified = v.get("modified").and_then(|v| v.as_array());
            let total = added.map(|a| a.len()).unwrap_or(0)
                + removed.map(|a| a.len()).unwrap_or(0)
                + modified.map(|a| a.len()).unwrap_or(0);
            if total == 0 {
                eprintln!("  {} No differences vs '{name}'", "⋯".dim());
                return;
            }
            eprintln!("\n  diff: {name}");
            eprintln!("  {}", "─".repeat(50).dim());
            for m in added.into_iter().flatten() {
                eprintln!("  + {}", diff_entry_preview(m));
            }
            for m in removed.into_iter().flatten() {
                eprintln!("  - {}", diff_entry_preview(m));
            }
            for m in modified.into_iter().flatten() {
                eprintln!("  ~ {}", diff_entry_preview(m));
            }
            eprintln!();
        }
        Err(_) => print_json_or_raw(body),
    }
}

fn diff_entry_preview(m: &serde_json::Value) -> String {
    m.get("content")
        .and_then(|v| v.as_str())
        .map(|c| c.lines().next().unwrap_or("").chars().take(70).collect())
        .unwrap_or_else(|| "?".to_string())
}

/// Render `/memory stats`: count memories by type.
fn render_memory_stats(arr: &[serde_json::Value]) {
    for line in memory_stats_lines(arr) {
        // Style: title/separator dim, labels dim, counts magenta.
        if line.starts_with("  ─") || line == "  Memory Stats" {
            eprintln!("{}", line.dim());
            continue;
        }
        // Lines like "  Semantic        5" or "  Total:           15" or "  Session:         3 (hidden)"
        if let Some((label, val)) = line.split_once("  ") {
            let label = label.trim();
            if !label.is_empty() && !val.is_empty() {
                eprintln!("  {:<16}  {}", label.dim(), val.magenta());
                continue;
            }
        }
        eprintln!("{line}");
    }
}

pub(crate) fn memory_health_lines(body: &str) -> Vec<String> {
    if body.trim().is_empty() {
        return vec!["  Memory health returned an empty response.".to_string()];
    }
    match serde_json::from_str::<serde_json::Value>(body) {
        Ok(v) => {
            let mut lines = vec![
                String::new(),
                "  Memory Health".to_string(),
                format!("  {}", "─".repeat(50)),
            ];
            let status = v.get("status").and_then(|v| v.as_str()).unwrap_or("ok");
            let icon = if status == "ok" || status == "healthy" {
                "✓"
            } else {
                "⚠"
            };
            lines.push(format!("  {icon} {status}"));
            if let Some(total) = v
                .get("total_memories")
                .or_else(|| v.get("total"))
                .and_then(|v| v.as_u64())
            {
                lines.push(format!("  total memories:        {total}"));
            }
            if let Some(gc) = v
                .get("last_gc")
                .or_else(|| v.get("last_consolidation"))
                .and_then(|v| v.as_str())
            {
                lines.push(format!("  last consolidation:    {gc}"));
            }
            if let Some(q) = v.get("quarantined").and_then(|v| v.as_u64()) {
                if q > 0 {
                    lines.push(format!("  quarantined:           {q}"));
                }
            }
            lines.push(String::new());
            lines
        }
        Err(_) => vec![format!("  {}", body.trim())],
    }
}

pub(crate) fn memory_stats_lines(arr: &[serde_json::Value]) -> Vec<String> {
    const TYPE_ORDER: &[(&str, &str)] = &[
        ("semantic", "Semantic"),
        ("profile", "Profile"),
        ("procedural", "Procedural"),
        ("episodic", "Episodic"),
        ("working", "Working"),
    ];
    let mut session_count = 0usize;
    let mut counts = [0usize; 5];
    for m in arr {
        let content = m.get("content").and_then(|v| v.as_str()).unwrap_or("");
        if is_session_proto(content) {
            session_count += 1;
            continue;
        }
        let mtype = m
            .get("memory_type")
            .and_then(|v| v.as_str())
            .unwrap_or("semantic");
        let idx = TYPE_ORDER
            .iter()
            .position(|(k, _)| *k == mtype)
            .unwrap_or(0);
        counts[idx] += 1;
    }
    let total: usize = counts.iter().sum();
    let mut lines = vec![
        String::new(),
        "  Memory Stats".to_string(),
        format!("  {}", "─".repeat(30)),
    ];
    for (i, (_, label)) in TYPE_ORDER.iter().enumerate() {
        if counts[i] > 0 {
            lines.push(format!("  {label:<16}  {}", counts[i]));
        }
    }
    if session_count > 0 {
        lines.push(format!("  {:<16}  {} (hidden)", "Session:", session_count));
    }
    lines.push(format!("  {}", "─".repeat(30)));
    lines.push(format!("  {:<16}  {}", "Total:", total));
    lines.push(String::new());
    lines
}

fn collect_dismiss_candidates(arr: &[serde_json::Value]) -> Vec<DismissCandidate> {
    arr.iter()
        .filter_map(|memory| {
            let content = memory.get("content").and_then(|v| v.as_str()).unwrap_or("");
            if is_session_proto(content) {
                return None;
            }
            Some(DismissCandidate {
                memory_id: memory_result_id(memory)?.to_string(),
                preview: format_memory_entry_line(memory),
            })
        })
        .collect()
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
        let result = format_session_memory_display("", None, None);
        assert!(
            result.contains("No session memory") || result.contains("not yet extracted"),
            "empty body should show a helpful message, got: {result:?}"
        );
    }

    #[test]
    fn format_session_memory_display_empty_shows_recent_failure_hint() {
        let result = strip_ansi(&format_session_memory_display(
            "",
            Some("sess-1"),
            Some("Latest extraction failed on turn 15: write_failed — upstream 500."),
        ));
        assert!(result.contains("Latest extraction failed on turn 15"));
        assert!(result.contains("sess-1"));
    }

    #[test]
    fn format_session_memory_display_shows_l0_content() {
        let body = "## L0 Critical\n- Goal: fix auth module\n";
        let result = format_session_memory_display(body, None, None);
        assert!(
            result.contains("fix auth module"),
            "should show L0 content, got: {result:?}"
        );
    }

    #[test]
    fn format_session_memory_display_shows_goals_todos_completed() {
        let body = "## Active Goals\n- Refactor memory\n\n## Pending Todos\n- Write tests\n\n## Completed\n- Scaffold done\n";
        let result = format_session_memory_display(body, None, None);
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
        let result = strip_ansi(&format_session_memory_display(body, None, None));
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

    #[test]
    fn parse_session_memory_status_hint_prefers_error_detail() {
        let journal = r#"
{"type":"session_memory_extraction","turn":7,"metadata":{"outcome":"skipped","reason":"in_flight"}}
{"type":"session_memory_extraction","turn":15,"metadata":{"outcome":"errored","reason":"write_failed","persist_detail":"memory store HTTP 500 Internal Server Error"}}
"#;
        let hint =
            parse_session_memory_status_hint_from_journal_text(journal).expect("status hint");
        assert!(hint.summary.contains("turn 15"));
        assert!(hint.summary.contains("write_failed"));
        assert!(hint.summary.contains("500 Internal Server Error"));
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
            "inspect",
            "help",
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
            "inspect",
            "dismiss",
            "help",
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
    fn session_proto_detection_covers_legacy_and_active_entries() {
        assert!(is_session_proto("[@session/memory] session_id=sess-1"));
        assert!(is_session_proto("[@session/active] Active session state"));
        assert!(!is_session_proto("[@fact/semantic] user prefers rust"));
    }

    #[test]
    fn memory_result_id_accepts_id_and_memory_id_shapes() {
        assert_eq!(
            memory_result_id(&serde_json::json!({"id": "mem-1"})),
            Some("mem-1")
        );
        assert_eq!(
            memory_result_id(&serde_json::json!({"memory_id": "mem-2"})),
            Some("mem-2")
        );
        assert_eq!(memory_result_id(&serde_json::json!({"id": ""})), None);
    }

    #[test]
    fn collect_dismiss_candidates_skips_session_entries_and_keeps_search_ids() {
        let arr = vec![
            serde_json::json!({
                "id": "mem-1",
                "memory_type": "semantic",
                "content": "Remember the auth cleanup plan"
            }),
            serde_json::json!({
                "memory_id": "mem-2",
                "memory_type": "working",
                "content": "[@session/active] session state"
            }),
        ];
        let candidates = collect_dismiss_candidates(&arr);
        assert_eq!(
            candidates,
            vec![DismissCandidate {
                memory_id: "mem-1".to_string(),
                preview: "[semantic] Remember the auth cleanup plan".to_string(),
            }]
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
