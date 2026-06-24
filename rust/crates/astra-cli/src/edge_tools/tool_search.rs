//! Tool search: delegates to astra_tools::tool_search.
//!
//! Union of the local CLI static catalog (`local_tool_schemas`) +
//! registered external schemas.
//! This keeps `tool_search(select:...)` aligned with the tool surface the local
//! CLI actually exposes, while still allowing deferred activation of
//! MCP and skill-backed tools.

use astra_turn_core::tool::schema::retain_tool_schemas_by_names;
use serde_json::Value;

use super::{ToolExecutor, local_tool_schemas};

impl ToolExecutor {
    pub(super) fn tool_search(&self, args: &Value) -> String {
        let Some(allowed_names) = self.current_searchable_tool_names() else {
            return astra_tools::tool_search::tool_search(&[], args);
        };
        let mut pool = local_tool_schemas();
        pool.extend(self.external_schemas_snapshot("external_schemas_tool_search"));
        retain_tool_schemas_by_names(&mut pool, &allowed_names);
        astra_tools::tool_search::tool_search(&pool, args)
    }
}
