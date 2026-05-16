//! Agent spawning tool for dynamic agent creation.
//!
//! Provides `spawn_agent` tool for creating sub-agents at runtime without
//! pre-defined team configurations.
//!
//! Features:
//! - Built-in agent types (explore, code-review, task, general-purpose)
//! - Background (async) and foreground (sync) execution
//! - Progress event broadcasting
//! - Integration with existing DelegationEngine
//!
//! # Example
//!
//! ```json
//! {
//!   "description": "Search codebase for auth",
//!   "prompt": "Find all authentication-related code in the project",
//!   "agent_type": "explore",
//!   "run_in_background": true
//! }
//! ```

use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;

use astra_runtime::orchestration::{
    DynamicAgentSpawner, InheritedPermissions, SpawnAgentInput, SpawnContext, spawn_agent_schema,
};

// ─── Tool Execution Context ────────────────────────────────────────────────

/// Context for spawn_agent tool execution.
pub struct SpawnAgentContext {
    /// Current agent's run ID
    pub run_id: String,
    /// Current agent's ID
    pub agent_id: String,
    /// Current active model for the parent turn. Used as the default
    /// child model when the tool call omits an explicit override.
    pub current_model: Option<String>,
    /// Current nested agent/sub-run depth of the agent.
    pub recursion_depth: u8,
    /// Working directory
    pub working_dir: PathBuf,
    /// The spawner instance
    pub spawner: Arc<DynamicAgentSpawner>,
    /// Effective permissions inherited by children spawned from this agent.
    pub inherited_permissions: InheritedPermissions,
    /// Skills available to this agent (inherited by children).
    pub active_skills: Vec<String>,
    /// Optional sink for live child token/tool/status events.
    pub live_event_sink: Option<astra_turn_core::agent_live_event::SharedAgentLiveEventSink>,
}

// ─── Tool Handler ──────────────────────────────────────────────────────────

/// Handle spawn_agent tool call from agentic loop.
///
/// This is called by the tool executor when the LLM invokes spawn_agent.
pub async fn handle_spawn_agent_tool(args: &Value, ctx: Option<&SpawnAgentContext>) -> String {
    // Parse input
    let mut input: SpawnAgentInput = match normalize_spawn_agent_args(args)
        .and_then(|patched_args| serde_json::from_value(patched_args).map_err(|e| e.to_string()))
    {
        Ok(i) => i,
        Err(e) => {
            return json!({
                "status": "failed",
                "error": format!("Invalid input: {}", e)
            })
            .to_string();
        }
    };

    // Check if spawner is available
    let ctx = match ctx {
        Some(c) => c,
        None => {
            return json!({
                "status": "failed",
                "error": "Agent spawning not available in this context."
            })
            .to_string();
        }
    };
    if input.model.is_none() {
        input.model = ctx.current_model.clone();
    }

    // Build spawn context
    let mut inherited_permissions = ctx.inherited_permissions.clone();
    inherited_permissions.is_background = input.run_in_background;
    let spawn_ctx = SpawnContext {
        parent_run_id: ctx.run_id.clone(),
        parent_agent_id: ctx.agent_id.clone(),
        recursion_depth: ctx.recursion_depth,
        working_dir: ctx.working_dir.clone(),
        inherited_permissions: Some(inherited_permissions),
        inherited_skills: ctx.active_skills.clone(),
        live_event_sink: ctx.live_event_sink.clone(),
    };

    // Execute spawn
    match ctx.spawner.spawn(input, &spawn_ctx).await {
        Ok(output) => serde_json::to_string(&output).unwrap_or_else(|_| {
            json!({
                "status": "failed",
                "error": "Failed to serialize output"
            })
            .to_string()
        }),
        Err(e) => json!({
            "status": "failed",
            "error": e.to_string()
        })
        .to_string(),
    }
}

fn non_empty_string(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn summarize_prompt(prompt: &str) -> String {
    const MAX_DESCRIPTION_CHARS: usize = 60;
    if prompt.chars().count() > MAX_DESCRIPTION_CHARS {
        let truncated: String = prompt
            .chars()
            .take(MAX_DESCRIPTION_CHARS.saturating_sub(1))
            .collect();
        format!("{}…", truncated.trim_end())
    } else {
        prompt.to_string()
    }
}

fn normalize_spawn_agent_args(args: &Value) -> Result<Value, String> {
    let mut patched_args = args.clone();
    let obj = patched_args
        .as_object_mut()
        .ok_or_else(|| "spawn input must be a JSON object".to_string())?;

    if obj.contains_key("agents") {
        return Err("unsupported `agents` payload for agent.spawn: each \
             `agent(action='spawn', ...)` call launches exactly one child. \
             To fan out N sub-agents in parallel, emit N separate `agent` \
             tool calls in a single assistant message, each with \
             `action='spawn'` and `run_in_background: true`."
            .to_string());
    }

    if obj.contains_key("task") {
        return Err("unsupported deprecated `task` field for agent.spawn. \
             Use top-level `prompt` for the full child task brief and \
             `description` for the short UI summary."
            .to_string());
    }

    let description = non_empty_string(obj.get("description")).map(str::to_string);
    let prompt = non_empty_string(obj.get("prompt")).map(str::to_string);
    if description.is_none() && prompt.is_none() {
        return Err("missing required field `prompt` or `description`".to_string());
    }

    if description.is_none() {
        let derived = summarize_prompt(prompt.as_deref().expect("checked above"));
        obj.insert("description".to_string(), Value::String(derived));
    }

    if prompt.is_none() {
        let derived = description.expect("checked above");
        obj.insert("prompt".to_string(), Value::String(derived));
    }

    Ok(patched_args)
}

// ─── get_agent_result tool ─────────────────────────────────────────────────

/// Handle get_agent_result tool call — retrieves a background child's result.
///
/// When a parent spawns a child with `run_in_background: true`, the child runs
/// asynchronously and the parent receives only a "launched" status. This
/// tool lets the parent poll for the child's result once it completes.
pub async fn handle_get_agent_result_tool(args: &Value, ctx: Option<&SpawnAgentContext>) -> String {
    let agent_id = match args.get("agent_id").and_then(|v| v.as_str()) {
        Some(id) => id,
        None => {
            return json!({
                "status": "failed",
                "error": "Missing required field: agent_id"
            })
            .to_string();
        }
    };

    let ctx = match ctx {
        Some(c) => c,
        None => {
            return json!({
                "status": "failed",
                "error": "Agent spawning not available in this context."
            })
            .to_string();
        }
    };

    use astra_turn_core::orchestration_types::AgentStatus;

    // Wait for the agent to complete using event notification — no polling.
    // The spawner's Notify is signaled by handle_completion, so this
    // returns as soon as the child finishes (or times out).
    let timeout = std::time::Duration::from_secs(120);
    match ctx.spawner.wait_for_agent(agent_id, timeout).await {
        Some(AgentStatus::Completed {
            result,
            finish_reason,
        }) => render_completed_agent_result(agent_id, &result, finish_reason.as_deref()),
        Some(AgentStatus::Failed {
            error,
            finish_reason,
        }) => {
            let reason = finish_reason.as_deref().unwrap_or("failed");
            json!({
                "status": "failed",
                "agent_id": agent_id,
                "error": error,
                "finish_reason": reason,
            })
            .to_string()
        }
        Some(status) => json!({
            "status": "unknown",
            "agent_id": agent_id,
            "detail": format!("{status:?}"),
        })
        .to_string(),
        None => {
            // P2 (session 8d9e5903 T10 regression): distinguish "truly
            // gone" from "still running — parent should call again".
            // Before this refinement, every wait timeout looked
            // identical to the LLM, so a still-executing child got
            // treated as dead. Now the parent sees "still_running"
            // when there is live state and "timeout" only when we
            // genuinely have nothing to report.
            let live_status = ctx
                .spawner
                .get_agent_state_any(agent_id)
                .await
                .map(|state| state.status);
            render_wait_timeout_outcome(agent_id, live_status.as_ref(), timeout)
        }
    }
}

fn render_completed_agent_result(
    agent_id: &str,
    result: &str,
    finish_reason: Option<&str>,
) -> String {
    let reason = finish_reason.unwrap_or("normal");
    let interrupted = reason != "normal";
    let mut body = json!({
        "status": if interrupted { "interrupted" } else { "completed" },
        "agent_id": agent_id,
        "result": result,
        "finish_reason": reason,
        "incomplete": interrupted,
    });
    if interrupted {
        body["hint"] = json!(
            "The child agent stopped before fully finishing. Treat this as incomplete and either continue it or report the interruption explicitly."
        );
    }
    body.to_string()
}

/// Decide the outcome JSON when `wait_for_agent` returns `None`.
///
/// Split out as a pure function so the P2 decision (timeout vs
/// still_running) can be regression-tested without standing up a
/// full `DynamicAgentSpawner`.
pub(crate) fn render_wait_timeout_outcome(
    agent_id: &str,
    live_status: Option<&astra_turn_core::orchestration_types::AgentStatus>,
    timeout: std::time::Duration,
) -> String {
    match live_status {
        Some(status) if !status.is_terminal() => json!({
            "status": "still_running",
            "agent_id": agent_id,
            "current_status": format!("{status:?}"),
            "waited_secs": timeout.as_secs(),
            "hint": "The child agent is still working. Call `get_agent_result` again \
                    to continue waiting. Do NOT treat this as failure or fabricate \
                    what the child would have returned.",
        })
        .to_string(),
        _ => json!({
            "status": "timeout",
            "agent_id": agent_id,
            "error": format!(
                "Agent '{agent_id}' did not complete within {}s and has no live state",
                timeout.as_secs()
            ),
        })
        .to_string(),
    }
}

/// JSON schema for get_agent_result tool.
pub fn get_agent_result_schema() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "get_agent_result",
            "description": "Retrieve the result of a background-spawned agent. Call this after spawn_agent with run_in_background:true to get the child's output once it finishes.",
            "parameters": {
                "type": "object",
                "properties": {
                    "agent_id": {
                        "type": "string",
                        "description": "The agent_id returned by spawn_agent."
                    }
                },
                "required": ["agent_id"]
            }
        }
    })
}

// ─── Tool Schema ───────────────────────────────────────────────────────────

/// Generate the JSON schema for spawn_agent tool.
///
/// Re-exports from astra_runtime::orchestration for convenience.
pub fn get_spawn_agent_schema() -> Value {
    spawn_agent_schema()
}
// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use astra_runtime::orchestration::{
        DynamicAgentSpawner, SpawnAgentExecutor, SpawnRunConfig, SpawnRunResult,
    };
    use astra_runtime::server::delegation_engine::DelegationTracker;
    use astra_turn_core::permission_types::InheritedPermissions;
    use std::path::PathBuf;
    use std::sync::Arc;

    #[test]
    fn test_schema_structure() {
        let schema = get_spawn_agent_schema();
        assert_eq!(schema["type"], "function");
        assert_eq!(schema["function"]["name"], "spawn_agent");
        assert!(schema["function"]["parameters"]["properties"]["description"].is_object());
        assert!(schema["function"]["parameters"]["properties"]["prompt"].is_object());
    }

    #[test]
    fn get_agent_result_schema_has_agent_id() {
        let schema = get_agent_result_schema();
        assert_eq!(schema["type"], "function");
        assert_eq!(schema["function"]["name"], "get_agent_result");
        assert!(schema["function"]["parameters"]["properties"]["agent_id"].is_object());
    }

    #[tokio::test]
    async fn test_handle_invalid_input() {
        let args = json!({"invalid": "data"});
        let result = handle_spawn_agent_tool(&args, None).await;
        assert!(
            result.contains("Invalid input"),
            "expected invalid input error, got: {result}"
        );
        assert!(result.contains("\"status\":\"failed\""), "{result}");
    }

    #[tokio::test]
    async fn spawn_rejects_null_and_non_object_inputs() {
        for args in [Value::Null, json!("spawn"), json!(["prompt"])] {
            let result = handle_spawn_agent_tool(&args, None).await;
            assert!(result.contains("Invalid input"), "{result}");
            assert!(result.contains("\"status\":\"failed\""), "{result}");
        }
    }

    #[test]
    fn spawn_arg_normalization_derives_description_from_unicode_prompt() {
        let prompt = "审查并发安全、事件顺序、日志因果链、用户交互、失败恢复、批处理、取消传播、状态渲染、详情页滚动".repeat(2);
        let normalized = normalize_spawn_agent_args(&json!({ "prompt": prompt })).unwrap();
        let desc = normalized["description"].as_str().unwrap();
        assert!(desc.ends_with('…'), "{desc}");
        assert!(desc.chars().count() <= 60);
        assert_eq!(normalized["prompt"].as_str().unwrap(), prompt);
    }

    #[test]
    fn spawn_arg_normalization_rejects_legacy_task_field() {
        let err = normalize_spawn_agent_args(&json!({
            "description": "Audit auth flow",
            "task": "Read src/auth and report token refresh bugs."
        }))
        .expect_err("deprecated task field must be rejected");
        assert!(
            err.contains("deprecated `task` field") && err.contains("prompt"),
            "migration error must tell callers to move to prompt. Got: {err}"
        );
    }

    #[test]
    fn spawn_arg_normalization_rejects_task_even_when_prompt_is_present() {
        let err = normalize_spawn_agent_args(&json!({
            "description": "Audit auth flow",
            "prompt": "Use the new prompt field.",
            "task": "Do not use this deprecated alias."
        }))
        .expect_err("deprecated task field must stay forbidden even when prompt exists");
        assert!(
            err.contains("deprecated `task` field"),
            "mixed prompt/task payloads must still hard-fail. Got: {err}"
        );
    }

    #[test]
    fn spawn_arg_normalization_never_fabricates_placeholder_prompt() {
        let err = normalize_spawn_agent_args(&json!({ "name": "reviewer-only" }))
            .expect_err("name alone is not enough to spawn a meaningful agent");
        assert!(
            err.contains("prompt") || err.contains("description"),
            "{err}"
        );
    }

    #[test]
    fn spawn_arg_normalization_rejects_agents_batch_payload_with_redirect() {
        let err = normalize_spawn_agent_args(&json!({
            "action": "spawn",
            "agents": [
                {"description": "Review one", "prompt": "p1"},
                {"description": "Review two", "prompt": "p2"}
            ]
        }))
        .expect_err("batch payloads must be rejected with an actionable redirect");
        assert!(err.contains("agents"), "{err}");
        assert!(
            err.contains("single assistant message") || err.contains("separate"),
            "error must explain the supported fan-out shape. Got: {err}"
        );
    }

    #[tokio::test]
    async fn test_handle_no_context() {
        let args = json!({
            "description": "Test",
            "prompt": "Test prompt"
        });
        let result = handle_spawn_agent_tool(&args, None).await;
        assert!(result.contains("not available"));
        assert!(result.contains("\"status\":\"failed\""), "{result}");
    }

    struct CapturingModelExecutor {
        captured_model: std::sync::Mutex<Option<String>>,
    }

    impl CapturingModelExecutor {
        fn new() -> Self {
            Self {
                captured_model: std::sync::Mutex::new(None),
            }
        }

        fn take_captured_model(&self) -> Option<String> {
            self.captured_model.lock().unwrap().take()
        }
    }

    #[async_trait::async_trait]
    impl SpawnAgentExecutor for CapturingModelExecutor {
        async fn execute(&self, config: SpawnRunConfig) -> Result<SpawnRunResult, String> {
            *self.captured_model.lock().unwrap() = Some(config.model.clone());
            Ok(SpawnRunResult {
                agent_id: config.agent_id,
                run_id: config.run_id,
                status: "completed".into(),
                finish_reason: "normal".into(),
                output: Some("ok".into()),
                error: None,
                prompt_tokens: 0,
                completion_tokens: 0,
                tool_calls: 0,
                permission_summary: None,
                permission_requests: 0,
                permission_requests_approved: 0,
                tools_blocked: 0,
            })
        }
    }

    fn test_spawner(executor: Arc<dyn SpawnAgentExecutor>) -> Arc<DynamicAgentSpawner> {
        let transport = Arc::new(astra_messaging::InProcessTransport::new());
        let tracker = Arc::new(DelegationTracker::new());
        let router = Arc::new(astra_messaging::AgentMailboxRouter::new(transport, tracker));
        Arc::new(DynamicAgentSpawner::new(router).with_executor(executor))
    }

    fn test_spawn_context(
        spawner: Arc<DynamicAgentSpawner>,
        current_model: Option<&str>,
    ) -> SpawnAgentContext {
        SpawnAgentContext {
            run_id: "run-parent".into(),
            agent_id: "root-agent".into(),
            current_model: current_model.map(str::to_string),
            recursion_depth: 0,
            working_dir: PathBuf::from("."),
            spawner,
            inherited_permissions: InheritedPermissions::auto_approve(),
            active_skills: Vec::new(),
            live_event_sink: None,
        }
    }

    #[tokio::test]
    async fn handle_spawn_agent_tool_inherits_parent_model_when_omitted() {
        let executor = Arc::new(CapturingModelExecutor::new());
        let spawner = test_spawner(executor.clone());
        let ctx = test_spawn_context(spawner, Some("MiniMax-M2.7"));
        let args = json!({
            "description": "Code quality review",
            "prompt": "Review the latest commit",
            "agent_type": "general-purpose"
        });

        let result = handle_spawn_agent_tool(&args, Some(&ctx)).await;

        assert!(result.contains("\"status\":\"completed\""), "{result}");
        assert_eq!(
            executor.take_captured_model().as_deref(),
            Some("MiniMax-M2.7")
        );
    }

    #[tokio::test]
    async fn handle_spawn_agent_tool_preserves_explicit_model_override() {
        let executor = Arc::new(CapturingModelExecutor::new());
        let spawner = test_spawner(executor.clone());
        let ctx = test_spawn_context(spawner, Some("MiniMax-M2.7"));
        let args = json!({
            "description": "Code quality review",
            "prompt": "Review the latest commit",
            "agent_type": "general-purpose",
            "model": "claude-sonnet-4.6"
        });

        let result = handle_spawn_agent_tool(&args, Some(&ctx)).await;

        assert!(result.contains("\"status\":\"completed\""), "{result}");
        assert_eq!(
            executor.take_captured_model().as_deref(),
            Some("claude-sonnet-4.6")
        );
    }

    #[tokio::test]
    async fn get_agent_result_missing_agent_id() {
        let args = json!({});
        let result = handle_get_agent_result_tool(&args, None).await;
        assert!(result.contains("Missing required field"));
        assert!(result.contains("\"status\":\"failed\""), "{result}");
    }

    #[tokio::test]
    async fn get_agent_result_no_context() {
        let args = json!({"agent_id": "child-1"});
        let result = handle_get_agent_result_tool(&args, None).await;
        assert!(result.contains("not available"));
        assert!(result.contains("\"status\":\"failed\""), "{result}");
    }

    // ── P2: wait-timeout decision (session 8d9e5903 T10 regression) ──

    use astra_turn_core::orchestration_types::AgentStatus;
    use std::time::Duration;

    #[test]
    fn wait_timeout_still_running_when_live_state_non_terminal() {
        let status = AgentStatus::Running {
            activity: "reading file".into(),
        };
        let out =
            render_wait_timeout_outcome("reviewer-tests", Some(&status), Duration::from_secs(120));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v["status"], "still_running",
            "a timeout while the child is demonstrably running must NOT look like failure: {out}"
        );
        assert!(
            v["hint"].as_str().unwrap_or("").contains("still working"),
            "the hint must tell the LLM to call again rather than synthesize a fake result: {out}"
        );
        assert_eq!(v["waited_secs"], 120);
    }

    #[test]
    fn wait_timeout_timeout_when_no_live_state() {
        let out = render_wait_timeout_outcome("unknown-agent", None, Duration::from_secs(120));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v["status"], "timeout",
            "when there is no live state the outcome is a genuine timeout: {out}"
        );
        assert!(
            v["error"].as_str().unwrap_or("").contains("no live state"),
            "error must make the distinction auditable: {out}"
        );
    }

    #[test]
    fn wait_timeout_reports_terminal_states_as_timeout_not_still_running() {
        // A terminal state (Failed / Cancelled / Completed) that
        // somehow slipped past the wait-for-agent completion path
        // should not be rendered as "still_running". Rare but
        // possible if completion_notifier was dropped on a race; we
        // still want the decision to be clear.
        for terminal in [
            AgentStatus::Cancelled,
            AgentStatus::Failed {
                error: "x".into(),
                finish_reason: None,
            },
            AgentStatus::Completed {
                result: "done".into(),
                finish_reason: None,
            },
        ] {
            let out = render_wait_timeout_outcome("ag", Some(&terminal), Duration::from_secs(30));
            let v: Value = serde_json::from_str(&out).unwrap();
            assert_eq!(
                v["status"], "timeout",
                "terminal state must not render as still_running: status={terminal:?} out={out}"
            );
        }
    }

    #[test]
    fn completed_result_reports_budget_exhaustion_as_interrupted() {
        let out = render_completed_agent_result(
            "reviewer-tests",
            "partial findings",
            Some("budget_exhausted"),
        );
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["status"], "interrupted");
        assert_eq!(v["finish_reason"], "budget_exhausted");
        assert_eq!(v["incomplete"], true);
        assert!(
            v["hint"].as_str().unwrap_or("").contains("incomplete"),
            "{out}"
        );
    }

    #[test]
    fn completed_result_keeps_normal_completion_completed() {
        let out = render_completed_agent_result("reviewer-tests", "done", Some("normal"));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["status"], "completed");
        assert_eq!(v["finish_reason"], "normal");
        assert_eq!(v["incomplete"], false);
        assert!(v.get("hint").is_none(), "{out}");
    }
}
