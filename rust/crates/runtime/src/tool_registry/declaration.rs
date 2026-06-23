//! Stable declarations for known tool names.
//!
//! This module is deliberately not a surface builder. It only classifies names so the
//! schema catalog, runtime capability registry, and default always-load surface can
//! be checked for drift.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolLoadPolicy {
    /// Visible in the default `tools[]` surface when the schema exists.
    AlwaysLoad,
    /// Advertised through the deferred catalog and activated with `tool_search`.
    Deferred,
    /// Callable only through a narrower control-plane path, not via the public
    /// schema catalog.
    ExplicitOnly,
    /// Runtime implementation detail. The model should not see this as a
    /// standalone public schema.
    Internal,
}

impl ToolLoadPolicy {
    #[cfg(test)]
    pub const fn is_public_schema_policy(self) -> bool {
        matches!(self, Self::AlwaysLoad | Self::Deferred | Self::ExplicitOnly)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolDeclaration {
    pub name: &'static str,
    pub load_policy: ToolLoadPolicy,
    pub note: &'static str,
}

const fn always_load(name: &'static str, note: &'static str) -> ToolDeclaration {
    ToolDeclaration {
        name,
        load_policy: ToolLoadPolicy::AlwaysLoad,
        note,
    }
}

const fn deferred(name: &'static str, note: &'static str) -> ToolDeclaration {
    ToolDeclaration {
        name,
        load_policy: ToolLoadPolicy::Deferred,
        note,
    }
}

const fn explicit_only(name: &'static str, note: &'static str) -> ToolDeclaration {
    ToolDeclaration {
        name,
        load_policy: ToolLoadPolicy::ExplicitOnly,
        note,
    }
}

const fn internal(name: &'static str, note: &'static str) -> ToolDeclaration {
    ToolDeclaration {
        name,
        load_policy: ToolLoadPolicy::Internal,
        note,
    }
}

static TOOL_DECLARATIONS: &[ToolDeclaration] = &[
    always_load("ask_user", "structured clarification"),
    always_load("bash", "core shell escape hatch"),
    always_load("git", "core version-control observability"),
    always_load("glob", "core file discovery"),
    always_load("grep", "core content search"),
    always_load("list_dir", "core directory inspection"),
    always_load("memory", "intrinsic memory"),
    always_load("read_file", "core file read"),
    always_load("skill", "runtime-injected skill activation"),
    always_load("str_replace", "core targeted edit"),
    always_load("task", "visible task board"),
    always_load("tool_search", "deferred activation primitive"),
    always_load("write_file", "core file write/delete"),
    deferred("agent", "delegation"),
    deferred("agent_fanout", "parallel delegation"),
    deferred("compress_context", "manual context compression"),
    deferred("deprioritize_tool", "session tool preference"),
    deferred("enter_plan_mode", "planning mode entry"),
    deferred("exit_plan_mode", "planning mode exit"),
    deferred("get_agent_info", "runtime agent inspection"),
    deferred("github", "credentialed GitHub access"),
    deferred("introspect", "self-inspection"),
    deferred("lsp", "language-server operations"),
    deferred("mo", "MatrixOne operations"),
    deferred("mo_query", "MatrixOne query"),
    deferred("notify", "user notification"),
    deferred("powershell", "PowerShell shell"),
    deferred("prioritize_tool", "session tool preference"),
    deferred("publish_artifact", "artifact publishing"),
    deferred("rollback_database_snapshots", "database snapshot rollback"),
    deferred("rollback_session_state", "session-state rollback"),
    deferred("run_script", "server-side RPC script execution"),
    deferred("session", "session state operations"),
    deferred("symbols", "symbol index inspection"),
    deferred("task_list", "background task inventory"),
    deferred("task_output", "background task output"),
    deferred("task_stop", "background task cancellation"),
    deferred("web_fetch", "network fetch"),
    deferred("web_search", "network search"),
    internal("delete_file", "file-delete operation behind write_file"),
    internal("find_definition", "code-intel operation behind lsp"),
    internal("find_references", "code-intel operation behind lsp"),
    internal("multi_edit", "batch-edit operation behind str_replace"),
    explicit_only(
        "rollback_file_edits",
        "file edit rollback is routed through server/session internals",
    ),
    internal("background_shell", "user-controlled background shell task"),
    internal("git_clone", "deployment/runtime clone helper"),
];

pub fn all_tool_declarations() -> &'static [ToolDeclaration] {
    TOOL_DECLARATIONS
}

#[cfg(test)]
pub fn tool_declaration(name: &str) -> Option<&'static ToolDeclaration> {
    TOOL_DECLARATIONS
        .iter()
        .find(|declaration| declaration.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_turn_core::tool::schema::tool_schema_name;

    fn schema_names() -> std::collections::BTreeSet<String> {
        astra_tools::schemas::all_tool_schemas_with_env(|_| None)
            .iter()
            .filter_map(|schema| tool_schema_name(schema).map(str::to_string))
            .collect()
    }

    #[test]
    fn declarations_have_unique_names() {
        let mut seen = std::collections::BTreeSet::new();
        for declaration in all_tool_declarations() {
            assert!(
                seen.insert(declaration.name),
                "duplicate tool declaration: {}",
                declaration.name
            );
        }
    }

    /// Always-load names are derived from [`ToolDeclaration`] classification
    /// (see `surface::default_always_load_names()`), so consistency is
    /// guaranteed by construction. This test verifies that derivation contract.
    #[test]
    fn default_always_load_names_derived_from_always_load_declarations() {
        let derived: std::collections::BTreeSet<&str> =
            crate::tool_registry::surface::default_always_load_names()
                .iter()
                .copied()
                .collect();
        let declaration_always_load: std::collections::BTreeSet<&str> =
            crate::tool_registry::declaration::all_tool_declarations()
                .iter()
                .filter(|id| id.load_policy == ToolLoadPolicy::AlwaysLoad)
                .map(|id| id.name)
                .collect();
        assert_eq!(
            derived, declaration_always_load,
            "default_always_load_names() must include every AlwaysLoad declaration and nothing else"
        );
    }

    #[test]
    fn public_schema_names_have_public_declaration() {
        for name in schema_names() {
            let declaration = tool_declaration(&name)
                .unwrap_or_else(|| panic!("schema tool has no declaration: {name}"));
            assert!(
                declaration.load_policy.is_public_schema_policy(),
                "schema tool must not be internal or alias-only: {name}"
            );
        }
    }

    #[test]
    fn runtime_env_builtins_have_declaration() {
        let registry = astra_runtime_env::ToolRegistry::builtins();
        for spec in registry.iter() {
            assert!(
                tool_declaration(&spec.name).is_some(),
                "runtime-env builtin has no declaration: {}",
                spec.name
            );
        }
    }

    #[test]
    fn internal_names_do_not_have_public_schemas() {
        let schema_names = schema_names();
        for declaration in all_tool_declarations()
            .iter()
            .filter(|declaration| declaration.load_policy == ToolLoadPolicy::Internal)
        {
            assert!(
                !schema_names.contains(declaration.name),
                "internal declaration must not have a public schema: {}",
                declaration.name
            );
        }
    }
}
