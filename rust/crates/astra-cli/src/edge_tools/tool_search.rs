//! Tool search: delegates to astra_tools::tool_search.

use serde_json::Value;

use super::{ToolExecutor, all_tool_schemas};

impl ToolExecutor {
    pub(super) fn tool_search(&self, args: &Value) -> String {
        astra_tools::tool_search::tool_search(&all_tool_schemas(), args)
    }
}
