//! Tool surface — T1 pinned + T2 deferred model.
//!
//! See `memory/project_tool_surface_rewrite.md` and the plan file for the
//! architectural story. Short version:
//!
//! - **T1 pinned** = a small, stable set of tool schemas that go into the
//!   LLM `tools[]` array on every turn. Byte-stable across a session so the
//!   Anthropic/Bedrock prompt cache can hit the whole prefix.
//! - **T2 deferred** = every other known tool, listed as `name + short_desc`
//!   in a system-reminder block. The model activates one by calling
//!   `tool_search(query="select:NAME")`. Selecting a deferred tool makes its
//!   schema visible in upcoming `tools[]` payloads until the model actually
//!   calls that tool once.
//!
//! The default T1 set is the 13-member coding core (see `DEFAULT_PINNED`).
//! Users override via `runtime.tool_surface.pinned_tools` in TOML. A name
//! prefixed with `-` removes a default (e.g. `"-grep"`).
//!
//! Implementation is complete and wired into production. See
//! `memory/project_tool_surface_rewrite.md` for the architectural story.

use astra_config::ToolSurfaceConfig;
use astra_turn_core::tool::schema::tool_schema_name;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashSet;

/// Default T1 pinned tools — the coding golden path + astra intrinsics +
/// activation primitives.
///
/// Rationale for each inclusion:
/// - `bash`, `read_file`, `write_file`, `str_replace` — the coding four.
/// - `grep`, `glob`, `list_dir` — file discovery, used in 80%+ of turns.
/// - `memory` — astra's intrinsic memory capability (cf. TOOL_CATALOG
///   comment at tool_registry_meta:408 — "memory is pinned, intrinsic
///   store/retrieve capability"). Deferring it would force a
///   round-trip for every memory op.
/// - `git` — read-mostly VCS observability (`status`, `diff`, `log`) is part
///   of the normal coding loop and is referenced directly by the system
///   prompt. Deferring it makes "show diff" pay an activation round and
///   contradicts that contract.
/// - `skill`, `tool_search` — the two activation primitives. Needed to
///   reach anything in the deferred list.
/// - `task` — session_todos surface. The TUI's task dashboard lights up
///   only when the model emits `task.create / update`. Leaving it in
///   T2 means the model rarely activates it via `tool_search`, and the
///   board never shows up — defeating its purpose. Pin it so multi-step
///   work is visible by default.
/// - `ask_user` — the structured clarification primitive. It is pinned as
///   core interaction vocabulary, then removed by interaction-mode filtering
///   in Auto/NonInteractive/Headless turns where no user prompt sink exists.
///
/// Explicitly deferred: `github`, `mo`, `agent`, `web_fetch`,
/// `introspect`, `lsp`, `notify`, `session`, `symbols`, `powershell`,
/// `run_script`. Users pin via config as needed.
pub const DEFAULT_PINNED: &[&str] = &[
    "ask_user",
    "bash",
    "git",
    "glob",
    "grep",
    "list_dir",
    "memory",
    "read_file",
    "skill",
    "str_replace",
    "task",
    "tool_search",
    "write_file",
];

/// One entry in the deferred manifest.
///
/// Deliberately minimal: `name + short_desc`. No parameters, no schema — the
/// whole point of the deferred layer is that schema lives only in the
/// catalog until the model explicitly pulls it with `tool_search`.
#[derive(Clone, Debug, Serialize)]
pub struct DeferredEntry {
    pub name: String,
    pub short_desc: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeferredManifest {
    pub text: String,
    pub context_window: usize,
    pub names: Vec<String>,
}

/// The resolved tool surface for a session.
pub struct ToolSurface {
    pinned: Vec<Value>,
    deferred: Vec<DeferredEntry>,
}

impl ToolSurface {
    /// Build a `ToolSurface` using the process-wide runtime config.
    pub fn from_runtime_config(all_schemas: &[Value]) -> Self {
        let cfg = astra_config::runtime_config::RuntimeConfig::cached()
            .tool_surface
            .clone();
        Self::build(all_schemas.to_vec(), &cfg, &[])
    }

    /// Build a `ToolSurface` from a catalog snapshot, user config, and
    /// plugin schemas registered this session.
    ///
    /// Algorithm:
    /// 1. Start from [`DEFAULT_PINNED`].
    /// 2. Apply `cfg.pinned_tools`: a bare name adds, a `-name` removes.
    ///    Unknown names are silently ignored.
    /// 3. Partition the union of catalog + plugins: names in the resolved
    ///    pinned set → `pinned_schemas`; everything else → `deferred`.
    /// 4. Sort both alphabetically for byte-stability.
    pub fn build(
        catalog_schemas: Vec<Value>,
        cfg: &ToolSurfaceConfig,
        plugin_schemas: &[Value],
    ) -> Self {
        // Fold all known schemas into a single (name → schema) map. Plugin
        // schemas override catalog entries with the same name — plugins
        // are user-registered and authoritative for their own tool.
        let mut by_name: std::collections::BTreeMap<String, Value> =
            std::collections::BTreeMap::new();
        for schema in catalog_schemas
            .into_iter()
            .chain(plugin_schemas.iter().cloned())
        {
            if let Some(name) = tool_schema_name(&schema) {
                if by_name.contains_key(name) {
                    tracing::warn!(
                        target: "astra.tool_surface",
                        name,
                        "tool surface: schema name collision — '{name}' is already registered; overwriting with later entry"
                    );
                }
                by_name.insert(name.to_string(), schema);
            }
        }

        // Resolve the pinned name set: defaults + additive overrides, minus
        // any `-name` removals. Unknown names emit a warning — they are likely
        // typos (`+web_fetc`) or stale entries after a tool was renamed.
        let mut pinned_names: std::collections::BTreeSet<String> =
            DEFAULT_PINNED.iter().map(|s| (*s).to_string()).collect();
        for entry in &cfg.pinned_tools {
            let trimmed = entry.trim();
            // Empty / whitespace-only / bare "-" / double-prefixed "--" are
            // user-config noise — silently skip. The fence against "-- foo"
            // becoming "-foo" (which tries to remove a tool named "-foo")
            // is also a bug prevention.
            if trimmed.is_empty() || trimmed == "-" || trimmed.starts_with("--") {
                continue;
            }
            if let Some(name) = trimmed.strip_prefix('-') {
                pinned_names.remove(name);
            } else if by_name.contains_key(trimmed) {
                pinned_names.insert(trimmed.to_string());
            } else {
                tracing::warn!(
                    target: "astra.tool_surface",
                    entry = trimmed,
                    "tool_surface.pinned_tools: unknown tool name '{trimmed}' ignored — typo or renamed tool?"
                );
            }
        }

        // Partition. BTreeMap iteration is already alphabetical, so the
        // resulting vectors come out sorted for free.
        let mut pinned: Vec<Value> = Vec::new();
        let mut deferred: Vec<DeferredEntry> = Vec::new();
        for (name, schema) in by_name {
            if pinned_names.contains(&name) {
                pinned.push(schema);
            } else {
                let short_desc = short_description(&schema);
                deferred.push(DeferredEntry { name, short_desc });
            }
        }

        Self { pinned, deferred }
    }

    /// Build a deferred manifest from the eligible schema pool after the caller
    /// has already decided the final visible `tools[]` set for this turn.
    ///
    /// This keeps the wire contract self-consistent: a tool is either directly
    /// visible in `tools[]` or advertised as deferred/searchable, never both.
    pub fn build_excluding_visible(
        catalog_schemas: Vec<Value>,
        cfg: &ToolSurfaceConfig,
        plugin_schemas: &[Value],
        visible_names: &HashSet<String>,
    ) -> Self {
        let catalog_schemas: Vec<Value> = catalog_schemas
            .into_iter()
            .filter(|schema| {
                tool_schema_name(schema).is_none_or(|name| !visible_names.contains(name))
            })
            .collect();
        let plugin_schemas: Vec<Value> = plugin_schemas
            .iter()
            .filter(|schema| {
                tool_schema_name(schema).is_none_or(|name| !visible_names.contains(name))
            })
            .cloned()
            .collect();
        Self::build(catalog_schemas, cfg, &plugin_schemas)
    }

    /// The byte-stable T1 schemas to feed into `tools[]`.
    ///
    /// Returned by value so callers can annotate `cache_control` without
    /// mutating the surface.
    pub fn pinned_schemas(&self) -> Vec<Value> {
        self.pinned.clone()
    }

    /// The deferred manifest — one `name + short_desc` entry per non-pinned
    /// tool, ready to render into the system-reminder block.
    pub fn deferred(&self) -> &[DeferredEntry] {
        &self.deferred
    }

    /// Render the deferred manifest into the session-stable prompt block.
    ///
    /// `model` is the LLM model id (e.g. `"claude-sonnet-4"`, `"gpt-4o"`) — used
    /// to resolve the per-provider context window and size the listing budget.
    /// Pass `None` to use the default 16K-char fallback.
    pub fn deferred_block_text(&self, model: Option<&str>) -> Option<String> {
        let context_window =
            u32::try_from(crate::prompts::budget_for_model(model).model_limit).ok();
        crate::prompts::build_deferred_tools_section_with_budget(self, context_window)
            .map(|section| section.text)
    }

    pub fn deferred_manifest(&self, model: Option<&str>) -> Option<DeferredManifest> {
        if self.deferred.is_empty() {
            return None;
        }
        let context_window = crate::prompts::budget_for_model(model).model_limit;
        let context_window_u32 = u32::try_from(context_window).ok();
        let block = crate::prompts::build_deferred_tools_prompt_block_with_budget(
            self,
            context_window_u32,
        )?;
        let text = block.section.text;
        if text.trim().is_empty() || block.names.is_empty() {
            return None;
        }
        Some(DeferredManifest {
            text,
            context_window,
            names: block.names,
        })
    }
}

/// Truncate the schema description to a compact UTF-8 char-boundary summary.
///
/// The summary is discovery metadata, not a full schema. The cap is long
/// enough for one complete load-bearing sentence so deferred listings do not
/// cut off required shape constraints like per-action fields or count
/// invariants.
fn short_description(schema: &Value) -> String {
    let raw = schema
        .get("function")
        .and_then(|f| f.get("description"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    const MAX: usize = 180;
    if raw.chars().count() <= MAX {
        return raw.to_string();
    }
    let mut out = String::new();
    for (i, ch) in raw.chars().enumerate() {
        if i + 1 >= MAX {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}
