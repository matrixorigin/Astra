use std::collections::HashSet;

use astra_text_utils::xml_escape::xml_escape_text;
use serde_json::{Map, Value};

use crate::tool_registry::surface::DeferredManifest;

const LOG_TARGET: &str = "astra.deferred_tools";

fn names_from_key(edge_profile: &Map<String, Value>, key: &str) -> HashSet<String> {
    edge_profile
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect()
}

fn names(edge_profile: &Map<String, Value>) -> HashSet<String> {
    names_from_key(
        edge_profile,
        astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_DEFERRED_TOOL_NAMES,
    )
}

fn omitted_names(edge_profile: &Map<String, Value>) -> HashSet<String> {
    names_from_key(
        edge_profile,
        astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_DEFERRED_TOOL_OMITTED_NAMES,
    )
}

pub(crate) fn block_for_model(
    edge_profile: &Map<String, Value>,
    resolved_model_name: &str,
) -> Option<String> {
    manifest_for_model(edge_profile, resolved_model_name).map(|manifest| manifest.text)
}

#[cfg(test)]
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
    let omitted_names = omitted_names(edge_profile);
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
            "deferred tool manifest omitted because rendered text contains no name lines"
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
    let mut omitted_names: Vec<String> = omitted_names.into_iter().collect();
    omitted_names.sort();
    Some(DeferredManifest {
        text: block,
        context_window: effective_budget,
        names,
        omitted_names,
    })
}

fn rendered_name_keys_from_block(block: &str) -> HashSet<String> {
    deferred_tools_body(block)
        .into_iter()
        .flat_map(str::lines)
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

fn deferred_tools_body(block: &str) -> Option<&str> {
    let (_, body, _) = split_deferred_tools_block(block)?;
    Some(body)
}

fn split_deferred_tools_block(block: &str) -> Option<(&str, &str, &str)> {
    const OPEN: &str = "<deferred-tools>";
    const CLOSE: &str = "</deferred-tools>";

    let open_idx = block.find(OPEN)?;
    let body_start = open_idx + OPEN.len();
    let close_rel = block[body_start..].find(CLOSE)?;
    let close_idx = body_start + close_rel;
    Some((
        &block[..body_start],
        &block[body_start..close_idx],
        &block[close_idx..],
    ))
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
        let tools_list = rendered_names
            .iter()
            .map(|name| name.trim())
            .collect::<Vec<_>>()
            .join("\n");
        edge_profile.insert(
            astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_DEFERRED_TOOLS_TEXT
                .to_string(),
            Value::String(format!("<deferred-tools>\n{tools_list}\n</deferred-tools>")),
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
        assert!(block.contains("\nagent_fanout\n"));
        assert!(!block.contains("<tool>"));
        assert!(!block.contains("<name>"));
        assert!(!block.contains("<description>"));
    }

    #[test]
    fn omitted_names_are_observable_but_not_activatable() {
        let mut edge_profile = deferred_profile(&["agent_fanout"], &["agent_fanout"], "gpt-4o");
        edge_profile.insert(
            astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_DEFERRED_TOOL_OMITTED_NAMES
                .to_string(),
            json!(["web_fetch"]),
        );

        let names = names_for_model(&edge_profile, Some("gpt-4o"));
        assert_eq!(names, HashSet::from(["agent_fanout".to_string()]));

        let manifest = manifest_for_model(&edge_profile, "gpt-4o")
            .expect("consistent manifest should keep omitted metadata");
        assert_eq!(manifest.omitted_names, vec!["web_fetch".to_string()]);
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

    #[test]
    fn names_and_block_for_model_reject_legacy_schema_like_xml_manifest() {
        let mut edge_profile = deferred_profile(&["github"], &["github"], "gpt-4o");
        edge_profile.insert(
            astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_DEFERRED_TOOLS_TEXT
                .to_string(),
            Value::String(
                "<deferred-tools><tool><name>github</name><description>GitHub</description></tool></deferred-tools>"
                    .to_string(),
            ),
        );

        assert!(
            names_for_model(&edge_profile, Some("gpt-4o")).is_empty(),
            "legacy schema-like deferred manifests must not be activatable"
        );
        assert!(
            block_for_model(&edge_profile, "gpt-4o").is_none(),
            "legacy schema-like deferred manifests must not be shown to the model"
        );
    }
}
