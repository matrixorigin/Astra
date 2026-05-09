//! TUI-native slash command dispatch.
//!
//! Each command is handled inline without leaving the TUI. Commands that
//! need complex interactive UI push a BottomPaneView. Commands that only
//! produce output render to scrollback. Unrecognized or complex commands
//! fall back to `with_restored()` which temporarily exits the TUI.

use crate::command_registry;
use crate::repl_state::ReplState;
use crate::tui::bottom_pane::BottomPane;
use crate::tui::bottom_pane::list_selection_view::{ListSelectionView, SelectionItem};
use crate::tui::chat_cell::ChatCell;
use crate::tui::chat_cell::system_cell::SystemChatCell;
use crate::tui::terminal::TerminalGuard;

pub(crate) enum SlashResult {
    Handled,
    Exit,
    Fallback,
}

/// Context needed by slash dispatch — avoids passing 8+ loose arguments.
pub(crate) struct DispatchContext<'a> {
    pub api: &'a astra_thin_client::ThinClient,
    pub profile: Option<&'a str>,
    pub state: &'a mut ReplState,
    pub guard: &'a mut TerminalGuard,
    pub bottom_pane: &'a mut BottomPane,
    pub width: u16,
}

impl<'a> DispatchContext<'a> {
    fn show_info(&mut self, msg: String) {
        let cell = SystemChatCell::info(msg);
        self.guard
            .queue_history_lines(cell.display_lines(self.width));
    }

    fn show_error(&mut self, msg: String) {
        let cell = SystemChatCell::error(msg);
        self.guard
            .queue_history_lines(cell.display_lines(self.width));
    }
}

/// Parse and dispatch a slash command. Returns how the caller should proceed.
pub(crate) async fn dispatch(text: &str, ctx: &mut DispatchContext<'_>) -> SlashResult {
    let (cmd, args) = parse_slash(text);

    // Resolve command name via registry (handles prefix matching)
    let resolved = match command_registry::resolve_command(cmd) {
        Ok(name) => name,
        Err(candidates) => {
            if candidates.is_empty() {
                ctx.show_error(format!("Unknown command: {cmd}"));
            } else {
                ctx.show_error(format!(
                    "Ambiguous command: {cmd} (did you mean: {}?)",
                    candidates.join(", ")
                ));
            }
            return SlashResult::Handled;
        }
    };

    match resolved {
        // ── Exit ────────────────────────────────────────────────────
        "/exit" | "/quit" => SlashResult::Exit,

        // ── Help ────────────────────────────────────────────────────
        "/help" | "/commands" => {
            use crate::tui::bottom_pane::help_view::HelpView;
            ctx.bottom_pane.push_view(Box::new(HelpView::new()));
            SlashResult::Handled
        }

        // ── Auth forms (inline TUI card instead of dropping out to
        //    bare-terminal prompts that looked disjoint and stole keys) ─
        "/login" => {
            use crate::tui::bottom_pane::login_view::{LoginMode, LoginView};
            ctx.bottom_pane.push_view(Box::new(LoginView::new(LoginMode::Login)));
            SlashResult::Handled
        }
        "/register" => {
            use crate::tui::bottom_pane::login_view::{LoginMode, LoginView};
            ctx.bottom_pane.push_view(Box::new(LoginView::new(LoginMode::Register)));
            SlashResult::Handled
        }

        // ── Model selector ──────────────────────────────────────────
        "/model" => {
            if !args.is_empty() {
                // /model <name> — set directly
                ctx.state.model = Some(args.to_string());
                ctx.bottom_pane.footer.model = Some(args.to_string());
                ctx.show_info(format!("Model set to: {args}"));
                return SlashResult::Handled;
            }
            let token = crate::repl_runtime::current_access_token(ctx.profile);
            match crate::slash_router::fetch_model_list(ctx.api, token.as_deref()).await {
                Ok(models) => {
                    let current = ctx.state.model.clone().unwrap_or_default();
                    let items: Vec<SelectionItem> = models
                        .into_iter()
                        .map(|m| {
                            let is_current = m == current;
                            SelectionItem {
                                name: m,
                                description: None,
                                is_current,
                            }
                        })
                        .collect();
                    if items.is_empty() {
                        ctx.show_info("No models available".into());
                    } else {
                        let view = ListSelectionView::new(items, Some("Select model:".into()));
                        ctx.bottom_pane.push_view(Box::new(view));
                    }
                }
                Err(e) => ctx.show_error(format!("Failed to fetch models: {e}")),
            }
            SlashResult::Handled
        }

        // ── Stats ───────────────────────────────────────────────────
        "/stats" => {
            if !args.is_empty() {
                // Direct subcommand: /stats history, /stats tools, etc.
                show_stats_view(args, ctx.state, ctx.guard, ctx.bottom_pane);
                return SlashResult::Handled;
            }
            let items = vec![
                SelectionItem {
                    name: "Session overview".into(),
                    description: Some("Current session: turns, tokens, cost, duration".into()),
                    is_current: false,
                },
                SelectionItem {
                    name: "History".into(),
                    description: Some("Stats across recent sessions".into()),
                    is_current: false,
                },
                SelectionItem {
                    name: "Tools".into(),
                    description: Some("Tool call performance dashboard".into()),
                    is_current: false,
                },
                SelectionItem {
                    name: "Cost".into(),
                    description: Some("Token cost breakdown".into()),
                    is_current: false,
                },
                SelectionItem {
                    name: "Health".into(),
                    description: Some("Tool health status".into()),
                    is_current: false,
                },
                SelectionItem {
                    name: "Learn".into(),
                    description: Some("Learning insights: entities, patterns, drift".into()),
                    is_current: false,
                },
            ];
            let view = ListSelectionView::new(items, Some("Stats — choose a view:".into()));
            ctx.bottom_pane.push_view(Box::new(view));
            SlashResult::Handled
        }

        // ── Skills ──────────────────────────────────────────────────
        "/skill" | "/skills" => {
            let skill_count = ctx
                .state
                .unified_skill_registry
                .all_manifests()
                .iter()
                .filter(|m| m.user_invocable)
                .count();
            let items = vec![
                SelectionItem {
                    name: "List skills".into(),
                    description: Some(format!(
                        "Tip: press $ to open this list directly. ({skill_count} skills)"
                    )),
                    is_current: false,
                },
                SelectionItem {
                    name: "Skill info".into(),
                    description: Some("Show details of a specific skill".into()),
                    is_current: false,
                },
            ];
            let view = ListSelectionView::new(items, Some("Skills — choose an action:".into()));
            ctx.bottom_pane.push_view(Box::new(view));
            SlashResult::Handled
        }

        // ── Allow / permission mode ─────────────────────────────────
        "/allow" | "/yolo" => {
            use crate::permission_manager::PermissionMode;
            if resolved == "/yolo" {
                ctx.state.perm_manager.set_mode(PermissionMode::Auto);
                ctx.show_info(
                    "⚡ YOLO mode! All tools auto-approved. Use /allow prompt to restore.".into(),
                );
                return SlashResult::Handled;
            }
            match args {
                "" => {
                    let current = ctx.state.perm_manager.mode();
                    let rules_count = ctx.state.perm_manager.rules_summary().lines().count();
                    let items = vec![
                        SelectionItem {
                            name: "Auto".into(),
                            description: Some("All tools auto-approved".into()),
                            is_current: current == PermissionMode::Auto,
                        },
                        SelectionItem {
                            name: "Prompt".into(),
                            description: Some("Ask before write/execute tools".into()),
                            is_current: current == PermissionMode::Prompt,
                        },
                        SelectionItem {
                            name: "Deny".into(),
                            description: Some("Deny all tool calls".into()),
                            is_current: current == PermissionMode::Deny,
                        },
                        SelectionItem {
                            name: "Rules".into(),
                            description: Some(format!(
                                "View permission rules ({rules_count} lines)"
                            )),
                            is_current: false,
                        },
                    ];
                    ctx.bottom_pane.push_view(Box::new(ListSelectionView::new(
                        items,
                        Some(format!("Permission mode: {current}")),
                    )));
                    SlashResult::Handled
                }
                "all" | "auto" => {
                    ctx.state.perm_manager.set_mode(PermissionMode::Auto);
                    ctx.show_info("Permission mode → auto (all tools auto-approved)".into());
                    SlashResult::Handled
                }
                "prompt" => {
                    ctx.state.perm_manager.set_mode(PermissionMode::Prompt);
                    ctx.show_info("Permission mode → prompt".into());
                    SlashResult::Handled
                }
                "deny" => {
                    ctx.state.perm_manager.set_mode(PermissionMode::Deny);
                    ctx.show_info("Permission mode → deny".into());
                    SlashResult::Handled
                }
                "rules" | "status" => {
                    use crate::tui::bottom_pane::info_view::InfoView;
                    let summary = ctx.state.perm_manager.rules_summary();
                    ctx.bottom_pane.push_view(Box::new(InfoView::from_plain(
                        "Permission Rules",
                        summary.lines().map(|l| l.to_string()).collect(),
                    )));
                    SlashResult::Handled
                }
                _ => {
                    ctx.show_error(format!(
                        "Unknown mode '{args}'. Use: auto, prompt, deny, all, rules"
                    ));
                    SlashResult::Handled
                }
            }
        }

        // ── State commands → with_restored (share full logic with non-TUI) ──
        "/clear" | "/undo" | "/redo" | "/compact" | "/explain" | "/verbose" | "/reflect" => {
            SlashResult::Fallback
        }

        // ── Context panel (TUI-native) ──────────────────────────────
        //
        // `/context` with no args pops a live breakdown view built from
        // the most recent turn's `TokenBudgetTrace`. Subcommands
        // (`breakdown`, `explain`, `cognition`) fall through to the
        // existing rustyline-style printer via Fallback.
        "/context" => {
            if !args.is_empty() {
                return SlashResult::Fallback;
            }
            use crate::tui::bottom_pane::context_panel_view::ContextPanelView;
            use crate::tui::context_panel::ContextBreakdown;
            let breakdown = match ctx.state.observability_session.as_ref() {
                Some(session) => {
                    let guard = session.read().unwrap_or_else(|e| e.into_inner());
                    match guard.context_traces.last() {
                        Some(trace) => ContextBreakdown::from_trace(&trace.token_budget),
                        None => ContextBreakdown::empty(),
                    }
                }
                None => ContextBreakdown::empty(),
            };
            ctx.bottom_pane
                .push_view(Box::new(ContextPanelView::new(breakdown)));
            SlashResult::Handled
        }

        // ── SQL table view (TUI-native, astra-unique) ───────────────
        //
        // Runs a SQL query against MatrixOne via the existing `mo_query`
        // tool, parses the mysql-client ASCII output, and renders it as
        // a navigable ratatui table. Read-only-by-default: the safety
        // guard in mo_query blocks DROP/DELETE/TRUNCATE/ALTER without
        // an explicit flag, and we don't expose that flag here.
        "/table" => {
            if args.trim().is_empty() {
                ctx.show_info(
                    "Usage: /table <sql>\nExample: /table SELECT * FROM users LIMIT 20".into(),
                );
                return SlashResult::Handled;
            }
            // Paint a BusyView over the bottom pane so the user sees
            // *something* while the SQL runs. We force an immediate
            // draw because nothing else will redraw until the
            // spawn_blocking future resolves.
            use crate::tui::bottom_pane::busy_view::BusyView;
            ctx.bottom_pane.push_view(Box::new(
                BusyView::new("Running SQL query…").with_title(" /table "),
            ));
            let _ = crate::tui::do_draw(ctx.guard, &None, ctx.bottom_pane);

            // `mo_query` shells out to the mysql client (blocking IO) —
            // park it on a blocking thread so we don't freeze the async
            // event loop while the query runs.
            let sql_text = args.to_string();
            let output = tokio::task::spawn_blocking(move || {
                let executor = crate::edge_tools::ToolExecutor::new(
                    std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
                );
                executor.mo_query(&serde_json::json!({ "sql": sql_text }))
            })
            .await
            .unwrap_or_else(|e| format!("Error: SQL execution task failed: {e}"));

            // Remove the BusyView — a real panel (or info message)
            // takes its place below.
            let _ = ctx.bottom_pane.pop_view();
            use crate::tui::bottom_pane::table_view::TablePanelView;
            use crate::tui::table_view::parse;
            match parse(&output) {
                Some(table) => {
                    ctx.bottom_pane
                        .push_view(Box::new(TablePanelView::new(table)));
                }
                None => {
                    // Parser rejected — show raw output to scrollback so
                    // the user can see the error or "OK (no results)".
                    ctx.show_info(output);
                }
            }
            SlashResult::Handled
        }

        // ── Panels cheat sheet ──────────────────────────────────────
        "/panels" => {
            use crate::tui::bottom_pane::info_view::InfoView;
            let body = build_panels_cheat_sheet_lines();
            ctx.bottom_pane.push_view(Box::new(
                InfoView::from_plain("TUI panels", body).with_reopen("/panels"),
            ));
            SlashResult::Handled
        }

        // ── Worktrees (TUI-native) ──────────────────────────────────
        "/worktrees" => {
            use crate::tui::bottom_pane::worktrees_view::WorktreesView;
            use crate::tui::worktrees::{WorktreeList, parse};

            // `git worktree list --porcelain` on a blocking thread.
            let porcelain = tokio::task::spawn_blocking(|| {
                let cwd = std::env::current_dir()
                    .unwrap_or_else(|_| std::path::PathBuf::from("."));
                let out = std::process::Command::new("git")
                    .args(["worktree", "list", "--porcelain"])
                    .current_dir(&cwd)
                    .output();
                match out {
                    Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
                    _ => String::new(),
                }
            })
            .await
            .unwrap_or_default();

            let mut entries = parse(&porcelain);
            // Enrich each entry with session count (best-effort; any
            // errors collapse to zero).
            for e in entries.iter_mut() {
                let sessions = astra_services::session_workspace::list_sessions_by_git_root(
                    &e.path, None, 50,
                );
                e.session_count = sessions.len();
                e.last_session_at = sessions.first().map(|s| s.updated_at.clone());
            }

            if entries.is_empty() {
                ctx.show_info(
                    "No worktrees found (or `git worktree list` failed).".into(),
                );
                return SlashResult::Handled;
            }
            let list = WorktreeList::new(entries);
            ctx.bottom_pane.push_view(Box::new(WorktreesView::new(list)));
            SlashResult::Handled
        }

        // ── Session timeline (TUI-native) ───────────────────────────
        "/timeline" => {
            use crate::tui::bottom_pane::timeline_view::TimelineView;
            use crate::tui::timeline::{JournalTurnSource, Timeline};
            let Some(sid) = ctx.state.session_id.clone() else {
                ctx.show_info("No active session — /timeline needs a session id.".into());
                return SlashResult::Handled;
            };
            let timeline = Timeline::new(JournalTurnSource::new(), &sid);
            if timeline.is_empty() {
                ctx.show_info(format!("No turns recorded yet for session {sid}."));
                return SlashResult::Handled;
            }
            ctx.bottom_pane
                .push_view(Box::new(TimelineView::new(timeline)));
            SlashResult::Handled
        }

        // ── Resume picker (TUI-native) ──────────────────────────────
        "/resume" => {
            if !args.is_empty() {
                // `/resume <id>` — direct path takes the rustyline-compatible
                // fallback so we go through the full restore pipeline.
                return SlashResult::Fallback;
            }
            use crate::tui::bottom_pane::session_picker_view::SessionPickerView;
            use crate::tui::session_picker::{FsSessionSource, SessionDiscovery};
            let disco = SessionDiscovery::new(FsSessionSource::new(), 50);
            if disco.total() == 0 {
                ctx.show_info("No previous sessions found.".into());
                return SlashResult::Handled;
            }
            ctx.bottom_pane
                .push_view(Box::new(SessionPickerView::new(disco)));
            SlashResult::Handled
        }

        // ── Copy last response ──────────────────────────────────────
        "/copy" => {
            match &ctx.state.last_response {
                Some(resp) if !resp.is_empty() => {
                    let n = resp.chars().count();
                    if crate::slash_info::copy_to_clipboard(resp) {
                        let preview: String = resp.chars().take(60).collect();
                        let suffix = if n > 60 { "…" } else { "" };
                        ctx.show_info(format!("Copied {n} chars: {preview}{suffix}"));
                    } else {
                        ctx.show_error("No clipboard tool found (install xclip or xsel)".into());
                    }
                }
                _ => ctx.show_info("No response to copy".into()),
            }
            SlashResult::Handled
        }

        // ── Version ─────────────────────────────────────────────────
        "/version" => {
            ctx.show_info(format!("astra v{}", env!("CARGO_PKG_VERSION")));
            SlashResult::Handled
        }

        // ── Whoami — matches render_whoami() from slash_info.rs ─────
        "/whoami" => {
            use crate::tui::bottom_pane::info_view::InfoView;
            let recent_tools = if ctx.state.recent_tools.is_empty() {
                "<none>".to_string()
            } else {
                ctx.state
                    .recent_tools
                    .iter()
                    .take(8)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let pending = ctx
                .state
                .skill_improvement_tracker
                .pending_proposal
                .as_ref()
                .map(|p| p.skill_name.clone())
                .unwrap_or_else(|| "<none>".into());
            let pairs: Vec<(&str, String)> = vec![
                ("version", format!("astra v{}", env!("CARGO_PKG_VERSION"))),
                (
                    "model",
                    ctx.state.model.clone().unwrap_or_else(|| "<unset>".into()),
                ),
                (
                    "session",
                    ctx.state
                        .session_id
                        .clone()
                        .unwrap_or_else(|| "<none>".into()),
                ),
                ("turn", ctx.state.turn.to_string()),
                ("exchanges", ctx.state.history.len().to_string()),
                ("skills", ctx.state.unified_skill_registry.len().to_string()),
                ("pending improve", pending),
                ("recent tools", recent_tools),
                ("permission", format!("{}", ctx.state.perm_manager.mode())),
                ("explain", format!("{}", ctx.state.explain)),
                (
                    "verbose",
                    if ctx.state.verbose_mode { "on" } else { "off" }.into(),
                ),
            ];
            ctx.bottom_pane
                .push_view(Box::new(InfoView::from_key_value("whoami", pairs)));
            SlashResult::Handled
        }

        // ── History — interactive search view ────────────────────────
        "/history" => {
            use crate::tui::bottom_pane::history_view::HistoryView;
            if ctx.state.history.is_empty() {
                ctx.show_info("No history yet".into());
                return SlashResult::Handled;
            }
            let initial_query = if args.starts_with("grep ") {
                args.strip_prefix("grep ").unwrap_or("").trim()
            } else {
                ""
            };
            ctx.bottom_pane.push_view(Box::new(HistoryView::new(
                &ctx.state.history,
                initial_query,
            )));
            SlashResult::Handled
        }

        // ── Style ───────────────────────────────────────────────────
        "/style" => SlashResult::Fallback,

        // ── Instructions — subcommand menu or direct action ─────────
        "/instructions" => {
            use crate::tui::bottom_pane::info_view::InfoView;
            match args {
                "" => {
                    let cwd = std::env::current_dir().unwrap_or_default();
                    let inst_path = cwd.join(".astra").join("instructions.md");

                    let file_info = if let Ok(meta) = std::fs::metadata(&inst_path) {
                        let size = meta.len();
                        let age = meta
                            .modified()
                            .ok()
                            .and_then(|t| {
                                let s = t.elapsed().ok()?.as_secs();
                                Some(if s < 60 {
                                    format!("{s}s ago")
                                } else if s < 3600 {
                                    format!("{}m ago", s / 60)
                                } else if s < 86400 {
                                    format!("{}h ago", s / 3600)
                                } else {
                                    format!("{}d ago", s / 86400)
                                })
                            })
                            .unwrap_or_else(|| "?".into());
                        format!("{size}B, {age}")
                    } else {
                        "file not found".into()
                    };

                    let status = if let Some(ref pi) = ctx.state.project_instructions {
                        let lc = pi.lines().count();
                        format!("✓ loaded ({lc} lines) · {file_info}")
                    } else {
                        format!("✗ not loaded · {file_info}")
                    };

                    let items = vec![
                        SelectionItem {
                            name: "Show".into(),
                            description: Some(status),
                            is_current: false,
                        },
                        SelectionItem {
                            name: "Reload".into(),
                            description: Some(format!("Reload from {}", inst_path.display())),
                            is_current: false,
                        },
                        SelectionItem {
                            name: "Off".into(),
                            description: Some("Disable instructions for this session".into()),
                            is_current: false,
                        },
                    ];
                    ctx.bottom_pane.push_view(Box::new(ListSelectionView::new(
                        items,
                        Some("Project Instructions:".into()),
                    )));
                    SlashResult::Handled
                }
                "show" => {
                    if let Some(ref pi) = ctx.state.project_instructions {
                        let line_count = pi.lines().count();
                        let title = format!("Project Instructions ({line_count} lines)");
                        ctx.bottom_pane.push_view(Box::new(
                            InfoView::from_plain(
                                &title,
                                pi.lines().map(|l| format!("  {l}")).collect(),
                            )
                            .with_reopen("/instructions"),
                        ));
                    } else {
                        ctx.show_info("No project instructions loaded. Create .astra/instructions.md in your project root.".into());
                    }
                    SlashResult::Handled
                }
                "reload" => {
                    if let Some(instructions) =
                        crate::project_instructions::discover_project_instructions()
                    {
                        let lines = instructions.lines().count();
                        ctx.state.project_instructions = Some(instructions);
                        ctx.show_info(format!("Reloaded project instructions ({lines} lines)"));
                    } else {
                        ctx.state.project_instructions = None;
                        ctx.show_info("No .astra/instructions.md found".into());
                    }
                    SlashResult::Handled
                }
                "off" => {
                    ctx.state.project_instructions = None;
                    ctx.show_info("Project instructions disabled for this session".into());
                    SlashResult::Handled
                }
                _ => {
                    ctx.show_error("Usage: /instructions [show|reload|off]".into());
                    SlashResult::Handled
                }
            }
        }

        // ── Everything else → with_restored fallback ────────────────
        _ => SlashResult::Fallback,
    }
}

/// Cheat sheet content for `/panels`. Pure — separate from the
/// dispatch wiring so it can be snapshot-tested.
pub(crate) fn build_panels_cheat_sheet_lines() -> Vec<String> {
    // (command, one-line purpose, key hint)
    const PANELS: &[(&str, &str, &str)] = &[
        (
            "/resume",
            "pick and restore a recent session",
            "↑↓ navigate · type to filter · Enter resume · Esc close",
        ),
        (
            "/context",
            "visualise the current turn's token budget",
            "Enter / q / Esc close",
        ),
        (
            "/timeline",
            "browse this session's turn-by-turn journal",
            "↑↓ navigate · PgUp/PgDn page · q / Esc close",
        ),
        (
            "/table <sql>",
            "run a SQL query and render a navigable table",
            "↑↓ rows · ←→ cols · Home/End jump · q / Esc close",
        ),
        (
            "/worktrees",
            "list git worktrees with per-worktree session counts",
            "↑↓ navigate · q / Esc close",
        ),
        (
            "/help",
            "list every slash command grouped by category",
            "↑↓ browse · Esc close",
        ),
    ];
    let mut out = Vec::with_capacity(PANELS.len() * 3);
    for (cmd, desc, hint) in PANELS {
        out.push(format!("  {cmd}"));
        out.push(format!("      {desc}"));
        out.push(format!("      {hint}"));
        out.push(String::new());
    }
    // Trim trailing blank so InfoView scrolls cleanly.
    while out.last().is_some_and(|s| s.trim().is_empty()) {
        out.pop();
    }
    out
}

/// Detect the conventional "sess_<…>" / uuid-like session id shape.
pub(crate) fn looks_like_session_id(s: &str) -> bool {
    if s.starts_with("sess_") {
        return true;
    }
    // Fallback: UUID-like 36 chars with 4 dashes.
    s.len() == 36 && s.chars().filter(|c| *c == '-').count() == 4
}

/// Handle a ViewCompleted result from a BottomPaneView.
pub(crate) fn handle_view_result(
    name: &str,
    state: &mut ReplState,
    guard: &mut TerminalGuard,
    bottom_pane: &mut BottomPane,
) {
    let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);

    // Session picker result is handled by the outer event loop (it
    // needs to run the async resume pipeline); this sync fn just
    // lets it pass through — see `is_session_id` below.
    if looks_like_session_id(name) {
        return;
    }

    // Skill menu actions
    if name == "List skills" {
        bottom_pane.composer.set_text("$");
        return;
    }
    if name == "Skill info" {
        let msg = SystemChatCell::info("Use /skill info <name> for details".into());
        guard.queue_history_lines(msg.display_lines(w));
        return;
    }

    // Skill name → insert $name into composer
    if state.unified_skill_registry.get_manifest(name).is_some() {
        bottom_pane.composer.set_text(&format!("${name} "));
        return;
    }

    // Stats menu → show inline view
    let stats_sub = match name {
        "Session overview" => Some(""),
        "History" => Some("history"),
        "Tools" => Some("tools"),
        "Cost" => Some("cost"),
        "Health" => Some("health"),
        "Learn" => Some("learn"),
        _ => None,
    };
    if let Some(sub) = stats_sub {
        show_stats_view(sub, state, guard, bottom_pane);
        return;
    }

    // Instructions menu → dispatch subcommands
    match name {
        "Show" => {
            use crate::tui::bottom_pane::info_view::InfoView;
            if let Some(ref pi) = state.project_instructions {
                let lc = pi.lines().count();
                bottom_pane.push_view(Box::new(
                    InfoView::from_plain(
                        &format!("Project Instructions ({lc} lines)"),
                        pi.lines().map(|l| format!("  {l}")).collect(),
                    )
                    .with_reopen("/instructions"),
                ));
            } else {
                let msg = SystemChatCell::info(
                    "No project instructions loaded. Create .astra/instructions.md".into(),
                );
                guard.queue_history_lines(msg.display_lines(w));
            }
            return;
        }
        "Reload" => {
            if let Some(instructions) = crate::project_instructions::discover_project_instructions()
            {
                let lc = instructions.lines().count();
                state.project_instructions = Some(instructions);
                let msg =
                    SystemChatCell::info(format!("Reloaded project instructions ({lc} lines)"));
                guard.queue_history_lines(msg.display_lines(w));
            } else {
                state.project_instructions = None;
                let msg = SystemChatCell::info("No .astra/instructions.md found".into());
                guard.queue_history_lines(msg.display_lines(w));
            }
            return;
        }
        "Off" => {
            state.project_instructions = None;
            let msg = SystemChatCell::info("Project instructions disabled".into());
            guard.queue_history_lines(msg.display_lines(w));
            return;
        }
        _ => {}
    }

    // Permission menu
    match name {
        "Auto" => {
            use crate::permission_manager::PermissionMode;
            state.perm_manager.set_mode(PermissionMode::Auto);
            let msg = SystemChatCell::info("Permission mode → auto".into());
            guard.queue_history_lines(msg.display_lines(w));
            return;
        }
        "Prompt" => {
            use crate::permission_manager::PermissionMode;
            state.perm_manager.set_mode(PermissionMode::Prompt);
            let msg = SystemChatCell::info("Permission mode → prompt".into());
            guard.queue_history_lines(msg.display_lines(w));
            return;
        }
        "Deny" => {
            use crate::permission_manager::PermissionMode;
            state.perm_manager.set_mode(PermissionMode::Deny);
            let msg = SystemChatCell::info("Permission mode → deny".into());
            guard.queue_history_lines(msg.display_lines(w));
            return;
        }
        "Rules" => {
            use crate::tui::bottom_pane::info_view::InfoView;
            let summary = state.perm_manager.rules_summary();
            bottom_pane.push_view(Box::new(
                InfoView::from_plain(
                    "Permission Rules",
                    summary.lines().map(|l| l.to_string()).collect(),
                )
                .with_reopen("/allow"),
            ));
            return;
        }
        _ => {}
    }

    // Slash command selected from help → insert into composer
    if name.starts_with('/') {
        bottom_pane.composer.set_text(&format!("{name} "));
        return;
    }

    // Model name → apply
    state.model = Some(name.to_string());
    bottom_pane.footer.model = Some(name.to_string());
    let msg = SystemChatCell::info(format!("Model set to: {name}"));
    guard.queue_history_lines(msg.display_lines(w));
}

fn show_stats_view(
    sub: &str,
    state: &ReplState,
    _guard: &mut TerminalGuard,
    bottom_pane: &mut BottomPane,
) {
    use crate::tui::bottom_pane::info_view::InfoView;
    use astra_services::{session_analytics, session_journal};

    match sub {
        "" | "overview" => {
            let sid = state.session_id.clone().unwrap_or_default();
            let short_sid = sid.get(..8.min(sid.len())).unwrap_or(&sid);
            let mut pairs: Vec<(&str, String)> = vec![
                ("session", short_sid.to_string()),
                (
                    "model",
                    state.model.clone().unwrap_or_else(|| "<unset>".into()),
                ),
                ("turns", state.turn.to_string()),
                (
                    "tokens",
                    format!(
                        "{}↑ {}↓",
                        state.total_prompt_tokens, state.total_completion_tokens
                    ),
                ),
                ("cost", format!("${:.4}", state.total_session_cost)),
            ];
            if !sid.is_empty() {
                if let Ok(events) = session_journal::read_journal(&sid) {
                    let stats = session_analytics::compute_session_stats(&sid, &events);
                    pairs.push((
                        "duration",
                        format!(
                            "{:.1}s ({:.0}ms/turn)",
                            stats.total_duration_ms as f64 / 1000.0,
                            stats.avg_duration_ms as f64
                        ),
                    ));
                    pairs.push((
                        "tool calls",
                        format!(
                            "{} ({} failed, {:.0}% err)",
                            stats.total_tool_calls,
                            stats.failed_tool_calls,
                            stats.tool_error_rate * 100.0
                        ),
                    ));
                    if !stats.unique_tools.is_empty() {
                        pairs.push(("tools used", stats.unique_tools.join(", ")));
                    }
                    if stats.error_count > 0 || stats.stall_count > 0 {
                        pairs.push((
                            "issues",
                            format!("{} errors, {} stalls", stats.error_count, stats.stall_count),
                        ));
                    }
                    if stats.checkpoint_count > 0 {
                        pairs.push(("checkpoints", stats.checkpoint_count.to_string()));
                    }
                }
            }
            bottom_pane.push_view(Box::new(
                InfoView::from_key_value("Session Stats", pairs).with_reopen("/stats"),
            ));
        }

        "history" => {
            let sessions = session_journal::list_sessions().unwrap_or_default();
            if sessions.is_empty() {
                bottom_pane.push_view(Box::new(
                    InfoView::from_plain("Session History", vec!["  No sessions found.".into()])
                        .with_reopen("/stats"),
                ));
                return;
            }
            let mut lines = Vec::new();
            let recent: Vec<_> = sessions.into_iter().take(10).collect();
            for sid in &recent {
                if let Ok(events) = session_journal::read_journal(sid) {
                    let s = session_analytics::compute_session_stats(sid, &events);
                    let short = &sid[..8.min(sid.len())];
                    let model = s.model.as_deref().unwrap_or("?");
                    lines.push(format!(
                        "  {short}  {:>3} turns  {:>6}+{:<6} tok  {:>3} tools  {model}",
                        s.turn_count, s.total_tokens_in, s.total_tokens_out, s.total_tool_calls,
                    ));
                }
            }
            let agg = {
                let mut all = Vec::new();
                for sid in &recent {
                    if let Ok(events) = session_journal::read_journal(sid) {
                        all.push(session_analytics::compute_session_stats(sid, &events));
                    }
                }
                session_analytics::aggregate_stats(&all)
            };
            lines.push(String::new());
            lines.push(format!(
                "  Summary: {} sessions, {} turns, {}+{} tokens",
                agg.session_count, agg.total_turns, agg.total_tokens_in, agg.total_tokens_out,
            ));
            bottom_pane.push_view(Box::new(
                InfoView::from_plain("Recent Sessions", lines).with_reopen("/stats"),
            ));
        }

        "tools" => {
            let sid = state.session_id.clone().unwrap_or_default();
            if sid.is_empty() {
                bottom_pane.push_view(Box::new(
                    InfoView::from_plain("Tool Performance", vec!["  No active session.".into()])
                        .with_reopen("/stats"),
                ));
                return;
            }
            let events = session_journal::read_journal(&sid).unwrap_or_default();
            let profiles = session_analytics::compute_tool_profiles(&events);
            if profiles.is_empty() {
                bottom_pane.push_view(Box::new(
                    InfoView::from_plain(
                        "Tool Performance",
                        vec!["  No tool calls recorded.".into()],
                    )
                    .with_reopen("/stats"),
                ));
                return;
            }
            let mut lines = Vec::new();
            lines.push(format!(
                "  {:<20} {:>5} {:>5} {:>7} {:>7} {:>6}",
                "tool", "calls", "fail", "avg ms", "max ms", "err%"
            ));
            for p in &profiles {
                lines.push(format!(
                    "  {:<20} {:>5} {:>5} {:>7} {:>7} {:>5.0}%",
                    p.name,
                    p.call_count,
                    p.fail_count,
                    p.avg_ms,
                    p.max_ms,
                    p.error_rate * 100.0,
                ));
            }
            let total_calls: u32 = profiles.iter().map(|p| p.call_count).sum();
            let total_ms: u64 = profiles.iter().map(|p| p.total_ms).sum();
            lines.push(String::new());
            lines.push(format!(
                "  {} calls, {:.1}s total tool time",
                total_calls,
                total_ms as f64 / 1000.0
            ));
            bottom_pane.push_view(Box::new(
                InfoView::from_plain("Tool Performance", lines).with_reopen("/stats"),
            ));
        }

        "cost" => {
            let pricing = &state.cached_pricing;
            let cost = crate::slash_stats::cost_for_tokens(
                state.total_prompt_tokens,
                state.total_completion_tokens,
                state.total_cache_read_tokens,
                state.total_cache_creation_tokens,
                pricing,
            );
            let mut pairs: Vec<(&str, String)> = vec![
                (
                    "model",
                    state.model.clone().unwrap_or_else(|| "<unset>".into()),
                ),
                (
                    "rates",
                    format!(
                        "${:.4}/1k prompt, ${:.4}/1k completion",
                        pricing.prompt, pricing.completion
                    ),
                ),
                (
                    "prompt",
                    format!(
                        "{} ({})",
                        state.total_prompt_tokens,
                        crate::slash_stats::format_cost(
                            state.total_prompt_tokens as f64 * pricing.prompt / 1000.0
                        )
                    ),
                ),
                (
                    "completion",
                    format!(
                        "{} ({})",
                        state.total_completion_tokens,
                        crate::slash_stats::format_cost(
                            state.total_completion_tokens as f64 * pricing.completion / 1000.0
                        )
                    ),
                ),
            ];
            if state.total_cache_read_tokens > 0 {
                let rate = pricing.cache_read.unwrap_or(pricing.prompt * 0.1);
                pairs.push((
                    "cache read",
                    format!(
                        "{} ({})",
                        state.total_cache_read_tokens,
                        crate::slash_stats::format_cost(
                            state.total_cache_read_tokens as f64 * rate / 1000.0
                        )
                    ),
                ));
            }
            pairs.push(("total", crate::slash_stats::format_cost(cost)));
            if state.turn > 0 {
                pairs.push((
                    "avg/turn",
                    crate::slash_stats::format_cost(cost / state.turn as f64),
                ));
            }
            bottom_pane.push_view(Box::new(
                InfoView::from_key_value("Session Cost", pairs).with_reopen("/stats"),
            ));
        }

        "learn" => {
            let mut pairs: Vec<(&str, String)> = Vec::new();
            if let Some(ref eg) = state.entity_graph {
                if let Ok(g) = eg.lock() {
                    pairs.push(("entities", g.len().to_string()));
                }
            }
            if let Some(ref pl) = state.pattern_library {
                if let Ok(p) = pl.lock() {
                    pairs.push(("patterns", p.len().to_string()));
                }
            }
            pairs.push((
                "skills tracked",
                state.skill_quality_tracker.all_entries().len().to_string(),
            ));
            if !state.drift_user_corrections.is_empty() {
                pairs.push((
                    "corrections",
                    state.drift_user_corrections.len().to_string(),
                ));
            }
            if !state.drift_compressed_turns.is_empty() {
                pairs.push((
                    "compactions",
                    state.drift_compressed_turns.len().to_string(),
                ));
            }
            if let Some(ref q) = state.drift_original_query {
                let short: String = q.chars().take(50).collect();
                pairs.push(("original query", short));
            }
            pairs.push((
                "discovered skills",
                state.discovered_skills.len().to_string(),
            ));
            if pairs.is_empty() {
                pairs.push(("status", "No learning data yet.".into()));
            }
            bottom_pane.push_view(Box::new(
                InfoView::from_key_value("Learning Insights", pairs).with_reopen("/stats"),
            ));
        }

        "health" => {
            let sid = state.session_id.clone().unwrap_or_default();
            if sid.is_empty() {
                bottom_pane.push_view(Box::new(
                    InfoView::from_plain("Tool Health", vec!["  No active session.".into()])
                        .with_reopen("/stats"),
                ));
                return;
            }
            let events = session_journal::read_journal(&sid).unwrap_or_default();
            let profiles = session_analytics::compute_tool_profiles(&events);
            let mut lines = Vec::new();
            for p in &profiles {
                let status = if p.fail_count == 0 { "✓" } else { "✗" };
                lines.push(format!(
                    "  {status} {:<20} {}/{} ok  {:.0}% err  avg {}ms",
                    p.name,
                    p.success_count,
                    p.call_count,
                    p.error_rate * 100.0,
                    p.avg_ms,
                ));
                if let Some(ref err) = p.last_error {
                    let short: String = err.chars().take(60).collect();
                    lines.push(format!("    └ {short}"));
                }
            }
            if lines.is_empty() {
                lines.push("  No tool calls recorded.".into());
            }
            bottom_pane.push_view(Box::new(
                InfoView::from_plain("Tool Health", lines).with_reopen("/stats"),
            ));
        }

        _ => {}
    }
}

fn parse_slash(text: &str) -> (&str, &str) {
    let text = text.trim();
    match text.find(' ') {
        Some(pos) => (&text[..pos], text[pos..].trim()),
        None => (text, ""),
    }
}

#[cfg(test)]
mod panels_tests {
    use super::build_panels_cheat_sheet_lines;

    #[test]
    fn cheat_sheet_lists_every_tui_native_panel() {
        let text = build_panels_cheat_sheet_lines().join("\n");
        for cmd in [
            "/resume",
            "/context",
            "/timeline",
            "/table",
            "/worktrees",
        ] {
            assert!(
                text.contains(cmd),
                "cheat sheet missing {cmd}; got: {text}"
            );
        }
    }

    #[test]
    fn cheat_sheet_shows_key_hints() {
        let text = build_panels_cheat_sheet_lines().join("\n");
        assert!(text.contains("↑"));
        assert!(text.contains("Esc"));
    }

    #[test]
    fn cheat_sheet_has_stable_snapshot() {
        insta::assert_snapshot!(
            "panels_cheat_sheet",
            build_panels_cheat_sheet_lines().join("\n")
        );
    }
}
