//! CLI implementation of SpawnAgentExecutor.
//!
//! Runs spawned agents using the same agentic loop infrastructure as delegation.

use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use astra_core::SkillSearchSettings;
use astra_runtime::{
    orchestration::{PermissionSummary, SpawnAgentExecutor, SpawnRunConfig, SpawnRunResult},
    pipeline::step_protocol::InMemoryIdempotencyCache,
    pipeline::step_recorder::StepRecorder,
    semantic_dedup::SemanticDedup,
    turn::agentic_loop_finalization::run_agentic_loop_with_host,
    turn::agentic_loop_host::{
        AgenticLoopOutcome, AgenticLoopState, CancellationState, MessagingState, SkillState,
        StopHookState,
    },
    turn::chat_turn_heuristics::infer_task_execution_profile,
    turn::tool_schema_prune::openai_tool_names_from_schemas,
    turn::turn_guard::TurnGuard,
};
use serde_json::json;

use super::edge_tools;
use super::permission_manager::PermissionMode;
use super::skill_subrun::SubRunHost;

// ─── CliSpawnAgentExecutor ──────────────────────────────────────────────────

/// CLI implementation of [`SpawnAgentExecutor`].
///
/// Runs spawned agents using the same agentic loop as delegation,
/// but with agent-type-specific configuration (model, tools, prompts).
pub struct CliSpawnAgentExecutor {
    api: astra_thin_client::ThinClient,
    token: String,
    project_root: PathBuf,
    permission_mode: PermissionMode,
    cancel_token: Option<Arc<tokio_util::sync::CancellationToken>>,
    skill_resolver: Option<Arc<dyn astra_runtime::turn::skill_tool::SkillResolver>>,
    skill_search: SkillSearchSettings,
    active_session_id: Option<String>,
}

impl CliSpawnAgentExecutor {
    pub fn new(
        api: astra_thin_client::ThinClient,
        token: String,
        project_root: PathBuf,
        permission_mode: PermissionMode,
        cancel_token: Option<Arc<tokio_util::sync::CancellationToken>>,
    ) -> Self {
        Self {
            api,
            token,
            project_root,
            permission_mode,
            cancel_token,
            skill_resolver: None,
            skill_search: SkillSearchSettings::default(),
            active_session_id: None,
        }
    }

    pub fn with_skill_resolver(
        mut self,
        resolver: Option<Arc<dyn astra_runtime::turn::skill_tool::SkillResolver>>,
    ) -> Self {
        self.skill_resolver = resolver;
        self
    }

    pub fn with_skill_search(mut self, skill_search: SkillSearchSettings) -> Self {
        self.skill_search = skill_search;
        self
    }

    pub fn with_active_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.active_session_id = Some(session_id.into());
        self
    }
}

#[async_trait]
impl SpawnAgentExecutor for CliSpawnAgentExecutor {
    async fn execute(&self, config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
        let all_schemas = edge_tools::all_tool_schemas();
        let valid_tool_names = openai_tool_names_from_schemas(&all_schemas);

        // Create permission manager - use inherited permissions if available
        let perm_manager = if let Some(ref inherited) = config.inherited_permissions {
            super::permission_manager::PermissionManager::with_inherited(
                &self.project_root,
                inherited.clone(),
            )
        } else {
            super::permission_manager::PermissionManager::with_project_mode(
                self.permission_mode,
                &self.project_root,
            )
        };

        // Use the working directory from config (may be a worktree)
        let effective_root = config.working_dir.clone();

        let mut executor = edge_tools::ToolExecutor::new(&effective_root)
            .with_cloud(self.api.api_origin(), &self.token);
        if let Some(session_id) = self.active_session_id.as_deref() {
            executor = executor.with_active_session_id(session_id.to_string());
        }

        let mut host = SubRunHost {
            api: self.api.clone(),
            token: self.token.clone(),
            model: Some(config.model.clone()),
            project_root: effective_root.clone(),
            executor,
            all_schemas,
            valid_tool_names: valid_tool_names.clone(),
            perm_manager,
            max_completion_tokens: None,
            effort: None,
            agent_type: Some(config.agent_type.clone()),
            cancel_token: self.cancel_token.clone(),
            skill_resolver: self.skill_resolver.clone(),
            progress_tx: None,
            agent_id: config.agent_id.clone(),
            tool_cache: super::stream_render::EdgeToolCache::new(
                astra_runtime::runtime_config::RuntimeConfig::load()
                    .tool_selection
                    .effective_max_identical_calls(),
            ),
        };

        // Build system message from agent type definition
        let system_prompt = if config.system_prompt_addendum.is_empty() {
            format!(
                "You are '{}', a specialized sub-agent. Complete the task thoroughly.",
                config.agent_id
            )
        } else {
            format!(
                "You are '{}', a specialized sub-agent.\n\n{}\n\nComplete the task thoroughly.",
                config.agent_id, config.system_prompt_addendum
            )
        };

        let messages = vec![
            json!({ "role": "system", "content": system_prompt }),
            json!({ "role": "user", "content": config.task }),
        ];

        // Build restricted tools based on agent type's allowed_tools
        let restricted_tools: HashSet<String> = if config.allowed_tools.iter().any(|t| t == "*") {
            // All tools allowed
            HashSet::new()
        } else {
            // Only allow specified tools
            let allowed: HashSet<&str> = config.allowed_tools.iter().map(|s| s.as_str()).collect();
            valid_tool_names
                .iter()
                .filter(|name| !allowed.contains(name.as_str()))
                .cloned()
                .collect()
        };

        // Add edit/create to restricted if read_only
        let restricted_tools = if config.read_only {
            let mut restricted = restricted_tools;
            restricted.insert("edit".to_string());
            restricted.insert("create".to_string());
            restricted.insert("write_file".to_string());
            restricted.insert("str_replace".to_string());
            restricted
        } else {
            restricted_tools
        };

        let task_profile = infer_task_execution_profile(&config.task);
        let subrun_session_id = format!("spawn-{}-{}", config.run_id, config.agent_id);
        let step_recorder =
            StepRecorder::with_persistence(&subrun_session_id, &format!("{}-run", config.run_id));

        let start_time = std::time::Instant::now();
        let progress_emitter = config.progress_emitter.clone();
        let has_parent_permissions = config.parent_address.is_some();

        let max_turns = config.max_turns as usize;

        let mut state = AgenticLoopState {
            messages,
            tool_results: Vec::new(),
            current_session_id: Some(subrun_session_id),
            current_run_id: Some(config.run_id.clone()),
            recursion_depth: config.recursion_depth,
            final_text: String::new(),
            total_prompt: 0,
            total_completion: 0,
            total_cache_read: 0,
            total_cache_creation: 0,
            total_tool_calls: 0,
            total_evidence_tool_calls: 0,
            has_any_usage: false,
            max_turns,
            remaining_turns: max_turns,
            current_round_index: 0,
            turn_guard: TurnGuard::with_profile(task_profile),
            restricted_tools,
            boosted_tools: HashSet::new(),
            widen_selection_pending: false,
            step_recorder,
            idempotency_cache: InMemoryIdempotencyCache::new(),
            semantic_dedup: SemanticDedup::new(
                astra_runtime::semantic_dedup::DEFAULT_SIMILARITY_THRESHOLD,
            ),
            call_counts: HashMap::new(),
            max_identical_tool_calls: astra_runtime::runtime_config::RuntimeConfig::load()
                .tool_selection
                .effective_max_identical_calls(),
            max_tools_per_turn: astra_runtime::runtime_config::RuntimeConfig::load()
                .tool_selection
                .effective_max_tools_per_turn(),
            stall: Default::default(),
            telemetry: Default::default(),
            skills: SkillState {
                resolver: self.skill_resolver.clone(),
                quality_tracker: astra_runtime::skills::quality::SkillQualityTracker::new(),
                improvement_tracker: astra_runtime::skills::improvement::ImprovementTracker::new(),
                search: self.skill_search.clone(),
                tool_event_hooks: astra_runtime::skills::hooks::load_tool_event_hooks(
                    &effective_root,
                ),
                session_event_hooks: astra_runtime::skills::hooks::load_session_event_hooks(
                    &effective_root,
                ),
                ..Default::default()
            },
            hooks: StopHookState {
                workspace_root_hint: Some(effective_root.to_string_lossy().into_owned()),
                ..Default::default()
            },
            messaging: MessagingState {
                mailbox: config.mailbox,
                progress_emitter: config.progress_emitter,
                ..Default::default()
            },
            cancellation: CancellationState {
                flag: None,
                pause_flag: None,
                token: self.cancel_token.clone(),
            },
            error_recovery: Default::default(),
            message: config.task.clone(),
            recent_tools: Vec::new(),
            task_profile,
            last_turn_policy:
                astra_runtime::turn::agentic_loop_host::TurnInteractionPolicy::default(),
            api: self.api.clone(),
            api_token: self.token.clone(),
            delegation_engine: None,
            project_context: None,
            checkpoint_gate: None,
            evolution_service: None,
            rate_limit_cooldown: Default::default(),
            data_snapshot_provider: None,
            last_composite_snapshot: None,
            last_measured_prompt_tokens: None,
            consecutive_context_window_errors: 0,
            compaction_effectiveness: Default::default(),
            max_turn_input_tokens: astra_core::RuntimeLimits::global().max_turn_input_tokens,
            budget_wrapup_injected: false,
            skill_produced_output: false,
            max_cumulative_tokens: 0,
            thinking_budget_tokens: None,
            recent_file_reads: Vec::new(),
            permission_context: config.permission_context,
            permission_handler: None,
            tactical_adapter: None,
            step_signal_collector: None,
            tool_budget_override: None,
            pending_reflection_signals: Vec::new(),
            recent_tactical_actions: Vec::new(),
            server_tool_executor: None,
            interruption: None,
            session_facts: Default::default(),
            approval_overrides: None,
            confidence_trend: Default::default(),
            last_confidence_diagnosis: None,
            session_turn: 0,
            prefetch_injected: false,
            turn_event_buffer: None,
        };

        // Inherit skills from parent: pre-populate discovered skills
        if !config.inherited_skills.is_empty() {
            for skill_name in &config.inherited_skills {
                state.skills.discovered.insert(skill_name.clone());
            }
        }

        let loop_result = run_agentic_loop_with_host(&mut host, &mut state).await;

        let tool_calls = state.total_tool_calls as u32;
        let agent_id = config.agent_id.clone();
        let run_id = config.run_id;
        let prompt_tokens = state.total_prompt;
        let completion_tokens = state.total_completion;
        let duration_ms = start_time.elapsed().as_millis() as u64;
        let (permission_summary, permission_requests, permission_requests_approved, tools_blocked) =
            if let Some(ctx) = state.permission_context.as_ref() {
                let ctx_guard = ctx.read().await;
                let telemetry = ctx_guard.telemetry();
                let mode = match ctx_guard.mode() {
                    astra_runtime::orchestration::PermissionMode::Auto => "auto".to_string(),
                    astra_runtime::orchestration::PermissionMode::Prompt => "prompt".to_string(),
                    astra_runtime::orchestration::PermissionMode::Deny => "deny".to_string(),
                };
                (
                    Some(PermissionSummary {
                        mode,
                        allow_rules: ctx_guard.effective_allow_rule_count(),
                        deny_rules: ctx_guard.effective_deny_rule_count(),
                        has_parent: has_parent_permissions,
                        recent_denials: telemetry.recent_denials.clone(),
                    }),
                    telemetry.permission_requests,
                    telemetry.permission_requests_approved,
                    telemetry.tools_blocked,
                )
            } else {
                (None, 0, 0, 0)
            };

        match loop_result {
            Ok(AgenticLoopOutcome::Completed) => {
                // Emit completed event
                if let Some(ref emitter) = progress_emitter {
                    let summary = if state.final_text.len() > 100 {
                        format!(
                            "{}...",
                            state.final_text.chars().take(100).collect::<String>()
                        )
                    } else {
                        state.final_text.clone()
                    };
                    emitter.completed(
                        summary,
                        tool_calls,
                        (prompt_tokens, completion_tokens),
                        duration_ms,
                    );
                }
                Ok(SpawnRunResult {
                    agent_id,
                    run_id,
                    status: "completed".to_string(),
                    output: Some(state.final_text),
                    error: None,
                    prompt_tokens,
                    completion_tokens,
                    tool_calls,
                    permission_summary,
                    permission_requests,
                    permission_requests_approved,
                    tools_blocked,
                })
            }
            Ok(AgenticLoopOutcome::Cancelled) => {
                // Emit cancelled event
                if let Some(ref emitter) = progress_emitter {
                    emitter.cancelled("user cancellation");
                }
                Ok(SpawnRunResult {
                    agent_id,
                    run_id,
                    status: "cancelled".to_string(),
                    output: if state.final_text.is_empty() {
                        None
                    } else {
                        Some(state.final_text)
                    },
                    error: None,
                    prompt_tokens,
                    completion_tokens,
                    tool_calls,
                    permission_summary,
                    permission_requests,
                    permission_requests_approved,
                    tools_blocked,
                })
            }
            Ok(AgenticLoopOutcome::Error(error)) => {
                // Emit failed event
                if let Some(ref emitter) = progress_emitter {
                    emitter.failed(&error);
                }
                Ok(SpawnRunResult {
                    agent_id,
                    run_id,
                    status: "failed".to_string(),
                    output: if state.final_text.is_empty() {
                        None
                    } else {
                        Some(state.final_text)
                    },
                    error: Some(error),
                    prompt_tokens,
                    completion_tokens,
                    tool_calls,
                    permission_summary,
                    permission_requests,
                    permission_requests_approved,
                    tools_blocked,
                })
            }
            Ok(AgenticLoopOutcome::Waiting(reason)) => {
                // Emit idle event
                if let Some(ref emitter) = progress_emitter {
                    emitter.idle();
                }
                Ok(SpawnRunResult {
                    agent_id,
                    run_id,
                    status: "waiting".to_string(),
                    output: Some(reason),
                    error: None,
                    prompt_tokens,
                    completion_tokens,
                    tool_calls,
                    permission_summary,
                    permission_requests,
                    permission_requests_approved,
                    tools_blocked,
                })
            }
            Err(e) => {
                let msg = e.to_string();
                if let Some(ref emitter) = progress_emitter {
                    emitter.failed(&msg);
                }
                Err(msg)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_executor_creation() {
        let api = astra_thin_client::ThinClient::new("http://test", None).expect("test api");
        let executor = CliSpawnAgentExecutor::new(
            api,
            "token".to_string(),
            PathBuf::from("/tmp"),
            PermissionMode::Prompt,
            None,
        );
        assert!(executor.skill_resolver.is_none());
    }
}
