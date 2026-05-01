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
use serde_json::{Value, json};

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
    /// Optional sink for fork-cache telemetry. When `None` the
    /// executor still forwards `inherited_prefix` so child messages
    /// prepend the parent prefix — but no ForkCacheEvent is emitted.
    /// Zero-cost when unset.
    fork_cache_sink: Option<Arc<dyn astra_turn_core::fork_cache_event::ForkCacheEventSink>>,
}

/// Build the child agent's message array from system prompt, optional
/// inherited prefix, and the child task. Ensures role alternation is
/// valid for providers that require strict user/assistant alternation
/// (e.g. Bedrock Converse).
pub(crate) fn build_child_messages(
    system_prompt: &str,
    prefix_messages: Option<&[Value]>,
    child_task: &str,
) -> Vec<Value> {
    let prefix_len = prefix_messages.map_or(0, |p| p.len());
    let mut messages = Vec::with_capacity(3 + prefix_len);
    messages.push(json!({ "role": "system", "content": system_prompt }));
    if let Some(prefix) = prefix_messages {
        messages.extend(prefix.iter().cloned());
        // Bedrock Converse requires strict role alternation. If the
        // prefix ends with user or tool role, inserting the child task
        // (also user) would create consecutive user messages → HTTP 400.
        // Insert a synthetic assistant bridge to maintain alternation.
        let last_role = messages
            .iter()
            .rev()
            .find_map(|m| m.get("role").and_then(|r| r.as_str()))
            .filter(|r| *r != "system");
        if matches!(last_role, Some("user") | Some("tool")) {
            messages.push(json!({
                "role": "assistant",
                "content": "I'll now work on the delegated task."
            }));
        }
    }
    messages.push(json!({ "role": "user", "content": child_task }));
    messages
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
            fork_cache_sink: None,
        }
    }

    /// Install a fork-cache event sink. When present, every child
    /// spawn that inherited a parent prefix emits exactly one
    /// `ForkCacheEvent` on its first ingested turn.
    pub fn with_fork_cache_sink(
        mut self,
        sink: Arc<dyn astra_turn_core::fork_cache_event::ForkCacheEventSink>,
    ) -> Self {
        self.fork_cache_sink = Some(sink);
        self
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
        let compact_strategy =
            astra_turn_core::microcompact::CompactStrategy::from_provider_hint(&config.model);

        let executor = edge_tools::ToolExecutor::new(&effective_root)
            .with_cloud(self.api.api_origin(), &self.token);
        if let Some(session_id) = self.active_session_id.as_deref() {
            executor.set_active_session_id(session_id.to_string());
        }

        // Resolve per-model workflow-guard policy once; used for both the
        // `SubRunHost::tool_cache` and the `AgenticLoopState` below.
        let resolved_tool_policy = astra_config::runtime_config::RuntimeConfig::load()
            .tool_selection
            .resolve_for_model(Some(&config.model));

        let mut host = SubRunHost {
            api: self.api.clone(),
            token: self.token.clone(),
            model: Some(config.model.clone()),
            project_root: effective_root.clone(),
            executor: std::sync::Arc::new(executor),
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
                resolved_tool_policy.max_identical_tool_calls,
            ),
            inherited_prefix: config.inherited_prefix.clone(),
            fork_cache_sink: self.fork_cache_sink.clone(),
            fork_cache_probe_state: astra_runtime::orchestration::ForkCacheProbeState::new(),
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

        // PR 5.6: if the spawner resolved a parent prefix, prepend
        // the captured prefix messages between the system prompt
        // and the child's own task. System prompt stays at [0] so
        // it reads as the child's own identity; inherited messages
        // sit behind it as "historical context from parent" that
        // the provider can hit in its prompt cache. The child task
        // at the tail is always fresh (cacheable only for future
        // child turns, not for this first call).
        let messages = build_child_messages(
            &system_prompt,
            config
                .inherited_prefix
                .as_ref()
                .map(|ip| ip.prefix_messages.as_slice()),
            &config.task,
        );

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
        // Local step-recorder session id: kept synthetic (`spawn-...`)
        // because it's only used for local journal / step file
        // persistence — server never sees this.
        let local_subrun_session_id = format!("spawn-{}-{}", config.run_id, config.agent_id);
        let step_recorder = StepRecorder::with_persistence(
            &local_subrun_session_id,
            &format!("{}-run", config.run_id),
        );

        // Wire session for the *server-facing* `chat_turn_base_payload`:
        // pass None so the server opens a fresh session for this child
        // turn rather than rejecting a synthetic `spawn-...` id with
        // "Session not found" — discovered during real-world MiniMax
        // spawn_agent verification. Reusing the parent's active
        // session id would risk cross-contamination when multiple
        // children share one parent, and cross-child race conditions
        // on per-session state.
        //
        // Local continuity (transcript, step recorder, tool journal)
        // still uses `local_subrun_session_id` which is client-side
        // only, so children remain traceable offline.
        let server_session_id: Option<String> = None;

        let start_time = std::time::Instant::now();
        let progress_emitter = config.progress_emitter.clone();
        let has_parent_permissions = config.parent_address.is_some();

        let max_turns = config.max_turns as usize;

        let mut state = AgenticLoopState {
            messages,
            tool_results: Vec::new(),
            current_session_id: server_session_id,
            current_run_id: Some(config.run_id.clone()),
            recursion_depth: config.recursion_depth,
            final_text: String::new(),
            final_text_streamed: false,
            total_prompt: 0,
            total_completion: 0,
            total_cache_read: 0,
            total_cache_creation: 0,
            total_tool_calls: 0,
            total_evidence_tool_calls: 0,
            has_any_usage: false,
            max_turns,
            remaining_turns: max_turns,
            agentic_turn_budget: task_profile.agentic_turn_budget,
            current_round_index: 0,
            llm_rounds_completed: 0,
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
            max_identical_tool_calls: resolved_tool_policy.max_identical_tool_calls,
            max_tools_per_turn: resolved_tool_policy.max_tools_per_turn,
            repeated_cache_hit_suppression: resolved_tool_policy.repeated_cache_hit_suppression,
            max_consecutive_empty_name: resolved_tool_policy.max_consecutive_empty_name,
            stall: Default::default(),
            telemetry: Default::default(),
            skills: SkillState {
                resolver: self.skill_resolver.clone(),
                quality_tracker: astra_skills::quality::SkillQualityTracker::new(),
                improvement_tracker: astra_skills::improvement::ImprovementTracker::new(),
                search: self.skill_search.clone(),
                tool_event_hooks: astra_skills::hooks::load_tool_event_hooks(&effective_root),
                session_event_hooks: astra_skills::hooks::load_session_event_hooks(&effective_root),
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
            delegations_this_turn: 0,
            project_context: None,
            checkpoint_gate: None,
            evolution_service: None,
            rate_limit_cooldown: Default::default(),
            data_snapshot_provider: None,
            last_composite_snapshot: None,
            last_measured_prompt_tokens: None,
            consecutive_context_window_errors: 0,
            compaction_effectiveness: Default::default(),
            pinned_tool_schema_tokens: 0,
            max_turn_input_tokens: astra_core::RuntimeLimits::global().max_turn_input_tokens,
            budget_wrapup_injected: false,
            skill_produced_output: false,
            max_cumulative_tokens: 0,
            thinking: astra_turn_core::thinking_config::ThinkingConfig::Off,
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
            continuity: Default::default(),
            compact_strategy,
            approval_overrides: None,
            confidence_trend: Default::default(),
            last_confidence_diagnosis: None,
            session_turn: 0,
            bridge_turn_chain_id: None,
            bridge_user_query_event_id: None,
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

    /// Bug1 regression: when inherited prefix ends with a user or tool
    /// message, the child task (also user role) creates consecutive
    /// user messages. Bedrock Converse rejects this with HTTP 400:
    /// "The provided request is not valid".
    ///
    /// The fix must insert a synthetic assistant message between the
    /// prefix and the child task when the last prefix message has
    /// role "user" or "tool".
    #[test]
    fn prefix_ending_with_user_or_tool_must_insert_assistant_bridge() {
        // Simulate the message construction logic from execute()
        let prefix_messages = vec![json!({"role": "user", "content": "original prompt"})];
        let system_prompt = "You are a child agent.";
        let child_task = "Reply: inherited-ok";

        let messages = build_child_messages(system_prompt, Some(&prefix_messages), child_task);

        // After system, the messages should NOT have two consecutive user roles.
        let non_system: Vec<&str> = messages
            .iter()
            .filter_map(|m| m.get("role").and_then(|r| r.as_str()))
            .filter(|r| *r != "system")
            .collect();

        for window in non_system.windows(2) {
            let both_user = (window[0] == "user" || window[0] == "tool")
                && (window[1] == "user" || window[1] == "tool");
            assert!(
                !both_user,
                "consecutive user/tool messages detected: [{}, {}] — \
                 Bedrock will reject with HTTP 400",
                window[0], window[1]
            );
        }
    }

    /// Same as above but for prefix ending with tool role.
    #[test]
    fn prefix_ending_with_tool_must_insert_assistant_bridge() {
        let prefix_messages = vec![
            json!({"role": "user", "content": "do something"}),
            json!({"role": "assistant", "content": "", "tool_calls": [{"id": "1", "function": {"name": "bash", "arguments": "{}"}}]}),
            json!({"role": "tool", "tool_call_id": "1", "content": "done"}),
        ];
        let messages = build_child_messages("system", Some(&prefix_messages), "child task");

        let non_system: Vec<&str> = messages
            .iter()
            .filter_map(|m| m.get("role").and_then(|r| r.as_str()))
            .filter(|r| *r != "system")
            .collect();

        for window in non_system.windows(2) {
            let both_user = (window[0] == "user" || window[0] == "tool")
                && (window[1] == "user" || window[1] == "tool");
            assert!(
                !both_user,
                "consecutive user/tool messages detected: [{}, {}]",
                window[0], window[1]
            );
        }
    }

    /// When prefix ends with assistant, no bridge is needed.
    #[test]
    fn prefix_ending_with_assistant_needs_no_bridge() {
        let prefix_messages = vec![
            json!({"role": "user", "content": "hi"}),
            json!({"role": "assistant", "content": "hello"}),
        ];
        let messages = build_child_messages("system", Some(&prefix_messages), "child task");

        // Should be: system, user, assistant, user(child task) — no extra assistant
        let roles: Vec<&str> = messages
            .iter()
            .filter_map(|m| m.get("role").and_then(|r| r.as_str()))
            .collect();
        assert_eq!(roles, vec!["system", "user", "assistant", "user"]);
    }

    /// Regression guard for "Session not found" during real-world
    /// spawn_agent: the previous code set
    /// `current_session_id: Some("spawn-<run>-<agent>")` on the
    /// child's `AgenticLoopState`, then forwarded that synthetic id
    /// into the server-facing `chat_turn_base_payload` — which the
    /// server rejected because it had never registered the id. The
    /// fix is to pass `None` for the child's server-facing session,
    /// letting the server open a fresh one per child turn.
    ///
    /// We can't directly test the async `execute` path (needs a
    /// mock agentic loop + HTTP), so this is a structural regression:
    /// grep the source for the tell-tale synthetic-id pattern that
    /// would re-introduce the bug.
    #[test]
    fn child_must_not_send_synthetic_session_id_to_server() {
        let src = include_str!("spawn_subrun.rs");
        // The bug had `current_session_id: Some(subrun_session_id)`
        // where subrun_session_id was a `spawn-...` synthetic.
        // Guard the fix: the server-facing session id must be a
        // distinct variable explicitly set to None.
        assert!(
            src.contains("let server_session_id: Option<String> = None;"),
            "child must not reuse the local synthetic subrun id as \
             its server-facing session — regression of \"Session \
             not found\" during real-world spawn_agent calls"
        );
        assert!(
            src.contains("current_session_id: server_session_id"),
            "child AgenticLoopState must consume `server_session_id` \
             (the None-typed variable above), not a fresh Some(...)"
        );
    }

    #[test]
    fn local_subrun_session_id_still_threads_to_step_recorder() {
        // Local persistence (journal / transcript / step recorder)
        // must continue to use the synthetic subrun id so multiple
        // concurrent children don't collide on parent's session
        // files. This test pins the split between server-facing
        // (None) and local (`spawn-...`) session identities.
        let src = include_str!("spawn_subrun.rs");
        assert!(
            src.contains("let local_subrun_session_id = format!(\"spawn-{}-{}\""),
            "local-only subrun id must still be built for the step \
             recorder to avoid cross-child file collisions"
        );
        assert!(
            src.contains("StepRecorder::with_persistence(\n            &local_subrun_session_id,"),
            "step recorder must take the local synthetic id"
        );
    }
}
