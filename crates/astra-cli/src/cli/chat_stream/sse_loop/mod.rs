//! CLI streaming adapter for one Server-owned developer-loop admission.
//!
//! Entry [`stream_chat_sse`] builds a [`CliServerAdmissionHost`] and common
//! ingestion/finalization state. Any model/tool continuation is owned by the
//! Server; Edge callbacks complete while the sole response stream is open.

mod agentic_loop_turn;
mod agentic_sse_loop;
mod deferred_activation_state;
mod server_admission_host;

pub(crate) use agentic_loop_turn::{
    server_loop_admission_payload, turn_policy_from_payload_edge_tools,
};

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use astra_core::RuntimeLimits;
use astra_runtime::{
    pipeline::step_recorder::StepRecorder,
    semantic_dedup::SemanticDedup,
    tool_registry::ToolRegistry,
    turn::agentic_loop::finalization::run_agentic_loop_with_host,
    turn::agentic_loop::host::{
        AgenticLoopState, CancellationState, ErrorRecoveryState, MessagingState, SkillState,
        StallTrackingState, StopHookState, TelemetryState, runtime_manifest_for_model,
    },
    turn::chat_turn_heuristics::infer_task_execution_profile,
    turn::edge_prompt_context::detect_project_languages,
    turn::stop_hooks_yaml::detect_turn_hook_sets,
    turn::tool_health::ToolHealthTracker,
    turn::turn_guard::TurnGuard,
};

use crate::{
    ExplainMode, StreamResult,
    cli::cli_config::cli_utils::{cli_user_id, terminal_width_usize},
    edge_tools,
};

use crate::cli::chat_stream::ChatTurnParams;
use crate::cli::chat_stream::explain_reports;
use crate::cli::chat_stream::params::StreamEvent;
use crate::cli::session::session_runtime::{self, ServerDefaultModel};
use agentic_sse_loop::{
    StreamLoopSidecarEprint, StreamResultBuild, build_stream_result, eprint_stream_loop_sidecars,
    resolved_tool_metrics,
};
use serde_json::{Value, json};
use server_admission_host::CliServerAdmissionHost;

fn non_tty_output_failure(
    is_terminal: bool,
    state: crate::cli::stream::output_sink::StdoutState,
) -> Option<crate::cli::stream::streaming_types::OutputTransportFailure> {
    use crate::cli::stream::streaming_types::OutputTransportFailure;

    if is_terminal {
        return None;
    }
    match state {
        crate::cli::stream::output_sink::StdoutState::Open => None,
        crate::cli::stream::output_sink::StdoutState::Closed => {
            Some(OutputTransportFailure::Closed)
        }
        crate::cli::stream::output_sink::StdoutState::Failed => {
            Some(OutputTransportFailure::Failed)
        }
    }
}

/// Map `ToolPolicyConfig` (from `astra-config`) to `BreakerConfig` (from
/// `astra-turn-core`).
///
/// Lives in the CLI because `astra-config` and `astra-turn-core` are sibling
/// crates with no dependency edge between them; adding `impl From<…> for
/// BreakerConfig` in either crate would introduce an unwanted dependency.
/// The CLI is the natural composition layer that already depends on both.
/// If a second caller appears, promote this to a dedicated adapter crate
/// rather than coupling the two base crates.
fn circuit_breaker_config_from_tool_policy(
    config: &astra_config::runtime_config::ToolPolicyConfig,
) -> astra_turn_core::loop_circuit_breaker::BreakerConfig {
    // Resolve each threshold and warn when a user-supplied value was clamped
    // to its floor so operators can diagnose unexpected behaviour.
    macro_rules! resolve_and_warn {
        ($raw:expr, $effective:expr, $name:literal) => {{
            let raw = $raw;
            let effective = $effective;
            if raw > 0 && effective != raw {
                tracing::warn!(
                    config_field = $name,
                    user_value = raw,
                    applied_value = effective,
                    "circuit breaker config value below floor — clamped to minimum"
                );
            }
            effective as usize
        }};
    }

    astra_turn_core::loop_circuit_breaker::BreakerConfig {
        stall_threshold: resolve_and_warn!(
            config.circuit_breaker_stall_threshold,
            config.effective_circuit_breaker_stall_threshold(),
            "circuit_breaker_stall_threshold"
        ),
        repetition_threshold: resolve_and_warn!(
            config.circuit_breaker_repetition_threshold,
            config.effective_circuit_breaker_repetition_threshold(),
            "circuit_breaker_repetition_threshold"
        ),
        read_only_stall_threshold: resolve_and_warn!(
            config.circuit_breaker_read_only_stall_threshold,
            config.effective_circuit_breaker_read_only_stall_threshold(),
            "circuit_breaker_read_only_stall_threshold"
        ),
        max_introspect_emissions: resolve_and_warn!(
            config.circuit_breaker_max_introspect_emissions,
            config.effective_circuit_breaker_max_introspect_emissions(),
            "circuit_breaker_max_introspect_emissions"
        ),
        half_open_patience: resolve_and_warn!(
            config.circuit_breaker_half_open_patience,
            config.effective_circuit_breaker_half_open_patience(),
            "circuit_breaker_half_open_patience"
        ),
        absolute_max_rounds: resolve_and_warn!(
            config.circuit_breaker_absolute_max_rounds,
            config.effective_circuit_breaker_absolute_max_rounds(),
            "circuit_breaker_absolute_max_rounds"
        ),
    }
}

fn restored_compaction_effectiveness(
    compaction_state: Option<&serde_json::Value>,
) -> astra_runtime::turn::compaction_replay::CompactionEffectivenessTracker {
    compaction_state
        .map(
            astra_runtime::turn::compaction_replay::CompactionEffectivenessTracker::from_json_lossy,
        )
        .unwrap_or_default()
}

async fn finalize_root_mailbox(
    slot: Option<&mut Option<astra_messaging::router::AgentMailbox>>,
    mailbox: &mut Option<astra_messaging::router::AgentMailbox>,
) {
    if let Some(slot) = slot {
        *slot = mailbox.take();
        return;
    }

    if let Some(mailbox) = mailbox.take() {
        let addr = mailbox.address.clone();
        let router = mailbox.router();
        if let Err(e) = router.unregister(&addr).await {
            eprintln!(
                "astra: failed to unregister mailbox for run_id={} agent_id={}: {e}",
                addr.run_id, addr.agent_id
            );
        }
    }
}

type RootPermissionContextHandle = astra_runtime::orchestration::PermissionSyncHandle;

fn root_permission_context_handle(
    perm_manager: &crate::cli::permission_manager::PermissionManager,
) -> RootPermissionContextHandle {
    perm_manager.runtime_permission_handle()
}

fn step_recorder_for_cli_turn(
    user_id: &str,
    session_id: Option<&str>,
    run_id: &str,
) -> StepRecorder {
    if let Some(session_id) = session_id {
        StepRecorder::with_persistence_for_run(user_id, session_id, run_id, run_id)
    } else {
        StepRecorder::with_deferred_persistence_for_run(user_id, "ephemeral", run_id, run_id)
    }
}

async fn refresh_root_permission_context(
    handle: &mut Option<RootPermissionContextHandle>,
    perm_manager: &crate::cli::permission_manager::PermissionManager,
) {
    let latest = perm_manager.runtime_permission_context();
    if let Some(existing) = handle.as_ref() {
        // Merge — NOT replace. Overwriting the whole context would wipe
        // runtime telemetry (tools_blocked, recent_denials, ...) and the
        // in-session allow/deny decisions the user already made this turn,
        // breaking the self-model feedback loop. Only the policy half
        // (`inherited`) is refreshed from the manager's latest snapshot.
        let mut guard = existing.write().await;
        guard.merge_policy_from(&latest);
    } else {
        *handle = Some(latest.into_shared());
    }
}

pub(crate) async fn stream_chat_sse(
    mut p: ChatTurnParams<'_>,
) -> Result<StreamResult, crate::TurnFailure> {
    let start = Instant::now();
    p.model = normalize_turn_model(p.model);
    let mut model_context_window = None;
    let default_model = if p.model.is_none() {
        match session_runtime::resolve_server_default_model(p.api, p.token).await {
            ServerDefaultModel::Selected(selection) => {
                model_context_window = selection.context_window;
                p.offering_id = Some(selection.offering_id);
                Some(selection.name)
            }
            ServerDefaultModel::NoModels | ServerDefaultModel::Unavailable => None,
        }
    } else {
        None
    };
    if let Some(model) = default_model.as_deref() {
        tracing::info!(
            target: "astra_cli::model_selection",
            model,
            "selected default model from server model list for stream turn"
        );
        p.model = Some(model);
    }
    let Some(selected_model) = require_selected_turn_model(p.model, p.session_id, p.turn_index)
    else {
        record_missing_model_selection_failure(
            p.session_id,
            p.turn_index,
            p.message,
            start.elapsed().as_millis() as u64,
        );
        return Err(missing_model_selection_turn_failure(p.session_id));
    };
    p.model = Some(selected_model);
    if model_context_window.is_none() {
        match session_runtime::resolve_server_model_selection(p.api, p.token, selected_model).await
        {
            Ok(selection) => {
                p.offering_id = Some(selection.offering_id);
                model_context_window = selection.context_window;
            }
            Err(error) => {
                tracing::error!(
                    target: "astra_cli::model_selection",
                    model = selected_model,
                    error = %error,
                    "turn failed before SSE stream because model context_window metadata was unavailable"
                );
                return Err(crate::TurnFailure {
                    error,
                    partial: crate::PartialTurnData {
                        session_id: p.session_id.map(str::to_string),
                        ..Default::default()
                    },
                });
            }
        }
    }
    let Some(context_window_tokens) = model_context_window else {
        let error = format!(
            "model '{selected_model}' is missing positive context_window metadata in the server registry"
        );
        tracing::error!(
            target: "astra_cli::model_selection",
            model = selected_model,
            "turn failed before SSE stream because selected model did not include context_window"
        );
        return Err(crate::TurnFailure {
            error,
            partial: crate::PartialTurnData {
                session_id: p.session_id.map(str::to_string),
                ..Default::default()
            },
        });
    };
    if p.offering_id.is_none() {
        let error = format!(
            "model '{selected_model}' is missing an exact Offering identity in the server registry"
        );
        tracing::error!(
            target: "astra_cli::model_selection",
            model = selected_model,
            "turn failed before SSE stream because selected model did not include offering_id"
        );
        return Err(crate::TurnFailure {
            error,
            partial: crate::PartialTurnData {
                session_id: p.session_id.map(str::to_string),
                ..Default::default()
            },
        });
    }
    let effective_max_turn_input_tokens = RuntimeLimits::global()
        .effective_max_turn_input_tokens_with_context_window(p.model, model_context_window);
    // This value governs CLI-owned preparation and recovery state only.  Do
    // not publish it as the Server's context-window policy: the remote Server
    // owns context assembly and compaction for this admission and may run
    // under different process configuration.  The accepted `context_meta`
    // SSE event carries the authoritative policy that observers receive.
    let root_agent_id = p.root_agent_id.unwrap_or("main");
    p.perm_manager.clear_turn_overrides();

    // Stable run_id for this turn — shared by:
    //   1. state.current_run_id (so on_turn_completed captures the
    //      parent prefix keyed on this id)
    //   2. AgentActionContext.run_id (so the spawner's resolver looks
    //      up the same key)
    // Pre-fix these were different ("ephemeral" vs None), so the
    // parent capture never happened and fork-cache probes were dead.
    let parent_turn_run_id = p
        .stream_json_emitter
        .as_ref()
        .map(|emitter| emitter.execution_id().to_string())
        .unwrap_or_else(|| format!("run-{}", uuid::Uuid::new_v4()));
    let term_width = terminal_width_usize();
    // Tool policy and cache behavior follow the resolved model name, never the
    // opaque Offering identity used for admission.
    let model_for_policy = p.model;
    let runtime_manifest =
        runtime_manifest_for_model("cli_turn_selection", "cli_edge", model_for_policy);
    let tool_policy_config = astra_config::runtime_config::RuntimeConfig::load().tool_policy;
    let resolved_tool_policy = tool_policy_config.resolve_for_model(model_for_policy);
    let circuit_breaker_config = circuit_breaker_config_from_tool_policy(&tool_policy_config);

    // Paint an immediate spinner so the user sees feedback during init (executor, schemas,
    // skill discovery, etc.) before the per-turn prep spinner takes over.
    let show_early_hint = !p.render_policy.suppress_text()
        && std::io::IsTerminal::is_terminal(&std::io::stderr())
        && p.plan_assemble_line_release.is_none();
    let early_spinner: Option<crate::cli::effects::Spinner> = if show_early_hint {
        Some(crate::cli::effects::Spinner::start_immediate(
            "Preparing…".to_string(),
        ))
    } else {
        None
    };

    let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let file_context = detect_project_languages(&project_root);
    let current_session_turn = p.turn_index;
    // Only callers that provide a durable session journal own checkpoint and
    // composite-snapshot side effects. Internal utility calls such as
    // `/compact` may reuse the active session as retrieval context, but must
    // not advance its resumable timeline.
    let persist_session_artifacts = p.file_journal.is_some() || p.session_state_journal.is_some();
    let mut executor = {
        let ex = edge_tools::ToolExecutor::new(&project_root)
            .with_cloud(p.api.api_origin(), p.token)
            .with_memory_attribution_id(parent_turn_run_id.clone());
        let ex = if let Some(session_id) = p.session_id {
            ex.with_active_session_id(session_id.to_string())
        } else {
            ex
        };
        // Wire session-scoped file journal for cross-turn undo support
        let ex = if let Some(ref journal) = p.file_journal {
            ex.with_shared_file_journal(journal.clone())
        } else {
            ex
        };
        // Wire session-scoped file-state cache for cross-subtask read-before-write
        let ex = if let Some(ref state) = p.file_state {
            ex.with_shared_file_state(state.clone())
        } else {
            ex
        };
        let ex = if let Some(ref journal) = p.database_snapshot_journal {
            ex.with_shared_database_snapshot_journal(journal.clone())
        } else {
            ex
        };
        let ex = if let Some(ref journal) = p.git_stash_journal {
            ex.with_shared_git_stash_journal(journal.clone())
        } else {
            ex
        };
        let ex = if let Some(ref journal) = p.git_commit_journal {
            ex.with_shared_git_commit_journal(journal.clone())
        } else {
            ex
        };
        let ex = if let Some(ref journal) = p.git_worktree_journal {
            ex.with_shared_git_worktree_journal(journal.clone())
        } else {
            ex
        };
        let ex = if let Some(ref journal) = p.session_state_journal {
            ex.with_shared_session_state_journal(journal.clone())
        } else {
            ex
        };
        let ex = if let Some(ref cmds) = p.bg_task_commands {
            ex.with_bg_task_commands(cmds.clone())
        } else {
            ex
        };
        let ex = if let Some(ref cache) = p.bg_task_list_cache {
            ex.with_bg_task_list_cache(cache.clone())
        } else {
            ex
        };
        let ex = if let Some(ref slot) = p.bash_detach_slot {
            ex.with_bash_detach_slot(slot.clone())
        } else {
            ex
        };
        // Set the 1-based session turn so journal entries are scoped to the
        // user-visible turn currently in progress.
        ex.journal_turn_index
            .store(current_session_turn, std::sync::atomic::Ordering::Release);
        // Wire `agent(action='spawn'|'get_result')` context when a spawner is available.
        // The run_id MUST match what state.current_run_id uses so that
        // on_turn_completed captures the parent prefix under the same
        // key the spawner resolves against. Previously this was
        // p.session_id.unwrap_or("ephemeral") — a mismatch that made
        // on_turn_completed early-return (current_run_id = None) and
        // the spawner look up "ephemeral" in the prefix store, finding
        // nothing. Generating a stable UUID here and threading it into
        // both sites closes the gap.
        if let Some(ref spawner) = p.agent_spawner {
            let spawn_ctx = edge_tools::agent_spawning::AgentActionContext {
                run_id: parent_turn_run_id.clone(),
                agent_id: root_agent_id.to_string(),
                delegation_chain: Vec::new(),
                current_model: p.model.map(str::to_string),
                recursion_depth: 0,
                is_fork_child: false,
                working_dir: project_root.clone(),
                spawner: spawner.clone(),
                inherited_permissions: p.perm_manager.inherited_permissions_for_child(false),
                enabled_tools: None,
                active_skills: Vec::new(), // root agent — no inherited skills
                live_event_sink: p.agent_live_event_sink.clone(),
                client_tool_delivery_tx: None,
                trace_context: None,
                execution_metadata: None,
                workspace_mutation:
                    astra_runtime::orchestration::WorkspaceMutationAuthority::default(),
                transcript_location:
                    astra_runtime::orchestration::AgentTranscriptLocation::LocalJournal,
            };
            ex.with_spawn_context(spawn_ctx)
        } else {
            ex
        }
    };
    executor.set_current_model(selected_model.to_string());
    executor.set_current_context_window_tokens(u64::from(context_window_tokens));
    executor.set_current_effective_input_budget_tokens(effective_max_turn_input_tokens);
    // Wire observability session for context_analysis tool
    if let Some(ref obs) = p.observability_session {
        executor.observability_session = Some(obs.clone());
    }
    // P6: propagate cross-session lessons (loaded at first-turn bootstrap)
    // into the ToolExecutor so every SelfModel snapshot this turn carries
    // prior-session advice. No-op when the cache is empty.
    if !p.session_lessons.is_empty() {
        executor.set_session_lessons(p.session_lessons.to_vec());
    }
    // P8: propagate the previous turn's auto-invoke diagnosis so this
    // turn's LLM reads "the system already noticed X" in the self-awareness
    // section. Cloned because the setter takes ownership; the state-side
    // cache keeps its copy for the next turn's render / eventual clear.
    if let Some(diag) = p.latest_skill_diagnosis {
        executor.set_latest_skill_diagnosis(Some(diag.clone()));
    }
    if let Some(feedback) = p.latest_turn_quality_feedback {
        executor.set_latest_turn_quality_feedback(Some(feedback.clone()));
    }
    let root_send_message_context = p.agent_spawner.as_ref().map(|spawner| {
        edge_tools::agent_messaging::SendMessageRuntimeContext {
            agent_id: root_agent_id.to_string(),
            run_id: root_agent_id.to_string(),
            router: spawner.mailbox_router(),
        }
    });

    // --add-dir: expand sandbox to include additional directories
    if let Some(cli_context) = p.cli_context {
        for dir in &cli_context.add_dirs {
            if let Err(e) = executor.expand_sandbox_path(dir.clone()) {
                astra_core::agent_warn!("sandbox", "--add-dir {} rejected: {e}", dir.display());
            }
        }
    } else if let Ok(dirs) = std::env::var("ASTRA_CLI_ADD_DIRS") {
        for dir in dirs.split(':').filter(|s| !s.is_empty()) {
            let dir = PathBuf::from(dir);
            if let Err(e) = executor.expand_sandbox_path(dir.clone()) {
                astra_core::agent_warn!(
                    "sandbox",
                    "ASTRA_CLI_ADD_DIRS {} rejected: {e}",
                    dir.display()
                );
            }
        }
    }
    let mut messages = load_turn_messages(p.pre_loaded_messages.take(), p.history, p.message)
        .map_err(|error| crate::TurnFailure {
            error: error.to_string(),
            partial: crate::PartialTurnData {
                session_id: p.session_id.map(str::to_string),
                ..Default::default()
            },
        })?;
    if let Some(current) = messages.last_mut() {
        astra_turn_types::mark_turn_message(current, &parent_turn_run_id);
    }
    // Only the fresh user suffix starts this root execution transcript. The
    // preceding prompt history is inherited context, not a new conversation
    // item for this run.
    let root_initial_transcript_item = messages.last().and_then(|message| {
        (message.get("role").and_then(serde_json::Value::as_str) == Some("user"))
            .then(|| message.clone())
    });

    // ─── Context pre-fetch (disabled) ─────────────────────────────────────
    // Returns (all_schemas, mcp_runtime_schemas) so the edge executor can
    // install MCP routing and discovery data from the same snapshot while the
    // registry gets the capability-filtered list.
    // Refresh any MCP servers that received tool-list-changed notifications
    if let Some(ref mgr) = p.mcp_manager {
        let mut m = mgr.write().await;
        m.refresh_changed_tools().await;
        m.consume_prompt_changes();
        m.consume_resource_changes();
    }
    // Inject MCP tool schemas from connected servers.
    // Tracked separately from the static catalog so the edge `ToolExecutor`
    // can install MCP routing and schemas atomically for
    // `tool_search(select:mcp__X)` resolution.
    let mcp_schemas = if let Some(ref mgr) = p.mcp_manager {
        let m = mgr.read().await;
        m.all_tool_schemas()
    } else {
        Vec::new()
    };
    let cli_capabilities = edge_tools::cli_default_capabilities(
        p.agent_spawner.is_some(),
        p.bg_task_commands.is_some(),
        executor.github_token.is_some(),
    );
    let all_schemas: (Vec<Value>, Vec<Value>) = (
        astra_runtime::capabilities::cli_local_tool_schemas(
            edge_tools::local_tool_schemas(),
            mcp_schemas.clone(),
            &cli_capabilities,
        ),
        mcp_schemas,
    );
    let mcp_runtime_schemas = all_schemas.1.clone();
    let all_schemas = all_schemas.0;
    executor.set_cli_local_provider_schemas(all_schemas.clone());
    // Install MCP schemas on the edge executor so `tool_search(select:NAME)`
    // can resolve MCP tool schemas by name.
    if let Some(ref mgr) = p.mcp_manager {
        executor.install_mcp_bundle(mgr.clone(), mcp_runtime_schemas);
    }
    deferred_activation_state::restore_into_executor(&p.activated_deferred_tool_names, &executor);
    let registry = ToolRegistry::new_runtime_surface(all_schemas.clone());
    let always_load_schema_tokens = registry.total_always_load_token_cost() as u64;
    // Full runtime inventory is used only for static allow/deny policy
    // calculations. The headless validator's admitted tool set is populated
    // per round from the final `edge_tools` payload actually sent to the model.
    let runtime_tool_names: HashSet<String> = registry.all_schema_names().into_iter().collect();

    // --allowed-tools: if set, restrict to only the specified tools
    let mut initial_restricted: HashSet<String> = if let Some(cli_context) = p.cli_context {
        let allowed: HashSet<&str> = cli_context
            .allowed_tools
            .iter()
            .map(String::as_str)
            .collect();
        if !allowed.is_empty() {
            runtime_tool_names
                .iter()
                .filter(|name| !allowed.contains(name.as_str()))
                .cloned()
                .collect()
        } else {
            HashSet::new()
        }
    } else if let Ok(allowed_csv) = std::env::var("ASTRA_CLI_ALLOWED_TOOLS") {
        let allowed: HashSet<&str> = allowed_csv
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();
        if !allowed.is_empty() {
            runtime_tool_names
                .iter()
                .filter(|name| !allowed.contains(name.as_str()))
                .cloned()
                .collect()
        } else {
            HashSet::new()
        }
    } else {
        HashSet::new()
    };

    // --disallowed-tools: directly add to restricted set
    if let Some(cli_context) = p.cli_context {
        for name in &cli_context.disallowed_tools {
            initial_restricted.insert(name.clone());
        }
    } else if let Ok(denied_csv) = std::env::var("ASTRA_CLI_DISALLOWED_TOOLS") {
        for name in denied_csv
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            initial_restricted.insert(name.to_string());
        }
    }

    initial_restricted.extend(p.resume_restricted_tools.iter().cloned());

    let current_session_id = p.session_id.map(|s| s.to_string());
    let existing_root_mailbox = if let Some(slot) = p.root_mailbox_slot.as_deref_mut() {
        slot.take()
    } else {
        None
    };
    let root_mailbox = if let Some(mailbox) = existing_root_mailbox {
        Some(mailbox)
    } else if let Some(ref root_ctx) = root_send_message_context {
        let run_id = current_session_id
            .clone()
            .unwrap_or_else(|| "ephemeral".to_string());
        root_ctx
            .router
            .register(
                astra_messaging::types::AgentAddress::new(run_id, &root_ctx.agent_id),
                None,
            )
            .await
            .ok()
    } else {
        None
    };
    let task_profile = infer_task_execution_profile(p.message);
    let circuit_breaker_config = circuit_breaker_config.for_task_profile(task_profile);

    let turn_guard = if p.tool_health_entries.is_empty() {
        TurnGuard::with_profile(task_profile)
    } else {
        let health = ToolHealthTracker::from_entries(p.tool_health_entries);
        TurnGuard::with_health_and_profile(health, task_profile)
    };

    let max_turns = {
        let cfg = astra_config::RuntimeConfig::cached();
        cfg.runtime_limits.resolve_turn_ceiling(p.is_plan_subtask)
    };
    let current_user_id = cli_user_id();
    let step_recorder = step_recorder_for_cli_turn(
        &current_user_id,
        current_session_id.as_deref(),
        &parent_turn_run_id,
    );
    let mut local_discovered_skills = HashSet::new();
    let discovered_skills = match p.discovered_skills.as_deref_mut() {
        Some(shared) => std::mem::take(shared),
        None => std::mem::take(&mut local_discovered_skills),
    };

    // Capture the child permission envelope before perm_manager is moved into the host.
    let child_permissions = p.perm_manager.inherited_permissions_for_child(true);
    let parent_cancel_token = p.cancel_token.clone();
    let root_permission_context = root_permission_context_handle(p.perm_manager);

    // Snapshot approval overrides for checkpoint persistence.
    let initial_approval_overrides = p.perm_manager.export_session_overrides();

    // Bug B step 3: share the spawner's prefix_store with the
    // CLI host so on_turn_completed can write captured parent
    // prefixes into the same map the DelegationEngine + spawner
    // read from. Without shared state, a capture fires but lands
    // in a different store than resolvers look at — delegate
    // children always see None.
    let prefix_store_for_host = p
        .agent_spawner
        .as_ref()
        .and_then(|s| s.prefix_store().cloned());

    // ─── Build host + state ──────────────────────────────────────────────
    let mut host = CliServerAdmissionHost {
        api: p.api,
        token: p.token.to_string(),
        auth_profile: p.auth_profile,
        model: p.model,
        offering_id: p.offering_id.clone(),
        context_window_tokens,
        explain: p.explain,
        render_md: p.render_md,
        term_width,
        render_policy: p.render_policy,
        message: p.message,
        user_intent: p.user_intent,
        input_runtime_required_texts: p.input_runtime_required_texts,
        input_active_system_skills: p.input_active_system_skills,
        input_runtime_volatile_texts: p.input_runtime_volatile_texts,
        semantic_query_override: p.semantic_query_override,
        history: p.history,
        recent_tools: p.recent_tools,
        project_root: project_root.clone(),
        executor: std::sync::Arc::new(executor),
        registry,
        all_schemas,
        file_context,
        perm_manager: p.perm_manager,
        valid_tool_names: HashSet::new(),
        capabilities: cli_capabilities,
        pending_clear_lines: 0,
        is_plan_subtask: p.is_plan_subtask,
        plan_subtask_id: p.plan_subtask_id,
        plan_assemble_line_release: p.plan_assemble_line_release.clone(),
        stream_event_tx: p.stream_event_tx.clone(),
        stream_json_emitter: p.stream_json_emitter.clone(),
        pending_ordered_stream_events: std::collections::VecDeque::new(),
        agent_live_event_sink: p.agent_live_event_sink.clone(),
        approval_request_tx: p.approval_request_tx,
        ask_user_request_tx: p.ask_user_request_tx,
        plan_review_request_tx: p.plan_review_request_tx,
        root_send_message_context,
        agent_spawner: p.agent_spawner.clone(),
        chat_turn_index: current_session_turn,
        tool_cache: crate::cli::stream::stream_render::EdgeToolCache::new(
            resolved_tool_policy.max_identical_tool_calls,
        ),
        prefix_store: prefix_store_for_host,
        append_system_prompt: p.append_system_prompt.take(),
        execution_time_budget: p.execution_time_budget.take(),
        incremental_state: p.incremental_state.take(),
        request_session_execution_lease: p.request_session_execution_lease.take(),
        remote_cancel_required: false,
        remote_cancel_run_id: None,
        last_physical_run_id: None,
        output_transport_failure: None,
    };

    let hook_sets = detect_turn_hook_sets(&project_root, task_profile, p.is_plan_subtask);

    // Bind the turn to the shared registry without taking ownership of
    // discovery. Interactive startup converges external providers in a
    // supervised background task; a turn must use the currently available
    // snapshot rather than replaying provider retry/backoff before HTTP
    // dispatch. The resolver observes later registry convergence in place.
    let skill_resolver =
        crate::cli::agent_runtime::bind_skill_resolver(Arc::clone(&p.unified_skill_registry));

    // Build skill executor — fork sub-runs inherit the resolver for nesting.
    let skill_executor: Option<Arc<dyn astra_skills::SkillExecutor>> = {
        let mut subrun_exec = crate::cli::skill_subrun::CliSkillSubRunExecutor::new(
            p.api.clone(),
            p.token.to_string(),
            p.model.map(|m| m.to_string()),
            project_root.clone(),
            child_permissions,
            parent_cancel_token,
        )
        .with_skill_resolver(skill_resolver.clone())
        .with_parent_run_id(parent_turn_run_id.clone());
        if let Some(session_id) = p.session_id {
            subrun_exec = subrun_exec.with_active_session_id(session_id.to_string());
        }
        let subrun_exec = Arc::new(subrun_exec);
        let isolated = Arc::new(astra_skills::executor::IsolatedSkillExecutor::new(
            subrun_exec,
        ));
        let router = Arc::new(astra_skills::executor::SkillExecutionRouter::new(Some(
            isolated,
        )));
        Some(router as Arc<dyn astra_skills::SkillExecutor>)
    };

    // Pre-compute project-level cross-session context (knowledge backflow P2).
    // Cached per-process via OnceLock since git_root is constant for a session.
    use std::sync::OnceLock;
    static PROJECT_CONTEXT_CACHE: OnceLock<Option<String>> = OnceLock::new();

    let project_context: Option<String> = PROJECT_CONTEXT_CACHE
        .get_or_init(|| {
            let git_root = std::process::Command::new("git")
                .args(["rev-parse", "--show-toplevel"])
                .current_dir(&project_root)
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| {
                    String::from_utf8(o.stdout)
                        .ok()
                        .map(|s| s.trim().to_string())
                });
            git_root.and_then(|root| {
                let sid = current_session_id.as_deref();
                let summaries =
                    astra_services::session_workspace::list_sessions_by_git_root(&root, sid, 5);
                if summaries.is_empty() {
                    None
                } else {
                    let ctx = astra_services::session_workspace::format_project_context(&summaries);
                    if ctx.is_empty() { None } else { Some(ctx) }
                }
            })
        })
        .clone();

    let mut state = AgenticLoopState {
        observation_journal: Default::default(),
        tool_ledger_receipt: Default::default(),
        messages,
        run_transcript_capture: None,
        volatile_pending: Vec::new(),
        recent_rounds: Vec::new(),
        tool_results: Vec::new(),
        session_memory_state: Default::default(),
        current_session_id,
        current_run_id: Some(parent_turn_run_id.clone()),
        current_run_owner_generation: None,
        inference_purpose: astra_turn_types::InferencePurpose::PrimaryAgent,
        context_manifest_pool: None,
        context_manifest_user_id: persist_session_artifacts.then_some(current_user_id),
        context_manifest_model_name: model_for_policy.map(str::to_string),
        runtime_manifest,
        recursion_depth: 0,
        final_text: String::new(),
        final_text_streamed: false,
        final_output_ready_notified: false,
        total_prompt: 0,
        total_completion: 0,
        total_cache_read: 0,
        total_cache_creation: 0,
        total_tool_calls: 0,
        last_finish_reason: None,
        total_observation_tool_calls: 0,
        has_any_usage: false,
        max_turns,
        remaining_turns: max_turns,
        agentic_turn_budget: task_profile.agentic_turn_budget,
        budget_is_explicit: false,
        current_round_index: 0,
        llm_rounds_completed: 0,
        last_request_message_count: None,
        turn_guard,
        budget_policy: None,
        restricted_tools: initial_restricted,
        boosted_tools: HashSet::new(),
        widen_selection_pending: false,
        step_recorder,
        idempotency_cache: p.idempotency_cache.unwrap_or_default(),
        semantic_dedup: SemanticDedup::new(
            astra_runtime::semantic_dedup::DEFAULT_SIMILARITY_THRESHOLD,
        ),
        call_counts: HashMap::new(),
        max_identical_tool_calls: resolved_tool_policy.max_identical_tool_calls,
        max_tools_per_turn: resolved_tool_policy.max_tools_per_turn,
        repeated_cache_hit_suppression: resolved_tool_policy.repeated_cache_hit_suppression,
        max_consecutive_empty_name: resolved_tool_policy.max_consecutive_empty_name,
        stall: StallTrackingState {
            workspace_observation_quarantine: p.workspace_observation_quarantine.clone(),
            work_unit_observations: Default::default(),
            active_work_registry: None,
            turn_sigs: Vec::new(),
            turn_tool_names: Vec::new(),
            events: Vec::new(),
            verdict_events: Vec::new(),
            last_heavy_checkpoint: None,
            tool_call_records: Vec::new(),
            server_terminal_unverified: false,
            execution_escalation_advisory_emitted: false,
            work_evidence_advisory_emitted: false,
            parallel_batching_advisory_emitted: false,
            repetition_advisory_emitted: false,
            introspection_count: 0,
            cache_waste_advisory_emitted: false,
            active_policy_feedback: Default::default(),
            runtime_policy_evaluation: Default::default(),
            nudge_count: 0,
            circuit_breaker: astra_turn_core::loop_circuit_breaker::LoopCircuitBreaker::new(
                circuit_breaker_config,
            ),
            guardrail_tuner: astra_runtime::config_admin::guardrail::GuardrailTuner::default(),
            guardrail_tuner_records_cursor: 0,
        },
        telemetry: TelemetryState {
            explain_turns: Vec::new(),
            first_ttft_ms: None,
            all_tools_used: HashSet::new(),
            authoritative_llm_rounds: None,
            server_summary_run_ids: HashSet::new(),
            server_summary_llm_rounds: 0,
            server_summary_tool_calls: 0,
            server_summary_observation_tool_calls: 0,
            server_summary_tools_used: HashSet::new(),
            local_usage_attempts: 0,
            local_usage_provider_reported: 0,
            local_usage_unavailable: 0,
            server_summary_usage_attempts: 0,
            server_summary_usage_provider_reported: 0,
            server_summary_usage_unavailable: 0,
            server_record_gap_observed: false,
            terminal_execution_authority: Some(
                astra_runtime::turn::agentic_loop::host::TerminalExecutionAuthority::EdgeLedger,
            ),
            first_selection_report: None,
            first_budget_pressure: 0.0,
            first_context_assembly_ms: None,
            first_memoria_ms: None,
            first_round_prompt_tokens: None,
            max_round_prompt_tokens: None,
            all_selected_skills: Vec::new(),
            observability_session: p.observability_session.clone(),
            observability_hub: p.observability_hub.clone(),
            turn_trace_collector: None,
            evaluation_persistence: None,
            context_trace_persistence: None,
            promotion_events: Vec::new(),
            pending_context_assembly_trace: None,
            completed_turns_for_tuning: 0,
            initial_skill_selector_shortlist: None,
        },
        skills: SkillState {
            registry_for_activation: Some(Arc::clone(&p.unified_skill_registry)),
            resolver: skill_resolver,
            executor: skill_executor,
            quality_tracker: p.skill_quality_tracker.clone(),
            quality_tracker_baseline: p.skill_quality_tracker.clone(),
            improvement_tracker: astra_skills::improvement::ImprovementTracker::new(),
            discovered: discovered_skills,
            tool_event_hooks: astra_skills::hooks::load_tool_event_hooks(&project_root),
            session_event_hooks: astra_skills::hooks::load_session_event_hooks(&project_root),
            listing_message: None,
            invoked: std::collections::HashMap::new(),
            ..Default::default()
        },
        hooks: StopHookState {
            stop_hooks: hook_sets.stop_hooks,
            stop_hook_runs: 0,
            teammate_idle_hooks: hook_sets.teammate_idle_hooks,
            teammate_idle_hook_runs: 0,
            workspace_root_hint: Some(project_root.to_string_lossy().into_owned()),
            forward_headers: std::collections::HashMap::new(),
            admitted_model_execution: None,
            completion_settlement: Default::default(),
        },
        messaging: MessagingState {
            mailbox: root_mailbox,
            ack_tracker: None,
            metrics: p.messaging_metrics.clone(),
            progress_emitter: None,
            ..Default::default()
        },
        user_intents: Default::default(),
        cancellation: CancellationState {
            flag: None,
            pause_flag: None,
            token: p.cancel_token.clone(),
            execution_lease_lost: None,
            resolved_origin: None,
        },
        error_recovery: ErrorRecoveryState {
            consecutive_same_error: 0,
            last_error_category: None,
        },
        provider_adaptation: Default::default(),
        run_control: p.run_control.clone(),
        pipeline_session: Some({
            let config = astra_turn_core::pipeline_config::PipelineConfig::default();
            let session_current_date =
                astra_runtime::turn::session_current_date::resolve_session_current_date(
                    p.session_id.unwrap_or(""),
                );
            astra_turn_core::pipeline_session_serde::restore_or_new_with_current_date(
                config,
                p.pipeline_state.as_ref(),
                &session_current_date,
            )
        }),
        message: p.message.to_string(),
        user_intent: p.user_intent.to_string(),
        recent_tools: p.recent_tools.to_vec(),
        activated_deferred_tool_names: host.executor.activated_deferred_tool_names(),
        has_prior_assistant_turn: false,
        turn_intent: None,
        task_profile,
        last_turn_policy: astra_runtime::turn::agentic_loop::host::TurnInteractionPolicy::default(),
        api: p.api.clone(),
        api_token: p.token.to_string(),
        delegation_engine: p.delegation_engine,
        delegations_this_turn: 0,
        delegation_chain: Vec::new(),
        self_agent_id: "tui_session".to_string(),
        project_context,
        checkpoint_gate: None,
        last_llm_context_manifest_trace: None,
        rate_limit_cooldown: Default::default(),
        data_snapshot_provider: None,
        last_composite_snapshot: None,
        last_measured_prompt_tokens: None,
        consecutive_context_window_errors: p.consecutive_context_window_errors,
        compaction_effectiveness: restored_compaction_effectiveness(p.compaction_state.as_ref()),
        pinned_tool_schema_tokens: always_load_schema_tokens,
        sticky_tool_schemas: Vec::new(),
        max_turn_input_tokens: effective_max_turn_input_tokens,
        budget_wrapup_injected: false,
        context_compression_triggered: false,
        canonical_rewrite_state: Default::default(),
        budget_wrapup_ignored_rounds: 0,
        compact_tier_applied: astra_turn_core::compaction_types::CompactionTier::Normal,
        skill_produced_output: false,
        thinking: astra_turn_core::thinking_config::ThinkingConfig::Off,
        permission_context: Some(root_permission_context),
        permission_handler: None,
        tactical_adapter: None,
        step_signal_collector: None,
        tool_budget_override: None,
        recent_tactical_actions: Vec::new(),
        runtime_tool_executor: None,
        interruption: None,
        session_facts: Default::default(),
        // Canonical Server execution is the sole per-turn memory producer.
        memory_extraction_service: None,
        compact_strategy: astra_turn_core::microcompact::CompactStrategy::from_provider_and_model(
            p.provider, p.model,
        ),
        approval_overrides: initial_approval_overrides,
        confidence_trend: Default::default(),
        last_confidence_diagnosis: None,
        session_turn: current_session_turn,
        canonical_turn_chain_id: Some(parent_turn_run_id.clone()),
        root_user_query_event_id: Some(
            p.stream_json_emitter
                .as_ref()
                .map(|emitter| emitter.user_query_event_id().to_string())
                .unwrap_or_else(|| uuid::Uuid::now_v7().to_string()),
        ),
        turn_event_buffer: None,
        harness: {
            #[cfg(feature = "harness")]
            {
                match p.harness_sink.as_ref() {
                    Some(sink) => {
                        let sink_for_kernel =
                            sink.clone() as std::sync::Arc<dyn astra_harness::SnapshotSink>;
                        let base_kernel = std::sync::Arc::new(match p.benchmark_profile {
                            Some(profile) => astra_harness::StandardKernel::with_profile(
                                sink_for_kernel,
                                profile,
                            ),
                            None => astra_harness::StandardKernel::with_default_verifiers(
                                sink_for_kernel,
                            ),
                        });
                        let session_id = p.session_id.map(|s| s.to_string());
                        let recording = if let Some(ref trace_arc) = p.harness_trace {
                            // Share SessionState's trace Arc so /inspect reads live data
                            if let Ok(mut t) = trace_arc.write() {
                                t.session_id = session_id.clone();
                            }
                            std::sync::Arc::new(astra_harness::RecordingKernel::with_trace(
                                base_kernel,
                                trace_arc.clone(),
                            ))
                        } else {
                            std::sync::Arc::new(astra_harness::RecordingKernel::new(
                                base_kernel,
                                session_id,
                            ))
                        };
                        astra_runtime::turn::harness_adapter::HarnessSlot::new(
                            recording as std::sync::Arc<dyn astra_harness::HarnessKernel>,
                            sink.clone() as std::sync::Arc<dyn astra_harness::SnapshotSink>,
                        )
                    }
                    None => astra_runtime::turn::harness_adapter::HarnessSlot::empty(),
                }
            }
            #[cfg(not(feature = "harness"))]
            {
                astra_runtime::turn::harness_adapter::HarnessSlot::empty()
            }
        },
    };

    let input_work_unit_observations = p
        .input_work_unit_observations
        .iter()
        .filter(|observation| observation.is_valid())
        .cloned()
        .collect::<Vec<_>>();
    for observation in &input_work_unit_observations {
        state.observe_work_unit(observation);
    }
    if !input_work_unit_observations.is_empty() {
        state.push_volatile_payload(
            astra_runtime::turn::agentic_loop::host::VolatileKind::ActiveWorkSnapshot,
            serde_json::json!({
                "schema": "active_work_snapshot.v1",
                "work_unit_observations": input_work_unit_observations,
                "instruction": "This is producer-owned session work state at the current model boundary. Use canonical group IDs. A fanout is one work unit: do not copy child IDs or infer group completion from individual events. Non-terminal work makes any current answer a partial snapshot, not a completion report.",
                "authority": "runtime_producer",
            }),
        );
    }

    // Root and child runs capture the same append-only transcript lane. This
    // must happen before the first model boundary; final prompt history can
    // later be compacted and is not a valid recovery source for this data.
    if let Some(item) = root_initial_transcript_item {
        state.begin_run_transcript_capture(std::iter::once(item));
    }

    // ─── Run the runtime loop ────────────────────────────────────────────
    // Stop the early spinner — the per-turn prep spinner inside execute_turn takes over.
    if let Some(s) = early_spinner {
        s.stop_clear();
    }
    let loop_result = run_agentic_loop_with_host(&mut host, &mut state).await;
    let loop_failure = match loop_result {
        Err(error) => Some(error.to_string()),
        Ok(_) => host
            .output_transport_failure
            .map(|failure| failure.message().to_string()),
    };
    if let Some(error) = loop_failure {
        deferred_activation_state::snapshot_from_executor(
            &mut p.activated_deferred_tool_names,
            host.executor.as_ref(),
        );
        finalize_root_mailbox(p.root_mailbox_slot, &mut state.messaging.mailbox).await;
        if let Some(shared) = p.discovered_skills {
            *shared = state.skills.discovered.clone();
        }
        let (tool_calls_count, tools_used) = resolved_tool_metrics(
            state.total_tool_calls,
            state.telemetry.all_tools_used.iter().cloned(),
            &state.stall.tool_call_records,
        );
        let tool_outcomes = astra_services::session_journal::ToolOutcomeSummary::from_records(
            &state.stall.tool_call_records,
        );
        return Err(crate::TurnFailure {
            error,
            partial: crate::PartialTurnData {
                tool_call_records: std::mem::take(&mut state.stall.tool_call_records),
                tools_used,
                stall_events: std::mem::take(&mut state.stall.events),
                verdict_events: std::mem::take(&mut state.stall.verdict_events),
                prompt_tokens: state.total_prompt,
                completion_tokens: state.total_completion,
                cache_read_tokens: state.total_cache_read,
                cache_creation_tokens: state.total_cache_creation,
                tool_calls_count,
                llm_rounds: Some(state.llm_rounds_completed),
                token_usage_coverage: state.token_usage_coverage(),
                tool_outcomes: Some(tool_outcomes),
                applied_user_intents: state
                    .user_intents
                    .applied_user_intents()
                    .iter()
                    .map(
                        |input| crate::cli::stream::streaming_types::AppliedStreamUserIntent {
                            intent_id: input.intent_id.clone(),
                            delivery: input.delivery,
                            status: input.status,
                            event_index: input.event_index,
                            content: input.content.clone(),
                        },
                    )
                    .collect(),
                session_id: state.current_session_id.clone(),
                run_id: state.current_run_id.clone(),
                last_heavy_checkpoint: state.stall.last_heavy_checkpoint.take(),
                partial_text: std::mem::take(&mut state.final_text),
                run_transcript_messages: state.take_run_transcript_capture(),
                remote_cancel_required: host.remote_cancel_required,
                remote_cancel_run_id: host.remote_cancel_run_id,
                output_transport_failure: host.output_transport_failure,
                interruption: state.interruption.as_ref().map(|i| i.to_json()),
            },
        });
    }

    // The runtime publishes `AssistantOutputSettled` at the terminal-output
    // boundary, before its slow durable settlement. Everything below is
    // post-loop local projection work.
    let post_loop_projection_started_at = Instant::now();

    // ─── Finalize ────────────────────────────────────────────────────────
    deferred_activation_state::snapshot_from_executor(
        &mut p.activated_deferred_tool_names,
        host.executor.as_ref(),
    );
    let activated_deferred_tool_names = host.executor.activated_deferred_tool_names();
    // Merge skill quality data back to session-scoped tracker
    *p.skill_quality_tracker = state.skills.quality_tracker.clone();
    if let Some(shared) = p.discovered_skills {
        *shared = state.skills.discovered.clone();
    }
    finalize_root_mailbox(p.root_mailbox_slot, &mut state.messaging.mailbox).await;

    eprint_stream_loop_sidecars(StreamLoopSidecarEprint {
        explain: p.explain,
        quiet: p.render_policy.is_silent(),
        verbose_mode: p.verbose_mode,
        start,
        model: p.model,
        explain_turns: &state.telemetry.explain_turns,
        pending_context_assembly_trace: state
            .telemetry
            .pending_context_assembly_trace
            .as_ref()
            .map(|(_, trace_json)| trace_json),
        tool_call_records: &state.stall.tool_call_records,
        assistant_output: &state.final_text,
        ttft_ms: state.telemetry.first_ttft_ms,
        context_ms: state.telemetry.first_context_assembly_ms,
        memoria_ms: state.telemetry.first_memoria_ms,
        llm_rounds: Some(state.llm_rounds_completed),
        verdict_events: &state.stall.verdict_events,
        has_any_usage: state.has_any_usage,
        total_prompt: state.total_prompt,
        total_cache_read: state.total_cache_read,
        total_cache_creation: state.total_cache_creation,
        total_completion: state.total_completion,
        current_session_id: state.current_session_id.as_deref(),
    });

    // `turn_intent` is populated only by the strict LLM judge. Preserve
    // unknown as `None`; post-turn consumers must not reclassify user text.
    let routing_domain_hint = state
        .turn_intent
        .as_ref()
        .and_then(|intent| intent.domain)
        .map(|domain| domain.as_str().to_string());

    // Forward explain / verdict to TUI stream (if wired).
    if let Some(ref tx) = p.stream_event_tx {
        let explain_turns = state.telemetry.explain_turns.clone();
        let verdict_events = state.stall.verdict_events.clone();
        let _ = tx.send(StreamEvent::ExplainReport(explain_turns)).await;
        if p.explain != ExplainMode::Off {
            let tool_count = resolved_tool_metrics(
                0,
                std::iter::empty::<String>(),
                &state.stall.tool_call_records,
            )
            .0;
            let meta = crate::explain_dag::ExplainTurnMeta {
                turn_label: None,
                duration_ms: Some(start.elapsed().as_millis() as u64),
                ttft_ms: state.telemetry.first_ttft_ms,
                context_ms: state.telemetry.first_context_assembly_ms,
                memoria_ms: state.telemetry.first_memoria_ms,
                total_llm_ms: None,
                total_tool_ms: Some(
                    state
                        .stall
                        .tool_call_records
                        .iter()
                        .filter(|record| !record.is_synthetic_placeholder())
                        .map(|record| record.ms)
                        .sum(),
                ),
                prompt_tokens: Some(state.total_prompt),
                completion_tokens: Some(state.total_completion),
                cache_read_tokens: Some(state.total_cache_read),
                cache_creation_tokens: Some(state.total_cache_creation),
                tool_count: Some(tool_count),
                llm_rounds: Some(state.llm_rounds_completed),
                routing_domain_hint: routing_domain_hint.clone(),
                assistant_output: Some(&state.final_text),
                tool_call_records: &state.stall.tool_call_records,
                visible_tools: Vec::new(),
            };
            if let Some(text) = explain_reports::render_explain_report_text(
                &state.telemetry.explain_turns,
                Some(&meta),
                state
                    .telemetry
                    .pending_context_assembly_trace
                    .as_ref()
                    .map(|(_, trace_json)| trace_json),
                p.explain == ExplainMode::Verbose,
            ) {
                let _ = tx.send(StreamEvent::ExplainText(text)).await;
            }
        }
        let _ = tx.send(StreamEvent::VerdictReport(verdict_events)).await;
    }

    let applied_user_intents = state
        .user_intents
        .applied_user_intents()
        .iter()
        .map(
            |input| crate::cli::stream::streaming_types::AppliedStreamUserIntent {
                intent_id: input.intent_id.clone(),
                delivery: input.delivery,
                status: input.status,
                event_index: input.event_index,
                content: input.content.clone(),
            },
        )
        .collect::<Vec<_>>();
    let run_transcript_messages = state.take_run_transcript_capture();
    let final_messages = std::mem::take(&mut state.messages);

    let token_usage_coverage = state.token_usage_coverage();
    let tool_ledger_aggregate = state.tool_ledger_receipt.canonical_aggregate();
    let result = build_stream_result(StreamResultBuild {
        tool_health_entries: p.tool_health_entries,
        session_id: state.current_session_id,
        run_id: state.current_run_id,
        full_text: state.final_text,
        prompt_tokens: state.total_prompt,
        completion_tokens: state.total_completion,
        cache_read_tokens: state.total_cache_read,
        cache_creation_tokens: state.total_cache_creation,
        tool_calls_count: state.total_tool_calls,
        tool_ledger_aggregate,
        first_surface_report: state.telemetry.first_selection_report,
        selected_skills: state.telemetry.all_selected_skills,
        tools_used: state.telemetry.all_tools_used,
        activated_deferred_tool_names,
        tool_call_records: state.stall.tool_call_records,
        budget_pressure: state.telemetry.first_budget_pressure,
        stall_events: state.stall.events,
        verdict_events: state.stall.verdict_events,
        step_recorder: &state.step_recorder,
        turn_guard: &state.turn_guard,
        last_heavy_checkpoint: state.stall.last_heavy_checkpoint,
        ttft_ms: state.telemetry.first_ttft_ms,
        context_ms: state.telemetry.first_context_assembly_ms,
        memoria_ms: state.telemetry.first_memoria_ms,
        routing_domain_hint,
        entity_learn_skipped_no_domain: false,
        pending_context_assembly_trace: state.telemetry.pending_context_assembly_trace,
        turn_observability_events: state
            .turn_event_buffer
            .as_mut()
            .map(|b| b.drain())
            .unwrap_or_default(),
        llm_rounds: state
            .telemetry
            .authoritative_llm_rounds
            .or(Some(state.llm_rounds_completed)),
        token_usage_coverage,
        interruption: state.interruption.as_ref().map(|i| i.to_json()),
        server_terminal_unverified: state.stall.server_terminal_unverified,
        server_terminal_authoritative: state
            .telemetry
            .terminal_execution_authority
            .is_some_and(|authority| {
                authority
                    == astra_runtime::turn::agentic_loop::host::TerminalExecutionAuthority::RemoteServer
            }),
        tool_record_coverage_partial: state.telemetry.server_record_gap_observed,
        final_messages,
        run_transcript_messages,
        applied_user_intents,
    });
    tracing::debug!(
        target: "astra_cli::turn_settlement",
        post_loop_projection_ms = post_loop_projection_started_at.elapsed().as_millis() as u64,
        "completed stream-loop projections after runtime loop settled"
    );
    Ok(result)
}

fn normalize_turn_model(model: Option<&str>) -> Option<&str> {
    astra_core::model_override::normalize_model_override(model)
}

fn require_selected_turn_model<'a>(
    model: Option<&'a str>,
    session_id: Option<&str>,
    turn_index: u32,
) -> Option<&'a str> {
    let Some(model) = model else {
        tracing::warn!(
            target: "astra_cli::model_selection",
            reason = "missing_model_selection",
            session_id = ?session_id,
            turn_index,
            "missing concrete model selection; refusing to start turn before opening SSE stream"
        );
        return None;
    };
    Some(model)
}

fn record_missing_model_selection_failure(
    session_id: Option<&str>,
    turn_index: u32,
    message: &str,
    duration_ms: u64,
) {
    let error = astra_core::model_override::missing_model_selection_error();
    tracing::error!(
        target: "astra_cli::model_selection",
        error_kind = error.kind.as_str(),
        session_id = ?session_id,
        turn_index,
        "turn failed before SSE stream because no selectable Offering was resolved"
    );
    let Some(session_id) = session_id.filter(|sid| !sid.is_empty()) else {
        return;
    };
    let event = missing_model_selection_journal_event(session_id, turn_index, message, duration_ms);
    crate::cli::cli_config::cli_utils::append_session_journal_event_or_warn(
        session_id,
        &event,
        "sse_loop:missing_model_selection",
    );
}

fn missing_model_selection_journal_event(
    session_id: &str,
    turn_index: u32,
    message: &str,
    duration_ms: u64,
) -> astra_services::session_journal::JournalEvent {
    let error = astra_core::model_override::missing_model_selection_error();
    let mut event = astra_services::session_journal::JournalEvent::turn_error(
        Some(session_id),
        turn_index,
        None,
        message,
        &error.to_string(),
        duration_ms,
    );
    event.metadata = Some(json!({
        "error_kind": error.kind.as_str(),
        "reason": "missing_model_selection",
        "model_selection": null,
        "model_resolution": {
            "resolved": false,
            "source": "cli_turn_selection",
        },
    }));
    event
}

fn missing_model_selection_turn_failure(session_id: Option<&str>) -> crate::TurnFailure {
    crate::TurnFailure {
        error: astra_core::model_override::missing_model_selection_error().to_string(),
        partial: crate::PartialTurnData {
            session_id: session_id.map(str::to_string),
            ..Default::default()
        },
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
enum TurnMessageLoadError {
    #[error(
        "canonical prompt history is unavailable for a non-empty session; repair or resume from the canonical journal instead of using lossy display pairs"
    )]
    CanonicalHistoryRequired,
}

fn load_turn_messages(
    pre_loaded_messages: Option<Vec<serde_json::Value>>,
    history: &[(String, String)],
    current_message: &str,
) -> Result<Vec<serde_json::Value>, TurnMessageLoadError> {
    if let Some(msgs) = pre_loaded_messages {
        crate::cli::history_work::record_json_history(
            astra_core::history_work::HistoryWorkSite::CliPromptContinuationSanitization,
            &msgs,
        );
        let (mut msgs, invalid_turn_semantics_dropped) = astra_turn_core::prompt_facing::
            recover_canonical_continuation_messages_with_turn_semantics(msgs);
        if invalid_turn_semantics_dropped > 0 {
            tracing::warn!(
                invalid_turn_semantics_dropped,
                "dropped invalid typed turn metadata while sanitizing preloaded continuation"
            );
        }
        msgs.push(json!({"role": "user", "content": current_message}));
        return Ok(msgs);
    }
    if !history.is_empty() {
        return Err(TurnMessageLoadError::CanonicalHistoryRequired);
    }
    Ok(vec![json!({"role": "user", "content": current_message})])
}

#[cfg(test)]
mod tests {
    use super::{
        TurnMessageLoadError, circuit_breaker_config_from_tool_policy, detect_turn_hook_sets,
        load_turn_messages, missing_model_selection_journal_event,
        missing_model_selection_turn_failure, non_tty_output_failure, normalize_turn_model,
        refresh_root_permission_context, require_selected_turn_model,
        restored_compaction_effectiveness, root_permission_context_handle,
        step_recorder_for_cli_turn,
    };
    use crate::cli::permission_manager::{PermissionManager, PermissionMode};
    use astra_runtime::turn::permission_gate::{PermissionCheckResult, check_tool_permission};
    use astra_turn_core::chat_turn_heuristics::{
        TaskComplexity, TaskExecutionProfile, infer_task_execution_profile,
    };
    use serde_json::json;
    use std::path::Path;
    use std::sync::Arc;
    use tempfile::tempdir;

    fn mutating_profile() -> TaskExecutionProfile {
        TaskExecutionProfile::from_structured_intent(true, false, TaskComplexity::Standard)
    }

    #[test]
    fn final_non_tty_output_failure_reenters_the_turn_failure_boundary() {
        use crate::cli::stream::output_sink::StdoutState;
        use crate::cli::stream::streaming_types::OutputTransportFailure;

        assert_eq!(non_tty_output_failure(false, StdoutState::Open), None);
        assert_eq!(
            non_tty_output_failure(false, StdoutState::Closed),
            Some(OutputTransportFailure::Closed)
        );
        assert_eq!(
            non_tty_output_failure(false, StdoutState::Failed),
            Some(OutputTransportFailure::Failed)
        );
        assert_eq!(non_tty_output_failure(true, StdoutState::Closed), None);
    }

    #[test]
    fn circuit_breaker_config_uses_runtime_config_defaults() {
        let cfg = circuit_breaker_config_from_tool_policy(
            &astra_config::runtime_config::ToolPolicyConfig::default(),
        );

        assert_eq!(cfg.stall_threshold, 6);
        assert_eq!(cfg.repetition_threshold, 3);
        assert_eq!(cfg.read_only_stall_threshold, 12);
        // `0` in user config means "use default", NOT the BreakerConfig sentinel "unbounded".
        assert_eq!(cfg.max_introspect_emissions, 3);
        assert_eq!(cfg.half_open_patience, 2);
        assert_eq!(cfg.absolute_max_rounds, 1000);
    }

    #[test]
    fn preloaded_turn_messages_drop_stale_pre_compaction_goal_and_trace() {
        let preloaded = vec![
            json!({"role": "user", "content": "3 agents review everything"}),
            json!({"role": "system", "content": "arbitrary compaction boundary text", "_compact_boundary": true}),
            json!({"role": "user", "content": "不要review啊！"}),
            json!({"role": "assistant", "reasoning_content": "I may review anyway"}),
            json!({"role": "tool", "tool_call_id": "c1", "content": "No matches"}),
            json!({"role": "assistant", "content": "明白，不做 review。"}),
        ];

        let messages = load_turn_messages(Some(preloaded), &[], "修复刚才发现的问题")
            .expect("canonical messages");

        assert_eq!(messages.last().unwrap()["content"], "修复刚才发现的问题");
        assert!(
            messages
                .iter()
                .all(|msg| msg["role"] != "tool" && msg.get("reasoning_content").is_none())
        );
        assert!(
            messages
                .iter()
                .all(|msg| !msg["content"].as_str().unwrap_or("").contains("3 agents"))
        );
    }

    #[test]
    fn preloaded_turn_messages_with_corrupt_semantics_still_drop_runtime_scaffolding() {
        let corrupt_field = astra_turn_types::USER_TURN_SEMANTICS_FIELD;
        let preloaded = vec![
            json!({"role": "user", "content": "stale objective"}),
            json!({"role": "system", "content": "boundary", "_compact_boundary": true}),
            astra_turn_types::runtime_owned_message(
                "system",
                "runtime-only retry instruction",
                astra_turn_types::RuntimeMessageDelivery::EphemeralControl,
            ),
            json!({
                "role": "user",
                "content": "current objective",
                (corrupt_field): {
                    "schema_version": "invalid",
                    "objective_relation": "replace"
                }
            }),
            json!({"role": "tool", "tool_call_id": "orphan", "content": "orphan result"}),
            json!({"role": "assistant", "content": "current answer"}),
        ];

        let messages = load_turn_messages(Some(preloaded), &[], "continue safely")
            .expect("canonical messages");

        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["content"], "current objective");
        assert!(messages[0].get(corrupt_field).is_none());
        assert_eq!(messages[1]["content"], "current answer");
        assert_eq!(messages[2]["content"], "continue safely");
    }

    #[test]
    fn preloaded_turn_messages_keep_complete_tool_evidence_for_context_optimizer() {
        let preloaded = vec![
            json!({"role": "user", "content": "inspect Cargo.toml"}),
            json!({
                "role": "assistant",
                "tool_calls": [{
                    "id": "call-1",
                    "type": "function",
                    "function": {"name": "read_file", "arguments": "{}"}
                }]
            }),
            json!({
                "role": "tool",
                "tool_call_id": "call-1",
                "content": "canonical evidence"
            }),
            json!({"role": "assistant", "content": "done"}),
        ];

        let messages = load_turn_messages(Some(preloaded), &[], "what did you find?")
            .expect("canonical messages");

        assert_eq!(messages.len(), 5);
        assert_eq!(messages[1]["tool_calls"][0]["id"], "call-1");
        assert_eq!(messages[2]["tool_call_id"], "call-1");
        assert_eq!(messages[2]["content"], "canonical evidence");
        assert_eq!(messages[4]["content"], "what did you find?");
    }

    #[test]
    fn display_pair_history_cannot_become_a_healthy_prompt_source() {
        let error = load_turn_messages(
            None,
            &[("old user".to_string(), "old assistant".to_string())],
            "continue",
        )
        .expect_err("lossy display pairs must not enter the model prompt");
        assert_eq!(error, TurnMessageLoadError::CanonicalHistoryRequired);

        let fresh = load_turn_messages(None, &[], "new session").expect("fresh prompt");
        assert_eq!(
            fresh,
            vec![json!({"role": "user", "content": "new session"})]
        );
    }

    #[test]
    fn missing_model_selection_preflight_is_classified_and_session_scoped() {
        assert!(
            require_selected_turn_model(None, Some("sess-missing-model"), 3).is_none(),
            "missing model must fail before opening the SSE stream"
        );
        let failure = missing_model_selection_turn_failure(Some("sess-missing-model"));

        let classified = astra_core::ClassifiedError::from(failure.error.clone());
        assert_eq!(
            classified.kind,
            astra_core::ErrorKind::MissingModelSelection
        );
        assert_eq!(
            failure.partial.session_id.as_deref(),
            Some("sess-missing-model")
        );
        assert!(failure.error.contains("default_model"), "{}", failure.error);
    }

    #[test]
    fn missing_model_selection_journal_event_is_observable_and_fail_closed() {
        let event = missing_model_selection_journal_event(
            "sess-missing-model",
            3,
            "why is there no model?",
            17,
        );

        assert_eq!(
            event.event_type,
            astra_services::session_journal::JournalEventType::TurnError
        );
        assert_eq!(event.session_id.as_deref(), Some("sess-missing-model"));
        assert_eq!(event.turn, Some(3));
        assert_eq!(event.model, None);
        assert!(
            event
                .error
                .as_deref()
                .unwrap_or("")
                .contains("default_model")
        );
        let metadata = event.metadata.expect("metadata");
        assert_eq!(metadata["error_kind"], "missing_model_selection");
        assert_eq!(metadata["model_resolution"]["resolved"], false);
        assert_eq!(metadata["model_resolution"]["source"], "cli_turn_selection");
    }

    #[tokio::test]
    async fn root_permission_context_auto_mode_allows_visible_tools() {
        let mut manager = PermissionManager::new(false);
        manager.set_mode(PermissionMode::Auto);
        let ctx = root_permission_context_handle(&manager);

        let result = check_tool_permission(
            "git",
            Some(r#"{"action":"status"}"#),
            Some(&ctx),
            None,
            std::time::Duration::from_millis(1),
        )
        .await;

        assert!(
            matches!(
                result,
                PermissionCheckResult::Allowed | PermissionCheckResult::AllowedImplicit { .. }
            ),
            "auto mode must install a root permission context that allows visible tools, got {result:?}"
        );
    }

    #[tokio::test]
    async fn refresh_root_permission_context_updates_existing_handle() {
        let mut manager = PermissionManager::new(false);
        let mut handle = Some(root_permission_context_handle(&manager));
        let original = handle.as_ref().unwrap().clone();

        manager.set_mode(PermissionMode::Auto);
        refresh_root_permission_context(&mut handle, &manager).await;

        let refreshed = handle.as_ref().unwrap();
        assert!(
            Arc::ptr_eq(&original, refreshed),
            "refresh must update the existing context so any permission handler keeps seeing the current policy"
        );

        let result = check_tool_permission(
            "bash",
            Some(r#"{"command":"git status"}"#),
            Some(refreshed),
            None,
            std::time::Duration::from_millis(1),
        )
        .await;

        assert!(
            matches!(
                result,
                PermissionCheckResult::Allowed | PermissionCheckResult::AllowedImplicit { .. }
            ),
            "refreshed auto context should allow bash, got {result:?}"
        );
    }

    #[tokio::test]
    async fn refresh_root_permission_context_preserves_runtime_telemetry() {
        // Regression: `refresh_root_permission_context` used to wholesale
        // replace the context (`*existing.write().await = latest`), which
        // wiped runtime telemetry (tools_blocked, recent_denials, ...) and
        // broke the self-model feedback loop. It must now merge policy only.
        let mut manager = PermissionManager::new(false);
        manager.set_mode(PermissionMode::Auto);
        let mut handle = Some(root_permission_context_handle(&manager));

        // Seed runtime telemetry + a session deny rule into the existing handle.
        {
            let mut guard = handle.as_ref().unwrap().write().await;
            guard.record_blocked_tool_with_reason("bash", Some("seeded denial"));
            guard.apply_update(&astra_turn_core::permission::types::PermissionUpdate::deny(
                astra_turn_core::permission::types::PermissionRule::tool("dangerous_tool"),
            ));
        }

        // Mutate policy (mode flip) and refresh — the bug used to wipe the
        // telemetry + session deny above.
        manager.set_mode(PermissionMode::Prompt);
        refresh_root_permission_context(&mut handle, &manager).await;

        let guard = handle.as_ref().unwrap().read().await;
        assert_eq!(
            guard.telemetry().tools_blocked,
            1,
            "refresh must preserve runtime tools_blocked telemetry, not wipe it"
        );
        assert!(
            guard
                .telemetry()
                .recent_denials
                .iter()
                .any(|name| name == "bash"),
            "refresh must preserve recent_denials, not wipe it"
        );
        assert!(
            guard.is_denied("dangerous_tool", None),
            "refresh must preserve in-session deny rules, not wipe them"
        );
        assert_eq!(
            guard.mode(),
            PermissionMode::Prompt,
            "refresh must still apply the fresh policy (mode flip)"
        );
    }

    #[tokio::test]
    async fn prompt_mode_approval_refresh_allows_same_tool_call() {
        let mut manager = PermissionManager::new(false);
        manager.set_mode(PermissionMode::Prompt);
        let mut handle = Some(root_permission_context_handle(&manager));
        let args = json!({"command": "cargo test"});
        let args_str = serde_json::to_string(&args).unwrap();

        let before = check_tool_permission(
            "bash",
            Some(&args_str),
            handle.as_ref(),
            None,
            std::time::Duration::from_millis(1),
        )
        .await;
        assert!(
            matches!(before, PermissionCheckResult::Denied { .. }),
            "prompt mode should require approval before a bash execution, got {before:?}"
        );

        manager.record_approval("bash", Some(&args), true);
        refresh_root_permission_context(&mut handle, &manager).await;

        let after = check_tool_permission(
            "bash",
            Some(&args_str),
            handle.as_ref(),
            None,
            std::time::Duration::from_millis(1),
        )
        .await;
        assert!(
            matches!(
                after,
                PermissionCheckResult::Allowed | PermissionCheckResult::AllowedImplicit { .. }
            ),
            "prompt approval must refresh into the runtime permission context, got {after:?}"
        );
    }

    #[tokio::test]
    async fn accept_edits_root_context_allows_edits_but_not_bash() {
        let mut manager = PermissionManager::new(false);
        manager.set_mode(PermissionMode::AcceptEdits);
        let handle = root_permission_context_handle(&manager);

        let write_args = serde_json::to_string(&json!({
            "path": "src/lib.rs",
            "content": "pub fn demo() {}\n",
        }))
        .unwrap();
        let write_result = check_tool_permission(
            "write_file",
            Some(&write_args),
            Some(&handle),
            None,
            std::time::Duration::from_millis(1),
        )
        .await;
        assert!(
            matches!(
                write_result,
                PermissionCheckResult::Allowed | PermissionCheckResult::AllowedImplicit { .. }
            ),
            "accept_edits should allow workspace file edits, got {write_result:?}"
        );

        let bash_args = serde_json::to_string(&json!({"command": "cargo test"})).unwrap();
        let bash_result = check_tool_permission(
            "bash",
            Some(&bash_args),
            Some(&handle),
            None,
            std::time::Duration::from_millis(1),
        )
        .await;
        assert!(
            matches!(bash_result, PermissionCheckResult::Denied { .. }),
            "accept_edits should still require approval for bash, got {bash_result:?}"
        );
    }

    #[test]
    fn restored_compaction_effectiveness_decodes_checkpoint_tracker() {
        let tracker = restored_compaction_effectiveness(Some(&json!({
            "last_tokens_freed": 4000,
            "last_was_insufficient": true,
            "cumulative_tokens_freed": 15000,
            "attempt_count": 3,
            "consecutive_futile_attempts": 2,
        })));

        assert_eq!(tracker.last_tokens_freed, 4000);
        assert!(tracker.last_was_insufficient);
        assert_eq!(tracker.cumulative_tokens_freed, 15000);
        assert_eq!(tracker.attempt_count, 3);
        assert_eq!(tracker.consecutive_futile_attempts, 2);
    }

    #[test]
    fn turn_model_normalization_drops_symbolic_default_override() {
        assert_eq!(normalize_turn_model(None), None);
        assert_eq!(normalize_turn_model(Some(" default ")), None);
        assert_eq!(
            normalize_turn_model(Some("MiniMax-M2.7")),
            Some("MiniMax-M2.7")
        );
    }

    #[test]
    fn cli_step_recorder_uses_runtime_run_id_as_trace_identity() {
        let recorder =
            step_recorder_for_cli_turn("user-1", Some("session-1"), "run-parent-visible");
        let summary = recorder.summary();
        assert_eq!(summary.session_id, "session-1");
        assert_eq!(summary.task_id, "run-parent-visible");
        assert!(
            !summary.task_id.starts_with("chat-"),
            "StepRecorder identity must not diverge from AgenticLoopState.current_run_id"
        );

        let ephemeral = step_recorder_for_cli_turn("user-1", None, "run-parent-ephemeral");
        let summary = ephemeral.summary();
        assert_eq!(summary.session_id, "ephemeral");
        assert_eq!(summary.task_id, "run-parent-ephemeral");
        assert!(
            !summary.task_id.starts_with("chat-"),
            "ephemeral CLI sessions still use the runtime run id for trace identity"
        );
    }

    #[test]
    fn circuit_breaker_config_uses_runtime_config_overrides_with_floors() {
        let tool_policy = astra_config::runtime_config::ToolPolicyConfig {
            circuit_breaker_stall_threshold: 1,
            circuit_breaker_repetition_threshold: 7,
            circuit_breaker_read_only_stall_threshold: 2,
            // user=0 → effective_*() returns default (3); floor is 1
            circuit_breaker_max_introspect_emissions: 0,
            circuit_breaker_half_open_patience: 5,
            circuit_breaker_absolute_max_rounds: 10,
            ..Default::default()
        };

        let cfg = circuit_breaker_config_from_tool_policy(&tool_policy);

        // stall: resolve(1, 6, 3) = max(1, 3) = 3 (floored)
        assert_eq!(cfg.stall_threshold, 3);
        // repetition: resolve(7, 3, 2) = max(7, 2) = 7
        assert_eq!(cfg.repetition_threshold, 7);
        // read_only: resolve(2, 12, 4) = max(2, 4) = 4 (floored)
        assert_eq!(cfg.read_only_stall_threshold, 4);
        // introspect: resolve(0, 3, 1) = 3 (default)
        assert_eq!(cfg.max_introspect_emissions, 3);
        assert_eq!(cfg.half_open_patience, 5);
        // absolute: resolve(10, 200, 20) = max(10, 20) = 20 (floored)
        assert_eq!(cfg.absolute_max_rounds, 20);
    }

    #[test]
    fn circuit_breaker_config_introspect_floor_is_one() {
        // user supplies explicit value=1 (at the floor) — should pass through unchanged
        let tool_policy = astra_config::runtime_config::ToolPolicyConfig {
            circuit_breaker_max_introspect_emissions: 1,
            ..Default::default()
        };
        let cfg = circuit_breaker_config_from_tool_policy(&tool_policy);
        assert_eq!(cfg.max_introspect_emissions, 1);
    }

    #[test]
    fn read_only_requests_skip_stop_hooks() {
        let s = detect_turn_hook_sets(
            Path::new("."),
            infer_task_execution_profile("review 最新的commit"),
            false,
        );
        assert!(s.stop_hooks.is_empty());
        let s = detect_turn_hook_sets(
            Path::new("."),
            infer_task_execution_profile("explain this diff"),
            false,
        );
        assert!(s.stop_hooks.is_empty());
    }

    #[test]
    fn mutating_requests_keep_stop_hooks() {
        let s = detect_turn_hook_sets(Path::new("."), mutating_profile(), false);
        // Smart hook returns a single "verify-changes" entry (if project detected)
        // or empty (if no project markers in cwd)
        let _ = s.stop_hooks;
    }

    #[test]
    fn plan_subtask_ignores_when_stop_uses_task_completed() {
        let dir = tempdir().unwrap();
        let mo = dir.path().join(".astra");
        std::fs::create_dir_all(&mo).unwrap();
        std::fs::write(
            mo.join("stop-hooks.yaml"),
            r#"version: 1
auto_detect: false
hooks:
  - label: global
    command: echo stop
    when: stop
  - label: sub
    command: echo task
    when: task_completed
"#,
        )
        .unwrap();
        let prof = mutating_profile();
        let s = detect_turn_hook_sets(dir.path(), prof, true);
        assert_eq!(s.stop_hooks.len(), 1);
        assert_eq!(s.stop_hooks[0].label, "sub");
    }

    #[test]
    fn plan_subtask_empty_without_task_completed_hooks() {
        let dir = tempdir().unwrap();
        let mo = dir.path().join(".astra");
        std::fs::create_dir_all(&mo).unwrap();
        std::fs::write(
            mo.join("stop-hooks.yaml"),
            "version: 1\nauto_detect: false\nhooks:\n  - label: only_stop\n    command: true\n    when: stop\n",
        )
        .unwrap();
        let prof = mutating_profile();
        let s = detect_turn_hook_sets(dir.path(), prof, true);
        assert!(s.stop_hooks.is_empty());
    }

    #[test]
    fn teammate_idle_hooks_loaded_alongside_stop() {
        let dir = tempdir().unwrap();
        let mo = dir.path().join(".astra");
        std::fs::create_dir_all(&mo).unwrap();
        std::fs::write(
            mo.join("stop-hooks.yaml"),
            r#"version: 1
auto_detect: false
hooks:
  - label: fin
    command: cargo check
    when: stop
  - label: after_delegate
    command: ./scripts/sync-check.sh
    when: teammate_idle
"#,
        )
        .unwrap();
        let s = detect_turn_hook_sets(dir.path(), mutating_profile(), false);
        assert_eq!(s.stop_hooks.len(), 1);
        assert_eq!(s.teammate_idle_hooks.len(), 1);
        assert_eq!(s.teammate_idle_hooks[0].label, "after_delegate");
    }

    #[test]
    fn declarative_stop_hooks_apply_on_read_only_turn() {
        let dir = tempdir().unwrap();
        let mo = dir.path().join(".astra");
        std::fs::create_dir_all(&mo).unwrap();
        std::fs::write(
            mo.join("stop-hooks.yaml"),
            "version: 1\nauto_detect: false\nhooks:\n  - label: audit\n    command: ./scripts/audit.sh\n",
        )
        .unwrap();
        let s = detect_turn_hook_sets(
            dir.path(),
            infer_task_execution_profile("explain this file"),
            false,
        );
        assert_eq!(s.stop_hooks.len(), 1);
        assert_eq!(s.stop_hooks[0].label, "audit");
    }
}
