//! Runtime-owned artifact path guards shared by file-like tools.
//!
//! Tool-result artifacts are persistence internals, not workspace files. If a
//! model reads them directly through bash/read_file it bypasses the owning
//! tool's recovery protocol, sees stale or compacted bytes, and couples itself
//! to local disk layout. Keep this predicate in one place so every filesystem
//! entry point can fail the same way.

pub fn references_internal_tool_result_artifact(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.contains("artifact://session/tool-result/")
        || (lower.contains(".astra/sessions/") && contains_tool_results_segment(&lower))
        || (lower.contains(".astra/tool-results/") || lower.contains(".astra/tool-results "))
        || lower.ends_with(".astra/tool-results")
}

fn contains_tool_results_segment(value: &str) -> bool {
    value.contains("/tool-results/")
        || value.ends_with("/tool-results")
        || value.contains("/tool-results ")
        || value.contains("/tool-results\t")
        || value.contains("/tool-results\n")
        || value.contains("/tool-results'")
        || value.contains("/tool-results\"")
}

pub fn internal_tool_result_artifact_access_error(surface: &str, value: &str) -> Option<String> {
    if !references_internal_tool_result_artifact(value) {
        return None;
    }
    Some(format!(
        "Error: {surface} cannot read runtime-owned tool-result artifacts directly. \
         Use the owning tool's recovery action instead: \
         agent_fanout(action='get_results', group_id=...) for fanout groups, \
         agent(action='get_result', agent_id=...) for child agents, or \
         task_output(task_id=...) for background tasks. \
         Do not inspect ~/.astra session artifact files with shell/file tools; \
         if no first-class recovery action exists, report that the result was truncated."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_current_session_tool_result_artifacts() {
        assert!(references_internal_tool_result_artifact(
            "/home/me/.astra/sessions/session-1/tool-results/call_abc.txt"
        ));
        assert!(references_internal_tool_result_artifact(
            "find ~/.astra/sessions/session-1/tool-results -type f"
        ));
        assert!(references_internal_tool_result_artifact(
            "artifact://session/tool-result/call_abc"
        ));
        assert!(references_internal_tool_result_artifact(
            "/home/me/.astra/tool-results/call_abc.txt"
        ));
        assert!(!references_internal_tool_result_artifact(
            "/repo/.astra-notes/tool-results.txt"
        ));
    }
}
