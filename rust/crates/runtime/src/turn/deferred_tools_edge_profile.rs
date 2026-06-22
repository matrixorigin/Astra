use std::collections::HashSet;

use astra_text_utils::xml_escape::xml_escape_text;
use serde_json::{Map, Value};

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
    manifest_for_model(edge_profile, resolved_model_name).map(|manifest| manifest.block)
}

pub(crate) fn names_for_model(
    edge_profile: &Map<String, Value>,
    resolved_model_name: Option<&str>,
) -> HashSet<String> {
    resolved_model_name
        .and_then(|model| manifest_for_model(edge_profile, model))
        .map(|manifest| manifest.names)
        .unwrap_or_default()
}

struct DeferredToolsManifest {
    block: String,
    names: HashSet<String>,
}

fn manifest_for_model(
    edge_profile: &Map<String, Value>,
    resolved_model_name: &str,
) -> Option<DeferredToolsManifest> {
    let declared_names = names(edge_profile);
    if declared_names.is_empty() {
        return None;
    }
    let source_budget = edge_profile
        .get(
            astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_DEFERRED_TOOLS_CONTEXT_WINDOW,
        )
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())?;
    let resolved_budget = crate::prompts::budget_for_model(Some(resolved_model_name)).model_limit;
    if source_budget != resolved_budget {
        return None;
    }
    let block = edge_profile
        .get(astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_DEFERRED_TOOLS_TEXT)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)?;
    let rendered_name_keys = rendered_name_keys_from_block(&block);
    if rendered_name_keys.is_empty() {
        return None;
    }
    let declared_name_keys: HashSet<String> = declared_names
        .iter()
        .map(|name| xml_escape_text(name).into_owned())
        .collect();
    if declared_name_keys != rendered_name_keys {
        return None;
    }
    Some(DeferredToolsManifest {
        block,
        names: declared_names,
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
