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
        // Poison recovery: lock may be poisoned by an earlier panic on
        // the write side. Recover via `into_inner()` so plugin-backed
        // deferred activation survives the poison. A silent `if let Ok`
        // drop would leave the model unable to reach plugins with no
        // observability.
        let guard = self.plugin_schemas.read().unwrap_or_else(|poisoned| {
            tracing::warn!(
                "CLI plugin_schemas RwLock poisoned on read; recovering. \
                 Investigate the upstream panic."
            );
            poisoned.into_inner()
        });
        pool.extend(guard.iter().cloned());
        astra_tools::tool_search::tool_search(&pool, args)
    }
}
