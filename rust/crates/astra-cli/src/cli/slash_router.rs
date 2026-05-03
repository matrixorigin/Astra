//! Slash command routing for the REPL.

use std::io::IsTerminal;

use super::*;
use crate::command_usage;

/// GET `/models` returns `ModelListItemResponse` with field `is_active` (snake_case).
/// Accept legacy `active` if present; if neither is a bool, treat as active for unknown servers.
fn model_list_entry_is_active(entry: &serde_json::Value) -> bool {
    if let Some(v) = entry.get("is_active") {
        if let Some(b) = v.as_bool() {
            return b;
        }
        // Some gateways / hand-written JSON use 0/1 instead of booleans.
        if let Some(n) = v.as_i64() {
            return n != 0;
        }
        if let Some(n) = v.as_u64() {
            return n != 0;
        }
    }
    if let Some(b) = entry.get("active").and_then(|v| v.as_bool()) {
        return b;
    }
    if let Some(n) = entry.get("active").and_then(|v| v.as_i64()) {
        return n != 0;
    }
    true
}

fn model_list_entry_name(entry: &serde_json::Value) -> Option<&str> {
    entry
        .get("name")
        .or_else(|| entry.get("model_name"))
        .and_then(|v| v.as_str())
}

fn model_list_entry_thinking_mode(entry: &serde_json::Value) -> Option<&str> {
    // Flattened `thinking_mode` is the canonical shape emitted by the current
    // /models response. The `quirks.thinking_mode` branch is a transitional
    // fallback for older server builds still nesting the value inside quirks.
    // TODO: remove the nested-quirks fallback once all deployments are past
    // the flattened-response rollout.
    entry
        .get("thinking_mode")
        .and_then(|v| v.as_str())
        .or_else(|| {
            entry
                .get("quirks")
                .and_then(|q| q.get("thinking_mode"))
                .and_then(|v| v.as_str())
        })
}

fn model_list_entry_provider(entry: &serde_json::Value) -> Option<&str> {
    entry.get("provider").and_then(|v| v.as_str())
}

fn find_model_list_entry<'a>(
    models: &'a [serde_json::Value],
    name: &str,
) -> Option<&'a serde_json::Value> {
    models
        .iter()
        .find(|m| model_list_entry_name(m).is_some_and(|n| n.eq_ignore_ascii_case(name)))
}

/// Returns `true` when the REPL should exit.
pub(super) async fn handle_slash_command(
    line: &str,
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    state: &mut ReplState,
    token: Option<&str>,
    selector: &dyn tool_selector::ToolSelector,
) -> Result<bool, String> {
    clear_slash_overlay();

    let mut parts = line.splitn(2, ' ');
    let raw_cmd = parts.next().unwrap_or("");
    let arg = parts.next().unwrap_or("").trim();
    let cmd = match resolve_slash_command(raw_cmd) {
        Ok(command) => command,
        Err(candidates) if candidates.is_empty() => {
            let suggestions = suggest_commands(raw_cmd, 3);
            if suggestions.is_empty() {
                eprintln!(
                    "{}",
                    format!("  Unknown command: {}  (type /help for list)", raw_cmd).yellow()
                );
            } else {
                let hint = suggestions.join(", ");
                eprintln!(
                    "{}",
                    format!("  Unknown command: {}  (did you mean: {}?)", raw_cmd, hint).yellow()
                );
            }
            return Ok(false);
        }
        Err(candidates) if candidates.len() == 1 => candidates[0],
        Err(candidates) => {
            let preview: Vec<&str> = candidates.iter().take(5).copied().collect();
            eprintln!(
                "{}",
                format!("  Ambiguous: {}  — type more to narrow", preview.join(", ")).yellow()
            );
            return Ok(false);
        }
    };

    if cmd == "/" && arg.is_empty() && is_slash_picker_active() {
        return Ok(false);
    }

    if let Err(err) = command_usage::record_command_use(cmd) {
        eprintln!(
            "{}",
            format!("  Warning: failed to update command discovery history: {err}").yellow()
        );
    }

    match cmd {
        "/" | "/?" | "/commands" | "/help" => {
            if arg.trim() == "keys" {
                print_keyboard_shortcuts()
            } else {
                print_slash_commands(Some(arg))
            }
        }

        "/model" if arg.is_empty() => {
            let Some(tok) = token else {
                eprintln!("{}", "  Not logged in. Use /login.".yellow());
                return Ok(false);
            };
            let body = api.get_models_text(tok).await.map_err(map_thin_err)?;
            {
                let value: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
                let models = value
                    .as_array()
                    .cloned()
                    .or_else(|| value.get("models").and_then(|v| v.as_array()).cloned())
                    .unwrap_or_default();

                let items: Vec<(String, String)> = models
                    .iter()
                    .filter_map(|m| {
                        let name = model_list_entry_name(m)?;
                        if !model_list_entry_is_active(m) {
                            return None;
                        }
                        let desc = m
                            .get("description")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        Some((name.to_string(), desc))
                    })
                    .collect();

                if let Some(chosen) = interactive_select(
                    "Select model (type to search):",
                    &items,
                    state.model.as_deref(),
                ) {
                    // Two-level selection: if model supports thinking, prompt for mode
                    let selected_model = find_model_list_entry(&models, &chosen);
                    if selected_model.is_none() {
                        tracing::warn!(
                            model = %chosen,
                            "selected model not found in model list — \
                             thinking-mode detection unavailable, falling back to defaults"
                        );
                        if std::io::stderr().is_terminal() {
                            eprintln!(
                                "  ⚠ Unknown model '{}' — thinking mode detection unavailable.",
                                chosen
                            );
                        }
                    }
                    let thinking_mode = selected_model.and_then(model_list_entry_thinking_mode);
                    let provider = selected_model.and_then(model_list_entry_provider);
                    let opts = astra_turn_core::thinking_config::thinking_options_with_capability(
                        &chosen,
                        provider,
                        thinking_mode,
                    );
                    let model_with_suffix = if opts.is_empty() {
                        chosen.clone()
                    } else {
                        let thinking_items: Vec<(String, String)> = opts
                            .iter()
                            .map(|o| {
                                let marker = if o.is_default { " (default)" } else { "" };
                                (o.label.to_string(), marker.to_string())
                            })
                            .collect();
                        let default_idx = opts.iter().position(|o| o.is_default);
                        let default_label = default_idx.map(|i| opts[i].label);
                        if let Some(picked) = interactive_select(
                            "Select thinking mode:",
                            &thinking_items,
                            default_label,
                        ) {
                            let selected_opt = opts.iter().find(|o| o.label == picked);
                            let suffix = selected_opt
                                .map(|o| {
                                    astra_turn_core::thinking_config::thinking_suffix_for(&o.config)
                                })
                                .unwrap_or_default();
                            format!("{chosen}{suffix}")
                        } else {
                            // Cancelled thinking selection — use model without thinking
                            eprintln!("{}", "  Thinking mode: Normal".dim());
                            chosen.clone()
                        }
                    };

                    state.model = Some(model_with_suffix.clone());
                    state.cached_pricing = slash_stats::extract_pricing_for_model(&models, &chosen)
                        .unwrap_or_else(|| slash_stats::fallback_pricing(&chosen));
                    state.context_budget = prompts::ContextBudget::from_runtime_config(
                        &state.runtime_config,
                        Some(&chosen),
                    );
                    eprintln!(
                        "  {} {}",
                        theme::icon_ok(),
                        format!("Model set to: {model_with_suffix}").green()
                    );
                } else {
                    eprintln!("{}", "  Cancelled.".dim());
                }
            }
        }

        "/model" => {
            if let Some(tok) = token {
                match api.get_models_text(tok).await {
                    Ok(body) => {
                        let value: serde_json::Value =
                            serde_json::from_str(&body).unwrap_or_default();
                        let models = value
                            .as_array()
                            .cloned()
                            .or_else(|| value.get("models").and_then(|v| v.as_array()).cloned())
                            .unwrap_or_default();

                        if let Some(entry) = find_model_list_entry(&models, arg) {
                            if !model_list_entry_is_active(entry) {
                                eprintln!(
                                    "{}",
                                    format!(
                                        "  Model '{}' is registered but inactive (server will not use it). \
                                         Activate it in the admin UI or fix connectivity, then retry.",
                                        arg
                                    )
                                    .yellow()
                                );
                                return Ok(false);
                            }
                        }

                        let available: Vec<String> = models
                            .iter()
                            .filter_map(|m| {
                                let name = model_list_entry_name(m)?;
                                if model_list_entry_is_active(m) {
                                    Some(name.to_string())
                                } else {
                                    None
                                }
                            })
                            .collect();

                        let model_exists = available.iter().any(|m| m.eq_ignore_ascii_case(arg));

                        if !model_exists && !available.is_empty() {
                            let suggestions = cli_output::suggest_models(arg, &available);
                            let refs: Vec<&str> = suggestions.iter().map(|s| s.as_str()).collect();
                            cli_output::format_not_found_error(
                                "Model",
                                arg,
                                &refs,
                                Some("/model to see available models"),
                            );
                            return Ok(false);
                        }

                        if !model_exists && available.is_empty() && !models.is_empty() {
                            eprintln!(
                                "{}",
                                "  No active models returned by the server. \
                                 Add or activate a model (admin), or run a connectivity check."
                                    .yellow()
                            );
                            return Ok(false);
                        }
                    }
                    Err(_) => {}
                }
            }

            state.model = Some(arg.to_string());
            let base_model = astra_turn_core::thinking_config::resolve_model_thinking(arg).0;
            state.cached_pricing = slash_stats::fallback_pricing(base_model);
            state.context_budget = prompts::ContextBudget::from_runtime_config(
                &state.runtime_config,
                Some(base_model),
            );
            eprintln!("{}", format!("  \u{2713}  Model set to: {}", arg).green());
            if let Some(ref j) = state.journal {
                let _ = j.append(&session_journal::JournalEvent::config_change(
                    state.session_id.as_deref(),
                    "model",
                    arg,
                ));
            }
        }

        "/session" => handle_session_command(arg, api, profile, state, token).await,

        "/config" => slash_config::handle_config_command(arg),

        "/checkpoint" => match create_manual_repl_checkpoint(state, arg) {
            Ok(summary) => {
                eprintln!("  {} {}", theme::icon_ok(), summary.headline().green());
                eprintln!(
                    "    {}",
                    format!("session: {}", summary.checkpoint_path.display()).dim()
                );
                eprintln!(
                    "    {}",
                    format!("heavy:   {}", summary.heavy_path.display()).dim()
                );
                if summary.cloud_sync_queued {
                    eprintln!("    {}", theme::warning("Cloud sync queued in background."));
                }
            }
            Err(e) => {
                eprintln!("  {}", e.yellow());
            }
        },

        "/debug" => handle_debug_command(arg, state),

        "/inspect" => {
            slash_inspect::handle_inspect_command(arg, state);
        }

        "/style" => {
            slash_style::handle_style_command(arg);
        }

        "/history" | "/grep" | "/review" | "/copy" | "/diagnostics" | "/lsp" | "/context"
        | "/version" | "/whoami" | "/rewind" | "/turn" | "/report" => {
            handle_info_command(cmd, arg, api, state, profile, token).await?;
        }

        "/skill" => {
            handle_skill_command(arg, api, state, profile, token).await?;
        }

        "/mcp" => {
            slash_mcp::handle_mcp_command(arg, state).await?;
        }

        "/team" => {
            slash_team::handle_team_command(arg, api, profile, state).await;
        }

        "/telemetry" => {
            slash_telemetry::handle_telemetry_command(arg, state);
        }

        "/messaging" => {
            handle_messaging_command(arg, state);
        }

        "/agent" => {
            let ctx = slash_agent::AgentCommandContext {
                spawner: state.agent_spawner.clone(),
                session_id: state.session_id.clone(),
            };
            slash_agent::handle_agent_command(arg, &ctx).await;
        }

        "/profile" => {
            let user_id =
                profile.unwrap_or_else(|| state.session_id.as_deref().unwrap_or("default"));
            let ctx = slash_profile::ProfileCommandContext {
                profile_manager: &state.user_profile_manager,
                user_id,
            };
            slash_profile::handle_profile_command(arg, &ctx);
        }

        "/tuning" => {
            let ctx = slash_tuning::TuningCommandContext {
                engine: &state.auto_tuning_engine,
                runtime_config: &mut state.runtime_config,
                writer: &mut std::io::stderr(),
            };
            let _ = slash_tuning::handle_tuning_command(arg, ctx);
        }

        "/register" | "/login" | "/logout" | "/memory-setup" => {
            handle_account_command(cmd, arg, api, profile, state).await?;
        }

        "/allow" | "/yolo" => {
            use permission_manager::PermissionMode;
            if cmd == "/yolo" {
                state.perm_manager.set_mode(PermissionMode::Auto);
                eprintln!(
                    "  {} {} All tools auto-approved for this session.",
                    "⚡".yellow(),
                    "YOLO mode!".bold().yellow()
                );
                eprintln!(
                    "  {}",
                    "  Use /allow prompt to restore confirmation prompts.".dim()
                );
            } else {
                match arg {
                    "" => {
                        let next = match state.perm_manager.mode() {
                            PermissionMode::Prompt => PermissionMode::Auto,
                            PermissionMode::Auto => PermissionMode::Deny,
                            PermissionMode::Deny => PermissionMode::Prompt,
                        };
                        state.perm_manager.set_mode(next);
                        eprintln!(
                            "  {} Permission mode → {}",
                            theme::icon_info(),
                            next.to_string().cyan()
                        );
                    }
                    "all" => {
                        state.perm_manager.set_mode(PermissionMode::Auto);
                        eprintln!(
                            "  {} Permission mode → {} (all tools auto-approved)",
                            "⚡".yellow(),
                            "auto".cyan()
                        );
                    }
                    "rules" | "status" => {
                        let summary = state.perm_manager.rules_summary();
                        eprint!("{summary}");
                    }
                    _ => match arg.parse::<PermissionMode>() {
                        Ok(mode) => {
                            state.perm_manager.set_mode(mode);
                            eprintln!(
                                "  {} Permission mode → {}",
                                theme::icon_info(),
                                mode.to_string().cyan()
                            );
                        }
                        Err(_) => {
                            eprintln!(
                                "  {} Unknown mode '{}'. Use: auto, prompt, deny, all, rules",
                                theme::icon_warn(),
                                arg
                            );
                        }
                    },
                }
            }
        }

        "/instructions" => match arg {
            "" | "show" => {
                if let Some(ref pi) = state.project_instructions {
                    let lines = pi.lines().count();
                    eprintln!(
                        "  {} Project instructions ({lines} lines):\n",
                        theme::icon_info()
                    );
                    for line in pi.lines() {
                        eprintln!("  {line}");
                    }
                    eprintln!();
                } else {
                    eprintln!("  {} No project instructions loaded.", theme::icon_info());
                    eprintln!(
                        "  {}",
                        "  Create .astra/instructions.md in your project root to add instructions."
                            .dim()
                    );
                }
            }
            "reload" => {
                if let Some(instructions) = discover_project_instructions() {
                    let lines = instructions.lines().count();
                    state.project_instructions = Some(instructions);
                    eprintln!(
                        "  {} Reloaded project instructions ({lines} lines).",
                        theme::icon_ok()
                    );
                } else {
                    state.project_instructions = None;
                    eprintln!("  {} No .astra/instructions.md found.", theme::icon_info());
                }
            }
            "off" => {
                state.project_instructions = None;
                eprintln!(
                    "  {} Project instructions disabled for this session.",
                    theme::icon_ok()
                );
            }
            _ => {
                eprintln!(
                    "  {} Usage: /instructions [show|reload|off]",
                    theme::icon_warn()
                );
            }
        },

        "/clear" | "/explain" | "/verbose" | "/compact" | "/reflect" | "/undo" | "/redo" => {
            handle_state_command(
                cmd,
                arg,
                StateCommandContext {
                    api,
                    profile,
                    token,
                    selector,
                },
                state,
            )
            .await?;
        }

        "/memory" | "/plan" => {
            handle_memory_domain_command(cmd, arg, api, state, token).await?;
        }

        "/task" => {
            slash_task::handle_task_command(arg, state, api, profile, token).await;
        }

        "/resume" => {
            slash_session::handle_resume_command(arg, profile, api, state).await;
        }

        "/stats" => {
            slash_stats::handle_stats_command(arg, state).await;
        }

        "/bug" => {
            handle_bug_command(arg, state);
        }

        "/sync" => {
            slash_sync::handle_sync_command(arg, state).await;
        }

        "/diff" => {
            let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            diff_presenter::run_diff_command(&root, arg, cli_utils::terminal_width_usize());
        }

        "/exit" | "/quit" => {
            eprintln!("{}", "  Goodbye.".dim());
            return Ok(true);
        }

        _ => {
            let suggestions = suggest_commands(cmd, 3);
            if suggestions.is_empty() {
                eprintln!(
                    "{}",
                    format!("  Unknown command: {}  (type /help for list)", cmd).yellow()
                );
            } else {
                let hint = suggestions.join(", ");
                eprintln!(
                    "{}",
                    format!("  Unknown command: {}  (did you mean: {}?)", cmd, hint).yellow()
                );
            }
        }
    }

    Ok(false)
}

#[cfg(test)]
mod model_list_json_tests {
    use super::*;

    #[test]
    fn respects_is_active_false() {
        let v = serde_json::json!({"name": "m", "is_active": false});
        assert!(!model_list_entry_is_active(&v));
    }

    #[test]
    fn respects_legacy_active_false() {
        let v = serde_json::json!({"name": "m", "active": false});
        assert!(!model_list_entry_is_active(&v));
    }

    #[test]
    fn is_active_wins_over_active_when_both_present() {
        let v = serde_json::json!({"name": "m", "is_active": false, "active": true});
        assert!(!model_list_entry_is_active(&v));
    }

    #[test]
    fn missing_flags_defaults_true_for_unknown_servers() {
        let v = serde_json::json!({"name": "m"});
        assert!(model_list_entry_is_active(&v));
    }

    #[test]
    fn is_active_numeric_zero_means_inactive() {
        let v = serde_json::json!({"name": "m", "is_active": 0});
        assert!(!model_list_entry_is_active(&v));
    }

    #[test]
    fn reads_flattened_thinking_mode_from_model_list() {
        let v = serde_json::json!({"name": "claude", "thinking_mode": "controllable"});
        assert_eq!(model_list_entry_thinking_mode(&v), Some("controllable"));
    }

    #[test]
    fn reads_nested_quirks_thinking_mode() {
        let v = serde_json::json!({
            "name": "glm",
            "quirks": { "thinking_mode": "native" }
        });
        assert_eq!(model_list_entry_thinking_mode(&v), Some("native"));
    }

    #[test]
    fn flattened_thinking_mode_wins_over_nested_quirks() {
        let v = serde_json::json!({
            "name": "claude",
            "thinking_mode": "controllable",
            "quirks": { "thinking_mode": "native" }
        });
        assert_eq!(model_list_entry_thinking_mode(&v), Some("controllable"));
    }

    #[test]
    fn missing_thinking_mode_returns_none() {
        let v = serde_json::json!({"name": "minimax"});
        assert_eq!(model_list_entry_thinking_mode(&v), None);
    }
}
