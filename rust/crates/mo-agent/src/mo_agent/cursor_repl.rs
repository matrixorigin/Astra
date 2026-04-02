//! Cursor-style REPL with fixed input box and status bar.
//!
//! This module provides an alternative REPL implementation that features:
//! - Fixed input box at bottom (always visible)
//! - Status bar showing model, tokens, session info
//! - Non-blocking input (can type while model is generating)
//! - Ctrl+C cancels running task
//!
//! Enable with `MO_CURSOR_UX=1` environment variable.

use super::*;
use event_loop::{EventLoopRunner, InputBuffer, StatusBar, StatusMode, TerminalLayout};
use std::io::{self, Write};
use std::time::Duration;

/// Check if Cursor-style UX is enabled via environment variable.
pub fn is_cursor_ux_enabled() -> bool {
    std::env::var("MO_CURSOR_UX")
        .map(|v| v == "1" || v.to_lowercase() == "true")
        .unwrap_or(false)
}

/// Cursor-style REPL state extending the base ReplState.
pub struct CursorReplState {
    /// Event loop runner for non-blocking input.
    pub runner: EventLoopRunner,
    /// History file path (for saving on exit).
    pub hist_path: std::path::PathBuf,
    /// Whether currently executing a turn.
    #[allow(dead_code)]
    pub turn_running: bool,
}

impl CursorReplState {
    pub fn new(hist_path: std::path::PathBuf) -> io::Result<Self> {
        let mut runner = EventLoopRunner::new()?;

        // Load history from file.
        runner.input.load_history(&hist_path);

        Ok(Self {
            runner,
            hist_path,
            turn_running: false,
        })
    }

    /// Update status bar from ReplState.
    pub fn sync_status(&mut self, state: &ReplState) {
        self.runner
            .set_model(state.model.as_deref().unwrap_or("auto"));
        self.runner
            .set_tokens(state.total_prompt_tokens, state.total_completion_tokens);
        self.runner.set_session(state.session_id.as_deref());

        // Set mode based on state.
        let mode = if state.plan_mode.is_some() {
            StatusMode::Plan
        } else if state.executing_plan.is_some() {
            StatusMode::Paused
        } else if state.chat_plan_only {
            StatusMode::PlanOnly
        } else {
            StatusMode::Normal
        };
        self.runner.set_mode(mode);
    }

    /// Get the appropriate prompt string.
    pub fn prompt_str(&self, state: &ReplState) -> String {
        if state.plan_mode.is_some() {
            format!("{} ", "plan>".yellow().bold())
        } else if state.executing_plan.is_some() {
            format!("{} ", "⏸>".yellow().bold())
        } else if state.chat_plan_only {
            format!("{} ", "plan·".yellow().bold())
        } else {
            format!("{} ", "❯".cyan().bold())
        }
    }

    /// Save history to file.
    pub fn save_history(&self) {
        if let Ok(content) = std::fs::read_to_string(&self.hist_path) {
            let mut lines: Vec<String> = content.lines().map(|s| s.to_string()).collect();
            // Append new history entries.
            for entry in &self.runner.input.history {
                if lines.last().map(|s| s.as_str()) != Some(entry.as_str()) {
                    lines.push(entry.clone());
                }
            }
            // Keep last 1000 entries.
            if lines.len() > 1000 {
                lines = lines[lines.len() - 1000..].to_vec();
            }
            let _ = std::fs::write(&self.hist_path, lines.join("\n"));
        } else {
            // Create new history file.
            let content = self.runner.input.history.join("\n");
            let _ = std::fs::write(&self.hist_path, content);
        }
    }
}

/// Output writer that respects the fixed layout.
///
/// All output is written above the status bar, and the status bar + input
/// are redrawn after each write.
pub struct LayoutAwareWriter<'a> {
    layout: &'a TerminalLayout,
    status: &'a StatusBar,
    prompt: &'a str,
    input: &'a InputBuffer,
}

impl<'a> LayoutAwareWriter<'a> {
    pub fn new(
        layout: &'a TerminalLayout,
        status: &'a StatusBar,
        prompt: &'a str,
        input: &'a InputBuffer,
    ) -> Self {
        Self {
            layout,
            status,
            prompt,
            input,
        }
    }

    /// Write a line to the output area (above status bar).
    pub fn writeln(&self, line: &str) -> io::Result<()> {
        // Move to status bar position.
        self.layout.move_to_status()?;

        // Move up one line and insert a new line.
        crossterm::execute!(
            io::stdout(),
            crossterm::cursor::MoveUp(1),
            crossterm::cursor::MoveToColumn(0)
        )?;

        // Print the line (this scrolls the output up).
        println!("{}", line);

        // Redraw status bar and input.
        self.layout.render_status(self.status)?;
        self.layout.render_input(self.prompt, self.input)?;

        Ok(())
    }

    /// Write multiple lines.
    #[allow(dead_code)]
    pub fn write_lines(&self, lines: &[&str]) -> io::Result<()> {
        for line in lines {
            self.writeln(line)?;
        }
        Ok(())
    }
}

/// Run the Cursor-style REPL.
///
/// This is an alternative to `run_chat_repl` that uses the event loop
/// for non-blocking input.
pub async fn run_cursor_style_repl(
    api: &mo_thin_client::ThinClient,
    profile: Option<&str>,
    initial_model: Option<&str>,
) -> Result<(), String> {
    // Same initialization as run_chat_repl.
    try_silent_auth(api, profile).await;

    let (_, hist_path) = build_repl_editor()?;
    let mut state = initialize_repl_state(profile, initial_model);

    // Initialize Cursor-style state.
    let mut cursor_state =
        CursorReplState::new(hist_path).map_err(|e| format!("Failed to init cursor UX: {e}"))?;

    // Session-scoped quality tracker and calibrator.
    let quality_tracker = std::sync::Arc::new(std::sync::Mutex::new(
        tool_registry::ToolQualityTracker::new(),
    ));
    let confidence_calibrator = std::sync::Arc::new(
        mo_agent_runtime::turn::routing_metrics::ConfidenceCalibrator::default(),
    );
    let (selector, pipeline_modules) = create_tool_selector_with_quality(
        api,
        profile,
        Some(quality_tracker),
        Some(confidence_calibrator),
    );

    // Load cross-session learning state.
    let profile_name = profile.unwrap_or("default");
    let (cross_session_health_entries, cloud_pull_result, pref_keys_after_pull) = {
        let loaded = mo_agent_runtime::pipeline::persistence::load_learning_state(
            profile_name,
            &pipeline_modules.entity_graph,
            &pipeline_modules.pattern_library,
            &pipeline_modules.calibrator,
        );
        if loaded {
            eprintln!("{}", "  ✓ Loaded learning state from prior sessions".dim());
        }
        let mut cross_session_health_entries =
            mo_agent_runtime::pipeline::persistence::load_tool_health(profile_name);
        state.synced_tool_health_entries =
            mo_agent_runtime::pipeline::persistence::load_synced_tool_health(profile_name);
        if !cross_session_health_entries.is_empty() {
            eprintln!(
                "{}",
                format!(
                    "  ✓ Restored tool health ({} tools tracked)",
                    cross_session_health_entries.len()
                )
                .dim()
            );
        }
        let cloud_pull_result = try_cloud_pull(
            profile_name,
            &pipeline_modules.entity_graph,
            &pipeline_modules.pattern_library,
            &pipeline_modules.calibrator,
        )
        .await;
        state.cloud_learning_version = cloud_pull_result.version;
        if !cloud_pull_result.tool_health.is_empty() {
            let (merged, cloud_wins, cloud_only) =
                mo_agent_runtime::pipeline::persistence::merge_tool_health(
                    &cross_session_health_entries,
                    &cloud_pull_result.tool_health,
                );
            cross_session_health_entries = merged;
            if cloud_wins > 0 || cloud_only > 0 {
                let mut parts = Vec::new();
                if cloud_wins > 0 {
                    parts.push(format!("{cloud_wins} updated from cloud"));
                }
                if cloud_only > 0 {
                    parts.push(format!("{cloud_only} new from cloud"));
                }
                eprintln!(
                    "{}",
                    format!("  ✓ Merged tool health: {}", parts.join(", ")).dim()
                );
            }
        }
        let pref_keys = try_cloud_pull_preferences(&mut state).await;
        (cross_session_health_entries, cloud_pull_result, pref_keys)
    };
    state.tool_health_entries = cross_session_health_entries.clone();
    if state.synced_tool_health_entries.is_empty() {
        state.synced_tool_health_entries = cross_session_health_entries;
    }

    // Matrix pool initialization.
    {
        let settings = mo_agent_runtime::matrix_settings_from_env();
        state.matrix_runtime = match mo_agent_core::SharedPool::new(&settings).await {
            Ok(pool) => {
                let user_id = std::env::var("MO_USER_ID").unwrap_or_else(|_| "local".to_string());
                let th =
                    std::sync::Arc::new(std::sync::Mutex::new(state.tool_health_entries.clone()));
                let lease = std::sync::Arc::new(mo_agent_services::TaskLeaseHoldCache::default());
                Some(std::sync::Arc::new(
                    mo_agent_runtime::MatrixCloudRuntime::attach(
                        pool,
                        profile.unwrap_or("default"),
                        &user_id,
                        pipeline_modules.entity_graph.clone(),
                        pipeline_modules.pattern_library.clone(),
                        pipeline_modules.calibrator.clone(),
                        th,
                        state.cloud_learning_version,
                        lease,
                    ),
                ))
            }
            Err(_) => None,
        };
    }

    state.pattern_library = Some(pipeline_modules.pattern_library.clone());
    state.entity_graph = Some(pipeline_modules.entity_graph.clone());
    state.calibrator = Some(pipeline_modules.calibrator.clone());
    state.skill_registry = pipeline_modules.skill_registry.clone();
    state.mcp_manager = pipeline_modules.mcp_manager.clone();

    append_cloud_pull_sync_journal(
        &state,
        profile_name,
        "repl_startup",
        &cloud_pull_result,
        &pref_keys_after_pull,
    );

    let profile_name_str = profile_name.to_string();

    // Pre-flight model check.
    if let Some(token) = current_access_token(profile) {
        let has_models = check_server_has_models(api, &token).await;
        if !has_models {
            state.model = Some("⚠ none".to_string());
        }
    }

    // Print banner (before entering raw mode).
    print_repl_banner(profile, &state);
    eprintln!("{}", "  ⬢ Cursor-style UX enabled (MO_CURSOR_UX=1)".cyan());
    eprintln!();

    let mut edge_heartbeat_task: Option<tokio::task::JoinHandle<()>> = None;
    if let Some(ref tok) = current_access_token(profile) {
        edge_heartbeat_task = register_and_start_heartbeat(api, tok).await;
    }

    if state.model.as_deref() == Some("⚠ none") {
        eprintln!(
            "  {}  {}",
            "⚠".yellow(),
            "No LLM model configured on server. Run: mo-admin model add".yellow()
        );
        eprintln!();
        state.model = None;
    }

    // Enter raw mode for event-driven input.
    EventLoopRunner::enter_raw_mode().map_err(|e| format!("Failed to enter raw mode: {e}"))?;

    // Main event loop.
    let result = run_cursor_event_loop(
        api,
        profile,
        &mut state,
        &mut cursor_state,
        &*selector,
        &pipeline_modules,
        &profile_name_str,
    )
    .await;

    // Exit raw mode.
    let _ = EventLoopRunner::exit_raw_mode();

    // Clear screen and show goodbye.
    print!("\x1b[2J\x1b[H");
    let _ = io::stdout().flush();

    // Save state (same as run_chat_repl).
    {
        let profile_name = profile.unwrap_or("default");
        if let Err(e) = mo_agent_runtime::pipeline::persistence::save_learning_state_with_health(
            profile_name,
            &pipeline_modules.entity_graph,
            &pipeline_modules.pattern_library,
            &pipeline_modules.calibrator,
            &state.tool_health_entries,
        ) {
            eprintln!(
                "{}",
                format!("  ⚠ Learning state not saved (will retry next session): {e}").yellow()
            );
        }

        const MAX_SYNC_RETRIES: u32 = 3;
        let mut expected_version = state.cloud_learning_version;
        for attempt in 0..MAX_SYNC_RETRIES {
            if try_cloud_push_versioned(
                profile_name,
                &pipeline_modules.entity_graph,
                &pipeline_modules.pattern_library,
                &pipeline_modules.calibrator,
                &state.tool_health_entries,
                expected_version,
            )
            .await
            .is_some()
            {
                break;
            }
            if attempt + 1 < MAX_SYNC_RETRIES {
                eprintln!("{}", "  ↻ Pulling fresh cloud state for merge...".dim());
                let pull_result = try_cloud_pull(
                    profile_name,
                    &pipeline_modules.entity_graph,
                    &pipeline_modules.pattern_library,
                    &pipeline_modules.calibrator,
                )
                .await;
                expected_version = pull_result.version;
                if !pull_result.tool_health.is_empty() {
                    let (merged, _, _) = mo_agent_runtime::pipeline::persistence::merge_tool_health(
                        &state.tool_health_entries,
                        &pull_result.tool_health,
                    );
                    state.tool_health_entries = merged;
                }
            }
        }
        try_cloud_push_preferences(&state).await;
    }

    if let Some(h) = edge_heartbeat_task.take() {
        h.abort();
    }

    cursor_state.save_history();

    eprintln!("{}", "Goodbye.".dim());

    result
}

/// The main event loop for Cursor-style REPL.
async fn run_cursor_event_loop(
    api: &mo_thin_client::ThinClient,
    profile: Option<&str>,
    state: &mut ReplState,
    cursor_state: &mut CursorReplState,
    selector: &dyn tool_selector::ToolSelector,
    pipeline_modules: &repl_runtime::PipelineModules,
    profile_name: &str,
) -> Result<(), String> {
    // Sync initial status.
    cursor_state.sync_status(state);

    // Initial render.
    let prompt = cursor_state.prompt_str(state);
    cursor_state.runner.set_prompt(&prompt);
    cursor_state
        .runner
        .render()
        .map_err(|e| format!("Render error: {e}"))?;

    loop {
        let current_token = current_access_token(profile);

        // Update prompt based on state.
        let prompt = cursor_state.prompt_str(state);
        cursor_state.runner.set_prompt(&prompt);

        // Show skill dev hint if active.
        if let Some(ref sname) = state.skill_dev_name {
            let writer = LayoutAwareWriter::new(
                &cursor_state.runner.layout,
                &cursor_state.runner.status,
                &prompt,
                &cursor_state.runner.input,
            );
            let _ = writer.writeln(&format!(
                "  \u{1f527} {}",
                format!("Skill dev: {sname}").cyan()
            ));
        }

        // Poll for input with a short timeout to remain responsive.
        let cmd = match cursor_state.runner.poll_command(Duration::from_millis(50)) {
            Ok(Some(cmd)) => cmd,
            Ok(None) => {
                // Check for cancellation.
                if cursor_state.runner.cancel_token.is_cancelled() {
                    cursor_state.runner.cancel_token.reset();
                    cursor_state.runner.set_busy(false);
                    cursor_state.sync_status(state);
                    cursor_state
                        .runner
                        .render()
                        .map_err(|e| format!("Render error: {e}"))?;
                }
                continue;
            }
            Err(e) => {
                // Input error — exit.
                return Err(format!("Input error: {e}"));
            }
        };

        // Skip empty input.
        if cmd.is_empty() {
            continue;
        }

        // Handle Ctrl+D (exit).
        if cmd == "/exit" && cursor_state.runner.input.text().is_empty() {
            // Session end.
            if let Some(ref j) = state.journal {
                let end_event = session_journal::JournalEvent::session_end(
                    state.session_id.as_deref(),
                    state.turn,
                );
                let _ = j.append(&end_event);
                repl_turn::enqueue_ingestion_pub(state, &end_event);
            }
            if let Some(mc) = state.matrix_runtime.as_ref() {
                mc.shutdown_ingestion();
            }
            if state.turn > 0
                && let Some(ref sid) = state.session_id
            {
                let short = if sid.len() > 8 { &sid[..8] } else { sid };
                eprintln!(
                    "{}",
                    format!("  Session {short}… saved. To resume: /resume {sid}").dim()
                );
            }
            break;
        }

        // Process multi-line input (backslash continuation).
        let cmd = cmd
            .lines()
            .map(|l| l.strip_suffix('\\').unwrap_or(l))
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string();

        // Add to history.
        cursor_state.runner.input.add_history(&cmd);

        // Handle slash commands.
        if cmd.starts_with('/') {
            let pending = take_slash_pending_execute();
            let dispatch_line = pending.as_deref().unwrap_or(&cmd);

            // Note: slash commands may use blocking I/O, so we temporarily
            // exit raw mode for some commands.
            let needs_normal_mode = matches!(
                dispatch_line.split_whitespace().next(),
                Some("/login") | Some("/register") | Some("/model") | Some("/debug")
            );

            if needs_normal_mode {
                let _ = EventLoopRunner::exit_raw_mode();
            }

            let should_exit = handle_slash_command(
                dispatch_line,
                api,
                profile,
                state,
                current_token.as_deref(),
                selector,
            )
            .await?;

            if needs_normal_mode {
                let _ = EventLoopRunner::enter_raw_mode();
            }

            if should_exit {
                break;
            }

            // Merge learning snapshot if /resume deposited one.
            if let Some(json) = state.learning_snapshot.take() {
                merge_learning_snapshot(
                    &json,
                    &pipeline_modules.entity_graph,
                    &pipeline_modules.pattern_library,
                    &pipeline_modules.calibrator,
                );
            }

            // Handle plan execution if triggered.
            if state.executing_plan.is_some() && state.plan_mode.is_none() {
                run_plan_execution(state, current_token.as_deref(), api, profile, selector).await?;
            }
        } else if state.plan_mode.is_some() {
            // Plan mode input.
            let _ = EventLoopRunner::exit_raw_mode();
            handle_plan_mode_input(cmd.clone(), current_token.as_deref(), state, api).await?;
            let _ = EventLoopRunner::enter_raw_mode();

            if state.executing_plan.is_some() {
                run_plan_execution(state, current_token.as_deref(), api, profile, selector).await?;
            }
        } else if state.executing_plan.is_some() && plan_decompose::is_resume_command(&cmd) {
            // Resume paused plan.
            eprintln!();
            eprintln!("{}  Resuming plan execution...", "▶".cyan());
            run_plan_execution(state, current_token.as_deref(), api, profile, selector).await?;
        } else {
            // Abandon paused plan if different message.
            if state.executing_plan.is_some() && !plan_decompose::is_resume_command(&cmd) {
                let plan = state.executing_plan.take().unwrap();
                let done = plan.items_done();
                let total = plan.subtasks.len();
                if done < total as u32 {
                    eprintln!(
                        "{}  Plan abandoned ({}/{} done). Processing as normal chat.",
                        "·".dim(),
                        done,
                        total
                    );
                }
            }

            // Auto plan detection.
            let mut should_proceed_normal = true;
            let line_for_plan = cmd.clone();
            if let Some(reason) = plan_decompose::should_suggest_plan_mode(&cmd) {
                eprintln!();
                eprintln!("{}  {}", "📋".yellow(), reason);
                eprintln!(
                    "{}  This task might benefit from planning. Enter plan mode? (y/n)",
                    "💡".cyan()
                );

                // Read response (temporarily exit raw mode).
                let _ = EventLoopRunner::exit_raw_mode();
                let mut response = String::new();
                if std::io::stdin().read_line(&mut response).is_ok() {
                    let resp = response.trim().to_lowercase();
                    if resp == "y" || resp == "yes" || resp == "是" {
                        let project_root = std::env::current_dir()
                            .unwrap_or_else(|_| std::path::PathBuf::from("."));
                        let context = plan_decompose::analyze_project(&project_root);
                        let goal_display = line_for_plan.clone();
                        let plan_state =
                            plan_decompose::PlanModeState::new(line_for_plan.clone(), context);

                        eprintln!();
                        eprintln!(
                            "{}  Entering plan mode for: {}",
                            "📋".green(),
                            goal_display.cyan()
                        );
                        eprintln!("{}  Generating plan...", "⋯".dim());

                        state.plan_mode = Some(plan_state);
                        should_proceed_normal = false;

                        handle_plan_mode_input(line_for_plan, current_token.as_deref(), state, api)
                            .await?;
                    } else {
                        eprintln!("{}  Proceeding with normal chat...", "→".dim());
                    }
                }
                let _ = EventLoopRunner::enter_raw_mode();
            }

            if should_proceed_normal {
                // Set busy state.
                cursor_state.runner.set_busy(true);
                cursor_state.sync_status(state);
                cursor_state
                    .runner
                    .render()
                    .map_err(|e| format!("Render error: {e}"))?;

                // Process chat input.
                // Note: This uses the existing SSE streaming which writes directly to stdout.
                // For full Cursor-style UX, we'd need to modify stream_render to use the layout.
                let _ = EventLoopRunner::exit_raw_mode();
                handle_chat_input(
                    cmd,
                    current_token.as_deref(),
                    state,
                    ReplTurnContext {
                        api,
                        profile,
                        selector,
                    },
                )
                .await?;
                let _ = EventLoopRunner::enter_raw_mode();

                cursor_state.runner.set_busy(false);
            }

            // Periodic learning sync.
            if state.matrix_runtime.is_some()
                && state.turn > 0
                && state
                    .turn
                    .is_multiple_of(mo_agent_services::session_checkpoint::CHECKPOINT_INTERVAL)
                && let Some(new_version) = try_cloud_push_delta(
                    profile_name,
                    &pipeline_modules.entity_graph,
                    &pipeline_modules.pattern_library,
                    &pipeline_modules.calibrator,
                    &state.tool_health_entries,
                    &mut state.synced_tool_health_entries,
                    state.cloud_learning_version,
                )
                .await
            {
                state.cloud_learning_version = Some(new_version);
                if let Some(ref mc) = state.matrix_runtime {
                    let orch = mc.sync_orchestrator_lock();
                    if let Some(mut env) = orch.envelope(mo_agent_services::SyncDomain::Learning) {
                        env.mark_synced(new_version as u64);
                        orch.update_envelope(mo_agent_services::SyncDomain::Learning, env);
                    }
                }
            }
        }

        // Sync status after each command.
        cursor_state.sync_status(state);
        cursor_state
            .runner
            .render()
            .map_err(|e| format!("Render error: {e}"))?;
    }

    Ok(())
}
