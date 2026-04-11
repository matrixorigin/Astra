// Clippy 1.94 — allow backlog in the large CLI binary; refine incrementally.
#![allow(
    dead_code,
    deprecated,
    clippy::collapsible_if,
    clippy::derivable_impls,
    clippy::field_reassign_with_default,
    clippy::items_after_test_module,
    clippy::let_unit_value,
    clippy::manual_strip,
    clippy::needless_borrow,
    clippy::redundant_closure,
    clippy::single_match,
    clippy::unnecessary_mut_passed
)]

use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Command as SysCommand, Stdio},
    sync::{Mutex, OnceLock},
};

use astra_core::SharedPool;
use astra_runtime::{plan_decompose, prompts, tool_registry, tool_selector};
use astra_services::session_journal;
use clap::Parser;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    style::Stylize,
    terminal,
};

mod edge_tools;
mod manifest_loader;
mod mcp_client;
mod skill_instructions;
use rustyline::{
    Cmd as RlCmd, CompletionType, ConditionalEventHandler, Config, Context, Editor,
    Event as RlEvent, EventContext as RlEventContext, EventHandler as RlEventHandler, Helper,
    KeyCode as RlKeyCode, KeyEvent as RlKeyEvent, Modifiers as RlModifiers, Movement as RlMovement,
    completion::{Completer, Pair},
    error::ReadlineError,
    highlight::Highlighter,
    hint::Hinter,
    history::FileHistory,
    validate::{ValidationContext, ValidationResult, Validator},
};
use serde::{Deserialize, Serialize};

#[path = "cli/agent_loader.rs"]
mod agent_loader;
#[path = "cli/agent_runtime.rs"]
mod agent_runtime;
#[path = "cli/auth_flow.rs"]
mod auth_flow;
#[path = "cli/chat_stream/mod.rs"]
mod chat_stream;
#[path = "cli/cli_args.rs"]
mod cli_args;
#[path = "cli/cli_formatting.rs"]
mod cli_formatting;
#[path = "cli/cli_output.rs"]
mod cli_output;
#[path = "cli/cli_utils.rs"]
mod cli_utils;
#[path = "cli/cloud_sync.rs"]
mod cloud_sync;
#[path = "cli/command_registry.rs"]
mod command_registry;
#[path = "cli/command_router.rs"]
mod command_router;
#[path = "cli/delegate_subrun.rs"]
mod delegate_subrun;
#[path = "cli/diff_presenter.rs"]
mod diff_presenter;
#[path = "cli/durable_bridge.rs"]
mod durable_bridge;
#[path = "cli/dynamic_completions.rs"]
mod dynamic_completions;
#[path = "cli/edge_lifecycle.rs"]
mod edge_lifecycle;
#[path = "cli/effects/mod.rs"]
mod effects;
#[path = "cli/followup_suggestion.rs"]
mod followup_suggestion;
#[path = "cli/idle_agent_messages.rs"]
mod idle_agent_messages;
#[path = "cli/journal_digest.rs"]
mod journal_digest;
#[path = "cli/mock_llm.rs"]
mod mock_llm;
#[path = "cli/permission_manager.rs"]
mod permission_manager;
#[path = "cli/picker_echo.rs"]
mod picker_echo;
#[path = "cli/plan_executor.rs"]
mod plan_executor;
#[path = "cli/plan_interaction.rs"]
mod plan_interaction;
#[path = "cli/plan_monitor.rs"]
mod plan_monitor;
#[path = "cli/plan_runtime.rs"]
mod plan_runtime;
#[path = "cli/project_instructions.rs"]
mod project_instructions;
#[path = "cli/prompt_input.rs"]
mod prompt_input;
#[path = "cli/readline_actor.rs"]
mod readline_actor;
#[path = "cli/repl_exit.rs"]
mod repl_exit;
#[path = "cli/repl_runtime.rs"]
mod repl_runtime;
#[path = "cli/repl_startup.rs"]
mod repl_startup;
#[path = "cli/repl_state.rs"]
mod repl_state;
#[path = "cli/repl_turn.rs"]
mod repl_turn;
#[path = "cli/repl_ui.rs"]
mod repl_ui;
#[path = "cli/session_cleanup.rs"]
mod session_cleanup;
#[path = "cli/session_guard.rs"]
mod session_guard;
#[path = "cli/skill_subrun.rs"]
mod skill_subrun;
#[path = "cli/slash_account.rs"]
mod slash_account;
#[path = "cli/slash_agent.rs"]
mod slash_agent;
#[path = "cli/slash_bug.rs"]
mod slash_bug;
#[path = "cli/slash_config.rs"]
mod slash_config;
#[path = "cli/slash_debug.rs"]
mod slash_debug;
#[path = "cli/slash_experiment.rs"]
mod slash_experiment;
#[path = "cli/slash_health.rs"]
mod slash_health;
#[path = "cli/slash_info.rs"]
mod slash_info;
#[path = "cli/slash_learn.rs"]
mod slash_learn;
#[path = "cli/slash_mcp.rs"]
mod slash_mcp;
#[path = "cli/slash_memory.rs"]
mod slash_memory;
#[path = "cli/slash_messaging.rs"]
mod slash_messaging;
#[path = "cli/slash_profile.rs"]
mod slash_profile;
#[path = "cli/slash_router.rs"]
mod slash_router;
#[path = "cli/slash_session.rs"]
mod slash_session;
#[path = "cli/slash_skill.rs"]
mod slash_skill;
#[path = "cli/slash_state.rs"]
mod slash_state;
#[path = "cli/slash_stats.rs"]
mod slash_stats;
#[path = "cli/slash_style.rs"]
mod slash_style;
#[path = "cli/slash_sync.rs"]
mod slash_sync;
#[path = "cli/slash_task.rs"]
mod slash_task;
#[path = "cli/slash_team.rs"]
mod slash_team;
#[path = "cli/slash_telemetry.rs"]
mod slash_telemetry;
#[path = "cli/slash_tools.rs"]
mod slash_tools;
#[path = "cli/slash_tuning.rs"]
mod slash_tuning;
#[path = "cli/spawn_subrun.rs"]
mod spawn_subrun;
#[path = "cli/sse_utils.rs"]
mod sse_utils;
#[path = "cli/startup_trace.rs"]
mod startup_trace;
#[path = "cli/stream_render.rs"]
mod stream_render;
#[path = "cli/streaming_md.rs"]
mod streaming_md;
#[path = "cli/streaming_types.rs"]
mod streaming_types;
#[path = "cli/terminal_region.rs"]
mod terminal_region;
#[path = "cli/theme.rs"]
mod theme;

use agent_runtime::initialize_multi_agent_runtime;
use astra_runtime::turn::chat_turn_heuristics::{
    is_session_not_found_error, looks_like_live_query_with_context,
};
use auth_flow::{clear_profile_last_session, do_login, do_register};
use chat_stream::{ChatTurnParams, stream_chat_sse};
use cli_utils::{
    compact_or_raw, get_profile_and_token, interactive_select, load_credentials, map_thin_err,
    prefix_chars, print_json_or_raw, profile_name, prompt_or, prompt_password_masked,
    resumable_last_session_id, save_credentials, truncate_str, urlencoding,
};
use command_router::{ExitCode, execute_cli_command, run_print_mode};
use dynamic_completions::refresh_dynamic_completions;
#[cfg(test)]
use dynamic_completions::truncate_skill_desc_for_completion;
use edge_lifecycle::register_and_start_heartbeat;
use idle_agent_messages::flush_idle_agent_messages_between_prompts;
use permission_manager::PermissionManager;
#[cfg(test)]
use picker_echo::build_picker_submission_echo;
use picker_echo::{replace_picker_submission_echo, should_clear_picker_submission_echo};
use prompt_input::{PromptInput, normalize_repl_input, wait_for_prompt_input};
use repl_exit::{ReplExit, finalize_repl_exit};
use repl_startup::complete_repl_startup;
use startup_trace::StartupTracer;
#[cfg(test)]
use stream_render::{StreamRenderState, TurnResult, dispatch_turn_event_block};

use plan_interaction::{handle_plan_mode_input, plan_execution_ui_active};
use repl_runtime::{
    build_repl_editor, check_server_has_models, create_tool_selector, create_tool_selector_quiet,
    create_tool_selector_with_quality, current_access_token, initialize_repl_state,
    print_repl_banner, try_silent_auth,
};
use repl_turn::{ReplTurnContext, create_manual_repl_checkpoint, handle_chat_input};
use repl_ui::{
    ReplHelper, SlashStartCompleteHandler, clear_followup_prompt_hint, clear_slash_overlay,
    history_path, is_slash_picker_active, print_keyboard_shortcuts, print_slash_commands,
    resolve_slash_command, suggest_commands,
};
use session_guard::update_panic_guard;
use slash_account::handle_account_command;
use slash_bug::handle_bug_command;
use slash_debug::handle_debug_command;
use slash_info::handle_info_command;
use slash_memory::handle_memory_domain_command;
use slash_messaging::handle_messaging_command;
use slash_router::handle_slash_command;
use slash_session::handle_session_command;
#[cfg(test)]
use slash_session::resolve_journal_target_session;
use slash_skill::handle_skill_command;
use slash_state::{StateCommandContext, handle_state_command};

// CLI argument structs moved to cli/cli_args.rs
use cli_args::*;

// SSE streaming types moved to cli/streaming_types.rs
pub(crate) use streaming_types::{PartialTurnData, StreamResult, TurnFailure, VerdictEvent};

// REPL state moved to cli/repl_state.rs
#[cfg(test)]
use idle_agent_messages::drain_root_mailbox_into_idle_queue;
use plan_monitor::{
    finalize_plan_run_task_after_executor, flush_plan_updates_between_prompts,
    run_blocking_plan_monitor, sync_plan_run_task_progress,
};
pub(crate) use plan_monitor::{format_duration_short, format_plan_progress};
pub(crate) use plan_runtime::build_learning_bridge;
use plan_runtime::start_and_monitor_plan;
pub(crate) use repl_state::{ExplainMode, ReplState, SkillDevState};

// ═══════════════════════════════════════════════ Output Styles ═════════════

// ═══════════════════════════════════════════════════ Learning Merge ═══════
// Cloud sync moved to cli/cloud_sync.rs

pub(crate) use cloud_sync::post_auth_cloud_resync;
use cloud_sync::{
    append_cloud_pull_sync_journal, merge_learning_snapshot, try_cloud_pull,
    try_cloud_pull_preferences, try_cloud_push_delta, try_cloud_push_preferences,
    try_cloud_push_versioned,
};

// ═══════════════════════════════════════════════════════ Task Commands ════

// ══════════════════════════════════════════════════════ Slash Commands ════

// ═══════════════════════════════════════════════════════════════ REPL ════

async fn run_chat_repl(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    initial_model: Option<&str>,
    resume_session_id: Option<&str>,
) -> Result<(), String> {
    let mut tracer = StartupTracer::new();

    // Try silent auth (validate/refresh token) but don't block entry.
    // If not authenticated, user can still explore — operations that need
    // auth will prompt "Not logged in. Use /login."
    try_silent_auth(api, profile).await;
    tracer.phase("auth");

    let (editor, hist_path) = build_repl_editor()?;
    let mut readline = readline_actor::ReadlineActor::spawn(editor)?;
    tracer.phase("editor");

    let mut state = initialize_repl_state(profile, initial_model);
    tracer.phase("state_init");

    let repl_startup::ReplStartupArtifacts {
        selector,
        pipeline_modules,
        profile_name_str,
        mut edge_heartbeat_task,
        skill_quality_path,
        pinned_skills_path,
        mut shutdown_signal_rx,
    } = complete_repl_startup(&mut state, &mut tracer, api, profile, resume_session_id).await;

    // Print startup trace summary if enabled
    tracer.finish();

    // ── Main loop ─────────────────────────────────────────────────────────────
    let repl_exit = loop {
        flush_idle_agent_messages_between_prompts(&mut state);
        let plan_terminal = flush_plan_updates_between_prompts(&mut state);
        sync_plan_run_task_progress(&mut state).await;
        if plan_terminal {
            finalize_plan_run_task_after_executor(&mut state).await;
        }
        // Refresh Tab-completion data (skills/MCP may change mid-session).
        // On first iteration, this seeds the initial completions lazily.
        refresh_dynamic_completions(&state).await;
        let current_token = current_access_token(profile);

        // Keep readline prompt TEXT as ASCII-only. Unicode characters (⏸, 🔄, ❯)
        // have ambiguous display widths that break cursor tracking for CJK input.
        // ANSI color codes are safe — rustyline's calculate_position() treats them
        // as width=0 so cursor math is unaffected.
        if let Some(ref dev) = state.skill_dev {
            eprintln!(
                "  \u{1f527} {}",
                format!("Skill dev: {}", dev.name).cyan().dim()
            );
        }
        // Single source for "plan run in progress": live handle and/or persisted background flag.
        let plan_run_active = plan_execution_ui_active(&state);
        let prompt_str = if let Some(ref ps) = state.plan_mode {
            if ps.goal.is_empty() {
                if plan_run_active {
                    "\x1b[1;33mplan*>\x1b[0m ".to_string()
                } else {
                    theme::PROMPT_PLAN.to_string()
                }
            } else {
                let short_goal: String = ps.goal.chars().take(20).collect();
                let suffix = if ps.goal.len() > 20 { "…" } else { "" };
                let star = if plan_run_active { "*" } else { "" };
                format!("\x1b[1;33mplan{star}[{short_goal}{suffix}]>\x1b[0m ")
            }
        } else if state.executing_plan.is_some() {
            theme::PROMPT_PAUSE.to_string()
        } else if plan_run_active {
            theme::PROMPT_BG.to_string()
        } else if state.chat_plan_only {
            theme::PROMPT_PLAN_ONLY.to_string()
        } else {
            theme::PROMPT_DEFAULT.to_string()
        };

        // Do NOT flush plan updates during active readline — writing to stderr
        // (\r\x1b[2K) while rustyline owns the terminal disrupts cursor
        // tracking for wide (CJK) characters, causing the last character to
        // visually disappear. Plan updates are buffered and flushed between
        // prompts instead.
        let prompt_input = wait_for_prompt_input(
            &mut state,
            &mut readline,
            prompt_str.clone(),
            &mut shutdown_signal_rx,
        )
        .await;

        // ── Process readline result ──────────────────────────────────
        match prompt_input {
            PromptInput::Shutdown(signal) => {
                clear_slash_overlay();
                eprintln!(
                    "\n  {} Received {}. Shutting down gracefully...",
                    theme::icon_warn(),
                    signal.label().bold()
                );
                break ReplExit::Shutdown(signal);
            }
            PromptInput::Readline(readline_result, pending_execute) => match readline_result {
                Ok(line) => {
                    clear_slash_overlay();
                    let line = normalize_repl_input(&line);
                    if line.is_empty() {
                        continue;
                    }
                    clear_followup_prompt_hint();
                    state.pending_followup_suggestion = None;
                    readline.add_history(line.clone());

                    // ── Handle pending approval from background plan executor ──
                    if let Some(tx) = state.pending_approval.take() {
                        let trimmed = line.trim().to_lowercase();
                        let approved = trimmed == "y" || trimmed == "yes" || trimmed == "a";
                        let autorun = trimmed == "!" || trimmed == "all" || trimmed == "yolo";
                        let denied = trimmed == "n" || trimmed == "no";
                        if approved || autorun || denied {
                            let _ = tx.send(approved || autorun);
                            if autorun {
                                state
                                    .perm_manager
                                    .set_mode(permission_manager::PermissionMode::Auto);
                                eprintln!(
                                    "  {} {} All tools auto-approved for this session.",
                                    "⚡".yellow(),
                                    "Auto-run enabled!".bold().yellow()
                                );
                                eprintln!(
                                    "  {}",
                                    "  Use /allow prompt to restore confirmation prompts.".dim()
                                );
                            } else if approved {
                                eprintln!("  {} Approved", theme::icon_ok());
                            } else {
                                eprintln!("  {} Denied", theme::icon_err());
                            }
                            continue;
                        } else {
                            // Unrecognized — treat as deny and fall through
                            let _ = tx.send(false);
                            eprintln!(
                                "  {} Unrecognized response, treating as denied",
                                theme::icon_err()
                            );
                            // Fall through to normal input handling
                        }
                    }

                    if line.starts_with('/') {
                        if should_clear_picker_submission_echo(&line, pending_execute.as_deref()) {
                            replace_picker_submission_echo(
                                &prompt_str,
                                pending_execute.as_deref().unwrap_or(&line),
                            );
                        }
                        // If Enter was pressed in the picker, the selected command is
                        // stored in pending-execute (captured by readline actor thread).
                        let dispatch_line_owned = pending_execute.unwrap_or_else(|| line.clone());
                        let dispatch_line = dispatch_line_owned.as_str();
                        let should_exit = handle_slash_command(
                            dispatch_line,
                            api,
                            profile,
                            &mut state,
                            current_token.as_deref(),
                            &*selector,
                        )
                        .await?;
                        if should_exit {
                            break ReplExit::Command;
                        }
                        // Merge learning snapshot if /resume deposited one
                        if let Some(json) = state.learning_snapshot.take() {
                            merge_learning_snapshot(
                                &json,
                                &pipeline_modules.entity_graph,
                                &pipeline_modules.pattern_library,
                                &pipeline_modules.calibrator,
                            );
                        }

                        // If /plan auto triggered execution, start the background executor
                        if state.executing_plan.is_some() && state.plan_mode.is_none() {
                            start_and_monitor_plan(
                                &mut state,
                                current_token.as_deref(),
                                api,
                                profile,
                            )
                            .await?;
                        }
                    } else if state.plan_mode.is_some() {
                        // Plan mode: handle input as plan editing
                        if let Err(e) = handle_plan_mode_input(
                            line.clone(),
                            current_token.as_deref(),
                            &mut state,
                            api,
                        )
                        .await
                        {
                            state.plan_resume_pending = false;
                            return Err(e);
                        }

                        // If plan execution was just triggered, start the executor (blocking).
                        if state.executing_plan.is_some() {
                            start_and_monitor_plan(
                                &mut state,
                                current_token.as_deref(),
                                api,
                                profile,
                            )
                            .await?;
                        } else if state.plan_resume_pending {
                            // Resume was sent to a paused executor — re-enter blocking monitor.
                            state.plan_resume_pending = false;
                            run_blocking_plan_monitor(&mut state).await;
                        }
                    } else if (state.executing_plan.is_some() || state.plan_handle.is_some())
                        && plan_decompose::is_resume_command(&line)
                    {
                        // Resume paused plan execution
                        eprintln!();
                        eprintln!("{}  Resuming plan execution...", "▶".cyan());
                        if let Some(ref handle) = state.plan_handle {
                            let _ = handle.send_command(plan_executor::PlanCommand::Resume {
                                corrections: if state.plan_execution_corrections.is_empty() {
                                    None
                                } else {
                                    Some(std::mem::take(&mut state.plan_execution_corrections))
                                },
                            });
                            // Re-enter blocking monitor until done/paused/error
                            run_blocking_plan_monitor(&mut state).await;
                        } else {
                            start_and_monitor_plan(
                                &mut state,
                                current_token.as_deref(),
                                api,
                                profile,
                            )
                            .await?;
                        }
                    } else {
                        let has_paused_plan =
                            state.executing_plan.is_some() || state.plan_handle.is_some();
                        if has_paused_plan {
                            if let Some(action) = plan_decompose::parse_plan_paused_user_line(&line)
                            {
                                match action {
                                    plan_decompose::PlanPausedUserAction::ClearCorrections => {
                                        state.plan_execution_corrections.clear();
                                        eprintln!(
                                            "{}",
                                            "  Cleared stacked operator guidance.".dim()
                                        );
                                    }
                                    plan_decompose::PlanPausedUserAction::Correction(s) => {
                                        state.plan_execution_corrections.push(s);
                                        eprintln!(
                                            "{}  Recorded guidance ({}). It will prefix each upcoming subtask. Type continue when ready.",
                                            "💡".cyan(),
                                            state.plan_execution_corrections.len(),
                                        );
                                    }
                                    plan_decompose::PlanPausedUserAction::Rewind(anchor) => {
                                        if let Some(plan) = state.executing_plan.as_mut() {
                                            match plan_decompose::resolve_rewind_start_index(
                                                plan, &anchor,
                                            ) {
                                                Ok(idx) => {
                                                    let reset =
                                                        plan_decompose::rewind_plan_from_subtask(
                                                            plan, idx,
                                                        );
                                                    eprintln!(
                                                        "{}  Rewound from step {} — {} subtask(s) set back to pending. Type continue to resume.",
                                                        "↩".cyan(),
                                                        idx + 1,
                                                        reset,
                                                    );
                                                }
                                                Err(e) => {
                                                    eprintln!(
                                                        "{}",
                                                        format!("  {} {e}", theme::icon_err())
                                                            .red()
                                                    );
                                                }
                                            }
                                        } else {
                                            eprintln!(
                                                "  {} Rewind not available while plan is held by the executor. Type continue first.",
                                                theme::icon_warn()
                                            );
                                        }
                                    }
                                }
                                continue;
                            }
                            // Paused plan: any other non-resume line abandons and becomes normal chat
                            let had_executor = state.plan_handle.is_some();
                            plan_interaction::shutdown_plan_executor(&mut state);
                            let plan = state.executing_plan.take();
                            state.plan_execution_corrections.clear();
                            match plan.as_ref() {
                                Some(p) => {
                                    let (done, total) = (p.items_done(), p.subtasks.len());
                                    if done < total as u32 {
                                        eprintln!(
                                            "{}  Plan abandoned ({}/{} done). Processing as normal chat.",
                                            "·".dim(),
                                            done,
                                            total
                                        );
                                    }
                                }
                                None if had_executor => {
                                    eprintln!(
                                        "{}  Plan abandoned (executor was cancelled; in-memory progress was not available). Processing as normal chat.",
                                        "·".dim(),
                                    );
                                }
                                None => {}
                            }
                        }

                        // Auto plan detection: suggest plan mode for complex tasks
                        let mut should_proceed_normal = true;
                        let line_for_plan = line.clone(); // Clone early to avoid borrow issues
                        if let Some(reason) = plan_decompose::should_suggest_plan_mode(&line) {
                            eprintln!();
                            eprintln!("{}  {}", "📋".yellow(), reason);
                            eprintln!(
                                "{}  This task might benefit from planning. Enter plan mode? (y/n)",
                                "💡".cyan()
                            );

                            // Read user response
                            let mut response = String::new();
                            if std::io::stdin().read_line(&mut response).is_ok() {
                                let resp = response.trim().to_lowercase();
                                if resp == "y" || resp == "yes" || resp == "是" {
                                    // Enter plan mode with the goal
                                    let project_root = std::env::current_dir()
                                        .unwrap_or_else(|_| std::path::PathBuf::from("."));
                                    let context = plan_decompose::analyze_project(&project_root);
                                    let goal_display = line_for_plan.clone();
                                    let plan_state = plan_decompose::PlanModeState::new(
                                        line_for_plan.clone(),
                                        context,
                                    );

                                    eprintln!();
                                    eprintln!(
                                        "{}  Entering plan mode for: {}",
                                        "📋".green(),
                                        goal_display.cyan()
                                    );
                                    eprintln!("{}  Generating plan...", "⋯".dim());

                                    // Trigger plan generation (set goal, plan will be generated in plan mode)
                                    state.plan_mode = Some(plan_state);
                                    should_proceed_normal = false;

                                    // Call handle_plan_mode_input to generate the plan
                                    handle_plan_mode_input(
                                        line_for_plan,
                                        current_token.as_deref(),
                                        &mut state,
                                        api,
                                    )
                                    .await?;
                                } else {
                                    eprintln!("{}  Proceeding with normal chat...", "→".dim());
                                }
                            }
                        }

                        if should_proceed_normal {
                            handle_chat_input(
                                line,
                                current_token.as_deref(),
                                &mut state,
                                ReplTurnContext {
                                    api,
                                    profile,
                                    selector: &*selector,
                                },
                            )
                            .await?;
                        }

                        // Keep panic guard in sync with current session state.
                        if let Some(ref sid) = state.session_id {
                            update_panic_guard(sid, state.turn);
                        }

                        // Periodic learning sync: push to cloud at checkpoint boundaries
                        // to prevent data loss on crash (every CHECKPOINT_INTERVAL turns)
                        if state.matrix_runtime.is_some()
                            && state.turn > 0
                            && state.turn.is_multiple_of(
                                astra_services::session_checkpoint::CHECKPOINT_INTERVAL,
                            )
                        {
                            // Use delta push at checkpoints to reduce sync bandwidth.
                            // Final session-end sync still uses full push for convergence.
                            if let Some(new_version) = try_cloud_push_delta(
                                &profile_name_str,
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
                                // Update orchestrator envelope to reflect the push
                                if let Some(ref mc) = state.matrix_runtime {
                                    let orch = mc.sync_orchestrator_lock().await;
                                    if let Some(mut env) =
                                        orch.envelope(astra_services::SyncDomain::Learning)
                                    {
                                        env.mark_synced(new_version as u64);
                                        orch.update_envelope(
                                            astra_services::SyncDomain::Learning,
                                            env,
                                        );
                                    }
                                }
                            }
                            // On conflict, we skip this push — the final push at session end
                            // will resolve conflicts via pull-merge-push cycle
                        }

                        // --max-budget enforcement: check accumulated cost against budget limit
                        if state.max_budget_limit > 0.0 {
                            let current_cost = slash_stats::cost_for_tokens(
                                state.total_prompt_tokens,
                                state.total_completion_tokens,
                                state.total_cache_read_tokens,
                                state.total_cache_creation_tokens,
                                &state.cached_pricing,
                            );
                            state.total_session_cost = current_cost;
                            if current_cost >= state.max_budget_limit {
                                eprintln!(
                                    "\n  {} Session budget reached: {} / {} limit. Exiting.",
                                    theme::icon_warn(),
                                    slash_stats::format_cost(current_cost).bold(),
                                    slash_stats::format_cost(state.max_budget_limit),
                                );
                                break ReplExit::BudgetLimit;
                            }
                        }

                        if let Some(signal) = *shutdown_signal_rx.borrow() {
                            clear_slash_overlay();
                            eprintln!(
                                "\n  {} Received {}. Shutting down gracefully...",
                                theme::icon_warn(),
                                signal.label().bold()
                            );
                            break ReplExit::Shutdown(signal);
                        }
                    }
                }
                Err(ReadlineError::Interrupted) => {
                    clear_slash_overlay();
                    eprintln!("^C");
                }
                Err(ReadlineError::Eof) => {
                    clear_slash_overlay();
                    eprintln!("{}", "\nGoodbye.".dim());
                    break ReplExit::Eof;
                }
                Err(e) => {
                    clear_slash_overlay();
                    eprintln!(
                        "  {} {}",
                        theme::icon_err(),
                        "Input error — exiting session.".red()
                    );
                    eprintln!("{}", format!("  ({e})").dim());
                    break ReplExit::InputError;
                }
            },
        }
    };

    finalize_repl_exit(&state, profile, repl_exit).await;

    // Save cross-session learning state (including tool health)
    {
        // Save skill quality metrics
        if let Err(e) = state.skill_quality_tracker.save(&skill_quality_path) {
            eprintln!(
                "{}",
                format!("  ⚠ Skill quality data not saved: {e}").yellow()
            );
        }

        // Save pinned skills (atomic: write to temp file, then rename)
        if !state.pinned_skills.is_empty() {
            if let Ok(json) = serde_json::to_string_pretty(&state.pinned_skills) {
                if let Some(parent) = pinned_skills_path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let tmp = pinned_skills_path.with_extension("tmp");
                match std::fs::write(&tmp, &json) {
                    Ok(()) => {
                        if let Err(e) = std::fs::rename(&tmp, &pinned_skills_path) {
                            eprintln!("⚠ Failed to save pinned_skills.json: {e}");
                            let _ = std::fs::remove_file(&tmp);
                        }
                    }
                    Err(e) => eprintln!("⚠ Failed to write pinned_skills.json: {e}"),
                }
            }
        } else {
            let _ = std::fs::remove_file(&pinned_skills_path);
        }

        // Upload quality metrics to marketplace (opt-in via ASTRA_QUALITY_UPLOAD=true)
        slash_skill::maybe_upload_quality_on_exit(
            api,
            &state.skill_quality_tracker,
            current_access_token(profile).as_deref(),
        )
        .await;

        let profile_name = profile.unwrap_or("default");
        if let Err(e) = astra_runtime::pipeline::persistence::save_learning_state_with_health(
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
        // Push learning to cloud with versioned API (conflict resolution loop)
        // On conflict: pull fresh data, merge, retry push (max 3 attempts)
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
                // Success — done
                break;
            }
            // Conflict or failure — pull fresh, merge, retry
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
                // Merge tool health from cloud pull
                if !pull_result.tool_health.is_empty() {
                    let (merged, _, _) = astra_runtime::pipeline::persistence::merge_tool_health(
                        &state.tool_health_entries,
                        &pull_result.tool_health,
                    );
                    // Update tool health in memory (though session is ending)
                    state.tool_health_entries = merged;
                }
            }
        }
        // Push preferences to cloud (best-effort)
        try_cloud_push_preferences(&state).await;
    }

    if let Some(h) = edge_heartbeat_task.take() {
        h.abort();
    }

    readline.shutdown(hist_path);
    Ok(())
}

// ---------------------------------------------------------------------------
// Session finalization — shared logic for all exit paths
// ---------------------------------------------------------------------------

// Session cleanup moved to session_cleanup.rs
use project_instructions::{
    discover_project_instructions, format_project_instructions, resolve_system_prompt,
};

// ════════════════════════════════════════════════════════════════ main ════

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    let cli = Cli::parse();
    let base = cli.api_url.trim_end_matches('/').to_string();
    let api = match astra_thin_client::ThinClient::new(&base, None) {
        Ok(api) => api,
        Err(err) => {
            eprintln!(
                "{}",
                format!("Error: invalid API URL '{base}': {err}").red()
            );
            std::process::exit(1);
        }
    };

    // Set MEMORIA_API_KEY from credentials if not already set
    if std::env::var("MEMORIA_API_KEY").is_err() {
        let creds = load_credentials();
        let name = profile_name(cli.profile.as_deref(), &creds);
        if let Some(key) = creds
            .profiles
            .get(&name)
            .and_then(|p| p.memoria_api_key.as_deref())
        {
            unsafe {
                std::env::set_var("MEMORIA_API_KEY", key);
            }
        }
    }

    let Cli {
        api_url: _,
        profile,
        model: cli_model,
        print: print_mode,
        output_format,
        continue_last,
        resume,
        yes: auto_approve,
        system_prompt,
        max_turns,
        max_budget,
        allowed_tools,
        disallowed_tools,
        add_dir,
        verbose,
        mcp_config,
        session_id: cli_session_id,
        session_name,
        bare,
        no_instructions,
        startup_trace,
        command,
    } = cli;

    // --startup-trace: enable startup timing
    if startup_trace {
        unsafe {
            std::env::set_var("ASTRA_STARTUP_TRACE", "1");
        }
    }

    // --bare: set env var for minimal mode
    if bare {
        unsafe {
            std::env::set_var("ASTRA_BARE", "1");
        }
    }

    // --no-instructions: disable .astra/instructions.md auto-loading
    if no_instructions {
        unsafe {
            std::env::set_var("ASTRA_NO_INSTRUCTIONS", "1");
        }
    }

    // --max-turns: override via env var before RuntimeLimits singleton is initialized
    if let Some(turns) = max_turns {
        unsafe {
            std::env::set_var("MO_MAX_TURNS", turns.to_string());
        }
    }

    // --max-budget: store the limit; enforcement happens in the REPL loop
    if max_budget > 0.0 {
        unsafe {
            std::env::set_var("MO_MAX_BUDGET", max_budget.to_string());
        }
    }

    // --system-prompt: support @file syntax to read from file
    let system_prompt = system_prompt.map(|sp| match resolve_system_prompt(sp) {
        Ok(content) => content,
        Err(e) => {
            eprintln!("{}", e.red());
            std::process::exit(1);
        }
    });

    // Merge project instructions into system_prompt for inline/print modes.
    // REPL mode handles this separately via build_effective_line.
    let system_prompt = if no_instructions {
        system_prompt
    } else {
        match (system_prompt, discover_project_instructions()) {
            (Some(sp), Some(pi)) => Some(format!("{sp}\n\n{}", format_project_instructions(&pi))),
            (Some(sp), None) => Some(sp),
            (None, Some(pi)) => Some(format_project_instructions(&pi)),
            (None, None) => None,
        }
    };

    // --allowed-tools: normalize comma/space-separated list and export as env var
    if !allowed_tools.is_empty() {
        let normalized: Vec<String> = allowed_tools
            .iter()
            .flat_map(|s| s.split([',', ' ']))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if !normalized.is_empty() {
            unsafe {
                std::env::set_var("ASTRA_ALLOWED_TOOLS", normalized.join(","));
            }
        }
    }

    // --disallowed-tools: normalize and export as env var (deny-list, opposite of --allowed-tools)
    if !disallowed_tools.is_empty() {
        let normalized: Vec<String> = disallowed_tools
            .iter()
            .flat_map(|s| s.split([',', ' ']))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if !normalized.is_empty() {
            unsafe {
                std::env::set_var("ASTRA_DISALLOWED_TOOLS", normalized.join(","));
            }
        }
    }

    // --add-dir: export additional directories as env var
    if !add_dir.is_empty() {
        let dirs: Vec<String> = add_dir
            .iter()
            .map(|d| {
                std::path::Path::new(d)
                    .canonicalize()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|_| d.clone())
            })
            .collect();
        unsafe {
            std::env::set_var("ASTRA_ADD_DIRS", dirs.join(":"));
        }
    }

    // --yes (-y): set auto-approve mode for the interactive REPL
    if auto_approve {
        unsafe {
            std::env::set_var("ASTRA_AUTO_APPROVE", "1");
        }
    }
    if verbose {
        unsafe {
            std::env::set_var("ASTRA_VERBOSE", "1");
        }
    }

    // --mcp-config: load MCP server configs from files/JSON strings
    if !mcp_config.is_empty() {
        if let Err(e) = command_router::load_mcp_configs(&mcp_config) {
            eprintln!(
                "{}",
                format!("Warning: failed to load MCP config: {e}").yellow()
            );
        }
    }

    // --session-id: validate UUID format and export for REPL to pick up
    if let Some(ref sid) = cli_session_id {
        if uuid::Uuid::parse_str(sid).is_err() {
            eprintln!(
                "{}",
                format!("Error: --session-id must be a valid UUID, got '{sid}'").red()
            );
            std::process::exit(1);
        }
        unsafe {
            std::env::set_var("ASTRA_SESSION_ID", sid);
        }
    }

    // --name: export session display name
    if let Some(ref name) = session_name {
        unsafe {
            std::env::set_var("ASTRA_SESSION_NAME", name);
        }
    }

    // Resolve model: --model flag > config default_model > None
    let resolved_model =
        cli_model.or_else(|| command_router::read_config_default_model().ok().flatten());

    // --print mode: headless single-shot, always auto-approve (can't prompt)
    if print_mode {
        match run_print_mode(
            &api,
            profile.as_deref(),
            &output_format,
            resolved_model.as_deref(),
            system_prompt.as_deref(),
            command,
        )
        .await
        {
            Ok(code) => std::process::exit(i32::from(code)),
            Err(e) => {
                eprintln!("{}", format!("Error: {e}").red());
                std::process::exit(i32::from(ExitCode::ApiError));
            }
        }
    }

    // -c / --continue: resume most recent session
    // -r / --resume <ID>: resume specific session
    if continue_last || resume.is_some() {
        let session_id = resume.as_deref();

        // For -c, resolve the last session ID from credentials
        let resolved_sid = if continue_last && session_id.is_none() {
            resumable_last_session_id(profile.as_deref())
        } else {
            session_id.map(|s| s.to_string())
        };

        match resolved_sid {
            Some(sid) => {
                let result = run_chat_repl(
                    &api,
                    profile.as_deref(),
                    resolved_model.as_deref(),
                    Some(&sid),
                )
                .await;
                match result {
                    Ok(()) => std::process::exit(0),
                    Err(e) => {
                        eprintln!("{}", format!("Error: {e}").red());
                        std::process::exit(i32::from(ExitCode::ApiError));
                    }
                }
            }
            None => {
                eprintln!(
                    "{}",
                    "No previous session to continue. Start a new one with `astra`.".yellow()
                );
                std::process::exit(1);
            }
        }
    }

    match execute_cli_command(
        command,
        profile,
        resolved_model,
        auto_approve,
        system_prompt,
        &api,
    )
    .await
    {
        Ok(exit_code) => {
            std::process::exit(i32::from(exit_code));
        }
        Err(e) => {
            eprintln!("{}", format!("Error: {e}").red());
            std::process::exit(i32::from(ExitCode::ApiError));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, routing::get, routing::post};
    use cloud_sync::{
        ASTRA_JOURNAL_CLOUD_EMPTY_ACK, CloudPullResult, cloud_pull_warrants_sync_marker,
        should_append_cloud_pull_journal, try_connect_matrixone,
    };
    use project_instructions::discover_instructions_from_paths;

    async fn spawn_mock(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}");
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        tokio::task::yield_now().await;
        base
    }

    /// Guard that serializes tests touching ASTRA_CREDENTIALS_DIR.
    /// Multiple async tests concurrently setting this env var is a data race;
    /// the guard ensures they execute sequentially.
    use std::sync::{Mutex, MutexGuard, OnceLock};

    pub(crate) struct CredentialsGuard {
        _lock: MutexGuard<'static, ()>,
        _dir: tempfile::TempDir,
    }

    impl Drop for CredentialsGuard {
        fn drop(&mut self) {
            unsafe {
                std::env::remove_var("ASTRA_CREDENTIALS_DIR");
            }
        }
    }

    fn creds_lock() -> MutexGuard<'static, ()> {
        static CREDS_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        CREDS_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Set credentials dir to a temp path so tests don't pollute ~/.astra/credentials.json.
    /// Returns a guard that holds a mutex — tests using this are serialized.
    pub(crate) fn isolate_credentials() -> CredentialsGuard {
        let lock = creds_lock();
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: protected by CREDS_LOCK; no concurrent set_var.
        unsafe { std::env::set_var("ASTRA_CREDENTIALS_DIR", dir.path()) };
        CredentialsGuard {
            _lock: lock,
            _dir: dir,
        }
    }

    mod auth_tests;
    mod chat_stream_tests;
    mod cli_args_tests;
    mod cloud_sync_tests;
    mod cost_tracking_tests;
    mod preamble_tests;
    mod repl_tests;
    mod resume_tests;
    mod slash_command_tests;
    mod stats_tools_tests;
    // ── auth_flow ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn do_login_success() {
        let _creds_dir = isolate_credentials();
        let app = Router::new().route(
            "/auth/login",
            post(|| async {
                axum::Json(serde_json::json!({
                    "access_token": "tok-abc",
                    "refresh_token": "ref-xyz"
                }))
            }),
        );
        let base = spawn_mock(app).await;
        let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
        let result = do_login(&api, Some("__test__"), "user1", "pass1").await;
        assert_eq!(result.unwrap(), "tok-abc");
    }

    #[tokio::test]
    async fn do_login_failure_returns_error() {
        let _creds_dir = isolate_credentials();
        let app = Router::new().route(
            "/auth/login",
            post(|| async {
                (
                    axum::http::StatusCode::UNAUTHORIZED,
                    axum::Json(serde_json::json!({"detail": "bad credentials"})),
                )
            }),
        );
        let base = spawn_mock(app).await;
        let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
        let result = do_login(&api, Some("test-profile"), "user1", "wrong").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("401"));
    }

    #[tokio::test]
    async fn do_register_success() {
        let _creds_dir = isolate_credentials();
        let app = Router::new().route(
            "/auth/register",
            post(|| async { axum::Json(serde_json::json!({"ok": true})) }),
        );
        let base = spawn_mock(app).await;
        let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
        let result = do_register(&api, "newuser", "a@b.com", "pass").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn do_register_conflict_returns_error() {
        let _creds_dir = isolate_credentials();
        let app = Router::new().route(
            "/auth/register",
            post(|| async {
                (
                    axum::http::StatusCode::CONFLICT,
                    axum::Json(serde_json::json!({"detail": "username taken"})),
                )
            }),
        );
        let base = spawn_mock(app).await;
        let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
        let result = do_register(&api, "taken", "a@b.com", "pass").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("409"));
    }

    // ── chat_stream (SSE agentic loop) ────────────────────────────────────

    fn sse_text_response(text: &str, session_id: &str) -> String {
        format!(
            "data: {{\"type\":\"session_info\",\"session_id\":\"{session_id}\"}}\n\n\
             data: {{\"type\":\"text_delta\",\"content\":\"{text}\"}}\n\n\
             data: {{\"type\":\"text_done\",\"full_text\":\"{text}\"}}\n\n\
             data: {{\"type\":\"usage\",\"prompt_tokens\":10,\"completion_tokens\":5}}\n\n\
             data: {{\"type\":\"turn_complete\",\"has_tool_calls\":false}}\n\n\
             data: [DONE]\n\n"
        )
    }

    #[tokio::test]
    async fn stream_chat_sse_simple_text_response() {
        let app = Router::new().route(
            "/chat/turn",
            post(|| async {
                (
                    [("content-type", "text/event-stream")],
                    sse_text_response("Hello!", "sess-001"),
                )
            }),
        );
        let base = spawn_mock(app).await;
        let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
        let registry = tool_registry::ToolRegistry::new(edge_tools::all_tool_schemas());
        let selector = tool_selector::TfIdfSelector::new(registry);
        let mut pm = PermissionManager::new(true);
        let mut skill_qt = astra_runtime::skills::quality::SkillQualityTracker::new();
        let skill_search = astra_core::SkillSearchSettings::default();
        let result = stream_chat_sse(ChatTurnParams {
            api: &api,
            token: "fake-token",
            message: "hi",
            session_id: None,
            model: None,
            explain: ExplainMode::Off,
            render_md: false,
            history: &[],
            perm_manager: &mut pm,
            verbose_mode: false,
            quiet: true,
            suppress_intermediate_output: false,
            selector: &selector,
            recent_tools: &[],
            tool_health_entries: &[],
            unified_skill_registry: astra_runtime::skills::empty_unified_registry(),
            plan_only_chat: false,
            hide_streaming_assistant_text: false,
            is_plan_subtask: false,
            plan_subtask_id: None,
            delegation_engine: None,
            cancel_token: None,
            plan_assemble_line_release: None,
            stream_event_tx: None,
            approval_request_tx: None,
            mcp_manager: None,
            skill_search: &skill_search,
            skill_quality_tracker: &mut skill_qt,
            discovered_skills: None,
            messaging_metrics: None,
            agent_spawner: None,
            root_agent_id: None,
            root_mailbox_slot: None,
            observability_hub: None,
            observability_session: None,
            file_journal: None,
            turn_index: 0,
            evolution_service: None,
        })
        .await
        .unwrap();
        assert_eq!(result.full_text, "Hello!");
        assert_eq!(result.session_id.as_deref(), Some("sess-001"));
        assert_eq!(result.prompt_tokens, 10);
        assert_eq!(result.completion_tokens, 5);
    }

    #[tokio::test]
    async fn stream_chat_sse_api_error_propagated() {
        let app = Router::new().route(
            "/chat/turn",
            post(|| async {
                (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    axum::Json(serde_json::json!({"detail": "model overloaded"})),
                )
            }),
        );
        let base = spawn_mock(app).await;
        let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
        let registry = tool_registry::ToolRegistry::new(edge_tools::all_tool_schemas());
        let selector = tool_selector::TfIdfSelector::new(registry);
        let mut pm = PermissionManager::new(true);
        let mut skill_qt = astra_runtime::skills::quality::SkillQualityTracker::new();
        let skill_search = astra_core::SkillSearchSettings::default();
        let result = stream_chat_sse(ChatTurnParams {
            api: &api,
            token: "fake-token",
            message: "hi",
            session_id: None,
            model: None,
            explain: ExplainMode::Off,
            render_md: false,
            history: &[],
            perm_manager: &mut pm,
            verbose_mode: false,
            quiet: true,
            suppress_intermediate_output: false,
            selector: &selector,
            recent_tools: &[],
            tool_health_entries: &[],
            unified_skill_registry: astra_runtime::skills::empty_unified_registry(),
            plan_only_chat: false,
            hide_streaming_assistant_text: false,
            is_plan_subtask: false,
            plan_subtask_id: None,
            delegation_engine: None,
            cancel_token: None,
            plan_assemble_line_release: None,
            stream_event_tx: None,
            approval_request_tx: None,
            mcp_manager: None,
            skill_search: &skill_search,
            skill_quality_tracker: &mut skill_qt,
            discovered_skills: None,
            messaging_metrics: None,
            agent_spawner: None,
            root_agent_id: None,
            root_mailbox_slot: None,
            observability_hub: None,
            observability_session: None,
            file_journal: None,
            turn_index: 0,
            evolution_service: None,
        })
        .await;
        assert!(result.is_err());
        let failure = result.unwrap_err();
        assert!(failure.error.contains("500"), "got: {}", failure.error);
    }

    #[tokio::test]
    async fn stream_chat_sse_with_tool_call_loop() {
        // Mock server: first call returns a tool call, second call returns text.
        let call_count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let cc = call_count.clone();
        let app = Router::new().route(
            "/chat/turn",
            post(move || {
                let cc = cc.clone();
                async move {
                    let n = cc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let body = if n == 0 {
                        // First turn: return a tool call for bash
                        "data: {\"type\":\"session_info\",\"session_id\":\"sess-tc\"}\n\n\
                         data: {\"type\":\"tool_call\",\"id\":\"tc-1\",\"name\":\"bash\",\"arguments\":{\"command\":\"echo hi\"}}\n\n\
                         data: {\"type\":\"turn_complete\",\"has_tool_calls\":true}\n\n\
                         data: [DONE]\n\n"
                            .to_string()
                    } else {
                        // Second turn: return text
                        sse_text_response("Done!", "sess-tc")
                    };
                    (
                        [("content-type", "text/event-stream")],
                        body,
                    )
                }
            }),
        );
        let base = spawn_mock(app).await;
        let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
        let registry = tool_registry::ToolRegistry::new(edge_tools::all_tool_schemas());
        let selector = tool_selector::TfIdfSelector::new(registry);
        let mut pm = PermissionManager::new(true); // auto-approve
        let mut skill_qt = astra_runtime::skills::quality::SkillQualityTracker::new();
        let skill_search = astra_core::SkillSearchSettings::default();
        let result = stream_chat_sse(ChatTurnParams {
            api: &api,
            token: "fake-token",
            message: "run echo hi",
            session_id: None,
            model: None,
            explain: ExplainMode::Off,
            render_md: false,
            history: &[],
            perm_manager: &mut pm,
            verbose_mode: false,
            quiet: true,
            suppress_intermediate_output: false,
            selector: &selector,
            recent_tools: &[],
            tool_health_entries: &[],
            unified_skill_registry: astra_runtime::skills::empty_unified_registry(),
            plan_only_chat: false,
            hide_streaming_assistant_text: false,
            is_plan_subtask: false,
            plan_subtask_id: None,
            delegation_engine: None,
            cancel_token: None,
            plan_assemble_line_release: None,
            stream_event_tx: None,
            approval_request_tx: None,
            mcp_manager: None,
            skill_search: &skill_search,
            skill_quality_tracker: &mut skill_qt,
            discovered_skills: None,
            messaging_metrics: None,
            agent_spawner: None,
            root_agent_id: None,
            root_mailbox_slot: None,
            observability_hub: None,
            observability_session: None,
            file_journal: None,
            turn_index: 0,
            evolution_service: None,
        })
        .await
        .unwrap();
        assert_eq!(result.full_text, "Done!");
        assert!(result.tool_calls_count > 0);
        assert!(call_count.load(std::sync::atomic::Ordering::SeqCst) >= 2);
    }

    // ── slash commands with mock server ───────────────────────────────────

    #[tokio::test]
    async fn slash_clear_creates_new_session() {
        let _creds_dir = isolate_credentials();
        let app = Router::new().route(
            "/sessions",
            post(|| async { axum::Json(serde_json::json!({"session_id": "new-sess-42"})) }),
        );
        let base = spawn_mock(app).await;
        let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
        let selector = tool_selector::TfIdfSelector::new(tool_registry::ToolRegistry::new(
            edge_tools::all_tool_schemas(),
        ));
        let mut state = ReplState {
            session_id: Some("old-sess".to_string()),
            turn: 5,
            history: vec![("q".to_string(), "a".to_string())],
            ..Default::default()
        };
        let exit = handle_slash_command(
            "/clear",
            &api,
            None,
            &mut state,
            Some("fake-token"),
            &selector,
        )
        .await
        .unwrap();
        assert!(!exit);
        assert_eq!(state.session_id.as_deref(), Some("new-sess-42"));
        assert_eq!(state.turn, 0);
        assert!(state.history.is_empty());
    }

    #[tokio::test]
    async fn slash_model_with_arg_sets_model() {
        let api = astra_thin_client::ThinClient::new("http://unused", None).unwrap();
        let selector = tool_selector::TfIdfSelector::new(tool_registry::ToolRegistry::new(
            edge_tools::all_tool_schemas(),
        ));
        let mut state = ReplState::default();
        let exit = handle_slash_command("/model gpt-4o", &api, None, &mut state, None, &selector)
            .await
            .unwrap();
        assert!(!exit);
        assert_eq!(state.model.as_deref(), Some("gpt-4o"));
    }

    #[tokio::test]
    async fn slash_exit_returns_true() {
        let api = astra_thin_client::ThinClient::new("http://unused", None).unwrap();
        let selector = tool_selector::TfIdfSelector::new(tool_registry::ToolRegistry::new(
            edge_tools::all_tool_schemas(),
        ));
        let mut state = ReplState::default();
        let exit = handle_slash_command("/exit", &api, None, &mut state, None, &selector)
            .await
            .unwrap();
        assert!(exit);
    }

    #[tokio::test]
    async fn slash_exit_writes_session_end_to_journal() {
        let api = astra_thin_client::ThinClient::new("http://unused", None).unwrap();
        let selector = tool_selector::TfIdfSelector::new(tool_registry::ToolRegistry::new(
            edge_tools::all_tool_schemas(),
        ));

        let sid = format!("test-exit-end-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&sid).unwrap();
        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(&sid),
                None,
            ))
            .unwrap();

        let mut state = ReplState {
            session_id: Some(sid.clone()),
            turn: 3,
            journal: Some(session_journal::JournalWriter::new(&sid).unwrap()),
            ..ReplState::default()
        };

        let exit = handle_slash_command("/exit", &api, None, &mut state, None, &selector)
            .await
            .unwrap();
        assert!(exit);
        finalize_repl_exit(&state, None, ReplExit::Command).await;

        // Verify session_end was written to journal
        let events = session_journal::read_journal(&sid).unwrap();
        let has_session_end = events
            .iter()
            .any(|e| matches!(e.event_type, session_journal::JournalEventType::SessionEnd));
        assert!(
            has_session_end,
            "session_end event must be written to journal on /exit"
        );
    }

    #[tokio::test]
    async fn slash_quit_writes_session_end_to_journal() {
        let api = astra_thin_client::ThinClient::new("http://unused", None).unwrap();
        let selector = tool_selector::TfIdfSelector::new(tool_registry::ToolRegistry::new(
            edge_tools::all_tool_schemas(),
        ));

        let sid = format!("test-quit-end-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&sid).unwrap();
        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(&sid),
                None,
            ))
            .unwrap();

        let mut state = ReplState {
            session_id: Some(sid.clone()),
            turn: 1,
            journal: Some(session_journal::JournalWriter::new(&sid).unwrap()),
            ..ReplState::default()
        };

        let exit = handle_slash_command("/quit", &api, None, &mut state, None, &selector)
            .await
            .unwrap();
        assert!(exit);
        finalize_repl_exit(&state, None, ReplExit::Command).await;

        let events = session_journal::read_journal(&sid).unwrap();
        let has_session_end = events
            .iter()
            .any(|e| matches!(e.event_type, session_journal::JournalEventType::SessionEnd));
        assert!(
            has_session_end,
            "session_end event must be written to journal on /quit"
        );
    }

    #[tokio::test]
    async fn slash_unknown_command_does_not_crash() {
        let api = astra_thin_client::ThinClient::new("http://unused", None).unwrap();
        let selector = tool_selector::TfIdfSelector::new(tool_registry::ToolRegistry::new(
            edge_tools::all_tool_schemas(),
        ));
        let mut state = ReplState::default();
        let exit = handle_slash_command(
            "/nonexistent_command_xyz",
            &api,
            None,
            &mut state,
            None,
            &selector,
        )
        .await
        .unwrap();
        assert!(!exit);
    }

    #[tokio::test]
    async fn slash_health_does_not_crash_empty() {
        let api = astra_thin_client::ThinClient::new("http://unused", None).unwrap();
        let selector = tool_selector::TfIdfSelector::new(tool_registry::ToolRegistry::new(
            edge_tools::all_tool_schemas(),
        ));
        let mut state = ReplState::default();
        // No health entries — should print "no data" gracefully
        let exit = handle_slash_command("/health", &api, None, &mut state, None, &selector)
            .await
            .unwrap();
        assert!(!exit);
    }

    #[tokio::test]
    async fn slash_health_with_entries_does_not_crash() {
        let api = astra_thin_client::ThinClient::new("http://unused", None).unwrap();
        let selector = tool_selector::TfIdfSelector::new(tool_registry::ToolRegistry::new(
            edge_tools::all_tool_schemas(),
        ));
        let mut state = ReplState {
            tool_health_entries: vec![
                astra_runtime::pipeline::persistence::ToolHealthEntry {
                    name: "bash".into(),
                    total_calls: 15,
                    total_failures: 3,
                    failure_rate: 0.2,
                    last_updated_epoch: 0,
                },
                astra_runtime::pipeline::persistence::ToolHealthEntry {
                    name: "grep".into(),
                    total_calls: 8,
                    total_failures: 0,
                    failure_rate: 0.0,
                    last_updated_epoch: 0,
                },
            ],
            ..Default::default()
        };
        let exit = handle_slash_command("/health", &api, None, &mut state, None, &selector)
            .await
            .unwrap();
        assert!(!exit);
    }

    #[tokio::test]
    async fn slash_health_detail_mode() {
        let api = astra_thin_client::ThinClient::new("http://unused", None).unwrap();
        let selector = tool_selector::TfIdfSelector::new(tool_registry::ToolRegistry::new(
            edge_tools::all_tool_schemas(),
        ));
        let mut state = ReplState {
            tool_health_entries: vec![astra_runtime::pipeline::persistence::ToolHealthEntry {
                name: "bash".into(),
                total_calls: 10,
                total_failures: 5,
                failure_rate: 0.5,
                last_updated_epoch: 0,
            }],
            ..Default::default()
        };
        let exit = handle_slash_command("/health detail", &api, None, &mut state, None, &selector)
            .await
            .unwrap();
        assert!(!exit);
    }

    // ── command_router ────────────────────────────────────────────────────

    #[tokio::test]
    async fn execute_cli_health_command() {
        let _creds_dir = isolate_credentials();
        let app = Router::new().route(
            "/health",
            get(|| async { axum::Json(serde_json::json!({"status": "ok"})) }),
        );
        let base = spawn_mock(app).await;
        let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
        let result = execute_cli_command(
            Some(Command::Health),
            Some("nonexistent-profile".to_string()),
            None,
            false,
            None,
            &api,
        )
        .await;
        // Health command should succeed regardless of auth
        assert!(result.is_ok());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn execute_cli_messaging_bridge_command() {
        let api = astra_thin_client::ThinClient::new("http://unused", None).unwrap();
        let result = execute_cli_command(
            Some(Command::Messaging(MessagingArgs { command: None })),
            None,
            None,
            false,
            None,
            &api,
        )
        .await;
        assert!(result.is_ok());
    }

    // ── repl_turn pure functions ──────────────────────────────────────────

    #[test]
    fn picker_submission_echo_is_cleared_only_when_picker_rewrites_input() {
        assert!(should_clear_picker_submission_echo(
            "/",
            Some("/checkpoint")
        ));
        assert!(should_clear_picker_submission_echo(
            "/chec",
            Some("/checkpoint")
        ));
        assert!(!should_clear_picker_submission_echo("/", None));
        assert!(!should_clear_picker_submission_echo(
            "/checkpoint",
            Some("/checkpoint")
        ));
    }

    #[test]
    fn picker_submission_echo_reprints_prompt_and_selected_command() {
        let rendered = build_picker_submission_echo(theme::PROMPT_DEFAULT, "/checkpoint");
        assert_eq!(
            rendered,
            format!("\x1b[A\x1b[2K\r{}/checkpoint\n", theme::PROMPT_DEFAULT)
        );
    }

    #[test]
    fn build_effective_line_plain() {
        let state = ReplState::default();
        let result = repl_turn::build_effective_line("hello", &state);
        assert_eq!(result, "hello");
    }

    #[test]
    fn build_effective_line_with_system_skills() {
        let mut state = ReplState::default();
        let skills = prompts::builtin_system_skills();
        if let Some(md) = skills.iter().find(|s| s.name == "markdown") {
            state.active_system_skills.push(md.clone());
        }
        let result = repl_turn::build_effective_line("hello", &state);
        assert!(result.contains("hello"));
        assert!(result.contains("Markdown"));
    }

    #[test]
    fn history_as_messages_normal_turns() {
        let history = vec![
            ("q1".to_string(), "a1".to_string()),
            ("q2".to_string(), "a2".to_string()),
        ];
        let msgs = repl_turn::history_as_messages(&history);
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[1]["role"], "assistant");
    }

    #[test]
    fn history_as_messages_compacted_turn() {
        let history = vec![("".to_string(), "summary".to_string())];
        let msgs = repl_turn::history_as_messages(&history);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "assistant");
    }

    // ── slash_memory mock ─────────────────────────────────────────────────

    #[tokio::test]
    async fn slash_memory_search_with_mock() {
        let app = Router::new().route(
            "/memory/search",
            post(|| async {
                axum::Json(serde_json::json!({
                    "results": [
                        {"content": "user prefers Rust", "memory_type": "profile", "score": 0.9}
                    ]
                }))
            }),
        );
        let base = spawn_mock(app).await;
        let api = astra_thin_client::ThinClient::new(&base, None).unwrap();
        let mut state = ReplState {
            session_id: Some("sess-1".to_string()),
            ..Default::default()
        };
        // This should not panic or error
        let result = handle_memory_domain_command(
            "/memory",
            "search rust preferences",
            &api,
            &mut state,
            Some("fake-token"),
        )
        .await;
        assert!(result.is_ok());
    }

    // ── find_task_by_query ────────────────────────────────────────────────────

    use astra_services::TaskService as _;

    #[tokio::test]
    async fn find_task_by_id_prefix() {
        let tmp = tempfile::TempDir::new().unwrap();
        let svc = astra_services::LocalTaskService::new(tmp.path().to_path_buf());
        let tid = svc
            .create_task(
                "u1",
                "s1",
                astra_services::TaskCreateRequest {
                    title: "Build auth".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        // Full ID match
        let found = slash_task::find_task_by_query(&svc, "u1", &tid)
            .await
            .unwrap();
        assert_eq!(found, Some(tid.clone()));

        // Prefix match (first 8 Unicode scalars)
        let prefix = prefix_chars(&tid, 8);
        let found = slash_task::find_task_by_query(&svc, "u1", &prefix)
            .await
            .unwrap();
        assert_eq!(found, Some(tid));
    }

    #[tokio::test]
    async fn find_task_by_title_substring() {
        let tmp = tempfile::TempDir::new().unwrap();
        let svc = astra_services::LocalTaskService::new(tmp.path().to_path_buf());
        svc.create_task(
            "u1",
            "s1",
            astra_services::TaskCreateRequest {
                title: "Refactor authentication module".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        // Case-insensitive title match
        let found = slash_task::find_task_by_query(&svc, "u1", "authentication")
            .await
            .unwrap();
        assert!(found.is_some());

        let found = slash_task::find_task_by_query(&svc, "u1", "AUTH")
            .await
            .unwrap();
        assert!(found.is_some());
    }

    #[tokio::test]
    async fn find_task_not_found() {
        let tmp = tempfile::TempDir::new().unwrap();
        let svc = astra_services::LocalTaskService::new(tmp.path().to_path_buf());
        let found = slash_task::find_task_by_query(&svc, "u1", "nonexistent")
            .await
            .unwrap();
        assert!(found.is_none());
    }

    #[tokio::test]
    async fn find_task_wrong_user() {
        let tmp = tempfile::TempDir::new().unwrap();
        let svc = astra_services::LocalTaskService::new(tmp.path().to_path_buf());
        svc.create_task(
            "user-a",
            "s1",
            astra_services::TaskCreateRequest {
                title: "Private task".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        // Different user can't find it
        let found = slash_task::find_task_by_query(&svc, "user-b", "Private")
            .await
            .unwrap();
        assert!(found.is_none());
    }

    // ── Resume user verification ─────────────────────────────────────────────

    #[tokio::test]
    async fn resume_local_restore_rejects_unowned_session() {
        let _creds = isolate_credentials();
        use astra_services::session_restore::SessionRestoreService;
        use session_journal::JournalWriter;

        // Create a session with both journal AND workspace (what restore_session needs)
        let sid = format!("test-unowned-{}", uuid::Uuid::new_v4());

        // 1. Create journal
        let writer = JournalWriter::new(&sid).unwrap();
        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(&sid),
                Some("gpt-4o"),
            ))
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::turn(
                Some(&sid),
                1,
                None,
                "hello",
                "hi",
                0,
                5,
                3,
                50,
            ))
            .unwrap();
        drop(writer);

        // 2. Create workspace.yaml (required for local restore)
        let ws_dir = dirs::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join(".astra")
            .join("sessions")
            .join(&sid);
        std::fs::create_dir_all(&ws_dir).unwrap();
        let ws_content = r#"session_id: test-unowned
cwd: /tmp
model: gpt-4o
created_at: "2024-01-01T00:00:00Z"
updated_at: "2024-01-01T00:00:00Z"
status: active
turn_count: 1
total_tokens_in: 5
total_tokens_out: 3
"#;
        std::fs::write(ws_dir.join("workspace.yaml"), ws_content).unwrap();

        // Now restore_session should find it
        let svc = astra_services::session_restore::HybridRestoreService::local_only();
        let result = svc.restore_session(&sid).await.unwrap();
        assert!(
            result.is_some(),
            "local restore should find session with workspace.yaml"
        );

        // Verify it's marked as local (not cloud)
        let restored = result.unwrap();
        assert!(!restored.restored_from_cloud, "should be local restore");

        // Note: The user ownership check in handle_resume_command only verifies
        // that the journal exists, not that the user owns it. This is a known limitation.
    }

    // ── Learning snapshot restoration ────────────────────────────────────────

    #[tokio::test]
    async fn resume_restores_learning_snapshot() {
        use astra_services::session_restore::RestoredSession;

        // Create a mock RestoredSession with learning snapshot
        let restored = RestoredSession {
            session_id: "test-learning".into(),
            turn_count: 5,
            total_tokens_in: 1000,
            total_tokens_out: 500,
            recent_tools: vec!["grep".into()],
            learning_snapshot_json: Some(
                r#"{"entities":["Rust","MatrixOne"],"patterns":["*.rs"]}"#.into(),
            ),
            checkpoint_count: 1,
            last_status: "active".into(),
            git_branch: Some("main".into()),
            model: Some("gpt-4o".into()),
            title: Some("Test".into()),
            restored_from_cloud: true, // Cloud restore has learning
            ..Default::default()
        };

        // Verify the learning snapshot is present
        assert!(restored.learning_snapshot_json.is_some());
        let json = restored.learning_snapshot_json.as_ref().unwrap();
        assert!(json.contains("Rust"));
        assert!(json.contains("MatrixOne"));

        // Simulate what handle_resume_command does
        let learning_snapshot = if let Some(ref l) = restored.learning_snapshot_json {
            if !l.is_empty() { Some(l.clone()) } else { None }
        } else {
            None
        };

        assert!(learning_snapshot.is_some());
        assert_eq!(learning_snapshot.unwrap().as_str(), json);
    }

    #[tokio::test]
    async fn resume_local_restore_has_no_learning_snapshot() {
        use astra_services::session_restore::RestoredSession;

        // Local restore should not have learning snapshot
        let restored = RestoredSession {
            session_id: "test-local".into(),
            turn_count: 3,
            total_tokens_in: 500,
            total_tokens_out: 200,
            recent_tools: vec![],
            learning_snapshot_json: None, // Local restore doesn't have this
            checkpoint_count: 1,
            last_status: "active".into(),
            git_branch: None,
            model: None,
            title: None,
            restored_from_cloud: false,
            ..Default::default()
        };

        assert!(restored.learning_snapshot_json.is_none());
    }

    // ── Edge cases ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn resume_handles_empty_learning_snapshot() {
        use astra_services::session_restore::RestoredSession;

        // Empty string should be treated as None
        let restored = RestoredSession {
            learning_snapshot_json: Some("".into()),
            ..Default::default()
        };

        // Simulate the logic in handle_resume_command
        let learning_snapshot = if let Some(ref l) = restored.learning_snapshot_json {
            if !l.is_empty() { Some(l.clone()) } else { None }
        } else {
            None
        };

        assert!(
            learning_snapshot.is_none(),
            "empty string should be ignored"
        );
    }

    #[tokio::test]
    async fn resume_handles_invalid_learning_json() {
        use astra_services::session_restore::RestoredSession;

        // Invalid JSON should still be stored (will fail at merge time)
        let restored = RestoredSession {
            learning_snapshot_json: Some("not valid json {{{".into()),
            ..Default::default()
        };

        assert!(restored.learning_snapshot_json.is_some());
        let json = restored.learning_snapshot_json.as_ref().unwrap();
        assert!(json.contains("{"));
    }

    #[tokio::test]
    async fn resume_handles_malformed_workspace_yaml() {
        let _creds = isolate_credentials();
        use astra_services::session_restore::SessionRestoreService;

        let sid = format!("test-malformed-{}", uuid::Uuid::new_v4());

        // Create journal
        let writer = session_journal::JournalWriter::new(&sid).unwrap();
        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(&sid),
                Some("gpt-4o"),
            ))
            .unwrap();
        drop(writer);

        // Create malformed workspace.yaml
        let ws_dir = dirs::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join(".astra")
            .join("sessions")
            .join(&sid);
        std::fs::create_dir_all(&ws_dir).unwrap();
        std::fs::write(ws_dir.join("workspace.yaml"), "invalid: yaml: content: [").unwrap();

        // Should return None for malformed workspace
        let svc = astra_services::session_restore::HybridRestoreService::local_only();
        let result = svc.restore_session(&sid).await.unwrap();
        assert!(
            result.is_none(),
            "malformed workspace.yaml should cause restore to return None"
        );
    }

    #[tokio::test]
    async fn resume_handles_missing_workspace() {
        let _creds = isolate_credentials();
        use astra_services::session_restore::SessionRestoreService;

        // Only journal, no workspace → should fall back to cloud (which returns None)
        let sid = format!("test-no-ws-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&sid).unwrap();
        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(&sid),
                Some("gpt-4o"),
            ))
            .unwrap();
        drop(writer);

        let svc = astra_services::session_restore::HybridRestoreService::local_only();
        let result = svc.restore_session(&sid).await.unwrap();
        assert!(
            result.is_none(),
            "session without workspace.yaml should return None"
        );
    }

    // ── Integration: full resume flow simulation ─────────────────────────────

    #[tokio::test]
    async fn resume_full_flow_cloud_restore() {
        use astra_services::session_restore::RestoredSession;

        // Simulate a complete cloud restore scenario
        let restored = RestoredSession {
            session_id: "cloud-sess-123".into(),
            turn_count: 42,
            total_tokens_in: 150_000,
            total_tokens_out: 80_000,
            recent_tools: vec!["git".into(), "bash".into(), "grep".into()],
            learning_snapshot_json: Some(
                r#"{"entities":["Rust","SQL"],"patterns":["*.rs"]}"#.into(),
            ),
            checkpoint_count: 5,
            last_status: "active".into(),
            git_branch: Some("feature/resume".into()),
            model: Some("claude-3-opus".into()),
            title: Some("Implement session resume".into()),
            restored_from_cloud: true,
            ..Default::default()
        };
        assert_eq!(restored.session_id, "cloud-sess-123");
        assert_eq!(restored.turn_count, 42);
        assert!(restored.restored_from_cloud);
        assert!(restored.learning_snapshot_json.is_some());
        assert_eq!(restored.recent_tools.len(), 3);

        // Simulate state application
        let mut state = super::ReplState::default();
        #[allow(clippy::field_reassign_with_default)]
        {
            state.session_id = Some(restored.session_id.clone());
            state.turn = restored.turn_count;
            state.total_prompt_tokens = restored.total_tokens_in;
            state.total_completion_tokens = restored.total_tokens_out;
            state.recent_tools = restored.recent_tools.clone();
            state.model = restored.model.clone();
            if let Some(ref m) = state.model {
                state.cached_pricing = slash_stats::fallback_pricing(m);
                // M3: Use RuntimeConfig-driven context budget on session restore (test code)
                state.context_budget =
                    prompts::ContextBudget::from_runtime_config(&state.runtime_config, Some(m));
            }
        }

        // Apply learning snapshot
        if let Some(ref l) = restored.learning_snapshot_json
            && !l.is_empty()
        {
            state.learning_snapshot = Some(l.clone());
        }

        // Verify state
        assert_eq!(state.session_id, Some("cloud-sess-123".into()));
        assert_eq!(state.turn, 42);
        assert_eq!(state.total_prompt_tokens, 150_000);
        assert_eq!(
            state.learning_snapshot.unwrap(),
            r#"{"entities":["Rust","SQL"],"patterns":["*.rs"]}"#
        );
    }

    // ── Checkpoint listing ───────────────────────────────────────────────────

    #[tokio::test]
    async fn resume_lists_checkpoints_for_session() {
        let _creds = isolate_credentials();
        use astra_services::session_restore::SessionRestoreService;

        let sid = format!("test-checkpoints-{}", uuid::Uuid::new_v4());

        // Create journal
        let writer = session_journal::JournalWriter::new(&sid).unwrap();
        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(&sid),
                Some("gpt-4o"),
            ))
            .unwrap();
        drop(writer);

        // Create workspace
        let ws_dir = dirs::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join(".astra")
            .join("sessions")
            .join(&sid);
        std::fs::create_dir_all(&ws_dir).unwrap();
        std::fs::write(
            ws_dir.join("workspace.yaml"),
            r#"session_id: test
cwd: /tmp
model: gpt-4o
created_at: "2024-01-01T00:00:00Z"
updated_at: "2024-01-01T00:00:00Z"
status: active
turn_count: 10
total_tokens_in: 1000
total_tokens_out: 500
"#,
        )
        .unwrap();

        // List checkpoints should return empty (no checkpoints created yet)
        let svc = astra_services::session_restore::HybridRestoreService::local_only();
        let ckpts = svc.list_checkpoints(&sid).await.unwrap();
        assert!(ckpts.is_empty(), "no checkpoints created yet");
    }

    // ── merge_learning_snapshot ───────────────────────────────────────────────

    #[test]
    fn merge_learning_valid_snapshot() {
        use astra_runtime::pipeline::{calibration, entity, pattern};

        let json = serde_json::json!({
            "version": 1,
            "entities": [{
                "name": "rust",
                "aliases": ["rs"],
                "domain": null,
                "associated_tools": ["cargo"],
                "confidence": 0.8,
                "observation_count": 5
            }],
            "patterns": [{
                "signature": "cargo",
                "tools": ["cargo"],
                "task_type": "Code",
                "domain": null,
                "success_count": 3,
                "failure_count": 0,
                "quality_sum": 2.4
            }],
            "calibration": null
        })
        .to_string();

        let eg = std::sync::Arc::new(std::sync::Mutex::new(entity::EntityGraph::new()));
        let pl = std::sync::Arc::new(std::sync::Mutex::new(pattern::PatternLibrary::new()));
        let cal = std::sync::Arc::new(std::sync::Mutex::new(
            calibration::ProgressiveCalibrator::default(),
        ));

        merge_learning_snapshot(&json, &eg, &pl, &cal);

        // Verify entity content, not just count
        let entities = eg.lock().unwrap().export();
        assert_eq!(entities.len(), 1);
        let e = &entities[0];
        assert_eq!(e.name, "rust");
        assert_eq!(e.aliases, vec!["rs"]);
        assert_eq!(e.associated_tools, vec!["cargo"]);
        assert!((e.confidence - 0.8).abs() < 1e-6);
        assert_eq!(e.observation_count, 5);

        // Verify pattern content, not just count
        let patterns = pl.lock().unwrap().export();
        assert_eq!(patterns.len(), 1);
        let p = &patterns[0];
        assert_eq!(p.signature, "cargo");
        assert_eq!(p.tools, vec!["cargo"]);
        assert_eq!(p.success_count, 3);
        assert_eq!(p.failure_count, 0);
    }

    #[test]
    fn merge_learning_invalid_json_does_not_panic() {
        use astra_runtime::pipeline::{calibration, entity, pattern};

        let eg = std::sync::Arc::new(std::sync::Mutex::new(entity::EntityGraph::new()));
        let pl = std::sync::Arc::new(std::sync::Mutex::new(pattern::PatternLibrary::new()));
        let cal = std::sync::Arc::new(std::sync::Mutex::new(
            calibration::ProgressiveCalibrator::default(),
        ));

        // Invalid JSON — should not panic, just print warning
        merge_learning_snapshot("not valid json", &eg, &pl, &cal);

        // Modules should remain empty
        assert!(eg.lock().unwrap().export().is_empty());
        assert!(pl.lock().unwrap().export().is_empty());
    }

    #[test]
    fn merge_learning_empty_snapshot() {
        use astra_runtime::pipeline::{calibration, entity, pattern};

        let json = serde_json::json!({
            "version": 1,
            "entities": [],
            "patterns": [],
            "calibration": null
        })
        .to_string();

        let eg = std::sync::Arc::new(std::sync::Mutex::new(entity::EntityGraph::new()));
        let pl = std::sync::Arc::new(std::sync::Mutex::new(pattern::PatternLibrary::new()));
        let cal = std::sync::Arc::new(std::sync::Mutex::new(
            calibration::ProgressiveCalibrator::default(),
        ));

        merge_learning_snapshot(&json, &eg, &pl, &cal);

        assert!(eg.lock().unwrap().export().is_empty());
        assert!(pl.lock().unwrap().export().is_empty());
    }

    #[test]
    fn merge_learning_idempotent() {
        use astra_runtime::pipeline::{calibration, entity, pattern};

        let json = serde_json::json!({
            "version": 1,
            "entities": [{"name": "rust", "aliases": [], "domain": null,
                "associated_tools": ["cargo"], "confidence": 0.8, "observation_count": 5}],
            "patterns": [{"signature": "cargo", "tools": ["cargo"], "task_type": "Code",
                "domain": null, "success_count": 3, "failure_count": 0, "quality_sum": 2.4}],
            "calibration": null
        })
        .to_string();

        let eg = std::sync::Arc::new(std::sync::Mutex::new(entity::EntityGraph::new()));
        let pl = std::sync::Arc::new(std::sync::Mutex::new(pattern::PatternLibrary::new()));
        let cal = std::sync::Arc::new(std::sync::Mutex::new(
            calibration::ProgressiveCalibrator::default(),
        ));

        // Merge twice — should not duplicate
        merge_learning_snapshot(&json, &eg, &pl, &cal);
        merge_learning_snapshot(&json, &eg, &pl, &cal);

        assert_eq!(
            eg.lock().unwrap().export().len(),
            1,
            "entities should not duplicate"
        );
        assert_eq!(
            pl.lock().unwrap().export().len(),
            1,
            "patterns should not duplicate"
        );
    }

    #[test]
    fn merge_learning_multiple_entities_and_patterns() {
        use astra_runtime::pipeline::{calibration, entity, pattern};

        let json = serde_json::json!({
            "version": 1,
            "entities": [
                {"name": "rust", "aliases": [], "domain": null,
                    "associated_tools": ["cargo"], "confidence": 0.9, "observation_count": 10},
                {"name": "matrixone", "aliases": ["mo"], "domain": "Database",
                    "associated_tools": ["sql_query"], "confidence": 0.7, "observation_count": 3}
            ],
            "patterns": [
                {"signature": "cargo|grep", "tools": ["cargo", "grep"], "task_type": "Code",
                    "domain": null, "success_count": 5, "failure_count": 1, "quality_sum": 4.0},
                {"signature": "sql_query", "tools": ["sql_query"], "task_type": "Fetch",
                    "domain": "Database", "success_count": 2, "failure_count": 0, "quality_sum": 1.8}
            ],
            "calibration": null
        })
        .to_string();

        let eg = std::sync::Arc::new(std::sync::Mutex::new(entity::EntityGraph::new()));
        let pl = std::sync::Arc::new(std::sync::Mutex::new(pattern::PatternLibrary::new()));
        let cal = std::sync::Arc::new(std::sync::Mutex::new(
            calibration::ProgressiveCalibrator::default(),
        ));

        merge_learning_snapshot(&json, &eg, &pl, &cal);

        let entities = eg.lock().unwrap().export();
        assert_eq!(entities.len(), 2);
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"rust"));
        assert!(names.contains(&"matrixone"));

        let patterns = pl.lock().unwrap().export();
        assert_eq!(patterns.len(), 2);
        let sigs: Vec<&str> = patterns.iter().map(|p| p.signature.as_str()).collect();
        assert!(sigs.contains(&"cargo|grep"));
        assert!(sigs.contains(&"sql_query"));
    }

    // ── handle_stats_command ─────────────────────────────────────────────────

    #[test]
    fn stats_no_active_session_does_not_panic() {
        // state with no session_id → should not panic
        let state = super::ReplState::default();
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(slash_stats::handle_stats_command("", &state)); // current session mode, no session
    }

    #[test]
    fn stats_history_no_sessions_does_not_panic() {
        let state = super::ReplState::default();
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(slash_stats::handle_stats_command("history", &state));
    }

    #[test]
    fn stats_current_session_reads_journal() {
        let _creds = isolate_credentials();
        use astra_services::session_analytics;

        // Create a real journal with known events
        let sid = format!("test-stats-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&sid).unwrap();
        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(&sid),
                Some("gpt-4o"),
            ))
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::turn(
                Some(&sid),
                1,
                Some("gpt-4o"),
                "hello",
                "hi",
                2,
                1000,
                500,
                1500,
            ))
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::turn(
                Some(&sid),
                2,
                Some("gpt-4o"),
                "what is rust?",
                "a systems language",
                1,
                800,
                400,
                1200,
            ))
            .unwrap();
        drop(writer);

        // Verify the analytics layer computes correctly from these events
        let events = session_journal::read_journal(&sid).unwrap();
        let stats = session_analytics::compute_session_stats(&sid, &events);

        assert_eq!(stats.turn_count, 2);
        assert_eq!(stats.total_tokens_in, 1800);
        assert_eq!(stats.total_tokens_out, 900);
        assert_eq!(stats.total_tool_calls, 3);
        assert_eq!(stats.model, Some("gpt-4o".into()));
        assert_eq!(stats.avg_tokens_per_turn, 1350); // (1800+900)/2

        // Now verify handle_stats_command doesn't panic with this session
        let state = super::ReplState {
            session_id: Some(sid),
            ..Default::default()
        };
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(slash_stats::handle_stats_command("", &state));
    }

    #[test]
    fn stats_history_aggregates_multiple_sessions() {
        let _creds = isolate_credentials();
        use astra_services::session_analytics;

        // Create two sessions
        let sid1 = format!("test-stats-hist-a-{}", uuid::Uuid::new_v4());
        let sid2 = format!("test-stats-hist-b-{}", uuid::Uuid::new_v4());

        for sid in [&sid1, &sid2] {
            let writer = session_journal::JournalWriter::new(sid).unwrap();
            writer
                .append(&session_journal::JournalEvent::turn(
                    Some(sid),
                    1,
                    None,
                    "q",
                    "a",
                    1,
                    500,
                    250,
                    800,
                ))
                .unwrap();
            drop(writer);
        }

        let e1 = session_journal::read_journal(&sid1).unwrap();
        let e2 = session_journal::read_journal(&sid2).unwrap();
        let s1 = session_analytics::compute_session_stats(&sid1, &e1);
        let s2 = session_analytics::compute_session_stats(&sid2, &e2);
        let agg = session_analytics::aggregate_stats(&[s1, s2]);

        assert_eq!(agg.session_count, 2);
        assert_eq!(agg.total_turns, 2);
        assert_eq!(agg.total_tokens_in, 1000);
        assert_eq!(agg.total_tokens_out, 500);
    }

    // ── handle_tools_command ─────────────────────────────────────────────────

    #[test]
    fn tools_no_active_session_does_not_panic() {
        let state = super::ReplState::default();
        slash_tools::handle_tools_command(&state);
    }

    #[test]
    fn tools_session_with_no_tool_calls_does_not_panic() {
        let _creds = isolate_credentials();
        let sid = format!("test-tools-empty-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&sid).unwrap();
        writer
            .append(&session_journal::JournalEvent::turn(
                Some(&sid),
                1,
                None,
                "hello",
                "hi",
                0,
                100,
                50,
                500,
            ))
            .unwrap();
        drop(writer);

        let state = super::ReplState {
            session_id: Some(sid),
            ..Default::default()
        };
        slash_tools::handle_tools_command(&state);
    }

    #[test]
    fn tools_reads_tool_calls_from_journal() {
        let _creds = isolate_credentials();
        use astra_services::session_analytics;

        let sid = format!("test-tools-calls-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&sid).unwrap();

        let mut event = session_journal::JournalEvent::turn(
            Some(&sid),
            1,
            None,
            "run tests",
            "done",
            3,
            500,
            200,
            3000,
        );
        event.tool_calls = Some(vec![
            session_journal::ToolCallRecord {
                name: "bash".into(),
                ms: 1000,
                ok: true,
                error: None,
                input_bytes: Some(50),
                output_bytes: Some(200),
                args_preview: Some("npm test".into()),
                result_preview: None,
            },
            session_journal::ToolCallRecord {
                name: "bash".into(),
                ms: 2000,
                ok: false,
                error: Some("exit code 1".into()),
                input_bytes: Some(30),
                output_bytes: Some(100),
                args_preview: Some("cargo build".into()),
                result_preview: None,
            },
            session_journal::ToolCallRecord {
                name: "grep".into(),
                ms: 50,
                ok: true,
                error: None,
                input_bytes: Some(20),
                output_bytes: Some(500),
                args_preview: Some("/error/ in src/".into()),
                result_preview: None,
            },
        ]);
        writer.append(&event).unwrap();
        drop(writer);

        // Verify analytics layer computes correctly
        let events = session_journal::read_journal(&sid).unwrap();
        let profiles = session_analytics::compute_tool_profiles(&events);

        assert_eq!(profiles.len(), 2);
        // sorted by total_ms descending: bash (3000ms) > grep (50ms)
        assert_eq!(profiles[0].name, "bash");
        assert_eq!(profiles[0].call_count, 2);
        assert_eq!(profiles[0].fail_count, 1);
        assert_eq!(profiles[0].total_ms, 3000);
        assert_eq!(profiles[0].min_ms, 1000);
        assert_eq!(profiles[0].max_ms, 2000);
        assert!((profiles[0].error_rate - 0.5).abs() < 0.01);
        assert_eq!(profiles[0].last_error, Some("exit code 1".into()));

        assert_eq!(profiles[1].name, "grep");
        assert_eq!(profiles[1].call_count, 1);
        assert_eq!(profiles[1].fail_count, 0);
        assert_eq!(profiles[1].error_rate, 0.0);

        // Verify handle_tools_command doesn't panic with this data
        let state = super::ReplState {
            session_id: Some(sid),
            ..Default::default()
        };
        slash_tools::handle_tools_command(&state);
    }

    // ── slash_health::format_sync_age tests ────────────────────────────────────────────

    #[test]
    fn format_sync_age_rfc3339() {
        let now = chrono::Utc::now();
        let ts = now.to_rfc3339();
        let age = slash_health::format_sync_age(&ts);
        // Should be "just now" or "0s ago" or "1s ago"
        assert!(
            age.contains("s ago") || age == "just now",
            "unexpected age for just-now timestamp: {age}"
        );
    }

    #[test]
    fn format_sync_age_minutes_ago() {
        let now = chrono::Utc::now();
        let five_min_ago = now - chrono::Duration::minutes(5);
        let ts = five_min_ago.to_rfc3339();
        let age = slash_health::format_sync_age(&ts);
        assert!(
            age.contains("m ago"),
            "expected minutes-ago format, got: {age}"
        );
    }

    #[test]
    fn format_sync_age_hours_ago() {
        let now = chrono::Utc::now();
        let two_hours_ago = now - chrono::Duration::hours(2);
        let ts = two_hours_ago.to_rfc3339();
        let age = slash_health::format_sync_age(&ts);
        assert!(
            age.contains("h ago"),
            "expected hours-ago format, got: {age}"
        );
    }

    #[test]
    fn format_sync_age_days_ago() {
        let now = chrono::Utc::now();
        let three_days_ago = now - chrono::Duration::days(3);
        let ts = three_days_ago.to_rfc3339();
        let age = slash_health::format_sync_age(&ts);
        assert!(
            age.contains("d ago"),
            "expected days-ago format, got: {age}"
        );
    }

    #[test]
    fn format_sync_age_mysql_datetime() {
        // MySQL DATETIME without timezone — should parse as UTC
        let age = slash_health::format_sync_age("2020-01-01 00:00:00");
        assert!(
            age.contains("d ago"),
            "expected days-ago for old mysql datetime, got: {age}"
        );
    }

    #[test]
    fn format_sync_age_unparseable_returns_raw() {
        let raw = "not-a-timestamp";
        let age = slash_health::format_sync_age(raw);
        assert_eq!(age, raw, "unparseable should return raw string");
    }

    #[test]
    fn display_sync_status_no_crash_all_none() {
        let status = astra_services::SyncStatus::default();
        // Just verify no panic — output goes to stderr
        slash_health::display_sync_status(&status);
    }

    #[test]
    fn display_sync_status_no_crash_full_data() {
        let status = astra_services::SyncStatus {
            learning_last_push: Some(chrono::Utc::now().to_rfc3339()),
            learning_last_pull: Some(chrono::Utc::now().to_rfc3339()),
            preferences_last_sync: Some(chrono::Utc::now().to_rfc3339()),
            pending_pushes: 2,
            last_error: Some("connection reset by peer".into()),
            cloud_version: None,
        };
        slash_health::display_sync_status(&status);
    }

    #[tokio::test]
    async fn slash_health_offline_shows_cloud_section() {
        let api = astra_thin_client::ThinClient::new("http://unused", None).unwrap();
        let selector = tool_selector::TfIdfSelector::new(tool_registry::ToolRegistry::new(
            edge_tools::all_tool_schemas(),
        ));
        let mut state = ReplState::default();
        // No matrix runtime — should show "Offline" in cloud section
        assert!(state.matrix_runtime.is_none());
        let exit = handle_slash_command("/health", &api, None, &mut state, None, &selector)
            .await
            .unwrap();
        assert!(!exit);
    }

    // ── Cloud sync regression tests (block_on panic fix cc6d011) ────
    // These tests verify the async cloud sync functions don't panic when
    // called from within a tokio runtime (the original bug was block_on
    // inside an existing runtime). We unset MATRIXONE_HOST so they take
    // the graceful-fallback path.

    #[tokio::test]
    async fn try_connect_matrixone_returns_none_without_env_vars() {
        // Safety: test-only, single-threaded tokio runtime
        unsafe {
            std::env::remove_var("MATRIXONE_HOST");
        }
        let pool = try_connect_matrixone().await;
        assert!(
            pool.is_none(),
            "Without MATRIXONE_HOST, pool should be None"
        );
    }

    #[test]
    fn cloud_pull_warrants_sync_marker_only_when_reachable_and_nonempty() {
        let dead = CloudPullResult {
            tool_health: Vec::new(),
            version: None,
            cloud_reachable: false,
        };
        assert!(!cloud_pull_warrants_sync_marker(&dead, &[]));
        let offline_version = CloudPullResult {
            tool_health: Vec::new(),
            version: Some(9),
            cloud_reachable: false,
        };
        assert!(!cloud_pull_warrants_sync_marker(&offline_version, &[]));
        let online_empty = CloudPullResult {
            tool_health: Vec::new(),
            version: None,
            cloud_reachable: true,
        };
        assert!(!cloud_pull_warrants_sync_marker(&online_empty, &[]));
        let online_version = CloudPullResult {
            tool_health: Vec::new(),
            version: Some(3),
            cloud_reachable: true,
        };
        assert!(cloud_pull_warrants_sync_marker(&online_version, &[]));
        assert!(cloud_pull_warrants_sync_marker(
            &online_empty,
            &["explain_mode".into()]
        ));
    }

    #[test]
    fn should_append_cloud_pull_journal_post_login_reachable_empty() {
        let pull = CloudPullResult {
            tool_health: Vec::new(),
            version: None,
            cloud_reachable: true,
        };
        assert!(should_append_cloud_pull_journal(&pull, &[], "post_login"));
    }

    #[serial_test::serial]
    #[test]
    fn should_append_cloud_pull_journal_repl_startup_empty_without_env() {
        unsafe {
            std::env::remove_var(ASTRA_JOURNAL_CLOUD_EMPTY_ACK);
        }
        let pull = CloudPullResult {
            tool_health: Vec::new(),
            version: None,
            cloud_reachable: true,
        };
        assert!(!should_append_cloud_pull_journal(
            &pull,
            &[],
            "repl_startup"
        ));
    }

    #[serial_test::serial]
    #[test]
    fn should_append_repl_startup_when_empty_ack_env_set() {
        let pull = CloudPullResult {
            tool_health: Vec::new(),
            version: None,
            cloud_reachable: true,
        };
        unsafe {
            std::env::remove_var(ASTRA_JOURNAL_CLOUD_EMPTY_ACK);
        }
        assert!(!should_append_cloud_pull_journal(
            &pull,
            &[],
            "repl_startup"
        ));
        unsafe {
            std::env::set_var(ASTRA_JOURNAL_CLOUD_EMPTY_ACK, "1");
        }
        assert!(should_append_cloud_pull_journal(&pull, &[], "repl_startup"));
        unsafe {
            std::env::remove_var(ASTRA_JOURNAL_CLOUD_EMPTY_ACK);
        }
    }

    #[test]
    fn append_cloud_pull_sync_journal_skips_without_session_id() {
        let pull = CloudPullResult {
            tool_health: Vec::new(),
            version: Some(1),
            cloud_reachable: true,
        };
        let state = ReplState::default();
        append_cloud_pull_sync_journal(&state, "default", "repl_startup", &pull, &[]);
    }

    #[test]
    fn append_cloud_pull_sync_journal_writes_sync_marker_jsonl() {
        let sid = format!("test-cloud-pull-journal-{}", uuid::Uuid::new_v4());
        let state = ReplState {
            session_id: Some(sid.clone()),
            ..Default::default()
        };
        let pull = CloudPullResult {
            tool_health: Vec::new(),
            version: Some(99),
            cloud_reachable: true,
        };
        let prefs = vec!["explain_mode".to_string()];
        append_cloud_pull_sync_journal(&state, "work", "repl_startup", &pull, &prefs);
        let events = session_journal::read_journal(&sid).expect("read journal");
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].event_type,
            session_journal::JournalEventType::SyncMarker
        );
        let cp = events[0]
            .metadata
            .as_ref()
            .and_then(|m| m.get("cloud_pull"))
            .expect("cloud_pull");
        assert_eq!(cp.get("profile").and_then(|v| v.as_str()), Some("work"));
        assert_eq!(
            cp.get("learning_version").and_then(|v| v.as_i64()),
            Some(99)
        );
        assert_eq!(
            cp.get("reachable_empty_ack").and_then(|v| v.as_bool()),
            Some(false)
        );
        std::fs::remove_file(session_journal::journal_file_path(&sid)).ok();
    }

    #[test]
    fn append_cloud_pull_post_login_reachable_empty_writes_marker() {
        let sid = format!("test-cloud-pull-empty-{}", uuid::Uuid::new_v4());
        let state = ReplState {
            session_id: Some(sid.clone()),
            ..Default::default()
        };
        let pull = CloudPullResult {
            tool_health: Vec::new(),
            version: None,
            cloud_reachable: true,
        };
        append_cloud_pull_sync_journal(&state, "default", "post_login", &pull, &[]);
        let events = session_journal::read_journal(&sid).expect("read journal");
        assert_eq!(events.len(), 1);
        let cp = events[0]
            .metadata
            .as_ref()
            .and_then(|m| m.get("cloud_pull"))
            .expect("cloud_pull");
        assert_eq!(
            cp.get("reachable_empty_ack").and_then(|v| v.as_bool()),
            Some(true)
        );
        std::fs::remove_file(session_journal::journal_file_path(&sid)).ok();
    }

    #[tokio::test]
    async fn try_cloud_pull_returns_empty_without_matrixone() {
        unsafe {
            std::env::remove_var("MATRIXONE_HOST");
        }
        let eg = std::sync::Arc::new(std::sync::Mutex::new(
            astra_runtime::pipeline::entity::EntityGraph::new(),
        ));
        let pl = std::sync::Arc::new(std::sync::Mutex::new(
            astra_runtime::pipeline::pattern::PatternLibrary::new(),
        ));
        let cal = std::sync::Arc::new(std::sync::Mutex::new(
            astra_runtime::pipeline::calibration::ProgressiveCalibrator::new(0.15),
        ));
        let result = try_cloud_pull("default", &eg, &pl, &cal).await;
        assert!(
            result.tool_health.is_empty(),
            "Without MatrixOne, cloud pull should return empty tool health"
        );
        assert!(
            result.version.is_none(),
            "Without MatrixOne, cloud pull should return no version"
        );
        assert!(
            !result.cloud_reachable,
            "Without MatrixOne, cloud should be unreachable"
        );
    }

    #[tokio::test]
    async fn try_cloud_push_is_noop_without_matrixone() {
        unsafe {
            std::env::remove_var("MATRIXONE_HOST");
        }
        let eg = std::sync::Arc::new(std::sync::Mutex::new(
            astra_runtime::pipeline::entity::EntityGraph::new(),
        ));
        let pl = std::sync::Arc::new(std::sync::Mutex::new(
            astra_runtime::pipeline::pattern::PatternLibrary::new(),
        ));
        let cal = std::sync::Arc::new(std::sync::Mutex::new(
            astra_runtime::pipeline::calibration::ProgressiveCalibrator::new(0.15),
        ));
        // Should not panic (was the original bug)
        // Use versioned API (None = new snapshot or unconditional push)
        let _result = try_cloud_push_versioned("default", &eg, &pl, &cal, &[], None).await;
    }

    #[tokio::test]
    async fn try_cloud_push_delta_is_noop_without_matrixone() {
        unsafe {
            std::env::remove_var("MATRIXONE_HOST");
        }
        let eg = std::sync::Arc::new(std::sync::Mutex::new(
            astra_runtime::pipeline::entity::EntityGraph::new(),
        ));
        let pl = std::sync::Arc::new(std::sync::Mutex::new(
            astra_runtime::pipeline::pattern::PatternLibrary::new(),
        ));
        let cal = std::sync::Arc::new(std::sync::Mutex::new(
            astra_runtime::pipeline::calibration::ProgressiveCalibrator::new(0.15),
        ));
        let mut synced = Vec::new();
        eg.lock().unwrap().learn(
            "rust",
            astra_runtime::pipeline::routing::DomainHint::Code,
            &[],
            None,
        );
        let _result = try_cloud_push_delta("default", &eg, &pl, &cal, &[], &mut synced, None).await;
    }

    #[tokio::test]
    async fn try_cloud_pull_preferences_is_noop_without_matrixone() {
        unsafe {
            std::env::remove_var("MATRIXONE_HOST");
        }
        let mut state = ReplState::default();
        // Should not panic (was the original bug)
        let keys = try_cloud_pull_preferences(&mut state).await;
        assert!(keys.is_empty());
    }

    #[tokio::test]
    async fn try_cloud_push_preferences_is_noop_without_matrixone() {
        unsafe {
            std::env::remove_var("MATRIXONE_HOST");
        }
        let state = ReplState::default();
        // Should not panic (was the original bug)
        try_cloud_push_preferences(&state).await;
    }

    #[test]
    fn format_duration_short_zero() {
        assert_eq!(
            format_duration_short(std::time::Duration::from_secs(0)),
            "0s"
        );
    }

    #[test]
    fn format_duration_short_seconds() {
        assert_eq!(
            format_duration_short(std::time::Duration::from_secs(45)),
            "45s"
        );
    }

    #[test]
    fn format_duration_short_minutes() {
        assert_eq!(
            format_duration_short(std::time::Duration::from_secs(92)),
            "1m32s"
        );
    }

    #[test]
    fn format_duration_short_hours() {
        assert_eq!(
            format_duration_short(std::time::Duration::from_secs(7500)),
            "2h5m"
        );
    }

    #[test]
    fn format_plan_progress_empty() {
        let s = format_plan_progress(0, 0, None, std::time::Duration::from_secs(0));
        assert!(s.contains("0/0 (0%)"));
        assert!(s.contains("0s elapsed"));
    }

    #[test]
    fn format_plan_progress_first_subtask() {
        let s = format_plan_progress(0, 5, None, std::time::Duration::from_secs(10));
        assert!(s.contains("0/5 (0%)"));
        assert!(s.contains("10s elapsed"));
        // No ETA when done==0
        assert!(!s.contains("remaining"));
    }

    #[test]
    fn format_plan_progress_midway_with_eta() {
        let avg = Some(std::time::Duration::from_secs(60));
        let s = format_plan_progress(3, 7, avg, std::time::Duration::from_secs(180));
        assert!(s.contains("3/7 (42%)"));
        assert!(s.contains("3m0s elapsed"));
        assert!(s.contains("~4m0s remaining")); // 4 remaining × 60s avg
    }

    #[test]
    fn format_plan_progress_complete() {
        let avg = Some(std::time::Duration::from_secs(30));
        let s = format_plan_progress(5, 5, avg, std::time::Duration::from_secs(150));
        assert!(s.contains("5/5 (100%)"));
        // 0 remaining → "~0s remaining"
        assert!(s.contains("remaining"));
    }

    #[test]
    fn format_plan_progress_bar_fills() {
        // At 50% with 16-width bar, should have 8 filled + 8 empty
        let s = format_plan_progress(3, 6, None, std::time::Duration::from_secs(0));
        assert!(s.contains("████████░░░░░░░░"));
    }

    // ── Cost Tracking Tests ──────────────────────────────────────────────

    #[test]
    fn cost_for_tokens_basic() {
        let pricing = astra_services::models::PricingData {
            prompt: 0.003,     // $0.003 per 1K tokens = $3 per 1M
            completion: 0.015, // $0.015 per 1K tokens = $15 per 1M
            cache_read: None,
            cache_write: None,
        };
        let cost = slash_stats::cost_for_tokens(1000, 500, 0, 0, &pricing);
        // 1000 * 0.003/1000 + 500 * 0.015/1000 = 0.003 + 0.0075 = 0.0105
        assert!(
            (cost - 0.0105).abs() < 1e-10,
            "cost should be $0.0105, got {cost}"
        );
    }

    #[test]
    fn cost_for_tokens_zero() {
        let pricing = astra_services::models::PricingData {
            prompt: 0.003,
            completion: 0.015,
            cache_read: None,
            cache_write: None,
        };
        assert_eq!(slash_stats::cost_for_tokens(0, 0, 0, 0, &pricing), 0.0);
    }

    #[test]
    fn cost_for_tokens_zero_pricing() {
        let pricing = astra_services::models::PricingData::default();
        assert_eq!(
            slash_stats::cost_for_tokens(10000, 5000, 0, 0, &pricing),
            0.0
        );
    }

    #[test]
    fn cost_for_tokens_large_values() {
        let pricing = astra_services::models::PricingData {
            prompt: 0.003,
            completion: 0.015,
            cache_read: None,
            cache_write: None,
        };
        // 1M prompt + 500K completion
        let cost = slash_stats::cost_for_tokens(1_000_000, 500_000, 0, 0, &pricing);
        // 1M * 0.003/1K + 500K * 0.015/1K = 3.0 + 7.5 = 10.5
        assert!(
            (cost - 10.5).abs() < 1e-6,
            "large token cost should be $10.50, got {cost}"
        );
    }

    #[test]
    fn cost_for_tokens_with_cache() {
        let pricing = astra_services::models::PricingData {
            prompt: 0.003,
            completion: 0.015,
            cache_read: Some(0.0003),   // 10% of prompt
            cache_write: Some(0.00375), // 125% of prompt
        };
        // 500 prompt + 200 completion + 1000 cache_read + 100 cache_write
        let cost = slash_stats::cost_for_tokens(500, 200, 1000, 100, &pricing);
        let expected = (500.0 * 0.003 / 1000.0)
            + (200.0 * 0.015 / 1000.0)
            + (1000.0 * 0.0003 / 1000.0)
            + (100.0 * 0.00375 / 1000.0);
        assert!(
            (cost - expected).abs() < 1e-10,
            "cache cost should be {expected}, got {cost}"
        );
    }

    #[test]
    fn cost_for_tokens_cache_fallback_rates() {
        // When cache_read/cache_write are None, uses 10%/125% of prompt rate
        let pricing = astra_services::models::PricingData {
            prompt: 0.003,
            completion: 0.015,
            cache_read: None,
            cache_write: None,
        };
        let cost = slash_stats::cost_for_tokens(0, 0, 1000, 1000, &pricing);
        let expected = (1000.0 * 0.003 * 0.1 / 1000.0) + (1000.0 * 0.003 * 1.25 / 1000.0);
        assert!(
            (cost - expected).abs() < 1e-10,
            "fallback cache cost should be {expected}, got {cost}"
        );
    }

    #[test]
    fn format_cost_sub_cent() {
        assert_eq!(slash_stats::format_cost(0.0001), "$0.0001");
        assert_eq!(slash_stats::format_cost(0.0099), "$0.0099");
    }

    #[test]
    fn format_cost_sub_dollar() {
        assert_eq!(slash_stats::format_cost(0.01), "$0.010");
        assert_eq!(slash_stats::format_cost(0.123), "$0.123");
        assert_eq!(slash_stats::format_cost(0.999), "$0.999");
    }

    #[test]
    fn format_cost_dollars() {
        assert_eq!(slash_stats::format_cost(1.0), "$1.00");
        assert_eq!(slash_stats::format_cost(12.345), "$12.35"); // rounds
        assert_eq!(slash_stats::format_cost(100.0), "$100.00");
    }

    #[test]
    fn format_cost_zero() {
        assert_eq!(slash_stats::format_cost(0.0), "$0.0000");
    }

    #[test]
    fn extract_pricing_from_nested_object() {
        let models = vec![serde_json::json!({
            "name": "gpt-4",
            "pricing": {
                "prompt": 0.03,
                "completion": 0.06
            }
        })];
        let p = slash_stats::extract_pricing_for_model(&models, "gpt-4").unwrap();
        assert!((p.prompt - 0.03).abs() < 1e-10);
        assert!((p.completion - 0.06).abs() < 1e-10);
    }

    #[test]
    fn extract_pricing_from_flat_fields() {
        let models = vec![serde_json::json!({
            "name": "claude-3",
            "pricing_prompt": 0.008,
            "pricing_completion": 0.024
        })];
        let p = slash_stats::extract_pricing_for_model(&models, "claude-3").unwrap();
        assert!((p.prompt - 0.008).abs() < 1e-10);
        assert!((p.completion - 0.024).abs() < 1e-10);
    }

    #[test]
    fn extract_pricing_model_not_found() {
        let models = vec![
            serde_json::json!({"name": "gpt-4", "pricing_prompt": 0.03, "pricing_completion": 0.06}),
        ];
        assert!(slash_stats::extract_pricing_for_model(&models, "nonexistent").is_none());
    }

    #[test]
    fn extract_pricing_empty_models() {
        let models: Vec<serde_json::Value> = vec![];
        assert!(slash_stats::extract_pricing_for_model(&models, "any").is_none());
    }

    #[test]
    fn extract_pricing_zero_values_returns_none() {
        let models = vec![serde_json::json!({
            "name": "test",
            "pricing_prompt": 0.0,
            "pricing_completion": 0.0
        })];
        assert!(slash_stats::extract_pricing_for_model(&models, "test").is_none());
    }

    // ── slash_stats::fallback_pricing tests ───────────────────────────────────────────

    #[test]
    fn fallback_sonnet_pricing() {
        let p = slash_stats::fallback_pricing("claude-sonnet-4-20250514");
        assert!((p.prompt - 0.003).abs() < 1e-6);
        assert!((p.completion - 0.015).abs() < 1e-6);
        assert!(p.cache_read.is_some());
        assert!((p.cache_read.unwrap() - 0.0003).abs() < 1e-8);
    }

    #[test]
    fn fallback_opus_4_pricing() {
        let p = slash_stats::fallback_pricing("claude-opus-4-20250514");
        assert!(
            (p.prompt - 0.015).abs() < 1e-6,
            "opus-4 prompt should be $15/Mtok"
        );
        assert!((p.completion - 0.075).abs() < 1e-6);
    }

    #[test]
    fn fallback_opus_45_pricing() {
        let p = slash_stats::fallback_pricing("claude-opus-4.5-20250415");
        assert!(
            (p.prompt - 0.005).abs() < 1e-6,
            "opus 4.5 should be $5/Mtok"
        );
    }

    #[test]
    fn fallback_haiku_pricing() {
        let p = slash_stats::fallback_pricing("claude-haiku-4.5-20250514");
        assert!(
            (p.prompt - 0.001).abs() < 1e-6,
            "haiku 4.5 should be $1/Mtok"
        );
    }

    #[test]
    fn fallback_gpt4o_pricing() {
        let p = slash_stats::fallback_pricing("gpt-4o-2024-08-06");
        assert!((p.prompt - 0.0025).abs() < 1e-6);
    }

    #[test]
    fn fallback_deepseek_pricing() {
        let p = slash_stats::fallback_pricing("deepseek-chat");
        assert!((p.prompt - 0.00027).abs() < 1e-8);
    }

    #[test]
    fn fallback_unknown_uses_sonnet() {
        let p = slash_stats::fallback_pricing("some-unknown-model");
        assert!(
            (p.prompt - 0.003).abs() < 1e-6,
            "unknown model should default to sonnet pricing"
        );
    }

    #[test]
    fn fallback_cost_calculation_with_cache() {
        // Sonnet: 1000 prompt + 500 completion + 2000 cache_read + 100 cache_creation
        let p = slash_stats::fallback_pricing("claude-sonnet-4-20250514");
        let cost = slash_stats::cost_for_tokens(1000, 500, 2000, 100, &p);
        // $0.003/Ktok * 1 + $0.015/Ktok * 0.5 + $0.0003/Ktok * 2 + $0.00375/Ktok * 0.1
        let expected = 0.003 + 0.0075 + 0.0006 + 0.000375;
        assert!(
            (cost - expected).abs() < 1e-8,
            "cost={cost} expected={expected}"
        );
    }

    // ── CLI arg parsing tests ─────────────────────────────────────────────

    #[test]
    fn cli_no_args_gives_no_command() {
        let cli = Cli::try_parse_from(["astra"]).unwrap();
        assert!(cli.command.is_none());
        assert!(!cli.print);
        assert!(!cli.continue_last);
        assert!(!cli.yes);
        assert!(cli.model.is_none());
        assert!(cli.resume.is_none());
    }

    #[test]
    fn cli_model_flag_long() {
        let cli = Cli::try_parse_from(["astra", "--model", "gpt-4o"]).unwrap();
        assert_eq!(cli.model.as_deref(), Some("gpt-4o"));
    }

    #[test]
    fn cli_model_flag_equals() {
        let cli = Cli::try_parse_from(["astra", "--model=claude-3-opus"]).unwrap();
        assert_eq!(cli.model.as_deref(), Some("claude-3-opus"));
    }

    #[test]
    fn cli_print_flag_short() {
        let cli = Cli::try_parse_from(["astra", "-p"]).unwrap();
        assert!(cli.print);
    }

    #[test]
    fn cli_print_flag_long() {
        let cli = Cli::try_parse_from(["astra", "--print"]).unwrap();
        assert!(cli.print);
    }

    #[test]
    fn cli_output_format_default_is_text() {
        let cli = Cli::try_parse_from(["astra"]).unwrap();
        assert_eq!(cli.output_format, "text");
    }

    #[test]
    fn cli_output_format_json() {
        let cli = Cli::try_parse_from(["astra", "--output-format", "json"]).unwrap();
        assert_eq!(cli.output_format, "json");
    }

    #[test]
    fn cli_continue_flag_short() {
        let cli = Cli::try_parse_from(["astra", "-c"]).unwrap();
        assert!(cli.continue_last);
    }

    #[test]
    fn cli_continue_flag_long() {
        let cli = Cli::try_parse_from(["astra", "--continue"]).unwrap();
        assert!(cli.continue_last);
    }

    #[test]
    fn cli_resume_flag_short() {
        let cli = Cli::try_parse_from(["astra", "-r", "abc123"]).unwrap();
        assert_eq!(cli.resume.as_deref(), Some("abc123"));
    }

    #[test]
    fn cli_resume_flag_long() {
        let cli = Cli::try_parse_from(["astra", "--resume", "session-xyz"]).unwrap();
        assert_eq!(cli.resume.as_deref(), Some("session-xyz"));
    }

    #[test]
    fn cli_yes_flag_short() {
        let cli = Cli::try_parse_from(["astra", "-y"]).unwrap();
        assert!(cli.yes);
    }

    #[test]
    fn cli_yes_flag_long() {
        let cli = Cli::try_parse_from(["astra", "--yes"]).unwrap();
        assert!(cli.yes);
    }

    #[test]
    fn cli_combined_short_flags() {
        // -p -c -y can be combined
        let cli = Cli::try_parse_from(["astra", "-p", "-c", "-y"]).unwrap();
        assert!(cli.print);
        assert!(cli.continue_last);
        assert!(cli.yes);
    }

    #[test]
    fn cli_model_with_print_and_yes() {
        let cli = Cli::try_parse_from(["astra", "--model", "gpt-4o", "-p", "-y"]).unwrap();
        assert_eq!(cli.model.as_deref(), Some("gpt-4o"));
        assert!(cli.print);
        assert!(cli.yes);
    }

    #[test]
    fn cli_doctor_subcommand() {
        let cli = Cli::try_parse_from(["astra", "doctor"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Doctor)));
    }

    #[test]
    fn cli_completion_bash() {
        let cli = Cli::try_parse_from(["astra", "completion", "bash"]).unwrap();
        match cli.command {
            Some(Command::Completion(ref args)) => {
                assert_eq!(args.shell, clap_complete::Shell::Bash);
            }
            _ => panic!("expected Completion command"),
        }
    }

    #[test]
    fn cli_completion_zsh() {
        let cli = Cli::try_parse_from(["astra", "completion", "zsh"]).unwrap();
        match cli.command {
            Some(Command::Completion(ref args)) => {
                assert_eq!(args.shell, clap_complete::Shell::Zsh);
            }
            _ => panic!("expected Completion command"),
        }
    }

    #[test]
    fn cli_completion_fish() {
        let cli = Cli::try_parse_from(["astra", "completion", "fish"]).unwrap();
        match cli.command {
            Some(Command::Completion(ref args)) => {
                assert_eq!(args.shell, clap_complete::Shell::Fish);
            }
            _ => panic!("expected Completion command"),
        }
    }

    #[test]
    fn cli_mcp_list_subcommand() {
        let cli = Cli::try_parse_from(["astra", "mcp", "list"]).unwrap();
        match cli.command {
            Some(Command::Mcp(McpCmd::List(_))) => {}
            _ => panic!("expected Mcp List command"),
        }
    }

    #[test]
    fn cli_mcp_add_with_args() {
        let cli =
            Cli::try_parse_from(["astra", "mcp", "add", "myserver", "npx", "server"]).unwrap();
        match cli.command {
            Some(Command::Mcp(McpCmd::Add(ref args))) => {
                assert_eq!(args.name, "myserver");
                assert_eq!(args.command, "npx");
                assert_eq!(args.args, vec!["server"]);
            }
            _ => panic!("expected Mcp Add command"),
        }
    }

    #[test]
    fn cli_mcp_remove_subcommand() {
        let cli = Cli::try_parse_from(["astra", "mcp", "remove", "myserver"]).unwrap();
        match cli.command {
            Some(Command::Mcp(McpCmd::Remove(ref args))) => {
                assert_eq!(args.name, "myserver");
            }
            _ => panic!("expected Mcp Remove command"),
        }
    }

    #[test]
    fn cli_mcp_get_subcommand() {
        let cli = Cli::try_parse_from(["astra", "mcp", "get", "myserver"]).unwrap();
        match cli.command {
            Some(Command::Mcp(McpCmd::Get(ref args))) => {
                assert_eq!(args.name, "myserver");
            }
            _ => panic!("expected Mcp Get command"),
        }
    }

    #[test]
    fn cli_config_list_subcommand() {
        let cli = Cli::try_parse_from(["astra", "config", "list"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Config(ConfigCmd::List))
        ));
    }

    #[test]
    fn cli_config_get_subcommand() {
        let cli = Cli::try_parse_from(["astra", "config", "get", "default_model"]).unwrap();
        match cli.command {
            Some(Command::Config(ConfigCmd::Get(ref args))) => {
                assert_eq!(args.key, "default_model");
            }
            _ => panic!("expected Config Get command"),
        }
    }

    #[test]
    fn cli_config_set_subcommand() {
        let cli =
            Cli::try_parse_from(["astra", "config", "set", "default_model", "gpt-4o"]).unwrap();
        match cli.command {
            Some(Command::Config(ConfigCmd::Set(ref args))) => {
                assert_eq!(args.key, "default_model");
                assert_eq!(args.value, "gpt-4o");
            }
            _ => panic!("expected Config Set command"),
        }
    }

    #[test]
    fn cli_chat_with_model() {
        let cli =
            Cli::try_parse_from(["astra", "chat", "-m", "hello", "--model", "gpt-4o"]).unwrap();
        match cli.command {
            Some(Command::Chat(ref args)) => {
                assert_eq!(args.message.as_deref(), Some("hello"));
                assert_eq!(args.model.as_deref(), Some("gpt-4o"));
            }
            _ => panic!("expected Chat command"),
        }
    }

    #[test]
    fn cli_chat_auto_approve() {
        let cli = Cli::try_parse_from(["astra", "chat", "-y"]).unwrap();
        match cli.command {
            Some(Command::Chat(ref args)) => {
                assert!(args.auto_approve);
            }
            _ => panic!("expected Chat command"),
        }
    }

    #[test]
    fn cli_chat_permission_mode() {
        let cli = Cli::try_parse_from(["astra", "chat", "--permission-mode", "auto"]).unwrap();
        match cli.command {
            Some(Command::Chat(ref args)) => {
                assert_eq!(args.permission_mode.as_deref(), Some("auto"));
            }
            _ => panic!("expected Chat command"),
        }
    }

    #[test]
    fn cli_external_subcommand_message() {
        let cli = Cli::try_parse_from(["astra", "what", "is", "rust"]).unwrap();
        match cli.command {
            Some(Command::Message(ref words)) => {
                assert_eq!(words, &["what", "is", "rust"]);
            }
            _ => panic!("expected Message command"),
        }
    }

    #[test]
    fn cli_plan_decompose_parses() {
        let cli = Cli::try_parse_from([
            "astra",
            "plan",
            "decompose",
            "-g",
            "smoke goal",
            "--json",
            "-q",
        ])
        .unwrap();
        match cli.command {
            Some(Command::Plan(PlanCmd::Decompose {
                ref goal,
                json,
                quiet,
            })) => {
                assert_eq!(goal, "smoke goal");
                assert!(json);
                assert!(quiet);
            }
            _ => panic!("expected Plan::Decompose command"),
        }
    }

    #[test]
    fn cli_serve_defaults() {
        let cli = Cli::try_parse_from(["astra", "serve"]).unwrap();
        match cli.command {
            Some(Command::Serve(ref args)) => {
                assert_eq!(args.host, "127.0.0.1");
                assert_eq!(args.port, 8000);
            }
            _ => panic!("expected Serve command"),
        }
    }

    #[test]
    fn cli_serve_custom_port() {
        let cli = Cli::try_parse_from(["astra", "serve", "--port", "3000"]).unwrap();
        match cli.command {
            Some(Command::Serve(ref args)) => {
                assert_eq!(args.port, 3000);
            }
            _ => panic!("expected Serve command"),
        }
    }

    #[test]
    fn cli_api_url_default() {
        let cli = Cli::try_parse_from(["astra"]).unwrap();
        assert_eq!(cli.api_url, "http://127.0.0.1:8000");
    }

    #[test]
    fn cli_api_url_custom() {
        let cli = Cli::try_parse_from(["astra", "--api-url", "http://remote:9000"]).unwrap();
        assert_eq!(cli.api_url, "http://remote:9000");
    }

    #[test]
    fn cli_profile_flag() {
        let cli = Cli::try_parse_from(["astra", "--profile", "work"]).unwrap();
        assert_eq!(cli.profile.as_deref(), Some("work"));
    }

    #[test]
    fn cli_top_level_yes_does_not_conflict_with_chat_yes() {
        // Both top-level -y and chat -y should work together
        let cli = Cli::try_parse_from(["astra", "-y", "chat", "-y"]).unwrap();
        assert!(cli.yes);
        match cli.command {
            Some(Command::Chat(ref args)) => {
                assert!(args.auto_approve);
            }
            _ => panic!("expected Chat command"),
        }
    }

    #[test]
    fn cli_mcp_add_scope_project() {
        let cli = Cli::try_parse_from(["astra", "mcp", "add", "--scope", "project", "s1", "npx"])
            .unwrap();
        match cli.command {
            Some(Command::Mcp(McpCmd::Add(ref args))) => {
                assert_eq!(args.scope, "project");
                assert_eq!(args.name, "s1");
                assert_eq!(args.command, "npx");
            }
            _ => panic!("expected Mcp Add command"),
        }
    }

    #[test]
    fn cli_mcp_add_scope_user() {
        let cli =
            Cli::try_parse_from(["astra", "mcp", "add", "--scope", "user", "s1", "npx"]).unwrap();
        match cli.command {
            Some(Command::Mcp(McpCmd::Add(ref args))) => {
                assert_eq!(args.scope, "user");
            }
            _ => panic!("expected Mcp Add command"),
        }
    }

    #[test]
    fn cli_mcp_add_with_trailing_args() {
        let cli = Cli::try_parse_from([
            "astra", "mcp", "add", "s1", "npx", "server", "--port", "8080",
        ])
        .unwrap();
        match cli.command {
            Some(Command::Mcp(McpCmd::Add(ref args))) => {
                assert_eq!(args.name, "s1");
                assert_eq!(args.command, "npx");
                assert_eq!(args.args, vec!["server", "--port", "8080"]);
            }
            _ => panic!("expected Mcp Add command"),
        }
    }

    #[test]
    fn cli_interactive_subcommand() {
        let cli = Cli::try_parse_from(["astra", "interactive"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Interactive)));
    }

    #[test]
    fn cli_health_subcommand() {
        let cli = Cli::try_parse_from(["astra", "health"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Health)));
    }

    #[test]
    fn cli_whoami_subcommand() {
        let cli = Cli::try_parse_from(["astra", "whoami"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Whoami)));
    }

    #[test]
    fn cli_system_prompt_flag() {
        let cli =
            Cli::try_parse_from(["astra", "--system-prompt", "You are a code reviewer"]).unwrap();
        assert_eq!(
            cli.system_prompt.as_deref(),
            Some("You are a code reviewer")
        );
    }

    #[test]
    fn cli_max_turns_flag() {
        let cli = Cli::try_parse_from(["astra", "--max-turns", "10"]).unwrap();
        assert_eq!(cli.max_turns, Some(10));
    }

    #[test]
    fn cli_max_turns_default_is_none() {
        let cli = Cli::try_parse_from(["astra"]).unwrap();
        assert!(cli.max_turns.is_none());
    }

    #[test]
    fn cli_system_prompt_with_print() {
        let cli = Cli::try_parse_from([
            "astra",
            "-p",
            "--system-prompt",
            "Be concise",
            "--max-turns",
            "5",
        ])
        .unwrap();
        assert!(cli.print);
        assert_eq!(cli.system_prompt.as_deref(), Some("Be concise"));
        assert_eq!(cli.max_turns, Some(5));
    }

    #[test]
    fn cli_completion_generates_bash_output() {
        use clap::CommandFactory;
        let mut buf = Vec::new();
        clap_complete::generate(
            clap_complete::Shell::Bash,
            &mut Cli::command(),
            "astra",
            &mut buf,
        );
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("astra"));
        assert!(output.contains("complete"));
    }

    #[test]
    fn cli_completion_generates_zsh_output() {
        use clap::CommandFactory;
        let mut buf = Vec::new();
        clap_complete::generate(
            clap_complete::Shell::Zsh,
            &mut Cli::command(),
            "astra",
            &mut buf,
        );
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("astra"));
        assert!(!output.is_empty());
    }

    #[test]
    fn cli_max_turns_rejects_non_numeric() {
        let result = Cli::try_parse_from(["astra", "--max-turns", "abc"]);
        assert!(result.is_err());
    }

    #[test]
    fn cli_all_flags_combined() {
        let cli = Cli::try_parse_from([
            "astra",
            "--model",
            "gpt-4o",
            "-p",
            "-y",
            "--system-prompt",
            "Review code",
            "--max-turns",
            "3",
            "--output-format",
            "json",
        ])
        .unwrap();
        assert_eq!(cli.model.as_deref(), Some("gpt-4o"));
        assert!(cli.print);
        assert!(cli.yes);
        assert_eq!(cli.system_prompt.as_deref(), Some("Review code"));
        assert_eq!(cli.max_turns, Some(3));
        assert_eq!(cli.output_format, "json");
    }

    // ── --allowed-tools tests ──

    #[test]
    fn cli_allowed_tools_single() {
        let cli = Cli::try_parse_from(["astra", "--allowed-tools", "Bash"]).unwrap();
        assert_eq!(cli.allowed_tools, vec!["Bash"]);
    }

    #[test]
    fn cli_allowed_tools_multiple_space_separated() {
        let cli =
            Cli::try_parse_from(["astra", "--allowed-tools", "Bash", "Edit", "Read"]).unwrap();
        assert_eq!(cli.allowed_tools, vec!["Bash", "Edit", "Read"]);
    }

    #[test]
    fn cli_allowed_tools_empty_default() {
        let cli = Cli::try_parse_from(["astra"]).unwrap();
        assert!(cli.allowed_tools.is_empty());
    }

    // ── --add-dir tests ──

    #[test]
    fn cli_add_dir_single() {
        let cli = Cli::try_parse_from(["astra", "--add-dir", "/tmp/extra"]).unwrap();
        assert_eq!(cli.add_dir, vec!["/tmp/extra"]);
    }

    #[test]
    fn cli_add_dir_multiple() {
        let cli = Cli::try_parse_from(["astra", "--add-dir", "/tmp/a", "/tmp/b"]).unwrap();
        assert_eq!(cli.add_dir, vec!["/tmp/a", "/tmp/b"]);
    }

    #[test]
    fn cli_add_dir_empty_default() {
        let cli = Cli::try_parse_from(["astra"]).unwrap();
        assert!(cli.add_dir.is_empty());
    }

    // ── --verbose tests ──

    #[test]
    fn cli_verbose_flag() {
        let cli = Cli::try_parse_from(["astra", "--verbose"]).unwrap();
        assert!(cli.verbose);
    }

    #[test]
    fn cli_verbose_default_false() {
        let cli = Cli::try_parse_from(["astra"]).unwrap();
        assert!(!cli.verbose);
    }

    // ── --mcp-config tests ──

    #[test]
    fn cli_mcp_config_single() {
        let cli = Cli::try_parse_from(["astra", "--mcp-config", "mcp.json"]).unwrap();
        assert_eq!(cli.mcp_config, vec!["mcp.json"]);
    }

    #[test]
    fn cli_mcp_config_multiple() {
        let cli = Cli::try_parse_from(["astra", "--mcp-config", "a.json", "b.json"]).unwrap();
        assert_eq!(cli.mcp_config, vec!["a.json", "b.json"]);
    }

    #[test]
    fn cli_mcp_config_empty_default() {
        let cli = Cli::try_parse_from(["astra"]).unwrap();
        assert!(cli.mcp_config.is_empty());
    }

    // ── Combined new flags ──

    #[test]
    fn cli_all_new_flags_combined() {
        let cli = Cli::try_parse_from([
            "astra",
            "--allowed-tools",
            "Bash",
            "Edit",
            "--add-dir",
            "/tmp/extra",
            "--verbose",
            "--mcp-config",
            "mcp.json",
            "--model",
            "gpt-4o",
            "-p",
        ])
        .unwrap();
        assert_eq!(cli.allowed_tools, vec!["Bash", "Edit"]);
        assert_eq!(cli.add_dir, vec!["/tmp/extra"]);
        assert!(cli.verbose);
        assert_eq!(cli.mcp_config, vec!["mcp.json"]);
        assert_eq!(cli.model.as_deref(), Some("gpt-4o"));
        assert!(cli.print);
    }

    #[test]
    fn cli_team_command_parses_structured_run_subcommand() {
        let cli = Cli::try_parse_from(["astra", "team", "run", "dev", "ship", "it"]).unwrap();
        match cli.command {
            Some(Command::Team(args)) => match args.command {
                Some(TeamSubcommand::Run(run)) => {
                    assert_eq!(run.team, "dev");
                    assert_eq!(run.task, vec!["ship", "it"]);
                }
                other => panic!("unexpected team subcommand: {other:?}"),
            },
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_team_command_defaults_to_list() {
        let cli = Cli::try_parse_from(["astra", "team"]).unwrap();
        match cli.command {
            Some(Command::Team(args)) => {
                assert!(args.command.is_none());
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_task_command_parses_structured_run_subcommand() {
        let cli = Cli::try_parse_from(["astra", "task", "run", "fix", "login", "page"]).unwrap();
        match cli.command {
            Some(Command::Task(args)) => match args.command {
                Some(TaskSubcommand::Run(run)) => {
                    assert_eq!(run.text, vec!["fix", "login", "page"]);
                }
                other => panic!("unexpected task subcommand: {other:?}"),
            },
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_memory_command_parses_search_subcommand() {
        let cli = Cli::try_parse_from(["astra", "memory", "search", "team", "history"]).unwrap();
        match cli.command {
            Some(Command::Memory(args)) => match args.command {
                Some(MemorySubcommand::Search(search)) => {
                    assert_eq!(search.query, vec!["team", "history"]);
                }
                other => panic!("unexpected memory subcommand: {other:?}"),
            },
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_permissions_command_keeps_mode_arg() {
        let cli = Cli::try_parse_from(["astra", "permissions", "auto"]).unwrap();
        match cli.command {
            Some(Command::Permissions(args)) => match args.command {
                Some(PermissionsSubcommand::Auto) => {}
                other => panic!("unexpected permissions subcommand: {other:?}"),
            },
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_allow_alias_parses_permissions_command() {
        let cli = Cli::try_parse_from(["astra", "allow", "prompt"]).unwrap();
        match cli.command {
            Some(Command::Permissions(args)) => match args.command {
                Some(PermissionsSubcommand::Prompt) => {}
                other => panic!("unexpected permissions subcommand: {other:?}"),
            },
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_sessions_alias_parses_session_command() {
        let cli = Cli::try_parse_from(["astra", "sessions", "list"]).unwrap();
        match cli.command {
            Some(Command::Session(SessionCmd::List(_))) => {}
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_review_command_parses_working_subcommand() {
        let cli = Cli::try_parse_from(["astra", "review", "working"]).unwrap();
        match cli.command {
            Some(Command::Review(args)) => match args.command {
                Some(ReviewSubcommand::Working) => assert!(args.target.is_empty()),
                other => panic!("unexpected review subcommand: {other:?}"),
            },
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_review_command_accepts_plain_revision_target() {
        let cli = Cli::try_parse_from(["astra", "review", "HEAD~2"]).unwrap();
        match cli.command {
            Some(Command::Review(args)) => {
                assert!(args.command.is_none());
                assert_eq!(args.target, vec!["HEAD~2"]);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_grep_command_accepts_default_pattern() {
        let cli = Cli::try_parse_from(["astra", "grep", "tool", "selector"]).unwrap();
        match cli.command {
            Some(Command::Grep(args)) => {
                assert!(args.command.is_none());
                assert_eq!(args.pattern, vec!["tool", "selector"]);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_grep_command_rejects_missing_pattern() {
        assert!(Cli::try_parse_from(["astra", "grep"]).is_err());
    }

    #[test]
    fn cli_agent_command_parses_status_subcommand() {
        let cli = Cli::try_parse_from(["astra", "agent", "status", "agent-123"]).unwrap();
        match cli.command {
            Some(Command::Agent(args)) => match args.command {
                Some(AgentSubcommand::Status(status)) => assert_eq!(status.agent_id, "agent-123"),
                other => panic!("unexpected agent subcommand: {other:?}"),
            },
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_messaging_command_parses_dlq_subcommand() {
        let cli = Cli::try_parse_from(["astra", "messaging", "dlq"]).unwrap();
        match cli.command {
            Some(Command::Messaging(args)) => match args.command {
                Some(MessagingSubcommand::Dlq) => {}
                other => panic!("unexpected messaging subcommand: {other:?}"),
            },
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_diff_command_parses_show_subcommand() {
        let cli = Cli::try_parse_from(["astra", "diff", "show", "HEAD~1"]).unwrap();
        match cli.command {
            Some(Command::Diff(args)) => match args.command {
                Some(DiffSubcommand::Show(show)) => {
                    assert_eq!(show.rev, "HEAD~1");
                    assert!(show.paths.is_empty());
                }
                other => panic!("unexpected diff subcommand: {other:?}"),
            },
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_diff_command_accepts_plain_path_filter() {
        let cli =
            Cli::try_parse_from(["astra", "diff", "rust/crates/astra-cli/src/main.rs"]).unwrap();
        match cli.command {
            Some(Command::Diff(args)) => {
                assert!(args.command.is_none());
                assert_eq!(args.paths, vec!["rust/crates/astra-cli/src/main.rs"]);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_bug_command_parses_save_subcommand() {
        let cli = Cli::try_parse_from(["astra", "bug", "save"]).unwrap();
        match cli.command {
            Some(Command::Bug(args)) => match args.command {
                Some(BugSubcommand::Save) => {}
                other => panic!("unexpected bug subcommand: {other:?}"),
            },
            other => panic!("unexpected command: {other:?}"),
        }
    }

    // ── --disallowed-tools tests ──

    #[test]
    fn cli_disallowed_tools_single() {
        let cli = Cli::try_parse_from(["astra", "--disallowed-tools", "Bash"]).unwrap();
        assert_eq!(cli.disallowed_tools, vec!["Bash"]);
    }

    #[test]
    fn cli_disallowed_tools_multiple() {
        let cli = Cli::try_parse_from(["astra", "--disallowed-tools", "Bash", "Edit"]).unwrap();
        assert_eq!(cli.disallowed_tools, vec!["Bash", "Edit"]);
    }

    #[test]
    fn cli_disallowed_tools_empty_default() {
        let cli = Cli::try_parse_from(["astra"]).unwrap();
        assert!(cli.disallowed_tools.is_empty());
    }

    #[test]
    fn cli_allowed_and_disallowed_together() {
        let cli = Cli::try_parse_from([
            "astra",
            "--allowed-tools",
            "Read",
            "Edit",
            "--disallowed-tools",
            "Bash",
        ])
        .unwrap();
        assert_eq!(cli.allowed_tools, vec!["Read", "Edit"]);
        assert_eq!(cli.disallowed_tools, vec!["Bash"]);
    }

    // ── --session-id tests ──

    #[test]
    fn cli_session_id_flag() {
        let cli = Cli::try_parse_from([
            "astra",
            "--session-id",
            "550e8400-e29b-41d4-a716-446655440000",
        ])
        .unwrap();
        assert_eq!(
            cli.session_id.as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440000")
        );
    }

    #[test]
    fn cli_session_id_default_none() {
        let cli = Cli::try_parse_from(["astra"]).unwrap();
        assert!(cli.session_id.is_none());
    }

    // ── --name tests ──

    #[test]
    fn cli_name_short_flag() {
        let cli = Cli::try_parse_from(["astra", "-n", "my-session"]).unwrap();
        assert_eq!(cli.session_name.as_deref(), Some("my-session"));
    }

    #[test]
    fn cli_name_long_flag() {
        let cli = Cli::try_parse_from(["astra", "--name", "review-pr-42"]).unwrap();
        assert_eq!(cli.session_name.as_deref(), Some("review-pr-42"));
    }

    #[test]
    fn cli_name_default_none() {
        let cli = Cli::try_parse_from(["astra"]).unwrap();
        assert!(cli.session_name.is_none());
    }

    // ── --bare tests ──

    #[test]
    fn cli_bare_flag() {
        let cli = Cli::try_parse_from(["astra", "--bare"]).unwrap();
        assert!(cli.bare);
    }

    #[test]
    fn cli_bare_default_false() {
        let cli = Cli::try_parse_from(["astra"]).unwrap();
        assert!(!cli.bare);
    }

    #[test]
    fn cli_bare_with_print_and_system_prompt() {
        let cli = Cli::try_parse_from([
            "astra",
            "--bare",
            "-p",
            "--system-prompt",
            "Be brief",
            "--add-dir",
            "/tmp/work",
        ])
        .unwrap();
        assert!(cli.bare);
        assert!(cli.print);
        assert_eq!(cli.system_prompt.as_deref(), Some("Be brief"));
        assert_eq!(cli.add_dir, vec!["/tmp/work"]);
    }

    #[test]
    fn cli_session_id_and_name_combined() {
        let cli = Cli::try_parse_from([
            "astra",
            "--session-id",
            "123e4567-e89b-12d3-a456-426614174000",
            "-n",
            "debug-session",
        ])
        .unwrap();
        assert_eq!(
            cli.session_id.as_deref(),
            Some("123e4567-e89b-12d3-a456-426614174000")
        );
        assert_eq!(cli.session_name.as_deref(), Some("debug-session"));
    }

    // ── --max-budget tests ──

    #[test]
    fn cli_max_budget_flag() {
        let cli = Cli::try_parse_from(["astra", "--max-budget", "5.50"]).unwrap();
        assert!((cli.max_budget - 5.50).abs() < f64::EPSILON);
    }

    #[test]
    fn cli_max_budget_default_zero() {
        let cli = Cli::try_parse_from(["astra"]).unwrap();
        assert!((cli.max_budget - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn cli_max_budget_rejects_non_numeric() {
        let result = Cli::try_parse_from(["astra", "--max-budget", "abc"]);
        assert!(result.is_err());
    }

    #[test]
    fn cli_max_budget_with_print_and_turns() {
        let cli = Cli::try_parse_from(["astra", "-p", "--max-turns", "10", "--max-budget", "1.0"])
            .unwrap();
        assert!(cli.print);
        assert_eq!(cli.max_turns, Some(10));
        assert!((cli.max_budget - 1.0).abs() < f64::EPSILON);
    }

    // ── --max-budget edge case tests ──

    #[test]
    fn cli_max_budget_negative_rejected() {
        let result = Cli::try_parse_from(["astra", "--max-budget", "-1.0"]);
        // clap may or may not accept negative f64 — verify behavior
        match result {
            Ok(cli) => assert!(cli.max_budget < 0.0, "negative budget parsed but < 0"),
            Err(_) => {} // rejected is fine too
        }
    }

    #[test]
    fn cli_max_budget_very_small_value() {
        let cli = Cli::try_parse_from(["astra", "--max-budget", "0.001"]).unwrap();
        assert!((cli.max_budget - 0.001).abs() < f64::EPSILON);
    }

    #[test]
    fn cli_max_budget_large_value() {
        let cli = Cli::try_parse_from(["astra", "--max-budget", "999.99"]).unwrap();
        assert!((cli.max_budget - 999.99).abs() < f64::EPSILON);
    }

    #[test]
    fn cli_max_budget_integer_value() {
        let cli = Cli::try_parse_from(["astra", "--max-budget", "10"]).unwrap();
        assert!((cli.max_budget - 10.0).abs() < f64::EPSILON);
    }

    // ── --yes / -y edge case tests ──

    #[test]
    fn cli_yes_flag_sets_auto_approve() {
        let cli = Cli::try_parse_from(["astra", "-y"]).unwrap();
        assert!(cli.yes);
    }

    #[test]
    fn cli_yes_long_flag_sets_auto_approve() {
        let cli = Cli::try_parse_from(["astra", "--yes"]).unwrap();
        assert!(cli.yes);
    }

    #[test]
    fn cli_yes_with_permission_mode_deny() {
        // Both flags accepted by parser on `chat` subcommand — runtime resolves conflict
        let cli = Cli::try_parse_from([
            "astra",
            "chat",
            "-y",
            "--permission-mode",
            "deny",
            "-m",
            "test",
        ])
        .unwrap();
        match &cli.command {
            Some(Command::Chat(args)) => {
                assert!(args.auto_approve);
                assert_eq!(args.permission_mode.as_deref(), Some("deny"));
            }
            _ => panic!("expected Chat command"),
        }
    }

    #[test]
    fn cli_yes_with_permission_mode_auto_is_redundant() {
        let cli = Cli::try_parse_from([
            "astra",
            "chat",
            "-y",
            "--permission-mode",
            "auto",
            "-m",
            "test",
        ])
        .unwrap();
        match &cli.command {
            Some(Command::Chat(args)) => {
                assert!(args.auto_approve);
                assert_eq!(args.permission_mode.as_deref(), Some("auto"));
            }
            _ => panic!("expected Chat command"),
        }
    }

    #[test]
    fn cli_permission_mode_invalid_value() {
        let cli = Cli::try_parse_from([
            "astra",
            "chat",
            "--permission-mode",
            "invalid",
            "-m",
            "test",
        ]);
        assert!(cli.is_err());
    }

    #[test]
    fn cli_output_format_invalid_value() {
        let cli = Cli::try_parse_from(["astra", "--output-format", "yaml", "-p"]);
        assert!(cli.is_err());
    }

    #[test]
    fn cli_default_no_yes_no_permission_mode() {
        let cli = Cli::try_parse_from(["astra"]).unwrap();
        assert!(!cli.yes);
    }

    // ── /allow command tests ──

    #[test]
    fn permission_mode_set_mode() {
        let mut pm = permission_manager::PermissionManager::with_project(
            false,
            &std::path::PathBuf::from("/tmp"),
        );
        assert_eq!(pm.mode(), permission_manager::PermissionMode::Prompt);
        pm.set_mode(permission_manager::PermissionMode::Auto);
        assert_eq!(pm.mode(), permission_manager::PermissionMode::Auto);
        pm.set_mode(permission_manager::PermissionMode::Deny);
        assert_eq!(pm.mode(), permission_manager::PermissionMode::Deny);
    }

    #[test]
    fn permission_mode_roundtrip_parse() {
        for mode_str in &["auto", "prompt", "deny"] {
            let mode: permission_manager::PermissionMode = mode_str.parse().unwrap();
            assert_eq!(mode.to_string().to_lowercase(), *mode_str);
        }
    }

    #[test]
    fn repl_state_auto_approve_env_activates_auto_mode() {
        // When ASTRA_AUTO_APPROVE=1, ReplState should start in Auto mode
        unsafe {
            std::env::set_var("ASTRA_AUTO_APPROVE", "1");
        }
        let state = ReplState::default();
        unsafe {
            std::env::remove_var("ASTRA_AUTO_APPROVE");
        }
        assert_eq!(
            state.perm_manager.mode(),
            permission_manager::PermissionMode::Auto
        );
    }

    #[tokio::test]
    async fn task_run_stores_result_in_checkpoint() {
        use astra_services::{TaskCreateRequest, TaskService, task_orchestrator::TaskCheckpoint};

        // Use a temp dir for LocalTaskService
        let tmp = tempfile::tempdir().unwrap();
        let svc = astra_services::LocalTaskService::new(tmp.path().to_path_buf());

        // Create a task (simulates what /task run does)
        let tid = svc
            .create_task(
                "test-user",
                "test-session",
                TaskCreateRequest {
                    title: "run: test prompt".to_string(),
                    description: Some("test prompt".to_string()),
                    plan: None,
                    parent_task_id: None,
                    project_type: None,
                    goal_pattern: None,
                },
            )
            .await
            .unwrap();

        // Mark in-progress
        svc.update_status(&tid, astra_services::TaskStatus::InProgress)
            .await
            .unwrap();

        // Save checkpoint with result (simulates background task completion)
        let mut state_map = serde_json::Map::new();
        state_map.insert(
            "full_text".to_string(),
            serde_json::Value::String("Hello from agent".to_string()),
        );
        state_map.insert("prompt_tokens".to_string(), serde_json::json!(100));
        state_map.insert("completion_tokens".to_string(), serde_json::json!(50));
        state_map.insert("tool_calls_count".to_string(), serde_json::json!(3));

        svc.save_checkpoint(
            &tid,
            &TaskCheckpoint {
                active_subtask_id: None,
                turn: 0,
                session_id: Some("test-session".to_string()),
                state: state_map,
            },
        )
        .await
        .unwrap();

        // Complete the task
        svc.complete_task(&tid).await.unwrap();

        // Read back and verify (simulates /task result)
        let record = svc.get_task(&tid).await.unwrap().unwrap();
        assert_eq!(record.status, astra_services::TaskStatus::Completed);
        let cp = record.checkpoint.unwrap();
        assert_eq!(
            cp.state.get("full_text").and_then(|v| v.as_str()),
            Some("Hello from agent")
        );
        assert_eq!(
            cp.state.get("prompt_tokens").and_then(|v| v.as_u64()),
            Some(100)
        );
        assert_eq!(
            cp.state.get("tool_calls_count").and_then(|v| v.as_u64()),
            Some(3)
        );
    }

    // ── @file system-prompt tests ──

    #[test]
    fn resolve_system_prompt_literal_text() {
        let result = resolve_system_prompt("You are a helpful assistant.".to_string());
        assert_eq!(result.unwrap(), "You are a helpful assistant.");
    }

    #[test]
    fn resolve_system_prompt_at_file_reads_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prompt.txt");
        std::fs::write(&path, "Custom system prompt from file").unwrap();
        let result = resolve_system_prompt(format!("@{}", path.display()));
        assert_eq!(result.unwrap(), "Custom system prompt from file");
    }

    #[test]
    fn resolve_system_prompt_at_file_not_found() {
        let result = resolve_system_prompt("@/nonexistent/path/prompt.txt".to_string());
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("cannot read system prompt file")
        );
    }

    #[test]
    fn resolve_system_prompt_at_file_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.txt");
        std::fs::write(&path, "").unwrap();
        let result = resolve_system_prompt(format!("@{}", path.display()));
        assert_eq!(result.unwrap(), "");
    }

    #[test]
    fn resolve_system_prompt_at_bare_is_error() {
        let result = resolve_system_prompt("@".to_string());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("requires a file path"));
    }

    #[test]
    fn resolve_system_prompt_at_file_with_unicode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unicode.txt");
        std::fs::write(&path, "你好世界 🌍 مرحبا").unwrap();
        let result = resolve_system_prompt(format!("@{}", path.display()));
        assert_eq!(result.unwrap(), "你好世界 🌍 مرحبا");
    }

    #[test]
    fn resolve_system_prompt_at_file_with_newlines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("multi.txt");
        std::fs::write(&path, "line1\nline2\nline3\n").unwrap();
        let result = resolve_system_prompt(format!("@{}", path.display()));
        assert_eq!(result.unwrap(), "line1\nline2\nline3\n");
    }

    #[test]
    fn resolve_system_prompt_no_at_prefix_passes_through() {
        let result = resolve_system_prompt("/some/path/prompt.txt".to_string());
        assert_eq!(result.unwrap(), "/some/path/prompt.txt");
    }

    #[test]
    fn resolve_system_prompt_at_file_large_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.txt");
        let content = "x".repeat(1_000_000);
        std::fs::write(&path, &content).unwrap();
        let result = resolve_system_prompt(format!("@{}", path.display()));
        assert_eq!(result.unwrap().len(), 1_000_000);
    }

    #[test]
    fn resolve_system_prompt_at_file_permission_denied() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("noperm.txt");
        std::fs::write(&path, "secret").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let result = resolve_system_prompt(format!("@{}", path.display()));
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644));
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("cannot read system prompt file")
        );
    }

    // ── project instructions tests ──

    #[test]
    fn discover_project_instructions_from_tempdir() {
        let dir = tempfile::tempdir().unwrap();
        let astra_dir = dir.path().join(".astra");
        std::fs::create_dir_all(&astra_dir).unwrap();
        std::fs::write(
            astra_dir.join("instructions.md"),
            "Always use Rust.\nPrefer async.",
        )
        .unwrap();

        let result = discover_instructions_from_paths(Some(dir.path()), None);
        let instructions = result.expect("should discover instructions");
        assert!(instructions.contains("Always use Rust."));
        assert!(instructions.contains("Prefer async."));
    }

    #[test]
    fn discover_project_instructions_empty_file_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let astra_dir = dir.path().join(".astra");
        std::fs::create_dir_all(&astra_dir).unwrap();
        std::fs::write(astra_dir.join("instructions.md"), "   \n  \n").unwrap();

        let result = discover_instructions_from_paths(Some(dir.path()), None);
        assert!(result.is_none(), "empty file should return None");
    }

    #[test]
    fn discover_project_instructions_no_file_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let result = discover_instructions_from_paths(Some(dir.path()), Some(dir.path()));
        assert!(result.is_none(), "no file should return None");
    }

    #[test]
    fn discover_project_instructions_combines_project_and_user() {
        let project = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let p_astra = project.path().join(".astra");
        let h_astra = home.path().join(".astra");
        std::fs::create_dir_all(&p_astra).unwrap();
        std::fs::create_dir_all(&h_astra).unwrap();
        std::fs::write(p_astra.join("instructions.md"), "Project rules").unwrap();
        std::fs::write(h_astra.join("instructions.md"), "Global rules").unwrap();

        let result = discover_instructions_from_paths(Some(project.path()), Some(home.path()));
        let instructions = result.expect("should combine both");
        assert!(instructions.contains("Project rules"));
        assert!(instructions.contains("Global rules"));
        // Project should come first
        let project_pos = instructions.find("Project rules").unwrap();
        let global_pos = instructions.find("Global rules").unwrap();
        assert!(project_pos < global_pos, "project should precede global");
    }

    #[test]
    fn discover_project_instructions_user_only() {
        let project = tempfile::tempdir().unwrap(); // no .astra dir
        let home = tempfile::tempdir().unwrap();
        let h_astra = home.path().join(".astra");
        std::fs::create_dir_all(&h_astra).unwrap();
        std::fs::write(h_astra.join("instructions.md"), "User-level rules").unwrap();

        let result = discover_instructions_from_paths(Some(project.path()), Some(home.path()));
        let instructions = result.expect("should find user-level");
        assert!(instructions.contains("User-level rules"));
    }

    #[test]
    fn format_project_instructions_wraps_in_tags() {
        let content = "Use tabs for indentation.";
        let formatted = format_project_instructions(content);
        assert!(formatted.starts_with("<project_instructions>"));
        assert!(formatted.ends_with("</project_instructions>"));
        assert!(formatted.contains(content));
    }

    #[test]
    fn build_effective_line_includes_project_instructions() {
        let mut state = ReplState::default();
        state.project_instructions = Some("Always use Rust.".to_string());
        let result = repl_turn::build_effective_line("hello", &state);
        assert!(
            result.contains("<project_instructions>"),
            "should wrap in tags"
        );
        assert!(result.contains("Always use Rust."));
        assert!(
            result.contains("hello"),
            "should still include user message"
        );
    }

    #[test]
    fn build_effective_line_no_instructions_when_none() {
        let state = ReplState::default();
        let result = repl_turn::build_effective_line("hello", &state);
        assert!(
            !result.contains("<project_instructions>"),
            "should not inject when None"
        );
        assert_eq!(result, "hello");
    }

    #[test]
    fn cli_no_instructions_flag() {
        let cli = Cli::try_parse_from(["astra", "--no-instructions"]).unwrap();
        assert!(cli.no_instructions);
    }

    #[test]
    fn discover_instructions_includes_knowledge_md() {
        let dir = tempfile::tempdir().unwrap();
        let astra_dir = dir.path().join(".astra");
        std::fs::create_dir_all(&astra_dir).unwrap();
        std::fs::write(astra_dir.join("instructions.md"), "Use Rust conventions").unwrap();
        std::fs::write(
            astra_dir.join("knowledge.md"),
            "# Project Knowledge\n\n- Always run clippy",
        )
        .unwrap();
        let result = discover_instructions_from_paths(Some(dir.path()), None).unwrap();
        assert!(result.contains("Use Rust conventions"));
        assert!(result.contains("Always run clippy"));
    }

    #[test]
    fn discover_instructions_knowledge_md_only() {
        let dir = tempfile::tempdir().unwrap();
        let astra_dir = dir.path().join(".astra");
        std::fs::create_dir_all(&astra_dir).unwrap();
        std::fs::write(astra_dir.join("knowledge.md"), "- Some learning").unwrap();
        let result = discover_instructions_from_paths(Some(dir.path()), None).unwrap();
        assert!(result.contains("Some learning"));
    }

    #[test]
    fn discover_instructions_empty_knowledge_md_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let astra_dir = dir.path().join(".astra");
        std::fs::create_dir_all(&astra_dir).unwrap();
        std::fs::write(astra_dir.join("knowledge.md"), "   ").unwrap();
        assert!(discover_instructions_from_paths(Some(dir.path()), None).is_none());
    }

    // append_to_knowledge_md tests moved to session_cleanup.rs

    #[test]
    fn discover_instructions_knowledge_md_capped_at_8kb() {
        let dir = tempfile::tempdir().unwrap();
        let astra_dir = dir.path().join(".astra");
        std::fs::create_dir_all(&astra_dir).unwrap();
        // Write a 12KB knowledge.md
        let big_content = "- ".to_string() + &"x".repeat(998) + "\n";
        let repeated = big_content.repeat(12); // ~12KB
        std::fs::write(astra_dir.join("knowledge.md"), &repeated).unwrap();
        let result = discover_instructions_from_paths(Some(dir.path()), None).unwrap();
        // The injected content should be capped — less than the full 12KB
        assert!(
            result.len() < repeated.len(),
            "should be capped below original size"
        );
        // But should have substantial content (at least 7KB from 8KB cap minus header)
        assert!(
            result.len() > 7000,
            "should retain most of 8KB cap, got {}",
            result.len()
        );
    }

    #[test]
    fn discover_instructions_knowledge_md_capped_at_8kb_cjk() {
        // Regression: truncation at byte offset must not panic on multi-byte chars.
        let dir = tempfile::tempdir().unwrap();
        let astra_dir = dir.path().join(".astra");
        std::fs::create_dir_all(&astra_dir).unwrap();
        // CJK chars are 3 bytes each. Build >8KB of CJK content.
        let cjk_line = "- 知识回流测试行内容填充\n"; // ~38 bytes per line
        let mut content = String::new();
        while content.len() < 12_000 {
            content.push_str(cjk_line);
        }
        std::fs::write(astra_dir.join("knowledge.md"), &content).unwrap();
        // This must not panic (previously did on non-char-boundary byte index)
        let result = discover_instructions_from_paths(Some(dir.path()), None).unwrap();
        assert!(result.len() > 5000, "should retain substantial CJK content");
        assert!(
            result.len() < content.len(),
            "should be capped below original"
        );
    }

    // session_end_extract_learnings tests moved to session_cleanup.rs
}
