//! Helpers for the deferred-tool activation contract.
//!
//! Deferred tool entries are discovery metadata. A tool becomes executable
//! only when it is already visible in `tools[]`, including a later request
//! after `tool_search(query="select:NAME")` activates its full schema.

use std::collections::{BTreeSet, HashSet};

use serde_json::Value;

/// Current tool surface installed by the runtime for admission/search.
///
/// `Uninstalled` means no LLM-request surface has been installed yet; callers
/// must fail closed because they cannot prove the model saw any tool schema.
/// `Installed { visible: ∅, activatable: ∅ }` is different: the runtime
/// deliberately sent a no-tool turn. That turn must not discard previously
/// activated deferred tools, because no schema-injection opportunity occurred.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ToolSurfaceNames {
    #[default]
    Uninstalled,
    Installed {
        visible: HashSet<String>,
        activatable: HashSet<String>,
    },
}

impl ToolSurfaceNames {
    #[must_use]
    pub fn installed(visible: HashSet<String>, activatable: HashSet<String>) -> Self {
        Self::Installed {
            visible,
            activatable,
        }
    }

    #[must_use]
    pub fn visible(&self) -> Option<&HashSet<String>> {
        match self {
            Self::Uninstalled => None,
            Self::Installed { visible, .. } => Some(visible),
        }
    }

    #[must_use]
    pub fn activatable(&self) -> Option<&HashSet<String>> {
        match self {
            Self::Uninstalled => None,
            Self::Installed { activatable, .. } => Some(activatable),
        }
    }

    #[must_use]
    pub fn has_any_tool(&self) -> bool {
        match self {
            Self::Uninstalled => false,
            Self::Installed {
                visible,
                activatable,
            } => !visible.is_empty() || !activatable.is_empty(),
        }
    }

    #[must_use]
    pub fn visible_contains(&self, name: &str) -> bool {
        self.visible().is_some_and(|visible| visible.contains(name))
    }

    #[must_use]
    pub fn activatable_contains(&self, name: &str) -> bool {
        self.activatable()
            .is_some_and(|activatable| activatable.contains(name))
    }
}

/// Build the per-turn tool-search pool from the tools that are already
/// visible plus the deferred tools advertised in this turn's prompt.
///
/// `None` means no caller-installed surface exists yet; callers should fail
/// closed instead of falling back to a global catalog.
#[must_use]
pub fn searchable_tool_names(surface: &ToolSurfaceNames) -> Option<HashSet<String>> {
    match surface {
        ToolSurfaceNames::Uninstalled => None,
        ToolSurfaceNames::Installed {
            visible,
            activatable,
        } => {
            let mut names = HashSet::new();
            names.extend(visible.iter().cloned());
            names.extend(activatable.iter().cloned());
            Some(names)
        }
    }
}

/// Keep only tool names that the current runtime can actually execute.
#[must_use]
pub fn runtime_bound_tool_names<F>(
    names: HashSet<String>,
    has_runtime_binding: F,
) -> HashSet<String>
where
    F: Fn(&str) -> bool,
{
    names
        .into_iter()
        .filter(|name| has_runtime_binding(name))
        .collect()
}

/// Build the tool-search pool and remove names that the current runtime
/// cannot execute. This keeps `tool_search` aligned with the real execution
/// surface instead of cached or declarative schemas.
#[must_use]
pub fn searchable_runtime_bound_tool_names<F>(
    surface: &ToolSurfaceNames,
    has_runtime_binding: F,
) -> Option<HashSet<String>>
where
    F: Fn(&str) -> bool,
{
    let names = searchable_tool_names(surface)?;
    Some(runtime_bound_tool_names(names, has_runtime_binding))
}

/// Record names selected by `tool_search(select:...)` or by the direct
/// deferred-call recovery path.
///
/// Activation is a materialization record in the session's retained context.
/// It remains active after the corresponding full schema reaches `tools[]` and
/// after the model calls the tool, so schema visibility and admission cannot
/// drift across later turns. Session reset, context restoration, an installed
/// non-empty surface, or a missing runtime binding can prove an entry stale.
pub fn refresh_activated_tool_names<I>(activated: &mut HashSet<String>, names: I)
where
    I: IntoIterator<Item = String>,
{
    for name in names {
        activated.insert(name);
    }
}

/// Keep pending activated tools that still make sense for the current runtime.
///
/// `Uninstalled` fails closed. An installed no-tool surface preserves
/// runtime-bound activations because no full schema was advertised. A non-empty
/// surface prunes activations that are neither visible nor activatable.
#[must_use]
pub fn retained_runtime_bound_activated_tool_names<F>(
    activated: &HashSet<String>,
    surface: &ToolSurfaceNames,
    has_runtime_binding: F,
) -> Vec<String>
where
    F: Fn(&str) -> bool,
{
    let Some(searchable) = searchable_runtime_bound_tool_names(surface, &has_runtime_binding)
    else {
        return Vec::new();
    };
    let surface_has_names = surface.has_any_tool();
    let mut names: Vec<String> = activated
        .iter()
        .filter(|name| has_runtime_binding(name))
        .filter(|name| !surface_has_names || searchable.contains(*name))
        .cloned()
        .collect();
    names.sort();
    names
}

/// Return activated tool names for schema injection and prune stale entries.
///
/// This does not consume activation. A selected deferred tool remains visible
/// while its selection evidence remains in retained conversation context,
/// including after repeated calls. This matches the user-facing contract: a
/// schema returned by `tool_search(select:a,b,c)` is callable like a regular
/// visible schema until context/session reset or the surface becomes invalid.
pub fn activated_tool_names_for_schema_injection<F>(
    activated: &mut HashSet<String>,
    surface: &ToolSurfaceNames,
    has_runtime_binding: F,
) -> Vec<String>
where
    F: Fn(&str) -> bool,
{
    let retained =
        retained_runtime_bound_activated_tool_names(activated, surface, has_runtime_binding);
    if matches!(surface, ToolSurfaceNames::Uninstalled) {
        return retained;
    }
    let retained_set: HashSet<&str> = retained.iter().map(String::as_str).collect();
    activated.retain(|name| retained_set.contains(name.as_str()));
    retained
}

/// Extract activation names from a `tool_search(select:...)` result and keep
/// only names that this turn's deferred manifest advertised.
#[must_use]
pub fn recordable_activated_tool_names<F>(
    output: &str,
    surface: &ToolSurfaceNames,
    has_runtime_binding: F,
) -> Vec<String>
where
    F: Fn(&str) -> bool,
{
    let Some(activatable) = surface.activatable() else {
        return Vec::new();
    };
    activated_tool_names_from_tool_search_output(output)
        .into_iter()
        .filter(|name| activatable.contains(name) && has_runtime_binding(name))
        .collect()
}

/// Extract names activated by a `tool_search(select:...)` JSON result.
///
/// Keyword search results intentionally do not activate tools; they are only
/// discovery. Select-mode results return full schemas and are the activation
/// boundary.
#[must_use]
pub fn activated_tool_names_from_tool_search_output(output: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<Value>(output) else {
        return Vec::new();
    };
    if value.get("mode").and_then(Value::as_str) != Some("select") {
        return Vec::new();
    }
    let Some(query) = value.get("query").and_then(Value::as_str) else {
        return Vec::new();
    };
    let requested = requested_tool_names_from_select_query(query);
    if requested.is_empty() {
        return Vec::new();
    }
    let Some(output_requested) = requested_tool_names_from_output(&value) else {
        return Vec::new();
    };
    if !requested_tool_names_match(&requested, &output_requested) {
        return Vec::new();
    }

    let activation_candidates =
        resolved_tool_names_from_output(&value).unwrap_or_else(|| output_requested.clone());

    let mut names = Vec::new();
    let Some(matches) = value.get("matches").and_then(Value::as_array) else {
        return names;
    };
    for entry in matches {
        let Some(name) = entry
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
        else {
            continue;
        };
        if !activation_candidates
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(name))
        {
            continue;
        }
        if !names
            .iter()
            .any(|existing: &String| existing.eq_ignore_ascii_case(name))
        {
            names.push(name.to_string());
        }
    }
    names
}

/// Reconstruct deferred-tool activation from canonical conversation history.
///
/// Activation is accepted only from a tool result paired by `tool_call_id`
/// with an assistant `tool_search` call. This deliberately ignores user or
/// assistant prose and unpaired lookalike JSON. It lets process restarts and
/// legacy session projections recover the same prompt fact without treating
/// transient executor memory as the source of truth.
#[must_use]
pub fn activated_tool_names_from_messages(messages: &[Value]) -> Vec<String> {
    let mut pending_tool_search_call_ids = HashSet::new();
    let mut activated = BTreeSet::new();
    for message in messages {
        match message.get("role").and_then(Value::as_str) {
            Some("assistant") => {
                pending_tool_search_call_ids.extend(
                    message
                        .get("tool_calls")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter(|call| {
                            call.get("function")
                                .and_then(|function| function.get("name"))
                                .and_then(Value::as_str)
                                .is_some_and(|name| name.eq_ignore_ascii_case("tool_search"))
                        })
                        .filter_map(|call| call.get("id").and_then(Value::as_str))
                        .map(str::trim)
                        .filter(|id| !id.is_empty())
                        .map(ToOwned::to_owned),
                );
                continue;
            }
            Some("tool") => {}
            _ => continue,
        }
        let paired_tool_search = message
            .get("tool_call_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .is_some_and(|id| pending_tool_search_call_ids.remove(id));
        if !paired_tool_search {
            continue;
        }
        let Some(content) = message.get("content") else {
            continue;
        };
        let serialized;
        let output = if let Some(content) = content.as_str() {
            content
        } else {
            serialized = match serde_json::to_string(content) {
                Ok(serialized) => serialized,
                Err(_) => continue,
            };
            serialized.as_str()
        };
        activated.extend(activated_tool_names_from_tool_search_output(output));
    }
    activated.into_iter().collect()
}

/// Merge an explicit session snapshot with activation reconstructed from the
/// same retained message projection. The explicit snapshot survives
/// compaction; history reconstruction upgrades older snapshots and remote
/// projections that did not yet persist this field.
#[must_use]
pub fn merged_activated_tool_names(
    messages: &[Value],
    persisted_names: impl IntoIterator<Item = String>,
) -> Vec<String> {
    let mut names: BTreeSet<String> = persisted_names
        .into_iter()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .collect();
    names.extend(activated_tool_names_from_messages(messages));
    names.into_iter().collect()
}

fn requested_tool_names_from_select_query(query: &str) -> Vec<String> {
    let query = query.trim_start();
    const SELECT_PREFIX: &str = "select:";
    if !query
        .get(..SELECT_PREFIX.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(SELECT_PREFIX))
    {
        return Vec::new();
    }

    let mut names = Vec::new();
    for name in query[SELECT_PREFIX.len()..]
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        if !names
            .iter()
            .any(|existing: &String| existing.eq_ignore_ascii_case(name))
        {
            names.push(name.to_string());
        }
    }
    names
}

fn requested_tool_names_from_output(value: &Value) -> Option<Vec<String>> {
    let mut names = Vec::new();
    for name in value.get("requested")?.as_array()? {
        let name = name.as_str()?.trim();
        if name.is_empty() {
            return None;
        }
        if !names
            .iter()
            .any(|existing: &String| existing.eq_ignore_ascii_case(name))
        {
            names.push(name.to_string());
        }
    }
    Some(names)
}

fn resolved_tool_names_from_output(value: &Value) -> Option<Vec<String>> {
    let mut names = Vec::new();
    for name in value.get("resolved")?.as_array()? {
        let name = name.as_str()?.trim();
        if name.is_empty() {
            return None;
        }
        if !names
            .iter()
            .any(|existing: &String| existing.eq_ignore_ascii_case(name))
        {
            names.push(name.to_string());
        }
    }
    Some(names)
}

fn requested_tool_names_match(left: &[String], right: &[String]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| left.eq_ignore_ascii_case(right))
}

#[must_use]
pub fn tool_not_admitted_message(name: &str, deferred_select_allowed: bool) -> String {
    if deferred_select_allowed {
        format!(
            "Error: Tool '{name}' is not available in this turn yet. It appears \
             in `<deferred-tools>`, so first call `tool_search` with \
             `query=\"select:{name}\"` to activate its full schema for the \
             next model request. Call `{name}` only after it appears in \
             `tools[]`, using the schema's exact fields."
        )
    } else {
        format!(
            "Error: Tool '{name}' is not available in this turn. Call only tools \
             visible in this turn's `tools[]`. If you need a deferred tool, it \
             must appear in this turn's `<deferred-tools>` before you can select \
             it with `tool_search`. If the tool is hidden by interaction mode or \
             policy, use a visible tool or ask in the normal response."
        )
    }
}

#[must_use]
pub fn deferred_tool_not_activatable_message(name: &str) -> String {
    format!(
        "Error: Tool '{name}' is listed in this turn's `<deferred-tools>`, \
         but it is not activatable in the current runtime surface. Do not \
         retry `{name}` or `tool_search(query=\"select:{name}\")` in this \
         turn; use visible tools or explain that the deferred capability is \
         currently unavailable."
    )
}

/// Outcome of a direct call to a tool name that is not in the visible
/// `tools[]` surface but may be advertised in `<deferred-tools>`.
///
/// First-principle: a direct call to a deferred tool is an *activation intent*,
/// not an executable request. The model has not seen the tool's full schema, so
/// the supplied arguments cannot be trusted. When the name is advertised in
/// `<deferred-tools>` and the runtime can execute it, we record the activation
/// and ask the model to retry on the next model request once the schema is
/// visible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirectDeferredCallAdmission {
    /// The name is in the deferred manifest AND has a runtime binding. Treat
    /// the direct call as an activation intent: record the name so the next
    /// model request's `tools[]` includes the full schema, then ask the model
    /// to retry with the schema's exact fields. Do NOT execute the supplied
    /// args.
    Activate { name: String },
    /// The name is in the deferred manifest but has no runtime binding on this
    /// runtime. Cannot activate; the not-admitted hint lets the model
    /// self-correct (the tool genuinely cannot run here).
    NotAdmitted,
    /// The name is not in the deferred manifest at all. It is either
    /// hallucinated or hidden by policy — return the unknown-tool body.
    Unknown,
}

/// Classify a direct call to `name` given whether it is advertised in this
/// turn's `<deferred-tools>` and whether the current runtime can execute it.
#[must_use]
pub fn classify_direct_deferred_call<F>(
    name: &str,
    is_deferred: bool,
    has_runtime_binding: F,
) -> DirectDeferredCallAdmission
where
    F: Fn(&str) -> bool,
{
    if !is_deferred {
        return DirectDeferredCallAdmission::Unknown;
    }
    if has_runtime_binding(name) {
        DirectDeferredCallAdmission::Activate {
            name: name.to_string(),
        }
    } else {
        DirectDeferredCallAdmission::NotAdmitted
    }
}

/// Message returned when a direct deferred call is treated as an activation
/// intent. The message is deliberately non-immediate: the failed direct call
/// must not become a same-batch retry loop while the schema is still absent.
#[must_use]
pub fn direct_deferred_call_activation_message(name: &str) -> String {
    format!(
        "Tool '{name}' is deferred and is not currently present in `tools[]`. \
         If `tool_search` is available, call `tool_search(query=\"select:{name}\")` \
         once to request activation. The direct call was not executed and its \
         arguments were ignored. Do not call `{name}` again in the same \
         tool-call batch; only invoke it after a later model request shows \
         `{name}` in `tools[]`."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn searchable_names_fail_closed_without_installed_surface() {
        assert!(
            searchable_tool_names(&ToolSurfaceNames::Uninstalled).is_none(),
            "missing per-turn surface must not fall back to a global catalog"
        );
    }

    #[test]
    fn searchable_names_are_visible_union_activatable() {
        let visible = HashSet::from(["bash".to_string(), "tool_search".to_string()]);
        let activatable = HashSet::from(["web_fetch".to_string()]);
        let surface = ToolSurfaceNames::installed(visible, activatable);

        let names = searchable_tool_names(&surface)
            .expect("installed surface should produce a search pool");

        assert_eq!(
            names,
            HashSet::from([
                "bash".to_string(),
                "tool_search".to_string(),
                "web_fetch".to_string()
            ])
        );
    }

    #[test]
    fn searchable_runtime_bound_names_filter_visible_and_activatable() {
        let visible = HashSet::from([
            "bash".to_string(),
            "mcp__stale_visible".to_string(),
            "tool_search".to_string(),
        ]);
        let activatable =
            HashSet::from(["web_fetch".to_string(), "mcp__stale_deferred".to_string()]);
        let surface = ToolSurfaceNames::installed(visible, activatable);

        let names =
            searchable_runtime_bound_tool_names(&surface, |name| !name.starts_with("mcp__stale"))
                .expect("installed surface should produce a search pool");

        assert_eq!(
            names,
            HashSet::from([
                "bash".to_string(),
                "tool_search".to_string(),
                "web_fetch".to_string()
            ])
        );
    }

    #[test]
    fn activation_survives_explicit_no_tool_surface_without_consuming() {
        let mut activated = HashSet::new();
        refresh_activated_tool_names(&mut activated, ["memory".to_string()]);
        let visible = HashSet::new();
        let activatable = HashSet::new();
        let surface = ToolSurfaceNames::installed(visible, activatable);

        assert_eq!(
            retained_runtime_bound_activated_tool_names(&activated, &surface, |_| true),
            vec!["memory".to_string()],
            "explicit no-tool surfaces must not invalidate previous activations"
        );
        assert_eq!(
            activated_tool_names_for_schema_injection(&mut activated, &surface, |_| true),
            vec!["memory".to_string()]
        );
        assert!(activated.contains("memory"));
    }

    #[test]
    fn activation_no_tool_surface_prunes_runtime_unbound_orphans() {
        let mut activated = HashSet::new();
        refresh_activated_tool_names(
            &mut activated,
            ["memory".to_string(), "mcp__stale".to_string()],
        );
        let surface = ToolSurfaceNames::installed(HashSet::new(), HashSet::new());

        assert_eq!(
            activated_tool_names_for_schema_injection(&mut activated, &surface, |name| {
                name != "mcp__stale"
            }),
            vec!["memory".to_string()],
            "explicit no-tool surfaces preserve valid activation but still drop unbound orphans"
        );
        assert_eq!(activated, HashSet::from(["memory".to_string()]));
    }

    #[test]
    fn activation_repeated_schema_injection_does_not_expire_without_tool_call() {
        let mut activated = HashSet::new();
        refresh_activated_tool_names(&mut activated, ["memory".to_string()]);
        let surface =
            ToolSurfaceNames::installed(HashSet::from(["memory".to_string()]), HashSet::new());

        assert_eq!(
            activated_tool_names_for_schema_injection(&mut activated, &surface, |_| true),
            vec!["memory".to_string()]
        );
        assert!(activated.contains("memory"));
        assert_eq!(
            activated_tool_names_for_schema_injection(&mut activated, &surface, |_| true),
            vec!["memory".to_string()]
        );
        assert!(activated.contains("memory"));
        assert!(activated.contains("memory"));
    }

    #[test]
    fn activation_remains_stable_after_repeated_calls() {
        let mut activated = HashSet::new();
        refresh_activated_tool_names(
            &mut activated,
            [
                "bash".to_string(),
                "grep".to_string(),
                "glob".to_string(),
                "read_file".to_string(),
            ],
        );
        let surface = ToolSurfaceNames::installed(
            HashSet::from([
                "bash".to_string(),
                "grep".to_string(),
                "glob".to_string(),
                "read_file".to_string(),
            ]),
            HashSet::new(),
        );

        assert_eq!(
            activated_tool_names_for_schema_injection(&mut activated, &surface, |_| true),
            vec![
                "bash".to_string(),
                "glob".to_string(),
                "grep".to_string(),
                "read_file".to_string()
            ]
        );
        assert_eq!(
            activated_tool_names_for_schema_injection(&mut activated, &surface, |_| true),
            vec![
                "bash".to_string(),
                "glob".to_string(),
                "grep".to_string(),
                "read_file".to_string()
            ],
            "calls must not revoke schemas that remain valid for this agentic run"
        );
    }

    #[test]
    fn activation_prunes_stale_entries_on_non_empty_surface() {
        let mut activated = HashSet::new();
        refresh_activated_tool_names(&mut activated, ["memory".to_string(), "stale".to_string()]);
        let visible = HashSet::from(["tool_search".to_string()]);
        let activatable = HashSet::from(["memory".to_string()]);
        let surface = ToolSurfaceNames::installed(visible, activatable);

        assert_eq!(
            activated_tool_names_for_schema_injection(&mut activated, &surface, |_| true),
            vec!["memory".to_string()]
        );
        assert!(
            activated.contains("memory") && !activated.contains("stale"),
            "non-empty surfaces should prune activations outside visible ∪ activatable"
        );
    }

    #[test]
    fn recordable_activation_names_require_current_deferred_manifest() {
        let out = json!({
            "mode": "select",
            "query": "select:web_fetch,bash",
            "requested": ["web_fetch", "bash"],
            "matches": [
                {"name": "web_fetch", "parameters": {"type": "object"}},
                {"name": "bash", "parameters": {"type": "object"}}
            ],
            "missing": []
        })
        .to_string();
        let surface = ToolSurfaceNames::installed(
            HashSet::from(["bash".to_string()]),
            HashSet::from(["web_fetch".to_string()]),
        );

        assert_eq!(
            recordable_activated_tool_names(&out, &surface, |_| true),
            vec!["web_fetch".to_string()],
            "visible/non-deferred select results must not create deferred activation"
        );
        assert!(
            recordable_activated_tool_names(&out, &ToolSurfaceNames::Uninstalled, |_| true)
                .is_empty(),
            "missing manifest must fail closed"
        );
        assert!(
            recordable_activated_tool_names(&out, &surface, |_| false).is_empty(),
            "callers can fail closed for names without runtime binding"
        );
    }

    #[test]
    fn select_result_activates_matched_names() {
        let out = json!({
            "mode": "select",
            "query": "select:agent_fanout",
            "requested": ["agent_fanout"],
            "matches": [
                {"name": "agent_fanout", "description": "full", "parameters": {"type": "object"}}
            ],
            "missing": []
        })
        .to_string();
        assert_eq!(
            activated_tool_names_from_tool_search_output(&out),
            vec!["agent_fanout".to_string()]
        );
    }

    #[test]
    fn canonical_history_reconstructs_only_paired_tool_search_activation() {
        let selected = json!({
            "mode": "select",
            "query": "select:github,web_fetch",
            "requested": ["github", "web_fetch"],
            "matches": [
                {"name": "web_fetch", "parameters": {"type": "object"}},
                {"name": "github", "parameters": {"type": "object"}}
            ],
            "missing": []
        })
        .to_string();
        let messages = vec![
            json!({"role": "user", "content": selected}),
            json!({
                "role": "assistant",
                "tool_calls": [{
                    "id": "search-1",
                    "function": {"name": "tool_search", "arguments": "{}"}
                }]
            }),
            json!({"role": "tool", "tool_call_id": "search-1", "content": selected}),
            json!({
                "role": "assistant",
                "tool_calls": [{
                    "id": "other-1",
                    "function": {"name": "read_file", "arguments": "{}"}
                }]
            }),
            json!({
                "role": "tool",
                "tool_call_id": "other-1",
                "content": {
                    "mode": "select",
                    "query": "select:lookalike",
                    "requested": ["lookalike"],
                    "matches": [{"name": "lookalike"}],
                    "missing": []
                }
            }),
        ];

        assert_eq!(
            activated_tool_names_from_messages(&messages),
            vec!["github".to_string(), "web_fetch".to_string()]
        );
    }

    #[test]
    fn canonical_history_ignores_unpaired_or_non_selection_search_results() {
        let selected = json!({
            "mode": "select",
            "query": "select:github",
            "requested": ["github"],
            "matches": [{"name": "github"}],
            "missing": []
        })
        .to_string();
        let non_selection = json!({
            "mode": "error",
            "status": "failed",
            "query": "github",
            "matches": [{"name": "github"}]
        })
        .to_string();
        let messages = vec![
            json!({"role": "tool", "tool_call_id": "search-1", "content": selected}),
            json!({
                "role": "assistant",
                "tool_calls": [{
                    "id": "search-1",
                    "function": {"name": "tool_search", "arguments": "{}"}
                }]
            }),
            json!({"role": "tool", "tool_call_id": "search-1", "content": non_selection}),
            json!({
                "role": "tool",
                "tool_call_id": "missing-call",
                "content": {
                    "mode": "select",
                    "query": "select:github",
                    "requested": ["github"],
                    "matches": [{"name": "github"}],
                    "missing": []
                }
            }),
        ];

        assert!(activated_tool_names_from_messages(&messages).is_empty());
    }

    #[test]
    fn select_prefix_is_case_insensitive_for_activation() {
        let out = json!({
            "mode": "select",
            "query": " Select:GitHub",
            "requested": ["GitHub"],
            "matches": [
                {"name": "github", "description": "full", "parameters": {"type": "object"}}
            ],
            "missing": []
        })
        .to_string();
        assert_eq!(
            activated_tool_names_from_tool_search_output(&out),
            vec!["github".to_string()]
        );
    }

    #[test]
    fn select_result_activates_resolved_canonical_name() {
        let out = json!({
            "mode": "select",
            "query": "select:github",
            "requested": ["github"],
            "resolved": ["github"],
            "matches": [
                {
                    "name": "github",
                    "description": "full",
                    "matched_by": "exact",
                    "requested": "github",
                    "parameters": {"type": "object"}
                }
            ],
            "missing": []
        })
        .to_string();

        assert_eq!(
            activated_tool_names_from_tool_search_output(&out),
            vec!["github".to_string()]
        );
    }

    #[test]
    fn non_selection_result_does_not_activate_names() {
        let out = json!({
            "mode": "error",
            "status": "failed",
            "query": "agent",
            "matches": [{"name": "agent_fanout"}]
        })
        .to_string();
        assert!(activated_tool_names_from_tool_search_output(&out).is_empty());
    }

    #[test]
    fn select_without_mode_does_not_activate() {
        let out = json!({
            "query": "select:agent_fanout",
            "requested": ["agent_fanout"],
            "matches": [
                {"name": "agent_fanout", "description": "full", "parameters": {"type": "object"}}
            ],
            "missing": []
        })
        .to_string();
        assert!(activated_tool_names_from_tool_search_output(&out).is_empty());
    }

    #[test]
    fn select_result_with_mismatched_requested_list_does_not_activate() {
        let out = json!({
            "mode": "select",
            "query": "select:agent_fanout",
            "requested": ["github"],
            "matches": [
                {"name": "agent_fanout", "description": "full", "parameters": {"type": "object"}}
            ],
            "missing": []
        })
        .to_string();
        assert!(activated_tool_names_from_tool_search_output(&out).is_empty());
    }

    #[test]
    fn select_result_ignores_matches_that_were_not_requested() {
        let out = json!({
            "mode": "select",
            "query": "select:agent_fanout",
            "requested": ["agent_fanout"],
            "matches": [
                {"name": "agent_fanout", "description": "full", "parameters": {"type": "object"}},
                {"name": "github", "description": "polluted", "parameters": {"type": "object"}}
            ],
            "missing": []
        })
        .to_string();
        assert_eq!(
            activated_tool_names_from_tool_search_output(&out),
            vec!["agent_fanout".to_string()]
        );
    }

    #[test]
    fn direct_deferred_call_with_runtime_binding_activates() {
        assert_eq!(
            classify_direct_deferred_call("run_script", true, |n| n == "run_script"),
            DirectDeferredCallAdmission::Activate {
                name: "run_script".to_string()
            },
            "a deferred tool with a runtime binding must be treated as an activation intent"
        );
    }

    #[test]
    fn direct_deferred_call_without_runtime_binding_is_not_admitted() {
        assert_eq!(
            classify_direct_deferred_call("run_script", true, |_| false),
            DirectDeferredCallAdmission::NotAdmitted,
            "a deferred tool without a runtime binding cannot be activated"
        );
    }

    #[test]
    fn direct_call_to_name_not_in_deferred_manifest_is_unknown() {
        assert_eq!(
            classify_direct_deferred_call("hallucinated_tool", false, |_| true),
            DirectDeferredCallAdmission::Unknown,
            "a name not advertised in <deferred-tools> must not be activated even if a binding exists"
        );
    }

    #[test]
    fn direct_deferred_call_activation_message_names_the_tool_and_forbids_execution() {
        let msg = direct_deferred_call_activation_message("run_script");
        assert!(msg.contains("run_script"), "message must name the tool");
        assert!(
            msg.contains("select:run_script"),
            "message must frame the call as a select intent"
        );
        assert!(
            msg.contains("not executed"),
            "message must state the args were not executed"
        );
        assert!(
            msg.contains("arguments were ignored"),
            "message must state direct-call args are not reused"
        );
        assert!(
            msg.contains("Do not call `run_script` again in the same tool-call batch"),
            "message must prevent same-batch retry loops"
        );
        assert!(
            msg.contains("later model request"),
            "message must require a later request with the full schema"
        );
    }

    #[test]
    fn deferred_tool_not_activatable_message_avoids_search_retry_loop() {
        let msg = deferred_tool_not_activatable_message("github");

        assert!(msg.contains("<deferred-tools>"), "{msg}");
        assert!(msg.contains("not activatable"), "{msg}");
        assert!(msg.contains("Do not retry"), "{msg}");
        assert!(
            msg.contains("tool_search(query=\"select:github\")"),
            "{msg}"
        );
    }

    #[test]
    fn duplicate_select_matches_activate_once() {
        let out = json!({
            "mode": "select",
            "query": "select:Agent_Fanout,agent_fanout",
            "requested": ["Agent_Fanout"],
            "matches": [
                {"name": "agent_fanout", "description": "full", "parameters": {"type": "object"}},
                {"name": "agent_fanout", "description": "full", "parameters": {"type": "object"}}
            ],
            "missing": []
        })
        .to_string();
        assert_eq!(
            activated_tool_names_from_tool_search_output(&out),
            vec!["agent_fanout".to_string()]
        );
    }
}
