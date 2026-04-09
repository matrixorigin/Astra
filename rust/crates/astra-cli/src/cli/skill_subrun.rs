//! Fork skill sub-run executor for the CLI.
//!
//! When a skill has `execution_context: Fork`, the agentic loop delegates
//! execution to an [`IsolatedSkillExecutor`] which wraps a [`SkillSubRunExecutor`].
//! This module provides the CLI implementation that runs a separate agentic loop
//! using the same API and tool infrastructure as the parent conversation.

use async_trait::async_trait;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use astra_core::SkillSearchSettings;
use astra_runtime::skills::executor::isolated::{SkillSubRunExecutor, SubRunResult};
use astra_runtime::{
    pipeline::step_protocol::InMemoryIdempotencyCache,
    pipeline::step_recorder::StepRecorder,
    semantic_dedup::SemanticDedup,
    turn::agentic_headless_round::HeadlessStderrStyle,
    turn::agentic_loop_host::{
        AgenticLoopHost, AgenticLoopState, HostTurnResult, run_agentic_loop_with_host,
    },
    turn::chat_turn_heuristics::infer_task_execution_profile,
    turn::chat_turn_payload::{
        ChatTurnBasePayloadInput, chat_turn_base_payload, set_payload_tool_results_if_non_empty,
    },
    turn::tool_schema_prune::openai_tool_names_from_schemas,
    turn::turn_guard::TurnGuard,
};
use serde_json::{Value, json};

use super::edge_tools;
use super::effects::ChatTurnPrepLineGuard;
use super::permission_manager::{PermissionManager, PermissionMode};
use super::stream_render::{EdgeSseContext, consume_turn_sse};

const SUBRUN_MAX_TURNS: usize = 25;

// ─── SubRunHost ──────────────────────────────────────────────────────────────

/// Minimal agentic loop host for fork sub-runs.
///
/// Owns all resources so it doesn't borrow from a parent scope. Runs in quiet
/// mode with all terminal rendering suppressed.
///
/// Shared between skill sub-runs and delegate sub-runs.
pub(crate) struct SubRunHost {
    pub(crate) api: astra_thin_client::ThinClient,
    pub(crate) token: String,
    pub(crate) model: Option<String>,
    pub(crate) project_root: PathBuf,
    pub(crate) executor: edge_tools::ToolExecutor,
    pub(crate) all_schemas: Vec<Value>,
    pub(crate) valid_tool_names: HashSet<String>,
    pub(crate) perm_manager: PermissionManager,
    /// Per-response completion token limit from the skill manifest.
    pub(crate) max_completion_tokens: Option<u32>,
    /// Effort level from the skill manifest.
    pub(crate) effort: Option<String>,
    /// Agent type hint from the skill manifest.
    pub(crate) agent_type: Option<String>,
    /// Parent cancellation token — so Ctrl+C / stop propagates into sub-runs.
    pub(crate) cancel_token: Option<std::sync::Arc<tokio_util::sync::CancellationToken>>,
    /// Same resolver as the parent loop so `skill` tool calls during the SSE edge
    /// round resolve (nested skills run inline — sub-run has no `skill_executor`).
    pub(crate) skill_resolver: Option<Arc<dyn astra_runtime::turn::skill_tool::SkillResolver>>,
}

pub(crate) fn persist_failed_subrun(state: &mut AgenticLoopState, error: &str) -> String {
    let failure_output = if state.final_text.trim().is_empty() {
        format!("[sub-run failed] {error}")
    } else {
        format!(
            "[sub-run failed] {error}\n\nPartial output:\n{}",
            state.final_text
        )
    };
    state.final_text = failure_output.clone();
    state.messages.push(json!({
        "role": "assistant",
        "content": failure_output.clone(),
    }));
    state.step_recorder.end_turn(false);

    let summary = state.step_recorder.summary();
    let blocked_tools = state
        .turn_guard
        .health
        .deprioritized_tools()
        .iter()
        .map(|tool| tool.to_string())
        .collect::<Vec<_>>();
    if let Some(heavy) = state.step_recorder.build_heavy_checkpoint(
        &state.messages,
        state.max_turn_input_tokens,
        state.remaining_turns as u32,
        &blocked_tools,
        &state.recent_tools,
    ) {
        let checkpoint =
            astra_runtime::pipeline::step_protocol::StepCheckpoint::Heavy(Box::new(heavy));
        let _ = astra_runtime::pipeline::step_checkpoint::write_step_checkpoint(
            &summary.session_id,
            summary.checkpoints,
            &checkpoint,
        );
    }

    failure_output
}

#[async_trait]
impl AgenticLoopHost for SubRunHost {
    async fn execute_turn(
        &mut self,
        state: &mut AgenticLoopState,
    ) -> Result<HostTurnResult, String> {
        self.executor
            .set_send_message_context(state.mailbox.as_ref().map(|mailbox| {
                crate::edge_tools::agent_messaging::SendMessageRuntimeContext {
                    agent_id: mailbox.address.agent_id.clone(),
                    router: mailbox.router(),
                    metrics: state.messaging_metrics.clone(),
                    delegation_id: mailbox.delegation_id.clone(),
                }
            }));

        let effective_model = state
            .skill_model_override
            .as_deref()
            .or(self.model.as_deref());

        let mut payload = chat_turn_base_payload(ChatTurnBasePayloadInput {
            messages: &state.messages,
            session_id: state.current_session_id.as_deref(),
            agent_id: Some("astra-cli"),
            model: effective_model,
            explain_verbose: false,
            explain_on: false,
            edge_executor_id: "subrun",
            capabilities: astra_thin_client::builtin_capability_preset(),
            project_root: &self.project_root,
            git_branch: None,
            thinking_budget_tokens: None,
        });

        if let Some(max_tokens) = self.max_completion_tokens {
            payload["max_tokens"] = json!(max_tokens);
        }

        if let Some(ref effort) = self.effort {
            payload["effort"] = json!(effort);
        }

        if let Some(ref agent_type) = self.agent_type {
            payload["agent_type"] = json!(agent_type);
        }
        payload["skill_search"] =
            serde_json::to_value(&state.skill_search).unwrap_or_else(|_| json!({}));

        // Attach tool schemas directly (no selector).
        astra_runtime::turn::agentic_prepare_payload::apply_selector_hints_then_attach_filtered_edge_tools(
            &mut payload,
            self.all_schemas.clone(),
            &mut state.restricted_tools,
            None,  // no selection report
            0.5,   // neutral confidence
            "",    // no learned context
            None,  // no learned task type
        );

        set_payload_tool_results_if_non_empty(&mut payload, &state.tool_results);

        let resp = self
            .api
            .post_chat_turn_retry_429(&self.token, &payload, 3, true)
            .await
            .map_err(|e| e.to_string())?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            return Err(format!("Sub-run API error {status}: {body}"));
        }

        let edge_ctx = EdgeSseContext {
            api: &self.api,
            token: &self.token,
            executor_id: "subrun",
            executor: &mut self.executor,
            quiet: true,
            suppress_intermediate_output: true,
            hide_streaming_assistant_text: false,
            show_reasoning_preview: false,
            perm_manager: Some(&mut self.perm_manager),
            cancel_token: self.cancel_token.as_ref().map(|t| t.as_ref()),
            stream_event_tx: None,
            approval_request_tx: None,
            skill_resolver: self.skill_resolver.clone(),
        };

        let prep_line = ChatTurnPrepLineGuard::maybe_start(false, None);
        let turn = consume_turn_sse(
            prep_line,
            resp,
            false, // render_md
            80,    // term_width
            true,  // quiet
            true,  // suppress_intermediate_output
            Some(edge_ctx),
            0,                                              // pre_clear_lines
            self.cancel_token.as_ref().map(|t| t.as_ref()), // propagate parent cancel
        )
        .await;

        Ok(HostTurnResult {
            accum: turn.core,
            ttft_ms: turn.ttft_ms,
            edge_tool_round: turn.edge_tool_round,
        })
    }

    fn emit_headless_line(&mut self, _style: HeadlessStderrStyle, _line: String) {
        // Sub-runs are completely silent.
    }

    fn is_quiet(&self) -> bool {
        true
    }

    fn valid_tool_names(&self) -> &HashSet<String> {
        &self.valid_tool_names
    }

    fn inject_tool_schema(&mut self, schema: Value) {
        if let Some(name) = schema
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
        {
            let name_owned = name.to_string();
            self.valid_tool_names.insert(name_owned.clone());
            if let Some(existing) = self.all_schemas.iter_mut().find(|tool| {
                tool.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    == Some(name_owned.as_str())
            }) {
                *existing = schema;
            } else {
                self.all_schemas.push(schema);
            }
        }
    }
}

// ─── CliSkillSubRunExecutor ──────────────────────────────────────────────────

/// CLI implementation of [`SkillSubRunExecutor`].
///
/// Creates a fresh [`SubRunHost`] and [`AgenticLoopState`] for each sub-run,
/// then runs [`run_agentic_loop_with_host`] to completion.
///
/// Inherits the parent session's full [`PermissionMode`] (Auto/Prompt/Deny)
/// so that fork sub-runs enforce the same approval policy as the parent.
pub(crate) struct CliSkillSubRunExecutor {
    api: astra_thin_client::ThinClient,
    token: String,
    default_model: Option<String>,
    project_root: PathBuf,
    /// Full permission mode inherited from the parent session.
    permission_mode: PermissionMode,
    /// Parent cancellation token — propagated so Ctrl+C / stop interrupts subruns.
    cancel_token: Option<std::sync::Arc<tokio_util::sync::CancellationToken>>,
    /// Skill resolver inherited from parent — enables nested skill invocations.
    skill_resolver: Option<std::sync::Arc<dyn astra_runtime::turn::skill_tool::SkillResolver>>,
    /// Same surfacing policy as the parent loop / session state.
    skill_search: SkillSearchSettings,
}

impl CliSkillSubRunExecutor {
    pub fn new(
        api: astra_thin_client::ThinClient,
        token: String,
        default_model: Option<String>,
        project_root: PathBuf,
        permission_mode: PermissionMode,
        cancel_token: Option<std::sync::Arc<tokio_util::sync::CancellationToken>>,
    ) -> Self {
        Self {
            api,
            token,
            default_model,
            project_root,
            permission_mode,
            cancel_token,
            skill_resolver: None,
            skill_search: SkillSearchSettings::default(),
        }
    }

    /// Attach a skill resolver so sub-runs can invoke other skills.
    pub fn with_skill_resolver(
        mut self,
        resolver: Option<std::sync::Arc<dyn astra_runtime::turn::skill_tool::SkillResolver>>,
    ) -> Self {
        self.skill_resolver = resolver;
        self
    }

    pub fn with_skill_search(mut self, skill_search: SkillSearchSettings) -> Self {
        self.skill_search = skill_search;
        self
    }
}

#[async_trait]
impl SkillSubRunExecutor for CliSkillSubRunExecutor {
    async fn execute_skill_subrun(
        &self,
        skill_name: &str,
        instructions: &str,
        task_context: &str,
        model: Option<&str>,
        max_tokens: Option<u32>,
        allowed_tools: &[String],
        effort: Option<&str>,
        agent_type: Option<&str>,
    ) -> Result<SubRunResult, String> {
        let effective_model = model
            .map(String::from)
            .or_else(|| self.default_model.clone());

        let all_schemas = edge_tools::all_tool_schemas();
        let valid_tool_names = openai_tool_names_from_schemas(&all_schemas);

        let perm_manager =
            PermissionManager::with_project_mode(self.permission_mode, &self.project_root);

        let mut host = SubRunHost {
            api: self.api.clone(),
            token: self.token.clone(),
            model: effective_model,
            project_root: self.project_root.clone(),
            executor: edge_tools::ToolExecutor::new(&self.project_root)
                .with_cloud(self.api.api_origin(), &self.token),
            all_schemas,
            valid_tool_names: valid_tool_names.clone(),
            perm_manager,
            max_completion_tokens: max_tokens,
            effort: effort.map(String::from),
            agent_type: agent_type.map(String::from),
            cancel_token: self.cancel_token.clone(),
            skill_resolver: self.skill_resolver.clone(),
        };

        let messages = vec![
            json!({
                "role": "system",
                "content": instructions,
            }),
            json!({
                "role": "user",
                "content": if task_context.is_empty() {
                    format!("Execute the skill '{skill_name}' according to the instructions above.")
                } else {
                    task_context.to_string()
                },
            }),
        ];

        let restricted_tools: HashSet<String> = if allowed_tools.is_empty() {
            HashSet::new()
        } else {
            let allowed: HashSet<&str> = allowed_tools.iter().map(|s| s.as_str()).collect();
            valid_tool_names
                .iter()
                .filter(|name| {
                    !allowed.contains(name.as_str())
                        && name.as_str() != astra_runtime::turn::skill_tool::SKILL_TOOL_NAME
                        && name.as_str()
                            != astra_runtime::turn::skill_tool::DISCOVER_SKILLS_TOOL_NAME
                })
                .cloned()
                .collect()
        };

        let task_profile = infer_task_execution_profile(task_context);
        let safe_name = astra_runtime::skills::loader::sanitize_for_path(skill_name);
        let subrun_session_id = format!(
            "subrun-{}-{}",
            safe_name,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_micros()
        );
        let step_recorder = StepRecorder::with_persistence(
            &subrun_session_id,
            &format!("{}-task", subrun_session_id),
        );

        let mut state = AgenticLoopState {
            messages,
            tool_results: Vec::new(),
            current_session_id: None,
            current_run_id: None,
            final_text: String::new(),
            total_prompt: 0,
            total_completion: 0,
            total_cache_read: 0,
            total_cache_creation: 0,
            total_tool_calls: 0,
            has_any_usage: false,
            max_turns: SUBRUN_MAX_TURNS,
            remaining_turns: SUBRUN_MAX_TURNS,
            turn_guard: TurnGuard::with_profile(task_profile),
            restricted_tools,
            step_recorder,
            idempotency_cache: InMemoryIdempotencyCache::new(),
            semantic_dedup: SemanticDedup::new(
                astra_runtime::semantic_dedup::DEFAULT_SIMILARITY_THRESHOLD,
            ),
            turn_sigs: Vec::new(),
            turn_tool_names: Vec::new(),
            stall_events: Vec::new(),
            intent_tool_turns: Vec::new(),
            verdict_events: Vec::new(),
            last_heavy_checkpoint: None,
            tool_call_records: Vec::new(),
            forced_factual_retry: false,
            explain_turns: Vec::new(),
            first_ttft_ms: None,
            all_tools_used: HashSet::new(),
            first_selection_report: None,
            first_budget_pressure: 0.0,
            first_context_assembly_ms: None,
            first_memoria_ms: None,
            first_selector_ms: None,
            first_selector_strategy: None,
            first_selector_confidence: None,
            selector_tokens_in: 0,
            selector_tokens_out: 0,
            all_selected_skills: Vec::new(),
            message: task_context.to_string(),
            recent_tools: Vec::new(),
            task_profile: infer_task_execution_profile(task_context),
            api: self.api.clone(),
            api_token: self.token.clone(),
            cancel_flag: None,
            cancel_token: self.cancel_token.clone(),
            delegation_engine: None,
            skill_registry_for_activation: None,
            skill_resolver: self.skill_resolver.clone(),
            skill_executor: None, // no recursive forking
            skill_model_override: None,
            skill_effort: None,
            skill_agent_type: None,
            skill_allowed_tools: None,
            skill_sandbox_policy: None,
            // Fresh tracker — sub-run metrics are intentionally not propagated
            // back to the parent session's tracker.
            skill_quality_tracker: astra_runtime::skills::quality::SkillQualityTracker::new(),
            skill_improvement_tracker: astra_runtime::skills::improvement::ImprovementTracker::new(
            ),
            pinned_skills: std::collections::HashSet::new(),
            discovered_skills: std::collections::HashSet::new(),
            skill_search: self.skill_search.clone(),
            tool_event_hooks: astra_runtime::skills::hooks::load_tool_event_hooks(
                &self.project_root,
            ),
            session_event_hooks: astra_runtime::skills::hooks::load_session_event_hooks(
                &self.project_root,
            ),
            stop_hooks: Vec::new(),
            stop_hook_runs: 0,
            teammate_idle_hooks: Vec::new(),
            teammate_idle_hook_runs: 0,
            workspace_root_hint: Some(self.project_root.to_string_lossy().into_owned()),
            consecutive_same_error: 0,
            last_error_category: None,
            checkpoint_gate: None,
            data_snapshot_provider: None,
            last_composite_snapshot: None,
            last_measured_prompt_tokens: None,
            consecutive_context_window_errors: 0,
            max_turn_input_tokens: astra_core::RuntimeLimits::global().max_turn_input_tokens,
            budget_wrapup_injected: false,
            thinking_budget_tokens: None,
            skill_listing_message: None,
            invoked_skills: std::collections::HashMap::new(),
            recent_file_reads: Vec::new(),
            mailbox: None,
            ack_tracker: None,
            dead_letter_queue: None,
            messaging_metrics: None,
            progress_emitter: None,
            permission_context: None,
            permission_handler: None,
        };

        if let Err(err) = run_agentic_loop_with_host(&mut host, &mut state).await {
            let failure_output = persist_failed_subrun(&mut state, &err);
            return Err(failure_output);
        }

        let turns = (SUBRUN_MAX_TURNS - state.remaining_turns) as u32;
        let tokens_used = (state.total_prompt + state.total_completion) as u32;

        Ok(SubRunResult {
            output: state.final_text,
            tokens_used,
            turns,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subrun_host_is_quiet() {
        let root = PathBuf::from(".");
        let host = SubRunHost {
            api: astra_thin_client::ThinClient::new("http://unused", None).unwrap(),
            token: String::new(),
            model: None,
            project_root: root.clone(),
            executor: edge_tools::ToolExecutor::new(&root),
            all_schemas: Vec::new(),
            valid_tool_names: HashSet::new(),
            perm_manager: PermissionManager::with_project(true, &root),
            max_completion_tokens: None,
            effort: None,
            agent_type: None,
            cancel_token: None,
            skill_resolver: None,
        };
        assert!(host.is_quiet());
    }

    #[test]
    fn subrun_host_inject_tool_schema() {
        let root = PathBuf::from(".");
        let mut host = SubRunHost {
            api: astra_thin_client::ThinClient::new("http://unused", None).unwrap(),
            token: String::new(),
            model: None,
            project_root: root.clone(),
            executor: edge_tools::ToolExecutor::new(&root),
            all_schemas: Vec::new(),
            valid_tool_names: HashSet::new(),
            perm_manager: PermissionManager::with_project(true, &root),
            max_completion_tokens: None,
            effort: None,
            agent_type: None,
            cancel_token: None,
            skill_resolver: None,
        };
        let schema = json!({
            "type": "function",
            "function": {
                "name": "test_tool",
                "description": "A test tool",
            }
        });
        host.inject_tool_schema(schema);
        assert!(host.valid_tool_names.contains("test_tool"));
        assert_eq!(host.all_schemas.len(), 1);
    }
}
