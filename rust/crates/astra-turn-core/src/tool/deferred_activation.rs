//! Helpers for the deferred-tool activation contract.
//!
//! Deferred tool entries are discovery metadata. A tool becomes executable
//! only when it is already visible in `tools[]` or the model has fetched its
//! full schema with `tool_search(query="select:NAME")`.

use std::collections::HashSet;

use serde_json::Value;

/// Build the per-turn tool-search pool from the tools that are already
/// visible plus the deferred tools advertised in this turn's prompt.
///
/// `None` means no caller-installed surface exists yet; callers should fail
/// closed instead of falling back to a global catalog.
#[must_use]
pub fn searchable_tool_names(
    visible: Option<&HashSet<String>>,
    activatable: Option<&HashSet<String>>,
) -> Option<HashSet<String>> {
    match (visible, activatable) {
        (None, None) => None,
        (visible, activatable) => {
            let mut names = HashSet::new();
            if let Some(visible) = visible {
                names.extend(visible.iter().cloned());
            }
            if let Some(activatable) = activatable {
                names.extend(activatable.iter().cloned());
            }
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
    visible: Option<&HashSet<String>>,
    activatable: Option<&HashSet<String>>,
    has_runtime_binding: F,
) -> Option<HashSet<String>>
where
    F: Fn(&str) -> bool,
{
    let names = searchable_tool_names(visible, activatable)?;
    Some(runtime_bound_tool_names(names, has_runtime_binding))
}

/// Keep only previously activated deferred tools that are still present in the
/// current surface (`visible ∪ activatable`).
#[must_use]
pub fn retained_activated_deferred_tool_names(
    activated: &HashSet<String>,
    visible: Option<&HashSet<String>>,
    activatable: Option<&HashSet<String>>,
) -> Vec<String> {
    let Some(searchable) = searchable_tool_names(visible, activatable) else {
        return Vec::new();
    };
    let mut names: Vec<String> = activated
        .iter()
        .filter(|name| searchable.contains(*name))
        .cloned()
        .collect();
    names.sort();
    names
}

/// Keep only previously activated deferred tools that are still present in the
/// current surface and still executable by this runtime.
#[must_use]
pub fn retained_runtime_bound_activated_deferred_tool_names<F>(
    activated: &HashSet<String>,
    visible: Option<&HashSet<String>>,
    activatable: Option<&HashSet<String>>,
    has_runtime_binding: F,
) -> Vec<String>
where
    F: Fn(&str) -> bool,
{
    let Some(searchable) =
        searchable_runtime_bound_tool_names(visible, activatable, has_runtime_binding)
    else {
        return Vec::new();
    };
    let mut names: Vec<String> = activated
        .iter()
        .filter(|name| searchable.contains(*name))
        .cloned()
        .collect();
    names.sort();
    names
}

/// Extract activation names from a `tool_search(select:...)` result and keep
/// only names that this turn's deferred manifest advertised.
#[must_use]
pub fn recordable_activated_tool_names<F>(
    output: &str,
    activatable: Option<&HashSet<String>>,
    has_runtime_binding: F,
) -> Vec<String>
where
    F: Fn(&str) -> bool,
{
    let Some(activatable) = activatable else {
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
        if !requested
            .iter()
            .any(|requested| requested.eq_ignore_ascii_case(name))
        {
            continue;
        }
        if !names.iter().any(|existing| existing == name) {
            names.push(name.to_string());
        }
    }
    names
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
             in `<deferred_tools>`, so first call `tool_search` with \
             `query=\"select:{name}\"` to fetch the full schema, then call \
             `{name}` with the schema's exact fields."
        )
    } else {
        format!(
            "Error: Tool '{name}' is not available in this turn. Call only tools \
             visible in this turn's `tools[]`. If you need a deferred tool, it \
             must appear in this turn's `<deferred_tools>` before you can select \
             it with `tool_search`. If the tool is hidden by interaction mode or \
             policy, use a visible tool or ask in the normal response."
        )
    }
}

/// Outcome of a direct call to a tool name that is not in the visible
/// `tools[]` surface but may be advertised in `<deferred_tools>`.
///
/// First-principle: a direct call to a deferred tool is an *activation intent*,
/// not an executable request. The model has not seen the tool's full schema, so
/// the supplied arguments cannot be trusted. When the name is advertised in
/// `<deferred_tools>` and the runtime can execute it, we record the activation
/// and ask the model to retry next turn once the schema is visible — mirroring
/// Claude Code's `defer_loading` / `tool_reference` contract locally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirectDeferredCallAdmission {
    /// The name is in the deferred manifest AND has a runtime binding. Treat
    /// the direct call as an activation intent: record the name so the next
    /// turn's `tools[]` includes the full schema, then ask the model to retry
    /// with the schema's exact fields. Do NOT execute the supplied args.
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
/// turn's `<deferred_tools>` and whether the current runtime can execute it.
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
/// intent. The tool is NOT executed because the model has not seen its full
/// schema and the supplied arguments cannot be trusted.
#[must_use]
pub fn direct_deferred_call_activated_message(name: &str) -> String {
    format!(
        "Tool '{name}' was called directly, but its full schema has not been \
         loaded yet. This call has been treated as a `select:{name}` intent — \
         the next turn will include the complete schema in `tools[]`. Do NOT \
         repeat this call with guessed arguments. On the next turn, call \
         `{name}` again using the schema's exact fields. The arguments from \
         this attempt were not executed because the schema was not visible."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn searchable_names_fail_closed_without_installed_surface() {
        assert!(
            searchable_tool_names(None, None).is_none(),
            "missing per-turn surface must not fall back to a global catalog"
        );
    }

    #[test]
    fn searchable_names_are_visible_union_activatable() {
        let visible = HashSet::from(["bash".to_string(), "tool_search".to_string()]);
        let activatable = HashSet::from(["web_fetch".to_string()]);

        let names = searchable_tool_names(Some(&visible), Some(&activatable))
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

        let names =
            searchable_runtime_bound_tool_names(Some(&visible), Some(&activatable), |name| {
                !name.starts_with("mcp__stale")
            })
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
    fn retained_activated_names_are_scoped_to_current_surface() {
        let activated = HashSet::from(["web_fetch".to_string(), "github".to_string()]);
        let visible = HashSet::from(["bash".to_string(), "web_fetch".to_string()]);
        let activatable = HashSet::from(["memory".to_string()]);

        assert_eq!(
            retained_activated_deferred_tool_names(&activated, Some(&visible), Some(&activatable)),
            vec!["web_fetch".to_string()]
        );
        assert!(
            retained_activated_deferred_tool_names(&activated, None, None).is_empty(),
            "without a current surface no deferred activation may remain visible"
        );
    }

    #[test]
    fn retained_activated_names_require_runtime_binding() {
        let activated = HashSet::from(["mcp__weather".to_string(), "github".to_string()]);
        let visible = HashSet::from(["mcp__weather".to_string(), "bash".to_string()]);
        let activatable = HashSet::from(["github".to_string()]);

        assert_eq!(
            retained_runtime_bound_activated_deferred_tool_names(
                &activated,
                Some(&visible),
                Some(&activatable),
                |name| name != "mcp__weather"
            ),
            vec!["github".to_string()],
            "stale visible schemas must not retain an activated tool after runtime binding disappears"
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
        let activatable = HashSet::from(["web_fetch".to_string()]);

        assert_eq!(
            recordable_activated_tool_names(&out, Some(&activatable), |_| true),
            vec!["web_fetch".to_string()],
            "visible/non-deferred select results must not create deferred activation"
        );
        assert!(
            recordable_activated_tool_names(&out, None, |_| true).is_empty(),
            "missing manifest must fail closed"
        );
        assert!(
            recordable_activated_tool_names(&out, Some(&activatable), |_| false).is_empty(),
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
    fn keyword_result_does_not_activate_names() {
        let out = json!({
            "mode": "keyword",
            "query": "agent",
            "matches": [{"name": "agent_fanout", "description": "short", "score": 0.8}]
        })
        .to_string();
        assert!(activated_tool_names_from_tool_search_output(&out).is_empty());
    }

    #[test]
    fn legacy_select_without_mode_does_not_activate() {
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
            "a name not advertised in <deferred_tools> must not be activated even if a binding exists"
        );
    }

    #[test]
    fn direct_deferred_call_activated_message_names_the_tool_and_forbids_retry() {
        let msg = direct_deferred_call_activated_message("run_script");
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
            msg.contains("next turn"),
            "message must point to the next turn for the full schema"
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
