//! Delegate sub-run executor for the CLI.
//!
//! When the LLM calls the `delegate` tool, the agentic loop's Step 3b
//! routes the call through a [`DelegationEngine`] which in turn invokes
//! a [`SubRunExecutor`].  This module provides the CLI implementation
//! that runs a real agentic loop for each sub-agent — using the same
//! API and tool infrastructure as the parent conversation.

use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use astra_core::SkillSearchSettings;
use astra_runtime::{
    pipeline::step_protocol::InMemoryIdempotencyCache,
    pipeline::step_recorder::StepRecorder,
    semantic_dedup::SemanticDedup,
    server::delegation_engine::{SubRunConfig, SubRunExecutor},
    turn::agentic_loop_finalization::run_agentic_loop_with_host,
    turn::agentic_loop_host::{
        AgenticLoopState, CancellationState, MessagingState, SkillState, StopHookState,
    },
    turn::chat_turn_heuristics::infer_task_execution_profile,
    turn::tool_schema_prune::openai_tool_names_from_schemas,
    turn::turn_guard::TurnGuard,
};
use astra_services::coordination::AgentResult;

use super::edge_tools;
use super::permission_manager::PermissionMode;
use super::skill_subrun::{SubRunHost, persist_failed_subrun};

const DELEGATE_MAX_TURNS: usize = 25;

// ─── Worktree Path Validation ───────────────────────────────────────────────

/// Resolve worktree path from delegation context with security validation.
///
/// Returns the agent-specific worktree path if:
/// 1. The context contains a `worktree_path_{agent_id}` entry
/// 2. The path canonicalizes to a location under the worktree base directory
///
/// Falls back to `default_root` if no valid worktree path is found.
///
/// Security: Returns the **canonicalized** path to prevent TOCTOU attacks where
/// a symlink target is changed between validation and use.
fn resolve_worktree_path(
    context: &HashMap<String, serde_json::Value>,
    agent_id: &str,
    worktree_base: &Path,
    default_root: &Path,
) -> PathBuf {
    // Ensure worktree base exists before validation; create if missing.
    // This handles cold-start scenarios where the base dir hasn't been created yet.
    if !worktree_base.exists() {
        if let Err(e) = std::fs::create_dir_all(worktree_base) {
            eprintln!(
                "[delegate] failed to create worktree base {}: {}",
                worktree_base.display(),
                e
            );
            return default_root.to_path_buf();
        }
    }

    context
        .get(&format!("worktree_path_{}", agent_id))
        .and_then(|v| v.as_str())
        .and_then(|path| {
            let p = PathBuf::from(path);
            // Canonicalize both paths to resolve symlinks before comparison.
            // Return the canonicalized path to prevent TOCTOU races.
            match (p.canonicalize(), worktree_base.canonicalize()) {
                (Ok(canon_p), Ok(canon_base)) if canon_p.starts_with(&canon_base) => Some(canon_p),
                _ => {
                    eprintln!(
                        "[delegate] ignoring untrusted worktree_path for {}: {}",
                        agent_id, path
                    );
                    None
                }
            }
        })
        .unwrap_or_else(|| default_root.to_path_buf())
}

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
    progress_tx:
        Option<tokio::sync::mpsc::UnboundedSender<super::skill_subrun::SubRunProgressEvent>>,
    /// Global progress broadcaster for emitting events visible in /agent watch.
    progress_broadcaster: Option<Arc<astra_runtime::orchestration::ProgressBroadcaster>>,
    /// Optional fork-cache event sink. When set, the delegated
    /// child's `on_turn_completed` hook fires a ForkCacheEvent on
    /// its first successful ingested turn — same pattern as
    /// spawn_subrun. `None` (the default) keeps delegate behavior
    /// pre-fork-prefix.
    ///
    /// Note: populating `inherited_prefix` on the delegated child's
    /// SubRunConfig requires DelegationEngine changes not done in
    /// this PR; without that wiring, the probe helper always sees
    /// `inherited_prefix: None` and no event ever fires. The sink
    /// is accepted here so the follow-up wire-up PR only touches
    /// the engine, not the executor.
    fork_cache_sink: Option<Arc<dyn astra_turn_core::fork_cache_event::ForkCacheEventSink>>,
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
            progress_tx: None,
            progress_broadcaster: None,
            fork_cache_sink: None,
        }
    }

    /// Install a fork-cache event sink. See the struct-level field
    /// doc for the completeness caveat: until DelegationEngine
    /// populates `SubRunConfig.inherited_prefix`, delegated children
    /// won't fire events even with a sink installed — it's a no-op
    /// until the other half of the wiring lands.
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

    pub fn with_progress_tx(
        mut self,
        tx: tokio::sync::mpsc::UnboundedSender<super::skill_subrun::SubRunProgressEvent>,
    ) -> Self {
        self.progress_tx = Some(tx);
        self
    }

    pub fn with_progress_broadcaster(
        mut self,
        broadcaster: Arc<astra_runtime::orchestration::ProgressBroadcaster>,
    ) -> Self {
        self.progress_broadcaster = Some(broadcaster);
        self
    }
}

/// Build the set of restricted tools from an agent profile's `skill_filter`.
///
/// `skill_filter` may contain tool names (from `agent_loader`, e.g. `["read_file",
/// "grep"]`) or skill names (from team member definitions, e.g.
/// `["review-changes"]`).  Only entries that match at least one known tool name
/// are treated as an allowlist; when none match, the entries are skill names and
/// all tools remain available.
fn build_restricted_tools(
    skill_filter: &[String],
    valid_tool_names: &HashSet<String>,
) -> HashSet<String> {
    if skill_filter.is_empty() {
        return HashSet::new();
    }
    let allowed: HashSet<&str> = skill_filter.iter().map(|s| s.as_str()).collect();
    if valid_tool_names
        .iter()
        .any(|n| allowed.contains(n.as_str()))
    {
        valid_tool_names
            .iter()
            .filter(|name| !allowed.contains(name.as_str()))
            .cloned()
            .collect()
    } else {
        HashSet::new()
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
        let compact_strategy = astra_turn_core::microcompact::CompactStrategy::from_provider_hint(
            effective_model.as_deref().unwrap_or(""),
        );
        // Resolve per-model workflow-guard policy up front; `effective_model`
        // is moved into the SubRunHost below.
        let resolved_tool_policy = astra_config::runtime_config::RuntimeConfig::load()
            .tool_selection
            .resolve_for_model(effective_model.as_deref());

        let all_schemas = edge_tools::all_tool_schemas();
        let valid_tool_names = openai_tool_names_from_schemas(&all_schemas);

        let perm_manager = super::permission_manager::PermissionManager::with_project_mode(
            self.permission_mode,
            &self.project_root,
        );

        // T-9: Worktree CWD injection — when team isolation provides a per-agent
        // worktree path via context, use it as the working directory instead of
        // the shared project root. This enables file-system isolation between agents.
        let worktree_base = astra_core::worktree_base_path();
        let effective_root = resolve_worktree_path(
            &config.context,
            &profile.agent_id,
            &worktree_base,
            &self.project_root,
        );

        let executor = edge_tools::ToolExecutor::new(&effective_root)
            .with_cloud(self.api.api_origin(), &self.token);
        if !config.session_id.trim().is_empty() {
            executor.set_active_session_id(config.session_id.clone());
        }

        let mut host = SubRunHost {
            api: self.api.clone(),
            token: self.token.clone(),
            model: effective_model,
            project_root: effective_root.clone(),
            executor: std::sync::Arc::new(executor),
            all_schemas,
            valid_tool_names: valid_tool_names.clone(),
            perm_manager,
            max_completion_tokens: None,
            effort: None,
            agent_type: None,
            cancel_token: self.cancel_token.clone(),
            skill_resolver: self.skill_resolver.clone(),
            progress_tx: self.progress_tx.clone(),
            agent_id: profile.agent_id.clone(),
            tool_cache: super::stream_render::EdgeToolCache::new(
                resolved_tool_policy.max_identical_tool_calls,
            ),
            // Bug B step 2: consume the inherited_prefix the
            // DelegationEngine resolved (from its prefix_store + the
            // parent's run id). When None, the child runs fresh —
            // same as pre-fork-prefix. The probe helper inside
            // `on_turn_completed` early-returns on None, so a delegate
            // without resolved inheritance emits nothing, just like
            // the spawn_agent path.
            inherited_prefix: config.inherited_prefix.clone(),
            fork_cache_sink: self.fork_cache_sink.clone(),
            fork_cache_probe_state: astra_runtime::orchestration::ForkCacheProbeState::new(),
        };

        // Build system message from agent profile.
        // Always append the non-interactive directive so sub-agents never stall
        // waiting for user input — they must make autonomous decisions.
        const NON_INTERACTIVE: &str = "\n\nIMPORTANT: You are running as an autonomous sub-agent with no user present. \
            Do NOT ask clarifying questions or prompt for input. \
            Make reasonable assumptions and proceed to completion.";

        let system_prompt = profile
            .system_prompt
            .as_deref()
            .map(|p| format!("{p}{NON_INTERACTIVE}"))
            .unwrap_or_else(|| {
                format!(
                    "You are '{}', a specialized sub-agent. Complete the delegated task thoroughly.{NON_INTERACTIVE}",
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

        // Bug B step 2: prepend the parent's captured messages when
        // the engine resolved a ForkPrefix for this delegation.
        // Mirrors the spawn_subrun ordering: system prompt stays
        // at [0] as the child's identity, the parent transcript
        // sits behind it as cacheable context, and the child's
        // task message is the fresh suffix. When no prefix was
        // resolved, this degenerates to the pre-fix 2-message layout.
        let messages = crate::spawn_subrun::build_child_messages(
            &system_prompt,
            config.inherited_prefix.as_ref().map(|ip| ip.prefix_messages.as_slice()),
            &user_message,
        );

        let restricted_tools = build_restricted_tools(&profile.skill_filter, &valid_tool_names);

        let task_profile = infer_task_execution_profile(&config.task);
        let subrun_session_id = format!("delegate-{}-{}", config.run_id, profile.agent_id);
        let step_recorder =
            StepRecorder::with_persistence(&subrun_session_id, &format!("{}-run", config.run_id));

        let mut state = AgenticLoopState {
            messages,
            tool_results: Vec::new(),
            current_session_id: Some(config.session_id.clone()),
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
            max_turns: DELEGATE_MAX_TURNS,
            remaining_turns: DELEGATE_MAX_TURNS,
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
            stall: {
                let mut s = astra_runtime::turn::agentic_loop_host::StallTrackingState::default();
                s.circuit_breaker = astra_turn_core::loop_circuit_breaker::LoopCircuitBreaker::new(
                    astra_turn_core::loop_circuit_breaker::BreakerConfig {
                        absolute_max_rounds: 40,
                        ..Default::default()
                    },
                );
                s
            },
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
                progress_emitter: self
                    .progress_broadcaster
                    .as_ref()
                    .map(|b| b.for_agent(profile.agent_id.clone())),
                ..Default::default()
            },
            cancellation: CancellationState {
                flag: None,
                pause_flag: config.pause_flag.clone(),
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
            checkpoint_gate: config.checkpoint_gate.clone(),
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
            permission_context: None,
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
            Ok(astra_runtime::turn::agentic_loop_host::AgenticLoopOutcome::Error(err)) => {
                let failure_output = persist_failed_subrun(&mut state, &err);
                Ok(AgentResult {
                    agent_id,
                    run_id,
                    status: "failed".to_string(),
                    output: Some(failure_output),
                    error: Some(err),
                    prompt_tokens,
                    completion_tokens,
                    tool_calls,
                })
            }
            Err(err) => {
                let err_str = err.to_string();
                let failure_output = persist_failed_subrun(&mut state, &err_str);
                Ok(AgentResult {
                    agent_id,
                    run_id,
                    status: "failed".to_string(),
                    output: Some(failure_output),
                    error: Some(err_str),
                    prompt_tokens,
                    completion_tokens,
                    tool_calls,
                })
            }
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
        // Root orchestrator for main REPL session — can delegate to all agents.
        AgentProfile {
            agent_id: "main".into(),
            name: "Main".into(),
            tier: AgentTier::Orchestrator,
            system_prompt: None,
            skill_filter: Vec::new(),
            model_override: None,
            can_delegate: true,
            delegate_to: Vec::new(), // empty = all
            max_delegation_depth: 3,
            triggers: Vec::new(),
            metadata: HashMap::new(),
            mcp_servers: Vec::new(),
        },
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

        assert!(registry.get("main").is_some());
        assert!(registry.get("coder").is_some());
        assert!(registry.get("reviewer").is_some());
        assert!(registry.get("writer").is_some());
        assert!(registry.get("nonexistent").is_none());
    }

    #[test]
    fn main_agent_can_delegate() {
        let mut registry = astra_services::coordination::AgentProfileRegistry::new();
        register_default_agents(&mut registry);

        let main = registry.get("main").unwrap();
        assert_eq!(
            main.tier,
            astra_services::coordination::AgentTier::Orchestrator
        );
        assert!(main.can_delegate, "main should be able to delegate");
        assert!(
            main.delegate_to.is_empty(),
            "empty delegate_to = all agents"
        );
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

    #[test]
    fn main_can_delegate_to_default_agents() {
        use astra_services::coordination::{
            AggregationStrategy, CoordinationPattern, DelegationRequest,
        };
        use std::collections::HashMap;

        let mut registry = astra_services::coordination::AgentProfileRegistry::new();
        register_default_agents(&mut registry);

        // Simulate a team delegation from "main" to coder/reviewer
        let request = DelegationRequest {
            delegation_id: "d1".into(),
            parent_run_id: "run-1".into(),
            task: "Implement feature".into(),
            pattern: CoordinationPattern::FanOut {
                agent_ids: vec!["coder".into(), "reviewer".into()],
                aggregation: AggregationStrategy::AllResults,
                timeout_sec: 60,
            },
            user_id: "test-user".into(),
            depth: 0,
            context: HashMap::new(),
        };

        // This should succeed now that source_agent_id is "main" (registered)
        let result = registry.validate_delegation(&request, "main");
        assert!(
            result.is_ok(),
            "main should be able to delegate to default agents: {:?}",
            result
        );
    }

    // ─── Worktree Path Resolution Tests ────────────────────────────────────

    #[test]
    fn resolve_worktree_path_returns_default_when_missing() {
        let ctx = HashMap::new();
        let default = PathBuf::from("/project/root");
        let base = PathBuf::from("/tmp/worktrees");

        let result = resolve_worktree_path(&ctx, "agent-a", &base, &default);
        assert_eq!(result, default);
    }

    #[test]
    fn resolve_worktree_path_returns_default_on_non_string_value() {
        let mut ctx = HashMap::new();
        ctx.insert(
            "worktree_path_agent-b".to_string(),
            serde_json::json!(12345), // not a string
        );
        let default = PathBuf::from("/project/root");
        let base = PathBuf::from("/tmp/worktrees");

        let result = resolve_worktree_path(&ctx, "agent-b", &base, &default);
        assert_eq!(result, default);
    }

    #[test]
    fn resolve_worktree_path_accepts_valid_worktree_under_base() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_path_buf();
        let agent_wt = base.join("agent-c-wt");
        std::fs::create_dir_all(&agent_wt).unwrap();

        let mut ctx = HashMap::new();
        ctx.insert(
            "worktree_path_agent-c".to_string(),
            serde_json::json!(agent_wt.to_string_lossy()),
        );
        let default = PathBuf::from("/project/root");

        let result = resolve_worktree_path(&ctx, "agent-c", &base, &default);
        // Should return the canonicalized path to prevent TOCTOU
        assert_eq!(result, agent_wt.canonicalize().unwrap());
    }

    #[test]
    fn resolve_worktree_path_returns_canonicalized_path() {
        // Verify that even with a relative or non-canonical input,
        // the returned path is fully canonicalized.
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_path_buf();
        let agent_wt = base.join("nested").join("agent-wt");
        std::fs::create_dir_all(&agent_wt).unwrap();

        // Provide a path with redundant components
        let non_canonical = base
            .join("nested")
            .join("..")
            .join("nested")
            .join("agent-wt");

        let mut ctx = HashMap::new();
        ctx.insert(
            "worktree_path_agent-x".to_string(),
            serde_json::json!(non_canonical.to_string_lossy()),
        );
        let default = PathBuf::from("/project/root");

        let result = resolve_worktree_path(&ctx, "agent-x", &base, &default);
        // Result should be fully canonicalized
        assert_eq!(result, agent_wt.canonicalize().unwrap());
        assert!(!result.to_string_lossy().contains(".."));
    }

    #[test]
    fn resolve_worktree_path_creates_missing_base() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("new-worktree-base");
        // base does NOT exist yet

        let ctx = HashMap::new();
        let default = PathBuf::from("/project/root");

        // Should not panic; base gets created
        let result = resolve_worktree_path(&ctx, "agent-y", &base, &default);
        assert_eq!(result, default);
        // Base should now exist
        assert!(base.exists());
    }

    #[test]
    fn resolve_worktree_path_rejects_path_outside_base() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("worktrees");
        std::fs::create_dir_all(&base).unwrap();

        // Attempt escape: path is outside the worktree base
        let escape_path = tmp.path().join("malicious");
        std::fs::create_dir_all(&escape_path).unwrap();

        let mut ctx = HashMap::new();
        ctx.insert(
            "worktree_path_agent-d".to_string(),
            serde_json::json!(escape_path.to_string_lossy()),
        );
        let default = PathBuf::from("/project/root");

        let result = resolve_worktree_path(&ctx, "agent-d", &base, &default);
        // Should fall back to default
        assert_eq!(result, default);
    }

    #[test]
    fn resolve_worktree_path_rejects_symlink_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("worktrees");
        std::fs::create_dir_all(&base).unwrap();

        // Create a directory outside the base
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();

        // Create a symlink inside the base that points outside
        let symlink_path = base.join("escape-link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &symlink_path).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&outside, &symlink_path).unwrap();

        let mut ctx = HashMap::new();
        ctx.insert(
            "worktree_path_agent-e".to_string(),
            serde_json::json!(symlink_path.to_string_lossy()),
        );
        let default = PathBuf::from("/project/root");

        let result = resolve_worktree_path(&ctx, "agent-e", &base, &default);
        // Canonicalization should reveal the escape; fall back to default
        assert_eq!(result, default);
    }

    #[test]
    fn resolve_worktree_path_rejects_nonexistent_path() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_path_buf();
        let nonexistent = base.join("does-not-exist");

        let mut ctx = HashMap::new();
        ctx.insert(
            "worktree_path_agent-f".to_string(),
            serde_json::json!(nonexistent.to_string_lossy()),
        );
        let default = PathBuf::from("/project/root");

        let result = resolve_worktree_path(&ctx, "agent-f", &base, &default);
        // Canonicalize fails on nonexistent path; fall back to default
        assert_eq!(result, default);
    }

    // ─── build_restricted_tools Tests ──────────────────────────────────────

    fn tools(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }
    fn toolset(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn empty_skill_filter_restricts_nothing() {
        let r = build_restricted_tools(&[], &toolset(&["bash", "read_file", "grep"]));
        assert!(r.is_empty());
    }

    #[test]
    fn tool_names_in_filter_restrict_other_tools() {
        // agent_loader path: skill_filter = ["read_file", "grep"]
        let r = build_restricted_tools(
            &tools(&["read_file", "grep"]),
            &toolset(&["bash", "read_file", "grep", "write_file"]),
        );
        assert!(r.contains("bash"));
        assert!(r.contains("write_file"));
        assert!(!r.contains("read_file"));
        assert!(!r.contains("grep"));
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn skill_names_in_filter_restrict_nothing() {
        // team path: skill_filter = ["review-changes"] — no tool matches
        let r = build_restricted_tools(
            &tools(&["review-changes"]),
            &toolset(&["bash", "read_file", "grep", "write_file"]),
        );
        assert!(r.is_empty(), "skill names must not restrict tools: {r:?}");
    }

    #[test]
    fn multiple_skill_names_restrict_nothing() {
        // team with multiple skills, none matching tool names
        let r = build_restricted_tools(
            &tools(&["review-changes", "analyze-session", "verify-task"]),
            &toolset(&["bash", "read_file", "grep"]),
        );
        assert!(r.is_empty());
    }

    #[test]
    fn mixed_tool_and_skill_names_uses_tool_filter() {
        // If at least one entry matches a tool, treat as tool allowlist
        let r = build_restricted_tools(
            &tools(&["bash", "review-changes"]),
            &toolset(&["bash", "read_file", "grep"]),
        );
        // "bash" matches → allowlist mode → restrict read_file and grep
        assert!(r.contains("read_file"));
        assert!(r.contains("grep"));
        assert!(!r.contains("bash"));
    }

    #[test]
    fn single_tool_restricts_all_others() {
        let r = build_restricted_tools(
            &tools(&["bash"]),
            &toolset(&["bash", "read_file", "write_file", "grep", "glob"]),
        );
        assert_eq!(r.len(), 4);
        assert!(!r.contains("bash"));
    }

    #[test]
    fn all_tool_schemas_includes_write_tools() {
        let schemas = crate::edge_tools::all_tool_schemas();
        let names: Vec<&str> = schemas
            .iter()
            .filter_map(|s| {
                s.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
            })
            .collect();
        assert!(names.contains(&"bash"), "must include bash");
        assert!(names.contains(&"str_replace"), "must include str_replace");
        assert!(names.contains(&"write_file"), "must include write_file");
        assert!(names.contains(&"read_file"), "must include read_file");
        assert!(
            names.len() > 20,
            "expected >20 tool schemas, got {}",
            names.len()
        );
    }

    #[test]
    fn coder_agent_restricted_tools_allows_write_tools() {
        let mut registry = astra_services::coordination::AgentProfileRegistry::new();
        register_default_agents(&mut registry);

        let coder = registry.get("coder").unwrap();
        let all_schemas = crate::edge_tools::all_tool_schemas();
        let valid_tool_names =
            astra_runtime::turn::tool_schema_prune::openai_tool_names_from_schemas(&all_schemas);

        let restricted = build_restricted_tools(&coder.skill_filter, &valid_tool_names);

        // Empty skill_filter = no restrictions = all tools available (including write tools)
        assert!(
            restricted.is_empty(),
            "coder agent should have no tool restrictions, but got: {:?}",
            restricted
        );
    }

    // ─── Bug B regression: fork-cache sink propagates through delegate ───
    //
    // The ask: `CliDelegateSubRunExecutor` must accept a
    // `ForkCacheEventSink` and pass it to the internal `SubRunHost`
    // so that when `DelegationEngine` eventually populates
    // `SubRunConfig.inherited_prefix` (follow-up PR), the probe
    // helper can actually emit events. Without the sink, even a
    // correctly-resolved inherited prefix would fire into a void.
    //
    // Structural tests — avoiding the trait-mocking rabbit hole
    // from the `basic_cli` tests earlier.

    #[test]
    fn delegate_executor_accepts_fork_cache_sink() {
        let src = include_str!("delegate_subrun.rs");
        assert!(
            src.contains("pub fn with_fork_cache_sink"),
            "CliDelegateSubRunExecutor must expose with_fork_cache_sink; \
             without it, agent_runtime can't wire observability into \
             the delegate path"
        );
    }

    #[test]
    fn delegate_subrun_host_consumes_executor_sink() {
        // When the executor holds a sink, the SubRunHost it builds
        // must receive it (not `None`). Grep the production code
        // path for the assignment.
        let src = include_str!("delegate_subrun.rs");
        assert!(
            src.contains("fork_cache_sink: self.fork_cache_sink.clone()"),
            "delegate_subrun must propagate executor.fork_cache_sink \
             into SubRunHost, not hardcode None"
        );
    }
}
