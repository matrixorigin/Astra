//! TUI-native slash command dispatch.
//!
//! Each command is handled inline without leaving the TUI. Commands that
//! need complex interactive UI push a BottomPaneView. Commands that only
//! produce output render to scrollback. Unrecognized or complex commands
//! fall back to `with_restored()` which temporarily exits the TUI.

use crate::cli::command_registry;
use crate::cli::command_registry::TuiHandler;
use crate::cli::session::session_state::ExplainMode;
use crate::cli::session::session_state::SessionState;
use crate::tui::bottom_pane::BottomPane;
use crate::tui::bottom_pane::list_selection_view::{ListSelectionView, SelectionItem};
use crate::tui::bottom_pane::view::BottomPaneView;
use crate::tui::history_cell::system::SystemCell;
use crate::tui::terminal::TerminalGuard;

pub(crate) enum SlashResult {
    Handled,
    Deferred,
    Exit,
    Fallback,
    /// Forward the raw slash command text to the chat composer for the user
    /// to review and send as a normal message (ChatForward handler).
    Forward(String),
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

    fn open_view(&mut self, msg: impl Into<String>, view: Box<dyn BottomPaneView>) {
        self.show_response(msg.into());
        self.bottom_pane.push_view(view);
    }

    fn open_deferred_view(&mut self, msg: impl Into<String>, view: Box<dyn BottomPaneView>) {
        self.show_response(msg.into());
        self.bottom_pane.push_view(view);
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
                // No registry match → use the shared fuzzy scorer to surface
                // the top-3 closest known commands as a "did you mean?" hint.
                // Keeps the suggestion UX aligned with the slash popup.
                let needle = cmd.trim_start_matches('/');
                let mut scored: Vec<(u32, &'static str)> = crate::cli::command_registry::COMMANDS
                    .iter()
                    .filter(|m| !m.is_alias && !m.name.contains(' '))
                    .filter_map(|m| {
                        let name = m.name.trim_start_matches('/');
                        crate::tui::score_slash_token(needle, name).map(|s| (s, m.name))
                    })
                    .collect();
                scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
                let hints: Vec<&'static str> = scored.into_iter().take(3).map(|(_, n)| n).collect();
                if hints.is_empty() {
                    ctx.show_error(format!(
                        "Unknown command: {cmd}  ·  type / to browse all commands"
                    ));
                } else {
                    ctx.show_error(format!(
                        "Unknown command: {cmd}  ·  did you mean: {}?",
                        hints.join(", ")
                    ));
                }
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
            ctx.open_view("Opened command help", Box::new(HelpView::new()));
            SlashResult::Handled
        }

        // ── Auth forms (inline TUI card instead of dropping out to
        //    bare-terminal prompts that looked disjoint and stole keys) ─
        "/login" => {
            use crate::tui::bottom_pane::login_view::{LoginMode, LoginView};
            ctx.open_deferred_view("Opened login", Box::new(LoginView::new(LoginMode::Login)));
            SlashResult::Deferred
        }
        "/register" => {
            use crate::tui::bottom_pane::login_view::{LoginMode, LoginView};
            ctx.open_deferred_view(
                "Opened registration",
                Box::new(LoginView::new(LoginMode::Register)),
            );
            SlashResult::Deferred
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

        "/mcp" => handle_mcp_dispatch(args, ctx).await,

        "/plan" => {
            let trimmed = args.trim();
            if !trimmed.is_empty() {
                return SlashResult::Fallback;
            }

            if crate::cli::plan::plan_lifecycle::looks_like_pending_local_plan_entry(ctx.state) {
                crate::cli::slash::slash_plan::exit_local_plan_mode(ctx.state);
                return SlashResult::Handled;
            }

            if ctx.state.cloud_plan_mirror.is_some() {
                let Some(token) =
                    crate::cli::plan::plan_lifecycle::fresh_token_for_plan(ctx.api, ctx.profile)
                        .await
                else {
                    ctx.show_error("Not logged in. Use /login.".into());
                    return SlashResult::Handled;
                };
                if let Err(error) = crate::cli::plan::plan_lifecycle::exit_remote_plan_mode(
                    ctx.api, &token, ctx.state, true,
                )
                .await
                {
                    ctx.show_error(error);
                    return SlashResult::Handled;
                }
                ctx.state
                    .perm_manager
                    .set_mode(crate::cli::permission_manager::PermissionMode::Auto);
                return SlashResult::Handled;
            }

            let Some(_token) =
                crate::cli::plan::plan_lifecycle::fresh_token_for_plan(ctx.api, ctx.profile).await
            else {
                ctx.show_error("Not logged in. Use /login.".into());
                return SlashResult::Handled;
            };
            crate::cli::slash::slash_plan::enter_local_plan_mode(ctx.state);
            SlashResult::Handled
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
            ctx.open_view("Opened stats picker", Box::new(view));
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
            ctx.open_view("Opened skills picker", Box::new(view));
            SlashResult::Handled
        }

        // ── Allow / permission mode ─────────────────────────────────
        "/allow" => {
            use crate::cli::permission_command::{
                PERMISSION_COMMAND_USAGE, PermissionCommandAction, parse_permission_command,
            };

            match parse_permission_command(args) {
                PermissionCommandAction::Cycle => {
                    ctx.open_view(
                        "Opened permission mode picker",
                        Box::new(build_permission_mode_picker(ctx.state.perm_manager.mode())),
                    );
                    SlashResult::Handled
                }
                PermissionCommandAction::SetMode(mode) => {
                    ctx.state.perm_manager.set_mode(mode);
                    ctx.show_response(permission_mode_feedback(mode));
                    SlashResult::Handled
                }
                PermissionCommandAction::ShowRules => {
                    use crate::tui::bottom_pane::info_view::InfoView;
                    let summary = ctx.state.perm_manager.rules_summary();
                    ctx.open_view(
                        "Opened permission rules",
                        Box::new(InfoView::from_plain(
                            "Permission Rules",
                            summary.lines().map(|l| l.to_string()).collect(),
                        )),
                    );
                    SlashResult::Handled
                }
                PermissionCommandAction::TrustWorkspace => {
                    match ctx.state.perm_manager.trust_workspace() {
                        Ok(message) => ctx.show_response(message),
                        Err(err) => {
                            ctx.show_error(format!("Failed to trust workspace: {err}"));
                        }
                    }
                    SlashResult::Handled
                }
                PermissionCommandAction::UntrustWorkspace => {
                    match ctx.state.perm_manager.untrust_workspace() {
                        Ok(message) => ctx.show_response(message),
                        Err(err) => {
                            ctx.show_error(format!("Failed to mark workspace untrusted: {err}"));
                        }
                    }
                    SlashResult::Handled
                }
                PermissionCommandAction::ShowTrace => {
                    use crate::tui::bottom_pane::info_view::InfoView;
                    let lines = astra_turn_core::permission::audit::format_snapshot_lines(50);
                    ctx.open_view(
                        "Opened permission trace",
                        Box::new(InfoView::from_plain("Permission Trace", lines)),
                    );
                    SlashResult::Handled
                }
                PermissionCommandAction::ExportTrace(path) => {
                    let lines = astra_turn_core::permission::audit::snapshot_redacted_jsonl_lines();
                    let body = if lines.is_empty() {
                        String::new()
                    } else {
                        format!("{}\n", lines.join("\n"))
                    };
                    match std::fs::write(path, body) {
                        Ok(()) => ctx.show_response(format!("Permission trace exported to {path}")),
                        Err(err) => {
                            ctx.show_error(format!("Failed to export permission trace: {err}"));
                        }
                    }
                    SlashResult::Handled
                }
                PermissionCommandAction::MissingTraceExport => {
                    ctx.show_error("Missing export path".to_string());
                    SlashResult::Handled
                }
                PermissionCommandAction::Unknown(args) => {
                    ctx.show_error(format!(
                        "Unknown mode '{args}'. Use: {PERMISSION_COMMAND_USAGE}"
                    ));
                    SlashResult::Handled
                }
            }
        }

        // ── State commands → with_restored (share full logic with non-TUI) ──
        "/clear" | "/undo" | "/redo" | "/compact" | "/reflect" => SlashResult::Fallback,

        // ── Explain ─────────────────────────────────────────────────
        "/explain" => {
            ctx.state.explain = match ctx.state.explain {
                ExplainMode::Off => ExplainMode::On,
                ExplainMode::On => ExplainMode::Verbose,
                ExplainMode::Verbose => ExplainMode::Off,
            };
            let label = match ctx.state.explain {
                ExplainMode::Off => "off",
                ExplainMode::On => "on",
                ExplainMode::Verbose => "verbose",
            };
            ctx.show_response(format!("Explain mode: {label}"));
            SlashResult::Handled
        }

        // ── Inspect (TUI-native) ────────────────────────────────────
        //
        // Routes to the harness snapshot for session introspection.
        //   `/inspect`                → subcommand picker
        //   `/inspect budget`         → token budget breakdown
        //   `/inspect tools`          → tool call dashboard
        //   `/inspect context`        → context window snapshot
        //   `/inspect cache`          → per-round cache diagnosis
        //   `/inspect json`           → raw snapshot as JSON
        //   `/inspect diff`           → state diff vs start-of-session
        //   `/inspect history [N]`    → recent turn history
        //   `/inspect trace`          → permission trace
        //   `/inspect forensics`      → forensics dump
        //   `/inspect export [path]`  → export to file
        "/inspect" => {
            return handle_inspect_dispatch(args, ctx).await;
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
                ctx.show_info(CONTEXT_USAGE_MESSAGE.into());
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
            snap.selected_skills = ctx
                .state
                .last_turn_event
                .as_ref()
                .and_then(|event| event.selected_skills.clone())
                .unwrap_or_default();

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
                    let guard = astra_core::sync_poison::recover_rwlock_read(&session);
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
            ctx.open_view(
                "Opened context panel",
                Box::new(ContextPanelView::new(breakdown)),
            );
            SlashResult::Handled
        }

        // ── /config (panel for edit, text fallback for read-only forms) ──
        "/config" => match config_command_route(args) {
            Ok(ConfigCommandRoute::Panel) => {
                use crate::tui::bottom_pane::config_edit_view::ConfigEditView;
                let cfg = astra_config::runtime_config::RuntimeConfig::load();
                ctx.open_view("Opened config editor", Box::new(ConfigEditView::new(cfg)));
                SlashResult::Handled
            }
            Ok(ConfigCommandRoute::Fallback) => SlashResult::Fallback,
            Err(usage) => {
                ctx.show_error(usage.to_string());
                SlashResult::Handled
            }
        },

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
            let _ = crate::tui::do_draw(
                ctx.guard,
                crate::tui::ActiveView::Empty,
                None,
                ctx.bottom_pane,
                None,
                None,
            );

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
                    ctx.open_view("Opened table results", Box::new(TablePanelView::new(table)));
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
            ctx.open_view(
                "Opened panels cheat sheet",
                Box::new(InfoView::from_plain("TUI panels", body).with_reopen("/panels")),
            );
            SlashResult::Handled
        }

        // ── Worktrees (TUI-native) ──────────────────────────────────
        "/worktrees" => {
            use crate::tui::bottom_pane::worktrees_view::WorktreesView;
            use crate::tui::worktrees::{WorktreeList, parse};

            // Both the `git worktree list --porcelain` invocation AND the
            // per-entry session-count enrichment do blocking filesystem IO
            // (process exec, journal scans, workspace YAML reads). Run the
            // whole bundle on a blocking thread — keeping any portion of
            // it on the runtime thread freezes the TUI on filesystems
            // with tens of sessions per worktree.
            let entries = tokio::task::spawn_blocking(|| {
                let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
                let out = std::process::Command::new("git")
                    .args(["worktree", "list", "--porcelain"])
                    .current_dir(&cwd)
                    .output();
                let porcelain = match out {
                    Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
                    _ => String::new(),
                };
                let mut entries = parse(&porcelain);
                for e in entries.iter_mut() {
                    let sessions = astra_services::session_workspace::list_sessions_by_git_root(
                        &e.path, None, 50,
                    );
                    e.session_count = sessions.len();
                    e.last_session_at = sessions.first().map(|s| s.updated_at.clone());
                }
                entries
            })
            .await
            .unwrap_or_default();

            if entries.is_empty() {
                ctx.show_info("No worktrees found (or `git worktree list` failed).".into());
                return SlashResult::Handled;
            }
            let list = WorktreeList::new(entries);
            ctx.open_view("Opened worktrees", Box::new(WorktreesView::new(list)));
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
            // Timeline construction synchronously reads the entire JSONL
            // session journal from disk; on long sessions this freezes
            // the TUI for hundreds of ms. Run on a blocking thread.
            let sid_owned = sid.clone();
            let timeline = match tokio::task::spawn_blocking(move || {
                Timeline::new(JournalTurnSource::new(), &sid_owned)
            })
            .await
            {
                Ok(t) => t,
                Err(error) => {
                    ctx.show_info(format!("/timeline failed: {error}"));
                    return SlashResult::Handled;
                }
            };
            if let Some(error) = timeline.load_error() {
                ctx.show_info(error.to_string());
                return SlashResult::Handled;
            }
            if timeline.is_empty() {
                ctx.show_info(format!("No turns recorded yet for session {sid}."));
                return SlashResult::Handled;
            }
            ctx.open_view("Opened timeline", Box::new(TimelineView::new(timeline)));
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
            // SessionDiscovery::new walks the on-disk journal index and
            // reads up to `limit` workspace YAML files (~3 fs ops per
            // session). On a runtime thread that's a TUI freeze; do it
            // on a blocking thread instead.
            let disco = match tokio::task::spawn_blocking(|| {
                SessionDiscovery::new(FsSessionSource::new(), 50)
            })
            .await
            {
                Ok(d) => d,
                Err(error) => {
                    ctx.show_info(format!("session discovery failed: {error}"));
                    return SlashResult::Handled;
                }
            };
            if disco.total() == 0 {
                ctx.show_info("No previous sessions found.".into());
                return SlashResult::Handled;
            }
            ctx.open_view(
                "Opened session picker",
                Box::new(SessionPickerView::new(disco)),
            );
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
                "list" => handle_session_list_view(ctx).await,
                "history" => handle_session_history_view(ctx, rest),
                "fork" => handle_session_fork_view(ctx).await,
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
                    if let Err(error) = crate::cli::slash::slash_info::copy_to_clipboard(resp) {
                        ctx.show_error(format!("Copy failed: {error}"));
                    } else {
                        let preview: String = resp.chars().take(60).collect();
                        let suffix = if n > 60 { "…" } else { "" };
                        ctx.show_response(format!("Copied {n} chars: {preview}{suffix}"));
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

        "/whoami" | "/info" => {
            use crate::tui::bottom_pane::info_view::InfoView;

            let model = ctx.state.model.as_deref().unwrap_or("<unset>");
            let session = ctx.state.session_id.as_deref().unwrap_or("<none>");
            let perm = format!("{:?}", ctx.state.perm_manager.mode());
            let skills = ctx.state.unified_skill_registry.len();
            let version = env!("CARGO_PKG_VERSION");
            let pending = ctx
                .state
                .skill_improvement_tracker
                .pending_proposal
                .as_ref()
                .map(|p| p.skill_name.clone())
                .unwrap_or_else(|| "<none>".into());
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

            let pairs: Vec<(&str, String)> = vec![
                ("version", format!("astra v{version}")),
                ("session", session.to_string()),
                ("model", model.to_string()),
                ("permission", perm),
                ("skills loaded", skills.to_string()),
                ("turn", ctx.state.turn.to_string()),
                ("pending improve", pending),
                ("recent tools", recent_tools),
                ("context width", format!("{} cols", ctx.width)),
            ];

            ctx.open_view(
                "Opened system info",
                Box::new(InfoView::from_key_value("System Info", pairs).with_reopen("/info")),
            );
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
            ctx.open_view(
                "Opened history search",
                Box::new(HistoryView::new(&ctx.state.history, initial_query)),
            );
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
                    ctx.open_view(
                        "Opened project instructions",
                        Box::new(ListSelectionView::new(
                            items,
                            Some("Project Instructions:".into()),
                        )),
                    );
                    SlashResult::Handled
                }
                "show" => {
                    if let Some(ref pi) = ctx.state.project_instructions {
                        let line_count = pi.lines().count();
                        let title = format!("Project Instructions ({line_count} lines)");
                        ctx.open_view(
                            "Opened project instructions",
                            Box::new(
                                InfoView::from_plain(
                                    &title,
                                    pi.lines().map(|l| format!("  {l}")).collect(),
                                )
                                .with_reopen("/instructions"),
                            ),
                        );
                    } else {
                        ctx.show_info("No project instructions loaded. Create .astra/instructions.md in your project root.".into());
                    }
                    SlashResult::Handled
                }
                "reload" => {
                    if let Some(instructions) =
                        crate::cli::project_instructions::discover_project_instructions()
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

        // ── /memory — list/search in TUI; inspect falls back to text detail ──
        "/memory" => {
            let route = match memory_command_route(args) {
                Ok(route) => route,
                Err(msg) => {
                    ctx.show_error(msg.into());
                    return SlashResult::Handled;
                }
            };
            let token =
                crate::cli::session::session_runtime::fresh_access_token(ctx.api, ctx.profile)
                    .await;
            let Some(token) = token else {
                ctx.show_error("Not logged in. Use /login.".into());
                return SlashResult::Handled;
            };

            if args.trim() == "session" {
                let Some(session_id) = ctx.state.session_id.as_deref() else {
                    ctx.show_error("No active session yet.".into());
                    return SlashResult::Handled;
                };
                match crate::cli::slash::slash_memory::load_current_session_memory(
                    ctx.api, &token, session_id,
                )
                .await
                {
                    Ok(record) => {
                        let body = record
                            .as_ref()
                            .map(|memory| memory.body.as_str())
                            .unwrap_or_default();
                        let hint = if body.trim().is_empty() {
                            crate::cli::slash::slash_memory::latest_session_memory_status_hint(
                                session_id,
                            )
                        } else {
                            None
                        };
                        let summary = record.as_ref().and_then(|memory| memory.summary.as_deref());
                        let status = crate::cli::slash::slash_memory::session_memory_surface_status(
                            session_id,
                            record.as_ref(),
                        );
                        ctx.show_response(
                            crate::cli::slash::slash_memory::format_session_memory_response(
                                summary,
                                body,
                                Some(session_id),
                                hint.as_ref().map(|hint| hint.summary.as_str()),
                                Some(&status),
                            ),
                        );
                    }
                    Err(e) => ctx.show_error(format!("Session memory failed: {e}")),
                }
                return SlashResult::Handled;
            }

            if route == MemoryCommandRoute::Health {
                use crate::tui::bottom_pane::info_view::InfoView;

                match crate::edge_tools::memoria::memoria_health().await {
                    Ok(body) => {
                        let lines = crate::cli::slash::slash_memory::memory_health_lines(&body);
                        ctx.open_view(
                            "Opened memory health",
                            Box::new(
                                InfoView::from_plain("Memory Health", lines)
                                    .with_reopen("/memory health"),
                            ),
                        );
                    }
                    Err(e) => ctx.show_error(format!("Memory health failed: {e}")),
                }
                return SlashResult::Handled;
            }

            let (query, top_k, stats_view) = match route {
                MemoryCommandRoute::Search(query) => (query, 20, false),
                MemoryCommandRoute::List => (
                    crate::cli::slash::slash_memory::MEMORY_BROWSE_QUERY.to_string(),
                    crate::cli::slash::slash_memory::MEMORY_BROWSE_TOP_K,
                    false,
                ),
                MemoryCommandRoute::Stats => (
                    crate::cli::slash::slash_memory::MEMORY_BROWSE_QUERY.to_string(),
                    crate::cli::slash::slash_memory::MEMORY_STATS_TOP_K,
                    true,
                ),
                MemoryCommandRoute::Fallback => return SlashResult::Fallback,
                MemoryCommandRoute::Health => unreachable!("handled above"),
            };

            let payload = serde_json::json!({
                "query": query,
                "top_k": top_k,
            });
            match ctx.api.post_memory_search_json(&token, &payload).await {
                Ok(r) if r.status().is_success() => {
                    let body = r.text().await.unwrap_or_default();
                    match serde_json::from_str::<Vec<serde_json::Value>>(&body) {
                        Ok(arr) if !arr.is_empty() => {
                            if stats_view {
                                use crate::tui::bottom_pane::info_view::InfoView;

                                let lines =
                                    crate::cli::slash::slash_memory::memory_stats_lines(&arr);
                                ctx.open_view(
                                    "Opened memory stats",
                                    Box::new(
                                        InfoView::from_plain("Memory Stats", lines)
                                            .with_reopen("/memory stats"),
                                    ),
                                );
                                return SlashResult::Handled;
                            }
                            let mut hidden_session_entries = 0usize;
                            let items: Vec<SelectionItem> = arr
                                .iter()
                                .filter_map(|m| {
                                    let content =
                                        m.get("content").and_then(|v| v.as_str()).unwrap_or("?");
                                    if crate::cli::slash::slash_memory::is_session_proto(content) {
                                        hidden_session_entries += 1;
                                        return None;
                                    }
                                    let id = m
                                        .get("memory_id")
                                        .or(m.get("id"))
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("");
                                    let short_id = &id[..std::cmp::min(8, id.len())];
                                    Some(SelectionItem {
                                        name: crate::cli::slash::slash_memory::format_memory_entry_line(m),
                                        description: Some(format!("id:{short_id}")),
                                        is_current: false,
                                    })
                                })
                                .collect();
                            if items.is_empty() {
                                let mut message = "No non-session memories found.".to_string();
                                if hidden_session_entries > 0 {
                                    message.push_str(" Use /memory session to view session state.");
                                }
                                ctx.show_info(message);
                                return SlashResult::Handled;
                            }
                            let header = format!(
                                "Memory — {} result{} for: {}{}",
                                items.len(),
                                if items.len() == 1 { "" } else { "s" },
                                query,
                                if hidden_session_entries > 0 {
                                    format!(
                                        " ({hidden_session_entries} session entr{} hidden)",
                                        if hidden_session_entries == 1 {
                                            "y"
                                        } else {
                                            "ies"
                                        }
                                    )
                                } else {
                                    String::new()
                                }
                            );
                            ctx.open_view(
                                "Opened memory browser",
                                Box::new(
                                    ListSelectionView::new(items, Some(header))
                                        .with_footer_hint("↑↓ navigate · q / Esc close"),
                                ),
                            );
                            SlashResult::Handled
                        }
                        Ok(_) => {
                            ctx.show_info("No memories found.".into());
                            SlashResult::Handled
                        }
                        Err(_) => {
                            ctx.show_error("Failed to parse memory results.".into());
                            SlashResult::Handled
                        }
                    }
                }
                Ok(r) => {
                    ctx.show_error(format!("Memory search failed (HTTP {})", r.status()));
                    SlashResult::Handled
                }
                Err(e) => {
                    ctx.show_error(format!("Memory unreachable: {e}"));
                    SlashResult::Handled
                }
            }
        }

        // ── Everything else → route via TuiHandler metadata ────────
        _ => match command_registry::resolve_command_meta(cmd).map(|m| m.tui_handler) {
            // ChatForward (default, no tui_handler set) → forward to chat composer
            None | Some(TuiHandler::ChatForward) => SlashResult::Forward(text.to_string()),
            // Fallback → tear down TUI, run REPL handler, restore
            Some(TuiHandler::Fallback) => SlashResult::Fallback,
            // Panel → open native TUI panel (portals built in later phases)
            Some(TuiHandler::Panel) => {
                ctx.show_info(format!(
                    "`{}` panel is not yet implemented in TUI — forwarding to chat",
                    resolved
                ));
                SlashResult::Forward(text.to_string())
            }
            // Selector → open picker/selector (built in later phases)
            Some(TuiHandler::Selector) => {
                ctx.show_info(format!(
                    "`{}` selector is not yet implemented in TUI — forwarding to chat",
                    resolved
                ));
                SlashResult::Forward(text.to_string())
            }
            // Inline → should have been matched explicitly above
            Some(TuiHandler::Inline) => {
                ctx.show_error(format!("Command `{resolved}` not handled inline"));
                SlashResult::Handled
            }
        },
    }
}

fn build_permission_mode_picker(
    current: crate::cli::permission_manager::PermissionMode,
) -> ListSelectionView {
    let items = vec![
        SelectionItem {
            name: "Ask".into(),
            description: Some("Ask before write or execute tools".into()),
            is_current: current == crate::cli::permission_manager::PermissionMode::Prompt,
        },
        SelectionItem {
            name: "Edits".into(),
            description: Some(
                "Auto-approve workspace edits; still ask for shell and external writes".into(),
            ),
            is_current: current == crate::cli::permission_manager::PermissionMode::AcceptEdits,
        },
        SelectionItem {
            name: "Plan".into(),
            description: Some(
                "Read-only investigation mode; writes and shell mutations are denied".into(),
            ),
            is_current: current == crate::cli::permission_manager::PermissionMode::Plan,
        },
        SelectionItem {
            name: "Auto".into(),
            description: Some("All tools auto-approved".into()),
            is_current: current == crate::cli::permission_manager::PermissionMode::Auto,
        },
        SelectionItem {
            name: "Deny".into(),
            description: Some("Deny all tool calls".into()),
            is_current: current == crate::cli::permission_manager::PermissionMode::Deny,
        },
    ];
    ListSelectionView::new(items, Some("Modes".into())).with_footer_hint(
        "Shift+Tab cycles ask → edits → plan → auto · /allow rules · /allow trust · /allow trace",
    )
}

pub(crate) fn next_permission_mode_for_cycle(
    current: crate::cli::permission_manager::PermissionMode,
) -> crate::cli::permission_manager::PermissionMode {
    crate::cli::permission_command::next_permission_mode_for_cycle(current)
}

pub(crate) fn permission_mode_feedback(
    mode: crate::cli::permission_manager::PermissionMode,
) -> String {
    crate::cli::permission_command::permission_mode_feedback(mode)
}

fn apply_permission_mode_selection(
    state: &mut SessionState,
    chat_widget: &mut crate::tui::chat_widget::ChatWidget,
    mode: crate::cli::permission_manager::PermissionMode,
) {
    state.perm_manager.set_mode(mode);
    chat_widget.commit_system(SystemCell::response(permission_mode_feedback(mode)));
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
            "/config [edit|show|paths|sources|diff|export]",
            "edit opens the panel; show/paths/sources/diff/export stay text-first",
            "panel: ↑↓ navigate · Enter edit/set · type to search · Esc save/close",
        ),
        (
            "/model",
            "fuzzy-search model picker; switch or inspect current model",
            "type to filter · Enter select · Esc close",
        ),
        (
            "/skill",
            "browse, search, install, and manage skills",
            "type to filter · Enter select · Esc close",
        ),
        (
            "/memory [list|search <q>|show <id>|session|help]",
            "browse/search/stats in-panel; other subcommands stay text-first",
            "↑↓ navigate · Enter select · Esc close",
        ),
        (
            "/session",
            "list, fork, history, analyze, or export sessions",
            "↑↓ navigate · Enter select · Esc close",
        ),
        (
            "/stats",
            "view token cost, tool usage, and session statistics",
            "Enter / q / Esc close",
        ),
        (
            "/inspect",
            "session introspection: budget, tools, history, traces",
            "type subcommand · Esc close",
        ),
        (
            "/info",
            "system info at a glance — version, model, session, skills",
            "↑↓ scroll · Esc close",
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
            if let Some(instructions) =
                crate::cli::project_instructions::discover_project_instructions()
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
            apply_permission_mode_selection(
                state,
                chat_widget,
                crate::cli::permission_manager::PermissionMode::Auto,
            );
            return;
        }
        "Edits" => {
            apply_permission_mode_selection(
                state,
                chat_widget,
                crate::cli::permission_manager::PermissionMode::AcceptEdits,
            );
            return;
        }
        "Ask" | "Default" => {
            apply_permission_mode_selection(
                state,
                chat_widget,
                crate::cli::permission_manager::PermissionMode::Prompt,
            );
            return;
        }
        "Plan" => {
            apply_permission_mode_selection(
                state,
                chat_widget,
                crate::cli::permission_manager::PermissionMode::Plan,
            );
            return;
        }
        "Deny" => {
            apply_permission_mode_selection(
                state,
                chat_widget,
                crate::cli::permission_manager::PermissionMode::Deny,
            );
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
        "Trust Workspace" => {
            match state.perm_manager.trust_workspace() {
                Ok(message) => chat_widget.commit_system(SystemCell::response(message)),
                Err(err) => chat_widget.commit_system(SystemCell::error(format!(
                    "Failed to trust workspace: {err}"
                ))),
            }
            return;
        }
        "Untrust Workspace" => {
            match state.perm_manager.untrust_workspace() {
                Ok(message) => chat_widget.commit_system(SystemCell::response(message)),
                Err(err) => chat_widget.commit_system(SystemCell::error(format!(
                    "Failed to mark workspace untrusted: {err}"
                ))),
            }
            return;
        }
        "Trace" => {
            use crate::tui::bottom_pane::info_view::InfoView;
            let lines = astra_turn_core::permission::audit::format_snapshot_lines(50);
            bottom_pane.push_view(Box::new(
                InfoView::from_plain("Permission Trace", lines).with_reopen("/allow trace"),
            ));
            return;
        }
        _ => {}
    }

    // Slash command selected from help → insert into composer
    if name.starts_with('/') {
        bottom_pane.composer.set_text(&format!("{name} "));
    }

    // Unknown picker results must stay inert here. Model picks are
    // routed asynchronously by the outer event loop via
    // `MODEL_PICK_SENTINEL`; treating arbitrary selection text as a
    // model lets unrelated pickers (notably `/memory` results) poison
    // `state.model`.
}

fn show_stats_view(sub: &str, state: &SessionState, bottom_pane: &mut BottomPane) {
    use crate::tui::bottom_pane::info_view::InfoView;
    use astra_services::session_analytics;

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
                match crate::cli::session::session_stats_scan::read_session_journal_for_stats(&sid)
                {
                    Ok(events) => {
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
                                format!(
                                    "{} errors, {} stalls",
                                    stats.error_count, stats.stall_count
                                ),
                            ));
                        }
                        if stats.checkpoint_count > 0 {
                            pairs.push(("checkpoints", stats.checkpoint_count.to_string()));
                        }
                    }
                    Err(error) => pairs.push(("journal", format!("unavailable ({error})"))),
                }
            }
            bottom_pane.push_view(Box::new(
                InfoView::from_key_value("Session Stats", pairs).with_reopen("/stats"),
            ));
        }

        "history" => {
            let lines = match build_recent_session_history_lines(10) {
                Ok(lines) => lines,
                Err(error) => vec![format!("  {error}")],
            };
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
            let events =
                match crate::cli::session::session_stats_scan::read_session_journal_for_stats(&sid)
                {
                    Ok(events) => events,
                    Err(error) => {
                        bottom_pane.push_view(Box::new(
                            InfoView::from_plain("Tool Performance", vec![format!("  {error}")])
                                .with_reopen("/stats"),
                        ));
                        return;
                    }
                };
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
            let cost = crate::cli::slash::slash_stats::cost_for_tokens(
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
                        crate::cli::slash::slash_stats::format_cost(
                            state.total_prompt_tokens as f64 * pricing.prompt / 1000.0
                        )
                    ),
                ),
                (
                    "completion",
                    format!(
                        "{} ({})",
                        state.total_completion_tokens,
                        crate::cli::slash::slash_stats::format_cost(
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
                        crate::cli::slash::slash_stats::format_cost(
                            state.total_cache_read_tokens as f64 * rate / 1000.0
                        )
                    ),
                ));
            }
            pairs.push(("total", crate::cli::slash::slash_stats::format_cost(cost)));
            if state.turn > 0 {
                pairs.push((
                    "avg/turn",
                    crate::cli::slash::slash_stats::format_cost(cost / state.turn as f64),
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
            let events =
                match crate::cli::session::session_stats_scan::read_session_journal_for_stats(&sid)
                {
                    Ok(events) => events,
                    Err(error) => {
                        bottom_pane.push_view(Box::new(
                            InfoView::from_plain("Tool Health", vec![format!("  {error}")])
                                .with_reopen("/stats"),
                        ));
                        return;
                    }
                };
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

fn build_recent_session_history_lines(limit: usize) -> Result<Vec<String>, String> {
    use astra_services::session_analytics;

    let scan = crate::cli::session::session_stats_scan::collect_recent_session_stats(limit)?;
    if scan.stats.is_empty() && scan.unreadable.is_empty() {
        return Ok(vec!["  No sessions found.".into()]);
    }

    let mut lines = Vec::new();
    for stats in &scan.stats {
        let short = &stats.session_id[..8.min(stats.session_id.len())];
        let model = stats.model.as_deref().unwrap_or("?");
        lines.push(format!(
            "  {short}  {:>3} turns  {:>6}+{:<6} tok  {:>3} tools  {model}",
            stats.turn_count, stats.total_tokens_in, stats.total_tokens_out, stats.total_tool_calls,
        ));
    }
    for unreadable in &scan.unreadable {
        let short = &unreadable.session_id[..8.min(unreadable.session_id.len())];
        let preview: String = unreadable.error.chars().take(96).collect();
        lines.push(format!("  {short}  journal unreadable ({preview})"));
    }

    if scan.stats.is_empty() {
        if !lines.is_empty() {
            lines.push(String::new());
        }
        lines.push("  Summary: no readable session data".into());
        lines.push(format!(
            "  Skipped {} unreadable journal(s).",
            scan.unreadable.len()
        ));
        return Ok(lines);
    }

    let agg = session_analytics::aggregate_stats(&scan.stats);
    if !lines.is_empty() {
        lines.push(String::new());
    }
    lines.push(format!(
        "  Summary: {} sessions, {} turns, {}+{} tokens",
        agg.session_count, agg.total_turns, agg.total_tokens_in, agg.total_tokens_out,
    ));
    if !scan.unreadable.is_empty() {
        lines.push(format!(
            "  Skipped {} unreadable journal(s).",
            scan.unreadable.len()
        ));
    }

    Ok(lines)
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

async fn handle_mcp_dispatch(args: &str, ctx: &mut DispatchContext<'_>) -> SlashResult {
    use crate::cli::slash::slash_mcp::ParsedMcpCommand as Cmd;

    match crate::cli::slash::slash_mcp::parse_mcp_command(args) {
        Cmd::Help => {
            ctx.show_response(mcp_help_text(ctx.state).await);
            SlashResult::Handled
        }
        Cmd::Overview => {
            ctx.show_response(mcp_overview_text(ctx.state).await);
            SlashResult::Handled
        }
        Cmd::Servers => {
            ctx.show_response(mcp_servers_text(ctx.state).await);
            SlashResult::Handled
        }
        Cmd::Tools(server) => {
            ctx.show_response(mcp_tools_text(ctx.state, server).await);
            SlashResult::Handled
        }
        Cmd::Prompts => {
            ctx.show_response(mcp_prompts_text(ctx.state).await);
            SlashResult::Handled
        }
        Cmd::Resources => {
            ctx.show_response(mcp_resources_text(ctx.state).await);
            SlashResult::Handled
        }
        Cmd::Read(Some(spec)) => {
            ctx.show_response(mcp_read_text(ctx.state, spec).await);
            SlashResult::Handled
        }
        Cmd::Read(None) => {
            ctx.show_error("Usage: /mcp read <server>:<uri>".into());
            SlashResult::Handled
        }
        Cmd::History => {
            ctx.show_response(mcp_history_text(ctx.state).await);
            SlashResult::Handled
        }
        Cmd::Inspect(Some(query)) => {
            ctx.show_response(mcp_inspect_text(ctx.state, query).await);
            SlashResult::Handled
        }
        Cmd::Inspect(None) => {
            ctx.show_error(
                "Usage: /mcp inspect <server>:<tool>  ·  try `/mcp tools` first.".into(),
            );
            SlashResult::Handled
        }
        Cmd::Ping(server) => {
            ctx.show_response(mcp_ping_text(ctx.state, server).await);
            SlashResult::Handled
        }
        Cmd::Add(Some(_)) => mcp_fallback_notice(ctx, "add"),
        Cmd::Add(None) => {
            ctx.show_error("Usage: /mcp add <name> <command> [args…]".into());
            SlashResult::Handled
        }
        Cmd::Remove(Some(_)) => mcp_fallback_notice(ctx, "remove"),
        Cmd::Remove(None) => {
            ctx.show_error("Usage: /mcp remove <name>".into());
            SlashResult::Handled
        }
        Cmd::Subscribe(Some(_)) => mcp_fallback_notice(ctx, "subscribe"),
        Cmd::Subscribe(None) => {
            ctx.show_error("Usage: /mcp subscribe <server>:<uri>".into());
            SlashResult::Handled
        }
        Cmd::Unsubscribe(Some(_)) => mcp_fallback_notice(ctx, "unsubscribe"),
        Cmd::Unsubscribe(None) => {
            ctx.show_error("Usage: /mcp unsubscribe <server>:<uri>".into());
            SlashResult::Handled
        }
        Cmd::LogLevel(Some(_)) => mcp_fallback_notice(ctx, "log-level"),
        Cmd::LogLevel(None) => {
            ctx.show_error("Usage: /mcp log-level <server> <level>".into());
            SlashResult::Handled
        }
        Cmd::Prompt(Some(_)) => mcp_fallback_notice(ctx, "prompt"),
        Cmd::Prompt(None) => {
            ctx.show_error("Usage: /mcp prompt <server>:<name> [args…]".into());
            SlashResult::Handled
        }
        Cmd::Complete(Some(_)) => mcp_fallback_notice(ctx, "complete"),
        Cmd::Complete(None) => {
            ctx.show_error("Usage: /mcp complete <server>:<prompt|resource> <arg> [value]".into());
            SlashResult::Handled
        }
        Cmd::Unknown(sub) => {
            ctx.show_error(format!(
                "Unknown `/mcp` subcommand: `{sub}`. Try `/mcp help`."
            ));
            SlashResult::Handled
        }
    }
}

fn mcp_fallback_notice(ctx: &mut DispatchContext<'_>, subcommand: &str) -> SlashResult {
    ctx.show_info(format!(
        "`/mcp {subcommand}` still uses terminal fallback. Core discovery commands (`/mcp list`, `/mcp tools`, `/mcp prompts`, `/mcp resources`, `/mcp read`, `/mcp inspect`, `/mcp ping`) are native in TUI."
    ));
    SlashResult::Fallback
}

async fn mcp_help_text(state: &SessionState) -> String {
    let count = state.mcp_manager.read().await.connection_count();
    let mut lines = vec!["MCP commands".to_string()];
    if count == 0 {
        lines.push("No MCP servers connected yet.".into());
        lines.push("Add one with: /mcp add <name> <command> [args…]".into());
    } else {
        lines.push(format!(
            "{count} server(s) connected. Start with `/mcp list`."
        ));
    }
    lines.push("/mcp list                 overview of connected servers".into());
    lines.push("/mcp tools [server]       list callable tools".into());
    lines.push("/mcp inspect <server>:<tool>  show tool parameters".into());
    lines.push("/mcp prompts              list prompt templates".into());
    lines.push("/mcp resources            list readable resources".into());
    lines.push("/mcp read <server>:<uri>  read one resource".into());
    lines.push("/mcp ping [server]        connectivity check".into());
    lines.push(
        "Advanced: /mcp add, remove, prompt, complete, log-level, subscribe, unsubscribe, history"
            .into(),
    );
    lines.join("\n")
}

fn mcp_no_servers_text() -> String {
    "No MCP servers connected.\nAdd one with: /mcp add <name> <command> [args…]\nThen use `/mcp list` or `/mcp tools`.".into()
}

fn mcp_format_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    }
}

fn mcp_state_text(state: crate::mcp_client::ConnectionState) -> &'static str {
    match state {
        crate::mcp_client::ConnectionState::Connected => "connected",
        crate::mcp_client::ConnectionState::Connecting => "connecting",
        crate::mcp_client::ConnectionState::Reconnecting => "reconnecting",
        crate::mcp_client::ConnectionState::Disconnected => "disconnected",
        crate::mcp_client::ConnectionState::Failed => "failed",
    }
}

fn mcp_trim_text(text: &str, max_chars: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let end = text
        .char_indices()
        .nth(max_chars)
        .map(|(idx, _)| idx)
        .unwrap_or(text.len());
    format!("{}…", &text[..end])
}

fn mcp_truncate_block(text: &str, max_lines: usize, max_chars_per_line: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = lines
        .iter()
        .take(max_lines)
        .map(|line| mcp_trim_text(line, max_chars_per_line))
        .collect::<Vec<_>>();
    if lines.len() > max_lines {
        out.push(format!("… (+{} more lines)", lines.len() - max_lines));
    }
    out.join("\n")
}

async fn mcp_overview_text(state: &SessionState) -> String {
    let manager = state.mcp_manager.read().await;
    let count = manager.connection_count();
    if count == 0 {
        return mcp_no_servers_text();
    }

    let tools = manager.all_tools().len();
    let prompts = manager.all_prompts().await.len();
    let resources = manager.all_resources().await.len();
    let mut capabilities = vec!["elicitation"];
    if manager.has_sampling() {
        capabilities.insert(0, "sampling");
    }

    let mut lines = vec![
        "MCP overview".into(),
        format!("Servers: {count} connected"),
        format!("Capabilities: {}", capabilities.join(", ")),
        format!("Tools: {tools}  ·  Prompts: {prompts}  ·  Resources: {resources}"),
    ];

    let roots = manager.roots().read().await;
    if !roots.is_empty() {
        let names = roots
            .iter()
            .map(|root| root.name.clone().unwrap_or_else(|| root.uri.clone()))
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(format!("Roots: {names}"));
    }
    drop(roots);

    lines.push(String::new());
    lines.push("Servers".into());

    let mut servers = manager.connected_servers();
    servers.sort_unstable();
    for name in servers {
        if let Some(conn) = manager.get(name) {
            let state_text = mcp_state_text(
                manager
                    .server_state(name)
                    .unwrap_or(crate::mcp_client::ConnectionState::Connected),
            );
            let uptime = conn
                .uptime()
                .map(mcp_format_duration)
                .unwrap_or_else(|| "n/a".into());
            lines.push(format!(
                "- {name} — {state_text} · {} tools · uptime {uptime}",
                conn.tools().len()
            ));
        }
    }

    lines.push(String::new());
    lines.push("Next: /mcp tools [server] · /mcp prompts · /mcp resources".into());
    lines.join("\n")
}

async fn mcp_servers_text(state: &SessionState) -> String {
    let manager = state.mcp_manager.read().await;
    if manager.connection_count() == 0 {
        return mcp_no_servers_text();
    }

    let mut lines = vec!["MCP servers".into()];
    let mut servers = manager.connected_servers();
    servers.sort_unstable();
    for name in servers {
        if let Some(conn) = manager.get(name) {
            let state_text = mcp_state_text(
                manager
                    .server_state(name)
                    .unwrap_or(crate::mcp_client::ConnectionState::Connected),
            );
            let uptime = conn
                .uptime()
                .map(mcp_format_duration)
                .unwrap_or_else(|| "n/a".into());
            let preview = conn
                .tools()
                .iter()
                .take(4)
                .map(|tool| tool.name.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            lines.push(format!(
                "- {name} — {state_text} · {} tools · uptime {uptime}",
                conn.tools().len()
            ));
            if !preview.is_empty() {
                let more = conn.tools().len().saturating_sub(4);
                if more > 0 {
                    lines.push(format!("  tools: {preview}, … (+{more} more)"));
                } else {
                    lines.push(format!("  tools: {preview}"));
                }
            }
        }
    }
    lines.push(String::new());
    lines.push("Inspect one server's tools: /mcp tools <server>".into());
    lines.join("\n")
}

async fn mcp_tools_text(state: &SessionState, server: Option<&str>) -> String {
    let manager = state.mcp_manager.read().await;
    if manager.connection_count() == 0 {
        return mcp_no_servers_text();
    }

    let mut lines = Vec::new();
    match server.map(str::trim).filter(|s| !s.is_empty()) {
        Some(server_name) => {
            let Some(conn) = manager.get(server_name) else {
                return format!(
                    "Server '{server_name}' not found.\nTry `/mcp list` or `/mcp servers`."
                );
            };
            lines.push(format!(
                "MCP tools from {server_name} ({})",
                conn.tools().len()
            ));
            for tool in conn.tools() {
                let desc = tool.description.as_deref().unwrap_or("(no description)");
                lines.push(format!("- {} — {}", tool.name, mcp_trim_text(desc, 90)));
            }
            lines.push(String::new());
            lines.push(format!("Inspect one: /mcp inspect {server_name}:<tool>"));
        }
        None => {
            let mut tools = manager
                .all_tools()
                .into_iter()
                .map(|(server_name, tool)| {
                    (
                        server_name.to_string(),
                        tool.name.to_string(),
                        tool.description
                            .as_deref()
                            .map(|text| mcp_trim_text(text, 90))
                            .unwrap_or_else(|| "(no description)".into()),
                    )
                })
                .collect::<Vec<_>>();
            tools.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
            lines.push(format!("MCP tools ({})", tools.len()));
            for (server_name, tool_name, desc) in tools {
                lines.push(format!("- {server_name}:{tool_name} — {desc}"));
            }
            lines.push(String::new());
            lines.push("Inspect one: /mcp inspect <server>:<tool>".into());
        }
    }
    lines.join("\n")
}

async fn mcp_prompts_text(state: &SessionState) -> String {
    let manager = state.mcp_manager.read().await;
    if manager.connection_count() == 0 {
        return mcp_no_servers_text();
    }

    let mut prompts = manager.all_prompts().await;
    if prompts.is_empty() {
        return "No MCP prompts available from connected servers.".into();
    }
    prompts.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.name.cmp(&b.1.name)));

    let mut lines = vec![format!("MCP prompts ({})", prompts.len())];
    for (server_name, prompt) in prompts {
        let args = prompt
            .arguments
            .as_ref()
            .map(|args| {
                args.iter()
                    .map(|arg| {
                        if arg.required.unwrap_or(false) {
                            format!("<{}>", arg.name)
                        } else {
                            format!("[{}]", arg.name)
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        let desc = prompt
            .description
            .as_deref()
            .map(|text| mcp_trim_text(text, 90))
            .unwrap_or_else(|| "(no description)".into());
        if args.is_empty() {
            lines.push(format!("- {server_name}:{} — {desc}", prompt.name));
        } else {
            lines.push(format!("- {server_name}:{} {args} — {desc}", prompt.name));
        }
    }
    lines.push(String::new());
    lines.push("Run one: /mcp prompt <server>:<name> [args…]".into());
    lines.join("\n")
}

async fn mcp_resources_text(state: &SessionState) -> String {
    let manager = state.mcp_manager.read().await;
    if manager.connection_count() == 0 {
        return mcp_no_servers_text();
    }

    let mut resources = manager.all_resources().await;
    if resources.is_empty() {
        return "No MCP resources available from connected servers.".into();
    }
    resources.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.raw.uri.cmp(&b.1.raw.uri)));

    let mut lines = vec![format!("MCP resources ({})", resources.len())];
    for (server_name, resource) in resources {
        let mime = resource.raw.mime_type.as_deref().unwrap_or("unknown");
        let desc = resource
            .description
            .as_deref()
            .map(|text| mcp_trim_text(text, 80))
            .unwrap_or_default();
        if desc.is_empty() {
            lines.push(format!("- {server_name}:{} [{mime}]", resource.raw.uri));
        } else {
            lines.push(format!(
                "- {server_name}:{} [{mime}] — {desc}",
                resource.raw.uri
            ));
        }
    }
    lines.push(String::new());
    lines.push("Read one: /mcp read <server>:<uri>".into());
    lines.join("\n")
}

async fn mcp_read_text(state: &SessionState, spec: &str) -> String {
    let spec = spec.trim();
    let (server_name, uri) = match spec.split_once(':') {
        Some((server_name, uri)) if !server_name.is_empty() && !uri.is_empty() => {
            (server_name, uri)
        }
        _ => return "Usage: /mcp read <server>:<uri>".into(),
    };

    let conn = {
        let manager = state.mcp_manager.read().await;
        if manager.connection_count() == 0 {
            return mcp_no_servers_text();
        }
        match manager.get(server_name) {
            Some(conn) => conn,
            None => {
                return format!(
                    "Server '{server_name}' not found.\nTry `/mcp list` or `/mcp servers`."
                );
            }
        }
    };

    match conn.read_resource(uri).await {
        Ok(content) if content.trim().is_empty() => {
            format!("{server_name}:{uri}\n(empty resource)")
        }
        Ok(content) => format!(
            "{server_name}:{uri}\n\n{}",
            mcp_truncate_block(&content, 40, 140)
        ),
        Err(error) => format!("Failed to read '{uri}' from '{server_name}': {error}"),
    }
}

async fn mcp_history_text(state: &SessionState) -> String {
    let manager = state.mcp_manager.read().await;
    if manager.connection_count() == 0 {
        return mcp_no_servers_text();
    }

    let mut entries = Vec::new();
    for name in manager.server_names() {
        if let Some(conn) = manager.get_connection(name) {
            let log = conn.call_log.read().await;
            entries.extend(log.iter().cloned());
        }
    }
    if entries.is_empty() {
        return "No MCP tool calls recorded yet.\nUse an MCP tool, then run `/mcp history` again."
            .into();
    }
    entries.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));

    let mut lines = vec![format!("MCP call history ({})", entries.len())];
    for entry in entries.into_iter().take(20) {
        let status = if entry.success { "ok" } else { "fail" };
        lines.push(format!(
            "- {} · {}:{} · {}ms · {status}",
            entry.timestamp, entry.server, entry.tool, entry.latency_ms
        ));
        if let Some(error) = entry.error {
            lines.push(format!("  error: {}", mcp_trim_text(&error, 120)));
        }
    }
    lines.join("\n")
}

fn mcp_protocol_tool_text(server: &str, tool: &rmcp::model::Tool) -> String {
    let mut lines = vec![
        format!("Tool: {server}:{}", tool.name),
        format!(
            "Description: {}",
            tool.description.as_deref().unwrap_or("(no description)")
        ),
    ];
    let schema = &*tool.input_schema;
    if let Some(props) = schema.get("properties").and_then(|value| value.as_object()) {
        let required = schema
            .get("required")
            .and_then(|value| value.as_array())
            .map(|values| {
                values
                    .iter()
                    .filter_map(|value| value.as_str())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if props.is_empty() {
            lines.push("Parameters: none".into());
        } else {
            lines.push("Parameters:".into());
            for (name, param_schema) in props {
                let required_marker = if required.contains(&name.as_str()) {
                    "required"
                } else {
                    "optional"
                };
                let param_type = param_schema
                    .get("type")
                    .and_then(|value| value.as_str())
                    .unwrap_or("any");
                let desc = param_schema
                    .get("description")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                if desc.is_empty() {
                    lines.push(format!("- {name}: {param_type} ({required_marker})"));
                } else {
                    lines.push(format!(
                        "- {name}: {param_type} ({required_marker}) — {}",
                        mcp_trim_text(desc, 100)
                    ));
                }
            }
        }
    } else {
        lines.push("Parameters: none".into());
    }
    lines.join("\n")
}

fn mcp_builtin_tool_text(meta: &astra_turn_core::tool::registry::meta::ToolMeta) -> String {
    [
        format!("Tool: {}", meta.name),
        format!("Description: {}", meta.description),
        format!("Scope: {:?}", meta.scope),
        format!("Intents: {:?}", meta.intents),
        format!("Schema tokens: {}", meta.schema_tokens),
    ]
    .join("\n")
}

async fn mcp_inspect_text(state: &SessionState, query: &str) -> String {
    let manager = state.mcp_manager.read().await;
    match crate::cli::slash::slash_mcp::resolve_protocol_tool_query(&manager, query) {
        Ok((server, tool)) => mcp_protocol_tool_text(server, tool),
        Err(protocol_error) => {
            for meta in astra_turn_core::tool::registry::meta::TOOL_CATALOG {
                if meta.name == query
                    || format!("mcp_{}", meta.name) == query
                    || format!("mcp_memoria_{}", meta.name) == query
                {
                    return mcp_builtin_tool_text(meta);
                }
            }
            protocol_error
        }
    }
}

async fn mcp_ping_text(state: &SessionState, server: Option<&str>) -> String {
    let mut manager = state.mcp_manager.write().await;
    if manager.connection_count() == 0 {
        return mcp_no_servers_text();
    }

    match server.map(str::trim).filter(|name| !name.is_empty()) {
        Some(name) => match manager.ping(name).await {
            Ok(duration) => format!("✓ {name}: {:.1}ms", duration.as_secs_f64() * 1000.0),
            Err(error) => format!("✗ {name}: {error}"),
        },
        None => {
            let mut results = manager.ping_all().await;
            if results.is_empty() {
                return "No MCP servers connected.".into();
            }
            results.sort_by(|a, b| a.0.cmp(&b.0));
            results
                .into_iter()
                .map(|(name, result)| match result {
                    Ok(duration) => format!("✓ {name}: {:.1}ms", duration.as_secs_f64() * 1000.0),
                    Err(error) => format!("✗ {name}: {error}"),
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigCommandRoute {
    Panel,
    Fallback,
}

fn config_command_route(args: &str) -> Result<ConfigCommandRoute, &'static str> {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        return Ok(ConfigCommandRoute::Panel);
    }

    let (sub, _) = split_sub(trimmed);
    match sub {
        "edit" => Ok(ConfigCommandRoute::Panel),
        "show" | "paths" | "sources" | "diff" | "export" | "help" | "-h" | "--help" => {
            Ok(ConfigCommandRoute::Fallback)
        }
        _ => Err("Usage: /config [edit|show|paths|sources|diff|export [path]]"),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum MemoryCommandRoute {
    List,
    Search(String),
    Stats,
    Health,
    Fallback,
}

fn memory_command_route(args: &str) -> Result<MemoryCommandRoute, &'static str> {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        return Ok(MemoryCommandRoute::List);
    }

    let (sub, rest) = split_sub(trimmed);
    match sub {
        "list" | "ls" if rest.is_empty() => Ok(MemoryCommandRoute::List),
        "search" if !rest.is_empty() => Ok(MemoryCommandRoute::Search(rest.to_string())),
        "stats" | "count" if rest.is_empty() => Ok(MemoryCommandRoute::Stats),
        "health" if rest.is_empty() => Ok(MemoryCommandRoute::Health),
        _ => Ok(MemoryCommandRoute::Fallback),
    }
}

fn inspect_command_supported(args: &str) -> Result<(), String> {
    let sub = args.trim();
    match sub {
        "" => Ok(()),
        "budget" | "tools" | "context" | "cache" | "json" | "diff" | "history" | "trace"
        | "forensics" | "export" => Ok(()),
        _ => Err(format!("Unknown `/inspect` subcommand: `{sub}`.")),
    }
}

const CONTEXT_USAGE_MESSAGE: &str = "Usage: /context — open the context panel\n       /context dump [path] — write a JSON snapshot.";

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
pub(crate) const MODEL_PICKER_FOOTER_HINT: &str =
    "Type to filter | Enter to choose | Some models then ask for thinking mode | Esc to go back";
/// Sentinel prefix for the thinking-mode picker. Payload format is
/// `__model_thinking__\n<base_model>\n<thinking_label>`.  The
/// handler composes `base + thinking_suffix_for(label)` and sets
/// `state.model`.
pub(crate) const MODEL_THINKING_SENTINEL: &str = "__model_thinking__\n";
pub(crate) const MODEL_THINKING_PICKER_FOOTER_HINT: &str =
    "Type to filter | Enter to finish model selection | Esc to go back";

/// `/model` with no args (or `list`) — fetch the catalog and push
/// the picker.  The picker emits `MODEL_PICK_SENTINEL + <name>`; the
/// outer loop then checks the model's `thinking_capability` and
/// either commits or pushes a thinking-mode picker.
/// True when an error string represents an Astra session auth failure.
/// Matches both session-specific error patterns (via `is_astra_session_auth_error`)
/// and Astra's own HTTP 401 format (`"request failed (401): ..."`).
/// Generic upstream `401 Unauthorized` text must NOT trigger `/login`.
fn is_astra_auth_error(msg: &str) -> bool {
    crate::cli::cli_config::cli_utils::is_astra_session_auth_error(msg)
        || msg.contains("request failed (401)")
}

/// Build the model picker view from a fetched model list and push it.
fn push_model_picker(ctx: &mut DispatchContext<'_>, models: Vec<String>) -> bool {
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
        false
    } else {
        let view = ListSelectionView::new(items, Some("Select model:".into()))
            .with_footer_hint(MODEL_PICKER_FOOTER_HINT)
            .with_result_prefix(MODEL_PICK_SENTINEL);
        ctx.open_deferred_view("Opened model picker", Box::new(view));
        true
    }
}

async fn open_model_picker(ctx: &mut DispatchContext<'_>) -> SlashResult {
    let token =
        crate::cli::session::session_runtime::fresh_access_token(ctx.api, ctx.profile).await;
    match crate::cli::slash::slash_router::fetch_model_list(ctx.api, token.as_deref()).await {
        Ok(models) => {
            if push_model_picker(ctx, models) {
                return SlashResult::Deferred;
            }
        }
        Err(e) => {
            let msg = e.to_string();
            if is_astra_auth_error(&msg) {
                // Attempt silent token refresh + retry once. If the retry
                // itself fails with a non-auth error (e.g. 5xx after refresh),
                // surface that real error instead of the generic /login hint.
                if crate::cli::session::session_runtime::attempt_token_refresh(ctx.api, ctx.profile)
                    .await
                {
                    let fresh =
                        crate::cli::session::session_runtime::current_access_token(ctx.profile);
                    match crate::cli::slash::slash_router::fetch_model_list(
                        ctx.api,
                        fresh.as_deref(),
                    )
                    .await
                    {
                        Ok(models) => {
                            if push_model_picker(ctx, models) {
                                return SlashResult::Deferred;
                            }
                            return SlashResult::Handled;
                        }
                        Err(retry_err) => {
                            let retry_msg = retry_err.to_string();
                            if !is_astra_auth_error(&retry_msg) {
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
    let Some(name) = crate::cli::cli_config::cli_utils::normalize_model_override(Some(name)) else {
        ctx.state.model = None;
        crate::cli::slash::slash_config::set_active_model_for_display(None);
        ctx.bottom_pane.footer.model = None;
        ctx.show_response("Model override cleared — using API default.".into());
        return;
    };
    ctx.state.model = Some(name.to_string());
    crate::cli::slash::slash_config::set_active_model_for_display(Some(name.to_string()));
    ctx.bottom_pane.footer.model = Some(name.to_string());
    ctx.show_response(format!("Set model to {name}"));
}

/// `/model clear` — unset the session override so the edge's
/// default model applies.  Reports the change to scrollback so
/// the user sees the footer switch.
async fn handle_model_clear(ctx: &mut DispatchContext<'_>) -> SlashResult {
    ctx.state.model = None;
    crate::cli::slash::slash_config::set_active_model_for_display(None);
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

    ctx.open_view(
        format!("Opened model info · {name}"),
        Box::new(InfoView::from_key_value(&format!("Model · {name}"), pairs)),
    );
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
    let (ws, workspace_error) = if sid.is_empty() {
        (None, None)
    } else {
        match session_workspace::read_workspace_optional(&sid) {
            Ok(workspace) => (workspace, None),
            Err(error) => (None, Some(error)),
        }
    };
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
    } else if workspace_error.is_some() {
        pairs.push((
            "workspace",
            "metadata unreadable; using live/journal state".into(),
        ));
    } else {
        let cwd = std::env::current_dir()
            .map(|p| tilde_session_path(&p.to_string_lossy()))
            .unwrap_or_else(|_| "?".into());
        pairs.push(("cwd", cwd));
    }
    if let Some(error) = session_hub_persistence_error(ctx.state, ws.as_ref()) {
        pairs.push(("persistence", error));
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
        let guard = astra_core::sync_poison::recover_rwlock_read(&obs);
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
    ctx.open_view(
        format!("Opened {title}"),
        Box::new(InfoView::from_key_value(&title, pairs)),
    );
    SlashResult::Handled
}

fn session_hub_persistence_error(
    state: &crate::cli::session::session_state::SessionState,
    workspace: Option<&astra_services::session_workspace::WorkspaceMetadata>,
) -> Option<String> {
    state
        .session_persistence_error
        .as_deref()
        .or_else(|| workspace.and_then(|ws| ws.last_persistence_error.as_deref()))
        .map(str::trim)
        .filter(|error| !error.is_empty())
        .map(|error| {
            format!(
                "degraded: {error} · live session can continue; resume/fork metadata may be stale until the next successful save"
            )
        })
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
async fn handle_session_list_view(ctx: &mut DispatchContext<'_>) -> SlashResult {
    use crate::tui::bottom_pane::session_picker_view::SessionPickerView;
    use crate::tui::session_picker::{FsSessionSource, SessionDiscovery};
    let disco =
        match tokio::task::spawn_blocking(|| SessionDiscovery::new(FsSessionSource::new(), 50))
            .await
        {
            Ok(d) => d,
            Err(error) => {
                ctx.show_info(format!("session discovery failed: {error}"));
                return SlashResult::Handled;
            }
        };
    if disco.total() == 0 {
        ctx.show_info("No previous sessions found.".into());
        return SlashResult::Handled;
    }
    ctx.open_view(
        "Opened session list",
        Box::new(SessionPickerView::new(disco)),
    );
    SlashResult::Handled
}

fn handle_session_history_view(ctx: &mut DispatchContext<'_>, arg: &str) -> SlashResult {
    let sid = resolve_session_arg(ctx, arg);
    let Some(sid) = sid else {
        return SlashResult::Handled;
    };
    let events = match astra_services::session_journal::read_journal(&sid) {
        Ok(events) => events,
        Err(error) => {
            ctx.show_error(format!("Failed to read journal: {error}"));
            return SlashResult::Handled;
        }
    };
    if events.is_empty() {
        ctx.show_info(format!("No journal events for session {sid}."));
        return SlashResult::Handled;
    }
    let sid_short = if sid.len() > 8 { &sid[..8] } else { &sid };
    ctx.show_response(format!("Opened session history · {sid_short}"));
    push_history_info(ctx, &sid, &events);
    SlashResult::Handled
}

/// `/session fork` — interactive parent picker.  On Enter the
/// picker emits `"__fork__\n<sid>"`; the outer loop recognises the
/// sentinel and runs `fork_local_session`.  No args short-circuits
/// through the picker; `/session fork <sid>` falls back to the
/// line-mode handler (covers scripted use).
async fn handle_session_fork_view(ctx: &mut DispatchContext<'_>) -> SlashResult {
    use crate::tui::bottom_pane::session_picker_view::SessionPickerView;
    use crate::tui::session_picker::{FsSessionSource, SessionDiscovery};
    let disco =
        match tokio::task::spawn_blocking(|| SessionDiscovery::new(FsSessionSource::new(), 50))
            .await
        {
            Ok(d) => d,
            Err(error) => {
                ctx.show_info(format!("session discovery failed: {error}"));
                return SlashResult::Handled;
            }
        };
    if disco.total() == 0 {
        ctx.show_info("No previous sessions to fork from.".into());
        return SlashResult::Handled;
    }
    ctx.open_view(
        "Opened session fork picker",
        Box::new(SessionPickerView::new(disco).with_result_prefix(FORK_PICK_SENTINEL)),
    );
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
            crate::cli::slash::slash_config::set_deep_analyze_arg(Some(rest.to_string()));
        } else {
            crate::cli::slash::slash_config::set_deep_analyze_arg(None);
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
    ctx.open_view(
        format!("Opened session analysis · {sid_short}"),
        Box::new(InfoView::from_key_value(
            &format!("Session analyze · {sid_short}"),
            pairs,
        )),
    );
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
    let workspace = match astra_services::session_workspace::read_workspace_optional(&sid) {
        Ok(workspace) => workspace,
        Err(error) => {
            ctx.show_info(format!(
                "workspace.yaml is invalid; export omits workspace health metadata: {error}"
            ));
            None
        }
    };
    let md =
        crate::cli::slash::slash_session::build_export_markdown(&sid, workspace.as_ref(), &events);
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
///
/// Cached process-wide so a flurry of slash commands doesn't spawn
/// repeat `gix::discover` walks. See `crate::git_branch_cache`.
fn detect_git_branch() -> Option<String> {
    crate::git_branch_cache::detect_git_branch_cached()
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
    let path = match crate::cli::context_dump::write_dump_for_repl(
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

// ── /inspect dispatch ────────────────────────────────────────────────
//
// Routes `/inspect` subcommands. Until the dedicated TUI panel ships,
// all supported forms fall back to the text renderer immediately so the
// command still executes instead of being bounced back into the composer.
async fn handle_inspect_dispatch(args: &str, ctx: &mut DispatchContext<'_>) -> SlashResult {
    match inspect_command_supported(args) {
        Ok(()) => SlashResult::Fallback,
        Err(message) => {
            ctx.show_error(message);
            SlashResult::Handled
        }
    }
}

#[cfg(test)]
mod blocking_io_guard_tests {
    /// Source-level guard: every slash command that loads from disk must do
    /// it on a blocking thread. Specifically, the three known offenders —
    /// `/timeline`, `/worktrees`, the SessionDiscovery used by `/resume`
    /// and `/session list|fork` — must each appear in a `spawn_blocking`
    /// closure. This test catches accidental regressions where someone
    /// inlines a sync filesystem call back onto the runtime thread (which
    /// freezes the TUI for hundreds of ms on long sessions).
    #[test]
    fn known_blocking_paths_are_wrapped_in_spawn_blocking() {
        let source = include_str!("slash_dispatch.rs");

        // /worktrees: the parse(porcelain) + list_sessions_by_git_root
        // bundle must happen on a blocking thread. Look for the
        // distinctive `list_sessions_by_git_root` call appearing inside
        // a `spawn_blocking` block.
        let worktrees_idx = source
            .find("\"/worktrees\"")
            .expect("/worktrees handler must exist");
        let worktrees_block_end = worktrees_idx
            + source[worktrees_idx..]
                .find("\n        }")
                .expect("/worktrees handler must close");
        let worktrees_block = &source[worktrees_idx..worktrees_block_end];
        assert!(
            worktrees_block.contains("spawn_blocking"),
            "/worktrees handler must wrap fs IO in spawn_blocking"
        );
        assert!(
            worktrees_block.contains("list_sessions_by_git_root"),
            "/worktrees handler should still enrich entries with session count"
        );

        // /timeline must build Timeline on a blocking thread.
        let timeline_idx = source
            .find("\"/timeline\"")
            .expect("/timeline handler must exist");
        let timeline_block_end = timeline_idx
            + source[timeline_idx..]
                .find("\n        }")
                .expect("/timeline handler must close");
        let timeline_block = &source[timeline_idx..timeline_block_end];
        assert!(
            timeline_block.contains("spawn_blocking"),
            "/timeline handler must wrap Timeline::new in spawn_blocking"
        );

        // SessionDiscovery::new must always be called inside spawn_blocking.
        // Allow the test snapshot blocks themselves to call it directly.
        for (idx, _) in source.match_indices("SessionDiscovery::new(") {
            let preceding = &source[..idx];
            let last_512 = if preceding.len() > 512 {
                &preceding[preceding.len() - 512..]
            } else {
                preceding
            };
            assert!(
                last_512.contains("spawn_blocking"),
                "SessionDiscovery::new at byte {idx} must be inside a spawn_blocking closure"
            );
        }
    }

    #[test]
    fn allow_dispatch_uses_shared_permission_parser() {
        let source = include_str!("slash_dispatch.rs");
        let start = source
            .find("\"/allow\" => {")
            .expect("/allow handler must exist");
        let end = start
            + source[start..]
                .find("\n        \"/instructions\"")
                .expect("/allow handler must close before /instructions");
        let allow_block = &source[start..end];

        assert!(allow_block.contains("parse_permission_command(args)"));
        for legacy in [
            "\"all\"",
            "\"default\"",
            "\"ask\"",
            "\"status\"",
            "\"accept-edits\"",
        ] {
            assert!(
                !allow_block.contains(legacy),
                "/allow dispatch must not reintroduce legacy token branch {legacy}"
            );
        }
    }
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
        crate::tui::testing::assert_tui_snapshot!(
            "panels_cheat_sheet",
            build_panels_cheat_sheet_lines().join("\n")
        );
    }

    #[test]
    fn config_entry_lists_all_subcommands() {
        let text = build_panels_cheat_sheet_lines().join("\n");
        // The /config entry MUST mention each subcommand so users
        // discover show / paths / sources / diff / export.
        for sub in &["show", "paths", "sources", "diff", "export"] {
            assert!(
                text.contains(sub),
                "cheat sheet /config entry missing subcommand: {sub}"
            );
        }
    }

    #[test]
    fn memory_entry_lists_all_subcommands() {
        let text = build_panels_cheat_sheet_lines().join("\n");
        // The /memory entry MUST mention list / search / inspect
        // so users discover they can search and inspect memories.
        for sub in &["list", "search", "inspect"] {
            assert!(
                text.contains(sub),
                "cheat sheet /memory entry missing subcommand: {sub}"
            );
        }
    }

    #[test]
    fn info_entry_shows_system_info() {
        let text = build_panels_cheat_sheet_lines().join("\n");
        assert!(text.contains("/info"), "cheat sheet missing /info");
        // The /info entry should mention key system details.
        for kw in &["version", "model", "session", "skills"] {
            assert!(
                text.contains(kw),
                "cheat sheet /info entry missing keyword: {kw}"
            );
        }
    }

    #[test]
    fn session_hub_view_emits_transcript_response() {
        let source = include_str!("slash_dispatch.rs");
        let start = source
            .find("fn handle_session_hub(ctx: &mut DispatchContext<'_>) -> SlashResult {")
            .expect("handle_session_hub must exist");
        let end = source[start..]
            .find("fn session_hub_persistence_error(")
            .map(|offset| start + offset)
            .expect("session hub helper should end before session_hub_persistence_error");
        let body = &source[start..end];
        assert!(
            body.contains("ctx.open_view("),
            "/session should emit a transcript response before opening the hub"
        );
    }

    #[test]
    fn session_views_emit_transcript_responses() {
        let source = include_str!("slash_dispatch.rs");

        fn body_between<'a>(source: &'a str, start: &str, end: &str) -> &'a str {
            let start_idx = source.find(start).expect(start);
            let end_idx = source[start_idx..]
                .find(end)
                .map(|offset| start_idx + offset)
                .expect(end);
            &source[start_idx..end_idx]
        }

        for (name, body, needles) in [
            (
                "session list",
                body_between(
                    source,
                    "async fn handle_session_list_view",
                    "fn handle_session_history_view",
                ),
                &["ctx.open_view(", "Opened session list"][..],
            ),
            (
                "session history",
                body_between(
                    source,
                    "fn handle_session_history_view",
                    "async fn handle_session_fork_view",
                ),
                &["ctx.show_response(", "Opened session history"][..],
            ),
            (
                "session fork",
                body_between(
                    source,
                    "async fn handle_session_fork_view",
                    "fn handle_session_analyze_view",
                ),
                &["ctx.open_view(", "Opened session fork picker"][..],
            ),
            (
                "session analysis",
                body_between(
                    source,
                    "fn push_analyze_summary",
                    "fn handle_session_export_view",
                ),
                &["ctx.open_view(", "Opened session analysis"][..],
            ),
        ] {
            for needle in needles {
                assert!(
                    body.contains(needle),
                    "{name} slash command must emit a transcript response: missing {needle}"
                );
            }
        }
        for needle in ["ctx.open_view(", "ctx.show_response("] {
            assert!(
                source.contains(needle),
                "session view slash commands must emit transcript responses: missing {needle}"
            );
        }
    }
}

#[cfg(test)]
mod routing_tests {
    use super::{
        CONTEXT_USAGE_MESSAGE, ConfigCommandRoute, MODEL_PICKER_FOOTER_HINT,
        MODEL_THINKING_PICKER_FOOTER_HINT, MemoryCommandRoute, config_command_route,
        inspect_command_supported, memory_command_route,
    };

    #[test]
    fn config_route_keeps_read_only_forms_on_text_path() {
        for form in [
            "show",
            "paths",
            "sources",
            "diff",
            "export ./runtime.toml",
            "help",
        ] {
            assert_eq!(
                config_command_route(form),
                Ok(ConfigCommandRoute::Fallback),
                "{form} should keep the text fallback behavior"
            );
        }
    }

    #[test]
    fn config_route_opens_panel_for_edit_forms() {
        assert_eq!(config_command_route(""), Ok(ConfigCommandRoute::Panel));
        assert_eq!(config_command_route("edit"), Ok(ConfigCommandRoute::Panel));
    }

    #[test]
    fn model_picker_footer_warns_about_thinking_follow_up() {
        assert!(MODEL_PICKER_FOOTER_HINT.contains("thinking mode"));
        assert!(MODEL_THINKING_PICKER_FOOTER_HINT.contains("finish model selection"));
    }

    #[test]
    fn memory_route_uses_panel_for_list_search_and_fallback_for_full_domain_commands() {
        assert_eq!(memory_command_route(""), Ok(MemoryCommandRoute::List));
        assert_eq!(memory_command_route("list"), Ok(MemoryCommandRoute::List));
        assert_eq!(memory_command_route("ls"), Ok(MemoryCommandRoute::List));
        assert_eq!(
            memory_command_route("search auth preferences"),
            Ok(MemoryCommandRoute::Search("auth preferences".into()))
        );
        assert_eq!(
            memory_command_route("show mem_123"),
            Ok(MemoryCommandRoute::Fallback)
        );
        assert_eq!(
            memory_command_route("inspect mem_123"),
            Ok(MemoryCommandRoute::Fallback)
        );
        assert_eq!(
            memory_command_route("search"),
            Ok(MemoryCommandRoute::Fallback)
        );
        assert_eq!(memory_command_route("stats"), Ok(MemoryCommandRoute::Stats));
        assert_eq!(
            memory_command_route("health"),
            Ok(MemoryCommandRoute::Health)
        );
        assert_eq!(
            memory_command_route("session"),
            Ok(MemoryCommandRoute::Fallback)
        );
        assert_eq!(
            memory_command_route("help"),
            Ok(MemoryCommandRoute::Fallback)
        );
    }

    #[test]
    fn inspect_route_accepts_known_subcommands_and_rejects_unknown_ones() {
        for form in [
            "", "budget", "tools", "context", "cache", "json", "diff", "history", "trace",
        ] {
            assert!(
                inspect_command_supported(form).is_ok(),
                "{form} should be accepted"
            );
        }
        assert_eq!(
            inspect_command_supported("mystery"),
            Err("Unknown `/inspect` subcommand: `mystery`.".into())
        );
    }

    #[test]
    fn context_usage_message_is_aligned() {
        assert_eq!(
            CONTEXT_USAGE_MESSAGE,
            "Usage: /context — open the context panel\n       /context dump [path] — write a JSON snapshot."
        );
    }
}

#[cfg(test)]
mod mcp_ux_tests {
    use super::{mcp_help_text, mcp_no_servers_text};

    #[tokio::test]
    async fn mcp_help_mentions_core_commands() {
        let state = crate::cli::session::session_state::SessionState::default();
        let text = mcp_help_text(&state).await;
        assert!(text.contains("/mcp list"), "missing list help: {text}");
        assert!(text.contains("/mcp tools"), "missing tools help: {text}");
        assert!(text.contains("/mcp read"), "missing read help: {text}");
        assert!(
            text.contains("/mcp inspect"),
            "missing inspect help: {text}"
        );
    }

    #[test]
    fn mcp_no_servers_text_guides_user_to_add_then_list() {
        let text = mcp_no_servers_text();
        assert!(text.contains("/mcp add"), "missing add guidance: {text}");
        assert!(text.contains("/mcp list"), "missing list guidance: {text}");
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
    use super::{handle_view_result, next_permission_mode_for_cycle};
    use crate::cli::permission_manager::PermissionMode;
    use crate::cli::session::session_state::SessionState;
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
            Some("Mode → Auto")
        );
    }

    #[test]
    fn accept_selection_updates_state_and_commits_feedback() {
        let mut state = SessionState::default();
        let mut bottom_pane = BottomPane::new();
        let mut chat_widget = ChatWidget::new("");

        handle_view_result("Edits", &mut state, &mut bottom_pane, &mut chat_widget);

        assert_eq!(state.perm_manager.mode(), PermissionMode::AcceptEdits);
        assert_eq!(
            last_system_message(&chat_widget).as_deref(),
            Some("Mode → Edits")
        );
    }

    #[test]
    fn plan_selection_updates_state_and_commits_feedback() {
        let mut state = SessionState::default();
        let mut bottom_pane = BottomPane::new();
        let mut chat_widget = ChatWidget::new("");

        handle_view_result("Plan", &mut state, &mut bottom_pane, &mut chat_widget);

        assert_eq!(state.perm_manager.mode(), PermissionMode::Plan);
        assert_eq!(
            last_system_message(&chat_widget).as_deref(),
            Some("Mode → Plan")
        );
    }

    #[test]
    fn default_selection_updates_state_and_commits_feedback() {
        let mut state = SessionState::default();
        state.perm_manager.set_mode(PermissionMode::Auto);
        let mut bottom_pane = BottomPane::new();
        let mut chat_widget = ChatWidget::new("");

        handle_view_result("Ask", &mut state, &mut bottom_pane, &mut chat_widget);

        assert_eq!(state.perm_manager.mode(), PermissionMode::Prompt);
        assert_eq!(
            last_system_message(&chat_widget).as_deref(),
            Some("Mode → Ask")
        );
    }

    #[test]
    fn permission_mode_cycle_skips_deny_and_wraps() {
        assert_eq!(
            next_permission_mode_for_cycle(PermissionMode::Prompt),
            PermissionMode::AcceptEdits
        );
        assert_eq!(
            next_permission_mode_for_cycle(PermissionMode::AcceptEdits),
            PermissionMode::Plan
        );
        assert_eq!(
            next_permission_mode_for_cycle(PermissionMode::Plan),
            PermissionMode::Auto
        );
        assert_eq!(
            next_permission_mode_for_cycle(PermissionMode::Auto),
            PermissionMode::Prompt
        );
        assert_eq!(
            next_permission_mode_for_cycle(PermissionMode::Deny),
            PermissionMode::AcceptEdits
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
    fn arbitrary_selection_does_not_mutate_model() {
        let mut state = SessionState::default();
        state.model = Some("deepseek-v4-pro".to_string());
        let mut bottom_pane = BottomPane::new();
        bottom_pane.footer.model = Some("deepseek-v4-pro".to_string());
        let mut chat_widget = ChatWidget::new("");

        handle_view_result(
            "[working] [@session/active] foo",
            &mut state,
            &mut bottom_pane,
            &mut chat_widget,
        );

        assert_eq!(state.model.as_deref(), Some("deepseek-v4-pro"));
        assert_eq!(bottom_pane.footer.model.as_deref(), Some("deepseek-v4-pro"));
        assert!(chat_widget.history().is_empty());
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
mod stats_view_tests {
    use super::build_recent_session_history_lines;
    use astra_services::session_journal::{self, JournalDirGuard};

    fn write_stats_session(session_id: &str) -> std::io::Result<()> {
        let writer = session_journal::JournalWriter::new(session_id)?;
        writer.append(&session_journal::JournalEvent::session_start(
            Some(session_id),
            Some("gpt-5"),
        ))?;
        writer.append(&session_journal::JournalEvent::turn(
            Some(session_id),
            1,
            Some("gpt-5"),
            "continue",
            "restored",
            0,
            15,
            7,
            8,
        ))?;
        Ok(())
    }

    #[test]
    #[serial_test::serial(stats_view)]
    fn build_recent_session_history_lines_surfaces_scan_error() {
        let tmp = crate::tests::test_temp_dir();
        let broken_root = tmp.path().join("broken-sessions-root");
        std::fs::write(&broken_root, "not-a-directory").expect("write broken root file");
        let _guard = JournalDirGuard::new(&broken_root);

        let error = build_recent_session_history_lines(10)
            .expect_err("session scan failure should surface");

        assert!(error.contains("failed to scan local sessions"), "{error}");
    }

    #[test]
    #[serial_test::serial(stats_view)]
    fn build_recent_session_history_lines_marks_unreadable_journals() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let good_session = format!("stats-good-{}", uuid::Uuid::new_v4());
        let bad_session = format!("stats-bad-{}", uuid::Uuid::new_v4());
        write_stats_session(&good_session).expect("write_stats_session");
        std::fs::create_dir_all(session_journal::journal_file_path(&bad_session)).unwrap();

        let rendered = build_recent_session_history_lines(10)
            .expect("history lines should still render")
            .join("\n");

        assert!(rendered.contains("journal unreadable"), "{rendered}");
        assert!(
            rendered.contains("Skipped 1 unreadable journal"),
            "{rendered}"
        );
        assert!(rendered.contains("Summary: 1 sessions"), "{rendered}");
    }

    #[test]
    #[serial_test::serial(stats_view)]
    fn build_recent_session_history_lines_surfaces_no_readable_sessions() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let bad_session = format!("stats-bad-only-{}", uuid::Uuid::new_v4());
        std::fs::create_dir_all(session_journal::journal_file_path(&bad_session)).unwrap();

        let rendered = build_recent_session_history_lines(10)
            .expect("history lines should still render")
            .join("\n");

        assert!(rendered.contains("journal unreadable"), "{rendered}");
        assert!(
            rendered.contains("Summary: no readable session data"),
            "{rendered}"
        );
        assert!(
            rendered.contains("Skipped 1 unreadable journal"),
            "{rendered}"
        );
    }

    #[test]
    #[serial_test::serial(stats_view)]
    fn read_session_journal_for_stats_surfaces_directory_error() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let session_id = format!("stats-dir-{}", uuid::Uuid::new_v4());
        std::fs::create_dir_all(session_journal::journal_file_path(&session_id)).unwrap();

        let error =
            crate::cli::session::session_stats_scan::read_session_journal_for_stats(&session_id)
                .expect_err("directory journal path should fail to read");

        assert!(error.contains("failed to read session journal"), "{error}");
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
mod session_hub_tests {
    use super::session_hub_persistence_error;
    use crate::cli::session::session_state::SessionState;
    use astra_services::session_workspace::WorkspaceMetadata;

    #[test]
    fn session_hub_persistence_error_prefers_live_state() {
        let state = SessionState {
            session_persistence_error: Some("live commit failed".into()),
            ..SessionState::default()
        };
        let mut ws = WorkspaceMetadata::new("sess-hub", "gpt-5");
        ws.last_persistence_error = Some("stale workspace error".into());

        assert_eq!(
            session_hub_persistence_error(&state, Some(&ws)).as_deref(),
            Some(
                "degraded: live commit failed · live session can continue; resume/fork metadata may be stale until the next successful save"
            )
        );
    }

    #[test]
    fn session_hub_persistence_error_falls_back_to_workspace_state() {
        let state = SessionState::default();
        let mut ws = WorkspaceMetadata::new("sess-hub", "gpt-5");
        ws.last_persistence_error = Some("workspace write failed".into());

        assert_eq!(
            session_hub_persistence_error(&state, Some(&ws)).as_deref(),
            Some(
                "degraded: workspace write failed · live session can continue; resume/fork metadata may be stale until the next successful save"
            )
        );
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

#[cfg(test)]
mod auth_error_tests {
    use super::is_astra_auth_error;

    #[test]
    fn matches_astra_session_auth_failures() {
        let msg =
            "request failed (401): invalid token\n  Hint: Authentication required — try /login";
        assert!(is_astra_auth_error(msg));
    }

    #[test]
    fn matches_bare_astra_401_without_known_body() {
        // Astra API returns "request failed (401): <anything>" — must trigger /login
        assert!(is_astra_auth_error(
            "request failed (401): unexpected auth state"
        ));
    }

    #[test]
    fn matches_authentication_failed() {
        assert!(is_astra_auth_error("Authentication failed"));
    }

    #[test]
    fn ignores_generic_upstream_401s() {
        assert!(!is_astra_auth_error("GitHub API Error: 401 Unauthorized"));
    }
}
