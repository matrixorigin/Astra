use serde_json::Value;

/// Argument-aware plan-mode guard shared by server-local execution and
/// headless permission gating.
///
/// Plan mode is an authoring permission overlay: the model keeps a stable
/// exploration tool surface, while write/execute-class invocations are denied
/// before normal permission escalation.
pub(crate) fn is_plan_mode_blocked_tool(tool_name: &str, args: &Value) -> bool {
    astra_turn_core::plan_mode_policy::is_plan_mode_blocked_tool(tool_name, args)
}
