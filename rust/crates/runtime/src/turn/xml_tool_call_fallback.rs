//! Fallback parser for XML-formatted tool calls in LLM text output.
//!
//! Some models (e.g. kimi-k2.5 under context pressure) emit tool calls as XML
//! `<invoke>` blocks inside `content` instead of using the OpenAI function-calling
//! protocol (`delta.tool_calls`).  When the structured `tool_calls` array is empty
//! but the text contains `<invoke>` XML, this module extracts them into the same
//! `Vec<Value>` shape the rest of the pipeline expects.

use serde_json::{Value, json};
use uuid::Uuid;

/// Try to extract tool calls from XML `<invoke>` blocks in `text`.
///
/// Returns `Some(tool_calls)` (non-empty) on success, `None` if no valid
/// invocations were found.  Each returned value matches the OpenAI
/// `tool_calls` element shape:
///
/// ```json
/// { "id": "...", "type": "function", "function": { "name": "...", "arguments": "{...}" } }
/// ```
pub fn parse_xml_tool_calls(text: &str) -> Option<Vec<Value>> {
    if !text.contains("<invoke") {
        return None;
    }

    let mut calls = Vec::new();
    let mut search_from = 0;

    while let Some(start) = text[search_from..].find("<invoke") {
        let abs_start = search_from + start;
        // Find closing tag — either </invoke> or self-closing />
        let block_end = if let Some(close) = text[abs_start..].find("</invoke>") {
            abs_start + close + "</invoke>".len()
        } else if let Some(close) = text[abs_start..].find("/>") {
            abs_start + close + "/>".len()
        } else {
            search_from = abs_start + 1;
            continue;
        };

        let block = &text[abs_start..block_end];
        if let Some(tc) = parse_single_invoke(block) {
            calls.push(tc);
        }
        search_from = block_end;
    }

    if calls.is_empty() { None } else { Some(calls) }
}

/// Strip successfully-parsed `<invoke>` blocks from text, returning the
/// remaining content (trimmed).  Unparseable fragments are kept.
pub fn strip_parsed_invocations(text: &str) -> String {
    if !text.contains("<invoke") {
        return text.to_string();
    }

    let mut result = text.to_string();
    let mut search_from = 0;

    while let Some(start) = result[search_from..].find("<invoke") {
        let abs_start = search_from + start;
        let block_end = if let Some(close) = result[abs_start..].find("</invoke>") {
            abs_start + close + "</invoke>".len()
        } else if let Some(close) = result[abs_start..].find("/>") {
            abs_start + close + "/>".len()
        } else {
            search_from = abs_start + 1;
            continue;
        };

        let block = &result[abs_start..block_end];
        if parse_single_invoke(block).is_some() {
            result.replace_range(abs_start..block_end, "");
            // don't advance search_from — next block may now start at same position
        } else {
            search_from = block_end;
        }
    }

    result.trim().to_string()
}

/// Parse one `<invoke name="tool_name">…</invoke>` block.
fn parse_single_invoke(block: &str) -> Option<Value> {
    // Extract tool name from <invoke name="...">
    let name = extract_attr(block, "name")?;
    if name.is_empty() {
        return None;
    }

    // Collect <parameter name="key">value</parameter> pairs
    let mut args = serde_json::Map::new();
    let mut search = 0;
    while let Some(ps) = block[search..].find("<parameter") {
        let abs_ps = search + ps;
        let Some(pe) = block[abs_ps..].find("</parameter>") else {
            search = abs_ps + 1;
            continue;
        };
        let param_block = &block[abs_ps..abs_ps + pe + "</parameter>".len()];
        if let Some(pname) = extract_attr(param_block, "name") {
            // Value is between first '>' and '</parameter>'
            if let Some(gt) = param_block.find('>') {
                let inner = &param_block[gt + 1..];
                let value = inner.strip_suffix("</parameter>").unwrap_or(inner);
                args.insert(pname, Value::String(value.to_string()));
            }
        }
        search = abs_ps + pe + "</parameter>".len();
    }

    let arguments = serde_json::to_string(&args).unwrap_or_else(|_| "{}".to_string());
    let id = format!("xmlfb_{}", &Uuid::new_v4().to_string()[..8]);

    Some(json!({
        "id": id,
        "type": "function",
        "function": {
            "name": name,
            "arguments": arguments,
        }
    }))
}

/// Extract `attr="value"` from an XML opening tag.
fn extract_attr(tag: &str, attr: &str) -> Option<String> {
    let pattern = format!("{attr}=\"");
    let start = tag.find(&pattern)? + pattern.len();
    let end = tag[start..].find('"')? + start;
    Some(tag[start..end].to_string())
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_single_invoke_with_params() {
        let xml = r#"<invoke name="read_file">
<parameter name="path">src/main.rs</parameter>
</invoke>"#;
        let calls = parse_xml_tool_calls(xml).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["function"]["name"], "read_file");
        let args: serde_json::Map<String, Value> =
            serde_json::from_str(calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["path"], "src/main.rs");
        assert!(calls[0]["id"].as_str().unwrap().starts_with("xmlfb_"));
        assert_eq!(calls[0]["type"], "function");
    }

    #[test]
    fn parse_multiple_invokes() {
        let xml = r#"<invoke name="read_file">
<parameter name="path">a.rs</parameter>
</invoke>
<invoke name="grep">
<parameter name="pattern">TODO</parameter>
<parameter name="path">src</parameter>
</invoke>"#;
        let calls = parse_xml_tool_calls(xml).unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["function"]["name"], "read_file");
        assert_eq!(calls[1]["function"]["name"], "grep");
        let args: serde_json::Map<String, Value> =
            serde_json::from_str(calls[1]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["pattern"], "TODO");
        assert_eq!(args["path"], "src");
    }

    #[test]
    fn returns_none_for_no_invokes() {
        assert!(parse_xml_tool_calls("just normal text").is_none());
        assert!(parse_xml_tool_calls("").is_none());
    }

    #[test]
    fn returns_none_for_malformed_invoke() {
        // Missing closing tag
        assert!(parse_xml_tool_calls(r#"<invoke name="bash">"#).is_none());
        // Missing name attribute
        assert!(parse_xml_tool_calls(r#"<invoke></invoke>"#).is_none());
    }

    #[test]
    fn invoke_with_no_params() {
        let xml = r#"<invoke name="list_dir"></invoke>"#;
        let calls = parse_xml_tool_calls(xml).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["function"]["name"], "list_dir");
        let args: serde_json::Map<String, Value> =
            serde_json::from_str(calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert!(args.is_empty());
    }

    #[test]
    fn mixed_text_and_invokes() {
        let xml = r#"Let me read the file first.
<invoke name="read_file">
<parameter name="path">src/lib.rs</parameter>
</invoke>
And also search for errors.
<invoke name="grep">
<parameter name="pattern">error</parameter>
</invoke>"#;
        let calls = parse_xml_tool_calls(xml).unwrap();
        assert_eq!(calls.len(), 2);
    }

    #[test]
    fn strip_parsed_invocations_removes_valid_blocks() {
        let xml = r#"Let me check.
<invoke name="read_file">
<parameter name="path">a.rs</parameter>
</invoke>
Done."#;
        let remaining = strip_parsed_invocations(xml);
        assert_eq!(remaining, "Let me check.\n\nDone.");
        assert!(!remaining.contains("<invoke"));
    }

    #[test]
    fn strip_preserves_unparseable_fragments() {
        let text = "some text <invoke broken";
        let remaining = strip_parsed_invocations(text);
        assert_eq!(remaining, text);
    }

    #[test]
    fn real_world_kimi_output() {
        // Exact pattern from the session log
        let xml = r#"<invoke name="read_file">
<parameter name="path">rust/crates/runtime/src/tasks/task_learning.rs</parameter>
</invoke>
<invoke name="read_file">
<parameter name="path">rust/crates/runtime/src/tasks/pattern.rs</parameter>
</invoke>
<invoke name="grep">
<parameter name="pattern">success_rate|failure|penalty|decay|expire|outdated|bad|quality</parameter>
<parameter name="path">rust/crates/runtime/src/tasks</parameter>
</invoke>"#;
        let calls = parse_xml_tool_calls(xml).unwrap();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0]["function"]["name"], "read_file");
        assert_eq!(calls[1]["function"]["name"], "read_file");
        assert_eq!(calls[2]["function"]["name"], "grep");

        let args0: serde_json::Map<String, Value> =
            serde_json::from_str(calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(
            args0["path"],
            "rust/crates/runtime/src/tasks/task_learning.rs"
        );

        let args2: serde_json::Map<String, Value> =
            serde_json::from_str(calls[2]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(
            args2["pattern"],
            "success_rate|failure|penalty|decay|expire|outdated|bad|quality"
        );

        let remaining = strip_parsed_invocations(xml);
        assert!(remaining.is_empty());
    }

    #[test]
    fn unique_ids_per_call() {
        let xml = r#"<invoke name="a"></invoke><invoke name="b"></invoke>"#;
        let calls = parse_xml_tool_calls(xml).unwrap();
        assert_ne!(
            calls[0]["id"].as_str().unwrap(),
            calls[1]["id"].as_str().unwrap()
        );
    }
}
