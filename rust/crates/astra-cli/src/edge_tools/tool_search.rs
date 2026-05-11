//! Tool search: delegates to astra_tools::tool_search.
//!
//! Union of static catalog (`all_tool_schemas`) + plugin-registered
//! schemas installed via `ToolExecutor::set_plugin_schemas`. The union
//! lets the deferred-activation flow reach MCP/skill-backed tools; the
//! static catalog alone would make `select:mcp__X` always return
//! `missing:[...]`.

use serde_json::Value;

use super::{ToolExecutor, all_tool_schemas};

impl ToolExecutor {
    pub(super) fn tool_search(&self, args: &Value) -> String {
        let mut pool = all_tool_schemas();
        if let Ok(guard) = self.plugin_schemas.read() {
            pool.extend(guard.iter().cloned());
        }
        astra_tools::tool_search::tool_search(&pool, args)
    }
}
