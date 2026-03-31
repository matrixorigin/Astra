//! Explain-mode stderr lines tied to tool schema restriction and selector guidance.

use std::collections::HashSet;

use crossterm::style::Stylize;
use serde_json::Value;

pub(crate) fn eprint_restricted_tools_explain(show: bool, restricted_tools: &HashSet<String>) {
    if !show || restricted_tools.is_empty() {
        return;
    }
    eprintln!(
        "{}",
        format!(
            "  ├─ restricted: {} tool(s) filtered [{}]",
            restricted_tools.len(),
            restricted_tools
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        )
        .dim()
    );
}

pub(crate) fn eprint_selector_guidance_explain(
    show: bool,
    payload: &Value,
    selection_confidence: f64,
) {
    if !show {
        return;
    }
    let Some(recommended) = payload["edge_profile"]["recommended_tools"].as_array() else {
        return;
    };
    let names: Vec<&str> = recommended.iter().filter_map(|v| v.as_str()).collect();
    if names.is_empty() {
        return;
    }
    eprintln!(
        "{}",
        format!(
            "  ├─ guidance: {} (confidence: {:.2})",
            names.join(", "),
            selection_confidence
        )
        .dim()
    );
}
