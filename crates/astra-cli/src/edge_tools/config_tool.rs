//! Config tool: delegates to astra_tools::config_tool.

use serde_json::Value;

use super::{ToolExecutor, global_output_limit, tool_output_limit};

impl ToolExecutor {
    pub(super) fn config_tool(&self, args: &Value) -> String {
        astra_tools::config_tool::config_tool(global_output_limit(), tool_output_limit(), args)
    }
}
