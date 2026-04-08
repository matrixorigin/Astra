//! Delegate sub-run executor for the CLI.
//!
//! When the LLM calls the `delegate` tool, the agentic loop's Step 3b
//! routes the call through a [`DelegationEngine`] which in turn invokes
//! a [`SubRunExecutor`].  This module provides the CLI implementation
//! that runs a real agentic loop for each sub-agent — using the same
//! API and tool infrastructure as the parent conversation.

use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use astra_core::SkillSearchSettings;
use astra_runtime::{
    pipeline::step_protocol::InMemoryIdempotencyCache,
    pipeline::step_recorder::StepRecorder,
    semantic_dedup::SemanticDedup,
    server::delegation_engine::{SubRunConfig, SubRunExecutor},
    turn::agentic_loop_host::{AgenticLoopState, run_agentic_loop_with_host},
    turn::chat_turn_heuristics::infer_task_execution_profile,
    turn::tool_schema_prune::openai_tool_names_from_schemas,
    turn::turn_guard::TurnGuard,
};
use astra_services::coordination::AgentResult;
use serde_json::json;

use super::edge_tools;
use super::permission_manager::PermissionMode;
use super::skill_subrun::SubRunHost;

const DELEGATE_MAX_TURNS: usize = 25;

// ─── CliDelegateSubRunExecutor ──────────────────────────────────────────────

/// CLI implementation of [`SubRunExecutor`].
///
/// Creates a fresh agentic loop host for each delegated sub-run, runs it to
/// completion, and collects the result as [`AgentResult`].
///
/// Inherits the parent session's permission mode so sub-agents enforce the
/// same approval policy.
pub(crate) struct CliDelegateSubRunExecutor {
    api: astra_thin_client::ThinClient,
    token: String,
    default_model: Option<String>,
    project_root: PathBuf,
    permission_mode: PermissionMode,
    cancel_token: Option<Arc<tokio_util::sync::CancellationToken>>,
    skill_resolver: Option<Arc<dyn astra_runtime::turn::skill_tool::SkillResolver>>,
    skill_search: SkillSearchSettings,
}

impl CliDelegateSubRunExecutor {
    pub fn new(
        api: astra_thin_client::ThinClient,
        token: String,
        default_model: Option<String>,
        project_root: PathBuf,
        permission_mode: PermissionMode,
        cancel_token: Option<Arc<tokio_util::sync::CancellationToken>>,
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
impl SubRunExecutor for CliDelegateSubRunExecutor {
    async fn execute(&self, config: SubRunConfig) -> Result<AgentResult, String> {
        let profile = &config.agent_profile;
        let effective_model = profile
            .model_override
            .clone()
            .or_else(|| self.default_model.clone());

        let all_schemas = edge_tools::all_tool_schemas();
        let valid_tool_names = openai_tool_names_from_schemas(&all_schemas);

        let perm_manager = super::permission_manager::PermissionManager::with_project_mode(
            self.permission_mode,
            &self.project_root,
        );

        // T-9: Worktree CWD injection — when team isolation provides a per-agent
        // worktree path via context, use it as the working directory instead of
        // the shared project root. This enables file-system isolation between agents.
        //
        // Security: Canonicalize paths to defeat symlink TOCTOU attacks.
        // Only accept paths that resolve under the system temp dir's worktree base.
        let worktree_base = astra_core::worktree_base_path();
        let effective_root = config
            .context
            .get(&format!("worktree_path_{}", profile.agent_id))
            .and_then(|v| v.as_str())
            .and_then(|path| {
                let p = PathBuf::from(path);
                // Canonicalize both paths to resolve symlinks before comparison
                match (p.canonicalize(), worktree_base.canonicalize()) {
                    (Ok(canon_p), Ok(canon_base)) if canon_p.starts_with(&canon_base) => {
                        Some(p)
                    }
                    _ => {
                        eprintln!(
                            "[delegate] ignoring untrusted worktree_path for {}: {}",
                            profile.agent_id, path
                        );
                        None
                    }
                }
            })
            .unwrap_or_else(|| self.project_root.clone());

        let mut host = SubRunHost {
            api: self.api.clone(),
            token: self.token.clone(),
            model: effective_model,
            project_root: effective_root.clone(),
            executor: edge_tools::ToolExecutor::new(&effective_root)
                .with_cloud(self.api.api_origin(), &self.token),
            all_schemas,
            valid_tool_names: valid_tool_names.clone(),
            perm_manager,
            max_completion_tokens: None,
            effort: None,
            agent_type: None,
            cancel_token: self.cancel_token.clone(),
            skill_resolver: self.skill_resolver.clone(),
        };

        // Build system message from agent profile
        let system_prompt = profile.system_prompt.clone().unwrap_or_else(|| {
            format!(
                "You are '{}', a specialized sub-agent. Complete the delegated task thoroughly.",
                profile.name
            )
        });

        // Build user message: task + optional context + previous_output
        let mut user_parts = vec![config.task.clone()];
        if !config.context.is_empty() {
            if let Ok(ctx_str) = serde_json::to_string_pretty(&config.context) {
                user_parts.push(format!("\n[Context]\n{ctx_str}"));
            }
        }
        if let Some(ref prev) = config.previous_output {
            user_parts.push(format!("\n[Previous agent output]\n{prev}"));
        }
        let user_message = user_parts.join("");

        let messages = vec![
            json!({ "role": "system", "content": system_prompt }),
            json!({ "role": "user", "content": user_message }),
        ];

        // Restricted tools from agent profile's skill_filter
        let restricted_tools: HashSet<String> = if profile.skill_filter.is_empty() {
            HashSet::new()
        } else {
            let allowed: HashSet<&str> = profile.skill_filter.iter().map(|s| s.as_str()).collect();
            valid_tool_names
                .iter()
                .filter(|name| !allowed.contains(name.as_str()))
                .cloned()
                .collect()
        };

        let task_profile = infer_task_execution_profile(&config.task);
        let subrun_session_id = format!("delegate-{}-{}", config.run_id, profile.agent_id);
        let step_recorder =
            StepRecorder::with_persistence(&subrun_session_id, &format!("{}-run", config.run_id));

        let mut state = AgenticLoopState {
            messages,
            tool_results: Vec::new(),
            current_session_id: Some(config.session_id.clone()),
            current_run_id: Some(config.run_id.clone()),
            final_text: String::new(),
            total_prompt: 0,
            total_completion: 0,
            total_cache_read: 0,
            total_cache_creation: 0,
            total_tool_calls: 0,
            has_any_usage: false,
            max_turns: DELEGATE_MAX_TURNS,
            remaining_turns: DELEGATE_MAX_TURNS,
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
            delegation_engine: None, // no recursive delegation from sub-agents
            skill_registry_for_activation: None,
            skill_resolver: self.skill_resolver.clone(),
            skill_executor: None,
            skill_model_override: None,
            skill_effort: None,
            skill_agent_type: None,
            skill_allowed_tools: None,
            skill_sandbox_policy: None,
            skill_quality_tracker: astra_runtime::skills::quality::SkillQualityTracker::new(),
            skill_improvement_tracker: astra_runtime::skills::improvement::ImprovementTracker::new(
            ),
            pinned_skills: HashSet::new(),
            discovered_skills: HashSet::new(),
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
            checkpoint_gate: config.checkpoint_gate.clone(),
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
        };

        let loop_result = run_agentic_loop_with_host(&mut host, &mut state).await;

        let tool_calls = state.total_tool_calls as u32;
        let agent_id = profile.agent_id.clone();
        let run_id = config.run_id;
        let prompt_tokens = state.total_prompt;
        let completion_tokens = state.total_completion;

        let partial_output = || {
            if state.final_text.is_empty() {
                None
            } else {
                Some(state.final_text.clone())
            }
        };

        match loop_result {
            Ok(astra_runtime::turn::agentic_loop_host::AgenticLoopOutcome::Completed) => {
                Ok(AgentResult {
                    agent_id,
                    run_id,
                    status: "completed".to_string(),
                    output: Some(state.final_text),
                    error: None,
                    prompt_tokens,
                    completion_tokens,
                    tool_calls,
                })
            }
            Ok(astra_runtime::turn::agentic_loop_host::AgenticLoopOutcome::Cancelled) => {
                Ok(AgentResult {
                    agent_id,
                    run_id,
                    status: "paused".to_string(),
                    output: partial_output(),
                    error: None,
                    prompt_tokens,
                    completion_tokens,
                    tool_calls,
                })
            }
            Ok(astra_runtime::turn::agentic_loop_host::AgenticLoopOutcome::Waiting(reason)) => {
                Ok(AgentResult {
                    agent_id,
                    run_id,
                    status: "waiting".to_string(),
                    output: Some(reason),
                    error: None,
                    prompt_tokens,
                    completion_tokens,
                    tool_calls,
                })
            }
            Ok(astra_runtime::turn::agentic_loop_host::AgenticLoopOutcome::Error(err))
            | Err(err) => Ok(AgentResult {
                agent_id,
                run_id,
                status: "failed".to_string(),
                output: partial_output(),
                error: Some(err),
                prompt_tokens,
                completion_tokens,
                tool_calls,
            }),
        }
    }
}

// ─── Default Agent Profiles ─────────────────────────────────────────────────

/// Register default agent profiles (coder, reviewer, writer) in the registry.
pub(crate) fn register_default_agents(
    registry: &mut astra_services::coordination::AgentProfileRegistry,
) {
    use astra_services::coordination::{AgentProfile, AgentTier};
    use std::collections::HashMap;

    let profiles = [
        AgentProfile {
            agent_id: "coder".into(),
            name: "Coder".into(),
            tier: AgentTier::User,
            system_prompt: Some(
                "You are a code implementation agent. Write clean, correct code. \
                 Use tools to read files, understand context, then make precise changes. \
                 Always verify your changes compile/pass before finishing."
                    .into(),
            ),
            skill_filter: Vec::new(),
            model_override: None,
            can_delegate: false,
            delegate_to: Vec::new(),
            max_delegation_depth: 0,
            triggers: Vec::new(),
            metadata: HashMap::new(),
            mcp_servers: Vec::new(),
        },
        AgentProfile {
            agent_id: "reviewer".into(),
            name: "Reviewer".into(),
            tier: AgentTier::User,
            system_prompt: Some(
                "You are a code review agent. Analyze code changes with high signal-to-noise. \
                 Only surface issues that genuinely matter — bugs, security vulnerabilities, \
                 logic errors. Never comment on style or formatting."
                    .into(),
            ),
            skill_filter: Vec::new(),
            model_override: None,
            can_delegate: false,
            delegate_to: Vec::new(),
            max_delegation_depth: 0,
            triggers: Vec::new(),
            metadata: HashMap::new(),
            mcp_servers: Vec::new(),
        },
        AgentProfile {
            agent_id: "writer".into(),
            name: "Writer".into(),
            tier: AgentTier::User,
            system_prompt: Some(
                "You are a documentation agent. Write clear, concise documentation. \
                 Read existing docs and code to understand conventions, then produce \
                 consistent, helpful documentation."
                    .into(),
            ),
            skill_filter: Vec::new(),
            model_override: None,
            can_delegate: false,
            delegate_to: Vec::new(),
            max_delegation_depth: 0,
            triggers: Vec::new(),
            metadata: HashMap::new(),
            mcp_servers: Vec::new(),
        },
    ];

    for profile in profiles {
        let _ = registry.register(profile);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_default_agents_populates_registry() {
        let mut registry = astra_services::coordination::AgentProfileRegistry::new();
        register_default_agents(&mut registry);

        assert!(registry.get("coder").is_some());
        assert!(registry.get("reviewer").is_some());
        assert!(registry.get("writer").is_some());
        assert!(registry.get("nonexistent").is_none());
    }

    #[test]
    fn default_agents_have_system_prompts() {
        let mut registry = astra_services::coordination::AgentProfileRegistry::new();
        register_default_agents(&mut registry);

        for id in &["coder", "reviewer", "writer"] {
            let profile = registry.get(id).unwrap();
            assert!(
                profile.system_prompt.is_some(),
                "{id} should have a system prompt"
            );
            assert!(!profile.can_delegate, "{id} should not be able to delegate");
        }
    }

    #[test]
    fn default_agents_are_user_tier() {
        let mut registry = astra_services::coordination::AgentProfileRegistry::new();
        register_default_agents(&mut registry);

        for id in &["coder", "reviewer", "writer"] {
            let profile = registry.get(id).unwrap();
            assert_eq!(profile.tier, astra_services::coordination::AgentTier::User);
        }
    }
}
