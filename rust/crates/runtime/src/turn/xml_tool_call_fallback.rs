//! Fallback parser for degraded tool calls in LLM text output.
//!
//! Some models emit tool calls as text instead of using the OpenAI function-calling
//! protocol (`delta.tool_calls`).  Two degradation patterns are handled:
//!
//! 1. **`<invoke>` XML** (e.g. kimi-k2.5 under context pressure): structured XML
//!    blocks with `<parameter>` children.
//! 2. **`<tool_call>` text** (e.g. delegation sub-agents): corrupted function-call
//!    syntax like `<tool_call>bash)(echo hello)` or `<tool_call>grep}{pattern: "x"}`.
//!
//! When the structured `tool_calls` array is empty but the text contains either
//! pattern, this module extracts them into the same `Vec<Value>` shape the rest
//! of the pipeline expects.
//!
//! **False-positive guard**: if the non-tag portion of the text exceeds 20% of
//! the total length, the content is likely a normal response that *mentions*
//! the format rather than a degraded tool call.
//! In that case the parsers return `None`.

use serde_json::{Value, json};
use uuid::Uuid;

/// Maximum ratio of non-XML text to total text before we refuse to treat the
/// content as degraded tool calls.  0.20 = if more than 20% of the content is
/// plain prose surrounding the `<invoke>` blocks, it's probably a normal
/// response that happens to contain XML examples.
const MAX_NON_XML_RATIO: f64 = 0.20;

/// Try to extract tool calls from XML `<invoke>` blocks in `text`.
///
/// Returns `Some(tool_calls)` (non-empty) on success, `None` if no valid
/// invocations were found or if the text looks like a normal response that
/// merely *mentions* `<invoke>` XML.
///
/// Each returned value matches the OpenAI `tool_calls` element shape:
///
/// ```json
/// { "id": "...", "type": "function", "function": { "name": "...", "arguments": "{...}" } }
/// ```
pub fn parse_xml_tool_calls(text: &str) -> Option<Vec<Value>> {
    if !text.contains("<invoke") {
        return None;
    }

    let mut calls = Vec::new();
    let mut xml_bytes: usize = 0;
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
            xml_bytes += block_end - abs_start;
        }
        search_from = block_end;
    }

    if calls.is_empty() {
        return None;
    }

    // False-positive guard: if the surrounding prose is substantial relative
    // to the XML blocks, this is likely a normal response discussing the
    // format rather than a degraded tool call.
    let total = text.trim().len();
    if total > 0 {
        let non_xml = total.saturating_sub(xml_bytes);
        let ratio = non_xml as f64 / total as f64;
        if ratio > MAX_NON_XML_RATIO {
            return None;
        }
    }

    Some(calls)
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

// ─── <tool_call> Fallback ────────────────────────────────────────────────────

/// Try to extract tool calls from `<tool_call>` blocks in `text`.
///
/// Models sometimes emit corrupted function-call syntax such as:
/// - `<tool_call>bash)(echo hello)</tool_call>`
/// - `<tool_call>grep}{pattern: "x"}</tool_call>`
/// - `<tool_call>read_file({"path":"a.rs"})</tool_call>`
///
/// We extract the tool name (first word-like token after `<tool_call>`) and
/// attempt to recover arguments from the remainder.
pub fn parse_tool_call_tags(text: &str) -> Option<Vec<Value>> {
    if !text.contains("<tool_call>") {
        return None;
    }

    let mut calls = Vec::new();
    let mut tag_bytes: usize = 0;
    let mut search_from = 0;

    while let Some(start) = text[search_from..].find("<tool_call>") {
        let abs_start = search_from + start;
        let content_start = abs_start + "<tool_call>".len();

        // Find end: explicit </tool_call> or next <tool_call> or end-of-text
        let block_end = text[content_start..]
            .find("</tool_call>")
            .map(|i| content_start + i + "</tool_call>".len())
            .or_else(|| {
                text[content_start..]
                    .find("<tool_call>")
                    .map(|i| content_start + i)
            })
            .unwrap_or(text.len());

        let inner = text[content_start..block_end]
            .trim_end_matches("</tool_call>")
            .replace('\0', ""); // strip null bytes

        if let Some(tc) = parse_single_tool_call_tag(inner.trim()) {
            tag_bytes += block_end - abs_start;
            calls.push(tc);
        }
        search_from = block_end;
    }

    if calls.is_empty() {
        return None;
    }

    // Same false-positive guard as <invoke>
    let total = text.trim().len();
    if total > 0 {
        let non_tag = total.saturating_sub(tag_bytes);
        let ratio = non_tag as f64 / total as f64;
        if ratio > MAX_NON_XML_RATIO {
            return None;
        }
    }

    Some(calls)
}

/// Parse the inner content of a `<tool_call>…</tool_call>` block.
///
/// Handles patterns like:
/// - `tool_name)(args)` — parenthesized args
/// - `tool_name}{json}` — JSON-like args with leading `}`
/// - `tool_name({"key":"val"})` — function-call style
/// - `tool_name {"key":"val"}` — space-separated JSON
fn parse_single_tool_call_tag(inner: &str) -> Option<Value> {
    if inner.is_empty() {
        return None;
    }

    // Extract tool name: first contiguous word chars (a-z, A-Z, 0-9, _, -)
    let name_end = inner
        .find(|c: char| !c.is_alphanumeric() && c != '_' && c != '-')
        .unwrap_or(inner.len());
    let name = &inner[..name_end];
    if name.is_empty() {
        return None;
    }

    // Remainder after tool name
    let rest = inner[name_end..].trim();

    // Try to recover arguments
    let arguments = recover_tool_call_args(rest);

    let id = format!("tcfb_{}", &Uuid::new_v4().to_string()[..8]);
    Some(json!({
        "id": id,
        "type": "function",
        "function": {
            "name": name,
            "arguments": arguments,
        }
    }))
}

/// Best-effort argument recovery from corrupted tool call remainder.
fn recover_tool_call_args(rest: &str) -> String {
    if rest.is_empty() {
        return "{}".to_string();
    }

    // Strip leading delimiters: `)`, `}`, `(`, `{`, `:` that are syntax noise
    let cleaned = rest
        .trim_start_matches([')', '(', '}', '{', ':', ' ']);

    // If what remains looks like JSON (starts with { after cleanup), try to parse it
    if let Some(json_start) = rest.find('{') {
        let json_candidate = &rest[json_start..];
        // Find matching closing brace
        if let Some(json_str) = extract_balanced_braces(json_candidate) {
            if serde_json::from_str::<Value>(json_str).is_ok() {
                return json_str.to_string();
            }
        }
    }

    // If it looks like parenthesized args: )(content) or (content)
    if let (Some(open), Some(close)) = (rest.find('('), rest.rfind(')')) {
        if open < close {
            let arg_content = &rest[open + 1..close];
            if !arg_content.is_empty() {
                // If the content is valid JSON object, use it directly
                if let Ok(v) = serde_json::from_str::<Value>(arg_content) {
                    if v.is_object() {
                        return arg_content.to_string();
                    }
                }
                // Best-effort: wrap as {"command": "..."} — works for bash/shell.
                // For other tools the executor will return an error, triggering retry.
                let escaped = arg_content.replace('\\', "\\\\").replace('"', "\\\"");
                return format!("{{\"command\":\"{escaped}\"}}");
            }
        }
    }

    // Last resort: if cleaned text is non-empty, wrap as command
    if !cleaned.is_empty() {
        let escaped = cleaned.replace('\\', "\\\\").replace('"', "\\\"");
        return format!("{{\"command\":\"{escaped}\"}}");
    }

    "{}".to_string()
}

/// Extract a balanced `{…}` substring from the start of `s`.
fn extract_balanced_braces(s: &str) -> Option<&str> {
    if !s.starts_with('{') {
        return None;
    }
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escape = false;
    for (i, c) in s.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        if c == '\\' && in_string {
            escape = true;
            continue;
        }
        if c == '"' {
            in_string = !in_string;
            continue;
        }
        if in_string {
            continue;
        }
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Strip successfully-parsed `<tool_call>` blocks from text.
pub fn strip_parsed_tool_call_tags(text: &str) -> String {
    if !text.contains("<tool_call>") {
        return text.to_string();
    }

    let mut result = text.to_string();
    let mut search_from = 0;

    while let Some(start) = result[search_from..].find("<tool_call>") {
        let abs_start = search_from + start;
        let content_start = abs_start + "<tool_call>".len();

        let block_end = result[content_start..]
            .find("</tool_call>")
            .map(|i| content_start + i + "</tool_call>".len())
            .or_else(|| {
                result[content_start..]
                    .find("<tool_call>")
                    .map(|i| content_start + i)
            })
            .unwrap_or(result.len());

        let inner = result[content_start..block_end]
            .trim_end_matches("</tool_call>")
            .replace('\0', "");

        if parse_single_tool_call_tag(inner.trim()).is_some() {
            result.replace_range(abs_start..block_end, "");
        } else {
            search_from = block_end;
        }
    }

    result.trim().to_string()
}

/// Unified fallback: try `<invoke>` first, then `<tool_call>`.
/// Returns the first successful parse.
pub fn parse_degraded_tool_calls(text: &str) -> Option<Vec<Value>> {
    parse_xml_tool_calls(text).or_else(|| parse_tool_call_tags(text))
}

/// Unified strip: remove whichever degraded format was parsed.
/// Runs both strip passes — intentional: text may contain both formats,
/// and leftover tags should always be cleaned regardless of which format
/// `parse_degraded_tool_calls` matched first.
pub fn strip_degraded_tool_calls(text: &str) -> String {
    let after_invoke = strip_parsed_invocations(text);
    strip_parsed_tool_call_tags(&after_invoke)
}

// ─── <invoke> Parsing Helpers ───────────────────────────────────────────────

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
    fn mixed_text_and_invokes_with_little_prose() {
        // Short transitional text between invokes — still treated as degraded tool calls
        let xml = r#"
<invoke name="read_file">
<parameter name="path">src/lib.rs</parameter>
</invoke>
<invoke name="grep">
<parameter name="pattern">error</parameter>
</invoke>"#;
        let calls = parse_xml_tool_calls(xml).unwrap();
        assert_eq!(calls.len(), 2);
    }

    #[test]
    fn rejects_invoke_embedded_in_prose() {
        // Normal response that discusses the XML format — should NOT trigger fallback
        let text = r#"The system uses XML tool calls like this:
<invoke name="read_file">
<parameter name="path">src/lib.rs</parameter>
</invoke>
This format is used when the model degrades under context pressure.
You can see the invoke block contains parameter elements."#;
        assert!(parse_xml_tool_calls(text).is_none());
    }

    #[test]
    fn rejects_single_invoke_in_explanation() {
        let text = r#"Here is an example of the invoke format:
<invoke name="bash">
<parameter name="command">echo hello</parameter>
</invoke>
As you can see, the name attribute specifies the tool."#;
        assert!(parse_xml_tool_calls(text).is_none());
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

    // ─── <tool_call> Fallback Tests ────────────────────────────────────

    #[test]
    fn tool_call_tag_bash_parenthesized() {
        let text = r#"<tool_call>bash)(echo hello)</tool_call>"#;
        let calls = parse_tool_call_tags(text).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["function"]["name"], "bash");
        let args: serde_json::Map<String, Value> =
            serde_json::from_str(calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["command"], "echo hello");
        assert!(calls[0]["id"].as_str().unwrap().starts_with("tcfb_"));
    }

    #[test]
    fn tool_call_tag_json_args() {
        let text = r#"<tool_call>grep}{"pattern": "TODO", "path": "src"}</tool_call>"#;
        let calls = parse_tool_call_tags(text).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["function"]["name"], "grep");
        let args: serde_json::Map<String, Value> =
            serde_json::from_str(calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["pattern"], "TODO");
        assert_eq!(args["path"], "src");
    }

    #[test]
    fn tool_call_tag_function_call_style() {
        let text = r#"<tool_call>read_file({"path":"a.rs"})</tool_call>"#;
        let calls = parse_tool_call_tags(text).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["function"]["name"], "read_file");
        let args: serde_json::Map<String, Value> =
            serde_json::from_str(calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["path"], "a.rs");
    }

    #[test]
    fn tool_call_tag_with_null_bytes() {
        // Real-world pattern: tool call followed by binary garbage
        let text = "<tool_call>bash)(ls)\0\0\0\0</tool_call>";
        let calls = parse_tool_call_tags(text).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["function"]["name"], "bash");
    }

    #[test]
    fn tool_call_tag_no_closing_tag() {
        // Some models don't emit closing tag
        let text = "<tool_call>bash)(echo hi)";
        let calls = parse_tool_call_tags(text).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["function"]["name"], "bash");
    }

    #[test]
    fn tool_call_tag_returns_none_for_no_tags() {
        assert!(parse_tool_call_tags("just normal text").is_none());
        assert!(parse_tool_call_tags("").is_none());
    }

    #[test]
    fn tool_call_tag_rejects_prose_with_tag() {
        let text = "The model emitted <tool_call>bash)(echo hello)</tool_call> which is a known degradation pattern that we handle.";
        assert!(parse_tool_call_tags(text).is_none());
    }

    #[test]
    fn tool_call_tag_strip_removes_parsed() {
        let text = "Let me try.\n<tool_call>bash)(echo hi)</tool_call>\nDone.";
        let remaining = strip_parsed_tool_call_tags(text);
        assert!(!remaining.contains("<tool_call>"));
    }

    #[test]
    fn tool_call_real_session_pattern_bash() {
        // Exact pattern from session 3bc7fc43
        let text = r#"<tool_call>bash)(git log -1 --format="%H%n%an%n%ae%n%s%n%b" HEAD)"#;
        let calls = parse_tool_call_tags(text).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["function"]["name"], "bash");
        let args: serde_json::Map<String, Value> =
            serde_json::from_str(calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert!(args["command"].as_str().unwrap().contains("git log"));
    }

    #[test]
    fn tool_call_real_session_pattern_mcp() {
        // Exact pattern from session 3bc7fc43 (with null bytes stripped)
        let text = "<tool_call>mcp__git_log}{\"per_page\": 1, \"format\": \"fuller\"}}</tool_call>";
        let calls = parse_tool_call_tags(text).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["function"]["name"], "mcp__git_log");
        let args: serde_json::Map<String, Value> =
            serde_json::from_str(calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["per_page"], 1);
    }

    // ─── Unified Parser Tests ──────────────────────────────────────

    #[test]
    fn unified_prefers_invoke_over_tool_call() {
        let xml = r#"<invoke name="bash">
<parameter name="command">echo hi</parameter>
</invoke>"#;
        let calls = parse_degraded_tool_calls(xml).unwrap();
        assert_eq!(calls[0]["function"]["name"], "bash");
        assert!(calls[0]["id"].as_str().unwrap().starts_with("xmlfb_"));
    }

    #[test]
    fn unified_falls_back_to_tool_call() {
        let text = "<tool_call>bash)(echo hi)</tool_call>";
        let calls = parse_degraded_tool_calls(text).unwrap();
        assert_eq!(calls[0]["function"]["name"], "bash");
        assert!(calls[0]["id"].as_str().unwrap().starts_with("tcfb_"));
    }

    #[test]
    fn unified_returns_none_for_plain_text() {
        assert!(parse_degraded_tool_calls("just normal text").is_none());
    }
}
