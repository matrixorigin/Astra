/// Append names from `names` to `target`, skipping blank entries and
/// duplicates. Used to build unique tool / agent name lists from multi-source
/// metadata (events, config, runtime bindings).
pub fn append_unique_names<I, S>(target: &mut Vec<String>, names: I)
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    for name in names {
        let Some(name) = normalize_name(name.as_ref()) else {
            continue;
        };
        if !target.iter().any(|existing| existing == name) {
            target.push(name.to_string());
        }
    }
}

/// Return a trimmed, non-empty slice of `name`, or `None` if blank.
pub fn normalize_name(name: &str) -> Option<&str> {
    let name = name.trim();
    (!name.is_empty()).then_some(name)
}

/// Collect a normalized, deduplicated list of names from an iterator.
pub fn normalize_name_list<I, S>(names: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut normalized = Vec::new();
    append_unique_names(&mut normalized, names);
    normalized
}

/// Normalize an optional string — `Some("  ")` becomes `None`.
pub fn normalize_optional_name(name: Option<String>) -> Option<String> {
    name.and_then(|name| normalize_name(&name).map(str::to_string))
}

/// Extract a canonical tool name from event metadata JSON.
///
/// Looks only for an explicit `"tool_name"` field; generic `"name"` metadata is
/// intentionally ignored because it is ambiguous across event producers. Trims
/// whitespace and surrounding quotes, returning `None` if blank.
pub fn metadata_tool_name(metadata: Option<&serde_json::Value>) -> Option<String> {
    metadata
        .and_then(|v| v.get("tool_name"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().trim_matches('"').to_string())
        .and_then(|s| normalize_optional_name(Some(s)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_name_list_trims_drops_blank_and_dedupes() {
        assert_eq!(
            normalize_name_list([" ", " rg ", "rg", "bash", "bash "]),
            vec!["rg".to_string(), "bash".to_string()]
        );
    }

    #[test]
    fn normalize_name_borrows_trimmed_non_empty_slice() {
        assert_eq!(normalize_name(" tool "), Some("tool"));
        assert_eq!(normalize_name(" "), None);
    }

    #[test]
    fn normalize_optional_name_drops_blank_names() {
        assert_eq!(
            normalize_optional_name(Some(" tool ".to_string())).as_deref(),
            Some("tool")
        );
        assert_eq!(normalize_optional_name(Some(" ".to_string())), None);
        assert_eq!(normalize_optional_name(None), None);
    }

    #[test]
    fn metadata_tool_name_requires_explicit_canonical_field() {
        assert_eq!(
            metadata_tool_name(Some(&serde_json::json!({"tool_name": " bash "}))).as_deref(),
            Some("bash")
        );
        assert_eq!(
            metadata_tool_name(Some(&serde_json::json!({"tool_name": "\"bash\""}))).as_deref(),
            Some("bash")
        );
        assert_eq!(
            metadata_tool_name(Some(
                &serde_json::json!({"tool_name": "preferred", "name": "read_file"})
            ))
            .as_deref(),
            Some("preferred")
        );
        assert!(metadata_tool_name(Some(&serde_json::json!({"name": "read_file"}))).is_none());
        assert!(metadata_tool_name(Some(&serde_json::json!({"tool_name": "\"\""}))).is_none());
        assert!(metadata_tool_name(Some(&serde_json::json!({"tool_name": 42}))).is_none());
        assert!(metadata_tool_name(None).is_none());
    }
}
