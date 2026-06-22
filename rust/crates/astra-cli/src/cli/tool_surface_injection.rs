use std::collections::HashSet;

use astra_runtime::tool_registry::ToolRegistry;
use astra_turn_core::tool::registry::meta::tool_meta;
use astra_turn_core::tool::schema::tool_schema_name;
use serde_json::Value;

use crate::edge_tools::ToolExecutor;

pub(crate) fn install_injected_tool_schema(
    executor: &ToolExecutor,
    schema: Value,
    all_schemas: &mut Vec<Value>,
    valid_tool_names: &mut HashSet<String>,
    registry: Option<&mut ToolRegistry>,
) -> bool {
    let Some(name) = tool_schema_name(&schema).map(str::to_string) else {
        return false;
    };
    if tool_meta(&name).is_none() {
        return false;
    }
    let runtime_bound = executor.runtime_bound_tool_schemas(vec![schema.clone()]);
    if runtime_bound.len() != 1 || tool_schema_name(&runtime_bound[0]) != Some(name.as_str()) {
        return false;
    }

    valid_tool_names.insert(name.clone());
    if let Some(registry) = registry {
        registry.upsert_schema(schema.clone());
    }
    if let Some(existing) = all_schemas
        .iter_mut()
        .find(|tool| tool_schema_name(tool) == Some(name.as_str()))
    {
        *existing = schema;
    } else {
        all_schemas.push(schema);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema(name: &str) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": name,
                "description": "test schema",
                "parameters": {"type": "object", "properties": {}}
            }
        })
    }

    fn test_executor() -> (tempfile::TempDir, ToolExecutor) {
        let dir = tempfile::TempDir::new().unwrap();
        let executor = ToolExecutor::new(dir.path());
        (dir, executor)
    }

    #[test]
    fn install_injected_tool_schema_accepts_runtime_bound_skill() {
        let (_dir, executor) = test_executor();
        let mut all_schemas = Vec::new();
        let mut valid_tool_names = HashSet::new();
        let mut registry = ToolRegistry::new(Vec::new());

        let accepted = install_injected_tool_schema(
            &executor,
            astra_runtime::turn::skill_tool::skill_tool_schema_v2(),
            &mut all_schemas,
            &mut valid_tool_names,
            Some(&mut registry),
        );

        assert!(accepted);
        assert!(valid_tool_names.contains("skill"));
        assert_eq!(all_schemas.len(), 1);
        assert_eq!(tool_schema_name(&all_schemas[0]), Some("skill"));
        assert!(registry.schema_by_name("skill").is_some());
    }

    #[test]
    fn install_injected_tool_schema_rejects_agent_fanout_without_spawner() {
        let (_dir, executor) = test_executor();
        let mut all_schemas = Vec::new();
        let mut valid_tool_names = HashSet::new();

        let accepted = install_injected_tool_schema(
            &executor,
            schema("agent_fanout"),
            &mut all_schemas,
            &mut valid_tool_names,
            None,
        );

        assert!(!accepted);
        assert!(all_schemas.is_empty());
        assert!(valid_tool_names.is_empty());
    }

    #[test]
    fn install_injected_tool_schema_rejects_unknown_function_schema() {
        let (_dir, executor) = test_executor();
        let mut all_schemas = Vec::new();
        let mut valid_tool_names = HashSet::new();

        let accepted = install_injected_tool_schema(
            &executor,
            schema("not_registered"),
            &mut all_schemas,
            &mut valid_tool_names,
            None,
        );

        assert!(!accepted);
        assert!(all_schemas.is_empty());
        assert!(valid_tool_names.is_empty());
    }

    #[test]
    fn install_injected_tool_schema_accepts_missing_type_function_shorthand() {
        let (_dir, executor) = test_executor();
        let mut all_schemas = Vec::new();
        let mut valid_tool_names = HashSet::new();

        let accepted = install_injected_tool_schema(
            &executor,
            json!({"function": {"name": "skill"}}),
            &mut all_schemas,
            &mut valid_tool_names,
            None,
        );

        assert!(accepted);
        assert_eq!(all_schemas.len(), 1);
        assert!(valid_tool_names.contains("skill"));
    }

    #[test]
    fn install_injected_tool_schema_rejects_non_function_schema() {
        let (_dir, executor) = test_executor();
        let mut all_schemas = Vec::new();
        let mut valid_tool_names = HashSet::new();

        let accepted = install_injected_tool_schema(
            &executor,
            json!({"type": "custom", "function": {"name": "skill"}}),
            &mut all_schemas,
            &mut valid_tool_names,
            None,
        );

        assert!(!accepted);
        assert!(all_schemas.is_empty());
        assert!(valid_tool_names.is_empty());
    }
}
