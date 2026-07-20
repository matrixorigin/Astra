//! Slash command fallback routing for the interactive session.

use astra_runtime::prompts;
use astra_services::{ModelListItemResponse, session_journal};
use crossterm::style::Stylize;
use std::{io::IsTerminal, path::PathBuf};

use crate::cli::slash::{
    slash_account::handle_account_command,
    slash_bug::handle_bug_command,
    slash_debug::handle_debug_command,
    slash_info::handle_info_command,
    slash_memory::handle_memory_domain_command,
    slash_messaging::handle_messaging_command,
    slash_session::handle_session_command,
    slash_skill::handle_skill_command,
    slash_state::{StateCommandContext, handle_state_command},
};
use crate::cli::{
    cli_config::{
        cli_output,
        cli_utils::{self, interactive_select, map_thin_err},
    },
    command_registry, command_usage, diff_presenter,
    project_instructions::discover_project_instructions,
    session::{session_checkpointing, session_runtime, session_state::SessionState},
    slash::{
        slash_agent, slash_cache, slash_config, slash_inspect, slash_mcp, slash_profile,
        slash_session, slash_stats, slash_sync, slash_task, slash_team, slash_telemetry,
    },
    theme,
};

pub(crate) type ModelCatalogEntry = ModelListItemResponse;

fn model_list_entry_is_active(entry: &ModelCatalogEntry) -> bool {
    entry.is_active
}

fn model_list_entry_name(entry: &ModelCatalogEntry) -> Option<&str> {
    let name = entry.name.trim();
    (!name.is_empty()).then_some(name)
}

fn model_list_entry_offering_id(entry: &ModelCatalogEntry) -> &str {
    entry.offering_id.as_str()
}

fn model_list_entry_thinking_capability(entry: &ModelCatalogEntry) -> Option<&'static str> {
    entry.thinking_capability.map(|value| value.as_str())
}

fn model_list_entry_provider(entry: &ModelCatalogEntry) -> Option<&str> {
    let provider = entry.provider.trim();
    (!provider.is_empty()).then_some(provider)
}

fn find_model_list_entry<'a>(
    models: &'a [ModelCatalogEntry],
    name: &str,
) -> Option<&'a ModelCatalogEntry> {
    models.iter().find(|m| {
        model_list_entry_offering_id(m) == name
            || model_list_entry_name(m).is_some_and(|n| n.eq_ignore_ascii_case(name))
    })
}

/// Shared slash-command implementation for non-TUI command-line use.
/// Returns `Ok(true)` when the caller should exit.
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
                let models = parse_model_catalog(&body).map_err(|error| error.to_string())?;

                let items: Vec<(String, String)> = models
                    .iter()
                    .filter_map(|m| {
                        let name = model_list_entry_name(m)?;
                        if !model_list_entry_is_active(m) {
                            return None;
                        }
                        let desc = m.description.clone().unwrap_or_default();
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
                    slash_config::set_active_offering_id_for_request(
                        selected_model.map(|entry| entry.offering_id.clone()),
                    );
                    state.cached_pricing = slash_stats::fallback_pricing(&chosen);
                    let context_window =
                        selected_model.and_then(session_runtime::model_list_entry_context_window);
                    state.context_budget =
                        prompts::ContextBudget::from_runtime_config_with_context_window(
                            &state.runtime_config,
                            Some(&chosen),
                            context_window,
                        );
                    eprintln!(
                        "  {} {}",
                        theme::icon_ok(),
                        format!("Set model to {model_with_suffix}").green()
                    );
                } else {
                    eprintln!("{}", "  Cancelled.".dim());
                }
            }
        }

        "/model" => {
            let mut selected_offering_id: Option<String> = None;
            let mut selected_model_name: Option<String> = None;
            let mut context_window = None;
            if let Some(tok) = token {
                match api.get_models_text(tok).await {
                    Ok(body) => {
                        let models = match parse_model_catalog(&body) {
                            Ok(models) => models,
                            Err(err) => {
                                eprintln!(
                                    "{}",
                                    format!("  Failed to parse model list response: {err}")
                                        .yellow()
                                );
                                return Ok(false);
                            }
                        };
                        if models.is_empty() {
                            eprintln!("{}", "  No models returned by the server.".yellow());
                            return Ok(false);
                        }

                        let matched_entry = find_model_list_entry(&models, arg);
                        if let Some(entry) = matched_entry {
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
                            selected_offering_id = Some(entry.offering_id.clone());
                            selected_model_name =
                                model_list_entry_name(entry).map(ToOwned::to_owned);
                            context_window =
                                session_runtime::model_list_entry_context_window(entry);
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

                        let model_exists = matched_entry.is_some()
                            || available.iter().any(|m| m.eq_ignore_ascii_case(arg));

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
                    Err(err) => {
                        eprintln!(
                            "{}",
                            format!("  Failed to list models: {}", map_thin_err(err)).yellow()
                        );
                        return Ok(false);
                    }
                }
            }

            let selected_model = selected_model_name.unwrap_or_else(|| arg.to_string());
            state.model = Some(selected_model.clone());
            slash_config::set_active_offering_id_for_request(selected_offering_id);
            slash_config::set_active_model_for_display(Some(selected_model.clone()));
            let base_model =
                astra_turn_core::thinking_config::resolve_model_thinking(&selected_model).0;
            state.cached_pricing = slash_stats::fallback_pricing(base_model);
            state.context_budget = prompts::ContextBudget::from_runtime_config_with_context_window(
                &state.runtime_config,
                Some(base_model),
                context_window,
            );
            eprintln!(
                "{}",
                format!("  \u{2713}  Set model to {}", selected_model).green()
            );
            if let Some(ref j) = state.journal {
                crate::cli::cli_config::cli_utils::append_journal_event_or_warn(
                    j,
                    state.session_id.as_deref(),
                    &session_journal::JournalEvent::config_change(
                        state.session_id.as_deref(),
                        "model",
                        &selected_model,
                    ),
                    "slash_router:model",
                );
            }
        }

        "/session" => handle_session_command(arg, api, profile, state, token).await,

        "/config" => slash_config::handle_config_command(arg),

        "/checkpoint" => match session_checkpointing::create_manual_checkpoint(state, arg) {
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

        "/cache" => {
            slash_cache::handle_cache_command(arg, state);
        }

        "/inspect" => {
            slash_inspect::handle_inspect_command(arg, state);
        }

        "/history" | "/grep" | "/review" | "/copy" | "/diagnostics" | "/lsp" | "/context"
        | "/version" | "/info" | "/whoami" | "/rewind" | "/report" => {
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

        "/register" | "/login" | "/logout" | "/memory-setup" => {
            handle_account_command(cmd, arg, api, profile, state).await?;
        }

        "/allow" => {
            crate::cli::permission_command::handle_permission_command(arg, state);
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

        "/memory" => {
            handle_memory_domain_command(cmd, arg, api, state, token).await?;
        }

        "/plan" => {
            crate::cli::slash::slash_plan::handle_plan_command(arg, api, profile, state, token)
                .await?;
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
                // it, with diagnostics_context routed through required runtime context.
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

        "/stop" => {
            eprintln!("{}", "  No active run to stop.".dim());
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

#[derive(Debug, thiserror::Error)]
pub(crate) enum ModelCatalogError {
    #[error("not logged in")]
    NotAuthenticated,
    #[error(transparent)]
    Request(#[from] astra_thin_client::ThinClientError),
    #[error("invalid model catalog JSON: {0}")]
    InvalidJson(#[from] serde_json::Error),
}

impl ModelCatalogError {
    pub(crate) fn is_authentication_failure(&self) -> bool {
        matches!(self, Self::NotAuthenticated)
            || matches!(
                self,
                Self::Request(astra_thin_client::ThinClientError::Api { status, .. })
                    if *status == reqwest::StatusCode::UNAUTHORIZED
            )
    }

    pub(crate) fn is_transport_failure(&self) -> bool {
        matches!(self, Self::Request(error) if error.is_transport())
    }
}

fn parse_model_catalog(body: &str) -> Result<Vec<ModelCatalogEntry>, ModelCatalogError> {
    Ok(serde_json::from_str(body)?)
}

/// Fetch the exact public model catalog. Only active Offerings reach the
/// picker; administration surfaces use the server catalog directly.
pub(crate) async fn fetch_model_catalog(
    api: &astra_thin_client::ThinClient,
    token: Option<&str>,
) -> Result<Vec<ModelCatalogEntry>, ModelCatalogError> {
    let tok = token.ok_or(ModelCatalogError::NotAuthenticated)?;
    let body = api.get_models_text(tok).await?;
    Ok(parse_model_catalog(&body)?
        .into_iter()
        .filter(model_list_entry_is_active)
        .collect())
}

/// Lookup a model entry by canonical Offering ID or display name.
pub(crate) fn find_model_entry_by_name<'a>(
    models: &'a [ModelCatalogEntry],
    name: &str,
) -> Option<&'a ModelCatalogEntry> {
    find_model_list_entry(models, name)
}

/// Public accessor for a model entry's `thinking_capability`
/// field.  Used by the TUI to decide whether to show the
/// thinking-mode picker after the main model picker.
pub(crate) fn entry_thinking_capability(entry: &ModelCatalogEntry) -> Option<&'static str> {
    model_list_entry_thinking_capability(entry)
}

/// Public accessor for the stable Offering identity selected by clients.
pub(crate) fn entry_offering_id(entry: &ModelCatalogEntry) -> &str {
    model_list_entry_offering_id(entry)
}

/// Public accessor for a model entry's display name.
pub(crate) fn entry_model_name(entry: &ModelCatalogEntry) -> Option<&str> {
    model_list_entry_name(entry)
}

/// Public accessor for a model entry's active state.
pub(crate) fn entry_model_is_active(entry: &ModelCatalogEntry) -> bool {
    model_list_entry_is_active(entry)
}

/// Public accessor for a model entry's `provider` field.
pub(crate) fn entry_provider(entry: &ModelCatalogEntry) -> Option<&str> {
    model_list_entry_provider(entry)
}

#[cfg(test)]
mod model_list_json_tests {
    use super::{
        ModelCatalogError, entry_model_is_active, entry_model_name, entry_offering_id,
        find_model_entry_by_name, model_list_entry_thinking_capability, parse_model_catalog,
    };

    fn canonical_catalog_json() -> serde_json::Value {
        serde_json::json!([{
            "offering_id": "offer-coding",
            "access_id": "self-hosted",
            "access_kind": "self_hosted",
            "access_label": "Self-hosted",
            "execution_placement": "server",
            "name": "Coding Model",
            "provider": "openai",
            "description": "Primary coding model",
            "is_active": true,
            "context_window": 128000,
            "max_completion_tokens": 8192,
            "architecture": null,
            "thinking_capability": "both"
        }])
    }

    #[test]
    fn model_catalog_auth_classification_uses_http_status_not_body_text() {
        let unauthorized = ModelCatalogError::Request(astra_thin_client::ThinClientError::Api {
            status: reqwest::StatusCode::UNAUTHORIZED,
            body: "arbitrary provider response".into(),
        });
        assert!(unauthorized.is_authentication_failure());

        let misleading_body = ModelCatalogError::Request(astra_thin_client::ThinClientError::Api {
            status: reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            body: "request failed (401): not actually an auth response".into(),
        });
        assert!(!misleading_body.is_authentication_failure());
    }

    #[test]
    fn missing_token_is_an_authentication_failure() {
        assert!(ModelCatalogError::NotAuthenticated.is_authentication_failure());
    }

    #[test]
    fn canonical_catalog_preserves_offering_and_model_facts() {
        let models =
            parse_model_catalog(&canonical_catalog_json().to_string()).expect("canonical catalog");
        let entry = find_model_entry_by_name(&models, "offer-coding").expect("offering entry");
        assert_eq!(entry_offering_id(entry), "offer-coding");
        assert_eq!(entry_model_name(entry), Some("Coding Model"));
        assert_eq!(model_list_entry_thinking_capability(entry), Some("both"));
        assert!(entry_model_is_active(entry));
        assert!(find_model_entry_by_name(&models, "coding model").is_some());
    }

    #[test]
    fn obsolete_catalog_shapes_are_rejected_at_the_boundary() {
        let item = canonical_catalog_json()[0].clone();
        let envelope = serde_json::json!({"models": [item.clone()]});
        parse_model_catalog(&envelope.to_string()).expect_err("envelopes are not the contract");

        let mut obsolete = item;
        let object = obsolete.as_object_mut().expect("catalog item object");
        object.remove("offering_id");
        object.insert("model_id".into(), serde_json::json!("provider-model-id"));
        parse_model_catalog(&serde_json::json!([obsolete]).to_string())
            .expect_err("provider model ids cannot select an Offering");
    }
}
