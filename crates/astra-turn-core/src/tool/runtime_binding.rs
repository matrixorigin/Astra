use crate::capability::Capability;
use astra_core::tool_offer::is_mcp_namespaced_tool_name;

/// Whether a tool name is known to require a live runtime binding before it can
/// execute.
///
/// This is about the tool's declared shape, not the current runtime state. It
/// lets dispatch paths classify stale direct calls from resumed sessions as a
/// missing binding instead of as a generic unknown/deferred-tool mistake.
pub fn tool_name_requires_runtime_binding(name: &str) -> bool {
    if is_mcp_namespaced_tool_name(name) {
        return true;
    }

    crate::tool::registry::meta::tool_meta(name).is_some_and(|meta| {
        meta.requires
            .iter()
            .any(|capability| capability.is_executor_gated())
    })
}

/// Whether a tool needs an executor handle rather than only a service/plugin
/// binding.
///
/// MCP names also require runtime binding, but their binding comes from the
/// MCP manager/plugin transport and may be granted out-of-band by validator
/// extras. Executor-gated tools need a concrete executor path in the current
/// turn; if neither a matched edge result nor a server executor exists, the
/// call is not executable.
pub fn tool_name_requires_executor_binding(name: &str) -> bool {
    crate::tool::registry::meta::tool_meta(name).is_some_and(|meta| {
        meta.requires
            .iter()
            .any(|capability| capability.is_executor_gated())
    })
}

/// User-facing explanation for a tool call whose backing runtime is absent.
///
/// This intentionally names the real recovery path. `tool_search` can only
/// select from runtimes that are already connected in the current turn.
pub fn runtime_binding_denial_message(name: &str, action: Option<&str>) -> String {
    if is_mcp_namespaced_tool_name(name) {
        return format!(
            "Tool `{name}` is not available in this turn because no connected MCP server \
             currently provides it. Calling `tool_search(query=\"select:{name}\")` cannot \
             attach an MCP server. Use currently visible tools, or ask the user to connect \
             or enable the MCP server if this capability is required."
        );
    }

    if crate::tool::registry::meta::tool_meta(name)
        .is_some_and(|meta| meta.requires.contains(&Capability::AgentSpawner))
    {
        let action = action
            .filter(|action| !action.is_empty())
            .map(|action| format!(" action `{action}`"))
            .unwrap_or_default();
        return format!(
            "Tool `{name}`{action} is not available in this turn because \
             the multi-agent runtime is not connected. Retrying this call or calling \
             `tool_search` will not attach that runtime. Continue with currently \
             visible tools, or ask the user to start a session with multi-agent \
             support if delegation is required."
        );
    }

    format!(
        "Tool `{name}` is not available in this turn because its required runtime \
         capability is not connected. Calling `tool_search(query=\"select:{name}\")` \
         cannot make it executable until that runtime is available. Use currently \
         visible tools or report the missing runtime capability."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_denial_names_the_real_recovery_path() {
        let message = runtime_binding_denial_message("mcp__weather", None);

        assert!(message.contains("no connected MCP server"), "{message}");
        assert!(
            message.contains("tool_search(query=\"select:mcp__weather\")"),
            "{message}"
        );
        assert!(
            message.contains("connect or enable the MCP server"),
            "{message}"
        );
    }

    #[test]
    fn runtime_binding_requirement_is_declared_by_tool_shape() {
        assert!(tool_name_requires_runtime_binding("agent_fanout"));
        assert!(tool_name_requires_runtime_binding("agent"));
        assert!(tool_name_requires_runtime_binding("mcp__weather"));
        assert!(!tool_name_requires_runtime_binding("mcp__"));
        assert!(!tool_name_requires_runtime_binding("mcp__bad/name"));
        assert!(!tool_name_requires_runtime_binding("github"));
        assert!(!tool_name_requires_runtime_binding("reflect"));
        assert!(!tool_name_requires_runtime_binding("definitely_unknown"));
    }

    #[test]
    fn executor_binding_requirement_is_distinct_from_mcp_binding() {
        assert!(tool_name_requires_executor_binding("agent_fanout"));
        assert!(tool_name_requires_executor_binding("agent"));
        assert!(!tool_name_requires_executor_binding("mcp__weather"));
        assert!(!tool_name_requires_executor_binding("github"));
        assert!(!tool_name_requires_executor_binding("reflect"));
    }

    #[test]
    fn agent_denial_includes_action_and_multi_agent_recovery() {
        let message = runtime_binding_denial_message("agent", Some("spawn"));

        assert!(
            message.contains("Tool `agent` action `spawn` is not available"),
            "{message}"
        );
        assert!(
            message.contains("multi-agent runtime is not connected"),
            "{message}"
        );
        assert!(message.contains("tool_search"), "{message}");
        assert!(
            message.contains("start a session with multi-agent support"),
            "{message}"
        );
    }

    #[test]
    fn agent_denial_without_action_still_reads_like_a_direct_tool_call() {
        let message = runtime_binding_denial_message("agent_fanout", None);

        assert!(
            message.contains("Tool `agent_fanout` is not available"),
            "{message}"
        );
        assert!(!message.contains("action ``"), "{message}");
    }

    #[test]
    fn generic_denial_explains_that_search_cannot_create_runtime() {
        let message = runtime_binding_denial_message("future_runtime_tool", None);

        assert!(
            message.contains("required runtime capability is not connected"),
            "{message}"
        );
        assert!(
            message.contains("tool_search(query=\"select:future_runtime_tool\")"),
            "{message}"
        );
        assert!(
            message.contains("cannot make it executable until that runtime is available"),
            "{message}"
        );
    }

    #[test]
    fn invalid_mcp_shaped_names_do_not_get_mcp_recovery_guidance() {
        let message = runtime_binding_denial_message("mcp__bad/name", None);

        assert!(
            message.contains("required runtime capability is not connected"),
            "{message}"
        );
        assert!(!message.contains("no connected MCP server"), "{message}");
    }
}
