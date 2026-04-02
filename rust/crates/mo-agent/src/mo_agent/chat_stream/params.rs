use mo_agent_runtime::{pipeline::persistence::ToolHealthEntry, tool_selector::ToolSelector};
use std::sync::Arc;

use crate::{
    ExplainMode, permission_manager::PermissionManager, skill_instructions::SharedSkillRegistry,
};

/// Parameters for a single agentic chat turn — groups the many arguments
/// to `stream_chat_sse` into a named struct to reduce cognitive load.
pub(crate) struct ChatTurnParams<'a> {
    pub(crate) api: &'a mo_thin_client::ThinClient,
    pub(crate) token: &'a str,
    pub(crate) message: &'a str,
    pub(crate) session_id: Option<&'a str>,
    pub(crate) model: Option<&'a str>,
    pub(crate) explain: ExplainMode,
    pub(crate) render_md: bool,
    pub(crate) history: &'a [(String, String)],
    pub(crate) perm_manager: &'a mut PermissionManager,
    pub(crate) verbose_mode: bool,
    pub(crate) quiet: bool,
    /// When true, suppress incremental UI (spinner/tool status/draft markdown)
    /// and only surface the final accumulated answer after the loop completes.
    pub(crate) suppress_intermediate_output: bool,
    pub(crate) selector: &'a dyn ToolSelector,
    pub(crate) recent_tools: &'a [String],
    pub(crate) tool_health_entries: &'a [ToolHealthEntry],
    /// Skill registry for loading instructions when LLM selects a skill.
    pub(crate) skill_registry: &'a SharedSkillRegistry,
    /// When true, omit edge tools and inject plan-only system instructions (CLI `/plan on`).
    pub(crate) plan_only_chat: bool,
    /// When true, this turn is executing a plan subtask — `when: task_completed` stop hooks apply.
    pub(crate) is_plan_subtask: bool,
    /// Sent on `/chat/turn` JSON so cloud can classify the turn like local `is_plan_subtask`.
    pub(crate) plan_subtask_id: Option<&'a str>,
    /// Optional delegation engine for multi-agent coordination with verification gates.
    pub(crate) delegation_engine:
        Option<Arc<mo_agent_runtime::server::delegation_engine::DelegationEngine>>,
    /// Optional cancellation token for interrupting SSE streaming mid-flight.
    pub(crate) cancel_token: Option<Arc<tokio_util::sync::CancellationToken>>,
}
