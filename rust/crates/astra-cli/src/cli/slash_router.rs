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

fn model_list_entry_thinking_capability(entry: &serde_json::Value) -> Option<&str> {
    entry.get("thinking_capability").and_then(|v| v.as_str())
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

/// Slash-command fallback path used by the TUI for state-bearing
/// commands (`/clear`, `/undo`, `/redo`, `/compact`, `/explain`,
/// `/verbose`, `/reflect`, `/model`, etc.) that are dispatched from
/// `tui::slash_dispatch::SlashResult::Fallback`. Returns `Ok(true)`
/// when the caller should exit.
pub(crate) async fn handle_slash_command(
    line: &str,
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    state: &mut SessionState,
    token: Option<&str>,
) -> Result<bool, String> {
    let mut parts = line.splitn(2, ' ');
    let raw_cmd = parts.next().unwrap_or("");
    let arg = parts.next().unwrap_or("").trim();
    let cmd = match command_registry::resolve_command(raw_cmd) {
        Ok(command) => command,
        Err(candidates) if candidates.is_empty() => {
            let suggestions = command_registry::suggest_commands(raw_cmd, 3);
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

    if let Err(err) = command_usage::record_command_use(cmd) {
        eprintln!(
            "{}",
            format!("  Warning: failed to update command discovery history: {err}").yellow()
        );
    }

    match cmd {
        // `/help`, `/`, `/?`, `/commands` are handled by `tui::slash_dispatch`;
        // they should not fall through to this path.
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
                    let thinking_cap =
                        selected_model.and_then(model_list_entry_thinking_capability);
                    let provider = selected_model.and_then(model_list_entry_provider);
                    let opts = astra_turn_core::thinking_config::thinking_options_with_capability(
                        &chosen,
                        provider,
                        thinking_cap,
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
            slash_config::set_active_model_for_display(Some(arg.to_string()));
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

        "/checkpoint" => match create_manual_checkpoint(state, arg) {
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

        "/history" | "/grep" | "/review" | "/copy" | "/diagnostics" | "/lsp" | "/context"
        | "/version" | "/whoami" | "/rewind" | "/report" => {
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
                            // Issue #326 P0 / R1 Minor 5: BypassSafety
                            // is opt-in only via /yolo + --yolo, never
                            // entered via cycling. Cycling out of it
                            // returns to the safe default (Prompt).
                            PermissionMode::BypassSafety => PermissionMode::Prompt,
                        };
                        state.perm_manager.set_mode(next);
                        eprintln!(
                            "  {} Permission mode → {}",
                            theme::icon_info(),
                            next.to_string().magenta()
                        );
                    }
                    "all" => {
                        state.perm_manager.set_mode(PermissionMode::Auto);
                        eprintln!(
                            "  {} Permission mode → {} (all tools auto-approved)",
                            "⚡".yellow(),
                            "auto".magenta()
                        );
                    }
                    "rules" | "status" => {
                        let summary = state.perm_manager.rules_summary();
                        eprint!("{summary}");
                    }
                    "trust" => match state.perm_manager.trust_workspace() {
                        Ok(message) => eprintln!("  {} {message}", theme::icon_info()),
                        Err(err) => {
                            eprintln!("  {} Failed to trust workspace: {err}", theme::icon_warn())
                        }
                    },
                    "untrust" => match state.perm_manager.untrust_workspace() {
                        Ok(message) => eprintln!("  {} {message}", theme::icon_info()),
                        Err(err) => eprintln!(
                            "  {} Failed to mark workspace untrusted: {err}",
                            theme::icon_warn()
                        ),
                    },
                    "trace" => {
                        for line in astra_turn_core::permission_audit::format_snapshot_lines(50) {
                            eprintln!("{line}");
                        }
                    }
                    arg if arg.starts_with("trace --export ") => {
                        let path = arg.trim_start_matches("trace --export ").trim();
                        if path.is_empty() {
                            eprintln!("  {} Missing export path", theme::icon_warn());
                        } else {
                            let lines =
                                astra_turn_core::permission_audit::snapshot_redacted_jsonl_lines();
                            let body = if lines.is_empty() {
                                String::new()
                            } else {
                                format!("{}\n", lines.join("\n"))
                            };
                            match std::fs::write(path, body) {
                                Ok(()) => eprintln!(
                                    "  {} Permission trace exported to {path}",
                                    theme::icon_info()
                                ),
                                Err(err) => eprintln!(
                                    "  {} Failed to export permission trace to {path}: {err}",
                                    theme::icon_warn()
                                ),
                            }
                        }
                    }
                    _ => match arg.parse::<PermissionMode>() {
                        Ok(mode) => {
                            state.perm_manager.set_mode(mode);
                            eprintln!(
                                "  {} Permission mode → {}",
                                theme::icon_info(),
                                mode.to_string().magenta()
                            );
                        }
                        Err(_) => {
                            eprintln!(
                                "  {} Unknown mode '{}'. Use: auto, prompt, deny, all, rules, trust, untrust, trace",
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

        "/clear" | "/explain" | "/compact" | "/reflect" | "/undo" | "/redo" => {
            handle_state_command(
                cmd,
                arg,
                StateCommandContext {
                    api,
                    profile,
                    token,
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

        "/ask" => {
            if arg.trim().is_empty() {
                eprintln!("{}", "  Usage: /ask <question about this session>".yellow());
                eprintln!("{}", "  Example: /ask why is my cache hit rate low?".dim());
            } else {
                // Build diagnostics context and inject WITH the question.
                let mut diag_parts = Vec::new();
                diag_parts.push("[Runtime Diagnostics]".to_string());
                diag_parts.push(format!("Turn: {}", state.turn));
                diag_parts.push(format!(
                    "Tokens: {}in + {}out (cache_read={}, cache_create={})",
                    state.total_prompt_tokens,
                    state.total_completion_tokens,
                    state.total_cache_read_tokens,
                    state.total_cache_creation_tokens,
                ));
                let total_in = state.total_prompt_tokens
                    + state.total_cache_read_tokens
                    + state.total_cache_creation_tokens;
                let cache_pct = if total_in > 0 {
                    state.total_cache_read_tokens as f64 / total_in as f64 * 100.0
                } else {
                    0.0
                };
                diag_parts.push(format!("Cache hit rate: {cache_pct:.1}%"));
                diag_parts.push(format!(
                    "\nAnswer this question using the diagnostics above: {}",
                    arg
                ));
                state.diagnostics_context = Some(diag_parts.join("\n"));
                // Queue the user's question for immediate dispatch. The REPL
                // loop picks up queued_message and sends it as if the user typed
                // it, with diagnostics_context prepended by build_effective_line.
                state.queued_message = Some(arg.to_string());
            }
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
            let suggestions = command_registry::suggest_commands(cmd, 3);
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

/// Fetch active model names from the API (for TUI inline selection).
pub(crate) async fn fetch_model_list(
    api: &astra_thin_client::ThinClient,
    token: Option<&str>,
) -> Result<Vec<String>, String> {
    let entries = fetch_model_list_raw(api, token).await?;
    Ok(entries
        .iter()
        .filter_map(|m| {
            let name = model_list_entry_name(m)?;
            if !model_list_entry_is_active(m) {
                return None;
            }
            Some(name.to_string())
        })
        .collect())
}

/// Fetch the full JSON catalog (used when the caller needs
/// `thinking_capability` / `provider` / pricing alongside the
/// name).  Returns only active entries so downstream lookups don't
/// have to re-filter.
pub(crate) async fn fetch_model_list_raw(
    api: &astra_thin_client::ThinClient,
    token: Option<&str>,
) -> Result<Vec<serde_json::Value>, String> {
    let tok = token.ok_or_else(|| "Not logged in".to_string())?;
    let body = api.get_models_text(tok).await.map_err(|e| format!("{e}"))?;
    let value: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| format!("Failed to parse model list response: {e}"))?;
    let models = value
        .as_array()
        .cloned()
        .or_else(|| value.get("models").and_then(|v| v.as_array()).cloned())
        .unwrap_or_default();
    Ok(models
        .into_iter()
        .filter(model_list_entry_is_active)
        .collect())
}

/// Lookup a model entry by name in a raw list.
pub(crate) fn find_model_entry_by_name<'a>(
    models: &'a [serde_json::Value],
    name: &str,
) -> Option<&'a serde_json::Value> {
    find_model_list_entry(models, name)
}

/// Public accessor for a model entry's `thinking_capability`
/// field.  Used by the TUI to decide whether to show the
/// thinking-mode picker after the main model picker.
pub(crate) fn entry_thinking_capability(entry: &serde_json::Value) -> Option<&str> {
    model_list_entry_thinking_capability(entry)
}

/// Public accessor for a model entry's `provider` field.
pub(crate) fn entry_provider(entry: &serde_json::Value) -> Option<&str> {
    model_list_entry_provider(entry)
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
    fn reads_thinking_capability_both() {
        let v = serde_json::json!({"name": "claude", "thinking_capability": "both"});
        assert_eq!(model_list_entry_thinking_capability(&v), Some("both"));
    }

    #[test]
    fn reads_thinking_capability_native_only() {
        let v = serde_json::json!({"name": "glm", "thinking_capability": "native_only"});
        assert_eq!(
            model_list_entry_thinking_capability(&v),
            Some("native_only")
        );
    }

    #[test]
    fn missing_thinking_capability_returns_none() {
        let v = serde_json::json!({"name": "minimax"});
        assert_eq!(model_list_entry_thinking_capability(&v), None);
    }

    #[test]
    fn null_thinking_capability_returns_none() {
        let v = serde_json::json!({"name": "x", "thinking_capability": null});
        assert_eq!(model_list_entry_thinking_capability(&v), None);
    }
}
