use crate::capability::Capability;

/// User-facing explanation for a tool call whose backing runtime is absent.
///
/// This intentionally names the real recovery path. `tool_search` can only
/// select from runtimes that are already connected in the current turn.
pub fn runtime_binding_denial_message(name: &str, action: Option<&str>) -> String {
    if name.starts_with("mcp__") {
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
        return agent_runtime_binding_denial_message(name, action);
    }

    format!(
        "Tool `{name}` is not available in this turn because its required runtime \
         capability is not connected. Calling `tool_search(query=\"select:{name}\")` \
         cannot make it executable until that runtime is available. Use currently \
         visible tools or report the missing runtime capability."
    )
}

pub fn agent_runtime_binding_denial_message(tool_name: &str, action: Option<&str>) -> String {
    let action = action
        .filter(|action| !action.is_empty())
        .map(|action| format!(" action `{action}`"))
        .unwrap_or_default();
    format!(
        "Tool `{tool_name}`{action} is not available in this turn because \
         the multi-agent runtime is not connected. Retrying this call or calling \
         `tool_search` will not attach that runtime. Continue with currently \
         visible tools, or ask the user to start a session with multi-agent \
         support if delegation is required."
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
}
