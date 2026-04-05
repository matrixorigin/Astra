//! Isolated (fork) skill executor — runs skills in a separate sub-agent loop.
//!
//! Skills with `context: fork` or `isolated: true` are executed in their own
//! context window with a separate token budget, tool set, and model override.
//! Only the summarized result returns to the parent conversation.
//!
//! The actual sub-run execution is delegated to a [`SkillSubRunExecutor`] which
//! is implemented differently for CLI (OwnedCliLoopHost) vs Server
//! (ServerSubRunExecutor wrapper).

use async_trait::async_trait;
use std::sync::Arc;

use super::super::manifest::{ExecutionContext, LoadedSkill};
use super::super::traits::{
    SkillError, SkillExecutionContext, SkillExecutionResult, SkillExecutor,
};

/// Trait for executing isolated skill sub-runs.
///
/// Implemented by the CLI (via `OwnedCliLoopHost`) and the server
/// (via `ServerSubRunExecutor` wrapper).
#[async_trait]
pub trait SkillSubRunExecutor: Send + Sync {
    /// Run a skill in an isolated sub-agent loop.
    /// Returns the final text output from the sub-run.
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
    ) -> Result<SubRunResult, String>;
}

/// Result from a sub-run execution.
#[derive(Clone, Debug)]
pub struct SubRunResult {
    /// Final text output.
    pub output: String,
    /// Tokens consumed by the sub-run.
    pub tokens_used: u32,
    /// Number of agentic loop turns.
    pub turns: u32,
}

/// Executes skills in an isolated sub-agent loop via a [`SkillSubRunExecutor`].
pub struct IsolatedSkillExecutor {
    sub_run_executor: Arc<dyn SkillSubRunExecutor>,
}

impl IsolatedSkillExecutor {
    pub fn new(sub_run_executor: Arc<dyn SkillSubRunExecutor>) -> Self {
        Self { sub_run_executor }
    }
}

#[async_trait]
impl SkillExecutor for IsolatedSkillExecutor {
    async fn execute(
        &self,
        skill: &LoadedSkill,
        context: &SkillExecutionContext,
    ) -> Result<SkillExecutionResult, SkillError> {
        let start = std::time::Instant::now();
        let result = self
            .sub_run_executor
            .execute_skill_subrun(
                &skill.manifest.name,
                &skill.instructions,
                &context.task,
                skill.manifest.model.as_deref(),
                skill.manifest.max_tokens,
                &skill.manifest.allowed_tools,
                skill
                    .manifest
                    .effort
                    .as_ref()
                    .map(|e| e.to_string())
                    .as_deref(),
                skill.manifest.agent_type.as_deref(),
            )
            .await
            .map_err(|e| SkillError::ExecutionFailed(e))?;
        let duration_ms = start.elapsed().as_millis() as u64;

        let formatted_output = format!(
            "## Skill Result: {}\n\n{}\n\n---\n\
             *Executed in isolated sub-run: {} turns, {} tokens{}*",
            skill.manifest.name,
            result.output,
            result.turns,
            result.tokens_used,
            skill
                .manifest
                .model
                .as_ref()
                .map(|m| format!(", model: {m}"))
                .unwrap_or_default(),
        );

        Ok(SkillExecutionResult {
            output: formatted_output,
            tokens_used: result.tokens_used,
            turns: result.turns,
            duration_ms,
            success: true,
            verification_results: Vec::new(),
            error_category: None,
        })
    }

    fn supports(&self, context: &ExecutionContext) -> bool {
        *context == ExecutionContext::Fork
    }
}

/// Routes skill execution to the appropriate executor based on execution context.
pub struct SkillExecutionRouter {
    inline: super::inline::InlineSkillExecutor,
    isolated: Option<Arc<dyn SkillExecutor>>,
}

impl SkillExecutionRouter {
    pub fn new(isolated: Option<Arc<dyn SkillExecutor>>) -> Self {
        Self {
            inline: super::inline::InlineSkillExecutor,
            isolated,
        }
    }

    /// Create a router with inline-only execution (no isolation support).
    pub fn inline_only() -> Self {
        Self::new(None)
    }
}

#[async_trait]
impl SkillExecutor for SkillExecutionRouter {
    async fn execute(
        &self,
        skill: &LoadedSkill,
        context: &SkillExecutionContext,
    ) -> Result<SkillExecutionResult, SkillError> {
        if skill.manifest.is_isolated() {
            if let Some(ref isolated) = self.isolated {
                return isolated.execute(skill, context).await;
            }
            // Fallback to inline if no isolated executor is available
            eprintln!(
                "  ⚠ Skill '{}' requests isolated execution but no executor is available; falling back to inline",
                skill.manifest.name
            );
        }

        self.inline.execute(skill, context).await
    }

    fn supports(&self, context: &ExecutionContext) -> bool {
        match context {
            ExecutionContext::Inline => true,
            ExecutionContext::Fork => self.isolated.is_some(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::manifest::SkillManifest;
    use std::collections::HashMap;

    struct MockSubRunExecutor;

    #[async_trait]
    impl SkillSubRunExecutor for MockSubRunExecutor {
        async fn execute_skill_subrun(
            &self,
            skill_name: &str,
            _instructions: &str,
            task_context: &str,
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _allowed_tools: &[String],
            _effort: Option<&str>,
            _agent_type: Option<&str>,
        ) -> Result<SubRunResult, String> {
            Ok(SubRunResult {
                output: format!("Result from {skill_name}: processed '{task_context}'"),
                tokens_used: 500,
                turns: 3,
            })
        }
    }

    #[tokio::test]
    async fn isolated_executor_formats_output() {
        let executor = IsolatedSkillExecutor::new(Arc::new(MockSubRunExecutor));
        let skill = LoadedSkill {
            manifest: SkillManifest {
                name: "deep-review".into(),
                execution_context: ExecutionContext::Fork,
                model: Some("claude-sonnet-4-20250514".into()),
                ..Default::default()
            },
            instructions: "Review everything.".into(),
            instruction_tokens: 10,
            resources: None,
            skill_dir: None,
        };

        let context = SkillExecutionContext {
            task: "Review auth module".into(),
            arguments: HashMap::new(),
        };

        let result = executor.execute(&skill, &context).await.unwrap();
        assert!(result.output.contains("## Skill Result: deep-review"));
        assert!(result.output.contains("3 turns, 500 tokens"));
        assert!(result.output.contains("claude-sonnet-4-20250514"));
        assert_eq!(result.turns, 3);
        assert_eq!(result.tokens_used, 500);
    }

    #[tokio::test]
    async fn router_uses_inline_for_inline_skills() {
        let router = SkillExecutionRouter::inline_only();
        let skill = LoadedSkill {
            manifest: SkillManifest {
                name: "simple".into(),
                execution_context: ExecutionContext::Inline,
                ..Default::default()
            },
            instructions: "Do the thing.".into(),
            instruction_tokens: 10,
            resources: None,
            skill_dir: None,
        };

        let context = SkillExecutionContext {
            task: String::new(),
            arguments: HashMap::new(),
        };

        let result = router.execute(&skill, &context).await.unwrap();
        assert!(result.output.contains("# Skill: simple"));
    }

    #[tokio::test]
    async fn router_uses_isolated_when_available() {
        let isolated = Arc::new(IsolatedSkillExecutor::new(Arc::new(MockSubRunExecutor)));
        let router = SkillExecutionRouter::new(Some(isolated));
        let skill = LoadedSkill {
            manifest: SkillManifest {
                name: "deep-review".into(),
                execution_context: ExecutionContext::Fork,
                ..Default::default()
            },
            instructions: "Review.".into(),
            instruction_tokens: 5,
            resources: None,
            skill_dir: None,
        };

        let context = SkillExecutionContext {
            task: "test".into(),
            arguments: HashMap::new(),
        };

        let result = router.execute(&skill, &context).await.unwrap();
        assert!(result.output.contains("## Skill Result: deep-review"));
    }

    #[tokio::test]
    async fn router_falls_back_to_inline_when_no_isolated() {
        let router = SkillExecutionRouter::inline_only();
        let skill = LoadedSkill {
            manifest: SkillManifest {
                name: "forked".into(),
                execution_context: ExecutionContext::Fork,
                ..Default::default()
            },
            instructions: "Isolated instructions.".into(),
            instruction_tokens: 10,
            resources: None,
            skill_dir: None,
        };

        let context = SkillExecutionContext {
            task: String::new(),
            arguments: HashMap::new(),
        };

        // Falls back to inline
        let result = router.execute(&skill, &context).await.unwrap();
        assert!(result.output.contains("# Skill: forked"));
    }

    // ── Additional executor edge case tests ──────────────────────────────

    struct FailingSubRunExecutor;

    #[async_trait]
    impl SkillSubRunExecutor for FailingSubRunExecutor {
        async fn execute_skill_subrun(
            &self,
            _skill_name: &str,
            _instructions: &str,
            _task_context: &str,
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _allowed_tools: &[String],
            _effort: Option<&str>,
            _agent_type: Option<&str>,
        ) -> Result<SubRunResult, String> {
            Err("sub-run failed: timeout".into())
        }
    }

    #[tokio::test]
    async fn isolated_executor_propagates_subrun_error() {
        let executor = IsolatedSkillExecutor::new(Arc::new(FailingSubRunExecutor));
        let skill = LoadedSkill {
            manifest: SkillManifest {
                name: "will-fail".into(),
                execution_context: ExecutionContext::Fork,
                ..Default::default()
            },
            instructions: "Try to do something.".into(),
            instruction_tokens: 10,
            resources: None,
            skill_dir: None,
        };

        let context = SkillExecutionContext {
            task: "test".into(),
            arguments: HashMap::new(),
        };

        let err = executor.execute(&skill, &context).await.unwrap_err();
        assert!(matches!(err, SkillError::ExecutionFailed(_)));
    }

    #[tokio::test]
    async fn isolated_executor_no_model_omits_model_suffix_in_output() {
        let executor = IsolatedSkillExecutor::new(Arc::new(MockSubRunExecutor));
        let skill = LoadedSkill {
            manifest: SkillManifest {
                name: "no-model".into(),
                execution_context: ExecutionContext::Fork,
                model: None,
                ..Default::default()
            },
            instructions: "Review.".into(),
            instruction_tokens: 5,
            resources: None,
            skill_dir: None,
        };

        let context = SkillExecutionContext {
            task: "test".into(),
            arguments: HashMap::new(),
        };

        let result = executor.execute(&skill, &context).await.unwrap();
        // When no model is set, the ", model: X" suffix should be absent
        assert!(!result.output.contains(", model:"));
    }

    #[test]
    fn isolated_executor_supports_fork_only() {
        let executor = IsolatedSkillExecutor::new(Arc::new(MockSubRunExecutor));
        assert!(executor.supports(&ExecutionContext::Fork));
        assert!(!executor.supports(&ExecutionContext::Inline));
    }

    #[test]
    fn router_inline_only_supports_inline_not_fork() {
        let router = SkillExecutionRouter::inline_only();
        assert!(router.supports(&ExecutionContext::Inline));
        assert!(!router.supports(&ExecutionContext::Fork));
    }

    #[test]
    fn router_with_isolated_supports_both() {
        let isolated = Arc::new(IsolatedSkillExecutor::new(Arc::new(MockSubRunExecutor)));
        let router = SkillExecutionRouter::new(Some(isolated));
        assert!(router.supports(&ExecutionContext::Inline));
        assert!(router.supports(&ExecutionContext::Fork));
    }

    #[tokio::test]
    async fn router_delegates_fork_to_isolated_executor() {
        let isolated = Arc::new(IsolatedSkillExecutor::new(Arc::new(MockSubRunExecutor)));
        let router = SkillExecutionRouter::new(Some(isolated));
        let skill = LoadedSkill {
            manifest: SkillManifest {
                name: "forked".into(),
                execution_context: ExecutionContext::Fork,
                model: Some("test-model".into()),
                ..Default::default()
            },
            instructions: "Fork instructions.".into(),
            instruction_tokens: 10,
            resources: None,
            skill_dir: None,
        };

        let context = SkillExecutionContext {
            task: "fork task".into(),
            arguments: HashMap::new(),
        };

        let result = router.execute(&skill, &context).await.unwrap();
        // Should use isolated (## Skill Result) not inline (# Skill)
        assert!(result.output.contains("## Skill Result: forked"));
        assert!(result.output.contains("test-model"));
    }

    // ── Effort/agent_type threading test ─────────────────────────────────

    /// Mock that captures the effort and agent_type passed to it.
    struct CapturingSubRunExecutor {
        captured_effort: std::sync::Mutex<Option<String>>,
        captured_agent_type: std::sync::Mutex<Option<String>>,
    }

    impl CapturingSubRunExecutor {
        fn new() -> Self {
            Self {
                captured_effort: std::sync::Mutex::new(None),
                captured_agent_type: std::sync::Mutex::new(None),
            }
        }
    }

    #[async_trait]
    impl SkillSubRunExecutor for CapturingSubRunExecutor {
        async fn execute_skill_subrun(
            &self,
            _skill_name: &str,
            _instructions: &str,
            _task_context: &str,
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _allowed_tools: &[String],
            effort: Option<&str>,
            agent_type: Option<&str>,
        ) -> Result<SubRunResult, String> {
            *self.captured_effort.lock().unwrap() = effort.map(String::from);
            *self.captured_agent_type.lock().unwrap() = agent_type.map(String::from);
            Ok(SubRunResult {
                output: "done".into(),
                tokens_used: 100,
                turns: 1,
            })
        }
    }

    #[tokio::test]
    async fn isolated_executor_threads_effort_and_agent_type() {
        use crate::skills::manifest::EffortLevel;

        let executor_inner = Arc::new(CapturingSubRunExecutor::new());
        let executor = IsolatedSkillExecutor::new(executor_inner.clone());
        let skill = LoadedSkill {
            manifest: SkillManifest {
                name: "threaded".into(),
                execution_context: ExecutionContext::Fork,
                effort: Some(EffortLevel::High),
                agent_type: Some("coder".into()),
                ..Default::default()
            },
            instructions: "Thread test.".into(),
            instruction_tokens: 5,
            resources: None,
            skill_dir: None,
        };

        let context = SkillExecutionContext {
            task: "test effort threading".into(),
            arguments: HashMap::new(),
        };

        let _result = executor.execute(&skill, &context).await.unwrap();

        assert_eq!(
            *executor_inner.captured_effort.lock().unwrap(),
            Some("high".to_string()),
            "effort should be threaded through to SubRunExecutor"
        );
        assert_eq!(
            *executor_inner.captured_agent_type.lock().unwrap(),
            Some("coder".to_string()),
            "agent_type should be threaded through to SubRunExecutor"
        );
    }

    #[tokio::test]
    async fn isolated_executor_threads_none_effort() {
        let executor_inner = Arc::new(CapturingSubRunExecutor::new());
        let executor = IsolatedSkillExecutor::new(executor_inner.clone());
        let skill = LoadedSkill {
            manifest: SkillManifest {
                name: "no-effort".into(),
                execution_context: ExecutionContext::Fork,
                effort: None,
                agent_type: None,
                ..Default::default()
            },
            instructions: "No effort.".into(),
            instruction_tokens: 5,
            resources: None,
            skill_dir: None,
        };

        let context = SkillExecutionContext {
            task: "test".into(),
            arguments: HashMap::new(),
        };

        let _result = executor.execute(&skill, &context).await.unwrap();
        assert_eq!(*executor_inner.captured_effort.lock().unwrap(), None);
        assert_eq!(*executor_inner.captured_agent_type.lock().unwrap(), None);
    }
}
