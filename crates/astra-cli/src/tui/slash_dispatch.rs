//! TUI-native slash command dispatch.
//!
//! Each command is handled inline without leaving the TUI. Commands that need
//! complex interactive UI push a `BottomPaneView`; commands that only produce
//! output render to scrollback. The dispatcher never swaps to a second
//! terminal UI to complete an action.

use crate::cli::command_registry;
use crate::cli::session::session_state::ExplainMode;
use crate::cli::session::session_state::SessionState;
use crate::tui::bottom_pane::BottomPane;
use crate::tui::bottom_pane::list_selection_view::{ListSelectionView, SelectionItem};
use crate::tui::bottom_pane::view::{
    BottomPaneView, ProjectInstructionsAction, StatsPanel, ViewResult,
};
use crate::tui::history_cell::system::SystemCell;
use crate::tui::terminal::TerminalGuard;

pub(crate) enum SlashResult {
    Handled,
    Deferred,
    /// Open the canonical root transcript workspace. The event loop owns the
    /// durable/local source selection because it also owns session binding,
    /// live suffix refresh and asynchronous page loading.
    OpenRootTranscript {
        session_id: Option<String>,
    },
    OpenBackgroundTasks,
    BackgroundRead(Box<SlashBackgroundRead>),
    Exit,
}

/// Session-lifecycle controls that must be honored before an input can enter
/// the active-run intent ledger. These controls are phase-independent: a
/// settling model turn must never reinterpret them as conversational input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImmediateControl {
    Exit,
    StopCurrentRun,
}

pub(crate) fn immediate_control(text: &str) -> Option<ImmediateControl> {
    let (command, _args) = parse_slash(text);
    match command_registry::resolve_command(command).ok()? {
        "/exit" => Some(ImmediateControl::Exit),
        "/stop" => Some(ImmediateControl::StopCurrentRun),
        _ => None,
    }
}

impl SlashResult {
    fn background_read(action: SlashBackgroundRead) -> Self {
        Self::BackgroundRead(Box::new(action))
    }
}

/// A read-only workbench action whose I/O is owned by the event loop rather
/// than the slash dispatcher. These actions have no mutable session effect,
/// so they can complete after the user keeps composing without borrowing UI
/// state across a filesystem or process wait.
pub(crate) enum SlashBackgroundRead {
    Clipboard {
        text: String,
        success_message: String,
    },
    Worktrees,
    Timeline {
        session_id: String,
    },
    ResumePicker,
    ForkPicker,
    SessionHub {
        snapshot: Box<SessionHubSnapshot>,
    },
    SessionAnalysis {
        session_id: String,
    },
    Reflection {
        session_id: String,
        api: astra_thin_client::ThinClient,
        profile: Option<String>,
        token: Option<String>,
        args: String,
    },
    Memory(MemoryReadRequest),
    Mcp {
        manager: McpManagerHandle,
        action: McpReadAction,
    },
    Context {
        breakdown: Box<crate::tui::context_panel::ContextBreakdown>,
        session_id: Option<String>,
        journal_dir_override: Option<std::path::PathBuf>,
    },
}

/// Immutable request captured when a read-only memory surface is submitted.
/// Authentication, local artifact reads and remote retrieval happen in the
/// background worker; the event loop only projects a typed completion.
pub(crate) enum MemoryReadRequest {
    Health,
    Session {
        session_id: String,
        api: astra_thin_client::ThinClient,
        profile: Option<String>,
    },
    Search {
        api: astra_thin_client::ThinClient,
        profile: Option<String>,
        query: String,
        top_k: usize,
        stats_view: bool,
    },
}

/// Read-only MCP surface requested from the workbench. Parsing happens while
/// the user submits the command; the manager lock and any provider I/O belong
/// to the background worker.
pub(crate) enum McpReadAction {
    Help,
    Overview,
    Servers,
    Tools(Option<String>),
    Prompts,
    Resources,
    Read(String),
    History,
    Inspect(String),
    Ping(Option<String>),
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
    /// Routed through `ChatWidget::commit_system` so the line lands in
    /// scrollback and the durable workbench transcript. Prompt continuation
    /// deliberately excludes these local UI/control events.
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
                let mut scored: Vec<(u32, &'static str)> =
                    crate::cli::command_registry::tui_commands()
                        .filter(|m| !m.name.contains(' '))
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

    if let Some(meta) = command_registry::resolve_command_meta(resolved)
        && !meta.is_available_in_tui()
    {
        ctx.show_error(format!(
            "`{resolved}` is not available in the workbench. Type `/` to browse actions that preserve this session."
        ));
        return SlashResult::Handled;
    }

    match resolved {
        // ── Exit ────────────────────────────────────────────────────
        "/exit" => SlashResult::Exit,
        "/stop" => {
            ctx.show_info("No active run to stop.".to_string());
            SlashResult::Handled
        }

        // ── Help ────────────────────────────────────────────────────
        "/help" => match help_command_route(args) {
            HelpCommandRoute::Commands => {
                use crate::tui::bottom_pane::help_view::HelpView;
                ctx.open_view("Opened command help", Box::new(HelpView::new()));
                SlashResult::Handled
            }
            HelpCommandRoute::Keys => {
                use crate::tui::bottom_pane::info_view::InfoView;
                ctx.open_view(
                    "Opened keyboard shortcuts",
                    Box::new(InfoView::from_key_value(
                        "Keyboard shortcuts",
                        keyboard_shortcut_pairs(),
                    )),
                );
                SlashResult::Handled
            }
            HelpCommandRoute::Unsupported => {
                ctx.show_error("Usage: /help [keys]".into());
                SlashResult::Handled
            }
        },

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
        //   /model clear             → clear the active model selection
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

        "/mcp" => handle_mcp_dispatch(args, ctx),

        "/task" if matches!(args.trim(), "" | "list") => {
            ctx.show_response("Opened background work".to_string());
            SlashResult::OpenBackgroundTasks
        }

        "/task" => {
            ctx.show_error("Usage: /task — opens background work".to_string());
            SlashResult::Handled
        }

        "/agent" if matches!(args.trim(), "" | "list") => {
            if crate::tui::agent_view::open_agents_view(ctx.chat_widget, ctx.bottom_pane) {
                ctx.show_response("Opened agent monitor".to_string());
            } else {
                ctx.show_info(
                    "No agent runs yet. Active and recent delegated work will appear here."
                        .to_string(),
                );
            }
            SlashResult::Handled
        }

        "/agent" => {
            ctx.show_error(
                "Usage: /agent [list] — select an agent in the workbench to inspect, guide, pause, resume, or stop it."
                    .to_string(),
            );
            SlashResult::Handled
        }

        "/plan" => {
            let trimmed = args.trim();
            if !trimmed.is_empty() {
                ctx.show_error(
                    "Usage: /plan — then describe the plan in the composer.".to_string(),
                );
                return SlashResult::Handled;
            }

            crate::cli::plan::plan_lifecycle::clear_pending_local_plan_entry_if_inactive(ctx.state);
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
            let view = ListSelectionView::new(items, Some("Stats — choose a view:".into()))
                .with_results(vec![
                    ViewResult::Stats(StatsPanel::Overview),
                    ViewResult::Stats(StatsPanel::History),
                    ViewResult::Stats(StatsPanel::Tools),
                    ViewResult::Stats(StatsPanel::Cost),
                    ViewResult::Stats(StatsPanel::Health),
                    ViewResult::Stats(StatsPanel::Learn),
                ]);
            ctx.open_view("Opened stats picker", Box::new(view));
            SlashResult::Handled
        }

        // ── Skills ──────────────────────────────────────────────────
        "/skill" => {
            match skill_command_route(args) {
                SkillCommandRoute::Browse => {
                    let skill_count = ctx
                        .state
                        .unified_skill_registry
                        .all_manifests()
                        .iter()
                        .filter(|manifest| manifest.user_invocable)
                        .count();
                    ctx.bottom_pane.composer.set_text("$");
                    ctx.bottom_pane.sync_popups();
                    ctx.show_response(format!(
                        "Opened skills · {skill_count} available · type to filter"
                    ));
                }
                SkillCommandRoute::Unsupported => ctx.show_error(
                    "This skill action has no workbench flow. Available here: `/skill` or `$` to browse and activate a skill."
                        .to_string(),
                ),
            }
            SlashResult::Handled
        }

        // ── Allow / permission mode ─────────────────────────────────
        "/allow" => {
            use crate::cli::permission_command::{
                PERMISSION_COMMAND_USAGE, PermissionCommandAction, parse_permission_command,
            };

            match parse_permission_command(args) {
                PermissionCommandAction::ChooseMode => {
                    ctx.open_view(
                        "Opened permission mode picker",
                        Box::new(build_permission_mode_picker(ctx.state.perm_manager.mode())),
                    );
                    SlashResult::Handled
                }
                PermissionCommandAction::SetMode(mode) => {
                    if permission_mode_requires_confirmation(mode) {
                        ctx.open_view(
                            "Confirm Bypass permission mode",
                            Box::new(build_permission_mode_confirmation(mode)),
                        );
                    } else {
                        ctx.state.perm_manager.set_mode(mode);
                        crate::cli::plan::plan_lifecycle::clear_pending_local_plan_entry_if_inactive(
                            ctx.state,
                        );
                        ctx.show_response(permission_mode_feedback(mode));
                    }
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
                    match tokio::fs::write(path, body).await {
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
        "/clear" => {
            if !args.trim().is_empty() {
                ctx.show_error("Usage: /clear".into());
                return SlashResult::Handled;
            }
            let Some(token) =
                crate::cli::session::session_runtime::fresh_access_token(ctx.api, ctx.profile)
                    .await
            else {
                ctx.show_error("Not logged in. Use /login.".into());
                return SlashResult::Handled;
            };
            match crate::cli::slash::slash_state::start_fresh_session(
                ctx.api,
                ctx.profile,
                &token,
                ctx.state,
            )
            .await
            {
                Ok(session_id) => {
                    ctx.show_response(format!("Started new session {session_id}"));
                    SlashResult::Handled
                }
                Err(error) => {
                    ctx.show_error(format!("Could not start a new session: {error}"));
                    SlashResult::Handled
                }
            }
        }

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

        // ── Reflect (TUI-native) ───────────────────────────────────
        //
        // Reflection is a read-only evidence surface. It shares the exact
        // local-first/server-fallback operation with line mode, then presents
        // its provenance and proposals in a scrollable workbench panel.
        "/reflect" => handle_reflect_dispatch(args, ctx),

        // ── Inspect (TUI-native) ────────────────────────────────────
        //
        // `/inspect` opens the current runtime snapshot in the same panel
        // system as the rest of the workbench. Focused facets belong in that
        // inspector rather than being a second, text-only command surface.
        "/inspect" => handle_inspect_dispatch(args, ctx),

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
            if let Some(path) = context_dump_argument(args_trim) {
                return handle_context_dump(path, ctx);
            }
            if !args_trim.is_empty() {
                ctx.show_info(CONTEXT_USAGE_MESSAGE.into());
                return SlashResult::Handled;
            }
            use crate::tui::bottom_pane::context_panel_view::ContextPanelView;
            use crate::tui::context_panel::ContextSnapshot;
            use crate::tui::context_panel::model::{
                ActiveSkill, RequestContextEvidence, RequestContextScope, SessionSummary,
            };

            // Collect human-readable previews the trace doesn't carry:
            // per-turn transcript snippets and already-observed process
            // state. Slash dispatch runs on the UI event path, so it must
            // not probe the filesystem or walk a git repository here.
            let mut snap = ContextSnapshot::default();
            snap.model = ctx.state.model.as_deref();
            (snap.cwd, snap.git_branch) = context_environment_from_footer(&ctx.bottom_pane.footer);

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
                canonical_conversation: ctx.state.active_conversation.as_ref().map(
                    |conversation| {
                        crate::tui::context_panel::model::CanonicalConversationEvidence {
                            cursor: conversation.cursor().clone(),
                            source: conversation.source(),
                        }
                    },
                ),
                request_context: ctx.bottom_pane.footer.context_window.map(|usage| {
                    let scope = if ctx.bottom_pane.footer.context_window_is_previous() {
                        RequestContextScope::PreviousRequestWhileAssembling
                    } else if ctx.bottom_pane.footer.is_turn_active {
                        RequestContextScope::CurrentRequest
                    } else {
                        RequestContextScope::LastCompletedRequest
                    };
                    RequestContextEvidence {
                        usage,
                        scope,
                        raw_window_tokens: ctx.bottom_pane.footer.raw_context_window_tokens,
                        token_usage: ctx.bottom_pane.footer.request_token_usage,
                    }
                }),
                continuation_anchor: ctx.state.continuation_anchor.clone(),
                queued_message: ctx.state.queued_message.clone(),
                diagnostics_context: ctx.state.diagnostics_context.clone(),
                read_activity: Default::default(),
            });

            // Local transcript cells and prompt-history trace records have
            // distinct identities. Keep the local list as an explicitly
            // labelled fallback for trace-less sessions; never pair the two
            // merely because their ordinal positions happen to match.
            snap.visible_conversation = collect_visible_conversation(ctx.chat_widget);

            let breakdown = context_breakdown_for_panel(ctx.state, &mut snap);
            let session_id = ctx
                .state
                .session_id
                .as_deref()
                .filter(|session_id| !session_id.is_empty())
                .map(str::to_owned);
            if session_id.is_some() {
                ctx.show_response("Loading context diagnostics".to_string());
                SlashResult::background_read(SlashBackgroundRead::Context {
                    breakdown: Box::new(breakdown),
                    session_id,
                    journal_dir_override:
                        astra_services::session_journal::current_journal_dir_override(),
                })
            } else {
                ctx.open_view(
                    "Opened context panel",
                    Box::new(ContextPanelView::new(breakdown)),
                );
                SlashResult::Handled
            }
        }

        // ── /config (the workbench editor) ────────────────────────────
        "/config" => match config_command_route(args) {
            Ok(ConfigCommandRoute::Panel) => {
                use crate::tui::bottom_pane::config_edit_view::ConfigEditView;
                let cfg = astra_config::runtime_config::RuntimeConfig::load();
                ctx.open_view("Opened config editor", Box::new(ConfigEditView::new(cfg)));
                SlashResult::Handled
            }
            Ok(ConfigCommandRoute::Unsupported) => {
                ctx.show_error(
                    "This config form has no workbench action. Use `/config` to edit runtime configuration."
                        .to_string(),
                );
                SlashResult::Handled
            }
            Err(usage) => {
                ctx.show_error(usage.to_string());
                SlashResult::Handled
            }
        },

        // ── Worktrees (TUI-native) ──────────────────────────────────
        "/worktrees" => {
            ctx.show_response("Loading worktrees…".into());
            SlashResult::background_read(SlashBackgroundRead::Worktrees)
        }

        // ── Session timeline (TUI-native) ───────────────────────────
        "/timeline" => {
            let Some(sid) = ctx.state.session_id.clone() else {
                ctx.show_info("No active session — /timeline needs a session id.".into());
                return SlashResult::Handled;
            };
            ctx.show_response("Loading session timeline…".into());
            SlashResult::background_read(SlashBackgroundRead::Timeline { session_id: sid })
        }

        // ── Resume picker (TUI-native) ──────────────────────────────
        "/resume" => {
            if !args.is_empty() {
                let session_id = args.trim();
                if !looks_like_session_id(session_id) {
                    ctx.show_error("Usage: /resume [session_id]".to_string());
                    return SlashResult::Handled;
                }
                match crate::cli::slash::slash_session::restore_session_into_state(
                    session_id,
                    ctx.profile,
                    ctx.api,
                    ctx.state,
                )
                .await
                {
                    Ok(()) => ctx.show_response(format!("Resumed session {session_id}")),
                    Err(error) => ctx.show_error(format!("Could not resume session: {error}")),
                }
                return SlashResult::Handled;
            }
            ctx.show_response("Loading previous sessions…".into());
            SlashResult::background_read(SlashBackgroundRead::ResumePicker)
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
        //   everything else          → explain the available workbench action
        "/session" => {
            let trimmed = args.trim();
            if trimmed.is_empty() {
                let snapshot = session_hub_snapshot(ctx.state);
                ctx.show_response("Loading session overview…".into());
                return SlashResult::background_read(SlashBackgroundRead::SessionHub {
                    snapshot: Box::new(snapshot),
                });
            }
            let (sub, rest) = split_sub(trimmed);
            match sub {
                "list" => {
                    ctx.show_response("Loading previous sessions…".into());
                    SlashResult::background_read(SlashBackgroundRead::ResumePicker)
                }
                "history" => match resolve_session_arg(ctx, rest) {
                    Some(session_id) => SlashResult::OpenRootTranscript {
                        session_id: Some(session_id),
                    },
                    None => SlashResult::Handled,
                },
                "fork" => {
                    ctx.show_response("Loading sessions to fork…".into());
                    SlashResult::background_read(SlashBackgroundRead::ForkPicker)
                }
                "analyze" | "diag" => {
                    // TUI-side analysis is a concise session summary. A deep
                    // text-only analyzer is not a separate workbench action.
                    let (flag, _) = split_sub(rest);
                    if flag == "deep" {
                        ctx.show_error(
                            "`/session analyze deep` has no workbench action. Use `/session analyze` for the available summary."
                                .to_string(),
                        );
                        SlashResult::Handled
                    } else {
                        match resolve_session_arg(ctx, rest) {
                            Some(session_id) => {
                                ctx.show_response("Loading session analysis…".into());
                                SlashResult::background_read(SlashBackgroundRead::SessionAnalysis {
                                    session_id,
                                })
                            }
                            None => SlashResult::Handled,
                        }
                    }
                }
                "export" => handle_session_export_view(ctx, rest).await,
                _ => {
                    ctx.show_error(
                        "This session action is not available in the workbench. Use `/session` for the available session views."
                            .to_string(),
                    );
                    SlashResult::Handled
                }
            }
        }

        // ── Copy last response ──────────────────────────────────────
        "/copy" => {
            match &ctx.state.last_response {
                Some(resp) if !resp.is_empty() => {
                    let n = resp.chars().count();
                    let preview: String = resp.chars().take(60).collect();
                    let suffix = if n > 60 { "…" } else { "" };
                    return SlashResult::background_read(SlashBackgroundRead::Clipboard {
                        text: resp.clone(),
                        success_message: format!("Copied {n} chars: {preview}{suffix}"),
                    });
                }
                _ => ctx.show_info("No response to copy".into()),
            }
            SlashResult::Handled
        }

        "/info" => {
            use crate::tui::bottom_pane::info_view::InfoView;

            let model = ctx.state.model.as_deref().unwrap_or("<unset>");
            let session = ctx.state.session_id.as_deref().unwrap_or("<none>");
            let perm = ctx.state.perm_manager.mode().chip_text().to_owned();
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

        // ── History — canonical transcript workspace ─────────────────
        // `/history` used to open a second, in-memory `(user, assistant)`
        // list. That view silently omitted tools, reasoning, compacted turns,
        // delegated runs and server history. One conversation must have one
        // browser and one source-selection contract, so this command is now a
        // discoverable alias for the same workspace as Ctrl+O.
        "/history" => match history_command_route(args) {
            HistoryCommandRoute::Transcript => SlashResult::OpenRootTranscript { session_id: None },
            HistoryCommandRoute::Unsupported => {
                ctx.show_error(
                    "Usage: /history — open the transcript, then press / to search.".into(),
                );
                SlashResult::Handled
            }
        },

        // ── Instructions — subcommand menu or direct action ─────────
        "/instructions" => {
            use crate::tui::bottom_pane::info_view::InfoView;
            match args {
                "" => {
                    let cwd = std::env::current_dir().unwrap_or_default();
                    let inst_path = cwd.join(".astra").join("instructions.md");

                    let file_info = if let Ok(meta) = tokio::fs::metadata(&inst_path).await {
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
                        Box::new(
                            ListSelectionView::new(items, Some("Project Instructions:".into()))
                                .with_results(vec![
                                    ViewResult::Instructions(ProjectInstructionsAction::Show),
                                    ViewResult::Instructions(ProjectInstructionsAction::Reload),
                                    ViewResult::Instructions(ProjectInstructionsAction::Disable),
                                ]),
                        ),
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

            if route == MemoryCommandRoute::Unsupported {
                ctx.show_error(
                    "This memory action has no workbench UI. Available: `/memory`, `/memory search <query>`, `/memory stats`, `/memory health`, `/memory session`."
                        .to_string(),
                );
                return SlashResult::Handled;
            }

            if route == MemoryCommandRoute::Health {
                ctx.show_response("Loading memory health…".into());
                return SlashResult::background_read(SlashBackgroundRead::Memory(
                    MemoryReadRequest::Health,
                ));
            };

            if route == MemoryCommandRoute::Session {
                let Some(session_id) = ctx.state.session_id.clone() else {
                    ctx.show_error("No active session yet.".into());
                    return SlashResult::Handled;
                };
                ctx.show_response("Loading session memory…".into());
                return SlashResult::background_read(SlashBackgroundRead::Memory(
                    MemoryReadRequest::Session {
                        session_id,
                        api: ctx.api.clone(),
                        profile: ctx.profile.map(str::to_owned),
                    },
                ));
            }

            let (query, top_k, stats_view, list_view) = match route {
                MemoryCommandRoute::Search(query) => (query, 20, false, false),
                MemoryCommandRoute::List => (
                    crate::cli::slash::slash_memory::MEMORY_BROWSE_QUERY.to_string(),
                    crate::cli::slash::slash_memory::MEMORY_BROWSE_TOP_K,
                    false,
                    true,
                ),
                MemoryCommandRoute::Stats => (
                    crate::cli::slash::slash_memory::MEMORY_BROWSE_QUERY.to_string(),
                    crate::cli::slash::slash_memory::MEMORY_STATS_TOP_K,
                    true,
                    false,
                ),
                MemoryCommandRoute::Session => unreachable!("handled above"),
                MemoryCommandRoute::Unsupported => unreachable!("handled above"),
                MemoryCommandRoute::Health => unreachable!("handled above"),
            };

            let progress = if stats_view {
                "Loading memory stats…"
            } else if list_view {
                "Loading memories…"
            } else {
                "Searching memories…"
            };
            ctx.show_response(progress.into());
            SlashResult::background_read(SlashBackgroundRead::Memory(MemoryReadRequest::Search {
                api: ctx.api.clone(),
                profile: ctx.profile.map(str::to_owned),
                query,
                top_k,
                stats_view,
            }))
        }

        // A native registry entry without a dispatcher is a product bug. It
        // must never fall through into a user prompt or a terminal handoff.
        _ => {
            ctx.show_error(format!(
                "Command `{resolved}` is registered but has no workbench action."
            ));
            SlashResult::Handled
        }
    }
}

/// Build `/context` from the same committed trace that session recovery and
/// `/inspect` use. Streaming/server paths may commit a trace into
/// `SessionState` without mirroring it into the local observability ring, so
/// treating that ring as the only source makes a completed turn appear to
/// have no context. The ring remains a fallback for older local traces and
/// retains cross-turn compaction history.
fn context_breakdown_for_panel(
    state: &SessionState,
    snapshot: &mut crate::tui::context_panel::ContextSnapshot<'_>,
) -> crate::tui::context_panel::ContextBreakdown {
    use crate::tui::context_panel::ContextBreakdown;

    if let Some(session) = state.observability_session.as_ref() {
        let guard = astra_core::sync_poison::recover_rwlock_read(session);
        // Pull session-level compaction history into the snapshot so the
        // Compaction section shows all past events, not just the latest turn.
        snapshot.compressed_turns = guard.compressed_turns.clone();
    }

    if let Some(trace) = state.latest_context_assembly_trace.as_ref() {
        return ContextBreakdown::from_trace_with(trace, snapshot);
    }

    let Some(session) = state.observability_session.as_ref() else {
        return ContextBreakdown::from_snapshot_without_trace(snapshot);
    };
    let guard = astra_core::sync_poison::recover_rwlock_read(session);
    guard
        .context_traces
        .last()
        .map(|trace| ContextBreakdown::from_trace_with(trace, snapshot))
        .unwrap_or_else(|| ContextBreakdown::from_snapshot_without_trace(snapshot))
}

pub(crate) fn build_permission_mode_picker(
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
            name: "Read-only".into(),
            description: Some(
                "Read-only tool capability; /plan uses this policy while authoring a workflow"
                    .into(),
            ),
            is_current: current == crate::cli::permission_manager::PermissionMode::Plan,
        },
        SelectionItem {
            name: "Auto".into(),
            description: Some(
                "Auto-approve normal tool risk; some git/sensitive gates may still stop".into(),
            ),
            is_current: current == crate::cli::permission_manager::PermissionMode::Auto,
        },
        SelectionItem {
            name: "Bypass".into(),
            description: Some(
                "Skip approval prompts; catastrophic and policy hard-denies still apply".into(),
            ),
            is_current: current == crate::cli::permission_manager::PermissionMode::Bypass,
        },
        SelectionItem {
            name: "Deny".into(),
            description: Some("Deny all tool calls".into()),
            is_current: current == crate::cli::permission_manager::PermissionMode::Deny,
        },
    ];
    ListSelectionView::new(
        items,
        Some("Tool policy · enter planning workflow with /plan".into()),
    )
    .with_results(vec![
        ViewResult::Permission(crate::cli::permission_manager::PermissionMode::Prompt),
        ViewResult::Permission(crate::cli::permission_manager::PermissionMode::AcceptEdits),
        ViewResult::Permission(crate::cli::permission_manager::PermissionMode::Plan),
        ViewResult::Permission(crate::cli::permission_manager::PermissionMode::Auto),
        ViewResult::Permission(crate::cli::permission_manager::PermissionMode::Bypass),
        ViewResult::Permission(crate::cli::permission_manager::PermissionMode::Deny),
    ])
    .with_footer_hint(
        "↑↓ select · Enter apply · Esc keep current · /allow rules · /allow trust · /allow trace",
    )
}

pub(crate) fn build_permission_mode_confirmation(
    mode: crate::cli::permission_manager::PermissionMode,
) -> ListSelectionView {
    debug_assert!(permission_mode_requires_confirmation(mode));
    ListSelectionView::new(
        vec![
            SelectionItem {
                name: "Keep current permissions".into(),
                description: Some("No policy change".into()),
                is_current: true,
            },
            SelectionItem {
                name: "Use Bypass".into(),
                description: Some(
                    "Skip approval prompts; real safety and policy boundaries still apply".into(),
                ),
                is_current: false,
            },
        ],
        Some("Confirm Bypass permission mode".into()),
    )
    .with_results(vec![
        ViewResult::PermissionConfirmation {
            mode,
            confirmed: false,
        },
        ViewResult::PermissionConfirmation {
            mode,
            confirmed: true,
        },
    ])
    .with_footer_hint("Enter confirm · Esc keep current permissions")
}

pub(crate) fn permission_mode_requires_confirmation(
    mode: crate::cli::permission_manager::PermissionMode,
) -> bool {
    matches!(mode, crate::cli::permission_manager::PermissionMode::Bypass)
}

pub(crate) fn permission_mode_feedback(
    mode: crate::cli::permission_manager::PermissionMode,
) -> String {
    crate::cli::permission_command::permission_mode_feedback(mode)
}

pub(crate) fn apply_permission_mode_selection(
    state: &mut SessionState,
    bottom_pane: &mut BottomPane,
    chat_widget: &mut crate::tui::chat_widget::ChatWidget,
    mode: crate::cli::permission_manager::PermissionMode,
) {
    state.perm_manager.set_mode(mode);
    crate::cli::plan::plan_lifecycle::clear_pending_local_plan_entry_if_inactive(state);
    let released = bottom_pane.reevaluate_approvals_for_mode(mode);
    chat_widget.commit_system(SystemCell::response(permission_mode_feedback(mode)));
    if released > 0 {
        chat_widget.commit_system(SystemCell::response(format!(
            "{released} pending approval(s) resolved by the selected permission mode"
        )));
    }
}

fn apply_or_confirm_permission_mode_selection(
    state: &mut SessionState,
    bottom_pane: &mut BottomPane,
    chat_widget: &mut crate::tui::chat_widget::ChatWidget,
    mode: crate::cli::permission_manager::PermissionMode,
) {
    if permission_mode_requires_confirmation(mode) {
        bottom_pane.push_view(Box::new(build_permission_mode_confirmation(mode)));
    } else {
        apply_permission_mode_selection(state, bottom_pane, chat_widget, mode);
    }
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
    result: ViewResult,
    state: &mut SessionState,
    bottom_pane: &mut BottomPane,
    chat_widget: &mut crate::tui::chat_widget::ChatWidget,
) {
    match result {
        ViewResult::Stats(panel) => {
            let sub = match panel {
                StatsPanel::Overview => "",
                StatsPanel::History => "history",
                StatsPanel::Tools => "tools",
                StatsPanel::Cost => "cost",
                StatsPanel::Health => "health",
                StatsPanel::Learn => "learn",
            };
            show_stats_view(sub, state, bottom_pane);
        }
        ViewResult::Instructions(ProjectInstructionsAction::Show) => {
            use crate::tui::bottom_pane::info_view::InfoView;
            if let Some(ref pi) = state.project_instructions {
                let lc = pi.lines().count();
                bottom_pane.push_view(Box::new(
                    InfoView::from_plain(
                        &format!("Project Instructions ({lc} lines)"),
                        pi.lines().map(|line| format!("  {line}")).collect(),
                    )
                    .with_reopen("/instructions"),
                ));
            } else {
                chat_widget.commit_system(SystemCell::info(
                    "No project instructions loaded. Create .astra/instructions.md",
                ));
            }
        }
        ViewResult::Instructions(ProjectInstructionsAction::Reload) => {
            if let Some(instructions) =
                crate::cli::project_instructions::discover_project_instructions()
            {
                let lines = instructions.lines().count();
                state.project_instructions = Some(instructions);
                chat_widget.commit_system(SystemCell::response(format!(
                    "Reloaded project instructions ({lines} lines)"
                )));
            } else {
                state.project_instructions = None;
                chat_widget.commit_system(SystemCell::info("No .astra/instructions.md found"));
            }
        }
        ViewResult::Instructions(ProjectInstructionsAction::Disable) => {
            state.project_instructions = None;
            chat_widget.commit_system(SystemCell::response("Project instructions disabled"));
        }
        ViewResult::Permission(mode) => {
            apply_or_confirm_permission_mode_selection(state, bottom_pane, chat_widget, mode)
        }
        ViewResult::PermissionConfirmation {
            mode,
            confirmed: true,
        } => apply_permission_mode_selection(state, bottom_pane, chat_widget, mode),
        ViewResult::PermissionConfirmation {
            confirmed: false, ..
        } => {}
        ViewResult::Memory(memory) => {
            use crate::tui::bottom_pane::info_view::InfoView;
            bottom_pane.push_view(Box::new(
                InfoView::from_plain(
                    "Memory detail",
                    vec![
                        format!("id: {}", memory.memory_id),
                        String::new(),
                        memory.content,
                    ],
                )
                .with_reopen("/memory"),
            ));
        }
        ViewResult::InsertCommand(command) => {
            bottom_pane.composer.set_text(&format!("{command} "));
        }
        // These results have async or state-transition handling in the event
        // loop. Keeping them explicit prevents a future picker from silently
        // falling through based on its rendered label.
        ViewResult::Login { .. }
        | ViewResult::Register { .. }
        | ViewResult::ConfigEdit { .. }
        | ViewResult::Model { .. }
        | ViewResult::ModelThinking { .. }
        | ViewResult::Session { .. }
        | ViewResult::WorkspaceTrust(_) => {}
    }
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
                        "${:.3}/1M prompt, ${:.3}/1M completion",
                        pricing.prompt * 1_000_000.0,
                        pricing.completion * 1_000_000.0
                    ),
                ),
                (
                    "prompt",
                    format!(
                        "{} ({})",
                        state.total_prompt_tokens,
                        crate::cli::slash::slash_stats::format_cost(
                            state.total_prompt_tokens as f64 * pricing.prompt
                        )
                    ),
                ),
                (
                    "completion",
                    format!(
                        "{} ({})",
                        state.total_completion_tokens,
                        crate::cli::slash::slash_stats::format_cost(
                            state.total_completion_tokens as f64 * pricing.completion
                        )
                    ),
                ),
            ];
            if state.total_cache_read_tokens > 0 {
                let rate = pricing.cache_read.unwrap_or(pricing.prompt);
                pairs.push((
                    "cache read",
                    format!(
                        "{} ({})",
                        state.total_cache_read_tokens,
                        crate::cli::slash::slash_stats::format_cost(
                            state.total_cache_read_tokens as f64 * rate
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

fn handle_mcp_dispatch(args: &str, ctx: &mut DispatchContext<'_>) -> SlashResult {
    use crate::cli::slash::slash_mcp::ParsedMcpCommand as Cmd;

    let action = match crate::cli::slash::slash_mcp::parse_mcp_command(args) {
        Cmd::Help => McpReadAction::Help,
        Cmd::Overview => McpReadAction::Overview,
        Cmd::Servers => McpReadAction::Servers,
        Cmd::Tools(server) => McpReadAction::Tools(server.map(str::to_owned)),
        Cmd::Prompts => McpReadAction::Prompts,
        Cmd::Resources => McpReadAction::Resources,
        Cmd::Read(Some(spec)) => McpReadAction::Read(spec.to_owned()),
        Cmd::Read(None) => {
            ctx.show_error("Usage: /mcp read <server>:<uri>".into());
            return SlashResult::Handled;
        }
        Cmd::History => McpReadAction::History,
        Cmd::Inspect(Some(query)) => McpReadAction::Inspect(query.to_owned()),
        Cmd::Inspect(None) => {
            ctx.show_error(
                "Usage: /mcp inspect <server>:<tool>  ·  try `/mcp tools` first.".into(),
            );
            return SlashResult::Handled;
        }
        Cmd::Ping(server) => McpReadAction::Ping(server.map(str::to_owned)),
        Cmd::Add(_) => return mcp_unavailable_notice(ctx, "add"),
        Cmd::Remove(_) => return mcp_unavailable_notice(ctx, "remove"),
        Cmd::Subscribe(_) => return mcp_unavailable_notice(ctx, "subscribe"),
        Cmd::Unsubscribe(_) => return mcp_unavailable_notice(ctx, "unsubscribe"),
        Cmd::LogLevel(_) => return mcp_unavailable_notice(ctx, "log-level"),
        Cmd::Prompt(_) => return mcp_unavailable_notice(ctx, "prompt"),
        Cmd::Complete(_) => return mcp_unavailable_notice(ctx, "complete"),
        Cmd::Unknown(sub) => {
            ctx.show_error(format!(
                "Unknown `/mcp` subcommand: `{sub}`. Try `/mcp help`."
            ));
            return SlashResult::Handled;
        }
    };

    ctx.show_response("Loading MCP information…".into());
    SlashResult::background_read(SlashBackgroundRead::Mcp {
        manager: ctx.state.mcp_manager.clone(),
        action,
    })
}

fn mcp_unavailable_notice(ctx: &mut DispatchContext<'_>, subcommand: &str) -> SlashResult {
    ctx.show_error(format!(
        "`/mcp {subcommand}` has no workbench action. Available here: list, servers, tools, inspect, prompts, resources, read, ping, history."
    ));
    SlashResult::Handled
}

pub(crate) type McpManagerHandle =
    std::sync::Arc<tokio::sync::RwLock<crate::mcp_client::McpClientManager>>;

async fn mcp_help_text(manager: &McpManagerHandle) -> String {
    let count = manager.read().await.connection_count();
    let mut lines = vec!["MCP commands".to_string()];
    if count == 0 {
        lines.push("No MCP servers connected yet.".into());
        lines
            .push("Configure a server before starting the workbench, then use `/mcp list`.".into());
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
    lines.push("/mcp history              recent MCP tool-call history".into());
    lines.join("\n")
}

fn mcp_no_servers_text() -> String {
    "No MCP servers connected. Configure a server before starting the workbench, then use `/mcp list` or `/mcp tools`.".into()
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

async fn mcp_overview_text(manager: &McpManagerHandle) -> String {
    let manager = manager.read().await;
    let count = manager.connection_count();
    if count == 0 {
        return mcp_no_servers_text();
    }

    let tools = manager.all_tools().len();
    let prompts = manager.all_prompts().await.len();
    let resources = manager.all_resources().await.len();
    let mut lines = vec![
        "MCP overview".into(),
        format!("Servers: {count} connected"),
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

async fn mcp_servers_text(manager: &McpManagerHandle) -> String {
    let manager = manager.read().await;
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

async fn mcp_tools_text(manager: &McpManagerHandle, server: Option<&str>) -> String {
    let manager = manager.read().await;
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

async fn mcp_prompts_text(manager: &McpManagerHandle) -> String {
    let manager = manager.read().await;
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

async fn mcp_resources_text(manager: &McpManagerHandle) -> String {
    let manager = manager.read().await;
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

async fn mcp_read_text(manager: &McpManagerHandle, spec: &str) -> String {
    let spec = spec.trim();
    let (server_name, uri) = match spec.split_once(':') {
        Some((server_name, uri)) if !server_name.is_empty() && !uri.is_empty() => {
            (server_name, uri)
        }
        _ => return "Usage: /mcp read <server>:<uri>".into(),
    };

    let conn = {
        let manager = manager.read().await;
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

async fn mcp_history_text(manager: &McpManagerHandle) -> String {
    let connections = {
        let manager = manager.read().await;
        if manager.connection_count() == 0 {
            return mcp_no_servers_text();
        }
        manager
            .connected_servers()
            .into_iter()
            .filter_map(|name| manager.get(name))
            .collect::<Vec<_>>()
    };

    let mut entries = Vec::new();
    for conn in connections {
        let log = conn.call_log.read().await;
        entries.extend(log.iter().cloned());
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

async fn mcp_inspect_text(manager: &McpManagerHandle, query: &str) -> String {
    let manager = manager.read().await;
    match crate::cli::slash::slash_mcp::resolve_protocol_tool_query(&manager, query) {
        Ok((server, tool)) => mcp_protocol_tool_text(server, tool),
        Err(protocol_error) => {
            for meta in astra_turn_core::tool::registry::meta::TOOL_CATALOG {
                if meta.name == query {
                    return mcp_builtin_tool_text(meta);
                }
            }
            protocol_error
        }
    }
}

async fn mcp_ping_text(manager: &McpManagerHandle, server: Option<&str>) -> String {
    let requested = server.map(str::trim).filter(|name| !name.is_empty());
    let connections = {
        let manager = manager.read().await;
        if manager.connection_count() == 0 {
            return mcp_no_servers_text();
        }
        match requested {
            Some(name) => match manager.get(name) {
                Some(connection) => vec![(name.to_string(), connection)],
                None => {
                    return format!(
                        "Server '{name}' not found. Try `/mcp list` or `/mcp servers`."
                    );
                }
            },
            None => manager
                .connected_servers()
                .into_iter()
                .filter_map(|name| {
                    manager
                        .get(name)
                        .map(|connection| (name.to_string(), connection))
                })
                .collect::<Vec<_>>(),
        }
    };

    let mut lines = Vec::with_capacity(connections.len());
    for (name, connection) in connections {
        let started = std::time::Instant::now();
        match connection.ping().await {
            Ok(()) => lines.push(format!(
                "✓ {name}: {:.1}ms",
                started.elapsed().as_secs_f64() * 1000.0
            )),
            Err(error) => lines.push(format!("✗ {name}: {error}")),
        }
    }
    lines.join("\n")
}

/// Execute one already-parsed MCP workbench read. This owns all manager
/// contention and provider waits, and is deliberately invoked only by the
/// event loop's background-read worker.
pub(crate) async fn execute_mcp_read(manager: McpManagerHandle, action: McpReadAction) -> String {
    match action {
        McpReadAction::Help => mcp_help_text(&manager).await,
        McpReadAction::Overview => mcp_overview_text(&manager).await,
        McpReadAction::Servers => mcp_servers_text(&manager).await,
        McpReadAction::Tools(server) => mcp_tools_text(&manager, server.as_deref()).await,
        McpReadAction::Prompts => mcp_prompts_text(&manager).await,
        McpReadAction::Resources => mcp_resources_text(&manager).await,
        McpReadAction::Read(spec) => mcp_read_text(&manager, &spec).await,
        McpReadAction::History => mcp_history_text(&manager).await,
        McpReadAction::Inspect(query) => mcp_inspect_text(&manager, &query).await,
        McpReadAction::Ping(server) => mcp_ping_text(&manager, server.as_deref()).await,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigCommandRoute {
    Panel,
    Unsupported,
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
            Ok(ConfigCommandRoute::Unsupported)
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
    Session,
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SkillCommandRoute {
    Browse,
    Unsupported,
}

fn skill_command_route(args: &str) -> SkillCommandRoute {
    match args.trim() {
        "" | "browse" | "list" => SkillCommandRoute::Browse,
        _ => SkillCommandRoute::Unsupported,
    }
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
        "session" if rest.is_empty() => Ok(MemoryCommandRoute::Session),
        _ => Ok(MemoryCommandRoute::Unsupported),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HelpCommandRoute {
    Commands,
    Keys,
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HistoryCommandRoute {
    Transcript,
    Unsupported,
}

fn history_command_route(args: &str) -> HistoryCommandRoute {
    if args.trim().is_empty() {
        HistoryCommandRoute::Transcript
    } else {
        HistoryCommandRoute::Unsupported
    }
}

fn help_command_route(args: &str) -> HelpCommandRoute {
    match args.trim() {
        "" => HelpCommandRoute::Commands,
        "keys" => HelpCommandRoute::Keys,
        _ => HelpCommandRoute::Unsupported,
    }
}

/// Key bindings that are meaningful in the current workbench.  This is an
/// action-oriented panel, not a dump of every editor binding: it answers the
/// user's high-frequency questions about recovery, transcript navigation,
/// agent work, and input control.
fn keyboard_shortcut_pairs() -> Vec<(&'static str, String)> {
    vec![
        (
            "Enter",
            "send a message or run the selected slash action".into(),
        ),
        ("Shift+Enter", "insert a newline in the composer".into()),
        (
            "Ctrl+C",
            "clear draft, interrupt a run, or quit when idle".into(),
        ),
        (
            "Ctrl+O",
            "open the live root transcript; close returns to the prior workspace".into(),
        ),
        (
            "Ctrl+G",
            "open the run navigator; Enter/Right opens the selected conversation".into(),
        ),
        (
            "Shift+Left/Right",
            "switch among open root and agent conversation workspaces".into(),
        ),
        (
            "Ctrl+E",
            "toggle transcript thinking/tool details; composer keeps line-end behavior".into(),
        ),
        (
            "Alt+E",
            "open the current composer draft in the external editor".into(),
        ),
        (
            "Ctrl+R",
            "when idle with an empty composer, restore the last user draft".into(),
        ),
        (
            "Ctrl+B",
            format!(
                "{} promotes eligible foreground work to a background task",
                crate::tui::background_shortcut::ctrl_b_background_shortcut()
            ),
        ),
        (
            "Shift+Down",
            "open and manage live background tasks and local agents".into(),
        ),
        (
            "/ then Tab",
            "browse and complete native workbench actions".into(),
        ),
        ("$", "browse available skills".into()),
    ]
}

const CONTEXT_USAGE_MESSAGE: &str = "Usage: /context — open the context panel\n       /context dump [path] — write a JSON snapshot.";

/// Return the optional dump path only when `dump` is the complete first token.
/// Prefixes such as `dumpster` are ordinary invalid `/context` arguments.
fn context_dump_argument(args: &str) -> Option<&str> {
    let mut tokens = args.splitn(2, char::is_whitespace);
    (tokens.next() == Some("dump")).then(|| tokens.next().unwrap_or_default().trim())
}

// ── /model subcommand helpers ───────────────────────────────────

pub(crate) const MODEL_PICKER_FOOTER_HINT: &str =
    "Type to filter | Enter to choose | Some models then ask for thinking mode | Esc to go back";
pub(crate) const MODEL_THINKING_PICKER_FOOTER_HINT: &str =
    "Type to filter | Enter to finish model selection | Esc to go back";

/// `/model` with no args (or `list`) — fetch the catalog and push
/// the picker. The typed result lets the outer loop check the model's
/// `thinking_capability` and
/// either commits or pushes a thinking-mode picker.
/// Whether a slash submission asks to browse the model catalog. Kept separate
/// from direct model selection so the event loop can fetch the catalog without
/// borrowing its UI state across a network wait.
pub(crate) fn is_model_picker_request(text: &str) -> bool {
    let (command, args) = parse_slash(text);
    matches!(command_registry::resolve_command(command), Ok("/model"))
        && matches!(args.trim(), "" | "list")
}

/// Build the model picker view from a fetched model list and push it.
///
/// This is deliberately a synchronous UI projection: networking belongs to
/// [`load_model_catalog`], so callers never have to hold the TUI loop while
/// waiting for a remote catalog.
pub(crate) fn push_model_picker(
    state: &SessionState,
    bottom_pane: &mut BottomPane,
    chat_widget: &mut crate::tui::chat_widget::ChatWidget,
    models: Vec<String>,
) -> bool {
    // Strip any `-thinking:*` suffix from the cached model when
    // highlighting the current row — the picker shows base names only,
    // and the suffix is re-applied by the thinking stage.
    let current_raw = state.model.clone().unwrap_or_default();
    let current_base = current_raw
        .split_once("-thinking:")
        .map(|(b, _)| b.to_string())
        .unwrap_or(current_raw);
    let items: Vec<SelectionItem> = models
        .iter()
        .map(|m| {
            let is_current = *m == current_base;
            SelectionItem {
                name: m.clone(),
                description: None,
                is_current,
            }
        })
        .collect();
    if items.is_empty() {
        chat_widget.commit_system(SystemCell::info("No models available"));
        false
    } else {
        let view = ListSelectionView::new(items, Some("Select model:".into()))
            .with_footer_hint(MODEL_PICKER_FOOTER_HINT)
            .with_results(
                models
                    .into_iter()
                    .map(|name| ViewResult::Model { name })
                    .collect(),
            );
        chat_widget.commit_system(SystemCell::response("Opened model picker"));
        bottom_pane.push_view(Box::new(view));
        true
    }
}

fn model_catalog_error_message(
    error: &crate::cli::slash::slash_router::ModelCatalogError,
) -> String {
    if error.is_authentication_failure() {
        "Not authorized — try /login first".into()
    } else if error.is_transport_failure() {
        "Cannot reach server — check connection".into()
    } else {
        format!(
            "Failed to fetch models: {}",
            error.to_string().lines().next().unwrap_or("unknown error")
        )
    }
}

/// Fetch the full active model catalog, including provider and thinking
/// metadata. The caller owns scheduling; this function is free of TUI
/// references so it can run outside the input event loop.
pub(crate) async fn load_model_catalog(
    api: astra_thin_client::ThinClient,
    profile: Option<String>,
) -> Result<Vec<crate::cli::slash::slash_router::ModelCatalogEntry>, String> {
    let token =
        crate::cli::session::session_runtime::fresh_access_token(&api, profile.as_deref()).await;
    match crate::cli::slash::slash_router::fetch_model_catalog(&api, token.as_deref()).await {
        Ok(models) => Ok(models),
        Err(error) => {
            if !error.is_authentication_failure() {
                return Err(model_catalog_error_message(&error));
            }

            if crate::cli::session::session_runtime::attempt_token_refresh(&api, profile.as_deref())
                .await
            {
                let refreshed =
                    crate::cli::session::session_runtime::current_access_token(profile.as_deref());
                match crate::cli::slash::slash_router::fetch_model_catalog(
                    &api,
                    refreshed.as_deref(),
                )
                .await
                {
                    Ok(models) => return Ok(models),
                    Err(retry_error) => {
                        if !retry_error.is_authentication_failure() {
                            return Err(model_catalog_error_message(&retry_error));
                        }
                    }
                }
            }
            Err("Not authorized — try /login first".into())
        }
    }
}

async fn open_model_picker(ctx: &mut DispatchContext<'_>) -> SlashResult {
    match load_model_catalog(ctx.api.clone(), ctx.profile.map(str::to_string)).await {
        Ok(models) => {
            let names = models
                .iter()
                .filter_map(crate::cli::slash::slash_router::entry_model_name)
                .map(ToOwned::to_owned)
                .collect();
            if push_model_picker(ctx.state, ctx.bottom_pane, ctx.chat_widget, names) {
                return SlashResult::Deferred;
            }
        }
        Err(error) => ctx.show_error(error),
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
        crate::cli::slash::slash_config::set_active_offering_id_for_request(None);
        ctx.bottom_pane.footer.model = None;
        ctx.show_response("Model selection cleared — choose a model before the next turn.".into());
        return;
    };
    ctx.state.model = Some(name.to_string());
    crate::cli::slash::slash_config::set_active_model_for_display(Some(name.to_string()));
    crate::cli::slash::slash_config::set_active_offering_id_for_request(None);
    ctx.bottom_pane.footer.model = Some(name.to_string());
    ctx.show_response(format!("Set model to {name}"));
}

/// `/model clear` — unset the session model. Reports the change to
/// scrollback so the user sees the footer switch.
async fn handle_model_clear(ctx: &mut DispatchContext<'_>) -> SlashResult {
    ctx.state.model = None;
    crate::cli::slash::slash_config::set_active_model_for_display(None);
    crate::cli::slash::slash_config::set_active_offering_id_for_request(None);
    ctx.bottom_pane.footer.model = None;
    ctx.show_response("Model selection cleared — choose a model before the next turn.".into());
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

/// Immutable input captured when `/session` is submitted. The background
/// workspace read must never inspect a later re-bound `SessionState`.
pub(crate) struct SessionHubSnapshot {
    pub(crate) session_id: String,
    turn: u32,
    model: String,
    total_cost: f64,
    max_budget: f64,
    prompt_tokens: u64,
    completion_tokens: u64,
    cache_read_tokens: u64,
    permission: String,
    explain: String,
    skills: usize,
    recent_tools: Option<String>,
    run_id: Option<String>,
    compactions: usize,
    journal_path: Option<String>,
    persistence_error: Option<String>,
    cwd_fallback: String,
}

pub(crate) fn session_hub_snapshot(state: &SessionState) -> SessionHubSnapshot {
    let compactions = state
        .observability_session
        .as_ref()
        .and_then(|observation| observation.try_read().ok())
        .map(|observation| observation.compressed_turns.len())
        .unwrap_or_default();
    let cwd_fallback = std::env::current_dir()
        .map(|path| tilde_session_path(&path.to_string_lossy()))
        .unwrap_or_else(|_| "?".into());
    SessionHubSnapshot {
        session_id: state.session_id.clone().unwrap_or_default(),
        turn: state.turn,
        model: state.model.clone().unwrap_or_else(|| "—".into()),
        total_cost: state.total_session_cost,
        max_budget: state.max_budget_limit,
        prompt_tokens: state.total_prompt_tokens,
        completion_tokens: state.total_completion_tokens,
        cache_read_tokens: state.total_cache_read_tokens,
        permission: state.perm_manager.mode().to_string(),
        explain: state.explain.to_string(),
        skills: state.unified_skill_registry.len(),
        recent_tools: (!state.recent_tools.is_empty()).then(|| {
            state
                .recent_tools
                .iter()
                .take(6)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        }),
        run_id: state.run_id.clone(),
        compactions,
        journal_path: state
            .journal
            .as_ref()
            .map(|journal| tilde_session_path(&journal.path().display().to_string())),
        persistence_error: state.session_persistence_error.clone(),
        cwd_fallback,
    }
}

/// Build the session hub after its workspace read finishes. All live state was
/// captured in [`SessionHubSnapshot`] at submit time, keeping a late result
/// attributable to the session the user actually asked to inspect.
pub(crate) fn session_hub_view(
    snapshot: SessionHubSnapshot,
    workspace: Result<Option<astra_services::session_workspace::WorkspaceMetadata>, String>,
) -> crate::tui::bottom_pane::info_view::InfoView {
    use crate::tui::bottom_pane::info_view::InfoView;

    let sid_short = &snapshot.session_id[..snapshot.session_id.len().min(8)];
    let cumulative_tokens = snapshot
        .prompt_tokens
        .saturating_add(snapshot.completion_tokens);
    let mut pairs: Vec<(&str, String)> = vec![
        (
            "session id",
            if snapshot.session_id.is_empty() {
                "— (no active session)".into()
            } else {
                snapshot.session_id.clone()
            },
        ),
        ("turn", snapshot.turn.to_string()),
        ("model", snapshot.model.clone()),
    ];

    let (workspace, workspace_error) = match workspace {
        Ok(workspace) => (workspace, None),
        Err(error) => (None, Some(error)),
    };
    if let Some(ref ws) = workspace {
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
        pairs.push(("cwd", snapshot.cwd_fallback.clone()));
    }
    if let Some(error) =
        session_hub_persistence_error(snapshot.persistence_error.as_deref(), workspace.as_ref())
    {
        pairs.push(("persistence", error));
    }

    // Live state
    pairs.push(("cost", format!("${:.4}", snapshot.total_cost)));
    if snapshot.max_budget > 0.0 {
        pairs.push(("budget", format!("${:.2}", snapshot.max_budget)));
    }
    pairs.push(("prompt tokens", fmt_tokens(snapshot.prompt_tokens)));
    pairs.push(("completion tokens", fmt_tokens(snapshot.completion_tokens)));
    pairs.push(("cache-read tokens", fmt_tokens(snapshot.cache_read_tokens)));
    pairs.push(("total tokens", fmt_tokens(cumulative_tokens)));

    // Agent identity (from former /whoami)
    pairs.push(("permission", snapshot.permission));
    pairs.push(("explain", snapshot.explain));
    pairs.push(("skills", snapshot.skills.to_string()));
    if let Some(tools) = snapshot.recent_tools {
        pairs.push(("recent tools", tools));
    }
    if let Some(run_id) = snapshot.run_id {
        pairs.push(("run_id", run_id));
    }

    if snapshot.compactions > 0 {
        pairs.push(("compactions", snapshot.compactions.to_string()));
    }

    if let Some(journal_path) = snapshot.journal_path {
        pairs.push(("journal", journal_path));
    }

    // Action cheatsheet
    pairs.push(("", String::new()));
    pairs.push(("/session list", "pick a session to resume".into()));
    pairs.push(("/session history", "scroll transcript".into()));
    pairs.push(("/timeline", "per-turn trace timeline".into()));
    pairs.push(("/context", "context panel".into()));
    pairs.push(("/session fork", "branch a parallel session".into()));
    pairs.push(("/session export", "write markdown transcript".into()));

    let title = if snapshot.session_id.is_empty() {
        "Session · no active session".to_string()
    } else {
        format!("Session · {sid_short}")
    };
    InfoView::from_key_value(&title, pairs)
}

fn session_hub_persistence_error(
    state_error: Option<&str>,
    workspace: Option<&astra_services::session_workspace::WorkspaceMetadata>,
) -> Option<String> {
    state_error
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

/// Counter-only summary of a session journal — fast to compute
/// (no workspace reads, no per-event allocation) so the InfoView
/// renders instantly.  Users who want the deep report still get
/// it via `/session analyze deep [id]`.
pub(crate) fn session_analysis_view(
    sid: &str,
    events: &[astra_services::session_journal::JournalEvent],
) -> crate::tui::bottom_pane::info_view::InfoView {
    use crate::tui::bottom_pane::info_view::InfoView;
    use astra_services::session_journal::JournalEventType;

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
    for ev in events {
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
    InfoView::from_key_value(&format!("Session analyze · {sid_short}"), pairs)
}

async fn handle_session_export_view(ctx: &mut DispatchContext<'_>, arg: &str) -> SlashResult {
    let Some(sid) = resolve_session_arg(ctx, arg) else {
        return SlashResult::Handled;
    };
    let sid_for_export = sid.clone();
    let export = tokio::task::spawn_blocking(move || {
        let events = astra_services::session_journal::read_journal(&sid_for_export)
            .map_err(|error| format!("Failed to read journal: {error}"))?;
        if events.is_empty() {
            return Ok(None);
        }
        let (workspace, workspace_warning) =
            match astra_services::session_workspace::read_workspace_optional(&sid_for_export) {
                Ok(workspace) => (workspace, None),
                Err(error) => (
                    None,
                    Some(format!(
                        "workspace.yaml is invalid; export omits workspace health metadata: {error}"
                    )),
                ),
            };
        let markdown = crate::cli::slash::slash_session::build_export_markdown(
            &sid_for_export,
            workspace.as_ref(),
            &events,
        );
        // Default path mirrors the line-mode exporter so scripts see the same
        // artifact shape regardless of surface.
        let path = format!(
            "astra-session-{}.md",
            chrono::Local::now().format("%Y%m%d-%H%M")
        );
        std::fs::write(&path, markdown)
            .map_err(|error| format!("Failed to write {path}: {error}"))?;
        Ok(Some((path, workspace_warning)))
    })
    .await;
    match export {
        Ok(Ok(Some((path, workspace_warning)))) => {
            if let Some(warning) = workspace_warning {
                ctx.show_info(warning);
            }
            ctx.show_response(format!("Exported {sid} → {path}"));
        }
        Ok(Ok(None)) => {
            ctx.show_info(format!("Session {sid} has no journal events to export."));
        }
        Ok(Err(error)) => ctx.show_error(error),
        Err(error) => ctx.show_error(format!("Session export task failed: {error}")),
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

fn parse_slash(text: &str) -> (&str, &str) {
    let text = text.trim();
    match text.find(' ') {
        Some(pos) => (&text[..pos], text[pos..].trim()),
        None => (text, ""),
    }
}

/// Render a filesystem path for the `/context` Environment row.
/// Replaces `$HOME` with `~` so absolute paths stay short.
/// Context inspection consumes the footer's last confirmed environment
/// observation. Refreshing it belongs to the regular environment refresh
/// path, not a user-input dispatch branch.
fn context_environment_from_footer(
    footer: &crate::tui::bottom_pane::footer::Footer,
) -> (Option<String>, Option<String>) {
    (footer.cwd.clone(), footer.git_branch.clone())
}

/// Collect user-visible conversation cells as local evidence. This vector is
/// intentionally not keyed by `ContextAssemblyTrace::turn_index`: the trace
/// represents exact prompt groups, while the TUI transcript represents
/// rendered cells and may have a different shape after resume, tool activity,
/// or server-side compaction.
fn collect_visible_conversation(
    chat: &crate::tui::chat_widget::ChatWidget,
) -> Vec<crate::tui::context_panel::model::VisibleConversationItem> {
    use crate::tui::history_cell::{
        assistant::AssistantCell, reasoning::ReasoningCell, user::UserCell,
    };
    let mut visible = Vec::new();
    for cell in chat.history() {
        let any = cell.as_any_ref();
        if let Some(u) = any.downcast_ref::<UserCell>() {
            let body = u.text();
            let preview = one_line_preview(body);
            if !preview.is_empty() {
                visible.push(crate::tui::context_panel::model::VisibleConversationItem {
                    role: "user".into(),
                    preview,
                    body: body.to_string(),
                });
            }
        } else if let Some(a) = any.downcast_ref::<AssistantCell>() {
            let body = a.source();
            let preview = one_line_preview(body);
            if !preview.is_empty() {
                visible.push(crate::tui::context_panel::model::VisibleConversationItem {
                    role: "assistant".into(),
                    preview,
                    body: body.to_string(),
                });
            }
        } else if let Some(r) = any.downcast_ref::<ReasoningCell>() {
            let body = r.text();
            let preview = one_line_preview(body);
            if !preview.is_empty() {
                visible.push(crate::tui::context_panel::model::VisibleConversationItem {
                    role: "reasoning".into(),
                    preview,
                    body: body.to_string(),
                });
            }
        }
    }
    visible
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
fn handle_inspect_dispatch(args: &str, ctx: &mut DispatchContext<'_>) -> SlashResult {
    if !args.trim().is_empty() {
        ctx.show_error("Usage: /inspect".to_string());
        return SlashResult::Handled;
    }

    use crate::tui::bottom_pane::info_view::InfoView;
    let inspection = crate::cli::slash::slash_inspect::inspect_workbench(ctx.state);
    ctx.open_view(
        "Opened runtime inspector",
        Box::new(
            InfoView::from_inspection("Runtime Inspector", inspection)
                .with_primary_workspace()
                .with_reopen("/inspect"),
        ),
    );
    SlashResult::Handled
}

// ── /reflect dispatch ───────────────────────────────────────────────
fn handle_reflect_dispatch(args: &str, ctx: &mut DispatchContext<'_>) -> SlashResult {
    use crate::cli::session::session_runtime::current_access_token;
    use crate::cli::slash::slash_state::{is_reflect_diff_request, render_reflect_diff};
    use crate::tui::bottom_pane::info_view::InfoView;

    if is_reflect_diff_request(args) {
        let body = render_reflect_diff(ctx.state);
        let lines = body.lines().map(str::to_owned).collect();
        ctx.open_view(
            "Opened session reflection delta",
            Box::new(
                InfoView::from_plain("Reflection · Session Delta", lines).with_primary_workspace(),
            ),
        );
        return SlashResult::Handled;
    }

    let Some(session_id) = ctx
        .state
        .session_id
        .as_deref()
        .filter(|session_id| !session_id.trim().is_empty())
        .map(ToOwned::to_owned)
    else {
        ctx.show_error("Reflect needs an active session.".into());
        return SlashResult::Handled;
    };
    ctx.show_response("Loading session reflection…".into());
    SlashResult::background_read(SlashBackgroundRead::Reflection {
        session_id,
        api: ctx.api.clone(),
        profile: ctx.profile.map(str::to_owned),
        token: current_access_token(ctx.profile),
        args: args.to_owned(),
    })
}

#[cfg(test)]
mod routing_tests {
    use super::{
        CONTEXT_USAGE_MESSAGE, ConfigCommandRoute, HelpCommandRoute, HistoryCommandRoute,
        MODEL_PICKER_FOOTER_HINT, MODEL_THINKING_PICKER_FOOTER_HINT, MemoryCommandRoute,
        SkillCommandRoute, config_command_route, context_breakdown_for_panel,
        context_dump_argument, help_command_route, history_command_route, is_model_picker_request,
        keyboard_shortcut_pairs, memory_command_route, skill_command_route,
    };
    use crate::cli::session::session_state::SessionState;
    use crate::tui::context_panel::{
        ContextSnapshot,
        model::{SessionSummary, VisibleConversationItem},
    };
    use astra_turn_core::context_assembly_trace::ContextAssemblyTrace;

    #[test]
    fn config_route_marks_non_editor_forms_unavailable_in_tui() {
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
                Ok(ConfigCommandRoute::Unsupported),
                "{form} has no native editor action"
            );
        }
    }

    #[test]
    fn config_route_opens_panel_for_edit_forms() {
        assert_eq!(config_command_route(""), Ok(ConfigCommandRoute::Panel));
        assert_eq!(config_command_route("edit"), Ok(ConfigCommandRoute::Panel));
    }

    #[test]
    fn help_keys_is_a_native_keyboard_shortcut_surface() {
        assert_eq!(help_command_route(""), HelpCommandRoute::Commands);
        assert_eq!(help_command_route(" keys "), HelpCommandRoute::Keys);
        assert_eq!(
            help_command_route("commands"),
            HelpCommandRoute::Unsupported
        );

        let bindings = keyboard_shortcut_pairs();
        assert!(bindings.iter().any(|(key, _)| *key == "Ctrl+O"));
        assert!(bindings.iter().any(|(key, _)| *key == "Ctrl+G"));
        assert!(bindings.iter().any(|(key, _)| *key == "Ctrl+E"));
        assert!(bindings.iter().any(|(key, _)| *key == "Shift+Down"));
    }

    #[test]
    fn history_routes_only_to_the_canonical_transcript_workspace() {
        assert_eq!(history_command_route(""), HistoryCommandRoute::Transcript);
        assert_eq!(
            history_command_route(" grep old text "),
            HistoryCommandRoute::Unsupported
        );
    }

    #[test]
    fn model_picker_footer_warns_about_thinking_follow_up() {
        assert!(MODEL_PICKER_FOOTER_HINT.contains("thinking mode"));
        assert!(MODEL_THINKING_PICKER_FOOTER_HINT.contains("finish model selection"));
    }

    #[test]
    fn model_catalog_request_matches_only_picker_forms() {
        assert!(is_model_picker_request("/model"));
        assert!(is_model_picker_request("/model list"));
        assert!(!is_model_picker_request("/model info"));
        assert!(!is_model_picker_request("/model gpt-5"));
        assert!(!is_model_picker_request("/context"));
    }

    #[test]
    fn memory_route_keeps_discovery_actions_native_and_marks_the_rest_unsupported() {
        assert_eq!(memory_command_route(""), Ok(MemoryCommandRoute::List));
        assert_eq!(memory_command_route("list"), Ok(MemoryCommandRoute::List));
        assert_eq!(memory_command_route("ls"), Ok(MemoryCommandRoute::List));
        assert_eq!(
            memory_command_route("search auth preferences"),
            Ok(MemoryCommandRoute::Search("auth preferences".into()))
        );
        assert_eq!(
            memory_command_route("show mem_123"),
            Ok(MemoryCommandRoute::Unsupported)
        );
        assert_eq!(
            memory_command_route("inspect mem_123"),
            Ok(MemoryCommandRoute::Unsupported)
        );
        assert_eq!(
            memory_command_route("search"),
            Ok(MemoryCommandRoute::Unsupported)
        );
        assert_eq!(memory_command_route("stats"), Ok(MemoryCommandRoute::Stats));
        assert_eq!(
            memory_command_route("health"),
            Ok(MemoryCommandRoute::Health)
        );
        assert_eq!(
            memory_command_route("session"),
            Ok(MemoryCommandRoute::Session)
        );
        assert_eq!(
            memory_command_route("help"),
            Ok(MemoryCommandRoute::Unsupported)
        );
    }

    #[test]
    fn skill_route_only_advertises_the_native_browser() {
        assert_eq!(skill_command_route(""), SkillCommandRoute::Browse);
        assert_eq!(skill_command_route("browse"), SkillCommandRoute::Browse);
        assert_eq!(skill_command_route("list"), SkillCommandRoute::Browse);
        assert_eq!(
            skill_command_route("install demo"),
            SkillCommandRoute::Unsupported
        );
        assert_eq!(
            skill_command_route("info demo"),
            SkillCommandRoute::Unsupported
        );
    }

    #[test]
    fn context_usage_message_is_aligned() {
        assert_eq!(
            CONTEXT_USAGE_MESSAGE,
            "Usage: /context — open the context panel\n       /context dump [path] — write a JSON snapshot."
        );
    }

    #[test]
    fn context_dump_requires_an_exact_command_token() {
        assert_eq!(context_dump_argument("dump"), Some(""));
        assert_eq!(
            context_dump_argument("dump   snapshot.json"),
            Some("snapshot.json")
        );
        assert_eq!(context_dump_argument("dumpster"), None);
        assert_eq!(context_dump_argument("dumpster output.json"), None);
        assert_eq!(context_dump_argument(""), None);
    }

    #[test]
    fn context_panel_uses_committed_trace_when_observability_ring_is_empty() {
        let mut state = SessionState::default();
        state.latest_context_assembly_trace = Some(ContextAssemblyTrace {
            turn_id: "turn-7".into(),
            token_budget: astra_turn_core::context_assembly_trace::TokenBudgetTrace {
                max_tokens: 100_000,
                system_prompt_tokens: 5_000,
                history_tokens: 12_000,
                tool_schema_tokens: 3_000,
                user_message_tokens: 200,
                total_used: 20_200,
                ..Default::default()
            },
            ..Default::default()
        });
        let mut snapshot = ContextSnapshot::default();

        let breakdown = context_breakdown_for_panel(&state, &mut snapshot);

        assert_eq!(breakdown.limit, 100_000);
        assert_eq!(breakdown.total_used, 20_200);
        assert!(
            breakdown
                .categories
                .iter()
                .any(|category| category.tokens == 12_000)
        );
    }

    #[test]
    fn context_panel_keeps_session_and_history_visible_without_a_trace() {
        let mut state = SessionState::default();
        state.session_id = Some("session-context".into());
        state.turn = 3;
        state.model = Some("model-x".into());
        state.total_prompt_tokens = 1_200;
        state.total_completion_tokens = 600;
        let mut snapshot = ContextSnapshot::default();
        snapshot.session = Some(SessionSummary {
            session_id: "session-context".into(),
            turn: 3,
            model: Some("model-x".into()),
            total_cost: 0.01,
            max_budget: 1.0,
            prompt_tokens: 1_200,
            completion_tokens: 600,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            canonical_conversation: None,
            request_context: None,
            continuation_anchor: None,
            queued_message: None,
            diagnostics_context: None,
            read_activity: Default::default(),
        });
        snapshot.visible_conversation.push(VisibleConversationItem {
            role: "user".into(),
            preview: "first request".into(),
            body: "first request\nfull body".into(),
        });

        let breakdown = context_breakdown_for_panel(&state, &mut snapshot);

        assert_eq!(breakdown.limit, 0, "do not fabricate a prompt budget");
        assert!(breakdown.categories.is_empty());
        assert!(breakdown.session_summary.is_some());
        assert_eq!(breakdown.history.retained, 1);
        assert_eq!(breakdown.history.turns[0].preview, "first request");
        assert!(breakdown.has_observable_data());
    }

    #[test]
    fn context_environment_uses_the_latest_footer_observation() {
        let mut footer = crate::tui::bottom_pane::footer::Footer::new();
        footer.cwd = Some("~/work/astra".into());
        footer.git_branch = Some("feature/workbench".into());

        assert_eq!(
            super::context_environment_from_footer(&footer),
            (
                Some("~/work/astra".into()),
                Some("feature/workbench".into())
            )
        );
    }
}

#[cfg(test)]
mod context_history_tests {
    use super::{collect_visible_conversation, one_line_preview};
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
    fn visible_conversation_keeps_unpaired_rendered_cells_without_faking_turn_identity() {
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

        let visible = collect_visible_conversation(&chat);

        assert_eq!(visible.len(), 2);
        assert_eq!(visible[0].role, "assistant");
        assert_eq!(visible[1].role, "reasoning");
    }

    #[test]
    fn visible_conversation_keeps_real_roles_and_excludes_system_cells() {
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

        let visible = collect_visible_conversation(&chat);

        assert_eq!(visible.len(), 3);
        assert_eq!(visible[0].role, "user");
        assert_eq!(visible[0].body, "first user");
        assert_eq!(visible[1].role, "assistant");
        assert_eq!(visible[1].body, "assistant answer");
        assert_eq!(visible[2].role, "user");
        assert_eq!(visible[2].body, "second user");
        assert!(
            visible
                .iter()
                .all(|item| item.body != "system note should not appear in context history"),
            "system cells should not pollute visible conversation evidence"
        );
    }
}

#[cfg(test)]
mod view_result_tests {
    use super::handle_view_result;
    use crate::cli::permission_manager::PermissionMode;
    use crate::cli::session::session_state::SessionState;
    use crate::tui::bottom_pane::BottomPane;
    use crate::tui::bottom_pane::view::{BottomPaneView, SessionSelectionIntent, ViewResult};
    use crate::tui::chat_widget::ChatWidget;
    use crate::tui::history_cell::system::SystemCell;
    use astra_runtime::plan;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn last_system_message(widget: &ChatWidget) -> Option<String> {
        widget
            .history()
            .last()
            .and_then(|cell| cell.as_any_ref().downcast_ref::<SystemCell>())
            .map(|cell| cell.message().to_string())
    }

    #[test]
    fn permission_picker_returns_a_typed_selected_mode() {
        let mut picker = super::build_permission_mode_picker(PermissionMode::Prompt);
        picker.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        picker.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(
            picker.completion().and_then(|completion| completion.result),
            Some(ViewResult::Permission(PermissionMode::AcceptEdits))
        );
    }

    #[test]
    fn bypass_selection_requires_a_separate_typed_confirmation() {
        let mut state = SessionState::default();
        state.perm_manager.set_mode(PermissionMode::Prompt);
        let mut bottom_pane = BottomPane::new();
        let mut chat_widget = ChatWidget::new("");

        handle_view_result(
            ViewResult::Permission(PermissionMode::Bypass),
            &mut state,
            &mut bottom_pane,
            &mut chat_widget,
        );

        assert_eq!(state.perm_manager.mode(), PermissionMode::Prompt);
        assert!(bottom_pane.has_active_view());
    }

    #[test]
    fn bypass_confirmation_has_typed_keep_and_apply_outcomes() {
        let mut confirmation = super::build_permission_mode_confirmation(PermissionMode::Bypass);
        confirmation.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            confirmation
                .completion()
                .and_then(|completion| completion.result),
            Some(ViewResult::PermissionConfirmation {
                mode: PermissionMode::Bypass,
                confirmed: false,
            })
        );

        let mut confirmation = super::build_permission_mode_confirmation(PermissionMode::Bypass);
        confirmation.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        confirmation.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            confirmation
                .completion()
                .and_then(|completion| completion.result),
            Some(ViewResult::PermissionConfirmation {
                mode: PermissionMode::Bypass,
                confirmed: true,
            })
        );
    }

    #[test]
    fn confirmed_bypass_changes_policy_but_cancel_keeps_it_unchanged() {
        let mut state = SessionState::default();
        state.perm_manager.set_mode(PermissionMode::Prompt);
        let mut bottom_pane = BottomPane::new();
        let mut chat_widget = ChatWidget::new("");

        handle_view_result(
            ViewResult::PermissionConfirmation {
                mode: PermissionMode::Bypass,
                confirmed: false,
            },
            &mut state,
            &mut bottom_pane,
            &mut chat_widget,
        );
        assert_eq!(state.perm_manager.mode(), PermissionMode::Prompt);

        handle_view_result(
            ViewResult::PermissionConfirmation {
                mode: PermissionMode::Bypass,
                confirmed: true,
            },
            &mut state,
            &mut bottom_pane,
            &mut chat_widget,
        );
        assert_eq!(state.perm_manager.mode(), PermissionMode::Bypass);
    }

    #[test]
    fn session_picker_result_is_reserved_for_outer_resume_pipeline() {
        let mut state = SessionState::default();
        let mut bottom_pane = BottomPane::new();
        let mut chat_widget = ChatWidget::new("");

        handle_view_result(
            ViewResult::Session {
                session_id: "sess_1234567890".into(),
                intent: SessionSelectionIntent::Resume,
            },
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

        handle_view_result(
            ViewResult::Permission(PermissionMode::Auto),
            &mut state,
            &mut bottom_pane,
            &mut chat_widget,
        );

        assert_eq!(state.perm_manager.mode(), PermissionMode::Auto);
        assert_eq!(
            last_system_message(&chat_widget).as_deref(),
            Some("Mode → Auto")
        );
    }

    #[test]
    fn permission_selection_clears_stale_pending_local_plan_entry() {
        let mut state = SessionState::default();
        state.cloud_plan_mirror = Some(plan::PlanModeState::new(String::new()));
        state.perm_manager.set_mode(PermissionMode::Plan);
        let mut bottom_pane = BottomPane::new();
        let mut chat_widget = ChatWidget::new("");

        handle_view_result(
            ViewResult::Permission(PermissionMode::Auto),
            &mut state,
            &mut bottom_pane,
            &mut chat_widget,
        );

        assert_eq!(state.perm_manager.mode(), PermissionMode::Auto);
        assert!(
            state.cloud_plan_mirror.is_none(),
            "permission picker leaving Plan must clear a bare-/plan pending goal"
        );
    }

    #[test]
    fn accept_selection_updates_state_and_commits_feedback() {
        let mut state = SessionState::default();
        let mut bottom_pane = BottomPane::new();
        let mut chat_widget = ChatWidget::new("");

        handle_view_result(
            ViewResult::Permission(PermissionMode::AcceptEdits),
            &mut state,
            &mut bottom_pane,
            &mut chat_widget,
        );

        assert_eq!(state.perm_manager.mode(), PermissionMode::AcceptEdits);
        assert_eq!(
            last_system_message(&chat_widget).as_deref(),
            Some("Mode → Edits")
        );
    }

    #[test]
    fn read_only_policy_result_updates_state_and_commits_feedback() {
        let mut state = SessionState::default();
        let mut bottom_pane = BottomPane::new();
        let mut chat_widget = ChatWidget::new("");

        handle_view_result(
            ViewResult::Permission(PermissionMode::Plan),
            &mut state,
            &mut bottom_pane,
            &mut chat_widget,
        );

        assert_eq!(state.perm_manager.mode(), PermissionMode::Plan);
        assert_eq!(
            last_system_message(&chat_widget).as_deref(),
            Some("Mode → Read-only")
        );
    }

    #[test]
    fn default_selection_updates_state_and_commits_feedback() {
        let mut state = SessionState::default();
        state.perm_manager.set_mode(PermissionMode::Auto);
        let mut bottom_pane = BottomPane::new();
        let mut chat_widget = ChatWidget::new("");

        handle_view_result(
            ViewResult::Permission(PermissionMode::Prompt),
            &mut state,
            &mut bottom_pane,
            &mut chat_widget,
        );

        assert_eq!(state.perm_manager.mode(), PermissionMode::Prompt);
        assert_eq!(
            last_system_message(&chat_widget).as_deref(),
            Some("Mode → Ask")
        );
    }

    #[test]
    fn slash_command_selection_returns_command_to_composer() {
        let mut state = SessionState::default();
        let mut bottom_pane = BottomPane::new();
        let mut chat_widget = ChatWidget::new("");

        handle_view_result(
            ViewResult::InsertCommand("/resume".into()),
            &mut state,
            &mut bottom_pane,
            &mut chat_widget,
        );

        assert_eq!(bottom_pane.composer.text(), "/resume ");
        assert!(
            chat_widget.history().is_empty(),
            "selecting a command hint should not emit scrollback noise"
        );
    }

    #[test]
    fn memory_selection_opens_the_observed_record_without_mutating_model() {
        let mut state = SessionState::default();
        state.model = Some("deepseek-v4-pro".to_string());
        let mut bottom_pane = BottomPane::new();
        bottom_pane.footer.model = Some("deepseek-v4-pro".to_string());
        let mut chat_widget = ChatWidget::new("");

        handle_view_result(
            ViewResult::Memory(crate::tui::bottom_pane::view::MemorySelection {
                memory_id: "mem-1".into(),
                content: "remembered fact".into(),
            }),
            &mut state,
            &mut bottom_pane,
            &mut chat_widget,
        );

        assert_eq!(state.model.as_deref(), Some("deepseek-v4-pro"));
        assert_eq!(bottom_pane.footer.model.as_deref(), Some("deepseek-v4-pro"));
        assert!(chat_widget.history().is_empty());
        assert!(
            bottom_pane.has_active_view(),
            "memory selection must open detail"
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
mod immediate_control_tests {
    use super::{ImmediateControl, immediate_control};

    #[test]
    fn exit_is_phase_independent_even_with_whitespace_or_arguments() {
        for input in ["/exit", "  /exit  ", "/exit now"] {
            assert_eq!(immediate_control(input), Some(ImmediateControl::Exit));
        }
    }

    #[test]
    fn stop_is_a_typed_phase_independent_control() {
        for input in ["/stop", "  /stop  ", "/stop now"] {
            assert_eq!(
                immediate_control(input),
                Some(ImmediateControl::StopCurrentRun)
            );
        }
    }

    #[test]
    fn ordinary_slash_and_conversation_inputs_are_not_immediate_controls() {
        for input in [
            "/agent",
            "/help",
            "please /exit later",
            "停！",
            "stop the server",
            "",
        ] {
            assert_eq!(immediate_control(input), None, "input: {input}");
        }
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
        let _guard = JournalDirGuard::new(tmp.path());
        let owner_sessions_root = session_journal::local_owner_sessions_dir();
        std::fs::create_dir_all(owner_sessions_root.parent().unwrap())
            .expect("create owner layout parent");
        std::fs::write(&owner_sessions_root, "not-a-directory")
            .expect("write broken owner sessions root");

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
mod model_catalog_loading_tests {
    use super::load_model_catalog;
    use crate::cli::cli_config::cli_utils::{
        CredentialsFile, Profile, load_credentials, save_credentials,
    };
    use crate::test_utils::ProcessEnvGuard;
    use crate::tests::isolate_credentials;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn save_profile(access_token: &str, refresh_token: &str) {
        let mut credentials = CredentialsFile {
            current_profile: Some("default".into()),
            ..Default::default()
        };
        credentials.profiles.insert(
            "default".into(),
            Profile {
                account_id: Some("user-id-1".into()),
                access_token: Some(access_token.into()),
                refresh_token: Some(refresh_token.into()),
                ..Default::default()
            },
        );
        save_credentials(&credentials).expect("save isolated credentials");
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn model_catalog_unauthorized_response_refreshes_and_retries_with_new_token() {
        let _credentials = isolate_credentials();
        let _env = ProcessEnvGuard::remove("ASTRA_ACCESS_TOKEN");
        save_profile("stale-access", "refresh-old");

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .and(header("authorization", "Bearer stale-access"))
            .respond_with(ResponseTemplate::new(401).set_body_string("expired"))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/auth/refresh"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "user_id": "user-id-1",
                "access_token": "fresh-access",
                "refresh_token": "refresh-new"
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .and(header("authorization", "Bearer fresh-access"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!([{
                    "offering_id": "offer-gpt-5",
                    "access_id": "self-hosted",
                    "access_kind": "self_hosted",
                    "access_label": "Self-hosted",
                    "execution_placement": "server",
                    "name": "gpt-5",
                    "provider": "openai",
                    "description": null,
                    "is_active": true,
                    "context_window": 128000,
                    "max_completion_tokens": null,
                    "architecture": null,
                    "thinking_capability": "both"
                }])),
            )
            .expect(1)
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();

        let models = load_model_catalog(api, None)
            .await
            .expect("refreshed catalog");

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].offering_id, "offer-gpt-5");
        let credentials = load_credentials();
        let profile = credentials.profiles.get("default").unwrap();
        assert_eq!(profile.access_token.as_deref(), Some("fresh-access"));
        assert_eq!(profile.refresh_token.as_deref(), Some("refresh-new"));
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn model_catalog_invalid_payload_is_visible_without_auth_retry() {
        let _credentials = isolate_credentials();
        let _env = ProcessEnvGuard::remove("ASTRA_ACCESS_TOKEN");
        save_profile("valid-access", "refresh-unused");

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .and(header("authorization", "Bearer valid-access"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": []
            })))
            .expect(1)
            .mount(&server)
            .await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();

        load_model_catalog(api, None)
            .await
            .expect_err("invalid catalog shape must fail visibly");
        assert_eq!(
            load_credentials()
                .profiles
                .get("default")
                .and_then(|profile| profile.access_token.as_deref()),
            Some("valid-access")
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
mod session_hub_tests {
    use super::session_hub_persistence_error;
    use astra_services::session_workspace::WorkspaceMetadata;

    #[test]
    fn session_hub_persistence_error_prefers_live_state() {
        let mut ws = WorkspaceMetadata::new("sess-hub", "gpt-5");
        ws.last_persistence_error = Some("stale workspace error".into());

        assert_eq!(
            session_hub_persistence_error(Some("live commit failed"), Some(&ws)).as_deref(),
            Some(
                "degraded: live commit failed · live session can continue; resume/fork metadata may be stale until the next successful save"
            )
        );
    }

    #[test]
    fn session_hub_persistence_error_falls_back_to_workspace_state() {
        let mut ws = WorkspaceMetadata::new("sess-hub", "gpt-5");
        ws.last_persistence_error = Some("workspace write failed".into());

        assert_eq!(
            session_hub_persistence_error(None, Some(&ws)).as_deref(),
            Some(
                "degraded: workspace write failed · live session can continue; resume/fork metadata may be stale until the next successful save"
            )
        );
    }
}
