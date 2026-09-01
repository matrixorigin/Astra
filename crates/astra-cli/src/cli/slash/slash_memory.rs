use crate::cli::surface::health_status_surface::health_status_icon;
use crate::cli::{
    cli_config::cli_utils::{prefix_chars, print_json_or_raw},
    session::session_runtime,
    session::session_state::SessionState,
    theme,
};
use astra_runtime::prompts;
use astra_services::session_artifact_store::SessionArtifactStore;
use crossterm::style::Stylize;

pub(crate) const MEMORY_BROWSE_QUERY: &str = "memory knowledge fact preference plan task note";
pub(crate) const MEMORY_BROWSE_TOP_K: usize = 50;
pub(crate) const MEMORY_STATS_TOP_K: usize = 200;
const MEMORY_DISMISS_TOP_K: usize = 3;
const SESSION_MEMORY_STATUS_TAIL_LIMIT: usize = 128;

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

/// When the primary `SECTION_DISPLAY_NAMES` yield at least one populated
/// section, these are skipped — fallback sections only appear when the body
/// has *no* primary-section content at all. This keeps the compact TUI view
/// focused on actionable state while still showing context when that's the
/// only available information.
const FALLBACK_SECTION_DISPLAY_NAMES: &[(&str, &str)] = &[
    ("Task Specification", "🧭 Task Specification"),
    ("Current State", "📍 Current State"),
    ("Workflow", "🛠 Workflow"),
    ("Files and Functions", "📂 Files & Functions"),
    ("Errors & Corrections", "⚠ Errors & Corrections"),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionMemoryRecord {
    pub(crate) memory_id: String,
    pub(crate) summary: Option<String>,
    pub(crate) body: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionMemoryStatusHint {
    pub(crate) summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionMemorySurfaceStatus {
    pub(crate) snapshot: String,
    pub(crate) snapshot_provenance: Option<String>,
    pub(crate) extraction: Option<String>,
    pub(crate) prompt_injection: Option<String>,
    pub(crate) repository_prompt_memories: Option<String>,
    pub(crate) user_preferences: Option<String>,
    pub(crate) remote_sync: Option<String>,
    pub(crate) last_local_refresh_at: Option<String>,
    pub(crate) stable_memory_epoch: Option<u32>,
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
        return Err("usage: /memory forget <memory_id> [--reason TEXT]".to_string());
    }
    let reason = parts
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| "user requested /memory forget".to_string());
    Ok((memory_id, reason))
}

fn confirmed_memory_purge_notice(body: &str, memory_id: &str) -> Result<String, String> {
    let receipt: serde_json::Value = serde_json::from_str(body)
        .map_err(|error| format!("Purge returned an invalid receipt: {error}"))?;
    let status = receipt
        .get("status")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    let deleted = receipt
        .get("deleted_count")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let unresolved = receipt
        .get("unresolved_count")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(1);
    if status == "completed" && deleted == 1 && unresolved == 0 {
        return Ok(format!("Forgot memory {memory_id}"));
    }
    let message = receipt
        .get("message")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("the backend did not confirm this deletion");
    Err(format!("Memory was not confirmed deleted: {message}"))
}

pub(crate) async fn handle_memory_domain_command(
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
                "session" => {
                    let Some(session_id) = state.session_id.as_deref() else {
                        eprintln!("  {}", "No active session yet.".yellow());
                        return Ok(());
                    };
                    match load_current_session_memory(api, tok, session_id).await {
                        Ok(record) => {
                            let body = record
                                .as_ref()
                                .map(|memory| memory.body.as_str())
                                .unwrap_or_default();
                            let summary =
                                record.as_ref().and_then(|memory| memory.summary.as_deref());
                            let hint = latest_session_memory_status_hint(session_id);
                            let status = session_memory_surface_status(session_id, record.as_ref());
                            let out = format_session_memory_response(
                                summary,
                                body,
                                Some(session_id),
                                hint.as_ref().map(|h| h.summary.as_str()),
                                Some(&status),
                            );
                            stdout_println!("{out}");
                        }
                        Err(error) => {
                            eprintln!("  {} {}", theme::icon_err(), error.red());
                        }
                    }
                }
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
                    // If arg looks like a memory ID, dismiss directly.
                    // Otherwise search and confirm interactively.
                    if sub_arg.contains("mem-")
                        || (sub_arg.len() < 64 && sub_arg.contains('-') && !sub_arg.contains(' '))
                    {
                        let memory_id = sub_arg.trim().to_string();
                        eprintln!(
                            "  {} Dismissing {}",
                            "⋯".dim(),
                            prefix_chars(&memory_id, 8).dim()
                        );
                        match crate::edge_tools::memoria::memoria_feedback(
                            &memory_id,
                            "irrelevant",
                            Some("user /memory dismiss"),
                        )
                        .await
                        {
                            Ok(_) => {
                                eprintln!(
                                    "  {} dismissed: {}",
                                    theme::icon_ok(),
                                    prefix_chars(&memory_id, 8).dim()
                                );
                            }
                            Err(error) => {
                                eprintln!(
                                    "  {} failed to dismiss {}: {error}",
                                    theme::icon_err(),
                                    prefix_chars(&memory_id, 8).dim()
                                );
                            }
                        }
                        return Ok(());
                    }
                    // Search-based dismiss with interactive confirmation
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
                                        match crate::edge_tools::memoria::memoria_feedback(
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
                // ─── Show one memory by id ──────────────────────
                "show" if !sub_arg.is_empty() => {
                    let memory_id = sub_arg.trim();
                    match crate::edge_tools::memoria::memoria_show(memory_id).await {
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
                    match crate::edge_tools::memoria::memoria_purge(&body).await {
                        Ok(receipt) => match confirmed_memory_purge_notice(&receipt, &memory_id) {
                            Ok(message) => {
                                eprintln!("  {} {}", theme::icon_ok(), message.magenta());
                            }
                            Err(message) => eprintln!("  {}", message.yellow()),
                        },
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
                    match crate::edge_tools::memoria::memoria_snapshot_create(&name).await {
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
                    match crate::edge_tools::memoria::memoria_snapshot_rollback(sub_arg).await {
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
                "snapshots" => match crate::edge_tools::memoria::memoria_snapshots_list().await {
                    Ok(body) => print_snapshots_list(&body),
                    Err(e) => eprintln!("  {} {e}", theme::icon_err()),
                },
                // ─── Cloud: Branches ──────────────────────────────
                "branch" if !sub_arg.is_empty() => {
                    match crate::edge_tools::memoria::memoria_branch_create(sub_arg).await {
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
                    match crate::edge_tools::memoria::memoria_branch_checkout(sub_arg).await {
                        Ok(_) => eprintln!(
                            "  {} Switched to branch '{}'",
                            theme::icon_ok(),
                            sub_arg.magenta()
                        ),
                        Err(e) => eprintln!("  {} Checkout failed: {e}", theme::icon_err()),
                    }
                }
                "merge" if !sub_arg.is_empty() => {
                    match crate::edge_tools::memoria::memoria_branch_merge(sub_arg).await {
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
                    match crate::edge_tools::memoria::memoria_branch_diff(sub_arg).await {
                        Ok(body) => print_memory_diff(&body, sub_arg),
                        Err(branch_err) => {
                            match crate::edge_tools::memoria::memoria_snapshot_diff(sub_arg).await {
                                Ok(body) => print_memory_diff(&body, sub_arg),
                                Err(_) => eprintln!(
                                    "  {} diff failed (branch: {branch_err})",
                                    theme::icon_err()
                                ),
                            }
                        }
                    }
                }
                "branches" => match crate::edge_tools::memoria::memoria_branches_list().await {
                    Ok(body) => print_branches_list(&body),
                    Err(e) => eprintln!("  {} {e}", theme::icon_err()),
                },
                // ─── Cloud: Analysis ─────────────────────────────
                "reflect" => {
                    eprintln!("  {} Analyzing memory patterns...", "⋯".dim());
                    match crate::edge_tools::memoria::memoria_reflect().await {
                        Ok(body) => print_reflect_result(&body),
                        Err(e) => eprintln!("  {} {e}", theme::icon_err()),
                    }
                }
                "health" => match crate::edge_tools::memoria::memoria_health().await {
                    Ok(body) => print_health_status(&body),
                    Err(e) => eprintln!("  {} {e}", theme::icon_err()),
                },
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

                "stats" => {
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
    eprintln!("  {}", "Mental model".dim());
    eprintln!(
        "  {}",
        "    session      Current conversation state; auto-extracted and short-lived".dim()
    );
    eprintln!(
        "  {}",
        "    repository   Durable cross-session memories shown by list/search".dim()
    );
    eprintln!(
        "  {}",
        "    retrieved    Memories selected for the current prompt; inspect in /context".dim()
    );
    eprintln!();
    eprintln!("  {}", "View & Search".dim());
    eprintln!(
        "  {}",
        "    list                  List memories grouped by type".dim()
    );
    eprintln!("  {}", "    search <query>        Search by content".dim());
    eprintln!(
        "  {}",
        "    show <id>             Inspect one memory in detail".dim()
    );
    eprintln!(
        "  {}",
        "    stats                 Count memories by type".dim()
    );
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
    eprintln!("  {}", "Clean up".dim());
    eprintln!(
        "  {}",
        "    forget <id> [--reason] Permanently delete a memory".dim()
    );
    eprintln!(
        "  {}",
        "    dismiss <id|query>    Mark memories as irrelevant".dim()
    );
    eprintln!();
    eprintln!("  {}", "Snapshots".dim());
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
    for sections in [SECTION_DISPLAY_NAMES, FALLBACK_SECTION_DISPLAY_NAMES] {
        if sections_shown > 0 {
            break;
        }
        for (section_name, label) in sections {
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

pub(crate) fn format_session_memory_response(
    summary: Option<&str>,
    body: &str,
    session_id: Option<&str>,
    status_hint: Option<&str>,
    surface_status: Option<&SessionMemorySurfaceStatus>,
) -> String {
    let mut out = String::new();
    let headline = summary
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| session_memory_headline_from_body(body))
        .unwrap_or_else(|| "Session memory".to_string());
    out.push_str(&headline);

    if let Some(sid) = session_id.filter(|sid| !sid.trim().is_empty()) {
        out.push_str(&format!("\nsession: {sid}"));
    }

    if let Some(status) = surface_status {
        let block = render_session_memory_surface_status(status);
        if !block.is_empty() {
            out.push_str("\n\n");
            out.push_str(&block);
        }
    }

    if body.trim().is_empty() {
        if let Some(hint) = status_hint.filter(|hint| !hint.trim().is_empty()) {
            out.push_str(&format!("\n\n{hint}"));
        } else {
            out.push_str("\n\nNo session memory yet.");
        }
        return out;
    }

    out.push('\n');
    append_section_block(&mut out, body, SECTION_DISPLAY_NAMES, 8);
    if !has_any_section(body, SECTION_DISPLAY_NAMES) {
        append_section_block(&mut out, body, FALLBACK_SECTION_DISPLAY_NAMES, 8);
    }
    out
}

pub(crate) fn render_session_memory_surface_status(status: &SessionMemorySurfaceStatus) -> String {
    let mut lines = vec![
        "Memory Status".to_string(),
        format!("- Current Session Snapshot: {}", status.snapshot),
    ];
    if let Some(provenance) = status.snapshot_provenance.as_deref() {
        lines.push(format!("- Snapshot Provenance: {provenance}"));
    }
    if let Some(ts) = status.last_local_refresh_at.as_deref() {
        lines.push(format!("- Last Local Refresh: {ts}"));
    }
    if let Some(extraction) = status.extraction.as_deref() {
        lines.push(format!("- Extraction Status: {extraction}"));
    }
    if let Some(injection) = status.prompt_injection.as_deref() {
        lines.push(format!("- Current Session Snapshot Injection: {injection}"));
    }
    if let Some(repo) = status.repository_prompt_memories.as_deref() {
        lines.push(format!("- Repository Memory Injection: {repo}"));
    }
    if let Some(preferences) = status.user_preferences.as_deref() {
        lines.push(format!("- User Preferences: {preferences}"));
    }
    if let Some(sync) = status.remote_sync.as_deref() {
        lines.push(format!("- Remote Sync: {sync}"));
    }
    if let Some(epoch) = status.stable_memory_epoch {
        lines.push(format!("- Stable Memory Epoch: {epoch}"));
    }
    lines.join("\n")
}

pub(crate) fn session_memory_surface_status(
    session_id: &str,
    record: Option<&SessionMemoryRecord>,
) -> SessionMemorySurfaceStatus {
    let metadata =
        astra_runtime::session_memory::runner::load_local_session_memory_metadata(session_id);
    let snapshot = match record {
        Some(record) if record.memory_id == "local-session-memory" => {
            "local current-session artifact".to_string()
        }
        Some(_) => "remote Memoria session memory".to_string(),
        None => metadata
            .as_ref()
            .and_then(|meta| meta.current_snapshot_source.as_deref())
            .map(|source| format!("not loaded (last writer: {source})"))
            .unwrap_or_else(|| "not available".to_string()),
    };
    let extraction = latest_session_memory_status_hint(session_id).map(|hint| hint.summary);
    let snapshot_provenance = metadata.as_ref().and_then(render_snapshot_provenance);
    let prompt_trace = latest_prompt_memory_trace(session_id);
    let prompt_injection = prompt_trace.as_ref().map(|trace| {
        let status = if trace.session_memory_present {
            "present"
        } else {
            "absent"
        };
        format!(
            "{status} on turn {}; {} tokens",
            trace.turn, trace.session_memory_tokens
        )
    });
    let repository_prompt_memories = prompt_trace.as_ref().map(|trace| {
        format!(
            "{} repository {} on turn {}",
            trace.repository_memory_count,
            if trace.repository_memory_count == 1 {
                "memory"
            } else {
                "memories"
            },
            trace.turn
        )
    });
    let user_preferences = prompt_trace.as_ref().and_then(|trace| {
        if trace.user_preferences_tokens == 0 {
            None
        } else {
            Some(format!(
                "{} prompt tokens on turn {}",
                trace.user_preferences_tokens, trace.turn
            ))
        }
    });
    let remote_sync = metadata
        .as_ref()
        .and_then(render_remote_sync_status)
        .or_else(|| {
            record
                .filter(|record| record.memory_id != "local-session-memory")
                .map(|_| "using remote Memoria snapshot".to_string())
        });
    SessionMemorySurfaceStatus {
        snapshot,
        snapshot_provenance,
        extraction,
        prompt_injection,
        repository_prompt_memories,
        user_preferences,
        remote_sync,
        last_local_refresh_at: metadata
            .as_ref()
            .and_then(|meta| meta.last_local_refresh_at.clone()),
        stable_memory_epoch: metadata
            .as_ref()
            .map(|meta| meta.stable_memory_epoch)
            .filter(|epoch| *epoch > 0),
    }
}

fn render_snapshot_provenance(
    metadata: &astra_runtime::session_memory::runner::SessionMemoryArtifactMetadata,
) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(turn) = metadata.last_extracted_turn {
        parts.push(format!("turn {turn}"));
    }
    if let Some(source) = metadata
        .last_extraction_source
        .as_deref()
        .filter(|source| !source.trim().is_empty())
    {
        parts.push(format!("source {}", source.replace('_', " ")));
    }
    if let Some(writer) = metadata
        .current_snapshot_source
        .as_deref()
        .filter(|writer| !writer.trim().is_empty())
    {
        parts.push(format!("writer {}", writer.replace('_', " ")));
    }
    if let Some(model) = metadata
        .last_selector_model
        .as_deref()
        .filter(|model| !model.trim().is_empty())
    {
        parts.push(format!("selector {model}"));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    }
}

fn render_remote_sync_status(
    metadata: &astra_runtime::session_memory::runner::SessionMemoryArtifactMetadata,
) -> Option<String> {
    let status = match metadata.last_remote_sync_status.as_deref()? {
        "memoria_pending" => "pending remote Memoria sync",
        "memoria_synced" => "Memoria sync complete",
        "memoria_failed" => "Memoria sync failed",
        other => other,
    };
    let when = metadata
        .last_remote_sync_at
        .as_deref()
        .map(|ts| format!(" at {ts}"))
        .unwrap_or_default();
    let detail = metadata
        .last_remote_sync_detail
        .as_deref()
        .filter(|detail| !detail.trim().is_empty())
        .map(|detail| format!(" ({detail})"))
        .unwrap_or_default();
    Some(format!("{status}{when}{detail}"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PromptMemoryTraceStatus {
    turn: u32,
    session_memory_present: bool,
    session_memory_tokens: u64,
    repository_memory_count: usize,
    user_preferences_tokens: u64,
}

fn latest_prompt_memory_trace(session_id: &str) -> Option<PromptMemoryTraceStatus> {
    let events = astra_services::session_journal::read_journal_tail(
        session_id,
        SESSION_MEMORY_STATUS_TAIL_LIMIT,
    )
    .ok()?;
    events.iter().rev().find_map(|event| {
        if event.event_type
            != astra_services::session_journal::JournalEventType::ContextAssemblyRecorded
        {
            return None;
        }
        let trace = event.context_assembly_trace.as_ref()?;
        let system_prompt = trace.get("system_prompt")?;
        let session_memory = system_prompt.get("session_memory_injected");
        let repository_memory_count = system_prompt
            .get("repository_memories")
            .and_then(serde_json::Value::as_array)
            .map(|entries| entries.len())
            .unwrap_or(0);
        let user_preferences_tokens = system_prompt
            .get("user_preferences_tokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        Some(PromptMemoryTraceStatus {
            turn: event.turn.unwrap_or(0),
            session_memory_present: session_memory.is_some_and(|value| !value.is_null()),
            session_memory_tokens: session_memory
                .and_then(|value| value.get("tokens"))
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
            repository_memory_count,
            user_preferences_tokens,
        })
    })
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
            let summary = prompts::memory_proto::MemoryEntry::parse(&memory.content)
                .map(|entry| entry.compact_view().trim().to_string())
                .filter(|summary| !summary.is_empty());
            astra_runtime::session_memory::runner::decode_session_memory_entry(
                &memory.content,
                session_id,
            )
            .map(|body| SessionMemoryRecord {
                memory_id: memory.memory_id,
                summary,
                body,
            })
        })
}

fn parse_session_memory_status_hint_from_journal_text(
    journal_text: &str,
) -> Option<SessionMemoryStatusHint> {
    let mut latest: Option<SessionMemoryStatusHint> = None;

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
            "extracted" => {
                let source = metadata
                    .get("source")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("background");
                latest = Some(SessionMemoryStatusHint {
                    summary: format!(
                        "Latest extraction completed on {turn} via {}. If this view is still empty, the snapshot may still be loading.",
                        humanize_session_memory_source(source)
                    ),
                });
            }
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
                latest = Some(SessionMemoryStatusHint {
                    summary: humanize_session_memory_error(turn.as_str(), reason, detail),
                });
            }
            "skipped" => {
                let reason = metadata
                    .get("reason")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown_skip");
                latest = Some(SessionMemoryStatusHint {
                    summary: humanize_session_memory_skip(turn.as_str(), reason),
                });
            }
            _ => {}
        }
    }

    latest
}

fn humanize_session_memory_source(source: &str) -> &'static str {
    match source {
        "llm" => "the background extractor",
        "rule_fallback" => "the fallback extractor",
        _ => "the extractor",
    }
}

fn humanize_session_memory_error(turn: &str, reason: &str, detail: &str) -> String {
    let reason_text = match reason {
        "llm_timeout" => "timed out while generating session memory",
        "llm_error" => "hit a model error while generating session memory",
        "empty_response" => "returned an empty session-memory update",
        "purge_failed" => "could not replace the previous session-memory snapshot",
        "write_failed" => "could not store the new session-memory snapshot",
        _ => "failed while updating session memory",
    };
    if detail.is_empty() {
        format!("Latest extraction on {turn} {reason_text}.")
    } else {
        format!("Latest extraction on {turn} {reason_text}: {detail}.")
    }
}

fn humanize_session_memory_skip(turn: &str, reason: &str) -> String {
    match reason {
        "in_flight" => format!(
            "Session memory extraction was already running before {turn}, so this turn did not start a duplicate run."
        ),
        "no_growth" => format!(
            "Session memory was not refreshed on {turn} because the conversation had not changed enough since the last snapshot."
        ),
        "already_current" => format!(
            "Session memory already contains a durable snapshot through {turn}, so a duplicate extraction was not started."
        ),
        "selector_cooldown" => format!(
            "Session memory was not refreshed on {turn} because the extractor model is cooling down after a recent failure."
        ),
        "memoria_unhealthy" => format!(
            "Session memory was not refreshed on {turn} because the memory backend is temporarily unhealthy."
        ),
        "no_session_id" => {
            format!("Session memory could not start on {turn} because no session id was available.")
        }
        _ => format!("Session memory was skipped on {turn}."),
    }
}

pub(crate) fn latest_session_memory_status_hint(
    session_id: &str,
) -> Option<SessionMemoryStatusHint> {
    let events = astra_services::session_journal::read_journal_tail(
        session_id,
        SESSION_MEMORY_STATUS_TAIL_LIMIT,
    )
    .ok()?;
    let journal = events
        .into_iter()
        .filter_map(|event| serde_json::to_string(&event).ok())
        .collect::<Vec<_>>()
        .join("\n");
    parse_session_memory_status_hint_from_journal_text(&journal)
}

pub(crate) async fn load_current_session_memory(
    api: &astra_thin_client::ThinClient,
    token: &str,
    session_id: &str,
) -> Result<Option<SessionMemoryRecord>, String> {
    if let Some(record) = load_local_session_memory(session_id) {
        return Ok(Some(record));
    }

    match load_remote_session_memory(api, token, session_id).await {
        Ok(record) => Ok(record.or_else(|| load_local_session_memory(session_id))),
        Err(error) => load_local_session_memory(session_id).map(Some).ok_or(error),
    }
}

/// Load only the remote session snapshot. Callers that can use a local
/// artifact without authentication should check it before entering this
/// network path; the CLI wrapper above still reconciles a concurrent local
/// write after a remote attempt.
pub(crate) async fn load_remote_session_memory(
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
    let response = match api
        .post_memory_retrieve_json(token, &fallback_payload)
        .await
    {
        Ok(response) => response,
        Err(error) => return Err(format!("memory retrieve failed: {error}")),
    };
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

pub(crate) fn load_local_session_memory(session_id: &str) -> Option<SessionMemoryRecord> {
    let path = astra_services::local_session_artifact_store()
        .session_path(session_id, "session-memory.md")
        .ok()?;
    let body = std::fs::read_to_string(path).ok()?;
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(SessionMemoryRecord {
        memory_id: "local-session-memory".to_string(),
        summary: session_memory_headline_from_body(trimmed),
        body: trimmed.to_string(),
    })
}

fn session_memory_headline_from_body(body: &str) -> Option<String> {
    [
        "Current State",
        "Active Goals",
        "Task Specification",
        "Session Title",
    ]
    .into_iter()
    .find_map(|section| {
        extract_md_section(body, section).and_then(|content| {
            content
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .map(str::to_string)
        })
    })
}

fn has_any_section(body: &str, sections: &[(&str, &str)]) -> bool {
    sections.iter().any(|(section_name, _)| {
        extract_md_section(body, section_name)
            .map(|content| !content.trim().is_empty())
            .unwrap_or(false)
    })
}

fn append_section_block(
    out: &mut String,
    body: &str,
    sections: &[(&str, &str)],
    per_section_limit: usize,
) {
    for (section_name, label) in sections {
        if let Some(content) = extract_md_section(body, section_name) {
            let trimmed = content.trim();
            if trimmed.is_empty() {
                continue;
            }
            out.push_str(&format!("\n\n{label}"));
            let lines: Vec<&str> = trimmed.lines().collect();
            for line in lines.iter().take(per_section_limit) {
                out.push_str(&format!("\n{line}"));
            }
            if lines.len() > per_section_limit {
                out.push_str(&format!(
                    "\n… {} more lines",
                    lines.len() - per_section_limit
                ));
            }
        }
    }
}

pub(crate) async fn load_current_session_memory_body_with_profile(
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
    let canonical_body =
        astra_runtime::session_memory::runner::canonicalize_session_memory_markdown(
            session_id,
            body,
            0,
            &astra_turn_types::session_facts::SessionFacts::default(),
        );
    astra_runtime::session_memory::runner::persist_local_session_memory_artifact(
        session_id,
        &canonical_body,
    )
    .map_err(|error| format!("current session memory write failed: {error}"))?;
    update_manual_session_memory_sync_metadata(session_id, "memoria_pending", None)
        .map_err(|error| format!("current session memory metadata write failed: {error}"))?;
    let encoded = astra_runtime::session_memory::runner::encode_session_memory_entry(
        session_id,
        &canonical_body,
    );
    let remote_memory_id = memory_id.filter(|memory_id| *memory_id != "local-session-memory");
    if let Some(memory_id) = remote_memory_id {
        let path = format!("/memory/{memory_id}/correct");
        let payload = serde_json::json!({
            "new_content": encoded,
            "reason": "manual /memory edit",
        });
        api.put_bearer_path_json_text(token, &path, &payload)
            .await
            .map_err(|error| {
                if let Err(write_error) = update_manual_session_memory_sync_metadata(
                    session_id,
                    "memoria_failed",
                    Some(&error.to_string()),
                ) {
                    tracing::warn!(
                        session_id = %session_id,
                        error = %write_error,
                        "failed to persist manual session-memory sync failure metadata"
                    );
                }
                format!("current session memory updated locally but cloud sync failed: {error}")
            })?;
        update_manual_session_memory_sync_metadata(session_id, "memoria_synced", None)?;
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
        .map_err(|error| {
            if let Err(write_error) = update_manual_session_memory_sync_metadata(
                session_id,
                "memoria_failed",
                Some(&error.to_string()),
            ) {
                tracing::warn!(
                    session_id = %session_id,
                    error = %write_error,
                    "failed to persist manual session-memory sync failure metadata"
                );
            }
            format!("current session memory updated locally but cloud sync failed: {error}")
        })?;
    if response.status().is_success() {
        update_manual_session_memory_sync_metadata(session_id, "memoria_synced", None)?;
        Ok(())
    } else {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if let Err(write_error) = update_manual_session_memory_sync_metadata(
            session_id,
            "memoria_failed",
            Some(body.trim()),
        ) {
            tracing::warn!(
                session_id = %session_id,
                error = %write_error,
                "failed to persist manual session-memory sync failure metadata"
            );
        }
        Err(format!(
            "current session memory updated locally but cloud sync failed ({status}): {}",
            body.trim()
        ))
    }
}

fn update_manual_session_memory_sync_metadata(
    session_id: &str,
    status: &str,
    detail: Option<&str>,
) -> Result<(), String> {
    let mut metadata =
        astra_runtime::session_memory::runner::load_local_session_memory_metadata(session_id)
            .unwrap_or_default();
    metadata.session_id = session_id.to_string();
    if metadata.current_snapshot_source.as_deref() != Some("manual_edit") {
        metadata.current_snapshot_source = Some("manual_edit".to_string());
    }
    if metadata.last_extraction_source.as_deref() != Some("manual_edit") {
        metadata.last_extraction_source = Some("manual_edit".to_string());
    }
    metadata.last_remote_sync_status = Some(status.to_string());
    metadata.last_remote_sync_at = Some(chrono::Utc::now().to_rfc3339());
    metadata.last_remote_sync_detail = detail
        .map(str::trim)
        .filter(|detail| !detail.is_empty())
        .map(str::to_string);
    astra_runtime::session_memory::runner::persist_local_session_memory_metadata(
        session_id, &metadata,
    )
}

/// Extract a `## SectionName` block from a markdown string.
/// Returns content between the header and the next `##` header (exclusive).
fn extract_md_section(md: &str, section_name: &str) -> Option<String> {
    let (_, content_start, section_end) = find_md_section_bounds(md, section_name)?;
    sanitize_md_section_content(&md[content_start..section_end])
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

fn sanitize_md_section_content(content: &str) -> Option<String> {
    let section_body = content.trim();
    if section_body.is_empty()
        || (section_body.starts_with("<!--")
            && section_body.ends_with("-->")
            && section_body.matches("-->").count() == 1)
    {
        return None;
    }
    let stripped = if section_body.starts_with("<!--") {
        match section_body.find("-->") {
            Some(after_comment) => section_body[after_comment + 3..].trim(),
            None => section_body,
        }
    } else {
        section_body
    };
    (!stripped.is_empty()).then(|| stripped.to_string())
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
            let icon = health_status_icon(status);
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
    use super::{
        DismissCandidate, SECTION_NAMES, SessionMemorySurfaceStatus, collect_dismiss_candidates,
        confirmed_memory_purge_notice, extract_md_section, format_session_memory_display,
        format_session_memory_response, is_session_proto, load_current_session_memory,
        load_local_session_memory, memory_health_lines, memory_result_id, parse_memory_forget_args,
        parse_session_memory_status_hint_from_journal_text, replace_md_section,
        sanitize_md_section_content, select_session_memory_record,
        session_memory_headline_from_body, store_current_session_memory,
    };
    use astra_services::SessionArtifactStore;
    use regex::Regex;

    fn strip_ansi(input: &str) -> String {
        Regex::new(r"\x1b\[[0-9;]*m")
            .unwrap()
            .replace_all(input, "")
            .into_owned()
    }

    #[test]
    fn memory_health_lines_uses_shared_health_icon_semantics() {
        let lines = memory_health_lines(r#"{"status":"healthy","total_memories":3}"#);
        let rendered = lines.join("\n");
        assert!(rendered.contains("✓ healthy"), "{rendered}");

        let degraded = memory_health_lines(r#"{"status":"degraded","total_memories":3}"#);
        let rendered = degraded.join("\n");
        assert!(rendered.contains("⚠ degraded"), "{rendered}");
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
    fn format_session_memory_display_hides_template_comment_sections() {
        let body = "## Active Goals\n<!-- Current goals explicitly stated by the user or assistant. Do NOT invent goals. -->\n\n## Pending Todos\n- real todo\n";
        let result = strip_ansi(&format_session_memory_display(body, None, None));
        assert!(!result.contains("Current goals explicitly stated"));
        assert!(!result.contains("🎯 Active Goals"));
        assert!(result.contains("📌 Pending Todos"));
        assert!(result.contains("real todo"));
    }

    #[test]
    fn format_session_memory_display_preserves_explicit_completion_goal() {
        let body =
            "## Active Goals\n- None remaining; task completed.\n\n## Completed\n- Finished work\n";
        let result = strip_ansi(&format_session_memory_display(body, None, None));
        assert!(result.contains("🎯 Active Goals"));
        assert!(result.contains("None remaining; task completed."));
        assert!(result.contains("✅ Completed"));
        assert!(result.contains("Finished work"));
    }

    #[test]
    fn format_session_memory_display_falls_back_to_context_sections() {
        let body = "## Task Specification\nClean up /memory subcommands\n\n## Current State\nSession memory extracted after a long /memory cleanup turn.\n\n## Workflow\n- Reviewed slash_memory routing\n- Reworked help text\n";
        let result = strip_ansi(&format_session_memory_display(body, None, None));
        assert!(result.contains("🧭 Task Specification"));
        assert!(result.contains("Clean up /memory subcommands"));
        assert!(result.contains("📍 Current State"));
        assert!(result.contains("🛠 Workflow"));
    }

    #[test]
    fn format_session_memory_response_uses_summary_first() {
        let body =
            "## Active Goals\n- Ship the fix\n\n## Completed\n- Root-caused the regression\n";
        let result =
            format_session_memory_response(Some("Task complete"), body, Some("sess-1"), None, None);
        assert!(result.starts_with("Task complete"));
        assert!(result.contains("session: sess-1"));
        assert!(result.contains("🎯 Active Goals"));
        assert!(result.contains("✅ Completed"));
    }

    #[test]
    fn session_memory_headline_preserves_current_state_without_text_classification() {
        let body = "## Session Title\nReview uncommitted changes in the session memory feature.\n\n## Current State\nThe user's request is complete. No issues remain. The session is idle.\n\n## Completed\n- Ran tests\n";
        let headline = session_memory_headline_from_body(body).expect("headline");
        assert_eq!(
            headline,
            "The user's request is complete. No issues remain. The session is idle."
        );
    }

    #[test]
    fn sanitize_current_state_preserves_nonempty_text() {
        let result = sanitize_md_section_content(
            "The user's request is complete. No issues remain. The session is idle.",
        );
        assert_eq!(
            result.as_deref(),
            Some("The user's request is complete. No issues remain. The session is idle.")
        );
    }

    #[test]
    fn format_session_memory_response_uses_hint_when_body_empty() {
        let result = format_session_memory_response(
            None,
            "",
            Some("sess-1"),
            Some("Latest extraction on turn 3 could not store the new session-memory snapshot."),
            None,
        );
        assert!(result.contains("Session memory"));
        assert!(result.contains("session: sess-1"));
        assert!(!result.contains("No session memory extracted yet."));
        assert!(result.contains("could not store the new session-memory snapshot"));
    }

    #[test]
    fn format_session_memory_response_renders_surface_status_block() {
        let status = SessionMemorySurfaceStatus {
            snapshot: "local current-session artifact".to_string(),
            snapshot_provenance: Some(
                "turn 9 · source llm · writer background extraction · selector mini-judge"
                    .to_string(),
            ),
            extraction: Some(
                "Latest extraction completed on turn 9 via the background extractor.".to_string(),
            ),
            prompt_injection: Some("present on turn 10; 27 tokens".to_string()),
            repository_prompt_memories: Some("2 repository memories on turn 10".to_string()),
            user_preferences: Some("200 prompt tokens on turn 10".to_string()),
            remote_sync: Some("Memoria sync complete at 2026-05-25T10:00:00+08:00".to_string()),
            last_local_refresh_at: Some("2026-05-25T10:00:00+08:00".to_string()),
            stable_memory_epoch: Some(1),
        };
        let result = format_session_memory_response(
            Some("Task complete"),
            "## Active Goals\n- Ship it\n",
            Some("sess-1"),
            None,
            Some(&status),
        );
        assert!(result.contains("Memory Status"));
        assert!(result.contains("Current Session Snapshot: local current-session artifact"));
        assert!(
            result.contains(
                "Snapshot Provenance: turn 9 · source llm · writer background extraction · selector mini-judge"
            )
        );
        assert!(
            result.contains("Current Session Snapshot Injection: present on turn 10; 27 tokens")
        );
        assert!(result.contains("Repository Memory Injection: 2 repository memories on turn 10"));
        assert!(result.contains("User Preferences: 200 prompt tokens on turn 10"));
        assert!(result.contains("Stable Memory Epoch: 1"));
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
        assert!(
            record
                .summary
                .as_deref()
                .is_some_and(|summary| summary.contains("Fix memory"))
        );
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
        assert!(
            hint.summary
                .contains("could not store the new session-memory snapshot")
        );
        assert!(hint.summary.contains("500 Internal Server Error"));
    }

    #[test]
    fn parse_session_memory_status_hint_humanizes_in_flight_skip() {
        let journal = r#"
{"type":"session_memory_extraction","turn":8,"metadata":{"outcome":"skipped","reason":"in_flight"}}
"#;
        let hint =
            parse_session_memory_status_hint_from_journal_text(journal).expect("status hint");
        assert!(hint.summary.contains("already running before turn 8"));
        assert!(hint.summary.contains("did not start a duplicate run"));
        assert!(!hint.summary.contains("in_flight"));
    }

    #[test]
    fn parse_session_memory_status_hint_uses_latest_success_over_earlier_skip() {
        let journal = r#"
{"type":"session_memory_extraction","turn":8,"metadata":{"outcome":"skipped","reason":"in_flight"}}
{"type":"session_memory_extraction","turn":8,"metadata":{"outcome":"extracted","source":"llm","bytes_written":1769}}
"#;
        let hint =
            parse_session_memory_status_hint_from_journal_text(journal).expect("status hint");
        assert!(
            hint.summary
                .contains("Latest extraction completed on turn 8")
        );
        assert!(hint.summary.contains("background extractor"));
        assert!(!hint.summary.contains("duplicate run"));
    }

    #[test]
    fn load_local_session_memory_reads_session_memory_md() {
        let session_id = format!(
            "sess-local-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        );
        let path = astra_services::local_session_artifact_store()
            .session_path(&session_id, "session-memory.md")
            .expect("session path");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create session dir");
        }
        std::fs::write(
            &path,
            "# Session Memory\n\n## Current State\nSummarized locally before remote persistence.\n",
        )
        .expect("write session memory");

        let record = load_local_session_memory(&session_id).expect("local session memory");
        assert_eq!(record.memory_id, "local-session-memory");
        assert!(record.summary.as_deref().is_some_and(|summary| {
            summary.contains("Summarized locally before remote persistence.")
        }));
        assert!(record.body.contains("# Session Memory"));

        std::fs::remove_file(&path).expect("remove session memory");
    }

    #[tokio::test]
    async fn load_current_session_memory_prefers_local_artifact() {
        let session_id = format!(
            "sess-local-first-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        );
        let path = astra_services::local_session_artifact_store()
            .session_path(&session_id, "session-memory.md")
            .expect("session path");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create session dir");
        }
        std::fs::write(
            &path,
            "# Session Memory\n\n## Current State\nPrefer the local session-memory artifact for the current session.\n",
        )
        .expect("write session memory");

        let api =
            astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).expect("thin client");
        let record = load_current_session_memory(&api, "", &session_id)
            .await
            .expect("load session memory")
            .expect("record");
        assert_eq!(record.memory_id, "local-session-memory");
        assert!(
            record
                .body
                .contains("Prefer the local session-memory artifact")
        );

        std::fs::remove_file(&path).expect("remove session memory");
    }

    #[tokio::test]
    async fn store_current_session_memory_rejects_invalid_local_session_id_before_cloud_sync() {
        let api =
            astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).expect("thin client");
        let error = store_current_session_memory(
            &api,
            "",
            "bad/session-id",
            None,
            "# Session Memory\n\n## Current State\nshould fail locally first\n",
        )
        .await
        .expect_err("invalid session id should fail before any cloud sync");

        assert!(error.contains("current session memory write failed"));
    }

    #[tokio::test]
    async fn store_current_session_memory_reports_local_success_when_cloud_sync_fails() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let _dir_guard = astra_services::session_journal::JournalDirGuard::new(tmp.path());
        let session_id = format!(
            "sess-store-local-first-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        );
        let api =
            astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).expect("thin client");
        let body =
            "# Session Memory\n\n## Current State\nlocal write should land before cloud sync\n";

        let error = store_current_session_memory(&api, "", &session_id, None, body)
            .await
            .expect_err("unreachable cloud should fail after local write");

        assert!(error.contains("updated locally but cloud sync failed"));
        let path = astra_services::local_session_artifact_store()
            .session_path(&session_id, "session-memory.md")
            .expect("session path");
        let written = std::fs::read_to_string(path).expect("local session-memory.md");
        assert!(written.contains("local write should land before cloud sync"));
        let metadata =
            astra_runtime::session_memory::runner::load_local_session_memory_metadata(&session_id)
                .expect("local metadata");
        assert_eq!(
            metadata.current_snapshot_source.as_deref(),
            Some("manual_edit")
        );
        assert_eq!(
            metadata.last_remote_sync_status.as_deref(),
            Some("memoria_failed")
        );
    }

    #[tokio::test]
    async fn store_current_session_memory_treats_local_artifact_id_as_create_not_correct() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let tmp = tempfile::TempDir::new().expect("tempdir");
        let _dir_guard = astra_services::session_journal::JournalDirGuard::new(tmp.path());
        let session_id = format!(
            "sess-local-artifact-id-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        );
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/memory/store"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .expect(1)
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).expect("thin client");

        store_current_session_memory(
            &api,
            "token",
            &session_id,
            Some("local-session-memory"),
            "# Session Memory\n\n## Current State\nsynthetic local ids must store remotely\n",
        )
        .await
        .expect("synthetic local ids should create/store remotely");
    }

    // ── /memory subcommand contracts ──

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
    fn memory_forget_requires_non_empty_id() {
        assert!(parse_memory_forget_args("").is_err());
        // --reason is now optional; defaults to "user requested /memory forget"
        let parsed = parse_memory_forget_args("mem-1").unwrap();
        assert_eq!(parsed.0, "mem-1");
        assert_eq!(parsed.1, "user requested /memory forget");
        // empty --reason still uses default
        let parsed = parse_memory_forget_args("mem-1 --reason   ").unwrap();
        assert_eq!(parsed.0, "mem-1");
        assert_eq!(parsed.1, "user requested /memory forget");
        // explicit reason works
        let parsed = parse_memory_forget_args("mem-1 --reason duplicate stale memory").unwrap();
        assert_eq!(parsed.0, "mem-1");
        assert_eq!(parsed.1, "duplicate stale memory");
    }

    #[test]
    fn memory_forget_ui_requires_a_confirmed_complete_receipt() {
        assert!(
            confirmed_memory_purge_notice(
                r#"{"status":"completed","deleted_count":1,"unresolved_count":0}"#,
                "mem-1"
            )
            .is_ok()
        );
        assert!(
            confirmed_memory_purge_notice(
                r#"{"status":"not_found","deleted_count":0,"unresolved_count":1,"message":"0 removed"}"#,
                "mem-1"
            )
            .is_err()
        );
        assert!(confirmed_memory_purge_notice(r#"{"memory_id":"generic"}"#, "mem-1").is_err());
    }

    // ── /memory edit — replace_md_section ───────────────────────────────

    #[test]
    fn extract_md_section_returns_exact_named_section() {
        let body = "## Active Goals\n- old goal\n\n## Pending Todos\n- do stuff\n";
        let result = extract_md_section(body, "Active Goals").expect("section exists");
        assert_eq!(result, "- old goal");
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
    fn extract_md_section_treats_single_comment_placeholder_as_empty() {
        let body = "## Active Goals\n<!-- Current goals explicitly stated by the user or assistant. Do NOT invent goals. -->\n\n## Pending Todos\n- real todo\n";
        assert!(extract_md_section(body, "Active Goals").is_none());
    }

    #[test]
    fn extract_md_section_strips_leading_comment_placeholder() {
        let body = "## Active Goals\n<!-- Current goals explicitly stated by the user or assistant. Do NOT invent goals. -->\n- real goal\n\n## Pending Todos\n- real todo\n";
        let result = extract_md_section(body, "Active Goals").expect("section exists");
        assert_eq!(result, "- real goal");
    }

    #[test]
    fn extract_md_section_preserves_nonempty_active_goal_wording() {
        let body =
            "## Active Goals\n- None remaining; task completed.\n\n## Completed\n- Finished work\n";
        assert_eq!(
            extract_md_section(body, "Active Goals").as_deref(),
            Some("- None remaining; task completed.")
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
}
