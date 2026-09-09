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
//!   in a system-reminder block. The model selects one with
//!   `tool_search(query="select:NAME")`, then invokes it through the stable
//!   `invoke_tool` carrier. Selection records a compact, schema-addressed
//!   contract; the target's full schema never re-enters `tools[]`.
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

/// Maximum compact-JSON bytes for the built-in default T1 schema set.
///
/// This is a regression budget, not a runtime truncation rule. Config-pinned
/// tools may intentionally exceed it. The default must remain below 8 KiB:
/// it is the cacheable prefix sent on every agentic provider request, not a
/// general catalog. Deferred discovery keeps the complete capability catalog
/// reachable without quietly turning that repeated prefix back into a second
/// system prompt. Resident schemas retain their executable types, constraints,
/// enums, and required fields, while verbose per-parameter prose remains in
/// the canonical catalog selected through `tool_search`.
#[cfg(test)]
pub(crate) const DEFAULT_ALWAYS_LOAD_SCHEMA_BYTE_BUDGET: usize = 8 * 1024;

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
            // This is intentionally evaluated in declaration order. A
            // deployment may start from the small stable default, remove a
            // costly resident schema, and (if needed) add it back later in
            // the same boundary-specific configuration. Treating `-name` as
            // an unknown name silently defeated that token and cache policy.
            let (name, pin) = match trimmed.strip_prefix('-') {
                Some(name) if !name.is_empty() && !name.starts_with('-') => (name, false),
                Some(_) => continue,
                None => (trimmed, true),
            };
            if !pin && name == "tool_search" {
                // The deferred catalog is only useful if the model retains
                // its one activation primitive. Allowing a boundary config to
                // defer `tool_search` produces a prompt that advertises
                // reachable capabilities without a callable way to select
                // them, which is a liveness failure rather than a cost
                // policy. Keep this protocol floor resident; every other T1
                // candidate remains configurable.
                tracing::warn!(
                    target: "astra.tool_surface",
                    "tool_surface.pinned_tools cannot defer required protocol tool 'tool_search'; entry ignored"
                );
            } else if by_name.contains_key(name) {
                if pin {
                    always_load_names.insert(name.to_string());
                } else {
                    always_load_names.remove(name);
                }
            } else {
                tracing::warn!(
                    target: "astra.tool_surface",
                    entry = trimmed,
                    "tool_surface.pinned_tools: unknown tool name '{name}' ignored — typo or renamed tool?"
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
                always_load.push(resident_schema_projection(&name, schema));
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

/// Project a high-frequency tool's resident schema to its ordinary operation.
///
/// The canonical catalog keeps the complete contract. A model can select that
/// contract explicitly with `tool_search`; deferred targets then use the stable
/// carrier, while a resident target remains directly callable. This is a
/// prompt-surface optimization rather than an executor capability reduction.
/// Keeping rare, safety-specialized fields off the default prefix prevents a
/// single broad tool from consuming the budget meant for every first request.
fn resident_schema_projection(name: &str, mut schema: Value) -> Value {
    let Some(function) = schema.get_mut("function").and_then(Value::as_object_mut) else {
        return schema;
    };
    let (resident_fields, description) = match name {
        "bash" => (
            &[
                "command",
                "workdir",
                "mode",
                "timeout",
                "force",
                // This optional observation contract is part of Bash's
                // stable resident shape. Adding it only while a terminal
                // obligation is pending changes the provider tool prefix
                // when that obligation settles and breaks prompt-cache
                // continuity. Runtime admission still validates the field
                // independently of this presentation contract.
                "external_state_paths",
                // These fields are present only on the CLI edge projection.
                // The shared/server Bash schema does not declare them, so
                // retaining them here cannot widen a foreground executor.
                "run_in_background",
                "ready_check",
                "background_ttl",
            ][..],
            "Run shell in the bounded workspace. Before external mutations, set external_state_paths to the smallest absolute external roots. Evidence requires an owned foreground delta. Omit for workspace-only or read-only work.",
        ),
        "str_replace" => (
            &[
                "path",
                "old_str",
                "new_str",
                "dry_run",
                "replace_all",
                "allow_structural_change",
            ][..],
            "Replace text in one file. Select str_replace with tool_search first for batch edits or structural-change overrides.",
        ),
        "ask_user" => (
            &["context", "questions"][..],
            "Ask the user focused clarification questions. Select ask_user with tool_search first for choices, headers, or multi-select.",
        ),
        "introspect" => (
            &[
                "topic",
                "facet",
                "depth",
                "horizon",
                "question",
                "source_policy",
                "include_context",
                "format",
                "artifact",
                "offset",
                "max_bytes",
            ][..],
            "Read bounded live runtime/session observations or a retained result artifact. Use reflect after tool_search for persisted causal history.",
        ),
        "memory" => (
            &["action", "content", "query", "memory_type"][..],
            "Store or recall persistent memory. remember requires content; recall requires query. Select memory with tool_search first for advanced operations.",
        ),
        "read_file" => (
            &["path", "start_line", "end_line", "outline"][..],
            "Read a bounded file range or return its code outline.",
        ),
        "list_dir" => (
            &["path", "depth"][..],
            "List entries in a bounded workspace directory.",
        ),
        "grep" => (
            &[
                "pattern",
                "path",
                "include",
                "case_sensitive",
                "fixed_strings",
                "max_matches",
                "output_mode",
            ][..],
            "Search workspace files with a bounded regular-expression query.",
        ),
        "glob" => (
            &["pattern", "path", "sort_by", "offset", "head_limit"][..],
            "Find workspace files matching a bounded glob pattern.",
        ),
        "write_file" => (
            &["path", "content", "delete"][..],
            "Create, overwrite, or delete one workspace file. Use str_replace for targeted edits.",
        ),
        "skill" => (
            &["skill_name", "task"][..],
            "Run a named skill from the available skill listing before substantive work.",
        ),
        "tool_search" => (
            &["query"][..],
            "Select deferred tools explicitly with select:NAME or select:NAME1,NAME2.",
        ),
        "notify" => (
            &["message", "notification_type"][..],
            "Send a user notification or status update.",
        ),
        "start_work" => (
            &["goal", "activation", "tasks"][..],
            "Declare current executable acceptance units as Work; add future, conditional, or replacement outcomes later via a revision-pinned proposal. start returns first assignment; defer records the graph without execution.",
        ),
        "run_next_work_item" => (
            &[][..],
            "Request the next canonical Work assignment only when start or settlement returned none.",
        ),
        "settle_work_item" => (
            &[
                "outcome",
                "summary",
                "blocker_kind",
                "unavailable_capabilities",
            ][..],
            "Settle the active Work attempt truthfully before the final response; use blocked or failed when delivery evidence is incomplete.",
        ),
        _ => return schema,
    };

    let Some(parameters) = function
        .get_mut("parameters")
        .and_then(Value::as_object_mut)
    else {
        return schema;
    };
    if name == "str_replace" {
        parameters.insert(
            "x-astra-per-action-required".to_string(),
            serde_json::json!({"single": ["path", "old_str", "new_str"]}),
        );
    } else if name == "memory" {
        parameters.insert(
            "x-astra-per-action-required".to_string(),
            serde_json::json!({"remember": ["content"], "recall": ["query"]}),
        );
        parameters.remove("x-astra-per-action-any-of-required");
    }
    // The projected schema is the exact provider authority for a resident
    // call, not documentation for a broader canonical contract. Once fields
    // are removed, close the reduced object so omitted advanced fields cannot
    // bypass deferred activation at execution admission.
    parameters.insert("additionalProperties".to_string(), Value::Bool(false));
    let Some(properties) = parameters
        .get_mut("properties")
        .and_then(Value::as_object_mut)
    else {
        return schema;
    };
    properties.retain(|field, _| resident_fields.contains(&field.as_str()));
    if name == "memory"
        && let Some(action) = properties.get_mut("action").and_then(Value::as_object_mut)
    {
        action.insert(
            "enum".to_string(),
            serde_json::json!(["remember", "recall"]),
        );
    }
    if name == "ask_user"
        && let Some(question_items) = properties
            .get_mut("questions")
            .and_then(Value::as_object_mut)
            .and_then(|questions| questions.get_mut("items"))
            .and_then(Value::as_object_mut)
    {
        if let Some(question_properties) = question_items
            .get_mut("properties")
            .and_then(Value::as_object_mut)
        {
            question_properties.retain(|field, _| field == "question");
        }
        question_items.insert("additionalProperties".to_string(), Value::Bool(false));
    }
    for property in properties.values_mut() {
        strip_resident_property_descriptions(property);
    }
    // A managed edge Bash schema is a real executable contract, not a rare
    // catalog-only option: without these fields the model cannot keep a
    // service alive across calls. Preserve the small lifecycle shape when the
    // edge executor supplied it, while the server-owned foreground schema
    // remains unchanged because it never contains these properties.
    let managed_background = name == "bash"
        && ["run_in_background", "ready_check", "background_ttl"]
            .iter()
            .all(|field| properties.contains_key(*field));
    if managed_background {
        if let Some(property) = properties.get_mut("run_in_background") {
            property["description"] = Value::String(
                "Keep the service alive after this call; requires ready_check.".to_string(),
            );
        }
        if let Some(property) = properties.get_mut("ready_check") {
            property["description"] = Value::String(
                "Independent side-effect-free command proving service readiness.".to_string(),
            );
        }
        if let Some(property) = properties.get_mut("background_ttl") {
            property["description"] =
                Value::String("Maximum managed service lifetime in seconds.".to_string());
        }
    }
    function.insert(
        "description".to_string(),
        Value::String(if managed_background {
            format!(
                "{description} For services that must survive this call, set run_in_background with an independent ready_check; neither supplies the foreground delta. Select the full contract with tool_search for artifact preservation."
            )
        } else {
            description.to_string()
        }),
    );
    schema
}

/// Remove prose that is useful in the canonical catalog but expensive in the
/// cacheable resident prefix. This deliberately preserves every structural
/// keyword (types, enums, bounds, defaults, required fields, and Astra
/// extension metadata), so projection changes token cost without changing the
/// executable contract.
fn strip_resident_property_descriptions(value: &mut Value) {
    match value {
        Value::Object(object) => {
            object.remove("description");
            for child in object.values_mut() {
                strip_resident_property_descriptions(child);
            }
        }
        Value::Array(values) => {
            for child in values {
                strip_resident_property_descriptions(child);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
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
        // `invoke_tool` is the runtime-owned deferred invocation protocol, not
        // a catalog capability. Letting a plugin or MCP server publish that
        // name would turn an untrusted schema collision into a control-plane
        // escape hatch. The carrier is assembled explicitly from its canonical
        // schema after catalog validation.
        || name == astra_turn_core::tool::deferred_activation::DEFERRED_TOOL_INVOCATION_CARRIER
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
