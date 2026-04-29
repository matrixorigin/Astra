//! Concurrency-safety registry for **dynamic / MCP tools**.
//!
//! Static tools are classified by [`crate::tool_categories::classify`]
//! which is args-aware and authoritative. This module provides a
//! process-wide registry for tools discovered at runtime (MCP servers,
//! plugins) that are not in the static tool table.
//!
//! ## Levels
//!
//! * [`ConcurrencySafety::ReadOnly`] — pure reads, may run fully in
//!   parallel with each other.
//! * [`ConcurrencySafety::Mutating`] — may modify state; serialized
//!   after all read-only calls complete.
//! * [`ConcurrencySafety::Serial`] — must be the only tool running.
//! * [`ConcurrencySafety::Unknown`] — fallback; treated as `Mutating`.
//!
//! ## Usage
//!
//! MCP tools call [`global_register`] on load. The parallel executor
//! falls back to [`global_is_parallelizable`] for names not in the
//! static table.

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

/// Declared concurrency semantics for a single tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConcurrencySafety {
    ReadOnly,
    Mutating,
    Serial,
    Unknown,
}

impl ConcurrencySafety {
    /// `true` when this level allows concurrent execution with other
    /// read-only tools.
    pub fn is_parallelizable(self) -> bool {
        matches!(self, Self::ReadOnly)
    }

    /// `true` when this level must be the sole running tool.
    pub fn is_strictly_serial(self) -> bool {
        matches!(self, Self::Serial)
    }
}

/// Tools that opt into concurrency declarations implement this trait.
///
/// Typically a constant method on a tool struct:
/// ```ignore
/// impl ConcurrencySafetyDeclaration for ReadFileTool {
///     fn concurrency_safety() -> ConcurrencySafety {
///         ConcurrencySafety::ReadOnly
///     }
/// }
/// ```
pub trait ConcurrencySafetyDeclaration {
    fn concurrency_safety() -> ConcurrencySafety;
}

/// Name → safety level registry for runtime lookup.
///
/// MCP tools and any dynamically-discovered tool register themselves
/// here on load. Static tools can be seeded via
/// [`Self::bootstrap_default`] which mirrors the canonical read-only
/// list.
#[derive(Debug, Default, Clone)]
pub struct ConcurrencySafetyRegistry {
    entries: HashMap<String, ConcurrencySafety>,
}

impl ConcurrencySafetyRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create an empty registry for MCP / dynamic tool registrations.
    ///
    /// Static tools are classified by [`crate::tool_categories::classify`]
    /// which is consulted first by the parallel executor. This registry
    /// is only a fallback for dynamically discovered tools.
    pub fn bootstrap_default() -> Self {
        Self::new()
    }

    /// Register or override a tool's safety level.
    pub fn register(&mut self, tool_name: &str, level: ConcurrencySafety) {
        self.entries.insert(tool_name.to_string(), level);
    }

    /// Bulk register from an iterator.
    pub fn register_all<I, S>(&mut self, it: I, level: ConcurrencySafety)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for n in it {
            self.register(n.as_ref(), level);
        }
    }

    /// Look up a tool's declared safety. Unknown tools return
    /// [`ConcurrencySafety::Unknown`] — callers should treat Unknown as
    /// Mutating for safety.
    pub fn classify(&self, tool_name: &str) -> ConcurrencySafety {
        self.entries
            .get(tool_name)
            .copied()
            .unwrap_or(ConcurrencySafety::Unknown)
    }

    /// `true` when the tool is registered as [`ConcurrencySafety::ReadOnly`].
    /// Drop-in replacement for `parallel_tool_exec::is_read_only_tool` once
    /// the registry is wired as the source of truth.
    pub fn is_parallelizable(&self, tool_name: &str) -> bool {
        self.classify(tool_name).is_parallelizable()
    }

    /// Number of registered tools.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterate over `(name, safety)` pairs.
    pub fn iter(&self) -> impl Iterator<Item = (&str, ConcurrencySafety)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), *v))
    }
}

// ── Global registry (MCP / dynamic tools only) ──────────────────────────
//
// Static tools are classified by `tool_categories::classify()` which is
// consulted first. This global registry is the fallback for MCP and
// dynamically-discovered tools that call `global_register` on load.

fn global_cell() -> &'static RwLock<ConcurrencySafetyRegistry> {
    static CELL: OnceLock<RwLock<ConcurrencySafetyRegistry>> = OnceLock::new();
    CELL.get_or_init(|| RwLock::new(ConcurrencySafetyRegistry::bootstrap_default()))
}

/// Classify a tool via the process-wide default registry. Returns
/// [`ConcurrencySafety::Unknown`] for unregistered names.
pub fn global_classify(tool_name: &str) -> ConcurrencySafety {
    global_cell()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .classify(tool_name)
}

/// Register or override a tool on the process-wide default registry.
pub fn global_register(tool_name: &str, level: ConcurrencySafety) {
    global_cell()
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .register(tool_name, level);
}

/// `true` when the process-wide registry marks the tool as read-only.
pub fn global_is_parallelizable(tool_name: &str) -> bool {
    global_classify(tool_name).is_parallelizable()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeTool;
    impl ConcurrencySafetyDeclaration for FakeTool {
        fn concurrency_safety() -> ConcurrencySafety {
            ConcurrencySafety::ReadOnly
        }
    }

    #[test]
    fn level_parallelizable_only_for_read_only() {
        assert!(ConcurrencySafety::ReadOnly.is_parallelizable());
        assert!(!ConcurrencySafety::Mutating.is_parallelizable());
        assert!(!ConcurrencySafety::Serial.is_parallelizable());
        assert!(!ConcurrencySafety::Unknown.is_parallelizable());
    }

    #[test]
    fn level_strictly_serial_only_for_serial() {
        assert!(ConcurrencySafety::Serial.is_strictly_serial());
        assert!(!ConcurrencySafety::Mutating.is_strictly_serial());
        assert!(!ConcurrencySafety::ReadOnly.is_strictly_serial());
        assert!(!ConcurrencySafety::Unknown.is_strictly_serial());
    }

    #[test]
    fn empty_registry_reports_unknown() {
        let r = ConcurrencySafetyRegistry::new();
        assert_eq!(r.classify("anything"), ConcurrencySafety::Unknown);
        assert!(!r.is_parallelizable("anything"));
    }

    #[test]
    fn register_and_classify_roundtrip() {
        let mut r = ConcurrencySafetyRegistry::new();
        r.register("grep", ConcurrencySafety::ReadOnly);
        assert_eq!(r.classify("grep"), ConcurrencySafety::ReadOnly);
        assert!(r.is_parallelizable("grep"));
    }

    #[test]
    fn register_overrides_previous_value() {
        let mut r = ConcurrencySafetyRegistry::new();
        r.register("tool", ConcurrencySafety::ReadOnly);
        r.register("tool", ConcurrencySafety::Mutating);
        assert_eq!(r.classify("tool"), ConcurrencySafety::Mutating);
    }

    #[test]
    fn register_all_applies_single_level() {
        let mut r = ConcurrencySafetyRegistry::new();
        r.register_all(["a", "b", "c"], ConcurrencySafety::ReadOnly);
        for n in ["a", "b", "c"] {
            assert_eq!(r.classify(n), ConcurrencySafety::ReadOnly);
        }
    }

    #[test]
    fn bootstrap_default_is_empty() {
        let r = ConcurrencySafetyRegistry::bootstrap_default();
        assert!(
            r.is_empty(),
            "bootstrap should be empty — static tools use tool_categories::classify"
        );
    }

    #[test]
    fn static_tools_classified_via_tool_categories() {
        // Static tools are NOT in the registry — they go through
        // tool_categories::classify which is the authoritative path.
        assert!(super::super::parallel_tool_exec::is_read_only_tool(
            "read_file"
        ));
        assert!(super::super::parallel_tool_exec::is_read_only_tool("grep"));
        assert!(super::super::parallel_tool_exec::is_read_only_tool("glob"));
        assert!(!super::super::parallel_tool_exec::is_read_only_tool("bash"));
        assert!(!super::super::parallel_tool_exec::is_read_only_tool(
            "write_file"
        ));
    }

    #[test]
    fn iter_yields_all_registered_entries() {
        let mut r = ConcurrencySafetyRegistry::new();
        r.register("a", ConcurrencySafety::ReadOnly);
        r.register("b", ConcurrencySafety::Mutating);
        let collected: Vec<_> = r.iter().collect();
        assert_eq!(collected.len(), 2);
    }

    #[test]
    fn declaration_trait_compile_check() {
        assert_eq!(FakeTool::concurrency_safety(), ConcurrencySafety::ReadOnly);
    }

    #[test]
    fn global_register_opts_mcp_tool_into_parallelizable() {
        let name = "mcp_test_global_register_tool";
        // Pre-registration: unknown in both paths.
        assert_eq!(global_classify(name), ConcurrencySafety::Unknown);
        assert!(!super::super::parallel_tool_exec::is_read_only_tool(name));
        // Register via the global dynamic registry.
        global_register(name, ConcurrencySafety::ReadOnly);
        // Post-registration: the fallback path picks it up.
        assert_eq!(global_classify(name), ConcurrencySafety::ReadOnly);
        assert!(global_is_parallelizable(name));
        assert!(super::super::parallel_tool_exec::is_read_only_tool(name));
    }

    #[test]
    fn unknown_dynamic_tool_is_not_parallelizable() {
        assert_eq!(
            global_classify("brand_new_mcp_tool"),
            ConcurrencySafety::Unknown
        );
        assert!(!global_is_parallelizable("brand_new_mcp_tool"));
    }
}
