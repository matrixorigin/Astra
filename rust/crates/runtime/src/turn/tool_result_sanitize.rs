//! Strip CLI-only payloads from tool results before they are sent to the model API.

use regex::Regex;
use serde_json::Value;
use std::sync::OnceLock;

/// Marks unified diff appended to `str_replace` text results (not sent to the model).
pub const STR_REPLACE_DIFF_START: &str = "\n<<<ASTRA_UNIFIED_DIFF>>>\n";
pub const STR_REPLACE_DIFF_END: &str = "\n<<<END_ASTRA_UNIFIED_DIFF>>>\n";

fn str_replace_diff_block_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\n<<<ASTRA_UNIFIED_DIFF>>>\n[\s\S]*?\n<<<END_ASTRA_UNIFIED_DIFF>>>\n?")
            .expect("regex")
    })
}

/// Remove `_cli_*` keys from JSON tool results and diff sentinels from `str_replace` text.
#[must_use]
pub fn tool_result_content_for_model(tool_name: &str, content: &str) -> String {
    match tool_name {
        "write_file" => strip_cli_json_keys(content),
        "str_replace" | "multi_edit" => str_replace_diff_block_re()
            .replace_all(content, "")
            .to_string(),
        _ => content.to_string(),
    }
}

fn strip_cli_json_keys(content: &str) -> String {
    let Ok(mut v) = serde_json::from_str::<Value>(content) else {
        return content.to_string();
    };
    let Some(obj) = v.as_object_mut() else {
        return content.to_string();
    };
    obj.retain(|k, _| !k.starts_with("_cli_"));
    v.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strips_cli_keys_from_write_file_json() {
        let raw = json!({
            "success": true,
            "bytes_written": 3,
            "path": "a.rs",
            "_cli_unified_diff": "diff --git"
        })
        .to_string();
        let out = tool_result_content_for_model("write_file", &raw);
        assert!(!out.contains("_cli"));
        assert!(out.contains("success"));
    }

    #[test]
    fn strips_str_replace_sentinel() {
        let raw = "Replaced ok\n<<<ASTRA_UNIFIED_DIFF>>>\n+a\n<<<END_ASTRA_UNIFIED_DIFF>>>\n";
        let out = tool_result_content_for_model("str_replace", raw);
        assert!(!out.contains("ASTRA_UNIFIED_DIFF"));
        assert!(out.contains("Replaced"));
    }

    #[test]
    fn strips_multi_edit_sentinel() {
        let raw =
            "Applied 1 edit(s)\n<<<ASTRA_UNIFIED_DIFF>>>\n+a\n<<<END_ASTRA_UNIFIED_DIFF>>>\n";
        let out = tool_result_content_for_model("multi_edit", raw);
        assert!(!out.contains("ASTRA_UNIFIED_DIFF"));
        assert!(out.contains("Applied"));
    }
}
