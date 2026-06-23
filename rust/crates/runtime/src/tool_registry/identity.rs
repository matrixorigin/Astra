//! Stable identity metadata for known tool names.
//!
//! This module is deliberately not a surface builder. It only classifies names so the
//! schema catalog, runtime capability registry, and default pinned surface can
//! be checked for drift.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolPublicStatus {
    /// Visible in the default `tools[]` surface when the schema exists.
    Pinned,
    /// Advertised through the deferred catalog and activated with `tool_search`.
    Deferred,
    /// Callable only through a narrower control-plane path, not via the public
    /// schema catalog.
    ExplicitOnly,
    /// Runtime implementation detail. The model should not see this as a
    /// standalone public schema.
    Internal,
}

impl ToolPublicStatus {
    #[cfg(test)]
    pub const fn is_public_schema_status(self) -> bool {
        matches!(self, Self::Pinned | Self::Deferred | Self::ExplicitOnly)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolIdentity {
    pub name: &'static str,
    pub status: ToolPublicStatus,
    pub note: &'static str,
}

const fn pinned(name: &'static str, note: &'static str) -> ToolIdentity {
    ToolIdentity {
        name,
        status: ToolPublicStatus::Pinned,
        note,
    }
}

const fn deferred(name: &'static str, note: &'static str) -> ToolIdentity {
    ToolIdentity {
        name,
        status: ToolPublicStatus::Deferred,
        note,
    }
}

const fn explicit_only(name: &'static str, note: &'static str) -> ToolIdentity {
    ToolIdentity {
        name,
        status: ToolPublicStatus::ExplicitOnly,
        note,
    }
}

const fn internal(name: &'static str, note: &'static str) -> ToolIdentity {
    ToolIdentity {
        name,
        status: ToolPublicStatus::Internal,
        note,
    }
}

static TOOL_IDENTITIES: &[ToolIdentity] = &[
    pinned("ask_user", "structured clarification"),
    pinned("bash", "core shell escape hatch"),
    pinned("git", "core version-control observability"),
    pinned("glob", "core file discovery"),
    pinned("grep", "core content search"),
    pinned("list_dir", "core directory inspection"),
    pinned("memory", "intrinsic memory"),
    pinned("read_file", "core file read"),
    pinned("skill", "runtime-injected skill activation"),
    pinned("str_replace", "core targeted edit"),
    pinned("task", "visible task board"),
    pinned("tool_search", "deferred activation primitive"),
    pinned("write_file", "core file write/delete"),
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

pub fn all_tool_identities() -> &'static [ToolIdentity] {
    TOOL_IDENTITIES
}

#[cfg(test)]
pub fn tool_identity(name: &str) -> Option<&'static ToolIdentity> {
    TOOL_IDENTITIES
        .iter()
        .find(|identity| identity.name == name)
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
    fn identities_have_unique_names() {
        let mut seen = std::collections::BTreeSet::new();
        for identity in all_tool_identities() {
            assert!(
                seen.insert(identity.name),
                "duplicate tool identity: {}",
                identity.name
            );
        }
    }

    /// Renamed: pinned names are now derived from [`ToolIdentity`] classification
    /// (see `surface::default_pinned_names()`), so identity-pinned consistency is
    /// guaranteed by construction. This test verifies the derivation contract.
    #[test]
    fn default_pinned_names_derived_from_pinned_identities() {
        let derived: std::collections::BTreeSet<&str> =
            crate::tool_registry::surface::default_pinned_names()
                .iter()
                .copied()
                .collect();
        let identity_pinned: std::collections::BTreeSet<&str> =
            crate::tool_registry::identity::all_tool_identities()
                .iter()
                .filter(|id| id.status == ToolPublicStatus::Pinned)
                .map(|id| id.name)
                .collect();
        assert_eq!(
            derived, identity_pinned,
            "default_pinned_names() must include every Pinned identity and nothing else"
        );
    }

    #[test]
    fn public_schema_names_have_public_identity() {
        for name in schema_names() {
            let identity = tool_identity(&name)
                .unwrap_or_else(|| panic!("schema tool has no identity: {name}"));
            assert!(
                identity.status.is_public_schema_status(),
                "schema tool must not be internal or alias-only: {name}"
            );
        }
    }

    #[test]
    fn runtime_env_builtins_have_identity() {
        let registry = astra_runtime_env::ToolRegistry::builtins();
        for spec in registry.iter() {
            assert!(
                tool_identity(&spec.name).is_some(),
                "runtime-env builtin has no identity: {}",
                spec.name
            );
        }
    }

    #[test]
    fn internal_names_do_not_have_public_schemas() {
        let schema_names = schema_names();
        for identity in all_tool_identities()
            .iter()
            .filter(|identity| identity.status == ToolPublicStatus::Internal)
        {
            assert!(
                !schema_names.contains(identity.name),
                "internal identity must not have a public schema: {}",
                identity.name
            );
        }
    }
}
