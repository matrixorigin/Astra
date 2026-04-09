//! CLI implementation of SpawnAgentExecutor.
//!
//! Runs spawned agents using the same agentic loop infrastructure as delegation.

use async_trait::async_trait;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use astra_core::SkillSearchSettings;
use astra_runtime::{
    orchestration::{SpawnAgentExecutor, SpawnRunConfig, SpawnRunResult},
    pipeline::step_protocol::InMemoryIdempotencyCache,
    pipeline::step_recorder::StepRecorder,
    semantic_dedup::SemanticDedup,
    turn::agentic_loop_host::{AgenticLoopState, run_agentic_loop_with_host, AgenticLoopOutcome},
    turn::chat_turn_heuristics::infer_task_execution_profile,
    turn::tool_schema_prune::openai_tool_names_from_schemas,
    turn::turn_guard::TurnGuard,
};
use serde_json::json;
use std::collections::HashMap;

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
}

#[async_trait]
impl SpawnAgentExecutor for CliSpawnAgentExecutor {
    async fn execute(&self, config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
        let all_schemas = edge_tools::all_tool_schemas();
        let valid_tool_names = openai_tool_names_from_schemas(&all_schemas);

        let perm_manager = super::permission_manager::PermissionManager::with_project_mode(
            self.permission_mode,
            &self.project_root,
        );

        // Use the working directory from config (may be a worktree)
        let effective_root = config.working_dir.clone();

        let mut host = SubRunHost {
            api: self.api.clone(),
            token: self.token.clone(),
            model: Some(config.model.clone()),
            project_root: effective_root.clone(),
            executor: edge_tools::ToolExecutor::new(&effective_root)
                .with_cloud(self.api.api_origin(), &self.token),
            all_schemas,
            valid_tool_names: valid_tool_names.clone(),
            perm_manager,
            max_completion_tokens: None,
            effort: None,
            agent_type: Some(config.agent_type.clone()),
            cancel_token: self.cancel_token.clone(),
            skill_resolver: self.skill_resolver.clone(),
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

        let max_turns = config.max_turns as usize;

        let mut state = AgenticLoopState {
            messages,
            tool_results: Vec::new(),
            current_session_id: Some(subrun_session_id),
            current_run_id: Some(config.run_id.clone()),
            final_text: String::new(),
            total_prompt: 0,
            total_completion: 0,
            total_cache_read: 0,
            total_cache_creation: 0,
            total_tool_calls: 0,
            has_any_usage: false,
            max_turns,
            remaining_turns: max_turns,
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
            first_selector_confidence: None,
            first_selector_strategy: None,
            selector_tokens_in: 0,
            selector_tokens_out: 0,
            all_selected_skills: Vec::new(),
            message: config.task.clone(),
            recent_tools: Vec::new(),
            task_profile,
            api: self.api.clone(),
            api_token: self.token.clone(),
            cancel_flag: None,
            cancel_token: self.cancel_token.clone(),
            delegation_engine: None, // no recursive delegation from spawned agents
            skill_registry_for_activation: None,
            skill_resolver: self.skill_resolver.clone(),
            skill_executor: None,
            skill_model_override: None,
            skill_effort: None,
            skill_agent_type: None,
            skill_allowed_tools: None,
            skill_sandbox_policy: None,
            skill_quality_tracker: astra_runtime::skills::quality::SkillQualityTracker::new(),
            skill_improvement_tracker: astra_runtime::skills::improvement::ImprovementTracker::new(),
            pinned_skills: HashSet::new(),
            discovered_skills: HashSet::new(),
            skill_search: self.skill_search.clone(),
            tool_event_hooks: astra_runtime::skills::hooks::load_tool_event_hooks(&effective_root),
            session_event_hooks: astra_runtime::skills::hooks::load_session_event_hooks(&effective_root),
            stop_hooks: Vec::new(),
            stop_hook_runs: 0,
            teammate_idle_hooks: Vec::new(),
            teammate_idle_hook_runs: 0,
            workspace_root_hint: Some(effective_root.to_string_lossy().into_owned()),
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
            invoked_skills: HashMap::new(),
            recent_file_reads: Vec::new(),
            mailbox: config.mailbox,
            ack_tracker: None,
            dead_letter_queue: None,
            messaging_metrics: None,
            progress_emitter: None,
        };

        let loop_result = run_agentic_loop_with_host(&mut host, &mut state).await;

        let tool_calls = state.total_tool_calls as u32;
        let agent_id = config.agent_id.clone();
        let run_id = config.run_id;
        let prompt_tokens = state.total_prompt;
        let completion_tokens = state.total_completion;

        match loop_result {
            Ok(AgenticLoopOutcome::Completed) => Ok(SpawnRunResult {
                agent_id,
                run_id,
                status: "completed".to_string(),
                output: Some(state.final_text),
                error: None,
                prompt_tokens,
                completion_tokens,
                tool_calls,
            }),
            Ok(AgenticLoopOutcome::Cancelled) => Ok(SpawnRunResult {
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
            }),
            Ok(AgenticLoopOutcome::Error(error)) => Ok(SpawnRunResult {
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
            }),
            Ok(AgenticLoopOutcome::Waiting(reason)) => Ok(SpawnRunResult {
                agent_id,
                run_id,
                status: "waiting".to_string(),
                output: Some(reason),
                error: None,
                prompt_tokens,
                completion_tokens,
                tool_calls,
            }),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_executor_creation() {
        let api = astra_thin_client::ThinClient::new("http://test", None);
        let executor = CliSpawnAgentExecutor::new(
            api,
            "token".to_string(),
            PathBuf::from("/tmp"),
            PermissionMode::Ask,
            None,
        );
        assert!(executor.skill_resolver.is_none());
    }
}
