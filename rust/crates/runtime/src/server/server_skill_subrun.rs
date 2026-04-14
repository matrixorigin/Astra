//! Server-side skill fork (sub-run) executor.
//!
//! Enables skills with `execution_context: Fork` to run in isolated sub-agent
//! loops on the server, matching the CLI's `CliSkillSubRunExecutor` behavior.
//!
//! Each sub-run creates a fresh [`ServerAgenticLoopHost`] +
//! [`AgenticLoopState`] pair and runs [`run_agentic_loop_with_host`] to
//! completion, inheriting the parent's LLM credentials, skill resolver,
//! and cancellation token.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Map, Value, json};
use tokio::sync::Mutex as TokioMutex;

use astra_core::SharedPool;

use crate::FernetTokenEncryptor;
use crate::MatrixOneSettings;
use crate::pipeline::step_protocol::InMemoryIdempotencyCache;
use crate::pipeline::step_recorder::StepRecorder;
use crate::semantic_dedup::SemanticDedup;
use crate::skills::executor::isolated::{SkillSubRunExecutor, SubRunResult};
use crate::turn::agentic_loop_host::{
    AgenticLoopHost as _, AgenticLoopState, CancellationState, SkillState, StopHookState,
    TurnInteractionPolicy, run_agentic_loop_with_host,
};
use crate::turn::chat_turn_heuristics::infer_task_execution_profile;
use crate::turn::turn_guard::TurnGuard;

use super::server_loop_host::ServerAgenticLoopHostBuilder;

/// Maximum turns for a skill sub-run (matches CLI's SUBRUN_MAX_TURNS).
const SUBRUN_MAX_TURNS: usize = 30;

/// Maximum cumulative tokens for a skill sub-run.
const SUBRUN_MAX_CUMULATIVE_TOKENS: u64 = 500_000;

/// Server-side implementation of [`SkillSubRunExecutor`].
///
/// Creates a [`ServerAgenticLoopHost`] for each sub-run with isolated context
/// but shared LLM credentials and skill resolver.
pub struct ServerSkillSubRunExecutor {
    matrixone: MatrixOneSettings,
    encryptor: Arc<FernetTokenEncryptor>,
    shared_pool: Option<SharedPool>,
    /// Default model to use when the skill manifest doesn't specify one.
    default_model: Option<String>,
    /// Edge tools available to sub-runs (inherited from parent host).
    edge_tools: Vec<Value>,
    /// Edge profile (cwd, git_branch, etc.) inherited from parent.
    edge_profile: Map<String, Value>,
    /// Skill resolver inherited from parent — enables nested inline skills.
    skill_resolver: Option<Arc<dyn crate::turn::skill_tool::SkillResolver>>,
    /// Parent cancellation token — propagated so stop/cancel interrupts sub-runs.
    cancel_token: Option<Arc<tokio_util::sync::CancellationToken>>,
    /// Session ID for the parent run.
    session_id: String,
}

impl ServerSkillSubRunExecutor {
    pub fn new(
        matrixone: MatrixOneSettings,
        encryptor: Arc<FernetTokenEncryptor>,
        session_id: String,
    ) -> Self {
        Self {
            matrixone,
            encryptor,
            shared_pool: None,
            default_model: None,
            edge_tools: Vec::new(),
            edge_profile: Map::new(),
            skill_resolver: None,
            cancel_token: None,
            session_id,
        }
    }

    pub fn with_pool(mut self, pool: Option<SharedPool>) -> Self {
        self.shared_pool = pool;
        self
    }

    pub fn with_default_model(mut self, model: Option<String>) -> Self {
        self.default_model = model;
        self
    }

    pub fn with_edge_tools(mut self, tools: Vec<Value>) -> Self {
        self.edge_tools = tools;
        self
    }

    pub fn with_edge_profile(mut self, profile: Map<String, Value>) -> Self {
        self.edge_profile = profile;
        self
    }

    pub fn with_skill_resolver(
        mut self,
        resolver: Option<Arc<dyn crate::turn::skill_tool::SkillResolver>>,
    ) -> Self {
        self.skill_resolver = resolver;
        self
    }

    pub fn with_cancel_token(
        mut self,
        token: Option<Arc<tokio_util::sync::CancellationToken>>,
    ) -> Self {
        self.cancel_token = token;
        self
    }
}

#[async_trait]
impl SkillSubRunExecutor for ServerSkillSubRunExecutor {
    async fn execute_skill_subrun(
        &self,
        skill_name: &str,
        instructions: &str,
        task_context: &str,
        model: Option<&str>,
        _max_tokens: Option<u32>,
        allowed_tools: &[String],
        parent_recursion_depth: u8,
        effort: Option<&str>,
        agent_type: Option<&str>,
    ) -> Result<SubRunResult, String> {
        let child_recursion_depth =
            crate::turn::agentic_recursion_guard::checked_child_recursion_depth(
                parent_recursion_depth,
            )?;

        let effective_model = model
            .map(String::from)
            .or_else(|| self.default_model.clone());

        // Build a sub-run session ID for isolation.
        let safe_name = crate::skills::loader::sanitize_for_path(skill_name);
        let subrun_session_id = format!(
            "subrun-{}-{}",
            safe_name,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_micros()
        );

        // Build the host for the sub-run.
        let mut builder = ServerAgenticLoopHostBuilder::new(
            self.matrixone.clone(),
            self.encryptor.clone(),
            String::new(), // sub-runs don't need user_id for LLM calls
            subrun_session_id.clone(),
        )
        .with_model(effective_model)
        .with_edge_tools(self.edge_tools.clone())
        .with_edge_profile(self.edge_profile.clone())
        .with_edge_callback_ledger(Arc::new(TokioMutex::new(HashMap::new())));

        if let Some(pool) = &self.shared_pool {
            builder = builder.with_pool(pool.clone());
        }

        let mut host = builder.build();

        // Build tool restriction set: if allowed_tools is non-empty, only those
        // tools (plus skill discovery) are permitted.
        let valid_tool_names = host.valid_tool_names();
        let restricted_tools: HashSet<String> = if allowed_tools.is_empty() {
            HashSet::new()
        } else {
            let allowed: HashSet<&str> = allowed_tools.iter().map(|s| s.as_str()).collect();
            valid_tool_names
                .iter()
                .filter(|name: &&String| {
                    !allowed.contains(name.as_str())
                        && name.as_str() != crate::turn::skill_tool::SKILL_TOOL_NAME
                        && name.as_str() != crate::turn::skill_tool::DISCOVER_SKILLS_TOOL_NAME
                })
                .cloned()
                .collect()
        };

        // Build initial messages: system = skill instructions, user = task context.
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

        let task_profile = infer_task_execution_profile(task_context);
        let workspace_root_hint = self
            .edge_profile
            .get("cwd")
            .and_then(Value::as_str)
            .map(String::from);

        let (tool_event_hooks, session_event_hooks) = workspace_root_hint
            .as_ref()
            .map(|root| crate::skills::hooks::load_all_hooks(std::path::Path::new(root)))
            .unwrap_or_default();

        let step_recorder = StepRecorder::new(&subrun_session_id, &subrun_session_id);

        let mut state = AgenticLoopState {
            messages,
            tool_results: Vec::new(),
            current_session_id: Some(self.session_id.clone()),
            current_run_id: None,
            recursion_depth: child_recursion_depth,
            final_text: String::new(),
            total_prompt: 0,
            total_completion: 0,
            total_cache_read: 0,
            total_cache_creation: 0,
            total_tool_calls: 0,
            total_evidence_tool_calls: 0,
            has_any_usage: false,
            max_turns: SUBRUN_MAX_TURNS,
            remaining_turns: SUBRUN_MAX_TURNS,
            turn_guard: TurnGuard::with_profile(task_profile),
            restricted_tools,
            step_recorder,
            idempotency_cache: InMemoryIdempotencyCache::new(),
            semantic_dedup: SemanticDedup::new(crate::semantic_dedup::DEFAULT_SIMILARITY_THRESHOLD),
            call_counts: HashMap::new(),
            max_identical_tool_calls: crate::runtime_config::RuntimeConfig::load()
                .tool_selection
                .effective_max_identical_calls(),
            max_tools_per_turn: crate::runtime_config::RuntimeConfig::load()
                .tool_selection
                .effective_max_tools_per_turn(),
            stall: Default::default(),
            telemetry: Default::default(),
            skills: SkillState {
                // Inherit resolver for nested inline skills, but NO executor
                // to prevent Fork→Fork recursion (same as CLI design).
                resolver: self.skill_resolver.clone(),
                quality_tracker: crate::skills::quality::SkillQualityTracker::new(),
                improvement_tracker: crate::skills::improvement::ImprovementTracker::new(),
                tool_event_hooks,
                session_event_hooks,
                // Skill-level effort/agent_type from manifest
                effort: effort.and_then(crate::skills::manifest::EffortLevel::parse),
                agent_type: agent_type.map(String::from),
                ..Default::default()
            },
            hooks: StopHookState {
                workspace_root_hint,
                ..Default::default()
            },
            cancellation: CancellationState {
                flag: None,
                pause_flag: None,
                token: self.cancel_token.clone(),
            },
            messaging: Default::default(),
            error_recovery: Default::default(),
            message: task_context.to_string(),
            recent_tools: Vec::new(),
            task_profile: infer_task_execution_profile(task_context),
            last_turn_policy: TurnInteractionPolicy::default(),
            api: astra_thin_client::ThinClient::new("http://127.0.0.1:1", None)
                .expect("valid dummy URL"),
            api_token: String::new(),
            delegation_engine: None,
            project_context: None,
            checkpoint_gate: None,
            evolution_service: None,
            rate_limit_cooldown: Default::default(),
            data_snapshot_provider: None,
            last_composite_snapshot: None,
            last_measured_prompt_tokens: None,
            consecutive_context_window_errors: 0,
            max_turn_input_tokens: astra_core::RuntimeLimits::global().max_turn_input_tokens,
            budget_wrapup_injected: false,
            skill_produced_output: false,
            max_cumulative_tokens: SUBRUN_MAX_CUMULATIVE_TOKENS,
            thinking_budget_tokens: None,
            recent_file_reads: Vec::new(),
            permission_context: None,
            permission_handler: None,
            tactical_adapter: None,
            step_signal_collector: None,
            tool_budget_override: None,
            pending_reflection_signals: Vec::new(),
            recent_tactical_actions: Vec::new(),
            server_tool_executor: None,
        };

        if let Err(err) = run_agentic_loop_with_host(&mut host, &mut state).await {
            return Err(format!(
                "Skill sub-run '{}' failed after {} turns: {}",
                skill_name,
                SUBRUN_MAX_TURNS - state.remaining_turns,
                err
            ));
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

    fn mock_matrixone() -> MatrixOneSettings {
        MatrixOneSettings {
            host: "127.0.0.1".to_string(),
            port: 6001,
            user: "test".to_string(),
            password: "test".to_string(),
            database: "test".to_string(),
        }
    }

    fn mock_encryptor() -> Arc<FernetTokenEncryptor> {
        Arc::new(FernetTokenEncryptor::new("cJ8pxr3t6iJmSYqe6wD7vu2rN_C3ovGUxkC5H3NXFNY=").unwrap())
    }

    #[test]
    fn server_skill_subrun_executor_builds() {
        let executor = ServerSkillSubRunExecutor::new(
            mock_matrixone(),
            mock_encryptor(),
            "test-session".to_string(),
        );
        assert!(executor.cancel_token.is_none());
        assert!(executor.skill_resolver.is_none());
    }

    #[test]
    fn server_skill_subrun_executor_with_builders() {
        let executor = ServerSkillSubRunExecutor::new(
            mock_matrixone(),
            mock_encryptor(),
            "test-session".to_string(),
        )
        .with_default_model(Some("claude-sonnet-4-20250514".to_string()))
        .with_edge_tools(vec![
            json!({"type": "function", "function": {"name": "bash"}}),
        ])
        .with_cancel_token(Some(Arc::new(tokio_util::sync::CancellationToken::new())));

        assert!(executor.default_model.is_some());
        assert_eq!(executor.edge_tools.len(), 1);
        assert!(executor.cancel_token.is_some());
    }
}
