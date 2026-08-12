//! Tool surface — T1 always_load + T2 deferred model.
//!
//! See `plans/tool-surface-deferred-simplification-2026-06-23.md` and
//! `docs/design/skills-and-tools.md` for the architectural story. Short version:
//!
//! - **T1 always_load** = a small, stable set of candidate tool schemas. After
//!   this declaration step, the server still applies the runtime
//!   provider/binding/capability gate before anything reaches the LLM
//!   `tools[]` array. The post-gate bytes stay stable across a session so the
//!   Anthropic/Bedrock prompt cache can hit the whole prefix.
//! - **T2 deferred** = every other known tool, listed as `name + short_desc`
//!   in a system-reminder block. The model activates one by calling
//!   `tool_search(query="select:NAME")`. Selecting a deferred tool makes its
//!   schema visible in upcoming `tools[]` payloads until the model actually
//!   calls that tool once.
//!
//! The default T1 candidate set is derived from
//! `astra_runtime_env::ToolSpec::load_policy`. It intentionally is not the
//! final visible surface for every access mode: runs without a file-environment
//! provider must hide workspace/process executor tools, while CLI/edge/sandbox/
//! managed runtimes may expose them when their provider binding is ready.
//! Users can add extra T1 tools via `runtime.tool_surface.pinned_tools` in TOML.
//!
//! Implementation is complete and wired into production.

use astra_config::ToolSurfaceConfig;
use astra_turn_core::tool::schema::tool_schema_name;
use astra_turn_core::tool_registry_report::{ToolSurfaceSnapshot, ToolSurfaceTierCounts};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::LazyLock;

/// Default T1 always_load candidate tool names, derived from the single
/// authority [`astra_runtime_env::ToolSpec`] classification.
/// Any name classified as `ToolLoadPolicy::AlwaysLoad` automatically
/// appears here — no manual copy needed. Callers must still apply the current
/// provider/binding/capability filter before exposing schemas to a model.
pub fn default_always_load_names() -> &'static [String] {
    static NAMES: LazyLock<Vec<String>> = LazyLock::new(|| {
        let registry = astra_runtime_env::ToolRegistry::builtins();
        let mut names: Vec<String> = registry
            .iter()
            .filter(|spec| spec.load_policy == astra_runtime_env::ToolLoadPolicy::AlwaysLoad)
            .map(|spec| spec.name.clone())
            .collect();
        names.sort_unstable();

        assert_always_load_schemas(&names);

        names
    });
    &NAMES
}

fn canonical_builtin_surface_schema_names() -> std::collections::BTreeSet<String> {
    let mut schemas = astra_tools::schemas::all_tool_schemas();
    schemas.push(crate::turn::skill_tool::skill_tool_schema_v2());
    schemas
        .iter()
        .filter_map(|schema| tool_schema_name(schema).map(str::to_string))
        .collect()
}

pub(crate) fn missing_always_load_schema_names(always_load_names: &[String]) -> Vec<String> {
    let schema_names = canonical_builtin_surface_schema_names();
    always_load_names
        .iter()
        .filter(|name| !schema_names.contains(name.as_str()))
        .cloned()
        .collect()
}

fn assert_always_load_schemas(always_load_names: &[String]) {
    let missing = missing_always_load_schema_names(always_load_names);
    assert!(
        missing.is_empty(),
        "AlwaysLoad tools missing schemas in canonical builtin surface pool: {}",
        missing.join(", ")
    );
}

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
    pub omitted_names: Vec<String>,
}

/// The resolved tool surface for a session.
pub struct ToolSurface {
    always_load: Vec<Value>,
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
    /// 1. Start from names classified as `ToolLoadPolicy::AlwaysLoad`.
    /// 2. Apply `cfg.pinned_tools`: a known tool name adds that tool to
    ///    always_load. Unknown or malformed entries are ignored.
    /// 3. Partition the union of catalog + plugins: names in the resolved
    ///    always_load set → `always_load_schemas`; everything else → `deferred`.
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
                if tool_name_is_forbidden_model_surface(name) {
                    tracing::warn!(
                        target: "astra.tool_surface",
                        name,
                        "tool surface: forbidden schema name '{name}' ignored"
                    );
                    continue;
                }
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

        // Resolve the always_load name set: defaults plus additive overrides.
        // Unknown names emit a warning — they are likely typos or stale
        // entries after a tool was renamed.
        let mut always_load_names: std::collections::BTreeSet<String> = default_always_load_names()
            .iter()
            .map(|s| s.to_string())
            .collect();
        for entry in &cfg.pinned_tools {
            let trimmed = entry.trim();
            if trimmed.is_empty() {
                continue;
            }
            if by_name.contains_key(trimmed) {
                always_load_names.insert(trimmed.to_string());
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
        let mut always_load: Vec<Value> = Vec::new();
        let mut deferred: Vec<DeferredEntry> = Vec::new();
        let registry = astra_runtime_env::ToolRegistry::builtins();
        for (name, schema) in by_name {
            if registry.get(&name).is_some_and(|spec| {
                spec.load_policy == astra_runtime_env::ToolLoadPolicy::RequestScoped
            }) {
                continue;
            }
            if always_load_names.contains(&name) {
                always_load.push(schema);
            } else {
                let short_desc = short_description(&schema);
                deferred.push(DeferredEntry { name, short_desc });
            }
        }

        Self {
            always_load,
            deferred,
        }
    }

    /// Build a deferred manifest from the eligible schema pool after the caller
    /// has already decided the final visible `tools[]` set for this turn.
    ///
    /// `visible_names` is authoritative for the current request: a tool that
    /// already appears in `tools[]` must not also be advertised as deferred.
    /// The deferred manifest is discovery metadata for tools that still require
    /// explicit activation, not a second copy of the visible surface.
    pub fn build_excluding_visible(
        catalog_schemas: Vec<Value>,
        cfg: &ToolSurfaceConfig,
        plugin_schemas: &[Value],
        visible_names: &HashSet<String>,
    ) -> Self {
        let plugin_names: HashSet<String> = plugin_schemas
            .iter()
            .filter_map(|schema| tool_schema_name(schema).map(str::to_string))
            .collect();

        // Callers may pass a mixed all-schemas pool that already contains
        // plugin/MCP names. Remove plugin names from the catalog half first,
        // then apply the same visible-name exclusion to both catalog and
        // dynamic schemas so the deferred manifest is disjoint from tools[].
        let catalog_schemas: Vec<Value> = catalog_schemas
            .into_iter()
            .filter(|schema| {
                tool_schema_name(schema).is_none_or(|name| {
                    !plugin_names.contains(name) && !visible_names.contains(name)
                })
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

    /// The byte-stable T1 candidate schemas.
    ///
    /// This is the declaration-level surface. Server/Web paths must pass these
    /// schemas through `tool_binding_projection` before feeding `tools[]`.
    /// CLI/edge/local paths should likewise use their resolved runtime binding.
    ///
    /// Returned by value so callers can annotate `cache_control` without
    /// mutating the surface.
    pub fn always_load_schemas(&self) -> Vec<Value> {
        self.always_load.clone()
    }

    /// Resolved always_load names in the same stable order as [`always_load_schemas`].
    ///
    /// This is the single runtime answer to "which tools are T1 for this
    /// surface?". Callers that need cache markers, edge metadata, or diagnostics
    /// should derive from the resolved surface instead of rebuilding the
    /// declaration + TOML addition rules locally.
    pub fn always_load_names(&self) -> Vec<String> {
        self.always_load
            .iter()
            .filter_map(|schema| tool_schema_name(schema).map(str::to_string))
            .collect()
    }

    pub fn snapshot(&self) -> ToolSurfaceSnapshot {
        ToolSurfaceSnapshot {
            visible_tools: self.always_load_names(),
            tier_counts: ToolSurfaceTierCounts {
                always_load: self.always_load.len().min(u32::MAX as usize) as u32,
                deferred_active: 0,
                deferred_available: self.deferred.len().min(u32::MAX as usize) as u32,
            },
        }
    }

    /// The deferred manifest — one `name + short_desc` entry per non-always_load
    /// tool, ready to render into the system-reminder block.
    pub fn deferred(&self) -> &[DeferredEntry] {
        &self.deferred
    }

    pub fn deferred_block_text_with_context_window(
        &self,
        context_window: Option<u32>,
    ) -> Option<String> {
        crate::prompts::build_deferred_tools_section_with_budget(self, context_window)
            .map(|section| section.text)
    }

    pub fn deferred_manifest_with_context_window(
        &self,
        context_window_tokens: Option<u32>,
    ) -> Option<DeferredManifest> {
        if self.deferred.is_empty() {
            return None;
        }
        let context_window = context_window_tokens
            .map(|value| value as usize)
            .unwrap_or(crate::prompts::DEFAULT_CONTEXT_WINDOW_TOKENS);
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
            omitted_names: block.omitted_names,
        })
    }
}

fn builtin_tool_is_internal(name: &str) -> bool {
    static REGISTRY: LazyLock<astra_runtime_env::ToolRegistry> =
        LazyLock::new(astra_runtime_env::ToolRegistry::builtins);
    REGISTRY
        .get(name)
        .is_some_and(|spec| !spec.load_policy.is_public_schema_policy())
}

fn tool_name_is_forbidden_model_surface(name: &str) -> bool {
    builtin_tool_is_internal(name)
}

/// Truncate the schema description to a compact UTF-8 char-boundary summary.
///
/// The summary is discovery metadata, not a full schema. The cap is long
/// enough for one complete load-bearing sentence so deferred listings do not
/// cut off required shape constraints like per-action fields or count
/// invariants.
fn short_description(schema: &Value) -> String {
    let function = schema.get("function");
    let raw = function
        .and_then(|f| f.get("parameters"))
        .and_then(|parameters| parameters.get("x-astra-discovery-summary"))
        .and_then(Value::as_str)
        .or_else(|| {
            function
                .and_then(|f| f.get("description"))
                .and_then(Value::as_str)
        })
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
