//! Plain-text lines for `--explain` stderr during `/chat` payload prep (CLI applies styling).

use std::collections::HashSet;

use serde_json::Value;

/// `Some` when there are restricted tools (names sorted for stable output).
#[must_use]
pub fn restricted_tools_explain_text(restricted_tools: &HashSet<String>) -> Option<String> {
    if restricted_tools.is_empty() {
        return None;
    }
    let mut names: Vec<_> = restricted_tools.iter().cloned().collect();
    names.sort();
    Some(format!(
        "  ├─ restricted: {} tool(s) filtered [{}]",
        restricted_tools.len(),
        names.join(", ")
    ))
}

/// `Some` when `edge_profile.recommended_tools` is a non-empty JSON array of strings.
#[must_use]
pub fn selector_guidance_explain_text(
    payload: &Value,
    selection_confidence: f64,
) -> Option<String> {
    let recommended = payload["edge_profile"]["recommended_tools"].as_array()?;
    let names: Vec<&str> = recommended.iter().filter_map(|v| v.as_str()).collect();
    if names.is_empty() {
        return None;
    }
    Some(format!(
        "  ├─ guidance: {} (confidence: {:.2})",
        names.join(", "),
        selection_confidence
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn restricted_text_sorts_names() {
        let mut s = HashSet::new();
        s.insert("zebra".into());
        s.insert("alpha".into());
        let t = restricted_tools_explain_text(&s).expect("line");
        assert!(t.contains("alpha, zebra"));
    }

    #[test]
    fn guidance_none_without_recommended() {
        assert!(selector_guidance_explain_text(&json!({}), 0.5).is_none());
    }

    #[test]
    fn guidance_some_with_tools() {
        let p = json!({"edge_profile":{"recommended_tools":["read_file","bash"]}});
        let t = selector_guidance_explain_text(&p, 0.88).expect("line");
        assert!(t.contains("read_file, bash"));
        assert!(t.contains("0.88"));
    }
}
