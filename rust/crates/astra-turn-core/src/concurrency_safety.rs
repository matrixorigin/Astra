//! Concurrency-safety metadata for tool calls (gap #3).
//!
//! Replaces the static `READ_ONLY_TOOLS` list in
//! [`crate::parallel_tool_exec`] with a registry-backed declaration so
//! tools (including MCP-provided ones) can register their own concurrency
//! semantics without patching a central list.
//!
//! ## Levels
//!
//! * [`ConcurrencySafety::ReadOnly`] — pure reads, may run fully in
//!   parallel with each other and with concurrent writes to unrelated
//!   resources.
//! * [`ConcurrencySafety::Mutating`] — may modify state; must not run
//!   concurrently with reads or writes of the same resource. The
//!   executor serializes these after all read-only calls complete.
//! * [`ConcurrencySafety::Serial`] — stronger form of mutating; must be
//!   the only tool running when it executes (e.g. shell / bash that
//!   affects arbitrary state). Sibling mutating tools are aborted on
//!   error to match claude-code semantics.
//! * [`ConcurrencySafety::Unknown`] — fallback when the tool isn't
//!   registered. The executor treats `Unknown` as `Mutating` to stay
//!   safe by default.
//!
//! ## Integration path
//!
//! The registry is additive: existing `is_read_only_tool` callers keep
//! working. A helper [`ConcurrencySafetyRegistry::bootstrap_default`]
//! seeds the canonical read-only list from `parallel_tool_exec` so
//! migration to the new trait can proceed tool-by-tool.

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

    /// Seed the registry with the canonical read-only tools. Call once
    /// at runtime startup; later callers may override individual entries
    /// via [`Self::register`].
    pub fn bootstrap_default() -> Self {
        let mut r = Self::new();
        for name in super::parallel_tool_exec::read_only_tool_names() {
            r.register(name, ConcurrencySafety::ReadOnly);
        }
        // The canonical mutating-and-serial tool is `bash` — sibling aborts
        // key off this in parallel_tool_exec.
        r.register("bash", ConcurrencySafety::Serial);
        r.register("BashTool", ConcurrencySafety::Serial);
        r.register("powershell", ConcurrencySafety::Serial);
        r.register("PowerShellTool", ConcurrencySafety::Serial);
        // Common file-mutating tools. Consumers may refine via register().
        for name in [
            "write_file",
            "WriteFileTool",
            "edit_file",
            "EditFileTool",
            "apply_patch",
            "ApplyPatchTool",
            "delete_file",
        ] {
            r.register(name, ConcurrencySafety::Mutating);
        }
        r
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

// ── Global registry ───────────────────────────────────────────────────────
//
// The global registry is the source of truth consulted by
// `parallel_tool_exec::is_read_only_tool` for names that are NOT in the static
// READ_ONLY_TOOLS list. MCP tools / dynamic tools call `global_register` on
// load and their declared safety is picked up transparently by the existing
// parallel-dispatch path.
//
// The global is additive: it never downgrades the static list (a tool in
// READ_ONLY_TOOLS is always treated as read-only) — it only provides a path
// for tools that would otherwise be Unknown to opt in.

fn global_cell() -> &'static RwLock<ConcurrencySafetyRegistry> {
    static CELL: OnceLock<RwLock<ConcurrencySafetyRegistry>> = OnceLock::new();
    CELL.get_or_init(|| RwLock::new(ConcurrencySafetyRegistry::bootstrap_default()))
}

/// Classify a tool via the process-wide default registry. Returns
/// [`ConcurrencySafety::Unknown`] for unregistered names.
pub fn global_classify(tool_name: &str) -> ConcurrencySafety {
    global_cell()
        .read()
        .expect("concurrency_safety global poisoned")
        .classify(tool_name)
}

/// Register or override a tool on the process-wide default registry.
pub fn global_register(tool_name: &str, level: ConcurrencySafety) {
    global_cell()
        .write()
        .expect("concurrency_safety global poisoned")
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
    fn bootstrap_default_seeds_canonical_read_only_tools() {
        let r = ConcurrencySafetyRegistry::bootstrap_default();
        // Representative tools from the canonical list.
        assert_eq!(r.classify("read_file"), ConcurrencySafety::ReadOnly);
        assert_eq!(r.classify("grep"), ConcurrencySafety::ReadOnly);
        assert_eq!(r.classify("git_status"), ConcurrencySafety::ReadOnly);
        assert!(r.is_parallelizable("glob"));
    }

    #[test]
    fn bootstrap_default_marks_bash_as_serial() {
        let r = ConcurrencySafetyRegistry::bootstrap_default();
        assert_eq!(r.classify("bash"), ConcurrencySafety::Serial);
        assert!(r.classify("bash").is_strictly_serial());
    }

    #[test]
    fn bootstrap_default_marks_writes_as_mutating() {
        let r = ConcurrencySafetyRegistry::bootstrap_default();
        assert_eq!(r.classify("write_file"), ConcurrencySafety::Mutating);
        assert_eq!(r.classify("edit_file"), ConcurrencySafety::Mutating);
        assert!(!r.is_parallelizable("write_file"));
    }

    #[test]
    fn bootstrap_default_unknown_tool_falls_through() {
        let r = ConcurrencySafetyRegistry::bootstrap_default();
        assert_eq!(
            r.classify("brand_new_mcp_tool"),
            ConcurrencySafety::Unknown
        );
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
    fn bootstrap_default_classification_matches_static_read_only_list() {
        // The registry should agree with the legacy is_read_only_tool for
        // every name in the canonical list — this guards against drift if
        // the list is updated without refreshing bootstrap_default.
        let r = ConcurrencySafetyRegistry::bootstrap_default();
        for name in super::super::parallel_tool_exec::read_only_tool_names() {
            assert!(
                r.is_parallelizable(name),
                "canonical read-only tool {name} not flagged parallelizable"
            );
            assert!(
                super::super::parallel_tool_exec::is_read_only_tool(name),
                "legacy classifier disagrees for {name}"
            );
        }
    }

    #[test]
    fn global_registry_bootstrap_mirrors_canonical_list() {
        // Global is bootstrapped lazily with bootstrap_default, so every
        // canonical read-only name must classify as ReadOnly there too.
        for name in super::super::parallel_tool_exec::read_only_tool_names() {
            assert_eq!(
                global_classify(name),
                ConcurrencySafety::ReadOnly,
                "global registry disagrees for {name}"
            );
            assert!(global_is_parallelizable(name));
        }
    }

    #[test]
    fn global_register_opts_new_tool_into_parallelizable() {
        // A fresh MCP-style tool not in the static list should be treated
        // as Unknown → mutating by default, then become parallelizable
        // once it declares itself ReadOnly via global_register.
        let name = "mcp_test_global_register_tool";
        // Pre-registration: unknown.
        assert_eq!(global_classify(name), ConcurrencySafety::Unknown);
        assert!(!super::super::parallel_tool_exec::is_read_only_tool(name));
        // Register.
        global_register(name, ConcurrencySafety::ReadOnly);
        // Post-registration: seen as read-only by both APIs.
        assert_eq!(global_classify(name), ConcurrencySafety::ReadOnly);
        assert!(super::super::parallel_tool_exec::is_read_only_tool(name));
    }
}
