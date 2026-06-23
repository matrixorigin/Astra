//! Plain-text lines for `--explain` stderr during `/chat` payload prep.

use std::collections::HashSet;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restricted_text_sorts_names() {
        let mut s = HashSet::new();
        s.insert("zebra".into());
        s.insert("alpha".into());
        let t = restricted_tools_explain_text(&s).expect("line");
        assert!(t.contains("alpha, zebra"));
    }

    #[test]
    fn restricted_text_is_none_without_restrictions() {
        assert!(restricted_tools_explain_text(&HashSet::new()).is_none());
    }
}
