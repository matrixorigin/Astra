use std::collections::HashSet;

use astra_text_utils::xml_escape::xml_escape_text;
use serde_json::{Map, Value};

use crate::tool_registry::surface::DeferredManifest;

const LOG_TARGET: &str = "astra.deferred_tools";

fn names(edge_profile: &Map<String, Value>) -> HashSet<String> {
    edge_profile
        .get(astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_DEFERRED_TOOL_NAMES)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect()
}

pub(crate) fn block_for_model(
    edge_profile: &Map<String, Value>,
    resolved_model_name: &str,
) -> Option<String> {
    manifest_for_model(edge_profile, resolved_model_name).map(|manifest| manifest.text)
}

pub(crate) fn block_for_model_filtered(
    edge_profile: &Map<String, Value>,
    resolved_model_name: &str,
    allowed_names: &HashSet<String>,
) -> Option<String> {
    if allowed_names.is_empty() {
        return None;
    }
    let manifest = manifest_for_model(edge_profile, resolved_model_name)?;
    let manifest_names: HashSet<String> = manifest.names.iter().cloned().collect();
    let retained_names: HashSet<String> = manifest_names
        .intersection(allowed_names)
        .cloned()
        .collect();
    if retained_names.is_empty() {
        tracing::warn!(
            target: LOG_TARGET,
            model = resolved_model_name,
            manifest_count = manifest_names.len(),
            allowed_count = allowed_names.len(),
            "deferred tool manifest filtered to zero names for the current runtime surface"
        );
        return None;
    }
    if retained_names == manifest_names {
        return Some(manifest.text);
    }
    filter_block_to_names(&manifest.text, &retained_names)
}

pub(crate) fn names_for_model(
    edge_profile: &Map<String, Value>,
    resolved_model_name: Option<&str>,
) -> HashSet<String> {
    resolved_model_name
        .and_then(|model| manifest_for_model(edge_profile, model))
        .map(|manifest| manifest.names.into_iter().collect())
        .unwrap_or_default()
}

fn manifest_for_model(
    edge_profile: &Map<String, Value>,
    resolved_model_name: &str,
) -> Option<DeferredManifest> {
    let declared_names = names(edge_profile);
    if declared_names.is_empty() {
        if edge_profile.contains_key(
            astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_DEFERRED_TOOLS_TEXT,
        ) || edge_profile.contains_key(
            astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_DEFERRED_TOOLS_CONTEXT_WINDOW,
        ) {
            tracing::warn!(
                target: LOG_TARGET,
                model = resolved_model_name,
                "deferred tool manifest omitted because declared name metadata is empty"
            );
        }
        return None;
    }
    let Some(source_budget) = edge_profile
        .get(
            astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_DEFERRED_TOOLS_CONTEXT_WINDOW,
        )
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
    else {
        tracing::warn!(
            target: LOG_TARGET,
            model = resolved_model_name,
            declared_count = declared_names.len(),
            "deferred tool manifest omitted because context-window metadata is missing or invalid"
        );
        return None;
    };
    let resolved_budget = crate::prompts::budget_for_model(Some(resolved_model_name)).model_limit;
    let effective_budget = if source_budget != resolved_budget {
        tracing::warn!(
            target: LOG_TARGET,
            model = resolved_model_name,
            source_budget,
            resolved_budget,
            declared_count = declared_names.len(),
            "deferred tool manifest budget mismatch — using min({source_budget}, {resolved_budget})"
        );
        source_budget.min(resolved_budget)
    } else {
        source_budget
    };
    let Some(block) = edge_profile
        .get(astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_DEFERRED_TOOLS_TEXT)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
    else {
        tracing::warn!(
            target: LOG_TARGET,
            model = resolved_model_name,
            declared_count = declared_names.len(),
            "deferred tool manifest omitted because rendered text is empty"
        );
        return None;
    };
    let rendered_name_keys = rendered_name_keys_from_block(&block);
    if rendered_name_keys.is_empty() {
        tracing::warn!(
            target: LOG_TARGET,
            model = resolved_model_name,
            declared_count = declared_names.len(),
            "deferred tool manifest omitted because rendered text contains no <name> entries"
        );
        return None;
    }
    let declared_name_keys: HashSet<String> = declared_names
        .iter()
        .map(|name| xml_escape_text(name).into_owned())
        .collect();
    if declared_name_keys != rendered_name_keys {
        let missing_from_rendered: Vec<&str> = declared_name_keys
            .difference(&rendered_name_keys)
            .map(String::as_str)
            .collect();
        let missing_from_metadata: Vec<&str> = rendered_name_keys
            .difference(&declared_name_keys)
            .map(String::as_str)
            .collect();
        tracing::warn!(
            target: LOG_TARGET,
            model = resolved_model_name,
            declared_count = declared_name_keys.len(),
            rendered_count = rendered_name_keys.len(),
            ?missing_from_rendered,
            ?missing_from_metadata,
            "deferred tool manifest omitted because declared names and rendered names diverge"
        );
        return None;
    }
    let mut names: Vec<String> = declared_names.into_iter().collect();
    names.sort();
    Some(DeferredManifest {
        text: block,
        context_window: effective_budget,
        names,
    })
}

fn rendered_name_keys_from_block(block: &str) -> HashSet<String> {
    const OPEN: &str = "<name>";
    const CLOSE: &str = "</name>";

    let mut names = HashSet::new();
    let mut rest = block;
    while let Some(open_idx) = rest.find(OPEN) {
        let after_open = &rest[open_idx + OPEN.len()..];
        let Some(close_idx) = after_open.find(CLOSE) else {
            break;
        };
        let name = after_open[..close_idx].trim();
        if !name.is_empty() {
            names.insert(name.to_string());
        }
        rest = &after_open[close_idx + CLOSE.len()..];
    }
    names
}

fn filter_block_to_names(block: &str, names: &HashSet<String>) -> Option<String> {
    const OPEN_TOOL: &str = "<tool>";
    const CLOSE_TOOL: &str = "</tool>";

    let allowed_name_keys: HashSet<String> = names
        .iter()
        .map(|name| xml_escape_text(name).into_owned())
        .collect();
    let mut rendered = String::with_capacity(block.len());
    let mut rest = block;
    let mut kept = 0usize;

    while let Some(open_idx) = rest.find(OPEN_TOOL) {
        rendered.push_str(&rest[..open_idx]);
        let from_tool = &rest[open_idx..];
        let close_idx = from_tool.find(CLOSE_TOOL)?;
        let close_end = close_idx + CLOSE_TOOL.len();
        let tool_block = &from_tool[..close_end];
        let block_names = rendered_name_keys_from_block(tool_block);
        if block_names
            .iter()
            .any(|name| allowed_name_keys.contains(name))
        {
            rendered.push_str(tool_block);
            kept += 1;
        }
        rest = &from_tool[close_end..];
    }
    rendered.push_str(rest);

    if kept == 0 { None } else { Some(rendered) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn deferred_profile(
        declared_names: &[&str],
        rendered_names: &[&str],
        model: &str,
    ) -> Map<String, Value> {
        let mut edge_profile = Map::new();
        let tools_xml = rendered_names
            .iter()
            .map(|name| {
                format!(
                    "  <tool>\n    <name>{}</name>\n    <description>{} tool</description>\n  </tool>\n",
                    name.trim(),
                    name.trim()
                )
            })
            .collect::<String>();
        edge_profile.insert(
            astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_DEFERRED_TOOLS_TEXT
                .to_string(),
            Value::String(format!("<deferred_tools>\n{tools_xml}</deferred_tools>")),
        );
        edge_profile.insert(
            astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_DEFERRED_TOOLS_CONTEXT_WINDOW
                .to_string(),
            Value::Number(
                crate::prompts::budget_for_model(Some(model))
                    .model_limit
                    .into(),
            ),
        );
        edge_profile.insert(
            astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_DEFERRED_TOOL_NAMES
                .to_string(),
            json!(declared_names),
        );
        edge_profile
    }

    #[test]
    fn names_and_block_for_model_accept_cli_rendered_manifest() {
        let edge_profile = deferred_profile(&["agent_fanout", " "], &["agent_fanout"], "gpt-4o");

        let names = names_for_model(&edge_profile, Some("gpt-4o"));

        assert_eq!(names, HashSet::from(["agent_fanout".to_string()]));
        let block = block_for_model(&edge_profile, "gpt-4o")
            .expect("consistent deferred manifest should render");
        assert!(block.contains("<name>agent_fanout</name>"));
    }

    #[test]
    fn block_for_model_filtered_keeps_only_allowed_manifest_entries() {
        let edge_profile = deferred_profile(
            &["github", "agent_fanout"],
            &["github", "agent_fanout"],
            "gpt-4o",
        );

        let block = block_for_model_filtered(
            &edge_profile,
            "gpt-4o",
            &HashSet::from(["github".to_string()]),
        )
        .expect("filtered manifest should keep allowed names");

        assert!(block.contains("<name>github</name>"));
        assert!(!block.contains("<name>agent_fanout</name>"));
    }

    #[test]
    fn names_and_block_for_model_reject_declared_names_not_shown_to_model() {
        let edge_profile =
            deferred_profile(&["agent_fanout", "web_fetch"], &["agent_fanout"], "gpt-4o");

        assert!(
            names_for_model(&edge_profile, Some("gpt-4o")).is_empty(),
            "activation/search names must not include tools absent from the prompt manifest"
        );
        assert!(
            block_for_model(&edge_profile, "gpt-4o").is_none(),
            "prompt rendering must fail closed when metadata and rendered names diverge"
        );
    }

    #[test]
    fn names_and_block_for_model_reject_rendered_names_missing_from_metadata() {
        let edge_profile =
            deferred_profile(&["agent_fanout"], &["agent_fanout", "web_fetch"], "gpt-4o");

        assert!(
            names_for_model(&edge_profile, Some("gpt-4o")).is_empty(),
            "tool_search must not expose only a subset of the names shown in the prompt"
        );
        assert!(
            block_for_model(&edge_profile, "gpt-4o").is_none(),
            "the model must not see deferred tools that activation/search will not honor"
        );
    }
}
