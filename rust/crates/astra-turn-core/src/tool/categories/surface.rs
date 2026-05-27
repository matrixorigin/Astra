//! Capability-driven tool surface resolution.

use serde_json::Value;

use crate::capability::CapabilitySet;
use crate::tool::registry::meta::TOOL_CATALOG;

/// User-facing execution surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Surface {
    /// Browser/web-agent; tools execute in the API server process.
    Web,
    /// Thin/remote CLI; tools execute in the API server process.
    CliRemote,
    /// Local CLI; tools execute in the CLI process.
    CliLocal,
}

/// Diagnostic result for a surface resolution pass.
#[derive(Debug, Clone)]
pub struct ResolveOutcome {
    pub schemas: Vec<Value>,
    pub missing_schemas: Vec<&'static str>,
    /// Built-in catalog tools excluded by surface policy before capability
    /// filtering. This is currently empty because runtime source selection
    /// pre-filters the schema pool per surface.
    pub dropped_by_surface: Vec<&'static str>,
}

/// Resolve schemas to advertise to the model for one turn.
pub fn resolve(
    surface: Surface,
    capabilities: &CapabilitySet,
    all_schemas: &[Value],
) -> Vec<Value> {
    resolve_with_diagnostics(surface, capabilities, all_schemas).schemas
}

/// Resolve schemas and include missing catalog-schema diagnostics.
pub fn resolve_with_diagnostics(
    surface: Surface,
    capabilities: &CapabilitySet,
    all_schemas: &[Value],
) -> ResolveOutcome {
    // The caller already selected the schema pool for this surface; keep the
    // parameter so the resolver API remains explicit and future narrowing does
    // not need another signature change.
    let _ = surface;
    let mut schemas = Vec::new();
    let mut missing_schemas = Vec::new();
    let mut emitted = std::collections::HashSet::new();

    for meta in TOOL_CATALOG {
        if !capabilities.has_all(meta.requires) {
            continue;
        }
        if let Some(schema) = find_schema(all_schemas, meta.name) {
            schemas.push(schema.clone());
            emitted.insert(meta.name.to_string());
        } else {
            missing_schemas.push(meta.name);
        }
    }

    // Pass through plugin/MCP schemas not present in TOOL_CATALOG. If a plugin
    // collides with a catalog name, the catalog filter remains authoritative
    // and the plugin entry is dropped instead of bypassing capability gates.
    for schema in all_schemas {
        let Some(name) = schema_name(schema) else {
            continue;
        };
        if emitted.contains(name) || TOOL_CATALOG.iter().any(|meta| meta.name == name) {
            continue;
        }
        schemas.push(schema.clone());
        emitted.insert(name.to_string());
    }

    ResolveOutcome {
        schemas,
        missing_schemas,
        dropped_by_surface: Vec::new(),
    }
}

fn find_schema<'a>(schemas: &'a [Value], name: &str) -> Option<&'a Value> {
    schemas
        .iter()
        .find(|schema| schema_name(schema) == Some(name))
}

fn schema_name(schema: &Value) -> Option<&str> {
    schema
        .get("function")
        .and_then(|function| function.get("name"))
        .and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::Capability;
    use serde_json::json;

    fn schema(name: &str) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": name,
                "description": format!("schema for {name}"),
                "parameters": {"type": "object"}
            }
        })
    }

    fn names(schemas: &[Value]) -> Vec<String> {
        schemas
            .iter()
            .filter_map(|schema| schema_name(schema).map(str::to_string))
            .collect()
    }

    #[test]
    fn cli_local_with_no_capabilities_hides_gated_tools() {
        let pool = vec![schema("bash"), schema("agent"), schema("memory")];
        let visible = names(&resolve(Surface::CliLocal, &CapabilitySet::empty(), &pool));

        assert!(visible.contains(&"bash".to_string()));
        assert!(!visible.contains(&"agent".to_string()));
        assert!(!visible.contains(&"memory".to_string()));
    }

    #[test]
    fn cli_local_with_agent_spawner_includes_agent() {
        let pool = vec![schema("bash"), schema("agent")];
        let caps = CapabilitySet::empty().with(Capability::AgentSpawner);
        let visible = names(&resolve(Surface::CliLocal, &caps, &pool));

        assert!(visible.contains(&"agent".to_string()));
    }

    #[test]
    fn plugin_tools_pass_through() {
        let pool = vec![
            schema("bash"),
            schema("mcp__github__create_issue"),
            schema("custom_plugin_tool"),
        ];
        let visible = names(&resolve(Surface::CliLocal, &CapabilitySet::empty(), &pool));

        assert!(visible.contains(&"bash".to_string()));
        assert!(visible.contains(&"mcp__github__create_issue".to_string()));
        assert!(visible.contains(&"custom_plugin_tool".to_string()));
    }

    #[test]
    fn pass_through_does_not_bypass_filter() {
        let pool = vec![schema("agent"), schema("mcp__plugin__custom")];
        let visible = names(&resolve(Surface::CliLocal, &CapabilitySet::empty(), &pool));

        assert!(!visible.contains(&"agent".to_string()));
        assert!(visible.contains(&"mcp__plugin__custom".to_string()));
    }

    #[test]
    fn resolve_is_byte_stable_across_calls() {
        let pool = astra_tools::schemas::all_tool_schemas();
        let caps = CapabilitySet::all();

        let a = serde_json::to_vec(&resolve(Surface::CliLocal, &caps, &pool)).unwrap();
        let b = serde_json::to_vec(&resolve(Surface::CliLocal, &caps, &pool)).unwrap();

        assert_eq!(a, b);
    }

    #[test]
    fn resolve_emits_catalog_order_for_catalog_tools() {
        let pool = astra_tools::schemas::all_tool_schemas();
        let caps = CapabilitySet::all();
        let resolved_names = names(&resolve(Surface::CliLocal, &caps, &pool));

        let catalog_names: Vec<String> = resolved_names
            .iter()
            .filter(|name| TOOL_CATALOG.iter().any(|meta| meta.name == name.as_str()))
            .cloned()
            .collect();
        let expected: Vec<String> = TOOL_CATALOG
            .iter()
            .filter_map(|meta| {
                catalog_names
                    .iter()
                    .find(|name| name.as_str() == meta.name)
                    .cloned()
            })
            .collect();

        assert_eq!(catalog_names, expected);
    }

    #[test]
    fn agent_hidden_when_agent_spawner_capability_absent() {
        let pool = astra_tools::schemas::all_tool_schemas();
        let visible = names(&resolve(Surface::CliLocal, &CapabilitySet::empty(), &pool));

        assert!(!visible.contains(&"agent".to_string()));
        assert!(!visible.contains(&"memory".to_string()));
        assert!(visible.contains(&"bash".to_string()));
    }

    #[test]
    fn diagnostics_do_not_report_surface_drops_when_pool_is_pre_filtered() {
        let pool = vec![schema("bash"), schema("agent"), schema("memory")];
        for surface in [Surface::Web, Surface::CliRemote, Surface::CliLocal] {
            let outcome = resolve_with_diagnostics(surface, &CapabilitySet::empty(), &pool);
            assert!(
                outcome.dropped_by_surface.is_empty(),
                "surface filtering is owned by the upstream schema pool; resolver should not report phantom drops"
            );
        }
    }
}
