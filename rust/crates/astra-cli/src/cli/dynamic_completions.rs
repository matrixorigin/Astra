//! Helpers for refreshing readline dynamic completions.

use super::ReplState;

/// Truncate skill description for readline completion (≤39 chars + `…` when longer).
///
/// Do not slice by byte index: descriptions may contain multi-byte Unicode (em dash, CJK, …).
pub(crate) fn truncate_skill_desc_for_completion(description: &str) -> String {
    const MAX_CHARS: usize = 39;
    let mut iter = description.chars();
    let preview: String = iter.by_ref().take(MAX_CHARS).collect();
    if iter.next().is_some() {
        format!("{preview}…")
    } else {
        description.to_string()
    }
}

/// Refresh dynamic Tab-completion data (skill names, MCP server names) from
/// the current REPL state so the readline completer offers them.
pub(crate) async fn refresh_dynamic_completions(state: &ReplState) {
    let skill_entries: Vec<(String, String)> = {
        let manifests = state.unified_skill_registry.all_manifests();
        manifests
            .into_iter()
            .map(|m| {
                let desc = truncate_skill_desc_for_completion(m.description.as_str());
                (m.name, desc)
            })
            .collect()
    };
    super::repl_ui::update_skill_completions(skill_entries);

    let mcp_entries: Vec<(String, String)> = {
        let mgr = state.mcp_manager.read().await;
        mgr.server_states()
            .into_iter()
            .map(|(name, st)| (name.to_string(), format!("{:?}", st)))
            .collect()
    };
    super::repl_ui::update_mcp_completions(mcp_entries);
}
