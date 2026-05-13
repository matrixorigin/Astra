//! TUI-native slash command dispatch.
//!
//! Each command is handled inline without leaving the TUI. Commands that
//! need complex interactive UI push a BottomPaneView. Commands that only
//! produce output render to scrollback. Unrecognized or complex commands
//! fall back to `with_restored()` which temporarily exits the TUI.

use crate::command_registry;
use crate::session_state::SessionState;
use crate::tui::bottom_pane::list_selection_view::{ListSelectionView, SelectionItem};
use crate::tui::bottom_pane::BottomPane;
use crate::tui::history_cell::system::SystemCell;
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
    pub state: &'a mut SessionState,
    pub guard: &'a mut TerminalGuard,
    pub bottom_pane: &'a mut BottomPane,
    pub chat_widget: &'a mut crate::tui::chat_widget::ChatWidget,
    pub width: u16,
}

impl<'a> DispatchContext<'a> {
    /// Free-floating informational line (dim, no corner glyph).
    /// Use for passive state ("No history yet", "astra v0.1.0"),
    /// NOT for command acknowledgements — those should visually pair
    /// with the `› /cmd` prompt above them; see `show_response`.
    ///
    /// Routed through `ChatWidget::commit_system` so the line lands
    /// in both the on-screen scrollback AND the JSONL transcript —
    /// resume will surface it, Ctrl+O will include it, the model's
    /// next turn will see it in history.
    fn show_info(&mut self, msg: String) {
        self.chat_widget.commit_system(SystemCell::info(msg));
    }

    /// Slash-command response ("Set model to Opus 4.6", "Permission
    /// mode → auto"). Rendered with the `⎿` corner glyph on the first
    /// line so the eye visually threads `› /cmd` → `⎿ …`.
    fn show_response(&mut self, msg: String) {
        self.chat_widget.commit_system(SystemCell::response(msg));
    }

    fn show_error(&mut self, msg: String) {
        self.chat_widget.commit_system(SystemCell::error(msg));
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
            ctx.bottom_pane
                .push_view(Box::new(LoginView::new(LoginMode::Login)));
            SlashResult::Handled
        }
        "/register" => {
            use crate::tui::bottom_pane::login_view::{LoginMode, LoginView};
            ctx.bottom_pane
                .push_view(Box::new(LoginView::new(LoginMode::Register)));
            SlashResult::Handled
        }

        // ── Model ───────────────────────────────────────────────────
        //
        // Forms:
        //   /model                   → open the picker (legacy default)
        //   /model list              → explicit alias for the picker
        //   /model info              → details panel for current model
        //   /model clear             → reset to API default
        //   /model <name>            → direct switch to <name>
        //
        // There is intentionally no `/model set <name>` — the
        // direct-name form is already the shortest path, and
        // having `set` as a second way to express the same thing
        // just clutters the subcommand popup.
        "/model" => {
            let trimmed = args.trim();
            if trimmed.is_empty() || trimmed == "list" {
                return open_model_picker(ctx).await;
            }
            let (sub, rest) = split_sub(trimmed);
            match sub {
                "info" => handle_model_info(ctx, rest).await,
                "clear" => handle_model_clear(ctx).await,
                // Everything else is the `/model <name>` shorthand.
                _ => {
                    handle_model_set(ctx, trimmed);
                    SlashResult::Handled
                }
            }
        }

        // ── Stats ───────────────────────────────────────────────────
        "/stats" => {
            if !args.is_empty() {
                // Direct subcommand: /stats history, /stats tools, etc.
                show_stats_view(args, ctx.state, ctx.bottom_pane);
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
                    ctx.show_response("Permission mode → auto (all tools auto-approved)".into());
                    SlashResult::Handled
                }
                "prompt" => {
                    ctx.state.perm_manager.set_mode(PermissionMode::Prompt);
                    ctx.show_response("Permission mode → prompt".into());
                    SlashResult::Handled
                }
                "deny" => {
                    ctx.state.perm_manager.set_mode(PermissionMode::Deny);
                    ctx.show_response("Permission mode → deny".into());
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
        "/clear" | "/undo" | "/redo" | "/compact" | "/explain" | "/reflect" => {
            SlashResult::Fallback
        }

        // ── Context panel (TUI-native) ──────────────────────────────
        //
        // Only two forms are supported:
        //   `/context`            → open the TUI panel
        //   `/context dump [path]` → write a JSON snapshot to disk
        //
        // Earlier iterations fell through to a rustyline-style
        // `breakdown`/`explain`/`cognition` printer, but those just
        // duplicate what the panel shows.  Anything else now gets
        // a short error that points the user at the two valid forms.
        "/context" => {
            let args_trim = args.trim();
            if let Some(rest) = args_trim.strip_prefix("dump") {
                return handle_context_dump(rest.trim(), ctx);
            }
            if !args_trim.is_empty() {
                use crate::tui::history_cell::system::SystemCell;
                ctx.chat_widget.commit_system(SystemCell::info(
                    "Usage: /context          — open the context panel\n       \
                     /context dump [path] — write a JSON snapshot",
                ));
                return SlashResult::Handled;
            }
            use crate::tui::bottom_pane::context_panel_view::ContextPanelView;
            use crate::tui::context_panel::model::{ActiveSkill, SessionSummary};
            use crate::tui::context_panel::{ContextBreakdown, ContextSnapshot};

            // Collect human-readable previews the trace doesn't
            // carry: per-turn transcript snippets (from the chat
            // widget's history) and process-state bits for the
            // System-prompt sub-rows (model id, cwd, git branch,
            // user-rules path).  The panel renders these under the
            // count rows when the user expands a section.
            let mut snap = ContextSnapshot::default();
            snap.model = ctx.state.model.as_deref();
            if let Ok(cwd) = std::env::current_dir() {
                snap.cwd = Some(display_path(&cwd));
            }
            snap.git_branch = detect_git_branch();
            snap.user_rules_path = find_user_rules_path();

            // Loaded system skills.  Surfaced as a Skills-section
            // fallback when the trace is silent (common for CLI
            // sessions where edge_profile.active_skills isn't set).
            snap.active_skills = ctx
                .state
                .active_system_skills
                .iter()
                .map(|s| ActiveSkill {
                    name: s.name.clone(),
                    description: s.description.clone(),
                })
                .collect();

            // Build the Session / Budget summary from SessionState.
            // All fields are cheap reads — no I/O, no extra locks.
            snap.session = Some(SessionSummary {
                session_id: ctx.state.session_id.clone().unwrap_or_default(),
                turn: ctx.state.turn,
                model: ctx.state.model.clone(),
                total_cost: ctx.state.total_session_cost,
                max_budget: ctx.state.max_budget_limit,
                prompt_tokens: ctx.state.total_prompt_tokens,
                completion_tokens: ctx.state.total_completion_tokens,
                cache_read_tokens: ctx.state.total_cache_read_tokens,
                cache_creation_tokens: ctx.state.total_cache_creation_tokens,
                continuation_anchor: ctx.state.continuation_anchor.clone(),
                queued_message: ctx.state.queued_message.clone(),
                diagnostics_context: ctx.state.diagnostics_context.clone(),
            });

            // Walk the committed history cells and pair them with
            // the trace's turn indices.  We use the cell ordering
            // (user/assistant pairs) as a rough proxy — the trace
            // doesn't emit a stable cell→turn_index mapping today,
            // so we populate by position.  Each turn contributes a
            // one-line preview for the collapsed view plus the
            // full body text for the drill-in view.
            let (previews, bodies) = collect_history_text(ctx.chat_widget);
            snap.history_previews = previews;
            snap.history_bodies = bodies;

            let breakdown = match ctx.state.observability_session.as_ref() {
                Some(session) => {
                    let guard = session.read().unwrap_or_else(|e| e.into_inner());
                    // Pull session-level compaction history into the
                    // snapshot so the Compaction section can show all
                    // past events, not just the last-turn trace.
                    snap.compressed_turns = guard.compressed_turns.clone();
                    match guard.context_traces.last() {
                        // Use the full assembly trace so the panel
                        // can render the nested tool / memory /
                        // skill / section rows under the top-level
                        // category bar. Old code only passed the
                        // scalar TokenBudgetTrace which lost that
                        // detail.
                        Some(trace) => ContextBreakdown::from_trace_with(trace, &snap),
                        None => ContextBreakdown::empty(),
                    }
                }
                None => ContextBreakdown::empty(),
            };
            ctx.bottom_pane
                .push_view(Box::new(ContextPanelView::new(breakdown)));
            SlashResult::Handled
        }

        // ── /config (TUI-native, matches the reference CLI) ──────────
        //
        // `/config` with no args opens the interactive panel directly —
        // this is the user's primary entry point, same as the reference
        // implementation's `/config` (aliased `/settings`).
        //
        // `/config edit` is kept as an alias for muscle memory / docs.
        //
        // Subcommands that only print static text (`show`, `paths`,
        // `diff`, `sources`, `export`) fall back to the line-mode
        // printer via `with_restored`. Those briefly tear down the TUI
        // which is acceptable for a print-and-done flow.
        "/config" if args.trim().is_empty() || args.trim() == "edit" => {
            use crate::tui::bottom_pane::config_edit_view::ConfigEditView;
            let cfg = astra_config::runtime_config::RuntimeConfig::load();
            ctx.bottom_pane
                .push_view(Box::new(ConfigEditView::new(cfg)));
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
            let _ = crate::tui::do_draw(ctx.guard, crate::tui::ActiveView::Empty, ctx.bottom_pane);

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
            use crate::tui::worktrees::{parse, WorktreeList};

            // `git worktree list --porcelain` on a blocking thread.
            let porcelain = tokio::task::spawn_blocking(|| {
                let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
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
                let sessions =
                    astra_services::session_workspace::list_sessions_by_git_root(&e.path, None, 50);
                e.session_count = sessions.len();
                e.last_session_at = sessions.first().map(|s| s.updated_at.clone());
            }

            if entries.is_empty() {
                ctx.show_info("No worktrees found (or `git worktree list` failed).".into());
                return SlashResult::Handled;
            }
            let list = WorktreeList::new(entries);
            ctx.bottom_pane
                .push_view(Box::new(WorktreesView::new(list)));
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

        // Deprecated commands — graceful hints
        "/turn" => {
            ctx.show_info("Use /timeline (Enter to drill into a turn).".into());
            SlashResult::Handled
        }
        "/verbose" | "/tuning" | "/experiment" => {
            ctx.show_info("Removed. Use /stats for metrics, /timeline for turn traces.".into());
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

        // ── Session ─────────────────────────────────────────────────
        //
        // Five subcommands + the default hub cover the overwhelming
        // majority of interactive usage.  Diagnostic subs (cleanup /
        // drift / errors / trace / verify / adaptive / switch) used
        // to live here too, but they duplicate functionality that
        // already exists elsewhere (diag tooling, /resume for
        // switch) and their output is text-only, so they fall
        // through to the line-mode printer instead.
        //
        //   /session                 → session hub (current overview)
        //   /session list            → session picker
        //   /session history [id]    → conversation history
        //   /session fork            → interactive fork flow
        //   /session analyze [id]    → counter-only diagnostics
        //   /session export [id]     → write markdown, echo path
        //   everything else          → line-mode fallback
        "/session" => {
            let trimmed = args.trim();
            if trimmed.is_empty() {
                return handle_session_hub(ctx);
            }
            let (sub, rest) = split_sub(trimmed);
            match sub {
                "list" => handle_session_list_view(ctx),
                "history" => handle_session_history_view(ctx, rest),
                "fork" => handle_session_fork_view(ctx),
                "analyze" | "diag" => handle_session_analyze_view(ctx, rest),
                "export" => handle_session_export_view(ctx, rest),
                _ => SlashResult::Fallback,
            }
        }

        // ── Copy last response ──────────────────────────────────────
        "/copy" => {
            match &ctx.state.last_response {
                Some(resp) if !resp.is_empty() => {
                    let n = resp.chars().count();
                    if crate::slash_info::copy_to_clipboard(resp) {
                        let preview: String = resp.chars().take(60).collect();
                        let suffix = if n > 60 { "…" } else { "" };
                        ctx.show_response(format!("Copied {n} chars: {preview}{suffix}"));
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
            ctx.show_response(format!("astra v{}", env!("CARGO_PKG_VERSION")));
            SlashResult::Handled
        }

        // /whoami is now folded into /session — redirect for muscle memory
        "/whoami" => handle_session_hub(ctx),

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
                        ctx.show_response(format!("Reloaded project instructions ({lines} lines)"));
                    } else {
                        ctx.state.project_instructions = None;
                        ctx.show_info("No .astra/instructions.md found".into());
                    }
                    SlashResult::Handled
                }
                "off" => {
                    ctx.state.project_instructions = None;
                    ctx.show_response("Project instructions disabled for this session".into());
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
            "/config",
            "interactive panel — search, pick, and edit runtime config",
            "↑↓ navigate · Enter edit · type to search · Esc save/close",
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
    state: &mut SessionState,
    bottom_pane: &mut BottomPane,
    chat_widget: &mut crate::tui::chat_widget::ChatWidget,
) {
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
        chat_widget.commit_system(SystemCell::info("Use /skill info <name> for details"));
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
        show_stats_view(sub, state, bottom_pane);
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
                chat_widget.commit_system(SystemCell::info(
                    "No project instructions loaded. Create .astra/instructions.md",
                ));
            }
            return;
        }
        "Reload" => {
            if let Some(instructions) = crate::project_instructions::discover_project_instructions()
            {
                let lc = instructions.lines().count();
                state.project_instructions = Some(instructions);
                chat_widget.commit_system(SystemCell::response(format!(
                    "Reloaded project instructions ({lc} lines)"
                )));
            } else {
                state.project_instructions = None;
                chat_widget.commit_system(SystemCell::info("No .astra/instructions.md found"));
            }
            return;
        }
        "Off" => {
            state.project_instructions = None;
            chat_widget.commit_system(SystemCell::response("Project instructions disabled"));
            return;
        }
        _ => {}
    }

    // Permission menu
    match name {
        "Auto" => {
            use crate::permission_manager::PermissionMode;
            state.perm_manager.set_mode(PermissionMode::Auto);
            chat_widget.commit_system(SystemCell::response("Permission mode → auto"));
            return;
        }
        "Prompt" => {
            use crate::permission_manager::PermissionMode;
            state.perm_manager.set_mode(PermissionMode::Prompt);
            chat_widget.commit_system(SystemCell::response("Permission mode → prompt"));
            return;
        }
        "Deny" => {
            use crate::permission_manager::PermissionMode;
            state.perm_manager.set_mode(PermissionMode::Deny);
            chat_widget.commit_system(SystemCell::response("Permission mode → deny"));
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
    crate::slash_config::set_active_model_for_display(Some(name.to_string()));
    bottom_pane.footer.model = Some(name.to_string());
    chat_widget.commit_system(SystemCell::response(format!("Set model to {name}")));
}

fn show_stats_view(sub: &str, state: &SessionState, bottom_pane: &mut BottomPane) {
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
            // Entity graph + pattern library panes removed along with the
            // self-evolution subsystem. Skill quality + drift metrics remain.
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

/// Split `"sub rest of args"` → `("sub", "rest of args")`.  Trims
/// both halves.  Used by slash commands that want a clean
/// `match sub { … }` without re-parsing with `split_whitespace`
/// everywhere.
fn split_sub(text: &str) -> (&str, &str) {
    let t = text.trim();
    match t.find(char::is_whitespace) {
        Some(pos) => (&t[..pos], t[pos..].trim()),
        None => (t, ""),
    }
}

// ── /model subcommand helpers ───────────────────────────────────

/// Sentinel prefix used by the `/session fork` picker to hand the
/// chosen parent session id back to the main input loop.  The
/// outer loop strips this prefix and runs `fork_local_session`
/// with the remainder.  Lives here (co-located with the dispatcher
/// that emits it) so `tui/mod.rs` can import it instead of
/// duplicating the literal and silently drifting.
pub(crate) const FORK_PICK_SENTINEL: &str = "__fork__\n";

/// Sentinel prefix emitted by the model-name picker.  The outer
/// loop (in `tui/mod.rs`) strips the prefix and decides whether to
/// commit immediately or push a second picker for the model's
/// thinking modes.  Kept public(crate) so the mod.rs arm can
/// strip it symmetrically with the other sentinels.
pub(crate) const MODEL_PICK_SENTINEL: &str = "__model_pick__\n";
/// Sentinel prefix for the thinking-mode picker. Payload format is
/// `__model_thinking__\n<base_model>\n<thinking_label>`.  The
/// handler composes `base + thinking_suffix_for(label)` and sets
/// `state.model`.
pub(crate) const MODEL_THINKING_SENTINEL: &str = "__model_thinking__\n";

/// `/model` with no args (or `list`) — fetch the catalog and push
/// the picker.  The picker emits `MODEL_PICK_SENTINEL + <name>`; the
/// outer loop then checks the model's `thinking_capability` and
/// either commits or pushes a thinking-mode picker.
/// True when an error string came from an HTTP 401 response.
/// Mirrors `chat_turn::is_auth_error` minus the LLM-provider escape — kept
/// local because slash dispatch only sees `fetch_model_list` errors which
/// never come from upstream model providers.
fn is_http_401(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    lower.contains("401 unauthorized")
        || lower.contains("status: 401")
        || lower.contains("status code: 401")
        || lower.contains("http 401")
        || lower.contains("unauthorized")
}

/// Build the model picker view from a fetched model list and push it.
fn push_model_picker(ctx: &mut DispatchContext<'_>, models: Vec<String>) {
    // Strip any `-thinking:*` suffix from the cached model when
    // highlighting the current row — the picker shows base names only,
    // and the suffix is re-applied by the thinking stage.
    let current_raw = ctx.state.model.clone().unwrap_or_default();
    let current_base = current_raw
        .split_once("-thinking:")
        .map(|(b, _)| b.to_string())
        .unwrap_or(current_raw);
    let items: Vec<SelectionItem> = models
        .into_iter()
        .map(|m| {
            let is_current = m == current_base;
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
        let view = ListSelectionView::new(items, Some("Select model:".into()))
            .with_result_prefix(MODEL_PICK_SENTINEL);
        ctx.bottom_pane.push_view(Box::new(view));
    }
}

async fn open_model_picker(ctx: &mut DispatchContext<'_>) -> SlashResult {
    let token = crate::session_runtime::current_access_token(ctx.profile);
    match crate::slash_router::fetch_model_list(ctx.api, token.as_deref()).await {
        Ok(models) => push_model_picker(ctx, models),
        Err(e) => {
            let msg = e.to_string();
            if is_http_401(&msg) {
                // Attempt silent token refresh + retry once. If the retry
                // itself fails with a non-auth error (e.g. 5xx after refresh),
                // surface that real error instead of the generic /login hint.
                if crate::session_runtime::attempt_token_refresh(ctx.api, ctx.profile).await {
                    let fresh = crate::session_runtime::current_access_token(ctx.profile);
                    match crate::slash_router::fetch_model_list(ctx.api, fresh.as_deref()).await {
                        Ok(models) => {
                            push_model_picker(ctx, models);
                            return SlashResult::Handled;
                        }
                        Err(retry_err) => {
                            let retry_msg = retry_err.to_string();
                            if !is_http_401(&retry_msg) {
                                ctx.show_error(format!(
                                    "Failed to fetch models: {}",
                                    retry_msg.lines().next().unwrap_or(&retry_msg)
                                ));
                                return SlashResult::Handled;
                            }
                            // Still 401 after refresh — fall through to /login hint.
                        }
                    }
                }
                ctx.show_error("Not authorized — try /login first".into());
            } else if msg.contains("connect") || msg.contains("timeout") {
                ctx.show_error("Cannot reach server — check connection".into());
            } else {
                ctx.show_error(format!(
                    "Failed to fetch models: {}",
                    msg.lines().next().unwrap_or(&msg)
                ));
            }
        }
    }
    SlashResult::Handled
}

/// `/model set <name>` — apply immediately.  Also used as the
/// fallback for `/model <name>` shorthand.
fn handle_model_set(ctx: &mut DispatchContext<'_>, name: &str) {
    let name = name.trim();
    if name.is_empty() {
        ctx.show_error("Model name cannot be empty — try `/model list`.".into());
        return;
    }
    ctx.state.model = Some(name.to_string());
    crate::slash_config::set_active_model_for_display(Some(name.to_string()));
    ctx.bottom_pane.footer.model = Some(name.to_string());
    ctx.show_response(format!("Set model to {name}"));
}

/// `/model clear` — unset the session override so the edge's
/// default model applies.  Reports the change to scrollback so
/// the user sees the footer switch.
async fn handle_model_clear(ctx: &mut DispatchContext<'_>) -> SlashResult {
    ctx.state.model = None;
    crate::slash_config::set_active_model_for_display(None);
    ctx.bottom_pane.footer.model = None;
    ctx.show_response("Model override cleared — using API default.".into());
    SlashResult::Handled
}

/// `/model info [name]` — push a read-only [`InfoView`] with the
/// model's metadata.  Without an explicit name, uses the
/// currently-selected model.
async fn handle_model_info(ctx: &mut DispatchContext<'_>, arg: &str) -> SlashResult {
    use crate::tui::bottom_pane::info_view::InfoView;

    let target = if arg.is_empty() {
        ctx.state.model.clone()
    } else {
        Some(arg.trim().to_string())
    };
    let Some(name) = target else {
        ctx.show_error("No active model — try `/model set <name>` or `/model list`.".into());
        return SlashResult::Handled;
    };

    // Prefer the cached pricing the session already carries so
    // `/model info` is instant — live refetch happens via
    // `/model list` when the user explicitly asks.
    let pricing = &ctx.state.cached_pricing;
    let prompt_usd = if pricing.prompt > 0.0 {
        format!("${:.3} / 1M tokens", pricing.prompt * 1_000_000.0)
    } else {
        "— (not cached)".into()
    };
    let completion_usd = if pricing.completion > 0.0 {
        format!("${:.3} / 1M tokens", pricing.completion * 1_000_000.0)
    } else {
        "— (not cached)".into()
    };
    let cache_read = pricing
        .cache_read
        .map(|v| format!("${:.3} / 1M", v * 1_000_000.0))
        .unwrap_or_else(|| "—".into());
    let cache_write = pricing
        .cache_write
        .map(|v| format!("${:.3} / 1M", v * 1_000_000.0))
        .unwrap_or_else(|| "—".into());

    let cumulative_tokens = ctx
        .state
        .total_prompt_tokens
        .saturating_add(ctx.state.total_completion_tokens);
    let mut pairs: Vec<(&str, String)> = vec![
        ("model", name.clone()),
        (
            "current",
            if ctx.state.model.as_deref() == Some(name.as_str()) {
                "yes".into()
            } else {
                "no (info for override target)".into()
            },
        ),
        ("prompt cost", prompt_usd),
        ("completion cost", completion_usd),
        ("cache read", cache_read),
        ("cache write", cache_write),
        (
            "session prompt tokens",
            fmt_tokens(ctx.state.total_prompt_tokens),
        ),
        (
            "session completion tokens",
            fmt_tokens(ctx.state.total_completion_tokens),
        ),
        (
            "session cache-read tokens",
            fmt_tokens(ctx.state.total_cache_read_tokens),
        ),
        (
            "session cache-creation tokens",
            fmt_tokens(ctx.state.total_cache_creation_tokens),
        ),
        ("session total tokens", fmt_tokens(cumulative_tokens)),
        (
            "session cost",
            format!("${:.4}", ctx.state.total_session_cost),
        ),
    ];
    if ctx.state.max_budget_limit > 0.0 {
        pairs.push((
            "budget limit",
            format!("${:.2}", ctx.state.max_budget_limit),
        ));
    }

    ctx.bottom_pane.push_view(Box::new(InfoView::from_key_value(
        &format!("Model · {name}"),
        pairs,
    )));
    SlashResult::Handled
}

fn fmt_tokens(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 999_950 {
        // Cut over to the "M" scale slightly before exactly 1_000_000
        // so boundary values (999_999) don't render as "1000.0k" —
        // the "1000.0k" suffix is wider than the 6-char column the
        // InfoView reserves and visually jarring next to "1.0M".
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}

// ── /session subcommand helpers ─────────────────────────────────

/// `/session` with no args — push the session hub with a
/// snapshot of the current session's vital stats and shortcut
/// hints for the common flows (list / history / context /
/// fork / export).
fn handle_session_hub(ctx: &mut DispatchContext<'_>) -> SlashResult {
    use crate::tui::bottom_pane::info_view::InfoView;
    use astra_services::session_workspace;

    let sid = ctx.state.session_id.clone().unwrap_or_default();
    let sid_short = if sid.len() > 8 {
        &sid[..8]
    } else {
        sid.as_str()
    };
    let model = ctx.state.model.clone().unwrap_or_else(|| "—".into());
    let cumulative_tokens = ctx
        .state
        .total_prompt_tokens
        .saturating_add(ctx.state.total_completion_tokens);

    let mut pairs: Vec<(&str, String)> = vec![
        (
            "session id",
            if sid.is_empty() {
                "— (no active session)".into()
            } else {
                sid.clone()
            },
        ),
        ("turn", ctx.state.turn.to_string()),
        ("model", model),
    ];

    // Workspace info (cwd, git, timestamps)
    let ws = (!sid.is_empty())
        .then(|| session_workspace::read_workspace(&sid).ok())
        .flatten();
    if let Some(ref ws) = ws {
        pairs.push(("cwd", tilde_session_path(&ws.cwd)));
        let git_line = match (&ws.git_branch, &ws.git_head) {
            (Some(b), Some(h)) => format!("{b} @ {}", &h[..h.len().min(8)]),
            (Some(b), None) => b.clone(),
            (None, Some(h)) => format!("@ {}", &h[..h.len().min(8)]),
            (None, None) => "—".into(),
        };
        pairs.push(("git", git_line));
        let started = ws.created_at.get(..19).unwrap_or(&ws.created_at);
        pairs.push(("started", started.to_string()));
        let saved = ws.updated_at.get(..19).unwrap_or(&ws.updated_at);
        pairs.push(("last saved", saved.to_string()));
        if ws.status != "active" {
            pairs.push(("status", ws.status.clone()));
        }
    } else {
        let cwd = std::env::current_dir()
            .map(|p| tilde_session_path(&p.to_string_lossy()))
            .unwrap_or_else(|_| "?".into());
        pairs.push(("cwd", cwd));
    }

    // Live state
    pairs.push(("cost", format!("${:.4}", ctx.state.total_session_cost)));
    if ctx.state.max_budget_limit > 0.0 {
        pairs.push(("budget", format!("${:.2}", ctx.state.max_budget_limit)));
    }
    pairs.push(("prompt tokens", fmt_tokens(ctx.state.total_prompt_tokens)));
    pairs.push((
        "completion tokens",
        fmt_tokens(ctx.state.total_completion_tokens),
    ));
    pairs.push((
        "cache-read tokens",
        fmt_tokens(ctx.state.total_cache_read_tokens),
    ));
    pairs.push(("total tokens", fmt_tokens(cumulative_tokens)));

    // Agent identity (from former /whoami)
    pairs.push(("permission", format!("{}", ctx.state.perm_manager.mode())));
    pairs.push(("explain", format!("{}", ctx.state.explain)));
    pairs.push(("skills", ctx.state.unified_skill_registry.len().to_string()));
    if !ctx.state.recent_tools.is_empty() {
        let tools: String = ctx
            .state
            .recent_tools
            .iter()
            .take(6)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        pairs.push(("recent tools", tools));
    }
    if let Some(ref rid) = ctx.state.run_id {
        pairs.push(("run_id", rid.clone()));
    }

    // Compaction + drift counters
    if let Some(obs) = ctx.state.observability_session.as_ref() {
        let guard = obs.read().unwrap_or_else(|e| e.into_inner());
        if !guard.compressed_turns.is_empty() {
            pairs.push(("compactions", guard.compressed_turns.len().to_string()));
        }
    }

    // Journal path
    if let Some(ref j) = ctx.state.journal {
        let jp = j.path().display().to_string();
        pairs.push(("journal", tilde_session_path(&jp)));
    }

    // Action cheatsheet
    pairs.push(("", String::new()));
    pairs.push(("/session list", "pick a session to resume".into()));
    pairs.push(("/session history", "scroll transcript".into()));
    pairs.push(("/timeline", "per-turn trace timeline".into()));
    pairs.push(("/context", "context panel".into()));
    pairs.push(("/session fork", "branch a parallel session".into()));
    pairs.push(("/session export", "write markdown transcript".into()));

    let title = if sid.is_empty() {
        "Session · no active session".to_string()
    } else {
        format!("Session · {sid_short}")
    };
    ctx.bottom_pane
        .push_view(Box::new(InfoView::from_key_value(&title, pairs)));
    SlashResult::Handled
}

fn tilde_session_path(abs: &str) -> String {
    let Some(home) = dirs::home_dir() else {
        return abs.to_string();
    };
    let home = home.to_string_lossy();
    let home = home.trim_end_matches('/');
    if abs == home {
        return "~".to_string();
    }
    let prefix = format!("{home}/");
    if let Some(rest) = abs.strip_prefix(&prefix) {
        return format!("~/{rest}");
    }
    abs.to_string()
}

/// `/session list` — same picker as `/resume`, but reached via
/// the session namespace so the registry reads naturally.  Empty
/// store → info message instead of a blank picker.
fn handle_session_list_view(ctx: &mut DispatchContext<'_>) -> SlashResult {
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

fn handle_session_history_view(ctx: &mut DispatchContext<'_>, arg: &str) -> SlashResult {
    let sid = resolve_session_arg(ctx, arg);
    let Some(sid) = sid else {
        return SlashResult::Handled;
    };
    let events = astra_services::session_journal::read_journal(&sid).unwrap_or_default();
    if events.is_empty() {
        ctx.show_info(format!("No journal events for session {sid}."));
        return SlashResult::Handled;
    }
    push_history_info(ctx, &sid, &events);
    SlashResult::Handled
}

/// `/session fork` — interactive parent picker.  On Enter the
/// picker emits `"__fork__\n<sid>"`; the outer loop recognises the
/// sentinel and runs `fork_local_session`.  No args short-circuits
/// through the picker; `/session fork <sid>` falls back to the
/// line-mode handler (covers scripted use).
fn handle_session_fork_view(ctx: &mut DispatchContext<'_>) -> SlashResult {
    use crate::tui::bottom_pane::session_picker_view::SessionPickerView;
    use crate::tui::session_picker::{FsSessionSource, SessionDiscovery};
    let disco = SessionDiscovery::new(FsSessionSource::new(), 50);
    if disco.total() == 0 {
        ctx.show_info("No previous sessions to fork from.".into());
        return SlashResult::Handled;
    }
    ctx.bottom_pane.push_view(Box::new(
        SessionPickerView::new(disco).with_result_prefix(FORK_PICK_SENTINEL),
    ));
    SlashResult::Handled
}

fn handle_session_analyze_view(ctx: &mut DispatchContext<'_>, arg: &str) -> SlashResult {
    // TUI-side analyze is a *summary* — counters only.  The full
    // textual diagnostic (latency spikes, per-turn token shape,
    // issue detection bullets) still lives in the line-mode
    // printer.  Users who want the full thing get it via
    // `/session analyze deep [id]` falling back through
    // `SlashResult::Fallback`; we propagate the optional session
    // id (`rest`) so the downstream handler can see
    // `/session analyze deep <id>` verbatim.
    let (flag, rest) = split_sub(arg);
    if flag == "deep" {
        // Expose the trailing id (if any) through a thread-local so
        // the line-mode analyzer can recover the user's original
        // intent without re-parsing the slash string.
        let rest = rest.trim();
        if !rest.is_empty() {
            crate::slash_config::set_deep_analyze_arg(Some(rest.to_string()));
        } else {
            crate::slash_config::set_deep_analyze_arg(None);
        }
        return SlashResult::Fallback;
    }
    let Some(sid) = resolve_session_arg(ctx, arg) else {
        return SlashResult::Handled;
    };
    push_analyze_summary(ctx, &sid);
    SlashResult::Handled
}

/// Counter-only summary of a session journal — fast to compute
/// (no workspace reads, no per-event allocation) so the InfoView
/// renders instantly.  Users who want the deep report still get
/// it via `/session analyze deep [id]`.
fn push_analyze_summary(ctx: &mut DispatchContext<'_>, sid: &str) {
    use crate::tui::bottom_pane::info_view::InfoView;
    use astra_services::session_journal::JournalEventType;

    let events = match astra_services::session_journal::read_journal(sid) {
        Ok(e) => e,
        Err(e) => {
            ctx.show_error(format!("Failed to read journal: {e}"));
            return;
        }
    };
    if events.is_empty() {
        ctx.show_info(format!("Session {sid} has no journal events."));
        return;
    }

    let mut turns = 0u32;
    let mut errors = 0u32;
    let mut stalls = 0u32;
    let mut checkpoints = 0u32;
    let mut compactions = 0u32;
    let mut prompt_tokens = 0u64;
    let mut completion_tokens = 0u64;
    let mut cache_read_tokens = 0u64;
    let mut cache_creation_tokens = 0u64;
    let mut first_ts: Option<String> = None;
    let mut last_ts: Option<String> = None;
    for ev in &events {
        if first_ts.is_none() {
            first_ts = Some(ev.ts.clone());
        }
        last_ts = Some(ev.ts.clone());
        match ev.event_type {
            JournalEventType::Turn => turns += 1,
            JournalEventType::TurnError | JournalEventType::Error => errors += 1,
            JournalEventType::StallDetected => stalls += 1,
            JournalEventType::Checkpoint => checkpoints += 1,
            JournalEventType::Compact => compactions += 1,
            _ => {}
        }
        if let Some(t) = ev.tokens_in {
            prompt_tokens = prompt_tokens.saturating_add(t);
        }
        if let Some(t) = ev.tokens_out {
            completion_tokens = completion_tokens.saturating_add(t);
        }
        if let Some(t) = ev.cache_read_tokens {
            cache_read_tokens = cache_read_tokens.saturating_add(t);
        }
        if let Some(t) = ev.cache_creation_tokens {
            cache_creation_tokens = cache_creation_tokens.saturating_add(t);
        }
    }
    let total_tokens = prompt_tokens.saturating_add(completion_tokens);

    let mut pairs: Vec<(&str, String)> = Vec::new();
    pairs.push(("session id", sid.to_string()));
    if let Some(ts) = first_ts {
        pairs.push(("started", ts));
    }
    if let Some(ts) = last_ts {
        pairs.push(("last event", ts));
    }
    pairs.push(("", String::new()));
    pairs.push(("turns", turns.to_string()));
    pairs.push(("errors", errors.to_string()));
    pairs.push(("stalls detected", stalls.to_string()));
    pairs.push(("checkpoints", checkpoints.to_string()));
    pairs.push(("compactions", compactions.to_string()));
    pairs.push(("", String::new()));
    pairs.push(("prompt tokens", fmt_tokens(prompt_tokens)));
    pairs.push(("completion tokens", fmt_tokens(completion_tokens)));
    pairs.push(("cache-read tokens", fmt_tokens(cache_read_tokens)));
    pairs.push(("cache-creation tokens", fmt_tokens(cache_creation_tokens)));
    pairs.push(("total tokens", fmt_tokens(total_tokens)));
    pairs.push(("", String::new()));
    pairs.push((
        "deep report",
        "`/session analyze deep <id>` prints the full diagnostic".into(),
    ));

    let sid_short = if sid.len() > 8 { &sid[..8] } else { sid };
    ctx.bottom_pane.push_view(Box::new(InfoView::from_key_value(
        &format!("Session analyze · {sid_short}"),
        pairs,
    )));
}

fn handle_session_export_view(ctx: &mut DispatchContext<'_>, arg: &str) -> SlashResult {
    let Some(sid) = resolve_session_arg(ctx, arg) else {
        return SlashResult::Handled;
    };
    let events = match astra_services::session_journal::read_journal(&sid) {
        Ok(events) => events,
        Err(e) => {
            ctx.show_error(format!("Failed to read journal: {e}"));
            return SlashResult::Handled;
        }
    };
    if events.is_empty() {
        ctx.show_info(format!("Session {sid} has no journal events to export."));
        return SlashResult::Handled;
    }
    let md = crate::slash_session::build_export_markdown(&sid, &events);
    let now = chrono::Local::now();
    // Default path mirrors the legacy line-mode exporter so users
    // with scripts expecting that filename shape keep working.
    let path = format!("astra-session-{}.md", now.format("%Y%m%d-%H%M"));
    match std::fs::write(&path, &md) {
        Ok(_) => ctx.show_response(format!("Exported {sid} → {path}")),
        Err(e) => ctx.show_error(format!("Failed to write {path}: {e}")),
    }
    SlashResult::Handled
}

/// Resolve a user-supplied session id (or default-to-current)
/// and surface a helpful error on the common failure modes.
fn resolve_session_arg(ctx: &mut DispatchContext<'_>, arg: &str) -> Option<String> {
    if arg.is_empty() {
        match ctx.state.session_id.clone() {
            Some(id) if !id.is_empty() => Some(id),
            _ => {
                ctx.show_error("No active session — try `/session list` first.".into());
                None
            }
        }
    } else {
        Some(arg.trim().to_string())
    }
}

/// Render a session's conversation history into an InfoView.
/// Keeps the logic tight: titles, user → assistant pairing,
/// truncated previews.  Heavier browsing (drill, scroll) can be
/// added later with a dedicated view.
fn push_history_info(
    ctx: &mut DispatchContext<'_>,
    sid: &str,
    events: &[astra_services::session_journal::JournalEvent],
) {
    use crate::tui::bottom_pane::info_view::InfoView;
    use astra_services::session_journal::JournalEventType;
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};

    let dim = Style::default().fg(Color::DarkGray);
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let role_user = Style::default().fg(crate::tui::theme::current().accent);
    let role_assistant = Style::default().fg(Color::Green);

    let mut lines: Vec<Line<'static>> = Vec::new();
    for ev in events {
        match ev.event_type {
            JournalEventType::Turn => {
                if let Some(user) = &ev.user_input {
                    lines.push(Line::from(vec![
                        Span::styled(format!("  #{turn} ", turn = ev.turn.unwrap_or(0)), dim),
                        Span::styled("user", role_user),
                    ]));
                    for row in truncate_rows(user, 3) {
                        lines.push(Line::from(Span::styled(format!("    {row}"), bold)));
                    }
                }
                if let Some(out) = &ev.assistant_output {
                    lines.push(Line::from(Span::styled("       assistant", role_assistant)));
                    for row in truncate_rows(out, 3) {
                        lines.push(Line::from(Span::raw(format!("    {row}"))));
                    }
                }
                lines.push(Line::default());
            }
            JournalEventType::Compact => {
                lines.push(Line::from(Span::styled(
                    format!("  ⚠ compaction at turn {}", ev.turn.unwrap_or(0)),
                    Style::default().fg(Color::Yellow),
                )));
            }
            _ => {}
        }
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no user/assistant turns recorded in this journal)",
            dim,
        )));
    }
    let sid_short = if sid.len() > 8 { &sid[..8] } else { sid };
    ctx.bottom_pane.push_view(Box::new(InfoView::new(
        format!("Session history · {sid_short}"),
        lines,
    )));
}

/// Split `text` into `max` short rows. Trims whitespace, drops
/// empty lines, caps each row at 76 chars.  Keeps the InfoView
/// dense without wrapping surprises.
fn truncate_rows(text: &str, max: usize) -> Vec<String> {
    let mut rows: Vec<String> = Vec::new();
    if max == 0 {
        return rows;
    }
    let total_non_blank = text.lines().filter(|l| !l.trim().is_empty()).count();
    let mut seen = 0usize;
    for logical in text.lines() {
        let trimmed = logical.trim();
        if trimmed.is_empty() {
            continue;
        }
        seen += 1;
        // Reserve the last slot for an overflow marker only when
        // there is still *further* non-blank content after the
        // current line.  `seen` counts lines already processed
        // (including this one) so `total_non_blank - seen` is the
        // true remainder.  This keeps the final allowed row when
        // the source has exactly `max` non-blank lines instead of
        // prematurely ellipsing it.
        let remaining_after = total_non_blank.saturating_sub(seen);
        if rows.len() + 1 >= max && remaining_after > 0 {
            rows.push("…".into());
            break;
        }
        if trimmed.chars().count() > 76 {
            let short: String = trimmed.chars().take(75).collect();
            rows.push(format!("{short}…"));
        } else {
            rows.push(trimmed.to_string());
        }
    }
    rows
}

fn parse_slash(text: &str) -> (&str, &str) {
    let text = text.trim();
    match text.find(' ') {
        Some(pos) => (&text[..pos], text[pos..].trim()),
        None => (text, ""),
    }
}

/// Render a filesystem path for the `/context` Environment row.
/// Replaces `$HOME` with `~` so absolute paths stay short.
fn display_path(path: &std::path::Path) -> String {
    let s = path.display().to_string();
    if let Ok(home) = std::env::var("HOME") {
        if let Some(rest) = s.strip_prefix(&home) {
            return format!("~{rest}");
        }
    }
    s
}

/// Detect current git branch via `gix`. Returns `None` when the cwd
/// isn't a git repo, in detached HEAD, or on any I/O error — in any
/// of those cases the Environment row falls back to just the cwd.
fn detect_git_branch() -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    let repo = gix::discover(cwd).ok()?;
    let head = repo.head().ok()?;
    let name = head.referent_name()?;
    Some(name.shorten().to_string())
}

/// Locate the user-rules directory under `~/.astra/rules/`, if
/// present.  Returns the home-shortened path via `display_path`
/// so the snapshot already reads like `~/.astra/rules`.  `None`
/// when the directory doesn't exist.
fn find_user_rules_path() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let p = std::path::PathBuf::from(&home).join(".astra/rules");
    if p.exists() {
        return Some(display_path(&p));
    }
    None
}

/// Build a `turn_index → one-line preview` map from the chat
/// widget's committed scrollback.  Turn indices come from the
/// trace side of the API, but the trace doesn't store text — we
/// use the cell position within each user-turn as the index.
///
/// The mapping is heuristic (cells don't carry a turn id) but
/// matches the common case: each user/assistant pair is one turn.
fn collect_history_text(
    chat: &crate::tui::chat_widget::ChatWidget,
) -> (
    std::collections::HashMap<u32, String>,
    std::collections::HashMap<u32, String>,
) {
    use crate::tui::history_cell::{
        assistant::AssistantCell, reasoning::ReasoningCell, user::UserCell,
    };
    let mut previews: std::collections::HashMap<u32, String> = std::collections::HashMap::new();
    let mut bodies: std::collections::HashMap<u32, String> = std::collections::HashMap::new();
    // Walk history cells; each user cell advances the turn index.
    // For both preview (one-line) and body (full text) we follow
    // the same priority: user message wins; otherwise assistant
    // reply for that turn; otherwise reasoning text as last resort.
    let mut turn_idx: u32 = 0;
    let record = |idx: u32,
                  text: &str,
                  previews: &mut std::collections::HashMap<u32, String>,
                  bodies: &mut std::collections::HashMap<u32, String>,
                  force: bool| {
        if text.trim().is_empty() {
            return;
        }
        if force || !previews.contains_key(&idx) {
            let p = one_line_preview(text);
            if !p.is_empty() {
                previews.insert(idx, p);
            }
        }
        if force || !bodies.contains_key(&idx) {
            bodies.insert(idx, text.to_string());
        }
    };
    for cell in chat.history() {
        let any = cell.as_any_ref();
        if let Some(u) = any.downcast_ref::<UserCell>() {
            record(turn_idx, u.text(), &mut previews, &mut bodies, true);
            turn_idx = turn_idx.saturating_add(1);
        } else if let Some(a) = any.downcast_ref::<AssistantCell>() {
            if turn_idx == 0 {
                continue;
            }
            let slot = turn_idx.saturating_sub(1);
            record(slot, a.source(), &mut previews, &mut bodies, false);
        } else if let Some(r) = any.downcast_ref::<ReasoningCell>() {
            if turn_idx == 0 {
                continue;
            }
            let slot = turn_idx.saturating_sub(1);
            record(slot, r.text(), &mut previews, &mut bodies, false);
        }
    }
    (previews, bodies)
}

fn one_line_preview(text: &str) -> String {
    text.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .chars()
        .take(200)
        .collect()
}

/// `/context dump [path]` — write a full JSON snapshot of the
/// context-panel state (trace + chat history + environment) to
/// disk for sharing or forensic replay.  When `path` is empty,
/// writes to `~/.astra/context-dumps/<session>-<turn>-<ts>.json`.
///
/// Kept inline so the user sees the output path as a normal
/// scrollback cell instead of tearing down the TUI like the
/// fallback printer would.
fn handle_context_dump(arg: &str, ctx: &mut DispatchContext<'_>) -> SlashResult {
    use crate::tui::history_cell::system::SystemCell;
    let chat_history = crate::tui::collect_chat_turns_for_dump(ctx.chat_widget);
    let path = match crate::context_dump::write_dump_for_repl(
        ctx.state,
        chat_history,
        if arg.is_empty() { None } else { Some(arg) },
    ) {
        Ok(p) => p,
        Err(e) => {
            ctx.chat_widget
                .commit_system(SystemCell::error(format!("/context dump failed: {e}")));
            return SlashResult::Handled;
        }
    };
    ctx.chat_widget.commit_system(SystemCell::info(format!(
        "Context snapshot written to {}",
        path.display()
    )));
    SlashResult::Handled
}

#[cfg(test)]
mod panels_tests {
    use super::build_panels_cheat_sheet_lines;

    #[test]
    fn cheat_sheet_lists_every_tui_native_panel() {
        let text = build_panels_cheat_sheet_lines().join("\n");
        for cmd in ["/resume", "/context", "/timeline", "/table", "/worktrees"] {
            assert!(text.contains(cmd), "cheat sheet missing {cmd}; got: {text}");
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

#[cfg(test)]
mod context_history_tests {
    use super::{collect_history_text, one_line_preview};
    use crate::tui::chat_widget::ChatWidget;
    use crate::tui::turn_event::{SystemLevel, TurnEvent};

    #[test]
    fn one_line_preview_skips_leading_blanks_crlf_and_truncates() {
        assert_eq!(one_line_preview("\n\n  first line  \nsecond"), "first line");
        assert_eq!(one_line_preview("\r\nCRLF line\r\n"), "CRLF line");

        let long = "a".repeat(240);
        let preview = one_line_preview(&long);
        assert_eq!(preview.chars().count(), 200);
        assert!(preview.chars().all(|ch| ch == 'a'));
    }

    #[test]
    fn collect_history_text_ignores_assistant_without_user_anchor() {
        let mut chat = ChatWidget::new("");
        chat.replay(vec![
            TurnEvent::Assistant {
                ts: None,
                markdown: "orphan assistant should not attach to turn zero".into(),
            },
            TurnEvent::Thinking {
                ts: None,
                text: "orphan reasoning should not attach to turn zero".into(),
                duration_ms: None,
            },
        ]);

        let (previews, bodies) = collect_history_text(&chat);

        assert!(
            previews.is_empty() && bodies.is_empty(),
            "assistant/reasoning cells before the first user turn must not create a fake turn 0"
        );
    }

    #[test]
    fn collect_history_text_skips_system_cells_and_keeps_turn_indices_aligned() {
        let mut chat = ChatWidget::new("");
        chat.replay(vec![
            TurnEvent::User {
                ts: None,
                text: "first user".into(),
            },
            TurnEvent::System {
                ts: None,
                level: SystemLevel::Info,
                text: "system note should not appear in context history".into(),
            },
            TurnEvent::Assistant {
                ts: None,
                markdown: "assistant answer".into(),
            },
            TurnEvent::User {
                ts: None,
                text: "second user".into(),
            },
        ]);

        let (previews, bodies) = collect_history_text(&chat);

        assert_eq!(previews.get(&0).map(String::as_str), Some("first user"));
        assert_eq!(bodies.get(&0).map(String::as_str), Some("first user"));
        assert_eq!(previews.get(&1).map(String::as_str), Some("second user"));
        assert_eq!(bodies.get(&1).map(String::as_str), Some("second user"));
        assert!(
            previews
                .values()
                .chain(bodies.values())
                .all(|text| !text.contains("system note")),
            "system cells should not pollute turn previews or bodies"
        );
    }
}

#[cfg(test)]
mod view_result_tests {
    use super::handle_view_result;
    use crate::permission_manager::PermissionMode;
    use crate::session_state::SessionState;
    use crate::tui::bottom_pane::BottomPane;
    use crate::tui::chat_widget::ChatWidget;
    use crate::tui::history_cell::system::SystemCell;

    fn last_system_message(widget: &ChatWidget) -> Option<String> {
        widget
            .history()
            .last()
            .and_then(|cell| cell.as_any_ref().downcast_ref::<SystemCell>())
            .map(|cell| cell.message().to_string())
    }

    #[test]
    fn session_picker_result_is_reserved_for_outer_resume_pipeline() {
        let mut state = SessionState::default();
        let mut bottom_pane = BottomPane::new();
        let mut chat_widget = ChatWidget::new("");

        handle_view_result(
            "sess_1234567890",
            &mut state,
            &mut bottom_pane,
            &mut chat_widget,
        );

        assert!(chat_widget.history().is_empty());
        assert!(bottom_pane.composer.is_empty());
        assert_eq!(state.model, None);
    }

    #[test]
    fn permission_selection_updates_state_and_commits_feedback() {
        let mut state = SessionState::default();
        let mut bottom_pane = BottomPane::new();
        let mut chat_widget = ChatWidget::new("");

        handle_view_result("Auto", &mut state, &mut bottom_pane, &mut chat_widget);

        assert_eq!(state.perm_manager.mode(), PermissionMode::Auto);
        assert_eq!(
            last_system_message(&chat_widget).as_deref(),
            Some("Permission mode → auto")
        );
    }

    #[test]
    fn slash_command_selection_returns_command_to_composer() {
        let mut state = SessionState::default();
        let mut bottom_pane = BottomPane::new();
        let mut chat_widget = ChatWidget::new("");

        handle_view_result("/resume", &mut state, &mut bottom_pane, &mut chat_widget);

        assert_eq!(bottom_pane.composer.text(), "/resume ");
        assert!(
            chat_widget.history().is_empty(),
            "selecting a command hint should not emit scrollback noise"
        );
    }

    #[test]
    fn model_selection_updates_footer_and_commits_feedback() {
        let mut state = SessionState::default();
        let mut bottom_pane = BottomPane::new();
        let mut chat_widget = ChatWidget::new("");

        handle_view_result(
            "claude-sonnet-4.6",
            &mut state,
            &mut bottom_pane,
            &mut chat_widget,
        );

        assert_eq!(state.model.as_deref(), Some("claude-sonnet-4.6"));
        assert_eq!(
            bottom_pane.footer.model.as_deref(),
            Some("claude-sonnet-4.6")
        );
        assert_eq!(
            last_system_message(&chat_widget).as_deref(),
            Some("Set model to claude-sonnet-4.6")
        );
    }
}

#[cfg(test)]
mod split_sub_tests {
    use super::split_sub;

    #[test]
    fn split_sub_no_whitespace_returns_empty_rest() {
        assert_eq!(split_sub("info"), ("info", ""));
    }

    #[test]
    fn split_sub_single_space() {
        assert_eq!(split_sub("set gpt-4"), ("set", "gpt-4"));
    }

    #[test]
    fn split_sub_trims_both_halves() {
        assert_eq!(split_sub("  set   gpt-4  "), ("set", "gpt-4"));
    }

    #[test]
    fn split_sub_empty_input() {
        assert_eq!(split_sub(""), ("", ""));
    }

    #[test]
    fn split_sub_preserves_multi_word_rest() {
        assert_eq!(
            split_sub("analyze deep abc-123"),
            ("analyze", "deep abc-123")
        );
    }
}

#[cfg(test)]
mod fmt_tokens_tests {
    use super::fmt_tokens;

    #[test]
    fn fmt_tokens_handles_all_magnitudes() {
        assert_eq!(fmt_tokens(0), "0");
        assert_eq!(fmt_tokens(42), "42");
        assert_eq!(fmt_tokens(1_200), "1.2k");
        assert_eq!(fmt_tokens(999_999), "1.0M");
        assert_eq!(fmt_tokens(1_500_000), "1.5M");
    }
}

#[cfg(test)]
mod truncate_rows_tests {
    use super::truncate_rows;

    #[test]
    fn truncate_rows_drops_blank_lines() {
        let rows = truncate_rows("\n\nfirst\n\nsecond\n", 3);
        assert_eq!(rows, vec!["first".to_string(), "second".to_string()]);
    }

    #[test]
    fn truncate_rows_adds_ellipsis_when_truncating() {
        let input = "a\nb\nc\nd\ne";
        let rows = truncate_rows(input, 3);
        assert_eq!(
            rows,
            vec!["a".to_string(), "b".to_string(), "…".to_string()]
        );
    }

    #[test]
    fn truncate_rows_caps_long_single_line_at_76() {
        let long = "x".repeat(100);
        let rows = truncate_rows(&long, 2);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].ends_with('…'));
        assert_eq!(rows[0].chars().count(), 76);
    }

    #[test]
    fn truncate_rows_empty_input_produces_empty_output() {
        assert!(truncate_rows("", 5).is_empty());
    }
}
