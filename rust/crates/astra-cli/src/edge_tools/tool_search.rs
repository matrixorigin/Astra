//! Tool search: delegates to astra_tools::tool_search.
//!
//! Union of the local CLI static catalog (`local_tool_schemas`) +
//! plugin-registered schemas installed via `ToolExecutor::set_plugin_schemas`.
//! This keeps `tool_search(select:...)` aligned with the tool surface the local
//! CLI actually exposes, while still allowing deferred activation of
//! MCP/skill-backed tools.

use serde_json::Value;

use super::{ToolExecutor, local_tool_schemas};

impl ToolExecutor {
    pub(super) fn tool_search(&self, args: &Value) -> String {
        let mut pool = local_tool_schemas();
        pool.extend(self.plugin_schemas_snapshot("plugin_schemas_tool_search"));
        if let Some(allowed_names) = self.current_searchable_tool_names() {
            pool.retain(|schema| {
                schema
                    .get("function")
                    .and_then(|function| function.get("name"))
                    .and_then(Value::as_str)
                    .is_some_and(|name| allowed_names.contains(name))
            });
        }
        astra_tools::tool_search::tool_search(&pool, args)
    }
}
